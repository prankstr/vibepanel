//! GPU widget - displays current GPU usage via the `GpuService`.
//!
//! The GpuService polls GPU metrics at regular intervals using vendor-specific
//! backends (AMD sysfs, NVIDIA NVML); this widget subscribes to those snapshots
//! and renders icon/text/CSS/tooltip accordingly.
//!
//! Uses:
//! - `IconsService` (via BaseWidget) for themed GPU icon
//! - `TooltipManager` for styled tooltips
//! - Shared popover with CPU/Memory widgets for detailed system info

use gtk4::Label;
use gtk4::prelude::*;
use vibepanel_core::config::WidgetEntry;

use crate::services::callbacks::CallbackId;
use crate::services::config_manager::ConfigManager;
use crate::services::gpu::{GpuPowerState, GpuService, GpuSnapshot};
use crate::services::icons::IconHandle;
use crate::services::system::{SystemService, SystemSnapshot, format_bytes_long};
use crate::services::tooltip::TooltipManager;
use crate::styles::{class, widget};
use crate::widgets::base::BaseWidget;
use crate::widgets::system_popover::SystemPopoverBinding;
use crate::widgets::{WidgetConfig, warn_unknown_options};

const DEFAULT_SHOW_ICON: bool = true;

/// GPU display format options.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum GpuFormat {
    /// "76%"
    #[default]
    Usage,
    /// "72°C"
    Temperature,
    /// "76% 72°C"
    Both,
}

impl GpuFormat {
    fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "temperature" | "temp" => Self::Temperature,
            "both" => Self::Both,
            _ => Self::Usage,
        }
    }
}

/// Configuration for the GPU widget.
#[derive(Debug, Clone)]
pub struct GpuConfig {
    /// Whether to show an icon.
    pub show_icon: bool,
    /// Display format for GPU metrics.
    pub format: GpuFormat,
}

impl WidgetConfig for GpuConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options("gpu", entry, &["show_icon", "format", "device"]);

        let show_icon = entry
            .options
            .get("show_icon")
            .and_then(|v| v.as_bool())
            .unwrap_or(DEFAULT_SHOW_ICON);

        let format = entry
            .options
            .get("format")
            .and_then(|v| v.as_str())
            .map(GpuFormat::from_str)
            .unwrap_or_default();

        Self { show_icon, format }
    }
}

impl Default for GpuConfig {
    fn default() -> Self {
        Self {
            show_icon: DEFAULT_SHOW_ICON,
            format: GpuFormat::default(),
        }
    }
}

/// GPU widget that displays icon, usage, and opens a shared system popover on click.
pub struct GpuWidget {
    /// Shared base widget container.
    base: BaseWidget,
    /// Callback ID for GpuService, used to disconnect on drop.
    gpu_callback_id: CallbackId,
    /// Callback ID for SystemService, used to disconnect on drop.
    system_callback_id: CallbackId,
}

impl GpuWidget {
    /// Create a new GPU widget with the given configuration.
    pub fn new(config: GpuConfig) -> Self {
        let base = BaseWidget::new(&[widget::GPU]);
        let popover_binding = SystemPopoverBinding::new(&base);
        Self::build(config, base, popover_binding)
    }

    /// Create a passive GPU widget for use in a merge group.
    pub fn new_passive(config: GpuConfig, shared_binding: SystemPopoverBinding) -> Self {
        let base = BaseWidget::new_passive(&[widget::GPU]);
        Self::build(config, base, shared_binding)
    }

    /// Shared construction for active and passive modes.
    fn build(config: GpuConfig, base: BaseWidget, popover_binding: SystemPopoverBinding) -> Self {
        base.set_tooltip("GPU: unknown");

        let icon_handle = base.add_icon("video-display-symbolic", &[widget::GPU_ICON]);

        let gpu_label = base.add_label(None, &[widget::GPU_LABEL, class::VCENTER_CAPS]);

        icon_handle.widget().set_visible(config.show_icon);

        let gpu_service = GpuService::global();

        // Bar widget needs continuous polling while it exists.
        GpuService::request_polling(&gpu_service);

        let gpu_callback_id = {
            let container = base.widget().clone();
            let icon_handle = icon_handle.clone();
            let gpu_label = gpu_label.clone();
            let show_icon = config.show_icon;
            let format = config.format.clone();
            let is_vertical = ConfigManager::global().bar_position().is_vertical();
            let popover_binding = popover_binding.clone();

            gpu_service.connect(move |snapshot: &GpuSnapshot| {
                update_gpu_widget(
                    &container,
                    &icon_handle,
                    &gpu_label,
                    show_icon,
                    &format,
                    is_vertical,
                    snapshot,
                );

                popover_binding.update_gpu_if_open(snapshot);
            })
        };

        // Also subscribe to SystemService to keep the shared popover's CPU/memory data live.
        let system_service = SystemService::global();
        let system_callback_id = {
            let popover_binding = popover_binding.clone();

            system_service.connect(move |snapshot: &SystemSnapshot| {
                popover_binding.update_if_open(snapshot);
            })
        };

        Self {
            base,
            gpu_callback_id,
            system_callback_id,
        }
    }

    /// Get the root GTK widget for embedding in the bar.
    pub fn widget(&self) -> &gtk4::Box {
        self.base.widget()
    }
}

impl Drop for GpuWidget {
    fn drop(&mut self) {
        let gpu_service = GpuService::global();
        gpu_service.disconnect(self.gpu_callback_id);
        gpu_service.release_polling();
        SystemService::global().disconnect(self.system_callback_id);
    }
}

/// Format GPU label text according to the selected format.
fn format_gpu_label(snapshot: &GpuSnapshot, format: &GpuFormat, is_vertical: bool) -> String {
    if snapshot.power_state == GpuPowerState::Suspended {
        return "Idle".to_string();
    }
    match format {
        GpuFormat::Usage => match snapshot.gpu_usage {
            Some(usage) => format_gpu_usage(usage, is_vertical),
            None => "—".to_string(),
        },
        GpuFormat::Temperature => match snapshot.temperature {
            Some(temp) if is_vertical => format!("{:.0}°", temp),
            Some(temp) => format!("{:.0}°C", temp),
            None => "—".to_string(),
        },
        GpuFormat::Both => {
            let usage_part = match snapshot.gpu_usage {
                Some(usage) => format_gpu_usage(usage, is_vertical),
                None => "—".to_string(),
            };
            let temp_part = match snapshot.temperature {
                Some(temp) if is_vertical => format!("{:.0}°", temp),
                Some(temp) => format!("{:.0}°C", temp),
                None => "—".to_string(),
            };
            if is_vertical {
                format!("{}\n{}", usage_part, temp_part)
            } else {
                format!("{} {}", usage_part, temp_part)
            }
        }
    }
}

fn format_gpu_usage(usage: f32, is_vertical: bool) -> String {
    if is_vertical {
        format!("{usage:.0}")
    } else {
        format!("{usage:.0}%")
    }
}

/// Update GPU widget visuals and tooltip from a snapshot.
fn update_gpu_widget(
    container: &gtk4::Box,
    icon_handle: &IconHandle,
    gpu_label: &Label,
    show_icon: bool,
    format: &GpuFormat,
    is_vertical: bool,
    snapshot: &GpuSnapshot,
) {
    if !snapshot.available {
        container.remove_css_class(widget::GPU_HIGH);
        icon_handle.remove_css_class(widget::GPU_HIGH);

        if show_icon {
            icon_handle.widget().set_visible(true);
        }
        gpu_label.set_label("—");
        gpu_label.set_visible(true);

        let tooltip_manager = TooltipManager::global();
        tooltip_manager.set_styled_tooltip(container, "GPU: No supported GPU detected");
        return;
    }

    if snapshot.is_gpu_high() {
        container.add_css_class(widget::GPU_HIGH);
        icon_handle.add_css_class(widget::GPU_HIGH);
    } else {
        container.remove_css_class(widget::GPU_HIGH);
        icon_handle.remove_css_class(widget::GPU_HIGH);
    }

    if snapshot.power_state == GpuPowerState::Suspended {
        container.add_css_class(widget::GPU_SUSPENDED);
    } else {
        container.remove_css_class(widget::GPU_SUSPENDED);
    }

    icon_handle.widget().set_visible(show_icon);

    let text = format_gpu_label(snapshot, format, is_vertical);
    gpu_label.set_label(&text);
    gpu_label.set_visible(true);

    let mut lines = Vec::new();

    if snapshot.power_state == GpuPowerState::Suspended {
        lines.push("GPU: Idle (suspended)".to_string());
    } else if let Some(usage) = snapshot.gpu_usage {
        lines.push(format!("GPU: {:.1}%", usage));
    } else {
        lines.push("GPU: --".to_string());
    }

    if let Some(temp) = snapshot.temperature {
        lines.push(format!("Temp: {:.0}°C", temp));
    }

    if let (Some(used), Some(total)) = (snapshot.vram_used, snapshot.vram_total) {
        lines.push(format!(
            "VRAM: {} / {}",
            format_bytes_long(used),
            format_bytes_long(total)
        ));
    }

    if let Some(mhz) = snapshot.clock_mhz {
        lines.push(format!("Clock: {} MHz", mhz));
    }

    if let Some(watts) = snapshot.power_watts {
        lines.push(format!("Power: {:.1} W", watts));
    }

    if let Some(ref name) = snapshot.device_name {
        lines.push(name.clone());
    }

    let tooltip = lines.join("\n");
    let tooltip_manager = TooltipManager::global();
    tooltip_manager.set_styled_tooltip(container, &tooltip);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gpu_config_defaults() {
        let entry = WidgetEntry {
            name: "gpu".to_string(),
            options: Default::default(),
        };
        let config = GpuConfig::from_entry(&entry);
        assert!(config.show_icon);
        assert_eq!(config.format, GpuFormat::Usage);
    }

    #[test]
    fn test_gpu_config_custom() {
        let mut options = std::collections::HashMap::new();
        options.insert("show_icon".to_string(), toml::Value::Boolean(false));
        options.insert(
            "format".to_string(),
            toml::Value::String("temperature".to_string()),
        );

        let entry = WidgetEntry {
            name: "gpu".to_string(),
            options,
        };
        let config = GpuConfig::from_entry(&entry);
        assert!(!config.show_icon);
        assert_eq!(config.format, GpuFormat::Temperature);
    }

    #[test]
    fn test_gpu_format_from_str() {
        assert_eq!(GpuFormat::from_str("usage"), GpuFormat::Usage);
        assert_eq!(GpuFormat::from_str("Usage"), GpuFormat::Usage);
        assert_eq!(GpuFormat::from_str("percentage"), GpuFormat::Usage);
        assert_eq!(GpuFormat::from_str("temperature"), GpuFormat::Temperature);
        assert_eq!(GpuFormat::from_str("Temperature"), GpuFormat::Temperature);
        assert_eq!(GpuFormat::from_str("temp"), GpuFormat::Temperature);
        assert_eq!(GpuFormat::from_str("TEMP"), GpuFormat::Temperature);
        assert_eq!(GpuFormat::from_str("both"), GpuFormat::Both);
        assert_eq!(GpuFormat::from_str("Both"), GpuFormat::Both);
        assert_eq!(GpuFormat::from_str("unknown"), GpuFormat::Usage);
    }

    #[test]
    fn test_format_gpu_label_usage() {
        let snapshot = GpuSnapshot {
            available: true,
            gpu_usage: Some(76.0),
            temperature: Some(72.0),
            ..Default::default()
        };
        assert_eq!(format_gpu_label(&snapshot, &GpuFormat::Usage, false), "76%");
        assert_eq!(format_gpu_label(&snapshot, &GpuFormat::Usage, true), "76");
    }

    #[test]
    fn test_format_gpu_label_temperature() {
        let snapshot = GpuSnapshot {
            available: true,
            gpu_usage: Some(76.0),
            temperature: Some(72.0),
            ..Default::default()
        };
        assert_eq!(
            format_gpu_label(&snapshot, &GpuFormat::Temperature, false),
            "72°C"
        );
        assert_eq!(
            format_gpu_label(&snapshot, &GpuFormat::Temperature, true),
            "72°"
        );
    }

    #[test]
    fn test_format_gpu_label_temperature_unavailable() {
        let snapshot = GpuSnapshot {
            available: true,
            gpu_usage: Some(76.0),
            temperature: None,
            ..Default::default()
        };
        // Shows dash when temperature is unavailable — no silent fallback
        assert_eq!(
            format_gpu_label(&snapshot, &GpuFormat::Temperature, false),
            "—"
        );
    }

    #[test]
    fn test_format_gpu_label_both() {
        let snapshot = GpuSnapshot {
            available: true,
            gpu_usage: Some(76.0),
            temperature: Some(72.0),
            ..Default::default()
        };
        assert_eq!(
            format_gpu_label(&snapshot, &GpuFormat::Both, false),
            "76% 72°C"
        );
        assert_eq!(
            format_gpu_label(&snapshot, &GpuFormat::Both, true),
            "76\n72°"
        );
    }

    #[test]
    fn test_format_gpu_label_both_no_temp() {
        let snapshot = GpuSnapshot {
            available: true,
            gpu_usage: Some(76.0),
            temperature: None,
            ..Default::default()
        };
        assert_eq!(
            format_gpu_label(&snapshot, &GpuFormat::Both, false),
            "76% —"
        );
        assert_eq!(format_gpu_label(&snapshot, &GpuFormat::Both, true), "76\n—");
    }

    #[test]
    fn test_format_gpu_label_no_data() {
        let snapshot = GpuSnapshot {
            available: true,
            gpu_usage: None,
            temperature: None,
            ..Default::default()
        };
        assert_eq!(format_gpu_label(&snapshot, &GpuFormat::Usage, false), "—");
        assert_eq!(
            format_gpu_label(&snapshot, &GpuFormat::Temperature, false),
            "—"
        );
        assert_eq!(format_gpu_label(&snapshot, &GpuFormat::Both, false), "— —");
    }
}
