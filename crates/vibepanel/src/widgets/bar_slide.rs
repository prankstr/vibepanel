//! Visual-only, edge-directed bar transitions with stable hidden layout.

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use std::cell::Cell;

use vibepanel_core::config::BarPosition;

mod imp {
    use super::*;

    pub struct BarSlide {
        /// Revealed fraction (1.0 = fully visible).
        pub(super) progress: Cell<f64>,
        pub(super) position: Cell<BarPosition>,
        pub(super) child: glib::WeakRef<gtk4::Widget>,
    }

    impl Default for BarSlide {
        fn default() -> Self {
            Self {
                progress: Cell::default(),
                position: Cell::new(BarPosition::Top),
                child: glib::WeakRef::new(),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for BarSlide {
        const NAME: &'static str = "VibepanelBarSlide";
        type Type = super::BarSlide;
        type ParentType = gtk4::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("bar-slide");
        }
    }

    impl ObjectImpl for BarSlide {
        fn constructed(&self) {
            self.parent_constructed();
            self.progress.set(1.0);
        }

        fn dispose(&self) {
            if let Some(child) = self.child.upgrade() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for BarSlide {
        fn request_mode(&self) -> gtk4::SizeRequestMode {
            if let Some(child) = self.child.upgrade() {
                child.request_mode()
            } else {
                gtk4::SizeRequestMode::ConstantSize
            }
        }

        fn measure(&self, orientation: gtk4::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            if let Some(child) = self.child.upgrade() {
                child.measure(orientation, for_size)
            } else {
                (0, 0, -1, -1)
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            // Full allocation keeps hidden geometry and intellihide stable.
            if let Some(child) = self.child.upgrade() {
                child.allocate(width, height, baseline, None);
            }
        }

        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            let Some(child) = self.child.upgrade() else {
                return;
            };

            let s = self.progress.get();
            let widget = self.obj();

            if s >= 1.0 {
                widget.snapshot_child(&child, snapshot);
                return;
            }
            if s <= 0.0 {
                return;
            }

            let width = widget.width() as f32;
            let height = widget.height() as f32;
            let distance = 1.0 - s as f32;
            let (x, y) = match self.position.get() {
                BarPosition::Top => (0.0, -height * distance),
                BarPosition::Bottom => (0.0, height * distance),
                BarPosition::Left => (-width * distance, 0.0),
                BarPosition::Right => (width * distance, 0.0),
            };
            snapshot.push_clip(&gtk4::graphene::Rect::new(0.0, 0.0, width, height));
            snapshot.save();
            snapshot.translate(&gtk4::graphene::Point::new(x.round(), y.round()));
            widget.snapshot_child(&child, snapshot);
            snapshot.restore();
            snapshot.pop();
        }
    }
}

glib::wrapper! {
    /// Slides content visually without changing its allocation or measured bounds.
    pub struct BarSlide(ObjectSubclass<imp::BarSlide>)
        @extends gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl Default for BarSlide {
    fn default() -> Self {
        Self::new()
    }
}

impl BarSlide {
    pub fn new() -> Self {
        glib::Object::builder().build()
    }

    pub fn set_progress(&self, progress: f64) {
        let imp = self.imp();
        let progress = progress.clamp(0.0, 1.0);
        if (imp.progress.get() - progress).abs() < f64::EPSILON {
            return;
        }
        imp.progress.set(progress);
        self.queue_draw();
    }

    pub fn set_position(&self, position: BarPosition) {
        self.imp().position.set(position);
    }

    pub fn set_child(&self, child: &impl IsA<gtk4::Widget>) {
        let imp = self.imp();
        if let Some(old) = imp.child.upgrade() {
            old.unparent();
        }
        let widget = child.as_ref();
        widget.set_parent(self);
        imp.child.set(Some(widget));
    }
}
