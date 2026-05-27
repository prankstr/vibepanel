//! MangoWC / DWL compositor backend.
//!
//! This backend prefers Mango's socket IPC (`MANGO_INSTANCE_SIGNATURE`) and
//! falls back to the legacy `zdwl_ipc_manager_v2` Wayland protocol for DWL and
//! older Mango builds.
//!
//! # Protocol
//!
//! The Mango socket/DWL IPC paths provide:
//! - Tag/workspace state: active, urgent, client count, focus state
//! - Window info: title, app_id
//! - Workspace switching
//!
//! DWL protocol events are double-buffered: state is collected and applied on
//! `frame` events.

// TODO(mango): Remove the legacy DWL IPC fallback once Mango drops it upstream.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::env;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use gtk4::glib;
use parking_lot::RwLock;
use serde_json::Value;
use tracing::{debug, error, trace, warn};
use wayland_backend::client::ObjectId;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};

use super::dwl_ipc::{
    TagState, ZdwlIpcManagerV2, ZdwlIpcOutputV2, zdwl_ipc_manager_v2, zdwl_ipc_output_v2,
};
use super::{
    CompositorBackend, KeyboardLayoutCallback, KeyboardLayoutInfo, Window, WindowCallback,
    WindowInfo, WindowListCallback, WindowListSnapshot, WorkspaceCallback, WorkspaceMeta,
    WorkspaceSnapshot, xkb_names,
};

const MANGO_SOCKET_ENV: &str = "MANGO_INSTANCE_SIGNATURE";
const SOCKET_READ_TIMEOUT: Duration = Duration::from_secs(1);
const SOCKET_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const SOCKET_RECONNECT_MS: u64 = 1000;

/// Default number of workspaces/tags for DWL.
const DEFAULT_WORKSPACE_COUNT: u32 = 9;
const OVERVIEW_WORKSPACE_ID: i32 = 0;
const OVERVIEW_WORKSPACE_NAME: &str = "overview";
// Mango's DWL IPC path exposes only the layout symbol. This is Mango's
// default overview symbol; custom compositor configs may need updating here.
const OVERVIEW_LAYOUT_SYMBOL: &str = "󰃇";

#[derive(Debug)]
struct MangoSocketSharedState {
    snapshot: RwLock<WorkspaceSnapshot>,
    output_geometry: RwLock<HashMap<String, (i64, i64)>>,
    focused_window: RwLock<Option<WindowInfo>>,
    focused_client_id: RwLock<Option<u64>>,
    windows: RwLock<Vec<Window>>,
    keyboard_layout: RwLock<Option<KeyboardLayoutInfo>>,
    tag_count: AtomicU32,
}

impl Default for MangoSocketSharedState {
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

struct MangoSocketBackend {
    socket_path: String,
    shared: Arc<MangoSocketSharedState>,
    running: Arc<AtomicBool>,
    watch_threads: Mutex<Vec<JoinHandle<()>>>,
    keyboard_layout_callback: Mutex<Option<KeyboardLayoutCallback>>,
    window_list_callback: Mutex<Option<WindowListCallback>>,
}

impl MangoSocketBackend {
    fn from_env() -> Option<Self> {
        let socket_path = env::var(MANGO_SOCKET_ENV).ok()?;
        if socket_path.is_empty() {
            return None;
        }
        Some(Self::new(socket_path))
    }

    fn new(socket_path: String) -> Self {
        Self {
            socket_path,
            shared: Arc::new(MangoSocketSharedState::default()),
            running: Arc::new(AtomicBool::new(false)),
            watch_threads: Mutex::new(Vec::new()),
            keyboard_layout_callback: Mutex::new(None),
            window_list_callback: Mutex::new(None),
        }
    }

    fn send_command(&self, command: &str) -> Option<Value> {
        let mut stream = match UnixStream::connect(&self.socket_path) {
            Ok(stream) => stream,
            Err(e) => {
                warn!("Failed to connect to Mango socket IPC: {}", e);
                return None;
            }
        };
        let _ = stream.set_read_timeout(Some(SOCKET_REQUEST_TIMEOUT));
        let _ = stream.set_write_timeout(Some(SOCKET_REQUEST_TIMEOUT));

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
        let mut stream = match UnixStream::connect(&self.socket_path) {
            Ok(stream) => stream,
            Err(e) => {
                warn!("Failed to connect to Mango socket IPC: {}", e);
                return;
            }
        };
        let _ = stream.set_read_timeout(Some(SOCKET_REQUEST_TIMEOUT));
        let _ = stream.set_write_timeout(Some(SOCKET_REQUEST_TIMEOUT));

        if let Err(e) = writeln!(stream, "{}", command) {
            warn!("Failed to send Mango IPC dispatch '{}': {}", command, e);
            return;
        }

        let mut response = String::new();
        let mut reader = BufReader::new(stream);
        if let Err(e) = reader.read_line(&mut response) {
            warn!(
                "Failed to read Mango IPC dispatch response '{}': {}",
                command, e
            );
        } else if response.contains("\"error\"") {
            warn!(
                "Mango IPC dispatch '{}' returned {}",
                command,
                response.trim()
            );
        }
    }

    fn fetch_initial_state(&self) {
        if let Some(value) = self.send_command("get all-monitors") {
            apply_workspace_from_monitors(&self.shared, &value);
        }
        if let Some(value) = self.send_command("get focusing-client") {
            apply_focused_window_from_client(&self.shared, &value);
        }
        if let Some(value) = self.send_command("get all-clients") {
            apply_window_list_from_clients(&self.shared, &value);
        }
        if let Some(value) = self.send_command("get keyboardlayout") {
            apply_keyboard_layout_from_value(&self.shared, &value);
        }
    }

    fn spawn_workspace_watch(
        socket_path: String,
        shared: Arc<MangoSocketSharedState>,
        running: Arc<AtomicBool>,
        callback: WorkspaceCallback,
        window_list_callback: Option<WindowListCallback>,
    ) -> JoinHandle<()> {
        thread::spawn(move || {
            watch_mango_command(socket_path, "watch all-monitors", running, move |value| {
                if apply_workspace_from_monitors(&shared, &value) {
                    let snapshot = shared.snapshot.read().clone();
                    callback(snapshot);
                    if let Some(callback) = &window_list_callback {
                        let windows = shared.windows.read().clone();
                        callback(WindowListSnapshot { windows });
                    }
                }
            });
        })
    }

    fn spawn_focused_window_watch(
        socket_path: String,
        shared: Arc<MangoSocketSharedState>,
        running: Arc<AtomicBool>,
        callback: WindowCallback,
        window_list_callback: Option<WindowListCallback>,
    ) -> JoinHandle<()> {
        thread::spawn(move || {
            watch_mango_command(
                socket_path,
                "watch focusing-client",
                running,
                move |value| {
                    if apply_focused_window_from_client(&shared, &value) {
                        let info = shared.focused_window.read().clone();
                        if let Some(info) = info {
                            callback(info);
                        }
                        // Mango leaves all-clients focus state stale when focus moves to
                        // an empty workspace. The focusing-client watch sends id:null,
                        // which is the only signal that taskbar buttons should all clear
                        // active styling.
                        if apply_window_list_focus_from_client(&shared, &value)
                            && let Some(callback) = &window_list_callback
                        {
                            let windows = shared.windows.read().clone();
                            callback(WindowListSnapshot { windows });
                        }
                    }
                },
            );
        })
    }

    fn spawn_keyboard_layout_watch(
        socket_path: String,
        shared: Arc<MangoSocketSharedState>,
        running: Arc<AtomicBool>,
        callback: KeyboardLayoutCallback,
    ) -> JoinHandle<()> {
        thread::spawn(move || {
            watch_mango_command(socket_path, "watch keyboardlayout", running, move |value| {
                if apply_keyboard_layout_from_value(&shared, &value) {
                    let info = shared.keyboard_layout.read().clone();
                    if let Some(info) = info {
                        callback(info);
                    }
                }
            });
        })
    }

    fn spawn_window_list_watch(
        socket_path: String,
        shared: Arc<MangoSocketSharedState>,
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

impl CompositorBackend for MangoSocketBackend {
    fn start(&self, on_workspace_update: WorkspaceCallback, on_window_update: WindowCallback) {
        if self.running.swap(true, Ordering::SeqCst) {
            warn!("MangoSocketBackend already running");
            return;
        }

        debug!("Starting Mango socket IPC backend");

        self.fetch_initial_state();
        let snapshot = self.shared.snapshot.read().clone();
        on_workspace_update(snapshot);
        let focused_window = self.shared.focused_window.read().clone();
        if let Some(info) = focused_window {
            on_window_update(info);
        }
        if let Some(callback) = self
            .window_list_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            let windows = self.shared.windows.read().clone();
            callback(WindowListSnapshot { windows });
        }
        if let Some(callback) = self
            .keyboard_layout_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            let keyboard_layout = self.shared.keyboard_layout.read().clone();
            if let Some(info) = keyboard_layout {
                callback(info);
            }
        }

        let mut threads = self.watch_threads.lock().unwrap_or_else(|e| e.into_inner());
        threads.push(Self::spawn_workspace_watch(
            self.socket_path.clone(),
            self.shared.clone(),
            self.running.clone(),
            on_workspace_update,
            self.window_list_callback
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        ));
        threads.push(Self::spawn_focused_window_watch(
            self.socket_path.clone(),
            self.shared.clone(),
            self.running.clone(),
            on_window_update,
            self.window_list_callback
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        ));
        if let Some(callback) = self
            .keyboard_layout_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            threads.push(Self::spawn_keyboard_layout_watch(
                self.socket_path.clone(),
                self.shared.clone(),
                self.running.clone(),
                callback,
            ));
        }
        if let Some(callback) = self
            .window_list_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            threads.push(Self::spawn_window_list_watch(
                self.socket_path.clone(),
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
        "MangoWC socket IPC"
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
            .unwrap_or_else(|e| e.into_inner()) = Some(callback.clone());

        if self.running.load(Ordering::SeqCst) {
            let windows = self.shared.windows.read().clone();
            callback(WindowListSnapshot { windows });
        }
    }

    fn focus_window(&self, window_id: u64) {
        let target = self
            .shared
            .windows
            .read()
            .iter()
            .find(|window| window.id == window_id)
            .cloned();

        for command in mango_focus_window_commands(target.as_ref(), window_id) {
            self.send_dispatch(&command);
        }
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

fn apply_workspace_from_monitors(shared: &Arc<MangoSocketSharedState>, value: &Value) -> bool {
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
            }
        }

        if is_overview {
            per_output.active_workspace.insert(OVERVIEW_WORKSPACE_ID);
            if is_focused_monitor {
                snapshot.active_workspace.insert(OVERVIEW_WORKSPACE_ID);
            }
        }
    }

    if max_tag > 0 {
        shared.tag_count.store(max_tag, Ordering::Relaxed);
    }
    if !output_geometry.is_empty() {
        *shared.output_geometry.write() = output_geometry;
    }
    *shared.snapshot.write() = snapshot;
    true
}

fn apply_focused_window_from_client(shared: &Arc<MangoSocketSharedState>, value: &Value) -> bool {
    let focused_client_id = parse_focused_client_id(value);

    let info = if focused_client_id.is_none() {
        WindowInfo::default()
    } else {
        client_value_to_window_info(value)
    };
    *shared.focused_client_id.write() = focused_client_id;
    *shared.focused_window.write() = Some(info);
    true
}

fn parse_focused_client_id(value: &Value) -> Option<u64> {
    if value.get("id").is_some_and(Value::is_null)
        || value.get("error").and_then(Value::as_str) == Some("no focused client")
    {
        None
    } else {
        value.get("id").and_then(Value::as_u64)
    }
}

fn client_value_to_window_info(value: &Value) -> WindowInfo {
    WindowInfo {
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
    }
}

fn apply_window_list_from_clients(shared: &Arc<MangoSocketSharedState>, value: &Value) -> bool {
    let Some(clients) = value.get("clients").and_then(Value::as_array) else {
        return false;
    };

    // Prefer focusing-client once seen. all-clients can keep a previously
    // focused client marked active after switching to an empty workspace.
    let focused_client_id = *shared.focused_client_id.read();
    let focused_client_known = shared.focused_window.read().is_some();
    let output_geometry = shared.output_geometry.read().clone();
    let mut windows: Vec<_> = clients
        .iter()
        .filter_map(|client| {
            client_value_to_window(client, focused_client_id, focused_client_known)
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

    let windows = windows.into_iter().map(|(_, window)| window).collect();
    *shared.windows.write() = windows;
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

fn mango_focus_window_commands(window: Option<&Window>, window_id: u64) -> Vec<String> {
    let mut commands = Vec::new();

    if let Some(window) = window
        && let (Some(workspace_id), Some(output)) = (window.workspace_id, window.output.as_deref())
        && workspace_id > 0
        && !output.is_empty()
    {
        commands.push(format!("dispatch viewcrossmon,{},{}", workspace_id, output));
    }

    commands.push(format!("dispatch focusid client,{}", window_id));
    commands
}

fn apply_window_list_focus_from_client(
    shared: &Arc<MangoSocketSharedState>,
    value: &Value,
) -> bool {
    let focused_id = parse_focused_client_id(value);

    *shared.focused_client_id.write() = focused_id;
    let mut windows = shared.windows.write();
    for window in windows.iter_mut() {
        window.is_focused = focused_id == Some(window.id);
    }
    true
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
    })
}

fn apply_keyboard_layout_from_value(shared: &Arc<MangoSocketSharedState>, value: &Value) -> bool {
    let Some(short_name) = value.get("layout").and_then(Value::as_str) else {
        return false;
    };
    let info = KeyboardLayoutInfo {
        layout_name: xkb_names::language_from_xkb(short_name)
            .map(String::from)
            .unwrap_or_else(|| short_name.to_uppercase()),
        short_name: short_name.to_string(),
        layout_count: None,
    };
    *shared.keyboard_layout.write() = Some(info);
    true
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

/// Per-output state accumulated during a frame.
#[derive(Debug, Clone, Default)]
struct OutputFrameState {
    /// Whether this output is active/focused.
    active: Option<bool>,
    /// Tag updates: (tag_index, is_active, is_urgent, clients, focused)
    tags: Vec<(u32, bool, bool, u32, bool)>,
    /// Window title update.
    title: Option<String>,
    /// Window app_id update.
    appid: Option<String>,
    /// Layout symbol update.
    layout_symbol: Option<String>,
}

impl OutputFrameState {
    fn clear(&mut self) {
        self.active = None;
        self.tags.clear();
        self.title = None;
        self.appid = None;
        self.layout_symbol = None;
    }
}

/// State for a tracked output.
#[derive(Debug)]
struct TrackedOutput {
    /// The wl_output this tracks.
    #[allow(dead_code)]
    wl_output: WlOutput,
    /// The DWL IPC output proxy.
    dwl_output: ZdwlIpcOutputV2,
    /// Output name (from wl_output, if available).
    name: Option<String>,
    /// Buffered frame state.
    frame_state: OutputFrameState,
    /// Last known window title.
    last_title: String,
    /// Last known app_id.
    last_appid: String,
}

/// Thread-safe shared state that can be updated from callbacks.
#[derive(Debug)]
struct SharedState {
    /// Current workspace snapshot.
    snapshot: RwLock<WorkspaceSnapshot>,
    /// Current focused window info.
    focused_window: RwLock<Option<WindowInfo>>,
    /// Current keyboard layout info.
    keyboard_layout: RwLock<Option<KeyboardLayoutInfo>>,
    /// Number of tags from protocol.
    tag_count: AtomicU32,
    /// Pending workspace switch request (-1 = none).
    pending_switch: AtomicI32,
    /// Pending compositor quit request.
    pending_quit: AtomicBool,
    /// Pending keyboard layout switch request.
    pending_kb_switch: AtomicBool,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            snapshot: RwLock::new(WorkspaceSnapshot::default()),
            focused_window: RwLock::new(None),
            keyboard_layout: RwLock::new(None),
            tag_count: AtomicU32::new(DEFAULT_WORKSPACE_COUNT),
            pending_switch: AtomicI32::new(-1),
            pending_quit: AtomicBool::new(false),
            pending_kb_switch: AtomicBool::new(false),
        }
    }
}

/// Main-thread-only Wayland state.
struct WaylandState {
    /// The DWL IPC manager global.
    manager: Option<ZdwlIpcManagerV2>,
    /// Number of tags from protocol.
    tag_count: u32,
    /// Layout names from protocol.
    layouts: Vec<String>,
    /// Tracked outputs by wl_output ObjectId.
    outputs: HashMap<ObjectId, TrackedOutput>,
    /// wl_outputs waiting for DWL manager to be ready.
    pending_outputs: Vec<(WlOutput, Option<String>)>,
    /// Current workspace snapshot (local copy).
    snapshot: WorkspaceSnapshot,
    /// Current focused output ID.
    focused_output: Option<ObjectId>,
    /// Outputs currently reporting Mango's overview layout symbol.
    overview_outputs: HashSet<ObjectId>,
    /// Workspace update callback.
    on_workspace_update: Option<WorkspaceCallback>,
    /// Window update callback.
    on_window_update: Option<WindowCallback>,
    /// Keyboard layout update callback.
    on_keyboard_layout_update: Option<KeyboardLayoutCallback>,
    /// Shared state for cross-thread access.
    shared: Arc<SharedState>,
}

impl WaylandState {
    fn new(shared: Arc<SharedState>) -> Self {
        Self {
            manager: None,
            tag_count: DEFAULT_WORKSPACE_COUNT,
            layouts: Vec::new(),
            outputs: HashMap::new(),
            pending_outputs: Vec::new(),
            snapshot: WorkspaceSnapshot::default(),
            focused_output: None,
            overview_outputs: HashSet::new(),
            on_workspace_update: None,
            on_window_update: None,
            on_keyboard_layout_update: None,
            shared,
        }
    }

    /// Process pending outputs now that we have a manager.
    fn process_pending_outputs(&mut self, qh: &QueueHandle<Self>) {
        let Some(manager) = &self.manager else { return };

        for (wl_output, name) in self.pending_outputs.drain(..) {
            let id = wl_output.id();
            debug!(
                "Creating DWL output for wl_output {:?} (name: {:?})",
                id, name
            );

            let dwl_output = manager.get_output(&wl_output, qh, id.clone());

            self.outputs.insert(
                id,
                TrackedOutput {
                    wl_output,
                    dwl_output,
                    name,
                    frame_state: OutputFrameState::default(),
                    last_title: String::new(),
                    last_appid: String::new(),
                },
            );
        }
    }

    fn output_name(output_id: &ObjectId, output: &TrackedOutput) -> String {
        output
            .name
            .clone()
            .unwrap_or_else(|| format!("output-{output_id:?}"))
    }

    fn is_overview_layout_symbol(symbol: &str) -> bool {
        symbol == OVERVIEW_LAYOUT_SYMBOL || symbol.eq_ignore_ascii_case("overview")
    }

    /// Check for and process any pending workspace switch.
    fn process_pending_switch(&self) {
        let workspace_id = self.shared.pending_switch.swap(-1, Ordering::SeqCst);
        if workspace_id > 0
            && let Some(dwl_output) = self.get_focused_dwl_output()
        {
            let tagmask = 1u32 << (workspace_id - 1);
            debug!("Setting tags to 0x{:x}", tagmask);
            dwl_output.set_tags(tagmask, 0);
        }
    }

    /// Check for and process any pending compositor quit request.
    fn process_pending_quit(&self) {
        if self.shared.pending_quit.swap(false, Ordering::SeqCst)
            && let Some(dwl_output) = self.get_focused_dwl_output()
        {
            debug!("Sending quit request to compositor");
            dwl_output.quit();
        }
    }

    /// Check for and process any pending keyboard layout switch.
    fn process_pending_kb_switch(&self) {
        if self.shared.pending_kb_switch.swap(false, Ordering::SeqCst)
            && let Some(dwl_output) = self.get_focused_dwl_output()
        {
            debug!("Sending keyboard layout switch via dispatch");
            dwl_output.dispatch(
                "switch_keyboard_layout".to_string(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            );
        }
    }

    /// Apply buffered frame state for an output.
    fn apply_frame(&mut self, output_id: &ObjectId) {
        // First, extract all the data we need from the output
        let (output_name, is_focused_output, frame_tags, frame_title, frame_appid, layout_symbol) = {
            let Some(output) = self.outputs.get_mut(output_id) else {
                return;
            };

            // Get output name for per-output tracking
            let output_name = Self::output_name(output_id, output);

            let frame = &mut output.frame_state;

            // Handle active output change
            if let Some(active) = frame.active
                && active
            {
                self.focused_output = Some(output_id.clone());
            }

            let is_focused = self.focused_output.as_ref() == Some(output_id);

            // Clone the data we need
            let tags = frame.tags.clone();
            let title = frame.title.take();
            let appid = frame.appid.take();
            let layout_symbol = frame.layout_symbol.take();

            // Clear frame state for next frame
            frame.clear();

            (output_name, is_focused, tags, title, appid, layout_symbol)
        };

        if let Some(symbol) = layout_symbol {
            if Self::is_overview_layout_symbol(&symbol) {
                self.overview_outputs.insert(output_id.clone());
            } else {
                self.overview_outputs.remove(output_id);
            }
        }
        let is_overview = self.overview_outputs.contains(output_id);

        // Get or create per-output state
        let per_output = self
            .snapshot
            .per_output
            .entry(output_name.clone())
            .or_default();

        // Clear previous per-output state for this output
        per_output.window_counts.clear();
        per_output.occupied_workspaces.clear();
        per_output.active_workspace.clear();

        // Clear global active workspace if this is the focused output
        // (will be rebuilt from the active tags below)
        if is_focused_output {
            self.snapshot.active_workspace.clear();
        }

        // Handle tag updates - store per-output state
        for &(tag, is_active, is_urgent, clients, _focused) in &frame_tags {
            // Tags are 0-indexed in protocol, we use 1-indexed IDs
            let workspace_id = (tag + 1) as i32;

            // In Mango overview, ext-workspace-v1 exposes only workspace 0.
            // Keep the widget-facing per-output tag list equally narrow.
            if !is_overview {
                per_output.window_counts.insert(workspace_id, clients);
                if clients > 0 {
                    per_output.occupied_workspaces.insert(workspace_id);
                }
            }
            if is_active && !is_overview {
                per_output.active_workspace.insert(workspace_id);
            }

            // Update global active workspace (only for focused output)
            if is_active && is_focused_output && !is_overview {
                self.snapshot.active_workspace.insert(workspace_id);
            }

            // Urgent is global (any output can trigger urgency)
            if is_urgent {
                self.snapshot.urgent_workspaces.insert(workspace_id);
            } else {
                self.snapshot.urgent_workspaces.remove(&workspace_id);
            }

            trace!(
                "Tag {} on {}: active={}, urgent={}, clients={}",
                workspace_id, output_name, is_active, is_urgent, clients
            );
        }

        if is_overview {
            per_output.active_workspace.insert(OVERVIEW_WORKSPACE_ID);
            if is_focused_output {
                self.snapshot.active_workspace.insert(OVERVIEW_WORKSPACE_ID);
            }
        }

        // Rebuild global window_counts and occupied from all per-output states
        self.rebuild_global_from_per_output();

        // Handle window info updates
        let mut window_changed = false;
        if let Some(title) = frame_title {
            if let Some(output) = self.outputs.get_mut(output_id) {
                output.last_title = title;
            }
            window_changed = true;
        }
        if let Some(appid) = frame_appid {
            if let Some(output) = self.outputs.get_mut(output_id) {
                output.last_appid = appid;
            }
            window_changed = true;
        }

        // Update shared state
        *self.shared.snapshot.write() = self.snapshot.clone();

        // Emit callbacks
        if let Some(cb) = &self.on_workspace_update {
            cb(self.snapshot.clone());
        }

        if window_changed && is_focused_output {
            // Get the output info for the window.
            // Use the same output_name key that we use for per_output state, so that
            // WindowTitleWidget can reliably match its output_id against this value.
            let window_info = if let Some(output) = self.outputs.get(output_id) {
                WindowInfo {
                    title: output.last_title.clone(),
                    app_id: output.last_appid.clone(),
                    // In multi-tag view, the focused window could be on any of the visible tags.
                    // We pick an arbitrary one since WindowInfo only holds a single workspace_id.
                    workspace_id: self.snapshot.active_workspace.iter().next().copied(),
                    output: Some(output_name.clone()),
                }
            } else {
                return;
            };

            *self.shared.focused_window.write() = Some(window_info.clone());

            if let Some(cb) = &self.on_window_update {
                cb(window_info);
            }
        }
    }

    /// Rebuild global window_counts and occupied_workspaces from per-output state.
    fn rebuild_global_from_per_output(&mut self) {
        self.snapshot.window_counts.clear();
        self.snapshot.occupied_workspaces.clear();

        for per_out in self.snapshot.per_output.values() {
            for (&ws_id, &count) in &per_out.window_counts {
                *self.snapshot.window_counts.entry(ws_id).or_insert(0) += count;
                if count > 0 {
                    self.snapshot.occupied_workspaces.insert(ws_id);
                }
            }
        }
    }

    /// Get the DWL output for switching workspaces.
    fn get_focused_dwl_output(&self) -> Option<&ZdwlIpcOutputV2> {
        let output_id = self
            .focused_output
            .as_ref()
            .or_else(|| self.outputs.keys().next())?;
        self.outputs.get(output_id).map(|o| &o.dwl_output)
    }
}

/// Parse TagState from WEnum.
fn parse_tag_state(state: WEnum<TagState>) -> (bool, bool) {
    match state {
        WEnum::Value(TagState::None) => (false, false),
        WEnum::Value(TagState::Active) => (true, false),
        WEnum::Value(TagState::Urgent) => (false, true),
        // Handle combined states - Active | Urgent would be 3
        WEnum::Unknown(bits) => {
            let is_active = (bits & 1) != 0;
            let is_urgent = (bits & 2) != 0;
            (is_active, is_urgent)
        }
    }
}

impl Dispatch<WlRegistry, ()> for WaylandState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                trace!("Global: {} v{} (name={})", interface, version, name);

                if interface == "zdwl_ipc_manager_v2" {
                    debug!("Found DWL IPC manager v{}", version);
                    let manager: ZdwlIpcManagerV2 = registry.bind(name, version.min(2), qh, ());
                    state.manager = Some(manager);

                    // Process any outputs that were discovered before the manager
                    state.process_pending_outputs(qh);
                } else if interface == "wl_output" {
                    // Bind to wl_output to get it for DWL
                    let wl_output: WlOutput = registry.bind(name, version.min(4), qh, name);

                    if let Some(manager) = &state.manager {
                        // Manager already exists, create DWL output immediately
                        let id = wl_output.id();
                        debug!("Creating DWL output for wl_output {:?}", id);

                        let dwl_output = manager.get_output(&wl_output, qh, id.clone());

                        state.outputs.insert(
                            id,
                            TrackedOutput {
                                wl_output,
                                dwl_output,
                                name: None,
                                frame_state: OutputFrameState::default(),
                                last_title: String::new(),
                                last_appid: String::new(),
                            },
                        );
                    } else {
                        // Queue for later
                        state.pending_outputs.push((wl_output, None));
                    }
                }
            }
            wl_registry::Event::GlobalRemove { name: _ } => {
                // Handle global removal if needed
            }
            _ => {}
        }
    }
}

impl Dispatch<ZdwlIpcManagerV2, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _manager: &ZdwlIpcManagerV2,
        event: zdwl_ipc_manager_v2::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zdwl_ipc_manager_v2::Event::Tags { amount } => {
                debug!("DWL tag count: {}", amount);
                state.tag_count = amount;
                state.shared.tag_count.store(amount, Ordering::Relaxed);
            }
            zdwl_ipc_manager_v2::Event::Layout { name } => {
                debug!("DWL layout: {}", name);
                state.layouts.push(name);
            }
        }
    }
}

impl Dispatch<ZdwlIpcOutputV2, ObjectId> for WaylandState {
    fn event(
        state: &mut Self,
        _output: &ZdwlIpcOutputV2,
        event: zdwl_ipc_output_v2::Event,
        output_id: &ObjectId,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(tracked) = state.outputs.get_mut(output_id) else {
            trace!("Event for unknown output {:?}", output_id);
            return;
        };

        match event {
            zdwl_ipc_output_v2::Event::Active { active } => {
                tracked.frame_state.active = Some(active != 0);
            }
            zdwl_ipc_output_v2::Event::Tag {
                tag,
                state: tag_state,
                clients,
                focused,
            } => {
                let (is_active, is_urgent) = parse_tag_state(tag_state);
                tracked
                    .frame_state
                    .tags
                    .push((tag, is_active, is_urgent, clients, focused != 0));
            }
            zdwl_ipc_output_v2::Event::Title { title } => {
                tracked.frame_state.title = Some(title);
            }
            zdwl_ipc_output_v2::Event::Appid { appid } => {
                tracked.frame_state.appid = Some(appid);
            }
            zdwl_ipc_output_v2::Event::Frame => {
                // Apply all buffered state
                state.apply_frame(output_id);
            }
            zdwl_ipc_output_v2::Event::ToggleVisibility => {}
            zdwl_ipc_output_v2::Event::Layout { layout: _ } => {}
            zdwl_ipc_output_v2::Event::LayoutSymbol { layout } => {
                tracked.frame_state.layout_symbol = Some(layout);
            }
            zdwl_ipc_output_v2::Event::Fullscreen { is_fullscreen: _ } => {}
            zdwl_ipc_output_v2::Event::Floating { is_floating: _ } => {}
            zdwl_ipc_output_v2::Event::KbLayout { kb_layout } => {
                debug!("DWL keyboard layout: {}", kb_layout);

                let info = KeyboardLayoutInfo {
                    layout_name: xkb_names::language_from_xkb(&kb_layout)
                        .map(String::from)
                        .unwrap_or_else(|| kb_layout.to_uppercase()),
                    short_name: kb_layout,
                    layout_count: None,
                };

                *state.shared.keyboard_layout.write() = Some(info.clone());

                if let Some(ref cb) = state.on_keyboard_layout_update {
                    cb(info);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, u32> for WaylandState {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: wayland_client::protocol::wl_output::Event,
        _name: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_output::Event::Name { name } = event {
            // Update the output name if we have it tracked
            let id = output.id();
            if let Some(tracked) = state.outputs.get_mut(&id) {
                tracked.name = Some(name);
            }
        }
    }
}

/// MangoWC/DWL backend using native Wayland protocol.
pub struct MangoBackend {
    /// New Mango socket IPC backend, when available.
    socket_backend: Option<MangoSocketBackend>,
    /// Output allow-list (empty = all outputs).
    #[allow(dead_code)]
    allowed_outputs: RwLock<Vec<String>>,
    /// Shared state accessible from any thread.
    shared: Arc<SharedState>,
    /// Whether the backend is running.
    running: AtomicBool,
    /// glib source IDs for cleanup.
    source_ids: Mutex<Vec<glib::SourceId>>,
    /// Eventfd used to wake the fd watcher for workspace switching.
    wake_fd: Mutex<Option<OwnedFd>>,
    /// Keyboard layout change callback.
    keyboard_layout_callback: Mutex<Option<KeyboardLayoutCallback>>,
}

impl MangoBackend {
    /// Create a new MangoWC/DWL backend.
    pub fn new(outputs: Option<Vec<String>>) -> Self {
        Self {
            socket_backend: MangoSocketBackend::from_env(),
            allowed_outputs: RwLock::new(outputs.unwrap_or_default()),
            shared: Arc::new(SharedState::default()),
            running: AtomicBool::new(false),
            source_ids: Mutex::new(Vec::new()),
            wake_fd: Mutex::new(None),
            keyboard_layout_callback: Mutex::new(None),
        }
    }

    /// Get the Wayland connection by connecting directly.
    fn get_wayland_connection() -> Option<Connection> {
        Connection::connect_to_env().ok()
    }
}

impl CompositorBackend for MangoBackend {
    fn start(&self, on_workspace_update: WorkspaceCallback, on_window_update: WindowCallback) {
        if let Some(socket_backend) = &self.socket_backend {
            socket_backend.start(on_workspace_update, on_window_update);
            return;
        }

        if self.running.swap(true, Ordering::SeqCst) {
            warn!("MangoBackend already running");
            return;
        }

        debug!("Starting MangoBackend with native Wayland protocol");

        // Get the Wayland connection
        let Some(connection) = Self::get_wayland_connection() else {
            error!("Failed to connect to Wayland display");
            self.running.store(false, Ordering::SeqCst);
            return;
        };

        // Create event queue and state
        let event_queue: EventQueue<WaylandState> = connection.new_event_queue();
        let qh = event_queue.handle();

        let shared = self.shared.clone();
        let mut state = WaylandState::new(shared);
        state.on_workspace_update = Some(on_workspace_update.clone());
        state.on_window_update = Some(on_window_update.clone());
        state.on_keyboard_layout_update = self
            .keyboard_layout_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        // Get the registry and bind to globals
        let display = connection.display();
        let _registry = display.get_registry(&qh, ());

        // Wrap in Rc<RefCell<>> for the glib closure
        let event_queue = Rc::new(RefCell::new(event_queue));
        let state = Rc::new(RefCell::new(state));

        // Do initial roundtrips on the main thread via glib
        {
            let mut eq = event_queue.borrow_mut();
            let mut st = state.borrow_mut();

            // Roundtrip to get globals
            if let Err(e) = eq.roundtrip(&mut *st) {
                error!("Wayland roundtrip failed: {}", e);
                self.running.store(false, Ordering::SeqCst);
                return;
            }

            // Check if we found the DWL manager
            if st.manager.is_none() {
                error!("DWL IPC manager not found - is this a MangoWC/DWL compositor?");
                self.running.store(false, Ordering::SeqCst);
                return;
            }

            // Another roundtrip to process DWL manager events
            if let Err(e) = eq.roundtrip(&mut *st) {
                error!("Wayland roundtrip failed: {}", e);
                self.running.store(false, Ordering::SeqCst);
                return;
            }

            debug!(
                "DWL manager ready: {} tags, {} layouts",
                st.tag_count,
                st.layouts.len()
            );
        }

        // Set up fd-based event watching using glib's unix_fd_add_local.
        // This is more efficient than polling - we only wake up when events are available.
        let eq_fd = event_queue.borrow().as_fd().as_raw_fd();
        let shared_for_loop = self.shared.clone();
        let event_queue_for_fd = event_queue.clone();
        let state_for_fd = state.clone();

        // Create eventfd for wake-on-demand workspace switching.
        // This avoids continuous polling - we only wake when switch_workspace() is called.
        // SAFETY: eventfd() is a safe syscall that returns a valid fd or -1 on error.
        let wake_fd_raw = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if wake_fd_raw < 0 {
            error!(
                "Failed to create eventfd: {}",
                std::io::Error::last_os_error()
            );
            self.running.store(false, Ordering::SeqCst);
            return;
        }
        // SAFETY: wake_fd_raw >= 0 (checked above), so it's a valid fd. OwnedFd takes ownership.
        let wake_fd = unsafe { OwnedFd::from_raw_fd(wake_fd_raw) };
        *self.wake_fd.lock().unwrap_or_else(|e| e.into_inner()) = Some(wake_fd);

        let fd_source_id =
            glib::unix_fd_add_local(eq_fd, glib::IOCondition::IN, move |_fd, _condition| {
                // Check for pending workspace switch
                {
                    let st = state_for_fd.borrow();
                    st.process_pending_switch();
                }

                let mut eq = event_queue_for_fd.borrow_mut();
                let mut st = state_for_fd.borrow_mut();

                // Dispatch pending events
                if let Err(e) = eq.dispatch_pending(&mut *st) {
                    error!("Wayland dispatch error: {}", e);
                    return glib::ControlFlow::Break;
                }

                // Prepare read and check for events
                if let Some(guard) = eq.prepare_read() {
                    match guard.read() {
                        Ok(_) => {
                            // Events were read, dispatch them
                            let _ = eq.dispatch_pending(&mut *st);
                        }
                        Err(wayland_client::backend::WaylandError::Io(io_err)) => {
                            if io_err.kind() != std::io::ErrorKind::WouldBlock {
                                error!("Wayland read error: {}", io_err);
                            }
                        }
                        Err(e) => {
                            error!("Wayland error: {}", e);
                        }
                    }
                }

                // Flush any pending requests (like workspace switches)
                let _ = eq.flush();

                // Check if we should stop
                if shared_for_loop.pending_switch.load(Ordering::Relaxed) == i32::MIN {
                    return glib::ControlFlow::Break;
                }

                glib::ControlFlow::Continue
            });

        // Watch the eventfd to wake up for workspace switch requests.
        // When switch_workspace() writes to the eventfd, this callback fires
        // and processes the pending switch without any continuous polling.
        let state_for_wake = state.clone();
        let event_queue_for_wake = event_queue.clone();
        let shared_for_wake = self.shared.clone();

        let wake_source_id =
            glib::unix_fd_add_local(wake_fd_raw, glib::IOCondition::IN, move |fd, _condition| {
                // Drain the eventfd (read the counter to reset it)
                // SAFETY: fd is a valid eventfd from glib callback. Reading 8 bytes (u64 counter)
                // into correctly-sized buffer. Return value ignored - we just need to reset it.
                let mut buf = [0u8; 8];
                unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 8) };

                // Check for stop signal
                let pending = shared_for_wake.pending_switch.load(Ordering::Relaxed);
                if pending == i32::MIN {
                    return glib::ControlFlow::Break;
                }

                // Process the pending switch and quit requests
                {
                    let st = state_for_wake.borrow();
                    st.process_pending_switch();
                    st.process_pending_quit();
                    st.process_pending_kb_switch();
                }

                // Flush to send the request
                let eq = event_queue_for_wake.borrow();
                let _ = eq.flush();

                glib::ControlFlow::Continue
            });

        self.source_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend([fd_source_id, wake_source_id]);

        debug!("MangoBackend started");
    }

    fn stop(&self) {
        if let Some(socket_backend) = &self.socket_backend {
            socket_backend.stop();
            return;
        }

        if !self.running.swap(false, Ordering::SeqCst) {
            return;
        }

        debug!("Stopping MangoBackend");

        // Signal the loop to stop
        self.shared.pending_switch.store(i32::MIN, Ordering::SeqCst);

        // Wake the eventfd watcher so it sees the stop signal
        if let Some(wake_fd) = self
            .wake_fd
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            // SAFETY: wake_fd is valid (held by Mutex), writing 8-byte u64 with correct alignment.
            let val: u64 = 1;
            unsafe {
                libc::write(
                    wake_fd.as_raw_fd(),
                    &val as *const u64 as *const libc::c_void,
                    8,
                );
            }
        }

        // Remove the glib sources
        for source_id in self
            .source_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            source_id.remove();
        }

        // Drop the eventfd
        *self.wake_fd.lock().unwrap_or_else(|e| e.into_inner()) = None;

        debug!("MangoBackend stopped");
    }

    fn list_workspaces(&self) -> Vec<WorkspaceMeta> {
        if let Some(socket_backend) = &self.socket_backend {
            return socket_backend.list_workspaces();
        }

        let count = self.shared.tag_count.load(Ordering::Relaxed);
        let snapshot = self.shared.snapshot.read();
        mango_workspace_meta(count, &snapshot)
    }

    fn get_workspace_snapshot(&self) -> WorkspaceSnapshot {
        if let Some(socket_backend) = &self.socket_backend {
            return socket_backend.get_workspace_snapshot();
        }

        self.shared.snapshot.read().clone()
    }

    fn get_focused_window(&self) -> Option<WindowInfo> {
        if let Some(socket_backend) = &self.socket_backend {
            return socket_backend.get_focused_window();
        }

        self.shared.focused_window.read().clone()
    }

    fn switch_workspace(&self, workspace_id: i32) {
        if let Some(socket_backend) = &self.socket_backend {
            socket_backend.switch_workspace(workspace_id);
            return;
        }

        debug!("Requesting switch to workspace {}", workspace_id);
        self.shared
            .pending_switch
            .store(workspace_id, Ordering::SeqCst);

        // Wake the fd watcher to process the switch immediately.
        // Write any non-zero value to the eventfd.
        if let Some(wake_fd) = self
            .wake_fd
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            // SAFETY: wake_fd is valid (held by Mutex), writing 8-byte u64 with correct alignment.
            let val: u64 = 1;
            unsafe {
                libc::write(
                    wake_fd.as_raw_fd(),
                    &val as *const u64 as *const libc::c_void,
                    8,
                );
            }
        }
    }

    fn quit_compositor(&self) {
        if let Some(socket_backend) = &self.socket_backend {
            socket_backend.quit_compositor();
            return;
        }

        debug!("Requesting compositor quit");
        self.shared.pending_quit.store(true, Ordering::SeqCst);

        // Wake the fd watcher to process the quit immediately.
        if let Some(wake_fd) = self
            .wake_fd
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            // SAFETY: wake_fd is valid (held by Mutex), writing 8-byte u64 with correct alignment.
            let val: u64 = 1;
            unsafe {
                libc::write(
                    wake_fd.as_raw_fd(),
                    &val as *const u64 as *const libc::c_void,
                    8,
                );
            }
        }
    }

    fn name(&self) -> &'static str {
        if let Some(socket_backend) = &self.socket_backend {
            return socket_backend.name();
        }

        "MangoWC/DWL"
    }

    fn set_keyboard_layout_callback(&self, callback: KeyboardLayoutCallback) {
        if let Some(socket_backend) = &self.socket_backend {
            socket_backend.set_keyboard_layout_callback(callback);
            return;
        }

        *self
            .keyboard_layout_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(callback);
    }

    fn get_keyboard_layout(&self) -> Option<KeyboardLayoutInfo> {
        if let Some(socket_backend) = &self.socket_backend {
            return socket_backend.get_keyboard_layout();
        }

        self.shared.keyboard_layout.read().clone()
    }

    fn switch_keyboard_layout_next(&self) {
        if let Some(socket_backend) = &self.socket_backend {
            socket_backend.switch_keyboard_layout_next();
            return;
        }

        // MangoWC uses the dispatch request with "switch_keyboard_layout".
        // We need to signal the main thread to process this, similar to
        // switch_workspace. For now, we store a pending request and wake
        // the eventfd — but dispatch requires a DWL output proxy which
        // is only accessible on the main thread.
        //
        // Since the WaylandState with the DWL output proxy runs on the
        // glib main loop, we use a similar wake mechanism as switch_workspace.
        // However, dispatch is a different operation — we use a separate
        // atomic flag for it.
        debug!("Requesting keyboard layout switch");
        self.shared.pending_kb_switch.store(true, Ordering::SeqCst);

        // Wake the fd watcher to process the switch.
        if let Some(wake_fd) = self
            .wake_fd
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            // SAFETY: wake_fd is valid (held by Mutex), writing 8-byte u64 with correct alignment.
            let val: u64 = 1;
            unsafe {
                libc::write(
                    wake_fd.as_raw_fd(),
                    &val as *const u64 as *const libc::c_void,
                    8,
                );
            }
        }
    }

    fn list_windows(&self) -> Vec<Window> {
        if let Some(socket_backend) = &self.socket_backend {
            return socket_backend.list_windows();
        }

        Vec::new()
    }

    fn set_window_list_callback(&self, callback: WindowListCallback) {
        if let Some(socket_backend) = &self.socket_backend {
            socket_backend.set_window_list_callback(callback);
        }
    }

    fn focus_window(&self, window_id: u64) {
        if let Some(socket_backend) = &self.socket_backend {
            socket_backend.focus_window(window_id);
        }
    }
}

impl Drop for MangoBackend {
    fn drop(&mut self) {
        if let Some(socket_backend) = &self.socket_backend {
            socket_backend.stop();
            return;
        }

        // Signal stop but don't call stop() directly (may already be stopped)
        self.running.store(false, Ordering::SeqCst);
        self.shared.pending_switch.store(i32::MIN, Ordering::SeqCst);
        // Eventfd is dropped automatically via OwnedFd
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::compositor::PerOutputState;

    #[test]
    fn overview_layout_symbol_matches_mango_symbol_and_name() {
        assert!(WaylandState::is_overview_layout_symbol(
            OVERVIEW_LAYOUT_SYMBOL
        ));
        assert!(WaylandState::is_overview_layout_symbol("overview"));
        assert!(!WaylandState::is_overview_layout_symbol("tile"));
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
        let shared = Arc::new(MangoSocketSharedState::default());
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
    }

    #[test]
    fn socket_workspace_parser_global_active_uses_active_monitor_only() {
        let shared = Arc::new(MangoSocketSharedState::default());
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
    fn socket_workspace_parser_suppresses_real_tags_in_overview() {
        let shared = Arc::new(MangoSocketSharedState::default());
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
    fn socket_focused_window_parser_handles_no_client() {
        let shared = Arc::new(MangoSocketSharedState::default());
        let value = serde_json::json!({"id": null});

        assert!(apply_focused_window_from_client(&shared, &value));

        assert_eq!(
            shared.focused_window.read().clone(),
            Some(WindowInfo::default())
        );
    }

    #[test]
    fn socket_window_list_parser_maps_mango_clients() {
        let shared = Arc::new(MangoSocketSharedState::default());
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
        let shared = Arc::new(MangoSocketSharedState::default());
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
        let shared = Arc::new(MangoSocketSharedState::default());
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
        let shared = Arc::new(MangoSocketSharedState::default());
        let clients = serde_json::json!({
            "clients": [
                {"id": 10, "tags": [1], "is_focused": true},
                {"id": 20, "tags": [2], "is_focused": false}
            ]
        });

        assert!(apply_window_list_from_clients(&shared, &clients));
        assert!(shared.windows.read()[0].is_focused);

        assert!(apply_window_list_focus_from_client(
            &shared,
            &serde_json::json!({"id": null})
        ));

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
        let shared = Arc::new(MangoSocketSharedState::default());
        let clients = serde_json::json!({
            "clients": [
                {"id": 10, "tags": [1], "is_focused": true},
                {"id": 20, "tags": [2], "is_focused": false}
            ]
        });

        assert!(apply_window_list_from_clients(&shared, &clients));
        assert!(apply_window_list_focus_from_client(
            &shared,
            &serde_json::json!({"id": 20})
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
    fn socket_window_list_parser_ignores_stale_all_clients_focus() {
        let shared = Arc::new(MangoSocketSharedState::default());

        assert!(apply_focused_window_from_client(
            &shared,
            &serde_json::json!({"id": 20})
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
    fn socket_focus_window_commands_switch_target_output_workspace_first() {
        let window = Window {
            id: 13,
            workspace_id: Some(5),
            output: Some("DP-1".to_string()),
            ..Default::default()
        };

        assert_eq!(
            mango_focus_window_commands(Some(&window), window.id),
            vec![
                "dispatch viewcrossmon,5,DP-1".to_string(),
                "dispatch focusid client,13".to_string()
            ]
        );
    }

    #[test]
    fn socket_focus_window_commands_focus_directly_without_location() {
        assert_eq!(
            mango_focus_window_commands(None, 13),
            vec!["dispatch focusid client,13".to_string()]
        );
    }

    #[test]
    fn socket_window_list_parser_handles_optional_fields() {
        let shared = Arc::new(MangoSocketSharedState::default());
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
    fn socket_keyboard_layout_parser_maps_layout_name() {
        let shared = Arc::new(MangoSocketSharedState::default());
        let value = serde_json::json!({"layout": "us"});

        assert!(apply_keyboard_layout_from_value(&shared, &value));
        let info = shared.keyboard_layout.read().clone().unwrap();

        assert_eq!(info.short_name, "us");
        assert_eq!(info.layout_name, "English");
        assert_eq!(info.layout_count, None);
    }
}
