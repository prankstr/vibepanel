//! `ext-workspace-v1` protocol client.
//!
//! [`ExtWorkspaceClient`] tracks the full protocol state (workspaces, groups,
//! outputs) on its own Wayland connection and thread, and publishes an
//! [`ExtWorkspaceModel`] after every `done`. It has two consumers in this
//! module: the standalone [`super::ExtWorkspaceBackend`] and the overlay that
//! `CompositorManager` merges over the Hyprland backend.
//!
//! It deliberately does not share GDK's connection: compositor backends run
//! off the GTK main loop and start before any GTK surface exists.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread::{self, JoinHandle};

use parking_lot::Mutex;
use tracing::{debug, trace, warn};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, event_created_child,
};
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1, State as WsState},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};

use crate::services::compositor::wayland_loop;

pub(crate) const MANAGER_INTERFACE: &str = "ext_workspace_manager_v1";

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// One workspace as reported over ext-workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtWorkspace {
    /// Workspace ID for its whole lifetime; never reused.
    pub id: i32,
    pub name: String,
    pub coordinates: Vec<u32>,
    pub active: bool,
    pub urgent: bool,
    pub hidden: bool,
    /// Index into [`ExtWorkspaceModel::groups`].
    pub group: Option<usize>,
}

/// One workspace group (usually one per output).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtWorkspaceGroup {
    /// Connector names of the outputs this group is shown on.
    pub outputs: Vec<String>,
}

/// Complete ext-workspace state after a `done` event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtWorkspaceModel {
    pub workspaces: Vec<ExtWorkspace>,
    pub groups: Vec<ExtWorkspaceGroup>,
}

// ---------------------------------------------------------------------------
// Protocol client
// ---------------------------------------------------------------------------

/// Source of workspace IDs. Process-wide and never reset, so an ID is never
/// reused. Wayland object ids are recycled, and the protocol `id` may arrive
/// after the workspace, so neither is used.
static NEXT_ID: AtomicI32 = AtomicI32::new(1);

#[derive(Debug)]
struct WsEntry {
    id: i32,
    name: String,
    coordinates: Vec<u32>,
    state: u32,
    group: Option<u32>,
}

impl WsEntry {
    fn new() -> Self {
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            name: String::new(),
            coordinates: Vec::new(),
            state: 0,
            group: None,
        }
    }
}

struct ClientState {
    manager: Option<ExtWorkspaceManagerV1>,
    /// wl_output protocol id -> connector name.
    outputs: HashMap<u32, String>,
    /// Registry global name -> bound wl_output, to release it on removal.
    output_globals: HashMap<u32, wl_output::WlOutput>,
    /// group protocol id -> output protocol ids.
    groups: HashMap<u32, Vec<u32>>,
    /// workspace protocol id -> state.
    workspaces: HashMap<u32, (ExtWorkspaceHandleV1, WsEntry)>,
    last_model: Option<ExtWorkspaceModel>,
    shared: Arc<ClientShared>,
    on_update: ModelCallback,
}

impl ClientState {
    fn build_model(&self) -> ExtWorkspaceModel {
        let mut group_ids: Vec<u32> = self.groups.keys().copied().collect();
        group_ids.sort_unstable();
        let group_index: HashMap<u32, usize> =
            group_ids.iter().enumerate().map(|(i, g)| (*g, i)).collect();
        let groups = group_ids
            .iter()
            .map(|g| ExtWorkspaceGroup {
                outputs: self.groups[g]
                    .iter()
                    .filter_map(|o| self.outputs.get(o).filter(|n| !n.is_empty()).cloned())
                    .collect(),
            })
            .collect();

        let mut ids: Vec<u32> = self.workspaces.keys().copied().collect();
        ids.sort_unstable();
        let workspaces = ids
            .iter()
            .map(|id| {
                let entry = &self.workspaces[id].1;
                ExtWorkspace {
                    id: entry.id,
                    name: entry.name.clone(),
                    coordinates: entry.coordinates.clone(),
                    active: entry.state & WsState::Active.bits() != 0,
                    urgent: entry.state & WsState::Urgent.bits() != 0,
                    hidden: entry.state & WsState::Hidden.bits() != 0,
                    group: entry.group.and_then(|g| group_index.get(&g).copied()),
                }
            })
            .collect();

        ExtWorkspaceModel { workspaces, groups }
    }

    /// Forget everything and tell consumers, e.g. after losing the connection,
    /// so an overlay falls back to native state instead of going stale.
    fn reset(&mut self) {
        self.manager = None;
        self.groups.clear();
        self.workspaces.clear();
        self.publish();
    }

    fn publish(&mut self) {
        let model = self.build_model();

        *self.shared.handles.lock() = self
            .workspaces
            .values()
            .map(|(handle, entry)| (entry.id, handle.clone()))
            .collect();
        *self.shared.manager.lock() = self.manager.clone();

        if self.last_model.as_ref() == Some(&model) {
            return;
        }
        trace!("ext-workspace model: {:?}", model);
        self.last_model = Some(model.clone());
        (self.on_update)(model);
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for ClientState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
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
            } => match interface.as_str() {
                MANAGER_INTERFACE if state.manager.is_none() => {
                    state.manager = Some(registry.bind(name, 1, qh, ()));
                }
                "wl_output" if version >= 4 => {
                    let output: wl_output::WlOutput = registry.bind(name, 4, qh, ());
                    state
                        .outputs
                        .insert(output.id().protocol_id(), String::new());
                    state.output_globals.insert(name, output);
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(output) = state.output_globals.remove(&name) {
                    // Object ids are recycled, so drop every reference.
                    let oid = output.id().protocol_id();
                    state.outputs.remove(&oid);
                    for outputs in state.groups.values_mut() {
                        outputs.retain(|o| *o != oid);
                    }
                    output.release();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for ClientState {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            state.outputs.insert(output.id().protocol_id(), name);
        }
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_workspace_manager_v1::Event::WorkspaceGroup { workspace_group } => {
                state
                    .groups
                    .insert(workspace_group.id().protocol_id(), Vec::new());
            }
            ext_workspace_manager_v1::Event::Workspace { workspace } => {
                state
                    .workspaces
                    .insert(workspace.id().protocol_id(), (workspace, WsEntry::new()));
            }
            ext_workspace_manager_v1::Event::Done => state.publish(),
            ext_workspace_manager_v1::Event::Finished => {
                debug!("ext-workspace manager finished");
                state.reset();
            }
            _ => {}
        }
    }

    event_created_child!(ClientState, ExtWorkspaceManagerV1, [
        ext_workspace_manager_v1::EVT_WORKSPACE_GROUP_OPCODE => (ExtWorkspaceGroupHandleV1, ()),
        ext_workspace_manager_v1::EVT_WORKSPACE_OPCODE => (ExtWorkspaceHandleV1, ()),
    ]);
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        group: &ExtWorkspaceGroupHandleV1,
        event: ext_workspace_group_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let gid = group.id().protocol_id();
        match event {
            ext_workspace_group_handle_v1::Event::OutputEnter { output } => {
                let oid = output.id().protocol_id();
                let outputs = state.groups.entry(gid).or_default();
                if !outputs.contains(&oid) {
                    outputs.push(oid);
                }
            }
            ext_workspace_group_handle_v1::Event::OutputLeave { output } => {
                let oid = output.id().protocol_id();
                if let Some(outputs) = state.groups.get_mut(&gid) {
                    outputs.retain(|o| *o != oid);
                }
            }
            ext_workspace_group_handle_v1::Event::WorkspaceEnter { workspace } => {
                if let Some((_, entry)) = state.workspaces.get_mut(&workspace.id().protocol_id()) {
                    entry.group = Some(gid);
                }
            }
            ext_workspace_group_handle_v1::Event::WorkspaceLeave { workspace } => {
                if let Some((_, entry)) = state.workspaces.get_mut(&workspace.id().protocol_id())
                    && entry.group == Some(gid)
                {
                    entry.group = None;
                }
            }
            ext_workspace_group_handle_v1::Event::Removed => {
                state.groups.remove(&gid);
                for (_, entry) in state.workspaces.values_mut() {
                    if entry.group == Some(gid) {
                        entry.group = None;
                    }
                }
                group.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        handle: &ExtWorkspaceHandleV1,
        event: ext_workspace_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let wid = handle.id().protocol_id();
        if let ext_workspace_handle_v1::Event::Removed = event {
            state.workspaces.remove(&wid);
            handle.destroy();
            return;
        }
        let Some((_, entry)) = state.workspaces.get_mut(&wid) else {
            return;
        };
        match event {
            ext_workspace_handle_v1::Event::Name { name } => entry.name = name,
            ext_workspace_handle_v1::Event::Coordinates { coordinates } => {
                entry.coordinates = coordinates
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| u32::from_ne_bytes(*c))
                    .collect();
            }
            ext_workspace_handle_v1::Event::State { state: ws_state } => {
                entry.state = match ws_state {
                    WEnum::Value(value) => value.bits(),
                    WEnum::Unknown(raw) => raw,
                };
            }
            _ => {}
        }
    }
}

/// Called (on the client thread) with every changed model.
pub type ModelCallback = Arc<dyn Fn(ExtWorkspaceModel) + Send + Sync>;

/// State shared between the client thread and request callers.
struct ClientShared {
    connection: Connection,
    manager: Mutex<Option<ExtWorkspaceManagerV1>>,
    handles: Mutex<HashMap<i32, ExtWorkspaceHandleV1>>,
    running: AtomicBool,
}

/// ext-workspace protocol client on a dedicated connection and thread.
pub struct ExtWorkspaceClient {
    shared: Arc<ClientShared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl ExtWorkspaceClient {
    /// Connect and start tracking. Returns `None` if there is no Wayland
    /// display or the compositor lacks the protocol.
    pub fn start(on_update: ModelCallback) -> Option<Self> {
        let connection = Connection::connect_to_env()
            .map_err(|e| debug!("ext-workspace: no Wayland connection: {e}"))
            .ok()?;
        let mut queue: EventQueue<ClientState> = connection.new_event_queue();
        let qh = queue.handle();
        let _registry = connection.display().get_registry(&qh, ());

        let shared = Arc::new(ClientShared {
            connection: connection.clone(),
            manager: Mutex::new(None),
            handles: Mutex::new(HashMap::new()),
            running: AtomicBool::new(true),
        });
        let mut state = ClientState {
            manager: None,
            outputs: HashMap::new(),
            output_globals: HashMap::new(),
            groups: HashMap::new(),
            workspaces: HashMap::new(),
            last_model: None,
            shared: shared.clone(),
            on_update,
        };

        // Globals, then output names and the initial workspace burst.
        for _ in 0..2 {
            if let Err(e) = queue.roundtrip(&mut state) {
                warn!("ext-workspace roundtrip failed: {e}");
                return None;
            }
        }
        if state.manager.is_none() {
            debug!("Compositor does not advertise {MANAGER_INTERFACE}");
            return None;
        }

        let thread_shared = shared.clone();
        let thread = thread::Builder::new()
            .name("ext-workspace".into())
            .spawn(move || run_loop(queue, state, thread_shared))
            .ok()?;

        debug!("ext-workspace client started");
        Some(Self {
            shared,
            thread: Mutex::new(Some(thread)),
        })
    }

    /// Ask the compositor to activate the workspace with this model ID.
    pub fn activate(&self, id: i32) {
        let Some(manager) = self.shared.manager.lock().clone() else {
            return;
        };
        let Some(handle) = self.shared.handles.lock().get(&id).cloned() else {
            debug!("ext-workspace: no workspace with ID {id}");
            return;
        };
        handle.activate();
        manager.commit();
        if let Err(e) = self.shared.connection.flush() {
            warn!("ext-workspace: flush failed: {e}");
        }
    }

    pub fn stop(&self) {
        self.shared.running.store(false, Ordering::SeqCst);
        if let Some(thread) = self.thread.lock().take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ExtWorkspaceClient {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_loop(mut queue: EventQueue<ClientState>, mut state: ClientState, shared: Arc<ClientShared>) {
    if !wayland_loop::run(&mut queue, &mut state, &shared.running, "ext-workspace") {
        state.reset();
    }
    debug!("ext-workspace client stopped");
}
