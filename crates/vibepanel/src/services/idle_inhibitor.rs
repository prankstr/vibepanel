//! IdleInhibitorService - Prevents the system from going idle.
//!
//! Two mechanisms are used together:
//!
//! - The Wayland `idle-inhibit-unstable-v1` protocol, attached to a bar
//!   surface. This is what Sway/swayidle (and most wlroots-based compositors)
//!   honor; they ignore logind inhibitors.
//! - The `org.freedesktop.login1.Manager.Inhibit` D-Bus API on the system bus,
//!   which yields an fd-based inhibit lock (visible in `systemd-inhibit`).
//!   Holding the fd prevents automatic idle actions; dropping it releases the
//!   lock. Idle managers that honor systemd idle inhibitors (e.g. hypridle)
//!   respect this one.
//!
//! ## Usage
//!
//! The service is a singleton that can be toggled on/off. When active,
//! it prevents automatic idle actions without blocking explicit suspend.

use std::cell::RefCell;
use std::os::unix::io::OwnedFd;
use std::rc::Rc;

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use tracing::{debug, warn};

use super::callbacks::{CallbackId, Callbacks};
use super::idle_inhibit::{IdleInhibitProtocol, WaylandIdleInhibitor};

/// How often to check that the Wayland inhibitor's bar surface still exists
/// and is mapped.
const SURFACE_CHECK_INTERVAL_SECS: u32 = 2;

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
/// Holds a Wayland idle inhibitor (when the compositor supports the protocol)
/// and a logind fd-based inhibit lock. Dropping the fd releases the logind
/// lock — the kernel guarantees cleanup even on SIGKILL; the compositor drops
/// the Wayland inhibitor when the client disconnects.
pub struct IdleInhibitorService {
    /// Current snapshot of inhibitor state.
    snapshot: RefCell<IdleInhibitorSnapshot>,
    /// Registered callbacks for state changes.
    callbacks: Callbacks<IdleInhibitorSnapshot>,
    /// System bus connection for logind calls.
    dbus_connection: Option<gio::DBusConnection>,
    /// Inhibit lock fd. Holding this = inhibited, dropping it = released.
    inhibit_fd: RefCell<Option<OwnedFd>>,
    /// Wayland idle-inhibit protocol, if the compositor supports it.
    wayland: Option<IdleInhibitProtocol>,
    /// Active Wayland inhibitor, attached to a bar surface.
    wayland_inhibitor: RefCell<Option<WaylandIdleInhibitor>>,
    /// Periodic check that re-attaches the Wayland inhibitor when bars are
    /// recreated or hidden.
    surface_check: RefCell<Option<glib::SourceId>>,
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
                     logind idle inhibition unavailable.",
                    e
                );
                None
            }
        };

        let wayland = IdleInhibitProtocol::try_new();
        let available = connection.is_some() || wayland.is_some();

        Rc::new(Self {
            snapshot: RefCell::new(IdleInhibitorSnapshot {
                active: false,
                available,
            }),
            callbacks: Callbacks::new(),
            dbus_connection: connection,
            inhibit_fd: RefCell::new(None),
            wayland,
            wayland_inhibitor: RefCell::new(None),
            surface_check: RefCell::new(None),
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

    // Internal - logind Inhibit API and Wayland idle-inhibit

    fn enable_inhibitor(&self) {
        let logind_ok = self.acquire_logind_lock();
        let wayland_ok = self.wayland.is_some();

        if !logind_ok && !wayland_ok {
            warn!(
                "IdleInhibitorService: cannot enable - no logind lock and no Wayland idle-inhibit"
            );
            return;
        }

        if wayland_ok {
            self.refresh_wayland_inhibitor(true);

            // Bars are recreated on monitor hotplug / config reload and can be
            // hidden via IPC, which makes the inhibitor inert, so keep it
            // attached to a mapped bar.
            let id = glib::timeout_add_seconds_local(SURFACE_CHECK_INTERVAL_SECS, || {
                IdleInhibitorService::global().refresh_wayland_inhibitor(false);
                glib::ControlFlow::Continue
            });
            if let Some(old) = self.surface_check.borrow_mut().replace(id) {
                old.remove();
            }
        }

        let mut snapshot = self.snapshot.borrow_mut();
        snapshot.active = true;
        let snapshot_clone = snapshot.clone();
        drop(snapshot);

        self.callbacks.notify(&snapshot_clone);
    }

    /// Acquire the logind `idle` inhibit lock. Returns whether it was acquired.
    fn acquire_logind_lock(&self) -> bool {
        let Some(connection) = self.dbus_connection.as_ref() else {
            debug!("IdleInhibitorService: no system bus connection, skipping logind lock");
            return false;
        };

        // Call org.freedesktop.login1.Manager.Inhibit(what, who, why, mode)
        let args = (
            "idle",                           // what
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
                    debug!(
                        "IdleInhibitorService: logind lock acquired (fd={:?})",
                        owned_fd
                    );
                    *self.inhibit_fd.borrow_mut() = Some(owned_fd);
                    true
                }
                Err(e) => {
                    warn!("IdleInhibitorService: failed to get fd from reply: {}", e);
                    false
                }
            },
            Ok((_reply, None)) => {
                warn!("IdleInhibitorService: no fd list in reply");
                false
            }
            Err(e) => {
                warn!("IdleInhibitorService: failed to call Inhibit: {}", e);
                false
            }
        }
    }

    /// Ensure a Wayland inhibitor is attached to a mapped bar surface.
    ///
    /// No-op when the protocol is unavailable or when the current inhibitor's
    /// surface still exists and is mapped. Logs once when no mapped bar is
    /// available, on `first_attempt` or when a previous inhibitor is lost.
    fn refresh_wayland_inhibitor(&self, first_attempt: bool) {
        let Some(protocol) = self.wayland.as_ref() else {
            return;
        };

        let mut slot = self.wayland_inhibitor.borrow_mut();
        let was_attached = slot.is_some();
        if let Some(inhibitor) = slot.as_ref() {
            if inhibitor.is_alive() {
                return;
            }
            debug!(
                "IdleInhibitorService: bar surface gone or hidden, re-attaching Wayland inhibitor"
            );
            *slot = None;
        }
        *slot = protocol.acquire();

        if slot.is_none() && (was_attached || first_attempt) {
            debug!("IdleInhibitorService: no mapped bar to attach the Wayland inhibitor to");
        }
    }

    fn disable_inhibitor(&self) {
        if let Some(id) = self.surface_check.borrow_mut().take() {
            id.remove();
        }

        // Dropping the inhibitor destroys the Wayland object.
        self.wayland_inhibitor.borrow_mut().take();

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
