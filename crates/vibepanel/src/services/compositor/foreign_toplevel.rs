//! `wlr-foreign-toplevel-management-unstable-v1` protocol client.
//!
//! Window titles for the generic backend: the focused window, and the last
//! focused window on each unfocused output. The protocol has no notion of
//! workspaces, so it cannot provide occupancy.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use parking_lot::Mutex;
use tracing::{debug, trace, warn};
use wayland_client::protocol::{wl_callback, wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, event_created_child};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, State, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

use super::wayland_loop;
use super::{WindowCallback, WindowInfo};

pub(super) const MANAGER_INTERFACE: &str = "zwlr_foreign_toplevel_manager_v1";
const MANAGER_VERSION: u32 = 3;

#[derive(Debug, Clone, Default)]
struct ToplevelState {
    title: String,
    app_id: String,
    /// wl_output protocol ids.
    outputs: Vec<u32>,
    activated: bool,
}

/// Events write to `pending`; the toplevel's own `done` commits it, so a
/// half-received batch is never published.
#[derive(Debug, Default)]
struct Toplevel {
    pending: ToplevelState,
    current: ToplevelState,
    activated_at: u64,
}

/// Window state and the publish decision, independent of Wayland objects.
#[derive(Default)]
struct Model {
    /// wl_output protocol id -> connector name.
    outputs: HashMap<u32, String>,
    /// toplevel protocol id -> state.
    toplevels: HashMap<u32, Toplevel>,
    activations: u64,
    focused: Option<WindowInfo>,
    /// Output name -> toplevel its bar shows and the update last sent for it.
    /// The protocol can't tell which window is visible on an unfocused output,
    /// so the last focused one is kept until it closes or leaves that output.
    shown: HashMap<String, (u32, WindowInfo)>,
    /// Set once any output is named. Stays set after the last output is
    /// removed, so titles keep waiting for an output instead of going to
    /// every bar.
    names_known: bool,
}

impl Model {
    fn commit(&mut self, tid: u32) {
        let Some(toplevel) = self.toplevels.get_mut(&tid) else {
            return;
        };
        if toplevel.pending.activated && !toplevel.current.activated {
            self.activations += 1;
            toplevel.activated_at = self.activations;
        }
        toplevel.current = toplevel.pending.clone();
    }

    /// Returns an empty update if a bar showed a window on the output, so it
    /// isn't replayed when the output reconnects.
    fn remove_output(&mut self, oid: u32) -> Option<WindowInfo> {
        for toplevel in self.toplevels.values_mut() {
            toplevel.pending.outputs.retain(|o| *o != oid);
            toplevel.current.outputs.retain(|o| *o != oid);
        }
        let name = self.outputs.remove(&oid)?;
        // Forget focus on the output, so the window is republished if it
        // returns with the same state.
        let focused = self
            .focused
            .take_if(|f| f.output.as_deref() == Some(name.as_str()))
            .is_some();
        self.shown.remove(&name)?;
        Some(WindowInfo {
            output: Some(name),
            focused,
            ..Default::default()
        })
    }

    fn output_name(&self, window: &ToplevelState) -> Option<String> {
        window
            .outputs
            .iter()
            .find_map(|o| self.outputs.get(o).filter(|n| !n.is_empty()).cloned())
    }

    /// `tid`'s update for an unfocused `output`, if it is still on it.
    fn unfocused_info(&self, tid: u32, output: &str) -> Option<WindowInfo> {
        let window = &self.toplevels.get(&tid)?.current;
        let on_output = window
            .outputs
            .iter()
            .any(|o| self.outputs.get(o).is_some_and(|n| n == output));
        on_output.then(|| WindowInfo {
            title: window.title.clone(),
            app_id: window.app_id.clone(),
            output: Some(output.to_string()),
            focused: false,
        })
    }

    fn updates(&mut self) -> Vec<WindowInfo> {
        // Compositors usually report at most one activated toplevel; if there
        // are several, the most recently activated wins.
        let next = self
            .toplevels
            .iter()
            .filter(|(_, t)| t.current.activated)
            .max_by_key(|(_, t)| t.activated_at)
            .map(|(tid, t)| {
                let info = WindowInfo {
                    title: t.current.title.clone(),
                    app_id: t.current.app_id.clone(),
                    output: self.output_name(&t.current),
                    focused: true,
                };
                (*tid, info)
            });

        // An update without an output replaces every bar's title. Unless the
        // compositor never names outputs, hold back the focused window's
        // update until its output is known.
        let waiting =
            self.names_known && next.as_ref().is_some_and(|(_, info)| info.output.is_none());

        let mut updates = Vec::new();
        match &next {
            Some(_) if waiting => debug!("foreign-toplevel: focused window has no output yet"),
            Some((tid, info)) => {
                if self.focused.as_ref() != Some(info) {
                    updates.push(info.clone());
                }
                if let Some(output) = &info.output {
                    let sent = WindowInfo {
                        focused: false,
                        ..info.clone()
                    };
                    self.shown.insert(output.clone(), (*tid, sent));
                }
            }
            // Focus lost: clear the previously focused output. The protocol
            // can't tell where focus went (e.g. an empty workspace elsewhere).
            None => {
                if let Some(prev) = self.focused.take() {
                    if let Some(output) = &prev.output {
                        self.shown.remove(output);
                    }
                    updates.push(WindowInfo {
                        output: prev.output,
                        focused: true,
                        ..Default::default()
                    });
                }
            }
        }

        let focused_output = next.as_ref().and_then(|(_, info)| info.output.as_deref());
        let changed: Vec<(String, Option<WindowInfo>)> = self
            .shown
            .iter()
            .filter(|(output, _)| Some(output.as_str()) != focused_output)
            .filter_map(|(output, (tid, sent))| {
                let info = self.unfocused_info(*tid, output);
                (info.as_ref() != Some(sent)).then(|| (output.clone(), info))
            })
            .collect();
        for (output, info) in changed {
            match info {
                Some(info) => {
                    if let Some(entry) = self.shown.get_mut(&output) {
                        entry.1 = info.clone();
                    }
                    updates.push(info);
                }
                // Closed or left the output.
                None => {
                    self.shown.remove(&output);
                    updates.push(WindowInfo {
                        output: Some(output),
                        ..Default::default()
                    });
                }
            }
        }

        if !waiting {
            self.focused = next.map(|(_, info)| info);
        }
        updates
    }
}

struct ClientState {
    manager: Option<ZwlrForeignToplevelManagerV1>,
    /// Manager global (name, version) waiting to be bound; see `start`.
    manager_global: Option<(u32, u32)>,
    defer_manager: bool,
    /// Registry global name -> bound wl_output, to release it on removal.
    output_globals: HashMap<u32, wl_output::WlOutput>,
    handles: HashMap<u32, ZwlrForeignToplevelHandleV1>,
    model: Model,
    /// Holds back updates until output names are known.
    ready: bool,
    sync_pending: bool,
    on_update: WindowCallback,
}

impl ClientState {
    /// Publish once the compositor has flushed everything it sent so far.
    /// `done` is per toplevel, so publishing on each would briefly see the old
    /// window unfocused and the new one not yet focused.
    fn schedule_publish(&mut self, conn: &Connection, qh: &QueueHandle<Self>) {
        if self.ready && !self.sync_pending {
            conn.display().sync(qh, ());
            self.sync_pending = true;
        }
    }

    fn publish(&mut self) {
        if !self.ready {
            return;
        }
        for info in self.model.updates() {
            trace!("foreign-toplevel update: {:?}", info);
            (self.on_update)(info);
        }
    }

    fn reset(&mut self) {
        self.manager = None;
        for handle in self.handles.values() {
            handle.destroy();
        }
        self.handles.clear();
        self.model.toplevels.clear();
        self.publish();
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for ClientState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                MANAGER_INTERFACE if state.manager.is_none() && state.manager_global.is_none() => {
                    let version = version.min(MANAGER_VERSION);
                    if state.defer_manager {
                        state.manager_global = Some((name, version));
                    } else {
                        state.manager = Some(registry.bind(name, version, qh, ()));
                    }
                }
                "wl_output" if version >= 4 => {
                    let output: wl_output::WlOutput = registry.bind(name, 4, qh, ());
                    state
                        .model
                        .outputs
                        .insert(output.id().protocol_id(), String::new());
                    state.output_globals.insert(name, output);
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(output) = state.output_globals.remove(&name) {
                    // Object ids are recycled, so drop every reference.
                    let cleared = state.model.remove_output(output.id().protocol_id());
                    output.release();
                    if let Some(info) = cleared.filter(|_| state.ready) {
                        (state.on_update)(info);
                    }
                    state.schedule_publish(conn, qh);
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
        conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            state.model.outputs.insert(output.id().protocol_id(), name);
            state.model.names_known = true;
            state.schedule_publish(conn, qh);
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for ClientState {
    fn event(
        state: &mut Self,
        _callback: &wl_callback::WlCallback,
        _event: wl_callback::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        state.sync_pending = false;
        state.publish();
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } => {
                let tid = toplevel.id().protocol_id();
                state.model.toplevels.insert(tid, Toplevel::default());
                state.handles.insert(tid, toplevel);
            }
            zwlr_foreign_toplevel_manager_v1::Event::Finished => {
                debug!("foreign-toplevel manager finished");
                state.reset();
            }
            _ => {}
        }
    }

    event_created_child!(ClientState, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        handle: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _data: &(),
        conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let tid = handle.id().protocol_id();
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                state.model.toplevels.remove(&tid);
                state.handles.remove(&tid);
                handle.destroy();
                state.schedule_publish(conn, qh);
                return;
            }
            zwlr_foreign_toplevel_handle_v1::Event::Done => {
                state.model.commit(tid);
                state.schedule_publish(conn, qh);
                return;
            }
            _ => {}
        }

        let Some(toplevel) = state.model.toplevels.get_mut(&tid) else {
            return;
        };
        let pending = &mut toplevel.pending;
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => pending.title = title,
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => pending.app_id = app_id,
            zwlr_foreign_toplevel_handle_v1::Event::OutputEnter { output } => {
                let oid = output.id().protocol_id();
                if !pending.outputs.contains(&oid) {
                    pending.outputs.push(oid);
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::OutputLeave { output } => {
                let oid = output.id().protocol_id();
                pending.outputs.retain(|o| *o != oid);
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state: states } => {
                pending.activated = states
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|c| u32::from_ne_bytes(*c) == State::Activated as u32);
            }
            _ => {}
        }
    }
}

/// Runs on its own Wayland connection and thread.
pub struct ForeignToplevelClient {
    running: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl ForeignToplevelClient {
    /// `on_update` is called on the client thread with the focused window and
    /// with changes to the window shown on each unfocused output. Returns
    /// `None` if there is no Wayland display or the compositor lacks the
    /// protocol.
    pub fn start(on_update: WindowCallback) -> Option<Self> {
        let connection = Connection::connect_to_env()
            .map_err(|e| debug!("foreign-toplevel: no Wayland connection: {e}"))
            .ok()?;
        let mut queue: EventQueue<ClientState> = connection.new_event_queue();
        let qh = queue.handle();
        let registry = connection.display().get_registry(&qh, ());

        let mut state = ClientState {
            manager: None,
            manager_global: None,
            defer_manager: true,
            output_globals: HashMap::new(),
            handles: HashMap::new(),
            model: Model::default(),
            ready: false,
            sync_pending: false,
            on_update,
        };

        let roundtrip = |queue: &mut EventQueue<ClientState>, state: &mut ClientState| {
            queue
                .roundtrip(state)
                .map_err(|e| warn!("foreign-toplevel roundtrip failed: {e}"))
                .ok()
        };
        // Globals first. The manager is bound after the outputs, because
        // `output_enter` is only sent for outputs the client has bound.
        roundtrip(&mut queue, &mut state)?;
        state.defer_manager = false;
        if let Some((name, version)) = state.manager_global.take() {
            state.manager = Some(registry.bind(name, version, &qh, ()));
        }
        // Output names and the initial toplevel burst.
        roundtrip(&mut queue, &mut state)?;
        if state.manager.is_none() {
            debug!("Compositor does not advertise {MANAGER_INTERFACE}");
            return None;
        }
        state.ready = true;
        state.publish();

        let running = Arc::new(AtomicBool::new(true));
        let thread_running = running.clone();
        let thread = thread::Builder::new()
            .name("foreign-toplevel".into())
            .spawn(move || {
                if !wayland_loop::run(&mut queue, &mut state, &thread_running, "foreign-toplevel") {
                    state.reset();
                }
                debug!("foreign-toplevel client stopped");
            })
            .map_err(|e| warn!("foreign-toplevel: failed to spawn thread: {e}"))
            .ok()?;

        debug!("foreign-toplevel client started");
        Some(Self {
            running,
            thread: Mutex::new(Some(thread)),
        })
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(thread) = self.thread.lock().take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ForeignToplevelClient {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: u32 = 10;
    const B: u32 = 20;

    /// A focused on DP-1 and B on DP-2, already published.
    fn model() -> Model {
        let mut m = Model {
            outputs: HashMap::from([(1, "DP-1".into()), (2, "DP-2".into())]),
            names_known: true,
            ..Default::default()
        };
        for (tid, output, title) in [(A, 1, "a"), (B, 2, "b")] {
            m.toplevels.insert(tid, Toplevel::default());
            let p = pending(&mut m, tid);
            p.title = title.into();
            p.outputs = vec![output];
            p.activated = tid == A;
            m.commit(tid);
        }
        m.updates();
        m
    }

    fn pending(m: &mut Model, tid: u32) -> &mut ToplevelState {
        &mut m.toplevels.get_mut(&tid).unwrap().pending
    }

    fn info(title: &str, output: &str, focused: bool) -> WindowInfo {
        WindowInfo {
            title: title.into(),
            output: Some(output.into()),
            focused,
            ..Default::default()
        }
    }

    #[test]
    fn unfocused_output_follows_its_window_until_closed() {
        let mut m = model();
        pending(&mut m, A).activated = false;
        m.commit(A);
        pending(&mut m, B).activated = true;
        m.commit(B);
        assert_eq!(m.updates(), vec![info("b", "DP-2", true)]);

        pending(&mut m, A).title = "renamed".into();
        m.commit(A);
        assert_eq!(m.updates(), vec![info("renamed", "DP-1", false)]);

        m.toplevels.remove(&A);
        assert_eq!(m.updates(), vec![info("", "DP-1", false)]);
    }

    #[test]
    fn focused_window_waits_for_its_output() {
        let mut m = model();
        pending(&mut m, A).activated = false;
        m.commit(A);
        let b = pending(&mut m, B);
        b.activated = true;
        b.outputs.clear();
        m.commit(B);
        assert!(m.updates().is_empty());

        pending(&mut m, A).title = "renamed".into();
        m.commit(A);
        assert_eq!(m.updates(), vec![info("renamed", "DP-1", false)]);

        pending(&mut m, B).outputs = vec![2];
        m.commit(B);
        assert_eq!(m.updates(), vec![info("b", "DP-2", true)]);
    }

    #[test]
    fn removed_output_is_cleared_and_republished_on_return() {
        let mut m = model();
        assert_eq!(m.remove_output(2), None);
        assert_eq!(m.remove_output(1), Some(info("", "DP-1", true)));
        assert_eq!(m.remove_output(1), None);
        assert!(m.updates().is_empty());

        m.outputs.insert(3, "DP-1".into());
        pending(&mut m, A).outputs = vec![3];
        m.commit(A);
        assert_eq!(m.updates(), vec![info("a", "DP-1", true)]);
    }

    #[test]
    fn uncommitted_changes_are_not_published() {
        let mut m = model();
        pending(&mut m, A).title = "renamed".into();
        m.commit(B);
        assert!(m.updates().is_empty());

        m.commit(A);
        assert_eq!(m.updates(), vec![info("renamed", "DP-1", true)]);
    }

    #[test]
    fn moving_focused_window_clears_old_output() {
        let mut m = model();
        pending(&mut m, A).outputs = vec![2];
        m.commit(A);
        assert_eq!(
            m.updates(),
            vec![info("a", "DP-2", true), info("", "DP-1", false)]
        );
    }
}
