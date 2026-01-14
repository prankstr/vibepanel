//! Bar window implementation using GTK4 and layer-shell.

use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::RefCell;
use std::path::PathBuf;
use tracing::{debug, info, warn};

use vibepanel_core::config::{WidgetEntry, WidgetOrGroup};
use vibepanel_core::{Config, ThemePalette};

use crate::sectioned_bar::SectionedBar;
use crate::styles::class;
use crate::widgets::{
    self, BarState, QuickSettingsConfig, WidgetConfig, WidgetFactory, apply_widget_color,
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
    let bar_height = config.bar.size as i32;

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
    window.set_layer(Layer::Top);

    // Bind to specific monitor - this should handle width automatically
    window.set_monitor(Some(monitor));
    debug!("Bar bound to monitor: {:?}", monitor.connector());

    // Anchor to top edge, stretch horizontally
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Left, true);
    window.set_anchor(Edge::Right, true);
    window.set_anchor(Edge::Bottom, false);

    // Reserve space (exclusive zone) so other windows don't overlap
    window.auto_exclusive_zone_enable();

    // Bar doesn't need keyboard input
    window.set_keyboard_mode(KeyboardMode::None);

    // Set margins from config (legacy behavior)
    // We keep window margins at 0 for left/right so the bar window
    // fills the monitor width; outer_margin is applied inside the
    // bar content instead.
    let margin = config.bar.outer_margin as i32;
    window.set_margin(Edge::Top, 0);
    window.set_margin(Edge::Left, 0);
    window.set_margin(Edge::Right, 0);

    // Create the bar container using SectionedBar for proper left/center/right layout
    let bar_box = SectionedBar::new(
        config.bar.widget_spacing as i32,
        config.bar.section_edge_margin as i32,
        config.widgets.left_has_expander(),
        config.widgets.right_has_expander(),
    );
    bar_box.add_css_class(class::BAR);
    bar_box.set_hexpand(true);
    bar_box.set_vexpand(true);

    // Wrap bar_box in an outer container so we can inset the
    // visible bar from the top, left, and right edges while
    // keeping the window and exclusive zone full-width.
    let outer_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    outer_box.add_css_class(class::BAR_SHELL);
    outer_box.set_hexpand(true);
    outer_box.set_vexpand(true);

    // Top spacer: empty area above the bar content.
    if margin > 0 {
        let spacer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        spacer.set_size_request(-1, margin);
        spacer.add_css_class(class::BAR_MARGIN_SPACER);
        outer_box.append(&spacer);
    }

    // Inner horizontal box adds left/right padding via CSS.
    let inner_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    inner_box.add_css_class(class::BAR_SHELL_INNER);
    inner_box.set_hexpand(true);
    inner_box.set_vexpand(false);
    inner_box.append(&bar_box);

    outer_box.append(&inner_box);

    // Find quick_settings config from widget entries to configure the window.
    // Get options from [widgets.quick_settings] if defined.
    let qs_cards_config = config
        .widgets
        .get_options("quick_settings")
        .map(|opts| {
            let entry = WidgetEntry::with_options("quick_settings", opts);
            QuickSettingsConfig::from_entry(&entry).cards
        })
        .unwrap_or_default();

    // Create handle for this bar's Quick Settings window.
    // The window itself is created lazily on first open and destroyed on close.
    let qs_handle = crate::widgets::QuickSettingsWindowHandle::new(app.clone(), qs_cards_config);

    // Create left section
    let left_section = create_section("left", config, state, &qs_handle, Some(output_id));
    bar_box.set_start_widget(Some(&left_section));

    // Create center section only if notch is enabled or there are center widgets
    // Without a center widget, the layout manager uses linear allocation
    let has_center_content =
        config.bar.notch_enabled || !config.widgets.resolved_center().is_empty();
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

    info!(
        "Bar window created: size={}px, margin={}px, monitor={:?}, widgets={}",
        config.bar.size,
        config.bar.outer_margin,
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

            // Apply first widget's color to the group island for unified background
            if let Some(first_entry) = group.first()
                && let Some(ref color) = first_entry.color
            {
                apply_widget_color(&island, color);
            }

            // Create inner content box (matching BaseWidget structure)
            let content = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            content.add_css_class(class::CONTENT);
            content.set_vexpand(true);
            content.set_valign(gtk4::Align::Fill);
            island.append(&content);

            let mut count = 0;
            for entry in group {
                if let Some(built) = WidgetFactory::build(entry, Some(qs_handle), output_id) {
                    // Remove the .widget class from this widget since it's inside a group
                    built.widget.remove_css_class(class::WIDGET);
                    content.append(&built.widget);
                    state.add_handle(built.handle);
                    count += 1;
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

/// Create the center section, optionally with notch spacer.
fn create_center_section(
    config: &Config,
    state: &mut BarState,
    qs_handle: &crate::widgets::QuickSettingsWindowHandle,
    output_id: Option<&str>,
) -> gtk4::Box {
    let section = gtk4::Box::new(
        gtk4::Orientation::Horizontal,
        config.bar.widget_spacing as i32,
    );
    section.add_css_class(class::BAR_SECTION_CENTER);

    if config.bar.notch_enabled {
        // Notch mode: center section is just a fixed-width empty spacer for the notch.
        // Users place widgets adjacent to the notch using the spacer widget in left/right sections.
        let notch_width = config.bar.effective_notch_width();
        section.set_size_request(notch_width as i32, -1);

        debug!(
            "Notch mode: {}px center spacer{}",
            notch_width,
            if config.bar.notch_width == 0 {
                " (auto)"
            } else {
                ""
            }
        );
    } else {
        // Non-notch mode: simple centered section with widgets
        let mut widget_count = 0;
        for item in &config.widgets.resolved_center() {
            widget_count += build_widget_or_group(item, &section, state, qs_handle, output_id);
        }

        debug!("Created center section with {} widget(s)", widget_count);
    }

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
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_USER,
        );
        debug!(
            "CSS loaded and applied (dark_mode={})",
            palette.is_dark_mode
        );

        // Load user's custom style.css if it exists
        load_user_css(&display);
    } else {
        warn!("No default display available, CSS styling not applied");
    }
}

/// Priority for user CSS - higher than everything else to ensure overrides work.
/// USER = 800, we use 900 to be above all internal styles (which use USER + 10 max).
const USER_CSS_PRIORITY: u32 = gtk4::STYLE_PROVIDER_PRIORITY_USER + 100;

// Thread-local storage for the user CSS provider so we can replace it on reload
thread_local! {
    static USER_CSS_PROVIDER: RefCell<Option<gtk4::CssProvider>> = const { RefCell::new(None) };
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

    // Utility CSS shared across widgets and surfaces
    let utility_css = widgets::css::utility_css();

    // Widget-specific CSS
    let widget_css = widgets::css::widget_css(config);

    format!("{}\n{}\n{}", css_vars, utility_css, widget_css)
}
