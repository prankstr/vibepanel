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
pub use network_manager::{NmService, NmSnapshot};

/// Failure reason for authentication errors (wrong password).
/// Matched by the UI to distinguish auth failures from other connection errors.
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
/// Some fields are backend-specific and will be `None` for the other backend.
#[derive(Debug, Clone)]
pub struct WifiNetwork {
    pub ssid: String,
    /// Signal strength percentage (0-100).
    pub strength: i32,
    pub security: SecurityType,
    /// Whether this is the currently connected network.
    pub active: bool,
    /// Whether this SSID has a saved connection profile.
    pub known: bool,
    /// IWD-only: D-Bus path to the KnownNetwork object (for `forget_network()`).
    pub known_network_path: Option<String>,
    /// IWD-only: D-Bus path to the Network object (for `connect_to_network()`).
    pub path: Option<String>,
}

enum NetworkBackend {
    NetworkManager(Rc<NmService>),
    Iwd(Rc<IwdService>),
}

/// Unified snapshot of Wi-Fi state from either backend.
///
/// Use the accessor methods for unified values across backends.
pub enum NetworkSnapshot {
    NetworkManager(NmSnapshot),
    Iwd(IwdSnapshot),
}

/// Connection state for Wi-Fi (unified across NM and IWD).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkConnectionState {
    Disconnected,
    Connecting,
    Connected,
}

impl NetworkSnapshot {
    /// SSID of the active/connecting network.
    ///
    /// NM has separate `connecting_ssid`/`ssid` fields; IWD uses a single `ssid`
    /// guarded by connection state.
    pub fn active_ssid(&self) -> Option<&str> {
        match self {
            Self::NetworkManager(inner) => inner
                .wifi
                .connecting_ssid
                .as_deref()
                .or(inner.wifi.ssid.as_deref()),
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

    /// Whether Wi-Fi is connected (does not include wired or mobile).
    pub fn connected(&self) -> bool {
        match self {
            Self::NetworkManager(inner) => inner.wifi.connected,
            Self::Iwd(inner) => inner.connected(),
        }
    }

    pub fn connecting_ssid(&self) -> Option<&str> {
        match self {
            Self::NetworkManager(inner) => inner.wifi.connecting_ssid.as_deref(),
            Self::Iwd(inner) => {
                if inner.connecting() {
                    inner.ssid.as_deref()
                } else {
                    None
                }
            }
        }
    }

    /// Whether the NM WiFi device is in a connecting state (PREPARE through SECONDARIES).
    ///
    /// This fires earlier than `connecting_ssid()` because NM sets the Device
    /// `State` property before `ActiveAccessPoint`, giving us an early signal
    /// to show the spinner (matching nm-applet's behavior).
    pub fn wifi_device_connecting(&self) -> bool {
        match self {
            Self::NetworkManager(inner) => matches!(inner.wifi.device_state, Some(40..=90)),
            Self::Iwd(_) => false, // IWD has its own state machine
        }
    }

    /// Whether Wi-Fi is in any connecting/scanning state that warrants a spinner.
    ///
    /// Covers three cases:
    ///   1. User-initiated connection (`connecting_ssid` is set)
    ///   2. NM device is in a connecting state (PREPARE through SECONDARIES)
    ///   3. Wi-Fi enabled, not connected, not wired, initial scan in progress
    pub fn wifi_connecting(&self) -> bool {
        self.connecting_ssid().is_some()
            || self.wifi_device_connecting()
            || (self.wifi_enabled().unwrap_or(false)
                && !self.connected()
                && !self.wired_connected()
                && (self.scanning() || !self.is_ready()))
    }

    pub fn connection_state(&self) -> NetworkConnectionState {
        match self {
            Self::NetworkManager(inner) => {
                if inner.wifi.connecting_ssid.is_some()
                    || matches!(inner.wifi.device_state, Some(40..=90))
                {
                    NetworkConnectionState::Connecting
                } else if inner.wifi.connected {
                    NetworkConnectionState::Connected
                } else {
                    NetworkConnectionState::Disconnected
                }
            }
            Self::Iwd(inner) => {
                if inner.connecting() {
                    NetworkConnectionState::Connecting
                } else if inner.connected() {
                    NetworkConnectionState::Connected
                } else {
                    NetworkConnectionState::Disconnected
                }
            }
        }
    }

    /// Whether the system has an Ethernet (wired) network device.
    pub fn has_ethernet_device(&self) -> bool {
        match self {
            Self::NetworkManager(inner) => inner.wired.has_device,
            Self::Iwd(_) => false,
        }
    }

    /// Whether the system has a modem (cellular) device.
    pub fn has_modem_device(&self) -> bool {
        match self {
            Self::NetworkManager(inner) => inner.mobile.has_device,
            Self::Iwd(_) => false,
        }
    }

    /// Whether the system has any non-WiFi network device (Ethernet or cellular).
    pub fn has_non_wifi_device(&self) -> bool {
        self.has_ethernet_device() || self.has_modem_device()
    }

    /// Whether the system has wifi hardware.
    /// For IWD, implied by service availability (IWD requires wifi hardware).
    pub fn has_wifi_device(&self) -> bool {
        match self {
            Self::NetworkManager(inner) => inner.wifi.has_device,
            Self::Iwd(inner) => inner.available,
        }
    }

    pub fn is_ready(&self) -> bool {
        match self {
            Self::NetworkManager(inner) => inner.wifi.is_ready,
            // Mirrors NM's is_ready for consistent UI behavior.
            Self::Iwd(inner) => inner.initial_scan_complete,
        }
    }

    pub fn networks(&self) -> &[WifiNetwork] {
        match self {
            Self::NetworkManager(inner) => &inner.wifi.networks,
            Self::Iwd(inner) => &inner.networks,
        }
    }

    pub fn scanning(&self) -> bool {
        match self {
            Self::NetworkManager(inner) => inner.wifi.scanning,
            Self::Iwd(inner) => inner.scanning,
        }
    }

    pub fn wifi_enabled(&self) -> Option<bool> {
        match self {
            Self::NetworkManager(inner) => inner.wifi.enabled,
            Self::Iwd(inner) => inner.wifi_enabled,
        }
    }

    pub fn wired_connected(&self) -> bool {
        match self {
            Self::NetworkManager(inner) => inner.wired.connected,
            Self::Iwd(_) => false,
        }
    }

    /// Whether mobile data is the primary connection (set via NM's PrimaryConnectionType).
    pub fn mobile_is_primary(&self) -> bool {
        match self {
            Self::NetworkManager(inner) => inner.mobile.is_primary,
            Self::Iwd(_) => false,
        }
    }

    /// Whether a GSM/CDMA connection is activated (set from ModemManager device info).
    pub fn mobile_active(&self) -> bool {
        match self {
            Self::NetworkManager(inner) => inner.mobile.active,
            Self::Iwd(_) => false,
        }
    }

    /// Whether a GSM/CDMA connection is currently activating.
    pub fn mobile_connecting(&self) -> bool {
        match self {
            Self::NetworkManager(inner) => inner.mobile.connecting,
            Self::Iwd(_) => false,
        }
    }

    /// Whether mobile networking is supported (modem + SIM + profile all present).
    pub fn mobile_supported(&self) -> bool {
        match self {
            Self::NetworkManager(inner) => inner.mobile.supported,
            Self::Iwd(_) => false,
        }
    }

    /// Whether WWAN (mobile broadband) is enabled in NetworkManager, if known.
    pub fn mobile_enabled(&self) -> Option<bool> {
        match self {
            Self::NetworkManager(inner) => inner.mobile.enabled,
            Self::Iwd(_) => None,
        }
    }

    pub fn wired_iface(&self) -> Option<&str> {
        match self {
            Self::NetworkManager(inner) => inner.wired.iface.as_deref(),
            Self::Iwd(_) => None,
        }
    }

    pub fn wired_name(&self) -> Option<&str> {
        match self {
            Self::NetworkManager(inner) => inner.wired.name.as_deref(),
            Self::Iwd(_) => None,
        }
    }

    pub fn wired_speed(&self) -> Option<u32> {
        match self {
            Self::NetworkManager(inner) => inner.wired.speed,
            Self::Iwd(_) => None,
        }
    }

    /// Connection profile name for the active mobile connection (e.g., "MyCarrier").
    pub fn mobile_name(&self) -> Option<&str> {
        match self {
            Self::NetworkManager(inner) => inner.mobile.name.as_deref(),
            Self::Iwd(_) => None,
        }
    }

    /// Mobile carrier / operator name reported by the 3GPP modem.
    pub fn mobile_operator(&self) -> Option<&str> {
        match self {
            Self::NetworkManager(inner) => inner.mobile.operator.as_deref(),
            Self::Iwd(_) => None,
        }
    }

    /// Best available display name for the mobile connection.
    ///
    /// Prefers the operator name, falls back to the NM
    /// connection profile name (e.g., "MyCarrier"), and finally to "Mobile".
    pub fn mobile_display_name(&self) -> &str {
        self.mobile_operator()
            .or_else(|| self.mobile_name())
            .unwrap_or("Mobile")
    }

    /// Radio access technology label (e.g., "LTE", "5G NR", "HSPA+").
    pub fn mobile_access_technology(&self) -> Option<&str> {
        match self {
            Self::NetworkManager(inner) => inner.mobile.access_technology.as_deref(),
            Self::Iwd(_) => None,
        }
    }

    /// Signal quality percentage (0–100) from ModemManager.
    pub fn mobile_signal_quality(&self) -> Option<u32> {
        match self {
            Self::NetworkManager(inner) => inner.mobile.signal_quality,
            Self::Iwd(_) => None,
        }
    }

    /// Whether the last mobile connection attempt failed.
    pub fn mobile_failed(&self) -> bool {
        match self {
            Self::NetworkManager(inner) => inner.mobile.failed,
            Self::Iwd(_) => false,
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
            Self::NetworkManager(inner) => inner.wifi.failed_ssid.as_deref(),
            Self::Iwd(inner) => inner.failed_ssid.as_deref(),
        }
    }

    /// Signal strength of the active network, or 0 if not connected.
    pub fn active_strength(&self) -> i32 {
        match self {
            Self::NetworkManager(inner) => {
                if inner.wifi.connected {
                    inner.wifi.strength
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

    /// Human-readable reason for the last connection failure (IWD only).
    pub fn failed_reason(&self) -> Option<&str> {
        match self {
            Self::NetworkManager(_) => None,
            Self::Iwd(inner) => inner.failed_reason.as_deref(),
        }
    }
}

/// Unified network service abstracting over NetworkManager and IWD backends.
///
/// Automatically detects which backend is available at startup (preferring NM).
/// NM supports Wi-Fi, wired, and mobile; IWD supports Wi-Fi only.
///
/// Auth flow differs by backend: NM takes the password upfront via
/// `connect_to_network(ssid, password, _)`, while IWD initiates connection
/// first and calls back via the agent pattern — the UI then shows a password
/// dialog and calls `submit_password()` to complete authentication.
pub struct NetworkService {
    backend: NetworkBackend,
}

impl NetworkService {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            backend: detect_backend(),
        })
    }

    /// Get the global wifi service singleton.
    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<NetworkService> = NetworkService::new();
        }

        INSTANCE.with(|s| s.clone())
    }

    pub fn connect<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&NetworkSnapshot) + 'static,
    {
        match &self.backend {
            NetworkBackend::NetworkManager(inner) => inner.connect(move |snap| {
                let wrapped = NetworkSnapshot::NetworkManager(snap.clone());
                callback(&wrapped);
            }),
            NetworkBackend::Iwd(inner) => inner.connect(move |snap| {
                let wrapped = NetworkSnapshot::Iwd(snap.clone());
                callback(&wrapped);
            }),
        }
    }

    pub fn unsubscribe(&self, id: CallbackId) {
        match &self.backend {
            NetworkBackend::NetworkManager(inner) => inner.unsubscribe(id),
            NetworkBackend::Iwd(inner) => inner.unsubscribe(id),
        }
    }

    /// Re-emit the current snapshot to all callbacks without any state change.
    ///
    /// This triggers all network callbacks to re-evaluate their rendering logic.
    /// Used when external factors (e.g., icon theme switching between Material
    /// and GTK) require widgets to update their icon selection even though the
    /// underlying network state hasn't changed.
    pub fn re_notify(&self) {
        match &self.backend {
            NetworkBackend::NetworkManager(inner) => inner.re_notify(),
            NetworkBackend::Iwd(inner) => inner.re_notify(),
        }
    }

    /// Connect to a Wi-Fi network.
    ///
    /// - `ssid`: Network name (used by NM).
    /// - `password`: Optional password (NM only; IWD uses agent callbacks).
    /// - `path`: D-Bus object path (IWD only; ignored by NM).
    pub fn connect_to_network(&self, ssid: &str, password: Option<&str>, path: Option<&str>) {
        match &self.backend {
            NetworkBackend::NetworkManager(inner) => inner.connect_to_network(ssid, password),
            NetworkBackend::Iwd(inner) => {
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
            NetworkBackend::NetworkManager(inner) => inner.disconnect(),
            NetworkBackend::Iwd(inner) => inner.disconnect(),
        }
    }

    /// Forget a saved Wi-Fi network.
    ///
    /// - `ssid`: Network name (used by NM to delete the connection).
    /// - `path`: D-Bus KnownNetwork path (IWD only; ignored by NM).
    pub fn forget(&self, ssid: &str, path: Option<&str>) {
        match &self.backend {
            NetworkBackend::NetworkManager(inner) => inner.forget_network(ssid),
            NetworkBackend::Iwd(inner) => {
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
            NetworkBackend::NetworkManager(inner) => inner.scan_networks(),
            NetworkBackend::Iwd(inner) => inner.scan_networks(),
        }
    }

    pub fn set_wifi_enabled(&self, enabled: bool) {
        match &self.backend {
            NetworkBackend::NetworkManager(inner) => inner.set_wifi_enabled(enabled),
            NetworkBackend::Iwd(inner) => inner.set_wifi_enabled(enabled),
        }
    }

    pub fn set_mobile_enabled(&self, enabled: bool) {
        if let NetworkBackend::NetworkManager(inner) = &self.backend {
            inner.set_mobile_enabled(enabled);
        }
    }

    pub fn connect_mobile(&self) {
        if let NetworkBackend::NetworkManager(inner) = &self.backend {
            inner.connect_mobile();
        }
    }

    pub fn disconnect_mobile(&self) {
        if let NetworkBackend::NetworkManager(inner) = &self.backend {
            inner.disconnect_mobile();
        }
    }

    pub fn snapshot(&self) -> NetworkSnapshot {
        match &self.backend {
            NetworkBackend::NetworkManager(inner) => {
                NetworkSnapshot::NetworkManager(inner.snapshot())
            }
            NetworkBackend::Iwd(inner) => NetworkSnapshot::Iwd(inner.snapshot()),
        }
    }

    /// Submit a password for a pending IWD auth request (no-op for NM).
    pub fn submit_password(&self, password: &str) {
        match &self.backend {
            NetworkBackend::NetworkManager(_) => {}
            NetworkBackend::Iwd(inner) => inner.submit_passphrase(password),
        }
    }

    /// Cancel a pending auth request (IWD only).
    pub fn cancel_auth(&self) {
        match &self.backend {
            NetworkBackend::NetworkManager(_) => {}
            NetworkBackend::Iwd(inner) => inner.cancel_auth(),
        }
    }

    /// Clear the failed state (called when user cancels password dialog).
    pub fn clear_failed_state(&self) {
        match &self.backend {
            NetworkBackend::NetworkManager(inner) => inner.clear_failed_state(),
            NetworkBackend::Iwd(inner) => inner.clear_failed_state(),
        }
    }

    /// Clear the mobile failed connection state (called by UI after showing error).
    pub fn clear_mobile_failed_state(&self) {
        if let NetworkBackend::NetworkManager(inner) = &self.backend {
            inner.clear_mobile_failed_state();
        }
    }
}

/// Detect which Wi-Fi backend is available (NM preferred, then IWD).
///
/// If neither is running, defaults to NM — its `init_dbus()` handler fires
/// when the service registers on D-Bus.
fn detect_backend() -> NetworkBackend {
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
        return NetworkBackend::NetworkManager(NmService::global());
    }

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
        return NetworkBackend::Iwd(IwdService::global());
    }

    // Neither detected — default to NM. NM can be session-activated (late start
    // is common), whereas IWD starts early in boot and is unlikely to be missing.
    warn!(
        "Wi-Fi backend: neither NetworkManager nor IWD detected; \
         defaulting to NetworkManager (will activate when service appears)"
    );
    NetworkBackend::NetworkManager(NmService::global())
}

/// Extract a D-Bus object path (`type o`) from a [`glib::Variant`] as a `String`.
pub(super) fn objpath_to_string(v: &glib::Variant) -> Option<String> {
    v.get::<glib::variant::ObjectPath>()
        .map(|p| p.as_str().to_string())
}
