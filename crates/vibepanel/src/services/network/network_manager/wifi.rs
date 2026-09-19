//! Wi-Fi device proxy, state management, network scanning, and connection control.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

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

                // Device state only feeds reconciliation; the AP list is owned by the
                // Device.Wireless proxy, whose handler refreshes it.
                this.notify_snapshot(|_| {});

                let this_weak = Rc::downgrade(&this);
                proxy.connect_local("g-properties-changed", false, move |_| {
                    if let Some(this) = this_weak.upgrade() {
                        this.notify_snapshot(|_| {});
                    }
                    None
                });
            },
        );
    }

    pub(super) fn update_state(&self) {
        // Reconcile immediately; list I/O must not delay clearing a disconnected row.
        self.notify_snapshot(|_| {});
        self.refresh_networks_async();
    }

    pub(super) fn reconcile_wifi_state(&self, state: &mut super::WifiState) {
        let device_state = self
            .wifi
            .device_proxy
            .borrow()
            .as_ref()
            .and_then(|proxy| proxy.cached_property("State"))
            .and_then(|value| value.get::<u32>());
        let active_ap = self
            .wifi
            .proxy
            .borrow()
            .as_ref()
            .and_then(|proxy| proxy.cached_property("ActiveAccessPoint"))
            .and_then(|value| objpath_to_string(&value));
        state.reconcile_connection(
            device_state,
            active_ap.as_deref(),
            &self.wifi.networks.borrow(),
        );
        // Mark active APs before deduplication so a weaker connected AP keeps its row.
        state.networks =
            Self::sort_networks(Self::dedupe_networks(std::mem::take(&mut state.networks)));
    }

    // Network List Refresh

    pub(super) fn refresh_networks_async(&self) {
        let generation = self.wifi.refresh_generation.get() + 1;
        self.wifi.refresh_generation.set(generation);
        let Some(wifi) = self.wifi.proxy.borrow().clone() else {
            return;
        };

        let Some(owner) = wifi.name_owner() else {
            return;
        };
        let active_ap = wifi
            .cached_property("ActiveAccessPoint")
            .and_then(|value| objpath_to_string(&value))
            .filter(|path| path != "/");

        let known_ssids = Arc::clone(&self.wifi.known_ssids);
        let known_ssids_refresh = Arc::clone(&self.wifi.known_ssids_last_refresh);

        thread::spawn(move || {
            let Ok(wifi) = system_dbus_proxy_sync(&owner, &wifi.object_path(), IFACE_WIFI) else {
                return;
            };
            // Get LastScan timestamp
            let last_scan = wifi
                .cached_property("LastScan")
                .and_then(|v| v.get::<i64>());

            // Get access point paths
            let mut ap_paths = match Self::get_access_points_sync(&wifi) {
                Ok(paths) => paths,
                Err(e) => {
                    error!("Failed to get access points: {}", e);
                    return;
                }
            };

            // A roam target may not be in GetAccessPoints yet. Read that exact identity;
            // a later device/AP change invalidates this refresh's generation.
            if let Some(active_ap) = active_ap
                && !ap_paths.contains(&active_ap)
            {
                ap_paths.push(active_ap);
            }

            // Refresh known SSIDs cache if needed
            Self::refresh_known_ssids_if_needed(&known_ssids, &known_ssids_refresh);

            // Fetch details for each AP
            let mut networks: Vec<WifiNetwork> = Vec::new();
            let known = known_ssids
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();

            for path in ap_paths {
                if let Ok(net) = Self::get_network_details_sync(&owner, &path, &known) {
                    networks.push(net);
                }
            }

            // Send update to main thread.
            send_nm_update(NmUpdate::NetworksRefreshed {
                generation,
                networks,
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
        owner: &str,
        path: &str,
        known_ssids: &HashSet<String>,
    ) -> Result<WifiNetwork, String> {
        let proxy = system_dbus_proxy_sync(owner, path, IFACE_AP)
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
        let is_known = known_ssids.contains(&ssid_str);

        Ok(WifiNetwork {
            ssid: ssid_str,
            strength,
            security,
            active: false,
            known_network_path: None,
            known: is_known,
            path: Some(path.to_string()),
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

        let mut ssids = HashSet::new();
        match Self::wifi_profiles() {
            Ok(profiles) => {
                for (_, ssid) in profiles {
                    ssids.insert(ssid);
                }
            }
            Err(e) => {
                warn!("Failed to read saved Wi-Fi profiles: {e}");
                return;
            }
        }

        *known_ssids.lock().unwrap_or_else(|e| e.into_inner()) = ssids;
        *last_refresh.lock().unwrap_or_else(|e| e.into_inner()) = Some(now);
    }

    /// Return nonvolatile Wi-Fi profiles pinned to one daemon, with actual SSIDs.
    fn wifi_profiles() -> Result<Vec<(gio::DBusProxy, String)>, String> {
        let bus = gio::bus_get_sync(gio::BusType::System, None::<&gio::Cancellable>)
            .map_err(|e| e.to_string())?;
        Self::wifi_profiles_on_bus(&bus)
    }

    fn wifi_profiles_on_bus(
        bus: &gio::DBusConnection,
    ) -> Result<Vec<(gio::DBusProxy, String)>, String> {
        let owner = bus
            .call_sync(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "GetNameOwner",
                Some(&(NM_SERVICE,).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
            )
            .map_err(|e| e.to_string())?
            .get::<(String,)>()
            .ok_or("Invalid NetworkManager owner")?
            .0;
        let proxy_for = |path: &str, iface: &str| {
            gio::DBusProxy::new_sync(
                bus,
                gio::DBusProxyFlags::NONE,
                None::<&gio::DBusInterfaceInfo>,
                Some(&owner),
                path,
                iface,
                None::<&gio::Cancellable>,
            )
        };
        // Private profiles and profiles removed during enumeration do not invalidate others.
        let unavailable_profile = |error: &glib::Error| {
            matches!(
                gio::DBusError::remote_error(error).as_deref(),
                Some(
                    "org.freedesktop.NetworkManager.Settings.PermissionDenied"
                        | "org.freedesktop.DBus.Error.AccessDenied"
                        | "org.freedesktop.DBus.Error.UnknownObject"
                        | "org.freedesktop.DBus.Error.UnknownInterface"
                        | "org.freedesktop.DBus.Error.UnknownMethod"
                )
            )
        };
        let proxy =
            proxy_for(super::NM_SETTINGS_PATH, super::IFACE_SETTINGS).map_err(|e| e.to_string())?;
        let paths = proxy
            .call_sync(
                "ListConnections",
                None,
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
            )
            .map_err(|e| e.to_string())?;
        let mut profiles = Vec::new();
        for path in paths
            .child_value(0)
            .iter()
            .filter_map(|v| objpath_to_string(&v))
        {
            let profile = match proxy_for(&path, super::IFACE_SETTINGS_CONN) {
                Ok(profile) => profile,
                Err(e) if unavailable_profile(&e) => continue,
                Err(e) => return Err(e.to_string()),
            };
            // NM_SETTINGS_CONNECTION_FLAG_VOLATILE (0x04) excludes temporary activation attempts.
            if profile
                .cached_property("Flags")
                .and_then(|v| v.get::<u32>())
                .is_none_or(|flags| flags & 0x04 != 0)
            {
                continue;
            }
            let settings = match profile.call_sync(
                "GetSettings",
                None,
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
            ) {
                Ok(settings) => settings,
                Err(e) if unavailable_profile(&e) => continue,
                Err(e) => return Err(e.to_string()),
            };
            if let Some(wifi) = Self::settings_section(&settings, "802-11-wireless")
                && let Some(ssid) =
                    Self::get_prop_variant(&wifi, "ssid").and_then(|v| v.get::<Vec<u8>>())
                && let Ok(ssid) = String::from_utf8(ssid)
            {
                profiles.push((profile, ssid));
            }
        }
        Ok(profiles)
    }

    fn dedupe_networks(networks: Vec<WifiNetwork>) -> Vec<WifiNetwork> {
        let mut merged: HashMap<(String, SecurityType), WifiNetwork> = HashMap::new();

        for net in networks {
            let key = (net.ssid.clone(), net.security);
            if let Some(existing) = merged.get_mut(&key) {
                if (net.active && !existing.active)
                    || (net.active == existing.active && net.strength > existing.strength)
                {
                    existing.path = net.path.clone();
                    existing.strength = net.strength;
                }
                existing.active = existing.active || net.active;
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
        if !enabled {
            self.cancel_wifi_attempt();
        }
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
    pub fn connect_to_network(
        self: &Rc<Self>,
        ssid: &str,
        password: Option<&str>,
        ap: Option<&str>,
    ) {
        let ssid = ssid.to_string();
        if ssid.is_empty() {
            return;
        }
        let Some(device) = self.wifi.device_proxy.borrow().clone() else {
            return;
        };
        let Some(owner) = device.name_owner() else {
            return;
        };
        let device_path = device.object_path().to_string();
        let bus = device.connection();
        // Replace the previous attempt in one snapshot; no transient `connecting = None`.
        self.abort_wifi_attempt();
        let attempt = self.wifi.attempt.get();
        let cancel = gio::Cancellable::new();
        self.wifi.activation_cancel.replace(Some(cancel.clone()));
        let previous = self.wifi.activation_task.take();

        // Clear any previous failed state and set connecting state for UI feedback.
        *self.wifi.failed_ssid.borrow_mut() = None;
        *self.wifi.connecting_ssid.borrow_mut() = Some(ssid.clone());
        self.notify_snapshot(|s| {
            s.wifi.failed_ssid = None;
            s.wifi.connecting_ssid = Some(ssid.clone());
        });

        let password = password.map(|s| s.to_string());
        let ap = ap.map(str::to_string).or_else(|| {
            self.snapshot()
                .wifi
                .networks
                .iter()
                .find(|network| network.ssid == ssid)
                .and_then(|network| network.path.clone())
        });

        let this = self.clone();
        let task = glib::spawn_future_local(async move {
            // Do not abort an in-flight activation call: its reply identifies what to deactivate.
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            if cancel.is_cancelled() {
                return;
            }
            // Pin calls to this daemon instance: object paths can be reused after NM restarts.
            let call = |path: &str, iface: &str, method: &str, args: Option<&Variant>| {
                bus.call_future(
                    Some(&owner),
                    path,
                    iface,
                    method,
                    args,
                    None,
                    gio::DBusCallFlags::NONE,
                    30_000,
                )
            };
            let result = Self::activate_wifi(
                call,
                |active| {
                    wait_activation(
                        bus.clone(),
                        owner.to_string(),
                        active.to_string(),
                        cancel.clone(),
                        Duration::from_secs(90),
                    )
                },
                &device_path,
                ap.as_deref(),
                &ssid,
                password.as_deref(),
                &cancel,
            )
            .await;
            let (success, message) = match result {
                Ok(save_error) => (true, save_error),
                Err(e) => {
                    warn!("Wi-Fi activation failed for '{ssid}': {e}");
                    (false, None)
                }
            };
            this.apply_update(NmUpdate::ConnectionAttemptFinished {
                attempt,
                ssid,
                success,
                message,
            });
        });
        self.wifi.activation_task.replace(Some(task));
    }

    /// Invalidate the in-flight attempt without touching the snapshot.
    fn abort_wifi_attempt(&self) {
        self.wifi.attempt.set(self.wifi.attempt.get() + 1);
        if let Some(cancel) = self.wifi.activation_cancel.take() {
            cancel.cancel();
        }
    }

    pub(super) fn cancel_wifi_attempt(&self) {
        self.abort_wifi_attempt();
        *self.wifi.connecting_ssid.borrow_mut() = None;
        self.notify_snapshot(|s| s.wifi.connecting_ssid = None);
    }

    async fn activate_wifi<F, W>(
        raw_call: impl Fn(&str, &str, &str, Option<&Variant>) -> F,
        wait: impl FnOnce(&str) -> W,
        device: &str,
        selected_ap: Option<&str>,
        ssid: &str,
        password: Option<&str>,
        cancel: &gio::Cancellable,
    ) -> Result<Option<String>, String>
    where
        F: std::future::Future<Output = Result<Variant, glib::Error>>,
        W: std::future::Future<Output = Result<(), String>>,
    {
        let call = async |path: &str, iface: &str, method: &str, args: Option<&Variant>| {
            raw_call(path, iface, method, args)
                .await
                .map_err(|e| format!("{method}: {e}"))
        };
        let properties =
            async |path: &str, iface: &str| -> Result<HashMap<String, Variant>, String> {
                call(
                    path,
                    super::PROPERTIES_IFACE,
                    "GetAll",
                    Some(&(iface,).to_variant()),
                )
                .await?
                .child_value(0)
                .get()
                .ok_or_else(|| "Invalid properties reply".to_string())
            };
        let ap = selected_ap.ok_or("Wi-Fi network is no longer visible")?;
        let props = properties(ap, IFACE_AP).await?;
        if props
            .get("Ssid")
            .and_then(|v| v.get::<Vec<u8>>())
            .as_deref()
            != Some(ssid.as_bytes())
        {
            return Err("Wi-Fi access point no longer matches the selected network".into());
        }
        let flags = props.get("Flags").and_then(|v| v.get::<u32>()).unwrap_or(0);
        let security = props
            .get("WpaFlags")
            .and_then(|v| v.get::<u32>())
            .unwrap_or(0)
            | props
                .get("RsnFlags")
                .and_then(|v| v.get::<u32>())
                .unwrap_or(0);
        if cancel.is_cancelled() {
            return Err("Connection cancelled".into());
        }
        let device = glib::variant::ObjectPath::try_from(device).map_err(|e| e.to_string())?;
        let ap = glib::variant::ObjectPath::try_from(ap).map_err(|e| e.to_string())?;
        let automatic = glib::variant::ObjectPath::try_from("/").expect("valid root path");
        // NM selects a saved profile compatible with this AP, including band and security restrictions.
        let (profile, active) = match raw_call(
            super::NM_PATH,
            NM_IFACE,
            "ActivateConnection",
            Some(&(automatic, device.clone(), ap.clone()).to_variant()),
        )
        .await
        {
            Ok(reply) => (
                None,
                reply
                    .child_value(0)
                    .get::<glib::variant::ObjectPath>()
                    .ok_or("Invalid activation path")?,
            ),
            Err(e)
                if gio::DBusError::remote_error(&e).as_deref()
                    == Some("org.freedesktop.NetworkManager.UnknownConnection") =>
            {
                if cancel.is_cancelled() {
                    return Err("Connection cancelled".into());
                }
                let settings = new_wifi_settings(ssid, password, flags, security)?;
                let options = HashMap::from([("persist", "volatile".to_variant())]);
                let reply = call(
                    super::NM_PATH,
                    NM_IFACE,
                    "AddAndActivateConnection2",
                    Some(&(settings, device, ap, options).to_variant()),
                )
                .await?;
                let profile = reply
                    .child_value(0)
                    .get::<glib::variant::ObjectPath>()
                    .ok_or("Invalid profile path")?;
                let active = reply
                    .child_value(1)
                    .get::<glib::variant::ObjectPath>()
                    .ok_or("Invalid activation path")?;
                (Some(profile), active)
            }
            Err(e) => return Err(format!("ActivateConnection: {e}")),
        };

        // Saved credentials take precedence; the supplied password is only for a new profile.
        let outcome = if cancel.is_cancelled() {
            Err("Connection cancelled".into())
        } else {
            wait(active.as_str()).await
        };
        if outcome.is_err() || cancel.is_cancelled() {
            // Deactivate only this attempt, never whichever connection is now on the device.
            if let Err(e) = call(
                super::NM_PATH,
                NM_IFACE,
                "DeactivateConnection",
                Some(&(active,).to_variant()),
            )
            .await
            {
                debug!("Wi-Fi activation cleanup: {e}");
            }
            if let Some(profile) = profile
                && let Err(e) =
                    call(profile.as_str(), super::IFACE_SETTINGS_CONN, "Delete", None).await
            {
                // NM may already have removed the volatile profile after deactivation.
                debug!("Wi-Fi profile cleanup: {e}");
            }
            return Err(outcome
                .err()
                .unwrap_or_else(|| "Connection cancelled".into()));
        }
        if let Some(profile) = profile {
            let settings: HashMap<String, HashMap<String, Variant>> = HashMap::new();
            let args: HashMap<String, Variant> = HashMap::new();
            // TO_DISK promotes the volatile profile without replacing settings or secrets.
            if let Err(e) = call(
                profile.as_str(),
                super::IFACE_SETTINGS_CONN,
                "Update2",
                Some(&(settings, 1u32, args).to_variant()),
            )
            .await
            {
                warn!("Connected to '{ssid}', but saving profile failed: {e}");
                return Ok(Some(e));
            }
        }
        Ok(None)
    }

    /// Disconnect from the current Wi-Fi network.
    pub fn disconnect(self: &Rc<Self>) {
        self.cancel_wifi_attempt();
        let Some(device) = self.wifi.device_proxy.borrow().clone() else {
            return;
        };
        let Some(owner) = device.name_owner() else {
            return;
        };
        let attempt = self.wifi.attempt.get();
        let previous = self.wifi.activation_task.take();
        let this = self.clone();

        let task = glib::spawn_future_local(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            if this.wifi.attempt.get() != attempt {
                return;
            }
            if let Err(e) = device
                .connection()
                .call_future(
                    Some(&owner),
                    &device.object_path(),
                    IFACE_DEV,
                    "Disconnect",
                    None,
                    None,
                    gio::DBusCallFlags::NONE,
                    5000,
                )
                .await
            {
                error!("Wi-Fi disconnect failed: {e}");
            }

            // Request refresh.
            if this.wifi.attempt.get() == attempt {
                this.update_state();
            }
        });
        self.wifi.activation_task.replace(Some(task));
    }

    /// Delete all nonvolatile Wi-Fi profiles whose actual SSID matches, not just one row/profile.
    pub fn forget_network(&self, ssid: &str) {
        let ssid = ssid.to_string();
        if ssid.is_empty() {
            return;
        }

        let known_ssids_refresh = Arc::clone(&self.wifi.known_ssids_last_refresh);

        thread::spawn(move || {
            let result = Self::wifi_profiles()
                .and_then(|profiles| Self::delete_wifi_profiles(profiles, &ssid));
            if let Err(e) = result {
                error!("Wi-Fi forget failed: {e}");
            }

            // Invalidate known SSIDs cache
            *known_ssids_refresh
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;

            // Request refresh.
            send_nm_update(NmUpdate::RefreshNetworks);
        });
    }

    fn delete_wifi_profiles(
        profiles: Vec<(gio::DBusProxy, String)>,
        ssid: &str,
    ) -> Result<(), String> {
        let mut result = Ok(());
        for (proxy, profile_ssid) in profiles {
            if profile_ssid != ssid {
                continue;
            }
            if let Err(e) = proxy.call_sync(
                "Delete",
                None,
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
            ) {
                result = Err(e.to_string());
            }
        }
        result
    }
}

pub(super) async fn wait_activation(
    bus: gio::DBusConnection,
    owner: String,
    active: String,
    cancel: gio::Cancellable,
    timeout: Duration,
) -> Result<(), String> {
    let (sender, receiver) = async_channel::unbounded::<Result<u32, String>>();
    let saw_signal = Rc::new(std::cell::Cell::new(false));
    // Subscribe before reading: an activation can finish before its method reply arrives.
    let _state = bus.subscribe_to_signal(
        Some(&owner),
        Some(super::IFACE_ACTIVE_CONN),
        Some("StateChanged"),
        Some(&active),
        None,
        gio::DBusSignalFlags::NONE,
        {
            let sender = sender.clone();
            let saw_signal = saw_signal.clone();
            move |signal| {
                saw_signal.set(true);
                let state = signal
                    .parameters
                    .get::<(u32, u32)>()
                    .map(|(state, _reason)| state)
                    .ok_or_else(|| "Invalid activation state signal".into());
                let _ = sender.try_send(state);
            }
        },
    );
    let _removed = bus.subscribe_to_signal(
        Some(&owner),
        Some(super::PROPERTIES_IFACE),
        Some("PropertiesChanged"),
        Some(super::NM_PATH),
        Some(NM_IFACE),
        gio::DBusSignalFlags::NONE,
        {
            let sender = sender.clone();
            let active = active.clone();
            move |signal| {
                if let Some((_, properties, _)) =
                    signal
                        .parameters
                        .get::<(String, HashMap<String, Variant>, Vec<String>)>()
                    && let Some(paths) = properties
                        .get("ActiveConnections")
                        .and_then(|v| v.get::<Vec<glib::variant::ObjectPath>>())
                    && !paths.iter().any(|path| path.as_str() == active)
                {
                    let _ = sender.try_send(Err("Connection activation disappeared".into()));
                }
            }
        },
    );
    let _owner = bus.subscribe_to_signal(
        Some("org.freedesktop.DBus"),
        Some("org.freedesktop.DBus"),
        Some("NameOwnerChanged"),
        Some("/org/freedesktop/DBus"),
        Some(&owner),
        gio::DBusSignalFlags::NONE,
        {
            let sender = sender.clone();
            move |signal| {
                if let Some((_, _, new_owner)) = signal.parameters.get::<(String, String, String)>()
                    && new_owner.is_empty()
                {
                    let _ = sender.try_send(Err("NetworkManager disappeared".into()));
                }
            }
        },
    );
    let closed = bus.connect_closed({
        let sender = sender.clone();
        move |_, _, _| {
            let _ = sender.try_send(Err("System bus disconnected".into()));
        }
    });
    let cancelled = cancel.connect_cancelled({
        let sender = sender.clone();
        move |_| {
            let _ = sender.try_send(Err("Connection cancelled".into()));
        }
    });
    let read_cancel = gio::Cancellable::new();
    bus.call(
        Some(&owner),
        &active,
        super::PROPERTIES_IFACE,
        "Get",
        Some(&(super::IFACE_ACTIVE_CONN, "State").to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        5000,
        Some(&read_cancel),
        move |reply| {
            // A queued initial reply must not overwrite a newer StateChanged signal.
            if !saw_signal.get() {
                let state = reply.map_err(|e| e.to_string()).and_then(|reply| {
                    reply
                        .get::<(Variant,)>()
                        .and_then(|(state,)| state.get::<u32>())
                        .ok_or_else(|| "Invalid activation state reply".into())
                });
                let _ = sender.try_send(state);
            }
        },
    );
    let result = glib::future_with_timeout(timeout, async {
        loop {
            let mut event = receiver
                .recv()
                .await
                .map_err(|_| "Activation watcher closed".to_string())?;
            // Prefer the latest queued state if activation and deactivation arrived together,
            // but never let a state overwrite a queued terminal error (cancel, removal, owner loss).
            while event.is_ok()
                && let Ok(newer) = receiver.try_recv()
            {
                event = newer;
            }
            match event? {
                1 => continue,
                2 => return Ok(()),
                3 | 4 => return Err("Connection activation ended before connecting".into()),
                _ => return Err("Invalid connection activation state".into()),
            }
        }
    })
    .await
    .unwrap_or_else(|_| Err("Connection activation timed out".into()));
    read_cancel.cancel();
    if let Some(id) = cancelled {
        cancel.disconnect_cancelled(id);
    }
    bus.disconnect(closed);
    result
}

fn new_wifi_settings(
    ssid: &str,
    password: Option<&str>,
    flags: u32,
    security: u32,
) -> Result<HashMap<String, HashMap<String, Variant>>, String> {
    let mut settings = HashMap::from([(
        "802-11-wireless".into(),
        HashMap::from([("ssid".into(), ssid.as_bytes().to_variant())]),
    )]);
    let wep = flags & 1 != 0 && security == 0;
    // Only route credentials here. NM completes key management, mode, and other settings.
    if wep || security & (0x100 | 0x400) != 0 {
        let password = password
            .filter(|p| !p.is_empty())
            .ok_or("Wi-Fi password is required")?;
        let mut secrets = HashMap::new();
        if wep {
            let raw = matches!(password.len(), 10 | 26)
                && password.bytes().all(|b| b.is_ascii_hexdigit());
            let ascii = matches!(password.len(), 5 | 13) && password.is_ascii();
            secrets.insert("wep-key0".into(), password.to_variant());
            secrets.insert(
                "wep-key-type".into(),
                (if raw || ascii { 1u32 } else { 2u32 }).to_variant(),
            );
            secrets.insert("wep-key-flags".into(), 0u32.to_variant());
        } else {
            secrets.insert("psk".into(), password.to_variant());
            secrets.insert("psk-flags".into(), 0u32.to_variant());
        }
        settings.insert("802-11-wireless-security".into(), secrets);
    }
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_network(path: &str) -> WifiNetwork {
        WifiNetwork {
            ssid: "test".into(),
            strength: 70,
            security: SecurityType::Secured,
            active: true,
            known: true,
            known_network_path: None,
            path: Some(path.into()),
        }
    }

    #[test]
    fn header_and_rows_follow_confirmed_device_state() {
        let mut state = super::super::WifiState {
            enabled: Some(true),
            ..Default::default()
        };
        let mut networks = vec![test_network("/ap")];
        state.reconcile_connection(Some(100), Some("/ap"), &networks);
        assert!(state.connected && state.networks[0].active);
        assert_eq!(state.ssid.as_deref(), Some("test"));

        // Forget/disconnect can leave ActiveAccessPoint set during deactivation.
        // Authentication can also select an AP without ever reaching ACTIVATED.
        for device_state in [110, 30, 40, 60, 120] {
            state.reconcile_connection(Some(device_state), Some("/ap"), &networks);
            assert!(!state.connected && !state.networks[0].active);
            assert!(state.ssid.is_none());
            assert_eq!(state.strength, 0);
            // A delayed list response must not restore its old active flags.
            networks = vec![test_network("/ap")];
            state.reconcile_connection(Some(device_state), Some("/ap"), &networks);
            assert!(!state.connected && !state.networks[0].active);
        }
        // A different AP (even for the same SSID) must not inherit the active flag.
        networks.push(test_network("/other_ap"));
        state.reconcile_connection(Some(100), Some("/other_ap"), &networks);
        assert!(state.connected && !state.networks[0].active && state.networks[1].active);
        state.reconcile_connection(Some(100), Some("/"), &networks);
        assert!(!state.connected && state.networks.iter().all(|n| !n.active));
        state.enabled = Some(false);
        state.reconcile_connection(Some(100), Some("/ap"), &networks);
        assert!(!state.connected && state.networks.iter().all(|n| !n.active));
    }

    #[test]
    fn deduplicated_rows_do_not_lose_roam_identity() {
        let mut state = super::super::WifiState {
            enabled: Some(true),
            ..Default::default()
        };
        let mut networks = vec![test_network("/strong"), test_network("/weak")];
        networks[1].strength = 30;
        state.reconcile_connection(Some(100), Some("/strong"), &networks);
        state.networks = NmService::dedupe_networks(state.networks);
        assert_eq!(state.networks.len(), 1);
        assert_eq!(state.networks[0].path.as_deref(), Some("/strong"));

        state.reconcile_connection(Some(100), Some("/weak"), &networks);
        state.networks = NmService::dedupe_networks(state.networks);
        assert!(state.connected && state.networks[0].active);
        assert_eq!(state.ssid.as_deref(), Some("test"));
        assert_eq!(state.strength, 30);
        assert_eq!(state.networks[0].path.as_deref(), Some("/weak"));

        // Unknown identity must clear the old label until its own details arrive.
        state.reconcile_connection(Some(100), Some("/unknown"), &networks);
        assert!(state.connected && state.ssid.is_none());
        assert_eq!(state.strength, 0);
        assert!(state.networks.iter().all(|n| !n.active));
        let mut refreshed = test_network("/unknown");
        refreshed.ssid = "other network".into();
        networks.push(refreshed);
        state.reconcile_connection(Some(100), Some("/unknown"), &networks);
        assert_eq!(state.ssid.as_deref(), Some("other network"));
        // A delayed identity result cannot relabel a subsequent roam or disconnect.
        state.reconcile_connection(Some(100), Some("/weak"), &networks);
        assert_eq!(state.ssid.as_deref(), Some("test"));
        state.reconcile_connection(Some(120), Some("/unknown"), &networks);
        assert!(!state.connected && state.ssid.is_none());
    }

    #[test]
    fn obsolete_list_results_cannot_overwrite_newer_snapshot() {
        use super::super::{MobileInternal, NmSnapshot, WifiInternal};
        use std::cell::RefCell;
        let service = NmService {
            nm_proxy: RefCell::new(None),
            snapshot: RefCell::new(NmSnapshot::unknown()),
            callbacks: crate::services::callbacks::Callbacks::new(),
            wifi: WifiInternal::new(),
            mobile: MobileInternal::new(),
        };
        service.wifi.refresh_generation.set(2);
        service.apply_update(NmUpdate::NetworksRefreshed {
            generation: 2,
            networks: vec![
                test_network("/new"),
                WifiNetwork {
                    strength: 30,
                    ..test_network("/roam")
                },
            ],
            last_scan: Some(2),
        });
        service.apply_update(NmUpdate::NetworksRefreshed {
            generation: 1,
            networks: vec![test_network("/old")],
            last_scan: Some(1),
        });
        let snapshot = service.snapshot();
        assert_eq!(snapshot.wifi.networks.len(), 1);
        assert_eq!(service.wifi.networks.borrow().len(), 2);
        assert_eq!(
            service.wifi.networks.borrow()[1].path.as_deref(),
            Some("/roam")
        );
        assert_eq!(snapshot.wifi.networks[0].path.as_deref(), Some("/new"));
        assert!(!snapshot.wifi.connected && !snapshot.wifi.networks[0].active);
        assert_eq!(service.wifi.last_scan_value.get(), Some(2));
        service.wifi.attempt.set(2);
        service
            .wifi
            .connecting_ssid
            .replace(Some("new attempt".into()));
        service.notify_snapshot(|s| s.wifi.connecting_ssid = Some("new attempt".into()));
        service.apply_update(NmUpdate::ConnectionAttemptFinished {
            attempt: 1,
            ssid: "old attempt".into(),
            success: true,
            message: None,
        });
        assert_eq!(
            service.snapshot().wifi.connecting_ssid.as_deref(),
            Some("new attempt")
        );
        service.apply_update(NmUpdate::ConnectionAttemptFinished {
            attempt: 2,
            ssid: "new attempt".into(),
            success: false,
            message: None,
        });
        let snapshot = service.snapshot();
        assert!(snapshot.wifi.connecting_ssid.is_none());
        assert_eq!(snapshot.wifi.failed_ssid.as_deref(), Some("new attempt"));
        assert!(!snapshot.wifi.connected && snapshot.wifi.networks.iter().all(|n| !n.active));
    }

    #[test]
    fn activation_signals_on_private_bus() {
        // Isolate D-Bus and GLib globals from the user's session and other unit tests.
        let output = std::process::Command::new("dbus-run-session")
            .arg("--")
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "services::network::network_manager::wifi::tests::activation_signal_runner",
                "--ignored",
                "--nocapture",
            ])
            .env("VIBEPANEL_TEST_BUS", "1")
            .output()
            .expect("dbus-run-session is required for D-Bus integration tests");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[ignore = "internal runner; launched on a private bus by activation_signals_on_private_bus"]
    fn activation_signal_runner() {
        assert_eq!(std::env::var("VIBEPANEL_TEST_BUS").as_deref(), Ok("1"));
        let context = glib::MainContext::new();
        context.with_thread_default(|| context.block_on(async {
            let address = std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap();
            let flags = gio::DBusConnectionFlags::AUTHENTICATION_CLIENT | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION;
            let client = gio::DBusConnection::for_address_future(&address, flags, None).await.unwrap();
            let info = gio::DBusNodeInfo::for_xml(r#"<node><interface name="org.freedesktop.NetworkManager.Connection.Active">
                <property name="State" type="u" access="read"/>
                <signal name="StateChanged"><arg type="u"/><arg type="u"/></signal>
                </interface></node>"#).unwrap();
            for case in ["already_active", "signal", "fast_signal", "failure_before_read", "failure", "cancel", "cancel_then_state", "timeout", "removed", "owner_lost"] {
                let server = gio::DBusConnection::for_address_future(&address, flags, None).await.unwrap();
                let cancel = gio::Cancellable::new();
                let reads = Rc::new(std::cell::Cell::new(0));
                let registration = server.register_object("/active", &info.interfaces()[0]).property({
                    let cancel = cancel.clone();
                    let reads = reads.clone();
                    move |server, _, _, _, _| {
                        reads.set(reads.get() + 1);
                        if case == "already_active" { return 2u32.to_variant(); }
                        if case == "cancel_then_state" {
                            cancel.cancel();
                            server.emit_signal(None, "/active", super::super::IFACE_ACTIVE_CONN,
                                "StateChanged", Some(&(2u32, 0u32).to_variant())).unwrap();
                            return 2u32.to_variant();
                        }
                        if matches!(case, "fast_signal" | "failure_before_read") {
                            server.emit_signal(None, "/active", super::super::IFACE_ACTIVE_CONN,
                                "StateChanged", Some(&(if case == "failure_before_read" { 4u32 } else { 2u32 }, 0u32).to_variant())).unwrap();
                            // Deliberately stale initial read, delivered after the state signal.
                            return (if case == "failure_before_read" { 2u32 } else { 1u32 }).to_variant();
                        }
                        if case != "timeout" {
                            let cancel = cancel.clone();
                            glib::spawn_future_local(async move {
                                glib::timeout_future(Duration::from_millis(5)).await;
                                match case {
                                    "cancel" => cancel.cancel(),
                                    "owner_lost" => { server.close_future().await.unwrap(); }
                                    "removed" => server.emit_signal(None, super::super::NM_PATH,
                                        super::super::PROPERTIES_IFACE, "PropertiesChanged",
                                        Some(&(NM_IFACE, HashMap::from([("ActiveConnections", Vec::<glib::variant::ObjectPath>::new().to_variant())]), Vec::<String>::new()).to_variant())).unwrap(),
                                    _ => server.emit_signal(None, "/active", super::super::IFACE_ACTIVE_CONN,
                                        "StateChanged", Some(&(if case == "failure" { 4u32 } else { 2u32 }, 0u32).to_variant())).unwrap(),
                                }
                            });
                        }
                        1u32.to_variant()
                    }
                }).build().unwrap();
                let mut watcher = std::pin::pin!(wait_activation(client.clone(), server.unique_name().unwrap().to_string(),
                    "/active".into(), cancel, Duration::from_millis(if case == "timeout" { 20 } else { 2000 })));
                if case == "cancel_then_state" {
                    // Install the watcher, then hold it until both cancellation and state are queued.
                    std::future::poll_fn(|cx| {
                        assert!(std::future::Future::poll(watcher.as_mut(), cx).is_pending());
                        std::task::Poll::Ready(())
                    }).await;
                    let (sent, received) = async_channel::bounded(1);
                    let _signal = client.subscribe_to_signal(Some(&server.unique_name().unwrap()),
                        Some(super::super::IFACE_ACTIVE_CONN), Some("StateChanged"), Some("/active"),
                        None, gio::DBusSignalFlags::NONE, move |_| { sent.try_send(()).unwrap(); });
                    glib::future_with_timeout(Duration::from_secs(2), received.recv()).await.unwrap().unwrap();
                    while context.pending() { context.iteration(false); }
                }
                let result = watcher.await;
                if case == "cancel_then_state" {
                    assert_eq!(result, Err("Connection cancelled".into()));
                }
                assert_eq!(result.is_ok(), matches!(case, "already_active" | "signal" | "fast_signal"), "{case}: {result:?}");
                if case == "timeout" {
                    // The deadline may expire before the initial read is even served.
                    assert!(reads.get() <= 1, "{case}: only the initial state read is allowed");
                    assert!(result.unwrap_err().contains("timed out"));
                } else {
                    assert_eq!(reads.get(), 1, "{case}: only the initial state read is allowed");
                }
                server.unregister_object(registration).unwrap();
                if !server.is_closed() { server.close_future().await.unwrap(); }
            }
            profiles_stay_on_original_owner(&address, flags, &client).await;
            client.close_future().await.unwrap();
        })).unwrap();
    }

    /// One enumeration+forget pass against a fake NM whose name is taken over mid-enumeration:
    /// fatal errors propagate, private/removed/volatile profiles are skipped, forget keeps
    /// deleting after an error, and nothing ever reaches the replacement daemon.
    async fn profiles_stay_on_original_owner(
        address: &str,
        flags: gio::DBusConnectionFlags,
        client: &gio::DBusConnection,
    ) {
        use glib::variant::ObjectPath;
        use std::cell::{Cell, RefCell};
        let settings_info = gio::DBusNodeInfo::for_xml(
            r#"<node><interface name="org.freedesktop.NetworkManager.Settings">
            <method name="ListConnections"><arg type="ao" direction="out"/></method>
            </interface></node>"#,
        )
        .unwrap();
        let profile_info = gio::DBusNodeInfo::for_xml(
            r#"<node><interface name="org.freedesktop.NetworkManager.Settings.Connection">
            <property name="Flags" type="u" access="read"/>
            <method name="GetSettings"><arg type="a{sa{sv}}" direction="out"/></method>
            <method name="Delete"/>
            </interface></node>"#,
        )
        .unwrap();
        let original = gio::DBusConnection::for_address_future(address, flags, None)
            .await
            .unwrap();
        let replacement = gio::DBusConnection::for_address_future(address, flags, None)
            .await
            .unwrap();
        original
            .call_future(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "RequestName",
                Some(&(NM_SERVICE, 1u32).to_variant()), // ALLOW_REPLACEMENT
                None,
                gio::DBusCallFlags::NONE,
                5000,
            )
            .await
            .unwrap();
        // The replacement exports no profiles: retargeting fails the listing/deletion assertions.
        let fatal = Rc::new(Cell::new(false));
        let deleted = Rc::new(RefCell::new(Vec::new()));
        let mut registrations = Vec::new();
        for (path, info) in [
            (super::super::NM_SETTINGS_PATH, &settings_info),
            ("/private", &profile_info),
            ("/removed", &profile_info),
            ("/volatile", &profile_info),
            ("/profile", &profile_info),
        ] {
            let replacement = replacement.clone();
            let fatal = fatal.clone();
            let deleted = deleted.clone();
            // NM_SETTINGS_CONNECTION_FLAG_VOLATILE marks an in-flight activation profile.
            let profile_flags = if path == "/volatile" {
                0x05u32
            } else {
                0x01u32
            };
            registrations.push(
                original
                    .register_object(path, &info.interfaces()[0])
                    .property(move |_, _, _, _, _| profile_flags.to_variant())
                    .method_call(move |_, _, _, _, method, _, invocation| {
                        assert_ne!(path, "/volatile", "Volatile profiles must be skipped");
                        let error = match (method, path) {
                            ("GetSettings", _) if fatal.get() => {
                                "org.freedesktop.DBus.Error.NoReply"
                            }
                            ("GetSettings", "/private") => {
                                "org.freedesktop.NetworkManager.Settings.PermissionDenied"
                            }
                            ("Delete", "/removed") => "org.freedesktop.DBus.Error.UnknownObject",
                            _ => "",
                        };
                        if method == "Delete" {
                            deleted.borrow_mut().push(path);
                        }
                        if !error.is_empty() {
                            invocation.return_dbus_error(error, "Profile unavailable");
                            return;
                        }
                        let reply = match method {
                            "ListConnections" => {
                                (["/private", "/removed", "/volatile", "/profile"]
                                    .map(|path| ObjectPath::try_from(path).unwrap())
                                    .to_vec(),)
                                    .to_variant()
                            }
                            "GetSettings" => (HashMap::from([(
                                "802-11-wireless",
                                HashMap::from([("ssid", b"original".to_vec().to_variant())]),
                            )]),)
                                .to_variant(),
                            _ => ().to_variant(),
                        };
                        if method == "ListConnections" && !fatal.get() {
                            // Daemon replaced mid-enumeration: later calls must stay pinned.
                            let replacement = replacement.clone();
                            glib::spawn_future_local(async move {
                                let owner_reply = replacement
                                    .call_future(
                                        Some("org.freedesktop.DBus"),
                                        "/org/freedesktop/DBus",
                                        "org.freedesktop.DBus",
                                        "RequestName",
                                        Some(&(NM_SERVICE, 2u32).to_variant()), // REPLACE_EXISTING
                                        None,
                                        gio::DBusCallFlags::NONE,
                                        5000,
                                    )
                                    .await
                                    .unwrap();
                                assert_eq!(owner_reply.get::<(u32,)>(), Some((1,)));
                                invocation.return_value(Some(&reply));
                            });
                        } else {
                            invocation.return_value(Some(&reply));
                        }
                    })
                    .build()
                    .unwrap(),
            );
        }
        let enumerate = || {
            let bus = client.clone();
            gio::spawn_blocking(move || NmService::wifi_profiles_on_bus(&bus))
        };

        fatal.set(true);
        assert!(enumerate().await.unwrap().unwrap_err().contains("NoReply"));

        fatal.set(false);
        let profiles = enumerate().await.unwrap().unwrap();
        let listed: Vec<_> = profiles
            .iter()
            .map(|(p, ssid)| (p.object_path().to_string(), ssid.as_str()))
            .collect();
        assert_eq!(
            listed,
            [
                ("/removed".to_string(), "original"),
                ("/profile".to_string(), "original")
            ]
        );
        assert!(
            profiles
                .iter()
                .all(|(p, _)| p.name() == original.unique_name())
        );

        let forget =
            gio::spawn_blocking(move || NmService::delete_wifi_profiles(profiles, "original"))
                .await
                .unwrap();
        assert!(forget.unwrap_err().contains("UnknownObject"));
        assert_eq!(*deleted.borrow(), ["/removed", "/profile"]);

        for registration in registrations {
            original.unregister_object(registration).unwrap();
        }
        original.close_future().await.unwrap();
        replacement.close_future().await.unwrap();
    }

    #[test]
    fn wifi_profile_lifecycle() {
        use glib::variant::ObjectPath;
        use std::cell::{Cell, RefCell};

        for (existing, activated, cancel, save_fails, password) in [
            (true, true, false, false, None),
            (true, true, false, false, Some("ignored password")),
            (true, false, false, false, Some("ignored password")),
            (true, true, true, false, None),
            (false, true, false, false, Some("password")),
            (false, false, false, false, Some("password")),
            (false, true, true, false, Some("password")),
            (false, true, false, true, Some("password")),
        ] {
            let cancellable = gio::Cancellable::new();
            let calls = RefCell::new(Vec::new());
            let watched = Cell::new(false);
            let object = |path: &str| ObjectPath::try_from(path).unwrap();
            let result = glib::MainContext::new().block_on(NmService::activate_wifi(
                |path, _, method, args| {
                    std::future::ready((|| {
                        calls.borrow_mut().push(method.to_string());
                        match method {
                            "GetAll" => {
                                let props = match path {
                                    "/ap" => HashMap::from([
                                        ("Ssid", b" network:with\\spaces ".to_vec().to_variant()),
                                        ("Flags", 1u32.to_variant()),
                                        ("RsnFlags", 0x100u32.to_variant()),
                                    ]),
                                    _ => panic!("Unexpected property path: {path}"),
                                };
                                Ok((props,).to_variant())
                            }
                            "ActivateConnection" => {
                                assert_eq!(
                                    args.unwrap().get::<(ObjectPath, ObjectPath, ObjectPath)>(),
                                    Some((object("/"), object("/device"), object("/ap")))
                                );
                                if !existing {
                                    return Err(gio::DBusError::new_for_dbus_error(
                                        "org.freedesktop.NetworkManager.UnknownConnection",
                                        "No compatible profile",
                                    ));
                                }
                                if cancel {
                                    cancellable.cancel();
                                }
                                Ok((object("/active"),).to_variant())
                            }
                            "AddAndActivateConnection2" => {
                                assert!(!existing);
                                let args = args.unwrap();
                                assert_eq!(args.type_().as_str(), "(a{sa{sv}}ooa{sv})");
                                assert_eq!(
                                    args.child_value(1).get::<ObjectPath>().unwrap(),
                                    object("/device")
                                );
                                assert_eq!(
                                    args.child_value(2).get::<ObjectPath>().unwrap(),
                                    object("/ap")
                                );
                                let wifi =
                                    NmService::settings_section(args, "802-11-wireless").unwrap();
                                assert_eq!(
                                    NmService::get_prop_variant(&wifi, "ssid")
                                        .unwrap()
                                        .get::<Vec<u8>>()
                                        .unwrap(),
                                    b" network:with\\spaces "
                                );
                                let secrets =
                                    NmService::settings_section(args, "802-11-wireless-security")
                                        .unwrap();
                                assert!(
                                    NmService::get_prop_variant(&secrets, "key-mgmt").is_none()
                                );
                                assert_eq!(
                                    NmService::get_prop_variant(&secrets, "psk")
                                        .unwrap()
                                        .get::<String>()
                                        .as_deref(),
                                    Some("password")
                                );
                                assert_eq!(
                                    NmService::get_prop_variant(&secrets, "psk-flags")
                                        .unwrap()
                                        .get::<u32>(),
                                    Some(0)
                                );
                                let options = args
                                    .child_value(3)
                                    .get::<HashMap<String, Variant>>()
                                    .unwrap();
                                assert_eq!(
                                    options["persist"].get::<String>().as_deref(),
                                    Some("volatile")
                                );
                                assert!(!options.contains_key("bind-activation"));
                                if cancel {
                                    cancellable.cancel();
                                }
                                Ok((
                                    object("/profile"),
                                    object("/active"),
                                    HashMap::<String, Variant>::new(),
                                )
                                    .to_variant())
                            }
                            "Update2" => {
                                assert!(!existing && activated && !cancel);
                                assert!(
                                    watched.get(),
                                    "Saving must wait for the activation watcher"
                                );
                                assert_eq!(path, "/profile");
                                let args = args.unwrap();
                                assert_eq!(args.type_().as_str(), "(a{sa{sv}}ua{sv})");
                                assert_eq!(args.child_value(0).n_children(), 0);
                                assert_eq!(args.child_value(1).get::<u32>(), Some(1));
                                if save_fails {
                                    Err(glib::Error::new(
                                        gio::IOErrorEnum::PermissionDenied,
                                        "Permission denied",
                                    ))
                                } else {
                                    Ok((HashMap::<String, Variant>::new(),).to_variant())
                                }
                            }
                            "DeactivateConnection" => {
                                assert!(!activated || cancel);
                                assert_eq!(
                                    args.unwrap().child_value(0).get::<ObjectPath>().unwrap(),
                                    object("/active")
                                );
                                Ok(().to_variant())
                            }
                            "Delete" => {
                                assert!(!existing && (!activated || cancel));
                                assert_eq!(path, "/profile");
                                assert!(args.is_none());
                                Ok(().to_variant())
                            }
                            _ => panic!("Unexpected method: {method}"),
                        }
                    })())
                },
                |active| {
                    assert_eq!(active, "/active");
                    watched.set(true);
                    std::future::ready(if activated {
                        Ok(())
                    } else {
                        Err("Activation failed".into())
                    })
                },
                "/device",
                Some("/ap"),
                " network:with\\spaces ",
                password,
                &cancellable,
            ));
            assert_eq!(result.is_ok(), activated && !cancel);
            assert_eq!(watched.get(), !cancel);
            if save_fails {
                assert!(result.unwrap().unwrap().contains("Permission denied"));
            }
            let calls = calls.borrow();
            assert_eq!(
                calls.iter().any(|c| c == "Update2"),
                !existing && activated && !cancel
            );
            assert_eq!(
                calls.iter().any(|c| c == "DeactivateConnection"),
                !activated || cancel
            );
            assert_eq!(
                calls.iter().any(|c| c == "Delete"),
                !existing && (!activated || cancel)
            );
            if let Some(delete) = calls.iter().position(|call| call == "Delete") {
                assert!(
                    calls[..delete]
                        .iter()
                        .any(|call| call == "DeactivateConnection"),
                    "volatile profile deletion must follow active connection cleanup"
                );
            }
        }
    }

    #[test]
    fn wifi_secret_routing_leaves_settings_completion_to_nm() {
        for (flags, security, password, expected) in [
            (0, 0, None, None),
            (1, 0x100, Some("password"), Some("psk")),
            (1, 0x400, Some("password"), Some("psk")),
            (1, 0x500, Some("password"), Some("psk")),
            (1, 0x800, None, None),
            (1, 0, Some("abcde"), Some("wep-key0")),
        ] {
            let settings = new_wifi_settings("test", password, flags, security).unwrap();
            if let Some(key) = expected {
                let secrets = &settings["802-11-wireless-security"];
                assert_eq!(secrets[key].get::<String>().as_deref(), password);
                assert!(!secrets.contains_key("key-mgmt"));
            } else {
                assert!(!settings.contains_key("802-11-wireless-security"));
            }
        }
        assert!(new_wifi_settings("test", None, 1, 0x100).is_err());
    }

    #[test]
    fn activation_errors_do_not_create_fallback_profiles() {
        for name in [
            "org.freedesktop.NetworkManager.PermissionDenied",
            "org.freedesktop.NetworkManager.UnknownDevice",
            "org.freedesktop.DBus.Error.NoReply",
        ] {
            let result = glib::MainContext::new().block_on(NmService::activate_wifi(
                |_, _, method, _| {
                    std::future::ready(match method {
                        "GetAll" => Ok((HashMap::from([("Ssid", b"test".to_vec().to_variant())]),)
                            .to_variant()),
                        "ActivateConnection" => Err(gio::DBusError::new_for_dbus_error(
                            name,
                            "org.freedesktop.NetworkManager.UnknownConnection",
                        )),
                        _ => panic!("Must not call {method} after {name}"),
                    })
                },
                |_| std::future::ready(Err("Must not start watching after a method error".into())),
                "/device",
                Some("/ap"),
                "test",
                None,
                &gio::Cancellable::new(),
            ));
            assert!(result.is_err());
        }
    }

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
