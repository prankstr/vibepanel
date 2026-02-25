//! TooltipManager - process-wide tooltip handling for the vibepanel bar.
//!
//! Uses layer-shell positioned tooltip windows instead of GTK's native tooltips,
//! which don't position correctly on layer-shell surfaces.
//!
//! Tooltip styling is derived from `ThemePalette::surface_styles()` for full
//! theme integration. Initialize with `TooltipManager::init_global(styles)`
//! before first use.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use gtk4::glib::{self, SourceId};
use gtk4::prelude::*;
use gtk4::{Label, Window};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use tracing::debug;
use vibepanel_core::SurfaceStyles;

use crate::services::config_manager::ConfigManager;
use crate::services::surfaces::SurfaceStyleManager;
use crate::styles::tooltip;

// Thread-local singleton storage for TooltipManager
thread_local! {
    static TOOLTIP_INSTANCE: RefCell<Option<Rc<TooltipManager>>> = const { RefCell::new(None) };
}

/// Delay before showing tooltip (ms)
const TOOLTIP_SHOW_DELAY_MS: u32 = 500;

/// Offset from cursor position
const TOOLTIP_CURSOR_OFFSET_X: i32 = 10;
const TOOLTIP_CURSOR_OFFSET_Y: i32 = 0;

/// Margin from screen edges
const SCREEN_EDGE_MARGIN: i32 = 8;

/// Fallback tooltip width when measurement fails
const FALLBACK_TOOLTIP_WIDTH: i32 = 300;

/// Default tooltip styles, used when init_global is not called.
/// Provides a reasonable dark-mode appearance as fallback.
fn default_surface_styles() -> SurfaceStyles {
    SurfaceStyles {
        background_color: "#111217".to_string(),
        text_color: "#ffffff".to_string(),
        font_family: "monospace".to_string(),
        font_size: 14,
        border_radius: 8,
        border_color: "rgba(255, 255, 255, 0.10)".to_string(),
        opacity: 1.0,
        shadow: "0 1px 2px rgba(0, 0, 0, 0.24)".to_string(),
        is_dark_mode: true,
    }
}

/// A layer-shell tooltip window.
struct TooltipWindow {
    window: Window,
    label: Label,
}

/// Positioning mode for tooltips.
#[derive(Clone, Copy)]
enum TooltipAnchor {
    /// Anchor from bar-edge + left, use left margin for X position
    Left,
    /// Anchor from bar-edge + right, use right margin for X position
    Right,
}

impl TooltipWindow {
    fn new(styles: &SurfaceStyles) -> Self {
        let window = Window::builder().decorated(false).resizable(false).build();

        window.add_css_class(tooltip::WINDOW);

        // Initialize layer-shell
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_exclusive_zone(0);
        window.set_keyboard_mode(KeyboardMode::None);

        // Anchor to bar-adjacent edge - will be adjusted horizontally in show_at
        let is_bottom = ConfigManager::global().bar_is_bottom();
        window.set_anchor(Edge::Top, !is_bottom);
        window.set_anchor(Edge::Left, true);
        window.set_anchor(Edge::Right, false);
        window.set_anchor(Edge::Bottom, is_bottom);

        // Create label
        let label = Label::new(None);
        label.add_css_class(tooltip::LABEL);
        window.set_child(Some(&label));

        // Apply styles via inline CSS on the window
        Self::apply_styles(&window, &label, styles);

        Self { window, label }
    }

    fn apply_styles(window: &Window, label: &Label, styles: &SurfaceStyles) {
        // Apply slight transparency to tooltip background
        let bg = format!(
            "color-mix(in srgb, {} 80%, transparent)",
            styles.background_color
        );

        // Use CSS for font styling (native GTK behavior)
        // Use var(--radius-surface) for border-radius to respect theme settings including 0
        let css = format!(
            r#"
.vibepanel-tooltip {{
    background-color: {bg};
    border-radius: var(--radius-surface);
    border: none;
    padding: 6px 10px;
}}

.vibepanel-tooltip-label {{
    font-family: {font};
    font-size: {size}px;
    color: {fg};
}}
"#,
            bg = bg,
            font = styles.font_family,
            size = styles.font_size,
            fg = styles.text_color,
        );

        let provider = gtk4::CssProvider::new();
        provider.load_from_string(&css);

        // Apply to window and label via display-wide provider with USER priority to override GTK themes
        let display = gtk4::prelude::WidgetExt::display(window);
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_USER + 10,
        );

        // Apply Pango font attributes if enabled (workaround for layer-shell font issues)
        SurfaceStyleManager::global().apply_pango_attrs(label);
    }

    /// Measure the natural width of the tooltip with the given text.
    /// This sets the text and returns the preferred width.
    fn measure_width(&self, text: &str) -> i32 {
        self.label.set_text(text);

        // Get the natural width of the label
        let (_, natural_width, _, _) = self.label.measure(gtk4::Orientation::Horizontal, -1);

        // Add padding (6px on each side from CSS: padding: 6px 10px)
        // Actually it's 10px horizontal padding on each side
        natural_width + 20
    }

    fn show_at(&self, x: i32, y: i32, anchor: TooltipAnchor, monitor: Option<&gtk4::gdk::Monitor>) {
        // Bind to monitor if provided
        if let Some(monitor) = monitor {
            self.window.set_monitor(Some(monitor));
        }

        // Determine vertical edge based on bar position
        let is_bottom = ConfigManager::global().bar_is_bottom();
        let bar_edge = if is_bottom { Edge::Bottom } else { Edge::Top };
        let opposite_edge = if is_bottom { Edge::Top } else { Edge::Bottom };

        // Set vertical anchors
        self.window.set_anchor(bar_edge, true);
        self.window.set_anchor(opposite_edge, false);

        // Set anchors based on horizontal positioning mode
        match anchor {
            TooltipAnchor::Left => {
                self.window.set_anchor(Edge::Left, true);
                self.window.set_anchor(Edge::Right, false);
                self.window.set_margin(Edge::Left, x);
                self.window.set_margin(Edge::Right, 0);
            }
            TooltipAnchor::Right => {
                self.window.set_anchor(Edge::Left, false);
                self.window.set_anchor(Edge::Right, true);
                self.window.set_margin(Edge::Left, 0);
                self.window.set_margin(Edge::Right, x);
            }
        }

        self.window.set_margin(bar_edge, y);
        self.window.present();
    }

    fn hide(&self) {
        self.window.set_visible(false);
    }

    fn update_styles(&self, styles: &SurfaceStyles) {
        Self::apply_styles(&self.window, &self.label, styles);
    }
}

/// Process-wide tooltip manager using layer-shell windows.
///
/// Provides `set_styled_tooltip` for applying tooltips to widgets.
/// Unlike GTK's native tooltips, these are positioned correctly on layer-shell surfaces.
pub struct TooltipManager {
    /// Surface styling configuration.
    styles: RefCell<SurfaceStyles>,
    /// The tooltip window (lazily created).
    tooltip_window: RefCell<Option<TooltipWindow>>,
    /// Pending show timer source ID.
    pending_show: RefCell<Option<SourceId>>,
    /// Currently hovered widget (weak ref to avoid preventing cleanup).
    current_widget: RefCell<Option<glib::WeakRef<gtk4::Widget>>>,
    /// Current tooltip text.
    current_text: RefCell<String>,
    /// Map of widget pointer addresses to tooltip text.
    tooltip_texts: RefCell<HashMap<usize, String>>,
    /// Set of widget addresses that have controllers attached.
    setup_widgets: RefCell<std::collections::HashSet<usize>>,
    /// Last known cursor X position (relative to widget).
    cursor_x: Cell<f64>,
}

impl TooltipManager {
    /// Create a new TooltipManager with the given styles.
    fn new(styles: SurfaceStyles) -> Rc<Self> {
        Rc::new(Self {
            styles: RefCell::new(styles),
            tooltip_window: RefCell::new(None),
            pending_show: RefCell::new(None),
            current_widget: RefCell::new(None),
            current_text: RefCell::new(String::new()),
            tooltip_texts: RefCell::new(HashMap::new()),
            setup_widgets: RefCell::new(std::collections::HashSet::new()),
            cursor_x: Cell::new(0.0),
        })
    }

    /// Initialize the global TooltipManager with styles from ThemePalette.
    ///
    /// Should be called during application startup after loading config:
    /// ```ignore
    /// let palette = ThemePalette::from_config(&config);
    /// TooltipManager::init_global(palette.surface_styles());
    /// ```
    pub fn init_global(styles: SurfaceStyles) {
        TOOLTIP_INSTANCE.with(|cell| {
            let mut opt = cell.borrow_mut();
            if opt.is_some() {
                debug!("TooltipManager already initialized, ignoring init_global call");
                return;
            }
            *opt = Some(TooltipManager::new(styles));
        });
    }

    /// Get the global TooltipManager singleton.
    ///
    /// If not initialized via `init_global`, uses default dark-mode styles.
    pub fn global() -> Rc<Self> {
        TOOLTIP_INSTANCE.with(|cell| {
            let mut opt = cell.borrow_mut();
            if opt.is_none() {
                debug!("TooltipManager not initialized, using defaults");
                *opt = Some(TooltipManager::new(default_surface_styles()));
            }
            opt.as_ref().unwrap().clone()
        })
    }

    /// Reconfigure the manager with new styles (for live config reload).
    ///
    /// This updates the internal styles and updates any existing tooltip window.
    pub fn reconfigure(&self, styles: SurfaceStyles) {
        debug!(
            "TooltipManager reconfiguring: bg={} -> {}",
            self.styles.borrow().background_color,
            styles.background_color
        );
        *self.styles.borrow_mut() = styles.clone();

        // Update existing tooltip window if it exists
        if let Some(ref tooltip_window) = *self.tooltip_window.borrow() {
            tooltip_window.update_styles(&styles);
        }
    }

    /// Set a styled tooltip on a widget.
    ///
    /// This sets up hover handlers on the widget to show/hide our custom tooltip.
    /// The tooltip will appear after a short delay when hovering.
    pub fn set_styled_tooltip(&self, widget: &impl IsA<gtk4::Widget>, text: &str) {
        let widget = widget.as_ref();

        // Use widget pointer as key
        let widget_addr = widget.as_ptr() as usize;

        // Store/update the tooltip text
        self.tooltip_texts
            .borrow_mut()
            .insert(widget_addr, text.to_string());

        // If the tooltip is currently visible for this widget, update it live
        if let Some(ref current_weak) = *self.current_widget.borrow()
            && let Some(current) = current_weak.upgrade()
            && current.as_ptr() as usize == widget_addr
        {
            *self.current_text.borrow_mut() = text.to_string();
            if let Some(ref tw) = *self.tooltip_window.borrow() {
                tw.label.set_text(text);
            }
        }

        // Only set up controllers once per widget
        if self.setup_widgets.borrow().contains(&widget_addr) {
            return;
        }
        self.setup_widgets.borrow_mut().insert(widget_addr);

        // Clean up when widget is destroyed to prevent stale entries
        // (memory addresses can be reused for new widgets)
        let manager = Self::global();
        let addr = widget_addr;
        widget.connect_destroy(move |_| {
            manager.setup_widgets.borrow_mut().remove(&addr);
            manager.tooltip_texts.borrow_mut().remove(&addr);
        });

        // Create motion controller for enter/leave/motion
        let motion = gtk4::EventControllerMotion::new();

        // On enter: start timer to show tooltip
        let manager = Self::global();
        let addr = widget_addr;
        motion.connect_enter(move |controller, x, _y| {
            let Some(widget) = controller.widget() else {
                return;
            };
            // Store cursor X position relative to widget
            manager.cursor_x.set(x);
            if let Some(text) = manager.tooltip_texts.borrow().get(&addr) {
                let text = text.clone();
                manager.schedule_show(&widget, &text);
            }
        });

        // Track motion to update cursor X position
        let manager = Self::global();
        motion.connect_motion(move |_controller, x, _y| {
            manager.cursor_x.set(x);
        });

        // On leave: cancel timer and hide tooltip
        let manager = Self::global();
        motion.connect_leave(move |_controller| {
            manager.cancel_and_hide();
        });

        widget.add_controller(motion);
    }

    /// Schedule showing a tooltip after the delay.
    fn schedule_show(&self, widget: &gtk4::Widget, text: &str) {
        // Cancel any pending show
        self.cancel_pending();

        // Store current widget and text
        let weak_ref = glib::WeakRef::new();
        weak_ref.set(Some(widget));
        *self.current_widget.borrow_mut() = Some(weak_ref);
        *self.current_text.borrow_mut() = text.to_string();

        // Schedule the show
        let manager = Self::global();
        let source_id = glib::timeout_add_local_once(
            std::time::Duration::from_millis(TOOLTIP_SHOW_DELAY_MS as u64),
            move || {
                manager.do_show();
            },
        );
        *self.pending_show.borrow_mut() = Some(source_id);
    }

    /// Actually show the tooltip.
    fn do_show(&self) {
        *self.pending_show.borrow_mut() = None;

        let text = self.current_text.borrow().clone();
        if text.is_empty() {
            return;
        }

        let widget = match self
            .current_widget
            .borrow()
            .as_ref()
            .and_then(|w| w.upgrade())
        {
            Some(w) => w,
            None => return,
        };

        // Don't show tooltip for hidden widgets
        if !widget.is_visible() {
            return;
        }

        // Get monitor info
        let (monitor_width, monitor) = match self.get_monitor_info(&widget) {
            Some(info) => info,
            None => return,
        };

        // cursor_x is relative to the widget's top-left corner
        let cursor_rel_x = self.cursor_x.get() as i32;

        // Get cursor's screen X position, accounting for window anchor type
        let cursor_screen_x = self
            .get_cursor_screen_x(&widget, cursor_rel_x, monitor_width)
            .unwrap_or(cursor_rel_x);

        // For Y position: layer-shell exclusive zone means the tooltip's bar-edge anchor
        // starts past the bar's exclusive zone, so we only need a small offset
        let tooltip_y = TOOLTIP_CURSOR_OFFSET_Y;

        // Ensure tooltip window exists
        self.ensure_tooltip_window();

        if let Some(ref tooltip_window) = *self.tooltip_window.borrow() {
            // Measure actual tooltip width with the text
            let tooltip_width = tooltip_window.measure_width(&text);
            let effective_width = if tooltip_width > 0 {
                tooltip_width
            } else {
                FALLBACK_TOOLTIP_WIDTH
            };

            // Position tooltip near bar and near cursor X
            let tooltip_x = cursor_screen_x + TOOLTIP_CURSOR_OFFSET_X;

            // Check right edge overflow using actual measured width
            let (anchor, x_margin) =
                if tooltip_x + effective_width > monitor_width - SCREEN_EDGE_MARGIN {
                    // Anchor from right edge
                    let right_margin = (monitor_width - cursor_screen_x + TOOLTIP_CURSOR_OFFSET_X)
                        .max(SCREEN_EDGE_MARGIN);
                    (TooltipAnchor::Right, right_margin)
                } else {
                    (TooltipAnchor::Left, tooltip_x.max(SCREEN_EDGE_MARGIN))
                };

            tooltip_window.show_at(x_margin, tooltip_y, anchor, monitor.as_ref());
        }
    }

    /// Get widget's X coordinate within its root window.
    fn get_widget_window_x(&self, widget: &gtk4::Widget) -> Option<i32> {
        let root = widget.root()?;
        let root_widget = root.clone().upcast::<gtk4::Widget>();
        let point = gtk4::graphene::Point::new(0.0, 0.0);
        let computed = widget.compute_point(&root_widget, &point)?;
        Some(computed.x() as i32)
    }

    /// Get cursor's screen X, accounting for right-anchored layer-shell windows (popovers).
    fn get_cursor_screen_x(
        &self,
        widget: &gtk4::Widget,
        cursor_rel_x: i32,
        monitor_width: i32,
    ) -> Option<i32> {
        let root = widget.root()?;
        let window = root.downcast_ref::<gtk4::Window>()?;
        let widget_in_window_x = self.get_widget_window_x(widget)?;

        // Right-anchored (but not left) windows need adjusted calculation
        if window.is_layer_window()
            && window.is_anchor(Edge::Right)
            && !window.is_anchor(Edge::Left)
        {
            let right_margin = window.margin(Edge::Right);
            let window_width = window.width();
            let window_left_x = monitor_width - right_margin - window_width;
            return Some(window_left_x + widget_in_window_x + cursor_rel_x);
        }

        Some(widget_in_window_x + cursor_rel_x)
    }

    /// Get monitor info for the widget's window.
    fn get_monitor_info(&self, widget: &gtk4::Widget) -> Option<(i32, Option<gtk4::gdk::Monitor>)> {
        let root = widget.root()?;
        let surface = root.downcast_ref::<gtk4::Window>()?.surface()?;

        let display = gtk4::gdk::Display::default()?;
        let monitor = display.monitor_at_surface(&surface);

        let width = monitor
            .as_ref()
            .map(|m| m.geometry().width())
            .unwrap_or(1920);

        Some((width, monitor))
    }

    /// Cancel pending show timer.
    fn cancel_pending(&self) {
        if let Some(source_id) = self.pending_show.borrow_mut().take() {
            source_id.remove();
        }
    }

    /// Cancel pending timer and hide tooltip.
    ///
    /// Use this to programmatically dismiss any visible tooltip,
    /// e.g., when closing a popover or transitioning to a pop-out window.
    pub fn cancel_and_hide(&self) {
        self.cancel_pending();
        self.hide_tooltip();
    }

    /// Hide the tooltip window.
    fn hide_tooltip(&self) {
        if let Some(ref tooltip_window) = *self.tooltip_window.borrow() {
            tooltip_window.hide();
        }
        *self.current_widget.borrow_mut() = None;
        *self.current_text.borrow_mut() = String::new();
    }

    /// Ensure the tooltip window is created.
    fn ensure_tooltip_window(&self) {
        if self.tooltip_window.borrow().is_some() {
            return;
        }

        let styles = self.styles.borrow();
        let tooltip_window = TooltipWindow::new(&styles);
        *self.tooltip_window.borrow_mut() = Some(tooltip_window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_surface_styles() {
        // Verify default styles are sensible
        let styles = default_surface_styles();
        assert_eq!(styles.background_color, "#111217");
        assert_eq!(styles.text_color, "#ffffff");
        assert!(styles.border_radius > 0);
        assert!(styles.font_size > 0);
        assert!(styles.is_dark_mode);
    }
}
