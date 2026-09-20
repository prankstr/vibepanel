//! Mobile/cellular networking via ModemManager and NetworkManager D-Bus.

use std::rc::Rc;
use std::thread;
use std::time::Duration;

use gtk4::gio::{self, prelude::*};
use gtk4::glib::{self, Variant};
use tracing::warn;

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

/// Modem information gathered from ModemManager.
#[derive(Default)]
pub(super) struct MobileInfo {
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
    /// The first GSM/CDMA connection profile name, if one exists.
    pub profile_name: Option<String>,
    pub active_name: Option<String>,
    pub profile_path: Option<String>,
    pub active_path: Option<String>,
}

impl NmService {
    /// Discover mobile info in a background thread.
    pub(super) fn fetch_mobile_device_info() {
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
            let conn_name = nm_status
                .active_name
                .or(nm_status.profile_name.filter(|_| supported));

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
    /// The pending flag is cleared at the timeout callback (before spawning the
    /// fetch thread) so that new signals arriving during the fetch are not lost.
    /// This matches the IWD debounce pattern in [`IwdService::schedule_network_refresh`].
    pub(super) fn queue_mobile_refresh(&self) {
        if self.mobile.refresh_pending.get() {
            return;
        }
        self.mobile.refresh_pending.set(true);

        glib::timeout_add_local_once(Duration::from_millis(MOBILE_REFRESH_DEBOUNCE_MS), || {
            Self::global().mobile.refresh_pending.set(false);
            Self::fetch_mobile_device_info();
        });
    }

    /// Return mobile status from NetworkManager.
    pub(super) fn get_mobile_nm_status_sync() -> Result<MobileNmStatus, String> {
        let nm_proxy = system_dbus_proxy_sync(NM_SERVICE, NM_PATH, NM_IFACE)
            .map_err(|e| format!("Failed to create NM proxy: {}", e))?;
        let owner = nm_proxy.name_owner().ok_or("NetworkManager unavailable")?;
        Self::get_mobile_nm_status_for_owner(&owner, true)
    }

    fn get_mobile_nm_status_for_owner(
        owner: &str,
        include_profiles: bool,
    ) -> Result<MobileNmStatus, String> {
        let nm_proxy =
            system_dbus_proxy_sync(owner, NM_PATH, NM_IFACE).map_err(|e| e.to_string())?;

        let mut mobile_active = false;
        let mut mobile_connecting = false;
        let mut active_name: Option<String> = None;
        let mut active_path = None;
        let mut active_profile = None;
        if let Some(active_conns) = nm_proxy.cached_property("ActiveConnections") {
            for conn_path in active_conns.iter().filter_map(|v| objpath_to_string(&v)) {
                let conn_proxy = system_dbus_proxy_sync(owner, &conn_path, IFACE_ACTIVE_CONN)
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
                    if matches!(state, 1 | 2) {
                        active_path = Some(conn_path);
                        active_profile = conn_proxy
                            .cached_property("Connection")
                            .and_then(|v| objpath_to_string(&v))
                            .filter(|path| path != "/");
                    }
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

        // Disconnect needs only the active path; inaccessible settings must not block it.
        let profile = if include_profiles {
            Self::find_first_mobile_profile_sync(owner)?
        } else {
            None
        };

        Ok(MobileNmStatus {
            active: mobile_active,
            connecting: mobile_connecting,
            profile_name: profile.as_ref().map(|(_, name)| name.clone()),
            profile_path: active_profile.or_else(|| profile.map(|(path, _)| path)),
            active_path,
            active_name,
        })
    }

    fn get_connection_settings(owner: &str, conn_path: &str) -> Result<Variant, String> {
        let conn_proxy = system_dbus_proxy_sync(owner, conn_path, IFACE_SETTINGS_CONN)
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
    fn parse_connection_prop(settings: &Variant, key: &str) -> Option<String> {
        Self::get_variant_map_entry(&settings.child_value(0), "connection")
            .and_then(|props| Self::get_string_prop(&props, key))
    }

    /// Find the first GSM/CDMA profile path and display name.
    fn find_first_mobile_profile_sync(owner: &str) -> Result<Option<(String, String)>, String> {
        let settings_proxy = system_dbus_proxy_sync(owner, NM_SETTINGS_PATH, IFACE_SETTINGS)
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
            if let Ok(settings) = Self::get_connection_settings(owner, &conn)
                && let Some(ctype) = Self::parse_connection_prop(&settings, "type")
                && (ctype == "gsm" || ctype == "cdma")
            {
                return Ok(Some((
                    conn,
                    Self::parse_connection_prop(&settings, "id").unwrap_or_default(),
                )));
            }
        }
        Ok(None)
    }

    /// Read cellular signal/operator/technology from ModemManager.
    ///
    /// Returns info for the **first** modem with a SIM inserted.
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

            let Some(modem_props) = Self::get_variant_map_entry(&interfaces, MM_MODEM_IFACE) else {
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

            let operator_name = Self::get_variant_map_entry(&interfaces, MM_MODEM_3GPP_IFACE)
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

    pub(super) fn get_variant_map_entry(map: &Variant, key: &str) -> Option<Variant> {
        for i in 0..map.n_children() {
            let entry = map.child_value(i);
            if entry.child_value(0).str() == Some(key) {
                return Some(entry.child_value(1));
            }
        }
        None
    }

    pub(super) fn get_prop_variant(props: &Variant, key: &str) -> Option<Variant> {
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
            warn!(
                "SignalQuality: primary (u32, bool) parse failed, falling back to child_value(0)"
            );
            return v.child_value(0).get::<u32>().unwrap_or(0);
        }

        0
    }

    /// Enable or disable WWAN/modem via NetworkManager.
    pub fn set_mobile_enabled(self: &Rc<Self>, enabled: bool) {
        self.cancel_mobile_attempt();
        #[cfg(debug_assertions)]
        if debug_mobile_mock::is_enabled() {
            if enabled {
                self.notify_snapshot(|s| {
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
        let Some(owner) = nm.name_owner() else {
            return;
        };
        let attempt = self.mobile.attempt.get();
        let previous = self.mobile.activation_task.take();

        if enabled {
            // Enabling WWAN often triggers auto-connect of the mobile profile.
            self.mobile.connecting_local.set(true);
            self.notify_snapshot(|s| {
                s.mobile.connecting = true;
                s.mobile.enabled = Some(true);
            });
        } else {
            self.mobile.connecting_local.set(false);
            self.notify_snapshot(|s| {
                s.mobile.connecting = false;
                s.mobile.enabled = Some(false);
                s.mobile.active = false;
            });
        }

        let this = self.clone();
        let task = glib::spawn_future_local(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            let variant = Variant::tuple_from_iter([
                NM_IFACE.to_variant(),
                "WwanEnabled".to_variant(),
                enabled.to_variant().to_variant(),
            ]);

            let dbus_result = nm
                .connection()
                .call_future(
                    Some(&owner),
                    NM_PATH,
                    super::PROPERTIES_IFACE,
                    "Set",
                    Some(&variant),
                    None,
                    gio::DBusCallFlags::NONE,
                    5000,
                )
                .await;
            if let Err(ref e) = dbus_result {
                warn!("Failed to set WwanEnabled: {}", e);
                this.update_nm_flags();
            }
            // The WwanEnabled property change triggers NM's PropertiesChanged
            // signal, which fires update_nm_flags → fetch_mobile_device_info.
            this.apply_update(NmUpdate::MobileOperationFinished {
                attempt,
                success: dbus_result.is_ok(),
            });
            Self::fetch_mobile_device_info();
        });
        self.mobile.activation_task.replace(Some(task));
    }

    /// Connect the first configured mobile profile (gsm/cdma) via NetworkManager.
    pub fn connect_mobile(self: &Rc<Self>) {
        self.mobile.connecting_local.set(true);
        self.notify_snapshot(|s| {
            s.mobile.connecting = true;
            s.mobile.failed = false;
        });

        // In debug builds, simulate connect.
        #[cfg(debug_assertions)]
        if debug_mobile_mock::is_enabled() {
            // connecting (1.5s) -> connected
            debug_mobile_mock::transition_through_states(&[("connecting", 1500), ("connected", 0)]);
            return;
        }

        self.start_mobile_connection(true);
    }

    /// Disconnect active mobile connection via NetworkManager.
    pub fn disconnect_mobile(self: &Rc<Self>) {
        self.mobile.connecting_local.set(false);
        self.notify_snapshot(|s| {
            s.mobile.connecting = false;
            s.mobile.active = false;
        });

        // In debug builds, simulate disconnect.
        #[cfg(debug_assertions)]
        if debug_mobile_mock::is_enabled() {
            // -> registered (after 800ms settling)
            debug_mobile_mock::transition_through_states(&[("enabled", 800), ("registered", 0)]);
            return;
        }

        self.start_mobile_connection(false);
    }

    /// Invalidate the in-flight attempt; leaves `connecting_local` to the caller.
    fn abort_mobile_attempt(&self) {
        self.mobile.attempt.set(self.mobile.attempt.get() + 1);
        if let Some(cancel) = self.mobile.activation_cancel.take() {
            cancel.cancel();
        }
    }

    pub(super) fn cancel_mobile_attempt(&self) {
        self.abort_mobile_attempt();
        self.mobile.connecting_local.set(false);
    }

    fn start_mobile_connection(self: &Rc<Self>, connect: bool) {
        // Callers already set `connecting_local`; don't clear and re-set it here.
        self.abort_mobile_attempt();
        let attempt = self.mobile.attempt.get();
        let cancel = gio::Cancellable::new();
        self.mobile.activation_cancel.replace(Some(cancel.clone()));
        let previous = self.mobile.activation_task.take();
        let target = self.nm_proxy.borrow().as_ref().and_then(|nm| {
            nm.name_owner()
                .map(|owner| (nm.connection(), owner.to_string()))
        });
        let this = self.clone();
        let task = glib::spawn_future_local(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            if cancel.is_cancelled() {
                return;
            }
            let result = async {
                let (bus, owner) = target.ok_or("NetworkManager unavailable")?;
                let (sender, receiver) = async_channel::bounded(1);
                let cancelled = cancel.connect_cancelled({
                    let sender = sender.clone();
                    move |_| {
                        let _ = sender.try_send(Err("Connection cancelled".into()));
                    }
                });
                let selected_owner = owner.clone();
                thread::spawn(move || {
                    let _ = sender.try_send(Self::get_mobile_nm_status_for_owner(
                        &selected_owner,
                        connect,
                    ));
                });
                let status = receiver.recv().await.map_err(|e| e.to_string());
                if let Some(id) = cancelled {
                    cancel.disconnect_cancelled(id);
                }
                let status = status??;
                mobile_connection(
                    |method, args| {
                        bus.call_future(
                            Some(&owner),
                            NM_PATH,
                            NM_IFACE,
                            method,
                            Some(args),
                            None,
                            gio::DBusCallFlags::NONE,
                            30_000,
                        )
                    },
                    |active| {
                        super::wifi::wait_activation(
                            bus.clone(),
                            owner.clone(),
                            active.into(),
                            cancel.clone(),
                            Duration::from_secs(60),
                        )
                    },
                    &status,
                    connect,
                    &cancel,
                )
                .await
            }
            .await;
            if let Err(e) = &result {
                warn!("Mobile connection operation failed: {e}");
            }
            if this.mobile.attempt.get() == attempt {
                this.apply_update(NmUpdate::MobileOperationFinished {
                    attempt,
                    success: result.is_ok(),
                });
                Self::fetch_mobile_device_info();
            }
        });
        self.mobile.activation_task.replace(Some(task));
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

async fn mobile_connection<F, W>(
    call: impl Fn(&str, &Variant) -> F,
    wait: impl FnOnce(&str) -> W,
    status: &MobileNmStatus,
    connect: bool,
    cancel: &gio::Cancellable,
) -> Result<(), String>
where
    F: std::future::Future<Output = Result<Variant, glib::Error>>,
    W: std::future::Future<Output = Result<(), String>>,
{
    use glib::variant::ObjectPath;
    if cancel.is_cancelled() {
        return Err("Connection cancelled".into());
    }
    if !connect {
        if let Some(active) = &status.active_path {
            let active = ObjectPath::try_from(active.as_str()).map_err(|e| e.to_string())?;
            call("DeactivateConnection", &(active,).to_variant())
                .await
                .map_err(|e| e.to_string())?;
        }
        return Ok(());
    }
    let profile = status
        .profile_path
        .as_deref()
        .ok_or("No GSM/CDMA profile found")?;
    let profile = ObjectPath::try_from(profile).map_err(|e| e.to_string())?;
    let automatic = ObjectPath::try_from("/").expect("valid root path");
    // Keep the reply even when cancelled: it identifies exactly what needs cleanup.
    let reply = call(
        "ActivateConnection",
        &(profile, automatic.clone(), automatic).to_variant(),
    )
    .await
    .map_err(|e| e.to_string())?;
    let (active,) = reply
        .get::<(ObjectPath,)>()
        .ok_or("Invalid activation path")?;
    let result = if cancel.is_cancelled() {
        Err("Connection cancelled".into())
    } else {
        wait(active.as_str()).await
    };
    if result.is_err() || cancel.is_cancelled() {
        if let Err(e) = call("DeactivateConnection", &(active,).to_variant()).await {
            warn!("Mobile activation cleanup failed: {e}");
        }
        return Err(result
            .err()
            .unwrap_or_else(|| "Connection cancelled".into()));
    }
    Ok(())
}

/// Check if a mobile/cellular connection is active.
pub(super) fn is_mobile_connected(primary_type: Option<&str>) -> bool {
    primary_type.is_some_and(|t| t == "gsm" || t == "cdma")
}

/// Resolve the effective `connecting` state for mobile by merging the local
/// optimistic flag with the real D-Bus state.
///
/// Returns `(effective_connecting, clear_local_flag)`.
/// See the `MobileDeviceInfo` arm of `apply_update` for the full strategy.
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

    #[test]
    fn mobile_connection_lifecycle() {
        use glib::variant::ObjectPath;
        use std::cell::{Cell, RefCell};
        for case in [
            "success",
            "failure",
            "cancel_before",
            "cancel_reply",
            "cancel_wait",
            "method_error",
            "disconnect",
            "disconnect_error",
            "inactive",
            "no_profile",
        ] {
            let cancel = gio::Cancellable::new();
            if case == "cancel_before" {
                cancel.cancel();
            }
            let status = MobileNmStatus {
                profile_path: (case != "no_profile").then(|| "/profile".into()),
                active_path: (case != "inactive").then(|| "/old_active".into()),
                ..Default::default()
            };
            let calls = RefCell::new(Vec::new());
            let waited = Cell::new(false);
            let object = |path: &str| ObjectPath::try_from(path).unwrap();
            let connect = !matches!(case, "disconnect" | "disconnect_error" | "inactive");
            let result = glib::MainContext::new().block_on(mobile_connection(
                |method, args| {
                    calls.borrow_mut().push(method.to_string());
                    std::future::ready(match method {
                        "ActivateConnection" => {
                            assert_eq!(
                                args.get::<(ObjectPath, ObjectPath, ObjectPath)>(),
                                Some((object("/profile"), object("/"), object("/")))
                            );
                            if case == "cancel_reply" {
                                cancel.cancel();
                            }
                            if case == "method_error" {
                                Err(glib::Error::new(
                                    gio::IOErrorEnum::PermissionDenied,
                                    "Denied",
                                ))
                            } else {
                                Ok((object("/new_active"),).to_variant())
                            }
                        }
                        "DeactivateConnection" => {
                            assert_eq!(
                                args.get::<(ObjectPath,)>(),
                                Some((object(if connect {
                                    "/new_active"
                                } else {
                                    "/old_active"
                                }),))
                            );
                            if case == "disconnect_error" {
                                Err(glib::Error::new(
                                    gio::IOErrorEnum::PermissionDenied,
                                    "Denied",
                                ))
                            } else {
                                Ok(().to_variant())
                            }
                        }
                        _ => panic!("Unexpected method {method}"),
                    })
                },
                |active| {
                    assert_eq!(active, "/new_active");
                    waited.set(true);
                    if case == "cancel_wait" {
                        cancel.cancel();
                    }
                    std::future::ready(if case == "failure" {
                        Err("Activation failed".into())
                    } else {
                        Ok(())
                    })
                },
                &status,
                connect,
                &cancel,
            ));
            assert_eq!(
                result.is_ok(),
                matches!(case, "success" | "disconnect" | "inactive"),
                "{case}"
            );
            assert_eq!(
                waited.get(),
                matches!(case, "success" | "failure" | "cancel_wait"),
                "{case}"
            );
            let expected: &[&str] = match case {
                "cancel_before" | "inactive" | "no_profile" => &[],
                "disconnect" | "disconnect_error" => &["DeactivateConnection"],
                "failure" | "cancel_reply" | "cancel_wait" => {
                    &["ActivateConnection", "DeactivateConnection"]
                }
                _ => &["ActivateConnection"],
            };
            assert_eq!(*calls.borrow(), expected, "{case}");
        }
    }

    #[test]
    fn obsolete_mobile_completion_cannot_clear_new_attempt() {
        use super::super::{MobileInternal, NmSnapshot, WifiInternal};
        use std::cell::RefCell;
        let service = NmService {
            nm_proxy: RefCell::new(None),
            snapshot: RefCell::new(NmSnapshot::unknown()),
            callbacks: crate::services::callbacks::Callbacks::new(),
            wifi: WifiInternal::new(),
            mobile: MobileInternal::new(),
        };
        let cancel = gio::Cancellable::new();
        service
            .mobile
            .activation_cancel
            .replace(Some(cancel.clone()));
        service.cancel_mobile_attempt();
        assert!(cancel.is_cancelled());
        service.mobile.connecting_local.set(true);
        service.notify_snapshot(|s| s.mobile.connecting = true);
        service.apply_update(NmUpdate::MobileOperationFinished {
            attempt: 0,
            success: false,
        });
        assert!(service.mobile.connecting_local.get());
        assert!(service.snapshot().mobile.connecting);
        assert!(!service.snapshot().mobile.failed);
        service.apply_update(NmUpdate::MobileOperationFinished {
            attempt: 1,
            success: true,
        });
        assert!(!service.mobile.connecting_local.get());
        assert!(!service.snapshot().mobile.connecting);
    }
}
