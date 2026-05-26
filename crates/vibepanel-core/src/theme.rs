//! Unified theming system for vibepanel.
//!
//! `ThemePalette` is the single source of truth for all theme-related values.
//! It parses config, computes derived values, and generates CSS variables.

use material_colors::color::Argb;
use material_colors::scheme::Scheme;
use tracing::warn;

use crate::Config;
use crate::config::{BarPosition, SchemePolarity, ThemeStates};

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
pub const BORDER_OPACITY_DARK: f64 = 0.10;
pub const BORDER_OPACITY_LIGHT: f64 = 0.12;
// GTK mode: average of dark/light since we can't know the theme at build time
pub const BORDER_OPACITY_GTK: f64 = 0.11;

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

// Foreground opacity factors for text hierarchy
const FOREGROUND_MUTED_OPACITY: f64 = 0.6;
const FOREGROUND_DISABLED_OPACITY: f64 = 0.4;
const FOREGROUND_FAINT_OPACITY: f64 = 0.3;

/// Perceptual dark/light boundary in WCAG linear relative luminance.
///
/// CIELAB L*=50 (perceptual midpoint) ⇒ Y = ((50+16)/116)³ ≈ 0.184187.
/// Aligned with Material You's HCT tone axis (CIELAB-derived), which the
/// rest of the theme pipeline already uses.
const PERCEPTUAL_LIGHT_DARK_THRESHOLD: f64 = 0.184_187;

// Default colors (based on typical dark/light theme surface colors)
const DEFAULT_BAR_BG_DARK: &str = "#1a1a1f";
const DEFAULT_BAR_BG_LIGHT: &str = "#e8e8e8";
const DEFAULT_WIDGET_BG_DARK: &str = "#111217";
const DEFAULT_WIDGET_BG_LIGHT: &str = "#ffffff";
const DEFAULT_STATE_SUCCESS: &str = "#4a7a4a";
const DEFAULT_STATE_WARNING: &str = "#e5c07b";
const DEFAULT_STATE_URGENT: &str = "#ff6b6b";
const DEFAULT_FONT_FAMILY: &str = "monospace";

const WIDGET_HOVER_BG_VALUE: &str = "color-mix(in srgb, color-mix(in srgb, var(--widget-background-color) var(--widget-background-opacity), transparent) 92%, var(--widget-hover-tint))";

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

/// Resolve an outline color value to a CSS color expression.
///
/// Symbolic names map to existing theme tokens (`subtle` → `--color-border-subtle`,
/// `accent` → `--color-accent-primary`, `foreground` → `--color-foreground-primary`)
/// so the outline tracks the active theme polarity automatically. Hex colors
/// (`#rgb` / `#rrggbb`) are normalized to the lowercase 6-character form.
///
/// Unrecognized values fall through unchanged — validation runs at config-load
/// time, so unrecognized values shouldn't reach this function in practice.
pub fn resolve_outline_color(value: &str) -> String {
    match value {
        "subtle" => "var(--color-border-subtle)".to_string(),
        "accent" => "var(--color-accent-primary)".to_string(),
        "foreground" => "var(--color-foreground-primary)".to_string(),
        hex if hex.starts_with('#') => match parse_hex_color(hex) {
            Some((r, g, b)) => format!("#{:02x}{:02x}{:02x}", r, g, b),
            None => hex.to_string(),
        },
        other => other.to_string(),
    }
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

/// Resolve the effective scheme polarity for `mode = "auto"`.
///
/// Returns `true` if light mode should be used. When `scheme` is `None`, derives
/// polarity from average wallpaper luminance using the perceptual midpoint
/// threshold (`PERCEPTUAL_LIGHT_DARK_THRESHOLD` ≈ CIELAB L*=50):
/// `>= threshold` → light, `< threshold` → dark.
pub(crate) fn effective_scheme(scheme: Option<SchemePolarity>, luminance: Option<f64>) -> bool {
    match scheme {
        Some(SchemePolarity::Light) => true,
        Some(SchemePolarity::Dark) => false,
        Some(SchemePolarity::Gtk) | None => luminance
            .map(|l| l >= PERCEPTUAL_LIGHT_DARK_THRESHOLD)
            .unwrap_or(false),
    }
}

/// Return true if the color is considered dark (low luminance).
///
/// Uses the perceptual light/dark midpoint (`PERCEPTUAL_LIGHT_DARK_THRESHOLD`).
/// Returns `true` for unparseable color strings (conservative fallback — the
/// caller treats the color as dark and reacts accordingly).
pub fn is_dark_color(color: &str) -> bool {
    is_dark_color_with_threshold(color, PERCEPTUAL_LIGHT_DARK_THRESHOLD)
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

/// Format layered shadow CSS values (tight + diffuse) for a given color and opacity.
///
/// Returns `(shadow_soft, shadow_strong)` using the shared shadow geometry constants.
fn format_shadows(r: u8, g: u8, b: u8, shadow_opacity: f64) -> (String, String) {
    let tight_opacity = shadow_opacity * SHADOW_TIGHT_OPACITY_FACTOR;
    let diffuse_opacity = shadow_opacity * SHADOW_DIFFUSE_OPACITY_FACTOR;

    let soft = format!(
        "0 {}px {}px rgba({}, {}, {}, {:.2}), 0 {}px {}px rgba({}, {}, {}, {:.2})",
        SHADOW_TIGHT_OFFSET_Y,
        SHADOW_TIGHT_BLUR,
        r,
        g,
        b,
        tight_opacity,
        SHADOW_DIFFUSE_OFFSET_Y,
        SHADOW_DIFFUSE_BLUR_SOFT,
        r,
        g,
        b,
        diffuse_opacity
    );
    let strong = format!(
        "0 {}px {}px rgba({}, {}, {}, {:.2}), 0 {}px {}px rgba({}, {}, {}, {:.2})",
        SHADOW_TIGHT_OFFSET_Y,
        SHADOW_TIGHT_BLUR,
        r,
        g,
        b,
        tight_opacity,
        SHADOW_DIFFUSE_OFFSET_Y,
        SHADOW_DIFFUSE_BLUR_STRONG,
        r,
        g,
        b,
        diffuse_opacity
    );

    (soft, strong)
}

/// Build a `color-mix()` expression that blends `@window_fg_color` at the given
/// percentage with `transparent`.  Used throughout GTK mode to derive
/// foreground-based colors that adapt to the active GTK theme at runtime.
fn gtk_fg_mix(pct: f64) -> String {
    format!("color-mix(in srgb, @window_fg_color {pct:.1}%, transparent)")
}

/// Computed sizes based on bar height.
#[derive(Debug, Clone)]
pub struct ThemeSizes {
    pub bar_height: u32,
    pub widget_height: u32,
    pub widget_padding_y: u32,
    pub font_size: u32,
    pub text_icon_size: u32,
    pub pixmap_icon_size: u32,
    pub internal_spacing: u32,
    /// Roomy content padding inside widgets. User CSS can tune this via the
    /// additive `--widget-content-padding-offset` value.
    pub widget_content_padding: u32,
    /// Roomy gap between children inside widget content. User CSS can tune this
    /// via the additive `--widget-content-gap-offset` value.
    pub widget_content_gap: u32,
    /// Compact vertical content padding for label-bearing widgets. Vertical bars
    /// put rounded widget ends above and below content instead of beside it;
    /// text/icon advances are not square, so content padding needs separate
    /// tuning from inter-child gaps.
    pub widget_content_padding_vertical: u32,
    /// Compact vertical child gap for label-bearing widgets. Stacked children
    /// expose Pango line-height slack differently than rounded content padding,
    /// so this intentionally differs from `widget_content_padding_vertical`.
    pub widget_content_gap_vertical: u32,
}

impl Default for ThemeSizes {
    fn default() -> Self {
        Self {
            bar_height: 36,
            widget_height: 26,
            widget_padding_y: 2,
            font_size: 14,
            text_icon_size: 16,
            pixmap_icon_size: 15,
            internal_spacing: 9,
            widget_content_padding: 10,
            widget_content_gap: 10,
            widget_content_padding_vertical: 7,
            widget_content_gap_vertical: 7,
        }
    }
}

impl ThemeSizes {
    pub fn content_gap_for_orientation(&self, vertical: bool) -> u32 {
        if vertical {
            self.widget_content_gap_vertical
        } else {
            self.widget_content_gap
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
    pub shadows_enabled: bool,
}

/// Single source of truth for all theme values.
///
/// Constructed via `ThemePalette::from_config(&config, None, None)`.
#[derive(Debug, Clone)]
pub struct ThemePalette {
    // Mode
    pub is_dark_mode: bool,
    /// Whether mode is "gtk" (derive colors from GTK theme).
    pub is_gtk_mode: bool,
    /// Whether wallpaper-adaptive theming is active (full Material You scheme).
    pub is_wallpaper_mode: bool,

    // Background colors
    pub bar_background: String,
    pub widget_background: String,

    // Foreground colors
    pub foreground_primary: String,
    pub foreground_muted: String,
    pub foreground_disabled: String,
    pub foreground_faint: String,

    // Accent configuration
    pub accent_source: AccentSource,
    /// Primary accent color (only meaningful when accent_source is Custom).
    pub accent_primary: String,
    pub accent_subtle: String,
    pub accent_text: String,
    /// Pre-computed accent hover background (color-mix with luminance-aware tint and ratio).
    pub accent_hover_bg: String,
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
    /// Whether CSS box-shadows are enabled (from `theme.shadows` config).
    pub shadows_enabled: bool,

    // Slider tracks
    pub slider_track: String,
    pub slider_track_disabled: String,

    // Critical backgrounds
    pub row_critical_background: String,

    // Typography
    pub font_family: String,

    // Opacities
    pub bar_opacity: f64,
    pub widget_opacity: f64,
    pub popover_opacity: Option<f64>, // None = max(bar, widget) heuristic

    // Radii (pixels)
    pub bar_border_radius: u32,
    pub widget_border_radius: u32,
    pub surface_border_radius: u32,
    pub radius_pill: u32,

    // Sizes
    pub sizes: ThemeSizes,

    // Outline (theme-level inputs, resolved for CSS emission)
    /// Configured outline width in pixels (always present; per-scope visibility
    /// is gated separately via the `*_outline_enabled` flags).
    pub outline_width_px: u32,
    /// Resolved outline color as a CSS color expression (e.g.,
    /// `var(--color-border-subtle)` or `#aabbcc`).
    pub outline_color_resolved: String,
    /// Outline opacity as an integer percentage (0-100).
    pub outline_opacity_pct: u32,
    /// Effective outline state for the bar (theme default + `bar.outline` override).
    pub bar_outline_enabled: bool,
    /// Effective outline state for widgets/widget groups (theme default +
    /// `widgets.outline` override).
    pub widget_outline_enabled: bool,
    /// Effective outline state for surfaces (popovers, OSD, toasts, tray, media,
    /// Quick Settings). In v1 this just mirrors `theme.outline` — per-surface
    /// overrides may be added later.
    pub surface_outline_enabled: bool,

    // Internal: config values needed for computation
    bar_radius_percent: u32,
    widget_radius_percent: u32,
    bar_size: u32,
    bar_padding: u32,
    bar_position: BarPosition,
}

/// Convert a material-colors `Argb` value to a CSS hex color string.
fn argb_to_hex(argb: &Argb) -> String {
    format!("#{:02x}{:02x}{:02x}", argb.red, argb.green, argb.blue)
}

/// Convert a material-colors `Argb` value to an rgba string at the given opacity.
fn argb_to_rgba(argb: &Argb, alpha: f64) -> String {
    format!(
        "rgba({}, {}, {}, {:.2})",
        argb.red, argb.green, argb.blue, alpha
    )
}

impl ThemePalette {
    /// Create a ThemePalette from configuration.
    ///
    /// When `mode = "auto"`, pass the pre-extracted Material You theme from
    /// the wallpaper. The caller (app crate) handles wallpaper detection and
    /// image processing; core stays I/O-free.
    pub fn from_config(
        config: &Config,
        material_theme: Option<&material_colors::theme::Theme>,
        luminance: Option<f64>,
    ) -> Self {
        let mut palette = Self::default();
        palette.parse_config(config, material_theme, luminance);
        palette.compute_derived_values();
        palette
    }

    /// Create an optional popover palette with flipped polarity.
    ///
    /// When `theme.popover` is set (e.g., `popover = "light"` with `mode = "dark"`),
    /// this creates a second palette with the opposite polarity for popover surfaces.
    /// Returns `None` if `popover` is not configured, matches the current polarity,
    /// or cannot be applied (e.g., `mode = "gtk"`).
    ///
    /// In `auto` mode, uses the opposite Material You color scheme polarity
    /// so popovers get wallpaper-adaptive colors.
    pub fn popover_palette(
        config: &Config,
        material_theme: Option<&material_colors::theme::Theme>,
        luminance: Option<f64>,
    ) -> Option<Self> {
        let popover_mode = config.theme.popover.as_deref()?;

        // GTK mode: can't split runtime CSS variables by surface
        if config.theme.mode == "gtk" {
            return None;
        }

        // Check if the popover polarity actually differs from the bar's
        let bar_is_dark = match config.theme.mode.as_str() {
            "light" => false,
            "auto" => !effective_scheme(config.theme.scheme, luminance),
            _ => true,
        };
        if bar_is_dark == (popover_mode == "dark") {
            return None;
        }

        // Build a modified config with the popover's polarity
        let mut popover_config = config.clone();

        if config.theme.mode == "auto" {
            popover_config.theme.scheme = Some(if popover_mode == "light" {
                SchemePolarity::Light
            } else {
                SchemePolarity::Dark
            });
        } else {
            popover_config.theme.mode = popover_mode.to_string();
        }

        Some(Self::from_config(
            &popover_config,
            material_theme,
            luminance,
        ))
    }

    /// Generate CSS variable overrides scoped to `.popover`.
    ///
    /// Only emits polarity-dependent color variables (foregrounds, overlays,
    /// borders, shadows, slider tracks, critical backgrounds, accent derivatives,
    /// hover tint, and widget background). Size, spacing, radius, and typography
    /// variables are inherited from `:root` and not overridden.
    pub fn css_popover_vars_block(&self) -> String {
        let (accent_primary_css, accent_subtle_css) = match &self.accent_source {
            AccentSource::Gtk => (
                "@accent_color".to_string(),
                "color-mix(in srgb, @accent_color 20%, transparent)".to_string(),
            ),
            _ => (self.accent_primary.clone(), self.accent_subtle.clone()),
        };

        let hover_tint = if self.is_gtk_mode {
            "@window_fg_color"
        } else if self.is_dark_mode {
            "white"
        } else {
            "black"
        };

        format!(
            r#"
.popover {{
    /* ===== Popover polarity override ===== */
    --widget-background-color: {widget_bg};
    --widget-hover-tint: {hover_tint};

    /* Foreground */
    --color-foreground-primary: {fg_primary};
    --color-foreground-muted: {fg_muted};
    --color-foreground-disabled: {fg_disabled};
    --color-foreground-faint: {fg_faint};

    /* Accent */
    --color-accent-primary: {accent_primary};
    --color-accent-subtle: {accent_subtle};
    --color-accent-text: {accent_text};
    --color-accent-hover-bg: {accent_hover_bg};

    /* Card overlays */
    --color-card-overlay: {card_overlay};
    --color-card-overlay-hover: {card_overlay_hover};
    --color-card-overlay-subtle: {card_overlay_subtle};
    --color-card-overlay-strong: {card_overlay_strong};

    /* Borders & shadows */
    --color-border-subtle: {border_subtle};
    --shadow-soft: {shadow_soft};
    --shadow-strong: {shadow_strong};

    /* Slider tracks */
    --color-slider-track: {slider_track};
    --color-slider-track-disabled: {slider_track_disabled};

    /* Contextual backgrounds */
    --color-row-critical-background: {row_critical_bg};
}}
"#,
            widget_bg = self.widget_background,
            hover_tint = hover_tint,
            fg_primary = self.foreground_primary,
            fg_muted = self.foreground_muted,
            fg_disabled = self.foreground_disabled,
            fg_faint = self.foreground_faint,
            accent_primary = accent_primary_css,
            accent_subtle = accent_subtle_css,
            accent_text = self.accent_text,
            accent_hover_bg = self.accent_hover_bg,
            card_overlay = self.card_overlay,
            card_overlay_hover = self.card_overlay_hover,
            card_overlay_subtle = self.card_overlay_subtle,
            card_overlay_strong = self.card_overlay_strong,
            border_subtle = self.border_subtle,
            shadow_soft = self.shadow_soft,
            shadow_strong = self.shadow_strong,
            slider_track = self.slider_track,
            slider_track_disabled = self.slider_track_disabled,
            row_critical_bg = self.row_critical_background,
        )
    }

    /// Generate the :root CSS variable block.
    /// NOTE: polarity-dependent variables are also emitted in `css_popover_vars_block()`.
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

        let pad = self.bar_padding;
        let is_island = self.bar_opacity == 0.0;

        // Public bar padding hooks (`--bar-padding-y`, `--bar-padding-x`, and
        // `--bar-padding-{side}`) are intentionally left unset. They exist as
        // user override slots in bar CSS; generated defaults live in internal
        // applied-side vars so island-mode asymmetry does not leak into the API.
        let (bar_padding_top, bar_padding_right, bar_padding_bottom, bar_padding_left) =
            if is_island {
                match self.bar_position {
                    BarPosition::Top => (pad, 0, 0, 0),
                    BarPosition::Bottom => (0, 0, pad, 0),
                    BarPosition::Left => (0, 0, 0, pad),
                    BarPosition::Right => (0, pad, 0, 0),
                }
            } else if self.bar_position.is_vertical() {
                (0, pad, 0, pad)
            } else {
                (pad, 0, pad, 0)
            };

        format!(
            r#"
:root {{
    /* ===== Widget Styling (Base values, can be overridden per-widget) ===== */
    /* These are combined via color-mix() in widget/popover CSS rules.
     * --widget-background-opacity is a percentage with % suffix (e.g., "80%").
     * To override in user CSS, use: --widget-background-opacity: 50%; */
    --widget-background-color: {widget_bg_color};
    --widget-background-opacity: {widget_bg_opacity}%;
    --popover-background-opacity: {popover_bg_opacity}%;
    --widget-hover-tint: {widget_hover_tint};
    /* Semantic hover backgrounds for user CSS overrides. */
    --color-widget-hover-bg: {widget_hover_bg_value};
    --color-workspace-indicator-hover-default-bg: var(--color-card-overlay-hover);
    --color-workspace-indicator-active-hover-bg: var(--color-accent-hover-bg);
    --color-workspace-indicator-urgent-hover-bg: color-mix(in srgb, var(--color-state-urgent) 80%, var(--widget-hover-tint));
    --color-taskbar-button-hover-bg: color-mix(in srgb, transparent 92%, var(--widget-hover-tint));
    --color-taskbar-button-active-hover-bg: var(--color-accent-hover-bg);
    --color-taskbar-button-urgent-hover-bg: color-mix(in srgb, var(--color-state-urgent) 80%, var(--widget-hover-tint));

    /* ===== Background Colors ===== */
    /* Bar background with opacity applied via color-mix */
    --color-background-bar: {bar_bg_with_opacity};

    /* ===== Foreground Colors ===== */
    --color-foreground-primary: {fg_primary};
    --color-foreground-muted: {fg_muted};
    --color-foreground-disabled: {fg_disabled};
    --color-foreground-faint: {fg_faint};

    /* ===== Accent Colors ===== */
    --color-accent-primary: {accent_primary};
    --color-accent-subtle: {accent_subtle};
    /* Slider accent - alias for user CSS overrides */
    --color-accent-slider: var(--color-accent-primary);
    --color-accent-text: {accent_text};
    --color-accent-hover-bg: {accent_hover_bg};

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

    /* ===== Outlines =====
     * Base values are theme-level. Per-scope `--*-outline-width` resolves
     * to `var(--outline-width)` when the scope's outline is enabled, or
     * `0px` when disabled. Color and opacity are simple aliases in v1;
     * per-scope overrides can specialize them later without renaming.
     */
    --outline-width: {outline_width}px;
    --outline-color: {outline_color};
    --outline-opacity: {outline_opacity}%;
    --bar-outline-width: {bar_outline_width};
    --bar-outline-color: var(--outline-color);
    --bar-outline-opacity: var(--outline-opacity);
    --widget-outline-width: {widget_outline_width};
    --widget-outline-color: var(--outline-color);
    --widget-outline-opacity: var(--outline-opacity);
    --surface-outline-width: {surface_outline_width};
    --surface-outline-color: var(--outline-color);
    --surface-outline-opacity: var(--outline-opacity);

    /* ===== Slider Tracks ===== */
    --color-slider-track: {slider_track};
    --color-slider-track-disabled: {slider_track_disabled};

    /* ===== Contextual Backgrounds ===== */
    --color-row-background: var(--color-card-overlay-subtle);
    --color-row-background-hover: var(--color-card-overlay-hover);
    --color-row-critical-background: {row_critical_bg};

    /* ===== Radii ===== */
    --radius-bar: {radius_bar}px;
    --radius-surface: {radius_surface}px;
    --radius-widget: {radius_widget};
    --radius-widget-lg: calc({radius_widget} * 2);            /* Larger surfaces that scale with widget */
    --radius-pill: {radius_pill}px;
    --radius-card: {radius_card}px;                        /* Cards/containers - never goes pill */
    --radius-round: 9999px;                                /* Always circular */
    --radius-factor: {radius_factor};                      /* 0.0 at 0%, 1.0 at 50%+ for fixed-size elements */

    /* ===== Sizes & Spacing ===== */
    --bar-height: {bar_height}px;
    --vp-internal-bar-padding-top: {bar_padding_top}px;
    --vp-internal-bar-padding-right: {bar_padding_right}px;
    --vp-internal-bar-padding-bottom: {bar_padding_bottom}px;
    --vp-internal-bar-padding-left: {bar_padding_left}px;
    --widget-height: {widget_height}px;
    --widget-padding-y: {widget_padding_y}px;
    --spacing-internal: {internal_spacing}px;
    --vp-widget-content-padding-h: {widget_content_padding}px;
    --vp-widget-content-gap-h: {widget_content_gap}px;
    --vp-widget-content-padding-v: {widget_content_padding_vertical}px;
    --vp-widget-content-gap-v: {widget_content_gap_vertical}px;
    /* Public density offsets: user values are added to each widget's tuned base. */
    --widget-content-padding-offset: 0px;
    --widget-content-gap-offset: 0px;
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

    /* ===== Typography ===== */
    --font-family: {font_family};
    --font-scale: {font_scale};
    --font-size: calc(var(--widget-height) * var(--font-scale));
    --font-size-text-icon: {text_icon_size}px;

    /* Slider height - scales with widget height */
    --slider-height: calc(var(--widget-height) * 0.25);
    --slider-height-thick: calc(var(--widget-height) * 0.4);
    --slider-knob-size: calc(var(--widget-height) * 0.65);
    /* Slider radii - half of height, scaled by radius factor */
    --slider-radius: calc(var(--widget-height) * 0.125 * {radius_factor});
    --slider-radius-thick: calc(var(--widget-height) * 0.2 * {radius_factor});
    --slider-knob-radius: calc(var(--widget-height) * 0.325 * {radius_factor});

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
            widget_bg_color = self.widget_background,
            widget_hover_bg_value = WIDGET_HOVER_BG_VALUE,
            widget_bg_opacity = (self.widget_opacity * 100.0).round() as u32,
            popover_bg_opacity = match self.popover_opacity {
                Some(explicit) => (explicit * 100.0).round() as u32,
                None => (self.bar_opacity.max(self.widget_opacity) * 100.0).round() as u32,
            },
            widget_hover_tint = if self.is_gtk_mode {
                "@window_fg_color"
            } else if self.is_dark_mode {
                "white"
            } else {
                "black"
            },
            fg_primary = self.foreground_primary,
            fg_muted = self.foreground_muted,
            fg_disabled = self.foreground_disabled,
            fg_faint = self.foreground_faint,
            accent_primary = accent_primary_css,
            accent_subtle = accent_subtle_css,
            accent_text = self.accent_text,
            accent_hover_bg = self.accent_hover_bg,
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
            outline_width = self.outline_width_px,
            outline_color = self.outline_color_resolved,
            outline_opacity = self.outline_opacity_pct,
            bar_outline_width = if self.bar_outline_enabled {
                "var(--outline-width)"
            } else {
                "0px"
            },
            widget_outline_width = if self.widget_outline_enabled {
                "var(--outline-width)"
            } else {
                "0px"
            },
            surface_outline_width = if self.surface_outline_enabled {
                "var(--outline-width)"
            } else {
                "0px"
            },
            slider_track = self.slider_track,
            slider_track_disabled = self.slider_track_disabled,
            row_critical_bg = self.row_critical_background,
            radius_bar = self.bar_border_radius,
            radius_surface = self.surface_border_radius,
            radius_widget = if self.widget_radius_percent >= 50 {
                "9999px".to_string()
            } else {
                format!("{}px", self.widget_border_radius)
            },
            radius_card = self.widget_border_radius,
            radius_pill = self.radius_pill,
            radius_factor = (self.widget_radius_percent as f64 / 50.0).min(1.0),
            bar_height = self.sizes.bar_height,
            bar_padding_top = bar_padding_top,
            bar_padding_right = bar_padding_right,
            bar_padding_bottom = bar_padding_bottom,
            bar_padding_left = bar_padding_left,
            widget_height = self.sizes.widget_height,
            widget_padding_y = self.sizes.widget_padding_y,
            internal_spacing = self.sizes.internal_spacing,
            widget_content_padding = self.sizes.widget_content_padding,
            widget_content_gap = self.sizes.widget_content_gap,
            widget_content_padding_vertical = self.sizes.widget_content_padding_vertical,
            widget_content_gap_vertical = self.sizes.widget_content_gap_vertical,
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

    /// Get surface styling for popovers and menus.
    pub fn surface_styles(&self) -> SurfaceStyles {
        SurfaceStyles {
            background_color: self.widget_background.clone(),
            text_color: self.foreground_primary.clone(),
            font_family: self.font_family.clone(),
            font_size: self.sizes.font_size,
            shadows_enabled: self.shadows_enabled,
        }
    }

    /// Generate per-widget CSS overrides from `[widgets.xxx]` config sections.
    ///
    /// Generates rules like `.widget.clock, .clock-popover { --widget-background-color: #f5c2e7; }`.
    /// Widget names are normalized to CSS conventions (underscores → hyphens).
    ///
    /// Per-widget `outline_color` is emitted as both `--widget-outline-color`
    /// (for the in-bar widget body) and `--surface-outline-color` (for the
    /// matching `.<name>-popover` surface, since GTK4 popovers render in
    /// separate windows and a CSS-variable scope on `.widget.<name>` would
    /// not propagate to them).
    pub fn generate_per_widget_css(config: &Config) -> String {
        let mut css = String::new();

        for (widget_name, options) in &config.widgets.widget_configs {
            let mut rules = Vec::new();

            if let Some(ref color) = options.background_color {
                if let Some((r, g, b)) = parse_hex_color(color) {
                    let normalized = format!("#{:02x}{:02x}{:02x}", r, g, b);
                    rules.push(format!("--widget-background-color: {};", normalized));
                    rules.push(format!("--color-widget-hover-bg: {WIDGET_HOVER_BG_VALUE};"));
                } else {
                    tracing::warn!(
                        "Invalid background_color '{}' for widget '{}' - expected hex color",
                        color,
                        widget_name
                    );
                }
            }

            if let Some(ref color) = options.outline_color {
                let resolved = resolve_outline_color(color);
                rules.push(format!("--widget-outline-color: {};", resolved));
                rules.push(format!("--surface-outline-color: {};", resolved));
            }

            if !rules.is_empty() {
                let rules_str = rules.join("\n    ");
                let css_name = widget_name.replace('_', "-");
                css.push_str(&format!(
                    r#"
.widget.{css_name},
.widget-item.{css_name},
.widget-merge-group.{css_name},
.{css_name}-popover {{
    {rules}
}}
"#,
                    css_name = css_name,
                    rules = rules_str
                ));
            }
        }

        css
    }

    fn parse_config(
        &mut self,
        config: &Config,
        material_theme: Option<&material_colors::theme::Theme>,
        luminance: Option<f64>,
    ) {
        // Check if GTK mode is requested
        self.is_gtk_mode = config.theme.mode == "gtk";

        // Common setup — these are the same in every mode
        self.bar_opacity = config.bar.background_opacity;
        self.widget_opacity = config.widgets.background_opacity;
        self.popover_opacity = config.widgets.popover_background_opacity;
        self.shadows_enabled = config.theme.shadows;
        self.state_success = config.theme.states.success.clone();
        self.state_warning = config.theme.states.warning.clone();
        self.state_urgent = config.theme.states.urgent.clone();

        // Outline configuration. Per-scope `Option<bool>` overrides inherit
        // from `theme.outline` when omitted. Width and opacity are theme-level
        // only in v1; effective visibility on a surface is `enabled && width > 0`.
        //
        // Bar outline in islands mode (`bar.background_opacity == 0.0`):
        // when the user has not set an explicit `bar.outline`, suppress the
        // outline so we don't draw a 1px rectangle around an invisible bar
        // (an "outline framing nothing" surrounding the floating widgets).
        // An explicit `bar.outline = true` still wins — users who want that
        // framing look can opt in.
        self.outline_width_px = config.theme.outline_width;
        self.outline_color_resolved = resolve_outline_color(&config.theme.outline_color);
        self.outline_opacity_pct = (config.theme.outline_opacity * 100.0).round() as u32;
        self.bar_outline_enabled = config
            .bar
            .outline
            .unwrap_or(config.theme.outline && config.bar.background_opacity > 0.0);
        self.widget_outline_enabled = config.widgets.outline.unwrap_or(config.theme.outline);
        self.surface_outline_enabled = config.theme.outline;

        // Handle auto mode (wallpaper-adaptive Material You theming) first,
        // since it sets backgrounds, foregrounds, accent, and state colors directly.
        if config.theme.mode == "auto" {
            if let Some(theme) = material_theme {
                let is_light = effective_scheme(config.theme.scheme, luminance);
                let scheme = if is_light {
                    &theme.schemes.light
                } else {
                    &theme.schemes.dark
                };
                self.is_dark_mode = !is_light;
                self.is_wallpaper_mode = true;

                // Set defaults before apply_wallpaper_theme (which may override backgrounds)
                self.bar_background = if is_light {
                    DEFAULT_BAR_BG_LIGHT.to_string()
                } else {
                    DEFAULT_BAR_BG_DARK.to_string()
                };
                self.widget_background = if is_light {
                    DEFAULT_WIDGET_BG_LIGHT.to_string()
                } else {
                    DEFAULT_WIDGET_BG_DARK.to_string()
                };

                // Apply full Material You palette
                self.apply_wallpaper_theme(scheme, config);
            } else {
                // No material theme available (extraction failed or was skipped).
                // Still honour config.theme.scheme and derived luminance so that an
                // explicit `scheme = "light"` or a bright wallpaper that failed
                // colour-extraction doesn't silently fall back to dark mode.
                let is_light = effective_scheme(config.theme.scheme, luminance);
                warn!(
                    "Auto mode: no wallpaper theme provided, falling back to {} defaults",
                    if is_light { "light" } else { "dark" }
                );
                self.is_dark_mode = !is_light;
                self.bar_background = config.bar.background_color.clone().unwrap_or_else(|| {
                    if is_light {
                        DEFAULT_BAR_BG_LIGHT.to_string()
                    } else {
                        DEFAULT_BAR_BG_DARK.to_string()
                    }
                });
                self.widget_background =
                    config.widgets.background_color.clone().unwrap_or_else(|| {
                        if is_light {
                            DEFAULT_WIDGET_BG_LIGHT.to_string()
                        } else {
                            DEFAULT_WIDGET_BG_DARK.to_string()
                        }
                    });

                self.accent_source = AccentSource::Custom("#adabe0".to_string());
                self.accent_primary = "#adabe0".to_string();
            }
        } else {
            // Non-auto modes: dark, light, gtk

            // Determine which default backgrounds to use based on explicit mode
            // For "gtk" mode, we reference GTK CSS variables instead of hardcoded colors
            let (default_bar_bg, default_widget_bg) = if self.is_gtk_mode {
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

            self.bar_background = config
                .bar
                .background_color
                .clone()
                .unwrap_or(default_bar_bg);

            self.widget_background = config
                .widgets
                .background_color
                .clone()
                .unwrap_or(default_widget_bg);

            // Resolve is_dark_mode
            // For GTK mode, we assume dark for overlay calculations since we can't query GTK's actual colors at build time
            self.is_dark_mode = match config.theme.mode.as_str() {
                "light" => false,
                "gtk" => true, // Default to dark for overlays/borders; GTK handles actual background colors
                _ => true,     // "dark" and any unknown mode
            };

            // Parse accent configuration from the `theme.accent` field.
            // Smart default: if mode is "gtk" and accent is not specified, default to "gtk".
            let accent_str = config.theme.accent.as_deref().unwrap_or_else(|| {
                if config.theme.mode == "gtk" {
                    "gtk"
                } else {
                    "#adabe0"
                }
            });
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
                    // Monochrome mode - match foreground color
                    if self.is_gtk_mode {
                        self.accent_primary = "@window_fg_color".to_string();
                    } else if self.is_dark_mode {
                        self.accent_primary = "#ffffff".to_string();
                    } else {
                        self.accent_primary = "#1a1a1a".to_string();
                    }
                }
                AccentSource::Gtk => {
                    self.accent_primary = "@accent_color".to_string();
                }
            }
        }

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
        self.bar_padding = config.bar.padding;
        self.bar_position = config.bar.position();
    }

    fn compute_derived_values(&mut self) {
        if !self.is_wallpaper_mode {
            // Normal mode: compute all derived color values from config inputs.
            // In wallpaper mode, apply_wallpaper_theme() already set all colors directly.
            self.compute_foreground_colors();
            self.compute_accent_derived();
            self.compute_overlays();
            self.compute_borders_and_shadows();
            self.compute_slider_tracks();
            self.compute_critical_backgrounds();
        }
        self.compute_sizes();
    }

    fn compute_foreground_colors(&mut self) {
        if self.is_gtk_mode {
            // GTK mode: reference the theme's foreground color so it adapts to
            // both light and dark GTK themes at runtime.
            self.foreground_primary = "@window_fg_color".to_string();
            self.foreground_muted = gtk_fg_mix(FOREGROUND_MUTED_OPACITY * 100.0);
            self.foreground_disabled = gtk_fg_mix(FOREGROUND_DISABLED_OPACITY * 100.0);
            self.foreground_faint = gtk_fg_mix(FOREGROUND_FAINT_OPACITY * 100.0);
        } else if self.is_dark_mode {
            self.foreground_primary = "#ffffff".to_string();
            self.foreground_muted = format!("rgba(255, 255, 255, {:.2})", FOREGROUND_MUTED_OPACITY);
            self.foreground_disabled =
                format!("rgba(255, 255, 255, {:.2})", FOREGROUND_DISABLED_OPACITY);
            self.foreground_faint = format!("rgba(255, 255, 255, {:.2})", FOREGROUND_FAINT_OPACITY);
        } else {
            self.foreground_primary = "#1a1a1a".to_string();
            self.foreground_muted = format!("rgba(0, 0, 0, {:.2})", FOREGROUND_MUTED_OPACITY);
            self.foreground_disabled = format!("rgba(0, 0, 0, {:.2})", FOREGROUND_DISABLED_OPACITY);
            self.foreground_faint = format!("rgba(0, 0, 0, {:.2})", FOREGROUND_FAINT_OPACITY);
        }
    }

    fn compute_accent_derived(&mut self) {
        // Accent text matches system text direction:
        // - GTK mode → use theme's foreground (adapts at runtime)
        // - Light mode (dark system text) → dark accent text
        // - Dark mode (light system text) → light accent text
        let accent_text_color = if self.is_gtk_mode {
            "@window_fg_color".to_string()
        } else if self.is_dark_mode {
            "#ffffff".to_string()
        } else {
            "#000000".to_string()
        };

        match &self.accent_source {
            AccentSource::Custom(color) => {
                self.accent_subtle = format!("color-mix(in srgb, {} 20%, transparent)", color);
                self.accent_text = accent_text_color;
                // Bright accent → darken on hover (80/20); dark accent → lighten (90/10, subtler)
                self.accent_hover_bg = if is_dark_color(color) {
                    format!("color-mix(in srgb, {} 90%, white)", color)
                } else {
                    format!("color-mix(in srgb, {} 80%, black)", color)
                };
            }
            AccentSource::Gtk => {
                // GTK accent - use @accent_color references
                self.accent_subtle =
                    "color-mix(in srgb, @accent_color 20%, transparent)".to_string();
                self.accent_text = accent_text_color;
                // GTK accents are almost always bright → darken on hover
                self.accent_hover_bg = "color-mix(in srgb, @accent_color 80%, black)".to_string();
            }
            AccentSource::None => {
                // Monochrome mode - adapt to theme
                if self.is_gtk_mode {
                    self.accent_subtle = gtk_fg_mix(8.0);
                    self.accent_text = self.foreground_primary.clone();
                    self.accent_hover_bg = format!(
                        "color-mix(in srgb, {} 90%, @window_fg_color)",
                        self.accent_primary
                    );
                } else if self.is_dark_mode {
                    self.accent_subtle = "rgba(255, 255, 255, 0.08)".to_string();
                    self.accent_text = self.foreground_primary.clone();
                    self.accent_hover_bg =
                        format!("color-mix(in srgb, {} 90%, white)", self.accent_primary);
                } else {
                    self.accent_subtle = "rgba(0, 0, 0, 0.06)".to_string();
                    self.accent_text = self.foreground_primary.clone();
                    self.accent_hover_bg =
                        format!("color-mix(in srgb, {} 80%, black)", self.accent_primary);
                }
            }
        }
    }

    fn compute_overlays(&mut self) {
        if self.is_gtk_mode {
            // GTK mode: use the theme's foreground color for overlays so they
            // adapt to both light and dark themes.  We use OVERLAY_OPACITY_DARK
            // percentages as the base (same as the dark-mode default that was
            // previously hardcoded for GTK mode).
            let base = OVERLAY_OPACITY_DARK * 100.0;
            self.card_overlay = gtk_fg_mix(base);
            self.card_overlay_hover = gtk_fg_mix(base * HOVER_MULTIPLIER);
            self.card_overlay_subtle = gtk_fg_mix(base * SUBTLE_MULTIPLIER);
            self.card_overlay_strong = gtk_fg_mix(base * ACTIVE_MULTIPLIER);
            self.click_catcher_overlay = rgba_str(128, 128, 128, CLICK_CATCHER_OPACITY);
            return;
        }

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
        // In GTK mode, derive border color from the theme's foreground.
        // Shadows always use black (shadows are naturally dark regardless of theme).
        if !self.shadows_enabled {
            if self.is_gtk_mode {
                self.border_subtle = gtk_fg_mix(BORDER_OPACITY_GTK * 100.0);
            } else {
                let border_opacity = if self.is_dark_mode {
                    BORDER_OPACITY_DARK
                } else {
                    BORDER_OPACITY_LIGHT
                };
                self.border_subtle = if self.is_dark_mode {
                    format!("rgba(255, 255, 255, {:.2})", border_opacity)
                } else {
                    format!("rgba(0, 0, 0, {:.2})", border_opacity)
                };
            }
            self.shadow_soft = "none".to_string();
            self.shadow_strong = "none".to_string();
            return;
        }

        if self.is_gtk_mode {
            self.border_subtle = gtk_fg_mix(BORDER_OPACITY_GTK * 100.0);
        } else if self.is_dark_mode {
            self.border_subtle = format!("rgba(255, 255, 255, {:.2})", BORDER_OPACITY_DARK);
        } else {
            self.border_subtle = format!("rgba(0, 0, 0, {:.2})", BORDER_OPACITY_LIGHT);
        }

        let shadow_opacity = if self.is_dark_mode {
            SHADOW_OPACITY_DARK
        } else {
            SHADOW_OPACITY_LIGHT
        };

        (self.shadow_soft, self.shadow_strong) = format_shadows(0, 0, 0, shadow_opacity);
    }

    fn compute_slider_tracks(&mut self) {
        if self.is_gtk_mode {
            self.slider_track = gtk_fg_mix(TRACK_OPACITY_DARK * 100.0);
            self.slider_track_disabled = gtk_fg_mix(TRACK_OPACITY_DARK * 0.6 * 100.0);
        } else if self.is_dark_mode {
            self.slider_track = format!("rgba(255, 255, 255, {:.2})", TRACK_OPACITY_DARK);
            self.slider_track_disabled =
                format!("rgba(255, 255, 255, {:.2})", TRACK_OPACITY_DARK * 0.6);
        } else {
            self.slider_track = format!("rgba(0, 0, 0, {:.2})", TRACK_OPACITY_LIGHT);
            self.slider_track_disabled = format!("rgba(0, 0, 0, {:.2})", TRACK_OPACITY_LIGHT * 0.6);
        }
    }

    fn compute_critical_backgrounds(&mut self) {
        if self.is_gtk_mode {
            // GTK mode: blend via CSS color-mix since we can't parse GTK named
            // colors at build time.
            self.row_critical_background = format!(
                "color-mix(in srgb, {} 18%, @view_bg_color)",
                self.state_urgent
            );
            return;
        }

        // Row critical: 18% urgent blended over widget background
        self.row_critical_background =
            match blend_colors(&self.state_urgent, &self.widget_background, 0.18) {
                Some((r, g, b)) => rgba_str(r, g, b, 0.95),
                None => "rgba(255, 100, 100, 0.15)".to_string(),
            };
    }

    /// Apply a full Material You color scheme from a wallpaper-extracted theme.
    ///
    /// This sets all color fields on the palette directly from the Material You scheme,
    /// bypassing the normal `compute_*` methods for colors. Non-color fields (opacities,
    /// radii, sizes, font, shadows_enabled) are left untouched.
    ///
    /// If the user explicitly set `bar.background_color` or `widgets.background_color`
    /// in their config, those values are preserved (not overridden by the Material You palette).
    fn apply_wallpaper_theme(&mut self, scheme: &Scheme, config: &Config) {
        // Backgrounds — only override if user didn't explicitly set them
        if config.bar.background_color.is_none() {
            self.bar_background = argb_to_hex(&scheme.surface_container);
        }
        if config.widgets.background_color.is_none() {
            self.widget_background = argb_to_hex(&scheme.surface_container_low);
        }

        // Foregrounds
        self.foreground_primary = argb_to_hex(&scheme.on_surface);
        self.foreground_muted = argb_to_hex(&scheme.on_surface_variant);
        self.foreground_disabled = argb_to_rgba(&scheme.on_surface, FOREGROUND_DISABLED_OPACITY);
        self.foreground_faint = argb_to_rgba(&scheme.on_surface, FOREGROUND_FAINT_OPACITY);

        // Accent — only override if user didn't explicitly set one
        if let Some(ref accent) = config.theme.accent {
            self.accent_source = match accent.as_str() {
                "gtk" => AccentSource::Gtk,
                "none" => AccentSource::None,
                color => AccentSource::Custom(color.to_string()),
            };
            match &self.accent_source {
                AccentSource::Custom(color) => {
                    self.accent_primary = color.clone();
                }
                AccentSource::None => {
                    self.accent_primary = self.foreground_primary.clone();
                }
                AccentSource::Gtk => {
                    self.accent_primary = "@accent_color".to_string();
                }
            }
            self.compute_accent_derived();
        } else {
            self.accent_source = AccentSource::Custom(argb_to_hex(&scheme.primary));
            self.accent_primary = argb_to_hex(&scheme.primary);
            self.accent_subtle = argb_to_rgba(&scheme.primary, 0.20);
            self.accent_text = argb_to_hex(&scheme.on_primary);
            self.accent_hover_bg = argb_to_hex(&scheme.primary_container);
        }

        // State colors: use Material You error unless the user customized urgent.
        if config.theme.states.urgent == ThemeStates::default().urgent {
            self.state_urgent = argb_to_hex(&scheme.error);
        }

        // Overlays — use surface_tint at varying opacities
        let base_opacity = if self.is_dark_mode {
            OVERLAY_OPACITY_DARK
        } else {
            OVERLAY_OPACITY_LIGHT
        };
        self.card_overlay = argb_to_rgba(&scheme.surface_tint, base_opacity);
        self.card_overlay_hover =
            argb_to_rgba(&scheme.surface_tint, base_opacity * HOVER_MULTIPLIER);
        self.card_overlay_subtle =
            argb_to_rgba(&scheme.surface_tint, base_opacity * SUBTLE_MULTIPLIER);
        self.card_overlay_strong =
            argb_to_rgba(&scheme.surface_tint, base_opacity * ACTIVE_MULTIPLIER);
        self.click_catcher_overlay = rgba_str(128, 128, 128, CLICK_CATCHER_OPACITY);

        // Borders
        self.border_subtle = argb_to_hex(&scheme.outline_variant);

        // Shadows — use the scheme's shadow color with existing shadow structure
        let shadow_opacity = if self.is_dark_mode {
            SHADOW_OPACITY_DARK
        } else {
            SHADOW_OPACITY_LIGHT
        };

        if self.shadows_enabled {
            (self.shadow_soft, self.shadow_strong) = format_shadows(
                scheme.shadow.red,
                scheme.shadow.green,
                scheme.shadow.blue,
                shadow_opacity,
            );
        } else {
            self.shadow_soft = "none".to_string();
            self.shadow_strong = "none".to_string();
        }

        // Slider tracks
        self.slider_track = argb_to_hex(&scheme.surface_container_highest);
        self.slider_track_disabled = argb_to_rgba(&scheme.surface_container_highest, 0.6);

        // Critical backgrounds
        let error_hex = argb_to_hex(&scheme.error);
        let widget_bg_hex = argb_to_hex(&scheme.surface_container_low);
        self.row_critical_background = match blend_colors(&error_hex, &widget_bg_hex, 0.18) {
            Some((r, g, b)) => rgba_str(r, g, b, 0.95),
            None => argb_to_hex(&scheme.error_container),
        };
    }

    fn compute_sizes(&mut self) {
        let bar_size = self.bar_size;
        let bar_padding_config = self.bar_padding;

        // Round to even numbers for proper pixel-perfect centering
        // This internal padding is used for widget sizing, separate from user's padding config
        let internal_bar_padding = round_to_even((bar_size as f64 * PADDING_SCALE) as u32);
        let widget_height = round_to_even(bar_size - 2 * internal_bar_padding);

        // Bar rendered height includes the user's padding config
        let bar_rendered_height = bar_size + 2 * bar_padding_config;
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

        // Content spacing starts from a shared base. Vertical bars expose
        // compact defaults through the same public CSS variables: once the bar
        // is vertical, rounded widget ends sit on the flow axis and text/icon
        // advances are not square, so content padding and stacked child gaps need
        // different compensation. Opt-out widgets (tray, taskbar)
        // use the internal base aliases directly.
        let base_content_spacing = (internal_spacing / 2).max(4) + 5;
        let widget_content_padding = base_content_spacing;
        let widget_content_gap = base_content_spacing.saturating_sub(2);

        self.sizes = ThemeSizes {
            // bar_height is the content height (widgets area), CSS padding adds the rest
            bar_height: bar_size,
            widget_height,
            // Vertical padding - fixed 2px for visual breathing room (already even)
            widget_padding_y: 2,
            font_size,
            text_icon_size,
            pixmap_icon_size,
            internal_spacing,
            widget_content_padding,
            widget_content_gap,
            widget_content_padding_vertical: widget_content_padding.saturating_sub(3),
            widget_content_gap_vertical: widget_content_gap.saturating_sub(6),
        };
    }
}

impl Default for ThemePalette {
    fn default() -> Self {
        Self {
            is_dark_mode: true,
            is_gtk_mode: false,
            is_wallpaper_mode: false,
            bar_background: DEFAULT_BAR_BG_DARK.to_string(),
            widget_background: DEFAULT_WIDGET_BG_DARK.to_string(),
            foreground_primary: "#ffffff".to_string(),
            foreground_muted: String::new(),
            foreground_disabled: String::new(),
            foreground_faint: String::new(),
            accent_source: AccentSource::Gtk, // Default to GTK accent
            accent_primary: "@accent_color".to_string(),
            accent_subtle: String::new(),
            accent_text: String::new(),
            accent_hover_bg: "color-mix(in srgb, @accent_color 80%, black)".to_string(),
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
            shadows_enabled: true,
            slider_track: String::new(),
            slider_track_disabled: String::new(),
            row_critical_background: String::new(),
            font_family: DEFAULT_FONT_FAMILY.to_string(),
            bar_opacity: 0.0,
            widget_opacity: 1.0,
            popover_opacity: None,
            bar_border_radius: 0,
            widget_border_radius: 0,
            surface_border_radius: 0,
            radius_pill: 0,
            sizes: ThemeSizes::default(),
            outline_width_px: 1,
            outline_color_resolved: "var(--color-accent-primary)".to_string(),
            outline_opacity_pct: 100,
            bar_outline_enabled: false,
            widget_outline_enabled: false,
            surface_outline_enabled: false,
            bar_radius_percent: 30,
            widget_radius_percent: 40,
            bar_size: 32,
            bar_padding: 4,
            bar_position: BarPosition::Top,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn css_var_value<'a>(css: &'a str, var_name: &str) -> Option<&'a str> {
        let needle = format!("{var_name}:");
        css.lines().find_map(|line| {
            let trimmed = line.trim();
            let value = trimmed.strip_prefix(&needle)?;
            Some(value.trim().trim_end_matches(';').trim())
        })
    }

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
    fn test_is_dark_color_boundary_near_perceptual_midpoint() {
        // Concept-anchored: pin the semantics, not the literal value.
        // Colors whose linear luminance sits just below the perceptual midpoint
        // (≈0.184187) should read as dark; just above should read as light.
        //
        // Mid-grey #757575 has linear luminance ≈ 0.1779 (below threshold).
        // Mid-grey #787878 has linear luminance ≈ 0.1878 (above threshold).
        assert!(
            is_dark_color("#757575"),
            "#757575 sits below the perceptual midpoint and should read as dark"
        );
        assert!(
            !is_dark_color("#787878"),
            "#787878 sits above the perceptual midpoint and should read as light"
        );
    }

    #[test]
    fn test_is_dark_color_unparseable_falls_back_to_dark() {
        // Documented fallback: unparseable color strings are treated as dark.
        assert!(is_dark_color("not a color"));
        assert!(is_dark_color(""));
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
        let mut config = Config::default();
        config.theme.mode = "dark".to_string();
        let palette = ThemePalette::from_config(&config, None, None);
        assert!(palette.is_dark_mode);
    }

    #[test]
    fn test_theme_palette_light_mode() {
        let mut config = Config::default();
        config.theme.mode = "light".to_string();
        let palette = ThemePalette::from_config(&config, None, None);
        assert!(!palette.is_dark_mode);
        assert_eq!(palette.foreground_primary, "#1a1a1a");
    }

    #[test]
    fn test_theme_palette_css_vars_contains_expected_vars() {
        let config = Config::default();
        let palette = ThemePalette::from_config(&config, None, None);
        let css = palette.css_vars_block();

        assert!(css.contains("--color-background-bar:"));
        assert!(css.contains("--widget-background-color:"));
        assert!(css.contains("--color-foreground-primary:"));
        assert!(css.contains("--color-accent-primary:"));
        assert!(css.contains("--radius-bar:"));
        assert!(css.contains("--widget-height:"));
        assert!(css.contains("--font-family:"));
    }

    #[test]
    fn test_css_vars_block_emits_bar_visual_config_tokens() {
        let mut config = Config::default();
        config.bar.size = 40;
        config.bar.padding = 10;
        config.bar.background_opacity = 1.0;
        config.bar.background_color = Some("#445566".to_string());
        config.bar.border_radius = 50;
        config.theme.outline = true;
        config.theme.outline_width = 3;
        config.bar.outline = Some(true);

        let palette = ThemePalette::from_config(&config, None, None);
        let css = palette.css_vars_block();
        let expected_widget_height = format!("{}px", palette.sizes.widget_height);
        let expected_bar_radius = format!("{}px", palette.bar_border_radius);

        assert_eq!(css_var_value(&css, "--bar-height"), Some("40px"));
        assert_eq!(
            css_var_value(&css, "--widget-height"),
            Some(expected_widget_height.as_str())
        );
        assert_bar_padding_contract(&css, 10, 0, 10, 0, "visible top bar");
        assert_eq!(
            css_var_value(&css, "--color-background-bar"),
            Some("#445566")
        );
        assert_eq!(
            css_var_value(&css, "--radius-bar"),
            Some(expected_bar_radius.as_str())
        );
        assert_eq!(
            css_var_value(&css, "--bar-outline-width"),
            Some("var(--outline-width)")
        );
        assert_eq!(css_var_value(&css, "--outline-width"), Some("3px"));
    }

    #[test]
    fn test_css_vars_block_emits_transparent_bar_tokens() {
        let mut config = Config::default();
        config.bar.padding = 10;
        config.bar.background_opacity = 0.0;

        let css = ThemePalette::from_config(&config, None, None).css_vars_block();

        assert_eq!(
            css_var_value(&css, "--color-background-bar"),
            Some("transparent")
        );
        assert_bar_padding_contract(&css, 10, 0, 0, 0, "transparent top bar");
    }

    #[test]
    fn test_css_vars_block_emits_widget_visual_config_tokens() {
        let mut config = Config::default();
        config.widgets.background_color = Some("#445566".to_string());
        config.widgets.background_opacity = 0.35;
        config.widgets.popover_background_opacity = Some(0.42);
        config.widgets.border_radius = 50;
        config.theme.outline = true;
        config.theme.outline_width = 3;
        config.widgets.outline = Some(true);

        let palette = ThemePalette::from_config(&config, None, None);
        let css = palette.css_vars_block();

        assert_eq!(
            css_var_value(&css, "--widget-background-color"),
            Some("#445566")
        );
        assert_eq!(
            css_var_value(&css, "--widget-background-opacity"),
            Some("35%")
        );
        assert_eq!(
            css_var_value(&css, "--popover-background-opacity"),
            Some("42%")
        );
        assert_eq!(css_var_value(&css, "--radius-widget"), Some("9999px"));
        assert_eq!(
            css_var_value(&css, "--widget-outline-width"),
            Some("var(--outline-width)")
        );
        assert_eq!(css_var_value(&css, "--outline-width"), Some("3px"));
        assert_eq!(palette.widget_radius_percent, 50);
    }

    #[test]
    fn test_workspace_indicator_hover_default_uses_internal_token() {
        let config = Config::default();
        let palette = ThemePalette::from_config(&config, None, None);
        let css = palette.css_vars_block();

        assert!(css.contains(
            "--color-workspace-indicator-hover-default-bg: var(--color-card-overlay-hover);"
        ));
        assert!(!css.contains("--color-workspace-indicator-hover-bg:"));
        assert!(!css.contains("--color-workspace-indicator-minimal-hover-bg"));
    }

    #[test]
    fn test_bar_padding_vars_bar_mode() {
        let mut config = Config::default();
        config.bar.padding = 7;
        config.bar.background_opacity = 1.0;

        for (pos, top, right, bottom, left) in &[
            ("top", 7, 0, 7, 0),
            ("bottom", 7, 0, 7, 0),
            ("left", 0, 7, 0, 7),
            ("right", 0, 7, 0, 7),
        ] {
            config.bar.position = pos.to_string();
            let css = ThemePalette::from_config(&config, None, None).css_vars_block();
            assert_bar_padding_contract(&css, *top, *right, *bottom, *left, pos);
        }
    }

    #[test]
    fn test_bar_padding_vars_island_mode() {
        let mut config = Config::default();
        config.bar.padding = 7;
        config.bar.background_opacity = 0.0;

        for (position, top, right, bottom, left) in &[
            ("top", 7, 0, 0, 0),
            ("bottom", 0, 0, 7, 0),
            ("left", 0, 0, 0, 7),
            ("right", 0, 7, 0, 0),
        ] {
            config.bar.position = position.to_string();
            let css = ThemePalette::from_config(&config, None, None).css_vars_block();
            assert_bar_padding_contract(&css, *top, *right, *bottom, *left, position);
        }
    }

    fn assert_bar_padding_contract(
        css: &str,
        top: i32,
        right: i32,
        bottom: i32,
        left: i32,
        case: &str,
    ) {
        for (var, value) in [
            ("--vp-internal-bar-padding-top", top),
            ("--vp-internal-bar-padding-right", right),
            ("--vp-internal-bar-padding-bottom", bottom),
            ("--vp-internal-bar-padding-left", left),
        ] {
            assert!(
                css.contains(&format!("{var}: {value}px;")),
                "{case}: expected {var}={value}px"
            );
        }

        for public_var in [
            "--bar-padding-y:",
            "--bar-padding-x:",
            "--bar-padding-top:",
            "--bar-padding-right:",
            "--bar-padding-bottom:",
            "--bar-padding-left:",
            "--bar-padding-y-top:",
            "--bar-padding-y-bottom:",
            "--bar-padding-x-left:",
            "--bar-padding-x-right:",
        ] {
            assert!(
                !css.contains(public_var),
                "{case}: public override hook should not be emitted: {public_var}"
            );
        }
    }

    #[test]
    fn test_generate_per_widget_css_with_background_color() {
        use crate::config::WidgetOptions;

        let mut config = Config::default();
        config.widgets.widget_configs.insert(
            "clock".to_string(),
            WidgetOptions {
                background_color: Some("#f5c2e7".to_string()),
                ..Default::default()
            },
        );

        let css = ThemePalette::generate_per_widget_css(&config);

        // Should generate CSS targeting widget surfaces, grouped paint elements, and popover.
        // The transparent .widget-group surface must not receive per-widget variables;
        // they would inherit into unrelated children in mixed groups.
        assert!(css.contains(".widget.clock"), "should target .widget.clock");
        assert!(
            css.contains(".widget-item.clock"),
            "should target .widget-item.clock"
        );
        assert!(
            !css.contains(".widget-group.clock"),
            "should not target transparent .widget-group.clock"
        );
        assert!(
            css.contains(".widget-merge-group.clock"),
            "should target .widget-merge-group.clock"
        );
        assert!(
            css.contains(".clock-popover"),
            "should target .clock-popover"
        );
        // Should set the CSS variable with normalized hex color
        assert!(
            css.contains("--widget-background-color: #f5c2e7"),
            "should set --widget-background-color"
        );
        assert!(
            css.contains(&format!(
                "--color-widget-hover-bg: {WIDGET_HOVER_BG_VALUE};"
            )),
            "should rederive --color-widget-hover-bg from per-widget background"
        );
    }

    #[test]
    fn test_generate_per_widget_css_normalizes_underscores() {
        use crate::config::WidgetOptions;

        let mut config = Config::default();
        config.widgets.widget_configs.insert(
            "quick_settings".to_string(),
            WidgetOptions {
                background_color: Some("#ff0000".to_string()),
                ..Default::default()
            },
        );

        let css = ThemePalette::generate_per_widget_css(&config);

        // Underscores should be converted to hyphens for CSS class names
        assert!(
            css.contains(".widget.quick-settings"),
            "should normalize underscores to hyphens"
        );
        assert!(
            css.contains(".quick-settings-popover"),
            "popover class should use hyphens"
        );
    }

    #[test]
    fn test_generate_per_widget_css_empty_without_overrides() {
        let config = Config::default();
        let css = ThemePalette::generate_per_widget_css(&config);

        // No widget configs with background_color = empty CSS
        assert!(
            css.is_empty() || !css.contains("--widget-background-color"),
            "should not generate CSS when no overrides configured"
        );
    }

    #[test]
    fn test_theme_sizes_computed_from_bar_size() {
        let mut config = Config::default();
        config.bar.size = 48;
        // bar_height is the content height (bar.size), CSS padding adds the visual padding
        let palette = ThemePalette::from_config(&config, None, None);

        assert_eq!(palette.sizes.bar_height, 48);
        assert!(palette.sizes.widget_height > 0);
        assert!(palette.sizes.font_size > 0);
    }

    #[test]
    fn test_accent_default_is_custom() {
        // Default accent = None with mode = "dark" means use "#adabe0" as custom hex color
        let mut config = Config::default();
        config.theme.mode = "dark".to_string();
        let palette = ThemePalette::from_config(&config, None, None);

        assert_eq!(
            palette.accent_source,
            AccentSource::Custom("#adabe0".to_string())
        );
    }

    #[test]
    fn test_accent_defaults_to_gtk_when_mode_is_gtk() {
        // When mode = "gtk" and accent is not specified, accent should default to "gtk"
        let mut config = Config::default();
        config.theme.mode = "gtk".to_string();
        // accent remains None

        let palette = ThemePalette::from_config(&config, None, None);

        assert_eq!(palette.accent_source, AccentSource::Gtk);
        // Verify derived values for GTK accent in GTK mode
        assert_eq!(palette.accent_primary, "@accent_color");
        assert_eq!(
            palette.accent_subtle,
            "color-mix(in srgb, @accent_color 20%, transparent)"
        );
        assert_eq!(
            palette.accent_hover_bg,
            "color-mix(in srgb, @accent_color 80%, black)"
        );
        assert_eq!(
            palette.accent_text, "@window_fg_color",
            "accent_text should use GTK theme foreground in GTK mode"
        );
    }

    #[test]
    fn test_accent_custom_color() {
        // When accent is a hex color, use it as custom accent
        let mut config = Config::default();
        config.theme.mode = "dark".to_string();
        config.theme.accent = Some("#ff0000".to_string());

        let palette = ThemePalette::from_config(&config, None, None);

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
        config.theme.mode = "dark".to_string();
        config.theme.accent = Some("none".to_string());

        let palette = ThemePalette::from_config(&config, None, None);

        assert_eq!(palette.accent_source, AccentSource::None);
        // In dark mode, monochrome uses foreground color
        assert_eq!(palette.accent_primary, "#ffffff");
    }

    #[test]
    fn test_accent_none_adapts_to_light_mode() {
        // Monochrome mode should use dark colors in light mode
        let mut config = Config::default();
        config.theme.mode = "light".to_string();
        config.theme.accent = Some("none".to_string());

        let palette = ThemePalette::from_config(&config, None, None);

        assert_eq!(palette.accent_source, AccentSource::None);
        // In light mode, monochrome uses foreground color
        assert_eq!(palette.accent_primary, "#1a1a1a");
    }

    #[test]
    fn test_gtk_mode() {
        // When mode = "gtk", is_gtk_mode should be true
        let mut config = Config::default();
        config.theme.mode = "gtk".to_string();

        let palette = ThemePalette::from_config(&config, None, None);

        assert!(palette.is_gtk_mode);
        // is_dark_mode remains true as a fallback for shadow opacity etc.
        assert!(palette.is_dark_mode);
    }

    #[test]
    fn test_gtk_mode_foreground_uses_theme_color() {
        let mut config = Config::default();
        config.theme.mode = "gtk".to_string();

        let palette = ThemePalette::from_config(&config, None, None);

        assert_eq!(palette.foreground_primary, "@window_fg_color");
        // Verify exact computed value to catch arithmetic bugs
        assert_eq!(
            palette.foreground_muted,
            "color-mix(in srgb, @window_fg_color 60.0%, transparent)"
        );
        assert!(
            palette.foreground_disabled.contains("@window_fg_color"),
            "disabled should reference @window_fg_color, got: {}",
            palette.foreground_disabled
        );
        assert!(
            palette.foreground_faint.contains("@window_fg_color"),
            "faint should reference @window_fg_color, got: {}",
            palette.foreground_faint
        );
    }

    #[test]
    fn test_gtk_mode_css_vars_contain_theme_colors() {
        let mut config = Config::default();
        config.theme.mode = "gtk".to_string();

        let palette = ThemePalette::from_config(&config, None, None);
        let css = palette.css_vars_block();

        // Foreground should reference GTK theme color
        assert!(
            css.contains("--color-foreground-primary: @window_fg_color"),
            "CSS should contain @window_fg_color for foreground-primary"
        );
        // Hover tint should reference GTK theme color
        assert!(
            css.contains("--widget-hover-tint: @window_fg_color"),
            "CSS should contain @window_fg_color for widget-hover-tint"
        );
        // Accent text should reference GTK theme color
        assert!(
            css.contains("--color-accent-text: @window_fg_color"),
            "CSS should contain @window_fg_color for accent-text"
        );
    }

    #[test]
    fn test_gtk_mode_derived_colors_use_theme_references() {
        // Verify that borders, sliders, overlays, and critical backgrounds all
        // reference GTK named colors instead of hardcoded rgba values.
        let mut config = Config::default();
        config.theme.mode = "gtk".to_string();

        let palette = ThemePalette::from_config(&config, None, None);

        // Borders
        assert!(
            palette.border_subtle.contains("@window_fg_color"),
            "border_subtle should reference @window_fg_color, got: {}",
            palette.border_subtle
        );

        // Slider tracks
        assert!(
            palette.slider_track.contains("@window_fg_color"),
            "slider_track should reference @window_fg_color, got: {}",
            palette.slider_track
        );
        assert!(
            palette.slider_track_disabled.contains("@window_fg_color"),
            "slider_track_disabled should reference @window_fg_color, got: {}",
            palette.slider_track_disabled
        );

        // Overlay variants
        for (name, value) in [
            ("card_overlay", &palette.card_overlay),
            ("card_overlay_hover", &palette.card_overlay_hover),
            ("card_overlay_subtle", &palette.card_overlay_subtle),
            ("card_overlay_strong", &palette.card_overlay_strong),
        ] {
            assert!(
                value.contains("@window_fg_color"),
                "{} should reference @window_fg_color, got: {}",
                name,
                value
            );
        }
        // click_catcher_overlay is neutral gray, not theme-dependent
        assert!(
            palette.click_catcher_overlay.contains("rgba(128, 128, 128"),
            "click_catcher_overlay should remain neutral gray"
        );

        // Critical backgrounds
        assert!(
            palette.row_critical_background.contains("@view_bg_color"),
            "row_critical_background should reference @view_bg_color, got: {}",
            palette.row_critical_background
        );
    }

    #[test]
    fn test_gtk_mode_accent_none_uses_theme_color() {
        // GTK mode + monochrome accent should use @window_fg_color
        let mut config = Config::default();
        config.theme.mode = "gtk".to_string();
        config.theme.accent = Some("none".to_string());

        let palette = ThemePalette::from_config(&config, None, None);

        assert_eq!(palette.accent_source, AccentSource::None);
        assert!(
            palette.accent_primary.contains("@window_fg_color"),
            "monochrome accent in GTK mode should use @window_fg_color, got: {}",
            palette.accent_primary
        );
        assert!(
            palette.accent_subtle.contains("@window_fg_color"),
            "monochrome accent_subtle in GTK mode should use @window_fg_color, got: {}",
            palette.accent_subtle
        );
    }

    #[test]
    fn test_non_gtk_modes_unchanged() {
        // Verify that dark/light modes still use hardcoded colors (no regression)
        let mut dark_config = Config::default();
        dark_config.theme.mode = "dark".to_string();
        let dark = ThemePalette::from_config(&dark_config, None, None);
        assert_eq!(dark.foreground_primary, "#ffffff");
        assert!(!dark.foreground_muted.contains("@window_fg_color"));

        let mut light_config = Config::default();
        light_config.theme.mode = "light".to_string();
        let light = ThemePalette::from_config(&light_config, None, None);
        assert_eq!(light.foreground_primary, "#1a1a1a");
        assert!(!light.foreground_muted.contains("@window_fg_color"));
    }

    #[test]
    fn test_gtk_mode_shadows_disabled() {
        // GTK mode with shadows disabled should still use @window_fg_color
        // for borders, and set shadows to "none"
        let mut config = Config::default();
        config.theme.mode = "gtk".to_string();
        config.theme.shadows = false;

        let palette = ThemePalette::from_config(&config, None, None);

        assert!(
            palette.border_subtle.contains("@window_fg_color"),
            "border_subtle should reference @window_fg_color when shadows disabled, got: {}",
            palette.border_subtle
        );
        assert_eq!(palette.shadow_soft, "none");
        assert_eq!(palette.shadow_strong, "none");
    }

    #[test]
    fn test_gtk_mode_custom_accent() {
        // GTK mode + custom accent: accent uses hex color, but accent_text
        // should still use @window_fg_color (adapts to theme)
        let mut config = Config::default();
        config.theme.mode = "gtk".to_string();
        config.theme.accent = Some("#ff0000".to_string());

        let palette = ThemePalette::from_config(&config, None, None);

        assert_eq!(
            palette.accent_source,
            AccentSource::Custom("#ff0000".to_string())
        );
        assert_eq!(palette.accent_primary, "#ff0000");
        assert_eq!(
            palette.accent_text, "@window_fg_color",
            "accent_text should use GTK theme color in GTK mode"
        );
        // Foreground should still be GTK-aware
        assert_eq!(palette.foreground_primary, "@window_fg_color");
    }

    #[test]
    fn test_theme_sizes_scale_proportionally() {
        // Test that sizes scale up proportionally with bar size
        let mut config_small = Config::default();
        config_small.bar.size = 24;
        let palette_small = ThemePalette::from_config(&config_small, None, None);

        let mut config_large = Config::default();
        config_large.bar.size = 48;
        let palette_large = ThemePalette::from_config(&config_large, None, None);

        // Larger bar should have proportionally larger sizes
        assert!(palette_large.sizes.widget_height > palette_small.sizes.widget_height);
        assert!(palette_large.sizes.font_size > palette_small.sizes.font_size);
        assert!(palette_large.sizes.text_icon_size > palette_small.sizes.text_icon_size);
    }

    #[test]
    fn test_theme_sizes_widget_fits_in_bar() {
        // CSS gives .widget: min-height + padding (top/bottom) + margin (top/bottom)
        // Total vertical footprint = widget_height + 4 * widget_padding_y
        // Note: Very small bar sizes (< 30) may not accommodate widgets properly
        for bar_size in [36, 48, 60, 72] {
            let mut config = Config::default();
            config.bar.size = bar_size;
            let palette = ThemePalette::from_config(&config, None, None);

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
        let palette = ThemePalette::from_config(&config, None, None);

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
            let palette = ThemePalette::from_config(&config, None, None);

            // Bar radius is computed from rendered height (bar_size + 2*padding config)
            // With default padding=4, max radius = (bar_size + 8) / 2
            let bar_rendered_height = bar_size + 2 * config.bar.padding;
            let max_possible_bar_radius = bar_rendered_height / 2;
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

    // --- Popover palette tests ---

    #[test]
    fn test_popover_palette_none_when_not_configured() {
        let config = Config::default(); // popover = None
        assert!(ThemePalette::popover_palette(&config, None, None).is_none());
    }

    #[test]
    fn test_popover_palette_none_for_gtk_mode() {
        let mut config = Config::default();
        config.theme.mode = "gtk".to_string();
        config.theme.popover = Some("light".to_string());
        assert!(ThemePalette::popover_palette(&config, None, None).is_none());
    }

    #[test]
    fn test_popover_palette_none_when_same_polarity() {
        // Dark mode with dark popover = no-op
        let mut config = Config::default();
        config.theme.mode = "dark".to_string();
        config.theme.popover = Some("dark".to_string());
        assert!(ThemePalette::popover_palette(&config, None, None).is_none());

        // Light mode with light popover = no-op
        config.theme.mode = "light".to_string();
        config.theme.popover = Some("light".to_string());
        assert!(ThemePalette::popover_palette(&config, None, None).is_none());
    }

    #[test]
    fn test_popover_palette_dark_to_light() {
        let mut config = Config::default();
        config.theme.mode = "dark".to_string();
        config.theme.popover = Some("light".to_string());

        let popover = ThemePalette::popover_palette(&config, None, None);
        assert!(popover.is_some(), "should produce a light popover palette");
        let popover = popover.unwrap();
        assert!(!popover.is_dark_mode, "popover should be light mode");

        // Bar palette should be dark
        let bar = ThemePalette::from_config(&config, None, None);
        assert!(bar.is_dark_mode, "bar should be dark mode");
    }

    #[test]
    fn test_popover_palette_light_to_dark() {
        let mut config = Config::default();
        config.theme.mode = "light".to_string();
        config.theme.popover = Some("dark".to_string());

        let popover = ThemePalette::popover_palette(&config, None, None);
        assert!(popover.is_some(), "should produce a dark popover palette");
        let popover = popover.unwrap();
        assert!(popover.is_dark_mode, "popover should be dark mode");

        // Bar palette should be light
        let bar = ThemePalette::from_config(&config, None, None);
        assert!(!bar.is_dark_mode, "bar should be light mode");
    }

    #[test]
    fn test_popover_palette_auto_mode_flips_scheme() {
        use material_colors::theme::ThemeBuilder;

        // Create a real Material You theme from a source color
        let source_color = Argb::new(255, 53, 132, 228); // blue-ish
        let material_theme = ThemeBuilder::with_source(source_color).build();

        let mut config = Config::default();
        config.theme.mode = "auto".to_string();
        config.theme.scheme = Some(SchemePolarity::Dark);
        config.theme.popover = Some("light".to_string());

        let popover = ThemePalette::popover_palette(&config, Some(&material_theme), None);
        assert!(
            popover.is_some(),
            "auto mode should produce popover palette"
        );
        let popover = popover.unwrap();
        assert!(
            !popover.is_dark_mode,
            "popover should be light (scheme flipped)"
        );

        let bar = ThemePalette::from_config(&config, Some(&material_theme), None);
        assert!(bar.is_dark_mode, "bar should be dark (scheme=dark)");
    }

    #[test]
    fn test_popover_css_vars_block_scoped() {
        let mut config = Config::default();
        config.theme.mode = "dark".to_string();
        config.theme.popover = Some("light".to_string());

        let popover = ThemePalette::popover_palette(&config, None, None).unwrap();
        let css = popover.css_popover_vars_block();

        // Should be scoped under .popover
        assert!(
            css.contains(".popover"),
            "popover CSS should be scoped to .popover"
        );
        // Should contain polarity-dependent variables
        assert!(css.contains("--widget-background-color:"));
        assert!(css.contains("--color-foreground-primary:"));
        assert!(css.contains("--color-accent-primary:"));
        assert!(css.contains("--color-card-overlay:"));
        assert!(css.contains("--color-border-subtle:"));
        assert!(css.contains("--shadow-soft:"));
        assert!(css.contains("--color-slider-track:"));

        // Should NOT contain size/spacing variables (those are polarity-independent)
        assert!(!css.contains("--bar-height:"));
        assert!(!css.contains("--font-size:"));
        assert!(!css.contains("--spacing:"));
        assert!(!css.contains("--border-radius:"));
    }

    // -------------------------------------------------------------------------
    // effective_scheme() tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_effective_scheme_explicit_light_wins_regardless_of_luminance() {
        // Explicit light override — luminance is dark but scheme wins
        assert!(effective_scheme(
            Some(crate::config::SchemePolarity::Light),
            Some(0.1)
        ));
    }

    #[test]
    fn test_effective_scheme_explicit_dark_wins_regardless_of_luminance() {
        // Explicit dark override — luminance is bright but scheme wins
        assert!(!effective_scheme(
            Some(crate::config::SchemePolarity::Dark),
            Some(0.9)
        ));
    }

    #[test]
    fn test_effective_scheme_none_derives_light_from_high_luminance() {
        // No override — luminance >= perceptual midpoint → light
        assert!(effective_scheme(None, Some(0.7)));
    }

    #[test]
    fn test_effective_scheme_none_derives_dark_from_low_luminance() {
        // No override — luminance < perceptual midpoint → dark
        assert!(!effective_scheme(None, Some(0.1)));
    }

    #[test]
    fn test_effective_scheme_boundary_at_threshold_is_light() {
        // Boundary: exactly at threshold → light (>= rule)
        assert!(effective_scheme(
            None,
            Some(PERCEPTUAL_LIGHT_DARK_THRESHOLD)
        ));
    }

    #[test]
    fn test_effective_scheme_just_below_boundary_is_dark() {
        // Just below threshold → dark
        assert!(!effective_scheme(
            None,
            Some(PERCEPTUAL_LIGHT_DARK_THRESHOLD - 0.001)
        ));
    }

    #[test]
    fn test_effective_scheme_no_luminance_falls_back_to_dark() {
        // No override, no luminance → fallback dark
        assert!(!effective_scheme(None, None));
    }

    #[test]
    fn test_effective_scheme_gtk_falls_back_to_luminance() {
        assert_eq!(
            effective_scheme(Some(crate::config::SchemePolarity::Gtk), Some(0.7)),
            effective_scheme(None, Some(0.7))
        );
        assert_eq!(
            effective_scheme(Some(crate::config::SchemePolarity::Gtk), Some(0.1)),
            effective_scheme(None, Some(0.1))
        );
        assert_eq!(
            effective_scheme(Some(crate::config::SchemePolarity::Gtk), None),
            effective_scheme(None, None)
        );
    }

    #[test]
    fn test_from_config_auto_mode_derives_light_from_high_luminance() {
        use material_colors::theme::ThemeBuilder;

        // Build a real Material You theme so the auto branch has schemes to select from
        let source_color = Argb::new(255, 53, 132, 228); // blue-ish
        let material_theme = ThemeBuilder::with_source(source_color).build();

        let mut config = Config::default();
        config.theme.mode = "auto".to_string();
        config.theme.scheme = None; // rely on luminance auto-derivation

        // High luminance (0.8) → light scheme → is_dark_mode should be false
        let palette = ThemePalette::from_config(&config, Some(&material_theme), Some(0.8));
        assert!(
            !palette.is_dark_mode,
            "high luminance wallpaper should auto-select light scheme"
        );

        // Low luminance (0.1) → dark scheme → is_dark_mode should be true
        let palette = ThemePalette::from_config(&config, Some(&material_theme), Some(0.1));
        assert!(
            palette.is_dark_mode,
            "low luminance wallpaper should auto-select dark scheme"
        );
    }

    #[test]
    fn test_auto_mode_fallback_respects_scheme_and_luminance_when_no_material_theme() {
        // material_theme = None simulates wallpaper extraction failure.
        // effective_scheme should still be consulted for the fallback branch.

        let mut config = Config::default();
        config.theme.mode = "auto".to_string();

        // Explicit scheme = Light → light fallback, even with no material theme
        config.theme.scheme = Some(SchemePolarity::Light);
        let palette = ThemePalette::from_config(&config, None, None);
        assert!(
            !palette.is_dark_mode,
            "explicit scheme=light should produce light fallback when material_theme is None"
        );

        // High luminance + no explicit scheme → light fallback
        config.theme.scheme = None;
        let palette = ThemePalette::from_config(&config, None, Some(0.8));
        assert!(
            !palette.is_dark_mode,
            "high luminance should produce light fallback when material_theme is None"
        );

        // Low luminance + no explicit scheme → dark fallback
        let palette = ThemePalette::from_config(&config, None, Some(0.1));
        assert!(
            palette.is_dark_mode,
            "low luminance should produce dark fallback when material_theme is None"
        );
    }

    #[test]
    fn test_auto_mode_respects_custom_urgent_state() {
        use material_colors::theme::ThemeBuilder;

        let source_color = Argb::new(255, 53, 132, 228);
        let material_theme = ThemeBuilder::with_source(source_color).build();

        let mut config = Config::default();
        config.theme.mode = "auto".to_string();
        config.theme.states.urgent = "#adeba0".to_string();

        let palette = ThemePalette::from_config(&config, Some(&material_theme), Some(0.1));

        assert_eq!(palette.state_urgent, "#adeba0");
    }

    // ===== Outline =====

    #[test]
    fn test_resolve_outline_color_hex_normalizes_to_lowercase_six() {
        // Real parsing logic: 3-char hex expands to 6-char, mixed case folds.
        assert_eq!(resolve_outline_color("#FFF"), "#ffffff");
        assert_eq!(resolve_outline_color("#3584E4"), "#3584e4");
        assert_eq!(resolve_outline_color("#abc"), "#aabbcc");
    }

    #[test]
    fn test_outline_enabled_propagates_to_all_scopes() {
        // Catches "forgot to wire a scope" regressions in the cascade.
        let mut config = Config::default();
        config.theme.outline = true;
        // Default bar opacity is 0.0 (islands mode), which suppresses the
        // inherited bar outline; raise it to verify the propagation path.
        config.bar.background_opacity = 1.0;
        let palette = ThemePalette::from_config(&config, None, None);
        assert!(palette.bar_outline_enabled);
        assert!(palette.widget_outline_enabled);
        assert!(palette.surface_outline_enabled);
    }

    #[test]
    fn test_outline_override_matrix() {
        // Per-section `Option<bool>` overrides must win over the theme default
        // in either direction, and only affect their own scope. surface_outline
        // has no per-section override in v1 so it always tracks theme.outline.
        struct Case {
            theme: bool,
            bar_override: Option<bool>,
            widgets_override: Option<bool>,
            expect_bar: bool,
            expect_widget: bool,
            expect_surface: bool,
        }
        let cases = [
            Case {
                theme: true,
                bar_override: Some(false),
                widgets_override: None,
                expect_bar: false,
                expect_widget: true,
                expect_surface: true,
            },
            Case {
                theme: false,
                bar_override: Some(true),
                widgets_override: None,
                expect_bar: true,
                expect_widget: false,
                expect_surface: false,
            },
            Case {
                theme: true,
                bar_override: None,
                widgets_override: Some(false),
                expect_bar: true,
                expect_widget: false,
                expect_surface: true,
            },
            Case {
                theme: false,
                bar_override: None,
                widgets_override: Some(true),
                expect_bar: false,
                expect_widget: true,
                expect_surface: false,
            },
        ];
        for (i, c) in cases.iter().enumerate() {
            let mut config = Config::default();
            config.theme.outline = c.theme;
            // Avoid islands-mode bar suppression contaminating the matrix —
            // that interaction has its own dedicated test below.
            config.bar.background_opacity = 1.0;
            config.bar.outline = c.bar_override;
            config.widgets.outline = c.widgets_override;
            let p = ThemePalette::from_config(&config, None, None);
            assert_eq!(p.bar_outline_enabled, c.expect_bar, "case {} bar", i);
            assert_eq!(
                p.widget_outline_enabled, c.expect_widget,
                "case {} widget",
                i
            );
            assert_eq!(
                p.surface_outline_enabled, c.expect_surface,
                "case {} surface",
                i
            );
        }
    }

    #[test]
    fn test_islands_mode_outline_suppression() {
        // In islands mode (bar.background_opacity == 0) the bar surface is
        // invisible, so an inherited bar outline would frame nothing — suppress
        // it. Explicit `bar.outline = Some(true)` must still win for users who
        // want a framing look around their islands. Other scopes inherit
        // normally regardless.
        let mut inherited = Config::default();
        inherited.theme.outline = true;
        inherited.bar.background_opacity = 0.0;
        let p = ThemePalette::from_config(&inherited, None, None);
        assert!(
            !p.bar_outline_enabled,
            "inherited bar outline must be suppressed"
        );
        assert!(p.widget_outline_enabled);
        assert!(p.surface_outline_enabled);

        let mut explicit = Config::default();
        explicit.theme.outline = false;
        explicit.bar.background_opacity = 0.0;
        explicit.bar.outline = Some(true);
        let p = ThemePalette::from_config(&explicit, None, None);
        assert!(
            p.bar_outline_enabled,
            "explicit bar.outline = true must win"
        );
    }

    #[test]
    fn test_css_vars_block_emits_outline_variables() {
        // CSS variable names are the contract with widgets/css/*.rs and
        // services/surfaces.rs — there is no compile-time check across that
        // boundary, so renames here would silently break the feature.
        let mut config = Config::default();
        config.theme.outline = true;
        config.theme.outline_width = 2;
        config.theme.outline_color = "accent".to_string();
        config.theme.outline_opacity = 0.5;
        config.bar.background_opacity = 1.0;
        let css = ThemePalette::from_config(&config, None, None).css_vars_block();

        for needle in [
            "--outline-width: 2px;",
            "--outline-color: var(--color-accent-primary);",
            "--outline-opacity: 50%;",
            "--bar-outline-width: var(--outline-width);",
            "--widget-outline-width: var(--outline-width);",
            "--surface-outline-width: var(--outline-width);",
        ] {
            assert!(css.contains(needle), "missing CSS var: {}", needle);
        }
    }

    #[test]
    fn test_css_vars_block_outline_disabled_emits_zero_width() {
        // When a scope is disabled its `--*-outline-width` resolves to 0px so
        // the consumer's `border: var(--*-outline-width) ...` renders nothing.
        let css = ThemePalette::from_config(&Config::default(), None, None).css_vars_block();
        for needle in [
            "--bar-outline-width: 0px;",
            "--widget-outline-width: 0px;",
            "--surface-outline-width: 0px;",
        ] {
            assert!(css.contains(needle), "missing CSS var: {}", needle);
        }
    }

    #[test]
    fn test_per_widget_outline_color_emits_both_variables() {
        // Widget popovers are separate GTK4 windows — CSS variables don't
        // inherit across the window boundary, so a per-widget outline_color
        // must be emitted on BOTH the bar-body selector (--widget-outline-color)
        // and the popover selector (--surface-outline-color).
        use crate::config::WidgetOptions;

        let mut config = Config::default();
        config.widgets.widget_configs.insert(
            "clock".to_string(),
            WidgetOptions {
                outline_color: Some("accent".to_string()),
                ..Default::default()
            },
        );

        let css = ThemePalette::generate_per_widget_css(&config);
        assert!(
            css.contains("--widget-outline-color: var(--color-accent-primary);"),
            "missing widget outline color"
        );
        assert!(
            css.contains("--surface-outline-color: var(--color-accent-primary);"),
            "missing surface outline color (popover would not pick up the override)"
        );
    }
}
