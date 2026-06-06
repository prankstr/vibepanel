//! Taskbar widget - displays a list of all windows.
//!
//! Shows all open windows as clickable buttons with app icons.
//! Clicking a window button focuses that window.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gtk4::gdk::BUTTON_PRIMARY;
use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, CssProvider, GestureClick, Image, Label, Orientation, Widget};
use tracing::{debug, warn};
use vibepanel_core::config::WidgetEntry;

use crate::services::callbacks::CallbackId;
use crate::services::compositor::{CompositorManager, WorkspaceMeta, WorkspaceSnapshot};
use crate::services::config_manager::ConfigManager;
use crate::services::icons::get_app_icon_name;
use crate::services::tooltip::TooltipManager;
use crate::services::window_list::WindowListService;
use crate::styles::{icon, state, widget};
use crate::widgets::WidgetConfig;
use crate::widgets::base::BaseWidget;
use crate::widgets::warn_unknown_options;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceSeparatorLabel {
    None,
    Number,
}

impl WorkspaceSeparatorLabel {
    fn from_str(value: &str) -> Self {
        match value.to_lowercase().as_str() {
            "none" => Self::None,
            "number" | "numbers" => Self::Number,
            other => {
                warn!(
                    "unknown taskbar workspace_separator_label {:?}, falling back to {:?}",
                    other,
                    Self::None
                );
                Self::None
            }
        }
    }
}

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
    /// Optional workspace identity rendered inside workspace separators.
    pub workspace_separator_label: WorkspaceSeparatorLabel,
    /// Whether to show visible scratchpad windows (MangoWC) in the taskbar.
    /// Dismissed scratchpads have no tags and are never shown.
    pub show_scratchpad_windows: bool,
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
                "workspace_separator_label",
                "show_scratchpad_windows",
            ],
        );

        let defaults = Self::default();

        let show_workspace_separator = entry
            .options
            .get("show_workspace_separator")
            .and_then(|v| v.as_bool())
            .unwrap_or(defaults.show_workspace_separator);
        let workspace_separator_label = entry
            .options
            .get("workspace_separator_label")
            .and_then(|v| v.as_str())
            .map(WorkspaceSeparatorLabel::from_str)
            .unwrap_or(defaults.workspace_separator_label);

        if !show_workspace_separator && workspace_separator_label != WorkspaceSeparatorLabel::None {
            warn!(
                "taskbar workspace_separator_label is ignored when show_workspace_separator is false"
            );
        }

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
            show_workspace_separator,
            workspace_separator_label,
            show_scratchpad_windows: entry
                .options
                .get("show_scratchpad_windows")
                .and_then(|v| v.as_bool())
                .unwrap_or(defaults.show_scratchpad_windows),
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
            workspace_separator_label: WorkspaceSeparatorLabel::None,
            show_scratchpad_windows: true,
        }
    }
}

/// Window identity snapshot used to detect when the button list needs a full rebuild.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct WindowListKey {
    windows: Vec<WindowEntryKey>,
    active_workspaces: Vec<i32>,
    urgent_workspaces: Vec<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WindowEntryKey {
    id: u64,
    workspace_id: Option<i32>,
    output: Option<String>,
    separator_label: Option<String>,
}

// Visual-state priority: active > urgent. Window data may have both states;
// the rendered taskbar button intentionally picks one visual class.
fn taskbar_button_state_class(
    window: &crate::services::compositor::Window,
    config: &TaskbarConfig,
) -> Option<&'static str> {
    if config.show_active && window.is_focused {
        Some(widget::ACTIVE)
    } else if window.is_urgent {
        Some(state::URGENT)
    } else {
        None
    }
}

fn sync_taskbar_button_state(button: &Widget, target_class: Option<&str>) {
    for &cls in &[widget::ACTIVE, state::URGENT] {
        if Some(cls) == target_class {
            if !button.has_css_class(cls) {
                button.add_css_class(cls);
            }
        } else if button.has_css_class(cls) {
            button.remove_css_class(cls);
        }
    }
}

/// Taskbar widget that displays all windows as clickable buttons.
pub struct TaskbarWidget {
    base: BaseWidget,
    window_list_callback_id: CallbackId,
}

impl TaskbarWidget {
    pub fn new(mut config: TaskbarConfig, output_id: Option<String>) -> Self {
        let layout = TaskbarLayout::from_config_manager();
        let is_vertical = ConfigManager::global().bar_position().is_vertical();

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
        if is_vertical {
            content.set_halign(Align::Fill);
            content.set_hexpand(true);
        }

        // Compute per-instance button padding (the space inside each button,
        // around the icon). `pad` is baked into the calc expressions below.
        // Taskbar spacing below derives from the raw widget spacing tokens plus
        // user density offsets, while still subtracting per-button padding.
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

        // Per-instance variables defined as calc expressions against the
        // raw theme spacing tokens so public offsets scoped to `.taskbar`
        // inherit into `.content` before the effective taskbar variables are
        // calculated. max(0px, ...) guards against user density offsets that
        // would push the result negative.
        let two_pad = 2 * pad;
        let (theme_padding_var, theme_gap_var) = if is_vertical {
            (
                "--vp-widget-padding-cross-base",
                "--vp-widget-gap-cross-base",
            )
        } else {
            ("--vp-widget-padding-flow-base", "--vp-widget-gap-flow-base")
        };
        let content_css = CssProvider::new();
        content_css.load_from_string(&format!(
            ".taskbar .content {{ \
               --vp-taskbar-widget-content-padding: max(0px, calc(var({theme_padding_var}) + var(--widget-padding-adjust, 0px))); \
               --vp-taskbar-widget-content-gap: max(0px, calc(var({theme_gap_var}) + var(--widget-gap-adjust, 0px))); \
               --vp-taskbar-content-edge: max(0px, calc(var(--vp-taskbar-widget-content-padding) - {pad}px)); \
               --vp-taskbar-button-gap: max(0px, calc(var(--vp-taskbar-widget-content-gap) - {two_pad}px)); \
               --vp-taskbar-separator-gap: max(0px, calc(var(--vp-taskbar-widget-content-gap) - {pad}px - 2px)); \
            }}",
        ));
        #[allow(deprecated)]
        content
            .style_context()
            .add_provider(&content_css, gtk4::STYLE_PROVIDER_PRIORITY_USER + 10);

        let window_buttons: Rc<RefCell<HashMap<u64, Widget>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let current_window_ids: Rc<RefCell<WindowListKey>> =
            Rc::new(RefCell::new(WindowListKey::default()));
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
                is_vertical,
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

fn workspace_separator_labels() -> HashMap<i32, String> {
    // Grouping uses stable workspace IDs, but labels mirror the workspaces widget:
    // display idx when meaningful, otherwise the workspace name.
    CompositorManager::global()
        .list_workspaces()
        .into_iter()
        .map(|workspace| (workspace.id, workspace_meta_label(workspace)))
        .collect()
}

fn workspace_meta_label(workspace: WorkspaceMeta) -> String {
    if workspace.idx >= 0 {
        workspace.idx.to_string()
    } else {
        workspace.name
    }
}

fn workspace_separator_label(workspace_id: i32, workspace_labels: &HashMap<i32, String>) -> String {
    workspace_labels
        .get(&workspace_id)
        .cloned()
        .unwrap_or_else(|| workspace_id.to_string())
}

fn taskbar_active_workspaces(
    snapshot: &WorkspaceSnapshot,
    output_id: Option<&str>,
    filter_by_output: bool,
) -> HashSet<i32> {
    if filter_by_output
        && let Some(output_id) = output_id
        && let Some(per_output) = snapshot.per_output.get(output_id)
    {
        return per_output.active_workspace.clone();
    }

    snapshot.active_workspace.clone()
}

#[allow(clippy::too_many_arguments)]
fn update_window_buttons(
    container: &GtkBox,
    buttons: &Rc<RefCell<HashMap<u64, Widget>>>,
    current_ids: &Rc<RefCell<WindowListKey>>,
    snapshot: &crate::services::compositor::WindowListSnapshot,
    config: &TaskbarConfig,
    button_css: &CssProvider,
    effective_icon: i32,
    is_vertical: bool,
    output_id: Option<&str>,
) {
    let mut windows: Vec<_> = snapshot
        .windows
        .iter()
        .filter(|win| {
            if !config.show_scratchpad_windows && win.is_scratchpad {
                return false;
            }
            if !config.filter_by_output || output_id.is_none() {
                return true;
            }
            win.output.as_deref() == output_id || win.output.is_none()
        })
        .cloned()
        .collect();

    // When max_windows is set, preserve the focused window first. If focus is
    // already visible, preserve one urgent window so attention requests show up.
    if config.max_windows > 0 && windows.len() > config.max_windows {
        if let Some(focused_idx) = windows.iter().position(|w| w.is_focused)
            && focused_idx >= config.max_windows
        {
            windows.swap(config.max_windows - 1, focused_idx);
        } else if let Some(urgent_idx) = windows.iter().position(|w| w.is_urgent)
            && urgent_idx >= config.max_windows
        {
            windows.swap(config.max_windows - 1, urgent_idx);
        }
        windows.truncate(config.max_windows);
    }

    let track_active_separator = config.show_workspace_separator
        && config.workspace_separator_label != WorkspaceSeparatorLabel::None;
    let (
        workspace_labels,
        active_workspaces,
        urgent_workspaces,
        active_workspace_key,
        urgent_workspace_key,
    ) = if track_active_separator {
        let workspace_snapshot = CompositorManager::global().get_workspace_snapshot();
        let active_workspaces =
            taskbar_active_workspaces(&workspace_snapshot, output_id, config.filter_by_output);
        let urgent_workspaces = workspace_snapshot.urgent_workspaces;

        let mut active_workspace_key: Vec<_> = active_workspaces.iter().copied().collect();
        active_workspace_key.sort_unstable();
        let mut urgent_workspace_key: Vec<_> = urgent_workspaces.iter().copied().collect();
        urgent_workspace_key.sort_unstable();

        (
            Some(workspace_separator_labels()),
            active_workspaces,
            urgent_workspaces,
            active_workspace_key,
            urgent_workspace_key,
        )
    } else {
        (None, HashSet::new(), HashSet::new(), Vec::new(), Vec::new())
    };
    let new_ids = WindowListKey {
        windows: windows
            .iter()
            .map(|w| WindowEntryKey {
                id: w.id,
                workspace_id: w.workspace_id,
                output: w.output.clone(),
                separator_label: w.workspace_id.and_then(|id| {
                    workspace_labels
                        .as_ref()
                        .map(|labels| workspace_separator_label(id, labels))
                }),
            })
            .collect(),
        active_workspaces: active_workspace_key,
        urgent_workspaces: urgent_workspace_key,
    };

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
                    let sep = separator_widget(
                        is_vertical,
                        WorkspaceSeparatorLabel::None,
                        None,
                        false,
                        false,
                    );
                    sep.add_css_class(widget::TASKBAR_OUTPUT_SEPARATOR);
                    container.append(&sep);

                    if !config.filter_by_output
                        && config.workspace_separator_label != WorkspaceSeparatorLabel::None
                        && let Some(cur) = window.workspace_id
                    {
                        let label = workspace_labels
                            .as_ref()
                            .map(|labels| workspace_separator_label(cur, labels));
                        let sep = separator_widget(
                            is_vertical,
                            config.workspace_separator_label,
                            label.as_deref(),
                            active_workspaces.contains(&cur),
                            urgent_workspaces.contains(&cur),
                        );
                        container.append(&sep);
                    }
                } else if let (Some(prev), Some(cur)) = (prev_workspace, window.workspace_id)
                    && prev != cur
                {
                    let label = workspace_labels
                        .as_ref()
                        .map(|labels| workspace_separator_label(cur, labels));
                    let sep = separator_widget(
                        is_vertical,
                        config.workspace_separator_label,
                        label.as_deref(),
                        active_workspaces.contains(&cur),
                        urgent_workspaces.contains(&cur),
                    );
                    container.append(&sep);
                } else if prev_workspace.is_none()
                    && config.workspace_separator_label != WorkspaceSeparatorLabel::None
                    && let Some(cur) = window.workspace_id
                {
                    // Label the first workspace-bearing group, even after ungrouped windows.
                    let label = workspace_labels
                        .as_ref()
                        .map(|labels| workspace_separator_label(cur, labels));
                    let sep = separator_widget(
                        is_vertical,
                        config.workspace_separator_label,
                        label.as_deref(),
                        active_workspaces.contains(&cur),
                        urgent_workspaces.contains(&cur),
                    );
                    container.append(&sep);
                }

                prev_workspace = window.workspace_id;
                prev_output = window.output.as_deref();
            }

            let button =
                create_window_button(window, config, button_css, effective_icon, is_vertical);
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
    let min_pad = 2;
    let effective_icon = icon_size.min(max_button_size - 2 * min_pad);
    let ideal_pad = effective_icon / 4;
    let available = ((max_button_size - effective_icon) / 2).max(0);
    let pad = ideal_pad.min(available).saturating_sub(1).max(min_pad);
    (effective_icon, pad)
}

fn separator_widget(
    is_vertical: bool,
    label_type: WorkspaceSeparatorLabel,
    label_text: Option<&str>,
    is_active_workspace: bool,
    is_urgent_workspace: bool,
) -> GtkBox {
    let wrapper = GtkBox::new(
        if is_vertical {
            Orientation::Vertical
        } else {
            Orientation::Horizontal
        },
        0,
    );
    wrapper.add_css_class(widget::TASKBAR_SEPARATOR);
    if label_type != WorkspaceSeparatorLabel::None {
        if is_active_workspace {
            wrapper.add_css_class(widget::TASKBAR_SEPARATOR_ACTIVE);
        } else if is_urgent_workspace {
            wrapper.add_css_class(widget::TASKBAR_SEPARATOR_URGENT);
        }
    }

    if let Some(label_text) = label_text {
        match label_type {
            WorkspaceSeparatorLabel::None => {}
            WorkspaceSeparatorLabel::Number => {
                wrapper.add_css_class(widget::TASKBAR_SEPARATOR_HAS_LABEL);
                let label = Label::new(Some(label_text));
                label.add_css_class(widget::TASKBAR_SEPARATOR_LABEL);
                label.set_halign(Align::Center);
                label.set_valign(Align::Center);
                wrapper.append(&label);
            }
        }
    }

    if is_vertical {
        wrapper.set_halign(Align::Fill);
        wrapper.set_hexpand(true);
    }

    let line = GtkBox::new(
        if is_vertical {
            Orientation::Horizontal
        } else {
            Orientation::Vertical
        },
        0,
    );
    line.add_css_class("taskbar-separator-line");

    if is_vertical {
        line.set_halign(Align::Fill);
        line.set_hexpand(true);
        line.set_valign(Align::Center);
    } else {
        line.set_halign(Align::Center);
        line.set_valign(Align::Fill);
        line.set_vexpand(true);
    }

    wrapper.append(&line);
    wrapper
}

fn create_window_button(
    window: &crate::services::compositor::Window,
    config: &TaskbarConfig,
    button_css: &CssProvider,
    effective_icon: i32,
    is_vertical: bool,
) -> Widget {
    let button = GtkBox::new(
        if is_vertical {
            Orientation::Vertical
        } else {
            Orientation::Horizontal
        },
        4,
    );
    button.add_css_class(widget::TASKBAR_BUTTON);
    button.add_css_class(state::CLICKABLE);
    button.set_valign(Align::Center);
    if is_vertical {
        button.set_halign(Align::Center);
    }

    #[allow(deprecated)]
    button
        .style_context()
        .add_provider(button_css, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);

    sync_taskbar_button_state(
        button.upcast_ref(),
        taskbar_button_state_class(window, config),
    );

    if config.show_icon {
        let icon_name = get_app_icon_name(&window.app_id);
        let icon = Image::from_icon_name(&icon_name);
        icon.add_css_class(icon::TEXT);
        icon.add_css_class(widget::TASKBAR_ICON);
        icon.set_valign(Align::Center);
        icon.set_halign(Align::Center);

        icon.set_pixel_size(effective_icon);

        button.append(&icon);
    }

    if config.show_title && !is_vertical {
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
    sync_taskbar_button_state(button, taskbar_button_state_class(window, config));

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

    fn make_workspace_meta(id: i32, idx: i32, name: &str) -> WorkspaceMeta {
        WorkspaceMeta {
            id,
            idx,
            name: name.to_string(),
            output: None,
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
        assert_eq!(
            config.workspace_separator_label,
            WorkspaceSeparatorLabel::None
        );
        assert!(config.show_scratchpad_windows);
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
        options.insert(
            "workspace_separator_label".to_string(),
            Value::String("number".to_string()),
        );
        options.insert("show_scratchpad_windows".to_string(), Value::Boolean(false));

        let entry = make_widget_entry("taskbar", options);
        let config = TaskbarConfig::from_entry(&entry);
        assert!(!config.show_title);
        assert!(!config.show_icon);
        assert_eq!(config.max_windows, 5);
        assert!(!config.filter_by_output);
        assert_eq!(config.icon_size, Some(24));
        assert!(!config.show_active);
        assert!(!config.show_workspace_separator);
        assert_eq!(
            config.workspace_separator_label,
            WorkspaceSeparatorLabel::Number
        );
        assert!(!config.show_scratchpad_windows);
    }

    #[test]
    fn test_taskbar_config_icon_size_min_clamp() {
        let mut options = HashMap::new();
        options.insert("icon_size".to_string(), Value::Integer(2));

        let entry = make_widget_entry("taskbar", options);
        let config = TaskbarConfig::from_entry(&entry);
        assert_eq!(config.icon_size, Some(8));
    }

    #[test]
    fn test_workspace_display_label_uses_idx_with_name_fallback() {
        assert_eq!(workspace_meta_label(make_workspace_meta(42, 3, "web")), "3");
        assert_eq!(
            workspace_meta_label(make_workspace_meta(42, -1, "web")),
            "web"
        );
    }
}
