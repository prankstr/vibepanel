//! Configuration manager with live reload support.
//!
//! This service watches the configuration file for changes and coordinates
//! updates across all subsystems when the config changes.
//!
//! ## Architecture
//!
//! - A file watcher thread monitors `config.toml` for modifications.
//! - On change, the new config is parsed and validated.
//! - If valid, changes are dispatched to the GTK main thread via glib::idle_add_once.
//! - The main thread applies changes by calling `reconfigure` on each subsystem.
//!
//! ## Supported Live Reload
//!
//! Currently supports:
//! - `icons.theme`: Switches icon backend (Material ↔ GTK themes)
//! - `theme.*`: Updates colors, palette, CSS variables
//!
//! Structural changes (widget list, layout, bar count) are logged but require restart.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use gtk4::glib;
use notify_debouncer_mini::{DebounceEventResult, new_debouncer, notify::RecursiveMode};
use tracing::{debug, error, info, warn};

use vibepanel_core::{Config, ThemePalette, ThemeSizes};

/// Debounce interval (in ms) for file change events. Editors often trigger
/// multiple events for a single save; this batches them into one reload.
const FILE_CHANGE_DEBOUNCE_MS: u64 = 300;

use crate::bar;
use crate::services::bar_manager::BarManager;
use crate::services::icons::IconsService;
use crate::services::surfaces::SurfaceStyleManager;
use crate::services::tooltip::TooltipManager;

/// Messages sent from the file watcher thread to the GTK main thread.
#[derive(Debug)]
pub enum ConfigMessage {
    /// A new valid config was loaded.
    Reloaded(Box<Config>),
    /// Config file changed but failed to load/validate.
    Error(String),
    /// User style.css file changed and should be reloaded.
    StyleCssChanged,
}

/// Send a config message to the main thread via glib::idle_add_once.
fn send_config_message(msg: ConfigMessage) {
    glib::idle_add_once(move || {
        ConfigManager::global().handle_config_message(msg);
    });
}

/// Manages configuration state and live reload.
///
/// This is a singleton service that:
/// - Holds the current configuration
/// - Watches the config file for changes
/// - Coordinates updates to subsystems when config changes
pub struct ConfigManager {
    /// Current configuration.
    config: RefCell<Config>,
    /// Path to the config file being watched (if any).
    config_path: RefCell<Option<PathBuf>>,
    /// Shutdown flag for the file watcher thread.
    shutdown_flag: Arc<AtomicBool>,
}

// Thread-local singleton storage
thread_local! {
    static CONFIG_MANAGER_INSTANCE: RefCell<Option<Rc<ConfigManager>>> = const { RefCell::new(None) };
}

impl ConfigManager {
    /// Create a new ConfigManager with the given initial config.
    fn new(config: Config, config_path: Option<PathBuf>) -> Rc<Self> {
        Rc::new(Self {
            config: RefCell::new(config),
            config_path: RefCell::new(config_path),
            shutdown_flag: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Get the global ConfigManager singleton.
    ///
    /// Panics if `init_global` hasn't been called.
    pub fn global() -> Rc<Self> {
        CONFIG_MANAGER_INSTANCE.with(|cell| {
            cell.borrow()
                .as_ref()
                .expect("ConfigManager not initialized; call init_global first")
                .clone()
        })
    }

    /// Initialize the global ConfigManager singleton.
    ///
    /// Must be called once during application startup, before `global()` is used.
    pub fn init_global(config: Config, config_path: Option<PathBuf>) {
        CONFIG_MANAGER_INSTANCE.with(|cell| {
            let mut opt = cell.borrow_mut();
            if opt.is_some() {
                warn!("ConfigManager already initialized, ignoring init_global call");
                return;
            }
            *opt = Some(ConfigManager::new(config, config_path));
        });
    }

    /// Get the computed theme sizes from the current configuration.
    ///
    /// This computes sizes based on the current bar size and theme scale constants.
    /// Widgets can use this to get the default icon sizes, font sizes, etc.
    pub fn theme_sizes(&self) -> ThemeSizes {
        let config = self.config.borrow();
        let palette = ThemePalette::from_config(&config);
        palette.sizes.clone()
    }

    /// Get the bar size (height) from the current configuration.
    pub fn bar_size(&self) -> u32 {
        self.config.borrow().bar.size
    }

    /// Get the bar outer margin from the current configuration.
    pub fn bar_outer_margin(&self) -> u32 {
        self.config.borrow().bar.outer_margin
    }

    /// Get the popover offset (gap between widget and popover) from the current configuration.
    pub fn popover_offset(&self) -> u32 {
        self.config.borrow().bar.popover_offset
    }

    /// Get the widget opacity from the current theme configuration.
    pub fn widget_opacity(&self) -> f64 {
        self.config.borrow().theme.widget_opacity
    }

    /// Start watching the config file for changes.
    ///
    /// This spawns a background thread that monitors the config file. When changes
    /// are detected, the new config is parsed and sent to the GTK main thread.
    ///
    /// Does nothing if no config file path is set (using defaults).
    pub fn start_watching(self: &Rc<Self>) {
        let config_path = self.config_path.borrow().clone();
        let Some(path) = config_path else {
            info!("No config file to watch (using defaults)");
            return;
        };

        if !path.exists() {
            warn!(
                "Config file does not exist, cannot watch: {}",
                path.display()
            );
            return;
        }

        info!("Starting config file watcher for: {}", path.display());

        // Clone path for the watcher thread
        let watch_path = path.clone();
        let shutdown_flag = self.shutdown_flag.clone();

        // Spawn file watcher thread
        thread::spawn(move || {
            Self::run_file_watcher(watch_path, shutdown_flag);
        });
    }

    /// Run the file watcher loop (called on a background thread).
    fn run_file_watcher(path: PathBuf, shutdown_flag: Arc<AtomicBool>) {
        // Debounce events to avoid multiple reloads for a single save
        let debounce_duration = Duration::from_millis(FILE_CHANGE_DEBOUNCE_MS);

        // Canonicalize the path so we can compare with absolute paths from notify
        let path_for_handler = match path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to canonicalize config path: {}", e);
                return;
            }
        };

        // Also watch for style.css in the same directory
        let style_css_path = path_for_handler.parent().map(|p| p.join("style.css"));

        let mut debouncer =
            match new_debouncer(debounce_duration, move |res: DebounceEventResult| {
                match res {
                    Ok(events) => {
                        // Check if any event is for our config file
                        let config_changed = events.iter().any(|e| e.path == path_for_handler);
                        if config_changed {
                            debug!("Config file change detected");
                            Self::reload_and_send(&path_for_handler);
                        }

                        // Check if style.css changed
                        if let Some(ref style_path) = style_css_path {
                            let style_changed = events.iter().any(|e| e.path == *style_path);
                            if style_changed {
                                debug!("User style.css change detected");
                                send_config_message(ConfigMessage::StyleCssChanged);
                            }
                        }
                    }
                    Err(err) => {
                        error!("File watcher error: {}", err);
                    }
                }
            }) {
                Ok(d) => d,
                Err(e) => {
                    error!("Failed to create file watcher: {}", e);
                    return;
                }
            };

        // Watch the config file's parent directory (more reliable than watching file directly)
        // Use the original path for watching since we already canonicalized for comparison
        let canonical_path = match path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to canonicalize config path for watching: {}", e);
                return;
            }
        };
        let watch_dir = canonical_path.parent().unwrap_or(&canonical_path);
        if let Err(e) = debouncer
            .watcher()
            .watch(watch_dir, RecursiveMode::NonRecursive)
        {
            error!("Failed to watch config directory: {}", e);
            return;
        }

        info!("File watcher started, watching: {}", watch_dir.display());

        // Keep the thread alive until shutdown is signaled
        // Use shorter sleep intervals to allow responsive shutdown
        while !shutdown_flag.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(500));
        }

        debug!("Config file watcher thread shutting down");
    }

    /// Reload config from file and send result to GTK thread via idle_add_once.
    fn reload_and_send(path: &std::path::Path) {
        match Config::load(path) {
            Ok(new_config) => {
                // Validate the new config
                if let Err(e) = new_config.validate() {
                    let msg = format!("Config validation failed: {}", e);
                    warn!("{}", msg);
                    send_config_message(ConfigMessage::Error(msg));
                    return;
                }

                info!("Config reloaded successfully from: {}", path.display());
                send_config_message(ConfigMessage::Reloaded(Box::new(new_config)));
            }
            Err(e) => {
                let msg = format!("Failed to reload config: {}", e);
                warn!("{}", msg);
                send_config_message(ConfigMessage::Error(msg));
            }
        }
    }

    /// Handle a config message from the file watcher.
    /// Called via glib::idle_add_once from send_config_message.
    pub(crate) fn handle_config_message(&self, msg: ConfigMessage) {
        match msg {
            ConfigMessage::Reloaded(new_config) => {
                self.apply_config(*new_config);
            }
            ConfigMessage::Error(err) => {
                // Just log the error - keep using the old config
                error!("Config reload error: {}", err);
            }
            ConfigMessage::StyleCssChanged => {
                // Reload user CSS
                info!("Reloading user style.css...");
                crate::bar::reload_user_css();
            }
        }
    }

    /// Apply a new configuration, updating all subsystems.
    ///
    /// This is the central "fan-out" function that coordinates updates across
    /// all services and widgets when the config changes.
    fn apply_config(&self, new_config: Config) {
        let old_config = self.config.borrow().clone();

        info!("Applying new configuration...");

        // Update icons theme and/or weight
        if old_config.icons.theme != new_config.icons.theme
            || old_config.icons.weight != new_config.icons.weight
        {
            info!(
                "Icon config changed: theme {} -> {}, weight {} -> {}",
                old_config.icons.theme,
                new_config.icons.theme,
                old_config.icons.weight,
                new_config.icons.weight
            );
            IconsService::global().reconfigure(&new_config.icons.theme, new_config.icons.weight);
        }

        // Update theme/palette if theme config changed
        if config_theme_changed(&old_config, &new_config) {
            info!("Theme configuration changed, updating styles...");

            // Regenerate palette and update services
            let palette = ThemePalette::from_config(&new_config);
            let surface_styles = palette.surface_styles();

            // Update surface style manager
            SurfaceStyleManager::global().reconfigure(
                surface_styles.clone(),
                new_config.advanced.pango_font_rendering,
            );

            // Update tooltip manager
            TooltipManager::global().reconfigure(surface_styles);

            // Reload CSS with new theme values
            bar::load_css(&new_config);

            // Note: QuickSettingsWindow doesn't need explicit reload_styles() anymore.
            // It's destroyed on close and recreated with fresh styles on next open.

            debug!("Theme styles updated");
        }

        // Store the new config BEFORE rebuilding the bar, so widgets created
        // during rebuild will see the new config values (e.g., theme_sizes())
        *self.config.borrow_mut() = new_config.clone();

        // Check for structural changes that require bar rebuild
        if config_structure_changed(&old_config, &new_config) {
            info!("Structural configuration changed, rebuilding bar...");
            // Reload CSS first (in case theme also changed)
            bar::load_css(&new_config);
            // Rebuild the bar with new config
            if let Some(display) = gtk4::gdk::Display::default() {
                BarManager::global().reconfigure_all(&display, &new_config);
            }
        }

        info!("Configuration applied successfully");
    }

    /// Stop watching the config file.
    pub fn stop_watching(&self) {
        // Signal the watcher thread to shut down
        self.shutdown_flag.store(true, Ordering::Relaxed);
        debug!("Config watcher stopped");
    }
}

/// Check if theme-related config has changed.
fn config_theme_changed(old: &Config, new: &Config) -> bool {
    old.theme.mode != new.theme.mode
        || old.theme.accent != new.theme.accent
        || old.theme.bar_background_color != new.theme.bar_background_color
        || old.theme.bar_opacity != new.theme.bar_opacity
        || old.theme.widget_background_color != new.theme.widget_background_color
        || old.theme.widget_opacity != new.theme.widget_opacity
        || old.theme.states.success != new.theme.states.success
        || old.theme.states.warning != new.theme.states.warning
        || old.theme.states.urgent != new.theme.states.urgent
        || old.theme.typography.font_family != new.theme.typography.font_family
        || old.bar.border_radius != new.bar.border_radius
        || old.widgets.border_radius != new.widgets.border_radius
        // bar.size affects computed font sizes in ThemeSizes/SurfaceStyles
        || old.bar.size != new.bar.size
        // advanced.pango_font_rendering affects how fonts are applied
        || old.advanced.pango_font_rendering != new.advanced.pango_font_rendering
}

/// Check if structural configuration has changed (requires bar rebuild).
fn config_structure_changed(old: &Config, new: &Config) -> bool {
    if old.bar.size != new.bar.size {
        debug!("bar.size changed ({} -> {})", old.bar.size, new.bar.size);
        return true;
    }

    if old.bar.outer_margin != new.bar.outer_margin {
        debug!(
            "bar.outer_margin changed ({} -> {})",
            old.bar.outer_margin, new.bar.outer_margin
        );
        return true;
    }

    if old.bar.widget_spacing != new.bar.widget_spacing {
        debug!(
            "bar.widget_spacing changed ({} -> {})",
            old.bar.widget_spacing, new.bar.widget_spacing
        );
        return true;
    }

    if old.bar.section_edge_margin != new.bar.section_edge_margin {
        debug!(
            "bar.section_edge_margin changed ({} -> {})",
            old.bar.section_edge_margin, new.bar.section_edge_margin
        );
        return true;
    }

    if old.bar.notch_enabled != new.bar.notch_enabled {
        debug!("bar.notch_enabled changed");
        return true;
    }

    if old.bar.notch_width != new.bar.notch_width {
        debug!(
            "bar.notch_width changed ({} -> {})",
            old.bar.notch_width, new.bar.notch_width
        );
        return true;
    }

    // Widget list changes
    let old_widgets = widget_names(old);
    let new_widgets = widget_names(new);
    if old_widgets != new_widgets {
        debug!("Widget configuration changed");
        debug!("Old widgets: {:?}", old_widgets);
        debug!("New widgets: {:?}", new_widgets);
        return true;
    }

    // Workspace backend changes
    if old.workspace.backend != new.workspace.backend {
        debug!(
            "workspace.backend changed ({} -> {})",
            old.workspace.backend, new.workspace.backend
        );
        return true;
    }

    false
}

/// Get a summary of widget names and options for comparison.
fn widget_names(config: &Config) -> Vec<String> {
    use vibepanel_core::config::WidgetPlacement;

    let mut names = Vec::new();

    fn format_item(prefix: &str, item: &WidgetPlacement) -> Vec<String> {
        match item {
            WidgetPlacement::Single(name) => {
                vec![format!("{}:{}", prefix, name)]
            }
            WidgetPlacement::Group { group } => {
                vec![format!("{}:group:[{}]", prefix, group.join(", "))]
            }
        }
    }

    for w in &config.widgets.left {
        names.extend(format_item("left", w));
    }
    for w in &config.widgets.center {
        names.extend(format_item("center", w));
    }
    for w in &config.widgets.right {
        names.extend(format_item("right", w));
    }

    // Also include per-widget configs for comparison
    for (name, opts) in &config.widgets.widget_configs {
        names.push(format!(
            "config:{}:disabled={},{:?}",
            name, opts.disabled, opts.options
        ));
    }

    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_theme_changed_mode() {
        let old = Config::default();
        let mut new = Config::default();

        assert!(!config_theme_changed(&old, &new));

        new.theme.mode = "light".to_string();
        assert!(config_theme_changed(&old, &new));
    }

    #[test]
    fn test_config_theme_changed_accent() {
        let old = Config::default();
        let mut new = Config::default();

        new.theme.accent = "#ff0000".to_string();
        assert!(config_theme_changed(&old, &new));
    }

    #[test]
    fn test_config_theme_changed_bar_opacity() {
        let old = Config::default();
        let mut new = Config::default();

        new.theme.bar_opacity = 0.5;
        assert!(config_theme_changed(&old, &new));
    }

    #[test]
    fn test_widget_names() {
        use vibepanel_core::config::WidgetPlacement;

        let mut config = Config::default();
        config
            .widgets
            .left
            .push(WidgetPlacement::Single("workspace".to_string()));
        config
            .widgets
            .right
            .push(WidgetPlacement::Single("clock".to_string()));

        let names = widget_names(&config);
        assert!(names.iter().any(|n| n == "left:workspace"));
        assert!(names.iter().any(|n| n == "right:clock"));
    }
}
