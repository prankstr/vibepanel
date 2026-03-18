//! Network speed widget - displays live download/upload speeds via the
//! shared `SystemService`.
//!
//! The SystemService polls system metrics at regular intervals and exposes
//! canonical snapshots; this widget subscribes to those snapshots and renders
//! icon/text/tooltip accordingly.
//!
//! Uses:
//! - `IconsService` (via BaseWidget) for themed network icon
//! - `TooltipManager` for styled tooltips
//! - Shared popover with CPU/Memory widgets for detailed system info

use gtk4::Label;
use gtk4::prelude::*;
use vibepanel_core::config::WidgetEntry;

use crate::services::callbacks::CallbackId;
use crate::services::icons::IconHandle;
use crate::services::system::{SystemService, SystemSnapshot, format_speed};
use crate::services::tooltip::TooltipManager;
use crate::styles::{class, widget};
use crate::widgets::base::BaseWidget;
use crate::widgets::system_popover::SystemPopoverBinding;
use crate::widgets::{WidgetConfig, warn_unknown_options};

/// Default configuration values
const DEFAULT_SHOW_ICON: bool = true;
const DEFAULT_SHOW_ARROWS: bool = true;

/// Baseline reference string for Pango width measurement.
///
/// Digit `8` is the widest in most proportional fonts; "888.8 KB/s" is the
/// widest realistic output from `format_speed()` (KB/s and MB/s both produce
/// up to 4 digits + unit). Each speed label is measured against this string
/// on `connect_realize` and given a minimum-width floor via `set_size_request`.
/// If actual content is wider, the label gracefully grows.
const SPEED_BASELINE: &str = "888.8 KB/s";

/// Network speed display format options.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum NetworkSpeedFormat {
    /// Show both download and upload speeds.
    #[default]
    Both,
    /// Show download speed only.
    Download,
    /// Show upload speed only.
    Upload,
}

impl NetworkSpeedFormat {
    /// Parse from a string value.
    fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "download" | "dl" => Self::Download,
            "upload" | "ul" => Self::Upload,
            _ => Self::Both,
        }
    }
}

/// Configuration for the Network widget.
#[derive(Debug, Clone)]
pub struct NetworkSpeedConfig {
    /// Whether to show an icon.
    pub show_icon: bool,
    /// Whether to show ↓/↑ direction arrows.
    pub show_arrows: bool,
    /// Display format: download, upload, or both.
    pub format: NetworkSpeedFormat,
}

impl WidgetConfig for NetworkSpeedConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options(
            "network_speed",
            entry,
            &["show_icon", "show_arrows", "format"],
        );

        let show_icon = entry
            .options
            .get("show_icon")
            .and_then(|v| v.as_bool())
            .unwrap_or(DEFAULT_SHOW_ICON);

        let show_arrows = entry
            .options
            .get("show_arrows")
            .and_then(|v| v.as_bool())
            .unwrap_or(DEFAULT_SHOW_ARROWS);

        let format = entry
            .options
            .get("format")
            .and_then(|v| v.as_str())
            .map(NetworkSpeedFormat::from_str)
            .unwrap_or_default();

        Self {
            show_icon,
            show_arrows,
            format,
        }
    }
}

impl Default for NetworkSpeedConfig {
    fn default() -> Self {
        Self {
            show_icon: DEFAULT_SHOW_ICON,
            show_arrows: DEFAULT_SHOW_ARROWS,
            format: NetworkSpeedFormat::default(),
        }
    }
}

/// Network throughput widget that displays download/upload speeds and opens a
/// shared system popover on click.
pub struct NetworkSpeedWidget {
    /// Shared base widget container.
    base: BaseWidget,
    /// Callback ID for SystemService, used to disconnect on drop.
    system_callback_id: CallbackId,
}

impl NetworkSpeedWidget {
    /// Create a new Network widget with the given configuration.
    pub fn new(config: NetworkSpeedConfig) -> Self {
        let base = BaseWidget::new(&[widget::NETWORK_SPEED]);

        base.set_tooltip("Network: unknown");

        let icon_handle = base.add_icon(
            "network-transmit-receive-symbolic",
            &[widget::NETWORK_SPEED_ICON],
        );

        // Build download labels (arrow + speed) if format includes download
        let dl_arrow = if config.show_arrows
            && matches!(
                config.format,
                NetworkSpeedFormat::Both | NetworkSpeedFormat::Download
            ) {
            let label = base.add_label(Some("↓"), &[widget::NETWORK_SPEED_DL_ARROW]);
            Some(label)
        } else {
            None
        };

        let dl_label = if matches!(
            config.format,
            NetworkSpeedFormat::Both | NetworkSpeedFormat::Download
        ) {
            let label =
                base.add_label(None, &[widget::NETWORK_SPEED_DL_LABEL, class::VCENTER_CAPS]);
            setup_baseline_sizing(&label);
            Some(label)
        } else {
            None
        };

        // Build upload labels (arrow + speed) if format includes upload
        let ul_arrow = if config.show_arrows
            && matches!(
                config.format,
                NetworkSpeedFormat::Both | NetworkSpeedFormat::Upload
            ) {
            let label = base.add_label(Some("↑"), &[widget::NETWORK_SPEED_UL_ARROW]);
            Some(label)
        } else {
            None
        };

        let ul_label = if matches!(
            config.format,
            NetworkSpeedFormat::Both | NetworkSpeedFormat::Upload
        ) {
            let label =
                base.add_label(None, &[widget::NETWORK_SPEED_UL_LABEL, class::VCENTER_CAPS]);
            setup_baseline_sizing(&label);
            Some(label)
        } else {
            None
        };

        let popover_binding = SystemPopoverBinding::new(&base);

        icon_handle.widget().set_visible(config.show_icon);

        let system_service = SystemService::global();
        let system_callback_id = {
            let container = base.widget().clone();
            let icon_handle = icon_handle.clone();
            let dl_label = dl_label.clone();
            let ul_label = ul_label.clone();
            let dl_arrow = dl_arrow.clone();
            let ul_arrow = ul_arrow.clone();
            let show_icon = config.show_icon;
            let format = config.format.clone();
            let show_arrows = config.show_arrows;
            let popover_binding = popover_binding.clone();

            system_service.connect(move |snapshot: &SystemSnapshot| {
                update_network_widget(
                    &container,
                    &icon_handle,
                    dl_arrow.as_ref(),
                    dl_label.as_ref(),
                    ul_arrow.as_ref(),
                    ul_label.as_ref(),
                    show_icon,
                    show_arrows,
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

impl Drop for NetworkSpeedWidget {
    fn drop(&mut self) {
        SystemService::global().disconnect(self.system_callback_id);
    }
}

/// Set up Pango-based baseline width measurement for a speed label.
///
/// On `connect_realize`, measures the baseline reference string ("888.8 KB/s")
/// with the label's actual font and sets a minimum width floor via
/// `set_size_request`. This prevents the label from jittering as speed values
/// change between e.g. "0 B/s" and "1.5 MB/s".
///
/// The label is left-aligned (`xalign=0.0`) so that text hugs the directional
/// arrow, with any extra padding appearing on the right where digits grow into.
fn setup_baseline_sizing(label: &Label) {
    label.set_xalign(0.0);
    label.connect_realize(|label| {
        let layout = label.create_pango_layout(Some(SPEED_BASELINE));
        let (width, _height) = layout.pixel_size();
        label.set_size_request(width, -1);
    });
}

/// Update the Network speed widget visuals from a system snapshot.
#[allow(clippy::too_many_arguments)]
fn update_network_widget(
    container: &gtk4::Box,
    icon_handle: &IconHandle,
    dl_arrow: Option<&Label>,
    dl_label: Option<&Label>,
    ul_arrow: Option<&Label>,
    ul_label: Option<&Label>,
    show_icon: bool,
    _show_arrows: bool,
    format: &NetworkSpeedFormat,
    snapshot: &SystemSnapshot,
) {
    if !snapshot.available {
        if show_icon {
            icon_handle.widget().set_visible(true);
        }
        if let Some(dl) = dl_label {
            dl.set_label("?");
            dl.set_visible(true);
        }
        if let Some(ul) = ul_label {
            ul.set_label("?");
            ul.set_visible(true);
        }

        let tooltip_manager = TooltipManager::global();
        tooltip_manager.set_styled_tooltip(container, "Network: Service unavailable");
        return;
    }

    icon_handle.widget().set_visible(show_icon);

    let dl_text = format_speed(snapshot.net_download_speed);
    let ul_text = format_speed(snapshot.net_upload_speed);

    if let Some(dl) = dl_label {
        dl.set_label(&dl_text);
        dl.set_visible(true);
    }
    if let Some(dl_a) = dl_arrow {
        dl_a.set_visible(true);
    }

    if let Some(ul) = ul_label {
        ul.set_label(&ul_text);
        ul.set_visible(true);
    }
    if let Some(ul_a) = ul_arrow {
        ul_a.set_visible(true);
    }

    let tooltip = match format {
        NetworkSpeedFormat::Both => format!("Download: {}\nUpload: {}", dl_text, ul_text),
        NetworkSpeedFormat::Download => format!("Download: {}", dl_text),
        NetworkSpeedFormat::Upload => format!("Upload: {}", ul_text),
    };
    let tooltip_manager = TooltipManager::global();
    tooltip_manager.set_styled_tooltip(container, &tooltip);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_network_speed_config_defaults() {
        let entry = WidgetEntry {
            name: "network_speed".to_string(),
            options: Default::default(),
        };
        let config = NetworkSpeedConfig::from_entry(&entry);
        assert!(config.show_icon);
        assert!(config.show_arrows);
        assert_eq!(config.format, NetworkSpeedFormat::Both);
    }

    #[test]
    fn test_network_speed_config_custom() {
        let mut options = std::collections::HashMap::new();
        options.insert("show_icon".to_string(), toml::Value::Boolean(false));
        options.insert("show_arrows".to_string(), toml::Value::Boolean(false));
        options.insert(
            "format".to_string(),
            toml::Value::String("download".to_string()),
        );

        let entry = WidgetEntry {
            name: "network_speed".to_string(),
            options,
        };
        let config = NetworkSpeedConfig::from_entry(&entry);
        assert!(!config.show_icon);
        assert!(!config.show_arrows);
        assert_eq!(config.format, NetworkSpeedFormat::Download);
    }

    #[test]
    fn test_network_speed_format_from_str() {
        assert_eq!(
            NetworkSpeedFormat::from_str("both"),
            NetworkSpeedFormat::Both
        );
        assert_eq!(
            NetworkSpeedFormat::from_str("Both"),
            NetworkSpeedFormat::Both
        );
        assert_eq!(
            NetworkSpeedFormat::from_str("download"),
            NetworkSpeedFormat::Download
        );
        assert_eq!(
            NetworkSpeedFormat::from_str("Download"),
            NetworkSpeedFormat::Download
        );
        assert_eq!(
            NetworkSpeedFormat::from_str("dl"),
            NetworkSpeedFormat::Download
        );
        assert_eq!(
            NetworkSpeedFormat::from_str("upload"),
            NetworkSpeedFormat::Upload
        );
        assert_eq!(
            NetworkSpeedFormat::from_str("Upload"),
            NetworkSpeedFormat::Upload
        );
        assert_eq!(
            NetworkSpeedFormat::from_str("ul"),
            NetworkSpeedFormat::Upload
        );
        assert_eq!(
            NetworkSpeedFormat::from_str("unknown"),
            NetworkSpeedFormat::Both
        );
    }
}
