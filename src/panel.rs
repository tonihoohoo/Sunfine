//! Tray flyout (left click on the icon): brightness slider and quick toggles,
//! styled like the Windows 11 quick settings flyout.

use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tao::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use tao::event::{ElementState, MouseButton, WindowEvent};
use tao::event_loop::EventLoopWindowTarget;
use tao::keyboard::KeyCode;
use tao::window::{Window, WindowBuilder, WindowId};
use tiny_skia::{Color, FillRule, PathBuilder, Pixmap, Transform};

use crate::ui::{new_surface, paint, present, round_corners, rounded_rect, sun, Painter, Surface};

const W: f64 = 360.0;
const H: f64 = 190.0;
const ACCENT: [u8; 3] = [0x4c, 0xc2, 0xff];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PanelState {
    pub connected: bool,
    pub percent: f64,
    pub auto: bool,
    pub sync: bool,
    pub laptop_sensor: bool,
    /// Short status, e.g. "auto · 18 lx".
    pub status: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PanelAction {
    SetPercent(f64),
    ToggleAuto,
    ToggleSync,
    ToggleSource,
    EditCurve,
}

/// Hit areas in physical pixels.
struct Layout {
    track_x0: f32,
    track_x1: f32,
    track_y: f32,
    chips: [(f32, f32, f32, f32); 3],
    edit: (f32, f32, f32, f32),
}

fn layout(w: f32, s: f32) -> Layout {
    let chip_w = (w - 2.0 * 16.0 * s - 2.0 * 8.0 * s) / 3.0;
    let chip = |i: usize| {
        (
            16.0 * s + i as f32 * (chip_w + 8.0 * s),
            104.0 * s,
            chip_w,
            34.0 * s,
        )
    };
    Layout {
        track_x0: 52.0 * s,
        track_x1: w - 64.0 * s,
        track_y: 72.0 * s,
        chips: [chip(0), chip(1), chip(2)],
        edit: (16.0 * s, 150.0 * s, 160.0 * s, 28.0 * s),
    }
}

fn inside((x, y, w, h): (f32, f32, f32, f32), px: f32, py: f32) -> bool {
    px >= x && px <= x + w && py >= y && py <= y + h
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
        anchor: (PhysicalPosition<f64>, PhysicalSize<u32>),
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
        let (pos, size) = anchor;
        let (ax, ay) = (pos.x as i32 + size.width as i32 / 2, pos.y as i32);
        let monitor = target
            .available_monitors()
            .find(|m| {
                let (p, s) = (m.position(), m.size());
                ax >= p.x && ax < p.x + s.width as i32 && ay >= p.y && ay < p.y + s.height as i32
            })
            .or_else(|| target.primary_monitor());
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
                y = pos.y as i32 + size.height as i32 + gap;
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

    fn slider_percent(&self) -> f64 {
        let s = self.window.scale_factor() as f32;
        let l = layout(self.window.inner_size().width as f32, s);
        (((self.cursor.0 - l.track_x0) / (l.track_x1 - l.track_x0)).clamp(0.0, 1.0) * 100.0) as f64
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
                let s = self.window.scale_factor() as f32;
                let l = layout(self.window.inner_size().width as f32, s);
                let (cx, cy) = self.cursor;
                if (cy - l.track_y).abs() < 16.0 * s
                    && cx > l.track_x0 - 12.0 * s
                    && cx < l.track_x1 + 12.0 * s
                {
                    if !self.state.connected {
                        return None;
                    }
                    self.dragging = true;
                    let p = self.slider_percent();
                    self.state.percent = p;
                    self.draw();
                    return Some(PanelAction::SetPercent(p));
                }
                let actions = [
                    PanelAction::ToggleAuto,
                    PanelAction::ToggleSync,
                    PanelAction::ToggleSource,
                ];
                for (rect, a) in l.chips.iter().zip(actions) {
                    if inside(*rect, cx, cy) {
                        return Some(a);
                    }
                }
                if inside(l.edit, cx, cy) {
                    self.hide();
                    return Some(PanelAction::EditCurve);
                }
                None
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
    if let Some(border) = rounded_rect(0.5, 0.5, w - 1.0, h - 1.0, 8.0 * s) {
        let stroke = tiny_skia::Stroke {
            width: 1.0,
            ..Default::default()
        };
        pm.stroke_path(
            &border,
            &paint([0x40, 0x40, 0x40]),
            &stroke,
            Transform::identity(),
            None,
        );
    }
    let l = layout(w, s);
    let white = [0xff, 0xff, 0xff];
    let grey = [0x9a, 0x9a, 0x9a];

    p.text(&mut pm, "LG UltraFine", 16.0 * s, 32.0 * s, 15.0 * s, white);
    let status = if st.connected {
        st.status.as_str()
    } else {
        "not connected"
    };
    let sw = p.text_width(status, 12.0 * s);
    p.text(&mut pm, status, w - 16.0 * s - sw, 32.0 * s, 12.0 * s, grey);

    // Slider.
    sun(&mut pm, 30.0 * s, l.track_y, s * 0.9, white);
    let bh = 4.0 * s;
    let tw = l.track_x1 - l.track_x0;
    if let Some(track) = rounded_rect(l.track_x0, l.track_y - bh / 2.0, tw, bh, bh / 2.0) {
        pm.fill_path(
            &track,
            &paint([0x5d, 0x5d, 0x5d]),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    let accent = if st.connected {
        ACCENT
    } else {
        [0x70, 0x70, 0x70]
    };
    let fx = l.track_x0 + tw * (st.percent / 100.0).clamp(0.0, 1.0) as f32;
    if let Some(fill) = rounded_rect(
        l.track_x0,
        l.track_y - bh / 2.0,
        (fx - l.track_x0).max(bh),
        bh,
        bh / 2.0,
    ) {
        pm.fill_path(
            &fill,
            &paint(accent),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    if let Some(knob) = PathBuilder::from_circle(fx, l.track_y, 9.0 * s) {
        pm.fill_path(
            &knob,
            &paint([0x45, 0x45, 0x45]),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    if let Some(knob) = PathBuilder::from_circle(fx, l.track_y, 5.5 * s) {
        pm.fill_path(
            &knob,
            &paint(accent),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
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
        l.track_y + 5.5 * s,
        15.0 * s,
        white,
    );

    // Toggle chips.
    let chips = [
        ("Auto", st.auto),
        ("Follow laptop", st.sync),
        ("Laptop sensor", st.laptop_sensor),
    ];
    for ((x, y, cw, ch), (label, on)) in l.chips.iter().zip(chips) {
        if let Some(r) = rounded_rect(*x, *y, *cw, *ch, 5.0 * s) {
            if on {
                pm.fill_path(
                    &r,
                    &paint(ACCENT),
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            } else {
                pm.fill_path(
                    &r,
                    &paint([0x2d, 0x2d, 0x2d]),
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
                let stroke = tiny_skia::Stroke {
                    width: 1.0,
                    ..Default::default()
                };
                pm.stroke_path(
                    &r,
                    &paint([0x50, 0x50, 0x50]),
                    &stroke,
                    Transform::identity(),
                    None,
                );
            }
        }
        let fg = if on { [0x00, 0x00, 0x00] } else { white };
        let lw = p.text_width(label, 12.0 * s);
        p.text(
            &mut pm,
            label,
            x + (cw - lw) / 2.0,
            y + ch / 2.0 + 4.5 * s,
            12.0 * s,
            fg,
        );
    }

    let (ex, ey, _, eh) = l.edit;
    p.text(
        &mut pm,
        "Brightness curve…",
        ex,
        ey + eh / 2.0 + 4.5 * s,
        13.0 * s,
        ACCENT,
    );
    p.text(
        &mut pm,
        "Right-click icon: more",
        w - 16.0 * s - p.text_width("Right-click icon: more", 11.0 * s),
        ey + eh / 2.0 + 4.0 * s,
        11.0 * s,
        grey,
    );
    Some(pm)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders the flyout at 2x: target/panel_preview.png
    #[test]
    fn panel_preview() {
        let st = PanelState {
            connected: true,
            percent: 27.0,
            auto: true,
            sync: true,
            laptop_sensor: false,
            status: "follows laptop 66 %".into(),
        };
        let pm = render_panel(
            &Painter::new(),
            (W * 2.0) as u32,
            (H * 2.0) as u32,
            2.0,
            &st,
        )
        .unwrap();
        pm.save_png(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/panel_preview.png"),
        )
        .unwrap();
    }
}
