//! CPU widget - displays current CPU usage via the shared `SystemService`.
//!
//! The SystemService polls system metrics at regular intervals and exposes
//! canonical snapshots; this widget subscribes to those snapshots and renders
//! icon/text/CSS/tooltip accordingly.
//!
//! Uses:
//! - `IconsService` (via BaseWidget) for themed CPU icon
//! - `TooltipManager` for styled tooltips
//! - Shared popover with Memory widget for detailed system info

use gtk4::Label;
use gtk4::prelude::*;
use vibepanel_core::config::WidgetEntry;

use crate::services::icons::IconHandle;
use crate::services::system::{SystemService, SystemSnapshot};
use crate::services::tooltip::TooltipManager;
use crate::styles::{class, widget};
use crate::widgets::base::BaseWidget;
use crate::widgets::system_popover::SystemPopoverBinding;
use crate::widgets::{WidgetConfig, warn_unknown_options};

/// Default configuration values
const DEFAULT_SHOW_ICON: bool = true;
const DEFAULT_SHOW_PERCENTAGE: bool = true;

/// Configuration for the CPU widget.
#[derive(Debug, Clone)]
pub struct CpuConfig {
    /// Whether to show an icon.
    pub show_icon: bool,
    /// Whether to show the CPU usage percentage.
    pub show_percentage: bool,
}

impl WidgetConfig for CpuConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options("cpu", entry, &["show_icon", "show_percentage"]);

        let show_icon = entry
            .options
            .get("show_icon")
            .and_then(|v| v.as_bool())
            .unwrap_or(DEFAULT_SHOW_ICON);

        let show_percentage = entry
            .options
            .get("show_percentage")
            .and_then(|v| v.as_bool())
            .unwrap_or(DEFAULT_SHOW_PERCENTAGE);

        Self {
            show_icon,
            show_percentage,
        }
    }
}

impl Default for CpuConfig {
    fn default() -> Self {
        Self {
            show_icon: DEFAULT_SHOW_ICON,
            show_percentage: DEFAULT_SHOW_PERCENTAGE,
        }
    }
}

/// CPU widget that displays icon, usage percentage, and opens a shared system
/// popover on click.
pub struct CpuWidget {
    /// Shared base widget container.
    base: BaseWidget,
    /// Icon handle from IconsService.
    icon_handle: IconHandle,
    /// Usage percentage label.
    percentage_label: Label,
    /// Configuration.
    config: CpuConfig,
    /// Popover binding for the shared system popover.
    popover_binding: SystemPopoverBinding,
}

impl CpuWidget {
    /// Create a new CPU widget with the given configuration.
    pub fn new(config: CpuConfig) -> Self {
        let base = BaseWidget::new(&[widget::CPU]);

        base.set_tooltip("CPU: unknown");

        let icon_handle = base.add_icon("memory", &[widget::CPU_ICON]);

        let percentage_label = base.add_label(None, &[widget::CPU_LABEL, class::VCENTER_CAPS]);

        let popover_binding = SystemPopoverBinding::new(&base);

        let widget = Self {
            base,
            icon_handle,
            percentage_label,
            config,
            popover_binding,
        };

        widget
            .icon_handle
            .widget()
            .set_visible(widget.config.show_icon);
        widget
            .percentage_label
            .set_visible(widget.config.show_percentage);

        let system_service = SystemService::global();
        {
            let container = widget.base.widget().clone();
            let icon_handle = widget.icon_handle.clone();
            let percentage_label = widget.percentage_label.clone();
            let show_icon = widget.config.show_icon;
            let show_percentage = widget.config.show_percentage;
            let popover_binding = widget.popover_binding.clone();

            system_service.connect(move |snapshot: &SystemSnapshot| {
                update_cpu_widget(
                    &container,
                    &icon_handle,
                    &percentage_label,
                    show_icon,
                    show_percentage,
                    snapshot,
                );

                popover_binding.update_if_open(snapshot);
            });
        }

        widget
    }

    /// Get the root GTK widget for embedding in the bar.
    pub fn widget(&self) -> &gtk4::Box {
        self.base.widget()
    }
}

/// Update the CPU widget visuals from a system snapshot.
fn update_cpu_widget(
    container: &gtk4::Box,
    icon_handle: &IconHandle,
    percentage_label: &Label,
    show_icon: bool,
    show_percentage: bool,
    snapshot: &SystemSnapshot,
) {
    if !snapshot.available {
        if show_icon {
            icon_handle.widget().set_visible(true);
        }
        if show_percentage {
            percentage_label.set_label("?");
            percentage_label.set_visible(true);
        }

        let tooltip_manager = TooltipManager::global();
        tooltip_manager.set_styled_tooltip(container, "CPU: Service unavailable");
        return;
    }

    if snapshot.is_cpu_high() {
        container.add_css_class(widget::CPU_HIGH);
        icon_handle.add_css_class(widget::CPU_HIGH);
    } else {
        container.remove_css_class(widget::CPU_HIGH);
        icon_handle.remove_css_class(widget::CPU_HIGH);
    }

    if show_icon {
        icon_handle.widget().set_visible(true);
    } else {
        icon_handle.widget().set_visible(false);
    }

    if show_percentage {
        let text = format!("{:.0}%", snapshot.cpu_usage);
        percentage_label.set_label(&text);
        percentage_label.set_visible(true);
    } else {
        percentage_label.set_visible(false);
    }

    let tooltip = format!(
        "CPU: {:.1}%\nCores: {}",
        snapshot.cpu_usage, snapshot.cpu_core_count
    );
    let tooltip_manager = TooltipManager::global();
    tooltip_manager.set_styled_tooltip(container, &tooltip);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cpu_config_defaults() {
        let entry = WidgetEntry {
            name: "cpu".to_string(),
            options: Default::default(),
        };
        let config = CpuConfig::from_entry(&entry);
        assert!(config.show_icon);
        assert!(config.show_percentage);
    }

    #[test]
    fn test_cpu_config_custom() {
        let mut options = std::collections::HashMap::new();
        options.insert("show_icon".to_string(), toml::Value::Boolean(false));
        options.insert("show_percentage".to_string(), toml::Value::Boolean(true));

        let entry = WidgetEntry {
            name: "cpu".to_string(),
            options,
        };
        let config = CpuConfig::from_entry(&entry);
        assert!(!config.show_icon);
        assert!(config.show_percentage);
    }
}
