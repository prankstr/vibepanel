//! Quick Settings UI helpers - shared card/row builders.
//!
//! Provides reusable UI builders for the quick settings control center panels.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::services::icons::{CairoSpinner, IconHandle, IconsService};
use crate::styles::{button, color, icon, qs, row, state};
use crate::widgets::base::add_ripple_to_row;
use crate::widgets::layer_shell_popover::animate_reveal;
use gtk4::pango::EllipsizeMode;
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, Label, ListBox, ListBoxRow, Orientation, Overlay, Popover,
    Revealer, SelectionMode, ToggleButton,
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

pub fn device_subtitle(
    description: &str,
    port_description: Option<&str>,
    form_factor: Option<&str>,
) -> Option<String> {
    port_description
        .and_then(|subtitle| meaningful_device_subtitle(description, subtitle))
        .map(str::to_owned)
        .or_else(|| {
            form_factor
                .and_then(|factor| meaningful_device_subtitle(description, factor))
                .map(title_case_device_hint)
        })
}

pub fn audio_output_icon_name(
    device_icon_name: Option<&str>,
    form_factor: Option<&str>,
) -> &'static str {
    form_factor
        .and_then(audio_form_factor_icon)
        .or_else(|| device_icon_name.and_then(audio_icon_family))
        .unwrap_or("audio-card-symbolic")
}

pub fn audio_input_icon_name(form_factor: Option<&str>) -> &'static str {
    match form_factor.map(str::trim).map(str::to_ascii_lowercase) {
        Some(form_factor)
            if matches!(form_factor.as_str(), "headset" | "hands-free" | "handsfree") =>
        {
            "audio-headset"
        }
        _ => "audio-input-microphone-symbolic",
    }
}

fn audio_icon_family(icon_name: &str) -> Option<&'static str> {
    let icon_name = icon_name.trim();

    [
        ("audio-headphones", "audio-headphones"),
        ("audio-headset", "audio-headset"),
        ("audio-speakers", "audio-speakers"),
        ("audio-card", "audio-card-symbolic"),
    ]
    .into_iter()
    .find_map(|(family, canonical)| {
        icon_name
            .strip_prefix(family)
            .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with('-'))
            .then_some(canonical)
    })
}

fn audio_form_factor_icon(form_factor: &str) -> Option<&'static str> {
    match form_factor.trim().to_ascii_lowercase().as_str() {
        "headphone" | "headphones" => Some("audio-headphones"),
        "headset" | "hands-free" | "handsfree" => Some("audio-headset"),
        "speaker" | "speakers" => Some("audio-speakers"),
        _ => None,
    }
}

pub(super) fn create_device_row(
    description: &str,
    subtitle: Option<&str>,
    icon_name: Option<&str>,
    is_default: bool,
) -> ListBoxRow {
    let list_row = ListBoxRow::new();
    list_row.add_css_class(row::QS);
    list_row.add_css_class(row::BASE);

    let content = GtkBox::new(Orientation::Horizontal, 6);
    content.add_css_class(row::QS_CONTENT);

    if let Some(icon_name) = icon_name {
        let icon_handle = IconsService::global()
            .create_icon(icon_name, &[icon::TEXT, row::QS_ICON, color::PRIMARY]);
        icon_handle.widget().set_valign(Align::Center);
        content.append(&icon_handle.widget());
    }

    let labels = GtkBox::new(Orientation::Vertical, 2);
    labels.set_hexpand(true);

    let title = Label::new(Some(description));
    title.set_xalign(0.0);
    title.set_ellipsize(EllipsizeMode::End);
    title.set_single_line_mode(true);
    title.set_width_chars(22);
    title.set_max_width_chars(22);
    title.add_css_class(row::QS_TITLE);
    title.add_css_class(color::PRIMARY);
    labels.append(&title);

    if let Some(subtitle) = subtitle {
        let label = Label::new(Some(subtitle));
        label.set_xalign(0.0);
        label.set_ellipsize(EllipsizeMode::End);
        label.set_single_line_mode(true);
        label.add_css_class(row::QS_SUBTITLE);
        label.add_css_class(color::MUTED);
        labels.append(&label);
    }

    content.append(&labels);

    if is_default {
        let overlay = Overlay::new();
        overlay.set_valign(Align::Center);
        overlay.set_margin_end(8);

        let background = GtkBox::new(Orientation::Horizontal, 0);
        background.add_css_class(row::QS_INDICATOR_BG);
        overlay.set_child(Some(&background));

        let indicator =
            IconsService::global().create_icon("object-select-symbolic", &[row::QS_INDICATOR]);
        indicator.widget().set_halign(Align::Center);
        indicator.widget().set_valign(Align::Center);
        overlay.add_overlay(&indicator.widget());
        content.append(&overlay);
    } else {
        let indicator = GtkBox::new(Orientation::Horizontal, 0);
        indicator.set_valign(Align::Center);
        indicator.set_margin_end(8);
        indicator.add_css_class(row::QS_RADIO_INDICATOR);
        content.append(&indicator);
    }

    content.set_margin_top(6);
    content.set_margin_bottom(6);
    content.set_margin_start(10);
    content.set_margin_end(10);
    add_ripple_to_row(&list_row, &content);
    list_row.set_activatable(true);
    list_row.set_focusable(true);

    list_row
}

fn meaningful_device_subtitle<'a>(description: &str, subtitle: &'a str) -> Option<&'a str> {
    let subtitle = subtitle.trim();
    if subtitle.is_empty() || subtitle.eq_ignore_ascii_case(description.trim()) {
        return None;
    }

    Some(subtitle)
}

fn title_case_device_hint(value: &str) -> String {
    value
        .split(['-', '_', ' '])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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

            animate_reveal(revealer, expanding);

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
/// Returns the button and its icon handle. The icon handle can be used to
/// toggle the `expanded` CSS class for rotation animation when the popover
/// opens and closes.
///
/// # CSS Classes Applied
///
/// - `.qs-row-menu-button` and `.vp-btn-reset` on the button
/// - `.qs-row-menu-icon` on the menu icon
pub fn create_row_menu_button() -> (Button, IconHandle) {
    let menu_btn = crate::widgets::base::vp_button();
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

    (menu_btn, icon_handle)
}

/// Close and detach a row menu before its parent button can be removed.
pub fn close_row_menu_popover(popover: &Popover) {
    if popover.parent().is_none() {
        return;
    }

    popover.popdown();
    if popover.parent().is_some() {
        popover.unparent();
    }
}

/// Attach and present a row menu with cleanup for dynamic list rebuilds.
pub fn present_row_menu_popover(
    popover: &Popover,
    button: &Button,
    menu_icon: &impl IsA<gtk4::Widget>,
) {
    popover.set_parent(button);

    let menu_icon = menu_icon.as_ref();
    menu_icon.add_css_class(state::EXPANDED);
    let unmap_handler = Rc::new(RefCell::new(None));
    let popover_weak = popover.downgrade();
    let id = button.connect_unmap(move |_| {
        if let Some(popover) = popover_weak.upgrade() {
            close_row_menu_popover(&popover);
        }
    });
    *unmap_handler.borrow_mut() = Some(id);

    let icon_for_close = menu_icon.clone();
    let button_weak = button.downgrade();
    let unmap_handler_for_close = Rc::clone(&unmap_handler);
    popover.connect_closed(move |popover| {
        if let Some(button) = button_weak.upgrade()
            && let Some(id) = unmap_handler_for_close.borrow_mut().take()
        {
            button.disconnect(id);
        }
        icon_for_close.remove_css_class(state::EXPANDED);
        if popover.parent().is_some() {
            popover.unparent();
        }
    });

    popover.popup();
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
    let btn = crate::widgets::base::vp_button_with_label(label_text);
    btn.set_has_frame(false);
    btn.set_valign(gtk4::Align::Center);
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
    let btn = crate::widgets::base::vp_button();
    btn.set_has_frame(false);
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
    /// Whether the button had keyboard-navigated focus when it was made
    /// insensitive (i.e. `focus_visible` was active on the window).
    had_keyboard_focus: Cell<bool>,
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
        let button = crate::widgets::base::vp_button();
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
            had_keyboard_focus: Cell::new(false),
        })
    }

    /// Get the button widget for adding to a container.
    pub fn widget(&self) -> &Button {
        &self.button
    }

    /// Set button sensitivity.
    ///
    /// When becoming insensitive, records whether the button had
    /// keyboard-navigated focus (i.e. `focus_visible` was active) so it
    /// can be restored when scanning ends.
    pub fn set_sensitive(&self, sensitive: bool) {
        if !sensitive && self.button.has_focus() {
            // Record that we had focus *and* that keyboard nav was active.
            // We check focus_visible now because GTK may clear it once the
            // focused widget becomes insensitive.
            let kbd_nav = self
                .button
                .root()
                .and_then(|r| r.downcast::<gtk4::Window>().ok())
                .is_some_and(|w| gtk4::prelude::GtkWindowExt::gets_focus_visible(&w));
            self.had_keyboard_focus.set(kbd_nav);
        }
        self.button.set_sensitive(sensitive);
    }

    /// Set button visibility.
    pub fn set_visible(&self, visible: bool) {
        self.button.set_visible(visible);
    }

    /// Update active/scanning state.
    ///
    /// When `active` is true, hides label and shows spinner.  When false,
    /// hides spinner and shows idle text.  If the button had
    /// keyboard-navigated focus before scanning started, focus is restored
    /// automatically and `focus_visible` is re-enabled on the window.
    pub fn set_scanning(&self, active: bool) {
        if active {
            self.label.set_visible(false);
            self.spinner.start();
        } else {
            self.spinner.stop();
            self.label.set_visible(true);
            // Restore focus if the button had keyboard-nav focus before it
            // was made insensitive.  We also re-enable focus_visible since
            // GTK may have cleared it when focus was lost.
            if self.had_keyboard_focus.replace(false) && self.button.is_sensitive() {
                if let Some(root) = self.button.root()
                    && let Some(window) = root.downcast_ref::<gtk4::Window>()
                {
                    gtk4::prelude::GtkWindowExt::set_focus_visible(window, true);
                }
                self.button.grab_focus();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{audio_input_icon_name, audio_output_icon_name, device_subtitle};

    #[test]
    fn device_subtitle_preserves_port_description_casing() {
        assert_eq!(
            device_subtitle("USB Audio", Some("HDMI / DisplayPort 2"), Some("speaker")),
            Some("HDMI / DisplayPort 2".to_string())
        );
    }

    #[test]
    fn device_subtitle_title_cases_form_factor_fallback() {
        assert_eq!(
            device_subtitle("USB Audio", None, Some("headset-microphone")),
            Some("Headset Microphone".to_string())
        );
    }

    #[test]
    fn audio_output_icon_normalizes_supported_device_hints() {
        assert_eq!(
            audio_output_icon_name(Some("audio-speakers-bluetooth"), None),
            "audio-speakers"
        );
        assert_eq!(
            audio_output_icon_name(Some("audio-card-analog-usb"), None),
            "audio-card-symbolic"
        );
        assert_eq!(
            audio_output_icon_name(Some("audio-headphones-symbolic"), None),
            "audio-headphones"
        );
    }

    #[test]
    fn audio_output_icon_uses_safe_fallbacks() {
        assert_eq!(
            audio_output_icon_name(Some("vendor-specific-device"), Some("headset")),
            "audio-headset"
        );
        assert_eq!(
            audio_output_icon_name(Some("audio-card-analog"), Some("headset")),
            "audio-headset"
        );
        assert_eq!(
            audio_output_icon_name(Some("vendor-specific-device"), Some("internal")),
            "audio-card-symbolic"
        );
    }

    #[test]
    fn audio_input_icon_distinguishes_headsets() {
        assert_eq!(audio_input_icon_name(Some("headset")), "audio-headset");
        assert_eq!(
            audio_input_icon_name(Some("microphone")),
            "audio-input-microphone-symbolic"
        );
        assert_eq!(
            audio_input_icon_name(None),
            "audio-input-microphone-symbolic"
        );
    }
}
