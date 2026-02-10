//! Audio card for Quick Settings panel.
//!
//! This module contains:
//! - Audio icon helpers (volume_icon_name)
//! - Audio row building (mute button, slider, expander)
//! - Audio details (sink list)
//! - State change handling

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::pango::EllipsizeMode;
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, EventControllerScroll, EventControllerScrollFlags, Label,
    ListBox, ListBoxRow, Orientation, Overlay, Revealer, RevealerTransitionType, Scale,
};

use super::components::SliderRow;
use super::ui_helpers::{add_placeholder_row, clear_list_box, create_qs_list_box};
use crate::services::audio::{AudioService, AudioSnapshot};
use crate::services::icons::{IconHandle, IconsService};
use crate::services::surfaces::SurfaceStyleManager;
use crate::styles::{color, qs, row, state};

/// Get the appropriate volume icon name based on volume level and mute state.
///
/// Uses standard GTK/Adwaita icon names.
pub fn volume_icon_name(volume: u32, muted: bool) -> &'static str {
    if muted {
        return "audio-volume-muted-symbolic";
    }
    if volume >= 66 {
        return "audio-volume-high-symbolic";
    }
    if volume >= 33 {
        return "audio-volume-medium-symbolic";
    }
    if volume >= 1 {
        return "audio-volume-low-symbolic";
    }
    "audio-volume-muted-symbolic"
}

/// State for the Audio card in the Quick Settings panel.
pub struct AudioCardState {
    /// Audio mute button.
    pub mute_button: RefCell<Option<Button>>,
    /// Audio volume icon handle.
    pub icon_handle: RefCell<Option<IconHandle>>,
    /// Audio volume slider.
    pub slider: RefCell<Option<Scale>>,
    /// Audio expander arrow icon handle.
    pub arrow: RefCell<Option<IconHandle>>,
    /// Audio details revealer.
    pub revealer: RefCell<Option<Revealer>>,
    /// Audio sink list box.
    pub list_box: RefCell<Option<ListBox>>,
    /// Flag to prevent slider feedback loop.
    pub updating: Cell<bool>,
    /// Audio row container (for CSS class toggling).
    pub row: RefCell<Option<GtkBox>>,
    /// Hint label shown when audio control is unavailable.
    pub hint_label: RefCell<Option<Label>>,
}

impl AudioCardState {
    pub fn new() -> Self {
        Self {
            mute_button: RefCell::new(None),
            icon_handle: RefCell::new(None),
            slider: RefCell::new(None),
            arrow: RefCell::new(None),
            revealer: RefCell::new(None),
            list_box: RefCell::new(None),
            updating: Cell::new(false),
            row: RefCell::new(None),
            hint_label: RefCell::new(None),
        }
    }
}

impl Default for AudioCardState {
    fn default() -> Self {
        Self::new()
    }
}

/// Container for audio row widgets.
pub struct AudioRowWidgets {
    /// The outer row container.
    pub row: GtkBox,
    /// The mute toggle button.
    pub mute_button: Button,
    /// Handle to the volume icon.
    pub icon_handle: IconHandle,
    /// The volume slider.
    pub slider: Scale,
    /// The expander button for sink list.
    pub expander_button: Button,
    /// Handle to the expander arrow icon.
    pub arrow_handle: IconHandle,
}

/// Build the audio row with mute button, volume slider, and expander.
///
/// Uses `SliderRow` for consistent styling with other slider rows.
pub fn build_audio_row() -> AudioRowWidgets {
    let result = SliderRow::builder()
        .icon("audio-volume-high-symbolic")
        .interactive_icon(true) // Mute button is clickable
        .range(0.0, 100.0)
        .step(1.0)
        .with_expander(true) // Sink list expander
        .build();

    AudioRowWidgets {
        row: result.container,
        mute_button: result.icon_button,
        icon_handle: result.icon_handle,
        slider: result.slider,
        expander_button: result.expander_button.expect("expander requested"),
        arrow_handle: result.expander_icon.expect("expander requested"),
    }
}

/// Container for audio details (sink list) widgets.
pub struct AudioDetailsWidgets {
    /// The revealer for accordion behavior.
    pub revealer: Revealer,
    /// The list box for sinks.
    pub list_box: ListBox,
}

/// Build the audio details section with sink list.
///
/// # CSS Classes Applied
///
/// - `.qs-audio-details` on the container
/// - `.qs-section-header` on the header
/// - `.qs-list` on the list box
pub fn build_audio_details() -> AudioDetailsWidgets {
    let container = GtkBox::new(Orientation::Vertical, 8);
    container.add_css_class(qs::AUDIO_DETAILS);

    // Section header
    let header = Label::new(Some("Sound"));
    header.set_xalign(0.0);
    header.add_css_class(qs::SECTION_HEADER);
    container.append(&header);

    // Sink list
    let list_box = create_qs_list_box();
    container.append(&list_box);

    // Wrap in revealer
    let revealer = Revealer::new();
    revealer.set_transition_type(RevealerTransitionType::SlideDown);
    revealer.set_transition_duration(200);
    revealer.set_reveal_child(false);
    revealer.set_child(Some(&container));

    AudioDetailsWidgets { revealer, list_box }
}

/// Create a hint label for when audio control is unavailable.
pub fn build_audio_hint_label() -> Label {
    let label = Label::new(Some(
        "Audio sink suspended. Play audio to enable volume control.",
    ));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_max_width_chars(40);
    label.add_css_class(qs::MUTED_LABEL);
    label.add_css_class(qs::AUDIO_HINT);
    label.add_css_class(color::MUTED);
    label
}

/// Create a sink row for the audio sink list.
///
/// # Arguments
///
/// - `description`: The human-readable sink description.
/// - `is_default`: Whether this sink is the current default.
/// - `port_available`: Whether the sink's port is available (e.g., headphones plugged in).
///   `None` means no jack detection, `Some(false)` means unavailable.
pub fn create_sink_row(
    description: &str,
    is_default: bool,
    port_available: Option<bool>,
) -> ListBoxRow {
    let list_row = ListBoxRow::new();
    list_row.add_css_class(row::QS);
    list_row.add_css_class(row::BASE);

    // Check if port is unavailable (explicitly false, not unknown/None)
    let is_unavailable = port_available == Some(false);

    let hbox = GtkBox::new(Orientation::Horizontal, 6);
    hbox.add_css_class(row::QS_CONTENT);

    // Description label
    let label = Label::new(Some(description));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_ellipsize(EllipsizeMode::End);
    label.set_single_line_mode(true);
    label.set_width_chars(22);
    label.set_max_width_chars(22);
    label.add_css_class(row::QS_TITLE);
    label.add_css_class(color::PRIMARY);
    hbox.append(&label);

    // Selection indicator
    if is_default {
        // Overlay: background box + checkmark icon floating on top
        let overlay = Overlay::new();
        overlay.set_valign(Align::Center);

        // Background box (same size as unselected indicator)
        let bg = GtkBox::new(Orientation::Horizontal, 0);
        bg.add_css_class(row::QS_INDICATOR_BG);
        overlay.set_child(Some(&bg));

        // Checkmark icon (larger, overflows the background)
        let icons = IconsService::global();
        let indicator = icons.create_icon("object-select-symbolic", &[row::QS_INDICATOR]);
        indicator.widget().set_halign(Align::Center);
        indicator.widget().set_valign(Align::Center);
        overlay.add_overlay(&indicator.widget());

        hbox.append(&overlay);
    } else {
        // CSS-styled box for unselected (respects --radius-pill)
        let indicator = GtkBox::new(Orientation::Horizontal, 0);
        indicator.add_css_class(row::QS_RADIO_INDICATOR);
        hbox.append(&indicator);
    }

    list_row.set_child(Some(&hbox));

    // If port is unavailable, gray out the row and make it non-activatable
    if is_unavailable {
        list_row.set_activatable(false);
        list_row.set_focusable(false);
        list_row.set_sensitive(false);
    } else {
        list_row.set_activatable(true);
        list_row.set_focusable(true);
    }

    list_row
}

/// Populate the audio sink list with available sinks.
///
/// Sinks with unavailable ports (e.g., headphones not plugged in) are shown
/// but grayed out and non-selectable.
pub fn populate_audio_sink_list(list_box: &ListBox, snapshot: &AudioSnapshot) {
    clear_list_box(list_box);

    if !snapshot.available {
        add_placeholder_row(list_box, "Audio unavailable");
        return;
    }

    if snapshot.sinks.is_empty() {
        add_placeholder_row(list_box, "No audio devices");
        return;
    }

    // Count how many sinks are actually available
    let available_count = snapshot
        .sinks
        .iter()
        .filter(|s| s.port_available != Some(false))
        .count();

    // If all sinks are unavailable, show a message
    if available_count == 0 {
        add_placeholder_row(list_box, "No audio devices available");
        return;
    }

    for sink in &snapshot.sinks {
        // Skip sinks with unavailable ports entirely - they clutter the UI
        // and can't be selected anyway
        if sink.port_available == Some(false) {
            continue;
        }

        let row = create_sink_row(&sink.description, sink.is_default, sink.port_available);
        list_box.append(&row);
    }
}

/// Handle Audio state changes from AudioService.
pub fn on_audio_changed(state: &AudioCardState, snapshot: &AudioSnapshot) {
    let control_ok = snapshot.available && snapshot.control_available;

    // Update volume slider (with flag to prevent feedback loop)
    if let Some(slider) = state.slider.borrow().as_ref() {
        state.updating.set(true);
        slider.set_value(snapshot.volume as f64);
        slider.set_sensitive(control_ok);
        state.updating.set(false);
    }

    // Update mute button sensitivity
    if let Some(mute_btn) = state.mute_button.borrow().as_ref() {
        mute_btn.set_sensitive(control_ok);
    }

    // Update audio row disabled styling
    if let Some(audio_row) = state.row.borrow().as_ref() {
        if control_ok {
            audio_row.remove_css_class(qs::AUDIO_ROW_DISABLED);
        } else {
            audio_row.add_css_class(qs::AUDIO_ROW_DISABLED);
        }
    }

    // Update hint label visibility (show when backend available but control is not)
    if let Some(hint_label) = state.hint_label.borrow().as_ref() {
        let should_show = snapshot.available && !snapshot.control_available;
        hint_label.set_visible(should_show);
    }

    // Update volume icon based on volume and mute state
    if let Some(icon_handle) = state.icon_handle.borrow().as_ref() {
        let icon_name = volume_icon_name(snapshot.volume, snapshot.muted);
        icon_handle.set_icon(icon_name);

        // Toggle muted class for styling
        let widget = icon_handle.widget();
        if snapshot.muted {
            widget.add_css_class(state::MUTED);
        } else {
            widget.remove_css_class(state::MUTED);
        }
    }

    // Update sink list
    if let Some(list_box) = state.list_box.borrow().as_ref() {
        populate_audio_sink_list(list_box, snapshot);
        // Apply Pango font attrs to dynamically created list rows
        SurfaceStyleManager::global().apply_pango_attrs_all(list_box);
    }
}

/// Handle audio sink row activation.
pub fn on_audio_sink_row_activated(row: &ListBoxRow) {
    // Get the row index and look up the sink in the current snapshot
    let index = row.index();
    if index < 0 {
        return;
    }

    let audio = AudioService::global();
    let snapshot = audio.current();

    // The row index corresponds to the Nth *available* sink (since we skip unavailable ones)
    // Filter to only available sinks and get the one at the requested index
    let available_sinks: Vec<_> = snapshot
        .sinks
        .iter()
        .filter(|s| s.port_available != Some(false))
        .collect();

    if let Some(sink) = available_sinks.get(index as usize) {
        audio.set_default_sink(&sink.name);
    }
}

/// Attach an `EventControllerScroll` that adjusts volume on vertical scroll.
///
/// Each full scroll tick changes volume by `step` percentage points.
/// Fractional scroll events (e.g. from touchpads) are accumulated so that
/// volume only changes once a full tick is reached. The accumulator resets
/// on direction change so that reversing scroll direction feels responsive.
pub fn attach_volume_scroll_controller(widget: &impl IsA<gtk4::Widget>, step: i32) {
    let scroll = EventControllerScroll::new(EventControllerScrollFlags::VERTICAL);
    scroll.set_propagation_phase(gtk4::PropagationPhase::Capture);

    let accumulated = Rc::new(Cell::new(0.0f64));

    scroll.connect_scroll(move |_controller, _dx, dy| {
        let snapshot = AudioService::global().current();
        if !snapshot.available || !snapshot.control_available {
            return gtk4::glib::Propagation::Proceed;
        }

        let mut acc = accumulated.get();

        // Reset accumulator on direction change to avoid a "dead zone"
        // when reversing scroll direction.
        if (acc > 0.0 && dy < 0.0) || (acc < 0.0 && dy > 0.0) {
            acc = 0.0;
        }

        acc += dy;

        while acc.abs() >= 1.0 {
            let direction = if acc < 0.0 { 1 } else { -1 };
            AudioService::global().set_volume_relative(direction * step);
            acc -= acc.signum();
        }

        accumulated.set(acc);
        gtk4::glib::Propagation::Stop
    });

    widget.add_controller(scroll);
}
