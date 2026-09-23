//! Per-output automatic visibility. Policy and timing are independent of GTK.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, EventControllerMotion, gdk, glib};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use vibepanel_core::Config;
use vibepanel_core::config::{BarPosition, BarVisibility};

use crate::popover_tracker::PopoverTracker;
use crate::sectioned_bar::{CenterPriorityLayout, SectionedBar};
use crate::services::background_effect::BackgroundEffectManager;
use crate::services::callbacks::CallbackId;
use crate::services::compositor::CompositorManager;
use crate::services::compositor::visibility::{HideDecision, Rect, VisibilitySubscription};
use crate::services::config_manager::ConfigManager;
use crate::widgets::layer_shell_popover::{AnimDirection, AnimState};

#[derive(Default)]
struct Policy {
    shown: bool,
    hide_since: Option<Instant>,
    edge_since: Option<Instant>,
    /// Last shown because of the pointer, a popup or a pin.
    held: bool,
    /// IPC hide; holds against intellihide until this output's decision changes.
    dismissed: Option<HideDecision>,
}

struct Inputs {
    suppressed: bool,
    interacting: bool,
    edge: bool,
    decision: HideDecision,
}

impl Policy {
    /// Hide now, skipping the hide delay.
    fn dismiss(&mut self, decision: HideDecision) {
        *self = Self {
            dismissed: Some(decision),
            ..Self::default()
        };
    }

    fn update(&mut self, now: Instant, settings: &Settings, input: Inputs) -> bool {
        if input.suppressed {
            *self = Self {
                dismissed: self.dismissed,
                ..Self::default()
            };
            return false;
        }
        if self.dismissed.is_some_and(|d| d != input.decision) {
            self.dismissed = None;
        }
        let edge_ready = if input.edge {
            now.duration_since(*self.edge_since.get_or_insert(now)) >= settings.reveal_delay
        } else {
            self.edge_since = None;
            false
        };
        let pointer = input.interacting || edge_ready;
        if settings.mode == BarVisibility::Always
            || pointer
            || (settings.mode == BarVisibility::Intellihide
                && input.decision == HideDecision::Show
                && self.dismissed.is_none())
        {
            self.shown = true;
            self.hide_since = None;
            self.held = pointer;
        } else if self.shown
            && (!self.held
                || now.duration_since(*self.hide_since.get_or_insert(now)) >= settings.hide_delay)
        {
            // Delays guard pointer interaction; scene-driven hides are immediate.
            self.shown = false;
            self.hide_since = None;
        }
        self.shown
    }
    fn next_deadline(&self, now: Instant, settings: &Settings) -> Option<Instant> {
        [
            self.edge_since.map(|since| since + settings.reveal_delay),
            self.hide_since.map(|since| since + settings.hide_delay),
        ]
        .into_iter()
        .flatten()
        .filter(|deadline| *deadline > now)
        .min()
    }
}

struct Settings {
    mode: BarVisibility,
    hide_delay: Duration,
    reveal_delay: Duration,
}

type InputRects = Vec<(i32, i32, i32, i32)>;

pub struct BarVisibilityController {
    window: ApplicationWindow,
    trigger: Option<ApplicationWindow>,
    bar: SectionedBar,
    content: gtk4::Widget,
    slide: crate::widgets::bar_slide::BarSlide,
    monitor: gdk::Monitor,
    output: String,
    position: BarPosition,
    islands: bool,
    settings: Settings,
    policy: RefCell<Policy>,
    shown: Cell<bool>,
    #[cfg(test)]
    evaluations: Cell<u64>,
    progress: Cell<f64>,
    hovered: Cell<bool>,
    edge_hovered: Cell<bool>,
    manual_hidden: Cell<bool>,
    pinned: Cell<bool>,
    monitor_suppressed: Cell<bool>,
    unknown: Cell<bool>,
    input_state: RefCell<Option<InputRects>>,
    island_apply: Option<Rc<dyn Fn()>>,
    subscription: Option<Rc<VisibilitySubscription>>,
    timer: RefCell<Option<glib::SourceId>>,
    update_source: RefCell<Option<glib::SourceId>>,
    popup_callback: Cell<Option<CallbackId>>,
    scene_callback: Cell<Option<CallbackId>>,
    allocation_callback: RefCell<Option<(CenterPriorityLayout, CallbackId)>>,
    monitor_handler: RefCell<Option<glib::SignalHandlerId>>,
    animation: RefCell<AnimState>,
    animation_tick: RefCell<Option<gtk4::TickCallbackId>>,
}

impl BarVisibilityController {
    #[cfg(test)]
    pub(crate) fn test_evaluations(&self) -> u64 {
        self.evaluations.get()
    }

    #[cfg(test)]
    pub(crate) fn test_progress(&self) -> f64 {
        self.progress.get()
    }

    #[cfg(test)]
    pub(crate) fn test_shown(&self) -> bool {
        self.shown.get()
    }

    #[cfg(test)]
    pub(crate) fn test_edge_hover(self: &Rc<Self>, hovered: bool) {
        self.edge_hovered.set(hovered);
        self.tick();
    }

    #[cfg(test)]
    pub(crate) fn test_footprint(&self) -> Vec<Rect> {
        self.footprint()
    }

    pub(crate) fn reveal_trigger(&self) -> Option<ApplicationWindow> {
        self.trigger.clone()
    }

    pub fn new(
        app: &Application,
        window: &ApplicationWindow,
        bar: &SectionedBar,
        monitor: &gdk::Monitor,
        output: &str,
        config: &Config,
        island_apply: Option<Rc<dyn Fn()>>,
    ) -> Rc<Self> {
        let mode = config.bar.visibility;
        let automatic = mode != BarVisibility::Always;
        let trigger = automatic.then(|| {
            let trigger = ApplicationWindow::builder()
                .application(app)
                .title("vibepanel reveal trigger")
                .decorated(false)
                // Let layer-shell stretch the tiny trigger across the full edge.
                .resizable(true)
                .build();
            trigger.init_layer_shell();
            trigger.set_namespace(Some("vibepanel-reveal-trigger"));
            trigger.set_layer(Layer::Top);
            trigger.set_monitor(Some(monitor));
            trigger.set_keyboard_mode(KeyboardMode::None);
            trigger.set_exclusive_zone(-1);
            let position = config.bar.position();
            trigger.set_anchor(
                Edge::Top,
                position == BarPosition::Top || position.is_vertical(),
            );
            trigger.set_anchor(
                Edge::Bottom,
                position == BarPosition::Bottom || position.is_vertical(),
            );
            trigger.set_anchor(
                Edge::Left,
                position == BarPosition::Left || position.is_horizontal(),
            );
            trigger.set_anchor(
                Edge::Right,
                position == BarPosition::Right || position.is_horizontal(),
            );
            trigger.set_default_size(
                if position.is_vertical() { 2 } else { -1 },
                if position.is_horizontal() { 2 } else { -1 },
            );
            // Opacity zero can prevent GTK from attaching its first buffer.
            // Keep the surface renderable and make its contents transparent.
            trigger.add_css_class(crate::styles::surface::LAYER_SHELL_CLICK_CATCHER);
            let content = transparent_buffer();
            content.set_size_request(
                if position.is_vertical() { 2 } else { 1 },
                if position.is_horizontal() { 2 } else { 1 },
            );
            trigger.set_child(Some(&content));
            trigger
        });
        let subscription = (mode == BarVisibility::Intellihide).then(|| {
            VisibilitySubscription::acquire(CompositorManager::global().visibility_reader())
        });
        let content = window
            .child()
            .expect("bar content installed before visibility controller");
        // Submit a cleared frame while preserving layout and reserved space.
        // Window opacity zero can leave the previous Wayland buffer visible.
        window.set_child(gtk4::Widget::NONE);
        let overlay = gtk4::Overlay::new();
        let slide = crate::widgets::bar_slide::BarSlide::new();
        slide.set_position(config.bar.position());
        slide.set_child(&content);
        slide.set_progress(0.0);
        overlay.set_child(Some(&slide));
        let clear = transparent_buffer();
        clear.set_can_target(false);
        overlay.add_overlay(&clear);
        window.set_child(Some(&overlay));
        window.add_css_class(crate::styles::class::BAR_AUTO_HIDDEN);
        let controller = Rc::new(Self {
            window: window.clone(),
            trigger,
            bar: bar.clone(),
            content,
            slide,
            monitor: monitor.clone(),
            output: output.to_string(),
            position: config.bar.position(),
            islands: config.bar.background_opacity == 0.0,
            settings: Settings {
                mode,
                hide_delay: Duration::from_millis(config.bar.hide_delay_ms.into()),
                reveal_delay: Duration::from_millis(config.bar.reveal_delay_ms.into()),
            },
            policy: RefCell::new(Policy::default()),
            shown: Cell::new(false),
            #[cfg(test)]
            evaluations: Cell::new(0),
            progress: Cell::new(0.0),
            hovered: Cell::new(false),
            edge_hovered: Cell::new(false),
            manual_hidden: Cell::new(false),
            pinned: Cell::new(false),
            monitor_suppressed: Cell::new(false),
            unknown: Cell::new(true),
            input_state: RefCell::new(None),
            island_apply,
            subscription,
            timer: RefCell::new(None),
            update_source: RefCell::new(None),
            popup_callback: Cell::new(None),
            scene_callback: Cell::new(None),
            allocation_callback: RefCell::new(None),
            monitor_handler: RefCell::new(None),
            animation: RefCell::new(AnimState::new_idle().with_bar_slide_curves()),
            animation_tick: RefCell::new(None),
        });
        if automatic {
            Self::track_pointer(&controller, window, false);
            if let Some(trigger) = &controller.trigger {
                Self::track_pointer(&controller, trigger, true);
            }
            let weak = Rc::downgrade(&controller);
            window.connect_map(move |_| {
                if let Some(controller) = weak.upgrade() {
                    *controller.input_state.borrow_mut() = None;
                    controller.apply_input();
                }
            });
            let weak = Rc::downgrade(&controller);
            controller
                .popup_callback
                .set(Some(PopoverTracker::global().connect_changed(move || {
                    if let Some(controller) = weak.upgrade() {
                        controller.queue_update();
                    }
                })));
            if let Some(subscription) = &controller.subscription {
                let weak = Rc::downgrade(&controller);
                controller
                    .scene_callback
                    .set(Some(subscription.connect_changed(move || {
                        if let Some(controller) = weak.upgrade() {
                            controller.queue_update();
                        }
                    })));
            }
            if let Some(layout) = bar.layout_manager().and_downcast::<CenterPriorityLayout>() {
                let weak = Rc::downgrade(&controller);
                let id = layout.connect_allocated(move || {
                    if let Some(controller) = weak.upgrade() {
                        controller.queue_update();
                    }
                });
                *controller.allocation_callback.borrow_mut() = Some((layout, id));
            }
            let weak = Rc::downgrade(&controller);
            *controller.monitor_handler.borrow_mut() =
                Some(monitor.connect_geometry_notify(move |_| {
                    if let Some(controller) = weak.upgrade() {
                        controller.queue_update();
                    }
                }));
        }
        controller.tick();
        controller
    }

    fn track_pointer(this: &Rc<Self>, window: &ApplicationWindow, edge: bool) {
        let motion = EventControllerMotion::new();
        motion.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let weak = Rc::downgrade(this);
        motion.connect_enter(move |_, _, _| {
            if let Some(this) = weak.upgrade() {
                if edge {
                    this.edge_hovered.set(true);
                } else {
                    this.hovered.set(true);
                }
                this.queue_update();
            }
        });
        let weak = Rc::downgrade(this);
        motion.connect_leave(move |_| {
            if let Some(this) = weak.upgrade() {
                if edge {
                    this.edge_hovered.set(false);
                } else {
                    this.hovered.set(false);
                }
                this.queue_update();
            }
        });
        window.add_controller(motion);
    }

    pub(crate) fn is_fully_revealed(&self) -> bool {
        self.shown.get() && self.progress.get() >= 1.0
    }

    pub fn is_shown(&self) -> bool {
        self.shown.get()
    }

    /// Always keeps manual suppression. Automatic modes pin on show; hide
    /// retracts immediately and holds against intellihide until the scene changes.
    pub fn set_ipc_shown(self: &Rc<Self>, shown: bool) {
        let automatic = self.settings.mode != BarVisibility::Always;
        self.manual_hidden.set(!shown && !automatic);
        self.pinned.set(shown && automatic);
        if !shown {
            PopoverTracker::global().dismiss_on_output(&self.output);
        }
        if automatic {
            let mut policy = self.policy.borrow_mut();
            if shown {
                policy.dismissed = None;
            } else {
                policy.dismiss(self.decision());
            }
        }
        self.tick();
    }

    pub fn set_monitor_suppressed(self: &Rc<Self>, suppressed: bool) {
        self.monitor_suppressed.set(suppressed);
        self.tick();
    }

    fn footprint(&self) -> Vec<Rect> {
        let (x, y) = match self.position {
            BarPosition::Top | BarPosition::Left => (0.0, 0.0),
            BarPosition::Bottom => (
                0.0,
                (self.monitor.geometry().height() - self.window.height()) as f64,
            ),
            BarPosition::Right => (
                (self.monitor.geometry().width() - self.window.width()) as f64,
                0.0,
            ),
        };
        if self.islands {
            crate::bar::collect_island_bounds(&self.bar, self.window.upcast_ref())
                .into_iter()
                .filter_map(|(a, b, w, h)| {
                    Rect::new(a as f64 + x, b as f64 + y, w as f64, h as f64)
                })
                .collect()
        } else {
            self.bar
                .compute_bounds(&self.window)
                .and_then(|b| {
                    Rect::new(
                        b.x() as f64 + x,
                        b.y() as f64 + y,
                        b.width() as f64,
                        b.height() as f64,
                    )
                })
                .into_iter()
                .collect()
        }
    }

    fn decision(&self) -> HideDecision {
        self.subscription
            .as_ref()
            .map_or(HideDecision::Unknown, |subscription| {
                subscription.decision(&self.output, &self.footprint())
            })
    }

    fn queue_update(self: &Rc<Self>) {
        if self.update_source.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        *self.update_source.borrow_mut() = Some(glib::idle_add_local_once(move || {
            if let Some(this) = weak.upgrade() {
                this.update_source.borrow_mut().take();
                this.tick();
            }
        }));
    }

    fn tick(self: &Rc<Self>) {
        #[cfg(test)]
        self.evaluations.set(self.evaluations.get() + 1);
        if let Some(timer) = self.timer.borrow_mut().take() {
            timer.remove();
        }
        let now = Instant::now();
        let suppressed = self.manual_hidden.get() || self.monitor_suppressed.get();
        let decision = self.decision();
        if self.subscription.is_some() {
            let unknown = decision == HideDecision::Unknown;
            if self.unknown.replace(unknown) != unknown {
                if unknown {
                    tracing::debug!(output = %self.output, "Intellihide state unavailable; using auto-hide until window state is known");
                } else {
                    tracing::debug!(output = %self.output, "Intellihide window state available");
                }
            }
        }
        let interacting = self.pinned.get()
            || self.hovered.get()
            || PopoverTracker::global().holds_output(&self.output)
            || has_open_native_popover(self.window.upcast_ref());
        let shown = self.policy.borrow_mut().update(
            now,
            &self.settings,
            Inputs {
                suppressed,
                interacting,
                edge: self.edge_hovered.get(),
                decision,
            },
        );
        let changed = self.shown.replace(shown) != shown;
        if suppressed {
            self.hovered.set(false);
            self.edge_hovered.set(false);
            self.snap_visual(false);
            // Hotplug suppression must preserve the exclusive zone.
            self.window.set_visible(!self.manual_hidden.get());
        } else {
            self.window.set_visible(true);
            if changed {
                self.animate_visual(shown);
            } else if self.animation.borrow().active
                && !ConfigManager::global().animations_enabled()
            {
                self.snap_visual(shown);
            }
        }
        if let Some(trigger) = &self.trigger {
            // Mango does not send enter to the revealed bar until the pointer
            // moves. Keep the hovered trigger mapped so stationary hover holds.
            if !suppressed && (!shown || self.edge_hovered.get()) {
                if !trigger.is_visible() {
                    trigger.present();
                }
            } else {
                trigger.set_visible(false);
            }
        }
        self.apply_input();
        if let Some(deadline) = self.policy.borrow().next_deadline(now, &self.settings) {
            let weak = Rc::downgrade(self);
            *self.timer.borrow_mut() = Some(glib::timeout_add_local_once(
                deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(1)),
                move || {
                    if let Some(this) = weak.upgrade() {
                        this.timer.borrow_mut().take();
                        this.tick();
                    }
                },
            ));
        }
    }

    fn snap_visual(&self, shown: bool) {
        if let Some(tick) = self.animation_tick.borrow_mut().take() {
            tick.remove();
        }
        self.animation.borrow_mut().active = false;
        self.apply_visual(if shown { 1.0 } else { 0.0 });
    }

    fn animate_visual(self: &Rc<Self>, shown: bool) {
        if self.settings.mode == BarVisibility::Always
            || !ConfigManager::global().animations_enabled()
        {
            self.snap_visual(shown);
            return;
        }
        let Some(clock) = self.window.frame_clock() else {
            self.snap_visual(shown);
            return;
        };
        let direction = if shown {
            AnimDirection::Opening
        } else {
            AnimDirection::Closing
        };
        self.animation
            .borrow_mut()
            .prepare(direction, 0, clock.frame_time(), self.progress.get());
        if self.animation_tick.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        let tick = self.window.add_tick_callback(move |_, clock| {
            let Some(this) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let (progress, complete) = {
                let animation = this.animation.borrow();
                if ConfigManager::global().animations_enabled() {
                    (
                        animation.current_progress(clock.frame_time()),
                        animation.is_complete(clock.frame_time()),
                    )
                } else {
                    (animation.target_progress, true)
                }
            };
            this.apply_visual(progress);
            if complete {
                this.animation.borrow_mut().active = false;
                this.animation_tick.borrow_mut().take();
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
        *self.animation_tick.borrow_mut() = Some(tick);
    }

    fn apply_visual(&self, progress: f64) {
        self.progress.set(progress);
        self.content.set_can_target(progress >= 1.0);
        self.slide.set_progress(progress);
        // Keep allocation callbacks from applying a full-size blur while the
        // content is in flight. Restore it once the bar settles fully open.
        let settled = progress >= 1.0;
        let was_settled = !self
            .window
            .has_css_class(crate::styles::class::BAR_AUTO_HIDDEN);
        if settled {
            self.window
                .remove_css_class(crate::styles::class::BAR_AUTO_HIDDEN);
        } else {
            self.window
                .add_css_class(crate::styles::class::BAR_AUTO_HIDDEN);
        }
        if was_settled != settled
            && let Some(apply) = &self.island_apply
        {
            apply();
        }
        if was_settled != settled
            && let Some(blur) = BackgroundEffectManager::global()
        {
            if !settled {
                blur.remove_blur_region(&self.window);
            } else if ConfigManager::global().blur_enabled() && !self.islands {
                blur.apply_bar_blur_region(&self.window, &self.bar);
            }
        }
        self.apply_input();
    }

    fn apply_input(&self) {
        if self.trigger.is_none() {
            return;
        }
        if let Some(surface) = self.window.surface() {
            // Island input stays island-shaped while sliding: full-surface input
            // would let a pointer in a gap register hover and reverse a hide.
            // BarSlide only translates the snapshot, so bounds are the rest pose.
            let rects = if !self.shown.get() && self.progress.get() <= 0.0 {
                Vec::new()
            } else if self.islands && !PopoverTracker::global().holds_output(&self.output) {
                crate::bar::collect_island_bounds(&self.bar, self.window.upcast_ref())
                    .into_iter()
                    .map(|(x, y, w, h)| match self.position {
                        BarPosition::Top => (x, 0, w, y + h),
                        BarPosition::Bottom => (x, y, w, surface.height() - y),
                        BarPosition::Left => (0, y, x + w, h),
                        BarPosition::Right => (x, y, surface.width() - x, h),
                    })
                    .collect()
            } else {
                vec![(0, 0, surface.width(), surface.height())]
            };
            if self.input_state.borrow().as_ref() == Some(&rects) {
                return;
            }
            let region = gtk4::cairo::Region::create();
            for &(x, y, w, h) in &rects {
                let _ = region.union_rectangle(&gtk4::cairo::RectangleInt::new(x, y, w, h));
            }
            surface.set_input_region(&region);
            // Popup state can change input without changing any visible pixels.
            self.window.queue_draw();
            *self.input_state.borrow_mut() = Some(rects);
        }
    }
}

fn transparent_buffer() -> gtk4::DrawingArea {
    let area = gtk4::DrawingArea::new();
    area.set_draw_func(|_, cr, _, _| {
        cr.set_operator(gtk4::cairo::Operator::Source);
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
        let _ = cr.paint();
    });
    area
}

fn has_open_native_popover(widget: &gtk4::Widget) -> bool {
    if widget.is::<gtk4::Popover>() && widget.is_visible() {
        return true;
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        if has_open_native_popover(&current) {
            return true;
        }
        child = current.next_sibling();
    }
    false
}

impl Drop for BarVisibilityController {
    fn drop(&mut self) {
        if let Some(source) = self.update_source.borrow_mut().take() {
            source.remove();
        }
        if let Some(id) = self.popup_callback.take() {
            PopoverTracker::global().disconnect_changed(id);
        }
        if let Some(id) = self.scene_callback.take()
            && let Some(subscription) = &self.subscription
        {
            subscription.disconnect_changed(id);
        }
        if let Some((layout, id)) = self.allocation_callback.borrow_mut().take() {
            layout.disconnect_allocated(id);
        }
        if let Some(id) = self.monitor_handler.borrow_mut().take() {
            self.monitor.disconnect(id);
        }
        if let Some(tick) = self.animation_tick.borrow_mut().take() {
            tick.remove();
        }
        if let Some(timer) = self.timer.borrow_mut().take() {
            timer.remove();
        }
        if let Some(trigger) = &self.trigger {
            trigger.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(mode: BarVisibility) -> Settings {
        Settings {
            mode,
            hide_delay: Duration::from_millis(300),
            reveal_delay: Duration::from_millis(100),
        }
    }
    fn input(decision: HideDecision) -> Inputs {
        Inputs {
            suppressed: false,
            interacting: false,
            edge: false,
            decision,
        }
    }

    #[test]
    fn scene_hides_immediately_but_pointer_release_waits_for_delay() {
        let now = Instant::now();
        let settings = settings(BarVisibility::Intellihide);
        let mut policy = Policy::default();
        assert!(policy.update(now, &settings, input(HideDecision::Show)));
        assert!(!policy.update(now, &settings, input(HideDecision::Hide)));
        let mut hover = input(HideDecision::Hide);
        hover.interacting = true;
        assert!(policy.update(now + Duration::from_millis(301), &settings, hover));
        assert!(policy.update(
            now + Duration::from_millis(500),
            &settings,
            input(HideDecision::Hide)
        ));
        assert!(!policy.update(
            now + Duration::from_millis(800),
            &settings,
            input(HideDecision::Hide)
        ));
    }

    #[test]
    fn edge_requires_continuous_dwell_and_manual_hide_wins() {
        let now = Instant::now();
        let settings = settings(BarVisibility::AutoHide);
        let mut policy = Policy::default();
        let edge = || Inputs {
            edge: true,
            ..input(HideDecision::Show)
        };
        assert!(!policy.update(now, &settings, edge()));
        assert!(!policy.update(
            now + Duration::from_millis(50),
            &settings,
            input(HideDecision::Show)
        ));
        assert!(!policy.update(now + Duration::from_millis(90), &settings, edge()));
        assert!(!policy.update(now + Duration::from_millis(100), &settings, edge()));
        assert!(policy.update(now + Duration::from_millis(190), &settings, edge()));
        assert!(!policy.update(
            now + Duration::from_millis(191),
            &settings,
            Inputs {
                suppressed: true,
                interacting: true,
                ..edge()
            }
        ));
    }

    #[test]
    fn unavailable_scene_falls_back_but_clear_scene_reveals_immediately() {
        let now = Instant::now();
        let settings = settings(BarVisibility::Intellihide);
        let mut policy = Policy::default();
        assert!(!policy.update(now, &settings, input(HideDecision::Unknown)));
        assert!(policy.update(now, &settings, input(HideDecision::Show)));
    }

    #[test]
    fn ipc_hide_retracts_immediately_unless_hovered() {
        let now = Instant::now();
        let settings = settings(BarVisibility::AutoHide);
        let mut policy = Policy::default();
        let hover = || Inputs {
            interacting: true,
            ..input(HideDecision::Unknown)
        };
        assert!(policy.update(now, &settings, hover()));
        policy.dismiss(HideDecision::Unknown);
        assert!(policy.update(now, &settings, hover()));
        policy.dismiss(HideDecision::Unknown);
        assert!(!policy.update(now, &settings, input(HideDecision::Unknown)));
    }

    #[test]
    fn intellihide_dismissal_holds_until_decision_changes() {
        let now = Instant::now();
        let settings = settings(BarVisibility::Intellihide);
        let mut policy = Policy::default();
        assert!(policy.update(now, &settings, input(HideDecision::Show)));
        policy.dismiss(HideDecision::Show);
        let later = now + Duration::from_secs(5);
        assert!(!policy.update(later, &settings, input(HideDecision::Show)));
        assert!(!policy.update(later, &settings, input(HideDecision::Hide)));
        assert!(policy.update(later, &settings, input(HideDecision::Show)));
    }
}
