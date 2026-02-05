use crate::services::callbacks::Callbacks;
use gtk4::gio::{self, prelude::*};
use gtk4::glib::{self, Variant};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use tracing::{debug, error, warn};

use super::WifiNetwork;

const IWD_SERVICE: &str = "net.connman.iwd";
const IWD_ROOT_PATH: &str = "/net/connman/iwd";
const IFACE_INTROSPECTABLE: &str = "org.freedesktop.DBus.Introspectable";
const IFACE_ADAPTER: &str = "net.connman.iwd.Adapter";
const IFACE_STATION: &str = "net.connman.iwd.Station";
const IFACE_NETWORK: &str = "net.connman.iwd.Network";
const IFACE_KNOWN_NETWORK: &str = "net.connman.iwd.KnownNetwork";

const AGENT_IFACE: &str = "net.connman.iwd.Agent";
const AGENT_MANAGER_IFACE: &str = "net.connman.iwd.AgentManager";
const AGENT_PATH: &str = "/org/vibepanel/iwd/agent";

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
    ServiceUnavailable,
    NetworksRefreshed {
        networks: Vec<WifiNetwork>,
    },
    /// Connection failed before agent was invoked (e.g., network disappeared).
    ConnectionFailed {
        ssid: String,
    },
}

/// Authentication request from IWD agent.
#[derive(Debug, Clone)]
pub struct IwdAuthRequest {
    pub ssid: String,
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
    watcher_proxy: RefCell<Option<gio::DBusProxy>>,
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
            watcher_proxy: RefCell::new(None),
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

                // Create a proxy to monitor IWD service name owner changes
                let this_weak = Rc::downgrade(&this);
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
                        let this = match this_weak.upgrade() {
                            Some(this) => this,
                            None => return,
                        };

                        let proxy = match res {
                            Ok(p) => p,
                            Err(e) => {
                                debug!("Failed to create IWD watcher proxy: {}", e);
                                // Still try to discover - service might be available
                                Self::start_discovery();
                                return;
                            }
                        };

                        // Monitor for IWD service appearing/disappearing
                        let this_weak = Rc::downgrade(&this);
                        proxy.connect_notify_local(Some("g-name-owner"), move |proxy, _| {
                            let Some(this) = this_weak.upgrade() else {
                                return;
                            };

                            let has_owner = proxy.name_owner().is_some();
                            if has_owner {
                                // Service reappeared - rediscover IWD devices
                                debug!("IWD service appeared, rediscovering devices");
                                Self::start_discovery();
                            } else {
                                // Service disappeared - mark unavailable and clear proxies
                                debug!("IWD service disappeared");
                                this.clear_proxies();
                                this.set_unavailable();
                            }
                        });

                        // Store the watcher proxy to keep it alive
                        this.watcher_proxy.replace(Some(proxy.clone()));

                        // Check if service is currently available and start discovery
                        if proxy.name_owner().is_some() {
                            Self::start_discovery();
                        } else {
                            debug!("IWD service not available at startup");
                            this.set_unavailable();
                        }
                    },
                );
            },
        );
    }

    /// Start device discovery in a background thread.
    fn start_discovery() {
        std::thread::spawn(move || match Self::discover_paths() {
            Ok((adapter_path, station_path, powered)) => {
                debug!(
                    "Found iwd adapter at: {} (powered: {}), station at: {}",
                    adapter_path, powered, station_path
                );
                send_network_update(IwdUpdate::AdapterDiscovered { path: adapter_path });
                send_network_update(IwdUpdate::StationDiscovered { path: station_path });
            }
            Err(e) => {
                error!("Failed to discover iwd: {}", e);
                send_network_update(IwdUpdate::ServiceUnavailable)
            }
        });
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

        // Clear agent registration (will re-register on service reappear)
        *self.agent_registration_id.borrow_mut() = None;
    }

    fn apply_update(&self, update: IwdUpdate) {
        match update {
            IwdUpdate::AdapterDiscovered { path } => {
                *self.adapter_path.borrow_mut() = Some(path.clone());
                let mut snapshot = self.snapshot.borrow_mut();
                let was_available = snapshot.available;
                snapshot.available = true;
                // Only notify if state changed to avoid redundant UI updates
                if !was_available {
                    let snapshot_clone = snapshot.clone();
                    drop(snapshot);
                    self.callbacks.notify(&snapshot_clone);
                } else {
                    drop(snapshot);
                }
                let this = IwdService::global();
                Self::setup_adapter_proxy(&this, &path);
                // Register the agent for password authentication
                Self::register_agent(&this);
            }
            IwdUpdate::StationDiscovered { path } => {
                *self.station_path.borrow_mut() = Some(path.clone());
                let mut snapshot = self.snapshot.borrow_mut();
                let was_available = snapshot.available;
                snapshot.available = true;
                // Only notify if state changed to avoid redundant UI updates
                if !was_available {
                    let snapshot_clone = snapshot.clone();
                    drop(snapshot);
                    self.callbacks.notify(&snapshot_clone);
                } else {
                    drop(snapshot);
                }
                let this = IwdService::global();
                Self::setup_station_proxy(&this, &path);
            }
            IwdUpdate::ServiceUnavailable => {
                self.set_unavailable();
            }
            IwdUpdate::NetworksRefreshed { networks } => {
                let mut snapshot = self.snapshot.borrow_mut();
                snapshot.networks = networks;
                snapshot.initial_scan_complete = true;
                let snapshot_clone = snapshot.clone();
                drop(snapshot);
                self.callbacks.notify(&snapshot_clone);
            }
            IwdUpdate::ConnectionFailed { ssid } => {
                // Connection failed before agent was invoked - set failed_ssid for UI feedback
                let mut snapshot = self.snapshot.borrow_mut();
                snapshot.failed_ssid = Some(ssid);
                let snapshot_clone = snapshot.clone();
                drop(snapshot);
                self.callbacks.notify(&snapshot_clone);
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

    fn discover_paths() -> Result<(String, String, bool), String> {
        // Find adapter under root
        let children = Self::introspect_children(IWD_ROOT_PATH)?;
        let (adapter_path, powered) = children
            .into_iter()
            .map(|name| format!("{}/{}", IWD_ROOT_PATH, name))
            .find_map(|path| Self::has_adapter_interface(&path).map(|p| (path, p)))
            .ok_or("No adapter found")?;

        // Find station under adapter
        let children = Self::introspect_children(&adapter_path)?;
        let station_path = children
            .into_iter()
            .map(|name| format!("{}/{}", adapter_path, name))
            .find(|path| Self::has_station_interface(path))
            .ok_or("No station found")?;

        Ok((adapter_path, station_path, powered))
    }

    fn introspect_children(path: &str) -> Result<Vec<String>, String> {
        let proxy = gio::DBusProxy::for_bus_sync(
            gio::BusType::System,
            gio::DBusProxyFlags::NONE,
            None::<&gio::DBusInterfaceInfo>,
            IWD_SERVICE,
            path,
            IFACE_INTROSPECTABLE,
            None::<&gio::Cancellable>,
        )
        .map_err(|e| format!("Failed to create introspect proxy for {}: {}", path, e))?;

        let result = proxy
            .call_sync(
                "Introspect",
                None,
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
            )
            .map_err(|e| format!("Introspect failed for {}: {}", path, e))?;

        let xml = result
            .child_value(0)
            .get::<String>()
            .ok_or_else(|| format!("Failed to get XML from {}", path))?;

        Ok(Self::parse_child_nodes(&xml))
    }

    fn parse_child_nodes(xml: &str) -> Vec<String> {
        let mut nodes = Vec::new();

        for line in xml.lines() {
            if let Some(start) = line.find("<node name=\"") {
                let after_quote = start + 12;

                if let Some(end) = line[after_quote..].find('"') {
                    let name = &line[after_quote..after_quote + end];
                    nodes.push(name.to_string());
                }
            }
        }

        nodes
    }

    fn has_station_interface(path: &str) -> bool {
        gio::DBusProxy::for_bus_sync(
            gio::BusType::System,
            gio::DBusProxyFlags::NONE,
            None::<&gio::DBusInterfaceInfo>,
            IWD_SERVICE,
            path,
            IFACE_STATION,
            None::<&gio::Cancellable>,
        )
        .is_ok()
    }

    fn has_adapter_interface(path: &str) -> Option<bool> {
        let proxy = match gio::DBusProxy::for_bus_sync(
            gio::BusType::System,
            gio::DBusProxyFlags::NONE,
            None::<&gio::DBusInterfaceInfo>,
            IWD_SERVICE,
            path,
            IFACE_ADAPTER,
            None::<&gio::Cancellable>,
        ) {
            Ok(p) => p,
            Err(_) => return None,
        };

        // Return the Powered state, defaulting to false if property is missing.
        // This ensures adapter discovery succeeds even if IWD doesn't report the property.
        proxy
            .cached_property("Powered")
            .and_then(|v| v.get::<bool>())
            .or(Some(false))
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

        let ssid = if matches!(
            state.as_deref(),
            Some(STATE_CONNECTED | STATE_CONNECTING | STATE_ROAMING)
        ) {
            name_path.and_then(|path| Self::read_network_name(&path))
        } else {
            None
        };

        let should_fetch_networks = {
            let snap = self.snapshot.borrow();
            let was_connected =
                matches!(snap.state.as_deref(), Some(STATE_CONNECTED | STATE_ROAMING));
            let is_connected = matches!(state.as_deref(), Some(STATE_CONNECTED | STATE_ROAMING));
            snap.networks.is_empty() || (!was_connected && is_connected)
        };

        let scan_just_completed = {
            let snap = self.snapshot.borrow();
            snap.scanning && !scanning
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
                        send_network_update(IwdUpdate::ConnectionFailed { ssid });
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
                // Only report failure if this wasn't an agent-related error.
                // Agent errors (wrong password) are handled by the Cancel callback.
                let is_agent_error = gio::DBusError::remote_error(&e)
                    .map(|name| name.starts_with("net.connman.iwd.Agent"))
                    .unwrap_or(false);
                if !is_agent_error {
                    warn!("Connect failed: {}", e);
                    if let Some(ssid) = ssid {
                        send_network_update(IwdUpdate::ConnectionFailed { ssid });
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
                    // Clear scan_in_progress on failure
                    glib::idle_add_once(|| {
                        IwdService::global().scan_in_progress.set(false);
                    });
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

            let net_proxy = match gio::DBusProxy::for_bus_sync(
                gio::BusType::System,
                gio::DBusProxyFlags::NONE,
                None::<&gio::DBusInterfaceInfo>,
                IWD_SERVICE,
                &path,
                IFACE_NETWORK,
                None::<&gio::Cancellable>,
            ) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let ssid = net_proxy
                .cached_property("Name")
                .and_then(|v| v.get::<String>())
                .unwrap_or_default();
            let net_type = net_proxy
                .cached_property("Type")
                .and_then(|v| v.get::<String>())
                .unwrap_or_else(|| "open".to_string());
            let connected = net_proxy
                .cached_property("Connected")
                .and_then(|v| v.get::<bool>())
                .unwrap_or(false);
            let known_network_path = net_proxy
                .cached_property("KnownNetwork")
                .and_then(|v| v.get::<String>());
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
                        this.set_failed_ssid(&auth_req.ssid);
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

        // Get SSID from network path
        let ssid = Self::read_network_name(&network_path).unwrap_or_else(|| "Unknown".to_string());

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

    /// Set the failed SSID and notify listeners.
    pub fn set_failed_ssid(&self, ssid: &str) {
        let mut snapshot = self.snapshot.borrow_mut();
        snapshot.failed_ssid = Some(ssid.to_string());
        let snapshot_clone = snapshot.clone();
        drop(snapshot);
        self.callbacks.notify(&snapshot_clone);
    }

    /// Clear the failed state and notify listeners.
    pub fn clear_failed_state(&self) {
        let mut snapshot = self.snapshot.borrow_mut();
        if snapshot.failed_ssid.is_some() {
            snapshot.failed_ssid = None;
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
        IwdService::global().apply_update(update);
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
