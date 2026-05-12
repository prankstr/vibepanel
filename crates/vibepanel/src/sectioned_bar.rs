//! Center-priority layout manager and sectioned bar widget.
//!
//! Custom GTK4 LayoutManager that positions:
//! - Left section: anchored to left edge
//! - Center section: anchored to the true center of the bar
//! - Right section: anchored to right edge
//!
//! The center section has priority - side sections truncate before center when space is tight.

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{LayoutChild, LayoutManager, Orientation, Widget};

use crate::layout_math::{
    SectionSizes, compute_center_priority_allocation, compute_linear_allocation,
};

mod imp {
    use super::*;
    use std::cell::{Cell, RefCell};

    pub struct CenterPriorityLayout {
        pub orientation: Cell<Orientation>,
        pub spacing: Cell<i32>,
        pub edge_margin: Cell<i32>,
        pub left_expand: Cell<bool>,
        pub right_expand: Cell<bool>,
        // Last allocation positions and widths for snapshot/clipping
        pub last_left_x: Cell<i32>,
        pub last_left_width: Cell<i32>,
        pub last_center_x: Cell<i32>,
        pub last_center_width: Cell<i32>,
        pub last_right_x: Cell<i32>,
        pub last_right_width: Cell<i32>,
        /// Optional callback fired after every allocation pass.
        /// Used by bar blur to recompute island regions when layout changes.
        pub on_allocate: RefCell<Option<Box<dyn Fn()>>>,
    }

    impl Default for CenterPriorityLayout {
        fn default() -> Self {
            Self {
                orientation: Cell::new(Orientation::Horizontal),
                spacing: Cell::default(),
                edge_margin: Cell::default(),
                left_expand: Cell::default(),
                right_expand: Cell::default(),
                last_left_x: Cell::default(),
                last_left_width: Cell::default(),
                last_center_x: Cell::default(),
                last_center_width: Cell::default(),
                last_right_x: Cell::default(),
                last_right_width: Cell::default(),
                on_allocate: RefCell::default(),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CenterPriorityLayout {
        const NAME: &'static str = "VibepanelCenterPriorityLayout";
        type Type = super::CenterPriorityLayout;
        type ParentType = LayoutManager;
    }

    impl ObjectImpl for CenterPriorityLayout {}

    impl LayoutManagerImpl for CenterPriorityLayout {
        fn request_mode(&self, _widget: &Widget) -> gtk4::SizeRequestMode {
            gtk4::SizeRequestMode::ConstantSize
        }

        fn measure(
            &self,
            widget: &Widget,
            orientation: Orientation,
            _for_size: i32,
        ) -> (i32, i32, i32, i32) {
            let bar = widget.downcast_ref::<super::SectionedBar>().unwrap();
            let spacing = self.spacing.get();
            let edge = self.edge_margin.get();

            if orientation == self.orientation.get() {
                let mut min_primary = edge * 2;
                let mut nat_primary = edge * 2;
                let mut active_count = 0;

                for section in ["left", "center", "right"] {
                    if let Some(child) = bar.section(section)
                        && child.is_visible()
                    {
                        let (min, nat, _, _) = child.measure(self.orientation.get(), -1);
                        min_primary += min;
                        nat_primary += nat;
                        active_count += 1;
                    }
                }

                let spacing_total = spacing * (active_count - 1).max(0);
                min_primary += spacing_total;
                nat_primary += spacing_total;

                (min_primary, nat_primary, -1, -1)
            } else {
                let mut min_cross = 0;
                let mut nat_cross = 0;

                for section in ["left", "center", "right"] {
                    if let Some(child) = bar.section(section)
                        && child.is_visible()
                    {
                        let (min, nat, _, _) = child.measure(orientation, -1);
                        min_cross = min_cross.max(min);
                        nat_cross = nat_cross.max(nat);
                    }
                }

                (min_cross, nat_cross, -1, -1)
            }
        }

        /// Allocate space to child widgets within the given dimensions.
        ///
        /// # Layout Algorithm
        ///
        /// 1. **Center-first**: The center section is given priority and is anchored
        ///    to the true horizontal center of the bar.
        ///
        /// 2. **Budget calculation**: After center is placed, remaining space on each
        ///    side (minus spacing) becomes the "budget" for left/right sections.
        ///
        /// 3. **Clamping**: Each section receives `clamp_width(budget, min, natural)`,
        ///    which gives the natural size if it fits, otherwise shrinks toward min.
        ///
        /// 4. **Fallback**: If no center widget exists, `allocate_linear` is used
        ///    instead, which distributes space between left and right only.
        ///
        /// # Coordinate system
        ///
        /// - `edge`: margin from container edges to first/last widget
        /// - `interior`: usable width after edge margins (`width - 2*edge`)
        /// - All x-coordinates are relative to the container's allocation
        fn allocate(&self, widget: &Widget, width: i32, height: i32, baseline: i32) {
            let bar = widget.downcast_ref::<super::SectionedBar>().unwrap();
            let orientation = self.orientation.get();
            let spacing = self.spacing.get();
            let edge = self.edge_margin.get();
            let primary = if orientation == Orientation::Horizontal {
                width
            } else {
                height
            };
            let cross = if orientation == Orientation::Horizontal {
                height
            } else {
                width
            };
            let interior = (primary - 2 * edge).max(0);

            let left = bar.section("left").filter(|w| w.is_visible());
            let center = bar.section("center").filter(|w| w.is_visible());
            let right = bar.section("right").filter(|w| w.is_visible());

            // Helper to measure a widget
            fn measure_section(
                widget: Option<&Widget>,
                orientation: Orientation,
            ) -> Option<SectionSizes> {
                widget.map(|w| {
                    let (min, nat, _, _) = w.measure(orientation, -1);
                    SectionSizes { min, natural: nat }
                })
            }

            // If no center, do linear layout
            if center.is_none() {
                let alloc = compute_linear_allocation(
                    interior,
                    spacing,
                    measure_section(left.as_ref(), orientation),
                    measure_section(right.as_ref(), orientation),
                );

                // Record last allocation for snapshot/clipping
                self.last_left_x.set(edge + alloc.left_x);
                self.last_left_width.set(alloc.left_width);
                self.last_center_x.set(0);
                self.last_center_width.set(0);
                self.last_right_x.set(edge + alloc.right_x);
                self.last_right_width.set(alloc.right_width);

                if let Some(left_widget) = left {
                    allocate_child_at(
                        &left_widget,
                        edge + alloc.left_x,
                        alloc.left_width,
                        cross,
                        orientation,
                        baseline,
                    );
                }
                if let Some(right_widget) = right {
                    allocate_child_at(
                        &right_widget,
                        edge + alloc.right_x,
                        alloc.right_width,
                        cross,
                        orientation,
                        baseline,
                    );
                }

                // Fire post-allocate callback (used by bar blur).
                if let Some(cb) = self.on_allocate.borrow().as_ref() {
                    cb();
                }
                return;
            }

            let center = center.unwrap();

            // Measure all sections
            let left_sizes = measure_section(left.as_ref(), orientation);
            let center_sizes = {
                let (min, nat, _, _) = center.measure(orientation, -1);
                SectionSizes { min, natural: nat }
            };
            let right_sizes = measure_section(right.as_ref(), orientation);

            // Compute allocation using pure math function
            let alloc = compute_center_priority_allocation(
                interior,
                spacing,
                left_sizes,
                self.left_expand.get(),
                center_sizes,
                right_sizes,
                self.right_expand.get(),
            );

            tracing::debug!(
                "CenterPriorityLayout: interior={}, left_expand={}, left_budget={}, left_alloc_width={}, center_x={}, center_width={}",
                interior,
                self.left_expand.get(),
                (alloc.center_x - spacing).max(0),
                alloc.left_width,
                alloc.center_x,
                alloc.center_width
            );

            // Record last allocation for snapshot/clipping
            self.last_left_x.set(edge + alloc.left_x);
            self.last_left_width.set(alloc.left_width);
            self.last_center_x.set(edge + alloc.center_x);
            self.last_center_width.set(alloc.center_width);
            self.last_right_x.set(edge + alloc.right_x);
            self.last_right_width.set(alloc.right_width);

            // Apply allocations
            if let Some(left_widget) = left {
                allocate_child_at(
                    &left_widget,
                    edge + alloc.left_x,
                    alloc.left_width,
                    cross,
                    orientation,
                    baseline,
                );
            }

            allocate_child_at(
                &center,
                edge + alloc.center_x,
                alloc.center_width,
                cross,
                orientation,
                baseline,
            );

            if let Some(right_widget) = right {
                allocate_child_at(
                    &right_widget,
                    edge + alloc.right_x,
                    alloc.right_width,
                    cross,
                    orientation,
                    baseline,
                );
            }

            // Fire post-allocate callback (used by bar blur).
            if let Some(cb) = self.on_allocate.borrow().as_ref() {
                cb();
            }
        }

        fn create_layout_child(&self, widget: &Widget, for_child: &Widget) -> LayoutChild {
            self.parent_create_layout_child(widget, for_child)
        }
    }
}

glib::wrapper! {
    pub struct CenterPriorityLayout(ObjectSubclass<imp::CenterPriorityLayout>)
        @extends LayoutManager;
}

impl CenterPriorityLayout {
    pub fn new(
        orientation: Orientation,
        spacing: i32,
        edge_margin: i32,
        left_expand: bool,
        right_expand: bool,
    ) -> Self {
        let obj: Self = glib::Object::builder().build();
        obj.imp().orientation.set(orientation);
        obj.imp().spacing.set(spacing);
        obj.imp().edge_margin.set(edge_margin);
        obj.imp().left_expand.set(left_expand);
        obj.imp().right_expand.set(right_expand);
        obj
    }

    pub fn set_spacing(&self, spacing: i32) {
        self.imp().spacing.set(spacing);
    }

    pub fn set_edge_margin(&self, edge_margin: i32) {
        self.imp().edge_margin.set(edge_margin);
    }

    pub fn set_left_expand(&self, expand: bool) {
        self.imp().left_expand.set(expand);
    }

    pub fn set_right_expand(&self, expand: bool) {
        self.imp().right_expand.set(expand);
    }

    /// Set a callback to be fired after every layout allocation pass.
    ///
    /// Used by bar blur to recompute island regions when layout changes.
    pub fn set_on_allocate<F: Fn() + 'static>(&self, cb: F) {
        *self.imp().on_allocate.borrow_mut() = Some(Box::new(cb));
    }
}

impl Default for CenterPriorityLayout {
    fn default() -> Self {
        Self::new(Orientation::Horizontal, 8, 12, false, false)
    }
}

/// Allocate a child widget at a specific x position.
///
/// Uses a translation transform to position the child horizontally.
/// The child is given the full height of the container.
/// Baseline is set to -1 (none) to avoid text alignment issues with certain fonts.
fn allocate_child_at(
    child: &Widget,
    position: i32,
    primary: i32,
    cross: i32,
    orientation: Orientation,
    _baseline: i32,
) {
    let primary = primary.max(0);
    let cross = cross.max(0);
    let transform = if position != 0 {
        let transform = gtk4::gsk::Transform::new();
        let point = if orientation == Orientation::Horizontal {
            gtk4::graphene::Point::new(position as f32, 0.0)
        } else {
            gtk4::graphene::Point::new(0.0, position as f32)
        };
        Some(transform.translate(&point))
    } else {
        None
    };
    // Pass -1 for baseline to disable baseline alignment
    if orientation == Orientation::Horizontal {
        child.allocate(primary, cross, -1, transform);
    } else {
        child.allocate(cross, primary, -1, transform);
    }
}

mod bar_imp {
    use super::*;
    use std::cell::RefCell;

    use crate::styles::class;

    #[derive(Default)]
    pub struct SectionedBar {
        pub left: RefCell<Option<Widget>>,
        pub center: RefCell<Option<Widget>>,
        pub right: RefCell<Option<Widget>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SectionedBar {
        const NAME: &'static str = "VibepanelSectionedBar";
        type Type = super::SectionedBar;
        type ParentType = Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name(class::SECTIONED_BAR);
        }
    }

    impl ObjectImpl for SectionedBar {
        fn dispose(&self) {
            if let Some(w) = self.left.borrow_mut().take() {
                w.unparent();
            }
            if let Some(w) = self.center.borrow_mut().take() {
                w.unparent();
            }
            if let Some(w) = self.right.borrow_mut().take() {
                w.unparent();
            }
        }
    }

    impl WidgetImpl for SectionedBar {
        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            // Use default snapshot behavior - let GTK handle clipping
            self.parent_snapshot(snapshot);
        }
    }
}

glib::wrapper! {
    pub struct SectionedBar(ObjectSubclass<bar_imp::SectionedBar>)
        @extends Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl SectionedBar {
    pub fn new(
        orientation: Orientation,
        spacing: i32,
        edge_margin: i32,
        left_expand: bool,
        right_expand: bool,
    ) -> Self {
        let obj: Self = glib::Object::builder().build();
        let layout =
            CenterPriorityLayout::new(orientation, spacing, edge_margin, left_expand, right_expand);
        obj.set_layout_manager(Some(layout));
        obj
    }

    pub fn section(&self, name: &str) -> Option<Widget> {
        let imp = self.imp();
        match name {
            "left" => imp.left.borrow().clone(),
            "center" => imp.center.borrow().clone(),
            "right" => imp.right.borrow().clone(),
            _ => None,
        }
    }

    pub fn set_section(&self, name: &str, widget: Option<Widget>) {
        let imp = self.imp();
        let slot = match name {
            "left" => &imp.left,
            "center" => &imp.center,
            "right" => &imp.right,
            _ => return,
        };

        // Unparent old widget
        if let Some(old) = slot.borrow_mut().take() {
            old.unparent();
        }

        // Set and parent new widget
        if let Some(ref w) = widget {
            w.set_parent(self);
        }
        *slot.borrow_mut() = widget;

        self.queue_resize();
    }

    pub fn set_start_widget(&self, widget: Option<&impl IsA<Widget>>) {
        self.set_section("left", widget.map(|w| w.upcast_ref::<Widget>().clone()));
    }

    pub fn set_center_widget(&self, widget: Option<&impl IsA<Widget>>) {
        self.set_section("center", widget.map(|w| w.upcast_ref::<Widget>().clone()));
    }

    pub fn set_end_widget(&self, widget: Option<&impl IsA<Widget>>) {
        self.set_section("right", widget.map(|w| w.upcast_ref::<Widget>().clone()));
    }
}

impl Default for SectionedBar {
    fn default() -> Self {
        Self::new(Orientation::Horizontal, 8, 12, false, false)
    }
}

#[cfg(test)]
#[path = "sectioned_bar_tests.rs"]
mod tests;
