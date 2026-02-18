//! NmService — Network state via NetworkManager over D-Bus.
//!
//! Uses Gio's async D-Bus proxy; background threads deliver updates via `glib::idle_add_once()`.
//!
//! Sub-modules split by technology:
//! - [`wifi`] — Wi-Fi proxy, scanning, connect/disconnect/forget
//! - [`wired`] — Ethernet device info fetching
//! - [`mobile`] — ModemManager integration, cellular connect/disconnect

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use gtk4::gio::{self, prelude::*};
use gtk4::glib::{self, Variant};
use tracing::{debug, error};

use crate::services::callbacks::{CallbackId, Callbacks};
use crate::services::network::{WifiNetwork, objpath_to_string};

mod mobile;
mod wifi;
mod wired;

// D-Bus Constants

pub const NM_SERVICE: &str = "org.freedesktop.NetworkManager";
pub const NM_PATH: &str = "/org/freedesktop/NetworkManager";
pub const NM_IFACE: &str = "org.freedesktop.NetworkManager";
const IFACE_DEV: &str = "org.freedesktop.NetworkManager.Device";
const IFACE_WIFI: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const IFACE_WIRED: &str = "org.freedesktop.NetworkManager.Device.Wired";
const IFACE_AP: &str = "org.freedesktop.NetworkManager.AccessPoint";
const IFACE_ACTIVE_CONN: &str = "org.freedesktop.NetworkManager.Connection.Active";
const NM_SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const IFACE_SETTINGS: &str = "org.freedesktop.NetworkManager.Settings";
const IFACE_SETTINGS_CONN: &str = "org.freedesktop.NetworkManager.Settings.Connection";

const ETHERNET_DEVICE_TYPE: u32 = 1; // NM_DEVICE_TYPE_ETHERNET
const WIFI_DEVICE_TYPE: u32 = 2; // NM_DEVICE_TYPE_WIFI
const MODEM_DEVICE_TYPE: u32 = 8; // NM_DEVICE_TYPE_MODEM

const MM_SERVICE: &str = "org.freedesktop.ModemManager1";
const MM_PATH: &str = "/org/freedesktop/ModemManager1";
const OBJECT_MANAGER_IFACE: &str = "org.freedesktop.DBus.ObjectManager";
const MM_MODEM_IFACE: &str = "org.freedesktop.ModemManager1.Modem";
const MM_MODEM_3GPP_IFACE: &str = "org.freedesktop.ModemManager1.Modem.Modem3gpp";
const PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";

/// Debounce interval for mobile info refreshes triggered by ModemManager signals.
const MOBILE_REFRESH_DEBOUNCE_MS: u64 = 75;

/// Create a synchronous D-Bus proxy on the system bus.
///
/// All sync proxy call sites in this module use identical flags
/// (`BusType::System`, `DBusProxyFlags::NONE`, no interface info, no cancellable).
/// Only the service name, object path, and interface vary.
fn system_dbus_proxy_sync(
    service: &str,
    path: &str,
    iface: &str,
) -> Result<gio::DBusProxy, glib::Error> {
    gio::DBusProxy::for_bus_sync(
        gio::BusType::System,
        gio::DBusProxyFlags::NONE,
        None::<&gio::DBusInterfaceInfo>,
        service,
        path,
        iface,
        None::<&gio::Cancellable>,
    )
}

/// Debug mock file path. See [`debug_mobile_mock`] module docs for usage.
#[cfg(debug_assertions)]
const DEBUG_MOBILE_MOCK_FILE: &str = "/tmp/vibepanel-debug-mobile";

const MM_ACCESS_TECH_GSM: u32 = 1 << 1;
const MM_ACCESS_TECH_GSM_COMPACT: u32 = 1 << 2;
const MM_ACCESS_TECH_GPRS: u32 = 1 << 3;
const MM_ACCESS_TECH_EDGE: u32 = 1 << 4;
const MM_ACCESS_TECH_UMTS: u32 = 1 << 5;
const MM_ACCESS_TECH_HSDPA: u32 = 1 << 6;
const MM_ACCESS_TECH_HSUPA: u32 = 1 << 7;
const MM_ACCESS_TECH_HSPA_PLUS: u32 = 1 << 8;
const MM_ACCESS_TECH_LTE: u32 = 1 << 14;
const MM_ACCESS_TECH_NR5G: u32 = 1 << 15;
const MM_ACCESS_TECH_LTE_CAT_M: u32 = 1 << 19;
const MM_ACCESS_TECH_LTE_NB_IOT: u32 = 1 << 20;

// ── Data types ───────────────────────────────────────────────────────

/// Wi-Fi networking state from NetworkManager.
#[derive(Debug, Clone, Default)]
pub struct WifiState {
    pub enabled: Option<bool>,
    pub connected: bool,
    /// Whether the system has a Wi-Fi device.
    pub has_device: bool,
    pub ssid: Option<String>,
    /// Signal strength (0-100).
    pub strength: i32,
    pub scanning: bool,
    /// Whether the first scan has completed.
    pub is_ready: bool,
    pub networks: Vec<WifiNetwork>,
    /// SSID currently being connected to (for loading state).
    pub connecting_ssid: Option<String>,
    /// SSID that failed to connect.
    pub failed_ssid: Option<String>,
    /// NM Device `State` property (e.g., 40=PREPARE … 90=SECONDARIES → connecting).
    pub device_state: Option<u32>,
}

impl WifiState {
    fn unknown() -> Self {
        Self::default()
    }
}

/// Wired (Ethernet) networking state from NetworkManager.
#[derive(Debug, Clone, Default)]
pub struct WiredState {
    pub connected: bool,
    /// Whether the system has an Ethernet device (drives "Network" vs "Wi-Fi" card title).
    pub has_device: bool,
    /// Interface name (e.g., "enp3s0").
    pub iface: Option<String>,
    /// Connection name (e.g., "Wired connection 1").
    pub name: Option<String>,
    /// Link speed in Mb/s (e.g., 1000 for gigabit).
    pub speed: Option<u32>,
}

impl WiredState {
    fn unknown() -> Self {
        Self::default()
    }
}

/// Mobile/cellular networking state from NetworkManager and ModemManager.
#[derive(Debug, Clone, Default)]
pub struct MobileState {
    /// Whether mobile is the primary connection route.
    pub is_primary: bool,
    /// Whether a GSM/CDMA connection is activated (regardless of primary route).
    pub active: bool,
    pub connecting: bool,
    /// Modem exists + SIM present + at least one GSM/CDMA profile exists.
    pub supported: bool,
    /// Whether WWAN is enabled in NetworkManager.
    pub enabled: Option<bool>,
    /// Whether the system has a modem device (drives "Network" vs "Wi-Fi" card title).
    pub has_device: bool,
    /// Connection profile name.
    pub name: Option<String>,
    /// Operator name from ModemManager (e.g., "MyCarrier").
    pub operator: Option<String>,
    /// Access technology label (e.g., "LTE", "5G").
    pub access_technology: Option<String>,
    /// Signal quality (0-100).
    pub signal_quality: Option<u32>,
    /// Set on nmcli failure, auto-cleared after 5s by UI or on next successful connection.
    pub failed: bool,
}

impl MobileState {
    fn unknown() -> Self {
        Self::default()
    }
}

/// Canonical snapshot of network state.
#[derive(Debug, Clone)]
pub struct NmSnapshot {
    pub available: bool,
    pub wifi: WifiState,
    pub wired: WiredState,
    pub mobile: MobileState,
    pub(crate) primary_connection_type: Option<String>,
}

impl NmSnapshot {
    fn unknown() -> Self {
        Self {
            available: false,
            wifi: WifiState::unknown(),
            wired: WiredState::unknown(),
            mobile: MobileState::unknown(),
            primary_connection_type: None,
        }
    }
}

/// Messages sent from background threads to the main thread.
#[derive(Debug)]
enum NmUpdate {
    WifiDeviceFound {
        path: String,
        iface_name: Option<String>,
    },
    EthernetDeviceExists,
    ModemDeviceExists,
    DeviceDiscoveryFailed,
    ApDetails {
        ssid: Option<String>,
        strength: i32,
    },
    ApDetailsFailed,
    NetworksRefreshed {
        networks: Vec<WifiNetwork>,
        last_scan: Option<i64>,
    },
    RefreshNetworks,
    ConnectionAttemptFinished {
        ssid: String,
        success: bool,
    },
    WiredDeviceInfo {
        iface_name: Option<String>,
        conn_name: Option<String>,
        /// Link speed in Mb/s.
        speed: Option<u32>,
    },
    MobileDeviceInfo {
        conn_name: Option<String>,
        operator_name: Option<String>,
        access_technology: Option<String>,
        signal_quality: Option<u32>,
        active: bool,
        connecting: bool,
        supported: bool,
        has_modem: bool,
    },
    /// Sent after nmcli connect/disconnect returns. Clears the local connecting
    /// intent flag so the next MobileDeviceInfo uses the real D-Bus state.
    MobileConnectionAttemptFinished {
        success: bool,
    },
    /// Sent after toggling WwanEnabled via D-Bus. Semantically the same as
    /// `MobileConnectionAttemptFinished` but makes the call site's intent
    /// explicit.
    MobileToggleFinished {
        success: bool,
    },
    #[cfg(debug_assertions)]
    MobileEnabled(bool),
}

// ── NmService internal mobile state ──────────────────────────────────

/// Internal mobile bookkeeping fields, grouped to keep [`NmService`] focused.
pub(super) struct MobileInternal {
    /// D-Bus signal subscriptions kept alive for the lifetime of the service.
    pub(super) signal_subscriptions: RefCell<Vec<gio::SignalSubscription>>,
    pub(super) refresh_pending: Cell<bool>,
    /// Set synchronously in connect/enable, cleared when real D-Bus state arrives.
    pub(super) connecting_local: Cell<bool>,
}

impl MobileInternal {
    fn new() -> Self {
        Self {
            signal_subscriptions: RefCell::new(Vec::new()),
            refresh_pending: Cell::new(false),
            connecting_local: Cell::new(false),
        }
    }
}

// ── NmService internal Wi-Fi state ──────────────────────────────────

/// Internal Wi-Fi bookkeeping (not exposed in snapshots).
pub(super) struct WifiInternal {
    pub(super) proxy: RefCell<Option<gio::DBusProxy>>,
    /// Proxy for the base `org.freedesktop.NetworkManager.Device` interface,
    /// used to monitor the `State` property for connecting states (40-90).
    pub(super) device_proxy: RefCell<Option<gio::DBusProxy>>,
    pub(super) iface_name: RefCell<Option<String>>,
    pub(super) scan_in_progress: Cell<bool>,
    pub(super) last_scan_value: Cell<Option<i64>>,
    pub(super) known_ssids: Arc<Mutex<HashSet<String>>>,
    pub(super) known_ssids_last_refresh: Arc<Mutex<Option<Instant>>>,
    pub(super) connecting_ssid: RefCell<Option<String>>,
    pub(super) failed_ssid: RefCell<Option<String>>,
}

impl WifiInternal {
    fn new() -> Self {
        Self {
            proxy: RefCell::new(None),
            device_proxy: RefCell::new(None),
            iface_name: RefCell::new(None),
            scan_in_progress: Cell::new(false),
            last_scan_value: Cell::new(None),
            known_ssids: Arc::new(Mutex::new(HashSet::new())),
            known_ssids_last_refresh: Arc::new(Mutex::new(None)),
            connecting_ssid: RefCell::new(None),
            failed_ssid: RefCell::new(None),
        }
    }
}

// ── NmService ────────────────────────────────────────────────────────

/// Shared, process-wide network service for Wi-Fi, Ethernet, and mobile state and control.
pub struct NmService {
    pub(super) nm_proxy: RefCell<Option<gio::DBusProxy>>,
    snapshot: RefCell<NmSnapshot>,
    callbacks: Callbacks<NmSnapshot>,
    pub(super) wifi: WifiInternal,
    pub(super) mobile: MobileInternal,
}

impl NmService {
    fn new() -> Rc<Self> {
        let service = Rc::new(Self {
            nm_proxy: RefCell::new(None),
            snapshot: RefCell::new(NmSnapshot::unknown()),
            callbacks: Callbacks::new(),
            wifi: WifiInternal::new(),
            mobile: MobileInternal::new(),
        });

        // Initialize D-Bus — NM property signals deliver updates without polling.
        Self::init_dbus(&service);

        // In debug builds, start polling the mock file if it exists.
        #[cfg(debug_assertions)]
        if debug_mobile_mock::is_enabled() {
            // Send initial mock state immediately.
            if let Some(mock) = debug_mobile_mock::read_state() {
                debug_mobile_mock::send_mock_updates(&mock);
            }
            debug_mobile_mock::start_polling();
        }

        service
    }

    /// Get the global NmService singleton.
    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<NmService> = NmService::new();
        }

        INSTANCE.with(|s| s.clone())
    }

    /// Register a callback to be invoked whenever the network state changes.
    pub fn connect<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&NmSnapshot) + 'static,
    {
        let id = self.callbacks.register(callback);

        // Immediately send current snapshot to the new callback only.
        let snapshot = self.snapshot.borrow().clone();
        self.callbacks.notify_single(id, &snapshot);
        id
    }

    pub fn unsubscribe(&self, id: CallbackId) {
        self.callbacks.unregister(id);
    }

    pub fn snapshot(&self) -> NmSnapshot {
        self.snapshot.borrow().clone()
    }

    /// Re-emit the current snapshot to all callbacks without any state change.
    ///
    /// Used when external factors (e.g., icon theme switch) require callbacks
    /// to re-evaluate their rendering logic with unchanged network state.
    pub fn re_notify(&self) {
        let snapshot = self.snapshot.borrow().clone();
        self.callbacks.notify(&snapshot);
    }

    /// Mutate the snapshot and unconditionally notify all callbacks.
    pub(super) fn notify_snapshot(&self, f: impl FnOnce(&mut NmSnapshot)) {
        let mut snapshot = self.snapshot.borrow_mut();
        f(&mut snapshot);
        let clone = snapshot.clone();
        drop(snapshot);
        self.callbacks.notify(&clone);
    }

    /// Mutate the snapshot and notify callbacks only if the closure returns `true`.
    pub(super) fn notify_snapshot_if(&self, f: impl FnOnce(&mut NmSnapshot) -> bool) {
        let mut snapshot = self.snapshot.borrow_mut();
        if f(&mut snapshot) {
            let clone = snapshot.clone();
            drop(snapshot);
            self.callbacks.notify(&clone);
        }
    }

    // ── Update Handling ──────────────────────────────────────────────

    fn apply_update(&self, update: NmUpdate) {
        match update {
            NmUpdate::WifiDeviceFound { path, iface_name } => {
                *self.wifi.iface_name.borrow_mut() = iface_name;
                self.notify_snapshot_if(|s| {
                    let changed = !s.wifi.has_device;
                    s.wifi.has_device = true;
                    changed
                });
                self.create_wifi_proxy_from_self(&path);
            }
            NmUpdate::EthernetDeviceExists => {
                self.notify_snapshot_if(|s| {
                    let changed = !s.wired.has_device;
                    s.wired.has_device = true;
                    changed
                });
            }
            NmUpdate::ModemDeviceExists => {
                let is_new = !self.snapshot.borrow().mobile.has_device;
                if is_new {
                    self.notify_snapshot(|s| s.mobile.has_device = true);
                    Self::fetch_mobile_device_info();
                }
            }
            NmUpdate::DeviceDiscoveryFailed => {
                self.set_unavailable();
            }
            NmUpdate::ApDetails { ssid, strength } => {
                self.notify_snapshot(|s| {
                    s.wifi.connected = true;
                    s.wifi.ssid = ssid;
                    s.wifi.strength = strength;
                });
                self.refresh_networks_async();
            }
            NmUpdate::ApDetailsFailed => {
                self.set_disconnected();
            }
            NmUpdate::NetworksRefreshed {
                networks,
                last_scan,
            } => {
                let prev_last_scan = self.wifi.last_scan_value.get();
                if let Some(ls) = last_scan {
                    self.wifi.last_scan_value.set(Some(ls));
                }

                // Clear scan flag if we got newer results (or first results).
                if self.wifi.scan_in_progress.get() {
                    let got_fresh_results = match (last_scan, prev_last_scan) {
                        (Some(new), Some(old)) => new > old,
                        (Some(_), None) => true,
                        _ => false,
                    };
                    if last_scan.is_none() || got_fresh_results {
                        self.wifi.scan_in_progress.set(false);
                    }
                }

                // Don't clear connecting_ssid here — NM may briefly show active during auth.
                // Wait for ConnectionAttemptFinished.

                let scanning = self.wifi.scan_in_progress.get();
                let connecting_ssid = self.wifi.connecting_ssid.borrow().clone();
                let failed_ssid = self.wifi.failed_ssid.borrow().clone();
                self.notify_snapshot(|s| {
                    s.wifi.networks = networks;
                    s.wifi.is_ready = true;
                    s.wifi.scanning = scanning;
                    s.wifi.connecting_ssid = connecting_ssid;
                    s.wifi.failed_ssid = failed_ssid;
                });
            }
            NmUpdate::RefreshNetworks => {
                self.refresh_networks_async();
            }
            NmUpdate::ConnectionAttemptFinished { ssid, success } => {
                *self.wifi.connecting_ssid.borrow_mut() = None;

                if success {
                    *self.wifi.failed_ssid.borrow_mut() = None;
                } else {
                    *self.wifi.failed_ssid.borrow_mut() = Some(ssid);
                    // Invalidate known SSIDs cache so failed network doesn't show "Saved".
                    *self
                        .wifi
                        .known_ssids_last_refresh
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = None;
                }

                let failed_ssid = self.wifi.failed_ssid.borrow().clone();
                self.notify_snapshot(|s| {
                    s.wifi.connecting_ssid = None;
                    s.wifi.failed_ssid = failed_ssid;
                });

                self.refresh_networks_async();
            }
            NmUpdate::WiredDeviceInfo {
                iface_name,
                conn_name,
                speed,
            } => {
                self.notify_snapshot_if(|s| {
                    let changed = s.wired.iface != iface_name
                        || s.wired.name != conn_name
                        || s.wired.speed != speed;
                    if changed {
                        s.wired.iface = iface_name;
                        s.wired.name = conn_name;
                        s.wired.speed = speed;
                    }
                    changed
                });
            }
            NmUpdate::MobileDeviceInfo {
                conn_name,
                operator_name,
                access_technology,
                signal_quality,
                active,
                connecting,
                supported,
                has_modem,
            } => {
                // Merge local "connecting" intent with D-Bus state.
                //
                // `connecting_local` is set synchronously so the UI shows
                // "Connecting…" before D-Bus signals arrive. When NM confirms
                // active/connecting, the local flag is redundant and cleared.
                // If NM shows neither, keep the flag until the next update.
                // `MobileConnectionAttemptFinished` / `MobileToggleFinished`
                // clears it unconditionally as a safety net.
                let (effective_connecting, clear_local) = mobile::resolve_mobile_connecting(
                    self.mobile.connecting_local.get(),
                    active,
                    connecting,
                );
                if clear_local {
                    self.mobile.connecting_local.set(false);
                }

                self.notify_snapshot_if(|s| {
                    let changed = s.mobile.name != conn_name
                        || s.mobile.operator != operator_name
                        || s.mobile.access_technology != access_technology
                        || s.mobile.signal_quality != signal_quality
                        || s.mobile.active != active
                        || s.mobile.connecting != effective_connecting
                        || s.mobile.supported != supported
                        || s.mobile.has_device != has_modem;
                    if changed {
                        s.mobile.name = conn_name;
                        s.mobile.operator = operator_name;
                        s.mobile.access_technology = access_technology;
                        s.mobile.signal_quality = signal_quality;
                        s.mobile.active = active;
                        s.mobile.connecting = effective_connecting;
                        s.mobile.supported = supported;
                        s.mobile.has_device = has_modem;
                    }
                    changed
                });
            }
            NmUpdate::MobileConnectionAttemptFinished { success }
            | NmUpdate::MobileToggleFinished { success } => {
                // Clear local connecting intent; the next MobileDeviceInfo
                // will use real D-Bus state.
                self.mobile.connecting_local.set(false);

                if !success {
                    // Re-read the actual WwanEnabled property from NM so
                    // the optimistic `mobile.enabled` set in
                    // `set_mobile_enabled()` is reverted to the real value
                    // (the D-Bus Set call failed, so no PropertiesChanged
                    // signal will fire to correct it automatically).
                    self.update_nm_flags();
                    self.notify_snapshot(|s| {
                        s.mobile.failed = true;
                        s.mobile.connecting = false;
                    });
                } else {
                    self.notify_snapshot_if(|s| {
                        let changed = s.mobile.failed || s.mobile.connecting;
                        s.mobile.failed = false;
                        s.mobile.connecting = false;
                        changed
                    });
                }
            }
            #[cfg(debug_assertions)]
            NmUpdate::MobileEnabled(enabled) => {
                self.notify_snapshot_if(|s| {
                    let new_val = Some(enabled);
                    let changed = s.mobile.enabled != new_val;
                    s.mobile.enabled = new_val;
                    changed
                });
            }
        }
    }

    // ── D-Bus Initialization ─────────────────────────────────────────

    fn init_dbus(this: &Rc<Self>) {
        let this_weak = Rc::downgrade(this);

        gio::bus_get(
            gio::BusType::System,
            None::<&gio::Cancellable>,
            move |res| {
                let this = match this_weak.upgrade() {
                    Some(this) => this,
                    None => return,
                };

                let connection = match res {
                    Ok(c) => c,
                    Err(e) => {
                        error!("Failed to get system bus: {}", e);
                        return;
                    }
                };

                // Subscribe to ModemManager D-Bus signals at the bus level
                // (wildcard object paths) because modems can appear/disappear
                // at runtime and MM is a separate service from NM.
                let sub_props = connection.subscribe_to_signal(
                    Some(MM_SERVICE),
                    Some(PROPERTIES_IFACE),
                    Some("PropertiesChanged"),
                    None, // any object path (any modem)
                    None,
                    gio::DBusSignalFlags::NONE,
                    {
                        let this_weak = Rc::downgrade(&this);
                        move |signal| {
                            if let Some(iface_name) = signal.parameters.child_value(0).str()
                                && (iface_name == MM_MODEM_IFACE
                                    || iface_name == MM_MODEM_3GPP_IFACE)
                                && let Some(this) = this_weak.upgrade()
                            {
                                this.queue_mobile_refresh();
                            }
                        }
                    },
                );

                let sub_added = connection.subscribe_to_signal(
                    Some(MM_SERVICE),
                    Some(OBJECT_MANAGER_IFACE),
                    Some("InterfacesAdded"),
                    Some(MM_PATH),
                    None,
                    gio::DBusSignalFlags::NONE,
                    {
                        let this_weak = Rc::downgrade(&this);
                        move |_signal| {
                            if let Some(this) = this_weak.upgrade() {
                                this.queue_mobile_refresh();
                            }
                        }
                    },
                );

                let sub_removed = connection.subscribe_to_signal(
                    Some(MM_SERVICE),
                    Some(OBJECT_MANAGER_IFACE),
                    Some("InterfacesRemoved"),
                    Some(MM_PATH),
                    None,
                    gio::DBusSignalFlags::NONE,
                    {
                        let this_weak = Rc::downgrade(&this);
                        move |_signal| {
                            if let Some(this) = this_weak.upgrade() {
                                this.queue_mobile_refresh();
                            }
                        }
                    },
                );

                // Refresh mobile state after resume from suspend/hibernate.
                let sub_sleep = connection.subscribe_to_signal(
                    Some("org.freedesktop.login1"),
                    Some("org.freedesktop.login1.Manager"),
                    Some("PrepareForSleep"),
                    Some("/org/freedesktop/login1"),
                    None,
                    gio::DBusSignalFlags::NONE,
                    {
                        let this_weak = Rc::downgrade(&this);
                        move |signal| {
                            // Only refresh on resume (preparing=false), not on suspend.
                            if let Some(preparing) = signal.parameters.child_value(0).get::<bool>()
                                && !preparing
                                && let Some(this) = this_weak.upgrade()
                            {
                                this.queue_mobile_refresh();
                            }
                        }
                    },
                );

                this.mobile.signal_subscriptions.borrow_mut().extend([
                    sub_props,
                    sub_added,
                    sub_removed,
                    sub_sleep,
                ]);

                // Create NetworkManager main proxy
                let this_weak = Rc::downgrade(&this);
                gio::DBusProxy::new(
                    &connection,
                    gio::DBusProxyFlags::NONE,
                    None::<&gio::DBusInterfaceInfo>,
                    Some(NM_SERVICE),
                    NM_PATH,
                    NM_IFACE,
                    None::<&gio::Cancellable>,
                    move |res| {
                        let this = match this_weak.upgrade() {
                            Some(this) => this,
                            None => return,
                        };

                        let proxy = match res {
                            Ok(p) => p,
                            Err(e) => {
                                error!("Failed to create NetworkManager proxy: {}", e);
                                return;
                            }
                        };

                        this.nm_proxy.replace(Some(proxy.clone()));

                        let this_weak = Rc::downgrade(&this);
                        proxy.connect_local("g-properties-changed", false, move |_| {
                            if let Some(this) = this_weak.upgrade() {
                                this.update_nm_flags();
                            }
                            None
                        });

                        // Monitor for device added (e.g., USB ethernet adapter plugged in)
                        proxy.connect_local("g-signal", false, move |values| {
                            let signal_name = values
                                .get(2)
                                .and_then(|v| v.get::<&str>().ok())
                                .unwrap_or("");
                            if signal_name == "DeviceAdded"
                                && let Some(params) =
                                    values.get(3).and_then(|v| v.get::<Variant>().ok())
                                && let Some(device_path) = objpath_to_string(&params.child_value(0))
                            {
                                Self::check_device_type_for_network_devices(&device_path);
                            }
                            None
                        });

                        // Monitor for service appearing/disappearing (e.g., NM restart).
                        let this_weak = Rc::downgrade(&this);
                        proxy.connect_local("notify::g-name-owner", false, move |values| {
                            let this = this_weak.upgrade()?;
                            let proxy = values[0].get::<gio::DBusProxy>().ok();
                            let has_owner = proxy.as_ref().and_then(|p| p.name_owner()).is_some();
                            if has_owner {
                                // Service reappeared - restore proxy and rediscover Wi-Fi device.
                                if let Some(p) = proxy {
                                    this.nm_proxy.replace(Some(p));
                                }
                                this.set_available(true);
                                this.update_nm_flags();
                                Self::discover_network_devices();
                            } else {
                                // Service disappeared - mark unavailable.
                                this.set_unavailable();
                            }
                            None
                        });

                        // Mark as available now that we have a proxy.
                        this.set_available(true);
                        this.update_nm_flags();

                        Self::discover_network_devices();
                    },
                );
            },
        );
    }

    fn set_available(&self, available: bool) {
        self.notify_snapshot_if(|s| {
            let changed = s.available != available;
            s.available = available;
            changed
        });
    }

    fn set_unavailable(&self) {
        if !self.snapshot.borrow().available {
            return; // Already unavailable
        }
        self.notify_snapshot(|s| *s = NmSnapshot::unknown());
        self.nm_proxy.replace(None);
        self.wifi.proxy.replace(None);
        self.wifi.device_proxy.replace(None);
    }

    // ── Shared Device Discovery ──────────────────────────────────────

    fn discover_network_devices() {
        // Synchronous D-Bus calls — spawn a thread to avoid blocking the main loop.
        thread::spawn(move || {
            let device_paths = match Self::get_device_paths_sync() {
                Ok(paths) => paths,
                Err(e) => {
                    tracing::warn!("Failed to get device paths: {}", e);
                    send_nm_update(NmUpdate::DeviceDiscoveryFailed);
                    return;
                }
            };

            let mut wifi_path: Option<String> = None;
            let mut iface_name: Option<String> = None;
            let mut has_ethernet = false;
            let mut has_modem = false;

            for path in device_paths {
                match Self::get_device_type_sync(&path) {
                    Ok((dtype, iface)) => {
                        if dtype == WIFI_DEVICE_TYPE && wifi_path.is_none() {
                            wifi_path = Some(path);
                            iface_name = iface;
                        } else if dtype == ETHERNET_DEVICE_TYPE {
                            has_ethernet = true;
                        } else if dtype == MODEM_DEVICE_TYPE {
                            has_modem = true;
                        }
                    }
                    Err(e) => {
                        debug!("Failed to get device type for {}: {}", path, e);
                    }
                }
            }

            // In debug builds, treat modem as present when mock file exists.
            #[cfg(debug_assertions)]
            if !has_modem && debug_mobile_mock::is_enabled() {
                has_modem = true;
            }

            if has_ethernet {
                send_nm_update(NmUpdate::EthernetDeviceExists);
            }
            if has_modem {
                send_nm_update(NmUpdate::ModemDeviceExists);
            }

            let Some(path) = wifi_path else {
                tracing::warn!("No Wi-Fi device found");
                return;
            };

            debug!("Found Wi-Fi device: {} (iface: {:?})", path, iface_name);

            send_nm_update(NmUpdate::WifiDeviceFound { path, iface_name });
        });
    }

    fn get_device_paths_sync() -> Result<Vec<String>, String> {
        let proxy = system_dbus_proxy_sync(NM_SERVICE, NM_PATH, NM_IFACE)
            .map_err(|e| format!("Failed to create NM proxy: {}", e))?;

        let result = proxy
            .call_sync(
                "GetDevices",
                None,
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
            )
            .map_err(|e| format!("GetDevices failed: {}", e))?;

        // Result is (ao,) — array of object paths in a tuple
        let paths: Vec<String> = result
            .child_value(0)
            .iter()
            .filter_map(|v| objpath_to_string(&v))
            .collect();

        Ok(paths)
    }

    fn get_device_type_sync(path: &str) -> Result<(u32, Option<String>), String> {
        let proxy = system_dbus_proxy_sync(NM_SERVICE, path, IFACE_DEV)
            .map_err(|e| format!("Failed to create device proxy: {}", e))?;

        let dtype = proxy
            .cached_property("DeviceType")
            .and_then(|v| v.get::<u32>())
            .ok_or_else(|| "No DeviceType property".to_string())?;

        let iface = proxy
            .cached_property("Interface")
            .and_then(|v| v.get::<String>());

        Ok((dtype, iface))
    }

    /// Check if a newly added device is a network adapter we track.
    fn check_device_type_for_network_devices(device_path: &str) {
        let path = device_path.to_string();
        thread::spawn(move || match Self::get_device_type_sync(&path) {
            Ok((dtype, _)) if dtype == ETHERNET_DEVICE_TYPE => {
                debug!("New ethernet device detected: {}", path);
                send_nm_update(NmUpdate::EthernetDeviceExists);
            }
            Ok((dtype, _)) if dtype == MODEM_DEVICE_TYPE => {
                debug!("New modem device detected: {}", path);
                send_nm_update(NmUpdate::ModemDeviceExists);
            }
            _ => {}
        });
    }

    // ── NM Flags (cross-cutting state) ───────────────────────────────

    fn update_nm_flags(&self) {
        let Some(nm) = self.nm_proxy.borrow().clone() else {
            return;
        };

        let wifi_enabled = nm
            .cached_property("WirelessEnabled")
            .and_then(|v| v.get::<bool>());
        #[allow(unused_mut)]
        let mut mobile_enabled = nm
            .cached_property("WwanEnabled")
            .and_then(|v| v.get::<bool>());

        // In debug builds, override mobile_enabled from mock state.
        #[cfg(debug_assertions)]
        if debug_mobile_mock::is_enabled()
            && let Some(mock) = debug_mobile_mock::read_state()
        {
            mobile_enabled = Some(mock.state.is_enabled());
        }

        let primary_connection_type = nm
            .cached_property("PrimaryConnectionType")
            .and_then(|v| v.get::<String>());

        let wired_connected = wired::is_wired_connected(primary_connection_type.as_deref());
        let mobile_connected = mobile::is_mobile_connected(primary_connection_type.as_deref());

        let mut snapshot = self.snapshot.borrow_mut();
        let mut changed = false;
        let mut wifi_reenabled = false;
        if snapshot.wifi.enabled != wifi_enabled {
            // Detect WiFi being re-enabled so we can trigger a network refresh.
            wifi_reenabled = snapshot.wifi.enabled == Some(false) && wifi_enabled == Some(true);

            snapshot.wifi.enabled = wifi_enabled;
            changed = true;

            // When WiFi is disabled, clear connection state and reset scan
            // readiness so the spinner shows during the next re-enable cycle
            // (matching IWD's clear_station() behavior).
            if wifi_enabled == Some(false) {
                snapshot.wifi.connected = false;
                snapshot.wifi.ssid = None;
                snapshot.wifi.strength = 0;
                snapshot.wifi.is_ready = false;
                snapshot.wifi.scanning = false;
                snapshot.wifi.device_state = None;
                for net in &mut snapshot.wifi.networks {
                    net.active = false;
                }
            }
        }

        if snapshot.mobile.enabled != mobile_enabled {
            snapshot.mobile.enabled = mobile_enabled;
            changed = true;
        }

        if snapshot.primary_connection_type != primary_connection_type {
            snapshot.primary_connection_type = primary_connection_type;
            changed = true;
        }

        let wired_changed = snapshot.wired.connected != wired_connected;
        if wired_changed {
            snapshot.wired.connected = wired_connected;
            changed = true;

            // Clear wired info when disconnecting
            if !wired_connected {
                snapshot.wired.iface = None;
                snapshot.wired.name = None;
                snapshot.wired.speed = None;
            }
        }

        let mobile_changed = snapshot.mobile.is_primary != mobile_connected;
        if mobile_changed {
            snapshot.mobile.is_primary = mobile_connected;
            changed = true;
        }

        if changed {
            let snapshot_clone = snapshot.clone();
            drop(snapshot);
            self.callbacks.notify(&snapshot_clone);

            // Fetch wired device info in background when newly connected
            if wired_changed && wired_connected {
                Self::fetch_wired_device_info();
            }
            if mobile_changed {
                Self::fetch_mobile_device_info();
            }
            // When WiFi is re-enabled, trigger a network refresh so
            // is_ready eventually becomes true (ending the spinner).
            // Without this, if NM doesn't auto-connect, is_ready stays
            // false because nothing triggers refresh_networks_async.
            if wifi_reenabled {
                self.refresh_networks_async();
            }
        } else {
            drop(snapshot);
        }
    }
}

/// Send an update from a background thread to the main GLib loop.
///
/// Thread-safe: marshals to the main loop via `glib::idle_add_once`.
fn send_nm_update(update: NmUpdate) {
    glib::idle_add_once(move || {
        NmService::global().apply_update(update);
    });
}

#[cfg(debug_assertions)]
pub(super) mod debug_mobile_mock;
