//! Window buttons shared by the taskbar and the workspaces widget.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk4::gdk::BUTTON_PRIMARY;
use gtk4::prelude::*;
use gtk4::{Align, GestureClick, Image, Widget};

use crate::services::compositor::Window;
use crate::services::icons::resolve_app_icon_name;
use crate::services::window_list::WindowListService;
use crate::styles::icon;
use crate::widgets::window_title::friendly_app_name;

/// Icon used when an app ID matches neither a desktop entry nor a themed icon.
const FALLBACK_APP_ICON: &str = "application-x-executable";

/// Themed icon name for an app ID, with a generic fallback.
pub(crate) fn app_icon_name(app_id: &str) -> String {
    resolve_app_icon_name(app_id, FALLBACK_APP_ICON)
}

/// Centered app icon image for an app ID.
pub(crate) fn app_icon(app_id: &str, pixel_size: i32) -> Image {
    let icon = Image::from_icon_name(&app_icon_name(app_id));
    icon.add_css_class(icon::TEXT);
    icon.set_valign(Align::Center);
    icon.set_halign(Align::Center);
    icon.set_pixel_size(pixel_size);
    icon
}

/// The window's display title: title if non-empty, otherwise app_id.
pub(crate) fn window_display_title(window: &Window) -> &str {
    if window.title.is_empty() {
        &window.app_id
    } else {
        &window.title
    }
}

/// Readable app name for an app ID (e.g. "com.mitchellh.ghostty" → "Ghostty"),
/// as shown by the window title widget. Falls back to the app ID itself.
pub(crate) fn app_display_name(app_id: &str) -> String {
    thread_local! {
        static NAMES: Rc<RefCell<HashMap<String, String>>> = Rc::default();
    }
    let name = NAMES.with(|names| friendly_app_name(names, app_id));
    if name.is_empty() {
        app_id.to_string()
    } else {
        name
    }
}

/// Tooltip for a window: "App - title", or just the app name.
pub(crate) fn window_tooltip(window: &Window) -> String {
    let app = app_display_name(&window.app_id);
    if window.title.is_empty() {
        app
    } else {
        format!("{app} - {}", window.title)
    }
}

/// Set at most one of `classes` on `widget`, leaving unchanged classes alone.
///
/// Removing and re-adding the same class makes GTK's style system return
/// stale measure() values, so only toggle what changed.
pub(crate) fn sync_exclusive_class(widget: &Widget, classes: &[&str], target: Option<&str>) {
    for &cls in classes {
        if Some(cls) == target {
            if !widget.has_css_class(cls) {
                widget.add_css_class(cls);
            }
        } else if widget.has_css_class(cls) {
            widget.remove_css_class(cls);
        }
    }
}

/// Focus the window chosen by `target` when `widget` is clicked.
///
/// The press is claimed so that clickable ancestors (e.g. a workspace
/// indicator) don't also act on it.
pub(crate) fn connect_focus_on_click(
    widget: &impl IsA<Widget>,
    target: impl Fn() -> Option<u64> + 'static,
) {
    connect_window_click(widget, target, |window_id| {
        WindowListService::global().focus_window(window_id);
    });
}

/// Like `connect_focus_on_click`, but runs `action` on the chosen window.
pub(crate) fn connect_window_click(
    widget: &impl IsA<Widget>,
    target: impl Fn() -> Option<u64> + 'static,
    action: impl Fn(u64) + 'static,
) {
    let gesture = GestureClick::new();
    gesture.set_button(BUTTON_PRIMARY);
    gesture.connect_pressed(|gesture, _n_press, _x, _y| {
        gesture.set_state(gtk4::EventSequenceState::Claimed);
    });
    gesture.connect_released(move |gesture, _n_press, _x, _y| {
        if gesture.current_button() != BUTTON_PRIMARY {
            return;
        }
        if let Some(window_id) = target() {
            action(window_id);
        }
    });
    widget.add_controller(gesture);
}
