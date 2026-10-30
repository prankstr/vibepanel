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
    EventControllerScrollFlags, GestureClick, Label, Overlay, Widget,
};
use gtk4_layer_shell::{KeyboardMode, LayerShell};
use tr::tr;
use tracing::{debug, trace, warn};
use vibepanel_core::config::WidgetEntry;

use crate::services::callbacks::CallbackId;
use crate::services::compositor::CompositorManager;
use crate::services::config_manager::ConfigManager;
use crate::services::tooltip::TooltipManager;
use crate::services::workspace::{Workspace, WorkspaceService, WorkspaceServiceSnapshot};
use crate::styles::{state, widget};
use crate::widgets::WidgetConfig;
use crate::widgets::base::BaseWidget;
use crate::widgets::ripple::{trigger_ripple_from_gesture, wrap_with_ripple};
use crate::widgets::warn_unknown_options;

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
            self.obj().remove_long_mw_provider();
            for e in self.entries.borrow_mut().drain(..) {
                e.widget.unparent();
            }
        }
    }

    impl WidgetImpl for WorkspaceContainer {
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

    fn remove_long_mw_provider(&self) {
        if let Some(provider) = self.imp().long_mw_css_provider.borrow_mut().take()
            && let Some(display) = gtk4::gdk::Display::default()
        {
            gtk4::style_context_remove_provider_for_display(&display, &provider);
        }
    }

    /// Load per-instance, display-level CSS with computed `min-width` rules
    /// for each long indicator (active/inactive delta matches short indicators).
    fn update_long_indicator_css(&self, labels: &HashMap<i32, Widget>) {
        use std::fmt::Write;

        let sizes = ConfigManager::global().theme_sizes();
        let wh = sizes.widget_height as f64;
        let delta = (wh * (INDICATOR_ACTIVE_MULT - INDICATOR_INACTIVE_MULT)).round();

        let mut css = String::new();
        for (&id, indicator) in labels {
            if !indicator.has_css_class(widget::WORKSPACE_INDICATOR_LONG) {
                continue;
            }

            // Measure the label's natural width (content box, excludes padding).
            // The indicator is an Overlay wrapping the label.
            let label_width = indicator
                .downcast_ref::<Overlay>()
                .and_then(|o| o.child())
                .map(|child| {
                    let (_, nat, _, _) = child.measure(gtk4::Orientation::Horizontal, -1);
                    nat
                })
                .unwrap_or(0);

            let short_inactive = (wh * INDICATOR_INACTIVE_MULT).round() as i32;
            let inactive_mw = label_width.max(short_inactive);
            let active_mw = inactive_mw + delta as i32;

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

/// Switch/addition duration (linear). Vertical growth reads faster than
/// horizontal at the same duration, so it gets a shorter one.
const SWITCH_DURATION_US: f64 = 200_000.0;
const SWITCH_DURATION_VERTICAL_US: f64 = 150_000.0;
/// Removal (gap closing) duration, ease-out, both orientations.
const REMOVAL_DURATION_US: f64 = 200_000.0;

fn ease_out_quad(t: f64) -> f64 {
    1.0 - (1.0 - t).powi(2)
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
}

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

        Self {
            label_type,
            separator,
            animate,
            filter_by_output,
            show_unoccupied,
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
        }
    }
}

/// Workspaces widget that displays workspace indicators.
pub struct WorkspacesWidget {
    /// Shared base widget container.
    base: BaseWidget,
    /// Callback ID for WorkspaceService, used to disconnect on drop.
    workspace_callback_id: CallbackId,
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

        let label_type = config.label_type;
        let filter_by_output = config.filter_by_output;
        let show_unoccupied = config.show_unoccupied;
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
                    TooltipManager::global().cancel_and_hide();
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

        let workspace_callback_id = WorkspaceService::global().connect(move |snapshot| {
            update_indicators(
                &focus_for_updates,
                &content_box,
                ws_container.as_ref(),
                &workspace_labels,
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
            );

            let labels = workspace_labels.borrow();
            focus_for_updates.update(current_ids.borrow().iter().copied().find(|id| {
                labels
                    .get(id)
                    .is_some_and(|indicator| indicator.has_css_class(widget::ACTIVE))
            }));
        });

        debug!(
            "WorkspacesWidget created (output_id: {:?})",
            output_id_debug
        );
        Self {
            base,
            workspace_callback_id,
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
) -> Widget {
    let workspace_id = workspace.id;

    let (overlay, ripple_handle, is_long) = if label_type == LabelType::None {
        // GtkBox avoids font-metric intrinsic sizing; CSS controls dimensions.
        let dot = GtkBox::new(gtk4::Orientation::Horizontal, 0);
        let (o, rh) = wrap_with_ripple(&dot);
        (o, rh, false)
    } else {
        let label_text = workspace_label_text_for_orientation(label_type, workspace, is_vertical);
        let label = Label::new(Some(&label_text));
        // Optical centering
        label.set_xalign(if is_vertical { 0.58 } else { 0.55 });
        label.set_ellipsize(EllipsizeMode::End);
        label.set_single_line_mode(true);
        let (o, rh) = wrap_with_ripple(&label);
        (o, rh, label_text.chars().count() > 2)
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
        TooltipManager::global().cancel_and_hide();
        scroll_focus.clear();
        scroll_focus.set_target(workspace_id);
        debug!("Switching to workspace {}", workspace_id);
        WorkspaceService::global().switch_workspace(workspace_id);
    });
    overlay.add_controller(gesture);

    overlay.upcast()
}

/// Recreate all indicators in the plain (non-animated) GtkBox.
fn create_indicators(
    scroll_focus: &Rc<WorkspaceScrollFocus>,
    container: &GtkBox,
    labels: &mut HashMap<i32, Widget>,
    label_type: LabelType,
    separator: &str,
    workspaces: &[Workspace],
    is_vertical: bool,
) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
    labels.clear();

    for (i, workspace) in workspaces.iter().enumerate() {
        let indicator = create_single_indicator(scroll_focus, label_type, workspace, is_vertical);
        labels.insert(workspace.id, indicator.clone());
        container.append(&indicator);

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
#[allow(clippy::too_many_arguments)]
fn update_indicators(
    scroll_focus: &Rc<WorkspaceScrollFocus>,
    container: &GtkBox,
    ws_container: Option<&WorkspaceContainer>,
    labels_cell: &Rc<RefCell<HashMap<i32, Widget>>>,
    ids_cell: &Rc<RefCell<Vec<i32>>>,
    label_type: LabelType,
    separator: &str,
    snapshot: &WorkspaceServiceSnapshot,
    show_unoccupied: bool,
    is_vertical: bool,
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
        let mut labels = labels_cell.borrow_mut();
        if let Some(wsc) = ws_container {
            // Keep existing indicators (identity + animated extent); the
            // container diffs by ID and animates additions/removals.
            labels.retain(|id, _| new_ids.contains(id));
            for ws in &display_workspaces {
                labels.entry(ws.id).or_insert_with(|| {
                    create_single_indicator(scroll_focus, label_type, ws, is_vertical)
                });
            }
            wsc.set_children(new_ids.iter().map(|id| (*id, labels[id].clone())).collect());
        } else {
            create_indicators(
                scroll_focus,
                container,
                &mut labels,
                label_type,
                separator,
                &display_workspaces,
                is_vertical,
            );
        }
        *ids_cell.borrow_mut() = new_ids;
    }

    // ── Styling — CSS class changes resize indicators; the container picks
    // the new sizes up in its next measure() and animates towards them. ──
    let labels = labels_cell.borrow();
    let mut long_indicator_changed = ids_changed; // New indicators may be long.
    for workspace in &display_workspaces {
        let Some(indicator) = labels.get(&workspace.id) else {
            continue;
        };

        // Only toggle classes that changed — remove+re-add of the same
        // class causes GTK's style system to return stale measure() values.
        let target_class: Option<&str> = if workspace.active {
            Some(widget::ACTIVE)
        } else if workspace.urgent {
            Some(state::URGENT)
        } else if workspace.occupied {
            Some(state::OCCUPIED)
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

        // Update icon/name/index label.
        // The indicator is an Overlay wrapping the inner label.
        if let Some(label) = (label_type != LabelType::None)
            .then(|| {
                indicator
                    .downcast_ref::<Overlay>()
                    .and_then(|o| o.child())
                    .and_then(|w| w.downcast::<Label>().ok())
            })
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
                LabelType::Name | LabelType::Index => {
                    let label_text =
                        workspace_label_text_for_orientation(label_type, workspace, is_vertical);
                    label.set_text(&label_text);
                    let now_long = label_text.chars().count() > 2;
                    let was_long = indicator.has_css_class(widget::WORKSPACE_INDICATOR_LONG);
                    if now_long != was_long {
                        if now_long {
                            indicator.add_css_class(widget::WORKSPACE_INDICATOR_LONG);
                            indicator.add_css_class(&ws_mw_class(workspace.id));
                        } else {
                            indicator.remove_css_class(widget::WORKSPACE_INDICATOR_LONG);
                            indicator.remove_css_class(&ws_mw_class(workspace.id));
                        }
                        long_indicator_changed = true;
                    }
                }
                LabelType::None => unreachable!(),
            }
        }

        let tooltip_text = build_tooltip(workspace);
        TooltipManager::global().set_styled_tooltip(indicator, &tooltip_text);
    }
    drop(labels);

    if long_indicator_changed && let Some(wsc) = ws_container {
        wsc.update_long_indicator_css(&labels_cell.borrow());
    }
}

/// Build tooltip text for a workspace.
fn build_tooltip(workspace: &Workspace) -> String {
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
    } else {
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

    #[test]
    fn test_build_tooltip_named_workspace_hides_negative_id() {
        let ws = make_workspace(-1337, "web", true, true, false, Some(2));
        assert_eq!(build_tooltip(&ws), "Workspace web • Active • 2 windows");
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
