//! Disk widget - displays space usage of the filesystem holding a path.
//!
//! Usage comes from `SystemService`, which queries the path off the GTK thread.
//! Opens the shared system popover, which lists every mounted drive.

use gtk4::Label;
use gtk4::prelude::*;
use vibepanel_core::config::WidgetEntry;
use vibepanel_core::expand_tilde;

use crate::services::callbacks::CallbackId;
use crate::services::config_manager::ConfigManager;
use crate::services::icons::IconHandle;
use crate::services::system::{
    DISK_HIGH_THRESHOLD, SpaceUsage, SystemService, format_bytes, format_bytes_long,
    format_used_of_total,
};
use crate::services::tooltip::TooltipManager;
use crate::styles::{class, widget};
use crate::widgets::base::BaseWidget;
use crate::widgets::system_popover::wire_system_popover;
use crate::widgets::{
    VERTICAL_METRIC_CHARS, WidgetConfig, format_vertical_metric, warn_unknown_options,
};

const DEFAULT_PATH: &str = "/";
const DEFAULT_SHOW_ICON: bool = true;
const DEFAULT_STABLE_WIDTH: bool = false;

/// Disk display format options.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum DiskFormat {
    /// Used percentage: "76%"
    #[default]
    Percentage,
    /// Free space: "106.0G"
    Free,
    /// Used of total, in the total's unit: "1.2/1.8T"
    UsedOfTotal,
}

impl DiskFormat {
    fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "free" => Self::Free,
            "used/total" | "both" => Self::UsedOfTotal,
            _ => Self::Percentage,
        }
    }
}

/// Configuration for the Disk widget.
#[derive(Debug, Clone)]
pub struct DiskConfig {
    /// Widget name ("disk" or a named instance like "disk-home"), used as a CSS class.
    pub name: String,
    /// Any existing path; usage is reported for the filesystem containing it.
    pub path: String,
    pub show_icon: bool,
    pub format: DiskFormat,
    /// Stabilize label width for common metric values to reduce layout jitter.
    pub stable_width: bool,
}

impl WidgetConfig for DiskConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options(
            &entry.name,
            entry,
            &["path", "show_icon", "format", "stable_width"],
        );
        let option = |key: &str| entry.options.get(key);

        Self {
            name: entry.name.clone(),
            path: expand_tilde(
                option("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or(DEFAULT_PATH),
                &std::env::var("HOME").unwrap_or_else(|_| "~".to_string()),
            ),
            show_icon: option("show_icon")
                .and_then(|v| v.as_bool())
                .unwrap_or(DEFAULT_SHOW_ICON),
            format: option("format")
                .and_then(|v| v.as_str())
                .map(DiskFormat::from_str)
                .unwrap_or_default(),
            stable_width: option("stable_width")
                .and_then(|v| v.as_bool())
                .unwrap_or(DEFAULT_STABLE_WIDTH),
        }
    }
}

impl Default for DiskConfig {
    fn default() -> Self {
        Self {
            name: widget::DISK.to_string(),
            path: DEFAULT_PATH.to_string(),
            show_icon: DEFAULT_SHOW_ICON,
            format: DiskFormat::default(),
            stable_width: DEFAULT_STABLE_WIDTH,
        }
    }
}

/// Disk widget that displays icon, usage, and opens the shared system popover.
pub struct DiskWidget {
    base: BaseWidget,
    system_callback_id: CallbackId,
    path: String,
}

impl DiskWidget {
    pub fn new(config: DiskConfig) -> Self {
        // Instance name first: BaseWidget reads show_if and click handlers by it.
        let base = BaseWidget::new(&[&config.name, widget::DISK]);
        wire_system_popover(&base);
        Self::build(config, base)
    }

    /// Create a passive Disk widget for use in a merge group.
    pub fn new_passive(config: DiskConfig) -> Self {
        let base = BaseWidget::new_passive(&[&config.name, widget::DISK]);
        Self::build(config, base)
    }

    fn build(config: DiskConfig, base: BaseWidget) -> Self {
        let icon_handle = base.add_icon("disk-symbolic", &[widget::DISK_ICON]);
        icon_handle.widget().set_visible(config.show_icon);

        let is_vertical = ConfigManager::global().bar_position().is_vertical();
        let label = base.add_label(None, &[widget::DISK_LABEL, class::VCENTER_CAPS]);
        if config.stable_width {
            label.set_width_chars(disk_label_width(&config.format, is_vertical));
        }

        let system_service = SystemService::global();
        system_service.watch_path(&config.path);
        let path = config.path.clone();
        let system_callback_id = {
            let container = base.widget().clone();
            system_service.connect(move |snapshot| {
                update_disk_widget(
                    &container,
                    &icon_handle,
                    &label,
                    &config,
                    is_vertical,
                    snapshot.path_usage.get(&config.path),
                );
            })
        };

        Self {
            base,
            system_callback_id,
            path,
        }
    }

    pub fn widget(&self) -> &gtk4::Box {
        self.base.widget()
    }

    pub(crate) fn edge_interaction(&self) -> Option<crate::widgets::EdgeInteraction> {
        self.base.edge_interaction()
    }
}

impl Drop for DiskWidget {
    fn drop(&mut self) {
        let system_service = SystemService::global();
        system_service.disconnect(self.system_callback_id);
        system_service.unwatch_path(&self.path);
    }
}

fn disk_label_width(format: &DiskFormat, is_vertical: bool) -> i32 {
    match (format, is_vertical) {
        (_, true) => VERTICAL_METRIC_CHARS,
        (DiskFormat::Percentage, _) => 3,  // 99%
        (DiskFormat::Free, _) => 7,        // 1023.9G
        (DiskFormat::UsedOfTotal, _) => 0, // sized from the drive on update
    }
}

fn format_disk(usage: &SpaceUsage, format: &DiskFormat, is_vertical: bool) -> String {
    if is_vertical {
        return format_vertical_metric(usage.percent(), '%');
    }
    match format {
        DiskFormat::Percentage => format!("{:.0}%", usage.percent()),
        DiskFormat::Free => format_bytes(usage.available),
        DiskFormat::UsedOfTotal => format_used_of_total(usage.used, usage.total),
    }
}

fn update_disk_widget(
    container: &gtk4::Box,
    icon_handle: &IconHandle,
    label: &Label,
    config: &DiskConfig,
    is_vertical: bool,
    usage: Option<&Option<SpaceUsage>>,
) {
    let tooltips = TooltipManager::global();
    let usage = match usage {
        Some(Some(usage)) => usage,
        pending_or_failed => {
            let (text, status) = match pending_or_failed {
                None => ("--", "loading"),
                _ => ("?", "unavailable"),
            };
            label.set_label(text);
            container.remove_css_class(widget::DISK_HIGH);
            icon_handle.remove_css_class(widget::DISK_HIGH);
            tooltips.set_styled_tooltip(container, &format!("Disk {}: {status}", config.path));
            return;
        }
    };

    if usage.percent() >= DISK_HIGH_THRESHOLD {
        container.add_css_class(widget::DISK_HIGH);
        icon_handle.add_css_class(widget::DISK_HIGH);
    } else {
        container.remove_css_class(widget::DISK_HIGH);
        icon_handle.remove_css_class(widget::DISK_HIGH);
    }

    if config.stable_width && !is_vertical && config.format == DiskFormat::UsedOfTotal {
        // A full drive is the widest text for this total, e.g. "1.8/1.8T".
        let full = format_used_of_total(usage.total, usage.total);
        label.set_width_chars(full.chars().count() as i32);
    }
    label.set_label(&format_disk(usage, &config.format, is_vertical));
    tooltips.set_styled_tooltip(
        container,
        &format!(
            "Disk {}: {:.1}%\n{} / {}\n{} free",
            config.path,
            usage.percent(),
            format_bytes_long(usage.used),
            format_bytes_long(usage.total),
            format_bytes_long(usage.available)
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn test_disk_config_parses_options() {
        let mut options = std::collections::HashMap::new();
        options.insert("path".to_string(), toml::Value::String("/home".into()));
        options.insert("format".to_string(), toml::Value::String("FREE".into()));
        assert_eq!(DiskFormat::from_str("both"), DiskFormat::UsedOfTotal);
        let config = DiskConfig::from_entry(&WidgetEntry {
            name: "disk".to_string(),
            options,
        });
        assert_eq!(config.path, "/home");
        assert_eq!(config.format, DiskFormat::Free);
        assert!(config.show_icon);

        let defaults = DiskConfig::from_entry(&WidgetEntry {
            name: "disk".to_string(),
            options: Default::default(),
        });
        assert_eq!(defaults.path, "/");
        assert_eq!(defaults.format, DiskFormat::Percentage);
        assert!(!defaults.stable_width);

        let mut options = std::collections::HashMap::new();
        options.insert("path".to_string(), toml::Value::String("/mnt".into()));
        let named = DiskConfig::from_entry(&WidgetEntry {
            name: "disk-mnt".to_string(),
            options,
        });
        assert_eq!(named.name, "disk-mnt");
        assert_eq!(named.path, "/mnt");
    }

    #[test]
    fn test_disk_config_expands_tilde() {
        let mut options = std::collections::HashMap::new();
        options.insert("path".to_string(), toml::Value::String("~/data".into()));
        let config = DiskConfig::from_entry(&WidgetEntry {
            name: "disk".to_string(),
            options,
        });
        // Read HOME rather than setting it; env mutation races other tests.
        match std::env::var("HOME") {
            Ok(home) => assert_eq!(config.path, format!("{home}/data")),
            Err(_) => assert_eq!(config.path, "~/data"),
        }
    }

    #[test]
    fn test_format_disk_fits_stable_width() {
        let usage = SpaceUsage {
            total: 460 * GIB,
            used: 337 * GIB,
            available: 106 * GIB,
        };
        assert_eq!(format_disk(&usage, &DiskFormat::Percentage, false), "76%");
        assert_eq!(format_disk(&usage, &DiskFormat::Free, false), "106.0G");
        assert_eq!(
            format_disk(&usage, &DiskFormat::UsedOfTotal, false),
            "337.0/460.0G"
        );
        assert_eq!(format_disk(&usage, &DiskFormat::UsedOfTotal, true), "76%");
        // Used is shown in the total's unit, so small values round down.
        let small = SpaceUsage {
            total: 1024 * GIB,
            used: 900 * 1024 * 1024,
            ..Default::default()
        };
        assert_eq!(
            format_disk(&small, &DiskFormat::UsedOfTotal, false),
            "0.0/1.0T"
        );
        for format in [DiskFormat::Percentage, DiskFormat::Free] {
            let text = format_disk(&usage, &format, false);
            assert!(text.chars().count() <= disk_label_width(&format, false) as usize);
        }
    }
}
