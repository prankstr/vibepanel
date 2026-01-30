//! Layer shell popover infrastructure for widget menus.
//!
//! This module provides a unified approach to creating popup menus that:
//! - Use layer-shell surfaces for proper keyboard focus and focus return
//! - Have click-catcher overlays for click-outside-to-close behavior
//! - Handle ESC key to close
//! - Support seamless transitions between menus
//!
//! # Architecture
//!
//! The module provides two levels of abstraction:
//!
//! 1. **Helper functions** - Low-level utilities that can be used by any layer-shell
//!    surface (like Quick Settings) that needs click-catcher or focus handling.
//!
//! 2. **`LayerShellPopover`** - A complete popover solution for simple widget menus
//!    that handles window creation, positioning, click-catcher, and lifecycle.
//!
//! # Usage
//!
//! For simple widget menus, use `LayerShellPopover`:
//!
//! ```ignore
//! let popover = LayerShellPopover::new(app, "clock", move || {
//!     build_calendar_content()
//! });
//! popover.show_at(anchor_x, Some(monitor));
//! ```
//!
//! For complex surfaces like Quick Settings, use the helper functions directly:
//!
//! ```ignore
//! let catcher = create_click_catcher(app, || { qs.hide_panel(); });
//! setup_esc_handler(&window, || { qs.hide_panel(); });
//! setup_focus_loss_handler(&window, || { qs.hide_panel(); });
//! ```

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

use crate::services::compositor::CompositorManager;
use crate::services::config_manager::ConfigManager;
use crate::services::surfaces::SurfaceStyleManager;
use crate::styles::{class, surface};

// =============================================================================
// Helper Functions - Shared Infrastructure
// =============================================================================

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

/// Create a fullscreen click-catcher window for click-outside-to-close behavior.
///
/// The click-catcher covers the entire screen except the bar area (via top margin).
/// When clicked, it calls `on_dismiss`. The top margin ensures bar widgets remain
/// clickable for seamless transitions between menus.
///
/// # Arguments
///
/// * `app` - The GTK application
/// * `on_dismiss` - Callback invoked when the catcher is clicked
///
/// # Returns
///
/// The click-catcher window. Caller is responsible for showing it and storing it.
pub fn create_click_catcher<F>(app: &Application, on_dismiss: F) -> ApplicationWindow
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

    // Layer shell configuration - fullscreen overlay behind the popover
    catcher.init_layer_shell();
    catcher.set_layer(Layer::Overlay);
    catcher.set_exclusive_zone(-1); // Cover everything
    catcher.set_anchor(Edge::Top, true);
    catcher.set_anchor(Edge::Bottom, true);
    catcher.set_anchor(Edge::Left, true);
    catcher.set_anchor(Edge::Right, true);
    catcher.set_keyboard_mode(KeyboardMode::OnDemand);

    // Leave bar area uncovered for seamless transitions
    let bar_zone = calculate_bar_exclusive_zone();
    catcher.set_margin(Edge::Top, bar_zone);

    // Transparent content
    let overlay = GtkBox::new(Orientation::Vertical, 0);
    overlay.set_hexpand(true);
    overlay.set_vexpand(true);
    catcher.set_child(Some(&overlay));

    // Click handler
    let gesture = GestureClick::new();
    gesture.set_button(0); // All buttons
    {
        let on_dismiss = on_dismiss.clone();
        // Use connect_released to allow GTK to complete the gesture lifecycle
        // before hiding windows. This avoids "Broken accounting of active state" warnings.
        gesture.connect_released(move |_gesture, _, _x, _y| {
            on_dismiss();
            // Note: Seamless transitions to bar widgets happen automatically because
            // the click catcher has a top margin that leaves the bar area uncovered.
            // Clicks on bar widgets go directly to them, triggering their click handlers
            // which call PopupTracker::dismiss_active() before opening their menus.
        });
    }
    catcher.add_controller(gesture);

    // ESC key handler
    {
        let on_dismiss = on_dismiss.clone();
        let key_controller = EventControllerKey::new();
        key_controller.connect_key_pressed(move |_, keyval, _, _| {
            if keyval == gdk::Key::Escape {
                on_dismiss();
                Propagation::Stop
            } else {
                Propagation::Proceed
            }
        });
        catcher.add_controller(key_controller);
    }

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

/// Set up auto-close behavior when the window loses focus.
///
/// This is compositor-aware:
/// - **Hyprland**: Uses window-opened events (layer-shell surfaces retain focus)
/// - **Other compositors**: Uses `is-active` property with debouncing
///
/// # Arguments
///
/// * `window` - The window to monitor
/// * `on_close` - Callback invoked when focus is lost and should close
/// * `pending_close` - Cell to store the pending close timeout (for cancellation)
pub fn setup_focus_loss_handler<F>(
    window: &ApplicationWindow,
    on_close: F,
    pending_close: Rc<Cell<Option<glib::SourceId>>>,
) where
    F: Fn() + Clone + 'static,
{
    let compositor_manager = CompositorManager::global();
    let is_hyprland = compositor_manager.backend_name() == "Hyprland";

    if is_hyprland {
        // Hyprland: Subscribe to window-opened events
        let window_weak = window.downgrade();
        let on_close = on_close.clone();
        compositor_manager.register_window_opened_callback(move |window_info| {
            let Some(window) = window_weak.upgrade() else {
                return;
            };

            // Only close if visible
            if !window.is_visible() {
                return;
            }

            // Don't close for vibepanel's own windows
            let app_id_lower = window_info.app_id.to_lowercase();
            if app_id_lower.contains("vibepanel") {
                return;
            }

            // External window opened - close
            on_close();
        });
    } else {
        // Other compositors: Debounced is-active property watch
        let window_weak = window.downgrade();
        let pending_close_inner = pending_close.clone();
        window.connect_notify_local(Some("is-active"), move |window, _| {
            // Cancel any existing pending close
            if let Some(source_id) = pending_close_inner.take() {
                source_id.remove();
            }

            if !window.is_active() {
                // Focus lost - schedule close after short delay
                // Will be cancelled if focus returns quickly (internal click)
                let window_weak = window_weak.clone();
                let on_close = on_close.clone();
                let pending_close_timeout = pending_close_inner.clone();
                let source_id =
                    glib::timeout_add_local_once(Duration::from_millis(50), move || {
                        pending_close_timeout.set(None);
                        if let Some(window) = window_weak.upgrade()
                            && !window.is_active()
                        {
                            on_close();
                        }
                    });
                pending_close_inner.set(Some(source_id));
            }
        });
    }
}

// =============================================================================
// LayerShellPopover - Complete Solution for Widget Menus
// =============================================================================

/// Configuration for positioning a layer-shell popover.
#[derive(Debug, Clone, Default)]
pub struct PopoverAnchor {
    /// X coordinate of the anchor point (widget center) in monitor coordinates.
    pub x: i32,
    /// Target monitor for the popover.
    pub monitor: Option<Monitor>,
}

/// A layer-shell popover for widget menus.
///
/// This provides a complete solution for simple widget menus with:
/// - Layer-shell window for proper focus handling
/// - Click-catcher for click-outside-to-close
/// - ESC key handling
/// - Smart positioning relative to the anchor widget
/// - Seamless transitions to other bar widget menus
///
/// # Lifecycle
///
/// The popover creates fresh windows on each `show()` call and destroys them
/// on `hide()`. This ensures clean state without remembered scroll positions
/// or expanded sections.
pub struct LayerShellPopover {
    app: Application,
    /// Widget name for CSS class generation (e.g., "clock" -> "clock-popover")
    widget_name: String,
    /// Content builder - called each time the popover is shown
    builder: Rc<dyn Fn() -> gtk4::Widget>,

    /// Current window instance (if visible)
    window: RefCell<Option<ApplicationWindow>>,
    /// Current click-catcher instance (if visible)
    click_catcher: RefCell<Option<ApplicationWindow>>,

    /// Anchor position for smart positioning
    anchor: Cell<PopoverAnchor>,

    /// Pending close timeout for debounced focus-loss handling
    pending_close: Rc<Cell<Option<glib::SourceId>>>,
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
            anchor: Cell::new(PopoverAnchor::default()),
            pending_close: Rc::new(Cell::new(None)),
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
        self.anchor.set(PopoverAnchor {
            x,
            monitor: monitor.clone(),
        });
        self.show_internal();
    }

    /// Show the popover using the previously set anchor position.
    #[allow(dead_code)]
    pub fn show(self: &Rc<Self>) {
        self.show_internal();
    }

    /// Hide the popover and destroy windows.
    pub fn hide(&self) {
        // Cancel any pending focus-loss close
        if let Some(source_id) = self.pending_close.take() {
            source_id.remove();
        }

        // Destroy click-catcher
        if let Some(catcher) = self.click_catcher.borrow_mut().take() {
            catcher.close();
        }

        // Destroy main window
        if let Some(window) = self.window.borrow_mut().take() {
            window.close();
        }
    }

    /// Toggle visibility at the given anchor position.
    #[allow(dead_code)]
    pub fn toggle_at(self: &Rc<Self>, x: i32, monitor: Option<Monitor>) {
        if self.is_visible() {
            self.hide();
        } else {
            self.show_at(x, monitor);
        }
    }

    fn show_internal(self: &Rc<Self>) {
        // Create the main window
        let window = self.create_window();

        // Set monitor if specified
        let anchor = self.anchor.take();
        if let Some(ref monitor) = anchor.monitor {
            window.set_monitor(Some(monitor));
        }
        self.anchor.set(anchor);

        // Create and show click-catcher first
        let weak_self = Rc::downgrade(self);
        let catcher = create_click_catcher(&self.app, move || {
            if let Some(popover) = weak_self.upgrade() {
                popover.hide();
            }
        });

        let anchor = self.anchor.take();
        if let Some(ref monitor) = anchor.monitor {
            catcher.set_monitor(Some(monitor));
        }
        self.anchor.set(anchor);

        catcher.set_visible(true);
        *self.click_catcher.borrow_mut() = Some(catcher);

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
        let popover_class = format!("{}-popover", self.widget_name);
        window.add_css_class(&popover_class);

        // Layer shell configuration
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_exclusive_zone(0);
        window.set_anchor(Edge::Top, true);
        window.set_anchor(Edge::Right, true);
        window.set_anchor(Edge::Bottom, false);
        window.set_anchor(Edge::Left, false);
        window.set_keyboard_mode(KeyboardMode::OnDemand);

        // Build content
        let content = (self.builder)();
        content.add_css_class(surface::POPOVER);
        content.add_css_class(&popover_class);

        // Wrap in container with margins for shadow space
        let outer = GtkBox::new(Orientation::Vertical, 0);
        outer.add_css_class(surface::WIDGET_MENU);
        outer.add_css_class(surface::NO_FOCUS);
        outer.set_margin_top(0);
        outer.set_margin_bottom(8);
        outer.set_margin_start(8);
        outer.set_margin_end(8);
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

        // Focus loss handler
        {
            let weak_self = Rc::downgrade(self);
            setup_focus_loss_handler(
                &window,
                move || {
                    if let Some(popover) = weak_self.upgrade() {
                        popover.hide();
                    }
                },
                self.pending_close.clone(),
            );
        }

        window
    }

    fn update_position(&self) {
        let Some(ref window) = *self.window.borrow() else {
            return;
        };

        let anchor = self.anchor.take();
        let anchor_x = anchor.x;

        // Get monitor
        let monitor_opt = anchor.monitor.clone().or_else(|| {
            gdk::Display::default().and_then(|display| {
                display
                    .monitors()
                    .item(0)
                    .and_then(|obj| obj.downcast::<Monitor>().ok())
            })
        });

        self.anchor.set(anchor);

        let Some(monitor) = monitor_opt else {
            return;
        };

        let geom = monitor.geometry();

        // Get bar dimensions from config
        let config_mgr = ConfigManager::global();
        let bar_padding = config_mgr.bar_padding() as i32;
        let bar_opacity = config_mgr.bar_background_opacity();
        let popover_offset = config_mgr.popover_offset() as i32;

        // Calculate top margin
        let top_margin = if bar_opacity > 0.0 {
            popover_offset - bar_padding
        } else {
            popover_offset
        };
        window.set_margin(Edge::Top, top_margin);

        // Calculate horizontal position (center on anchor_x)
        if anchor_x > 0 {
            let monitor_width = geom.width();
            let window_width = {
                let w = window.width();
                if w > 20 {
                    w
                } else {
                    320 // estimate
                }
            };
            let right_margin = monitor_width - anchor_x - window_width / 2;
            let max_margin = monitor_width.saturating_sub(window_width + 4);
            let clamped = if max_margin >= 4 {
                right_margin.clamp(4, max_margin)
            } else {
                4.max(max_margin)
            };
            window.set_margin(Edge::Right, clamped);
        } else {
            window.set_margin(Edge::Right, 8);
        }
    }
}

// =============================================================================
// Dismissible Trait Implementation
// =============================================================================

/// Trait for surfaces that can be dismissed (closed).
///
/// This is implemented by both `LayerShellPopover` and Quick Settings to allow
/// unified handling in `PopupTracker`.
pub trait Dismissible {
    /// Dismiss (hide/close) the surface.
    fn dismiss(&self);

    /// Check if the surface is currently visible.
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
