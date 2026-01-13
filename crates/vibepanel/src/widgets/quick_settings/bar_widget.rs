//! Quick Settings bar widget - slim indicator that toggles the
//! global Quick Settings window.
//!
//! For Phase 1 this widget only renders a basic icon and toggles the
//! window when clicked. Wi-Fi status and other indicators will be
//! wired up in later phases.

use gtk4::gdk::BUTTON_PRIMARY;
use gtk4::prelude::*;
use gtk4::{Box as GtkBox, GestureClick};
use tracing::debug;

use super::QuickSettingsWindowHandle;
use super::audio_card::volume_icon_name;
use super::bluetooth_card::bt_icon_name;
use super::vpn_card::vpn_icon_name;
use super::wifi_card::wifi_icon_name;
use crate::services::audio::{AudioService, AudioSnapshot};
use crate::services::bluetooth::{BluetoothService, BluetoothSnapshot};
use crate::services::config_manager::ConfigManager;
use crate::services::network::{NetworkService, NetworkSnapshot};
use crate::services::tooltip::TooltipManager;
use crate::services::vpn::{VpnService, VpnSnapshot};
use crate::styles::{icon, qs, state, widget};
use crate::widgets::BaseWidget;
use crate::widgets::WidgetConfig;
use crate::widgets::warn_unknown_options;
use vibepanel_core::config::WidgetEntry;

/// Configuration for which cards are shown in Quick Settings.
///
/// All cards default to `true` (shown). Users can disable specific
/// cards they don't need in their config.toml:
///
/// ```toml
/// [[widgets.right]]
/// name = "quick_settings"
/// vpn = false
/// idle_inhibitor = false
/// ```
#[derive(Debug, Clone)]
pub struct QuickSettingsCardsConfig {
    pub wifi: bool,
    pub bluetooth: bool,
    pub vpn: bool,
    pub idle_inhibitor: bool,
    pub updates: bool,
    pub audio: bool,
    pub mic: bool,
    pub brightness: bool,
    pub power: bool,
}

impl Default for QuickSettingsCardsConfig {
    fn default() -> Self {
        Self {
            wifi: true,
            bluetooth: true,
            vpn: true,
            idle_inhibitor: true,
            updates: true,
            audio: true,
            mic: true,
            brightness: true,
            power: true,
        }
    }
}

/// Configuration for the Quick Settings widget.
#[derive(Debug, Clone, Default)]
pub struct QuickSettingsConfig {
    /// Which cards to show in the Quick Settings panel.
    pub cards: QuickSettingsCardsConfig,
    /// Custom background color for this widget.
    pub color: Option<String>,
}

impl WidgetConfig for QuickSettingsConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        let known_options = &[
            "wifi",
            "bluetooth",
            "vpn",
            "idle_inhibitor",
            "updates",
            "audio",
            "mic",
            "brightness",
            "power",
        ];
        warn_unknown_options("quick_settings", entry, known_options);

        let get_bool = |key: &str| -> bool {
            entry
                .options
                .get(key)
                .and_then(|v| v.as_bool())
                .unwrap_or(true) // default to true (shown)
        };

        Self {
            cards: QuickSettingsCardsConfig {
                wifi: get_bool("wifi"),
                bluetooth: get_bool("bluetooth"),
                vpn: get_bool("vpn"),
                idle_inhibitor: get_bool("idle_inhibitor"),
                updates: get_bool("updates"),
                audio: get_bool("audio"),
                mic: get_bool("mic"),
                brightness: get_bool("brightness"),
                power: get_bool("power"),
            },
            color: entry.color.clone(),
        }
    }
}

/// Bar-side Quick Settings indicator.
pub struct QuickSettingsWidget {
    base: BaseWidget,
}

impl QuickSettingsWidget {
    pub fn new(cfg: QuickSettingsConfig, qs_window: QuickSettingsWindowHandle) -> Self {
        let cards = &cfg.cards;
        let base = BaseWidget::new(&[widget::QUICK_SETTINGS], cfg.color);

        // Build icons only for enabled cards (order: Audio, Bluetooth, Wi-Fi, VPN)
        // Audio icon
        if cards.audio {
            let audio_snapshot = AudioService::global().current();
            let audio_icon_name_initial =
                volume_icon_name(audio_snapshot.volume, audio_snapshot.muted);
            let audio_icon = base.add_icon(audio_icon_name_initial, &[icon::ICON, icon::TEXT]);

            // Subscribe to AudioService updates
            let audio_icon_handle = audio_icon.clone();
            AudioService::global().connect(move |snapshot: &AudioSnapshot| {
                let widget = audio_icon_handle.widget();

                if !snapshot.available {
                    widget.add_css_class(state::SERVICE_UNAVAILABLE);
                    audio_icon_handle.set_icon("audio-volume-muted");
                    TooltipManager::global()
                        .set_styled_tooltip(&widget, "Audio: Service unavailable");
                    return;
                }

                // Backend present but volume control unavailable (e.g., Asahi before playback)
                if !snapshot.control_available {
                    widget.add_css_class(state::SERVICE_UNAVAILABLE);
                    audio_icon_handle.set_icon("audio-volume-muted-symbolic");
                    TooltipManager::global()
                        .set_styled_tooltip(&widget, "Volume control unavailable");
                    return;
                }

                widget.remove_css_class(state::SERVICE_UNAVAILABLE);

                let icon_name = volume_icon_name(snapshot.volume, snapshot.muted);
                audio_icon_handle.set_icon(icon_name);

                let tooltip = if snapshot.muted {
                    "Muted".to_string()
                } else {
                    format!("Volume: {}%", snapshot.volume)
                };
                TooltipManager::global().set_styled_tooltip(&widget, &tooltip);
            });
        }

        // Bluetooth icon
        if cards.bluetooth {
            let bt_snapshot = BluetoothService::global().snapshot();
            let bt_powered = bt_snapshot.powered;
            let bt_connected_devices = bt_snapshot.connected_devices;
            let bt_icon_name_initial = bt_icon_name(bt_powered, bt_connected_devices);
            let bt_icon = base.add_icon(bt_icon_name_initial, &[icon::ICON, icon::TEXT]);

            if bt_connected_devices > 0 {
                bt_icon.widget().add_css_class(state::ICON_ACTIVE);
            }

            // Subscribe to BluetoothService updates
            let bt_icon_handle = bt_icon.clone();
            BluetoothService::global().connect(move |snapshot: &BluetoothSnapshot| {
                let widget = bt_icon_handle.widget();

                if !snapshot.has_adapter && snapshot.is_ready {
                    widget.add_css_class(state::SERVICE_UNAVAILABLE);
                    widget.remove_css_class(state::ICON_ACTIVE);
                    bt_icon_handle.set_icon("bluetooth-disabled");
                    TooltipManager::global()
                        .set_styled_tooltip(&widget, "Bluetooth: No adapter found");
                    return;
                }

                widget.remove_css_class(state::SERVICE_UNAVAILABLE);

                let powered = snapshot.powered;
                let connected_devices = snapshot.connected_devices;

                let icon_name = bt_icon_name(powered, connected_devices);
                bt_icon_handle.set_icon(icon_name);

                if connected_devices > 0 {
                    widget.add_css_class(state::ICON_ACTIVE);
                } else {
                    widget.remove_css_class(state::ICON_ACTIVE);
                }

                let tooltip = if connected_devices > 0 {
                    let mut lines: Vec<String> = snapshot
                        .devices
                        .iter()
                        .filter(|d| d.connected)
                        .map(|d| d.name.clone())
                        .collect();
                    if lines.is_empty() {
                        lines.push("Bluetooth On".to_string());
                    }
                    lines.join("\n")
                } else if powered {
                    "Bluetooth On".to_string()
                } else {
                    "Bluetooth Off".to_string()
                };
                TooltipManager::global().set_styled_tooltip(&widget, &tooltip);
            });
        }

        // Wi-Fi icon
        if cards.wifi {
            let wifi_snapshot = NetworkService::global().snapshot();
            let wifi_enabled = wifi_snapshot.wifi_enabled.unwrap_or(false);
            let wifi_connected = wifi_snapshot.connected;
            let wifi_icon_name_initial = wifi_icon_name(wifi_connected, wifi_enabled);
            let wifi_icon = base.add_icon(wifi_icon_name_initial, &[icon::ICON, icon::TEXT]);

            if !wifi_enabled {
                wifi_icon.widget().add_css_class(qs::WIFI_DISABLED_ICON);
            }
            if wifi_enabled && wifi_connected {
                wifi_icon.widget().add_css_class(state::ICON_ACTIVE);
            }

            // Subscribe to NetworkService updates
            let wifi_icon_handle = wifi_icon.clone();
            NetworkService::global().connect(move |snapshot: &NetworkSnapshot| {
                let widget = wifi_icon_handle.widget();

                if !snapshot.available {
                    widget.add_css_class(state::SERVICE_UNAVAILABLE);
                    widget.remove_css_class(qs::WIFI_DISABLED_ICON);
                    widget.remove_css_class(state::ICON_ACTIVE);
                    wifi_icon_handle.set_icon("network-wireless-offline");
                    TooltipManager::global()
                        .set_styled_tooltip(&widget, "Wi-Fi: Service unavailable");
                    return;
                }
                widget.remove_css_class(state::SERVICE_UNAVAILABLE);

                let enabled = snapshot.wifi_enabled.unwrap_or(false);
                let connected = snapshot.connected;

                let icon_name = wifi_icon_name(connected, enabled);
                wifi_icon_handle.set_icon(icon_name);

                if !enabled {
                    widget.add_css_class(qs::WIFI_DISABLED_ICON);
                } else {
                    widget.remove_css_class(qs::WIFI_DISABLED_ICON);
                }

                if enabled && connected {
                    widget.add_css_class(state::ICON_ACTIVE);
                } else {
                    widget.remove_css_class(state::ICON_ACTIVE);
                }

                let tooltip = if connected {
                    let ssid = snapshot.ssid.as_deref().unwrap_or("Connected");
                    let strength = snapshot.strength;
                    if strength > 0 {
                        format!("{}\nSignal: {}%", ssid, strength)
                    } else {
                        ssid.to_string()
                    }
                } else {
                    "Disconnected".to_string()
                };
                TooltipManager::global().set_styled_tooltip(&widget, &tooltip);
            });
        }

        // VPN icon
        if cards.vpn {
            let vpn_snapshot = VpnService::global().snapshot();
            let vpn_any_active = vpn_snapshot.any_active;
            let vpn_icon_name_initial = vpn_icon_name(vpn_any_active);
            let vpn_icon = base.add_icon(vpn_icon_name_initial, &[icon::ICON, icon::TEXT]);

            if vpn_any_active {
                vpn_icon.widget().add_css_class(state::ICON_ACTIVE);
            }

            // Subscribe to VpnService updates
            let vpn_icon_handle = vpn_icon.clone();
            VpnService::global().connect(move |snapshot: &VpnSnapshot| {
                let widget = vpn_icon_handle.widget();

                if !snapshot.available {
                    widget.add_css_class(state::SERVICE_UNAVAILABLE);
                    widget.remove_css_class(state::ICON_ACTIVE);
                    vpn_icon_handle.set_icon("network-vpn-disabled");
                    TooltipManager::global()
                        .set_styled_tooltip(&widget, "VPN: Service unavailable");
                    return;
                }
                widget.remove_css_class(state::SERVICE_UNAVAILABLE);

                let icon_name = vpn_icon_name(snapshot.any_active);
                vpn_icon_handle.set_icon(icon_name);

                if snapshot.any_active {
                    widget.add_css_class(state::ICON_ACTIVE);
                } else {
                    widget.remove_css_class(state::ICON_ACTIVE);
                }

                let tooltip = if snapshot.any_active {
                    let active_names: Vec<String> = snapshot
                        .connections
                        .iter()
                        .filter(|c| c.active)
                        .map(|c| c.name.clone())
                        .collect();
                    if active_names.is_empty() {
                        "VPN Connected".to_string()
                    } else {
                        active_names.join("\n")
                    }
                } else {
                    "VPN Disconnected".to_string()
                };
                TooltipManager::global().set_styled_tooltip(&widget, &tooltip);
            });
        }

        // Ensure the root box is clickable.
        base.widget().add_css_class(state::CLICKABLE);

        // Gesture to toggle the Quick Settings window when clicked.
        let gesture = GestureClick::new();
        gesture.set_button(BUTTON_PRIMARY);

        {
            let qs_window_handle = qs_window.clone();
            let root = base.widget().clone();
            gesture.connect_pressed(move |gesture, n_press, _x, _y| {
                debug!(
                    "QuickSettingsWidget click: n_press={}, button={}",
                    n_press,
                    gesture.current_button()
                );

                if n_press != 1 || gesture.current_button() != BUTTON_PRIMARY {
                    return;
                }

                if let Some(native) = root.native() {
                    let surface = native.surface();
                    let monitor = surface.as_ref().map(|s| {
                        let display = s.display();
                        display.monitor_at_surface(s)
                    });

                    // Compute widget bounds relative to the native window
                    if let Some(bounds) = root.compute_bounds(&native) {
                        // The bar window is offset from the monitor edge by outer_margin.
                        // Widget bounds are relative to the bar window's (0,0).
                        // To get monitor-relative coordinates, we need to add outer_margin.
                        let outer_margin = ConfigManager::global().bar_outer_margin() as i32;

                        let widget_center_x =
                            (bounds.x() + bounds.width() / 2.0) as i32 + outer_margin;
                        let widget_bottom_y = (bounds.y() + bounds.height()) as i32 + outer_margin;

                        let monitor = monitor.flatten();
                        qs_window_handle.toggle_at(widget_center_x, widget_bottom_y, monitor);
                    } else {
                        // Fallback: toggle without positioning
                        qs_window_handle.toggle_at(0, 0, None);
                    }
                } else {
                    qs_window_handle.toggle_at(0, 0, None);
                }
            });
        }

        base.widget().add_controller(gesture);

        Self { base }
    }

    /// Get the root GTK widget for this bar item.
    pub fn widget(&self) -> &GtkBox {
        self.base.widget()
    }
}
