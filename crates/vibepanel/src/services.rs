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
//! - **idle_inhibitor**: System idle prevention
//! - **state**: Persistent state storage (DND, VPN last used, notification history)
//! - **system**: CPU, memory, and system resource monitoring
//! - **gpu**: GPU utilization and VRAM monitoring (AMD sysfs, NVIDIA NVML)
//! - **media**: MPRIS media player control and monitoring
//! - **sleep_watcher**: Shared resume-from-sleep notifications via logind
//! - **weather**: Open-Meteo-backed weather and forecast data

mod wayland;
pub use wayland::{activation, background_effect};

pub mod audio;
pub mod bar_manager;
pub mod bar_visibility;
pub mod battery;
pub mod battery_alert;
pub mod bluetooth;
pub mod brightness;
pub mod calendar;
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

/// Worker-only: distinguish daemon exit from a live daemon's timeout.
pub(crate) fn is_dbus_disconnect(
    error: &gtk4::glib::Error,
    bus: &gtk4::gio::DBusConnection,
    name: &str,
) -> bool {
    use gtk4::gio::DBusError;
    use gtk4::glib::variant::ToVariant;
    match error.kind::<DBusError>() {
        Some(DBusError::ServiceUnknown | DBusError::NameHasNoOwner | DBusError::Disconnected) => {
            true
        }
        // NoReply also means timeout. Keep it visible if the daemon is still alive.
        Some(DBusError::NoReply) => {
            bus.call_sync(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "NameHasOwner",
                Some(&(name,).to_variant()),
                None,
                gtk4::gio::DBusCallFlags::NONE,
                1000,
                None::<&gtk4::gio::Cancellable>,
            )
            .ok()
            .and_then(|reply| reply.get::<(bool,)>())
                == Some((false,))
        }
        _ => false,
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use gio::prelude::*;
    use gtk4::{gio, glib};
    use std::time::Duration;

    pub async fn check_nm_restarts(
        address: &str,
        flags: gio::DBusConnectionFlags,
        proxy: gio::DBusProxy,
        check: impl Fn(bool),
    ) {
        let (changed, changes) = async_channel::unbounded();
        assert!(proxy.name_owner().is_none());
        let name = proxy.name().unwrap();
        proxy.connect_local("notify::g-name-owner", false, move |values| {
            changed
                .try_send(values[0].get::<gio::DBusProxy>().unwrap().name_owner())
                .unwrap();
            None
        });
        // Only the service may keep the watcher alive across teardown.
        drop(proxy);
        let next_owner = async || {
            glib::future_with_timeout(Duration::from_secs(5), changes.recv())
                .await
                .unwrap()
                .unwrap()
        };
        for _ in 0..2 {
            let server = gio::DBusConnection::for_address_future(address, flags, None)
                .await
                .unwrap();
            server
                .call_future(
                    Some("org.freedesktop.DBus"),
                    "/org/freedesktop/DBus",
                    "org.freedesktop.DBus",
                    "RequestName",
                    Some(&(name.as_str(), 0u32).to_variant()),
                    None,
                    gio::DBusCallFlags::NONE,
                    5000,
                )
                .await
                .unwrap();
            assert_eq!(next_owner().await, server.unique_name());
            check(true);
            server.close_future().await.unwrap();
            assert!(next_owner().await.is_none());
            check(false);
        }
    }
}
