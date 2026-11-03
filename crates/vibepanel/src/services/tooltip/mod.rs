//! Tooltips for vibepanel widgets.
//!
//! Use [`set_tooltip`] for every tooltip. GTK's `set_tooltip_text` and
//! `set_tooltip_markup` are disallowed by clippy so call sites can't pick the
//! wrong implementation.
//!
//! The implementation depends on where the widget is when it's hovered:
//!
//! - **On the bar**, the tooltip is our own layer-shell surface placed next to
//!   the bar (see `window.rs` and `placement.rs`). Each bar window calls
//!   [`dismiss_on_bar_input`] so clicks hide it.
//! - **Everywhere else** (popovers, Quick Settings, standalone windows), GTK's
//!   native tooltip is shown.
//!
//! The custom surface exists for blur. Native tooltips are popups of the bar
//! surface, and Mango blurs layer surfaces but not popups
//! (<https://github.com/mangowm/mango/issues/320>). If that changes,
//! `uses_custom_tooltip` can return `false` and the custom path can go.

mod placement;
mod window;

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk4::glib::{self, SourceId};
use gtk4::prelude::*;
use gtk4::{EventControllerLegacy, EventControllerMotion, PropagationPhase, Widget, gdk, graphene};

use crate::services::config_manager::ConfigManager;
use crate::styles::class;
use crate::widgets::layer_shell_popover::{
    calculate_bar_exclusive_zone, calculate_bar_reserved_zone,
};
use window::TooltipWindow;

const SHOW_DELAY: Duration = Duration::from_millis(500);

/// Marks widgets that already have the tooltip controllers attached.
const HOOKED_KEY: &str = "vibepanel-tooltip-hooked";

thread_local! {
    static MANAGER: Rc<TooltipManager> = Rc::new(TooltipManager::default());
}

fn manager() -> Rc<TooltipManager> {
    MANAGER.with(Rc::clone)
}

/// Set a widget's tooltip; `None` or an empty string clears it. A visible
/// tooltip updates in place.
#[expect(clippy::disallowed_methods)]
pub fn set_tooltip(widget: &impl IsA<Widget>, text: Option<&str>) {
    let widget = widget.as_ref();
    // GTK's tooltip text doubles as storage for the custom tooltip.
    widget.set_tooltip_text(text);
    install_hooks(widget);
    manager().text_changed(widget);
}

/// Hide the custom tooltip, e.g. when a popover opens.
pub fn hide_tooltip() {
    manager().hide();
}

/// Dismiss the custom tooltip when anything on the bar is clicked or touched.
///
/// This sits on the bar window rather than on each widget: capture runs from
/// the window down, so widget handlers that stop the event can't skip it.
/// Scrolling doesn't dismiss, so tooltips like the volume one can update live.
pub fn dismiss_on_bar_input(bar_window: &impl IsA<Widget>) {
    let input = EventControllerLegacy::new();
    input.set_propagation_phase(PropagationPhase::Capture);
    input.connect_event(|_, event| {
        if matches!(
            event.event_type(),
            gdk::EventType::ButtonPress | gdk::EventType::TouchBegin
        ) {
            manager().hide();
        }
        glib::Propagation::Proceed
    });
    bar_window.add_controller(input);
}

/// Widgets inside a GTK popover on the bar have the bar as root, so check the
/// widget's own surface instead.
fn uses_custom_tooltip(widget: &Widget) -> bool {
    widget
        .native()
        .is_some_and(|native| native.has_css_class(class::BAR_WINDOW))
}

fn install_hooks(widget: &Widget) {
    // SAFETY: HOOKED_KEY is only ever stored and read as `bool`.
    unsafe {
        if widget.data::<bool>(HOOKED_KEY).is_some() {
            return;
        }
        widget.set_data(HOOKED_KEY, true);
    }

    // On the bar, suppress GTK's native tooltip. Returning false alone isn't
    // enough: GTK's default handler would still run and show the text.
    widget.connect_query_tooltip(|widget, _, _, _, _| {
        if uses_custom_tooltip(widget) {
            widget.stop_signal_emission_by_name("query-tooltip");
        }
        false
    });

    let motion = EventControllerMotion::new();
    motion.connect_enter(|controller, x, y| {
        if let Some(widget) = controller.widget() {
            manager().entered(&widget, (x, y));
        }
    });
    motion.connect_motion(|controller, x, y| {
        if let Some(widget) = controller.widget() {
            manager().moved(&widget, (x, y));
        }
    });
    motion.connect_leave(|controller| {
        if let Some(widget) = controller.widget() {
            manager().left(&widget);
        }
    });
    widget.add_controller(motion);
}

/// State of the custom bar tooltip.
#[derive(Default)]
struct TooltipManager {
    window: OnceCell<TooltipWindow>,
    pending_show: RefCell<Option<SourceId>>,
    /// The hovered widget whose tooltip is shown or about to be.
    current: RefCell<Option<glib::WeakRef<Widget>>>,
    /// Pointer position relative to `current`.
    pointer: Cell<(f64, f64)>,
}

impl TooltipManager {
    fn current(&self) -> Option<Widget> {
        self.current
            .borrow()
            .as_ref()
            .and_then(|weak| weak.upgrade())
    }

    fn is_current(&self, widget: &Widget) -> bool {
        self.current().as_ref() == Some(widget)
    }

    fn text_changed(&self, widget: &Widget) {
        if !self.is_current(widget) {
            return;
        }
        match widget.tooltip_text() {
            Some(text) => {
                if let Some(window) = self.window.get() {
                    window.set_text(&text);
                }
            }
            None => self.hide(),
        }
    }

    fn entered(&self, widget: &Widget, pointer: (f64, f64)) {
        if uses_custom_tooltip(widget) && widget.tooltip_text().is_some() {
            self.pointer.set(pointer);
            self.schedule_show(widget);
        }
    }

    fn moved(&self, widget: &Widget, pointer: (f64, f64)) {
        // Ancestors with tooltips see the same motion in their own
        // coordinates, so only track the current widget.
        if self.is_current(widget) {
            self.pointer.set(pointer);
        }
    }

    fn left(&self, widget: &Widget) {
        self.hide();

        // A still-hovered ancestor with its own tooltip (a workspace around a
        // window icon) gets no new enter event, so hand the tooltip back to it.
        let Some(ancestor) = std::iter::successors(widget.parent(), Widget::parent)
            .find(|ancestor| ancestor.tooltip_text().is_some())
        else {
            return;
        };
        if !uses_custom_tooltip(&ancestor)
            || !ancestor.state_flags().contains(gtk4::StateFlags::PRELIGHT)
        {
            return;
        }
        // The last pointer position is relative to the widget that was left.
        let (x, y) = self.pointer.get();
        if let Some(point) =
            widget.compute_point(&ancestor, &graphene::Point::new(x as f32, y as f32))
        {
            self.pointer.set((point.x().into(), point.y().into()));
        }
        self.schedule_show(&ancestor);
    }

    fn schedule_show(&self, widget: &Widget) {
        self.hide();
        *self.current.borrow_mut() = Some(widget.downgrade());
        let source = glib::timeout_add_local_once(SHOW_DELAY, || manager().show());
        *self.pending_show.borrow_mut() = Some(source);
    }

    fn show(&self) {
        self.pending_show.take();

        let Some(widget) = self.current() else {
            return;
        };
        let Some(text) = widget.tooltip_text() else {
            return;
        };
        if !widget.is_mapped() || !uses_custom_tooltip(&widget) {
            return;
        }
        let Some(bar) = widget.native().and_downcast::<gtk4::Window>() else {
            return;
        };
        let Some(monitor) = bar
            .surface()
            .and_then(|surface| surface.display().monitor_at_surface(&surface))
        else {
            return;
        };
        // The bar spans its edge and placement only uses the position along
        // it, so bar coordinates stand in for monitor coordinates.
        let (x, y) = self.pointer.get();
        let Some(pointer) = widget.compute_point(&bar, &graphene::Point::new(x as f32, y as f32))
        else {
            return;
        };
        let geometry = monitor.geometry();

        let window = self.window.get_or_init(TooltipWindow::new);
        window.set_text(&text);
        let position = ConfigManager::global().bar_position();
        // Bars that reserve no space need their thickness added explicitly.
        let bar_offset = calculate_bar_exclusive_zone() - calculate_bar_reserved_zone();
        let placement = placement::place(
            position,
            (geometry.width(), geometry.height()),
            (pointer.x() as i32, pointer.y() as i32),
            window.size(),
            bar_offset,
        );
        window.show(&monitor, position, placement);
    }

    fn cancel_pending(&self) {
        if let Some(source) = self.pending_show.take() {
            source.remove();
        }
    }

    fn hide(&self) {
        self.cancel_pending();
        if let Some(window) = self.window.get() {
            window.hide();
        }
        self.current.take();
    }
}
