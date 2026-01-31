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
//!
//! # Identity Tracking
//!
//! The tracker uses unique IDs rather than pointer equality because:
//! - `Rc<dyn Dismissible>` pointer equality is unreliable (casting creates new fat pointers)
//! - IDs are simple, unambiguous, and work correctly across all use cases

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::widgets::layer_shell_popover::Dismissible;

thread_local! {
    static POPOVER_TRACKER_INSTANCE: RefCell<Option<Rc<PopoverTracker>>> = const { RefCell::new(None) };
}

/// Unique identifier for a registered popover.
pub type PopoverId = u64;

/// Global popover tracker singleton.
///
/// Manages the currently active popover and provides seamless transitions
/// between bar widget menus.
pub struct PopoverTracker {
    /// Currently active dismissible surface (if any), with its ID.
    active: RefCell<Option<(PopoverId, Rc<dyn Dismissible>)>>,
    /// Next ID to assign.
    next_id: Cell<PopoverId>,
}

impl Default for PopoverTracker {
    fn default() -> Self {
        Self {
            active: RefCell::new(None),
            next_id: Cell::new(1), // Start at 1 so 0 can be "no ID"
        }
    }
}

impl PopoverTracker {
    /// Get the global PopoverTracker instance.
    ///
    /// Lazily initializes the singleton on first access.
    pub fn global() -> Rc<Self> {
        POPOVER_TRACKER_INSTANCE.with(|cell| {
            let mut opt = cell.borrow_mut();
            if opt.is_none() {
                *opt = Some(Rc::new(PopoverTracker::default()));
            }
            opt.as_ref().unwrap().clone()
        })
    }

    /// Set the currently active popover.
    ///
    /// Returns a unique ID that should be stored and passed to `clear_if_active()`
    /// when the popover closes.
    ///
    /// If there's already an active popover, it will be dismissed first.
    #[must_use = "the returned PopoverId must be stored and passed to clear_if_active() on close"]
    pub fn set_active(&self, popover: Rc<dyn Dismissible>) -> PopoverId {
        // Dismiss any existing active popover
        self.dismiss_active();

        // Assign new ID
        let id = self.next_id.get();
        self.next_id.set(id + 1);

        // Set the new active popover
        *self.active.borrow_mut() = Some((id, popover));

        id
    }

    /// Clear the active popover reference without dismissing it.
    ///
    /// Called when a popover hides itself and wants to unregister from tracking.
    /// Only clears if the given ID matches the currently active one, preventing
    /// one surface from accidentally clearing another's registration.
    pub fn clear_if_active(&self, id: PopoverId) {
        let is_same = self
            .active
            .borrow()
            .as_ref()
            .is_some_and(|(active_id, _)| *active_id == id);
        if is_same {
            *self.active.borrow_mut() = None;
        }
    }

    /// Dismiss the currently active popover (if any).
    pub fn dismiss_active(&self) {
        // Take the active popover while releasing the borrow immediately.
        // This is important because dismiss() may call clear_if_active() which needs to borrow.
        let active = self.active.borrow_mut().take();
        if let Some((_, dismissible)) = active
            && dismissible.is_visible()
        {
            dismissible.dismiss();
        }
    }
}
