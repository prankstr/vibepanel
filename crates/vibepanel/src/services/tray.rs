//! TrayService - StatusNotifierItem host implementation for the system tray widget.
//!
//! - Acts as StatusNotifierWatcher when possible (owns the bus name)
//! - Falls back to connecting to an external watcher if another host is active
//! - Maintains proxies for each tray item and their menus
//! - Provides canonical snapshots for the widget to render
//! - Supports debounced updates for rapid signal batching

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gtk4::gio::{self, prelude::*};
use gtk4::glib::{self, Variant};
use sha2::{Digest, Sha256};
use tracing::{debug, error, info, warn};

use crate::services::callbacks::{CallbackId, Callbacks};

const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const ITEM_INTERFACE: &str = "org.kde.StatusNotifierItem";
const DBUSMENU_INTERFACE: &str = "com.canonical.dbusmenu";

/// Debounce time in milliseconds for batching rapid signals.
const UPDATE_DEBOUNCE_MS: u32 = 10;

/// DBus introspection XML for StatusNotifierWatcher interface.
const WATCHER_XML: &str = r#"
<node>
  <interface name="org.kde.StatusNotifierWatcher">
    <method name="RegisterStatusNotifierItem">
      <arg direction="in" name="service" type="s"/>
    </method>
    <method name="RegisterStatusNotifierHost">
      <arg direction="in" name="service" type="s"/>
    </method>
    <property name="RegisteredStatusNotifierItems" type="as" access="read"/>
    <property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
    <property name="ProtocolVersion" type="i" access="read"/>
    <signal name="StatusNotifierItemRegistered">
      <arg name="service" type="s"/>
    </signal>
    <signal name="StatusNotifierItemUnregistered">
      <arg name="service" type="s"/>
    </signal>
    <signal name="StatusNotifierHostRegistered"/>
  </interface>
</node>
"#;

/// Signals that trigger snapshot refresh.
const SNAPSHOT_SIGNAL_NAMES: &[&str] = &[
    "NewIcon",
    "NewStatus",
    "NewToolTip",
    "NewTitle",
    "NewAttentionIcon",
    "NewOverlayIcon",
    "NewIconThemePath",
];

/// Signals that invalidate menu proxies.
const MENU_RESET_SIGNALS: &[&str] = &["NewMenu"];

/// Raw pixmap data from a tray item.
#[derive(Debug, Clone)]
pub struct TrayPixmap {
    pub width: i32,
    pub height: i32,
    pub buffer: glib::Bytes,
    pub hash_key: String,
}

/// Snapshot of a tray item's current state.
#[derive(Debug, Clone)]
pub struct TrayItem {
    pub identifier: String,
    pub title: String,
    pub tooltip: Option<String>,
    pub status: String,
    pub icon_name: Option<String>,
    pub attention_icon_name: Option<String>,
    pub pixmap: Option<TrayPixmap>,
    pub attention_pixmap: Option<TrayPixmap>,
    pub menu_path: Option<String>,
    pub bus_name: String,
    /// If true, left-click should show menu instead of activate.
    pub item_is_menu: bool,
    /// Custom icon theme path provided by the application.
    pub icon_theme_path: Option<String>,
}

/// A single entry in a tray item's context menu.
#[derive(Debug, Clone)]
pub struct TrayMenuEntry {
    pub menu_id: i32,
    pub label: String,
    pub enabled: bool,
    pub is_separator: bool,
    pub toggle_type: Option<String>,
    pub toggle_state: Option<i32>,
    pub children: Vec<TrayMenuEntry>,
}

impl TrayMenuEntry {
    /// Check if this entry has children (submenu).
    pub fn has_children(&self) -> bool {
        !self.children.is_empty()
    }
}

/// Shared, process-wide tray service implementing StatusNotifierHost.
pub struct TrayService {
    /// Current tray items by identifier.
    items: RefCell<HashMap<String, TrayItem>>,
    /// DBus connection.
    bus: RefCell<Option<gio::DBusConnection>>,
    /// External watcher proxy (when not acting as watcher).
    watcher: RefCell<Option<gio::DBusProxy>>,
    /// Proxies for each tray item.
    proxies: RefCell<HashMap<String, gio::DBusProxy>>,
    /// Menu proxies for each tray item.
    menu_proxies: RefCell<HashMap<String, gio::DBusProxy>>,

    // Watcher implementation state
    is_watcher: Cell<bool>,
    watcher_registration_id: RefCell<Option<gio::RegistrationId>>,
    registered_items: RefCell<HashSet<String>>,
    registered_hosts: RefCell<HashSet<String>>,
    // Note: We don't track watcher IDs for item names because the proxy's
    // g-name-owner signal already handles name disappearance detection.

    // Host identity
    host_id: String,

    // Debouncing state
    pending_updates: RefCell<HashMap<String, HashSet<String>>>,
    debounce_timers: RefCell<HashMap<String, glib::SourceId>>,

    // Proxies being created asynchronously
    pending_proxies: RefCell<HashSet<String>>,

    // Callbacks and readiness
    callbacks: Callbacks<TrayService>,
    ready: Cell<bool>,

    /// D-Bus signal subscriptions for external watcher signals (kept alive for service lifetime).
    _watcher_signal_subscriptions: RefCell<Vec<gio::SignalSubscription>>,
}

impl TrayService {
    fn new() -> Rc<Self> {
        let host_id = format!("org.vibepanel.TrayHost-{}", std::process::id());

        let service = Rc::new(Self {
            items: RefCell::new(HashMap::new()),
            bus: RefCell::new(None),
            watcher: RefCell::new(None),
            proxies: RefCell::new(HashMap::new()),
            menu_proxies: RefCell::new(HashMap::new()),
            is_watcher: Cell::new(false),
            watcher_registration_id: RefCell::new(None),
            registered_items: RefCell::new(HashSet::new()),
            registered_hosts: RefCell::new(HashSet::new()),
            host_id,
            pending_updates: RefCell::new(HashMap::new()),
            debounce_timers: RefCell::new(HashMap::new()),
            pending_proxies: RefCell::new(HashSet::new()),
            callbacks: Callbacks::new(),
            ready: Cell::new(false),
            _watcher_signal_subscriptions: RefCell::new(Vec::new()),
        });

        Self::init_dbus(&service);
        service
    }

    /// Get the global TrayService singleton.
    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<TrayService> = TrayService::new();
        }
        INSTANCE.with(|s| s.clone())
    }

    /// Register a callback to be invoked when tray state changes.
    pub fn connect<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&TrayService) + 'static,
    {
        let id = self.callbacks.register(callback);

        // Immediately send current state if ready.
        if self.ready.get() {
            self.callbacks.notify_single(id, self);
        }

        id
    }

    /// Unregister a previously registered tray callback.
    pub fn disconnect(&self, id: CallbackId) -> bool {
        self.callbacks.unregister(id)
    }

    /// Check if the service is ready.
    pub fn is_ready(&self) -> bool {
        self.ready.get()
    }

    /// Get current tray items as a sorted list (by identifier).
    ///
    /// Returns a Vec of (identifier, snapshot) pairs sorted by identifier
    /// for stable ordering across updates.
    pub fn items(&self) -> Vec<(String, TrayItem)> {
        let items = self.items.borrow();
        let mut result: Vec<_> = items.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        result.sort_by(|a, b| a.0.cmp(&b.0));
        result
    }

    /// Activate a tray item (left-click action).
    pub fn activate(&self, identifier: &str, x: i32, y: i32) {
        let proxies = self.proxies.borrow();
        let Some(proxy) = proxies.get(identifier) else {
            debug!("No proxy for activate: {}", identifier);
            return;
        };

        proxy.call(
            "Activate",
            Some(&(x, y).to_variant()),
            gio::DBusCallFlags::NONE,
            5000,
            None::<&gio::Cancellable>,
            |result| {
                if let Err(e) = result {
                    debug!("Activate call failed: {}", e);
                }
            },
        );
    }

    /// Get the context menu for a tray item asynchronously.
    ///
    /// The callback receives the menu entries when ready. This prevents UI freezes
    /// if a tray application is slow to respond.
    pub fn get_menu<F>(&self, identifier: &str, callback: F)
    where
        F: FnOnce(Vec<TrayMenuEntry>) + 'static,
    {
        let identifier = identifier.to_string();

        // Check if we already have a cached menu proxy
        if let Some(proxy) = self.menu_proxies.borrow().get(&identifier).cloned() {
            Self::fetch_menu_layout(identifier, proxy, callback);
            return;
        }

        // Need to create a menu proxy - get item info first
        let (bus_name, menu_path) = {
            let items = self.items.borrow();
            match items.get(&identifier) {
                Some(item) => match &item.menu_path {
                    Some(path) => (item.bus_name.clone(), path.clone()),
                    None => {
                        callback(Vec::new());
                        return;
                    }
                },
                None => {
                    callback(Vec::new());
                    return;
                }
            }
        };

        // Create menu proxy asynchronously
        let identifier_clone = identifier.clone();
        gio::DBusProxy::for_bus(
            gio::BusType::Session,
            gio::DBusProxyFlags::DO_NOT_CONNECT_SIGNALS,
            None,
            &bus_name,
            &menu_path,
            DBUSMENU_INTERFACE,
            None::<&gio::Cancellable>,
            move |result| {
                let proxy = match result {
                    Ok(p) => p,
                    Err(e) => {
                        error!(
                            "Failed to create menu proxy for {}: {}",
                            identifier_clone, e
                        );
                        callback(Vec::new());
                        return;
                    }
                };

                // Cache the proxy
                let service = TrayService::global();
                service
                    .menu_proxies
                    .borrow_mut()
                    .insert(identifier_clone.clone(), proxy.clone());

                Self::fetch_menu_layout(identifier_clone, proxy, callback);
            },
        );
    }

    /// Internal: Fetch menu layout from a menu proxy (async chain).
    fn fetch_menu_layout<F>(identifier: String, menu_proxy: gio::DBusProxy, callback: F)
    where
        F: FnOnce(Vec<TrayMenuEntry>) + 'static,
    {
        let identifier_clone = identifier.clone();
        let menu_proxy_clone = menu_proxy.clone();

        // First call AboutToShow async, then GetLayout
        menu_proxy.call(
            "AboutToShow",
            Some(&(0i32,).to_variant()),
            gio::DBusCallFlags::NONE,
            5000,
            None::<&gio::Cancellable>,
            move |_result| {
                // Ignore AboutToShow result - some apps don't implement it
                // Now call GetLayout
                let properties: Vec<&str> = vec![
                    "label",
                    "enabled",
                    "visible",
                    "type",
                    "toggle-type",
                    "toggle-state",
                    "children-display",
                ];

                menu_proxy_clone.call(
                    "GetLayout",
                    Some(&(0i32, -1i32, properties).to_variant()),
                    gio::DBusCallFlags::NONE,
                    5000,
                    None::<&gio::Cancellable>,
                    move |result| {
                        let entries = match result {
                            Ok(r) => {
                                let service = TrayService::global();
                                service.parse_layout_result(&r)
                            }
                            Err(e) => {
                                error!("GetLayout failed for {}: {}", identifier_clone, e);
                                Vec::new()
                            }
                        };
                        callback(entries);
                    },
                );
            },
        );
    }

    /// Send a menu event (clicked, hovered, etc.).
    pub fn send_menu_event(&self, identifier: &str, menu_id: i32, event: &str) {
        let menu_proxy = match self.ensure_menu_proxy(identifier) {
            Some(p) => p,
            None => return,
        };

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as u32)
            .unwrap_or(0);

        // Event signature: (isvu) - id, event_id, data variant, timestamp
        // Create an empty string variant for data
        let data_variant = "".to_variant();
        let params = (menu_id, event, data_variant, timestamp).to_variant();

        menu_proxy.call(
            "Event",
            Some(&params),
            gio::DBusCallFlags::NONE,
            5000,
            None::<&gio::Cancellable>,
            |result| {
                if let Err(e) = result {
                    debug!("Menu Event call failed: {}", e);
                }
            },
        );
    }

    fn init_dbus(this: &Rc<Self>) {
        debug!("TrayService: initializing DBus connection");

        let this_weak = Rc::downgrade(this);
        gio::bus_get(
            gio::BusType::Session,
            None::<&gio::Cancellable>,
            move |result| {
                let this = match this_weak.upgrade() {
                    Some(t) => t,
                    None => return,
                };

                let connection = match result {
                    Ok(c) => c,
                    Err(e) => {
                        error!("Failed to get session bus: {}", e);
                        warn!("Tray support disabled; no bus available");
                        this.set_ready();
                        return;
                    }
                };

                *this.bus.borrow_mut() = Some(connection.clone());

                // Export watcher interface before trying to own the name
                this.export_watcher_interface(&connection);

                // Try to own the watcher name
                this.try_own_watcher_name(&connection);
            },
        );
    }

    fn try_own_watcher_name(self: &Rc<Self>, connection: &gio::DBusConnection) {
        let this_weak = Rc::downgrade(self);
        let this_weak2 = Rc::downgrade(self);

        gio::bus_own_name_on_connection(
            connection,
            WATCHER_NAME,
            gio::BusNameOwnerFlags::NONE,
            move |_connection, _name| {
                // Name acquired
                if let Some(this) = this_weak.upgrade() {
                    this.on_watcher_name_acquired();
                }
            },
            move |_connection, _name| {
                // Name lost
                if let Some(this) = this_weak2.upgrade() {
                    this.on_watcher_name_lost();
                }
            },
        );
    }

    fn on_watcher_name_acquired(self: &Rc<Self>) {
        info!("Acquired {}, acting as StatusNotifierWatcher", WATCHER_NAME);
        self.is_watcher.set(true);

        // Register ourselves as a host
        self.registered_hosts
            .borrow_mut()
            .insert(self.host_id.clone());

        // Emit host registered signal
        self.emit_host_registered_signal();

        // Ready to receive items
        self.set_ready();
    }

    fn on_watcher_name_lost(self: &Rc<Self>) {
        if self.is_watcher.get() {
            warn!("Lost {}, falling back to external watcher", WATCHER_NAME);
            self.is_watcher.set(false);
            self.unexport_watcher_interface();
        }

        // Fall back to external watcher
        if let Some(ref bus) = *self.bus.borrow() {
            self.setup_external_watcher(bus);
        } else {
            self.set_ready();
        }
    }

    fn export_watcher_interface(&self, connection: &gio::DBusConnection) {
        let node_info = match gio::DBusNodeInfo::for_xml(WATCHER_XML) {
            Ok(n) => n,
            Err(e) => {
                error!("Failed to parse watcher XML: {}", e);
                return;
            }
        };

        let interface_info = match node_info.lookup_interface(WATCHER_NAME) {
            Some(i) => i,
            None => {
                error!("Interface {} not found in XML", WATCHER_NAME);
                return;
            }
        };

        // Use register_object with the builder pattern
        let registration = connection
            .register_object(WATCHER_PATH, &interface_info)
            .method_call(
                |_connection,
                 sender,
                 _object_path,
                 _interface_name,
                 method_name,
                 parameters,
                 invocation| {
                    let service = TrayService::global();
                    service.handle_watcher_method_call(
                        sender,
                        method_name,
                        &parameters,
                        invocation,
                    );
                },
            )
            .property(
                |_connection, _sender, _object_path, _interface_name, property_name| {
                    let service = TrayService::global();
                    service
                        .handle_watcher_get_property(property_name)
                        .unwrap_or_else(|| {
                            // Unknown property requested - this shouldn't happen if our XML is correct.
                            // Log error and return a dummy value to satisfy the API.
                            error!("Unknown watcher property requested: {}", property_name);
                            // Return empty string array as safest default for unknown props
                            Vec::<String>::new().to_variant()
                        })
                },
            )
            .build();

        match registration {
            Ok(reg_id) => {
                *self.watcher_registration_id.borrow_mut() = Some(reg_id);
                debug!("Exported watcher interface at {}", WATCHER_PATH);
            }
            Err(e) => {
                error!("Failed to register watcher object: {}", e);
            }
        }
    }

    fn unexport_watcher_interface(&self) {
        if let Some(reg_id) = self.watcher_registration_id.borrow_mut().take()
            && let Some(ref bus) = *self.bus.borrow()
        {
            let _ = bus.unregister_object(reg_id);
        }
    }

    fn handle_watcher_method_call(
        &self,
        sender: Option<&str>,
        method_name: &str,
        parameters: &Variant,
        invocation: gio::DBusMethodInvocation,
    ) {
        match method_name {
            "RegisterStatusNotifierItem" => {
                // Sender is required for RegisterStatusNotifierItem
                let Some(sender) = sender else {
                    invocation.return_error(
                        gio::IOErrorEnum::InvalidArgument,
                        "RegisterStatusNotifierItem requires a sender",
                    );
                    return;
                };
                if let Some(service) = parameters.child_value(0).str() {
                    self.handle_register_item(sender, service);
                }
                invocation.return_value(None);
            }
            "RegisterStatusNotifierHost" => {
                if let Some(service) = parameters.child_value(0).str() {
                    self.handle_register_host(service);
                }
                invocation.return_value(None);
            }
            _ => {
                invocation.return_error(
                    gio::IOErrorEnum::InvalidArgument,
                    &format!("Unknown method: {}", method_name),
                );
            }
        }
    }

    fn handle_watcher_get_property(&self, property_name: &str) -> Option<Variant> {
        match property_name {
            "RegisteredStatusNotifierItems" => {
                let items: Vec<String> = self.registered_items.borrow().iter().cloned().collect();
                Some(items.to_variant())
            }
            "IsStatusNotifierHostRegistered" => {
                Some((!self.registered_hosts.borrow().is_empty()).to_variant())
            }
            "ProtocolVersion" => Some(1i32.to_variant()),
            _ => None,
        }
    }

    fn handle_register_item(&self, sender: &str, service: &str) {
        // Build the full identifier
        let identifier = if service.starts_with('/') {
            // service is an object path, use sender as bus name
            format!("{}{}", sender, service)
        } else if service.contains('/') {
            // service already contains bus_name/path
            service.to_string()
        } else {
            // service is a bus name, use default path
            format!("{}/StatusNotifierItem", service)
        };

        if !self.registered_items.borrow().contains(&identifier) {
            self.registered_items
                .borrow_mut()
                .insert(identifier.clone());
            debug!("Registered item: {}", identifier);

            // Emit signal
            self.emit_item_registered_signal(&identifier);

            // Process the item - use global() to get Rc<Self> required by process_item
            // Name disappearance is detected via the proxy's g-name-owner signal
            TrayService::global().process_item(&identifier);
        }
    }

    fn handle_register_host(&self, service: &str) {
        if !self.registered_hosts.borrow().contains(service) {
            self.registered_hosts
                .borrow_mut()
                .insert(service.to_string());
            debug!("Registered host: {}", service);
            self.emit_host_registered_signal();
        }
    }

    /// Handle item disappearance - cleans up both watcher registry and internal state.
    fn on_item_name_vanished(&self, identifier: &str) {
        debug!("Item vanished: {}", identifier);

        // Remove from watcher's registered items and emit signal (if we're acting as watcher)
        if self.is_watcher.get() && self.registered_items.borrow_mut().remove(identifier) {
            self.emit_item_unregistered_signal(identifier);
        }

        // Clean up internal tracking (proxies, snapshots, etc.)
        self.remove_item(identifier);
    }

    fn emit_item_registered_signal(&self, identifier: &str) {
        let Some(ref bus) = *self.bus.borrow() else {
            return;
        };

        if let Err(e) = bus.emit_signal(
            None::<&str>,
            WATCHER_PATH,
            WATCHER_NAME,
            "StatusNotifierItemRegistered",
            Some(&(identifier,).to_variant()),
        ) {
            error!("Failed to emit ItemRegistered signal: {}", e);
        }
    }

    fn emit_item_unregistered_signal(&self, identifier: &str) {
        let Some(ref bus) = *self.bus.borrow() else {
            return;
        };

        if let Err(e) = bus.emit_signal(
            None::<&str>,
            WATCHER_PATH,
            WATCHER_NAME,
            "StatusNotifierItemUnregistered",
            Some(&(identifier,).to_variant()),
        ) {
            error!("Failed to emit ItemUnregistered signal: {}", e);
        }
    }

    fn emit_host_registered_signal(&self) {
        let Some(ref bus) = *self.bus.borrow() else {
            return;
        };

        if let Err(e) = bus.emit_signal(
            None::<&str>,
            WATCHER_PATH,
            WATCHER_NAME,
            "StatusNotifierHostRegistered",
            None,
        ) {
            error!("Failed to emit HostRegistered signal: {}", e);
        }
    }

    fn setup_external_watcher(self: &Rc<Self>, connection: &gio::DBusConnection) {
        let this_weak = Rc::downgrade(self);
        let connection_clone = connection.clone();

        gio::DBusProxy::for_bus(
            gio::BusType::Session,
            gio::DBusProxyFlags::GET_INVALIDATED_PROPERTIES,
            None,
            WATCHER_NAME,
            WATCHER_PATH,
            WATCHER_NAME,
            None::<&gio::Cancellable>,
            move |result| {
                let this = match this_weak.upgrade() {
                    Some(t) => t,
                    None => return,
                };

                let watcher = match result {
                    Ok(w) => w,
                    Err(e) => {
                        error!("Failed to connect to external watcher: {}", e);
                        warn!("Tray support disabled; no watcher available");
                        this.set_ready();
                        return;
                    }
                };

                debug!("Connected to external watcher");
                *this.watcher.borrow_mut() = Some(watcher.clone());

                // Register as host
                let host_id = this.host_id.clone();
                watcher.call(
                    "RegisterStatusNotifierHost",
                    Some(&(host_id.as_str(),).to_variant()),
                    gio::DBusCallFlags::NONE,
                    5000,
                    None::<&gio::Cancellable>,
                    |result| {
                        if let Err(e) = result {
                            error!("Host registration failed: {}", e);
                        } else {
                            debug!("Registered with external watcher");
                        }
                    },
                );

                // Get initial items
                if let Some(prop) = watcher.cached_property("RegisteredStatusNotifierItems")
                    && let Ok(items) = prop.array_iter_str()
                {
                    for identifier in items {
                        this.process_item(identifier);
                    }
                }

                // Subscribe to signals
                this.subscribe_to_watcher_signals(&connection_clone);

                this.set_ready();
            },
        );
    }

    fn subscribe_to_watcher_signals(self: &Rc<Self>, connection: &gio::DBusConnection) {
        // StatusNotifierItemRegistered
        let this_weak = Rc::downgrade(self);
        let sub1 = connection.subscribe_to_signal(
            None::<&str>,
            Some(WATCHER_NAME),
            Some("StatusNotifierItemRegistered"),
            Some(WATCHER_PATH),
            None,
            gio::DBusSignalFlags::NONE,
            move |signal| {
                if let Some(this) = this_weak.upgrade()
                    && let Some(identifier) = signal.parameters.child_value(0).str()
                {
                    debug!("StatusNotifierItemRegistered: {}", identifier);
                    this.process_item(identifier);
                }
            },
        );

        // StatusNotifierItemUnregistered
        let this_weak = Rc::downgrade(self);
        let sub2 = connection.subscribe_to_signal(
            None::<&str>,
            Some(WATCHER_NAME),
            Some("StatusNotifierItemUnregistered"),
            Some(WATCHER_PATH),
            None,
            gio::DBusSignalFlags::NONE,
            move |signal| {
                if let Some(this) = this_weak.upgrade()
                    && let Some(identifier) = signal.parameters.child_value(0).str()
                {
                    debug!("StatusNotifierItemUnregistered: {}", identifier);
                    if let Some((bus_name, object_path)) = this.split_identifier(identifier) {
                        let key = this.make_identifier(&bus_name, &object_path);
                        this.remove_item(&key);
                    }
                }
            },
        );

        // Store subscriptions to keep them alive
        self._watcher_signal_subscriptions
            .borrow_mut()
            .extend([sub1, sub2]);
    }

    fn process_item(self: &Rc<Self>, identifier: &str) {
        let Some((bus_name, object_path)) = self.split_identifier(identifier) else {
            return;
        };

        let key = self.make_identifier(&bus_name, &object_path);

        if self.proxies.borrow().contains_key(&key) || self.pending_proxies.borrow().contains(&key)
        {
            return;
        }

        let Some(ref connection) = *self.bus.borrow() else {
            return;
        };

        self.pending_proxies.borrow_mut().insert(key.clone());

        let this_weak = Rc::downgrade(self);
        let key_clone = key.clone();

        gio::DBusProxy::new(
            connection,
            gio::DBusProxyFlags::GET_INVALIDATED_PROPERTIES,
            None,
            Some(&bus_name),
            &object_path,
            ITEM_INTERFACE,
            None::<&gio::Cancellable>,
            move |result| {
                let this = match this_weak.upgrade() {
                    Some(t) => t,
                    None => return,
                };

                this.pending_proxies.borrow_mut().remove(&key_clone);

                let proxy = match result {
                    Ok(p) => p,
                    Err(e) => {
                        error!("Failed to create proxy for {}: {}", key_clone, e);
                        return;
                    }
                };

                this.setup_item_proxy(&key_clone, proxy);
            },
        );
    }

    fn setup_item_proxy(self: &Rc<Self>, key: &str, proxy: gio::DBusProxy) {
        let key_owned = key.to_string();

        // Connect to properties changed using connect_local (doesn't require Send+Sync)
        let this_weak = Rc::downgrade(self);
        let key_for_props = key_owned.clone();
        proxy.connect_local("g-properties-changed", false, move |values| {
            if let Some(this) = this_weak.upgrade() {
                let invalidated = values
                    .get(2)
                    .and_then(|v| v.get::<Vec<String>>().ok())
                    .unwrap_or_default();
                let props: HashSet<String> = invalidated.into_iter().collect();
                this.queue_debounced_update(&key_for_props, props, false);
            }
            None
        });

        // Connect to name owner changes using connect_local
        // When name owner disappears, clean up both internal state AND watcher registry
        let this_weak = Rc::downgrade(self);
        let key_for_owner = key_owned.clone();
        proxy.connect_local("notify::g-name-owner", false, move |values| {
            if let Some(this) = this_weak.upgrade() {
                // Check if the proxy still has an owner
                if let Some(proxy_value) = values.first()
                    && let Ok(proxy) = proxy_value.get::<gio::DBusProxy>()
                    && proxy.name_owner().is_none()
                {
                    // Use on_item_name_vanished to properly clean up registered_items
                    // and emit the unregistered signal (if we're the watcher)
                    this.on_item_name_vanished(&key_for_owner);
                }
            }
            None
        });

        // Connect to signals using connect_local
        let this_weak = Rc::downgrade(self);
        let key_for_signal = key_owned.clone();
        proxy.connect_local("g-signal", false, move |values| {
            if let Some(this) = this_weak.upgrade() {
                let signal_name = values
                    .get(2)
                    .and_then(|v| v.get::<String>().ok())
                    .unwrap_or_default();

                if MENU_RESET_SIGNALS.contains(&signal_name.as_str()) {
                    this.menu_proxies.borrow_mut().remove(&key_for_signal);
                }

                if SNAPSHOT_SIGNAL_NAMES.contains(&signal_name.as_str()) {
                    // Signal-to-properties mapping for efficient updates
                    let affected_props: HashSet<String> = match signal_name.as_str() {
                        "NewIcon" => ["IconName", "IconPixmap"]
                            .iter()
                            .map(|s| s.to_string())
                            .collect(),
                        "NewToolTip" => ["ToolTip"].iter().map(|s| s.to_string()).collect(),
                        "NewStatus" => ["Status"].iter().map(|s| s.to_string()).collect(),
                        "NewTitle" => ["Title"].iter().map(|s| s.to_string()).collect(),
                        "NewAttentionIcon" => ["AttentionIconName", "AttentionIconPixmap"]
                            .iter()
                            .map(|s| s.to_string())
                            .collect(),
                        "NewOverlayIcon" => ["OverlayIconName", "OverlayIconPixmap"]
                            .iter()
                            .map(|s| s.to_string())
                            .collect(),
                        "NewIconThemePath" => {
                            ["IconThemePath"].iter().map(|s| s.to_string()).collect()
                        }
                        _ => HashSet::new(),
                    };
                    this.queue_debounced_update(&key_for_signal, affected_props, true);
                }
            }
            None
        });

        self.proxies
            .borrow_mut()
            .insert(key_owned.clone(), proxy.clone());
        self.refresh_snapshot(&key_owned, &proxy, true);
    }

    fn remove_item(&self, identifier: &str) {
        self.items.borrow_mut().remove(identifier);
        self.proxies.borrow_mut().remove(identifier);
        self.menu_proxies.borrow_mut().remove(identifier);

        // Clean up debouncing state
        self.pending_updates.borrow_mut().remove(identifier);
        if let Some(timer_id) = self.debounce_timers.borrow_mut().remove(identifier) {
            timer_id.remove();
        }

        self.pending_proxies.borrow_mut().remove(identifier);

        self.notify_listeners();
    }

    fn queue_debounced_update(
        self: &Rc<Self>,
        identifier: &str,
        affected_props: HashSet<String>,
        reload_properties: bool,
    ) {
        // Cancel existing timer for this item
        if let Some(timer_id) = self.debounce_timers.borrow_mut().remove(identifier) {
            timer_id.remove();
        }

        // Accumulate pending properties
        self.pending_updates
            .borrow_mut()
            .entry(identifier.to_string())
            .or_default()
            .extend(affected_props);

        // Set new timer
        let this_weak = Rc::downgrade(self);
        let identifier_owned = identifier.to_string();

        let timer_id = glib::timeout_add_local_once(
            std::time::Duration::from_millis(UPDATE_DEBOUNCE_MS as u64),
            move || {
                if let Some(this) = this_weak.upgrade() {
                    this.process_debounced_update(&identifier_owned, reload_properties);
                }
            },
        );

        self.debounce_timers
            .borrow_mut()
            .insert(identifier.to_string(), timer_id);
    }

    fn process_debounced_update(&self, identifier: &str, reload_properties: bool) {
        self.debounce_timers.borrow_mut().remove(identifier);

        let pending_props = self.pending_updates.borrow_mut().remove(identifier);
        if pending_props.is_none() || pending_props.as_ref().map(|p| p.is_empty()).unwrap_or(true) {
            return;
        }

        let proxy = match self.proxies.borrow().get(identifier).cloned() {
            Some(p) => p,
            None => return,
        };

        if reload_properties {
            self.fetch_proxy_properties_async(identifier, &proxy);
        } else if let Some(snapshot) = self.snapshot_from_proxy(identifier, &proxy, None) {
            self.items
                .borrow_mut()
                .insert(identifier.to_string(), snapshot);
            self.notify_listeners();
        }
    }

    fn refresh_snapshot(&self, identifier: &str, proxy: &gio::DBusProxy, reload_properties: bool) {
        if reload_properties {
            // Build initial snapshot from cached properties
            if let Some(snapshot) = self.snapshot_from_proxy(identifier, proxy, None) {
                self.items
                    .borrow_mut()
                    .insert(identifier.to_string(), snapshot);
                self.notify_listeners();
            }

            self.fetch_proxy_properties_async(identifier, proxy);
        } else if let Some(snapshot) = self.snapshot_from_proxy(identifier, proxy, None) {
            self.items
                .borrow_mut()
                .insert(identifier.to_string(), snapshot);
            self.notify_listeners();
        }
    }

    fn fetch_proxy_properties_async(&self, identifier: &str, proxy: &gio::DBusProxy) {
        let identifier_owned = identifier.to_string();

        proxy.call(
            "org.freedesktop.DBus.Properties.GetAll",
            Some(&(ITEM_INTERFACE,).to_variant()),
            gio::DBusCallFlags::NONE,
            5000,
            None::<&gio::Cancellable>,
            move |result| {
                let this = TrayService::global();

                let proxy = match this.proxies.borrow().get(&identifier_owned).cloned() {
                    Some(p) => p,
                    None => return,
                };

                // Check if proxy still has an owner
                if proxy.name_owner().is_none() {
                    this.remove_item(&identifier_owned);
                    return;
                }

                let variant = match result {
                    Ok(v) => v,
                    Err(e) => {
                        let msg = e.to_string();
                        if msg.contains("ServiceUnknown") || msg.contains("NameHasNoOwner") {
                            debug!(
                                "TrayService: item {} disappeared or is not activatable while refreshing properties: {}",
                                identifier_owned, msg
                            );
                            this.remove_item(&identifier_owned);
                        } else {
                            error!(
                                "TrayService: failed to refresh properties for {}: {}",
                                identifier_owned, msg
                            );
                        }
                        return;
                    }
                };

                let overrides = this.parse_properties_result(&variant);
                if let Some(snapshot) = this.snapshot_from_proxy(&identifier_owned, &proxy, overrides.as_ref()) {
                    this.items.borrow_mut().insert(identifier_owned, snapshot);
                    this.notify_listeners();
                }
            },
        );
    }

    fn parse_properties_result(&self, result: &Variant) -> Option<HashMap<String, Variant>> {
        // Result is (a{sv}) - tuple containing dict
        let inner = result.child_value(0);
        let mut map = HashMap::new();

        for i in 0..inner.n_children() {
            let entry = inner.child_value(i);
            if entry.n_children() >= 2
                && let Some(key) = entry.child_value(0).str()
            {
                let value = entry.child_value(1);
                // Unwrap the variant wrapper if present
                let actual_value = if value.type_().is_variant() {
                    value.child_value(0)
                } else {
                    value
                };
                map.insert(key.to_string(), actual_value);
            }
        }

        if map.is_empty() { None } else { Some(map) }
    }

    fn snapshot_from_proxy(
        &self,
        identifier: &str,
        proxy: &gio::DBusProxy,
        overrides: Option<&HashMap<String, Variant>>,
    ) -> Option<TrayItem> {
        let get_prop = |name: &str| -> Option<Variant> {
            if let Some(map) = overrides
                && let Some(v) = map.get(name)
            {
                return Some(v.clone());
            }
            proxy.cached_property(name)
        };

        let status = get_prop("Status")
            .and_then(|v| v.str().map(|s| s.to_string()))
            .unwrap_or_else(|| "Passive".to_string());

        let icon_name = get_prop("IconName").and_then(|v| v.str().map(|s| s.to_string()));
        let attention_icon_name =
            get_prop("AttentionIconName").and_then(|v| v.str().map(|s| s.to_string()));
        let pixmap = self.pixmap_from_variant(get_prop("IconPixmap"));
        let attention_pixmap = self.pixmap_from_variant(get_prop("AttentionIconPixmap"));
        let title = get_prop("Title")
            .and_then(|v| v.str().map(|s| s.to_string()))
            .unwrap_or_default();
        let tooltip = self.extract_tooltip(get_prop("ToolTip"));
        let menu_path = get_prop("Menu").and_then(|v| v.str().map(|s| s.to_string()));
        let item_is_menu = get_prop("ItemIsMenu")
            .and_then(|v| v.get::<bool>())
            .unwrap_or(false);
        let icon_theme_path =
            get_prop("IconThemePath").and_then(|v| v.str().map(|s| s.to_string()));

        Some(TrayItem {
            identifier: identifier.to_string(),
            title,
            tooltip,
            status,
            icon_name,
            attention_icon_name,
            pixmap,
            attention_pixmap,
            menu_path,
            bus_name: proxy.name().map(|s| s.to_string()).unwrap_or_default(),
            item_is_menu,
            icon_theme_path,
        })
    }

    fn pixmap_from_variant(&self, value: Option<Variant>) -> Option<TrayPixmap> {
        let variant = value?;

        // IconPixmap is a(iiay) - array of (width, height, data)
        let n_children = variant.n_children();
        if n_children == 0 {
            return None;
        }

        // Find the largest valid pixmap
        let mut best: Option<(i32, i32, Vec<u8>)> = None;

        for i in 0..n_children {
            let child = variant.child_value(i);
            if child.n_children() < 3 {
                continue;
            }

            let width = child.child_value(0).get::<i32>().unwrap_or(0);
            let height = child.child_value(1).get::<i32>().unwrap_or(0);

            if width <= 0 || height <= 0 {
                continue;
            }

            // Get the byte array - it might be ay or v containing ay
            let data_variant = child.child_value(2);
            let Some(data) = Self::extract_bytes_from_variant(&data_variant) else {
                continue;
            };

            // Validate buffer size: must be at least width * height * 4 bytes (ARGB)
            let expected_size = (width as usize) * (height as usize) * 4;
            if data.len() < expected_size {
                debug!(
                    "Pixmap buffer too small: got {} bytes, expected {} for {}x{}",
                    data.len(),
                    expected_size,
                    width,
                    height
                );
                continue;
            }

            if best.is_none()
                || (width * height) > (best.as_ref().unwrap().0 * best.as_ref().unwrap().1)
            {
                best = Some((width, height, data));
            }
        }

        let (width, height, buffer) = best?;
        let hash_key = self.hash_bytes(&buffer);

        Some(TrayPixmap {
            width,
            height,
            buffer: glib::Bytes::from_owned(buffer),
            hash_key,
        })
    }

    fn extract_bytes_from_variant(variant: &Variant) -> Option<Vec<u8>> {
        // Try to get as byte array directly using fixed_array
        if let Ok(bytes) = variant.fixed_array::<u8>() {
            return Some(bytes.to_vec());
        }

        // Try unpacking nested variant
        if variant.type_().is_variant() {
            let inner = variant.child_value(0);
            return Self::extract_bytes_from_variant(&inner);
        }

        // Try as array of bytes by iterating children
        let mut bytes = Vec::new();
        for i in 0..variant.n_children() {
            if let Some(b) = variant.child_value(i).get::<u8>() {
                bytes.push(b);
            }
        }

        if bytes.is_empty() { None } else { Some(bytes) }
    }

    fn hash_bytes(&self, data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        format!("{:x}", hasher.finalize())
    }

    fn extract_tooltip(&self, value: Option<Variant>) -> Option<String> {
        let variant = value?;

        // ToolTip is (sa(iiay)ss) - (icon_name, icon_pixmap, title, description)
        if variant.n_children() < 4 {
            // Maybe it's just a string
            return variant.str().map(|s| s.to_string());
        }

        // Try description (index 3) first
        let description = variant.child_value(3);
        if let Some(s) = description.str()
            && !s.is_empty()
        {
            return Some(s.to_string());
        }

        // Fall back to title (index 2)
        let title = variant.child_value(2);
        if let Some(s) = title.str()
            && !s.is_empty()
        {
            return Some(s.to_string());
        }

        None
    }

    fn ensure_menu_proxy(&self, identifier: &str) -> Option<gio::DBusProxy> {
        // Check if we already have a menu proxy
        if let Some(proxy) = self.menu_proxies.borrow().get(identifier).cloned() {
            return Some(proxy);
        }

        // Get item info
        let item = self.items.borrow().get(identifier).cloned()?;
        let menu_path = item.menu_path?;

        // Create menu proxy synchronously (called when user opens menu)
        let proxy = match gio::DBusProxy::for_bus_sync(
            gio::BusType::Session,
            gio::DBusProxyFlags::DO_NOT_CONNECT_SIGNALS,
            None,
            &item.bus_name,
            &menu_path,
            DBUSMENU_INTERFACE,
            None::<&gio::Cancellable>,
        ) {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to create menu proxy for {}: {}", identifier, e);
                return None;
            }
        };

        self.menu_proxies
            .borrow_mut()
            .insert(identifier.to_string(), proxy.clone());

        Some(proxy)
    }

    fn parse_layout_result(&self, result: &Variant) -> Vec<TrayMenuEntry> {
        // Result is (u(ia{sv}av)) - (revision, layout)
        if result.n_children() < 2 {
            return Vec::new();
        }

        let layout = result.child_value(1);
        self.parse_layout_node(&layout)
    }

    fn parse_layout_node(&self, node: &Variant) -> Vec<TrayMenuEntry> {
        // Layout node is (ia{sv}av) - (id, properties, children)
        if node.n_children() < 3 {
            return Vec::new();
        }

        let children_variant = node.child_value(2);
        let mut entries = Vec::new();

        for i in 0..children_variant.n_children() {
            let child = children_variant.child_value(i);
            // Child might be wrapped in a variant
            let actual_child = if child.type_().is_variant() {
                child.child_value(0)
            } else {
                child
            };

            if let Some(entry) = self.node_to_entry(&actual_child) {
                entries.push(entry);
            }
        }

        entries
    }

    fn node_to_entry(&self, node: &Variant) -> Option<TrayMenuEntry> {
        if node.n_children() < 3 {
            return None;
        }

        let menu_id = node.child_value(0).get::<i32>().unwrap_or(0);
        let props = self.parse_menu_properties(&node.child_value(1));

        // Check visibility
        let visible = props
            .get("visible")
            .and_then(|v| v.get::<bool>())
            .unwrap_or(true);
        if !visible {
            return None;
        }

        // Check if separator
        let entry_type = props.get("type").and_then(|v| v.str()).unwrap_or("");
        if entry_type == "separator" {
            return Some(TrayMenuEntry {
                menu_id,
                label: String::new(),
                enabled: false,
                is_separator: true,
                toggle_type: None,
                toggle_state: None,
                children: Vec::new(),
            });
        }

        // Get label and clean it (remove mnemonics)
        let mut label = props
            .get("label")
            .and_then(|v| v.str())
            .unwrap_or("")
            .to_string();

        // Replace __ with placeholder, remove _, restore placeholder
        label = label
            .replace("__", "\u{FFFF}")
            .replace('_', "")
            .replace('\u{FFFF}', "_");

        let enabled = props
            .get("enabled")
            .and_then(|v| v.get::<bool>())
            .unwrap_or(true);
        let toggle_type = props
            .get("toggle-type")
            .and_then(|v| v.str().map(|s| s.to_string()));
        let toggle_state = props.get("toggle-state").and_then(|v| v.get::<i32>());

        // Parse children recursively
        let children = self.parse_layout_node(node);

        Some(TrayMenuEntry {
            menu_id,
            label,
            enabled,
            is_separator: false,
            toggle_type,
            toggle_state,
            children,
        })
    }

    fn parse_menu_properties(&self, props_variant: &Variant) -> HashMap<String, Variant> {
        let mut map = HashMap::new();

        for i in 0..props_variant.n_children() {
            let entry = props_variant.child_value(i);
            if entry.n_children() >= 2
                && let Some(key) = entry.child_value(0).str()
            {
                let value = entry.child_value(1);
                // Unwrap variant wrapper if present
                let actual_value = if value.type_().is_variant() {
                    value.child_value(0)
                } else {
                    value
                };
                map.insert(key.to_string(), actual_value);
            }
        }

        map
    }

    fn split_identifier(&self, identifier: &str) -> Option<(String, String)> {
        if identifier.is_empty() {
            return None;
        }

        if !identifier.contains('/') {
            return Some((identifier.to_string(), "/StatusNotifierItem".to_string()));
        }

        let idx = identifier.find('/').unwrap();
        let bus_name = &identifier[..idx];
        let path = &identifier[idx..];

        if bus_name.is_empty() {
            return None;
        }

        let path = if path.is_empty() {
            "/StatusNotifierItem".to_string()
        } else {
            path.to_string()
        };

        Some((bus_name.to_string(), path))
    }

    fn make_identifier(&self, bus_name: &str, object_path: &str) -> String {
        format!("{}{}", bus_name, object_path)
    }

    fn set_ready(&self) {
        if !self.ready.get() {
            self.ready.set(true);
            self.notify_listeners();
        }
    }

    fn notify_listeners(&self) {
        self.callbacks.notify(self);
    }
}

impl Drop for TrayService {
    fn drop(&mut self) {
        debug!("TrayService dropped");
    }
}
