//! Workspaces widget — displays workspace indicators with animated transitions.
//!
//! Shows occupied/active workspaces as visual indicators (dots/pills/labels).
//! Clicking an indicator switches to that workspace.
//!
//! # Configuration
//!
//! - `label_type`: `"none"` (minimal dots, default), `"icons"` (●/○/◆ glyphs),
//!   or `"numbers"` (workspace names).
//! - `separator`: string between indicators (non-minimal modes only).
//! - `animate`: `true` (default) enables the `WorkspaceContainer` custom widget
//!   for smooth transitions; `false` uses a plain GtkBox with no animation.
//!
//! # Architecture
//!
//! ## Two layout modes
//!
//! - **Minimal** (`label_type = "none"`, `animate = true`): Indicators are
//!   children of a [`WorkspaceContainer`] custom widget that measures children's
//!   live CSS widths each frame, preventing neighboring bar widgets from
//!   jittering during transitions.
//!
//! - **Non-minimal** (or `animate = false`): Indicators go directly in the
//!   content `GtkBox`. No animation, no `WorkspaceContainer`.
//!
//! ## WorkspaceContainer layout
//!
//! Children are split into two groups around the active indicator:
//! - **Left group** `[0..=active]`: laid out left-to-right from x=0.
//! - **Right group** `[active+1..n]`: laid out right-to-left from x=width.
//!
//! The gap between groups absorbs ±1px subpixel rounding drift from
//! CSS-animated `min-width` values. `Overflow::Hidden` clips any transient
//! overshoot. See [`compute_left_count`] for the split rules.
//!
//! ## Animation paths
//!
//! [`classify_change`] determines which of four paths to take:
//!
//! - **Switch** (`ChangeType::None`): Same IDs, different active. CSS classes
//!   update in place. Active grows / inactive shrinks are self-balancing (same
//!   transition duration), so container width stays constant. No container
//!   animation needed.
//!
//! - **Removal** (`ChangeType::Removal`): Departed indicators are surgically
//!   removed (preserving surviving widgets' GTK identity). The two-group split
//!   is frozen ([`frozen_left_count`]) to prevent group-jumping. Container
//!   animates from pre-removal to post-removal width.
//!
//! - **Addition** (`ChangeType::Addition`): Full recreate. New indicators get
//!   `.workspace-grow-in` (min-width: 0) + `.workspace-grow-in-notrans`
//!   (transition: none). The grow-in convergence tick callback removes these
//!   classes over two frames, firing CSS transitions from 0→full width.
//!   Container tracks `children_width` directly during this phase.
//!
//! - **Swap** (`ChangeType::Swap`): Same count, different IDs (e.g., ws4→ws5).
//!   Same grow-in mechanism as Addition. Container stays pinned at pre-recreate
//!   width; reconciliation adjusts after CSS transitions settle.
//!
//! ## Key mechanisms
//!
//! - **`suppress_reconcile`**: Set during Addition/Swap while grow-in CSS is
//!   resolving. Prevents `size_allocate` reconciliation from snapping to
//!   partially-resolved widths. Cleared by the convergence tick callback when
//!   `children_width` stabilizes, or by the 2-second safety cap.
//!
//! - **Grow-in convergence** ([`start_grow_in_convergence`]): Three-phase tick
//!   callback — phase 0 re-enables transitions, phase 1 removes `min-width: 0`
//!   (firing the CSS transition), phase 2+ monitors `children_width` for
//!   stability. Uses a generation counter for stale callback detection.
//!
//! - **Live target correction**: The animation tick callback re-measures
//!   `children_width` each frame and updates the target to match. This handles
//!   CSS hot-reloads and the fact that GTK4 batches style resolution (initial
//!   measurements may be stale).
//!
//! - **Reconciliation guard** (`size_allocate`): When no animation is running
//!   and `suppress_reconcile` is off, detects external CSS changes (e.g., user
//!   stylesheet hot-reload) and kicks off a smooth animation to the new width.
//!   The `queue_resize()` call is safe — GTK coalesces resize requests.
//!
//! - **Transient CSS**: `bar.rs` loads `.workspace-grow-in { min-width: 0 }`
//!   and `.workspace-grow-in-notrans { transition: none }` at `USER+200`
//!   priority, ensuring they override user CSS during animations.
//!
//! ## Concurrent tick callbacks
//!
//! The animation tick (from `set_target_width`) and the convergence tick (from
//! `start_grow_in_convergence`) may run simultaneously. This is safe: they
//! write to disjoint `Cell` fields, and GTK runs tick callbacks sequentially
//! on the main thread.
//!
//! ## Testing
//!
//! Pure logic (`classify_change`, `compute_left_count`, `build_tooltip`) is
//! unit-tested. Layout behavior requires a GTK display server and is verified
//! manually.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gtk4::gdk::BUTTON_PRIMARY;
use gtk4::glib;
use gtk4::pango::EllipsizeMode;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{Align, Box as GtkBox, GestureClick, Label, Widget};
use tracing::{debug, trace};
use vibepanel_core::config::WidgetEntry;

use crate::services::callbacks::CallbackId;
use crate::services::config_manager::ConfigManager;
use crate::services::tooltip::TooltipManager;
use crate::services::workspace::{Workspace, WorkspaceService, WorkspaceServiceSnapshot};
use crate::styles::{state, widget};
use crate::widgets::WidgetConfig;
use crate::widgets::base::BaseWidget;
use crate::widgets::warn_unknown_options;

// ---------------------------------------------------------------------------
// WorkspaceContainer — custom widget that follows children's live CSS widths
// and splits them into two groups (left/right anchored) to absorb rounding
// drift during CSS min-width transitions. Uses its own animation only for
// workspace count changes (additions/removals).
// ---------------------------------------------------------------------------

mod ws_container_imp {
    use super::*;
    use std::cell::Cell;

    #[derive(Default)]
    pub struct WorkspaceContainer {
        pub(super) children: RefCell<Vec<Widget>>,
        pub(super) target_width: Cell<i32>,
        pub(super) gap: Cell<i32>,
        /// Current animated width (f64 for smooth interpolation).
        pub(super) current_width: Cell<f64>,
        /// Width at the start of the current animation.
        pub(super) anim_start_width: Cell<f64>,
        /// Frame time (microseconds) at the start of the current animation.
        pub(super) anim_start_time: Cell<Option<i64>>,
        /// Whether an animation is currently running.
        pub(super) animating: Cell<bool>,
        /// When set, size_allocate uses this left-group size instead of
        /// computing from the active CSS class. Used during removal
        /// animations to keep surviving indicators in their original groups.
        pub(super) frozen_left_count: Cell<Option<usize>>,
        /// Temporarily suppresses reconciliation in size_allocate.
        /// Set during additions/swaps while grow-in CSS classes are
        /// resolving, cleared when CSS transitions converge.
        pub(super) suppress_reconcile: Cell<bool>,
        /// Previous frame's children width during convergence detection.
        /// Negative sentinel (-1.0) means "not yet initialized".
        pub(super) convergence_prev_width: Cell<f64>,
        /// Number of consecutive frames where children width changed < 1px.
        /// When this reaches CONVERGENCE_STABLE_FRAMES, transitions have
        /// settled and suppress_reconcile can be cleared.
        pub(super) convergence_stable_frames: Cell<u32>,
        /// Frame time (microseconds) when convergence detection started.
        /// Used for the safety cap timeout.
        pub(super) convergence_start_time: Cell<Option<i64>>,
        /// Generation counter for convergence tick callbacks. Incremented
        /// each time a new grow-in starts; stale callbacks compare their
        /// captured generation against this and self-terminate if it has
        /// advanced.
        pub(super) convergence_generation: Cell<u32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for WorkspaceContainer {
        const NAME: &'static str = "VibepanelWorkspaceContainer";
        type Type = super::WorkspaceContainer;
        type ParentType = Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("workspace-container");
        }
    }

    impl ObjectImpl for WorkspaceContainer {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().set_overflow(gtk4::Overflow::Hidden);
        }

        fn dispose(&self) {
            for child in self.children.borrow_mut().drain(..) {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for WorkspaceContainer {
        fn request_mode(&self) -> gtk4::SizeRequestMode {
            gtk4::SizeRequestMode::ConstantSize
        }

        fn measure(&self, orientation: gtk4::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let children = self.children.borrow();
            if children.is_empty() {
                return (0, 0, -1, -1);
            }
            if orientation == gtk4::Orientation::Horizontal {
                // Always use the animated/reconciled width so that
                // per-frame CSS transition rounding noise does not
                // cause the container to jitter during switches.
                let w = self.current_width.get().round() as i32;
                (w, w, -1, -1)
            } else {
                let mut max_min = 0i32;
                let mut max_nat = 0i32;
                for child in children.iter() {
                    let (cmin, cnat, _, _) = child.measure(orientation, for_size);
                    max_min = max_min.max(cmin);
                    max_nat = max_nat.max(cnat);
                }
                (max_min, max_nat, -1, -1)
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            let children = self.children.borrow();
            let n = children.len();
            if n == 0 {
                return;
            }

            if n == 1 {
                let (_, cw, _, _) = children[0].measure(gtk4::Orientation::Horizontal, height);
                let x = (width - cw) / 2;
                let t = gtk4::gsk::Transform::new()
                    .translate(&gtk4::graphene::Point::new(x as f32, 0.0));
                children[0].allocate(cw, height, baseline, Some(t));
                return;
            }

            let gap = self.gap.get();

            // Two-group split around the active indicator:
            //   Left group: [0..=active_idx]  — laid out left-to-right
            //   Right group: [active_idx+1..n] — laid out right-to-left
            // The gap between groups absorbs ±1px rounding drift from
            // CSS-animated min-width values. Overflow::Hidden clips any
            // transient overshoot during multi-active transitions.
            let active_css = crate::styles::widget::ACTIVE;
            let active_idx = children.iter().position(|c| c.has_css_class(active_css));

            let left_count = if let Some(frozen) = self.frozen_left_count.get() {
                // During removal animations, use the frozen split to keep
                // surviving indicators in their original groups.
                frozen.min(n)
            } else {
                compute_left_count(n, active_idx)
            };

            // Left group: laid out left-to-right from x=0
            let mut x = 0i32;
            for child in children[..left_count].iter() {
                let (_, cw, _, _) = child.measure(gtk4::Orientation::Horizontal, height);
                let t = gtk4::gsk::Transform::new()
                    .translate(&gtk4::graphene::Point::new(x as f32, 0.0));
                child.allocate(cw, height, baseline, Some(t));
                x += cw + gap;
            }

            // Right group: laid out right-to-left from x=width
            let mut rx = width;
            for child in children[left_count..].iter().rev() {
                let (_, cw, _, _) = child.measure(gtk4::Orientation::Horizontal, height);
                rx -= cw;
                let t = gtk4::gsk::Transform::new()
                    .translate(&gtk4::graphene::Point::new(rx as f32, 0.0));
                child.allocate(cw, height, baseline, Some(t));
                rx -= gap;
            }

            // Reconcile target width when CSS changes externally (e.g. user
            // style.css hot-reload). If no animation is running and the
            // children's measured widths diverge from the stored target, snap
            // both target and current width so the container tracks the new
            // CSS dimensions without waiting for the next workspace event.
            // The queue_resize() is safe here: GTK coalesces resize requests
            // and will re-layout on the next frame, not recursively.
            if !self.animating.get() && !self.suppress_reconcile.get() {
                let obj = self.obj();
                let measured = obj.compute_children_current_width();
                if (measured - self.target_width.get()).abs() > 1 {
                    // Animate to the new width rather than snapping, so
                    // additions and CSS hot-reloads transition smoothly.
                    obj.set_target_width(measured);
                }
            }
        }
    }
}

glib::wrapper! {
    pub struct WorkspaceContainer(ObjectSubclass<ws_container_imp::WorkspaceContainer>)
        @extends Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl WorkspaceContainer {
    fn new() -> Self {
        glib::Object::builder().build()
    }

    fn set_target_width(&self, width: i32) {
        let imp = self.imp();
        imp.target_width.set(width);

        let current = imp.current_width.get();
        if (current - width as f64).abs() < 1.0 {
            // Already at target — snap to exact value.
            imp.current_width.set(width as f64);
            self.queue_resize();
            return;
        }

        // Start (or restart) animation from current interpolated position.
        // NOTE: This animation tick callback may coexist with the swap
        // convergence tick callback — that's safe because they modify
        // disjoint Cell fields (animation uses current_width/anim_start_*,
        // convergence uses convergence_*/suppress_reconcile), and the
        // !animating guard in size_allocate prevents conflicts.
        imp.anim_start_width.set(current);
        // anim_start_time will be set on the first tick (we don't have a
        // frame clock reference here outside of a tick callback).
        imp.anim_start_time.set(None);

        if !imp.animating.get() {
            imp.animating.set(true);
            let widget = self.clone();
            // TickCallbackId is intentionally not stored — GTK4 automatically
            // removes tick callbacks when the widget is unrealized/disposed.
            self.add_tick_callback(move |_w, frame_clock| {
                let imp = widget.imp();
                let now = frame_clock.frame_time(); // microseconds

                let start_time = match imp.anim_start_time.get() {
                    Some(t) => t,
                    None => {
                        imp.anim_start_time.set(Some(now));
                        now
                    }
                };

                let elapsed = now - start_time;

                // Live target correction: as CSS resolves across frames,
                // update the target to match children's actual measured
                // widths. This handles the case where the initial target
                // was stale (GTK4 batches style resolution after CSS class
                // changes) and also supports custom CSS overrides.
                //
                // Skip while suppress_reconcile is set: the tracking
                // branch below handles that case by following
                // children_width directly.
                let children_width = sum_children_widths(&imp.children.borrow(), imp.gap.get());
                if !imp.suppress_reconcile.get()
                    && (children_width - imp.target_width.get()).abs() > 1
                {
                    imp.target_width.set(children_width);
                }
                let target = imp.target_width.get() as f64;

                // While suppress_reconcile is active, track children_width
                // directly to avoid overshoot from the arithmetic target.
                // Safe during rapid events: children_width reflects the
                // current (recreated) children, so a new Addition/Swap
                // while this tick is running just sees the latest layout.
                if imp.suppress_reconcile.get() {
                    imp.current_width.set(children_width as f64);
                    widget.queue_resize();
                    return glib::ControlFlow::Continue;
                }

                if elapsed >= INDICATOR_ANIM_DURATION_US {
                    // Animation complete and CSS is settled. Snap to
                    // children's live width (rather than the possibly-stale
                    // target) so that the left-group and right-group
                    // positions are pixel-perfect when frozen_left_count
                    // is cleared.
                    imp.current_width.set(children_width as f64);
                    imp.target_width.set(children_width);
                    imp.animating.set(false);
                    imp.frozen_left_count.set(None);
                    widget.queue_resize();
                    return glib::ControlFlow::Break;
                }

                // Ease-out quadratic.
                let t = elapsed as f64 / INDICATOR_ANIM_DURATION_US as f64;
                let eased = 1.0 - (1.0 - t).powi(2);
                let start = imp.anim_start_width.get();
                let interpolated = start + (target - start) * eased;

                // Floor-clamp: never narrower than children's actual widths.
                // During removal + active-change, the ease-out curve can
                // outpace the CSS transition on the newly-active indicator,
                // which would squeeze neighbours. Using the larger of the
                // two values keeps the layout correct at every frame.
                imp.current_width
                    .set(interpolated.max(children_width as f64));
                widget.queue_resize();
                glib::ControlFlow::Continue
            });
        }
    }

    fn set_gap(&self, gap: i32) {
        self.imp().gap.set(gap);
    }

    fn add_child(&self, child: &Widget) {
        child.set_parent(self);
        self.imp().children.borrow_mut().push(child.clone());
    }

    fn clear_children(&self) {
        self.imp().frozen_left_count.set(None);
        for child in self.imp().children.borrow_mut().drain(..) {
            child.unparent();
        }
    }

    /// Remove a specific child widget by reference. Order-preserving.
    fn remove_child(&self, child: &Widget) {
        let mut children = self.imp().children.borrow_mut();
        if let Some(pos) = children.iter().position(|c| c == child) {
            children.remove(pos);
            child.unparent();
        }
    }

    /// Freeze the left-group size based on where the removed workspace(s)
    /// were in the old layout. The split is placed at the leftmost removal
    /// index so that:
    ///   - indicators to the LEFT of the gap stay left-anchored
    ///   - indicators to the RIGHT of the gap stay right-anchored
    ///   - the container width shrinks, closing the gap where the removed
    ///     workspace was
    ///
    /// `leftmost_removed_idx` is the position in the old children list of
    /// the first workspace being removed.
    fn freeze_left_count_at(&self, leftmost_removed_idx: usize) -> usize {
        let children = self.imp().children.borrow();
        let n = children.len();
        let left_count = leftmost_removed_idx.min(n);
        self.imp().frozen_left_count.set(Some(left_count));
        left_count
    }

    /// Adjust frozen_left_count by subtracting removals (saturating to 0).
    fn adjust_frozen_left_count(&self, removed_from_left: usize) {
        if removed_from_left == 0 {
            return;
        }
        if let Some(frozen) = self.imp().frozen_left_count.get() {
            self.imp()
                .frozen_left_count
                .set(Some(frozen.saturating_sub(removed_from_left)));
        }
    }

    /// Compute the current total width of children from live CSS measurements.
    fn compute_children_current_width(&self) -> i32 {
        let children = self.imp().children.borrow();
        sum_children_widths(&children, self.imp().gap.get())
    }

    /// Seed `current_width` to a known value without triggering animation.
    ///
    /// Used before `set_target_width()` in the removal path where the
    /// animation must start from the pre-removal width rather than the
    /// current interpolated position.
    fn seed_current_width(&self, width: f64) {
        self.imp().current_width.set(width);
    }

    /// Common setup for Addition and Swap grow-in animations.
    ///
    /// Seeds the container width to `seed`, sets the target, and — if
    /// `grow_in_indicators` is non-empty — enables `suppress_reconcile`
    /// and starts the convergence tick callback that waits for CSS
    /// transitions to settle before allowing reconciliation.
    fn begin_grow_in_animation(&self, seed: f64, target: i32, grow_in_indicators: Vec<Widget>) {
        self.seed_current_width(seed);
        self.set_target_width(target);
        if !grow_in_indicators.is_empty() {
            self.imp().suppress_reconcile.set(true);
            self.start_grow_in_convergence(grow_in_indicators);
        }
    }

    /// Start the two-phase grow-in convergence tick callback.
    ///
    /// Used by both Addition and Swap handlers to animate newly-created
    /// indicators. The callback progresses through three phases:
    ///
    /// - **Phase 0** (tick 1): Remove `WORKSPACE_GROW_IN_NOTRANS` — re-enables
    ///   CSS transitions while indicators stay at `min-width: 0`.
    /// - **Phase 1** (tick 2): Remove `WORKSPACE_GROW_IN` — `min-width` changes
    ///   from 0 to the CSS-defined value, firing the CSS transition.
    /// - **Phase 2+** (subsequent ticks): Convergence detection — waits for
    ///   `CONVERGENCE_STABLE_FRAMES` consecutive frames where `children_width`
    ///   changes by less than 1px, then clears `suppress_reconcile` and
    ///   calls `set_target_width()` to smoothly animate any residual mismatch.
    ///
    /// Why not use an idle for grow-in removal? When the idle fires before
    /// the first tick (common), GTK batches the "add class + remove class"
    /// into one style resolution — the indicator never renders at
    /// `min-width: 0`, so the CSS transition from 0→full never fires.
    /// Deferring removal to the first tick guarantees at least one frame
    /// has been rendered with grow-in committed.
    ///
    /// Uses `convergence_*` fields — Addition and Swap are mutually
    /// exclusive `ChangeType` branches, so the fields are never used
    /// concurrently. A generation counter lets stale callbacks from
    /// superseded events self-terminate.
    fn start_grow_in_convergence(&self, grow_in_indicators: Vec<Widget>) {
        let imp = self.imp();
        let generation = imp.convergence_generation.get().wrapping_add(1);
        imp.convergence_generation.set(generation);
        imp.convergence_prev_width.set(-1.0);
        imp.convergence_stable_frames.set(0);
        imp.convergence_start_time.set(None);
        let wsc_ref = self.clone();
        let grow_in_indicators = Rc::new(grow_in_indicators);
        let phase = Rc::new(Cell::new(0u8));
        self.add_tick_callback(move |_w, frame_clock| {
            let imp = wsc_ref.imp();

            if imp.convergence_generation.get() != generation {
                return glib::ControlFlow::Break;
            }

            if !imp.suppress_reconcile.get() {
                return glib::ControlFlow::Break;
            }

            let current_phase = phase.get();

            if current_phase == 0 {
                for ind in grow_in_indicators.iter() {
                    ind.remove_css_class(widget::WORKSPACE_GROW_IN_NOTRANS);
                }
                phase.set(1);
                return glib::ControlFlow::Continue;
            }

            if current_phase == 1 {
                // Remove grow-in — CSS transition fires.
                for ind in grow_in_indicators.iter() {
                    ind.remove_css_class(widget::WORKSPACE_GROW_IN);
                }
                phase.set(2);
                return glib::ControlFlow::Continue;
            }

            let now = frame_clock.frame_time();

            let start = match imp.convergence_start_time.get() {
                Some(t) => t,
                None => {
                    imp.convergence_start_time.set(Some(now));
                    now
                }
            };

            // Safety cap: clear after 2s to prevent stuck state.
            if now - start > CONVERGENCE_SAFETY_CAP_US {
                imp.suppress_reconcile.set(false);
                return glib::ControlFlow::Break;
            }

            let current = sum_children_widths(&imp.children.borrow(), imp.gap.get()) as f64;
            let prev = imp.convergence_prev_width.get();

            if prev >= 0.0 && (current - prev).abs() < 1.0 {
                let frames = imp.convergence_stable_frames.get() + 1;
                if frames >= CONVERGENCE_STABLE_FRAMES {
                    // Converged — clear suppress and animate residual mismatch.
                    imp.suppress_reconcile.set(false);
                    let final_w = current.round() as i32;
                    wsc_ref.set_target_width(final_w);
                    return glib::ControlFlow::Break;
                }
                imp.convergence_stable_frames.set(frames);
            } else {
                imp.convergence_stable_frames.set(0);
            }

            imp.convergence_prev_width.set(current);
            glib::ControlFlow::Continue
        });
    }
}

/// Sum the current natural widths of `children` plus inter-child gaps.
///
/// Each child is measured horizontally to get its live CSS-transitioning
/// width, so the result tracks in-flight CSS transitions frame by frame.
///
/// Note: not unit-tested because `Widget::measure()` requires a GTK display
/// server.  The gap arithmetic is trivially `(n-1) * gap`; the function is
/// exercised end-to-end by the layout's `size_allocate` path.
fn sum_children_widths(children: &[Widget], gap: i32) -> i32 {
    let n = children.len();
    if n == 0 {
        return 0;
    }
    let mut total = 0i32;
    for child in children {
        let (_, cw, _, _) = child.measure(gtk4::Orientation::Horizontal, -1);
        total += cw;
    }
    total + (n as i32 - 1) * gap
}

/// Compute the left-group size for the two-group layout split.
///
/// Splits `n` children into a left group (laid out left-to-right) and a right
/// group (laid out right-to-left), with the gap between them absorbing
/// subpixel rounding drift during CSS transitions.
///
/// Rules:
/// - Active child is the last in the left group (split after it).
/// - If active is the last child, keep it in the left group but ensure the
///   right group has at least 1 child to anchor from the right edge.
/// - If no active child, fall back to a midpoint split.
fn compute_left_count(n: usize, active_idx: Option<usize>) -> usize {
    debug_assert!(n >= 2, "compute_left_count requires n >= 2, got {n}");
    match active_idx {
        Some(idx) if idx < n - 1 => idx + 1,
        Some(_) => n - 1,      // active is last: right group = [last]
        None => n.div_ceil(2), // fallback midpoint
    }
}

/// Label type for workspace indicators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelType {
    /// Show icon glyphs (●, ○, ◆).
    Icons,
    /// Show workspace numbers/names.
    Numbers,
    /// Minimal - no text, just CSS styling.
    None,
}

impl LabelType {
    fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "numbers" => LabelType::Numbers,
            "none" => LabelType::None,
            _ => LabelType::Icons,
        }
    }
}

const DEFAULT_LABEL_TYPE: LabelType = LabelType::None;
const DEFAULT_SEPARATOR: &str = "";

/// Multiplier for indicator height relative to widget_height.
/// Used in CSS generation (bar.rs) for min-height on all indicators.
pub(crate) const INDICATOR_HEIGHT_MULT: f64 = 0.7;
/// Multiplier for inactive indicator width relative to widget_height.
/// Used in CSS generation (bar.rs) for min-width on inactive indicators.
pub(crate) const INDICATOR_INACTIVE_MULT: f64 = 0.7;
/// Multiplier for active indicator width relative to widget_height.
/// Used in CSS generation (bar.rs) for min-width on active indicators.
pub(crate) const INDICATOR_ACTIVE_MULT: f64 = 1.0;

/// Configuration for the workspaces widget.
#[derive(Debug, Clone)]
pub struct WorkspacesConfig {
    /// How to display workspace labels.
    pub label_type: LabelType,
    /// Separator string between workspace indicators.
    pub separator: String,
    /// Whether to animate circle↔pill transitions (default: true).
    pub animate: bool,
}

impl WidgetConfig for WorkspacesConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options("workspaces", entry, &["label_type", "separator", "animate"]);

        let label_type = entry
            .options
            .get("label_type")
            .and_then(|v| v.as_str())
            .map(LabelType::from_str)
            .unwrap_or(DEFAULT_LABEL_TYPE);

        let separator = entry
            .options
            .get("separator")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_SEPARATOR)
            .to_string();

        let animate = entry
            .options
            .get("animate")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        Self {
            label_type,
            separator,
            animate,
        }
    }
}

impl Default for WorkspacesConfig {
    fn default() -> Self {
        Self {
            label_type: DEFAULT_LABEL_TYPE,
            separator: DEFAULT_SEPARATOR.to_string(),
            animate: true,
        }
    }
}

/// Workspaces widget that displays workspace indicators.
pub struct WorkspacesWidget {
    /// Shared base widget container.
    base: BaseWidget,
    /// Callback ID for WorkspaceService, used to disconnect on drop.
    workspace_callback_id: CallbackId,
}

impl WorkspacesWidget {
    /// Create a new workspaces widget with the given configuration.
    ///
    /// # Arguments
    ///
    /// * `config` - Widget configuration (label type, separator).
    /// * `output_id` - Optional output/monitor name. When set, the widget will:
    ///   - For Niri: only show workspaces belonging to this output.
    ///   - For MangoWC: show all workspaces but with per-output window counts.
    ///   - For Hyprland: ignored (global workspace view).
    pub fn new(config: WorkspacesConfig, output_id: Option<String>) -> Self {
        let base = BaseWidget::new(&[widget::WORKSPACES]);

        let label_type = config.label_type;
        let animate = config.animate;

        // Cache theme sizes at construction time. These values are derived
        // from bar.size/bar.padding, and any change to those triggers a full
        // bar rebuild (config_structure_changed → reconfigure_all), which
        // destroys and recreates this widget with fresh values.
        let sizes = ConfigManager::global().theme_sizes();
        let content_gap = sizes.widget_content_gap;

        // Animated mode uses WorkspaceContainer; otherwise indicators go in the GtkBox.
        let ws_container: Option<WorkspaceContainer> = if animate {
            let container = WorkspaceContainer::new();
            container.set_gap(content_gap as i32);
            base.content().append(&container);
            Some(container)
        } else {
            None
        };

        let content_box = base.content().clone();

        let workspace_labels: Rc<RefCell<HashMap<i32, Widget>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let current_ids = Rc::new(RefCell::new(Vec::new()));
        let separator = config.separator;

        let output_id_debug = output_id.clone();

        let workspace_callback_id = WorkspaceService::global().connect(move |snapshot| {
            update_indicators(
                &content_box,
                ws_container.as_ref(),
                &workspace_labels,
                &current_ids,
                label_type,
                &separator,
                snapshot,
                output_id.as_deref(),
            );
        });

        debug!(
            "WorkspacesWidget created (output_id: {:?})",
            output_id_debug
        );
        Self {
            base,
            workspace_callback_id,
        }
    }

    /// Get the root GTK widget for embedding in the bar.
    pub fn widget(&self) -> &GtkBox {
        self.base.widget()
    }
}

impl Drop for WorkspacesWidget {
    fn drop(&mut self) {
        WorkspaceService::global().disconnect(self.workspace_callback_id);
    }
}

/// Icon glyphs for workspace indicators.
const ICON_OCCUPIED: &str = "●";
const ICON_EMPTY: &str = "○";
const ICON_ACTIVE: &str = "◆";

/// Duration of the enter/exit animation for workspace indicators (in microseconds).
/// Matches the CSS transition duration on `.workspace-indicator`.
const INDICATOR_ANIM_DURATION_US: i64 = 200_000;

/// Number of consecutive frames where children width must be stable (delta < 1px)
/// before we consider CSS transitions settled. 3 frames guards against false
/// positives from non-linear easing functions (ease-in-out) that have
/// near-zero-velocity zones at inflection points.
const CONVERGENCE_STABLE_FRAMES: u32 = 3;

/// Safety cap for convergence detection (microseconds). If children widths
/// haven't stabilized after this long, clear suppress_reconcile anyway to
/// prevent stuck state. 2 seconds accommodates even extreme custom CSS
/// transition durations.
const CONVERGENCE_SAFETY_CAP_US: i64 = 2_000_000;

/// Clear all workspace indicator widgets from the container.
fn clear_indicators(
    container: &GtkBox,
    ws_container: Option<&WorkspaceContainer>,
    labels: &Rc<RefCell<HashMap<i32, Widget>>>,
    ids: &Rc<RefCell<Vec<i32>>>,
) {
    if let Some(wsc) = ws_container {
        wsc.clear_children();
    } else {
        while let Some(child) = container.first_child() {
            container.remove(&child);
        }
    }
    labels.borrow_mut().clear();
    ids.borrow_mut().clear();
}

/// Create a single workspace indicator widget.
fn create_single_indicator(label_type: LabelType, workspace: &Workspace) -> Widget {
    let workspace_id = workspace.id;
    let gesture = GestureClick::new();
    gesture.set_button(BUTTON_PRIMARY);
    gesture.connect_released(move |gesture, _n_press, _x, _y| {
        if gesture.current_button() != BUTTON_PRIMARY {
            return;
        }
        debug!("Switching to workspace {}", workspace_id);
        WorkspaceService::global().switch_workspace(workspace_id);
    });

    if label_type == LabelType::None {
        // GtkBox avoids font-metric intrinsic sizing; CSS controls dimensions.
        let dot = GtkBox::new(gtk4::Orientation::Horizontal, 0);
        dot.add_css_class(widget::WORKSPACE_INDICATOR);
        dot.add_css_class(widget::WORKSPACE_INDICATOR_MINIMAL);
        dot.add_css_class(state::CLICKABLE);
        dot.set_valign(Align::Center);
        dot.add_controller(gesture);
        dot.upcast()
    } else {
        let label_text = match label_type {
            LabelType::Icons => ICON_EMPTY,
            LabelType::Numbers => &workspace.name,
            LabelType::None => unreachable!(),
        };
        let label = Label::new(Some(label_text));
        label.add_css_class(widget::WORKSPACE_INDICATOR);
        label.add_css_class(state::CLICKABLE);
        label.set_valign(Align::Center);
        // Optical centering: glyphs ●/○/◆ appear left-heavy at 0.5;
        // 0.55 nudges them to look visually centered in the pill.
        label.set_xalign(0.55);
        label.set_ellipsize(EllipsizeMode::End);
        label.set_single_line_mode(true);
        label.add_controller(gesture);
        label.upcast()
    }
}

/// Create workspace indicator widgets for the given workspaces.
#[allow(clippy::too_many_arguments)]
fn create_indicators(
    container: &GtkBox,
    ws_container: Option<&WorkspaceContainer>,
    labels_cell: &Rc<RefCell<HashMap<i32, Widget>>>,
    ids_cell: &Rc<RefCell<Vec<i32>>>,
    label_type: LabelType,
    separator: &str,
    workspaces: &[Workspace],
) {
    clear_indicators(container, ws_container, labels_cell, ids_cell);

    let mut labels = labels_cell.borrow_mut();
    let mut ids = ids_cell.borrow_mut();

    for (i, workspace) in workspaces.iter().enumerate() {
        let indicator = create_single_indicator(label_type, workspace);

        labels.insert(workspace.id, indicator.clone());
        if let Some(wsc) = ws_container {
            wsc.add_child(&indicator);
        } else {
            container.append(&indicator);
        }
        ids.push(workspace.id);

        if ws_container.is_none() && i < workspaces.len() - 1 && !separator.is_empty() {
            let sep = Label::new(Some(separator));
            sep.set_valign(Align::Center);
            sep.add_css_class(widget::WORKSPACE_SEPARATOR);
            container.append(&sep);
        }
    }
}

/// Collect indicators that have the grow-in CSS class.
fn collect_grow_in_indicators(
    labels_cell: &Rc<RefCell<HashMap<i32, Widget>>>,
    new_ids: &[i32],
) -> Vec<Widget> {
    let labels = labels_cell.borrow();
    let mut grow_in = Vec::new();
    for &id in new_ids {
        if let Some(ind) = labels.get(&id)
            && ind.has_css_class(widget::WORKSPACE_GROW_IN)
        {
            grow_in.push(ind.clone());
        }
    }
    grow_in
}

/// Full recreate of indicators with grow-in CSS class on new IDs.
///
/// Shared by the Swap and Addition paths: calls `create_indicators` then
/// tags any indicator whose ID was not in `old_ids` with the grow-in class
/// so it animates from 0-width to its CSS-defined size.
#[allow(clippy::too_many_arguments)]
fn recreate_with_grow_in(
    container: &GtkBox,
    wsc: &WorkspaceContainer,
    labels_cell: &Rc<RefCell<HashMap<i32, Widget>>>,
    ids_cell: &Rc<RefCell<Vec<i32>>>,
    label_type: LabelType,
    separator: &str,
    display_workspaces: &[Workspace],
    old_ids: &HashSet<i32>,
    new_ids: &[i32],
) {
    create_indicators(
        container,
        Some(wsc),
        labels_cell,
        ids_cell,
        label_type,
        separator,
        display_workspaces,
    );
    if !old_ids.is_empty() {
        let labels = labels_cell.borrow();
        for &id in new_ids {
            if old_ids.contains(&id) {
                continue;
            }
            if let Some(indicator) = labels.get(&id) {
                indicator.add_css_class(widget::WORKSPACE_GROW_IN);
                indicator.add_css_class(widget::WORKSPACE_GROW_IN_NOTRANS);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ChangeType — classifies how the visible workspace set changed.
// ---------------------------------------------------------------------------

/// What kind of change occurred to the set of visible workspace IDs.
///
/// Used by `update_indicators` to decide which animation path to take.
/// Extracted as a module-level type so `classify_change()` can be tested
/// independently of GTK widget logic.
#[derive(Debug, PartialEq)]
enum ChangeType {
    /// No animation action needed (IDs unchanged, or non-minimal mode).
    None,
    /// IDs were removed but none added (minimal mode only).
    Removal,
    /// Same IDs in a different order (e.g., workspace reorder in Niri).
    Reorder,
    /// New workspace IDs appeared (minimal mode only). Also covers
    /// simultaneous add+remove (e.g., `[1,2,3] → [1,4]`): the full
    /// recreate replaces all indicators at once — removed workspaces
    /// don't get a removal animation since GTK4 can't animate on
    /// `unparent()` anyway.
    Addition,
    /// IDs changed but total count stayed the same (e.g., ws4→ws5).
    /// Full recreate with grow-in on new indicators. Container is
    /// pinned at pre-recreate width; convergence detection clears
    /// suppress_reconcile once CSS transitions settle.
    Swap,
}

/// Pure classification of how the visible workspace set changed.
///
/// Inputs:
/// - `ids_changed`: whether `new_ids != old_ids` (order-sensitive)
/// - `has_additions`: whether any ID in `new_ids` is not in `old_ids`
/// - `old_ids_empty`: whether the old ID set is empty
/// - `old_count`: number of old IDs
/// - `new_count`: number of new IDs
/// - `is_minimal`: whether a `WorkspaceContainer` is present (minimal mode)
///
/// This function has no side effects — the caller uses the result to decide
/// which animation/recreation path to take.
fn classify_change(
    ids_changed: bool,
    has_additions: bool,
    old_ids_empty: bool,
    old_count: usize,
    new_count: usize,
    is_minimal: bool,
) -> ChangeType {
    if !ids_changed {
        return ChangeType::None;
    }

    if !is_minimal {
        // Non-minimal: no animations.
        return ChangeType::None;
    }

    if !has_additions && !old_ids_empty {
        if old_count == new_count {
            // Same IDs, different order — pure reorder (e.g., user reordered in Niri).
            ChangeType::Reorder
        } else {
            ChangeType::Removal
        }
    } else if new_count == old_count {
        // Note: old_ids_empty with equal counts is unreachable (both would be 0).
        ChangeType::Swap
    } else {
        ChangeType::Addition
    }
}

/// Update workspace indicators based on the current snapshot.
///
/// When `output_id` is provided:
/// - Uses per-output workspace data if available.
/// - For Niri: only shows workspaces belonging to this output.
/// - For MangoWC: shows all workspaces with per-output window counts.
#[allow(clippy::too_many_arguments)]
fn update_indicators(
    container: &GtkBox,
    ws_container: Option<&WorkspaceContainer>,
    labels_cell: &Rc<RefCell<HashMap<i32, Widget>>>,
    ids_cell: &Rc<RefCell<Vec<i32>>>,
    label_type: LabelType,
    separator: &str,
    snapshot: &WorkspaceServiceSnapshot,
    output_id: Option<&str>,
) {
    let (workspaces, active_workspaces, source): (&[Workspace], &HashSet<i32>, &str) = if let Some(
        output,
    ) =
        output_id
    {
        if let Some(per_output) = snapshot.per_output.get(output) {
            (
                &per_output.workspaces,
                &per_output.active_workspace,
                "per_output",
            )
        } else {
            debug!(
                "workspace widget: output_id={:?} not found in per_output keys={:?}, using global",
                output,
                snapshot.per_output.keys().collect::<Vec<_>>()
            );
            (
                &snapshot.workspaces,
                &snapshot.active_workspace,
                "global_fallback",
            )
        }
    } else {
        (&snapshot.workspaces, &snapshot.active_workspace, "global")
    };

    trace!(
        "workspace widget: source={}, output_id={:?}, active_workspaces={:?}",
        source, output_id, active_workspaces
    );

    // Display occupied + active workspaces.
    let mut display_ids: std::collections::HashSet<i32> = workspaces
        .iter()
        .filter(|ws| ws.occupied)
        .map(|ws| ws.id)
        .collect();

    trace!(
        "workspace widget: occupied_ids={:?}, adding active={:?}",
        display_ids, active_workspaces
    );

    // Include active workspaces (supports multi-tag view).
    display_ids.extend(active_workspaces.iter());

    let display_workspaces: Vec<_> = workspaces
        .iter()
        .filter(|ws| display_ids.contains(&ws.id))
        .cloned()
        .collect();

    trace!(
        "workspace widget: display_ids={:?}, display_workspaces={:?}",
        display_ids,
        display_workspaces
            .iter()
            .map(|ws| (ws.id, ws.active, ws.occupied))
            .collect::<Vec<_>>()
    );

    if display_workspaces.is_empty() {
        let current_ids = ids_cell.borrow();
        if !current_ids.is_empty() {
            drop(current_ids);
            clear_indicators(container, ws_container, labels_cell, ids_cell);
        }
        if let Some(wsc) = ws_container {
            wsc.set_target_width(0);
        }
        return;
    }

    let new_ids: Vec<i32> = display_workspaces.iter().map(|ws| ws.id).collect();

    let ids_changed = new_ids != *ids_cell.borrow();

    let old_ids: HashSet<i32> = ids_cell.borrow().iter().copied().collect();
    let new_ids_set: HashSet<i32> = new_ids.iter().copied().collect();
    let has_additions = ids_changed && new_ids_set.iter().any(|id| !old_ids.contains(id));

    let change_type = classify_change(
        ids_changed,
        has_additions,
        old_ids.is_empty(),
        old_ids.len(),
        new_ids.len(),
        ws_container.is_some(),
    );

    // Saved before full-recreate in the additions/swap paths so the
    // container animation starts from the visually correct width (grow-in
    // indicators measure as 0, making post-recreate measurement unreliable).
    let mut pre_recreate_width: Option<f64> = None;

    if ids_changed {
        // Three paths: A. Removal-only, B. Addition/swap, C. Non-minimal (no animation).
        if let Some(wsc) = ws_container {
            match change_type {
                ChangeType::Removal => {
                    // ── Path A: Removal-only — surgically remove departed indicators. ──

                    let pre_removal_width = wsc.compute_children_current_width();

                    // Find the leftmost removed index BEFORE removing children.
                    // This determines the freeze split: everything to the left
                    // stays left-anchored, everything to the right stays
                    // right-anchored, and the gap closes where the removed
                    // workspace was.
                    let ids = ids_cell.borrow();
                    let leftmost_removed_idx = ids
                        .iter()
                        .position(|id| !new_ids_set.contains(id))
                        .unwrap_or(ids.len());
                    drop(ids);

                    let frozen = wsc.freeze_left_count_at(leftmost_removed_idx);

                    {
                        let mut labels = labels_cell.borrow_mut();
                        let ids = ids_cell.borrow();
                        let mut left_removed = 0usize;
                        for (i, &id) in ids.iter().enumerate() {
                            if new_ids_set.contains(&id) {
                                continue;
                            }
                            if i < frozen {
                                left_removed += 1;
                            }
                            if let Some(indicator) = labels.remove(&id) {
                                wsc.remove_child(&indicator);
                            }
                        }
                        drop(ids);
                        ids_cell.borrow_mut().retain(|id| new_ids_set.contains(id));
                        wsc.adjust_frozen_left_count(left_removed);
                    }

                    wsc.seed_current_width(pre_removal_width as f64);
                }
                ChangeType::Reorder => {
                    // ── Path A2: Reorder — same IDs, different order. ──
                    // Full recreate without grow-in; container width unchanged.
                    recreate_with_grow_in(
                        container,
                        wsc,
                        labels_cell,
                        ids_cell,
                        label_type,
                        separator,
                        &display_workspaces,
                        &old_ids,
                        &new_ids,
                    );
                }
                ChangeType::Swap => {
                    // ── Path B1: Swap — same count, different IDs. ──
                    pre_recreate_width = Some(wsc.imp().current_width.get());

                    recreate_with_grow_in(
                        container,
                        wsc,
                        labels_cell,
                        ids_cell,
                        label_type,
                        separator,
                        &display_workspaces,
                        &old_ids,
                        &new_ids,
                    );
                }
                ChangeType::Addition => {
                    // ── Path B2: Net additions ──
                    // Save pre-recreate width (grow-in indicators measure
                    // as 0, making post-recreate measurement unreliable).
                    pre_recreate_width = Some(wsc.imp().current_width.get());

                    recreate_with_grow_in(
                        container,
                        wsc,
                        labels_cell,
                        ids_cell,
                        label_type,
                        separator,
                        &display_workspaces,
                        &old_ids,
                        &new_ids,
                    );
                }
                ChangeType::None => {
                    // Unreachable in practice.
                    debug_assert!(
                        false,
                        "classify_change returned None with ids_changed=true and is_minimal=true"
                    );
                }
            }
        } else {
            // Non-minimal — full recreate.
            create_indicators(
                container,
                None,
                labels_cell,
                ids_cell,
                label_type,
                separator,
                &display_workspaces,
            );
        }
    }

    // ── Shared styling tail — update CSS before measuring (active changes min-width). ──
    let labels = labels_cell.borrow();
    for workspace in &display_workspaces {
        let Some(indicator) = labels.get(&workspace.id) else {
            continue;
        };

        // Only toggle classes that changed — remove+re-add of the same
        // class causes GTK's style system to return stale measure() values.
        let target_class: Option<&str> = if workspace.active {
            Some(widget::ACTIVE)
        } else if workspace.occupied {
            Some(state::OCCUPIED)
        } else if workspace.urgent {
            Some(state::URGENT)
        } else {
            None
        };

        // State classes are mutually exclusive — only toggle what changed.
        for &cls in &[widget::ACTIVE, state::OCCUPIED, state::URGENT] {
            if Some(cls) == target_class {
                if !indicator.has_css_class(cls) {
                    indicator.add_css_class(cls);
                }
            } else if indicator.has_css_class(cls) {
                indicator.remove_css_class(cls);
            }
        }

        // Update icon text (Icons/Numbers mode only).
        if let Some(label) = (label_type != LabelType::None)
            .then(|| indicator.downcast_ref::<Label>())
            .flatten()
        {
            match label_type {
                LabelType::Icons => {
                    if workspace.active {
                        label.set_text(ICON_ACTIVE);
                    } else if workspace.occupied {
                        label.set_text(ICON_OCCUPIED);
                    } else {
                        label.set_text(ICON_EMPTY);
                    }
                }
                LabelType::Numbers => label.set_text(&workspace.name),
                LabelType::None => unreachable!(),
            }
        }

        let tooltip_text = build_tooltip(workspace);
        TooltipManager::global().set_styled_tooltip(indicator, &tooltip_text);
    }
    drop(labels);

    // ── Container target width — may be stale; tick callback corrects per-frame. ──
    if let Some(wsc) = ws_container {
        match change_type {
            ChangeType::Addition => {
                let grow_in_indicators = collect_grow_in_indicators(labels_cell, &new_ids);

                debug_assert!(
                    pre_recreate_width.is_some(),
                    "Addition: pre_recreate_width should be set before recreate"
                );
                let seed = pre_recreate_width
                    .unwrap_or_else(|| wsc.compute_children_current_width() as f64);

                // Approximate target to start tick callback; accuracy doesn't
                // matter — tick tracks children_width while suppress_reconcile active.
                let wh = ConfigManager::global().theme_sizes().widget_height as f64;
                let gap = wsc.imp().gap.get();
                let added_width: f64 = display_workspaces
                    .iter()
                    .filter(|ws| !old_ids.contains(&ws.id))
                    .map(|ws| {
                        let mult = if ws.active {
                            INDICATOR_ACTIVE_MULT
                        } else {
                            INDICATOR_INACTIVE_MULT
                        };
                        (wh * mult).floor() + gap as f64
                    })
                    .sum();
                let initial_target = (seed + added_width).round() as i32;

                wsc.begin_grow_in_animation(seed, initial_target, grow_in_indicators);
            }
            ChangeType::Removal => {
                // Target may be stale; tick callback corrects via
                // live target correction + floor-clamp.
                let target = wsc.compute_children_current_width();
                wsc.set_target_width(target);
            }
            ChangeType::Reorder => {
                // Same count, just reordered — snap to current width.
                let target = wsc.compute_children_current_width();
                wsc.set_target_width(target);
            }
            ChangeType::Swap => {
                // Pin at pre-recreate width; reconciliation animates after CSS settles.
                let grow_in_indicators = collect_grow_in_indicators(labels_cell, &new_ids);

                debug_assert!(
                    pre_recreate_width.is_some(),
                    "Swap: pre_recreate_width should be set before recreate"
                );
                let seed = pre_recreate_width
                    .unwrap_or_else(|| wsc.compute_children_current_width() as f64);

                wsc.begin_grow_in_animation(seed, seed.round() as i32, grow_in_indicators);
            }
            ChangeType::None => {
                // Switch-only — skip if suppress_reconcile active (prior grow-in resolving).
                if !wsc.imp().suppress_reconcile.get() {
                    let target = wsc.compute_children_current_width();
                    wsc.set_target_width(target);
                }
            }
        }
    }
}

/// Build tooltip text for a workspace.
fn build_tooltip(workspace: &Workspace) -> String {
    let mut parts = Vec::new();

    // Niri can have custom workspace names separate from the index.
    let idx_str = workspace.idx.to_string();
    if workspace.name != idx_str {
        parts.push(format!("Workspace {}: {}", workspace.idx, workspace.name));
    } else {
        parts.push(format!("Workspace {}", workspace.name));
    }

    if workspace.active {
        parts.push("Active".to_string());
    } else if workspace.urgent {
        parts.push("Urgent".to_string());
    }

    if let Some(count) = workspace.window_count {
        let windows_str = if count == 1 { "window" } else { "windows" };
        parts.push(format!("{} {}", count, windows_str));
    } else if workspace.occupied {
        parts.push("Has windows".to_string());
    } else {
        parts.push("Empty".to_string());
    }

    parts.join(" • ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use toml::Value;

    fn make_widget_entry(name: &str, options: HashMap<String, Value>) -> WidgetEntry {
        WidgetEntry {
            name: name.to_string(),
            options,
        }
    }

    #[test]
    fn test_workspace_config_default() {
        let entry = make_widget_entry("workspaces", HashMap::new());
        let config = WorkspacesConfig::from_entry(&entry);
        assert_eq!(config.label_type, LabelType::None);
        assert_eq!(config.separator, "");
    }

    #[test]
    fn test_workspace_config_numbers() {
        let mut options = HashMap::new();
        options.insert(
            "label_type".to_string(),
            Value::String("numbers".to_string()),
        );
        options.insert("separator".to_string(), Value::String("|".to_string()));
        let entry = make_widget_entry("workspaces", options);
        let config = WorkspacesConfig::from_entry(&entry);
        assert_eq!(config.label_type, LabelType::Numbers);
        assert_eq!(config.separator, "|");
    }

    #[test]
    fn test_workspace_config_none() {
        let mut options = HashMap::new();
        options.insert("label_type".to_string(), Value::String("none".to_string()));
        let entry = make_widget_entry("workspaces", options);
        let config = WorkspacesConfig::from_entry(&entry);
        assert_eq!(config.label_type, LabelType::None);
    }

    #[test]
    fn test_label_type_from_str() {
        assert_eq!(LabelType::from_str("icons"), LabelType::Icons);
        assert_eq!(LabelType::from_str("ICONS"), LabelType::Icons);
        assert_eq!(LabelType::from_str("numbers"), LabelType::Numbers);
        assert_eq!(LabelType::from_str("none"), LabelType::None);
        assert_eq!(LabelType::from_str("unknown"), LabelType::Icons); // default
    }

    #[test]
    fn test_workspace_config_animate_default() {
        let entry = make_widget_entry("workspaces", HashMap::new());
        let config = WorkspacesConfig::from_entry(&entry);
        assert!(config.animate);
    }

    #[test]
    fn test_workspace_config_animate_disabled() {
        let mut options = HashMap::new();
        options.insert("animate".to_string(), Value::Boolean(false));
        let entry = make_widget_entry("workspaces", options);
        let config = WorkspacesConfig::from_entry(&entry);
        assert!(!config.animate);
    }

    // -- compute_left_count tests --

    #[test]
    fn test_compute_left_count_active_first() {
        // n=5, active=0 → left=[0], right=[1,2,3,4]
        assert_eq!(compute_left_count(5, Some(0)), 1);
    }

    #[test]
    fn test_compute_left_count_active_middle() {
        // n=5, active=2 → left=[0,1,2], right=[3,4]
        assert_eq!(compute_left_count(5, Some(2)), 3);
    }

    #[test]
    fn test_compute_left_count_active_second_to_last() {
        // n=5, active=3 → left=[0,1,2,3], right=[4]
        assert_eq!(compute_left_count(5, Some(3)), 4);
    }

    #[test]
    fn test_compute_left_count_active_last() {
        // n=5, active=4 → left=[0,1,2,3], right=[4] (right-anchored)
        assert_eq!(compute_left_count(5, Some(4)), 4);
    }

    #[test]
    fn test_compute_left_count_no_active() {
        // n=5, no active → midpoint split: left=[0,1,2], right=[3,4]
        assert_eq!(compute_left_count(5, None), 3);
    }

    #[test]
    fn test_compute_left_count_no_active_even() {
        // n=4, no active → midpoint split: left=[0,1], right=[2,3]
        assert_eq!(compute_left_count(4, None), 2);
    }

    #[test]
    fn test_compute_left_count_two_active_first() {
        // n=2, active=0 → left=[0], right=[1]
        assert_eq!(compute_left_count(2, Some(0)), 1);
    }

    #[test]
    fn test_compute_left_count_two_active_last() {
        // n=2, active=1 → left=[0], right=[1] (active-is-last rule)
        assert_eq!(compute_left_count(2, Some(1)), 1);
    }

    // -- build_tooltip tests --

    fn make_workspace(
        id: i32,
        name: &str,
        active: bool,
        occupied: bool,
        urgent: bool,
        window_count: Option<u32>,
    ) -> Workspace {
        Workspace {
            id,
            idx: id,
            name: name.to_string(),
            active,
            occupied,
            urgent,
            window_count,
            output: None,
        }
    }

    #[test]
    fn test_build_tooltip_active_with_windows() {
        let ws = make_workspace(1, "1", true, true, false, Some(3));
        assert_eq!(build_tooltip(&ws), "Workspace 1 • Active • 3 windows");
    }

    #[test]
    fn test_build_tooltip_active_single_window() {
        let ws = make_workspace(2, "2", true, true, false, Some(1));
        assert_eq!(build_tooltip(&ws), "Workspace 2 • Active • 1 window");
    }

    #[test]
    fn test_build_tooltip_inactive_empty() {
        let ws = make_workspace(3, "3", false, false, false, None);
        assert_eq!(build_tooltip(&ws), "Workspace 3 • Empty");
    }

    #[test]
    fn test_build_tooltip_occupied_no_count() {
        let ws = make_workspace(4, "4", false, true, false, None);
        assert_eq!(build_tooltip(&ws), "Workspace 4 • Has windows");
    }

    #[test]
    fn test_build_tooltip_urgent() {
        let ws = make_workspace(5, "5", false, true, true, Some(2));
        assert_eq!(build_tooltip(&ws), "Workspace 5 • Urgent • 2 windows");
    }

    #[test]
    fn test_build_tooltip_custom_name() {
        let ws = make_workspace(1, "browser", true, true, false, Some(5));
        assert_eq!(
            build_tooltip(&ws),
            "Workspace 1: browser • Active • 5 windows"
        );
    }

    // -- classify_change tests --

    #[test]
    fn test_classify_no_change() {
        // IDs didn't change at all.
        assert_eq!(
            classify_change(false, false, false, 3, 3, true),
            ChangeType::None
        );
    }

    #[test]
    fn test_classify_no_change_non_minimal() {
        // IDs didn't change, non-minimal mode.
        assert_eq!(
            classify_change(false, false, false, 3, 3, false),
            ChangeType::None
        );
    }

    #[test]
    fn test_classify_removal_only() {
        // IDs changed, no additions, old not empty, count decreased, minimal mode.
        // e.g., [1,2,3] → [1,2] — workspace 3 removed.
        assert_eq!(
            classify_change(true, false, false, 3, 2, true),
            ChangeType::Removal
        );
    }

    #[test]
    fn test_classify_reorder() {
        // IDs changed (different order), no additions, same count, minimal mode.
        // e.g., [1,6,2,15] → [1,2,6,15] — user reordered workspaces.
        assert_eq!(
            classify_change(true, false, false, 4, 4, true),
            ChangeType::Reorder
        );
    }

    #[test]
    fn test_classify_swap() {
        // IDs changed, has additions, same count, minimal mode.
        // e.g., [1,2,3] → [1,2,4] — workspace 3→4 swap.
        assert_eq!(
            classify_change(true, true, false, 3, 3, true),
            ChangeType::Swap
        );
    }

    #[test]
    fn test_classify_addition() {
        // IDs changed, has additions, count increased, minimal mode.
        // e.g., [1,2] → [1,2,3] — workspace 3 added.
        assert_eq!(
            classify_change(true, true, false, 2, 3, true),
            ChangeType::Addition
        );
    }

    #[test]
    fn test_classify_non_minimal_returns_none() {
        // Non-minimal mode always returns None (no animations),
        // even when IDs changed with additions.
        assert_eq!(
            classify_change(true, true, false, 2, 3, false),
            ChangeType::None
        );
    }

    #[test]
    fn test_classify_initial_population() {
        // Old IDs empty, new IDs populated, minimal mode.
        // has_additions=true (all new), old_count=0, new_count=3.
        // This is a net addition (first workspace creation).
        assert_eq!(
            classify_change(true, true, true, 0, 3, true),
            ChangeType::Addition
        );
    }

    #[test]
    fn test_classify_removal_not_triggered_with_additions() {
        // Even if count decreases, if there are additions it's not
        // a pure removal. e.g., [1,2,3] → [1,4] — ws 2,3 removed,
        // ws 4 added. has_additions=true, so it falls through to
        // Addition (count decreased: 3→2).
        assert_eq!(
            classify_change(true, true, false, 3, 2, true),
            ChangeType::Addition
        );
    }
}
