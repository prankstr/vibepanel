//! Shared utilities for media-related widgets.
//!
//! This module provides common helpers used by the media widget, media popover,
//! and media window components.

use gtk4::prelude::*;
use gtk4::{Align, Button};

use crate::services::icons::IconsService;
use crate::styles::icon;

/// Create a media control button with the standard styling pattern.
///
/// This helper reduces boilerplate for creating media control buttons that
/// share the same structure: icon + CSS classes + tooltip + click handler.
/// The icon is automatically centered within the button.
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
    // Center the icon within the button
    icon_handle.widget().set_halign(Align::Center);
    icon_handle.widget().set_valign(Align::Center);

    let btn = Button::new();
    btn.set_child(Some(&icon_handle.widget()));
    for class in css_classes {
        btn.add_css_class(class);
    }
    btn.set_tooltip_text(Some(tooltip));
    btn.connect_clicked(move |_| on_click());
    btn
}
