//! NmService — Network state via NetworkManager over D-Bus.
//!
//! Uses Gio's async D-Bus proxy; background threads deliver updates via `glib::idle_add_once()`.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use gtk4::gio::{self, prelude::*};
use gtk4::glib::{self, Variant, VariantTy};
use tracing::{debug, error, warn};

use crate::services::callbacks::{CallbackId, Callbacks};
use crate::services::network::{SecurityType, WifiNetwork, objpath_to_string};

// D-Bus Constants

/// NetworkManager service name.
pub const NM_SERVICE: &str = "org.freedesktop.NetworkManager";
/// NetworkManager main object path.
pub const NM_PATH: &str = "/org/freedesktop/NetworkManager";
/// NetworkManager main interface.
pub const NM_IFACE: &str = "org.freedesktop.NetworkManager";
/// Device interface for type detection.
const IFACE_DEV: &str = "org.freedesktop.NetworkManager.Device";
/// Wireless device interface.
const IFACE_WIFI: &str = "org.freedesktop.NetworkManager.Device.Wireless";
/// Wired/Ethernet device interface.
const IFACE_WIRED: &str = "org.freedesktop.NetworkManager.Device.Wired";
/// Access point interface.
const IFACE_AP: &str = "org.freedesktop.NetworkManager.AccessPoint";
/// Active connection interface (for connection name/Id).
const IFACE_ACTIVE_CONN: &str = "org.freedesktop.NetworkManager.Connection.Active";
/// NetworkManager settings path.
const NM_SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
/// NetworkManager settings interface.
const IFACE_SETTINGS: &str = "org.freedesktop.NetworkManager.Settings";
/// NetworkManager settings connection interface.
const IFACE_SETTINGS_CONN: &str = "org.freedesktop.NetworkManager.Settings.Connection";

/// NetworkManager device type for Ethernet (NM_DEVICE_TYPE_ETHERNET = 1).
const ETHERNET_DEVICE_TYPE: u32 = 1;
/// NetworkManager device type for Wi-Fi (NM_DEVICE_TYPE_WIFI = 2).
const WIFI_DEVICE_TYPE: u32 = 2;
/// NetworkManager device type for modem/cellular (NM_DEVICE_TYPE_MODEM = 8).
const MODEM_DEVICE_TYPE: u32 = 8;

/// ModemManager service name.
const MM_SERVICE: &str = "org.freedesktop.ModemManager1";
/// ModemManager manager path.
const MM_PATH: &str = "/org/freedesktop/ModemManager1";
/// Object manager interface.
const OBJECT_MANAGER_IFACE: &str = "org.freedesktop.DBus.ObjectManager";
/// Modem interface.
const MM_MODEM_IFACE: &str = "org.freedesktop.ModemManager1.Modem";
/// 3GPP modem interface.
const MM_MODEM_3GPP_IFACE: &str = "org.freedesktop.ModemManager1.Modem.Modem3gpp";
const PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";

/// Debounce interval for mobile info refreshes triggered by ModemManager signals.
const MOBILE_REFRESH_DEBOUNCE_MS: u64 = 75;

/// Create a synchronous D-Bus proxy on the system bus.
///
/// All 13 sync proxy call sites in this module use identical flags
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

/// Modem information gathered from ModemManager (SIM, signal, technology, operator).
struct MobileInfo {
    /// Whether a modem object was found in ModemManager.
    has_modem: bool,
    has_sim: bool,
    signal_quality: Option<u32>,
    access_technology: Option<String>,
    operator_name: Option<String>,
}

/// Mobile connection status from NetworkManager (active connections & profiles).
struct MobileNmStatus {
    active: bool,
    connecting: bool,
    /// The name of the first GSM/CDMA connection profile, if one exists.
    /// Use `.is_some()` to check whether a mobile profile is configured.
    profile_name: Option<String>,
    active_name: Option<String>,
}

/// Wi-Fi networking state from NetworkManager.
///
/// Groups all Wi-Fi–related fields that were previously spread across
/// `NmSnapshot` to improve readability and make subsystems easier
/// to reason about in isolation.
#[derive(Debug, Clone, Default)]
pub struct WifiState {
    /// Whether Wi-Fi hardware is enabled.
    pub enabled: Option<bool>,
    /// Whether connected to a Wi-Fi network.
    pub connected: bool,
    /// Whether the system has a Wi-Fi device.
    /// Used to determine whether to enable the Wi-Fi toggle.
    pub has_device: bool,
    /// Current SSID if connected.
    pub ssid: Option<String>,
    /// Current signal strength if connected (0-100).
    pub strength: i32,
    /// Whether a scan is in progress.
    pub scanning: bool,
    /// Whether the service is ready (first scan complete).
    pub is_ready: bool,
    /// List of visible networks.
    pub networks: Vec<WifiNetwork>,
    /// SSID currently being connected to (for loading state).
    pub connecting_ssid: Option<String>,
    /// SSID that failed to connect (for re-showing password prompt).
    pub failed_ssid: Option<String>,
}

impl WifiState {
    fn unknown() -> Self {
        Self::default()
    }
}

/// Wired (Ethernet) networking state from NetworkManager.
///
/// Groups all Ethernet-related fields for the same reasons as [`WifiState`]
/// and [`MobileState`].
#[derive(Debug, Clone, Default)]
pub struct WiredState {
    /// Whether a wired (Ethernet) connection is active as the primary link.
    pub connected: bool,
    /// Whether the system has an Ethernet device (regardless of connection state).
    /// Used to determine whether to show "Network" or "Wi-Fi" as the card title.
    pub has_device: bool,
    /// Wired interface name (e.g., "enp3s0") when connected via Ethernet.
    pub iface: Option<String>,
    /// Wired connection name from NetworkManager (e.g., "Wired connection 1").
    pub name: Option<String>,
    /// Wired link speed in Mb/s (e.g., 1000 for gigabit) when connected via Ethernet.
    pub speed: Option<u32>,
}

impl WiredState {
    fn unknown() -> Self {
        Self::default()
    }
}

/// Mobile/cellular networking state from NetworkManager and ModemManager.
///
/// Groups all modem-related fields for the same reasons as [`WifiState`]
/// and [`WiredState`].
#[derive(Debug, Clone, Default)]
pub struct MobileState {
    /// Whether a mobile/cellular connection is active as the primary link.
    pub is_primary: bool,
    /// Whether a mobile/cellular connection is active (regardless of primary route).
    pub active: bool,
    /// Whether a mobile/cellular connection is currently activating (connecting).
    pub connecting: bool,
    /// Whether mobile is supported for display in UI:
    /// modem exists + SIM present + at least one GSM/CDMA connection profile exists.
    pub supported: bool,
    /// Whether WWAN/modem is enabled in NetworkManager.
    pub enabled: Option<bool>,
    /// Whether the system has a modem/cellular device (regardless of connection state).
    /// Used to determine whether to show "Network" or "Wi-Fi" as the card title.
    pub has_device: bool,
    /// Mobile connection name from NetworkManager (e.g., carrier profile name).
    pub name: Option<String>,
    /// Mobile operator name from ModemManager (e.g., "T-Mobile").
    pub operator: Option<String>,
    /// Mobile access technology label (e.g., "LTE", "5G").
    pub access_technology: Option<String>,
    /// Mobile signal quality (0-100).
    pub signal_quality: Option<u32>,
    /// Whether the last mobile connection attempt failed.
    /// Set when nmcli exits with non-zero status, cleared on next successful
    /// connection or when explicitly cleared by the UI.
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
    /// Whether the NetworkManager service is available.
    pub available: bool,
    /// Wi-Fi networking state (connection, scanning, networks, etc.).
    pub wifi: WifiState,
    /// Wired (Ethernet) networking state (connection, interface, speed).
    pub wired: WiredState,
    /// Mobile/cellular networking state (modem, connection, signal, etc.).
    pub mobile: MobileState,
    /// NetworkManager primary connection type (e.g., "802-11-wireless", "802-3-ethernet").
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
    /// Wi-Fi device discovered - path and interface name.
    WifiDeviceFound {
        path: String,
        iface_name: Option<String>,
    },
    /// Ethernet device exists on this system (detected during device discovery).
    EthernetDeviceExists,
    /// Modem/cellular device exists on this system (detected during device discovery).
    ModemDeviceExists,
    /// Device discovery failed - service is unavailable.
    DeviceDiscoveryFailed,
    /// Active access point details.
    ApDetails { ssid: Option<String>, strength: i32 },
    /// Failed to get AP details - set disconnected.
    ApDetailsFailed,
    /// Network list refresh complete.
    NetworksRefreshed {
        networks: Vec<WifiNetwork>,
        last_scan: Option<i64>,
    },
    /// Request a network list refresh (from main thread context).
    RefreshNetworks,
    /// Connection attempt finished (success or failure).
    ConnectionAttemptFinished {
        /// The SSID that was attempted.
        ssid: String,
        /// Whether the connection succeeded.
        success: bool,
    },
    /// Wired device info fetched.
    WiredDeviceInfo {
        /// Interface name (e.g., "enp3s0").
        iface_name: Option<String>,
        /// Connection name from NetworkManager (e.g., "Wired connection 1").
        conn_name: Option<String>,
        /// Link speed in Mb/s (e.g., 1000 for gigabit).
        speed: Option<u32>,
    },
    /// Mobile device info fetched.
    MobileDeviceInfo {
        /// Connection name from NetworkManager (e.g., profile name).
        conn_name: Option<String>,
        /// Operator name from ModemManager.
        operator_name: Option<String>,
        /// Access technology label (e.g., LTE, 5G).
        access_technology: Option<String>,
        /// Signal quality percentage (0-100).
        signal_quality: Option<u32>,
        /// Whether a GSM/CDMA connection is currently active.
        active: bool,
        /// Whether a GSM/CDMA connection is currently activating (connecting).
        connecting: bool,
        /// Whether the system supports mobile usage in UI
        /// (modem + SIM + GSM/CDMA profile).
        supported: bool,
        /// Whether a modem device is currently present (for hot-unplug detection).
        has_modem: bool,
    },
    /// Mobile connection attempt finished (nmcli returned).
    /// Clears the local connecting intent flag so the next MobileDeviceInfo
    /// uses the real D-Bus state.
    MobileConnectionAttemptFinished {
        /// Whether the connection attempt succeeded.
        success: bool,
    },
    /// Override the mobile_enabled (WWAN) flag (used by debug mock).
    #[cfg(debug_assertions)]
    MobileEnabled(bool),
}

/// Shared, process-wide network service for Wi-Fi state and control.
pub struct NmService {
    /// NetworkManager main proxy.
    nm_proxy: RefCell<Option<gio::DBusProxy>>,
    /// Wi-Fi device proxy.
    wifi_proxy: RefCell<Option<gio::DBusProxy>>,
    /// Wi-Fi interface name (e.g., "wlan0").
    iface_name: RefCell<Option<String>>,
    /// Current snapshot of network state.
    snapshot: RefCell<NmSnapshot>,
    /// Registered callbacks for state changes.
    callbacks: Callbacks<NmSnapshot>,
    /// Whether a scan is in progress.
    scan_in_progress: Cell<bool>,
    /// Last scan timestamp from NetworkManager.
    last_scan_value: Cell<Option<i64>>,
    /// Cache of known SSIDs (saved connections).
    known_ssids: Arc<Mutex<HashSet<String>>>,
    /// When the known SSIDs cache was last refreshed.
    known_ssids_last_refresh: Arc<Mutex<Option<Instant>>>,
    /// SSID currently being connected to (cleared on success/failure).
    connecting_ssid: RefCell<Option<String>>,
    /// SSID that failed to connect (for re-showing password prompt).
    failed_ssid: RefCell<Option<String>>,
    /// ModemManager signal subscriptions (kept alive for the service lifetime).
    _signal_subscriptions: RefCell<Vec<gio::SignalSubscription>>,
    /// Debounce guard for mobile refresh requests.
    mobile_refresh_pending: Cell<bool>,
    /// Whether a mobile connection attempt is in progress (for instant UI feedback).
    /// Set synchronously in connect_mobile() / set_mobile_enabled(true), cleared when
    /// MobileDeviceInfo arrives with the real state.
    mobile_connecting_local: Cell<bool>,
}

impl NmService {
    /// Create a new NmService.
    fn new() -> Rc<Self> {
        let service = Rc::new(Self {
            nm_proxy: RefCell::new(None),
            wifi_proxy: RefCell::new(None),
            iface_name: RefCell::new(None),
            snapshot: RefCell::new(NmSnapshot::unknown()),
            callbacks: Callbacks::new(),
            scan_in_progress: Cell::new(false),
            last_scan_value: Cell::new(None),
            known_ssids: Arc::new(Mutex::new(HashSet::new())),
            known_ssids_last_refresh: Arc::new(Mutex::new(None)),
            connecting_ssid: RefCell::new(None),
            failed_ssid: RefCell::new(None),
            _signal_subscriptions: RefCell::new(Vec::new()),
            mobile_refresh_pending: Cell::new(false),
            mobile_connecting_local: Cell::new(false),
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

    /// Mutate the snapshot and unconditionally notify all callbacks.
    fn notify_snapshot(&self, f: impl FnOnce(&mut NmSnapshot)) {
        let mut snapshot = self.snapshot.borrow_mut();
        f(&mut snapshot);
        let clone = snapshot.clone();
        drop(snapshot);
        self.callbacks.notify(&clone);
    }

    /// Mutate the snapshot and notify callbacks only if the closure returns `true`.
    ///
    /// The closure should apply its changes and return whether anything actually
    /// changed. If it returns `false`, callbacks are not invoked.
    fn notify_snapshot_if(&self, f: impl FnOnce(&mut NmSnapshot) -> bool) {
        let mut snapshot = self.snapshot.borrow_mut();
        if f(&mut snapshot) {
            let clone = snapshot.clone();
            drop(snapshot);
            self.callbacks.notify(&clone);
        }
    }

    // Update Handling

    fn apply_update(&self, update: NmUpdate) {
        match update {
            NmUpdate::WifiDeviceFound { path, iface_name } => {
                *self.iface_name.borrow_mut() = iface_name;
                self.notify_snapshot_if(|s| {
                    let changed = !s.wifi.has_device;
                    s.wifi.has_device = true;
                    changed
                });
                Self::create_wifi_proxy_from_self(self, &path);
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
                // Device discovery failed - mark service as unavailable
                self.set_unavailable();
            }
            NmUpdate::ApDetails { ssid, strength } => {
                self.notify_snapshot(|s| {
                    s.wifi.connected = true;
                    s.wifi.ssid = ssid;
                    s.wifi.strength = strength;
                });
                // Also trigger a network list refresh.
                self.refresh_networks_async();
            }
            NmUpdate::ApDetailsFailed => {
                self.set_disconnected();
            }
            NmUpdate::NetworksRefreshed {
                networks,
                last_scan,
            } => {
                let prev_last_scan = self.last_scan_value.get();
                if let Some(ls) = last_scan {
                    self.last_scan_value.set(Some(ls));
                }

                // Clear scan flag if we got newer results (or first results).
                if self.scan_in_progress.get() {
                    let got_fresh_results = match (last_scan, prev_last_scan) {
                        (Some(new), Some(old)) => new > old,
                        (Some(_), None) => true,
                        _ => false,
                    };
                    if last_scan.is_none() || got_fresh_results {
                        self.scan_in_progress.set(false);
                    }
                }

                // Don't clear connecting_ssid here — NM may briefly show active during auth.
                // Wait for ConnectionAttemptFinished.

                let scanning = self.scan_in_progress.get();
                let connecting_ssid = self.connecting_ssid.borrow().clone();
                let failed_ssid = self.failed_ssid.borrow().clone();
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
                // Clear connecting state.
                *self.connecting_ssid.borrow_mut() = None;

                // If connection failed, set failed_ssid so UI can re-show password prompt.
                // If succeeded, clear any previous failed_ssid.
                if success {
                    *self.failed_ssid.borrow_mut() = None;
                } else {
                    *self.failed_ssid.borrow_mut() = Some(ssid);
                    // Invalidate known SSIDs cache so failed network doesn't show "Saved".
                    *self
                        .known_ssids_last_refresh
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = None;
                }

                let failed_ssid = self.failed_ssid.borrow().clone();
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
                // Clear debounce guard so future MM signals can trigger another refresh.
                self.mobile_refresh_pending.set(false);

                // Merge local "connecting" intent with D-Bus state.
                //
                // Race resolution strategy:
                // `mobile_connecting_local` is set synchronously on the main thread
                // in `connect_mobile()` / `set_mobile_enabled(true)` so the UI shows
                // a "Connecting…" state immediately, before NM/MM D-Bus signals
                // arrive (which may take hundreds of milliseconds).
                //
                // When a `MobileDeviceInfo` update arrives from D-Bus we reconcile:
                //   • NM confirms `active` or `connecting` → local flag is redundant,
                //     clear it and trust the real D-Bus state from here on.
                //   • NM shows neither active nor connecting → the D-Bus signal
                //     arrived before NM reflected the attempt; keep the local flag so
                //     the UI continues showing "Connecting…" until the next update.
                //
                // The flag is also cleared unconditionally via the
                // `MobileConnectionAttemptFinished` update sent after `nmcli` returns,
                // which acts as a safety net if the state machine gets stuck.
                let (effective_connecting, clear_local) = resolve_mobile_connecting(
                    self.mobile_connecting_local.get(),
                    active,
                    connecting,
                );
                if clear_local {
                    self.mobile_connecting_local.set(false);
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
            NmUpdate::MobileConnectionAttemptFinished { success } => {
                // nmcli returned — clear the local connecting intent flag.
                // The next MobileDeviceInfo (triggered right after this) will
                // use the real D-Bus state.
                self.mobile_connecting_local.set(false);

                if !success {
                    self.notify_snapshot(|s| {
                        s.mobile.failed = true;
                        s.mobile.connecting = false;
                    });
                } else {
                    // Clear any stale failed state from a previous attempt
                    // and stop showing the connecting spinner.
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

    // D-Bus Initialization

    fn init_dbus(this: &Rc<Self>) {
        let this_weak = Rc::downgrade(this);

        // First, get the system bus
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

                // Subscribe to ModemManager D-Bus signals for responsive modem
                // state updates (signal quality, registration, operator, etc.).
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

                this._signal_subscriptions.borrow_mut().extend([
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

                        // Track WirelessEnabled property changes
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
                                // Check if the new device is a network adapter we care about
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
                                Self::discover_wifi_device();
                            } else {
                                // Service disappeared - mark unavailable.
                                this.set_unavailable();
                            }
                            None
                        });

                        // Mark as available now that we have a proxy.
                        this.set_available(true);
                        this.update_nm_flags();

                        // Discover Wi-Fi device in background thread
                        Self::discover_wifi_device();
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

        // Clear proxies.
        self.nm_proxy.replace(None);
        self.wifi_proxy.replace(None);
    }

    fn discover_wifi_device() {
        // We need to do synchronous D-Bus calls to find the Wi-Fi device,
        // so we spawn a thread to avoid blocking the main loop.
        thread::spawn(move || {
            // Get device paths from NetworkManager
            let device_paths = match Self::get_device_paths_sync() {
                Ok(paths) => paths,
                Err(e) => {
                    warn!("Failed to get device paths: {}", e);
                    send_nm_update(NmUpdate::DeviceDiscoveryFailed);
                    return;
                }
            };

            // Find Wi-Fi device and check for Ethernet/Modem devices
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

            // Notify if ethernet device exists (for adaptive card title)
            if has_ethernet {
                send_nm_update(NmUpdate::EthernetDeviceExists);
            }
            if has_modem {
                send_nm_update(NmUpdate::ModemDeviceExists);
            }

            let Some(path) = wifi_path else {
                warn!("No Wi-Fi device found");
                return;
            };

            debug!("Found Wi-Fi device: {} (iface: {:?})", path, iface_name);

            // Send update to main thread.
            send_nm_update(NmUpdate::WifiDeviceFound { path, iface_name });
        });
    }

    fn get_device_paths_sync() -> Result<Vec<String>, String> {
        // Create a sync proxy to NetworkManager
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

        // Result is (ao,) - array of object paths in a tuple
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

    /// Check if a newly added device is an ethernet adapter and notify if so.
    /// Called when NetworkManager emits DeviceAdded signal.
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

    /// Get wired device info (interface name and speed) synchronously.
    fn get_wired_device_info_sync(path: &str) -> Result<(String, u32), String> {
        // Get interface name from Device interface
        let dev_proxy = system_dbus_proxy_sync(NM_SERVICE, path, IFACE_DEV)
            .map_err(|e| format!("Failed to create device proxy: {}", e))?;

        let iface_name = dev_proxy
            .cached_property("Interface")
            .and_then(|v| v.get::<String>())
            .ok_or_else(|| "No Interface property".to_string())?;

        // Get speed from Wired interface
        let wired_proxy = system_dbus_proxy_sync(NM_SERVICE, path, IFACE_WIRED)
            .map_err(|e| format!("Failed to create wired proxy: {}", e))?;

        let speed = wired_proxy
            .cached_property("Speed")
            .and_then(|v| v.get::<u32>())
            .unwrap_or(0);

        Ok((iface_name, speed))
    }

    /// Get the primary connection name (Id) from NetworkManager.
    /// Returns None if no primary connection or on error.
    fn get_primary_connection_name_sync() -> Option<String> {
        // Get NM proxy to read PrimaryConnection path
        let nm_proxy = system_dbus_proxy_sync(NM_SERVICE, NM_PATH, NM_IFACE).ok()?;

        // Get PrimaryConnection object path
        let primary_conn_path = nm_proxy
            .cached_property("PrimaryConnection")
            .and_then(|v| v.get::<glib::variant::ObjectPath>())?;

        let path_str = primary_conn_path.as_str();
        if path_str == "/" {
            return None; // No primary connection
        }

        // Get the connection name (Id) from the ActiveConnection
        let conn_proxy = system_dbus_proxy_sync(NM_SERVICE, path_str, IFACE_ACTIVE_CONN).ok()?;

        conn_proxy
            .cached_property("Id")
            .and_then(|v| v.get::<String>())
    }

    /// Discover wired device and fetch its info in a background thread.
    fn fetch_wired_device_info() {
        thread::spawn(move || {
            // In debug builds, return mock data if the debug file exists
            #[cfg(debug_assertions)]
            if std::path::Path::new("/tmp/vibepanel-debug-wired").exists() {
                debug!("Using mock wired device info (debug mode)");
                // Also send EthernetDeviceExists so card shows "Network" title
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

            // Find first Ethernet device
            for path in device_paths {
                match Self::get_device_type_sync(&path) {
                    Ok((dtype, _)) if dtype == ETHERNET_DEVICE_TYPE => {
                        match Self::get_wired_device_info_sync(&path) {
                            Ok((iface_name, speed)) => {
                                // Also get the connection name from the primary connection
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

            // No wired device found
            send_nm_update(NmUpdate::WiredDeviceInfo {
                iface_name: None,
                conn_name: None,
                speed: None,
            });
        });
    }

    /// Discover mobile info in a background thread.
    fn fetch_mobile_device_info() {
        // In debug builds, return mock data if the debug mock is enabled.
        #[cfg(debug_assertions)]
        if debug_mobile_mock::is_enabled()
            && let Some(mock) = debug_mobile_mock::read_state()
        {
            debug_mobile_mock::send_mock_updates(&mock);
            return;
        }

        thread::spawn(move || {
            let nm_status = Self::get_mobile_nm_status_sync().unwrap_or(MobileNmStatus {
                active: false,
                connecting: false,
                profile_name: None,
                active_name: None,
            });

            let mm_info = Self::get_mobile_info_from_mm_sync().unwrap_or(MobileInfo {
                has_modem: false,
                has_sim: false,
                signal_quality: None,
                access_technology: None,
                operator_name: None,
            });

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
    fn queue_mobile_refresh(&self) {
        if self.mobile_refresh_pending.get() {
            return; // Already pending — coalesce.
        }
        self.mobile_refresh_pending.set(true);

        glib::timeout_add_local_once(Duration::from_millis(MOBILE_REFRESH_DEBOUNCE_MS), || {
            Self::fetch_mobile_device_info();
        });
    }

    /// Return mobile status from NetworkManager:
    /// (mobile_active, has_mobile_profile, active_mobile_connection_name)
    fn get_mobile_nm_status_sync() -> Result<MobileNmStatus, String> {
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
    fn find_first_mobile_profile_sync() -> Result<Option<String>, String> {
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
        if let Some((quality, _recent)) = v.get::<(u32, bool)>() {
            return quality;
        }

        if v.n_children() > 0 {
            return v.child_value(0).get::<u32>().unwrap_or(0);
        }

        0
    }

    /// Create wifi proxy - called from apply_update on main thread.
    fn create_wifi_proxy_from_self(&self, path: &str) {
        // Get a strong Rc to self for the callback.
        let this = NmService::global();
        Self::create_wifi_proxy(&this, path);
    }

    fn create_wifi_proxy(this: &Rc<Self>, path: &str) {
        let this_weak = Rc::downgrade(this);
        let path = path.to_string();

        // Get connection from NM proxy
        let Some(nm_proxy) = this.nm_proxy.borrow().clone() else {
            return;
        };

        let connection = nm_proxy.connection();

        gio::DBusProxy::new(
            &connection,
            gio::DBusProxyFlags::NONE,
            None::<&gio::DBusInterfaceInfo>,
            Some(NM_SERVICE),
            &path,
            IFACE_WIFI,
            None::<&gio::Cancellable>,
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

                this.wifi_proxy.replace(Some(proxy.clone()));

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
            },
        );
    }

    // State Updates

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

        let wired_connected = is_wired_connected(primary_connection_type.as_deref());
        let mobile_connected = is_mobile_connected(primary_connection_type.as_deref());

        let mut snapshot = self.snapshot.borrow_mut();
        let mut changed = false;
        if snapshot.wifi.enabled != wifi_enabled {
            snapshot.wifi.enabled = wifi_enabled;
            changed = true;

            // When WiFi is disabled, clear connection state and mark all networks as inactive
            if wifi_enabled == Some(false) {
                snapshot.wifi.connected = false;
                snapshot.wifi.ssid = None;
                snapshot.wifi.strength = 0;
                // Mark all networks as not active (they can't be connected if WiFi is off)
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
        } else {
            drop(snapshot);
        }
    }

    fn update_state(&self) {
        let Some(wifi) = self.wifi_proxy.borrow().clone() else {
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

    fn set_disconnected(&self) {
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

    fn refresh_networks_async(&self) {
        let Some(wifi) = self.wifi_proxy.borrow().clone() else {
            return;
        };

        let known_ssids = Arc::clone(&self.known_ssids);
        let known_ssids_refresh = Arc::clone(&self.known_ssids_last_refresh);

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
        use std::collections::HashMap;

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

    // Public API: Actions

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

    /// Enable or disable WWAN/modem via NetworkManager.
    pub fn set_mobile_enabled(&self, enabled: bool) {
        // In debug builds, update mock state with realistic delays.
        #[cfg(debug_assertions)]
        if debug_mobile_mock::is_enabled() {
            if enabled {
                // Set connecting state for instant UI feedback (same as production path).
                self.mobile_connecting_local.set(true);
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
                self.mobile_connecting_local.set(false);
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
            self.mobile_connecting_local.set(true);
            self.notify_snapshot(|s| {
                s.mobile.connecting = true;
                s.mobile.enabled = Some(true);
            });
        } else {
            // Disabling — clear connecting state immediately.
            self.mobile_connecting_local.set(false);
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
        self.mobile_connecting_local.set(true);
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
        self.mobile_connecting_local.set(false);
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

    /// Request a Wi-Fi scan.
    pub fn scan_networks(&self) {
        if self.scan_in_progress.get() {
            return;
        }

        let Some(wifi) = self.wifi_proxy.borrow().clone() else {
            return;
        };

        self.scan_in_progress.set(true);

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
        *self.failed_ssid.borrow_mut() = None;
        self.notify_snapshot(|s| {
            s.wifi.failed_ssid = None;
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
        *self.failed_ssid.borrow_mut() = None;
        *self.connecting_ssid.borrow_mut() = Some(ssid.clone());
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
        let iface = self.iface_name.borrow().clone();
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

        let known_ssids_refresh = Arc::clone(&self.known_ssids_last_refresh);

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

/// Send an update from a background thread to the main GLib loop.
///
/// # Thread safety
/// Safe to call from any thread — `glib::idle_add_once` marshals the
/// closure to the main loop.
fn send_nm_update(update: NmUpdate) {
    glib::idle_add_once(move || {
        NmService::global().apply_update(update);
    });
}

/// Check if a wired (Ethernet) connection is active.
///
/// In debug builds, this can be overridden by creating `/tmp/vibepanel-debug-wired`
/// for testing without physical hardware. Toggle at runtime with:
/// - Enable: `touch /tmp/vibepanel-debug-wired`
/// - Disable: `rm /tmp/vibepanel-debug-wired`
fn is_wired_connected(primary_type: Option<&str>) -> bool {
    #[cfg(debug_assertions)]
    if std::path::Path::new("/tmp/vibepanel-debug-wired").exists() {
        return true;
    }

    primary_type.is_some_and(|t| t == "802-3-ethernet")
}

/// Check if a mobile/cellular connection is active.
fn is_mobile_connected(primary_type: Option<&str>) -> bool {
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
fn resolve_mobile_connecting(
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
fn access_technology_label(bits: u32) -> &'static str {
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

#[cfg(debug_assertions)]
mod debug_mobile_mock;

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
