//! BluetoothService - Bluetooth adapter and device state via BlueZ over D-Bus.
//!
//! This service provides:
//!   - Discovery of a single adapter (org.bluez.Adapter1)
//!   - Snapshot of adapter power state and devices
//!   - Debounced updates on adapter/device property changes
//!   - Simple control API: power, scan, connect/disconnect, pair, forget
//!   - BlueZ Agent for handling pairing authentication (PIN, passkey, confirmation)

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::gio::{self, BusType, DBusCallFlags, DBusProxy, DBusProxyFlags, prelude::*};
use gtk4::glib::{self, Variant};
use tracing::{debug, error};

use super::callbacks::Callbacks;

// BlueZ D-Bus constants
const BLUEZ_SERVICE: &str = "org.bluez";
const ADAPTER_PATH: &str = "/org/bluez/hci0";
const ADAPTER_IFACE: &str = "org.bluez.Adapter1";
const DEVICE_IFACE: &str = "org.bluez.Device1";
const OBJECT_MANAGER_IFACE: &str = "org.freedesktop.DBus.ObjectManager";
const PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";
const AGENT_MANAGER_IFACE: &str = "org.bluez.AgentManager1";
const AGENT_IFACE: &str = "org.bluez.Agent1";
const AGENT_PATH: &str = "/org/vibepanel/bluetooth/agent";

/// BlueZ Agent1 interface introspection XML for D-Bus registration.
const AGENT_INTROSPECTION: &str = r#"
<node>
    <interface name="org.bluez.Agent1">
        <method name="Release" />
        <method name="RequestPinCode">
            <arg type="o" name="device" direction="in"/>
            <arg type="s" name="pincode" direction="out"/>
        </method>
        <method name="DisplayPinCode">
            <arg type="o" name="device" direction="in"/>
            <arg type="s" name="pincode" direction="in"/>
        </method>
        <method name="RequestPasskey">
            <arg type="o" name="device" direction="in"/>
            <arg type="u" name="passkey" direction="out"/>
        </method>
        <method name="DisplayPasskey">
            <arg type="o" name="device" direction="in"/>
            <arg type="u" name="passkey" direction="in"/>
            <arg type="q" name="entered" direction="in"/>
        </method>
        <method name="RequestConfirmation">
            <arg type="o" name="device" direction="in"/>
            <arg type="u" name="passkey" direction="in"/>
        </method>
        <method name="AuthorizeService">
            <arg type="o" name="device" direction="in"/>
            <arg type="s" name="uuid" direction="in"/>
        </method>
        <method name="Cancel" />
    </interface>
</node>
"#;

/// Debounce interval (in ms) for device list updates. BlueZ emits multiple
/// property changes in quick succession; this batches them into one UI update.
const DEVICE_UPDATE_DEBOUNCE_MS: u64 = 100;

/// Duration (in seconds) after which we call StopDiscovery.
/// BlueZ uses reference counting, so we must stop what we started.
const SCAN_DURATION_SECS: u32 = 10;

/// Timeout (in seconds) for user to respond to auth requests.
const AUTH_TIMEOUT_SECS: u64 = 30;

/// Check if a device name looks like a MAC address (fallback name).
/// MAC format: XX-XX-XX-XX-XX-XX or XX:XX:XX:XX:XX:XX (17 chars).
fn is_mac_like_name(name: &str) -> bool {
    name.len() == 17
        && name
            .chars()
            .nth(2)
            .map(|c| c == '-' || c == ':')
            .unwrap_or(false)
}

/// Authentication request types from the BlueZ Agent.
#[derive(Debug, Clone)]
pub enum BluetoothAuthRequest {
    /// Device requests a PIN code (user must enter it).
    /// PIN can be 1-16 alphanumeric characters.
    RequestPinCode {
        device_path: String,
        device_name: String,
    },
    /// Device requests a numeric passkey (user must enter 6-digit number).
    RequestPasskey {
        device_path: String,
        device_name: String,
    },
    /// Device requests confirmation that displayed passkey matches.
    RequestConfirmation {
        device_path: String,
        device_name: String,
        passkey: u32,
    },
    /// Display a PIN code for user to enter on the remote device.
    DisplayPinCode {
        device_path: String,
        device_name: String,
        pincode: String,
    },
    /// Display a passkey for user to enter on the remote device.
    DisplayPasskey {
        device_path: String,
        device_name: String,
        passkey: u32,
    },
}

impl BluetoothAuthRequest {
    /// Get the device path for this auth request.
    pub fn device_path(&self) -> &str {
        match self {
            Self::RequestPinCode { device_path, .. }
            | Self::RequestPasskey { device_path, .. }
            | Self::RequestConfirmation { device_path, .. }
            | Self::DisplayPinCode { device_path, .. }
            | Self::DisplayPasskey { device_path, .. } => device_path,
        }
    }

    /// Get the device name for this auth request.
    pub fn device_name(&self) -> &str {
        match self {
            Self::RequestPinCode { device_name, .. }
            | Self::RequestPasskey { device_name, .. }
            | Self::RequestConfirmation { device_name, .. }
            | Self::DisplayPinCode { device_name, .. }
            | Self::DisplayPasskey { device_name, .. } => device_name,
        }
    }

    /// Returns true for display-only auth requests (DisplayPinCode, DisplayPasskey).
    ///
    /// Display-only requests show a code for the user to enter on the remote device.
    /// The D-Bus invocation returns immediately, so cancellation requires calling
    /// CancelPairing() on the device rather than returning an error to BlueZ.
    pub fn is_display_only(&self) -> bool {
        matches!(
            self,
            Self::DisplayPinCode { .. } | Self::DisplayPasskey { .. }
        )
    }

    /// Returns the number of characters expected for this auth request's input.
    pub fn char_count(&self) -> usize {
        match self {
            Self::RequestPinCode { .. } => 4,
            Self::RequestPasskey { .. } => 6,
            Self::RequestConfirmation { .. } => 6,
            Self::DisplayPinCode { pincode, .. } => pincode.len().max(4),
            Self::DisplayPasskey { .. } => 6,
        }
    }
}

/// Response to an authentication request.
#[derive(Debug, Clone)]
pub enum BluetoothAuthResponse {
    /// PIN code entered by user.
    PinCode(String),
    /// Passkey entered by user.
    Passkey(u32),
    /// User confirmed the passkey.
    Confirmed,
}

/// Type of pending authentication method (determines expected response type).
#[derive(Debug, Clone, Copy)]
enum PendingAuthKind {
    /// Expects PinCode response, returns (s,) tuple.
    PinCode,
    /// Expects Passkey response, returns (u,) tuple.
    Passkey,
    /// Expects Confirmed response, returns empty tuple.
    Confirmation,
}

/// Pending authentication state - stores the D-Bus invocation and expected response type.
struct PendingAuth {
    /// The D-Bus method invocation to complete when user responds.
    invocation: gio::DBusMethodInvocation,
    /// The type of auth request (determines how to handle the response).
    kind: PendingAuthKind,
}

/// A single Bluetooth device exposed by BlueZ.
#[derive(Debug, Clone)]
pub struct BluetoothDevice {
    pub path: String,
    pub name: String,
    pub address: String,
    pub connected: bool,
    pub paired: bool,
    pub trusted: bool,
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
    /// Current authentication request from BlueZ Agent (if any).
    pub auth_request: Option<BluetoothAuthRequest>,
    /// Device path currently being paired (if any). Set when Pair() is called,
    /// cleared on success or failure.
    pub pairing_device_path: Option<String>,
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
            auth_request: None,
            pairing_device_path: None,
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
    /// Debounce source ID for batched state updates.
    debounce_id: RefCell<Option<glib::SourceId>>,
    /// D-Bus signal subscriptions (kept alive for the service lifetime).
    _signal_subscriptions: RefCell<Vec<gio::SignalSubscription>>,
    /// Agent registration ID for D-Bus object.
    agent_registration_id: RefCell<Option<gio::RegistrationId>>,
    /// Pending authentication state (invocation + response handler).
    pending_auth: RefCell<Option<PendingAuth>>,
    /// Timeout source ID for auth request expiry.
    auth_timeout_source: RefCell<Option<glib::SourceId>>,
}

impl BluetoothService {
    fn new() -> Rc<Self> {
        let service = Rc::new(Self {
            connection: RefCell::new(None),
            adapter: RefCell::new(None),
            object_manager: RefCell::new(None),
            snapshot: RefCell::new(BluetoothSnapshot::empty()),
            callbacks: Callbacks::new(),
            debounce_id: RefCell::new(None),
            _signal_subscriptions: RefCell::new(Vec::new()),
            agent_registration_id: RefCell::new(None),
            pending_auth: RefCell::new(None),
            auth_timeout_source: RefCell::new(None),
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

    /// Mutate the snapshot and notify callbacks.
    fn update_snapshot(&self, f: impl FnOnce(&mut BluetoothSnapshot)) {
        let mut snapshot = self.snapshot.borrow_mut();
        f(&mut snapshot);
        let snapshot_clone = snapshot.clone();
        drop(snapshot);
        self.callbacks.notify(&snapshot_clone);
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

                            // Register the BlueZ agent for pairing authentication
                            Self::register_agent(&this);
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

    /// Register the BlueZ Agent for handling pairing authentication.
    fn register_agent(this: &Rc<Self>) {
        // Guard against duplicate registration (e.g., if adapter disappears/reappears)
        if this.agent_registration_id.borrow().is_some() {
            debug!("BluetoothService: agent already registered, skipping");
            return;
        }

        let Some(connection) = this.connection.borrow().clone() else {
            return;
        };

        let node_info = match gio::DBusNodeInfo::for_xml(AGENT_INTROSPECTION) {
            Ok(info) => info,
            Err(e) => {
                error!(
                    "BluetoothService: failed to parse agent introspection: {}",
                    e
                );
                return;
            }
        };

        let interface_info = match node_info.lookup_interface(AGENT_IFACE) {
            Some(info) => info,
            None => {
                error!("BluetoothService: Agent1 interface not found in introspection");
                return;
            }
        };

        // Register the agent object on the bus using builder pattern
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
                debug!("BluetoothService: registered agent at {}", AGENT_PATH);
                *this.agent_registration_id.borrow_mut() = Some(id);

                // Now register with AgentManager
                Self::register_with_agent_manager(this, &connection);
            }
            Err(e) => {
                error!("BluetoothService: failed to register agent object: {}", e);
            }
        }
    }

    /// Register our agent with BlueZ's AgentManager.
    fn register_with_agent_manager(this: &Rc<Self>, connection: &gio::DBusConnection) {
        let this_weak = Rc::downgrade(this);

        DBusProxy::new(
            connection,
            DBusProxyFlags::NONE,
            None,
            Some(BLUEZ_SERVICE),
            "/org/bluez",
            AGENT_MANAGER_IFACE,
            None::<&gio::Cancellable>,
            move |res| {
                if this_weak.upgrade().is_none() {
                    return;
                }

                let proxy = match res {
                    Ok(p) => p,
                    Err(e) => {
                        error!(
                            "BluetoothService: failed to create AgentManager proxy: {}",
                            e
                        );
                        return;
                    }
                };

                // RegisterAgent(object agent, string capability)
                // Capability "KeyboardDisplay" means we can display and input
                let agent_path = glib::variant::ObjectPath::try_from(AGENT_PATH).unwrap();
                let args = Variant::tuple_from_iter([
                    agent_path.to_variant(),
                    "KeyboardDisplay".to_variant(),
                ]);

                proxy.call(
                    "RegisterAgent",
                    Some(&args),
                    DBusCallFlags::NONE,
                    5000,
                    None::<&gio::Cancellable>,
                    move |res| {
                        if let Err(e) = res {
                            // AlreadyExists is fine (agent already registered)
                            let is_already_exists = gio::DBusError::remote_error(&e)
                                .map(|e| e == "org.bluez.Error.AlreadyExists")
                                .unwrap_or(false);
                            if !is_already_exists {
                                error!("BluetoothService: RegisterAgent failed: {}", e);
                                return;
                            }
                        }
                        debug!("BluetoothService: agent registered with AgentManager");
                        // Note: We intentionally do NOT call RequestDefaultAgent here.
                        // That would make us the system-wide default agent, stealing
                        // pairing prompts from other Bluetooth managers. Our agent will
                        // still be used for pairings we initiate via Pair().
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
        debug!("BluetoothService: agent method '{}' called", method);

        match method {
            "Release" => {
                debug!("BluetoothService: agent released");
                invocation.return_value(None);
            }
            "Cancel" => {
                debug!("BluetoothService: pairing cancelled by BlueZ");
                // Complete any pending auth invocation with Canceled error
                if let Some(source_id) = this.auth_timeout_source.borrow_mut().take() {
                    source_id.remove();
                }
                if let Some(pending) = this.pending_auth.borrow_mut().take() {
                    pending
                        .invocation
                        .return_dbus_error("org.bluez.Error.Canceled", "Canceled");
                }
                // Clear auth request and notify UI
                this.clear_auth_state();
                invocation.return_value(None);
            }
            "RequestPinCode" => {
                Self::handle_request_pin_code(this, params, invocation);
            }
            "RequestPasskey" => {
                Self::handle_request_passkey(this, params, invocation);
            }
            "RequestConfirmation" => {
                Self::handle_request_confirmation(this, params, invocation);
            }
            "DisplayPinCode" => {
                Self::handle_display_pin_code(this, params, invocation);
            }
            "DisplayPasskey" => {
                Self::handle_display_passkey(this, params, invocation);
            }
            "AuthorizeService" => {
                // Called when a paired device wants to use a service profile (e.g., audio, HID).
                // Since we're not the default agent (didn't call RequestDefaultAgent), we only
                // receive this for pairings we initiated. Auto-accept as pairing established trust.
                // If we were the default agent, we'd want to verify the device is in our paired list.
                invocation.return_value(None);
            }
            _ => {
                error!("BluetoothService: unknown agent method: {}", method);
                invocation.return_error(
                    gio::IOErrorEnum::NotSupported,
                    &format!("Unknown method: {}", method),
                );
            }
        }
    }

    /// Get device name from path by looking up in current snapshot.
    fn get_device_name_for_path(&self, device_path: &str) -> String {
        let snapshot = self.snapshot.borrow();
        snapshot
            .devices
            .iter()
            .find(|d| d.path == device_path)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "Unknown device".to_string())
    }

    /// Setup auth response handling for a D-Bus agent method.
    ///
    /// Stores the invocation and expected response kind, sets up a timeout, and notifies UI.
    /// When the user responds (via submit_pin, etc.), finish_auth handles the response based on kind.
    fn setup_auth_handler(
        this: &Rc<Self>,
        invocation: gio::DBusMethodInvocation,
        auth_request: BluetoothAuthRequest,
        kind: PendingAuthKind,
    ) {
        // Cancel any existing auth timeout
        if let Some(source_id) = this.auth_timeout_source.borrow_mut().take() {
            source_id.remove();
        }

        // If a previous auth request is still pending, cancel it to avoid
        // orphaning the D-Bus invocation.
        if let Some(pending) = this.pending_auth.borrow_mut().take() {
            pending
                .invocation
                .return_dbus_error("org.bluez.Error.Canceled", "Superseded by new auth request");
        }

        // Store the pending auth state
        *this.pending_auth.borrow_mut() = Some(PendingAuth { invocation, kind });

        // Update snapshot with auth request and notify UI
        this.update_snapshot(|s| s.auth_request = Some(auth_request));

        // Set up a single timeout for auth expiry
        let this_weak = Rc::downgrade(this);
        let timeout_source = glib::timeout_add_local_once(
            std::time::Duration::from_secs(AUTH_TIMEOUT_SECS),
            move || {
                let Some(this) = this_weak.upgrade() else {
                    return;
                };
                if let Some(pending) = this.pending_auth.borrow_mut().take() {
                    pending
                        .invocation
                        .return_dbus_error("org.bluez.Error.Rejected", "Pairing timed out");
                }
                *this.auth_timeout_source.borrow_mut() = None;
                this.clear_auth_state();
            },
        );
        *this.auth_timeout_source.borrow_mut() = Some(timeout_source);
    }

    /// Finish a pending auth request with a response.
    fn finish_auth(&self, response: Option<BluetoothAuthResponse>) {
        if let Some(source_id) = self.auth_timeout_source.borrow_mut().take() {
            source_id.remove();
        }

        if let Some(pending) = self.pending_auth.borrow_mut().take() {
            match response {
                Some(resp) => {
                    // Match response type to pending auth kind and complete the D-Bus invocation
                    match (pending.kind, resp) {
                        (PendingAuthKind::PinCode, BluetoothAuthResponse::PinCode(pin)) => {
                            let variant = Variant::tuple_from_iter([pin.to_variant()]);
                            pending.invocation.return_value(Some(&variant));
                        }
                        (PendingAuthKind::Passkey, BluetoothAuthResponse::Passkey(key)) => {
                            let variant = Variant::tuple_from_iter([key.to_variant()]);
                            pending.invocation.return_value(Some(&variant));
                        }
                        (PendingAuthKind::Confirmation, BluetoothAuthResponse::Confirmed) => {
                            pending.invocation.return_value(None); // Empty tuple
                        }
                        _ => {
                            // Response type mismatch - shouldn't happen in practice
                            pending.invocation.return_dbus_error(
                                "org.bluez.Error.Rejected",
                                "Invalid response type",
                            );
                        }
                    }
                }
                None => {
                    // No response provided - return error to avoid hanging invocation
                    pending
                        .invocation
                        .return_dbus_error("org.bluez.Error.Canceled", "No response provided");
                }
            }
        }
        self.clear_auth_state();
    }

    /// Clear auth state and notify callbacks.
    fn clear_auth_state(&self) {
        self.update_snapshot(|s| s.auth_request = None);
    }

    /// Clear pairing state if it matches the given path, and notify callbacks.
    fn clear_pairing_if_match(&self, path: &str) {
        self.update_snapshot(|s| {
            if s.pairing_device_path.as_deref() == Some(path) {
                s.pairing_device_path = None;
            }
        });
    }

    /// Handle RequestPinCode agent method.
    fn handle_request_pin_code(
        this: &Rc<Self>,
        params: Variant,
        invocation: gio::DBusMethodInvocation,
    ) {
        let device_path: String = params.child_value(0).get().unwrap_or_default();
        let device_name = this.get_device_name_for_path(&device_path);

        debug!(
            "BluetoothService: RequestPinCode for {} ({})",
            device_name, device_path
        );

        let auth_request = BluetoothAuthRequest::RequestPinCode {
            device_path,
            device_name,
        };

        Self::setup_auth_handler(this, invocation, auth_request, PendingAuthKind::PinCode);
    }

    /// Handle RequestPasskey agent method.
    fn handle_request_passkey(
        this: &Rc<Self>,
        params: Variant,
        invocation: gio::DBusMethodInvocation,
    ) {
        let device_path: String = params.child_value(0).get().unwrap_or_default();
        let device_name = this.get_device_name_for_path(&device_path);

        debug!(
            "BluetoothService: RequestPasskey for {} ({})",
            device_name, device_path
        );

        let auth_request = BluetoothAuthRequest::RequestPasskey {
            device_path,
            device_name,
        };

        Self::setup_auth_handler(this, invocation, auth_request, PendingAuthKind::Passkey);
    }

    /// Handle RequestConfirmation agent method.
    fn handle_request_confirmation(
        this: &Rc<Self>,
        params: Variant,
        invocation: gio::DBusMethodInvocation,
    ) {
        let device_path: String = params.child_value(0).get().unwrap_or_default();
        let passkey: u32 = params.child_value(1).get().unwrap_or(0);
        let device_name = this.get_device_name_for_path(&device_path);

        debug!(
            "BluetoothService: RequestConfirmation for {} ({})",
            device_name, device_path
        );

        let auth_request = BluetoothAuthRequest::RequestConfirmation {
            device_path,
            device_name,
            passkey,
        };

        Self::setup_auth_handler(
            this,
            invocation,
            auth_request,
            PendingAuthKind::Confirmation,
        );
    }

    /// Handle DisplayPinCode agent method.
    fn handle_display_pin_code(
        this: &Rc<Self>,
        params: Variant,
        invocation: gio::DBusMethodInvocation,
    ) {
        let device_path: String = params.child_value(0).get().unwrap_or_default();
        let pincode: String = params.child_value(1).get().unwrap_or_default();
        let device_name = this.get_device_name_for_path(&device_path);

        debug!(
            "BluetoothService: DisplayPinCode for {} ({})",
            device_name, device_path
        );

        // Update snapshot with auth request (display only)
        this.update_snapshot(|s| {
            s.auth_request = Some(BluetoothAuthRequest::DisplayPinCode {
                device_path,
                device_name,
                pincode,
            });
        });

        // DisplayPinCode returns immediately - pairing continues
        invocation.return_value(None);
    }

    /// Handle DisplayPasskey agent method.
    fn handle_display_passkey(
        this: &Rc<Self>,
        params: Variant,
        invocation: gio::DBusMethodInvocation,
    ) {
        let device_path: String = params.child_value(0).get().unwrap_or_default();
        let passkey: u32 = params.child_value(1).get().unwrap_or(0);
        // entered parameter is the number of digits entered on remote device (we ignore it)
        let device_name = this.get_device_name_for_path(&device_path);

        debug!(
            "BluetoothService: DisplayPasskey for {} ({})",
            device_name, device_path
        );

        // Update snapshot with auth request (display only)
        this.update_snapshot(|s| {
            s.auth_request = Some(BluetoothAuthRequest::DisplayPasskey {
                device_path,
                device_name,
                passkey,
            });
        });

        // DisplayPasskey returns immediately - pairing continues
        invocation.return_value(None);
    }

    /// Unregister the agent from D-Bus and AgentManager.
    fn unregister_agent(&self) {
        // Unregister from AgentManager and D-Bus object (only if we registered)
        let Some(reg_id) = self.agent_registration_id.borrow_mut().take() else {
            return;
        };

        // First unregister from AgentManager (best effort - BlueZ may already be gone)
        if let Some(connection) = self.connection.borrow().clone() {
            let agent_path = glib::variant::ObjectPath::try_from(AGENT_PATH).unwrap();
            let args = Variant::tuple_from_iter([agent_path.to_variant()]);

            // Fire-and-forget: we don't wait for the result since BlueZ may be unavailable
            connection.call(
                Some(BLUEZ_SERVICE),
                "/org/bluez",
                AGENT_MANAGER_IFACE,
                "UnregisterAgent",
                Some(&args),
                None,
                DBusCallFlags::NONE,
                1000, // Short timeout - don't block if BlueZ is gone
                None::<&gio::Cancellable>,
                |_| {}, // Ignore result
            );
        }

        // Then unregister the D-Bus object
        if let Some(connection) = self.connection.borrow().as_ref() {
            let _ = connection.unregister_object(reg_id);
            debug!("BluetoothService: unregistered agent");
        }
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

        // Unregister agent from D-Bus so it can re-register when BlueZ returns.
        self.unregister_agent();

        // Clear any pending auth to avoid leaving D-Bus invocations hanging.
        if let Some(source_id) = self.auth_timeout_source.borrow_mut().take() {
            source_id.remove();
        }
        self.pending_auth.borrow_mut().take();
    }

    fn update_state_debounced(self: &Rc<Self>) {
        if self.debounce_id.borrow().is_some() {
            return;
        }

        let this_weak = Rc::downgrade(self);
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

    fn update_state(self: &Rc<Self>) {
        let adapter = self.adapter.borrow().clone();
        let object_manager = self.object_manager.borrow().clone();
        let connection = self.connection.borrow().clone();

        let Some(_connection) = connection else {
            self.update_snapshot(|s| *s = BluetoothSnapshot::empty());
            return;
        };

        let has_adapter = adapter.is_some();
        let powered = adapter
            .as_ref()
            .and_then(|p| p.cached_property("Powered"))
            .and_then(|v| v.get::<bool>())
            .unwrap_or(false);
        let discovering = adapter
            .as_ref()
            .and_then(|p| p.cached_property("Discovering"))
            .and_then(|v| v.get::<bool>())
            .unwrap_or(false);

        // Get managed objects to enumerate devices
        if let Some(om) = object_manager {
            let this_weak = Rc::downgrade(self);
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
                    let discovering = adapter
                        .as_ref()
                        .and_then(|p| p.cached_property("Discovering"))
                        .and_then(|v| v.get::<bool>())
                        .unwrap_or(false);

                    let mut snapshot = this.snapshot.borrow_mut();
                    snapshot.has_adapter = has_adapter;
                    snapshot.powered = powered;
                    snapshot.connected_devices = connected_count;
                    snapshot.devices = devices; // Move, not clone
                    snapshot.scanning = discovering;
                    snapshot.is_ready = true;

                    // Clear display-only auth requests when:
                    // - Device becomes paired (success)
                    // - Device disappeared from the list
                    // - Pairing is no longer in progress for this device (failure/cancel)
                    // (DisplayPinCode/DisplayPasskey have no pending invocation to complete)
                    if let Some(ref auth_req) = snapshot.auth_request
                        && auth_req.is_display_only()
                    {
                        let device_path = auth_req.device_path();
                        let device = snapshot.devices.iter().find(|d| d.path == device_path);
                        let pairing_cleared = snapshot
                            .pairing_device_path
                            .as_ref()
                            .map(|p| p != device_path)
                            .unwrap_or(true);
                        let should_clear = match device {
                            Some(d) => d.paired || pairing_cleared, // Clear when paired or pairing ended
                            None => true, // Clear if device disappeared
                        };
                        if should_clear {
                            debug!(
                                "BluetoothService: clearing display auth request - device paired or pairing ended"
                            );
                            snapshot.auth_request = None;
                        }
                    }

                    let snapshot_clone = snapshot.clone();
                    drop(snapshot);
                    this.callbacks.notify(&snapshot_clone);
                },
            );
        } else {
            // No object manager yet, just update what we know
            self.update_snapshot(|s| {
                s.has_adapter = has_adapter;
                s.powered = powered;
                s.scanning = discovering;
                s.is_ready = true;
            });
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
                let key_a = (
                    !a.connected,
                    !a.paired,
                    !a.trusted,
                    is_mac_like_name(&a.name),
                    a.name.to_lowercase(),
                );
                let key_b = (
                    !b.connected,
                    !b.paired,
                    !b.trusted,
                    is_mac_like_name(&b.name),
                    b.name.to_lowercase(),
                );
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

        // Sort: connected first, then paired, then trusted, then readable names before MAC-like, then by name
        devices.sort_by(|a, b| {
            let key_a = (
                !a.connected,
                !a.paired,
                !a.trusted,
                is_mac_like_name(&a.name),
                a.name.to_lowercase(),
            );
            let key_b = (
                !b.connected,
                !b.paired,
                !b.trusted,
                is_mac_like_name(&b.name),
                b.name.to_lowercase(),
            );
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
        let mut trusted = false;
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
                "Trusted" => trusted = inner.get::<bool>().unwrap_or(false),
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
            trusted,
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

    pub fn scan_for_devices(self: &Rc<Self>) {
        let Some(adapter) = self.adapter.borrow().clone() else {
            return;
        };

        // Check if already discovering (BlueZ tracks this via Discovering property)
        let already_scanning = self.snapshot.borrow().scanning;
        if already_scanning {
            return;
        }

        // Start discovery - BlueZ will emit PropertiesChanged when Discovering changes
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

        // Schedule StopDiscovery after timeout.
        // BlueZ uses reference counting - we must stop what we started.
        // The actual UI state comes from the Discovering property, not this timeout.
        let this_weak = Rc::downgrade(self);
        glib::timeout_add_seconds_local(SCAN_DURATION_SECS, move || {
            if let Some(this) = this_weak.upgrade()
                && let Some(adapter) = this.adapter.borrow().clone()
            {
                adapter.call(
                    "StopDiscovery",
                    None,
                    DBusCallFlags::NONE,
                    5000,
                    None::<&gio::Cancellable>,
                    |res| {
                        if let Err(e) = res {
                            // This can fail if discovery was already stopped - that's fine
                            tracing::debug!("BluetoothService: StopDiscovery: {}", e);
                        }
                    },
                );
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

    pub fn pair_device(self: &Rc<Self>, path_or_address: &str) {
        let Some((path, connection)) = self.get_device_proxy(path_or_address) else {
            return;
        };

        // Set pairing state immediately and notify UI
        self.update_snapshot(|s| s.pairing_device_path = Some(path.clone()));

        let pairing_path = path.clone();
        let this_weak = Rc::downgrade(self);
        DBusProxy::new(
            &connection,
            DBusProxyFlags::NONE,
            None,
            Some(BLUEZ_SERVICE),
            &path,
            DEVICE_IFACE,
            None::<&gio::Cancellable>,
            move |res| {
                let Some(this) = this_weak.upgrade() else {
                    return;
                };
                match res {
                    Ok(proxy) => {
                        let proxy_for_trust = proxy.clone();
                        let proxy_for_connect = proxy.clone();
                        let pairing_path_for_callback = pairing_path.clone();
                        let this_weak_inner = Rc::downgrade(&this);

                        proxy.call(
                            "Pair",
                            None,
                            DBusCallFlags::NONE,
                            30000, // Pairing can take time
                            None::<&gio::Cancellable>,
                            move |res| {
                                let Some(this) = this_weak_inner.upgrade() else {
                                    return;
                                };
                                let pair_err = res.err();

                                // Clear pairing state, and on failure also clear display-only auth
                                // (success case is handled by update_state when device becomes paired)
                                this.update_snapshot(|s| {
                                    if s.pairing_device_path.as_deref()
                                        == Some(pairing_path_for_callback.as_str())
                                    {
                                        s.pairing_device_path = None;
                                    }
                                    if pair_err.is_some()
                                        && let Some(ref auth_req) = s.auth_request
                                        && auth_req.is_display_only()
                                        && auth_req.device_path() == pairing_path_for_callback
                                    {
                                        s.auth_request = None;
                                    }
                                });

                                let (allow_connect, is_auth_error) = match pair_err.as_ref() {
                                    None => (true, false),
                                    Some(err) => {
                                        let remote_err = gio::DBusError::remote_error(err);
                                        let is_already = remote_err
                                            .as_ref()
                                            .map(|e| e == "org.bluez.Error.AlreadyExists")
                                            .unwrap_or(false);
                                        let is_auth_error = remote_err
                                            .as_ref()
                                            .map(|e| {
                                                e == "org.bluez.Error.AuthenticationCanceled"
                                                    || e == "org.bluez.Error.AuthenticationFailed"
                                                    || e == "org.bluez.Error.AuthenticationRejected"
                                            })
                                            .unwrap_or(false);
                                        (is_already, is_auth_error)
                                    }
                                };

                                if let Some(err) = pair_err.as_ref() {
                                    // AlreadyExists means we can proceed to connect.
                                    // Auth errors (cancelled/failed/rejected) are logged at debug
                                    // level since they're often user-initiated or expected.
                                    if !allow_connect {
                                        if is_auth_error {
                                            debug!("BluetoothService: pairing auth error: {}", err);
                                        } else {
                                            error!("BluetoothService: Pair failed: {}", err);
                                        }
                                    }
                                }

                                if !allow_connect {
                                    return;
                                }

                                // Trust the device so future reconnects are seamless.
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
                            },
                        );
                    }
                    Err(e) => {
                        // Clear pairing state on proxy creation failure
                        this.clear_pairing_if_match(&pairing_path);
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

    // Authentication response API

    /// Submit a PIN code in response to a RequestPinCode auth request.
    pub fn submit_pin(&self, pin: &str) {
        self.finish_auth(Some(BluetoothAuthResponse::PinCode(pin.to_string())));
    }

    /// Submit a passkey in response to a RequestPasskey auth request.
    pub fn submit_passkey(&self, passkey: u32) {
        self.finish_auth(Some(BluetoothAuthResponse::Passkey(passkey)));
    }

    /// Confirm a passkey confirmation request.
    pub fn confirm_passkey(&self) {
        self.finish_auth(Some(BluetoothAuthResponse::Confirmed));
    }

    /// Cancel any pending auth request and abort pairing if in progress.
    pub fn cancel_auth(&self) {
        if let Some(source_id) = self.auth_timeout_source.borrow_mut().take() {
            source_id.remove();
        }
        if let Some(pending) = self.pending_auth.borrow_mut().take() {
            pending
                .invocation
                .return_dbus_error("org.bluez.Error.Canceled", "Canceled by user");
        }

        // Call CancelPairing() on the device to abort pairing. This is especially
        // important for display-only auth where the D-Bus invocation already returned.
        if let Some(connection) = self.connection.borrow().clone()
            && let Some(path) = self.snapshot.borrow().pairing_device_path.clone()
        {
            DBusProxy::new(
                &connection,
                DBusProxyFlags::NONE,
                None,
                Some(BLUEZ_SERVICE),
                &path,
                DEVICE_IFACE,
                None::<&gio::Cancellable>,
                |res| {
                    if let Ok(proxy) = res {
                        proxy.call(
                            "CancelPairing",
                            None,
                            DBusCallFlags::NONE,
                            5000,
                            None::<&gio::Cancellable>,
                            |_| {},
                        );
                    }
                },
            );
        }

        // Clear pairing state
        self.update_snapshot(|s| {
            s.pairing_device_path = None;
            s.auth_request = None;
        });
    }
}
