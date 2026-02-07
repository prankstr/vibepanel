use crate::services::callbacks::CallbackId;
use gtk4::gio::{self, prelude::*};
use std::rc::Rc;
use tracing::{debug, warn};

pub mod iwd;
pub mod network_manager;
pub use iwd::{IwdService, IwdSnapshot};
use network_manager::{NM_IFACE, NM_PATH, NM_SERVICE};
pub use network_manager::{NetworkService, NetworkSnapshot};

/// Whether a Wi-Fi network requires authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecurityType {
    Open,
    Secured,
}

impl SecurityType {
    pub fn is_secured(self) -> bool {
        self == Self::Secured
    }
}

/// A Wi-Fi network visible in the scan results.
///
/// Used by both NetworkManager and IWD backends. Some fields
/// are backend-specific and will be `None` when using the other backend.
#[derive(Debug, Clone)]
pub struct WifiNetwork {
    pub ssid: String,
    /// Signal strength percentage (0-100).
    pub strength: i32,
    pub security: SecurityType,
    /// Whether this is the currently connected network.
    pub active: bool,
    /// Whether there is a saved connection profile for this SSID.
    pub known: bool,
    /// IWD-only: D-Bus path to the KnownNetwork object (for `forget_network()`).
    pub known_network_path: Option<String>,
    /// IWD-only: D-Bus path to the Network object (for `connect_to_network()`).
    pub path: Option<String>,
}

enum WifiBackend {
    NetworkManager(Rc<NetworkService>),
    Iwd(Rc<IwdService>),
}

/// Unified snapshot of Wi-Fi state from either backend.
///
/// Use the accessor methods (e.g., `connected()`, `networks()`) to get
/// unified values that work with both backends.
pub enum WifiSnapshot {
    NetworkManager(NetworkSnapshot),
    Iwd(IwdSnapshot),
}

/// Connection state for Wi-Fi.
///
/// This enum provides a unified view of the connection state across both
/// NetworkManager and IWD backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WifiConnectionState {
    /// Not connected to any network.
    Disconnected,
    /// Currently connecting to a network.
    Connecting,
    /// Connected to a network.
    Connected,
}

impl WifiSnapshot {
    /// Get the SSID of the active or connecting network.
    ///
    /// For NetworkManager: returns `connecting_ssid` if connecting, else `ssid`.
    /// For IWD: returns `ssid` (which is set during both connecting and connected states).
    pub fn active_ssid(&self) -> Option<&str> {
        match self {
            Self::NetworkManager(inner) => {
                inner.connecting_ssid.as_deref().or(inner.ssid.as_deref())
            }
            Self::Iwd(inner) => inner.ssid.as_deref(),
        }
    }

    /// Whether the Wi-Fi backend service is available.
    ///
    /// Returns `false` if the service (NetworkManager or IWD) is not running
    /// or no Wi-Fi adapter was found.
    pub fn available(&self) -> bool {
        match self {
            Self::NetworkManager(inner) => inner.available,
            Self::Iwd(inner) => inner.available,
        }
    }

    /// Whether currently connected to a Wi-Fi network.
    ///
    /// For NetworkManager: checks `connected` field.
    /// For IWD: uses `connected()` method (state is "connected" or "roaming").
    pub fn connected(&self) -> bool {
        match self {
            Self::NetworkManager(inner) => inner.connected,
            Self::Iwd(inner) => inner.connected(),
        }
    }

    pub fn connecting_ssid(&self) -> Option<&str> {
        match self {
            Self::NetworkManager(inner) => inner.connecting_ssid.as_deref(),
            Self::Iwd(inner) => {
                if inner.connecting() {
                    inner.ssid.as_deref()
                } else {
                    None
                }
            }
        }
    }

    pub fn connection_state(&self) -> WifiConnectionState {
        match self {
            Self::NetworkManager(inner) => {
                if inner.connecting_ssid.is_some() {
                    WifiConnectionState::Connecting
                } else if inner.connected {
                    WifiConnectionState::Connected
                } else {
                    WifiConnectionState::Disconnected
                }
            }
            Self::Iwd(inner) => {
                if inner.connecting() {
                    WifiConnectionState::Connecting
                } else if inner.connected() {
                    WifiConnectionState::Connected
                } else {
                    WifiConnectionState::Disconnected
                }
            }
        }
    }

    pub fn has_ethernet_device(&self) -> bool {
        match self {
            Self::NetworkManager(inner) => inner.has_ethernet_device,
            Self::Iwd(_) => false,
        }
    }

    /// Whether the system has wifi hardware.
    /// For iwd: implied by service availability (iwd requires wifi hardware).
    pub fn has_wifi_device(&self) -> bool {
        match self {
            Self::NetworkManager(inner) => inner.has_wifi_device,
            Self::Iwd(inner) => inner.available, // adapter found = wifi device exists
        }
    }

    pub fn is_ready(&self) -> bool {
        match self {
            Self::NetworkManager(inner) => inner.is_ready,
            // IWD is ready once the initial network refresh has completed.
            // This mirrors NetworkManager's is_ready field for consistent UI behavior.
            Self::Iwd(inner) => inner.initial_scan_complete,
        }
    }

    pub fn networks(&self) -> &[WifiNetwork] {
        match self {
            Self::NetworkManager(inner) => &inner.networks,
            Self::Iwd(inner) => &inner.networks,
        }
    }

    pub fn scanning(&self) -> bool {
        match self {
            Self::NetworkManager(inner) => inner.scanning,
            Self::Iwd(inner) => inner.scanning,
        }
    }

    pub fn wifi_enabled(&self) -> Option<bool> {
        match self {
            Self::NetworkManager(inner) => inner.wifi_enabled,
            Self::Iwd(inner) => inner.wifi_enabled,
        }
    }

    pub fn wired_connected(&self) -> bool {
        match self {
            Self::NetworkManager(inner) => inner.wired_connected,
            Self::Iwd(_) => false,
        }
    }

    pub fn wired_iface(&self) -> Option<&str> {
        match self {
            Self::NetworkManager(inner) => inner.wired_iface.as_deref(),
            Self::Iwd(_) => None,
        }
    }

    pub fn wired_name(&self) -> Option<&str> {
        match self {
            Self::NetworkManager(inner) => inner.wired_name.as_deref(),
            Self::Iwd(_) => None,
        }
    }

    pub fn wired_speed(&self) -> Option<u32> {
        match self {
            Self::NetworkManager(inner) => inner.wired_speed,
            Self::Iwd(_) => None,
        }
    }

    /// Check if there's a pending auth request (IWD only).
    /// Returns the SSID of the network requesting authentication.
    pub fn auth_request_ssid(&self) -> Option<&str> {
        match self {
            Self::NetworkManager(_) => None,
            Self::Iwd(inner) => inner.auth_request.as_ref().map(|r| r.ssid.as_str()),
        }
    }

    /// Get the SSID of the network that failed to connect, if any.
    pub fn failed_ssid(&self) -> Option<&str> {
        match self {
            Self::NetworkManager(inner) => inner.failed_ssid.as_deref(),
            Self::Iwd(inner) => inner.failed_ssid.as_deref(),
        }
    }

    /// Get the human-readable reason for the last connection failure.
    ///
    /// - IWD: specific reasons like "Wrong password", "Connection failed", etc.
    /// - NetworkManager: always `None` (NM doesn't provide granular failure reasons).
    pub fn failed_reason(&self) -> Option<&str> {
        match self {
            Self::NetworkManager(_) => None,
            Self::Iwd(inner) => inner.failed_reason.as_deref(),
        }
    }
}

/// Unified Wi-Fi service that abstracts over NetworkManager and IWD backends.
///
/// This service automatically detects which backend is available at startup
/// (preferring NetworkManager) and provides a unified API for Wi-Fi operations.
///
/// # Backend differences
///
/// While most operations are equivalent, there are key differences in authentication:
///
/// - **NetworkManager**: Password is provided upfront via `connect_to_network(ssid, password, _)`.
///   The connection attempt happens synchronously with the password.
///
/// - **IWD**: Connection is initiated via `connect_to_network(_, _, path)`, and if the network
///   requires authentication, IWD calls back via the agent pattern. The UI then shows a password
///   dialog and calls `submit_password()` to complete the authentication.
///
/// # Usage
///
/// ```ignore
/// let service = WifiService::global();
///
/// // Subscribe to state changes
/// service.connect(|snapshot| {
///     println!("Connected: {}", snapshot.connected());
/// });
///
/// // Scan for networks
/// service.scan();
///
/// // Connect to a network
/// service.connect_to_network("MyNetwork", Some("password123"), network.path.as_deref());
/// ```
pub struct WifiService {
    backend: WifiBackend,
}

impl WifiService {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            backend: detect_backend(),
        })
    }

    /// Get the global wifi service singleton.
    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<WifiService> = WifiService::new();
        }

        INSTANCE.with(|s| s.clone())
    }

    pub fn connect<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&WifiSnapshot) + 'static,
    {
        match &self.backend {
            WifiBackend::NetworkManager(inner) => inner.connect(move |snap| {
                let wrapped = WifiSnapshot::NetworkManager(snap.clone());
                callback(&wrapped);
            }),
            WifiBackend::Iwd(inner) => inner.connect(move |snap| {
                let wrapped = WifiSnapshot::Iwd(snap.clone());
                callback(&wrapped);
            }),
        }
    }

    /// Unregister a previously registered callback.
    pub fn unsubscribe(&self, id: CallbackId) {
        match &self.backend {
            WifiBackend::NetworkManager(inner) => inner.unsubscribe(id),
            WifiBackend::Iwd(inner) => inner.unsubscribe(id),
        }
    }

    /// Connect to a Wi-Fi network.
    ///
    /// # Parameters
    /// - `ssid`: Network name. Used by NetworkManager to identify the network.
    /// - `password`: Optional password. Used by NetworkManager for secured networks.
    ///   IWD uses the agent pattern instead (password requested via callback).
    /// - `path`: D-Bus object path. Required by IWD to identify the network.
    ///   Ignored by NetworkManager.
    ///
    /// # Backend behavior
    /// - **NetworkManager**: Calls `nmcli device wifi connect <ssid> [password <pw>]`
    /// - **IWD**: Calls `Network.Connect()` on the D-Bus path. If authentication is
    ///   needed, IWD will invoke the agent's `RequestPassphrase` method.
    pub fn connect_to_network(&self, ssid: &str, password: Option<&str>, path: Option<&str>) {
        match &self.backend {
            WifiBackend::NetworkManager(inner) => inner.connect_to_network(ssid, password),
            WifiBackend::Iwd(inner) => {
                if let Some(p) = path {
                    inner.connect_to_network(p);
                } else {
                    warn!(
                        "IWD connect_to_network called without path for SSID '{}' - ignoring",
                        ssid
                    );
                    // Report failure so the UI can exit "Connecting..." state.
                    inner.set_failed_ssid(ssid, "Network not found");
                }
            }
        }
    }

    pub fn disconnect(&self) {
        match &self.backend {
            WifiBackend::NetworkManager(inner) => inner.disconnect(),
            WifiBackend::Iwd(inner) => inner.disconnect(),
        }
    }

    /// Forget a saved Wi-Fi network.
    ///
    /// # Parameters
    /// - `ssid`: Network name. Used by NetworkManager to identify the saved connection.
    /// - `path`: D-Bus path to the KnownNetwork object. Required by IWD.
    ///   Ignored by NetworkManager.
    ///
    /// # Backend behavior
    /// - **NetworkManager**: Calls `nmcli connection delete id <ssid>`
    /// - **IWD**: Calls `KnownNetwork.Forget()` on the D-Bus path.
    pub fn forget(&self, ssid: &str, path: Option<&str>) {
        match &self.backend {
            WifiBackend::NetworkManager(inner) => inner.forget_network(ssid),
            WifiBackend::Iwd(inner) => {
                if let Some(p) = path {
                    inner.forget_network(p);
                } else {
                    warn!(
                        "IWD forget called without path for SSID '{}' - ignoring",
                        ssid
                    );
                }
            }
        }
    }

    pub fn scan(&self) {
        match &self.backend {
            WifiBackend::NetworkManager(inner) => inner.scan_networks(),
            WifiBackend::Iwd(inner) => inner.scan_networks(),
        }
    }

    pub fn set_wifi_enabled(&self, enabled: bool) {
        match &self.backend {
            WifiBackend::NetworkManager(inner) => inner.set_wifi_enabled(enabled),
            WifiBackend::Iwd(inner) => inner.set_wifi_enabled(enabled),
        }
    }

    pub fn snapshot(&self) -> WifiSnapshot {
        match &self.backend {
            WifiBackend::NetworkManager(inner) => WifiSnapshot::NetworkManager(inner.snapshot()),
            WifiBackend::Iwd(inner) => WifiSnapshot::Iwd(inner.snapshot()),
        }
    }

    /// Submit a password for a pending IWD auth request.
    /// For NetworkManager, this is a no-op (NM uses connect_to_network with password).
    pub fn submit_password(&self, password: &str) {
        match &self.backend {
            WifiBackend::NetworkManager(_) => {
                // NM doesn't use agent pattern - password is passed directly to connect_to_network
            }
            WifiBackend::Iwd(inner) => inner.submit_passphrase(password),
        }
    }

    /// Cancel a pending auth request.
    pub fn cancel_auth(&self) {
        match &self.backend {
            WifiBackend::NetworkManager(_) => {
                // NM doesn't have pending auth state in the same way
            }
            WifiBackend::Iwd(inner) => inner.cancel_auth(),
        }
    }

    /// Clear the failed state (called when user cancels password dialog).
    pub fn clear_failed_state(&self) {
        match &self.backend {
            WifiBackend::NetworkManager(inner) => inner.clear_failed_state(),
            WifiBackend::Iwd(inner) => inner.clear_failed_state(),
        }
    }
}

/// Detect which Wi-Fi backend is available.
///
/// Checks for NetworkManager first (the most common Linux network manager).
/// If NetworkManager is not available, falls back to IWD.
///
/// Called once at startup when the [`WifiService`] singleton is created;
/// the chosen backend is fixed for the lifetime of the process.
///
/// Note: If neither backend is available, IWD is still returned but will
/// mark itself as unavailable after D-Bus initialization fails.
fn detect_backend() -> WifiBackend {
    let result = gio::DBusProxy::for_bus_sync(
        gio::BusType::System,
        gio::DBusProxyFlags::DO_NOT_AUTO_START | gio::DBusProxyFlags::DO_NOT_LOAD_PROPERTIES,
        None::<&gio::DBusInterfaceInfo>,
        NM_SERVICE,
        NM_PATH,
        NM_IFACE,
        None::<&gio::Cancellable>,
    );

    if let Ok(proxy) = result
        && proxy.name_owner().is_some()
    {
        debug!("Wi-Fi backend: NetworkManager detected");
        return WifiBackend::NetworkManager(NetworkService::global());
    }

    debug!("Wi-Fi backend: NetworkManager not available, falling back to IWD");
    WifiBackend::Iwd(IwdService::global())
}
