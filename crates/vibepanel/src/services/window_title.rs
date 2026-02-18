//! WindowTitleService - shared, event-driven window title state via CompositorManager.
//!
//! This provides a GTK-friendly API for focused window information:
//! - Uses the shared CompositorManager singleton
//! - Provides current window info access
//! - Supports callback registration for reactive updates

use std::cell::RefCell;
use std::rc::Rc;

use tracing::debug;

use super::callbacks::{CallbackId, Callbacks};
use super::compositor::{CompositorManager, WindowInfo};

/// Snapshot of window title service state for callbacks.
#[derive(Debug, Clone, Default)]
pub struct WindowTitleSnapshot {
    /// Current window title.
    pub title: String,
    /// Current application ID.
    pub app_id: String,
    /// Output/monitor name (if available).
    pub output: Option<String>,
}

impl From<WindowInfo> for WindowTitleSnapshot {
    fn from(info: WindowInfo) -> Self {
        Self {
            title: info.title,
            app_id: info.app_id,
            output: info.output,
        }
    }
}

impl From<&WindowInfo> for WindowTitleSnapshot {
    fn from(info: &WindowInfo) -> Self {
        Self {
            title: info.title.clone(),
            app_id: info.app_id.clone(),
            output: info.output.clone(),
        }
    }
}

/// Shared, process-wide window title service.
///
/// Provides reactive window title state with GTK main loop integration.
/// Widgets should call `connect()` to receive updates when the focused window changes.
pub struct WindowTitleService {
    /// Current window info.
    current: RefCell<WindowTitleSnapshot>,
    /// Registered callbacks.
    callbacks: Callbacks<WindowTitleSnapshot>,
    /// Whether the service has received at least one update.
    ready: RefCell<bool>,
}

impl WindowTitleService {
    fn new() -> Rc<Self> {
        // Get the shared compositor manager
        let manager = CompositorManager::global();

        // Get initial state from manager
        let initial: WindowTitleSnapshot = manager
            .get_focused_window()
            .map(|info| (&info).into())
            .unwrap_or_default();

        let service = Rc::new(Self {
            current: RefCell::new(initial),
            callbacks: Callbacks::new(),
            ready: RefCell::new(true), // Ready immediately since manager handles startup
        });

        // Register with compositor manager
        Self::register_with_manager(&service, &manager);

        debug!("WindowTitleService initialized (using CompositorManager)");
        service
    }

    /// Get the global WindowTitleService singleton.
    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<WindowTitleService> = WindowTitleService::new();
        }

        INSTANCE.with(|s| s.clone())
    }

    /// Register a callback to be invoked when window title changes.
    /// The callback is always executed on the GLib main loop.
    pub fn connect<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&WindowTitleSnapshot) + 'static,
    {
        let id = self.callbacks.register(callback);

        // Immediately send current state so this widget can render.
        if *self.ready.borrow() {
            let snapshot = self.current.borrow().clone();
            self.callbacks.notify_single(id, &snapshot);
        }
        id
    }

    /// Unregister a callback by its ID.
    pub fn disconnect(&self, id: CallbackId) -> bool {
        self.callbacks.unregister(id)
    }

    fn handle_update(&self, window_info: &WindowInfo) {
        // Update stored state
        let snapshot: WindowTitleSnapshot = window_info.into();
        *self.current.borrow_mut() = snapshot.clone();
        *self.ready.borrow_mut() = true;

        // Notify callbacks.
        self.callbacks.notify(&snapshot);
    }

    fn register_with_manager(this: &Rc<Self>, manager: &Rc<CompositorManager>) {
        // Create callback that handles updates
        let service_weak = Rc::downgrade(this);
        manager.register_window_callback(move |window_info| {
            if let Some(service) = service_weak.upgrade() {
                service.handle_update(window_info);
            }
        });
    }
}

impl Drop for WindowTitleService {
    fn drop(&mut self) {
        debug!("WindowTitleService dropped");
    }
}
