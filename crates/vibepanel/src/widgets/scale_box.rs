//! A container that animates a true scale transform around its center.
//!
//! The child is always measured and allocated at full size — `snapshot()`
//! applies a GSK scale transform about the widget center, so text, icons,
//! and borders genuinely scale with the popover.
//!
//! ## Why the scale is quantized
//!
//! The scale value is quantized to multiples of `1/QUANT_DENOM` when set.
//! Text rendered under a transform is rasterized per unique
//! effective scale, and renderer glyph caches key their entries by that
//! scale. A continuous per-frame scale therefore creates an endless stream
//! of single-use cache entries: cairo caps its caches (bounded bloat), but
//! the GPU renderers grow without bound. Quantization keeps the set of text
//! scales small and repeating, so the caches warm up once and stay flat.
//!
//! CSS `transform: scale()` transitions hit the same leak but offer no
//! quantization hook, which is why the animation is driven from a tick
//! callback instead of CSS.

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use std::cell::Cell;

/// Quantization denominator: scale moves in steps of 1/128.
const QUANT_DENOM: f64 = 128.0;

fn quantize_scale(s: f64) -> f64 {
    (s * QUANT_DENOM).round() / QUANT_DENOM
}

mod imp {
    use super::*;

    pub struct ScaleBox {
        /// Current scale factor (1.0 = normal size).
        pub(super) scale: Cell<f64>,
        /// The single child widget.
        pub(super) child: glib::WeakRef<gtk4::Widget>,
    }

    impl Default for ScaleBox {
        fn default() -> Self {
            Self {
                scale: Cell::default(),
                child: glib::WeakRef::new(),
            }
        }
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
            // Full allocation — the scale effect is purely visual via snapshot().
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

            if s >= 1.0 {
                widget.snapshot_child(&child, snapshot);
                return;
            }
            if s <= 0.0 {
                return;
            }

            let cx = widget.width() as f32 / 2.0;
            let cy = widget.height() as f32 / 2.0;
            snapshot.save();
            // Scale about the widget center: translate(c*(1-s)) then scale(s).
            snapshot.translate(&gtk4::graphene::Point::new(
                cx * (1.0 - s as f32),
                cy * (1.0 - s as f32),
            ));
            snapshot.scale(s as f32, s as f32);
            widget.snapshot_child(&child, snapshot);
            snapshot.restore();
        }
    }
}

glib::wrapper! {
    /// A container that renders its child through an animated center scale
    /// transform. Child always gets full allocation; the scale is quantized
    /// (see module docs) to keep renderer glyph caches bounded.
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

    /// Get the current quantized scale factor.
    pub fn scale(&self) -> f64 {
        self.imp().scale.get()
    }

    /// Set the scale factor and queue a repaint.
    /// Only calls `queue_draw()` — no layout or CSS resolution.
    pub fn set_scale(&self, scale: f64) {
        let imp = self.imp();
        let scale = quantize_scale(scale.clamp(0.0, 1.0));
        if (imp.scale.get() - scale).abs() < f64::EPSILON {
            return;
        }
        imp.scale.set(scale);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantize_scale_maps_animation_endpoints() {
        assert_eq!(quantize_scale(0.94), 120.0 / QUANT_DENOM);
        assert_eq!(quantize_scale(1.0), 1.0);
    }

    #[test]
    fn quantize_scale_rounds_at_boundary() {
        let midpoint = 120.5 / QUANT_DENOM;
        assert_eq!(quantize_scale(midpoint - 1e-6), 120.0 / QUANT_DENOM);
        assert_eq!(quantize_scale(midpoint), 121.0 / QUANT_DENOM);
        assert_eq!(quantize_scale(midpoint + 1e-6), 121.0 / QUANT_DENOM);
    }
}
