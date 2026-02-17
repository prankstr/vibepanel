//! Workspaces widget - displays workspace indicators.
//!
//! Shows occupied/active workspaces with visual indicators and CSS classes.
//! Clicking on a workspace indicator switches to that workspace.
//!
//! For `LabelType::None` (minimal mode), uses a custom `WorkspaceContainer`
//! widget that reports a constant width to prevent neighboring widgets from
//! shifting during circle-to-pill CSS transitions. Indicators are split into
//! two groups (left-anchored and right-anchored) with the gap between them
//! absorbing subpixel rounding drift during animations.

use std::cell::RefCell;
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
// WorkspaceContainer — minimal custom widget that reports a constant width
// and splits children into two groups (left/right anchored) to absorb
// rounding drift during CSS min-width transitions.
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
        if imp.target_width.get() == width {
            return;
        }
        imp.target_width.set(width);

        let current = imp.current_width.get();
        if current == 0.0 {
            // First call (no previous width) — snap immediately, no animation.
            imp.current_width.set(width as f64);
            self.queue_resize();
            return;
        }

        // Start (or restart) animation from current interpolated position.
        imp.anim_start_width.set(current);
        // anim_start_time will be set on the first tick (we don't have a
        // frame clock reference here outside of a tick callback).
        imp.anim_start_time.set(None);

        if !imp.animating.get() {
            imp.animating.set(true);
            let widget = self.clone();
            self.add_tick_callback(move |_w, frame_clock| {
                let imp = widget.imp();
                let now = frame_clock.frame_time(); // microseconds

                // Lazily record the start time on the first tick.
                let start_time = match imp.anim_start_time.get() {
                    Some(t) => t,
                    None => {
                        imp.anim_start_time.set(Some(now));
                        now
                    }
                };

                let elapsed = now - start_time;
                let target = imp.target_width.get() as f64;

                if elapsed >= INDICATOR_ANIM_DURATION_US {
                    // Animation complete.
                    imp.current_width.set(target);
                    imp.animating.set(false);
                    imp.frozen_left_count.set(None);
                    widget.queue_resize();
                    return glib::ControlFlow::Break;
                }

                // Ease-out quadratic interpolation (decelerating).
                let t = elapsed as f64 / INDICATOR_ANIM_DURATION_US as f64;
                let eased = 1.0 - (1.0 - t).powi(2);
                let start = imp.anim_start_width.get();
                let interpolated = start + (target - start) * eased;
                imp.current_width.set(interpolated);
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

    /// Freeze the current left-group size for use during removal animations.
    fn freeze_left_count(&self) -> usize {
        let children = self.imp().children.borrow();
        let n = children.len();
        let left_count = if n < 2 {
            n
        } else {
            let active_css = crate::styles::widget::ACTIVE;
            let active_idx = children.iter().position(|c| c.has_css_class(active_css));
            compute_left_count(n, active_idx)
        };
        self.imp().frozen_left_count.set(Some(left_count));
        left_count
    }
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
            // Default to Icons for any other value including "icons"
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
/// Used in both `compute_target_width` and CSS generation (bar.rs).
pub(crate) const INDICATOR_INACTIVE_MULT: f64 = 0.7;
/// Multiplier for active indicator width relative to widget_height.
/// Used in both `compute_target_width` and CSS generation (bar.rs).
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
        let widget_height = sizes.widget_height;
        let content_gap = sizes.widget_content_gap;

        // When animated, use a WorkspaceContainer that reports constant
        // width and drives enter/exit animations. Otherwise, indicators
        // go directly in the content GtkBox.
        let ws_container: Option<WorkspaceContainer> = if animate {
            let container = WorkspaceContainer::new();
            container.set_gap(content_gap as i32);
            base.content().append(&container);
            Some(container)
        } else {
            None
        };

        let content_box = base.content().clone();

        // State shared with the callback (callback owns these via Rc).
        let workspace_labels: Rc<RefCell<HashMap<i32, Widget>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let current_ids = Rc::new(RefCell::new(Vec::new()));
        let separator = config.separator;

        // Clone output_id for the debug message
        let output_id_debug = output_id.clone();

        // Connect to workspace service.
        // The callback owns its own Rc clones of the state.
        let workspace_callback_id = WorkspaceService::global().connect(move |snapshot| {
            update_indicators(
                &content_box,
                ws_container.as_ref(),
                &workspace_labels,
                &current_ids,
                label_type,
                animate,
                &separator,
                snapshot,
                output_id.as_deref(),
                widget_height,
                content_gap,
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
fn create_single_indicator(label_type: LabelType, animate: bool, workspace: &Workspace) -> Widget {
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
        // Minimal mode: use GtkBox to avoid font-metric intrinsic sizing.
        // CSS min-width/min-height controls the exact circle/pill dimensions.
        let dot = GtkBox::new(gtk4::Orientation::Horizontal, 0);
        dot.add_css_class(widget::WORKSPACE_INDICATOR);
        dot.add_css_class(widget::WORKSPACE_INDICATOR_MINIMAL);
        dot.add_css_class(state::CLICKABLE);
        if !animate {
            dot.add_css_class(widget::WORKSPACE_NO_ANIMATE);
        }
        dot.set_valign(Align::Center);
        dot.add_controller(gesture);
        dot.upcast()
    } else {
        // Icons or Numbers mode: use Label directly.
        let label_text = match label_type {
            LabelType::Icons => ICON_EMPTY,
            LabelType::Numbers => &workspace.name,
            LabelType::None => unreachable!(),
        };
        let label = Label::new(Some(label_text));
        label.add_css_class(widget::WORKSPACE_INDICATOR);
        label.add_css_class(state::CLICKABLE);
        if !animate {
            label.add_css_class(widget::WORKSPACE_NO_ANIMATE);
        }
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
    animate: bool,
    separator: &str,
    workspaces: &[Workspace],
) {
    clear_indicators(container, ws_container, labels_cell, ids_cell);

    let mut labels = labels_cell.borrow_mut();
    let mut ids = ids_cell.borrow_mut();

    for (i, workspace) in workspaces.iter().enumerate() {
        let indicator = create_single_indicator(label_type, animate, workspace);

        labels.insert(workspace.id, indicator.clone());
        if let Some(wsc) = ws_container {
            wsc.add_child(&indicator);
        } else {
            container.append(&indicator);
        }
        ids.push(workspace.id);

        // Add separator if not the last workspace (only for non-minimal modes)
        if ws_container.is_none() && i < workspaces.len() - 1 && !separator.is_empty() {
            let sep = Label::new(Some(separator));
            sep.set_valign(Align::Center);
            sep.add_css_class(widget::WORKSPACE_SEPARATOR);
            container.append(&sep);
        }
    }
}

/// Compute the steady-state target width for the workspace container.
fn compute_target_width(
    widget_height: u32,
    active_count: i32,
    inactive_count: i32,
    gap: i32,
) -> i32 {
    let wh = widget_height as f64;
    // ceil() is intentionally conservative: CSS calc() may round fractional
    // min-width values differently (e.g., 17.5 → 17 vs 18). Being 1px too
    // wide is invisible; being 1px too narrow could clip. The two-group
    // layout gap absorbs any surplus from this conservative rounding.
    let pill_w = (wh * INDICATOR_ACTIVE_MULT).ceil();
    let circle_w = (wh * INDICATOR_INACTIVE_MULT).ceil();
    let indicators_w = active_count as f64 * pill_w + inactive_count as f64 * circle_w;
    let n = active_count + inactive_count;
    indicators_w.ceil() as i32 + (n - 1).max(0) * gap
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
    animate: bool,
    separator: &str,
    snapshot: &WorkspaceServiceSnapshot,
    output_id: Option<&str>,
    widget_height: u32,
    content_gap: u32,
) {
    // Get the workspace list to use - either per-output or global
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
            // No per-output data available, fall back to global
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
        // No output_id specified, use global data
        (&snapshot.workspaces, &snapshot.active_workspace, "global")
    };

    trace!(
        "workspace widget: source={}, output_id={:?}, active_workspaces={:?}",
        source, output_id, active_workspaces
    );

    // Determine which workspaces to display (occupied + active)
    // Use the workspace's own occupied flag (which reflects per-output state if available)
    let mut display_ids: std::collections::HashSet<i32> = workspaces
        .iter()
        .filter(|ws| ws.occupied)
        .map(|ws| ws.id)
        .collect();

    trace!(
        "workspace widget: occupied_ids={:?}, adding active={:?}",
        display_ids, active_workspaces
    );

    // Add all active workspaces to display (supports multi-tag view)
    display_ids.extend(active_workspaces.iter());

    // Filter to only display relevant workspaces
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

    // The new set of workspace IDs that should be visible (in order).
    let new_ids: Vec<i32> = display_workspaces.iter().map(|ws| ws.id).collect();

    // Determine if the set of visible workspaces changed.
    let ids_changed = new_ids != *ids_cell.borrow();

    if ids_changed {
        // Three mutually exclusive paths handle the update:
        //   A. Removal-only (minimal mode): surgical removal + frozen layout + width animation
        //   B. Additions (minimal mode): full recreate + collapsed CSS class for grow-in
        //   C. Non-minimal mode: full recreate, no animations
        // After any path, the shared tail updates container target width and indicator CSS.
        // Snapshot old IDs before modifying so we can diff.
        let old_ids: HashSet<i32> = ids_cell.borrow().iter().copied().collect();
        let new_ids_set: HashSet<i32> = new_ids.iter().copied().collect();
        let has_additions = new_ids_set.iter().any(|id| !old_ids.contains(id));

        if let Some(wsc) = ws_container {
            if !has_additions && !old_ids.is_empty() {
                // Removal-only: surgically remove departed indicators so
                // surviving widgets keep their GTK identity and position.
                // The container width animation smoothly closes the gap.

                // Freeze the two-group split BEFORE removing children so
                // e.g. ws2 doesn't jump groups when active changes from
                // ws3 to ws1.
                let frozen = wsc.freeze_left_count();

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

                    if left_removed > 0 {
                        wsc.imp().frozen_left_count.set(Some(frozen - left_removed));
                    }
                }
            } else {
                // Additions (possibly with removals) — full recreate.
                // New indicators get the grow-in animation.
                create_indicators(
                    container,
                    Some(wsc),
                    labels_cell,
                    ids_cell,
                    label_type,
                    animate,
                    separator,
                    &display_workspaces,
                );
                if !old_ids.is_empty() {
                    let labels = labels_cell.borrow();
                    for &id in &new_ids {
                        if old_ids.contains(&id) {
                            continue;
                        }
                        if let Some(indicator) = labels.get(&id) {
                            indicator.add_css_class(widget::WORKSPACE_NO_ANIMATE);
                            indicator.add_css_class(widget::WORKSPACE_INDICATOR_COLLAPSED);
                            let ind = indicator.clone();
                            glib::idle_add_local_once(move || {
                                ind.remove_css_class(widget::WORKSPACE_NO_ANIMATE);
                                ind.remove_css_class(widget::WORKSPACE_INDICATOR_COLLAPSED);
                            });
                        }
                    }
                }
            }
        } else {
            // Non-minimal mode — full recreate, no animations.
            create_indicators(
                container,
                None,
                labels_cell,
                ids_cell,
                label_type,
                animate,
                separator,
                &display_workspaces,
            );
        }
    }

    // Update the container's target width for minimal/animated mode.
    // This prevents neighboring widgets from shifting during transitions.
    if let Some(wsc) = ws_container {
        let active_count = display_workspaces.iter().filter(|ws| ws.active).count() as i32;
        let inactive_count = display_workspaces.len() as i32 - active_count;
        let total = compute_target_width(
            widget_height,
            active_count,
            inactive_count,
            content_gap as i32,
        );
        wsc.set_target_width(total);
    }

    // Update indicator styling (for all indicators, including just-created ones).
    let labels = labels_cell.borrow();
    for workspace in &display_workspaces {
        let Some(indicator) = labels.get(&workspace.id) else {
            continue;
        };

        // Remove existing state classes
        indicator.remove_css_class(widget::ACTIVE);
        indicator.remove_css_class(state::OCCUPIED);
        indicator.remove_css_class(state::URGENT);

        // Update icon text if using Labels (Icons/Numbers mode)
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

        // Add appropriate state class (mutually exclusive)
        if workspace.active {
            indicator.add_css_class(widget::ACTIVE);
        } else if workspace.occupied {
            indicator.add_css_class(state::OCCUPIED);
        } else if workspace.urgent {
            indicator.add_css_class(state::URGENT);
        }

        // Set tooltip with workspace info
        let tooltip_text = build_tooltip(workspace);
        TooltipManager::global().set_styled_tooltip(indicator, &tooltip_text);
    }
}

/// Build tooltip text for a workspace.
fn build_tooltip(workspace: &Workspace) -> String {
    let mut parts = Vec::new();

    // Workspace identifier - show both ID and name if they differ
    // (Niri can have custom workspace names separate from the index)
    let id_str = workspace.id.to_string();
    if workspace.name != id_str {
        // Custom name - show "Workspace N: Name"
        parts.push(format!("Workspace {}: {}", workspace.id, workspace.name));
    } else {
        // No custom name - just show "Workspace N"
        parts.push(format!("Workspace {}", workspace.name));
    }

    // State
    if workspace.active {
        parts.push("Active".to_string());
    } else if workspace.urgent {
        parts.push("Urgent".to_string());
    }

    // Window count
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

    // -- compute_target_width tests --

    #[test]
    fn test_compute_target_width_single_active() {
        // widget_height=30, 1 active + 0 inactive, gap=4
        // pill_w = 30, total = 30
        let total = compute_target_width(30, 1, 0, 4);
        assert_eq!(total, 30);
    }

    #[test]
    fn test_compute_target_width_single_inactive() {
        // widget_height=30, 0 active + 1 inactive, gap=4
        // circle_w = ceil(30 * 0.7) = ceil(21.0) = 21, total = 21
        let total = compute_target_width(30, 0, 1, 4);
        assert_eq!(total, 21);
    }

    #[test]
    fn test_compute_target_width_mixed() {
        // widget_height=30, 1 active + 2 inactive, gap=4
        // pill_w = 30, circle_w = 21
        // indicators_w = 30 + 2*21 = 72, gaps = (3-1)*4 = 8, total = 80
        let total = compute_target_width(30, 1, 2, 4);
        assert_eq!(total, 80);
    }

    #[test]
    fn test_compute_target_width_multiple_active() {
        // widget_height=30, 2 active + 1 inactive, gap=4
        // indicators_w = 2*30 + 21 = 81, gaps = (3-1)*4 = 8, total = 89
        let total = compute_target_width(30, 2, 1, 4);
        assert_eq!(total, 89);
    }

    #[test]
    fn test_compute_target_width_empty() {
        let total = compute_target_width(30, 0, 0, 4);
        assert_eq!(total, 0);
    }

    #[test]
    fn test_compute_target_width_no_gap() {
        // widget_height=30, 1 active + 2 inactive, gap=0
        // indicators_w = 30 + 2*21 = 72, gaps = 0, total = 72
        let total = compute_target_width(30, 1, 2, 0);
        assert_eq!(total, 72);
    }

    #[test]
    fn test_compute_target_width_fractional_rounding() {
        // widget_height=25: circle_w = ceil(25 * 0.7) = ceil(17.5) = 18
        // pill_w = ceil(25 * 1.0) = 25
        // 1 active + 2 inactive, gap=4
        // indicators_w = 25 + 2*18 = 61, gaps = (3-1)*4 = 8, total = 69
        let total = compute_target_width(25, 1, 2, 4);
        assert_eq!(total, 69);
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
}
