use crate::services::callbacks::{CallbackId, Callbacks};
use gtk4::gio::{self, prelude::*};
use gtk4::glib::{self, Variant};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;
use tracing::{debug, error, warn};

use std::collections::HashMap;

use super::{SecurityType, WifiNetwork, objpath_to_string};

pub(super) const IWD_SERVICE: &str = "net.connman.iwd";
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
/// D-Bus error returned by the agent when cancelling an auth request.
const AGENT_ERROR_CANCELED: &str = "net.connman.iwd.Agent.Error.Canceled";
const DBUS_PEER_IFACE: &str = "org.freedesktop.DBus.Peer";

/// Max SSID resolution retries before giving up.
const MAX_SSID_RESOLVE_ATTEMPTS: u8 = 3;

/// IWD station state (from `net.connman.iwd.Station` `State` property).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StationState {
    Connecting,
    Connected,
    Roaming,
    Disconnected,
}

impl StationState {
    fn from_dbus(s: &str) -> Self {
        match s {
            "connecting" => Self::Connecting,
            "connected" => Self::Connected,
            "roaming" => Self::Roaming,
            "disconnected" | "disconnecting" => Self::Disconnected,
            other => {
                debug!("Unknown IWD station state: '{other}', treating as Disconnected");
                Self::Disconnected
            }
        }
    }
}

/// Delay (ms) to coalesce rapid ObjectManager signals into a single network refresh.
const NETWORK_REFRESH_DEBOUNCE_MS: u64 = 100;

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
        /// Generation at refresh start; discarded if stale.
        generation: u64,
    },
    /// Connection failed (e.g., network disappeared, auth failure, DHCP timeout).
    /// Ignored if a more specific failure (from agent Cancel) is already set.
    ConnectionFailed {
        ssid: String,
        /// Human-readable reason for the failure.
        reason: String,
        /// Generation at connection start; discarded if stale.
        generation: u64,
    },
    /// Network refresh D-Bus call failed; clears the in-progress flag without
    /// replacing the network list.
    RefreshFailed {
        /// Generation at refresh start; discarded if stale.
        generation: u64,
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
    /// GLib timeout source that fires after AUTH_TIMEOUT_SECS to auto-cancel.
    timeout_source: Option<glib::SourceId>,
}

/// Timeout (seconds) for pending auth requests. Auto-cancels if no password submitted.
const AUTH_TIMEOUT_SECS: u32 = 30;

/// Canonical snapshot of Wi-Fi state from IWD.
#[derive(Debug, Clone)]
pub struct IwdSnapshot {
    pub available: bool,
    pub ssid: Option<String>,
    /// IWD station state.
    pub state: Option<StationState>,
    pub wifi_enabled: Option<bool>,
    pub scanning: bool,
    pub networks: Vec<WifiNetwork>,
    pub auth_request: Option<IwdAuthRequest>,
    pub failed_ssid: Option<String>,
    /// Human-readable reason for the last connection failure.
    pub failed_reason: Option<String>,
    /// Whether initial scan has completed (for is_ready check).
    pub initial_scan_complete: bool,
}

impl IwdSnapshot {
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

    pub fn connected(&self) -> bool {
        matches!(
            self.state,
            Some(StationState::Connected | StationState::Roaming)
        )
    }

    pub fn connecting(&self) -> bool {
        self.state == Some(StationState::Connecting)
    }
}

/// Shared, process-wide IWD service for Wi-Fi state and control.
///
/// # Thread Safety
/// All public methods are designed to be called from the GLib main loop.
/// Synchronous D-Bus operations spawn background threads and use
/// `glib::idle_add_once()` to deliver results to the main thread.
pub struct IwdService {
    snapshot: RefCell<IwdSnapshot>,
    callbacks: Callbacks<IwdSnapshot>,
    adapter_path: RefCell<Option<String>>,
    adapter_proxy: RefCell<Option<gio::DBusProxy>>,
    connection: RefCell<Option<gio::DBusConnection>>,
    station_proxy: RefCell<Option<gio::DBusProxy>>,
    station_path: RefCell<Option<String>>,
    agent_registration_id: RefCell<Option<gio::RegistrationId>>,
    /// Whether the agent is registered with IWD's AgentManager (tracked separately from
    /// D-Bus object registration because `RegisterAgent` can fail independently).
    agent_registered_with_manager: Cell<bool>,
    pending_auth: RefCell<Option<PendingAuth>>,
    scan_in_progress: Cell<bool>,
    /// Guards against redundant concurrent network refresh threads.
    refresh_in_progress: Cell<bool>,
    /// Set when a refresh is requested while one is already in progress.
    refresh_needed: Cell<bool>,
    /// Whether a debounced network refresh is pending (from IFACE_NETWORK signals).
    network_refresh_pending: Cell<bool>,
    /// SSID resolution retry counter, capped at MAX_SSID_RESOLVE_ATTEMPTS.
    ssid_resolve_attempts: Cell<u8>,
    watcher_proxy: RefCell<Option<gio::DBusProxy>>,
    /// D-Bus signal subscriptions (kept alive for the service lifetime).
    _signal_subscriptions: RefCell<Vec<gio::SignalSubscription>>,
    /// Network (path, SSID) being connected to (C5 race guard for password dialog).
    connecting_network: RefCell<Option<(String, String)>>,
    /// Stashed password for IWD retry flow. Auto-submitted on next RequestPassphrase
    /// instead of re-prompting.
    pending_password: RefCell<Option<String>>,
    /// Generation counter incremented on station/proxy teardown; background
    /// threads capture the current value so stale results are discarded.
    generation: Cell<u64>,
    /// Set when our agent returns AGENT_ERROR_CANCELED (user cancel, timeout,
    /// race guard). Checked in the Connect() error handler to distinguish
    /// our cancels from IWD's wrong-password agent errors.
    ///
    /// This is a global flag (not per-connection) which is safe because:
    /// 1. The UI disables the connect button while a connection is in progress
    /// 2. IWD rejects concurrent connects with InProgress error
    /// 3. GTK is single-threaded so set/check cannot race
    agent_canceled_by_us: Cell<bool>,
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
            agent_registered_with_manager: Cell::new(false),
            pending_auth: RefCell::new(None),
            scan_in_progress: Cell::new(false),
            refresh_in_progress: Cell::new(false),
            refresh_needed: Cell::new(false),
            network_refresh_pending: Cell::new(false),
            ssid_resolve_attempts: Cell::new(0),
            watcher_proxy: RefCell::new(None),
            _signal_subscriptions: RefCell::new(Vec::new()),
            connecting_network: RefCell::new(None),
            pending_password: RefCell::new(None),
            generation: Cell::new(0),
            agent_canceled_by_us: Cell::new(false),
        });

        Self::init_dbus(&service);

        service
    }

    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<IwdService> = IwdService::new();
        }

        INSTANCE.with(|s| s.clone())
    }

    /// Register a callback to be invoked whenever the network state changes.
    pub fn connect<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&IwdSnapshot) + 'static,
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
                        let iface_name: Option<String> = signal.parameters.child_value(0).get();
                        match iface_name.as_deref() {
                            Some(IFACE_ADAPTER) => this.update_adapter_state(),
                            Some(IFACE_STATION) => this.update_station_state(),
                            _ => {}
                        }
                    },
                );

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
                    DBUS_PEER_IFACE,
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
            let path: Option<String> = objpath_to_string(&entry.child_value(0));
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

    fn handle_interfaces_added(this: &Rc<Self>, params: &Variant) {
        // InterfacesAdded(OBJPATH, DICT<STRING,DICT<STRING,VARIANT>>)
        let path: Option<String> = objpath_to_string(&params.child_value(0));
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
                Some(IFACE_NETWORK) => {
                    Self::schedule_network_refresh(this);
                }
                _ => {}
            }
        }
    }

    fn handle_interfaces_removed(this: &Rc<Self>, params: &Variant) {
        // InterfacesRemoved(OBJPATH, ARRAY<STRING>)
        let path: Option<String> = objpath_to_string(&params.child_value(0));
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
                Some(IFACE_NETWORK) => {
                    Self::schedule_network_refresh(this);
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

    fn clear_proxies(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        *self.adapter_proxy.borrow_mut() = None;
        *self.station_proxy.borrow_mut() = None;
        *self.adapter_path.borrow_mut() = None;
        *self.station_path.borrow_mut() = None;
        *self.connecting_network.borrow_mut() = None;
        *self.pending_password.borrow_mut() = None;

        // Cancel any pending auth (including its timeout)
        if let Some(mut pending) = self.pending_auth.borrow_mut().take() {
            if let Some(source_id) = pending.timeout_source.take() {
                source_id.remove();
            }
            pending
                .invocation
                .return_dbus_error(AGENT_ERROR_CANCELED, "Service unavailable");
            self.agent_canceled_by_us.set(true);
        }

        // Reset all operational flags (superset of clear_station's resets).
        self.scan_in_progress.set(false);
        self.refresh_in_progress.set(false);
        self.refresh_needed.set(false);
        self.network_refresh_pending.set(false);
        self.ssid_resolve_attempts.set(0);

        // Unregister agent D-Bus object so re-registration succeeds on service reappear
        if let (Some(conn), Some(reg_id)) = (
            self.connection.borrow().clone(),
            self.agent_registration_id.borrow_mut().take(),
        ) {
            let _ = conn.unregister_object(reg_id);
        }
        self.agent_registered_with_manager.set(false);
    }

    /// Clear only station-related state when WiFi is powered off.
    ///
    /// Unlike `clear_proxies()`, this preserves the adapter proxy and path so
    /// that `set_wifi_enabled(true)` can still reach the adapter.
    fn clear_station(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        *self.station_proxy.borrow_mut() = None;
        *self.station_path.borrow_mut() = None;
        *self.connecting_network.borrow_mut() = None;
        *self.pending_password.borrow_mut() = None;

        // Reset operational flags (e.g., scan/refresh stuck mid-operation).
        self.scan_in_progress.set(false);
        self.refresh_in_progress.set(false);
        self.refresh_needed.set(false);
        self.network_refresh_pending.set(false);
        self.ssid_resolve_attempts.set(0);

        // Cancel any pending auth (including its timeout — station is gone, auth can't succeed)
        if let Some(mut pending) = self.pending_auth.borrow_mut().take() {
            if let Some(source_id) = pending.timeout_source.take() {
                source_id.remove();
            }
            pending
                .invocation
                .return_dbus_error(AGENT_ERROR_CANCELED, "WiFi powered off");
            self.agent_canceled_by_us.set(true);
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
            IwdUpdate::NetworksRefreshed {
                networks,
                generation,
            } => {
                if generation != this.generation.get() {
                    this.refresh_in_progress.set(false);
                    debug!(
                        "Discarding stale NetworksRefreshed (gen {} != {})",
                        generation,
                        this.generation.get()
                    );
                    // Drain queued refresh so it isn't silently lost
                    if this.refresh_needed.replace(false) {
                        this.refresh_networks_async(false);
                    }
                    return;
                }
                this.refresh_in_progress.set(false);
                let mut snapshot = this.snapshot.borrow_mut();
                snapshot.networks = networks;
                snapshot.initial_scan_complete = true;
                let snapshot_clone = snapshot.clone();
                drop(snapshot);
                this.callbacks.notify(&snapshot_clone);
                this.update_station_state();

                if this.refresh_needed.replace(false) {
                    this.refresh_networks_async(false);
                }
            }
            IwdUpdate::ConnectionFailed {
                ssid,
                reason,
                generation,
            } => {
                if generation != this.generation.get() {
                    return;
                }
                let mut snapshot = this.snapshot.borrow_mut();
                let already_has_specific_reason = snapshot.failed_ssid.as_deref()
                    == Some(ssid.as_str())
                    && snapshot.failed_reason.is_some();
                if !already_has_specific_reason {
                    snapshot.failed_ssid = Some(ssid);
                    snapshot.failed_reason = Some(reason);
                }

                if snapshot.state == Some(StationState::Connecting) {
                    snapshot.state = Some(StationState::Disconnected);
                    snapshot.ssid = None;
                }
                *this.connecting_network.borrow_mut() = None;
                *this.pending_password.borrow_mut() = None;

                let snapshot_clone = snapshot.clone();
                drop(snapshot);
                this.callbacks.notify(&snapshot_clone);
            }
            IwdUpdate::RefreshFailed { generation } => {
                if generation != this.generation.get() {
                    return;
                }
                this.refresh_in_progress.set(false);
                if this.refresh_needed.replace(false) {
                    this.refresh_networks_async(false);
                }
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
            gio::DBusProxyFlags::DO_NOT_CONNECT_SIGNALS,
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

    fn setup_proxy<F, E>(
        this: &Rc<Self>,
        path: &str,
        interface: &'static str,
        on_ready: F,
        on_error: E,
    ) where
        F: FnOnce(&Rc<Self>, gio::DBusProxy) + 'static,
        E: FnOnce(&Rc<Self>) + 'static,
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
                        on_error(&this);
                        return;
                    }
                };

                on_ready(&this, proxy);
            },
        )
    }

    fn setup_adapter_proxy(this: &Rc<Self>, path: &str) {
        Self::setup_proxy(
            this,
            path,
            IFACE_ADAPTER,
            |this, proxy| {
                this.adapter_proxy.replace(Some(proxy.clone()));

                let this_weak = Rc::downgrade(this);
                proxy.connect_local("g-properties-changed", false, move |_| {
                    if let Some(this) = this_weak.upgrade() {
                        this.update_adapter_state();
                    }
                    None
                });

                this.update_adapter_state();
            },
            |this| {
                *this.adapter_path.borrow_mut() = None;
                let mut snapshot = this.snapshot.borrow_mut();
                if this.station_path.borrow().is_none() {
                    snapshot.available = false;
                }
                let snapshot_clone = snapshot.clone();
                drop(snapshot);
                this.callbacks.notify(&snapshot_clone);
            },
        );
    }

    fn setup_station_proxy(this: &Rc<Self>, path: &str) {
        Self::setup_proxy(
            this,
            path,
            IFACE_STATION,
            |this, proxy| {
                this.station_proxy.replace(Some(proxy.clone()));

                let this_weak = Rc::downgrade(this);
                proxy.connect_local("g-properties-changed", false, move |_| {
                    if let Some(this) = this_weak.upgrade() {
                        this.update_station_state();
                    }
                    None
                });

                this.update_station_state();
            },
            |this| {
                *this.station_path.borrow_mut() = None;
                let mut snapshot = this.snapshot.borrow_mut();
                if this.adapter_path.borrow().is_none() {
                    snapshot.available = false;
                }
                let snapshot_clone = snapshot.clone();
                drop(snapshot);
                this.callbacks.notify(&snapshot_clone);
            },
        );
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
            .and_then(|v| v.get::<String>())
            .map(|s| StationState::from_dbus(&s));

        let scanning = proxy
            .cached_property("Scanning")
            .and_then(|v| v.get::<bool>())
            .unwrap_or(false);

        let name_path = proxy
            .cached_property("ConnectedNetwork")
            .and_then(|v| objpath_to_string(&v));

        let is_connected_or_connecting = matches!(
            state,
            Some(StationState::Connected | StationState::Connecting | StationState::Roaming)
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
        let (should_fetch_networks, scan_just_completed, is_ssid_resolve) = {
            let snap = self.snapshot.borrow();
            let was_connected = matches!(
                snap.state,
                Some(StationState::Connected | StationState::Roaming)
            );
            let is_connected =
                matches!(state, Some(StationState::Connected | StationState::Roaming));
            let needs_fetch = snap.networks.is_empty()
                || (!was_connected && is_connected)
                || (is_connected && ssid.is_none());

            // If we need to fetch because SSID is unresolved while connected,
            // check the retry limit to avoid an infinite loop.
            let is_ssid_resolve = needs_fetch && is_connected && ssid.is_none();
            let should_fetch = if is_ssid_resolve {
                let attempts = self.ssid_resolve_attempts.get();
                if attempts >= MAX_SSID_RESOLVE_ATTEMPTS {
                    warn!(
                        "SSID resolution failed after {} attempts, giving up",
                        attempts
                    );
                    false
                } else {
                    // Don't increment here — let refresh_networks_async() do it
                    // so the counter only advances when a refresh actually starts
                    // (not when skipped due to refresh_in_progress guard).
                    true
                }
            } else {
                needs_fetch
            };

            let scan_done = snap.scanning && !scanning;

            (should_fetch, scan_done, is_ssid_resolve)
        };

        // Clear our scan_in_progress guard when IWD reports scan complete
        if scan_just_completed {
            self.scan_in_progress.set(false);
        }

        let mut snapshot = self.snapshot.borrow_mut();
        let mut changed = false;
        if snapshot.state != state {
            // Clear the connecting_network race guard when the connection
            // attempt finishes (success or failure). This prevents stale
            // paths from rejecting future auth requests.
            if matches!(
                state,
                Some(StationState::Connected | StationState::Disconnected) | None
            ) {
                *self.connecting_network.borrow_mut() = None;
                *self.pending_password.borrow_mut() = None;
            }
            snapshot.state = state;
            changed = true;
        }
        if snapshot.ssid != ssid {
            snapshot.ssid = ssid;
            changed = true;

            // Only clear active network flags when truly disconnected.
            // When Connected/Roaming, SSID may be temporarily None during
            // the SSID resolve window — don't flicker the UI in that case.
            if snapshot.ssid.is_none()
                && !matches!(
                    snapshot.state,
                    Some(StationState::Connected | StationState::Roaming)
                )
            {
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
            self.refresh_networks_async(is_ssid_resolve && should_fetch_networks);
        }
    }

    pub fn set_wifi_enabled(&self, enabled: bool) {
        let Some(proxy) = self.adapter_proxy.borrow().clone() else {
            debug!("set_wifi_enabled called but adapter_proxy is None");
            return;
        };
        debug!("set_wifi_enabled: setting Powered to {}", enabled);
        std::thread::spawn(move || {
            // Set Powered via D-Bus Properties.Set (ssv signature). Double to_variant() boxes the bool as (v).
            let variant = Variant::tuple_from_iter([
                IFACE_ADAPTER.to_variant(),
                "Powered".to_variant(),
                enabled.to_variant().to_variant(), // double to_variant() boxes the bool as (v)
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
        self.agent_canceled_by_us.set(false);

        // Resolve the SSID from the cached network list before spawning the
        // background thread. This is passed as a fallback for error reporting.
        let cached_ssid;
        {
            let mut snapshot = self.snapshot.borrow_mut();

            // Clear any previous failed state
            snapshot.failed_ssid = None;
            snapshot.failed_reason = None;

            // Set connecting state immediately for UI feedback (before the
            // background thread calls Network.Connect and IWD's PropertiesChanged
            // signal arrives). This matches NetworkManager's behavior where
            // connecting_ssid is set before spawning.
            let ssid = snapshot
                .networks
                .iter()
                .find(|n| n.path.as_deref() == Some(path))
                .map(|n| n.ssid.clone());

            // C5: Store the network (path, SSID) we're connecting to for race guard
            // in handle_request_passphrase(). The SSID is also used as a fallback
            // when the cached network list is stale during agent auth.
            *self.connecting_network.borrow_mut() =
                Some((path.to_string(), ssid.clone().unwrap_or_default()));

            // Always set Connecting state for UI feedback, even if the SSID
            // isn't in our cached network list (it may be stale).
            snapshot.state = Some(StationState::Connecting);
            // Use cached SSID if available, otherwise clear to avoid
            // displaying a stale SSID from the previous connection.
            snapshot.ssid = ssid.clone();

            cached_ssid = ssid;
            let snapshot_clone = snapshot.clone();
            drop(snapshot);
            self.callbacks.notify(&snapshot_clone);
        }

        let path = path.to_string();
        let current_gen = self.generation.get();
        std::thread::spawn(move || {
            // Using a fallback ensures ConnectionFailed is always sent even if
            // read_network_name() fails (e.g., IWD service is flaky), preventing
            // the UI from getting stuck in "Connecting" state.
            let ssid = Self::read_network_name(&path)
                .or(cached_ssid)
                .unwrap_or_else(|| "Unknown network".to_string());

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
                    send_network_update(IwdUpdate::ConnectionFailed {
                        ssid,
                        reason: "Network not found".to_string(),
                        generation: current_gen,
                    });
                    return;
                }
            };
            if let Err(e) = proxy.call_sync(
                "Connect",
                None,
                gio::DBusCallFlags::NONE,
                120000,
                None::<&gio::Cancellable>,
            ) {
                // Check the error type to determine how to handle it
                let error_name = gio::DBusError::remote_error(&e);
                let error_name_str = error_name.as_ref().map(|s| s.as_str()).unwrap_or("");

                // Agent errors mean IWD returned Agent.Error.Canceled from
                // Connect(). This happens both for wrong passwords (4-way
                // handshake failed) and for our own cancellations (user cancel,
                // timeout, race guard, etc.). Use agent_canceled_by_us to
                // distinguish: if we didn't cancel, it's a wrong password.
                let is_agent_error = error_name_str.starts_with("net.connman.iwd.Agent");

                let is_transient_error = matches!(
                    error_name_str,
                    "net.connman.iwd.Aborted"
                        | "net.connman.iwd.InProgress"
                        | "net.connman.iwd.NotAvailable"
                );

                if is_transient_error {
                    debug!("Connect got transient error '{}': {}", error_name_str, e);
                    // InProgress means another connect is already in flight —
                    // that attempt will resolve the state via PropertiesChanged.
                    // NotAvailable/Aborted may not trigger a PropertiesChanged,
                    // so send ConnectionFailed to reset the UI and avoid being
                    // stuck in "Connecting..." indefinitely.
                    if error_name_str != "net.connman.iwd.InProgress" {
                        send_network_update(IwdUpdate::ConnectionFailed {
                            ssid,
                            reason: super::CONNECTION_FAILURE_REASON.to_string(),
                            generation: current_gen,
                        });
                    }
                } else if is_agent_error {
                    let ssid_for_auth = ssid;
                    glib::idle_add_once(move || {
                        let service = IwdService::global();
                        let we_canceled = service.agent_canceled_by_us.replace(false);
                        if !we_canceled {
                            IwdService::apply_update(
                                &service,
                                IwdUpdate::ConnectionFailed {
                                    ssid: ssid_for_auth,
                                    reason: super::AUTH_FAILURE_REASON.to_string(),
                                    generation: current_gen,
                                },
                            );
                        }
                    });
                } else {
                    warn!("Connect failed: {} (error: {})", e, error_name_str);
                    let reason = match error_name_str {
                        "net.connman.iwd.NotFound" => "Network not found".to_string(),
                        "net.connman.iwd.Failed" => super::CONNECTION_FAILURE_REASON.to_string(),
                        "net.connman.iwd.NotSupported" => "Not supported".to_string(),
                        "net.connman.iwd.NotConfigured" => "Not configured".to_string(),
                        "net.connman.iwd.PermissionDenied" => "Permission denied".to_string(),
                        "net.connman.iwd.NotConnected" => "Not connected".to_string(),
                        _ => super::CONNECTION_FAILURE_REASON.to_string(),
                    };
                    send_network_update(IwdUpdate::ConnectionFailed {
                        ssid,
                        reason,
                        generation: current_gen,
                    });
                }
            }
        });
    }
    pub fn disconnect(&self) {
        self.cancel_auth();

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
                    service.refresh_networks_async(false);
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

        // Optimistically notify UI that scanning has started, so the scan button
        // reflects the action immediately rather than waiting for the D-Bus
        // Station.Scanning property change signal (~10-50ms lag).
        {
            let mut snapshot = self.snapshot.borrow_mut();
            if !snapshot.scanning {
                snapshot.scanning = true;
                let snapshot_clone = snapshot.clone();
                drop(snapshot);
                self.callbacks.notify(&snapshot_clone);
            }
        }

        // Use async call to avoid blocking the main thread
        proxy.call(
            "Scan",
            None,
            gio::DBusCallFlags::NONE,
            30000, // 30 second timeout for scan
            None::<&gio::Cancellable>,
            |res| {
                // Callback runs on main thread — safe to access global directly.
                // Always clear scan_in_progress here: if IWD was already scanning
                // when we called Scan, the Scanning property won't toggle from
                // true→false, so the property-change handler in update_station_state
                // would never clear the flag.
                IwdService::global().scan_in_progress.set(false);
                if let Err(e) = res {
                    debug!("Scan failed: {}", e);
                }
            },
        );
    }

    /// Schedule a debounced network list refresh (100ms delay).
    ///
    /// Called when ObjectManager signals indicate a Network object was added
    /// or removed (e.g., during forget, or when IWD re-creates Network objects
    /// after a scan). The delay coalesces rapid signals to avoid redundant
    /// D-Bus calls.
    fn schedule_network_refresh(this: &Rc<Self>) {
        if this.network_refresh_pending.get() {
            return;
        }
        this.network_refresh_pending.set(true);
        let weak = Rc::downgrade(this);
        glib::timeout_add_local_once(
            Duration::from_millis(NETWORK_REFRESH_DEBOUNCE_MS),
            move || {
                if let Some(this) = weak.upgrade() {
                    this.network_refresh_pending.set(false);
                    this.refresh_networks_async(false);
                }
            },
        );
    }

    fn refresh_networks_async(&self, is_ssid_resolve: bool) {
        if self.refresh_in_progress.get() {
            self.refresh_needed.set(true);
            return;
        }

        let Some(proxy) = self.station_proxy.borrow().clone() else {
            return;
        };

        self.refresh_in_progress.set(true);

        // Only increment the SSID resolve counter when a refresh actually
        // starts (not when skipped by the refresh_in_progress guard above).
        if is_ssid_resolve {
            let attempts = self.ssid_resolve_attempts.get();
            self.ssid_resolve_attempts.set(attempts + 1);
        }

        // I4: If this is an SSID resolve retry (attempts > 0), add a small backoff
        // delay in the background thread to avoid hammering D-Bus.
        let resolve_attempts = self.ssid_resolve_attempts.get();
        let current_gen = self.generation.get();

        std::thread::spawn(move || {
            if resolve_attempts > 0 {
                let delay_ms = 200 * (resolve_attempts as u64);
                std::thread::sleep(Duration::from_millis(delay_ms));
            }
            if let Some(networks) = Self::get_networks_sync(&proxy) {
                send_network_update(IwdUpdate::NetworksRefreshed {
                    networks,
                    generation: current_gen,
                });
            } else {
                send_network_update(IwdUpdate::RefreshFailed {
                    generation: current_gen,
                });
            }
        });
    }

    fn get_networks_sync(proxy: &gio::DBusProxy) -> Option<Vec<WifiNetwork>> {
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
                return None;
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

            let path: String = match objpath_to_string(&tuple.child_value(0)) {
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
                SecurityType::Open
            } else {
                SecurityType::Secured
            };
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
        Some(networks)
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
            let path: Option<String> = objpath_to_string(&entry.child_value(0));
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
                        "KnownNetwork" => {
                            known_network_path =
                                objpath_to_string(&inner).filter(|p| p != "/" && !p.is_empty())
                        }
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
        // D-Bus object already exported — only retry AgentManager registration if needed
        if this.agent_registration_id.borrow().is_some() {
            if !this.agent_registered_with_manager.get() {
                debug!(
                    "IwdService: agent object exists but not registered with AgentManager, retrying"
                );
                if let Some(connection) = this.connection.borrow().clone() {
                    Self::register_with_agent_manager(this, &connection);
                }
            }
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
    ///
    /// On transient failure (I2), retries once after a short delay.
    /// On `AlreadyExists` (I3), attempts `UnregisterAgent` followed by `RegisterAgent`.
    fn register_with_agent_manager(this: &Rc<Self>, connection: &gio::DBusConnection) {
        Self::register_with_agent_manager_inner(this, connection, false);
    }

    /// Inner implementation for agent manager registration.
    /// `is_retry` prevents infinite retry loops — only one retry is attempted.
    fn register_with_agent_manager_inner(
        this: &Rc<Self>,
        connection: &gio::DBusConnection,
        is_retry: bool,
    ) {
        let this_weak = Rc::downgrade(this);
        let conn_clone = connection.clone();

        gio::DBusProxy::new(
            connection,
            gio::DBusProxyFlags::NONE,
            None,
            Some(IWD_SERVICE),
            IWD_ROOT_PATH,
            AGENT_MANAGER_IFACE,
            None::<&gio::Cancellable>,
            move |res| {
                let Some(this) = this_weak.upgrade() else {
                    return;
                };

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

                let this_weak = Rc::downgrade(&this);
                let proxy_clone = proxy.clone();
                proxy.call(
                    "RegisterAgent",
                    Some(&args),
                    gio::DBusCallFlags::NONE,
                    5000,
                    None::<&gio::Cancellable>,
                    move |res| {
                        let Some(this) = this_weak.upgrade() else {
                            return;
                        };

                        match res {
                            Ok(_) => {
                                this.agent_registered_with_manager.set(true);
                                debug!("IwdService: agent registered with AgentManager");
                            }
                            Err(e) => {
                                let error_name = gio::DBusError::remote_error(&e);
                                let error_str =
                                    error_name.as_ref().map(|s| s.as_str()).unwrap_or("");

                                if error_str == "net.connman.iwd.AlreadyExists" {
                                    // I3: Agent name conflict — attempt UnregisterAgent + re-register
                                    if !is_retry {
                                        debug!("IwdService: agent already registered, unregistering and retrying");
                                        Self::unregister_and_reregister(&this, &proxy_clone, &conn_clone);
                                    } else {
                                        // Already retried — accept as registered
                                        this.agent_registered_with_manager.set(true);
                                        debug!("IwdService: agent already registered (retry), treating as success");
                                    }
                                } else if !is_retry {
                                    // I2: Transient failure — retry once after 2s
                                    warn!("IwdService: RegisterAgent failed: {}, retrying in 2s", e);
                                    let this_weak = Rc::downgrade(&this);
                                    glib::timeout_add_local_once(
                                        Duration::from_secs(2),
                                        move || {
                                            let Some(this) = this_weak.upgrade() else {
                                                return;
                                            };
                                            if let Some(connection) =
                                                this.connection.borrow().clone()
                                            {
                                                Self::register_with_agent_manager_inner(
                                                    &this,
                                                    &connection,
                                                    true,
                                                );
                                            }
                                        },
                                    );
                                } else {
                                    error!(
                                        "IwdService: RegisterAgent retry also failed: {}",
                                        e
                                    );
                                }
                            }
                        }
                    },
                );
            },
        );
    }

    /// Unregister our agent from IWD's AgentManager, then re-register (I3).
    fn unregister_and_reregister(
        this: &Rc<Self>,
        proxy: &gio::DBusProxy,
        connection: &gio::DBusConnection,
    ) {
        let agent_path = glib::variant::ObjectPath::try_from(AGENT_PATH)
            .expect("AGENT_PATH constant must be a valid D-Bus object path");
        let args = (agent_path,).to_variant();

        let this_weak = Rc::downgrade(this);
        let conn_clone = connection.clone();
        proxy.call(
            "UnregisterAgent",
            Some(&args),
            gio::DBusCallFlags::NONE,
            5000,
            None::<&gio::Cancellable>,
            move |res| {
                if let Err(e) = res {
                    debug!(
                        "IwdService: UnregisterAgent failed (may be expected): {}",
                        e
                    );
                }
                let Some(this) = this_weak.upgrade() else {
                    return;
                };
                // Re-register after unregister (mark as retry to prevent loops)
                Self::register_with_agent_manager_inner(&this, &conn_clone, true);
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
                this.agent_registered_with_manager.set(false);
                invocation.return_value(None);
            }
            "Cancel" => {
                // Reason is a fixed set from IWD agent.c: "user-canceled",
                // "timed-out", "out-of-range", "shutdown", "unknown".
                // Wrong-password failures come via Connect() errors, not Cancel.
                let reason: String = params.child_value(0).get().unwrap_or_default();
                debug!("IwdService: auth cancelled by IWD, reason: '{}'", reason);

                let cancel_ssid = this
                    .snapshot
                    .borrow()
                    .auth_request
                    .as_ref()
                    .map(|a| a.ssid.clone())
                    .or_else(|| {
                        this.connecting_network
                            .borrow()
                            .as_ref()
                            .map(|(_, ssid)| ssid.clone())
                    });

                match reason.as_str() {
                    "timed-out" => {
                        if let Some(ssid) = &cancel_ssid {
                            this.set_failed_ssid(ssid, "Authentication timed out");
                        }
                    }
                    "out-of-range" => {
                        if let Some(ssid) = &cancel_ssid {
                            this.set_failed_ssid(ssid, "Network out of range");
                        }
                    }
                    // User-initiated cancel and daemon shutdown are intentionally
                    // silent — user knows they canceled, and shutdown is handled
                    // by the name-owner disappearance handler.
                    "user-canceled" | "shutdown" => {}
                    _ => {
                        if let Some(ssid) = &cancel_ssid {
                            this.set_failed_ssid(ssid, super::CONNECTION_FAILURE_REASON);
                        }
                    }
                }
                if let Some(mut pending) = this.pending_auth.borrow_mut().take() {
                    if let Some(source_id) = pending.timeout_source.take() {
                        source_id.remove();
                    }
                    pending
                        .invocation
                        .return_dbus_error(AGENT_ERROR_CANCELED, "Canceled");
                    this.agent_canceled_by_us.set(true);
                }
                // Clear auth request and connection guard from snapshot
                *this.connecting_network.borrow_mut() = None;
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
                // Enterprise auth - not supported yet.
                // Extract SSID from the network path so the UI can show which
                // network failed and why.
                warn!("IwdService: enterprise auth not supported");
                if let Some(network_path) = objpath_to_string(&params.child_value(0)) {
                    let ssid = this
                        .snapshot
                        .borrow()
                        .networks
                        .iter()
                        .find(|n| n.path.as_deref() == Some(network_path.as_str()))
                        .map(|n| n.ssid.clone())
                        .or_else(|| {
                            this.connecting_network
                                .borrow()
                                .as_ref()
                                .map(|(_, s)| s.clone())
                        })
                        .unwrap_or_else(|| "Unknown".to_string());
                    this.set_failed_ssid(&ssid, "Enterprise WiFi not supported");
                }
                invocation.return_dbus_error(
                    AGENT_ERROR_CANCELED,
                    "Enterprise authentication not supported",
                );
                this.agent_canceled_by_us.set(true);
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
    /// Includes a race guard (C5): rejects the request if the network path
    /// doesn't match what `connect_to_network()` initiated. Also starts a
    /// timeout (C4) that auto-cancels the pending auth after `AUTH_TIMEOUT_SECS`.
    fn handle_request_passphrase(
        this: &Rc<Self>,
        params: Variant,
        invocation: gio::DBusMethodInvocation,
    ) {
        // Extract network path from params
        let network_path: String = match objpath_to_string(&params.child_value(0)) {
            Some(p) => p,
            None => {
                error!("IwdService: RequestPassphrase missing network path");
                invocation.return_error(gio::IOErrorEnum::InvalidArgument, "Missing network path");
                return;
            }
        };

        // C5: Race guard — verify incoming network path matches the one we initiated.
        // This prevents a stale or rogue auth request from a different connection
        // attempt from being shown to the user.
        let connecting = this.connecting_network.borrow().clone();
        if let Some((ref expected_path, _)) = connecting
            && *expected_path != network_path
        {
            warn!(
                "IwdService: RequestPassphrase for unexpected network {} (expected {}), rejecting",
                network_path, expected_path
            );
            invocation.return_dbus_error(AGENT_ERROR_CANCELED, "Unexpected network");
            this.agent_canceled_by_us.set(true);
            return;
        }

        // Get SSID from cached network list. If the list is stale, fall back
        // to the SSID we stored at connect-initiation time to avoid showing
        // "Unknown" in the password dialog.
        let fallback_ssid = connecting.map(|(_, ssid)| ssid).filter(|s| !s.is_empty());
        let ssid = this
            .snapshot
            .borrow()
            .networks
            .iter()
            .find(|n| n.path.as_deref() == Some(network_path.as_str()))
            .map(|n| n.ssid.clone())
            .or(fallback_ssid)
            .unwrap_or_else(|| "Unknown".to_string());

        debug!(
            "IwdService: RequestPassphrase for network '{}' ({})",
            ssid, network_path
        );

        // Cancel any existing pending auth (including its timeout)
        Self::cancel_pending_auth_timeout(this);

        // C4: Start a timeout that auto-cancels the auth after AUTH_TIMEOUT_SECS.
        let this_weak = Rc::downgrade(this);
        let timeout_source = glib::timeout_add_local_once(
            Duration::from_secs(AUTH_TIMEOUT_SECS as u64),
            move || {
                let Some(this) = this_weak.upgrade() else {
                    return;
                };
                warn!(
                    "IwdService: auth request timed out after {}s",
                    AUTH_TIMEOUT_SECS
                );

                // Take the pending auth and return an error to IWD
                if let Some(mut pending) = this.pending_auth.borrow_mut().take() {
                    // Clear the timeout source since we're handling it now
                    pending.timeout_source = None;
                    pending
                        .invocation
                        .return_dbus_error(AGENT_ERROR_CANCELED, "Authentication timed out");
                    this.agent_canceled_by_us.set(true);
                }

                // Get the SSID before clearing the race guard, for the error message
                let ssid = this
                    .connecting_network
                    .borrow()
                    .as_ref()
                    .map(|(_, ssid)| ssid.clone());

                // Clear the race guard on timeout
                *this.connecting_network.borrow_mut() = None;

                // Show timeout error to the user
                if let Some(ssid) = ssid {
                    this.set_failed_ssid(&ssid, "Authentication timed out");
                }

                // Clear auth state and notify UI
                this.clear_auth_state();
            },
        );

        // Store the pending auth with timeout
        *this.pending_auth.borrow_mut() = Some(PendingAuth {
            invocation,
            timeout_source: Some(timeout_source),
        });

        // Check for a stashed password (from a wrong-password retry via
        // connect_to_network). If present, auto-submit instead of showing
        // the password dialog again.
        // SAFETY: Take the value and drop the RefMut before calling submit_passphrase,
        // which borrows other RefCells. This prevents holding the borrow across the call.
        let stashed_password = this.pending_password.borrow_mut().take();
        if let Some(password) = stashed_password {
            debug!(
                "IwdService: auto-submitting stashed password for '{}'",
                ssid
            );
            // submit_passphrase takes pending_auth, cancels timeout, and
            // clears auth_request — no UI flash.
            this.submit_passphrase(&password);
            return;
        }

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

    /// Cancel the timeout on an existing pending auth (if any) and return
    /// the D-Bus error before replacing it.
    fn cancel_pending_auth_timeout(this: &Rc<Self>) {
        if let Some(mut pending) = this.pending_auth.borrow_mut().take() {
            if let Some(source_id) = pending.timeout_source.take() {
                source_id.remove();
            }
            pending
                .invocation
                .return_dbus_error(AGENT_ERROR_CANCELED, "Superseded by new auth request");
            this.agent_canceled_by_us.set(true);
        }
    }

    /// Stash a password for the next `RequestPassphrase` callback. When IWD
    /// fires the agent callback, `handle_request_passphrase` will auto-submit
    /// this password instead of showing the password dialog (avoiding the
    /// double-prompt issue on wrong-password retry).
    pub fn set_pending_password(&self, password: Option<&str>) {
        *self.pending_password.borrow_mut() = password.map(|p| p.to_string());
    }

    /// Submit a passphrase in response to a pending auth request.
    pub fn submit_passphrase(&self, passphrase: &str) {
        let mut pending = match self.pending_auth.borrow_mut().take() {
            Some(p) => p,
            None => {
                debug!("IwdService: submit_passphrase called but no pending auth");
                return;
            }
        };

        // Cancel the auth timeout
        if let Some(source_id) = pending.timeout_source.take() {
            source_id.remove();
        }

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
        let mut pending = match self.pending_auth.borrow_mut().take() {
            Some(p) => p,
            None => {
                // No pending auth - this is normal during cleanup
                return;
            }
        };

        // Cancel the auth timeout
        if let Some(source_id) = pending.timeout_source.take() {
            source_id.remove();
        }

        debug!("IwdService: cancelling auth");

        // Return error to IWD
        pending
            .invocation
            .return_dbus_error(AGENT_ERROR_CANCELED, "User canceled");
        self.agent_canceled_by_us.set(true);

        // Clear the race guard — user cancelled, so any future auth for a
        // different network should be accepted.
        *self.connecting_network.borrow_mut() = None;

        // Clear any stashed password to avoid leaking it to a future connection.
        *self.pending_password.borrow_mut() = None;

        // Clear auth request from snapshot
        self.clear_auth_state();
    }

    /// Clear the auth request from snapshot and notify.
    ///
    /// # Re-entrancy safety
    /// The `borrow_mut` on `snapshot` is dropped before `callbacks.notify()`,
    /// which may re-enter this service (e.g., UI callback reads snapshot).
    /// No mutable borrow is held during the notification.
    fn clear_auth_state(&self) {
        let mut snapshot = self.snapshot.borrow_mut();
        if snapshot.auth_request.is_some() {
            snapshot.auth_request = None;
            let snapshot_clone = snapshot.clone();
            drop(snapshot); // Drop borrow before notify — callbacks may re-enter
            self.callbacks.notify(&snapshot_clone);
        }
    }

    /// Set the failed SSID with a reason and notify listeners.
    ///
    /// # Re-entrancy safety
    /// Same pattern as `clear_auth_state` — mutable borrow is dropped before
    /// `callbacks.notify()`.
    pub fn set_failed_ssid(&self, ssid: &str, reason: &str) {
        let mut snapshot = self.snapshot.borrow_mut();
        snapshot.failed_ssid = Some(ssid.to_string());
        snapshot.failed_reason = Some(reason.to_string());
        let snapshot_clone = snapshot.clone();
        drop(snapshot); // Drop borrow before notify — callbacks may re-enter
        self.callbacks.notify(&snapshot_clone);
    }

    /// Clear the failed state and notify listeners.
    ///
    /// # Re-entrancy safety
    /// Same pattern as `clear_auth_state` — mutable borrow is dropped before
    /// `callbacks.notify()`.
    pub fn clear_failed_state(&self) {
        let mut snapshot = self.snapshot.borrow_mut();
        if snapshot.failed_ssid.is_some() {
            snapshot.failed_ssid = None;
            snapshot.failed_reason = None;
            let snapshot_clone = snapshot.clone();
            drop(snapshot); // Drop borrow before notify — callbacks may re-enter
            self.callbacks.notify(&snapshot_clone);
        }
    }
}

impl Drop for IwdService {
    fn drop(&mut self) {
        debug!("IwdService: dropping, cleaning up resources");

        // Cancel any pending authentication (including its timeout)
        if let Some(mut pending) = self.pending_auth.borrow_mut().take() {
            if let Some(source_id) = pending.timeout_source.take() {
                source_id.remove();
            }
            pending
                .invocation
                .return_dbus_error(AGENT_ERROR_CANCELED, "Service shutting down");
            self.agent_canceled_by_us.set(true);
        }

        // Unregister the local agent D-Bus object. We skip the remote
        // UnregisterAgent call to IWD's AgentManager because closing the D-Bus
        // connection on exit automatically unregisters the agent on IWD's side,
        // and a sync D-Bus round-trip here could block shutdown if IWD is
        // unresponsive.
        if let Some(conn) = self.connection.borrow().as_ref()
            && let Some(reg_id) = self.agent_registration_id.borrow_mut().take()
        {
            match conn.unregister_object(reg_id) {
                Ok(()) => debug!("IwdService: unregistered agent object from D-Bus"),
                Err(e) => debug!("IwdService: failed to unregister agent object: {}", e),
            }
        }
    }
}

/// # Thread safety
/// Safe to call from any thread — `glib::idle_add_once` marshals the
/// closure to the main loop.
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
