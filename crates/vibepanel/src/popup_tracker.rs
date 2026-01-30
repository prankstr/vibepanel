//! Popup tracker for managing active popups and seamless transitions.
//!
//! This module provides a global singleton that tracks which popup is currently
//! active and enables seamless transitions between bar widget menus.
//!
//! # Architecture
//!
//! When a bar widget's menu is about to open:
//! 1. A capture-phase click handler on the bar checks `PopupTracker`
//! 2. If another popup is active, it's dismissed before the new one opens
//! 3. The new popup is registered as the active popup
//!
//! This enables clicking directly from one widget's menu to another without
//! requiring the first menu to be explicitly closed.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::widgets::layer_shell_popover::Dismissible;

/// Callback type for opening a widget's menu.
pub type OpenMenuCallback = Box<dyn Fn()>;

/// Global popup tracker singleton.
///
/// Manages the currently active popup and provides seamless transitions
/// between bar widget menus.
pub struct PopupTracker {
    /// Currently active dismissible surface (if any).
    active: RefCell<Option<Rc<dyn Dismissible>>>,

    /// Registered menu open callbacks by widget pointer.
    /// Maps widget pointer (as usize) to the callback that opens its menu.
    menu_callbacks: RefCell<HashMap<usize, OpenMenuCallback>>,
}

impl PopupTracker {
    /// Get the global PopupTracker instance.
    pub fn global() -> &'static Self {
        thread_local! {
            static INSTANCE: PopupTracker = PopupTracker::new();
        }
        // SAFETY: We're returning a reference to thread-local storage.
        // The PopupTracker lives for the lifetime of the thread.
        INSTANCE.with(|instance| unsafe {
            // Extend lifetime to 'static - safe because it's thread-local
            std::mem::transmute::<&PopupTracker, &'static PopupTracker>(instance)
        })
    }

    fn new() -> Self {
        Self {
            active: RefCell::new(None),
            menu_callbacks: RefCell::new(HashMap::new()),
        }
    }

    /// Set the currently active popup.
    ///
    /// If there's already an active popup that is different from the new one,
    /// it will be dismissed first.
    pub fn set_active(&self, popup: Rc<dyn Dismissible>) {
        // Check if this is the same popup
        let same = self.active.borrow().as_ref().is_some_and(|active| {
            // Compare by Rc pointer
            Rc::ptr_eq(active, &popup)
        });

        if same {
            return;
        }

        // Dismiss any existing active popup
        self.dismiss_active();

        // Set the new active popup
        *self.active.borrow_mut() = Some(popup);
    }

    /// Clear the active popup reference without dismissing it.
    ///
    /// Used when a popup hides itself and wants to unregister.
    pub fn clear(&self) {
        *self.active.borrow_mut() = None;
    }

    /// Dismiss the currently active popup (if any).
    pub fn dismiss_active(&self) {
        if let Some(active) = self.active.borrow_mut().take()
            && active.is_visible()
        {
            active.dismiss();
        }
    }

    /// Check if there's a currently active popup.
    pub fn has_active(&self) -> bool {
        self.active
            .borrow()
            .as_ref()
            .is_some_and(|p| p.is_visible())
    }

    /// Register a callback to open a widget's menu.
    ///
    /// # Arguments
    ///
    /// * `widget_ptr` - The widget's pointer as usize (used as key)
    /// * `callback` - Function that opens the widget's menu
    pub fn register_widget_menu<F>(&self, widget_ptr: usize, callback: F)
    where
        F: Fn() + 'static,
    {
        self.menu_callbacks
            .borrow_mut()
            .insert(widget_ptr, Box::new(callback));
    }

    /// Unregister a widget's menu callback.
    pub fn unregister_widget_menu(&self, widget_ptr: usize) {
        self.menu_callbacks.borrow_mut().remove(&widget_ptr);
    }

    /// Open a widget's menu by its pointer.
    ///
    /// This is called from `check_bar_widget_at_position()` in the click catcher
    /// to enable seamless transitions.
    ///
    /// # Arguments
    ///
    /// * `widget_ptr` - The widget's pointer as usize
    ///
    /// # Returns
    ///
    /// `true` if a callback was found and invoked, `false` otherwise.
    pub fn open_menu_for_widget(&self, widget_ptr: usize) -> bool {
        // First dismiss any active popup
        self.dismiss_active();

        // Then open the target widget's menu
        if let Some(callback) = self.menu_callbacks.borrow().get(&widget_ptr) {
            callback();
            true
        } else {
            false
        }
    }
}
