//! Wi-Fi device proxy, state management, network scanning, and connection control.

use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use gtk4::gio::{self, prelude::*};
use gtk4::glib::{self, Variant, VariantTy};
use tracing::{debug, error, warn};

use super::{
    IFACE_AP, IFACE_DEV, IFACE_WIFI, NM_IFACE, NM_SERVICE, NmService, NmUpdate, send_nm_update,
    system_dbus_proxy_sync,
};
use crate::services::network::{
    SecurityType, WifiAuthentication, WifiCredentials, WifiNetwork, objpath_to_string,
};

impl NmService {
    pub fn request_active_wifi_credentials<F>(&self, callback: F)
    where
        F: FnOnce(Result<WifiCredentials, String>) + 'static,
    {
        let active_connection = self
            .wifi
            .device_proxy
            .borrow()
            .as_ref()
            .and_then(|proxy| proxy.cached_property("ActiveConnection"))
            .and_then(|value| objpath_to_string(&value))
            .filter(|path| path != "/");

        let Some(active_connection) = active_connection else {
            callback(Err("No active Wi-Fi connection".to_string()));
            return;
        };

        let (sender, receiver) = async_channel::bounded(1);
        thread::spawn(move || {
            let _ = sender.send_blocking(Self::get_wifi_credentials_sync(&active_connection));
        });
        glib::spawn_future_local(async move {
            let result = receiver
                .recv()
                .await
                .unwrap_or_else(|_| Err("Wi-Fi credential lookup failed".to_string()));
            callback(result);
        });
    }

    fn get_wifi_credentials_sync(active_connection: &str) -> Result<WifiCredentials, String> {
        let active_proxy =
            system_dbus_proxy_sync(NM_SERVICE, active_connection, super::IFACE_ACTIVE_CONN)
                .map_err(|e| format!("Failed to open active connection: {e}"))?;
        let connection_path = active_proxy
            .cached_property("Connection")
            .and_then(|value| objpath_to_string(&value))
            .ok_or_else(|| "Active Wi-Fi profile is unavailable".to_string())?;
        let profile_proxy =
            system_dbus_proxy_sync(NM_SERVICE, &connection_path, super::IFACE_SETTINGS_CONN)
                .map_err(|e| format!("Failed to open Wi-Fi profile: {e}"))?;

        let settings = profile_proxy
            .call_sync(
                "GetSettings",
                None,
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
            )
            .map_err(|e| format!("Failed to read Wi-Fi profile: {e}"))?;
        let wifi = Self::settings_section(&settings, "802-11-wireless")
            .ok_or_else(|| "Active connection is not Wi-Fi".to_string())?;
        let ssid = Self::get_prop_variant(&wifi, "ssid")
            .map(|value| value.iter().filter_map(|byte| byte.get::<u8>()).collect())
            .and_then(|bytes: Vec<u8>| String::from_utf8(bytes).ok())
            .filter(|ssid| !ssid.is_empty())
            .ok_or_else(|| "Wi-Fi profile has no valid SSID".to_string())?;
        let hidden = Self::get_prop_variant(&wifi, "hidden")
            .and_then(|value| value.get::<bool>())
            .unwrap_or(false);

        let Some(security) = Self::settings_section(&settings, "802-11-wireless-security") else {
            return Ok(WifiCredentials {
                ssid,
                password: None,
                hidden,
                authentication: WifiAuthentication::Open,
            });
        };
        let key_mgmt = Self::get_prop_variant(&security, "key-mgmt")
            .and_then(|value| value.get::<String>())
            .unwrap_or_default();
        let authentication = match key_mgmt.as_str() {
            "wpa-psk" => WifiAuthentication::Wpa,
            "sae" => WifiAuthentication::Sae,
            _ => return Err(format!("Unsupported Wi-Fi security: {key_mgmt}")),
        };

        let secrets = profile_proxy
            .call_sync(
                "GetSecrets",
                Some(&("802-11-wireless-security",).to_variant()),
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
            )
            .map_err(|e| format!("Saved Wi-Fi password is unavailable: {e}"))?;
        let password = Self::settings_section(&secrets, "802-11-wireless-security")
            .and_then(|section| Self::get_prop_variant(&section, "psk"))
            .and_then(|value| value.get::<String>())
            .filter(|password| !password.is_empty())
            .ok_or_else(|| "Saved Wi-Fi password is unavailable".to_string())?;

        Ok(WifiCredentials {
            ssid,
            password: Some(password),
            hidden,
            authentication,
        })
    }

    fn settings_section(settings: &glib::Variant, name: &str) -> Option<glib::Variant> {
        Self::get_variant_map_entry(&settings.child_value(0), name)
    }

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
                    // Also fetch the AP list; update_state() alone won't for disconnected users.
                    this.refresh_networks_async();
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

    /// List all UUIDs of every saved Wi-Fi connection profile.
    fn saved_wifi_uuids() -> Result<HashSet<String>, String> {
        let output = Command::new("nmcli")
            .args(["-t", "-f", "UUID,TYPE", "connection", "show"])
            .output()
            .map_err(|e| format!("Failed to fetch known Wi-Fi connections: {e}"))?;

        if !output.status.success() {
            return Err(format!(
                "nmcli exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        let stdout = String::from_utf8(output.stdout)
            .map_err(|e| format!("nmcli output was not valid UTF-8: {e}"))?;

        Ok(stdout
            .lines()
            .filter_map(|line| {
                let (uuid, ctype) = line.split_once(':')?;
                (ctype.contains("wifi") || ctype.contains("wireless")).then(|| uuid.to_string())
            })
            .collect())
    }

    /// Get the SSID of a connection by UUID.
    fn profile_ssid(uuid: &str) -> Result<String, String> {
        let output = Command::new("nmcli")
            .args([
                "-g",
                "802-11-wireless.ssid",
                "--escape",
                "no",
                "connection",
                "show",
                "uuid",
                uuid,
            ])
            .output()
            .map_err(|e| format!("Failed to fetch profile SSID: {e}"))?;

        if !output.status.success() {
            return Err(format!(
                "nmcli exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        if output.stdout.is_empty() {
            return Err(format!("no ssid found for connection {}", uuid));
        }

        String::from_utf8(output.stdout)
            .map(|stdout| stdout.trim().to_string())
            .map_err(|e| format!("nmcli output was not valid UTF-8: {e}"))
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
            let saved_before = Self::saved_wifi_uuids();

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
                        let saved_after = Self::saved_wifi_uuids();
                        match (&saved_before, &saved_after) {
                            (Ok(before), Ok(after)) => {
                                for uuid in after.difference(before) {
                                    match Self::profile_ssid(uuid) {
                                        Ok(profile_ssid) => {
                                            if profile_ssid == ssid {
                                                debug!("removing profile {} for '{}'", uuid, ssid);
                                                let _ = Command::new("nmcli")
                                                    .args(["connection", "delete", "uuid", uuid])
                                                    .output();
                                            }
                                        }
                                        Err(e) => {
                                            warn!("skipping cleanup of profile {}: {}", uuid, e);
                                        }
                                    }
                                }
                            }
                            (Err(e), _) | (_, Err(e)) => {
                                warn!("skipping profile cleanup for '{}': {}", ssid, e);
                            }
                        }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_networkmanager_settings_variants() {
        let wifi = HashMap::from([
            ("ssid".to_string(), b"test-network".to_vec().to_variant()),
            ("hidden".to_string(), true.to_variant()),
        ]);
        let settings = (HashMap::from([("802-11-wireless".to_string(), wifi)]),).to_variant();

        let section = NmService::settings_section(&settings, "802-11-wireless").unwrap();
        let ssid: Vec<u8> = NmService::get_prop_variant(&section, "ssid")
            .unwrap()
            .iter()
            .filter_map(|byte| byte.get::<u8>())
            .collect();

        assert_eq!(ssid, b"test-network");
        assert_eq!(
            NmService::get_prop_variant(&section, "hidden").and_then(|value| value.get::<bool>()),
            Some(true)
        );
        assert!(NmService::settings_section(&settings, "missing").is_none());
    }
}
