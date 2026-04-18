//! Wayland background effect (blur) region hints.
//!
//! Uses the `ext-background-effect-v1` staging protocol to tell the compositor
//! exactly which region of each surface should be blurred, excluding shadows
//! and transparent padding. This is a **zero-cost hint** — if the compositor
//! has no blur configured, it is silently ignored.
//!
//! ## Surface scope
//!
//! Covers all vibepanel-managed surfaces with visible backgrounds:
//! bar (opaque and island modes), layer-shell popovers, Quick Settings,
//! notification toasts, OSD, tray menus, and the media pop-out window.
//!
//! Intentionally excluded:
//! - **Quick Settings row sub-menus** (`gtk4::Popover` / `xdg_popup`) — wifi,
//!   bluetooth, and power card context menus.  These are subordinate menus
//!   inside an already-blurred layer-shell surface; the visual benefit is
//!   negligible and they are not "top-level" surfaces in the user's view.
//! - **Tooltips** (`services/tooltip.rs`) — layer-shell surfaces but tiny and
//!   ephemeral; blur would not be visible behind a single-line label.
//!
//! Note: tray menus and the media pop-out are `xdg_popup` / XDG-toplevel
//! surfaces respectively.  Whether the compositor actually renders blur for
//! them depends on compositor support.  niri supports both as of PR #3483
//! (`wip/branch`).  On compositors without support the hints are silently
//! ignored — no harm done.
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
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::ext::background_effect::v1::client::{
    ext_background_effect_manager_v1::{self, ExtBackgroundEffectManagerV1},
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
}

impl BlurState {
    fn new() -> Self {
        Self {
            manager: None,
            compositor: None,
            effects: HashMap::new(),
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
        _state: &mut Self,
        _proxy: &ExtBackgroundEffectManagerV1,
        _event: ext_background_effect_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // No events we need to handle.
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
/// If `radius` is zero or the dimensions are too small to accommodate it,
/// clamps to a pill shape or falls back to a plain rectangle.
/// Non-positive `width` or `height` are silently ignored (no `wl_region.add`
/// call is made — per Wayland protocol, non-positive dimensions are invalid).
fn add_rounded_rect_to_region(
    region: &wayland_client::protocol::wl_region::WlRegion,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    radius: i32,
) {
    if width <= 0 || height <= 0 {
        return;
    }
    if radius <= 0 {
        region.add(x, y, width, height);
        return;
    }
    // Clamp to half the smallest dimension so oversized radii produce a
    // pill shape instead of a plain rectangle.
    let radius = radius.min(width / 2).min(height / 2);
    if radius <= 0 {
        region.add(x, y, width, height);
        return;
    }

    // Central rectangle spanning the full width, excluding the top and bottom
    // radius strips.
    if height > 2 * radius {
        region.add(x, y + radius, width, height - 2 * radius);
    }

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

/// Compute effective shadow margins and border radius for a blur region.
///
/// Resolves the base `shadow_margin` through `SurfaceStyleManager` (respecting
/// the `shadows_enabled` flag) and applies the asymmetric layout where the
/// bar-adjacent side gets 0 margin.
///
/// Returns `(margin_top, margin_bottom, margin_start, margin_end, radius)`.
fn compute_shadow_layout(shadow_margin: i32) -> (i32, i32, i32, i32, i32) {
    let effective_margin = if shadow_margin > 0 {
        crate::services::surfaces::SurfaceStyleManager::global().shadow_margin(shadow_margin)
    } else {
        0
    };

    let m = effective_margin;
    let (margin_top, margin_bottom, margin_start, margin_end) = if m > 0 {
        let is_bottom = crate::services::config_manager::ConfigManager::global().bar_is_bottom();
        if is_bottom {
            (m, 0, m, m) // bar at bottom → top/start/end get margin, bottom = 0
        } else {
            (0, m, m, m) // bar at top → bottom/start/end get margin, top = 0
        }
    } else {
        (0, 0, 0, 0)
    };

    let radius = if shadow_margin > 0 {
        crate::services::config_manager::ConfigManager::global().surface_border_radius() as i32
    } else {
        0
    };

    (margin_top, margin_bottom, margin_start, margin_end, radius)
}

// ── Surface info helper ─────────────────────────────────────────────────────

/// Resolved Wayland surface info for a GTK widget.
///
/// Extracts the `wl_surface`, GDK `WaylandSurface`, and a stable `ObjectId`
/// for use as a cache key.  Avoids duplicating the same 15-line lookup
/// boilerplate across every method that needs to interact with a surface.
struct SurfaceInfo {
    wl_surface: wayland_client::protocol::wl_surface::WlSurface,
    wayland_surface: gdk4_wayland::WaylandSurface,
    surface_id: wayland_client::backend::ObjectId,
}

impl SurfaceInfo {
    /// Resolve surface info from any widget that has a native surface.
    fn from_widget(widget: &impl gtk4::prelude::IsA<gtk4::Widget>) -> Option<Self> {
        let native = widget.as_ref().native()?;
        let gdk_surface = native.surface()?;
        let wayland_surface = gdk_surface
            .downcast::<gdk4_wayland::WaylandSurface>()
            .ok()?;
        let wl_surface = wayland_surface.wl_surface()?;
        let surface_id =
            <wayland_client::protocol::wl_surface::WlSurface as wayland_client::Proxy>::id(
                &wl_surface,
            );
        Some(Self {
            wl_surface,
            wayland_surface,
            surface_id,
        })
    }

    /// Surface width in logical pixels.
    fn width(&self) -> i32 {
        self.wayland_surface.width()
    }

    /// Surface height in logical pixels.
    fn height(&self) -> i32 {
        self.wayland_surface.height()
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
}

impl BackgroundEffectManager {
    /// Initialize the global singleton.
    ///
    /// Must be called on the main thread after `gtk4::gdk::Display::default()` is available.
    /// If the compositor does not advertise `ext_background_effect_manager_v1`, the
    /// singleton remains `None` and all callers' `global()` checks become no-ops.
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

        debug!(
            "Blur service initialized (compositor={:?})",
            state.compositor.is_some()
        );

        let mgr = Self {
            state: RefCell::new(state),
            event_queue: RefCell::new(event_queue),
            qh,
        };

        // Install fd watcher to dispatch incoming protocol events.
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

    /// Get or create the per-surface effect object and return it alongside
    /// a cloned compositor reference.
    ///
    /// The effect is cached by `wl_surface` `ObjectId` to avoid the protocol
    /// error raised when creating duplicates.
    fn get_or_create_effect(
        &self,
        info: &SurfaceInfo,
    ) -> Option<(ExtBackgroundEffectSurfaceV1, WlCompositor)> {
        let mut state = self.state.borrow_mut();
        let (Some(manager), Some(compositor)) = (&state.manager, &state.compositor) else {
            return None;
        };
        let manager = manager.clone();
        let compositor = compositor.clone();

        let effect = state
            .effects
            .entry(info.surface_id.clone())
            .or_insert_with(|| {
                debug!("Creating background effect object for surface");
                manager.get_background_effect(&info.wl_surface, &self.qh, ())
            })
            .clone();

        Some((effect, compositor))
    }

    /// Flush the event queue so requests reach the compositor promptly.
    fn flush(&self) {
        if let Ok(eq) = self.event_queue.try_borrow() {
            let _ = eq.flush();
        }
    }

    /// Install a one-shot resize watcher on a window's GDK surface.
    ///
    /// We watch only `width`, which is sufficient: GDK's internal
    /// `_gdk_surface_update_size()` emits both `notify::width` and
    /// `notify::height` whenever *any* dimension changes, so height-only
    /// configures still fire `notify::width`.
    ///
    /// The watcher is installed at most once per `key` per window.
    fn install_resize_watcher(
        window: &gtk4::ApplicationWindow,
        key: &'static str,
        on_resize: impl Fn() + 'static,
    ) {
        unsafe {
            if window.data::<bool>(key).is_some() {
                return;
            }
            window.set_data(key, true);
        }
        if let Some(gdk_surface) = window.native().and_then(|n| n.surface()) {
            gdk_surface.connect_notify_local(Some("width"), move |_, _| on_resize());
        }
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
        let Some(info) = SurfaceInfo::from_widget(window) else {
            trace!("No wl_surface for window, skipping blur");
            return;
        };

        let width = info.width();
        let height = info.height();

        if width <= 0 || height <= 0 {
            // Surface not sized yet (common on first map). Schedule a one-shot
            // idle retry so we apply the region once GTK has committed actual
            // dimensions. A set_data guard prevents stacking multiple retries if
            // this is called several times before the surface gets sized.
            const RETRY_KEY: &str = "vibepanel-blur-region-retry-pending";
            if unsafe { window.data::<bool>(RETRY_KEY) }.is_some() {
                trace!("Idle retry already pending, skipping duplicate");
                return;
            }
            unsafe { window.set_data(RETRY_KEY, true) };
            trace!("Surface has no size yet, deferring blur region to idle");
            let win_clone = window.clone();
            glib::idle_add_local_once(move || {
                unsafe { win_clone.steal_data::<bool>(RETRY_KEY) };
                if crate::services::config_manager::ConfigManager::global().blur_enabled()
                    && let Some(blur) = Self::global()
                {
                    blur.apply_blur_region(&win_clone, shadow_margin);
                }
            });
            return;
        }

        let Some((effect, compositor)) = self.get_or_create_effect(&info) else {
            return;
        };

        let (margin_top, margin_bottom, margin_start, margin_end, radius) =
            compute_shadow_layout(shadow_margin);

        let region = compositor.create_region(&self.qh, ());
        let x = margin_start;
        let y = margin_top;
        let w = width - margin_start - margin_end;
        let h = height - margin_top - margin_bottom;

        add_rounded_rect_to_region(&region, x, y, w, h, radius);

        effect.set_blur_region(Some(&region));
        region.destroy();
        self.flush();

        debug!(
            "Applied blur region: {}x{} at ({},{}) r={} margins t={} b={} s={} e={} (surface {}x{})",
            w, h, x, y, radius, margin_top, margin_bottom, margin_start, margin_end, width, height
        );

        // Install a resize watcher (once per window) so the blur region
        // is re-applied whenever the surface dimensions change — e.g. when
        // a Revealer expands and the layer-shell surface reconfigures.
        let win_clone = window.clone();
        Self::install_resize_watcher(window, "vibepanel-blur-resize-watched", move || {
            if crate::services::config_manager::ConfigManager::global().blur_enabled()
                && let Some(blur) = BackgroundEffectManager::global()
            {
                blur.apply_blur_region(&win_clone, shadow_margin);
            }
        });
    }

    /// Apply a blur region for the bar surface.
    ///
    /// When the bar has a non-zero background opacity (translucent/opaque bar),
    /// the blur region is derived from `bar_box`'s allocation within the surface
    /// via `compute_bounds`.  This correctly excludes the transparent
    /// `.bar-margin-spacer` and `.bar-shell-inner` padding areas that surround
    /// the visible bar background when `screen_margin > 0`.
    ///
    /// When the bar background is fully transparent (opacity == 0.0, islands mode),
    /// individual widget island regions are blurred instead via
    /// `apply_bar_island_blur_regions`.
    ///
    /// Called from bar.rs on `connect_map` and from the `on_theme_change` handler.
    pub fn apply_bar_blur_region(
        &self,
        window: &gtk4::ApplicationWindow,
        bar_box: &impl gtk4::prelude::IsA<gtk4::Widget>,
    ) {
        let bar_opacity =
            crate::services::config_manager::ConfigManager::global().bar_background_opacity();

        if bar_opacity == 0.0 {
            // Islands mode: defer to per-island path (called separately by the
            // layout allocate callback once island bounds are known).
            return;
        }

        // Opaque/translucent bar: blur only the bar_box bounds, not the full surface.
        // Using apply_blur_surface so compute_bounds accounts for any margin/padding.
        let radius =
            crate::services::config_manager::ConfigManager::global().bar_border_radius() as i32;

        self.apply_blur_surface(window, bar_box, radius);
    }

    /// Apply blur regions for individual widget islands on a transparent bar.
    ///
    /// `islands` is a slice of `(x, y, width, height)` tuples in surface-local
    /// logical coordinates, one per visible `.widget-wrapper` island. Each island
    /// gets a rounded rectangle region matching the widget border radius.
    ///
    /// Called from bar.rs via the `CenterPriorityLayout` allocate callback.
    pub fn apply_bar_island_blur_regions(
        &self,
        window: &gtk4::ApplicationWindow,
        islands: &[(i32, i32, i32, i32)],
    ) {
        if islands.is_empty() {
            return;
        }

        let Some(info) = SurfaceInfo::from_widget(window) else {
            return;
        };

        let Some((effect, compositor)) = self.get_or_create_effect(&info) else {
            return;
        };

        let radius =
            crate::services::config_manager::ConfigManager::global().widget_border_radius() as i32;

        let region = compositor.create_region(&self.qh, ());
        for &(x, y, w, h) in islands {
            add_rounded_rect_to_region(&region, x, y, w, h, radius);
        }

        effect.set_blur_region(Some(&region));
        region.destroy();
        self.flush();

        debug!(
            "Applied bar island blur regions: {} islands, r={}",
            islands.len(),
            radius
        );
    }

    /// Remove the blur region for a window (e.g. on destroy).
    pub fn remove_blur_region(&self, window: &impl gtk4::prelude::IsA<gtk4::Widget>) {
        let Some(info) = SurfaceInfo::from_widget(window) else {
            return;
        };

        let mut state = self.state.borrow_mut();
        if let Some(effect) = state.effects.remove(&info.surface_id) {
            effect.destroy();
            debug!("Removed blur region for surface");
        }
        drop(state);

        // Flush so the destroy request reaches the compositor promptly.
        // The change takes effect on the next wl_surface.commit (driven by GTK).
        self.flush();
    }

    /// Apply a blur region that tracks the ScaleBox grow-in animation.
    ///
    /// During the open animation the ScaleBox clips its child to a centered rect
    /// whose size is `content_size * scale`.  This method sets the blur region to
    /// match that clip so the compositor blur grows in sync with the visual.
    ///
    /// `scale` should be the current ScaleBox scale (ANIM_SCALE_FROM → 1.0).
    /// At `scale == 1.0` this produces the same region as `apply_blur_region`.
    pub fn apply_blur_region_animated(
        &self,
        window: &gtk4::ApplicationWindow,
        shadow_margin: i32,
        scale: f64,
    ) {
        let Some(info) = SurfaceInfo::from_widget(window) else {
            return;
        };

        let Some((effect, compositor)) = self.get_or_create_effect(&info) else {
            return;
        };

        let width = info.width();
        let height = info.height();

        if width <= 0 || height <= 0 {
            return;
        }

        let (margin_top, margin_bottom, margin_start, margin_end, radius) =
            compute_shadow_layout(shadow_margin);

        // Content area within shadow margins (= ScaleBox allocation).
        let content_w = (width - margin_start - margin_end) as f64;
        let content_h = (height - margin_top - margin_bottom) as f64;

        // ScaleBox clips to a centered rect of size content * scale.
        let scaled_w = content_w * scale;
        let scaled_h = content_h * scale;
        let dx = (content_w - scaled_w) / 2.0;
        let dy = (content_h - scaled_h) / 2.0;

        // Final rect in surface coordinates.
        let x = margin_start as f64 + dx;
        let y = margin_top as f64 + dy;

        let region = compositor.create_region(&self.qh, ());
        add_rounded_rect_to_region(
            &region,
            x.round() as i32,
            y.round() as i32,
            scaled_w.round() as i32,
            scaled_h.round() as i32,
            radius,
        );

        effect.set_blur_region(Some(&region));
        region.destroy();
        self.flush();
    }

    /// Apply a blur region matching a content widget's allocation within its surface.
    ///
    /// Designed for surfaces without explicit shadow margins (OSD, notification
    /// toast, tray menu popover, media pop-out) where the surface may be
    /// slightly larger than the visible content due to CSS box-shadow expansion.
    /// The `content` widget's allocation provides the exact bounds to blur.
    ///
    /// `surface_root` is any widget whose `GtkNative` owns the `wl_surface`
    /// (a `gtk4::Window`, `gtk4::ApplicationWindow`, or `gtk4::Popover`);
    /// `content` is the child widget whose allocation defines the blur region;
    /// `radius` is the corner radius.
    ///
    /// On first map the Wayland surface may still be a 1×1 placeholder before
    /// the compositor sends configure.  In that case, a one-shot watcher on
    /// the GDK surface's `width` property defers the apply until configure
    /// arrives and layout completes.
    pub fn apply_blur_surface(
        &self,
        surface_root: &impl gtk4::prelude::IsA<gtk4::Widget>,
        content: &impl gtk4::prelude::IsA<gtk4::Widget>,
        radius: i32,
    ) {
        let Some(info) = SurfaceInfo::from_widget(surface_root) else {
            debug!("apply_blur_surface: no wl_surface, skipping");
            return;
        };

        let width = info.width();
        let height = info.height();

        let surface_root_widget = surface_root.as_ref();
        let content_widget = content.as_ref();

        // Validate that the surface has a real size (not the initial 1×1
        // placeholder) and that compute_bounds returns sensible values.
        let surface_ready = width > 1 && height > 1;
        let bounds = content_widget.compute_bounds(surface_root_widget);
        let bounds_valid = bounds
            .as_ref()
            .is_some_and(|b| b.x() >= 0.0 && b.y() >= 0.0 && b.width() > 0.0 && b.height() > 0.0);

        if !surface_ready || !bounds_valid {
            debug!(
                "apply_blur_surface: not ready (surface {}x{}, bounds {:?}), deferring",
                width, height, bounds
            );
            // Watch `width` (covers height-only changes too — see
            // install_resize_watcher doc).  Defer to idle so the GTK layout
            // pass completes before we read compute_bounds.
            if let Some(gdk_surface) = surface_root_widget.native().and_then(|n| n.surface()) {
                let key = "vibepanel-blur-surface-watched";
                unsafe {
                    if gdk_surface.data::<bool>(key).is_some() {
                        return; // watcher already installed
                    }
                    gdk_surface.set_data(key, true);
                }
                // Clone as gtk4::Widget (the common base) so the closure can
                // hold the value regardless of the concrete surface_root type.
                let root_clone = surface_root_widget.clone();
                let content_clone = content_widget.clone();
                gdk_surface.connect_notify_local(Some("width"), move |_, _| {
                    let rc = root_clone.clone();
                    let cc = content_clone.clone();
                    glib::idle_add_local_once(move || {
                        if crate::services::config_manager::ConfigManager::global().blur_enabled()
                            && let Some(blur) = Self::global()
                        {
                            blur.apply_blur_surface(&rc, &cc, radius);
                        }
                    });
                });
            }
            return;
        }

        let bounds = bounds.unwrap();
        let bx = bounds.x().round() as i32;
        let by = bounds.y().round() as i32;
        let bw = bounds.width().round() as i32;
        let bh = bounds.height().round() as i32;

        let Some((effect, compositor)) = self.get_or_create_effect(&info) else {
            return;
        };

        let region = compositor.create_region(&self.qh, ());
        add_rounded_rect_to_region(&region, bx, by, bw, bh, radius);

        effect.set_blur_region(Some(&region));
        region.destroy();
        // Commit the surface so the double-buffered blur region takes effect
        // immediately.  Without this, the region stays pending until GTK's
        // next wl_surface.commit — which may never come for surfaces whose
        // layout is already complete (e.g. tray popovers reached via the
        // deferred idle path).
        info.wl_surface.commit();
        self.flush();

        // No dedicated resize watcher is installed here beyond the readiness
        // watcher above.  That watcher remains connected and will re-apply the
        // blur region on subsequent resizes, which is correct behaviour.
        // Current consumers have stable content bounds after initial layout,
        // so re-applies are infrequent.  A future consumer with dynamically-
        // resizing content gets automatic re-apply for free via the same
        // watcher.
        debug!(
            "Applied blur surface: {}x{} at ({},{}) r={} (surface {}x{})",
            bw, bh, bx, by, radius, width, height
        );
    }
}
