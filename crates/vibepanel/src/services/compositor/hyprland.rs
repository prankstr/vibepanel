//! Hyprland compositor backend using native socket IPC.
//!
//! This backend communicates with Hyprland via its Unix sockets:
//! - `.socket.sock` for commands/queries (JSON responses)
//! - `.socket2.sock` for event subscription
//!
//! Reference: https://wiki.hyprland.org/IPC/

use std::collections::HashMap;
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

use super::{
    CompositorBackend, KeyboardLayoutCallback, KeyboardLayoutInfo, WindowCallback, WindowInfo,
    WorkspaceCallback, WorkspaceMeta, WorkspaceSnapshot,
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
    focused_monitor: RwLock<Option<String>>,
    /// Hyprland monitor ID → connector name, used to resolve window outputs.
    monitor_names: RwLock<HashMap<i64, String>>,
    /// Current keyboard layout info.
    keyboard_layout: RwLock<Option<KeyboardLayoutInfo>>,
    /// Name of the main keyboard device (auto-detected from `hyprctl devices`).
    main_keyboard_name: RwLock<Option<String>>,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            workspace_snapshot: RwLock::new(WorkspaceSnapshot::default()),
            focused_window: RwLock::new(None),
            workspaces: RwLock::new(HyprlandBackend::default_workspaces()),
            focused_monitor: RwLock::new(None),
            monitor_names: RwLock::new(HashMap::new()),
            keyboard_layout: RwLock::new(None),
            main_keyboard_name: RwLock::new(None),
        }
    }
}

/// Workspace state rebuilt from one `monitors` and `workspaces` query pair.
#[derive(Debug, Default)]
struct HyprlandState {
    snapshot: WorkspaceSnapshot,
    workspaces: Vec<WorkspaceMeta>,
    focused_monitor: Option<String>,
    monitor_names: HashMap<i64, String>,
}

pub struct HyprlandBackend {
    running: Arc<AtomicBool>,
    event_thread: Mutex<Option<JoinHandle<()>>>,
    socket_path: RwLock<Option<String>>,
    event_socket_path: RwLock<Option<String>>,
    shared: Arc<SharedState>,
    callbacks: Mutex<Option<(WorkspaceCallback, WindowCallback)>>,
    /// Callback for keyboard layout changes, set by CompositorManager.
    keyboard_layout_callback: Mutex<Option<KeyboardLayoutCallback>>,
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

    fn workspace_identity(id: Option<i64>, raw_name: &str) -> Option<(i32, String)> {
        if raw_name.starts_with("special:") {
            return None;
        }

        // Hyprland reserves 0 as invalid/no workspace; valid numeric workspaces
        // are positive and named workspaces use negative compositor-assigned IDs.
        if let Some(id) = id.and_then(|id| i32::try_from(id).ok())
            && id != 0
        {
            // Hyprland IPC normally returns bare names (`web`). Accept the
            // dispatcher form (`name:web`) defensively because callers use it
            // when switching to named workspaces.
            let name = if let Some(name) = raw_name.strip_prefix("name:")
                && !name.is_empty()
            {
                name.to_string()
            } else if raw_name.is_empty() {
                id.to_string()
            } else {
                raw_name.to_string()
            };
            return Some((id, name));
        }

        None
    }

    fn workspace_identity_from_ipc(workspace: &Value) -> Option<(i32, String)> {
        let raw_name = workspace.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let id = workspace.get("id").and_then(|v| v.as_i64());
        Self::workspace_identity(id, raw_name)
    }

    fn workspace_meta_from_ipc(workspaces: &[Value]) -> Vec<WorkspaceMeta> {
        let mut merged: HashMap<i32, WorkspaceMeta> = Self::default_workspaces()
            .into_iter()
            .map(|ws| (ws.id, ws))
            .collect();

        for ws in workspaces {
            let Some((id, name)) = Self::workspace_identity_from_ipc(ws) else {
                continue;
            };
            merged.insert(
                id,
                WorkspaceMeta {
                    id,
                    // Positive IDs are user-facing numeric workspaces. Negative
                    // IDs belong to truly named workspaces and are Hyprland
                    // internals, so they have no meaningful numeric index.
                    idx: if id > 0 { id } else { -1 },
                    name,
                    output: None,
                },
            );
        }

        let mut workspaces: Vec<_> = merged.into_values().collect();
        workspaces.sort_by(|a, b| match (a.id > 0, b.id > 0) {
            (true, true) => a.id.cmp(&b.id),
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            // Hyprland assigns named workspace IDs downward from -1337; values
            // closer to zero were created earlier and should appear first.
            (false, false) => b.id.cmp(&a.id),
        });
        workspaces
    }

    fn workspace_id_from_snapshot(workspace: &Value) -> Option<i32> {
        let raw_name = workspace.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let id = workspace.get("id").and_then(|v| v.as_i64());
        Self::workspace_identity(id, raw_name).map(|(id, _)| id)
    }

    fn workspace_switch_target(&self, workspace_id: i32) -> String {
        if workspace_id < 0 {
            let workspaces = self.shared.workspaces.read();
            if let Some(ws) = workspaces.iter().find(|ws| ws.id == workspace_id) {
                if ws.name.chars().any(char::is_whitespace) {
                    warn!(
                        "Hyprland named workspace {:?} contains whitespace; switching may fail \
                         if Hyprland does not accept the raw name in workspace dispatch",
                        ws.name
                    );
                }
                return format!("name:{}", ws.name);
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
    /// carried over by `apply_state`.
    fn build_state(monitors: &[Value], workspaces: &[Value]) -> HyprlandState {
        let mut state = HyprlandState {
            workspaces: Self::workspace_meta_from_ipc(workspaces),
            ..Default::default()
        };
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
            if let Some(ws_id) = mon
                .get("activeWorkspace")
                .and_then(Self::workspace_id_from_snapshot)
            {
                per_output.active_workspace.insert(ws_id);
                if focused {
                    snapshot.active_workspace.insert(ws_id);
                }
            }
        }

        for ws in workspaces {
            let (Some(id), Some(windows)) = (
                Self::workspace_id_from_snapshot(ws),
                ws.get("windows").and_then(Value::as_u64),
            ) else {
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

        state
    }

    /// Store a rebuilt state. Returns true if the workspace snapshot or
    /// metadata changed.
    fn apply_state(&self, state: HyprlandState) -> bool {
        let HyprlandState {
            mut snapshot,
            workspaces,
            focused_monitor,
            monitor_names,
        } = state;
        *self.shared.focused_monitor.write() = focused_monitor;
        *self.shared.monitor_names.write() = monitor_names;

        let metadata_changed = {
            let mut current = self.shared.workspaces.write();
            let changed = *current != workspaces;
            if changed {
                *current = workspaces;
            }
            changed
        };

        let mut current = self.shared.workspace_snapshot.write();
        // Urgency only arrives as events. Keep it while the workspace has
        // windows, and treat switching to the workspace as acknowledging it.
        snapshot.urgent_workspaces = current
            .urgent_workspaces
            .iter()
            .copied()
            .filter(|id| {
                snapshot.occupied_workspaces.contains(id)
                    && (!snapshot.active_workspace.contains(id)
                        || current.active_workspace.contains(id))
            })
            .collect();
        let snapshot_changed = *current != snapshot;
        if snapshot_changed {
            *current = snapshot;
        }

        metadata_changed || snapshot_changed
    }

    /// Re-query monitors and workspaces and store the result.
    /// Returns true if the workspace snapshot or metadata changed.
    fn refresh_workspaces(&self) -> bool {
        let monitors = self.query_json("monitors");
        let workspaces = self.query_json("workspaces");
        let (Some(monitors), Some(workspaces)) = (
            monitors.as_ref().and_then(Value::as_array),
            workspaces.as_ref().and_then(Value::as_array),
        ) else {
            warn!("Failed to query Hyprland monitors or workspaces");
            return false;
        };
        self.apply_state(Self::build_state(monitors, workspaces))
    }

    /// Fetch initial state from Hyprland.
    fn fetch_initial_state(&self) {
        self.refresh_workspaces();
        self.refresh_active_window();
        debug!("Fetched initial Hyprland state");
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

    fn clear_urgent_workspace(&self, ws_id: i32) -> bool {
        // Hyprland exposes urgency as an event only, not in clients/workspaces JSON.
        // Treat focusing a window on the workspace as acknowledging workspace-level urgency.
        // TODO: When Hyprland list_windows support lands, replace workspace-level
        // urgency with address-based tracking so multiple urgent clients on the same
        // workspace survive focusing/viewing only one client.
        self.shared
            .workspace_snapshot
            .write()
            .urgent_workspaces
            .remove(&ws_id)
    }

    /// Refresh the focused window from `activewindow`.
    /// Returns (window_changed, workspace_changed).
    fn refresh_active_window(&self) -> (bool, bool) {
        let Some(active_window) = self.query_json("activewindow") else {
            return (false, false);
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
        let mut workspace_changed = false;
        if unknown_monitor || new_focused.is_none() {
            workspace_changed = self.refresh_workspaces();
            new_focused = parse();
        }

        if let Some(ws_id) = active_window
            .get("workspace")
            .and_then(Self::workspace_id_from_snapshot)
        {
            workspace_changed |= self.clear_urgent_workspace(ws_id);
        }

        let mut focused = self.shared.focused_window.write();
        let window_changed = *focused != new_focused;
        if window_changed {
            *focused = new_focused;
        }
        (window_changed, workspace_changed)
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
    /// Returns (workspace_changed, window_changed, keyboard_layout_changed).
    fn handle_event(&self, line: &str) -> (bool, bool, bool) {
        super::visibility::notify_changed();
        let Some((event, data)) = line.split_once(">>") else {
            return (false, false, false);
        };

        trace!(
            "Hyprland event: {}>>{}...",
            event,
            &data[..data.len().min(50)]
        );

        let mut workspace_changed = false;
        let mut window_changed = false;
        let mut keyboard_layout_changed = false;

        match event {
            "workspace" | "workspacev2" | "createworkspace" | "createworkspacev2"
            | "destroyworkspace" | "destroyworkspacev2" | "renameworkspace" | "openwindow"
            | "closewindow" | "monitoradded" | "monitoraddedv2" | "monitorremoved"
            | "monitorremovedv2" => {
                workspace_changed = self.refresh_workspaces();
            }
            "focusedmon" => {
                workspace_changed = self.refresh_workspaces();
                // With no focused window, refresh before emitting: the new
                // monitor may have a window whose `activewindow` event hasn't
                // arrived yet, and emitting the empty title first would flicker.
                // Always emit: the empty update follows the focused monitor,
                // and duplicate clears are harmless.
                if self.shared.focused_window.read().is_none() {
                    workspace_changed |= self.refresh_active_window().1;
                    window_changed = true;
                }
            }
            "movewindow" | "moveworkspace" | "moveworkspacev2" => {
                // The focused window may have changed monitor while keeping
                // focus, which emits no `activewindow`; refresh its output.
                workspace_changed = self.refresh_workspaces();
                let (win, ws) = self.refresh_active_window();
                window_changed = win;
                workspace_changed |= ws;
            }
            "activewindow" | "activewindowv2" => {
                (window_changed, workspace_changed) = self.refresh_active_window();
            }
            "urgent" => {
                // urgent>>WINDOW_ADDRESS
                if let Some(clients) = self.query_json("clients")
                    && let Some(clients) = clients.as_array()
                {
                    for client in clients {
                        let addr = client.get("address").and_then(|v| v.as_str()).unwrap_or("");
                        if addr == data || addr == format!("0x{}", data) {
                            if let Some(ws) = client.get("workspace")
                                && let Some(ws_id) = Self::workspace_id_from_snapshot(ws)
                            {
                                let mut snapshot = self.shared.workspace_snapshot.write();
                                workspace_changed = snapshot.urgent_workspaces.insert(ws_id);
                            }
                            break;
                        }
                    }
                }
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
                        keyboard_layout_changed = true;
                    } else {
                        let previous = self.shared.keyboard_layout.read().clone();
                        debug!(
                            event_keyboard = kb_name,
                            cached_main_keyboard = ?*self.shared.main_keyboard_name.read(),
                            "Refreshing keyboard layout after non-main activelayout event"
                        );
                        self.fetch_keyboard_layout();
                        keyboard_layout_changed =
                            self.shared.keyboard_layout.read().as_ref() != previous.as_ref();
                    }
                }
            }
            _ => {}
        }

        (workspace_changed, window_changed, keyboard_layout_changed)
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

        // Fetch initial state and emit
        backend.fetch_initial_state();

        // Fetch initial keyboard layout (queries devices, auto-detects main keyboard)
        backend.fetch_keyboard_layout();

        // Emit initial state
        if let Some((ws_cb, win_cb)) = backend
            .callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            ws_cb(backend.shared.workspace_snapshot.read().clone());
            win_cb(backend.window_update());
        }
        // Emit initial keyboard layout
        if let Some(ref kb_cb) = *backend
            .keyboard_layout_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            && let Some(ref info) = *backend.shared.keyboard_layout.read()
        {
            kb_cb(info.clone());
        }

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

            // Set read timeout for graceful shutdown
            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));

            let reader = BufReader::new(stream);

            for line in reader.lines() {
                if !backend.running.load(Ordering::SeqCst) {
                    break;
                }

                match line {
                    Ok(line) => {
                        let (ws_changed, win_changed, kb_changed) = backend.handle_event(&line);

                        if let Some((ws_cb, win_cb)) = backend
                            .callbacks
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .as_ref()
                        {
                            if ws_changed {
                                ws_cb(backend.shared.workspace_snapshot.read().clone());
                            }
                            if win_changed {
                                win_cb(backend.window_update());
                            }
                        }

                        if kb_changed
                            && let Some(ref kb_cb) = *backend
                                .keyboard_layout_callback
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                            && let Some(ref info) = *backend.shared.keyboard_layout.read()
                        {
                            kb_cb(info.clone());
                        }
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

    #[test]
    fn workspace_meta_from_ipc_merges_names_with_defaults() {
        let ipc = vec![
            json!({ "id": 2, "name": "web", "windows": 1, "monitor": "DP-1" }),
            json!({ "id": 11, "name": "chat", "windows": 0, "monitor": "HDMI-A-1" }),
        ];

        let workspaces = HyprlandBackend::workspace_meta_from_ipc(&ipc);

        assert_eq!(workspaces.iter().find(|ws| ws.id == 1).unwrap().name, "1");
        assert_eq!(workspaces.iter().find(|ws| ws.id == 2).unwrap().name, "web");
        assert_eq!(
            workspaces.iter().find(|ws| ws.id == 11).unwrap().name,
            "chat"
        );
        assert!(workspaces.iter().all(|ws| ws.output.is_none()));
    }

    #[test]
    fn workspace_meta_from_ipc_skips_special_workspaces() {
        let ipc = vec![
            json!({ "id": -99, "name": "special:magic" }),
            json!({ "id": 4, "name": "" }),
        ];

        let workspaces = HyprlandBackend::workspace_meta_from_ipc(&ipc);

        assert!(workspaces.iter().all(|ws| ws.name != "special:magic"));
        assert_eq!(workspaces.iter().find(|ws| ws.id == 4).unwrap().name, "4");
    }

    #[test]
    fn workspace_meta_from_ipc_preserves_named_workspace_ids_without_display_index() {
        let ipc = vec![json!({
            "id": -1337,
            "name": "name:web",
            "windows": 0,
            "monitor": "DP-1"
        })];

        let workspaces = HyprlandBackend::workspace_meta_from_ipc(&ipc);
        let web = workspaces.iter().find(|ws| ws.name == "web").unwrap();

        assert_eq!(web.id, -1337);
        assert_eq!(web.idx, -1);
        assert_eq!(
            HyprlandBackend::workspace_id_from_snapshot(&ipc[0]),
            Some(web.id)
        );
    }

    #[test]
    fn workspace_meta_from_ipc_preserves_numbered_workspace_index() {
        let ipc = vec![json!({
            "id": 4,
            "name": "Discord",
            "windows": 1,
            "monitor": "DP-1"
        })];

        let workspaces = HyprlandBackend::workspace_meta_from_ipc(&ipc);
        let discord = workspaces.iter().find(|ws| ws.id == 4).unwrap();

        assert_eq!(discord.name, "Discord");
        assert_eq!(discord.idx, 4);
    }

    #[test]
    fn workspace_meta_from_ipc_sorts_numbered_before_named_by_id() {
        let ipc = vec![
            json!({ "id": -1337, "name": "name:web" }),
            json!({ "id": 2, "name": "2" }),
            json!({ "id": -1338, "name": "name:chat" }),
        ];

        let workspaces = HyprlandBackend::workspace_meta_from_ipc(&ipc);
        let workspace_10_pos = workspaces.iter().position(|ws| ws.id == 10).unwrap();
        let first_named_pos = workspaces.iter().position(|ws| ws.id == -1337).unwrap();
        let second_named_pos = workspaces.iter().position(|ws| ws.id == -1338).unwrap();

        assert!(workspace_10_pos < first_named_pos);
        assert!(first_named_pos < second_named_pos);

        let named_ids: Vec<_> = workspaces
            .iter()
            .filter(|ws| ws.id < 0)
            .map(|ws| ws.id)
            .collect();
        assert_eq!(named_ids, vec![-1337, -1338]);
    }

    #[test]
    fn workspace_switch_target_formats_named_workspaces() {
        let backend = HyprlandBackend::new();
        *backend.shared.workspaces.write() = vec![WorkspaceMeta {
            id: -1337,
            idx: -1,
            name: "web".to_string(),
            output: None,
        }];

        assert_eq!(backend.workspace_switch_target(-1337), "name:web");
        assert_eq!(backend.workspace_switch_target(3), "3");
        assert_eq!(backend.workspace_switch_target(-9999), "-9999");
    }

    #[test]
    fn workspace_lua_target_quotes_named_workspaces() {
        let backend = HyprlandBackend::new();
        *backend.shared.workspaces.write() = vec![WorkspaceMeta {
            id: -1337,
            idx: -1,
            name: "web".to_string(),
            output: None,
        }];

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
    fn clear_urgent_workspace_removes_matching_workspace() {
        let backend = HyprlandBackend::new();
        backend
            .shared
            .workspace_snapshot
            .write()
            .urgent_workspaces
            .insert(4);

        assert!(backend.clear_urgent_workspace(4));
        assert!(
            !backend
                .shared
                .workspace_snapshot
                .read()
                .urgent_workspaces
                .contains(&4)
        );
    }

    #[test]
    fn activelayout_main_keyboard_updates_layout_from_event() {
        let backend = HyprlandBackend::new();
        *backend.shared.main_keyboard_name.write() = Some("keyboard-a".to_string());
        *backend.shared.keyboard_layout.write() = Some(KeyboardLayoutInfo {
            layout_name: "English (US)".to_string(),
            layout_count: Some(2),
        });

        let (_, _, keyboard_changed) = backend.handle_event("activelayout>>keyboard-a,German");

        assert!(keyboard_changed);
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
    fn build_state_tracks_named_workspaces() {
        let state = state(
            json!([{"id": 0, "name": "DP-1", "focused": true,
                    "activeWorkspace": {"id": -1337, "name": "web"}}]),
            json!([{"id": -1337, "name": "web", "monitor": "DP-1", "windows": 1}]),
        );

        assert_eq!(state.snapshot.active_workspace, HashSet::from([-1337]));
        assert_eq!(state.snapshot.occupied_workspaces, HashSet::from([-1337]));
        assert!(state.workspaces.iter().any(|ws| ws.name == "web"));
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

    #[test]
    fn apply_state_clears_urgency_when_workspace_becomes_active() {
        let backend = HyprlandBackend::new();
        let workspaces = json!([
            {"id": 1, "name": "1", "monitor": "DP-1", "windows": 1},
            {"id": 2, "name": "2", "monitor": "DP-1", "windows": 1}
        ]);
        backend.apply_state(state(two_monitors(1, 5), workspaces.clone()));
        backend
            .shared
            .workspace_snapshot
            .write()
            .urgent_workspaces
            .extend([1, 2]);

        // Urgency survives rebuilds that don't switch to the workspace.
        backend.apply_state(state(two_monitors(1, 5), workspaces.clone()));
        assert_eq!(
            backend.get_workspace_snapshot().urgent_workspaces,
            HashSet::from([1, 2])
        );

        assert!(backend.apply_state(state(two_monitors(2, 5), workspaces)));
        assert_eq!(
            backend.get_workspace_snapshot().urgent_workspaces,
            HashSet::from([1])
        );
    }

    #[test]
    fn apply_state_drops_urgency_for_empty_workspaces() {
        let backend = HyprlandBackend::new();
        backend
            .shared
            .workspace_snapshot
            .write()
            .urgent_workspaces
            .insert(3);

        backend.apply_state(state(
            two_monitors(1, 5),
            json!([{"id": 3, "name": "3", "monitor": "DP-1", "windows": 0}]),
        ));

        assert!(
            backend
                .get_workspace_snapshot()
                .urgent_workspaces
                .is_empty()
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

        let (_, window_changed, _) = backend.handle_event("focusedmon>>DP-1,2");
        assert!(window_changed);
        assert_eq!(backend.window_update().output, Some("DP-1".to_string()));
        assert!(backend.window_update().title.is_empty());

        *backend.shared.focused_window.write() = Some(WindowInfo {
            title: "t".to_string(),
            app_id: "a".to_string(),
            output: Some("DP-1".to_string()),
            focused: true,
        });
        let (_, window_changed, _) = backend.handle_event("focusedmon>>DP-1,2");
        assert!(!window_changed);
    }
}
