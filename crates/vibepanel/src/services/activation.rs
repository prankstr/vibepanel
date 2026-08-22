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
use std::os::fd::{AsFd, AsRawFd};
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::Cast;
use tracing::{debug, warn};
use wayland_client::protocol::wl_keyboard::{self, WlKeyboard};
use wayland_client::protocol::wl_pointer::{self, WlPointer};
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat::{self, Capability as SeatCapability, WlSeat};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::protocol::wl_touch::{self, WlTouch};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::xdg::activation::v1::client::{
    xdg_activation_token_v1::{self, XdgActivationTokenV1},
    xdg_activation_v1::XdgActivationV1,
};

/// Internal wayland-client dispatch state.
struct ActivationState {
    activation: Option<XdgActivationV1>,
    /// first advertised seat only; multi-seat setups are unicorns.
    seat: Option<WlSeat>,
    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,
    touch: Option<WlTouch>,
    /// Surface currently under the pointer / holding keyboard focus (ours only).
    pointer_surface: Option<WlSurface>,
    keyboard_surface: Option<WlSurface>,
    /// Serial + surface of the most recent input event that carried a serial.
    last_input: Option<(u32, WlSurface)>,
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

    fn record_input(&mut self, serial: u32, surface: Option<&WlSurface>) {
        if let Some(surface) = surface {
            self.last_input = Some((serial, surface.clone()));
        }
    }

    fn forget_surface(&mut self, surface: &WlSurface) {
        if self
            .last_input
            .as_ref()
            .is_some_and(|(_, remembered)| remembered == surface)
        {
            self.last_input = None;
        }
    }
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
                if let Some(surface) = state.pointer_surface.take() {
                    state.forget_surface(&surface);
                }
            }
            if caps.contains(SeatCapability::Keyboard) {
                if state.keyboard.is_none() {
                    state.keyboard = Some(seat.get_keyboard(qh, ()));
                }
            } else if let Some(keyboard) = state.keyboard.take() {
                keyboard.release();
                if let Some(surface) = state.keyboard_surface.take() {
                    state.forget_surface(&surface);
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
                state.pointer_surface = Some(surface);
            }
            wl_pointer::Event::Leave { surface, .. } => {
                state.forget_surface(&surface);
                state.pointer_surface = None;
            }
            wl_pointer::Event::Button { serial, .. } => {
                let surface = state.pointer_surface.clone();
                state.record_input(serial, surface.as_ref());
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
                state.keyboard_surface = Some(surface);
            }
            wl_keyboard::Event::Leave { surface, .. } => {
                state.forget_surface(&surface);
                state.keyboard_surface = None;
            }
            wl_keyboard::Event::Key { serial, .. } => {
                let surface = state.keyboard_surface.clone();
                state.record_input(serial, surface.as_ref());
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
            state.record_input(serial, Some(&surface));
        }
    }
}

// Thread-local singleton.

thread_local! {
    static INSTANCE: RefCell<Option<Rc<ActivationService>>> = const { RefCell::new(None) };
}

/// Creates xdg-activation tokens bound to the last input event on our surfaces.
pub struct ActivationService {
    state: RefCell<ActivationState>,
    event_queue: RefCell<EventQueue<ActivationState>>,
    qh: QueueHandle<ActivationState>,
}

impl ActivationService {
    /// Initialize the global singleton.
    ///
    /// Must be called on the main thread after the GDK display exists. If the
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

        // Reuse GDK's connection; see BackgroundEffectManager for why a
        // separate Backend must not be created.
        let wl_display = wayland_display.wl_display()?;
        let backend = wl_display.backend().upgrade()?;
        let connection = Connection::from_backend(backend);

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
            state: RefCell::new(state),
            event_queue: RefCell::new(event_queue),
            qh,
        };
        svc.install_event_dispatch();
        Some(svc)
    }

    /// Install a glib fd watcher to dispatch wayland events for our queue.
    ///
    /// Input events mirrored to our seat binding land in this queue whenever
    /// the socket is read; draining them keeps the serial fresh and the buffer
    /// bounded.
    fn install_event_dispatch(&self) {
        let raw_fd = self.event_queue.borrow().as_fd().as_raw_fd();

        glib::unix_fd_add_local(raw_fd, glib::IOCondition::IN, move |_fd, _cond| {
            INSTANCE.with(|cell| {
                let borrow = cell.borrow();
                let Some(svc) = borrow.as_ref() else {
                    return glib::ControlFlow::Break;
                };

                let mut eq = svc.event_queue.borrow_mut();
                let mut st = svc.state.borrow_mut();

                if let Err(e) = eq.dispatch_pending(&mut *st) {
                    warn!("Activation event dispatch error: {e}");
                    return glib::ControlFlow::Continue;
                }

                if let Some(guard) = eq.prepare_read() {
                    match guard.read() {
                        Ok(_) => {
                            let _ = eq.dispatch_pending(&mut *st);
                        }
                        Err(wayland_client::backend::WaylandError::Io(io_err))
                            if io_err.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(e) => {
                            warn!("Activation wayland read error: {e}");
                        }
                    }
                }

                let _ = eq.flush();
                glib::ControlFlow::Continue
            })
        });
    }

    /// Create an activation token tied to the most recent actionable input
    /// event on one of our surfaces (the notification click that got us here).
    ///
    /// Returns `None` if no input has been observed yet or the compositor
    /// never answers.
    ///
    /// This must be called synchronously from a handler for real user input.
    /// GDK owns the recorded surfaces, so their liveness cannot be validated.
    pub fn create_token(&self) -> Option<String> {
        let mut eq = self.event_queue.borrow_mut();
        let mut state = self.state.borrow_mut();

        // Drain anything buffered so the click that triggered this call is in.
        if let Err(e) = eq.dispatch_pending(&mut *state) {
            warn!("Activation dispatch_pending failed: {e}");
        }

        let activation = state.activation.clone()?;
        let seat = state.seat.clone()?;
        let (serial, surface) = state.last_input.take()?;

        state.pending_token = None;
        let token = activation.get_activation_token(&self.qh, ());
        token.set_serial(serial, &seat);
        token.set_surface(&surface);
        token.commit();

        // One roundtrip normally suffices; the compositor sends `done` while
        // processing the commit. Cap at 2 to stay bounded.
        for _ in 0..2 {
            if let Err(e) = eq.roundtrip(&mut *state) {
                warn!("Activation token roundtrip failed: {e}");
                break;
            }
            if state.pending_token.is_some() {
                break;
            }
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
