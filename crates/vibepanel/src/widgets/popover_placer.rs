//! Sizes and places a popover's content inside its surface.
//!
//! The content never gets taller than its height cap (see
//! [`PopoverPlacer::set_height_cap`]), the screen space a popover may use, so
//! a scrolled window inside it scrolls the rest. Content that can't scroll
//! keeps its minimum height.

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use std::cell::Cell;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct PopoverPlacer {
        pub(super) child: glib::WeakRef<gtk4::Widget>,
        /// Most height the content gets.
        pub(super) height_cap: Cell<Option<i32>>,
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
            let (min, nat, min_baseline, nat_baseline) = child.measure(orientation, for_size);
            match self.height_cap.get() {
                // Only what can scroll gives way; the minimum stays.
                Some(cap) if orientation == gtk4::Orientation::Vertical => {
                    (min, nat.min(cap).max(min), -1, -1)
                }
                _ => (min, nat, min_baseline, nat_baseline),
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            let Some(child) = self.child.upgrade() else {
                return;
            };
            let height = self.height_cap.get().map_or(height, |cap| {
                let (min, _, _, _) = child.measure(gtk4::Orientation::Vertical, width);
                height.min(cap).max(min)
            });
            child.allocate(width, height, baseline, None);
        }
    }
}

glib::wrapper! {
    /// Caps its single child's height.
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

    /// Cap the content's height, e.g. to the screen space beyond the bar. A
    /// scrolled window inside the content scrolls the rest. `None` leaves the
    /// height uncapped.
    pub(crate) fn set_height_cap(&self, cap: Option<i32>) {
        if self.imp().height_cap.replace(cap) != cap {
            self.queue_resize();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

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
