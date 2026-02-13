//! Quick Settings UI helpers - shared card/row builders.
//!
//! Provides reusable UI builders for the quick settings control center panels.

use std::cell::RefCell;
use std::rc::Rc;

use crate::services::icons::{CairoSpinner, IconHandle, IconsService};
use crate::styles::{button, color, qs, row, state};
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, Label, ListBox, ListBoxRow, Orientation, Revealer, SelectionMode,
    ToggleButton,
};

/// Base state for expandable cards (Wi-Fi, Bluetooth, VPN).
///
/// This struct contains the common fields shared by all expandable cards
/// in the Quick Settings panel. Card-specific state should be stored
/// separately and composed with this base.
#[derive(Default)]
pub struct ExpandableCardBase {
    /// The toggle button for power on/off.
    pub toggle: RefCell<Option<ToggleButton>>,
    /// The card icon handle for dynamic updates.
    pub card_icon: RefCell<Option<IconHandle>>,
    /// The subtitle label showing current status.
    pub subtitle: RefCell<Option<Label>>,
    /// The list box containing items (networks/devices/connections).
    pub list_box: RefCell<Option<ListBox>>,
    /// The revealer for accordion expand/collapse.
    pub revealer: RefCell<Option<Revealer>>,
    /// The arrow icon handle for expand indicator.
    pub arrow: RefCell<Option<IconHandle>>,
}

impl ExpandableCardBase {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Trait for expandable card state types.
///
/// This trait provides access to the common base fields and allows
/// the AccordionManager to work with different card types uniformly.
pub trait ExpandableCard {
    /// Get a reference to the base state.
    fn base(&self) -> &ExpandableCardBase;
}

/// Set the active state styling on an icon handle's backend widget.
///
/// When active, applies `qs-icon-active` and removes `vp-primary`.
/// When inactive, removes `qs-icon-active` and adds `vp-primary`.
///
/// This uses IconHandle's tracked CSS class methods so the state survives
/// theme switches (when the backend widget is recreated).
pub fn set_icon_active(icon_handle: &IconHandle, active: bool) {
    if active {
        icon_handle.add_css_class(state::ICON_ACTIVE);
        icon_handle.remove_css_class(color::PRIMARY);
    } else {
        icon_handle.remove_css_class(state::ICON_ACTIVE);
        icon_handle.add_css_class(color::PRIMARY);
    }
}

/// Set the active state styling on a subtitle label.
///
/// When active, applies `qs-subtitle-active` (accent color).
/// When inactive, removes `qs-subtitle-active`.
pub fn set_subtitle_active(label: &Label, active: bool) {
    if active {
        label.add_css_class(state::SUBTITLE_ACTIVE);
    } else {
        label.remove_css_class(state::SUBTITLE_ACTIVE);
    }
}

/// Build a subtitle with an error-colored primary word followed by muted dot-separated parts.
pub fn build_error_subtitle(error_word: &str, extra_parts: &[&str]) -> GtkBox {
    use gtk4::pango::EllipsizeMode;

    let hbox = GtkBox::new(Orientation::Horizontal, 0);

    // Primary word in error color
    let error_label = Label::new(Some(error_word));
    error_label.add_css_class(color::ERROR);
    error_label.add_css_class(row::QS_SUBTITLE);
    hbox.append(&error_label);

    // Remaining parts in muted color
    if !extra_parts.is_empty() {
        let rest = format!(" \u{2022} {}", extra_parts.join(" \u{2022} "));
        let rest_label = Label::new(Some(&rest));
        rest_label.add_css_class(color::MUTED);
        rest_label.add_css_class(row::QS_SUBTITLE);
        rest_label.set_ellipsize(EllipsizeMode::End);
        hbox.append(&rest_label);
    }

    hbox
}

/// Build a subtitle widget with an accent-colored primary word followed by muted parts.
///
/// Creates an HBox containing:
/// - Primary word label with accent color (e.g., "Connected", "Active")
/// - " · part1 · part2 · ..." label with muted color
///
/// Used for Wi-Fi, Ethernet, Bluetooth, and VPN rows.
pub fn build_accent_subtitle(accent_word: &str, extra_parts: &[&str]) -> GtkBox {
    use gtk4::pango::EllipsizeMode;

    let hbox = GtkBox::new(Orientation::Horizontal, 0);

    // Primary word in accent color
    let accent_label = Label::new(Some(accent_word));
    accent_label.add_css_class(color::ACCENT);
    accent_label.add_css_class(row::QS_SUBTITLE);
    hbox.append(&accent_label);

    // Remaining parts in muted color
    if !extra_parts.is_empty() {
        let rest = format!(" \u{2022} {}", extra_parts.join(" \u{2022} "));
        let rest_label = Label::new(Some(&rest));
        rest_label.add_css_class(color::MUTED);
        rest_label.add_css_class(row::QS_SUBTITLE);
        rest_label.set_ellipsize(EllipsizeMode::End);
        hbox.append(&rest_label);
    }

    hbox
}

/// Manages accordion behavior for expandable cards within a single row.
///
/// Each row of cards gets its own `AccordionManager` instance, so cards in
/// different rows are independent. When a card is expanded, all other cards
/// **in the same row** are collapsed instantly.
pub struct AccordionManager {
    /// Registered expandable cards (stored as trait objects).
    cards: RefCell<Vec<Rc<dyn ExpandableCard>>>,
}

impl AccordionManager {
    /// Create a new accordion manager.
    pub fn new() -> Self {
        Self {
            cards: RefCell::new(Vec::new()),
        }
    }

    /// Register an expandable card with the accordion.
    #[allow(dead_code)]
    pub fn register<T: ExpandableCard + 'static>(&self, card: Rc<T>) {
        self.cards.borrow_mut().push(card);
    }

    /// Register an expandable card trait object with the accordion.
    pub fn register_dyn(&self, card: Rc<dyn ExpandableCard>) {
        self.cards.borrow_mut().push(card);
    }

    /// Collapse all cards except the one with the given revealer.
    ///
    /// This should be called when a card is about to expand.
    pub fn collapse_others(&self, except_revealer: &Revealer) {
        for card in self.cards.borrow().iter() {
            let base = card.base();
            if let Some(revealer) = base.revealer.borrow().as_ref() {
                // Skip the card that's being expanded
                if revealer == except_revealer {
                    continue;
                }
                // Collapse this card if it's expanded
                if revealer.reveals_child() {
                    collapse_revealer_instant(revealer);
                    if let Some(arrow) = base.arrow.borrow().as_ref() {
                        arrow.widget().remove_css_class(state::EXPANDED);
                    }
                }
            }
        }
    }

    /// Set up accordion behavior for a card's expander button.
    ///
    /// This connects the expander button to toggle the revealer and
    /// automatically collapse other cards when expanding.
    #[allow(dead_code)]
    pub fn setup_expander<T: ExpandableCard + 'static>(
        accordion: &Rc<Self>,
        card: &Rc<T>,
        expander_btn: &Button,
    ) {
        Self::setup_expander_with_callback(
            accordion,
            &(Rc::clone(card) as Rc<dyn ExpandableCard>),
            expander_btn,
            None,
        );
    }

    /// Set up accordion behavior with an optional post-toggle callback.
    ///
    /// This is the more flexible version of `setup_expander` that accepts
    /// a callback invoked after the revealer and arrow state are updated.
    /// The callback receives `true` if expanding, `false` if collapsing.
    ///
    /// Use this for cards that need custom behavior on expand/collapse,
    /// such as updating a subtitle label.
    pub fn setup_expander_with_callback(
        accordion: &Rc<Self>,
        card: &Rc<dyn ExpandableCard>,
        expander_btn: &Button,
        on_toggle: Option<Rc<dyn Fn(bool)>>,
    ) {
        let accordion = Rc::clone(accordion);
        let revealer = card.base().revealer.borrow().clone();
        let arrow = card.base().arrow.borrow().clone();

        expander_btn.connect_clicked(move |_| {
            let Some(revealer) = revealer.as_ref() else {
                return;
            };

            let expanding = !revealer.reveals_child();

            // Collapse other cards first (accordion behavior)
            if expanding {
                accordion.collapse_others(revealer);
            }

            revealer.set_reveal_child(expanding);

            if let Some(ref arrow) = arrow {
                if expanding {
                    arrow.widget().add_css_class(state::EXPANDED);
                } else {
                    arrow.widget().remove_css_class(state::EXPANDED);
                }
            }

            // Invoke custom callback if provided
            if let Some(ref callback) = on_toggle {
                callback(expanding);
            }
        });
    }
}

impl Default for AccordionManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Add a placeholder row to a list box (e.g., "No networks found").
pub fn add_placeholder_row(list_box: &ListBox, text: &str) {
    let label = Label::new(Some(text));
    label.add_css_class(qs::MUTED_LABEL);
    label.add_css_class(color::MUTED);
    label.set_xalign(0.0);

    let list_row = ListBoxRow::new();
    list_row.set_child(Some(&label));
    list_row.set_activatable(false);
    list_box.append(&list_row);
}

/// Create a hamburger menu button for list rows with multiple actions.
///
/// # CSS Classes Applied
///
/// - `.qs-row-menu-button` and `.vp-btn-reset` on the button
/// - `.qs-row-menu-icon` on the menu icon
pub fn create_row_menu_button() -> Button {
    let menu_btn = Button::new();
    menu_btn.set_has_frame(false);
    menu_btn.add_css_class(row::QS_MENU_BUTTON);
    menu_btn.add_css_class(button::RESET);

    // Use IconsService so Material mapping is applied
    let icons = IconsService::global();
    let icon_handle = icons.create_icon("open-menu-symbolic", &[row::QS_MENU_ICON, color::PRIMARY]);

    // Center the icon within the button's hover area
    let icon_widget = icon_handle.widget();
    icon_widget.set_halign(gtk4::Align::Center);
    icon_widget.set_valign(gtk4::Align::Center);
    menu_btn.set_child(Some(&icon_widget));

    menu_btn
}

/// Create a single inline action as accent-colored text (no background).
///
/// Use this for rows with only one action (e.g., VPN Connect/Disconnect,
/// Wi-Fi Connect for unknown networks).
///
/// # CSS Classes Applied
///
/// - `.qs-row-action-label` on the button
pub fn create_row_action_label(label_text: &str) -> Button {
    let btn = Button::with_label(label_text);
    btn.set_has_frame(false);
    btn.add_css_class(row::QS_ACTION_LABEL);
    btn.add_css_class(color::ACCENT);
    btn
}

/// Create a menu action button for row context menus.
///
/// Use this inside popover menus created from `create_row_menu_button`.
/// The button has a left-aligned label and ghost styling.
///
/// # CSS Classes Applied
///
/// - `.qs-row-menu-item` and `.vp-btn-ghost` on the button
/// - `.vp-primary` on the label
pub fn create_row_menu_action<F>(label_text: &str, on_click: F) -> Button
where
    F: Fn() + 'static,
{
    let btn = Button::new();
    btn.set_has_frame(false);
    btn.set_focusable(false);
    btn.set_focus_on_click(false);
    btn.add_css_class(qs::ROW_MENU_ITEM);
    btn.add_css_class(button::GHOST);

    let lbl = Label::new(Some(label_text));
    lbl.set_xalign(0.0);
    lbl.add_css_class(color::PRIMARY);
    btn.set_child(Some(&lbl));

    btn.connect_clicked(move |_| {
        on_click();
    });

    btn
}

/// Collapse a revealer instantly without animation.
///
/// This temporarily sets the transition duration to 0, collapses the revealer,
/// then restores the original duration. Used for accordion behavior where
/// closing other panels should be instant while the active panel animates open.
pub fn collapse_revealer_instant(revealer: &Revealer) {
    if revealer.reveals_child() {
        let old_dur = revealer.transition_duration();
        revealer.set_transition_duration(0);
        revealer.set_reveal_child(false);
        revealer.set_transition_duration(old_dur);
    }
}

/// Clear all children from a ListBox.
pub fn clear_list_box(list_box: &ListBox) {
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }
}

/// Add a disabled state placeholder to a list box.
///
/// Creates a centered container with an icon and message label.
/// Used when a service is disabled (e.g., Wi-Fi off, Bluetooth powered off).
///
/// # Arguments
///
/// * `list_box` - The list box to add the placeholder to
/// * `icon_name` - The symbolic icon name (e.g., "bluetooth-disabled-symbolic")
/// * `message` - The message to display (e.g., "Bluetooth is disabled")
///
/// # CSS Classes Applied
///
/// - `.qs-disabled-state` on the container
/// - `.qs-disabled-state-icon` and `.vp-muted` on the icon
/// - `.qs-disabled-state-label` and `.vp-muted` on the label
pub fn add_disabled_placeholder(list_box: &ListBox, icon_name: &str, message: &str) {
    let icons = IconsService::global();

    let container = GtkBox::new(Orientation::Vertical, 6);
    container.add_css_class(qs::DISABLED_STATE);
    container.set_valign(Align::Center);
    container.set_halign(Align::Center);
    container.set_hexpand(true);

    // Icon
    let icon_handle = icons.create_icon(icon_name, &[qs::DISABLED_STATE_ICON, color::MUTED]);
    let icon_widget = icon_handle.widget();
    icon_widget.set_halign(Align::Center);
    container.append(&icon_widget);

    // Message
    let label = Label::new(Some(message));
    label.add_css_class(qs::DISABLED_STATE_LABEL);
    label.add_css_class(color::MUTED);
    label.set_halign(Align::Center);
    label.set_justify(gtk4::Justification::Center);
    container.append(&label);

    let row = ListBoxRow::new();
    row.set_child(Some(&container));
    row.set_activatable(false);
    list_box.append(&row);
}

/// Create a new ListBox configured for quick settings panels.
///
/// # CSS Classes Applied
///
/// - `.qs-list` on the list box
pub fn create_qs_list_box() -> ListBox {
    let list_box = ListBox::new();
    list_box.add_css_class(qs::LIST);
    list_box.set_selection_mode(SelectionMode::None);
    list_box
}

/// Self-contained scan button widget with spinner state.
///
/// This provides a consistent scan/refresh button used by Wi-Fi, Bluetooth,
/// and other cards. It handles:
/// - Button and label styling
/// - Spinner shown during active state (label hidden)
/// - Automatic state management
///
/// The spinner is a Cairo-drawn rotating arc that inherits the CSS foreground
/// color, providing a consistent appearance across all icon themes.
///
/// # CSS Classes Applied
///
/// - `.qs-scan-button` on the button
/// - `.qs-scan-label` and `.vp-primary` on the label
/// - `.qs-scan-spinner` on the spinner drawing area
pub struct ScanButton {
    button: Button,
    label: Label,
    spinner: CairoSpinner,
}

impl ScanButton {
    /// Create a new scan button with default label ("Scan").
    ///
    /// The `on_click` callback is invoked when the button is clicked.
    pub fn new<F>(on_click: F) -> Rc<Self>
    where
        F: Fn() + 'static,
    {
        Self::with_label("Scan", on_click)
    }

    /// Create a new scan button with custom label.
    ///
    /// - `label_text`: Label when not scanning (e.g., "Refresh")
    /// - `on_click`: Callback invoked when the button is clicked
    pub fn with_label<F>(label_text: &str, on_click: F) -> Rc<Self>
    where
        F: Fn() + 'static,
    {
        let button = Button::new();
        button.add_css_class(qs::SCAN_BUTTON);
        button.set_has_frame(false);
        button.set_halign(Align::Start);

        let content = GtkBox::new(Orientation::Horizontal, 6);

        let label = Label::new(Some(label_text));
        label.add_css_class(qs::SCAN_LABEL);
        label.add_css_class(color::PRIMARY);
        content.append(&label);

        // Shared Cairo spinner
        let spinner = CairoSpinner::new_self_colored();
        spinner.set_size(12);
        spinner.widget().add_css_class(qs::SCAN_SPINNER);

        content.append(spinner.widget());
        button.set_child(Some(&content));
        button.connect_clicked(move |_| on_click());

        Rc::new(Self {
            button,
            label,
            spinner,
        })
    }

    /// Get the button widget for adding to a container.
    pub fn widget(&self) -> &Button {
        &self.button
    }

    /// Set button sensitivity.
    pub fn set_sensitive(&self, sensitive: bool) {
        self.button.set_sensitive(sensitive);
    }

    /// Set button visibility.
    pub fn set_visible(&self, visible: bool) {
        self.button.set_visible(visible);
    }

    /// Update active/scanning state.
    ///
    /// When `active` is true, hides label and shows spinner.
    /// When false, hides spinner and shows idle text.
    pub fn set_scanning(&self, active: bool) {
        if active {
            self.label.set_visible(false);
            self.spinner.start();
        } else {
            self.spinner.stop();
            self.label.set_visible(true);
        }
    }
}
