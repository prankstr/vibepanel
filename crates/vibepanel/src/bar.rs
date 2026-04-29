//! Bar window implementation using GTK4 and layer-shell.

use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, GestureClick, Overlay, gdk};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tracing::{debug, info, warn};

use vibepanel_core::config::{WidgetEntry, WidgetOrGroup};
use vibepanel_core::{Config, ThemePalette};

/// Horizontal spacing (px) between widgets inside a merge group.
/// Matches the `.content` horizontal padding in `css/bar.rs`.
const MERGE_GROUP_SPACING: i32 = 10;

use crate::popover_tracker::PopoverTracker;
use crate::sectioned_bar::SectionedBar;
use crate::services::config_manager::{ConfigManager, ThemeCallbackGuard};
use crate::services::tooltip::TooltipManager;
use crate::styles::{class, state, widget as style_widget};
use crate::widgets::{
    self, BarState, MenuHandle, PopoverKind, QuickSettingsConfig, RippleHandle,
    SystemPopoverBinding, WidgetConfig, WidgetFactory, popover_kind_for,
    trigger_ripple_from_gesture,
};

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
    // Window height determines the exclusive zone (via auto_exclusive_zone_enable).
    // - When bar is visible (opacity > 0): include padding on both sides
    // - When bar is transparent (opacity = 0): exclusive zone = size only
    //   The screen-edge padding offsets widgets visually but the inner padding is 0 via CSS
    let bar_height = if config.bar.background_opacity > 0.0 {
        config.bar.size as i32 + 2 * config.bar.padding as i32
    } else {
        // Islands mode: exclusive zone = widget height only
        config.bar.size as i32
    };

    let window = ApplicationWindow::builder()
        .application(app)
        .title("vibepanel")
        .decorated(false)
        .resizable(false)
        .default_height(bar_height)
        .build();

    window.add_css_class(class::BAR_WINDOW);

    // Initialize layer-shell
    window.init_layer_shell();
    window.set_namespace(Some("vibepanel"));
    window.set_layer(Layer::Top);

    // Bind to specific monitor - this should handle width automatically
    window.set_monitor(Some(monitor));
    debug!("Bar bound to monitor: {:?}", monitor.connector());

    // Anchor to the configured edge, stretch horizontally
    let is_bottom = config.bar.is_bottom();
    window.set_anchor(Edge::Top, !is_bottom);
    window.set_anchor(Edge::Left, true);
    window.set_anchor(Edge::Right, true);
    window.set_anchor(Edge::Bottom, is_bottom);

    // Reserve space (exclusive zone) so other windows don't overlap
    window.auto_exclusive_zone_enable();

    // Bar doesn't need keyboard input
    window.set_keyboard_mode(KeyboardMode::None);

    // Set margins from config (legacy behavior)
    // We keep window margins at 0 for left/right so the bar window
    // fills the monitor width; screen_margin is applied inside the
    // bar content instead.
    let margin = config.bar.screen_margin as i32;

    // Create the bar container using SectionedBar for proper left/center/right layout
    let bar_box = SectionedBar::new(
        config.bar.spacing as i32,
        config.bar.inset as i32,
        config.widgets.left_has_expander(),
        config.widgets.right_has_expander(),
    );
    bar_box.add_css_class(class::BAR);
    bar_box.set_hexpand(true);
    bar_box.set_vexpand(true);

    // Wrap bar_box in an outer container so we can inset the
    // visible bar from the anchored edge and sides while
    // keeping the window and exclusive zone full-width.
    let outer_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    outer_box.add_css_class(class::BAR_SHELL);
    outer_box.set_hexpand(true);
    outer_box.set_vexpand(true);

    // Spacer: empty area between bar content and screen edge.
    // For top bar, spacer goes above (pushes bar down from top edge).
    // For bottom bar, spacer goes below (pushes bar up from bottom edge).
    let spacer = if margin > 0 {
        let s = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        s.set_size_request(-1, margin);
        s.add_css_class(class::BAR_MARGIN_SPACER);
        Some(s)
    } else {
        None
    };

    if !is_bottom && let Some(ref spacer) = spacer {
        outer_box.append(spacer);
    }

    // Inner horizontal box adds left/right padding via CSS.
    let inner_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    inner_box.add_css_class(class::BAR_SHELL_INNER);
    inner_box.set_hexpand(true);
    inner_box.set_vexpand(false);
    inner_box.append(&bar_box);

    outer_box.append(&inner_box);

    if is_bottom && let Some(ref spacer) = spacer {
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

    // Create left section
    let left_section = create_section("left", config, state, &qs_handle, Some(output_id));
    bar_box.set_start_widget(Some(&left_section));

    // Create center section only if there are center widgets
    // Without a center widget, the layout manager uses linear allocation
    let has_center_content = !config.widgets.resolved_center().is_empty();
    if has_center_content {
        let center_section = create_center_section(config, state, &qs_handle, Some(output_id));
        bar_box.set_center_widget(Some(&center_section));
    }

    // Create right section
    let right_section = create_section("right", config, state, &qs_handle, Some(output_id));
    bar_box.set_end_widget(Some(&right_section));

    window.set_child(Some(&outer_box));

    // Set window width to the target monitor's width on map.
    // We capture the geometry now rather than using monitor_at_surface() later,
    // because the surface might not be on the correct monitor yet at map time.
    let target_geometry = monitor.geometry();
    let target_width = target_geometry.width();

    let is_island_mode = config.bar.background_opacity == 0.0;

    let bar_box_for_blur = bar_box.clone();
    window.connect_map(move |win| {
        win.set_default_size(target_width, bar_height);
        debug!(
            "Set window width to target monitor size: {}px",
            target_width
        );

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

/// Build a single widget or a group of widgets sharing one island.
///
/// Returns the number of widgets built (for counting purposes).
fn build_widget_or_group(
    item: &WidgetOrGroup,
    container: &gtk4::Box,
    state: &mut BarState,
    qs_handle: &crate::widgets::QuickSettingsWindowHandle,
    output_id: Option<&str>,
) -> usize {
    match item {
        WidgetOrGroup::Single(entry) => {
            // Single widget with its own island
            if let Some(built) = WidgetFactory::build(entry, Some(qs_handle), output_id) {
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

            // Create a shared island container for the group
            let island = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            island.add_css_class(class::WIDGET_WRAPPER);

            // Create inner content box (matching BaseWidget structure)
            let content = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            content.add_css_class(class::CONTENT);
            content.set_vexpand(true);
            content.set_valign(gtk4::Align::Fill);

            // Group surface — transparent in CSS. Direct children paint their
            // own backgrounds so hover colors composite once over the bar.
            let surface = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            surface.add_css_class(class::WIDGET);
            surface.add_css_class(class::WIDGET_GROUP);
            surface.set_overflow(gtk4::Overflow::Hidden);
            surface.set_hexpand(true);
            surface.set_vexpand(true);

            surface.append(&content);
            island.append(&surface);

            // Partition into runs of same-popover widgets for merge grouping.
            // Widgets with custom click handlers stay unmergeable so their
            // on_click_right / on_click_middle commands aren't silently lost.
            let kinds: Vec<PopoverKind> = group
                .iter()
                .map(|e| {
                    let (right, middle) = ConfigManager::global().get_click_handlers(&e.name);
                    if right.is_some() || middle.is_some() {
                        PopoverKind::Unmergeable
                    } else {
                        popover_kind_for(&e.name)
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
                            // Strip the standalone wrapper class so the wrapper-hover
                            // rule doesn't fire — per-item hover is handled by a
                            // group-scoped rule that paints on the .widget-item.
                            built.widget.remove_css_class(class::WIDGET_WRAPPER);
                            built.widget.add_css_class(&entry.name.replace('_', "-"));
                            // Grouped hover uses a large box-shadow spread to refill
                            // the cell around the pill; this clips it to item bounds.
                            built.widget.set_overflow(gtk4::Overflow::Hidden);
                            content.append(&built.widget);
                            state.add_handle(built.handle);
                            n += 1;
                        }
                    }
                    n
                };

            for (kind, start, end) in &runs {
                let run_entries = &group[*start..*end];
                let run_len = end - start;

                if run_len >= 2 && *kind != PopoverKind::Unmergeable {
                    let merged = build_merge_group(run_entries, *kind, &content, state);
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

/// Partition `PopoverKind` values into runs of adjacent equal values.
/// `Unmergeable` entries are never grouped — each becomes its own singleton run.
fn compute_merge_runs(kinds: &[PopoverKind]) -> Vec<(PopoverKind, usize, usize)> {
    let mut runs = Vec::new();
    if kinds.is_empty() {
        return runs;
    }

    let mut start = 0;
    while start < kinds.len() {
        let kind = kinds[start];
        if kind == PopoverKind::Unmergeable {
            runs.push((kind, start, start + 1));
            start += 1;
        } else {
            let mut end = start + 1;
            while end < kinds.len() && kinds[end] == kind {
                end += 1;
            }
            runs.push((kind, start, end));
            start = end;
        }
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
) -> usize {
    // Overlay wrapper — ripple sits on top of the content box.
    let wrapper = Overlay::new();
    wrapper.add_css_class(class::WIDGET_MERGE_GROUP);
    wrapper.add_css_class(state::CLICKABLE);
    // Merged groups paint as a single button; only the leading widget's
    // per-widget background class applies.
    wrapper.add_css_class(&entries[0].name.replace('_', "-"));
    // Required for the merge-group hover pill: its 9999px box-shadow
    // refill is clipped here so it cannot bleed outside the group.
    wrapper.set_overflow(gtk4::Overflow::Hidden);

    let inner_content = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    inner_content.add_css_class(class::MERGE_GROUP_CONTENT);
    inner_content.set_vexpand(true);
    inner_content.set_valign(gtk4::Align::Fill);
    inner_content.set_spacing(MERGE_GROUP_SPACING);
    wrapper.set_child(Some(&inner_content));

    let ripple_handle = RippleHandle::new();
    // Wrap the ripple DrawingArea in a Box that establishes a fully-rounded
    // clip, so the ripple matches the inner pill shape on hover. The merge
    // group itself uses position-aware radius (square at seams in mixed
    // groups), which would otherwise leak the ripple into the corners.
    let ripple_clip = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    ripple_clip.add_css_class(class::WIDGET_MERGE_GROUP_RIPPLE_CLIP);
    ripple_clip.set_overflow(gtk4::Overflow::Hidden);
    ripple_clip.append(ripple_handle.widget());
    wrapper.add_overlay(&ripple_clip);
    wrapper.set_measure_overlay(&ripple_clip, true);

    let widget_name = entries[0].name.clone();
    let menu_handle = MenuHandle::new_placeholder(widget_name, wrapper.clone());

    let binding = match kind {
        PopoverKind::System => Some(SystemPopoverBinding::new_for_menu(&menu_handle)),
        _ => {
            warn!("Merge group for {:?} popover not yet supported", kind);
            None
        }
    };

    let Some(binding) = binding else {
        return 0;
    };

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
            }
        });
    }

    wrapper.add_controller(gesture_click.clone());

    let mut built_widgets: Vec<widgets::BuiltWidget> = Vec::new();
    for entry in entries {
        if let Some(built) = WidgetFactory::build_passive(entry, &binding) {
            built_widgets.push(built);
        }
    }

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

    // Register the shared menu handle under ALL participating widget names
    // for IPC popover control. Uses underscores (config convention).
    for entry in entries {
        crate::popover_registry::register(
            &entry.name,
            menu_handle.clone() as Rc<dyn crate::popover_registry::PopoverToggleable>,
        );
    }

    // Keep the menu handle, gesture, and ripple alive
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
) -> gtk4::Box {
    let section = gtk4::Box::new(
        gtk4::Orientation::Horizontal,
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
        widget_count += build_widget_or_group(item, &section, state, qs_handle, output_id);
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
) -> gtk4::Box {
    let section = gtk4::Box::new(gtk4::Orientation::Horizontal, config.bar.spacing as i32);
    section.add_css_class(class::BAR_SECTION_CENTER);

    let mut widget_count = 0;
    for item in &config.widgets.resolved_center() {
        widget_count += build_widget_or_group(item, &section, state, qs_handle, output_id);
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
    debug!("  state_success = {}", palette.state_success);

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

        let provider = gtk4::CssProvider::new();
        provider.load_from_string(&format!(
            ".{} {{ min-width: 0; }} .{} {{ transition: none; }}",
            style_widget::WORKSPACE_GROW_IN,
            style_widget::WORKSPACE_GROW_IN_NOTRANS
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
fn generate_css(
    config: &Config,
    palette: &ThemePalette,
    popover_palette: Option<&ThemePalette>,
) -> String {
    // Get CSS variables from theme palette
    let css_vars = palette.css_vars_block();

    // Per-widget CSS overrides (background_color, etc. from [widgets.xxx] sections)
    let per_widget_css = ThemePalette::generate_per_widget_css(config);

    // Popover polarity overrides (scoped under .vp-surface-popover)
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
mod tests {
    use super::*;
    use crate::widgets::PopoverKind::{System, Unmergeable};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let unique = format!(
                "{}_{}_{}",
                name,
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let path = std::env::temp_dir().join(unique);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn merge_runs_empty() {
        assert_eq!(compute_merge_runs(&[]), vec![]);
    }

    #[test]
    fn merge_runs_unmergeable_never_grouped() {
        let runs = compute_merge_runs(&[Unmergeable, Unmergeable, Unmergeable]);
        assert_eq!(
            runs,
            vec![
                (Unmergeable, 0, 1),
                (Unmergeable, 1, 2),
                (Unmergeable, 2, 3),
            ]
        );
    }

    #[test]
    fn merge_runs_system_grouping() {
        // Consecutive System entries merge into one run
        assert_eq!(
            compute_merge_runs(&[System, System, System]),
            vec![(System, 0, 3)]
        );
        // Unmergeable breaks a System run; singleton System stays singleton
        assert_eq!(
            compute_merge_runs(&[System, System, Unmergeable, System]),
            vec![(System, 0, 2), (Unmergeable, 2, 3), (System, 3, 4)],
        );
        // Single System is its own run
        assert_eq!(compute_merge_runs(&[System]), vec![(System, 0, 1)]);
    }

    #[test]
    fn user_css_search_paths_config_dir_first() {
        let dir = std::path::Path::new("/custom/config/dir");
        let paths = user_css_search_paths_from_env(
            Some(dir),
            Some(Path::new("/xdg-home")),
            Some(Path::new("/home/test")),
        );
        assert_eq!(
            paths,
            vec![
                dir.join("style.css"),
                PathBuf::from("/xdg-home/vibepanel/style.css"),
                PathBuf::from("/home/test/.config/vibepanel/style.css"),
                PathBuf::from("style.css"),
            ]
        );
    }

    #[test]
    fn user_css_search_paths_deduplicates() {
        let paths = user_css_search_paths_from_env(
            Some(Path::new("/home/test/.config/vibepanel")),
            Some(Path::new("/home/test/.config")),
            Some(Path::new("/home/test")),
        );
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/home/test/.config/vibepanel/style.css"),
                PathBuf::from("style.css"),
            ]
        );
    }

    #[test]
    fn user_css_search_paths_none_config_dir_still_has_entries() {
        let paths = user_css_search_paths_from_env(None, None, None);
        assert_eq!(
            paths,
            vec![PathBuf::from("style.css")],
            "without config/XDG/HOME, only the CWD fallback remains"
        );
    }

    #[test]
    fn user_css_search_paths_xdg_dedup_with_home() {
        let paths = user_css_search_paths_from_env(
            None,
            Some(Path::new("/home/test/.config")),
            Some(Path::new("/home/test")),
        );
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/home/test/.config/vibepanel/style.css"),
                PathBuf::from("style.css"),
            ]
        );
    }

    #[test]
    fn find_user_css_returns_none_when_no_file_exists() {
        // Pass a config_dir that does not contain style.css; without HOME/XDG
        // overrides the CWD fallback is unlikely to exist either, but we use an
        // empty tmp dir as config_dir so that slot is definitely absent.
        let tmp = TestDir::new("vibepanel_test_missing_css");
        // An empty directory: none of the candidate paths will exist.
        let result =
            user_css_search_paths_from_env(Some(tmp.path()), Some(tmp.path()), Some(tmp.path()))
                .into_iter()
                .find(|p| p.exists());
        assert_eq!(result, None);
    }

    #[test]
    fn find_user_css_finds_existing_file() {
        // Create a real style.css in a temp directory and point config_dir at
        // it so find_user_css returns the config-adjacent path (highest priority).
        let tmp = TestDir::new("vibepanel_test_css");
        let style_path = tmp.path().join("style.css");
        std::fs::write(&style_path, "/* test */").unwrap();

        // Use the injected-env helper so we don't depend on real HOME/XDG.
        let found =
            user_css_search_paths_from_env(Some(tmp.path()), Some(tmp.path()), Some(tmp.path()))
                .into_iter()
                .find(|p| p.exists());

        assert_eq!(found, Some(style_path));
    }
}
