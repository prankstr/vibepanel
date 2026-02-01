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
use tracing::{debug, error, trace, warn};

use super::{
    CompositorBackend, WindowCallback, WindowInfo, WorkspaceCallback, WorkspaceMeta,
    WorkspaceSnapshot,
};

/// Default workspaces for Hyprland (dynamic workspaces, but we expose 1-10).
const DEFAULT_WORKSPACE_COUNT: i32 = 10;

const RECONNECT_INITIAL_MS: u64 = 1000;
const RECONNECT_MAX_MS: u64 = 30000;
const RECONNECT_MULTIPLIER: f64 = 1.5;

pub struct HyprlandBackend {
    allowed_outputs: RwLock<Vec<String>>,
    running: Arc<AtomicBool>,
    event_thread: Mutex<Option<JoinHandle<()>>>,
    socket_path: RwLock<Option<String>>,
    event_socket_path: RwLock<Option<String>>,
    workspace_snapshot: RwLock<WorkspaceSnapshot>,
    focused_window: RwLock<Option<WindowInfo>>,
    workspaces: RwLock<Vec<WorkspaceMeta>>,
    callbacks: Mutex<Option<(WorkspaceCallback, WindowCallback)>>,
    monitor_workspaces: RwLock<HashMap<String, i32>>,
    focused_monitor: RwLock<Option<String>>,
}

impl HyprlandBackend {
    pub fn new(outputs: Option<Vec<String>>) -> Self {
        // Pre-generate workspace metadata (Hyprland uses dynamic workspaces,
        // but we expose 1-10 for consistent UI)
        let workspaces: Vec<WorkspaceMeta> = (1..=DEFAULT_WORKSPACE_COUNT)
            .map(|i| WorkspaceMeta {
                id: i,
                name: i.to_string(),
                output: None, // Hyprland workspaces are global
            })
            .collect();

        Self {
            allowed_outputs: RwLock::new(outputs.unwrap_or_default()),
            running: Arc::new(AtomicBool::new(false)),
            event_thread: Mutex::new(None),
            socket_path: RwLock::new(None),
            event_socket_path: RwLock::new(None),
            workspace_snapshot: RwLock::new(WorkspaceSnapshot::default()),
            focused_window: RwLock::new(None),
            workspaces: RwLock::new(workspaces),
            callbacks: Mutex::new(None),
            monitor_workspaces: RwLock::new(HashMap::new()),
            focused_monitor: RwLock::new(None),
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

    /// Send a command to Hyprland and get the response.
    fn send_command(&self, command: &str) -> Option<String> {
        let socket_path = self.socket_path.read();
        let socket_path = socket_path.as_ref()?;

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
                    .and_then(|ws| ws.get("id"))
                    .and_then(|v| v.as_i64());
                let is_focused = mon
                    .get("focused")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                if let (Some(name), Some(ws_id)) = (name, active_ws_id) {
                    monitor_ws.insert(name.to_string(), ws_id as i32);
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
                let id = ws.get("id").and_then(|v| v.as_i64());
                let windows = ws.get("windows").and_then(|v| v.as_i64());
                let monitor = ws.get("monitor").and_then(|v| v.as_str());

                if let (Some(id), Some(windows)) = (id, windows)
                    && id > 0
                {
                    let id = id as i32;
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
                snapshot.active_workspace.insert(active_ws);
            }
        }

        // Fetch active window (including its monitor)
        self.refresh_active_window();

        debug!("Fetched initial Hyprland state");
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
            let mut snapshot = self.workspace_snapshot.write();
            let monitor_ws = self.monitor_workspaces.read();
            let focused_mon = self.focused_monitor.read();

            // Track previous state to detect changes
            let previous_active = snapshot.active_workspace.clone();
            let old_occupied = snapshot.occupied_workspaces.clone();

            snapshot.occupied_workspaces.clear();
            snapshot.window_counts.clear();
            snapshot.per_output.clear();

            // Initialize per_output entries for all known monitors
            for (mon_name, &active_ws) in monitor_ws.iter() {
                let per_output = snapshot.per_output.entry(mon_name.clone()).or_default();
                per_output.active_workspace.insert(active_ws);
            }

            for ws in workspaces {
                let id = ws.get("id").and_then(|v| v.as_i64());
                let windows = ws.get("windows").and_then(|v| v.as_i64());
                let monitor = ws.get("monitor").and_then(|v| v.as_str());

                if let (Some(id), Some(windows)) = (id, windows)
                    && id > 0
                {
                    let id = id as i32;
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

            if occupied_changed || active_changed {
                trace!(
                    "refresh_occupied: occupied_changed={}, active_changed={} ({:?} -> {:?})",
                    occupied_changed, active_changed, previous_active, snapshot.active_workspace
                );
            }

            return occupied_changed || active_changed;
        }
        false
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

    /// Refresh active window info from Hyprland.
    ///
    /// Queries `activewindow` JSON and updates `focused_window`.
    /// Returns true if the window info changed.
    fn refresh_active_window(&self) -> bool {
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
                .and_then(|ws| ws.get("id"))
                .and_then(|v| v.as_i64())
                .map(|id| id as i32);
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
                return true;
            }
        }
        false
    }

    /// Handle a Hyprland event line.
    /// Returns (workspace_changed, window_changed).
    fn handle_event(&self, line: &str) -> (bool, bool) {
        let Some((event, data)) = line.split_once(">>") else {
            return (false, false);
        };

        trace!(
            "Hyprland event: {}>>{}...",
            event,
            &data[..data.len().min(50)]
        );

        let mut workspace_changed = false;
        let mut window_changed = false;

        match event {
            "workspace" => {
                // workspace>>ID or workspace>>NAME
                if let Ok(ws_id) = data.parse::<i32>() {
                    workspace_changed = self.update_active_workspace(ws_id);
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
                if let Some(id_str) = data.split(',').next()
                    && let Ok(ws_id) = id_str.parse::<i32>()
                {
                    workspace_changed = self.update_active_workspace(ws_id);
                }
            }
            "createworkspace" | "destroyworkspace" | "closewindow" | "movewindow" => {
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
                                && let Some(ws_id) = ws.get("id").and_then(|v| v.as_i64())
                                && ws_id > 0
                            {
                                let mut snapshot = self.workspace_snapshot.write();
                                snapshot.urgent_workspaces.insert(ws_id as i32);
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
                window_changed = self.refresh_active_window();
            }
            "activewindowv2" => {
                // activewindowv2>>ADDRESS
                // Query the window info from Hyprland
                window_changed = self.refresh_active_window();
            }
            "focusedmon" => {
                // focusedmon>>MONNAME,WORKSPACENAME
                // Update focused monitor and global active workspace
                if let Some((mon_name, _ws_name)) = data.split_once(',') {
                    *self.focused_monitor.write() = Some(mon_name.to_string());

                    // Update global active_workspace to this monitor's active workspace
                    let monitor_ws = self.monitor_workspaces.read();
                    if let Some(&ws_id) = monitor_ws.get(mon_name) {
                        let mut snapshot = self.workspace_snapshot.write();
                        if !snapshot.active_workspace.contains(&ws_id)
                            || snapshot.active_workspace.len() != 1
                        {
                            snapshot.active_workspace.clear();
                            snapshot.active_workspace.insert(ws_id);
                            // Also update per_output active workspace
                            if let Some(per_output) = snapshot.per_output.get_mut(mon_name) {
                                per_output.active_workspace.clear();
                                per_output.active_workspace.insert(ws_id);
                            }
                            workspace_changed = true;
                        }
                    }
                }
            }
            "moveworkspace" | "moveworkspacev2" => {
                // Workspace moved to different monitor - refresh all state
                workspace_changed = self.refresh_occupied();
            }
            _ => {}
        }

        (workspace_changed, window_changed)
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
                        let (ws_changed, win_changed) = backend.handle_event(&line);

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
        let workspaces = self.workspaces.read().clone();

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
            workspaces: RwLock::new(workspaces),
            callbacks: Mutex::new(callbacks),
            monitor_workspaces: RwLock::new(HashMap::new()),
            focused_monitor: RwLock::new(None),
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
        let _ = self.send_command(&format!("dispatch workspace {}", workspace_id));
    }

    fn quit_compositor(&self) {
        debug!("Sending exit command to Hyprland");
        let _ = self.send_command("dispatch exit");
    }

    fn name(&self) -> &'static str {
        "Hyprland"
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
