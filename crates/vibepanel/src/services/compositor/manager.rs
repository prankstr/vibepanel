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
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use gtk4::glib;
use tracing::{debug, info};
use vibepanel_core::config::AdvancedConfig;

use super::{
    BackendKind, CompositorBackend, KeyboardLayoutCallback, KeyboardLayoutInfo, WindowCallback,
    WindowInfo, WindowLayoutCallback, WindowLayoutSnapshot, WorkspaceCallback, WorkspaceMeta,
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
    keyboard_layout_callbacks: Callbacks<KeyboardLayoutInfo>,
    window_layout_callbacks: Callbacks<WindowLayoutSnapshot>,
    window_list_callbacks: Callbacks<super::WindowListSnapshot>,
    last_workspace_snapshot: RefCell<Option<WorkspaceSnapshot>>,
    /// Latest window update per output (`None` key for unscoped updates).
    last_window_infos: RefCell<HashMap<Option<String>, WindowInfo>>,
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
    ///
    /// Delegates to the backend directly, since window callbacks are emitted
    /// per output rather than only for the focused window.
    pub fn get_focused_window(&self) -> Option<WindowInfo> {
        if let Some(ref backend) = *self.backend.borrow() {
            backend.get_focused_window()
        } else {
            None
        }
    }

    /// Get the connector name of the focused output, if the backend tracks it.
    pub fn get_focused_output(&self) -> Option<String> {
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

    /// Handle a workspace update from the backend.
    /// Called via glib::idle_add_once from the backend thread.
    pub(crate) fn handle_workspace_update(&self, snapshot: WorkspaceSnapshot) {
        // Store for new listeners
        *self.last_workspace_snapshot.borrow_mut() = Some(snapshot.clone());

        // Drop windows of removed outputs before subscribers can recreate bars,
        // so a reconnected output doesn't replay a stale title.
        prune_removed_outputs(&mut self.last_window_infos.borrow_mut(), &snapshot);

        // Dispatch to all registered callbacks
        self.workspace_callbacks.notify(&snapshot);
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
        let backend = factory::create_backend(backend_kind);

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
        let pending_ws_snapshot: Arc<Mutex<Option<WorkspaceSnapshot>>> = Arc::new(Mutex::new(None));
        let ws_idle_scheduled: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

        let on_workspace_update: WorkspaceCallback = Arc::new({
            let pending = Arc::clone(&pending_ws_snapshot);
            let scheduled = Arc::clone(&ws_idle_scheduled);
            move |snapshot| {
                // Always store the latest snapshot (overwrites any pending one)
                *pending.lock().unwrap() = Some(snapshot);

                // Only schedule one idle per batch of rapid-fire events
                if !scheduled.swap(true, Ordering::SeqCst) {
                    let pending = Arc::clone(&pending);
                    let scheduled = Arc::clone(&scheduled);
                    glib::idle_add_once(move || {
                        scheduled.store(false, Ordering::SeqCst);
                        if let Some(snapshot) = pending.lock().unwrap().take() {
                            CompositorManager::global().handle_workspace_update(snapshot);
                        }
                    });
                }
            }
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

        backend.start(on_workspace_update, on_window_update);

        // Store whatever state the backend has so far. Most backends load it
        // on their event thread, so it may still be empty; the callbacks
        // above replace it once it arrives.
        *this.last_workspace_snapshot.borrow_mut() = Some(backend.get_workspace_snapshot());
        *this.last_keyboard_layout.borrow_mut() = backend.get_keyboard_layout();
        *this.last_window_layouts.borrow_mut() = backend.get_window_layouts();

        // Store backend
        *this.backend.borrow_mut() = Some(backend);
        *this.started.borrow_mut() = true;

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
