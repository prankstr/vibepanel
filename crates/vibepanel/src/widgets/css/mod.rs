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
pub fn utility_css() -> &'static str {
    base::css()
}

/// Generate all widget CSS.
pub fn widget_css(config: &Config) -> String {
    let screen_margin = config.bar.screen_margin;
    let spacing = config.bar.spacing;

    // Collect all CSS from submodules
    let bar_css = bar::css(screen_margin, spacing);
    let tray_css = tray::css();
    let buttons_css = buttons::css();
    let calendar_css = calendar::css();
    let quick_settings_css = quick_settings::css();
    let battery_css = battery::css();
    let notifications_css = notifications::css();
    let osd_css = osd::css();
    let media_css = media::css();
    let system_css = system::css();

    format!(
        "{bar_css}\n{tray_css}\n{buttons_css}\n{calendar_css}\n{quick_settings_css}\n{battery_css}\n{notifications_css}\n{osd_css}\n{media_css}\n{system_css}"
    )
}
