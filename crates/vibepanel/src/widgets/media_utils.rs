//! Shared utilities for media-related widgets.

use gtk4::prelude::*;
use gtk4::{Align, Button};

use crate::services::icons::IconsService;
use crate::styles::icon;

/// Create a media control button with standard styling.
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
    icon_handle.widget().set_halign(Align::Center);
    icon_handle.widget().set_valign(Align::Center);

    let btn = Button::new();
    btn.set_has_frame(false);
    btn.set_valign(Align::Center);
    btn.set_child(Some(&icon_handle.widget()));
    for class in css_classes {
        btn.add_css_class(class);
    }
    btn.set_tooltip_text(Some(tooltip));
    btn.connect_clicked(move |_| on_click());
    btn
}
