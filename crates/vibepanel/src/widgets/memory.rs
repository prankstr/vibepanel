//! Memory widget - displays current memory usage via the shared `SystemService`.
//!
//! The SystemService polls system metrics at regular intervals and exposes
//! canonical snapshots; this widget subscribes to those snapshots and renders
//! icon/text/CSS/tooltip accordingly.
//!
//! Uses:
//! - `IconsService` (via BaseWidget) for themed memory icon
//! - `TooltipManager` for styled tooltips
//! - Shared popover with CPU widget for detailed system info

use gtk4::Label;
use gtk4::prelude::*;
use vibepanel_core::config::WidgetEntry;

use crate::services::callbacks::CallbackId;
use crate::services::icons::IconHandle;
use crate::services::system::{SystemService, SystemSnapshot, format_bytes, format_bytes_long};
use crate::services::tooltip::TooltipManager;
use crate::styles::{class, widget};
use crate::widgets::base::BaseWidget;
use crate::widgets::system_popover::SystemPopoverBinding;
use crate::widgets::{WidgetConfig, warn_unknown_options};

/// Default configuration values
const DEFAULT_SHOW_ICON: bool = true;

/// Memory display format options.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum MemoryFormat {
    /// Show percentage only: "76%"
    #[default]
    Percentage,
    /// Show absolute value only: "8.2G"
    Absolute,
    /// Show both used and total: "8.2/16G"
    Both,
}

impl MemoryFormat {
    /// Parse from a string value.
    fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "absolute" => Self::Absolute,
            "both" => Self::Both,
            _ => Self::Percentage,
        }
    }
}

/// Configuration for the Memory widget.
#[derive(Debug, Clone)]
pub struct MemoryConfig {
    /// Whether to show an icon.
    pub show_icon: bool,
    /// Display format for memory usage.
    pub format: MemoryFormat,
}

impl WidgetConfig for MemoryConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options("memory", entry, &["show_icon", "format"]);

        let show_icon = entry
            .options
            .get("show_icon")
            .and_then(|v| v.as_bool())
            .unwrap_or(DEFAULT_SHOW_ICON);

        let format = entry
            .options
            .get("format")
            .and_then(|v| v.as_str())
            .map(MemoryFormat::from_str)
            .unwrap_or_default();

        Self { show_icon, format }
    }
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            show_icon: DEFAULT_SHOW_ICON,
            format: MemoryFormat::default(),
        }
    }
}

/// Memory widget that displays icon, usage, and opens a shared system popover
/// on click.
pub struct MemoryWidget {
    /// Shared base widget container.
    base: BaseWidget,
    /// Callback ID for SystemService, used to disconnect on drop.
    system_callback_id: CallbackId,
}

impl MemoryWidget {
    /// Create a new Memory widget with the given configuration.
    pub fn new(config: MemoryConfig) -> Self {
        let base = BaseWidget::new(&[widget::MEMORY]);

        base.set_tooltip("Memory: unknown");

        let icon_handle = base.add_icon("ram-symbolic", &[widget::MEMORY_ICON]);

        let memory_label = base.add_label(None, &[widget::MEMORY_LABEL, class::VCENTER_CAPS]);

        let popover_binding = SystemPopoverBinding::new(&base);

        icon_handle.widget().set_visible(config.show_icon);

        let system_service = SystemService::global();
        let system_callback_id = {
            let container = base.widget().clone();
            let icon_handle = icon_handle.clone();
            let memory_label = memory_label.clone();
            let show_icon = config.show_icon;
            let format = config.format.clone();
            let popover_binding = popover_binding.clone();

            system_service.connect(move |snapshot: &SystemSnapshot| {
                update_memory_widget(
                    &container,
                    &icon_handle,
                    &memory_label,
                    show_icon,
                    &format,
                    snapshot,
                );

                popover_binding.update_if_open(snapshot);
            })
        };

        Self {
            base,
            system_callback_id,
        }
    }

    /// Get the root GTK widget for embedding in the bar.
    pub fn widget(&self) -> &gtk4::Box {
        self.base.widget()
    }
}

impl Drop for MemoryWidget {
    fn drop(&mut self) {
        SystemService::global().disconnect(self.system_callback_id);
    }
}

/// Format memory usage according to the selected format.
fn format_memory(snapshot: &SystemSnapshot, format: &MemoryFormat) -> String {
    match format {
        MemoryFormat::Percentage => format!("{:.0}%", snapshot.memory_percent),
        MemoryFormat::Absolute => format_bytes(snapshot.memory_used),
        MemoryFormat::Both => format!(
            "{}/{}",
            format_bytes(snapshot.memory_used),
            format_bytes(snapshot.memory_total)
        ),
    }
}

/// Update the Memory widget visuals from a system snapshot.
fn update_memory_widget(
    container: &gtk4::Box,
    icon_handle: &IconHandle,
    memory_label: &Label,
    show_icon: bool,
    format: &MemoryFormat,
    snapshot: &SystemSnapshot,
) {
    if !snapshot.available {
        if show_icon {
            icon_handle.widget().set_visible(true);
        }
        memory_label.set_label("?");
        memory_label.set_visible(true);

        let tooltip_manager = TooltipManager::global();
        tooltip_manager.set_styled_tooltip(container, "Memory: Service unavailable");
        return;
    }

    if snapshot.is_memory_high() {
        container.add_css_class(widget::MEMORY_HIGH);
        icon_handle.add_css_class(widget::MEMORY_HIGH);
    } else {
        container.remove_css_class(widget::MEMORY_HIGH);
        icon_handle.remove_css_class(widget::MEMORY_HIGH);
    }

    if show_icon {
        icon_handle.widget().set_visible(true);
    } else {
        icon_handle.widget().set_visible(false);
    }

    let text = format_memory(snapshot, format);
    memory_label.set_label(&text);
    memory_label.set_visible(true);

    let tooltip = format!(
        "Memory: {:.1}%\n{} / {}",
        snapshot.memory_percent,
        format_bytes_long(snapshot.memory_used),
        format_bytes_long(snapshot.memory_total)
    );
    let tooltip_manager = TooltipManager::global();
    tooltip_manager.set_styled_tooltip(container, &tooltip);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_config_defaults() {
        let entry = WidgetEntry {
            name: "memory".to_string(),
            options: Default::default(),
        };
        let config = MemoryConfig::from_entry(&entry);
        assert!(config.show_icon);
        assert_eq!(config.format, MemoryFormat::Percentage);
    }

    #[test]
    fn test_memory_config_custom() {
        let mut options = std::collections::HashMap::new();
        options.insert("show_icon".to_string(), toml::Value::Boolean(false));
        options.insert(
            "format".to_string(),
            toml::Value::String("absolute".to_string()),
        );

        let entry = WidgetEntry {
            name: "memory".to_string(),
            options,
        };
        let config = MemoryConfig::from_entry(&entry);
        assert!(!config.show_icon);
        assert_eq!(config.format, MemoryFormat::Absolute);
    }

    #[test]
    fn test_memory_format_from_str() {
        assert_eq!(
            MemoryFormat::from_str("percentage"),
            MemoryFormat::Percentage
        );
        assert_eq!(
            MemoryFormat::from_str("Percentage"),
            MemoryFormat::Percentage
        );
        assert_eq!(MemoryFormat::from_str("absolute"), MemoryFormat::Absolute);
        assert_eq!(MemoryFormat::from_str("ABSOLUTE"), MemoryFormat::Absolute);
        assert_eq!(MemoryFormat::from_str("both"), MemoryFormat::Both);
        assert_eq!(MemoryFormat::from_str("Both"), MemoryFormat::Both);
        assert_eq!(MemoryFormat::from_str("unknown"), MemoryFormat::Percentage);
    }
}
