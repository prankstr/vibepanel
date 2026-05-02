//! Wayland background effect (blur) region hints.
//!
//! Uses the `ext-background-effect-v1` staging protocol to tell the compositor
//! exactly which region of each surface should be blurred, excluding shadows
//! and transparent padding. This is a **zero-cost hint** — if the compositor
//! has no blur configured, it is silently ignored.
//!
//! ## Surface scope
//!
//! Covers vibepanel-managed surfaces with visible backgrounds: bar, layer-shell
//! popovers, Quick Settings, notification toasts, OSD, tray menus, and the media
//! pop-out window. Child popovers inside already-blurred surfaces and tiny
//! tooltips are intentionally excluded because the visual benefit is negligible.
//! On compositors without support for a surface role, the hint is ignored.
//!
//! ## Architecture
//!
//! The service bridges GDK's Wayland connection with `wayland-client` objects.
//! It creates its own `EventQueue` on GDK's `wl_display` and integrates it
//! into the glib main loop via `unix_fd_add_local`.
//!
//! Per-surface `ExtBackgroundEffectSurfaceV1` objects are cached by
//! `ObjectId` to avoid the protocol error raised when creating duplicates.
//!
//! **Fragile dependency:** `connection_from_gdk_display()` reconstructs
//! GDK's internal `wayland_client::Connection` via proxy backend
//! extraction.  This is the only viable approach (a second
//! `Backend::from_foreign_display()` would steal events from GDK), but
//! it depends on `gdk4-wayland` internals — see that function for details.
//!
//! ## Blur lifecycle
//!
//! Each consumer surface manages its own blur lifecycle.  The correct
//! approach depends on two orthogonal axes:
//!
//! **Axis 1 — hide mechanism:**
//!
//! | Mechanism | What happens to blur | Consumer action |
//! |-----------|---------------------|-----------------|
//! | **Opacity-hide** (`set_opacity(0.0)`) | Surface stays mapped; compositor continues rendering blur behind an invisible surface | Must call `remove_blur_region()` before hiding |
//! | **Reusable unmap** (`set_visible(false)`) | Surface is unmapped; compositor suspends blur rendering but the protocol object persists | Usually preserve the object and clean stale state on the next `connect_map` |
//! | **Transient unmap / destroy** | Surface is short-lived or cheap to reapply | Clean up on `connect_unmap`; keep `connect_destroy` as a safety net |
//!
//! **Axis 2 — surface identity:**
//!
//! | Identity | Re-apply strategy | Theme hot-reload |
//! |----------|-------------------|-----------------|
//! | **Reusable** (surface persists across show/hide) | `connect_map` applies blur if enabled, removes stale objects if not | Optional — `on_theme_change` if surface may be visible during config edit |
//! | **Transient/standalone** (one-shot or cheap to recreate blur) | `connect_map` applies blur; `connect_unmap` removes it | `on_theme_change` if surface may remain visible long enough for live config edits |
//!
//! **Animation caveat:** if a close path fades opacity *before* unmapping
//! (e.g. popovers, Quick Settings), blur must be removed at fade start.
//! Compositor-side blur renders independently of widget opacity — without
//! removal, a blur rectangle remains visible through the fading surface.
//!
//! ### Terminology
//!
//! - **Protocol object**: the `ExtBackgroundEffectSurfaceV1` entry cached in
//!   the manager's `effects` HashMap.  Created by `get_or_create_effect()`,
//!   destroyed by `remove_blur_region()`.  Persists across unmap/remap.
//! - **Compositor-side blur**: the visual blur effect rendered by the
//!   compositor.  Only drawn while the surface is mapped.  Suspended (not
//!   destroyed) on unmap; restored on remap if the protocol object persists.
//!
//! ### Known constraints
//!
//! - **`remove_blur_region` requires a mapped surface.**
//!   `SurfaceInfo::from_widget()` calls `widget.native()?.surface()?`,
//!   which returns `None` for unmapped layer-shell surfaces.  Therefore
//!   `on_theme_change` callbacks cannot clean up blur on hidden windows.
//!   For reusable surfaces that may be unmapped when blur is toggled off,
//!   `connect_map` is the earliest reliable cleanup point — it fires when
//!   the surface becomes available again.
//!
//! ### Shared lifecycle pattern (OSD, toast, media)
//!
//! These three standalone-window consumers share a near-identical lifecycle:
//! `connect_map` (apply blur) + `connect_unmap` (primary cleanup) +
//! `connect_destroy` (safety net) + `on_theme_change` (re-apply/remove) +
//! `ThemeCallbackGuard`.  The only variations are the content widget
//! resolution and the radius source, so
//! [`attach_blur_surface_lifecycle`] centralizes that wiring.

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
    ext_background_effect_manager_v1::{self, Capability, ExtBackgroundEffectManagerV1},
    ext_background_effect_surface_v1::{self, ExtBackgroundEffectSurfaceV1},
};

const BLUR_REGION_RESIZE_WATCHED_KEY: &str = "vibepanel-blur-resize-watched";
const BLUR_SURFACE_RESIZE_WATCHED_KEY: &str = "vibepanel-blur-surface-watched";

/// Attach the standard blur lifecycle for standalone GTK windows.
///
/// Used by OSD, notification toasts, and the media pop-out: apply on map,
/// remove on unmap while the wl_surface is still resolvable, keep destroy as a
/// safety net, and live-update on theme changes. Reusable animated surfaces
/// (bar, popovers, Quick Settings) have bespoke lifecycles and should not use
/// this helper.
pub fn attach_blur_surface_lifecycle<W, C, R>(
    window: &W,
    content_resolver: C,
    radius_fn: R,
) -> crate::services::config_manager::ThemeCallbackGuard
where
    W: IsA<gtk4::Window> + IsA<gtk4::Widget> + Clone + 'static,
    C: Fn(&W) -> Option<gtk4::Widget> + Clone + 'static,
    R: Fn() -> i32 + Clone + 'static,
{
    use crate::services::config_manager::{ConfigManager, ThemeCallbackGuard};

    let content_for_map = content_resolver.clone();
    let radius_for_map = radius_fn.clone();
    window.connect_map(move |win| {
        if ConfigManager::global().blur_enabled() {
            if let Some(blur) = BackgroundEffectManager::global()
                && let Some(content) = content_for_map(win)
            {
                blur.apply_blur_surface(win, &content, radius_for_map.clone());
            }
        } else if let Some(blur) = BackgroundEffectManager::global() {
            // Remove any stale effect from a previous map cycle when blur was
            // toggled off while the surface was unmapped — the unmap and
            // theme-change cleanup paths are best-effort on an unmapped surface.
            blur.remove_blur_region(win);
        }
    });

    window.connect_unmap(|win| {
        if let Some(blur) = BackgroundEffectManager::global() {
            blur.remove_blur_region(win);
        }
    });

    window.connect_destroy(|win| {
        if let Some(blur) = BackgroundEffectManager::global() {
            blur.remove_blur_region(win);
        }
    });

    let win_weak = window.downgrade();
    let id = ConfigManager::global().on_theme_change(move || {
        let Some(win) = win_weak.upgrade() else {
            return;
        };
        if ConfigManager::global().blur_enabled() {
            if let Some(blur) = BackgroundEffectManager::global()
                && let Some(content) = content_resolver(&win)
            {
                blur.apply_blur_surface(&win, &content, radius_fn.clone());
            }
        } else if let Some(blur) = BackgroundEffectManager::global() {
            blur.remove_blur_region(&win);
        }
    });

    ThemeCallbackGuard(id)
}

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
    /// Whether the compositor advertises blur. Mutable at runtime per spec —
    /// compositor stops applying blur on revocation regardless of client state.
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
        // Only Global is handled; GlobalRemove is intentionally ignored.
        // wl_compositor and ext_background_effect_manager_v1 are core
        // globals that are never removed during a session.
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
        // `capabilities` is sent on bind and on change. Empty bitfield = no blur.
        if let ext_background_effect_manager_v1::Event::Capabilities { flags } = event {
            let now_capable = flags
                .into_result()
                .map(|f| f.contains(Capability::Blur))
                .unwrap_or(false);
            let was_capable = state.blur_capable;
            state.blur_capable = now_capable;
            if was_capable && !now_capable {
                // Revoked — drop bookkeeping; compositor already stopped drawing.
                debug!(
                    "Blur capability revoked; destroying {} effect object(s)",
                    state.effects.len()
                );
                for (_id, effect) in state.effects.drain() {
                    effect.destroy();
                }
            } else if !was_capable && now_capable {
                // Internal state updated; visible surfaces re-apply on next
                // map/resize/theme event (no live walk of mapped surfaces).
                debug!("Blur capability advertised");
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

/// Compute the set of axis-aligned rectangles that tile a rounded rectangle.
///
/// Returns a `Vec<(x, y, width, height)>` in surface-local logical coordinates.
/// The rectangles are non-overlapping and together cover exactly the filled
/// rounded rectangle.
///
/// Uses `round(exact_inset)` per row — nearest-integer assignment minimises
/// total error and max adjacent delta compared to ceil, floor, or Bresenham
/// for the filled-region use case. Some flat runs at the bottom of the arc
/// are geometrically unavoidable (pigeonhole principle) but are imperceptible
/// there since the circle is nearly vertical.
///
/// If `radius` is zero or the dimensions are too small to accommodate it,
/// clamps to a pill shape or falls back to a plain rectangle.
/// Non-positive `width` or `height` return an empty vec (per Wayland protocol,
/// non-positive dimensions are invalid for `wl_region.add`).
#[cfg(test)]
fn compute_rounded_rect_rects(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    radius: i32,
) -> Vec<(i32, i32, i32, i32)> {
    compute_rounded_rect_rects_with_corner_inset(x, y, width, height, radius, 0)
}

/// Compute rounded-rect scanlines, optionally trimming only the rounded corner
/// rows inward.  Used when an outline is visible: the compositor blur region is
/// made slightly more conservative at corners so rectangular scanline edges do
/// not peek through semi-transparent CSS borders.
fn compute_rounded_rect_rects_with_corner_inset(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    radius: i32,
    corner_inset: i32,
) -> Vec<(i32, i32, i32, i32)> {
    if width <= 0 || height <= 0 {
        return Vec::new();
    }
    if radius <= 0 {
        return vec![(x, y, width, height)];
    }
    // Clamp to half the smallest dimension so oversized radii produce a
    // pill shape instead of a plain rectangle.
    let radius = radius.min(width / 2).min(height / 2);
    if radius <= 0 {
        return vec![(x, y, width, height)];
    }

    // Capacity: central rect (if present) + 2 rows per radius scanline.
    let has_center = height > 2 * radius;
    let mut rects = Vec::with_capacity(if has_center { 1 } else { 0 } + 2 * radius as usize);

    // Central rectangle spanning the full width, excluding the top and bottom
    // radius strips.
    if has_center {
        rects.push((x, y + radius, width, height - 2 * radius));
    }

    let corner_inset = corner_inset.max(0);
    let r = radius as f64;
    for i in 0..radius as usize {
        let dy = r - 0.5 - i as f64;
        let inset = if dy < 0.0 {
            0
        } else {
            (r - (r * r - dy * dy).sqrt()).round() as i32
        };
        let inset = (inset + corner_inset).min((width - 1) / 2);
        let row_w = (width - 2 * inset).max(1);
        rects.push((x + inset, y + i as i32, row_w, 1));
        rects.push((x + inset, y + height - 1 - i as i32, row_w, 1));
    }

    rects
}

/// Add a rounded rectangle to a `wl_region`, trimming rounded corner rows
/// inward by 1 logical pixel when an outline is visible.
fn add_rounded_rect_to_region_with_outline(
    region: &wayland_client::protocol::wl_region::WlRegion,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    radius: i32,
    outline_visible: bool,
) {
    // 1px inset hides the staircase artifact at the anti-aliased rounded-corner
    // edge. This doesn't scale with outline_width — the artifact is always at the
    // outermost sub-pixel row; wider outlines simply cover more area inward.
    let corner_inset = if outline_visible { 1 } else { 0 };
    for (rx, ry, rw, rh) in
        compute_rounded_rect_rects_with_corner_inset(x, y, width, height, radius, corner_inset)
    {
        region.add(rx, ry, rw, rh);
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

        // Second roundtrip: the bind from the first roundtrip's Global dispatch
        // triggers a `capabilities` event that won't arrive until we round-trip
        // again. We don't *require* blur to be advertised here — keeping the
        // manager alive lets the fd watcher pick up a later `Capabilities`
        // event (false → true transitions). `get_or_create_effect` gates on
        // `blur_capable`, so consumers no-op until then.
        //
        // Note: a later false→true capability gain updates internal state but
        // does NOT walk currently-mapped surfaces. Re-apply happens on the next
        // natural surface event (map, resize, theme change). No real compositor
        // exhibits runtime capability gain today; revisit if that changes.
        if let Err(e) = event_queue.roundtrip(&mut state) {
            warn!("Failed blur service capability roundtrip: {e}");
            return None;
        }

        debug!(
            "Blur service initialized (compositor={:?}, blur_capable={})",
            state.compositor.is_some(),
            state.blur_capable
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
                    // Continue: blur is cosmetic, protocol failures must not
                    // destabilise the panel.
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
        if !state.blur_capable {
            return None; // Capability revoked or never advertised.
        }
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
    /// Watches both `width` and `height`; either dimension may change
    /// independently. Handler is idempotent.
    ///
    /// The watcher is installed at most once per window.
    fn install_resize_watcher(
        window: &gtk4::ApplicationWindow,
        on_resize: impl Fn() + 'static + Clone,
    ) {
        unsafe {
            if window
                .data::<bool>(BLUR_REGION_RESIZE_WATCHED_KEY)
                .is_some()
            {
                return;
            }
            window.set_data(BLUR_REGION_RESIZE_WATCHED_KEY, true);
        }
        if let Some(gdk_surface) = window.native().and_then(|n| n.surface()) {
            let on_resize_w = on_resize.clone();
            gdk_surface.connect_notify_local(Some("width"), move |_, _| on_resize_w());
            gdk_surface.connect_notify_local(Some("height"), move |_, _| on_resize());
        }
    }

    /// Install a resize watcher for [`apply_blur_surface`](Self::apply_blur_surface)
    /// on a generic widget's GDK surface.
    ///
    /// Watches both `width` and `height`. On change, an idle callback re-invokes
    /// `apply_blur_surface` so the blur region tracks content size; this also
    /// serves as the readiness trigger for the deferred path (surface not yet
    /// sized on first call). The guard key is stored on the `GdkSurface` (not
    /// the widget) because generic widgets don't support `set_data`.
    ///
    /// Installed at most once per surface.
    fn install_surface_resize_watcher(
        surface_root: &gtk4::Widget,
        content: &gtk4::Widget,
        radius_fn: impl Fn() -> i32 + Clone + 'static,
        outline_visible_fn: impl Fn() -> bool + Clone + 'static,
    ) {
        let Some(gdk_surface) = surface_root.native().and_then(|n| n.surface()) else {
            return;
        };
        unsafe {
            if gdk_surface
                .data::<bool>(BLUR_SURFACE_RESIZE_WATCHED_KEY)
                .is_some()
            {
                return; // watcher already installed
            }
            gdk_surface.set_data(BLUR_SURFACE_RESIZE_WATCHED_KEY, true);
        }
        // Use weak refs to avoid a GObject ref cycle
        // (GdkSurface → closure → Widget → GdkSurface).
        let root_weak = surface_root.downgrade();
        let content_weak = content.downgrade();
        let make_handler = || {
            let root_weak = root_weak.clone();
            let content_weak = content_weak.clone();
            let radius_fn = radius_fn.clone();
            let outline_visible_fn = outline_visible_fn.clone();
            move |_: &gdk4_wayland::gdk::Surface, _: &glib::ParamSpec| {
                let root_weak = root_weak.clone();
                let content_weak = content_weak.clone();
                let radius_fn = radius_fn.clone();
                let outline_visible_fn = outline_visible_fn.clone();
                glib::idle_add_local_once(move || {
                    if let Some(rc) = root_weak.upgrade()
                        && let Some(cc) = content_weak.upgrade()
                        && crate::services::config_manager::ConfigManager::global().blur_enabled()
                        && let Some(blur) = Self::global()
                    {
                        blur.apply_blur_surface_with_outline(
                            &rc,
                            &cc,
                            radius_fn,
                            outline_visible_fn,
                        );
                    }
                });
            }
        };
        gdk_surface.connect_notify_local(Some("width"), make_handler());
        gdk_surface.connect_notify_local(Some("height"), make_handler());
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
    /// Commits the surface explicitly so the double-buffered blur region takes
    /// effect even when this runs from a deferred idle or resize callback with
    /// no guaranteed subsequent GTK frame commit.
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

        add_rounded_rect_to_region_with_outline(
            &region,
            x,
            y,
            w,
            h,
            radius,
            crate::services::config_manager::ConfigManager::global().surface_outline_visible(),
        );

        effect.set_blur_region(Some(&region));
        region.destroy();
        info.wl_surface.commit();
        self.flush();

        debug!(
            "Applied blur region: {}x{} at ({},{}) r={} margins t={} b={} s={} e={} (surface {}x{})",
            w, h, x, y, radius, margin_top, margin_bottom, margin_start, margin_end, width, height
        );

        // Install a resize watcher (once per window) so the blur region
        // is re-applied whenever the surface dimensions change — e.g. when
        // a Revealer expands and the layer-shell surface reconfigures.
        // Use a weak ref to avoid a GObject ref cycle
        // (GdkSurface → closure → Window → GdkSurface).
        let win_weak = window.downgrade();
        Self::install_resize_watcher(window, move || {
            if let Some(win) = win_weak.upgrade()
                && crate::services::config_manager::ConfigManager::global().blur_enabled()
                && let Some(blur) = BackgroundEffectManager::global()
            {
                blur.apply_blur_region(&win, shadow_margin);
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
        self.apply_blur_surface_with_outline(
            window,
            bar_box,
            || crate::services::config_manager::ConfigManager::global().bar_border_radius() as i32,
            || crate::services::config_manager::ConfigManager::global().bar_outline_visible(),
        );
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

        let outline_visible =
            crate::services::config_manager::ConfigManager::global().widget_outline_visible();
        let region = compositor.create_region(&self.qh, ());
        for &(x, y, w, h) in islands {
            add_rounded_rect_to_region_with_outline(&region, x, y, w, h, radius, outline_visible);
        }

        effect.set_blur_region(Some(&region));
        region.destroy();
        // This path can run from theme hot-reload, not only GTK allocation,
        // so commit explicitly to apply the double-buffered region now.
        info.wl_surface.commit();
        self.flush();

        debug!(
            "Applied bar island blur regions: {} islands, r={}",
            islands.len(),
            radius
        );
    }

    /// Remove the blur region for a window (e.g. on destroy).
    ///
    /// Best-effort cleanup.  Requires a currently mapped surface so
    /// `SurfaceInfo::from_widget()` can resolve the `wl_surface`.  If
    /// called from a late `Drop` after the surface is already unmapped or
    /// destroyed, this silently no-ops and the stale `effects` HashMap
    /// entry persists.  This is intentional and benign: primary cleanup
    /// always happens from mapped-surface paths (`connect_destroy`,
    /// `connect_closed`, fade-start removal, or `connect_map` stale
    /// cleanup on next show), so the `Drop` safety nets in consumers are
    /// defence-in-depth only.  Stale entries are small and bounded by the
    /// number of surfaces torn down without prior cleanup (typically zero
    /// during normal operation).  `ObjectId` equality is instance-based, not
    /// wire-integer based, so stale entries never alias new surfaces even if
    /// wire IDs are reused.
    pub fn remove_blur_region(&self, window: &impl gtk4::prelude::IsA<gtk4::Widget>) {
        let Some(info) = SurfaceInfo::from_widget(window) else {
            return;
        };

        let mut state = self.state.borrow_mut();
        if let Some(effect) = state.effects.remove(&info.surface_id) {
            effect.destroy();
            debug!("Removed blur region for surface");
            drop(state);

            // Destroy is double-buffered; commit so the compositor removes
            // the effect immediately.  Committing GDK's wl_surface outside
            // the render cycle is safe here: we only touch protocol state
            // that GDK does not manage (blur regions), and GTK's next frame
            // commit will simply re-apply its own pending state on top.
            info.wl_surface.commit();
            self.flush();
        }
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
        add_rounded_rect_to_region_with_outline(
            &region,
            x.round() as i32,
            y.round() as i32,
            scaled_w.round() as i32,
            scaled_h.round() as i32,
            radius,
            crate::services::config_manager::ConfigManager::global().surface_outline_visible(),
        );

        effect.set_blur_region(Some(&region));
        region.destroy();
        // Called only from GTK frame-clock ticks; GTK commits the surface for
        // that frame, so this path intentionally avoids an extra commit per
        // animation frame.
        self.flush();
    }

    /// Apply blur for the popover/Quick Settings opening animation.
    ///
    /// While opening, the region tracks the current grow-in scale. On the final
    /// tick, switch to the normal full-size path so the resize watcher is
    /// installed for later content/size changes.
    pub fn apply_open_animation_blur(
        &self,
        window: &gtk4::ApplicationWindow,
        shadow_margin: i32,
        scale: f64,
        complete: bool,
    ) {
        if complete {
            self.apply_blur_region(window, shadow_margin);
        } else {
            self.apply_blur_region_animated(window, shadow_margin, scale);
        }
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
    /// `radius_fn` is a closure returning the current corner radius.
    ///
    /// Using a closure instead of a plain `i32` ensures the deferred
    /// readiness/resize watcher always reads the latest theme value.
    /// Without this, a theme change after initial apply would leave the
    /// watcher using the stale radius captured at first call.
    ///
    /// On first map the Wayland surface may still be a 1×1 placeholder before
    /// the compositor sends configure.  In that case, a one-shot watcher on
    /// the GDK surface's `height` property defers the apply until configure
    /// arrives and layout completes.
    pub fn apply_blur_surface(
        &self,
        surface_root: &impl gtk4::prelude::IsA<gtk4::Widget>,
        content: &impl gtk4::prelude::IsA<gtk4::Widget>,
        radius_fn: impl Fn() -> i32 + Clone + 'static,
    ) {
        self.apply_blur_surface_with_outline(surface_root, content, radius_fn, || {
            crate::services::config_manager::ConfigManager::global().surface_outline_visible()
        });
    }

    fn apply_blur_surface_with_outline(
        &self,
        surface_root: &impl gtk4::prelude::IsA<gtk4::Widget>,
        content: &impl gtk4::prelude::IsA<gtk4::Widget>,
        radius_fn: impl Fn() -> i32 + Clone + 'static,
        outline_visible_fn: impl Fn() -> bool + Clone + 'static,
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
            // Install a resize watcher so we re-try once the surface gets a
            // real size.  The watcher also handles subsequent resizes after
            // the initial apply succeeds.
            Self::install_surface_resize_watcher(
                surface_root_widget,
                content_widget,
                radius_fn,
                outline_visible_fn,
            );
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
        let radius = radius_fn();
        add_rounded_rect_to_region_with_outline(
            &region,
            bx,
            by,
            bw,
            bh,
            radius,
            outline_visible_fn(),
        );

        effect.set_blur_region(Some(&region));
        region.destroy();
        // Commit the surface so the double-buffered blur region takes effect
        // immediately.  Without this, the region stays pending until GTK's
        // next wl_surface.commit -- which may never come for surfaces whose
        // layout is already complete (e.g. tray popovers reached via the
        // deferred idle path).  Safe for the same reason as the commit in
        // remove_blur_region: we only touch blur state GDK doesn't manage.
        info.wl_surface.commit();
        self.flush();

        // Install a resize watcher so the blur region is updated if the
        // surface dimensions change after initial layout.  The idempotency
        // guard inside ensures this is a no-op when the deferred path
        // already installed one.
        Self::install_surface_resize_watcher(
            surface_root_widget,
            content_widget,
            radius_fn,
            outline_visible_fn,
        );

        debug!(
            "Applied blur surface: {}x{} at ({},{}) r={} (surface {}x{})",
            bw, bh, bx, by, radius, width, height
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{compute_rounded_rect_rects, compute_rounded_rect_rects_with_corner_inset};

    /// Helper: compute total pixel area covered by non-overlapping scanline rects.
    fn total_area(rects: &[(i32, i32, i32, i32)]) -> i64 {
        rects.iter().map(|&(_, _, w, h)| w as i64 * h as i64).sum()
    }

    /// Helper: rasterize to a pixel grid and count set pixels (detects overlaps).
    fn rasterize_pixel_count(
        rects: &[(i32, i32, i32, i32)],
        bx: i32,
        by: i32,
        bw: i32,
        bh: i32,
    ) -> usize {
        let mut grid = vec![false; (bw * bh) as usize];
        for &(rx, ry, rw, rh) in rects {
            for py in ry..ry + rh {
                for px in rx..rx + rw {
                    let idx = ((py - by) * bw + (px - bx)) as usize;
                    grid[idx] = true;
                }
            }
        }
        grid.iter().filter(|&&v| v).count()
    }

    // ── Degenerate / invalid inputs ─────────────────────────────────────

    #[test]
    fn zero_width_returns_empty() {
        assert!(compute_rounded_rect_rects(0, 0, 0, 10, 5).is_empty());
    }

    #[test]
    fn zero_height_returns_empty() {
        assert!(compute_rounded_rect_rects(0, 0, 10, 0, 5).is_empty());
    }

    #[test]
    fn negative_width_returns_empty() {
        assert!(compute_rounded_rect_rects(0, 0, -5, 10, 5).is_empty());
    }

    #[test]
    fn negative_height_returns_empty() {
        assert!(compute_rounded_rect_rects(0, 0, 10, -3, 5).is_empty());
    }

    #[test]
    fn one_by_one_returns_single_rect() {
        let rects = compute_rounded_rect_rects(5, 10, 1, 1, 0);
        assert_eq!(rects, vec![(5, 10, 1, 1)]);
    }

    #[test]
    fn one_by_one_with_radius_returns_single_rect() {
        // radius clamps to min(1/2, 1/2) = 0 → plain rect
        let rects = compute_rounded_rect_rects(0, 0, 1, 1, 10);
        assert_eq!(rects, vec![(0, 0, 1, 1)]);
    }

    // ── Zero / negative radius ──────────────────────────────────────────

    #[test]
    fn zero_radius_returns_single_rect() {
        let rects = compute_rounded_rect_rects(10, 20, 100, 50, 0);
        assert_eq!(rects, vec![(10, 20, 100, 50)]);
    }

    #[test]
    fn negative_radius_returns_single_rect() {
        let rects = compute_rounded_rect_rects(0, 0, 40, 30, -5);
        assert_eq!(rects, vec![(0, 0, 40, 30)]);
    }

    // ── Bounding-box containment ────────────────────────────────────────

    #[test]
    fn all_rects_within_bounding_box() {
        for radius in [1, 5, 10, 20, 50] {
            for (w, h) in [(20, 20), (100, 40), (40, 100), (3, 3), (50, 50)] {
                let x = 10;
                let y = 20;
                let rects = compute_rounded_rect_rects(x, y, w, h, radius);
                for &(rx, ry, rw, rh) in &rects {
                    assert!(
                        rx >= x && ry >= y && rx + rw <= x + w && ry + rh <= y + h,
                        "rect ({rx},{ry},{rw},{rh}) outside bbox ({x},{y},{w},{h}) with r={radius}"
                    );
                }
            }
        }
    }

    #[test]
    fn all_rects_have_positive_dimensions() {
        for radius in [1, 5, 10, 20, 50] {
            for (w, h) in [(20, 20), (100, 40), (2, 2), (3, 7)] {
                let rects = compute_rounded_rect_rects(0, 0, w, h, radius);
                for &(_, _, rw, rh) in &rects {
                    assert!(
                        rw > 0 && rh > 0,
                        "non-positive dim ({rw},{rh}) for {w}x{h} r={radius}"
                    );
                }
            }
        }
    }

    // ── Area properties ─────────────────────────────────────────────────

    #[test]
    fn area_less_than_or_equal_to_bounding_box() {
        for radius in [1, 5, 10, 20] {
            for (w, h) in [(20, 20), (100, 40), (40, 100)] {
                let rects = compute_rounded_rect_rects(0, 0, w, h, radius);
                let area = total_area(&rects);
                let bbox_area = w as i64 * h as i64;
                assert!(
                    area <= bbox_area,
                    "area {area} > bbox {bbox_area} for {w}x{h} r={radius}"
                );
            }
        }
    }

    #[test]
    fn zero_radius_area_equals_bounding_box() {
        let rects = compute_rounded_rect_rects(0, 0, 100, 50, 0);
        assert_eq!(total_area(&rects), 100 * 50);
    }

    #[test]
    fn rounded_area_strictly_less_than_bounding_box() {
        // With a non-trivial radius, corners are cut so area must be less.
        let rects = compute_rounded_rect_rects(0, 0, 100, 50, 10);
        let area = total_area(&rects);
        let bbox = 100i64 * 50;
        assert!(area < bbox, "expected area < {bbox}, got {area}");
        // But should still cover the vast majority.
        assert!(area > bbox * 90 / 100, "area {area} too small for {bbox}");
    }

    #[test]
    fn no_pixel_overlaps() {
        // Rasterize and verify unique pixel count matches summed rect area.
        for radius in [1, 5, 10, 15] {
            for (w, h) in [(30, 30), (50, 20), (20, 50)] {
                let rects = compute_rounded_rect_rects(0, 0, w, h, radius);
                let sum_area = total_area(&rects) as usize;
                let pixel_count = rasterize_pixel_count(&rects, 0, 0, w, h);
                assert_eq!(
                    sum_area, pixel_count,
                    "overlap detected for {w}x{h} r={radius}: sum={sum_area} pixels={pixel_count}"
                );
            }
        }
    }

    #[test]
    fn zero_corner_inset_matches_default_rasterization() {
        let default = compute_rounded_rect_rects(3, 7, 40, 24, 8);
        let inset_zero = compute_rounded_rect_rects_with_corner_inset(3, 7, 40, 24, 8, 0);
        assert_eq!(default, inset_zero);
    }

    #[test]
    fn corner_inset_trims_only_corner_scanlines() {
        let base = compute_rounded_rect_rects(0, 0, 40, 30, 8);
        let inset = compute_rounded_rect_rects_with_corner_inset(0, 0, 40, 30, 8, 1);

        assert_eq!(base.len(), inset.len());
        assert_eq!(base[0], inset[0], "central rect should remain unchanged");

        for (before, after) in base.iter().skip(1).zip(inset.iter().skip(1)) {
            assert_eq!(before.1, after.1, "y should not move");
            assert_eq!(before.3, after.3, "scanline height should stay 1");
            assert!(after.0 >= before.0, "left edge should move inward or stay");
            assert!(
                after.0 + after.2 <= before.0 + before.2,
                "right edge should move inward or stay"
            );
            assert!(after.2 > 0, "scanline width must stay positive");
        }
    }

    // ── Symmetry ────────────────────────────────────────────────────────

    #[test]
    fn vertical_symmetry() {
        // The top and bottom halves should mirror each other.
        let (w, h, r) = (40, 40, 10);
        let rects = compute_rounded_rect_rects(0, 0, w, h, r);
        let mut grid = vec![vec![false; w as usize]; h as usize];
        for &(rx, ry, rw, rh) in &rects {
            for py in ry..ry + rh {
                for px in rx..rx + rw {
                    grid[py as usize][px as usize] = true;
                }
            }
        }
        for y in 0..h as usize / 2 {
            let mirror_y = h as usize - 1 - y;
            assert_eq!(
                grid[y], grid[mirror_y],
                "row {y} != mirrored row {mirror_y}"
            );
        }
    }

    #[test]
    fn horizontal_symmetry() {
        // Left and right insets should be equal (symmetric corners).
        let (w, h, r) = (40, 40, 10);
        let rects = compute_rounded_rect_rects(0, 0, w, h, r);
        let mut grid = vec![vec![false; w as usize]; h as usize];
        for &(rx, ry, rw, rh) in &rects {
            for py in ry..ry + rh {
                for px in rx..rx + rw {
                    grid[py as usize][px as usize] = true;
                }
            }
        }
        for (y, row) in grid.iter().enumerate() {
            for x in 0..w as usize / 2 {
                let mirror_x = w as usize - 1 - x;
                assert_eq!(
                    row[x], row[mirror_x],
                    "({x},{y}) != mirror ({mirror_x},{y})"
                );
            }
        }
    }

    // ── Pill shape (oversized radius) ───────────────────────────────────

    #[test]
    fn oversized_radius_clamps_to_pill() {
        // radius=100 on a 20x10 rect → clamped to min(10, 5) = 5
        let rects = compute_rounded_rect_rects(0, 0, 20, 10, 100);
        assert!(!rects.is_empty());
        for &(rx, ry, rw, rh) in &rects {
            assert!(rx >= 0 && ry >= 0 && rx + rw <= 20 && ry + rh <= 10);
        }
    }

    #[test]
    fn pill_shape_has_no_central_rect_when_height_equals_two_radii() {
        // 20x10 with r=5 → clamped radius = 5, height = 2*5 = 10 → no center
        let rects = compute_rounded_rect_rects(0, 0, 20, 10, 5);
        // All rects should be height=1 scanlines (no tall central rect).
        for &(_, _, _, rh) in &rects {
            assert_eq!(rh, 1, "expected all scanlines, got height {rh}");
        }
    }

    // ── Offset (non-zero origin) ────────────────────────────────────────

    #[test]
    fn offset_origin_shifts_all_rects() {
        let base = compute_rounded_rect_rects(0, 0, 30, 30, 8);
        let shifted = compute_rounded_rect_rects(100, 200, 30, 30, 8);
        assert_eq!(base.len(), shifted.len());
        for (b, s) in base.iter().zip(shifted.iter()) {
            assert_eq!(b.0 + 100, s.0, "x mismatch");
            assert_eq!(b.1 + 200, s.1, "y mismatch");
            assert_eq!(b.2, s.2, "width mismatch");
            assert_eq!(b.3, s.3, "height mismatch");
        }
    }

    // ── Odd dimensions ──────────────────────────────────────────────────

    #[test]
    fn odd_dimensions_produce_valid_rects() {
        let rects = compute_rounded_rect_rects(0, 0, 31, 31, 7);
        assert!(!rects.is_empty());
        let pixel_count = rasterize_pixel_count(&rects, 0, 0, 31, 31);
        let sum_area = total_area(&rects) as usize;
        assert_eq!(sum_area, pixel_count, "overlap with odd dims");
    }

    // ── Full coverage: every row has at least one pixel ─────────────────

    #[test]
    fn every_row_covered() {
        for radius in [1, 5, 10] {
            let (w, h) = (30, 30);
            let rects = compute_rounded_rect_rects(0, 0, w, h, radius);
            let mut row_covered = vec![false; h as usize];
            for &(_, ry, _, rh) in &rects {
                for y in ry..ry + rh {
                    row_covered[y as usize] = true;
                }
            }
            assert!(
                row_covered.iter().all(|&c| c),
                "not all rows covered for r={radius}"
            );
        }
    }
}
