//! IdleInhibitorService - Prevents system idle/sleep using systemd-logind.
//!
//! This service uses the `org.freedesktop.login1.Manager.Inhibit` D-Bus API
//! on the system bus to acquire an fd-based inhibit lock. Holding the fd
//! prevents idle and sleep; dropping it releases the lock. This is the same
//! mechanism used by systemd-inhibit(1) and is supported by all systemd-based
//! systems regardless of which idle daemon is running (hypridle, swayidle, etc.).
//!
//! ## Usage
//!
//! The service is a singleton that can be toggled on/off. When active,
//! it prevents the system from going idle or suspending.

use std::cell::RefCell;
use std::os::unix::io::OwnedFd;
use std::rc::Rc;

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use tracing::{debug, warn};

use super::callbacks::{CallbackId, Callbacks};

/// Canonical snapshot of idle inhibitor state.
#[derive(Debug, Clone)]
pub struct IdleInhibitorSnapshot {
    /// Whether the inhibitor is currently active.
    pub active: bool,
    /// Whether the inhibitor is available.
    pub available: bool,
}

/// Shared, process-wide idle inhibitor service.
///
/// Uses `org.freedesktop.login1.Manager.Inhibit` on the system bus to acquire
/// an fd-based inhibit lock. The lock prevents idle and sleep while the fd is
/// open. Dropping the fd releases the lock — the kernel guarantees cleanup
/// even on SIGKILL.
pub struct IdleInhibitorService {
    /// Current snapshot of inhibitor state.
    snapshot: RefCell<IdleInhibitorSnapshot>,
    /// Registered callbacks for state changes.
    callbacks: Callbacks<IdleInhibitorSnapshot>,
    /// System bus connection for logind calls.
    dbus_connection: Option<gio::DBusConnection>,
    /// Inhibit lock fd. Holding this = inhibited, dropping it = released.
    inhibit_fd: RefCell<Option<OwnedFd>>,
}

impl IdleInhibitorService {
    /// Create a new IdleInhibitorService.
    fn new() -> Rc<Self> {
        let connection = match gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE) {
            Ok(conn) => {
                debug!("IdleInhibitorService: connected to system bus");
                Some(conn)
            }
            Err(e) => {
                warn!(
                    "IdleInhibitorService: failed to connect to system bus: {}. \
                     Idle inhibition will not work.",
                    e
                );
                None
            }
        };

        let available = connection.is_some();

        Rc::new(Self {
            snapshot: RefCell::new(IdleInhibitorSnapshot {
                active: false,
                available,
            }),
            callbacks: Callbacks::new(),
            dbus_connection: connection,
            inhibit_fd: RefCell::new(None),
        })
    }

    /// Get the global IdleInhibitorService singleton.
    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<IdleInhibitorService> = IdleInhibitorService::new();
        }

        INSTANCE.with(|s| s.clone())
    }

    /// Register a callback to be invoked whenever the inhibitor state changes.
    pub fn connect<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&IdleInhibitorSnapshot) + 'static,
    {
        let id = self.callbacks.register(callback);

        // Immediately send current snapshot.
        let snapshot = self.snapshot.borrow().clone();
        self.callbacks.notify_single(id, &snapshot);
        id
    }

    /// Unregister a callback by its ID.
    pub fn disconnect(&self, id: CallbackId) -> bool {
        self.callbacks.unregister(id)
    }

    /// Return the current inhibitor snapshot.
    pub fn snapshot(&self) -> IdleInhibitorSnapshot {
        self.snapshot.borrow().clone()
    }

    /// Whether the inhibitor is available.
    #[allow(dead_code)]
    pub fn available(&self) -> bool {
        self.snapshot.borrow().available
    }

    /// Toggle the inhibitor state.
    pub fn toggle(&self) {
        let current = self.snapshot.borrow().active;
        self.set_active(!current);
    }

    /// Set the inhibitor state.
    pub fn set_active(&self, active: bool) {
        let current = self.snapshot.borrow().active;
        if current == active {
            return;
        }

        if active {
            self.enable_inhibitor();
        } else {
            self.disable_inhibitor();
        }
    }

    /// Stop the service and release any inhibitor.
    #[allow(dead_code)]
    pub fn stop(&self) {
        if self.snapshot.borrow().active {
            self.disable_inhibitor();
        }
    }

    // Internal - logind Inhibit API

    fn enable_inhibitor(&self) {
        let Some(connection) = self.dbus_connection.as_ref() else {
            warn!("IdleInhibitorService: cannot enable - no system bus connection");
            return;
        };

        // Call org.freedesktop.login1.Manager.Inhibit(what, who, why, mode)
        let args = (
            "idle:sleep",                     // what
            "vibepanel",                      // who
            "User requested idle inhibition", // why
            "block",                          // mode
        );

        let result = connection.call_with_unix_fd_list_sync(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
            "Inhibit",
            Some(&args.to_variant()),
            Some(glib::VariantTy::new("(h)").unwrap()),
            gio::DBusCallFlags::NONE,
            5000,
            gio::UnixFDList::NONE,
            gio::Cancellable::NONE,
        );

        match result {
            Ok((_reply, Some(fd_list))) => match fd_list.get(0) {
                Ok(owned_fd) => {
                    debug!("IdleInhibitorService: enabled (fd={:?})", owned_fd);
                    *self.inhibit_fd.borrow_mut() = Some(owned_fd);

                    let mut snapshot = self.snapshot.borrow_mut();
                    snapshot.active = true;
                    let snapshot_clone = snapshot.clone();
                    drop(snapshot);

                    self.callbacks.notify(&snapshot_clone);
                }
                Err(e) => {
                    warn!("IdleInhibitorService: failed to get fd from reply: {}", e);
                }
            },
            Ok((_reply, None)) => {
                warn!("IdleInhibitorService: no fd list in reply");
            }
            Err(e) => {
                warn!("IdleInhibitorService: failed to call Inhibit: {}", e);
            }
        }
    }

    fn disable_inhibitor(&self) {
        // Drop the fd — this releases the logind inhibit lock.
        let had_fd = self.inhibit_fd.borrow_mut().take().is_some();

        if had_fd {
            debug!("IdleInhibitorService: disabled (fd dropped)");
        }

        let mut snapshot = self.snapshot.borrow_mut();
        if snapshot.active {
            snapshot.active = false;
            let snapshot_clone = snapshot.clone();
            drop(snapshot);
            self.callbacks.notify(&snapshot_clone);
        }
    }
}
