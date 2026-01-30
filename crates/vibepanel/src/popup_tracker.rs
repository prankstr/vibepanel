//! Popup tracker for managing active popups and seamless transitions.
//!
//! This module provides a global singleton that tracks which popup is currently
//! active and enables seamless transitions between bar widget menus.
//!
//! # Architecture
//!
//! When a bar widget is clicked:
//! 1. The widget's click handler calls `PopupTracker::dismiss_active()`
//! 2. Any existing popup is dismissed
//! 3. The widget's menu is shown and registered via `set_active()`
//!
//! This enables clicking directly from one widget's menu to another without
//! requiring the first menu to be explicitly closed.

use std::cell::RefCell;
use std::rc::Rc;

use crate::widgets::layer_shell_popover::Dismissible;

thread_local! {
    static POPUP_TRACKER_INSTANCE: RefCell<Option<Rc<PopupTracker>>> = const { RefCell::new(None) };
}

/// Global popup tracker singleton.
///
/// Manages the currently active popup and provides seamless transitions
/// between bar widget menus.
pub struct PopupTracker {
    /// Currently active dismissible surface (if any).
    active: RefCell<Option<Rc<dyn Dismissible>>>,
}

impl PopupTracker {
    /// Get the global PopupTracker instance.
    ///
    /// Lazily initializes the singleton on first access.
    pub fn global() -> Rc<Self> {
        POPUP_TRACKER_INSTANCE.with(|cell| {
            let mut opt = cell.borrow_mut();
            if opt.is_none() {
                *opt = Some(Rc::new(PopupTracker::new()));
            }
            opt.as_ref().unwrap().clone()
        })
    }

    fn new() -> Self {
        Self {
            active: RefCell::new(None),
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
    #[allow(dead_code)]
    pub fn has_active(&self) -> bool {
        self.active
            .borrow()
            .as_ref()
            .is_some_and(|p| p.is_visible())
    }
}
