//! Wired (Ethernet) device info fetching via NetworkManager D-Bus.

use std::thread;

use tracing::{debug, warn};

use super::{
    ETHERNET_DEVICE_TYPE, IFACE_ACTIVE_CONN, IFACE_DEV, IFACE_WIRED, NM_IFACE, NM_PATH, NM_SERVICE,
    NmService, NmUpdate, send_nm_update, system_dbus_proxy_sync,
};
use gtk4::gio::prelude::*;
use gtk4::glib;

impl NmService {
    /// Get wired device info (interface name and speed) synchronously.
    pub(super) fn get_wired_device_info_sync(path: &str) -> Result<(String, u32), String> {
        let dev_proxy = system_dbus_proxy_sync(NM_SERVICE, path, IFACE_DEV)
            .map_err(|e| format!("Failed to create device proxy: {}", e))?;

        let iface_name = dev_proxy
            .cached_property("Interface")
            .and_then(|v| v.get::<String>())
            .ok_or_else(|| "No Interface property".to_string())?;

        let wired_proxy = system_dbus_proxy_sync(NM_SERVICE, path, IFACE_WIRED)
            .map_err(|e| format!("Failed to create wired proxy: {}", e))?;

        let speed = wired_proxy
            .cached_property("Speed")
            .and_then(|v| v.get::<u32>())
            .unwrap_or(0);

        Ok((iface_name, speed))
    }

    /// Get the primary connection name (Id) from NetworkManager.
    pub(super) fn get_primary_connection_name_sync() -> Option<String> {
        let nm_proxy = system_dbus_proxy_sync(NM_SERVICE, NM_PATH, NM_IFACE).ok()?;

        let primary_conn_path = nm_proxy
            .cached_property("PrimaryConnection")
            .and_then(|v| v.get::<glib::variant::ObjectPath>())?;

        let path_str = primary_conn_path.as_str();
        if path_str == "/" {
            return None;
        }

        let conn_proxy = system_dbus_proxy_sync(NM_SERVICE, path_str, IFACE_ACTIVE_CONN).ok()?;

        conn_proxy
            .cached_property("Id")
            .and_then(|v| v.get::<String>())
    }

    /// Discover wired device and fetch its info in a background thread.
    pub(super) fn fetch_wired_device_info() {
        thread::spawn(move || {
            #[cfg(debug_assertions)]
            if std::path::Path::new("/tmp/vibepanel-debug-wired").exists() {
                debug!("Using mock wired device info (debug mode)");
                send_nm_update(NmUpdate::EthernetDeviceExists);
                send_nm_update(NmUpdate::WiredDeviceInfo {
                    iface_name: Some("enp0s31f6".to_string()),
                    conn_name: Some("Wired connection 1".to_string()),
                    speed: Some(1000),
                });
                return;
            }

            let device_paths = match Self::get_device_paths_sync() {
                Ok(paths) => paths,
                Err(e) => {
                    warn!("Failed to get device paths for wired lookup: {}", e);
                    send_nm_update(NmUpdate::WiredDeviceInfo {
                        iface_name: None,
                        conn_name: None,
                        speed: None,
                    });
                    return;
                }
            };

            for path in device_paths {
                match Self::get_device_type_sync(&path) {
                    Ok((dtype, _)) if dtype == ETHERNET_DEVICE_TYPE => {
                        match Self::get_wired_device_info_sync(&path) {
                            Ok((iface_name, speed)) => {
                                let conn_name = Self::get_primary_connection_name_sync();
                                debug!(
                                    "Found wired device: {} ({} Mb/s), connection: {:?}",
                                    iface_name, speed, conn_name
                                );
                                send_nm_update(NmUpdate::WiredDeviceInfo {
                                    iface_name: Some(iface_name),
                                    conn_name,
                                    speed: if speed > 0 { Some(speed) } else { None },
                                });
                                return;
                            }
                            Err(e) => {
                                debug!("Failed to get wired device info for {}: {}", path, e);
                            }
                        }
                    }
                    _ => {}
                }
            }

            send_nm_update(NmUpdate::WiredDeviceInfo {
                iface_name: None,
                conn_name: None,
                speed: None,
            });
        });
    }
}

/// Check if a wired (Ethernet) connection is active.
///
/// In debug builds, overridable via `/tmp/vibepanel-debug-wired`:
/// - Enable: `touch /tmp/vibepanel-debug-wired`
/// - Disable: `rm /tmp/vibepanel-debug-wired`
pub(super) fn is_wired_connected(primary_type: Option<&str>) -> bool {
    #[cfg(debug_assertions)]
    if std::path::Path::new("/tmp/vibepanel-debug-wired").exists() {
        return true;
    }

    primary_type.is_some_and(|t| t == "802-3-ethernet")
}
