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
mod theme;
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
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use config::{AutoMode, Config, Hotkeys, LightSource};
use curve::{level_to_percent, percent_to_level, Mode};
use editor::{CurveEditor, EditorAction};
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
    /// Unregistered while the capture window is open (Windows would
    /// otherwise swallow a combination that is already registered).
    suspended: bool,
}

impl Shortcuts {
    fn new(cfg: &Hotkeys) -> Result<Self> {
        let mut s = Self {
            manager: GlobalHotKeyManager::new().context("hotkey manager")?,
            up: None,
            down: None,
            toggle: None,
            suspended: false,
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

    fn all(&self) -> Vec<HotKey> {
        [self.up, self.down, self.toggle]
            .into_iter()
            .flatten()
            .collect()
    }

    fn suspend(&mut self) {
        if !self.suspended {
            for h in self.all() {
                let _ = self.manager.unregister(h);
            }
            self.suspended = true;
        }
    }

    fn resume(&mut self) {
        if self.suspended {
            for h in self.all() {
                let _ = self.manager.register(h);
            }
            self.suspended = false;
        }
    }

    /// Replaces all three at once (so swapped combinations don't collide
    /// with each other). Returns errors for the ones that failed.
    fn set_all(&mut self, hk: &Hotkeys) -> Vec<String> {
        self.resume();
        for h in self.all() {
            let _ = self.manager.unregister(h);
        }
        self.up = None;
        self.down = None;
        self.toggle = None;
        [
            (Action::Up, &hk.up),
            (Action::Down, &hk.down),
            (Action::ToggleAuto, &hk.toggle_auto),
        ]
        .into_iter()
        .filter_map(|(a, spec)| self.set(a, spec).err())
        .collect()
    }

    fn action(&self, id: u32) -> Option<Action> {
        if self.suspended {
            return None;
        }
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
    init_logging();
    if let Err(e) = run() {
        log::error!("{e:#}");
        std::process::exit(1);
    }
}

/// Brightness presets in the menu (percent), top to bottom.
const PRESETS: [u8; 11] = [100, 90, 80, 70, 60, 50, 40, 30, 20, 10, 5];

/// Native tray menu. Windows closes it after each click; items that do not
/// apply right now are disabled (greyed) with a short reason in the text.
struct Menus {
    status: MenuItem,
    brightness: Submenu,
    presets: Vec<(u8, CheckMenuItem)>,
    up: MenuItem,
    down: MenuItem,
    mode_manual: CheckMenuItem,
    mode_sensor: CheckMenuItem,
    mode_follow: CheckMenuItem,
    sensor: Submenu,
    sensor_lg: CheckMenuItem,
    sensor_laptop: CheckMenuItem,
    curve: MenuItem,
    sc_up: MenuItem,
    sc_down: MenuItem,
    sc_auto: MenuItem,
    sc_reset: MenuItem,
    osd: CheckMenuItem,
    fix: CheckMenuItem,
    open_cfg: MenuItem,
    quit: MenuItem,
    /// What the menu currently shows. Windows repaints an open menu on every
    /// item change (visible flicker), so only changed items are touched.
    shown: std::cell::RefCell<Option<MenuView>>,
}

impl Menus {
    fn new() -> Result<(Self, Menu)> {
        let presets: Vec<(u8, CheckMenuItem)> = PRESETS
            .iter()
            .map(|&p| (p, CheckMenuItem::new(format!("{p} %"), true, false, None)))
            .collect();
        let m = Self {
            status: MenuItem::new("LG UltraFine: searching…", false, None),
            brightness: Submenu::new("Brightness", true),
            presets,
            up: MenuItem::new("", true, None),
            down: MenuItem::new("", true, None),
            mode_manual: CheckMenuItem::new("Manual", true, false, None),
            mode_sensor: CheckMenuItem::new("Automatic – light sensor", true, false, None),
            mode_follow: CheckMenuItem::new("Automatic – follow laptop screen", true, false, None),
            sensor: Submenu::new("Light sensor", true),
            sensor_lg: CheckMenuItem::new("LG display", true, false, None),
            sensor_laptop: CheckMenuItem::new("Laptop", true, false, None),
            curve: MenuItem::new("Brightness curve…", true, None),
            sc_up: MenuItem::new("", true, None),
            sc_down: MenuItem::new("", true, None),
            sc_auto: MenuItem::new("", true, None),
            sc_reset: MenuItem::new("Reset to defaults", true, None),
            osd: CheckMenuItem::new("Popup on brightness keys", true, false, None),
            fix: CheckMenuItem::new("Fix black screen on connect", cfg!(windows), false, None),
            open_cfg: MenuItem::new("Open settings file", true, None),
            quit: MenuItem::new("Quit", true, None),
            shown: std::cell::RefCell::new(None),
        };
        for (_, item) in &m.presets {
            m.brightness.append(item)?;
        }
        m.sensor.append_items(&[&m.sensor_lg, &m.sensor_laptop])?;
        let mode = Submenu::with_items(
            "Mode",
            true,
            &[&m.mode_manual, &m.mode_sensor, &m.mode_follow],
        )?;
        let shortcuts = Submenu::with_items(
            "Shortcuts",
            true,
            &[
                &m.sc_up,
                &m.sc_down,
                &m.sc_auto,
                &PredefinedMenuItem::separator(),
                &m.sc_reset,
            ],
        )?;
        let menu = Menu::new();
        menu.append_items(&[
            &m.status,
            &PredefinedMenuItem::separator(),
            &m.brightness,
            &m.up,
            &m.down,
            &PredefinedMenuItem::separator(),
            &mode,
            &m.sensor,
            &m.curve,
            &PredefinedMenuItem::separator(),
            &shortcuts,
            &m.osd,
            &m.fix,
            &m.open_cfg,
            &PredefinedMenuItem::separator(),
            &m.quit,
        ])?;
        Ok((m, menu))
    }

    /// Brings every item in line with the settings and the device state.
    /// Also undoes the self-toggle of check items used as radio buttons.
    /// After a click: check items toggled themselves, so re-apply all.
    fn refresh(&self, cfg: &Config, s: Option<&Status>) {
        self.shown.replace(None);
        self.update(cfg, s);
    }

    /// Applies the view, touching only items whose state changed.
    fn update(&self, cfg: &Config, s: Option<&Status>) {
        let v = menu_view(cfg, s);
        let old = self.shown.borrow().clone();
        if old.as_ref() == Some(&v) {
            return;
        }
        // `ch(f)`: field differs from what is shown (or nothing shown yet).
        let ch = |f: &dyn Fn(&MenuView) -> bool| old.as_ref().is_none_or(f);
        if ch(&|o| o.status != v.status) {
            self.status.set_text(&v.status);
        }
        if ch(&|o| o.connected != v.connected) {
            self.brightness.set_enabled(v.connected);
            self.up.set_enabled(v.connected);
            self.down.set_enabled(v.connected);
        }
        if ch(&|o| o.brightness != v.brightness) {
            self.brightness.set_text(&v.brightness);
        }
        if ch(&|o| o.preset != v.preset) {
            for (p, item) in &self.presets {
                item.set_checked(Some(*p) == v.preset);
            }
        }
        if ch(&|o| o.up != v.up) {
            self.up.set_text(&v.up);
        }
        if ch(&|o| o.down != v.down) {
            self.down.set_text(&v.down);
        }
        if ch(&|o| o.mode != v.mode) {
            self.mode_manual.set_checked(v.mode == ModeView::Manual);
            self.mode_sensor
                .set_checked(v.mode == ModeView::LightSensor);
            self.mode_follow
                .set_checked(v.mode == ModeView::FollowLaptop);
        }
        if ch(&|o| o.follow_enabled != v.follow_enabled) {
            self.mode_follow.set_enabled(v.follow_enabled);
        }
        if ch(&|o| o.follow != v.follow) {
            self.mode_follow.set_text(&v.follow);
        }
        if ch(&|o| o.sensor_enabled != v.sensor_enabled) {
            self.sensor.set_enabled(v.sensor_enabled);
        }
        if ch(&|o| o.sensor != v.sensor) {
            self.sensor.set_text(&v.sensor);
        }
        if ch(&|o| o.sensor_laptop != v.sensor_laptop) {
            self.sensor_lg.set_checked(!v.sensor_laptop);
            self.sensor_laptop.set_checked(v.sensor_laptop);
        }
        if ch(&|o| o.sensor_laptop_enabled != v.sensor_laptop_enabled) {
            self.sensor_laptop.set_enabled(v.sensor_laptop_enabled);
        }
        if ch(&|o| o.sensor_laptop_text != v.sensor_laptop_text) {
            self.sensor_laptop.set_text(&v.sensor_laptop_text);
        }
        if ch(&|o| o.curve_enabled != v.curve_enabled) {
            self.curve.set_enabled(v.curve_enabled);
        }
        if ch(&|o| o.shortcuts != v.shortcuts) {
            self.sc_up.set_text(&v.shortcuts[0]);
            self.sc_down.set_text(&v.shortcuts[1]);
            self.sc_auto.set_text(&v.shortcuts[2]);
        }
        if ch(&|o| o.osd != v.osd) {
            self.osd.set_checked(v.osd);
        }
        if ch(&|o| o.fix != v.fix) {
            self.fix.set_checked(v.fix);
        }
        self.shown.replace(Some(v));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModeView {
    Manual,
    LightSensor,
    FollowLaptop,
}

/// What the menu shows: pure function of settings + device state (tested).
#[derive(Debug, Clone, PartialEq)]
struct MenuView {
    status: String,
    connected: bool,
    brightness: String,
    /// Preset to tick (nearest to the current level).
    preset: Option<u8>,
    up: String,
    down: String,
    mode: ModeView,
    follow: String,
    follow_enabled: bool,
    sensor: String,
    sensor_enabled: bool,
    sensor_laptop: bool,
    sensor_laptop_text: String,
    sensor_laptop_enabled: bool,
    curve_enabled: bool,
    shortcuts: [String; 3],
    osd: bool,
    fix: bool,
}

fn menu_view(cfg: &Config, s: Option<&Status>) -> MenuView {
    let connected = s.is_some_and(|s| s.connected);
    let laptop = s.and_then(|s| s.laptop);
    let follow_available = laptop.is_some_and(|l| l.lid_open && l.panel > 0);
    let laptop_sensor_available = laptop.is_some_and(|l| l.lid_open && l.lux.is_some());
    let mode = match (cfg.auto, cfg.auto_mode) {
        (false, _) => ModeView::Manual,
        (true, AutoMode::LightSensor) => ModeView::LightSensor,
        (true, AutoMode::FollowLaptop) => ModeView::FollowLaptop,
    };
    // The light sensor is in use in light-sensor mode, and in follow mode
    // while it falls back (lid closed / no laptop data).
    let sensor_in_use = match mode {
        ModeView::Manual => false,
        ModeView::LightSensor => true,
        ModeView::FollowLaptop => s.is_none_or(|s| s.mode == Mode::LightSensor),
    };
    let pct = s
        .filter(|s| s.connected)
        .and_then(|s| s.level)
        .map(level_to_percent);
    let preset = pct.and_then(|p| {
        PRESETS
            .iter()
            .copied()
            .min_by(|a, b| (*a as f64 - p).abs().total_cmp(&(*b as f64 - p).abs()))
    });
    let follow_selected = mode == ModeView::FollowLaptop;
    let laptop_src = cfg.light_source == LightSource::Laptop;
    MenuView {
        status: s
            .map(status_text)
            .unwrap_or_else(|| "LG UltraFine: searching…".into()),
        connected,
        brightness: pct
            .map(|p| format!("Brightness: {p:.0} %"))
            .unwrap_or_else(|| "Brightness".into()),
        preset,
        up: format!("Brighter\t{}", cfg.hotkeys.up),
        down: format!("Darker\t{}", cfg.hotkeys.down),
        mode,
        follow: if follow_available {
            "Automatic – follow laptop screen".into()
        } else if follow_selected {
            "Automatic – follow laptop screen (lid closed: using light sensor)".into()
        } else {
            "Automatic – follow laptop screen (needs the lid open)".into()
        },
        follow_enabled: follow_available || follow_selected,
        sensor: if sensor_in_use {
            "Light sensor".into()
        } else {
            "Light sensor (not used now)".into()
        },
        sensor_enabled: sensor_in_use,
        sensor_laptop: laptop_src,
        sensor_laptop_text: if laptop_sensor_available {
            "Laptop".into()
        } else if laptop_src {
            "Laptop (lid closed: using LG display)".into()
        } else {
            "Laptop (needs the lid open)".into()
        },
        sensor_laptop_enabled: laptop_sensor_available || laptop_src,
        curve_enabled: sensor_in_use,
        shortcuts: [
            format!("Brighter: {}…", cfg.hotkeys.up),
            format!("Darker: {}…", cfg.hotkeys.down),
            format!("Automatic on/off: {}…", cfg.hotkeys.toggle_auto),
        ],
        osd: cfg.osd,
        fix: cfg.fix_black_screen,
    }
}

fn run() -> Result<()> {
    let (mut cfg, broken) = match Config::load_or_create() {
        Ok(c) => (c, false),
        Err(e) => {
            log::error!("config: {e:#}; using defaults and NOT saving until the file is fixed");
            (Config::default(), true)
        }
    };
    let mut store = Store {
        broken,
        mtime: Config::mtime(),
    };
    log::info!(
        "ultrafine-ctl {} – config: {}",
        env!("CARGO_PKG_VERSION"),
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

    let (menus, menu) = Menus::new()?;
    menus.update(&cfg, None);

    let mut menu = Some(menu);
    let mut tray: Option<TrayIcon> = None;
    let mut worker = Some(worker);
    let painter = Rc::new(Painter::new());
    let mut osd: Option<Osd> = None;
    let mut editor: Option<CurveEditor> = None;
    let mut capture: Option<Capture> = None;
    let mut last_seq = 0u64;
    let mut last_status: Option<Status> = None;
    // Config writes are batched (learning can change the curve often).
    let mut save_at: Option<Instant> = None;
    let mut next_file_check = Instant::now() + Duration::from_secs(2);
    let mut tooltip = String::new();

    event_loop.run(move |event, target, control_flow| {
        let send = |c: Cmd| {
            let _ = tx.send(c);
        };
        let mark_dirty = |save_at: &mut Option<Instant>| {
            save_at.get_or_insert(Instant::now() + Duration::from_secs(2));
        };
        let set_auto = |cfg: &mut Config, on: bool, save_at: &mut Option<Instant>| {
            cfg.auto = on;
            send(Cmd::SetAuto(on));
            mark_dirty(save_at);
        };
        let set_mode = |cfg: &mut Config, m: AutoMode, save_at: &mut Option<Instant>| {
            cfg.auto_mode = m;
            send(Cmd::SetAutoMode(m));
            set_auto(cfg, true, save_at);
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
                    .with_menu_on_left_click(true)
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
                let m = &menus;
                if let Some((p, _)) = m.presets.iter().find(|(_, i)| i.id() == id) {
                    send(Cmd::SetLevel(percent_to_level(*p as f64)));
                } else if id == m.up.id() {
                    send(Cmd::Step { up: true });
                } else if id == m.down.id() {
                    send(Cmd::Step { up: false });
                } else if id == m.mode_manual.id() {
                    set_auto(&mut cfg, false, &mut save_at);
                } else if id == m.mode_sensor.id() {
                    set_mode(&mut cfg, AutoMode::LightSensor, &mut save_at);
                } else if id == m.mode_follow.id() {
                    set_mode(&mut cfg, AutoMode::FollowLaptop, &mut save_at);
                } else if id == m.sensor_lg.id() || id == m.sensor_laptop.id() {
                    cfg.light_source = if id == m.sensor_laptop.id() {
                        LightSource::Laptop
                    } else {
                        LightSource::Display
                    };
                    send(Cmd::SetSource(cfg.light_source));
                    mark_dirty(&mut save_at);
                } else if id == m.curve.id() {
                    open_editor(&cfg, &mut editor, last_status.as_ref());
                } else if id == m.sc_up.id() {
                    shortcuts.suspend();
                    start_capture(Action::Up, &cfg, &mut capture);
                } else if id == m.sc_down.id() {
                    shortcuts.suspend();
                    start_capture(Action::Down, &cfg, &mut capture);
                } else if id == m.sc_auto.id() {
                    shortcuts.suspend();
                    start_capture(Action::ToggleAuto, &cfg, &mut capture);
                } else if id == m.sc_reset.id() {
                    for e in shortcuts.set_all(&Hotkeys::default()) {
                        log::warn!("{e}");
                    }
                    cfg.hotkeys = Hotkeys::default();
                    mark_dirty(&mut save_at);
                } else if id == m.osd.id() {
                    cfg.osd = !cfg.osd;
                    mark_dirty(&mut save_at);
                } else if id == m.fix.id() {
                    cfg.fix_black_screen = !cfg.fix_black_screen;
                    fix_enabled.store(cfg.fix_black_screen, Ordering::Relaxed);
                    mark_dirty(&mut save_at);
                } else if id == m.open_cfg.id() {
                    store.save(&cfg);
                    save_at = None;
                    if let Some(p) = Config::path() {
                        open_file(&p);
                    }
                } else if id == m.quit.id() {
                    if save_at.is_some() {
                        store.save(&cfg);
                    }
                    send(Cmd::Quit);
                    if let Some(w) = worker.take() {
                        let _ = w.join();
                    }
                    tray = None;
                    *control_flow = ControlFlow::Exit;
                    return;
                }
                menus.refresh(&cfg, last_status.as_ref());
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
                menus.update(&cfg, last_status.as_ref());
            }
            Event::UserEvent(UserEvent::Status(s)) => {
                if let Some(t) = &tray {
                    // Only on change: a tooltip refresh flickers while it is shown.
                    let text = status_text(&s);
                    if text != tooltip {
                        let _ = t.set_tooltip(Some(&text));
                        tooltip = text;
                    }
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
                if let Some(e) = editor.as_mut() {
                    e.update(s.lux, &s.curve);
                }
                menus.update(&cfg, Some(&s));
                last_status = Some(s);
            }
            Event::WindowEvent {
                window_id, event, ..
            } => {
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
                        Some(CaptureResult::Cancel) => {
                            capture = None;
                            shortcuts.resume();
                        }
                        Some(CaptureResult::Chosen(spec)) => {
                            let a = c.action;
                            let mut new = cfg.clone();
                            set_spec(&mut new, a, spec.clone());
                            let clash = [Action::Up, Action::Down, Action::ToggleAuto]
                                .into_iter()
                                .find(|&o| o != a && spec_of(&cfg, o).eq_ignore_ascii_case(&spec));
                            if let Some(o) = clash {
                                c.set_message(&format!(
                                    "{spec} is already used for {}.",
                                    o.label()
                                ));
                            } else {
                                let errors = shortcuts.set_all(&new.hotkeys);
                                if errors.is_empty() {
                                    cfg.hotkeys = new.hotkeys;
                                    menus.update(&cfg, last_status.as_ref());
                                    mark_dirty(&mut save_at);
                                    capture = None;
                                } else {
                                    // Put the old ones back and keep asking.
                                    shortcuts.set_all(&cfg.hotkeys);
                                    shortcuts.suspend();
                                    c.set_message(&errors.join(" "));
                                }
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
        // Edited by hand while running? Take the file's version.
        if now >= next_file_check {
            next_file_check = now + Duration::from_secs(2);
            if store.changed_on_disk() {
                store.mtime = Config::mtime();
                match Config::load() {
                    Ok(Some(new)) => {
                        log::info!("settings file changed on disk; reloaded");
                        for e in shortcuts.set_all(&new.hotkeys) {
                            log::warn!("{e}");
                        }
                        fix_enabled.store(new.fix_black_screen, Ordering::Relaxed);
                        send(Cmd::Reload(Box::new(new.clone())));
                        cfg = new;
                        store.broken = false;
                        save_at = None;
                        menus.update(&cfg, last_status.as_ref());
                    }
                    Ok(None) => {}
                    Err(e) => {
                        log::error!(
                            "settings file has an error, not saving until it is fixed: {e:#}"
                        );
                        store.broken = true;
                    }
                }
            }
        }
        if save_at.is_some_and(|t| now >= t) {
            store.save(&cfg);
            save_at = None;
        }
        let wake = [
            osd.as_mut().and_then(|o| o.tick(now)),
            save_at,
            Some(next_file_check),
        ]
        .into_iter()
        .flatten()
        .min();
        *control_flow = match wake {
            Some(t) => ControlFlow::WaitUntil(t),
            None => ControlFlow::Wait,
        };
    });
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
            .map(|l| format!("follows laptop ({} %)", l.panel))
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

/// Logs to stderr and to `ultrafine-ctl.log` next to the config (the
/// Windows release has no console). The file is restarted when over 1 MB.
fn init_logging() {
    use std::io::Write;
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    let file = Config::path().and_then(|p| {
        let path = p.with_file_name("ultrafine-ctl.log");
        std::fs::create_dir_all(path.parent()?).ok()?;
        let big = std::fs::metadata(&path).is_ok_and(|m| m.len() > 1_000_000);
        std::fs::OpenOptions::new()
            .create(true)
            .append(!big)
            .write(true)
            .truncate(big)
            .open(path)
            .ok()
    });
    if let Some(file) = file {
        let file = std::sync::Mutex::new(file);
        builder.format(move |buf, rec| {
            let line = format!(
                "{} {:<5} {}",
                buf.timestamp_millis(),
                rec.level(),
                rec.args()
            );
            if let Ok(mut f) = file.lock() {
                let _ = writeln!(f, "{line}");
            }
            writeln!(buf, "{line}")
        });
    }
    builder.init();
}

/// Writes the settings file, but never over a file the user broke or over
/// edits made since we last read or wrote it.
struct Store {
    broken: bool,
    mtime: Option<std::time::SystemTime>,
}

impl Store {
    fn changed_on_disk(&self) -> bool {
        Config::mtime() != self.mtime
    }

    fn save(&mut self, cfg: &Config) {
        if self.broken {
            log::warn!("not saving: the settings file has an error (fix it or delete it)");
            return;
        }
        if self.changed_on_disk() {
            log::info!("settings file changed on disk; not overwriting it");
            return;
        }
        match cfg.save() {
            Ok(()) => self.mtime = Config::mtime(),
            Err(e) => log::warn!("saving config: {e:#}"),
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::laptop::Laptop;

    fn status(mode: Mode, laptop: Option<Laptop>, level: u32) -> Status {
        Status {
            connected: true,
            light_sensor: true,
            level: Some(level),
            lux: Some(18.0),
            auto: mode != Mode::Manual,
            mode,
            source: LightSource::Display,
            laptop,
            sync_cal: None,
            curve: config::default_curve(),
            manual_seq: 0,
        }
    }

    fn lid(open: bool, lux: Option<f64>) -> Option<Laptop> {
        Some(Laptop {
            panel: 60,
            adaptive: true,
            lid_open: open,
            lux,
        })
    }

    #[test]
    fn disconnected_greys_brightness() {
        let v = menu_view(&Config::default(), None);
        assert!(!v.connected);
        assert_eq!(v.preset, None);
        assert_eq!(v.brightness, "Brightness");
        let mut s = status(Mode::LightSensor, None, 14307);
        s.connected = false;
        let v = menu_view(&Config::default(), Some(&s));
        assert!(!v.connected);
        assert_eq!(v.preset, None);
    }

    #[test]
    fn preset_tick_is_nearest() {
        let cfg = Config {
            auto: false,
            ..Config::default()
        };
        for (level, want) in [
            (14307, 30),
            (54000, 100),
            (100, 5),
            (27000, 50),
            (2000, 5),
            (4000, 5),
            (4500, 10),
        ] {
            let v = menu_view(&cfg, Some(&status(Mode::Manual, None, level)));
            assert_eq!(v.preset, Some(want), "level {level}");
        }
    }

    #[test]
    fn manual_mode_greys_sensor_and_curve() {
        let cfg = Config {
            auto: false,
            ..Config::default()
        };
        let v = menu_view(
            &cfg,
            Some(&status(Mode::Manual, lid(true, Some(10.0)), 14307)),
        );
        assert_eq!(v.mode, ModeView::Manual);
        assert!(!v.sensor_enabled);
        assert!(!v.curve_enabled);
        assert!(v.follow_enabled, "can switch to follow with lid open");
    }

    #[test]
    fn follow_mode_states() {
        let cfg = Config {
            auto: true,
            auto_mode: AutoMode::FollowLaptop,
            ..Config::default()
        };
        // Following: sensor not used.
        let v = menu_view(
            &cfg,
            Some(&status(Mode::LaptopSync, lid(true, None), 14307)),
        );
        assert_eq!(v.mode, ModeView::FollowLaptop);
        assert!(!v.sensor_enabled && !v.curve_enabled);
        assert_eq!(v.follow, "Automatic – follow laptop screen");
        // Lid closed: falls back, sensor + curve usable, follow stays selectable.
        let v = menu_view(
            &cfg,
            Some(&status(Mode::LightSensor, lid(false, None), 14307)),
        );
        assert!(v.sensor_enabled && v.curve_enabled);
        assert!(v.follow_enabled);
        assert!(v.follow.contains("lid closed"));
    }

    #[test]
    fn follow_unavailable_when_not_selected() {
        let cfg = Config {
            auto: true,
            auto_mode: AutoMode::LightSensor,
            ..Config::default()
        };
        let v = menu_view(
            &cfg,
            Some(&status(Mode::LightSensor, lid(false, None), 14307)),
        );
        assert!(!v.follow_enabled);
        assert!(v.follow.contains("needs the lid open"));
        let v = menu_view(&cfg, Some(&status(Mode::LightSensor, None, 14307)));
        assert!(!v.follow_enabled, "no laptop panel (desktop)");
    }

    #[test]
    fn laptop_sensor_option() {
        let cfg = Config {
            auto: true,
            auto_mode: AutoMode::LightSensor,
            ..Config::default()
        };
        let v = menu_view(
            &cfg,
            Some(&status(Mode::LightSensor, lid(true, Some(5.0)), 14307)),
        );
        assert!(v.sensor_laptop_enabled && !v.sensor_laptop);
        let v = menu_view(
            &cfg,
            Some(&status(Mode::LightSensor, lid(true, None), 14307)),
        );
        assert!(!v.sensor_laptop_enabled, "no laptop sensor");
        let cfg = Config {
            light_source: LightSource::Laptop,
            ..cfg
        };
        let v = menu_view(
            &cfg,
            Some(&status(Mode::LightSensor, lid(false, Some(5.0)), 14307)),
        );
        assert!(v.sensor_laptop && v.sensor_laptop_enabled);
        assert!(v.sensor_laptop_text.contains("lid closed"));
    }

    #[test]
    fn status_texts() {
        let s = status(Mode::LightSensor, None, 14307);
        assert_eq!(
            status_text(&s),
            "LG UltraFine: 26 % (auto · 18 lx (LG sensor))"
        );
        let mut s = status(Mode::LaptopSync, lid(true, None), 27000);
        assert_eq!(
            status_text(&s),
            "LG UltraFine: 50 % (follows laptop (60 %))"
        );
        s.connected = false;
        assert_eq!(status_text(&s), "LG UltraFine: not connected");
        let mut s = status(Mode::LightSensor, None, 14307);
        s.light_sensor = false;
        assert!(status_text(&s).contains("LG sensor not readable"));
    }

    #[test]
    fn shortcut_slots_round_trip() {
        let mut cfg = Config::default();
        for a in [Action::Up, Action::Down, Action::ToggleAuto] {
            set_spec(&mut cfg, a, format!("ctrl+F{}", a as u8 + 1));
        }
        assert_eq!(spec_of(&cfg, Action::Up), "ctrl+F1");
        assert_eq!(spec_of(&cfg, Action::Down), "ctrl+F2");
        assert_eq!(spec_of(&cfg, Action::ToggleAuto), "ctrl+F3");
    }

    #[test]
    fn menu_view_is_stable_under_small_changes() {
        // Flicker fix: tiny lux / laptop-lux changes must not change what
        // the menu shows (no item would need repainting).
        let cfg = Config { auto: true, auto_mode: AutoMode::LightSensor, ..Config::default() };
        let mut a = status(Mode::LightSensor, lid(true, Some(10.0)), 15556);
        let mut b = a.clone();
        a.lux = Some(18.2);
        b.lux = Some(18.4);
        b.laptop = lid(true, Some(11.0));
        assert_eq!(menu_view(&cfg, Some(&a)), menu_view(&cfg, Some(&b)));
    }
}

