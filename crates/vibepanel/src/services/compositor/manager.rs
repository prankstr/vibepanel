//! CompositorManager - shared backend singleton for workspace and window title services.
//!
//! This module provides a centralized compositor backend instance that can be shared
//! across multiple services (WorkspaceService, WindowTitleService). This avoids the
//! problem of creating multiple backend instances that would duplicate IPC connections
//! and monitoring threads.
//!
//! # Architecture
//!
//! The CompositorManager receives updates from the backend thread via glib::idle_add_once(),
//! which schedules callbacks directly on the GTK main loop without polling. It maintains:
//! - A single backend instance
//! - Registered callbacks for workspace and window updates
//! - Alongside the Hyprland backend, an optional ext-workspace overlay whose state
//!   is merged over the backend's workspace state before it is published (see
//!   `ext_workspace::overlay`). All consumers see the merged state.
//! - A wlr-foreign-toplevel client for what the backend lacks: window titles
//!   (see `CompositorBackend::provides_window_titles`), and the taskbar's
//!   window list with focus and minimize (see `provides_window_list`). The
//!   list is only started once something subscribes to it.
//!
//! # Usage
//!
//! ```rust,ignore
//! let manager = CompositorManager::global();
//!
//! // Register for workspace updates
//! manager.register_workspace_callback(|snapshot| {
//!     // Handle workspace state change
//! });
//! ```

use std::cell::{Cell, Ref, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use gtk4::glib;
use tracing::{debug, info};
use vibepanel_core::config::AdvancedConfig;

use super::ext_workspace::overlay::{ExtOverlayState, ExtWorkspaceOverlay};
use super::ext_workspace::{ExtWorkspaceClient, ModelCallback};
use super::foreign_toplevel::ForeignToplevelClient;
use super::{
    BackendKind, CompositorBackend, KeyboardLayoutCallback, KeyboardLayoutInfo, WindowCallback,
    WindowInfo, WindowLayoutCallback, WindowLayoutSnapshot, WorkspaceCallback, WorkspaceMeta,
    WorkspaceSnapshot, factory,
};
use crate::services::callbacks::{CallbackId, Callbacks};

/// Latest unprocessed workspace input from each source.
///
/// One slot per source, so a burst of updates from one source can never drop
/// the latest update from the other before the idle callback runs.
#[derive(Default)]
struct PendingWorkspace {
    native: Option<WorkspaceSnapshot>,
    ext: Option<ExtWorkspaceOverlay>,
}

/// Coalesces workspace inputs from any thread into one main-loop idle.
#[derive(Clone, Default)]
struct WorkspaceInbox {
    pending: Arc<Mutex<PendingWorkspace>>,
    scheduled: Arc<AtomicBool>,
}

impl WorkspaceInbox {
    fn push(&self, update: impl FnOnce(&mut PendingWorkspace)) {
        update(&mut self.pending.lock().unwrap());
        if !self.scheduled.swap(true, Ordering::SeqCst) {
            let inbox = self.clone();
            glib::idle_add_once(move || {
                inbox.scheduled.store(false, Ordering::SeqCst);
                let pending = std::mem::take(&mut *inbox.pending.lock().unwrap());
                CompositorManager::global().handle_workspace_inputs(pending.native, pending.ext);
            });
        }
    }
}

/// ext-workspace client plus the merge state it feeds.
struct ExtOverlay {
    client: ExtWorkspaceClient,
    state: ExtOverlayState,
}

/// foreign-toplevel client plus what it provides in place of the backend.
struct ToplevelSource {
    client: ForeignToplevelClient,
    titles: bool,
    list: bool,
}

/// Forwards window list updates from any thread to the main loop.
fn window_list_callback() -> super::WindowListCallback {
    // No coalescing needed: window changes are relatively infrequent and
    // each event should update the UI.
    Arc::new(move |snapshot| {
        glib::idle_add_once(move || {
            CompositorManager::global().handle_window_list_update(snapshot);
        });
    })
}

// Thread-local singleton storage for CompositorManager
thread_local! {
    static COMPOSITOR_MANAGER: RefCell<Option<Rc<CompositorManager>>> = const { RefCell::new(None) };
}

/// GTK main-thread singleton that multiplexes backend callbacks to listeners.
pub struct CompositorManager {
    backend: RefCell<Option<Box<dyn CompositorBackend>>>,
    workspace_callbacks: Callbacks<WorkspaceSnapshot>,
    window_callbacks: Callbacks<WindowInfo>,
    keyboard_layout_callbacks: Callbacks<KeyboardLayoutInfo>,
    window_layout_callbacks: Callbacks<WindowLayoutSnapshot>,
    window_list_callbacks: Callbacks<super::WindowListSnapshot>,
    last_workspace_snapshot: RefCell<Option<WorkspaceSnapshot>>,
    /// Latest window update per output (`None` key for unscoped updates).
    last_window_infos: RefCell<HashMap<Option<String>, WindowInfo>>,
    ext_overlay: RefCell<Option<ExtOverlay>>,
    /// wlr-foreign-toplevel, for what the backend lacks.
    toplevels: RefCell<Option<ToplevelSource>>,
    /// Whether the first window list subscriber has been seen.
    window_list_requested: Cell<bool>,
    last_keyboard_layout: RefCell<Option<KeyboardLayoutInfo>>,
    last_window_layouts: RefCell<Option<WindowLayoutSnapshot>>,
    last_window_list: RefCell<Option<super::WindowListSnapshot>>,
    started: RefCell<bool>,
}

impl CompositorManager {
    pub fn visibility_reader(&self) -> Option<super::visibility::VisibilityReader> {
        self.backend.borrow().as_ref()?.visibility_reader()
    }
    fn new(advanced_config: &AdvancedConfig) -> Rc<Self> {
        let manager = Rc::new(Self {
            backend: RefCell::new(None),
            workspace_callbacks: Callbacks::new(),
            window_callbacks: Callbacks::new(),
            keyboard_layout_callbacks: Callbacks::new(),
            window_layout_callbacks: Callbacks::new(),
            window_list_callbacks: Callbacks::new(),
            last_workspace_snapshot: RefCell::new(None),
            last_window_infos: RefCell::new(HashMap::new()),
            ext_overlay: RefCell::new(None),
            toplevels: RefCell::new(None),
            window_list_requested: Cell::new(false),
            last_keyboard_layout: RefCell::new(None),
            last_window_layouts: RefCell::new(None),
            last_window_list: RefCell::new(None),
            started: RefCell::new(false),
        });

        // Initialize backend with config
        Self::init_backend(&manager, advanced_config);

        manager
    }

    /// Initialize the global CompositorManager singleton with advanced configuration.
    ///
    /// This must be called once from the GTK main thread before any calls to `global()`.
    /// Typically called during application startup after ConfigManager is initialized.
    pub fn init_global(advanced_config: &AdvancedConfig) {
        COMPOSITOR_MANAGER.with(|cell| {
            let mut opt = cell.borrow_mut();
            if opt.is_some() {
                debug!("CompositorManager already initialized, skipping re-init");
                return;
            }
            *opt = Some(CompositorManager::new(advanced_config));
        });
    }

    /// Get the global CompositorManager singleton.
    ///
    /// This must be called from the GTK main thread.
    /// Panics if `init_global()` has not been called.
    pub fn global() -> Rc<Self> {
        COMPOSITOR_MANAGER.with(|cell| {
            cell.borrow()
                .clone()
                .expect("CompositorManager::global() called before init_global()")
        })
    }

    #[cfg(test)]
    pub(crate) fn replace_global_for_test(snapshot: WorkspaceSnapshot) {
        COMPOSITOR_MANAGER.with(|cell| {
            *cell.borrow_mut() = Some(Rc::new(Self {
                backend: RefCell::new(None),
                workspace_callbacks: Callbacks::new(),
                window_callbacks: Callbacks::new(),
                keyboard_layout_callbacks: Callbacks::new(),
                window_layout_callbacks: Callbacks::new(),
                window_list_callbacks: Callbacks::new(),
                last_workspace_snapshot: RefCell::new(Some(snapshot)),
                last_window_infos: RefCell::new(HashMap::new()),
                ext_overlay: RefCell::new(None),
                toplevels: RefCell::new(None),
                window_list_requested: Cell::new(false),
                last_keyboard_layout: RefCell::new(None),
                last_window_layouts: RefCell::new(None),
                last_window_list: RefCell::new(None),
                started: RefCell::new(true),
            }));
        });
    }

    /// Register a callback for workspace state changes.
    ///
    /// The callback will be immediately invoked with the current state if available.
    /// Returns a `CallbackId` that can be used to unregister the callback.
    pub fn register_workspace_callback<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&WorkspaceSnapshot) + 'static,
    {
        let id = self.workspace_callbacks.register(callback);

        // Immediately send current state if available
        if let Some(ref snapshot) = *self.last_workspace_snapshot.borrow() {
            self.workspace_callbacks.notify_single(id, snapshot);
        }

        id
    }

    /// Unregister a workspace callback by its ID.
    pub fn unregister_workspace_callback(&self, id: CallbackId) -> bool {
        self.workspace_callbacks.unregister(id)
    }

    /// Register a callback for window focus changes.
    ///
    /// The callback will be immediately invoked with the current state if available.
    /// Returns a `CallbackId` that can be used to unregister the callback.
    pub fn register_window_callback<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&WindowInfo) + 'static,
    {
        let id = self.window_callbacks.register(callback);

        // Immediately replay the latest update for every output, like the
        // other register_* methods. WindowTitleService registers before it has
        // subscribers and replays to them itself via `window_updates()`.
        for info in self.window_updates() {
            self.window_callbacks.notify_single(id, &info);
        }

        id
    }

    /// Latest window update for every output, in replay order.
    pub(crate) fn window_updates(&self) -> Vec<WindowInfo> {
        window_updates_for_replay(&self.last_window_infos.borrow())
    }

    /// Get the list of workspaces (merged, if an ext-workspace overlay is active).
    pub fn list_workspaces(&self) -> Vec<WorkspaceMeta> {
        if let Some(metas) = self
            .ext_overlay
            .borrow()
            .as_ref()
            .and_then(|overlay| overlay.state.metas())
        {
            return metas.to_vec();
        }
        if let Some(ref backend) = *self.backend.borrow() {
            backend.list_workspaces()
        } else {
            Vec::new()
        }
    }

    /// Get the current workspace snapshot.
    pub fn get_workspace_snapshot(&self) -> WorkspaceSnapshot {
        if let Some(ref snapshot) = *self.last_workspace_snapshot.borrow() {
            snapshot.clone()
        } else if let Some(ref backend) = *self.backend.borrow() {
            backend.get_workspace_snapshot()
        } else {
            WorkspaceSnapshot::default()
        }
    }

    /// Get the current focused window info.
    ///
    /// Delegates to the backend directly, since window callbacks are emitted
    /// per output rather than only for the focused window. With titles from
    /// foreign-toplevel, it is the focused update.
    pub fn get_focused_window(&self) -> Option<WindowInfo> {
        if self.toplevel_titles() {
            return focused_update(&self.last_window_infos.borrow())
                .filter(|w| !w.title.is_empty() || !w.app_id.is_empty());
        }
        if let Some(ref backend) = *self.backend.borrow() {
            backend.get_focused_window()
        } else {
            None
        }
    }

    /// Get the connector name of the focused output, if the backend tracks it.
    ///
    /// With titles from foreign-toplevel, it is the output that last had a
    /// focused window: a guess, as the protocol has no output focus.
    pub fn get_focused_output(&self) -> Option<String> {
        if self.toplevel_titles() {
            return focused_update(&self.last_window_infos.borrow())?.output;
        }
        self.backend
            .borrow()
            .as_ref()
            .and_then(|backend| backend.get_focused_output())
    }

    /// Switch to a workspace.
    pub fn switch_workspace(&self, workspace_id: i32) {
        if let Some(ref backend) = *self.backend.borrow() {
            backend.switch_workspace(workspace_id);
        }
    }

    /// Request the compositor to quit/exit.
    ///
    /// Used for logout functionality. Sends a quit command to the compositor
    /// via its native IPC.
    pub fn quit_compositor(&self) {
        if let Some(ref backend) = *self.backend.borrow() {
            backend.quit_compositor();
        }
    }

    /// Register a callback for keyboard layout changes.
    ///
    /// The callback will be immediately invoked with the current state if available.
    /// Returns a `CallbackId` that can be used to unregister the callback.
    pub fn register_keyboard_layout_callback<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&KeyboardLayoutInfo) + 'static,
    {
        let id = self.keyboard_layout_callbacks.register(callback);

        // Immediately send current state if available
        if let Some(ref info) = *self.last_keyboard_layout.borrow() {
            self.keyboard_layout_callbacks.notify_single(id, info);
        }

        id
    }

    /// Unregister a keyboard layout callback by its ID.
    pub fn unregister_keyboard_layout_callback(&self, id: CallbackId) -> bool {
        self.keyboard_layout_callbacks.unregister(id)
    }

    /// Register a callback for window list changes.
    ///
    /// The callback will be immediately invoked with the current state if available.
    /// Returns a `CallbackId` that can be used to unregister the callback.
    pub fn register_window_list_callback<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&super::WindowListSnapshot) + 'static,
    {
        let id = self.window_list_callbacks.register(callback);
        self.start_toplevel_list();

        // Immediately send current state if available
        if let Some(ref snapshot) = *self.last_window_list.borrow() {
            self.window_list_callbacks.notify_single(id, snapshot);
        }

        id
    }

    /// Unregister a window list callback by its ID.
    pub fn unregister_window_list_callback(&self, id: CallbackId) -> bool {
        self.window_list_callbacks.unregister(id)
    }

    /// Switch to the next keyboard layout.
    pub fn switch_keyboard_layout_next(&self) {
        if let Some(ref backend) = *self.backend.borrow() {
            backend.switch_keyboard_layout_next();
        }
    }

    /// Register a callback for window-layout changes.
    pub fn register_window_layout_callback<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&WindowLayoutSnapshot) + 'static,
    {
        let id = self.window_layout_callbacks.register(callback);
        if let Some(ref snapshot) = *self.last_window_layouts.borrow() {
            self.window_layout_callbacks.notify_single(id, snapshot);
        }
        id
    }

    /// Unregister a window-layout callback by its ID.
    pub fn unregister_window_layout_callback(&self, id: CallbackId) -> bool {
        self.window_layout_callbacks.unregister(id)
    }

    /// Set the active tag's window layout on an output.
    pub fn set_window_layout(&self, output: &str, layout_name: &str) {
        if let Some(ref backend) = *self.backend.borrow() {
            backend.set_window_layout(output, layout_name);
        }
    }

    /// Ask the active compositor to refresh pointer focus, if supported.
    pub fn refresh_pointer_focus(&self) {
        if let Some(ref backend) = *self.backend.borrow() {
            backend.refresh_pointer_focus();
        }
    }

    /// Get the list of all windows.
    pub fn list_windows(&self) -> Vec<super::Window> {
        if self.toplevel_list() {
            return self
                .last_window_list
                .borrow()
                .as_ref()
                .map(|snapshot| snapshot.windows.clone())
                .unwrap_or_default();
        }
        if let Some(ref backend) = *self.backend.borrow() {
            backend.list_windows()
        } else {
            Vec::new()
        }
    }

    /// Whether listed windows carry their workspaces. Only native window
    /// lists do; the wlr-foreign-toplevel list has no workspaces.
    pub fn window_list_has_workspaces(&self) -> bool {
        self.backend
            .borrow()
            .as_ref()
            .is_some_and(|backend| backend.provides_window_list())
    }

    /// Focus a specific window by its ID.
    pub fn focus_window(&self, window_id: u64) {
        if let Some(client) = self.list_client() {
            // Whether activating a minimized window also restores it is up
            // to the compositor, so restore it explicitly.
            if self.listed_window(window_id, |w| w.is_minimized) {
                client.restore(window_id);
            } else {
                client.activate(window_id);
            }
        } else if let Some(ref backend) = *self.backend.borrow() {
            backend.focus_window(window_id);
        }
    }

    /// Minimize the focused window, or focus it otherwise (`focus_window`
    /// restores a minimized one). Without minimize support this only focuses.
    pub fn activate_or_minimize(&self, window_id: u64) {
        let minimize = self.supports_minimize()
            && self.listed_window(window_id, |w| w.is_focused && !w.is_minimized);
        if minimize {
            self.minimize_window(window_id);
        } else {
            self.focus_window(window_id);
        }
    }

    /// Whether minimize requests can be sent (only via foreign-toplevel).
    /// Compositors may ignore them.
    fn supports_minimize(&self) -> bool {
        self.toplevel_list()
    }

    /// Minimize a window. `focus_window` restores it.
    fn minimize_window(&self, window_id: u64) {
        if let Some(client) = self.list_client() {
            client.minimize(window_id);
        }
    }

    /// Whether the window is listed and matches `pred`.
    fn listed_window(&self, window_id: u64, pred: impl Fn(&super::Window) -> bool) -> bool {
        self.last_window_list
            .borrow()
            .as_ref()
            .is_some_and(|snapshot| {
                snapshot
                    .windows
                    .iter()
                    .any(|w| w.id == window_id && pred(w))
            })
    }

    /// Whether window titles come from foreign-toplevel.
    fn toplevel_titles(&self) -> bool {
        self.toplevels.borrow().as_ref().is_some_and(|s| s.titles)
    }

    /// The foreign-toplevel client, if it provides the window list.
    fn list_client(&self) -> Option<Ref<'_, ForeignToplevelClient>> {
        Ref::filter_map(self.toplevels.borrow(), |toplevels| {
            toplevels.as_ref().filter(|s| s.list).map(|s| &s.client)
        })
        .ok()
    }

    /// Whether the window list comes from foreign-toplevel.
    fn toplevel_list(&self) -> bool {
        self.list_client().is_some()
    }

    /// On the first window list subscriber, get the list from foreign-toplevel
    /// if the backend has none. A running title client also takes on the
    /// list, so there is at most one connection.
    fn start_toplevel_list(&self) {
        let Some((has_list, has_titles)) = self.backend.borrow().as_ref().map(|backend| {
            (
                backend.provides_window_list(),
                backend.provides_window_titles(),
            )
        }) else {
            return;
        };
        if self.window_list_requested.replace(true) || has_list {
            return;
        }
        let mut toplevels = self.toplevels.borrow_mut();
        match toplevels.as_mut() {
            Some(source) => {
                source.client.enable_list(window_list_callback());
                source.list = true;
            }
            // The title client already failed to start, so the protocol is
            // missing; don't connect again.
            None if !has_titles => return,
            None => {
                *toplevels = ForeignToplevelClient::start(None, Some(window_list_callback())).map(
                    |client| ToplevelSource {
                        client,
                        titles: false,
                        list: true,
                    },
                );
            }
        }
        if toplevels.is_some() {
            info!("Window list from wlr-foreign-toplevel");
        }
    }

    /// Get the backend name (e.g., "Hyprland", "Niri", "MangoWC").
    pub fn backend_name(&self) -> &'static str {
        if let Some(ref backend) = *self.backend.borrow() {
            backend.name()
        } else {
            "unknown"
        }
    }

    /// Handle coalesced workspace input from the backend and/or ext-workspace.
    /// Called on the main loop via `WorkspaceInbox`.
    fn handle_workspace_inputs(
        &self,
        native: Option<WorkspaceSnapshot>,
        ext: Option<ExtWorkspaceOverlay>,
    ) {
        let snapshot = {
            let mut overlay = self.ext_overlay.borrow_mut();
            match overlay.as_mut() {
                // Native backend alone: publish as-is.
                None => match native {
                    Some(snapshot) => snapshot,
                    None => return,
                },
                Some(overlay) => {
                    let mut changed = false;
                    if let Some(snapshot) = native {
                        // Read metadata as close to the snapshot as possible;
                        // see the coalescing note in `init_backend`.
                        let metas = self
                            .backend
                            .borrow()
                            .as_ref()
                            .map(|b| b.list_workspaces())
                            .unwrap_or_default();
                        overlay.state.set_native(metas, snapshot);
                        changed = true;
                    }
                    if let Some(ext) = ext {
                        changed |= overlay.state.set_overlay(ext);
                    }
                    match overlay.state.snapshot() {
                        Some(snapshot) if changed => snapshot.clone(),
                        _ => return,
                    }
                }
            }
        };

        // Drop windows of removed outputs before subscribers can recreate bars,
        // so a reconnected output doesn't replay a stale title.
        // Not with titles from foreign-toplevel: workspace snapshots may not
        // list every output, and the client clears removed outputs itself.
        if !self.toplevel_titles() {
            prune_removed_outputs(&mut self.last_window_infos.borrow_mut(), &snapshot);
        }

        // Dispatch to all registered callbacks
        // Effective metadata is already installed (list_workspaces reads the
        // overlay state), so listeners re-reading it see a matching pair.
        *self.last_workspace_snapshot.borrow_mut() = Some(snapshot.clone());
        self.workspace_callbacks.notify(&snapshot);
    }

    /// Start merging ext-workspace state over the backend, if offered.
    fn start_ext_overlay(&self, inbox: &WorkspaceInbox) {
        let inbox = inbox.clone();
        let on_model: ModelCallback = Arc::new(move |model| {
            let overlay = ExtWorkspaceOverlay::from_model(&model);
            inbox.push(|pending| pending.ext = Some(overlay));
        });
        let Some(client) = ExtWorkspaceClient::start(on_model) else {
            return;
        };

        let mut state = ExtOverlayState::default();
        if let Some(ref backend) = *self.backend.borrow() {
            state.set_native(backend.list_workspaces(), backend.get_workspace_snapshot());
        }
        info!("Merging ext-workspace state over the native backend");
        *self.ext_overlay.borrow_mut() = Some(ExtOverlay { client, state });
    }

    /// Handle a window update from the backend.
    /// Called via glib::idle_add_once from the backend thread.
    pub(crate) fn handle_window_update(&self, window_info: WindowInfo) {
        // Store for new listeners. Window updates aren't coalesced like
        // workspace snapshots, so one queued before its output was unplugged
        // can land after the prune. That's accepted on purpose: it can briefly
        // show a stale title, or on a reconnected output replay its old title
        // until it's focused or the next workspace update prunes it. Dropping
        // it would require every backend to publish an output in a snapshot
        // before any window update for it, and silently lose titles when one
        // doesn't.
        store_window_update(&mut self.last_window_infos.borrow_mut(), &window_info);

        // Dispatch to all registered callbacks
        self.window_callbacks.notify(&window_info);
    }

    /// Handle a keyboard layout update from the backend.
    /// Called via glib::idle_add_once from the backend thread.
    pub(crate) fn handle_keyboard_layout_update(&self, info: KeyboardLayoutInfo) {
        // Store for new listeners
        *self.last_keyboard_layout.borrow_mut() = Some(info.clone());

        // Dispatch to all registered callbacks
        self.keyboard_layout_callbacks.notify(&info);
    }

    pub(crate) fn handle_window_layout_update(&self, snapshot: WindowLayoutSnapshot) {
        *self.last_window_layouts.borrow_mut() = Some(snapshot.clone());
        self.window_layout_callbacks.notify(&snapshot);
    }

    /// Handle a window list update from the backend.
    /// Called via glib::idle_add_once from the backend thread.
    pub(crate) fn handle_window_list_update(&self, snapshot: super::WindowListSnapshot) {
        // Store for new listeners
        *self.last_window_list.borrow_mut() = Some(snapshot.clone());

        // Dispatch to all registered callbacks
        self.window_list_callbacks.notify(&snapshot);
    }

    /// Initialize the backend.
    fn init_backend(this: &Rc<Self>, advanced_config: &AdvancedConfig) {
        // Parse backend kind from config
        let backend_kind = BackendKind::from_str(&advanced_config.compositor);

        // Backends no longer filter by outputs - that's now handled at the widget level
        let (resolved_kind, backend) = factory::create_backend(backend_kind);

        info!(
            "CompositorManager using backend: {} (config: {})",
            backend.name(),
            advanced_config.compositor,
        );

        // Create thread-safe callbacks that use idle_add_once to schedule on main loop.
        //
        // Workspace events are coalesced: rapid-fire events from compositors like
        // Niri (which sends WorkspaceActivated then WorkspacesChanged as two separate
        // events when a workspace is destroyed) are merged into a single idle callback.
        // Without this, the first idle would see an inconsistent hybrid state: Event 1's
        // snapshot (old workspace list, new active) combined with Event 2's already-updated
        // workspace list read via list_workspaces(), causing the wrong indicator to be removed.
        let inbox = WorkspaceInbox::default();
        let on_workspace_update: WorkspaceCallback = Arc::new({
            let inbox = inbox.clone();
            move |snapshot| inbox.push(|pending| pending.native = Some(snapshot))
        });

        let on_window_update: WindowCallback = Arc::new(move |window_info| {
            glib::idle_add_once(move || {
                CompositorManager::global().handle_window_update(window_info);
            });
        });

        // Keyboard layout events: no coalescing needed — layout changes are
        // infrequent, user-initiated, and atomic (one event per switch).
        let on_keyboard_layout_update: KeyboardLayoutCallback =
            Arc::new(move |keyboard_layout_info| {
                glib::idle_add_once(move || {
                    CompositorManager::global().handle_keyboard_layout_update(keyboard_layout_info);
                });
            });

        let on_window_layout_update: WindowLayoutCallback = Arc::new(move |snapshot| {
            glib::idle_add_once(move || {
                CompositorManager::global().handle_window_layout_update(snapshot);
            });
        });

        // Register callbacks before start() so the backend
        // can fire them during initialization.
        backend.set_keyboard_layout_callback(on_keyboard_layout_update);
        backend.set_window_layout_callback(on_window_layout_update);
        backend.set_window_list_callback(window_list_callback());

        backend.start(on_workspace_update, on_window_update.clone());

        // The window list is started on its first subscriber instead; see
        // `start_toplevel_list`.
        if !backend.provides_window_titles() {
            *this.toplevels.borrow_mut() =
                ForeignToplevelClient::start(Some(on_window_update), None).map(|client| {
                    ToplevelSource {
                        client,
                        titles: true,
                        list: false,
                    }
                });
        }

        // Store whatever state the backend has so far. Most backends load it
        // on their event thread, so it may still be empty; the callbacks
        // above replace it once it arrives.
        *this.last_workspace_snapshot.borrow_mut() = Some(backend.get_workspace_snapshot());
        *this.last_keyboard_layout.borrow_mut() = backend.get_keyboard_layout();
        *this.last_window_layouts.borrow_mut() = backend.get_window_layouts();

        // Store backend
        *this.backend.borrow_mut() = Some(backend);
        *this.started.borrow_mut() = true;

        // Only merge ext-workspace state where it fills a gap in native IPC:
        // Hyprland reports a single active workspace per monitor, while
        // plugins like Hyprtagger expose several over ext-workspace. Other
        // native backends already report this, and may apply their own
        // `hidden` policy (Mango hides inactive tags) that would override
        // ours. The ext-workspace backend reports it as primary data.
        if resolved_kind == BackendKind::Hyprland {
            this.start_ext_overlay(&inbox);
            if let Some(snapshot) = this
                .ext_overlay
                .borrow()
                .as_ref()
                .and_then(|overlay| overlay.state.snapshot().cloned())
            {
                *this.last_workspace_snapshot.borrow_mut() = Some(snapshot);
            }
        }

        debug!("CompositorManager initialized");
    }
}

/// Record a window update for replay to late subscribers. An unscoped update
/// applies to every output and replaces all entries; otherwise at most one
/// entry stays marked as focused.
fn store_window_update(infos: &mut HashMap<Option<String>, WindowInfo>, info: &WindowInfo) {
    if info.output.is_none() {
        infos.clear();
    } else if info.focused {
        for other in infos.values_mut() {
            other.focused = false;
        }
    }
    infos.insert(info.output.clone(), info.clone());
}

/// The stored update for the focused output.
fn focused_update(infos: &HashMap<Option<String>, WindowInfo>) -> Option<WindowInfo> {
    infos.values().find(|info| info.focused).cloned()
}

/// Remove stored windows for outputs missing from the workspace snapshot.
/// Unscoped entries are kept. An empty `per_output` means no outputs are
/// connected, so every per-output entry is removed.
fn prune_removed_outputs(
    infos: &mut HashMap<Option<String>, WindowInfo>,
    snapshot: &WorkspaceSnapshot,
) {
    infos.retain(|output, _| {
        output
            .as_deref()
            .is_none_or(|o| snapshot.per_output.contains_key(o))
    });
}

/// Stored updates in replay order: the unscoped update first, so later
/// per-output updates take precedence.
fn window_updates_for_replay(infos: &HashMap<Option<String>, WindowInfo>) -> Vec<WindowInfo> {
    let mut updates: Vec<_> = infos.values().cloned().collect();
    updates.sort_by_key(|info| info.output.is_some());
    updates
}

impl Drop for CompositorManager {
    fn drop(&mut self) {
        if let Some(overlay) = self.ext_overlay.borrow_mut().take() {
            overlay.client.stop();
        }
        if let Some(source) = self.toplevels.borrow_mut().take() {
            source.client.stop();
        }
        if let Some(ref backend) = *self.backend.borrow() {
            backend.stop();
        }
        debug!("CompositorManager dropped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(output: &str, focused: bool) -> WindowInfo {
        WindowInfo {
            output: Some(output.to_string()),
            focused,
            ..Default::default()
        }
    }

    fn snapshot_with(outputs: &[&str]) -> WorkspaceSnapshot {
        let mut snapshot = WorkspaceSnapshot::default();
        for output in outputs {
            snapshot
                .per_output
                .insert(output.to_string(), Default::default());
        }
        snapshot
    }

    fn titled(output: &str, title: &str) -> WindowInfo {
        WindowInfo {
            title: title.to_string(),
            ..info(output, false)
        }
    }

    fn focused_outputs(infos: &HashMap<Option<String>, WindowInfo>) -> Vec<Option<String>> {
        infos
            .values()
            .filter(|i| i.focused)
            .map(|i| i.output.clone())
            .collect()
    }

    #[test]
    fn cache_keeps_one_focused_output_and_replays_unscoped_first() {
        let mut infos = HashMap::new();
        store_window_update(&mut infos, &info("DP-1", true));
        store_window_update(&mut infos, &info("DP-2", false));
        store_window_update(&mut infos, &info("DP-3", true));
        assert_eq!(focused_outputs(&infos), vec![Some("DP-3".to_string())]);

        // An unscoped update applies everywhere and replaces every entry.
        store_window_update(&mut infos, &WindowInfo::default());
        assert_eq!(infos.len(), 1);

        store_window_update(&mut infos, &info("DP-2", true));
        let replay = window_updates_for_replay(&infos);
        assert_eq!(replay[0].output, None);
        assert_eq!(replay[1].output.as_deref(), Some("DP-2"));
    }

    #[test]
    fn focused_update_is_the_stored_focused_output() {
        let mut infos = HashMap::new();
        assert!(focused_update(&infos).is_none());

        store_window_update(&mut infos, &titled("DP-1", "a"));
        store_window_update(&mut infos, &info("DP-2", true));
        assert_eq!(
            focused_update(&infos).and_then(|i| i.output).as_deref(),
            Some("DP-2")
        );
    }

    #[test]
    fn hotplug_prunes_removed_outputs() {
        let mut infos = HashMap::new();
        store_window_update(&mut infos, &WindowInfo::default());
        store_window_update(&mut infos, &info("DP-1", true));
        store_window_update(&mut infos, &titled("DP-2", "X"));

        // DP-2 is unplugged.
        prune_removed_outputs(&mut infos, &snapshot_with(&["DP-1"]));
        assert!(!infos.contains_key(&Some("DP-2".to_string())));
        assert!(infos.contains_key(&None));
        assert_eq!(focused_outputs(&infos), vec![Some("DP-1".to_string())]);

        // Every output disconnects (e.g. undocking with the lid closed).
        prune_removed_outputs(&mut infos, &WorkspaceSnapshot::default());
        assert_eq!(infos.len(), 1);

        // Reconnecting doesn't replay a title from before the unplug.
        prune_removed_outputs(&mut infos, &snapshot_with(&["DP-1", "DP-2"]));
        assert!(
            window_updates_for_replay(&infos)
                .iter()
                .all(|i| i.output.is_none())
        );
    }
}
