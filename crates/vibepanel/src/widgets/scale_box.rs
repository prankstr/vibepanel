//! A container that simulates scale animation via symmetric rounded clip.
//!
//! The child is always allocated at full size — the scale effect is achieved
//! by clipping to a centered rect in `snapshot()` that grows from smaller to
//! full size. Combined with opacity fade this approximates `transform: scale()`
//! without any scale transforms in the render tree (which are observed to
//! cause unbounded memory growth in GTK4).
//!
//! Only calls `queue_draw()` on scale changes — no layout or CSS resolution.

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use std::cell::Cell;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct ScaleBox {
        /// Current scale factor (1.0 = normal size).
        pub(super) scale: Cell<f64>,
        /// Border radius for the rounded clip (pixels).
        pub(super) radius: Cell<f32>,
        /// The single child widget.
        pub(super) child: glib::WeakRef<gtk4::Widget>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ScaleBox {
        const NAME: &'static str = "VibepanelScaleBox";
        type Type = super::ScaleBox;
        type ParentType = gtk4::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("scale-box");
        }
    }

    impl ObjectImpl for ScaleBox {
        fn constructed(&self) {
            self.parent_constructed();
            self.scale.set(1.0);
        }

        fn dispose(&self) {
            if let Some(child) = self.child.upgrade() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for ScaleBox {
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
            // Full allocation — scale effect is purely visual via snapshot() clipping.
            if let Some(child) = self.child.upgrade() {
                child.allocate(width, height, baseline, None);
            }
        }

        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            let Some(child) = self.child.upgrade() else {
                return;
            };

            let s = self.scale.get();
            let widget = self.obj();

            if (s - 1.0).abs() < f64::EPSILON {
                widget.snapshot_child(&child, snapshot);
                return;
            }
            if s <= 0.0 {
                return;
            }

            // Rounded center-clip: crop edges uniformly, matching surface border radius.
            let w = widget.width() as f32;
            let h = widget.height() as f32;
            let cw = w * s as f32;
            let ch = h * s as f32;
            let dx = (w - cw) / 2.0;
            let dy = (h - ch) / 2.0;

            let radius = self.radius.get();
            let rect = gtk4::graphene::Rect::new(dx, dy, cw, ch);
            let rounded = gtk4::gsk::RoundedRect::new(
                rect,
                gtk4::graphene::Size::new(radius, radius),
                gtk4::graphene::Size::new(radius, radius),
                gtk4::graphene::Size::new(radius, radius),
                gtk4::graphene::Size::new(radius, radius),
            );

            snapshot.push_rounded_clip(&rounded);
            widget.snapshot_child(&child, snapshot);
            snapshot.pop();
        }
    }
}

glib::wrapper! {
    /// A container that simulates scale via symmetric center-clip in `snapshot()`.
    /// Child always gets full allocation. No scale transforms in the render tree.
    pub struct ScaleBox(ObjectSubclass<imp::ScaleBox>)
        @extends gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl Default for ScaleBox {
    fn default() -> Self {
        Self::new()
    }
}

impl ScaleBox {
    /// Create a new ScaleBox with scale 1.0.
    pub fn new() -> Self {
        glib::Object::builder().build()
    }

    /// Get the current scale factor.
    pub fn scale(&self) -> f64 {
        self.imp().scale.get()
    }

    /// Set the border radius used for the rounded clip (pixels).
    pub fn set_radius(&self, radius: f32) {
        let imp = self.imp();
        if (imp.radius.get() - radius).abs() < f32::EPSILON {
            return;
        }
        imp.radius.set(radius);
        self.queue_draw();
    }

    /// Set the scale factor and queue a repaint.
    /// Values below 1.0 crop edges inward; only calls `queue_draw()`.
    pub fn set_scale(&self, scale: f64) {
        let imp = self.imp();
        if (imp.scale.get() - scale).abs() < f64::EPSILON {
            return;
        }
        imp.scale.set(scale.clamp(0.0, 1.0));
        self.queue_draw();
    }

    /// Set the single child widget.
    pub fn set_child(&self, child: &impl IsA<gtk4::Widget>) {
        let imp = self.imp();
        if let Some(old) = imp.child.upgrade() {
            old.unparent();
        }
        let widget = child.as_ref();
        widget.set_parent(self);
        imp.child.set(Some(widget));
    }

    /// Remove the current child widget, if any.
    pub fn remove_child(&self) {
        if let Some(child) = self.imp().child.upgrade() {
            child.unparent();
        }
        self.imp().child.set(None::<&gtk4::Widget>);
    }
}
