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
use crate::widgets::css::CONTENT_PADDING_X;
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
    /// Icon size in pixels.  `None` means "use theme default" (pixmap_icon_size).
    /// Resolved to a concrete value in `TaskbarWidget::new()`.
    pub icon_size: Option<i32>,
    /// Whether to highlight the focused window.
    pub show_active: bool,
    /// Whether to show a separator between windows on different workspaces.
    pub show_workspace_separator: bool,
}

/// Theme-derived layout values computed once at widget construction time.
///
/// These are not user-configurable — they come from the bar geometry and theme
/// settings via `ConfigManager`, which is guaranteed to be initialized when
/// widgets are built.
#[derive(Debug, Clone, Copy)]
struct TaskbarLayout {
    /// Maximum button size (bar_size - 2 * widget_padding_y).
    max_button_size: i32,
    /// Widget radius percent from theme (0 = square, 100 = pill).
    widget_radius_percent: u32,
    /// Theme-derived icon size (pixmap_icon_size).
    theme_icon_size: i32,
}

impl TaskbarLayout {
    /// Compute layout from the current theme configuration.
    fn from_config_manager() -> Self {
        let cm = ConfigManager::global();
        let sizes = cm.theme_sizes();
        Self {
            max_button_size: (cm.bar_size() - 2 * sizes.widget_padding_y) as i32,
            widget_radius_percent: cm.widget_radius_percent(),
            theme_icon_size: sizes.pixmap_icon_size as i32,
        }
    }
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
                "show_workspace_separator",
            ],
        );

        let defaults = Self::default();

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
            // None = use theme default; Some = user-specified (clamped to min 8px).
            icon_size: entry
                .options
                .get("icon_size")
                .and_then(|v| v.as_integer())
                .map(|v| (v as i32).max(8)),
            show_active: entry
                .options
                .get("show_active")
                .and_then(|v| v.as_bool())
                .unwrap_or(defaults.show_active),
            show_workspace_separator: entry
                .options
                .get("show_workspace_separator")
                .and_then(|v| v.as_bool())
                .unwrap_or(defaults.show_workspace_separator),
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
            icon_size: None,
            show_active: true,
            show_workspace_separator: true,
        }
    }
}

/// Window identity snapshot: `(window_id, workspace_id, output)`.
/// Used to detect when the button list needs a full rebuild (window added/removed,
/// moved to a different workspace, or reordered across outputs).
type WindowIdList = Vec<(u64, Option<i32>, Option<String>)>;

/// Taskbar widget that displays all windows as clickable buttons.
pub struct TaskbarWidget {
    base: BaseWidget,
    window_list_callback_id: CallbackId,
}

impl TaskbarWidget {
    pub fn new(mut config: TaskbarConfig, output_id: Option<String>) -> Self {
        let layout = TaskbarLayout::from_config_manager();

        // Resolve icon_size: user-specified value takes priority, otherwise
        // use the theme-derived pixmap_icon_size.  Clamp to fit within buttons.
        config.icon_size = Some(
            config
                .icon_size
                .unwrap_or(layout.theme_icon_size)
                .min(layout.max_button_size),
        );

        let base = BaseWidget::new(&[widget::TASKBAR]);
        let content = base.content().clone();

        // Reduce .content horizontal padding so button padding doesn't add
        // extra space at the widget edges.  The default .content rule uses
        // `padding: var(--widget-padding-y) {CONTENT_PADDING_X}px`.  We override
        // the horizontal component to `CONTENT_PADDING_X - pad` so the first/last
        // button's visual padding fills the remaining edge space exactly.
        let (effective_icon, pad) = compute_button_padding(
            config.icon_size.unwrap_or(layout.theme_icon_size),
            layout.max_button_size,
        );

        // Shared CssProvider for all taskbar buttons — padding and radius are
        // constant (derived from icon_size + theme), so one provider suffices.
        let total_button_size = effective_icon + 2 * pad;
        let max_radius = total_button_size / 2;
        let radius = (total_button_size as u32 * layout.widget_radius_percent / 100)
            .min(max_radius as u32) as i32;
        let button_css = CssProvider::new();
        button_css.load_from_string(&format!(
            ".taskbar-button {{ padding: {pad}px; border-radius: {radius}px; }}"
        ));

        let content_hpad = (CONTENT_PADDING_X - pad).max(0);
        let content_css = CssProvider::new();
        content_css.load_from_string(&format!(
            ".taskbar .content {{ padding-left: {content_hpad}px; padding-right: {content_hpad}px; }}"
        ));
        #[allow(deprecated)]
        content
            .style_context()
            .add_provider(&content_css, gtk4::STYLE_PROVIDER_PRIORITY_USER + 1000);

        let window_buttons: Rc<RefCell<HashMap<u64, Widget>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let current_window_ids: Rc<RefCell<WindowIdList>> = Rc::new(RefCell::new(Vec::new()));
        let output_id_for_log = output_id.clone();

        let window_list_callback_id = WindowListService::global().connect(move |snapshot| {
            update_window_buttons(
                &content,
                &window_buttons,
                &current_window_ids,
                snapshot,
                &config,
                &button_css,
                effective_icon,
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

#[allow(clippy::too_many_arguments)]
fn update_window_buttons(
    container: &GtkBox,
    buttons: &Rc<RefCell<HashMap<u64, Widget>>>,
    current_ids: &Rc<RefCell<WindowIdList>>,
    snapshot: &crate::services::compositor::WindowListSnapshot,
    config: &TaskbarConfig,
    button_css: &CssProvider,
    effective_icon: i32,
    output_id: Option<&str>,
) {
    let mut windows: Vec<_> = snapshot
        .windows
        .iter()
        .filter(|win| {
            if !config.filter_by_output || output_id.is_none() {
                return true;
            }
            win.output.as_deref() == output_id || win.output.is_none()
        })
        .cloned()
        .collect();

    // When max_windows is set and the focused window would be cut off,
    // swap it into the last visible slot so the user always sees it.
    if config.max_windows > 0 && windows.len() > config.max_windows {
        if let Some(focused_idx) = windows.iter().position(|w| w.is_focused)
            && focused_idx >= config.max_windows
        {
            windows.swap(config.max_windows - 1, focused_idx);
        }
        windows.truncate(config.max_windows);
    }

    let new_ids: WindowIdList = windows
        .iter()
        .map(|w| (w.id, w.workspace_id, w.output.clone()))
        .collect();

    let needs_rebuild = {
        let current = current_ids.borrow();
        new_ids != *current
    };

    if needs_rebuild {
        while let Some(child) = container.first_child() {
            container.remove(&child);
        }
        buttons.borrow_mut().clear();

        let mut prev_workspace: Option<i32> = None;
        let mut prev_output: Option<&str> = None;
        for window in windows.iter() {
            if config.show_workspace_separator {
                let output_changed = match (prev_output, window.output.as_deref()) {
                    (Some(prev), Some(cur)) => prev != cur,
                    _ => false,
                };

                if output_changed {
                    // Output boundary — insert a more prominent separator.
                    let sep = GtkBox::new(Orientation::Vertical, 0);
                    sep.add_css_class(widget::TASKBAR_SEPARATOR);
                    sep.add_css_class(widget::TASKBAR_OUTPUT_SEPARATOR);
                    container.append(&sep);
                } else if let (Some(prev), Some(cur)) = (prev_workspace, window.workspace_id)
                    && prev != cur
                {
                    let sep = GtkBox::new(Orientation::Vertical, 0);
                    sep.add_css_class(widget::TASKBAR_SEPARATOR);
                    container.append(&sep);
                }

                prev_workspace = window.workspace_id;
                prev_output = window.output.as_deref();
            }

            let button = create_window_button(window, config, button_css, effective_icon);
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

/// The window's display title: title if non-empty, otherwise app_id.
fn window_display_title(window: &crate::services::compositor::Window) -> &str {
    if window.title.is_empty() {
        &window.app_id
    } else {
        &window.title
    }
}

/// Tooltip string for a window button: "app_id - title", or just app_id.
fn window_tooltip(window: &crate::services::compositor::Window) -> String {
    if window.title.is_empty() {
        window.app_id.clone()
    } else {
        format!("{} - {}", window.app_id, window.title)
    }
}

/// Compute the padding used around each taskbar button icon.
/// Returns `(effective_icon, pad)`.
fn compute_button_padding(icon_size: i32, max_button_size: i32) -> (i32, i32) {
    let min_pad = 3;
    let effective_icon = icon_size.min(max_button_size - 2 * min_pad);
    let ideal_pad = effective_icon / 4;
    let available = ((max_button_size - effective_icon) / 2).max(0);
    let pad = ideal_pad.min(available).max(min_pad);
    (effective_icon, pad)
}

fn create_window_button(
    window: &crate::services::compositor::Window,
    config: &TaskbarConfig,
    button_css: &CssProvider,
    effective_icon: i32,
) -> Widget {
    let button = GtkBox::new(Orientation::Horizontal, 4);
    button.add_css_class(widget::TASKBAR_BUTTON);
    button.add_css_class(state::CLICKABLE);
    button.set_valign(Align::Center);

    #[allow(deprecated)]
    button
        .style_context()
        .add_provider(button_css, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);

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
        let label = Label::new(Some(window_display_title(window)));
        label.add_css_class(widget::TASKBAR_LABEL);
        label.set_single_line_mode(true);
        label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        label.set_max_width_chars(20);
        button.append(&label);
    }

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

    TooltipManager::global().set_styled_tooltip(&button, &window_tooltip(window));

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

    TooltipManager::global().set_styled_tooltip(button, &window_tooltip(window));

    // Update the label text if present (title may have changed)
    if let Some(container) = button.downcast_ref::<GtkBox>() {
        let mut next = container.first_child();
        while let Some(child_widget) = next {
            if let Some(label) = child_widget.downcast_ref::<Label>() {
                label.set_label(window_display_title(window));
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
        assert_eq!(config.icon_size, None); // resolved to theme default in new()
        assert!(config.show_active);
        assert!(config.show_workspace_separator);
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
        options.insert(
            "show_workspace_separator".to_string(),
            Value::Boolean(false),
        );

        let entry = make_widget_entry("taskbar", options);
        let config = TaskbarConfig::from_entry(&entry);
        assert!(!config.show_title);
        assert!(!config.show_icon);
        assert_eq!(config.max_windows, 5);
        assert!(!config.filter_by_output);
        assert_eq!(config.icon_size, Some(24));
        assert!(!config.show_active);
        assert!(!config.show_workspace_separator);
    }

    #[test]
    fn test_taskbar_config_icon_size_min_clamp() {
        let mut options = HashMap::new();
        options.insert("icon_size".to_string(), Value::Integer(2));

        let entry = make_widget_entry("taskbar", options);
        let config = TaskbarConfig::from_entry(&entry);
        assert_eq!(config.icon_size, Some(8));
    }
}
