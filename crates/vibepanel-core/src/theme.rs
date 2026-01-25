//! Unified theming system for vibepanel.
//!
//! `ThemePalette` is the single source of truth for all theme-related values.
//! It parses config, computes derived values, and generates CSS variables.

use crate::Config;

// Overlay opacities: base values for card backgrounds.
// Dark mode uses lower opacity (0.06) since white overlays on dark are more visible.
// Light mode uses higher opacity (0.14) to maintain visible separation on light backgrounds.
const OVERLAY_OPACITY_DARK: f64 = 0.06;
const OVERLAY_OPACITY_LIGHT: f64 = 0.14;

// Overlay multipliers for interactive states
const HOVER_MULTIPLIER: f64 = 2.2;
const ACTIVE_MULTIPLIER: f64 = 2.0;
const SUBTLE_MULTIPLIER: f64 = 0.5;

// Click catcher: nearly invisible but clickable
const CLICK_CATCHER_OPACITY: f64 = 0.005;

// Border opacities (subtle borders that don't compete with content)
const BORDER_OPACITY_DARK: f64 = 0.10;
const BORDER_OPACITY_LIGHT: f64 = 0.12;

// Shadow configuration (layered shadows for natural look)
const SHADOW_OPACITY_DARK: f64 = 0.40;
const SHADOW_OPACITY_LIGHT: f64 = 0.25;
const SHADOW_TIGHT_OFFSET_Y: u32 = 1;
const SHADOW_TIGHT_BLUR: u32 = 2;
const SHADOW_TIGHT_OPACITY_FACTOR: f64 = 0.5;
const SHADOW_DIFFUSE_OFFSET_Y: u32 = 1;
const SHADOW_DIFFUSE_BLUR_SOFT: u32 = 3;
const SHADOW_DIFFUSE_BLUR_STRONG: u32 = 5;
const SHADOW_DIFFUSE_OPACITY_FACTOR: f64 = 0.6;

// Slider track opacities
const TRACK_OPACITY_DARK: f64 = 0.15;
const TRACK_OPACITY_LIGHT: f64 = 0.12;

// Foreground opacity factors for text hierarchy (secondary, tertiary, disabled)
const FOREGROUND_MUTED_OPACITY: f64 = 0.7;
const FOREGROUND_SUBTLE_OPACITY: f64 = 0.4;
const FOREGROUND_DISABLED_OPACITY: f64 = 0.4;

// Toast critical background blend weight
const TOAST_CRITICAL_URGENT_WEIGHT: f64 = 0.35;

// Default colors (based on typical dark/light theme surface colors)
const DEFAULT_BAR_BG_DARK: &str = "#1a1a1f";
const DEFAULT_BAR_BG_LIGHT: &str = "#e8e8e8";
const DEFAULT_WIDGET_BG_DARK: &str = "#111217";
const DEFAULT_WIDGET_BG_LIGHT: &str = "#ffffff";
const DEFAULT_STATE_SUCCESS: &str = "#4a7a4a";
const DEFAULT_STATE_WARNING: &str = "#e5c07b";
const DEFAULT_STATE_URGENT: &str = "#ff6b6b";
const DEFAULT_FONT_FAMILY: &str = "\"Cascadia Mono NF\", monospace";

// Size scaling factors (empirically tuned for visual balance at bar sizes 28-60px)
const FONT_SCALE: f64 = 0.6;
const TEXT_ICON_SCALE: f64 = 0.50;
const PIXMAP_ICON_SCALE: f64 = 0.50;
const PADDING_SCALE: f64 = 0.14;
const SPACING_SCALE: f64 = 0.25;
// Fixed 2px vertical padding for widgets ensures consistent spacing regardless of bar size.

/// Round a value to the nearest even number (for proper centering with integer pixels).
fn round_to_even(value: u32) -> u32 {
    if value.is_multiple_of(2) {
        value
    } else {
        value + 1
    }
}

/// Where the accent color comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum AccentSource {
    /// Use GTK theme's accent color (don't override @accent_color).
    Gtk,
    /// Monochrome mode - no colored accents.
    None,
    /// Use a specific custom color.
    Custom(String),
}

/// Parse a hex color string to RGB tuple. Returns None if invalid.
pub fn parse_hex_color(color: &str) -> Option<(u8, u8, u8)> {
    let color = color.trim().trim_start_matches('#');

    // Expand shorthand (e.g., "fff" -> "ffffff")
    let color = if color.len() == 3 {
        color.chars().flat_map(|c| [c, c]).collect::<String>()
    } else {
        color.to_string()
    };

    if color.len() != 6 {
        return None;
    }

    let r = u8::from_str_radix(&color[0..2], 16).ok()?;
    let g = u8::from_str_radix(&color[2..4], 16).ok()?;
    let b = u8::from_str_radix(&color[4..6], 16).ok()?;

    Some((r, g, b))
}

/// Calculate relative luminance per WCAG formula (0.0 = black, 1.0 = white).
pub fn relative_luminance(r: u8, g: u8, b: u8) -> f64 {
    fn channel(c: u8) -> f64 {
        let c_srgb = c as f64 / 255.0;
        if c_srgb <= 0.03928 {
            c_srgb / 12.92
        } else {
            ((c_srgb + 0.055) / 1.055).powf(2.4)
        }
    }

    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

/// Return true if the color is considered dark (low luminance).
pub fn is_dark_color(color: &str) -> bool {
    is_dark_color_with_threshold(color, 0.179)
}

/// Return true if the color is considered dark, with custom threshold.
pub fn is_dark_color_with_threshold(color: &str, threshold: f64) -> bool {
    match parse_hex_color(color) {
        Some((r, g, b)) => relative_luminance(r, g, b) < threshold,
        None => true, // Default to dark if parsing fails
    }
}

/// Blend two hex colors together.
///
/// `weight1` is the weight for color1 (0.0 to 1.0), color2 gets (1 - weight1).
pub fn blend_colors(color1: &str, color2: &str, weight1: f64) -> Option<(u8, u8, u8)> {
    let rgb1 = parse_hex_color(color1)?;
    let rgb2 = parse_hex_color(color2)?;

    let weight2 = 1.0 - weight1;
    let r = (rgb1.0 as f64 * weight1 + rgb2.0 as f64 * weight2) as u8;
    let g = (rgb1.1 as f64 * weight1 + rgb2.1 as f64 * weight2) as u8;
    let b = (rgb1.2 as f64 * weight1 + rgb2.2 as f64 * weight2) as u8;

    Some((r, g, b))
}

/// Convert RGB tuple to hex color string.
pub fn rgb_to_hex(r: u8, g: u8, b: u8) -> String {
    format!("#{:02x}{:02x}{:02x}", r, g, b)
}

/// Format an RGBA color string.
pub fn rgba_str(r: u8, g: u8, b: u8, a: f64) -> String {
    format!("rgba({}, {}, {}, {:.2})", r, g, b, a)
}

/// Computed sizes based on bar height.
#[derive(Debug, Clone)]
pub struct ThemeSizes {
    pub bar_height: u32,
    pub bar_padding: u32,
    pub widget_height: u32,
    pub widget_padding_x: u32,
    pub widget_padding_y: u32,
    pub font_size: u32,
    pub text_icon_size: u32,
    pub pixmap_icon_size: u32,
    pub internal_spacing: u32,
    /// Edge padding for widget content (inside .content box)
    pub widget_content_edge: u32,
    /// Gap between children inside widget content
    pub widget_content_gap: u32,
}

impl Default for ThemeSizes {
    fn default() -> Self {
        Self {
            bar_height: 36,
            bar_padding: 5,
            widget_height: 26,
            widget_padding_x: 5,
            widget_padding_y: 2,
            font_size: 14,
            text_icon_size: 16,
            pixmap_icon_size: 15,
            internal_spacing: 9,
            widget_content_edge: 6,
            widget_content_gap: 10,
        }
    }
}

/// Styles for popover/menu surfaces.
#[derive(Debug, Clone)]
pub struct SurfaceStyles {
    pub background_color: String,
    pub text_color: String,
    pub font_family: String,
    pub font_size: u32,
    pub border_radius: u32,
    pub border_color: String,
    pub opacity: f64,
    pub shadow: String,
    pub is_dark_mode: bool,
}

/// Single source of truth for all theme values.
///
/// Constructed via `ThemePalette::from_config(&config)`.
#[derive(Debug, Clone)]
pub struct ThemePalette {
    // Mode
    pub is_dark_mode: bool,
    /// Whether mode is "gtk" (derive colors from GTK theme).
    pub is_gtk_mode: bool,

    // Background colors
    pub bar_background: String,
    pub widget_background: String,

    // Foreground colors
    pub foreground_primary: String,
    pub foreground_muted: String,
    pub foreground_subtle: String,
    pub foreground_disabled: String,

    // Accent configuration
    pub accent_source: AccentSource,
    /// Primary accent color (only meaningful when accent_source is Custom).
    pub accent_primary: String,
    pub accent_subtle: String,
    pub accent_text: String,
    // NOTE: accent_icon and accent_slider were removed - they always equaled accent_primary.
    // CSS vars --color-accent-icon and --color-accent-slider now alias to --color-accent-primary.

    // State colors
    pub state_success: String,
    pub state_warning: String,
    pub state_urgent: String,

    // Overlay colors
    pub card_overlay: String,
    pub card_overlay_hover: String,
    pub card_overlay_subtle: String,
    pub card_overlay_strong: String,
    pub click_catcher_overlay: String,

    // Border and shadows
    pub border_subtle: String,
    pub shadow_soft: String,
    pub shadow_strong: String,

    // Slider tracks
    pub slider_track: String,
    pub slider_track_disabled: String,

    // Critical backgrounds
    pub row_critical_background: String,
    pub toast_critical_background: String,

    // Typography
    pub font_family: String,

    // Opacities
    pub bar_opacity: f64,
    pub widget_opacity: f64,

    // Radii (pixels)
    pub bar_border_radius: u32,
    pub widget_border_radius: u32,
    pub surface_border_radius: u32,
    pub radius_pill: u32,

    // Sizes
    pub sizes: ThemeSizes,

    // Internal: config values needed for computation
    bar_radius_percent: u32,
    widget_radius_percent: u32,
    bar_size: u32,
}

impl ThemePalette {
    /// Create a ThemePalette from configuration.
    pub fn from_config(config: &Config) -> Self {
        let mut palette = Self::default();
        palette.parse_config(config);
        palette.compute_derived_values();
        palette
    }

    /// Generate the :root CSS variable block.
    pub fn css_vars_block(&self) -> String {
        // For GTK accent mode, we reference @accent_color in CSS.
        // For custom/none modes, we use computed values.
        let (accent_primary_css, accent_subtle_css) = match &self.accent_source {
            AccentSource::Gtk => (
                // Reference GTK's accent color
                "@accent_color".to_string(),
                "color-mix(in srgb, @accent_color 20%, transparent)".to_string(),
            ),
            _ => (self.accent_primary.clone(), self.accent_subtle.clone()),
        };

        format!(
            r#"
:root {{
    /* ===== Background Colors ===== */
    /* Bar background with opacity applied via color-mix */
    --color-background-bar: {bar_bg_with_opacity};
    --color-background-widget: {widget_bg};

    /* ===== Foreground Colors ===== */
    --color-foreground-primary: {fg_primary};
    --color-foreground-muted: {fg_muted};
    --color-foreground-subtle: {fg_subtle};
    --color-foreground-disabled: {fg_disabled};

    /* ===== Accent Colors ===== */
    --color-accent-primary: {accent_primary};
    --color-accent-subtle: {accent_subtle};
    /* Slider accent - alias for user CSS overrides */
    --color-accent-slider: var(--color-accent-primary);
    --color-accent-text: {accent_text};

    /* ===== State Colors ===== */
    --color-state-success: {state_success};
    --color-state-warning: {state_warning};
    --color-state-urgent: {state_urgent};

    /* ===== Card Overlays ===== */
    --color-card-overlay: {card_overlay};
    --color-card-overlay-hover: {card_overlay_hover};
    --color-card-overlay-subtle: {card_overlay_subtle};
    --color-card-overlay-strong: {card_overlay_strong};
    --color-click-catcher-overlay: {click_catcher_overlay};

    /* ===== Borders & Shadows ===== */
    --color-border-subtle: {border_subtle};
    --shadow-soft: {shadow_soft};
    --shadow-strong: {shadow_strong};

    /* ===== Slider Tracks ===== */
    --color-slider-track: {slider_track};
    --color-slider-track-disabled: {slider_track_disabled};

    /* ===== Contextual Backgrounds ===== */
    --color-row-background: var(--color-card-overlay-subtle);
    --color-row-background-hover: var(--color-card-overlay-hover);
    --color-row-critical-background: {row_critical_bg};
    --color-toast-critical-background: {toast_critical_bg};

    /* ===== Radii ===== */
    --radius-bar: {radius_bar}px;
    --radius-surface: {radius_surface}px;
    --radius-widget: {radius_widget};
    --radius-widget-lg: calc({radius_widget} * 2);            /* Larger surfaces that scale with widget */
    --radius-pill: {radius_pill}px;
    --radius-card: {radius_card}px;                        /* Cards/containers - never goes pill */
    --radius-track: calc(var(--radius-pill) * 0.4);        /* Thin (6px) slider troughs */
    --radius-track-thick: calc(var(--radius-pill) * 0.75); /* Thick (10px) OSD sliders, switch sliders */
    --radius-round: 9999px;                                /* Always circular */

    /* ===== Sizes & Spacing ===== */
    --bar-height: {bar_height}px;
    --bar-padding: {bar_padding}px;
    --widget-height: {widget_height}px;
    --widget-padding-x: {widget_padding_x}px;
    --widget-padding-y: {widget_padding_y}px;
    --spacing-internal: {internal_spacing}px;
    --spacing-widget-edge: {widget_content_edge}px;
    --spacing-widget-gap: {widget_content_gap}px;
    --widget-opacity: {widget_opacity};

    /* Spacing tokens - consistent spacing scale */
    --spacing-xs: 4px;
    --spacing-sm: 8px;
    --spacing-md: 12px;
    --spacing-lg: 16px;
    --spacing-xl: 24px;

    /* Component tokens */
    --card-padding: var(--spacing-md);
    --row-padding-v: var(--spacing-sm);
    --row-padding-h: var(--spacing-md);
    --slider-height: 6px;

    /* ===== Typography ===== */
    --font-family: {font_family};
    --font-scale: {font_scale};
    --font-size: calc(var(--widget-height) * var(--font-scale));
    --font-size-text-icon: {text_icon_size}px;

    /* Font size scale for visual hierarchy */
    --font-size-lg: 1.1em;    /* Headings, section titles */
    --font-size-base: 1.0em;  /* Primary content, main labels */
    --font-size-md: 0.9em;    /* Row titles, content that needs slight reduction */
    --font-size-sm: 0.85em;   /* Supporting content, secondary text */
    --font-size-xs: 0.7em;    /* De-emphasized (timestamps, week numbers) */

    /* ===== Icon Sizes ===== */
    --pixmap-icon-size: {pixmap_icon_size}px;
    /* Canonical icon box size for bar widgets - all icons sit in this size container */
    --icon-size: {text_icon_size}px;
}}
"#,
            bar_bg_with_opacity = self.bar_background_with_opacity(),
            widget_bg = self.widget_background_with_opacity(),
            fg_primary = self.foreground_primary,
            fg_muted = self.foreground_muted,
            fg_subtle = self.foreground_subtle,
            fg_disabled = self.foreground_disabled,
            accent_primary = accent_primary_css,
            accent_subtle = accent_subtle_css,
            accent_text = self.accent_text,
            state_success = self.state_success,
            state_warning = self.state_warning,
            state_urgent = self.state_urgent,
            card_overlay = self.card_overlay,
            card_overlay_hover = self.card_overlay_hover,
            card_overlay_subtle = self.card_overlay_subtle,
            card_overlay_strong = self.card_overlay_strong,
            click_catcher_overlay = self.click_catcher_overlay,
            border_subtle = self.border_subtle,
            shadow_soft = self.shadow_soft,
            shadow_strong = self.shadow_strong,
            slider_track = self.slider_track,
            slider_track_disabled = self.slider_track_disabled,
            row_critical_bg = self.row_critical_background,
            toast_critical_bg = self.toast_critical_background,
            radius_bar = self.bar_border_radius,
            radius_surface = self.surface_border_radius,
            radius_widget = if self.widget_radius_percent >= 50 {
                "9999px".to_string()
            } else {
                format!("{}px", self.widget_border_radius)
            },
            radius_card = self.widget_border_radius,
            radius_pill = self.radius_pill,
            bar_height = self.sizes.bar_height,
            bar_padding = self.sizes.bar_padding,
            widget_height = self.sizes.widget_height,
            widget_padding_x = self.sizes.widget_padding_x,
            widget_padding_y = self.sizes.widget_padding_y,
            internal_spacing = self.sizes.internal_spacing,
            widget_content_edge = self.sizes.widget_content_edge,
            widget_content_gap = self.sizes.widget_content_gap,
            widget_opacity = self.widget_opacity,
            font_family = self.font_family,
            font_scale = FONT_SCALE,
            text_icon_size = self.sizes.text_icon_size,
            pixmap_icon_size = self.sizes.pixmap_icon_size,
        )
    }

    /// Generate bar background CSS value with opacity applied.
    ///
    /// For opacity 0, returns "transparent".
    /// For opacity 1, returns the raw background color.
    /// For values in between, uses color-mix to blend with transparent.
    fn bar_background_with_opacity(&self) -> String {
        if self.bar_opacity <= 0.0 {
            "transparent".to_string()
        } else if self.bar_opacity >= 1.0 {
            self.bar_background.clone()
        } else {
            // Use color-mix to apply opacity to the background
            // This works for both hex colors and GTK CSS variables like @window_bg_color
            let opacity_percent = (self.bar_opacity * 100.0).round() as u32;
            format!(
                "color-mix(in srgb, {} {}%, transparent)",
                self.bar_background, opacity_percent
            )
        }
    }

    /// Generate widget background CSS value with opacity applied.
    ///
    /// For opacity 0, returns "transparent".
    /// For opacity 1, returns the raw background color.
    /// For values in between, uses color-mix to blend with transparent.
    fn widget_background_with_opacity(&self) -> String {
        if self.widget_opacity <= 0.0 {
            "transparent".to_string()
        } else if self.widget_opacity >= 1.0 {
            self.widget_background.clone()
        } else {
            // Use color-mix to apply opacity to the background
            // This works for both hex colors and GTK CSS variables like @view_bg_color
            let opacity_percent = (self.widget_opacity * 100.0).round() as u32;
            format!(
                "color-mix(in srgb, {} {}%, transparent)",
                self.widget_background, opacity_percent
            )
        }
    }

    /// Get surface styling for popovers and menus.
    pub fn surface_styles(&self) -> SurfaceStyles {
        SurfaceStyles {
            background_color: self.widget_background.clone(),
            text_color: self.foreground_primary.clone(),
            font_family: self.font_family.clone(),
            font_size: self.sizes.font_size,
            border_radius: self.surface_border_radius,
            border_color: self.border_subtle.clone(),
            opacity: self.widget_opacity,
            shadow: self.shadow_soft.clone(),
            is_dark_mode: self.is_dark_mode,
        }
    }

    fn parse_config(&mut self, config: &Config) {
        // Check if GTK mode is requested
        self.is_gtk_mode = config.theme.mode == "gtk";

        // Determine which default backgrounds to use based on explicit mode
        // For "gtk" mode, we reference GTK CSS variables instead of hardcoded colors
        let (default_bar_bg, default_widget_bg) = if self.is_gtk_mode {
            // Reference GTK theme's colors - these will be resolved by GTK at runtime
            ("@window_bg_color".to_string(), "@view_bg_color".to_string())
        } else if config.theme.mode == "light" {
            (
                DEFAULT_BAR_BG_LIGHT.to_string(),
                DEFAULT_WIDGET_BG_LIGHT.to_string(),
            )
        } else {
            (
                DEFAULT_BAR_BG_DARK.to_string(),
                DEFAULT_WIDGET_BG_DARK.to_string(),
            )
        };

        // Bar background - user can override with explicit color in bar.background_color
        self.bar_background = config
            .bar
            .background_color
            .clone()
            .unwrap_or(default_bar_bg);

        // Widget background - user can override with explicit color in widgets.background_color
        self.widget_background = config
            .widgets
            .background_color
            .clone()
            .unwrap_or(default_widget_bg);

        // Opacities from bar/widgets config
        self.bar_opacity = config.bar.background_opacity;
        self.widget_opacity = config.widgets.background_opacity;

        // Resolve is_dark_mode
        // For GTK mode, we assume dark for overlay calculations since we can't query GTK's actual colors at build time
        self.is_dark_mode = match config.theme.mode.as_str() {
            "dark" => true,
            "light" => false,
            "gtk" => true, // Default to dark for overlays/borders; GTK handles actual background colors
            _ => is_dark_color(&self.widget_background), // "auto"
        };

        // Parse accent configuration from the single `theme.accent` field
        let accent_str = config.theme.accent.as_str();
        self.accent_source = match accent_str {
            "gtk" => AccentSource::Gtk,
            "none" => AccentSource::None,
            color => AccentSource::Custom(color.to_string()),
        };

        // Set accent colors based on source
        match &self.accent_source {
            AccentSource::Custom(color) => {
                self.accent_primary = color.clone();
            }
            AccentSource::None => {
                // Monochrome mode - use mode-appropriate colors
                if self.is_dark_mode {
                    self.accent_primary = "rgba(255, 255, 255, 0.25)".to_string();
                } else {
                    self.accent_primary = "rgba(0, 0, 0, 0.20)".to_string();
                }
            }
            AccentSource::Gtk => {
                // For GTK accent, we'll reference @accent_color in CSS.
                // Store a fallback value here for any code that reads accent_primary directly.
                self.accent_primary = "@accent_color".to_string();
            }
        }

        // State colors
        self.state_success = config.theme.states.success.clone();
        self.state_warning = config.theme.states.warning.clone();
        self.state_urgent = config.theme.states.urgent.clone();

        // Typography - use "inherit" for empty font_family to use system font
        self.font_family = if config.theme.typography.font_family.is_empty() {
            "inherit".to_string()
        } else {
            config.theme.typography.font_family.clone()
        };

        // Radii percentages (now directly on bar/widgets)
        self.bar_radius_percent = config.bar.border_radius;
        self.widget_radius_percent = config.widgets.border_radius;

        // Bar size
        self.bar_size = config.bar.size;
    }

    fn compute_derived_values(&mut self) {
        self.compute_foreground_colors();
        self.compute_accent_derived();
        self.compute_overlays();
        self.compute_borders_and_shadows();
        self.compute_slider_tracks();
        self.compute_critical_backgrounds();
        self.compute_sizes();
    }

    fn compute_foreground_colors(&mut self) {
        if self.is_dark_mode {
            self.foreground_primary = "#ffffff".to_string();
            self.foreground_muted = format!("rgba(255, 255, 255, {:.2})", FOREGROUND_MUTED_OPACITY);
            self.foreground_subtle =
                format!("rgba(255, 255, 255, {:.2})", FOREGROUND_SUBTLE_OPACITY);
            self.foreground_disabled =
                format!("rgba(255, 255, 255, {:.2})", FOREGROUND_DISABLED_OPACITY);
        } else {
            self.foreground_primary = "#1a1a1a".to_string();
            self.foreground_muted = format!("rgba(0, 0, 0, {:.2})", FOREGROUND_MUTED_OPACITY);
            self.foreground_subtle = format!("rgba(0, 0, 0, {:.2})", FOREGROUND_SUBTLE_OPACITY);
            self.foreground_disabled = format!("rgba(0, 0, 0, {:.2})", FOREGROUND_DISABLED_OPACITY);
        }
    }

    fn compute_accent_derived(&mut self) {
        // Accent text matches system text direction:
        // - Light mode (dark system text) → dark accent text
        // - Dark mode (light system text) → light accent text
        let accent_text_color = if self.is_dark_mode {
            "#ffffff".to_string()
        } else {
            "#000000".to_string()
        };

        match &self.accent_source {
            AccentSource::Custom(color) => {
                self.accent_subtle = format!("color-mix(in srgb, {} 20%, transparent)", color);
                self.accent_text = accent_text_color;
            }
            AccentSource::Gtk => {
                // GTK accent - use @accent_color references
                // These will be overridden in css_vars_block() to reference GTK colors
                self.accent_subtle =
                    "color-mix(in srgb, @accent_color 20%, transparent)".to_string();
                self.accent_text = accent_text_color;
            }
            AccentSource::None => {
                // Monochrome mode - adapt to dark/light theme
                if self.is_dark_mode {
                    self.accent_subtle = "rgba(255, 255, 255, 0.08)".to_string();
                    self.accent_text = self.foreground_primary.clone();
                } else {
                    self.accent_subtle = "rgba(0, 0, 0, 0.06)".to_string();
                    self.accent_text = self.foreground_primary.clone();
                }
            }
        }
    }

    fn compute_overlays(&mut self) {
        let ((r, g, b), base_opacity) = if self.is_dark_mode {
            ((255u8, 255u8, 255u8), OVERLAY_OPACITY_DARK)
        } else {
            ((50u8, 50u8, 50u8), OVERLAY_OPACITY_LIGHT)
        };

        self.card_overlay = rgba_str(r, g, b, base_opacity);
        self.card_overlay_hover = rgba_str(r, g, b, base_opacity * HOVER_MULTIPLIER);
        self.card_overlay_subtle = rgba_str(r, g, b, base_opacity * SUBTLE_MULTIPLIER);
        self.card_overlay_strong = rgba_str(r, g, b, base_opacity * ACTIVE_MULTIPLIER);
        self.click_catcher_overlay = rgba_str(128, 128, 128, CLICK_CATCHER_OPACITY);
    }

    fn compute_borders_and_shadows(&mut self) {
        let shadow_opacity = if self.is_dark_mode {
            self.border_subtle = format!("rgba(255, 255, 255, {:.2})", BORDER_OPACITY_DARK);
            SHADOW_OPACITY_DARK
        } else {
            self.border_subtle = format!("rgba(0, 0, 0, {:.2})", BORDER_OPACITY_LIGHT);
            SHADOW_OPACITY_LIGHT
        };

        let tight_opacity = shadow_opacity * SHADOW_TIGHT_OPACITY_FACTOR;
        let diffuse_opacity = shadow_opacity * SHADOW_DIFFUSE_OPACITY_FACTOR;

        self.shadow_soft = format!(
            "0 {}px {}px rgba(0, 0, 0, {:.2}), 0 {}px {}px rgba(0, 0, 0, {:.2})",
            SHADOW_TIGHT_OFFSET_Y,
            SHADOW_TIGHT_BLUR,
            tight_opacity,
            SHADOW_DIFFUSE_OFFSET_Y,
            SHADOW_DIFFUSE_BLUR_SOFT,
            diffuse_opacity
        );

        self.shadow_strong = format!(
            "0 {}px {}px rgba(0, 0, 0, {:.2}), 0 {}px {}px rgba(0, 0, 0, {:.2})",
            SHADOW_TIGHT_OFFSET_Y,
            SHADOW_TIGHT_BLUR,
            tight_opacity,
            SHADOW_DIFFUSE_OFFSET_Y,
            SHADOW_DIFFUSE_BLUR_STRONG,
            diffuse_opacity
        );
    }

    fn compute_slider_tracks(&mut self) {
        if self.is_dark_mode {
            self.slider_track = format!("rgba(255, 255, 255, {:.2})", TRACK_OPACITY_DARK);
            self.slider_track_disabled =
                format!("rgba(255, 255, 255, {:.2})", TRACK_OPACITY_DARK * 0.6);
        } else {
            self.slider_track = format!("rgba(0, 0, 0, {:.2})", TRACK_OPACITY_LIGHT);
            self.slider_track_disabled = format!("rgba(0, 0, 0, {:.2})", TRACK_OPACITY_LIGHT * 0.6);
        }
    }

    fn compute_critical_backgrounds(&mut self) {
        // Row critical: 18% urgent blended over widget background
        self.row_critical_background =
            match blend_colors(&self.state_urgent, &self.widget_background, 0.18) {
                Some((r, g, b)) => rgba_str(r, g, b, 0.95),
                None => "rgba(255, 100, 100, 0.15)".to_string(),
            };

        // Toast critical: darker, more opaque
        let base = if self.is_dark_mode {
            "#1a1a1a"
        } else {
            "#f5f5f5"
        };

        self.toast_critical_background =
            match blend_colors(&self.state_urgent, base, TOAST_CRITICAL_URGENT_WEIGHT) {
                Some((r, g, b)) => rgba_str(r, g, b, 0.95),
                None => "rgba(40, 20, 20, 0.95)".to_string(),
            };
    }

    fn compute_sizes(&mut self) {
        let bar_size = self.bar_size;

        // Round to even numbers for proper pixel-perfect centering
        let bar_padding = round_to_even((bar_size as f64 * PADDING_SCALE) as u32);
        let widget_height = round_to_even(bar_size - 2 * bar_padding);

        // Bar radius: use rendered height (bar + padding on both sides)
        let bar_rendered_height = bar_size + 2 * bar_padding;
        let bar_max_radius = bar_rendered_height / 2;
        self.bar_border_radius =
            (bar_rendered_height * self.bar_radius_percent / 100).min(bar_max_radius);

        // Widget radius: percentage of bar height (widgets expand to fill bar height)
        let widget_max_radius = bar_size / 2;
        self.widget_border_radius =
            (bar_size * self.widget_radius_percent / 100).min(widget_max_radius);

        self.radius_pill = (self.widget_border_radius / 2).max(1);

        // Surface radius: larger for outer containers (popovers, menus)
        self.surface_border_radius = self.widget_border_radius;

        // Sizes - ensure vertical-related sizes are even for proper centering
        let internal_spacing = (bar_size as f64 * SPACING_SCALE) as u32;
        let font_size = round_to_even((widget_height as f64 * FONT_SCALE) as u32);
        let text_icon_size = round_to_even((bar_size as f64 * TEXT_ICON_SCALE) as u32);
        let pixmap_icon_size = round_to_even((bar_size as f64 * PIXMAP_ICON_SCALE) as u32);

        self.sizes = ThemeSizes {
            bar_height: bar_size,
            bar_padding,
            widget_height,
            widget_padding_x: (bar_size as f64 * PADDING_SCALE) as u32,
            // Vertical padding - fixed 2px for visual breathing room (already even)
            widget_padding_y: 2,
            font_size,
            text_icon_size,
            pixmap_icon_size,
            internal_spacing,
            // Widget content spacing: fixed values that work well visually
            // Edge padding provides breathing room at widget boundaries
            widget_content_edge: 6,
            // Gap between children (icon, label, etc.) - derived from internal_spacing
            widget_content_gap: (internal_spacing / 2).max(4) + 5,
        };
    }
}

impl Default for ThemePalette {
    fn default() -> Self {
        Self {
            is_dark_mode: true,
            is_gtk_mode: false,
            bar_background: DEFAULT_BAR_BG_DARK.to_string(),
            widget_background: DEFAULT_WIDGET_BG_DARK.to_string(),
            foreground_primary: "#ffffff".to_string(),
            foreground_muted: String::new(),
            foreground_subtle: String::new(),
            foreground_disabled: String::new(),
            accent_source: AccentSource::Gtk, // Default to GTK accent
            accent_primary: "@accent_color".to_string(),
            accent_subtle: String::new(),
            accent_text: String::new(),
            state_success: DEFAULT_STATE_SUCCESS.to_string(),
            state_warning: DEFAULT_STATE_WARNING.to_string(),
            state_urgent: DEFAULT_STATE_URGENT.to_string(),
            card_overlay: String::new(),
            card_overlay_hover: String::new(),
            card_overlay_subtle: String::new(),
            card_overlay_strong: String::new(),
            click_catcher_overlay: String::new(),
            border_subtle: String::new(),
            shadow_soft: String::new(),
            shadow_strong: String::new(),
            slider_track: String::new(),
            slider_track_disabled: String::new(),
            row_critical_background: String::new(),
            toast_critical_background: String::new(),
            font_family: DEFAULT_FONT_FAMILY.to_string(),
            bar_opacity: 0.0,
            widget_opacity: 1.0,
            bar_border_radius: 0,
            widget_border_radius: 0,
            surface_border_radius: 0,
            radius_pill: 0,
            sizes: ThemeSizes::default(),
            bar_radius_percent: 30,
            widget_radius_percent: 40,
            bar_size: 32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hex_color_valid() {
        assert_eq!(parse_hex_color("#ff0000"), Some((255, 0, 0)));
        assert_eq!(parse_hex_color("00ff00"), Some((0, 255, 0)));
        assert_eq!(parse_hex_color("#0000ff"), Some((0, 0, 255)));
        assert_eq!(parse_hex_color("#fff"), Some((255, 255, 255)));
        assert_eq!(parse_hex_color("000"), Some((0, 0, 0)));
    }

    #[test]
    fn test_parse_hex_color_invalid() {
        assert_eq!(parse_hex_color("not a color"), None);
        assert_eq!(parse_hex_color("#gggggg"), None);
        assert_eq!(parse_hex_color("#ff"), None);
    }

    #[test]
    fn test_relative_luminance() {
        // Black should be 0
        assert!((relative_luminance(0, 0, 0) - 0.0).abs() < 0.001);
        // White should be 1
        assert!((relative_luminance(255, 255, 255) - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_is_dark_color() {
        assert!(is_dark_color("#000000"));
        assert!(is_dark_color("#1a1a1f"));
        assert!(!is_dark_color("#ffffff"));
        assert!(!is_dark_color("#e8e8e8"));
    }

    #[test]
    fn test_blend_colors() {
        // 50/50 blend of black and white should be gray
        let result = blend_colors("#000000", "#ffffff", 0.5);
        assert!(result.is_some());
        let (r, g, b) = result.unwrap();
        assert!(r > 120 && r < 135);
        assert!(g > 120 && g < 135);
        assert!(b > 120 && b < 135);
    }

    #[test]
    fn test_rgba_str() {
        assert_eq!(rgba_str(255, 0, 0, 0.5), "rgba(255, 0, 0, 0.50)");
        assert_eq!(rgba_str(0, 255, 0, 1.0), "rgba(0, 255, 0, 1.00)");
    }

    #[test]
    fn test_rgb_to_hex() {
        assert_eq!(rgb_to_hex(255, 0, 0), "#ff0000");
        assert_eq!(rgb_to_hex(0, 255, 0), "#00ff00");
        assert_eq!(rgb_to_hex(0, 0, 255), "#0000ff");
    }

    #[test]
    fn test_theme_palette_default_is_dark() {
        let config = Config::default();
        let palette = ThemePalette::from_config(&config);
        assert!(palette.is_dark_mode);
    }

    #[test]
    fn test_theme_palette_light_mode() {
        let mut config = Config::default();
        config.theme.mode = "light".to_string();
        let palette = ThemePalette::from_config(&config);
        assert!(!palette.is_dark_mode);
        assert_eq!(palette.foreground_primary, "#1a1a1a");
    }

    #[test]
    fn test_theme_palette_css_vars_contains_expected_vars() {
        let config = Config::default();
        let palette = ThemePalette::from_config(&config);
        let css = palette.css_vars_block();

        assert!(css.contains("--color-background-bar:"));
        assert!(css.contains("--color-background-widget:"));
        assert!(css.contains("--color-foreground-primary:"));
        assert!(css.contains("--color-accent-primary:"));
        assert!(css.contains("--radius-bar:"));
        assert!(css.contains("--widget-height:"));
        assert!(css.contains("--font-family:"));
    }

    #[test]
    fn test_theme_sizes_computed_from_bar_size() {
        let mut config = Config::default();
        config.bar.size = 48;
        let palette = ThemePalette::from_config(&config);

        assert_eq!(palette.sizes.bar_height, 48);
        assert!(palette.sizes.widget_height > 0);
        assert!(palette.sizes.font_size > 0);
    }

    #[test]
    fn test_accent_default_is_custom() {
        // Default accent = "#adabe0" means use custom hex color
        let config = Config::default();
        let palette = ThemePalette::from_config(&config);

        assert_eq!(
            palette.accent_source,
            AccentSource::Custom("#adabe0".to_string())
        );
    }

    #[test]
    fn test_accent_custom_color() {
        // When accent is a hex color, use it as custom accent
        let mut config = Config::default();
        config.theme.accent = "#ff0000".to_string();

        let palette = ThemePalette::from_config(&config);

        assert_eq!(
            palette.accent_source,
            AccentSource::Custom("#ff0000".to_string())
        );
        assert_eq!(palette.accent_primary, "#ff0000");
        // CSS should output the custom color for accent-primary
        let css = palette.css_vars_block();
        assert!(css.contains("--color-accent-primary: #ff0000"));
    }

    #[test]
    fn test_accent_none_monochrome() {
        // When accent = "none", use monochrome mode
        let mut config = Config::default();
        config.theme.accent = "none".to_string();

        let palette = ThemePalette::from_config(&config);

        assert_eq!(palette.accent_source, AccentSource::None);
        // In dark mode, monochrome uses white-based colors
        assert!(palette.accent_primary.contains("rgba"));
    }

    #[test]
    fn test_accent_none_adapts_to_light_mode() {
        // Monochrome mode should use dark colors in light mode
        let mut config = Config::default();
        config.theme.mode = "light".to_string();
        config.theme.accent = "none".to_string();

        let palette = ThemePalette::from_config(&config);

        assert_eq!(palette.accent_source, AccentSource::None);
        // In light mode, monochrome uses black-based colors
        assert!(palette.accent_primary.contains("rgba(0, 0, 0"));
    }

    #[test]
    fn test_gtk_mode() {
        // When mode = "gtk", is_gtk_mode should be true
        let mut config = Config::default();
        config.theme.mode = "gtk".to_string();

        let palette = ThemePalette::from_config(&config);

        assert!(palette.is_gtk_mode);
        // Should default to dark for overlay calculations
        assert!(palette.is_dark_mode);
    }

    #[test]
    fn test_theme_sizes_scale_proportionally() {
        // Test that sizes scale up proportionally with bar size
        let mut config_small = Config::default();
        config_small.bar.size = 24;
        let palette_small = ThemePalette::from_config(&config_small);

        let mut config_large = Config::default();
        config_large.bar.size = 48;
        let palette_large = ThemePalette::from_config(&config_large);

        // Larger bar should have proportionally larger sizes
        assert!(palette_large.sizes.widget_height > palette_small.sizes.widget_height);
        assert!(palette_large.sizes.font_size > palette_small.sizes.font_size);
        assert!(palette_large.sizes.text_icon_size > palette_small.sizes.text_icon_size);
        assert!(palette_large.sizes.bar_padding > palette_small.sizes.bar_padding);
    }

    #[test]
    fn test_theme_sizes_widget_fits_in_bar() {
        // CSS gives .widget: min-height + padding (top/bottom) + margin (top/bottom)
        // Total vertical footprint = widget_height + 4 * widget_padding_y
        // Note: Very small bar sizes (< 30) may not accommodate widgets properly
        for bar_size in [36, 48, 60, 72] {
            let mut config = Config::default();
            config.bar.size = bar_size;
            let palette = ThemePalette::from_config(&config);

            // widget_height + 2*padding + 2*margin = widget_height + 4*widget_padding_y
            let total_widget_footprint =
                palette.sizes.widget_height + 4 * palette.sizes.widget_padding_y;
            assert!(
                total_widget_footprint <= bar_size,
                "Widget footprint {} (height={} + 4*padding_y={}) exceeds bar size {} for bar_size={}",
                total_widget_footprint,
                palette.sizes.widget_height,
                palette.sizes.widget_padding_y,
                bar_size,
                bar_size
            );
        }
    }

    #[test]
    fn test_theme_sizes_minimum_values() {
        // Even with small bar, sizes should have sensible minimums
        let mut config = Config::default();
        config.bar.size = 16; // Very small bar
        let palette = ThemePalette::from_config(&config);

        assert!(
            palette.sizes.widget_padding_y >= 1,
            "widget_padding_y should be at least 1"
        );
        assert!(
            palette.sizes.font_size >= 1,
            "font_size should be at least 1"
        );
    }

    #[test]
    fn test_border_radius_respects_max() {
        // Border radius should never exceed half the height (to avoid artifacts)
        for bar_size in [24, 36, 48] {
            let mut config = Config::default();
            config.bar.size = bar_size;
            config.bar.border_radius = 100; // Request maximum radius
            let palette = ThemePalette::from_config(&config);

            let max_possible_bar_radius = (bar_size + 2 * palette.sizes.bar_padding) / 2;
            assert!(
                palette.bar_border_radius <= max_possible_bar_radius,
                "Bar radius {} exceeds max {} for bar_size={}",
                palette.bar_border_radius,
                max_possible_bar_radius,
                bar_size
            );

            let widget_rendered_height =
                palette.sizes.widget_height + 2 * palette.sizes.widget_padding_y;
            let max_widget_radius = widget_rendered_height / 2;
            assert!(
                palette.widget_border_radius <= max_widget_radius,
                "Widget radius {} exceeds max {} for bar_size={}",
                palette.widget_border_radius,
                max_widget_radius,
                bar_size
            );
        }
    }
}
