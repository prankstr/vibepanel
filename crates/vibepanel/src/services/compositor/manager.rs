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
use std::sync::Arc;

use gtk4::glib;
use tracing::{debug, info};
use vibepanel_core::config::AdvancedConfig;

use super::{
    BackendKind, CompositorBackend, WindowCallback, WindowInfo, WorkspaceCallback, WorkspaceMeta,
    WorkspaceSnapshot, factory,
};
use crate::services::callbacks::{CallbackId, Callbacks};

// Thread-local singleton storage for CompositorManager
thread_local! {
    static COMPOSITOR_MANAGER: RefCell<Option<Rc<CompositorManager>>> = const { RefCell::new(None) };
}

/// GTK main-thread singleton that multiplexes backend callbacks to listeners.
pub struct CompositorManager {
    backend: RefCell<Option<Box<dyn CompositorBackend>>>,
    workspace_callbacks: Callbacks<WorkspaceSnapshot>,
    window_callbacks: Callbacks<WindowInfo>,
    last_workspace_snapshot: RefCell<Option<WorkspaceSnapshot>>,
    last_window_info: RefCell<Option<WindowInfo>>,
    started: RefCell<bool>,
}

impl CompositorManager {
    fn new(advanced_config: &AdvancedConfig) -> Rc<Self> {
        let manager = Rc::new(Self {
            backend: RefCell::new(None),
            workspace_callbacks: Callbacks::new(),
            window_callbacks: Callbacks::new(),
            last_workspace_snapshot: RefCell::new(None),
            last_window_info: RefCell::new(None),
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

    /// Get the list of workspaces from the backend.
    pub fn list_workspaces(&self) -> Vec<WorkspaceMeta> {
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
    pub fn get_focused_window(&self) -> Option<WindowInfo> {
        self.last_window_info.borrow().clone()
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

    /// Get the backend name (e.g., "Hyprland", "Niri", "MangoWC").
    pub fn backend_name(&self) -> &'static str {
        if let Some(ref backend) = *self.backend.borrow() {
            backend.name()
        } else {
            "unknown"
        }
    }

    /// Handle a workspace update from the backend.
    /// Called via glib::idle_add_once from the backend thread.
    pub(crate) fn handle_workspace_update(&self, snapshot: WorkspaceSnapshot) {
        // Store for new listeners
        *self.last_workspace_snapshot.borrow_mut() = Some(snapshot.clone());

        // Dispatch to all registered callbacks
        self.workspace_callbacks.notify(&snapshot);
    }

    /// Handle a window update from the backend.
    /// Called via glib::idle_add_once from the backend thread.
    pub(crate) fn handle_window_update(&self, window_info: WindowInfo) {
        // Store for new listeners
        *self.last_window_info.borrow_mut() = Some(window_info.clone());

        // Dispatch to all registered callbacks
        self.window_callbacks.notify(&window_info);
    }

    /// Initialize the backend.
    fn init_backend(this: &Rc<Self>, advanced_config: &AdvancedConfig) {
        // Parse backend kind from config
        let backend_kind = BackendKind::from_str(&advanced_config.compositor);

        // Backends no longer filter by outputs - that's now handled at the widget level
        let backend = factory::create_backend(backend_kind, None);

        info!(
            "CompositorManager using backend: {} (config: {})",
            backend.name(),
            advanced_config.compositor,
        );

        // Create thread-safe callbacks that use idle_add_once to schedule on main loop
        let on_workspace_update: WorkspaceCallback = Arc::new(move |snapshot| {
            glib::idle_add_once(move || {
                CompositorManager::global().handle_workspace_update(snapshot);
            });
        });

        let on_window_update: WindowCallback = Arc::new(move |window_info| {
            glib::idle_add_once(move || {
                CompositorManager::global().handle_window_update(window_info);
            });
        });

        // Start the backend first (which fetches initial state internally)
        backend.start(on_workspace_update, on_window_update);

        // Now store initial state - backend has fetched it during start()
        *this.last_workspace_snapshot.borrow_mut() = Some(backend.get_workspace_snapshot());
        *this.last_window_info.borrow_mut() = backend.get_focused_window();

        // Store backend
        *this.backend.borrow_mut() = Some(backend);
        *this.started.borrow_mut() = true;

        debug!("CompositorManager initialized");
    }
}

impl Drop for CompositorManager {
    fn drop(&mut self) {
        if let Some(ref backend) = *self.backend.borrow() {
            backend.stop();
        }
        debug!("CompositorManager dropped");
    }
}
