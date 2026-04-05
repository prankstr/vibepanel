//! WindowListService - shared, event-driven window list state via CompositorManager.
//!
//! This provides a GTK-friendly API for the list of all windows:
//! - Uses the shared CompositorManager singleton
//! - Provides window list snapshot access
//! - Supports callback registration for reactive updates

use std::cell::RefCell;
use std::rc::Rc;

use tracing::debug;

use super::callbacks::{CallbackId, Callbacks};
use super::compositor::{CompositorManager, WindowListSnapshot};

/// Shared, process-wide window list service.
///
/// Provides reactive window list state with GTK main loop integration.
/// Widgets should call `connect()` to receive updates when window list changes.
pub struct WindowListService {
    current: RefCell<WindowListSnapshot>,
    callbacks: Callbacks<WindowListSnapshot>,
    ready: RefCell<bool>,
    manager_callback_id: RefCell<Option<CallbackId>>,
}

impl WindowListService {
    fn new() -> Rc<Self> {
        let manager = CompositorManager::global();
        let windows = manager.list_windows();
        let initial = WindowListSnapshot { windows };

        let service = Rc::new(Self {
            current: RefCell::new(initial),
            callbacks: Callbacks::new(),
            ready: RefCell::new(true),
            manager_callback_id: RefCell::new(None),
        });

        Self::register_with_manager(&service, &manager);

        debug!("WindowListService initialized");
        service
    }

    /// Get the global WindowListService singleton.
    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<WindowListService> = WindowListService::new();
        }

        INSTANCE.with(|s| s.clone())
    }

    /// Register a callback to be invoked when window list changes.
    /// The callback is always executed on the GLib main loop.
    pub fn connect<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&WindowListSnapshot) + 'static,
    {
        let id = self.callbacks.register(callback);

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

    /// Focus a specific window by its ID.
    pub fn focus_window(&self, window_id: u64) {
        let manager = CompositorManager::global();
        manager.focus_window(window_id);
    }

    fn handle_update(&self, window_list: &WindowListSnapshot) {
        *self.current.borrow_mut() = window_list.clone();
        *self.ready.borrow_mut() = true;
        self.callbacks.notify(window_list);
    }

    fn register_with_manager(this: &Rc<Self>, manager: &Rc<CompositorManager>) {
        let service_weak = Rc::downgrade(this);
        let callback_id = manager.register_window_list_callback(move |snapshot| {
            if let Some(service) = service_weak.upgrade() {
                service.handle_update(snapshot);
            }
        });
        *this.manager_callback_id.borrow_mut() = Some(callback_id);
    }
}

impl Drop for WindowListService {
    fn drop(&mut self) {
        if let Some(id) = self.manager_callback_id.borrow_mut().take() {
            let manager = CompositorManager::global();
            manager.unregister_window_list_callback(id);
        }
        debug!("WindowListService dropped");
    }
}
