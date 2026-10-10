//! Window and popover subclasses that host native-backend blur.
//!
//! Toasts, the OSD and the media window blur their direct child, and tray menus
//! blur their content box inside a `GtkPopover`. Neither has a vibepanel widget
//! drawn before the blurred one, so these subclasses push the blur nodes (see
//! [`SnapshotBlur`]) before the stock snapshot. They keep the parent's CSS name,
//! so selectors like `window.osd-wrapper > .osd` are unaffected.

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;

use crate::services::background_effect::{BlurShapes, SnapshotBlur, rounded_bounds};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct BlurWindow {
        pub(super) blur: SnapshotBlur,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for BlurWindow {
        const NAME: &'static str = "VibepanelBlurWindow";
        type Type = super::BlurWindow;
        type ParentType = gtk4::Window;
    }

    impl ObjectImpl for BlurWindow {}

    impl WidgetImpl for BlurWindow {
        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            self.blur.snapshot(&*self.obj(), snapshot);
            self.parent_snapshot(snapshot);
        }
    }

    impl WindowImpl for BlurWindow {}

    #[derive(Default)]
    pub struct BlurPopover {
        pub(super) blur: SnapshotBlur,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for BlurPopover {
        const NAME: &'static str = "VibepanelBlurPopover";
        type Type = super::BlurPopover;
        type ParentType = gtk4::Popover;
    }

    impl ObjectImpl for BlurPopover {}

    impl WidgetImpl for BlurPopover {
        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            self.blur.snapshot(&*self.obj(), snapshot);
            self.parent_snapshot(snapshot);
        }
    }

    impl PopoverImpl for BlurPopover {}
}

glib::wrapper! {
    /// A `GtkWindow` that blurs behind its child on the native blur backend.
    pub struct BlurWindow(ObjectSubclass<imp::BlurWindow>)
        @extends gtk4::Window, gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget,
            gtk4::Native, gtk4::Root, gtk4::ShortcutManager;
}

impl BlurWindow {
    /// Create a window that blurs its child's border box with the radius from
    /// `radius_fn`.
    pub fn new(radius_fn: impl Fn() -> u32 + 'static) -> Self {
        let window: Self = glib::Object::builder().build();
        window.imp().blur.set_shapes(
            &window,
            child_shape(move |host: &gtk4::Widget| {
                host.downcast_ref::<gtk4::Window>()
                    .and_then(|window| window.child())
                    .map(|child| (child, radius_fn()))
            }),
        );
        window
    }
}

glib::wrapper! {
    /// A `GtkPopover` that blurs behind its child on the native blur backend.
    pub struct BlurPopover(ObjectSubclass<imp::BlurPopover>)
        @extends gtk4::Popover, gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget,
            gtk4::Native, gtk4::ShortcutManager;
}

impl BlurPopover {
    /// Create a popover that blurs its child's border box with the radius from
    /// `radius_fn`.
    pub fn new(radius_fn: impl Fn() -> u32 + 'static) -> Self {
        let popover: Self = glib::Object::builder().build();
        popover.imp().blur.set_shapes(
            &popover,
            child_shape(move |host: &gtk4::Widget| {
                host.downcast_ref::<gtk4::Popover>()
                    .and_then(|popover| popover.child())
                    .map(|child| (child, radius_fn()))
            }),
        );
        popover
    }
}

/// Shapes for a host that blurs a single child, resolved on every snapshot so
/// a replaced child (e.g. a toast update) is picked up.
fn child_shape(
    resolve: impl Fn(&gtk4::Widget) -> Option<(gtk4::Widget, u32)> + 'static,
) -> BlurShapes {
    std::rc::Rc::new(move |host| {
        resolve(host)
            .and_then(|(child, radius)| rounded_bounds(&child, host, radius as f32))
            .into_iter()
            .collect()
    })
}
