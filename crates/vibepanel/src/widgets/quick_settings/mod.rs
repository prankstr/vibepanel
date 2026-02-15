//! Quick Settings module - control center panel and supporting components.
//!
//! This module contains:
//! - `bar_widget` - The bar-side Quick Settings indicator widget
//! - `window` - The main Quick Settings window (layer shell surface)
//! - `ui_helpers` - Shared UI builders (cards, rows, etc.)
//! - `components` - Reusable component builders (SliderRowBuilder, etc.)
//! - `network_card` - Network panel logic and icon helpers (Wi-Fi, Ethernet, Mobile)
//! - `bluetooth_card` - Bluetooth panel logic and icon helpers
//! - `vpn_card` - VPN panel logic and icon helpers
//! - `audio_card` - Audio panel logic (volume, sinks)
//! - `mic_card` - Microphone panel logic (input volume, sources)
//! - `brightness_card` - Brightness slider
//! - `idle_inhibitor_card` - Idle inhibitor toggle
//! - `updates_card` - System updates panel
//! - `power_card` - Power menu (shutdown, reboot, etc.)

pub mod audio_card;
pub mod bar_widget;
pub mod bluetooth_card;
pub mod brightness_card;
pub mod components;
pub mod idle_inhibitor_card;
pub mod mic_card;
pub mod network_card;
pub mod power_card;
pub mod ui_helpers;
pub mod updates_card;
pub mod vpn_card;
pub mod window;

pub use bar_widget::{QuickSettingsConfig, QuickSettingsWidget};
pub use window::QuickSettingsWindowHandle;
