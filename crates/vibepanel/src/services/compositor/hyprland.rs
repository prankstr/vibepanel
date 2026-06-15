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

pub struct HyprlandBackend {
    allowed_outputs: RwLock<Vec<String>>,
    running: Arc<AtomicBool>,
    event_thread: Mutex<Option<JoinHandle<()>>>,
    socket_path: RwLock<Option<String>>,
    event_socket_path: RwLock<Option<String>>,
    workspace_snapshot: RwLock<WorkspaceSnapshot>,
    focused_window: RwLock<Option<WindowInfo>>,
    workspaces: Arc<RwLock<Vec<WorkspaceMeta>>>,
    callbacks: Mutex<Option<(WorkspaceCallback, WindowCallback)>>,
    monitor_workspaces: RwLock<HashMap<String, i32>>,
    focused_monitor: RwLock<Option<String>>,
    /// Callback for keyboard layout changes, set by CompositorManager.
    keyboard_layout_callback: Mutex<Option<KeyboardLayoutCallback>>,
    /// Current keyboard layout info.
    keyboard_layout: RwLock<Option<KeyboardLayoutInfo>>,
    /// Name of the main keyboard device (auto-detected from `hyprctl devices`).
    main_keyboard_name: RwLock<Option<String>>,
    /// Whether this Hyprland instance supports the Lua dispatch API.
    supports_lua_dispatch: AtomicBool,
}

impl HyprlandBackend {
    pub fn new(outputs: Option<Vec<String>>) -> Self {
        Self {
            allowed_outputs: RwLock::new(outputs.unwrap_or_default()),
            running: Arc::new(AtomicBool::new(false)),
            event_thread: Mutex::new(None),
            socket_path: RwLock::new(None),
            event_socket_path: RwLock::new(None),
            workspace_snapshot: RwLock::new(WorkspaceSnapshot::default()),
            focused_window: RwLock::new(None),
            workspaces: Arc::new(RwLock::new(Self::default_workspaces())),
            callbacks: Mutex::new(None),
            monitor_workspaces: RwLock::new(HashMap::new()),
            focused_monitor: RwLock::new(None),
            keyboard_layout_callback: Mutex::new(None),
            keyboard_layout: RwLock::new(None),
            main_keyboard_name: RwLock::new(None),
            supports_lua_dispatch: AtomicBool::new(false),
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

    /// Disable Hyprland's compositor-level layer animations for our popover surfaces.
    ///
    /// Hyprland animates layer surface resize/move by default which clashes
    /// with GTK4 animations and causes a visible content-shift glitch.
    ///
    /// Session-scoped, only set on startup and not persisted anywhere
    fn apply_layer_rules(&self) {
        let cmd = "keyword layerrule no_anim on, match:namespace ^vibepanel-.*-popover$";
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

    fn update_workspace_metadata(&self, workspaces: &[Value]) -> bool {
        let new_workspaces = Self::workspace_meta_from_ipc(workspaces);
        let mut current = self.workspaces.write();
        if *current == new_workspaces {
            return false;
        }
        *current = new_workspaces;
        true
    }

    fn workspace_id_from_snapshot(workspace: &Value) -> Option<i32> {
        let raw_name = workspace.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let id = workspace.get("id").and_then(|v| v.as_i64());
        Self::workspace_identity(id, raw_name).map(|(id, _)| id)
    }

    fn workspace_id_for_name(&self, name: &str) -> Option<i32> {
        self.workspaces
            .read()
            .iter()
            .find(|ws| ws.name == name)
            .map(|ws| ws.id)
    }

    fn has_workspace_metadata(&self, workspace_id: i32) -> bool {
        self.workspaces
            .read()
            .iter()
            .any(|ws| ws.id == workspace_id)
    }

    fn workspace_switch_target(&self, workspace_id: i32) -> String {
        if workspace_id < 0 {
            let workspaces = self.workspaces.read();
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

    /// Fetch monitor information from Hyprland.
    ///
    /// Updates `monitor_workspaces` with each monitor's active workspace,
    /// and `focused_monitor` with the currently focused monitor name.
    fn fetch_monitors(&self) {
        if let Some(monitors) = self.query_json("monitors")
            && let Some(monitors) = monitors.as_array()
        {
            let mut monitor_ws = self.monitor_workspaces.write();
            let mut focused_mon = self.focused_monitor.write();
            monitor_ws.clear();
            *focused_mon = None; // Reset before iterating to avoid stale state

            for mon in monitors {
                let name = mon.get("name").and_then(|v| v.as_str());
                let active_ws_id = mon
                    .get("activeWorkspace")
                    .and_then(Self::workspace_id_from_snapshot);
                let is_focused = mon
                    .get("focused")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                if let (Some(name), Some(ws_id)) = (name, active_ws_id) {
                    monitor_ws.insert(name.to_string(), ws_id);
                    if is_focused {
                        *focused_mon = Some(name.to_string());
                    }
                }
            }

            trace!(
                "fetch_monitors: {} monitors, focused={:?}",
                monitor_ws.len(),
                *focused_mon
            );
        } else {
            warn!("fetch_monitors: failed to query monitors from Hyprland");
        }
    }

    /// Fetch initial state from Hyprland.
    fn fetch_initial_state(&self) {
        // Fetch monitors first to know per-output active workspaces
        self.fetch_monitors();

        // Fetch workspaces (occupied state and window counts)
        if let Some(workspaces) = self.query_json("workspaces")
            && let Some(workspaces) = workspaces.as_array()
        {
            self.update_workspace_metadata(workspaces);

            let mut snapshot = self.workspace_snapshot.write();
            let monitor_ws = self.monitor_workspaces.read();
            let focused_mon = self.focused_monitor.read();

            snapshot.occupied_workspaces.clear();
            snapshot.window_counts.clear();
            snapshot.per_output.clear();

            // Initialize per_output entries for all known monitors
            for (mon_name, &active_ws) in monitor_ws.iter() {
                let per_output = snapshot.per_output.entry(mon_name.clone()).or_default();
                per_output.active_workspace.insert(active_ws);
            }

            for ws in workspaces {
                let id = Self::workspace_id_from_snapshot(ws);
                let windows = ws.get("windows").and_then(|v| v.as_i64());
                let monitor = ws.get("monitor").and_then(|v| v.as_str());

                if let (Some(id), Some(windows)) = (id, windows) {
                    let windows = windows as u32;

                    // Update global state
                    snapshot.window_counts.insert(id, windows);
                    if windows > 0 {
                        snapshot.occupied_workspaces.insert(id);
                    }

                    // Update per-output state
                    if let Some(mon_name) = monitor {
                        let per_output =
                            snapshot.per_output.entry(mon_name.to_string()).or_default();
                        per_output.window_counts.insert(id, windows);
                        if windows > 0 {
                            per_output.occupied_workspaces.insert(id);
                        }
                    }
                }
            }

            // Set global active workspace from focused monitor
            // This should always succeed on initial fetch since we just queried monitors
            if let Some(ref focused) = *focused_mon
                && let Some(&active_ws) = monitor_ws.get(focused)
            {
                snapshot.active_workspace.clear();
                snapshot.active_workspace.insert(active_ws);
            }
        }

        // Fetch active window (including its monitor)
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

        *self.main_keyboard_name.write() = Some(kb_name);
        *self.keyboard_layout.write() = Some(KeyboardLayoutInfo {
            layout_name: active_layout,
            short_name: String::new(), // Widget extracts short name
            layout_count,
        });
    }

    /// Refresh occupied workspaces and window counts from Hyprland.
    ///
    /// Also updates per-output state and monitor tracking.
    /// Returns true if occupied workspaces OR active workspace changed.
    fn refresh_occupied(&self) -> bool {
        // Refresh monitors first to get current per-output active workspaces
        self.fetch_monitors();

        if let Some(workspaces) = self.query_json("workspaces")
            && let Some(workspaces) = workspaces.as_array()
        {
            let metadata_changed = self.update_workspace_metadata(workspaces);

            let mut snapshot = self.workspace_snapshot.write();
            let monitor_ws = self.monitor_workspaces.read();
            let focused_mon = self.focused_monitor.read();

            // Track previous state to detect changes
            let previous_active = snapshot.active_workspace.clone();
            let old_occupied = snapshot.occupied_workspaces.clone();
            let old_urgent = snapshot.urgent_workspaces.clone();
            let old_per_output = snapshot.per_output.clone();

            snapshot.occupied_workspaces.clear();
            snapshot.window_counts.clear();
            snapshot.per_output.clear();

            // Initialize per_output entries for all known monitors
            for (mon_name, &active_ws) in monitor_ws.iter() {
                let per_output = snapshot.per_output.entry(mon_name.clone()).or_default();
                per_output.active_workspace.insert(active_ws);
            }

            for ws in workspaces {
                let id = Self::workspace_id_from_snapshot(ws);
                let windows = ws.get("windows").and_then(|v| v.as_i64());
                let monitor = ws.get("monitor").and_then(|v| v.as_str());

                if let (Some(id), Some(windows)) = (id, windows) {
                    let windows = windows as u32;

                    // Update global state
                    snapshot.window_counts.insert(id, windows);
                    if windows > 0 {
                        snapshot.occupied_workspaces.insert(id);
                    }

                    // Update per-output state
                    if let Some(mon_name) = monitor {
                        let per_output =
                            snapshot.per_output.entry(mon_name.to_string()).or_default();
                        per_output.window_counts.insert(id, windows);
                        if windows > 0 {
                            per_output.occupied_workspaces.insert(id);
                        }
                    }
                }
            }

            // Set global active workspace from focused monitor, or preserve previous
            // if monitor lookup fails (e.g., during rapid workspace switches)
            if let Some(ref focused) = *focused_mon
                && let Some(&active_ws) = monitor_ws.get(focused)
            {
                snapshot.active_workspace.clear();
                snapshot.active_workspace.insert(active_ws);
            } else if snapshot.active_workspace.is_empty() {
                // Restore previous active workspace if we couldn't determine current
                snapshot.active_workspace = previous_active.clone();
            }

            let occupied_changed = snapshot.occupied_workspaces != old_occupied;
            let active_changed = snapshot.active_workspace != previous_active;
            let per_output_changed = snapshot.per_output != old_per_output;

            let occupied_workspaces = snapshot.occupied_workspaces.clone();
            snapshot
                .urgent_workspaces
                .retain(|ws_id| occupied_workspaces.contains(ws_id));
            let urgent_changed = snapshot.urgent_workspaces != old_urgent;

            if metadata_changed
                || occupied_changed
                || active_changed
                || per_output_changed
                || urgent_changed
            {
                trace!(
                    "refresh_occupied: metadata_changed={}, occupied_changed={}, active_changed={} ({:?} -> {:?}), per_output_changed={}, urgent_changed={}",
                    metadata_changed,
                    occupied_changed,
                    active_changed,
                    previous_active,
                    snapshot.active_workspace,
                    per_output_changed,
                    urgent_changed
                );
            }

            return metadata_changed
                || occupied_changed
                || active_changed
                || per_output_changed
                || urgent_changed;
        }
        false
    }

    fn clear_urgent_workspace(&self, ws_id: i32) -> bool {
        // Hyprland exposes urgency as an event only, not in clients/workspaces JSON.
        // Treat viewing the workspace as acknowledging workspace-level urgency.
        // TODO: When Hyprland list_windows support lands, replace workspace-level
        // urgency with address-based tracking so multiple urgent clients on the same
        // workspace survive focusing/viewing only one client.
        self.workspace_snapshot
            .write()
            .urgent_workspaces
            .remove(&ws_id)
    }

    /// Update active workspace for the focused monitor.
    ///
    /// Called when workspace/workspacev2 events fire. Updates:
    /// - Global `active_workspace`
    /// - `monitor_workspaces` for the focused monitor
    /// - `per_output[focused_monitor].active_workspace`
    ///
    /// Returns true if state changed.
    fn update_active_workspace(&self, ws_id: i32) -> bool {
        let focused_mon = self.focused_monitor.read().clone();

        let mut snapshot = self.workspace_snapshot.write();
        let old_active = snapshot.active_workspace.clone();
        // Changed if: (a) new workspace wasn't already active, or (b) multiple were active
        let changed = !old_active.contains(&ws_id) || old_active.len() != 1;

        trace!(
            "update_active_workspace: ws_id={}, old_active={:?}, focused_mon={:?}, changed={}",
            ws_id, old_active, focused_mon, changed
        );

        if changed {
            snapshot.active_workspace.clear();
            snapshot.active_workspace.insert(ws_id);

            // Update per-monitor tracking to stay in sync
            if let Some(ref mon_name) = focused_mon {
                // Update monitor_workspaces so focusedmon events see correct state
                self.monitor_workspaces
                    .write()
                    .insert(mon_name.clone(), ws_id);

                // Update per_output active workspace (create entry if needed)
                let per_output = snapshot.per_output.entry(mon_name.clone()).or_default();
                per_output.active_workspace.clear();
                per_output.active_workspace.insert(ws_id);
            } else {
                warn!(
                    "update_active_workspace: focused_mon is None, per_output NOT updated! \
                     Global active_workspace set to {}, but per_output entries unchanged.",
                    ws_id
                );
            }
        }

        changed
    }

    fn workspace_id_from_event_name(&self, workspace_name: &str) -> Option<i32> {
        let name = workspace_name
            .strip_prefix("name:")
            .unwrap_or(workspace_name);

        name.parse::<i64>()
            .ok()
            .and_then(|id| i32::try_from(id).ok())
            .or_else(|| self.workspace_id_for_name(name))
    }

    /// Refresh active window info from Hyprland.
    ///
    /// Queries `activewindow` JSON and updates `focused_window`.
    /// Returns whether the window info changed and the freshly queried workspace ID.
    fn refresh_active_window(&self) -> (bool, Option<i32>) {
        if let Some(active_window) = self.query_json("activewindow") {
            let title = active_window
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let app_id = active_window
                .get("class")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let workspace_id = active_window
                .get("workspace")
                .and_then(Self::workspace_id_from_snapshot);
            let output = active_window
                .get("monitor")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let new_focused = WindowInfo {
                title,
                app_id,
                workspace_id,
                output,
            };

            let mut focused = self.focused_window.write();
            if focused.as_ref() != Some(&new_focused) {
                *focused = Some(new_focused);
                return (true, workspace_id);
            }

            return (false, workspace_id);
        }
        (false, None)
    }

    /// Handle a Hyprland event line.
    /// Returns (workspace_changed, window_changed, keyboard_layout_changed).
    fn handle_event(&self, line: &str) -> (bool, bool, bool) {
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
            "workspace" => {
                // workspace>>ID or workspace>>NAME
                if let Some(ws_id) = self.workspace_id_from_event_name(data) {
                    if !self.has_workspace_metadata(ws_id) {
                        workspace_changed |= self.refresh_occupied();
                    }
                    workspace_changed |= self.update_active_workspace(ws_id);
                    workspace_changed |= self.clear_urgent_workspace(ws_id);
                } else {
                    // Named workspace - refetch state
                    debug!(
                        "workspace event: named workspace '{}', refetching state",
                        data
                    );
                    self.fetch_initial_state();
                    workspace_changed = true;
                }
            }
            "workspacev2" => {
                // workspacev2>>ID,NAME
                if let Some((id_str, name)) = data.split_once(',')
                    && let Some(ws_id) = self
                        .workspace_id_from_event_name(id_str)
                        .or_else(|| self.workspace_id_from_event_name(name))
                {
                    if !self.has_workspace_metadata(ws_id) {
                        workspace_changed |= self.refresh_occupied();
                    }
                    workspace_changed |= self.update_active_workspace(ws_id);
                    workspace_changed |= self.clear_urgent_workspace(ws_id);
                }
            }
            "createworkspace" | "createworkspacev2" | "destroyworkspace" | "destroyworkspacev2"
            | "renameworkspace" | "closewindow" | "movewindow" => {
                workspace_changed = self.refresh_occupied();
            }
            "openwindow" => {
                // openwindow>>ADDRESS,WORKSPACE,CLASS,TITLE
                workspace_changed = self.refresh_occupied();
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
                                let mut snapshot = self.workspace_snapshot.write();
                                snapshot.urgent_workspaces.insert(ws_id);
                                workspace_changed = true;
                            }
                            break;
                        }
                    }
                }
            }
            "activewindow" => {
                // activewindow>>CLASS,TITLE
                // Query full window info from Hyprland for consistency
                let (changed, workspace_id) = self.refresh_active_window();
                window_changed = changed;
                if let Some(ws_id) = workspace_id {
                    workspace_changed |= self.clear_urgent_workspace(ws_id);
                }
            }
            "activewindowv2" => {
                // activewindowv2>>ADDRESS
                // Query the window info from Hyprland
                let (changed, workspace_id) = self.refresh_active_window();
                window_changed = changed;
                if let Some(ws_id) = workspace_id {
                    workspace_changed |= self.clear_urgent_workspace(ws_id);
                }
            }
            "focusedmon" => {
                // focusedmon>>MONNAME,WORKSPACENAME
                // Update focused monitor and global active workspace
                if let Some((mon_name, ws_name)) = data.split_once(',') {
                    *self.focused_monitor.write() = Some(mon_name.to_string());

                    // Update global active_workspace to this monitor's active workspace
                    let cached_ws_id = self.monitor_workspaces.read().get(mon_name).copied();
                    let event_ws_id = self.workspace_id_from_event_name(ws_name);

                    if let Some(ws_id) = event_ws_id.or(cached_ws_id) {
                        self.monitor_workspaces
                            .write()
                            .insert(mon_name.to_string(), ws_id);

                        let mut snapshot = self.workspace_snapshot.write();
                        if !snapshot.active_workspace.contains(&ws_id)
                            || snapshot.active_workspace.len() != 1
                        {
                            snapshot.active_workspace.clear();
                            snapshot.active_workspace.insert(ws_id);
                            // Also update per_output active workspace.
                            let per_output =
                                snapshot.per_output.entry(mon_name.to_string()).or_default();
                            per_output.active_workspace.clear();
                            per_output.active_workspace.insert(ws_id);
                            workspace_changed = true;
                        }
                        workspace_changed |= snapshot.urgent_workspaces.remove(&ws_id);
                    } else {
                        workspace_changed = self.refresh_occupied();
                    }
                }
            }
            "moveworkspace" | "moveworkspacev2" => {
                // Workspace moved to different monitor - refresh all state
                workspace_changed = self.refresh_occupied();
            }
            "activelayout" => {
                // activelayout>>KEYBOARD_NAME,LAYOUT_NAME
                // Only process events from the main keyboard
                if let Some((kb_name, layout_name)) = data.split_once(',') {
                    let is_main = self
                        .main_keyboard_name
                        .read()
                        .as_ref()
                        .is_some_and(|name| name == kb_name);

                    if is_main {
                        let info = KeyboardLayoutInfo {
                            layout_name: layout_name.to_string(),
                            short_name: String::new(), // Widget extracts short name
                            layout_count: self
                                .keyboard_layout
                                .read()
                                .as_ref()
                                .and_then(|i| i.layout_count),
                        };
                        *self.keyboard_layout.write() = Some(info);
                        keyboard_layout_changed = true;
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
            ws_cb(backend.workspace_snapshot.read().clone());
            if let Some(ref win) = *backend.focused_window.read() {
                win_cb(win.clone());
            }
        }
        // Emit initial keyboard layout
        if let Some(ref kb_cb) = *backend
            .keyboard_layout_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            && let Some(ref info) = *backend.keyboard_layout.read()
        {
            kb_cb(info.clone());
        }

        // Exponential backoff state
        let mut backoff_ms = RECONNECT_INITIAL_MS;

        while backend.running.load(Ordering::SeqCst) {
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
                                ws_cb(backend.workspace_snapshot.read().clone());
                            }
                            if win_changed && let Some(ref win) = *backend.focused_window.read() {
                                win_cb(win.clone());
                            }
                        }

                        if kb_changed
                            && let Some(ref kb_cb) = *backend
                                .keyboard_layout_callback
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                            && let Some(ref info) = *backend.keyboard_layout.read()
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

        // Disable compositor-level layer animations for our surfaces.
        self.apply_layer_rules();

        self.probe_lua_dispatch_support();

        // Store callbacks
        *self.callbacks.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((on_workspace_update, on_window_update));

        // Clone the socket paths for the thread
        let socket_path = self.socket_path.read().clone();
        let event_socket_path = self.event_socket_path.read().clone();
        let callbacks = self
            .callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let allowed_outputs = self.allowed_outputs.read().clone();
        // The manager-side backend uses this metadata for named workspace switching.
        let workspaces = Arc::clone(&self.workspaces);
        let kb_callback = self
            .keyboard_layout_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        // Share the running flag with the thread so stop() works correctly
        let running = Arc::clone(&self.running);

        // Create Arc for shared access in thread
        // Note: This is a separate instance for the thread, but socket_path is now
        // also set on `self` so switch_workspace() works correctly.
        // The `running` flag is shared so stop() can signal the thread to exit.
        let backend = Arc::new(HyprlandBackend {
            allowed_outputs: RwLock::new(allowed_outputs),
            running,
            event_thread: Mutex::new(None),
            socket_path: RwLock::new(socket_path),
            event_socket_path: RwLock::new(event_socket_path),
            workspace_snapshot: RwLock::new(WorkspaceSnapshot::default()),
            focused_window: RwLock::new(None),
            workspaces,
            callbacks: Mutex::new(callbacks),
            monitor_workspaces: RwLock::new(HashMap::new()),
            focused_monitor: RwLock::new(None),
            keyboard_layout_callback: Mutex::new(kb_callback),
            keyboard_layout: RwLock::new(None),
            main_keyboard_name: RwLock::new(None),
            supports_lua_dispatch: AtomicBool::new(
                self.supports_lua_dispatch.load(Ordering::Relaxed),
            ),
        });

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
        self.workspaces.read().clone()
    }

    fn get_workspace_snapshot(&self) -> WorkspaceSnapshot {
        self.workspace_snapshot.read().clone()
    }

    fn get_focused_window(&self) -> Option<WindowInfo> {
        self.focused_window.read().clone()
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
        self.keyboard_layout.read().clone()
    }

    fn switch_keyboard_layout_next(&self) {
        let main_kb = self.main_keyboard_name.read().clone();
        if let Some(kb_name) = main_kb {
            debug!("Switching keyboard layout for '{}'", kb_name);
            let _ = self.send_command(&format!("switchxkblayout {} next", kb_name));
        } else {
            // Fallback: try "all" which switches all keyboards
            debug!("No main keyboard detected, switching all keyboards");
            let _ = self.send_command("switchxkblayout all next");
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

// Implement PartialEq for WindowInfo for comparison
impl PartialEq for WindowInfo {
    fn eq(&self, other: &Self) -> bool {
        self.title == other.title
            && self.app_id == other.app_id
            && self.workspace_id == other.workspace_id
            && self.output == other.output
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
    fn workspace_id_for_name_finds_named_workspace_id() {
        let backend = HyprlandBackend::new(None);
        *backend.workspaces.write() = vec![WorkspaceMeta {
            id: -1337,
            idx: -1,
            name: "web".to_string(),
            output: None,
        }];

        assert_eq!(backend.workspace_id_for_name("web"), Some(-1337));
        assert_eq!(backend.workspace_id_for_name("chat"), None);
    }

    #[test]
    fn workspace_switch_target_formats_named_workspaces() {
        let backend = HyprlandBackend::new(None);
        *backend.workspaces.write() = vec![WorkspaceMeta {
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
        let backend = HyprlandBackend::new(None);
        *backend.workspaces.write() = vec![WorkspaceMeta {
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
    fn has_workspace_metadata_checks_workspace_id() {
        let backend = HyprlandBackend::new(None);
        *backend.workspaces.write() = vec![WorkspaceMeta {
            id: -1337,
            idx: -1,
            name: "web".to_string(),
            output: None,
        }];

        assert!(backend.has_workspace_metadata(-1337));
        assert!(!backend.has_workspace_metadata(-1338));
    }

    #[test]
    fn focusedmon_uses_event_workspace_name_when_cache_missing() {
        let backend = HyprlandBackend::new(None);
        *backend.workspaces.write() = vec![WorkspaceMeta {
            id: -1337,
            idx: -1,
            name: "web".to_string(),
            output: None,
        }];

        backend.handle_event("focusedmon>>DP-1,name:web");

        let snapshot = backend.workspace_snapshot.read();
        assert_eq!(snapshot.active_workspace, HashSet::from([-1337]));
        assert_eq!(
            snapshot.per_output.get("DP-1").unwrap().active_workspace,
            HashSet::from([-1337])
        );
        assert_eq!(*backend.focused_monitor.read(), Some("DP-1".to_string()));
        assert_eq!(backend.monitor_workspaces.read().get("DP-1"), Some(&-1337));
    }

    #[test]
    fn focusedmon_prefers_event_workspace_name_over_stale_cache() {
        let backend = HyprlandBackend::new(None);
        *backend.workspaces.write() = vec![WorkspaceMeta {
            id: -1337,
            idx: -1,
            name: "web".to_string(),
            output: None,
        }];
        backend
            .monitor_workspaces
            .write()
            .insert("DP-1".to_string(), 1);

        backend.handle_event("focusedmon>>DP-1,name:web");

        let snapshot = backend.workspace_snapshot.read();
        assert_eq!(snapshot.active_workspace, HashSet::from([-1337]));
        assert_eq!(backend.monitor_workspaces.read().get("DP-1"), Some(&-1337));
    }

    #[test]
    fn workspace_event_resolves_prefixed_named_workspace() {
        let backend = HyprlandBackend::new(None);
        *backend.workspaces.write() = vec![WorkspaceMeta {
            id: -1337,
            idx: -1,
            name: "web".to_string(),
            output: None,
        }];

        backend.handle_event("workspace>>name:web");

        let snapshot = backend.workspace_snapshot.read();
        assert_eq!(snapshot.active_workspace, HashSet::from([-1337]));
    }

    #[test]
    fn workspace_event_clears_matching_urgent_workspace() {
        let backend = HyprlandBackend::new(None);
        *backend.workspaces.write() = vec![WorkspaceMeta {
            id: 2,
            idx: 2,
            name: "2".to_string(),
            output: None,
        }];
        backend
            .workspace_snapshot
            .write()
            .urgent_workspaces
            .insert(2);

        let (workspace_changed, _, _) = backend.handle_event("workspace>>2");

        let snapshot = backend.workspace_snapshot.read();
        assert!(workspace_changed);
        assert_eq!(snapshot.active_workspace, HashSet::from([2]));
        assert!(!snapshot.urgent_workspaces.contains(&2));
    }

    #[test]
    fn clear_urgent_workspace_removes_matching_workspace() {
        let backend = HyprlandBackend::new(None);
        backend
            .workspace_snapshot
            .write()
            .urgent_workspaces
            .insert(4);

        assert!(backend.clear_urgent_workspace(4));
        assert!(
            !backend
                .workspace_snapshot
                .read()
                .urgent_workspaces
                .contains(&4)
        );
    }

    #[test]
    fn workspacev2_event_resolves_prefixed_named_workspace() {
        let backend = HyprlandBackend::new(None);
        *backend.workspaces.write() = vec![WorkspaceMeta {
            id: -1337,
            idx: -1,
            name: "web".to_string(),
            output: None,
        }];

        backend.handle_event("workspacev2>>-1337,name:web");

        let snapshot = backend.workspace_snapshot.read();
        assert_eq!(snapshot.active_workspace, HashSet::from([-1337]));
    }
}
