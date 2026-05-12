//! TooltipManager - process-wide tooltip handling for the vibepanel bar.
//!
//! Uses layer-shell positioned tooltip windows instead of GTK's native tooltips,
//! which don't position correctly on layer-shell surfaces.
//!
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use gtk4::glib::{self, SourceId};
use gtk4::prelude::*;
use gtk4::{Label, Window};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use tracing::debug;
use vibepanel_core::config::BarPosition;

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

/// Side-bar tooltips already start past the bar's exclusive zone; keep the
/// inward gap tighter than cursor-following horizontal tooltips.
const TOOLTIP_SIDE_BAR_OFFSET_X: i32 = 4;

/// Margin from screen edges
const SCREEN_EDGE_MARGIN: i32 = 8;

/// Fallback tooltip width when measurement fails
const FALLBACK_TOOLTIP_WIDTH: i32 = 300;

/// Fallback tooltip height when measurement fails
const FALLBACK_TOOLTIP_HEIGHT: i32 = 32;
/// A layer-shell tooltip window.
struct TooltipWindow {
    window: Window,
    label: Label,
}

/// Horizontal positioning mode for top/bottom tooltips.
#[derive(Clone, Copy)]
enum TooltipHorizontalAnchor {
    /// Anchor from bar-edge + left, use left margin for X position
    Left,
    /// Anchor from bar-edge + right, use right margin for X position
    Right,
}

/// Vertical positioning mode for left/right tooltips.
#[derive(Clone, Copy)]
enum TooltipVerticalAnchor {
    /// Anchor from top edge, use top margin for Y position
    Top,
    /// Anchor from bottom edge, use bottom margin for Y position
    Bottom,
}

fn clamp_margin(anchor: i32, monitor_size: i32, window_size: i32, min_edge_margin: i32) -> i32 {
    let margin = anchor - window_size / 2;
    let max_margin = monitor_size.saturating_sub(window_size + min_edge_margin);

    if max_margin >= min_edge_margin {
        margin.clamp(min_edge_margin, max_margin)
    } else {
        min_edge_margin.max(max_margin)
    }
}

fn clamp_far_margin(anchor: i32, monitor_size: i32, window_size: i32, min_edge_margin: i32) -> i32 {
    let margin = monitor_size - anchor - window_size / 2;
    let max_margin = monitor_size.saturating_sub(window_size + min_edge_margin);

    if max_margin >= min_edge_margin {
        margin.clamp(min_edge_margin, max_margin)
    } else {
        min_edge_margin.max(max_margin)
    }
}

impl TooltipWindow {
    fn new() -> Self {
        let window = Window::builder().decorated(false).resizable(false).build();

        window.add_css_class(tooltip::WINDOW);

        // Initialize layer-shell
        window.init_layer_shell();
        window.set_namespace(Some("vibepanel-tooltip"));
        window.set_layer(Layer::Overlay);
        window.set_exclusive_zone(0);
        window.set_keyboard_mode(KeyboardMode::None);

        Self::reset_margins(&window);

        // Create label
        let label = Label::new(None);
        label.add_css_class(tooltip::LABEL);
        window.set_child(Some(&label));

        SurfaceStyleManager::global().apply_pango_attrs(&label);

        Self { window, label }
    }

    /// Measure the natural size of the tooltip with the given text.
    /// This sets the text and returns the preferred size including CSS padding.
    fn measure_size(&self, text: &str) -> (i32, i32) {
        self.label.set_text(text);

        // Get the natural size of the label
        let (_, natural_width, _, _) = self.label.measure(gtk4::Orientation::Horizontal, -1);
        let (_, natural_height, _, _) = self.label.measure(gtk4::Orientation::Vertical, -1);

        // CSS padding is 6px vertical and 10px horizontal on each side.
        (natural_width + 20, natural_height + 12)
    }

    fn reset_margins(window: &Window) {
        for edge in [Edge::Top, Edge::Right, Edge::Bottom, Edge::Left] {
            window.set_margin(edge, 0);
        }
    }

    fn show_horizontal(
        &self,
        x: i32,
        y: i32,
        anchor: TooltipHorizontalAnchor,
        monitor: Option<&gtk4::gdk::Monitor>,
    ) {
        // Bind to monitor if provided
        if let Some(monitor) = monitor {
            self.window.set_monitor(Some(monitor));
        }

        Self::reset_margins(&self.window);

        // Determine vertical edge based on bar position
        let bar_edge = match ConfigManager::global().bar_position() {
            BarPosition::Bottom => Edge::Bottom,
            _ => Edge::Top,
        };
        let opposite_edge = match bar_edge {
            Edge::Top => Edge::Bottom,
            _ => Edge::Top,
        };

        // Set vertical anchors
        self.window.set_anchor(bar_edge, true);
        self.window.set_anchor(opposite_edge, false);

        // Set anchors based on horizontal positioning mode
        match anchor {
            TooltipHorizontalAnchor::Left => {
                self.window.set_anchor(Edge::Left, true);
                self.window.set_anchor(Edge::Right, false);
                self.window.set_margin(Edge::Left, x);
            }
            TooltipHorizontalAnchor::Right => {
                self.window.set_anchor(Edge::Left, false);
                self.window.set_anchor(Edge::Right, true);
                self.window.set_margin(Edge::Right, x);
            }
        }

        self.window.set_margin(bar_edge, y);
        self.window.present();
    }

    fn show_vertical(
        &self,
        x: i32,
        y: i32,
        anchor: TooltipVerticalAnchor,
        monitor: Option<&gtk4::gdk::Monitor>,
    ) {
        if let Some(monitor) = monitor {
            self.window.set_monitor(Some(monitor));
        }

        Self::reset_margins(&self.window);

        let bar_edge = match ConfigManager::global().bar_position() {
            BarPosition::Right => Edge::Right,
            _ => Edge::Left,
        };
        let opposite_edge = match bar_edge {
            Edge::Left => Edge::Right,
            _ => Edge::Left,
        };

        self.window.set_anchor(bar_edge, true);
        self.window.set_anchor(opposite_edge, false);
        self.window.set_margin(bar_edge, x);

        match anchor {
            TooltipVerticalAnchor::Top => {
                self.window.set_anchor(Edge::Top, true);
                self.window.set_anchor(Edge::Bottom, false);
                self.window.set_margin(Edge::Top, y);
            }
            TooltipVerticalAnchor::Bottom => {
                self.window.set_anchor(Edge::Top, false);
                self.window.set_anchor(Edge::Bottom, true);
                self.window.set_margin(Edge::Bottom, y);
            }
        }

        self.window.present();
    }

    fn hide(&self) {
        self.window.set_visible(false);
    }
}

/// Process-wide tooltip manager using layer-shell windows.
///
/// Provides `set_styled_tooltip` for applying tooltips to widgets.
/// Unlike GTK's native tooltips, these are positioned correctly on layer-shell surfaces.
pub struct TooltipManager {
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
    /// Last known cursor Y position (relative to widget).
    cursor_y: Cell<f64>,
}

impl TooltipManager {
    /// Create a new TooltipManager.
    fn new() -> Rc<Self> {
        Rc::new(Self {
            tooltip_window: RefCell::new(None),
            pending_show: RefCell::new(None),
            current_widget: RefCell::new(None),
            current_text: RefCell::new(String::new()),
            tooltip_texts: RefCell::new(HashMap::new()),
            setup_widgets: RefCell::new(std::collections::HashSet::new()),
            cursor_x: Cell::new(0.0),
            cursor_y: Cell::new(0.0),
        })
    }

    /// Initialize the global TooltipManager.
    pub fn init_global() {
        TOOLTIP_INSTANCE.with(|cell| {
            let mut opt = cell.borrow_mut();
            if opt.is_some() {
                debug!("TooltipManager already initialized, ignoring init_global call");
                return;
            }
            *opt = Some(TooltipManager::new());
        });
    }

    /// Get the global TooltipManager singleton.
    ///
    /// If not initialized via `init_global`, initializes on demand.
    pub fn global() -> Rc<Self> {
        TOOLTIP_INSTANCE.with(|cell| {
            let mut opt = cell.borrow_mut();
            if opt.is_none() {
                debug!("TooltipManager not initialized, using defaults");
                *opt = Some(TooltipManager::new());
            }
            opt.as_ref().unwrap().clone()
        })
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
        motion.connect_enter(move |controller, x, y| {
            let Some(widget) = controller.widget() else {
                return;
            };
            // Store cursor position relative to widget
            manager.cursor_x.set(x);
            manager.cursor_y.set(y);
            if let Some(text) = manager.tooltip_texts.borrow().get(&addr) {
                let text = text.clone();
                manager.schedule_show(&widget, &text);
            }
        });

        // Track motion to update cursor X position
        let manager = Self::global();
        motion.connect_motion(move |_controller, x, y| {
            manager.cursor_x.set(x);
            manager.cursor_y.set(y);
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
        let (monitor_width, monitor_height, monitor) = match self.get_monitor_info(&widget) {
            Some(info) => info,
            None => return,
        };

        // Cursor position is relative to the widget's top-left corner
        let cursor_rel_x = self.cursor_x.get() as i32;
        let cursor_rel_y = self.cursor_y.get() as i32;

        let (cursor_screen_x, cursor_screen_y) = self
            .get_cursor_screen_position(
                &widget,
                cursor_rel_x,
                cursor_rel_y,
                monitor_width,
                monitor_height,
            )
            .unwrap_or((cursor_rel_x, cursor_rel_y));

        // For Y position: layer-shell exclusive zone means the tooltip's bar-edge anchor
        // starts past the bar's exclusive zone, so we only need a small offset
        let tooltip_y = TOOLTIP_CURSOR_OFFSET_Y;

        // Ensure tooltip window exists
        self.ensure_tooltip_window();

        if let Some(ref tooltip_window) = *self.tooltip_window.borrow() {
            // Measure actual tooltip size with the text
            let (tooltip_width, tooltip_height) = tooltip_window.measure_size(&text);
            let effective_width = if tooltip_width > 0 {
                tooltip_width
            } else {
                FALLBACK_TOOLTIP_WIDTH
            };
            let effective_height = if tooltip_height > 0 {
                tooltip_height
            } else {
                FALLBACK_TOOLTIP_HEIGHT
            };

            if ConfigManager::global().bar_position().is_horizontal() {
                // Position tooltip near bar and near cursor X
                let tooltip_x = cursor_screen_x + TOOLTIP_CURSOR_OFFSET_X;

                let (anchor, x_margin) = if tooltip_x + effective_width
                    > monitor_width - SCREEN_EDGE_MARGIN
                {
                    let right_margin = (monitor_width - cursor_screen_x + TOOLTIP_CURSOR_OFFSET_X)
                        .max(SCREEN_EDGE_MARGIN);
                    (TooltipHorizontalAnchor::Right, right_margin)
                } else {
                    (
                        TooltipHorizontalAnchor::Left,
                        tooltip_x.max(SCREEN_EDGE_MARGIN),
                    )
                };

                tooltip_window.show_horizontal(x_margin, tooltip_y, anchor, monitor.as_ref());
            } else {
                let tooltip_x = TOOLTIP_SIDE_BAR_OFFSET_X;
                let tooltip_y = cursor_screen_y + TOOLTIP_CURSOR_OFFSET_Y;

                let (anchor, y_margin) =
                    if tooltip_y + effective_height > monitor_height - SCREEN_EDGE_MARGIN {
                        let bottom_margin = clamp_far_margin(
                            cursor_screen_y,
                            monitor_height,
                            effective_height,
                            SCREEN_EDGE_MARGIN,
                        );
                        (TooltipVerticalAnchor::Bottom, bottom_margin)
                    } else {
                        let top_margin = clamp_margin(
                            tooltip_y,
                            monitor_height,
                            effective_height,
                            SCREEN_EDGE_MARGIN,
                        );
                        (TooltipVerticalAnchor::Top, top_margin)
                    };

                tooltip_window.show_vertical(tooltip_x, y_margin, anchor, monitor.as_ref());
            }
        }
    }

    /// Get widget's coordinate within its root window.
    fn get_widget_window_position(&self, widget: &gtk4::Widget) -> Option<(i32, i32)> {
        let root = widget.root()?;
        let root_widget = root.clone().upcast::<gtk4::Widget>();
        let point = gtk4::graphene::Point::new(0.0, 0.0);
        let computed = widget.compute_point(&root_widget, &point)?;
        Some((computed.x() as i32, computed.y() as i32))
    }

    /// Get cursor's monitor-local position, accounting for edge-anchored layer-shell windows.
    fn get_cursor_screen_position(
        &self,
        widget: &gtk4::Widget,
        cursor_rel_x: i32,
        cursor_rel_y: i32,
        monitor_width: i32,
        monitor_height: i32,
    ) -> Option<(i32, i32)> {
        let root = widget.root()?;
        let window = root.downcast_ref::<gtk4::Window>()?;
        let (widget_in_window_x, widget_in_window_y) = self.get_widget_window_position(widget)?;

        let mut window_left_x = 0;
        let mut window_top_y = 0;

        // Far-edge anchored windows need adjusted calculation.
        if window.is_layer_window()
            && window.is_anchor(Edge::Right)
            && !window.is_anchor(Edge::Left)
        {
            let right_margin = window.margin(Edge::Right);
            let window_width = window.width();
            window_left_x = monitor_width - right_margin - window_width;
        }

        if window.is_layer_window()
            && window.is_anchor(Edge::Bottom)
            && !window.is_anchor(Edge::Top)
        {
            let bottom_margin = window.margin(Edge::Bottom);
            let window_height = window.height();
            window_top_y = monitor_height - bottom_margin - window_height;
        }

        Some((
            window_left_x + widget_in_window_x + cursor_rel_x,
            window_top_y + widget_in_window_y + cursor_rel_y,
        ))
    }

    /// Get monitor info for the widget's window.
    fn get_monitor_info(
        &self,
        widget: &gtk4::Widget,
    ) -> Option<(i32, i32, Option<gtk4::gdk::Monitor>)> {
        let root = widget.root()?;
        let surface = root.downcast_ref::<gtk4::Window>()?.surface()?;

        let display = gtk4::gdk::Display::default()?;
        let monitor = display.monitor_at_surface(&surface);

        let width = monitor
            .as_ref()
            .map(|m| m.geometry().width())
            .unwrap_or(1920);
        let height = monitor
            .as_ref()
            .map(|m| m.geometry().height())
            .unwrap_or(1080);

        Some((width, height, monitor))
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

        let tooltip_window = TooltipWindow::new();
        *self.tooltip_window.borrow_mut() = Some(tooltip_window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_margin_centers_when_space_allows() {
        assert_eq!(clamp_margin(400, 800, 100, 8), 350);
    }

    #[test]
    fn clamp_margin_clamps_to_near_edge() {
        assert_eq!(clamp_margin(10, 800, 100, 8), 8);
    }

    #[test]
    fn clamp_far_margin_centers_from_far_edge() {
        assert_eq!(clamp_far_margin(400, 800, 100, 8), 350);
    }
}
