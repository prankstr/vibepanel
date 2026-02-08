//! NetworkService — Wi-Fi state via NetworkManager over D-Bus.
//!
//! Uses Gio's async D-Bus proxy; background threads deliver updates via `glib::idle_add_once()`.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use gtk4::gio::{self, prelude::*};
use gtk4::glib::{self, Variant, VariantTy};
use tracing::{debug, error, warn};

use crate::services::callbacks::{CallbackId, Callbacks};
use crate::services::wifi::{SecurityType, WifiNetwork, objpath_to_string};

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

/// NetworkManager device type for Ethernet (NM_DEVICE_TYPE_ETHERNET = 1).
const ETHERNET_DEVICE_TYPE: u32 = 1;
/// NetworkManager device type for Wi-Fi (NM_DEVICE_TYPE_WIFI = 2).
const WIFI_DEVICE_TYPE: u32 = 2;

/// Canonical snapshot of Wi-Fi state.
#[derive(Debug, Clone)]
pub struct NetworkSnapshot {
    /// Whether the NetworkManager service is available.
    pub available: bool,
    /// Whether Wi-Fi hardware is enabled.
    pub wifi_enabled: Option<bool>,
    /// Whether connected to a Wi-Fi network.
    pub connected: bool,
    /// Whether a non-Wi-Fi (e.g., Ethernet) connection is active as the primary link.
    pub wired_connected: bool,
    /// Whether the system has a Wi-Fi device.
    /// Used to determine whether to enable the Wi-Fi toggle.
    pub has_wifi_device: bool,
    /// Whether the system has an Ethernet device (regardless of connection state).
    /// Used to determine whether to show "Network" or "Wi-Fi" as the card title.
    pub has_ethernet_device: bool,
    /// NetworkManager primary connection type (e.g., "802-11-wireless", "802-3-ethernet").
    pub primary_connection_type: Option<String>,
    /// Wired interface name (e.g., "enp3s0") when connected via Ethernet.
    pub wired_iface: Option<String>,
    /// Wired connection name from NetworkManager (e.g., "Wired connection 1").
    pub wired_name: Option<String>,
    /// Wired link speed in Mb/s (e.g., 1000 for gigabit) when connected via Ethernet.
    pub wired_speed: Option<u32>,
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

impl NetworkSnapshot {
    fn unknown() -> Self {
        Self {
            available: false,
            wifi_enabled: None,
            connected: false,
            wired_connected: false,
            has_wifi_device: false,
            has_ethernet_device: false,
            primary_connection_type: None,
            wired_iface: None,
            wired_name: None,
            wired_speed: None,
            ssid: None,
            strength: 0,
            scanning: false,
            is_ready: false,
            networks: Vec::new(),
            connecting_ssid: None,
            failed_ssid: None,
        }
    }
}

/// Messages sent from background threads to the main thread.
#[derive(Debug)]
enum NetworkUpdate {
    /// Wi-Fi device discovered - path and interface name.
    WifiDeviceFound {
        path: String,
        iface_name: Option<String>,
    },
    /// Ethernet device exists on this system (detected during device discovery).
    EthernetDeviceExists,
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
}

/// Shared, process-wide network service for Wi-Fi state and control.
pub struct NetworkService {
    /// NetworkManager main proxy.
    nm_proxy: RefCell<Option<gio::DBusProxy>>,
    /// Wi-Fi device proxy.
    wifi_proxy: RefCell<Option<gio::DBusProxy>>,
    /// Wi-Fi interface name (e.g., "wlan0").
    iface_name: RefCell<Option<String>>,
    /// Current snapshot of network state.
    snapshot: RefCell<NetworkSnapshot>,
    /// Registered callbacks for state changes.
    callbacks: Callbacks<NetworkSnapshot>,
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
}

impl NetworkService {
    /// Create a new NetworkService.
    fn new() -> Rc<Self> {
        let service = Rc::new(Self {
            nm_proxy: RefCell::new(None),
            wifi_proxy: RefCell::new(None),
            iface_name: RefCell::new(None),
            snapshot: RefCell::new(NetworkSnapshot::unknown()),
            callbacks: Callbacks::new(),
            scan_in_progress: Cell::new(false),
            last_scan_value: Cell::new(None),
            known_ssids: Arc::new(Mutex::new(HashSet::new())),
            known_ssids_last_refresh: Arc::new(Mutex::new(None)),
            connecting_ssid: RefCell::new(None),
            failed_ssid: RefCell::new(None),
        });

        // Initialize D-Bus — NM property signals deliver updates without polling.
        Self::init_dbus(&service);

        service
    }

    /// Get the global NetworkService singleton.
    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<NetworkService> = NetworkService::new();
        }

        INSTANCE.with(|s| s.clone())
    }

    /// Register a callback to be invoked whenever the network state changes.
    pub fn connect<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&NetworkSnapshot) + 'static,
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

    pub fn snapshot(&self) -> NetworkSnapshot {
        self.snapshot.borrow().clone()
    }

    // Update Handling

    fn apply_update(&self, update: NetworkUpdate) {
        match update {
            NetworkUpdate::WifiDeviceFound { path, iface_name } => {
                *self.iface_name.borrow_mut() = iface_name;
                // Mark that we have a Wi-Fi device
                {
                    let mut snapshot = self.snapshot.borrow_mut();
                    if !snapshot.has_wifi_device {
                        snapshot.has_wifi_device = true;
                        let snapshot_clone = snapshot.clone();
                        drop(snapshot);
                        self.callbacks.notify(&snapshot_clone);
                    }
                }
                Self::create_wifi_proxy_from_self(self, &path);
            }
            NetworkUpdate::EthernetDeviceExists => {
                let mut snapshot = self.snapshot.borrow_mut();
                if !snapshot.has_ethernet_device {
                    snapshot.has_ethernet_device = true;
                    let snapshot_clone = snapshot.clone();
                    drop(snapshot);
                    self.callbacks.notify(&snapshot_clone);
                }
            }
            NetworkUpdate::DeviceDiscoveryFailed => {
                // Device discovery failed - mark service as unavailable
                self.set_unavailable();
            }
            NetworkUpdate::ApDetails { ssid, strength } => {
                let mut snapshot = self.snapshot.borrow_mut();
                snapshot.connected = true;
                snapshot.ssid = ssid;
                snapshot.strength = strength;
                let snapshot_clone = snapshot.clone();
                drop(snapshot);
                self.callbacks.notify(&snapshot_clone);
                // Also trigger a network list refresh.
                self.refresh_networks_async();
            }
            NetworkUpdate::ApDetailsFailed => {
                self.set_disconnected();
            }
            NetworkUpdate::NetworksRefreshed {
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

                let mut snapshot = self.snapshot.borrow_mut();
                snapshot.networks = networks;
                snapshot.is_ready = true;
                snapshot.scanning = self.scan_in_progress.get();
                snapshot.connecting_ssid = self.connecting_ssid.borrow().clone();
                snapshot.failed_ssid = self.failed_ssid.borrow().clone();
                let snapshot_clone = snapshot.clone();
                drop(snapshot);
                self.callbacks.notify(&snapshot_clone);
            }
            NetworkUpdate::RefreshNetworks => {
                self.refresh_networks_async();
            }
            NetworkUpdate::ConnectionAttemptFinished { ssid, success } => {
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

                let mut snapshot = self.snapshot.borrow_mut();
                snapshot.connecting_ssid = None;
                snapshot.failed_ssid = self.failed_ssid.borrow().clone();
                let snapshot_clone = snapshot.clone();
                drop(snapshot);
                self.callbacks.notify(&snapshot_clone);

                self.refresh_networks_async();
            }
            NetworkUpdate::WiredDeviceInfo {
                iface_name,
                conn_name,
                speed,
            } => {
                let mut snapshot = self.snapshot.borrow_mut();
                let changed = snapshot.wired_iface != iface_name
                    || snapshot.wired_name != conn_name
                    || snapshot.wired_speed != speed;
                if changed {
                    snapshot.wired_iface = iface_name;
                    snapshot.wired_name = conn_name;
                    snapshot.wired_speed = speed;
                    let snapshot_clone = snapshot.clone();
                    drop(snapshot);
                    self.callbacks.notify(&snapshot_clone);
                }
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
                                // Check if the new device is an ethernet adapter
                                Self::check_device_type_for_ethernet(&device_path);
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
        let mut snapshot = self.snapshot.borrow_mut();
        if snapshot.available != available {
            snapshot.available = available;
            let snapshot_clone = snapshot.clone();
            drop(snapshot);
            self.callbacks.notify(&snapshot_clone);
        }
    }

    fn set_unavailable(&self) {
        let mut snapshot = self.snapshot.borrow_mut();
        if !snapshot.available {
            return; // Already unavailable
        }
        *snapshot = NetworkSnapshot::unknown();
        let snapshot_clone = snapshot.clone();
        drop(snapshot);
        self.callbacks.notify(&snapshot_clone);

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
                    send_network_update(NetworkUpdate::DeviceDiscoveryFailed);
                    return;
                }
            };

            // Find Wi-Fi device and check for Ethernet devices
            let mut wifi_path: Option<String> = None;
            let mut iface_name: Option<String> = None;
            let mut has_ethernet = false;

            for path in device_paths {
                match Self::get_device_type_sync(&path) {
                    Ok((dtype, iface)) => {
                        if dtype == WIFI_DEVICE_TYPE && wifi_path.is_none() {
                            wifi_path = Some(path);
                            iface_name = iface;
                        } else if dtype == ETHERNET_DEVICE_TYPE {
                            has_ethernet = true;
                        }
                    }
                    Err(e) => {
                        debug!("Failed to get device type for {}: {}", path, e);
                    }
                }
            }

            // Notify if ethernet device exists (for adaptive card title)
            if has_ethernet {
                send_network_update(NetworkUpdate::EthernetDeviceExists);
            }

            let Some(path) = wifi_path else {
                warn!("No Wi-Fi device found");
                return;
            };

            debug!("Found Wi-Fi device: {} (iface: {:?})", path, iface_name);

            // Send update to main thread.
            send_network_update(NetworkUpdate::WifiDeviceFound { path, iface_name });
        });
    }

    fn get_device_paths_sync() -> Result<Vec<String>, String> {
        // Create a sync proxy to NetworkManager
        let proxy = gio::DBusProxy::for_bus_sync(
            gio::BusType::System,
            gio::DBusProxyFlags::NONE,
            None::<&gio::DBusInterfaceInfo>,
            NM_SERVICE,
            NM_PATH,
            NM_IFACE,
            None::<&gio::Cancellable>,
        )
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
        let proxy = gio::DBusProxy::for_bus_sync(
            gio::BusType::System,
            gio::DBusProxyFlags::NONE,
            None::<&gio::DBusInterfaceInfo>,
            NM_SERVICE,
            path,
            IFACE_DEV,
            None::<&gio::Cancellable>,
        )
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
    fn check_device_type_for_ethernet(device_path: &str) {
        let path = device_path.to_string();
        thread::spawn(move || match Self::get_device_type_sync(&path) {
            Ok((dtype, _)) if dtype == ETHERNET_DEVICE_TYPE => {
                debug!("New ethernet device detected: {}", path);
                send_network_update(NetworkUpdate::EthernetDeviceExists);
            }
            _ => {}
        });
    }

    /// Get wired device info (interface name and speed) synchronously.
    fn get_wired_device_info_sync(path: &str) -> Result<(String, u32), String> {
        // Get interface name from Device interface
        let dev_proxy = gio::DBusProxy::for_bus_sync(
            gio::BusType::System,
            gio::DBusProxyFlags::NONE,
            None::<&gio::DBusInterfaceInfo>,
            NM_SERVICE,
            path,
            IFACE_DEV,
            None::<&gio::Cancellable>,
        )
        .map_err(|e| format!("Failed to create device proxy: {}", e))?;

        let iface_name = dev_proxy
            .cached_property("Interface")
            .and_then(|v| v.get::<String>())
            .ok_or_else(|| "No Interface property".to_string())?;

        // Get speed from Wired interface
        let wired_proxy = gio::DBusProxy::for_bus_sync(
            gio::BusType::System,
            gio::DBusProxyFlags::NONE,
            None::<&gio::DBusInterfaceInfo>,
            NM_SERVICE,
            path,
            IFACE_WIRED,
            None::<&gio::Cancellable>,
        )
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
        let nm_proxy = gio::DBusProxy::for_bus_sync(
            gio::BusType::System,
            gio::DBusProxyFlags::NONE,
            None::<&gio::DBusInterfaceInfo>,
            NM_SERVICE,
            NM_PATH,
            NM_IFACE,
            None::<&gio::Cancellable>,
        )
        .ok()?;

        // Get PrimaryConnection object path
        let primary_conn_path = nm_proxy
            .cached_property("PrimaryConnection")
            .and_then(|v| v.get::<glib::variant::ObjectPath>())?;

        let path_str = primary_conn_path.as_str();
        if path_str == "/" {
            return None; // No primary connection
        }

        // Get the connection name (Id) from the ActiveConnection
        let conn_proxy = gio::DBusProxy::for_bus_sync(
            gio::BusType::System,
            gio::DBusProxyFlags::NONE,
            None::<&gio::DBusInterfaceInfo>,
            NM_SERVICE,
            path_str,
            IFACE_ACTIVE_CONN,
            None::<&gio::Cancellable>,
        )
        .ok()?;

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
                send_network_update(NetworkUpdate::EthernetDeviceExists);
                send_network_update(NetworkUpdate::WiredDeviceInfo {
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
                    send_network_update(NetworkUpdate::WiredDeviceInfo {
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
                                send_network_update(NetworkUpdate::WiredDeviceInfo {
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
            send_network_update(NetworkUpdate::WiredDeviceInfo {
                iface_name: None,
                conn_name: None,
                speed: None,
            });
        });
    }

    /// Create wifi proxy - called from apply_update on main thread.
    fn create_wifi_proxy_from_self(&self, path: &str) {
        // Get a strong Rc to self for the callback.
        let this = NetworkService::global();
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

        let primary_connection_type = nm
            .cached_property("PrimaryConnectionType")
            .and_then(|v| v.get::<String>());

        let wired_connected = is_wired_connected(primary_connection_type.as_deref());

        let mut snapshot = self.snapshot.borrow_mut();
        let mut changed = false;
        if snapshot.wifi_enabled != wifi_enabled {
            snapshot.wifi_enabled = wifi_enabled;
            changed = true;

            // When WiFi is disabled, clear connection state and mark all networks as inactive
            if wifi_enabled == Some(false) {
                snapshot.connected = false;
                snapshot.ssid = None;
                snapshot.strength = 0;
                // Mark all networks as not active (they can't be connected if WiFi is off)
                for net in &mut snapshot.networks {
                    net.active = false;
                }
            }
        }

        if snapshot.primary_connection_type != primary_connection_type {
            snapshot.primary_connection_type = primary_connection_type;
            changed = true;
        }

        let wired_changed = snapshot.wired_connected != wired_connected;
        if wired_changed {
            snapshot.wired_connected = wired_connected;
            changed = true;

            // Clear wired info when disconnecting
            if !wired_connected {
                snapshot.wired_iface = None;
                snapshot.wired_name = None;
                snapshot.wired_speed = None;
            }
        }

        if changed {
            let snapshot_clone = snapshot.clone();
            drop(snapshot);
            self.callbacks.notify(&snapshot_clone);

            // Fetch wired device info in background when newly connected
            if wired_changed && wired_connected {
                Self::fetch_wired_device_info();
            }
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
                send_network_update(NetworkUpdate::ApDetails { ssid, strength });
            }
            Err(e) => {
                debug!("Failed to get AP details: {}", e);
                send_network_update(NetworkUpdate::ApDetailsFailed);
            }
        });
    }

    fn get_ap_details_sync(path: &str) -> Result<(Option<String>, i32), String> {
        let proxy = gio::DBusProxy::for_bus_sync(
            gio::BusType::System,
            gio::DBusProxyFlags::NONE,
            None::<&gio::DBusInterfaceInfo>,
            NM_SERVICE,
            path,
            IFACE_AP,
            None::<&gio::Cancellable>,
        )
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
        let mut snapshot = self.snapshot.borrow_mut();
        if !snapshot.connected && snapshot.ssid.is_none() && snapshot.strength == 0 {
            return; // Already disconnected
        }
        snapshot.connected = false;
        snapshot.ssid = None;
        snapshot.strength = 0;
        let snapshot_clone = snapshot.clone();
        drop(snapshot);
        self.callbacks.notify(&snapshot_clone);
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
            send_network_update(NetworkUpdate::NetworksRefreshed {
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
        let proxy = gio::DBusProxy::for_bus_sync(
            gio::BusType::System,
            gio::DBusProxyFlags::NONE,
            None::<&gio::DBusInterfaceInfo>,
            NM_SERVICE,
            path,
            IFACE_AP,
            None::<&gio::Cancellable>,
        )
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
        let mut snapshot = self.snapshot.borrow_mut();
        snapshot.scanning = true;
        let snapshot_clone = snapshot.clone();
        drop(snapshot);
        self.callbacks.notify(&snapshot_clone);

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
                send_network_update(NetworkUpdate::RefreshNetworks);
            },
        );
    }

    /// Clear the failed connection state (called when user cancels password dialog).
    pub fn clear_failed_state(&self) {
        *self.failed_ssid.borrow_mut() = None;
        let mut snapshot = self.snapshot.borrow_mut();
        snapshot.failed_ssid = None;
        let snapshot_clone = snapshot.clone();
        drop(snapshot);
        self.callbacks.notify(&snapshot_clone);
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
        let mut snapshot = self.snapshot.borrow_mut();
        snapshot.failed_ssid = None;
        snapshot.connecting_ssid = Some(ssid.clone());
        let snapshot_clone = snapshot.clone();
        drop(snapshot);
        self.callbacks.notify(&snapshot_clone);

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
            send_network_update(NetworkUpdate::ConnectionAttemptFinished { ssid, success });
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
            send_network_update(NetworkUpdate::RefreshNetworks);
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
            send_network_update(NetworkUpdate::RefreshNetworks);
        });
    }
}

/// Send an update from a background thread to the main GLib loop.
///
/// # Thread safety
/// Safe to call from any thread — `glib::idle_add_once` marshals the
/// closure to the main loop.
fn send_network_update(update: NetworkUpdate) {
    glib::idle_add_once(move || {
        NetworkService::global().apply_update(update);
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
