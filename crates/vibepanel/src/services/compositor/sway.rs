//! Sway-compatible compositor backend using i3 IPC protocol.
//!
//! Supports compositors that implement the Sway IPC protocol:
//! - Sway ($SWAYSOCK)
//! - Miracle WM ($MIRACLESOCK)
//! - Scroll ($SWAYSOCK)
//!
//! Protocol: i3 IPC binary framing with JSON payloads.
//! Uses a re-fetch strategy on events for simplicity.
//!
//! Reference: https://man.archlinux.org/man/sway-ipc.7.en

use std::collections::HashMap;
use std::env;
use std::io::{Read, Write};
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

// i3 IPC constants
const IPC_MAGIC: &[u8; 6] = b"i3-ipc";
const IPC_HEADER_SIZE: usize = 14; // 6 (magic) + 4 (length) + 4 (type)

// Message types (outgoing)
const IPC_RUN_COMMAND: u32 = 0;
const IPC_GET_WORKSPACES: u32 = 1;
const IPC_SUBSCRIBE: u32 = 2;
const IPC_GET_TREE: u32 = 4;

// Event types have bit 31 set in the response type
const IPC_EVENT_BIT: u32 = 1 << 31;
const IPC_EVENT_WORKSPACE: u32 = IPC_EVENT_BIT; // event type 0
const IPC_EVENT_WINDOW: u32 = IPC_EVENT_BIT | 3;

const RECONNECT_INITIAL_MS: u64 = 1000;
const RECONNECT_MAX_MS: u64 = 30000;
const RECONNECT_MULTIPLIER: f64 = 1.5;

/// Default workspace count (dynamic workspaces, expose 1-10).
const DEFAULT_WORKSPACE_COUNT: i32 = 10;

/// Reject IPC payloads larger than this to guard against bogus length fields.
const MAX_IPC_PAYLOAD: usize = 64 * 1024 * 1024; // 64 MB

/// Map a named workspace string to a stable synthetic i32 ID.
///
/// Sway assigns `num: -1` to all named workspaces, so they lack a unique numeric
/// identity. This function produces a deterministic ID from the name using FNV-1a,
/// mapped into [-2_000_000_000, -1_000_000_001] to avoid collisions with:
///   - Positive values and 0 (numbered workspaces)
///   - -1 (Sway's sentinel for named workspaces)
///   - Values near i32::MIN (used as stop signal by MangoWC backend)
fn synthetic_id_for_name(name: &str) -> i32 {
    // FNV-1a 32-bit
    let mut hash: u32 = 0x811c_9dc5;
    for byte in name.as_bytes() {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    // Map into range [-2_000_000_000, -1_000_000_001] (999_999_000 values)
    let range: u32 = 999_999_000;
    let offset = (hash % range) as i32;
    -2_000_000_000 + offset
}

fn ipc_send(stream: &mut UnixStream, msg_type: u32, payload: &[u8]) -> std::io::Result<()> {
    let len = payload.len() as u32;
    let mut header = [0u8; IPC_HEADER_SIZE];
    header[..6].copy_from_slice(IPC_MAGIC);
    header[6..10].copy_from_slice(&len.to_le_bytes());
    header[10..14].copy_from_slice(&msg_type.to_le_bytes());
    stream.write_all(&header)?;
    if !payload.is_empty() {
        stream.write_all(payload)?;
    }
    Ok(())
}

/// Read an i3 IPC message from the given stream.
/// Returns (message_type, payload_bytes).
fn ipc_recv(stream: &mut UnixStream) -> std::io::Result<(u32, Vec<u8>)> {
    let mut header = [0u8; IPC_HEADER_SIZE];
    stream.read_exact(&mut header)?;

    if &header[..6] != IPC_MAGIC {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid i3-ipc magic",
        ));
    }

    let len = u32::from_le_bytes([header[6], header[7], header[8], header[9]]) as usize;
    let msg_type = u32::from_le_bytes([header[10], header[11], header[12], header[13]]);

    if len > MAX_IPC_PAYLOAD {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("IPC payload too large: {} bytes", len),
        ));
    }

    let mut payload = vec![0u8; len];
    if len > 0 {
        stream.read_exact(&mut payload)?;
    }

    Ok((msg_type, payload))
}

/// Send an IPC request on a fresh connection and return the JSON response.
fn ipc_request(socket_path: &str, msg_type: u32, payload: &[u8], wm: &str) -> Option<Value> {
    let mut stream = match UnixStream::connect(socket_path) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to connect to {} socket: {}", wm, e);
            return None;
        }
    };

    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

    if let Err(e) = ipc_send(&mut stream, msg_type, payload) {
        error!("Failed to send IPC message to {}: {}", wm, e);
        return None;
    }

    match ipc_recv(&mut stream) {
        Ok((_msg_type, data)) => match serde_json::from_slice(&data) {
            Ok(v) => Some(v),
            Err(e) => {
                trace!("Failed to parse JSON from {}: {}", wm, e);
                None
            }
        },
        Err(e) => {
            error!("Failed to read IPC response from {}: {}", wm, e);
            None
        }
    }
}

struct SharedState {
    workspace_snapshot: RwLock<WorkspaceSnapshot>,
    focused_window: RwLock<Option<WindowInfo>>,
    workspaces: RwLock<Vec<WorkspaceMeta>>,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            workspace_snapshot: RwLock::new(WorkspaceSnapshot::default()),
            focused_window: RwLock::new(None),
            workspaces: RwLock::new(
                (1..=DEFAULT_WORKSPACE_COUNT)
                    .map(|i| WorkspaceMeta {
                        id: i,
                        idx: i,
                        name: i.to_string(),
                        output: None,
                    })
                    .collect(),
            ),
        }
    }
}

pub struct SwayBackend {
    running: Arc<AtomicBool>,
    event_thread: Mutex<Option<JoinHandle<()>>>,
    socket_path: RwLock<Option<String>>,
    shared: Arc<SharedState>,
    callbacks: Mutex<Option<(WorkspaceCallback, WindowCallback)>>,
    compositor_name: &'static str,
    socket_env_var: &'static str,
}

impl SwayBackend {
    pub fn new(_outputs: Option<Vec<String>>) -> Self {
        let (compositor_name, socket_env_var) = if env::var("MIRACLESOCK").is_ok() {
            ("Miracle WM", "MIRACLESOCK")
        } else {
            ("Sway", "SWAYSOCK")
        };

        Self {
            running: Arc::new(AtomicBool::new(false)),
            event_thread: Mutex::new(None),
            socket_path: RwLock::new(None),
            shared: Arc::new(SharedState::default()),
            callbacks: Mutex::new(None),
            compositor_name,
            socket_env_var,
        }
    }

    fn fetch_workspaces(socket_path: &str, shared: &SharedState, wm: &str) {
        let Some(response) = ipc_request(socket_path, IPC_GET_WORKSPACES, b"", wm) else {
            warn!("Failed to fetch workspaces from {}", wm);
            return;
        };

        let Some(workspaces) = response.as_array() else {
            warn!("{} GET_WORKSPACES response is not an array", wm);
            return;
        };

        let mut ws_list = shared.workspaces.write();
        let mut snapshot = shared.workspace_snapshot.write();

        ws_list.clear();
        snapshot.active_workspace.clear();
        snapshot.occupied_workspaces.clear();
        snapshot.urgent_workspaces.clear();
        snapshot.window_counts.clear();
        snapshot.per_output.clear();

        let mut numbered: Vec<WorkspaceMeta> = Vec::new();
        let mut named: Vec<WorkspaceMeta> = Vec::new();

        for ws in workspaces {
            let num = ws.get("num").and_then(|v| v.as_i64()).unwrap_or(-1) as i32;
            let name = ws
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            // Skip the scratchpad pseudo-workspace
            if name == "__i3_scratch" {
                continue;
            }

            let output = ws
                .get("output")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let focused = ws.get("focused").and_then(|v| v.as_bool()).unwrap_or(false);
            let visible = ws.get("visible").and_then(|v| v.as_bool()).unwrap_or(false);
            let urgent = ws.get("urgent").and_then(|v| v.as_bool()).unwrap_or(false);

            // Named workspaces have num == -1 in Sway; assign a synthetic ID
            let ws_id = if num < 0 {
                synthetic_id_for_name(&name)
            } else {
                num
            };

            let meta = WorkspaceMeta {
                id: ws_id,
                idx: 0, // Assigned below after sorting
                name: if name.is_empty() {
                    num.to_string()
                } else {
                    name
                },
                output: output.clone(),
            };

            if num < 0 {
                named.push(meta);
            } else {
                numbered.push(meta);
            }

            if focused {
                snapshot.active_workspace.insert(ws_id);
            }

            if urgent {
                snapshot.urgent_workspaces.insert(ws_id);
            }

            // Sway auto-destroys empty workspaces, so all existing ones are occupied
            snapshot.occupied_workspaces.insert(ws_id);

            if let Some(ref out_name) = output {
                let per_out = snapshot.per_output.entry(out_name.clone()).or_default();

                per_out.occupied_workspaces.insert(ws_id);

                if visible {
                    per_out.active_workspace.insert(ws_id);
                }
            }
        }

        // Numbered first (sorted by num), then named (sorted alphabetically)
        numbered.sort_by_key(|ws| ws.id);
        named.sort_by(|a, b| a.name.cmp(&b.name));

        ws_list.extend(numbered);
        ws_list.extend(named);

        // Assign 1-based positional index for display
        for (i, ws) in ws_list.iter_mut().enumerate() {
            ws.idx = (i + 1) as i32;
        }
    }

    /// Walk the tree to count windows per workspace and find the focused window.
    ///
    /// The tree is a hierarchy: root -> outputs -> workspaces -> containers -> windows.
    fn fetch_tree(socket_path: &str, shared: &SharedState, wm: &str) {
        let Some(tree) = ipc_request(socket_path, IPC_GET_TREE, b"", wm) else {
            warn!("Failed to fetch tree from {}", wm);
            return;
        };

        let mut window_counts: HashMap<i32, u32> = HashMap::new();
        let mut per_output_counts: HashMap<String, HashMap<i32, u32>> = HashMap::new();
        let mut focused_window: Option<WindowInfo> = None;

        if let Some(outputs) = tree.get("nodes").and_then(|v| v.as_array()) {
            for output in outputs {
                let output_name = output
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                // Skip the __i3 root output (internal)
                if output_name == "__i3" {
                    continue;
                }

                let workspace_nodes = output.get("nodes").and_then(|v| v.as_array());
                if let Some(ws_nodes) = workspace_nodes {
                    for ws_node in ws_nodes {
                        let ws_num =
                            ws_node.get("num").and_then(|v| v.as_i64()).unwrap_or(-1) as i32;
                        let ws_name = ws_node.get("name").and_then(|v| v.as_str()).unwrap_or("");

                        if ws_name == "__i3_scratch" {
                            continue;
                        }

                        let ws_id = if ws_num < 0 {
                            synthetic_id_for_name(ws_name)
                        } else {
                            ws_num
                        };

                        let count = Self::count_windows(ws_node);
                        *window_counts.entry(ws_id).or_insert(0) += count;
                        *per_output_counts
                            .entry(output_name.clone())
                            .or_default()
                            .entry(ws_id)
                            .or_insert(0) += count;

                        if let Some(win) = Self::find_focused_window(ws_node, ws_id, &output_name) {
                            focused_window = Some(win);
                        }
                    }
                }
            }
        }

        {
            let mut snapshot = shared.workspace_snapshot.write();
            snapshot.window_counts = window_counts;

            // per_output entries are populated by the prior fetch_workspaces call
            for (out_name, counts) in &per_output_counts {
                if let Some(per_out) = snapshot.per_output.get_mut(out_name) {
                    per_out.window_counts = counts.clone();
                }
            }
        }

        *shared.focused_window.write() = focused_window;
    }

    /// Count leaf windows (application windows) in a container subtree.
    ///
    /// Uses `pid > 0` to identify windows (per Sway IPC spec, `pid` is set only
    /// on windows), which avoids false-counting empty split containers.
    fn count_windows(node: &Value) -> u32 {
        let children = node.get("nodes").and_then(|v| v.as_array());
        let floating = node.get("floating_nodes").and_then(|v| v.as_array());

        let has_children =
            children.is_some_and(|c| !c.is_empty()) || floating.is_some_and(|f| !f.is_empty());

        if !has_children {
            let is_window = node
                .get("pid")
                .and_then(|v| v.as_i64())
                .is_some_and(|p| p > 0);

            return if is_window { 1 } else { 0 };
        }

        let mut count = 0;
        if let Some(children) = children {
            for child in children {
                count += Self::count_windows(child);
            }
        }
        if let Some(floating) = floating {
            for child in floating {
                count += Self::count_windows(child);
            }
        }
        count
    }

    fn find_focused_window(
        node: &Value,
        workspace_num: i32,
        output_name: &str,
    ) -> Option<WindowInfo> {
        let focused = node
            .get("focused")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let children = node.get("nodes").and_then(|v| v.as_array());
        let floating = node.get("floating_nodes").and_then(|v| v.as_array());

        let has_children =
            children.is_some_and(|c| !c.is_empty()) || floating.is_some_and(|f| !f.is_empty());

        if focused && !has_children {
            let node_type = node.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if node_type == "workspace" {
                return None; // Focused workspace with no focused window
            }

            let app_id = node
                .get("app_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            // For XWayland windows, app_id may be null; fall back to window_properties.class
            let app_id = if app_id.is_empty() {
                node.get("window_properties")
                    .and_then(|wp| wp.get("class"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            } else {
                app_id
            };

            let title = node
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            return Some(WindowInfo {
                title,
                app_id,
                workspace_id: Some(workspace_num),
                output: Some(output_name.to_string()),
            });
        }

        if let Some(children) = children {
            for child in children {
                if let Some(win) = Self::find_focused_window(child, workspace_num, output_name) {
                    return Some(win);
                }
            }
        }
        if let Some(floating) = floating {
            for child in floating {
                if let Some(win) = Self::find_focused_window(child, workspace_num, output_name) {
                    return Some(win);
                }
            }
        }

        None
    }

    fn fetch_all(socket_path: &str, shared: &SharedState, wm: &str) {
        Self::fetch_workspaces(socket_path, shared, wm);
        Self::fetch_tree(socket_path, shared, wm);
    }

    /// Run the event loop (in background thread).
    fn event_loop(
        running: Arc<AtomicBool>,
        shared: Arc<SharedState>,
        socket_path: String,
        callbacks: Option<(WorkspaceCallback, WindowCallback)>,
        wm: &str,
    ) {
        Self::fetch_all(&socket_path, &shared, wm);

        if let Some((ref ws_cb, ref win_cb)) = callbacks {
            ws_cb(shared.workspace_snapshot.read().clone());
            if let Some(ref win) = *shared.focused_window.read() {
                win_cb(win.clone());
            } else {
                win_cb(WindowInfo::default());
            }
        }

        let mut backoff_ms = RECONNECT_INITIAL_MS;

        while running.load(Ordering::SeqCst) {
            let mut stream = match UnixStream::connect(&socket_path) {
                Ok(s) => {
                    backoff_ms = RECONNECT_INITIAL_MS;
                    s
                }
                Err(e) => {
                    if running.load(Ordering::SeqCst) {
                        warn!(
                            "Failed to connect to {} socket: {}. Retrying in {}ms",
                            wm, e, backoff_ms
                        );
                        thread::sleep(Duration::from_millis(backoff_ms));
                        backoff_ms = ((backoff_ms as f64) * RECONNECT_MULTIPLIER)
                            .min(RECONNECT_MAX_MS as f64)
                            as u64;
                    }
                    continue;
                }
            };

            let subscribe_payload = b"[\"workspace\", \"window\"]";
            if let Err(e) = ipc_send(&mut stream, IPC_SUBSCRIBE, subscribe_payload) {
                if running.load(Ordering::SeqCst) {
                    warn!(
                        "Failed to subscribe to {} events: {}. Retrying in {}ms",
                        wm, e, backoff_ms
                    );
                    thread::sleep(Duration::from_millis(backoff_ms));
                    backoff_ms = ((backoff_ms as f64) * RECONNECT_MULTIPLIER)
                        .min(RECONNECT_MAX_MS as f64) as u64;
                }
                continue;
            }

            match ipc_recv(&mut stream) {
                Ok((_msg_type, data)) => {
                    if let Ok(resp) = serde_json::from_slice::<Value>(&data) {
                        let success = resp
                            .get("success")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if !success {
                            warn!("{} subscribe failed: {:?}", wm, resp);
                            thread::sleep(Duration::from_millis(backoff_ms));
                            backoff_ms = ((backoff_ms as f64) * RECONNECT_MULTIPLIER)
                                .min(RECONNECT_MAX_MS as f64)
                                as u64;
                            continue;
                        }
                    }
                }
                Err(e) => {
                    if running.load(Ordering::SeqCst) {
                        warn!(
                            "Failed to read {} subscribe response: {}. Retrying in {}ms",
                            wm, e, backoff_ms
                        );
                        thread::sleep(Duration::from_millis(backoff_ms));
                        backoff_ms = ((backoff_ms as f64) * RECONNECT_MULTIPLIER)
                            .min(RECONNECT_MAX_MS as f64)
                            as u64;
                    }
                    continue;
                }
            }

            // 1s read timeout so we can check the `running` flag between events
            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));

            loop {
                if !running.load(Ordering::SeqCst) {
                    break;
                }

                match ipc_recv(&mut stream) {
                    Ok((msg_type, data)) => {
                        let event: Value = match serde_json::from_slice(&data) {
                            Ok(v) => v,
                            Err(e) => {
                                trace!("Failed to parse {} event JSON: {}", wm, e);
                                continue;
                            }
                        };

                        let (ws_changed, win_changed) =
                            Self::handle_event(msg_type, &event, &socket_path, &shared, wm);

                        if let Some((ref ws_cb, ref win_cb)) = callbacks {
                            if ws_changed {
                                ws_cb(shared.workspace_snapshot.read().clone());
                            }
                            if win_changed {
                                if let Some(ref win) = *shared.focused_window.read() {
                                    win_cb(win.clone());
                                } else {
                                    win_cb(WindowInfo::default());
                                }
                            }
                        }
                    }
                    Err(e) => {
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut
                        {
                            continue; // Expected timeout, check running flag
                        }
                        if running.load(Ordering::SeqCst) {
                            error!("Error reading {} event: {}", wm, e);
                        }
                        break; // Reconnect
                    }
                }
            }
        }

        debug!("{} event loop exiting", wm);
    }

    /// Handle a single event.
    /// Returns (workspace_changed, window_changed).
    fn handle_event(
        msg_type: u32,
        event: &Value,
        socket_path: &str,
        shared: &SharedState,
        wm: &str,
    ) -> (bool, bool) {
        let change = event.get("change").and_then(|v| v.as_str()).unwrap_or("");

        trace!("{} event: type=0x{:x}, change={}", wm, msg_type, change);

        match msg_type {
            IPC_EVENT_WORKSPACE => {
                Self::fetch_all(socket_path, shared, wm);
                (true, true)
            }
            IPC_EVENT_WINDOW => {
                match change {
                    "focus" | "title" => {
                        Self::fetch_tree(socket_path, shared, wm);
                        (false, true)
                    }
                    "close" | "new" | "move" => {
                        // Window count / occupancy may have changed
                        Self::fetch_all(socket_path, shared, wm);
                        (true, true)
                    }
                    "urgent" => {
                        Self::fetch_all(socket_path, shared, wm);
                        (true, false)
                    }
                    _ => {
                        trace!("Unhandled {} window change: {}", wm, change);
                        (false, false)
                    }
                }
            }
            _ => {
                trace!("Unhandled {} event type: 0x{:x}", wm, msg_type);
                (false, false)
            }
        }
    }
}

impl CompositorBackend for SwayBackend {
    fn start(&self, on_workspace_update: WorkspaceCallback, on_window_update: WindowCallback) {
        if self.running.swap(true, Ordering::SeqCst) {
            warn!("{} backend already running", self.compositor_name);
            return;
        }

        let wm = self.compositor_name;
        debug!("Starting {} backend", wm);

        let socket_path = match env::var(self.socket_env_var) {
            Ok(p) => p,
            Err(_) => {
                warn!("{} not set", self.socket_env_var);
                self.running.store(false, Ordering::SeqCst);
                return;
            }
        };
        *self.socket_path.write() = Some(socket_path.clone());

        *self.callbacks.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((on_workspace_update.clone(), on_window_update.clone()));

        let running = Arc::clone(&self.running);
        let shared = Arc::clone(&self.shared);
        let callbacks = Some((on_workspace_update, on_window_update));

        let handle = thread::Builder::new()
            .name(format!(
                "{}-event-loop",
                wm.to_lowercase().replace(' ', "-")
            ))
            .spawn(move || {
                Self::event_loop(running, shared, socket_path, callbacks, wm);
            })
            .ok();

        *self.event_thread.lock().unwrap_or_else(|e| e.into_inner()) = handle;

        debug!("{} backend started", wm);
    }

    fn stop(&self) {
        if !self.running.swap(false, Ordering::SeqCst) {
            return;
        }

        let wm = self.compositor_name;
        debug!("Stopping {} backend", wm);

        if let Some(handle) = self
            .event_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = handle.join();
        }

        debug!("{} backend stopped", wm);
    }

    fn list_workspaces(&self) -> Vec<WorkspaceMeta> {
        let workspaces = self.shared.workspaces.read();
        if workspaces.is_empty() {
            (1..=DEFAULT_WORKSPACE_COUNT)
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
        let socket_path = self.socket_path.read();
        if socket_path.is_none()
            && let Ok(path) = env::var(self.socket_env_var)
        {
            // Drop read lock before write. Benign race: worst case is double-init.
            drop(socket_path);
            *self.socket_path.write() = Some(path.clone());
            Self::fetch_all(&path, &self.shared, self.compositor_name);
        }
        self.shared.workspace_snapshot.read().clone()
    }

    fn get_focused_window(&self) -> Option<WindowInfo> {
        self.shared.focused_window.read().clone()
    }

    fn switch_workspace(&self, workspace_id: i32) {
        // workspace_id == -1 cannot occur here: fetch_workspaces maps named
        // workspaces (num == -1) to synthetic IDs in [-2B, -1B-1] via FNV hash.
        let socket_path = self.socket_path.read();
        if let Some(ref path) = *socket_path {
            let command = if workspace_id < -1 {
                // Synthetic ID for a named workspace — look up name and switch by name
                let workspaces = self.shared.workspaces.read();
                if let Some(ws) = workspaces.iter().find(|ws| ws.id == workspace_id) {
                    format!("workspace \"{}\"", ws.name)
                } else {
                    warn!("No workspace found for synthetic id {}", workspace_id);
                    return;
                }
            } else {
                format!("workspace number {}", workspace_id)
            };
            let _ = ipc_request(
                path,
                IPC_RUN_COMMAND,
                command.as_bytes(),
                self.compositor_name,
            );
        }
    }

    fn quit_compositor(&self) {
        debug!("Sending exit command to {}", self.compositor_name);
        let socket_path = self.socket_path.read();
        if let Some(ref path) = *socket_path {
            let _ = ipc_request(path, IPC_RUN_COMMAND, b"exit", self.compositor_name);
        }
    }

    fn name(&self) -> &'static str {
        self.compositor_name
    }
}

impl Drop for SwayBackend {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
    }
}
