//! Generic callback registry for service snapshot updates.
//!
//! This module provides `Callbacks<T>`, a reusable helper for the common
//! snapshot+callback pattern used across most services in the bar.
//!
//! ## Usage
//!
//! ```rust,ignore
//! pub struct MyService {
//!     snapshot: RefCell<MySnapshot>,
//!     callbacks: Callbacks<MySnapshot>,
//! }
//!
//! impl MyService {
//!     pub fn connect<F>(&self, callback: F) -> CallbackId
//!     where
//!         F: Fn(&MySnapshot) + 'static,
//!     {
//!         let id = self.callbacks.register(callback);
//!         // Immediately invoke with current snapshot
//!         self.callbacks.notify(&self.snapshot.borrow());
//!         id
//!     }
//!
//!     pub fn disconnect(&self, id: CallbackId) {
//!         self.callbacks.unregister(id);
//!     }
//!
//!     fn on_state_change(&self) {
//!         self.callbacks.notify(&self.snapshot.borrow());
//!     }
//! }
//! ```

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Unique identifier for a registered callback.
///
/// Used to unregister callbacks when they are no longer needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CallbackId(u64);

/// Global counter for generating unique callback IDs.
static NEXT_CALLBACK_ID: AtomicU64 = AtomicU64::new(1);

impl CallbackId {
    /// Generate a new unique callback ID.
    fn new() -> Self {
        Self(NEXT_CALLBACK_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// Entry in the callback registry, pairing an ID with a callback.
struct CallbackEntry<T> {
    id: CallbackId,
    callback: Rc<dyn Fn(&T)>,
}

/// Type alias for the callback storage to reduce complexity.
type CallbackList<T> = Vec<CallbackEntry<T>>;

/// A registry of callbacks that receive snapshot updates.
///
/// This is the standard pattern used by services to notify widgets of state changes.
/// Callbacks are stored as `Rc<dyn Fn(&T)>` to allow cloning for async notification.
///
/// Each callback is assigned a unique `CallbackId` which can be used to unregister
/// it when no longer needed (e.g., when a widget is destroyed).
pub struct Callbacks<T> {
    inner: RefCell<CallbackList<T>>,
}

impl<T> Callbacks<T> {
    /// Create a new empty callback registry.
    pub fn new() -> Self {
        Self {
            inner: RefCell::new(Vec::new()),
        }
    }

    /// Register a callback to be invoked on snapshot updates.
    ///
    /// The callback is wrapped in `Rc` for efficient cloning during notification.
    /// Returns a `CallbackId` that can be used to unregister the callback.
    pub fn register<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&T) + 'static,
    {
        let id = CallbackId::new();
        self.inner.borrow_mut().push(CallbackEntry {
            id,
            callback: Rc::new(callback),
        });
        id
    }

    /// Unregister a callback by its ID.
    ///
    /// Returns `true` if the callback was found and removed, `false` otherwise.
    pub fn unregister(&self, id: CallbackId) -> bool {
        let mut inner = self.inner.borrow_mut();
        let len_before = inner.len();
        inner.retain(|entry| entry.id != id);
        inner.len() < len_before
    }

    /// Notify all registered callbacks with the given snapshot.
    ///
    /// Callbacks are cloned before iteration to avoid holding the borrow
    /// during invocation, which prevents panics if callbacks re-enter the service.
    pub fn notify(&self, snapshot: &T) {
        let callbacks: Vec<_> = self
            .inner
            .borrow()
            .iter()
            .map(|entry| entry.callback.clone())
            .collect();
        for cb in callbacks {
            cb(snapshot);
        }
    }

    /// Notify a single callback by its ID with the given snapshot.
    ///
    /// This is useful for giving a newly registered callback the current state
    /// without re-notifying all other callbacks.
    ///
    /// Returns `true` if the callback was found and invoked, `false` otherwise.
    pub fn notify_single(&self, id: CallbackId, snapshot: &T) -> bool {
        let callback = self
            .inner
            .borrow()
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| entry.callback.clone());

        if let Some(cb) = callback {
            cb(snapshot);
            true
        } else {
            false
        }
    }

    /// Returns true if no callbacks are registered.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.inner.borrow().is_empty()
    }

    /// Returns the number of registered callbacks.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.inner.borrow().len()
    }
}

impl<T> Default for Callbacks<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn test_callbacks_register_and_notify() {
        let callbacks: Callbacks<i32> = Callbacks::new();
        let counter = Rc::new(Cell::new(0));

        let counter_clone = counter.clone();
        let _id = callbacks.register(move |value| {
            counter_clone.set(counter_clone.get() + *value);
        });

        callbacks.notify(&5);
        assert_eq!(counter.get(), 5);

        callbacks.notify(&3);
        assert_eq!(counter.get(), 8);
    }

    #[test]
    fn test_callbacks_multiple_listeners() {
        let callbacks: Callbacks<String> = Callbacks::new();
        let results = Rc::new(RefCell::new(Vec::new()));

        let results_clone = results.clone();
        let _id1 = callbacks.register(move |s| {
            results_clone.borrow_mut().push(format!("A:{}", s));
        });

        let results_clone = results.clone();
        let _id2 = callbacks.register(move |s| {
            results_clone.borrow_mut().push(format!("B:{}", s));
        });

        callbacks.notify(&"test".to_string());

        let collected: Vec<_> = results.borrow().clone();
        assert_eq!(collected, vec!["A:test", "B:test"]);
    }

    #[test]
    fn test_callbacks_empty() {
        let callbacks: Callbacks<()> = Callbacks::new();
        assert!(callbacks.is_empty());
        assert_eq!(callbacks.len(), 0);

        let _id = callbacks.register(|_| {});
        assert!(!callbacks.is_empty());
        assert_eq!(callbacks.len(), 1);
    }

    #[test]
    fn test_callbacks_unregister() {
        let callbacks: Callbacks<i32> = Callbacks::new();
        let counter = Rc::new(Cell::new(0));

        let counter_clone = counter.clone();
        let id1 = callbacks.register(move |value| {
            counter_clone.set(counter_clone.get() + *value);
        });

        let counter_clone = counter.clone();
        let id2 = callbacks.register(move |value| {
            counter_clone.set(counter_clone.get() + *value * 10);
        });

        assert_eq!(callbacks.len(), 2);

        // Both callbacks fire
        callbacks.notify(&1);
        assert_eq!(counter.get(), 11); // 1 + 10

        // Unregister first callback
        assert!(callbacks.unregister(id1));
        assert_eq!(callbacks.len(), 1);

        // Only second callback fires
        callbacks.notify(&1);
        assert_eq!(counter.get(), 21); // 11 + 10

        // Unregister second callback
        assert!(callbacks.unregister(id2));
        assert_eq!(callbacks.len(), 0);

        // No callbacks fire
        callbacks.notify(&1);
        assert_eq!(counter.get(), 21);

        // Unregistering non-existent ID returns false
        assert!(!callbacks.unregister(id1));
    }

    #[test]
    fn test_callback_ids_are_unique() {
        let id1 = CallbackId::new();
        let id2 = CallbackId::new();
        let id3 = CallbackId::new();

        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_notify_single() {
        let callbacks: Callbacks<i32> = Callbacks::new();
        let counter1 = Rc::new(Cell::new(0));
        let counter2 = Rc::new(Cell::new(0));

        let counter1_clone = counter1.clone();
        let id1 = callbacks.register(move |value| {
            counter1_clone.set(counter1_clone.get() + *value);
        });

        let counter2_clone = counter2.clone();
        let _id2 = callbacks.register(move |value| {
            counter2_clone.set(counter2_clone.get() + *value);
        });

        // Notify only the first callback
        assert!(callbacks.notify_single(id1, &5));
        assert_eq!(counter1.get(), 5);
        assert_eq!(counter2.get(), 0); // Second callback not invoked

        // Notify all callbacks
        callbacks.notify(&3);
        assert_eq!(counter1.get(), 8);
        assert_eq!(counter2.get(), 3);

        // Notify non-existent ID returns false
        let fake_id = CallbackId::new();
        assert!(!callbacks.notify_single(fake_id, &10));
    }
}
