//! Wayland idle inhibition via `zwp_idle_inhibit_manager_v1`.
//!
//! Compositors such as Sway (and idle daemons built on top of them, e.g.
//! swayidle) only honor the Wayland `idle-inhibit-unstable-v1` protocol; they
//! do not look at systemd-logind inhibitor locks. An inhibitor is tied to a
//! `wl_surface` and is active while that surface is visible, so we attach it to
//! one of the bar's layer-shell surfaces.
//!
//! Like `activation.rs` and `background_effect.rs`, this shares GDK's
//! connection and uses a private event queue.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::{Cast, SurfaceExt, WidgetExt};
use tracing::{debug, warn};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::wp::idle_inhibit::zv1::client::{
    zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1, zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1,
};

use super::{SurfaceInfo, connection_from_gdk_display, install_event_dispatch};
use crate::styles::class;

/// Internal wayland-client dispatch state.
struct IdleInhibitState {
    manager: Option<ZwpIdleInhibitManagerV1>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for IdleInhibitState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
            && interface == "zwp_idle_inhibit_manager_v1"
            && state.manager.is_none()
        {
            state.manager = Some(registry.bind(name, 1, qh, ()));
        }
    }
}

impl Dispatch<ZwpIdleInhibitManagerV1, ()> for IdleInhibitState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpIdleInhibitManagerV1,
        _event: <ZwpIdleInhibitManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // zwp_idle_inhibit_manager_v1 has no events.
    }
}

impl Dispatch<ZwpIdleInhibitorV1, ()> for IdleInhibitState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpIdleInhibitorV1,
        _event: <ZwpIdleInhibitorV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // zwp_idle_inhibitor_v1 has no events.
    }
}

/// Mapped bar surfaces (one per monitor). Compositors ignore inhibitors on
/// unmapped surfaces, e.g. bars hidden via IPC.
fn mapped_bar_surfaces() -> impl Iterator<Item = SurfaceInfo> {
    gtk4::Window::list_toplevels()
        .into_iter()
        .filter(|widget| widget.has_css_class(class::BAR_WINDOW) && widget.is_mapped())
        .filter_map(|widget| SurfaceInfo::from_widget(&widget))
        .filter(|surface| surface_is_usable(&surface.wayland_surface))
}

fn surface_is_usable(surface: &gdk4_wayland::WaylandSurface) -> bool {
    !surface.is_destroyed() && surface.is_mapped()
}

/// Handle to the compositor's idle-inhibit manager.
pub struct IdleInhibitProtocol {
    connection: Connection,
    manager: ZwpIdleInhibitManagerV1,
    qh: QueueHandle<IdleInhibitState>,
    // Kept alive so the private queue keeps being dispatched.
    _state: Rc<RefCell<IdleInhibitState>>,
    _event_queue: Rc<RefCell<EventQueue<IdleInhibitState>>>,
}

impl IdleInhibitProtocol {
    /// Bind `zwp_idle_inhibit_manager_v1`, if the compositor offers it.
    ///
    /// Must be called on the main thread after the GDK display exists.
    pub fn try_new() -> Option<Self> {
        let gdk_display = gtk4::gdk::Display::default()?;
        let wayland_display = gdk_display
            .downcast::<gdk4_wayland::WaylandDisplay>()
            .ok()?;

        if !wayland_display.query_registry("zwp_idle_inhibit_manager_v1") {
            debug!("Compositor does not advertise zwp_idle_inhibit_manager_v1");
            return None;
        }

        let connection = connection_from_gdk_display(&wayland_display)?;
        let mut event_queue: EventQueue<IdleInhibitState> = connection.new_event_queue();
        let qh = event_queue.handle();
        let _registry = connection.display().get_registry(&qh, ());

        let mut state = IdleInhibitState { manager: None };
        if let Err(e) = event_queue.roundtrip(&mut state) {
            warn!("Idle inhibit roundtrip failed: {e}");
            return None;
        }
        let manager = state.manager.clone()?;

        debug!("Wayland idle-inhibit protocol available");

        let state = Rc::new(RefCell::new(state));
        let event_queue = Rc::new(RefCell::new(event_queue));
        install_event_dispatch(&event_queue, &state, "IdleInhibit");

        Some(Self {
            connection,
            manager,
            qh,
            _state: state,
            _event_queue: event_queue,
        })
    }

    /// Create an inhibitor on one of the mapped bar surfaces.
    ///
    /// Returns `None` when no bar surface is mapped.
    pub fn acquire(&self) -> Option<WaylandIdleInhibitor> {
        let surface = mapped_bar_surfaces().next()?;
        let inhibitor = self
            .manager
            .create_inhibitor(&surface.wl_surface, &self.qh, ());
        if let Err(e) = self.connection.flush() {
            warn!("Idle inhibit flush failed: {e}");
        }
        debug!("Wayland idle inhibitor created");
        Some(WaylandIdleInhibitor {
            inhibitor,
            surface: surface.wayland_surface,
            connection: self.connection.clone(),
        })
    }
}

/// An active `zwp_idle_inhibitor_v1`. Destroyed on drop.
pub struct WaylandIdleInhibitor {
    inhibitor: ZwpIdleInhibitorV1,
    // Held as the GDK surface rather than an `ObjectId`: GDK-owned proxies
    // have no liveness tracking, so a recreated bar can reuse the same id.
    surface: gdk4_wayland::WaylandSurface,
    connection: Connection,
}

impl WaylandIdleInhibitor {
    /// Whether the surface this inhibitor is attached to still exists and is
    /// mapped (bars are recreated on monitor hotplug and config reload, and
    /// can be hidden via IPC).
    pub fn is_alive(&self) -> bool {
        surface_is_usable(&self.surface)
    }
}

impl Drop for WaylandIdleInhibitor {
    fn drop(&mut self) {
        self.inhibitor.destroy();
        let _ = self.connection.flush();
        debug!("Wayland idle inhibitor destroyed");
    }
}
