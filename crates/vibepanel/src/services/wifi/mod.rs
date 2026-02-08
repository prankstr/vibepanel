use crate::services::callbacks::CallbackId;
use gtk4::gio::{self, prelude::*};
use gtk4::glib;
use std::rc::Rc;
use tracing::{debug, warn};

pub mod iwd;
pub mod network_manager;
use iwd::IWD_SERVICE;
pub use iwd::{IwdService, IwdSnapshot};
use network_manager::{NM_IFACE, NM_PATH, NM_SERVICE};
pub use network_manager::{NetworkService, NetworkSnapshot};

/// Failure reason for authentication errors (wrong password).
/// Used as a semantic tag between the IWD backend (producer) and the UI (consumer)
/// to distinguish auth failures from other connection errors.
pub const AUTH_FAILURE_REASON: &str = "Wrong password";

/// Generic failure reason for connection errors.
pub const CONNECTION_FAILURE_REASON: &str = "Connection failed";

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
    /// SSID of the active/connecting network. NM returns connecting_ssid if connecting;
    /// IWD returns ssid for both states.
    pub fn active_ssid(&self) -> Option<&str> {
        match self {
            Self::NetworkManager(inner) => {
                inner.connecting_ssid.as_deref().or(inner.ssid.as_deref())
            }
            Self::Iwd(inner) => {
                if inner.connected() || inner.connecting() {
                    inner.ssid.as_deref()
                } else {
                    None
                }
            }
        }
    }

    pub fn available(&self) -> bool {
        match self {
            Self::NetworkManager(inner) => inner.available,
            Self::Iwd(inner) => inner.available,
        }
    }

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

    /// Signal strength of the active (connected) network, or 0 if not connected.
    ///
    /// - NM: uses the top-level `strength` field on the snapshot.
    /// - IWD: looks up the active network in the scan list.
    pub fn active_strength(&self) -> i32 {
        match self {
            Self::NetworkManager(inner) => {
                if inner.connected {
                    inner.strength
                } else {
                    0
                }
            }
            Self::Iwd(inner) => {
                if inner.connected() {
                    inner
                        .networks
                        .iter()
                        .find(|n| n.active)
                        .map(|n| n.strength)
                        .unwrap_or(0)
                } else {
                    0
                }
            }
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
/// Automatically detects which backend is available at startup (preferring
/// NetworkManager) and provides a unified API for Wi-Fi operations.
///
/// # Backend differences
///
/// - **NetworkManager**: Password is provided upfront via `connect_to_network(ssid, password, _)`.
/// - **IWD**: Connection is initiated via `connect_to_network(_, _, path)`, and if the network
///   requires authentication, IWD calls back via the agent pattern. The UI then shows a password
///   dialog and calls `submit_password()` to complete the authentication.
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

    pub fn unsubscribe(&self, id: CallbackId) {
        match &self.backend {
            WifiBackend::NetworkManager(inner) => inner.unsubscribe(id),
            WifiBackend::Iwd(inner) => inner.unsubscribe(id),
        }
    }

    /// Connect to a Wi-Fi network.
    ///
    /// - `ssid`: Network name (used by NM).
    /// - `password`: Optional password (NM only; IWD uses agent callbacks).
    /// - `path`: D-Bus object path (IWD only; ignored by NM).
    pub fn connect_to_network(&self, ssid: &str, password: Option<&str>, path: Option<&str>) {
        match &self.backend {
            WifiBackend::NetworkManager(inner) => inner.connect_to_network(ssid, password),
            WifiBackend::Iwd(inner) => {
                if let Some(p) = path {
                    // Stash the password so handle_request_passphrase can
                    // auto-submit it (avoids double-prompt on retry).
                    inner.set_pending_password(password);
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
    /// - `ssid`: Network name (used by NM to delete the connection).
    /// - `path`: D-Bus KnownNetwork path (IWD only; ignored by NM).
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

/// Detect which Wi-Fi backend is available (NM preferred, then IWD).
///
/// Called once at startup; the backend is fixed for the process lifetime.
/// If neither service is running, defaults to NetworkManager — its `init_dbus()`
/// connects a `notify::g-name-owner` handler that fires the moment NM registers
/// on D-Bus, restoring the pre-IWD self-healing behavior.
fn detect_backend() -> WifiBackend {
    // Check for NetworkManager
    let nm_result = gio::DBusProxy::for_bus_sync(
        gio::BusType::System,
        gio::DBusProxyFlags::DO_NOT_AUTO_START | gio::DBusProxyFlags::DO_NOT_LOAD_PROPERTIES,
        None::<&gio::DBusInterfaceInfo>,
        NM_SERVICE,
        NM_PATH,
        NM_IFACE,
        None::<&gio::Cancellable>,
    );

    if let Ok(proxy) = nm_result
        && proxy.name_owner().is_some()
    {
        debug!("Wi-Fi backend: NetworkManager detected");
        return WifiBackend::NetworkManager(NetworkService::global());
    }

    // Check for IWD
    let iwd_result = gio::DBusProxy::for_bus_sync(
        gio::BusType::System,
        gio::DBusProxyFlags::DO_NOT_AUTO_START | gio::DBusProxyFlags::DO_NOT_LOAD_PROPERTIES,
        None::<&gio::DBusInterfaceInfo>,
        IWD_SERVICE,
        "/",
        "org.freedesktop.DBus.Peer",
        None::<&gio::Cancellable>,
    );

    if let Ok(proxy) = iwd_result
        && proxy.name_owner().is_some()
    {
        debug!("Wi-Fi backend: IWD detected");
        return WifiBackend::Iwd(IwdService::global());
    }

    // Neither detected — default to NetworkManager. Both backends monitor for
    // service appearance via notify::g-name-owner in their init_dbus(), but NM
    // is the overwhelmingly more common backend and NM startup races are the
    // realistic scenario (NM can be session-activated, whereas IWD is a system
    // service that starts early in boot).
    warn!(
        "Wi-Fi backend: neither NetworkManager nor IWD detected; \
         defaulting to NetworkManager (will activate when service appears)"
    );
    WifiBackend::NetworkManager(NetworkService::global())
}

/// Extract a D-Bus object path (`type o`) from a [`glib::Variant`] as a `String`.
pub(super) fn objpath_to_string(v: &glib::Variant) -> Option<String> {
    v.get::<glib::variant::ObjectPath>()
        .map(|p| p.as_str().to_string())
}
