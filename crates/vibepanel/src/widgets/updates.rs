//! Updates widget - displays available package updates in the bar.
//!
//! This widget:
//! - Shows an icon and count when updates are available
//! - Hides itself when there are no updates (and no errors)
//! - Shows "!" when there's an error checking for updates
//! - Opens a terminal with the upgrade command on click
//!
//! Configuration options:
//! - `check_interval`: How often to check for updates (seconds, default: 3600)
//! - `terminal`: Override terminal emulator detection

use gtk4::prelude::*;
use gtk4::{GestureClick, Label};
use vibepanel_core::config::WidgetEntry;

use crate::services::icons::IconHandle;
use crate::services::tooltip::TooltipManager;
use crate::services::updates::{UpdatesService, UpdatesSnapshot};
use crate::styles::{class, widget};
use crate::widgets::base::BaseWidget;
use crate::widgets::updates_common::{format_tooltip, icon_for_state, spawn_upgrade_terminal};
use crate::widgets::{WidgetConfig, warn_unknown_options};

const DEFAULT_CHECK_INTERVAL: u64 = 3600;

/// Configuration for the updates widget.
#[derive(Debug, Clone)]
pub struct UpdatesConfig {
    /// How often to check for updates (seconds).
    pub check_interval: u64,
    /// Override terminal emulator detection.
    pub terminal: Option<String>,
}

impl WidgetConfig for UpdatesConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options("updates", entry, &["check_interval", "terminal"]);

        let check_interval = entry
            .options
            .get("check_interval")
            .and_then(|v| v.as_integer())
            .map(|v| v as u64)
            .unwrap_or(DEFAULT_CHECK_INTERVAL);

        let terminal = entry
            .options
            .get("terminal")
            .and_then(|v| v.as_str())
            .map(String::from);

        Self {
            check_interval,
            terminal,
        }
    }
}

impl Default for UpdatesConfig {
    fn default() -> Self {
        Self {
            check_interval: DEFAULT_CHECK_INTERVAL,
            terminal: None,
        }
    }
}

/// Updates widget that displays available package updates.
pub struct UpdatesWidget {
    /// Shared base widget container.
    base: BaseWidget,
    /// Icon handle for the update icon.
    icon_handle: IconHandle,
    /// Label showing update count or "!".
    count_label: Label,
    /// Terminal override from config.
    terminal: Option<String>,
}

impl UpdatesWidget {
    /// Create a new updates widget with the given configuration.
    pub fn new(config: UpdatesConfig) -> Self {
        let base = BaseWidget::new(&[widget::UPDATES]);
        base.set_tooltip("Updates: checking...");

        let icon_handle = base.add_icon("software-update-available", &[widget::UPDATES_ICON]);
        let count_label = base.add_label(None, &[widget::UPDATES_COUNT, class::VCENTER_CAPS]);

        // Configure the service with our interval
        let service = UpdatesService::global();
        service.set_check_interval(config.check_interval);

        let widget = Self {
            base,
            icon_handle,
            count_label,
            terminal: config.terminal,
        };

        // Set up click handler to spawn terminal
        {
            let terminal = widget.terminal.clone();
            let container = widget.base.widget().clone();

            let click = GestureClick::new();
            click.connect_released(move |_, _, _, _| {
                let snapshot = UpdatesService::global().snapshot();
                if let Some(pm) = snapshot.package_manager
                    && let Err(e) = spawn_upgrade_terminal(pm, terminal.as_deref())
                {
                    tracing::error!("Failed to spawn upgrade terminal: {}", e);
                }
            });
            container.add_controller(click);
        }

        // Subscribe to updates service
        {
            let container = widget.base.widget().clone();
            let icon_handle = widget.icon_handle.clone();
            let count_label = widget.count_label.clone();

            service.connect(move |snapshot: &UpdatesSnapshot| {
                update_widget_from_snapshot(&container, &icon_handle, &count_label, snapshot);
            });
        }

        widget
    }

    /// Get the root GTK widget for embedding in the bar.
    pub fn widget(&self) -> &gtk4::Box {
        self.base.widget()
    }
}

/// Update the widget's visual state from a snapshot.
fn update_widget_from_snapshot(
    container: &gtk4::Box,
    icon_handle: &IconHandle,
    count_label: &Label,
    snapshot: &UpdatesSnapshot,
) {
    // Handle unavailable state (no package manager)
    if !snapshot.available {
        container.set_visible(false);
        return;
    }

    // Determine visibility: show only if updates available OR error
    let should_show = snapshot.update_count > 0 || snapshot.error.is_some();
    container.set_visible(should_show);

    if !should_show {
        return;
    }

    // Update CSS classes
    container.remove_css_class(widget::UPDATES_ERROR);
    container.remove_css_class(widget::UPDATES_CHECKING);
    icon_handle.remove_css_class(widget::UPDATES_ERROR);

    if snapshot.error.is_some() {
        container.add_css_class(widget::UPDATES_ERROR);
        icon_handle.add_css_class(widget::UPDATES_ERROR);
    } else if snapshot.checking {
        container.add_css_class(widget::UPDATES_CHECKING);
    }

    // Update icon
    let icon_name = icon_for_state(snapshot);
    icon_handle.set_icon(icon_name);

    // Update label: show "!" for error, count otherwise
    if snapshot.error.is_some() {
        count_label.set_label("!");
    } else {
        count_label.set_label(&snapshot.update_count.to_string());
    }

    // Update tooltip
    let tooltip = format_tooltip(snapshot);
    let tooltip_manager = TooltipManager::global();
    tooltip_manager.set_styled_tooltip(container, &tooltip);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_updates_config_defaults() {
        let entry = WidgetEntry {
            name: "updates".to_string(),
            options: Default::default(),
        };
        let config = UpdatesConfig::from_entry(&entry);

        assert_eq!(config.check_interval, DEFAULT_CHECK_INTERVAL);
        assert!(config.terminal.is_none());
    }

    #[test]
    fn test_updates_config_custom() {
        let mut options = std::collections::HashMap::new();
        options.insert("check_interval".to_string(), toml::Value::Integer(1800));
        options.insert(
            "terminal".to_string(),
            toml::Value::String("ghostty".to_string()),
        );

        let entry = WidgetEntry {
            name: "updates".to_string(),
            options,
        };
        let config = UpdatesConfig::from_entry(&entry);

        assert_eq!(config.check_interval, 1800);
        assert_eq!(config.terminal, Some("ghostty".to_string()));
    }
}
