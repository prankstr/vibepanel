//! Configuration types and parsing.
//!
//! This module defines the bar configuration schema. The Config type is
//! intended to be a stable schema that stays relatively simple and
//! serialization-friendly. More dynamic or derived values (e.g., computed
//! theme palettes) should live in separate types in future modules.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use toml::Table;

use crate::error::{Error, Result};

/// Known valid values for workspace.backend.
const VALID_BACKENDS: &[&str] = &["auto", "mango", "hyprland", "niri"];

/// Known valid values for theme.mode.
const VALID_THEME_MODES: &[&str] = &["auto", "dark", "light", "gtk"];

/// Known valid values for osd.position.
const VALID_OSD_POSITIONS: &[&str] = &["bottom", "left", "right", "top"];

/// Embedded default configuration TOML, compiled into the binary.
pub const DEFAULT_CONFIG_TOML: &str = include_str!("../../../config.toml");

/// Result of loading a configuration file.
#[derive(Debug)]
pub struct ConfigLoadResult {
    /// The loaded configuration.
    pub config: Config,
    /// Path where config was found, if any.
    pub source: Option<PathBuf>,
    /// Whether defaults were used (no config file found).
    pub used_defaults: bool,
}

/// Root configuration structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct Config {
    /// Bar-level configuration.
    pub bar: BarConfig,

    /// Widget configuration (left, center, right sections).
    pub widgets: WidgetsConfig,

    /// Icon theme configuration.
    pub icons: IconsConfig,

    /// Workspace/compositor configuration.
    pub workspace: WorkspaceConfig,

    /// Theme configuration (colors, typography).
    pub theme: ThemeConfig,

    /// On-screen display configuration.
    pub osd: OsdConfig,

    /// Advanced configuration options.
    pub advanced: AdvancedConfig,
}

impl Config {
    /// Load configuration from an embedded default TOML string.
    pub fn from_default_toml() -> Result<Self> {
        let config: Config = toml::from_str(DEFAULT_CONFIG_TOML)?;
        Ok(config)
    }

    /// Ensure DEFAULT_CONFIG_TOML parses to the same structure as a given Config.
    /// Useful for tests that compare the raw TOML with the typed defaults.
    pub fn from_strict_default_toml() -> Result<Self> {
        let config: Config = toml::from_str(DEFAULT_CONFIG_TOML)?;
        Ok(config)
    }

    /// Load configuration from a TOML file, merging with embedded defaults.
    ///
    /// User-provided values override defaults, but any missing sections or
    /// fields fall back to the embedded default config (which includes
    /// sensible widget definitions).
    ///
    /// Returns an error if the file doesn't exist or can't be parsed.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(Error::ConfigNotFound(path.to_path_buf()));
        }

        let content = std::fs::read_to_string(path)?;
        Self::load_with_defaults(&content)
    }

    /// Load configuration from a TOML string, merging with embedded defaults.
    ///
    /// This parses both the default config and user config as TOML tables,
    /// deep-merges them (user values win), then deserializes the result.
    fn load_with_defaults(user_toml: &str) -> Result<Self> {
        // This should never fail since it's embedded and tested
        let mut base: Table = toml::from_str(DEFAULT_CONFIG_TOML)
            .expect("embedded DEFAULT_CONFIG_TOML should always be valid");

        let user: Table = toml::from_str(user_toml)?;

        deep_merge_toml(&mut base, user);

        let config: Config = base.try_into()?;
        Ok(config)
    }

    /// Find and load configuration using the XDG lookup chain.
    ///
    /// If `explicit_path` is `Some`, that path is used directly and an error
    /// is returned if it doesn't exist or can't be parsed (no fallback).
    ///
    /// If `explicit_path` is `None`, searches in order:
    /// 1. `$XDG_CONFIG_HOME/vibepanel/config.toml`
    /// 2. `~/.config/vibepanel/config.toml`
    /// 3. `./config.toml` (current working directory)
    ///
    /// If no config file is found in the search chain, returns `Config::default()`.
    pub fn find_and_load(
        explicit_path: Option<&Path>,
    ) -> std::result::Result<ConfigLoadResult, Error> {
        // If an explicit path was provided, use it strictly (no fallback)
        if let Some(path) = explicit_path {
            let config = Self::load(path)?;
            return Ok(ConfigLoadResult {
                config,
                source: Some(path.to_path_buf()),
                used_defaults: false,
            });
        }

        // No explicit path - search the XDG chain
        // Rule: if a config file exists but fails to load, that's an error (no silent fallback).
        // Only use defaults when no config files exist at all.
        let search_paths = Self::config_search_paths();
        let mut first_error: Option<(PathBuf, Error)> = None;

        for path in &search_paths {
            if path.exists() {
                match Self::load(path) {
                    Ok(config) => {
                        return Ok(ConfigLoadResult {
                            config,
                            source: Some(path.clone()),
                            used_defaults: false,
                        });
                    }
                    Err(e) => {
                        // Record the first error we encounter - we'll return it if no config loads
                        if first_error.is_none() {
                            first_error = Some((path.clone(), e));
                        }
                    }
                }
            }
        }

        // If we found at least one config file that failed to load, return that error
        // instead of silently falling back to defaults
        if let Some((path, error)) = first_error {
            tracing::error!(
                "Config file {:?} exists but failed to load: {}",
                path,
                error
            );
            return Err(error);
        }

        // No config files exist anywhere - use embedded default TOML
        tracing::info!("No config file found, using built-in default config");
        tracing::debug!(
            "Searched: {}",
            search_paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );

        let config: Config = toml::from_str(DEFAULT_CONFIG_TOML)?;

        Ok(ConfigLoadResult {
            config,
            source: None,
            used_defaults: true,
        })
    }

    /// Get the list of paths to search for config files.
    pub fn config_search_paths() -> Vec<PathBuf> {
        let mut paths = Vec::new();

        // 1. $XDG_CONFIG_HOME/vibepanel/config.toml
        if let Ok(xdg_config) = env::var("XDG_CONFIG_HOME") {
            paths.push(PathBuf::from(xdg_config).join("vibepanel/config.toml"));
        }

        // 2. ~/.config/vibepanel/config.toml
        if let Ok(home) = env::var("HOME") {
            paths.push(PathBuf::from(home).join(".config/vibepanel/config.toml"));
        }

        // 3. ./config.toml (cwd)
        paths.push(PathBuf::from("config.toml"));

        paths
    }

    /// Validate the configuration, returning errors for invalid values.
    ///
    /// This performs strict validation - any invalid value causes an error.
    pub fn validate(&self) -> Result<()> {
        let mut errors = Vec::new();

        // Validate workspace.backend
        if !VALID_BACKENDS.contains(&self.workspace.backend.as_str()) {
            errors.push(format!(
                "workspace.backend: invalid value '{}', expected one of: {}",
                self.workspace.backend,
                VALID_BACKENDS.join(", ")
            ));
        }

        // Validate theme.mode
        if !VALID_THEME_MODES.contains(&self.theme.mode.as_str()) {
            errors.push(format!(
                "theme.mode: invalid value '{}', expected one of: {}",
                self.theme.mode,
                VALID_THEME_MODES.join(", ")
            ));
        }

        // Validate theme.accent: must be "gtk", "none", or a valid hex color
        let accent = self.theme.accent.as_str();
        if accent != "gtk" && accent != "none" {
            // Must be a hex color
            let is_valid_hex = accent.starts_with('#') && {
                let hex = accent.trim_start_matches('#');
                (hex.len() == 3 || hex.len() == 6) && hex.chars().all(|c| c.is_ascii_hexdigit())
            };
            if !is_valid_hex {
                errors.push(format!(
                    "theme.accent: invalid value '{}', expected 'gtk', 'none', or a hex color like '#3584e4'",
                    accent
                ));
            }
        }

        // Validate osd.position
        if !VALID_OSD_POSITIONS.contains(&self.osd.position.as_str()) {
            errors.push(format!(
                "osd.position: invalid value '{}', expected one of: {}",
                self.osd.position,
                VALID_OSD_POSITIONS.join(", ")
            ));
        }

        // Validate numeric ranges
        if self.bar.size == 0 {
            errors.push("bar.size: must be greater than 0".to_string());
        }

        if self.osd.timeout_ms == 0 {
            errors.push("osd.timeout_ms: must be greater than 0".to_string());
        }

        // Validate opacity ranges (0.0 to 1.0)
        if !(0.0..=1.0).contains(&self.theme.bar_opacity) {
            errors.push(format!(
                "theme.bar_opacity: invalid value '{}', must be between 0.0 and 1.0",
                self.theme.bar_opacity
            ));
        }

        if !(0.0..=1.0).contains(&self.theme.widget_opacity) {
            errors.push(format!(
                "theme.widget_opacity: invalid value '{}', must be between 0.0 and 1.0",
                self.theme.widget_opacity
            ));
        }

        // Validate center widget configuration based on notch mode
        let has_center = !self.widgets.center.is_empty();

        if self.bar.notch_enabled && has_center {
            // In notch mode, center section is reserved for the notch spacer
            errors.push(
                "widgets.center: cannot be used when notch_enabled=true; \
                 use spacer widget in left/right sections to place widgets near the notch"
                    .to_string(),
            );
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(Error::ConfigValidation(errors))
        }
    }

    /// Check for potential configuration issues and return warnings.
    ///
    /// Unlike `validate()`, these are non-fatal issues that might indicate
    /// typos or unused configuration.
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        // Check for widget configs that aren't referenced in any placement array
        let unreferenced = self.widgets.unreferenced_configs();
        for name in unreferenced {
            warnings.push(format!(
                "widgets.{}: config defined but widget not used in any section (possible typo?)",
                name
            ));
        }

        // Check for spacer widgets in center section (they have no effect there)
        for placement in &self.widgets.center {
            for name in placement.widget_names() {
                let base_name = name.split(':').next().unwrap_or(name);
                if base_name == "spacer" {
                    warnings.push(
                        "widgets.center: spacer widget has no effect in center section; \
                         use spacer in left/right sections to push widgets toward the center"
                            .to_string(),
                    );
                    break;
                }
            }
        }

        warnings
    }

    /// Print a human-readable summary of the configuration.
    pub fn summary(&self) -> String {
        let mut lines = Vec::new();

        lines.push("Bar Configuration:".to_string());
        lines.push(format!("  size: {}px", self.bar.size));
        lines.push(format!("  widget_spacing: {}px", self.bar.widget_spacing));
        lines.push(format!("  outer_margin: {}px", self.bar.outer_margin));
        lines.push(format!(
            "  notch: {} (width: {}px)",
            if self.bar.notch_enabled {
                "enabled"
            } else {
                "disabled"
            },
            self.bar.notch_width
        ));
        if !self.bar.outputs.is_empty() {
            lines.push(format!("  outputs: {:?}", self.bar.outputs));
        }

        lines.push("\nWidgets:".to_string());
        lines.push(format!(
            "  left: {} widget(s)",
            count_widgets(&self.widgets.left)
        ));
        for name in format_widget_section(&self.widgets.left) {
            lines.push(format!("    - {}", name));
        }

        if self.bar.notch_enabled {
            lines.push(format!(
                "  center: notch spacer ({}px)",
                self.bar.effective_notch_width()
            ));
        } else {
            lines.push(format!(
                "  center: {} widget(s)",
                count_widgets(&self.widgets.center)
            ));
            for name in format_widget_section(&self.widgets.center) {
                lines.push(format!("    - {}", name));
            }
        }

        lines.push(format!(
            "  right: {} widget(s)",
            count_widgets(&self.widgets.right)
        ));
        for name in format_widget_section(&self.widgets.right) {
            lines.push(format!("    - {}", name));
        }

        lines.push("\nTheme:".to_string());
        lines.push(format!("  mode: {}", self.theme.mode));
        lines.push(format!("  accent: {}", self.theme.accent));
        lines.push(format!("  bar_opacity: {}", self.theme.bar_opacity));
        lines.push(format!("  widget_opacity: {}", self.theme.widget_opacity));
        if let Some(ref color) = self.theme.bar_background_color {
            lines.push(format!("  bar_background_color: {}", color));
        }
        if let Some(ref color) = self.theme.widget_background_color {
            lines.push(format!("  widget_background_color: {}", color));
        }
        lines.push(format!(
            "  font_family: {}",
            self.theme.typography.font_family
        ));

        lines.push("\nWorkspace:".to_string());
        lines.push(format!("  backend: {}", self.workspace.backend));

        lines.push("\nOSD:".to_string());
        lines.push(format!(
            "  enabled: {}, position: {}, timeout: {}ms",
            self.osd.enabled, self.osd.position, self.osd.timeout_ms
        ));

        lines.join("\n")
    }
}

/// Deep merge two TOML tables, with `overlay` values taking precedence.
///
/// For nested tables, recursively merges. For arrays and other values,
/// the overlay value completely replaces the base value.
fn deep_merge_toml(base: &mut Table, overlay: Table) {
    for (key, overlay_value) in overlay {
        match (base.get_mut(&key), overlay_value) {
            // Both are tables: recursively merge
            (Some(toml::Value::Table(base_table)), toml::Value::Table(overlay_table)) => {
                deep_merge_toml(base_table, overlay_table);
            }
            // Otherwise: overlay value wins (insert or replace)
            (_, overlay_value) => {
                base.insert(key, overlay_value);
            }
        }
    }
}

/// Bar-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BarConfig {
    /// Base height of the bar in pixels.
    pub size: u32,

    /// Spacing between widgets in pixels.
    pub widget_spacing: u32,

    /// Distance from screen edge to bar window in pixels.
    pub outer_margin: u32,

    /// Distance from bar edge to first/last section in pixels.
    pub section_edge_margin: u32,

    /// Whether notch mode is enabled.
    pub notch_enabled: bool,

    /// Width of the notch spacer in pixels.
    /// Set to 0 or omit to auto-detect (falls back to default if detection fails).
    pub notch_width: u32,

    /// Border radius (percentage of bar height).
    pub border_radius: u32,

    /// Vertical offset between widgets and their popovers/quick settings (in pixels).
    /// This creates a gap between the bar and any popover or panel that opens below it.
    /// Default: 1
    pub popover_offset: u32,

    /// Output allow-list for bar windows.
    /// If empty, bars are created on all monitors.
    /// Example: ["eDP-1", "DP-1"]
    pub outputs: Vec<String>,
}

impl Default for BarConfig {
    fn default() -> Self {
        Self {
            size: 32,
            widget_spacing: 8,
            outer_margin: 4,
            section_edge_margin: 8,
            notch_enabled: false,
            notch_width: 0,
            border_radius: 30,
            popover_offset: 1,
            outputs: Vec::new(),
        }
    }
}

/// Default notch width when auto-detection is not available.
const DEFAULT_NOTCH_WIDTH: u32 = 200;

impl BarConfig {
    /// Get the effective notch width.
    ///
    /// If `notch_width` is 0 (auto), attempts to detect the notch width.
    /// Falls back to `DEFAULT_NOTCH_WIDTH` if detection is not available.
    pub fn effective_notch_width(&self) -> u32 {
        if self.notch_width > 0 {
            return self.notch_width;
        }

        // TODO: Implement actual notch detection
        // - Compositor-specific protocols (wlr-output-configuration, etc.)
        // - Display EDID data
        // - Monitor model lookup table
        //
        // For now, fall back to default
        DEFAULT_NOTCH_WIDTH
    }
}

/// Widget section configuration.
///
/// Widget placement is defined using simple name strings or groups of names.
/// Widget-specific options are configured in separate `[widgets.<name>]` tables.
///
/// # Example
///
/// ```toml
/// [widgets]
/// left = ["workspace", "window_title"]
/// right = [
///   "system_tray",
///   { group = ["battery", "clock"] },
///   "notifications",
/// ]
///
/// [widgets.clock]
/// format = "%H:%M"
///
/// [widgets.battery]
/// disabled = true
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WidgetsConfig {
    /// Widgets in the left section.
    /// Each entry is a widget name string or a group of widget names.
    pub left: Vec<WidgetPlacement>,

    /// Widgets in the center section.
    /// Each entry is a widget name string or a group of widget names.
    /// Note: Cannot be used when notch_enabled = true (center is reserved for notch spacer).
    pub center: Vec<WidgetPlacement>,

    /// Widgets in the right section.
    /// Each entry is a widget name string or a group of widget names.
    pub right: Vec<WidgetPlacement>,

    /// Border radius (percentage of widget height).
    pub border_radius: u32,

    /// Per-widget configuration tables.
    /// Keys are widget names, values are widget-specific options.
    #[serde(flatten)]
    pub widget_configs: HashMap<String, WidgetOptions>,
}

impl Default for WidgetsConfig {
    fn default() -> Self {
        Self {
            left: Vec::new(),
            center: Vec::new(),
            right: Vec::new(),
            border_radius: 40,
            widget_configs: HashMap::new(),
        }
    }
}

impl WidgetsConfig {
    /// Check if a widget is disabled via its `[widgets.<name>]` config.
    pub fn is_disabled(&self, name: &str) -> bool {
        self.widget_configs
            .get(name)
            .map(|opts| opts.disabled)
            .unwrap_or(false)
    }

    /// Get widget options for a given widget name.
    /// Returns None if no `[widgets.<name>]` section exists.
    pub fn get_options(&self, name: &str) -> Option<&WidgetOptions> {
        self.widget_configs.get(name)
    }

    /// Parse inline argument from widget name.
    ///
    /// Supports syntax like `"spacer:50"` where the part after the colon is the inline arg.
    /// Empty args (e.g., `"spacer:"`) are treated as None.
    ///
    /// Returns `(base_name, inline_arg)`.
    ///
    /// # Examples
    /// - `"spacer"` -> `("spacer", None)`
    /// - `"spacer:50"` -> `("spacer", Some("50"))`
    /// - `"spacer:"` -> `("spacer", None)`
    fn parse_inline_arg(name: &str) -> (&str, Option<&str>) {
        if let Some(pos) = name.find(':') {
            let arg = &name[pos + 1..];
            let arg = if arg.is_empty() { None } else { Some(arg) };
            (&name[..pos], arg)
        } else {
            (name, None)
        }
    }

    /// Resolve a single widget name to a WidgetEntry, applying options from config.
    /// Returns None if the widget is disabled.
    ///
    /// Supports inline spacer width syntax like "spacer:50".
    /// This is intentionally special-cased: the inline value is parsed and injected
    /// into the resolved entry as `options["width"]`.
    fn resolve_widget(&self, name: &str) -> Option<WidgetEntry> {
        let (base_name, inline_arg) = Self::parse_inline_arg(name);

        if self.is_disabled(base_name) {
            return None;
        }

        let mut entry = if let Some(opts) = self.get_options(base_name) {
            WidgetEntry::with_options(base_name, opts)
        } else {
            WidgetEntry::new(base_name)
        };

        if base_name == "spacer"
            && let Some(arg) = inline_arg
            && !arg.is_empty()
        {
            match arg.parse::<i64>() {
                Ok(width) if width > 0 => {
                    entry
                        .options
                        .insert("width".to_string(), toml::Value::Integer(width));
                }
                _ => {
                    tracing::warn!(
                        "Invalid spacer width '{}' - expected a positive integer",
                        arg
                    );
                }
            }
        }

        Some(entry)
    }

    /// Resolve a placement to a WidgetOrGroup, applying options and filtering disabled widgets.
    /// Returns None if all widgets in the placement are disabled.
    pub fn resolve_placement(&self, placement: &WidgetPlacement) -> Option<WidgetOrGroup> {
        match placement {
            WidgetPlacement::Single(name) => self.resolve_widget(name).map(WidgetOrGroup::Single),
            WidgetPlacement::Group { group } => {
                let resolved: Vec<WidgetEntry> = group
                    .iter()
                    .filter_map(|name| self.resolve_widget(name))
                    .collect();

                if resolved.is_empty() {
                    None
                } else {
                    Some(WidgetOrGroup::Group { group: resolved })
                }
            }
        }
    }

    /// Resolve all placements in a section to WidgetOrGroup items.
    pub fn resolve_section(&self, placements: &[WidgetPlacement]) -> Vec<WidgetOrGroup> {
        placements
            .iter()
            .filter_map(|p| self.resolve_placement(p))
            .collect()
    }

    /// Get resolved widgets for the left section.
    pub fn resolved_left(&self) -> Vec<WidgetOrGroup> {
        self.resolve_section(&self.left)
    }

    /// Get resolved widgets for the center section.
    pub fn resolved_center(&self) -> Vec<WidgetOrGroup> {
        self.resolve_section(&self.center)
    }

    /// Get resolved widgets for the right section.
    pub fn resolved_right(&self) -> Vec<WidgetOrGroup> {
        self.resolve_section(&self.right)
    }

    /// Check if a widget name refers to a flexible (expandable) spacer.
    ///
    /// Returns `true` only for spacer widgets that will expand to fill available space.
    /// Returns `false` for:
    /// - Non-spacer widgets
    /// - Disabled spacers
    /// - Spacers with fixed width (via inline arg like `"spacer:50"` or TOML `width` option)
    fn is_flexible_spacer(&self, name: &str) -> bool {
        let (base_name, inline_arg) = Self::parse_inline_arg(name);

        if base_name != "spacer" || self.is_disabled(base_name) {
            return false;
        }

        // Fixed width via inline arg (e.g., "spacer:50")
        if inline_arg.is_some() {
            return false;
        }

        // Fixed width via TOML options (e.g., [widgets.spacer] width = 50)
        if let Some(opts) = self.get_options(base_name)
            && opts.options.contains_key("width")
        {
            return false;
        }

        true
    }

    /// Check if a section contains any expandable widgets (like spacer without fixed width).
    ///
    /// A flexible spacer ("spacer" or "spacer:") expands to fill available space,
    /// while a fixed spacer ("spacer:50" or with `width` in options) has a fixed width.
    ///
    /// Disabled widgets are not considered expanders.
    pub fn section_has_expander(&self, section: &[WidgetPlacement]) -> bool {
        section.iter().any(|placement| {
            placement
                .widget_names()
                .iter()
                .any(|name| self.is_flexible_spacer(name))
        })
    }

    /// Check if the left section contains an expandable widget.
    pub fn left_has_expander(&self) -> bool {
        self.section_has_expander(&self.left)
    }

    /// Check if the right section contains an expandable widget.
    pub fn right_has_expander(&self) -> bool {
        self.section_has_expander(&self.right)
    }

    /// Get all widget names referenced in any placement array.
    pub fn all_referenced_widgets(&self) -> std::collections::HashSet<String> {
        let mut names = std::collections::HashSet::new();
        for section in [&self.left, &self.center, &self.right] {
            for placement in section {
                for name in placement.widget_names() {
                    names.insert(name.to_string());
                }
            }
        }
        names
    }

    /// Check for widget configs that aren't referenced in any placement array.
    /// Returns a list of unreferenced widget names (potential typos).
    pub fn unreferenced_configs(&self) -> Vec<String> {
        let referenced = self.all_referenced_widgets();
        self.widget_configs
            .keys()
            .filter(|name| !referenced.contains(*name))
            .cloned()
            .collect()
    }
}

/// Widget placement in a section: either a single widget name or a group of names.
///
/// # Example
///
/// ```toml
/// [widgets]
/// right = [
///   "clock",                              # single widget
///   { group = ["battery", "volume"] },    # grouped widgets sharing one island
/// ]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WidgetPlacement {
    /// A group of widgets sharing one island.
    /// Must come first for untagged deserialization to work correctly.
    Group {
        /// The widget names in this group.
        group: Vec<String>,
    },
    /// A single widget name.
    Single(String),
}

impl WidgetPlacement {
    /// Returns the total number of widgets (1 for single, N for group).
    pub fn widget_count(&self) -> usize {
        match self {
            WidgetPlacement::Single(_) => 1,
            WidgetPlacement::Group { group } => group.len(),
        }
    }

    /// Returns widget names for iteration.
    pub fn widget_names(&self) -> Vec<&str> {
        match self {
            WidgetPlacement::Single(name) => vec![name.as_str()],
            WidgetPlacement::Group { group } => group.iter().map(|s| s.as_str()).collect(),
        }
    }

    /// Returns a display representation for the summary.
    pub fn display_names(&self) -> Vec<String> {
        match self {
            WidgetPlacement::Single(name) => vec![name.clone()],
            WidgetPlacement::Group { group } => {
                vec![format!("[group: {}]", group.join(", "))]
            }
        }
    }
}

/// Per-widget configuration options.
///
/// Each widget can have a `[widgets.<name>]` table with widget-specific options.
/// The `disabled` field is common to all widgets; other fields are widget-specific.
///
/// # Example
///
/// ```toml
/// [widgets.clock]
/// format = "%H:%M"
/// color = "#f5c2e7"
///
/// [widgets.battery]
/// disabled = true
/// show_percentage = true
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WidgetOptions {
    /// If true, this widget is hidden from all sections where it would appear.
    #[serde(default)]
    pub disabled: bool,

    /// Background color override for this widget (hex like "#f5c2e7").
    /// If invalid or not set, uses the theme's default widget background.
    #[serde(default)]
    pub color: Option<String>,

    /// Widget-specific options (format, show_icon, etc.).
    #[serde(flatten)]
    pub options: HashMap<String, toml::Value>,
}

/// A resolved widget entry with name and options, ready for the widget factory.
///
/// This is the internal representation used after resolving placements
/// against per-widget configuration tables.
#[derive(Debug, Clone)]
pub struct WidgetEntry {
    /// Widget type name (e.g., "clock", "battery", "workspace").
    pub name: String,

    /// Merged widget-specific options from `[widgets.<name>]`.
    pub options: HashMap<String, toml::Value>,

    /// Background color override (hex like "#f5c2e7").
    /// None means use theme default.
    pub color: Option<String>,
}

impl WidgetEntry {
    /// Create a new widget entry with the given name and empty options.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            options: HashMap::new(),
            color: None,
        }
    }

    /// Create a widget entry with options from WidgetOptions.
    pub fn with_options(name: impl Into<String>, widget_options: &WidgetOptions) -> Self {
        let name = name.into();

        // Validate color if provided - warn on invalid hex colors
        if let Some(ref color) = widget_options.color
            && crate::theme::parse_hex_color(color).is_none()
        {
            tracing::warn!(
                "Invalid color '{}' for widget '{}' - expected hex color like '#ff0000' or '#f00'",
                color,
                name
            );
        }

        Self {
            name,
            options: widget_options.options.clone(),
            color: widget_options.color.clone(),
        }
    }
}

/// A resolved widget or group, ready for the widget factory.
///
/// This mirrors `WidgetPlacement` but with resolved `WidgetEntry` objects
/// instead of just names.
#[derive(Debug, Clone)]
pub enum WidgetOrGroup {
    /// A single widget with its own island.
    Single(WidgetEntry),
    /// A group of widgets sharing one island.
    Group { group: Vec<WidgetEntry> },
}

impl WidgetOrGroup {
    /// Returns the total number of widgets (1 for single, N for group).
    pub fn widget_count(&self) -> usize {
        match self {
            WidgetOrGroup::Single(_) => 1,
            WidgetOrGroup::Group { group } => group.len(),
        }
    }

    /// Returns a display representation for the summary.
    pub fn display_names(&self) -> Vec<String> {
        match self {
            WidgetOrGroup::Single(entry) => vec![entry.name.clone()],
            WidgetOrGroup::Group { group } => {
                let names: Vec<_> = group.iter().map(|e| e.name.clone()).collect();
                vec![format!("[group: {}]", names.join(", "))]
            }
        }
    }
}

/// Helper to count total widgets in a section (handles both single and grouped).
fn count_widgets(items: &[WidgetPlacement]) -> usize {
    items.iter().map(|item| item.widget_count()).sum()
}

/// Helper to format widget section for summary display.
fn format_widget_section(items: &[WidgetPlacement]) -> Vec<String> {
    items.iter().flat_map(|item| item.display_names()).collect()
}

/// Icon theme configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IconsConfig {
    /// Icon backend: "material" for bundled Material Symbols, or "gtk" for
    /// the system GTK icon theme.
    pub theme: String,

    /// Icon stroke weight for Material Symbols (100-700). Lower = thinner strokes.
    /// Only applies when theme = "material". Default: 400.
    pub weight: u16,
}

impl Default for IconsConfig {
    fn default() -> Self {
        Self {
            theme: "material".to_string(),
            weight: 400,
        }
    }
}

/// Workspace/compositor configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkspaceConfig {
    /// Compositor backend: "auto", "mango", "hyprland", "niri".
    pub backend: String,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            backend: "auto".to_string(),
        }
    }
}

/// Theme configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeConfig {
    /// Theme mode: "auto", "dark", "light", "gtk".
    /// - "auto": detects from widget background luminance
    /// - "dark": forces dark mode (light text on dark backgrounds)
    /// - "light": forces light mode (dark text on light backgrounds)
    /// - "gtk": derive colors from GTK theme where possible
    pub mode: String,

    /// Accent color configuration: "gtk", "none", or a hex color like "#3584e4".
    /// - "gtk": use the GTK theme's accent color (don't override @accent_color)
    /// - "none": monochrome mode (no colored accents)
    /// - "#rrggbb": use this specific color as the accent
    pub accent: String,

    /// Bar background color override (CSS format, e.g., "#1a1a2e").
    /// If not set, derived from theme mode.
    pub bar_background_color: Option<String>,

    /// Bar opacity (0.0 = fully transparent, 1.0 = fully opaque).
    /// Default: 0.0 (transparent bar for "islands" look).
    pub bar_opacity: f64,

    /// Widget background color override (CSS format, e.g., "#1a1a2e").
    /// If not set, derived from theme mode.
    pub widget_background_color: Option<String>,

    /// Widget opacity (0.0 = fully transparent, 1.0 = fully opaque).
    /// Default: 1.0 (fully visible widgets).
    pub widget_opacity: f64,

    /// State colors (success, warning, urgent).
    pub states: ThemeStates,

    /// Typography settings.
    pub typography: ThemeTypography,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            mode: "auto".to_string(),
            accent: "#adabe0".to_string(),
            bar_background_color: None,
            bar_opacity: 0.0,
            widget_background_color: None,
            widget_opacity: 1.0,
            states: ThemeStates::default(),
            typography: ThemeTypography::default(),
        }
    }
}

/// Theme state colors.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeStates {
    /// Success state color.
    pub success: String,

    /// Warning state color.
    pub warning: String,

    /// Urgent state color.
    pub urgent: String,
}

impl Default for ThemeStates {
    fn default() -> Self {
        Self {
            success: "#4a7a4a".to_string(),
            warning: "#e5c07b".to_string(),
            urgent: "#ff6b6b".to_string(),
        }
    }
}

/// Theme typography settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeTypography {
    /// Base font family.
    pub font_family: String,
}

impl Default for ThemeTypography {
    fn default() -> Self {
        Self {
            font_family: "monospace".to_string(),
        }
    }
}

/// On-screen display configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OsdConfig {
    /// Whether OSD is enabled.
    pub enabled: bool,

    /// OSD position: "bottom", "left", "right".
    pub position: String,

    /// How long the OSD stays visible (milliseconds).
    pub timeout_ms: u32,
}

impl Default for OsdConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            position: "bottom".to_string(),
            timeout_ms: 1500,
        }
    }
}

/// Advanced configuration options.
///
/// These settings are for power users and workarounds for specific
/// environments. Most users should not need to change these.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct AdvancedConfig {
    /// Use Pango attributes for font rendering instead of CSS.
    ///
    /// When enabled, applies Pango font attributes directly to labels,
    /// bypassing GTK CSS font handling. This can fix font rendering issues
    /// in layer-shell surfaces where CSS-based fonts may be clipped or
    /// rendered incorrectly at certain sizes.
    ///
    /// Default: false (use standard GTK/CSS font rendering)
    pub pango_font_rendering: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.bar.size, 32);
        assert_eq!(config.bar.outer_margin, 4);
        assert_eq!(config.workspace.backend, "auto");
        assert_eq!(config.theme.mode, "auto");
        assert_eq!(config.theme.accent, "#adabe0");
        assert_eq!(config.theme.bar_opacity, 0.0);
        assert_eq!(config.theme.widget_opacity, 1.0);
        assert_eq!(config.theme.typography.font_family, "monospace");
    }

    #[test]
    fn test_embedded_default_config_parses_and_validates() {
        let config = Config::from_default_toml().expect("embedded default config should parse");
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_embedded_default_matches_struct_defaults_shape() {
        let from_toml = Config::from_default_toml().expect("embedded default config should parse");
        let from_struct = Config::default();

        // We verify that both configs are valid and have the same fundamental structure.
        // Widget lists can differ since the embedded config is a user-facing example
        // with populated widgets, while struct defaults start empty.
        assert!(
            from_toml.validate().is_ok(),
            "embedded config should validate"
        );
        assert!(
            from_struct.validate().is_ok(),
            "struct default should validate"
        );

        // Basic structural fields should match
        assert_eq!(from_toml.workspace.backend, from_struct.workspace.backend);
    }

    #[test]
    fn test_default_config_validates() {
        let config = Config::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_parse_minimal_toml() {
        // Direct TOML parsing (without merge) uses struct defaults
        let toml = r#"
            [bar]
            size = 40
        "#;

        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.bar.size, 40);
        // Struct defaults should be applied
        assert_eq!(config.bar.outer_margin, 4);
        // Without merge, widgets are empty (struct default)
        assert!(config.widgets.left.is_empty());
    }

    #[test]
    fn test_load_with_defaults_minimal_config() {
        // Minimal config should inherit widgets from embedded defaults
        let user_toml = r#"
            [bar]
            size = 40
        "#;

        let config = Config::load_with_defaults(user_toml).unwrap();

        // User-specified value should be used
        assert_eq!(config.bar.size, 40);

        // Default values from embedded config should be inherited
        assert_eq!(config.bar.outer_margin, 4);

        // Widgets should come from embedded defaults, not be empty
        assert!(
            !config.widgets.left.is_empty(),
            "left widgets should inherit from defaults"
        );
        assert!(
            !config.widgets.right.is_empty(),
            "right widgets should inherit from defaults"
        );

        // Verify the config is valid
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_load_with_defaults_override_widgets() {
        // User can override widgets completely (new format: just names)
        let user_toml = r#"
            [widgets]
            left = ["clock"]
            right = []
        "#;

        let config = Config::load_with_defaults(user_toml).unwrap();

        // User-specified widgets should override defaults
        assert_eq!(config.widgets.left.len(), 1);
        match &config.widgets.left[0] {
            WidgetPlacement::Single(name) => assert_eq!(name, "clock"),
            WidgetPlacement::Group { .. } => panic!("expected single widget"),
        }
        assert!(
            config.widgets.right.is_empty(),
            "user can set empty widgets"
        );

        // Center should still come from defaults (empty in default config.toml)
        assert!(config.widgets.center.is_empty());
    }

    #[test]
    fn test_load_with_defaults_nested_override() {
        // User can override nested values while inheriting others
        let user_toml = r#"
            [theme]
            mode = "light"
        "#;

        let config = Config::load_with_defaults(user_toml).unwrap();

        // User-specified nested value
        assert_eq!(config.theme.mode, "light");

        // Other theme values should come from defaults
        assert_eq!(config.theme.accent, "#adabe0");
        assert_eq!(config.theme.bar_opacity, 0.0);
    }

    #[test]
    fn test_load_with_defaults_empty_config() {
        // Completely empty config should use all defaults
        let user_toml = "";

        let config = Config::load_with_defaults(user_toml).unwrap();

        // Should match the embedded default config
        let default_config = Config::from_default_toml().unwrap();

        assert_eq!(config.bar.size, default_config.bar.size);
        assert_eq!(config.widgets.left.len(), default_config.widgets.left.len());
        assert_eq!(
            config.widgets.right.len(),
            default_config.widgets.right.len()
        );
    }

    #[test]
    fn test_deep_merge_toml_tables() {
        let mut base: Table = toml::from_str(
            r#"
            [section]
            a = 1
            b = 2
        "#,
        )
        .unwrap();

        let overlay: Table = toml::from_str(
            r#"
            [section]
            b = 99
            c = 3
        "#,
        )
        .unwrap();

        deep_merge_toml(&mut base, overlay);

        let section = base.get("section").unwrap().as_table().unwrap();
        assert_eq!(section.get("a").unwrap().as_integer(), Some(1)); // unchanged
        assert_eq!(section.get("b").unwrap().as_integer(), Some(99)); // overridden
        assert_eq!(section.get("c").unwrap().as_integer(), Some(3)); // added
    }

    #[test]
    fn test_deep_merge_toml_arrays_replace() {
        // Arrays should be completely replaced, not merged
        let mut base: Table = toml::from_str(
            r#"
            items = [1, 2, 3]
        "#,
        )
        .unwrap();

        let overlay: Table = toml::from_str(
            r#"
            items = [99]
        "#,
        )
        .unwrap();

        deep_merge_toml(&mut base, overlay);

        let items = base.get("items").unwrap().as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].as_integer(), Some(99));
    }

    #[test]
    fn test_load_with_defaults_rejects_unknown_fields() {
        // Typo'd keys should be rejected with a helpful error
        let user_toml = r#"
            [bar]
            sizee = 40
        "#;

        let result = Config::load_with_defaults(user_toml);
        assert!(result.is_err());

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("sizee"),
            "error should mention the unknown field"
        );
        assert!(
            err.contains("size"),
            "error should suggest the correct field"
        );
    }

    #[test]
    fn test_load_with_defaults_rejects_unknown_section() {
        // Unknown top-level sections should be rejected
        let user_toml = r#"
            [barr]
            size = 40
        "#;

        let result = Config::load_with_defaults(user_toml);
        assert!(result.is_err());

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("barr"),
            "error should mention the unknown section"
        );
    }

    #[test]
    fn test_parse_widget_entries() {
        // New format: widget names as strings, options in separate sections
        let toml = r#"
            [widgets]
            left = ["workspace", "window_title"]
            right = ["clock"]

            [widgets.workspace]
            label_type = "none"

            [widgets.window_title]
            format = "{display}"

            [widgets.clock]
            format = "%H:%M"
        "#;

        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.widgets.left.len(), 2);
        match &config.widgets.left[0] {
            WidgetPlacement::Single(name) => assert_eq!(name, "workspace"),
            WidgetPlacement::Group { .. } => panic!("expected single widget"),
        }
        assert_eq!(config.widgets.right.len(), 1);
        match &config.widgets.right[0] {
            WidgetPlacement::Single(name) => assert_eq!(name, "clock"),
            WidgetPlacement::Group { .. } => panic!("expected single widget"),
        }

        // Verify options are in widget_configs
        assert_eq!(
            config
                .widgets
                .widget_configs
                .get("clock")
                .and_then(|o| o.options.get("format"))
                .and_then(|v| v.as_str()),
            Some("%H:%M")
        );
    }

    #[test]
    fn test_validate_invalid_backend() {
        let mut config = Config::default();
        config.workspace.backend = "sway".to_string();

        let result = config.validate();
        assert!(result.is_err());

        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("workspace.backend"));
        assert!(msg.contains("sway"));
    }

    #[test]
    fn test_validate_invalid_theme_mode() {
        let mut config = Config::default();
        config.theme.mode = "night".to_string();

        let result = config.validate();
        assert!(result.is_err());

        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("theme.mode"));
    }

    #[test]
    fn test_validate_invalid_osd_position() {
        let mut config = Config::default();
        config.osd.position = "center".to_string();

        let result = config.validate();
        assert!(result.is_err());

        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("osd.position"));
    }

    #[test]
    fn test_validate_zero_bar_size() {
        let mut config = Config::default();
        config.bar.size = 0;

        let result = config.validate();
        assert!(result.is_err());

        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bar.size"));
    }

    #[test]
    fn test_validate_multiple_errors() {
        let mut config = Config::default();
        config.workspace.backend = "invalid".to_string();
        config.bar.size = 0;

        let result = config.validate();
        assert!(result.is_err());

        let err = result.unwrap_err();
        let msg = err.to_string();
        // Should contain both errors
        assert!(msg.contains("workspace.backend"));
        assert!(msg.contains("bar.size"));
    }

    #[test]
    fn test_config_search_paths() {
        let paths = Config::config_search_paths();
        // Should at least have ./config.toml
        assert!(!paths.is_empty());
        assert!(paths.iter().any(|p| p.ends_with("config.toml")));
    }

    #[test]
    fn test_validate_center_without_notch_ok() {
        let mut config = Config::default();
        config.bar.notch_enabled = false;
        config
            .widgets
            .center
            .push(WidgetPlacement::Single("clock".to_string()));

        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_center_with_notch_error() {
        let mut config = Config::default();
        config.bar.notch_enabled = true;
        config
            .widgets
            .center
            .push(WidgetPlacement::Single("clock".to_string()));

        let result = config.validate();
        assert!(result.is_err());

        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("widgets.center"));
        assert!(msg.contains("notch_enabled=true"));
    }

    #[test]
    fn test_validate_notch_mode_empty_center_ok() {
        // Notch mode with empty center section is valid
        let mut config = Config::default();
        config.bar.notch_enabled = true;
        // No widgets in center - this is correct for notch mode
        config
            .widgets
            .left
            .push(WidgetPlacement::Single("clock".to_string()));

        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_empty_center_sections_ok() {
        // Empty center sections should be valid in either mode
        let mut config = Config::default();
        config.bar.notch_enabled = false;
        assert!(config.validate().is_ok());

        config.bar.notch_enabled = true;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_effective_notch_width_explicit() {
        let config = BarConfig {
            notch_width: 300,
            ..Default::default()
        };

        assert_eq!(config.effective_notch_width(), 300);
    }

    #[test]
    fn test_effective_notch_width_auto() {
        let config = BarConfig::default();
        // Default is 0 (auto), should fall back to DEFAULT_NOTCH_WIDTH
        assert_eq!(config.notch_width, 0);
        assert_eq!(config.effective_notch_width(), 200); // DEFAULT_NOTCH_WIDTH
    }

    #[test]
    fn test_parse_widget_group() {
        // New format: groups contain just names as strings
        let toml = r#"
            [widgets]
            right = [
                "clock",
                { group = ["battery", "volume"] },
                "notifications",
            ]
        "#;

        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.widgets.right.len(), 3);

        // First: single widget
        match &config.widgets.right[0] {
            WidgetPlacement::Single(name) => assert_eq!(name, "clock"),
            WidgetPlacement::Group { .. } => panic!("expected single widget"),
        }

        // Second: group of 2 widgets
        match &config.widgets.right[1] {
            WidgetPlacement::Group { group } => {
                assert_eq!(group.len(), 2);
                assert_eq!(group[0], "battery");
                assert_eq!(group[1], "volume");
            }
            WidgetPlacement::Single(_) => panic!("expected group"),
        }

        // Third: single widget
        match &config.widgets.right[2] {
            WidgetPlacement::Single(name) => assert_eq!(name, "notifications"),
            WidgetPlacement::Group { .. } => panic!("expected single widget"),
        }
    }

    #[test]
    fn test_widget_config_options() {
        // New format: widget options in [widgets.<name>] sections
        let toml = r#"
            [widgets]
            left = [{ group = ["clock", "battery"] }]

            [widgets.clock]
            format = "%H:%M"

            [widgets.battery]
            show_percentage = true
        "#;

        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.widgets.left.len(), 1);

        // Verify options are in widget_configs
        assert_eq!(
            config
                .widgets
                .widget_configs
                .get("clock")
                .and_then(|o| o.options.get("format"))
                .and_then(|v| v.as_str()),
            Some("%H:%M")
        );
        assert_eq!(
            config
                .widgets
                .widget_configs
                .get("battery")
                .and_then(|o| o.options.get("show_percentage"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn test_widget_count_helper() {
        let single = WidgetPlacement::Single("clock".to_string());
        assert_eq!(single.widget_count(), 1);

        let group = WidgetPlacement::Group {
            group: vec!["battery".to_string(), "volume".to_string()],
        };
        assert_eq!(group.widget_count(), 2);
    }

    #[test]
    fn test_empty_widget_group() {
        let toml = r#"
            [widgets]
            right = [
                { group = [] },
            ]
        "#;

        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.widgets.right.len(), 1);

        match &config.widgets.right[0] {
            WidgetPlacement::Group { group } => {
                assert!(group.is_empty());
            }
            WidgetPlacement::Single(_) => panic!("expected group"),
        }
    }

    #[test]
    fn test_widget_disabled() {
        let toml = r#"
            [widgets]
            right = ["clock", "battery"]

            [widgets.battery]
            disabled = true
        "#;

        let config: Config = toml::from_str(toml).unwrap();

        // Both widgets are in the placement array
        assert_eq!(config.widgets.right.len(), 2);

        // But battery is disabled
        assert!(config.widgets.is_disabled("battery"));
        assert!(!config.widgets.is_disabled("clock"));

        // Resolved section should only have clock
        let resolved = config.widgets.resolved_right();
        assert_eq!(resolved.len(), 1);
        match &resolved[0] {
            WidgetOrGroup::Single(entry) => assert_eq!(entry.name, "clock"),
            WidgetOrGroup::Group { .. } => panic!("expected single widget"),
        }
    }

    #[test]
    fn test_widget_resolve_with_options() {
        let toml = r#"
            [widgets]
            right = ["clock"]

            [widgets.clock]
            format = "%H:%M"
        "#;

        let config: Config = toml::from_str(toml).unwrap();

        let resolved = config.widgets.resolved_right();
        assert_eq!(resolved.len(), 1);

        match &resolved[0] {
            WidgetOrGroup::Single(entry) => {
                assert_eq!(entry.name, "clock");
                assert_eq!(
                    entry.options.get("format").and_then(|v| v.as_str()),
                    Some("%H:%M")
                );
            }
            WidgetOrGroup::Group { .. } => panic!("expected single widget"),
        }
    }

    #[test]
    fn test_unreferenced_config_warning() {
        let toml = r#"
            [widgets]
            right = ["clock"]

            [widgets.clokc]
            format = "%H:%M"
        "#;

        let config: Config = toml::from_str(toml).unwrap();

        let unreferenced = config.widgets.unreferenced_configs();
        assert!(unreferenced.contains(&"clokc".to_string()));
    }

    #[test]
    fn test_section_has_expander_flexible_spacer() {
        let section = vec![WidgetPlacement::Single("spacer".to_string())];
        let config = WidgetsConfig::default();
        assert!(config.section_has_expander(&section));
    }

    #[test]
    fn test_section_has_expander_fixed_spacer() {
        // "spacer:50" with arg is NOT expandable
        let section = vec![WidgetPlacement::Single("spacer:50".to_string())];
        let config = WidgetsConfig::default();
        assert!(!config.section_has_expander(&section));
    }

    #[test]
    fn test_section_has_expander_empty_arg() {
        // "spacer:" with empty arg IS expandable (matches resolve_widget behavior)
        let section = vec![WidgetPlacement::Single("spacer:".to_string())];
        let config = WidgetsConfig::default();
        assert!(config.section_has_expander(&section));
    }

    #[test]
    fn test_section_has_expander_no_spacer() {
        let section = vec![
            WidgetPlacement::Single("clock".to_string()),
            WidgetPlacement::Single("battery".to_string()),
        ];
        let config = WidgetsConfig::default();
        assert!(!config.section_has_expander(&section));
    }

    #[test]
    fn test_section_has_expander_in_group() {
        // Spacer in a group should still be detected
        let section = vec![WidgetPlacement::Group {
            group: vec!["clock".to_string(), "spacer".to_string()],
        }];
        let config = WidgetsConfig::default();
        assert!(config.section_has_expander(&section));
    }

    #[test]
    fn test_section_has_expander_mixed() {
        // Mix of regular widgets and flexible spacer
        let section = vec![
            WidgetPlacement::Single("workspace".to_string()),
            WidgetPlacement::Single("window_title".to_string()),
            WidgetPlacement::Single("spacer".to_string()),
            WidgetPlacement::Single("clock".to_string()),
        ];
        let config = WidgetsConfig::default();
        assert!(config.section_has_expander(&section));
    }

    #[test]
    fn test_section_has_expander_disabled_spacer() {
        // Disabled spacer should NOT count as expander
        let section = vec![
            WidgetPlacement::Single("workspace".to_string()),
            WidgetPlacement::Single("spacer".to_string()),
        ];

        let mut config = WidgetsConfig::default();
        config.widget_configs.insert(
            "spacer".to_string(),
            WidgetOptions {
                disabled: true,
                ..Default::default()
            },
        );

        assert!(!config.section_has_expander(&section));
    }

    #[test]
    fn test_section_has_expander_width_in_options() {
        // Spacer with width defined in TOML options should NOT count as expander
        let section = vec![
            WidgetPlacement::Single("workspace".to_string()),
            WidgetPlacement::Single("spacer".to_string()),
        ];

        let mut config = WidgetsConfig::default();
        let mut options = HashMap::new();
        options.insert("width".to_string(), toml::Value::Integer(50));
        config.widget_configs.insert(
            "spacer".to_string(),
            WidgetOptions {
                options,
                ..Default::default()
            },
        );

        assert!(!config.section_has_expander(&section));
    }

    #[test]
    fn test_resolve_widget_spacer_inline_width_injects_option() {
        let config = WidgetsConfig::default();
        let entry = config.resolve_widget("spacer:50").unwrap();

        assert_eq!(entry.name, "spacer");
        assert_eq!(entry.options.get("width"), Some(&toml::Value::Integer(50)));
    }

    #[test]
    fn test_resolve_widget_spacer_inline_overrides_config_width() {
        let mut config = WidgetsConfig::default();
        let mut options = HashMap::new();
        options.insert("width".to_string(), toml::Value::Integer(100));
        config.widget_configs.insert(
            "spacer".to_string(),
            WidgetOptions {
                options,
                ..Default::default()
            },
        );

        let entry = config.resolve_widget("spacer:50").unwrap();
        assert_eq!(entry.options.get("width"), Some(&toml::Value::Integer(50)));
    }

    #[test]
    fn test_resolve_widget_spacer_invalid_inline_width_warns_and_ignores() {
        let config = WidgetsConfig::default();
        let entry = config.resolve_widget("spacer:nope").unwrap();

        assert_eq!(entry.name, "spacer");
        assert!(!entry.options.contains_key("width"));
    }
}
