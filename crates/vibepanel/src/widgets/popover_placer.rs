//! Sizes and places a popover's content inside its surface.
//!
//! The content never gets taller than its height cap (see
//! [`PopoverPlacer::set_height_cap`]), the screen space a popover may use, so
//! a scrolled window inside it scrolls the rest. Content that can't scroll
//! keeps its minimum height.
//!
//! On left/right bars the placer also positions the content. A shrink-wrapped
//! surface positioned by layer-shell margins can't follow content that
//! resizes: the margin and the new buffer size reach the compositor in
//! separate commits, so every change overshoots or snaps. There the surface
//! instead spans the screen height and never resizes, and this widget
//! positions the content during layout, so each frame is drawn where it
//! belongs. The span it places the content in, inside the edge gaps, is the
//! same height as the cap.
//!
//! The content opens centred on the anchor, clamped on-screen: its *home*.
//! Growth keeps the top at home until the bottom reaches the edge gap, then
//! keeps the bottom there and grows upward until the top reaches its gap.
//! Content taller than that is allocated less than its natural height, so a
//! scrolled window inside it scrolls. Shrinking runs the same steps in
//! reverse.
//!
//! The transparent rest of the surface is excluded from the input region, so
//! clicks there reach the click-catcher underneath.

use gtk4::graphene::Point;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{glib, gsk};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Vertical placement inputs, in surface-local coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VerticalPlacement {
    /// Centre of the bar button, or `None` to open at the bottom edge.
    pub anchor_y: Option<i32>,
    /// Gap between the visible content and the top and bottom screen edges.
    pub edge_gap: i32,
}

/// Home top within the available span, latched for the span it was made for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Home {
    top: i32,
    span: i32,
}

/// Extra insets that bring the gap above and below the placer, which is
/// `before` and `after` within the surface, up to `gap`.
fn edge_insets(gap: i32, before: i32, after: i32) -> (i32, i32) {
    ((gap - before).max(0), (gap - after).max(0))
}

/// Top of content of `height` centred on `anchor` within `span`, clamped
/// inside it. Without an anchor the content starts at the bottom.
fn home_top(anchor: Option<i32>, height: i32, span: i32) -> i32 {
    let max_top = (span - height).max(0);
    anchor.map_or(max_top, |anchor| (anchor - height / 2).clamp(0, max_top))
}

/// Top of content of `height` within `span`: at home while it fits below,
/// otherwise with its bottom at the end of the span, never above the start.
fn placed_top(home: i32, height: i32, span: i32) -> i32 {
    home.min(span - height).max(0)
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct PopoverPlacer {
        pub(super) child: glib::WeakRef<gtk4::Widget>,
        pub(super) vertical: Cell<Option<VerticalPlacement>>,
        /// Most height the content gets. Vertical placement keeps it within
        /// the span instead, which on a full-height surface is the same.
        pub(super) height_cap: Cell<Option<i32>>,
        pub(super) home: Cell<Option<Home>>,
        /// Content rect last applied to the input region, in surface coordinates.
        pub(super) input_rect: Cell<Option<(i32, i32, i32, i32)>>,
        /// Re-apply the input region even if the rect is unchanged.
        pub(super) input_stale: Cell<bool>,
        pub(super) on_placed: RefCell<Option<Rc<dyn Fn()>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PopoverPlacer {
        const NAME: &'static str = "VibepanelPopoverPlacer";
        type Type = super::PopoverPlacer;
        type ParentType = gtk4::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("popover-placer");
        }
    }

    impl ObjectImpl for PopoverPlacer {
        fn dispose(&self) {
            if let Some(child) = self.child.upgrade() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for PopoverPlacer {
        fn request_mode(&self) -> gtk4::SizeRequestMode {
            self.child
                .upgrade()
                .map_or(gtk4::SizeRequestMode::ConstantSize, |child| {
                    child.request_mode()
                })
        }

        fn measure(&self, orientation: gtk4::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let Some(child) = self.child.upgrade() else {
                return (0, 0, -1, -1);
            };
            if self.vertical.get().is_none() {
                let (min, nat, min_baseline, nat_baseline) = child.measure(orientation, for_size);
                return match self.height_cap.get() {
                    // Only what can scroll gives way; the minimum stays.
                    Some(cap) if orientation == gtk4::Orientation::Vertical => {
                        (min, nat.min(cap).max(min), -1, -1)
                    }
                    _ => (min, nat, min_baseline, nat_baseline),
                };
            }
            match orientation {
                // The child's width doesn't depend on the surface height.
                gtk4::Orientation::Horizontal => {
                    let (min, nat, _, _) = child.measure(orientation, -1);
                    (min, nat, -1, -1)
                }
                // The surface is sized to the monitor and the content is
                // placed and capped within it in size_allocate. Asking for
                // the content's height would grow the window past the screen:
                // gtk4-layer-shell requests the window's size even when
                // anchored to both edges, and some compositors honour it.
                _ => (0, 0, -1, -1),
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            let Some(child) = self.child.upgrade() else {
                return;
            };
            match self.vertical.get() {
                Some(placement) => self.allocate_vertical(&child, placement, width, height),
                None => {
                    let height = self.height_cap.get().map_or(height, |cap| {
                        let (min, _, _, _) = child.measure(gtk4::Orientation::Vertical, width);
                        height.min(cap).max(min)
                    });
                    child.allocate(width, height, baseline, None);
                    self.clear_input_region();
                }
            }
        }

        fn unmap(&self) {
            self.parent_unmap();
            // Re-apply the input region on the next map. The rect is kept so
            // pass-through still clears it if the bar turns horizontal.
            self.input_stale.set(true);
        }
    }

    impl PopoverPlacer {
        fn allocate_vertical(
            &self,
            child: &gtk4::Widget,
            placement: VerticalPlacement,
            width: i32,
            height: i32,
        ) {
            let obj = self.obj();
            let native = obj.native();
            // Where this widget sits in its surface.
            let (surface_dx, surface_dy) = native
                .as_ref()
                .map_or((0.0, 0.0), |native| native.surface_transform());
            let origin = native
                .as_ref()
                .and_then(|native| obj.compute_point(native, &Point::zero()))
                .map_or((0, 0), |p| {
                    (
                        (p.x() as f64 + surface_dx).round() as i32,
                        (p.y() as f64 + surface_dy).round() as i32,
                    )
                });
            let surface_height = native
                .as_ref()
                .and_then(|native| native.surface())
                .map_or(height + origin.1, |surface| surface.height());

            let (top_inset, bottom_inset) = edge_insets(
                placement.edge_gap,
                origin.1,
                surface_height - origin.1 - height,
            );
            let span = (height - top_inset - bottom_inset).max(0);
            let (child_min, child_nat, _, _) = child.measure(gtk4::Orientation::Vertical, width);
            let child_height = child_nat.min(span).max(child_min);

            let home = match self.home.get() {
                Some(home) if home.span == span => home.top,
                _ => {
                    let anchor = placement.anchor_y.map(|y| y - origin.1 - top_inset);
                    let top = home_top(anchor, child_height, span);
                    self.home.set(Some(Home { top, span }));
                    top
                }
            };
            let top = top_inset + placed_top(home, child_height, span);
            let transform = gsk::Transform::new().translate(&Point::new(0.0, top as f32));
            child.allocate(width, child_height, -1, Some(transform));

            let rect = (origin.0, origin.1 + top, width, child_height);
            let stale = self.input_stale.replace(false);
            if self.input_rect.replace(Some(rect)) == Some(rect) && !stale {
                return;
            }
            if let Some(surface) = native.and_then(|native| native.surface()) {
                let region = gtk4::cairo::Region::create_rectangle(
                    &gtk4::cairo::RectangleInt::new(rect.0, rect.1, rect.2, rect.3),
                );
                surface.set_input_region(Some(&region));
            }
            let on_placed = self.on_placed.borrow().clone();
            if let Some(on_placed) = on_placed {
                on_placed();
            }
        }

        fn clear_input_region(&self) {
            self.input_stale.set(false);
            if self.input_rect.take().is_some()
                && let Some(surface) = self.obj().native().and_then(|native| native.surface())
            {
                surface.set_input_region(None);
            }
        }
    }
}

glib::wrapper! {
    /// Caps its single child's height and, on left/right bars, positions it
    /// inside a full-height popover surface.
    pub struct PopoverPlacer(ObjectSubclass<imp::PopoverPlacer>)
        @extends gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl Default for PopoverPlacer {
    fn default() -> Self {
        Self::new()
    }
}

impl PopoverPlacer {
    pub fn new() -> Self {
        glib::Object::builder().build()
    }

    pub fn set_child(&self, child: &impl IsA<gtk4::Widget>) {
        let imp = self.imp();
        if let Some(old) = imp.child.upgrade() {
            old.unparent();
        }
        child.as_ref().set_parent(self);
        imp.child.set(Some(child.as_ref()));
    }

    /// Place vertically (left/right bars), or pass layout through with `None`.
    /// A new anchor or mode starts from a fresh home.
    pub(crate) fn set_vertical(&self, placement: Option<VerticalPlacement>) {
        let imp = self.imp();
        let old = imp.vertical.replace(placement);
        if old == placement {
            return;
        }
        if old.map(|p| p.anchor_y) != placement.map(|p| p.anchor_y) {
            imp.home.set(None);
        }
        // A box only hands spare height to expanding children.
        self.set_vexpand(placement.is_some());
        self.queue_resize();
    }

    /// Cap the content's height, e.g. to the screen space beyond the bar. A
    /// scrolled window inside the content scrolls the rest. `None` leaves the
    /// height uncapped.
    pub(crate) fn set_height_cap(&self, cap: Option<i32>) {
        if self.imp().height_cap.replace(cap) != cap {
            self.queue_resize();
        }
    }

    /// Recentre on the anchor at the next layout, e.g. on a fresh open.
    pub(crate) fn reset_home(&self) {
        self.imp().home.set(None);
        self.queue_allocate();
    }

    /// Run `callback` during layout whenever vertical placement moves or
    /// resizes the child, e.g. to move a blur region in the same frame.
    pub(crate) fn connect_placed(&self, callback: impl Fn() + 'static) {
        *self.imp().on_placed.borrow_mut() = Some(Rc::new(callback));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insets_top_up_the_gap() {
        assert_eq!(edge_insets(8, 8, 8), (0, 0));
        assert_eq!(edge_insets(8, 4, 0), (4, 8));
        assert_eq!(edge_insets(8, 12, 20), (0, 0));
    }

    #[test]
    fn home_centres_on_anchor_within_span() {
        let cases = [
            ("centred", Some(500), 200, 1000, 400),
            ("clamped at top", Some(50), 200, 1000, 0),
            ("clamped at bottom", Some(990), 200, 1000, 800),
            ("no anchor opens at bottom", None, 200, 1000, 800),
            ("taller than span", Some(500), 1200, 1000, 0),
        ];
        for (name, anchor, height, span, expected) in cases {
            assert_eq!(home_top(anchor, height, span), expected, "{name}");
        }
    }

    /// Steps a popover homed at 400 in a 1000px span through growth and back.
    #[test]
    fn grows_down_then_up_and_shrinks_back_the_same_way() {
        let home = 400;
        // (height, expected top): top held, bottom pinned, top capped.
        let steps = [(200, 400), (600, 400), (700, 300), (1000, 0), (1200, 0)];
        for (height, top) in steps.iter().chain(steps.iter().rev()) {
            assert_eq!(placed_top(home, *height, 1000), *top, "height {height}");
        }
    }

    #[test]
    #[ignore = "UI regression test: needs a GTK display; run under Xvfb"]
    fn test_ui_regression_popover_placer() {
        crate::ui_regression_test_support::run_ignored_contract_subprocess(
            "widgets::popover_placer::tests::popover_placer_runner",
            "VIBEPANEL_POPOVER_PLACER_UI_REGRESSION_TEST",
            "popover.placer",
            "popover placer test",
        );
    }

    #[test]
    #[ignore = "internal runner for the popover placer subprocess"]
    fn popover_placer_runner() {
        match std::env::var("VIBEPANEL_POPOVER_PLACER_UI_REGRESSION_TEST").as_deref() {
            Ok("popover.placer") => run_test_popover_placer(),
            Ok(other) => panic!("unknown popover placer test: {other}"),
            Err(_) => eprintln!("skipping popover placer test: no test case selected"),
        }
    }

    /// Runs the main loop until a frame has had time to lay out.
    fn wait_for_frame() {
        let done = Rc::new(Cell::new(false));
        let flag = Rc::clone(&done);
        glib::timeout_add_local_once(std::time::Duration::from_millis(50), move || flag.set(true));
        let ctx = glib::MainContext::default();
        while !done.get() {
            ctx.iteration(true);
        }
    }

    /// Lays out a real placer under a wrapper margin, like the popover
    /// shadow margin, and checks where each content height lands.
    fn run_test_popover_placer() {
        use crate::ui_regression_test_support::init_gtk_or_skip;
        if !init_gtk_or_skip("popover placer test") {
            return;
        }

        const WRAPPER_MARGIN: i32 = 4;
        const GAP: i32 = 8;

        // Like the popovers: natural height from the content, scrolled when
        // allocated less.
        let filler = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        filler.set_size_request(200, 100);
        let content = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .propagate_natural_height(true)
            .child(&filler)
            .build();
        let placer = PopoverPlacer::new();
        placer.set_child(&content);
        let wrapper = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        wrapper.set_margin_top(WRAPPER_MARGIN);
        wrapper.set_margin_bottom(WRAPPER_MARGIN);
        wrapper.append(&placer);

        let window = gtk4::Window::builder()
            .decorated(false)
            .default_width(200)
            .default_height(600)
            .child(&wrapper)
            .build();
        let placed_count = Rc::new(Cell::new(0));
        let count = Rc::clone(&placed_count);
        placer.connect_placed(move || count.set(count.get() + 1));
        placer.set_vertical(Some(VerticalPlacement {
            anchor_y: Some(200),
            edge_gap: GAP,
        }));
        window.present();
        wait_for_frame();

        let bounds = |height: i32| {
            filler.set_size_request(200, height);
            wait_for_frame();
            let b = content
                .compute_bounds(&window)
                .expect("content is laid out");
            (b.y() as i32, b.height() as i32)
        };

        // The display may clamp the window below its default height.
        let surface = window.height();
        // Home is centred on the anchor: 200 - 100 / 2.
        let fits_below = surface - GAP - 150 - 20;
        let overflows = fits_below + 70;
        let steps = [
            ("opens centred", 100, (150, 100)),
            ("grows down from home", fits_below, (150, fits_below)),
            (
                "bottom held at the gap",
                overflows,
                (surface - GAP - overflows, overflows),
            ),
            (
                "capped at both gaps",
                surface + 100,
                (GAP, surface - 2 * GAP),
            ),
        ];
        for (name, height, expected) in steps.iter().chain(steps.iter().rev()) {
            assert_eq!(bounds(*height), *expected, "{name} at {height}px");
        }
        assert!(placed_count.get() > 0, "placement callback runs");

        // Pass-through gives the child the whole placer again.
        placer.set_vertical(None);
        wait_for_frame();
        let b = content
            .compute_bounds(&window)
            .expect("content is laid out");
        assert_eq!(b.y() as i32, WRAPPER_MARGIN);

        // The cap limits the content's height and the rest scrolls, but
        // never below what can't scroll.
        placer.set_height_cap(Some(150));
        assert_eq!(bounds(400).1, 150, "capped");
        content.set_min_content_height(200);
        assert_eq!(bounds(400).1, 200, "the content's minimum wins");
        content.set_min_content_height(-1);
        placer.set_height_cap(None);
        assert_eq!(bounds(400).1, 400, "uncapped");

        window.close();
    }
}
