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
    CompositorBackend, KeyboardLayoutCallback, KeyboardLayoutInfo, WindowCallback, WindowInfo,
    WorkspaceCallback, WorkspaceMeta, WorkspaceSnapshot,
};

const RECONNECT_INITIAL_MS: u64 = 1000;
const RECONNECT_MAX_MS: u64 = 30000;
const RECONNECT_MULTIPLIER: f64 = 1.5;

/// (workspace_changed, window_changed, keyboard_layout_changed)
type Changes = (bool, bool, bool);

const UNCHANGED: Changes = (false, false, false);

/// Niri's state as reported by the event stream. Everything the panel shows
/// is derived from it on demand.
#[derive(Debug, Default)]
struct NiriState {
    workspaces: HashMap<u64, WorkspaceData>,
    windows: HashMap<u64, WindowData>,
    keyboard_layout_names: Vec<String>,
    /// Current keyboard layout index, once Niri has reported one.
    keyboard_layout_idx: Option<usize>,
}

#[derive(Debug, Clone)]
struct WorkspaceData {
    id: u64,
    idx: i32,
    name: String,
    output: Option<String>,
    /// Visible on its output.
    is_active: bool,
    /// Visible on the focused output.
    is_focused: bool,
    is_urgent: bool,
    /// Unlike window focus, this is kept while a layer-shell surface has the
    /// keyboard.
    active_window_id: Option<u64>,
}

#[derive(Debug, Clone)]
struct WindowData {
    id: u64,
    title: String,
    app_id: String,
    workspace_id: Option<u64>,
    is_focused: bool,
    is_urgent: bool,
    /// Column and tile position in the scrolling layout (niri-specific).
    /// Used for ordering taskbar buttons to match visual window order.
    layout_position: Option<(i32, i32)>,
}

pub struct NiriBackend {
    running: Arc<AtomicBool>,
    event_thread: Mutex<Option<JoinHandle<()>>>,
    socket_path: RwLock<Option<String>>,
    state: Arc<RwLock<NiriState>>,
    callbacks: Mutex<Option<(WorkspaceCallback, WindowCallback)>>,
    keyboard_layout_callback: Mutex<Option<KeyboardLayoutCallback>>,
    window_list_callback: Mutex<Option<super::WindowListCallback>>,
}

/// Callbacks used by the event thread.
struct Callbacks {
    workspace: Option<(WorkspaceCallback, WindowCallback)>,
    keyboard_layout: Option<KeyboardLayoutCallback>,
    window_list: Option<super::WindowListCallback>,
}

fn u64_field(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn bool_field(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Set `field` to `value`, returning whether it changed.
fn update(field: &mut bool, value: bool) -> bool {
    std::mem::replace(field, value) != value
}

fn parse_workspace(ws: &Value) -> Option<WorkspaceData> {
    let idx = ws.get("idx").and_then(Value::as_i64).unwrap_or(1) as i32;
    Some(WorkspaceData {
        id: u64_field(ws, "id")?,
        idx,
        name: str_field(ws, "name").map_or_else(|| idx.to_string(), String::from),
        output: str_field(ws, "output").map(String::from),
        is_active: bool_field(ws, "is_active"),
        is_focused: bool_field(ws, "is_focused"),
        is_urgent: bool_field(ws, "is_urgent"),
        active_window_id: u64_field(ws, "active_window_id"),
    })
}

fn parse_window(window: &Value) -> Option<WindowData> {
    Some(WindowData {
        id: u64_field(window, "id")?,
        title: str_field(window, "title").unwrap_or_default().to_string(),
        app_id: str_field(window, "app_id").unwrap_or_default().to_string(),
        workspace_id: u64_field(window, "workspace_id"),
        is_focused: bool_field(window, "is_focused"),
        is_urgent: bool_field(window, "is_urgent"),
        layout_position: window.get("layout").and_then(parse_layout_position),
    })
}

/// Extract `pos_in_scrolling_layout` from a niri window layout.
fn parse_layout_position(layout: &Value) -> Option<(i32, i32)> {
    let pos = layout.get("pos_in_scrolling_layout")?.as_array()?;
    Some((pos.first()?.as_i64()? as i32, pos.get(1)?.as_i64()? as i32))
}

impl NiriState {
    /// Apply one event-stream event.
    fn apply(&mut self, event: &Value) -> Changes {
        let Some((kind, data)) = event.as_object().and_then(|obj| obj.iter().next()) else {
            return UNCHANGED;
        };

        match kind.as_str() {
            "WorkspacesChanged" => {
                let Some(workspaces) = data.get("workspaces").and_then(Value::as_array) else {
                    return UNCHANGED;
                };
                self.workspaces = workspaces
                    .iter()
                    .filter_map(parse_workspace)
                    .map(|ws| (ws.id, ws))
                    .collect();
                // Workspaces may have moved between outputs.
                (true, true, false)
            }
            "WorkspaceActivated" => {
                let Some(id) = u64_field(data, "id") else {
                    return UNCHANGED;
                };
                let Some(output) = self.workspaces.get(&id).map(|ws| ws.output.clone()) else {
                    return UNCHANGED;
                };
                let focused = bool_field(data, "focused");
                let mut changed = false;
                for ws in self.workspaces.values_mut() {
                    let activated = ws.id == id;
                    if ws.output == output {
                        changed |= update(&mut ws.is_active, activated);
                    }
                    if focused {
                        changed |= update(&mut ws.is_focused, activated);
                    }
                }
                (changed, true, false)
            }
            "WorkspaceUrgencyChanged" => {
                let urgent = bool_field(data, "urgent");
                let changed = u64_field(data, "id")
                    .and_then(|id| self.workspaces.get_mut(&id))
                    .is_some_and(|ws| update(&mut ws.is_urgent, urgent));
                (changed, false, false)
            }
            "WorkspaceActiveWindowChanged" => {
                let Some(ws) =
                    u64_field(data, "workspace_id").and_then(|id| self.workspaces.get_mut(&id))
                else {
                    return UNCHANGED;
                };
                ws.active_window_id = u64_field(data, "active_window_id");
                (false, true, false)
            }
            "WindowsChanged" => {
                let Some(windows) = data.get("windows").and_then(Value::as_array) else {
                    return UNCHANGED;
                };
                self.windows = windows
                    .iter()
                    .filter_map(parse_window)
                    .map(|win| (win.id, win))
                    .collect();
                (true, true, false)
            }
            "WindowOpenedOrChanged" => {
                let Some(window) = data.get("window").and_then(parse_window) else {
                    return UNCHANGED;
                };
                if window.is_focused {
                    for win in self.windows.values_mut() {
                        win.is_focused = false;
                    }
                }
                let workspace_id = window.workspace_id;
                let previous = self.windows.insert(window.id, window);
                // Window counts change when a window opens or moves.
                let moved = previous.is_none_or(|prev| prev.workspace_id != workspace_id);
                (moved, true, false)
            }
            "WindowLayoutsChanged" => {
                let Some(changes) = data.get("changes").and_then(Value::as_array) else {
                    return UNCHANGED;
                };
                // Each change is a [window_id, layout] pair.
                for change in changes {
                    if let Some([id, layout, ..]) = change.as_array().map(Vec::as_slice)
                        && let Some(win) = id.as_u64().and_then(|id| self.windows.get_mut(&id))
                    {
                        win.layout_position = parse_layout_position(layout);
                    }
                }
                (false, true, false)
            }
            "WindowClosed" => {
                let Some(id) = u64_field(data, "id") else {
                    return UNCHANGED;
                };
                self.windows.remove(&id);
                (true, true, false)
            }
            "WindowFocusChanged" => {
                let id = u64_field(data, "id");
                for win in self.windows.values_mut() {
                    win.is_focused = Some(win.id) == id;
                }
                (false, true, false)
            }
            "WindowUrgencyChanged" => {
                let urgent = bool_field(data, "urgent");
                let changed = u64_field(data, "id")
                    .and_then(|id| self.windows.get_mut(&id))
                    .is_some_and(|win| update(&mut win.is_urgent, urgent));
                (false, changed, false)
            }
            "KeyboardLayoutsChanged" => {
                let Some(layouts) = data.get("keyboard_layouts") else {
                    return UNCHANGED;
                };
                self.keyboard_layout_names = layouts
                    .get("names")
                    .and_then(Value::as_array)
                    .map(|names| {
                        names
                            .iter()
                            .filter_map(|name| name.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                self.keyboard_layout_idx =
                    Some(u64_field(layouts, "current_idx").unwrap_or(0) as usize);
                (false, false, true)
            }
            "KeyboardLayoutSwitched" => {
                let Some(idx) = u64_field(data, "idx") else {
                    return UNCHANGED;
                };
                self.keyboard_layout_idx = Some(idx as usize);
                (false, false, true)
            }
            _ => UNCHANGED,
        }
    }

    fn output_of(&self, workspace_id: Option<u64>) -> Option<&String> {
        self.workspaces.get(&workspace_id?)?.output.as_ref()
    }

    /// Workspaces ordered by output, then position.
    fn workspace_list(&self) -> Vec<WorkspaceMeta> {
        let mut workspaces: Vec<&WorkspaceData> = self.workspaces.values().collect();
        // Workspaces without an output sort last.
        workspaces.sort_by_key(|ws| (ws.output.is_none(), &ws.output, ws.idx, ws.id));
        workspaces
            .into_iter()
            .map(|ws| WorkspaceMeta {
                id: ws.id as i32,
                idx: ws.idx,
                name: ws.name.clone(),
                output: ws.output.clone(),
            })
            .collect()
    }

    fn snapshot(&self) -> WorkspaceSnapshot {
        let mut snapshot = WorkspaceSnapshot::default();

        for ws in self.workspaces.values() {
            let id = ws.id as i32;
            // Niri workspaces are dynamic, so every workspace counts as occupied.
            snapshot.occupied_workspaces.insert(id);
            snapshot.window_counts.insert(id, 0);
            if ws.is_focused {
                snapshot.active_workspace.insert(id);
            }
            if ws.is_urgent {
                snapshot.urgent_workspaces.insert(id);
            }
            if let Some(output) = &ws.output {
                let per_out = snapshot.per_output.entry(output.clone()).or_default();
                per_out.occupied_workspaces.insert(id);
                per_out.window_counts.insert(id, 0);
                if ws.is_active {
                    per_out.active_workspace.insert(id);
                }
            }
        }

        for ws_id in self.windows.values().filter_map(|win| win.workspace_id) {
            let id = ws_id as i32;
            *snapshot.window_counts.entry(id).or_insert(0) += 1;
            if let Some(output) = self.output_of(Some(ws_id))
                && let Some(per_out) = snapshot.per_output.get_mut(output)
            {
                *per_out.window_counts.entry(id).or_insert(0) += 1;
            }
        }

        snapshot
    }

    /// The window each output shows: the focused window on its visible
    /// workspace, otherwise that workspace's active window, otherwise nothing.
    fn per_output_windows(&self) -> HashMap<String, WindowInfo> {
        let mut per_output = HashMap::new();

        for ws in self.workspaces.values() {
            let Some(output) = &ws.output else {
                continue;
            };
            let info = per_output
                .entry(output.clone())
                .or_insert_with(|| WindowInfo {
                    output: Some(output.clone()),
                    ..Default::default()
                });
            if !ws.is_active {
                continue;
            }

            info.focused = ws.is_focused;
            let on_workspace = |win: &&WindowData| win.workspace_id == Some(ws.id);
            let window = self
                .windows
                .values()
                .filter(on_workspace)
                .find(|win| win.is_focused)
                .or_else(|| {
                    ws.active_window_id
                        .and_then(|id| self.windows.get(&id))
                        .filter(on_workspace)
                });
            if let Some(win) = window {
                info.title = win.title.clone();
                info.app_id = win.app_id.clone();
            }
        }

        per_output
    }

    fn focused_window(&self) -> Option<WindowInfo> {
        let win = self.windows.values().find(|win| win.is_focused)?;
        Some(WindowInfo {
            title: win.title.clone(),
            app_id: win.app_id.clone(),
            output: self.output_of(win.workspace_id).cloned(),
            focused: true,
        })
    }

    fn window_list(&self) -> Vec<super::Window> {
        let mut windows: Vec<&WindowData> = self.windows.values().collect();
        // Sort by output name, then workspace index, then layout position.
        // This mirrors the workspace order so multi-monitor taskbars with
        // filter_by_output=false group windows by monitor first.
        windows.sort_by_cached_key(|win| {
            let ws = win.workspace_id.and_then(|id| self.workspaces.get(&id));
            (
                ws.and_then(|ws| ws.output.clone()).unwrap_or_default(),
                ws.map_or(i32::MAX, |ws| ws.idx),
                win.layout_position.unwrap_or((i32::MAX, i32::MAX)),
                win.id,
            )
        });

        windows
            .into_iter()
            .map(|win| super::Window {
                id: win.id,
                title: win.title.clone(),
                app_id: win.app_id.clone(),
                workspace_id: win.workspace_id.map(|id| id as i32),
                output: self.output_of(win.workspace_id).cloned(),
                is_focused: win.is_focused,
                is_urgent: win.is_urgent,
                // Niri has no scratchpad concept.
                is_scratchpad: false,
            })
            .collect()
    }

    fn keyboard_layout(&self) -> Option<KeyboardLayoutInfo> {
        let idx = self.keyboard_layout_idx?;
        Some(KeyboardLayoutInfo {
            layout_name: self
                .keyboard_layout_names
                .get(idx)
                .cloned()
                .unwrap_or_default(),
            layout_count: Some(self.keyboard_layout_names.len()),
        })
    }
}

impl NiriBackend {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            event_thread: Mutex::new(None),
            socket_path: RwLock::new(None),
            state: Arc::new(RwLock::new(NiriState::default())),
            callbacks: Mutex::new(None),
            keyboard_layout_callback: Mutex::new(None),
            window_list_callback: Mutex::new(None),
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

    /// Fetch initial state from Niri.
    fn fetch_initial_state(socket_path: &str, state: &RwLock<NiriState>) {
        let request = |name: &str| {
            let mut reply = Self::send_request_static(socket_path, &Value::String(name.into()))?;
            Some(reply.get_mut("Ok")?.get_mut(name)?.take())
        };
        let workspaces = request("Workspaces");
        let windows = request("Windows");
        let keyboard_layouts = request("KeyboardLayouts");

        let mut state = state.write();
        if let Some(workspaces) = workspaces {
            state.apply(&serde_json::json!({"WorkspacesChanged": {"workspaces": workspaces}}));
        }
        if let Some(windows) = windows {
            state.apply(&serde_json::json!({"WindowsChanged": {"windows": windows}}));
        }
        if let Some(keyboard_layouts) = keyboard_layouts {
            state.apply(&serde_json::json!({
                "KeyboardLayoutsChanged": {"keyboard_layouts": keyboard_layouts}
            }));
        }

        debug!("Fetched initial Niri state");
    }

    /// Send the changed parts of the state to the callbacks. The lock is
    /// released before any callback runs.
    fn emit(state: &RwLock<NiriState>, callbacks: &Callbacks, changes: Changes) {
        let (ws_changed, win_changed, kb_changed) = changes;
        let (snapshot, per_output, windows, keyboard_layout) = {
            let state = state.read();
            (
                ws_changed.then(|| state.snapshot()),
                win_changed.then(|| state.per_output_windows()),
                // Workspace changes can affect taskbar ordering and separator
                // labels, which resolve workspace IDs to idx/name metadata.
                (ws_changed || win_changed).then(|| state.window_list()),
                kb_changed.then(|| state.keyboard_layout()).flatten(),
            )
        };

        if let Some((ws_cb, win_cb)) = &callbacks.workspace {
            if let Some(snapshot) = snapshot {
                ws_cb(snapshot);
            }
            // Emit every output, including empty info for outputs with no window.
            for info in per_output.into_iter().flat_map(HashMap::into_values) {
                win_cb(info);
            }
        }
        if let Some(wl_cb) = &callbacks.window_list
            && let Some(windows) = windows
        {
            wl_cb(super::WindowListSnapshot { windows });
        }
        if let Some(kb_cb) = &callbacks.keyboard_layout
            && let Some(info) = keyboard_layout
        {
            kb_cb(info);
        }
    }

    /// Run the event loop (in background thread).
    fn event_loop(
        running: Arc<AtomicBool>,
        state: Arc<RwLock<NiriState>>,
        socket_path: String,
        callbacks: Callbacks,
    ) {
        Self::fetch_initial_state(&socket_path, &state);
        Self::emit(&state, &callbacks, (true, true, true));

        // Exponential backoff state
        let mut backoff_ms = RECONNECT_INITIAL_MS;

        while running.load(Ordering::SeqCst) {
            super::visibility::notify_changed();
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

                                super::visibility::notify_changed();
                                let changes = state.write().apply(&event);
                                Self::emit(&state, &callbacks, changes);
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
                                super::visibility::notify_changed();
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
    fn visibility_reader(&self) -> Option<super::visibility::VisibilityReader> {
        Some(super::visibility::VisibilityReader::Niri(
            self.socket_path.read().clone()?,
        ))
    }

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

        // Clone state and running flag for the thread
        let running = Arc::clone(&self.running);
        let state = Arc::clone(&self.state);
        let callbacks = Callbacks {
            workspace: Some((on_workspace_update, on_window_update)),
            keyboard_layout: self
                .keyboard_layout_callback
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            window_list: self
                .window_list_callback
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        };

        // Start event loop thread
        let handle = thread::Builder::new()
            .name("niri-event-loop".into())
            .spawn(move || Self::event_loop(running, state, socket_path, callbacks))
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
        let workspaces = self.state.read().workspace_list();
        if workspaces.is_empty() {
            // Return default workspaces if not initialized yet
            (1..=10)
                .map(|i| WorkspaceMeta {
                    id: i,
                    idx: i,
                    name: i.to_string(),
                    output: None,
                })
                .collect()
        } else {
            workspaces
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
            Self::fetch_initial_state(&path, &self.state);
        }
        self.state.read().snapshot()
    }

    fn get_focused_window(&self) -> Option<WindowInfo> {
        self.state.read().focused_window()
    }

    fn switch_workspace(&self, workspace_id: i32) {
        // Use stable workspace ID (not positional index) for reliable switching.
        let request = serde_json::json!({
            "Action": {
                "FocusWorkspace": {
                    "reference": {
                        "Id": workspace_id
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

    fn set_keyboard_layout_callback(&self, callback: KeyboardLayoutCallback) {
        *self
            .keyboard_layout_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(callback);
    }

    fn set_window_list_callback(&self, callback: super::WindowListCallback) {
        *self
            .window_list_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(callback);
    }

    fn get_keyboard_layout(&self) -> Option<KeyboardLayoutInfo> {
        self.state.read().keyboard_layout()
    }

    fn switch_keyboard_layout_next(&self) {
        let request = serde_json::json!({
            "Action": {
                "SwitchLayout": {
                    "layout": "Next"
                }
            }
        });
        let _ = self.send_request(&request);
    }

    fn list_windows(&self) -> Vec<super::Window> {
        self.state.read().window_list()
    }

    fn focus_window(&self, window_id: u64) {
        let request = serde_json::json!({
            "Action": {
                "FocusWindow": {
                    "id": window_id
                }
            }
        });
        let _ = self.send_request(&request);
    }
}

impl Drop for NiriBackend {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: u64, ws: u64, title: &str, focused: bool) -> serde_json::Value {
        serde_json::json!({
            "id": id, "title": title, "app_id": format!("{title}-app"),
            "workspace_id": ws, "is_focused": focused, "is_urgent": false,
        })
    }

    fn workspace(
        id: u64,
        output: &str,
        active: bool,
        focused: bool,
        window: Option<u64>,
    ) -> serde_json::Value {
        serde_json::json!({
            "id": id, "idx": id, "name": null, "output": output,
            "is_active": active, "is_focused": focused, "is_urgent": false,
            "active_window_id": window,
        })
    }

    fn shown(state: &NiriState, output: &str) -> (String, bool) {
        let per_output = state.per_output_windows();
        let info = per_output.get(output).expect("output has an entry");
        (info.title.clone(), info.focused)
    }

    fn shown_focused(state: &NiriState) -> String {
        let per_output = state.per_output_windows();
        let focused: Vec<_> = per_output.values().filter(|info| info.focused).collect();
        assert_eq!(focused.len(), 1, "exactly one focused output");
        focused[0].title.clone()
    }

    /// Workspace 1 (DP-1, focused) has five windows with B (id 4) active;
    /// workspace 2 (DP-1) is empty; DP-2 shows workspace 3 with window X and
    /// hides workspace 4 with window Y.
    fn setup() -> NiriState {
        let mut state = NiriState::default();
        apply(
            &mut state,
            serde_json::json!({"WorkspacesChanged": {"workspaces": [
                workspace(1, "DP-1", true, true, Some(4)),
                workspace(2, "DP-1", false, false, None),
                workspace(3, "DP-2", true, false, Some(10)),
                workspace(4, "DP-2", false, false, Some(11)),
            ]}}),
        );
        apply(
            &mut state,
            serde_json::json!({"WindowsChanged": {"windows": [
                window(1, 1, "A1", false),
                window(2, 1, "A2", false),
                window(3, 1, "A3", false),
                window(4, 1, "B", true),
                window(5, 1, "A5", false),
                window(10, 3, "X", false),
                window(11, 4, "Y", false),
            ]}}),
        );
        state
    }

    #[test]
    fn layer_shell_focus_keeps_the_active_window() {
        let mut state = setup();
        assert_eq!(shown_focused(&state), "B");

        // A layer-shell surface (e.g. a popover) takes the keyboard.
        let (_, changed, _) = apply(
            &mut state,
            serde_json::json!({"WindowFocusChanged": {"id": null}}),
        );
        assert!(changed);
        assert_eq!(shown(&state, "DP-1"), ("B".to_string(), true));

        // The title still updates while the popover has focus.
        apply(
            &mut state,
            serde_json::json!({"WindowOpenedOrChanged": {
                "window": window(4, 1, "B renamed", false)
            }}),
        );
        assert_eq!(shown_focused(&state), "B renamed");

        // Focus returns to the window.
        apply(
            &mut state,
            serde_json::json!({"WindowFocusChanged": {"id": 4}}),
        );
        assert_eq!(shown_focused(&state), "B renamed");
    }

    #[test]
    fn unfocused_output_shows_its_active_window() {
        let mut state = setup();
        assert_eq!(shown(&state, "DP-2"), ("X".to_string(), false));

        // An active-window change on a hidden workspace leaves DP-2 alone.
        apply(
            &mut state,
            serde_json::json!({"WorkspaceActiveWindowChanged": {
                "workspace_id": 4, "active_window_id": 11
            }}),
        );
        assert_eq!(shown(&state, "DP-2"), ("X".to_string(), false));
    }

    #[test]
    fn empty_workspace_clears_the_title() {
        let mut state = setup();
        apply(
            &mut state,
            serde_json::json!({"WorkspaceActivated": {"id": 2, "focused": true}}),
        );
        apply(
            &mut state,
            serde_json::json!({"WindowFocusChanged": {"id": null}}),
        );

        assert_eq!(shown(&state, "DP-1"), (String::new(), true));
    }

    fn apply(state: &mut NiriState, event: serde_json::Value) -> (bool, bool, bool) {
        state.apply(&event)
    }

    fn window_ids(state: &NiriState) -> Vec<u64> {
        state.window_list().iter().map(|win| win.id).collect()
    }

    fn set<const N: usize>(ids: [i32; N]) -> std::collections::HashSet<i32> {
        ids.into_iter().collect()
    }

    fn counts<const N: usize>(entries: [(i32, u32); N]) -> HashMap<i32, u32> {
        entries.into_iter().collect()
    }

    #[test]
    fn snapshot_reports_workspaces_and_window_counts() {
        let state = setup();
        let snapshot = state.snapshot();

        assert_eq!(snapshot.active_workspace, set([1]));
        assert_eq!(snapshot.occupied_workspaces, set([1, 2, 3, 4]));
        assert!(snapshot.urgent_workspaces.is_empty());
        assert_eq!(
            snapshot.window_counts,
            counts([(1, 5), (2, 0), (3, 1), (4, 1)])
        );

        let dp1 = &snapshot.per_output["DP-1"];
        assert_eq!(dp1.active_workspace, set([1]));
        assert_eq!(dp1.occupied_workspaces, set([1, 2]));
        assert_eq!(dp1.window_counts, counts([(1, 5), (2, 0)]));

        let dp2 = &snapshot.per_output["DP-2"];
        assert_eq!(dp2.active_workspace, set([3]));
        assert_eq!(dp2.occupied_workspaces, set([3, 4]));
        assert_eq!(dp2.window_counts, counts([(3, 1), (4, 1)]));
    }

    #[test]
    fn window_counts_follow_moves_and_closes() {
        let mut state = setup();

        let (ws_changed, _, _) = apply(
            &mut state,
            serde_json::json!({"WindowOpenedOrChanged": {"window": window(5, 2, "A5", false)}}),
        );
        assert!(ws_changed);
        assert_eq!(
            state.snapshot().window_counts,
            counts([(1, 4), (2, 1), (3, 1), (4, 1)])
        );

        // A new window changes the counts; a title change doesn't.
        let opened =
            serde_json::json!({"WindowOpenedOrChanged": {"window": window(6, 2, "C", false)}});
        assert!(apply(&mut state, opened).0);
        let renamed =
            serde_json::json!({"WindowOpenedOrChanged": {"window": window(6, 2, "C2", false)}});
        assert!(!apply(&mut state, renamed).0);
        assert_eq!(state.snapshot().window_counts[&2], 2);
        apply(&mut state, serde_json::json!({"WindowClosed": {"id": 6}}));

        let (ws_changed, win_changed, _) =
            apply(&mut state, serde_json::json!({"WindowClosed": {"id": 10}}));
        assert!(ws_changed && win_changed);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.window_counts[&3], 0);
        assert_eq!(snapshot.per_output["DP-2"].window_counts[&3], 0);
        assert_eq!(window_ids(&state), vec![1, 2, 3, 4, 5, 11]);
    }

    #[test]
    fn workspaces_changed_keeps_window_counts() {
        let mut state = setup();
        apply(
            &mut state,
            serde_json::json!({"WorkspacesChanged": {"workspaces": [
                workspace(1, "DP-1", false, false, Some(4)),
                workspace(2, "DP-1", true, true, None),
                workspace(3, "DP-2", true, false, Some(10)),
                workspace(4, "DP-2", false, false, Some(11)),
            ]}}),
        );

        let snapshot = state.snapshot();
        assert_eq!(snapshot.active_workspace, set([2]));
        assert_eq!(snapshot.per_output["DP-1"].active_workspace, set([2]));
        assert_eq!(
            snapshot.window_counts,
            counts([(1, 5), (2, 0), (3, 1), (4, 1)])
        );
    }

    #[test]
    fn workspace_activation_updates_global_and_output_state() {
        let mut state = setup();

        // Activating without focus only changes what the output shows.
        let (ws_changed, _, _) = apply(
            &mut state,
            serde_json::json!({"WorkspaceActivated": {"id": 4, "focused": false}}),
        );
        assert!(ws_changed);
        let snap = state.snapshot();
        assert_eq!(snap.active_workspace, set([1]));
        assert_eq!(snap.per_output["DP-1"].active_workspace, set([1]));
        assert_eq!(snap.per_output["DP-2"].active_workspace, set([4]));
        assert_eq!(shown(&state, "DP-2"), ("Y".to_string(), false));

        // Focusing a workspace on another output moves global focus there.
        apply(
            &mut state,
            serde_json::json!({"WorkspaceActivated": {"id": 3, "focused": true}}),
        );
        let snap = state.snapshot();
        assert_eq!(snap.active_workspace, set([3]));
        assert_eq!(snap.per_output["DP-1"].active_workspace, set([1]));
        assert_eq!(snap.per_output["DP-2"].active_workspace, set([3]));
        assert_eq!(shown_focused(&state), "X");
    }

    #[test]
    fn urgency_is_tracked_for_workspaces_and_windows() {
        let mut state = setup();

        let urgent = serde_json::json!({"WorkspaceUrgencyChanged": {"id": 2, "urgent": true}});
        assert!(apply(&mut state, urgent.clone()).0);
        assert_eq!(state.snapshot().urgent_workspaces, set([2]));
        assert!(!apply(&mut state, urgent).0);

        apply(
            &mut state,
            serde_json::json!({"WorkspaceUrgencyChanged": {"id": 2, "urgent": false}}),
        );
        assert!(state.snapshot().urgent_workspaces.is_empty());

        let urgent = serde_json::json!({"WindowUrgencyChanged": {"id": 3, "urgent": true}});
        assert!(apply(&mut state, urgent.clone()).1);
        assert!(!apply(&mut state, urgent).1);
        let urgent_windows: Vec<u64> = state
            .window_list()
            .iter()
            .filter(|win| win.is_urgent)
            .map(|win| win.id)
            .collect();
        assert_eq!(urgent_windows, vec![3]);
    }

    #[test]
    fn window_list_follows_output_workspace_and_layout_order() {
        let mut state = setup();
        assert_eq!(window_ids(&state), vec![1, 2, 3, 4, 5, 10, 11]);

        apply(
            &mut state,
            serde_json::json!({"WindowLayoutsChanged": {"changes": [
                [5, {"pos_in_scrolling_layout": [1, 1]}],
                [3, {"pos_in_scrolling_layout": [2, 1]}],
                [1, {"pos_in_scrolling_layout": [2, 2]}],
            ]}}),
        );
        assert_eq!(window_ids(&state), vec![5, 3, 1, 2, 4, 10, 11]);

        // A full window update carries its layout position too.
        let mut moved = window(11, 3, "Y", false);
        moved["layout"] = serde_json::json!({"pos_in_scrolling_layout": [1, 1]});
        apply(
            &mut state,
            serde_json::json!({"WindowOpenedOrChanged": {"window": moved}}),
        );
        assert_eq!(window_ids(&state), vec![5, 3, 1, 2, 4, 11, 10]);

        let windows = state.window_list();
        let y = windows.iter().find(|win| win.id == 11).unwrap();
        assert_eq!(y.workspace_id, Some(3));
        assert_eq!(y.output.as_deref(), Some("DP-2"));
        assert_eq!(y.app_id, "Y-app");
    }

    #[test]
    fn focused_window_follows_focus_and_close() {
        let mut state = setup();
        assert_eq!(
            state.focused_window(),
            Some(WindowInfo {
                title: "B".to_string(),
                app_id: "B-app".to_string(),
                output: Some("DP-1".to_string()),
                focused: true,
            })
        );

        apply(
            &mut state,
            serde_json::json!({"WindowFocusChanged": {"id": 10}}),
        );
        let focused = state.focused_window().unwrap();
        assert_eq!(focused.title, "X");
        assert_eq!(focused.output.as_deref(), Some("DP-2"));

        // Opening a focused window takes focus from the previous one.
        apply(
            &mut state,
            serde_json::json!({"WindowOpenedOrChanged": {"window": window(20, 1, "New", true)}}),
        );
        assert_eq!(state.focused_window().unwrap().title, "New");
        let focused_ids: Vec<u64> = state
            .window_list()
            .iter()
            .filter(|win| win.is_focused)
            .map(|win| win.id)
            .collect();
        assert_eq!(focused_ids, vec![20]);

        apply(&mut state, serde_json::json!({"WindowClosed": {"id": 20}}));
        assert_eq!(state.focused_window(), None);
    }

    #[test]
    fn keyboard_layout_follows_list_and_switch() {
        let mut state = setup();
        assert_eq!(state.keyboard_layout(), None);

        let (_, _, changed) = apply(
            &mut state,
            serde_json::json!({"KeyboardLayoutsChanged": {"keyboard_layouts": {
                "names": ["English (US)", "Swedish"], "current_idx": 0
            }}}),
        );
        assert!(changed);
        assert_eq!(
            state.keyboard_layout(),
            Some(KeyboardLayoutInfo {
                layout_name: "English (US)".to_string(),
                layout_count: Some(2),
            })
        );

        let (_, _, changed) = apply(
            &mut state,
            serde_json::json!({"KeyboardLayoutSwitched": {"idx": 1}}),
        );
        assert!(changed);
        assert_eq!(state.keyboard_layout().unwrap().layout_name, "Swedish");
    }
}
