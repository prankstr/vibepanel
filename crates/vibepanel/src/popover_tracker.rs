//! Tracks the currently active popover for seamless transitions between bar widget menus.
//!
//! Uses unique IDs rather than pointer equality because `Rc<dyn Dismissible>` casting
//! creates new fat pointers that break pointer comparison.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::services::callbacks::{CallbackId, Callbacks};
use crate::widgets::layer_shell_popover::Dismissible;

thread_local! {
    static POPOVER_TRACKER_INSTANCE: RefCell<Option<Rc<PopoverTracker>>> = const { RefCell::new(None) };
}

/// Unique identifier for a registered popover.
pub type PopoverId = u64;

type ActivePopover = (PopoverId, Rc<dyn Dismissible>, Option<String>);

pub struct PopoverTracker {
    active: RefCell<Option<ActivePopover>>,
    next_id: Cell<PopoverId>,
    changes: Callbacks<()>,
}

impl Default for PopoverTracker {
    fn default() -> Self {
        Self {
            active: RefCell::new(None),
            next_id: Cell::new(1),
            changes: Callbacks::new(),
        }
    }
}

impl PopoverTracker {
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
    pub fn set_active(&self, popover: Rc<dyn Dismissible>, output: Option<String>) -> PopoverId {
        // Dismiss any existing active popover
        self.dismiss_active();

        // Assign new ID
        let id = self.next_id.get();
        self.next_id.set(id + 1);

        // Set the new active popover
        *self.active.borrow_mut() = Some((id, popover, output));
        self.notify_changed();

        id
    }

    pub fn holds_output(&self, output: &str) -> bool {
        self.active
            .borrow()
            .as_ref()
            .is_some_and(|(_, popover, owner)| {
                owner.as_deref().is_none_or(|owner| owner == output) && popover.is_visible()
            })
    }

    pub fn dismiss_on_output(&self, output: &str) {
        let owns = self
            .active
            .borrow()
            .as_ref()
            .is_some_and(|(_, _, owner)| owner.as_deref().is_none_or(|owner| owner == output));
        if owns {
            self.dismiss_active();
        }
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
            .is_some_and(|(active_id, _, _)| *active_id == id);
        if is_same {
            *self.active.borrow_mut() = None;
            self.notify_changed();
        }
    }

    /// Dismiss the currently active popover (if any).
    pub fn dismiss_active(&self) {
        // Take the active popover while releasing the borrow immediately.
        // This is important because dismiss() may call clear_if_active() which needs to borrow.
        let active = self.active.borrow_mut().take();
        if let Some((_, dismissible, _)) = active {
            if dismissible.is_visible() {
                dismissible.dismiss();
            }
            self.notify_changed();
        }
    }

    pub fn connect_changed(&self, callback: impl Fn() + 'static) -> CallbackId {
        self.changes.register(move |_| callback())
    }

    pub fn disconnect_changed(&self, id: CallbackId) {
        self.changes.unregister(id);
    }

    pub fn notify_changed(&self) {
        self.changes.notify(&());
    }
}
