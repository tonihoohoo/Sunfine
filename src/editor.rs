//! Brightness curve editor: brightness (%) for each light level (lux, log
//! axis). Drag points, double-click to add, right-click to remove. Changes
//! apply live; the current light level is shown on the curve.

use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tao::dpi::{LogicalSize, PhysicalPosition};
use tao::event::{ElementState, MouseButton, WindowEvent};
use tao::event_loop::EventLoopWindowTarget;
use tao::keyboard::KeyCode;
use tao::window::{Window, WindowBuilder, WindowId};
use tiny_skia::{Color, FillRule, PathBuilder, Pixmap, Stroke, StrokeDash, Transform};

use crate::config::{default_curve, CurvePoint};
use crate::curve::{default_follow_points, eval, sanitize_on, Axis};
use crate::theme::Palette;
use crate::ui::{new_surface, paint, present, rounded_rect, Painter, Surface};

const W: f64 = 640.0;
const H: f64 = 440.0;

/// Which curve is being edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurveKind {
    /// LG % for each light level (lux).
    Light,
    /// LG % for each laptop screen brightness (%).
    Follow,
}

impl CurveKind {
    pub fn axis(self) -> Axis {
        match self {
            CurveKind::Light => Axis::LogLux,
            CurveKind::Follow => Axis::Percent,
        }
    }
    fn default_points(self) -> Vec<CurvePoint> {
        match self {
            CurveKind::Light => default_curve(),
            CurveKind::Follow => default_follow_points(),
        }
    }
    fn title(self) -> &'static str {
        match self {
            CurveKind::Light => "LG brightness for each light level",
            CurveKind::Follow => "LG brightness for each laptop screen brightness",
        }
    }
    fn window_title(self) -> &'static str {
        match self {
            CurveKind::Light => "Sunfine – brightness curve (light sensor)",
            CurveKind::Follow => "Sunfine – brightness curve (follow laptop)",
        }
    }
    /// Grid lines with labels on the input axis.
    fn ticks(self) -> &'static [(f64, &'static str)] {
        match self {
            CurveKind::Light => &[
                (0.0, "0"),
                (1.0, "1"),
                (10.0, "10"),
                (100.0, "100"),
                (1000.0, "1 000"),
                (10000.0, "10 000 lx"),
            ],
            CurveKind::Follow => &[
                (0.0, "0"),
                (25.0, "25"),
                (50.0, "50"),
                (75.0, "75"),
                (100.0, "100 %"),
            ],
        }
    }
    /// Words under the axis.
    fn bands(self) -> &'static [(f64, &'static str)] {
        match self {
            CurveKind::Light => &[
                (3.0, "dark"),
                (60.0, "room"),
                (600.0, "bright room"),
                (5000.0, "daylight"),
            ],
            CurveKind::Follow => &[(50.0, "laptop screen brightness")],
        }
    }
    fn value_label(self, v: f64) -> String {
        match self {
            CurveKind::Light => format!("{v:.0} lx"),
            CurveKind::Follow => format!("laptop {v:.0} %"),
        }
    }
}

pub enum EditorAction {
    Changed(Vec<CurvePoint>),
    Close,
}

/// Plot geometry in physical pixels.
#[derive(Clone, Copy)]
struct Plot {
    axis: Axis,
    x0: f32,
    x1: f32,
    y0: f32,
    y1: f32,
}

impl Plot {
    fn new(w: f32, h: f32, s: f32, axis: Axis) -> Self {
        Self {
            axis,
            x0: 64.0 * s,
            x1: w - 28.0 * s,
            y0: 70.0 * s,
            y1: h - 96.0 * s,
        }
    }
    fn px(&self, v: f64) -> f32 {
        self.x0 + (self.x1 - self.x0) * (self.axis.to_x(v) / self.axis.max_x()) as f32
    }
    fn py(&self, pct: f64) -> f32 {
        self.y1 - (self.y1 - self.y0) * (pct / 100.0) as f32
    }
    /// Input value (lux or laptop %) at pixel column `x`.
    fn value_at(&self, x: f32) -> f64 {
        self.axis.value_of_x(
            (((x - self.x0) / (self.x1 - self.x0)) as f64).clamp(0.0, 1.0) * self.axis.max_x(),
        )
    }
    fn pct_at(&self, y: f32) -> f64 {
        (((self.y1 - y) / (self.y1 - self.y0)) as f64 * 100.0).clamp(0.0, 100.0)
    }
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x0 - 8.0 && x <= self.x1 + 8.0 && y >= self.y0 - 8.0 && y <= self.y1 + 8.0
    }
}

fn buttons(w: f32, h: f32, s: f32) -> [(f32, f32, f32, f32); 2] {
    let (bw, bh, y) = (96.0 * s, 32.0 * s, h - 52.0 * s);
    [(24.0 * s, y, bw, bh), (w - 24.0 * s - bw, y, bw, bh)]
}

fn inside((x, y, w, h): (f32, f32, f32, f32), px: f32, py: f32) -> bool {
    px >= x && px <= x + w && py >= y && py <= y + h
}

pub struct CurveEditor {
    window: Rc<Window>,
    surface: Surface,
    painter: Rc<Painter>,
    kind: CurveKind,
    points: Vec<CurvePoint>,
    lux_now: Option<f64>,
    cursor: (f32, f32),
    drag: Option<usize>,
    hover: Option<usize>,
    last_click: Option<(Instant, (f32, f32))>,
}

impl CurveEditor {
    pub fn new<T>(
        target: &EventLoopWindowTarget<T>,
        painter: Rc<Painter>,
        kind: CurveKind,
        points: Vec<CurvePoint>,
    ) -> Result<Self> {
        let window = Rc::new(
            WindowBuilder::new()
                .with_title(kind.window_title())
                .with_resizable(false)
                .with_inner_size(LogicalSize::new(W, H))
                .build(target)?,
        );
        if let Some(m) = target.primary_monitor() {
            let size = window.outer_size();
            let (p, s) = (m.position(), m.size());
            window.set_outer_position(PhysicalPosition::new(
                p.x + (s.width as i32 - size.width as i32) / 2,
                p.y + (s.height as i32 - size.height as i32) / 2,
            ));
        }
        window.set_focus();
        let surface = new_surface(&window)?;
        let mut e = Self {
            window,
            surface,
            painter,
            kind,
            points,
            lux_now: None,
            cursor: (0.0, 0.0),
            drag: None,
            hover: None,
            last_click: None,
        };
        e.draw();
        Ok(e)
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    pub fn kind(&self) -> CurveKind {
        self.kind
    }

    /// Live updates from the device thread (current lux, learned curve).
    pub fn update(&mut self, lux: Option<f64>, points: &[CurvePoint]) {
        let mut dirty = false;
        if lux != self.lux_now {
            self.lux_now = lux;
            dirty = true;
        }
        if self.drag.is_none() && points != self.points.as_slice() {
            self.points = points.to_vec();
            dirty = true;
        }
        if dirty {
            self.draw();
        }
    }

    fn geometry(&self) -> (Plot, f32, f32, f32) {
        let size = self.window.inner_size();
        let s = self.window.scale_factor() as f32;
        let (w, h) = (size.width as f32, size.height as f32);
        (Plot::new(w, h, s, self.kind.axis()), w, h, s)
    }

    fn hit_point(&self, plot: &Plot, s: f32) -> Option<usize> {
        let (cx, cy) = self.cursor;
        self.points
            .iter()
            .enumerate()
            .map(|(i, p)| (i, (plot.px(p.lux) - cx).hypot(plot.py(p.percent) - cy)))
            .filter(|(_, d)| *d < 12.0 * s)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    }

    pub fn handle(&mut self, event: &WindowEvent) -> Option<EditorAction> {
        let (plot, w, h, s) = self.geometry();
        match event {
            WindowEvent::CloseRequested => Some(EditorAction::Close),
            // A release outside the window never arrives; don't keep dragging.
            WindowEvent::CursorLeft { .. } | WindowEvent::Focused(false) => {
                self.drag = None;
                None
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.physical_key == KeyCode::Escape
                    && event.state == ElementState::Pressed =>
            {
                Some(EditorAction::Close)
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x as f32, position.y as f32);
                if let Some(i) = self.drag.filter(|&i| i < self.points.len()) {
                    let (cx, cy) = self.cursor;
                    move_point(
                        &mut self.points,
                        i,
                        plot.value_at(cx),
                        plot.pct_at(cy),
                        self.kind.axis(),
                    );
                    self.draw();
                    return Some(EditorAction::Changed(self.points.clone()));
                }
                let hover = self.hit_point(&plot, s);
                if hover != self.hover {
                    self.hover = hover;
                    self.draw();
                }
                None
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                self.drag = None;
                None
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                let (cx, cy) = self.cursor;
                let [reset, done] = buttons(w, h, s);
                if inside(reset, cx, cy) {
                    self.points = self.kind.default_points();
                    self.draw();
                    return Some(EditorAction::Changed(self.points.clone()));
                }
                if inside(done, cx, cy) {
                    return Some(EditorAction::Close);
                }
                if let Some(i) = self.hit_point(&plot, s) {
                    self.drag = Some(i);
                    return None;
                }
                let now = Instant::now();
                let double = self.last_click.is_some_and(|(t, (x, y))| {
                    now - t < Duration::from_millis(450) && (x - cx).hypot(y - cy) < 6.0 * s
                });
                self.last_click = Some((now, (cx, cy)));
                if double && plot.contains(cx, cy) {
                    let p = CurvePoint {
                        lux: plot.value_at(cx),
                        percent: plot.pct_at(cy).round(),
                    };
                    let i = self
                        .points
                        .iter()
                        .position(|q| q.lux > p.lux)
                        .unwrap_or(self.points.len());
                    self.points.insert(i, p);
                    let axis = self.kind.axis();
                    self.points = match self.kind {
                        CurveKind::Light => {
                            sanitize_on(std::mem::take(&mut self.points), axis, default_curve)
                        }
                        CurveKind::Follow => sanitize_on(
                            std::mem::take(&mut self.points),
                            axis,
                            default_follow_points,
                        ),
                    };
                    self.drag = self
                        .points
                        .iter()
                        .position(|q| (axis.to_x(q.lux) - axis.to_x(p.lux)).abs() < 1e-9);
                    self.draw();
                    return Some(EditorAction::Changed(self.points.clone()));
                }
                None
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Right,
                ..
            } => {
                // Removing while dragging would leave `drag` pointing at the
                // wrong (or a missing) point.
                if self.drag.is_some() {
                    return None;
                }
                if let Some(i) = self.hit_point(&plot, s) {
                    if self.points.len() > 2 {
                        self.points.remove(i);
                        self.hover = None;
                        self.draw();
                        return Some(EditorAction::Changed(self.points.clone()));
                    }
                }
                None
            }
            _ => None,
        }
    }

    pub fn draw(&mut self) {
        let size = self.window.inner_size();
        let s = self.window.scale_factor() as f32;
        let Some(pm) = render_editor(
            &self.painter,
            size.width,
            size.height,
            s,
            &self.points,
            self.lux_now,
            self.hover,
            &crate::theme::for_app(&self.window),
            self.kind,
        ) else {
            return;
        };
        if let Err(e) = present(&mut self.surface, &pm) {
            log::debug!("editor draw: {e:#}");
        }
    }
}

/// Moves point `i` to (`lux`, `percent`), kept between its neighbours on
/// the lux axis (so the order never changes) and inside the axes.
pub(crate) fn move_point(points: &mut [CurvePoint], i: usize, v: f64, percent: f64, axis: Axis) {
    if i >= points.len() {
        return;
    }
    let max = axis.max_x();
    let lo = if i > 0 {
        axis.to_x(points[i - 1].lux) + axis.gap()
    } else {
        0.0
    };
    let hi = points
        .get(i + 1)
        .map(|p| axis.to_x(p.lux) - axis.gap())
        .unwrap_or(max);
    let x = axis.to_x(v).clamp(lo.min(max), hi.max(lo).min(max));
    points[i] = CurvePoint {
        lux: axis.value_of_x(x),
        percent: percent.clamp(0.0, 100.0).round(),
    };
}

fn line(pm: &mut Pixmap, x0: f32, y0: f32, x1: f32, y1: f32, rgb: [u8; 3], stroke: &Stroke) {
    let mut pb = PathBuilder::new();
    pb.move_to(x0, y0);
    pb.line_to(x1, y1);
    if let Some(p) = pb.finish() {
        pm.stroke_path(&p, &paint(rgb), stroke, Transform::identity(), None);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn render_editor(
    p: &Painter,
    width: u32,
    height: u32,
    s: f32,
    points: &[CurvePoint],
    lux_now: Option<f64>,
    hover: Option<usize>,
    pal: &Palette,
    kind: CurveKind,
) -> Option<Pixmap> {
    let mut pm = Pixmap::new(width.max(1), height.max(1))?;
    let (w, h) = (width as f32, height as f32);
    let axis = kind.axis();
    let plot = Plot::new(w, h, s, axis);
    let white = pal.text;
    let grey = pal.subtext;
    let grid = pal.grid;
    let [r, g, b] = pal.window;
    pm.fill(Color::from_rgba8(r, g, b, 255));

    p.text(&mut pm, kind.title(), 24.0 * s, 32.0 * s, 16.0 * s, white);
    p.text(
        &mut pm,
        "Drag points · double-click to add · right-click to remove · brightness keys also teach the curve",
        24.0 * s,
        52.0 * s,
        11.5 * s,
        grey,
    );

    // Grid and axis labels.
    let thin = Stroke {
        width: 1.0,
        ..Default::default()
    };
    for pct in [0.0, 25.0, 50.0, 75.0, 100.0] {
        let y = plot.py(pct);
        line(&mut pm, plot.x0, y, plot.x1, y, grid, &thin);
        let label = format!("{pct:.0} %");
        let lw = p.text_width(&label, 11.0 * s);
        p.text(
            &mut pm,
            &label,
            plot.x0 - 10.0 * s - lw,
            y + 4.0 * s,
            11.0 * s,
            grey,
        );
    }
    let ticks = kind.ticks();
    for (i, &(v, label)) in ticks.iter().enumerate() {
        let x = plot.px(v);
        line(&mut pm, x, plot.y0, x, plot.y1, grid, &thin);
        let lw = p.text_width(label, 11.0 * s);
        let lx = if i + 1 == ticks.len() {
            x - lw
        } else {
            x - lw / 2.0
        };
        p.text(&mut pm, label, lx, plot.y1 + 18.0 * s, 11.0 * s, grey);
    }
    for &(v, label) in kind.bands() {
        let lw = p.text_width(label, 10.0 * s);
        p.text(
            &mut pm,
            label,
            plot.px(v) - lw / 2.0,
            plot.y1 + 34.0 * s,
            10.0 * s,
            pal.faint,
        );
    }

    // Curve.
    let n = ((plot.x1 - plot.x0) / 2.0) as usize;
    let mut pb = PathBuilder::new();
    for i in 0..=n {
        let x = plot.x0 + (plot.x1 - plot.x0) * i as f32 / n as f32;
        let y = plot.py(eval(points, axis, plot.value_at(x)));
        if i == 0 {
            pb.move_to(x, y);
        } else {
            pb.line_to(x, y);
        }
    }
    if let Some(path) = pb.finish() {
        let st = Stroke {
            width: 2.5 * s,
            line_join: tiny_skia::LineJoin::Round,
            ..Default::default()
        };
        pm.stroke_path(&path, &paint(pal.accent), &st, Transform::identity(), None);
    }

    // Current light level.
    if let Some(lux) = lux_now {
        let x = plot.px(lux.min(axis.max_value()));
        let pct = eval(points, axis, lux);
        let dash = Stroke {
            width: 1.0 * s,
            dash: StrokeDash::new(vec![4.0 * s, 4.0 * s], 0.0),
            ..Default::default()
        };
        line(&mut pm, x, plot.y0, x, plot.y1, pal.subtext, &dash);
        if let Some(c) = PathBuilder::from_circle(x, plot.py(pct), 4.0 * s) {
            pm.fill_path(
                &c,
                &paint(white),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
        let label = format!("now {} → LG {pct:.0} %", kind.value_label(lux));
        let lw = p.text_width(&label, 12.0 * s);
        let lx = (x + 8.0 * s).min(plot.x1 - lw);
        p.text(&mut pm, &label, lx, plot.y0 + 14.0 * s, 12.0 * s, white);
    }

    // Points.
    for (i, pt) in points.iter().enumerate() {
        let (x, y) = (plot.px(pt.lux), plot.py(pt.percent));
        let r = if hover == Some(i) { 8.0 } else { 6.0 } * s;
        if let Some(c) = PathBuilder::from_circle(x, y, r + 1.5 * s) {
            pm.fill_path(
                &c,
                &paint(pal.window),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
        if let Some(c) = PathBuilder::from_circle(x, y, r) {
            pm.fill_path(
                &c,
                &paint(pal.accent),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
        if hover == Some(i) {
            let label = format!("{} · LG {:.0} %", kind.value_label(pt.lux), pt.percent);
            let lw = p.text_width(&label, 11.0 * s);
            p.text(
                &mut pm,
                &label,
                (x - lw / 2.0).clamp(plot.x0, plot.x1 - lw),
                y - 14.0 * s,
                11.0 * s,
                white,
            );
        }
    }

    // Buttons.
    for ((bx, by, bw, bh), label, primary) in buttons(w, h, s)
        .into_iter()
        .zip(["Reset", "Done"])
        .zip([false, true])
        .map(|((r, l), p)| (r, l, p))
    {
        if let Some(r) = rounded_rect(bx, by, bw, bh, 5.0 * s) {
            let fill = if primary { pal.accent } else { pal.button };
            pm.fill_path(
                &r,
                &paint(fill),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
            if !primary {
                pm.stroke_path(
                    &r,
                    &paint(pal.button_border),
                    &thin,
                    Transform::identity(),
                    None,
                );
            }
        }
        let fg = if primary { pal.on_accent } else { white };
        let lw = p.text_width(label, 13.0 * s);
        p.text(
            &mut pm,
            label,
            bx + (bw - lw) / 2.0,
            by + bh / 2.0 + 4.5 * s,
            13.0 * s,
            fg,
        );
    }
    Some(pm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curve::LUX_MAX;

    #[test]
    fn plot_mapping_round_trips() {
        let p = Plot::new(1280.0, 880.0, 2.0, Axis::LogLux);
        for lux in [0.0, 1.0, 18.0, 300.0, 5000.0, 10000.0] {
            let back = p.value_at(p.px(lux));
            assert!((back - lux).abs() < 1e-6 * lux.max(1.0), "{lux} -> {back}");
        }
        for pct in [0.0, 27.0, 100.0] {
            assert!((p.pct_at(p.py(pct)) - pct).abs() < 1e-4);
        }
        assert_eq!(p.pct_at(-1000.0), 100.0);
        assert_eq!(p.pct_at(1e6), 0.0);
        assert_eq!(p.value_at(-1e6), 0.0);
        assert!((p.value_at(1e6) - LUX_MAX).abs() < 1e-6);
        let f = Plot::new(1280.0, 880.0, 2.0, Axis::Percent);
        for v in [0.0, 33.0, 100.0] {
            assert!((f.value_at(f.px(v)) - v).abs() < 1e-4);
        }
        assert_eq!(f.value_at(1e6), 100.0);
    }

    #[test]
    fn dragging_keeps_order_and_bounds() {
        for (axis, mut pts) in [
            (Axis::LogLux, default_curve()),
            (
                Axis::Percent,
                vec![
                    CurvePoint {
                        lux: 0.0,
                        percent: 1.0,
                    },
                    CurvePoint {
                        lux: 50.0,
                        percent: 20.0,
                    },
                    CurvePoint {
                        lux: 100.0,
                        percent: 40.0,
                    },
                ],
            ),
        ] {
            move_point(&mut pts, 1, 1e6, 150.0, axis);
            assert!(pts[1].lux < pts[2].lux, "{axis:?}");
            assert_eq!(pts[1].percent, 100.0);
            move_point(&mut pts, 1, 0.0, -5.0, axis);
            assert!(pts[1].lux > pts[0].lux, "{axis:?}");
            assert_eq!(pts[1].percent, 0.0);
            let last = pts.len() - 1;
            move_point(&mut pts, last, 1e9, 50.0, axis);
            assert!(pts[last].lux <= axis.max_value() + 1e-9);
            move_point(&mut pts, 99, 1.0, 1.0, axis); // out of range: ignored
            for w in pts.windows(2) {
                assert!(w[0].lux < w[1].lux, "{axis:?} {pts:?}");
            }
        }
    }

    #[test]
    fn dragging_between_crowded_neighbours_does_not_panic() {
        let mut pts = vec![
            CurvePoint {
                lux: 10.0,
                percent: 10.0,
            },
            CurvePoint {
                lux: 10.2,
                percent: 20.0,
            },
            CurvePoint {
                lux: 10.4,
                percent: 30.0,
            },
        ];
        move_point(&mut pts, 1, 5000.0, 50.0, Axis::LogLux);
        assert!(pts[1].lux.is_finite());
        move_point(&mut pts, 1, 5000.0, 50.0, Axis::Percent);
        assert!(pts[1].lux.is_finite());
    }

    /// Renders both editors in both themes: target/editor_preview_*.png
    #[test]
    fn editor_preview() {
        for (dark, theme) in [(true, "dark"), (false, "light")] {
            let pal = crate::theme::palette(dark, [0x00, 0x78, 0xd4]);
            for (kind, now, name) in [
                (CurveKind::Light, 18.0, "light"),
                (CurveKind::Follow, 66.0, "follow"),
            ] {
                let pm = render_editor(
                    &Painter::new(),
                    (W * 2.0) as u32,
                    (H * 2.0) as u32,
                    2.0,
                    &kind.default_points(),
                    Some(now),
                    Some(1),
                    &pal,
                    kind,
                )
                .unwrap();
                pm.save_png(
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join(format!("target/editor_preview_{name}_{theme}.png")),
                )
                .unwrap();
            }
        }
    }
}
