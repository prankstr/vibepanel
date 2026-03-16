//! GPU widget - displays current GPU usage via the `GpuService`.
//!
//! The GpuService polls GPU metrics at regular intervals by reading sysfs files
//! for AMD GPUs; this widget subscribes to those snapshots and renders
//! icon/text/CSS/tooltip accordingly.
//!
//! Uses:
//! - `IconsService` (via BaseWidget) for themed GPU icon
//! - `TooltipManager` for styled tooltips
//! - Shared popover with CPU/Memory widgets for detailed system info
//!
//! Currently supports AMD GPUs only (via `amdgpu` kernel driver).

use gtk4::Label;
use gtk4::prelude::*;
use vibepanel_core::config::WidgetEntry;

use crate::services::callbacks::CallbackId;
use crate::services::gpu::{GpuService, GpuSnapshot, format_vram};
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

/// Configuration for the GPU widget.
#[derive(Debug, Clone)]
pub struct GpuConfig {
    /// Whether to show an icon.
    pub show_icon: bool,
    /// Whether to show the GPU usage percentage.
    pub show_percentage: bool,
}

impl WidgetConfig for GpuConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options("gpu", entry, &["show_icon", "show_percentage"]);

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

impl Default for GpuConfig {
    fn default() -> Self {
        Self {
            show_icon: DEFAULT_SHOW_ICON,
            show_percentage: DEFAULT_SHOW_PERCENTAGE,
        }
    }
}

/// GPU widget that displays icon, usage percentage, and opens a shared system
/// popover on click.
pub struct GpuWidget {
    /// Shared base widget container.
    base: BaseWidget,
    /// Callback ID for GpuService, used to disconnect on drop.
    gpu_callback_id: CallbackId,
    /// Callback ID for SystemService (for shared popover updates).
    system_callback_id: CallbackId,
}

impl GpuWidget {
    /// Create a new GPU widget with the given configuration.
    pub fn new(config: GpuConfig) -> Self {
        let base = BaseWidget::new(&[widget::GPU]);

        base.set_tooltip("GPU: unknown");

        let icon_handle = base.add_icon("video-display-symbolic", &[widget::GPU_ICON]);

        let percentage_label = base.add_label(None, &[widget::GPU_LABEL, class::VCENTER_CAPS]);

        let popover_binding = SystemPopoverBinding::new(&base);

        icon_handle.widget().set_visible(config.show_icon);
        percentage_label.set_visible(config.show_percentage);

        // Subscribe to GpuService for GPU-specific updates
        let gpu_service = GpuService::global();
        let gpu_callback_id = {
            let container = base.widget().clone();
            let icon_handle = icon_handle.clone();
            let percentage_label = percentage_label.clone();
            let show_icon = config.show_icon;
            let show_percentage = config.show_percentage;
            let popover_binding = popover_binding.clone();

            gpu_service.connect(move |snapshot: &GpuSnapshot| {
                update_gpu_widget(
                    &container,
                    &icon_handle,
                    &percentage_label,
                    show_icon,
                    show_percentage,
                    snapshot,
                );

                popover_binding.update_gpu_if_open(snapshot);
            })
        };

        // Subscribe to SystemService for shared popover system updates
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
        GpuService::global().disconnect(self.gpu_callback_id);
        SystemService::global().disconnect(self.system_callback_id);
    }
}

/// Update the GPU widget visuals from a GPU snapshot.
fn update_gpu_widget(
    container: &gtk4::Box,
    icon_handle: &IconHandle,
    percentage_label: &Label,
    show_icon: bool,
    show_percentage: bool,
    snapshot: &GpuSnapshot,
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

    if show_icon {
        icon_handle.widget().set_visible(true);
    } else {
        icon_handle.widget().set_visible(false);
    }

    if show_percentage {
        let text = match snapshot.gpu_usage {
            Some(usage) => format!("{:.0}%", usage),
            None => "?".to_string(),
        };
        percentage_label.set_label(&text);
        percentage_label.set_visible(true);
    } else {
        percentage_label.set_visible(false);
    }

    // Build tooltip with available metrics
    let mut lines = Vec::new();

    if let Some(usage) = snapshot.gpu_usage {
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
            format_vram(used),
            format_vram(total)
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
        assert!(config.show_percentage);
    }

    #[test]
    fn test_gpu_config_custom() {
        let mut options = std::collections::HashMap::new();
        options.insert("show_icon".to_string(), toml::Value::Boolean(false));
        options.insert("show_percentage".to_string(), toml::Value::Boolean(true));

        let entry = WidgetEntry {
            name: "gpu".to_string(),
            options,
        };
        let config = GpuConfig::from_entry(&entry);
        assert!(!config.show_icon);
        assert!(config.show_percentage);
    }
}
