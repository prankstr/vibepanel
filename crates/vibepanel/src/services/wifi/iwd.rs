use crate::services::callbacks::Callbacks;
use gtk4::gio::{self, prelude::*};
use gtk4::glib::{self, Variant};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use tracing::{debug, error, warn};

use std::collections::HashMap;

use super::WifiNetwork;

const IWD_SERVICE: &str = "net.connman.iwd";
const IWD_ROOT_PATH: &str = "/net/connman/iwd";
const IFACE_ADAPTER: &str = "net.connman.iwd.Adapter";
const IFACE_STATION: &str = "net.connman.iwd.Station";
const IFACE_NETWORK: &str = "net.connman.iwd.Network";
const IFACE_KNOWN_NETWORK: &str = "net.connman.iwd.KnownNetwork";
const OBJECT_MANAGER_IFACE: &str = "org.freedesktop.DBus.ObjectManager";
const PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";

const AGENT_IFACE: &str = "net.connman.iwd.Agent";
const AGENT_MANAGER_IFACE: &str = "net.connman.iwd.AgentManager";
const AGENT_PATH: &str = "/org/vibepanel/iwd/agent";

/// Maximum number of SSID resolution attempts via network refresh before
/// giving up. Prevents an infinite loop when the connected network's SSID
/// is never found in the cached network list.
const MAX_SSID_RESOLVE_ATTEMPTS: u8 = 3;

// Station state constants (from net.connman.iwd.Station State property)
const STATE_CONNECTING: &str = "connecting";
const STATE_CONNECTED: &str = "connected";
const STATE_ROAMING: &str = "roaming";

/// IWD Agent interface introspection XML for D-Bus registration.
const AGENT_INTROSPECTION: &str = r#"
<node>
    <interface name="net.connman.iwd.Agent">
        <method name="Release" />
        <method name="RequestPassphrase">
            <arg type="o" name="network" direction="in"/>
            <arg type="s" name="passphrase" direction="out"/>
        </method>
        <method name="RequestPrivateKeyPassphrase">
            <arg type="o" name="network" direction="in"/>
            <arg type="s" name="passphrase" direction="out"/>
        </method>
        <method name="RequestUserNameAndPassword">
            <arg type="o" name="network" direction="in"/>
            <arg type="s" name="user" direction="out"/>
            <arg type="s" name="password" direction="out"/>
        </method>
        <method name="RequestUserPassword">
            <arg type="o" name="network" direction="in"/>
            <arg type="s" name="user" direction="in"/>
            <arg type="s" name="password" direction="out"/>
        </method>
        <method name="Cancel">
            <arg type="s" name="reason" direction="in"/>
        </method>
    </interface>
</node>
"#;

#[derive(Debug)]
enum IwdUpdate {
    AdapterDiscovered {
        path: String,
    },
    StationDiscovered {
        path: String,
    },
    NetworksRefreshed {
        networks: Vec<WifiNetwork>,
    },
    /// Connection failed (e.g., network disappeared, auth failure, DHCP timeout).
    /// Ignored if a more specific failure (from agent Cancel) is already set.
    ConnectionFailed {
        ssid: String,
        /// Human-readable reason for the failure.
        reason: String,
    },
}

/// Authentication request from IWD agent.
#[derive(Debug, Clone)]
pub struct IwdAuthRequest {
    pub ssid: String,
}

/// Cached network properties from GetManagedObjects, keyed by object path.
struct NetworkProps {
    name: String,
    net_type: String,
    connected: bool,
    known_network_path: Option<String>,
}

struct PendingAuth {
    invocation: gio::DBusMethodInvocation,
}

/// Canonical snapshot of Wi-Fi state from IWD.
#[derive(Debug, Clone)]
pub struct IwdSnapshot {
    pub available: bool,
    pub ssid: Option<String>,
    /// IWD station state (STATE_CONNECTING, STATE_CONNECTED, STATE_ROAMING).
    pub state: Option<String>,
    pub wifi_enabled: Option<bool>,
    pub scanning: bool,
    pub networks: Vec<WifiNetwork>,
    pub auth_request: Option<IwdAuthRequest>,
    pub failed_ssid: Option<String>,
    /// Human-readable reason for the last connection failure (e.g., "Wrong password",
    /// "Network not found", "Connection failed").
    pub failed_reason: Option<String>,
    /// Whether initial scan has completed (for is_ready check).
    pub initial_scan_complete: bool,
}

impl IwdSnapshot {
    /// Create an initial "unknown" snapshot.
    fn unknown() -> Self {
        Self {
            available: false,
            ssid: None,
            state: None,
            wifi_enabled: None,
            scanning: false,
            networks: Vec::new(),
            auth_request: None,
            failed_ssid: None,
            failed_reason: None,
            initial_scan_complete: false,
        }
    }

    /// Whether currently connected to a Wi-Fi network.
    pub fn connected(&self) -> bool {
        matches!(self.state.as_deref(), Some(STATE_CONNECTED | STATE_ROAMING))
    }

    /// Whether currently connecting to a Wi-Fi network.
    pub fn connecting(&self) -> bool {
        self.state.as_deref() == Some(STATE_CONNECTING)
    }
}

/// Shared, process-wide IWD service for Wi-Fi state and control.
///
/// This service manages Wi-Fi connectivity via the iNet Wireless Daemon (IWD).
/// It implements the IWD Agent interface for authentication - when connecting
/// to a secured network, IWD calls back to our agent requesting credentials,
/// and the UI prompts the user for a password.
///
/// # Thread Safety
/// All public methods are designed to be called from the GLib main loop.
/// Synchronous D-Bus operations (connection, scanning) spawn background
/// threads and use `glib::idle_add_once()` to deliver results to the main thread.
pub struct IwdService {
    snapshot: RefCell<IwdSnapshot>,
    callbacks: Callbacks<IwdSnapshot>,
    adapter_path: RefCell<Option<String>>,
    adapter_proxy: RefCell<Option<gio::DBusProxy>>,
    connection: RefCell<Option<gio::DBusConnection>>,
    station_proxy: RefCell<Option<gio::DBusProxy>>,
    station_path: RefCell<Option<String>>,
    agent_registration_id: RefCell<Option<gio::RegistrationId>>,
    pending_auth: RefCell<Option<PendingAuth>>,
    scan_in_progress: Cell<bool>,
    /// Counter for SSID resolution attempts when connected but SSID not found
    /// in the cached network list. Capped at MAX_SSID_RESOLVE_ATTEMPTS to
    /// prevent an infinite refresh_networks_async loop.
    ssid_resolve_attempts: Cell<u8>,
    watcher_proxy: RefCell<Option<gio::DBusProxy>>,
    /// D-Bus signal subscriptions (kept alive for the service lifetime).
    _signal_subscriptions: RefCell<Vec<gio::SignalSubscription>>,
}

impl IwdService {
    fn new() -> Rc<Self> {
        let service: Rc<IwdService> = Rc::new(Self {
            snapshot: RefCell::new(IwdSnapshot::unknown()),
            callbacks: Callbacks::new(),
            adapter_path: RefCell::new(None),
            adapter_proxy: RefCell::new(None),
            station_proxy: RefCell::new(None),
            station_path: RefCell::new(None),
            connection: RefCell::new(None),
            agent_registration_id: RefCell::new(None),
            pending_auth: RefCell::new(None),
            scan_in_progress: Cell::new(false),
            ssid_resolve_attempts: Cell::new(0),
            watcher_proxy: RefCell::new(None),
            _signal_subscriptions: RefCell::new(Vec::new()),
        });

        Self::init_dbus(&service);

        service
    }

    /// Get the global iwd service singleton.
    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<IwdService> = IwdService::new();
        }

        INSTANCE.with(|s| s.clone())
    }

    /// Register a callback to be invoked whenever the network state changes.
    pub fn connect<F>(&self, callback: F)
    where
        F: Fn(&IwdSnapshot) + 'static,
    {
        self.callbacks.register(callback);

        // Immediately send current snapshot.
        let snapshot = self.snapshot.borrow().clone();
        self.callbacks.notify(&snapshot);
    }

    pub fn snapshot(&self) -> IwdSnapshot {
        self.snapshot.borrow().clone()
    }

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

                this.connection.replace(Some(connection.clone()));

                // Subscribe to PropertiesChanged from IWD (any object path)
                let this_weak2 = Rc::downgrade(&this);
                let sub1 = connection.subscribe_to_signal(
                    Some(IWD_SERVICE),
                    Some(PROPERTIES_IFACE),
                    Some("PropertiesChanged"),
                    None,
                    None,
                    gio::DBusSignalFlags::NONE,
                    move |signal| {
                        let Some(this) = this_weak2.upgrade() else {
                            return;
                        };
                        // Check which interface changed properties
                        let iface_name: Option<String> = signal.parameters.child_value(0).get();
                        match iface_name.as_deref() {
                            Some(IFACE_ADAPTER) => this.update_adapter_state(),
                            Some(IFACE_STATION) => this.update_station_state(),
                            _ => {}
                        }
                    },
                );

                // Subscribe to InterfacesAdded from IWD ObjectManager
                let this_weak3 = Rc::downgrade(&this);
                let sub2 = connection.subscribe_to_signal(
                    Some(IWD_SERVICE),
                    Some(OBJECT_MANAGER_IFACE),
                    Some("InterfacesAdded"),
                    None,
                    None,
                    gio::DBusSignalFlags::NONE,
                    move |signal| {
                        let Some(this) = this_weak3.upgrade() else {
                            return;
                        };
                        Self::handle_interfaces_added(&this, signal.parameters);
                    },
                );

                // Subscribe to InterfacesRemoved from IWD ObjectManager
                let this_weak4 = Rc::downgrade(&this);
                let sub3 = connection.subscribe_to_signal(
                    Some(IWD_SERVICE),
                    Some(OBJECT_MANAGER_IFACE),
                    Some("InterfacesRemoved"),
                    None,
                    None,
                    gio::DBusSignalFlags::NONE,
                    move |signal| {
                        let Some(this) = this_weak4.upgrade() else {
                            return;
                        };
                        Self::handle_interfaces_removed(&this, signal.parameters);
                    },
                );

                // Store subscriptions to keep them alive
                this._signal_subscriptions
                    .borrow_mut()
                    .extend([sub1, sub2, sub3]);

                // Create a watcher proxy to monitor IWD service name owner changes
                let this_weak5 = Rc::downgrade(&this);
                gio::DBusProxy::new(
                    &connection,
                    gio::DBusProxyFlags::DO_NOT_LOAD_PROPERTIES
                        | gio::DBusProxyFlags::DO_NOT_CONNECT_SIGNALS,
                    None::<&gio::DBusInterfaceInfo>,
                    Some(IWD_SERVICE),
                    IWD_ROOT_PATH,
                    "org.freedesktop.DBus.Peer",
                    None::<&gio::Cancellable>,
                    move |res| {
                        let this = match this_weak5.upgrade() {
                            Some(this) => this,
                            None => return,
                        };

                        let proxy = match res {
                            Ok(p) => p,
                            Err(e) => {
                                debug!("Failed to create IWD watcher proxy: {}", e);
                                // Still try to discover — service might be available
                                Self::discover_from_managed_objects(&this);
                                return;
                            }
                        };

                        // Monitor for IWD service appearing/disappearing
                        let this_weak6 = Rc::downgrade(&this);
                        proxy.connect_notify_local(Some("g-name-owner"), move |proxy, _| {
                            let Some(this) = this_weak6.upgrade() else {
                                return;
                            };

                            let has_owner = proxy.name_owner().is_some();
                            if has_owner {
                                debug!("IWD service appeared, rediscovering devices");
                                Self::discover_from_managed_objects(&this);
                            } else {
                                debug!("IWD service disappeared");
                                this.clear_proxies();
                                this.set_unavailable();
                            }
                        });

                        // Store the watcher proxy to keep it alive
                        this.watcher_proxy.replace(Some(proxy.clone()));

                        // Check if service is currently available
                        if proxy.name_owner().is_some() {
                            Self::discover_from_managed_objects(&this);
                        } else {
                            debug!("IWD service not available at startup");
                            this.set_unavailable();
                        }
                    },
                );
            },
        );
    }

    /// Discover IWD adapter and station via ObjectManager's GetManagedObjects.
    /// Called from main thread (async D-Bus call).
    fn discover_from_managed_objects(this: &Rc<Self>) {
        let Some(connection) = this.connection.borrow().clone() else {
            return;
        };

        let this_weak = Rc::downgrade(this);
        connection.call(
            Some(IWD_SERVICE),
            "/",
            OBJECT_MANAGER_IFACE,
            "GetManagedObjects",
            None,
            None,
            gio::DBusCallFlags::NONE,
            5000,
            None::<&gio::Cancellable>,
            move |res| {
                let Some(this) = this_weak.upgrade() else {
                    return;
                };
                match res {
                    Ok(result) => {
                        Self::process_managed_objects(&this, &result);
                    }
                    Err(e) => {
                        debug!("GetManagedObjects failed: {}", e);
                        this.set_unavailable();
                    }
                }
            },
        );
    }

    /// Parse GetManagedObjects result to find adapter and station paths.
    fn process_managed_objects(this: &Rc<Self>, result: &Variant) {
        let dict = result.child_value(0);
        let n = dict.n_children();

        let mut adapter_path: Option<String> = None;
        let mut station_path: Option<String> = None;

        for i in 0..n {
            let entry = dict.child_value(i);
            let path: Option<String> = entry.child_value(0).get();
            let Some(path) = path else { continue };

            let ifaces = entry.child_value(1);
            let n_ifaces = ifaces.n_children();
            for j in 0..n_ifaces {
                let iface_entry = ifaces.child_value(j);
                let iface_name: Option<String> = iface_entry.child_value(0).get();
                match iface_name.as_deref() {
                    Some(IFACE_ADAPTER) if adapter_path.is_none() => {
                        debug!("Found IWD adapter at: {}", path);
                        adapter_path = Some(path.clone());
                    }
                    Some(IFACE_STATION) if station_path.is_none() => {
                        debug!("Found IWD station at: {}", path);
                        station_path = Some(path.clone());
                    }
                    _ => {}
                }
            }
        }

        if let Some(path) = adapter_path {
            Self::apply_update(this, IwdUpdate::AdapterDiscovered { path });
        }
        if let Some(path) = station_path {
            Self::apply_update(this, IwdUpdate::StationDiscovered { path });
        }

        if this.adapter_path.borrow().is_none() && this.station_path.borrow().is_none() {
            debug!("No IWD adapter or station found in managed objects");
            this.set_unavailable();
        }
    }

    /// Handle InterfacesAdded signal — detect new adapter/station objects.
    fn handle_interfaces_added(this: &Rc<Self>, params: &Variant) {
        // InterfacesAdded(OBJPATH, DICT<STRING,DICT<STRING,VARIANT>>)
        let path: Option<String> = params.child_value(0).get();
        let Some(path) = path else { return };

        let ifaces = params.child_value(1);
        let n_ifaces = ifaces.n_children();
        for j in 0..n_ifaces {
            let iface_entry = ifaces.child_value(j);
            let iface_name: Option<String> = iface_entry.child_value(0).get();
            match iface_name.as_deref() {
                Some(IFACE_ADAPTER) if this.adapter_path.borrow().is_none() => {
                    debug!("IWD adapter added: {}", path);
                    Self::apply_update(this, IwdUpdate::AdapterDiscovered { path: path.clone() });
                }
                Some(IFACE_STATION) if this.station_path.borrow().is_none() => {
                    debug!("IWD station added: {}", path);
                    Self::apply_update(this, IwdUpdate::StationDiscovered { path: path.clone() });
                }
                _ => {}
            }
        }
    }

    /// Handle InterfacesRemoved signal — detect adapter/station removal.
    fn handle_interfaces_removed(this: &Rc<Self>, params: &Variant) {
        // InterfacesRemoved(OBJPATH, ARRAY<STRING>)
        let path: Option<String> = params.child_value(0).get();
        let Some(path) = path else { return };

        let removed_ifaces = params.child_value(1);
        let n = removed_ifaces.n_children();
        let mut lost_adapter = false;
        let mut lost_station = false;

        for i in 0..n {
            let iface_name: Option<String> = removed_ifaces.child_value(i).get();
            match iface_name.as_deref() {
                Some(IFACE_ADAPTER) if this.adapter_path.borrow().as_deref() == Some(&path) => {
                    debug!("IWD adapter removed: {}", path);
                    lost_adapter = true;
                }
                Some(IFACE_STATION) if this.station_path.borrow().as_deref() == Some(&path) => {
                    debug!("IWD station removed: {}", path);
                    lost_station = true;
                }
                _ => {}
            }
        }

        if lost_adapter {
            // Adapter gone — full service loss.
            this.clear_proxies();
            this.set_unavailable();
        } else if lost_station {
            // Station removed but adapter still present — WiFi was powered off.
            // Only clear station-related state; keep adapter proxy so we can
            // re-enable WiFi without restarting the bar.
            this.clear_station();
            this.update_adapter_state();
        }
    }

    /// Clear all D-Bus proxies when service becomes unavailable.
    fn clear_proxies(&self) {
        *self.adapter_proxy.borrow_mut() = None;
        *self.station_proxy.borrow_mut() = None;
        *self.adapter_path.borrow_mut() = None;
        *self.station_path.borrow_mut() = None;

        // Cancel any pending auth
        if let Some(pending) = self.pending_auth.borrow_mut().take() {
            pending.invocation.return_dbus_error(
                "net.connman.iwd.Agent.Error.Canceled",
                "Service unavailable",
            );
        }

        // Unregister agent D-Bus object so re-registration succeeds on service reappear
        if let (Some(conn), Some(reg_id)) = (
            self.connection.borrow().clone(),
            self.agent_registration_id.borrow_mut().take(),
        ) {
            let _ = conn.unregister_object(reg_id);
        }
    }

    /// Clear only station-related state when WiFi is powered off.
    ///
    /// Unlike `clear_proxies()`, this preserves the adapter proxy and path so
    /// that `set_wifi_enabled(true)` can still reach the adapter.
    fn clear_station(&self) {
        *self.station_proxy.borrow_mut() = None;
        *self.station_path.borrow_mut() = None;

        // Cancel any pending auth (station is gone, auth can't succeed)
        if let Some(pending) = self.pending_auth.borrow_mut().take() {
            pending
                .invocation
                .return_dbus_error("net.connman.iwd.Agent.Error.Canceled", "WiFi powered off");
        }

        // Clear station-related snapshot fields without marking unavailable
        let mut snapshot = self.snapshot.borrow_mut();
        snapshot.state = None;
        snapshot.ssid = None;
        snapshot.networks.clear();
        snapshot.scanning = false;
        snapshot.auth_request = None;
        snapshot.failed_ssid = None;
        snapshot.failed_reason = None;
        snapshot.initial_scan_complete = false;
        let snapshot_clone = snapshot.clone();
        drop(snapshot);
        self.callbacks.notify(&snapshot_clone);
    }

    fn apply_update(this: &Rc<Self>, update: IwdUpdate) {
        match update {
            IwdUpdate::AdapterDiscovered { path } => {
                // Skip if we already have this adapter (avoids duplicate proxy setup
                // from race between InterfacesAdded signal and GetManagedObjects)
                if this.adapter_path.borrow().as_deref() == Some(&path) {
                    return;
                }
                *this.adapter_path.borrow_mut() = Some(path.clone());
                let mut snapshot = this.snapshot.borrow_mut();
                let was_available = snapshot.available;
                snapshot.available = true;
                // Only notify if state changed to avoid redundant UI updates
                if !was_available {
                    let snapshot_clone = snapshot.clone();
                    drop(snapshot);
                    this.callbacks.notify(&snapshot_clone);
                } else {
                    drop(snapshot);
                }
                Self::setup_adapter_proxy(this, &path);
                // Register the agent for password authentication
                Self::register_agent(this);
            }
            IwdUpdate::StationDiscovered { path } => {
                // Skip if we already have this station (avoids duplicate proxy setup
                // from race between InterfacesAdded signal and GetManagedObjects)
                if this.station_path.borrow().as_deref() == Some(&path) {
                    return;
                }
                *this.station_path.borrow_mut() = Some(path.clone());
                let mut snapshot = this.snapshot.borrow_mut();
                let was_available = snapshot.available;
                snapshot.available = true;
                // Only notify if state changed to avoid redundant UI updates
                if !was_available {
                    let snapshot_clone = snapshot.clone();
                    drop(snapshot);
                    this.callbacks.notify(&snapshot_clone);
                } else {
                    drop(snapshot);
                }
                Self::setup_station_proxy(this, &path);
            }
            IwdUpdate::NetworksRefreshed { networks } => {
                let mut snapshot = this.snapshot.borrow_mut();
                snapshot.networks = networks;
                snapshot.initial_scan_complete = true;
                let snapshot_clone = snapshot.clone();
                drop(snapshot);
                this.callbacks.notify(&snapshot_clone);
                this.update_station_state();
            }
            IwdUpdate::ConnectionFailed { ssid, reason } => {
                let mut snapshot = this.snapshot.borrow_mut();
                // Don't overwrite a more specific failure reason (e.g., "Wrong password"
                // set by the agent Cancel callback) with a generic "Connection failed".
                if snapshot.failed_ssid.is_some() {
                    return;
                }
                snapshot.failed_ssid = Some(ssid);
                snapshot.failed_reason = Some(reason);
                let snapshot_clone = snapshot.clone();
                drop(snapshot);
                this.callbacks.notify(&snapshot_clone);
            }
        }
    }

    fn set_unavailable(&self) {
        let mut snapshot = self.snapshot.borrow_mut();
        if !snapshot.available {
            return; // Already unavailable
        }
        *snapshot = IwdSnapshot::unknown();
        let snapshot_clone = snapshot.clone();
        drop(snapshot);
        self.callbacks.notify(&snapshot_clone);
    }

    fn read_network_name(path: &str) -> Option<String> {
        let proxy = match gio::DBusProxy::for_bus_sync(
            gio::BusType::System,
            gio::DBusProxyFlags::NONE,
            None::<&gio::DBusInterfaceInfo>,
            IWD_SERVICE,
            path,
            IFACE_NETWORK,
            None::<&gio::Cancellable>,
        ) {
            Ok(p) => p,
            Err(_) => return None,
        };

        proxy
            .cached_property("Name")
            .and_then(|v| v.get::<String>())
    }

    fn setup_proxy<F>(this: &Rc<Self>, path: &str, interface: &'static str, on_ready: F)
    where
        F: FnOnce(&Rc<Self>, gio::DBusProxy) + 'static,
    {
        let this_weak = Rc::downgrade(this);
        let path = path.to_string();

        let Some(connection) = this.connection.borrow().clone() else {
            return;
        };

        gio::DBusProxy::new(
            &connection,
            gio::DBusProxyFlags::NONE,
            None::<&gio::DBusInterfaceInfo>,
            Some(IWD_SERVICE),
            &path,
            interface,
            None::<&gio::Cancellable>,
            move |res| {
                let Some(this) = this_weak.upgrade() else {
                    return;
                };

                let proxy = match res {
                    Ok(p) => p,
                    Err(e) => {
                        error!("Failed to create {} proxy: {}", interface, e);
                        return;
                    }
                };

                on_ready(&this, proxy);
            },
        )
    }

    fn setup_adapter_proxy(this: &Rc<Self>, path: &str) {
        Self::setup_proxy(this, path, IFACE_ADAPTER, |this, proxy| {
            this.adapter_proxy.replace(Some(proxy.clone()));

            let this_weak = Rc::downgrade(this);
            proxy.connect_local("g-properties-changed", false, move |_| {
                if let Some(this) = this_weak.upgrade() {
                    this.update_adapter_state();
                }
                None
            });

            this.update_adapter_state();
        });
    }

    fn setup_station_proxy(this: &Rc<Self>, path: &str) {
        Self::setup_proxy(this, path, IFACE_STATION, |this, proxy| {
            this.station_proxy.replace(Some(proxy.clone()));

            let this_weak = Rc::downgrade(this);
            proxy.connect_local("g-properties-changed", false, move |_| {
                if let Some(this) = this_weak.upgrade() {
                    this.update_station_state();
                }
                None
            });

            this.update_station_state();
        });
    }

    fn update_adapter_state(&self) {
        let Some(proxy) = self.adapter_proxy.borrow().clone() else {
            return;
        };

        let powered = proxy
            .cached_property("Powered")
            .and_then(|v| v.get::<bool>());

        let mut snapshot = self.snapshot.borrow_mut();
        if snapshot.wifi_enabled != powered {
            snapshot.wifi_enabled = powered;

            if powered == Some(false) {
                snapshot.state = None;
                snapshot.ssid = None;
            }

            let snapshot_clone = snapshot.clone();
            drop(snapshot);
            self.callbacks.notify(&snapshot_clone);
        }
    }

    fn update_station_state(&self) {
        let Some(proxy) = self.station_proxy.borrow().clone() else {
            return;
        };

        let state = proxy
            .cached_property("State")
            .and_then(|v| v.get::<String>());

        let scanning = proxy
            .cached_property("Scanning")
            .and_then(|v| v.get::<bool>())
            .unwrap_or(false);

        let name_path = proxy
            .cached_property("ConnectedNetwork")
            .and_then(|v| v.get::<String>());

        let is_connected_or_connecting = matches!(
            state.as_deref(),
            Some(STATE_CONNECTED | STATE_CONNECTING | STATE_ROAMING)
        );

        let ssid = if is_connected_or_connecting {
            name_path.and_then(|path| {
                self.snapshot
                    .borrow()
                    .networks
                    .iter()
                    .find(|n| n.path.as_deref() == Some(path.as_str()))
                    .map(|n| n.ssid.clone())
            })
        } else {
            // Not connected — reset SSID resolve attempts
            self.ssid_resolve_attempts.set(0);
            None
        };

        // Track SSID resolution attempts to prevent infinite refresh loop
        if is_connected_or_connecting && ssid.is_some() {
            // SSID resolved successfully — reset counter
            self.ssid_resolve_attempts.set(0);
        }

        // Single borrow to read previous state for change detection
        let (should_fetch_networks, scan_just_completed) = {
            let snap = self.snapshot.borrow();
            let was_connected =
                matches!(snap.state.as_deref(), Some(STATE_CONNECTED | STATE_ROAMING));
            let is_connected = matches!(state.as_deref(), Some(STATE_CONNECTED | STATE_ROAMING));
            let needs_fetch = snap.networks.is_empty() || (!was_connected && is_connected);

            // If we need to fetch because SSID is unresolved while connected,
            // check the retry limit to avoid an infinite loop.
            let should_fetch = if needs_fetch && is_connected && ssid.is_none() {
                let attempts = self.ssid_resolve_attempts.get();
                if attempts >= MAX_SSID_RESOLVE_ATTEMPTS {
                    debug!(
                        "SSID resolution failed after {} attempts, giving up",
                        attempts
                    );
                    false
                } else {
                    self.ssid_resolve_attempts.set(attempts + 1);
                    true
                }
            } else {
                needs_fetch
            };

            let scan_done = snap.scanning && !scanning;

            (should_fetch, scan_done)
        };

        // Clear our scan_in_progress guard when IWD reports scan complete
        if scan_just_completed {
            self.scan_in_progress.set(false);
        }

        let mut snapshot = self.snapshot.borrow_mut();
        let mut changed = false;
        if snapshot.state != state {
            snapshot.state = state;
            changed = true;
        }
        if snapshot.ssid != ssid {
            snapshot.ssid = ssid;
            changed = true;

            if snapshot.ssid.is_none() {
                for net in &mut snapshot.networks {
                    net.active = false;
                }
            }
        }
        if snapshot.scanning != scanning {
            snapshot.scanning = scanning;
            changed = true;
        }
        if changed {
            let snapshot_clone = snapshot.clone();
            drop(snapshot);
            self.callbacks.notify(&snapshot_clone);
        } else {
            drop(snapshot);
        }

        // Trigger async network refresh if needed (after releasing borrow)
        if should_fetch_networks || scan_just_completed {
            self.refresh_networks_async();
        }
    }

    /// Enable or disable Wi-Fi hardware.
    pub fn set_wifi_enabled(&self, enabled: bool) {
        let Some(proxy) = self.adapter_proxy.borrow().clone() else {
            debug!("set_wifi_enabled called but adapter_proxy is None");
            return;
        };
        debug!("set_wifi_enabled: setting Powered to {}", enabled);
        std::thread::spawn(move || {
            // Set Powered property via D-Bus Properties interface
            let variant = Variant::tuple_from_iter([
                IFACE_ADAPTER.to_variant(),
                "Powered".to_variant(),
                enabled.to_variant().to_variant(),
            ]);
            if let Err(e) = proxy.call_sync(
                "org.freedesktop.DBus.Properties.Set",
                Some(&variant),
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
            ) {
                error!("Failed to set Powered: {}", e);
            }
        });
    }

    /// Connect to a Wi-Fi network by its D-Bus object path.
    pub fn connect_to_network(&self, path: &str) {
        // Reset SSID resolve attempts for the new connection
        self.ssid_resolve_attempts.set(0);

        {
            let mut snapshot = self.snapshot.borrow_mut();
            if snapshot.failed_ssid.is_some() {
                snapshot.failed_ssid = None;
                snapshot.failed_reason = None;
                let snapshot_clone = snapshot.clone();
                drop(snapshot);
                self.callbacks.notify(&snapshot_clone);
            }
        }

        let path = path.to_string();
        std::thread::spawn(move || {
            // Get the SSID first for error reporting
            let ssid = Self::read_network_name(&path);

            let proxy = match gio::DBusProxy::for_bus_sync(
                gio::BusType::System,
                gio::DBusProxyFlags::NONE,
                None::<&gio::DBusInterfaceInfo>,
                IWD_SERVICE,
                &path,
                IFACE_NETWORK,
                None::<&gio::Cancellable>,
            ) {
                Ok(p) => p,
                Err(e) => {
                    warn!("Failed to create network proxy: {}", e);
                    if let Some(ssid) = ssid {
                        send_network_update(IwdUpdate::ConnectionFailed {
                            ssid,
                            reason: "Network not found".to_string(),
                        });
                    }
                    return;
                }
            };
            if let Err(e) = proxy.call_sync(
                "Connect",
                None,
                gio::DBusCallFlags::NONE,
                30000,
                None::<&gio::Cancellable>,
            ) {
                // Check the error type to determine how to handle it
                let error_name = gio::DBusError::remote_error(&e);
                let error_name_str = error_name.as_ref().map(|s| s.as_str()).unwrap_or("");

                // Agent errors (wrong password) are handled by the Cancel callback
                let is_agent_error = error_name_str.starts_with("net.connman.iwd.Agent");

                let is_transient_error = matches!(
                    error_name_str,
                    "net.connman.iwd.Aborted"
                        | "net.connman.iwd.InProgress"
                        | "net.connman.iwd.NotAvailable"
                );

                if is_transient_error {
                    debug!(
                        "Connect got transient error '{}', not treating as failure",
                        error_name_str
                    );
                } else if !is_agent_error {
                    warn!("Connect failed: {} (error: {})", e, error_name_str);
                    if let Some(ssid) = ssid {
                        let reason = match error_name_str {
                            "net.connman.iwd.NotFound" => "Network not found".to_string(),
                            other => {
                                format!("Connection failed ({})", other)
                            }
                        };
                        send_network_update(IwdUpdate::ConnectionFailed { ssid, reason });
                    }
                }
            }
        });
    }
    /// Disconnect from the current Wi-Fi network.
    pub fn disconnect(&self) {
        let Some(proxy) = self.station_proxy.borrow().clone() else {
            return;
        };
        std::thread::spawn(move || {
            if let Err(e) = proxy.call_sync(
                "Disconnect",
                None,
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
            ) {
                warn!("Disconnect failed: {}", e);
            }
        });
    }

    /// Forget a saved Wi-Fi network by its KnownNetwork D-Bus path.
    pub fn forget_network(&self, path: &str) {
        let path = path.to_string();
        std::thread::spawn(move || {
            let proxy = match gio::DBusProxy::for_bus_sync(
                gio::BusType::System,
                gio::DBusProxyFlags::NONE,
                None::<&gio::DBusInterfaceInfo>,
                IWD_SERVICE,
                &path,
                IFACE_KNOWN_NETWORK,
                None::<&gio::Cancellable>,
            ) {
                Ok(p) => p,
                Err(e) => {
                    warn!("Failed to create known network proxy: {}", e);
                    return;
                }
            };
            if let Err(e) = proxy.call_sync(
                "Forget",
                None,
                gio::DBusCallFlags::NONE,
                30000,
                None::<&gio::Cancellable>,
            ) {
                warn!("Forget network failed: {}", e);
            } else {
                glib::idle_add_once(|| {
                    let service = IwdService::global();
                    service.refresh_networks_async();
                });
            }
        });
    }

    /// Request a Wi-Fi network scan.
    pub fn scan_networks(&self) {
        // Guard against duplicate scan requests (mirrors NetworkManager pattern)
        if self.scan_in_progress.get() {
            return;
        }

        let Some(proxy) = self.station_proxy.borrow().clone() else {
            return;
        };

        self.scan_in_progress.set(true);

        // Use async call to avoid blocking the main thread
        proxy.call(
            "Scan",
            None,
            gio::DBusCallFlags::NONE,
            30000, // 30 second timeout for scan
            None::<&gio::Cancellable>,
            |res| {
                if let Err(e) = res {
                    debug!("Scan failed: {}", e);
                    // Callback runs on main thread — safe to access global directly
                    IwdService::global().scan_in_progress.set(false);
                }
            },
        );
    }

    fn refresh_networks_async(&self) {
        let Some(proxy) = self.station_proxy.borrow().clone() else {
            return;
        };

        std::thread::spawn(move || {
            let networks = Self::get_networks_sync(&proxy);
            send_network_update(IwdUpdate::NetworksRefreshed { networks });
        });
    }

    fn get_networks_sync(proxy: &gio::DBusProxy) -> Vec<WifiNetwork> {
        // Step 1: Get ordered networks from Station (provides path + signal strength)
        let result = match proxy.call_sync(
            "GetOrderedNetworks",
            None,
            gio::DBusCallFlags::NONE,
            5000,
            None::<&gio::Cancellable>,
        ) {
            Ok(r) => r,
            Err(e) => {
                debug!("GetOrderedNetworks failed: {}", e);
                return Vec::new();
            }
        };

        // Step 2: Get all managed objects in a single call for property lookup
        let managed_props = Self::fetch_network_properties();

        // Step 3: Build network list by joining ordered networks with managed object properties
        let array = result.child_value(0);
        let count = array.n_children();
        let mut networks = Vec::new();
        for i in 0..count {
            let tuple = array.child_value(i);

            let path: String = match tuple.child_value(0).get() {
                Some(p) => p,
                None => continue,
            };

            let signal_raw: i16 = tuple.child_value(1).get().unwrap_or(-10000);
            let strength = dbm_to_percent(signal_raw);

            // Look up properties from managed objects HashMap
            let (ssid, net_type, connected, known_network_path) =
                if let Some(props) = managed_props.get(&path) {
                    (
                        props.name.clone(),
                        props.net_type.clone(),
                        props.connected,
                        props.known_network_path.clone(),
                    )
                } else {
                    // Fallback: network not found in managed objects (shouldn't normally happen)
                    debug!("Network {} not found in managed objects, skipping", path);
                    continue;
                };

            let security = if net_type == "open" {
                "open"
            } else {
                "secured"
            }
            .to_string();
            networks.push(WifiNetwork {
                ssid,
                strength,
                security,
                active: connected,
                known: known_network_path.is_some(),
                known_network_path,
                path: Some(path),
            });
        }
        networks
    }

    /// Fetch all IWD network properties via GetManagedObjects in a single D-Bus call.
    /// Returns a HashMap keyed by object path with network properties.
    /// Called from background thread (sync D-Bus).
    fn fetch_network_properties() -> HashMap<String, NetworkProps> {
        let mut props_map = HashMap::new();

        let om_proxy = match gio::DBusProxy::for_bus_sync(
            gio::BusType::System,
            gio::DBusProxyFlags::DO_NOT_LOAD_PROPERTIES
                | gio::DBusProxyFlags::DO_NOT_CONNECT_SIGNALS,
            None::<&gio::DBusInterfaceInfo>,
            IWD_SERVICE,
            "/",
            OBJECT_MANAGER_IFACE,
            None::<&gio::Cancellable>,
        ) {
            Ok(p) => p,
            Err(e) => {
                debug!("Failed to create ObjectManager proxy: {}", e);
                return props_map;
            }
        };

        let result = match om_proxy.call_sync(
            "GetManagedObjects",
            None,
            gio::DBusCallFlags::NONE,
            5000,
            None::<&gio::Cancellable>,
        ) {
            Ok(r) => r,
            Err(e) => {
                debug!("GetManagedObjects failed: {}", e);
                return props_map;
            }
        };

        let dict = result.child_value(0);
        let n = dict.n_children();
        for i in 0..n {
            let entry = dict.child_value(i);
            let path: Option<String> = entry.child_value(0).get();
            let Some(path) = path else { continue };

            let ifaces = entry.child_value(1);
            let n_ifaces = ifaces.n_children();
            for j in 0..n_ifaces {
                let iface_entry = ifaces.child_value(j);
                let iface_name: Option<String> = iface_entry.child_value(0).get();
                if iface_name.as_deref() != Some(IFACE_NETWORK) {
                    continue;
                }

                // Parse network properties from this interface's property dict
                let props_variant = iface_entry.child_value(1);
                let n_props = props_variant.n_children();
                let mut name = String::new();
                let mut net_type = "open".to_string();
                let mut connected = false;
                let mut known_network_path: Option<String> = None;

                for k in 0..n_props {
                    let prop = props_variant.child_value(k);
                    let key: Option<String> = prop.child_value(0).get();
                    let Some(key) = key else { continue };
                    let value = prop.child_value(1);
                    let inner = value.child_value(0);

                    match key.as_str() {
                        "Name" => name = inner.get::<String>().unwrap_or_default(),
                        "Type" => {
                            net_type = inner.get::<String>().unwrap_or_else(|| "open".to_string())
                        }
                        "Connected" => connected = inner.get::<bool>().unwrap_or(false),
                        "KnownNetwork" => known_network_path = inner.get::<String>(),
                        _ => {}
                    }
                }

                props_map.insert(
                    path.clone(),
                    NetworkProps {
                        name,
                        net_type,
                        connected,
                        known_network_path,
                    },
                );
                break; // Only one Network interface per object
            }
        }

        props_map
    }

    /// Register the IWD Agent for handling password authentication.
    fn register_agent(this: &Rc<Self>) {
        // Guard against duplicate registration
        if this.agent_registration_id.borrow().is_some() {
            debug!("IwdService: agent already registered, skipping");
            return;
        }

        let Some(connection) = this.connection.borrow().clone() else {
            debug!("IwdService: no connection available for agent registration");
            return;
        };

        let node_info = match gio::DBusNodeInfo::for_xml(AGENT_INTROSPECTION) {
            Ok(info) => info,
            Err(e) => {
                error!("IwdService: failed to parse agent introspection: {}", e);
                return;
            }
        };

        let interface_info = match node_info.lookup_interface(AGENT_IFACE) {
            Some(info) => info,
            None => {
                error!("IwdService: Agent interface not found in introspection");
                return;
            }
        };

        // Register the agent object on the bus
        let this_weak = Rc::downgrade(this);
        let registration = connection
            .register_object(AGENT_PATH, &interface_info)
            .method_call(
                move |_conn, _sender, _path, _iface, method, params, invocation| {
                    let this = match this_weak.upgrade() {
                        Some(s) => s,
                        None => {
                            invocation
                                .return_error(gio::IOErrorEnum::Failed, "Service unavailable");
                            return;
                        }
                    };

                    Self::handle_agent_method(&this, method, params, invocation);
                },
            )
            .build();

        match registration {
            Ok(id) => {
                debug!("IwdService: registered agent at {}", AGENT_PATH);
                *this.agent_registration_id.borrow_mut() = Some(id);

                // Now register with IWD's AgentManager
                Self::register_with_agent_manager(this, &connection);
            }
            Err(e) => {
                error!("IwdService: failed to register agent object: {}", e);
            }
        }
    }

    /// Register our agent with IWD's AgentManager.
    fn register_with_agent_manager(this: &Rc<Self>, connection: &gio::DBusConnection) {
        let this_weak = Rc::downgrade(this);

        gio::DBusProxy::new(
            connection,
            gio::DBusProxyFlags::NONE,
            None,
            Some(IWD_SERVICE),
            IWD_ROOT_PATH,
            AGENT_MANAGER_IFACE,
            None::<&gio::Cancellable>,
            move |res| {
                if this_weak.upgrade().is_none() {
                    return;
                }

                let proxy = match res {
                    Ok(p) => p,
                    Err(e) => {
                        error!("IwdService: failed to create AgentManager proxy: {}", e);
                        return;
                    }
                };

                // RegisterAgent(object path)
                let agent_path = glib::variant::ObjectPath::try_from(AGENT_PATH)
                    .expect("AGENT_PATH constant must be a valid D-Bus object path");
                let args = (agent_path,).to_variant();

                proxy.call(
                    "RegisterAgent",
                    Some(&args),
                    gio::DBusCallFlags::NONE,
                    5000,
                    None::<&gio::Cancellable>,
                    move |res| {
                        if let Err(e) = res {
                            // AlreadyExists is fine (agent already registered)
                            let is_already_exists = gio::DBusError::remote_error(&e)
                                .map(|e| e == "net.connman.iwd.AlreadyExists")
                                .unwrap_or(false);
                            if !is_already_exists {
                                error!("IwdService: RegisterAgent failed: {}", e);
                                return;
                            }
                        }
                        debug!("IwdService: agent registered with AgentManager");
                    },
                );
            },
        );
    }

    /// Handle incoming agent method calls.
    ///
    /// Sender validation is not performed here because the system bus policy
    /// already restricts which services can invoke methods on our registered
    /// agent object path. Only IWD (running as a system service) can reach
    /// this callback. See the same pattern in `bluetooth.rs`.
    fn handle_agent_method(
        this: &Rc<Self>,
        method: &str,
        params: Variant,
        invocation: gio::DBusMethodInvocation,
    ) {
        debug!("IwdService: agent method '{}' called", method);

        match method {
            "Release" => {
                debug!("IwdService: agent released");
                invocation.return_value(None);
            }
            "Cancel" => {
                // Extract reason from params - IWD passes reasons like "Error" for wrong password
                let reason: String = params.child_value(0).get().unwrap_or_default();
                debug!(
                    "IwdService: auth cancelled by IWD, reason: '{}' (raw)",
                    reason
                );

                // Treat "Error" and auth-related reasons as authentication failures.
                let reason_lower = reason.to_lowercase();
                let is_auth_failure = reason_lower == "error"
                    || reason_lower.contains("auth")
                    || reason_lower.contains("password")
                    || reason_lower.contains("psk");

                // Log all cancel reasons to help identify new patterns
                if !is_auth_failure {
                    debug!(
                        "IwdService: cancel reason '{}' not treated as auth failure",
                        reason
                    );
                }

                if is_auth_failure {
                    // Set failed_ssid before clearing auth state so UI can show error
                    if let Some(ref auth_req) = this.snapshot.borrow().auth_request {
                        this.set_failed_ssid(&auth_req.ssid, "Wrong password");
                    }
                }

                // Clear pending auth
                if let Some(pending) = this.pending_auth.borrow_mut().take() {
                    pending
                        .invocation
                        .return_dbus_error("net.connman.iwd.Agent.Error.Canceled", "Canceled");
                }
                // Clear auth request from snapshot
                this.clear_auth_state();
                invocation.return_value(None);
            }
            "RequestPassphrase" => {
                Self::handle_request_passphrase(this, params, invocation);
            }
            "RequestPrivateKeyPassphrase" => {
                // Treat the same as RequestPassphrase for now
                Self::handle_request_passphrase(this, params, invocation);
            }
            "RequestUserNameAndPassword" | "RequestUserPassword" => {
                // Enterprise auth - not supported yet
                warn!("IwdService: enterprise auth not supported");
                invocation.return_dbus_error(
                    "net.connman.iwd.Agent.Error.Canceled",
                    "Enterprise authentication not supported",
                );
            }
            _ => {
                error!("IwdService: unknown agent method: {}", method);
                invocation.return_error(
                    gio::IOErrorEnum::NotSupported,
                    &format!("Unknown method: {}", method),
                );
            }
        }
    }

    /// Handle RequestPassphrase - IWD is asking for the network password.
    ///
    /// No explicit timeout is needed. Auth is cancelled when the user cancels,
    /// the panel closes, or IWD sends a Cancel callback.
    fn handle_request_passphrase(
        this: &Rc<Self>,
        params: Variant,
        invocation: gio::DBusMethodInvocation,
    ) {
        // Extract network path from params
        let network_path: String = match params.child_value(0).get::<String>() {
            Some(p) => p,
            None => {
                error!("IwdService: RequestPassphrase missing network path");
                invocation.return_error(gio::IOErrorEnum::InvalidArgument, "Missing network path");
                return;
            }
        };

        // Get SSID from cached network list to avoid blocking D-Bus call on main thread.
        let ssid = this
            .snapshot
            .borrow()
            .networks
            .iter()
            .find(|n| n.path.as_deref() == Some(network_path.as_str()))
            .map(|n| n.ssid.clone())
            .unwrap_or_else(|| "Unknown".to_string());

        debug!(
            "IwdService: RequestPassphrase for network '{}' ({})",
            ssid, network_path
        );

        // Cancel any existing pending auth
        if let Some(pending) = this.pending_auth.borrow_mut().take() {
            pending.invocation.return_dbus_error(
                "net.connman.iwd.Agent.Error.Canceled",
                "Superseded by new auth request",
            );
        }

        // Store the pending auth
        *this.pending_auth.borrow_mut() = Some(PendingAuth { invocation });

        // Update snapshot with auth request and notify UI
        let mut snapshot = this.snapshot.borrow_mut();
        snapshot.auth_request = Some(IwdAuthRequest { ssid });
        // Clear any previous failed_ssid when starting new auth
        snapshot.failed_ssid = None;
        snapshot.failed_reason = None;
        let snapshot_clone = snapshot.clone();
        drop(snapshot);
        this.callbacks.notify(&snapshot_clone);
    }

    /// Submit a passphrase in response to a pending auth request.
    pub fn submit_passphrase(&self, passphrase: &str) {
        let pending = match self.pending_auth.borrow_mut().take() {
            Some(p) => p,
            None => {
                debug!("IwdService: submit_passphrase called but no pending auth");
                return;
            }
        };

        debug!("IwdService: submitting passphrase");

        // Complete the D-Bus invocation with the passphrase
        pending
            .invocation
            .return_value(Some(&(passphrase,).to_variant()));

        // Clear auth request from snapshot
        self.clear_auth_state();
    }

    /// Cancel a pending auth request.
    pub fn cancel_auth(&self) {
        let pending = match self.pending_auth.borrow_mut().take() {
            Some(p) => p,
            None => {
                // No pending auth - this is normal during cleanup
                return;
            }
        };

        debug!("IwdService: cancelling auth");

        // Return error to IWD
        pending
            .invocation
            .return_dbus_error("net.connman.iwd.Agent.Error.Canceled", "User canceled");

        // Clear auth request from snapshot
        self.clear_auth_state();
    }

    /// Clear the auth request from snapshot and notify.
    fn clear_auth_state(&self) {
        let mut snapshot = self.snapshot.borrow_mut();
        if snapshot.auth_request.is_some() {
            snapshot.auth_request = None;
            let snapshot_clone = snapshot.clone();
            drop(snapshot);
            self.callbacks.notify(&snapshot_clone);
        }
    }

    /// Set the failed SSID with a reason and notify listeners.
    pub fn set_failed_ssid(&self, ssid: &str, reason: &str) {
        let mut snapshot = self.snapshot.borrow_mut();
        snapshot.failed_ssid = Some(ssid.to_string());
        snapshot.failed_reason = Some(reason.to_string());
        let snapshot_clone = snapshot.clone();
        drop(snapshot);
        self.callbacks.notify(&snapshot_clone);
    }

    /// Clear the failed state and notify listeners.
    pub fn clear_failed_state(&self) {
        let mut snapshot = self.snapshot.borrow_mut();
        if snapshot.failed_ssid.is_some() {
            snapshot.failed_ssid = None;
            snapshot.failed_reason = None;
            let snapshot_clone = snapshot.clone();
            drop(snapshot);
            self.callbacks.notify(&snapshot_clone);
        }
    }
}

impl Drop for IwdService {
    fn drop(&mut self) {
        debug!("IwdService: dropping, cleaning up resources");

        // Cancel any pending authentication
        if let Some(pending) = self.pending_auth.borrow_mut().take() {
            pending.invocation.return_dbus_error(
                "net.connman.iwd.Agent.Error.Canceled",
                "Service shutting down",
            );
        }

        // Unregister from IWD's AgentManager before unregistering D-Bus object
        if let Some(conn) = self.connection.borrow().as_ref() {
            // Try to unregister agent with AgentManager synchronously
            if let Ok(proxy) = gio::DBusProxy::for_bus_sync(
                gio::BusType::System,
                gio::DBusProxyFlags::DO_NOT_LOAD_PROPERTIES
                    | gio::DBusProxyFlags::DO_NOT_CONNECT_SIGNALS,
                None::<&gio::DBusInterfaceInfo>,
                IWD_SERVICE,
                IWD_ROOT_PATH,
                AGENT_MANAGER_IFACE,
                None::<&gio::Cancellable>,
            ) && let Ok(agent_path) = glib::variant::ObjectPath::try_from(AGENT_PATH)
            {
                let args = (agent_path,).to_variant();
                if let Err(e) = proxy.call_sync(
                    "UnregisterAgent",
                    Some(&args),
                    gio::DBusCallFlags::NONE,
                    200, // Very short timeout - we're shutting down, don't block
                    None::<&gio::Cancellable>,
                ) {
                    debug!(
                        "IwdService: failed to unregister agent from AgentManager: {}",
                        e
                    );
                } else {
                    debug!("IwdService: unregistered agent from AgentManager");
                }
            }

            // Unregister the agent D-Bus object
            if let Some(reg_id) = self.agent_registration_id.borrow_mut().take() {
                match conn.unregister_object(reg_id) {
                    Ok(()) => debug!("IwdService: unregistered agent object from D-Bus"),
                    Err(e) => debug!("IwdService: failed to unregister agent object: {}", e),
                }
            }
        }
    }
}

fn send_network_update(update: IwdUpdate) {
    glib::idle_add_once(move || {
        IwdService::apply_update(&IwdService::global(), update);
    });
}

/// Convert IWD signal strength (dBm * 100) to percentage (0-100).
///
/// IWD reports signal strength as dBm multiplied by 100 (e.g., -5000 = -50 dBm).
/// This function uses a linear approximation:
/// - -100 dBm (very weak) maps to 0%
/// - -50 dBm (excellent) maps to 100%
///
/// The formula is: percent = 2 * (dBm + 100), clamped to [0, 100].
fn dbm_to_percent(dbm_times_100: i16) -> i32 {
    let dbm = dbm_times_100 as f64 / 100.0;
    ((2.0 * (dbm + 100.0)) as i32).clamp(0, 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dbm_to_percent_boundaries() {
        // -100 dBm (very weak) should map to 0%
        assert_eq!(dbm_to_percent(-10000), 0);
        // -50 dBm (excellent) should map to 100%
        assert_eq!(dbm_to_percent(-5000), 100);
    }

    #[test]
    fn test_dbm_to_percent_midpoints() {
        // -75 dBm should map to 50%
        assert_eq!(dbm_to_percent(-7500), 50);
        // -60 dBm should map to 80%
        assert_eq!(dbm_to_percent(-6000), 80);
        // -90 dBm should map to 20%
        assert_eq!(dbm_to_percent(-9000), 20);
    }

    #[test]
    fn test_dbm_to_percent_clamping() {
        // Values better than -50 dBm should clamp to 100%
        assert_eq!(dbm_to_percent(-4000), 100); // -40 dBm
        assert_eq!(dbm_to_percent(-3000), 100); // -30 dBm
        assert_eq!(dbm_to_percent(0), 100); // 0 dBm (unrealistic but should handle)

        // Values worse than -100 dBm should clamp to 0%
        assert_eq!(dbm_to_percent(-11000), 0); // -110 dBm
        assert_eq!(dbm_to_percent(-15000), 0); // -150 dBm
    }

    #[test]
    fn test_dbm_to_percent_typical_values() {
        // Typical Wi-Fi signal strengths
        // -55 dBm (good signal) -> 90%
        assert_eq!(dbm_to_percent(-5500), 90);
        // -70 dBm (fair signal) -> 60%
        assert_eq!(dbm_to_percent(-7000), 60);
        // -85 dBm (weak signal) -> 30%
        assert_eq!(dbm_to_percent(-8500), 30);
    }
}
