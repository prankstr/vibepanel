//! Bar window implementation using GTK4 and layer-shell.

use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, GestureClick, Overlay, gdk};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tracing::{debug, info, warn};

use vibepanel_core::config::{BarPosition, WidgetEntry, WidgetOrGroup};
use vibepanel_core::{Config, ThemePalette};

use crate::popover_tracker::PopoverTracker;
use crate::sectioned_bar::SectionedBar;
use crate::services::config_manager::{ConfigManager, ThemeCallbackGuard};
use crate::services::tooltip::TooltipManager;
use crate::styles::{class, state, widget as style_widget};
use crate::widgets::{
    self, BarState, CalendarWeatherPopoverBinding, ClockConfig, EdgeInteraction, MenuHandle,
    PopoverKind, QuickSettingsConfig, RippleHandle, SystemPopoverBinding, WidgetConfig,
    WidgetFactory, popover_kind_for, trigger_ripple_from_gesture,
};

/// Total bar window/content height reserved for the shell.
///
/// When the bar background is visible, user padding contributes on both sides.
/// In transparent/islands mode, CSS still applies the screen-edge padding so
/// widgets are visually offset from the edge, but suppresses center-side
/// padding to keep the reserved area tighter than visible-bar mode.
pub(crate) fn rendered_bar_height(config: &Config) -> i32 {
    if config.bar.background_opacity > 0.0 {
        config.bar.size as i32 + 2 * config.bar.padding as i32
    } else {
        config.bar.size as i32 + config.bar.padding as i32
    }
}

/// Returns the bar's content-flow orientation: horizontal for top/bottom
/// bars, vertical for left/right bars. This is the orientation passed to
/// `SectionedBar` and `BaseWidget`, not the layer-shell cross-axis orientation
/// returned by `shell_orientation_for`.
fn bar_flow_orientation_for(position: BarPosition) -> gtk4::Orientation {
    if position.is_vertical() {
        gtk4::Orientation::Vertical
    } else {
        gtk4::Orientation::Horizontal
    }
}

/// Returns the layer-shell cross-axis orientation: the axis along which the
/// bar window stretches to fill the monitor (e.g. a top bar stretches
/// horizontally, so this returns Vertical — the shell spacer flows
/// perpendicular to the bar's content flow).
fn shell_orientation_for(position: BarPosition) -> gtk4::Orientation {
    if position.is_vertical() {
        gtk4::Orientation::Horizontal
    } else {
        gtk4::Orientation::Vertical
    }
}

fn screen_margin_spacer_size(position: BarPosition, margin: i32) -> (i32, i32) {
    if position.is_vertical() {
        (margin, -1)
    } else {
        (-1, margin)
    }
}

fn screen_margin_spacer_precedes_bar(position: BarPosition) -> bool {
    matches!(position, BarPosition::Top | BarPosition::Left)
}

#[derive(Clone)]
struct EdgeClickTarget {
    widget: gtk4::Widget,
    interaction: EdgeInteraction,
}

type EdgeClickTargets = Rc<RefCell<Vec<EdgeClickTarget>>>;

fn register_edge_target(
    targets: &EdgeClickTargets,
    widget: &gtk4::Widget,
    interaction: Option<EdgeInteraction>,
) {
    if let Some(interaction) = interaction {
        targets.borrow_mut().push(EdgeClickTarget {
            widget: widget.clone(),
            interaction,
        });
    }
}

fn click_inside_edge_target(targets: &[EdgeClickTarget], picked: gtk4::Widget) -> bool {
    let mut current = Some(picked);
    while let Some(widget) = current {
        if targets.iter().any(|target| target.widget == widget) {
            return true;
        }
        current = widget.parent();
    }
    false
}

fn point_projects_to_edge_target(
    position: BarPosition,
    bounds_x: f32,
    bounds_y: f32,
    bounds_width: f32,
    bounds_height: f32,
    x: f32,
    y: f32,
) -> bool {
    let bounds_right = bounds_x + bounds_width;
    let bounds_bottom = bounds_y + bounds_height;
    let (in_flow, on_edge_side) = match position {
        BarPosition::Top => (x >= bounds_x && x <= bounds_right, y < bounds_y),
        BarPosition::Bottom => (x >= bounds_x && x <= bounds_right, y > bounds_bottom),
        BarPosition::Left => (y >= bounds_y && y <= bounds_bottom, x < bounds_x),
        BarPosition::Right => (y >= bounds_y && y <= bounds_bottom, x > bounds_right),
    };

    in_flow && on_edge_side
}

fn edge_target_at(
    targets: &[EdgeClickTarget],
    reference: &gtk4::Widget,
    position: BarPosition,
    x: f64,
    y: f64,
) -> Option<usize> {
    let x = x as f32;
    let y = y as f32;

    for (idx, target) in targets.iter().enumerate() {
        if !target.widget.is_visible() {
            continue;
        }

        let Some(mut rect) = target.widget.compute_bounds(reference) else {
            continue;
        };

        let mut ancestor = target.widget.parent();
        let mut clipped_out = false;
        while let Some(widget) = ancestor {
            if widget.overflow() == gtk4::Overflow::Hidden {
                match widget
                    .compute_bounds(reference)
                    .and_then(|bounds| rect.intersection(&bounds))
                {
                    Some(clipped) => rect = clipped,
                    None => {
                        clipped_out = true;
                        break;
                    }
                }
            }

            if widget == *reference {
                break;
            }
            ancestor = widget.parent();
        }

        if clipped_out || rect.width() <= 0.0 || rect.height() <= 0.0 {
            continue;
        }

        if point_projects_to_edge_target(
            position,
            rect.x(),
            rect.y(),
            rect.width(),
            rect.height(),
            x,
            y,
        ) {
            return Some(idx);
        }
    }

    None
}

fn apply_edge_hover(targets: &[EdgeClickTarget], active_idx: Option<usize>) {
    for (idx, target) in targets.iter().enumerate() {
        if Some(idx) == active_idx {
            target.widget.add_css_class(state::EDGE_HOVER);
        } else {
            target.widget.remove_css_class(state::EDGE_HOVER);
        }
    }
}

fn install_edge_click_handler(
    outer_box: &gtk4::Box,
    position: BarPosition,
    targets: &EdgeClickTargets,
) {
    let gesture = GestureClick::new();
    gesture.set_button(gdk::BUTTON_PRIMARY);
    gesture.set_propagation_phase(gtk4::PropagationPhase::Capture);

    let targets_for_cb = targets.clone();
    let outer_box_weak = outer_box.downgrade();
    gesture.connect_pressed(move |gesture, _n_press, x, y| {
        let Some(outer_box) = outer_box_weak.upgrade() else {
            return;
        };
        let outer_widget = outer_box.upcast_ref::<gtk4::Widget>();

        let interaction = {
            let targets = targets_for_cb.borrow();
            if let Some(picked) = outer_widget.pick(x, y, gtk4::PickFlags::DEFAULT)
                && click_inside_edge_target(&targets, picked)
            {
                return;
            }

            let Some(target_idx) = edge_target_at(&targets, outer_widget, position, x, y) else {
                return;
            };
            targets[target_idx].interaction.clone()
        };

        TooltipManager::global().cancel_and_hide();
        // Toggle via the popover registry path. MenuHandle::show() routes
        // through PopoverTracker::set_active(), which dismisses any other
        // active popover — net behavior matches direct widget clicks.
        interaction.popover.ipc_toggle();
        if let Some(ripple) = interaction.ripple.as_ref() {
            trigger_ripple_from_gesture(gesture, x, y, ripple);
        }

        gesture.set_state(gtk4::EventSequenceState::Claimed);
    });

    outer_box.add_controller(gesture);

    let motion = gtk4::EventControllerMotion::new();
    let active_hover_idx = Rc::new(Cell::new(None));
    let targets_for_motion = targets.clone();
    let active_hover_idx_for_motion = active_hover_idx.clone();
    let outer_box_weak = outer_box.downgrade();
    motion.connect_motion(move |_motion, x, y| {
        let Some(outer_box) = outer_box_weak.upgrade() else {
            return;
        };
        let outer_widget = outer_box.upcast_ref::<gtk4::Widget>();
        let targets = targets_for_motion.borrow();
        let active_idx = if let Some(picked) = outer_widget.pick(x, y, gtk4::PickFlags::DEFAULT)
            && click_inside_edge_target(&targets, picked)
        {
            None
        } else {
            edge_target_at(&targets, outer_widget, position, x, y)
        };
        if active_hover_idx_for_motion.get() == active_idx {
            return;
        }
        active_hover_idx_for_motion.set(active_idx);
        apply_edge_hover(&targets, active_idx);
    });

    let targets_for_leave = targets.clone();
    let active_hover_idx_for_leave = active_hover_idx.clone();
    motion.connect_leave(move |_| {
        if active_hover_idx_for_leave.get().is_none() {
            return;
        }
        active_hover_idx_for_leave.set(None);
        apply_edge_hover(&targets_for_leave.borrow(), None);
    });

    outer_box.add_controller(motion);
}

fn install_bar_background_click_handler(outer_box: &gtk4::Box) {
    let gesture = GestureClick::new();
    gesture.set_button(0);
    gesture.set_propagation_phase(gtk4::PropagationPhase::Bubble);
    gesture.connect_pressed(move |gesture, _n_press, x, y| {
        if click_inside_bar_widget(gesture, x, y) {
            return;
        }

        TooltipManager::global().cancel_and_hide();
        PopoverTracker::global().dismiss_active();
        gesture.set_state(gtk4::EventSequenceState::Claimed);
    });
    outer_box.add_controller(gesture);
}

fn click_inside_bar_widget(gesture: &GestureClick, x: f64, y: f64) -> bool {
    // Bar widget containers must keep one of these classes so background clicks
    // don't dismiss popovers before widget handlers run.
    let Some(root) = gesture.widget() else {
        return false;
    };
    let Some(target) = root.pick(x, y, gtk4::PickFlags::DEFAULT) else {
        return false;
    };

    let mut current = Some(target);
    while let Some(widget) = current {
        if widget.has_css_class(class::WIDGET_WRAPPER)
            || widget.has_css_class(class::WIDGET_ITEM)
            || widget.has_css_class(class::WIDGET)
            || widget.has_css_class(class::WIDGET_MERGE_GROUP)
        {
            return true;
        }
        current = widget.parent();
    }

    false
}

/// Production-built bar content shared by the layer-shell window path and
/// runtime UI regression tests.
pub(crate) struct BuiltBarContent {
    pub root: gtk4::Box,
    pub bar: SectionedBar,
}

/// Build the real bar widget tree without creating the layer-shell window.
///
/// This is intentionally production code: `create_bar_window()` uses this same
/// builder before adding layer-shell/window-specific behavior, and UI tests
/// use it to avoid maintaining a parallel bar implementation.
pub(crate) fn build_bar_content(
    app: &Application,
    config: &Config,
    state: &mut BarState,
    output_id: Option<&str>,
) -> BuiltBarContent {
    let position = config.bar.position();
    let is_vertical = position.is_vertical();
    let orientation = bar_flow_orientation_for(position);
    let margin = config.bar.screen_margin as i32;

    // Create the bar container using SectionedBar for proper left/center/right layout
    let bar_box = SectionedBar::new(
        orientation,
        config.bar.spacing as i32,
        config.bar.inset as i32,
        config.widgets.left_has_expander(),
        config.widgets.right_has_expander(),
    );
    bar_box.add_css_class(class::BAR);
    bar_box.add_css_class(if is_vertical {
        class::BAR_VERTICAL
    } else {
        class::BAR_HORIZONTAL
    });
    bar_box.add_css_class(match position {
        BarPosition::Top => class::BAR_TOP,
        BarPosition::Bottom => class::BAR_BOTTOM,
        BarPosition::Left => class::BAR_LEFT,
        BarPosition::Right => class::BAR_RIGHT,
    });
    bar_box.set_hexpand(true);
    bar_box.set_vexpand(true);

    // Wrap bar_box in an outer container so we can inset the
    // visible bar from the anchored edge and sides while
    // keeping the window and exclusive zone full-width.
    let shell_orientation = shell_orientation_for(position);
    let outer_box = gtk4::Box::new(shell_orientation, 0);
    outer_box.add_css_class(class::BAR_SHELL);
    outer_box.set_hexpand(true);
    outer_box.set_vexpand(true);

    // Spacer: empty area between bar content and screen edge.
    let spacer = if margin > 0 {
        let s = gtk4::Box::new(shell_orientation, 0);
        let (width, height) = screen_margin_spacer_size(position, margin);
        s.set_size_request(width, height);
        s.add_css_class(class::BAR_MARGIN_SPACER);
        Some(s)
    } else {
        None
    };

    if screen_margin_spacer_precedes_bar(position)
        && let Some(ref spacer) = spacer
    {
        outer_box.append(spacer);
    }

    let inner_box = gtk4::Box::new(orientation, 0);
    inner_box.add_css_class(class::BAR_SHELL_INNER);
    inner_box.add_css_class(if is_vertical {
        class::BAR_VERTICAL
    } else {
        class::BAR_HORIZONTAL
    });
    inner_box.add_css_class(match position {
        BarPosition::Top => class::BAR_TOP,
        BarPosition::Bottom => class::BAR_BOTTOM,
        BarPosition::Left => class::BAR_LEFT,
        BarPosition::Right => class::BAR_RIGHT,
    });
    inner_box.set_hexpand(!is_vertical);
    inner_box.set_vexpand(is_vertical);
    inner_box.append(&bar_box);

    outer_box.append(&inner_box);

    if !screen_margin_spacer_precedes_bar(position)
        && let Some(ref spacer) = spacer
    {
        outer_box.append(spacer);
    }

    // Find quick_settings config from widget entries to configure the window.
    // Get options from [widgets.quick_settings] if defined.
    let qs_config = config
        .widgets
        .get_options("quick_settings")
        .map(|opts| {
            let entry = WidgetEntry::with_options("quick_settings", opts);
            QuickSettingsConfig::from_entry(&entry)
        })
        .unwrap_or_default();

    // Create handle for this bar's Quick Settings window.
    // The window is created lazily on first open and kept alive for instant re-show.
    let qs_handle = crate::widgets::QuickSettingsWindowHandle::new(app.clone(), qs_config.clone());

    // Register QS handle with the popover registry for IPC control.
    crate::popover_registry::register(
        "quick_settings",
        Rc::new(qs_handle.clone()) as Rc<dyn crate::popover_registry::PopoverToggleable>,
    );

    let edge_targets: EdgeClickTargets = Rc::new(RefCell::new(Vec::new()));

    // Create left section
    let left_section = create_section(
        "left",
        config,
        state,
        &qs_handle,
        output_id,
        orientation,
        &edge_targets,
    );
    bar_box.set_start_widget(Some(&left_section));

    // Create center section only if there are center widgets
    // Without a center widget, the layout manager uses linear allocation
    let has_center_content = !config.widgets.resolved_center().is_empty();
    if has_center_content {
        let center_section = create_center_section(
            config,
            state,
            &qs_handle,
            output_id,
            orientation,
            &edge_targets,
        );
        bar_box.set_center_widget(Some(&center_section));
    }

    // Create right section
    let right_section = create_section(
        "right",
        config,
        state,
        &qs_handle,
        output_id,
        orientation,
        &edge_targets,
    );
    bar_box.set_end_widget(Some(&right_section));

    install_bar_background_click_handler(&outer_box);
    install_edge_click_handler(&outer_box, position, &edge_targets);

    BuiltBarContent {
        root: outer_box,
        bar: bar_box,
    }
}

/// Create and configure the bar window with layer-shell.
///
/// The `state` parameter is used to store widget handles, keeping them alive
/// for the lifetime of the bar. The `output_id` is the monitor connector name
/// used for per-monitor widget filtering.
pub fn create_bar_window(
    app: &Application,
    config: &Config,
    monitor: &gtk4::gdk::Monitor,
    output_id: &str,
    state: &mut BarState,
) -> ApplicationWindow {
    let position = config.bar.position();
    let is_vertical = position.is_vertical();
    let bar_height = rendered_bar_height(config);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("vibepanel")
        .decorated(false)
        .resizable(false)
        .default_height(if is_vertical { -1 } else { bar_height })
        .default_width(if is_vertical { bar_height } else { -1 })
        .build();

    window.add_css_class(class::BAR_WINDOW);

    // Initialize layer-shell
    window.init_layer_shell();
    window.set_namespace(Some("vibepanel"));
    window.set_layer(Layer::Top);

    // Bind to specific monitor - this should handle width automatically
    window.set_monitor(Some(monitor));
    debug!("Bar bound to monitor: {:?}", monitor.connector());

    // Anchor to the configured edge and stretch along the opposite axis.
    window.set_anchor(
        Edge::Top,
        matches!(position, BarPosition::Top) || is_vertical,
    );
    window.set_anchor(
        Edge::Bottom,
        matches!(position, BarPosition::Bottom) || is_vertical,
    );
    window.set_anchor(
        Edge::Left,
        matches!(position, BarPosition::Left) || !is_vertical,
    );
    window.set_anchor(
        Edge::Right,
        matches!(position, BarPosition::Right) || !is_vertical,
    );

    // Reserve space (exclusive zone) so other windows don't overlap
    window.auto_exclusive_zone_enable();

    // Bar doesn't need keyboard input
    window.set_keyboard_mode(KeyboardMode::None);

    let built_content = build_bar_content(app, config, state, Some(output_id));
    let bar_box = built_content.bar.clone();

    window.set_child(Some(&built_content.root));

    // Set window width to the target monitor's width on map.
    // We capture the geometry now rather than using monitor_at_surface() later,
    // because the surface might not be on the correct monitor yet at map time.
    let target_geometry = monitor.geometry();
    let target_width = target_geometry.width();
    let target_height = target_geometry.height();

    let is_island_mode = config.bar.background_opacity == 0.0;

    let bar_box_for_blur = bar_box.clone();
    window.connect_map(move |win| {
        if is_vertical {
            win.set_default_size(bar_height, target_height);
            debug!(
                "Set window height to target monitor size: {}px",
                target_height
            );
        } else {
            win.set_default_size(target_width, bar_height);
            debug!(
                "Set window width to target monitor size: {}px",
                target_width
            );
        }

        // Apply bar blur region on map (opaque/translucent bar path).
        // The islands path is handled by the layout allocate callback below.
        //
        // Island mode: allocation applies active blur regions. If blur was
        // disabled while unmapped, clean up the stale protocol object now that
        // the wl_surface is resolvable again.
        //
        // Opaque/translucent mode: apply blur on map.  The else-branch
        // removes any stale protocol object left from a previous map cycle
        // (blur enabled on last show, then disabled while bars were hidden).
        // `remove_blur_region` is idempotent (no-op when no effect exists).
        if is_island_mode {
            if !ConfigManager::global().blur_enabled()
                && let Some(blur) =
                    crate::services::background_effect::BackgroundEffectManager::global()
            {
                blur.remove_blur_region(win);
            }
        } else if ConfigManager::global().blur_enabled() {
            if let Some(blur) =
                crate::services::background_effect::BackgroundEffectManager::global()
            {
                blur.apply_bar_blur_region(win, &bar_box_for_blur);
            }
        } else if let Some(blur) =
            crate::services::background_effect::BackgroundEffectManager::global()
        {
            blur.remove_blur_region(win);
        }
    });

    // Install layout callback for island blur (transparent bar mode).
    // When bar.background_opacity == 0.0, we blur per-widget-island instead of
    // the whole surface. The callback fires after every layout pass so the blur
    // region stays in sync as widgets move or resize (tray changes, title width, etc).
    //
    // We also keep a shared clone of the island-apply closure so the theme-change
    // hot-reload handler can trigger an immediate re-apply when blur is toggled on.
    //
    // `prev_bounds` caches the last-applied island bounds to skip redundant
    // Wayland protocol traffic.  It is hoisted here (rather than inside the
    // closure) so the theme-change handler can clear it when blur is toggled off
    // — otherwise the stale cache would short-circuit the next apply.
    let prev_bounds = Rc::new(RefCell::new(Vec::<(i32, i32, i32, i32)>::new()));
    // Clone for the theme-change handler so it can invalidate the cache on any
    // theme change (the original `prev_bounds` is moved into the island closure).
    let prev_bounds_for_theme = Rc::clone(&prev_bounds);

    let island_apply: Option<Rc<dyn Fn()>> = if is_island_mode {
        let win_weak = window.downgrade();
        let bar_box_weak = bar_box.downgrade();
        let closure: Rc<dyn Fn()> = Rc::new(move || {
            if !ConfigManager::global().blur_enabled() {
                // Clean up any stale blur effect left from before blur was
                // disabled (e.g. ipc_hide -> blur-off -> ipc_show).
                // Only do this once: if prev_bounds is already empty we've
                // either already cleaned up or never had blur applied.
                if !prev_bounds.borrow().is_empty() {
                    prev_bounds.borrow_mut().clear();
                    // Defer the remove out of the GTK allocate pass: it calls
                    // wl_surface.commit() synchronously, and we'd rather not
                    // do that mid-layout.  Re-check guard inside idle in case
                    // a subsequent allocate flipped state back.
                    let win_weak_idle = win_weak.clone();
                    let prev_bounds_idle = Rc::clone(&prev_bounds);
                    gtk4::glib::idle_add_local_once(move || {
                        if !prev_bounds_idle.borrow().is_empty() {
                            return;
                        }
                        if ConfigManager::global().blur_enabled() {
                            return;
                        }
                        if let Some(win) = win_weak_idle.upgrade()
                            && let Some(blur) =
                                crate::services::background_effect::BackgroundEffectManager::global(
                                )
                        {
                            blur.remove_blur_region(&win);
                        }
                    });
                }
                return;
            }
            let Some(win) = win_weak.upgrade() else {
                return;
            };
            // Bar is mapped but opacity-hidden (e.g. hide_all during monitor
            // hotplug debounce).  Skip blur — it would be applied to an
            // invisible surface.  reconfigure_all() rebuilds bars and
            // connect_map re-applies blur when they are shown again.
            if win.opacity() <= 0.0 {
                return;
            }
            let Some(blur) = crate::services::background_effect::BackgroundEffectManager::global()
            else {
                return;
            };
            let Some(native) = win.native() else { return };
            let Some(bar_box) = bar_box_weak.upgrade() else {
                return;
            };
            let islands = collect_island_bounds(&bar_box, &native);
            // Skip redundant Wayland protocol traffic when bounds haven't changed.
            // The allocate callback fires on every layout pass (clock tick, tray
            // icon change, etc.) but most passes produce identical island bounds.
            if *prev_bounds.borrow() == islands {
                return;
            }
            *prev_bounds.borrow_mut() = islands.clone();
            if !islands.is_empty() {
                blur.apply_bar_island_blur_regions(&win, &islands);
            } else {
                // Defer the remove out of the GTK allocate pass: it calls
                // wl_surface.commit() synchronously, and we'd rather not
                // do that mid-layout.  Re-check inside idle so a fast
                // allocate-then-allocate sequence can't clear blur that
                // was just legitimately reapplied.
                let win_weak_idle = win_weak.clone();
                let prev_bounds_idle = Rc::clone(&prev_bounds);
                gtk4::glib::idle_add_local_once(move || {
                    if !prev_bounds_idle.borrow().is_empty() {
                        return;
                    }
                    if let Some(win) = win_weak_idle.upgrade()
                        && let Some(blur) =
                            crate::services::background_effect::BackgroundEffectManager::global()
                    {
                        blur.remove_blur_region(&win);
                    }
                });
            }
        });
        if let Some(lm) = bar_box
            .layout_manager()
            .and_downcast::<crate::sectioned_bar::CenterPriorityLayout>()
        {
            let closure_clone = Rc::clone(&closure);
            lm.set_on_allocate(move || closure_clone());
        }
        Some(closure)
    } else {
        None
    };

    // Hot-reload: re-apply or remove bar blur when the theme config changes
    // (e.g. user toggles `theme.blur` or changes `bar.border_radius`).
    //
    // Note: `background_opacity` changes trigger a structural rebuild
    // (config_structure_changed), so this callback only needs to handle
    // toggling blur on/off within the current mode (opaque or island).
    {
        let win_weak = window.downgrade();
        let bar_box_for_theme = bar_box.clone();
        let theme_cb_id = ConfigManager::global().on_theme_change(move || {
            let Some(win) = win_weak.upgrade() else {
                return;
            };
            if ConfigManager::global().blur_enabled() {
                // Invalidate the island-bounds cache so radius/theme changes
                // force a re-apply (the cache only tracks geometry, not radii).
                prev_bounds_for_theme.borrow_mut().clear();
                if let Some(apply) = &island_apply {
                    // Island mode: re-apply per-island regions immediately.
                    apply();
                } else if let Some(blur) =
                    crate::services::background_effect::BackgroundEffectManager::global()
                {
                    // Opaque/translucent mode: re-apply whole-bar region.
                    blur.apply_bar_blur_region(&win, &bar_box_for_theme);
                }
            } else if let Some(blur) =
                crate::services::background_effect::BackgroundEffectManager::global()
            {
                blur.remove_blur_region(&win);
            }
        });
        state.add_handle(Box::new(ThemeCallbackGuard(theme_cb_id)));
    }

    window.set_visible(true);

    info!(
        "Bar window created: size={}px, margin={}px, monitor={:?}, widgets={}",
        config.bar.size,
        config.bar.screen_margin,
        monitor.connector(),
        state.handle_count()
    );

    window
}

/// Collect the surface-local bounds of every visible widget island in the bar.
///
/// Walks the children of each section in the `SectionedBar`, finds all
/// `.widget-wrapper` boxes that are visible, and returns their
/// `(x, y, width, height)` in surface-local logical coordinates via
/// `Widget::compute_bounds()`.
fn collect_island_bounds(
    bar_box: &SectionedBar,
    native: &gtk4::Native,
) -> Vec<(i32, i32, i32, i32)> {
    use crate::styles::class;
    let mut result = Vec::new();

    for section_name in &["left", "center", "right"] {
        let Some(section) = bar_box.section(section_name) else {
            continue;
        };
        if !section.is_visible() {
            continue;
        }
        let mut child = section.first_child();
        while let Some(widget) = child {
            if widget.is_visible()
                && widget.has_css_class(class::WIDGET_WRAPPER)
                && let Some(bounds) = widget.compute_bounds(native.upcast_ref::<gtk4::Widget>())
            {
                let x = bounds.x().round() as i32;
                let y = bounds.y().round() as i32;
                let w = bounds.width().round() as i32;
                let h = bounds.height().round() as i32;
                if w > 0 && h > 0 {
                    result.push((x, y, w, h));
                }
            }
            child = widget.next_sibling();
        }
    }

    result
}

/// Return true when a resolved entry's `width` option is a positive
/// integer, mirroring `SpacerConfig::from_entry`.
///
/// Invalid or non-integer `width` values fall through to the flexible
/// branch, keeping group expansion policy in sync with what the spacer
/// widget itself does at build time.
fn entry_has_fixed_width_option(entry: &WidgetEntry) -> bool {
    entry
        .options
        .get("width")
        .and_then(|v| v.as_integer())
        .and_then(|n| u32::try_from(n).ok())
        .is_some()
}

/// Build a single widget or a group of widgets sharing one island.
///
/// Returns the number of widgets built (for counting purposes).
fn build_widget_or_group(
    item: &WidgetOrGroup,
    container: &gtk4::Box,
    state: &mut BarState,
    qs_handle: &crate::widgets::QuickSettingsWindowHandle,
    output_id: Option<&str>,
    orientation: gtk4::Orientation,
    edge_targets: &EdgeClickTargets,
) -> usize {
    match item {
        WidgetOrGroup::Single(entry) => {
            // Single widget with its own island
            if let Some(built) = WidgetFactory::build(entry, Some(qs_handle), output_id) {
                register_edge_target(edge_targets, &built.widget, built.edge_interaction.clone());
                container.append(&built.widget);
                state.add_handle(built.handle);
                1
            } else {
                0
            }
        }
        WidgetOrGroup::Group { group } => {
            if group.is_empty() {
                return 0;
            }

            let group_has_flexible_spacer = group
                .iter()
                .any(|entry| entry.name == "spacer" && !entry_has_fixed_width_option(entry));

            // Create a shared island container for the group
            let island = gtk4::Box::new(orientation, 0);
            island.add_css_class(class::WIDGET_WRAPPER);
            if orientation == gtk4::Orientation::Vertical {
                island.set_vexpand(group_has_flexible_spacer);
            } else {
                island.set_hexpand(group_has_flexible_spacer);
            }

            // Create inner content box (matching BaseWidget structure)
            let content = gtk4::Box::new(orientation, 0);
            content.add_css_class(class::CONTENT);
            content.set_hexpand(true);
            content.set_vexpand(true);
            // Cross-axis must fill the bar thickness so grouped children span
            // the full bar width (vertical) or bar height (horizontal).
            if orientation == gtk4::Orientation::Vertical {
                content.set_halign(gtk4::Align::Fill);
                content.set_valign(gtk4::Align::Center);
            } else {
                content.set_halign(gtk4::Align::Fill);
                content.set_valign(gtk4::Align::Fill);
            }

            // Group surface — transparent in CSS. Direct children paint their
            // own backgrounds so hover colors composite once over the bar.
            let surface = gtk4::Box::new(orientation, 0);
            surface.add_css_class(class::WIDGET);
            surface.add_css_class(class::WIDGET_GROUP);
            // First real widget's per-widget style (outline_color, background_color)
            // applies to the group surface so the shared border uses the right color.
            // Skip spacers — they carry no per-widget styling.
            if let Some(first) = group.iter().find(|e| e.name != "spacer") {
                surface.add_css_class(&first.name.replace('_', "-"));
            }
            surface.set_overflow(gtk4::Overflow::Hidden);
            surface.set_hexpand(true);
            surface.set_vexpand(true);

            surface.append(&content);
            island.append(&surface);

            // Partition into runs of same-popover widgets for merge grouping.
            // Widgets with custom click handlers stay unmergeable so their
            // on_click_right / on_click_middle commands aren't silently lost.
            // Spacers are tracked as MergeKind::Spacer so they can be absorbed
            // into adjacent runs rather than breaking merges.
            let group_has_weather = group.iter().any(|e| e.name == "weather");
            let kinds: Vec<MergeKind> = group
                .iter()
                .map(|e| {
                    if e.name == "spacer" {
                        MergeKind::Spacer
                    } else {
                        let (right, middle) = ConfigManager::global().get_click_handlers(&e.name);
                        let has_custom_click = right.is_some() || middle.is_some();
                        let clock_weather_opt_out = group_has_weather
                            && e.name == "clock"
                            && e.options.get("show_weather").and_then(|v| v.as_bool())
                                == Some(false);
                        if has_custom_click || clock_weather_opt_out {
                            MergeKind::Popover(PopoverKind::Unmergeable)
                        } else {
                            MergeKind::Popover(popover_kind_for(&e.name))
                        }
                    }
                })
                .collect();
            let runs = compute_merge_runs(&kinds);

            let mut count = 0;

            // Build entries individually (used for singletons and merge fallback).
            // Each child paints its own background; the group surface is transparent.
            let build_individually =
                |entries: &[WidgetEntry], content: &gtk4::Box, state: &mut BarState| -> usize {
                    let mut n = 0;
                    for entry in entries {
                        if let Some(built) = WidgetFactory::build(entry, Some(qs_handle), output_id)
                        {
                            register_edge_target(
                                edge_targets,
                                &built.widget,
                                built.edge_interaction.clone(),
                            );
                            // Strip the standalone wrapper class so the wrapper-hover
                            // rule doesn't fire — per-item hover is handled by a
                            // group-scoped rule that paints on the .widget-item.
                            built.widget.remove_css_class(class::WIDGET_WRAPPER);
                            built.widget.add_css_class(&entry.name.replace('_', "-"));
                            // Grouped hover uses a large box-shadow spread to refill
                            // the cell around the pill; this clips it to item bounds.
                            built.widget.set_overflow(gtk4::Overflow::Hidden);
                            // Spacers don't use BaseWidget, so they lack the
                            // .widget-item class that provides the grouped background.
                            // Add it so spacers inside groups inherit the background
                            // colour instead of showing a transparent gap.
                            if entry.name == "spacer" {
                                built.widget.add_css_class(class::WIDGET_ITEM);
                            }
                            content.append(&built.widget);
                            state.add_handle(built.handle);
                            n += 1;
                        }
                    }
                    n
                };

            // Spacers don't affect the merge decision. Only create a
            // merge group when the run contains ≥2 real widgets of the
            // same kind, or when the entire group has exactly one real
            // widget (per the explicit [cpu, spacer] single-button rule).
            let total_real = group.iter().filter(|e| e.name != "spacer").count();

            for (kind, start, end) in &runs {
                let run_entries = &group[*start..*end];
                let run_len = end - start;
                let real_in_run = run_entries.iter().filter(|e| e.name != "spacer").count();
                let run_has_clock = run_entries.iter().any(|e| e.name == "clock");
                let supports_merge = *kind != PopoverKind::Unmergeable
                    && (*kind != PopoverKind::CalendarWeather || run_has_clock);

                if run_len >= 2 && supports_merge && (real_in_run >= 2 || total_real == 1) {
                    let merged = build_merge_group(
                        run_entries,
                        *kind,
                        &content,
                        state,
                        orientation,
                        edge_targets,
                    );
                    if merged > 0 {
                        count += merged;
                    } else {
                        // Merge unsupported for this kind — fall back
                        count += build_individually(run_entries, &content, state);
                    }
                } else {
                    count += build_individually(run_entries, &content, state);
                }
            }

            // Only append the island if we built at least one widget
            if count > 0 {
                container.append(&island);
                debug!("Created widget group with {} widget(s)", count);
            }

            count
        }
    }
}

/// Merge-kind used by the group builder: either a real widget with a popover
/// kind, or a transparent spacer that should be absorbed into adjacent runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergeKind {
    Popover(PopoverKind),
    Spacer,
}

/// Partition `MergeKind` values into runs of adjacent equal popover kinds.
///
/// Spacers are transparently absorbed:
///   * Leading spacers join the first real run.
///   * Trailing spacers join the last real run.
///   * Spacers between two widgets of the same mergeable kind are swallowed
///     into that run so e.g. `[cpu, spacer, memory]` still merges.
///   * Spacers between different kinds (or next to `Unmergeable`) attach to
///     the left-hand run.
///   * `Unmergeable` entries are never grouped — each becomes its own singleton
///     run (any neighbouring spacers are absorbed into it).
fn compute_merge_runs(kinds: &[MergeKind]) -> Vec<(PopoverKind, usize, usize)> {
    let mut runs: Vec<(PopoverKind, usize, usize)> = Vec::new();
    if kinds.is_empty() {
        return runs;
    }

    let mut start = 0;
    while start < kinds.len() {
        match kinds[start] {
            MergeKind::Spacer => {
                // Absorb into the previous run if one exists, otherwise skip
                // and let the trailing-spacer fix-up attach it to the first run.
                if let Some(last) = runs.last_mut() {
                    last.2 = start + 1;
                }
                start += 1;
            }
            MergeKind::Popover(kind) => {
                if kind == PopoverKind::Unmergeable {
                    runs.push((kind, start, start + 1));
                    start += 1;
                } else {
                    let mut end = start + 1;
                    while end < kinds.len() {
                        match kinds[end] {
                            MergeKind::Spacer => {
                                // Look ahead past consecutive spacers to see
                                // whether the next real widget matches our kind.
                                let mut lookahead = end + 1;
                                while lookahead < kinds.len()
                                    && kinds[lookahead] == MergeKind::Spacer
                                {
                                    lookahead += 1;
                                }
                                if lookahead < kinds.len()
                                    && let MergeKind::Popover(k) = kinds[lookahead]
                                    && k == kind
                                {
                                    // Spacer bridges same-kind widgets — absorb it.
                                    end = lookahead + 1;
                                    continue;
                                }
                                // Spacer precedes a different kind or EOF — stop.
                                break;
                            }
                            MergeKind::Popover(k) => {
                                if k == kind {
                                    end += 1;
                                } else {
                                    break;
                                }
                            }
                        }
                    }
                    runs.push((kind, start, end));
                    start = end;
                }
            }
        }
    }

    // Leading spacers: if the first real run starts after index 0, extend it
    // backward so those spacers are still built as part of the group.
    if let Some(first) = runs.first_mut()
        && first.1 > 0
    {
        first.1 = 0;
    }

    runs
}

/// Build a merge-group wrapper for adjacent same-popover widgets.
/// Returns the number of widgets successfully built (0 if unsupported kind).
fn build_merge_group(
    entries: &[WidgetEntry],
    kind: PopoverKind,
    parent_content: &gtk4::Box,
    state: &mut BarState,
    orientation: gtk4::Orientation,
    edge_targets: &EdgeClickTargets,
) -> usize {
    // Overlay wrapper — ripple sits on top of the content box.
    let wrapper = Overlay::new();
    wrapper.add_css_class(class::WIDGET_MERGE_GROUP);
    wrapper.add_css_class(state::CLICKABLE);
    // Merged groups paint as a single button; use the first non-spacer
    // entry so a leading spacer doesn't determine the group's identity.
    let representative = entries
        .iter()
        .find(|e| e.name != "spacer")
        .unwrap_or(&entries[0]);
    wrapper.add_css_class(&representative.name.replace('_', "-"));
    // Required for the merge-group hover pill: its 9999px box-shadow
    // refill is clipped here so it cannot bleed outside the group.
    wrapper.set_overflow(gtk4::Overflow::Hidden);

    let inner_content = gtk4::Box::new(orientation, 0);
    inner_content.add_css_class(class::MERGE_GROUP_CONTENT);
    inner_content.set_hexpand(true);
    inner_content.set_vexpand(true);
    // Cross-axis fill: same as the outer group content box (see
    // build_widget_or_group).
    if orientation == gtk4::Orientation::Vertical {
        inner_content.set_halign(gtk4::Align::Fill);
        inner_content.set_valign(gtk4::Align::Center);
    } else {
        inner_content.set_halign(gtk4::Align::Fill);
        inner_content.set_valign(gtk4::Align::Fill);
    }
    // Visible gap between widgets inside a merge group. Uses the same
    // theme-derived spacing as inter-child gaps inside a single widget — both
    // are gaps between visual elements that may carry Pango line-height
    // baggage on their flow-axis edges, so both shrink in vertical mode.
    let merge_gap = ConfigManager::global()
        .theme_sizes()
        .content_gap_for_orientation(orientation == gtk4::Orientation::Vertical);
    inner_content.set_spacing(merge_gap as i32);
    wrapper.set_child(Some(&inner_content));

    let ripple_handle = RippleHandle::new();
    // Wrap the ripple DrawingArea in a Box that establishes a fully-rounded
    // clip, so the ripple matches the inner pill shape on hover. The merge
    // group itself uses position-aware radius (square at seams in mixed
    // groups), which would otherwise leak the ripple into the corners.
    let ripple_clip = gtk4::Box::new(orientation, 0);
    ripple_clip.add_css_class(class::WIDGET_MERGE_GROUP_RIPPLE_CLIP);
    ripple_clip.set_overflow(gtk4::Overflow::Hidden);
    ripple_clip.append(ripple_handle.widget());
    wrapper.add_overlay(&ripple_clip);
    wrapper.set_measure_overlay(&ripple_clip, true);

    let widget_name = match kind {
        PopoverKind::CalendarWeather => "clock".to_string(),
        _ => representative.name.clone(),
    };
    let menu_handle = MenuHandle::new_placeholder(widget_name, wrapper.clone());

    // Primary click toggles the shared popover. Right/middle-click handlers
    // are not forwarded — the merge group is a single button, and per-widget
    // click commands don't have a meaningful target here.
    let gesture_click = GestureClick::new();
    gesture_click.set_button(0);

    {
        let menu_for_cb = menu_handle.clone();
        let ripple_for_press = ripple_handle.clone();
        gesture_click.connect_pressed(move |gesture, _n_press, x, y| {
            let button = gesture.current_button();
            if button == gdk::BUTTON_PRIMARY {
                let my_menu_was_visible = menu_for_cb.is_visible();

                TooltipManager::global().cancel_and_hide();
                PopoverTracker::global().dismiss_active();

                if !my_menu_was_visible {
                    menu_for_cb.show();
                }

                trigger_ripple_from_gesture(gesture, x, y, &ripple_for_press);
                gesture.set_state(gtk4::EventSequenceState::Claimed);
            }
        });
    }

    wrapper.add_controller(gesture_click.clone());

    let (binding_handle, built_widgets): (Box<dyn std::any::Any>, Vec<widgets::BuiltWidget>) =
        match kind {
            PopoverKind::System => {
                let binding = SystemPopoverBinding::new_for_menu(&menu_handle);
                let built_widgets = entries
                    .iter()
                    .filter_map(|entry| WidgetFactory::build_passive(entry, &binding))
                    .collect();
                (Box::new(binding), built_widgets)
            }
            PopoverKind::CalendarWeather => {
                let Some(clock_entry) = entries.iter().find(|entry| entry.name == "clock") else {
                    warn!("Calendar/weather merge group requires a clock widget");
                    return 0;
                };
                let clock_config = ClockConfig::from_entry(clock_entry);
                let has_weather_widget = entries.iter().any(|entry| entry.name == "weather");
                let show_weather = has_weather_widget || clock_config.show_weather;
                let binding = CalendarWeatherPopoverBinding::new_for_menu(
                    &menu_handle,
                    clock_config.show_week_numbers,
                    show_weather,
                );
                let built_widgets = entries
                    .iter()
                    .filter_map(WidgetFactory::build_calendar_weather_passive)
                    .collect();
                (Box::new(binding), built_widgets)
            }
            PopoverKind::Unmergeable => {
                warn!("Merge group for {:?} popover not supported", kind);
                return 0;
            }
        };

    // If only 0–1 widgets survived (e.g. GPU unavailable), don't wrap in a
    // merge group — return 0 so the caller rebuilds via the normal active path.
    // Dropped passive widgets clean up their service callbacks via Drop.
    if built_widgets.len() <= 1 {
        return 0;
    }

    let count = built_widgets.len();
    for built in built_widgets {
        inner_content.append(&built.widget);
        state.add_handle(built.handle);
    }

    parent_content.append(&wrapper);
    register_edge_target(
        edge_targets,
        wrapper.upcast_ref::<gtk4::Widget>(),
        Some(EdgeInteraction {
            popover: menu_handle.clone() as Rc<dyn crate::popover_registry::PopoverToggleable>,
            ripple: Some(ripple_handle.clone()),
        }),
    );

    // Register the shared menu handle under ALL participating widget names
    // for IPC popover control. Uses underscores (config convention).
    for entry in entries {
        if entry.name == "spacer" {
            continue;
        }
        crate::popover_registry::register(
            &entry.name,
            menu_handle.clone() as Rc<dyn crate::popover_registry::PopoverToggleable>,
        );
    }

    // Keep the menu handle, gesture, and ripple alive
    state.add_handle(binding_handle);
    state.add_handle(Box::new(menu_handle));
    state.add_handle(Box::new(gesture_click));
    state.add_handle(Box::new(ripple_handle));
    debug!(
        "Created merge group with {} widget(s) ({:?} popover)",
        count, kind
    );

    count
}

fn create_section(
    position: &str,
    config: &Config,
    state: &mut BarState,
    qs_handle: &crate::widgets::QuickSettingsWindowHandle,
    output_id: Option<&str>,
    orientation: gtk4::Orientation,
    edge_targets: &EdgeClickTargets,
) -> gtk4::Box {
    let section = gtk4::Box::new(
        orientation,
        0, // Spacing handled via CSS margins to allow spacer widget to have no gaps
    );
    // Clip overflowing content to prevent widgets from rendering beyond section bounds
    section.set_overflow(gtk4::Overflow::Hidden);
    let section_class = match position {
        "left" => class::BAR_SECTION_LEFT,
        "right" => class::BAR_SECTION_RIGHT,
        _ => class::BAR_SECTION_CENTER,
    };
    section.add_css_class(section_class);

    // Get the resolved widget entries for this position (with options applied, disabled filtered)
    let resolved = match position {
        "left" => config.widgets.resolved_left(),
        "right" => config.widgets.resolved_right(),
        _ => return section,
    };

    // Build widgets from resolved entries
    let mut widget_count = 0;
    for item in &resolved {
        widget_count += build_widget_or_group(
            item,
            &section,
            state,
            qs_handle,
            output_id,
            orientation,
            edge_targets,
        );
    }

    debug!(
        "Created {} section with {} widget(s)",
        position, widget_count
    );
    section
}

/// Create the center section with widgets.
fn create_center_section(
    config: &Config,
    state: &mut BarState,
    qs_handle: &crate::widgets::QuickSettingsWindowHandle,
    output_id: Option<&str>,
    orientation: gtk4::Orientation,
    edge_targets: &EdgeClickTargets,
) -> gtk4::Box {
    let section = gtk4::Box::new(orientation, 0);
    section.add_css_class(class::BAR_SECTION_CENTER);

    let mut widget_count = 0;
    for item in &config.widgets.resolved_center() {
        widget_count += build_widget_or_group(
            item,
            &section,
            state,
            qs_handle,
            output_id,
            orientation,
            edge_targets,
        );
    }

    debug!("Created center section with {} widget(s)", widget_count);
    section
}

/// Load and apply CSS styling to the application.
pub fn load_css(config: &Config) {
    let provider = gtk4::CssProvider::new();

    // Use cached palettes from ConfigManager (avoids re-reading wallpaper image)
    let palette = ConfigManager::global().palette();
    let popover_palette = ConfigManager::global().popover_palette();
    let css = generate_css(config, &palette, popover_palette.as_ref());

    // Debug: print theme configuration
    debug!("Generated theme CSS:");
    debug!(
        "  mode = {} (is_gtk_mode={})",
        config.theme.mode, palette.is_gtk_mode
    );
    debug!("  accent_source = {:?}", palette.accent_source);
    debug!("  accent_primary = {}", palette.accent_primary);
    debug!("  state_warning = {}", palette.state_warning);
    debug!("  state_urgent = {}", palette.state_urgent);

    provider.load_from_string(&css);

    // Apply to default display with USER priority to override GTK themes
    if let Some(display) = gtk4::gdk::Display::default() {
        // Remove the old theme CSS provider first to ensure clean reload
        // (without this, removed config values would leave stale CSS rules)
        THEME_CSS_PROVIDER.with(|cell| {
            if let Some(old_provider) = cell.borrow_mut().take() {
                gtk4::style_context_remove_provider_for_display(&display, &old_provider);
            }
        });

        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_USER,
        );

        // Store the new provider so we can remove it on next reload
        THEME_CSS_PROVIDER.with(|cell| {
            *cell.borrow_mut() = Some(provider);
        });

        debug!(
            "CSS loaded and applied (dark_mode={})",
            palette.is_dark_mode
        );

        // Register transient CSS (grow-in rules) at priority above user CSS
        load_transient_css(&display);

        // Load user's custom style.css if it exists
        replace_user_css();
    } else {
        warn!("No default display available, CSS styling not applied");
    }
}

/// Priority for user CSS - higher than everything else to ensure overrides work.
/// USER = 800, we use 900 to be above all internal styles (which use USER + 10 max).
const USER_CSS_PRIORITY: u32 = gtk4::STYLE_PROVIDER_PRIORITY_USER + 100;

/// Priority for transient/internal CSS that must override even user CSS.
/// Used for `.workspace-grow-in` which forces `min-width: 0; transition: none;`
/// so the container animation system stays in control during grow-in sequences.
const TRANSIENT_CSS_PRIORITY: u32 = gtk4::STYLE_PROVIDER_PRIORITY_USER + 200;

// Thread-local storage for the theme CSS provider so we can replace it on reload
thread_local! {
    static THEME_CSS_PROVIDER: RefCell<Option<gtk4::CssProvider>> = const { RefCell::new(None) };
}

// Thread-local storage for the user CSS provider so we can replace it on reload
thread_local! {
    static USER_CSS_PROVIDER: RefCell<Option<gtk4::CssProvider>> = const { RefCell::new(None) };
}

// Thread-local storage for the transient CSS provider (grow-in rules).
// Stored to keep the provider alive for the process lifetime without
// using std::mem::forget.
thread_local! {
    static TRANSIENT_CSS_PROVIDER: RefCell<Option<gtk4::CssProvider>> = const { RefCell::new(None) };
}

/// Search paths for user style.css.
///
/// If `config_dir` is provided (the parent directory of the active config file),
/// it takes highest priority — this ensures `--config /custom/path/config.toml`
/// also picks up `/custom/path/style.css`. For the normal XDG case this is a
/// harmless duplicate that gets deduplicated.
///
/// Search order:
/// 1. `<config_dir>/style.css` (next to active config file, if known)
/// 2. `$XDG_CONFIG_HOME/vibepanel/style.css`
/// 3. `~/.config/vibepanel/style.css`
/// 4. `./style.css` (current working directory)
fn user_css_search_paths_from_env(
    config_dir: Option<&Path>,
    xdg_config_home: Option<&Path>,
    home_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut push_unique = |p: PathBuf| {
        if seen.insert(p.clone()) {
            paths.push(p);
        }
    };

    // 1. Next to the active config file (highest priority)
    if let Some(dir) = config_dir {
        push_unique(dir.join("style.css"));
    }

    // 2. $XDG_CONFIG_HOME/vibepanel/style.css
    if let Some(xdg_config) = xdg_config_home {
        push_unique(xdg_config.join("vibepanel/style.css"));
    }

    // 3. ~/.config/vibepanel/style.css
    if let Some(home) = home_dir {
        push_unique(home.join(".config/vibepanel/style.css"));
    }

    // 4. ./style.css (current working directory)
    push_unique(PathBuf::from("style.css"));

    paths
}

pub(crate) fn user_css_search_paths(config_dir: Option<&Path>) -> Vec<PathBuf> {
    let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home_dir = std::env::var_os("HOME").map(PathBuf::from);
    user_css_search_paths_from_env(config_dir, xdg_config_home.as_deref(), home_dir.as_deref())
}

/// Find user's style.css file if it exists.
///
/// Searches the unified path list (config-adjacent first, then XDG/HOME/CWD)
/// and returns the first path that exists on disk.
pub(crate) fn find_user_css(config_dir: Option<&Path>) -> Option<PathBuf> {
    user_css_search_paths(config_dir)
        .into_iter()
        .find(|path| path.exists())
}

/// Load transient CSS rules at high priority (above user CSS).
///
/// These rules exist to keep the container animation system in control
/// during workspace indicator grow-in/removal sequences. The `.workspace-grow-in`
/// class forces `min-width: 0` and `transition: none` so that:
/// - The indicator starts at zero width (container animates it in)
/// - CSS transitions don't fight the container's tick-driven animation
///
/// Registered once per display; survives CSS reloads (intentionally).
fn load_transient_css(display: &gtk4::gdk::Display) {
    TRANSIENT_CSS_PROVIDER.with(|cell| {
        if cell.borrow().is_some() {
            return;
        }

        // `.workspace-grow-in` collapses the flow-axis size to 0 so the
        // container animates the indicator in. In horizontal mode the flow
        // axis is width; in vertical it is height. The .bar--vertical
        // override restores min-width to the cross-axis size and zeros
        // min-height instead.
        let provider = gtk4::CssProvider::new();
        provider.load_from_string(&format!(
            ".{grow_in} {{ min-width: 0; }} \
             .bar--vertical .{grow_in} {{ min-width: var(--icon-size); min-height: 0; }} \
             .{notrans} {{ transition: none; }}",
            grow_in = style_widget::WORKSPACE_GROW_IN,
            notrans = style_widget::WORKSPACE_GROW_IN_NOTRANS,
        ));

        gtk4::style_context_add_provider_for_display(display, &provider, TRANSIENT_CSS_PRIORITY);

        *cell.borrow_mut() = Some(provider);
        debug!(
            "Transient CSS registered (priority={})",
            TRANSIENT_CSS_PRIORITY
        );
    });
}

/// Replace user's custom CSS provider (fail-safe).
///
/// This is the single function for both initial load and hot-reload of user
/// `style.css`. It reads and builds the new provider *before* removing the old
/// one, so a read failure keeps the current CSS intact rather than leaving the
/// bar un-styled.
///
/// Called from:
/// - `load_css()` after theme CSS is applied
/// - `handle_config_message(StyleCssChanged)` when the file watcher detects a change
pub(crate) fn replace_user_css() {
    let Some(display) = gtk4::gdk::Display::default() else {
        warn!("No default display available for CSS reload");
        return;
    };

    let config_dir = ConfigManager::global().config_dir();
    let Some(path) = find_user_css(config_dir.as_deref()) else {
        // No style.css found anywhere — remove old provider if any
        USER_CSS_PROVIDER.with(|cell| {
            if let Some(old) = cell.borrow_mut().take() {
                gtk4::style_context_remove_provider_for_display(&display, &old);
                debug!("Removed user CSS provider (no style.css found)");
            }
        });
        return;
    };

    match std::fs::read_to_string(&path) {
        Ok(css) => {
            let provider = gtk4::CssProvider::new();
            provider.load_from_string(&css);

            // Success — swap: remove old provider first, then add new
            USER_CSS_PROVIDER.with(|cell| {
                if let Some(old) = cell.borrow_mut().take() {
                    gtk4::style_context_remove_provider_for_display(&display, &old);
                }
            });

            gtk4::style_context_add_provider_for_display(&display, &provider, USER_CSS_PRIORITY);

            USER_CSS_PROVIDER.with(|cell| {
                *cell.borrow_mut() = Some(provider);
            });

            info!(
                "Loaded user CSS from: {} (priority={})",
                path.display(),
                USER_CSS_PRIORITY
            );
        }
        Err(e) => {
            // Read failed — keep the old provider intact
            warn!(
                "Failed to read user CSS from {}: {} — keeping current CSS",
                path.display(),
                e
            );
        }
    }
}

/// Generate CSS string from configuration and theme palette.
pub(crate) fn generate_css(
    config: &Config,
    palette: &ThemePalette,
    popover_palette: Option<&ThemePalette>,
) -> String {
    // Get CSS variables from theme palette
    let css_vars = palette.css_vars_block();

    // Per-widget CSS overrides (background_color, etc. from [widgets.xxx] sections)
    let per_widget_css = ThemePalette::generate_per_widget_css(config);

    // Popover polarity overrides (scoped under .popover)
    let popover_css = popover_palette
        .map(|p| p.css_popover_vars_block())
        .unwrap_or_default();

    // Utility CSS shared across widgets and surfaces
    let utility_css = widgets::css::utility_css(config);

    // Widget-specific CSS
    let widget_css = widgets::css::widget_css(config);

    format!(
        "{}\n{}\n{}\n{}\n{}",
        css_vars, per_widget_css, popover_css, utility_css, widget_css
    )
}

#[cfg(test)]
#[path = "bar_tests.rs"]
mod tests;
