//! Workspaces widget — displays workspace indicators with animated transitions.
//!
//! Shows occupied/active workspaces as visual indicators (dots/pills/labels).
//! Clicking an indicator switches to that workspace.
//!
//! # Configuration
//!
//! - `label_type`: `"none"` (minimal dots, default), `"icons"` (●/○/◆ glyphs),
//!   `"name"` (workspace names; legacy alias: `"numbers"`), or `"index"`
//!   (meaningful numeric index, falling back to name when none exists).
//! - `separator`: string between indicators (non-animated mode only).
//! - `animate`: `true` (default) lays indicators out in a [`WorkspaceContainer`]
//!   that animates size changes; `false` uses a plain GtkBox with no animation.
//! - `filter_by_output`: `true` (default) uses this bar output's per-output
//!   workspace state; `false` shows global/all-output workspace state, including
//!   each output's current workspace. In all-output mode, active styling still
//!   follows the compositor's globally focused workspace.
//! - `show_windows`: `false` (default). When `true`, each indicator also shows
//!   the app icons of its workspace's windows. Clicking an icon focuses that
//!   window; clicking elsewhere on the indicator switches workspace. Needs a
//!   backend that reports windows (Niri, MangoWC, Sway, Hyprland); otherwise ignored. Always
//!   shows a label: `"none"` and `"icons"` fall back to `"index"`.
//! - `max_windows`: icons per workspace, `3` (default); `0` = unlimited. The
//!   rest are summarized as "+N".
//! - `group_by_app`: `false` (default). When `true`, windows of the same app
//!   share one icon; clicking it cycles through them.
//! - `icon_size`: window icon size in pixels. The default matches the label
//!   (about half of `bar.size`); larger sizes make every pill taller and are
//!   capped to fit the bar.
//!
//! # Animation
//!
//! CSS decides indicator *sizes* (no size transitions); [`WorkspaceContainer`]
//! decides *timing*. Every indicator has an animated flow-axis extent that one
//! tick callback moves (linearly) from its previous value towards the size CSS
//! currently measures. Any target change restarts all extents on a shared clock, so a
//! switch (one grows, one shrinks by the same amount) keeps the total constant
//! and neighbouring bar widgets never jitter. Positions are rounded from the
//! cumulative sum ([`slot_layout`]), so per-child rounding never leaks into
//! the total.
//!
//! Added workspaces grow out of the middle of their gap. Removed ones are
//! hidden immediately and their empty slot closes (quadratic ease-out), then they
//! are unparented. CSS hot-reloads are picked up because targets are re-measured
//! on every flow-axis `measure()`.
//!
//! With `"index"` or `"name"` labels, the active background is a separate
//! highlight drawn below the indicators that slides to the active workspace,
//! stretching on the way ([`highlight_edges`]). Dots and glyphs keep the
//! growing active pill as their only motion.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gtk4::gdk::BUTTON_PRIMARY;
use gtk4::glib;
use gtk4::pango::EllipsizeMode;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{
    Align, ApplicationWindow, Box as GtkBox, CssProvider, EventControllerScroll,
    EventControllerScrollFlags, GestureClick, Image, Label, Orientation, Widget,
};
use gtk4_layer_shell::{KeyboardMode, LayerShell};
use tr::tr;
use tracing::{debug, trace, warn};
use vibepanel_core::config::WidgetEntry;

use crate::services::callbacks::CallbackId;
use crate::services::compositor::{CompositorManager, Window};
use crate::services::config_manager::ConfigManager;
use crate::services::tooltip::{hide_tooltip, set_tooltip};
use crate::services::window_list::WindowListService;
use crate::services::workspace::{Workspace, WorkspaceService, WorkspaceServiceSnapshot};
use crate::styles::{state, widget};
use crate::widgets::WidgetConfig;
use crate::widgets::base::BaseWidget;
use crate::widgets::ripple::{trigger_ripple_from_gesture, wrap_with_ripple};
use crate::widgets::warn_unknown_options;
use crate::widgets::window_button::{
    app_display_name, app_icon, app_icon_name, connect_focus_on_click, sync_exclusive_class,
    window_tooltip,
};

// ---------------------------------------------------------------------------
// WorkspaceContainer — lays indicators out along the flow axis using animated
// extents (see module docs).
// ---------------------------------------------------------------------------

mod ws_container_imp {
    use super::*;

    pub(super) struct Entry {
        pub(super) id: i32,
        pub(super) widget: Widget,
        /// Workspace is gone; extent animates to 0, then the widget is unparented.
        pub(super) leaving: bool,
        /// Last measured natural flow-axis size.
        pub(super) nat: i32,
        pub(super) from: f64,
        pub(super) to: f64,
        pub(super) cur: f64,
        /// Growing in from 0: the visible box scales with the extent while
        /// the CSS margin (gap) stays whole. Cleared when the animation ends.
        pub(super) entering: bool,
    }

    #[derive(Default)]
    pub struct WorkspaceContainer {
        pub(super) entries: RefCell<Vec<Entry>>,
        /// Frame time of the current animation's first tick.
        pub(super) anim_start: Cell<Option<i64>>,
        pub(super) ticking: Cell<bool>,
        /// Set by the first non-empty measure; until then indicators take
        /// their measured size without animating (initial population).
        pub(super) laid_out: Cell<bool>,
        /// CSS gap (trailing margin every indicator carries), learned in
        /// size_allocate. The last one's margin hangs past the end, so the
        /// container's extent is `sum - gap` and the overflow is clipped.
        pub(super) gap: Cell<i32>,
        /// Per-instance CSS provider for long-indicator min-width rules.
        /// Display-scoped CSS is safe because workspace IDs are globally
        /// unique across all outputs.
        pub(super) long_mw_css_provider: RefCell<Option<CssProvider>>,
        /// True when the bar is on a side edge; the flow axis is then vertical.
        pub(super) axis_vertical: Cell<bool>,
        /// Pointer position while it's over the container.
        pub(super) pointer: Cell<Option<(f64, f64)>>,
        /// Sliding active-workspace highlight (index/name labels).
        pub(super) highlight: RefCell<Option<Highlight>>,
    }

    pub(super) struct Highlight {
        pub(super) widget: Widget,
        /// Workspace whose indicator the highlight covers.
        pub(super) target: Option<i32>,
        /// Flow-axis (start, end) where the current slide began; `None` once
        /// settled, when the highlight follows its target exactly.
        pub(super) from: Option<(f64, f64)>,
        /// Frame time when the current slide began.
        pub(super) start: i64,
        /// Extra slide duration for the current distance (set on its first
        /// layout, once the target position is known).
        pub(super) extra_us: Option<f64>,
        /// Last allocated flow-axis (start, end), where a new slide starts.
        pub(super) cur: Option<(f64, f64)>,
        /// Target's flow-axis (start, end) at the last allocation.
        pub(super) to: Option<(f64, f64)>,
        pub(super) ticking: bool,
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
            let obj = self.obj();
            obj.set_overflow(gtk4::Overflow::Hidden);

            let motion = gtk4::EventControllerMotion::new();
            let track = |motion: &gtk4::EventControllerMotion, pointer| {
                let Some(wsc) = motion.widget().and_downcast::<super::WorkspaceContainer>() else {
                    return;
                };
                wsc.imp().pointer.set(pointer);
                wsc.sync_highlight_hover();
            };
            motion.connect_enter(move |m, x, y| track(m, Some((x, y))));
            motion.connect_motion(move |m, x, y| track(m, Some((x, y))));
            motion.connect_leave(move |m| track(m, None));
            obj.add_controller(motion);
        }

        fn dispose(&self) {
            self.obj().remove_long_mw_provider();
            for e in self.entries.borrow_mut().drain(..) {
                e.widget.unparent();
            }
            if let Some(highlight) = self.highlight.borrow_mut().take() {
                highlight.widget.unparent();
            }
        }
    }

    impl WidgetImpl for WorkspaceContainer {
        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            // The highlight is drawn below every indicator.
            let obj = self.obj();
            if let Some(highlight) = self.highlight.borrow().as_ref() {
                obj.snapshot_child(&highlight.widget, snapshot);
            }
            for e in self.entries.borrow().iter() {
                obj.snapshot_child(&e.widget, snapshot);
            }
        }

        fn request_mode(&self) -> gtk4::SizeRequestMode {
            gtk4::SizeRequestMode::ConstantSize
        }

        fn measure(&self, orientation: gtk4::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let obj = self.obj();
            if orientation == obj.flow_orientation() {
                obj.sync_targets();
                let sum: f64 = self.entries.borrow().iter().map(|e| e.cur).sum();
                let w = (sum.round() as i32 - self.gap.get()).max(0);
                (w, w, -1, -1)
            } else {
                let entries = self.entries.borrow();
                entries.iter().fold((0, 0, -1, -1), |(min, nat, _, _), e| {
                    let (cmin, cnat, _, _) = e.widget.measure(orientation, -1);
                    (min.max(cmin), nat.max(cnat), -1, -1)
                })
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            let vertical = self.axis_vertical.get();
            let (flow_size, cross_size) = if vertical {
                (height, width)
            } else {
                (width, height)
            };
            let cross_orient = if vertical {
                gtk4::Orientation::Horizontal
            } else {
                gtk4::Orientation::Vertical
            };
            let obj = self.obj();
            // Children fill both axes (see set_children), so GTK never
            // re-measures them for an allocation below their CSS min-size.
            // Cross-axis centering is done here instead.
            let allocate = |child: &Widget, size: i32, pos: i32| {
                let cross = child.measure(cross_orient, -1).1.min(cross_size);
                let cpos = (cross_size - cross) / 2;
                let (x, y, w, h) = if vertical {
                    (cpos, pos, cross, size)
                } else {
                    (pos, cpos, size, cross)
                };
                let point = gtk4::graphene::Point::new(x as f32, y as f32);
                child.allocate(
                    w,
                    h,
                    baseline,
                    Some(gtk4::gsk::Transform::new().translate(&point)),
                );
            };
            // Flow-axis end of a rect (container coordinates).
            let flow_end = |r: gtk4::graphene::Rect| {
                if vertical {
                    r.y() + r.height()
                } else {
                    r.x() + r.width()
                }
            };

            let entries = self.entries.borrow();
            let slots = slot_layout(entries.iter().map(|e| e.cur));
            let total = slots.last().map_or(0, |(pos, size)| pos + size);
            let gap = self.gap.get();
            let offset = (flow_size - (total - gap).max(0)) / 2;
            let mut last_gap = None;
            // Border-box end of the previous visible indicator, so a growing
            // one is never pushed over it (that one may be growing too).
            let mut prev_end = offset;

            for (e, (pos, size)) in entries.iter().zip(slots) {
                let mut pos = pos + offset;
                let mut alloc = size;
                if size < e.nat {
                    // Below natural size. Allocating under CSS
                    // margin+border+padding would give the overlay a negative
                    // content box, so probe at natural size to learn that
                    // floor and the trailing margin (gap).
                    allocate(&e.widget, e.nat, pos);
                    let content = if vertical {
                        e.widget.height()
                    } else {
                        e.widget.width()
                    };
                    let floor = e.nat - content;
                    alloc = if e.entering {
                        // Scale the box with the slot and center it in the
                        // gap it opens: it grows out of the middle;
                        // neighbours resizing alongside shift it towards them.
                        // The last one's gap hangs past the end, so keep the
                        // box inside the container, but never over the
                        // previous indicator (max wins if the bounds cross).
                        // A box can't be narrower than its border+padding, so
                        // padded pills may still clip for the first frames.
                        let frac = size as f64 / e.nat as f64;
                        let lead = ((gap as f64 * (1.0 - frac)) / 2.0).round() as i32;
                        let a = floor + (frac * (e.nat - floor) as f64).round() as i32;
                        pos = (pos - lead).min(offset + total - a).max(prev_end);
                        a
                    } else {
                        size.max(floor)
                    };
                }
                allocate(&e.widget, alloc, pos);
                if !e.leaving && alloc > 0 {
                    // The last live indicator's trailing margin is what hangs
                    // past the end: allocation end minus border-box end.
                    if let Some(r) = e.widget.compute_bounds(&*obj) {
                        let end = flow_end(r).round() as i32;
                        last_gap = Some(pos + alloc - end);
                        prev_end = end;
                    }
                }
            }
            drop(entries);
            if let Some(g) = last_gap
                && self.gap.replace(g) != g
            {
                // Sizes include the gap, so animating them would squish every
                // pill by the gap delta while the new margin applies at once.
                // Snap to the new sizes instead (spacing edits are instant).
                for e in self.entries.borrow_mut().iter_mut().filter(|e| !e.leaving) {
                    let n = e.nat as f64;
                    (e.from, e.to, e.cur) = (n, n, n);
                }
                // queue_resize() is ignored inside our own size_allocate.
                let weak = obj.downgrade();
                glib::idle_add_local_once(move || {
                    if let Some(w) = weak.upgrade() {
                        w.queue_resize();
                    }
                });
            }
            obj.allocate_highlight(baseline);
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

    fn flow_orientation(&self) -> gtk4::Orientation {
        if self.imp().axis_vertical.get() {
            gtk4::Orientation::Vertical
        } else {
            gtk4::Orientation::Horizontal
        }
    }

    fn set_axis_vertical(&self, vertical: bool) {
        self.imp().axis_vertical.set(vertical);
    }

    fn anim_duration_us(&self) -> f64 {
        if self.imp().axis_vertical.get() {
            SWITCH_DURATION_VERTICAL_US
        } else {
            SWITCH_DURATION_US
        }
    }

    /// Replace the displayed indicators. Indicators keep their identity (and
    /// animated extent) by workspace ID; new IDs grow in, missing IDs vanish
    /// and their slot closes in place.
    fn set_children(&self, children: Vec<(i32, Widget)>) {
        let imp = self.imp();
        let mut entries = imp.entries.borrow_mut();
        // Snap (no animation) until the first layout, so snapshots arriving
        // before it don't grow in at startup.
        let snap = !imp.laid_out.get();
        let old_ids: Vec<i32> = entries.iter().map(|e| e.id).collect();
        let new_ids: Vec<i32> = children.iter().map(|(id, _)| *id).collect();
        let mut pool: HashMap<i32, ws_container_imp::Entry> =
            entries.drain(..).map(|e| (e.id, e)).collect();
        let mut fresh: HashMap<i32, Widget> = children.into_iter().collect();

        for id in merge_ids(&old_ids, &new_ids) {
            let widget = fresh.remove(&id);
            if let Some(w) = &widget {
                // Fill both axes: GTK clamps a centered widget to its natural
                // size (a shrinking indicator would snap) and re-measures it
                // against the allocation (warns below CSS min-size).
                // size_allocate centers on the cross axis itself.
                w.set_halign(Align::Fill);
                w.set_valign(Align::Fill);
            }
            let entry = match (pool.remove(&id), widget) {
                (Some(mut e), Some(w)) => {
                    if e.widget != w {
                        // Re-added while its gap closes: continue from its extent.
                        e.widget.unparent();
                        e.widget = w;
                    }
                    e.widget.set_visible(true);
                    if e.leaving {
                        e.leaving = false;
                        e.entering = true;
                    }
                    e
                }
                // Removed before the first layout: nothing to animate, and
                // no tick would ever clean it up.
                (Some(e), None) if snap => {
                    e.widget.unparent();
                    continue;
                }
                (Some(mut e), None) => {
                    // Vanish immediately; the empty slot then closes. Hidden
                    // widgets also get no clicks/tooltips in the gap.
                    e.leaving = true;
                    e.entering = false;
                    e.widget.set_visible(false);
                    e
                }
                (None, Some(w)) => ws_container_imp::Entry {
                    id,
                    widget: w,
                    leaving: false,
                    nat: 0,
                    from: 0.0,
                    to: 0.0,
                    cur: 0.0,
                    entering: !snap,
                },
                // Duplicate ID from a backend: already placed, skip it.
                (None, None) => continue,
            };
            entries.push(entry);
        }

        // Keep GTK sibling order in sync (parents new widgets too).
        let mut next: Option<Widget> = None;
        for e in entries.iter().rev() {
            e.widget.insert_before(self, next.as_ref());
            next = Some(e.widget.clone());
        }

        drop(entries);
        // Tick runs before layout re-measures targets; restart now so the
        // in-flight animation can't finish (and drop) entries changed here.
        if !snap {
            self.restart_animation();
        }
        self.queue_resize();
    }

    /// Re-measure every indicator's natural size; if any target moved,
    /// restart the animation from the current extents.
    fn sync_targets(&self) {
        let imp = self.imp();
        let flow = self.flow_orientation();
        let mut entries = imp.entries.borrow_mut();
        // An empty measure at startup must not count as the first layout.
        if entries.is_empty() {
            return;
        }
        let snap = !imp.laid_out.replace(true);
        let mut retarget = false;
        for e in entries.iter_mut() {
            e.nat = e.widget.measure(flow, -1).1;
            // A retarget below the current extent (e.g. switched away
            // mid-grow) means it has fully grown in; otherwise a later grow
            // would reuse the grow-in geometry. A zero-size one never has.
            if e.entering && e.nat > 0 && e.cur >= e.nat as f64 {
                e.entering = false;
            }
            let to = if e.leaving { 0.0 } else { e.nat as f64 };
            if snap {
                (e.from, e.to, e.cur) = (to, to, to);
            } else if e.to != to {
                e.to = to;
                retarget = true;
            }
        }
        drop(entries);
        if retarget {
            self.restart_animation();
        }
    }

    /// One shared clock for all extents: restarting everyone together keeps
    /// equal-and-opposite changes (switches, swaps) summing to a constant.
    fn restart_animation(&self) {
        let imp = self.imp();
        for e in imp.entries.borrow_mut().iter_mut() {
            e.from = e.cur;
        }
        imp.anim_start.set(None);
        if imp.ticking.replace(true) {
            return;
        }
        // TickCallbackId is not stored: GTK keeps tick callbacks across
        // unrealize and drops them on dispose; `ticking` relies on that.
        self.add_tick_callback(|wsc, clock| {
            let imp = wsc.imp();
            let now = clock.frame_time();
            // First tick already shows one frame of progress, so a restart
            // from a retarget doesn't sit still for a frame.
            let start = imp.anim_start.get().unwrap_or_else(|| {
                let interval = clock.refresh_info(now).0;
                now - if interval > 0 { interval } else { 16_667 }
            });
            imp.anim_start.set(Some(start));
            // Switches/additions move linearly. While a slot closes, everything
            // eases out together over REMOVAL_DURATION_US so growing
            // neighbours can't lag the closing gap (no overshoot). One curve
            // for all entries keeps paired changes summing to a constant.
            let mut entries = imp.entries.borrow_mut();
            let removing = entries.iter().any(|e| e.leaving);
            let duration = if removing {
                REMOVAL_DURATION_US
            } else {
                wsc.anim_duration_us()
            };
            let t = ((now - start) as f64 / duration).min(1.0);
            let k = if removing { ease_out_quad(t) } else { t };
            for e in entries.iter_mut() {
                e.cur = e.from + (e.to - e.from) * k;
            }
            drop(entries);
            wsc.queue_resize();
            if t < 1.0 {
                return glib::ControlFlow::Continue;
            }
            imp.entries.borrow_mut().retain_mut(|e| {
                e.entering = false;
                if e.leaving {
                    e.widget.unparent();
                }
                !e.leaving
            });
            imp.ticking.set(false);
            glib::ControlFlow::Break
        });
    }

    /// Move the active-workspace highlight to the indicator of workspace
    /// `target`, sliding when it was already showing. `None` hides it; active
    /// indicators then paint their own background.
    fn set_highlight(&self, target: Option<i32>) {
        let imp = self.imp();
        let mut highlight = imp.highlight.borrow_mut();
        let h = highlight.get_or_insert_with(|| {
            let pill = GtkBox::new(Orientation::Horizontal, 0);
            pill.add_css_class(widget::WORKSPACE_HIGHLIGHT);
            pill.set_can_target(false);
            pill.set_child_visible(false);
            pill.set_parent(self);
            ws_container_imp::Highlight {
                widget: pill.upcast(),
                target: None,
                from: None,
                start: 0,
                extra_us: None,
                cur: None,
                to: None,
                ticking: false,
            }
        });
        if h.target == target {
            return;
        }
        h.target = target;
        match (h.cur, target, self.frame_clock()) {
            (Some(cur), Some(_), Some(clock)) => {
                h.from = Some(cur);
                h.start = clock.frame_time();
                h.extra_us = None;
            }
            // Appearing (or without a frame clock yet): snap into place.
            _ => {
                h.from = None;
                h.cur = None;
            }
        }
        h.widget.set_child_visible(target.is_some());
        let sliding = h.from.is_some();
        drop(highlight);
        if target.is_none() {
            self.sync_under_highlight(None);
        }

        sync_exclusive_class(
            self.upcast_ref(),
            &[widget::WORKSPACE_HAS_HIGHLIGHT],
            target.is_some().then_some(widget::WORKSPACE_HAS_HIGHLIGHT),
        );
        self.queue_allocate();
        if sliding {
            self.ensure_highlight_tick();
        }
        self.sync_highlight_hover();
    }

    /// Give the highlight the active hover color while the pointer is over
    /// its workspace. An opaque hover background on the indicator itself
    /// would hide the highlight sliding in under the pointer.
    fn sync_highlight_hover(&self) {
        let imp = self.imp();
        let highlight = imp.highlight.borrow();
        let Some(h) = highlight.as_ref() else {
            return;
        };
        let hovered = h.target.zip(imp.pointer.get()).is_some_and(|(id, (x, y))| {
            imp.entries
                .borrow()
                .iter()
                .find(|e| e.id == id && !e.leaving)
                .and_then(|e| e.widget.compute_bounds(self))
                .is_some_and(|r| r.contains_point(&gtk4::graphene::Point::new(x as f32, y as f32)))
        });
        sync_exclusive_class(
            &h.widget,
            &[state::HOVERED],
            hovered.then_some(state::HOVERED),
        );
    }

    /// Allocate the highlight over its target indicator (or on its way there).
    /// Runs after the indicators are allocated, so it follows them while
    /// their own extents animate.
    fn allocate_highlight(&self, baseline: i32) {
        let imp = self.imp();
        let mut highlight = imp.highlight.borrow_mut();
        let Some(h) = highlight.as_mut() else {
            return;
        };
        let Some(id) = h.target else {
            return;
        };
        // GTK expects a measure before every allocate; the size comes from
        // the target indicator instead.
        h.widget.measure(Orientation::Horizontal, -1);
        let bounds = imp
            .entries
            .borrow()
            .iter()
            .find(|e| e.id == id && !e.leaving)
            .and_then(|e| e.widget.compute_bounds(self));
        let Some(r) = bounds else {
            h.widget.allocate(0, 0, baseline, None);
            return;
        };
        let vertical = imp.axis_vertical.get();
        let (to, cross_pos, cross_size) = if vertical {
            (
                (r.y() as f64, (r.y() + r.height()) as f64),
                r.x(),
                r.width(),
            )
        } else {
            (
                (r.x() as f64, (r.x() + r.width()) as f64),
                r.y(),
                r.height(),
            )
        };
        h.to = Some(to);
        let (start, end) = match h.from {
            Some(from) => {
                let now = self
                    .frame_clock()
                    .map_or(h.start, |clock| clock.frame_time());
                let extra_us = *h
                    .extra_us
                    .get_or_insert_with(|| highlight_extra_us((to.0 - from.0).abs()));
                let (edges, done) = highlight_edges(from, to, (now - h.start) as f64, extra_us);
                if done {
                    h.from = None;
                }
                edges
            }
            None => to,
        };
        h.cur = Some((start, end));
        drop(highlight);
        self.sync_under_highlight(Some((start, end)));
        let highlight = imp.highlight.borrow();
        let Some(h) = highlight.as_ref() else {
            return;
        };

        let pos = start.round() as i32;
        let size = (end.round() as i32 - pos).max(0);
        let cross = cross_size.round() as i32;
        let (width, height, point) = if vertical {
            (
                cross,
                size,
                gtk4::graphene::Point::new(cross_pos, pos as f32),
            )
        } else {
            (
                size,
                cross,
                gtk4::graphene::Point::new(pos as f32, cross_pos),
            )
        };
        h.widget.allocate(
            width,
            height,
            baseline,
            Some(gtk4::gsk::Transform::new().translate(&point)),
        );
    }

    /// Mark indicators whose center the highlight covers, so labels light up
    /// as it passes over them. Only colors depend on the class, so this is
    /// safe during allocation.
    fn sync_under_highlight(&self, span: Option<(f64, f64)>) {
        let vertical = self.imp().axis_vertical.get();
        for e in self.imp().entries.borrow().iter() {
            let covered = !e.leaving
                && span.is_some_and(|(start, end)| {
                    e.widget.compute_bounds(self).is_some_and(|r| {
                        let center = if vertical {
                            r.y() + r.height() / 2.0
                        } else {
                            r.x() + r.width() / 2.0
                        } as f64;
                        (start..=end).contains(&center)
                    })
                });
            sync_exclusive_class(
                &e.widget,
                &[widget::WORKSPACE_UNDER_HIGHLIGHT],
                covered.then_some(widget::WORKSPACE_UNDER_HIGHLIGHT),
            );
        }
    }

    /// Re-allocate every frame while the highlight slides.
    fn ensure_highlight_tick(&self) {
        {
            let mut highlight = self.imp().highlight.borrow_mut();
            let Some(h) = highlight.as_mut() else {
                return;
            };
            if h.ticking {
                return;
            }
            h.ticking = true;
        }
        // Like the extent animation, the callback is dropped on dispose.
        self.add_tick_callback(|wsc, clock| {
            // Ticks run before styles are updated for the frame, so labels
            // change color in the same frame the highlight reaches or leaves
            // them. Labels don't move while it slides, so the last target
            // position is exact.
            let span = wsc.imp().highlight.borrow().as_ref().and_then(|h| {
                let (from, to, extra_us) = (h.from?, h.to?, h.extra_us?);
                let elapsed = (clock.frame_time() - h.start) as f64;
                Some(highlight_edges(from, to, elapsed, extra_us).0)
            });
            if span.is_some() {
                wsc.sync_under_highlight(span);
            }
            wsc.queue_allocate();
            let mut highlight = wsc.imp().highlight.borrow_mut();
            match highlight.as_mut() {
                Some(h) if h.from.is_some() => glib::ControlFlow::Continue,
                Some(h) => {
                    h.ticking = false;
                    glib::ControlFlow::Break
                }
                None => glib::ControlFlow::Break,
            }
        });
    }

    fn remove_long_mw_provider(&self) {
        if let Some(provider) = self.imp().long_mw_css_provider.borrow_mut().take()
            && let Some(display) = gtk4::gdk::Display::default()
        {
            gtk4::style_context_remove_provider_for_display(&display, &provider);
        }
    }

    /// Load per-instance, display-level CSS with computed `min-width` rules
    /// for each long indicator (active/inactive delta matches short indicators).
    fn update_long_indicator_css(&self, indicators: &HashMap<i32, Indicator>) {
        use std::fmt::Write;

        let sizes = ConfigManager::global().theme_sizes();
        let wh = sizes.widget_height as f64;
        let delta = (wh * (INDICATOR_ACTIVE_MULT - INDICATOR_INACTIVE_MULT)).round();

        let mut css = String::new();
        for (&id, indicator) in indicators {
            if !indicator
                .root
                .has_css_class(widget::WORKSPACE_INDICATOR_LONG)
            {
                continue;
            }

            // Measure the label's natural width (content box, excludes padding).
            let label_width = indicator
                .label
                .as_ref()
                .map(|label| label.measure(gtk4::Orientation::Horizontal, -1).1)
                .unwrap_or(0);

            let short_inactive = (wh * INDICATOR_INACTIVE_MULT).round() as i32;
            let active_mw = label_width.max(short_inactive) + delta as i32;
            // Sliding labels stay put, so inactive ones get the active width.
            let inactive_mw = if self.has_css_class(widget::WORKSPACE_SLIDING) {
                active_mw
            } else {
                label_width.max(short_inactive)
            };

            let cls = ws_mw_class(id);
            let _ = writeln!(css, ".{cls} {{ min-width: {inactive_mw}px; }}");
            let _ = writeln!(css, ".{cls}.active {{ min-width: {active_mw}px; }}");
        }

        self.remove_long_mw_provider();

        if css.is_empty() {
            return;
        }

        let Some(display) = gtk4::gdk::Display::default() else {
            return;
        };

        let provider = CssProvider::new();
        provider.load_from_string(&css);
        gtk4::style_context_add_provider_for_display(&display, &provider, LONG_MW_CSS_PRIORITY);
        *self.imp().long_mw_css_provider.borrow_mut() = Some(provider);
    }
}

// ---------------------------------------------------------------------------
// ClipBin — lets an indicator's content (label + window icons) be allocated
// below its minimum while WorkspaceContainer animates the indicator's extent.
// ---------------------------------------------------------------------------

mod clip_bin_imp {
    use super::*;

    #[derive(Default)]
    pub struct ClipBin {
        pub(super) vertical: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ClipBin {
        const NAME: &'static str = "VibepanelWorkspaceClipBin";
        type Type = super::ClipBin;
        type ParentType = Widget;
    }

    impl ObjectImpl for ClipBin {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().set_overflow(gtk4::Overflow::Hidden);
        }

        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for ClipBin {
        fn request_mode(&self) -> gtk4::SizeRequestMode {
            gtk4::SizeRequestMode::ConstantSize
        }

        fn measure(&self, orientation: gtk4::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let obj = self.obj();
            let Some(child) = obj.first_child() else {
                return (0, 0, -1, -1);
            };
            let (min, nat, _, _) = child.measure(orientation, -1);
            if orientation == obj.flow_orientation() {
                (0, nat, -1, -1)
            } else {
                (min, nat, -1, -1)
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            let obj = self.obj();
            let Some(child) = obj.first_child() else {
                return;
            };
            // Below its minimum the child keeps that minimum and the overflow
            // is clipped, so a growing pill reveals its icons.
            let min = child.measure(obj.flow_orientation(), -1).0;
            let (width, height) = if self.vertical.get() {
                (width, height.max(min))
            } else {
                (width.max(min), height)
            };
            child.allocate(width, height, baseline, None);
        }
    }
}

glib::wrapper! {
    pub struct ClipBin(ObjectSubclass<clip_bin_imp::ClipBin>)
        @extends Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl ClipBin {
    fn new(child: &impl IsA<Widget>, vertical: bool) -> Self {
        let bin: Self = glib::Object::builder().build();
        bin.imp().vertical.set(vertical);
        child.set_parent(&bin);
        bin
    }

    fn flow_orientation(&self) -> gtk4::Orientation {
        if self.imp().vertical.get() {
            gtk4::Orientation::Vertical
        } else {
            gtk4::Orientation::Horizontal
        }
    }
}

/// Switch/addition duration (linear). Vertical growth reads faster than
/// horizontal at the same duration, so it gets a shorter one.
const SWITCH_DURATION_US: f64 = 200_000.0;
const SWITCH_DURATION_VERTICAL_US: f64 = 150_000.0;
/// Removal (gap closing) duration, ease-out, both orientations.
const REMOVAL_DURATION_US: f64 = 200_000.0;

fn ease_out_quad(t: f64) -> f64 {
    1.0 - (1.0 - t).powi(2)
}

/// Highlight slide: the edge leading the move arrives after this long...
const HIGHLIGHT_LEAD_US: f64 = 140_000.0;
/// ...and the trailing edge after this long, stretching the highlight.
const HIGHLIGHT_TRAIL_US: f64 = 210_000.0;
/// Longer jumps take slightly longer, so they don't look rushed.
const HIGHLIGHT_EXTRA_US_PER_PX: f64 = 400.0;
const HIGHLIGHT_MAX_EXTRA_US: f64 = 55_000.0;

fn ease_out_cubic(t: f64) -> f64 {
    1.0 - (1.0 - t).powi(3)
}

fn ease_in_out_cubic(t: f64) -> f64 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

fn highlight_extra_us(distance_px: f64) -> f64 {
    (distance_px * HIGHLIGHT_EXTRA_US_PER_PX).min(HIGHLIGHT_MAX_EXTRA_US)
}

/// Flow-axis (start, end) of the highlight `elapsed_us` into a slide from
/// `from` to `to`, and whether the slide is done. The edge leading the move
/// arrives first and the trailing edge follows, so the highlight stretches
/// on the way and settles exactly on its target.
fn highlight_edges(
    from: (f64, f64),
    to: (f64, f64),
    elapsed_us: f64,
    extra_us: f64,
) -> ((f64, f64), bool) {
    let lead = ease_out_cubic((elapsed_us / (HIGHLIGHT_LEAD_US + extra_us)).clamp(0.0, 1.0));
    let trail_t = (elapsed_us / (HIGHLIGHT_TRAIL_US + extra_us)).clamp(0.0, 1.0);
    let trail = ease_in_out_cubic(trail_t);
    let (k_start, k_end) = if to.0 >= from.0 {
        (trail, lead)
    } else {
        (lead, trail)
    };
    (
        (
            from.0 + (to.0 - from.0) * k_start,
            from.1 + (to.1 - from.1) * k_end,
        ),
        trail_t >= 1.0,
    )
}

/// Integer `(pos, size)` per extent, rounded from the cumulative sum so the
/// total is always `round(sum)` regardless of per-child rounding.
fn slot_layout(extents: impl Iterator<Item = f64>) -> Vec<(i32, i32)> {
    let mut acc = 0.0;
    let mut prev = 0;
    extents
        .map(|x| {
            acc += x;
            let end = acc.round() as i32;
            let slot = (prev, end - prev);
            prev = end;
            slot
        })
        .collect()
}

/// `new` order, with ids that disappeared kept right after their old
/// predecessor so their slot can close in place.
fn merge_ids(old: &[i32], new: &[i32]) -> Vec<i32> {
    let mut out = new.to_vec();
    let mut prev: Option<i32> = None;
    for &id in old {
        if !new.contains(&id) {
            let at = prev
                .and_then(|p| out.iter().position(|&x| x == p))
                .map_or(0, |i| i + 1);
            out.insert(at, id);
        }
        prev = Some(id);
    }
    out
}

/// Label type for workspace indicators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelType {
    /// Show icon glyphs (●, ○, ◆).
    Icons,
    /// Show workspace labels/names.
    ///
    /// Historically configured as `label_type = "numbers"`; `"name"` is the
    /// preferred value for new configs.
    Name,
    /// Show a meaningful workspace index when available, otherwise the
    /// workspace name (e.g. for Hyprland named workspaces).
    Index,
    /// Minimal - no text, just CSS styling.
    None,
}

impl LabelType {
    fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "icons" => LabelType::Icons,
            "name" | "numbers" => LabelType::Name,
            "index" => LabelType::Index,
            "none" => LabelType::None,
            other => {
                warn!(
                    "unknown workspace label_type {:?}, falling back to {:?}",
                    other, DEFAULT_LABEL_TYPE
                );
                DEFAULT_LABEL_TYPE
            }
        }
    }
}

fn workspace_label_text(label_type: LabelType, workspace: &Workspace) -> String {
    match label_type {
        LabelType::Icons => ICON_EMPTY.to_string(),
        LabelType::Name => workspace.name.clone(),
        LabelType::Index => {
            if workspace.idx >= 0 {
                workspace.idx.to_string()
            } else {
                workspace.name.clone()
            }
        }
        LabelType::None => String::new(),
    }
}

fn compact_workspace_label_text(label_type: LabelType, workspace: &Workspace) -> String {
    let text = workspace_label_text(label_type, workspace);
    match label_type {
        LabelType::Name | LabelType::Index => text.chars().take(2).collect(),
        LabelType::Icons | LabelType::None => text,
    }
}

fn workspace_label_text_for_orientation(
    label_type: LabelType,
    workspace: &Workspace,
    is_vertical: bool,
) -> String {
    if is_vertical {
        compact_workspace_label_text(label_type, workspace)
    } else {
        workspace_label_text(label_type, workspace)
    }
}

const DEFAULT_LABEL_TYPE: LabelType = LabelType::None;
const DEFAULT_SEPARATOR: &str = "";

/// Multiplier for inactive indicator width relative to widget_height.
/// Also drives the per-instance long-indicator min-width rules.
pub(crate) const INDICATOR_INACTIVE_MULT: f64 = 0.7;
/// Multiplier for active indicator width relative to widget_height.
/// Used in CSS generation (css/bar.rs) for min-width on active indicators.
pub(crate) const INDICATOR_ACTIVE_MULT: f64 = 1.0;
/// Horizontal padding per side (px) for long (named) indicators.
pub(crate) const LONG_INDICATOR_HPAD: i32 = 6;

/// Priority for the long-indicator min-width CSS provider.
/// Above theme CSS (USER=800) but below user CSS (USER+100=900).
const LONG_MW_CSS_PRIORITY: u32 = gtk4::STYLE_PROVIDER_PRIORITY_USER + 10;

/// CSS class for a workspace's per-indicator min-width rule.
/// Negative IDs (Sway named workspaces) produce double hyphens, e.g. `ws-mw--3`.
fn ws_mw_class(id: i32) -> String {
    format!("ws-mw-{id}")
}

/// Configuration for the workspaces widget.
#[derive(Debug, Clone)]
pub struct WorkspacesConfig {
    /// How to display workspace labels.
    pub label_type: LabelType,
    /// Separator string between workspace indicators.
    pub separator: String,
    /// Whether to animate circle↔pill transitions.
    /// `None` = not explicitly set by user (inherits from global `theme.animations`).
    pub animate: Option<bool>,
    /// Whether to use this bar output's per-output workspace state.
    ///
    /// When `true`, active styling reflects the workspace active on this bar's
    /// output. When `false`, the widget shows global/all-output workspace state,
    /// including each output's current workspace, but active styling still
    /// reflects the compositor's globally focused workspace.
    pub filter_by_output: bool,
    /// Whether to show compositor-reported workspaces without windows.
    pub show_unoccupied: bool,
    /// Whether to show each workspace's window icons inside its indicator.
    pub show_windows: bool,
    /// Maximum window icons per workspace (0 = unlimited); the rest show as "+N".
    pub max_windows: usize,
    /// Whether windows of the same app share one icon.
    pub group_by_app: bool,
    /// Window icon size in pixels. `None` fits a plain indicator's height.
    pub icon_size: Option<i32>,
}

/// Options that only apply when `show_windows` is enabled.
const WINDOW_OPTIONS: &[&str] = &["max_windows", "group_by_app", "icon_size"];

impl WidgetConfig for WorkspacesConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options(
            "workspaces",
            entry,
            &[
                "label_type",
                "separator",
                "animate",
                "filter_by_output",
                "show_unoccupied",
                "show_windows",
                "max_windows",
                "group_by_app",
                "icon_size",
            ],
        );

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

        let animate = entry.options.get("animate").and_then(|v| v.as_bool());
        let defaults = Self::default();
        let filter_by_output = entry
            .options
            .get("filter_by_output")
            .and_then(|v| v.as_bool())
            .unwrap_or(defaults.filter_by_output);
        let show_unoccupied = entry
            .options
            .get("show_unoccupied")
            .and_then(|v| v.as_bool())
            .unwrap_or(defaults.show_unoccupied);
        let show_windows = entry
            .options
            .get("show_windows")
            .and_then(|v| v.as_bool())
            .unwrap_or(defaults.show_windows);
        let max_windows = entry
            .options
            .get("max_windows")
            .and_then(|v| v.as_integer())
            .map(|v| usize::try_from(v).unwrap_or(0))
            .unwrap_or(defaults.max_windows);
        let group_by_app = entry
            .options
            .get("group_by_app")
            .and_then(|v| v.as_bool())
            .unwrap_or(defaults.group_by_app);
        // Capped to fit the bar in `WindowsMode::resolve()`.
        let icon_size = entry
            .options
            .get("icon_size")
            .and_then(|v| v.as_integer())
            .map(|v| (v as i32).max(8));

        if show_windows
            && entry.options.contains_key("label_type")
            && windows_label_type(label_type) != label_type
        {
            warn!(
                "workspaces label_type = {:?} can't be combined with window icons; index is \
                 shown while they are. For window icons without workspaces, use the taskbar widget",
                entry
                    .options
                    .get("label_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
            );
        }

        if !show_windows {
            let ignored: Vec<&str> = WINDOW_OPTIONS
                .iter()
                .copied()
                .filter(|key| entry.options.contains_key(*key))
                .collect();
            if !ignored.is_empty() {
                warn!(
                    "workspaces {} ignored when show_windows is false",
                    ignored.join(", ")
                );
            }
        }

        Self {
            label_type,
            separator,
            animate,
            filter_by_output,
            show_unoccupied,
            show_windows,
            max_windows,
            group_by_app,
            icon_size,
        }
    }
}

impl Default for WorkspacesConfig {
    fn default() -> Self {
        Self {
            label_type: DEFAULT_LABEL_TYPE,
            separator: DEFAULT_SEPARATOR.to_string(),
            animate: None,
            filter_by_output: true,
            show_unoccupied: false,
            show_windows: false,
            max_windows: 3,
            group_by_app: false,
            icon_size: None,
        }
    }
}

/// Whether (animated) indicators use the sliding highlight. Text labels stay
/// put while one highlight slides between them; dots and glyphs keep their
/// growing active pill as their only motion.
fn highlight_slides(label_type: LabelType) -> bool {
    matches!(label_type, LabelType::Index | LabelType::Name)
}

/// Label type used with window icons. Without a workspace label the widget
/// would only be a weaker taskbar, so `none` and `icons` show the index.
fn windows_label_type(label_type: LabelType) -> LabelType {
    match label_type {
        LabelType::Name => LabelType::Name,
        LabelType::Index | LabelType::Icons | LabelType::None => LabelType::Index,
    }
}

/// Window icon settings, resolved when `show_windows` is enabled and the
/// compositor backend reports windows.
#[derive(Debug, Clone, Copy)]
struct WindowsMode {
    icon_size: i32,
    max_windows: usize,
    group_by_app: bool,
}

impl WindowsMode {
    fn resolve(config: &WorkspacesConfig) -> Option<Self> {
        if !config.show_windows {
            return None;
        }
        if !CompositorManager::global().window_list_has_workspaces() {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                warn!(
                    "workspaces show_windows is not supported on {}; showing workspaces only",
                    CompositorManager::global().backend_name()
                );
            });
            return None;
        }
        let sizes = ConfigManager::global().theme_sizes();
        let max_icon = (sizes.widget_height as i32 - 2 * WINDOW_ICON_PAD_CROSS).max(8);
        // By default an icon plus its padding fits a plain indicator's
        // height, so pills don't grow taller when windows show up.
        let default_icon = (sizes.text_icon_size as i32 - 2 * WINDOW_ICON_PAD_CROSS).max(8);
        let icon_size = config.icon_size.unwrap_or(default_icon);
        if icon_size > max_icon {
            warn!("workspaces icon_size {icon_size} is too big for the bar; using {max_icon}");
        }
        Some(Self {
            icon_size: icon_size.min(max_icon),
            max_windows: config.max_windows,
            group_by_app: config.group_by_app,
        })
    }
}

/// Padding (px) around each window icon inside an indicator, along the bar
/// and across it. The cross padding is kept small so icons stay large without
/// making pills taller.
pub(crate) const WINDOW_ICON_PAD_FLOW: i32 = 2;
pub(crate) const WINDOW_ICON_PAD_CROSS: i32 = 1;

/// Keeps label pills as round as the theme intends.
///
/// The theme's pill radius is sized for indicators as tall as `--icon-size`,
/// but a label's line height or a large `icon_size` makes pills taller,
/// flattening their ends. The radius is scaled by the measured height; all
/// pills are equally tall, so measuring one is enough.
struct PillRadius {
    provider: CssProvider,
    icon_size: f64,
    /// Scale applied so far, once measured.
    scale: Cell<Option<f64>>,
}

impl PillRadius {
    fn new(content: &GtkBox) -> Self {
        let provider = CssProvider::new();
        #[allow(deprecated)]
        content
            .style_context()
            .add_provider(&provider, gtk4::STYLE_PROVIDER_PRIORITY_USER + 10);
        Self {
            provider,
            icon_size: ConfigManager::global().theme_sizes().text_icon_size.max(1) as f64,
            scale: Cell::new(None),
        }
    }

    /// Re-measure a pill and update the radius scale if needed.
    fn sync(&self, indicators: &HashMap<i32, Indicator>, is_vertical: bool) {
        let cross = if is_vertical {
            Orientation::Horizontal
        } else {
            Orientation::Vertical
        };
        let scale = indicators
            .values()
            .map(|indicator| &indicator.root)
            // Unrooted widgets aren't styled by the theme yet.
            .find(|root| root.root().is_some())
            .map(|root| pill_radius_scale(root.measure(cross, -1).1, self.icon_size));
        if scale.is_some() && scale != self.scale.get() {
            self.scale.set(scale);
            debug!("Workspace pill radius scale: {scale:?}");
            self.provider.load_from_string(&format!(
                ".workspaces .content {{ --vp-workspace-pill-scale: {}; }}",
                scale.unwrap_or(1.0),
            ));
        }
    }
}

/// Radius scale for a pill `extent` px tall, rounded to keep CSS reloads rare.
fn pill_radius_scale(extent: i32, icon_size: f64) -> f64 {
    ((extent as f64 / icon_size).max(1.0) * 1000.0).round() / 1000.0
}

/// Padding (px) closing a workspace pill after its window icons. Such pills
/// sit closer to the next indicator, so this padding is what separates them.
pub(crate) const WINDOWS_INDICATOR_PAD: i32 = 4;

/// What one window icon in an indicator stands for: a single window, or every
/// window of an app when `group_by_app` is enabled.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum WindowItemKey {
    Window(u64),
    App(String),
}

#[derive(Debug, Clone, PartialEq)]
struct WindowItem {
    key: WindowItemKey,
    app_id: String,
    /// Windows behind this icon, in compositor order.
    window_ids: Vec<u64>,
    /// Index of the focused window in `window_ids`.
    focused: Option<usize>,
    urgent: bool,
    tooltip: String,
}

/// Icons to show for one workspace. With `output`, windows on other outputs
/// are skipped: MangoWC reuses tag IDs on every monitor.
fn workspace_window_items(
    windows: &[Window],
    workspace_id: i32,
    output: Option<&str>,
    group_by_app: bool,
) -> Vec<WindowItem> {
    let mut items: Vec<WindowItem> = Vec::new();
    let on_workspace = windows.iter().filter(|window| {
        window.is_on_workspace(workspace_id)
            && (output.is_none() || window.output.is_none() || window.output.as_deref() == output)
    });
    for window in on_workspace {
        if group_by_app
            && let Some(item) = items.iter_mut().find(|item| item.app_id == window.app_id)
        {
            if window.is_focused {
                item.focused = Some(item.window_ids.len());
            }
            item.window_ids.push(window.id);
            item.urgent |= window.is_urgent;
            let count = item.window_ids.len();
            item.tooltip = format!(
                "{} • {}",
                app_display_name(&window.app_id),
                tr!("{n} window" | "{n} windows" % count)
            );
            continue;
        }
        items.push(WindowItem {
            key: if group_by_app {
                WindowItemKey::App(window.app_id.clone())
            } else {
                WindowItemKey::Window(window.id)
            },
            app_id: window.app_id.clone(),
            window_ids: vec![window.id],
            focused: window.is_focused.then_some(0),
            urgent: window.is_urgent,
            tooltip: window_tooltip(window),
        });
    }
    items
}

/// Keep at most `max` items (0 = all) and return how many were hidden. The
/// focused item, or else an urgent one, is swapped into view when it would be
/// hidden, without pushing a visible focused item out.
fn limit_window_items(items: &mut Vec<WindowItem>, max: usize) -> usize {
    if max == 0 || items.len() <= max {
        return 0;
    }
    let hidden = items.len() - max;
    let beyond =
        |pred: fn(&WindowItem) -> bool| items[max..].iter().position(pred).map(|index| index + max);
    let focused = |item: &WindowItem| item.focused.is_some();
    let urgent = |item: &WindowItem| item.urgent;
    let promote = if items[..max].iter().any(focused) {
        beyond(urgent)
    } else {
        beyond(focused).or_else(|| beyond(urgent))
    };
    if let Some(from) = promote
        && let Some(slot) = (0..max).rev().find(|&index| !focused(&items[index]))
    {
        items.swap(slot, from);
    }
    items.truncate(max);
    hidden
}

/// Window to focus when an icon is clicked: the first one, or for grouped
/// icons the one after the focused window, cycling.
fn window_item_click_target(item: &WindowItem) -> Option<u64> {
    let next = item
        .focused
        .map_or(0, |index| (index + 1) % item.window_ids.len().max(1));
    item.window_ids.get(next).copied()
}

/// A displayed workspace indicator and the parts that are updated in place.
struct Indicator {
    /// Ripple overlay; laid out by the container and carries state classes.
    root: Widget,
    /// Workspace label, unless `label_type = "none"`.
    label: Option<Label>,
    /// Window icons, when `show_windows` is active.
    windows: Option<IndicatorWindows>,
}

struct IndicatorWindows {
    row: GtkBox,
    overflow: Label,
    icons: RefCell<Vec<WindowIcon>>,
    scroll_focus: Rc<WorkspaceScrollFocus>,
}

impl IndicatorWindows {
    /// Show `items`, reusing icon widgets by key so unchanged icons are not
    /// recreated on every window-list or workspace update.
    fn update(&self, items: Vec<WindowItem>, hidden: usize, icon_size: i32) {
        let mut icons = self.icons.borrow_mut();
        let keys_changed = icons.len() != items.len()
            || icons
                .iter()
                .zip(&items)
                .any(|(icon, item)| icon.item.borrow().key != item.key);
        if keys_changed {
            let mut old: HashMap<WindowItemKey, WindowIcon> = icons
                .drain(..)
                .map(|icon| {
                    let key = icon.item.borrow().key.clone();
                    (key, icon)
                })
                .collect();
            let mut prev: Option<Widget> = None;
            for item in &items {
                let icon = old
                    .remove(&item.key)
                    .unwrap_or_else(|| WindowIcon::new(item, icon_size, &self.scroll_focus));
                if icon.root.parent().is_some() {
                    self.row.reorder_child_after(&icon.root, prev.as_ref());
                } else {
                    self.row.insert_child_after(&icon.root, prev.as_ref());
                }
                prev = Some(icon.root.clone().upcast());
                icons.push(icon);
            }
            for stale in old.into_values() {
                self.row.remove(&stale.root);
            }
        }
        for (icon, item) in icons.iter().zip(items) {
            icon.update(item);
        }
        self.row.set_visible(!icons.is_empty());
        if hidden > 0 {
            self.overflow.set_text(&format!("+{hidden}"));
        }
        self.overflow.set_visible(hidden > 0);
    }
}

struct WindowIcon {
    root: GtkBox,
    icon: Image,
    /// Window count, shown when the icon stands for several windows.
    count: Label,
    item: Rc<RefCell<WindowItem>>,
}

impl WindowIcon {
    fn new(item: &WindowItem, icon_size: i32, scroll_focus: &Rc<WorkspaceScrollFocus>) -> Self {
        let root = GtkBox::new(Orientation::Horizontal, 0);
        root.add_css_class(widget::WORKSPACE_WINDOW);
        root.add_css_class(state::CLICKABLE);
        root.set_valign(Align::Center);
        root.set_halign(Align::Center);

        let icon = app_icon(&item.app_id, icon_size);
        icon.add_css_class(widget::WORKSPACE_WINDOW_ICON);
        let count = Label::new(None);
        count.add_css_class(widget::WORKSPACE_WINDOW_COUNT);
        count.set_halign(Align::End);
        count.set_valign(Align::End);
        count.set_visible(false);
        let overlay = gtk4::Overlay::new();
        overlay.set_child(Some(&icon));
        overlay.add_overlay(&count);
        root.append(&overlay);

        let item = Rc::new(RefCell::new(item.clone()));
        connect_focus_on_click(&root, {
            let item = Rc::clone(&item);
            let scroll_focus = Rc::clone(scroll_focus);
            move || {
                // Like a workspace click, this supersedes pending scrolls.
                scroll_focus.clear();
                scroll_focus.cancel_target();
                window_item_click_target(&item.borrow())
            }
        });

        Self {
            root,
            icon,
            count,
            item,
        }
    }

    fn update(&self, item: WindowItem) {
        let mut current = self.item.borrow_mut();
        if current.app_id != item.app_id {
            self.icon.set_icon_name(Some(&app_icon_name(&item.app_id)));
        }
        let windows = item.window_ids.len();
        if windows > 1 {
            self.count.set_text(&windows.to_string());
        }
        self.count.set_visible(windows > 1);
        let root = self.root.upcast_ref::<Widget>();
        sync_exclusive_class(
            root,
            &[state::FOCUSED],
            item.focused.is_some().then_some(state::FOCUSED),
        );
        sync_exclusive_class(root, &[state::URGENT], item.urgent.then_some(state::URGENT));
        set_tooltip(&self.root, Some(&item.tooltip));
        *current = item;
    }
}

/// Workspaces widget that displays workspace indicators.
pub struct WorkspacesWidget {
    /// Shared base widget container.
    base: BaseWidget,
    /// Callback ID for WorkspaceService, used to disconnect on drop.
    workspace_callback_id: CallbackId,
    /// Callback ID for WindowListService, when window icons are shown.
    window_list_callback_id: Option<CallbackId>,
    scroll_focus: Rc<WorkspaceScrollFocus>,
}

const SCROLL_FOCUS_RELEASE_DELAY_MS: u64 = 250;
const SCROLL_SETTLE_DELAY_SECS: u64 = 1;

#[derive(Default)]
struct WorkspaceScrollFocus {
    window: RefCell<Option<(glib::WeakRef<ApplicationWindow>, glib::SignalHandlerId)>>,
    pending: Cell<Option<i32>>,
    acquisition: RefCell<Option<glib::SourceId>>,
    release: RefCell<Option<glib::SourceId>>,
    confirmed: Cell<Option<i32>>,
    target: Cell<Option<(i32, std::time::Instant)>>,
}

impl WorkspaceScrollFocus {
    fn request(self: &Rc<Self>, window: &ApplicationWindow, workspace_id: i32) {
        if self.window.borrow().is_none() {
            let weak = Rc::downgrade(self);
            let handler = window.connect_is_active_notify(move |window| {
                if let Some(state) = weak.upgrade() {
                    state.dispatch_if_focused(window);
                }
            });
            self.window
                .borrow_mut()
                .replace((window.downgrade(), handler));
        }
        if let Some(id) = self.release.borrow_mut().take() {
            id.remove();
        }
        self.pending.set(Some(workspace_id));
        if self.acquisition.borrow().is_none() {
            // Niri may deny focus (for example, behind fullscreen). Fall back
            // to switching without warp protection rather than dropping the scroll.
            let weak = Rc::downgrade(self);
            self.acquisition
                .borrow_mut()
                .replace(glib::timeout_add_local_once(
                    std::time::Duration::from_secs(1),
                    move || {
                        if let Some(state) = weak.upgrade() {
                            state.acquisition.borrow_mut().take();
                            let pending = state.pending.take();
                            state.clear();
                            if let Some(workspace_id) = pending {
                                WorkspaceService::global().switch_workspace(workspace_id);
                            }
                        }
                    },
                ));
        }
        window.set_keyboard_mode(KeyboardMode::Exclusive);
        window.present();
        // Layer-shell state commits asynchronously. Only actual keyboard focus
        // confirms that workspace IPC can no longer race the cursor-warp check.
        self.dispatch_if_focused(window);
    }

    fn dispatch_if_focused(self: &Rc<Self>, window: &ApplicationWindow) {
        let Some(workspace_id) = take_focused_scroll(&self.pending, window.is_active()) else {
            return;
        };
        if let Some(id) = self.acquisition.borrow_mut().take() {
            id.remove();
        }
        if let Some(id) = self.release.borrow_mut().take() {
            id.remove();
        }
        WorkspaceService::global().switch_workspace(workspace_id);
        let weak = Rc::downgrade(self);
        let window = window.downgrade();
        self.release
            .borrow_mut()
            .replace(glib::timeout_add_local_once(
                std::time::Duration::from_millis(SCROLL_FOCUS_RELEASE_DELAY_MS),
                move || {
                    if let Some(state) = weak.upgrade() {
                        state.release.borrow_mut().take();
                        if let Some(window) = window.upgrade() {
                            window.set_keyboard_mode(KeyboardMode::None);
                        }
                    }
                },
            ));
    }

    fn clear(&self) {
        self.pending.set(None);
        if let Some(id) = self.acquisition.borrow_mut().take() {
            id.remove();
        }
        if let Some(id) = self.release.borrow_mut().take() {
            id.remove();
        }
        if let Some((window, handler)) = self.window.borrow_mut().take()
            && let Some(window) = window.upgrade()
        {
            window.disconnect(handler);
            window.set_keyboard_mode(KeyboardMode::None);
        }
    }

    fn current(&self) -> Option<i32> {
        if let Some((workspace_id, deadline)) = self.target.get()
            && std::time::Instant::now() < deadline
        {
            return Some(workspace_id);
        }
        self.target.set(None);
        self.confirmed.get()
    }

    fn set_target(&self, workspace_id: i32) {
        self.target.set(Some((
            workspace_id,
            std::time::Instant::now() + std::time::Duration::from_secs(SCROLL_SETTLE_DELAY_SECS),
        )));
    }

    fn update(&self, workspace_id: Option<i32>) {
        // Matching IDs cannot acknowledge requests: 2 -> 3 -> 2 may echo the first 2.
        // Retain target until deadline/cancellation; external changes may
        // lag navigation by 1s. Clicks supersede it; finer handling needs request IDs.
        self.confirmed.set(workspace_id);
    }

    fn cancel_target(&self) {
        self.target.set(None);
    }
}

fn take_focused_scroll(pending: &Cell<Option<i32>>, focused: bool) -> Option<i32> {
    focused.then(|| pending.take()).flatten()
}

impl WorkspacesWidget {
    /// Create a new workspaces widget with the given configuration.
    ///
    /// # Arguments
    ///
    /// * `config` - Widget configuration (label type, separator).
    /// * `output_id` - Optional output/monitor name. When set and
    ///   `filter_by_output = true`, the widget will:
    ///   - For Niri: only show workspaces belonging to this output.
    ///   - For MangoWC: show all workspaces but with per-output window counts.
    ///   - For Hyprland: show the workspace currently active on this output,
    ///     plus workspaces reported with windows on this output.
    pub fn new(config: WorkspacesConfig, output_id: Option<String>) -> Self {
        let base = BaseWidget::new(&[widget::WORKSPACES]);

        let filter_by_output = config.filter_by_output;
        let show_unoccupied = config.show_unoccupied;
        let windows_mode = WindowsMode::resolve(&config);
        // Only when window icons are actually shown, so unsupported backends
        // keep the configured label.
        let label_type = if windows_mode.is_some() {
            windows_label_type(config.label_type)
        } else {
            config.label_type
        };
        // Dots are exactly as tall as the theme's pill radius assumes.
        let pill_radius =
            (label_type != LabelType::None).then(|| Rc::new(PillRadius::new(base.content())));
        // Per-widget animate flag takes precedence when explicitly set.
        // Falls back to the global theme.animations setting.
        let is_vertical = ConfigManager::global().bar_position().is_vertical();

        let animate = config
            .animate
            .unwrap_or_else(|| ConfigManager::global().animations_enabled());

        // Animated mode uses WorkspaceContainer; otherwise indicators go in the GtkBox.
        let ws_container: Option<WorkspaceContainer> = if animate {
            let container = WorkspaceContainer::new();
            container.set_axis_vertical(is_vertical);
            if highlight_slides(label_type) {
                container.add_css_class(widget::WORKSPACE_SLIDING);
            }
            base.content().append(&container);
            Some(container)
        } else {
            None
        };

        let content_box = base.content().clone();

        let indicators: Rc<RefCell<HashMap<i32, Indicator>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let current_ids = Rc::new(RefCell::new(Vec::new()));
        let separator = config.separator;

        let scroll = EventControllerScroll::new(EventControllerScrollFlags::VERTICAL);
        scroll.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let accumulated = Cell::new(0.0f64);
        let scroll_focus = Rc::new(WorkspaceScrollFocus::default());
        let weak_focus = Rc::downgrade(&scroll_focus);
        base.widget().connect_unmap(move |_| {
            if let Some(state) = weak_focus.upgrade() {
                state.clear();
                state.cancel_target();
            }
        });
        let focus_for_scroll = Rc::clone(&scroll_focus);
        let current_ids_for_scroll = Rc::clone(&current_ids);
        scroll.connect_scroll(move |controller, _dx, dy| {
            let delta = workspace_scroll_delta(controller.unit(), dy);
            let mut acc = accumulated.get();
            if (acc > 0.0 && delta < 0.0) || (acc < 0.0 && delta > 0.0) {
                acc = 0.0;
            }
            acc += delta;

            while acc.abs() >= 1.0 {
                let next = adjacent_workspace_id(
                    &current_ids_for_scroll.borrow(),
                    focus_for_scroll.current(),
                    acc > 0.0,
                );
                if let Some(workspace_id) = next {
                    focus_for_scroll.set_target(workspace_id);
                    hide_tooltip();
                    debug!("Switching to workspace {} by scrolling", workspace_id);

                    let window = if CompositorManager::global().backend_name() == "Niri" {
                        controller
                            .widget()
                            .and_then(|widget| widget.root())
                            .and_then(|root| root.downcast::<ApplicationWindow>().ok())
                    } else {
                        None
                    };
                    let popover_has_focus = window.as_ref().is_some_and(|window| {
                        window
                            .application()
                            .and_then(|app| app.active_window())
                            .is_some_and(|active| {
                                active != window.clone().upcast::<gtk4::Window>()
                                    && active.is_active()
                            })
                    });

                    if let Some(window) = window.filter(|_| !popover_has_focus) {
                        // Niri's `warp-mouse-to-focus` can move the
                        // pointer off the bar during a workspace switch. Hold keyboard focus while
                        // dispatching so repeated scrolling remains over the workspace widget.
                        focus_for_scroll.request(&window, workspace_id);
                    } else {
                        focus_for_scroll.clear();
                        WorkspaceService::global().switch_workspace(workspace_id);
                    }
                }
                acc -= acc.signum();
            }

            accumulated.set(acc);
            glib::Propagation::Stop
        });
        base.widget().add_controller(scroll);

        let output_id_debug = output_id.clone();
        let focus_for_updates = Rc::clone(&scroll_focus);

        // Workspace and window-list updates arrive separately; each re-renders
        // from the latest state of both.
        let last_snapshot: Rc<RefCell<Option<WorkspaceServiceSnapshot>>> =
            Rc::new(RefCell::new(None));
        let windows: Rc<RefCell<Vec<Window>>> = Rc::new(RefCell::new(Vec::new()));
        if let Some(pill_radius) = &pill_radius {
            // The first renders happen before the widget is in the bar.
            let pill_radius = Rc::clone(pill_radius);
            let indicators = Rc::clone(&indicators);
            base.widget().connect_map(move |_| {
                pill_radius.sync(&indicators.borrow(), is_vertical);
            });
        }
        let render: Rc<dyn Fn()> = Rc::new({
            let last_snapshot = Rc::clone(&last_snapshot);
            let windows = Rc::clone(&windows);
            move || {
                let snapshot = last_snapshot.borrow();
                let Some(snapshot) = snapshot.as_ref() else {
                    return;
                };
                let windows = windows.borrow();
                update_indicators(
                    &focus_for_updates,
                    &content_box,
                    ws_container.as_ref(),
                    &indicators,
                    &current_ids,
                    label_type,
                    &separator,
                    snapshot,
                    show_unoccupied,
                    is_vertical,
                    if filter_by_output {
                        output_id.as_deref()
                    } else {
                        None
                    },
                    windows_mode.as_ref().map(|mode| (mode, windows.as_slice())),
                );

                let indicators = indicators.borrow();
                focus_for_updates.update(current_ids.borrow().iter().copied().find(|id| {
                    indicators
                        .get(id)
                        .is_some_and(|indicator| indicator.root.has_css_class(widget::ACTIVE))
                }));
                if let Some(pill_radius) = &pill_radius {
                    pill_radius.sync(&indicators, is_vertical);
                }
            }
        });

        let workspace_callback_id = WorkspaceService::global().connect({
            let render = Rc::clone(&render);
            move |snapshot| {
                *last_snapshot.borrow_mut() = Some(snapshot.clone());
                render();
            }
        });

        let window_list_callback_id = windows_mode.is_some().then(|| {
            // Skip our media pop-out (its app ID is the program name set in main).
            let own_app_id = glib::prgname();
            WindowListService::global().connect(move |snapshot| {
                *windows.borrow_mut() = snapshot
                    .windows
                    .iter()
                    .filter(|window| own_app_id.as_deref() != Some(window.app_id.as_str()))
                    .cloned()
                    .collect();
                render();
            })
        });

        debug!(
            "WorkspacesWidget created (output_id: {:?})",
            output_id_debug
        );
        Self {
            base,
            workspace_callback_id,
            window_list_callback_id,
            scroll_focus,
        }
    }

    /// Get the root GTK widget for embedding in the bar.
    pub fn widget(&self) -> &GtkBox {
        self.base.widget()
    }
}

impl Drop for WorkspacesWidget {
    fn drop(&mut self) {
        self.scroll_focus.clear();
        WorkspaceService::global().disconnect(self.workspace_callback_id);
        if let Some(id) = self.window_list_callback_id {
            WindowListService::global().disconnect(id);
        }
    }
}

const SURFACE_SCROLL_PIXELS_PER_WORKSPACE: f64 = 25.0;

fn workspace_scroll_delta(unit: gtk4::gdk::ScrollUnit, delta: f64) -> f64 {
    match unit {
        gtk4::gdk::ScrollUnit::Surface => delta / SURFACE_SCROLL_PIXELS_PER_WORKSPACE,
        _ => delta,
    }
}

fn adjacent_workspace_id(ids: &[i32], active_id: Option<i32>, forward: bool) -> Option<i32> {
    let active_index = ids.iter().position(|id| Some(*id) == active_id)?;
    let next_index = if forward {
        active_index.checked_add(1)
    } else {
        active_index.checked_sub(1)
    }?;
    ids.get(next_index).copied()
}

/// Icon glyphs for workspace indicators.
const ICON_OCCUPIED: &str = "●";
const ICON_EMPTY: &str = "○";
const ICON_ACTIVE: &str = "◆";

/// Create a single workspace indicator widget.
///
/// Returns an `Overlay` wrapping the inner dot/label with a ripple effect.
/// CSS classes for sizing and state go on the overlay, so its measured size
/// follows the active/long state.
fn create_single_indicator(
    scroll_focus: &Rc<WorkspaceScrollFocus>,
    label_type: LabelType,
    workspace: &Workspace,
    is_vertical: bool,
    windows_mode: Option<&WindowsMode>,
) -> Indicator {
    let workspace_id = workspace.id;

    let label = (label_type != LabelType::None).then(|| {
        let label_text = workspace_label_text_for_orientation(label_type, workspace, is_vertical);
        let label = Label::new(Some(&label_text));
        // Optical centering
        label.set_xalign(if is_vertical { 0.58 } else { 0.55 });
        label.set_ellipsize(EllipsizeMode::End);
        label.set_single_line_mode(true);
        label
    });
    let is_long = label
        .as_ref()
        .is_some_and(|label| label.text().chars().count() > 2);

    let (overlay, ripple_handle, windows) = if let Some(mode) = windows_mode {
        let orientation = if is_vertical {
            Orientation::Vertical
        } else {
            Orientation::Horizontal
        };
        let content = GtkBox::new(orientation, 0);
        content.set_halign(Align::Center);
        content.set_valign(Align::Center);
        // Every pill is as thick as a window icon, so a large icon_size
        // doesn't leave pills without windows (and the highlight on them)
        // thinner than the rest.
        let thickness = mode.icon_size + 2 * WINDOW_ICON_PAD_CROSS;
        if is_vertical {
            content.set_size_request(thickness, -1);
        } else {
            content.set_size_request(-1, thickness);
        }
        if let Some(label) = &label {
            label.add_css_class(widget::WORKSPACE_LABEL);
            content.append(label);
        }
        let row = GtkBox::new(orientation, 0);
        row.add_css_class(widget::WORKSPACE_WINDOWS);
        row.set_visible(false);
        content.append(&row);
        let overflow = Label::new(None);
        overflow.add_css_class(widget::WORKSPACE_WINDOWS_OVERFLOW);
        overflow.set_visible(false);
        content.append(&overflow);
        let (o, rh) = wrap_with_ripple(&ClipBin::new(&content, is_vertical));
        o.add_css_class(widget::WORKSPACE_INDICATOR_WINDOWS);
        let windows = IndicatorWindows {
            row,
            overflow,
            icons: RefCell::new(Vec::new()),
            scroll_focus: Rc::clone(scroll_focus),
        };
        (o, rh, Some(windows))
    } else if let Some(label) = &label {
        let (o, rh) = wrap_with_ripple(label);
        (o, rh, None)
    } else {
        // GtkBox avoids font-metric intrinsic sizing; CSS controls dimensions.
        let dot = GtkBox::new(Orientation::Horizontal, 0);
        let (o, rh) = wrap_with_ripple(&dot);
        (o, rh, None)
    };

    // Sizing, state, and visual classes go on the overlay — it is the
    // widget that WorkspaceContainer measures and lays out.
    overlay.add_css_class(widget::WORKSPACE_INDICATOR);
    overlay.add_css_class(state::CLICKABLE);
    if label_type == LabelType::None {
        overlay.add_css_class(widget::WORKSPACE_INDICATOR_MINIMAL);
    }
    if is_long {
        overlay.add_css_class(widget::WORKSPACE_INDICATOR_LONG);
        overlay.add_css_class(&ws_mw_class(workspace_id));
    }
    // Non-animated mode only; WorkspaceContainer overrides to Fill.
    overlay.set_valign(Align::Center);
    overlay.set_halign(Align::Center);

    let gesture = GestureClick::new();
    gesture.set_button(BUTTON_PRIMARY);
    gesture.connect_pressed({
        let rh = ripple_handle;
        move |gesture, _n_press, x, y| {
            trigger_ripple_from_gesture(gesture, x, y, &rh);
        }
    });
    let scroll_focus = Rc::clone(scroll_focus);
    gesture.connect_released(move |gesture, _n_press, _x, _y| {
        if gesture.current_button() != BUTTON_PRIMARY {
            return;
        }
        scroll_focus.clear();
        scroll_focus.set_target(workspace_id);
        debug!("Switching to workspace {}", workspace_id);
        WorkspaceService::global().switch_workspace(workspace_id);
    });
    overlay.add_controller(gesture);

    Indicator {
        root: overlay.upcast(),
        label,
        windows,
    }
}

/// Recreate all indicators in the plain (non-animated) GtkBox.
#[allow(clippy::too_many_arguments)]
fn create_indicators(
    scroll_focus: &Rc<WorkspaceScrollFocus>,
    container: &GtkBox,
    indicators: &mut HashMap<i32, Indicator>,
    label_type: LabelType,
    separator: &str,
    workspaces: &[Workspace],
    is_vertical: bool,
    windows_mode: Option<&WindowsMode>,
) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
    indicators.clear();

    for (i, workspace) in workspaces.iter().enumerate() {
        let indicator = create_single_indicator(
            scroll_focus,
            label_type,
            workspace,
            is_vertical,
            windows_mode,
        );
        container.append(&indicator.root);
        indicators.insert(workspace.id, indicator);

        if !is_vertical && i < workspaces.len() - 1 && !separator.is_empty() {
            let sep = Label::new(Some(separator));
            sep.set_valign(Align::Center);
            sep.add_css_class(widget::WORKSPACE_SEPARATOR);
            container.append(&sep);
        }
    }
}

fn collect_display_ids(
    workspaces: &[Workspace],
    active_workspaces: &HashSet<i32>,
    snapshot: &WorkspaceServiceSnapshot,
    show_unoccupied: bool,
    include_all_output_active: bool,
) -> HashSet<i32> {
    let mut display_ids: HashSet<i32> = workspaces
        .iter()
        // `window_count.is_some()` filters out synthetic placeholders and only
        // includes empty workspaces explicitly reported by the compositor.
        // Without occupancy data (ext-workspace), every listed workspace is
        // shown: the compositor already filtered what to list.
        .filter(|ws| {
            !snapshot.occupancy_known
                || ws.occupied
                || (show_unoccupied && ws.window_count.is_some())
        })
        .map(|ws| ws.id)
        .collect();

    // Include active workspaces (supports multi-tag view).
    display_ids.extend(active_workspaces.iter());

    if include_all_output_active {
        // In all-output mode, show each output's current workspace even when it
        // is empty. Styling still uses `active_workspaces`, so only the
        // compositor's globally focused workspace gets the active class.
        for per_output in snapshot.per_output.values() {
            display_ids.extend(per_output.active_workspace.iter());
        }
    }

    display_ids
}

/// Update workspace indicators based on the current snapshot.
///
/// When `output_id` is provided (i.e. `filter_by_output = true`):
/// - Uses per-output workspace data if available.
/// - For Niri: only shows workspaces belonging to this output.
/// - For MangoWC: shows all workspaces with per-output window counts.
/// - For Hyprland: shows the workspace currently active on this output,
///   plus workspaces reported with windows on this output.
///
/// When `output_id` is not provided (i.e. `filter_by_output = false`), uses
/// global/all-output workspace data and also displays each output's current
/// workspace. Active styling still follows the compositor's globally focused
/// workspace.
///
/// `windows` is set when window icons are shown; `output_id` then also limits
/// icons to windows on this output.
#[allow(clippy::too_many_arguments)]
fn update_indicators(
    scroll_focus: &Rc<WorkspaceScrollFocus>,
    container: &GtkBox,
    ws_container: Option<&WorkspaceContainer>,
    indicators_cell: &Rc<RefCell<HashMap<i32, Indicator>>>,
    ids_cell: &Rc<RefCell<Vec<i32>>>,
    label_type: LabelType,
    separator: &str,
    snapshot: &WorkspaceServiceSnapshot,
    show_unoccupied: bool,
    is_vertical: bool,
    output_id: Option<&str>,
    windows: Option<(&WindowsMode, &[Window])>,
) {
    let windows_mode = windows.map(|(mode, _)| mode);
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

    let display_ids = collect_display_ids(
        workspaces,
        active_workspaces,
        snapshot,
        show_unoccupied,
        output_id.is_none(),
    );

    trace!(
        "workspace widget: occupied_ids={:?}, adding active={:?}",
        display_ids, active_workspaces
    );

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

    let new_ids: Vec<i32> = display_workspaces.iter().map(|ws| ws.id).collect();
    let ids_changed = new_ids != *ids_cell.borrow();

    if ids_changed {
        let mut indicators = indicators_cell.borrow_mut();
        if let Some(wsc) = ws_container {
            // Keep existing indicators (identity + animated extent); the
            // container diffs by ID and animates additions/removals.
            indicators.retain(|id, _| new_ids.contains(id));
            for ws in &display_workspaces {
                indicators.entry(ws.id).or_insert_with(|| {
                    create_single_indicator(scroll_focus, label_type, ws, is_vertical, windows_mode)
                });
            }
            wsc.set_children(
                new_ids
                    .iter()
                    .map(|id| (*id, indicators[id].root.clone()))
                    .collect(),
            );
        } else {
            create_indicators(
                scroll_focus,
                container,
                &mut indicators,
                label_type,
                separator,
                &display_workspaces,
                is_vertical,
                windows_mode,
            );
        }
        *ids_cell.borrow_mut() = new_ids;
    }

    // ── Styling — CSS class changes resize indicators; the container picks
    // the new sizes up in its next measure() and animates towards them. ──
    let indicators = indicators_cell.borrow();
    let mut long_indicator_changed = ids_changed; // New indicators may be long.
    for workspace in &display_workspaces {
        let Some(indicator) = indicators.get(&workspace.id) else {
            continue;
        };
        let root = &indicator.root;

        let mut urgent_icon_shown = false;
        if let Some((mode, windows)) = windows
            && let Some(parts) = &indicator.windows
        {
            let mut items =
                workspace_window_items(windows, workspace.id, output_id, mode.group_by_app);
            let hidden = limit_window_items(&mut items, mode.max_windows);
            let has_windows = !items.is_empty();
            urgent_icon_shown = items.iter().any(|item| item.urgent);
            parts.update(items, hidden, mode.icon_size);
            sync_exclusive_class(
                root,
                &[widget::WORKSPACE_HAS_WINDOWS],
                has_windows.then_some(widget::WORKSPACE_HAS_WINDOWS),
            );
        }

        // An urgent window's icon marks the urgency instead of the label.
        let target_class: Option<&str> = if workspace.active {
            Some(widget::ACTIVE)
        } else if workspace.urgent && !urgent_icon_shown {
            Some(state::URGENT)
        } else if workspace.occupied {
            Some(state::OCCUPIED)
        } else {
            None
        };
        sync_exclusive_class(
            root,
            &[widget::ACTIVE, state::OCCUPIED, state::URGENT],
            target_class,
        );

        // Update icon/name/index label.
        if let Some(label) = &indicator.label {
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
                LabelType::Name | LabelType::Index => {
                    let label_text =
                        workspace_label_text_for_orientation(label_type, workspace, is_vertical);
                    label.set_text(&label_text);
                    let now_long = label_text.chars().count() > 2;
                    let was_long = root.has_css_class(widget::WORKSPACE_INDICATOR_LONG);
                    if now_long != was_long {
                        if now_long {
                            root.add_css_class(widget::WORKSPACE_INDICATOR_LONG);
                            root.add_css_class(&ws_mw_class(workspace.id));
                        } else {
                            root.remove_css_class(widget::WORKSPACE_INDICATOR_LONG);
                            root.remove_css_class(&ws_mw_class(workspace.id));
                        }
                        long_indicator_changed = true;
                    }
                }
                LabelType::None => unreachable!(),
            }
        }

        let tooltip_text = build_tooltip(workspace, snapshot.occupancy_known);
        set_tooltip(root, Some(&tooltip_text));
    }
    drop(indicators);

    if let Some(wsc) = ws_container {
        if long_indicator_changed {
            wsc.update_long_indicator_css(&indicators_cell.borrow());
        }
        // Several active workspaces (MangoWC multi-tag view) keep their own
        // pill backgrounds.
        if highlight_slides(label_type) {
            let mut active = display_workspaces.iter().filter(|ws| ws.active);
            let target = match (active.next(), active.next()) {
                (Some(ws), None) => Some(ws.id),
                _ => None,
            };
            wsc.set_highlight(target);
        }
    }
}

/// Build tooltip text for a workspace.
///
/// Without occupancy data (`occupancy_known = false`, e.g. ext-workspace),
/// the window/occupancy part is omitted rather than claiming "Empty".
fn build_tooltip(workspace: &Workspace, occupancy_known: bool) -> String {
    let mut parts = Vec::new();

    // Negative indexes mean the compositor has no meaningful numeric label, so
    // show the name without an index prefix.
    let idx_str = workspace.idx.to_string();
    if workspace.idx < 0 {
        parts.push(tr!("Workspace {name}", name = workspace.name));
    } else if workspace.name != idx_str {
        parts.push(tr!(
            "Workspace {idx}: {name}",
            idx = workspace.idx,
            name = workspace.name
        ));
    } else {
        parts.push(tr!("Workspace {name}", name = workspace.name));
    }

    if workspace.active {
        parts.push(tr!("workspace" => "Active"));
    } else if workspace.urgent {
        parts.push(tr!("Urgent"));
    }

    if let Some(count) = workspace.window_count {
        parts.push(tr!("{n} window" | "{n} windows" % count));
    } else if workspace.occupied {
        parts.push(tr!("Has windows"));
    } else if occupancy_known {
        parts.push(tr!("workspace" => "Empty"));
    }

    parts.join(" • ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::workspace::PerOutputWorkspaces;
    use std::collections::{HashMap, HashSet};
    use toml::Value;

    fn make_widget_entry(name: &str, options: HashMap<String, Value>) -> WidgetEntry {
        WidgetEntry {
            name: name.to_string(),
            options,
        }
    }

    #[test]
    fn test_adjacent_workspace_id() {
        let ids = [1, 2, 3];

        assert_eq!(adjacent_workspace_id(&ids, Some(2), false), Some(1));
        assert_eq!(adjacent_workspace_id(&ids, Some(2), true), Some(3));
        assert_eq!(adjacent_workspace_id(&ids, Some(1), false), None);
        assert_eq!(adjacent_workspace_id(&ids, Some(3), true), None);
        assert_eq!(adjacent_workspace_id(&ids, None, true), None);
    }

    #[test]
    fn test_scroll_waits_for_focus_and_cancels_on_clear() {
        let state = WorkspaceScrollFocus::default();
        state.pending.set(Some(2));
        assert_eq!(take_focused_scroll(&state.pending, false), None);
        assert_eq!(state.pending.get(), Some(2));
        state.pending.set(Some(3));
        assert_eq!(take_focused_scroll(&state.pending, true), Some(3));
        assert_eq!(take_focused_scroll(&state.pending, true), None);
        state.pending.set(Some(4));
        state.clear();
        assert_eq!(take_focused_scroll(&state.pending, true), None);
    }

    #[test]
    fn test_scroll_state_reconciles_snapshots_and_cancellation() {
        let state = WorkspaceScrollFocus::default();
        state.update(Some(1));
        state.set_target(2);
        state.set_target(3);
        state.set_target(2);

        state.update(Some(2));
        assert_eq!(state.current(), Some(2));
        assert!(state.target.get().is_some());

        state.update(Some(3));
        assert_eq!(state.current(), Some(2));

        state.cancel_target();
        assert_eq!(state.current(), Some(3));

        state.target.set(Some((4, std::time::Instant::now())));
        assert_eq!(state.current(), Some(3));
        assert_eq!(state.target.get(), None);
    }

    #[test]
    fn test_click_supersedes_scroll_before_and_after_confirmation() {
        let state = WorkspaceScrollFocus::default();
        state.update(Some(1));
        state.set_target(2);
        state.update(Some(2));
        state.pending.set(Some(3));
        state.clear();
        state.set_target(4);
        assert_eq!(take_focused_scroll(&state.pending, true), None);
        for confirmed in [Some(2), Some(4)] {
            state.update(confirmed);
            assert_eq!(
                adjacent_workspace_id(&[1, 2, 3, 4, 5], state.current(), true),
                Some(5)
            );
        }
    }

    #[test]
    fn test_workspace_scroll_delta_uses_scroll_units() {
        assert_eq!(
            workspace_scroll_delta(gtk4::gdk::ScrollUnit::Wheel, 1.0),
            1.0
        );
        assert_eq!(
            workspace_scroll_delta(gtk4::gdk::ScrollUnit::Surface, 25.0),
            1.0
        );
    }

    #[test]
    fn test_workspace_config_default() {
        let entry = make_widget_entry("workspaces", HashMap::new());
        let config = WorkspacesConfig::from_entry(&entry);
        assert_eq!(config.label_type, LabelType::None);
        assert_eq!(config.separator, "");
        assert!(config.filter_by_output);
        assert!(!config.show_unoccupied);
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
        assert_eq!(config.label_type, LabelType::Name);
        assert_eq!(config.separator, "|");
    }

    #[test]
    fn test_workspace_config_name() {
        let mut options = HashMap::new();
        options.insert("label_type".to_string(), Value::String("name".to_string()));
        let entry = make_widget_entry("workspaces", options);
        let config = WorkspacesConfig::from_entry(&entry);
        assert_eq!(config.label_type, LabelType::Name);
    }

    #[test]
    fn test_workspace_config_index() {
        let mut options = HashMap::new();
        options.insert("label_type".to_string(), Value::String("index".to_string()));
        let entry = make_widget_entry("workspaces", options);
        let config = WorkspacesConfig::from_entry(&entry);
        assert_eq!(config.label_type, LabelType::Index);
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
        assert_eq!(LabelType::from_str("name"), LabelType::Name);
        assert_eq!(LabelType::from_str("numbers"), LabelType::Name);
        assert_eq!(LabelType::from_str("index"), LabelType::Index);
        assert_eq!(LabelType::from_str("none"), LabelType::None);
        assert_eq!(LabelType::from_str("unknown"), DEFAULT_LABEL_TYPE);
    }

    #[test]
    fn test_workspace_label_text_name_and_index() {
        let named = make_workspace(4, "Discord", false, true, false, Some(1));
        assert_eq!(workspace_label_text(LabelType::Name, &named), "Discord");
        assert_eq!(workspace_label_text(LabelType::Index, &named), "4");

        let named_without_index = make_workspace(0, "web", false, false, false, Some(0));
        assert_eq!(
            workspace_label_text(LabelType::Index, &named_without_index),
            "0"
        );

        let named_without_index = Workspace {
            idx: -1,
            ..named_without_index
        };
        assert_eq!(
            workspace_label_text(LabelType::Index, &named_without_index),
            "web"
        );
    }

    #[test]
    fn test_workspace_config_animate_default() {
        let entry = make_widget_entry("workspaces", HashMap::new());
        let config = WorkspacesConfig::from_entry(&entry);
        assert!(config.animate.is_none());
    }

    #[test]
    fn test_workspace_config_animate_disabled() {
        let mut options = HashMap::new();
        options.insert("animate".to_string(), Value::Boolean(false));
        let entry = make_widget_entry("workspaces", options);
        let config = WorkspacesConfig::from_entry(&entry);
        assert_eq!(config.animate, Some(false));
    }

    #[test]
    fn test_workspace_config_filter_by_output_disabled() {
        let mut options = HashMap::new();
        options.insert("filter_by_output".to_string(), Value::Boolean(false));
        let entry = make_widget_entry("workspaces", options);
        let config = WorkspacesConfig::from_entry(&entry);
        assert!(!config.filter_by_output);
    }

    #[test]
    fn test_workspace_config_show_unoccupied_enabled() {
        let mut options = HashMap::new();
        options.insert("show_unoccupied".to_string(), Value::Boolean(true));
        let entry = make_widget_entry("workspaces", options);
        let config = WorkspacesConfig::from_entry(&entry);
        assert!(config.show_unoccupied);
    }

    #[test]
    fn test_workspace_config_windows_defaults() {
        let entry = make_widget_entry("workspaces", HashMap::new());
        let config = WorkspacesConfig::from_entry(&entry);
        assert!(!config.show_windows);
        assert_eq!(config.max_windows, 3);
        assert!(!config.group_by_app);
        assert_eq!(config.icon_size, None);
    }

    #[test]
    fn test_workspace_config_windows_options() {
        let mut options = HashMap::new();
        options.insert("show_windows".to_string(), Value::Boolean(true));
        options.insert("max_windows".to_string(), Value::Integer(3));
        options.insert("group_by_app".to_string(), Value::Boolean(true));
        options.insert("icon_size".to_string(), Value::Integer(4));
        let entry = make_widget_entry("workspaces", options);
        let config = WorkspacesConfig::from_entry(&entry);
        assert!(config.show_windows);
        assert_eq!(config.max_windows, 3);
        assert!(config.group_by_app);
        assert_eq!(config.icon_size, Some(8));
    }

    // -- window icons --

    fn make_window(id: u64, app_id: &str, workspace_id: i32, output: &str) -> Window {
        Window {
            id,
            title: format!("title {id}"),
            app_id: app_id.to_string(),
            workspace_id: Some(workspace_id),
            output: Some(output.to_string()),
            ..Default::default()
        }
    }

    fn item_ids(items: &[WindowItem]) -> Vec<Vec<u64>> {
        items.iter().map(|item| item.window_ids.clone()).collect()
    }

    #[test]
    fn test_window_items_one_per_window_in_compositor_order() {
        let windows = vec![
            make_window(1, "foot", 1, "eDP-1"),
            make_window(2, "firefox", 2, "eDP-1"),
            make_window(3, "foot", 1, "eDP-1"),
        ];
        let items = workspace_window_items(&windows, 1, Some("eDP-1"), false);
        assert_eq!(item_ids(&items), vec![vec![1], vec![3]]);
        assert_eq!(items[0].key, WindowItemKey::Window(1));
        assert_eq!(items[0].tooltip, "Foot - title 1");
    }

    #[test]
    fn test_window_items_skip_same_tag_on_other_outputs() {
        let windows = vec![
            make_window(1, "foot", 2, "eDP-1"),
            make_window(2, "firefox", 2, "HDMI-A-1"),
        ];
        let items = workspace_window_items(&windows, 2, Some("eDP-1"), false);
        assert_eq!(item_ids(&items), vec![vec![1]]);

        // Without output filtering, every output's windows on the tag show.
        let items = workspace_window_items(&windows, 2, None, false);
        assert_eq!(item_ids(&items), vec![vec![1], vec![2]]);
    }

    #[test]
    fn test_window_items_include_multi_tag_and_outputless_windows() {
        let mut multi_tag = make_window(1, "foot", 1, "eDP-1");
        multi_tag.extra_workspace_ids = vec![3];
        let mut no_output = make_window(2, "firefox", 3, "eDP-1");
        no_output.output = None;
        let mut no_workspace = make_window(3, "mpv", 3, "eDP-1");
        no_workspace.workspace_id = None;

        let windows = vec![multi_tag, no_output, no_workspace];
        let items = workspace_window_items(&windows, 3, Some("eDP-1"), false);
        assert_eq!(item_ids(&items), vec![vec![1], vec![2]]);
    }

    #[test]
    fn test_window_items_group_by_app() {
        let mut focused = make_window(3, "foot", 1, "eDP-1");
        focused.is_focused = true;
        let mut urgent = make_window(2, "firefox", 1, "eDP-1");
        urgent.is_urgent = true;
        let windows = vec![make_window(1, "foot", 1, "eDP-1"), urgent, focused];

        let items = workspace_window_items(&windows, 1, Some("eDP-1"), true);
        assert_eq!(item_ids(&items), vec![vec![1, 3], vec![2]]);
        assert_eq!(items[0].key, WindowItemKey::App("foot".to_string()));
        assert_eq!(items[0].focused, Some(1));
        assert!(!items[0].urgent);
        assert_eq!(items[0].tooltip, "Foot • 2 windows");
        assert!(items[1].urgent);
        assert_eq!(items[1].tooltip, "Firefox - title 2");
    }

    #[test]
    fn test_window_item_click_target_cycles_grouped_windows() {
        let windows = vec![
            make_window(1, "foot", 1, "eDP-1"),
            make_window(2, "foot", 1, "eDP-1"),
        ];
        let mut item = workspace_window_items(&windows, 1, None, true).remove(0);
        assert_eq!(window_item_click_target(&item), Some(1));
        item.focused = Some(0);
        assert_eq!(window_item_click_target(&item), Some(2));
        item.focused = Some(1);
        assert_eq!(window_item_click_target(&item), Some(1));
    }

    fn make_items(count: u64) -> Vec<WindowItem> {
        let windows: Vec<_> = (1..=count)
            .map(|id| make_window(id, &format!("app{id}"), 1, "eDP-1"))
            .collect();
        workspace_window_items(&windows, 1, None, false)
    }

    #[test]
    fn test_limit_window_items() {
        let mut items = make_items(3);
        assert_eq!(limit_window_items(&mut items, 0), 0);
        assert_eq!(limit_window_items(&mut items, 3), 0);
        assert_eq!(items.len(), 3);

        let mut items = make_items(5);
        assert_eq!(limit_window_items(&mut items, 2), 3);
        assert_eq!(item_ids(&items), vec![vec![1], vec![2]]);
    }

    #[test]
    fn test_limit_window_items_keeps_focused_then_urgent_visible() {
        let mut items = make_items(5);
        items[4].focused = Some(0);
        assert_eq!(limit_window_items(&mut items, 3), 2);
        assert_eq!(item_ids(&items), vec![vec![1], vec![2], vec![5]]);

        let mut items = make_items(5);
        items[3].urgent = true;
        limit_window_items(&mut items, 3);
        assert_eq!(item_ids(&items), vec![vec![1], vec![2], vec![4]]);

        // A visible focused item in the last slot isn't pushed out.
        let mut items = make_items(5);
        items[2].focused = Some(0);
        items[4].urgent = true;
        limit_window_items(&mut items, 3);
        assert_eq!(item_ids(&items), vec![vec![1], vec![5], vec![3]]);
    }

    #[test]
    fn test_highlight_slides_stretched_and_settles_on_target() {
        let (a, b) = ((0.0, 20.0), (100.0, 140.0));
        for (from, to) in [(a, b), (b, a)] {
            let extra = highlight_extra_us(100.0);
            assert_eq!(highlight_edges(from, to, 0.0, extra), (from, false));
            let ((start, end), done) = highlight_edges(from, to, HIGHLIGHT_LEAD_US, extra);
            assert!(
                !done && end - start > 40.0,
                "stretched midway: {start}..{end}"
            );
            assert_eq!(
                highlight_edges(from, to, HIGHLIGHT_TRAIL_US + extra, extra),
                (to, true)
            );
        }
    }

    #[test]
    fn test_global_display_includes_each_outputs_current_workspace() {
        let active_workspaces = HashSet::from([2]);
        let workspaces = vec![
            make_workspace(2, "2", true, false, false, None),
            make_workspace(4, "4", false, true, false, Some(1)),
            make_workspace(8, "8", false, false, false, None),
        ];
        let snapshot = WorkspaceServiceSnapshot {
            active_workspace: active_workspaces.clone(),
            occupied_workspaces: HashSet::from([4]),
            window_counts: HashMap::from([(4, 1)]),
            workspaces: workspaces.clone(),
            per_output: HashMap::from([
                (
                    "eDP-1".to_string(),
                    PerOutputWorkspaces {
                        active_workspace: HashSet::from([2]),
                        workspaces: vec![],
                    },
                ),
                (
                    "HDMI-A-1".to_string(),
                    PerOutputWorkspaces {
                        active_workspace: HashSet::from([8]),
                        workspaces: vec![],
                    },
                ),
            ]),
            occupancy_known: true,
        };

        let display_ids =
            collect_display_ids(&workspaces, &active_workspaces, &snapshot, false, true);

        assert!(display_ids.contains(&2));
        assert!(display_ids.contains(&4));
        assert!(display_ids.contains(&8));
        assert_eq!(active_workspaces, HashSet::from([2]));
        assert!(!workspaces.iter().find(|ws| ws.id == 8).unwrap().active);
    }

    #[test]
    fn test_show_unoccupied_includes_reported_empty_workspaces() {
        let active_workspaces = HashSet::from([1]);
        let workspaces = vec![
            make_workspace(1, "1", true, false, false, Some(0)),
            make_workspace(2, "2", false, true, false, Some(1)),
            make_workspace(3, "3", false, false, false, Some(0)),
            make_workspace(4, "4", false, false, false, None),
        ];
        let snapshot = WorkspaceServiceSnapshot {
            active_workspace: active_workspaces.clone(),
            occupied_workspaces: HashSet::from([2]),
            window_counts: HashMap::from([(1, 0), (2, 1), (3, 0)]),
            workspaces: workspaces.clone(),
            per_output: HashMap::new(),
            occupancy_known: true,
        };

        let default_ids =
            collect_display_ids(&workspaces, &active_workspaces, &snapshot, false, false);
        assert_eq!(default_ids, HashSet::from([1, 2]));

        let show_unoccupied_ids =
            collect_display_ids(&workspaces, &active_workspaces, &snapshot, true, false);
        assert_eq!(show_unoccupied_ids, HashSet::from([1, 2, 3]));
        assert!(!show_unoccupied_ids.contains(&4));
    }

    #[test]
    fn test_unknown_occupancy_shows_all_listed_workspaces() {
        let active_workspaces = HashSet::from([1]);
        let workspaces = vec![
            make_workspace(1, "1", true, false, false, None),
            make_workspace(2, "2", false, false, false, None),
            make_workspace(3, "3", false, false, false, None),
        ];
        let snapshot = WorkspaceServiceSnapshot {
            active_workspace: active_workspaces.clone(),
            occupied_workspaces: HashSet::new(),
            window_counts: HashMap::new(),
            workspaces: workspaces.clone(),
            per_output: HashMap::new(),
            occupancy_known: false,
        };

        let ids = collect_display_ids(&workspaces, &active_workspaces, &snapshot, false, false);
        assert_eq!(ids, HashSet::from([1, 2, 3]));
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
        assert_eq!(build_tooltip(&ws, true), "Workspace 1 • Active • 3 windows");
    }

    #[test]
    fn test_build_tooltip_active_single_window() {
        let ws = make_workspace(2, "2", true, true, false, Some(1));
        assert_eq!(build_tooltip(&ws, true), "Workspace 2 • Active • 1 window");
    }

    #[test]
    fn test_build_tooltip_inactive_empty() {
        let ws = make_workspace(3, "3", false, false, false, None);
        assert_eq!(build_tooltip(&ws, true), "Workspace 3 • Empty");
    }

    #[test]
    fn test_build_tooltip_unknown_occupancy_omits_empty() {
        let ws = make_workspace(3, "3", false, false, false, None);
        assert_eq!(build_tooltip(&ws, false), "Workspace 3");
        let ws = make_workspace(1, "1", true, false, false, None);
        assert_eq!(build_tooltip(&ws, false), "Workspace 1 • Active");
    }

    #[test]
    fn test_build_tooltip_occupied_no_count() {
        let ws = make_workspace(4, "4", false, true, false, None);
        assert_eq!(build_tooltip(&ws, true), "Workspace 4 • Has windows");
    }

    #[test]
    fn test_build_tooltip_urgent() {
        let ws = make_workspace(5, "5", false, true, true, Some(2));
        assert_eq!(build_tooltip(&ws, true), "Workspace 5 • Urgent • 2 windows");
    }

    #[test]
    fn test_build_tooltip_custom_name() {
        let ws = make_workspace(1, "browser", true, true, false, Some(5));
        assert_eq!(
            build_tooltip(&ws, true),
            "Workspace 1: browser • Active • 5 windows"
        );
    }

    #[test]
    fn test_build_tooltip_named_workspace_hides_negative_id() {
        let ws = make_workspace(-1337, "web", true, true, false, Some(2));
        assert_eq!(
            build_tooltip(&ws, true),
            "Workspace web • Active • 2 windows"
        );
    }

    // -- container layout helpers --

    #[test]
    fn test_merge_ids_keeps_removed_in_place() {
        assert_eq!(merge_ids(&[1, 2, 3], &[1, 3]), vec![1, 2, 3]);
        assert_eq!(merge_ids(&[1, 2, 3], &[2, 3]), vec![1, 2, 3]);
        assert_eq!(merge_ids(&[1, 2, 3], &[1, 2]), vec![1, 2, 3]);
        // Swap: old shrinks out right before the newcomer.
        assert_eq!(merge_ids(&[1, 4], &[1, 5]), vec![1, 4, 5]);
        assert_eq!(merge_ids(&[1, 2], &[3]), vec![1, 2, 3]);
        assert_eq!(merge_ids(&[], &[1, 2]), vec![1, 2]);
        assert_eq!(merge_ids(&[3, 1, 2], &[1, 2, 3]), vec![1, 2, 3]);
    }

    #[test]
    fn test_slot_layout_is_contiguous_and_total_is_rounded_sum() {
        let slots = slot_layout([10.4, 10.4, 10.4].into_iter());
        assert_eq!(slots, vec![(0, 10), (10, 11), (21, 10)]);

        // Switch mid-animation: one grows, one shrinks by the same amount.
        // The total must stay fixed for every interpolation step.
        let (inactive, active, gap) = (17.0, 24.0, 6.0);
        for step in 0..=20 {
            let k = step as f64 / 20.0;
            let a = active + gap + (inactive - active) * k;
            let b = inactive + gap + (active - inactive) * k;
            let slots = slot_layout([a, b, inactive].into_iter());
            let (pos, size) = *slots.last().unwrap();
            assert_eq!(pos + size, 70, "step {step}");
            assert!(slots.windows(2).all(|w| w[0].0 + w[0].1 == w[1].0));
        }
    }
}
