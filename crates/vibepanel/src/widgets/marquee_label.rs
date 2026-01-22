//! Marquee label widget - scrolls text that doesn't fit in the available space.
//!
//! The MarqueeLabel displays text that automatically scrolls horizontally when
//! it exceeds the available width. When the text fits, it displays statically.
//!
//! Features:
//! - Automatic scroll detection based on text vs container width
//! - Smooth scrolling animation with configurable speed
//! - Seamless loop scrolling with configurable gap
//!
//! Implementation uses a custom GtkWidget subclass to properly control size
//! reporting, ensuring the widget respects max_width_chars for layout purposes
//! while still allowing the full text to scroll.

use gtk4::glib::{self, SourceId};
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{Label, Overflow, Widget};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use tracing::debug;

/// Default scroll speed in pixels per tick (60 FPS).
/// 0.35 = ~21 pixels per second, smooth and readable.
const DEFAULT_SCROLL_SPEED: f64 = 0.35;
/// Animation tick interval in milliseconds (~60 FPS).
const TICK_INTERVAL_MS: u32 = 16;
/// Gap between repeated text when scrolling (in pixels).
const SCROLL_GAP: f64 = 50.0;

/// Internal state shared between the widget and animation callback.
struct MarqueeState {
    /// Whether currently scrolling.
    scrolling: bool,
    /// Current scroll offset in pixels.
    offset: f64,
    /// Total scroll distance needed (text_width + gap).
    scroll_distance: f64,
    /// Width of the text content.
    text_width: f64,
    /// Width of the container.
    container_width: f64,
    /// Scroll speed in pixels per tick.
    scroll_speed: f64,
    /// Animation timer source ID.
    timer_id: Option<SourceId>,
    /// Current text to detect changes.
    current_text: String,
}

impl Default for MarqueeState {
    fn default() -> Self {
        Self {
            scrolling: false,
            offset: 0.0,
            scroll_distance: 0.0,
            text_width: 0.0,
            container_width: 0.0,
            scroll_speed: DEFAULT_SCROLL_SPEED,
            timer_id: None,
            current_text: String::new(),
        }
    }
}

mod imp {
    use super::*;

    /// Inner implementation for the MarqueeContainer widget.
    #[derive(Default)]
    pub struct MarqueeContainer {
        /// The primary label widget.
        pub(super) label: RefCell<Option<Label>>,
        /// Secondary label for seamless loop mode (shows text appearing from right).
        pub(super) label2: RefCell<Option<Label>>,
        /// Maximum width in characters (0 = no limit).
        pub(super) max_width_chars: Cell<i32>,
        /// Animation state.
        pub(super) state: Rc<RefCell<MarqueeState>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MarqueeContainer {
        const NAME: &'static str = "VibepanelMarqueeContainer";
        type Type = super::MarqueeContainer;
        type ParentType = Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("marquee-container");
        }
    }

    impl ObjectImpl for MarqueeContainer {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().set_overflow(Overflow::Hidden);
            // Default max_width_chars is 0, meaning expand to fill available space
            self.obj().set_hexpand(true);
        }

        fn dispose(&self) {
            // Stop any running animation
            if let Some(id) = self.state.borrow_mut().timer_id.take() {
                id.remove();
            }
            // Unparent the labels
            if let Some(label) = self.label.borrow_mut().take() {
                label.unparent();
            }
            if let Some(label2) = self.label2.borrow_mut().take() {
                label2.unparent();
            }
        }
    }

    impl WidgetImpl for MarqueeContainer {
        fn measure(&self, orientation: gtk4::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let label = self.label.borrow();
            let Some(label) = label.as_ref() else {
                return (0, 0, -1, -1);
            };

            let (min, nat, min_base, nat_base) = label.measure(orientation, for_size);

            if orientation == gtk4::Orientation::Horizontal {
                let max_chars = self.max_width_chars.get();
                if max_chars > 0 {
                    // Calculate max width in pixels using pango metrics
                    let layout = label.layout();
                    let context = layout.context();
                    let font_desc = context.font_description().unwrap_or_default();
                    let metrics = context.metrics(Some(&font_desc), None);
                    let char_width_pango = metrics.approximate_char_width();
                    let char_width_px = char_width_pango as f64 / gtk4::pango::SCALE as f64;
                    let max_width = (max_chars as f64 * char_width_px).ceil() as i32;

                    // Constrain natural width to max_width, but keep minimum small
                    let constrained_nat = nat.min(max_width);
                    return (
                        min.min(constrained_nat),
                        constrained_nat,
                        min_base,
                        nat_base,
                    );
                } else {
                    // max_chars == 0: Report small minimum but full natural width.
                    // This allows the widget to shrink if needed (min=0) but requests
                    // its full text width as natural size. Combined with hexpand=true,
                    // the widget will expand to fill available space.
                    return (0, nat, min_base, nat_base);
                }
            }

            (min, nat, min_base, nat_base)
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            let label = self.label.borrow();
            let Some(label) = label.as_ref() else {
                return;
            };

            // Skip allocation if dimensions are invalid
            if width <= 0 || height <= 0 {
                return;
            }

            // Get the label's natural width (full text width)
            let (_, nat_req) = label.preferred_size();
            let text_width = nat_req.width();

            let state = self.state.borrow();
            let offset = state.offset as i32;

            // Position primary label (negative offset scrolls left)
            let transform = gtk4::gsk::Transform::new()
                .translate(&gtk4::graphene::Point::new(-offset as f32, 0.0));
            label.allocate(text_width.max(width), height, baseline, Some(transform));

            // Position secondary label for seamless loop
            // Second label appears at: text_width + gap - offset
            // When offset = 0, label2 is off-screen to the right
            // As offset increases, label2 slides in from the right
            if let Some(label2) = self.label2.borrow().as_ref() {
                let label2_x = text_width as f32 + SCROLL_GAP as f32 - offset as f32;
                let transform2 = gtk4::gsk::Transform::new()
                    .translate(&gtk4::graphene::Point::new(label2_x, 0.0));
                label2.allocate(text_width.max(width), height, baseline, Some(transform2));
            }
        }
    }
}

glib::wrapper! {
    /// A custom container widget that clips its content and controls size reporting.
    pub struct MarqueeContainer(ObjectSubclass<imp::MarqueeContainer>)
        @extends Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl MarqueeContainer {
    /// Create a new marquee container.
    pub fn new() -> Self {
        glib::Object::builder().build()
    }

    /// Set the child label (and create secondary label for loop mode).
    pub fn set_label(&self, label: &Label, label2: &Label) {
        let imp = self.imp();

        // Unparent old labels if any
        if let Some(old) = imp.label.borrow_mut().take() {
            old.unparent();
        }
        if let Some(old) = imp.label2.borrow_mut().take() {
            old.unparent();
        }

        // Parent new labels
        label.set_parent(self);
        label2.set_parent(self);
        *imp.label.borrow_mut() = Some(label.clone());
        *imp.label2.borrow_mut() = Some(label2.clone());
    }

    /// Get the child label.
    pub fn label(&self) -> Option<Label> {
        self.imp().label.borrow().clone()
    }

    /// Set maximum width in characters.
    /// When chars > 0, constrains widget to that width.
    /// When chars == 0, widget expands to fill available space.
    pub fn set_max_width_chars(&self, chars: i32) {
        self.imp().max_width_chars.set(chars);
        // When max_chars == 0, expand to fill available space
        // When max_chars > 0, constrain to that width (no expansion)
        self.set_hexpand(chars == 0);
        self.queue_resize();
    }

    /// Get the animation state.
    fn state(&self) -> Rc<RefCell<MarqueeState>> {
        self.imp().state.clone()
    }
}

impl Default for MarqueeContainer {
    fn default() -> Self {
        Self::new()
    }
}

/// A label that scrolls horizontally when text doesn't fit.
pub struct MarqueeLabel {
    /// Custom container widget that controls sizing.
    container: MarqueeContainer,
    /// The primary label displaying the text.
    label: Label,
    /// Secondary label for seamless loop scrolling.
    label2: Label,
}

impl MarqueeLabel {
    /// Create a new marquee label with default settings.
    pub fn new() -> Self {
        let container = MarqueeContainer::new();

        // Helper to configure a label
        let make_label = || {
            let label = Label::new(None);
            label.add_css_class("marquee-label");
            label.set_wrap(false);
            label.set_ellipsize(gtk4::pango::EllipsizeMode::None);
            label.set_single_line_mode(true);
            label.set_width_chars(1);
            label.set_hexpand(false);
            label.set_xalign(0.0);
            label
        };

        let label = make_label();
        let label2 = make_label();

        container.set_label(&label, &label2);

        Self {
            container,
            label,
            label2,
        }
    }

    /// Get the root widget for embedding.
    pub fn widget(&self) -> &MarqueeContainer {
        &self.container
    }

    /// Get the inner label widget (for adding CSS classes).
    pub fn label(&self) -> &Label {
        &self.label
    }

    /// Set the maximum width in characters.
    pub fn set_max_width_chars(&self, chars: i32) {
        self.container.set_max_width_chars(chars);
    }

    /// Set the text to display.
    pub fn set_text(&self, text: &str) {
        {
            let state = self.container.state();
            let s = state.borrow();
            if s.current_text == text {
                return;
            }
        }

        // Set text on both labels (label2 is used for seamless loop)
        self.label.set_text(text);
        self.label2.set_text(text);

        {
            let state = self.container.state();
            let mut s = state.borrow_mut();
            s.current_text = text.to_string();
        }

        self.reset_and_check_scroll();
    }

    /// Set visibility of the marquee label.
    pub fn set_visible(&self, visible: bool) {
        // Only act if visibility actually changes
        if self.container.is_visible() == visible {
            return;
        }

        self.container.set_visible(visible);
        if !visible {
            self.stop_animation();
        } else {
            self.reset_and_check_scroll();
        }
    }

    /// Reset scroll and check if scrolling is needed.
    fn reset_and_check_scroll(&self) {
        self.stop_animation();

        // Reset offset
        {
            let state = self.container.state();
            let mut s = state.borrow_mut();
            s.offset = 0.0;
        }
        if self.container.is_mapped() {
            self.container.queue_allocate();
        }

        // Schedule check after layout
        let state = self.container.state();
        let label = self.label.clone();
        let container = self.container.clone();

        glib::idle_add_local_once(move || {
            check_and_start_scroll(&state, &label, &container);
        });
    }

    /// Stop the animation timer.
    fn stop_animation(&self) {
        let state = self.container.state();
        let mut s = state.borrow_mut();
        if let Some(id) = s.timer_id.take() {
            id.remove();
        }
        s.scrolling = false;
        s.offset = 0.0;
    }
}

impl Default for MarqueeLabel {
    fn default() -> Self {
        Self::new()
    }
}

/// Check if scrolling is needed and start animation if so.
fn check_and_start_scroll(
    state: &Rc<RefCell<MarqueeState>>,
    label: &Label,
    container: &MarqueeContainer,
) {
    let container_width = container.width() as f64;
    if container_width <= 0.0 {
        // Not yet allocated - only retry if widget is still in the visible hierarchy.
        // This prevents infinite retries if the widget is hidden or removed.
        if container.is_mapped() {
            let state = state.clone();
            let label = label.clone();
            let container = container.clone();
            glib::idle_add_local_once(move || {
                check_and_start_scroll(&state, &label, &container);
            });
        }
        return;
    }

    // Get text width from label's natural size
    let (_, nat_req) = label.preferred_size();
    let text_width = nat_req.width() as f64;

    debug!(
        "Marquee check: container_width={}, text_width={}",
        container_width, text_width
    );

    let mut s = state.borrow_mut();
    s.container_width = container_width;
    s.text_width = text_width;

    let needs_scroll = text_width > container_width;

    if needs_scroll {
        // Scroll full text width + gap for seamless loop
        s.scroll_distance = text_width + SCROLL_GAP;
        s.scrolling = true;
        s.offset = 0.0;

        debug!(
            "Marquee: text_width={}, container_width={}, scroll_distance={}",
            text_width, container_width, s.scroll_distance
        );

        if s.timer_id.is_none() {
            drop(s);
            start_animation(state, container);
        }
    } else {
        s.scrolling = false;
        s.offset = 0.0;

        debug!(
            "Marquee: text_width={}, container_width={}, needs_scroll=false",
            text_width, container_width
        );
    }
}

/// Start the animation timer.
fn start_animation(state: &Rc<RefCell<MarqueeState>>, container: &MarqueeContainer) {
    let state_for_closure = state.clone();
    let container = container.clone();

    let id = glib::timeout_add_local(
        std::time::Duration::from_millis(TICK_INTERVAL_MS as u64),
        move || {
            let mut s = state_for_closure.borrow_mut();

            if !s.scrolling {
                s.timer_id = None;
                return glib::ControlFlow::Break;
            }

            s.offset += s.scroll_speed;

            // Seamless loop - reset when we've scrolled the full distance
            if s.offset >= s.scroll_distance {
                s.offset = 0.0;
                debug!("Marquee: loop reset");
            }

            if container.is_mapped() {
                container.queue_allocate();
            }

            glib::ControlFlow::Continue
        },
    );

    state.borrow_mut().timer_id = Some(id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        const _: () = assert!(DEFAULT_SCROLL_SPEED > 0.0);
        const _: () = assert!(TICK_INTERVAL_MS > 0);
        const _: () = assert!(SCROLL_GAP > 0.0);
    }

    #[test]
    fn test_marquee_state_default() {
        let state = MarqueeState::default();
        assert!(!state.scrolling);
        assert_eq!(state.offset, 0.0);
    }
}
