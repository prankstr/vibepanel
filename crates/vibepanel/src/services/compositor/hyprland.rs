//! Hyprland compositor backend using native socket IPC.
//!
//! This backend communicates with Hyprland via its Unix sockets:
//! - `.socket.sock` for commands/queries (JSON responses)
//! - `.socket2.sock` for event subscription
//!
//! Reference: https://wiki.hyprland.org/IPC/

use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use parking_lot::RwLock;
use serde_json::Value;
use tracing::{debug, error, info, trace, warn};

use super::visibility::hyprland_workspace_key;
use super::{
    CompositorBackend, KeyboardLayoutCallback, KeyboardLayoutInfo, Window, WindowCallback,
    WindowInfo, WindowListCallback, WindowListSnapshot, WorkspaceCallback, WorkspaceMeta,
    WorkspaceSnapshot, synthetic_id_for_name,
};

/// Default workspaces for Hyprland (dynamic workspaces, but we expose 1-10).
const DEFAULT_WORKSPACE_COUNT: i32 = 10;

const RECONNECT_INITIAL_MS: u64 = 1000;
const RECONNECT_MAX_MS: u64 = 30000;
const RECONNECT_MULTIPLIER: f64 = 1.5;
const POINTER_FOCUS_REFRESH_DELAY: Duration = Duration::from_millis(50);

/// State written by the event thread and read by the manager-side backend.
struct SharedState {
    workspace_snapshot: RwLock<WorkspaceSnapshot>,
    focused_window: RwLock<Option<WindowInfo>>,
    workspaces: RwLock<Vec<WorkspaceMeta>>,
    /// Named workspace ID → Hyprland address, used for switching.
    named_addresses: RwLock<HashMap<i32, String>>,
    focused_monitor: RwLock<Option<String>>,
    /// Hyprland monitor ID → connector name, used to resolve window outputs.
    monitor_names: RwLock<HashMap<i64, String>>,
    /// Current keyboard layout info.
    keyboard_layout: RwLock<Option<KeyboardLayoutInfo>>,
    /// Name of the main keyboard device (auto-detected from `hyprctl devices`).
    main_keyboard_name: RwLock<Option<String>>,
    /// Shown special workspaces, as in `HyprlandState`.
    shown_special: RwLock<HashMap<String, (String, Option<i32>)>>,
    /// Listed windows, in taskbar order.
    windows: RwLock<Vec<Window>>,
    /// Window state that `j/clients` doesn't report.
    tracking: RwLock<WindowTracking>,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            workspace_snapshot: RwLock::new(WorkspaceSnapshot::default()),
            focused_window: RwLock::new(None),
            workspaces: RwLock::new(HyprlandBackend::default_workspaces()),
            named_addresses: RwLock::new(HashMap::new()),
            focused_monitor: RwLock::new(None),
            monitor_names: RwLock::new(HashMap::new()),
            keyboard_layout: RwLock::new(None),
            main_keyboard_name: RwLock::new(None),
            shown_special: RwLock::new(HashMap::new()),
            windows: RwLock::new(Vec::new()),
            tracking: RwLock::new(WindowTracking::default()),
        }
    }
}

/// Window state kept across `j/clients` queries, by window address.
#[derive(Debug, Default)]
struct WindowTracking {
    /// Last position outside fullscreen, which moves a window to the corner.
    positions: HashMap<u64, (i64, i64)>,
    /// Hyprland only reports urgency as an event; cleared on focus.
    urgent: HashSet<u64>,
    focused: Option<u64>,
}

/// What a handled event or refresh changed.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct EventChanges {
    workspaces: bool,
    window: bool,
    window_list: bool,
    keyboard_layout: bool,
}

impl std::ops::BitOrAssign for EventChanges {
    fn bitor_assign(&mut self, other: Self) {
        self.workspaces |= other.workspaces;
        self.window |= other.window;
        self.window_list |= other.window_list;
        self.keyboard_layout |= other.keyboard_layout;
    }
}

/// A regular workspace parsed from Hyprland IPC.
#[derive(Debug, PartialEq)]
struct WorkspaceRef {
    id: i32,
    /// Display name.
    name: String,
    /// Stable identity used to address the workspace in dispatches.
    address: String,
}

/// Workspace state rebuilt from one `monitors` and `workspaces` query pair.
#[derive(Debug, Default)]
struct HyprlandState {
    snapshot: WorkspaceSnapshot,
    workspaces: Vec<WorkspaceMeta>,
    named_addresses: HashMap<i32, String>,
    focused_monitor: Option<String>,
    monitor_names: HashMap<i64, String>,
    /// Shown special workspaces, by `hyprland_workspace_key`, with the
    /// monitor showing each and that monitor's regular active workspace.
    shown_special: HashMap<String, (String, Option<i32>)>,
}

impl HyprlandState {
    fn window_context(&self) -> WindowContext<'_> {
        WindowContext {
            monitor_names: &self.monitor_names,
            shown_special: &self.shown_special,
            workspaces: &self.workspaces,
        }
    }
}

/// Monitor and workspace state needed to build the window list.
struct WindowContext<'a> {
    monitor_names: &'a HashMap<i64, String>,
    shown_special: &'a HashMap<String, (String, Option<i32>)>,
    workspaces: &'a [WorkspaceMeta],
}

/// Windows whose left edges are closer than this (px) are in one column, so
/// borders and gaps don't split a column.
const COLUMN_TOLERANCE: i64 = 48;

pub struct HyprlandBackend {
    running: Arc<AtomicBool>,
    event_thread: Mutex<Option<JoinHandle<()>>>,
    socket_path: RwLock<Option<String>>,
    event_socket_path: RwLock<Option<String>>,
    shared: Arc<SharedState>,
    callbacks: Mutex<Option<(WorkspaceCallback, WindowCallback)>>,
    /// Callback for keyboard layout changes, set by CompositorManager.
    keyboard_layout_callback: Mutex<Option<KeyboardLayoutCallback>>,
    /// Callback for window list changes, set by CompositorManager.
    window_list_callback: Mutex<Option<WindowListCallback>>,
    /// Whether this Hyprland instance supports the Lua dispatch API.
    supports_lua_dispatch: AtomicBool,
}

impl HyprlandBackend {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            event_thread: Mutex::new(None),
            socket_path: RwLock::new(None),
            event_socket_path: RwLock::new(None),
            shared: Arc::new(SharedState::default()),
            callbacks: Mutex::new(None),
            keyboard_layout_callback: Mutex::new(None),
            window_list_callback: Mutex::new(None),
            supports_lua_dispatch: AtomicBool::new(false),
        }
    }

    /// Create the event-thread instance. It shares `running` and all state
    /// with `self`; only the socket paths, callbacks and the
    /// `supports_lua_dispatch` flag are copied.
    fn event_thread_instance(&self) -> Self {
        Self {
            running: Arc::clone(&self.running),
            event_thread: Mutex::new(None),
            socket_path: RwLock::new(self.socket_path.read().clone()),
            event_socket_path: RwLock::new(self.event_socket_path.read().clone()),
            shared: Arc::clone(&self.shared),
            callbacks: Mutex::new(
                self.callbacks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            ),
            keyboard_layout_callback: Mutex::new(
                self.keyboard_layout_callback
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            ),
            window_list_callback: Mutex::new(
                self.window_list_callback
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            ),
            supports_lua_dispatch: AtomicBool::new(
                self.supports_lua_dispatch.load(Ordering::Relaxed),
            ),
        }
    }

    /// Resolve socket paths from environment.
    fn resolve_socket_paths(&self) -> bool {
        let signature = match env::var("HYPRLAND_INSTANCE_SIGNATURE") {
            Ok(s) => s,
            Err(_) => {
                warn!("HYPRLAND_INSTANCE_SIGNATURE not set");
                return false;
            }
        };

        let runtime_dir = env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| format!("/run/user/{}", std::process::id()));

        let base_path = format!("{}/hypr/{}", runtime_dir, signature);
        let socket_path = format!("{}/.socket.sock", base_path);
        let event_socket_path = format!("{}/.socket2.sock", base_path);

        *self.socket_path.write() = Some(socket_path);
        *self.event_socket_path.write() = Some(event_socket_path);

        true
    }

    /// Disable Hyprland's compositor-level layer animations for popovers and toasts.
    ///
    /// Hyprland animates layer surface resize/move by default which clashes
    /// with GTK4 animations and causes a visible content-shift glitch.
    ///
    /// Session-scoped, only set on startup and not persisted anywhere
    fn apply_layer_rules(&self) {
        let cmd = if self.supports_lua_dispatch.load(Ordering::Relaxed) {
            "eval hl.layer_rule({ match = { namespace = \"^vibepanel-(.*-popover|toast)$\" }, no_anim = true })"
        } else {
            "keyword layerrule no_anim on, match:namespace ^vibepanel-(.*-popover|toast)$"
        };
        match self.send_command(cmd) {
            Some(response) if Self::response_is_ok(&response) => {
                info!("Applied Hyprland layerrule: no_anim for vibepanel surfaces");
            }
            Some(response) => {
                warn!("Hyprland layerrule response: {}", response.trim());
            }
            None => {
                warn!("Failed to apply Hyprland no_anim layerrule");
            }
        }
    }

    fn probe_lua_dispatch_support(&self) {
        // Hyprland 0.55 documents hl.dsp.no_op() as a dispatcher that does nothing.
        // It is a safe capability probe for the new Lua dispatch API.
        match self.send_command("dispatch hl.dsp.no_op()") {
            Some(response) if Self::response_is_ok(&response) => {
                self.supports_lua_dispatch.store(true, Ordering::Relaxed);
                info!("Hyprland Lua dispatch API detected");
            }
            Some(response) => {
                self.supports_lua_dispatch.store(false, Ordering::Relaxed);
                debug!(
                    response = %response.trim(),
                    "Hyprland Lua dispatch API unavailable; using legacy dispatch commands"
                );
            }
            None => {
                self.supports_lua_dispatch.store(false, Ordering::Relaxed);
                warn!("Failed to probe Hyprland Lua dispatch API; using legacy dispatch commands");
            }
        }
    }

    /// Send a command to Hyprland and get the response.
    fn send_command(&self, command: &str) -> Option<String> {
        let socket_path = self.socket_path.read();
        let socket_path = socket_path.as_ref()?;

        Self::send_command_to_socket(socket_path, command)
    }

    fn send_command_to_socket(socket_path: &str, command: &str) -> Option<String> {
        let mut stream = match UnixStream::connect(socket_path) {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to connect to Hyprland socket: {}", e);
                return None;
            }
        };

        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

        if let Err(e) = stream.write_all(command.as_bytes()) {
            error!("Failed to send command to Hyprland: {}", e);
            return None;
        }

        let mut response = Vec::new();
        if let Err(e) = stream.read_to_end(&mut response) {
            error!("Failed to read Hyprland response: {}", e);
            return None;
        }

        String::from_utf8(response).ok()
    }

    /// Query Hyprland with a JSON command.
    fn query_json(&self, command: &str) -> Option<Value> {
        let response = self.send_command(&format!("j/{}", command))?;
        match serde_json::from_str(&response) {
            Ok(v) => Some(v),
            Err(e) => {
                trace!("Failed to parse JSON from Hyprland: {}", e);
                None
            }
        }
    }

    fn default_workspaces() -> Vec<WorkspaceMeta> {
        (1..=DEFAULT_WORKSPACE_COUNT)
            .map(|i| WorkspaceMeta {
                id: i,
                idx: i,
                name: i.to_string(),
                output: None, // Hyprland workspaces are globally identified.
            })
            .collect()
    }

    /// Parse a workspace object from `monitors`, `clients` or `workspaces`.
    ///
    /// Hyprland 0.56 and older send `{id, name}`, with negative IDs for named
    /// and special workspaces. Newer versions send `{address, type, name}`
    /// and include `id` only for numbered workspaces; `name` is then only the
    /// display name and `address` the stable identity.
    ///
    /// Numbered workspaces keep their number as ID. Named workspaces keep
    /// Hyprland's negative ID on older versions; newer versions have none, so
    /// they get a hash of their address, which survives renames. Special
    /// workspaces are skipped.
    fn parse_workspace(workspace: &Value) -> Option<WorkspaceRef> {
        let name = workspace.get("name").and_then(Value::as_str).unwrap_or("");
        let id = workspace
            .get("id")
            .and_then(Value::as_i64)
            .and_then(|id| i32::try_from(id).ok());

        if let Some(id) = id.filter(|id| *id > 0) {
            let name = if name.is_empty() {
                id.to_string()
            } else {
                name.to_string()
            };
            return Some(WorkspaceRef {
                id,
                name,
                address: id.to_string(),
            });
        }

        if let Some(address) = workspace.get("address").and_then(Value::as_str) {
            if address.is_empty()
                || workspace.get("type").and_then(Value::as_str) == Some("special")
            {
                return None;
            }
            return Some(WorkspaceRef {
                id: synthetic_id_for_name(address),
                name: if name.is_empty() { address } else { name }.to_string(),
                address: address.to_string(),
            });
        }

        // Older Hyprland: 0 and -1 mean no workspace, and special workspaces
        // are only recognisable by name.
        let id = id.filter(|id| *id < -1)?;
        if name.starts_with("special:") {
            return None;
        }
        // Hyprland IPC normally returns bare names (`web`). Accept the
        // dispatcher form (`name:web`) defensively.
        let name = name.strip_prefix("name:").unwrap_or(name);
        if name.is_empty() {
            return None;
        }
        Some(WorkspaceRef {
            id,
            name: name.to_string(),
            address: name.to_string(),
        })
    }

    fn workspace_id(workspace: &Value) -> Option<i32> {
        Self::parse_workspace(workspace).map(|ws| ws.id)
    }

    /// Merge existing workspaces with the default 1-10, numbered first by
    /// number, then named ones in creation order.
    fn workspace_meta(workspaces: &[(WorkspaceRef, i64)]) -> Vec<WorkspaceMeta> {
        let meta = |ws: &WorkspaceRef| WorkspaceMeta {
            id: ws.id,
            idx: if ws.id > 0 { ws.id } else { -1 },
            name: ws.name.clone(),
            output: None,
        };
        let mut numbered: BTreeMap<i32, WorkspaceMeta> = Self::default_workspaces()
            .into_iter()
            .map(|ws| (ws.id, ws))
            .collect();
        let mut named = Vec::new();
        for (ws, order) in workspaces {
            if ws.id > 0 {
                numbered.insert(ws.id, meta(ws));
            } else {
                named.push((*order, ws));
            }
        }
        named.sort_by_key(|(order, _)| *order);

        numbered
            .into_values()
            .chain(named.into_iter().map(|(_, ws)| meta(ws)))
            .collect()
    }

    fn workspace_switch_target(&self, workspace_id: i32) -> String {
        if workspace_id < 0 {
            if let Some(address) = self.shared.named_addresses.read().get(&workspace_id) {
                if address.chars().any(char::is_whitespace) {
                    warn!(
                        "Hyprland named workspace {:?} contains whitespace; switching may fail \
                         if Hyprland does not accept the raw name in workspace dispatch",
                        address
                    );
                }
                return format!("name:{address}");
            }
            warn!("No Hyprland workspace found for named workspace id {workspace_id}");
        }

        workspace_id.to_string()
    }

    fn lua_string(value: &str) -> String {
        let mut escaped = String::with_capacity(value.len() + 2);
        escaped.push('"');
        for ch in value.chars() {
            match ch {
                '\\' => escaped.push_str("\\\\"),
                '"' => escaped.push_str("\\\""),
                '\n' => escaped.push_str("\\n"),
                '\r' => escaped.push_str("\\r"),
                '\t' => escaped.push_str("\\t"),
                _ => escaped.push(ch),
            }
        }
        escaped.push('"');
        escaped
    }

    fn workspace_lua_target(&self, workspace_id: i32) -> String {
        let target = self.workspace_switch_target(workspace_id);
        if target.starts_with("name:") {
            Self::lua_string(&target)
        } else {
            target
        }
    }

    fn response_is_ok(response: &str) -> bool {
        response.trim().eq_ignore_ascii_case("ok")
    }

    fn parse_cursorpos(response: &str) -> Option<(i32, i32)> {
        let (x, y) = response.trim().split_once(',')?;
        Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
    }

    fn send_dispatch(&self, context: &str, lua: &str, legacy: &str) {
        // TODO: Remove the legacy hyprlang dispatch path once Hyprland drops hyprlang support.
        let command = if self.supports_lua_dispatch.load(Ordering::Relaxed) {
            lua
        } else {
            legacy
        };

        match self.send_command(command) {
            Some(response) if Self::response_is_ok(&response) => {
                debug!(context, command, "Hyprland command succeeded");
            }
            Some(response) => {
                warn!(
                    context,
                    command,
                    response = %response.trim(),
                    "Hyprland command failed"
                );
            }
            None => {
                warn!(
                    context,
                    command, "Hyprland command failed without a response"
                );
            }
        }
    }

    fn refresh_pointer_focus_impl(&self) {
        // Workaround for Hyprland: after layer-shell popovers map, Hyprland
        // does not recalculate pointer hover/focus until it processes cursor
        // activity. Delay the no-op move until the newly mapped surface has
        // been committed. A user cursor move inside this short window can be
        // pulled back to the sampled position; avoiding that race would add IPC
        // complexity for a very small, transient case.
        let socket_path = self.socket_path.read().clone();
        let use_lua = self.supports_lua_dispatch.load(Ordering::Relaxed);
        thread::spawn(move || {
            thread::sleep(POINTER_FOCUS_REFRESH_DELAY);
            let Some(socket_path) = socket_path else {
                return;
            };

            let Some(cursorpos) = Self::send_command_to_socket(&socket_path, "cursorpos") else {
                debug!("Hyprland cursor position unavailable for pointer focus refresh");
                return;
            };
            let Some((x, y)) = Self::parse_cursorpos(&cursorpos) else {
                debug!(
                    response = %cursorpos.trim(),
                    "Hyprland cursor position parse failed for pointer focus refresh"
                );
                return;
            };

            let command = if use_lua {
                format!("dispatch hl.dsp.cursor.move({{ x = {x}, y = {y} }})")
            } else {
                format!("dispatch movecursor {x} {y}")
            };

            match Self::send_command_to_socket(&socket_path, &command) {
                Some(response) if Self::response_is_ok(&response) => {
                    debug!(command, "Hyprland deferred pointer focus refresh succeeded");
                }
                Some(response) => {
                    warn!(
                        command,
                        response = %response.trim(),
                        "Hyprland deferred pointer focus refresh failed"
                    );
                }
                None => {
                    warn!(
                        command,
                        "Hyprland deferred pointer focus refresh failed without a response"
                    );
                }
            }
        });
    }

    /// Build workspace state from `j/monitors` and `j/workspaces`.
    ///
    /// Hyprland's JSON has no urgency, so the urgent set is left empty and
    /// filled in by `apply_state` from the urgent windows.
    fn build_state(monitors: &[Value], workspaces: &[Value]) -> HyprlandState {
        let mut state = HyprlandState::default();
        let snapshot = &mut state.snapshot;

        for mon in monitors {
            let Some(name) = mon.get("name").and_then(Value::as_str) else {
                continue;
            };
            if let Some(id) = mon.get("id").and_then(Value::as_i64) {
                state.monitor_names.insert(id, name.to_string());
            }
            let focused = mon.get("focused").and_then(Value::as_bool).unwrap_or(false);
            if focused {
                state.focused_monitor = Some(name.to_string());
            }
            let per_output = snapshot.per_output.entry(name.to_string()).or_default();
            let active = mon.get("activeWorkspace").and_then(Self::workspace_id);
            if let Some(ws_id) = active {
                per_output.active_workspace.insert(ws_id);
                if focused {
                    snapshot.active_workspace.insert(ws_id);
                }
            }
            if let Some(special) = mon.get("specialWorkspace").and_then(hyprland_workspace_key) {
                state
                    .shown_special
                    .insert(special, (name.to_string(), active));
            }
        }

        let mut refs = Vec::with_capacity(workspaces.len());
        for (position, ws) in workspaces.iter().enumerate() {
            let Some(ws_ref) = Self::parse_workspace(ws) else {
                continue;
            };
            let id = ws_ref.id;
            if id < 0 {
                state.named_addresses.insert(id, ws_ref.address.clone());
            }
            // Older Hyprland counts named workspace IDs down from -1337, while
            // newer Hyprland lists workspaces in creation order.
            let order = if ws.get("address").and_then(Value::as_str).is_some() {
                position as i64
            } else {
                -i64::from(id)
            };
            refs.push((ws_ref, order));

            let Some(windows) = ws.get("windows").and_then(Value::as_u64) else {
                continue;
            };
            let windows = windows as u32;
            snapshot.window_counts.insert(id, windows);
            if windows > 0 {
                snapshot.occupied_workspaces.insert(id);
            }
            if let Some(mon_name) = ws.get("monitor").and_then(Value::as_str) {
                let per_output = snapshot.per_output.entry(mon_name.to_string()).or_default();
                per_output.window_counts.insert(id, windows);
                if windows > 0 {
                    per_output.occupied_workspaces.insert(id);
                }
            }
        }

        state.workspaces = Self::workspace_meta(&refs);
        state
    }

    /// Parse a window address, as `j/clients` (`0x55d0c0ffee`) or events
    /// (`55d0c0ffee`) report it.
    fn parse_address(address: &str) -> Option<u64> {
        let hex = address.strip_prefix("0x").unwrap_or(address);
        u64::from_str_radix(hex, 16).ok().filter(|id| *id != 0)
    }

    fn client_address(client: &Value) -> Option<u64> {
        client
            .get("address")
            .and_then(Value::as_str)
            .and_then(Self::parse_address)
    }

    /// Build the window list from `j/clients`, by output and workspace like
    /// the workspaces widget, then by position: Hyprland exposes no layout
    /// order, and `j/clients` is in stacking order.
    ///
    /// Windows on a shown special workspace are listed as scratchpads on the
    /// workspace they cover; hidden special, unmapped and swallowed windows
    /// are skipped.
    fn build_windows(
        clients: &[Value],
        context: &WindowContext,
        tracking: &mut WindowTracking,
    ) -> Vec<Window> {
        let addresses: HashSet<u64> = clients.iter().filter_map(Self::client_address).collect();
        tracking.positions.retain(|id, _| addresses.contains(id));
        tracking.urgent.retain(|id| addresses.contains(id));

        let swallowed: HashSet<u64> = clients
            .iter()
            .filter_map(|c| c.get("swallowing").and_then(Value::as_str))
            .filter_map(Self::parse_address)
            .collect();

        let mut entries: Vec<(Window, (i64, i64), usize)> = Vec::new();
        for client in clients {
            let Some(id) = Self::client_address(client) else {
                continue;
            };
            if client.get("mapped").and_then(Value::as_bool) == Some(false)
                || swallowed.contains(&id)
            {
                continue;
            }

            let workspace = client.get("workspace").unwrap_or(&Value::Null);
            let monitor = client
                .get("monitor")
                .and_then(Value::as_i64)
                .and_then(|id| context.monitor_names.get(&id).cloned());
            let (workspace_id, output, is_scratchpad) =
                if let Some(ws_id) = Self::workspace_id(workspace) {
                    (Some(ws_id), monitor, false)
                } else if let Some((shown_on, active)) = hyprland_workspace_key(workspace)
                    .and_then(|key| context.shown_special.get(&key))
                {
                    (*active, Some(shown_on.clone()), true)
                } else {
                    continue;
                };

            let at = client
                .get("at")
                .and_then(Value::as_array)
                .and_then(|at| Some((at.first()?.as_i64()?, at.get(1)?.as_i64()?)))
                .unwrap_or((i64::MAX, i64::MAX));
            // Newer Hyprland reports a mode (0 = none), older a bool.
            let fullscreen = client.get("fullscreen").is_some_and(|f| {
                f.as_u64().is_some_and(|mode| mode > 0) || f.as_bool() == Some(true)
            });
            let position = if fullscreen {
                tracking.positions.get(&id).copied().unwrap_or(at)
            } else {
                tracking.positions.insert(id, at);
                at
            };
            // Tabs share a position.
            let tab = client
                .get("grouped")
                .and_then(Value::as_array)
                .and_then(|group| {
                    group.iter().position(|member| {
                        member.as_str().and_then(Self::parse_address) == Some(id)
                    })
                })
                .unwrap_or(0);

            let str_field = |key: &str| {
                client
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };

            let window = Window {
                id,
                title: str_field("title"),
                app_id: str_field("class"),
                workspace_id,
                output,
                is_focused: tracking.focused == Some(id),
                is_urgent: tracking.urgent.contains(&id),
                is_scratchpad,
                ..Default::default()
            };
            entries.push((window, position, tab));
        }

        // Column left edges, per group of windows sorted together.
        type Group = (Option<String>, Option<i32>, bool);
        let group_of =
            |win: &Window| -> Group { (win.output.clone(), win.workspace_id, win.is_scratchpad) };
        let mut columns: HashMap<Group, Vec<i64>> = HashMap::new();
        for (win, (x, _), _) in &entries {
            columns.entry(group_of(win)).or_default().push(*x);
        }
        for xs in columns.values_mut() {
            xs.sort_unstable();
            let mut starts: Vec<i64> = Vec::new();
            for &x in xs.iter() {
                if starts
                    .last()
                    .is_none_or(|start| x - start >= COLUMN_TOLERANCE)
                {
                    starts.push(x);
                }
            }
            *xs = starts;
        }
        let column_of =
            |win: &Window, x: i64| columns[&group_of(win)].partition_point(|start| *start <= x);

        let ws_position: HashMap<i32, usize> = context
            .workspaces
            .iter()
            .enumerate()
            .map(|(pos, ws)| (ws.id, pos))
            .collect();
        entries.sort_by_cached_key(|(win, (x, y), tab)| {
            (
                win.output.clone(),
                win.workspace_id
                    .and_then(|id| ws_position.get(&id).copied())
                    .unwrap_or(usize::MAX),
                win.is_scratchpad,
                column_of(win, *x),
                *y,
                *tab,
                win.id,
            )
        });
        entries.into_iter().map(|(win, _, _)| win).collect()
    }

    /// Scratchpads don't count: they're only shown over a workspace.
    fn urgent_workspaces(windows: &[Window]) -> HashSet<i32> {
        windows
            .iter()
            .filter(|win| win.is_urgent && !win.is_scratchpad)
            .filter_map(|win| win.workspace_id)
            .collect()
    }

    fn sync_urgent_workspaces(&self) -> bool {
        let urgent = Self::urgent_workspaces(&self.shared.windows.read());
        let mut snapshot = self.shared.workspace_snapshot.write();
        let changed = snapshot.urgent_workspaces != urgent;
        snapshot.urgent_workspaces = urgent;
        changed
    }

    /// Reapply tracked focus and urgency to the listed windows.
    fn sync_window_flags(&self) -> bool {
        let tracking = self.shared.tracking.read();
        let mut changed = false;
        for win in self.shared.windows.write().iter_mut() {
            let is_focused = tracking.focused == Some(win.id);
            let is_urgent = tracking.urgent.contains(&win.id);
            changed |= win.is_focused != is_focused || win.is_urgent != is_urgent;
            win.is_focused = is_focused;
            win.is_urgent = is_urgent;
        }
        changed
    }

    fn apply_windows(&self, windows: Vec<Window>) -> bool {
        let mut current = self.shared.windows.write();
        let changed = *current != windows;
        *current = windows;
        changed
    }

    /// Store a rebuilt state. Returns true if the workspace snapshot or
    /// metadata changed.
    fn apply_state(&self, state: HyprlandState) -> bool {
        let HyprlandState {
            mut snapshot,
            workspaces,
            named_addresses,
            focused_monitor,
            monitor_names,
            shown_special,
        } = state;
        *self.shared.named_addresses.write() = named_addresses;
        *self.shared.focused_monitor.write() = focused_monitor;
        *self.shared.monitor_names.write() = monitor_names;
        *self.shared.shown_special.write() = shown_special;

        let metadata_changed = {
            let mut current = self.shared.workspaces.write();
            let changed = *current != workspaces;
            if changed {
                *current = workspaces;
            }
            changed
        };

        snapshot.urgent_workspaces = Self::urgent_workspaces(&self.shared.windows.read());
        let mut current = self.shared.workspace_snapshot.write();
        let snapshot_changed = *current != snapshot;
        if snapshot_changed {
            *current = snapshot;
        }

        metadata_changed || snapshot_changed
    }

    /// Re-query monitors, workspaces and clients and store the result.
    fn refresh(&self) -> EventChanges {
        let monitors = self.query_json("monitors");
        let workspaces = self.query_json("workspaces");
        let clients = self.query_json("clients");
        let (Some(monitors), Some(workspaces)) = (
            monitors.as_ref().and_then(Value::as_array),
            workspaces.as_ref().and_then(Value::as_array),
        ) else {
            warn!("Failed to query Hyprland monitors or workspaces");
            return EventChanges::default();
        };
        let state = Self::build_state(monitors, workspaces);

        // Before `apply_state`, which takes urgency from the window list.
        let window_list = match clients.as_ref().and_then(Value::as_array) {
            Some(clients) => {
                let windows = Self::build_windows(
                    clients,
                    &state.window_context(),
                    &mut self.shared.tracking.write(),
                );
                self.apply_windows(windows)
            }
            None => {
                warn!("Failed to query Hyprland clients");
                false
            }
        };

        EventChanges {
            workspaces: self.apply_state(state),
            window_list,
            ..Default::default()
        }
    }

    /// Re-query only the clients, reusing the last monitor and workspace state.
    fn refresh_windows(&self) -> EventChanges {
        let clients = self.query_json("clients");
        let Some(clients) = clients.as_ref().and_then(Value::as_array) else {
            warn!("Failed to query Hyprland clients");
            return EventChanges::default();
        };
        let windows = {
            let monitor_names = self.shared.monitor_names.read();
            let shown_special = self.shared.shown_special.read();
            let workspaces = self.shared.workspaces.read();
            let context = WindowContext {
                monitor_names: &monitor_names,
                shown_special: &shown_special,
                workspaces: &workspaces,
            };
            Self::build_windows(clients, &context, &mut self.shared.tracking.write())
        };
        EventChanges {
            window_list: self.apply_windows(windows),
            workspaces: self.sync_urgent_workspaces(),
            ..Default::default()
        }
    }

    /// Fetch the full state from Hyprland.
    fn fetch_all(&self) {
        self.refresh();
        self.refresh_active_window(true);
        debug!("Fetched Hyprland state");
    }

    /// Fetch keyboard layout info from Hyprland devices.
    fn fetch_keyboard_layout(&self) {
        let Some(devices) = self.query_json("devices") else {
            debug!("fetch_keyboard_layout: failed to query devices");
            return;
        };

        let Some(keyboards) = devices.get("keyboards").and_then(|v| v.as_array()) else {
            debug!("fetch_keyboard_layout: no keyboards in devices response");
            return;
        };

        // Use Hyprland's `main` flag to find the primary keyboard.
        // Falls back to first keyboard if none is marked main.
        let main_kb = keyboards
            .iter()
            .find(|kb| kb.get("main").and_then(|v| v.as_bool()) == Some(true))
            .or_else(|| keyboards.first());

        let Some(kb) = main_kb else {
            debug!("fetch_keyboard_layout: no suitable keyboard found");
            return;
        };

        let kb_name = kb
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let active_layout = kb
            .get("active_keymap")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Hyprland's "layout" field is a comma-separated list of layout codes
        let layout_count = kb
            .get("layout")
            .and_then(|v| v.as_str())
            .map(|layout_str| layout_str.split(',').filter(|s| !s.is_empty()).count());

        debug!(
            "fetch_keyboard_layout: main_kb='{}', layout='{}', layout_count={:?}",
            kb_name, active_layout, layout_count
        );

        *self.shared.main_keyboard_name.write() = Some(kb_name);
        *self.shared.keyboard_layout.write() = Some(KeyboardLayoutInfo {
            layout_name: active_layout,
            layout_count,
        });
    }

    /// Skips the focused window, as no later focus event would clear it.
    fn mark_urgent(&self, address: &str) -> EventChanges {
        let Some(id) = Self::parse_address(address) else {
            return EventChanges::default();
        };
        let inserted = {
            let mut tracking = self.shared.tracking.write();
            tracking.focused != Some(id) && tracking.urgent.insert(id)
        };
        if !inserted {
            return EventChanges::default();
        }
        EventChanges {
            window_list: self.sync_window_flags(),
            workspaces: self.sync_urgent_workspaces(),
            ..Default::default()
        }
    }

    /// Focusing a window clears its urgency, not that of others on its workspace.
    fn set_focused_window(&self, id: Option<u64>) -> EventChanges {
        {
            let mut tracking = self.shared.tracking.write();
            tracking.focused = id;
            if let Some(id) = id {
                tracking.urgent.remove(&id);
            }
        }
        EventChanges {
            window_list: self.sync_window_flags(),
            workspaces: self.sync_urgent_workspaces(),
            ..Default::default()
        }
    }

    /// From `windowtitlev2>>ADDRESS,TITLE`.
    fn update_window_title(&self, data: &str) -> bool {
        let Some((address, title)) = data.split_once(',') else {
            return false;
        };
        let Some(id) = Self::parse_address(address) else {
            return false;
        };
        let mut windows = self.shared.windows.write();
        match windows.iter_mut().find(|win| win.id == id) {
            Some(win) if win.title != title => {
                win.title = title.to_string();
                true
            }
            _ => false,
        }
    }

    /// Refresh the focused window from `activewindow`. Pass `fresh` when the
    /// caller has just called `refresh`.
    fn refresh_active_window(&self, fresh: bool) -> EventChanges {
        let Some(active_window) = self.query_json("activewindow") else {
            return EventChanges::default();
        };

        let parse =
            || Self::window_info_from_ipc(&active_window, &self.shared.monitor_names.read());
        let mut new_focused = parse();
        let unknown_monitor = active_window
            .get("monitor")
            .and_then(Value::as_i64)
            .is_some_and(|id| !self.shared.monitor_names.read().contains_key(&id));

        // Monitors can be added after the last rebuild. With no focused window,
        // the empty update is scoped to the focused monitor, so query it now
        // rather than trusting `focusedmon` to have arrived first.
        let mut changes = EventChanges::default();
        if !fresh && (unknown_monitor || new_focused.is_none()) {
            changes = self.refresh();
            new_focused = parse();
        }

        changes |= self.set_focused_window(Self::client_address(&active_window));

        let mut focused = self.shared.focused_window.write();
        changes.window = *focused != new_focused;
        if changes.window {
            *focused = new_focused;
        }
        changes
    }

    /// Parse `activewindow` JSON. Hyprland returns `{}` when no window is
    /// focused, and reports the monitor as a numeric ID.
    fn window_info_from_ipc(
        window: &Value,
        monitor_names: &HashMap<i64, String>,
    ) -> Option<WindowInfo> {
        let address = window.get("address").and_then(|v| v.as_str())?;
        if address.is_empty() {
            return None;
        }

        let title = window
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let app_id = window
            .get("class")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let output = window
            .get("monitor")
            .and_then(|v| v.as_i64())
            .and_then(|id| monitor_names.get(&id).cloned());

        Some(WindowInfo {
            title,
            app_id,
            output,
            focused: true,
        })
    }

    /// Focused window for the window callback. With nothing focused, the
    /// empty update carries the focused monitor so only that bar clears.
    fn window_update(&self) -> WindowInfo {
        let focused = self.shared.focused_window.read().clone();
        focused.unwrap_or_else(|| WindowInfo {
            output: self.shared.focused_monitor.read().clone(),
            focused: true,
            ..Default::default()
        })
    }

    /// Handle a Hyprland event line.
    fn handle_event(&self, line: &str) -> EventChanges {
        super::visibility::notify_changed();
        let Some((event, data)) = line.split_once(">>") else {
            return EventChanges::default();
        };

        trace!(
            "Hyprland event: {}>>{}...",
            event,
            &data[..data.len().min(50)]
        );

        let mut changes = EventChanges::default();

        // Hyprland sends most events twice, as v1 and v2. Where the payload
        // isn't used, handle only the v1 name, which every version sends.
        match event {
            // `workspace` also moves pinned windows and shown scratchpads;
            // a config reload can move every window.
            "workspace" | "createworkspace" | "destroyworkspace" | "renameworkspace"
            | "changeworkspaceid" | "openwindow" | "closewindow" | "monitoradded"
            | "monitorremoved" | "activespecial" | "configreloaded" => {
                changes = self.refresh();
            }
            "focusedmon" => {
                changes = self.refresh();
                // With no focused window, refresh before emitting: the new
                // monitor may have a window whose `activewindow` event hasn't
                // arrived yet, and emitting the empty title first would flicker.
                // Always emit: the focused window stays `None`, but the empty
                // update has to move to the newly focused monitor.
                if self.shared.focused_window.read().is_none() {
                    changes |= self.refresh_active_window(true);
                    changes.window = true;
                }
            }
            "movewindow" | "moveworkspace" => {
                // The focused window may have changed monitor while keeping
                // focus, which emits no `activewindow`; refresh its output.
                changes = self.refresh();
                changes |= self.refresh_active_window(true);
            }
            "activewindow" => {
                let focus_key = || {
                    (
                        self.shared.tracking.read().focused,
                        self.shared
                            .focused_window
                            .read()
                            .as_ref()
                            .map(|w| (w.app_id.clone(), w.output.clone())),
                    )
                };
                let before = focus_key();
                changes = self.refresh_active_window(false);
                // Also sent on title changes, which `windowtitlev2` applies.
                // Swapped tiles are picked up on the next focus change.
                if focus_key() != before {
                    changes |= self.refresh_windows();
                }
            }
            "changefloatingmode" | "togglegroup" | "moveintogroup" | "moveoutofgroup" => {
                changes = self.refresh_windows();
            }
            // The v2 payload carries the title, so no query is needed.
            "windowtitlev2" => {
                changes.window_list = self.update_window_title(data);
            }
            "urgent" => {
                // urgent>>WINDOW_ADDRESS
                changes = self.mark_urgent(data);
            }
            "activelayout" => {
                // activelayout>>KEYBOARD_NAME,LAYOUT_NAME
                if let Some((kb_name, layout_name)) = data.split_once(',') {
                    let is_main = self
                        .shared
                        .main_keyboard_name
                        .read()
                        .as_ref()
                        .is_some_and(|name| name == kb_name);

                    if is_main {
                        let info = KeyboardLayoutInfo {
                            layout_name: layout_name.to_string(),
                            layout_count: self
                                .shared
                                .keyboard_layout
                                .read()
                                .as_ref()
                                .and_then(|i| i.layout_count),
                        };
                        *self.shared.keyboard_layout.write() = Some(info);
                        changes.keyboard_layout = true;
                    } else {
                        let previous = self.shared.keyboard_layout.read().clone();
                        debug!(
                            event_keyboard = kb_name,
                            cached_main_keyboard = ?*self.shared.main_keyboard_name.read(),
                            "Refreshing keyboard layout after non-main activelayout event"
                        );
                        self.fetch_keyboard_layout();
                        changes.keyboard_layout =
                            self.shared.keyboard_layout.read().as_ref() != previous.as_ref();
                    }
                }
            }
            _ => {}
        }

        changes
    }

    /// Invoke the callbacks for what changed.
    fn emit(&self, changes: EventChanges) {
        if let Some((ws_cb, win_cb)) = self
            .callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            if changes.workspaces {
                ws_cb(self.shared.workspace_snapshot.read().clone());
            }
            if changes.window {
                win_cb(self.window_update());
            }
        }

        if changes.window_list
            && let Some(ref cb) = *self
                .window_list_callback
                .lock()
                .unwrap_or_else(|e| e.into_inner())
        {
            let windows = self.shared.windows.read().clone();
            cb(WindowListSnapshot { windows });
        }

        if changes.keyboard_layout
            && let Some(ref kb_cb) = *self
                .keyboard_layout_callback
                .lock()
                .unwrap_or_else(|e| e.into_inner())
            && let Some(ref info) = *self.shared.keyboard_layout.read()
        {
            kb_cb(info.clone());
        }
    }

    /// Run the event loop (in background thread).
    fn event_loop(backend: Arc<Self>) {
        let event_socket_path = {
            let path = backend.event_socket_path.read();
            match path.as_ref() {
                Some(p) => p.clone(),
                None => {
                    error!("No event socket path for Hyprland");
                    return;
                }
            }
        };

        // Exponential backoff state
        let mut backoff_ms = RECONNECT_INITIAL_MS;

        while backend.running.load(Ordering::SeqCst) {
            super::visibility::notify_changed();
            // Connect to event socket
            let stream = match UnixStream::connect(&event_socket_path) {
                Ok(s) => {
                    // Reset backoff on successful connection
                    backoff_ms = RECONNECT_INITIAL_MS;
                    s
                }
                Err(e) => {
                    if backend.running.load(Ordering::SeqCst) {
                        warn!(
                            "Failed to connect to Hyprland event socket: {}. Retrying in {}ms",
                            e, backoff_ms
                        );
                        thread::sleep(Duration::from_millis(backoff_ms));
                        // Exponential backoff with cap
                        backoff_ms = ((backoff_ms as f64) * RECONNECT_MULTIPLIER)
                            .min(RECONNECT_MAX_MS as f64)
                            as u64;
                    }
                    continue;
                }
            };

            // Fetch after connecting so changes made while disconnected
            // aren't missed.
            backend.fetch_all();
            backend.fetch_keyboard_layout();
            backend.emit(EventChanges {
                workspaces: true,
                window: true,
                window_list: true,
                keyboard_layout: true,
            });

            // Set read timeout for graceful shutdown
            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));

            let reader = BufReader::new(stream);

            for line in reader.lines() {
                if !backend.running.load(Ordering::SeqCst) {
                    break;
                }

                match line {
                    Ok(line) => {
                        let changes = backend.handle_event(&line);
                        backend.emit(changes);
                    }
                    Err(e) => {
                        // Timeout is expected, other errors should be logged
                        if e.kind() != std::io::ErrorKind::WouldBlock
                            && e.kind() != std::io::ErrorKind::TimedOut
                        {
                            if backend.running.load(Ordering::SeqCst) {
                                super::visibility::notify_changed();
                                error!("Error reading from Hyprland event socket: {}", e);
                            }
                            break;
                        }
                    }
                }
            }
        }

        debug!("Hyprland event loop exiting");
    }
}

impl CompositorBackend for HyprlandBackend {
    fn visibility_reader(&self) -> Option<super::visibility::VisibilityReader> {
        Some(super::visibility::VisibilityReader::Hyprland(
            self.socket_path.read().clone()?,
        ))
    }

    fn start(&self, on_workspace_update: WorkspaceCallback, on_window_update: WindowCallback) {
        if self.running.swap(true, Ordering::SeqCst) {
            warn!("HyprlandBackend already running");
            return;
        }

        debug!("Starting HyprlandBackend");

        // Resolve socket paths BEFORE storing callbacks
        // This ensures socket_path is set on `self` for switch_workspace()
        if !self.resolve_socket_paths() {
            warn!("Failed to resolve Hyprland socket paths");
            self.running.store(false, Ordering::SeqCst);
            return;
        }

        self.probe_lua_dispatch_support();

        // Disable compositor-level layer animations for our surfaces.
        self.apply_layer_rules();

        // Store callbacks
        *self.callbacks.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((on_workspace_update, on_window_update));

        let backend = Arc::new(self.event_thread_instance());

        // Start event loop thread
        let handle = thread::Builder::new()
            .name("hyprland-event-loop".into())
            .spawn(move || {
                Self::event_loop(backend);
            })
            .ok();

        *self.event_thread.lock().unwrap_or_else(|e| e.into_inner()) = handle;

        debug!("HyprlandBackend started");
    }

    fn stop(&self) {
        if !self.running.swap(false, Ordering::SeqCst) {
            return;
        }

        debug!("Stopping HyprlandBackend");

        // Wait for thread to finish
        if let Some(handle) = self
            .event_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = handle.join();
        }

        debug!("HyprlandBackend stopped");
    }

    fn list_workspaces(&self) -> Vec<WorkspaceMeta> {
        self.shared.workspaces.read().clone()
    }

    fn get_workspace_snapshot(&self) -> WorkspaceSnapshot {
        self.shared.workspace_snapshot.read().clone()
    }

    fn get_focused_window(&self) -> Option<WindowInfo> {
        self.shared.focused_window.read().clone()
    }

    fn get_focused_output(&self) -> Option<String> {
        self.shared.focused_monitor.read().clone()
    }

    fn list_windows(&self) -> Vec<Window> {
        self.shared.windows.read().clone()
    }

    fn provides_window_list(&self) -> bool {
        true
    }

    fn set_window_list_callback(&self, callback: WindowListCallback) {
        *self
            .window_list_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(callback);
    }

    fn focus_window(&self, window_id: u64) {
        // Hyprland switches to the window's workspace and monitor, and to its
        // tab if it's grouped.
        let selector = format!("address:0x{window_id:x}");
        self.send_dispatch(
            "focus_window",
            &format!(
                "dispatch hl.dsp.focus({{ window = {} }})",
                Self::lua_string(&selector)
            ),
            &format!("dispatch focuswindow {selector}"),
        );
    }

    fn switch_workspace(&self, workspace_id: i32) {
        let legacy_target = self.workspace_switch_target(workspace_id);
        let lua_target = self.workspace_lua_target(workspace_id);
        self.send_dispatch(
            "switch_workspace",
            &format!("dispatch hl.dsp.focus({{ workspace = {lua_target} }})"),
            &format!("dispatch workspace {legacy_target}"),
        );
    }

    fn quit_compositor(&self) {
        debug!("Sending exit command to Hyprland");
        self.send_dispatch("quit_compositor", "dispatch hl.dsp.exit()", "dispatch exit");
    }

    fn name(&self) -> &'static str {
        "Hyprland"
    }

    fn set_keyboard_layout_callback(&self, callback: KeyboardLayoutCallback) {
        *self
            .keyboard_layout_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(callback);
    }

    fn get_keyboard_layout(&self) -> Option<KeyboardLayoutInfo> {
        self.shared.keyboard_layout.read().clone()
    }

    fn switch_keyboard_layout_next(&self) {
        // `current` targets the keyboard last typed on, resolved by Hyprland
        // at call time. Hyprland < 0.43 doesn't support it (nor `all`), so
        // fall back to the cached main keyboard by name. An error reply means
        // nothing was switched, so retrying is safe; no reply may mean the
        // command ran, so don't retry (it would switch twice).
        match self.send_command("switchxkblayout current next") {
            Some(response) if Self::response_is_ok(&response) => {
                debug!("Switched keyboard layout for current keyboard");
            }
            // An empty reply isn't an explicit error; the command may have run.
            Some(response) if response.trim().is_empty() => {
                warn!("switchxkblayout current: empty response from Hyprland");
            }
            Some(response) => {
                let main_keyboard = self.shared.main_keyboard_name.read().clone();
                debug!(
                    response = %response.trim(),
                    keyboard = ?main_keyboard,
                    "switchxkblayout current failed, trying cached main keyboard"
                );
                if let Some(name) = main_keyboard {
                    let _ = self.send_command(&format!("switchxkblayout {name} next"));
                }
            }
            None => warn!("switchxkblayout current: no response from Hyprland"),
        }
    }

    fn refresh_pointer_focus(&self) {
        self.refresh_pointer_focus_impl();
    }
}

impl Drop for HyprlandBackend {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashSet;

    fn meta(workspaces: Value) -> Vec<WorkspaceMeta> {
        state(json!([]), workspaces).workspaces
    }

    fn named_id(address: &str) -> i32 {
        synthetic_id_for_name(address)
    }

    #[test]
    fn workspace_meta_merges_names_with_defaults() {
        let workspaces = meta(json!([
            { "id": 2, "name": "web", "windows": 1, "monitor": "DP-1" },
            { "id": 11, "name": "chat", "windows": 0, "monitor": "HDMI-A-1" }
        ]));

        assert_eq!(workspaces.iter().find(|ws| ws.id == 1).unwrap().name, "1");
        assert_eq!(workspaces.iter().find(|ws| ws.id == 2).unwrap().name, "web");
        assert_eq!(
            workspaces.iter().find(|ws| ws.id == 11).unwrap().name,
            "chat"
        );
        assert!(workspaces.iter().all(|ws| ws.output.is_none()));
    }

    #[test]
    fn parse_workspace_skips_special_workspaces() {
        for special in [
            json!({ "id": -99, "name": "special:magic" }),
            json!({ "address": "special:magic", "type": "special", "name": "special:magic" }),
        ] {
            assert_eq!(HyprlandBackend::parse_workspace(&special), None);
        }
    }

    #[test]
    fn parse_workspace_reads_numbered_workspaces_in_both_formats() {
        let legacy = json!({ "id": 4, "name": "Discord" });
        let current = json!({ "address": "4", "id": 4, "type": "normal", "name": "Discord" });

        for ws in [legacy, current] {
            let parsed = HyprlandBackend::parse_workspace(&ws).unwrap();
            assert_eq!(parsed.id, 4);
            assert_eq!(parsed.name, "Discord");
            assert_eq!(parsed.address, "4");
        }
        assert_eq!(
            HyprlandBackend::parse_workspace(&json!({ "id": 4, "name": "" }))
                .unwrap()
                .name,
            "4"
        );
    }

    #[test]
    fn parse_workspace_keeps_hyprland_named_ids_on_older_versions() {
        let legacy = json!({ "id": -1337, "name": "web" });
        let prefixed = json!({ "id": -1337, "name": "name:web" });

        for ws in [legacy, prefixed] {
            let parsed = HyprlandBackend::parse_workspace(&ws).unwrap();
            assert_eq!(parsed.id, -1337);
            assert_eq!(parsed.name, "web");
            assert_eq!(parsed.address, "web");
        }
    }

    #[test]
    fn parse_workspace_hashes_named_addresses_on_newer_versions() {
        let current = json!({ "address": "web", "type": "normal", "name": "web" });

        let parsed = HyprlandBackend::parse_workspace(&current).unwrap();
        assert_eq!(parsed.id, named_id("web"));
        assert_eq!(parsed.name, "web");
        assert_eq!(parsed.address, "web");
    }

    #[test]
    fn parse_workspace_keeps_named_id_across_renames() {
        // Newer Hyprland renames only the display name.
        let renamed = json!({ "address": "web", "type": "normal", "name": "Browser" });

        let parsed = HyprlandBackend::parse_workspace(&renamed).unwrap();
        assert_eq!(parsed.id, named_id("web"));
        assert_eq!(parsed.name, "Browser");
        assert_eq!(parsed.address, "web");
    }

    #[test]
    fn parse_workspace_rejects_missing_workspaces() {
        for none in [
            json!({ "id": 0, "name": "" }),
            json!({ "id": -1, "name": "" }),
            json!({ "address": "", "type": "", "name": "" }),
            json!({}),
        ] {
            assert_eq!(HyprlandBackend::parse_workspace(&none), None);
        }
    }

    #[test]
    fn workspace_meta_has_no_display_index_for_named_workspaces() {
        let workspaces = meta(json!([{ "address": "web", "type": "normal", "name": "web" }]));
        let web = workspaces.iter().find(|ws| ws.name == "web").unwrap();

        assert_eq!(web.id, named_id("web"));
        assert_eq!(web.idx, -1);
    }

    #[test]
    fn workspace_meta_sorts_numbered_before_named_in_creation_order() {
        let expected = [
            "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "12", "web", "Chat", "mail",
        ];
        // Older Hyprland: named IDs count down from -1337 as they're created.
        let legacy = meta(json!([
            { "id": -1339, "name": "mail" },
            { "id": 12, "name": "12" },
            { "id": -1337, "name": "web" },
            { "id": -1338, "name": "Chat" }
        ]));
        // Newer Hyprland: workspaces are listed in creation order.
        let current = meta(json!([
            { "address": "web", "type": "normal", "name": "web" },
            { "address": "12", "id": 12, "type": "normal", "name": "12" },
            { "address": "Chat", "type": "normal", "name": "Chat" },
            { "address": "mail", "type": "normal", "name": "mail" }
        ]));

        for workspaces in [legacy, current] {
            let names: Vec<_> = workspaces.iter().map(|ws| ws.name.as_str()).collect();
            assert_eq!(names, expected);
        }
    }

    #[test]
    fn workspace_switch_target_uses_named_workspace_address() {
        let backend = HyprlandBackend::new();
        backend.apply_state(state(
            json!([]),
            json!([{ "address": "web", "type": "normal", "name": "Browser" }]),
        ));

        assert_eq!(backend.workspace_switch_target(named_id("web")), "name:web");
        assert_eq!(backend.workspace_switch_target(3), "3");
        assert_eq!(backend.workspace_switch_target(-9999), "-9999");
    }

    #[test]
    fn workspace_lua_target_quotes_named_workspaces() {
        let backend = HyprlandBackend::new();
        backend.apply_state(state(json!([]), json!([{ "id": -1337, "name": "web" }])));

        assert_eq!(backend.workspace_lua_target(-1337), "\"name:web\"");
        assert_eq!(backend.workspace_lua_target(3), "3");
        assert_eq!(backend.workspace_lua_target(-9999), "-9999");
    }

    #[test]
    fn lua_string_escapes_special_chars() {
        assert_eq!(HyprlandBackend::lua_string("quote\""), "\"quote\\\"\"");
        assert_eq!(HyprlandBackend::lua_string("slash\\"), "\"slash\\\\\"");
        assert_eq!(HyprlandBackend::lua_string("new\nline"), "\"new\\nline\"");
        assert_eq!(
            HyprlandBackend::lua_string("carriage\rreturn"),
            "\"carriage\\rreturn\""
        );
        assert_eq!(HyprlandBackend::lua_string("tab\tchar"), "\"tab\\tchar\"");
    }

    #[test]
    fn response_is_ok_rejects_errors() {
        assert!(HyprlandBackend::response_is_ok("ok"));
        assert!(HyprlandBackend::response_is_ok("ok\n"));
        assert!(HyprlandBackend::response_is_ok("OK"));
        assert!(!HyprlandBackend::response_is_ok(
            "error: return hl.dispatch(exit):1: hl.dispatch: expected a dispatcher"
        ));
        assert!(!HyprlandBackend::response_is_ok("invalid dispatcher"));
        assert!(!HyprlandBackend::response_is_ok("unknown dispatcher"));
        assert!(!HyprlandBackend::response_is_ok("workspace 1"));
        assert!(!HyprlandBackend::response_is_ok(""));
    }

    #[test]
    fn parse_cursorpos_accepts_trimmed_coordinates() {
        assert_eq!(
            HyprlandBackend::parse_cursorpos("123,456"),
            Some((123, 456))
        );
        assert_eq!(
            HyprlandBackend::parse_cursorpos("  -12, 34\n"),
            Some((-12, 34))
        );
        assert_eq!(HyprlandBackend::parse_cursorpos("123"), None);
        assert_eq!(HyprlandBackend::parse_cursorpos("x,456"), None);
    }

    #[test]
    fn window_info_resolves_monitor_id_to_connector() {
        let monitor_names = HashMap::from([(0, "eDP-1".to_string()), (1, "DP-3".to_string())]);
        let window = json!({
            "address": "0x55d0c0ffee",
            "title": "vibepanel",
            "class": "com.mitchellh.ghostty",
            "monitor": 1,
        });

        let info = HyprlandBackend::window_info_from_ipc(&window, &monitor_names).unwrap();

        assert_eq!(info.title, "vibepanel");
        assert_eq!(info.app_id, "com.mitchellh.ghostty");
        assert_eq!(info.output, Some("DP-3".to_string()));
    }

    #[test]
    fn window_info_is_none_without_focused_window() {
        let monitor_names = HashMap::from([(0, "eDP-1".to_string())]);

        assert!(HyprlandBackend::window_info_from_ipc(&json!({}), &monitor_names).is_none());
    }

    #[test]
    fn activelayout_main_keyboard_updates_layout_from_event() {
        let backend = HyprlandBackend::new();
        *backend.shared.main_keyboard_name.write() = Some("keyboard-a".to_string());
        *backend.shared.keyboard_layout.write() = Some(KeyboardLayoutInfo {
            layout_name: "English (US)".to_string(),
            layout_count: Some(2),
        });

        let changes = backend.handle_event("activelayout>>keyboard-a,German");

        assert!(changes.keyboard_layout);
        assert_eq!(
            backend
                .shared
                .keyboard_layout
                .read()
                .as_ref()
                .unwrap()
                .layout_name,
            "German"
        );
    }

    fn state(monitors: Value, workspaces: Value) -> HyprlandState {
        HyprlandBackend::build_state(monitors.as_array().unwrap(), workspaces.as_array().unwrap())
    }

    /// Fake command socket that answers `j/<name>` with the matching JSON and
    /// records each command it receives. Removes the socket file on drop.
    struct FakeSocket {
        path: std::path::PathBuf,
        commands: Arc<Mutex<Vec<String>>>,
    }

    impl Drop for FakeSocket {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn backend_with_fake_socket(
        test: &str,
        responses: Vec<(&'static str, Value)>,
    ) -> (HyprlandBackend, FakeSocket) {
        use std::os::unix::net::UnixListener;

        let path =
            std::env::temp_dir().join(format!("vp-hyprland-{}-{test}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind test Hyprland socket");
        let commands = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&commands);
        let responses: HashMap<String, String> = responses
            .into_iter()
            .map(|(name, value)| (format!("j/{name}"), value.to_string()))
            .collect();
        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut buf = [0u8; 1024];
                let n = stream.read(&mut buf).unwrap_or(0);
                let command = String::from_utf8_lossy(&buf[..n]).to_string();
                let response = responses.get(&command).cloned().unwrap_or_default();
                recorded.lock().unwrap().push(command);
                let _ = stream.write_all(response.as_bytes());
            }
        });

        let backend = HyprlandBackend::new();
        *backend.socket_path.write() = Some(path.to_string_lossy().into_owned());
        (backend, FakeSocket { path, commands })
    }

    fn two_monitors(focused_active: i32, other_active: i32) -> Value {
        json!([
            {"id": 0, "name": "DP-1", "focused": true,
             "activeWorkspace": {"id": focused_active, "name": focused_active.to_string()}},
            {"id": 1, "name": "HDMI-A-1", "focused": false,
             "activeWorkspace": {"id": other_active, "name": other_active.to_string()}}
        ])
    }

    #[test]
    fn build_state_tracks_active_workspace_per_output() {
        let state = state(two_monitors(2, 5), json!([]));

        assert_eq!(state.focused_monitor.as_deref(), Some("DP-1"));
        assert_eq!(state.snapshot.active_workspace, HashSet::from([2]));
        assert_eq!(
            state.snapshot.per_output["DP-1"].active_workspace,
            HashSet::from([2])
        );
        assert_eq!(
            state.snapshot.per_output["HDMI-A-1"].active_workspace,
            HashSet::from([5])
        );
        assert_eq!(state.monitor_names[&1], "HDMI-A-1");
    }

    #[test]
    fn build_state_counts_windows_per_output() {
        let state = state(
            two_monitors(1, 5),
            json!([
                {"id": 1, "name": "1", "monitor": "DP-1", "windows": 2},
                {"id": 3, "name": "3", "monitor": "DP-1", "windows": 0},
                {"id": 5, "name": "5", "monitor": "HDMI-A-1", "windows": 1}
            ]),
        );

        let snapshot = &state.snapshot;
        assert_eq!(snapshot.occupied_workspaces, HashSet::from([1, 5]));
        assert_eq!(snapshot.window_counts[&3], 0);
        assert_eq!(
            snapshot.per_output["DP-1"].occupied_workspaces,
            HashSet::from([1])
        );
        assert_eq!(snapshot.per_output["HDMI-A-1"].window_counts[&5], 1);
    }

    #[test]
    fn apply_state_reports_only_real_changes() {
        let backend = HyprlandBackend::new();
        let workspaces = json!([{"id": 1, "name": "1", "monitor": "DP-1", "windows": 1}]);

        assert!(backend.apply_state(state(two_monitors(1, 5), workspaces.clone())));
        assert!(!backend.apply_state(state(two_monitors(1, 5), workspaces.clone())));
        assert!(backend.apply_state(state(two_monitors(2, 5), workspaces)));
        assert_eq!(
            backend.get_workspace_snapshot().active_workspace,
            HashSet::from([2])
        );
    }

    fn client(address: u64, workspace: Value, at: (i64, i64)) -> Value {
        json!({
            "address": format!("0x{address:x}"),
            "workspace": workspace,
            "monitor": 0,
            "at": [at.0, at.1],
            "class": "app",
            "title": "title",
        })
    }

    fn ws(id: i32) -> Value {
        json!({ "id": id, "name": id.to_string() })
    }

    /// Rebuild the backend's windows and workspace state, as `refresh` does.
    fn apply_clients(backend: &HyprlandBackend, monitors: Value, clients: Value) {
        let state = state(monitors, json!([]));
        let windows = HyprlandBackend::build_windows(
            clients.as_array().unwrap(),
            &state.window_context(),
            &mut backend.shared.tracking.write(),
        );
        backend.apply_windows(windows);
        backend.apply_state(state);
    }

    fn order(backend: &HyprlandBackend) -> Vec<u64> {
        backend.list_windows().iter().map(|w| w.id).collect()
    }

    #[test]
    fn window_list_orders_by_output_workspace_then_position() {
        let backend = HyprlandBackend::new();
        let mut other_monitor = client(0x9, ws(5), (0, 0));
        other_monitor["monitor"] = 1.into();
        let group = json!(["0x6", "0x5"]);
        let mut tab_b = client(0x5, ws(1), (1720, 58));
        tab_b["grouped"] = group.clone();
        let mut tab_a = client(0x6, ws(1), (1720, 58));
        tab_a["grouped"] = group;
        // Stacking order. 0x4 is offset, but within the left column.
        let clients = json!([
            other_monitor,
            client(0x7, ws(3), (0, 0)),
            tab_b,
            client(0x2, ws(1), (22, 745)),
            tab_a,
            client(0x4, ws(1), (40, 400)),
            client(0x1, ws(1), (22, 58)),
        ]);

        apply_clients(&backend, two_monitors(1, 5), clients);

        assert_eq!(order(&backend), [0x1, 0x4, 0x2, 0x6, 0x5, 0x7, 0x9]);
    }

    #[test]
    fn fullscreen_window_keeps_its_slot() {
        let backend = HyprlandBackend::new();
        let left = client(0x2, ws(1), (22, 58));
        let tiled = json!([client(0x1, ws(1), (1720, 58)), left.clone()]);
        apply_clients(&backend, two_monitors(1, 5), tiled);

        // Newer Hyprland reports a mode, older a bool.
        for fullscreen in [json!(2), json!(true)] {
            let mut full = client(0x1, ws(1), (0, 0));
            full["fullscreen"] = fullscreen;
            apply_clients(&backend, two_monitors(1, 5), json!([full, left.clone()]));
            assert_eq!(order(&backend), [0x2, 0x1]);
        }
    }

    #[test]
    fn window_list_shows_only_shown_special_workspaces() {
        let new = |name: &str| json!({ "address": name, "type": "special", "name": name });
        let old = |id: i32, name: &str| json!({ "id": id, "name": name });
        let formats = [
            (
                new("special:a"),
                new("special:b"),
                json!({ "address": "", "name": "" }),
            ),
            (
                old(-98, "special:a"),
                old(-97, "special:b"),
                json!({ "id": 0, "name": "" }),
            ),
        ];
        for (shown, hidden, none) in formats {
            let backend = HyprlandBackend::new();
            let monitors = json!([
                {"id": 0, "name": "DP-1", "activeWorkspace": ws(2), "specialWorkspace": shown.clone()},
                {"id": 1, "name": "HDMI-A-1", "activeWorkspace": ws(5), "specialWorkspace": none}
            ]);
            let mut unmapped = client(0x4, ws(2), (0, 0));
            unmapped["mapped"] = false.into();
            let mut swallowing = client(0x5, ws(2), (0, 0));
            swallowing["swallowing"] = "0x6".into();
            let clients = json!([
                client(0x1, shown, (0, 0)),
                client(0x3, hidden, (0, 0)),
                unmapped,
                swallowing,
                client(0x6, ws(2), (0, 0)),
            ]);

            apply_clients(&backend, monitors, clients);

            let listed: Vec<_> = backend
                .list_windows()
                .iter()
                .map(|w| (w.id, w.workspace_id, w.is_scratchpad))
                .collect();
            assert_eq!(listed, [(0x5, Some(2), false), (0x1, Some(2), true)]);
        }
    }

    #[test]
    fn urgency_is_tracked_per_window() {
        let backend = HyprlandBackend::new();
        let clients = json!([client(0x1, ws(3), (0, 0)), client(0x2, ws(3), (0, 1))]);
        apply_clients(&backend, two_monitors(1, 5), clients.clone());
        let urgent = |backend: &HyprlandBackend| -> Vec<u64> {
            let windows = backend.list_windows();
            windows
                .iter()
                .filter(|w| w.is_urgent)
                .map(|w| w.id)
                .collect()
        };

        // Event payloads have no `0x` prefix.
        backend.handle_event("urgent>>1");
        backend.handle_event("urgent>>2");
        backend.set_focused_window(Some(0x1));
        assert_eq!(urgent(&backend), [0x2]);
        assert_eq!(
            backend.get_workspace_snapshot().urgent_workspaces,
            HashSet::from([3])
        );

        // The focused window doesn't become urgent, and rebuilds keep urgency.
        backend.handle_event("urgent>>1");
        apply_clients(&backend, two_monitors(3, 5), clients);
        assert_eq!(urgent(&backend), [0x2]);

        // Closing the window drops its urgency.
        apply_clients(
            &backend,
            two_monitors(3, 5),
            json!([client(0x1, ws(3), (0, 0))]),
        );
        assert!(
            backend
                .get_workspace_snapshot()
                .urgent_workspaces
                .is_empty()
        );
    }

    #[test]
    fn focus_window_dispatches_window_address() {
        let (backend, socket) = backend_with_fake_socket("focus-window", vec![]);

        backend.focus_window(0x55d0c0ffee);
        backend.supports_lua_dispatch.store(true, Ordering::Relaxed);
        backend.focus_window(0x55d0c0ffee);

        assert_eq!(
            *socket.commands.lock().unwrap(),
            [
                "dispatch focuswindow address:0x55d0c0ffee",
                "dispatch hl.dsp.focus({ window = \"address:0x55d0c0ffee\" })",
            ]
        );
    }

    #[test]
    fn event_thread_updates_are_visible_to_manager_side_backend() {
        let backend = HyprlandBackend::new();
        let event_thread = backend.event_thread_instance();

        event_thread.apply_state(state(two_monitors(2, 5), json!([])));

        assert_eq!(backend.get_focused_output(), Some("DP-1".to_string()));
        assert_eq!(
            backend.get_workspace_snapshot().active_workspace,
            HashSet::from([2])
        );
    }

    #[test]
    fn empty_window_update_follows_focused_monitor() {
        let backend = HyprlandBackend::new();
        backend.apply_state(state(two_monitors(2, 5), json!([])));

        assert!(backend.handle_event("focusedmon>>DP-1,2").window);
        assert_eq!(backend.window_update().output, Some("DP-1".to_string()));
        assert!(backend.window_update().title.is_empty());

        *backend.shared.focused_window.write() = Some(WindowInfo {
            title: "t".to_string(),
            app_id: "a".to_string(),
            output: Some("DP-1".to_string()),
            focused: true,
        });
        assert!(!backend.handle_event("focusedmon>>DP-1,2").window);
    }

    #[test]
    fn v2_event_twins_do_not_trigger_queries() {
        let (backend, socket) = backend_with_fake_socket(
            "v2-twins",
            vec![
                ("monitors", two_monitors(2, 5)),
                ("workspaces", json!([])),
                ("clients", json!([client(0xabc, ws(2), (0, 0))])),
                ("activewindow", json!({"address": "0xabc", "monitor": 0})),
            ],
        );

        for line in [
            "workspace>>2",
            "workspacev2>>2,2",
            "activewindow>>class,title",
            "activewindowv2>>abc",
            "createworkspacev2>>2,2",
            "destroyworkspacev2>>2,2",
            "moveworkspacev2>>2,2,DP-1",
            "monitoraddedv2>>1,DP-1,desc",
            "monitorremovedv2>>1,DP-1,desc",
            "movewindowv2>>abc,2,2",
            "activespecialv2>>,,DP-1",
            // The v2 twin updates titles without a query.
            "windowtitle>>abc",
            "windowtitlev2>>abc,vim: a, b",
        ] {
            backend.handle_event(line);
        }

        assert_eq!(
            *socket.commands.lock().unwrap(),
            [
                "j/monitors",
                "j/workspaces",
                "j/clients",
                "j/activewindow",
                "j/clients"
            ]
        );
        assert_eq!(
            backend.get_workspace_snapshot().active_workspace,
            HashSet::from([2])
        );
        let windows = backend.list_windows();
        assert_eq!(windows.len(), 1);
        assert!(windows[0].is_focused);
        assert_eq!(windows[0].title, "vim: a, b");
    }

    #[test]
    fn activewindow_requeries_clients_only_when_focus_changes() {
        // (previous focus, previous class, expected queries)
        let cases = [
            (0xa, "app", vec!["j/activewindow"]),
            (0xb, "app", vec!["j/activewindow", "j/clients"]),
            (0xa, "old", vec!["j/activewindow", "j/clients"]),
        ];
        for (i, (focused, class, expected)) in cases.into_iter().enumerate() {
            let active = json!({"address": "0xa", "class": "app", "monitor": 0});
            let (backend, socket) = backend_with_fake_socket(
                &format!("activewindow-{i}"),
                vec![("activewindow", active)],
            );
            backend.apply_state(state(two_monitors(1, 5), json!([])));
            backend.set_focused_window(Some(focused));
            *backend.shared.focused_window.write() = Some(WindowInfo {
                app_id: class.to_string(),
                output: Some("DP-1".to_string()),
                focused: true,
                ..Default::default()
            });

            backend.handle_event("activewindow>>app,new title");

            assert_eq!(*socket.commands.lock().unwrap(), expected, "case {i}");
        }
    }

    #[test]
    fn focusedmon_without_focused_window_rebuilds_once() {
        let (backend, socket) = backend_with_fake_socket(
            "focusedmon-rebuild",
            vec![
                ("monitors", two_monitors(2, 5)),
                ("workspaces", json!([])),
                ("activewindow", json!({})),
            ],
        );

        backend.handle_event("focusedmon>>DP-1,2");

        assert_eq!(
            *socket.commands.lock().unwrap(),
            ["j/monitors", "j/workspaces", "j/clients", "j/activewindow"]
        );
    }

    #[test]
    fn events_track_named_workspaces_without_ids() {
        let web = json!({ "address": "web", "type": "normal", "name": "web" });
        let (backend, _socket) = backend_with_fake_socket(
            "named-without-ids",
            vec![
                (
                    "monitors",
                    json!([{ "id": 0, "name": "DP-1", "focused": true,
                             "activeWorkspace": web,
                             "specialWorkspace": { "address": "", "type": "", "name": "" } }]),
                ),
                (
                    "workspaces",
                    json!([{ "address": "web", "type": "normal", "name": "web",
                             "monitor": "DP-1", "windows": 1 }]),
                ),
            ],
        );

        assert!(backend.handle_event("workspace>>web").workspaces);
        let snapshot = backend.get_workspace_snapshot();
        assert_eq!(snapshot.active_workspace, HashSet::from([named_id("web")]));
        assert_eq!(
            snapshot.occupied_workspaces,
            HashSet::from([named_id("web")])
        );
        assert!(backend.list_workspaces().iter().any(|ws| ws.name == "web"));
        assert_eq!(backend.workspace_switch_target(named_id("web")), "name:web");
    }
}
