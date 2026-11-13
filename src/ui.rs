//! Small software-rendered windows (softbuffer + tiny-skia + fontdue with a
//! system font): the brightness popup and the shortcut capture window.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use global_hotkey::hotkey::HotKey;
use tao::dpi::{LogicalSize, PhysicalPosition};
use tao::event::{ElementState, WindowEvent};
use tao::event_loop::EventLoopWindowTarget;
use tao::keyboard::{KeyCode, ModifiersState};
use tao::monitor::MonitorHandle;
use tao::window::{Window, WindowBuilder, WindowId};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Transform};

use crate::theme::{Palette, Rgb};

pub(crate) type Surface = softbuffer::Surface<Rc<Window>, Rc<Window>>;

// ---------------------------------------------------------------- drawing

pub struct Painter {
    font: Option<fontdue::Font>,
}

impl Painter {
    pub fn new() -> Self {
        let candidates: &[&str] = if cfg!(windows) {
            &[
                "C:\\Windows\\Fonts\\segoeui.ttf",
                "C:\\Windows\\Fonts\\arial.ttf",
            ]
        } else {
            &[
                "/System/Library/Fonts/Supplemental/Arial.ttf",
                "/Library/Fonts/Arial.ttf",
                "/System/Library/Fonts/Helvetica.ttc",
            ]
        };
        let font = candidates.iter().find_map(|p| {
            let bytes = std::fs::read(p).ok()?;
            fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()).ok()
        });
        if font.is_none() {
            log::warn!("no system font found; popup text disabled");
        }
        Self { font }
    }

    pub fn text_width(&self, s: &str, px: f32) -> f32 {
        let Some(f) = &self.font else { return 0.0 };
        s.chars().map(|c| f.metrics(c, px).advance_width).sum()
    }

    /// Draws `s` with its baseline at `y`, blending onto the pixmap.
    pub fn text(&self, pm: &mut Pixmap, s: &str, x: f32, y: f32, px: f32, rgb: [u8; 3]) {
        let Some(f) = &self.font else { return };
        let (w, h) = (pm.width() as i32, pm.height() as i32);
        let data = pm.data_mut();
        let mut pen = x;
        for c in s.chars() {
            let (m, cov) = f.rasterize(c, px);
            let gx = (pen + m.xmin as f32).round() as i32;
            let gy = (y - m.ymin as f32 - m.height as f32).round() as i32;
            for row in 0..m.height as i32 {
                for col in 0..m.width as i32 {
                    let (px_, py_) = (gx + col, gy + row);
                    if px_ < 0 || py_ < 0 || px_ >= w || py_ >= h {
                        continue;
                    }
                    let a = cov[(row * m.width as i32 + col) as usize] as u32;
                    if a == 0 {
                        continue;
                    }
                    let i = ((py_ * w + px_) * 4) as usize;
                    for k in 0..3 {
                        let dst = data[i + k] as u32;
                        data[i + k] = ((rgb[k] as u32 * a + dst * (255 - a)) / 255) as u8;
                    }
                    data[i + 3] = 255;
                }
            }
            pen += m.advance_width;
        }
    }
}

pub(crate) fn paint(rgb: [u8; 3]) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(Color::from_rgba8(rgb[0], rgb[1], rgb[2], 255));
    p.anti_alias = true;
    p
}

pub(crate) fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    let r = r.min(w / 2.0).min(h / 2.0);
    let k = 0.552_284_8 * r;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

pub(crate) fn sun(pm: &mut Pixmap, cx: f32, cy: f32, s: f32, rgb: [u8; 3]) {
    let p = paint(rgb);
    if let Some(c) = PathBuilder::from_circle(cx, cy, 4.5 * s) {
        pm.fill_path(&c, &p, FillRule::Winding, Transform::identity(), None);
    }
    let stroke = tiny_skia::Stroke {
        width: 1.6 * s,
        line_cap: tiny_skia::LineCap::Round,
        ..Default::default()
    };
    for i in 0..8 {
        let a = i as f32 * std::f32::consts::FRAC_PI_4;
        let mut pb = PathBuilder::new();
        pb.move_to(cx + a.cos() * 7.0 * s, cy + a.sin() * 7.0 * s);
        pb.line_to(cx + a.cos() * 9.5 * s, cy + a.sin() * 9.5 * s);
        if let Some(path) = pb.finish() {
            pm.stroke_path(&path, &p, &stroke, Transform::identity(), None);
        }
    }
}

pub(crate) fn new_surface(window: &Rc<Window>) -> Result<Surface> {
    let ctx =
        softbuffer::Context::new(window.clone()).map_err(|e| anyhow!("softbuffer context: {e}"))?;
    softbuffer::Surface::new(&ctx, window.clone()).map_err(|e| anyhow!("softbuffer surface: {e}"))
}

pub(crate) fn present(surface: &mut Surface, pm: &Pixmap) -> Result<()> {
    let (w, h) = (pm.width(), pm.height());
    let (Some(nw), Some(nh)) = (NonZeroU32::new(w), NonZeroU32::new(h)) else {
        return Ok(());
    };
    surface.resize(nw, nh).map_err(|e| anyhow!("resize: {e}"))?;
    let mut buf = surface.buffer_mut().map_err(|e| anyhow!("buffer: {e}"))?;
    let (pixels, _) = pm.data().as_chunks::<4>();
    for (dst, px) in buf.iter_mut().zip(pixels) {
        *dst = (px[0] as u32) << 16 | (px[1] as u32) << 8 | px[2] as u32;
    }
    buf.present().map_err(|e| anyhow!("present: {e}"))
}

#[cfg(windows)]
pub(crate) fn round_corners(window: &Window) {
    use tao::platform::windows::WindowExtWindows;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    };
    let pref = DWMWCP_ROUND;
    // Windows 11 only; harmless failure elsewhere.
    let _ = unsafe {
        DwmSetWindowAttribute(
            HWND(window.hwnd() as _),
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const _ as *const _,
            std::mem::size_of_val(&pref) as u32,
        )
    };
}

#[cfg(not(windows))]
pub(crate) fn round_corners(_: &Window) {}

// ---------------------------------------------------------------- popup

const OSD_W: f64 = 300.0;
const OSD_H: f64 = 64.0;
const OSD_SHOW: Duration = Duration::from_millis(1600);

/// Brightness popup in the style of the Windows 11 flyout, labelled with
/// the display name and shown on the LG itself.
pub struct Osd {
    window: Rc<Window>,
    surface: Surface,
    painter: Rc<Painter>,
    hide_at: Option<Instant>,
    percent: f64,
    detail: String,
}

impl Osd {
    pub fn new<T>(target: &EventLoopWindowTarget<T>, painter: Rc<Painter>) -> Result<Self> {
        #[allow(unused_mut)]
        let mut b = WindowBuilder::new()
            .with_title("LG UltraFine brightness")
            .with_decorations(false)
            .with_resizable(false)
            .with_always_on_top(true)
            .with_visible(false)
            .with_focused(false)
            .with_focusable(false)
            .with_inner_size(LogicalSize::new(OSD_W, OSD_H));
        #[cfg(windows)]
        {
            use tao::platform::windows::WindowBuilderExtWindows;
            b = b.with_skip_taskbar(true);
        }
        let window = Rc::new(b.build(target)?);
        // Click-through only on macOS: on Windows tao implements it with
        // WS_EX_LAYERED, and a layered window ignores our normal (BitBlt)
        // drawing – the popup showed up as an empty grey box (v0.2).
        #[cfg(not(windows))]
        let _ = window.set_ignore_cursor_events(true);
        round_corners(&window);
        let surface = new_surface(&window)?;
        Ok(Self {
            window,
            surface,
            painter,
            hide_at: None,
            percent: 0.0,
            detail: String::new(),
        })
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    /// Shows the popup on `monitor` (bottom centre, like the native one).
    pub fn show(&mut self, monitor: Option<MonitorHandle>, percent: f64, detail: &str) {
        self.percent = percent;
        self.detail = detail.to_string();
        if let Some(m) = monitor {
            let scale = m.scale_factor();
            let (w, h) = ((OSD_W * scale) as i32, (OSD_H * scale) as i32);
            let pos = m.position();
            let size = m.size();
            let x = pos.x + (size.width as i32 - w) / 2;
            let y = pos.y + size.height as i32 - h - (96.0 * scale) as i32;
            self.window.set_outer_position(PhysicalPosition::new(x, y));
        }
        self.window.set_visible(true);
        self.draw();
        self.hide_at = Some(Instant::now() + OSD_SHOW);
    }

    /// Hides when due; returns the next wake-up time while visible.
    pub fn tick(&mut self, now: Instant) -> Option<Instant> {
        match self.hide_at {
            Some(t) if now >= t => {
                self.window.set_visible(false);
                self.hide_at = None;
                None
            }
            other => other,
        }
    }

    pub fn draw(&mut self) {
        let size = self.window.inner_size();
        let scale = self.window.scale_factor() as f32;
        let Some(pm) = render_osd(
            &self.painter,
            size.width,
            size.height,
            scale,
            self.percent,
            &self.detail,
            &crate::theme::for_shell(&self.window),
        ) else {
            return;
        };
        if let Err(e) = present(&mut self.surface, &pm) {
            log::debug!("osd draw: {e:#}");
        }
    }
}

/// Draws the popup: sun icon, "LG UltraFine", mode, bar and percentage.
pub fn render_osd(
    p: &Painter,
    width: u32,
    height: u32,
    s: f32,
    percent: f64,
    detail: &str,
    pal: &Palette,
) -> Option<Pixmap> {
    let mut pm = Pixmap::new(width.max(1), height.max(1))?;
    let (w, h) = (width as f32, height as f32);
    let [r, g, b] = pal.flyout;
    pm.fill(Color::from_rgba8(r, g, b, 255));
    if let Some(border) = rounded_rect(0.5, 0.5, w - 1.0, h - 1.0, 8.0 * s) {
        let stroke = tiny_skia::Stroke {
            width: 1.0,
            ..Default::default()
        };
        pm.stroke_path(
            &border,
            &paint(pal.border),
            &stroke,
            Transform::identity(),
            None,
        );
    }
    sun(&mut pm, 30.0 * s, h / 2.0, s, pal.text);

    p.text(
        &mut pm,
        "LG UltraFine",
        58.0 * s,
        25.0 * s,
        13.0 * s,
        pal.text,
    );
    if !detail.is_empty() {
        let dw = p.text_width(detail, 11.0 * s);
        p.text(
            &mut pm,
            detail,
            w - 16.0 * s - dw,
            25.0 * s,
            11.0 * s,
            pal.subtext,
        );
    }
    let pct = format!("{:.0}", percent);
    let pw = p.text_width(&pct, 15.0 * s);
    p.text(
        &mut pm,
        &pct,
        w - 16.0 * s - pw,
        47.0 * s,
        15.0 * s,
        pal.text,
    );

    let (bx, by, bh) = (58.0 * s, 40.0 * s, 4.0 * s);
    let bw = w - bx - 56.0 * s;
    let fill = |pm: &mut Pixmap, path: Option<tiny_skia::Path>, c: Rgb| {
        if let Some(path) = path {
            pm.fill_path(
                &path,
                &paint(c),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    };
    fill(&mut pm, rounded_rect(bx, by, bw, bh, bh / 2.0), pal.track);
    let fill_w = (bw * (percent / 100.0).clamp(0.0, 1.0) as f32).max(bh);
    fill(
        &mut pm,
        rounded_rect(bx, by, fill_w, bh, bh / 2.0),
        pal.accent,
    );
    // Fluent slider thumb: ring in the surface colour around an accent dot.
    fill(
        &mut pm,
        PathBuilder::from_circle(bx + fill_w, by + bh / 2.0, 9.0 * s),
        pal.flyout,
    );
    fill(
        &mut pm,
        PathBuilder::from_circle(bx + fill_w, by + bh / 2.0, 9.0 * s - 1.0),
        pal.button_border,
    );
    fill(
        &mut pm,
        PathBuilder::from_circle(bx + fill_w, by + bh / 2.0, 8.0 * s),
        pal.flyout,
    );
    fill(
        &mut pm,
        PathBuilder::from_circle(bx + fill_w, by + bh / 2.0, 5.0 * s),
        pal.accent,
    );
    Some(pm)
}

/// Monitor the LG is on: Windows by GDI device name, macOS by name.
pub fn lg_monitor<T>(target: &EventLoopWindowTarget<T>) -> Option<MonitorHandle> {
    let gdi = crate::display_fix::lg_gdi_names();
    target
        .available_monitors()
        .find(|m| {
            let name = m.name().unwrap_or_default();
            gdi.iter().any(|g| g.eq_ignore_ascii_case(&name)) || name.contains("LG UltraFine")
        })
        .or_else(|| target.primary_monitor())
}

// ---------------------------------------------------------------- shortcut capture

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Up,
    Down,
    ToggleAuto,
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::Up => "Brighter",
            Action::Down => "Darker",
            Action::ToggleAuto => "Auto brightness on/off",
        }
    }
}

pub enum CaptureResult {
    Cancel,
    Chosen(String),
}

/// Window that records the next key combination.
pub struct Capture {
    window: Rc<Window>,
    surface: Surface,
    painter: Rc<Painter>,
    pub action: Action,
    current: String,
    mods: ModifiersState,
    message: String,
}

impl Capture {
    pub fn new<T>(
        target: &EventLoopWindowTarget<T>,
        painter: Rc<Painter>,
        action: Action,
        current: &str,
    ) -> Result<Self> {
        let window = Rc::new(
            WindowBuilder::new()
                .with_title("LG UltraFine – set shortcut")
                .with_resizable(false)
                .with_always_on_top(true)
                .with_inner_size(LogicalSize::new(460.0, 150.0))
                .build(target)?,
        );
        if let Some(m) = target.primary_monitor() {
            let size = window.outer_size();
            let (p, s) = (m.position(), m.size());
            window.set_outer_position(PhysicalPosition::new(
                p.x + (s.width as i32 - size.width as i32) / 2,
                p.y + (s.height as i32 - size.height as i32) / 3,
            ));
        }
        window.set_focus();
        let surface = new_surface(&window)?;
        let mut c = Self {
            window,
            surface,
            painter,
            action,
            current: current.to_string(),
            mods: ModifiersState::empty(),
            message: String::new(),
        };
        c.draw();
        Ok(c)
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    pub fn set_message(&mut self, msg: &str) {
        self.message = msg.to_string();
        self.draw();
    }

    pub fn handle(&mut self, event: &WindowEvent) -> Option<CaptureResult> {
        match event {
            WindowEvent::CloseRequested => Some(CaptureResult::Cancel),
            WindowEvent::ModifiersChanged(m) => {
                self.mods = *m;
                None
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed && !event.repeat =>
            {
                let key = event.physical_key;
                if key == KeyCode::Escape && self.mods.is_empty() {
                    return Some(CaptureResult::Cancel);
                }
                if is_modifier(key) {
                    return None;
                }
                match shortcut_spec(self.mods, key) {
                    Ok(spec) => Some(CaptureResult::Chosen(spec)),
                    Err(msg) => {
                        self.set_message(&msg);
                        None
                    }
                }
            }
            _ => None,
        }
    }

    pub fn draw(&mut self) {
        let size = self.window.inner_size();
        let s = self.window.scale_factor() as f32;
        let Some(mut pm) = Pixmap::new(size.width.max(1), size.height.max(1)) else {
            return;
        };
        let pal = crate::theme::for_app(&self.window);
        let [r, g, b] = pal.window;
        pm.fill(Color::from_rgba8(r, g, b, 255));
        let p = &self.painter;
        let dark = pal.text;
        let grey = pal.subtext;
        p.text(
            &mut pm,
            &format!("Press the new shortcut for: {}", self.action.label()),
            20.0 * s,
            36.0 * s,
            16.0 * s,
            dark,
        );
        p.text(
            &mut pm,
            &format!("Current: {}", self.current),
            20.0 * s,
            66.0 * s,
            13.0 * s,
            grey,
        );
        p.text(
            &mut pm,
            "Use Ctrl / Alt / Shift / Win + a key.  Esc = cancel.",
            20.0 * s,
            90.0 * s,
            13.0 * s,
            grey,
        );
        p.text(
            &mut pm,
            "Fn combinations are handled by the laptop itself and cannot be used.",
            20.0 * s,
            110.0 * s,
            12.0 * s,
            grey,
        );
        if !self.message.is_empty() {
            p.text(
                &mut pm,
                &self.message,
                20.0 * s,
                136.0 * s,
                13.0 * s,
                pal.error,
            );
        }
        if let Err(e) = present(&mut self.surface, &pm) {
            log::debug!("capture draw: {e:#}");
        }
    }
}

fn is_modifier(k: KeyCode) -> bool {
    matches!(
        k,
        KeyCode::ControlLeft
            | KeyCode::ControlRight
            | KeyCode::AltLeft
            | KeyCode::AltRight
            | KeyCode::ShiftLeft
            | KeyCode::ShiftRight
            | KeyCode::SuperLeft
            | KeyCode::SuperRight
    )
}

/// Builds a global-hotkey spec ("ctrl+alt+PageUp") and validates it.
pub fn shortcut_spec(mods: ModifiersState, key: KeyCode) -> Result<String, String> {
    let mut parts = Vec::new();
    if mods.control_key() {
        parts.push("ctrl".to_string());
    }
    if mods.alt_key() {
        parts.push("alt".to_string());
    }
    if mods.shift_key() {
        parts.push("shift".to_string());
    }
    if mods.super_key() {
        parts.push("super".to_string());
    }
    let key_name = format!("{key:?}");
    let high_f = matches!(
        key_name.as_str(),
        "F13"
            | "F14"
            | "F15"
            | "F16"
            | "F17"
            | "F18"
            | "F19"
            | "F20"
            | "F21"
            | "F22"
            | "F23"
            | "F24"
    );
    if parts.is_empty() && !high_f {
        return Err(
            "Add Ctrl, Alt, Shift or Win – a plain key would be taken from every app.".into(),
        );
    }
    parts.push(key_name);
    let spec = parts.join("+");
    spec.parse::<HotKey>()
        .map_err(|_| format!("{spec} cannot be used as a global shortcut."))?;
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs() {
        let ca = ModifiersState::CONTROL | ModifiersState::ALT;
        assert_eq!(
            shortcut_spec(ca, KeyCode::PageUp).unwrap(),
            "ctrl+alt+PageUp"
        );
        assert_eq!(
            shortcut_spec(ModifiersState::SHIFT | ModifiersState::SUPER, KeyCode::F5).unwrap(),
            "shift+super+F5"
        );
        assert_eq!(
            shortcut_spec(ModifiersState::CONTROL, KeyCode::KeyB).unwrap(),
            "ctrl+KeyB"
        );
        assert!(shortcut_spec(ModifiersState::empty(), KeyCode::F5).is_err());
        assert_eq!(
            shortcut_spec(ModifiersState::empty(), KeyCode::F13).unwrap(),
            "F13"
        );
    }

    /// Renders the popup at 2x so it can be eyeballed:
    /// target/osd_preview.png
    #[test]
    fn osd_preview() {
        for (dark, name) in [(true, "dark"), (false, "light")] {
            let pal = crate::theme::palette(dark, [0x00, 0x78, 0xd4]);
            let pm = render_osd(
                &Painter::new(),
                (OSD_W * 2.0) as u32,
                (OSD_H * 2.0) as u32,
                2.0,
                42.0,
                "follows laptop",
                &pal,
            )
            .unwrap();
            let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("target/osd_preview_{name}.png"));
            pm.save_png(&out).unwrap();
        }
    }
}
