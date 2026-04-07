//! Taskbar widget - displays a list of all windows.
//!
//! Shows all open windows as clickable buttons with app icons.
//! Clicking a window button focuses that window.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk4::gdk::BUTTON_PRIMARY;
use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, CssProvider, GestureClick, Image, Label, Orientation, Widget};
use tracing::debug;
use vibepanel_core::config::WidgetEntry;

use crate::services::callbacks::CallbackId;
use crate::services::config_manager::ConfigManager;
use crate::services::icons::get_app_icon_name;
use crate::services::tooltip::TooltipManager;
use crate::services::window_list::WindowListService;
use crate::styles::{icon, state, widget};
use crate::widgets::WidgetConfig;
use crate::widgets::base::BaseWidget;
use crate::widgets::warn_unknown_options;

/// Default icon size fallback when ConfigManager is not yet available (e.g. tests).
const DEFAULT_ICON_SIZE: i32 = 16;

/// Default max button size fallback for tests (bar_size - 2 * widget_padding_y).
const DEFAULT_MAX_BUTTON_SIZE: i32 = 28;

/// Default widget radius percent fallback for tests (0 = square).
const DEFAULT_WIDGET_RADIUS_PERCENT: u32 = 0;

/// Configuration for the taskbar widget.
#[derive(Debug, Clone)]
pub struct TaskbarConfig {
    /// Whether to show window titles.
    pub show_title: bool,
    /// Whether to show app icons.
    pub show_icon: bool,
    /// Maximum number of windows to show (0 = unlimited).
    pub max_windows: usize,
    /// Whether to only show windows on the same output as the bar.
    pub filter_by_output: bool,
    /// Icon size in pixels (default: theme-computed pixmap_icon_size).
    pub icon_size: i32,
    /// Whether to highlight the focused window.
    pub show_active: bool,
    /// Maximum button size (bar_size - 2 * widget_padding_y), used to cap icon + padding.
    max_button_size: i32,
    /// Widget radius percent from theme (0 = square, 100 = pill).
    widget_radius_percent: u32,
}

impl WidgetConfig for TaskbarConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options(
            "taskbar",
            entry,
            &[
                "show_title",
                "show_icon",
                "max_windows",
                "filter_by_output",
                "icon_size",
                "show_active",
            ],
        );

        let defaults = Self::default();

        let icon_size = entry
            .options
            .get("icon_size")
            .and_then(|v| v.as_integer())
            .map(|v| (v as i32).max(8))
            .unwrap_or(defaults.icon_size)
            .min(defaults.max_button_size);

        Self {
            show_title: entry
                .options
                .get("show_title")
                .and_then(|v| v.as_bool())
                .unwrap_or(defaults.show_title),
            show_icon: entry
                .options
                .get("show_icon")
                .and_then(|v| v.as_bool())
                .unwrap_or(defaults.show_icon),
            max_windows: entry
                .options
                .get("max_windows")
                .and_then(|v| v.as_integer())
                .map(|v| v as usize)
                .unwrap_or(defaults.max_windows),
            filter_by_output: entry
                .options
                .get("filter_by_output")
                .and_then(|v| v.as_bool())
                .unwrap_or(defaults.filter_by_output),
            icon_size,
            show_active: entry
                .options
                .get("show_active")
                .and_then(|v| v.as_bool())
                .unwrap_or(defaults.show_active),
            max_button_size: defaults.max_button_size,
            widget_radius_percent: defaults.widget_radius_percent,
        }
    }
}

impl Default for TaskbarConfig {
    fn default() -> Self {
        let (icon_size, max_button_size, widget_radius_percent) = std::panic::catch_unwind(|| {
            let cm = ConfigManager::global();
            let sizes = cm.theme_sizes();
            let max_button = (cm.bar_size() - 2 * sizes.widget_padding_y) as i32;
            (
                sizes.pixmap_icon_size as i32,
                max_button,
                cm.widget_radius_percent(),
            )
        })
        .unwrap_or((
            DEFAULT_ICON_SIZE,
            DEFAULT_MAX_BUTTON_SIZE,
            DEFAULT_WIDGET_RADIUS_PERCENT,
        ));

        Self {
            show_title: false,
            show_icon: true,
            max_windows: 0,
            filter_by_output: true,
            icon_size,
            show_active: true,
            max_button_size,
            widget_radius_percent,
        }
    }
}

/// Taskbar widget that displays all windows as clickable buttons.
pub struct TaskbarWidget {
    base: BaseWidget,
    window_list_callback_id: CallbackId,
}

impl TaskbarWidget {
    pub fn new(config: TaskbarConfig, output_id: Option<String>) -> Self {
        let base = BaseWidget::new(&[widget::TASKBAR]);
        let content = base.content().clone();

        let window_buttons: Rc<RefCell<HashMap<u64, Widget>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let current_window_ids: Rc<RefCell<Vec<u64>>> = Rc::new(RefCell::new(Vec::new()));
        let output_id_for_log = output_id.clone();

        let window_list_callback_id = WindowListService::global().connect(move |snapshot| {
            update_window_buttons(
                &content,
                &window_buttons,
                &current_window_ids,
                snapshot,
                &config,
                output_id.as_deref(),
            );
        });

        debug!("TaskbarWidget created (output_id: {:?})", output_id_for_log);

        Self {
            base,
            window_list_callback_id,
        }
    }

    pub fn widget(&self) -> &GtkBox {
        self.base.widget()
    }
}

impl Drop for TaskbarWidget {
    fn drop(&mut self) {
        WindowListService::global().disconnect(self.window_list_callback_id);
    }
}

fn update_window_buttons(
    container: &GtkBox,
    buttons: &Rc<RefCell<HashMap<u64, Widget>>>,
    current_ids: &Rc<RefCell<Vec<u64>>>,
    snapshot: &crate::services::compositor::WindowListSnapshot,
    config: &TaskbarConfig,
    output_id: Option<&str>,
) {
    let windows: Vec<_> = snapshot
        .windows
        .iter()
        .filter(|win| {
            if !config.filter_by_output || output_id.is_none() {
                return true;
            }
            win.output.as_deref() == output_id || win.output.is_none()
        })
        .take(if config.max_windows > 0 {
            config.max_windows
        } else {
            usize::MAX
        })
        .cloned()
        .collect();

    let new_ids: Vec<u64> = windows.iter().map(|w| w.id).collect();

    let needs_rebuild = {
        let current = current_ids.borrow();
        new_ids.len() != current.len()
            || new_ids.iter().enumerate().any(|(i, id)| current[i] != *id)
    };

    if needs_rebuild {
        while let Some(child) = container.first_child() {
            container.remove(&child);
        }
        buttons.borrow_mut().clear();

        for window in windows.iter() {
            let button = create_window_button(window, config);
            container.append(&button);
            buttons.borrow_mut().insert(window.id, button);
        }

        *current_ids.borrow_mut() = new_ids;
    } else {
        for window in &windows {
            if let Some(button) = buttons.borrow().get(&window.id) {
                update_button_state(button, window, config);
            }
        }
    }
}

fn create_window_button(
    window: &crate::services::compositor::Window,
    config: &TaskbarConfig,
) -> Widget {
    let button = GtkBox::new(Orientation::Horizontal, 4);
    button.add_css_class(widget::TASKBAR_BUTTON);
    button.add_css_class(state::CLICKABLE);
    button.set_valign(Align::Center);

    // Padding scales with icon_size (icon_size / 4), with a minimum of 3px so the
    // background is always visible. The icon shrinks only when it wouldn't leave
    // room for min_pad within max_button_size (bar_size minus widget padding).
    let min_pad = 3;
    let effective_icon = config.icon_size.min(config.max_button_size - 2 * min_pad);
    let ideal_pad = effective_icon / 4;
    let available = ((config.max_button_size - effective_icon) / 2).max(0);
    let pad = ideal_pad.min(available).max(min_pad);
    // Radius follows the theme's widget radius formula: (size * percent / 100), capped
    // at half the button size (fully pill-shaped). This means 50% already gives a pill,
    // matching how widget_border_radius behaves relative to bar_size.
    let total_button_size = effective_icon + 2 * pad;
    let max_radius = total_button_size / 2;
    let radius = (total_button_size as u32 * config.widget_radius_percent / 100)
        .min(max_radius as u32) as i32;
    let css = format!(".taskbar-button {{ padding: {pad}px; border-radius: {radius}px; }}");
    let provider = CssProvider::new();
    provider.load_from_string(&css);
    #[allow(deprecated)]
    button
        .style_context()
        .add_provider(&provider, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);

    if config.show_active && window.is_focused {
        button.add_css_class(widget::ACTIVE);
    }

    if config.show_icon {
        let icon_name = get_app_icon_name(&window.app_id);
        let icon = Image::from_icon_name(&icon_name);
        icon.add_css_class(icon::TEXT);
        icon.add_css_class(widget::TASKBAR_ICON);

        icon.set_pixel_size(effective_icon);

        button.append(&icon);
    }

    if config.show_title {
        let title = if window.title.is_empty() {
            &window.app_id
        } else {
            &window.title
        };

        let label = Label::new(Some(title));
        label.add_css_class(widget::TASKBAR_LABEL);
        label.set_single_line_mode(true);
        label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        label.set_max_width_chars(20);
        button.append(&label);
    }

    button.add_css_class(widget::TASKBAR_BUTTON_WRAPPER);

    let window_id = window.id;
    let gesture = GestureClick::new();
    gesture.set_button(BUTTON_PRIMARY);
    gesture.connect_released(move |gesture, _n_press, _x, _y| {
        if gesture.current_button() == BUTTON_PRIMARY {
            TooltipManager::global().cancel_and_hide();
            WindowListService::global().focus_window(window_id);
        }
    });
    button.add_controller(gesture);

    let tooltip = if window.title.is_empty() {
        window.app_id.clone()
    } else {
        format!("{} - {}", window.app_id, window.title)
    };
    TooltipManager::global().set_styled_tooltip(&button, &tooltip);

    button.upcast()
}

fn update_button_state(
    button: &Widget,
    window: &crate::services::compositor::Window,
    config: &TaskbarConfig,
) {
    if config.show_active {
        if window.is_focused {
            button.add_css_class(widget::ACTIVE);
        } else {
            button.remove_css_class(widget::ACTIVE);
        }
    } else {
        button.remove_css_class(widget::ACTIVE);
    }

    let tooltip = if window.title.is_empty() {
        window.app_id.clone()
    } else {
        format!("{} - {}", window.app_id, window.title)
    };
    TooltipManager::global().set_styled_tooltip(button, &tooltip);

    // Update the label text if present (title may have changed)
    if let Some(container) = button.downcast_ref::<GtkBox>() {
        let mut next = container.first_child();
        while let Some(child_widget) = next {
            if let Some(label) = child_widget.downcast_ref::<Label>() {
                let title = if window.title.is_empty() {
                    &window.app_id
                } else {
                    &window.title
                };
                label.set_label(title);
                break;
            }
            next = child_widget.next_sibling();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use toml::Value;

    fn make_widget_entry(name: &str, options: HashMap<String, Value>) -> WidgetEntry {
        WidgetEntry {
            name: name.to_string(),
            options,
        }
    }

    #[test]
    fn test_taskbar_config_default() {
        let entry = make_widget_entry("taskbar", HashMap::new());
        let config = TaskbarConfig::from_entry(&entry);
        assert!(!config.show_title);
        assert!(config.show_icon);
        assert_eq!(config.max_windows, 0);
        assert!(config.filter_by_output);
        assert_eq!(config.icon_size, DEFAULT_ICON_SIZE);
        assert!(config.show_active);
    }

    #[test]
    fn test_taskbar_config_custom() {
        let mut options = HashMap::new();
        options.insert("show_title".to_string(), Value::Boolean(false));
        options.insert("show_icon".to_string(), Value::Boolean(false));
        options.insert("max_windows".to_string(), Value::Integer(5));
        options.insert("filter_by_output".to_string(), Value::Boolean(false));
        options.insert("icon_size".to_string(), Value::Integer(24));
        options.insert("show_active".to_string(), Value::Boolean(false));

        let entry = make_widget_entry("taskbar", options);
        let config = TaskbarConfig::from_entry(&entry);
        assert!(!config.show_title);
        assert!(!config.show_icon);
        assert_eq!(config.max_windows, 5);
        assert!(!config.filter_by_output);
        assert_eq!(config.icon_size, 24);
        assert!(!config.show_active);
    }

    #[test]
    fn test_taskbar_config_icon_size_min_clamp() {
        let mut options = HashMap::new();
        options.insert("icon_size".to_string(), Value::Integer(2));

        let entry = make_widget_entry("taskbar", options);
        let config = TaskbarConfig::from_entry(&entry);
        assert_eq!(config.icon_size, 8);
    }
}
