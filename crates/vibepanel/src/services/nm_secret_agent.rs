//! NetworkManager SecretAgent for VPN and Wi-Fi authentication.
//!
//! Implements the `org.freedesktop.NetworkManager.SecretAgent` D-Bus interface
//! to handle VPN and Wi-Fi password prompts directly in the panel, eliminating
//! the need for nm-applet.
//!
//! Wi-Fi secrets (`802-11-wireless-security`) are only answered while the panel
//! is activating that same network, so requests we didn't trigger (autoconnect,
//! resume) stay with agents that may hold the secret in a keyring. Every
//! request we can't serve gets `NoSecrets`, so NetworkManager asks the next agent.
//!
//! When NetworkManager activates a VPN connection that requires credentials,
//! it calls `GetSecrets` on registered secret agents. This module:
//! 1. Looks up the VPN plugin's auth-dialog binary from `.name` files
//! 2. Spawns the auth-dialog to collect or negotiate secrets
//! 3. For plugins with external-ui-mode: parses the response and shows an
//!    inline prompt in the panel
//! 4. For plugins without external-ui-mode: lets the auth-dialog show its own
//!    GTK window and returns the secrets directly
//! 5. Falls back to inferring fields from `vpn.data` `-flags` if no auth-dialog
//!    is available
//!
//! This approach matches what nm-applet does, but with inline UI instead of
//! popup dialogs for plugins that support external-ui-mode.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gtk4::gio::{self, prelude::*};
use gtk4::glib::{self, Variant};
use tracing::{debug, error, warn};

// D-Bus constants

const NM_SERVICE: &str = "org.freedesktop.NetworkManager";
const AGENT_MANAGER_PATH: &str = "/org/freedesktop/NetworkManager/AgentManager";
const AGENT_MANAGER_IFACE: &str = "org.freedesktop.NetworkManager.AgentManager";
const AGENT_IFACE: &str = "org.freedesktop.NetworkManager.SecretAgent";
const AGENT_PATH: &str = "/org/freedesktop/NetworkManager/SecretAgent";
const AGENT_IDENTIFIER: &str = "vibepanel";

/// D-Bus error returned when the user cancels authentication.
const AGENT_ERROR_USER_CANCELED: &str = "org.freedesktop.NetworkManager.SecretAgent.UserCanceled";
/// D-Bus error returned for requests this agent can't serve; NM asks the next agent.
const AGENT_ERROR_NO_SECRETS: &str = "org.freedesktop.NetworkManager.SecretAgent.NoSecrets";
/// D-Bus error for operations this agent doesn't support.
const AGENT_ERROR_FAILED: &str = "org.freedesktop.NetworkManager.SecretAgent.Failed";

const VPN_SETTING: &str = "vpn";
const WIFI_SECURITY_SETTING: &str = "802-11-wireless-security";

/// `NMSettingSecretFlags`: the secret is stored by a user agent, not NM.
const SECRET_FLAG_AGENT_OWNED: u32 = 0x1;
/// `NMSettingSecretFlags`: the secret is never stored ("always ask").
const SECRET_FLAG_NOT_SAVED: u32 = 0x2;
/// `NMSettingSecretFlags`: the secret is not needed.
const SECRET_FLAG_NOT_REQUIRED: u32 = 0x4;

/// Retries for `RegisterWithCapabilities` while another process holds our identifier.
const REGISTER_RETRIES: u32 = 5;
const REGISTER_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Timeout for pending auth requests. Auto-cancels if no user response.
pub(crate) const AUTH_TIMEOUT: Duration = Duration::from_secs(120);

/// GetSecrets flag: agent may prompt the user.
const FLAG_ALLOW_INTERACTION: u32 = 0x1;
/// GetSecrets flag: previous secrets were wrong, request new ones.
const FLAG_REQUEST_NEW: u32 = 0x2;

/// Default directories containing NM VPN plugin `.name` files, searched in order.
/// The `$NM_VPN_PLUGIN_DIR` environment variable, if set, is searched first.
const VPN_PLUGIN_DIRS: &[&str] = &["/usr/lib/NetworkManager/VPN", "/etc/NetworkManager/VPN"];

/// NM SecretAgent introspection XML.
const AGENT_INTROSPECTION: &str = r#"
<node>
    <interface name="org.freedesktop.NetworkManager.SecretAgent">
        <method name="GetSecrets">
            <arg type="a{sa{sv}}" name="connection" direction="in"/>
            <arg type="o" name="connection_path" direction="in"/>
            <arg type="s" name="setting_name" direction="in"/>
            <arg type="as" name="hints" direction="in"/>
            <arg type="u" name="flags" direction="in"/>
            <arg type="a{sa{sv}}" name="secrets" direction="out"/>
        </method>
        <method name="CancelGetSecrets">
            <arg type="o" name="connection_path" direction="in"/>
            <arg type="s" name="setting_name" direction="in"/>
        </method>
        <method name="SaveSecrets">
            <arg type="a{sa{sv}}" name="connection" direction="in"/>
            <arg type="o" name="connection_path" direction="in"/>
        </method>
        <method name="DeleteSecrets">
            <arg type="a{sa{sv}}" name="connection" direction="in"/>
            <arg type="o" name="connection_path" direction="in"/>
        </method>
    </interface>
</node>
"#;

/// A secret field that the user needs to provide.
#[derive(Debug, Clone)]
pub struct SecretField {
    /// The secret key name (e.g., "password", "cert-pass").
    pub key: String,
    /// Human-readable label for the UI (e.g., "Password", "Certificate password").
    pub label: String,
    /// Pre-filled value from auth-dialog (external-ui-mode) or existing secrets.
    pub value: Option<String>,
}

/// An authentication request from NetworkManager for VPN secrets.
#[derive(Debug, Clone)]
pub struct VpnAuthRequest {
    /// UUID of the VPN connection.
    pub uuid: String,
    /// Human-readable name of the VPN connection.
    pub name: String,
    /// Fields the user must fill in.
    pub fields: Vec<SecretField>,
    /// Whether this is a retry (previous secrets were wrong).
    pub is_retry: bool,
    /// Optional description from auth-dialog (external-ui-mode).
    pub description: Option<String>,
}

/// A Wi-Fi password request from NetworkManager for the network being activated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiAuthRequest {
    /// Agent request id; pass back to [`NmSecretAgent::submit_wifi_secret`].
    pub id: u64,
    pub ssid: String,
    /// Previous secret was rejected (NM sent `REQUEST_NEW`).
    pub is_retry: bool,
    /// The profile's secret is agent-owned, so NM won't store what we return.
    /// The UI offers to save it into the profile instead.
    pub offer_save: bool,
    /// The same attempt already chose to save (set by NmService on a retry),
    /// so the UI shows "Save password" ticked.
    pub save_ticked: bool,
    /// Settings object path of the profile.
    pub connection_path: String,
    /// Secret key in `802-11-wireless-security` (`psk` or `wep-keyN`).
    pub secret_key: String,
    /// Flags property for `secret_key` (`psk-flags` or `wep-key-flags`).
    pub flags_key: String,
}

/// What a Wi-Fi `GetSecrets` request asks for, parsed from the connection dict.
#[derive(Debug, PartialEq, Eq)]
struct WifiSecretSpec {
    ssid: String,
    secret_key: String,
    flags_key: String,
    offer_save: bool,
}

/// Which UI a pending request belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestKind {
    Vpn,
    Wifi,
}

/// Parsed VPN plugin metadata from a `.name` file.
struct VpnPluginInfo {
    /// VPN service D-Bus name (e.g., "org.freedesktop.NetworkManager.openvpn").
    service: String,
    /// Path to the auth-dialog binary (from `[GNOME]` section).
    auth_dialog: Option<String>,
    /// Whether the auth-dialog supports external-ui-mode.
    supports_external_ui_mode: bool,
    /// Whether the auth-dialog supports hints.
    supports_hints: bool,
}

/// A secret field parsed from an auth-dialog external-ui-mode response.
struct ExternalUiField {
    /// Secret key name (section name in the GKeyFile).
    key: String,
    /// Human-readable label.
    label: String,
    /// Pre-filled value (if any).
    value: String,
    /// Whether this is a secret (password) field.
    is_secret: bool,
    /// Whether the user should be prompted.
    should_ask: bool,
}

/// Result of spawning an auth-dialog process.
enum AuthDialogResult {
    /// Auth-dialog provided secrets directly (legacy mode — it showed its own UI).
    Secrets(Vec<(String, String)>),
    /// Auth-dialog provided field descriptions (external-ui-mode — we show inline UI).
    ExternalUi {
        description: String,
        fields: Vec<ExternalUiField>,
    },
    /// User cancelled (non-zero exit).
    Cancelled,
    /// Process failed to spawn or other error.
    Error(String),
}

/// How a pending request ends; see [`NmSecretAgent::finish`].
enum End {
    Secrets(Vec<(String, String)>),
    Cancel(String),
    TimedOut,
}

/// Pending D-Bus invocation waiting for user response.
struct PendingSecretRequest {
    /// Identifies this request to callbacks that may outlive it.
    id: u64,
    kind: RequestKind,
    invocation: gio::DBusMethodInvocation,
    /// Connection path for matching cancel requests.
    connection_path: String,
    /// Setting name for matching cancel requests.
    setting_name: String,
    /// GLib timeout source that fires after AUTH_TIMEOUT.
    timeout_source: Option<glib::SourceId>,
    /// Child auth-dialog PID (shared with background thread). Killed on cancel/timeout.
    child_pid: Option<Arc<Mutex<Option<u32>>>>,
    /// A legacy auth-dialog (own GTK window) holds the keyboard until this ends.
    legacy_dialog: bool,
    /// Pre-filled secret values from auth-dialog (external-ui-mode) that don't
    /// need user prompting (`is_secret && !should_ask`). These are merged into
    /// the response when the user submits, so NM receives all expected secrets.
    prefilled_secrets: Vec<(String, String)>,
}

/// NetworkManager SecretAgent implementation.
///
/// This is a thread-local singleton that handles NM's secret requests for VPN
/// and Wi-Fi connections.
pub struct NmSecretAgent {
    /// D-Bus registration ID for the agent object.
    agent_registration_id: RefCell<Option<gio::RegistrationId>>,
    /// Requests waiting for user response, at most one per kind so a VPN and
    /// a Wi-Fi prompt can be open at the same time.
    pending: RefCell<Vec<PendingSecretRequest>>,
    last_request_id: Cell<u64>,
    /// Whether a VPN UI exists to prompt in. Without one, VPN requests are
    /// declined so another agent (e.g. nm-applet) can answer them.
    vpn_enabled: Cell<bool>,
}

impl NmSecretAgent {
    fn new() -> Self {
        Self {
            agent_registration_id: RefCell::new(None),
            pending: RefCell::new(Vec::new()),
            last_request_id: Cell::new(0),
            vpn_enabled: Cell::new(false),
        }
    }

    /// Get the global NmSecretAgent singleton.
    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<NmSecretAgent> = Rc::new(NmSecretAgent::new());
        }
        INSTANCE.with(|s| s.clone())
    }

    /// Initialize and register the secret agent on the given D-Bus connection.
    ///
    /// Called from NmService and VpnService once the system bus is available;
    /// repeated calls are no-ops.
    pub fn register(connection: &gio::DBusConnection) {
        let agent = Self::global();

        // Already registered
        if agent.agent_registration_id.borrow().is_some() {
            return;
        }

        let node_info = match gio::DBusNodeInfo::for_xml(AGENT_INTROSPECTION) {
            Ok(info) => info,
            Err(e) => {
                error!("NmSecretAgent: failed to parse introspection XML: {}", e);
                return;
            }
        };

        let interface_info = match node_info.lookup_interface(AGENT_IFACE) {
            Some(info) => info,
            None => {
                error!("NmSecretAgent: interface not found in introspection");
                return;
            }
        };

        // Register the agent D-Bus object
        let registration = connection
            .register_object(AGENT_PATH, &interface_info)
            .method_call(
                move |_conn, _sender, _path, _iface, method, params, invocation| {
                    Self::handle_method(method, params, invocation);
                },
            )
            .build();

        match registration {
            Ok(id) => {
                debug!("NmSecretAgent: registered object at {}", AGENT_PATH);
                *agent.agent_registration_id.borrow_mut() = Some(id);

                // Register with each NM instance's AgentManager as it appears
                // (now, and again after NM restarts). The agent lives for the
                // whole process, so the watch is never removed.
                gio::bus_watch_name_on_connection(
                    connection,
                    NM_SERVICE,
                    gio::BusNameWatcherFlags::NONE,
                    |connection, _name, _owner| {
                        Self::register_with_agent_manager(&connection, REGISTER_RETRIES);
                    },
                    |_connection, _name| {
                        let ids: Vec<u64> = Self::global()
                            .pending
                            .borrow()
                            .iter()
                            .map(|p| p.id)
                            .collect();
                        for id in ids {
                            Self::finish(id, End::Cancel("NetworkManager exited".to_string()));
                        }
                    },
                );
            }
            Err(e) => {
                error!("NmSecretAgent: failed to register D-Bus object: {}", e);
            }
        }
    }

    /// Answer VPN requests; called by VpnService, which shows the prompts.
    pub fn enable_vpn() {
        Self::global().vpn_enabled.set(true);
    }

    /// Register with NetworkManager's AgentManager.
    fn register_with_agent_manager(connection: &gio::DBusConnection, retries: u32) {
        let retry_connection = connection.clone();
        gio::DBusProxy::new(
            connection,
            gio::DBusProxyFlags::NONE,
            None,
            Some(NM_SERVICE),
            AGENT_MANAGER_PATH,
            AGENT_MANAGER_IFACE,
            None::<&gio::Cancellable>,
            move |res| {
                let proxy = match res {
                    Ok(p) => p,
                    Err(e) => {
                        error!("NmSecretAgent: failed to create AgentManager proxy: {}", e);
                        return;
                    }
                };

                // 0x1 = NM_SECRET_AGENT_CAPABILITY_VPN_HINTS
                let args = (AGENT_IDENTIFIER, 0x1_u32).to_variant();

                proxy.call(
                    "RegisterWithCapabilities",
                    Some(&args),
                    gio::DBusCallFlags::NONE,
                    5000,
                    None::<&gio::Cancellable>,
                    move |res| match res {
                        Ok(_) => {
                            debug!("NmSecretAgent: registered with AgentManager");
                        }
                        Err(e) => {
                            let error_name = gio::DBusError::remote_error(&e);
                            let error_str = error_name.as_ref().map(|s| s.as_str()).unwrap_or("");

                            // We register once per NM instance, so this is another
                            // process, usually a vibepanel instance still exiting.
                            if error_str
                                == "org.freedesktop.NetworkManager.AgentManager.PermissionDenied"
                                && e.message().contains("already registered")
                                && retries > 0
                            {
                                debug!(
                                    "NmSecretAgent: identifier held by another process, retrying"
                                );
                                glib::timeout_add_local_once(REGISTER_RETRY_DELAY, move || {
                                    Self::register_with_agent_manager(
                                        &retry_connection,
                                        retries - 1,
                                    );
                                });
                            } else {
                                error!(
                                    "NmSecretAgent: failed to register with AgentManager: {}",
                                    e
                                );
                            }
                        }
                    },
                );
            },
        );
    }

    /// Dispatch incoming D-Bus method calls.
    fn handle_method(method: &str, params: Variant, invocation: gio::DBusMethodInvocation) {
        debug!("NmSecretAgent: method '{}' called", method);

        match method {
            "GetSecrets" => Self::handle_get_secrets(params, invocation),
            "CancelGetSecrets" => Self::handle_cancel_get_secrets(params, invocation),
            // NM stops at the first agent that reports a save as done, so decline
            // and let a storing agent (e.g. a keyring) have it.
            "SaveSecrets" => {
                invocation
                    .return_dbus_error(AGENT_ERROR_FAILED, "vibepanel does not store secrets");
            }
            // NM asks every agent to delete regardless of the reply.
            "DeleteSecrets" => invocation.return_value(None),
            _ => {
                error!("NmSecretAgent: unknown method: {}", method);
                invocation.return_error(
                    gio::IOErrorEnum::NotSupported,
                    &format!("Unknown method: {method}"),
                );
            }
        }
    }

    /// Handle GetSecrets — NM is asking for VPN or Wi-Fi connection secrets.
    fn handle_get_secrets(params: Variant, invocation: gio::DBusMethodInvocation) {
        // Parse parameters: (a{sa{sv}}, o, s, as, u)
        debug!(
            "NmSecretAgent: GetSecrets params type={}, n_children={}",
            params.type_(),
            params.n_children()
        );

        if params.n_children() < 5 {
            error!(
                "NmSecretAgent: GetSecrets expected 5 params, got {}",
                params.n_children()
            );
            invocation.return_error(
                gio::IOErrorEnum::InvalidArgument,
                "Expected 5 parameters for GetSecrets",
            );
            return;
        }

        let connection_dict = params.child_value(0);
        let connection_path: String = params
            .child_value(1)
            .get()
            .or_else(|| params.child_value(1).str().map(|s| s.to_string()))
            .unwrap_or_default();
        let setting_name: String = params.child_value(2).get().unwrap_or_default();
        let hints_variant = params.child_value(3);
        let flags: u32 = params.child_value(4).get().unwrap_or(0);

        debug!(
            "NmSecretAgent: GetSecrets for path={}, setting={}, flags=0x{:x}",
            connection_path, setting_name, flags
        );

        // We have no stored secrets, so without interaction there's nothing to give.
        if flags & FLAG_ALLOW_INTERACTION == 0 && flags & FLAG_REQUEST_NEW == 0 {
            no_secrets(invocation, "interaction not allowed");
            return;
        }

        match setting_name.as_str() {
            VPN_SETTING if !Self::global().vpn_enabled.get() => {
                no_secrets(invocation, "VPN prompts are not enabled");
                return;
            }
            VPN_SETTING => {}
            WIFI_SECURITY_SETTING => {
                Self::handle_wifi_get_secrets(
                    &connection_dict,
                    connection_path,
                    setting_name,
                    flags,
                    invocation,
                );
                return;
            }
            _ => {
                no_secrets(invocation, &format!("unsupported setting '{setting_name}'"));
                return;
            }
        }

        // Extract connection info from the connection dict
        let (uuid, name) = Self::extract_connection_info(&connection_dict);
        let uuid = uuid.unwrap_or_default();
        let name = name.unwrap_or_else(|| "VPN".to_string());
        let is_retry = flags & FLAG_REQUEST_NEW != 0;

        // Collect hints
        let mut hints = Vec::new();
        let n_hints = hints_variant.n_children();
        for i in 0..n_hints {
            let hint: String = hints_variant.child_value(i).get().unwrap_or_default();
            if !hint.is_empty() {
                hints.push(hint);
            }
        }

        // Try auth-dialog path first
        let service_type = Self::extract_vpn_service_type(&connection_dict);
        if let Some(ref svc) = service_type
            && let Some(plugin) = find_vpn_plugin(svc)
            && plugin.auth_dialog.is_some()
        {
            debug!(
                "NmSecretAgent: found auth-dialog for service '{}', external-ui={}",
                svc, plugin.supports_external_ui_mode
            );

            Self::spawn_auth_dialog(
                &plugin,
                &connection_dict,
                &uuid,
                &name,
                svc,
                &hints,
                flags,
                invocation,
                connection_path,
                setting_name,
                is_retry,
            );
            return;
        }

        // Fallback: no auth-dialog found, infer fields from -flags
        debug!("NmSecretAgent: no auth-dialog found, using -flags fallback");
        Self::handle_get_secrets_fallback(
            &connection_dict,
            &uuid,
            &name,
            &hints,
            flags,
            invocation,
            connection_path,
            setting_name,
            is_retry,
        );
    }

    /// Handle a Wi-Fi `GetSecrets`: prompt only if the panel is activating this network.
    fn handle_wifi_get_secrets(
        connection_dict: &Variant,
        connection_path: String,
        setting_name: String,
        flags: u32,
        invocation: gio::DBusMethodInvocation,
    ) {
        let spec = match wifi_secret_spec(connection_dict) {
            Ok(spec) => spec,
            Err(reason) => {
                no_secrets(invocation, reason);
                return;
            }
        };
        let connecting = super::network::NmService::connecting_wifi_ssid();
        if connecting.as_deref() != Some(spec.ssid.as_str()) {
            no_secrets(invocation, "panel is not activating this network");
            return;
        }

        let id = Self::start_request(
            invocation,
            RequestKind::Wifi,
            connection_path.clone(),
            setting_name,
            None,
            false,
        );
        debug!(
            "NmSecretAgent: requesting Wi-Fi secret '{}' for '{}' (retry={}, offer_save={})",
            spec.secret_key,
            spec.ssid,
            flags & FLAG_REQUEST_NEW != 0,
            spec.offer_save
        );
        super::network::network_manager::wifi_auth_requested(WifiAuthRequest {
            id,
            ssid: spec.ssid,
            is_retry: flags & FLAG_REQUEST_NEW != 0,
            offer_save: spec.offer_save,
            save_ticked: false,
            connection_path,
            secret_key: spec.secret_key,
            flags_key: spec.flags_key,
        });
    }

    /// Fallback GetSecrets handler: infer fields from vpn.data `-flags` keys.
    ///
    /// Used when no auth-dialog binary is available for this VPN plugin.
    #[allow(clippy::too_many_arguments)]
    fn handle_get_secrets_fallback(
        connection_dict: &Variant,
        uuid: &str,
        name: &str,
        hints: &[String],
        _flags: u32,
        invocation: gio::DBusMethodInvocation,
        connection_path: String,
        setting_name: String,
        is_retry: bool,
    ) {
        // Parse vpn.data to find which secrets are needed
        let mut fields = Self::extract_secret_fields(connection_dict);

        // Also check hints for additional requested fields
        for hint in hints {
            // Skip special NM hint prefixes
            if hint.starts_with("x-vpn-message:")
                || hint.starts_with("x-dynamic-challenge:")
                || hint.starts_with("x-dynamic-challenge-echo:")
            {
                continue;
            }
            // Add as a field if not already present
            if !fields.iter().any(|f| f.key == *hint) {
                fields.push(SecretField {
                    label: humanize_secret_key(hint),
                    key: hint.clone(),
                    value: None,
                });
            }
        }

        // If no fields found, add a generic password field
        if fields.is_empty() {
            fields.push(SecretField {
                key: "password".to_string(),
                label: "Password".to_string(),
                value: None,
            });
        }

        let auth_request = VpnAuthRequest {
            uuid: uuid.to_string(),
            name: name.to_string(),
            fields,
            is_retry,
            description: None,
        };

        debug!(
            "NmSecretAgent: requesting auth for '{}' (uuid={}, retry={}, fields={})",
            name,
            uuid,
            is_retry,
            auth_request.fields.len()
        );

        Self::start_request(
            invocation,
            RequestKind::Vpn,
            connection_path,
            setting_name,
            None,
            false,
        );

        // Notify VPN service about the auth request
        super::vpn::send_vpn_update(super::vpn::VpnUpdate::AuthRequest(auth_request));
    }

    /// Spawn the VPN plugin's auth-dialog binary to collect secrets.
    ///
    /// For plugins with external-ui-mode, the auth-dialog runs headlessly and
    /// returns field descriptions, which we render as an inline prompt.
    /// For plugins without external-ui-mode, the auth-dialog shows its own GTK
    /// window and returns secrets directly.
    #[allow(clippy::too_many_arguments)]
    fn spawn_auth_dialog(
        plugin: &VpnPluginInfo,
        connection_dict: &Variant,
        uuid: &str,
        name: &str,
        service_type: &str,
        hints: &[String],
        flags: u32,
        invocation: gio::DBusMethodInvocation,
        connection_path: String,
        setting_name: String,
        is_retry: bool,
    ) {
        let auth_dialog_path = match plugin.auth_dialog {
            Some(ref p) => p.clone(),
            None => {
                error!("NmSecretAgent: spawn_auth_dialog called without auth-dialog path");
                invocation.return_dbus_error(AGENT_ERROR_USER_CANCELED, "No auth-dialog available");
                return;
            }
        };

        // Build command-line arguments
        let mut args = vec![
            "-u".to_string(),
            uuid.to_string(),
            "-n".to_string(),
            name.to_string(),
            "-s".to_string(),
            service_type.to_string(),
        ];

        if flags & FLAG_ALLOW_INTERACTION != 0 || flags & FLAG_REQUEST_NEW != 0 {
            args.push("-i".to_string());
        }
        if flags & FLAG_REQUEST_NEW != 0 {
            args.push("-r".to_string());
        }

        // Add hints if supported
        if plugin.supports_hints {
            for hint in hints {
                args.push("-t".to_string());
                args.push(hint.clone());
            }
        }

        let use_external_ui = plugin.supports_external_ui_mode;
        if use_external_ui {
            args.push("--external-ui-mode".to_string());
        }

        // Build stdin payload
        let stdin_data = build_auth_dialog_stdin(connection_dict);

        // Shared PID for cancel/timeout cleanup
        let child_pid: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
        let child_pid_thread = child_pid.clone();

        // Clone data for the closure
        let uuid_clone = uuid.to_string();
        let name_clone = name.to_string();

        let id = Self::start_request(
            invocation,
            RequestKind::Vpn,
            connection_path,
            setting_name,
            Some(child_pid),
            !use_external_ui,
        );

        // Spawn auth-dialog on a background thread
        debug!(
            "NmSecretAgent: spawning auth-dialog: {} {}",
            auth_dialog_path,
            if use_external_ui {
                "(external-ui)"
            } else {
                "(legacy)"
            }
        );

        // For legacy auth-dialogs, notify the UI to release the keyboard grab
        // so the dialog's own GTK window can receive keyboard input.
        if !use_external_ui {
            super::vpn::send_vpn_update(super::vpn::VpnUpdate::LegacyAuthDialogSpawned);
        }

        std::thread::spawn(move || {
            let result = run_auth_dialog(
                &auth_dialog_path,
                &args,
                &stdin_data,
                &child_pid_thread,
                use_external_ui,
            );

            // Marshal result back to main thread
            glib::idle_add_once(move || {
                Self::handle_auth_dialog_result(id, result, &uuid_clone, &name_clone, is_retry);
            });
        });
    }

    /// Handle the result from the auth-dialog background thread.
    ///
    /// Called on the main thread via `glib::idle_add_once`. `id` is the
    /// request the dialog was spawned for; it may have ended since.
    fn handle_auth_dialog_result(
        id: u64,
        result: AuthDialogResult,
        uuid: &str,
        name: &str,
        is_retry: bool,
    ) {
        match result {
            AuthDialogResult::Secrets(secrets) => {
                debug!(
                    "NmSecretAgent: auth-dialog returned {} secret(s)",
                    secrets.len()
                );
                Self::finish(id, End::Secrets(secrets));
            }

            AuthDialogResult::ExternalUi {
                description,
                fields,
            } => {
                // Determine which fields need user input
                let ask_fields: Vec<&ExternalUiField> = fields
                    .iter()
                    .filter(|f| f.is_secret && f.should_ask)
                    .collect();

                if ask_fields.is_empty() {
                    // No fields need asking — return all secret values directly
                    debug!(
                        "NmSecretAgent: external-ui has no fields to ask, returning existing secrets"
                    );
                    let secrets: Vec<(String, String)> = fields
                        .iter()
                        .filter(|f| f.is_secret && !f.value.is_empty())
                        .map(|f| (f.key.clone(), f.value.clone()))
                        .collect();
                    Self::finish(id, End::Secrets(secrets));
                    return;
                }

                // Build auth request for inline UI with fields that need prompting
                let secret_fields: Vec<SecretField> = fields
                    .iter()
                    .filter(|f| f.is_secret && f.should_ask)
                    .map(|f| SecretField {
                        key: f.key.clone(),
                        label: f.label.clone(),
                        value: if f.value.is_empty() {
                            None
                        } else {
                            Some(f.value.clone())
                        },
                    })
                    .collect();

                // Collect non-asked secret values (is_secret && !should_ask) so they
                // can be merged into the response when the user submits. Without this,
                // NM wouldn't receive pre-filled secrets like stored cert passwords.
                let prefilled: Vec<(String, String)> = fields
                    .iter()
                    .filter(|f| f.is_secret && !f.should_ask && !f.value.is_empty())
                    .map(|f| (f.key.clone(), f.value.clone()))
                    .collect();

                let auth_request = VpnAuthRequest {
                    uuid: uuid.to_string(),
                    name: name.to_string(),
                    fields: secret_fields,
                    is_retry,
                    description: if description.is_empty() {
                        None
                    } else {
                        Some(description)
                    },
                };

                debug!(
                    "NmSecretAgent: external-ui requesting {} field(s) for inline prompt",
                    auth_request.fields.len()
                );

                // Only prompt if this request is still pending (not ended by a
                // timeout, cancel or newer request while the auth-dialog ran).
                let agent = Self::global();
                let mut pending = agent.pending.borrow_mut();
                if let Some(pending) = pending.iter_mut().find(|p| p.id == id) {
                    pending.prefilled_secrets = prefilled;
                    super::vpn::send_vpn_update(super::vpn::VpnUpdate::AuthRequest(auth_request));
                } else {
                    debug!("NmSecretAgent: request {id} already finished, skipping auth prompt");
                }
            }

            AuthDialogResult::Cancelled => {
                debug!("NmSecretAgent: auth-dialog cancelled by user");
                Self::finish(id, End::Cancel("User canceled".to_string()));
            }

            AuthDialogResult::Error(msg) => {
                error!("NmSecretAgent: auth-dialog error: {}", msg);
                // Cancel the pending request and let NM retry
                Self::finish(id, End::Cancel(format!("Auth dialog failed: {msg}")));
            }
        }
    }

    /// Handle CancelGetSecrets — NM is cancelling a previous GetSecrets request.
    fn handle_cancel_get_secrets(params: Variant, invocation: gio::DBusMethodInvocation) {
        if params.n_children() < 2 {
            error!(
                "NmSecretAgent: CancelGetSecrets expected 2 params, got {}",
                params.n_children()
            );
            invocation.return_value(None);
            return;
        }

        let connection_path: String = params
            .child_value(0)
            .get()
            .or_else(|| params.child_value(0).str().map(|s| s.to_string()))
            .unwrap_or_default();
        let setting_name: String = params.child_value(1).get().unwrap_or_default();

        debug!(
            "NmSecretAgent: CancelGetSecrets for path={}, setting={}",
            connection_path, setting_name
        );

        let matching = Self::global()
            .pending
            .borrow()
            .iter()
            .find(|p| p.connection_path == connection_path && p.setting_name == setting_name)
            .map(|p| p.id);
        if let Some(id) = matching {
            Self::finish(id, End::Cancel("Canceled by NetworkManager".to_string()));
        }

        invocation.return_value(None);
    }

    /// Submit secrets in response to a pending VPN auth request.
    ///
    /// Called by VpnService when the user submits credentials from the UI.
    /// `secrets` is a list of (key, value) pairs (e.g., [("password", "hunter2")]).
    pub fn submit_secrets(secrets: &[(String, String)]) {
        let Some(id) = Self::current_id(RequestKind::Vpn) else {
            debug!("NmSecretAgent: submit_secrets called but no pending VPN request");
            return;
        };
        debug!(
            "NmSecretAgent: submitting {} user-provided secret(s)",
            secrets.len()
        );
        Self::finish(id, End::Secrets(secrets.to_vec()));
    }

    /// Cancel a pending VPN auth request (user pressed Cancel in the UI).
    pub fn cancel_auth() {
        if let Some(id) = Self::current_id(RequestKind::Vpn) {
            debug!("NmSecretAgent: user cancelled VPN auth");
            Self::finish(id, End::Cancel("User canceled".to_string()));
        }
    }

    /// Answer Wi-Fi request `request` with the password the user entered.
    /// Returns false if the request is no longer pending.
    pub fn submit_wifi_secret(request: &WifiAuthRequest, secret: &str) -> bool {
        if Self::current_id(RequestKind::Wifi) != Some(request.id) {
            debug!(
                "NmSecretAgent: Wi-Fi request {} is no longer pending",
                request.id
            );
            return false;
        }
        Self::finish(
            request.id,
            End::Secrets(vec![(request.secret_key.clone(), secret.to_string())]),
        );
        true
    }

    /// Cancel Wi-Fi request `id` (user pressed Cancel, or the attempt was replaced).
    pub fn cancel_wifi_auth(id: u64) {
        if Self::current_id(RequestKind::Wifi) == Some(id) {
            debug!("NmSecretAgent: Wi-Fi auth {id} cancelled");
            Self::finish(id, End::Cancel("User canceled".to_string()));
        }
    }

    fn current_id(kind: RequestKind) -> Option<u64> {
        Self::global()
            .pending
            .borrow()
            .iter()
            .find(|p| p.kind == kind)
            .map(|p| p.id)
    }

    /// Make `invocation` the pending request of its kind, ending the current
    /// one of that kind, and arm its timeout. Returns the id to pass to
    /// [`Self::finish`].
    fn start_request(
        invocation: gio::DBusMethodInvocation,
        kind: RequestKind,
        connection_path: String,
        setting_name: String,
        child_pid: Option<Arc<Mutex<Option<u32>>>>,
        legacy_dialog: bool,
    ) -> u64 {
        if let Some(id) = Self::current_id(kind) {
            Self::finish(
                id,
                End::Cancel("Superseded by new auth request".to_string()),
            );
        }
        let agent = Self::global();
        let id = agent.last_request_id.get() + 1;
        agent.last_request_id.set(id);
        let timeout_source = glib::timeout_add_local_once(AUTH_TIMEOUT, move || {
            warn!(
                "NmSecretAgent: auth request timed out after {}s",
                AUTH_TIMEOUT.as_secs()
            );
            Self::finish(id, End::TimedOut);
        });
        agent.pending.borrow_mut().push(PendingSecretRequest {
            id,
            kind,
            invocation,
            connection_path,
            setting_name,
            timeout_source: Some(timeout_source),
            child_pid,
            legacy_dialog,
            prefilled_secrets: Vec::new(), // Populated when ExternalUi result arrives
        });
        id
    }

    /// The single exit for a pending request: reply to NM, stop its timeout
    /// and auth-dialog, and clear the auth UI. Does nothing unless `id` is
    /// still pending, so a superseded or timed-out auth-dialog's late result
    /// can't answer or clear a newer request.
    fn finish(id: u64, end: End) {
        let agent = Self::global();
        let pending = {
            let mut pending = agent.pending.borrow_mut();
            pending
                .iter()
                .position(|p| p.id == id)
                .map(|index| pending.remove(index))
        };
        let Some(mut pending) = pending else {
            debug!("NmSecretAgent: request {id} already finished");
            return;
        };

        // A fired timeout has already removed its source.
        if let Some(source_id) = pending.timeout_source.take()
            && !matches!(end, End::TimedOut)
        {
            source_id.remove();
        }
        kill_child_process(&pending.child_pid);

        match end {
            End::Secrets(secrets) => {
                // Merge pre-filled secrets (from auth-dialog external-ui-mode)
                // with the result. The result takes precedence on key conflicts.
                let mut merged: HashMap<String, String> =
                    pending.prefilled_secrets.into_iter().collect();
                merged.extend(secrets);
                let merged: Vec<(String, String)> = merged.into_iter().collect();
                let response = match pending.kind {
                    RequestKind::Vpn => {
                        Self::build_secrets_response(&pending.setting_name, &merged)
                    }
                    RequestKind::Wifi => {
                        build_setting_secrets_response(&pending.setting_name, &merged)
                    }
                };
                pending.invocation.return_value(Some(&response));
            }
            End::Cancel(msg) => pending
                .invocation
                .return_dbus_error(AGENT_ERROR_USER_CANCELED, &msg),
            End::TimedOut => pending
                .invocation
                .return_dbus_error(AGENT_ERROR_USER_CANCELED, "Authentication timed out"),
        }

        match pending.kind {
            RequestKind::Vpn => {
                if pending.legacy_dialog {
                    // Restore the keyboard grab released for the dialog's own window.
                    super::vpn::send_vpn_update(super::vpn::VpnUpdate::LegacyAuthDialogFinished);
                }
                super::vpn::send_vpn_update(super::vpn::VpnUpdate::AuthCleared);
            }
            RequestKind::Wifi => super::network::network_manager::wifi_auth_cleared(id),
        }
    }

    // --- Variant parsing helpers ---

    /// Extract UUID and connection name from the connection dict.
    fn extract_connection_info(connection_dict: &Variant) -> (Option<String>, Option<String>) {
        let conn_section = match get_dict_section(connection_dict, "connection") {
            Some(s) => s,
            None => return (None, None),
        };

        let uuid = get_string_from_dict(&conn_section, "uuid");
        let name = get_string_from_dict(&conn_section, "id");
        (uuid, name)
    }

    /// Extract the VPN service type from the connection dict.
    ///
    /// This is the `service-type` key in the `vpn` section, e.g.,
    /// `"org.freedesktop.NetworkManager.openvpn"`.
    fn extract_vpn_service_type(connection_dict: &Variant) -> Option<String> {
        let vpn_section = get_dict_section(connection_dict, "vpn")?;
        get_string_from_dict(&vpn_section, "service-type")
    }

    /// Extract secret fields from the vpn.data dict by scanning for `-flags` keys.
    fn extract_secret_fields(connection_dict: &Variant) -> Vec<SecretField> {
        let mut fields = Vec::new();

        let vpn_section = match get_dict_section(connection_dict, "vpn") {
            Some(s) => s,
            None => return fields,
        };

        let data_dict = match get_variant_dict_value(&vpn_section, "data") {
            Some(d) => d,
            None => return fields,
        };

        // Scan vpn.data for keys ending in "-flags"
        let n = data_dict.n_children();
        for i in 0..n {
            let entry = data_dict.child_value(i);
            let key_variant = entry.child_value(0);
            let Some(key) = key_variant.str() else {
                continue;
            };

            if !key.ends_with("-flags") {
                continue;
            }

            // Get the flags value — vpn.data is a{ss} so values are plain strings
            let value_variant = entry.child_value(1);
            let flags_str = value_variant.str().unwrap_or_default();

            let flags_val: u32 = flags_str.parse().unwrap_or(0);

            // Bit 0 (0x1) = agent-owned, bit 1 (0x2) = not-saved (always ask),
            // bit 2 (0x4) = not-required. Prompt if agent-owned or not-saved,
            // but skip if not-required is set.
            if flags_val & 0x3 != 0 && flags_val & 0x4 == 0 {
                let secret_key = &key[..key.len() - "-flags".len()];
                fields.push(SecretField {
                    key: secret_key.to_string(),
                    label: humanize_secret_key(secret_key),
                    value: None,
                });
            }
        }

        fields
    }

    /// Build the secrets response dict for a VPN connection.
    ///
    /// Format: `({ "vpn": { "secrets": { key: value, ... } } },)`
    /// D-Bus type: `(a{sa{sv}})`
    fn build_secrets_response(setting_name: &str, secrets: &[(String, String)]) -> Variant {
        // nm-applet builds the response as:
        //   {"vpn": {"secrets": variant(a{ss}{key: value, ...})}}
        //
        // The inner secrets dict is a{ss} (string→string), NOT a{sv}.
        // It is wrapped in a variant and stored under the "secrets" key
        // in the VPN setting dict (a{sv}).

        // Build the inner secrets dict: a{ss}
        let ss_type = glib::VariantTy::new("{ss}").unwrap();
        let secrets_entries: Vec<Variant> = secrets
            .iter()
            .map(|(k, v)| Variant::from_dict_entry(&k.to_variant(), &v.to_variant()))
            .collect();
        let secrets_dict = if secrets_entries.is_empty() {
            Variant::array_from_iter_with_type(ss_type, std::iter::empty::<Variant>())
        } else {
            Variant::array_from_iter_with_type(ss_type, secrets_entries)
        };

        // Build the VPN setting dict: a{sv} with one entry "secrets" -> v(a{ss})
        let setting_builder = glib::VariantDict::new(None);
        setting_builder.insert("secrets", secrets_dict);

        // Create a dict entry: {sa{sv}}
        let entry = Variant::from_dict_entry(&setting_name.to_variant(), &setting_builder.end());

        // Create the outer array: a{sa{sv}}
        let entry_type = glib::VariantTy::new("{sa{sv}}").unwrap();
        let outer = Variant::array_from_iter_with_type(entry_type, [entry]);

        // Wrap in tuple for D-Bus return value: (a{sa{sv}})
        Variant::tuple_from_iter([outer])
    }
}

/// Decline a request so NetworkManager asks the next agent.
fn no_secrets(invocation: gio::DBusMethodInvocation, reason: &str) {
    debug!("NmSecretAgent: NoSecrets: {reason}");
    invocation.return_dbus_error(AGENT_ERROR_NO_SECRETS, reason);
}

/// Build `({setting_name: {key: value, ...}},)` for settings whose secrets are
/// plain string properties, like `802-11-wireless-security`.
fn build_setting_secrets_response(setting_name: &str, secrets: &[(String, String)]) -> Variant {
    let setting = glib::VariantDict::new(None);
    for (key, value) in secrets {
        setting.insert(key, value);
    }
    let entry = Variant::from_dict_entry(&setting_name.to_variant(), &setting.end());
    let entry_type = glib::VariantTy::new("{sa{sv}}").unwrap();
    Variant::tuple_from_iter([Variant::array_from_iter_with_type(entry_type, [entry])])
}

/// Work out which Wi-Fi secret a `GetSecrets` request needs, or why we can't
/// provide it. Only passphrase-style secrets (WPA/WPA3 personal and static WEP)
/// are supported; enterprise and LEAP need fields we don't prompt for.
fn wifi_secret_spec(connection_dict: &Variant) -> Result<WifiSecretSpec, &'static str> {
    let ssid = get_dict_section(connection_dict, "802-11-wireless")
        .and_then(|wifi| get_variant_dict_value(&wifi, "ssid"))
        .and_then(|ssid| ssid.get::<Vec<u8>>())
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .filter(|ssid| !ssid.is_empty())
        .ok_or("Wi-Fi profile has no valid SSID")?;
    let security =
        get_dict_section(connection_dict, WIFI_SECURITY_SETTING).ok_or("no Wi-Fi security")?;
    let key_mgmt = get_string_from_dict(&security, "key-mgmt").unwrap_or_default();
    let (secret_key, flags_key) = match key_mgmt.as_str() {
        "wpa-psk" | "sae" => ("psk".to_string(), "psk-flags"),
        "none" if get_string_from_dict(&security, "auth-alg").as_deref() != Some("leap") => {
            let index = get_u32_from_dict(&security, "wep-tx-keyidx").unwrap_or(0);
            (format!("wep-key{index}"), "wep-key-flags")
        }
        _ => return Err("unsupported Wi-Fi security"),
    };
    // Properties at their default value are omitted from the dict.
    let flags = get_u32_from_dict(&security, flags_key).unwrap_or(0);
    if flags & SECRET_FLAG_NOT_REQUIRED != 0 {
        return Err("secret not required");
    }
    // wep-key-flags covers all four WEP keys, but saving would write only
    // this one and drop the others, so saving is PSK/SAE only.
    let offer_save = secret_key == "psk"
        && flags & SECRET_FLAG_AGENT_OWNED != 0
        && flags & SECRET_FLAG_NOT_SAVED == 0;
    Ok(WifiSecretSpec {
        ssid,
        secret_key,
        flags_key: flags_key.to_string(),
        offer_save,
    })
}

// --- Auth-dialog process management ---

/// Run an auth-dialog binary synchronously (called from background thread).
///
/// Spawns the auth-dialog, writes connection data to its stdin,
/// reads the response from stdout, and waits for it to exit.
fn run_auth_dialog(
    auth_dialog_path: &str,
    args: &[String],
    stdin_data: &str,
    child_pid: &Arc<Mutex<Option<u32>>>,
    is_external_ui: bool,
) -> AuthDialogResult {
    // SAFETY: setpgid(0, 0) puts the child into its own process group so that
    // kill_child_process can send signals to the entire group (matching
    // nm-applet's vpn_child_setup). pre_exec runs after fork, before exec.
    let mut child = match unsafe {
        Command::new(auth_dialog_path)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env_remove("G_MESSAGES_DEBUG")
            .pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            })
            .spawn()
    } {
        Ok(c) => c,
        Err(e) => {
            return AuthDialogResult::Error(format!("Failed to spawn {}: {}", auth_dialog_path, e));
        }
    };

    // Store PID for cancel/timeout cleanup
    let pid = child.id();
    *child_pid.lock().unwrap() = Some(pid);

    debug!("NmSecretAgent: auth-dialog spawned (pid={})", pid);

    // Write stdin data and close stdin
    if let Some(mut stdin) = child.stdin.take()
        && let Err(e) = stdin.write_all(stdin_data.as_bytes())
    {
        warn!("NmSecretAgent: failed to write stdin to auth-dialog: {}", e);
        // Continue anyway — some dialogs might not need stdin
    }
    // stdin is dropped here, closing the pipe

    // Read all stdout
    let mut stdout_data = String::new();
    if let Some(mut stdout) = child.stdout.take()
        && let Err(e) = stdout.read_to_string(&mut stdout_data)
    {
        warn!(
            "NmSecretAgent: failed to read stdout from auth-dialog: {}",
            e
        );
    }

    // Wait for child to exit
    let exit_status = match child.wait() {
        Ok(status) => status,
        Err(e) => {
            return AuthDialogResult::Error(format!("Failed to wait for auth-dialog: {}", e));
        }
    };

    // Clear stored PID
    *child_pid.lock().unwrap() = None;

    debug!(
        "NmSecretAgent: auth-dialog exited with status={}, stdout={} bytes",
        exit_status,
        stdout_data.len()
    );

    if !exit_status.success() {
        return AuthDialogResult::Cancelled;
    }

    // Parse response based on mode
    if is_external_ui {
        match parse_external_ui_response(&stdout_data) {
            Some((_title, description, fields)) => AuthDialogResult::ExternalUi {
                description,
                fields,
            },
            None => {
                AuthDialogResult::Error("Failed to parse external-ui-mode response".to_string())
            }
        }
    } else {
        let secrets = parse_legacy_response(&stdout_data);
        AuthDialogResult::Secrets(secrets)
    }
}

/// Kill a child auth-dialog process (if running).
///
/// Uses process-group kill (`kill(-pid, ...)`) so that any subprocesses the
/// auth-dialog may have spawned are also cleaned up. The child is started with
/// `setpgid` so it leads its own process group.
fn kill_child_process(child_pid: &Option<Arc<Mutex<Option<u32>>>>) {
    if let Some(pid_lock) = child_pid
        && let Some(pid) = *pid_lock.lock().unwrap()
    {
        debug!(
            "NmSecretAgent: killing auth-dialog process group (pid={})",
            pid
        );
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
        // Give it a moment, then SIGKILL — but only if the PID is still
        // tracked (i.e., the process hasn't exited and been waited on).
        let pid_lock_clone = pid_lock.clone();
        glib::timeout_add_local_once(Duration::from_secs(2), move || {
            if let Some(stored_pid) = *pid_lock_clone.lock().unwrap()
                && stored_pid == pid
            {
                debug!(
                    "NmSecretAgent: SIGKILL auth-dialog process group (pid={})",
                    pid
                );
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
        });
    }
}

// --- Auth-dialog protocol helpers ---

/// Build the stdin payload for an auth-dialog binary.
///
/// Format:
/// ```text
/// DATA_KEY=<key>\n
/// DATA_VAL=<value>\n
/// ...
/// SECRET_KEY=<key>\n
/// SECRET_VAL=<value>\n
/// ...
/// DONE\n
/// \n
/// QUIT\n
/// \n
/// ```
fn build_auth_dialog_stdin(connection_dict: &Variant) -> String {
    let mut buf = String::new();

    if let Some(vpn_section) = get_dict_section(connection_dict, "vpn") {
        // Write DATA entries from vpn.data (a{ss} wrapped in variant)
        if let Some(data_dict) = get_variant_dict_value(&vpn_section, "data") {
            let data = extract_string_dict(&data_dict);
            for (key, value) in &data {
                buf.push_str("DATA_KEY=");
                buf.push_str(&sanitize_value(key));
                buf.push('\n');
                buf.push_str("DATA_VAL=");
                buf.push_str(&sanitize_value(value));
                buf.push('\n');
            }
        }

        // Write SECRET entries from vpn.secrets (a{sv} wrapped in variant)
        if let Some(secrets_dict) = get_variant_dict_value(&vpn_section, "secrets") {
            let secrets = extract_variant_dict_strings(&secrets_dict);
            for (key, value) in &secrets {
                buf.push_str("SECRET_KEY=");
                buf.push_str(&sanitize_value(key));
                buf.push('\n');
                buf.push_str("SECRET_VAL=");
                buf.push_str(&sanitize_value(value));
                buf.push('\n');
            }
        }
    }

    buf.push_str("DONE\n\nQUIT\n\n");
    buf
}

/// Replace newlines in a value with spaces (auth-dialog protocol requirement).
fn sanitize_value(s: &str) -> String {
    s.replace('\n', " ")
}

/// Parse legacy mode auth-dialog stdout response.
///
/// Format: alternating key/value lines, terminated by empty line.
/// ```text
/// password\n
/// hunter2\n
/// \n
/// ```
fn parse_legacy_response(stdout: &str) -> Vec<(String, String)> {
    let mut secrets = Vec::new();
    let lines: Vec<&str> = stdout.lines().collect();
    let mut i = 0;

    while i + 1 < lines.len() {
        let key = lines[i];
        if key.is_empty() {
            break;
        }
        let value = lines[i + 1];
        secrets.push((key.to_string(), value.to_string()));
        i += 2;
    }

    secrets
}

/// Parse external-ui-mode auth-dialog response (GKeyFile/INI format).
///
/// Returns `(title, description, fields)` or `None` if parsing fails.
///
/// Format:
/// ```ini
/// [VPN Plugin UI]
/// Version=2
/// Title=Authentication required
/// Description=Enter credentials...
///
/// [password]
/// Label=Password
/// Value=
/// IsSecret=true
/// ShouldAsk=true
/// ```
fn parse_external_ui_response(stdout: &str) -> Option<(String, String, Vec<ExternalUiField>)> {
    let sections = parse_ini(stdout);

    // Verify header section
    let header = sections
        .iter()
        .find(|(name, _)| name == "VPN Plugin UI")
        .map(|(_, v)| v)?;
    let version: u32 = header.get("Version")?.parse().ok()?;
    if version != 2 {
        warn!(
            "NmSecretAgent: unsupported external-ui version: {}",
            version
        );
        return None;
    }

    let title = header.get("Title").cloned().unwrap_or_default();
    let description = header.get("Description").cloned().unwrap_or_default();

    let mut fields = Vec::new();

    for (section_name, values) in &sections {
        if section_name == "VPN Plugin UI" {
            continue;
        }

        // Skip entries without Label
        let label = match values.get("Label") {
            Some(l) if !l.is_empty() => l.clone(),
            _ => continue,
        };

        let value = values.get("Value").cloned().unwrap_or_default();
        let is_secret = values
            .get("IsSecret")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let should_ask = values
            .get("ShouldAsk")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        fields.push(ExternalUiField {
            key: section_name.clone(),
            label,
            value,
            is_secret,
            should_ask,
        });
    }

    Some((title, description, fields))
}

/// Parse a simple INI-format string into sections.
///
/// Returns an ordered map of section_name -> key/value pairs.
/// Preserves section order (uses Vec internally).
fn parse_ini(text: &str) -> SectionMap {
    let mut sections = SectionMap::new();
    let mut current_section = String::new();

    for line in text.lines() {
        let trimmed = line.trim();

        // Skip empty lines and comments
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }

        // Section header
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            current_section = trimmed[1..trimmed.len() - 1].to_string();
            if !sections.iter().any(|(name, _)| name == &current_section) {
                sections.push((current_section.clone(), HashMap::new()));
            }
            continue;
        }

        // Key=Value pair
        if let Some(eq_pos) = trimmed.find('=') {
            let key = trimmed[..eq_pos].trim().to_string();
            let value = trimmed[eq_pos + 1..].trim().to_string();
            if !current_section.is_empty()
                && let Some((_, map)) = sections
                    .iter_mut()
                    .find(|(name, _)| name == &current_section)
            {
                map.insert(key, value);
            }
        }
    }

    sections
}

/// Section map for INI parsing. Uses Vec to preserve insertion order.
type SectionMap = Vec<(String, HashMap<String, String>)>;

// --- VPN plugin .name file parsing ---

/// Find a VPN plugin by its D-Bus service type.
///
/// Scans `.name` files in standard VPN plugin directories to find the plugin
/// matching the given service type, and extracts auth-dialog information.
///
/// Search order:
/// 1. `$NM_VPN_PLUGIN_DIR` (if set)
/// 2. `/usr/lib/NetworkManager/VPN/`
/// 3. `/etc/NetworkManager/VPN/` (legacy; NixOS symlinks here)
fn find_vpn_plugin(service_type: &str) -> Option<VpnPluginInfo> {
    // Collect owned env-var value (if any) so we can borrow it alongside the
    // static slices without leaking memory.
    let env_dir = std::env::var("NM_VPN_PLUGIN_DIR").ok();

    // Env override gets highest priority (matches NM's own behavior).
    let dirs: Vec<&str> = env_dir
        .as_deref()
        .into_iter()
        .chain(VPN_PLUGIN_DIRS.iter().copied())
        .collect();

    for plugin_dir in &dirs {
        let dir = match std::fs::read_dir(plugin_dir) {
            Ok(d) => d,
            Err(e) => {
                debug!(
                    "NmSecretAgent: cannot read VPN plugin dir {}: {}",
                    plugin_dir, e
                );
                continue;
            }
        };

        for entry in dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("name") {
                continue;
            }

            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let info = parse_vpn_plugin_name_file(&content);

            if info.service == service_type {
                debug!(
                    "NmSecretAgent: found plugin for '{}' in {:?} (auth-dialog={:?}, external-ui={})",
                    service_type, path, info.auth_dialog, info.supports_external_ui_mode
                );

                // Verify auth-dialog binary exists
                if let Some(ref dialog_path) = info.auth_dialog
                    && !std::path::Path::new(dialog_path).exists()
                {
                    debug!(
                        "NmSecretAgent: auth-dialog binary not found: {}",
                        dialog_path
                    );
                    return Some(VpnPluginInfo {
                        auth_dialog: None,
                        ..info
                    });
                }

                return Some(info);
            }
        }
    }

    debug!(
        "NmSecretAgent: no plugin found for service type '{}'",
        service_type
    );
    None
}

/// Parse a VPN plugin `.name` file.
fn parse_vpn_plugin_name_file(content: &str) -> VpnPluginInfo {
    let mut service = String::new();
    let mut auth_dialog = None;
    let mut supports_external_ui_mode = false;
    let mut supports_hints = false;

    let mut current_section = String::new();

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }

        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            current_section = trimmed[1..trimmed.len() - 1].to_string();
            continue;
        }

        if let Some(eq_pos) = trimmed.find('=') {
            let key = trimmed[..eq_pos].trim();
            let value = trimmed[eq_pos + 1..].trim();

            match current_section.as_str() {
                "VPN Connection" if key == "service" => {
                    service = value.to_string();
                }
                "GNOME" => match key {
                    "auth-dialog" => auth_dialog = Some(value.to_string()),
                    "supports-external-ui-mode" => {
                        supports_external_ui_mode = value.eq_ignore_ascii_case("true")
                    }
                    "supports-hints" => supports_hints = value.eq_ignore_ascii_case("true"),
                    _ => {}
                },
                _ => {}
            }
        }
    }

    VpnPluginInfo {
        service,
        auth_dialog,
        supports_external_ui_mode,
        supports_hints,
    }
}

// --- Variant helpers ---

/// Get a section from a settings dict (`a{sa{sv}}`).
///
/// The top-level connection dict is `a{sa{sv}}`, where each entry is
/// `{s, a{sv}}`. This returns the `a{sv}` for the matching section name.
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

/// Get a variant-wrapped sub-dict from an `a{sv}` dict.
///
/// In `a{sv}`, values are variant-wrapped. This looks up the key and unwraps
/// the variant to return the inner value (e.g., the `a{ss}` inside `v`).
/// Used for accessing nested dicts like `vpn.data` (which is `a{ss}` wrapped
/// in a variant `v` inside the vpn section's `a{sv}`).
fn get_variant_dict_value(dict: &Variant, key: &str) -> Option<Variant> {
    let n = dict.n_children();
    for i in 0..n {
        let entry = dict.child_value(i);
        let entry_key = entry.child_value(0);
        if entry_key.str() == Some(key) {
            let value = entry.child_value(1);
            // Unwrap the variant wrapper: v -> inner value
            if value.n_children() > 0 {
                return Some(value.child_value(0));
            }
            return Some(value);
        }
    }
    None
}

/// Get a string value from a dict (`a{sv}`).
fn get_string_from_dict(dict: &Variant, key: &str) -> Option<String> {
    let n = dict.n_children();
    for i in 0..n {
        let entry = dict.child_value(i);
        let entry_key = entry.child_value(0);
        if entry_key.str() == Some(key) {
            let value = entry.child_value(1);
            // Value is a variant, unwrap it
            let inner = value.child_value(0);
            return inner.str().map(|s| s.to_string());
        }
    }
    None
}

/// Get a `u32` value from a dict (`a{sv}`).
fn get_u32_from_dict(dict: &Variant, key: &str) -> Option<u32> {
    get_variant_dict_value(dict, key)?.get::<u32>()
}

/// Extract all key/value pairs from a string dict (`a{ss}`).
///
/// Used to build the DATA section of the auth-dialog stdin payload.
fn extract_string_dict(dict: &Variant) -> Vec<(String, String)> {
    let mut result = Vec::new();
    let n = dict.n_children();
    for i in 0..n {
        let entry = dict.child_value(i);
        let key = entry.child_value(0);
        let value = entry.child_value(1);

        let key_str = key.str().map(|s| s.to_string()).or_else(|| {
            if key.n_children() > 0 {
                key.child_value(0).str().map(|s| s.to_string())
            } else {
                None
            }
        });
        let value_str = value.str().map(|s| s.to_string()).or_else(|| {
            if value.n_children() > 0 {
                value.child_value(0).str().map(|s| s.to_string())
            } else {
                None
            }
        });

        if let (Some(k), Some(v)) = (key_str, value_str) {
            result.push((k, v));
        }
    }
    result
}

/// Extract string values from a variant dict (`a{sv}`).
///
/// Used to build the SECRET section of the auth-dialog stdin payload.
fn extract_variant_dict_strings(dict: &Variant) -> Vec<(String, String)> {
    let mut result = Vec::new();
    let n = dict.n_children();
    for i in 0..n {
        let entry = dict.child_value(i);
        let key = entry.child_value(0);
        let value = entry.child_value(1);

        let key_str = key.str().map(|s| s.to_string());

        let value_str = if value.is_container() && value.n_children() > 0 {
            value.child_value(0).str().map(|s| s.to_string())
        } else {
            value.str().map(|s| s.to_string())
        };

        if let (Some(k), Some(v)) = (key_str, value_str) {
            result.push((k, v));
        }
    }
    result
}

/// Convert a secret key name to a human-readable label.
fn humanize_secret_key(key: &str) -> String {
    match key {
        "password" => "Password".to_string(),
        "cert-pass" => "Certificate password".to_string(),
        "http-proxy-password" => "HTTP proxy password".to_string(),
        "Xauth password" => "Password".to_string(),
        other => {
            // Title-case the key: "my-secret-key" -> "My secret key"
            let mut result = other.replace(['-', '_'], " ");
            if let Some(first) = result.get_mut(..1) {
                first.make_ascii_uppercase();
            }
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection(security: Option<Vec<(&str, Variant)>>) -> Variant {
        let mut settings: HashMap<String, HashMap<String, Variant>> = HashMap::from([(
            "802-11-wireless".to_string(),
            HashMap::from([("ssid".to_string(), b"cafe".to_vec().to_variant())]),
        )]);
        if let Some(security) = security {
            settings.insert(
                WIFI_SECURITY_SETTING.to_string(),
                security
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
            );
        }
        settings.to_variant()
    }

    #[test]
    fn wifi_spec_picks_secret_and_offers_saving_only_agent_owned_secrets() {
        let psk = |key_mgmt: &str, flags: Option<u32>| {
            let mut security = vec![("key-mgmt", key_mgmt.to_variant())];
            if let Some(flags) = flags {
                security.push(("psk-flags", flags.to_variant()));
            }
            security
        };
        let cases = [
            (
                "psk without flags",
                psk("wpa-psk", None),
                "psk",
                "psk-flags",
                false,
            ),
            (
                "sae system-owned",
                psk("sae", Some(0)),
                "psk",
                "psk-flags",
                false,
            ),
            (
                "psk agent-owned",
                psk("wpa-psk", Some(SECRET_FLAG_AGENT_OWNED)),
                "psk",
                "psk-flags",
                true,
            ),
            (
                "psk not saved",
                psk("wpa-psk", Some(SECRET_FLAG_NOT_SAVED)),
                "psk",
                "psk-flags",
                false,
            ),
            (
                "psk agent-owned but not saved",
                psk(
                    "wpa-psk",
                    Some(SECRET_FLAG_AGENT_OWNED | SECRET_FLAG_NOT_SAVED),
                ),
                "psk",
                "psk-flags",
                false,
            ),
            (
                "wep uses the transmit key index",
                vec![
                    ("key-mgmt", "none".to_variant()),
                    ("wep-tx-keyidx", 2u32.to_variant()),
                    ("wep-key-flags", SECRET_FLAG_AGENT_OWNED.to_variant()),
                ],
                "wep-key2",
                "wep-key-flags",
                false,
            ),
        ];

        for (name, security, secret_key, flags_key, offer_save) in cases {
            let spec = wifi_secret_spec(&connection(Some(security))).unwrap();
            assert_eq!(
                spec,
                WifiSecretSpec {
                    ssid: "cafe".into(),
                    secret_key: secret_key.into(),
                    flags_key: flags_key.into(),
                    offer_save,
                },
                "{name}"
            );
        }
    }

    #[test]
    fn wifi_spec_declines_what_it_cannot_prompt_for() {
        for security in [
            None,
            Some(vec![("key-mgmt", "wpa-eap".to_variant())]),
            Some(vec![("key-mgmt", "owe".to_variant())]),
            Some(vec![
                ("key-mgmt", "none".to_variant()),
                ("auth-alg", "leap".to_variant()),
            ]),
            Some(vec![
                ("key-mgmt", "wpa-psk".to_variant()),
                ("psk-flags", SECRET_FLAG_NOT_REQUIRED.to_variant()),
            ]),
        ] {
            assert!(
                wifi_secret_spec(&connection(security.clone())).is_err(),
                "{security:?}"
            );
        }

        let no_ssid: HashMap<String, HashMap<String, Variant>> = HashMap::from([(
            WIFI_SECURITY_SETTING.to_string(),
            HashMap::from([("key-mgmt".to_string(), "wpa-psk".to_variant())]),
        )]);
        assert!(wifi_secret_spec(&no_ssid.to_variant()).is_err());
    }
}
