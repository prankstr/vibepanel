//! Niri compositor backend using native socket IPC.
//!
//! This backend communicates with Niri via its Unix socket at $NIRI_SOCKET.
//! Protocol: JSON request/response, with event streaming support.
//!
//! Provides both workspace and window title functionality through a single
//! event stream connection.
//!
//! Reference: https://github.com/YaLTeR/niri/wiki/IPC

use std::collections::HashMap;
use std::env;
use std::io::{BufRead, BufReader, Write};
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

const RECONNECT_INITIAL_MS: u64 = 1000;
const RECONNECT_MAX_MS: u64 = 30000;
const RECONNECT_MULTIPLIER: f64 = 1.5;

struct SharedState {
    workspace_snapshot: RwLock<WorkspaceSnapshot>,
    focused_window: RwLock<Option<WindowInfo>>,
    workspaces: RwLock<Vec<WorkspaceMeta>>,
    /// Map from Niri's u64 workspace ID to our 1-based index.
    id_to_idx: RwLock<HashMap<u64, i32>>,
    /// Map from Niri's u64 workspace ID to output name.
    id_to_output: RwLock<HashMap<u64, String>>,
    windows: RwLock<HashMap<u64, WindowData>>,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            workspace_snapshot: RwLock::new(WorkspaceSnapshot::default()),
            focused_window: RwLock::new(None),
            workspaces: RwLock::new(Vec::new()),
            id_to_idx: RwLock::new(HashMap::new()),
            id_to_output: RwLock::new(HashMap::new()),
            windows: RwLock::new(HashMap::new()),
        }
    }
}

pub struct NiriBackend {
    #[allow(dead_code)] // For future filtering support
    allowed_outputs: Vec<String>,
    running: Arc<AtomicBool>,
    event_thread: Mutex<Option<JoinHandle<()>>>,
    socket_path: RwLock<Option<String>>,
    shared: Arc<SharedState>,
    callbacks: Mutex<Option<(WorkspaceCallback, WindowCallback)>>,
}

#[derive(Debug, Clone)]
struct WindowData {
    id: u64,
    title: String,
    app_id: String,
    workspace_id: Option<u64>,
    is_focused: bool,
}

impl NiriBackend {
    pub fn new(outputs: Option<Vec<String>>) -> Self {
        Self {
            allowed_outputs: outputs.unwrap_or_default(),
            running: Arc::new(AtomicBool::new(false)),
            event_thread: Mutex::new(None),
            socket_path: RwLock::new(None),
            shared: Arc::new(SharedState::default()),
            callbacks: Mutex::new(None),
        }
    }

    /// Send a JSON request to Niri and get the response.
    fn send_request(&self, request: &Value) -> Option<Value> {
        let socket_path = self.socket_path.read();
        let socket_path = socket_path.as_ref()?;
        Self::send_request_static(socket_path, request)
    }

    /// Send a JSON request to Niri (static version for use without &self).
    fn send_request_static(socket_path: &str, request: &Value) -> Option<Value> {
        let mut stream = match UnixStream::connect(socket_path) {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to connect to Niri socket: {}", e);
                return None;
            }
        };

        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

        let message = format!("{}\n", serde_json::to_string(request).ok()?);
        if let Err(e) = stream.write_all(message.as_bytes()) {
            error!("Failed to send request to Niri: {}", e);
            return None;
        }

        // Shutdown write side to signal end of request
        let _ = stream.shutdown(std::net::Shutdown::Write);

        let mut response = String::new();
        let mut reader = BufReader::new(stream);
        if let Err(e) = reader.read_line(&mut response) {
            error!("Failed to read Niri response: {}", e);
            return None;
        }

        match serde_json::from_str(&response) {
            Ok(v) => Some(v),
            Err(e) => {
                trace!("Failed to parse JSON from Niri: {}", e);
                None
            }
        }
    }

    /// Process workspace list and update internal state.
    fn process_workspaces(shared: &SharedState, workspaces: &[Value]) {
        let mut ws_list = shared.workspaces.write();
        let mut id_map = shared.id_to_idx.write();
        let mut id_to_output = shared.id_to_output.write();
        let mut snapshot = shared.workspace_snapshot.write();

        ws_list.clear();
        id_map.clear();
        id_to_output.clear();
        snapshot.occupied_workspaces.clear();
        snapshot.urgent_workspaces.clear();
        snapshot.window_counts.clear();
        snapshot.active_workspace.clear();
        snapshot.per_output.clear();

        for ws in workspaces {
            let Some(ws_id) = ws.get("id").and_then(|v| v.as_u64()) else {
                continue;
            };
            let idx = ws.get("idx").and_then(|v| v.as_i64()).unwrap_or(1) as i32;
            let name = ws
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| idx.to_string());

            // Get output name (Niri workspaces are per-monitor)
            let output = ws
                .get("output")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            id_map.insert(ws_id, idx);
            // Store mapping from Niri workspace ID to output name
            if let Some(ref out) = output {
                id_to_output.insert(ws_id, out.clone());
            }
            ws_list.push(WorkspaceMeta {
                id: idx,
                name,
                output: output.clone(),
            });

            // All workspaces in Niri are occupied (dynamic workspaces)
            snapshot.occupied_workspaces.insert(idx);
            // Initialize window count to 0, will be updated from window cache
            snapshot.window_counts.insert(idx, 0);

            let is_focused = ws
                .get("is_focused")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let is_active = ws
                .get("is_active")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if is_focused {
                snapshot.active_workspace.insert(idx);
            }

            if ws
                .get("is_urgent")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                snapshot.urgent_workspaces.insert(idx);
            }

            // Build per-output state (Niri workspaces belong to specific outputs)
            if let Some(ref out_name) = output {
                let per_out = snapshot.per_output.entry(out_name.clone()).or_default();

                per_out.occupied_workspaces.insert(idx);
                // Window count will be updated from window cache
                per_out.window_counts.insert(idx, 0);

                // is_active means visible on this output, is_focused means globally focused
                if is_active {
                    per_out.active_workspace.insert(idx);
                }
            }
        }

        // Sort by output then id for consistent ordering
        ws_list.sort_by(|a, b| match (&a.output, &b.output) {
            (Some(oa), Some(ob)) => oa.cmp(ob).then(a.id.cmp(&b.id)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.id.cmp(&b.id),
        });

        // Update window counts from window cache
        // Must drop all write locks before calling update_window_counts
        drop(snapshot);
        drop(id_to_output);
        drop(id_map);
        drop(ws_list);
        Self::update_window_counts(shared);
    }

    /// Update window counts from the window cache.
    fn update_window_counts(shared: &SharedState) {
        let win_cache = shared.windows.read();
        let id_map = shared.id_to_idx.read();
        let id_to_output = shared.id_to_output.read();
        let mut snapshot = shared.workspace_snapshot.write();

        // Reset global counts
        for count in snapshot.window_counts.values_mut() {
            *count = 0;
        }

        // Reset per-output counts
        for per_out in snapshot.per_output.values_mut() {
            for count in per_out.window_counts.values_mut() {
                *count = 0;
            }
        }

        // Count windows per workspace
        for win in win_cache.values() {
            if let Some(ws_niri_id) = win.workspace_id
                && let Some(&idx) = id_map.get(&ws_niri_id)
            {
                // Update global count
                *snapshot.window_counts.entry(idx).or_insert(0) += 1;

                // Update per-output count using id_to_output (idx is not unique across outputs)
                if let Some(out_name) = id_to_output.get(&ws_niri_id)
                    && let Some(per_out) = snapshot.per_output.get_mut(out_name)
                {
                    *per_out.window_counts.entry(idx).or_insert(0) += 1;
                }
            }
        }
    }

    /// Process window list and update internal state.
    fn process_windows(shared: &SharedState, windows: &[Value]) {
        let mut win_cache = shared.windows.write();
        win_cache.clear();

        for win in windows {
            let Some(win_id) = win.get("id").and_then(|v| v.as_u64()) else {
                continue;
            };

            let data = WindowData {
                id: win_id,
                title: win
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                app_id: win
                    .get("app_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                workspace_id: win.get("workspace_id").and_then(|v| v.as_u64()),
                is_focused: win
                    .get("is_focused")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            };

            win_cache.insert(win_id, data);
        }

        drop(win_cache);
        Self::update_window_counts(shared);
        Self::update_focused_window_from_cache(shared);
    }

    /// Update focused window info from window cache.
    fn update_focused_window_from_cache(shared: &SharedState) -> bool {
        let win_cache = shared.windows.read();
        let id_map = shared.id_to_idx.read();
        let id_to_output = shared.id_to_output.read();

        let mut new_focused: Option<WindowInfo> = None;

        for win in win_cache.values() {
            if !win.is_focused {
                continue;
            }

            let workspace_idx = win
                .workspace_id
                .and_then(|ws_id| id_map.get(&ws_id).copied());
            // Look up the output directly from Niri's workspace ID (not the idx which is per-output)
            let output = win
                .workspace_id
                .and_then(|ws_id| id_to_output.get(&ws_id).cloned());

            new_focused = Some(WindowInfo {
                title: win.title.clone(),
                app_id: win.app_id.clone(),
                workspace_id: workspace_idx,
                output,
            });
            break;
        }

        let mut focused = shared.focused_window.write();
        let changed = *focused != new_focused;
        *focused = new_focused;
        changed
    }

    /// Update a single window in the cache.
    ///
    /// Returns true if this should trigger a window callback (focus changed).
    fn update_single_window(shared: &SharedState, window: &Value) -> bool {
        let Some(win_id) = window.get("id").and_then(|v| v.as_u64()) else {
            return false;
        };

        let title = window
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let app_id = window
            .get("app_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let workspace_id = window.get("workspace_id").and_then(|v| v.as_u64());
        let is_focused = window
            .get("is_focused")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let data = WindowData {
            id: win_id,
            title,
            app_id,
            workspace_id,
            is_focused,
        };

        shared.windows.write().insert(win_id, data);

        // Update window counts
        Self::update_window_counts(shared);

        // If the window is focused, update focused window.
        // Focus updates are used by WindowTitleService to display the active window title.
        if is_focused {
            return Self::update_focused_window_from_cache(shared);
        }

        false
    }

    /// Fetch initial state from Niri.
    fn fetch_initial_state(socket_path: &str, shared: &SharedState) {
        // Fetch workspaces
        if let Some(reply) =
            Self::send_request_static(socket_path, &Value::String("Workspaces".to_string()))
            && let Some(ok) = reply.get("Ok")
            && let Some(workspaces) = ok.get("Workspaces").and_then(|v| v.as_array())
        {
            Self::process_workspaces(shared, workspaces);
        }

        // Fetch windows
        if let Some(reply) =
            Self::send_request_static(socket_path, &Value::String("Windows".to_string()))
            && let Some(ok) = reply.get("Ok")
            && let Some(windows) = ok.get("Windows").and_then(|v| v.as_array())
        {
            Self::process_windows(shared, windows);
        }

        debug!("Fetched initial Niri state");
    }

    /// Handle a Niri event.
    fn handle_event(shared: &SharedState, event: &Value) -> (bool, bool) {
        let mut workspace_changed = false;
        let mut window_changed = false;

        if let Some(workspaces_changed) = event.get("WorkspacesChanged") {
            if let Some(workspaces) = workspaces_changed
                .get("workspaces")
                .and_then(|v| v.as_array())
            {
                Self::process_workspaces(shared, workspaces);
                workspace_changed = true;
            }
        } else if let Some(workspace_activated) = event.get("WorkspaceActivated") {
            let ws_niri_id = workspace_activated.get("id").and_then(|v| v.as_u64());
            let is_focused = workspace_activated
                .get("focused")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if let Some(ws_id) = ws_niri_id {
                let id_map = shared.id_to_idx.read();
                let id_to_output = shared.id_to_output.read();

                if let Some(&idx) = id_map.get(&ws_id) {
                    let mut snapshot = shared.workspace_snapshot.write();

                    // Update global active workspace if this is the focused one
                    if is_focused && !snapshot.active_workspace.contains(&idx) {
                        snapshot.active_workspace.clear();
                        snapshot.active_workspace.insert(idx);
                        workspace_changed = true;
                    }

                    // Always update per-output active workspace
                    // WorkspaceActivated fires when a workspace becomes visible on its output
                    if let Some(out_name) = id_to_output.get(&ws_id)
                        && let Some(per_out) = snapshot.per_output.get_mut(out_name)
                        && !per_out.active_workspace.contains(&idx)
                    {
                        per_out.active_workspace.clear();
                        per_out.active_workspace.insert(idx);
                        workspace_changed = true;
                    }
                }
            }
        } else if let Some(urgency_changed) = event.get("WorkspaceUrgencyChanged") {
            let ws_niri_id = urgency_changed.get("id").and_then(|v| v.as_u64());
            let is_urgent = urgency_changed
                .get("urgent")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if let Some(ws_id) = ws_niri_id {
                let id_map = shared.id_to_idx.read();
                if let Some(&idx) = id_map.get(&ws_id) {
                    let mut snapshot = shared.workspace_snapshot.write();
                    let was_urgent = snapshot.urgent_workspaces.contains(&idx);
                    if is_urgent && !was_urgent {
                        snapshot.urgent_workspaces.insert(idx);
                        workspace_changed = true;
                    } else if !is_urgent && was_urgent {
                        snapshot.urgent_workspaces.remove(&idx);
                        workspace_changed = true;
                    }
                }
            }
        } else if let Some(windows_changed) = event.get("WindowsChanged") {
            if let Some(windows) = windows_changed.get("windows").and_then(|v| v.as_array()) {
                Self::process_windows(shared, windows);
                window_changed = true;
            }
        } else if let Some(window_opened) = event.get("WindowOpenedOrChanged") {
            if let Some(window) = window_opened.get("window") {
                window_changed = Self::update_single_window(shared, window);

                // Also update occupied state
                if let Some(ws_niri_id) = window.get("workspace_id").and_then(|v| v.as_u64()) {
                    let id_map = shared.id_to_idx.read();
                    if let Some(&idx) = id_map.get(&ws_niri_id) {
                        let mut snapshot = shared.workspace_snapshot.write();
                        if !snapshot.occupied_workspaces.contains(&idx) {
                            snapshot.occupied_workspaces.insert(idx);
                            workspace_changed = true;
                        }
                    }
                }
            }
        } else if let Some(window_closed) = event.get("WindowClosed") {
            if let Some(win_id) = window_closed.get("id").and_then(|v| v.as_u64()) {
                shared.windows.write().remove(&win_id);
                Self::update_window_counts(shared);
                window_changed = Self::update_focused_window_from_cache(shared);
                // Window counts changed, so workspace state changed too
                workspace_changed = true;
            }
        } else if let Some(focus_changed) = event.get("WindowFocusChanged")
            && let Some(win_id) = focus_changed.get("id").and_then(|v| v.as_u64())
        {
            // Update focus state in cache
            let mut win_cache = shared.windows.write();
            for win in win_cache.values_mut() {
                win.is_focused = win.id == win_id;
            }
            drop(win_cache);
            window_changed = Self::update_focused_window_from_cache(shared);
        }

        (workspace_changed, window_changed)
    }

    /// Run the event loop (in background thread).
    fn event_loop(
        running: Arc<AtomicBool>,
        shared: Arc<SharedState>,
        socket_path: String,
        callbacks: Option<(WorkspaceCallback, WindowCallback)>,
    ) {
        // Fetch initial state
        Self::fetch_initial_state(&socket_path, &shared);

        // Emit initial state
        if let Some((ref ws_cb, ref win_cb)) = callbacks {
            ws_cb(shared.workspace_snapshot.read().clone());
            if let Some(ref win) = *shared.focused_window.read() {
                win_cb(win.clone());
            }
        }

        // Exponential backoff state
        let mut backoff_ms = RECONNECT_INITIAL_MS;

        while running.load(Ordering::SeqCst) {
            // Connect and request event stream
            let stream = match UnixStream::connect(&socket_path) {
                Ok(s) => {
                    // Reset backoff on successful connection
                    backoff_ms = RECONNECT_INITIAL_MS;
                    s
                }
                Err(e) => {
                    if running.load(Ordering::SeqCst) {
                        warn!(
                            "Failed to connect to Niri socket: {}. Retrying in {}ms",
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

            // Request event stream
            let message = "\"EventStream\"\n";
            if stream
                .try_clone()
                .ok()
                .and_then(|mut s| s.write_all(message.as_bytes()).ok())
                .is_none()
            {
                if running.load(Ordering::SeqCst) {
                    warn!(
                        "Failed to request Niri event stream. Retrying in {}ms",
                        backoff_ms
                    );
                    thread::sleep(Duration::from_millis(backoff_ms));
                    // Exponential backoff with cap
                    backoff_ms = ((backoff_ms as f64) * RECONNECT_MULTIPLIER)
                        .min(RECONNECT_MAX_MS as f64) as u64;
                }
                continue;
            }

            // Set read timeout for graceful shutdown
            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));

            let reader = BufReader::new(stream);

            for line in reader.lines() {
                if !running.load(Ordering::SeqCst) {
                    break;
                }

                match line {
                    Ok(line) => {
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }

                        match serde_json::from_str::<Value>(line) {
                            Ok(event) => {
                                // Skip "Ok": "Handled" responses
                                if event.get("Ok").and_then(|v| v.as_str()) == Some("Handled") {
                                    continue;
                                }

                                let (ws_changed, win_changed) = Self::handle_event(&shared, &event);

                                if let Some((ref ws_cb, ref win_cb)) = callbacks {
                                    if ws_changed {
                                        ws_cb(shared.workspace_snapshot.read().clone());
                                    }
                                    if win_changed
                                        && let Some(ref win) = *shared.focused_window.read()
                                    {
                                        win_cb(win.clone());
                                    }
                                }
                            }
                            Err(e) => {
                                trace!("Failed to parse Niri event: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        // Timeout is expected
                        if e.kind() != std::io::ErrorKind::WouldBlock
                            && e.kind() != std::io::ErrorKind::TimedOut
                        {
                            if running.load(Ordering::SeqCst) {
                                error!("Error reading from Niri socket: {}", e);
                            }
                            break;
                        }
                    }
                }
            }
        }

        debug!("Niri event loop exiting");
    }
}

impl CompositorBackend for NiriBackend {
    fn start(&self, on_workspace_update: WorkspaceCallback, on_window_update: WindowCallback) {
        if self.running.swap(true, Ordering::SeqCst) {
            warn!("NiriBackend already running");
            return;
        }

        debug!("Starting NiriBackend");

        // Get socket path from environment and store on `self` FIRST
        // This ensures socket_path is set for switch_workspace()
        let socket_path = match env::var("NIRI_SOCKET") {
            Ok(p) => p,
            Err(_) => {
                warn!("NIRI_SOCKET not set");
                self.running.store(false, Ordering::SeqCst);
                return;
            }
        };
        *self.socket_path.write() = Some(socket_path.clone());

        // Store callbacks for potential later use
        *self.callbacks.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((on_workspace_update.clone(), on_window_update.clone()));

        // Clone shared state and running flag for the thread
        let running = Arc::clone(&self.running);
        let shared = Arc::clone(&self.shared);
        let callbacks = Some((on_workspace_update, on_window_update));

        // Start event loop thread
        let handle = thread::Builder::new()
            .name("niri-event-loop".into())
            .spawn(move || {
                Self::event_loop(running, shared, socket_path, callbacks);
            })
            .ok();

        *self.event_thread.lock().unwrap_or_else(|e| e.into_inner()) = handle;

        debug!("NiriBackend started");
    }

    fn stop(&self) {
        if !self.running.swap(false, Ordering::SeqCst) {
            return;
        }

        debug!("Stopping NiriBackend");

        if let Some(handle) = self
            .event_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = handle.join();
        }

        debug!("NiriBackend stopped");
    }

    fn list_workspaces(&self) -> Vec<WorkspaceMeta> {
        let workspaces = self.shared.workspaces.read();
        if workspaces.is_empty() {
            // Return default workspaces if not initialized yet
            (1..=10)
                .map(|i| WorkspaceMeta {
                    id: i,
                    name: i.to_string(),
                    output: None,
                })
                .collect()
        } else {
            workspaces.clone()
        }
    }

    fn get_workspace_snapshot(&self) -> WorkspaceSnapshot {
        // If not initialized, try to fetch state
        let socket_path = self.socket_path.read();
        if socket_path.is_none()
            && let Ok(path) = env::var("NIRI_SOCKET")
        {
            drop(socket_path);
            *self.socket_path.write() = Some(path.clone());
            Self::fetch_initial_state(&path, &self.shared);
        }
        self.shared.workspace_snapshot.read().clone()
    }

    fn get_focused_window(&self) -> Option<WindowInfo> {
        self.shared.focused_window.read().clone()
    }

    fn switch_workspace(&self, workspace_id: i32) {
        let request = serde_json::json!({
            "Action": {
                "FocusWorkspace": {
                    "reference": {
                        "Index": workspace_id
                    }
                }
            }
        });
        let _ = self.send_request(&request);
    }

    fn quit_compositor(&self) {
        debug!("Sending quit request to Niri");
        let request = serde_json::json!({
            "Action": {
                "Quit": {
                    "skip_confirmation": true
                }
            }
        });
        let _ = self.send_request(&request);
    }

    fn name(&self) -> &'static str {
        "Niri"
    }
}

impl Drop for NiriBackend {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
    }
}
