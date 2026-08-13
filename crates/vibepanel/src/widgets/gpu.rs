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
use crate::services::gpu::{GpuDeviceSnapshot, GpuPowerState, GpuService, GpuSnapshot};
use crate::services::icons::IconHandle;
use crate::services::system::{SystemService, SystemSnapshot};
use crate::services::tooltip::TooltipManager;
use crate::styles::{class, widget};
use crate::widgets::base::BaseWidget;
use crate::widgets::gpu_format;
use crate::widgets::system_popover::SystemPopoverBinding;
use crate::widgets::{
    VERTICAL_METRIC_CHARS, WidgetConfig, format_vertical_metric, warn_unknown_options,
};

const DEFAULT_SHOW_ICON: bool = true;
const DEFAULT_STABLE_WIDTH: bool = true;
const DEVICE_SEPARATOR: &str = " | ";
const VERTICAL_DEVICE_SEPARATOR: &str = "\n<span size=\"2pt\">&#x2009;</span>\n";

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
    /// Stabilize label width for common metric values to reduce layout jitter.
    pub stable_width: bool,
}

impl WidgetConfig for GpuConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options(
            "gpu",
            entry,
            &["show_icon", "format", "device", "devices", "stable_width"],
        );

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

        let stable_width = entry
            .options
            .get("stable_width")
            .and_then(|v| v.as_bool())
            .unwrap_or(DEFAULT_STABLE_WIDTH);

        Self {
            show_icon,
            format,
            stable_width,
        }
    }
}

impl Default for GpuConfig {
    fn default() -> Self {
        Self {
            show_icon: DEFAULT_SHOW_ICON,
            format: GpuFormat::default(),
            stable_width: DEFAULT_STABLE_WIDTH,
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

        let is_vertical = ConfigManager::global().bar_position().is_vertical();
        let gpu_label = base.add_label(None, &[widget::GPU_LABEL, class::VCENTER_CAPS]);
        if is_vertical {
            gpu_label.set_justify(gtk4::Justification::Center);
        }

        icon_handle.widget().set_visible(config.show_icon);

        let gpu_service = GpuService::global();

        // Bar widget needs continuous polling while it exists.
        GpuService::request_polling(&gpu_service);

        let gpu_callback_id = {
            let container = base.widget().clone();
            let icon_handle = icon_handle.clone();
            let gpu_label = gpu_label.clone();
            let config = config.clone();

            gpu_service.connect(move |snapshot: &GpuSnapshot| {
                update_gpu_widget(
                    &container,
                    &icon_handle,
                    &gpu_label,
                    &config,
                    is_vertical,
                    snapshot,
                );
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

    pub(crate) fn edge_interaction(&self) -> Option<crate::widgets::EdgeInteraction> {
        self.base.edge_interaction()
    }
}

fn gpu_label_width(
    format: &GpuFormat,
    stable_width: bool,
    is_vertical: bool,
    device_count: usize,
) -> Option<i32> {
    if !stable_width {
        return None;
    }

    let per_device = match (format, is_vertical) {
        (GpuFormat::Usage, false) => 3,                          // 99%
        (GpuFormat::Usage, true) => VERTICAL_METRIC_CHARS,       // 99%
        (GpuFormat::Temperature, false) => 4,                    // 99°C
        (GpuFormat::Temperature, true) => VERTICAL_METRIC_CHARS, // 99°
        (GpuFormat::Both, false) => 8,                           // 99% 99°C
        (GpuFormat::Both, true) => VERTICAL_METRIC_CHARS,        // 99% / 99°
    };

    let device_count = device_count.max(1) as i32;
    Some(if is_vertical {
        per_device
    } else {
        device_count * per_device + (device_count - 1) * DEVICE_SEPARATOR.chars().count() as i32
    })
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
    let devices = &snapshot.devices;
    if devices.is_empty() {
        return "—".to_string();
    }

    let labels = devices
        .iter()
        .map(|device| format_gpu_device_label(device, format, is_vertical))
        .collect::<Vec<_>>();

    labels.join(if is_vertical {
        VERTICAL_DEVICE_SEPARATOR
    } else {
        DEVICE_SEPARATOR
    })
}

fn format_gpu_device_label(
    snapshot: &GpuDeviceSnapshot,
    format: &GpuFormat,
    is_vertical: bool,
) -> String {
    if snapshot.power_state == GpuPowerState::Suspended {
        return if is_vertical { "—" } else { "Idle" }.to_string();
    }

    match format {
        GpuFormat::Usage => match snapshot.gpu_usage {
            Some(usage) => format_gpu_usage(usage, is_vertical),
            None => "—".to_string(),
        },
        GpuFormat::Temperature => match snapshot.temperature {
            Some(temp) if is_vertical => format_vertical_metric(temp, '°'),
            Some(temp) => format!("{:.0}°C", temp),
            None => "—".to_string(),
        },
        GpuFormat::Both => {
            let usage_part = match snapshot.gpu_usage {
                Some(usage) => format_gpu_usage(usage, is_vertical),
                None => "—".to_string(),
            };
            let temp_part = match snapshot.temperature {
                Some(temp) if is_vertical => format_vertical_metric(temp, '°'),
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
        format_vertical_metric(usage, '%')
    } else {
        format!("{usage:.0}%")
    }
}

fn format_gpu_tooltip(snapshot: &GpuSnapshot) -> String {
    let devices = &snapshot.devices;
    if devices.is_empty() {
        return "GPU: No supported GPU detected".to_string();
    }

    devices
        .iter()
        .map(|device| format_gpu_tooltip_block(device, devices.len() > 1))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn format_gpu_tooltip_block(snapshot: &GpuDeviceSnapshot, show_index: bool) -> String {
    let mut lines = Vec::new();
    lines.push(gpu_format::device_title(snapshot, show_index));

    if snapshot.power_state == GpuPowerState::Suspended {
        lines.push("State: Idle (suspended)".to_string());
    } else if let Some(usage) = snapshot.gpu_usage {
        lines.push(format!("Usage: {:.1}%", usage));
    } else {
        lines.push("Usage: --".to_string());
    }

    if let Some(temp) = snapshot.temperature {
        lines.push(format!("Temp: {:.0}°C", temp));
    }

    if let Some(value) = gpu_format::vram(snapshot) {
        lines.push(format!("VRAM: {value}"));
    }

    if let Some(value) = gpu_format::clock(snapshot) {
        lines.push(format!("Clock: {value}"));
    }

    if let Some(value) = gpu_format::power(snapshot) {
        lines.push(format!("Power: {value}"));
    }

    lines.join("\n")
}

/// Update GPU widget visuals and tooltip from a snapshot.
fn update_gpu_widget(
    container: &gtk4::Box,
    icon_handle: &IconHandle,
    gpu_label: &Label,
    config: &GpuConfig,
    is_vertical: bool,
    snapshot: &GpuSnapshot,
) {
    let width_chars = gpu_label_width(
        &config.format,
        config.stable_width,
        is_vertical,
        snapshot.devices.len(),
    );
    gpu_label.set_width_chars(width_chars.unwrap_or(-1));
    gpu_label.set_markup(&format_gpu_label(snapshot, &config.format, is_vertical));
    gpu_label.set_visible(true);

    if snapshot.is_gpu_high() {
        container.add_css_class(widget::GPU_HIGH);
        icon_handle.add_css_class(widget::GPU_HIGH);
    } else {
        container.remove_css_class(widget::GPU_HIGH);
        icon_handle.remove_css_class(widget::GPU_HIGH);
    }

    if snapshot.all_devices_suspended() {
        container.add_css_class(widget::GPU_SUSPENDED);
    } else {
        container.remove_css_class(widget::GPU_SUSPENDED);
    }

    icon_handle.widget().set_visible(config.show_icon);

    let tooltip = format_gpu_tooltip(snapshot);
    let tooltip_manager = TooltipManager::global();
    tooltip_manager.set_styled_tooltip(container, &tooltip);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu_snapshot(gpu_usage: Option<f32>, temperature: Option<f32>) -> GpuSnapshot {
        GpuSnapshot {
            devices: vec![GpuDeviceSnapshot {
                gpu_usage,
                temperature,
                ..Default::default()
            }],
        }
    }

    fn multi_gpu_snapshot() -> GpuSnapshot {
        GpuSnapshot {
            devices: vec![
                GpuDeviceSnapshot {
                    device_index: 1,
                    gpu_usage: Some(76.0),
                    temperature: Some(72.0),
                    device_name: Some("AMD Radeon".to_string()),
                    ..Default::default()
                },
                GpuDeviceSnapshot {
                    device_index: 0,
                    gpu_usage: Some(41.0),
                    temperature: Some(61.0),
                    device_name: Some("NVIDIA RTX".to_string()),
                    ..Default::default()
                },
            ],
        }
    }

    #[test]
    fn test_gpu_config_defaults() {
        let entry = WidgetEntry {
            name: "gpu".to_string(),
            options: Default::default(),
        };
        let config = GpuConfig::from_entry(&entry);
        assert!(config.show_icon);
        assert_eq!(config.format, GpuFormat::Usage);
        assert!(config.stable_width);
    }

    #[test]
    fn test_gpu_config_custom() {
        let mut options = std::collections::HashMap::new();
        options.insert("show_icon".to_string(), toml::Value::Boolean(false));
        options.insert("stable_width".to_string(), toml::Value::Boolean(false));
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
        assert!(!config.stable_width);
    }

    #[test]
    fn test_gpu_label_width_usage_matches_percentage_width() {
        assert_eq!(gpu_label_width(&GpuFormat::Usage, true, false, 1), Some(3));
        assert_eq!(gpu_label_width(&GpuFormat::Usage, false, false, 1), None);
        assert_eq!(gpu_label_width(&GpuFormat::Usage, true, false, 2), Some(9));
        assert_eq!(gpu_label_width(&GpuFormat::Both, true, false, 2), Some(19));
        assert_eq!(gpu_label_width(&GpuFormat::Both, true, true, 2), Some(3));
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
        let snapshot = gpu_snapshot(Some(76.0), Some(72.0));
        assert_eq!(format_gpu_label(&snapshot, &GpuFormat::Usage, false), "76%");
        assert_eq!(format_gpu_label(&snapshot, &GpuFormat::Usage, true), "76%");
    }

    #[test]
    fn test_format_gpu_label_temperature() {
        let snapshot = gpu_snapshot(Some(76.0), Some(72.0));
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
        let snapshot = gpu_snapshot(Some(76.0), None);
        // Shows dash when temperature is unavailable — no silent fallback
        assert_eq!(
            format_gpu_label(&snapshot, &GpuFormat::Temperature, false),
            "—"
        );
    }

    #[test]
    fn test_format_gpu_label_both() {
        let snapshot = gpu_snapshot(Some(76.0), Some(72.0));
        assert_eq!(
            format_gpu_label(&snapshot, &GpuFormat::Both, false),
            "76% 72°C"
        );
        assert_eq!(
            format_gpu_label(&snapshot, &GpuFormat::Both, true),
            "76%\n72°"
        );

        let boundary = gpu_snapshot(Some(100.0), Some(100.0));
        assert_eq!(format_gpu_label(&boundary, &GpuFormat::Usage, true), "100");
        assert_eq!(
            format_gpu_label(&boundary, &GpuFormat::Temperature, true),
            "100"
        );
        assert_eq!(
            format_gpu_label(&boundary, &GpuFormat::Both, true),
            "100\n100"
        );
    }

    #[test]
    fn test_format_gpu_label_both_no_temp() {
        let snapshot = gpu_snapshot(Some(76.0), None);
        assert_eq!(
            format_gpu_label(&snapshot, &GpuFormat::Both, false),
            "76% —"
        );
        assert_eq!(
            format_gpu_label(&snapshot, &GpuFormat::Both, true),
            "76%\n—"
        );
    }

    #[test]
    fn test_format_gpu_label_no_data() {
        let snapshot = gpu_snapshot(None, None);
        assert_eq!(format_gpu_label(&snapshot, &GpuFormat::Usage, false), "—");
        assert_eq!(
            format_gpu_label(&snapshot, &GpuFormat::Temperature, false),
            "—"
        );
        assert_eq!(format_gpu_label(&snapshot, &GpuFormat::Both, false), "— —");
    }

    #[test]
    fn test_format_gpu_label_multiple_devices() {
        let snapshot = multi_gpu_snapshot();
        assert_eq!(
            format_gpu_label(&snapshot, &GpuFormat::Usage, false),
            "76% | 41%"
        );
        assert_eq!(
            format_gpu_label(&snapshot, &GpuFormat::Usage, true),
            "76%\n<span size=\"2pt\">&#x2009;</span>\n41%"
        );
        assert_eq!(
            format_gpu_label(&snapshot, &GpuFormat::Both, true),
            "76%\n72°\n<span size=\"2pt\">&#x2009;</span>\n41%\n61°"
        );
    }

    #[test]
    fn test_format_gpu_label_compacts_suspended_device_vertically() {
        let snapshot = GpuSnapshot {
            devices: vec![GpuDeviceSnapshot {
                power_state: GpuPowerState::Suspended,
                ..Default::default()
            }],
        };

        assert_eq!(
            format_gpu_label(&snapshot, &GpuFormat::Usage, false),
            "Idle"
        );
        assert_eq!(format_gpu_label(&snapshot, &GpuFormat::Usage, true), "—");
    }

    #[test]
    fn test_format_gpu_tooltip_multiple_devices() {
        let tooltip = format_gpu_tooltip(&multi_gpu_snapshot());
        assert!(tooltip.contains("GPU 1: AMD Radeon"));
        assert!(tooltip.contains("GPU 0: NVIDIA RTX"));
        assert!(tooltip.contains("Usage: 76.0%"));
        assert!(tooltip.contains("Usage: 41.0%"));
    }
}
