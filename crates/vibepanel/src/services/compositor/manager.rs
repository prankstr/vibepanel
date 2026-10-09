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
//! - Alongside a native backend, an optional ext-workspace overlay whose state
//!   is merged over the backend's workspace state before it is published (see
//!   `ext_workspace::overlay`). All consumers see the merged state.
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

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use gtk4::glib;
use tracing::{debug, info};
use vibepanel_core::config::AdvancedConfig;

use super::ext_workspace::overlay::{ExtOverlayState, ExtWorkspaceOverlay};
use super::ext_workspace::{ExtWorkspaceClient, ModelCallback};
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
    /// Effective (merged, if an overlay is active) workspace snapshot.
    last_workspace_snapshot: RefCell<Option<WorkspaceSnapshot>>,
    ext_overlay: RefCell<Option<ExtOverlay>>,
    last_window_info: RefCell<Option<WindowInfo>>,
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
            ext_overlay: RefCell::new(None),
            last_window_info: RefCell::new(None),
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
                ext_overlay: RefCell::new(None),
                last_window_info: RefCell::new(None),
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

        // Immediately send current state if available
        if let Some(ref info) = *self.last_window_info.borrow() {
            self.window_callbacks.notify_single(id, info);
        }

        id
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
    /// Delegates to the backend directly — `last_window_info` can be stale
    /// on backends that emit window callbacks for multiple outputs.
    pub fn get_focused_window(&self) -> Option<WindowInfo> {
        if let Some(ref backend) = *self.backend.borrow() {
            backend.get_focused_window()
        } else {
            None
        }
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
        if let Some(ref backend) = *self.backend.borrow() {
            backend.list_windows()
        } else {
            Vec::new()
        }
    }

    /// Focus a specific window by its ID.
    pub fn focus_window(&self, window_id: u64) {
        if let Some(ref backend) = *self.backend.borrow() {
            backend.focus_window(window_id);
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
                        // Metadata is read now, together with the snapshot, so
                        // both describe the same moment.
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

        // Effective metadata is already installed (list_workspaces reads the
        // overlay state), so listeners re-reading it see a matching pair.
        *self.last_workspace_snapshot.borrow_mut() = Some(snapshot.clone());
        self.workspace_callbacks.notify(&snapshot);
    }

    /// Start merging ext-workspace state over a native backend, if offered.
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
        // Store for new listeners
        *self.last_window_info.borrow_mut() = Some(window_info.clone());

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

        // Window list events: no coalescing needed — window changes are
        // relatively infrequent and each event should update the UI.
        let on_window_list_update: super::WindowListCallback =
            Arc::new(move |window_list_snapshot| {
                glib::idle_add_once(move || {
                    CompositorManager::global().handle_window_list_update(window_list_snapshot);
                });
            });

        // Register callbacks before start() so the backend
        // can fire them during initialization.
        backend.set_keyboard_layout_callback(on_keyboard_layout_update);
        backend.set_window_layout_callback(on_window_layout_update);
        backend.set_window_list_callback(on_window_list_update);

        // Start the backend first (which fetches initial state internally)
        backend.start(on_workspace_update, on_window_update);

        // Now store initial state - backend has fetched it during start()
        *this.last_workspace_snapshot.borrow_mut() = Some(backend.get_workspace_snapshot());
        *this.last_window_info.borrow_mut() = backend.get_focused_window();
        *this.last_keyboard_layout.borrow_mut() = backend.get_keyboard_layout();
        *this.last_window_layouts.borrow_mut() = backend.get_window_layouts();

        // Store backend
        *this.backend.borrow_mut() = Some(backend);
        *this.started.borrow_mut() = true;

        // The ext-workspace backend already reports that state as primary data.
        if resolved_kind != BackendKind::ExtWorkspace {
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

impl Drop for CompositorManager {
    fn drop(&mut self) {
        if let Some(overlay) = self.ext_overlay.borrow_mut().take() {
            overlay.client.stop();
        }
        if let Some(ref backend) = *self.backend.borrow() {
            backend.stop();
        }
        debug!("CompositorManager dropped");
    }
}
