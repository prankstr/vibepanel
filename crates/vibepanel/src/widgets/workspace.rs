//! Workspace widget - displays workspace indicators.
//!
//! Shows occupied/active workspaces with visual indicators and CSS classes.
//! Clicking on a workspace indicator switches to that workspace.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk4::gdk::BUTTON_PRIMARY;
use gtk4::pango::EllipsizeMode;
use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, GestureClick, Label};
use tracing::{debug, trace};
use vibepanel_core::config::WidgetEntry;

use crate::services::tooltip::TooltipManager;
use crate::services::workspace::{Workspace, WorkspaceService, WorkspaceServiceSnapshot};
use crate::styles::{state, widget};
use crate::widgets::WidgetConfig;
use crate::widgets::base::BaseWidget;
use crate::widgets::warn_unknown_options;

/// Label type for workspace indicators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelType {
    /// Show icon glyphs (●, ○, ◆).
    Icons,
    /// Show workspace numbers/names.
    Numbers,
    /// Minimal - no text, just CSS styling.
    None,
}

impl LabelType {
    fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "numbers" => LabelType::Numbers,
            "none" => LabelType::None,
            // Default to Icons for any other value including "icons"
            _ => LabelType::Icons,
        }
    }
}

const DEFAULT_LABEL_TYPE: LabelType = LabelType::None;
const DEFAULT_SEPARATOR: &str = "";

/// Configuration for the workspace widget.
#[derive(Debug, Clone)]
pub struct WorkspaceConfig {
    /// How to display workspace labels.
    pub label_type: LabelType,
    /// Separator string between workspace indicators.
    pub separator: String,
    /// Custom background color for this widget.
    pub color: Option<String>,
}

impl WidgetConfig for WorkspaceConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options("workspace", entry, &["label_type", "separator"]);

        let label_type = entry
            .options
            .get("label_type")
            .and_then(|v| v.as_str())
            .map(LabelType::from_str)
            .unwrap_or(DEFAULT_LABEL_TYPE);

        let separator = entry
            .options
            .get("separator")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_SEPARATOR)
            .to_string();

        Self {
            label_type,
            separator,
            color: entry.color.clone(),
        }
    }
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            label_type: DEFAULT_LABEL_TYPE,
            separator: DEFAULT_SEPARATOR.to_string(),
            color: None,
        }
    }
}

/// Workspace widget that displays workspace indicators.
pub struct WorkspaceWidget {
    /// Shared base widget container.
    base: BaseWidget,
}

impl WorkspaceWidget {
    /// Create a new workspace widget with the given configuration.
    ///
    /// # Arguments
    ///
    /// * `config` - Widget configuration (label type, separator).
    /// * `output_id` - Optional output/monitor name. When set, the widget will:
    ///   - For Niri: only show workspaces belonging to this output.
    ///   - For MangoWC: show all workspaces but with per-output window counts.
    ///   - For Hyprland: ignored (global workspace view).
    pub fn new(config: WorkspaceConfig, output_id: Option<String>) -> Self {
        let base = BaseWidget::new(&[widget::WORKSPACE], config.color);

        // Use the content box provided by BaseWidget
        let workspace_container = base.content().clone();

        // State shared with the callback (callback owns these via Rc).
        let workspace_labels = Rc::new(RefCell::new(HashMap::new()));
        let current_ids = Rc::new(RefCell::new(Vec::new()));
        let label_type = config.label_type;
        let separator = config.separator;

        // Clone output_id for the debug message
        let output_id_debug = output_id.clone();

        // Connect to workspace service.
        // The callback owns its own Rc clones of the state.
        WorkspaceService::global().connect(move |snapshot| {
            update_indicators(
                &workspace_container,
                &workspace_labels,
                &current_ids,
                label_type,
                &separator,
                snapshot,
                output_id.as_deref(),
            );
        });

        debug!("WorkspaceWidget created (output_id: {:?})", output_id_debug);
        Self { base }
    }

    /// Get the root GTK widget for embedding in the bar.
    pub fn widget(&self) -> &GtkBox {
        self.base.widget()
    }
}

/// Icon glyphs for workspace indicators.
const ICON_OCCUPIED: &str = "●";
const ICON_EMPTY: &str = "○";
const ICON_ACTIVE: &str = "◆";

/// Clear all workspace indicator widgets from the container.
fn clear_indicators(
    container: &GtkBox,
    labels: &Rc<RefCell<HashMap<i32, Label>>>,
    ids: &Rc<RefCell<Vec<i32>>>,
) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
    labels.borrow_mut().clear();
    ids.borrow_mut().clear();
}

/// Create workspace indicator labels for the given workspaces.
fn create_indicators(
    container: &GtkBox,
    labels_cell: &Rc<RefCell<HashMap<i32, Label>>>,
    ids_cell: &Rc<RefCell<Vec<i32>>>,
    label_type: LabelType,
    separator: &str,
    workspaces: &[Workspace],
) {
    clear_indicators(container, labels_cell, ids_cell);

    let mut labels = labels_cell.borrow_mut();
    let mut ids = ids_cell.borrow_mut();

    for (i, workspace) in workspaces.iter().enumerate() {
        let label_text = match label_type {
            LabelType::Icons => ICON_EMPTY,
            LabelType::Numbers => &workspace.name,
            LabelType::None => "",
        };

        let label = Label::new(Some(label_text));
        label.add_css_class(widget::WORKSPACE_INDICATOR);
        label.add_css_class(state::CLICKABLE);
        label.set_valign(Align::Center);
        label.set_xalign(0.5);
        label.set_ellipsize(EllipsizeMode::End);
        label.set_single_line_mode(true);

        if label_type == LabelType::None {
            label.add_css_class(widget::WORKSPACE_INDICATOR_MINIMAL);
        }

        // Add click handler to switch workspace
        let workspace_id = workspace.id;
        let gesture = GestureClick::new();
        gesture.set_button(BUTTON_PRIMARY);
        gesture.connect_released(move |gesture, _n_press, _x, _y| {
            if gesture.current_button() != BUTTON_PRIMARY {
                return;
            }
            debug!("Switching to workspace {}", workspace_id);
            WorkspaceService::global().switch_workspace(workspace_id);
        });
        label.add_controller(gesture);

        labels.insert(workspace.id, label.clone());
        container.append(&label);
        ids.push(workspace.id);

        // Add separator if not the last workspace
        if i < workspaces.len() - 1 && !separator.is_empty() {
            let sep = Label::new(Some(separator));
            sep.set_valign(Align::Center);
            sep.add_css_class(widget::WORKSPACE_SEPARATOR);
            container.append(&sep);
        }
    }
}

/// Update workspace indicators based on the current snapshot.
///
/// When `output_id` is provided:
/// - Uses per-output workspace data if available.
/// - For Niri: shows only workspaces belonging to this output.
/// - For MangoWC: shows all workspaces with per-output window counts.
fn update_indicators(
    container: &GtkBox,
    labels_cell: &Rc<RefCell<HashMap<i32, Label>>>,
    ids_cell: &Rc<RefCell<Vec<i32>>>,
    label_type: LabelType,
    separator: &str,
    snapshot: &WorkspaceServiceSnapshot,
    output_id: Option<&str>,
) {
    // Get the workspace list to use - either per-output or global
    let (workspaces, active_workspace, source): (&[Workspace], Option<i32>, &str) = if let Some(
        output,
    ) = output_id
    {
        if let Some(per_output) = snapshot.per_output.get(output) {
            (
                &per_output.workspaces,
                per_output.active_workspace,
                "per_output",
            )
        } else {
            // No per-output data available, fall back to global
            debug!(
                "workspace widget: output_id={:?} not found in per_output keys={:?}, using global",
                output,
                snapshot.per_output.keys().collect::<Vec<_>>()
            );
            (
                &snapshot.workspaces,
                snapshot.active_workspace,
                "global_fallback",
            )
        }
    } else {
        // No output_id specified, use global data
        (&snapshot.workspaces, snapshot.active_workspace, "global")
    };

    trace!(
        "workspace widget: source={}, output_id={:?}, active_workspace={:?}",
        source, output_id, active_workspace
    );

    // Determine which workspaces to display (occupied + active)
    // Use the workspace's own occupied flag (which reflects per-output state if available)
    let mut display_ids: std::collections::HashSet<i32> = workspaces
        .iter()
        .filter(|ws| ws.occupied)
        .map(|ws| ws.id)
        .collect();

    trace!(
        "workspace widget: occupied_ids={:?}, adding active={:?}",
        display_ids, active_workspace
    );

    if let Some(active) = active_workspace {
        display_ids.insert(active);
    }

    // Filter to only display relevant workspaces
    let display_workspaces: Vec<_> = workspaces
        .iter()
        .filter(|ws| display_ids.contains(&ws.id))
        .cloned()
        .collect();

    trace!(
        "workspace widget: display_ids={:?}, display_workspaces={:?}",
        display_ids,
        display_workspaces
            .iter()
            .map(|ws| (ws.id, ws.active, ws.occupied))
            .collect::<Vec<_>>()
    );

    if display_workspaces.is_empty() {
        let current_ids = ids_cell.borrow();
        if !current_ids.is_empty() {
            drop(current_ids);
            clear_indicators(container, labels_cell, ids_cell);
        }
        return;
    }

    // Check if we need to recreate indicators
    let new_ids: Vec<i32> = display_workspaces.iter().map(|ws| ws.id).collect();
    if new_ids != *ids_cell.borrow() {
        create_indicators(
            container,
            labels_cell,
            ids_cell,
            label_type,
            separator,
            &display_workspaces,
        );
    }

    // Update indicator styling
    let labels = labels_cell.borrow();
    for workspace in &display_workspaces {
        let Some(label) = labels.get(&workspace.id) else {
            continue;
        };

        // Remove existing state classes
        label.remove_css_class(widget::ACTIVE);
        label.remove_css_class(state::OCCUPIED);
        label.remove_css_class(state::URGENT);

        // Update icon text if using icons
        if label_type == LabelType::Icons {
            if workspace.active {
                label.set_text(ICON_ACTIVE);
            } else if workspace.occupied {
                label.set_text(ICON_OCCUPIED);
            } else {
                label.set_text(ICON_EMPTY);
            }
        } else if label_type == LabelType::Numbers {
            label.set_text(&workspace.name);
        }

        // Add appropriate state class (mutually exclusive)
        if workspace.active {
            label.add_css_class(widget::ACTIVE);
        } else if workspace.occupied {
            label.add_css_class(state::OCCUPIED);
        } else if workspace.urgent {
            label.add_css_class(state::URGENT);
        }

        // Set tooltip with workspace info
        let tooltip_text = build_tooltip(workspace);
        TooltipManager::global().set_styled_tooltip(label, &tooltip_text);
    }
}

/// Build tooltip text for a workspace.
fn build_tooltip(workspace: &Workspace) -> String {
    let mut parts = Vec::new();

    // Workspace identifier - show both ID and name if they differ
    // (Niri can have custom workspace names separate from the index)
    let id_str = workspace.id.to_string();
    if workspace.name != id_str {
        // Custom name - show "Workspace N: Name"
        parts.push(format!("Workspace {}: {}", workspace.id, workspace.name));
    } else {
        // No custom name - just show "Workspace N"
        parts.push(format!("Workspace {}", workspace.name));
    }

    // State
    if workspace.active {
        parts.push("Active".to_string());
    } else if workspace.urgent {
        parts.push("Urgent".to_string());
    }

    // Window count
    if let Some(count) = workspace.window_count {
        let windows_str = if count == 1 { "window" } else { "windows" };
        parts.push(format!("{} {}", count, windows_str));
    } else if workspace.occupied {
        parts.push("Has windows".to_string());
    } else {
        parts.push("Empty".to_string());
    }

    parts.join(" • ")
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
            color: None,
        }
    }

    #[test]
    fn test_workspace_config_default() {
        let entry = make_widget_entry("workspace", HashMap::new());
        let config = WorkspaceConfig::from_entry(&entry);
        assert_eq!(config.label_type, LabelType::None);
        assert_eq!(config.separator, "");
    }

    #[test]
    fn test_workspace_config_numbers() {
        let mut options = HashMap::new();
        options.insert(
            "label_type".to_string(),
            Value::String("numbers".to_string()),
        );
        options.insert("separator".to_string(), Value::String("|".to_string()));
        let entry = make_widget_entry("workspace", options);
        let config = WorkspaceConfig::from_entry(&entry);
        assert_eq!(config.label_type, LabelType::Numbers);
        assert_eq!(config.separator, "|");
    }

    #[test]
    fn test_workspace_config_none() {
        let mut options = HashMap::new();
        options.insert("label_type".to_string(), Value::String("none".to_string()));
        let entry = make_widget_entry("workspace", options);
        let config = WorkspaceConfig::from_entry(&entry);
        assert_eq!(config.label_type, LabelType::None);
    }

    #[test]
    fn test_label_type_from_str() {
        assert_eq!(LabelType::from_str("icons"), LabelType::Icons);
        assert_eq!(LabelType::from_str("ICONS"), LabelType::Icons);
        assert_eq!(LabelType::from_str("numbers"), LabelType::Numbers);
        assert_eq!(LabelType::from_str("none"), LabelType::None);
        assert_eq!(LabelType::from_str("unknown"), LabelType::Icons); // default
    }
}
