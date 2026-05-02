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
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use gtk4::glib;
use notify_debouncer_mini::{DebounceEventResult, new_debouncer, notify::RecursiveMode};
use std::collections::HashSet;
use tracing::{debug, error, info, warn};

use vibepanel_core::{Config, ThemePalette, ThemeSizes};

use super::callbacks::{CallbackId, Callbacks};
use super::wallpaper::{detect_wallpaper, extract_theme_from_image, theme_from_source_color};

/// Debounce interval (in ms) for file change events. Editors often trigger
/// multiple events for a single save; this batches them into one reload.
const FILE_CHANGE_DEBOUNCE_MS: u64 = 300;

/// Polling interval (in seconds) for checking if the wallpaper changed.
/// Only active when `mode = "auto"` and no explicit wallpaper path is set.
const WALLPAPER_POLL_INTERVAL_SECS: u32 = 2;

use crate::bar;
use crate::services::bar_manager::BarManager;
use crate::services::icons::IconsService;
use crate::services::network::NetworkService;
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

/// Return true when an event path should trigger user CSS reload.
///
/// Matches `style.css` by name (for direct writes to the logical path), or an
/// exact match against the full canonical symlink target path (to avoid false
/// positives when another watched directory coincidentally contains a file with
/// the same basename).
fn is_style_change_path(
    path: &std::path::Path,
    symlink_canonical_target: Option<&std::path::Path>,
) -> bool {
    path.file_name() == Some(std::ffi::OsStr::new("style.css"))
        || symlink_canonical_target.is_some_and(|target| path == target)
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
        let palette =
            ThemePalette::from_config(&config, material_theme.as_ref(), initial_luminance);
        let popover_palette =
            ThemePalette::popover_palette(&config, material_theme.as_ref(), initial_luminance);

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
        let palette = self.palette.borrow();
        palette.surface_outline_enabled
            && palette.outline_width_px > 0
            && palette.outline_opacity_pct > 0
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

    pub fn bar_is_bottom(&self) -> bool {
        self.config.borrow().bar.is_bottom()
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

    /// Compute the set of directories to watch for `style.css` changes, and
    /// (if the active `style.css` is a symlink) the full canonical path of the
    /// symlink target.
    ///
    /// `search_paths` and `style_css_logical` are passed in so the function
    /// can be unit-tested without touching global env vars.
    ///
    /// `config_watch_dir` is excluded from the returned set because it is
    /// already covered by the config file watcher.
    fn compute_style_watch_info(
        search_paths: Vec<PathBuf>,
        style_css_logical: Option<PathBuf>,
        config_watch_dir: &std::path::Path,
    ) -> (HashSet<PathBuf>, Option<PathBuf>) {
        let mut watch_dirs: HashSet<PathBuf> = search_paths
            .into_iter()
            .map(|path| make_absolute(&path))
            .filter_map(|path| path.parent().map(|dir| dir.to_path_buf()))
            .collect();
        watch_dirs.remove(config_watch_dir);

        let mut symlink_canonical_target: Option<PathBuf> = None;

        if let Some(logical) = style_css_logical
            && let Ok(meta) = std::fs::symlink_metadata(&logical)
            && meta.file_type().is_symlink()
            && let Ok(canonical) = logical.canonicalize()
            && let Some(target_dir) = canonical.parent()
        {
            info!(
                "style.css is a symlink: {} -> {}",
                logical.display(),
                canonical.display()
            );
            watch_dirs.insert(target_dir.to_path_buf());
            symlink_canonical_target = Some(canonical);
        }

        (watch_dirs, symlink_canonical_target)
    }

    /// Run the file watcher loop (called on a background thread).
    ///
    /// Watches `config.toml` and user `style.css`, including the symlink target's
    /// parent directory so direct writes (e.g. Matugen) are detected.
    ///
    /// **Limitation:** watch directories are resolved once at startup. Changes
    /// within already-watched directories (including a new higher-priority
    /// `style.css` appearing there) are detected normally. Writes will be
    /// missed until restart in two cases: if `style.css` becomes a symlink
    /// after startup (or an existing symlink is re-pointed to a directory not
    /// already watched), or if a candidate directory that did not exist at
    /// startup is created later.
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

        let (style_watch_dirs, symlink_canonical_target) = Self::compute_style_watch_info(
            crate::bar::user_css_search_paths(config_dir.as_deref()),
            crate::bar::find_user_css(config_dir.as_deref()),
            &config_watch_dir,
        );

        debug!(
            "Style CSS watch directories: {:?}",
            style_watch_dirs
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
        );

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

                        let style_changed = events.iter().any(|e| {
                            is_style_change_path(&e.path, symlink_canonical_target.as_deref())
                        });
                        if style_changed {
                            debug!("User style.css change detected");
                            send_config_message(ConfigMessage::StyleCssChanged);
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

        for watch_dir in style_watch_dirs {
            if !watch_dir.is_dir() {
                debug!(
                    "Skipping style.css watch for non-directory path: {}",
                    watch_dir.display()
                );
                continue;
            }

            if let Err(e) = debouncer
                .watcher()
                .watch(&watch_dir, RecursiveMode::NonRecursive)
            {
                warn!(
                    "Failed to watch style.css directory {}: {}",
                    watch_dir.display(),
                    e
                );
            } else {
                info!("Also watching style.css directory: {}", watch_dir.display());
            }
        }

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
    pub(crate) fn handle_config_message(self: &Rc<Self>, msg: ConfigMessage) {
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

            let luminance = self.cached_luminance.get();
            // Rebuild the cached palette once
            let palette =
                ThemePalette::from_config(&new_config, material_theme.as_ref(), luminance);
            let popover_palette =
                ThemePalette::popover_palette(&new_config, material_theme.as_ref(), luminance);
            let surface_styles = palette.surface_styles();

            // Update surface style manager
            SurfaceStyleManager::global().reconfigure(
                surface_styles.clone(),
                new_config.advanced.pango_font_rendering,
            );

            // Update tooltip manager
            TooltipManager::global().reconfigure(surface_styles);

            // Update the cached palettes before load_css so they're available
            *self.palette.borrow_mut() = palette;
            *self.popover_palette.borrow_mut() = popover_palette;

            // Reload CSS with new theme values
            bar::load_css(&new_config);

            debug!("Theme styles updated");
        }

        // Store the new config AFTER theme/CSS update but BEFORE widget rebuild,
        // so widgets see the new values when notified
        *self.config.borrow_mut() = new_config.clone();

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

                let palette =
                    ThemePalette::from_config(&config, material_theme.as_ref(), new_luminance);
                let popover_palette =
                    ThemePalette::popover_palette(&config, material_theme.as_ref(), new_luminance);
                let surface_styles = palette.surface_styles();

                SurfaceStyleManager::global()
                    .reconfigure(surface_styles.clone(), config.advanced.pango_font_rendering);
                TooltipManager::global().reconfigure(surface_styles);

                *mgr.palette.borrow_mut() = palette;
                *mgr.popover_palette.borrow_mut() = popover_palette;
                bar::load_css(&config);

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

    // Also include per-widget configs for comparison
    for (name, opts) in &config.widgets.widget_configs {
        names.push(format!(
            "config:{}:disabled={},click_r={:?},click_m={:?},show_if={:?},show_if_interval={:?},{:?}",
            name, opts.disabled, opts.on_click_right, opts.on_click_middle,
            opts.show_if, opts.show_if_interval, opts.options
        ));
    }

    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_make_absolute_passthrough_for_absolute_path() {
        let absolute = Path::new("/tmp/vibepanel-style.css");
        assert_eq!(make_absolute(absolute), absolute.to_path_buf());
    }

    #[test]
    fn test_make_absolute_joins_current_dir_for_relative_path() {
        let relative = Path::new("style.css");
        let expected = std::env::current_dir().unwrap().join(relative);
        assert_eq!(make_absolute(relative), expected);
    }

    #[test]
    fn test_is_style_change_path_matches_style_css() {
        assert!(is_style_change_path(Path::new("/tmp/style.css"), None));
    }

    #[test]
    fn test_is_style_change_path_matches_exact_canonical_target() {
        // Only the exact canonical path triggers a reload — not a same-named
        // file in a different directory.
        let target = Path::new("/run/matugen/colors.css");
        assert!(is_style_change_path(target, Some(target)));
    }

    #[test]
    fn test_is_style_change_path_rejects_same_name_different_dir() {
        // A file named "colors.css" in a different directory must NOT match,
        // unlike the old basename-only comparison which would have fired.
        let target = Path::new("/run/matugen/colors.css");
        let unrelated = Path::new("/home/user/.cache/colors.css");
        assert!(!is_style_change_path(unrelated, Some(target)));
    }

    #[test]
    fn test_is_style_change_path_target_name_none() {
        assert!(!is_style_change_path(Path::new("/tmp/colors.css"), None));
    }

    #[cfg(unix)]
    #[test]
    fn test_compute_style_watch_info_adds_symlink_target_dir() {
        use std::time::{SystemTime, UNIX_EPOCH};

        // Create two temp dirs: one for the "config" dir (where style.css lives
        // as a symlink) and one for the "target" dir (where the real file lives).
        let unique = format!(
            "vibepanel_test_symlink_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let config_dir = std::env::temp_dir().join(format!("{}_config", unique));
        let target_dir = std::env::temp_dir().join(format!("{}_target", unique));
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::create_dir_all(&target_dir).unwrap();

        let target_file = target_dir.join("colors.css");
        std::fs::write(&target_file, "/* target */").unwrap();

        let symlink_path = config_dir.join("style.css");
        std::os::unix::fs::symlink(&target_file, &symlink_path).unwrap();

        let canonical_target = target_file.canonicalize().unwrap();

        let search_paths = vec![symlink_path.clone()];
        let (watch_dirs, symlink_canonical_target) = ConfigManager::compute_style_watch_info(
            search_paths,
            Some(symlink_path),
            // Exclude config_dir to mirror the real usage.
            &config_dir,
        );

        // The symlink target's parent directory must be added for direct-write detection.
        assert!(
            watch_dirs.contains(&target_dir),
            "expected target_dir {:?} in watch_dirs {:?}",
            target_dir,
            watch_dirs,
        );
        // The returned canonical target must be the exact target file path.
        assert_eq!(symlink_canonical_target, Some(canonical_target));

        let _ = std::fs::remove_dir_all(&config_dir);
        let _ = std::fs::remove_dir_all(&target_dir);
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
    fn test_config_theme_changed_non_theme_css_fields() {
        // Fields outside [theme] that still affect CSS
        let old = Config::default();

        let mut new = old.clone();
        new.bar.background_opacity = 0.5;
        assert!(config_theme_changed(&old, &new));

        let mut new = old.clone();
        new.widgets.popover_background_opacity = Some(0.9);
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
}
