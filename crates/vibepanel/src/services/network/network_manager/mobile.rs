//! Mobile/cellular networking via ModemManager and NetworkManager D-Bus.

use std::process::Command;
use std::thread;
use std::time::Duration;

use gtk4::gio::{self, prelude::*};
use gtk4::glib::{self, Variant};
use tracing::{error, warn};

use super::{
    IFACE_ACTIVE_CONN, IFACE_SETTINGS, IFACE_SETTINGS_CONN, MM_ACCESS_TECH_EDGE,
    MM_ACCESS_TECH_GPRS, MM_ACCESS_TECH_GSM, MM_ACCESS_TECH_GSM_COMPACT, MM_ACCESS_TECH_HSDPA,
    MM_ACCESS_TECH_HSPA_PLUS, MM_ACCESS_TECH_HSUPA, MM_ACCESS_TECH_LTE, MM_ACCESS_TECH_LTE_CAT_M,
    MM_ACCESS_TECH_LTE_NB_IOT, MM_ACCESS_TECH_NR5G, MM_ACCESS_TECH_UMTS, MM_MODEM_3GPP_IFACE,
    MM_MODEM_IFACE, MM_PATH, MM_SERVICE, MOBILE_REFRESH_DEBOUNCE_MS, NM_IFACE, NM_PATH, NM_SERVICE,
    NM_SETTINGS_PATH, NmService, NmUpdate, OBJECT_MANAGER_IFACE, send_nm_update,
    system_dbus_proxy_sync,
};
use crate::services::network::objpath_to_string;

#[cfg(debug_assertions)]
use super::debug_mobile_mock;

/// Modem information gathered from ModemManager (SIM, signal, technology, operator).
#[derive(Default)]
pub(super) struct MobileInfo {
    /// Whether a modem object was found in ModemManager.
    pub has_modem: bool,
    pub has_sim: bool,
    pub signal_quality: Option<u32>,
    pub access_technology: Option<String>,
    pub operator_name: Option<String>,
}

/// Mobile connection status from NetworkManager (active connections & profiles).
#[derive(Default)]
pub(super) struct MobileNmStatus {
    pub active: bool,
    pub connecting: bool,
    /// The name of the first GSM/CDMA connection profile, if one exists.
    /// Use `.is_some()` to check whether a mobile profile is configured.
    pub profile_name: Option<String>,
    pub active_name: Option<String>,
}

impl NmService {
    /// Discover mobile info in a background thread.
    pub(super) fn fetch_mobile_device_info() {
        // In debug builds, return mock data if the debug mock is enabled.
        #[cfg(debug_assertions)]
        if debug_mobile_mock::is_enabled()
            && let Some(mock) = debug_mobile_mock::read_state()
        {
            debug_mobile_mock::send_mock_updates(&mock);
            return;
        }

        thread::spawn(move || {
            let nm_status = Self::get_mobile_nm_status_sync().unwrap_or_default();

            let mm_info = Self::get_mobile_info_from_mm_sync().unwrap_or_default();

            let has_profile = nm_status.profile_name.is_some();
            let supported = mm_info.has_modem && mm_info.has_sim && has_profile;
            let conn_name = nm_status.active_name.or(if supported {
                nm_status.profile_name
            } else {
                None
            });

            send_nm_update(NmUpdate::MobileDeviceInfo {
                conn_name,
                operator_name: mm_info.operator_name,
                access_technology: mm_info.access_technology,
                signal_quality: mm_info.signal_quality,
                active: nm_status.active,
                connecting: nm_status.connecting,
                supported,
                has_modem: mm_info.has_modem,
            });
        });
    }

    /// Queue a debounced mobile info refresh.
    ///
    /// Multiple calls within [`MOBILE_REFRESH_DEBOUNCE_MS`] are coalesced into one.
    pub(super) fn queue_mobile_refresh(&self) {
        if self.mobile.refresh_pending.get() {
            return; // Already pending — coalesce.
        }
        self.mobile.refresh_pending.set(true);

        glib::timeout_add_local_once(Duration::from_millis(MOBILE_REFRESH_DEBOUNCE_MS), || {
            Self::fetch_mobile_device_info();
        });
    }

    /// Return mobile status from NetworkManager:
    /// (mobile_active, has_mobile_profile, active_mobile_connection_name)
    pub(super) fn get_mobile_nm_status_sync() -> Result<MobileNmStatus, String> {
        let nm_proxy = system_dbus_proxy_sync(NM_SERVICE, NM_PATH, NM_IFACE)
            .map_err(|e| format!("Failed to create NM proxy: {}", e))?;

        let mut mobile_active = false;
        let mut mobile_connecting = false;
        let mut active_name: Option<String> = None;
        if let Some(active_conns) = nm_proxy.cached_property("ActiveConnections") {
            for conn_path in active_conns.iter().filter_map(|v| objpath_to_string(&v)) {
                let conn_proxy = system_dbus_proxy_sync(NM_SERVICE, &conn_path, IFACE_ACTIVE_CONN)
                    .map_err(|e| format!("Failed to create active conn proxy: {}", e))?;

                let ctype = conn_proxy
                    .cached_property("Type")
                    .and_then(|v| v.get::<String>())
                    .unwrap_or_default();
                if ctype == "gsm" || ctype == "cdma" {
                    let state = conn_proxy
                        .cached_property("State")
                        .and_then(|v| v.get::<u32>())
                        .unwrap_or(0);
                    active_name = conn_proxy
                        .cached_property("Id")
                        .and_then(|v| v.get::<String>());
                    match state {
                        // NM_ACTIVE_CONNECTION_STATE_ACTIVATED
                        2 => {
                            mobile_active = true;
                            break;
                        }
                        // NM_ACTIVE_CONNECTION_STATE_ACTIVATING
                        1 => {
                            mobile_connecting = true;
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }

        let mobile_profile_name = Self::find_first_mobile_profile_sync()?;

        Ok(MobileNmStatus {
            active: mobile_active,
            connecting: mobile_connecting,
            profile_name: mobile_profile_name,
            active_name,
        })
    }

    fn get_connection_settings(conn_path: &str) -> Result<Variant, String> {
        let conn_proxy = system_dbus_proxy_sync(NM_SERVICE, conn_path, IFACE_SETTINGS_CONN)
            .map_err(|e| format!("Failed to create settings conn proxy: {}", e))?;

        conn_proxy
            .call_sync(
                "GetSettings",
                None,
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
            )
            .map_err(|e| format!("GetSettings failed: {}", e))
    }

    /// Extract a property from the "connection" section of a D-Bus settings variant.
    ///
    /// Used to read `"type"` (gsm/cdma/wifi/...) or `"id"` (profile name) from
    /// the result of `GetSettings`.
    fn parse_connection_prop(settings: &Variant, key: &str) -> Option<String> {
        let root = settings.child_value(0);
        for i in 0..root.n_children() {
            let section = root.child_value(i);
            if section.child_value(0).str() == Some("connection") {
                let props = section.child_value(1);
                return Self::get_string_prop(&props, key);
            }
        }
        None
    }

    /// Find the first GSM/CDMA connection profile name via NetworkManager's Settings interface.
    ///
    /// Returns `Ok(Some(name))` if a mobile profile exists, `Ok(None)` if none found.
    /// This replaces both the old `has_mobile_profile_sync` (use `.is_some()`) and
    /// `get_first_mobile_profile_name_sync`.
    pub(super) fn find_first_mobile_profile_sync() -> Result<Option<String>, String> {
        let settings_proxy = system_dbus_proxy_sync(NM_SERVICE, NM_SETTINGS_PATH, IFACE_SETTINGS)
            .map_err(|e| format!("Failed to create NM settings proxy: {}", e))?;

        let result = settings_proxy
            .call_sync(
                "ListConnections",
                None,
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
            )
            .map_err(|e| format!("ListConnections failed: {}", e))?;

        for conn in result
            .child_value(0)
            .iter()
            .filter_map(|v| objpath_to_string(&v))
        {
            if let Ok(settings) = Self::get_connection_settings(&conn)
                && let Some(ctype) = Self::parse_connection_prop(&settings, "type")
                && (ctype == "gsm" || ctype == "cdma")
            {
                return Ok(Self::parse_connection_prop(&settings, "id"));
            }
        }
        Ok(None)
    }

    /// Read cellular signal/operator/technology from ModemManager.
    ///
    /// Also reports `has_modem` — whether at least one modem object was found
    /// in ModemManager, which replaces the separate `has_modem_device_sync()`
    /// NM device enumeration that was previously used.
    ///
    /// Note: returns info for the **first** modem with a SIM inserted.
    /// Multi-modem setups are not currently supported.
    fn get_mobile_info_from_mm_sync() -> Result<MobileInfo, String> {
        let proxy = system_dbus_proxy_sync(MM_SERVICE, MM_PATH, OBJECT_MANAGER_IFACE)
            .map_err(|e| format!("Failed to create MM proxy: {}", e))?;

        let result = proxy
            .call_sync(
                "GetManagedObjects",
                None,
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
            )
            .map_err(|e| format!("GetManagedObjects failed: {}", e))?;

        let objects = result.child_value(0);
        let mut found_modem = false;
        for i in 0..objects.n_children() {
            let object_entry = objects.child_value(i);
            let interfaces = object_entry.child_value(1);

            let Some(modem_props) = Self::get_interface_props(&interfaces, MM_MODEM_IFACE) else {
                continue;
            };

            found_modem = true;

            let sim_path = Self::get_object_path_prop(&modem_props, "Sim").unwrap_or_default();
            if sim_path.is_empty() || sim_path == "/" {
                continue;
            }

            let signal_quality = Self::get_signal_quality_prop(&modem_props);
            let access_bits = Self::get_u32_prop(&modem_props, "AccessTechnologies");
            let access_technology = access_bits
                .map(access_technology_label)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string);

            let operator_name = Self::get_interface_props(&interfaces, MM_MODEM_3GPP_IFACE)
                .and_then(|props| Self::get_string_prop(&props, "OperatorName"))
                .filter(|name| !name.trim().is_empty());

            return Ok(MobileInfo {
                has_modem: true,
                has_sim: true,
                signal_quality: Some(signal_quality),
                access_technology,
                operator_name,
            });
        }

        Ok(MobileInfo {
            has_modem: found_modem,
            has_sim: false,
            signal_quality: None,
            access_technology: None,
            operator_name: None,
        })
    }

    fn get_interface_props(interfaces: &Variant, iface_name: &str) -> Option<Variant> {
        for i in 0..interfaces.n_children() {
            let iface_entry = interfaces.child_value(i);
            if iface_entry.child_value(0).str() == Some(iface_name) {
                return Some(iface_entry.child_value(1));
            }
        }
        None
    }

    fn get_prop_variant(props: &Variant, key: &str) -> Option<Variant> {
        for i in 0..props.n_children() {
            let prop_entry = props.child_value(i);
            if prop_entry.child_value(0).str() == Some(key) {
                let boxed = prop_entry.child_value(1);
                return Some(boxed.child_value(0));
            }
        }
        None
    }

    fn get_u32_prop(props: &Variant, key: &str) -> Option<u32> {
        Self::get_prop_variant(props, key).and_then(|v| v.get::<u32>())
    }

    fn get_string_prop(props: &Variant, key: &str) -> Option<String> {
        Self::get_prop_variant(props, key).and_then(|v| v.get::<String>())
    }

    fn get_object_path_prop(props: &Variant, key: &str) -> Option<String> {
        Self::get_prop_variant(props, key)
            .and_then(|v| v.get::<glib::variant::ObjectPath>())
            .map(|p| p.as_str().to_string())
    }

    fn get_signal_quality_prop(props: &Variant) -> u32 {
        let Some(v) = Self::get_prop_variant(props, "SignalQuality") else {
            return 0;
        };
        // ModemManager documents SignalQuality as `(ub)` — a tuple of
        // (quality_percent, recently_updated).  Some MM versions / glib
        // bindings expose it as a nested Variant instead of a direct tuple,
        // so fall back to reading the first child as a raw u32.
        if let Some((quality, _recent)) = v.get::<(u32, bool)>() {
            return quality;
        }

        if v.n_children() > 0 {
            return v.child_value(0).get::<u32>().unwrap_or(0);
        }

        0
    }

    /// Enable or disable WWAN/modem via NetworkManager.
    pub fn set_mobile_enabled(&self, enabled: bool) {
        // In debug builds, update mock state with realistic delays.
        #[cfg(debug_assertions)]
        if debug_mobile_mock::is_enabled() {
            if enabled {
                // Set connecting state for instant UI feedback (same as production path).
                self.mobile.connecting_local.set(true);
                self.notify_snapshot(|s| {
                    s.mobile.connecting = true;
                    s.mobile.enabled = Some(true);
                });
                // disabled -> enabled (500ms) -> registered
                debug_mobile_mock::transition_through_states(&[
                    ("enabled", 500),
                    ("registered", 0),
                ]);
            } else {
                self.mobile.connecting_local.set(false);
                self.notify_snapshot(|s| {
                    s.mobile.connecting = false;
                    s.mobile.enabled = Some(false);
                    s.mobile.active = false;
                });
                // -> disabled
                debug_mobile_mock::transition_through_states(&[("disabled", 0)]);
            }
            return;
        }

        let Some(nm) = self.nm_proxy.borrow().clone() else {
            return;
        };

        if enabled {
            // Set connecting state synchronously for instant UI feedback.
            // Enabling WWAN often triggers auto-connect of the mobile profile.
            self.mobile.connecting_local.set(true);
            self.notify_snapshot(|s| {
                s.mobile.connecting = true;
                s.mobile.enabled = Some(true);
            });
        } else {
            // Disabling — clear connecting state immediately.
            self.mobile.connecting_local.set(false);
            self.notify_snapshot(|s| {
                s.mobile.connecting = false;
                s.mobile.enabled = Some(false);
                s.mobile.active = false;
            });
        }

        thread::spawn(move || {
            let variant = Variant::tuple_from_iter([
                NM_IFACE.to_variant(),
                "WwanEnabled".to_variant(),
                enabled.to_variant().to_variant(),
            ]);

            let dbus_result = nm.call_sync(
                "org.freedesktop.DBus.Properties.Set",
                Some(&variant),
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
            );
            if let Err(ref e) = dbus_result {
                error!("Failed to set WwanEnabled: {}", e);
            }
            // WwanEnabled property change will trigger NM PropertiesChanged signal,
            // which fires update_nm_flags and MM signal subscriptions.
            // Clear local connecting intent so the real state takes over.
            //
            // Reuses `MobileConnectionAttemptFinished` even though this is an
            // enable/disable operation, not a connection attempt.  The handler
            // only clears `connecting_local` and sets the `failed` flag on
            // error — both of which are exactly what we need here.
            if enabled {
                send_nm_update(NmUpdate::MobileConnectionAttemptFinished {
                    success: dbus_result.is_ok(),
                });
            }
            Self::fetch_mobile_device_info();
            send_nm_update(NmUpdate::RefreshNetworks);
        });
    }

    /// Connect the first configured mobile profile (gsm/cdma) via NetworkManager.
    pub fn connect_mobile(&self) {
        // Set connecting state synchronously for instant UI feedback (same pattern as
        // connecting_ssid for Wi-Fi). Cleared when MobileDeviceInfo arrives with real state.
        self.mobile.connecting_local.set(true);
        self.notify_snapshot(|s| {
            s.mobile.connecting = true;
            s.mobile.failed = false;
        });

        // In debug builds, simulate connect with realistic delays.
        #[cfg(debug_assertions)]
        if debug_mobile_mock::is_enabled() {
            // connecting (1.5s) -> connected
            debug_mobile_mock::transition_through_states(&[("connecting", 1500), ("connected", 0)]);
            return;
        }

        thread::spawn(move || {
            let conn_name = match Self::get_mobile_nm_status_sync() {
                Ok(status) => status.active_name.or(status.profile_name),
                _ => Self::find_first_mobile_profile_sync().ok().flatten(),
            };

            let Some(conn_name) = conn_name else {
                warn!("No GSM/CDMA profile found to connect");
                // No profile — clear connecting intent and refresh.
                send_nm_update(NmUpdate::MobileConnectionAttemptFinished { success: false });
                Self::fetch_mobile_device_info();
                return;
            };

            let success = match Command::new("nmcli")
                .args(["connection", "up", "id", &conn_name])
                .output()
            {
                Ok(output) => {
                    if output.status.success() {
                        true
                    } else {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        warn!(
                            "nmcli mobile connect failed for '{}': {}",
                            conn_name,
                            stderr.trim()
                        );
                        false
                    }
                }
                Err(e) => {
                    error!("Failed to run nmcli: {}", e);
                    false
                }
            };
            // nmcli returned — NM state is now settled. Clear local connecting intent
            // first, then fetch real mobile state.
            send_nm_update(NmUpdate::MobileConnectionAttemptFinished { success });
            Self::fetch_mobile_device_info();
            send_nm_update(NmUpdate::RefreshNetworks);
        });
    }

    /// Disconnect active mobile connection via NetworkManager.
    ///
    /// Unlike `connect_mobile()`, this does **not** explicitly call
    /// `fetch_mobile_device_info()` after `nmcli connection down`. It doesn't
    /// need to: deactivating a connection triggers NM's `PropertiesChanged`
    /// signal on the primary-connection and active-connections properties,
    /// which fires `update_nm_flags()`. That detects the mobile state change
    /// (`mobile_changed`) and calls `fetch_mobile_device_info()` automatically,
    /// so the UI converges to the correct state through the existing signal
    /// cascade without an explicit fetch.
    pub fn disconnect_mobile(&self) {
        // Optimistically clear connecting and active state so the UI updates
        // instantly. The real D-Bus state will arrive shortly via the NM signal
        // cascade (PrimaryConnectionType change → update_nm_flags → fetch).
        self.mobile.connecting_local.set(false);
        self.notify_snapshot(|s| {
            s.mobile.connecting = false;
            s.mobile.active = false;
        });

        // In debug builds, simulate disconnect with realistic delay.
        #[cfg(debug_assertions)]
        if debug_mobile_mock::is_enabled() {
            // -> registered (after 800ms settling)
            debug_mobile_mock::transition_through_states(&[("enabled", 800), ("registered", 0)]);
            return;
        }

        thread::spawn(move || {
            let active_name = Self::get_mobile_nm_status_sync().ok().and_then(|status| {
                if status.active {
                    status.active_name
                } else {
                    None
                }
            });

            if let Some(name) = active_name {
                match Command::new("nmcli")
                    .args(["connection", "down", "id", &name])
                    .output()
                {
                    Ok(output) if !output.status.success() => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        warn!(
                            "nmcli mobile disconnect failed for '{}': {}",
                            name,
                            stderr.trim()
                        );
                    }
                    Err(e) => {
                        error!("Failed to run nmcli: {}", e);
                    }
                    _ => {}
                }
            }

            send_nm_update(NmUpdate::RefreshNetworks);
        });
    }

    /// Clear the mobile failed connection state (called by UI after showing error).
    pub fn clear_mobile_failed_state(&self) {
        self.notify_snapshot_if(|s| {
            let changed = s.mobile.failed;
            s.mobile.failed = false;
            changed
        });
    }
}

/// Check if a mobile/cellular connection is active.
pub(super) fn is_mobile_connected(primary_type: Option<&str>) -> bool {
    primary_type.is_some_and(|t| t == "gsm" || t == "cdma")
}

/// Resolve the effective `connecting` state for mobile by merging the local
/// optimistic flag with the real D-Bus state.
///
/// Returns `(effective_connecting, clear_local_flag)`:
/// - `effective_connecting`: the value to store in `MobileState.connecting`.
/// - `clear_local_flag`: whether `mobile_connecting_local` should be cleared.
///
/// See the doc comment on the `MobileDeviceInfo` arm of `apply_update` for the
/// full race-resolution strategy.
pub(super) fn resolve_mobile_connecting(
    local_flag: bool,
    dbus_active: bool,
    dbus_connecting: bool,
) -> (bool, bool) {
    if local_flag {
        if dbus_active || dbus_connecting {
            // NM caught up — local flag no longer needed.
            (dbus_connecting, true)
        } else {
            // NM hasn't reflected the attempt yet — keep showing connecting.
            (true, false)
        }
    } else {
        (dbus_connecting, false)
    }
}

/// Convert MM access technology bit flags to a compact label.
pub(super) fn access_technology_label(bits: u32) -> &'static str {
    let hspa = MM_ACCESS_TECH_HSDPA | MM_ACCESS_TECH_HSUPA;

    if bits & MM_ACCESS_TECH_NR5G != 0 {
        "5G"
    } else if bits & (MM_ACCESS_TECH_LTE_CAT_M | MM_ACCESS_TECH_LTE_NB_IOT) != 0 {
        "LTE+"
    } else if bits & MM_ACCESS_TECH_LTE != 0 {
        "LTE"
    } else if bits & MM_ACCESS_TECH_HSPA_PLUS != 0 {
        "HSPA+"
    } else if bits & hspa != 0 {
        "HSPA"
    } else if bits & MM_ACCESS_TECH_UMTS != 0 {
        "3G"
    } else if bits & MM_ACCESS_TECH_EDGE != 0 {
        "EDGE"
    } else if bits & (MM_ACCESS_TECH_GPRS | MM_ACCESS_TECH_GSM | MM_ACCESS_TECH_GSM_COMPACT) != 0 {
        "2G"
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_technology_5g() {
        assert_eq!(access_technology_label(MM_ACCESS_TECH_NR5G), "5G");
    }

    #[test]
    fn access_technology_lte_plus() {
        assert_eq!(access_technology_label(MM_ACCESS_TECH_LTE_CAT_M), "LTE+");
        assert_eq!(access_technology_label(MM_ACCESS_TECH_LTE_NB_IOT), "LTE+");
    }

    #[test]
    fn access_technology_lte() {
        assert_eq!(access_technology_label(MM_ACCESS_TECH_LTE), "LTE");
    }

    #[test]
    fn access_technology_hspa_plus() {
        assert_eq!(access_technology_label(MM_ACCESS_TECH_HSPA_PLUS), "HSPA+");
    }

    #[test]
    fn access_technology_hspa() {
        assert_eq!(
            access_technology_label(MM_ACCESS_TECH_HSDPA | MM_ACCESS_TECH_HSUPA),
            "HSPA"
        );
        // Single HSDPA bit also counts as HSPA.
        assert_eq!(access_technology_label(MM_ACCESS_TECH_HSDPA), "HSPA");
    }

    #[test]
    fn access_technology_3g() {
        assert_eq!(access_technology_label(MM_ACCESS_TECH_UMTS), "3G");
    }

    #[test]
    fn access_technology_edge() {
        assert_eq!(access_technology_label(MM_ACCESS_TECH_EDGE), "EDGE");
    }

    #[test]
    fn access_technology_2g() {
        assert_eq!(access_technology_label(MM_ACCESS_TECH_GPRS), "2G");
        assert_eq!(access_technology_label(MM_ACCESS_TECH_GSM), "2G");
        assert_eq!(access_technology_label(MM_ACCESS_TECH_GSM_COMPACT), "2G");
    }

    #[test]
    fn access_technology_unknown_returns_empty() {
        assert_eq!(access_technology_label(0), "");
        assert_eq!(access_technology_label(1), ""); // bit 0, no known tech
    }

    #[test]
    fn access_technology_highest_wins() {
        // When multiple bits are set, the highest-priority tech should win.
        // 5G beats everything.
        assert_eq!(
            access_technology_label(MM_ACCESS_TECH_NR5G | MM_ACCESS_TECH_LTE),
            "5G"
        );
        // LTE beats 3G.
        assert_eq!(
            access_technology_label(MM_ACCESS_TECH_LTE | MM_ACCESS_TECH_UMTS),
            "LTE"
        );
    }

    // --- resolve_mobile_connecting tests ---

    #[test]
    fn mobile_connecting_local_true_dbus_active_clears_flag() {
        // D-Bus says active (connected) → local flag is redundant, clear it.
        // effective_connecting = dbus_connecting (false here).
        let (effective, clear) = resolve_mobile_connecting(true, true, false);
        assert!(!effective, "should use D-Bus connecting (false)");
        assert!(clear, "should clear local flag");
    }

    #[test]
    fn mobile_connecting_local_true_dbus_connecting_clears_flag() {
        // D-Bus says connecting → local flag is redundant, clear it.
        // effective_connecting = dbus_connecting (true).
        let (effective, clear) = resolve_mobile_connecting(true, false, true);
        assert!(effective, "should use D-Bus connecting (true)");
        assert!(clear, "should clear local flag");
    }

    #[test]
    fn mobile_connecting_local_true_dbus_neither_keeps_flag() {
        // D-Bus hasn't reflected the attempt yet → keep local flag,
        // show connecting = true.
        let (effective, clear) = resolve_mobile_connecting(true, false, false);
        assert!(effective, "should keep showing connecting from local flag");
        assert!(!clear, "should NOT clear local flag");
    }

    #[test]
    fn mobile_connecting_local_false_passes_through_dbus() {
        // No local intent — pass through whatever D-Bus says.
        let (eff1, clr1) = resolve_mobile_connecting(false, false, false);
        assert!(!eff1);
        assert!(!clr1);

        let (eff2, clr2) = resolve_mobile_connecting(false, false, true);
        assert!(eff2);
        assert!(!clr2);

        let (eff3, clr3) = resolve_mobile_connecting(false, true, false);
        assert!(!eff3);
        assert!(!clr3);
    }

    #[test]
    fn mobile_connecting_local_true_dbus_both_active_and_connecting() {
        // Edge case: both active and connecting set (shouldn't normally happen,
        // but D-Bus signals may race). Local flag clears, uses dbus_connecting.
        let (effective, clear) = resolve_mobile_connecting(true, true, true);
        assert!(effective, "should use D-Bus connecting (true)");
        assert!(clear, "should clear local flag");
    }
}
