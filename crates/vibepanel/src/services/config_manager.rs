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
//! - `icons.*`: Switches icon backend (Material ↔ GTK themes) and weight
//! - `theme.*`: Updates colors, palette, CSS variables
//! - Structural changes (widget list, layout, bar size, margins) trigger a full
//!   bar rebuild with a brief visual flicker.

use std::cell::{Cell, RefCell};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use gtk4::{gio, glib, prelude::*};
use notify_debouncer_mini::{DebounceEventResult, new_debouncer, notify::RecursiveMode};
use parking_lot::RwLock;
use std::collections::{HashSet, VecDeque};
use tracing::{debug, error, info, warn};

use vibepanel_core::{
    Config, ThemePalette, ThemeSizes,
    config::{BarPosition, SchemePolarity},
};

use super::callbacks::{CallbackId, Callbacks};
use super::wallpaper::{detect_wallpaper, extract_theme_from_image, theme_from_source_color};

/// Debounce interval (in ms) for file change events. Editors often trigger
/// multiple events for a single save; this batches them into one reload.
const FILE_CHANGE_DEBOUNCE_MS: u64 = 300;

/// Polling interval (in seconds) for checking if the wallpaper changed.
/// Only active when `mode = "auto"` and no explicit wallpaper path is set.
const WALLPAPER_POLL_INTERVAL_SECS: u32 = 2;
const GTK_INTERFACE_SCHEMA: &str = "org.gnome.desktop.interface";
const GTK_COLOR_SCHEME_KEY: &str = "color-scheme";
const CLOCK_WIDGET_NAME: &str = "clock";
const WEATHER_WIDGET_NAME: &str = "weather";
// Bounds both depth and breadth when symlinked directories evade the visited set.
const CSS_IMPORT_SCAN_LIMIT: usize = 256;

use crate::bar;
use crate::services::audio::AudioService;
use crate::services::bar_manager::BarManager;
use crate::services::icons::IconsService;
use crate::services::network::NetworkService;
use crate::services::surfaces::SurfaceStyleManager;
use crate::services::weather::{ResolvedWeatherConfig, WeatherService};

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

/// Make a path absolute by joining it with the current working directory if needed.
fn make_absolute(path: &std::path::Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

/// Send a config message to the main thread via glib::idle_add_once.
fn send_config_message(msg: ConfigMessage) {
    glib::idle_add_once(move || {
        ConfigManager::global().handle_config_message(msg);
    });
}

/// Normalize `.` and `..` components lexically to match GTK's import resolution.
fn normalize_path(path: &Path) -> PathBuf {
    let absolute = make_absolute(path);
    let mut normalized = PathBuf::new();

    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::Normal(part) => normalized.push(part),
        }
    }

    normalized
}

/// Remove `/* ... */` comments. CSS comments don't nest.
/// Comment markers inside string literals confuse this (e.g. `content: "/*"`);
/// worst case is a wrong or missing hot-reload watch — GTK stays the
/// authoritative parser.
fn strip_css_comments(css: &str) -> String {
    let mut stripped = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        stripped.push_str(&rest[..start]);
        rest = match rest[start + 2..].find("*/") {
            Some(end) => &rest[start + 2 + end + 2..],
            None => "",
        };
    }
    stripped.push_str(rest);
    stripped
}

fn strip_prefix_ci<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    value
        .get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &value[prefix.len()..])
}

/// Extract quoted, `url()`, and unquoted local `@import` paths.
/// GTK remains the authoritative parser; this scan only picks watch paths.
/// Naive statement scan on comment-stripped CSS — imports inside string
/// literals produce spurious watches, CSS escapes stay unsupported, and paths
/// containing `;` are truncated; worst case is a wrong or missing hot-reload
/// watch, never wrong styling.
fn css_import_values(css: &str) -> Vec<String> {
    let stripped = strip_css_comments(css);
    let lower = stripped.to_ascii_lowercase();
    let mut imports = Vec::new();

    for (index, _) in lower.match_indices("@import") {
        let rest = &stripped[index + "@import".len()..];
        if rest.starts_with(|c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')) {
            continue; // @important, @importurl(...), ...
        }
        let line = rest.split(';').next().unwrap_or("").trim();
        let line = strip_prefix_ci(line, "url(")
            .map(str::trim_start)
            .unwrap_or(line);
        let value = match line.chars().next() {
            Some(quote @ ('"' | '\'')) => line[1..].split(quote).next().unwrap_or(""),
            _ => line.split([')', ';']).next().unwrap_or(""),
        };
        let value = value.trim();
        if !value.is_empty() && !value.contains('\\') && value.len() <= 512 {
            imports.push(value.to_string());
        }
    }

    imports
}

fn local_css_import_path(value: &str) -> Option<PathBuf> {
    let value = value.trim();
    if value.is_empty() || value.starts_with('#') || value.starts_with("//") {
        return None;
    }

    if let Some(colon) = value.find(':') {
        let scheme = &value[..colon];
        if !scheme.is_empty()
            && scheme.bytes().enumerate().all(|(index, byte)| {
                byte.is_ascii_alphabetic()
                    || (index > 0 && (byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.')))
            })
        {
            return None;
        }
    }

    Some(PathBuf::from(value))
}

fn discover_css_dependencies(root: &Path) -> HashSet<PathBuf> {
    let root = normalize_path(root);
    let mut dependencies = HashSet::new();
    let mut pending = VecDeque::from([root]);

    while let Some(logical_path) = pending.pop_front() {
        if !dependencies.insert(logical_path.clone()) {
            continue;
        }
        if dependencies.len() >= CSS_IMPORT_SCAN_LIMIT {
            warn!("CSS import scan reached dependency limit");
            break;
        }

        let Ok(css) = std::fs::read_to_string(&logical_path) else {
            continue;
        };
        let importer_dir = logical_path.parent().unwrap_or(Path::new("/"));

        for import in css_import_values(&css) {
            let Some(import_path) = local_css_import_path(&import) else {
                continue;
            };
            let resolved = if import_path.is_absolute() {
                normalize_path(&import_path)
            } else {
                normalize_path(&importer_dir.join(import_path))
            };
            pending.push_back(resolved);
        }
    }

    dependencies
}

fn style_watch_dirs(watched_paths: &HashSet<PathBuf>, config_watch_dir: &Path) -> HashSet<PathBuf> {
    watched_paths
        .iter()
        .filter_map(|path| path.parent().map(Path::to_path_buf))
        .filter(|dir| dir != config_watch_dir)
        .collect()
}

fn arm_style_dirs(
    watcher: &mut (impl notify_debouncer_mini::notify::Watcher + ?Sized),
    dirs: &HashSet<PathBuf>,
    failed_dirs: &mut HashSet<PathBuf>,
) -> HashSet<PathBuf> {
    failed_dirs.retain(|dir| dirs.contains(dir));
    let mut armed = HashSet::new();
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }

        match watcher.watch(dir, RecursiveMode::NonRecursive) {
            Ok(()) => {
                armed.insert(dir.clone());
                failed_dirs.remove(dir);
            }
            Err(error) => {
                if failed_dirs.insert(dir.clone()) {
                    warn!("Failed to watch CSS directory {}: {}", dir.display(), error);
                }
            }
        }
    }
    armed
}

/// Return true for a stylesheet or its parent directory being replaced or removed.
fn is_style_change_path(path: &Path, watched_paths: &HashSet<PathBuf>) -> bool {
    let normalized = normalize_path(path);
    watched_paths.contains(&normalized)
        || watched_paths
            .iter()
            .any(|watched| watched.parent() == Some(normalized.as_path()))
}

fn config_uses_gtk_scheme(config: &Config) -> bool {
    config.theme.mode == "auto" && config.theme.scheme == Some(SchemePolarity::Gtk)
}

fn scheme_from_gtk_color_scheme_value(value: &str) -> Option<SchemePolarity> {
    match value {
        "prefer-dark" => Some(SchemePolarity::Dark),
        "prefer-light" => Some(SchemePolarity::Light),
        "default" => None,
        other => {
            debug!(
                "Unknown GTK color-scheme preference '{}', falling back to wallpaper luminance",
                other
            );
            None
        }
    }
}

fn gtk_color_scheme_settings() -> Option<gio::Settings> {
    let schema_source = gio::SettingsSchemaSource::default()?;
    let schema = schema_source.lookup(GTK_INTERFACE_SCHEMA, true)?;
    if !schema.has_key(GTK_COLOR_SCHEME_KEY) {
        return None;
    }
    Some(gio::Settings::new(GTK_INTERFACE_SCHEMA))
}

fn gtk_scheme_preference() -> Option<SchemePolarity> {
    let settings = gtk_color_scheme_settings()?;
    scheme_from_gtk_color_scheme_value(settings.string(GTK_COLOR_SCHEME_KEY).as_str())
}

fn weather_config_from_config(config: &Config) -> ResolvedWeatherConfig {
    let weather = &config.weather;
    let referenced_widgets = config.widgets.all_referenced_widgets();
    let widget_enabled = |name: &str| {
        referenced_widgets.iter().any(|referenced| {
            let base_name = widget_base_name(referenced);
            base_name == name && !config.widgets.is_disabled(base_name)
        })
    };
    let clock_weather_enabled = widget_enabled(CLOCK_WIDGET_NAME)
        && config
            .widgets
            .get_options(CLOCK_WIDGET_NAME)
            .and_then(|opts| opts.options.get("show_weather"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
    let consumer_enabled = widget_enabled(WEATHER_WIDGET_NAME) || clock_weather_enabled;

    ResolvedWeatherConfig {
        enabled: consumer_enabled,
        auto_locate: weather.auto_locate,
        latitude: weather.latitude,
        longitude: weather.longitude,
        location: weather.location.clone(),
        units: weather.units,
        wind_units: weather.wind_units,
        refresh_interval: weather.refresh_interval,
    }
}

fn widget_base_name(name: &str) -> &str {
    name.split_once(':').map(|(base, _)| base).unwrap_or(name)
}

fn resolve_gtk_scheme_config(config: &Config) -> Config {
    let mut resolved = config.clone();
    if config_uses_gtk_scheme(&resolved) {
        resolved.theme.scheme = gtk_scheme_preference();
    }
    resolved
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
    /// Cached theme palette — computed once per config change, not on every access.
    /// This avoids re-reading and re-processing the wallpaper image on every call
    /// to `theme_sizes()`, `surface_border_radius()`, etc.
    palette: RefCell<ThemePalette>,
    /// Cached popover palette — a second palette with flipped polarity for popover
    /// surfaces, computed when `theme.popover` is set. `None` when not configured,
    /// when the polarity matches the bar, or when mode is "gtk".
    popover_palette: RefCell<Option<ThemePalette>>,
    /// Path to the config file being watched (if any).
    config_path: RefCell<Option<PathBuf>>,
    /// Shutdown flag for the file watcher thread.
    shutdown_flag: Arc<AtomicBool>,
    /// Callbacks for theme/style changes (border radius, colors, etc.)
    /// that don't trigger a full bar rebuild.
    theme_callbacks: Callbacks<()>,
    /// Last wallpaper path detected from wallpaper daemon (for change detection).
    wallpaper_path: RefCell<Option<String>>,
    /// Cached source color extracted from the wallpaper image. Rebuilding a
    /// `material_colors::theme::Theme` from the source color is cheap (pure math);
    /// the expensive part is image I/O + quantization, which this cache avoids.
    cached_source_color: Cell<Option<material_colors::color::Argb>>,
    /// Average relative luminance of the last extracted wallpaper image (0.0–1.0).
    /// Used to auto-derive light/dark polarity when `theme.scheme` is not set.
    cached_luminance: Cell<Option<f64>>,
    /// Source ID for the wallpaper polling timer (so we can cancel it).
    wallpaper_poll_source: RefCell<Option<glib::SourceId>>,
    /// Guard against overlapping wallpaper polls (IPC + image processing is async).
    poll_in_progress: Cell<bool>,
    /// GSettings watcher kept alive for live `theme.scheme = "gtk"` updates.
    gtk_color_scheme_watch: RefCell<Option<(gio::Settings, glib::SignalHandlerId)>>,
}

// Thread-local singleton storage
thread_local! {
    static CONFIG_MANAGER_INSTANCE: RefCell<Option<Rc<ConfigManager>>> = const { RefCell::new(None) };
}

impl ConfigManager {
    fn new(config: Config, config_path: Option<PathBuf>) -> Rc<Self> {
        // Detect wallpaper and extract Material You theme if in auto mode
        let monitor_hint = config.bar.outputs.first().map(|s| s.as_str());
        let (initial_wallpaper, material_theme, initial_luminance) =
            if config.theme.mode == "auto" && config.theme.wallpaper.is_none() {
                let wp = detect_wallpaper(monitor_hint);
                let result = wp.as_deref().and_then(extract_theme_from_image);
                let luminance = result.as_ref().map(|(_, l)| *l);
                let theme = result.and_then(|(t, _)| t);
                (wp, theme, luminance)
            } else if config.theme.mode == "auto" {
                // Explicit wallpaper path set
                let result = config
                    .theme
                    .wallpaper
                    .as_deref()
                    .and_then(extract_theme_from_image);
                let luminance = result.as_ref().map(|(_, l)| *l);
                let theme = result.and_then(|(t, _)| t);
                (None, theme, luminance)
            } else {
                (None, None, None)
            };

        let source_color = material_theme.as_ref().map(|t| t.source);
        let resolved_config = resolve_gtk_scheme_config(&config);
        let palette =
            ThemePalette::from_config(&resolved_config, material_theme.as_ref(), initial_luminance);
        let popover_palette = ThemePalette::popover_palette(
            &resolved_config,
            material_theme.as_ref(),
            initial_luminance,
        );

        Rc::new(Self {
            config: RefCell::new(config),
            palette: RefCell::new(palette),
            popover_palette: RefCell::new(popover_palette),
            config_path: RefCell::new(config_path),
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            theme_callbacks: Callbacks::new(),
            wallpaper_path: RefCell::new(initial_wallpaper),
            cached_source_color: Cell::new(source_color),
            cached_luminance: Cell::new(initial_luminance),
            wallpaper_poll_source: RefCell::new(None),
            poll_in_progress: Cell::new(false),
            gtk_color_scheme_watch: RefCell::new(None),
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

    #[cfg(test)]
    pub(crate) fn replace_global_for_test(config: Config) {
        Self::replace_global_with_config_path_for_test(config, None);
    }

    #[cfg(test)]
    pub(crate) fn replace_global_with_config_path_for_test(
        config: Config,
        config_path: Option<PathBuf>,
    ) {
        CONFIG_MANAGER_INSTANCE.with(|cell| {
            *cell.borrow_mut() = Some(ConfigManager::new(config.clone(), config_path));
            WeatherService::global().configure(weather_config_from_config(&config));
        });
    }

    /// Apply the current weather configuration to the shared weather service.
    pub fn apply_weather_config(&self) {
        WeatherService::global().configure(weather_config_from_config(&self.config.borrow()));
    }

    /// Get the computed theme sizes from the current configuration.
    ///
    /// This returns sizes from the cached palette — no recomputation needed.
    pub fn theme_sizes(&self) -> ThemeSizes {
        self.palette.borrow().sizes.clone()
    }

    /// Get the cached theme palette.
    ///
    /// The palette is computed once per config change and cached. This avoids
    /// re-reading and re-processing the wallpaper image on every access.
    pub fn palette(&self) -> ThemePalette {
        self.palette.borrow().clone()
    }

    /// Get the cached popover palette, if any.
    ///
    /// Returns `Some` when `theme.popover` is configured and the polarity
    /// differs from the bar. Returns `None` otherwise.
    pub fn popover_palette(&self) -> Option<ThemePalette> {
        self.popover_palette.borrow().clone()
    }

    /// Get the computed surface border radius in pixels.
    pub fn surface_border_radius(&self) -> u32 {
        self.palette.borrow().surface_border_radius
    }

    /// Get the computed bar border radius in pixels.
    pub fn bar_border_radius(&self) -> u32 {
        self.palette.borrow().bar_border_radius
    }

    /// Get the computed widget border radius in pixels.
    ///
    /// This is the radius applied to individual widget islands (`.widget` elements).
    /// Use this for blur regions on widget islands — not `bar_border_radius`, which
    /// includes bar padding and applies to the whole bar surface.
    pub fn widget_border_radius(&self) -> u32 {
        self.palette.borrow().widget_border_radius
    }

    /// Whether the bar outline is effectively visible.
    pub fn bar_outline_visible(&self) -> bool {
        let palette = self.palette.borrow();
        palette.bar_outline_enabled
            && palette.outline_width_px > 0
            && palette.outline_opacity_pct > 0
    }

    /// Whether widget island outlines are effectively visible.
    pub fn widget_outline_visible(&self) -> bool {
        let palette = self.palette.borrow();
        palette.widget_outline_enabled
            && palette.outline_width_px > 0
            && palette.outline_opacity_pct > 0
    }

    /// Whether floating surface outlines are effectively visible.
    pub fn surface_outline_visible(&self) -> bool {
        self.surface_outline_width() > 0.0
    }

    /// Effective floating-surface outline width in logical pixels.
    /// Width is palette-independent; only outline color follows the popover palette.
    pub fn surface_outline_width(&self) -> f32 {
        let palette = self.palette.borrow();
        if palette.surface_outline_enabled
            && palette.outline_width_px > 0
            && palette.outline_opacity_pct > 0
        {
            palette.outline_width_px as f32
        } else {
            0.0
        }
    }

    /// Get the pill radius (used for rounded indicators, thumbnails, etc.).
    ///
    /// This is derived from the widget border radius configuration.
    /// Used by CSS variable generation in ThemePalette.
    #[allow(dead_code)]
    pub fn radius_pill(&self) -> u32 {
        self.palette.borrow().radius_pill
    }

    /// Get the raw widget border radius percentage (0-100) from config.
    ///
    /// This is the raw config value, useful for scaling other elements proportionally.
    /// At 0% = square, at 100% = maximum rounding (fully round for square elements).
    pub fn widget_radius_percent(&self) -> u32 {
        self.config.borrow().widgets.border_radius
    }

    pub fn bar_size(&self) -> u32 {
        self.config.borrow().bar.size
    }

    pub fn bar_padding(&self) -> u32 {
        self.config.borrow().bar.padding
    }

    pub fn screen_margin(&self) -> u32 {
        self.config.borrow().bar.screen_margin
    }

    pub fn popover_offset(&self) -> u32 {
        self.config.borrow().bar.popover_offset
    }

    pub fn bar_background_opacity(&self) -> f64 {
        self.config.borrow().bar.background_opacity
    }

    pub fn bar_position(&self) -> BarPosition {
        self.config.borrow().bar.position()
    }

    /// Whether UI animations are enabled (CSS transitions, revealer
    /// animations, workspace indicator transitions).
    pub fn animations_enabled(&self) -> bool {
        self.config.borrow().theme.animations
    }

    /// Return `default_ms` when animations are enabled, or `0` when disabled.
    ///
    /// Use this to set transition durations on GTK widgets (e.g. `Revealer`)
    /// so a single call replaces the recurring if/else pattern.
    pub fn animation_duration(&self, default_ms: u32) -> u32 {
        if self.animations_enabled() {
            default_ms
        } else {
            0
        }
    }

    /// Check if the ripple effect is enabled.
    ///
    /// When false, the Material Design-style ripple on button/widget press
    /// is suppressed.
    pub fn ripple_enabled(&self) -> bool {
        self.config.borrow().theme.ripple
    }

    /// Get the parent directory of the active config file, if any.
    ///
    /// Used by the unified CSS resolver to search for `style.css` next to the
    /// config file before falling back to XDG/HOME/CWD.
    pub(crate) fn config_dir(&self) -> Option<PathBuf> {
        self.config_path
            .borrow()
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    }

    /// Return a clone of the currently active configuration.
    pub fn config_snapshot(&self) -> Config {
        self.config.borrow().clone()
    }

    /// Check if compositor background blur is enabled.
    ///
    /// When true, vibepanel sends ext-background-effect-v1 blur region hints
    /// for the bar, popovers, quick settings, notification toasts, OSD,
    /// tray menus, and media pop-out windows.
    pub fn blur_enabled(&self) -> bool {
        self.config.borrow().theme.blur
    }

    /// Get a widget option value from the current configuration.
    ///
    /// Returns `None` if the widget has no config section or the option doesn't exist.
    pub fn get_widget_option(&self, widget_name: &str, option_name: &str) -> Option<toml::Value> {
        self.config
            .borrow()
            .widgets
            .get_options(widget_name)
            .and_then(|opts| opts.options.get(option_name).cloned())
    }

    /// Get click handler commands for a widget.
    ///
    /// Returns `(on_click_right, on_click_middle)` from `[widgets.<name>]`.
    pub fn get_click_handlers(&self, widget_name: &str) -> (Option<String>, Option<String>) {
        let config = self.config.borrow();
        config
            .widgets
            .get_options(widget_name)
            .map(|opts| (opts.on_click_right.clone(), opts.on_click_middle.clone()))
            .unwrap_or((None, None))
    }

    /// Get `show_if` command and interval for a widget.
    ///
    /// Returns `(show_if_command, show_if_interval)` from `[widgets.<name>]`.
    /// An interval of `0` is normalized to `None` (treated as no interval).
    pub fn get_show_if(&self, widget_name: &str) -> (Option<String>, Option<u64>) {
        let config = self.config.borrow();
        config
            .widgets
            .get_options(widget_name)
            .map(|opts| {
                let interval = opts.show_if_interval.filter(|&i| i > 0);
                (opts.show_if.clone(), interval)
            })
            .unwrap_or((None, None))
    }

    /// Register a callback to be called when theme/style configuration changes.
    ///
    /// This is called for changes like border radius, colors, opacity etc. that
    /// don't trigger a full bar rebuild but may require widgets to update
    /// programmatic styling (e.g., RoundedPicture corner radius).
    ///
    /// Returns a `CallbackId` that can be used to unregister the callback.
    pub fn on_theme_change<F>(&self, callback: F) -> CallbackId
    where
        F: Fn() + 'static,
    {
        self.theme_callbacks.register(move |_: &()| callback())
    }

    pub fn disconnect_theme_callback(&self, id: CallbackId) -> bool {
        self.theme_callbacks.unregister(id)
    }

    /// Start watching the config file for changes and wallpaper polling.
    ///
    /// This spawns a background thread that monitors the config file. When changes
    /// are detected, the new config is parsed and sent to the GTK main thread.
    ///
    /// Also starts wallpaper polling if `mode = "auto"` (wallpaper-adaptive theming).
    pub fn start_watching(self: &Rc<Self>) {
        // Start wallpaper polling if in auto-detect mode
        self.start_wallpaper_polling();
        self.start_gtk_color_scheme_watching();

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
        let config_dir = path.parent().map(|p| p.to_path_buf());
        let shutdown_flag = self.shutdown_flag.clone();

        // Spawn file watcher thread
        thread::spawn(move || {
            Self::run_file_watcher(watch_path, config_dir, shutdown_flag);
        });
    }

    fn start_gtk_color_scheme_watching(self: &Rc<Self>) {
        if self.gtk_color_scheme_watch.borrow().is_some() {
            return;
        }

        let Some(settings) = gtk_color_scheme_settings() else {
            debug!(
                "GTK color-scheme preference unavailable; theme.scheme=gtk will use wallpaper luminance"
            );
            return;
        };

        let weak = Rc::downgrade(self);
        let handler_id = settings.connect_changed(Some(GTK_COLOR_SCHEME_KEY), move |_, _| {
            let Some(mgr) = weak.upgrade() else {
                return;
            };
            mgr.handle_gtk_color_scheme_changed();
        });

        *self.gtk_color_scheme_watch.borrow_mut() = Some((settings, handler_id));
        debug!("Watching GTK color-scheme preference for theme.scheme=gtk");
    }

    /// Compute exact root/import paths to watch.
    ///
    /// `search_paths` and `style_css_logical` are passed in so the function
    /// can be unit-tested without touching global env vars.
    fn compute_style_watch_paths(
        search_paths: Vec<PathBuf>,
        style_css_logical: Option<PathBuf>,
    ) -> HashSet<PathBuf> {
        let mut watched_paths = HashSet::new();
        for search_path in search_paths {
            let logical = normalize_path(&search_path);
            watched_paths.insert(logical);
        }

        if let Some(root) = style_css_logical {
            for logical in discover_css_dependencies(&root) {
                watched_paths.insert(logical.clone());

                if let Ok(canonical) = logical.canonicalize() {
                    if canonical != logical {
                        debug!(
                            "Watching CSS symlink target: {} -> {}",
                            logical.display(),
                            canonical.display()
                        );
                    }
                    watched_paths.insert(canonical);
                }
            }
        }

        watched_paths
    }

    /// Run the file watcher loop (called on a background thread).
    ///
    /// Watches `config.toml`, user `style.css`, and local CSS imports. The CSS
    /// dependency graph is refreshed after stylesheet changes. Logical and
    /// canonical symlink paths are both tracked.
    fn run_file_watcher(
        config_path: PathBuf,
        config_dir: Option<PathBuf>,
        shutdown_flag: Arc<AtomicBool>,
    ) {
        // Debounce events to avoid multiple reloads for a single save
        let debounce_duration = Duration::from_millis(FILE_CHANGE_DEBOUNCE_MS);

        // Canonicalize the config path so we can compare with absolute paths from notify
        let config_canonical = match config_path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to canonicalize config path: {}", e);
                return;
            }
        };

        // Compute the config watch directory before the closure captures config_canonical.
        let config_watch_dir = config_canonical
            .parent()
            .unwrap_or(&config_canonical)
            .to_path_buf();

        let style_watch_paths = Self::compute_style_watch_paths(
            crate::bar::user_css_search_paths(config_dir.as_deref()),
            crate::bar::find_user_css(config_dir.as_deref()),
        );
        let mut desired_style_dirs = style_watch_dirs(&style_watch_paths, &config_watch_dir);

        debug!(
            "Style CSS watch directories: {:?}",
            desired_style_dirs
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
        );
        debug!(
            "Watched CSS files: {:?}",
            style_watch_paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
        );

        let watched_style_paths = Arc::new(RwLock::new(style_watch_paths));
        let needs_style_rescan = Arc::new(AtomicBool::new(false));

        let callback_watched_style_paths = watched_style_paths.clone();
        let callback_needs_style_rescan = needs_style_rescan.clone();

        let mut debouncer =
            match new_debouncer(debounce_duration, move |res: DebounceEventResult| {
                match res {
                    Ok(events) => {
                        // Check if any event is for our config file
                        let config_changed = events.iter().any(|e| e.path == config_canonical);
                        if config_changed {
                            debug!("Config file change detected");
                            Self::reload_and_send(&config_canonical);
                        }

                        let watched_paths = callback_watched_style_paths.read();
                        let style_changed = events
                            .iter()
                            .any(|e| is_style_change_path(&e.path, &watched_paths));
                        drop(watched_paths);
                        if style_changed {
                            debug!("User style.css change detected");
                            callback_needs_style_rescan.store(true, Ordering::Relaxed);
                            send_config_message(ConfigMessage::StyleCssChanged);
                        }
                    }
                    Err(err) => {
                        error!("File watcher error: {}", err);
                        callback_needs_style_rescan.store(true, Ordering::Relaxed);
                        send_config_message(ConfigMessage::StyleCssChanged);
                    }
                }
            }) {
                Ok(d) => d,
                Err(e) => {
                    error!("Failed to create file watcher: {}", e);
                    return;
                }
            };

        // Watch the config file's parent directory (more reliable than watching file directly).
        if let Err(e) = debouncer
            .watcher()
            .watch(&config_watch_dir, RecursiveMode::NonRecursive)
        {
            error!("Failed to watch config directory: {}", e);
            return;
        }
        info!(
            "File watcher started, watching: {}",
            config_watch_dir.display()
        );

        let mut failed_style_dirs = HashSet::new();
        let mut armed_style_dirs = arm_style_dirs(
            debouncer.watcher(),
            &desired_style_dirs,
            &mut failed_style_dirs,
        );

        // Keep the thread alive until shutdown is signaled
        // Use shorter sleep intervals to allow responsive shutdown
        while !shutdown_flag.load(Ordering::Relaxed) {
            let mut reload_style = false;
            let force_rearm = needs_style_rescan.swap(false, Ordering::Relaxed);
            if force_rearm {
                let refreshed = Self::compute_style_watch_paths(
                    crate::bar::user_css_search_paths(config_dir.as_deref()),
                    crate::bar::find_user_css(config_dir.as_deref()),
                );
                let mut watched = watched_style_paths.write();
                // A dependency discovered after the reload may have changed while untracked.
                reload_style = !refreshed.is_subset(&watched);
                let previous = std::mem::replace(&mut *watched, refreshed);
                // Retain vanished dependencies for delete/recreate recovery, with a bound for
                // paths that never reappear.
                watched.extend(
                    previous
                        .into_iter()
                        .filter(|path| !path.exists())
                        .take(CSS_IMPORT_SCAN_LIMIT),
                );
                desired_style_dirs = style_watch_dirs(&watched, &config_watch_dir);
            }

            for stale in armed_style_dirs.difference(&desired_style_dirs) {
                let _ = debouncer.watcher().unwatch(stale);
            }

            let dirs_to_arm = if force_rearm {
                desired_style_dirs.clone()
            } else {
                desired_style_dirs
                    .difference(&armed_style_dirs)
                    .cloned()
                    .collect()
            };
            let armed_now =
                arm_style_dirs(debouncer.watcher(), &dirs_to_arm, &mut failed_style_dirs);
            if !armed_now.is_subset(&armed_style_dirs) {
                needs_style_rescan.store(true, Ordering::Relaxed);
                reload_style = true;
            }
            if force_rearm {
                armed_style_dirs = armed_now;
            } else {
                armed_style_dirs.extend(armed_now);
            }

            if reload_style {
                // A newly armed directory may already contain a change emitted while unwatched.
                debug!("CSS dependencies changed; reloading after updating watches");
                send_config_message(ConfigMessage::StyleCssChanged);
            }
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
    pub(crate) fn handle_config_message(self: &Rc<Self>, msg: ConfigMessage) {
        match msg {
            ConfigMessage::Reloaded(new_config) => {
                AudioService::global().set_allow_overdrive(new_config.audio.allow_overdrive);
                self.apply_config(*new_config);
                self.apply_weather_config();
            }
            ConfigMessage::Error(err) => {
                // Just log the error - keep using the old config
                error!("Config reload error: {}", err);
            }
            ConfigMessage::StyleCssChanged => {
                // Reload user CSS
                info!("Reloading user style.css...");
                crate::bar::replace_user_css();
            }
        }
    }

    /// Apply a new configuration, updating all subsystems.
    ///
    /// This is the central "fan-out" function that coordinates updates across
    /// all services and widgets when the config changes.
    fn apply_config(self: &Rc<Self>, new_config: Config) {
        let old_config = self.config.borrow().clone();

        info!("Applying new configuration...");

        // Update icons theme and/or weight
        if old_config.theme.icons.theme != new_config.theme.icons.theme
            || old_config.theme.icons.weight != new_config.theme.icons.weight
        {
            info!(
                "Icon config changed: theme {} -> {}, weight {} -> {}",
                old_config.theme.icons.theme,
                new_config.theme.icons.theme,
                old_config.theme.icons.weight,
                new_config.theme.icons.weight
            );
            IconsService::global()
                .reconfigure(&new_config.theme.icons.theme, new_config.theme.icons.weight);

            // Icon theme changes affect Material unified mode logic in network
            // callbacks (e.g., showing cell_wifi vs separate icons). Re-emit
            // the current network snapshot so those callbacks re-evaluate.
            NetworkService::global().re_notify();
        }

        // Determine what changed
        let theme_changed = config_theme_changed(&old_config, &new_config);
        let structure_changed = config_structure_changed(&old_config, &new_config);

        // Update detected wallpaper path before theme rebuild so the palette
        // can use it (e.g. when an explicit wallpaper is removed and we need
        // to fall back to auto-detection).
        if new_config.theme.mode == "auto"
            && new_config.theme.wallpaper.is_none()
            && (old_config.theme.mode != "auto"
                || old_config.theme.wallpaper != new_config.theme.wallpaper)
        {
            *self.wallpaper_path.borrow_mut() =
                detect_wallpaper(new_config.bar.outputs.first().map(|s| s.as_str()));
        }

        // Update theme/palette if theme config changed
        if theme_changed {
            info!("Theme configuration changed, updating styles...");

            // Reuse cached source color unless the wallpaper source changed,
            // avoiding redundant image I/O + quantization on the main thread.
            // Rebuilding Theme from source color is cheap (pure math).
            let material_theme = if new_config.theme.mode == "auto" {
                let wallpaper_source_changed = old_config.theme.mode != "auto"
                    || old_config.theme.wallpaper != new_config.theme.wallpaper;

                if wallpaper_source_changed {
                    let result = new_config
                        .theme
                        .wallpaper
                        .as_deref()
                        .or(self.wallpaper_path.borrow().as_deref())
                        .and_then(extract_theme_from_image);
                    let luminance = result.as_ref().map(|(_, l)| *l);
                    let theme = result.and_then(|(t, _)| t);
                    self.cached_source_color
                        .set(theme.as_ref().map(|t| t.source));
                    self.cached_luminance.set(luminance);
                    theme
                } else {
                    self.cached_source_color.get().map(theme_from_source_color)
                }
            } else {
                None
            };

            self.rebuild_theme_from_material(
                &new_config,
                material_theme.as_ref(),
                self.cached_luminance.get(),
            );

            debug!("Theme styles updated");
        }

        // Store the new config AFTER theme/CSS update but BEFORE widget rebuild,
        // so widgets see the new values when notified
        *self.config.borrow_mut() = new_config.clone();

        if old_config.battery_alert_config() != new_config.battery_alert_config() {
            crate::services::battery_alert::BatteryAlertController::global()
                .configure(new_config.battery_alert_config());
        }

        // Restart or stop wallpaper polling if auto mode or wallpaper config changed
        if old_config.theme.mode != new_config.theme.mode
            || old_config.theme.wallpaper != new_config.theme.wallpaper
        {
            self.start_wallpaper_polling();
            // Clear cached path when leaving auto mode or setting an explicit wallpaper
            if new_config.theme.mode != "auto" || new_config.theme.wallpaper.is_some() {
                *self.wallpaper_path.borrow_mut() = None;
            }
        }

        if structure_changed {
            // Structural changes require full bar rebuild
            info!("Structural configuration changed, rebuilding bar...");
            if !theme_changed {
                // Reload CSS if we didn't already above
                bar::load_css(&new_config);
            }
            if let Some(display) = gtk4::gdk::Display::default() {
                BarManager::global().reconfigure_all(&display, &new_config);
            }
        }

        if theme_changed {
            // Notify theme callbacks even during structural rebuild, because
            // non-bar surfaces (e.g. media pop-out) persist across bar rebuilds
            // and need to react to theme changes independently.
            self.theme_callbacks.notify(&());
        }

        info!("Configuration applied successfully");
    }

    fn rebuild_theme_from_material(
        &self,
        config: &Config,
        material_theme: Option<&material_colors::theme::Theme>,
        luminance: Option<f64>,
    ) {
        let resolved_config = resolve_gtk_scheme_config(config);
        let palette = ThemePalette::from_config(&resolved_config, material_theme, luminance);
        let popover_palette =
            ThemePalette::popover_palette(&resolved_config, material_theme, luminance);
        let surface_styles = palette.surface_styles();

        SurfaceStyleManager::global()
            .reconfigure(surface_styles.clone(), config.advanced.pango_font_rendering);

        *self.palette.borrow_mut() = palette;
        *self.popover_palette.borrow_mut() = popover_palette;
        bar::load_css(config);
    }

    fn handle_gtk_color_scheme_changed(self: &Rc<Self>) {
        let config = self.config.borrow().clone();
        if !config_uses_gtk_scheme(&config) {
            return;
        }

        info!("GTK color-scheme preference changed, rebuilding auto theme...");
        let material_theme = self.cached_source_color.get().map(theme_from_source_color);
        self.rebuild_theme_from_material(
            &config,
            material_theme.as_ref(),
            self.cached_luminance.get(),
        );
        self.theme_callbacks.notify(&());
    }

    /// Start polling for wallpaper changes from supported daemons.
    ///
    /// Only active when `mode = "auto"` and no explicit `wallpaper` path is set.
    /// Polls every `WALLPAPER_POLL_INTERVAL_SECS` seconds, compares to the cached path, and triggers a theme
    /// rebuild if the wallpaper changed.
    pub fn start_wallpaper_polling(self: &Rc<Self>) {
        // Stop any existing poll timer first
        self.stop_wallpaper_polling();

        // Only poll when in auto mode with no explicit wallpaper path
        let config = self.config.borrow();
        let should_poll = config.theme.mode == "auto" && config.theme.wallpaper.is_none();
        drop(config);
        if !should_poll {
            return;
        }

        info!(
            "Starting wallpaper polling (every {}s)",
            WALLPAPER_POLL_INTERVAL_SECS
        );

        let mgr = Rc::downgrade(self);
        let source_id = glib::timeout_add_seconds_local(WALLPAPER_POLL_INTERVAL_SECS, move || {
            let Some(mgr) = mgr.upgrade() else {
                return glib::ControlFlow::Break;
            };
            mgr.check_wallpaper_changed();
            glib::ControlFlow::Continue
        });
        *self.wallpaper_poll_source.borrow_mut() = Some(source_id);
    }

    /// Stop wallpaper polling if active.
    fn stop_wallpaper_polling(&self) {
        if let Some(source_id) = self.wallpaper_poll_source.borrow_mut().take() {
            source_id.remove();
            debug!("Wallpaper polling stopped");
        }
    }

    /// Check if the wallpaper path changed and rebuild the theme if so.
    ///
    /// The IPC/detection call and image processing run on a background thread to
    /// avoid blocking the GTK main loop. Results are applied via `glib::idle_add_once`.
    fn check_wallpaper_changed(&self) {
        if self.poll_in_progress.get() {
            return;
        }
        self.poll_in_progress.set(true);

        let old_path = self.wallpaper_path.borrow().clone();
        let monitor_hint = self.config.borrow().bar.outputs.first().cloned();

        std::thread::spawn(move || {
            let new_path = detect_wallpaper(monitor_hint.as_deref());

            if new_path == old_path {
                glib::idle_add_once(|| {
                    ConfigManager::global().poll_in_progress.set(false);
                });
                return;
            }

            info!(
                "Wallpaper changed: {:?} -> {:?}, rebuilding theme...",
                old_path, new_path
            );

            // Heavy work: image I/O + quantization on background thread
            let result = new_path.as_deref().and_then(extract_theme_from_image);
            let new_luminance = result.as_ref().map(|(_, l)| *l);
            let material_theme = result.and_then(|(t, _)| t);
            let source_color = material_theme.as_ref().map(|t| t.source);

            // Palette construction uses live config on the main thread
            glib::idle_add_once(move || {
                let mgr = ConfigManager::global();
                mgr.poll_in_progress.set(false);

                // If we're no longer in auto mode, or an explicit wallpaper has been
                // set, skip — a config change already triggered its own theme rebuild.
                // Mirrors the precondition in `start_wallpaper_polling`.
                let config = mgr.config.borrow().clone();
                if config.theme.mode != "auto" || config.theme.wallpaper.is_some() {
                    debug!(
                        "Wallpaper polling no longer applicable (mode/explicit wallpaper changed), skipping result"
                    );
                    return;
                }

                *mgr.wallpaper_path.borrow_mut() = new_path;
                mgr.cached_source_color.set(source_color);
                mgr.cached_luminance.set(new_luminance);

                mgr.rebuild_theme_from_material(&config, material_theme.as_ref(), new_luminance);

                mgr.theme_callbacks.notify(&());
                info!("Wallpaper theme updated");
            });
        });
    }

    /// Stop watching the config file and wallpaper polling.
    pub fn stop_watching(&self) {
        // Signal the watcher thread to shut down
        self.shutdown_flag.store(true, Ordering::Relaxed);
        self.stop_wallpaper_polling();
        if let Some((settings, handler_id)) = self.gtk_color_scheme_watch.borrow_mut().take() {
            settings.disconnect(handler_id);
        }
        debug!("Config watcher stopped");
    }
}

/// Drop guard that disconnects a theme callback when dropped.
///
/// Wrap a `CallbackId` from [`ConfigManager::on_theme_change`] in this guard
/// to ensure the callback is automatically unregistered when the owning
/// widget is destroyed.
pub struct ThemeCallbackGuard(pub CallbackId);

impl Drop for ThemeCallbackGuard {
    fn drop(&mut self) {
        ConfigManager::global().disconnect_theme_callback(self.0);
    }
}

/// Check if per-widget style overrides have changed (triggers CSS-only reload).
///
/// This detects when widget-specific styling options (like `background_color`)
/// are added, removed, or changed in `[widgets.xxx]` sections.
fn per_widget_styles_changed(old: &Config, new: &Config) -> bool {
    old.widgets.widget_configs != new.widgets.widget_configs
}

/// Check if theme-related config has changed.
fn config_theme_changed(old: &Config, new: &Config) -> bool {
    old.theme != new.theme
        // These live outside [theme] but affect CSS variables / palette
        || old.bar.background_color != new.bar.background_color
        || old.bar.background_opacity != new.bar.background_opacity
        || old.bar.border_radius != new.bar.border_radius
        || old.bar.padding != new.bar.padding
        || old.bar.position != new.bar.position
        || old.bar.size != new.bar.size
        || old.bar.outline != new.bar.outline
        || old.widgets.background_color != new.widgets.background_color
        || old.widgets.background_opacity != new.widgets.background_opacity
        || old.widgets.popover_background_opacity != new.widgets.popover_background_opacity
        || old.widgets.border_radius != new.widgets.border_radius
        || old.widgets.outline != new.widgets.outline
        || old.advanced.pango_font_rendering != new.advanced.pango_font_rendering
        // Per-widget style overrides (background_color, etc.)
        || per_widget_styles_changed(old, new)
}

/// Check if structural configuration has changed (requires bar rebuild).
fn config_structure_changed(old: &Config, new: &Config) -> bool {
    if old.bar.size != new.bar.size {
        debug!("bar.size changed ({} -> {})", old.bar.size, new.bar.size);
        return true;
    }

    if old.bar.screen_margin != new.bar.screen_margin {
        debug!(
            "bar.screen_margin changed ({} -> {})",
            old.bar.screen_margin, new.bar.screen_margin
        );
        return true;
    }

    if old.bar.spacing != new.bar.spacing {
        debug!(
            "bar.spacing changed ({} -> {})",
            old.bar.spacing, new.bar.spacing
        );
        return true;
    }

    if old.bar.inset != new.bar.inset {
        debug!("bar.inset changed ({} -> {})", old.bar.inset, new.bar.inset);
        return true;
    }

    if old.bar.background_opacity != new.bar.background_opacity {
        debug!(
            "bar.background_opacity changed ({} -> {})",
            old.bar.background_opacity, new.bar.background_opacity
        );
        return true;
    }

    if old.bar.padding != new.bar.padding {
        debug!(
            "bar.padding changed ({} -> {})",
            old.bar.padding, new.bar.padding
        );
        return true;
    }

    if old.bar.position != new.bar.position {
        debug!(
            "bar.position changed ({} -> {})",
            old.bar.position, new.bar.position
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

    // Compositor changes
    if old.advanced.compositor != new.advanced.compositor {
        debug!(
            "advanced.compositor changed ({} -> {})",
            old.advanced.compositor, new.advanced.compositor
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

    // Also include per-widget configs for comparison. Keep this stable across
    // reloads: HashMap iteration order can differ even when config is identical.
    let mut widget_configs: Vec<_> = config.widgets.widget_configs.iter().collect();
    widget_configs.sort_by_key(|(name, _)| *name);
    for (name, opts) in widget_configs {
        if let Some(fingerprint) = widget_config_fingerprint(name, opts) {
            names.push(fingerprint);
        }
    }

    names
}

fn widget_config_fingerprint(
    name: &str,
    opts: &vibepanel_core::config::WidgetOptions,
) -> Option<String> {
    let mut options = opts.options.clone();
    if name == "battery" {
        options.remove("alerts");
        options.remove("low_threshold");
        options.remove("critical_threshold");

        if !opts.disabled
            && opts.on_click_right.is_none()
            && opts.on_click_middle.is_none()
            && opts.show_if.is_none()
            && opts.show_if_interval.is_none()
            && options.is_empty()
        {
            return None;
        }
    }

    let mut option_items: Vec<_> = options.iter().collect();
    option_items.sort_by_key(|(name, _)| *name);

    Some(format!(
        "config:{}:disabled={},click_r={:?},click_m={:?},show_if={:?},show_if_interval={:?},{:?}",
        name,
        opts.disabled,
        opts.on_click_right,
        opts.on_click_middle,
        opts.show_if,
        opts.show_if_interval,
        option_items
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_regression_test_support::TestDir;
    use std::path::Path;
    use vibepanel_core::config::{WeatherUnits, WidgetOptions, WidgetPlacement};

    #[test]
    fn test_make_absolute_passthrough_for_absolute_path() {
        let absolute = Path::new("/tmp/vibepanel-style.css");
        assert_eq!(make_absolute(absolute), absolute.to_path_buf());
    }

    #[test]
    fn test_weather_config_enabled_when_referenced() {
        let mut config = Config::default();
        config
            .widgets
            .right
            .push(WidgetPlacement::Single("weather".to_string()));

        let weather = weather_config_from_config(&config);

        assert!(weather.enabled);
    }

    #[test]
    fn test_weather_config_enabled_when_referenced_with_inline_arg() {
        let mut config = Config::default();
        config
            .widgets
            .right
            .push(WidgetPlacement::Single("weather:compact".to_string()));

        let weather = weather_config_from_config(&config);

        assert!(weather.enabled);
    }

    #[test]
    fn test_weather_config_disabled_widget_does_not_enable_service() {
        let mut config = Config::default();
        config
            .widgets
            .right
            .push(WidgetPlacement::Single("weather".to_string()));
        config.widgets.widget_configs.insert(
            "weather".to_string(),
            WidgetOptions {
                disabled: true,
                ..WidgetOptions::default()
            },
        );

        let weather = weather_config_from_config(&config);

        assert!(!weather.enabled);
    }

    #[test]
    fn test_weather_config_location_without_consumer_does_not_enable_service() {
        let mut config = Config::default();
        config.weather.location = Some("Berlin".to_string());

        let weather = weather_config_from_config(&config);

        assert!(!weather.enabled);
    }

    #[test]
    fn test_weather_config_enabled_when_clock_embeds_weather() {
        let mut config = Config::default();
        config
            .widgets
            .right
            .push(WidgetPlacement::Single("clock".to_string()));
        config.widgets.widget_configs.insert(
            "clock".to_string(),
            WidgetOptions {
                options: [("show_weather".to_string(), toml::Value::Boolean(true))]
                    .into_iter()
                    .collect(),
                ..WidgetOptions::default()
            },
        );

        let weather = weather_config_from_config(&config);

        assert!(weather.enabled);
    }

    #[test]
    fn test_weather_config_parses_top_level_options() {
        let mut config = Config::default();
        config.weather.auto_locate = true;
        config.weather.latitude = Some(40.7128);
        config.weather.longitude = Some(-74.0060);
        config.weather.location = Some("New York".to_string());
        config.weather.units = WeatherUnits::Imperial;
        config.weather.refresh_interval = 1200;

        let weather = weather_config_from_config(&config);

        assert!(!weather.enabled);
        assert!(weather.auto_locate);
        assert_eq!(weather.latitude, Some(40.7128));
        assert_eq!(weather.longitude, Some(-74.0060));
        assert_eq!(weather.location.as_deref(), Some("New York"));
        assert_eq!(weather.units, WeatherUnits::Imperial);
        assert_eq!(weather.refresh_interval, 1200);
    }

    #[test]
    fn test_make_absolute_joins_current_dir_for_relative_path() {
        let relative = Path::new("style.css");
        let expected = std::env::current_dir().unwrap().join(relative);
        assert_eq!(make_absolute(relative), expected);
    }

    #[test]
    fn test_css_import_values_supports_common_forms() {
        let css = r#"
            @import "colors.css";
            @import 'layout.css';
            @import url("widgets.css");
            @import url('popover.css');
            @import url(plain.css);
            @IMPORT /* generated */ URL("upper.css");
            @import
                "multiline.css";
            @import url(
                "multiline-url.css"
            );
        "#;

        assert_eq!(
            css_import_values(css),
            vec![
                "colors.css",
                "layout.css",
                "widgets.css",
                "popover.css",
                "plain.css",
                "upper.css",
                "multiline.css",
                "multiline-url.css",
            ]
        );
    }

    #[test]
    fn test_css_import_values_ignores_comments_and_longer_at_rules() {
        let css = r#"
            /* @import "comment.css"; */
            @important "not-an-import.css";
            @importurl("also-not-an-import.css");
            @import "real.css";
        "#;

        assert_eq!(css_import_values(css), vec!["real.css"]);
        assert!(css_import_values("/* unterminated @import \"fake.css\";").is_empty());
    }

    #[test]
    fn test_css_import_values_rejects_escaped_and_oversized_values() {
        assert!(css_import_values(r#"@import "escaped\"name.css";"#).is_empty());
        let long = format!("@import \"{}\";", "a".repeat(600));
        assert!(css_import_values(&long).is_empty());
    }

    #[test]
    fn test_local_css_import_path_rejects_non_file_urls() {
        assert_eq!(
            local_css_import_path("../theme.css"),
            Some(PathBuf::from("../theme.css"))
        );
        assert!(local_css_import_path("https://example.com/theme.css").is_none());
        assert!(local_css_import_path("resource:///theme.css").is_none());
        assert!(local_css_import_path("data:text/css,body{}").is_none());
    }

    #[test]
    fn test_discover_css_dependencies_resolves_nested_imports_and_cycles() {
        let root_dir = TestDir::new("vibepanel_test_imports");
        let root_dir = root_dir.path();
        let nested_dir = root_dir.join("nested");
        std::fs::create_dir_all(&nested_dir).unwrap();

        let root = root_dir.join("style.css");
        let nested = nested_dir.join("colors.css");
        let base = root_dir.join("base.css");
        let missing = root_dir.join("missing.css");
        std::fs::write(
            &root,
            "@import \"nested/colors.css\"; @import \"missing.css\";",
        )
        .unwrap();
        std::fs::write(&nested, "@import \"../base.css\";").unwrap();
        std::fs::write(&base, "@import \"style.css\";").unwrap();

        let dependencies = discover_css_dependencies(&root);
        assert_eq!(dependencies.len(), 4);
        assert!(dependencies.contains(&normalize_path(&root)));
        assert!(dependencies.contains(&normalize_path(&nested)));
        assert!(dependencies.contains(&normalize_path(&base)));
        assert!(dependencies.contains(&normalize_path(&missing)));
    }

    #[test]
    fn test_discover_css_dependencies_caps_total_work() {
        let root_dir = TestDir::new("vibepanel_test_import_limit");
        let root_dir = root_dir.path();
        std::fs::create_dir_all(root_dir).unwrap();

        let root = root_dir.join("style.css");
        let css = (0..CSS_IMPORT_SCAN_LIMIT)
            .map(|index| format!("@import \"{index}.css\";"))
            .collect::<String>();
        std::fs::write(&root, css).unwrap();

        assert_eq!(
            discover_css_dependencies(&root).len(),
            CSS_IMPORT_SCAN_LIMIT
        );
    }

    #[test]
    fn test_is_style_change_path_matches_dependency_or_parent_directory() {
        let target = normalize_path(Path::new("/run/matugen/colors.css"));
        let watched_paths = HashSet::from([target.clone()]);

        assert!(is_style_change_path(&target, &watched_paths));
        assert!(is_style_change_path(
            Path::new("/run/matugen"),
            &watched_paths
        ));
        assert!(!is_style_change_path(
            Path::new("/home/user/.cache/colors.css"),
            &watched_paths
        ));
        assert!(!is_style_change_path(
            Path::new("/tmp/style.css"),
            &watched_paths
        ));
    }

    #[cfg(unix)]
    #[test]
    fn test_compute_style_watch_paths_adds_symlink_target_dir() {
        // Create two temp dirs: one for the "config" dir (where style.css lives
        // as a symlink) and one for the "target" dir (where the real file lives).
        let config_dir = TestDir::new("vibepanel_test_symlink_config");
        let target_dir = TestDir::new("vibepanel_test_symlink_target");
        let config_dir = config_dir.path();
        let target_dir = target_dir.path();

        let target_file = target_dir.join("colors.css");
        std::fs::write(&target_file, "@import \"imported.css\";").unwrap();

        let imported_file = config_dir.join("imported.css");
        std::fs::write(&imported_file, "/* imported */").unwrap();

        let symlink_path = config_dir.join("style.css");
        std::os::unix::fs::symlink(&target_file, &symlink_path).unwrap();

        let canonical_target = target_file.canonicalize().unwrap();

        let search_paths = vec![symlink_path.clone()];
        let watched_paths =
            ConfigManager::compute_style_watch_paths(search_paths, Some(symlink_path));
        let watch_dirs = style_watch_dirs(&watched_paths, config_dir);

        // The symlink target's parent directory must be added for direct-write detection.
        assert!(
            watch_dirs.contains(target_dir),
            "expected target_dir {:?} in watch_dirs {:?}",
            target_dir,
            watch_dirs,
        );
        assert!(!watch_dirs.contains(config_dir));
        assert!(watched_paths.contains(&canonical_target));
        assert!(
            watched_paths.contains(&normalize_path(&imported_file)),
            "imports must resolve relative to the logical symlink path"
        );
    }

    #[test]
    fn test_compute_style_watch_paths_discovers_import_added_after_startup() {
        let config_dir = TestDir::new("vibepanel_test_dynamic_import_config");
        let external_dir = TestDir::new("vibepanel_test_dynamic_import_external");
        let config_dir = config_dir.path();
        let external_dir = external_dir.path();

        let root = config_dir.join("style.css");
        let imported = external_dir.join("colors.css");
        std::fs::write(&root, "/* no imports */").unwrap();

        let initial =
            ConfigManager::compute_style_watch_paths(vec![root.clone()], Some(root.clone()));
        assert!(!style_watch_dirs(&initial, config_dir).contains(external_dir));
        assert!(!initial.contains(&normalize_path(&imported)));

        std::fs::create_dir_all(external_dir).unwrap();
        std::fs::write(&imported, "/* generated colors */").unwrap();
        std::fs::write(&root, format!("@import \"{}\";", imported.display())).unwrap();

        let refreshed = ConfigManager::compute_style_watch_paths(vec![root.clone()], Some(root));
        assert!(style_watch_dirs(&refreshed, config_dir).contains(external_dir));
        assert!(refreshed.contains(&normalize_path(&imported)));
    }

    #[test]
    fn test_config_theme_changed_detects_theme_struct() {
        let old = Config::default();
        let new = Config::default();
        assert!(!config_theme_changed(&old, &new));

        // Any ThemeConfig field change is detected via PartialEq
        let mut new = old.clone();
        new.theme.popover = Some("light".to_string());
        assert!(config_theme_changed(&old, &new));
    }

    #[test]
    fn test_gtk_color_scheme_value_resolution() {
        assert_eq!(
            scheme_from_gtk_color_scheme_value("prefer-dark"),
            Some(SchemePolarity::Dark)
        );
        assert_eq!(
            scheme_from_gtk_color_scheme_value("prefer-light"),
            Some(SchemePolarity::Light)
        );
        assert_eq!(scheme_from_gtk_color_scheme_value("default"), None);
        assert_eq!(scheme_from_gtk_color_scheme_value("unexpected"), None);
    }

    #[test]
    fn test_config_uses_gtk_scheme_only_for_auto_mode() {
        let mut config = Config::default();
        config.theme.mode = "auto".to_string();
        config.theme.scheme = Some(SchemePolarity::Gtk);
        assert!(config_uses_gtk_scheme(&config));

        config.theme.mode = "gtk".to_string();
        assert!(!config_uses_gtk_scheme(&config));
    }

    #[test]
    fn test_config_theme_changed_non_theme_css_fields() {
        // Fields outside [theme] that still affect CSS
        let old = Config::default();

        let mut new = old.clone();
        new.bar.background_opacity = 0.5;
        assert!(config_theme_changed(&old, &new));

        let mut new = old.clone();
        new.widgets.popover_background_opacity = Some(0.9);
        assert!(config_theme_changed(&old, &new));

        let mut new = old.clone();
        new.bar.position = "right".to_string();
        assert!(config_theme_changed(&old, &new));

        let mut new = old.clone();
        new.bar.padding = old.bar.padding + 1;
        assert!(config_theme_changed(&old, &new));
    }

    #[test]
    fn test_config_theme_changed_detects_bar_outline_override() {
        // bar.outline = Option<bool> override; toggling it must trigger a
        // theme reload so the bar's --bar-outline-width updates live.
        let old = Config::default();

        let mut new = old.clone();
        new.bar.outline = Some(true);
        assert!(config_theme_changed(&old, &new));

        let mut new = old.clone();
        new.bar.outline = Some(false);
        assert!(config_theme_changed(&old, &new));
    }

    #[test]
    fn test_config_theme_changed_detects_bar_padding_tokens() {
        // bar.padding feeds the cached ThemePalette's internal bar padding tokens.
        // Without a theme reload, a structural rebuild can resize the layer-shell
        // window while CSS still applies the old screen-edge padding until restart.
        let old = Config::default();

        let mut new = old.clone();
        new.bar.padding = old.bar.padding + 1;
        assert!(config_theme_changed(&old, &new));
    }

    #[test]
    fn test_config_theme_changed_detects_bar_position_padding_side() {
        // bar.position swaps which CSS token is screen-adjacent vs center-adjacent.
        let old = Config::default();

        let mut new = old.clone();
        new.bar.position = "bottom".to_string();
        assert!(config_theme_changed(&old, &new));
    }

    #[test]
    fn test_config_theme_changed_detects_widgets_outline_override() {
        let old = Config::default();

        let mut new = old.clone();
        new.widgets.outline = Some(true);
        assert!(config_theme_changed(&old, &new));

        let mut new = old.clone();
        new.widgets.outline = Some(false);
        assert!(config_theme_changed(&old, &new));
    }

    #[test]
    fn test_widget_names() {
        use vibepanel_core::config::WidgetPlacement;

        let mut config = Config::default();
        config
            .widgets
            .left
            .push(WidgetPlacement::Single("workspaces".to_string()));
        config
            .widgets
            .right
            .push(WidgetPlacement::Single("clock".to_string()));

        let names = widget_names(&config);
        assert!(names.iter().any(|n| n == "left:workspaces"));
        assert!(names.iter().any(|n| n == "right:clock"));
    }

    #[test]
    fn test_widget_names_includes_show_if_fields() {
        use vibepanel_core::config::{WidgetOptions, WidgetPlacement};

        let mut config = Config::default();
        config
            .widgets
            .right
            .push(WidgetPlacement::Single("clock".to_string()));

        let names_before = widget_names(&config);

        // Adding show_if should change the fingerprint
        config.widgets.widget_configs.insert(
            "clock".to_string(),
            WidgetOptions {
                show_if: Some("true".to_string()),
                ..Default::default()
            },
        );
        let names_with_show_if = widget_names(&config);
        assert_ne!(names_before, names_with_show_if);

        // Changing show_if_interval should also change the fingerprint
        config
            .widgets
            .widget_configs
            .get_mut("clock")
            .unwrap()
            .show_if_interval = Some(30);
        let names_with_interval = widget_names(&config);
        assert_ne!(names_with_show_if, names_with_interval);
    }

    #[test]
    fn test_widget_fingerprint_is_stable_for_equivalent_configs() {
        let first_nested: toml::value::Table = [
            ("a".to_string(), toml::Value::Integer(1)),
            ("b".to_string(), toml::Value::Integer(2)),
        ]
        .into_iter()
        .collect();
        let second_nested: toml::value::Table = [
            ("b".to_string(), toml::Value::Integer(2)),
            ("a".to_string(), toml::Value::Integer(1)),
        ]
        .into_iter()
        .collect();

        let mut first = Config::default();
        first.widgets.widget_configs.insert(
            "clock".to_string(),
            WidgetOptions {
                options: [
                    (
                        "format".to_string(),
                        toml::Value::String("%H:%M".to_string()),
                    ),
                    ("show_icon".to_string(), toml::Value::Boolean(true)),
                    ("nested".to_string(), toml::Value::Table(first_nested)),
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            },
        );
        first.widgets.widget_configs.insert(
            "battery".to_string(),
            WidgetOptions {
                options: [("show_icon".to_string(), toml::Value::Boolean(true))]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        );

        let mut second = Config::default();
        second.widgets.widget_configs.insert(
            "battery".to_string(),
            WidgetOptions {
                options: [("show_icon".to_string(), toml::Value::Boolean(true))]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        );
        second.widgets.widget_configs.insert(
            "clock".to_string(),
            WidgetOptions {
                options: [
                    ("show_icon".to_string(), toml::Value::Boolean(true)),
                    (
                        "format".to_string(),
                        toml::Value::String("%H:%M".to_string()),
                    ),
                    ("nested".to_string(), toml::Value::Table(second_nested)),
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            },
        );

        assert_eq!(widget_names(&first), widget_names(&second));
        assert!(!config_structure_changed(&first, &second));
    }

    #[test]
    fn test_battery_alert_options_do_not_trigger_structural_change() {
        use vibepanel_core::config::WidgetPlacement;

        let mut old = Config::default();
        old.widgets
            .right
            .push(WidgetPlacement::Single("battery".to_string()));

        let mut new = old.clone();
        new.widgets.widget_configs.insert(
            "battery".to_string(),
            WidgetOptions {
                options: [
                    ("alerts".to_string(), toml::Value::Boolean(false)),
                    ("low_threshold".to_string(), toml::Value::Integer(30)),
                    ("critical_threshold".to_string(), toml::Value::Integer(10)),
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            },
        );

        assert!(!config_structure_changed(&old, &new));
    }

    #[test]
    fn test_battery_alert_options_do_not_trigger_structural_change_with_other_options() {
        use vibepanel_core::config::WidgetPlacement;

        let mut old = Config::default();
        old.widgets
            .right
            .push(WidgetPlacement::Single("battery".to_string()));
        old.widgets.widget_configs.insert(
            "battery".to_string(),
            WidgetOptions {
                options: [
                    ("show_icon".to_string(), toml::Value::Boolean(true)),
                    ("low_threshold".to_string(), toml::Value::Integer(20)),
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            },
        );

        let mut new = old.clone();
        new.widgets
            .widget_configs
            .get_mut("battery")
            .unwrap()
            .options
            .insert("low_threshold".to_string(), toml::Value::Integer(30));

        assert!(!config_structure_changed(&old, &new));
    }

    #[test]
    fn test_battery_non_alert_options_still_trigger_structural_change() {
        use vibepanel_core::config::WidgetPlacement;

        let mut old = Config::default();
        old.widgets
            .right
            .push(WidgetPlacement::Single("battery".to_string()));

        let mut new = old.clone();
        new.widgets.widget_configs.insert(
            "battery".to_string(),
            WidgetOptions {
                options: [("show_icon".to_string(), toml::Value::Boolean(false))]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        );

        assert!(config_structure_changed(&old, &new));
    }
}
