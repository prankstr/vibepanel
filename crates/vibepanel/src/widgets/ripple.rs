//! Material Design ripple effect for GTK4 widgets.
//!
//! Provides a Cairo-based ripple animation that expands from the click point
//! as a flat-opacity circle, matching Material Design spec. The ripple is
//! rendered via a `DrawingArea` overlaid on top of the target widget.
//!
//! # Usage
//!
//! For buttons, use [`vp_button()`] (and variants) which automatically apply
//! the ripple effect. For non-button widgets, use [`wrap_with_ripple()`] to
//! wrap a widget in an `Overlay` with a ripple drawing area on top.
//!
//! Clipping to rounded corners is handled by `Overlay::set_overflow(Hidden)`
//! rather than manual Cairo border-radius clipping — the GTK compositor
//! already knows the widget's CSS border-radius.

use gtk4::glib;
use gtk4::glib::object::IsA;
use gtk4::prelude::*;
use gtk4::{Align, DrawingArea, GestureClick, ListBoxRow, Overlay};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use tracing::warn;

use crate::styles::state;

/// Total ripple animation duration in milliseconds.
/// Radius expansion takes RIPPLE_EXPAND_MS; fade-out starts after RIPPLE_FADE_DELAY
/// fraction and takes the remaining time.
const RIPPLE_EXPAND_MS: f64 = 500.0;

/// Fraction of expansion duration to hold full opacity before starting fade-out.
const RIPPLE_FADE_DELAY: f64 = 0.6;

/// Ripple circle opacity (flat, not gradient -- matches Material Design spec).
const RIPPLE_OPACITY: f64 = 0.10;

/// Total animation duration: expansion + fade-out tail.
/// Fade-out starts at `RIPPLE_EXPAND_MS * RIPPLE_FADE_DELAY` and runs to this value.
const RIPPLE_TOTAL_MS: f64 = RIPPLE_EXPAND_MS * (2.0 - RIPPLE_FADE_DELAY);

/// Animation state for a single ripple instance.
struct RippleState {
    x: f64,
    y: f64,
    /// Distance from click to farthest corner.
    max_radius: f64,
    /// Microseconds from frame clock.
    start_time: i64,
    active: bool,
}

/// Shared state for the ripple drawing area.
#[derive(Clone)]
pub struct RippleHandle {
    state: Rc<RefCell<Option<RippleState>>>,
    drawing_area: DrawingArea,
    /// Whether the widget's theme is dark mode (ripple tint = white) or light (black).
    is_dark: Rc<Cell<bool>>,
    /// Generation counter — incremented on each new ripple trigger so stale
    /// tick callbacks from previous ripples exit immediately instead of
    /// running until their animation duration expires.
    generation: Rc<Cell<u64>>,
}

impl RippleHandle {
    /// Create a new ripple handle with a `DrawingArea` for Cairo-based rendering.
    pub(crate) fn new() -> Self {
        let drawing_area = DrawingArea::new();
        drawing_area.add_css_class(state::RIPPLE_OVERLAY);
        drawing_area.set_can_target(false); // Transparent to pointer events
        drawing_area.set_hexpand(true);
        drawing_area.set_vexpand(true);
        drawing_area.set_halign(Align::Fill);
        drawing_area.set_valign(Align::Fill);

        let state: Rc<RefCell<Option<RippleState>>> = Rc::new(RefCell::new(None));
        let is_dark = Rc::new(Cell::new(true)); // default to dark theme

        let state_for_draw = state.clone();
        let dark_for_draw = is_dark.clone();
        drawing_area.set_draw_func(move |da, cr, _width, _height| {
            let state_ref = state_for_draw.borrow();
            let Some(ripple) = state_ref.as_ref() else {
                return;
            };
            if !ripple.active {
                return;
            }

            // Get frame clock time to compute progress
            let elapsed = if let Some(clock) = da.frame_clock() {
                let now = clock.frame_time();
                (now - ripple.start_time) as f64 / 1000.0 // convert us to ms
            } else {
                return;
            };

            // Radius expansion: aggressive decel curve (cubic-bezier(0, 0, 0, 1))
            let expand_t = (elapsed / RIPPLE_EXPAND_MS).clamp(0.0, 1.0);
            // Approximate cubic-bezier(0, 0, 0, 1) -- very aggressive decel
            // This reaches ~80% at t=0.3, giving the snappy feel
            let eased_t = 1.0 - (1.0 - expand_t).powi(3);
            let radius = ripple.max_radius * eased_t;

            // Opacity: hold at full for RIPPLE_FADE_DELAY of expand duration,
            // then fade out over the remaining time
            let fade_start = RIPPLE_EXPAND_MS * RIPPLE_FADE_DELAY;
            let opacity = if elapsed < fade_start {
                RIPPLE_OPACITY
            } else {
                let fade_t =
                    ((elapsed - fade_start) / (RIPPLE_TOTAL_MS - fade_start)).clamp(0.0, 1.0);
                // Ease out the fade: cubic-bezier(0.2, 0, 0, 1) approximation
                let eased_fade = 1.0 - (1.0 - fade_t).powi(2);
                RIPPLE_OPACITY * (1.0 - eased_fade)
            };

            if opacity <= 0.001 {
                return;
            }

            // Draw flat-opacity circle from click point
            // (Clipping to rounded corners is handled by Overlay::set_overflow(Hidden))
            if dark_for_draw.get() {
                cr.set_source_rgba(1.0, 1.0, 1.0, opacity);
            } else {
                cr.set_source_rgba(0.0, 0.0, 0.0, opacity);
            }
            cr.arc(ripple.x, ripple.y, radius, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        });

        Self {
            state,
            drawing_area,
            is_dark,
            generation: Rc::new(Cell::new(0)),
        }
    }

    /// Get the `DrawingArea` widget for embedding in an overlay.
    pub fn widget(&self) -> &DrawingArea {
        &self.drawing_area
    }
}

/// Trigger a Material Design-style ripple animation from the click point.
///
/// The ripple is a flat-opacity circle that expands from `(x, y)` to the
/// farthest corner of the widget, using an aggressive deceleration curve.
/// Opacity holds steady during expansion, then fades out.
pub fn trigger_ripple(handle: &RippleHandle, x: f64, y: f64) {
    // Respect the global ripple config toggle
    if !crate::services::config_manager::ConfigManager::global().ripple_enabled() {
        return;
    }

    let da = &handle.drawing_area;
    let width = da.width() as f64;
    let height = da.height() as f64;

    // Calculate max radius: distance from click point to farthest corner
    let dist_sq = |dx: f64, dy: f64| dx * dx + dy * dy;
    let max_radius = dist_sq(x, y)
        .max(dist_sq(x, height - y))
        .max(dist_sq(width - x, y))
        .max(dist_sq(width - x, height - y))
        .sqrt();

    // Get start time from frame clock. If no frame clock is available the
    // widget is not mapped/displayed — bail out rather than leaving an
    // active: true state that no tick callback would ever clear.
    let Some(start_time) = da.frame_clock().map(|fc| fc.frame_time()) else {
        return;
    };

    // Detect dark mode from the widget's computed text color luminance
    // so the ripple tint matches the theme (white on dark, black on light).
    let is_dark = detect_dark_mode(da);
    handle.is_dark.set(is_dark);

    // Increment generation so any tick callback from a previous ripple
    // exits immediately on the next frame instead of running to completion.
    let ripple_gen = handle.generation.get().wrapping_add(1);
    handle.generation.set(ripple_gen);

    // Store the new ripple state
    *handle.state.borrow_mut() = Some(RippleState {
        x,
        y,
        max_radius,
        start_time,
        active: true,
    });

    // Add a tick callback to drive the animation.
    let state_ref = handle.state.clone();
    let gen_ref = handle.generation.clone();
    da.add_tick_callback(move |da, frame_clock| {
        // Stale callback from a superseded ripple — exit immediately.
        if gen_ref.get() != ripple_gen {
            return glib::ControlFlow::Break;
        }
        let should_continue = {
            let state = state_ref.borrow();
            if let Some(ripple) = state.as_ref() {
                if !ripple.active {
                    return glib::ControlFlow::Break;
                }
                let elapsed = (frame_clock.frame_time() - ripple.start_time) as f64 / 1000.0;
                elapsed < RIPPLE_TOTAL_MS
            } else {
                false
            }
        };

        da.queue_draw();

        if should_continue {
            glib::ControlFlow::Continue
        } else {
            // Animation complete -- clear state so draw func stops rendering
            if let Some(s) = state_ref.borrow_mut().as_mut() {
                s.active = false;
            }
            da.queue_draw(); // one final draw to clear
            glib::ControlFlow::Break
        }
    });
}

/// Trigger a ripple from a gesture's press coordinates.
///
/// Converts the press coordinates from the gesture widget's coordinate space
/// to the ripple overlay's coordinate space, then triggers the ripple animation.
pub fn trigger_ripple_from_gesture(
    gesture: &gtk4::GestureClick,
    x: f64,
    y: f64,
    handle: &RippleHandle,
) {
    if let Some(gesture_widget) = gesture.widget() {
        let point = gtk4::graphene::Point::new(x as f32, y as f32);
        if let Some(ripple_point) = gesture_widget.compute_point(handle.widget(), &point) {
            trigger_ripple(handle, ripple_point.x() as f64, ripple_point.y() as f64);
        }
    }
}

/// Wrap a widget in an `Overlay` with a `RippleHandle` drawing area on top.
///
/// Returns `(overlay, ripple_handle)`. The caller is responsible for
/// attaching a `GestureClick` to a trigger widget (which may be the
/// wrapped widget itself, or a different one) and calling
/// `trigger_ripple_from_gesture()` on press.
///
/// The ripple is clipped to the widget's CSS border-radius via
/// `Overlay::set_overflow(Hidden)`.
pub(crate) fn wrap_with_ripple(widget: &impl IsA<gtk4::Widget>) -> (Overlay, RippleHandle) {
    let overlay = Overlay::new();
    overlay.set_child(Some(widget));
    overlay.set_overflow(gtk4::Overflow::Hidden);
    overlay.add_css_class(state::RIPPLE_WRAP);

    let ripple_handle = RippleHandle::new();
    overlay.add_overlay(ripple_handle.widget());

    (overlay, ripple_handle)
}

/// Add a Material Design ripple overlay to a button.
///
/// Takes the button's existing child, wraps it in an `Overlay` with a
/// `RippleHandle` drawing area on top, and sets the overlay as the
/// button's new child. The button itself is unchanged -- callers still
/// interact with it as a normal `Button`.
///
/// The ripple is triggered by clicks on the button itself.
///
/// Does nothing (with a warning) if the button has no child.
/// Call this *after* `set_child()`.
pub(crate) fn add_ripple(button: &gtk4::Button) {
    let Some(child) = button.child() else {
        warn!("add_ripple: button has no child, skipping ripple");
        return;
    };

    // Detach the child from the button
    button.set_child(gtk4::Widget::NONE);

    let (overlay, ripple_handle) = wrap_with_ripple(&child);

    // Attach gesture in capture phase to trigger ripple on press
    let gesture = GestureClick::new();
    gesture.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let rh = ripple_handle;
    gesture.connect_pressed(move |gesture, _n_press, x, y| {
        trigger_ripple_from_gesture(gesture, x, y, &rh);
    });
    button.add_controller(gesture);

    // Mark the button so CSS can zero its padding (the ripple overlay
    // needs to fill edge-to-edge for the animation to cover the full
    // hover background area).
    button.add_css_class(state::HAS_RIPPLE);

    // Set the overlay as the button's new child
    button.set_child(Some(&overlay));
}

/// Create a `Button` with automatic ripple.
///
/// Returns a plain `gtk4::Button`. The first time a child widget is set
/// (via `set_child()`, `set_label()`, or `set_icon_name()`), a Material
/// Design ripple overlay is automatically applied.
///
/// Uses `SignalHandlerId` to cleanly disconnect the notify handler after
/// first use, avoiding re-entrant firing when `add_ripple()` re-parents
/// the child.
///
/// Use raw `Button::new()` instead when ripple is explicitly unwanted
/// (e.g. `button::LINK` text links, `button::RESET` dismiss buttons,
/// power-card hold-to-confirm buttons).
pub(crate) fn vp_button() -> gtk4::Button {
    let btn = gtk4::Button::new();

    // Store the signal handler ID so we can disconnect after first use.
    // `add_ripple()` calls `set_child(NONE)` then `set_child(Some(&overlay))`,
    // which re-triggers notify::child. Without disconnecting, the boolean
    // flag approach was a fragile guard against re-entrant calls.
    let signal_id: Rc<RefCell<Option<glib::SignalHandlerId>>> = Rc::new(RefCell::new(None));
    let signal_id_c = signal_id.clone();

    let id = btn.connect_notify_local(Some("child"), move |btn, _| {
        if btn.child().is_some() {
            // Disconnect ourselves *before* calling add_ripple, which will
            // set_child(NONE) + set_child(Some(overlay)) and would re-enter
            // this handler if we hadn't disconnected.
            if let Some(id) = signal_id_c.borrow_mut().take() {
                btn.disconnect(id);
            }
            add_ripple(btn);
        }
    });

    *signal_id.borrow_mut() = Some(id);
    btn
}

/// Create a labeled `Button` with automatic ripple.
///
/// Equivalent to `Button::with_label(label)` plus `add_ripple()`.
/// Use raw `Button::with_label()` when ripple is explicitly unwanted.
pub(crate) fn vp_button_with_label(label: &str) -> gtk4::Button {
    let btn = gtk4::Button::with_label(label);
    add_ripple(&btn);
    btn
}

/// Create an icon-name `Button` with automatic ripple.
///
/// Equivalent to `Button::from_icon_name(icon_name)` plus `add_ripple()`.
/// Use raw `Button::from_icon_name()` when ripple is explicitly unwanted.
pub(crate) fn vp_button_from_icon_name(icon_name: &str) -> gtk4::Button {
    let btn = gtk4::Button::from_icon_name(icon_name);
    add_ripple(&btn);
    btn
}

/// Add a ripple overlay to a `ListBoxRow`.
///
/// Wraps `content` in an `Overlay` with a ripple drawing area, sets
/// `set_overflow(Hidden)` for border-radius clipping, attaches a
/// capture-phase gesture to trigger the ripple, and marks the row
/// with `HAS_RIPPLE` so CSS can zero its padding.
///
/// The caller should transfer any row padding to content margins before
/// calling this (the CSS rule `.qs-row.vp-has-ripple { padding: 0 }`
/// zeros the row padding so the ripple fills edge-to-edge).
pub(crate) fn add_ripple_to_row(list_row: &ListBoxRow, content: &impl IsA<gtk4::Widget>) {
    list_row.add_css_class(state::HAS_RIPPLE);

    let (ripple_overlay, ripple_handle) = wrap_with_ripple(content);

    let gesture = GestureClick::new();
    gesture.set_propagation_phase(gtk4::PropagationPhase::Capture);
    gesture.connect_pressed(move |gesture, _n_press, x, y| {
        trigger_ripple_from_gesture(gesture, x, y, &ripple_handle);
    });
    list_row.add_controller(gesture);

    list_row.set_child(Some(&ripple_overlay));
}

/// Detect whether the current theme is dark mode.
///
/// Checks the widget's computed CSS `color` property. In dark mode,
/// the foreground text is light (luminance > 0.5); in light mode it's dark.
fn detect_dark_mode(widget: &impl IsA<gtk4::Widget>) -> bool {
    let color = widget.as_ref().color();
    let luminance =
        0.299 * color.red() as f64 + 0.587 * color.green() as f64 + 0.114 * color.blue() as f64;
    luminance > 0.5
}
