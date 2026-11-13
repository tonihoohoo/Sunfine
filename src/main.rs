//! ultrafine-ctl: tray app with brightness hotkeys and automatic brightness
//! for the LG UltraFine 5K on Windows. See CLAUDE.md and docs/PROTOCOL.md.
//! (Also builds on macOS for development; not released there.)

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod config;
mod curve;
mod device;
mod display_fix;
mod laptop;
mod ui;
mod worker;

use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Instant;

use anyhow::{Context, Result};
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use config::{Config, Hotkeys};
use curve::Mode;
use ui::{Action, Capture, CaptureResult, Osd, Painter};
use worker::{Cmd, Status};

#[derive(Debug)]
enum UserEvent {
    Menu(MenuEvent),
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
    GlobalHotKeyEvent::set_event_handler(Some(move |e| {
        let _ = proxy.send_event(UserEvent::HotKey(e));
    }));

    // Tray menu (the icon itself is created once the loop runs; macOS needs that).
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
    let mut capture: Option<Capture> = None;
    let mut last_seq = 0u64;

    event_loop.run(move |event, target, control_flow| {
        let send = |c: Cmd| {
            let _ = tx.send(c);
        };
        let set_auto = |cfg: &mut Config, on: bool| {
            cfg.auto = on;
            auto_item.set_checked(on);
            send(Cmd::SetAuto(on));
            save(cfg);
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
                // Visual check without the display: UF_DEMO_OSD=1.
                if std::env::var_os("UF_DEMO_OSD").is_some() {
                    if let Some(o) = osd.as_mut() {
                        o.show(ui::lg_monitor(target), 42.0, "demo");
                    }
                }
            }
            Event::UserEvent(UserEvent::Menu(e)) => {
                let id = e.id();
                if id == up_item.id() {
                    send(Cmd::Step { up: true });
                } else if id == down_item.id() {
                    send(Cmd::Step { up: false });
                } else if id == auto_item.id() {
                    // The check item already toggled itself; follow it.
                    set_auto(&mut cfg, auto_item.is_checked());
                } else if id == sync_item.id() {
                    cfg.sync_laptop = sync_item.is_checked();
                    send(Cmd::SetSync(cfg.sync_laptop));
                    save(&cfg);
                } else if id == fix_item.id() {
                    cfg.fix_black_screen = fix_item.is_checked();
                    fix_enabled.store(cfg.fix_black_screen, Ordering::Relaxed);
                    save(&cfg);
                } else if id == osd_item.id() {
                    cfg.osd = osd_item.is_checked();
                    save(&cfg);
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
                    save(&cfg);
                } else if id == open_cfg_item.id() {
                    if let Some(p) = Config::path() {
                        open_file(&p);
                    }
                } else if id == quit_item.id() {
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
                        set_auto(&mut cfg, on);
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
                if s.sync_cal != cfg.sync_cal {
                    cfg.sync_cal = s.sync_cal;
                    save(&cfg);
                }
                if s.manual_seq != last_seq {
                    last_seq = s.manual_seq;
                    if let (true, Some(o), Some(level)) = (cfg.osd, osd.as_mut(), s.level) {
                        let pct = level as f64 * 100.0 / device::MAX_LEVEL as f64;
                        o.show(ui::lg_monitor(target), pct, mode_short(&s));
                    }
                }
            }
            Event::WindowEvent {
                window_id, event, ..
            } => {
                if let Some(c) = capture.as_mut().filter(|c| c.id() == window_id) {
                    match c.handle(&event) {
                        Some(CaptureResult::Cancel) => capture = None,
                        Some(CaptureResult::Chosen(spec)) => {
                            let a = c.action;
                            match shortcuts.set(a, &spec) {
                                Ok(()) => {
                                    set_spec(&mut cfg, a, spec);
                                    relabel(&cfg);
                                    save(&cfg);
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
                if let Some(c) = capture.as_mut().filter(|c| c.id() == id) {
                    c.draw();
                }
            }
            _ => {}
        }

        *control_flow = match osd.as_mut().and_then(|o| o.tick(Instant::now())) {
            Some(t) => ControlFlow::WaitUntil(t),
            None => ControlFlow::Wait,
        };
    });
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
        std::thread::sleep(std::time::Duration::from_millis(500));
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
        Mode::Paused => "auto paused",
        Mode::LightSensor if !s.light_sensor => "manual",
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
        .map(|l| format!("{:.0} %", l as f64 * 100.0 / device::MAX_LEVEL as f64))
        .unwrap_or("?".into());
    let mode = match s.mode {
        Mode::Manual => "manual".to_string(),
        Mode::Paused => "auto paused".to_string(),
        Mode::LightSensor if !s.light_sensor => "auto: light sensor not readable".to_string(),
        Mode::LightSensor => s
            .lux
            .map(|l| format!("auto, {l:.0} lx"))
            .unwrap_or("auto".into()),
        Mode::LaptopSync => s
            .laptop
            .map(|l| format!("follows laptop {} %", l.panel))
            .unwrap_or_default(),
    };
    format!("LG UltraFine: {pct} ({mode})")
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
