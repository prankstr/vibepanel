//! VPNService - VPN connection management via NetworkManager over D-Bus.
//!
//! - Discovers VPN connections (WireGuard and OpenVPN) via NetworkManager
//! - Monitors active connection state changes
//! - Provides connect/disconnect operations via nmcli
//!
//! ## Architecture
//!
//! - Uses Gio's async D-Bus proxy for non-blocking operations
//! - Background threads use glib::idle_add_once() to schedule updates on the main loop
//! - Notifies listeners on the GLib main loop with canonical snapshots

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use gtk4::gio::{self, prelude::*};
use gtk4::glib::{self, Variant};
use tracing::{debug, error, warn};

use super::callbacks::Callbacks;
use super::state;

/// NetworkManager service name.
const NM_SERVICE: &str = "org.freedesktop.NetworkManager";
/// NetworkManager main object path.
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
/// NetworkManager main interface.
const NM_IFACE: &str = "org.freedesktop.NetworkManager";
/// NetworkManager Settings interface.
const NM_SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const NM_SETTINGS_IFACE: &str = "org.freedesktop.NetworkManager.Settings";
/// Connection Settings interface (per connection).
const IFACE_CONNECTION: &str = "org.freedesktop.NetworkManager.Settings.Connection";
/// Active connection interface.
const IFACE_ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";
/// D-Bus properties interface.
const IFACE_PROPS: &str = "org.freedesktop.DBus.Properties";

const VPN_TYPES: &[&str] = &["wireguard", "vpn"]; // "vpn" is OpenVPN in NM

/// Delay before refreshing connection state after activation signal.
const STATE_REFRESH_DELAY_MS: u64 = 50;

/// NetworkManager active connection states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VpnState {
    #[default]
    Unknown = 0,
    /// Connection is activating (e.g., waiting for credentials).
    Activating = 1,
    /// Connection is fully activated and connected.
    Activated = 2,
    Deactivating = 3,
    Deactivated = 4,
}

impl From<u32> for VpnState {
    fn from(value: u32) -> Self {
        match value {
            1 => VpnState::Activating,
            2 => VpnState::Activated,
            3 => VpnState::Deactivating,
            4 => VpnState::Deactivated,
            _ => VpnState::Unknown,
        }
    }
}

/// A VPN connection known to NetworkManager.
#[derive(Debug, Clone)]
pub struct VpnConnection {
    /// Connection UUID.
    pub uuid: String,
    /// Human-readable connection name.
    pub name: String,
    /// Whether this connection is currently active.
    pub active: bool,
    /// Detailed connection state (for active connections).
    pub state: VpnState,
    /// Whether autoconnect is enabled.
    pub autoconnect: bool,
    /// VPN type ("wireguard" or "vpn"/OpenVPN).
    pub vpn_type: String,
}

/// Canonical snapshot of VPN state.
#[derive(Debug, Clone)]
pub struct VpnSnapshot {
    /// Whether the NetworkManager service is available.
    pub available: bool,
    /// All known VPN connections.
    pub connections: Vec<VpnConnection>,
    /// Whether any VPN is currently active.
    pub any_active: bool,
    /// Count of active VPN connections.
    pub active_count: usize,
    /// Whether the service is ready (initial load complete).
    pub is_ready: bool,
    /// Preferred VPN UUID from last session (used for primary() selection when no VPN is active).
    pub preferred_uuid: Option<String>,
}

impl VpnSnapshot {
    /// Create an initial "unknown" snapshot.
    fn unknown() -> Self {
        Self {
            available: false,
            connections: Vec::new(),
            any_active: false,
            active_count: 0,
            is_ready: false,
            preferred_uuid: None,
        }
    }

    /// Get the primary VPN connection (first active, then preferred from last session, then first configured).
    pub fn primary(&self) -> Option<&VpnConnection> {
        // Priority: active VPN > preferred_uuid match > first configured
        self.connections
            .iter()
            .find(|c| c.active)
            .or_else(|| {
                self.preferred_uuid
                    .as_ref()
                    .and_then(|uuid| self.connections.iter().find(|c| &c.uuid == uuid))
            })
            .or_else(|| self.connections.first())
    }
}

/// Messages sent from background threads to the main thread.
#[derive(Debug)]
pub(crate) enum VpnUpdate {
    /// Full refresh of VPN connections complete.
    ConnectionsRefreshed {
        connections: Vec<VpnConnection>,
        /// Object paths of active VPN connections (for signal subscriptions).
        active_vpn_paths: Vec<String>,
    },
    /// Request a refresh (from signal handler).
    RequestRefresh,
}

/// Send a VPN update to the main thread via glib::idle_add_once.
/// This schedules the update to run on the GLib main loop without polling.
fn send_vpn_update(update: VpnUpdate) {
    glib::idle_add_once(move || {
        VpnService::global().apply_update(update);
    });
}

/// Shared, process-wide VPN service for connection state and control.
pub struct VpnService {
    connection: RefCell<Option<gio::DBusConnection>>,
    nm_proxy: RefCell<Option<gio::DBusProxy>>,
    settings_proxy: RefCell<Option<gio::DBusProxy>>,
    snapshot: RefCell<VpnSnapshot>,
    callbacks: Callbacks<VpnSnapshot>,
    refresh_pending: Cell<bool>,
    _signal_subscriptions: RefCell<Vec<gio::SignalSubscription>>,
    /// Recreated when active connections change.
    active_conn_subscriptions: RefCell<Vec<gio::SignalSubscription>>,
    last_used_uuid: RefCell<Option<String>>,
    /// Serializes D-Bus operations to prevent race conditions when rapidly toggling.
    operation_lock: Arc<Mutex<()>>,
}

impl VpnService {
    /// Create a new VPNService.
    fn new() -> Rc<Self> {
        // Load persisted state
        let persisted = state::load();
        let last_used_uuid = persisted.vpn.last_used_uuid.clone();

        debug!("VpnService: loaded last_used_uuid={:?}", last_used_uuid);

        // Create initial snapshot with preferred_uuid set
        let mut initial_snapshot = VpnSnapshot::unknown();
        initial_snapshot.preferred_uuid = last_used_uuid.clone();

        let service = Rc::new(Self {
            connection: RefCell::new(None),
            nm_proxy: RefCell::new(None),
            settings_proxy: RefCell::new(None),
            snapshot: RefCell::new(initial_snapshot),
            callbacks: Callbacks::new(),
            refresh_pending: Cell::new(false),
            _signal_subscriptions: RefCell::new(Vec::new()),
            active_conn_subscriptions: RefCell::new(Vec::new()),
            last_used_uuid: RefCell::new(last_used_uuid),
            operation_lock: Arc::new(Mutex::new(())),
        });

        // Initialize D-Bus connection.
        Self::init_dbus(&service);

        service
    }

    /// Get the global VPNService singleton.
    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<VpnService> = VpnService::new();
        }

        INSTANCE.with(|s| s.clone())
    }

    /// Register a callback to be invoked whenever the VPN state changes.
    pub fn connect<F>(&self, callback: F)
    where
        F: Fn(&VpnSnapshot) + 'static,
    {
        self.callbacks.register(callback);

        // Immediately send current snapshot.
        let snapshot = self.snapshot.borrow().clone();
        self.callbacks.notify(&snapshot);
    }

    /// Return the current VPN snapshot.
    pub fn snapshot(&self) -> VpnSnapshot {
        self.snapshot.borrow().clone()
    }

    /// Set the state of a VPN connection (connect or disconnect).
    pub fn set_connection_state(&self, uuid: &str, active: bool) {
        let uuid = uuid.to_string();
        let connection = self.connection.borrow().clone();

        let Some(connection) = connection else {
            warn!("VPN: Cannot set connection state - no D-Bus connection");
            return;
        };

        // Clone the lock for use in the background thread.
        let lock = self.operation_lock.clone();

        // Use D-Bus in a background thread to avoid blocking.
        // The mutex serializes operations to prevent race conditions when
        // rapidly toggling connections.
        thread::spawn(move || {
            let _guard = lock.lock().unwrap();

            if active {
                Self::activate_connection_dbus(&connection, &uuid);
            } else {
                Self::deactivate_connection_dbus(&connection, &uuid);
            }

            // Request a refresh after the action completes.
            send_vpn_update(VpnUpdate::RequestRefresh);
        });
    }

    /// Activate a VPN connection via D-Bus.
    ///
    /// Uses NetworkManager's ActivateConnection method which properly triggers
    /// secret agents (polkit) for connections that require authentication.
    fn activate_connection_dbus(connection: &gio::DBusConnection, uuid: &str) {
        // First, get the connection object path from UUID
        let conn_path = match connection.call_sync(
            Some(NM_SERVICE),
            NM_SETTINGS_PATH,
            NM_SETTINGS_IFACE,
            "GetConnectionByUuid",
            Some(&(uuid,).to_variant()),
            Some(glib::VariantTy::new("(o)").unwrap()),
            gio::DBusCallFlags::NONE,
            5000,
            None::<&gio::Cancellable>,
        ) {
            Ok(v) => {
                let path_variant = v.child_value(0);
                match path_variant.str() {
                    Some(p) => p.to_string(),
                    None => {
                        warn!(
                            "VPN: GetConnectionByUuid returned invalid path for {}",
                            uuid
                        );
                        return;
                    }
                }
            }
            Err(e) => {
                warn!("VPN: Failed to get connection path for {}: {}", uuid, e);
                return;
            }
        };

        debug!("VPN: Activating connection {} (path: {})", uuid, conn_path);

        // Call ActivateConnection(connection, device, specific_object)
        // Use "/" for device (auto-select) and specific_object (none)
        let args = (
            glib::variant::ObjectPath::try_from(conn_path.as_str()).unwrap(),
            glib::variant::ObjectPath::try_from("/").unwrap(),
            glib::variant::ObjectPath::try_from("/").unwrap(),
        );

        match connection.call_sync(
            Some(NM_SERVICE),
            NM_PATH,
            NM_IFACE,
            "ActivateConnection",
            Some(&args.to_variant()),
            Some(glib::VariantTy::new("(o)").unwrap()),
            gio::DBusCallFlags::NONE,
            30000, // 30s timeout for auth dialogs
            None::<&gio::Cancellable>,
        ) {
            Ok(_) => debug!("VPN: Connection {} activation initiated", uuid),
            Err(e) => warn!("VPN: Failed to activate connection {}: {}", uuid, e),
        }
    }

    /// Deactivate a VPN connection via D-Bus.
    fn deactivate_connection_dbus(connection: &gio::DBusConnection, uuid: &str) {
        // Find the active connection path for this UUID
        let active_path = match Self::find_active_connection_path(connection, uuid) {
            Some(p) => p,
            None => {
                warn!("VPN: No active connection found for {}", uuid);
                return;
            }
        };

        debug!(
            "VPN: Deactivating connection {} (active path: {})",
            uuid, active_path
        );

        let args = (glib::variant::ObjectPath::try_from(active_path.as_str()).unwrap(),);

        match connection.call_sync(
            Some(NM_SERVICE),
            NM_PATH,
            NM_IFACE,
            "DeactivateConnection",
            Some(&args.to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            5000,
            None::<&gio::Cancellable>,
        ) {
            Ok(_) => debug!("VPN: Connection {} deactivated", uuid),
            Err(e) => warn!("VPN: Failed to deactivate connection {}: {}", uuid, e),
        }
    }

    /// Find the active connection object path for a given UUID.
    fn find_active_connection_path(connection: &gio::DBusConnection, uuid: &str) -> Option<String> {
        // Get ActiveConnections property
        let active_conns = connection
            .call_sync(
                Some(NM_SERVICE),
                NM_PATH,
                IFACE_PROPS,
                "Get",
                Some(&("org.freedesktop.NetworkManager", "ActiveConnections").to_variant()),
                Some(glib::VariantTy::new("(v)").unwrap()),
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
            )
            .ok()?;

        let inner = active_conns.child_value(0);
        let paths_variant = inner.child_value(0);
        let n_paths = paths_variant.n_children();

        for i in 0..n_paths {
            let path_variant = paths_variant.child_value(i);
            let path = path_variant.str()?;

            // Get UUID from this active connection
            let active_uuid =
                Self::get_dbus_property_string(connection, path, IFACE_ACTIVE, "Uuid");

            if active_uuid.as_deref() == Some(uuid) {
                return Some(path.to_string());
            }
        }

        None
    }

    /// Apply an update from background threads to the service state.
    /// Called via glib::idle_add_once from send_vpn_update().
    pub(crate) fn apply_update(&self, update: VpnUpdate) {
        match update {
            VpnUpdate::ConnectionsRefreshed {
                mut connections,
                active_vpn_paths,
            } => {
                let active_count = connections.iter().filter(|c| c.active).count();
                let any_active = active_count > 0;

                // Subscribe to state changes on active VPN connections.
                // Clear old subscriptions first - they'll be dropped automatically.
                self.active_conn_subscriptions.borrow_mut().clear();

                if let Some(conn) = self.connection.borrow().as_ref() {
                    let mut subs = Vec::new();
                    for path in active_vpn_paths {
                        // Subscribe to StateChanged signal on this active connection
                        let sub = conn.subscribe_to_signal(
                            Some(NM_SERVICE),
                            Some(IFACE_ACTIVE),
                            Some("StateChanged"),
                            Some(&path),
                            None,
                            gio::DBusSignalFlags::NONE,
                            move |_signal| {
                                send_vpn_update(VpnUpdate::RequestRefresh);
                            },
                        );
                        subs.push(sub);
                    }
                    *self.active_conn_subscriptions.borrow_mut() = subs;
                }

                // If a VPN became active, update last_used_uuid and persist
                if let Some(active_conn) = connections.iter().find(|c| c.active) {
                    let current_last_used = self.last_used_uuid.borrow().clone();
                    if current_last_used.as_ref() != Some(&active_conn.uuid) {
                        debug!(
                            "VpnService: updating last_used_uuid to {}",
                            active_conn.uuid
                        );
                        *self.last_used_uuid.borrow_mut() = Some(active_conn.uuid.clone());
                        self.save_state();
                    }
                }

                // Re-sort connections: active first, then preferred (last used), then alphabetically
                let preferred_uuid = self.last_used_uuid.borrow().clone();
                connections.sort_by(|a, b| {
                    // Active connections come first
                    match (a.active, b.active) {
                        (true, false) => return std::cmp::Ordering::Less,
                        (false, true) => return std::cmp::Ordering::Greater,
                        _ => {}
                    }
                    // Then preferred (last used) connection
                    if let Some(ref pref) = preferred_uuid {
                        match (a.uuid == *pref, b.uuid == *pref) {
                            (true, false) => return std::cmp::Ordering::Less,
                            (false, true) => return std::cmp::Ordering::Greater,
                            _ => {}
                        }
                    }
                    // Then alphabetically by name
                    a.name.to_lowercase().cmp(&b.name.to_lowercase())
                });

                let mut snapshot = self.snapshot.borrow_mut();
                snapshot.available = true;
                snapshot.connections = connections;
                snapshot.any_active = any_active;
                snapshot.active_count = active_count;
                snapshot.is_ready = true;
                // Preserve preferred_uuid from last_used_uuid
                snapshot.preferred_uuid = preferred_uuid;
                let snapshot_clone = snapshot.clone();
                drop(snapshot);

                // Reset refresh_pending so future refreshes can proceed
                self.refresh_pending.set(false);

                self.callbacks.notify(&snapshot_clone);
            }
            VpnUpdate::RequestRefresh => {
                self.queue_refresh();
            }
        }
    }

    /// Queue a debounced refresh (50ms delay to coalesce rapid signals).
    /// Uses the same pattern as BluetoothService::update_state_debounced.
    fn queue_refresh(&self) {
        if self.refresh_pending.get() {
            return;
        }
        self.refresh_pending.set(true);

        let connection = self.connection.borrow().clone();

        // Use timeout_add_local with ControlFlow::Break (like BluetoothService)
        // This avoids issues with timeout_add_local_once auto-removal causing
        // panics when trying to .remove() an already-fired source.
        glib::timeout_add_local(Duration::from_millis(STATE_REFRESH_DELAY_MS), move || {
            if let Some(ref conn) = connection {
                Self::refresh_connections_async(conn.clone());
            }
            glib::ControlFlow::Break
        });
    }

    fn set_unavailable(&self) {
        let mut snapshot = self.snapshot.borrow_mut();
        if !snapshot.available {
            return; // Already unavailable
        }
        *snapshot = VpnSnapshot::unknown();
        // Preserve preferred_uuid even when unavailable
        snapshot.preferred_uuid = self.last_used_uuid.borrow().clone();
        let snapshot_clone = snapshot.clone();
        drop(snapshot);
        self.callbacks.notify(&snapshot_clone);

        // Clear proxies.
        self.nm_proxy.replace(None);
        self.settings_proxy.replace(None);
    }

    /// Save current VPN state to disk.
    fn save_state(&self) {
        // Load existing state to preserve notification state
        let mut persisted = state::load();

        // Update VPN state
        persisted.vpn.last_used_uuid = self.last_used_uuid.borrow().clone();

        state::save(&persisted);
    }

    // D-Bus Initialization

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
                        error!("VPN: Failed to get system bus: {}", e);
                        return;
                    }
                };

                *this.connection.borrow_mut() = Some(connection.clone());

                // Create NetworkManager main proxy.
                let this_weak2 = Rc::downgrade(&this);
                let conn_for_nm = connection.clone();
                gio::DBusProxy::new(
                    &connection,
                    gio::DBusProxyFlags::NONE,
                    None::<&gio::DBusInterfaceInfo>,
                    Some(NM_SERVICE),
                    NM_PATH,
                    NM_IFACE,
                    None::<&gio::Cancellable>,
                    move |res| {
                        let this = match this_weak2.upgrade() {
                            Some(this) => this,
                            None => return,
                        };

                        let proxy = match res {
                            Ok(p) => p,
                            Err(e) => {
                                error!("VPN: Failed to create NetworkManager proxy: {}", e);
                                return;
                            }
                        };

                        *this.nm_proxy.borrow_mut() = Some(proxy.clone());

                        // Monitor for service appearing/disappearing (e.g., NM restart).
                        let this_weak = Rc::downgrade(&this);
                        proxy.connect_local("notify::g-name-owner", false, move |values| {
                            let this = this_weak.upgrade()?;
                            let proxy = values[0].get::<gio::DBusProxy>().ok();
                            let has_owner = proxy.and_then(|p| p.name_owner()).is_some();
                            if has_owner {
                                // Service reappeared - refresh.
                                send_vpn_update(VpnUpdate::RequestRefresh);
                            } else {
                                // Service disappeared - mark unavailable.
                                this.set_unavailable();
                            }
                            None
                        });

                        // Subscribe to ActiveConnections property changes.
                        let sub = conn_for_nm.subscribe_to_signal(
                            Some(NM_SERVICE),
                            Some(IFACE_PROPS),
                            Some("PropertiesChanged"),
                            Some(NM_PATH),
                            None,
                            gio::DBusSignalFlags::NONE,
                            move |signal| {
                                // Check if ActiveConnections changed.
                                if let Some(iface_name) = signal.parameters.child_value(0).str()
                                    && iface_name == NM_IFACE
                                {
                                    send_vpn_update(VpnUpdate::RequestRefresh);
                                }
                            },
                        );
                        this._signal_subscriptions.borrow_mut().push(sub);
                    },
                );

                // Create Settings proxy.
                let this_weak3 = Rc::downgrade(&this);
                let conn_for_settings = connection.clone();
                gio::DBusProxy::new(
                    &connection,
                    gio::DBusProxyFlags::NONE,
                    None::<&gio::DBusInterfaceInfo>,
                    Some(NM_SERVICE),
                    NM_SETTINGS_PATH,
                    NM_SETTINGS_IFACE,
                    None::<&gio::Cancellable>,
                    move |res| {
                        let this = match this_weak3.upgrade() {
                            Some(this) => this,
                            None => return,
                        };

                        let proxy = match res {
                            Ok(p) => p,
                            Err(e) => {
                                error!("VPN: Failed to create Settings proxy: {}", e);
                                return;
                            }
                        };

                        *this.settings_proxy.borrow_mut() = Some(proxy);

                        // Subscribe to NewConnection and ConnectionRemoved signals.
                        let sub1 = conn_for_settings.subscribe_to_signal(
                            Some(NM_SERVICE),
                            Some(NM_SETTINGS_IFACE),
                            Some("NewConnection"),
                            Some(NM_SETTINGS_PATH),
                            None,
                            gio::DBusSignalFlags::NONE,
                            move |_signal| {
                                send_vpn_update(VpnUpdate::RequestRefresh);
                            },
                        );

                        let conn_for_settings2 = conn_for_settings.clone();
                        let sub2 = conn_for_settings2.subscribe_to_signal(
                            Some(NM_SERVICE),
                            Some(NM_SETTINGS_IFACE),
                            Some("ConnectionRemoved"),
                            Some(NM_SETTINGS_PATH),
                            None,
                            gio::DBusSignalFlags::NONE,
                            move |_signal| {
                                send_vpn_update(VpnUpdate::RequestRefresh);
                            },
                        );

                        this._signal_subscriptions.borrow_mut().extend([sub1, sub2]);

                        // Trigger initial refresh.
                        this.refresh_pending.set(false);
                        this.queue_refresh();
                    },
                );
            },
        );
    }

    // D-Bus: Refresh Connections

    fn refresh_connections_async(connection: gio::DBusConnection) {
        // Run in a background thread to avoid blocking.
        thread::spawn(move || {
            let (connections, active_vpn_paths) = Self::fetch_vpn_connections_sync(&connection);
            send_vpn_update(VpnUpdate::ConnectionsRefreshed {
                connections,
                active_vpn_paths,
            });
        });
    }

    /// Synchronously fetch all VPN connections from NetworkManager.
    /// Returns (connections, active VPN object paths for signal subscription).
    fn fetch_vpn_connections_sync(
        connection: &gio::DBusConnection,
    ) -> (Vec<VpnConnection>, Vec<String>) {
        let mut result = Vec::new();

        // Get list of connection paths from Settings.
        let conn_paths = match connection.call_sync(
            Some(NM_SERVICE),
            NM_SETTINGS_PATH,
            NM_SETTINGS_IFACE,
            "ListConnections",
            None,
            Some(glib::VariantTy::new("(ao)").unwrap()),
            gio::DBusCallFlags::NONE,
            5000,
            None::<&gio::Cancellable>,
        ) {
            Ok(v) => v,
            Err(e) => {
                warn!("VPN: Failed to list connections: {}", e);
                return (result, Vec::new());
            }
        };

        // Get active connections from NetworkManager.
        let (active_map, active_vpn_paths) = Self::get_active_connections_sync(connection);

        // Parse the array of object paths.
        let paths_variant = conn_paths.child_value(0);
        let n_paths = paths_variant.n_children();

        for i in 0..n_paths {
            let path_variant = paths_variant.child_value(i);
            let Some(path) = path_variant.str() else {
                continue;
            };

            // Get connection settings.
            let settings = match connection.call_sync(
                Some(NM_SERVICE),
                path,
                IFACE_CONNECTION,
                "GetSettings",
                None,
                Some(glib::VariantTy::new("(a{sa{sv}})").unwrap()),
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
            ) {
                Ok(v) => v,
                Err(e) => {
                    debug!("VPN: Failed to get settings for {}: {}", path, e);
                    continue;
                }
            };

            // Parse the settings dict.
            let settings_dict = settings.child_value(0);

            // Get connection section.
            let Some(conn_section) = Self::get_dict_section(&settings_dict, "connection") else {
                continue;
            };

            // Get connection type.
            let Some(conn_type) = Self::get_string_from_dict(&conn_section, "type") else {
                continue;
            };

            // Filter to VPN types only.
            if !VPN_TYPES.contains(&conn_type.as_str()) {
                continue;
            }

            // Get UUID and name.
            let Some(uuid) = Self::get_string_from_dict(&conn_section, "uuid") else {
                continue;
            };
            let name = Self::get_string_from_dict(&conn_section, "id").unwrap_or_default();
            let autoconnect =
                Self::get_bool_from_dict(&conn_section, "autoconnect").unwrap_or(false);

            // Check if active and get state.
            let (active, state) = active_map
                .get(&uuid)
                .map(|s| (true, *s))
                .unwrap_or((false, VpnState::Unknown));

            result.push(VpnConnection {
                uuid,
                name,
                active,
                state,
                autoconnect,
                vpn_type: conn_type,
            });
        }

        // Note: Sorting is handled by apply_update() which re-sorts with
        // preferred UUID logic. No need to sort here.

        (result, active_vpn_paths)
    }

    /// Get active VPN connection UUIDs with their states and object paths.
    /// Returns (uuid -> state map, list of VPN object paths for signal subscription).
    fn get_active_connections_sync(
        connection: &gio::DBusConnection,
    ) -> (std::collections::HashMap<String, VpnState>, Vec<String>) {
        use std::collections::HashMap;
        let mut result: HashMap<String, VpnState> = HashMap::new();
        let mut vpn_paths: Vec<String> = Vec::new();

        // Get ActiveConnections property.
        let active_conns = match connection.call_sync(
            Some(NM_SERVICE),
            NM_PATH,
            IFACE_PROPS,
            "Get",
            Some(&("org.freedesktop.NetworkManager", "ActiveConnections").to_variant()),
            Some(glib::VariantTy::new("(v)").unwrap()),
            gio::DBusCallFlags::NONE,
            5000,
            None::<&gio::Cancellable>,
        ) {
            Ok(v) => v,
            Err(e) => {
                debug!("VPN: Failed to get ActiveConnections: {}", e);
                return (result, vpn_paths);
            }
        };

        let inner = active_conns.child_value(0);
        let paths_variant = inner.child_value(0);
        let n_paths = paths_variant.n_children();

        for i in 0..n_paths {
            let path_variant = paths_variant.child_value(i);
            let Some(path) = path_variant.str() else {
                continue;
            };

            // Get UUID and Type from the active connection.
            let uuid = Self::get_dbus_property_string(connection, path, IFACE_ACTIVE, "Uuid");
            let conn_type = Self::get_dbus_property_string(connection, path, IFACE_ACTIVE, "Type");
            let state = Self::get_dbus_property_u32(connection, path, IFACE_ACTIVE, "State")
                .map(VpnState::from)
                .unwrap_or(VpnState::Unknown);

            let Some(uuid) = uuid else {
                continue;
            };

            // Only track VPN connections.
            if let Some(ref t) = conn_type
                && !VPN_TYPES.contains(&t.as_str())
            {
                continue;
            }

            result.insert(uuid, state);
            vpn_paths.push(path.to_string());
        }

        (result, vpn_paths)
    }

    /// Helper: Get a D-Bus property as a string.
    fn get_dbus_property_string(
        connection: &gio::DBusConnection,
        path: &str,
        interface: &str,
        property: &str,
    ) -> Option<String> {
        let result = connection
            .call_sync(
                Some(NM_SERVICE),
                path,
                IFACE_PROPS,
                "Get",
                Some(&(interface, property).to_variant()),
                Some(glib::VariantTy::new("(v)").unwrap()),
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
            )
            .ok()?;

        let inner = result.child_value(0);
        let value = inner.child_value(0);
        value.str().map(|s| s.to_string())
    }

    /// Helper: Get a D-Bus property as a u32.
    fn get_dbus_property_u32(
        connection: &gio::DBusConnection,
        path: &str,
        interface: &str,
        property: &str,
    ) -> Option<u32> {
        let result = connection
            .call_sync(
                Some(NM_SERVICE),
                path,
                IFACE_PROPS,
                "Get",
                Some(&(interface, property).to_variant()),
                Some(glib::VariantTy::new("(v)").unwrap()),
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
            )
            .ok()?;

        let inner = result.child_value(0);
        let value = inner.child_value(0);
        value.get::<u32>()
    }

    /// Helper: Get a section from a settings dict (a{sa{sv}}).
    fn get_dict_section(dict: &Variant, section: &str) -> Option<Variant> {
        let n = dict.n_children();
        for i in 0..n {
            let entry = dict.child_value(i);
            let key = entry.child_value(0);
            if key.str() == Some(section) {
                return Some(entry.child_value(1));
            }
        }
        None
    }

    /// Helper: Get a string value from a dict (a{sv}).
    fn get_string_from_dict(dict: &Variant, key: &str) -> Option<String> {
        let n = dict.n_children();
        for i in 0..n {
            let entry = dict.child_value(i);
            let entry_key = entry.child_value(0);
            if entry_key.str() == Some(key) {
                let value = entry.child_value(1);
                // The value is a variant, so unwrap it.
                let inner = value.child_value(0);
                return inner.str().map(|s| s.to_string());
            }
        }
        None
    }

    /// Helper: Get a bool value from a dict (a{sv}).
    fn get_bool_from_dict(dict: &Variant, key: &str) -> Option<bool> {
        let n = dict.n_children();
        for i in 0..n {
            let entry = dict.child_value(i);
            let entry_key = entry.child_value(0);
            if entry_key.str() == Some(key) {
                let value = entry.child_value(1);
                let inner = value.child_value(0);
                return inner.get::<bool>();
            }
        }
        None
    }
}
