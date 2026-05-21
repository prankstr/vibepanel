//! CPU widget - displays current CPU usage via the shared `SystemService`.
//!
//! The SystemService polls system metrics at regular intervals and exposes
//! canonical snapshots; this widget subscribes to those snapshots and renders
//! icon/text/CSS/tooltip accordingly.
//!
//! Uses:
//! - `IconsService` (via BaseWidget) for themed CPU icon
//! - `TooltipManager` for styled tooltips
//! - Shared popover with Memory/GPU widgets for detailed system info

use gtk4::Label;
use gtk4::prelude::*;
use vibepanel_core::config::WidgetEntry;

use crate::services::callbacks::CallbackId;
use crate::services::config_manager::ConfigManager;
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

/// CPU display format options.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum CpuFormat {
    /// "76%"
    #[default]
    Usage,
    /// "72°C"
    Temperature,
    /// "76% 72°C"
    Both,
}

impl CpuFormat {
    fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "temperature" | "temp" => Self::Temperature,
            "both" => Self::Both,
            _ => Self::Usage,
        }
    }
}

/// Configuration for the CPU widget.
#[derive(Debug, Clone)]
pub struct CpuConfig {
    /// Whether to show an icon.
    pub show_icon: bool,
    /// Whether to show the CPU value label.
    ///
    /// Deprecated for user configuration: prefer leaving this enabled and using
    /// `format` to choose the label contents. This legacy option gates the whole
    /// label, including temperature.
    pub show_percentage: bool,
    /// Display format for CPU metrics.
    pub format: CpuFormat,
}

impl WidgetConfig for CpuConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options("cpu", entry, &["show_icon", "show_percentage", "format"]);

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

        let format = entry
            .options
            .get("format")
            .and_then(|v| v.as_str())
            .map(CpuFormat::from_str)
            .unwrap_or_default();

        Self {
            show_icon,
            show_percentage,
            format,
        }
    }
}

impl Default for CpuConfig {
    fn default() -> Self {
        Self {
            show_icon: DEFAULT_SHOW_ICON,
            show_percentage: DEFAULT_SHOW_PERCENTAGE,
            format: CpuFormat::default(),
        }
    }
}

/// CPU widget that displays icon, usage/temperature label, and opens a shared system
/// popover on click.
pub struct CpuWidget {
    /// Shared base widget container.
    base: BaseWidget,
    /// Callback ID for SystemService, used to disconnect on drop.
    system_callback_id: CallbackId,
}

impl CpuWidget {
    /// Create a new CPU widget with the given configuration.
    pub fn new(config: CpuConfig) -> Self {
        let base = BaseWidget::new(&[widget::CPU]);
        let popover_binding = SystemPopoverBinding::new(&base);
        Self::build(config, base, popover_binding)
    }

    /// Create a passive CPU widget for use in a merge group.
    pub fn new_passive(config: CpuConfig, shared_binding: SystemPopoverBinding) -> Self {
        let base = BaseWidget::new_passive(&[widget::CPU]);
        Self::build(config, base, shared_binding)
    }

    /// Shared construction for active and passive modes.
    fn build(config: CpuConfig, base: BaseWidget, popover_binding: SystemPopoverBinding) -> Self {
        base.set_tooltip("CPU: unknown");

        let icon_handle = base.add_icon("cpu-symbolic", &[widget::CPU_ICON]);

        let percentage_label = base.add_label(None, &[widget::CPU_LABEL, class::VCENTER_CAPS]);

        icon_handle.widget().set_visible(config.show_icon);
        percentage_label.set_visible(config.show_percentage);

        let system_service = SystemService::global();
        let system_callback_id = {
            let container = base.widget().clone();
            let icon_handle = icon_handle.clone();
            let percentage_label = percentage_label.clone();
            let show_icon = config.show_icon;
            let show_percentage = config.show_percentage;
            let format = config.format.clone();
            let is_vertical = ConfigManager::global().bar_position().is_vertical();
            let popover_binding = popover_binding.clone();

            system_service.connect(move |snapshot: &SystemSnapshot| {
                update_cpu_widget(
                    &container,
                    &icon_handle,
                    show_percentage.then_some(&percentage_label),
                    show_icon,
                    &format,
                    is_vertical,
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

impl Drop for CpuWidget {
    fn drop(&mut self) {
        SystemService::global().disconnect(self.system_callback_id);
    }
}

/// Update the CPU widget visuals from a system snapshot.
fn update_cpu_widget(
    container: &gtk4::Box,
    icon_handle: &IconHandle,
    percentage_label: Option<&Label>,
    show_icon: bool,
    format: &CpuFormat,
    is_vertical: bool,
    snapshot: &SystemSnapshot,
) {
    if !snapshot.available {
        if show_icon {
            icon_handle.widget().set_visible(true);
        }
        if let Some(percentage_label) = percentage_label {
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

    icon_handle.widget().set_visible(show_icon);

    if let Some(percentage_label) = percentage_label {
        let text = format_cpu_label(snapshot, format, is_vertical);
        percentage_label.set_label(&text);
        percentage_label.set_visible(true);
    }

    let tooltip = match snapshot.cpu_temp {
        Some(temp) => format!(
            "CPU: {:.1}%\nTemp: {:.0}°C\nCores: {}",
            snapshot.cpu_usage, temp, snapshot.cpu_core_count
        ),
        None => format!(
            "CPU: {:.1}%\nCores: {}",
            snapshot.cpu_usage, snapshot.cpu_core_count
        ),
    };
    let tooltip_manager = TooltipManager::global();
    tooltip_manager.set_styled_tooltip(container, &tooltip);
}

fn format_cpu_usage(cpu_usage: f32, is_vertical: bool) -> String {
    if is_vertical {
        format!("{cpu_usage:.0}")
    } else {
        format!("{cpu_usage:.0}%")
    }
}

/// Format CPU label text according to the selected format.
fn format_cpu_label(snapshot: &SystemSnapshot, format: &CpuFormat, is_vertical: bool) -> String {
    match format {
        CpuFormat::Usage => format_cpu_usage(snapshot.cpu_usage, is_vertical),
        CpuFormat::Temperature => match snapshot.cpu_temp {
            Some(temp) if is_vertical => format!("{temp:.0}°"),
            Some(temp) => format!("{temp:.0}°C"),
            None => "—".to_string(),
        },
        CpuFormat::Both => {
            let usage_part = format_cpu_usage(snapshot.cpu_usage, is_vertical);
            let temp_part = match snapshot.cpu_temp {
                Some(temp) if is_vertical => format!("{temp:.0}°"),
                Some(temp) => format!("{temp:.0}°C"),
                None => "—".to_string(),
            };
            if is_vertical {
                format!("{usage_part}\n{temp_part}")
            } else {
                format!("{usage_part} {temp_part}")
            }
        }
    }
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
        assert_eq!(config.format, CpuFormat::Usage);
    }

    #[test]
    fn test_cpu_config_custom() {
        let mut options = std::collections::HashMap::new();
        options.insert("show_icon".to_string(), toml::Value::Boolean(false));
        options.insert("show_percentage".to_string(), toml::Value::Boolean(true));
        options.insert(
            "format".to_string(),
            toml::Value::String("both".to_string()),
        );

        let entry = WidgetEntry {
            name: "cpu".to_string(),
            options,
        };
        let config = CpuConfig::from_entry(&entry);
        assert!(!config.show_icon);
        assert!(config.show_percentage);
        assert_eq!(config.format, CpuFormat::Both);
    }

    #[test]
    fn test_format_cpu_label_compacts_vertical() {
        let snapshot = SystemSnapshot {
            available: true,
            cpu_usage: 42.4,
            cpu_temp: Some(72.0),
            ..Default::default()
        };
        assert_eq!(format_cpu_label(&snapshot, &CpuFormat::Usage, false), "42%");
        assert_eq!(format_cpu_label(&snapshot, &CpuFormat::Usage, true), "42");
        assert_eq!(
            format_cpu_label(&snapshot, &CpuFormat::Both, true),
            "42\n72°"
        );
        assert_eq!(
            format_cpu_label(&snapshot, &CpuFormat::Temperature, true),
            "72°"
        );
    }

    #[test]
    fn test_cpu_format_from_str() {
        assert_eq!(CpuFormat::from_str("usage"), CpuFormat::Usage);
        assert_eq!(CpuFormat::from_str("temperature"), CpuFormat::Temperature);
        assert_eq!(CpuFormat::from_str("temp"), CpuFormat::Temperature);
        assert_eq!(CpuFormat::from_str("both"), CpuFormat::Both);
        assert_eq!(CpuFormat::from_str("unknown"), CpuFormat::Usage);
    }

    #[test]
    fn test_format_cpu_label_temperature_unavailable() {
        let snapshot = SystemSnapshot {
            available: true,
            cpu_usage: 76.0,
            cpu_temp: None,
            ..Default::default()
        };
        assert_eq!(
            format_cpu_label(&snapshot, &CpuFormat::Temperature, false),
            "—"
        );
    }
}
