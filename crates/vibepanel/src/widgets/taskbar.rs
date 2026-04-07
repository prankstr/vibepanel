//! Taskbar widget - displays a list of all windows.
//!
//! Shows all open windows as clickable buttons with app icons.
//! Clicking a window button focuses that window.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk4::gdk::BUTTON_PRIMARY;
use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, GestureClick, Image, Label, Orientation, Widget};
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
}

impl WidgetConfig for TaskbarConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options(
            "taskbar",
            entry,
            &["show_title", "show_icon", "max_windows", "filter_by_output"],
        );

        Self {
            show_title: entry
                .options
                .get("show_title")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            show_icon: entry
                .options
                .get("show_icon")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            max_windows: entry
                .options
                .get("max_windows")
                .and_then(|v| v.as_integer())
                .map(|v| v as usize)
                .unwrap_or(0),
            filter_by_output: entry
                .options
                .get("filter_by_output")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
        }
    }
}

impl Default for TaskbarConfig {
    fn default() -> Self {
        Self {
            show_title: false,
            show_icon: true,
            max_windows: 0,
            filter_by_output: true,
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
                update_button_state(button, window);
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

    if window.is_focused {
        button.add_css_class(widget::ACTIVE);
    }

    if config.show_icon {
        let icon_name = get_app_icon_name(&window.app_id);
        let icon = Image::from_icon_name(&icon_name);
        icon.add_css_class(icon::TEXT);
        icon.add_css_class(widget::TASKBAR_ICON);

        let sizes = ConfigManager::global().theme_sizes();
        icon.set_pixel_size(sizes.pixmap_icon_size as i32);

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

fn update_button_state(button: &Widget, window: &crate::services::compositor::Window) {
    if window.is_focused {
        button.add_css_class(widget::ACTIVE);
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
    }

    #[test]
    fn test_taskbar_config_custom() {
        let mut options = HashMap::new();
        options.insert("show_title".to_string(), Value::Boolean(false));
        options.insert("show_icon".to_string(), Value::Boolean(false));
        options.insert("max_windows".to_string(), Value::Integer(5));
        options.insert("filter_by_output".to_string(), Value::Boolean(false));

        let entry = make_widget_entry("taskbar", options);
        let config = TaskbarConfig::from_entry(&entry);
        assert!(!config.show_title);
        assert!(!config.show_icon);
        assert_eq!(config.max_windows, 5);
        assert!(!config.filter_by_output);
    }
}
