//! Popover tracker for managing active popovers and seamless transitions.
//!
//! This module provides a global singleton that tracks which popover is currently
//! active and enables seamless transitions between bar widget menus.
//!
//! # Architecture
//!
//! When a bar widget is clicked:
//! 1. The widget's click handler calls `PopoverTracker::dismiss_active()`
//! 2. Any existing popover is dismissed
//! 3. The widget's menu is shown and registered via `set_active()`
//!
//! This enables clicking directly from one widget's menu to another without
//! requiring the first menu to be explicitly closed.

use std::cell::RefCell;
use std::rc::Rc;

use crate::widgets::layer_shell_popover::Dismissible;

thread_local! {
    static POPOVER_TRACKER_INSTANCE: RefCell<Option<Rc<PopoverTracker>>> = const { RefCell::new(None) };
}

/// Global popover tracker singleton.
///
/// Manages the currently active popover and provides seamless transitions
/// between bar widget menus.
pub struct PopoverTracker {
    /// Currently active dismissible surface (if any).
    active: RefCell<Option<Rc<dyn Dismissible>>>,
}

impl PopoverTracker {
    /// Get the global PopoverTracker instance.
    ///
    /// Lazily initializes the singleton on first access.
    pub fn global() -> Rc<Self> {
        POPOVER_TRACKER_INSTANCE.with(|cell| {
            let mut opt = cell.borrow_mut();
            if opt.is_none() {
                *opt = Some(Rc::new(PopoverTracker::new()));
            }
            opt.as_ref().unwrap().clone()
        })
    }

    fn new() -> Self {
        Self {
            active: RefCell::new(None),
        }
    }

    /// Set the currently active popover.
    ///
    /// If there's already an active popover that is different from the new one,
    /// it will be dismissed first.
    pub fn set_active(&self, popover: Rc<dyn Dismissible>) {
        // Check if this is the same popover
        let same = self.active.borrow().as_ref().is_some_and(|active| {
            // Compare by Rc pointer
            Rc::ptr_eq(active, &popover)
        });

        if same {
            return;
        }

        // Dismiss any existing active popover
        self.dismiss_active();

        // Set the new active popover
        *self.active.borrow_mut() = Some(popover);
    }

    /// Clear the active popover reference without dismissing it.
    ///
    /// Used when a popover hides itself and wants to unregister.
    pub fn clear(&self) {
        *self.active.borrow_mut() = None;
    }

    /// Dismiss the currently active popover (if any).
    pub fn dismiss_active(&self) {
        // Take the active popover while releasing the borrow immediately.
        // This is important because dismiss() may call clear() which needs to borrow.
        let active = self.active.borrow_mut().take();
        if let Some(active) = active
            && active.is_visible()
        {
            active.dismiss();
        }
    }

    /// Check if there's a currently active popover.
    #[allow(dead_code)]
    pub fn has_active(&self) -> bool {
        self.active
            .borrow()
            .as_ref()
            .is_some_and(|p| p.is_visible())
    }
}
