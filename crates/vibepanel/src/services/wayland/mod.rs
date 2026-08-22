//! Shared Wayland services and GDK connection integration.

use std::cell::RefCell;
use std::os::fd::{AsFd, AsRawFd};
use std::rc::Rc;

use gdk4_wayland::prelude::WaylandSurfaceExtManual;
use gtk4::glib;
use gtk4::prelude::{Cast, NativeExt, WidgetExt};
use tracing::warn;
use wayland_client::{Connection, EventQueue, Proxy};

pub mod activation;
pub mod background_effect;

/// Resolved Wayland surface identity for a GTK widget.
pub(super) struct SurfaceInfo {
    wl_surface: wayland_client::protocol::wl_surface::WlSurface,
    wayland_surface: gdk4_wayland::WaylandSurface,
    surface_id: wayland_client::backend::ObjectId,
}

impl SurfaceInfo {
    fn from_widget(widget: &impl gtk4::prelude::IsA<gtk4::Widget>) -> Option<Self> {
        let native = widget.as_ref().native()?;
        let gdk_surface = native.surface()?;
        let wayland_surface = gdk_surface
            .downcast::<gdk4_wayland::WaylandSurface>()
            .ok()?;
        let wl_surface = wayland_surface.wl_surface()?;
        let surface_id = wl_surface.id();
        Some(Self {
            wl_surface,
            wayland_surface,
            surface_id,
        })
    }
}

/// Get the `wayland_client::Connection` that GDK uses internally.
///
/// `gdk4_wayland::WaylandDisplay::connection()` is private, so obtain the
/// backend from GDK's cached display proxy and reconstruct the same connection.
/// Creating another backend for the foreign display would compete with GDK for
/// events and can cause it to miss layer-shell configure events.
fn connection_from_gdk_display(
    wayland_display: &gdk4_wayland::WaylandDisplay,
) -> Option<Connection> {
    let wl_display = wayland_display.wl_display()?;
    let backend = wl_display.backend().upgrade()?;
    Some(Connection::from_backend(backend))
}

/// Keep a private Wayland event queue moving from the GLib main loop.
///
/// GDK reads the shared connection continuously, which also accumulates events
/// in private queues. Drain them even when a consumer is not making requests.
fn install_event_dispatch<S: 'static>(
    event_queue: &Rc<RefCell<EventQueue<S>>>,
    state: &Rc<RefCell<S>>,
    label: &'static str,
) {
    let raw_fd = event_queue.borrow().as_fd().as_raw_fd();
    let event_queue = Rc::downgrade(event_queue);
    let state = Rc::downgrade(state);

    glib::unix_fd_add_local(raw_fd, glib::IOCondition::IN, move |_fd, _cond| {
        let (Some(event_queue), Some(state)) = (event_queue.upgrade(), state.upgrade()) else {
            return glib::ControlFlow::Break;
        };

        let mut event_queue = event_queue.borrow_mut();
        let mut state = state.borrow_mut();

        if let Err(error) = event_queue.dispatch_pending(&mut *state) {
            warn!("{label} event dispatch error: {error}");
            return glib::ControlFlow::Continue;
        }

        if let Some(guard) = event_queue.prepare_read() {
            match guard.read() {
                Ok(_) => {
                    let _ = event_queue.dispatch_pending(&mut *state);
                }
                Err(wayland_client::backend::WaylandError::Io(error))
                    if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => warn!("{label} wayland read error: {error}"),
            }
        }

        let _ = event_queue.flush();
        glib::ControlFlow::Continue
    });
}
