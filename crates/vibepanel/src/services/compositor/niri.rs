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

struct SharedState {
    workspace_snapshot: RwLock<WorkspaceSnapshot>,
    focused_window: RwLock<Option<WindowInfo>>,
    workspaces: RwLock<Vec<WorkspaceMeta>>,
    /// Map from Niri's u64 workspace ID to output name.
    id_to_output: RwLock<HashMap<u64, String>>,
    windows: RwLock<HashMap<u64, WindowData>>,
    /// Per-output active window info (output name -> WindowInfo).
    /// This tracks the "would be focused" window for each monitor.
    per_output_window: RwLock<HashMap<String, WindowInfo>>,
    /// Niri's active window per workspace (workspace ID -> window ID). Unlike
    /// window focus, this is kept while a layer-shell surface has the keyboard.
    active_windows: RwLock<HashMap<u64, u64>>,
    /// Current keyboard layout info.
    keyboard_layout: RwLock<Option<KeyboardLayoutInfo>>,
    /// List of available keyboard layout names (from Niri's KeyboardLayouts).
    keyboard_layout_names: RwLock<Vec<String>>,
    /// Current keyboard layout index.
    keyboard_layout_idx: RwLock<usize>,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            workspace_snapshot: RwLock::new(WorkspaceSnapshot::default()),
            focused_window: RwLock::new(None),
            workspaces: RwLock::new(Vec::new()),
            id_to_output: RwLock::new(HashMap::new()),
            windows: RwLock::new(HashMap::new()),
            per_output_window: RwLock::new(HashMap::new()),
            active_windows: RwLock::new(HashMap::new()),
            keyboard_layout: RwLock::new(None),
            keyboard_layout_names: RwLock::new(Vec::new()),
            keyboard_layout_idx: RwLock::new(0),
        }
    }
}

pub struct NiriBackend {
    running: Arc<AtomicBool>,
    event_thread: Mutex<Option<JoinHandle<()>>>,
    socket_path: RwLock<Option<String>>,
    shared: Arc<SharedState>,
    callbacks: Mutex<Option<(WorkspaceCallback, WindowCallback)>>,
    keyboard_layout_callback: Mutex<Option<KeyboardLayoutCallback>>,
    window_list_callback: Mutex<Option<super::WindowListCallback>>,
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

/// Extract `pos_in_scrolling_layout` from a niri window JSON value.
fn parse_layout_position(window: &Value) -> Option<(i32, i32)> {
    let layout = window.get("layout")?;
    let pos = layout.get("pos_in_scrolling_layout")?.as_array()?;
    if pos.len() >= 2 {
        Some((pos[0].as_i64()? as i32, pos[1].as_i64()? as i32))
    } else {
        None
    }
}

impl NiriBackend {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            event_thread: Mutex::new(None),
            socket_path: RwLock::new(None),
            shared: Arc::new(SharedState::default()),
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

    fn get_windows_from_shared(shared: &SharedState) -> Vec<super::Window> {
        let windows = shared.windows.read();
        let id_to_output = shared.id_to_output.read();
        let workspaces = shared.workspaces.read();

        // Build a map from workspace niri-ID to workspace display index for sorting.
        let ws_id_to_idx: HashMap<u64, i32> =
            workspaces.iter().map(|ws| (ws.id as u64, ws.idx)).collect();

        // Collect windows with their sorting keys.
        struct SortableWindow {
            window: super::Window,
            output_name: String,
            ws_idx: i32,
            layout_pos: (i32, i32),
        }

        let mut sortable: Vec<SortableWindow> = windows
            .values()
            .map(|win| {
                let output = win
                    .workspace_id
                    .and_then(|ws_id| id_to_output.get(&ws_id).cloned());

                let ws_idx = win
                    .workspace_id
                    .and_then(|ws_id| ws_id_to_idx.get(&ws_id).copied())
                    .unwrap_or(i32::MAX);

                let layout_pos = win.layout_position.unwrap_or((i32::MAX, i32::MAX));

                let output_name = output.clone().unwrap_or_default();

                SortableWindow {
                    window: super::Window {
                        id: win.id,
                        title: win.title.clone(),
                        app_id: win.app_id.clone(),
                        workspace_id: win.workspace_id.map(|id| id as i32),
                        output,
                        is_focused: win.is_focused,
                        is_urgent: win.is_urgent,
                        // Niri has no scratchpad concept.
                        is_scratchpad: false,
                    },
                    output_name,
                    ws_idx,
                    layout_pos,
                }
            })
            .collect();

        // Sort by output name, then workspace display index, then layout position,
        // then window ID.  This mirrors the workspace sort order (output → idx)
        // so multi-monitor taskbars with filter_by_output=false group windows by
        // monitor first.
        sortable.sort_by(|a, b| {
            a.output_name
                .cmp(&b.output_name)
                .then(a.ws_idx.cmp(&b.ws_idx))
                .then(a.layout_pos.cmp(&b.layout_pos))
                .then(a.window.id.cmp(&b.window.id))
        });

        sortable.into_iter().map(|s| s.window).collect()
    }

    /// Process workspace list and update internal state.
    fn process_workspaces(shared: &SharedState, workspaces: &[Value]) {
        let mut ws_list = shared.workspaces.write();
        let mut id_to_output = shared.id_to_output.write();
        let mut snapshot = shared.workspace_snapshot.write();
        let mut active_windows = shared.active_windows.write();

        ws_list.clear();
        id_to_output.clear();
        active_windows.clear();
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
            // Use Niri's stable workspace ID for identity tracking.
            let stable_id = ws_id as i32;
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

            // Store mapping from Niri workspace ID to output name
            if let Some(ref out) = output {
                id_to_output.insert(ws_id, out.clone());
            }
            if let Some(win_id) = ws.get("active_window_id").and_then(|v| v.as_u64()) {
                active_windows.insert(ws_id, win_id);
            }
            ws_list.push(WorkspaceMeta {
                id: stable_id,
                idx,
                name,
                output: output.clone(),
            });

            // All workspaces in Niri are occupied (dynamic workspaces)
            snapshot.occupied_workspaces.insert(stable_id);
            // Initialize window count to 0, will be updated from window cache
            snapshot.window_counts.insert(stable_id, 0);

            let is_focused = ws
                .get("is_focused")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let is_active = ws
                .get("is_active")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if is_focused {
                snapshot.active_workspace.insert(stable_id);
            }

            if ws
                .get("is_urgent")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                snapshot.urgent_workspaces.insert(stable_id);
            }

            // Build per-output state (Niri workspaces belong to specific outputs)
            if let Some(ref out_name) = output {
                let per_out = snapshot.per_output.entry(out_name.clone()).or_default();

                per_out.occupied_workspaces.insert(stable_id);
                // Window count will be updated from window cache
                per_out.window_counts.insert(stable_id, 0);

                // is_active means visible on this output, is_focused means globally focused
                if is_active {
                    per_out.active_workspace.insert(stable_id);
                }
            }
        }

        // Sort by output then positional index for consistent ordering
        ws_list.sort_by(|a, b| match (&a.output, &b.output) {
            (Some(oa), Some(ob)) => oa.cmp(ob).then(a.idx.cmp(&b.idx)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.idx.cmp(&b.idx),
        });

        // Update window counts and titles from window cache
        // Must drop all write locks before calling the update helpers
        drop(active_windows);
        drop(snapshot);
        drop(id_to_output);
        drop(ws_list);
        Self::update_window_counts(shared);
        Self::update_per_output_windows(shared);
    }

    /// Update window counts from the window cache.
    fn update_window_counts(shared: &SharedState) {
        let win_cache = shared.windows.read();
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
            if let Some(ws_niri_id) = win.workspace_id {
                let stable_id = ws_niri_id as i32;

                // Update global count
                *snapshot.window_counts.entry(stable_id).or_insert(0) += 1;

                // Update per-output count
                if let Some(out_name) = id_to_output.get(&ws_niri_id)
                    && let Some(per_out) = snapshot.per_output.get_mut(out_name)
                {
                    *per_out.window_counts.entry(stable_id).or_insert(0) += 1;
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
                is_urgent: win
                    .get("is_urgent")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                layout_position: parse_layout_position(win),
            };

            win_cache.insert(win_id, data);
        }

        drop(win_cache);
        Self::update_window_counts(shared);
        Self::update_focused_window_from_cache(shared);
        Self::update_per_output_windows(shared);
    }

    /// Update per-output active window info from window cache and workspace state.
    ///
    /// Each output shows the window on its visible workspace that has keyboard
    /// focus, otherwise the workspace's active window, otherwise nothing.
    fn update_per_output_windows(shared: &SharedState) {
        let win_cache = shared.windows.read();
        let id_to_output = shared.id_to_output.read();
        let snapshot = shared.workspace_snapshot.read();
        let active_windows = shared.active_windows.read();
        let mut per_output = shared.per_output_window.write();

        // Rebuild from scratch so removed outputs aren't emitted again.
        per_output.clear();

        for (out_name, per_out) in &snapshot.per_output {
            // Find active workspace's niri ID for this output
            let active_ws_id = id_to_output.iter().find_map(|(&ws_id, out)| {
                (out == out_name && per_out.active_workspace.contains(&(ws_id as i32)))
                    .then_some(ws_id)
            });
            // The output showing the globally focused workspace.
            let focused =
                active_ws_id.is_some_and(|id| snapshot.active_workspace.contains(&(id as i32)));

            let win_info = active_ws_id.and_then(|ws_id| {
                win_cache
                    .values()
                    .find(|win| win.workspace_id == Some(ws_id) && win.is_focused)
                    .or_else(|| {
                        active_windows
                            .get(&ws_id)
                            .and_then(|win_id| win_cache.get(win_id))
                            .filter(|win| win.workspace_id == Some(ws_id))
                    })
            });

            let info = win_info
                .map(|win| WindowInfo {
                    title: win.title.clone(),
                    app_id: win.app_id.clone(),
                    output: Some(out_name.clone()),
                    focused,
                })
                .unwrap_or_else(|| WindowInfo {
                    output: Some(out_name.clone()),
                    focused,
                    ..Default::default()
                });

            per_output.insert(out_name.clone(), info);
        }
    }

    /// Update focused window info from window cache.
    fn update_focused_window_from_cache(shared: &SharedState) -> bool {
        let win_cache = shared.windows.read();
        let id_to_output = shared.id_to_output.read();

        let mut new_focused: Option<WindowInfo> = None;

        for win in win_cache.values() {
            if !win.is_focused {
                continue;
            }

            // Look up the output directly from Niri's workspace ID
            let output = win
                .workspace_id
                .and_then(|ws_id| id_to_output.get(&ws_id).cloned());

            new_focused = Some(WindowInfo {
                title: win.title.clone(),
                app_id: win.app_id.clone(),
                output,
                focused: true,
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
        let is_urgent = window
            .get("is_urgent")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let data = WindowData {
            id: win_id,
            title,
            app_id,
            workspace_id,
            is_focused,
            is_urgent,
            layout_position: parse_layout_position(window),
        };

        {
            let mut win_cache = shared.windows.write();
            if is_focused {
                for win in win_cache.values_mut() {
                    win.is_focused = false;
                }
            }
            win_cache.insert(win_id, data);
        }

        // Update window counts
        Self::update_window_counts(shared);

        // If the window is focused, update focused window.
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

        // Fetch keyboard layouts
        Self::fetch_keyboard_layouts(socket_path, shared);

        debug!("Fetched initial Niri state");
    }

    /// Fetch keyboard layouts from Niri.
    fn fetch_keyboard_layouts(socket_path: &str, shared: &SharedState) {
        let Some(reply) =
            Self::send_request_static(socket_path, &Value::String("KeyboardLayouts".to_string()))
        else {
            debug!("fetch_keyboard_layouts: failed to query from Niri");
            return;
        };

        let Some(ok) = reply.get("Ok") else {
            debug!("fetch_keyboard_layouts: Niri returned error: {:?}", reply);
            return;
        };

        let Some(kb_layouts) = ok.get("KeyboardLayouts") else {
            debug!("fetch_keyboard_layouts: no KeyboardLayouts in response");
            return;
        };

        Self::process_keyboard_layouts(shared, kb_layouts);
    }

    /// Process keyboard layout data from Niri.
    fn process_keyboard_layouts(shared: &SharedState, kb_layouts: &Value) -> bool {
        let names = kb_layouts
            .get("names")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|entry| entry.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let current_idx = kb_layouts
            .get("current_idx")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        let layout_count = names.len();
        let layout_name = names.get(current_idx).cloned().unwrap_or_default();

        debug!(
            "process_keyboard_layouts: idx={}, layout='{}', count={}",
            current_idx, layout_name, layout_count
        );

        *shared.keyboard_layout_names.write() = names;
        *shared.keyboard_layout_idx.write() = current_idx;
        *shared.keyboard_layout.write() = Some(KeyboardLayoutInfo {
            layout_name,

            layout_count: Some(layout_count),
        });

        true
    }

    /// Process a keyboard layout switch event.
    fn process_keyboard_layout_switch(shared: &SharedState, idx: usize) -> bool {
        let names = shared.keyboard_layout_names.read();
        let layout_name = names.get(idx).cloned().unwrap_or_default();
        let layout_count = names.len();

        debug!(
            "process_keyboard_layout_switch: idx={}, layout='{}'",
            idx, layout_name
        );

        *shared.keyboard_layout_idx.write() = idx;
        *shared.keyboard_layout.write() = Some(KeyboardLayoutInfo {
            layout_name,

            layout_count: Some(layout_count),
        });

        true
    }

    /// Handle a Niri event.
    ///
    /// Returns (workspace_changed, window_changed, keyboard_layout_changed).
    fn handle_event(shared: &SharedState, event: &Value) -> (bool, bool, bool) {
        super::visibility::notify_changed();
        let mut workspace_changed = false;
        let mut window_changed = false;
        let mut keyboard_layout_changed = false;

        if let Some(kb_layouts_changed) = event.get("KeyboardLayoutsChanged") {
            // Full layout list changed (e.g., user reconfigured layouts)
            if let Some(kb_layouts) = kb_layouts_changed.get("keyboard_layouts") {
                keyboard_layout_changed = Self::process_keyboard_layouts(shared, kb_layouts);
            }
        } else if let Some(kb_switched) = event.get("KeyboardLayoutSwitched") {
            // Just switched to a different layout by index
            if let Some(idx) = kb_switched.get("idx").and_then(|v| v.as_u64()) {
                keyboard_layout_changed =
                    Self::process_keyboard_layout_switch(shared, idx as usize);
            }
        } else if let Some(workspaces_changed) = event.get("WorkspacesChanged") {
            if let Some(workspaces) = workspaces_changed
                .get("workspaces")
                .and_then(|v| v.as_array())
            {
                Self::process_workspaces(shared, workspaces);
                workspace_changed = true;
                // Workspaces may have moved between outputs.
                window_changed = true;
            }
        } else if let Some(workspace_activated) = event.get("WorkspaceActivated") {
            let ws_niri_id = workspace_activated.get("id").and_then(|v| v.as_u64());
            let is_focused = workspace_activated
                .get("focused")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if let Some(ws_id) = ws_niri_id {
                let stable_id = ws_id as i32;
                let id_to_output = shared.id_to_output.read();
                let output = id_to_output.get(&ws_id).cloned();
                drop(id_to_output);

                let mut snapshot = shared.workspace_snapshot.write();

                if is_focused && !snapshot.active_workspace.contains(&stable_id) {
                    snapshot.active_workspace.clear();
                    snapshot.active_workspace.insert(stable_id);
                    workspace_changed = true;
                }

                if let Some(ref out_name) = output
                    && let Some(per_out) = snapshot.per_output.get_mut(out_name)
                    && !per_out.active_workspace.contains(&stable_id)
                {
                    per_out.active_workspace.clear();
                    per_out.active_workspace.insert(stable_id);
                    workspace_changed = true;
                }

                drop(snapshot);

                // Workspace switched - update per-output windows
                Self::update_per_output_windows(shared);
                window_changed = true;
            }
        } else if let Some(urgency_changed) = event.get("WorkspaceUrgencyChanged") {
            if let Some(ws_id) = urgency_changed.get("id").and_then(|v| v.as_u64()) {
                let is_urgent = urgency_changed
                    .get("urgent")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                let stable_id = ws_id as i32;
                let mut snapshot = shared.workspace_snapshot.write();
                if is_urgent {
                    workspace_changed = snapshot.urgent_workspaces.insert(stable_id);
                } else {
                    workspace_changed = snapshot.urgent_workspaces.remove(&stable_id);
                }
            }
        } else if let Some(windows_changed) = event.get("WindowsChanged") {
            if let Some(windows) = windows_changed.get("windows").and_then(|v| v.as_array()) {
                Self::process_windows(shared, windows);
                window_changed = true;
            }
        } else if let Some(window_opened) = event.get("WindowOpenedOrChanged") {
            if let Some(window) = window_opened.get("window") {
                Self::update_single_window(shared, window);

                if let Some(ws_id) = window.get("workspace_id").and_then(|v| v.as_u64()) {
                    let stable_id = ws_id as i32;
                    let mut snapshot = shared.workspace_snapshot.write();
                    if snapshot.occupied_workspaces.insert(stable_id) {
                        workspace_changed = true;
                    }
                }

                // Window opened/changed - update per-output windows
                Self::update_per_output_windows(shared);
                window_changed = true;
            }
        } else if let Some(layouts_changed) = event.get("WindowLayoutsChanged") {
            // changes is Vec<(u64, WindowLayout)> which serializes as an array of tuples:
            // [[window_id, {layout_obj}], ...]
            if let Some(changes) = layouts_changed.get("changes").and_then(|v| v.as_array()) {
                let mut win_cache = shared.windows.write();
                for entry in changes {
                    let entry = match entry.as_array() {
                        Some(arr) if arr.len() >= 2 => arr,
                        _ => continue,
                    };
                    let win_id = match entry[0].as_u64() {
                        Some(id) => id,
                        None => continue,
                    };
                    if let Some(win) = win_cache.get_mut(&win_id) {
                        // entry[1] is a WindowLayout object with pos_in_scrolling_layout directly
                        win.layout_position = entry[1]
                            .get("pos_in_scrolling_layout")
                            .and_then(|v| v.as_array())
                            .and_then(|arr| {
                                if arr.len() >= 2 {
                                    Some((arr[0].as_i64()? as i32, arr[1].as_i64()? as i32))
                                } else {
                                    None
                                }
                            });
                    }
                }
                window_changed = true;
            }
        } else if let Some(window_closed) = event.get("WindowClosed") {
            if let Some(win_id) = window_closed.get("id").and_then(|v| v.as_u64()) {
                shared.windows.write().remove(&win_id);
                Self::update_window_counts(shared);
                Self::update_focused_window_from_cache(shared);
                Self::update_per_output_windows(shared);
                window_changed = true;
                workspace_changed = true;
            }
        } else if let Some(focus_changed) = event.get("WindowFocusChanged") {
            let win_id = focus_changed.get("id").and_then(|v| v.as_u64());
            let mut win_cache = shared.windows.write();
            for win in win_cache.values_mut() {
                win.is_focused = win_id.is_some_and(|id| win.id == id);
            }
            drop(win_cache);
            Self::update_focused_window_from_cache(shared);
            Self::update_per_output_windows(shared);
            window_changed = true;
        } else if let Some(urgency_changed) = event.get("WindowUrgencyChanged") {
            let win_id = urgency_changed.get("id").and_then(|v| v.as_u64());
            let is_urgent = urgency_changed
                .get("urgent")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if let Some(win_id) = win_id {
                let mut win_cache = shared.windows.write();
                if let Some(win) = win_cache.get_mut(&win_id)
                    && win.is_urgent != is_urgent
                {
                    win.is_urgent = is_urgent;
                    window_changed = true;
                }
            }
        } else if let Some(active_changed) = event.get("WorkspaceActiveWindowChanged")
            && let Some(ws_id) = active_changed.get("workspace_id").and_then(|v| v.as_u64())
        {
            let active_win_id = active_changed
                .get("active_window_id")
                .and_then(|v| v.as_u64());
            {
                let mut active_windows = shared.active_windows.write();
                match active_win_id {
                    Some(win_id) => active_windows.insert(ws_id, win_id),
                    None => active_windows.remove(&ws_id),
                };
            }
            Self::update_per_output_windows(shared);
            window_changed = true;
        }

        (workspace_changed, window_changed, keyboard_layout_changed)
    }

    /// Run the event loop (in background thread).
    fn event_loop(
        running: Arc<AtomicBool>,
        shared: Arc<SharedState>,
        socket_path: String,
        callbacks: Option<(WorkspaceCallback, WindowCallback)>,
        kb_callback: Option<KeyboardLayoutCallback>,
        window_list_callback: Option<super::WindowListCallback>,
    ) {
        // Fetch initial state
        Self::fetch_initial_state(&socket_path, &shared);

        // Emit initial state
        if let Some((ref ws_cb, ref win_cb)) = callbacks {
            ws_cb(shared.workspace_snapshot.read().clone());
            // Emit window info for all outputs (including empty info for outputs with no active window)
            let per_output = shared.per_output_window.read();
            for win_info in per_output.values() {
                win_cb(win_info.clone());
            }
        }

        // Emit the full initial window list for taskbar consumers.
        if let Some(ref wl_cb) = window_list_callback {
            let windows = Self::get_windows_from_shared(&shared);
            wl_cb(super::WindowListSnapshot { windows });
        }

        // Emit initial keyboard layout
        if let Some(ref kb_cb) = kb_callback
            && let Some(ref info) = *shared.keyboard_layout.read()
        {
            kb_cb(info.clone());
        }

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

                                let (ws_changed, win_changed, kb_changed) =
                                    Self::handle_event(&shared, &event);

                                if let Some((ref ws_cb, ref win_cb)) = callbacks {
                                    if ws_changed {
                                        ws_cb(shared.workspace_snapshot.read().clone());
                                    }
                                    if win_changed {
                                        // Emit updates for all outputs with their current active window
                                        let per_output = shared.per_output_window.read();
                                        for win_info in per_output.values() {
                                            win_cb(win_info.clone());
                                        }
                                    }
                                }

                                if kb_changed
                                    && let Some(ref kb_cb) = kb_callback
                                    && let Some(ref info) = *shared.keyboard_layout.read()
                                {
                                    kb_cb(info.clone());
                                }

                                // Workspace metadata changes can affect taskbar ordering and
                                // separator labels: windows are sorted by workspace idx, and
                                // labels resolve stable workspace IDs back to idx/name metadata.
                                if (win_changed || ws_changed)
                                    && let Some(ref wl_cb) = window_list_callback
                                {
                                    let windows = Self::get_windows_from_shared(&shared);
                                    wl_cb(super::WindowListSnapshot { windows });
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

        // Clone shared state and running flag for the thread
        let running = Arc::clone(&self.running);
        let shared = Arc::clone(&self.shared);
        let callbacks = Some((on_workspace_update, on_window_update));
        let kb_callback = self
            .keyboard_layout_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let window_list_callback = self
            .window_list_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        // Start event loop thread
        let handle = thread::Builder::new()
            .name("niri-event-loop".into())
            .spawn(move || {
                Self::event_loop(
                    running,
                    shared,
                    socket_path,
                    callbacks,
                    kb_callback,
                    window_list_callback,
                );
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
                    idx: i,
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
        self.shared.keyboard_layout.read().clone()
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
        Self::get_windows_from_shared(&self.shared)
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

    fn shown(shared: &SharedState, output: &str) -> (String, bool) {
        let per_output = shared.per_output_window.read();
        let info = per_output.get(output).expect("output has an entry");
        (info.title.clone(), info.focused)
    }

    fn shown_focused(shared: &SharedState) -> String {
        let per_output = shared.per_output_window.read();
        let focused: Vec<_> = per_output.values().filter(|info| info.focused).collect();
        assert_eq!(focused.len(), 1, "exactly one focused output");
        focused[0].title.clone()
    }

    /// Workspace 1 (DP-1, focused) has five windows with B (id 4) active;
    /// workspace 2 (DP-1) is empty; DP-2 shows workspace 3 with window X and
    /// hides workspace 4 with window Y.
    fn setup() -> SharedState {
        let shared = SharedState::default();
        NiriBackend::handle_event(
            &shared,
            &serde_json::json!({"WorkspacesChanged": {"workspaces": [
                workspace(1, "DP-1", true, true, Some(4)),
                workspace(2, "DP-1", false, false, None),
                workspace(3, "DP-2", true, false, Some(10)),
                workspace(4, "DP-2", false, false, Some(11)),
            ]}}),
        );
        NiriBackend::handle_event(
            &shared,
            &serde_json::json!({"WindowsChanged": {"windows": [
                window(1, 1, "A1", false),
                window(2, 1, "A2", false),
                window(3, 1, "A3", false),
                window(4, 1, "B", true),
                window(5, 1, "A5", false),
                window(10, 3, "X", false),
                window(11, 4, "Y", false),
            ]}}),
        );
        shared
    }

    #[test]
    fn layer_shell_focus_keeps_the_active_window() {
        let shared = setup();
        assert_eq!(shown_focused(&shared), "B");

        // A layer-shell surface (e.g. a popover) takes the keyboard.
        let (_, changed, _) = NiriBackend::handle_event(
            &shared,
            &serde_json::json!({"WindowFocusChanged": {"id": null}}),
        );
        assert!(changed);
        assert_eq!(shown(&shared, "DP-1"), ("B".to_string(), true));

        // The title still updates while the popover has focus.
        NiriBackend::handle_event(
            &shared,
            &serde_json::json!({"WindowOpenedOrChanged": {
                "window": window(4, 1, "B renamed", false)
            }}),
        );
        assert_eq!(shown_focused(&shared), "B renamed");

        // Focus returns to the window.
        NiriBackend::handle_event(
            &shared,
            &serde_json::json!({"WindowFocusChanged": {"id": 4}}),
        );
        assert_eq!(shown_focused(&shared), "B renamed");
    }

    #[test]
    fn unfocused_output_shows_its_active_window() {
        let shared = setup();
        assert_eq!(shown(&shared, "DP-2"), ("X".to_string(), false));

        // An active-window change on a hidden workspace leaves DP-2 alone.
        NiriBackend::handle_event(
            &shared,
            &serde_json::json!({"WorkspaceActiveWindowChanged": {
                "workspace_id": 4, "active_window_id": 11
            }}),
        );
        assert_eq!(shown(&shared, "DP-2"), ("X".to_string(), false));
    }

    #[test]
    fn empty_workspace_clears_the_title() {
        let shared = setup();
        NiriBackend::handle_event(
            &shared,
            &serde_json::json!({"WorkspaceActivated": {"id": 2, "focused": true}}),
        );
        NiriBackend::handle_event(
            &shared,
            &serde_json::json!({"WindowFocusChanged": {"id": null}}),
        );

        assert_eq!(shown(&shared, "DP-1"), (String::new(), true));
    }

    fn apply(shared: &SharedState, event: serde_json::Value) -> (bool, bool, bool) {
        NiriBackend::handle_event(shared, &event)
    }

    fn snapshot(shared: &SharedState) -> WorkspaceSnapshot {
        shared.workspace_snapshot.read().clone()
    }

    fn focused_window(shared: &SharedState) -> Option<WindowInfo> {
        shared.focused_window.read().clone()
    }

    fn keyboard_layout(shared: &SharedState) -> Option<KeyboardLayoutInfo> {
        shared.keyboard_layout.read().clone()
    }

    fn window_ids(shared: &SharedState) -> Vec<u64> {
        NiriBackend::get_windows_from_shared(shared)
            .iter()
            .map(|win| win.id)
            .collect()
    }

    fn set<const N: usize>(ids: [i32; N]) -> std::collections::HashSet<i32> {
        ids.into_iter().collect()
    }

    fn counts<const N: usize>(entries: [(i32, u32); N]) -> HashMap<i32, u32> {
        entries.into_iter().collect()
    }

    #[test]
    fn snapshot_reports_workspaces_and_window_counts() {
        let shared = setup();
        let snapshot = snapshot(&shared);

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
        let shared = setup();

        apply(
            &shared,
            serde_json::json!({"WindowOpenedOrChanged": {"window": window(5, 2, "A5", false)}}),
        );
        assert_eq!(
            snapshot(&shared).window_counts,
            counts([(1, 4), (2, 1), (3, 1), (4, 1)])
        );

        let (ws_changed, win_changed, _) =
            apply(&shared, serde_json::json!({"WindowClosed": {"id": 10}}));
        assert!(ws_changed && win_changed);
        let snapshot = snapshot(&shared);
        assert_eq!(snapshot.window_counts[&3], 0);
        assert_eq!(snapshot.per_output["DP-2"].window_counts[&3], 0);
        assert_eq!(window_ids(&shared), vec![1, 2, 3, 4, 5, 11]);
    }

    #[test]
    fn workspaces_changed_keeps_window_counts() {
        let shared = setup();
        apply(
            &shared,
            serde_json::json!({"WorkspacesChanged": {"workspaces": [
                workspace(1, "DP-1", false, false, Some(4)),
                workspace(2, "DP-1", true, true, None),
                workspace(3, "DP-2", true, false, Some(10)),
                workspace(4, "DP-2", false, false, Some(11)),
            ]}}),
        );

        let snapshot = snapshot(&shared);
        assert_eq!(snapshot.active_workspace, set([2]));
        assert_eq!(snapshot.per_output["DP-1"].active_workspace, set([2]));
        assert_eq!(
            snapshot.window_counts,
            counts([(1, 5), (2, 0), (3, 1), (4, 1)])
        );
    }

    #[test]
    fn workspace_activation_updates_global_and_output_state() {
        let shared = setup();

        // Activating without focus only changes what the output shows.
        let (ws_changed, _, _) = apply(
            &shared,
            serde_json::json!({"WorkspaceActivated": {"id": 4, "focused": false}}),
        );
        assert!(ws_changed);
        let snap = snapshot(&shared);
        assert_eq!(snap.active_workspace, set([1]));
        assert_eq!(snap.per_output["DP-1"].active_workspace, set([1]));
        assert_eq!(snap.per_output["DP-2"].active_workspace, set([4]));
        assert_eq!(shown(&shared, "DP-2"), ("Y".to_string(), false));

        // Focusing a workspace on another output moves global focus there.
        apply(
            &shared,
            serde_json::json!({"WorkspaceActivated": {"id": 3, "focused": true}}),
        );
        let snap = snapshot(&shared);
        assert_eq!(snap.active_workspace, set([3]));
        assert_eq!(snap.per_output["DP-1"].active_workspace, set([1]));
        assert_eq!(snap.per_output["DP-2"].active_workspace, set([3]));
        assert_eq!(shown_focused(&shared), "X");
    }

    #[test]
    fn urgency_is_tracked_for_workspaces_and_windows() {
        let shared = setup();

        let urgent = serde_json::json!({"WorkspaceUrgencyChanged": {"id": 2, "urgent": true}});
        assert!(apply(&shared, urgent.clone()).0);
        assert_eq!(snapshot(&shared).urgent_workspaces, set([2]));
        assert!(!apply(&shared, urgent).0);

        apply(
            &shared,
            serde_json::json!({"WorkspaceUrgencyChanged": {"id": 2, "urgent": false}}),
        );
        assert!(snapshot(&shared).urgent_workspaces.is_empty());

        let urgent = serde_json::json!({"WindowUrgencyChanged": {"id": 3, "urgent": true}});
        assert!(apply(&shared, urgent.clone()).1);
        assert!(!apply(&shared, urgent).1);
        let urgent_windows: Vec<u64> = NiriBackend::get_windows_from_shared(&shared)
            .iter()
            .filter(|win| win.is_urgent)
            .map(|win| win.id)
            .collect();
        assert_eq!(urgent_windows, vec![3]);
    }

    #[test]
    fn window_list_follows_output_workspace_and_layout_order() {
        let shared = setup();
        assert_eq!(window_ids(&shared), vec![1, 2, 3, 4, 5, 10, 11]);

        apply(
            &shared,
            serde_json::json!({"WindowLayoutsChanged": {"changes": [
                [5, {"pos_in_scrolling_layout": [1, 1]}],
                [3, {"pos_in_scrolling_layout": [2, 1]}],
                [1, {"pos_in_scrolling_layout": [2, 2]}],
            ]}}),
        );
        assert_eq!(window_ids(&shared), vec![5, 3, 1, 2, 4, 10, 11]);

        // A full window update carries its layout position too.
        let mut moved = window(11, 3, "Y", false);
        moved["layout"] = serde_json::json!({"pos_in_scrolling_layout": [1, 1]});
        apply(
            &shared,
            serde_json::json!({"WindowOpenedOrChanged": {"window": moved}}),
        );
        assert_eq!(window_ids(&shared), vec![5, 3, 1, 2, 4, 11, 10]);

        let windows = NiriBackend::get_windows_from_shared(&shared);
        let y = windows.iter().find(|win| win.id == 11).unwrap();
        assert_eq!(y.workspace_id, Some(3));
        assert_eq!(y.output.as_deref(), Some("DP-2"));
        assert_eq!(y.app_id, "Y-app");
    }

    #[test]
    fn focused_window_follows_focus_and_close() {
        let shared = setup();
        assert_eq!(
            focused_window(&shared),
            Some(WindowInfo {
                title: "B".to_string(),
                app_id: "B-app".to_string(),
                output: Some("DP-1".to_string()),
                focused: true,
            })
        );

        apply(
            &shared,
            serde_json::json!({"WindowFocusChanged": {"id": 10}}),
        );
        let focused = focused_window(&shared).unwrap();
        assert_eq!(focused.title, "X");
        assert_eq!(focused.output.as_deref(), Some("DP-2"));

        // Opening a focused window takes focus from the previous one.
        apply(
            &shared,
            serde_json::json!({"WindowOpenedOrChanged": {"window": window(20, 1, "New", true)}}),
        );
        assert_eq!(focused_window(&shared).unwrap().title, "New");
        let focused_ids: Vec<u64> = NiriBackend::get_windows_from_shared(&shared)
            .iter()
            .filter(|win| win.is_focused)
            .map(|win| win.id)
            .collect();
        assert_eq!(focused_ids, vec![20]);

        apply(&shared, serde_json::json!({"WindowClosed": {"id": 20}}));
        assert_eq!(focused_window(&shared), None);
    }

    #[test]
    fn keyboard_layout_follows_list_and_switch() {
        let shared = setup();
        assert_eq!(keyboard_layout(&shared), None);

        let (_, _, changed) = apply(
            &shared,
            serde_json::json!({"KeyboardLayoutsChanged": {"keyboard_layouts": {
                "names": ["English (US)", "Swedish"], "current_idx": 0
            }}}),
        );
        assert!(changed);
        assert_eq!(
            keyboard_layout(&shared),
            Some(KeyboardLayoutInfo {
                layout_name: "English (US)".to_string(),
                layout_count: Some(2),
            })
        );

        let (_, _, changed) = apply(
            &shared,
            serde_json::json!({"KeyboardLayoutSwitched": {"idx": 1}}),
        );
        assert!(changed);
        assert_eq!(keyboard_layout(&shared).unwrap().layout_name, "Swedish");
    }
}
