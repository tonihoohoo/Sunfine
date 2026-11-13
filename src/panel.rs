//! Tray flyout (left click on the icon): the main settings surface. Unlike
//! the native tray menu (which Windows always closes on click) it stays open
//! while adjusting. Options that do not apply right now are drawn greyed and
//! ignore clicks.

use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tao::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use tao::event::{ElementState, MouseButton, WindowEvent};
use tao::event_loop::EventLoopWindowTarget;
use tao::keyboard::KeyCode;
use tao::window::{Window, WindowBuilder, WindowId};
use tiny_skia::{Color, FillRule, PathBuilder, Pixmap, Stroke, Transform};

use crate::ui::{
    new_surface, paint, present, round_corners, rounded_rect, sun, Action, Painter, Surface,
};

const W: f64 = 360.0;
const H: f64 = 486.0;
const ACCENT: [u8; 3] = [0x4c, 0xc2, 0xff];
const WHITE: [u8; 3] = [0xff, 0xff, 0xff];
const GREY: [u8; 3] = [0x9a, 0x9a, 0x9a];
const DISABLED: [u8; 3] = [0x5c, 0x5c, 0x5c];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModeSel {
    #[default]
    Manual,
    LightSensor,
    FollowLaptop,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PanelState {
    pub connected: bool,
    pub percent: f64,
    /// Short status, e.g. "auto · 18 lx (LG sensor)".
    pub status: String,
    pub mode: ModeSel,
    /// Laptop panel present and lid open.
    pub follow_available: bool,
    pub sensor_laptop: bool,
    /// The sensor choice matters right now (light-sensor mode in effect).
    pub sensor_enabled: bool,
    pub laptop_sensor_available: bool,
    pub curve_enabled: bool,
    pub shortcuts: [String; 3],
    pub osd: bool,
    pub fix_black: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PanelAction {
    SetPercent(f64),
    SetMode(ModeSel),
    SetSensorLaptop(bool),
    EditShortcut(Action),
    ToggleOsd,
    ToggleFix,
    EditCurve,
    OpenSettingsFile,
}

type Rect = (f32, f32, f32, f32);

fn inside((x, y, w, h): Rect, px: f32, py: f32) -> bool {
    px >= x && px <= x + w && py >= y && py <= y + h
}

/// Geometry in physical pixels for a window `w` wide at scale `s`.
struct Layout {
    s: f32,
    w: f32,
    track: (f32, f32, f32),
    mode: [Rect; 3],
    sensor: [Rect; 2],
    shortcuts: [Rect; 3],
    toggles: [Rect; 2],
    curve: Rect,
    file: Rect,
}

fn segments(x: f32, y: f32, w: f32, h: f32, n: usize) -> Vec<Rect> {
    let sw = w / n as f32;
    (0..n).map(|i| (x + i as f32 * sw, y, sw, h)).collect()
}

fn layout(w: f32, s: f32) -> Layout {
    let m = 16.0 * s;
    let inner = w - 2.0 * m;
    let mode = segments(m, 130.0 * s, inner, 32.0 * s, 3);
    let sensor = segments(m, 198.0 * s, inner, 32.0 * s, 2);
    let row = |y: f32| (m, y * s, inner, 26.0 * s);
    Layout {
        s,
        w,
        track: (52.0 * s, w - 64.0 * s, 72.0 * s),
        mode: [mode[0], mode[1], mode[2]],
        sensor: [sensor[0], sensor[1]],
        shortcuts: [row(282.0), row(308.0), row(334.0)],
        toggles: [row(388.0), row(414.0)],
        curve: (m, 452.0 * s, 150.0 * s, 26.0 * s),
        file: (w - m - 110.0 * s, 452.0 * s, 110.0 * s, 26.0 * s),
    }
}

/// Clickable areas that are enabled in this state.
fn hits(l: &Layout, st: &PanelState) -> Vec<(Rect, PanelAction)> {
    let mut v = vec![
        (l.mode[0], PanelAction::SetMode(ModeSel::Manual)),
        (l.mode[1], PanelAction::SetMode(ModeSel::LightSensor)),
    ];
    if st.follow_available || st.mode == ModeSel::FollowLaptop {
        v.push((l.mode[2], PanelAction::SetMode(ModeSel::FollowLaptop)));
    }
    if st.sensor_enabled {
        v.push((l.sensor[0], PanelAction::SetSensorLaptop(false)));
        if st.laptop_sensor_available {
            v.push((l.sensor[1], PanelAction::SetSensorLaptop(true)));
        }
    }
    for (r, a) in l
        .shortcuts
        .iter()
        .zip([Action::Up, Action::Down, Action::ToggleAuto])
    {
        v.push((*r, PanelAction::EditShortcut(a)));
    }
    v.push((l.toggles[0], PanelAction::ToggleOsd));
    if cfg!(windows) {
        v.push((l.toggles[1], PanelAction::ToggleFix));
    }
    if st.curve_enabled {
        v.push((l.curve, PanelAction::EditCurve));
    }
    v.push((l.file, PanelAction::OpenSettingsFile));
    v
}

pub struct Panel {
    window: Rc<Window>,
    surface: Surface,
    painter: Rc<Painter>,
    pub state: PanelState,
    cursor: (f32, f32),
    dragging: bool,
    visible: bool,
    hidden_at: Option<Instant>,
}

impl Panel {
    pub fn new<T>(target: &EventLoopWindowTarget<T>, painter: Rc<Painter>) -> Result<Self> {
        #[allow(unused_mut)]
        let mut b = WindowBuilder::new()
            .with_title("LG UltraFine")
            .with_decorations(false)
            .with_resizable(false)
            .with_always_on_top(true)
            .with_visible(false)
            .with_inner_size(LogicalSize::new(W, H));
        #[cfg(windows)]
        {
            use tao::platform::windows::WindowBuilderExtWindows;
            b = b.with_skip_taskbar(true);
        }
        let window = Rc::new(b.build(target)?);
        round_corners(&window);
        let surface = new_surface(&window)?;
        Ok(Self {
            window,
            surface,
            painter,
            state: PanelState::default(),
            cursor: (0.0, 0.0),
            dragging: false,
            visible: false,
            hidden_at: None,
        })
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    /// Opens above the tray icon (`anchor` = icon rect, physical), or closes
    /// if open. A click on the icon first takes focus away from an open panel
    /// (which hides it); the short grace period stops it reopening at once.
    pub fn toggle<T>(
        &mut self,
        target: &EventLoopWindowTarget<T>,
        anchor: Option<(PhysicalPosition<f64>, PhysicalSize<u32>)>,
    ) {
        if self.visible {
            self.hide();
            return;
        }
        if self
            .hidden_at
            .is_some_and(|t| t.elapsed() < Duration::from_millis(300))
        {
            return;
        }
        let monitor_at = |x: i32, y: i32| {
            target.available_monitors().find(|m| {
                let (p, s) = (m.position(), m.size());
                x >= p.x && x < p.x + s.width as i32 && y >= p.y && y < p.y + s.height as i32
            })
        };
        let (ax, ay, ah) = match anchor {
            Some((pos, size)) => (
                pos.x as i32 + size.width as i32 / 2,
                pos.y as i32,
                size.height as i32,
            ),
            None => {
                // Opened from the menu: bottom right of the primary monitor.
                let m = target.primary_monitor();
                let (p, s) = m.map(|m| (m.position(), m.size())).unwrap_or_default();
                (p.x + s.width as i32 - 220, p.y + s.height as i32 - 60, 0)
            }
        };
        let monitor = monitor_at(ax, ay).or_else(|| target.primary_monitor());
        let scale = monitor.as_ref().map(|m| m.scale_factor()).unwrap_or(1.0);
        let (w, h) = ((W * scale) as i32, (H * scale) as i32);
        let gap = (12.0 * scale) as i32;
        let mut x = ax - w / 2;
        let mut y = ay - h - gap;
        if let Some(m) = &monitor {
            let (p, s) = (m.position(), m.size());
            x = x.clamp(p.x + gap, p.x + s.width as i32 - w - gap);
            if y < p.y {
                // Taskbar at the top.
                y = ay + ah + gap;
            }
        }
        self.window.set_outer_position(PhysicalPosition::new(x, y));
        self.window.set_visible(true);
        self.window.set_focus();
        self.visible = true;
        self.draw();
    }

    pub fn hide(&mut self) {
        if self.visible {
            self.window.set_visible(false);
            self.visible = false;
            self.dragging = false;
            self.hidden_at = Some(Instant::now());
        }
    }

    pub fn set_state(&mut self, st: PanelState) {
        // Don't fight the user's finger while dragging.
        let percent = if self.dragging {
            self.state.percent
        } else {
            st.percent
        };
        let st = PanelState { percent, ..st };
        if st != self.state {
            self.state = st;
            if self.visible {
                self.draw();
            }
        }
    }

    fn layout(&self) -> Layout {
        layout(
            self.window.inner_size().width as f32,
            self.window.scale_factor() as f32,
        )
    }

    fn slider_percent(&self) -> f64 {
        let (x0, x1, _) = self.layout().track;
        (((self.cursor.0 - x0) / (x1 - x0)).clamp(0.0, 1.0) * 100.0) as f64
    }

    pub fn handle(&mut self, event: &WindowEvent) -> Option<PanelAction> {
        match event {
            WindowEvent::Focused(false) => {
                self.hide();
                None
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.physical_key == KeyCode::Escape
                    && event.state == ElementState::Pressed =>
            {
                self.hide();
                None
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x as f32, position.y as f32);
                if self.dragging {
                    let p = self.slider_percent();
                    self.state.percent = p;
                    self.draw();
                    return Some(PanelAction::SetPercent(p));
                }
                None
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                if *state == ElementState::Released {
                    self.dragging = false;
                    return None;
                }
                let l = self.layout();
                let (cx, cy) = self.cursor;
                let (x0, x1, ty) = l.track;
                if (cy - ty).abs() < 16.0 * l.s && cx > x0 - 12.0 * l.s && cx < x1 + 12.0 * l.s {
                    if !self.state.connected {
                        return None;
                    }
                    self.dragging = true;
                    let p = self.slider_percent();
                    self.state.percent = p;
                    self.draw();
                    return Some(PanelAction::SetPercent(p));
                }
                let action = hits(&l, &self.state)
                    .into_iter()
                    .find(|(r, _)| inside(*r, cx, cy))
                    .map(|(_, a)| a);
                if matches!(
                    action,
                    Some(
                        PanelAction::EditCurve
                            | PanelAction::EditShortcut(_)
                            | PanelAction::OpenSettingsFile
                    )
                ) {
                    // These open another window.
                    self.hide();
                }
                action
            }
            _ => None,
        }
    }

    pub fn draw(&mut self) {
        let size = self.window.inner_size();
        let s = self.window.scale_factor() as f32;
        let Some(pm) = render_panel(&self.painter, size.width, size.height, s, &self.state) else {
            return;
        };
        if let Err(e) = present(&mut self.surface, &pm) {
            log::debug!("panel draw: {e:#}");
        }
    }
}

fn fill(pm: &mut Pixmap, r: Rect, radius: f32, rgb: [u8; 3]) {
    if let Some(p) = rounded_rect(r.0, r.1, r.2, r.3, radius) {
        pm.fill_path(
            &p,
            &paint(rgb),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

fn outline(pm: &mut Pixmap, r: Rect, radius: f32, rgb: [u8; 3]) {
    if let Some(p) = rounded_rect(r.0 + 0.5, r.1 + 0.5, r.2 - 1.0, r.3 - 1.0, radius) {
        pm.stroke_path(
            &p,
            &paint(rgb),
            &Stroke {
                width: 1.0,
                ..Default::default()
            },
            Transform::identity(),
            None,
        );
    }
}

fn centered(p: &Painter, pm: &mut Pixmap, r: Rect, text: &str, px: f32, rgb: [u8; 3]) {
    let tw = p.text_width(text, px);
    p.text(
        pm,
        text,
        r.0 + (r.2 - tw) / 2.0,
        r.1 + r.3 / 2.0 + px * 0.36,
        px,
        rgb,
    );
}

/// Segmented control: `selected` index, `enabled` per segment, `active`
/// = whether the whole control applies right now.
#[allow(clippy::too_many_arguments)]
fn segmented(
    p: &Painter,
    pm: &mut Pixmap,
    s: f32,
    rects: &[Rect],
    labels: &[&str],
    selected: usize,
    enabled: &[bool],
    active: bool,
) {
    let (x0, y0) = (rects[0].0, rects[0].1);
    let last = rects[rects.len() - 1];
    let whole = (x0, y0, last.0 + last.2 - x0, last.3);
    fill(pm, whole, 6.0 * s, [0x2d, 0x2d, 0x2d]);
    outline(pm, whole, 6.0 * s, [0x45, 0x45, 0x45]);
    for (i, (r, label)) in rects.iter().zip(labels).enumerate() {
        let inset = (r.0 + 3.0 * s, r.1 + 3.0 * s, r.2 - 6.0 * s, r.3 - 6.0 * s);
        let fg = if i == selected {
            fill(
                pm,
                inset,
                4.0 * s,
                if active { ACCENT } else { [0x4a, 0x4a, 0x4a] },
            );
            if active {
                [0, 0, 0]
            } else {
                [0xb0, 0xb0, 0xb0]
            }
        } else if enabled[i] && active {
            WHITE
        } else {
            DISABLED
        };
        centered(p, pm, *r, label, 12.0 * s, fg);
    }
}

pub fn render_panel(
    p: &Painter,
    width: u32,
    height: u32,
    s: f32,
    st: &PanelState,
) -> Option<Pixmap> {
    let mut pm = Pixmap::new(width.max(1), height.max(1))?;
    let (w, h) = (width as f32, height as f32);
    pm.fill(Color::from_rgba8(0x20, 0x20, 0x20, 255));
    outline(&mut pm, (0.0, 0.0, w, h), 8.0 * s, [0x40, 0x40, 0x40]);
    let l = layout(w, s);
    let m = 16.0 * s;

    // Header.
    p.text(&mut pm, "LG UltraFine", m, 32.0 * s, 15.0 * s, WHITE);
    let status = if st.connected {
        st.status.as_str()
    } else {
        "not connected"
    };
    let stw = p.text_width(status, 11.5 * s);
    p.text(&mut pm, status, w - m - stw, 32.0 * s, 11.5 * s, GREY);

    // Slider.
    let (x0, x1, ty) = l.track;
    sun(&mut pm, 30.0 * s, ty, s * 0.9, WHITE);
    let bh = 4.0 * s;
    fill(
        &mut pm,
        (x0, ty - bh / 2.0, x1 - x0, bh),
        bh / 2.0,
        [0x5d, 0x5d, 0x5d],
    );
    let accent = if st.connected {
        ACCENT
    } else {
        [0x70, 0x70, 0x70]
    };
    let fx = x0 + (x1 - x0) * (st.percent / 100.0).clamp(0.0, 1.0) as f32;
    fill(
        &mut pm,
        (x0, ty - bh / 2.0, (fx - x0).max(bh), bh),
        bh / 2.0,
        accent,
    );
    for (r, c) in [(9.0, [0x45, 0x45, 0x45]), (5.5, accent)] {
        if let Some(k) = PathBuilder::from_circle(fx, ty, r * s) {
            pm.fill_path(
                &k,
                &paint(c),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }
    let pct = if st.connected {
        format!("{:.0}", st.percent)
    } else {
        "–".into()
    };
    let pw = p.text_width(&pct, 15.0 * s);
    p.text(
        &mut pm,
        &pct,
        w - 18.0 * s - pw,
        ty + 5.5 * s,
        15.0 * s,
        WHITE,
    );

    // Automatic brightness mode.
    let mode_note = if st.follow_available {
        ""
    } else {
        "  – Follow laptop needs the lid open"
    };
    p.text(
        &mut pm,
        &format!("Brightness{mode_note}"),
        m,
        120.0 * s,
        11.5 * s,
        GREY,
    );
    let follow_label = "Follow laptop";
    let mode_idx = match st.mode {
        ModeSel::Manual => 0,
        ModeSel::LightSensor => 1,
        ModeSel::FollowLaptop => 2,
    };
    segmented(
        p,
        &mut pm,
        s,
        &l.mode,
        &["Manual", "Light sensor", follow_label],
        mode_idx,
        &[true, true, st.follow_available],
        true,
    );

    // Sensor choice.
    let note = if !st.sensor_enabled {
        match st.mode {
            ModeSel::Manual => "  – not used in manual mode",
            ModeSel::FollowLaptop => "  – not used while following the laptop",
            ModeSel::LightSensor => "",
        }
    } else if !st.laptop_sensor_available {
        "  – laptop sensor needs the lid open"
    } else {
        ""
    };
    p.text(
        &mut pm,
        &format!("Light sensor{note}"),
        m,
        188.0 * s,
        11.5 * s,
        GREY,
    );
    segmented(
        p,
        &mut pm,
        s,
        &l.sensor,
        &["LG display", "Laptop"],
        st.sensor_laptop as usize,
        &[true, st.laptop_sensor_available],
        st.sensor_enabled,
    );

    // Shortcuts.
    p.text(
        &mut pm,
        "Shortcuts (click to change)",
        m,
        274.0 * s,
        11.5 * s,
        GREY,
    );
    for ((x, y, rw, rh), (label, spec)) in l.shortcuts.iter().zip(
        ["Brighter", "Darker", "Auto on/off"]
            .iter()
            .zip(&st.shortcuts),
    ) {
        p.text(&mut pm, label, *x, y + rh / 2.0 + 4.5 * s, 12.5 * s, WHITE);
        let sw = p.text_width(spec, 12.5 * s);
        p.text(
            &mut pm,
            spec,
            x + rw - sw,
            y + rh / 2.0 + 4.5 * s,
            12.5 * s,
            ACCENT,
        );
    }

    // Toggles.
    p.text(&mut pm, "Options", m, 380.0 * s, 11.5 * s, GREY);
    let fix_label = if cfg!(windows) {
        "Fix black screen on connect"
    } else {
        "Fix black screen (Windows only)"
    };
    for ((x, y, _, rh), (label, on, enabled)) in l.toggles.iter().zip([
        ("Popup on brightness keys", st.osd, true),
        (fix_label, st.fix_black, cfg!(windows)),
    ]) {
        let b = (*x, y + (rh - 16.0 * s) / 2.0, 16.0 * s, 16.0 * s);
        if on {
            fill(&mut pm, b, 3.0 * s, if enabled { ACCENT } else { DISABLED });
            let mut pb = PathBuilder::new();
            pb.move_to(b.0 + 4.0 * s, b.1 + 8.5 * s);
            pb.line_to(b.0 + 7.0 * s, b.1 + 11.5 * s);
            pb.line_to(b.0 + 12.5 * s, b.1 + 5.0 * s);
            if let Some(path) = pb.finish() {
                pm.stroke_path(
                    &path,
                    &paint([0, 0, 0]),
                    &Stroke {
                        width: 2.0 * s,
                        ..Default::default()
                    },
                    Transform::identity(),
                    None,
                );
            }
        } else {
            outline(
                &mut pm,
                b,
                3.0 * s,
                if enabled {
                    [0x80, 0x80, 0x80]
                } else {
                    DISABLED
                },
            );
        }
        p.text(
            &mut pm,
            label,
            x + 26.0 * s,
            y + rh / 2.0 + 4.5 * s,
            12.5 * s,
            if enabled { WHITE } else { DISABLED },
        );
    }

    // Footer links.
    let (cx, cy, _, ch) = l.curve;
    p.text(
        &mut pm,
        "Brightness curve…",
        cx,
        cy + ch / 2.0 + 4.5 * s,
        13.0 * s,
        if st.curve_enabled { ACCENT } else { DISABLED },
    );
    let (fx2, fy, fw, fh) = l.file;
    let ft = "Settings file…";
    p.text(
        &mut pm,
        ft,
        fx2 + fw - p.text_width(ft, 12.0 * s),
        fy + fh / 2.0 + 4.5 * s,
        12.0 * s,
        GREY,
    );
    let _ = l.w;
    Some(pm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> PanelState {
        PanelState {
            connected: true,
            percent: 27.0,
            status: "auto · 18 lx (LG sensor)".into(),
            mode: ModeSel::LightSensor,
            follow_available: false,
            sensor_laptop: false,
            sensor_enabled: true,
            laptop_sensor_available: false,
            curve_enabled: true,
            shortcuts: [
                "ctrl+alt+PageUp".into(),
                "ctrl+alt+PageDown".into(),
                "ctrl+alt+Home".into(),
            ],
            osd: true,
            fix_black: true,
        }
    }

    #[test]
    fn disabled_options_are_not_clickable() {
        let l = layout(W as f32, 1.0);
        let acts: Vec<_> = hits(&l, &state()).into_iter().map(|(_, a)| a).collect();
        assert!(
            !acts.contains(&PanelAction::SetMode(ModeSel::FollowLaptop)),
            "lid closed"
        );
        assert!(
            !acts.contains(&PanelAction::SetSensorLaptop(true)),
            "no laptop sensor"
        );
        assert!(acts.contains(&PanelAction::EditCurve));
        let manual = PanelState {
            mode: ModeSel::Manual,
            sensor_enabled: false,
            curve_enabled: false,
            ..state()
        };
        let acts: Vec<_> = hits(&l, &manual).into_iter().map(|(_, a)| a).collect();
        assert!(!acts.contains(&PanelAction::SetSensorLaptop(false)));
        assert!(!acts.contains(&PanelAction::EditCurve));
    }

    /// Renders the flyout at 2x: target/panel_preview.png
    #[test]
    fn panel_preview() {
        let pm = render_panel(
            &Painter::new(),
            (W * 2.0) as u32,
            (H * 2.0) as u32,
            2.0,
            &state(),
        )
        .unwrap();
        pm.save_png(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/panel_preview.png"),
        )
        .unwrap();
    }
}
