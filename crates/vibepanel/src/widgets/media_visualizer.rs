//! Audio-reactive visualizers for the media widget.
//!
//! Two visualizers share a common [`AnimatedBars`] core that handles cava
//! connection, smoothing, and tick-callback lifecycle:
//!
//! - [`MediaVisualizer`] — blob that morphs around album art (popover/window).
//! - [`BarVisualizer`] — thin waveform underline for the bar widget.

use std::cell::{Cell, RefCell};
use std::f64::consts::{FRAC_PI_2, FRAC_PI_4, PI};
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Align, DrawingArea, cairo};

use crate::services::callbacks::CallbackId;
use crate::services::cava::{CavaService, NUM_BARS};
use crate::styles::{color, media};

// ============================================================================
// Shared animation core
// ============================================================================

/// Responsiveness factor (0.0 = no change, 1.0 = instant snap to target).
const SMOOTHING: f64 = 0.70;

/// Smoothing factor when paused (slower decay to zero).
const PAUSE_SMOOTHING: f64 = 0.15;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// Not yet started; drawing area hidden.
    Inactive,
    /// Connected to cava, actively animating.
    Playing,
    /// Decaying to zero (bars smoothing toward zero).
    Paused,
}

/// Shared animation state for cava-driven visualizers.
///
/// While playing, smoothing and redraws happen directly in the cava callback
/// (already fires at 60fps) — no tick callback needed. A tick callback is
/// only used for the brief pause-decay animation.
#[derive(Clone)]
struct AnimatedBars {
    bars: Rc<RefCell<[f64; NUM_BARS]>>,
    target_bars: Rc<RefCell<[f64; NUM_BARS]>>,
    state: Rc<Cell<State>>,
    cava_callback_id: Rc<RefCell<Option<CallbackId>>>,
    /// Bumped to cancel stale tick callbacks.
    generation: Rc<Cell<u64>>,
    /// Only redraw every N-th cava frame (1 = every frame).
    redraw_divisor: u32,
    frame_counter: Rc<Cell<u32>>,
}

impl AnimatedBars {
    fn new(redraw_divisor: u32) -> Self {
        Self {
            bars: Rc::new(RefCell::new([0.0; NUM_BARS])),
            target_bars: Rc::new(RefCell::new([0.0; NUM_BARS])),
            state: Rc::new(Cell::new(State::Inactive)),
            cava_callback_id: Rc::new(RefCell::new(None)),
            generation: Rc::new(Cell::new(0)),
            redraw_divisor: redraw_divisor.max(1),
            frame_counter: Rc::new(Cell::new(0)),
        }
    }

    /// Start animating. Connects to cava; smoothing + redraw happen in the
    /// cava callback directly.
    fn start(&self, da: &DrawingArea) {
        if self.state.get() == State::Playing {
            return;
        }

        let cava = CavaService::global();
        if !cava.available() {
            da.set_visible(false);
            return;
        }

        // Cancel any running pause-decay tick callback.
        self.generation.set(self.generation.get().wrapping_add(1));

        da.set_visible(true);
        self.state.set(State::Playing);
        self.frame_counter.set(0);

        cava.start();

        let bars = self.bars.clone();
        let target_bars = self.target_bars.clone();
        let da_clone = da.clone();
        let frame_counter = self.frame_counter.clone();
        let divisor = self.redraw_divisor;
        let cava_id = cava.connect(move |snapshot| {
            if snapshot.running {
                let mut targets = target_bars.borrow_mut();
                let mut current = bars.borrow_mut();
                for i in 0..NUM_BARS {
                    targets[i] = snapshot.bars[i] as f64;
                    current[i] += (targets[i] - current[i]) * SMOOTHING;
                }
            }
            let count = frame_counter.get().wrapping_add(1);
            frame_counter.set(count);
            if count.is_multiple_of(divisor) {
                da_clone.queue_draw();
            }
        });
        *self.cava_callback_id.borrow_mut() = Some(cava_id);
    }

    /// Pause: disconnect from cava and smoothly decay bars to zero.
    fn pause(&self, da: &DrawingArea) {
        if self.state.get() == State::Paused {
            return;
        }

        // Fresh visualizer (e.g. popover reopened while paused):
        // show a static shape without starting cava.
        if self.state.get() == State::Inactive {
            self.state.set(State::Paused);
            da.set_visible(true);
            da.queue_draw();
            return;
        }

        self.state.set(State::Paused);

        if let Some(cava_id) = self.cava_callback_id.borrow_mut().take() {
            CavaService::global().disconnect(cava_id);
        }

        self.target_bars.borrow_mut().fill(0.0);

        // Use a tick callback only for the brief decay animation.
        let current_gen = self.generation.get();
        let generation = self.generation.clone();
        let bars = self.bars.clone();
        let target_bars = self.target_bars.clone();
        let last_frame_time = Rc::new(Cell::new(0i64));

        da.add_tick_callback(move |da, frame_clock| {
            if generation.get() != current_gen {
                return glib::ControlFlow::Break;
            }

            let now = frame_clock.frame_time();
            let last = last_frame_time.get();
            let dt = if last == 0 {
                1.0 / 60.0
            } else {
                (now - last) as f64 / 1_000_000.0
            };
            last_frame_time.set(now);

            let settled;
            {
                let targets = target_bars.borrow();
                let mut current = bars.borrow_mut();
                let mut max_diff = 0.0f64;
                let factor = 1.0 - (1.0 - PAUSE_SMOOTHING).powf(dt * 60.0);
                for (cur, &tgt) in current.iter_mut().zip(targets.iter()) {
                    *cur += (tgt - *cur) * factor;
                    max_diff = max_diff.max((tgt - *cur).abs());
                }
                settled = max_diff < 0.005;
            }

            da.queue_draw();

            if settled {
                let targets = target_bars.borrow();
                let mut current = bars.borrow_mut();
                for (cur, &tgt) in current.iter_mut().zip(targets.iter()) {
                    *cur = tgt;
                }
                da.queue_draw();
                return glib::ControlFlow::Break;
            }

            glib::ControlFlow::Continue
        });
    }

    /// Full stop: pause then kill the cava subprocess.
    fn stop(&self, da: &DrawingArea) {
        self.pause(da);
        CavaService::global().stop();
    }
}

impl Drop for AnimatedBars {
    fn drop(&mut self) {
        self.generation.set(self.generation.get().wrapping_add(1));

        if let Some(cava_id) = self.cava_callback_id.borrow_mut().take() {
            CavaService::global().disconnect(cava_id);
        }
    }
}

// ============================================================================
// Blob visualizer (popover / window)
// ============================================================================

/// Maximum outward displacement (px) at full amplitude.
pub const BLOB_MAX_DISPLACEMENT: f64 = 16.0;

/// Inward shrink of the clip exclusion zone. Also serves as the base
/// outward displacement so the blob forms a visible border at rest.
const BLOB_CLIP_INSET: f64 = 2.0;

/// Outer glow opacity.
const BLOB_GLOW_OPACITY: f64 = 0.15;

/// Blob visualizer that morphs around album art.
#[derive(Clone)]
pub struct MediaVisualizer {
    drawing_area: DrawingArea,
    anim: AnimatedBars,
}

impl MediaVisualizer {
    /// Create a new visualizer for the given art size.
    ///
    /// `overflow_margin` is added on each side so the blob extends beyond the
    /// album art edges. `corner_radius` should match the art's rounded corners.
    /// `max_displacement` controls how far (px) the blob extends at full amplitude.
    pub fn new(
        art_size: i32,
        overflow_margin: i32,
        corner_radius: f64,
        max_displacement: f64,
    ) -> Self {
        let total_size = art_size + 2 * overflow_margin;
        let drawing_area = DrawingArea::new();
        drawing_area.set_size_request(total_size, total_size);
        drawing_area.set_halign(Align::Center);
        drawing_area.set_valign(Align::Center);
        drawing_area.set_can_target(false);
        drawing_area.add_css_class(media::VISUALIZER);
        drawing_area.add_css_class(color::ACCENT);

        let anim = AnimatedBars::new(1);

        let bars_for_draw = anim.bars.clone();
        let art_f = art_size as f64;
        let cr_f = corner_radius;
        let glow_extra = (max_displacement * 0.3).max(2.0);
        drawing_area.set_draw_func(move |da, cr, width, height| {
            let bars = bars_for_draw.borrow();

            let accent = da.color();
            let ar = accent.red() as f64;
            let ag = accent.green() as f64;
            let ab = accent.blue() as f64;

            let w = width as f64;
            let h = height as f64;

            let art_x = (w - art_f) / 2.0;
            let art_y = (h - art_f) / 2.0;
            let rect = RoundedRect::new(art_x, art_y, art_f, art_f, cr_f);

            // Clip out the art interior so the blob only shows around the edges.
            let clip = RoundedRect::new(
                art_x + BLOB_CLIP_INSET,
                art_y + BLOB_CLIP_INSET,
                art_f - 2.0 * BLOB_CLIP_INSET,
                art_f - 2.0 * BLOB_CLIP_INSET,
                (cr_f - BLOB_CLIP_INSET).max(0.0),
            );
            cr.new_path();
            cr.rectangle(0.0, 0.0, w, h);
            clip.clip_path(cr);
            cr.set_fill_rule(cairo::FillRule::EvenOdd);
            cr.clip();

            // Resample 24 bars to 20 for the blob — fewer, broader undulations.
            let blob_n = 20;
            let blob_bars: Vec<f64> = (0..blob_n)
                .map(|i| {
                    let src = i as f64 * (bars.len() - 1) as f64 / (blob_n - 1) as f64;
                    let lo = src as usize;
                    let hi = (lo + 1).min(bars.len() - 1);
                    let frac = src - lo as f64;
                    bars[lo] * (1.0 - frac) + bars[hi] * frac
                })
                .collect();

            let color = (ar, ag, ab);
            draw_rect_blob(
                cr,
                &rect,
                &blob_bars,
                max_displacement,
                glow_extra,
                color,
                BLOB_GLOW_OPACITY,
            );
            draw_rect_blob(cr, &rect, &blob_bars, max_displacement, 0.0, color, 1.0);
        });

        Self { drawing_area, anim }
    }

    pub fn widget(&self) -> &DrawingArea {
        &self.drawing_area
    }

    pub fn start(&self) {
        self.anim.start(&self.drawing_area);
    }

    pub fn pause(&self) {
        self.anim.pause(&self.drawing_area);
    }

    pub fn stop(&self) {
        self.anim.stop(&self.drawing_area);
    }
}

// ============================================================================
// Bar waveform visualizer
// ============================================================================

/// Maximum upward displacement (px) at full amplitude.
const WAVE_MAX_HEIGHT: f64 = 10.0;

/// Base line thickness (px) when bars are at zero.
const WAVE_BASE_HEIGHT: f64 = 2.0;

/// Height of the DrawingArea (enough room for max peak + base).
const WAVE_AREA_HEIGHT: i32 = 14;

/// Thin waveform underline for the bar widget.
#[derive(Clone)]
pub struct BarVisualizer {
    drawing_area: DrawingArea,
    anim: AnimatedBars,
}

impl BarVisualizer {
    pub fn new() -> Self {
        let drawing_area = DrawingArea::new();
        drawing_area.set_hexpand(true);
        drawing_area.set_valign(Align::End);
        drawing_area.set_halign(Align::Fill);
        drawing_area.set_size_request(-1, WAVE_AREA_HEIGHT);
        drawing_area.set_can_target(false);
        drawing_area.add_css_class(media::BAR_VISUALIZER);
        drawing_area.add_css_class(color::ACCENT);

        let anim = AnimatedBars::new(3);

        let bars_for_draw = anim.bars.clone();
        drawing_area.set_draw_func(move |da, cr, width, height| {
            let bars = bars_for_draw.borrow();

            let accent = da.color();
            let r = accent.red() as f64;
            let g = accent.green() as f64;
            let b = accent.blue() as f64;

            let w = width as f64;
            let h = height as f64;
            let n = bars.len();
            if n < 3 || w < 1.0 {
                return;
            }

            draw_waveform(cr, &*bars, w, h, (r, g, b));
        });

        Self { drawing_area, anim }
    }

    pub fn widget(&self) -> &DrawingArea {
        &self.drawing_area
    }

    pub fn start(&self) {
        self.anim.start(&self.drawing_area);
    }

    pub fn pause(&self) {
        self.anim.pause(&self.drawing_area);
    }

    pub fn stop(&self) {
        self.anim.stop(&self.drawing_area);
    }
}

/// Draw the waveform: smooth spline top edge, flat bottom edge.
fn draw_waveform(cr: &cairo::Context, bars: &[f64], w: f64, h: f64, (r, g, b): (f64, f64, f64)) {
    let n = bars.len();
    let baseline = h;

    // Base height rides along on bar values: full during playback (bars > 0),
    // fades to zero during pause decay. Ramps up quickly so even quiet music
    // shows the full base.
    let max_bar = bars.iter().cloned().fold(0.0f64, f64::max);
    let base = WAVE_BASE_HEIGHT * (max_bar * 5.0).min(1.0);

    let points: Vec<(f64, f64)> = (0..n)
        .map(|i| {
            let x = (i as f64 + 0.5) / n as f64 * w;
            let y = baseline - base - bars[i] * WAVE_MAX_HEIGHT;
            (x, y)
        })
        .collect();

    cr.new_path();

    // Left edge: start at bottom-left, line up to first point height.
    cr.move_to(0.0, baseline);
    cr.line_to(0.0, points[0].1);

    // Catmull-Rom spline through control points.
    for i in 0..n {
        let (x1, y1) = points[i];
        let (x2, y2) = points[(i + 1).min(n - 1)];
        let (x0, y0) = if i > 0 { points[i - 1] } else { points[0] };
        let (x3, y3) = points[(i + 2).min(n - 1)];

        if i == 0 {
            cr.line_to(x1, y1);
        }

        // Catmull-Rom to cubic Bézier control points.
        // Y divisor of 4.0 (vs standard 6.0) exaggerates vertical tangents,
        // creating sharper peaks and valleys.
        let cp1x = x1 + (x2 - x0) / 6.0;
        let cp1y = y1 + (y2 - y0) / 4.0;
        let cp2x = x2 - (x3 - x1) / 6.0;
        let cp2y = y2 - (y3 - y1) / 4.0;

        cr.curve_to(cp1x, cp1y, cp2x, cp2y, x2, y2);
    }

    // Right edge: line down to bottom-right, close along bottom.
    cr.line_to(w, points[n - 1].1);
    cr.line_to(w, baseline);
    cr.close_path();

    cr.set_source_rgba(r, g, b, 1.0);
    let _ = cr.fill();
}

// ============================================================================
// Blob drawing helpers
// ============================================================================

/// Precomputed rounded-rectangle geometry for the blob drawing functions.
///
/// Reused across glow + main blob passes and all per-point perimeter
/// lookups — avoids redundant trig/length calculations.
struct RoundedRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    r: f64,
    edge_h: f64,
    edge_v: f64,
    corner_arc: f64,
    perimeter: f64,
}

impl RoundedRect {
    fn new(x: f64, y: f64, w: f64, h: f64, corner_radius: f64) -> Self {
        let r = corner_radius.min(w / 2.0).min(h / 2.0).max(0.5);
        let edge_h = w - 2.0 * r;
        let edge_v = h - 2.0 * r;
        let corner_arc = FRAC_PI_2 * r;
        let perimeter = 2.0 * edge_h + 2.0 * edge_v + 4.0 * corner_arc;
        Self {
            x,
            y,
            w,
            h,
            r,
            edge_h,
            edge_v,
            corner_arc,
            perimeter,
        }
    }

    /// Add a closed sub-path (clockwise) for even-odd clipping.
    fn clip_path(&self, cr: &cairo::Context) {
        cr.new_sub_path();
        cr.arc(
            self.x + self.w - self.r,
            self.y + self.r,
            self.r,
            -FRAC_PI_2,
            0.0,
        );
        cr.arc(
            self.x + self.w - self.r,
            self.y + self.h - self.r,
            self.r,
            0.0,
            FRAC_PI_2,
        );
        cr.arc(
            self.x + self.r,
            self.y + self.h - self.r,
            self.r,
            FRAC_PI_2,
            PI,
        );
        cr.arc(
            self.x + self.r,
            self.y + self.r,
            self.r,
            PI,
            3.0 * FRAC_PI_2,
        );
        cr.close_path();
    }

    /// Map parametric distance `t` along the perimeter to position and
    /// outward unit normal.
    ///
    /// Clockwise from top-left: top edge, TR corner, right edge, BR corner,
    /// bottom edge, BL corner, left edge, TL corner.
    fn perimeter_point(&self, t: f64) -> (f64, f64, f64, f64) {
        let t = t.rem_euclid(self.perimeter);
        let mut rem = t;
        let (x, y, w, h, r) = (self.x, self.y, self.w, self.h, self.r);

        // Top edge
        if rem < self.edge_h {
            let frac = rem / self.edge_h;
            return (x + r + frac * self.edge_h, y, 0.0, -1.0);
        }
        rem -= self.edge_h;

        // TR corner
        if rem < self.corner_arc {
            let a = rem / r;
            return (
                x + w - r + r * a.sin(),
                y + r - r * a.cos(),
                a.sin(),
                -a.cos(),
            );
        }
        rem -= self.corner_arc;

        // Right edge
        if rem < self.edge_v {
            let frac = rem / self.edge_v;
            return (x + w, y + r + frac * self.edge_v, 1.0, 0.0);
        }
        rem -= self.edge_v;

        // BR corner
        if rem < self.corner_arc {
            let a = rem / r;
            return (
                x + w - r + r * a.cos(),
                y + h - r + r * a.sin(),
                a.cos(),
                a.sin(),
            );
        }
        rem -= self.corner_arc;

        // Bottom edge
        if rem < self.edge_h {
            let frac = rem / self.edge_h;
            return (x + w - r - frac * self.edge_h, y + h, 0.0, 1.0);
        }
        rem -= self.edge_h;

        // BL corner
        if rem < self.corner_arc {
            let a = rem / r;
            return (
                x + r - r * a.sin(),
                y + h - r + r * a.cos(),
                -a.sin(),
                a.cos(),
            );
        }
        rem -= self.corner_arc;

        // Left edge
        if rem < self.edge_v {
            let frac = rem / self.edge_v;
            return (x, y + h - r - frac * self.edge_v, -1.0, 0.0);
        }
        rem -= self.edge_v;

        // TL corner
        let a = (rem / r).min(FRAC_PI_2);
        (x + r - r * a.cos(), y + r - r * a.sin(), -a.cos(), -a.sin())
    }
}

/// Draw one blob pass around the art rectangle.
///
/// Control points are uniformly distributed along the perimeter, offset
/// so point 0 is at the TL corner arc midpoint (4-fold symmetry).
fn draw_rect_blob(
    cr: &cairo::Context,
    rect: &RoundedRect,
    bars: &[f64],
    max_displacement: f64,
    base_extra: f64,
    (r, g, b): (f64, f64, f64),
    opacity: f64,
) {
    let n = bars.len();
    if n < 3 {
        return;
    }

    // Offset so point 0 is at TL corner arc midpoint.
    let offset = rect.perimeter - FRAC_PI_4 * rect.r;

    let points: Vec<(f64, f64)> = (0..n)
        .map(|i| {
            let t = (i as f64 / n as f64) * rect.perimeter + offset;
            let (px, py, nx, ny) = rect.perimeter_point(t);
            let disp = BLOB_CLIP_INSET + base_extra + bars[i] * max_displacement;
            (px + nx * disp, py + ny * disp)
        })
        .collect();

    cr.new_path();

    for i in 0..n {
        let (x1, y1) = points[i];
        let (x2, y2) = points[(i + 1) % n];
        let (x0, y0) = points[(i + n - 1) % n];
        let (x3, y3) = points[(i + 2) % n];

        if i == 0 {
            cr.move_to(x1, y1);
        }

        // Catmull-Rom to cubic Bézier control points.
        let cp1x = x1 + (x2 - x0) / 6.0;
        let cp1y = y1 + (y2 - y0) / 6.0;
        let cp2x = x2 - (x3 - x1) / 6.0;
        let cp2y = y2 - (y3 - y1) / 6.0;

        cr.curve_to(cp1x, cp1y, cp2x, cp2y, x2, y2);
    }

    cr.close_path();
    cr.set_source_rgba(r, g, b, opacity);
    let _ = cr.fill();
}
