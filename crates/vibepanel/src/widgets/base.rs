//! Shared base widget abstraction for vibepanel widgets.
//!
//! Provides a thin, reusable wrapper around a root `gtk4::Box` with
//! common CSS classes and helpers for labels, icons, and tooltips.

use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, GestureClick, Label, Orientation, Popover, PositionType};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::services::config_manager::ConfigManager;
use crate::services::icons::{IconHandle, IconsService};
use crate::services::surfaces::SurfaceStyleManager;
use crate::services::tooltip::TooltipManager;
use crate::styles::{class, surface};
use tracing::debug;

// GTK 4.10 deprecated widget-scoped style contexts but didn't provide a replacement.
// We need widget-scoped CSS for per-widget color customization.
// This import is used for StyleContextExt::add_provider().
#[allow(deprecated)]
use gtk4::prelude::StyleContextExt;

/// Minimum distance from screen edge before switching alignment (in pixels).
const EDGE_MARGIN: i32 = 8;

/// Configure a popover with standard settings used across the application.
///
/// This applies:
/// - No arrow
/// - Autohide enabled
/// - `widget-menu` CSS class
/// - Bottom position
/// - Center alignment (will be adjusted dynamically when shown)
/// - Configurable vertical offset (from `bar.popover_offset`) to create gap with widget
pub fn configure_popover(popover: &Popover) {
    popover.set_has_arrow(false);
    popover.set_autohide(true);
    popover.add_css_class(surface::WIDGET_MENU);
    popover.set_position(PositionType::Bottom);
    popover.set_halign(Align::Center);

    // Get the popover offset from config (defaults to 1 if not set)
    let offset = ConfigManager::global().popover_offset() as i32;
    popover.set_offset(0, offset);
}

/// Get widget position and monitor width for smart popover positioning.
///
/// Returns (widget_x, widget_width, monitor_width) or None if unavailable.
fn get_widget_and_monitor_info(widget: &gtk4::Widget) -> Option<(i32, i32, i32)> {
    let native = widget.native()?;
    let bounds = widget.compute_bounds(&native)?;

    let widget_x = bounds.x() as i32;
    let widget_width = bounds.width() as i32;

    // Get monitor width
    let root = widget.root()?;
    let window = root.downcast_ref::<gtk4::Window>()?;
    let surface = window.surface()?;
    let display = gtk4::gdk::Display::default()?;
    let monitor = display.monitor_at_surface(&surface)?;
    let monitor_width = monitor.geometry().width();

    Some((widget_x, widget_width, monitor_width))
}

/// Calculate smart horizontal alignment for a popover based on screen position.
///
/// - Centers the popover if it fits
/// - Aligns to left edge if too close to left side of screen
/// - Aligns to right edge if too close to right side of screen
fn calculate_smart_alignment(
    widget_x: i32,
    widget_width: i32,
    popover_width: i32,
    monitor_width: i32,
) -> Align {
    let widget_center_x = widget_x + widget_width / 2;
    let half_popover = popover_width / 2;

    let popover_left = widget_center_x - half_popover;
    let popover_right = widget_center_x + half_popover;

    if popover_left < EDGE_MARGIN {
        // Too close to left edge - align left edges
        Align::Start
    } else if popover_right > monitor_width - EDGE_MARGIN {
        // Too close to right edge - align right edges
        Align::End
    } else {
        // Enough room - center it
        Align::Center
    }
}

/// Handle for managing a widget menu popover.
pub struct MenuHandle {
    popover: Popover,
    builder: Rc<dyn Fn() -> gtk4::Widget>,
    parent: GtkBox,
    /// Custom background color inherited from the parent widget.
    color: Rc<RefCell<Option<String>>>,
}

impl MenuHandle {
    fn new(
        popover: Popover,
        builder: Rc<dyn Fn() -> gtk4::Widget>,
        parent: GtkBox,
        color: Rc<RefCell<Option<String>>>,
    ) -> Self {
        Self {
            popover,
            builder,
            parent,
            color,
        }
    }

    /// Build or rebuild the popover content.
    ///
    /// On the first call, this creates the content widget and attaches it to
    /// the popover. On subsequent calls it rebuilds the content in place so
    /// dynamic sections (like lists of devices) stay fresh.
    ///
    /// Returns the content widget's preferred width for positioning calculations.
    fn refresh_content(&self) -> i32 {
        let content = (self.builder)();
        content.add_css_class(surface::WIDGET_MENU_CONTENT);

        // Apply surface styling to the content container rather than the Popover shell.
        // If the parent widget has a custom color, pass it as an override.
        let color_override = self.color.borrow();
        SurfaceStyleManager::global().apply_surface_styles(
            &content,
            true,
            color_override.as_deref(),
        );

        self.popover.set_child(Some(&content));

        // Apply Pango font attributes to all labels if enabled in config.
        // This is the central hook for popovers - widgets create standard
        // GTK labels, and we apply Pango attributes here after the tree is built.
        SurfaceStyleManager::global().apply_pango_attrs_all(&content);

        // Measure the content's preferred width for positioning
        let (_, natural_width, _, _) = content.measure(Orientation::Horizontal, -1);
        natural_width
    }

    /// Apply smart positioning based on widget location on screen.
    fn apply_smart_positioning(&self, popover_width: i32) {
        let Some((widget_x, widget_width, monitor_width)) =
            get_widget_and_monitor_info(self.parent.upcast_ref())
        else {
            // Fallback to end alignment if we can't determine position
            self.popover.set_halign(Align::End);
            return;
        };

        let alignment =
            calculate_smart_alignment(widget_x, widget_width, popover_width, monitor_width);

        debug!(
            "Smart popover positioning: widget_x={}, widget_width={}, popover_width={}, monitor_width={}, alignment={:?}",
            widget_x, widget_width, popover_width, monitor_width, alignment
        );

        self.popover.set_halign(alignment);
    }

    pub fn show(&self) {
        // Rebuild content on each show so that it always reflects the
        // latest service state, even if things changed while the menu was
        // closed.
        let popover_width = self.refresh_content();
        self.apply_smart_positioning(popover_width);
        self.popover.popup();
    }

    pub fn hide(&self) {
        self.popover.popdown();
    }

    pub fn toggle(&self) {
        // Use get_visible() instead of is_visible() to avoid ancestry checks
        if self.popover.get_visible() {
            self.hide();
        } else {
            self.show();
        }
    }

    /// Refresh the popover content if it's currently visible.
    ///
    /// This is useful for updating dynamic content (like notification lists)
    /// while the popover is open.
    pub fn refresh_if_visible(&self) {
        if self.popover.get_visible() {
            self.refresh_content();
        }
    }
}

/// Shared base widget container.
///
/// Each widget owns a `BaseWidget` instance and exposes the underlying
/// `gtk4::Box` as its root widget.
///
/// The BaseWidget automatically creates an inner `.content` box for consistent
/// padding and theming across all widgets. Widgets should add their children to
/// `content()` rather than `widget()` directly.
pub struct BaseWidget {
    container: GtkBox,
    content: GtkBox,
    menus: Rc<RefCell<HashMap<String, Rc<MenuHandle>>>>,
    /// Custom background color for this widget and its popovers.
    color: Rc<RefCell<Option<String>>>,
    _gesture_click: GestureClick,
}

impl BaseWidget {
    /// Create a new base widget container.
    ///
    /// - Uses a horizontal box with zero internal spacing (widget-specific
    ///   spacing should be configured by the widget itself).
    /// - Always adds the `widget` CSS class.
    /// - Creates an inner `.content` box for consistent padding/margins.
    /// - Applies any additional CSS classes passed in `extra_classes`.
    /// - If `color` is provided, applies custom background color to the widget.
    pub fn new(extra_classes: &[&str], color: Option<String>) -> Self {
        let container = GtkBox::new(Orientation::Horizontal, 0);
        container.add_css_class(class::WIDGET);
        for cls in extra_classes {
            container.add_css_class(cls);
        }

        // Apply custom color if provided
        if let Some(ref c) = color {
            apply_widget_color(&container, c);
        }

        // Store color for popover inheritance
        let color_rc = Rc::new(RefCell::new(color));

        // Create inner content box for consistent padding/margins via CSS
        // Spacing between children is controlled via CSS (see bar.rs .widget > .content)
        let content = GtkBox::new(Orientation::Horizontal, 0);
        content.add_css_class(class::CONTENT);
        // Fill the widget height so children can be properly centered within
        content.set_vexpand(true);
        content.set_valign(Align::Fill);
        // Disable baseline alignment - it can cause vertical offset issues with text
        content.set_baseline_position(gtk4::BaselinePosition::Center);
        container.append(&content);

        let menus: Rc<RefCell<HashMap<String, Rc<MenuHandle>>>> =
            Rc::new(RefCell::new(HashMap::new()));

        let gesture_click = GestureClick::new();
        {
            let menus_for_cb = menus.clone();
            gesture_click.connect_pressed(move |gesture, n_press, _x, _y| {
                debug!(
                    "BaseWidget click: n_press={}, button={}",
                    n_press,
                    gesture.current_button()
                );
                if n_press == 1 && gesture.current_button() == 1 {
                    if let Some((_name, menu)) = menus_for_cb.borrow().iter().next() {
                        debug!("Toggling first menu from BaseWidget click");
                        menu.toggle();
                    } else {
                        debug!("BaseWidget click: no menus registered");
                    }
                }
            });
        }

        container.add_controller(gesture_click.clone());

        Self {
            container,
            content,
            menus,
            color: color_rc,
            _gesture_click: gesture_click,
        }
    }

    /// Get the root GTK container for this widget.
    ///
    /// This is the outermost box with the `widget` CSS class.
    /// Most widgets should use `content()` to add children instead.
    pub fn widget(&self) -> &GtkBox {
        &self.container
    }

    /// Get the inner content box for adding widget children.
    ///
    /// This box has the `content` CSS class and receives consistent
    /// padding/margins via CSS rules like `.widget > .content`.
    /// Widgets should add their labels, icons, etc. to this box.
    pub fn content(&self) -> &GtkBox {
        &self.content
    }

    /// Create an icon using `IconsService`, apply CSS classes, pack it into the
    /// content box, and return the `IconHandle`.
    pub fn add_icon(&self, icon_name: &str, css_classes: &[&str]) -> IconHandle {
        let icons = IconsService::global();
        let handle = icons.create_icon(icon_name, css_classes);
        self.content.append(&handle.widget());
        handle
    }

    /// Create a label and append it to the content box.
    ///
    /// Creates a standard GTK label with CSS classes for styling.
    /// Font rendering is handled centrally by the Pango workaround system
    /// when `pango_font_rendering` is enabled in config.
    ///
    /// # Arguments
    /// * `text` - Initial label text (or None for empty)
    /// * `css_classes` - CSS classes to apply for styling (colors, etc.)
    ///
    /// # Example
    /// ```ignore
    /// use crate::styles::widget;
    /// let label = base.add_label(Some("100%"), &[widget::BATTERY_PERCENTAGE]);
    /// ```
    pub fn add_label(&self, text: Option<&str>, css_classes: &[&str]) -> Label {
        let label = Label::new(text);
        for class in css_classes {
            label.add_css_class(class);
        }
        self.content.append(&label);
        label
    }

    /// Set a styled tooltip on the root container using `TooltipManager`.
    pub fn set_tooltip(&self, text: &str) {
        let tooltip_manager = TooltipManager::global();
        tooltip_manager.set_styled_tooltip(&self.container, text);
    }

    /// Create a menu popover for this widget.
    pub fn create_menu<F>(&self, name: &str, builder: F) -> Rc<MenuHandle>
    where
        F: Fn() -> gtk4::Widget + 'static,
    {
        let popover = Popover::new();
        popover.set_parent(&self.container);
        configure_popover(&popover);

        let builder_rc: Rc<dyn Fn() -> gtk4::Widget> = Rc::new(builder);
        let handle = Rc::new(MenuHandle::new(
            popover,
            builder_rc,
            self.container.clone(),
            self.color.clone(),
        ));
        self.menus
            .borrow_mut()
            .insert(name.to_string(), handle.clone());
        handle
    }
}

/// Apply a custom background color to a widget via scoped CSS.
///
/// This is a standalone function that can be called from the WidgetFactory
/// after widget construction, without needing access to the BaseWidget internals.
///
/// The widget must have the `.widget` CSS class for the selector to match.
/// The global `widget_opacity` setting is applied to the color.
///
/// # Arguments
/// * `widget` - The GTK widget to style (should be a box with `.widget` class)
/// * `color` - A valid CSS color (hex like "#f5c2e7")
pub fn apply_widget_color(widget: &impl IsA<gtk4::Widget>, color: &str) {
    let opacity = ConfigManager::global().widget_opacity();

    // Apply opacity to the color using color-mix (same approach as ThemePalette)
    let bg_color = if opacity <= 0.0 {
        "transparent".to_string()
    } else if opacity >= 1.0 {
        color.to_string()
    } else {
        let opacity_percent = (opacity * 100.0).round() as u32;
        format!(
            "color-mix(in srgb, {} {}%, transparent)",
            color, opacity_percent
        )
    };

    let css = format!(
        r#"
        box.widget {{
            background-color: {};
        }}
        "#,
        bg_color
    );

    let provider = gtk4::CssProvider::new();
    provider.load_from_string(&css);

    // Apply scoped CSS to this widget's style context.
    // NOTE: style_context() and add_provider() are deprecated in GTK 4.10+
    // but GTK provides no replacement for widget-scoped CSS. See surfaces.rs
    // for detailed explanation of why we use this pattern.
    #[allow(deprecated)]
    widget
        .as_ref()
        .style_context()
        .add_provider(&provider, gtk4::STYLE_PROVIDER_PRIORITY_USER + 5);
}
