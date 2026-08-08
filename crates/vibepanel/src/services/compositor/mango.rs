//! MangoWC compositor backend.
//!
//! Communicates with MangoWC over its line-delimited JSON IPC on the Unix
//! socket advertised by `MANGO_INSTANCE_SIGNATURE`.
//!
//! # Protocol
//!
//! Requests are single lines; responses are single JSON lines.
//! - `get <topic>` — one-shot query
//! - `watch <topic>` — streaming subscription (one line per update)
//! - `dispatch <command>[,<args>]` — action request
//!
//! Topics consumed here: `all-monitors` (tag/workspace, focused-window and
//! keyboard-layout state) and `all-clients` (window list).

use std::collections::{HashMap, HashSet};
use std::env;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use parking_lot::RwLock;
use serde_json::Value;
use tracing::{debug, error, trace, warn};

use super::{
    CompositorBackend, KeyboardLayoutCallback, KeyboardLayoutInfo, Window, WindowCallback,
    WindowInfo, WindowListCallback, WindowListSnapshot, WorkspaceCallback, WorkspaceMeta,
    WorkspaceSnapshot,
};

const MANGO_SOCKET_ENV: &str = "MANGO_INSTANCE_SIGNATURE";
const SOCKET_READ_TIMEOUT: Duration = Duration::from_secs(1);
const SOCKET_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const SOCKET_RECONNECT_MS: u64 = 1000;

/// Fallback tag count used until `all-monitors` reports the real one.
const DEFAULT_WORKSPACE_COUNT: u32 = 9;
/// Synthetic workspace id MangoWC uses to signal overview mode.
const OVERVIEW_WORKSPACE_ID: i32 = 0;
const OVERVIEW_WORKSPACE_NAME: &str = "overview";

#[derive(Debug)]
struct MangoSharedState {
    snapshot: RwLock<WorkspaceSnapshot>,
    output_geometry: RwLock<HashMap<String, (i64, i64)>>,
    focused_window: RwLock<Option<WindowInfo>>,
    focused_client_id: RwLock<Option<u64>>,
    windows: RwLock<Vec<Window>>,
    keyboard_layout: RwLock<Option<KeyboardLayoutInfo>>,
    tag_count: AtomicU32,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct MonitorChanges {
    workspace: bool,
    focused_window: bool,
    window_list: bool,
    keyboard_layout: bool,
}

impl Default for MangoSharedState {
    fn default() -> Self {
        Self {
            snapshot: RwLock::new(WorkspaceSnapshot::default()),
            output_geometry: RwLock::new(HashMap::new()),
            focused_window: RwLock::new(None),
            focused_client_id: RwLock::new(None),
            windows: RwLock::new(Vec::new()),
            keyboard_layout: RwLock::new(None),
            tag_count: AtomicU32::new(DEFAULT_WORKSPACE_COUNT),
        }
    }
}

/// MangoWC compositor backend driven by Mango's JSON IPC socket.
pub struct MangoBackend {
    /// Path to the Mango IPC socket, or `None` when `MANGO_INSTANCE_SIGNATURE`
    /// is unset (i.e. we are not running under MangoWC).
    socket_path: Option<String>,
    shared: Arc<MangoSharedState>,
    running: Arc<AtomicBool>,
    watch_threads: Mutex<Vec<JoinHandle<()>>>,
    keyboard_layout_callback: Mutex<Option<KeyboardLayoutCallback>>,
    window_list_callback: Mutex<Option<WindowListCallback>>,
}

impl MangoBackend {
    /// Create a MangoWC backend.
    pub fn new() -> Self {
        Self {
            socket_path: env::var(MANGO_SOCKET_ENV).ok().filter(|p| !p.is_empty()),
            shared: Arc::new(MangoSharedState::default()),
            running: Arc::new(AtomicBool::new(false)),
            watch_threads: Mutex::new(Vec::new()),
            keyboard_layout_callback: Mutex::new(None),
            window_list_callback: Mutex::new(None),
        }
    }

    /// Open a short-lived connection to the Mango IPC socket.
    fn connect(&self) -> Option<UnixStream> {
        let socket_path = self.socket_path.as_deref()?;
        match UnixStream::connect(socket_path) {
            Ok(stream) => {
                let _ = stream.set_read_timeout(Some(SOCKET_REQUEST_TIMEOUT));
                let _ = stream.set_write_timeout(Some(SOCKET_REQUEST_TIMEOUT));
                Some(stream)
            }
            Err(e) => {
                warn!(
                    "Failed to connect to Mango IPC socket {}: {}",
                    socket_path, e
                );
                None
            }
        }
    }

    fn send_command(&self, command: &str) -> Option<Value> {
        let mut stream = self.connect()?;

        if let Err(e) = writeln!(stream, "{}", command) {
            warn!("Failed to send Mango IPC command '{}': {}", command, e);
            return None;
        }

        let mut response = String::new();
        let mut reader = BufReader::new(stream);
        if let Err(e) = reader.read_line(&mut response) {
            warn!("Failed to read Mango IPC response for '{}': {}", command, e);
            return None;
        }
        parse_json_line(&response)
    }

    fn send_dispatch(&self, command: &str) {
        if let Some(value) = self.send_command(command)
            && let Some(error) = value.get("error").and_then(Value::as_str)
        {
            warn!("Mango IPC dispatch '{}' failed: {}", command, error);
        }
    }

    fn fetch_initial_state(&self) {
        if let Some(value) = self.send_command("get all-monitors") {
            apply_workspace_from_monitors(&self.shared, &value);
            apply_focused_window_from_monitors(&self.shared, &value);
            apply_keyboard_layout_from_monitors(&self.shared, &value);
        }
        if let Some(value) = self.send_command("get all-clients") {
            apply_window_list_from_clients(&self.shared, &value);
        }
    }

    fn spawn_workspace_watch(
        socket_path: String,
        shared: Arc<MangoSharedState>,
        running: Arc<AtomicBool>,
        workspace_callback: WorkspaceCallback,
        window_callback: WindowCallback,
        window_list_callback: Option<WindowListCallback>,
        keyboard_layout_callback: Option<KeyboardLayoutCallback>,
    ) -> JoinHandle<()> {
        thread::spawn(move || {
            watch_mango_command(socket_path, "watch all-monitors", running, move |value| {
                let changes = apply_monitor_update(&shared, &value);
                if changes.workspace {
                    let snapshot = shared.snapshot.read().clone();
                    workspace_callback(snapshot);
                }
                if changes.focused_window {
                    let info = shared.focused_window.read().clone();
                    if let Some(info) = info {
                        window_callback(info);
                    }
                }
                // Taskbar separator state depends on workspace metadata, matching
                // Niri's workspace-triggered window-list refresh behavior.
                if changes.window_list
                    && let Some(callback) = &window_list_callback
                {
                    let windows = shared.windows.read().clone();
                    callback(WindowListSnapshot { windows });
                }
                if changes.keyboard_layout {
                    let info = shared.keyboard_layout.read().clone();
                    if let (Some(callback), Some(info)) = (&keyboard_layout_callback, info) {
                        callback(info);
                    }
                }
            });
        })
    }

    fn spawn_window_list_watch(
        socket_path: String,
        shared: Arc<MangoSharedState>,
        running: Arc<AtomicBool>,
        callback: WindowListCallback,
    ) -> JoinHandle<()> {
        thread::spawn(move || {
            watch_mango_command(socket_path, "watch all-clients", running, move |value| {
                if apply_window_list_from_clients(&shared, &value) {
                    let windows = shared.windows.read().clone();
                    callback(WindowListSnapshot { windows });
                }
            });
        })
    }
}

impl CompositorBackend for MangoBackend {
    fn start(&self, on_workspace_update: WorkspaceCallback, on_window_update: WindowCallback) {
        let Some(socket_path) = self.socket_path.clone() else {
            error!(
                "{} is not set - MangoWC IPC is unavailable. Workspace and window \
                 tracking are disabled. If you are not running MangoWC, set \
                 `compositor` under [advanced] to a supported backend.",
                MANGO_SOCKET_ENV
            );
            return;
        };

        if self.running.swap(true, Ordering::SeqCst) {
            warn!("MangoBackend already running");
            return;
        }

        debug!("Starting Mango IPC backend on {}", socket_path);

        let window_list_callback = self
            .window_list_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let keyboard_layout_callback = self
            .keyboard_layout_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        self.fetch_initial_state();
        let snapshot = self.shared.snapshot.read().clone();
        on_workspace_update(snapshot);
        let focused_window = self.shared.focused_window.read().clone();
        if let Some(info) = focused_window {
            on_window_update(info);
        }
        if let Some(callback) = &window_list_callback {
            let windows = self.shared.windows.read().clone();
            callback(WindowListSnapshot { windows });
        }
        if let Some(callback) = &keyboard_layout_callback {
            let keyboard_layout = self.shared.keyboard_layout.read().clone();
            if let Some(info) = keyboard_layout {
                callback(info);
            }
        }

        let mut threads = self.watch_threads.lock().unwrap_or_else(|e| e.into_inner());
        threads.push(Self::spawn_workspace_watch(
            socket_path.clone(),
            self.shared.clone(),
            self.running.clone(),
            on_workspace_update,
            on_window_update,
            window_list_callback.clone(),
            keyboard_layout_callback,
        ));
        if let Some(callback) = window_list_callback {
            threads.push(Self::spawn_window_list_watch(
                socket_path.clone(),
                self.shared.clone(),
                self.running.clone(),
                callback,
            ));
        }
    }

    fn stop(&self) {
        if !self.running.swap(false, Ordering::SeqCst) {
            return;
        }
        for handle in self
            .watch_threads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            let _ = handle.join();
        }
        debug!("Mango socket IPC backend stopped");
    }

    fn list_workspaces(&self) -> Vec<WorkspaceMeta> {
        mango_workspace_meta(
            self.shared.tag_count.load(Ordering::Relaxed),
            &self.shared.snapshot.read(),
        )
    }

    fn get_workspace_snapshot(&self) -> WorkspaceSnapshot {
        self.shared.snapshot.read().clone()
    }

    fn get_focused_window(&self) -> Option<WindowInfo> {
        self.shared.focused_window.read().clone()
    }

    fn switch_workspace(&self, workspace_id: i32) {
        if workspace_id > 0 {
            self.send_dispatch(&format!("dispatch view,{}", workspace_id));
        }
    }

    fn quit_compositor(&self) {
        self.send_dispatch("dispatch quit");
    }

    fn name(&self) -> &'static str {
        "MangoWC"
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
        self.send_dispatch("dispatch switch_keyboard_layout");
    }

    fn list_windows(&self) -> Vec<Window> {
        self.shared.windows.read().clone()
    }

    fn set_window_list_callback(&self, callback: WindowListCallback) {
        *self
            .window_list_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(callback);
    }

    fn focus_window(&self, window_id: u64) {
        self.send_dispatch(&format!("dispatch focusid client,{}", window_id));
    }
}

impl Drop for MangoBackend {
    fn drop(&mut self) {
        // Signal the watch threads to exit without joining, matching the other
        // backends. `CompositorManager::drop` calls `stop()` first, which is
        // where the joins happen; this is only the drop-without-stop safety net.
        self.running.store(false, Ordering::SeqCst);
    }
}

fn parse_json_line(line: &str) -> Option<Value> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    match serde_json::from_str(line) {
        Ok(value) => Some(value),
        Err(e) => {
            trace!("Failed to parse Mango IPC JSON: {}", e);
            None
        }
    }
}

fn watch_mango_command<F>(
    socket_path: String,
    command: &'static str,
    running: Arc<AtomicBool>,
    mut handle_value: F,
) where
    F: FnMut(Value),
{
    while running.load(Ordering::SeqCst) {
        let mut stream = match UnixStream::connect(&socket_path) {
            Ok(stream) => stream,
            Err(e) => {
                warn!(
                    "Failed to connect Mango IPC watch '{}': {}. Retrying",
                    command, e
                );
                thread::sleep(Duration::from_millis(SOCKET_RECONNECT_MS));
                continue;
            }
        };

        let _ = stream.set_read_timeout(Some(SOCKET_READ_TIMEOUT));
        if let Err(e) = writeln!(stream, "{}", command) {
            warn!("Failed to start Mango IPC watch '{}': {}", command, e);
            thread::sleep(Duration::from_millis(SOCKET_RECONNECT_MS));
            continue;
        }

        let reader = BufReader::new(stream);
        for line in reader.lines() {
            if !running.load(Ordering::SeqCst) {
                return;
            }
            match line {
                Ok(line) => {
                    if let Some(value) = parse_json_line(&line) {
                        handle_value(value);
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => {
                    warn!("Mango IPC watch '{}' ended: {}", command, e);
                    break;
                }
            }
        }
        if running.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(SOCKET_RECONNECT_MS));
        }
    }
}

fn apply_workspace_from_monitors(shared: &Arc<MangoSharedState>, value: &Value) -> bool {
    let Some(entries) = value.get("monitors").and_then(Value::as_array) else {
        return false;
    };

    let mut snapshot = WorkspaceSnapshot::default();
    let mut output_geometry = HashMap::new();
    let mut max_tag = 0u32;
    for entry in entries {
        let Some(output_name) = entry.get("name").and_then(Value::as_str) else {
            continue;
        };
        if let (Some(x), Some(y)) = (
            entry.get("x").and_then(Value::as_i64),
            entry.get("y").and_then(Value::as_i64),
        ) {
            output_geometry.insert(output_name.to_string(), (x, y));
        }
        let is_focused_monitor = entry
            .get("active")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let per_output = snapshot
            .per_output
            .entry(output_name.to_string())
            .or_default();
        per_output.urgent_workspaces = Some(HashSet::new());
        let Some(tags) = entry.get("tags").and_then(Value::as_array) else {
            continue;
        };
        let active_tags: HashSet<i32> = entry
            .get("active_tags")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|tag| tag.as_i64().map(|id| id as i32))
            .collect();
        let is_overview = active_tags.contains(&OVERVIEW_WORKSPACE_ID);
        for tag in tags {
            let Some(workspace_id) = tag.get("index").and_then(Value::as_i64).map(|id| id as i32)
            else {
                continue;
            };
            max_tag = max_tag.max(workspace_id.max(0) as u32);

            let is_active = if active_tags.is_empty() {
                tag.get("is_active")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            } else {
                active_tags.contains(&workspace_id)
            };
            let is_urgent = tag
                .get("is_urgent")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let client_count = tag.get("client_count").and_then(Value::as_u64).unwrap_or(0) as u32;

            if !is_overview {
                per_output.window_counts.insert(workspace_id, client_count);
                if client_count > 0 {
                    per_output.occupied_workspaces.insert(workspace_id);
                    *snapshot.window_counts.entry(workspace_id).or_insert(0) += client_count;
                    snapshot.occupied_workspaces.insert(workspace_id);
                }
                if is_active {
                    per_output.active_workspace.insert(workspace_id);
                    if is_focused_monitor {
                        snapshot.active_workspace.insert(workspace_id);
                    }
                }
            }
            if is_urgent {
                snapshot.urgent_workspaces.insert(workspace_id);
                if let Some(urgent_workspaces) = per_output.urgent_workspaces.as_mut() {
                    urgent_workspaces.insert(workspace_id);
                }
            }
        }

        if is_overview {
            per_output.active_workspace.insert(OVERVIEW_WORKSPACE_ID);
            if is_focused_monitor {
                snapshot.active_workspace.insert(OVERVIEW_WORKSPACE_ID);
            }
        }
    }

    let tag_count_changed =
        max_tag > 0 && shared.tag_count.swap(max_tag, Ordering::Relaxed) != max_tag;
    if !output_geometry.is_empty() {
        *shared.output_geometry.write() = output_geometry;
    }
    let snapshot_changed = {
        let mut current = shared.snapshot.write();
        if *current == snapshot {
            false
        } else {
            *current = snapshot;
            true
        }
    };
    tag_count_changed || snapshot_changed
}

fn apply_monitor_update(shared: &Arc<MangoSharedState>, value: &Value) -> MonitorChanges {
    let workspace = apply_workspace_from_monitors(shared, value);
    let focused_window = apply_focused_window_from_monitors(shared, value);
    let mut window_list = workspace;
    if focused_window {
        let focused_id = *shared.focused_client_id.read();
        window_list |= apply_window_list_focus(shared, focused_id);
    }

    MonitorChanges {
        workspace,
        focused_window,
        window_list,
        keyboard_layout: apply_keyboard_layout_from_monitors(shared, value),
    }
}

fn active_monitor(value: &Value) -> Option<&Value> {
    value
        .get("monitors")?
        .as_array()?
        .iter()
        .find(|monitor| monitor.get("active").and_then(Value::as_bool) == Some(true))
}

fn apply_focused_window_from_monitors(shared: &Arc<MangoSharedState>, value: &Value) -> bool {
    let Some(monitor) = active_monitor(value) else {
        return false;
    };
    let Some(output) = monitor.get("name").and_then(Value::as_str) else {
        return false;
    };
    let Some(client) = monitor.get("active_client") else {
        return false;
    };

    let focused_client_id = client.get("id").and_then(Value::as_u64);
    let info = WindowInfo {
        title: client
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        app_id: client
            .get("appid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        output: Some(output.to_string()),
    };

    let changed = *shared.focused_client_id.read() != focused_client_id
        || shared.focused_window.read().as_ref() != Some(&info);
    *shared.focused_client_id.write() = focused_client_id;
    *shared.focused_window.write() = Some(info);
    changed
}

/// True if a client is a scratchpad (regular or named), regardless of visibility.
fn is_scratchpad_client(client: &Value) -> bool {
    client
        .get("is_scratchpad")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || client
            .get("is_namedscratchpad")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

/// True if a client is a dismissed (hidden) scratchpad. Mango clears a
/// scratchpad's tags when dismissing it, so a scratchpad with no tags is hidden.
fn is_dismissed_scratchpad(client: &Value) -> bool {
    if !is_scratchpad_client(client) {
        return false;
    }

    let has_tags = client
        .get("tags")
        .and_then(Value::as_array)
        .is_some_and(|tags| !tags.is_empty());

    !has_tags
}

fn apply_window_list_from_clients(shared: &Arc<MangoSharedState>, value: &Value) -> bool {
    let Some(clients) = value.get("clients").and_then(Value::as_array) else {
        return false;
    };

    // Prefer all-monitors focus state once seen. all-clients can keep a previously
    // focused client marked active after switching to an empty workspace.
    let focused_client_id = *shared.focused_client_id.read();
    let focused_client_known = shared.focused_window.read().is_some();
    let output_geometry = shared.output_geometry.read().clone();
    let mut windows: Vec<_> = clients
        .iter()
        .filter_map(|client| {
            if client
                .get("is_swallowedby")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return None;
            }
            let window = client_value_to_window(client, focused_client_id, focused_client_known)?;
            // Always drop dismissed (hidden) scratchpads. Visible scratchpads are
            // flagged via Window.is_scratchpad and filtered by taskbar
            // show_scratchpad_windows.
            if is_dismissed_scratchpad(client) {
                return None;
            }
            Some(window)
        })
        .enumerate()
        .collect();

    windows.sort_by(|(a_idx, a), (b_idx, b)| {
        window_output_sort_key(a, &output_geometry)
            .cmp(&window_output_sort_key(b, &output_geometry))
            .then(
                a.workspace_id
                    .unwrap_or(i32::MAX)
                    .cmp(&b.workspace_id.unwrap_or(i32::MAX)),
            )
            .then(a_idx.cmp(b_idx))
    });

    let windows = windows
        .into_iter()
        .map(|(_, window)| window)
        .collect::<Vec<_>>();
    let mut current = shared.windows.write();
    if *current == windows {
        return false;
    }
    *current = windows;
    true
}

fn window_output_sort_key<'a>(
    window: &'a Window,
    output_geometry: &HashMap<String, (i64, i64)>,
) -> (i64, i64, &'a str) {
    let output = window.output.as_deref().unwrap_or_default();
    let (x, y) = output_geometry
        .get(output)
        .copied()
        .unwrap_or((i64::MAX, i64::MAX));

    (x, y, output)
}

fn apply_window_list_focus(shared: &Arc<MangoSharedState>, focused_id: Option<u64>) -> bool {
    let mut windows = shared.windows.write();
    let mut changed = false;
    for window in windows.iter_mut() {
        let is_focused = focused_id == Some(window.id);
        changed |= window.is_focused != is_focused;
        window.is_focused = is_focused;
    }
    changed
}

fn client_value_to_window(
    value: &Value,
    focused_client_id: Option<u64>,
    focused_client_known: bool,
) -> Option<Window> {
    let id = value.get("id")?.as_u64()?;

    Some(Window {
        id,
        title: value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        app_id: value
            .get("appid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        workspace_id: value
            .get("tags")
            .and_then(Value::as_array)
            .and_then(|tags| tags.first())
            .and_then(Value::as_i64)
            .map(|id| id as i32),
        output: value
            .get("monitor")
            .and_then(Value::as_str)
            .map(str::to_string),
        is_focused: if focused_client_known {
            focused_client_id == Some(id)
        } else {
            value
                .get("is_focused")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        },
        is_urgent: value
            .get("is_urgent")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        is_scratchpad: is_scratchpad_client(value),
    })
}

fn apply_keyboard_layout_from_monitors(shared: &Arc<MangoSharedState>, value: &Value) -> bool {
    let Some(layout_name) = active_monitor(value)
        .and_then(|monitor| monitor.get("keyboardlayout"))
        .and_then(Value::as_str)
    else {
        return false;
    };
    let info = KeyboardLayoutInfo {
        layout_name: layout_name.to_string(),
        layout_count: None,
    };
    let mut current = shared.keyboard_layout.write();
    if current.as_ref() == Some(&info) {
        false
    } else {
        *current = Some(info);
        true
    }
}

fn mango_workspace_meta(count: u32, snapshot: &WorkspaceSnapshot) -> Vec<WorkspaceMeta> {
    let mut workspaces: Vec<_> = (1..=count as i32)
        .map(|id| WorkspaceMeta {
            id,
            idx: id,
            name: id.to_string(),
            output: None,
        })
        .collect();

    if snapshot
        .per_output
        .values()
        .any(|state| state.active_workspace.contains(&OVERVIEW_WORKSPACE_ID))
        || snapshot.active_workspace.contains(&OVERVIEW_WORKSPACE_ID)
    {
        workspaces.push(WorkspaceMeta {
            id: OVERVIEW_WORKSPACE_ID,
            idx: OVERVIEW_WORKSPACE_ID,
            name: OVERVIEW_WORKSPACE_NAME.to_string(),
            output: None,
        });
    }

    workspaces
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::compositor::PerOutputState;

    /// Build a backend with no socket, simulating a non-MangoWC session.
    fn backend_without_socket() -> MangoBackend {
        let mut backend = MangoBackend::new();
        backend.socket_path = None;
        backend
    }

    #[test]
    fn start_without_socket_env_is_a_no_op() {
        let backend = backend_without_socket();

        backend.start(Arc::new(|_| {}), Arc::new(|_| {}));

        assert!(
            !backend.running.load(Ordering::SeqCst),
            "backend must not report running without MANGO_INSTANCE_SIGNATURE"
        );
        assert!(
            backend
                .watch_threads
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "no watch threads should be spawned without a socket"
        );
    }

    #[test]
    fn commands_without_socket_do_not_panic() {
        let backend = backend_without_socket();

        assert!(backend.send_command("get all-monitors").is_none());
        backend.switch_workspace(2);
        backend.quit_compositor();
        backend.switch_keyboard_layout_next();
        backend.focus_window(7);

        assert!(backend.list_windows().is_empty());
        assert!(backend.get_focused_window().is_none());
    }

    #[test]
    fn list_workspaces_adds_single_overview_when_any_output_active() {
        let mut snapshot = WorkspaceSnapshot::default();
        let mut overview_state = PerOutputState::default();
        overview_state
            .active_workspace
            .insert(OVERVIEW_WORKSPACE_ID);
        snapshot
            .per_output
            .insert("eDP-1".to_string(), overview_state);
        let mut other_overview_state = PerOutputState::default();
        other_overview_state
            .active_workspace
            .insert(OVERVIEW_WORKSPACE_ID);
        snapshot
            .per_output
            .insert("DP-1".to_string(), other_overview_state);

        let workspaces = mango_workspace_meta(2, &snapshot);

        assert_eq!(workspaces.len(), 3);
        assert_eq!(workspaces[0].id, 1);
        assert_eq!(workspaces[1].id, 2);
        assert_eq!(workspaces[2].id, OVERVIEW_WORKSPACE_ID);
        assert_eq!(workspaces[2].idx, OVERVIEW_WORKSPACE_ID);
        assert_eq!(workspaces[2].name, OVERVIEW_WORKSPACE_NAME);
        assert_eq!(workspaces[2].output, None);
    }

    #[test]
    fn socket_workspace_parser_accepts_all_monitors_name_field() {
        let shared = Arc::new(MangoSharedState::default());
        let value = serde_json::json!({
            "monitors": [
                {
                    "name": "eDP-1",
                    "active": true,
                    "active_tags": [2],
                    "tags": [
                        {"index": 1, "is_active": false, "is_urgent": false, "client_count": 0},
                        {"index": 2, "is_active": true, "is_urgent": false, "client_count": 3}
                    ]
                }
            ]
        });

        assert!(apply_workspace_from_monitors(&shared, &value));
        let snapshot = shared.snapshot.read();
        let output = snapshot.per_output.get("eDP-1").unwrap();

        assert!(output.active_workspace.contains(&2));
        assert_eq!(output.window_counts.get(&2), Some(&3));
        assert!(snapshot.active_workspace.contains(&2));
        assert!(snapshot.occupied_workspaces.contains(&2));
        drop(snapshot);
        assert!(!apply_workspace_from_monitors(&shared, &value));
    }

    #[test]
    fn monitor_update_refreshes_window_list_for_workspace_only_change() {
        let shared = Arc::new(MangoSharedState::default());
        let initial = serde_json::json!({
            "monitors": [{
                "name": "eDP-1",
                "active": true,
                "active_tags": [1],
                "active_client": {"id": 7, "title": "Terminal", "appid": "foot"},
                "tags": [
                    {"index": 1, "is_urgent": false, "client_count": 1},
                    {"index": 2, "is_urgent": false, "client_count": 0}
                ]
            }]
        });
        let workspace_only = serde_json::json!({
            "monitors": [{
                "name": "eDP-1",
                "active": true,
                "active_tags": [1, 2],
                "active_client": {"id": 7, "title": "Terminal", "appid": "foot"},
                "tags": [
                    {"index": 1, "is_urgent": false, "client_count": 1},
                    {"index": 2, "is_urgent": false, "client_count": 0}
                ]
            }]
        });

        apply_monitor_update(&shared, &initial);
        let changes = apply_monitor_update(&shared, &workspace_only);

        assert_eq!(
            changes,
            MonitorChanges {
                workspace: true,
                focused_window: false,
                window_list: true,
                keyboard_layout: false,
            }
        );
    }

    #[test]
    fn socket_workspace_parser_global_active_uses_active_monitor_only() {
        let shared = Arc::new(MangoSharedState::default());
        let value = serde_json::json!({
            "monitors": [
                {
                    "name": "eDP-1",
                    "active": true,
                    "active_tags": [2],
                    "tags": [
                        {"index": 1, "is_active": false, "is_urgent": false, "client_count": 0},
                        {"index": 2, "is_active": true, "is_urgent": false, "client_count": 1}
                    ]
                },
                {
                    "name": "DP-1",
                    "active": false,
                    "active_tags": [5],
                    "tags": [
                        {"index": 5, "is_active": true, "is_urgent": false, "client_count": 1}
                    ]
                }
            ]
        });

        assert!(apply_workspace_from_monitors(&shared, &value));
        let snapshot = shared.snapshot.read();

        assert!(snapshot.per_output["eDP-1"].active_workspace.contains(&2));
        assert!(snapshot.per_output["DP-1"].active_workspace.contains(&5));
        assert_eq!(snapshot.active_workspace, HashSet::from([2]));
    }

    #[test]
    fn socket_workspace_parser_tracks_urgency_per_output() {
        let shared = Arc::new(MangoSharedState::default());
        let value = serde_json::json!({
            "monitors": [
                {
                    "name": "eDP-1",
                    "active": true,
                    "active_tags": [2],
                    "tags": [
                        {"index": 1, "is_urgent": false, "client_count": 0},
                        {"index": 2, "is_urgent": false, "client_count": 1}
                    ]
                },
                {
                    "name": "HDMI-A-1",
                    "active": false,
                    "active_tags": [1],
                    "tags": [
                        {"index": 1, "is_urgent": true, "client_count": 1},
                        {"index": 2, "is_urgent": false, "client_count": 0}
                    ]
                }
            ]
        });

        assert!(apply_workspace_from_monitors(&shared, &value));
        let snapshot = shared.snapshot.read();

        assert!(snapshot.urgent_workspaces.contains(&1));
        assert_eq!(
            snapshot.per_output["eDP-1"].urgent_workspaces,
            Some(HashSet::new())
        );
        assert_eq!(
            snapshot.per_output["HDMI-A-1"].urgent_workspaces,
            Some(HashSet::from([1]))
        );
    }

    #[test]
    fn socket_workspace_parser_suppresses_real_tags_in_overview() {
        let shared = Arc::new(MangoSharedState::default());
        let value = serde_json::json!({
            "monitors": [
                {
                    "name": "eDP-1",
                    "active": true,
                    "active_tags": [OVERVIEW_WORKSPACE_ID],
                    "tags": [
                        {"index": 1, "is_active": false, "is_urgent": false, "client_count": 2},
                        {"index": 2, "is_active": false, "is_urgent": false, "client_count": 1}
                    ]
                }
            ]
        });

        assert!(apply_workspace_from_monitors(&shared, &value));
        let snapshot = shared.snapshot.read();
        let output = snapshot.per_output.get("eDP-1").unwrap();

        assert_eq!(output.active_workspace.len(), 1);
        assert!(output.active_workspace.contains(&OVERVIEW_WORKSPACE_ID));
        assert!(output.window_counts.is_empty());
        assert!(output.occupied_workspaces.is_empty());
        assert_eq!(snapshot.active_workspace.len(), 1);
        assert!(snapshot.active_workspace.contains(&OVERVIEW_WORKSPACE_ID));
        assert!(snapshot.window_counts.is_empty());
        assert!(snapshot.occupied_workspaces.is_empty());
    }

    #[test]
    fn socket_focused_window_parser_scopes_no_client_to_active_monitor() {
        let shared = Arc::new(MangoSharedState::default());
        let value = serde_json::json!({
            "monitors": [
                {
                    "name": "DP-1",
                    "active": false,
                    "active_tags": [1],
                    "active_client": {"id": 10, "title": "Terminal", "appid": "foot"}
                },
                {
                    "name": "HDMI-A-1",
                    "active": true,
                    "active_tags": [3],
                    "active_client": {"id": null, "title": null, "appid": null}
                }
            ]
        });

        assert!(apply_focused_window_from_monitors(&shared, &value));
        assert!(!apply_focused_window_from_monitors(&shared, &value));

        assert_eq!(
            shared.focused_window.read().clone(),
            Some(WindowInfo {
                output: Some("HDMI-A-1".to_string()),
                ..Default::default()
            })
        );
        assert_eq!(*shared.focused_client_id.read(), None);
    }

    #[test]
    fn socket_window_list_parser_maps_mango_clients() {
        let shared = Arc::new(MangoSharedState::default());
        let value = serde_json::json!({
            "clients": [
                {
                    "id": 7,
                    "title": "Terminal",
                    "appid": "foot",
                    "monitor": "eDP-1",
                    "tags": [2, 3],
                    "is_focused": true,
                    "is_urgent": false
                },
                {
                    "id": 8,
                    "title": "Browser",
                    "appid": "firefox",
                    "monitor": "DP-1",
                    "tags": [5],
                    "is_focused": false,
                    "is_urgent": true
                }
            ]
        });

        assert!(apply_window_list_from_clients(&shared, &value));
        assert!(!apply_window_list_from_clients(&shared, &value));
        let windows = shared.windows.read();

        assert_eq!(windows.len(), 2);
        let terminal = windows.iter().find(|window| window.id == 7).unwrap();
        assert_eq!(terminal.title, "Terminal");
        assert_eq!(terminal.app_id, "foot");
        assert_eq!(terminal.workspace_id, Some(2));
        assert_eq!(terminal.output.as_deref(), Some("eDP-1"));
        assert!(terminal.is_focused);
        assert!(!terminal.is_urgent);

        let browser = windows.iter().find(|window| window.id == 8).unwrap();
        assert_eq!(browser.workspace_id, Some(5));
        assert_eq!(browser.output.as_deref(), Some("DP-1"));
        assert!(!browser.is_focused);
        assert!(browser.is_urgent);
    }

    #[test]
    fn socket_window_list_parser_orders_by_workspace() {
        let shared = Arc::new(MangoSharedState::default());
        let value = serde_json::json!({
            "clients": [
                {"id": 30, "monitor": "eDP-1", "tags": [3]},
                {"id": 10, "monitor": "eDP-1", "tags": [1]},
                {"id": 20, "monitor": "eDP-1", "tags": [2]}
            ]
        });

        assert!(apply_window_list_from_clients(&shared, &value));
        let ids: Vec<_> = shared
            .windows
            .read()
            .iter()
            .map(|window| window.id)
            .collect();

        assert_eq!(ids, vec![10, 20, 30]);
    }

    #[test]
    fn socket_window_list_parser_orders_outputs_by_geometry() {
        let shared = Arc::new(MangoSharedState::default());
        let monitors = serde_json::json!({
            "monitors": [
                {"name": "DP-1", "x": 1920, "y": 0, "tags": []},
                {"name": "HDMI-A-1", "x": 0, "y": 0, "tags": []}
            ]
        });
        let clients = serde_json::json!({
            "clients": [
                {"id": 20, "monitor": "DP-1", "tags": [1]},
                {"id": 10, "monitor": "HDMI-A-1", "tags": [1]}
            ]
        });

        assert!(apply_workspace_from_monitors(&shared, &monitors));
        assert!(apply_window_list_from_clients(&shared, &clients));
        let ids: Vec<_> = shared
            .windows
            .read()
            .iter()
            .map(|window| window.id)
            .collect();

        assert_eq!(ids, vec![10, 20]);
    }

    #[test]
    fn socket_window_list_focus_update_clears_empty_workspace_focus() {
        let shared = Arc::new(MangoSharedState::default());
        let clients = serde_json::json!({
            "clients": [
                {"id": 10, "tags": [1], "is_focused": true},
                {"id": 20, "tags": [2], "is_focused": false}
            ]
        });

        assert!(apply_window_list_from_clients(&shared, &clients));
        assert!(shared.windows.read()[0].is_focused);

        assert!(apply_window_list_focus(&shared, None));

        assert!(
            shared
                .windows
                .read()
                .iter()
                .all(|window| !window.is_focused)
        );
    }

    #[test]
    fn socket_window_list_focus_update_moves_active_window() {
        let shared = Arc::new(MangoSharedState::default());
        let clients = serde_json::json!({
            "clients": [
                {"id": 10, "tags": [1], "is_focused": true},
                {"id": 20, "tags": [2], "is_focused": false}
            ]
        });

        assert!(apply_window_list_from_clients(&shared, &clients));
        assert!(apply_window_list_focus(&shared, Some(20)));
        let windows = shared.windows.read();

        assert!(
            !windows
                .iter()
                .find(|window| window.id == 10)
                .unwrap()
                .is_focused
        );
        assert!(
            windows
                .iter()
                .find(|window| window.id == 20)
                .unwrap()
                .is_focused
        );
    }

    #[test]
    fn socket_window_list_parser_ignores_stale_all_clients_focus() {
        let shared = Arc::new(MangoSharedState::default());

        assert!(apply_focused_window_from_monitors(
            &shared,
            &serde_json::json!({
                "monitors": [{
                    "name": "DP-1",
                    "active": true,
                    "active_tags": [2],
                    "active_client": {"id": 20, "title": "Browser", "appid": "firefox"}
                }]
            })
        ));
        assert!(apply_window_list_from_clients(
            &shared,
            &serde_json::json!({
                "clients": [
                    {"id": 10, "tags": [1], "is_focused": true},
                    {"id": 20, "tags": [2], "is_focused": false}
                ]
            })
        ));
        let windows = shared.windows.read();

        assert!(
            !windows
                .iter()
                .find(|window| window.id == 10)
                .unwrap()
                .is_focused
        );
        assert!(
            windows
                .iter()
                .find(|window| window.id == 20)
                .unwrap()
                .is_focused
        );
    }

    #[test]
    fn socket_window_list_parser_handles_optional_fields() {
        let shared = Arc::new(MangoSharedState::default());
        let value = serde_json::json!({
            "clients": [
                {"id": 9, "tags": []},
                {"title": "missing id"}
            ]
        });

        assert!(apply_window_list_from_clients(&shared, &value));
        let windows = shared.windows.read();

        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].id, 9);
        assert!(windows[0].title.is_empty());
        assert!(windows[0].app_id.is_empty());
        assert_eq!(windows[0].workspace_id, None);
        assert_eq!(windows[0].output, None);
        assert!(!windows[0].is_focused);
        assert!(!windows[0].is_urgent);
    }

    #[test]
    fn socket_keyboard_layout_parser_uses_active_monitor() {
        let shared = Arc::new(MangoSharedState::default());
        let value = serde_json::json!({
            "monitors": [
                {"active": false, "keyboardlayout": "German"},
                {"active": true, "keyboardlayout": "English (US)"}
            ]
        });

        assert!(apply_keyboard_layout_from_monitors(&shared, &value));
        let info = shared.keyboard_layout.read().clone().unwrap();

        assert_eq!(info.layout_name, "English (US)");
        assert_eq!(info.layout_count, None);
        assert!(!apply_keyboard_layout_from_monitors(&shared, &value));
    }

    // --- scratchpad filtering ---
    // Dismissed scratchpads have empty tags; summoned ones carry the visible
    // tagset. Visibility is tags-based, not focus-based.

    #[test]
    fn socket_window_list_hides_dismissed_scratchpads() {
        let shared = Arc::new(MangoSharedState::default());
        // Dismissed scratchpads and swallowed clients are not actionable taskbar entries.
        let value = serde_json::json!({
            "clients": [
                {
                    "id": 1, "tags": [],
                    "is_scratchpad": true, "is_namedscratchpad": false
                },
                {
                    "id": 2, "tags": [],
                    "is_scratchpad": false, "is_namedscratchpad": true
                },
                {
                    "id": 3, "tags": [1],
                    "is_scratchpad": false, "is_namedscratchpad": false
                },
                {
                    "id": 4, "tags": [1],
                    "is_swallowedby": true
                }
            ]
        });

        assert!(apply_window_list_from_clients(&shared, &value));
        let windows = shared.windows.read();

        assert_eq!(windows.len(), 1, "only the normal client should remain");
        assert_eq!(windows[0].id, 3);
    }

    #[test]
    fn socket_window_list_keeps_summoned_scratchpad() {
        // A summoned (tagged) scratchpad stays in the taskbar even when unfocused.
        let shared = Arc::new(MangoSharedState::default());
        let value = serde_json::json!({
            "clients": [
                {
                    "id": 5, "tags": [1],
                    "is_focused": false,
                    "is_scratchpad": false, "is_namedscratchpad": true
                },
                {
                    "id": 6, "tags": [2],
                    "is_focused": true,
                    "is_scratchpad": false, "is_namedscratchpad": false
                }
            ]
        });

        assert!(apply_window_list_from_clients(&shared, &value));
        let windows = shared.windows.read();

        assert_eq!(windows.len(), 2, "visible (tagged) scratchpad is kept");
        let scratch = windows.iter().find(|w| w.id == 5).unwrap();
        assert!(scratch.is_scratchpad, "visible scratchpad is flagged");
        let normal = windows.iter().find(|w| w.id == 6).unwrap();
        assert!(!normal.is_scratchpad);
    }

    #[test]
    fn socket_window_list_keeps_summoned_scratchpad_when_unfocused() {
        // Regression: a summoned scratchpad that lost focus must still show.
        let shared = Arc::new(MangoSharedState::default());
        let value = serde_json::json!({
            "clients": [
                {
                    "id": 10, "tags": [1],
                    "is_focused": false,
                    "is_scratchpad": true, "is_namedscratchpad": false
                }
            ]
        });

        assert!(apply_window_list_from_clients(&shared, &value));
        let windows = shared.windows.read();

        assert_eq!(windows.len(), 1, "unfocused but visible scratchpad is kept");
        assert_eq!(windows[0].id, 10);
    }

    #[test]
    fn socket_window_list_hides_dismissed_scratchpad_missing_tags_field() {
        // A missing tags field is treated the same as empty (hidden).
        let shared = Arc::new(MangoSharedState::default());
        let value = serde_json::json!({
            "clients": [
                {"id": 20, "is_scratchpad": false, "is_namedscratchpad": true}
            ]
        });

        assert!(!apply_window_list_from_clients(&shared, &value));
        let windows = shared.windows.read();

        assert!(
            windows.is_empty(),
            "scratchpad without tags should be hidden"
        );
    }
}
