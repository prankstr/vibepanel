//! Shared runtime services for the vibepanel bar.
//!
//! This module provides long-lived, process-wide services that can be
//! shared across multiple widgets and windows (e.g. multi-monitor bars).
//!
//! ## Services
//!
//! - **battery**: UPower-backed battery state monitoring
//! - **config_manager**: Configuration hot-reload with file watching
//! - **icons**: Icon theme management (Material Symbols font, icon name mapping)
//! - **tooltip**: Styled GTK tooltips
//! - **surfaces**: Shared surface styling for popovers, menus, overlays
//! - **compositor**: Pluggable compositor backend abstraction
//! - **workspaces**: Workspace state monitoring
//! - **window_title**: Focused window title monitoring
//! - **tray**: StatusNotifierItem host for system tray icons
//! - **vpn**: VPN connection management via NetworkManager
//! - **idle_inhibitor**: System idle/sleep prevention
//! - **state**: Persistent state storage (DND, VPN last used, notification history)
//! - **system**: CPU, memory, and system resource monitoring
//! - **gpu**: GPU utilization and VRAM monitoring (AMD sysfs, NVIDIA NVML)
//! - **media**: MPRIS media player control and monitoring
//! - **sleep_watcher**: Shared resume-from-sleep notifications via logind
//! - **weather**: Open-Meteo-backed weather and forecast data

pub mod audio;
pub mod background_effect;
pub mod bar_manager;
pub mod battery;
pub mod battery_alert;
pub mod bluetooth;
pub mod brightness;
pub mod callbacks;
pub mod cava;
pub mod compositor;
pub mod config_manager;
pub mod desktop_notification;
pub mod gpu;
pub mod icons;
pub mod idle_inhibitor;
pub mod ipc;
pub mod media;
pub mod media_ipc;
pub mod network;
pub mod notification;
pub mod power_profile;
pub mod sleep_watcher;
pub mod state;
pub mod surfaces;
pub mod system;
pub mod tooltip;
pub mod tray;
pub mod updates;
pub mod vpn;
pub mod vpn_secret_agent;
pub mod wallpaper;
pub mod weather;
pub mod window_list;
pub mod window_title;
pub mod workspace;
