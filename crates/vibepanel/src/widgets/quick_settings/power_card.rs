//! Power menu card for Quick Settings panel.
//!
//! Provides power actions with hold-to-confirm UX:
//! - Shutdown, Reboot, Suspend, Lock, Log Out
//! - Toggle card: Hold for [`HOLD_DURATION_MS`] to execute Shutdown (default action)
//! - Chevron: Click to expand/collapse action list
//! - Action rows: Hold for [`HOLD_DURATION_MS`] to execute
//!
//! Two UI variants for prototyping:
//! - Popover: Actions appear in a popover menu
//! - Expander: Actions appear as ListRows in accordion

use std::cell::{Cell, RefCell};
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

use gtk4::gdk::BUTTON_PRIMARY;
use gtk4::glib::{self, SourceId};
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, GestureClick, Label, ListBox, ListBoxRow, Orientation, Overlay,
    Popover, Revealer, RevealerTransitionType,
};
use tracing::{debug, warn};

use crate::services::compositor::CompositorManager;
use crate::services::icons::{IconHandle, IconsService};
use crate::styles::{button, card, color, qs, row};
use crate::widgets::base::configure_popover;

use super::components::{CardLabel, ToggleCard};
use super::ui_helpers::{ExpandableCard, ExpandableCardBase, create_qs_list_box};

/// Animation duration for hold-to-confirm (ms).
const HOLD_DURATION_MS: u64 = 800;

/// Set to `true` for popover variant, `false` for expander variant.
/// Change this to compare both approaches.
pub const USE_POPOVER_VARIANT: bool = false;

/// A power action definition.
struct PowerAction {
    /// Unique identifier for the action.
    id: &'static str,
    /// Display label for the action.
    label: &'static str,
    /// Icon name (GTK/Material mapped).
    icon: &'static str,
    /// Command to execute (first element is program, rest are args).
    /// Empty slice means special handling (e.g., logout via compositor IPC).
    command: &'static [&'static str],
}

/// Available power actions.
const POWER_ACTIONS: &[PowerAction] = &[
    PowerAction {
        id: "shutdown",
        label: "Shutdown",
        icon: "system-shutdown-symbolic",
        command: &["systemctl", "poweroff"],
    },
    PowerAction {
        id: "reboot",
        label: "Reboot",
        icon: "system-reboot-symbolic",
        command: &["systemctl", "reboot"],
    },
    PowerAction {
        id: "suspend",
        label: "Suspend",
        icon: "system-suspend-symbolic",
        command: &["systemctl", "suspend"],
    },
    PowerAction {
        id: "lock",
        label: "Lock",
        icon: "system-lock-screen-symbolic",
        command: &["loginctl", "lock-session"],
    },
    PowerAction {
        id: "logout",
        label: "Log Out",
        icon: "system-log-out-symbolic",
        // Empty command - handled specially via compositor IPC
        command: &[],
    },
];

/// Execute a power action command.
fn execute_power_action(action: &PowerAction) {
    // Special handling for logout - use compositor IPC
    if action.id == "logout" {
        debug!("Executing logout via compositor IPC");
        CompositorManager::global().quit_compositor();
        return;
    }

    if action.command.is_empty() {
        warn!("Power action {} has no command", action.id);
        return;
    }

    debug!("Executing power action {}: {:?}", action.id, action.command);

    match Command::new(action.command[0])
        .args(&action.command[1..])
        .spawn()
    {
        Ok(_) => debug!("Power action {} spawned successfully", action.id),
        Err(e) => warn!("Failed to execute power action {}: {}", action.id, e),
    }
}

/// State for managing hold-to-confirm gesture.
struct HoldToConfirmState {
    /// Timer ID for the animation completion callback.
    timeout_id: RefCell<Option<SourceId>>,
    /// Timer ID for the animation tick.
    anim_id: RefCell<Option<SourceId>>,
    /// Whether we're currently in the confirming state.
    is_confirming: Cell<bool>,
    /// Animation start time (ms since epoch, approximate).
    start_time: Cell<u64>,
}

impl HoldToConfirmState {
    fn new() -> Self {
        Self {
            timeout_id: RefCell::new(None),
            anim_id: RefCell::new(None),
            is_confirming: Cell::new(false),
            start_time: Cell::new(0),
        }
    }

    /// Cancel any pending confirmation and animation.
    fn cancel(&self) {
        if let Some(id) = self.timeout_id.borrow_mut().take() {
            id.remove();
        }
        if let Some(id) = self.anim_id.borrow_mut().take() {
            id.remove();
        }
        self.is_confirming.set(false);
    }
}

/// Animation frame interval in milliseconds (~60fps).
const ANIM_FRAME_MS: u64 = 16;

/// Set up hold-to-confirm gesture on a widget.
///
/// # Arguments
/// * `gesture_widget` - The widget to attach the gesture to (receives click events)
/// * `width_widget` - The widget to use for progress bar width calculation
/// * `progress_overlay` - A GtkBox that will animate width during hold
/// * `on_confirmed` - Callback when hold completes
///
/// The progress_overlay should be positioned as an overlay on top of the content.
/// It will grow from 0 to full width during the hold duration.
fn setup_hold_to_confirm<W1, W2, F>(
    gesture_widget: &W1,
    width_widget: &W2,
    progress_overlay: &GtkBox,
    on_confirmed: F,
) where
    W1: IsA<gtk4::Widget>,
    W2: IsA<gtk4::Widget>,
    F: Fn() + 'static,
{
    let gesture = GestureClick::new();
    gesture.set_button(BUTTON_PRIMARY);
    // Capture phase to get events before child widgets
    gesture.set_propagation_phase(gtk4::PropagationPhase::Capture);

    let state = Rc::new(HoldToConfirmState::new());
    let progress_weak = progress_overlay.downgrade();
    let width_widget_weak = width_widget.upcast_ref::<gtk4::Widget>().downgrade();
    let on_confirmed = Rc::new(on_confirmed);

    // On mouse down: start the visual animation and completion timer
    {
        let state = Rc::clone(&state);
        let progress_weak = progress_weak.clone();
        let width_widget_weak = width_widget_weak.clone();
        let on_confirmed = Rc::clone(&on_confirmed);

        gesture.connect_pressed(move |gesture, _, _, _| {
            // Cancel any previous animation
            state.cancel();

            let Some(progress) = progress_weak.upgrade() else {
                return;
            };

            // Add confirming class for background color
            progress.add_css_class(qs::POWER_CONFIRMING);
            // Also add to parent overlay so CSS can make card background transparent
            if let Some(parent) = progress.parent() {
                parent.add_css_class(qs::POWER_CONFIRMING);
            }
            state.is_confirming.set(true);

            // Record start time
            let start = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            state.start_time.set(start);

            // Reset progress width
            progress.set_size_request(0, -1);

            // Start animation loop
            let state_anim = Rc::clone(&state);
            let progress_weak_anim = progress_weak.clone();
            let width_widget_weak_anim = width_widget_weak.clone();

            let anim_id =
                glib::timeout_add_local(Duration::from_millis(ANIM_FRAME_MS), move || {
                    if !state_anim.is_confirming.get() {
                        return glib::ControlFlow::Break;
                    }

                    let Some(progress) = progress_weak_anim.upgrade() else {
                        return glib::ControlFlow::Break;
                    };

                    let Some(width_widget) = width_widget_weak_anim.upgrade() else {
                        return glib::ControlFlow::Break;
                    };

                    // Calculate elapsed time
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    let elapsed = now.saturating_sub(state_anim.start_time.get());

                    // Calculate progress (0.0 to 1.0)
                    let ratio = (elapsed as f64 / HOLD_DURATION_MS as f64).min(1.0);

                    // Get target width from width widget
                    let target_width = width_widget.width();
                    let current_width = (target_width as f64 * ratio) as i32;

                    progress.set_size_request(current_width, -1);

                    glib::ControlFlow::Continue
                });
            *state.anim_id.borrow_mut() = Some(anim_id);

            // Set completion timer
            let state_timeout = Rc::clone(&state);
            let progress_weak_timeout = progress_weak.clone();
            let on_confirmed = Rc::clone(&on_confirmed);

            let timeout_id =
                glib::timeout_add_local_once(Duration::from_millis(HOLD_DURATION_MS), move || {
                    if state_timeout.is_confirming.get() {
                        state_timeout.cancel();

                        if let Some(progress) = progress_weak_timeout.upgrade() {
                            progress.remove_css_class(qs::POWER_CONFIRMING);
                            if let Some(parent) = progress.parent() {
                                parent.remove_css_class(qs::POWER_CONFIRMING);
                            }
                            progress.set_size_request(0, -1);
                        }

                        on_confirmed();
                    }
                });
            *state.timeout_id.borrow_mut() = Some(timeout_id);

            // Claim the gesture sequence to prevent other handlers
            gesture.set_state(gtk4::EventSequenceState::Claimed);
        });
    }

    // On mouse up: cancel if still in progress
    {
        let state = Rc::clone(&state);
        let progress_weak = progress_weak.clone();

        gesture.connect_released(move |_, _, _, _| {
            if state.is_confirming.get() {
                state.cancel();

                if let Some(progress) = progress_weak.upgrade() {
                    progress.remove_css_class(qs::POWER_CONFIRMING);
                    if let Some(parent) = progress.parent() {
                        parent.remove_css_class(qs::POWER_CONFIRMING);
                    }
                    progress.set_size_request(0, -1);
                }
            }
        });
    }

    // Handle gesture cancel (pointer left, etc.)
    {
        let state = Rc::clone(&state);
        let progress_weak = progress_weak.clone();

        gesture.connect_cancel(move |_, _| {
            if state.is_confirming.get() {
                state.cancel();

                if let Some(progress) = progress_weak.upgrade() {
                    progress.remove_css_class(qs::POWER_CONFIRMING);
                    if let Some(parent) = progress.parent() {
                        parent.remove_css_class(qs::POWER_CONFIRMING);
                    }
                    progress.set_size_request(0, -1);
                }
            }
        });
    }

    gesture_widget.add_controller(gesture);
}

/// Create a card-like button with hold-to-confirm overlay.
///
/// Returns (overlay_container, progress_box) where:
/// - overlay_container should be used as the card widget
/// - progress_box is the progress overlay for CSS animation
fn create_hold_button_card(
    icon_name: &str,
    label_text: &str,
    subtitle_text: &str,
) -> (Overlay, GtkBox, Button, IconHandle, Option<Label>) {
    let overlay = Overlay::new();
    overlay.add_css_class(card::QS);
    overlay.add_css_class(card::BASE);
    overlay.add_css_class(qs::POWER_CARD);
    overlay.set_hexpand(true);

    // Progress bar (behind content, animates width)
    let progress = GtkBox::new(Orientation::Horizontal, 0);
    progress.add_css_class(qs::POWER_PROGRESS);
    progress.set_halign(Align::Start);
    progress.set_valign(Align::Fill);
    progress.set_vexpand(true);
    overlay.set_child(Some(&progress));

    // Button with card content (on top)
    let button = Button::new();
    button.set_hexpand(true);
    button.set_vexpand(true);
    button.set_halign(Align::Fill);
    button.set_valign(Align::Fill);
    button.add_css_class(button::RESET);

    let content = GtkBox::new(Orientation::Horizontal, 6);
    content.set_hexpand(true);
    content.set_margin_start(8);
    content.set_margin_end(8);
    content.set_margin_top(8);
    content.set_margin_bottom(8);

    // Icon
    let icons = IconsService::global();
    let icon_handle = icons.create_icon(icon_name, &[qs::TOGGLE_ICON, color::PRIMARY]);
    content.append(&icon_handle.widget());

    // Label + subtitle
    let label_result = CardLabel::new(label_text)
        .subtitle(subtitle_text)
        .width_chars(16)
        .title_class(qs::TOGGLE_LABEL)
        .subtitle_class(qs::TOGGLE_SUBTITLE)
        .build();
    content.append(&label_result.container);

    button.set_child(Some(&content));
    overlay.add_overlay(&button);

    (
        overlay,
        progress,
        button,
        icon_handle,
        label_result.subtitle,
    )
}

/// State for the Power card (shared between both variants).
pub struct PowerCardState {
    /// Card icon handle.
    pub card_icon: RefCell<Option<IconHandle>>,
    /// Card subtitle label.
    pub subtitle: RefCell<Option<Label>>,
}

impl PowerCardState {
    pub fn new() -> Self {
        Self {
            card_icon: RefCell::new(None),
            subtitle: RefCell::new(None),
        }
    }
}

impl Default for PowerCardState {
    fn default() -> Self {
        Self::new()
    }
}

/// State for the Power card (expander variant).
pub struct PowerCardExpanderState {
    /// Base expandable card state.
    pub base: ExpandableCardBase,
}

impl PowerCardExpanderState {
    pub fn new() -> Self {
        Self {
            base: ExpandableCardBase::new(),
        }
    }
}

impl Default for PowerCardExpanderState {
    fn default() -> Self {
        Self::new()
    }
}

impl ExpandableCard for PowerCardExpanderState {
    fn base(&self) -> &ExpandableCardBase {
        &self.base
    }
}

/// Build power card with popover menu (hold-to-open).
pub fn build_power_card_popover() -> (gtk4::Widget, Rc<PowerCardState>) {
    let state = Rc::new(PowerCardState::new());

    // Create the card wrapper box
    let card_box = GtkBox::new(Orientation::Horizontal, 0);
    card_box.set_hexpand(true);

    // Create hold button card
    let (overlay, progress, button, icon_handle, subtitle) =
        create_hold_button_card("system-shutdown-symbolic", "Power", "Hold to open");

    // Store references
    *state.card_icon.borrow_mut() = Some(icon_handle);
    *state.subtitle.borrow_mut() = subtitle;

    // Set up hold-to-confirm that opens popover
    let button_weak = button.downgrade();
    setup_hold_to_confirm(&button, &button, &progress, move || {
        let Some(btn) = button_weak.upgrade() else {
            return;
        };
        show_power_popover(&btn);
    });

    card_box.append(&overlay);
    (card_box.upcast(), state)
}

/// Show the power actions popover.
fn show_power_popover(parent: &Button) {
    let popover = Popover::new();
    configure_popover(&popover);

    let content = GtkBox::new(Orientation::Vertical, 2);
    content.add_css_class(qs::ROW_MENU_CONTENT);
    content.set_margin_top(4);
    content.set_margin_bottom(4);
    content.set_margin_start(4);
    content.set_margin_end(4);

    // Add a hold-to-confirm button for each power action
    for action in POWER_ACTIONS {
        let action_widget = create_power_popover_action(action);
        content.append(&action_widget);
    }

    popover.set_child(Some(&content));
    popover.set_parent(parent);
    popover.popup();

    // Unparent popover when closed
    popover.connect_closed(|p| {
        p.unparent();
    });
}

/// Create a power action button for the popover (with hold-to-confirm).
fn create_power_popover_action(action: &'static PowerAction) -> Overlay {
    let overlay = Overlay::new();
    overlay.add_css_class(qs::POWER_ROW);

    // Progress overlay
    let progress = GtkBox::new(Orientation::Horizontal, 0);
    progress.add_css_class(qs::POWER_PROGRESS);
    progress.set_halign(Align::Start);
    progress.set_valign(Align::Fill);
    progress.set_vexpand(true);
    overlay.set_child(Some(&progress));

    // Action button
    let btn = Button::new();
    btn.set_has_frame(false);
    btn.add_css_class(qs::ROW_MENU_ITEM);
    btn.add_css_class(button::GHOST);

    let hbox = GtkBox::new(Orientation::Horizontal, 8);
    hbox.set_margin_start(4);
    hbox.set_margin_end(8);

    // Icon
    let icons = IconsService::global();
    let icon = icons.create_icon(action.icon, &[row::QS_ICON, color::PRIMARY]);
    hbox.append(&icon.widget());

    // Label
    let label = Label::new(Some(action.label));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.add_css_class(color::PRIMARY);
    hbox.append(&label);

    btn.set_child(Some(&hbox));
    overlay.add_overlay(&btn);

    // Set up hold-to-confirm
    setup_hold_to_confirm(&btn, &btn, &progress, move || {
        execute_power_action(action);
    });

    overlay
}

/// Build power card with expander and ListRows.
///
/// - Toggle button: Hold-to-confirm for Shutdown (default action)
/// - Chevron button: Caller is responsible for setting up click handler
/// - Rows: Hold-to-confirm for each action
///
/// Returns `(card, revealer, state, expander_button)` - caller is responsible for
/// setting up the expander button click handler via `AccordionManager::setup_expander_with_callback`,
/// which handles accordion behavior, revealer toggling, and arrow CSS updates.
/// The caller can provide an `on_toggle` callback to update the subtitle text.
pub fn build_power_card_expander() -> (
    gtk4::Widget,
    Revealer,
    Rc<PowerCardExpanderState>,
    Option<Button>,
) {
    let state = Rc::new(PowerCardExpanderState::new());

    // Build the card using ToggleCard pattern
    let card = ToggleCard::builder()
        .icon("system-shutdown-symbolic")
        .label("Power")
        .subtitle("Hold to shutdown")
        .active(false)
        .sensitive(true)
        .icon_active(false)
        .with_expander(true)
        .with_ripple(false)
        .build();

    // Store base references
    *state.base.card_icon.borrow_mut() = Some(card.icon_handle.clone());
    *state.base.subtitle.borrow_mut() = card.subtitle.clone();
    *state.base.arrow.borrow_mut() = card.expander_icon.clone();

    // Build revealer with power action rows
    let revealer = Revealer::new();
    revealer.set_reveal_child(false);
    revealer.set_transition_type(RevealerTransitionType::SlideDown);

    let details = build_power_details();
    revealer.set_child(Some(&details.container));

    *state.base.revealer.borrow_mut() = Some(revealer.clone());
    *state.base.list_box.borrow_mut() = Some(details.list_box);

    // Create an overlay wrapper for the entire card (for hold-to-confirm progress)
    let card_overlay = Overlay::new();
    card_overlay.add_css_class(qs::POWER_CARD);
    card_overlay.set_hexpand(false); // Don't expand beyond card content

    // Progress bar as base child (behind)
    let progress = GtkBox::new(Orientation::Horizontal, 0);
    progress.add_css_class(qs::POWER_PROGRESS);
    progress.set_halign(Align::Start);
    progress.set_valign(Align::Fill);
    progress.set_vexpand(true);

    // Progress as base child, card content as overlay (text visible above progress)
    card_overlay.set_child(Some(&progress));
    card_overlay.add_overlay(&card.card);
    card_overlay.set_measure_overlay(&card.card, true);

    // Set up TOGGLE BUTTON: Hold-to-confirm for Shutdown (default action)
    // Gesture on toggle button, but width calculated from card_overlay (full card width)
    {
        let shutdown_action = &POWER_ACTIONS[0]; // Shutdown action (index 0)
        setup_hold_to_confirm(&card.toggle, &card_overlay, &progress, move || {
            execute_power_action(shutdown_action);
        });
    }

    // NOTE: Chevron button click handler is NOT connected here.
    // The caller must set up the handler to ensure proper accordion behavior
    // (collapse_others must be called BEFORE toggling the revealer).

    // Wrap overlay in a box to return (matching expected return type)
    let wrapper = GtkBox::new(Orientation::Horizontal, 0);
    wrapper.append(&card_overlay);
    // Don't hexpand the wrapper - let it size from the card content

    (wrapper.upcast(), revealer, state, card.expander_button)
}

/// Result of building power details section.
struct PowerDetailsResult {
    container: GtkBox,
    list_box: ListBox,
}

/// Build the power details section with action rows.
fn build_power_details() -> PowerDetailsResult {
    let container = GtkBox::new(Orientation::Vertical, 0);
    container.add_css_class(qs::POWER_DETAILS);

    let list_box = create_qs_list_box();

    // Add a row for each power action
    for action in POWER_ACTIONS {
        let row = build_power_action_row(action);
        list_box.append(&row);
    }

    container.append(&list_box);

    PowerDetailsResult {
        container,
        list_box,
    }
}

/// Build a power action row with hold-to-confirm.
fn build_power_action_row(action: &'static PowerAction) -> ListBoxRow {
    let list_row = ListBoxRow::new();
    list_row.add_css_class(row::QS);
    list_row.add_css_class(row::BASE);
    list_row.add_css_class(qs::POWER_ROW);
    list_row.set_activatable(false); // We handle activation via hold

    // Overlay structure: progress as base child (behind), content as overlay (on top)
    let overlay = Overlay::new();
    overlay.set_hexpand(true);
    overlay.set_vexpand(true);

    // Progress bar as base child (behind)
    let progress = GtkBox::new(Orientation::Horizontal, 0);
    progress.add_css_class(qs::POWER_PROGRESS);
    progress.set_halign(Align::Start);
    progress.set_valign(Align::Fill);
    progress.set_vexpand(true);
    overlay.set_child(Some(&progress));

    // Row content as overlay (text visible above progress)
    let hbox = GtkBox::new(Orientation::Horizontal, 6);
    hbox.add_css_class(row::QS_CONTENT);
    hbox.add_css_class(qs::POWER_ROW_CONTENT);
    hbox.set_hexpand(true);
    hbox.set_vexpand(true);

    // Icon
    let icons = IconsService::global();
    let icon = icons.create_icon(action.icon, &[row::QS_ICON, color::PRIMARY]);
    hbox.append(&icon.widget());

    // Title using CardLabel for consistency
    let label_result = CardLabel::new(action.label)
        .width_chars(22)
        .title_class(row::QS_TITLE)
        .build();
    hbox.append(&label_result.container);

    // Add overlay first, then set measure (must be in this order)
    overlay.add_overlay(&hbox);
    overlay.set_measure_overlay(&hbox, true);

    list_row.set_child(Some(&overlay));

    // Set up hold-to-confirm on the row
    setup_hold_to_confirm(&list_row, &list_row, &progress, move || {
        execute_power_action(action);
    });

    list_row
}

/// Build result for power card.
#[allow(dead_code)]
pub enum PowerCardBuildResult {
    /// Popover variant result.
    Popover {
        card: gtk4::Widget,
        state: Rc<PowerCardState>,
    },
    /// Expander variant result.
    Expander {
        card: gtk4::Widget,
        revealer: Revealer,
        state: Rc<PowerCardExpanderState>,
        /// Expander button for accordion registration (if caller wants to add accordion behavior)
        expander_button: Option<Button>,
    },
}

/// Build the power card using the configured variant.
///
/// For the Expander variant, returns an expander_button that can be used to set up
/// accordion behavior. Note that the Power card already handles its own expand/collapse
/// with subtitle updates, so the caller should use a custom accordion setup that
/// calls `accordion.collapse_others()` before the card's built-in handler runs.
pub fn build_power_card() -> PowerCardBuildResult {
    if USE_POPOVER_VARIANT {
        let (card, state) = build_power_card_popover();
        PowerCardBuildResult::Popover { card, state }
    } else {
        let (card, revealer, state, expander_button) = build_power_card_expander();
        PowerCardBuildResult::Expander {
            card,
            revealer,
            state,
            expander_button,
        }
    }
}
