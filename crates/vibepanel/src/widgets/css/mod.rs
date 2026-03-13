//! CSS for vibepanel bar, panels, and widgets.
//!
//! This module contains all CSS generation for vibepanel:
//! - `utility_css()` - Shared utility classes (colors, focus suppression, popovers)
//! - `widget_css()` - Widget-specific styling (bar, cards, sliders, etc.)
//!
//! CSS is organized into submodules by component:
//! - `base` - Shared utility classes used across all components
//! - `bar` - Bar container, sections, workspace indicators
//! - `buttons` - Button style classes (reset, accent, card, link, ghost)
//! - `tray` - System tray items and menus
//! - `calendar` - Calendar widget styles
//! - `quick_settings` - Quick settings panel, cards, rows
//! - `battery` - Battery widget and popover
//! - `notifications` - Notification rows and toasts
//! - `osd` - On-screen display overlays
//! - `media` - Media player widget
//! - `system` - System info popover

/// Widget background with opacity applied via `color-mix()`.
pub const WIDGET_BG_WITH_OPACITY: &str = "color-mix(in srgb, var(--widget-background-color) var(--widget-background-opacity), transparent)";

/// Widget hover background — blends 8% of the hover tint (white/black) into the base background.
/// Uses nested `color-mix()` so per-widget `--widget-background-color` overrides cascade automatically.
pub const WIDGET_BG_HOVER: &str = "color-mix(in srgb, color-mix(in srgb, var(--widget-background-color) var(--widget-background-opacity), transparent) 92%, var(--widget-hover-tint))";

/// Popover open/close animation duration in milliseconds.
///
/// Single source of truth for both CSS transitions and Rust timeout durations.
/// Used by `base.rs` for the `.popover-animate` CSS rule and by
/// `layer_shell_popover.rs` / `quick_settings/window.rs` for close-animation timeouts.
pub const POPOVER_ANIMATION_MS: u64 = 150;

mod bar;
mod base;
mod battery;
mod buttons;
mod calendar;
mod media;
mod notifications;
mod osd;
mod quick_settings;
mod system;
mod tray;

use vibepanel_core::Config;

/// Return shared utility CSS.
///
/// These are truly shared styles that apply across multiple surfaces
/// (bar, popovers, quick settings, etc).
pub fn utility_css(config: &Config) -> String {
    base::css(config.theme.animations)
}

/// Generate all widget CSS.
pub fn widget_css(config: &Config) -> String {
    let screen_margin = config.bar.screen_margin;
    let spacing = config.bar.spacing;
    let animations = config.theme.animations;

    // Resolve per-widget workspace animation flag: explicit `animate` in
    // [widgets.workspaces] overrides the global `theme.animations` default.
    let workspace_animations = config
        .widgets
        .get_options("workspaces")
        .and_then(|opts| opts.options.get("animate"))
        .and_then(|v| v.as_bool())
        .unwrap_or(animations);

    // Collect all CSS from submodules
    let bar_css = bar::css(screen_margin, spacing, workspace_animations);
    let tray_css = tray::css(animations);
    let buttons_css = buttons::css();
    let calendar_css = calendar::css();
    let quick_settings_css = quick_settings::css(animations);
    let battery_css = battery::css();
    let notifications_css = notifications::css();
    let osd_css = osd::css();
    let media_css = media::css(animations);
    let system_css = system::css();

    format!(
        "{bar_css}\n{tray_css}\n{buttons_css}\n{calendar_css}\n{quick_settings_css}\n{battery_css}\n{notifications_css}\n{osd_css}\n{media_css}\n{system_css}"
    )
}
