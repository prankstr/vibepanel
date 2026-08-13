//! A custom widget that displays a paintable with rounded corners.
//!
//! This widget uses GSK's `push_rounded_clip()` for GPU-accelerated
//! rounded corner clipping, avoiding manual pixel manipulation.

use gtk4::gdk::Paintable;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use std::cell::{Cell, RefCell};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct RoundedPicture {
        pub(super) paintable: RefCell<Option<Paintable>>,
        pub(super) corner_radius: Cell<f32>,
        pub(super) pixel_size: Cell<i32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RoundedPicture {
        const NAME: &'static str = "VibepanelRoundedPicture";
        type Type = super::RoundedPicture;
        type ParentType = gtk4::Widget;
        type Interfaces = (gtk4::Accessible, gtk4::Buildable);
    }

    impl ObjectImpl for RoundedPicture {
        fn constructed(&self) {
            self.parent_constructed();
            // Keep the widget from stretching to its parent's allocation.
            self.obj().set_hexpand(false);
            self.obj().set_vexpand(false);
            self.obj().set_halign(gtk4::Align::Center);
            self.obj().set_valign(gtk4::Align::Center);
        }
    }

    impl WidgetImpl for RoundedPicture {
        fn measure(&self, orientation: gtk4::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let size = self.pixel_size.get();
            if size > 0 {
                (size, size, -1, -1)
            } else if let Some(paintable) = self.paintable.borrow().as_ref() {
                let intrinsic = match orientation {
                    gtk4::Orientation::Horizontal => paintable.intrinsic_width(),
                    gtk4::Orientation::Vertical => paintable.intrinsic_height(),
                    _ => 0,
                };
                (intrinsic, intrinsic, -1, -1)
            } else {
                (0, 0, -1, -1)
            }
        }

        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            let Some(paintable) = self.paintable.borrow().clone() else {
                return;
            };

            let widget = self.obj();
            let pixel_size = self.pixel_size.get();

            let (width, height) = if pixel_size > 0 {
                (pixel_size as f32, pixel_size as f32)
            } else {
                (widget.width() as f32, widget.height() as f32)
            };

            if width <= 0.0 || height <= 0.0 {
                return;
            }

            let radius = self.corner_radius.get().min(width / 2.0).min(height / 2.0);

            if radius > 0.0 {
                let bounds = gtk4::graphene::Rect::new(0.0, 0.0, width, height);
                let corner = gtk4::graphene::Size::new(radius, radius);
                let rounded_rect = gtk4::gsk::RoundedRect::new(
                    bounds, corner, // top-left
                    corner, // top-right
                    corner, // bottom-right
                    corner, // bottom-left
                );

                snapshot.push_rounded_clip(&rounded_rect);
                snapshot_paintable_cropped(snapshot, &paintable, width, height);
                snapshot.pop();
            } else {
                snapshot.push_clip(&gtk4::graphene::Rect::new(0.0, 0.0, width, height));
                snapshot_paintable_cropped(snapshot, &paintable, width, height);
                snapshot.pop();
            }
        }
    }

    impl AccessibleImpl for RoundedPicture {}
    impl BuildableImpl for RoundedPicture {}
}

fn snapshot_paintable_cropped(
    snapshot: &gtk4::Snapshot,
    paintable: &Paintable,
    width: f32,
    height: f32,
) {
    let intrinsic_width = paintable.intrinsic_width() as f32;
    let intrinsic_height = paintable.intrinsic_height() as f32;
    if intrinsic_width <= 0.0 || intrinsic_height <= 0.0 {
        paintable.snapshot(snapshot, width as f64, height as f64);
        return;
    }

    let scale = (width / intrinsic_width).max(height / intrinsic_height);
    let render_width = intrinsic_width * scale;
    let render_height = intrinsic_height * scale;
    let offset =
        gtk4::graphene::Point::new((width - render_width) / 2.0, (height - render_height) / 2.0);

    snapshot.save();
    snapshot.translate(&offset);
    paintable.snapshot(snapshot, render_width as f64, render_height as f64);
    snapshot.restore();
}

glib::wrapper! {
    /// A widget that displays a paintable with rounded corners.
    ///
    /// Uses GPU-accelerated clipping via GSK's rounded clip nodes.
    pub struct RoundedPicture(ObjectSubclass<imp::RoundedPicture>)
        @extends gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl RoundedPicture {
    /// Create a new RoundedPicture widget.
    pub fn new() -> Self {
        glib::Object::new()
    }

    /// Set the paintable to display.
    pub fn set_paintable(&self, paintable: Option<&impl IsA<Paintable>>) {
        self.imp()
            .paintable
            .replace(paintable.map(|p| p.as_ref().clone()));
        self.queue_draw();
    }

    /// Set the corner radius for rounding.
    pub fn set_corner_radius(&self, radius: f32) {
        self.imp().corner_radius.set(radius);
        self.queue_draw();
    }

    /// Get the current corner radius.
    pub fn corner_radius(&self) -> f32 {
        self.imp().corner_radius.get()
    }

    /// Set a fixed pixel size (both width and height).
    ///
    /// If set to 0, the widget will use the paintable's intrinsic size.
    pub fn set_pixel_size(&self, size: i32) {
        self.imp().pixel_size.set(size);
        self.queue_resize();
    }

    /// Get the current pixel size.
    pub fn pixel_size(&self) -> i32 {
        self.imp().pixel_size.get()
    }
}

impl Default for RoundedPicture {
    fn default() -> Self {
        Self::new()
    }
}
