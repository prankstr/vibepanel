//! BluetoothService - Bluetooth adapter and device state via BlueZ over D-Bus.
//!
//! This service provides:
//!   - Discovery of a single adapter (org.bluez.Adapter1)
//!   - Snapshot of adapter power state and devices
//!   - Debounced updates on adapter/device property changes
//!   - Simple control API: power, scan, connect/disconnect, pair, forget

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::gio::{self, BusType, DBusCallFlags, DBusProxy, DBusProxyFlags, prelude::*};
use gtk4::glib::{self, Variant};
use tracing::error;

use super::callbacks::Callbacks;

// BlueZ D-Bus constants
const BLUEZ_SERVICE: &str = "org.bluez";
const ADAPTER_PATH: &str = "/org/bluez/hci0";
const ADAPTER_IFACE: &str = "org.bluez.Adapter1";
const DEVICE_IFACE: &str = "org.bluez.Device1";
const OBJECT_MANAGER_IFACE: &str = "org.freedesktop.DBus.ObjectManager";
const PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";

/// Debounce interval (in ms) for device list updates. BlueZ emits multiple
/// property changes in quick succession; this batches them into one UI update.
const DEVICE_UPDATE_DEBOUNCE_MS: u64 = 100;

/// Duration (in seconds) after which discovery scan is automatically stopped.
const SCAN_AUTO_STOP_SECS: u32 = 8;

/// A single Bluetooth device exposed by BlueZ.
#[derive(Debug, Clone)]
pub struct BluetoothDevice {
    pub path: String,
    pub name: String,
    pub address: String,
    pub connected: bool,
    pub paired: bool,
    pub icon: Option<String>,
}

/// Canonical snapshot of Bluetooth state.
#[derive(Debug, Clone)]
pub struct BluetoothSnapshot {
    /// Whether we have discovered at least one adapter.
    pub has_adapter: bool,
    /// Whether the adapter is powered.
    pub powered: bool,
    /// Number of currently connected devices.
    pub connected_devices: usize,
    /// All known devices (paired and unpaired) from BlueZ.
    pub devices: Vec<BluetoothDevice>,
    /// Whether a discovery scan is in progress.
    pub scanning: bool,
    /// Whether the service has produced an initial snapshot.
    pub is_ready: bool,
}

impl BluetoothSnapshot {
    fn empty() -> Self {
        Self {
            has_adapter: false,
            powered: false,
            connected_devices: 0,
            devices: Vec::new(),
            scanning: false,
            is_ready: false,
        }
    }
}

/// Process-wide Bluetooth service for adapter and device management.
pub struct BluetoothService {
    /// System bus connection.
    connection: RefCell<Option<gio::DBusConnection>>,
    /// Primary adapter proxy (hci0).
    adapter: RefCell<Option<DBusProxy>>,
    /// ObjectManager proxy at /
    object_manager: RefCell<Option<DBusProxy>>,
    /// Current snapshot of Bluetooth state.
    snapshot: RefCell<BluetoothSnapshot>,
    /// Registered callbacks for snapshot changes.
    callbacks: Callbacks<BluetoothSnapshot>,
    /// True while a discovery scan is active.
    scan_in_progress: Cell<bool>,
    /// Debounce source ID for batched state updates.
    debounce_id: RefCell<Option<glib::SourceId>>,
    /// D-Bus signal subscriptions (kept alive for the service lifetime).
    _signal_subscriptions: RefCell<Vec<gio::SignalSubscription>>,
}

impl BluetoothService {
    fn new() -> Rc<Self> {
        let service = Rc::new(Self {
            connection: RefCell::new(None),
            adapter: RefCell::new(None),
            object_manager: RefCell::new(None),
            snapshot: RefCell::new(BluetoothSnapshot::empty()),
            callbacks: Callbacks::new(),
            scan_in_progress: Cell::new(false),
            debounce_id: RefCell::new(None),
            _signal_subscriptions: RefCell::new(Vec::new()),
        });

        Self::init_dbus(&service);
        service
    }

    /// Global singleton instance.
    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<BluetoothService> = BluetoothService::new();
        }

        INSTANCE.with(|s| s.clone())
    }

    /// Register a callback to be invoked whenever the Bluetooth snapshot changes.
    pub fn connect<F>(&self, callback: F)
    where
        F: Fn(&BluetoothSnapshot) + 'static,
    {
        self.callbacks.register(callback);

        // Immediately send current snapshot.
        let snapshot = self.snapshot.borrow().clone();
        self.callbacks.notify(&snapshot);
    }

    /// Return the current snapshot.
    pub fn snapshot(&self) -> BluetoothSnapshot {
        self.snapshot.borrow().clone()
    }

    // D-Bus initialisation

    fn init_dbus(this: &Rc<Self>) {
        let this_weak = Rc::downgrade(this);

        gio::bus_get(BusType::System, None::<&gio::Cancellable>, move |res| {
            let this = match this_weak.upgrade() {
                Some(s) => s,
                None => return,
            };

            let connection = match res {
                Ok(c) => c,
                Err(e) => {
                    error!("BluetoothService: failed to get system bus: {}", e);
                    return;
                }
            };

            this.connection.replace(Some(connection.clone()));

            // Subscribe to all PropertiesChanged signals from BlueZ
            let this_weak2 = Rc::downgrade(&this);
            let sub1 = connection.subscribe_to_signal(
                Some(BLUEZ_SERVICE),
                Some(PROPERTIES_IFACE),
                Some("PropertiesChanged"),
                None, // any object path
                None, // arg0 filter
                gio::DBusSignalFlags::NONE,
                move |_signal| {
                    if let Some(this) = this_weak2.upgrade() {
                        this.update_state_debounced();
                    }
                },
            );

            // Subscribe to InterfacesAdded/Removed from ObjectManager
            let this_weak3 = Rc::downgrade(&this);
            let sub2 = connection.subscribe_to_signal(
                Some(BLUEZ_SERVICE),
                Some(OBJECT_MANAGER_IFACE),
                Some("InterfacesAdded"),
                None,
                None,
                gio::DBusSignalFlags::NONE,
                move |_signal| {
                    if let Some(this) = this_weak3.upgrade() {
                        this.update_state_debounced();
                    }
                },
            );

            let this_weak4 = Rc::downgrade(&this);
            let sub3 = connection.subscribe_to_signal(
                Some(BLUEZ_SERVICE),
                Some(OBJECT_MANAGER_IFACE),
                Some("InterfacesRemoved"),
                None,
                None,
                gio::DBusSignalFlags::NONE,
                move |_signal| {
                    if let Some(this) = this_weak4.upgrade() {
                        this.update_state_debounced();
                    }
                },
            );

            // Store subscriptions to keep them alive
            this._signal_subscriptions
                .borrow_mut()
                .extend([sub1, sub2, sub3]);

            // Create ObjectManager proxy
            let this_weak5 = Rc::downgrade(&this);
            DBusProxy::new(
                &connection,
                DBusProxyFlags::NONE,
                None,
                Some(BLUEZ_SERVICE),
                "/",
                OBJECT_MANAGER_IFACE,
                None::<&gio::Cancellable>,
                move |res| {
                    let this = match this_weak5.upgrade() {
                        Some(s) => s,
                        None => return,
                    };

                    match res {
                        Ok(proxy) => {
                            this.object_manager.replace(Some(proxy));
                        }
                        Err(e) => {
                            error!(
                                "BluetoothService: failed to create ObjectManager proxy: {}",
                                e
                            );
                        }
                    }
                },
            );

            // Create Adapter1 proxy
            let this_weak6 = Rc::downgrade(&this);
            DBusProxy::new(
                &connection,
                DBusProxyFlags::NONE,
                None,
                Some(BLUEZ_SERVICE),
                ADAPTER_PATH,
                ADAPTER_IFACE,
                None::<&gio::Cancellable>,
                move |res| {
                    let this = match this_weak6.upgrade() {
                        Some(s) => s,
                        None => return,
                    };

                    match res {
                        Ok(proxy) => {
                            // Monitor for BlueZ service appearing/disappearing.
                            let this_weak = Rc::downgrade(&this);
                            proxy.connect_local("notify::g-name-owner", false, move |values| {
                                let this = this_weak.upgrade()?;
                                let proxy = values[0].get::<gio::DBusProxy>().ok();
                                let has_owner = proxy.and_then(|p| p.name_owner()).is_some();
                                if has_owner {
                                    // Service reappeared - refresh state.
                                    this.update_state();
                                } else {
                                    // Service disappeared - mark unavailable.
                                    this.set_unavailable();
                                }
                                None
                            });

                            this.adapter.replace(Some(proxy));
                            this.update_state();
                        }
                        Err(e) => {
                            // No adapter might be normal (no Bluetooth hardware)
                            error!("BluetoothService: failed to create Adapter1 proxy: {}", e);
                            this.update_state();
                        }
                    }
                },
            );
        });
    }

    fn set_unavailable(&self) {
        let mut snapshot = self.snapshot.borrow_mut();
        if !snapshot.has_adapter && !snapshot.is_ready {
            return; // Already unavailable
        }
        *snapshot = BluetoothSnapshot::empty();
        let snapshot_clone = snapshot.clone();
        drop(snapshot);
        self.callbacks.notify(&snapshot_clone);

        // Clear proxies.
        self.adapter.replace(None);
        self.object_manager.replace(None);
    }

    fn update_state_debounced(&self) {
        if self.debounce_id.borrow().is_some() {
            return;
        }

        let this_weak = Rc::downgrade(&BluetoothService::global());
        let id = glib::timeout_add_local(
            std::time::Duration::from_millis(DEVICE_UPDATE_DEBOUNCE_MS),
            move || {
                if let Some(this) = this_weak.upgrade() {
                    *this.debounce_id.borrow_mut() = None;
                    this.update_state();
                }
                glib::ControlFlow::Break
            },
        );

        *self.debounce_id.borrow_mut() = Some(id);
    }

    fn update_state(&self) {
        let adapter = self.adapter.borrow().clone();
        let object_manager = self.object_manager.borrow().clone();
        let connection = self.connection.borrow().clone();

        let Some(_connection) = connection else {
            let mut snapshot = self.snapshot.borrow_mut();
            *snapshot = BluetoothSnapshot::empty();
            let snapshot_clone = snapshot.clone();
            drop(snapshot);
            self.callbacks.notify(&snapshot_clone);
            return;
        };

        let has_adapter = adapter.is_some();
        let powered = adapter
            .as_ref()
            .and_then(|p| p.cached_property("Powered"))
            .and_then(|v| v.get::<bool>())
            .unwrap_or(false);

        // Get managed objects to enumerate devices
        let this = BluetoothService::global();
        if let Some(om) = object_manager {
            let this_weak = Rc::downgrade(&this);
            om.call(
                "GetManagedObjects",
                None,
                DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
                move |res| {
                    let this = match this_weak.upgrade() {
                        Some(s) => s,
                        None => return,
                    };

                    let devices = match res {
                        Ok(result) => this.parse_managed_objects(&result),
                        Err(_) => Vec::new(),
                    };

                    let connected_count = devices.iter().filter(|d| d.connected).count();

                    let adapter = this.adapter.borrow().clone();
                    let has_adapter = adapter.is_some();
                    let powered = adapter
                        .as_ref()
                        .and_then(|p| p.cached_property("Powered"))
                        .and_then(|v| v.get::<bool>())
                        .unwrap_or(false);

                    let mut snapshot = this.snapshot.borrow_mut();
                    snapshot.has_adapter = has_adapter;
                    snapshot.powered = powered;
                    snapshot.connected_devices = connected_count;
                    snapshot.devices = devices;
                    snapshot.scanning = this.scan_in_progress.get();
                    snapshot.is_ready = true;

                    let snapshot_clone = snapshot.clone();
                    drop(snapshot);
                    this.callbacks.notify(&snapshot_clone);
                },
            );
        } else {
            // No object manager yet, just update what we know
            let mut snapshot = self.snapshot.borrow_mut();
            snapshot.has_adapter = has_adapter;
            snapshot.powered = powered;
            snapshot.scanning = self.scan_in_progress.get();
            snapshot.is_ready = true;

            let snapshot_clone = snapshot.clone();
            drop(snapshot);
            self.callbacks.notify(&snapshot_clone);
        }
    }

    fn parse_managed_objects(&self, result: &Variant) -> Vec<BluetoothDevice> {
        let mut devices = Vec::new();

        // Result is (a{oa{sa{sv}}},) - tuple containing dict of object paths
        let Some(inner) = result.child_value(0).get::<glib::VariantDict>() else {
            // Try iterating as array
            let inner = result.child_value(0);
            let n = inner.n_children();
            for i in 0..n {
                let entry = inner.child_value(i);
                if let Some(dev) = self.parse_object_entry(&entry) {
                    devices.push(dev);
                }
            }
            devices.sort_by(|a, b| {
                let key_a = (!a.connected, !a.paired, a.name.to_lowercase());
                let key_b = (!b.connected, !b.paired, b.name.to_lowercase());
                key_a.cmp(&key_b)
            });
            return devices;
        };

        // VariantDict approach
        drop(inner);

        // Fallback: iterate children of the dict variant
        let dict = result.child_value(0);
        let n = dict.n_children();
        for i in 0..n {
            let entry = dict.child_value(i);
            if let Some(dev) = self.parse_object_entry(&entry) {
                devices.push(dev);
            }
        }

        // Sort: connected first, then paired, then by name
        devices.sort_by(|a, b| {
            let key_a = (!a.connected, !a.paired, a.name.to_lowercase());
            let key_b = (!b.connected, !b.paired, b.name.to_lowercase());
            key_a.cmp(&key_b)
        });

        devices
    }

    fn parse_object_entry(&self, entry: &Variant) -> Option<BluetoothDevice> {
        // Entry is {o, a{sa{sv}}} - object path and dict of interfaces
        let path: String = entry.child_value(0).get()?;

        // Skip non-device paths
        if !path.starts_with("/org/bluez/hci") || !path.contains("/dev_") {
            return None;
        }

        let ifaces = entry.child_value(1);
        let n_ifaces = ifaces.n_children();

        for j in 0..n_ifaces {
            let iface_entry = ifaces.child_value(j);
            let iface_name: String = iface_entry.child_value(0).get()?;

            if iface_name != DEVICE_IFACE {
                continue;
            }

            let props = iface_entry.child_value(1);
            return Some(self.parse_device_properties(&path, &props));
        }

        None
    }

    fn parse_device_properties(&self, path: &str, props: &Variant) -> BluetoothDevice {
        let mut address = String::new();
        let mut name = String::new();
        let mut connected = false;
        let mut paired = false;
        let mut icon: Option<String> = None;

        let n = props.n_children();
        for i in 0..n {
            let prop = props.child_value(i);
            let key: Option<String> = prop.child_value(0).get();
            let Some(key) = key else { continue };

            let value = prop.child_value(1);
            // value is a variant containing the actual value
            let inner = value.child_value(0);

            match key.as_str() {
                "Address" => address = inner.get::<String>().unwrap_or_default(),
                "Name" => name = inner.get::<String>().unwrap_or_default(),
                "Connected" => connected = inner.get::<bool>().unwrap_or(false),
                "Paired" => paired = inner.get::<bool>().unwrap_or(false),
                "Icon" => icon = inner.get::<String>(),
                _ => {}
            }
        }

        if name.is_empty() {
            name = if address.is_empty() {
                "Unknown".to_string()
            } else {
                address.clone()
            };
        }

        BluetoothDevice {
            path: path.to_string(),
            name,
            address,
            connected,
            paired,
            icon,
        }
    }

    // Public control API

    pub fn set_powered(&self, enabled: bool) {
        let Some(adapter) = self.adapter.borrow().clone() else {
            return;
        };

        let variant = Variant::tuple_from_iter([
            ADAPTER_IFACE.to_variant(),
            "Powered".to_variant(),
            glib::Variant::from_variant(&enabled.to_variant()),
        ]);

        adapter.call(
            "org.freedesktop.DBus.Properties.Set",
            Some(&variant),
            DBusCallFlags::NONE,
            5000,
            None::<&gio::Cancellable>,
            |res| {
                if let Err(e) = res {
                    error!("BluetoothService: set_powered failed: {}", e);
                }
            },
        );
    }

    pub fn scan_for_devices(&self) {
        let Some(adapter) = self.adapter.borrow().clone() else {
            return;
        };

        if self.scan_in_progress.get() {
            return;
        }

        self.scan_in_progress.set(true);
        {
            let mut snapshot = self.snapshot.borrow_mut();
            snapshot.scanning = true;
            let snapshot_clone = snapshot.clone();
            drop(snapshot);
            self.callbacks.notify(&snapshot_clone);
        }

        // Start discovery
        adapter.call(
            "StartDiscovery",
            None,
            DBusCallFlags::NONE,
            5000,
            None::<&gio::Cancellable>,
            move |res| {
                if let Err(e) = res {
                    error!("BluetoothService: StartDiscovery failed: {}", e);
                }
            },
        );

        // Schedule StopDiscovery after timeout to limit scan duration.
        let this_weak = Rc::downgrade(&BluetoothService::global());
        glib::timeout_add_seconds_local(SCAN_AUTO_STOP_SECS, move || {
            if let Some(this) = this_weak.upgrade() {
                if let Some(adapter) = this.adapter.borrow().clone() {
                    adapter.call(
                        "StopDiscovery",
                        None,
                        DBusCallFlags::NONE,
                        5000,
                        None::<&gio::Cancellable>,
                        |res| {
                            if let Err(e) = res {
                                error!("BluetoothService: StopDiscovery failed: {}", e);
                            }
                        },
                    );
                }
                this.scan_in_progress.set(false);
                let mut snapshot = this.snapshot.borrow_mut();
                snapshot.scanning = false;
                let snapshot_clone = snapshot.clone();
                drop(snapshot);
                this.callbacks.notify(&snapshot_clone);
            }
            glib::ControlFlow::Break
        });
    }

    fn get_device_proxy(&self, path_or_address: &str) -> Option<(String, gio::DBusConnection)> {
        let connection = self.connection.borrow().clone()?;
        let snapshot = self.snapshot.borrow();

        // Find by path or address
        for dev in &snapshot.devices {
            if dev.path == path_or_address || dev.address == path_or_address {
                return Some((dev.path.clone(), connection));
            }
        }

        None
    }

    pub fn connect_device(&self, path_or_address: &str) {
        let Some((path, connection)) = self.get_device_proxy(path_or_address) else {
            return;
        };

        DBusProxy::new(
            &connection,
            DBusProxyFlags::NONE,
            None,
            Some(BLUEZ_SERVICE),
            &path,
            DEVICE_IFACE,
            None::<&gio::Cancellable>,
            move |res| {
                match res {
                    Ok(proxy) => {
                        proxy.call(
                            "Connect",
                            None,
                            DBusCallFlags::NONE,
                            30000, // Bluetooth connections can take time
                            None::<&gio::Cancellable>,
                            |res| {
                                if let Err(e) = res {
                                    error!("BluetoothService: Connect failed: {}", e);
                                }
                            },
                        );
                    }
                    Err(e) => {
                        error!("BluetoothService: failed to create device proxy: {}", e);
                    }
                }
            },
        );
    }

    pub fn disconnect_device(&self, path_or_address: &str) {
        let Some((path, connection)) = self.get_device_proxy(path_or_address) else {
            return;
        };

        DBusProxy::new(
            &connection,
            DBusProxyFlags::NONE,
            None,
            Some(BLUEZ_SERVICE),
            &path,
            DEVICE_IFACE,
            None::<&gio::Cancellable>,
            move |res| match res {
                Ok(proxy) => {
                    proxy.call(
                        "Disconnect",
                        None,
                        DBusCallFlags::NONE,
                        5000,
                        None::<&gio::Cancellable>,
                        |res| {
                            if let Err(e) = res {
                                error!("BluetoothService: Disconnect failed: {}", e);
                            }
                        },
                    );
                }
                Err(e) => {
                    error!("BluetoothService: failed to create device proxy: {}", e);
                }
            },
        );
    }

    pub fn pair_device(&self, path_or_address: &str) {
        let Some((path, connection)) = self.get_device_proxy(path_or_address) else {
            return;
        };

        DBusProxy::new(
            &connection,
            DBusProxyFlags::NONE,
            None,
            Some(BLUEZ_SERVICE),
            &path,
            DEVICE_IFACE,
            None::<&gio::Cancellable>,
            move |res| {
                match res {
                    Ok(proxy) => {
                        let proxy_for_trust = proxy.clone();
                        let proxy_for_connect = proxy.clone();

                        proxy.call(
                            "Pair",
                            None,
                            DBusCallFlags::NONE,
                            30000, // Pairing can take time
                            None::<&gio::Cancellable>,
                            move |res| {
                                let pair_err = res.err();
                                let allow_connect = match pair_err.as_ref() {
                                    None => true,
                                    Some(err) => {
                                        // BlueZ returns AlreadyExists for already-paired devices; still try to connect.
                                        err.to_string().contains("AlreadyExists")
                                    }
                                };

                                if allow_connect {
                                    if let Some(e) = pair_err {
                                        // Noise reduction: AlreadyExists is handled above.
                                        error!("BluetoothService: Pair reported error: {}", e);
                                    }

                                    let trusted_variant = Variant::tuple_from_iter([
                                        DEVICE_IFACE.to_variant(),
                                        "Trusted".to_variant(),
                                        glib::Variant::from_variant(&true.to_variant()),
                                    ]);

                                    proxy_for_trust.call(
                                        "org.freedesktop.DBus.Properties.Set",
                                        Some(&trusted_variant),
                                        DBusCallFlags::NONE,
                                        5000,
                                        None::<&gio::Cancellable>,
                                        |res| {
                                            if let Err(e) = res {
                                                error!(
                                                    "BluetoothService: failed to mark device trusted: {}",
                                                    e
                                                );
                                            }
                                        },
                                    );

                                    proxy_for_connect.call(
                                        "Connect",
                                        None,
                                        DBusCallFlags::NONE,
                                        30000, // Bluetooth connections can take time
                                        None::<&gio::Cancellable>,
                                        |res| {
                                            if let Err(e) = res {
                                                error!("BluetoothService: Connect after pair failed: {}", e);
                                            }
                                        },
                                    );
                                }
                            },
                        );
                    }
                    Err(e) => {
                        error!("BluetoothService: failed to create device proxy: {}", e);
                    }
                }
            },
        );
    }

    pub fn forget_device(&self, path_or_address: &str) {
        let Some((path, _connection)) = self.get_device_proxy(path_or_address) else {
            return;
        };
        let Some(adapter) = self.adapter.borrow().clone() else {
            return;
        };

        let obj_path = glib::variant::ObjectPath::try_from(path.as_str()).unwrap();
        let variant = Variant::tuple_from_iter([obj_path.to_variant()]);

        adapter.call(
            "RemoveDevice",
            Some(&variant),
            DBusCallFlags::NONE,
            5000,
            None::<&gio::Cancellable>,
            |res| {
                if let Err(e) = res {
                    error!("BluetoothService: Forget failed: {}", e);
                }
            },
        );
    }
}
