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
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

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

/// Duration of the popover open/close animation.
/// Derived from the single source of truth in `css::POPOVER_ANIMATION_MS`.
pub const POPOVER_ANIMATION_DURATION: Duration =
    Duration::from_millis(super::css::POPOVER_ANIMATION_MS);

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

/// Set up ESC key handler on a window to call the dismiss callback.
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
/// Content is built fresh on each `show()` via the builder closure, placed
/// inside a persistent `ScaleBox` animation shell.
///
/// Open/close animation is a simple opacity snap + timeout: the `ScaleBox`
/// opacity is set to 0 immediately on hide, then the window is hidden and
/// `on_close` fires after `POPOVER_ANIMATION_DURATION`.
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
    /// Generation counter incremented on every show/hide to cancel stale
    /// idle callbacks and timeout callbacks.
    generation: Rc<Cell<u32>>,
    /// Logical open state. True from the moment show() is called until
    /// hide() is called. Used by is_visible() so the toggle logic in BaseWidget works correctly
    /// even while a close animation is in flight.
    logically_open: Cell<bool>,
    /// Set when `mark_content_dirty()` is called while the popover is not
    /// logically open (e.g. a notification arrives during the close animation).
    /// Checked and cleared on the next show so the content gets rebuilt.
    content_dirty: Cell<bool>,
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
            generation: Rc::new(Cell::new(0)),
            logically_open: Cell::new(false),
            content_dirty: Cell::new(false),
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

    /// Mark the popover content as needing a rebuild.
    ///
    /// Called by `MenuHandle::refresh_if_visible()` when the popover is not
    /// logically open (e.g. a notification arrives during the close animation).
    /// The flag is checked on the next show so the stale content gets
    /// replaced before the user sees it again.
    pub fn mark_content_dirty(&self) {
        self.content_dirty.set(true);
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

    /// Hide the popover, keeping the window shell alive.
    ///
    /// The click-catcher is hidden immediately so the bar is interactive
    /// during the animation. The animation shell snaps to opacity 0,
    /// then a timeout hides the window and fires `on_close`.
    pub fn hide(&self) {
        // Mark as logically closed immediately — the toggle logic in BaseWidget
        // checks this to decide show vs hide on the next click.
        self.logically_open.set(false);

        // Bump generation to cancel any pending idle callback from show_internal().
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);

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

        // Snap the animation shell to hidden state.
        if let Some(ref shell) = anim_shell {
            shell.set_opacity(0.0);
        }

        // If animations are disabled, hide window immediately.
        if !ConfigManager::global().animations_enabled() {
            if let Some(ref shell) = anim_shell {
                shell.remove_child();
            }
            window.set_visible(false);
            if let Some(ref cb) = *self.on_close.borrow() {
                cb();
            }
            return;
        }

        // Hide the window and fire on_close after the animation duration.
        let on_close = self.on_close.borrow().clone();
        let gen_rc = Rc::clone(&self.generation);
        glib::timeout_add_local_once(POPOVER_ANIMATION_DURATION, move || {
            // Bail if a newer show/hide cycle started.
            if gen_rc.get() != generation {
                return;
            }
            if let Some(ref shell) = anim_shell {
                shell.remove_child();
            }
            window.set_visible(false);
            if let Some(ref cb) = on_close {
                cb();
            }
        });
    }

    /// Rebuild the popover content in-place without any animation.
    ///
    /// Used by `MenuHandle::refresh_if_visible()` to hot-swap content while the
    /// popover is already open (e.g. a new notification arrives). This avoids
    /// the hide→show cycle which would trigger unnecessary animation.
    pub fn rebuild_content(&self) {
        let Some(anim_shell) = self.anim_shell.borrow().as_ref().cloned() else {
            return;
        };

        anim_shell.remove_child();

        let content = (self.builder)();
        content.add_css_class(surface::POPOVER);
        let popover_class = format!("{}-popover", self.widget_name);
        content.add_css_class(&popover_class);

        anim_shell.set_child(&content);

        SurfaceStyleManager::global().apply_pango_attrs_all(&anim_shell);
    }

    fn show_internal(self: &Rc<Self>) {
        // Mark as logically open immediately.
        self.logically_open.set(true);

        // Fresh content will be built below, so clear any pending dirty flag.
        self.content_dirty.set(false);

        // Bump generation to cancel any stale timeout or idle callbacks.
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);

        // If the window is somehow still visible (shouldn't happen with
        // logically_open guard, but be defensive), hide it synchronously.
        if self
            .window
            .borrow()
            .as_ref()
            .is_some_and(|w| w.is_visible())
        {
            if let Some(ref shell) = *self.anim_shell.borrow() {
                shell.set_opacity(0.0);
                shell.remove_child();
            }
            if let Some(ref window) = *self.window.borrow() {
                window.set_visible(false);
            }
        }

        let window = self.ensure_window_shell();

        let anim_shell = self.ensure_anim_shell();

        anim_shell.remove_child();

        // Build fresh content from the builder closure.
        let content = (self.builder)();
        content.add_css_class(surface::POPOVER);
        let popover_class = format!("{}-popover", self.widget_name);
        content.add_css_class(&popover_class);

        anim_shell.set_child(&content);

        SurfaceStyleManager::global().apply_pango_attrs_all(&anim_shell);

        if let Some(ref monitor) = *self.anchor_monitor.borrow() {
            window.set_monitor(Some(monitor));
        }

        // Set the shell to the hidden state (will be revealed after positioning).
        anim_shell.set_opacity(0.0);

        // Ensure the outer wrapper is set as the window's child (persists).
        if window.child().is_none() {
            let outer = GtkBox::new(Orientation::Vertical, 0);
            outer.add_css_class(surface::WIDGET_MENU);
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

        // After window is mapped, update position and reveal.
        let weak_self = Rc::downgrade(self);
        let gen_rc = Rc::clone(&self.generation);
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
                if let Some(ref shell) = *popover.anim_shell.borrow() {
                    shell.set_opacity(1.0);
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

        // Start fully hidden.
        shell.set_opacity(0.0);

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
        // If the popover was still open when destroyed, fire on_close
        // synchronously so consumers can clean up resources
        // (e.g. SystemPopoverBinding releases GPU polling).
        if self.logically_open.get() {
            if let Some(ref cb) = *self.on_close.borrow() {
                cb();
            }
        }

        if let Some(catcher) = self.click_catcher.borrow_mut().take() {
            catcher.close();
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
