//! ultrafine-ctl: tray app with brightness hotkeys and automatic brightness
//! for the LG UltraFine 5K on Windows. See CLAUDE.md and docs/PROTOCOL.md.
//! (Also builds on macOS for development; not released there.)

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod config;
mod curve;
mod device;
mod display_fix;
mod editor;
mod laptop;
mod panel;
mod ui;
mod worker;

use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use config::{Config, Hotkeys, LightSource};
use curve::{level_to_percent, percent_to_level, Mode};
use editor::{CurveEditor, EditorAction};
use panel::{Panel, PanelAction, PanelState};
use ui::{Action, Capture, CaptureResult, Osd, Painter};
use worker::{Cmd, Status};

#[derive(Debug)]
enum UserEvent {
    Menu(MenuEvent),
    Tray(TrayIconEvent),
    HotKey(GlobalHotKeyEvent),
    Status(Status),
}

/// Registered global shortcuts.
struct Shortcuts {
    manager: GlobalHotKeyManager,
    up: Option<HotKey>,
    down: Option<HotKey>,
    toggle: Option<HotKey>,
}

impl Shortcuts {
    fn new(cfg: &Hotkeys) -> Result<Self> {
        let mut s = Self {
            manager: GlobalHotKeyManager::new().context("hotkey manager")?,
            up: None,
            down: None,
            toggle: None,
        };
        for (a, spec) in [
            (Action::Up, &cfg.up),
            (Action::Down, &cfg.down),
            (Action::ToggleAuto, &cfg.toggle_auto),
        ] {
            if let Err(e) = s.set(a, spec) {
                log::warn!("{e}");
            }
        }
        Ok(s)
    }

    fn slot(&mut self, a: Action) -> &mut Option<HotKey> {
        match a {
            Action::Up => &mut self.up,
            Action::Down => &mut self.down,
            Action::ToggleAuto => &mut self.toggle,
        }
    }

    /// Replaces the shortcut for `a`. On failure the old one stays active.
    fn set(&mut self, a: Action, spec: &str) -> Result<(), String> {
        let hk: HotKey = spec
            .parse()
            .map_err(|e| format!("{}: cannot parse {spec:?}: {e}", a.label()))?;
        let old = *self.slot(a);
        if old == Some(hk) {
            return Ok(());
        }
        if let Some(old) = old {
            let _ = self.manager.unregister(old);
        }
        match self.manager.register(hk) {
            Ok(()) => {
                log::info!("shortcut {}: {spec}", a.label());
                *self.slot(a) = Some(hk);
                Ok(())
            }
            Err(e) => {
                if let Some(old) = old {
                    let _ = self.manager.register(old);
                }
                Err(format!(
                    "{spec} is not available (used by another app?): {e}"
                ))
            }
        }
    }

    fn action(&self, id: u32) -> Option<Action> {
        let is = |h: &Option<HotKey>| h.is_some_and(|h| h.id() == id);
        if is(&self.up) {
            Some(Action::Up)
        } else if is(&self.down) {
            Some(Action::Down)
        } else if is(&self.toggle) {
            Some(Action::ToggleAuto)
        } else {
            None
        }
    }
}

fn spec_of(cfg: &Config, a: Action) -> &str {
    match a {
        Action::Up => &cfg.hotkeys.up,
        Action::Down => &cfg.hotkeys.down,
        Action::ToggleAuto => &cfg.hotkeys.toggle_auto,
    }
}

fn set_spec(cfg: &mut Config, a: Action, spec: String) {
    match a {
        Action::Up => cfg.hotkeys.up = spec,
        Action::Down => cfg.hotkeys.down = spec,
        Action::ToggleAuto => cfg.hotkeys.toggle_auto = spec,
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    if let Err(e) = run() {
        log::error!("{e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut cfg = Config::load_or_create().unwrap_or_else(|e| {
        log::error!("config: {e:#}; using defaults");
        Config::default()
    });
    log::info!(
        "config: {}",
        Config::path()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    );

    #[allow(unused_mut)]
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
    }

    // Device worker thread.
    let (tx, rx) = mpsc::channel::<Cmd>();
    let proxy = event_loop.create_proxy();
    let worker_cfg = cfg.clone();
    let worker = std::thread::Builder::new()
        .name("device".into())
        .spawn(move || {
            worker::run(worker_cfg, rx, move |s| {
                let _ = proxy.send_event(UserEvent::Status(s));
            })
        })
        .context("spawning device thread")?;

    // Windows: black-screen watchdog and laptop panel poller.
    let fix_enabled = Arc::new(AtomicBool::new(cfg.fix_black_screen));
    if cfg!(windows) {
        let flag = fix_enabled.clone();
        std::thread::Builder::new()
            .name("display-fix".into())
            .spawn(move || display_fix::watch(|| flag.load(Ordering::Relaxed)))
            .context("spawning display-fix thread")?;

        let tx = tx.clone();
        std::thread::Builder::new()
            .name("laptop".into())
            .spawn(move || laptop_poller(tx))
            .context("spawning laptop thread")?;
    }

    let mut shortcuts = Shortcuts::new(&cfg.hotkeys)?;

    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = proxy.send_event(UserEvent::Menu(e));
    }));
    let proxy = event_loop.create_proxy();
    TrayIconEvent::set_event_handler(Some(move |e| {
        let _ = proxy.send_event(UserEvent::Tray(e));
    }));
    let proxy = event_loop.create_proxy();
    GlobalHotKeyEvent::set_event_handler(Some(move |e| {
        let _ = proxy.send_event(UserEvent::HotKey(e));
    }));

    // Tray menu (right click). Left click opens the slider panel.
    let status_item = MenuItem::new("LG UltraFine: searching…", false, None);
    let up_item = MenuItem::new("", true, None);
    let down_item = MenuItem::new("", true, None);
    let auto_item = CheckMenuItem::new("", true, cfg.auto, None);
    let sync_item = CheckMenuItem::new(
        "Follow laptop screen (when its auto brightness is on)",
        true,
        cfg.sync_laptop,
        None,
    );
    let source_item = CheckMenuItem::new(
        "Use laptop light sensor (LG sensor when lid closed)",
        true,
        cfg.light_source == LightSource::Laptop,
        None,
    );
    let curve_item = MenuItem::new("Brightness curve…", true, None);
    let fix_item = CheckMenuItem::new(
        "Fix black screen on connect",
        true,
        cfg.fix_black_screen,
        None,
    );
    let osd_item = CheckMenuItem::new("Show brightness popup", true, cfg.osd, None);
    let sc_up = MenuItem::new("", true, None);
    let sc_down = MenuItem::new("", true, None);
    let sc_auto = MenuItem::new("", true, None);
    let sc_reset = MenuItem::new("Reset to defaults", true, None);
    let shortcuts_menu = Submenu::with_items(
        "Shortcuts",
        true,
        &[
            &sc_up,
            &sc_down,
            &sc_auto,
            &PredefinedMenuItem::separator(),
            &sc_reset,
        ],
    )?;
    let open_cfg_item = MenuItem::new("Open settings file", true, None);
    let quit_item = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    menu.append_items(&[
        &status_item,
        &PredefinedMenuItem::separator(),
        &up_item,
        &down_item,
        &auto_item,
        &sync_item,
        &source_item,
        &curve_item,
        &PredefinedMenuItem::separator(),
        &shortcuts_menu,
        &osd_item,
        &fix_item,
        &open_cfg_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ])?;
    let relabel = {
        let (up_item, down_item, auto_item) =
            (up_item.clone(), down_item.clone(), auto_item.clone());
        let (sc_up, sc_down, sc_auto) = (sc_up.clone(), sc_down.clone(), sc_auto.clone());
        move |cfg: &Config| {
            up_item.set_text(format!("Brighter\t{}", cfg.hotkeys.up));
            down_item.set_text(format!("Darker\t{}", cfg.hotkeys.down));
            auto_item.set_text(format!("Auto brightness\t{}", cfg.hotkeys.toggle_auto));
            sc_up.set_text(format!("Brighter: {}…", cfg.hotkeys.up));
            sc_down.set_text(format!("Darker: {}…", cfg.hotkeys.down));
            sc_auto.set_text(format!("Auto on/off: {}…", cfg.hotkeys.toggle_auto));
        }
    };
    relabel(&cfg);

    let mut menu = Some(menu);
    let mut tray: Option<TrayIcon> = None;
    let mut worker = Some(worker);
    let painter = Rc::new(Painter::new());
    let mut osd: Option<Osd> = None;
    let mut panel: Option<Panel> = None;
    let mut editor: Option<CurveEditor> = None;
    let mut capture: Option<Capture> = None;
    let mut last_seq = 0u64;
    let mut last_status: Option<Status> = None;
    // Config writes are batched (slider drags change the curve many times).
    let mut save_at: Option<Instant> = None;

    event_loop.run(move |event, target, control_flow| {
        let send = |c: Cmd| {
            let _ = tx.send(c);
        };
        let mark_dirty = |save_at: &mut Option<Instant>| {
            save_at.get_or_insert(Instant::now() + Duration::from_secs(2));
        };

        // Settings changes from the menu, panel and hotkeys go through here
        // so the menu checks, the panel and the worker stay consistent.
        let set_auto = |cfg: &mut Config, on: bool, save_at: &mut Option<Instant>| {
            cfg.auto = on;
            auto_item.set_checked(on);
            send(Cmd::SetAuto(on));
            mark_dirty(save_at);
        };
        let set_sync = |cfg: &mut Config, on: bool, save_at: &mut Option<Instant>| {
            cfg.sync_laptop = on;
            sync_item.set_checked(on);
            send(Cmd::SetSync(on));
            mark_dirty(save_at);
        };
        let set_source = |cfg: &mut Config, laptop: bool, save_at: &mut Option<Instant>| {
            cfg.light_source = if laptop {
                LightSource::Laptop
            } else {
                LightSource::Display
            };
            source_item.set_checked(laptop);
            send(Cmd::SetSource(cfg.light_source));
            mark_dirty(save_at);
        };
        let open_editor = |cfg: &Config, editor: &mut Option<CurveEditor>, st: Option<&Status>| {
            if editor.is_some() {
                return;
            }
            match CurveEditor::new(target, painter.clone(), cfg.curve.clone()) {
                Ok(mut e) => {
                    if let Some(s) = st {
                        e.update(s.lux, &s.curve);
                    }
                    *editor = Some(e);
                }
                Err(err) => log::error!("curve window: {err:#}"),
            }
        };
        let start_capture = |a: Action, cfg: &Config, capture: &mut Option<Capture>| {
            *capture = None;
            match Capture::new(target, painter.clone(), a, spec_of(cfg, a)) {
                Ok(c) => *capture = Some(c),
                Err(e) => log::error!("shortcut window: {e:#}"),
            }
        };

        match event {
            Event::NewEvents(StartCause::Init) => {
                match TrayIconBuilder::new()
                    .with_menu(Box::new(menu.take().expect("menu")))
                    .with_menu_on_left_click(false)
                    .with_tooltip("LG UltraFine")
                    .with_icon(icon())
                    .build()
                {
                    Ok(t) => tray = Some(t),
                    Err(e) => log::error!("tray icon: {e}"),
                }
                match Osd::new(target, painter.clone()) {
                    Ok(o) => osd = Some(o),
                    Err(e) => log::error!("popup window: {e:#}"),
                }
                match Panel::new(target, painter.clone()) {
                    Ok(mut p) => {
                        p.set_state(panel_state(&cfg, None));
                        panel = Some(p);
                    }
                    Err(e) => log::error!("panel window: {e:#}"),
                }
                // Visual check without the display: UF_DEMO_OSD=1.
                if std::env::var_os("UF_DEMO_OSD").is_some() {
                    if let Some(o) = osd.as_mut() {
                        o.show(ui::lg_monitor(target), 42.0, "demo");
                    }
                }
            }
            Event::UserEvent(UserEvent::Tray(TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                rect,
                ..
            })) => {
                if let Some(p) = panel.as_mut() {
                    p.toggle(target, (rect.position, rect.size));
                }
            }
            Event::UserEvent(UserEvent::Menu(e)) => {
                let id = e.id();
                if id == up_item.id() {
                    send(Cmd::Step { up: true });
                } else if id == down_item.id() {
                    send(Cmd::Step { up: false });
                } else if id == auto_item.id() {
                    // Check items toggle themselves; follow them.
                    set_auto(&mut cfg, auto_item.is_checked(), &mut save_at);
                } else if id == sync_item.id() {
                    set_sync(&mut cfg, sync_item.is_checked(), &mut save_at);
                } else if id == source_item.id() {
                    set_source(&mut cfg, source_item.is_checked(), &mut save_at);
                } else if id == curve_item.id() {
                    open_editor(&cfg, &mut editor, last_status.as_ref());
                } else if id == fix_item.id() {
                    cfg.fix_black_screen = fix_item.is_checked();
                    fix_enabled.store(cfg.fix_black_screen, Ordering::Relaxed);
                    mark_dirty(&mut save_at);
                } else if id == osd_item.id() {
                    cfg.osd = osd_item.is_checked();
                    mark_dirty(&mut save_at);
                } else if id == sc_up.id() {
                    start_capture(Action::Up, &cfg, &mut capture);
                } else if id == sc_down.id() {
                    start_capture(Action::Down, &cfg, &mut capture);
                } else if id == sc_auto.id() {
                    start_capture(Action::ToggleAuto, &cfg, &mut capture);
                } else if id == sc_reset.id() {
                    let d = Hotkeys::default();
                    for (a, spec) in [
                        (Action::Up, d.up),
                        (Action::Down, d.down),
                        (Action::ToggleAuto, d.toggle_auto),
                    ] {
                        match shortcuts.set(a, &spec) {
                            Ok(()) => set_spec(&mut cfg, a, spec),
                            Err(e) => log::warn!("{e}"),
                        }
                    }
                    relabel(&cfg);
                    mark_dirty(&mut save_at);
                } else if id == open_cfg_item.id() {
                    save(&cfg);
                    save_at = None;
                    if let Some(p) = Config::path() {
                        open_file(&p);
                    }
                } else if id == quit_item.id() {
                    save(&cfg);
                    send(Cmd::Quit);
                    if let Some(w) = worker.take() {
                        let _ = w.join();
                    }
                    tray = None;
                    *control_flow = ControlFlow::Exit;
                    return;
                }
            }
            Event::UserEvent(UserEvent::HotKey(e)) if e.state == HotKeyState::Pressed => {
                match shortcuts.action(e.id) {
                    Some(Action::Up) => send(Cmd::Step { up: true }),
                    Some(Action::Down) => send(Cmd::Step { up: false }),
                    Some(Action::ToggleAuto) => {
                        let on = !cfg.auto;
                        set_auto(&mut cfg, on, &mut save_at);
                    }
                    None => {}
                }
            }
            Event::UserEvent(UserEvent::Status(s)) => {
                let text = status_text(&s);
                status_item.set_text(&text);
                up_item.set_enabled(s.connected);
                down_item.set_enabled(s.connected);
                if let Some(t) = &tray {
                    let _ = t.set_tooltip(Some(&text));
                }
                // The worker learns (sync offset, curve points): persist.
                if s.sync_cal != cfg.sync_cal || s.curve != cfg.curve {
                    cfg.sync_cal = s.sync_cal;
                    cfg.curve = s.curve.clone();
                    mark_dirty(&mut save_at);
                }
                if s.manual_seq != last_seq {
                    last_seq = s.manual_seq;
                    if let (true, Some(o), Some(level)) = (cfg.osd, osd.as_mut(), s.level) {
                        o.show(
                            ui::lg_monitor(target),
                            level_to_percent(level),
                            mode_short(&s),
                        );
                    }
                }
                if let Some(p) = panel.as_mut() {
                    p.set_state(panel_state(&cfg, Some(&s)));
                }
                if let Some(e) = editor.as_mut() {
                    e.update(s.lux, &s.curve);
                }
                last_status = Some(s);
            }
            Event::WindowEvent {
                window_id, event, ..
            } => {
                if let Some(p) = panel.as_mut().filter(|p| p.id() == window_id) {
                    match p.handle(&event) {
                        Some(PanelAction::SetPercent(pct)) => {
                            send(Cmd::SetLevel(percent_to_level(pct)))
                        }
                        Some(PanelAction::ToggleAuto) => {
                            let on = !cfg.auto;
                            set_auto(&mut cfg, on, &mut save_at);
                        }
                        Some(PanelAction::ToggleSync) => {
                            let on = !cfg.sync_laptop;
                            set_sync(&mut cfg, on, &mut save_at);
                        }
                        Some(PanelAction::ToggleSource) => {
                            let laptop = cfg.light_source != LightSource::Laptop;
                            set_source(&mut cfg, laptop, &mut save_at);
                        }
                        Some(PanelAction::EditCurve) => {
                            open_editor(&cfg, &mut editor, last_status.as_ref())
                        }
                        None => {}
                    }
                    if let Some(p) = panel.as_mut() {
                        p.set_state(panel_state(&cfg, last_status.as_ref()));
                    }
                }
                if let Some(e) = editor.as_mut().filter(|e| e.id() == window_id) {
                    match e.handle(&event) {
                        Some(EditorAction::Changed(pts)) => {
                            cfg.curve = pts.clone();
                            send(Cmd::SetCurve(pts));
                            mark_dirty(&mut save_at);
                        }
                        Some(EditorAction::Close) => editor = None,
                        None => {}
                    }
                }
                if let Some(c) = capture.as_mut().filter(|c| c.id() == window_id) {
                    match c.handle(&event) {
                        Some(CaptureResult::Cancel) => capture = None,
                        Some(CaptureResult::Chosen(spec)) => {
                            let a = c.action;
                            match shortcuts.set(a, &spec) {
                                Ok(()) => {
                                    set_spec(&mut cfg, a, spec);
                                    relabel(&cfg);
                                    mark_dirty(&mut save_at);
                                    capture = None;
                                }
                                Err(msg) => c.set_message(&msg),
                            }
                        }
                        None => {}
                    }
                }
            }
            Event::RedrawRequested(id) => {
                if let Some(o) = osd.as_mut().filter(|o| o.id() == id) {
                    o.draw();
                }
                if let Some(p) = panel.as_mut().filter(|p| p.id() == id && p.visible()) {
                    p.draw();
                }
                if let Some(e) = editor.as_mut().filter(|e| e.id() == id) {
                    e.draw();
                }
                if let Some(c) = capture.as_mut().filter(|c| c.id() == id) {
                    c.draw();
                }
            }
            _ => {}
        }

        let now = Instant::now();
        if save_at.is_some_and(|t| now >= t) {
            save(&cfg);
            save_at = None;
        }
        let wake = [osd.as_mut().and_then(|o| o.tick(now)), save_at]
            .into_iter()
            .flatten()
            .min();
        *control_flow = match wake {
            Some(t) => ControlFlow::WaitUntil(t),
            None => ControlFlow::Wait,
        };
    });
}

fn panel_state(cfg: &Config, s: Option<&Status>) -> PanelState {
    PanelState {
        connected: s.is_some_and(|s| s.connected),
        percent: s.and_then(|s| s.level).map(level_to_percent).unwrap_or(0.0),
        auto: cfg.auto,
        sync: cfg.sync_laptop,
        laptop_sensor: cfg.light_source == LightSource::Laptop,
        status: s.map(panel_status).unwrap_or_default(),
    }
}

fn panel_status(s: &Status) -> String {
    let src = match s.source {
        LightSource::Display => "LG sensor",
        LightSource::Laptop => "laptop sensor",
    };
    match s.mode {
        Mode::Manual => "manual".into(),
        Mode::LaptopSync => s
            .laptop
            .map(|l| format!("follows laptop {} %", l.panel))
            .unwrap_or_default(),
        Mode::LightSensor if !s.light_sensor && s.source == LightSource::Display => {
            "LG sensor not readable".into()
        }
        Mode::LightSensor => match s.lux {
            Some(l) => format!("auto · {l:.0} lx ({src})"),
            None => format!("auto · waiting for {src}"),
        },
    }
}

/// Polls the laptop panel twice a second and forwards changes.
fn laptop_poller(tx: mpsc::Sender<Cmd>) {
    let mut reader = match laptop::Reader::new() {
        Ok(r) => r,
        Err(e) => {
            log::warn!("laptop panel reader unavailable: {e:#}");
            return;
        }
    };
    let mut last = None;
    loop {
        let now = reader.read();
        if now != last {
            log::debug!("laptop: {now:?}");
            if tx.send(Cmd::Laptop(now)).is_err() {
                return;
            }
            last = now;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn save(cfg: &Config) {
    if let Err(e) = cfg.save() {
        log::warn!("saving config: {e:#}");
    }
}

fn open_file(p: &std::path::Path) {
    let r = if cfg!(windows) {
        std::process::Command::new("notepad.exe").arg(p).spawn()
    } else {
        std::process::Command::new("open").arg("-t").arg(p).spawn()
    };
    if let Err(e) = r {
        log::warn!("opening {}: {e}", p.display());
    }
}

fn mode_short(s: &Status) -> &'static str {
    match s.mode {
        Mode::Manual => "manual",
        Mode::LightSensor if !s.light_sensor && s.source == LightSource::Display => "manual",
        Mode::LightSensor => "auto",
        Mode::LaptopSync => "follows laptop",
    }
}

fn status_text(s: &Status) -> String {
    if !s.connected {
        return "LG UltraFine: not connected".into();
    }
    let pct = s
        .level
        .map(|l| format!("{:.0} %", level_to_percent(l)))
        .unwrap_or("?".into());
    format!("LG UltraFine: {pct} ({})", panel_status(s))
}

/// 32x32 sun glyph, drawn in code so there are no asset files.
fn icon() -> Icon {
    const N: u32 = 32;
    let mut rgba = vec![0u8; (N * N * 4) as usize];
    let c = (N as f32 - 1.0) / 2.0;
    for y in 0..N {
        for x in 0..N {
            let (dx, dy) = (x as f32 - c, y as f32 - c);
            let r = (dx * dx + dy * dy).sqrt();
            let angle = dy.atan2(dx);
            let ray =
                r > 10.0 && r < 15.0 && (angle * 4.0 / std::f32::consts::PI).fract().abs() < 0.3;
            if r < 7.5 || ray {
                let i = ((y * N + x) * 4) as usize;
                rgba[i..i + 4].copy_from_slice(&[0xF5, 0xB7, 0x00, 0xFF]);
            }
        }
    }
    Icon::from_rgba(rgba, N, N).expect("valid icon")
}
