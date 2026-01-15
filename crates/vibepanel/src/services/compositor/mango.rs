//! MangoWC / DWL compositor backend using native Wayland protocol.
//!
//! This backend supports MangoWC and DWL compositors using the `zdwl_ipc_manager_v2`
//! Wayland protocol for IPC. It shares the Wayland connection from GDK and
//! dispatches events via glib's main loop.
//!
//! # Protocol
//!
//! The DWL IPC protocol provides:
//! - Tag/workspace state: active, urgent, client count, focus state
//! - Window info: title, app_id
//! - Workspace switching via `set_tags`
//!
//! Events are double-buffered: state is collected and applied on `frame` events.

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use gtk4::glib;
use parking_lot::RwLock;
use tracing::{debug, error, trace, warn};
use wayland_backend::client::ObjectId;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};

use super::dwl_ipc::{
    TagState, ZdwlIpcManagerV2, ZdwlIpcOutputV2, zdwl_ipc_manager_v2, zdwl_ipc_output_v2,
};
use super::{
    CompositorBackend, WindowCallback, WindowInfo, WorkspaceCallback, WorkspaceMeta,
    WorkspaceSnapshot,
};

/// Default number of workspaces/tags for DWL.
const DEFAULT_WORKSPACE_COUNT: u32 = 9;

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
}

impl OutputFrameState {
    fn clear(&mut self) {
        self.active = None;
        self.tags.clear();
        self.title = None;
        self.appid = None;
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
    /// Number of tags from protocol.
    tag_count: AtomicU32,
    /// Pending workspace switch request (-1 = none).
    pending_switch: AtomicI32,
    /// Pending compositor quit request.
    pending_quit: AtomicBool,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            snapshot: RwLock::new(WorkspaceSnapshot::default()),
            focused_window: RwLock::new(None),
            tag_count: AtomicU32::new(DEFAULT_WORKSPACE_COUNT),
            pending_switch: AtomicI32::new(-1),
            pending_quit: AtomicBool::new(false),
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
    /// Workspace update callback.
    on_workspace_update: Option<WorkspaceCallback>,
    /// Window update callback.
    on_window_update: Option<WindowCallback>,
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
            on_workspace_update: None,
            on_window_update: None,
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

    /// Apply buffered frame state for an output.
    fn apply_frame(&mut self, output_id: &ObjectId) {
        // First, extract all the data we need from the output
        let (output_name, is_focused_output, frame_tags, frame_title, frame_appid) = {
            let Some(output) = self.outputs.get_mut(output_id) else {
                return;
            };

            let frame = &mut output.frame_state;

            // Get output name for per-output tracking
            let output_name = output
                .name
                .clone()
                .unwrap_or_else(|| format!("output-{:?}", output_id));

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

            // Clear frame state for next frame
            frame.clear();

            (output_name, is_focused, tags, title, appid)
        };

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

            // Update per-output state
            per_output.window_counts.insert(workspace_id, clients);
            if clients > 0 {
                per_output.occupied_workspaces.insert(workspace_id);
            }
            if is_active {
                per_output.active_workspace.insert(workspace_id);
            }

            // Update global active workspace (only for focused output)
            if is_active && is_focused_output {
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
            zdwl_ipc_output_v2::Event::LayoutSymbol { layout: _ } => {}
            zdwl_ipc_output_v2::Event::Fullscreen { is_fullscreen: _ } => {}
            zdwl_ipc_output_v2::Event::Floating { is_floating: _ } => {}
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
}

impl MangoBackend {
    /// Create a new MangoWC/DWL backend.
    pub fn new(outputs: Option<Vec<String>>) -> Self {
        Self {
            allowed_outputs: RwLock::new(outputs.unwrap_or_default()),
            shared: Arc::new(SharedState::default()),
            running: AtomicBool::new(false),
            source_ids: Mutex::new(Vec::new()),
            wake_fd: Mutex::new(None),
        }
    }

    /// Get the Wayland connection by connecting directly.
    fn get_wayland_connection() -> Option<Connection> {
        Connection::connect_to_env().ok()
    }
}

impl CompositorBackend for MangoBackend {
    fn start(&self, on_workspace_update: WorkspaceCallback, on_window_update: WindowCallback) {
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
        let count = self.shared.tag_count.load(Ordering::Relaxed);
        (1..=count as i32)
            .map(|id| WorkspaceMeta {
                id,
                name: id.to_string(),
                output: None, // MangoWC/DWL tags are global
            })
            .collect()
    }

    fn get_workspace_snapshot(&self) -> WorkspaceSnapshot {
        self.shared.snapshot.read().clone()
    }

    fn get_focused_window(&self) -> Option<WindowInfo> {
        self.shared.focused_window.read().clone()
    }

    fn switch_workspace(&self, workspace_id: i32) {
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
        "MangoWC/DWL"
    }
}

impl Drop for MangoBackend {
    fn drop(&mut self) {
        // Signal stop but don't call stop() directly (may already be stopped)
        self.running.store(false, Ordering::SeqCst);
        self.shared.pending_switch.store(i32::MIN, Ordering::SeqCst);
        // Eventfd is dropped automatically via OwnedFd
    }
}
