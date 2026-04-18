//! Wayland background effect (blur) region hints.
//!
//! Uses the `ext-background-effect-v1` staging protocol to tell the compositor
//! exactly which region of each surface should be blurred, excluding shadows
//! and transparent padding. This is a **zero-cost hint** — if the compositor
//! has no blur configured, it is silently ignored.
//!
//! ## Architecture
//!
//! The service bridges GDK's Wayland connection with `wayland-client` objects.
//! It creates its own `EventQueue` on GDK's `wl_display` and integrates it
//! into the glib main loop via `unix_fd_add_local`.
//!
//! Per-surface `ExtBackgroundEffectSurfaceV1` objects are cached by
//! `ObjectId` to avoid the protocol error raised when creating duplicates.

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd};
use std::rc::Rc;

use gdk4_wayland::prelude::*;
use gtk4::glib;
use gtk4::prelude::*;
use tracing::{debug, trace, warn};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols::ext::background_effect::v1::client::{
    ext_background_effect_manager_v1::{self, Capability, ExtBackgroundEffectManagerV1},
    ext_background_effect_surface_v1::{self, ExtBackgroundEffectSurfaceV1},
};

// ── Dispatch state ──────────────────────────────────────────────────────────

/// Internal wayland-client dispatch state.
///
/// Holds the bound manager, compositor, and per-surface effect objects.
struct BlurState {
    /// The bound manager global (set after registry advertises it).
    manager: Option<ExtBackgroundEffectManagerV1>,
    /// The compositor global (needed to create `wl_region` objects).
    compositor: Option<WlCompositor>,
    /// Cached per-surface effect objects, keyed by `wl_surface` ObjectId.
    effects: HashMap<wayland_client::backend::ObjectId, ExtBackgroundEffectSurfaceV1>,
    /// Whether the compositor advertised blur capability.
    blur_capable: bool,
}

impl BlurState {
    fn new() -> Self {
        Self {
            manager: None,
            compositor: None,
            effects: HashMap::new(),
            blur_capable: false,
        }
    }
}

// ── Dispatch impls ──────────────────────────────────────────────────────────

impl Dispatch<wl_registry::WlRegistry, ()> for BlurState {
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
                "ext_background_effect_manager_v1" => {
                    debug!("Found ext_background_effect_manager_v1 v{version}");
                    let mgr: ExtBackgroundEffectManagerV1 =
                        registry.bind(name, version.min(1), qh, ());
                    state.manager = Some(mgr);
                }
                "wl_compositor" => {
                    let comp: WlCompositor = registry.bind(name, version.min(4), qh, ());
                    state.compositor = Some(comp);
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ExtBackgroundEffectManagerV1, ()> for BlurState {
    fn event(
        state: &mut Self,
        _proxy: &ExtBackgroundEffectManagerV1,
        event: ext_background_effect_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_background_effect_manager_v1::Event::Capabilities { flags } = event {
            let was_capable = state.blur_capable;
            // flags is WEnum<Capability> — extract the inner value and check for Blur.
            state.blur_capable = match flags {
                WEnum::Value(cap) => cap.contains(Capability::Blur),
                WEnum::Unknown(_) => false,
            };
            if state.blur_capable != was_capable {
                debug!(
                    "Background effect blur capability: {}",
                    if state.blur_capable {
                        "available"
                    } else {
                        "lost"
                    }
                );
            }
        }
    }
}

impl Dispatch<ExtBackgroundEffectSurfaceV1, ()> for BlurState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtBackgroundEffectSurfaceV1,
        _event: ext_background_effect_surface_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // This interface has no events.
    }
}

impl Dispatch<WlCompositor, ()> for BlurState {
    fn event(
        _state: &mut Self,
        _proxy: &WlCompositor,
        _event: wayland_client::protocol::wl_compositor::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // wl_compositor has no events.
    }
}

impl Dispatch<wayland_client::protocol::wl_region::WlRegion, ()> for BlurState {
    fn event(
        _state: &mut Self,
        _proxy: &wayland_client::protocol::wl_region::WlRegion,
        _event: wayland_client::protocol::wl_region::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // wl_region has no events.
    }
}

// ── Rounded-rect scanline rasterization ─────────────────────────────────────

/// Add a rounded rectangle to a `wl_region` using scanline rasterization.
///
/// Uses `round(exact_inset)` per row — nearest-integer assignment minimises
/// total error and max adjacent delta compared to ceil, floor, or Bresenham
/// for the filled-region use case. Some flat runs at the bottom of the arc
/// are geometrically unavoidable (pigeonhole principle) but are imperceptible
/// there since the circle is nearly vertical.
///
/// If `radius` is zero or the radius is too large for the dimensions, falls
/// back to a single rectangle.
fn add_rounded_rect_to_region(
    region: &wayland_client::protocol::wl_region::WlRegion,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    radius: i32,
) {
    if radius <= 0 || radius * 2 > width || radius * 2 > height {
        region.add(x, y, width, height);
        return;
    }

    // Central rectangle spanning the full width, excluding the top and bottom
    // radius strips.
    region.add(x, y + radius, width, height - 2 * radius);

    let r = radius as f64;
    for i in 0..radius as usize {
        let dy = r - 0.5 - i as f64;
        let inset = if dy < 0.0 {
            0
        } else {
            (r - (r * r - dy * dy).sqrt()).round() as i32
        };
        let row_w = (width - 2 * inset).max(1);
        region.add(x + inset, y + i as i32, row_w, 1);
        region.add(x + inset, y + height - 1 - i as i32, row_w, 1);
    }
}

// ── Thread-local singleton ──────────────────────────────────────────────────

thread_local! {
    static INSTANCE: RefCell<Option<Rc<BackgroundEffectManager>>> = const { RefCell::new(None) };
}

/// Manages `ext-background-effect-v1` blur region hints for all vibepanel surfaces.
pub struct BackgroundEffectManager {
    state: RefCell<BlurState>,
    event_queue: RefCell<EventQueue<BlurState>>,
    qh: QueueHandle<BlurState>,
    /// Whether the protocol is available on this compositor.
    available: bool,
}

impl BackgroundEffectManager {
    /// Initialize the global singleton.
    ///
    /// Must be called on the main thread after `gtk4::gdk::Display::default()` is available.
    /// If the compositor does not advertise `ext_background_effect_manager_v1`, the
    /// service becomes inert — all `apply_blur_region` calls are no-ops.
    pub fn init_global() {
        INSTANCE.with(|cell| {
            if cell.borrow().is_some() {
                return;
            }

            let mgr = Self::try_init();
            *cell.borrow_mut() = mgr.map(Rc::new);
        });
    }

    /// Get a reference to the global manager, if available.
    pub fn global() -> Option<Rc<Self>> {
        INSTANCE.with(|cell| cell.borrow().clone())
    }

    /// Get the `wayland_client::Connection` that GDK uses internally.
    ///
    /// `gdk4_wayland::WaylandDisplay::connection()` is `pub(crate)` so we can't call
    /// it directly. Instead we call `wl_display()` which internally creates and
    /// caches the Connection in GObject qdata, then extract the Backend from the
    /// returned proxy to reconstruct the *same* Connection.
    ///
    /// This is critical: creating a second `Backend::from_foreign_display()` would
    /// allocate a separate libwayland event queue, and roundtrips on it can consume
    /// events from the shared fd that GDK expects to read, causing missed
    /// layer-shell configure events (bar appears in middle of screen).
    fn connection_from_gdk_display(
        wayland_display: &gdk4_wayland::WaylandDisplay,
    ) -> Option<Connection> {
        use wayland_client::Proxy;

        // wl_display() internally calls the private connection() which creates
        // and caches the Connection. The returned WlDisplay proxy holds a
        // WeakBackend reference to that same Connection's backend.
        let wl_display = wayland_display.wl_display()?;
        let backend = wl_display.backend().upgrade()?;
        Some(Connection::from_backend(backend))
    }

    /// Attempt to initialize.
    fn try_init() -> Option<Self> {
        // Check we're on a Wayland display.
        let gdk_display = gtk4::gdk::Display::default()?;
        let wayland_display = gdk_display
            .downcast::<gdk4_wayland::WaylandDisplay>()
            .ok()?;

        // Quick check: does the compositor advertise this protocol at all?
        if !wayland_display.query_registry("ext_background_effect_manager_v1") {
            debug!(
                "Compositor does not advertise ext_background_effect_manager_v1, blur hints disabled"
            );
            return None;
        }

        debug!("ext_background_effect_manager_v1 found in registry, initializing blur service");

        // Build a wayland-client Connection from GDK's foreign wl_display.
        let connection = Self::connection_from_gdk_display(&wayland_display)?;

        // Create our own event queue on GDK's connection.
        let mut event_queue: EventQueue<BlurState> = connection.new_event_queue();
        let qh = event_queue.handle();

        // Get registry from the display on our queue.
        let display = connection.display();
        let _registry = display.get_registry(&qh, ());

        // Initial roundtrip to discover globals.
        let mut state = BlurState::new();
        if let Err(e) = event_queue.roundtrip(&mut state) {
            warn!("Failed blur service roundtrip: {e}");
            return None;
        }

        if state.manager.is_none() {
            debug!("ext_background_effect_manager_v1 not bound after roundtrip");
            return None;
        }

        // Second roundtrip to receive the capabilities event.
        if let Err(e) = event_queue.roundtrip(&mut state) {
            warn!("Failed blur service capabilities roundtrip: {e}");
            return None;
        }

        debug!(
            "Blur service initialized (blur_capable={}, compositor={:?})",
            state.blur_capable,
            state.compositor.is_some()
        );

        let mgr = Self {
            state: RefCell::new(state),
            event_queue: RefCell::new(event_queue),
            qh,
            available: true,
        };

        // Install fd watcher to dispatch events (e.g. capability changes).
        mgr.install_event_dispatch();

        Some(mgr)
    }

    /// Install a glib fd watcher to dispatch wayland events for our queue.
    fn install_event_dispatch(&self) {
        // We need a raw fd for glib::unix_fd_add_local.
        // The fd is borrowed from the event queue which lives as long as Self (thread-local singleton).
        let eq_ref = self.event_queue.borrow().as_fd().as_raw_fd();

        // Use the global accessor from inside the callback to avoid lifetime issues.
        glib::unix_fd_add_local(eq_ref, glib::IOCondition::IN, move |_fd, _cond| {
            INSTANCE.with(|cell| {
                let borrow = cell.borrow();
                let Some(mgr) = borrow.as_ref() else {
                    return glib::ControlFlow::Break;
                };

                let mut eq = mgr.event_queue.borrow_mut();
                let mut st = mgr.state.borrow_mut();

                if let Err(e) = eq.dispatch_pending(&mut *st) {
                    warn!("Blur event dispatch error: {e}");
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
                            warn!("Blur wayland read error: {e}");
                        }
                    }
                }

                let _ = eq.flush();
                glib::ControlFlow::Continue
            })
        });
    }

    /// Apply a blur region hint to the given window's surface.
    ///
    /// `shadow_margin` is the padding (in surface-local px) between the layer-shell
    /// surface edge and the visible content. The margins are applied asymmetrically
    /// to match `SurfaceStyleManager::apply_shadow_margins`: the bar-adjacent side
    /// gets 0 margin (content is flush against the bar), while the other three
    /// sides are inset by `shadow_margin`.
    ///
    /// If the surface has no size yet (first map), this schedules a one-shot idle
    /// retry so the blur region is applied once GTK has committed dimensions.
    ///
    /// This is fire-and-forget: the region is double-buffered and applied on
    /// GTK's next `wl_surface.commit`.
    pub fn apply_blur_region(&self, window: &gtk4::ApplicationWindow, shadow_margin: i32) {
        if !self.available {
            return;
        }

        // Get the wl_surface from the GDK surface.
        let Some(native) = window.native() else {
            trace!("No native for window, skipping blur");
            return;
        };
        let Some(gdk_surface) = native.surface() else {
            trace!("No GDK surface for window, skipping blur");
            return;
        };
        let wayland_surface = match gdk_surface.downcast::<gdk4_wayland::WaylandSurface>() {
            Ok(s) => s,
            Err(_) => return,
        };
        let Some(wl_surface) = wayland_surface.wl_surface() else {
            trace!("No wl_surface for window, skipping blur");
            return;
        };

        // Use fully-qualified syntax to avoid clash with gtk4::prelude's id() methods.
        let surface_id =
            <wayland_client::protocol::wl_surface::WlSurface as wayland_client::Proxy>::id(
                &wl_surface,
            );

        // Get or create the effect object for this surface.
        // Extract manager clone before entry API to avoid borrow conflict.
        let mut state = self.state.borrow_mut();

        let (Some(manager), Some(compositor)) = (&state.manager, &state.compositor) else {
            return;
        };
        let manager = manager.clone();
        let compositor = compositor.clone();

        let effect = state
            .effects
            .entry(surface_id)
            .or_insert_with(|| {
                debug!("Creating background effect object for surface");
                manager.get_background_effect(&wl_surface, &self.qh, ())
            })
            .clone();

        drop(state);

        // Get the surface dimensions from the WaylandSurface (which derefs to gdk::Surface).
        let width = wayland_surface.width();
        let height = wayland_surface.height();

        if width <= 0 || height <= 0 {
            // Surface not sized yet (common on first map). Schedule an idle retry
            // so we apply the region once GTK has committed actual dimensions.
            trace!("Surface has no size yet, deferring blur region to idle");
            let win_clone = window.clone();
            glib::idle_add_local_once(move || {
                if let Some(blur) = Self::global() {
                    blur.apply_blur_region(&win_clone, shadow_margin);
                }
            });
            return;
        }

        // Run the base margin through SurfaceStyleManager to respect the
        // shadows_enabled flag — when shadows are off, the actual margin is 0
        // even though the caller passes the base constant.
        let effective_margin = if shadow_margin > 0 {
            crate::services::surfaces::SurfaceStyleManager::global().shadow_margin(shadow_margin)
        } else {
            0
        };

        // Compute per-side margins matching `SurfaceStyleManager::apply_shadow_margins`:
        // the bar-adjacent side gets 0 margin (content is flush against the bar),
        // the other three sides get the full effective margin.
        let m = effective_margin;
        let (margin_top, margin_bottom, margin_start, margin_end) = if m > 0 {
            let is_bottom =
                crate::services::config_manager::ConfigManager::global().bar_is_bottom();
            if is_bottom {
                (m, 0, m, m) // bar at bottom → top/start/end get margin, bottom = 0
            } else {
                (0, m, m, m) // bar at top → bottom/start/end get margin, top = 0
            }
        } else {
            (0, 0, 0, 0)
        };

        let region = compositor.create_region(&self.qh, ());
        let x = margin_start;
        let y = margin_top;
        let w = width - margin_start - margin_end;
        let h = height - margin_top - margin_bottom;

        // Use rounded corners for surfaces with shadow margins (popovers, QS).
        // The bar (shadow_margin == 0) has no visible border-radius for blur.
        let radius = if shadow_margin > 0 {
            crate::services::config_manager::ConfigManager::global().surface_border_radius() as i32
        } else {
            0
        };

        add_rounded_rect_to_region(&region, x, y, w, h, radius);

        effect.set_blur_region(Some(&region));

        // The region object can be destroyed immediately (copy semantics).
        region.destroy();

        // Flush to ensure the request reaches the compositor.
        if let Ok(eq) = self.event_queue.try_borrow() {
            let _ = eq.flush();
        }

        debug!(
            "Applied blur region: {}x{} at ({},{}) r={} margins t={} b={} s={} e={} (surface {}x{})",
            w, h, x, y, radius, margin_top, margin_bottom, margin_start, margin_end, width, height
        );

        // Install a resize watcher (once per window) so the blur region
        // is re-applied whenever the surface dimensions change — e.g. when
        // a Revealer expands and the layer-shell surface reconfigures.
        const BLUR_RESIZE_WATCHED_KEY: &str = "vibepanel-blur-resize-watched";
        unsafe {
            if window.data::<bool>(BLUR_RESIZE_WATCHED_KEY).is_none() {
                window.set_data(BLUR_RESIZE_WATCHED_KEY, true);
                let win_clone = window.clone();
                if let Some(gdk_surface) = window.native().and_then(|n| n.surface()) {
                    gdk_surface.connect_notify_local(Some("width"), move |_, _| {
                        if let Some(blur) = BackgroundEffectManager::global() {
                            blur.apply_blur_region(&win_clone, shadow_margin);
                        }
                    });
                }
            }
        }
    }

    /// Remove the blur region for a window (e.g. on destroy).
    #[allow(dead_code)]
    pub fn remove_blur_region(&self, window: &gtk4::ApplicationWindow) {
        if !self.available {
            return;
        }

        let Some(native) = window.native() else {
            return;
        };
        let Some(gdk_surface) = native.surface() else {
            return;
        };
        let wayland_surface = match gdk_surface.downcast::<gdk4_wayland::WaylandSurface>() {
            Ok(s) => s,
            Err(_) => return,
        };
        let Some(wl_surface) = wayland_surface.wl_surface() else {
            return;
        };

        let surface_id =
            <wayland_client::protocol::wl_surface::WlSurface as wayland_client::Proxy>::id(
                &wl_surface,
            );
        let mut state = self.state.borrow_mut();
        if let Some(effect) = state.effects.remove(&surface_id) {
            effect.destroy();
            debug!("Removed blur region for surface");
        }
    }
}
