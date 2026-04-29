//! Layer shell popover infrastructure for widget menus.
//!
//! Provides two levels of abstraction:
//!
//! 1. **Helper functions** - Low-level utilities for layer-shell surfaces
//!    that need click-catcher or focus handling.
//!
//! 2. **`LayerShellPopover`** - Complete popover solution for simple widget menus.

use gtk4::gdk::{self, Monitor};
use gtk4::glib::{self, ControlFlow, Propagation};
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box as GtkBox, EventControllerKey, GestureClick, Orientation,
};

/// Whether a key is a keyboard navigation key (Tab, arrows, Home, End).
/// Used by the deferred keyboard nav controller to gate activation.
pub fn is_keynav_key(keyval: gdk::Key) -> bool {
    matches!(
        keyval,
        gdk::Key::Tab
            | gdk::Key::ISO_Left_Tab
            | gdk::Key::Up
            | gdk::Key::Down
            | gdk::Key::Left
            | gdk::Key::Right
            | gdk::Key::Home
            | gdk::Key::End
    )
}
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use super::scale_box::ScaleBox;
use crate::services::compositor::CompositorManager;
use crate::services::config_manager::ConfigManager;
use crate::services::surfaces::SurfaceStyleManager;
use crate::styles::{class, surface};

/// Margin around popover content for shadow rendering space.
///
/// GTK4 box-shadows extend beyond the widget bounds, so we need extra margin
/// on the outer container to prevent shadow clipping.
const POPOVER_SHADOW_MARGIN: i32 = 8;

/// Minimum margin from screen edge for popovers.
const POPOVER_MIN_EDGE_MARGIN: i32 = 4;

/// Estimated popover width when actual width not yet available.
const POPOVER_DEFAULT_WIDTH_ESTIMATE: i32 = 320;

const POPOVER_MIN_VALID_WIDTH: i32 = 20;

/// Animation duration as f64 milliseconds for tick-callback math.
pub(crate) const ANIM_DURATION_MS: f64 = super::css::POPOVER_ANIMATION_MS as f64;

/// Starting scale for popover open/close animation.
/// ScaleBox simulates this via symmetric center-clip (no actual scale transform).
pub(crate) const ANIM_SCALE_FROM: f64 = 0.94;

/// Direction of the popover animation.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum AnimDirection {
    Opening,
    Closing,
}

/// Shared animation state, passed to the tick callback via `Rc<RefCell<_>>`.
///
/// `progress` represents the current visual state:
///   0.0 = fully hidden (opacity 0)
///   1.0 = fully visible (opacity 1)
pub(crate) struct AnimState {
    /// Current direction of animation.
    pub(crate) direction: AnimDirection,
    /// Frame-clock time (microseconds) when this animation segment started.
    pub(crate) start_time_us: i64,
    /// Progress value at the start of this segment (for mid-flight reversal).
    pub(crate) start_progress: f64,
    /// Target progress (1.0 for opening, 0.0 for closing).
    pub(crate) target_progress: f64,
    /// Whether a tick callback is currently driving this state.
    pub(crate) active: bool,
    /// Generation counter that the current tick callback was started with.
    /// Used to detect when an active tick has a stale generation and needs
    /// to be replaced by a new one.
    pub(crate) tick_generation: u32,
}

impl AnimState {
    pub(crate) fn new_idle() -> Self {
        Self {
            direction: AnimDirection::Opening,
            start_time_us: 0,
            start_progress: 0.0,
            target_progress: 0.0,
            active: false,
            tick_generation: 0,
        }
    }

    /// Compute the current eased progress given the frame clock time.
    pub(crate) fn current_progress(&self, now_us: i64) -> f64 {
        let elapsed_ms = (now_us - self.start_time_us) as f64 / 1000.0;
        let distance = (self.target_progress - self.start_progress).abs();
        if distance < f64::EPSILON {
            return self.target_progress;
        }
        // Duration is proportional to remaining distance — a half-done
        // animation that reverses takes half the time.
        let segment_duration_ms = ANIM_DURATION_MS * distance;
        let t = (elapsed_ms / segment_duration_ms).clamp(0.0, 1.0);
        // Quintic ease-out: snappy start, long gentle tail.
        // Approximates the Material Design `cubic-bezier(0.2, 0, 0, 1)` curve
        // used in the original CSS transitions.
        let eased = 1.0 - (1.0 - t).powi(5);
        self.start_progress + (self.target_progress - self.start_progress) * eased
    }

    /// Whether the animation has reached its target.
    pub(crate) fn is_complete(&self, now_us: i64) -> bool {
        let elapsed_ms = (now_us - self.start_time_us) as f64 / 1000.0;
        let distance = (self.target_progress - self.start_progress).abs();
        if distance < f64::EPSILON {
            return true;
        }
        let segment_duration_ms = ANIM_DURATION_MS * distance;
        elapsed_ms >= segment_duration_ms
    }

    /// Prepare an animation segment and determine if a new tick callback is needed.
    ///
    /// Captures the current progress (for mid-flight reversal), updates all state
    /// fields, and returns `true` if a new tick callback must be registered. Returns
    /// `false` if the existing tick callback will pick up the new direction.
    ///
    /// `current_opacity` is the shell's current opacity, used as the starting
    /// progress when no animation is in flight.
    pub(crate) fn prepare(
        &mut self,
        direction: AnimDirection,
        generation: u32,
        start_time_us: i64,
        current_opacity: f64,
    ) -> bool {
        let target = match direction {
            AnimDirection::Opening => 1.0,
            AnimDirection::Closing => 0.0,
        };

        let start_progress = if self.active {
            self.current_progress(start_time_us)
        } else {
            current_opacity
        };

        let was_active = self.active;
        let tick_is_current = was_active && self.tick_generation == generation;
        self.direction = direction;
        self.start_time_us = start_time_us;
        self.start_progress = start_progress;
        self.target_progress = target;
        self.active = true;
        self.tick_generation = generation;
        // Need a new tick callback if none is running, or the running one
        // has a stale generation (it will self-cancel).
        !tick_is_current
    }
}

/// Calculate the margin for a popover on the bar-adjacent edge.
///
/// When the bar has a visible background (opacity > 0), the popover needs to
/// account for bar padding in its positioning. This ensures consistent visual
/// spacing regardless of bar transparency settings.
///
/// Used by both `LayerShellPopover` and Quick Settings for consistent positioning.
/// The returned value should be applied to `Edge::Top` when bar is top,
/// or `Edge::Bottom` when bar is bottom.
pub fn calculate_popover_bar_margin() -> i32 {
    let config_mgr = ConfigManager::global();
    let bar_padding = config_mgr.bar_padding() as i32;
    let bar_opacity = config_mgr.bar_background_opacity();
    let popover_offset = config_mgr.popover_offset() as i32;

    if bar_opacity > 0.0 {
        popover_offset - bar_padding
    } else {
        popover_offset
    }
}

/// Get the edge that popovers should anchor to (same side as the bar).
///
/// When bar is at the top, popovers anchor to `Edge::Top` and open downward.
/// When bar is at the bottom, popovers anchor to `Edge::Bottom` and open upward.
pub fn popover_bar_edge() -> Edge {
    if ConfigManager::global().bar_is_bottom() {
        Edge::Bottom
    } else {
        Edge::Top
    }
}

/// Calculate the right margin for a popover to center it on an anchor point.
///
/// This clamps the margin to keep the popover on-screen while centering it
/// as closely as possible to the anchor X coordinate.
///
/// # Coordinate Space
///
/// All parameters use **monitor-local coordinates** (0,0 at the monitor's top-left).
/// This is correct because:
/// - Layer-shell surfaces are anchored to specific monitors
/// - `anchor_x` comes from `compute_bounds()` which returns monitor-relative coords
/// - `monitor_width` is from `monitor.geometry().width()` (the monitor's own width)
/// - The resulting margin is applied to a layer-shell surface on the same monitor
///
/// # Arguments
///
/// * `anchor_x` - X coordinate of the anchor point (widget center) in monitor-local coordinates
/// * `monitor_width` - Width of the monitor (from `monitor.geometry().width()`)
/// * `window_width` - Actual or estimated width of the popover window
/// * `min_edge_margin` - Minimum margin from screen edge
///
/// # Returns
///
/// The right margin to apply to the window, clamped to valid bounds.
pub fn calculate_popover_right_margin(
    anchor_x: i32,
    monitor_width: i32,
    window_width: i32,
    min_edge_margin: i32,
) -> i32 {
    let right_margin = monitor_width - anchor_x - window_width / 2;
    let max_margin = monitor_width.saturating_sub(window_width + min_edge_margin);

    // Ensure min <= max to avoid clamp panic
    if max_margin >= min_edge_margin {
        right_margin.clamp(min_edge_margin, max_margin)
    } else {
        // Window is too wide for monitor, just use minimum margin
        min_edge_margin.max(max_margin)
    }
}

/// Get the appropriate keyboard mode for layer-shell popovers.
///
/// - **Hyprland**: Uses `OnDemand` because `Exclusive` mode breaks input handling
///   entirely (clicks don't work, can't interact with other surfaces).
/// - **Other compositors**: Uses `Exclusive` to maintain keyboard focus after
///   workspace switches.
pub fn popover_keyboard_mode() -> KeyboardMode {
    if CompositorManager::global().backend_name() == "Hyprland" {
        KeyboardMode::OnDemand
    } else {
        KeyboardMode::Exclusive
    }
}

/// Calculate the bar's exclusive zone height for click-catcher margin.
///
/// This matches the logic in `bar.rs` to ensure the click-catcher leaves
/// the bar area uncovered for seamless transitions.
pub fn calculate_bar_exclusive_zone() -> i32 {
    let config_mgr = ConfigManager::global();
    let bar_size = config_mgr.bar_size() as i32;
    let bar_padding = config_mgr.bar_padding() as i32;
    let bar_opacity = config_mgr.bar_background_opacity();
    let screen_margin = config_mgr.screen_margin() as i32;

    if bar_opacity > 0.0 {
        bar_size + 2 * bar_padding + 2 * screen_margin
    } else {
        bar_size + 2 * screen_margin
    }
}

/// Create a click-catcher layer-shell surface.
///
/// The click-catcher is a fullscreen transparent surface that sits behind popovers
/// and captures clicks outside the popover to dismiss it. It has a margin on the
/// bar-adjacent edge equal to the bar's exclusive zone so clicks on the bar pass
/// through.
///
/// # Arguments
///
/// * `app` - The GTK application
/// * `bar_zone` - Height of the bar's exclusive zone (margin on bar edge to leave bar uncovered)
/// * `on_dismiss` - Callback invoked when the catcher is clicked
///
/// # Returns
///
/// The click-catcher window. Caller is responsible for showing it and storing it.
pub fn create_click_catcher<F>(app: &Application, bar_zone: i32, on_dismiss: F) -> ApplicationWindow
where
    F: Fn() + Clone + 'static,
{
    let catcher = ApplicationWindow::builder()
        .application(app)
        .title("vibepanel click catcher")
        .decorated(false)
        .build();

    catcher.add_css_class(surface::LAYER_SHELL_CLICK_CATCHER);
    catcher.add_css_class(class::CLICK_CATCHER);

    // Layer shell configuration - fullscreen surface behind the popover.
    // Use Top layer (not Overlay) to avoid appearing on top of fullscreen apps.
    catcher.init_layer_shell();
    catcher.set_namespace(Some("vibepanel-click-catcher"));
    catcher.set_layer(Layer::Top);
    catcher.set_exclusive_zone(-1); // Cover everything
    catcher.set_anchor(Edge::Top, true);
    catcher.set_anchor(Edge::Bottom, true);
    catcher.set_anchor(Edge::Left, true);
    catcher.set_anchor(Edge::Right, true);
    // Click-catcher should never take keyboard focus - its only purpose is
    // catching clicks outside the popover. Keyboard focus belongs to the actual
    // popover window which is shown after this.
    catcher.set_keyboard_mode(KeyboardMode::None);

    // Leave the bar area uncovered so clicks/hovers pass through to bar widgets.
    let bar_edge = popover_bar_edge();
    catcher.set_margin(bar_edge, bar_zone);

    // Content - add CSS class to the child widget for background styling
    let overlay = GtkBox::new(Orientation::Vertical, 0);
    overlay.set_hexpand(true);
    overlay.set_vexpand(true);
    overlay.add_css_class(class::CLICK_CATCHER); // Apply background to child
    catcher.set_child(Some(&overlay));

    // Click handler
    let gesture = GestureClick::new();
    gesture.set_button(0); // All buttons
    {
        // Use connect_released to allow GTK to complete the gesture lifecycle
        // before hiding windows. This avoids "Broken accounting of active state" warnings.
        gesture.connect_released(move |_gesture, _, _x, _y| {
            on_dismiss();
        });
    }
    catcher.add_controller(gesture);

    // Note: No ESC handler on click-catcher. ESC handling is done by the actual
    // popover window via setup_esc_handler(). The click-catcher has KeyboardMode::None
    // so it won't receive keyboard events anyway.

    catcher
}

/// Set up ESC key handler on a window to dismiss the popover.
pub fn setup_esc_handler<F>(window: &ApplicationWindow, on_dismiss: F)
where
    F: Fn() + 'static,
{
    let key_controller = EventControllerKey::new();
    key_controller.connect_key_pressed(move |_, keyval, _, _| {
        if keyval == gdk::Key::Escape {
            on_dismiss();
            Propagation::Stop
        } else {
            Propagation::Proceed
        }
    });
    window.add_controller(key_controller);
}

/// A layer-shell popover for widget menus.
///
/// The window shell (`ApplicationWindow` with layer-shell configuration) is
/// created lazily on first show and **reused** across open/close cycles.
///
/// ## Animation architecture
///
/// Open/close animations (opacity fade) are driven by a **tick callback**
/// on the persistent animation shell, not by CSS `transition:` properties.
/// CSS `transform: scale()` transitions are observed to cause unbounded
/// memory growth in GTK4.
///
/// The tick callback reads the frame clock each frame, computes eased progress
/// from an `AnimState`, and applies opacity via `Widget::set_opacity()`. This gives:
///
/// - **No CSS transitions** (no `transition:` on any widget)
/// - **Smooth mid-flight reversal** (clicking close during open reverses from
///   the current position, proportional timing)
/// - **No jank** (no snapping between states on rapid clicks)
pub struct LayerShellPopover {
    app: Application,
    widget_name: String,
    builder: Rc<dyn Fn() -> gtk4::Widget>,
    window: RefCell<Option<ApplicationWindow>>,
    click_catcher: RefCell<Option<ApplicationWindow>>,
    /// Persistent animation shell. Never destroyed. Builder content is placed
    /// inside this as a child and swapped on each show.
    anim_shell: RefCell<Option<ScaleBox>>,
    /// Anchor X coordinate (widget center) in monitor coordinates.
    anchor_x: Cell<i32>,
    anchor_monitor: RefCell<Option<Monitor>>,
    /// Optional callback invoked when the popover is fully hidden (after close
    /// animation completes). NOT fired at the start of hide().
    on_close: RefCell<Option<Rc<dyn Fn()>>>,
    /// Optional callback invoked every time the popover is shown (after content
    /// is parented but before the animation starts). Use this to refresh data
    /// in reuse mode — e.g. updating the calendar to today's date.
    on_show: RefCell<Option<Rc<dyn Fn()>>>,
    /// Shared animation state driven by the tick callback.
    anim_state: Rc<RefCell<AnimState>>,
    /// Generation counter incremented on every show/hide to cancel stale
    /// tick callbacks and idle callbacks.
    anim_generation: Rc<Cell<u32>>,
    /// Logical open state. True from the moment show() is called until
    /// hide() is called. Used by is_visible() so the toggle logic in BaseWidget works correctly
    /// even while a close animation is in flight.
    logically_open: Cell<bool>,
    /// Set when `mark_content_dirty()` is called while the popover is not
    /// logically open (e.g. a notification arrives during the close animation).
    /// Checked and cleared on mid-close reversal so the content gets rebuilt.
    content_dirty: Cell<bool>,
    /// When true, the builder is called only once and the content widget is
    /// cached across open/close cycles. On subsequent opens the cached widget
    /// is re-parented into the anim shell instead of calling the builder again.
    ///
    /// This avoids per-cycle widget allocation which is observed to leak memory
    /// in GTK4 for widgets with complex internal trees (e.g. Calendar).
    reuse_content: Cell<bool>,
    /// Cached content widget for reuse mode. Kept alive across close cycles
    /// so it can be re-parented on the next open.
    cached_content: RefCell<Option<gtk4::Widget>>,
    /// One-shot key controller installed by `prepare_keyboard_nav()`.
    /// Stored so `hide()` can remove it if Tab was never pressed.
    deferred_kbd_controller: RefCell<Option<EventControllerKey>>,
}

impl LayerShellPopover {
    /// Create a new layer-shell popover.
    ///
    /// # Arguments
    ///
    /// * `app` - The GTK application
    /// * `widget_name` - Widget name for CSS classes (e.g., "clock")
    /// * `builder` - Function that builds the popover content
    pub fn new<F>(app: &Application, widget_name: &str, builder: F) -> Rc<Self>
    where
        F: Fn() -> gtk4::Widget + 'static,
    {
        Rc::new(Self {
            app: app.clone(),
            widget_name: widget_name.to_string(),
            builder: Rc::new(builder),
            window: RefCell::new(None),
            click_catcher: RefCell::new(None),
            anim_shell: RefCell::new(None),
            anchor_x: Cell::new(0),
            anchor_monitor: RefCell::new(None),
            on_close: RefCell::new(None),
            on_show: RefCell::new(None),
            anim_state: Rc::new(RefCell::new(AnimState::new_idle())),
            anim_generation: Rc::new(Cell::new(0)),
            logically_open: Cell::new(false),
            content_dirty: Cell::new(false),
            reuse_content: Cell::new(false),
            cached_content: RefCell::new(None),
            deferred_kbd_controller: RefCell::new(None),
        })
    }

    /// Check if the popover is logically open.
    ///
    /// Returns `true` from the moment `show_at()` is called until `hide()`
    /// is called, even though the window may still be visible during the close
    /// animation. This is critical for the toggle logic in `BaseWidget` to
    /// work correctly during rapid clicking.
    pub fn is_visible(&self) -> bool {
        self.logically_open.get()
    }

    /// Set a callback to be invoked when the popover is hidden.
    pub fn set_on_close<F: Fn() + 'static>(&self, callback: F) {
        *self.on_close.borrow_mut() = Some(Rc::new(callback));
    }

    /// Set a callback to be invoked every time the popover is shown.
    ///
    /// In reuse mode this is called after the cached content is re-parented,
    /// allowing consumers to refresh data (e.g. update calendar to today).
    pub fn set_on_show<F: Fn() + 'static>(&self, callback: F) {
        *self.on_show.borrow_mut() = Some(Rc::new(callback));
    }

    /// Enable content reuse mode.
    ///
    /// When enabled, the builder is called only once and the resulting widget
    /// is cached. On subsequent opens the cached widget is re-parented into
    /// the anim shell instead of calling the builder again.
    pub fn set_reuse_content(&self, reuse: bool) {
        self.reuse_content.set(reuse);
    }

    /// Mark the popover content as needing a rebuild.
    ///
    /// Called by `MenuHandle::refresh_if_visible()` when the popover is not
    /// logically open (e.g. a notification arrives during the close animation).
    /// The flag is checked on mid-close reversal so the stale content gets
    /// replaced before the user sees it again.
    pub fn mark_content_dirty(&self) {
        self.content_dirty.set(true);
    }

    /// Enable keyboard navigation by removing the `.vp-no-focus` CSS class
    /// from the outer wrapper and enabling GTK's `focus-visible` property so
    /// Adwaita renders `:focus-visible` rings on focused widgets.
    ///
    /// Activated by the deferred Tab controller installed in `show_internal()`.
    /// On `hide()`, `focus-visible` is reset to `false` and `.vp-no-focus`
    /// is restored so the next open starts focus-suppressed.
    pub fn enable_keyboard_nav(&self) {
        if let Some(ref window) = *self.window.borrow()
            && let Some(child) = window.child()
        {
            gtk4::prelude::GtkWindowExt::set_focus_visible(window, true);
            child.remove_css_class(surface::NO_FOCUS);
        }
    }

    /// Prepare deferred keyboard navigation.
    ///
    /// Clears any auto-focus set by `present()` and installs a one-shot key
    /// controller that waits for a keynav key (Tab, arrows, Home, End).
    /// On the first such press, `enable_keyboard_nav()` fires and focus
    /// lands on the first focusable widget with correct `:focus-visible`
    /// state. Until a keynav key is pressed, the popover shows no focus rings.
    pub fn prepare_keyboard_nav(self: &Rc<Self>) {
        let Some(ref window) = *self.window.borrow() else {
            return;
        };

        // Clear any auto-focus from present() so Tab starts from nothing
        // and lands on the first focusable widget.
        gtk4::prelude::GtkWindowExt::set_focus(window, None::<&gtk4::Widget>);

        // Remove any previous deferred controller (e.g. rapid toggle).
        self.remove_deferred_kbd_controller();

        let controller = EventControllerKey::new();
        let weak_self = Rc::downgrade(self);
        let ctrl_ref = controller.clone();
        controller.connect_key_pressed(move |_, keyval, _, _| {
            let is_keynav = is_keynav_key(keyval);
            if is_keynav && let Some(popover) = weak_self.upgrade() {
                popover.enable_keyboard_nav();
                if let Some(ref window) = *popover.window.borrow() {
                    window.remove_controller(&ctrl_ref);
                }
                *popover.deferred_kbd_controller.borrow_mut() = None;
            }
            if keyval == gdk::Key::Tab || keyval == gdk::Key::ISO_Left_Tab {
                // Let Tab propagate — GTK focuses the first widget with
                // correct :focus-visible via its own keynav path.
                Propagation::Proceed
            } else if is_keynav {
                // For arrows/Home/End, consume the key and simulate Tab's
                // focus behavior so we land on the first widget instead of
                // skipping it.
                if let Some(popover) = weak_self.upgrade()
                    && let Some(ref window) = *popover.window.borrow()
                {
                    window.child_focus(gtk4::DirectionType::TabForward);
                }
                Propagation::Stop
            } else {
                Propagation::Proceed
            }
        });

        window.add_controller(controller.clone());
        *self.deferred_kbd_controller.borrow_mut() = Some(controller);
    }

    /// Remove the deferred keyboard nav controller if installed.
    fn remove_deferred_kbd_controller(&self) {
        if let Some(controller) = self.deferred_kbd_controller.borrow_mut().take()
            && let Some(ref window) = *self.window.borrow()
        {
            window.remove_controller(&controller);
        }
    }

    /// Show the popover at the given anchor position.
    ///
    /// Reuses all persistent shells (window, animation, click-catcher) and
    /// builds fresh content.
    pub fn show_at(self: &Rc<Self>, x: i32, monitor: Option<Monitor>) {
        self.anchor_x.set(x);
        *self.anchor_monitor.borrow_mut() = monitor;
        self.show_internal();
    }

    /// Hide the popover with a close animation, keeping the window shell alive.
    ///
    /// The click-catcher is hidden immediately so the bar is interactive
    /// during the animation. The animation shell fades out via the tick
    /// callback, then content is removed and the window is hidden.
    ///
    /// If the popover is currently opening, the animation smoothly reverses
    /// from the current progress — no snapping.
    ///
    /// The `on_close` callback fires when the close animation **completes**,
    /// not when `hide()` is called.
    pub fn hide(&self) {
        // Mark as logically closed immediately — the toggle logic in BaseWidget
        // checks this to decide show vs hide on the next click.
        self.logically_open.set(false);

        // Restore focus suppression so the next open starts no-focus.
        // (keyboard nav defers removal to enable_keyboard_nav().)
        if let Some(ref window) = *self.window.borrow() {
            gtk4::prelude::GtkWindowExt::set_focus_visible(window, false);
            if let Some(child) = window.child()
                && !child.has_css_class(surface::NO_FOCUS)
            {
                child.add_css_class(surface::NO_FOCUS);
            }
        }
        self.remove_deferred_kbd_controller();

        // Bump generation to cancel any pending idle callback from show_internal().
        let generation = self.anim_generation.get().wrapping_add(1);
        self.anim_generation.set(generation);

        // Hide click-catcher immediately so bar is interactive during animation.
        if let Some(ref catcher) = *self.click_catcher.borrow() {
            catcher.set_visible(false);
        }

        let window = self.window.borrow().as_ref().cloned();
        let anim_shell = self.anim_shell.borrow().as_ref().cloned();

        let Some(window) = window else {
            return;
        };

        // Release keyboard grab while hiding.
        window.set_keyboard_mode(KeyboardMode::None);

        // If animations are disabled, snap closed immediately.
        if !ConfigManager::global().animations_enabled() {
            if let Some(ref shell) = anim_shell {
                shell.set_opacity(0.0);
                shell.set_scale(ANIM_SCALE_FROM);
                shell.remove_child();
            }
            // No explicit blur removal needed — unmapping suspends
            // compositor-side blur while the protocol object persists.
            // Blur is re-applied on next map via connect_map.
            window.set_visible(false);
            // Fire on_close now since there's no animation to wait for.
            if let Some(ref cb) = *self.on_close.borrow() {
                cb();
            }
            return;
        }

        // Ensure the window is fully visible (the idle callback from show_internal
        // may not have fired yet, leaving window.opacity at 0.0).
        window.set_opacity(1.0);

        // Remove blur immediately so the compositor stops drawing it while the
        // surface fades out.  Blur is a compositor effect independent of surface
        // opacity — if left in place it would remain visible as the content
        // becomes transparent.
        if let Some(blur) = crate::services::background_effect::BackgroundEffectManager::global() {
            blur.remove_blur_region(&window);
        }

        // Start (or reverse into) the close animation.
        // on_close fires when the animation completes (in the tick callback).
        self.start_animation(AnimDirection::Closing, generation);
    }

    /// Rebuild the popover content in-place without any animation.
    ///
    /// Used by `MenuHandle::refresh_if_visible()` to hot-swap content while the
    /// popover is already open (e.g. a new notification arrives). This avoids
    /// the hide→show cycle which would trigger the mid-close reversal path and
    /// skip the content rebuild.
    pub fn rebuild_content(&self) {
        let Some(anim_shell) = self.anim_shell.borrow().as_ref().cloned() else {
            return;
        };

        anim_shell.remove_child();

        // Invalidate cache so the builder runs fresh.
        *self.cached_content.borrow_mut() = None;

        let content = (self.builder)();
        content.add_css_class(surface::POPOVER);
        content.add_css_class(surface::SURFACE_POPOVER);
        content.add_css_class(surface::WIDGET_MENU);
        let popover_class = format!("{}-popover", self.widget_name);
        content.add_css_class(&popover_class);

        // Re-cache if in reuse mode.
        if self.reuse_content.get() {
            *self.cached_content.borrow_mut() = Some(content.clone());
        }

        anim_shell.set_child(&content);

        SurfaceStyleManager::global().apply_pango_attrs_all(&anim_shell);
    }

    fn show_internal(self: &Rc<Self>) {
        // Mark as logically open immediately.
        self.logically_open.set(true);

        // If we're currently animating a close, the window is still visible
        // with content — just reverse the animation direction. No need to
        // rebuild content, recreate click-catcher, etc.
        let was_closing = {
            let state = self.anim_state.borrow();
            state.active && state.direction == AnimDirection::Closing
        };

        if was_closing {
            // Content may have become stale during the close animation (e.g. a
            // notification arrived while logically_open was false). Rebuild now
            // so the user doesn't see outdated content when the reversal
            // completes.
            if self.content_dirty.take() {
                self.rebuild_content();
            }

            // Use the CURRENT generation (set by hide()) so the existing tick
            // callback stays valid — no new closure allocation needed.
            let generation = self.anim_generation.get();
            // Re-show click-catcher (hide() hid it).
            let catcher = self.ensure_click_catcher();
            if let Some(ref monitor) = *self.anchor_monitor.borrow() {
                catcher.set_monitor(Some(monitor));
            }
            catcher.set_margin(popover_bar_edge(), calculate_bar_exclusive_zone());
            catcher.set_visible(true);

            // Restore keyboard mode (hide() set it to None).
            if let Some(ref window) = *self.window.borrow() {
                window.set_keyboard_mode(popover_keyboard_mode());
            }

            // Anchor may have changed since the original open.
            self.update_position();

            // Reverse into opening — tick callback picks up new direction.
            self.start_animation(AnimDirection::Opening, generation);

            // Install deferred Tab controller so keyboard nav activates on Tab.
            self.prepare_keyboard_nav();
            return;
        }

        // Not mid-close — full open from scratch.
        // Fresh content will be built below, so clear any pending dirty flag.
        self.content_dirty.set(false);
        // Bump generation to cancel any stale tick callbacks or idle callbacks.
        let generation = self.anim_generation.get().wrapping_add(1);
        self.anim_generation.set(generation);

        // If the window is somehow still visible (shouldn't happen with
        // logically_open guard, but be defensive), hide it synchronously.
        if self
            .window
            .borrow()
            .as_ref()
            .is_some_and(|w| w.is_visible())
        {
            // Snap-close without animation to avoid recursion.
            if let Some(ref shell) = *self.anim_shell.borrow() {
                shell.set_opacity(0.0);
                shell.set_scale(ANIM_SCALE_FROM);
                shell.remove_child();
            }
            if let Some(ref window) = *self.window.borrow() {
                window.set_visible(false);
            }
        }

        let window = self.ensure_window_shell();

        let anim_shell = self.ensure_anim_shell();

        anim_shell.remove_child();

        // Get or build content. In reuse mode, the builder is called only once
        // and the widget is cached for subsequent opens. This avoids per-cycle
        // widget allocation which leaks memory in GTK4 for complex widgets.
        let content = if self.reuse_content.get() {
            if let Some(ref cached) = *self.cached_content.borrow() {
                cached.clone()
            } else {
                let fresh = (self.builder)();
                fresh.add_css_class(surface::POPOVER);
                fresh.add_css_class(surface::SURFACE_POPOVER);
                fresh.add_css_class(surface::WIDGET_MENU);
                let popover_class = format!("{}-popover", self.widget_name);
                fresh.add_css_class(&popover_class);
                *self.cached_content.borrow_mut() = Some(fresh.clone());
                fresh
            }
        } else {
            let fresh = (self.builder)();
            fresh.add_css_class(surface::POPOVER);
            fresh.add_css_class(surface::SURFACE_POPOVER);
            fresh.add_css_class(surface::WIDGET_MENU);
            let popover_class = format!("{}-popover", self.widget_name);
            fresh.add_css_class(&popover_class);
            fresh
        };

        anim_shell.set_child(&content);

        // Fire on_show callback (e.g. to refresh calendar to today's date).
        if let Some(ref cb) = *self.on_show.borrow() {
            cb();
        }

        SurfaceStyleManager::global().apply_pango_attrs_all(&anim_shell);

        if let Some(ref monitor) = *self.anchor_monitor.borrow() {
            window.set_monitor(Some(monitor));
        }

        // Set the shell to the hidden state (will be animated to visible).
        anim_shell.set_opacity(0.0);
        anim_shell.set_scale(ANIM_SCALE_FROM);

        // Ensure the outer wrapper is set as the window's child (persists).
        if window.child().is_none() {
            let outer = GtkBox::new(Orientation::Vertical, 0);
            outer.add_css_class(surface::POPOVER_WRAPPER);
            outer.add_css_class(surface::WIDGET_MENU_WRAPPER);
            outer.add_css_class(surface::NO_FOCUS);
            SurfaceStyleManager::global().apply_shadow_margins(&outer, POPOVER_SHADOW_MARGIN);
            outer.append(&anim_shell);
            window.set_child(Some(&outer));
        }

        // Restore keyboard mode (hide() sets it to None).
        window.set_keyboard_mode(popover_keyboard_mode());

        // Show click-catcher (persistent, created lazily).
        let catcher = self.ensure_click_catcher();
        if let Some(ref monitor) = *self.anchor_monitor.borrow() {
            catcher.set_monitor(Some(monitor));
        }
        catcher.set_margin(popover_bar_edge(), calculate_bar_exclusive_zone());
        catcher.set_visible(true);

        // Show window with opacity trick to avoid flicker during positioning.
        window.set_opacity(0.0);
        window.set_visible(true);
        window.present();

        // Install deferred Tab controller so keyboard nav activates on first
        // Tab press (for both mouse and IPC opens). Must run after present()
        // because present() auto-focuses a widget which we need to clear.
        self.prepare_keyboard_nav();

        // After window is mapped, update position and start the open animation.
        let weak_self = Rc::downgrade(self);
        let gen_rc = Rc::clone(&self.anim_generation);
        glib::idle_add_local_once(move || {
            // Bail if a newer show/hide cycle started before this idle fired.
            if gen_rc.get() != generation {
                return;
            }

            if let Some(popover) = weak_self.upgrade() {
                popover.update_position();
                if let Some(ref window) = *popover.window.borrow() {
                    window.set_opacity(1.0);
                }

                if ConfigManager::global().animations_enabled() {
                    popover.start_animation(AnimDirection::Opening, generation);
                } else {
                    // Animations disabled — snap open immediately.
                    if let Some(ref shell) = *popover.anim_shell.borrow() {
                        shell.set_opacity(1.0);
                        shell.set_scale(1.0);
                    }
                }
            }
        });
    }

    /// Ensure the window shell exists, creating it lazily if needed.
    ///
    /// The shell includes the `ApplicationWindow`, layer-shell configuration,
    /// and ESC key handler — but no content. Content is set by `show_internal()`
    /// on each open.
    fn ensure_window_shell(self: &Rc<Self>) -> ApplicationWindow {
        if let Some(ref window) = *self.window.borrow() {
            return window.clone();
        }

        let window = ApplicationWindow::builder()
            .application(&self.app)
            .title(format!("vibepanel {} popover", self.widget_name))
            .decorated(false)
            .resizable(false)
            .build();

        // CSS classes
        window.add_css_class(surface::LAYER_SHELL_POPOVER);

        // Layer shell configuration.
        // Use Top layer (not Overlay) to avoid appearing on top of fullscreen apps.
        window.init_layer_shell();
        window.set_namespace(Some(&format!("vibepanel-{}-popover", self.widget_name)));
        window.set_layer(Layer::Top);
        window.set_exclusive_zone(0);
        let is_bottom = ConfigManager::global().bar_is_bottom();
        window.set_anchor(Edge::Top, !is_bottom);
        window.set_anchor(Edge::Right, true);
        window.set_anchor(Edge::Bottom, is_bottom);
        window.set_anchor(Edge::Left, false);
        window.set_keyboard_mode(popover_keyboard_mode());

        // ESC key handler
        {
            let weak_self = Rc::downgrade(self);
            setup_esc_handler(&window, move || {
                if let Some(popover) = weak_self.upgrade() {
                    popover.hide();
                }
            });
        }

        // Apply blur on every map (first show and re-show).  Close calls
        // set_visible(false) which unmaps the surface, so connect_map fires
        // again when the window is re-shown.  On first map the surface has no
        // size yet, so apply_blur_region defers via idle.  On re-show it sets
        // the full-size region, which the animation tick overwrites with a
        // scaled region within 1-2 frames.
        //
        // The else-branch removes any stale protocol object left from a
        // previous map cycle.  This handles the case where blur was enabled
        // when the popover was last shown, then disabled while the popover
        // was hidden (unmapped).  `remove_blur_region` requires a mapped
        // surface, so connect_map is the earliest reliable cleanup point.
        //
        // Known limitation: config changes to `theme.blur` or border radius
        // while the popover is open take effect on next open, not immediately.
        // Popovers grab focus so config edits are unlikely while open.
        window.connect_map(move |win| {
            if ConfigManager::global().blur_enabled() {
                if let Some(blur) =
                    crate::services::background_effect::BackgroundEffectManager::global()
                {
                    blur.apply_blur_region(win, POPOVER_SHADOW_MARGIN);
                }
            } else if let Some(blur) =
                crate::services::background_effect::BackgroundEffectManager::global()
            {
                blur.remove_blur_region(win);
            }
        });

        *self.window.borrow_mut() = Some(window.clone());
        window
    }

    /// Ensure the persistent animation shell exists, creating it lazily.
    ///
    /// The animation shell is a `ScaleBox` whose child (builder content) is
    /// swapped on each show. It is **never destroyed** and carries no styling —
    /// it is a pure transparent animation wrapper. Visual styles (background,
    /// padding, border-radius) live on the content widget via CSS classes
    /// resolved by the global stylesheet.
    fn ensure_anim_shell(&self) -> ScaleBox {
        if let Some(ref shell) = *self.anim_shell.borrow() {
            return shell.clone();
        }

        let shell = ScaleBox::new();

        // Start fully hidden (opacity 0, scale at starting value).
        shell.set_opacity(0.0);
        shell.set_scale(ANIM_SCALE_FROM);

        *self.anim_shell.borrow_mut() = Some(shell.clone());
        shell
    }

    /// Ensure the persistent click-catcher exists, creating it lazily.
    ///
    /// The click-catcher is shown/hidden each cycle rather than created/destroyed
    /// to avoid per-cycle allocation of an `ApplicationWindow` + layer-shell surface.
    fn ensure_click_catcher(self: &Rc<Self>) -> ApplicationWindow {
        if let Some(ref catcher) = *self.click_catcher.borrow() {
            return catcher.clone();
        }

        let bar_zone = calculate_bar_exclusive_zone();
        let weak_self = Rc::downgrade(self);
        let catcher = create_click_catcher(&self.app, bar_zone, move || {
            if let Some(popover) = weak_self.upgrade() {
                popover.hide();
            }
        });

        *self.click_catcher.borrow_mut() = Some(catcher.clone());
        catcher
    }

    /// Start or reverse the open/close animation via a tick callback.
    ///
    /// If an animation is already in flight (e.g., opening and user clicks to
    /// close), the current progress is captured and the animation reverses from
    /// that point with proportional timing — no snapping.
    fn start_animation(&self, direction: AnimDirection, generation: u32) {
        let anim_shell = self.anim_shell.borrow().as_ref().cloned();
        let Some(anim_shell) = anim_shell else {
            return;
        };

        // Cache the current border radius for the duration of this animation.
        anim_shell.set_radius(ConfigManager::global().surface_border_radius() as f32);

        let start_time_us = anim_shell
            .frame_clock()
            .map(|fc| fc.frame_time())
            .unwrap_or(0);

        let need_tick = self.anim_state.borrow_mut().prepare(
            direction,
            generation,
            start_time_us,
            anim_shell.opacity(),
        );

        if !need_tick {
            return;
        }

        let anim_state = Rc::clone(&self.anim_state);
        let anim_gen = Rc::clone(&self.anim_generation);
        let window = self.window.borrow().as_ref().cloned();
        let shell_for_scale = anim_shell.clone();
        let on_close = self.on_close.borrow().clone();

        anim_shell.add_tick_callback(move |shell, frame_clock| {
            // Generation check — bail if a newer cycle started.
            // Do NOT touch `active` — a newer tick callback owns that now.
            if anim_gen.get() != generation {
                return ControlFlow::Break;
            }

            let now_us = frame_clock.frame_time();
            let (progress, complete, direction) = {
                let state = anim_state.borrow();
                if !state.active {
                    return ControlFlow::Break;
                }
                (
                    state.current_progress(now_us),
                    state.is_complete(now_us),
                    state.direction,
                )
            };

            // Apply visual state — opacity and scale, no CSS involvement.
            shell.set_opacity(progress);
            // Interpolate scale: ANIM_SCALE_FROM at progress=0 → 1.0 at progress=1.
            let scale = ANIM_SCALE_FROM + (1.0 - ANIM_SCALE_FROM) * progress;
            shell_for_scale.set_scale(scale);

            if direction == AnimDirection::Opening
                && ConfigManager::global().blur_enabled()
                && let Some(blur) =
                    crate::services::background_effect::BackgroundEffectManager::global()
                && let Some(ref w) = window
            {
                blur.apply_open_animation_blur(w, POPOVER_SHADOW_MARGIN, scale, complete);
            }

            if complete {
                anim_state.borrow_mut().active = false;

                if direction == AnimDirection::Closing {
                    // Close complete — remove content and hide window.
                    shell.set_opacity(0.0);
                    shell_for_scale.set_scale(ANIM_SCALE_FROM);
                    shell_for_scale.remove_child();
                    if let Some(ref w) = window {
                        w.set_visible(false);
                    }
                    // Fire on_close now that the popover is fully hidden.
                    if let Some(ref cb) = on_close {
                        cb();
                    }
                } else {
                    // Open complete — ensure we're at exactly 1.0.
                    shell.set_opacity(1.0);
                    shell_for_scale.set_scale(1.0);
                }
                return ControlFlow::Break;
            }

            ControlFlow::Continue
        });
    }

    fn update_position(&self) {
        let Some(ref window) = *self.window.borrow() else {
            return;
        };

        let anchor_x = self.anchor_x.get();

        // Get monitor from anchor or fall back to primary
        let monitor_opt = self.anchor_monitor.borrow().clone().or_else(|| {
            gdk::Display::default().and_then(|display| {
                display
                    .monitors()
                    .item(0)
                    .and_then(|obj| obj.downcast::<Monitor>().ok())
            })
        });

        let Some(monitor) = monitor_opt else {
            return;
        };

        let geom = monitor.geometry();

        // Set margin on the bar-adjacent edge
        let bar_edge = popover_bar_edge();
        window.set_margin(bar_edge, calculate_popover_bar_margin());

        // Calculate horizontal position (center on anchor_x)
        if anchor_x > 0 {
            let window_width = {
                let w = window.width();
                if w > POPOVER_MIN_VALID_WIDTH {
                    w
                } else {
                    POPOVER_DEFAULT_WIDTH_ESTIMATE
                }
            };
            let right_margin = calculate_popover_right_margin(
                anchor_x,
                geom.width(),
                window_width,
                POPOVER_MIN_EDGE_MARGIN,
            );
            window.set_margin(Edge::Right, right_margin);
        } else {
            let fallback_margin =
                SurfaceStyleManager::global().shadow_margin(POPOVER_SHADOW_MARGIN);
            window.set_margin(Edge::Right, fallback_margin);
        }
    }
}

/// Trait for surfaces that can be dismissed.
pub trait Dismissible {
    fn dismiss(&self);
    fn is_visible(&self) -> bool;
}

impl Drop for LayerShellPopover {
    fn drop(&mut self) {
        // If the popover was still open (or mid-animation) when destroyed,
        // fire on_close synchronously so consumers can clean up resources
        // (e.g. SystemPopoverBinding releases GPU polling).
        if (self.logically_open.get() || self.anim_state.borrow().active)
            && let Some(ref cb) = *self.on_close.borrow()
        {
            cb();
        }

        if let Some(catcher) = self.click_catcher.borrow_mut().take() {
            catcher.close();
        }
        // Best-effort blur cleanup; primary removal happens at fade-start
        // in hide().  May no-op if already unmapped.
        // See BackgroundEffectManager::remove_blur_region docs.
        if let Some(blur) = crate::services::background_effect::BackgroundEffectManager::global()
            && let Some(ref window) = *self.window.borrow()
        {
            blur.remove_blur_region(window);
        }
        if let Some(window) = self.window.borrow_mut().take() {
            window.close();
        }
    }
}

impl Dismissible for LayerShellPopover {
    fn dismiss(&self) {
        self.hide();
    }

    fn is_visible(&self) -> bool {
        self.is_visible()
    }
}
