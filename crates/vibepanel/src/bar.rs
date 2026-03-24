//! Bar window implementation using GTK4 and layer-shell.

use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, GestureClick, Overlay, gdk};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use tracing::{debug, info, warn};

use vibepanel_core::config::{WidgetEntry, WidgetOrGroup};
use vibepanel_core::{Config, ThemePalette};

/// Horizontal spacing (px) between widgets inside a merge group.
/// Matches the `.content` horizontal padding in `css/bar.rs`.
const MERGE_GROUP_SPACING: i32 = 10;

use crate::popover_tracker::PopoverTracker;
use crate::sectioned_bar::SectionedBar;
use crate::services::config_manager::ConfigManager;
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

    window.connect_map(move |win| {
        win.set_default_size(target_width, bar_height);
        debug!(
            "Set window width to target monitor size: {}px",
            target_width
        );
    });

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
            island.add_css_class(class::WIDGET);
            island.add_css_class(class::WIDGET_GROUP);

            // Add the first widget's name as a CSS class for per-widget CSS variable targeting
            // Normalize underscores to hyphens for CSS conventions
            if let Some(first_entry) = group.first() {
                island.add_css_class(&first_entry.name.replace('_', "-"));
            }

            // Create inner content box (matching BaseWidget structure)
            let content = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            content.add_css_class(class::CONTENT);
            content.set_vexpand(true);
            content.set_valign(gtk4::Align::Fill);

            // Visual surface for rounded background (see WIDGET_SURFACE doc).
            let surface = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            surface.add_css_class(class::WIDGET_SURFACE);
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
            let build_individually =
                |entries: &[WidgetEntry], content: &gtk4::Box, state: &mut BarState| -> usize {
                    let mut n = 0;
                    for entry in entries {
                        if let Some(built) = WidgetFactory::build(entry, Some(qs_handle), output_id)
                        {
                            built.widget.remove_css_class(class::WIDGET);
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
    wrapper.set_overflow(gtk4::Overflow::Hidden);

    let inner_content = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    inner_content.add_css_class(class::MERGE_GROUP_CONTENT);
    inner_content.set_vexpand(true);
    inner_content.set_valign(gtk4::Align::Fill);
    inner_content.set_spacing(MERGE_GROUP_SPACING);
    wrapper.set_child(Some(&inner_content));

    let ripple_handle = RippleHandle::new();
    wrapper.add_overlay(ripple_handle.widget());
    wrapper.set_measure_overlay(ripple_handle.widget(), true);

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

    // Create theme palette and generate CSS
    let palette = ThemePalette::from_config(config);
    let css = generate_css(config, &palette);

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
        load_user_css(&display);
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

/// Search paths for user style.css, following XDG conventions.
fn user_css_search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // 1. $XDG_CONFIG_HOME/vibepanel/style.css
    if let Ok(xdg_config) = std::env::var("XDG_CONFIG_HOME") {
        paths.push(PathBuf::from(xdg_config).join("vibepanel/style.css"));
    }

    // 2. ~/.config/vibepanel/style.css
    if let Ok(home) = std::env::var("HOME") {
        paths.push(PathBuf::from(home).join(".config/vibepanel/style.css"));
    }

    // 3. ./style.css (current working directory)
    paths.push(PathBuf::from("style.css"));

    paths
}

/// Find user's style.css file if it exists.
fn find_user_css() -> Option<PathBuf> {
    user_css_search_paths()
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

/// Load user's custom CSS from style.css with highest priority.
fn load_user_css(display: &gtk4::gdk::Display) {
    let Some(path) = find_user_css() else {
        debug!("No user style.css found");
        return;
    };

    match std::fs::read_to_string(&path) {
        Ok(css) => {
            let provider = gtk4::CssProvider::new();
            provider.load_from_string(&css);

            gtk4::style_context_add_provider_for_display(display, &provider, USER_CSS_PRIORITY);

            // Store the provider so we can remove it later on reload
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
            warn!("Failed to read user CSS from {}: {}", path.display(), e);
        }
    }
}

/// Reload user's custom CSS (called when style.css file changes).
pub fn reload_user_css() {
    let Some(display) = gtk4::gdk::Display::default() else {
        warn!("No default display available for CSS reload");
        return;
    };

    // Remove the old provider if it exists
    USER_CSS_PROVIDER.with(|cell| {
        if let Some(old_provider) = cell.borrow_mut().take() {
            gtk4::style_context_remove_provider_for_display(&display, &old_provider);
            debug!("Removed old user CSS provider");
        }
    });

    // Load the new CSS
    load_user_css(&display);
}

/// Generate CSS string from configuration and theme palette.
fn generate_css(config: &Config, palette: &ThemePalette) -> String {
    // Get CSS variables from theme palette
    let css_vars = palette.css_vars_block();

    // Per-widget CSS overrides (background_color, etc. from [widgets.xxx] sections)
    let per_widget_css = ThemePalette::generate_per_widget_css(config);

    // Utility CSS shared across widgets and surfaces
    let utility_css = widgets::css::utility_css(config);

    // Widget-specific CSS
    let widget_css = widgets::css::widget_css(config);

    format!(
        "{}\n{}\n{}\n{}",
        css_vars, per_widget_css, utility_css, widget_css
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgets::PopoverKind::{System, Unmergeable};

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
}
