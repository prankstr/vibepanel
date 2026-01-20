//! Shared utilities for media-related widgets.
//!
//! This module provides common helpers used by the media widget, media popover,
//! and media window components.

use gtk4::Button;
use gtk4::prelude::*;

use crate::services::icons::IconsService;
use crate::styles::icon;

/// Create a media control button with the standard styling pattern.
///
/// This helper reduces boilerplate for creating media control buttons that
/// share the same structure: icon + CSS classes + tooltip + click handler.
///
/// # Arguments
/// * `icons` - The IconsService to create icons from
/// * `icon_name` - Material Symbols icon name (e.g., "skip_previous", "play_arrow")
/// * `tooltip` - Tooltip text for the button
/// * `css_classes` - CSS classes to apply to the button
/// * `on_click` - Callback invoked when the button is clicked
///
/// # Example
/// ```ignore
/// let prev_btn = create_media_control_button(
///     &icons,
///     "skip_previous",
///     "Previous",
///     &[media::CONTROL_BTN, button::COMPACT],
///     || MediaService::global().previous(),
/// );
/// ```
pub fn create_media_control_button<F>(
    icons: &IconsService,
    icon_name: &str,
    tooltip: &str,
    css_classes: &[&str],
    on_click: F,
) -> Button
where
    F: Fn() + 'static,
{
    let icon_handle = icons.create_icon(icon_name, &[icon::ICON]);
    let btn = Button::new();
    btn.set_child(Some(&icon_handle.widget()));
    for class in css_classes {
        btn.add_css_class(class);
    }
    btn.set_tooltip_text(Some(tooltip));
    btn.connect_clicked(move |_| on_click());
    btn
}

/// Volume threshold for "low" volume icon.
pub const VOLUME_LOW_THRESHOLD: f64 = 0.33;

/// Volume threshold for "medium" volume icon (below this and above LOW is "low").
pub const VOLUME_MEDIUM_THRESHOLD: f64 = 0.66;

/// Get the appropriate volume icon name based on volume level.
///
/// # Arguments
/// * `volume` - Volume level from 0.0 to 1.0+
///
/// # Returns
/// A freedesktop icon name for the appropriate volume level. These names
/// (e.g., `"audio-volume-muted"`, `"audio-volume-high"`) follow the freedesktop
/// icon naming specification and are used with `IconHandle` and `BaseWidget.add_icon()`,
/// which map them internally to Material Symbols font glyphs.
pub fn volume_icon_name(volume: f64) -> &'static str {
    if volume <= 0.0 {
        "audio-volume-muted"
    } else if volume < VOLUME_LOW_THRESHOLD {
        "audio-volume-low"
    } else if volume < VOLUME_MEDIUM_THRESHOLD {
        "audio-volume-medium"
    } else {
        "audio-volume-high"
    }
}
