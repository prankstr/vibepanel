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

/// Calculate the top margin for a popover based on bar configuration.
///
/// When the bar has a visible background (opacity > 0), the popover needs to
/// account for bar padding in its positioning. This ensures consistent visual
/// spacing regardless of bar transparency settings.
///
/// Used by both `LayerShellPopover` and Quick Settings for consistent positioning.
pub fn calculate_popover_top_margin() -> i32 {
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
/// and captures clicks outside the popover to dismiss it. It has a top margin
/// equal to the bar's exclusive zone so clicks on the bar pass through.
///
/// # Arguments
///
/// * `app` - The GTK application
/// * `bar_zone` - Height of the bar's exclusive zone (margin at top to leave bar uncovered)
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
    catcher.set_margin(Edge::Top, bar_zone);

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
/// Creates fresh windows on each `show()` call and destroys them on `hide()`,
/// ensuring clean state without remembered scroll positions or expanded sections.
pub struct LayerShellPopover {
    app: Application,
    widget_name: String,
    builder: Rc<dyn Fn() -> gtk4::Widget>,
    window: RefCell<Option<ApplicationWindow>>,
    click_catcher: RefCell<Option<ApplicationWindow>>,
    /// Anchor X coordinate (widget center) in monitor coordinates.
    anchor_x: Cell<i32>,
    anchor_monitor: RefCell<Option<Monitor>>,
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
            anchor_x: Cell::new(0),
            anchor_monitor: RefCell::new(None),
        })
    }

    /// Check if the popover is currently visible.
    pub fn is_visible(&self) -> bool {
        self.window
            .borrow()
            .as_ref()
            .is_some_and(|w| w.is_visible())
    }

    /// Show the popover at the given anchor position.
    ///
    /// Creates fresh window and click-catcher instances.
    pub fn show_at(self: &Rc<Self>, x: i32, monitor: Option<Monitor>) {
        self.anchor_x.set(x);
        *self.anchor_monitor.borrow_mut() = monitor;
        self.show_internal();
    }

    /// Hide the popover and destroy windows.
    pub fn hide(&self) {
        // Destroy click-catcher
        if let Some(catcher) = self.click_catcher.borrow_mut().take() {
            catcher.close();
        }

        // Destroy main window
        if let Some(window) = self.window.borrow_mut().take() {
            window.close();
        }
    }

    fn show_internal(self: &Rc<Self>) {
        // Guard against re-entrancy: if already visible, hide first to avoid
        // orphaning the old window/click-catcher
        if self.is_visible() {
            self.hide();
        }

        // Create the main window
        let window = self.create_window();

        // Set monitor if specified
        if let Some(ref monitor) = *self.anchor_monitor.borrow() {
            window.set_monitor(Some(monitor));
        }

        // Create and show click-catcher first
        let bar_zone = calculate_bar_exclusive_zone();
        let weak_self = Rc::downgrade(self);
        let catcher = create_click_catcher(&self.app, bar_zone, move || {
            if let Some(popover) = weak_self.upgrade() {
                popover.hide();
            }
        });

        if let Some(ref monitor) = *self.anchor_monitor.borrow() {
            catcher.set_monitor(Some(monitor));
        }

        catcher.set_visible(true);
        *self.click_catcher.borrow_mut() = Some(catcher.clone());

        // Show window with opacity trick to avoid flicker during positioning
        window.set_opacity(0.0);
        window.set_visible(true);
        window.present();

        *self.window.borrow_mut() = Some(window.clone());

        // After window is mapped, update position and fade in
        let weak_self = Rc::downgrade(self);
        glib::idle_add_local(move || {
            if let Some(popover) = weak_self.upgrade() {
                popover.update_position();
                if let Some(ref window) = *popover.window.borrow() {
                    window.set_opacity(1.0);
                }
            }
            ControlFlow::Break
        });
    }

    fn create_window(self: &Rc<Self>) -> ApplicationWindow {
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
        window.set_layer(Layer::Top);
        window.set_exclusive_zone(0);
        window.set_anchor(Edge::Top, true);
        window.set_anchor(Edge::Right, true);
        window.set_anchor(Edge::Bottom, false);
        window.set_anchor(Edge::Left, false);
        window.set_keyboard_mode(popover_keyboard_mode());

        // Build content
        let content = (self.builder)();
        content.add_css_class(surface::POPOVER);
        let popover_class = format!("{}-popover", self.widget_name);
        content.add_css_class(&popover_class);

        // Wrap in container with margins for shadow space
        let outer = GtkBox::new(Orientation::Vertical, 0);
        outer.add_css_class(surface::WIDGET_MENU);
        outer.add_css_class(surface::NO_FOCUS);
        outer.set_margin_top(0);
        outer.set_margin_bottom(POPOVER_SHADOW_MARGIN);
        outer.set_margin_start(POPOVER_SHADOW_MARGIN);
        outer.set_margin_end(POPOVER_SHADOW_MARGIN);
        outer.append(&content);

        // Apply surface styles (background, shadow, font) to the content
        // Note: content does NOT have WIDGET_MENU_CONTENT class, so it gets shadow
        SurfaceStyleManager::global().apply_surface_styles(&content, true);

        // Apply Pango font attributes
        SurfaceStyleManager::global().apply_pango_attrs_all(&outer);

        window.set_child(Some(&outer));

        // ESC key handler
        {
            let weak_self = Rc::downgrade(self);
            setup_esc_handler(&window, move || {
                if let Some(popover) = weak_self.upgrade() {
                    popover.hide();
                }
            });
        }

        window
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

        // Set top margin
        window.set_margin(Edge::Top, calculate_popover_top_margin());

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
            window.set_margin(Edge::Right, POPOVER_SHADOW_MARGIN);
        }
    }
}

/// Trait for surfaces that can be dismissed.
pub trait Dismissible {
    fn dismiss(&self);
    fn is_visible(&self) -> bool;
}

impl Dismissible for LayerShellPopover {
    fn dismiss(&self) {
        self.hide();
    }

    fn is_visible(&self) -> bool {
        self.is_visible()
    }
}
