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
//! A revealer animation that crosses from one phase to the other (e.g. grows
//! past the bottom edge in one go) would otherwise slide down first, then up.
//! One that outgrows the cap would stop there while the revealer keeps
//! opening and squeezes the content for the rest of its animation. While
//! revealers registered with [`PopoverPlacer::begin_reveal`] animate, the
//! height and both edges instead move together from where the content was to
//! where it will settle, and arrive when the revealers do. On the way the
//! content is allocated its share of the height it will lose at the end, so
//! what has to shrink or scroll does so in step.
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

/// Top and height of the content within the span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Placed {
    top: i32,
    height: i32,
}

/// Placement of the content together with its uncapped natural height.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sized {
    placed: Placed,
    natural: i32,
}

/// Placement of content that is `natural` tall while it animates from `from`
/// to `to`. The top, and the height the content goes without, move in
/// proportion to how far the natural height has come, so both edges arrive
/// together with it. `None` when the natural height barely changes, so the
/// rule applies.
fn transition_placed(from: Sized, to: Sized, natural: i32) -> Option<Placed> {
    let distance = to.natural - from.natural;
    if distance.abs() <= 1 {
        return None;
    }
    let progress = (f64::from(natural - from.natural) / f64::from(distance)).clamp(0.0, 1.0);
    let between =
        |from: i32, to: i32| (f64::from(from) + progress * f64::from(to - from)).round() as i32;
    // Taken off the natural height, so content that never goes without
    // keeps exactly its natural height, however the end was predicted.
    let withheld = between(
        from.natural - from.placed.height,
        to.natural - to.placed.height,
    );
    Some(Placed {
        top: between(from.placed.top, to.placed.top),
        height: natural - withheld,
    })
}

/// Natural height `revealer` is heading for, and the one it has now.
fn reveal_heights(revealer: &gtk4::Revealer) -> (i32, i32) {
    let target = match revealer.child() {
        Some(child) if revealer.reveals_child() => {
            // A collapsed revealer keeps the width its parent gives it.
            let (min_width, _, _, _) = child.measure(gtk4::Orientation::Horizontal, -1);
            let width = revealer.width().max(min_width);
            child.measure(gtk4::Orientation::Vertical, width).1
        }
        _ => 0,
    };
    let (min_width, _, _, _) = revealer.measure(gtk4::Orientation::Horizontal, -1);
    let (_, current, _, _) =
        revealer.measure(gtk4::Orientation::Vertical, revealer.width().max(min_width));
    (target, current)
}

/// A reveal animation in progress, from where the content was placed.
#[derive(Clone, Copy, Debug)]
struct Transition {
    from: Sized,
    span: i32,
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
        /// Content placement at the last allocation, and its span: the room
        /// between the edge gaps, or the cap when not placed vertically.
        pub(super) placed: Cell<Option<(Sized, i32)>>,
        /// Revealers animating inside the content.
        pub(super) revealing: RefCell<Vec<gtk4::Revealer>>,
        pub(super) transition: Cell<Option<Transition>>,
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
                if orientation != gtk4::Orientation::Vertical {
                    return (min, nat, min_baseline, nat_baseline);
                }
                let span = self.cap_span();
                return match self.transition_placed(nat, span, |_| 0) {
                    Some(placed) => (min, placed.height.min(span).max(min), -1, -1),
                    // Only what can scroll gives way; the minimum stays.
                    None if self.height_cap.get().is_some() => {
                        (min, nat.min(span).max(min), -1, -1)
                    }
                    None => (min, nat, min_baseline, nat_baseline),
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
                    let (min, nat, _, _) = child.measure(gtk4::Orientation::Vertical, width);
                    let span = self.cap_span();
                    let limit = self
                        .transition_placed(nat, span, |_| 0)
                        .map_or(span, |placed| placed.height.min(span));
                    let height = height.min(limit).max(min);
                    self.placed.set(Some((
                        Sized {
                            placed: Placed { top: 0, height },
                            natural: nat,
                        },
                        span,
                    )));
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
            self.end_transition();
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
            let fitted = child_nat.min(span).max(child_min);

            let home = match self.home.get() {
                Some(home) if home.span == span => home.top,
                _ => {
                    let anchor = placement.anchor_y.map(|y| y - origin.1 - top_inset);
                    let top = home_top(anchor, fitted, span);
                    self.home.set(Some(Home { top, span }));
                    top
                }
            };
            // By the placement rule, or between start and end while
            // revealers animate.
            let placed = self
                .transition_placed(child_nat, span, |height| placed_top(home, height, span))
                .map_or(
                    Placed {
                        top: placed_top(home, fitted, span),
                        height: fitted,
                    },
                    |placed| {
                        let height = placed.height.min(span).max(child_min);
                        Placed {
                            top: placed.top.clamp(0, (span - height).max(0)),
                            height,
                        }
                    },
                );
            self.placed.set(Some((
                Sized {
                    placed,
                    natural: child_nat,
                },
                span,
            )));
            let (top, child_height) = (top_inset + placed.top, placed.height);
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

        /// Room for the content when it isn't placed vertically.
        fn cap_span(&self) -> i32 {
            self.height_cap.get().unwrap_or(i32::MAX)
        }

        /// Placement of the content, `natural` tall, while revealers animate
        /// within `span`. `top_for` gives the top it settles at for a height.
        fn transition_placed(
            &self,
            natural: i32,
            span: i32,
            top_for: impl Fn(i32) -> i32,
        ) -> Option<Placed> {
            let transition = self.transition.get().filter(|t| t.span == span)?;
            // Recomputed every frame, so content that changes mid-animation
            // moves the end along with it.
            let revealing = self.revealing.borrow();
            // A revealer inside one that is closing ends at 0 with it, which
            // the outer one already counts.
            let closing_inside = |revealer: &gtk4::Revealer| {
                revealing.iter().any(|outer| {
                    outer != revealer && !outer.reveals_child() && revealer.is_ancestor(outer)
                })
            };
            let to_natural = revealing
                .iter()
                .filter(|revealer| !closing_inside(revealer))
                .map(reveal_heights)
                .fold(natural, |sum, (target, current)| sum + target - current)
                .max(0);
            let to_height = to_natural.min(span);
            let to = Sized {
                placed: Placed {
                    top: top_for(to_height),
                    height: to_height,
                },
                natural: to_natural,
            };
            transition_placed(transition.from, to, natural)
        }

        pub(super) fn end_transition(&self) {
            self.revealing.borrow_mut().clear();
            self.transition.set(None);
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
        if old.is_some() != placement.is_some() {
            imp.end_transition();
            imp.placed.set(None);
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

    /// Run `measure` as if no reveal transition were running, e.g. to find
    /// the height the content will settle at.
    pub(crate) fn measure_settled<R>(&self, measure: impl FnOnce() -> R) -> R {
        let imp = self.imp();
        let Some(transition) = imp.transition.take() else {
            return measure();
        };
        // Drop measurements cached under the transition, before and after.
        self.queue_resize();
        let result = measure();
        imp.transition.set(Some(transition));
        self.queue_resize();
        result
    }

    /// Like [`Self::measure_settled`], with the height cap lifted too, and
    /// return its result with the cap, so a caller can adjust the uncapped
    /// height before capping it.
    pub(crate) fn measure_uncapped<R>(&self, measure: impl FnOnce() -> R) -> (R, Option<i32>) {
        self.measure_settled(|| {
            let imp = self.imp();
            let cap = imp.height_cap.take();
            if cap.is_none() {
                return (measure(), None);
            }
            // Drop measurements cached under the cap, before and after.
            self.queue_resize();
            let result = measure();
            imp.height_cap.set(cap);
            self.queue_resize();
            (result, cap)
        })
    }

    /// Recentre on the anchor at the next layout, e.g. on a fresh open.
    pub(crate) fn reset_home(&self) {
        self.imp().home.set(None);
        self.imp().end_transition();
        // Ending a transition changes what the placer measures.
        self.queue_resize();
    }

    /// Call before `revealer`, inside the content, starts animating. Until
    /// the animations end, the content's height and both its edges move
    /// together to where it will settle (see the module docs). Each call
    /// restarts from the current placement, so a reversal mid-animation
    /// stays smooth.
    ///
    /// The end is predicted by adding each revealer's remaining height change
    /// to the content's natural height, so it assumes those changes reach the
    /// content one for one. A revealer inside a capped scrolled window breaks
    /// that: the popover stops short and settles when the animation ends.
    pub(crate) fn begin_reveal(&self, revealer: &gtk4::Revealer) {
        let imp = self.imp();
        let Some((from, span)) = imp.placed.get() else {
            return;
        };
        if revealer.transition_duration() == 0 {
            return;
        }
        {
            let mut revealing = imp.revealing.borrow_mut();
            if !revealing.contains(revealer) {
                revealing.push(revealer.clone());
            }
        }
        imp.transition.set(Some(Transition { from, span }));
        self.queue_resize();
    }

    /// Call when `revealer` finishes animating or unmaps. The placement rule
    /// takes over once none are left; it lands where the transition ended.
    pub(crate) fn end_reveal(&self, revealer: &gtk4::Revealer) {
        let imp = self.imp();
        let empty = {
            let mut revealing = imp.revealing.borrow_mut();
            revealing.retain(|r| r != revealer);
            revealing.is_empty()
        };
        if empty && imp.transition.take().is_some() {
            self.queue_resize();
        }
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

    /// Content that fits: as tall as it naturally is.
    fn fitting(top: i32, height: i32) -> Sized {
        Sized {
            placed: Placed { top, height },
            natural: height,
        }
    }

    #[test]
    fn transition_moves_both_edges_together() {
        let span = 1000;
        let home = 400;
        let from = fitting(400, 200);
        // Grows past the bottom in one go: settles bottom-pinned.
        let to = fitting(placed_top(home, 800, span), 800);
        let mut previous = from.placed;
        for natural in (200..=800).step_by(25) {
            let placed = transition_placed(from, to, natural).expect("height changes");
            assert_eq!(placed.height, natural, "fits, so keeps its height");
            assert!(placed.top <= previous.top, "top only rises at {natural}px");
            assert!(
                placed.top + placed.height >= previous.top + previous.height,
                "bottom only falls at {natural}px"
            );
            assert!(
                placed.top >= 0 && placed.top + placed.height <= span,
                "stays inside at {natural}px"
            );
            previous = placed;
        }
        assert_eq!(previous, to.placed, "both edges arrive together");
        // Played backwards it returns home the same way.
        assert_eq!(transition_placed(to, from, 200), Some(from.placed));
        // Almost no height change: the placement rule applies.
        assert_eq!(transition_placed(from, fitting(0, 201), 200), None);
    }

    #[test]
    fn transition_reaches_the_cap_with_the_content() {
        let span = 1000;
        let from = fitting(400, 200);
        // Ends 600px taller than the span holds.
        let to = Sized {
            placed: Placed {
                top: 0,
                height: span,
            },
            natural: 1600,
        };
        // Halfway there the content already overflows the span on its own,
        // but the popover is only halfway to it.
        assert_eq!(
            transition_placed(from, to, 900),
            Some(Placed {
                top: 200,
                height: 600
            })
        );
        assert_eq!(transition_placed(from, to, 1600), Some(to.placed));
        // Back down it leaves the cap at once.
        assert_eq!(
            transition_placed(to, from, 1250),
            Some(Placed {
                top: 100,
                height: 800
            })
        );
        // A wrong guess at the end takes nothing off content that fits.
        let guessed = fitting(390, 810);
        assert_eq!(transition_placed(from, guessed, 800).unwrap().height, 800);
    }

    #[test]
    #[ignore = "UI regression test: needs a GTK display; run under Xvfb"]
    fn test_ui_regression_popover_placer_reveal() {
        crate::ui_regression_test_support::run_ignored_contract_subprocess(
            "widgets::popover_placer::tests::popover_placer_runner",
            "VIBEPANEL_POPOVER_PLACER_UI_REGRESSION_TEST",
            "popover.placer.reveal",
            "popover placer reveal test",
        );
    }

    #[test]
    #[ignore = "UI regression test: needs a GTK display; run under Xvfb"]
    fn test_ui_regression_popover_placer_reveal_past_cap() {
        crate::ui_regression_test_support::run_ignored_contract_subprocess(
            "widgets::popover_placer::tests::popover_placer_runner",
            "VIBEPANEL_POPOVER_PLACER_UI_REGRESSION_TEST",
            "popover.placer.reveal-past-cap",
            "popover placer reveal past cap test",
        );
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
            Ok("popover.placer.reveal") => run_test_popover_placer_reveal(),
            Ok("popover.placer.reveal-past-cap") => run_test_popover_placer_reveal_past_cap(),
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
        let growing = steps.iter().map(|step| ("growing", step));
        let shrinking = steps.iter().rev().map(|step| ("shrinking", step));
        for (direction, (name, height, expected)) in growing.chain(shrinking) {
            assert_eq!(
                bounds(*height),
                *expected,
                "{direction}: {name} at {height}px"
            );
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

    fn revealer_with(height: i32, revealed: bool) -> gtk4::Revealer {
        let child = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        child.set_size_request(200, height);
        gtk4::Revealer::builder()
            .transition_type(gtk4::RevealerTransitionType::SlideDown)
            .transition_duration(300)
            .reveal_child(revealed)
            .child(&child)
            .build()
    }

    /// Animates real revealers inside a placer and checks every placement
    /// on the way: crossing the bottom edge, back, and an accordion swap.
    fn run_test_popover_placer_reveal() {
        use crate::ui_regression_test_support::init_gtk_or_skip;
        if !init_gtk_or_skip("popover placer reveal test") {
            return;
        }
        const GAP: i32 = 8;
        const HEADER: i32 = 100;

        let header = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        header.set_size_request(200, HEADER);
        let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        content.append(&header);
        let placer = PopoverPlacer::new();
        placer.set_child(&content);
        let window = gtk4::Window::builder()
            .decorated(false)
            .default_width(200)
            .default_height(600)
            .child(&placer)
            .build();
        let samples: Rc<RefCell<Vec<Placed>>> = Rc::default();
        {
            let placer_weak = placer.downgrade();
            let samples = Rc::clone(&samples);
            placer.connect_placed(move || {
                if let Some((sized, _)) = placer_weak.upgrade().and_then(|p| p.imp().placed.get()) {
                    samples.borrow_mut().push(sized.placed);
                }
            });
        }
        placer.set_vertical(Some(VerticalPlacement {
            anchor_y: Some(250),
            edge_gap: GAP,
        }));
        window.present();
        wait_for_frame();
        let span = window.height() - 2 * GAP;
        // Home centres the 100px header on 250: top 200 in the surface.
        let home = 250 - GAP - HEADER / 2;

        let animate = |revealers: &[(&gtk4::Revealer, bool)]| -> Vec<Placed> {
            samples.borrow_mut().clear();
            for (revealer, reveal) in revealers {
                placer.begin_reveal(revealer);
                revealer.set_reveal_child(*reveal);
            }
            let settled = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while revealers
                .iter()
                .any(|(r, _)| r.is_child_revealed() != r.reveals_child())
                && std::time::Instant::now() < settled
            {
                wait_for_frame();
            }
            wait_for_frame();
            samples.borrow().clone()
        };

        // Grows 80px past the space below home in one animation.
        let below = span - home - HEADER;
        let grow = revealer_with(below + 80, false);
        content.append(&grow);
        {
            let placer_weak = placer.downgrade();
            grow.connect_child_revealed_notify(move |r| {
                if let Some(placer) = placer_weak.upgrade() {
                    placer.end_reveal(r);
                }
            });
        }
        wait_for_frame();
        let full = HEADER + below + 80;
        let settled = Placed {
            top: span - full,
            height: full,
        };
        let start = placer.imp().placed.get().expect("placed").0.placed;
        let expand = animate(&[(&grow, true)]);
        assert!(expand.len() > 3, "expansion is animated: {expand:?}");
        let edges = |p: &Placed| (p.top, p.top + p.height);
        for pair in expand.windows(2) {
            let ((top0, bottom0), (top1, bottom1)) = (edges(&pair[0]), edges(&pair[1]));
            assert!(
                top1 <= top0 && bottom1 >= bottom0,
                "edges only grow apart: {expand:?}"
            );
        }
        assert!(
            expand
                .iter()
                .all(|p| p.top >= 0 && p.top + p.height <= span),
            "stays inside the gaps: {expand:?}"
        );
        assert_eq!(expand.last(), Some(&settled), "settles bottom-pinned");
        // Both edges share one progress. A pixel of rounding is about 0.015
        // of the top's shorter trip.
        let along = |value: i32, from: i32, to: i32| f64::from(value - from) / f64::from(to - from);
        let ((start_top, start_bottom), (end_top, end_bottom)) = (edges(&start), edges(&settled));
        for p in &expand {
            let (top, bottom) = edges(p);
            let (top, bottom) = (
                along(top, start_top, end_top),
                along(bottom, start_bottom, end_bottom),
            );
            assert!(
                (top - bottom).abs() <= 0.03,
                "edges move together (top {top:.2}, bottom {bottom:.2} of the way): {expand:?}"
            );
        }

        // Collapsing runs it back to home.
        let collapse = animate(&[(&grow, false)]);
        for pair in collapse.windows(2) {
            let ((top0, bottom0), (top1, bottom1)) = (edges(&pair[0]), edges(&pair[1]));
            assert!(
                top1 >= top0 && bottom1 <= bottom0,
                "edges only close in: {collapse:?}"
            );
        }
        assert_eq!(
            collapse.last(),
            Some(&Placed {
                top: home,
                height: HEADER
            }),
            "returns home"
        );
        content.remove(&grow);

        // A row closes while a revealer inside it opens, like dismissing a
        // notification whose "Show more" is expanding. The inner one ends at
        // 0 with the row, so the popover heads straight home.
        let inner = revealer_with(200, false);
        let row_body = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        let row_filler = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        row_filler.set_size_request(200, 300);
        row_body.append(&row_filler);
        row_body.append(&inner);
        let row = gtk4::Revealer::builder()
            .transition_type(gtk4::RevealerTransitionType::SlideDown)
            .transition_duration(300)
            .reveal_child(true)
            .child(&row_body)
            .build();
        content.append(&row);
        for revealer in [&row, &inner] {
            let placer_weak = placer.downgrade();
            revealer.connect_child_revealed_notify(move |r| {
                if let Some(placer) = placer_weak.upgrade() {
                    placer.end_reveal(r);
                }
            });
        }
        wait_for_frame();
        let start = placer.imp().placed.get().unwrap().0.placed;
        let dismiss = animate(&[(&inner, true), (&row, false)]);
        let end = Placed {
            top: home,
            height: HEADER,
        };
        // Heading for a fixed end, every frame lies on the line between the
        // two placements; a moving estimate bends the path.
        for p in &dismiss {
            let expected = transition_placed(
                fitting(start.top, start.height),
                fitting(end.top, end.height),
                p.height,
            )
            .unwrap();
            assert!(
                (p.top - expected.top).abs() <= 2,
                "{p:?} is off the path from {start:?} to {end:?}: {dismiss:?}"
            );
        }
        for pair in dismiss.windows(2) {
            let ((top0, bottom0), (top1, bottom1)) = (edges(&pair[0]), edges(&pair[1]));
            assert!(
                top1 >= top0 && bottom1 <= bottom0,
                "edges only close in: {dismiss:?}"
            );
        }
        assert_eq!(
            dismiss.last(),
            Some(&Placed {
                top: home,
                height: HEADER
            }),
            "returns home"
        );
        content.remove(&row);

        // Accordion: one 150px card closes while another opens. The height
        // holds steady instead of dropping and growing back.
        let open = revealer_with(150, true);
        let closed = revealer_with(150, false);
        content.append(&open);
        content.append(&closed);
        for revealer in [&open, &closed] {
            let placer_weak = placer.downgrade();
            revealer.connect_child_revealed_notify(move |r| {
                if let Some(placer) = placer_weak.upgrade() {
                    placer.end_reveal(r);
                }
            });
        }
        wait_for_frame();
        let before = placer.imp().placed.get().unwrap().0.placed;
        let swap = animate(&[(&open, false), (&closed, true)]);
        assert!(
            swap.iter()
                .all(|p| (p.height - before.height).abs() <= 2 && (p.top - before.top).abs() <= 2),
            "swap keeps the popover steady around {before:?}: {swap:?}"
        );

        window.close();
    }

    /// Opens a card that outgrows the room the popover has, placed
    /// vertically and only capped, and closes it again. The popover and the
    /// card arrive together: the popover doesn't fill its room early and
    /// leave the card to squeeze the list inside it afterwards.
    fn run_test_popover_placer_reveal_past_cap() {
        use crate::ui_regression_test_support::init_gtk_or_skip;
        if !init_gtk_or_skip("popover placer reveal past cap test") {
            return;
        }
        const GAP: i32 = 8;
        const HEADER: i32 = 100;
        const CAP: i32 = 400;

        for vertical in [true, false] {
            let mode = if vertical { "placed" } else { "capped" };
            // Like a Quick Settings card: a list that gives way inside a
            // card, inside the popover's own scrolled window.
            let rows = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            let list = gtk4::ScrolledWindow::builder()
                .hscrollbar_policy(gtk4::PolicyType::Never)
                .propagate_natural_height(true)
                .child(&rows)
                .build();
            let card = gtk4::Revealer::builder()
                .transition_type(gtk4::RevealerTransitionType::SlideDown)
                .transition_duration(300)
                .child(&list)
                .build();
            let header = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            header.set_size_request(200, HEADER);
            let body = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            body.append(&header);
            body.append(&card);
            let content = gtk4::ScrolledWindow::builder()
                .hscrollbar_policy(gtk4::PolicyType::Never)
                .propagate_natural_height(true)
                .child(&body)
                .build();
            let placer = PopoverPlacer::new();
            placer.set_child(&content);
            let wrapper = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            wrapper.append(&placer);
            let window = gtk4::Window::builder()
                .decorated(false)
                .default_width(200)
                .default_height(600)
                .child(&wrapper)
                .build();
            if vertical {
                placer.set_vertical(Some(VerticalPlacement {
                    anchor_y: Some(250),
                    edge_gap: GAP,
                }));
            } else {
                placer.set_height_cap(Some(CAP));
            }
            {
                let placer_weak = placer.downgrade();
                card.connect_child_revealed_notify(move |r| {
                    if let Some(placer) = placer_weak.upgrade() {
                        placer.end_reveal(r);
                    }
                });
            }
            window.present();
            wait_for_frame();
            let span = if vertical {
                window.height() - 2 * GAP
            } else {
                CAP
            };
            // Taller than any window the display gives.
            let tall = window.height() + 300;
            rows.set_size_request(200, tall);
            list.set_max_content_height(tall);

            // (placement, list height) each frame.
            let samples: Rc<RefCell<Vec<(Sized, i32)>>> = Rc::default();
            {
                let samples = Rc::clone(&samples);
                let placer_weak = placer.downgrade();
                let list = list.clone();
                window.add_tick_callback(move |_, _| {
                    if let Some((sized, _)) =
                        placer_weak.upgrade().and_then(|p| p.imp().placed.get())
                        && samples.borrow().last().map(|(s, _)| *s) != Some(sized)
                    {
                        samples.borrow_mut().push((sized, list.height()));
                    }
                    glib::ControlFlow::Continue
                });
            }
            let animate = |reveal: bool| {
                wait_for_frame();
                samples.borrow_mut().clear();
                placer.begin_reveal(&card);
                card.set_reveal_child(reveal);
                let settled = std::time::Instant::now() + std::time::Duration::from_secs(3);
                while card.is_child_revealed() != reveal && std::time::Instant::now() < settled {
                    wait_for_frame();
                }
                wait_for_frame();
                samples.borrow().clone()
            };
            let closed = placer.imp().placed.get().unwrap().0;
            assert_eq!(closed.placed.height, HEADER, "{mode}: starts closed");

            let expand = animate(true);
            let open = expand.last().expect("animated").0;
            assert!(expand.len() > 5, "{mode}: animated: {expand:?}");
            assert_eq!(open.placed.height, span, "{mode}: fills its room");
            assert_eq!(open.natural, HEADER + tall, "{mode}: content outgrows it");
            let list_open = list.height();
            assert!(list_open < tall, "{mode}: the list gives way");
            let collapse = animate(false);
            assert_eq!(collapse.last().unwrap().0, closed, "{mode}: closes again");

            // How far `value` has come from `from` to `to`.
            let along =
                |value: i32, from: i32, to: i32| f64::from(value - from) / f64::from(to - from);
            for (name, run) in [("opening", &expand), ("closing", &collapse)] {
                for (sized, list_height) in run {
                    let progress = along(sized.natural, closed.natural, open.natural);
                    let height = along(sized.placed.height, closed.placed.height, span);
                    assert!(
                        (height - progress).abs() <= 0.03,
                        "{mode}, {name}: height keeps pace with the card \
                         ({height:.2} of the way at {progress:.2}): {run:?}"
                    );
                    if vertical && open.placed.top != closed.placed.top {
                        let top = along(sized.placed.top, closed.placed.top, open.placed.top);
                        assert!(
                            (top - progress).abs() <= 0.03,
                            "{mode}, {name}: top keeps pace with the card \
                             ({top:.2} of the way at {progress:.2}): {run:?}"
                        );
                    }
                    // The list slides in at the height it ends with.
                    if progress > 0.2 {
                        assert!(
                            (list_height - list_open).abs() <= 12,
                            "{mode}, {name}: list holds its {list_open}px, \
                             got {list_height}px at {progress:.2}: {run:?}"
                        );
                    }
                }
            }
            window.close();
            wait_for_frame();
        }
    }
}
