//! Layer shell popover infrastructure.
//!
//! - **Helper functions**: positioning, click-catcher, and focus utilities for
//!   layer-shell surfaces.
//! - **[`SurfaceAnimation`]**: open/close animation shared by widget popovers,
//!   Quick Settings, and dialogs.
//! - **[`BarPopoverChrome`]**: click-catcher and deferred keyboard navigation
//!   for bar-anchored popovers.
//! - **[`LayerShellPopover`]**: complete popover for widget menus.

use gtk4::gdk::{self, Monitor};
use gtk4::glib::{self, ControlFlow, Propagation};
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box as GtkBox, EventControllerKey, GestureClick, Orientation,
};

/// Whether a key is a keyboard navigation key (Tab, arrows, Home, End).
/// Used by the deferred keyboard nav controller to gate activation.
fn is_keynav_key(keyval: gdk::Key) -> bool {
    matches!(
        keyval,
        gdk::Key::Tab
            | gdk::Key::ISO_Left_Tab
            | gdk::Key::Up
            | gdk::Key::Down
            | gdk::Key::Left
            | gdk::Key::Right
            | gdk::Key::Home
            | gdk::Key::End
    )
}
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use vibepanel_core::config::BarPosition;

use super::scale_box::ScaleBox;
use crate::services::background_effect::{BackgroundEffectManager, sync_blur};
use crate::services::compositor::CompositorManager;
use crate::services::config_manager::ConfigManager;
use crate::services::surfaces::{SHADOW_MARGIN, SurfaceStyleManager};
use crate::styles::{class, surface};

type AnchorMonitorCallback = Rc<dyn Fn(Option<Monitor>)>;

/// Minimum margin from screen edge for popovers.
const POPOVER_MIN_EDGE_MARGIN: i32 = 4;

/// Estimated popover width when actual width not yet available.
const POPOVER_DEFAULT_WIDTH_ESTIMATE: i32 = 320;

const POPOVER_MIN_VALID_WIDTH: i32 = 20;
const POPOVER_MIN_VALID_HEIGHT: i32 = 20;
const POPOVER_DEFAULT_HEIGHT_ESTIMATE: i32 = 360;

/// Monitor-local widget center used to place bar-adjacent popovers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PopoverAnchor {
    pub x: i32,
    pub y: i32,
}

/// Animation duration as f64 milliseconds for tick-callback math.
const ANIM_DURATION_MS: f64 = super::css::POPOVER_ANIMATION_MS as f64;

/// Starting scale for popover open/close animation.
/// ScaleBox renders this as a true (quantized) center scale transform.
const ANIM_SCALE_FROM: f64 = 0.94;

/// Close progress at which a popover counts as fully hidden.
///
/// Compositor-side layer blur (e.g. mango `blur_layer`) is masked by buffer
/// alpha, not scaled by it, so it stays at full strength over near-invisible
/// content. Unmapping once content is imperceptible removes that lingering blur.
const CLOSE_CUTOFF: f64 = 0.05;

/// Direction of the popover animation.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum AnimDirection {
    Opening,
    Closing,
}

/// Shared animation state, passed to the tick callback via `Rc<RefCell<_>>`.
///
/// Progress ranges from fully hidden (0.0) to fully visible (1.0).
pub(crate) struct AnimState {
    /// Use cubic ease-out on reveal and quintic ease-in on hide for bar slides.
    bar_slide_curves: bool,
    /// Current direction of animation.
    pub(crate) direction: AnimDirection,
    /// Frame-clock time (microseconds) when this animation segment started.
    pub(crate) start_time_us: i64,
    /// Progress value at the start of this segment (for mid-flight reversal).
    pub(crate) start_progress: f64,
    /// Target progress (1.0 for opening, 0.0 for closing).
    pub(crate) target_progress: f64,
    /// Whether a tick callback is currently driving this state.
    pub(crate) active: bool,
    /// Generation counter that the current tick callback was started with.
    /// Used to detect when an active tick has a stale generation and needs
    /// to be replaced by a new one.
    pub(crate) tick_generation: u32,
}

impl AnimState {
    pub(crate) fn new_idle() -> Self {
        Self {
            bar_slide_curves: false,
            direction: AnimDirection::Opening,
            start_time_us: 0,
            start_progress: 0.0,
            target_progress: 0.0,
            active: false,
            tick_generation: 0,
        }
    }

    pub(crate) fn with_bar_slide_curves(mut self) -> Self {
        self.bar_slide_curves = true;
        self
    }

    /// Compute the current eased progress given the frame clock time.
    pub(crate) fn current_progress(&self, now_us: i64) -> f64 {
        let elapsed_ms = (now_us - self.start_time_us) as f64 / 1000.0;
        let distance = (self.target_progress - self.start_progress).abs();
        if distance < f64::EPSILON {
            return self.target_progress;
        }
        // Duration is proportional to remaining distance — a half-done
        // animation that reverses takes half the time.
        let segment_duration_ms = ANIM_DURATION_MS * distance;
        let t = (elapsed_ms / segment_duration_ms).clamp(0.0, 1.0);
        let eased = if self.bar_slide_curves {
            match self.direction {
                // Leave enough travel near the end to make the slowdown visible.
                AnimDirection::Opening => 1.0 - (1.0 - t).powi(3),
                AnimDirection::Closing => t.powi(5),
            }
        } else {
            // Popovers retain their snappy quintic ease-out in both directions.
            1.0 - (1.0 - t).powi(5)
        };
        self.start_progress + (self.target_progress - self.start_progress) * eased
    }

    /// Whether the animation has reached its target.
    pub(crate) fn is_complete(&self, now_us: i64) -> bool {
        let elapsed_ms = (now_us - self.start_time_us) as f64 / 1000.0;
        let distance = (self.target_progress - self.start_progress).abs();
        if distance < f64::EPSILON {
            return true;
        }
        let segment_duration_ms = ANIM_DURATION_MS * distance;
        elapsed_ms >= segment_duration_ms
    }

    /// Like [`is_complete`](Self::is_complete), but a close ends once progress
    /// drops to [`CLOSE_CUTOFF`].
    fn popover_is_complete(&self, now_us: i64) -> bool {
        self.is_complete(now_us)
            || (self.direction == AnimDirection::Closing
                && self.current_progress(now_us) <= CLOSE_CUTOFF)
    }

    /// Prepare an animation segment and determine if a new tick callback is needed.
    ///
    /// Captures the current progress (for mid-flight reversal), updates all state
    /// fields, and returns `true` if a new tick callback must be registered. Returns
    /// `false` if the existing tick callback will pick up the new direction.
    ///
    /// `current_opacity` is the shell's current opacity, used as the starting
    /// progress when no animation is in flight.
    pub(crate) fn prepare(
        &mut self,
        direction: AnimDirection,
        generation: u32,
        start_time_us: i64,
        current_opacity: f64,
    ) -> bool {
        let target = match direction {
            AnimDirection::Opening => 1.0,
            AnimDirection::Closing => 0.0,
        };

        let start_progress = if self.active {
            self.current_progress(start_time_us)
        } else {
            current_opacity
        };

        let was_active = self.active;
        let tick_is_current = was_active && self.tick_generation == generation;
        self.direction = direction;
        self.start_time_us = start_time_us;
        self.start_progress = start_progress;
        self.target_progress = target;
        self.active = true;
        self.tick_generation = generation;
        // Need a new tick callback if none is running, or the running one
        // has a stale generation (it will self-cancel).
        !tick_is_current
    }
}

/// Apply the animation fade to the widget matching `direction`.
///
/// GTK-native blur (GTK >= 4.23.3) is derived from the render tree: an
/// opacity node *above* the `backdrop-filter` widget isolates its backdrop
/// and drops the blur region, while opacity on the `backdrop-filter` widget
/// itself sits inside its copy/paste pair and keeps it. So:
///
/// - **Opening** fades the ScaleBox child (the blurred surface), keeping the
///   compositor blur at full strength from the first frame instead of popping
///   in when the fade completes.
/// - **Closing** fades the ScaleBox itself, which drops the blur at fade start
///   so it doesn't linger behind near-invisible content.
///
/// The other widget is reset to 1.0, so the visible result is identical and
/// mid-flight reversals stay seamless. Without a child the shell carries the
/// fade either way.
fn set_anim_fade(shell: &ScaleBox, opacity: f64, direction: AnimDirection) {
    match (direction, shell.child()) {
        (AnimDirection::Opening, Some(child)) => {
            shell.set_opacity(1.0);
            child.set_opacity(opacity);
        }
        (_, child) => {
            if let Some(child) = child {
                child.set_opacity(1.0);
            }
            shell.set_opacity(opacity);
        }
    }
}

/// Effective fade across the shell and its child (see [`set_anim_fade`]).
fn anim_fade(shell: &ScaleBox) -> f64 {
    shell.opacity() * shell.child().map_or(1.0, |child| child.opacity())
}

fn snap_anim_shell(shell: &ScaleBox, opacity: f64, scale: f64) {
    // Fully open or fully hidden: the shell carries the (trivial) fade.
    set_anim_fade(shell, opacity, AnimDirection::Closing);
    shell.set_scale(scale);
}

/// Open/close animation for a layer-shell surface whose content sits in a
/// [`ScaleBox`].
///
/// Owns the shell, animation state, and generation counter. Bumping the
/// generation cancels stale tick and idle callbacks; reusing the current one
/// while a close is in flight reverses it smoothly (see [`Self::is_closing`]).
pub(crate) struct SurfaceAnimation {
    shell: ScaleBox,
    state: Rc<RefCell<AnimState>>,
    generation: Rc<Cell<u32>>,
}

impl SurfaceAnimation {
    pub(crate) fn new() -> Self {
        let shell = ScaleBox::new();
        snap_anim_shell(&shell, 0.0, ANIM_SCALE_FROM);
        Self {
            shell,
            state: Rc::new(RefCell::new(AnimState::new_idle())),
            generation: Rc::new(Cell::new(0)),
        }
    }

    pub(crate) fn shell(&self) -> &ScaleBox {
        &self.shell
    }

    pub(crate) fn generation(&self) -> u32 {
        self.generation.get()
    }

    /// Invalidate pending callbacks and return the new generation.
    pub(crate) fn next_generation(&self) -> u32 {
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        generation
    }

    pub(crate) fn is_active(&self) -> bool {
        self.state.borrow().active
    }

    /// Whether a close is in flight. Callers reopening in this state should
    /// keep the current generation and call [`Self::run`] with `Opening`.
    ///
    /// An unmapped shell (e.g. its output went away mid-fade) has no frame
    /// clock, so its close can never finish and does not count.
    pub(crate) fn is_closing(&self) -> bool {
        let state = self.state.borrow();
        state.active && state.direction == AnimDirection::Closing && self.shell.is_mapped()
    }

    /// Whether reopening on `monitor` can reverse the close in flight. A move
    /// to another output needs a fresh open; `None` keeps the current output.
    pub(crate) fn can_reverse_on(
        &self,
        window: &ApplicationWindow,
        monitor: Option<&Monitor>,
    ) -> bool {
        self.is_closing() && monitor.is_none_or(|m| window.monitor().as_ref() == Some(m))
    }

    /// Put the shell in its hidden start state, ending any running segment.
    pub(crate) fn reset_hidden(&self) {
        self.state.borrow_mut().active = false;
        snap_anim_shell(&self.shell, 0.0, ANIM_SCALE_FROM);
    }

    /// Show the shell at full size, ending any running segment.
    pub(crate) fn snap_open(&self) {
        self.state.borrow_mut().active = false;
        snap_anim_shell(&self.shell, 1.0, 1.0);
    }

    /// Reset the shell, map the window at opacity 0, then on idle (if
    /// `generation` is still current) run `on_mapped` and reveal it. Hides the
    /// first frame while the compositor sizes the surface.
    pub(crate) fn present_hidden_then(
        &self,
        window: &ApplicationWindow,
        generation: u32,
        on_mapped: impl FnOnce() + 'static,
    ) {
        self.reset_hidden();
        window.set_opacity(0.0);
        window.set_visible(true);
        window.present();
        let window_weak = window.downgrade();
        let current = Rc::clone(&self.generation);
        glib::idle_add_local_once(move || {
            if current.get() != generation {
                return;
            }
            let Some(window) = window_weak.upgrade() else {
                return;
            };
            on_mapped();
            window.set_opacity(1.0);
            CompositorManager::global().refresh_pointer_focus();
        });
    }

    /// Animate towards `direction`, or snap when animations are disabled.
    ///
    /// Opening fades the shell's child so blur is present from the first frame;
    /// closing fades the shell and removes blur first, since compositor blur
    /// does not fade with the content (see [`set_anim_fade`]). When a close
    /// completes, the window is hidden and `on_hidden` runs. A close reversed
    /// with the same generation keeps its tick, which then finishes the open
    /// instead.
    pub(crate) fn run(
        &self,
        direction: AnimDirection,
        generation: u32,
        window: &ApplicationWindow,
        blur_content: &gtk4::Widget,
        on_hidden: impl Fn() + 'static,
    ) {
        // Nothing to close, and a tick on an unmapped widget would never run.
        if direction == AnimDirection::Closing && !window.is_visible() {
            return;
        }
        if !ConfigManager::global().animations_enabled() {
            match direction {
                AnimDirection::Opening => self.snap_open(),
                AnimDirection::Closing => {
                    self.reset_hidden();
                    window.set_visible(false);
                    on_hidden();
                }
            }
            return;
        }

        if direction == AnimDirection::Closing {
            // An open may still be waiting for its idle pass with window
            // opacity at 0; the fade must be visible.
            window.set_opacity(1.0);
            if let Some(blur) = BackgroundEffectManager::global() {
                blur.remove_blur_region(window);
            }
        }

        let start_time_us = self
            .shell
            .frame_clock()
            .map(|fc| fc.frame_time())
            .unwrap_or(0);
        let need_tick = self.state.borrow_mut().prepare(
            direction,
            generation,
            start_time_us,
            anim_fade(&self.shell),
        );
        // A running tick with this generation picks up the new direction.
        if !need_tick {
            return;
        }

        let state = Rc::clone(&self.state);
        let current = Rc::clone(&self.generation);
        let window_weak = window.downgrade();
        let blur_weak = blur_content.downgrade();
        self.shell.add_tick_callback(move |shell, frame_clock| {
            // A newer cycle owns `active`; just stop.
            if current.get() != generation {
                return ControlFlow::Break;
            }

            let now_us = frame_clock.frame_time();
            let (progress, complete, direction) = {
                let state = state.borrow();
                if !state.active {
                    return ControlFlow::Break;
                }
                (
                    state.current_progress(now_us),
                    state.popover_is_complete(now_us),
                    state.direction,
                )
            };

            set_anim_fade(shell, progress, direction);
            shell.set_scale(ANIM_SCALE_FROM + (1.0 - ANIM_SCALE_FROM) * progress);

            if direction == AnimDirection::Opening
                && ConfigManager::global().blur_enabled()
                && let Some(blur) = BackgroundEffectManager::global()
                && let Some(window) = window_weak.upgrade()
                && let Some(content) = blur_weak.upgrade()
            {
                // Match the blur to the quantized scale that ScaleBox renders.
                blur.apply_open_animation_blur(&window, &content, shell.scale(), complete);
            }

            if !complete {
                return ControlFlow::Continue;
            }
            state.borrow_mut().active = false;
            if direction == AnimDirection::Opening {
                snap_anim_shell(shell, 1.0, 1.0);
            } else {
                snap_anim_shell(shell, 0.0, ANIM_SCALE_FROM);
                if let Some(window) = window_weak.upgrade() {
                    window.set_visible(false);
                }
                on_hidden();
            }
            ControlFlow::Break
        });
    }
}

/// Click-catcher and deferred keyboard navigation shared by bar popovers.
///
/// The window's child carries `.vp-no-focus`; focus rings stay hidden until
/// the first keynav key (Tab, arrows, Home, End).
#[derive(Default)]
pub(crate) struct BarPopoverChrome {
    catcher: RefCell<Option<ApplicationWindow>>,
    keynav: Rc<RefCell<Option<EventControllerKey>>>,
}

impl BarPopoverChrome {
    #[cfg(test)]
    pub(crate) fn catcher(&self) -> Option<ApplicationWindow> {
        self.catcher.borrow().clone()
    }

    /// Show the click-catcher (created lazily) and grab the keyboard.
    /// `on_dismiss` is only used when the catcher is first created.
    pub(crate) fn open(
        &self,
        window: &ApplicationWindow,
        monitor: Option<&Monitor>,
        on_dismiss: impl Fn() + Clone + 'static,
    ) {
        let catcher = self
            .catcher
            .borrow_mut()
            .get_or_insert_with(|| {
                let app = window
                    .application()
                    .expect("popover window must have an application");
                create_click_catcher(&app, calculate_bar_exclusive_zone(), on_dismiss)
            })
            .clone();
        if let Some(monitor) = monitor {
            catcher.set_monitor(Some(monitor));
        }
        catcher.set_margin(popover_bar_edge(), calculate_bar_exclusive_zone());
        catcher.set_visible(true);
        window.set_keyboard_mode(popover_keyboard_mode());
    }

    /// Hide the catcher, release the keyboard and the height freeze, and
    /// restore focus suppression so the next open starts without focus rings.
    pub(crate) fn close(&self, window: &ApplicationWindow) {
        GtkWindowExt::set_focus_visible(window, false);
        if let Some(child) = window.child() {
            child.add_css_class(surface::NO_FOCUS);
        }
        if let Some(controller) = self.keynav.borrow_mut().take() {
            window.remove_controller(&controller);
        }
        if let Some(ref catcher) = *self.catcher.borrow() {
            catcher.set_visible(false);
        }
        clear_surface_height_freeze(window);
        window.set_keyboard_mode(KeyboardMode::None);
    }

    /// Clear auto-focus from `present()` and wait for a keynav key before
    /// showing focus rings.
    pub(crate) fn prepare_keynav(&self, window: &ApplicationWindow) {
        GtkWindowExt::set_focus(window, None::<&gtk4::Widget>);
        if let Some(old) = self.keynav.borrow_mut().take() {
            window.remove_controller(&old);
        }

        let controller = EventControllerKey::new();
        let slot = Rc::downgrade(&self.keynav);
        let window_weak = window.downgrade();
        controller.connect_key_pressed(move |ctrl, keyval, _, _| {
            if !is_keynav_key(keyval) {
                return Propagation::Proceed;
            }
            let Some(window) = window_weak.upgrade() else {
                return Propagation::Proceed;
            };
            GtkWindowExt::set_focus_visible(&window, true);
            if let Some(child) = window.child() {
                child.remove_css_class(surface::NO_FOCUS);
            }
            window.remove_controller(ctrl);
            if let Some(slot) = slot.upgrade() {
                slot.borrow_mut().take();
            }
            if keyval == gdk::Key::Tab || keyval == gdk::Key::ISO_Left_Tab {
                // GTK's own Tab keynav sets :focus-visible correctly.
                Propagation::Proceed
            } else {
                // Arrows/Home/End: land on the first widget like Tab would.
                window.child_focus(gtk4::DirectionType::TabForward);
                Propagation::Stop
            }
        });
        window.add_controller(controller.clone());
        *self.keynav.borrow_mut() = Some(controller);
    }

    /// Close the catcher window (for `Drop`).
    pub(crate) fn destroy(&self) {
        if let Some(catcher) = self.catcher.borrow_mut().take() {
            catcher.close();
        }
    }
}

fn measured_popover_size(widget: &gtk4::Widget) -> Option<(i32, i32)> {
    let (_, natural_width, _, _) = widget.measure(Orientation::Horizontal, -1);
    if natural_width <= POPOVER_MIN_VALID_WIDTH {
        return None;
    }

    let (_, natural_height_for_width, _, _) = widget.measure(Orientation::Vertical, natural_width);
    let natural_height = if natural_height_for_width > POPOVER_MIN_VALID_HEIGHT {
        natural_height_for_width
    } else {
        let (_, natural_height, _, _) = widget.measure(Orientation::Vertical, -1);
        natural_height
    };

    if natural_height <= POPOVER_MIN_VALID_HEIGHT {
        return None;
    }

    Some((natural_width, natural_height))
}

/// Calculate the margin for a popover on the bar-adjacent edge.
///
/// When the bar has a visible background (opacity > 0), the popover needs to
/// account for bar padding in its positioning. This ensures consistent visual
/// spacing regardless of bar transparency settings.
///
/// The returned value can be negative when `bar.padding > popover_offset`.
/// That is intentional: opaque bars include padding in their exclusive zone
/// (`calculate_bar_exclusive_zone_for`), so subtracting it moves the popover
/// back to the requested visual offset from the painted bar surface.
///
/// Used by both `LayerShellPopover` and Quick Settings for consistent positioning.
/// The returned value should be applied to whichever edge the bar occupies.
pub fn calculate_popover_bar_margin() -> i32 {
    let config_mgr = ConfigManager::global();
    let bar_padding = config_mgr.bar_padding() as i32;
    let bar_opacity = config_mgr.bar_background_opacity();
    let popover_offset = config_mgr.popover_offset() as i32;

    let offset = if bar_opacity > 0.0 {
        popover_offset - bar_padding
    } else {
        popover_offset
    };
    // Automatic modes do not reserve space. Anchor explicitly beyond the bar.
    offset + calculate_bar_exclusive_zone() - calculate_bar_reserved_zone()
}

/// Space actually reserved by the bar, distinct from its physical thickness.
pub fn calculate_bar_reserved_zone() -> i32 {
    if ConfigManager::global().bar_auto_hide() == vibepanel_core::config::AutoHide::Never {
        calculate_bar_exclusive_zone()
    } else {
        0
    }
}

/// Fallback popover scroll height when monitor geometry is unavailable.
const POPOVER_FALLBACK_MAX_HEIGHT: i32 = 500;

/// Minimum margin between a popover and the far screen edge.
const POPOVER_FAR_EDGE_MARGIN: i32 = 8;

/// Maximum height for a popover's scrollable content on `monitor`.
///
/// Subtracts the space the bar reserves plus the popover's bar margin (for
/// horizontal bars), the caller's non-scrolling `overhead`, and a far-edge margin.
/// Auto-hide bars reserve nothing; their thickness is already in the margin.
pub(crate) fn popover_max_content_height(monitor: Option<&Monitor>, overhead: i32) -> i32 {
    let Some(monitor) = monitor else {
        return POPOVER_FALLBACK_MAX_HEIGHT;
    };
    let bar_reservation = if ConfigManager::global().bar_position().is_horizontal() {
        calculate_bar_reserved_zone() + calculate_popover_bar_margin()
    } else {
        0
    };
    max_content_height_for(monitor.geometry().height(), bar_reservation, overhead)
}

fn max_content_height_for(monitor_height: i32, bar_reservation: i32, overhead: i32) -> i32 {
    (monitor_height - bar_reservation - overhead - POPOVER_FAR_EDGE_MARGIN).max(1)
}

/// Get the edge that popovers should anchor to (same side as the bar).
///
/// When bar is at the top, popovers anchor to `Edge::Top` and open downward.
/// When bar is at the bottom, popovers anchor to `Edge::Bottom` and open upward.
pub fn popover_bar_edge() -> Edge {
    match ConfigManager::global().bar_position() {
        BarPosition::Top => Edge::Top,
        BarPosition::Bottom => Edge::Bottom,
        BarPosition::Left => Edge::Left,
        BarPosition::Right => Edge::Right,
    }
}

/// Automatic bars use explicit offsets, ignoring other reserved screen edges.
pub(crate) fn popover_exclusive_zone() -> i32 {
    if ConfigManager::global().bar_auto_hide() == vibepanel_core::config::AutoHide::Never {
        0
    } else {
        -1
    }
}

/// Configure layer-shell anchors so the popover hugs the bar edge and can be
/// positioned along the opposite axis with a single far-edge margin.
pub fn configure_popover_layer_anchors(window: &ApplicationWindow) {
    window.set_exclusive_zone(popover_exclusive_zone());
    match ConfigManager::global().bar_position() {
        BarPosition::Top => {
            window.set_anchor(Edge::Top, true);
            window.set_anchor(Edge::Bottom, false);
            window.set_anchor(Edge::Left, false);
            window.set_anchor(Edge::Right, true);
        }
        BarPosition::Bottom => {
            window.set_anchor(Edge::Top, false);
            window.set_anchor(Edge::Bottom, true);
            window.set_anchor(Edge::Left, false);
            window.set_anchor(Edge::Right, true);
        }
        BarPosition::Left => {
            window.set_anchor(Edge::Top, false);
            window.set_anchor(Edge::Bottom, true);
            window.set_anchor(Edge::Left, true);
            window.set_anchor(Edge::Right, false);
        }
        BarPosition::Right => {
            window.set_anchor(Edge::Top, false);
            window.set_anchor(Edge::Bottom, true);
            window.set_anchor(Edge::Left, false);
            window.set_anchor(Edge::Right, true);
        }
    }
}

pub fn reset_popover_margins(window: &ApplicationWindow) {
    for edge in [Edge::Top, Edge::Right, Edge::Bottom, Edge::Left] {
        window.set_margin(edge, 0);
    }
}

/// Calculate the right margin for a popover to center it on an anchor point.
///
/// This clamps the margin to keep the popover on-screen while centering it
/// as closely as possible to the anchor X coordinate.
///
/// # Coordinate Space
///
/// All parameters use **monitor-local coordinates** (0,0 at the monitor's top-left).
/// This is correct because:
/// - Layer-shell surfaces are anchored to specific monitors
/// - horizontal bar surfaces span the monitor width, so surface-local X matches
///   monitor-local X along the placement axis
/// - `monitor_width` is from `monitor.geometry().width()` (the monitor's own width)
/// - The resulting margin is applied to a layer-shell surface on the same monitor
///
/// # Arguments
///
/// * `anchor_x` - X coordinate of the anchor point (widget center) in monitor-local coordinates
/// * `monitor_width` - Width of the monitor (from `monitor.geometry().width()`)
/// * `window_width` - Actual or estimated width of the popover window
/// * `min_edge_margin` - Minimum margin from screen edge
///
/// # Returns
///
/// The right margin to apply to the window, clamped to valid bounds.
pub fn calculate_popover_right_margin(
    anchor_x: i32,
    monitor_width: i32,
    window_width: i32,
    min_edge_margin: i32,
) -> i32 {
    let right_margin = monitor_width - anchor_x - window_width / 2;
    let max_margin = monitor_width.saturating_sub(window_width + min_edge_margin);

    // Ensure min <= max to avoid clamp panic
    if max_margin >= min_edge_margin {
        right_margin.clamp(min_edge_margin, max_margin)
    } else {
        // Window is too wide for monitor, just use minimum margin
        min_edge_margin.max(max_margin)
    }
}

pub fn calculate_popover_bottom_margin(
    anchor_y: i32,
    monitor_height: i32,
    window_height: i32,
    min_edge_margin: i32,
) -> i32 {
    calculate_popover_right_margin(anchor_y, monitor_height, window_height, min_edge_margin)
}

/// Get the appropriate keyboard mode for layer-shell popovers.
///
/// - **Hyprland**: Uses `OnDemand` because `Exclusive` mode breaks input handling
///   entirely (clicks don't work, can't interact with other surfaces).
/// - **Other compositors**: Uses `Exclusive` to maintain keyboard focus after
///   workspace switches.
pub fn popover_keyboard_mode() -> KeyboardMode {
    if CompositorManager::global().backend_name() == "Hyprland" {
        KeyboardMode::OnDemand
    } else {
        KeyboardMode::Exclusive
    }
}

/// Calculate the bar's exclusive-zone thickness for click-catcher margin.
///
/// This matches the layer-shell surface thickness in `bar.rs`: the visible bar
/// thickness plus the single screen-edge spacer inserted ahead of/after it.
pub fn calculate_bar_exclusive_zone() -> i32 {
    let config_mgr = ConfigManager::global();
    calculate_bar_exclusive_zone_for(
        config_mgr.bar_size() as i32,
        config_mgr.bar_padding() as i32,
        config_mgr.screen_margin() as i32,
        config_mgr.bar_background_opacity(),
    )
}

pub(crate) fn calculate_bar_exclusive_zone_for(
    bar_size: i32,
    bar_padding: i32,
    screen_margin: i32,
    bar_opacity: f64,
) -> i32 {
    if bar_opacity > 0.0 {
        bar_size + 2 * bar_padding + screen_margin
    } else {
        bar_size + bar_padding + screen_margin
    }
}

/// Create a click-catcher layer-shell surface.
///
/// The click-catcher is a fullscreen transparent surface that sits behind popovers
/// and captures clicks outside the popover to dismiss it. It has a margin on the
/// bar-adjacent edge equal to the bar's exclusive zone so clicks on the bar pass
/// through.
///
/// # Arguments
///
/// * `app` - The GTK application
/// * `bar_zone` - Height of the bar's exclusive zone (margin on bar edge to leave bar uncovered)
/// * `on_dismiss` - Callback invoked when the catcher is clicked
///
/// # Returns
///
/// The click-catcher window. Caller is responsible for showing it and storing it.
pub fn create_click_catcher<F>(app: &Application, bar_zone: i32, on_dismiss: F) -> ApplicationWindow
where
    F: Fn() + Clone + 'static,
{
    let catcher = ApplicationWindow::builder()
        .application(app)
        .title("vibepanel click catcher")
        .decorated(false)
        .build();

    catcher.add_css_class(surface::LAYER_SHELL_CLICK_CATCHER);
    catcher.add_css_class(class::CLICK_CATCHER);

    // Layer shell configuration - fullscreen transparent surface around popovers.
    catcher.init_layer_shell();
    catcher.set_namespace(Some("vibepanel-click-catcher"));
    catcher.set_layer(Layer::Top);
    catcher.set_exclusive_zone(-1); // Cover everything
    catcher.set_anchor(Edge::Top, true);
    catcher.set_anchor(Edge::Bottom, true);
    catcher.set_anchor(Edge::Left, true);
    catcher.set_anchor(Edge::Right, true);
    catcher.set_keyboard_mode(KeyboardMode::None);

    catcher.set_margin(popover_bar_edge(), bar_zone);

    // Content - add CSS class to the child widget for background styling
    let overlay = GtkBox::new(Orientation::Vertical, 0);
    overlay.set_hexpand(true);
    overlay.set_vexpand(true);
    overlay.add_css_class(class::CLICK_CATCHER); // Apply background to child
    catcher.set_child(Some(&overlay));

    // Click handler
    let gesture = GestureClick::new();
    gesture.set_button(0); // All buttons
    // Use connect_released to allow GTK to complete the gesture lifecycle
    // before hiding windows. This avoids "Broken accounting of active state" warnings.
    gesture.connect_released(move |_, _, _, _| on_dismiss());
    catcher.add_controller(gesture);

    // Note: No ESC handler on click-catcher. ESC handling is done by the actual
    // popover window via setup_esc_handler(). The click-catcher has KeyboardMode::None
    // so it won't receive keyboard events anyway.

    catcher
}

/// Set up ESC key handler on a window to dismiss the popover.
pub fn setup_esc_handler<F>(window: &ApplicationWindow, on_dismiss: F)
where
    F: Fn() + 'static,
{
    let key_controller = EventControllerKey::new();
    key_controller.connect_key_pressed(move |_, keyval, _, _| {
        if keyval == gdk::Key::Escape {
            on_dismiss();
            Propagation::Stop
        } else {
            Propagation::Proceed
        }
    });
    window.add_controller(key_controller);
}

const HEIGHT_FREEZE_DATA_KEY: &str = "vibepanel-surface-height-freeze";
const HEIGHT_FREEZE_RELEASE_HOOK_KEY: &str = "vibepanel-surface-height-freeze-release-hook";

/// Keeps a shrink-wrapped layer-shell surface at a constant height while child
/// revealers animate. Expansion pins the final height; collapse pins the initial
/// height, avoiding per-frame Wayland surface configures and buffer reallocations.
struct SurfaceHeightFreeze {
    window: glib::WeakRef<ApplicationWindow>,
    blur_widget: glib::WeakRef<gtk4::Widget>,
    active: RefCell<Vec<gtk4::Revealer>>,
    frozen_height: Cell<i32>,
    blur_generation: Cell<u32>,
}

impl SurfaceHeightFreeze {
    fn new(window: &ApplicationWindow, blur_widget: &impl IsA<gtk4::Widget>) -> Rc<Self> {
        Rc::new(Self {
            window: window.downgrade(),
            blur_widget: blur_widget.as_ref().downgrade(),
            active: RefCell::new(Vec::new()),
            frozen_height: Cell::new(0),
            blur_generation: Cell::new(0),
        })
    }

    fn begin_expand(self: &Rc<Self>, revealer: &gtk4::Revealer) {
        if !ConfigManager::global().animations_enabled()
            || self.active.borrow().iter().any(|active| active == revealer)
        {
            return;
        }
        let Some(window) = self.window.upgrade() else {
            return;
        };
        if !window.is_mapped() {
            return;
        }
        if revealer.child().is_none() {
            return;
        }
        let Some(root) = window.child() else {
            return;
        };
        let mut revealers = self.active.borrow().clone();
        revealers.push(revealer.clone());
        let mut old_height_requests = Vec::with_capacity(revealers.len());
        for active in revealers {
            let Some(child) = active.child() else {
                continue;
            };
            // Revealers retain their parent-allocated width while collapsed.
            let (child_min_width, _, _, _) = child.measure(Orientation::Horizontal, -1);
            let (_, child_nat, _, _) =
                child.measure(Orientation::Vertical, active.width().max(child_min_width));
            old_height_requests.push((active.clone(), active.height_request()));
            active.set_height_request(child_nat);
        }
        let (root_min_width, _, _, _) = root.measure(Orientation::Horizontal, -1);
        let (_, target, _, _) =
            root.measure(Orientation::Vertical, root.width().max(root_min_width));
        for (active, request) in old_height_requests {
            active.set_height_request(request);
        }

        // Active collapses may over-reserve space, but avoid another surface resize.
        let target = target.max(self.frozen_height.get());
        if target <= 0 {
            return;
        }

        let first = self.active.borrow().is_empty();
        self.active.borrow_mut().push(revealer.clone());
        self.frozen_height.set(target);
        self.apply(target);
        if first {
            self.start_blur_tracking();
        }
    }

    fn begin_collapse(self: &Rc<Self>, revealer: &gtk4::Revealer) {
        if !ConfigManager::global().animations_enabled()
            || self.active.borrow().iter().any(|active| active == revealer)
        {
            return;
        }
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let current = window.height();
        if current <= 0 {
            return;
        }

        let first = self.active.borrow().is_empty();
        self.active.borrow_mut().push(revealer.clone());
        if first {
            self.frozen_height.set(current);
            self.apply(current);
            self.start_blur_tracking();
        }
    }

    fn apply(&self, height: i32) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        if let Some(widget) = window.child() {
            // Match the content alignment to the layer-shell edge anchoring.
            widget.set_valign(if popover_bar_edge() == Edge::Top {
                gtk4::Align::Start
            } else {
                gtk4::Align::End
            });
        }
        // This is a floor: excess stays transparent; shortages resize naturally.
        window.set_size_request(-1, height);
    }

    fn end(self: &Rc<Self>, revealer: &gtk4::Revealer) {
        let removed = {
            let mut active = self.active.borrow_mut();
            let old_len = active.len();
            active.retain(|active| active != revealer);
            active.len() != old_len
        };
        if !removed {
            return;
        }
        if !self.active.borrow().is_empty() {
            return;
        }
        self.release();
        if !ConfigManager::global().blur_enabled() {
            return;
        }

        let generation = self.blur_generation.get();
        let weak_self = Rc::downgrade(self);
        // Expansion needs an explicit final commit because releasing its equal
        // height floor does not resize; collapse is corrected by the resize watcher.
        glib::idle_add_local_once(move || {
            let Some(freeze) = weak_self.upgrade() else {
                return;
            };
            if freeze.blur_generation.get() != generation || !ConfigManager::global().blur_enabled()
            {
                return;
            }
            let (Some(window), Some(content)) =
                (freeze.window.upgrade(), freeze.blur_widget.upgrade())
            else {
                return;
            };
            if let Some(blur) = BackgroundEffectManager::global() {
                blur.apply_blur_surface(&window, &content, || {
                    ConfigManager::global().surface_border_radius() as i32
                });
            }
        });
    }

    fn clear(&self) {
        self.active.borrow_mut().clear();
        self.blur_generation
            .set(self.blur_generation.get().wrapping_add(1));
        self.release();
    }

    fn release(&self) {
        self.frozen_height.set(0);
        if let Some(window) = self.window.upgrade() {
            window.set_size_request(-1, -1);
            if let Some(widget) = window.child() {
                widget.set_valign(gtk4::Align::Fill);
            }
        }
    }

    fn start_blur_tracking(self: &Rc<Self>) {
        if !ConfigManager::global().blur_enabled() {
            return;
        }
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let generation = self.blur_generation.get().wrapping_add(1);
        self.blur_generation.set(generation);
        let weak_self = Rc::downgrade(self);
        window.add_tick_callback(move |_, _| {
            let Some(freeze) = weak_self.upgrade() else {
                return ControlFlow::Break;
            };
            if freeze.blur_generation.get() != generation || freeze.active.borrow().is_empty() {
                return ControlFlow::Break;
            }
            freeze.apply_blur();
            ControlFlow::Continue
        });
    }

    fn apply_blur(&self) {
        if !ConfigManager::global().blur_enabled() {
            return;
        }
        let (Some(window), Some(content)) = (self.window.upgrade(), self.blur_widget.upgrade())
        else {
            return;
        };
        if let Some(blur) = BackgroundEffectManager::global() {
            blur.apply_blur_region_animated(&window, &content, 1.0);
        }
    }
}

/// Install shared revealer surface-freeze handling on a popover window.
pub(crate) fn install_surface_height_freeze(
    window: &ApplicationWindow,
    blur_widget: &impl IsA<gtk4::Widget>,
) {
    unsafe {
        window.set_data(
            HEIGHT_FREEZE_DATA_KEY,
            SurfaceHeightFreeze::new(window, blur_widget),
        );
    }
}

fn surface_height_freeze_for(widget: &impl IsA<gtk4::Widget>) -> Option<Rc<SurfaceHeightFreeze>> {
    let window = widget
        .as_ref()
        .root()?
        .downcast::<ApplicationWindow>()
        .ok()?;
    unsafe {
        window
            .data::<Rc<SurfaceHeightFreeze>>(HEIGHT_FREEZE_DATA_KEY)
            .map(|freeze| freeze.as_ref().clone())
    }
}

fn clear_surface_height_freeze(window: &ApplicationWindow) {
    unsafe {
        if let Some(freeze) = window.data::<Rc<SurfaceHeightFreeze>>(HEIGHT_FREEZE_DATA_KEY) {
            freeze.as_ref().clear();
        }
    }
}

/// Animate a revealer without resizing its layer-shell surface every frame.
pub(crate) fn animate_reveal(revealer: &gtk4::Revealer, expanding: bool) {
    if revealer.reveals_child() == expanding {
        return;
    }
    if let Some(freeze) = surface_height_freeze_for(revealer) {
        if expanding {
            freeze.begin_expand(revealer);
        } else {
            freeze.begin_collapse(revealer);
        }
        ensure_height_freeze_release_hook(revealer, &freeze);
    }
    revealer.set_reveal_child(expanding);
}

fn ensure_height_freeze_release_hook(revealer: &gtk4::Revealer, freeze: &Rc<SurfaceHeightFreeze>) {
    unsafe {
        if revealer
            .data::<bool>(HEIGHT_FREEZE_RELEASE_HOOK_KEY)
            .is_some()
        {
            return;
        }
        revealer.set_data(HEIGHT_FREEZE_RELEASE_HOOK_KEY, true);
    }
    let weak_freeze = Rc::downgrade(freeze);
    revealer.connect_child_revealed_notify(move |revealer| {
        if let Some(freeze) = weak_freeze.upgrade() {
            freeze.end(revealer);
        }
    });
    let weak_freeze = Rc::downgrade(freeze);
    revealer.connect_unmap(move |revealer| {
        if let Some(freeze) = weak_freeze.upgrade() {
            freeze.end(revealer);
        }
    });
}

/// A layer-shell popover for widget menus.
///
/// The window shell (`ApplicationWindow` with layer-shell configuration) is
/// created lazily on first show and **reused** across open/close cycles.
///
/// ## Animation architecture
///
/// Open/close animations (opacity fade + scale) are driven by a **tick
/// callback** on the persistent animation shell ([`SurfaceAnimation`]),
/// not by CSS `transition:` properties. The shell is a [`ScaleBox`] that
/// renders a true center scale transform with a *quantized* scale value —
/// continuous per-frame scales leak renderer glyph caches (see the
/// `scale_box` module docs), which is why CSS transitions cannot be used.
///
/// The tick callback reads the frame clock each frame, computes eased progress
/// from an `AnimState`, and applies opacity + scale. This gives:
///
/// - **Bounded memory** (quantized text scales reuse glyph cache entries)
/// - **Smooth mid-flight reversal** (clicking close during open reverses from
///   the current position, proportional timing)
/// - **No jank** (no snapping between states on rapid clicks)
pub struct LayerShellPopover {
    app: Application,
    widget_name: String,
    builder: Rc<dyn Fn() -> gtk4::Widget>,
    window: RefCell<Option<ApplicationWindow>>,
    chrome: BarPopoverChrome,
    /// Open/close animation. Its persistent shell is never destroyed; builder
    /// content is placed inside it and swapped on each show.
    anim: SurfaceAnimation,
    /// Widget center in monitor coordinates.
    anchor: Cell<PopoverAnchor>,
    anchor_monitor: RefCell<Option<Monitor>>,
    /// Optional callback invoked when the popover is fully hidden (after close
    /// animation completes). NOT fired at the start of hide().
    on_close: RefCell<Option<Rc<dyn Fn()>>>,
    /// Optional callback invoked every time the popover is shown (after content
    /// is parented but before the animation starts). Use this to refresh data
    /// in reuse mode — e.g. updating the calendar to today's date.
    on_show: RefCell<Option<Rc<dyn Fn()>>>,
    /// Optional callback invoked when `show_at()` receives a new anchor monitor.
    on_anchor_monitor_changed: RefCell<Option<AnchorMonitorCallback>>,
    /// Logical open state. True from the moment show() is called until
    /// hide() is called. Used by is_visible() so the toggle logic in BaseWidget works correctly
    /// even while a close animation is in flight.
    logically_open: Cell<bool>,
    /// Set when `mark_content_dirty()` is called while the popover is not
    /// logically open (e.g. a notification arrives during the close animation).
    /// Checked and cleared on mid-close reversal so the content gets rebuilt.
    content_dirty: Cell<bool>,
    /// When true, the builder is called only once and the content widget is
    /// cached across open/close cycles. On subsequent opens the cached widget
    /// is re-parented into the anim shell instead of calling the builder again.
    ///
    /// This avoids per-cycle widget allocation which is observed to leak memory
    /// in GTK4 for widgets with complex internal trees (e.g. Calendar).
    reuse_content: Cell<bool>,
    /// Cached content widget for reuse mode. Kept alive across close cycles
    /// so it can be re-parented on the next open.
    cached_content: RefCell<Option<gtk4::Widget>>,
}

impl LayerShellPopover {
    /// Create a new layer-shell popover.
    ///
    /// # Arguments
    ///
    /// * `app` - The GTK application
    /// * `widget_name` - Widget name for CSS classes (e.g., "clock")
    /// * `builder` - Function that builds the popover content
    pub fn new<F>(app: &Application, widget_name: &str, builder: F) -> Rc<Self>
    where
        F: Fn() -> gtk4::Widget + 'static,
    {
        Rc::new(Self {
            app: app.clone(),
            widget_name: widget_name.to_string(),
            builder: Rc::new(builder),
            window: RefCell::new(None),
            chrome: BarPopoverChrome::default(),
            anim: SurfaceAnimation::new(),
            anchor: Cell::new(PopoverAnchor::default()),
            anchor_monitor: RefCell::new(None),
            on_close: RefCell::new(None),
            on_show: RefCell::new(None),
            on_anchor_monitor_changed: RefCell::new(None),
            logically_open: Cell::new(false),
            content_dirty: Cell::new(false),
            reuse_content: Cell::new(false),
            cached_content: RefCell::new(None),
        })
    }

    /// Check if the popover is logically open.
    ///
    /// Returns `true` from the moment `show_at()` is called until `hide()`
    /// is called, even though the window may still be visible during the close
    /// animation. This is critical for the toggle logic in `BaseWidget` to
    /// work correctly during rapid clicking.
    pub fn is_visible(&self) -> bool {
        self.logically_open.get()
    }

    #[cfg(test)]
    pub(crate) fn test_window(&self) -> Option<ApplicationWindow> {
        self.window.borrow().as_ref().cloned()
    }

    #[cfg(test)]
    pub(crate) fn test_click_catcher(&self) -> Option<ApplicationWindow> {
        self.chrome.catcher()
    }

    /// Set a callback to be invoked when the popover is hidden.
    pub fn set_on_close<F: Fn() + 'static>(&self, callback: F) {
        *self.on_close.borrow_mut() = Some(Rc::new(callback));
    }

    /// Set a callback to be invoked every time the popover is shown.
    ///
    /// In reuse mode this is called after the cached content is re-parented,
    /// allowing consumers to refresh data (e.g. update calendar to today). It
    /// also fires when a close animation reverses before `on_close`; callbacks
    /// that acquire resources must therefore be idempotent until `on_close` runs.
    pub fn set_on_show<F: Fn() + 'static>(&self, callback: F) {
        *self.on_show.borrow_mut() = Some(Rc::new(callback));
    }

    pub fn set_on_anchor_monitor_changed<F: Fn(Option<Monitor>) + 'static>(&self, callback: F) {
        *self.on_anchor_monitor_changed.borrow_mut() = Some(Rc::new(callback));
    }

    /// Enable content reuse mode.
    ///
    /// When enabled, the builder is called only once and the resulting widget
    /// is cached. On subsequent opens the cached widget is re-parented into
    /// the anim shell instead of calling the builder again.
    pub fn set_reuse_content(&self, reuse: bool) {
        self.reuse_content.set(reuse);
    }

    /// Mark the popover content as needing a rebuild.
    ///
    /// Called by `MenuHandle::refresh_if_visible()` when the popover is not
    /// logically open (e.g. a notification arrives during the close animation).
    /// The flag is checked on mid-close reversal so the stale content gets
    /// replaced before the user sees it again.
    pub fn mark_content_dirty(&self) {
        self.content_dirty.set(true);
    }

    /// Show the popover at the given anchor position.
    ///
    /// Reuses all persistent shells (window, animation, click-catcher) and
    /// builds fresh content.
    pub fn show_at(self: &Rc<Self>, anchor: PopoverAnchor, monitor: Option<Monitor>) {
        self.anchor.set(anchor);
        if let Some(ref cb) = *self.on_anchor_monitor_changed.borrow() {
            cb(monitor.clone());
        }
        *self.anchor_monitor.borrow_mut() = monitor;
        self.show_internal();
    }

    /// Hide the popover with a close animation, keeping the window shell alive.
    ///
    /// The click-catcher is hidden immediately so the bar is interactive
    /// during the animation. The animation shell fades out via the tick
    /// callback, then content is removed and the window is hidden.
    ///
    /// If the popover is currently opening, the animation smoothly reverses
    /// from the current progress — no snapping.
    ///
    /// The `on_close` callback fires when the close animation **completes**,
    /// not when `hide()` is called.
    pub fn hide(&self) {
        // Mark as logically closed immediately — the toggle logic in BaseWidget
        // checks this to decide show vs hide on the next click.
        if self.logically_open.replace(false) {
            crate::popover_tracker::PopoverTracker::global().notify_changed();
        }

        // Bump generation to cancel any pending idle callback from show_internal().
        let generation = self.anim.next_generation();

        let Some(window) = self.window.borrow().as_ref().cloned() else {
            return;
        };
        // Catcher hides now so the bar is interactive during the fade.
        self.chrome.close(&window);

        // on_close fires once the window is hidden.
        self.start_animation(AnimDirection::Closing, generation);
    }

    /// Rebuild the popover content in-place without any animation.
    ///
    /// Used by `MenuHandle::refresh_if_visible()` to hot-swap content while the
    /// popover is already open (e.g. a new notification arrives). This avoids
    /// the hide→show cycle which would trigger the mid-close reversal path and
    /// skip the content rebuild.
    pub fn rebuild_content(self: &Rc<Self>) {
        // Nothing to rebuild before the first show.
        let Some(window) = self.window.borrow().as_ref().cloned() else {
            return;
        };
        clear_surface_height_freeze(&window);

        let anim_shell = self.anim.shell();
        anim_shell.remove_child();

        // Invalidate cache so the builder runs fresh.
        *self.cached_content.borrow_mut() = None;

        let content = self.build_content();

        // Re-cache if in reuse mode.
        if self.reuse_content.get() {
            *self.cached_content.borrow_mut() = Some(content.clone());
        }

        anim_shell.set_child(&content);
        SurfaceStyleManager::global().apply_pango_attrs_all(anim_shell);

        // Reposition after content updates while visible (e.g. notifications
        // list changing in-place). Do this in idle so GTK has a chance to
        // re-measure the new child before we read window dimensions.
        if self.logically_open.get() {
            let weak_self = Rc::downgrade(self);
            glib::idle_add_local_once(move || {
                if let Some(popover) = weak_self.upgrade()
                    && popover.logically_open.get()
                {
                    popover.update_position();
                }
            });
        }
    }

    fn show_internal(self: &Rc<Self>) {
        // Mark as logically open immediately.
        self.logically_open.set(true);

        // Same output mid-close: reverse the running animation.
        let window = self.ensure_window_shell();
        if self
            .anim
            .can_reverse_on(&window, self.anchor_monitor.borrow().as_ref())
        {
            // Content may have gone stale while logically closed (e.g. a
            // notification arrived); rebuild before the reversal completes.
            if self.content_dirty.take() {
                self.rebuild_content();
            }
            self.fire_on_show();
            self.open_chrome(&window);
            // Anchor may have changed since the original open.
            self.update_position();
            self.start_animation(AnimDirection::Opening, self.anim.generation());
            self.chrome.prepare_keynav(&window);
            return;
        }

        // Full open. If reused content was marked dirty while hidden, rebuild
        // it before attaching instead of mutating the mapped content in on_show.
        let rebuild_cached_content = self.content_dirty.take();
        let generation = self.anim.next_generation();

        // Fresh open; also drops a close still running on another output.
        if window.is_visible() {
            window.set_visible(false);
        }

        let anim_shell = self.anim.shell();
        anim_shell.remove_child();

        // In reuse mode the builder runs once; per-cycle allocation leaks
        // memory in GTK4 for complex widgets (e.g. Calendar).
        let cached = self
            .cached_content
            .borrow()
            .clone()
            .filter(|_| self.reuse_content.get() && !rebuild_cached_content);
        let content = cached.unwrap_or_else(|| {
            let fresh = self.build_content();
            if self.reuse_content.get() {
                *self.cached_content.borrow_mut() = Some(fresh.clone());
            }
            fresh
        });
        anim_shell.set_child(&content);

        // Fire on_show callback (e.g. to refresh calendar to today's date).
        self.fire_on_show();
        SurfaceStyleManager::global().apply_pango_attrs_all(anim_shell);

        if let Some(ref monitor) = *self.anchor_monitor.borrow() {
            window.set_monitor(Some(monitor));
        }
        self.open_chrome(&window);

        if ConfigManager::global().animations_enabled() {
            let weak_self = Rc::downgrade(self);
            self.anim.present_hidden_then(&window, generation, move || {
                if let Some(popover) = weak_self.upgrade() {
                    popover.update_position();
                    popover.start_animation(AnimDirection::Opening, generation);
                }
            });
        } else {
            // Snap visible before mapping so a close/open cycle cannot leave
            // the reused surface transparent if the idle pass is delayed.
            self.anim.snap_open();
            window.set_opacity(1.0);

            // Position before map using GTK's natural size so the first visible
            // frame is already placed; the idle pass refines with the real size.
            self.update_position_for_size(
                window
                    .child()
                    .and_then(|child| measured_popover_size(&child)),
            );
            window.set_visible(true);
            window.present();
            CompositorManager::global().refresh_pointer_focus();

            let weak_self = Rc::downgrade(self);
            glib::idle_add_local_once(move || {
                if let Some(popover) = weak_self.upgrade()
                    && popover.logically_open.get()
                {
                    popover.update_position();
                }
            });
        }

        // present() may assign auto-focus; clear it after mapping.
        self.chrome.prepare_keynav(&window);
    }

    fn open_chrome(self: &Rc<Self>, window: &ApplicationWindow) {
        let weak_self = Rc::downgrade(self);
        self.chrome
            .open(window, self.anchor_monitor.borrow().as_ref(), move || {
                if let Some(popover) = weak_self.upgrade() {
                    popover.hide();
                }
            });
    }

    fn build_content(&self) -> gtk4::Widget {
        let content = (self.builder)();
        content.add_css_class(surface::POPOVER);
        content.add_css_class(surface::SURFACE_POPOVER);
        content.add_css_class(surface::WIDGET_MENU);
        content.add_css_class(&format!("{}-popover", self.widget_name));
        content
    }

    fn fire_on_show(&self) {
        if let Some(ref callback) = *self.on_show.borrow() {
            callback();
        }
    }

    /// Ensure the window shell exists, creating it lazily if needed.
    ///
    /// The shell includes the `ApplicationWindow`, layer-shell configuration,
    /// ESC handler, and the persistent wrapper around the animation shell.
    /// Content is set by `show_internal()` on each open.
    fn ensure_window_shell(self: &Rc<Self>) -> ApplicationWindow {
        if let Some(ref window) = *self.window.borrow() {
            return window.clone();
        }

        let window = ApplicationWindow::builder()
            .application(&self.app)
            .title(format!("vibepanel {} popover", self.widget_name))
            .decorated(false)
            .resizable(false)
            .build();

        window.add_css_class(surface::LAYER_SHELL_POPOVER);
        window.init_layer_shell();
        window.set_namespace(Some(&format!("vibepanel-{}-popover", self.widget_name)));
        window.set_layer(Layer::Top);
        window.set_exclusive_zone(0);
        configure_popover_layer_anchors(&window);
        window.set_keyboard_mode(popover_keyboard_mode());

        let outer = GtkBox::new(Orientation::Vertical, 0);
        outer.add_css_class(surface::POPOVER_WRAPPER);
        outer.add_css_class(surface::WIDGET_MENU_WRAPPER);
        outer.add_css_class(surface::NO_FOCUS);
        SurfaceStyleManager::global().apply_shadow_margins(&outer, SHADOW_MARGIN);
        outer.append(self.anim.shell());
        window.set_child(Some(&outer));
        install_surface_height_freeze(&window, self.anim.shell());

        let weak_self = Rc::downgrade(self);
        setup_esc_handler(&window, move || {
            if let Some(popover) = weak_self.upgrade() {
                popover.hide();
            }
        });

        // Close unmaps the surface, so this runs on every show. On re-show the
        // animation tick replaces the full-size region within 1-2 frames.
        // Config changes to blur/radius while open apply on the next open.
        let anim_shell = self.anim.shell().clone();
        window.connect_map(move |win| {
            sync_blur(win, Some(anim_shell.clone().upcast()), || {
                ConfigManager::global().surface_border_radius() as i32
            });
        });

        *self.window.borrow_mut() = Some(window.clone());
        window
    }

    /// Start or reverse the open/close animation via a tick callback.
    ///
    /// If an animation is already in flight (e.g., opening and user clicks to
    /// close), the current progress is captured and the animation reverses from
    /// that point with proportional timing — no snapping.
    fn start_animation(&self, direction: AnimDirection, generation: u32) {
        let Some(window) = self.window.borrow().as_ref().cloned() else {
            return;
        };
        let on_close = self.on_close.borrow().clone();
        let shell = self.anim.shell().downgrade();

        self.anim.run(
            direction,
            generation,
            &window,
            self.anim.shell().upcast_ref(),
            move || {
                if let Some(shell) = shell.upgrade() {
                    shell.remove_child();
                }
                if let Some(ref cb) = on_close {
                    cb();
                }
            },
        );
    }

    fn update_position(&self) {
        self.update_position_for_size(None);
    }

    fn update_position_for_size(&self, size_override: Option<(i32, i32)>) {
        let Some(ref window) = *self.window.borrow() else {
            return;
        };

        let anchor = self.anchor.get();

        // Get monitor from anchor or fall back to primary
        let monitor_opt = self.anchor_monitor.borrow().clone().or_else(|| {
            gdk::Display::default().and_then(|display| {
                display
                    .monitors()
                    .item(0)
                    .and_then(|obj| obj.downcast::<Monitor>().ok())
            })
        });

        let Some(monitor) = monitor_opt else {
            return;
        };

        let geom = monitor.geometry();

        configure_popover_layer_anchors(window);
        reset_popover_margins(window);

        // Set margin on the bar-adjacent edge
        let bar_edge = popover_bar_edge();
        window.set_margin(bar_edge, calculate_popover_bar_margin());

        if ConfigManager::global().bar_position().is_horizontal() {
            // Calculate horizontal position (center on anchor.x)
            if anchor.x > 0 {
                let window_width = if let Some((width, _)) = size_override {
                    width
                } else {
                    let w = window.width();
                    if w > POPOVER_MIN_VALID_WIDTH {
                        w
                    } else {
                        POPOVER_DEFAULT_WIDTH_ESTIMATE
                    }
                };
                let right_margin = calculate_popover_right_margin(
                    anchor.x,
                    geom.width(),
                    window_width,
                    POPOVER_MIN_EDGE_MARGIN,
                );
                window.set_margin(Edge::Right, right_margin);
            } else {
                let fallback_margin = SurfaceStyleManager::global().shadow_margin(SHADOW_MARGIN);
                window.set_margin(Edge::Right, fallback_margin);
            }
        } else if anchor.y > 0 {
            let window_height = if let Some((_, height)) = size_override {
                height
            } else {
                let h = window.height();
                if h > POPOVER_MIN_VALID_HEIGHT {
                    h
                } else {
                    POPOVER_DEFAULT_HEIGHT_ESTIMATE
                }
            };
            let bottom_margin = calculate_popover_bottom_margin(
                anchor.y,
                geom.height(),
                window_height,
                POPOVER_MIN_EDGE_MARGIN,
            );
            window.set_margin(Edge::Bottom, bottom_margin);
        } else {
            let fallback_margin = SurfaceStyleManager::global().shadow_margin(SHADOW_MARGIN);
            window.set_margin(Edge::Bottom, fallback_margin);
        }
    }
}

/// Trait for surfaces that can be dismissed.
pub trait Dismissible {
    fn dismiss(&self);
    fn is_visible(&self) -> bool;
}

impl Drop for LayerShellPopover {
    fn drop(&mut self) {
        // If the popover was still open (or mid-animation) when destroyed,
        // fire on_close synchronously so consumers can clean up resources
        // (e.g. the system popover releases GPU polling).
        if (self.logically_open.get() || self.anim.is_active())
            && let Some(ref cb) = *self.on_close.borrow()
        {
            cb();
        }

        self.chrome.destroy();
        // Best-effort blur cleanup; primary removal happens when
        // SurfaceAnimation::run starts the close. May no-op if already unmapped.
        // See BackgroundEffectManager::remove_blur_region docs.
        if let Some(blur) = BackgroundEffectManager::global()
            && let Some(ref window) = *self.window.borrow()
        {
            blur.remove_blur_region(window);
        }
        if let Some(window) = self.window.borrow_mut().take() {
            window.close();
        }
    }
}

impl Dismissible for LayerShellPopover {
    fn dismiss(&self) {
        self.hide();
    }

    fn is_visible(&self) -> bool {
        self.is_visible()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_content_height_subtracts_reservations_and_never_exceeds_screen() {
        // 1080 - 40 bar - 112 overhead - 8 far edge
        assert_eq!(max_content_height_for(1080, 40, 112), 920);
        assert_eq!(max_content_height_for(100, 40, 112), 1);
    }

    #[test]
    fn right_margin_centers_anchor_when_space_allows() {
        assert_eq!(calculate_popover_right_margin(500, 1000, 200, 4), 400);
    }

    #[test]
    fn right_margin_clamps_to_edge_margins() {
        assert_eq!(calculate_popover_right_margin(20, 1000, 200, 4), 796);
        assert_eq!(calculate_popover_right_margin(990, 1000, 200, 4), 4);
    }

    #[test]
    fn bottom_margin_uses_same_axis_math_for_vertical_bars() {
        assert_eq!(calculate_popover_bottom_margin(400, 800, 200, 4), 300);
    }

    #[test]
    fn exclusive_zone_matches_bar_surface_thickness() {
        assert_eq!(calculate_bar_exclusive_zone_for(32, 4, 12, 1.0), 52);
        assert_eq!(calculate_bar_exclusive_zone_for(32, 4, 12, 0.5), 52);
        assert_eq!(calculate_bar_exclusive_zone_for(32, 4, 12, 0.0), 48);
    }

    #[test]
    fn popover_close_completes_at_cutoff_before_full_duration() {
        let half_us = (ANIM_DURATION_MS * 500.0) as i64;
        let mut state = AnimState::new_idle();
        state.prepare(AnimDirection::Closing, 1, 0, 1.0);
        assert!(!state.popover_is_complete(0));
        assert!(!state.is_complete(half_us));
        assert!(state.popover_is_complete(half_us));

        let mut state = AnimState::new_idle();
        state.prepare(AnimDirection::Opening, 1, 0, 0.0);
        assert!(!state.popover_is_complete(half_us));
    }
}
