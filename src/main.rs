//! ultrafine-ctl: tray app with brightness hotkeys and automatic brightness
//! for the LG UltraFine 5K. See CLAUDE.md and docs/PROTOCOL.md.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod config;
mod curve;
mod device;
mod display_fix;
mod worker;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

use anyhow::{Context, Result};
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use config::Config;
use worker::{Cmd, Status};

#[derive(Debug)]
enum UserEvent {
    Menu(MenuEvent),
    HotKey(GlobalHotKeyEvent),
    Status(Status),
}

struct HotKeyIds {
    up: Option<u32>,
    down: Option<u32>,
    toggle: Option<u32>,
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

    // Windows black-screen watchdog.
    let fix_enabled = Arc::new(AtomicBool::new(cfg.fix_black_screen));
    if cfg!(windows) {
        let flag = fix_enabled.clone();
        std::thread::Builder::new()
            .name("display-fix".into())
            .spawn(move || display_fix::watch(|| flag.load(Ordering::Relaxed)))
            .context("spawning display-fix thread")?;
    }

    // Hotkeys.
    let hotkeys = GlobalHotKeyManager::new().context("hotkey manager")?;
    let register = |name: &str, spec: &str| -> Option<u32> {
        match spec.parse::<HotKey>() {
            Ok(hk) => match hotkeys.register(hk) {
                Ok(()) => {
                    log::info!("hotkey {name}: {spec}");
                    Some(hk.id())
                }
                Err(e) => {
                    log::warn!("hotkey {name} ({spec}) not registered: {e}");
                    None
                }
            },
            Err(e) => {
                log::warn!("hotkey {name}: cannot parse {spec:?}: {e}");
                None
            }
        }
    };
    let ids = HotKeyIds {
        up: register("up", &cfg.hotkeys.up),
        down: register("down", &cfg.hotkeys.down),
        toggle: register("toggle_auto", &cfg.hotkeys.toggle_auto),
    };

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
    let up_item = MenuItem::new(format!("Brighter\t{}", cfg.hotkeys.up), true, None);
    let down_item = MenuItem::new(format!("Darker\t{}", cfg.hotkeys.down), true, None);
    let auto_item = CheckMenuItem::new(
        format!("Auto brightness\t{}", cfg.hotkeys.toggle_auto),
        true,
        cfg.auto,
        None,
    );
    let fix_item = CheckMenuItem::new(
        "Fix black screen on connect",
        cfg!(windows),
        cfg.fix_black_screen,
        None,
    );
    let quit_item = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    menu.append_items(&[
        &status_item,
        &PredefinedMenuItem::separator(),
        &up_item,
        &down_item,
        &auto_item,
        &fix_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ])?;
    let mut menu = Some(menu);
    let mut tray: Option<TrayIcon> = None;
    let mut worker = Some(worker);

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        let send = |c: Cmd| {
            let _ = tx.send(c);
        };
        let set_auto = |cfg: &mut Config, on: bool| {
            cfg.auto = on;
            auto_item.set_checked(on);
            send(Cmd::SetAuto(on));
            save(cfg);
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
            }
            Event::UserEvent(UserEvent::Menu(e)) => {
                if e.id() == up_item.id() {
                    send(Cmd::Step { up: true });
                } else if e.id() == down_item.id() {
                    send(Cmd::Step { up: false });
                } else if e.id() == auto_item.id() {
                    // The check item already toggled itself; follow it.
                    set_auto(&mut cfg, auto_item.is_checked());
                } else if e.id() == fix_item.id() {
                    cfg.fix_black_screen = fix_item.is_checked();
                    fix_enabled.store(cfg.fix_black_screen, Ordering::Relaxed);
                    save(&cfg);
                } else if e.id() == quit_item.id() {
                    send(Cmd::Quit);
                    if let Some(w) = worker.take() {
                        let _ = w.join();
                    }
                    tray = None;
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(UserEvent::HotKey(e)) if e.state == HotKeyState::Pressed => {
                let id = Some(e.id);
                if id == ids.up {
                    send(Cmd::Step { up: true });
                } else if id == ids.down {
                    send(Cmd::Step { up: false });
                } else if id == ids.toggle {
                    let on = !cfg.auto;
                    set_auto(&mut cfg, on);
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
            }
            _ => {}
        }
    });
}

fn save(cfg: &Config) {
    if let Err(e) = cfg.save() {
        log::warn!("saving config: {e:#}");
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
    let mode = match (s.auto, s.paused, s.light_sensor) {
        (false, _, _) => "manual".to_string(),
        (true, _, false) => "auto: light sensor not readable".to_string(),
        (true, true, _) => "auto paused".to_string(),
        (true, false, true) => "auto".to_string(),
    };
    let lux = s.lux.map(|l| format!(", {l:.0} lx")).unwrap_or_default();
    format!("LG UltraFine: {pct} ({mode}{lux})")
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
