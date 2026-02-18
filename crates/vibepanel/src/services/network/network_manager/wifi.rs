//! Wi-Fi device proxy, state management, network scanning, and connection control.

use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use gtk4::gio::{self, prelude::*};
use gtk4::glib::{Variant, VariantTy};
use tracing::{debug, error, warn};

use super::{
    IFACE_AP, IFACE_DEV, IFACE_WIFI, NM_IFACE, NM_SERVICE, NmService, NmUpdate, send_nm_update,
    system_dbus_proxy_sync,
};
use crate::services::network::{SecurityType, WifiNetwork, objpath_to_string};

impl NmService {
    /// Create wifi proxy - called from apply_update on main thread.
    pub(super) fn create_wifi_proxy_from_self(&self, path: &str) {
        // Get a strong Rc to self for the callback.
        let this = NmService::global();
        Self::create_wifi_proxy(&this, path);
    }

    pub(super) fn create_wifi_proxy(this: &Rc<Self>, path: &str) {
        let this_weak = Rc::downgrade(this);
        let path = path.to_string();

        // Get connection from NM proxy
        let Some(nm_proxy) = this.nm_proxy.borrow().clone() else {
            return;
        };

        let connection = nm_proxy.connection();

        // Create the Device.Wireless proxy (for ActiveAccessPoint, scanning, etc.)
        gio::DBusProxy::new(
            &connection,
            gio::DBusProxyFlags::NONE,
            None::<&gio::DBusInterfaceInfo>,
            Some(NM_SERVICE),
            &path,
            IFACE_WIFI,
            None::<&gio::Cancellable>,
            {
                let this_weak = this_weak.clone();
                move |res| {
                    let Some(this) = this_weak.upgrade() else {
                        return;
                    };

                    let proxy = match res {
                        Ok(p) => p,
                        Err(e) => {
                            error!("Failed to create Wi-Fi proxy: {}", e);
                            return;
                        }
                    };

                    this.wifi.proxy.replace(Some(proxy.clone()));

                    // Subscribe to property changes
                    let this_weak = Rc::downgrade(&this);
                    proxy.connect_local("g-properties-changed", false, move |_| {
                        if let Some(this) = this_weak.upgrade() {
                            this.update_state();
                        }
                        None
                    });

                    // Initial state update
                    this.update_state();
                }
            },
        );

        // Create the base Device proxy (for State property — connecting states 40-90).
        gio::DBusProxy::new(
            &connection,
            gio::DBusProxyFlags::NONE,
            None::<&gio::DBusInterfaceInfo>,
            Some(NM_SERVICE),
            &path,
            IFACE_DEV,
            None::<&gio::Cancellable>,
            move |res| {
                let Some(this) = this_weak.upgrade() else {
                    return;
                };

                let proxy = match res {
                    Ok(p) => p,
                    Err(e) => {
                        error!("Failed to create Wi-Fi Device proxy: {}", e);
                        return;
                    }
                };

                this.wifi.device_proxy.replace(Some(proxy.clone()));

                // Read initial state and notify.
                if let Some(state) = proxy.cached_property("State").and_then(|v| v.get::<u32>()) {
                    this.notify_snapshot(|s| s.wifi.device_state = Some(state));
                }

                // Subscribe to property changes for State updates.
                let this_weak = Rc::downgrade(&this);
                proxy.connect_local("g-properties-changed", false, move |_| {
                    if let Some(this) = this_weak.upgrade()
                        && let Some(proxy) = this.wifi.device_proxy.borrow().as_ref()
                        && let Some(state) =
                            proxy.cached_property("State").and_then(|v| v.get::<u32>())
                    {
                        this.notify_snapshot_if(|s| {
                            let new_val = Some(state);
                            let changed = s.wifi.device_state != new_val;
                            s.wifi.device_state = new_val;
                            changed
                        });
                    }
                    None
                });
            },
        );
    }

    pub(super) fn update_state(&self) {
        let Some(wifi) = self.wifi.proxy.borrow().clone() else {
            return;
        };

        // Get active access point path
        let ap_path = wifi
            .cached_property("ActiveAccessPoint")
            .and_then(|v| objpath_to_string(&v));

        let ap_path = match ap_path {
            Some(p) if !p.is_empty() && p != "/" => p,
            _ => {
                // Not connected
                self.set_disconnected();
                return;
            }
        };

        // Fetch AP details in background.
        thread::spawn(move || match Self::get_ap_details_sync(&ap_path) {
            Ok((ssid, strength)) => {
                send_nm_update(NmUpdate::ApDetails { ssid, strength });
            }
            Err(e) => {
                debug!("Failed to get AP details: {}", e);
                send_nm_update(NmUpdate::ApDetailsFailed);
            }
        });
    }

    fn get_ap_details_sync(path: &str) -> Result<(Option<String>, i32), String> {
        let proxy = system_dbus_proxy_sync(NM_SERVICE, path, IFACE_AP)
            .map_err(|e| format!("Failed to create AP proxy: {}", e))?;

        let ssid = proxy.cached_property("Ssid").and_then(|v| {
            // SSID is ay (array of bytes)
            let bytes: Vec<u8> = v.iter().filter_map(|b| b.get::<u8>()).collect();
            String::from_utf8(bytes).ok()
        });

        let strength = proxy
            .cached_property("Strength")
            .and_then(|v| v.get::<u8>())
            .map(|s| s as i32)
            .unwrap_or(0);

        Ok((ssid, strength))
    }

    pub(super) fn set_disconnected(&self) {
        self.notify_snapshot_if(|s| {
            if !s.wifi.connected && s.wifi.ssid.is_none() && s.wifi.strength == 0 {
                return false; // Already disconnected
            }
            s.wifi.connected = false;
            s.wifi.ssid = None;
            s.wifi.strength = 0;
            true
        });
    }

    // Network List Refresh

    pub(super) fn refresh_networks_async(&self) {
        let Some(wifi) = self.wifi.proxy.borrow().clone() else {
            return;
        };

        let known_ssids = Arc::clone(&self.wifi.known_ssids);
        let known_ssids_refresh = Arc::clone(&self.wifi.known_ssids_last_refresh);

        thread::spawn(move || {
            // Get active AP path
            let active_path = wifi
                .cached_property("ActiveAccessPoint")
                .and_then(|v| objpath_to_string(&v))
                .filter(|p| !p.is_empty() && p != "/");

            // Get LastScan timestamp
            let last_scan = wifi
                .cached_property("LastScan")
                .and_then(|v| v.get::<i64>());

            // Get access point paths
            let ap_paths = match Self::get_access_points_sync(&wifi) {
                Ok(paths) => paths,
                Err(e) => {
                    error!("Failed to get access points: {}", e);
                    return;
                }
            };

            // Refresh known SSIDs cache if needed
            Self::refresh_known_ssids_if_needed(&known_ssids, &known_ssids_refresh);

            // Fetch details for each AP
            let mut networks: Vec<WifiNetwork> = Vec::new();
            let known = known_ssids
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();

            for path in ap_paths {
                if let Ok(net) = Self::get_network_details_sync(&path, &active_path, &known) {
                    networks.push(net);
                }
            }

            // Deduplicate by SSID + security
            let deduped = Self::dedupe_networks(networks);

            // Sort: active first, then known, then by strength
            let sorted = Self::sort_networks(deduped);

            // Send update to main thread.
            send_nm_update(NmUpdate::NetworksRefreshed {
                networks: sorted,
                last_scan,
            });
        });
    }

    fn get_access_points_sync(wifi: &gio::DBusProxy) -> Result<Vec<String>, String> {
        let result = wifi
            .call_sync(
                "GetAccessPoints",
                None,
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
            )
            .map_err(|e| format!("GetAccessPoints failed: {}", e))?;

        let paths: Vec<String> = result
            .child_value(0)
            .iter()
            .filter_map(|v| objpath_to_string(&v))
            .collect();

        Ok(paths)
    }

    fn get_network_details_sync(
        path: &str,
        active_path: &Option<String>,
        known_ssids: &HashSet<String>,
    ) -> Result<WifiNetwork, String> {
        let proxy = system_dbus_proxy_sync(NM_SERVICE, path, IFACE_AP)
            .map_err(|e| format!("Failed to create AP proxy: {}", e))?;

        let ssid = proxy.cached_property("Ssid").and_then(|v| {
            let bytes: Vec<u8> = v.iter().filter_map(|b| b.get::<u8>()).collect();
            String::from_utf8(bytes).ok()
        });

        let strength = proxy
            .cached_property("Strength")
            .and_then(|v| v.get::<u8>())
            .map(|s| s as i32)
            .unwrap_or(0);

        // Check security flags
        let flags = proxy
            .cached_property("Flags")
            .and_then(|v| v.get::<u32>())
            .unwrap_or(0);
        let wpa_flags = proxy
            .cached_property("WpaFlags")
            .and_then(|v| v.get::<u32>())
            .unwrap_or(0);
        let rsn_flags = proxy
            .cached_property("RsnFlags")
            .and_then(|v| v.get::<u32>())
            .unwrap_or(0);

        let security = if flags != 0 || wpa_flags != 0 || rsn_flags != 0 {
            SecurityType::Secured
        } else {
            SecurityType::Open
        };

        let ssid_str = ssid.unwrap_or_default();
        let is_active = active_path.as_ref().is_some_and(|ap| ap == path);
        let is_known = known_ssids.contains(&ssid_str) || is_active;

        Ok(WifiNetwork {
            ssid: ssid_str,
            strength,
            security,
            active: is_active,
            known_network_path: None,
            known: is_known,
            path: None,
        })
    }

    fn refresh_known_ssids_if_needed(
        known_ssids: &Arc<Mutex<HashSet<String>>>,
        last_refresh: &Arc<Mutex<Option<Instant>>>,
    ) {
        let now = Instant::now();
        let use_cache = {
            let lr = last_refresh.lock().unwrap_or_else(|e| e.into_inner());
            lr.is_some_and(|t| now.duration_since(t).as_secs() < 30)
        };

        if use_cache {
            return;
        }

        // Query nmcli for saved connections
        let output = Command::new("nmcli")
            .args(["-t", "-f", "NAME,TYPE", "connection", "show"])
            .output();

        let mut ssids = HashSet::new();
        if let Ok(output) = output
            && let Ok(stdout) = String::from_utf8(output.stdout)
        {
            for line in stdout.lines() {
                let parts: Vec<&str> = line.split(':').collect();
                if parts.len() >= 2 {
                    let name = parts[0];
                    let ctype = parts[1];
                    if ctype.contains("wifi") || ctype.contains("wireless") {
                        ssids.insert(name.to_string());
                    }
                }
            }
        }

        *known_ssids.lock().unwrap_or_else(|e| e.into_inner()) = ssids;
        *last_refresh.lock().unwrap_or_else(|e| e.into_inner()) = Some(now);
    }

    fn dedupe_networks(networks: Vec<WifiNetwork>) -> Vec<WifiNetwork> {
        let mut merged: HashMap<(String, SecurityType), WifiNetwork> = HashMap::new();

        for net in networks {
            let key = (net.ssid.clone(), net.security);
            if let Some(existing) = merged.get_mut(&key) {
                existing.active = existing.active || net.active;
                existing.strength = existing.strength.max(net.strength);
                existing.known = existing.known || net.known;
            } else {
                merged.insert(key, net);
            }
        }

        merged.into_values().collect()
    }

    fn sort_networks(mut networks: Vec<WifiNetwork>) -> Vec<WifiNetwork> {
        networks.sort_by(|a, b| {
            // Group: 0 = active, 1 = known, 2 = other
            let group_a = if a.active {
                0
            } else if a.known {
                1
            } else {
                2
            };
            let group_b = if b.active {
                0
            } else if b.known {
                1
            } else {
                2
            };

            group_a
                .cmp(&group_b)
                .then_with(|| b.strength.cmp(&a.strength)) // Descending strength
                .then_with(|| a.ssid.cmp(&b.ssid))
        });

        networks
    }

    // Public API: WiFi Actions

    /// Enable or disable Wi-Fi.
    pub fn set_wifi_enabled(&self, enabled: bool) {
        let Some(nm) = self.nm_proxy.borrow().clone() else {
            return;
        };

        thread::spawn(move || {
            // Set WirelessEnabled property via D-Bus Properties interface
            // Signature is (ssv) - interface name, property name, variant value
            let variant = Variant::tuple_from_iter([
                NM_IFACE.to_variant(),
                "WirelessEnabled".to_variant(),
                enabled.to_variant().to_variant(),
            ]);

            if let Err(e) = nm.call_sync(
                "org.freedesktop.DBus.Properties.Set",
                Some(&variant),
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
            ) {
                error!("Failed to set WirelessEnabled: {}", e);
            }
        });
    }

    /// Request a Wi-Fi scan.
    pub fn scan_networks(&self) {
        if self.wifi.scan_in_progress.get() {
            return;
        }

        let Some(wifi) = self.wifi.proxy.borrow().clone() else {
            return;
        };

        self.wifi.scan_in_progress.set(true);

        // Update snapshot to reflect scanning state
        self.notify_snapshot(|s| s.wifi.scanning = true);

        // RequestScan expects (a{sv}) - empty options dict
        let empty_dict = Variant::parse(
            Some(VariantTy::new("a{sv}").expect("valid GVariant type string")),
            "{}",
        )
        .expect("valid empty dict literal for a{sv}");
        let args = Variant::tuple_from_iter([empty_dict]);

        wifi.call(
            "RequestScan",
            Some(&args),
            gio::DBusCallFlags::NONE,
            30000, // Scanning can take time
            None::<&gio::Cancellable>,
            move |_res| {
                // Callback runs on main GLib loop - request refresh.
                send_nm_update(NmUpdate::RefreshNetworks);
            },
        );
    }

    /// Clear the failed connection state (called when user cancels password dialog).
    pub fn clear_failed_state(&self) {
        *self.wifi.failed_ssid.borrow_mut() = None;
        self.notify_snapshot(|s| {
            s.wifi.failed_ssid = None;
        });
    }

    /// Connect to a Wi-Fi network by SSID.
    ///
    /// Uses `nmcli device wifi connect` to establish the connection.
    /// If a password is provided, it's passed to nmcli.
    ///
    /// # Parameters
    /// - `ssid`: Network name to connect to
    /// - `password`: Optional password for secured networks
    pub fn connect_to_network(&self, ssid: &str, password: Option<&str>) {
        let ssid = ssid.trim().to_string();
        if ssid.is_empty() {
            return;
        }

        // Clear any previous failed state and set connecting state for UI feedback.
        *self.wifi.failed_ssid.borrow_mut() = None;
        *self.wifi.connecting_ssid.borrow_mut() = Some(ssid.clone());
        self.notify_snapshot(|s| {
            s.wifi.failed_ssid = None;
            s.wifi.connecting_ssid = Some(ssid.clone());
        });

        let password = password.map(|s| s.to_string());

        thread::spawn(move || {
            let mut cmd = Command::new("nmcli");
            cmd.args(["device", "wifi", "connect", &ssid]);

            if let Some(ref pw) = password {
                cmd.args(["password", pw]);
            }

            let success = match cmd.output() {
                Ok(output) => {
                    if output.status.success() {
                        true
                    } else {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        warn!("nmcli connect failed for '{}': {}", ssid, stderr.trim());

                        // Delete the failed connection profile that nmcli created.
                        // This prevents showing "Saved" for a network that never connected.
                        let _ = Command::new("nmcli")
                            .args(["connection", "delete", "id", &ssid])
                            .output();

                        false
                    }
                }
                Err(e) => {
                    error!("Failed to run nmcli: {}", e);
                    false
                }
            };

            // Signal that connection attempt finished (success or failure).
            send_nm_update(NmUpdate::ConnectionAttemptFinished { ssid, success });
        });
    }

    /// Disconnect from the current Wi-Fi network.
    pub fn disconnect(&self) {
        let iface = self.wifi.iface_name.borrow().clone();
        let Some(iface) = iface else {
            return;
        };

        thread::spawn(move || {
            if let Err(e) = Command::new("nmcli")
                .args(["device", "disconnect", &iface])
                .output()
            {
                error!("nmcli disconnect failed: {}", e);
            }

            // Request refresh.
            send_nm_update(NmUpdate::RefreshNetworks);
        });
    }

    /// Forget a saved Wi-Fi network.
    pub fn forget_network(&self, ssid: &str) {
        let ssid = ssid.trim().to_string();
        if ssid.is_empty() {
            return;
        }

        let known_ssids_refresh = Arc::clone(&self.wifi.known_ssids_last_refresh);

        thread::spawn(move || {
            if let Err(e) = Command::new("nmcli")
                .args(["connection", "delete", "id", &ssid])
                .output()
            {
                error!("nmcli forget failed: {}", e);
            }

            // Invalidate known SSIDs cache
            *known_ssids_refresh
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;

            // Request refresh.
            send_nm_update(NmUpdate::RefreshNetworks);
        });
    }
}
