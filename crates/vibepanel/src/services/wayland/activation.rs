//! XDG activation token service.
//!
//! Creates `xdg_activation_v1` tokens for the notifications D-Bus
//! `ActivationToken` signal, so that clicking a notification lets the target
//! app (e.g. Firefox) raise its window with compositor blessing.
//!
//! Why not GDK's `AppLaunchContext::startup_notify_id()`? GTK only attaches a
//! *surface* to the token when one of our windows has **keyboard** focus. The
//! notification toast runs with `KeyboardMode::None`, so GDK produces a token
//! carrying just a serial. wlroots-derived compositors (dwl/Mango) refuse to
//! activate on tokens without a surface, and niri wants a fresh input serial.
//! A token carrying **both** the clicked surface and the click serial
//! satisfies every compositor.
//!
//! To know the serial and surface of the most recent input event we bind our
//! own `wl_seat` (a client may bind the seat several times; the compositor
//! mirrors input events to every binding), sharing GDK's connection the same
//! way `background_effect.rs` does.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::{Cast, SurfaceExt};
use tracing::{debug, warn};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_keyboard::{self, WlKeyboard};
use wayland_client::protocol::wl_pointer::{self, WlPointer};
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat::{self, Capability as SeatCapability, WlSeat};
use wayland_client::protocol::wl_touch::{self, WlTouch};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::xdg::activation::v1::client::{
    xdg_activation_token_v1::{self, XdgActivationTokenV1},
    xdg_activation_v1::XdgActivationV1,
};

use super::{SurfaceInfo, connection_from_gdk_display, install_event_dispatch};

/// Internal wayland-client dispatch state.
struct ActivationState {
    activation: Option<XdgActivationV1>,
    /// first advertised seat only; multi-seat setups are unicorns.
    seat: Option<WlSeat>,
    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,
    touch: Option<WlTouch>,
    /// Identity of the surface under the pointer / holding keyboard focus.
    pointer_surface: Option<ObjectId>,
    keyboard_surface: Option<ObjectId>,
    /// Serial + surface identity of the most recent actionable input event.
    last_input: Option<(u32, ObjectId)>,
    /// Token string delivered by the compositor for an in-flight request.
    pending_token: Option<String>,
}

impl ActivationState {
    fn new() -> Self {
        Self {
            activation: None,
            seat: None,
            pointer: None,
            keyboard: None,
            touch: None,
            pointer_surface: None,
            keyboard_surface: None,
            last_input: None,
            pending_token: None,
        }
    }

    fn record_input(&mut self, serial: u32, surface_id: Option<ObjectId>) {
        if let Some(surface_id) = surface_id {
            self.last_input = Some((serial, surface_id));
        }
    }

    fn forget_surface(&mut self, surface_id: &ObjectId) {
        if self
            .last_input
            .as_ref()
            .is_some_and(|(_, remembered)| remembered == surface_id)
        {
            self.last_input = None;
        }
    }
}

// GDK-owned foreign proxies do not provide reliable wayland-client liveness
// tracking. Keep only their ObjectId in state, then resolve a fresh proxy from
// a live GDK toplevel immediately before issuing the activation request.
fn live_surface(surface_id: &ObjectId) -> Option<SurfaceInfo> {
    gtk4::Window::list_toplevels()
        .into_iter()
        .filter_map(|widget| SurfaceInfo::from_widget(&widget))
        .filter(|surface| !surface.wayland_surface.is_destroyed())
        .find(|surface| surface.surface_id.eq(surface_id))
}

impl Dispatch<wl_registry::WlRegistry, ()> for ActivationState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "xdg_activation_v1" => {
                    state.activation = Some(registry.bind(name, 1, qh, ()));
                }
                "wl_seat" if state.seat.is_none() => {
                    let seat: WlSeat = registry.bind(name, version.min(5), qh, ());
                    state.seat = Some(seat);
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<XdgActivationV1, ()> for ActivationState {
    fn event(
        _state: &mut Self,
        _proxy: &XdgActivationV1,
        _event: <XdgActivationV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // xdg_activation_v1 has no events.
    }
}

impl Dispatch<XdgActivationTokenV1, ()> for ActivationState {
    fn event(
        state: &mut Self,
        _proxy: &XdgActivationTokenV1,
        event: xdg_activation_token_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_activation_token_v1::Event::Done { token } = event {
            state.pending_token = Some(token);
        }
    }
}

impl Dispatch<WlSeat, ()> for ActivationState {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(SeatCapability::Pointer) {
                if state.pointer.is_none() {
                    state.pointer = Some(seat.get_pointer(qh, ()));
                }
            } else if let Some(pointer) = state.pointer.take() {
                pointer.release();
                if let Some(surface_id) = state.pointer_surface.take() {
                    state.forget_surface(&surface_id);
                }
            }
            if caps.contains(SeatCapability::Keyboard) {
                if state.keyboard.is_none() {
                    state.keyboard = Some(seat.get_keyboard(qh, ()));
                }
            } else if let Some(keyboard) = state.keyboard.take() {
                keyboard.release();
                if let Some(surface_id) = state.keyboard_surface.take() {
                    state.forget_surface(&surface_id);
                }
            }
            if caps.contains(SeatCapability::Touch) {
                if state.touch.is_none() {
                    state.touch = Some(seat.get_touch(qh, ()));
                }
            } else if let Some(touch) = state.touch.take() {
                touch.release();
            }
        }
    }
}

impl Dispatch<WlPointer, ()> for ActivationState {
    fn event(
        state: &mut Self,
        _proxy: &WlPointer,
        event: wl_pointer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter { surface, .. } => {
                state.pointer_surface = Some(surface.id());
            }
            wl_pointer::Event::Leave { surface, .. } => {
                state.forget_surface(&surface.id());
                state.pointer_surface = None;
            }
            wl_pointer::Event::Button { serial, .. } => {
                state.record_input(serial, state.pointer_surface.clone());
            }
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for ActivationState {
    fn event(
        state: &mut Self,
        _proxy: &WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Enter { surface, .. } => {
                state.keyboard_surface = Some(surface.id());
            }
            wl_keyboard::Event::Leave { surface, .. } => {
                state.forget_surface(&surface.id());
                state.keyboard_surface = None;
            }
            wl_keyboard::Event::Key { serial, .. } => {
                state.record_input(serial, state.keyboard_surface.clone());
            }
            _ => {}
        }
    }
}

impl Dispatch<WlTouch, ()> for ActivationState {
    fn event(
        state: &mut Self,
        _proxy: &WlTouch,
        event: wl_touch::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_touch::Event::Down {
            serial, surface, ..
        } = event
        {
            state.record_input(serial, Some(surface.id()));
        }
    }
}

// Thread-local singleton.

thread_local! {
    static INSTANCE: RefCell<Option<Rc<ActivationService>>> = const { RefCell::new(None) };
}

/// Creates xdg-activation tokens bound to the last input event on our surfaces.
pub struct ActivationService {
    state: Rc<RefCell<ActivationState>>,
    event_queue: Rc<RefCell<EventQueue<ActivationState>>>,
    qh: QueueHandle<ActivationState>,
}

impl ActivationService {
    /// Initialize the global singleton.
    ///
    /// Must be called on the main thread after the GDK display exists.
    /// Initializing before notification UI is shown ensures the private seat
    /// binding observes the relevant surface focus and input events. If the
    /// compositor lacks `xdg_activation_v1` the singleton stays `None`.
    pub fn init_global() {
        INSTANCE.with(|cell| {
            if cell.borrow().is_some() {
                return;
            }
            let svc = Self::try_init();
            *cell.borrow_mut() = svc.map(Rc::new);
        });
    }

    /// Get a reference to the global service, if available.
    pub fn global() -> Option<Rc<Self>> {
        INSTANCE.with(|cell| cell.borrow().clone())
    }

    fn try_init() -> Option<Self> {
        let gdk_display = gtk4::gdk::Display::default()?;
        let wayland_display = gdk_display
            .downcast::<gdk4_wayland::WaylandDisplay>()
            .ok()?;

        if !wayland_display.query_registry("xdg_activation_v1") {
            debug!("Compositor does not advertise xdg_activation_v1, activation tokens disabled");
            return None;
        }

        let connection = connection_from_gdk_display(&wayland_display)?;

        let mut event_queue: EventQueue<ActivationState> = connection.new_event_queue();
        let qh = event_queue.handle();
        let display = connection.display();
        let _registry = display.get_registry(&qh, ());

        let mut state = ActivationState::new();
        // Two roundtrips: discover globals, then let the seat's `capabilities`
        // event (triggered by the bind) arrive.
        for _ in 0..2 {
            if let Err(e) = event_queue.roundtrip(&mut state) {
                warn!("Activation service roundtrip failed: {e}");
                return None;
            }
        }
        let _ = event_queue.flush();

        if state.activation.is_none() || state.seat.is_none() {
            debug!("xdg_activation_v1 or wl_seat not bound, activation tokens disabled");
            return None;
        }

        debug!("Activation token service initialized");

        let svc = Self {
            state: Rc::new(RefCell::new(state)),
            event_queue: Rc::new(RefCell::new(event_queue)),
            qh,
        };
        install_event_dispatch(&svc.event_queue, &svc.state, "Activation");
        Some(svc)
    }

    /// Create an activation token tied to the most recent actionable input
    /// event on one of our surfaces (the notification click that got us here).
    ///
    /// Returns `None` if no input has been observed or the compositor completes
    /// the request without providing a token.
    ///
    /// This must be called synchronously from a handler for real user input. It
    /// performs a Wayland roundtrip without a timeout and may block the GTK
    /// thread if the compositor stops responding.
    pub fn create_token(&self) -> Option<String> {
        let mut eq = self.event_queue.borrow_mut();
        let mut state = self.state.borrow_mut();

        // Drain anything buffered so the click that triggered this call is in.
        if let Err(e) = eq.dispatch_pending(&mut *state) {
            warn!("Activation dispatch_pending failed: {e}");
        }

        let activation = state.activation.clone()?;
        let seat = state.seat.clone()?;
        let (serial, surface_id) = state.last_input.take()?;
        let surface = live_surface(&surface_id)?;

        state.pending_token = None;
        let token = activation.get_activation_token(&self.qh, ());
        token.set_serial(serial, &seat);
        token.set_surface(&surface.wl_surface);
        token.commit();

        if let Err(e) = eq.roundtrip(&mut *state) {
            warn!("Activation token roundtrip failed: {e}");
        }
        token.destroy();

        let result = state.pending_token.take();
        debug!(
            "Activation token created (serial={}, got_token={})",
            serial,
            result.is_some()
        );
        result.filter(|t| !t.is_empty())
    }
}
