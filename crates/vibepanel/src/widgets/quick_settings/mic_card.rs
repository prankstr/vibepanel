//! Microphone card for Quick Settings panel.
//!
//! This module contains:
//! - Mic icon helpers (mic_icon_name)
//! - Mic row building (mute button, slider, expander)
//! - Mic details (source list)
//! - State change handling

use std::cell::{Cell, RefCell};

use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, Button, Label, ListBox, ListBoxRow, Orientation, Revealer,
    RevealerTransitionType, Scale,
};

use super::components::SliderRow;
use super::ui_helpers::{
    add_placeholder_row, audio_input_icon_name, clear_list_box, create_device_row,
    create_qs_list_box, device_subtitle,
};
use crate::services::audio::{AudioService, AudioSnapshot, SourceInfoSnapshot};
use crate::services::config_manager::ConfigManager;
use crate::services::icons::IconHandle;
use crate::services::surfaces::SurfaceStyleManager;
use crate::styles::{color, qs, state};

/// Get the appropriate mic icon name based on volume level and mute state.
pub fn mic_icon_name(volume: u32, muted: bool) -> &'static str {
    if muted {
        return "microphone-sensitivity-muted-symbolic";
    }
    if volume >= 66 {
        return "microphone-sensitivity-high-symbolic";
    }
    if volume >= 33 {
        return "microphone-sensitivity-medium-symbolic";
    }
    if volume >= 1 {
        return "microphone-sensitivity-low-symbolic";
    }
    "microphone-sensitivity-muted-symbolic"
}

/// State for the Mic card in the Quick Settings panel.
pub struct MicCardState {
    /// Mic mute button.
    pub mute_button: RefCell<Option<Button>>,
    /// Mic volume icon handle.
    pub icon_handle: RefCell<Option<IconHandle>>,
    /// Mic volume slider.
    pub slider: RefCell<Option<Scale>>,
    /// Mic expander arrow icon handle.
    pub arrow: RefCell<Option<IconHandle>>,
    /// Mic details revealer.
    pub revealer: RefCell<Option<Revealer>>,
    /// Mic source list box.
    pub list_box: RefCell<Option<ListBox>>,
    /// Last source list rendered into `list_box`.
    source_list_snapshot: RefCell<Option<Vec<SourceInfoSnapshot>>>,
    /// Flag to prevent slider feedback loop.
    pub updating: Cell<bool>,
    /// Mic row container (for CSS class toggling).
    pub row: RefCell<Option<GtkBox>>,
    /// Hint label shown when mic control is unavailable.
    pub hint_label: RefCell<Option<Label>>,
}

impl MicCardState {
    pub fn new() -> Self {
        Self {
            mute_button: RefCell::new(None),
            icon_handle: RefCell::new(None),
            slider: RefCell::new(None),
            arrow: RefCell::new(None),
            revealer: RefCell::new(None),
            list_box: RefCell::new(None),
            source_list_snapshot: RefCell::new(None),
            updating: Cell::new(false),
            row: RefCell::new(None),
            hint_label: RefCell::new(None),
        }
    }

    pub fn invalidate_list_cache(&self) {
        *self.source_list_snapshot.borrow_mut() = None;
    }
}

impl Default for MicCardState {
    fn default() -> Self {
        Self::new()
    }
}

/// Container for mic row widgets.
pub struct MicRowWidgets {
    /// The outer row container.
    pub row: GtkBox,
    /// The mute toggle button.
    pub mute_button: Button,
    /// Handle to the mic icon.
    pub icon_handle: IconHandle,
    /// The volume slider.
    pub slider: Scale,
    /// The expander button for source list.
    pub expander_button: Button,
    /// Handle to the expander arrow icon.
    pub arrow_handle: IconHandle,
}

/// Build the mic row with mute button, volume slider, and expander.
///
/// Uses `SliderRow` for consistent styling with other slider rows.
pub fn build_mic_row() -> MicRowWidgets {
    let result = SliderRow::builder()
        .icon("microphone-sensitivity-high-symbolic")
        .interactive_icon(true) // Mute button is clickable
        // The slider is an interactive control, so keep its range capped to
        // what Vibepanel is allowed to request. Programmatic updates are
        // guarded to avoid writing external over-cap values back to Pulse.
        .range(0.0, AudioService::global().user_max_percent() as f64)
        .step(1.0)
        .with_expander(true) // Source list expander
        .build();

    MicRowWidgets {
        row: result.container,
        mute_button: result.icon_button,
        icon_handle: result.icon_handle,
        slider: result.slider,
        expander_button: result.expander_button.expect("expander requested"),
        arrow_handle: result.expander_icon.expect("expander requested"),
    }
}

/// Update the slider from the backend state without causing write-back.
///
/// External mic volume can exceed Vibepanel's configured cap, but this is an
/// interactive control: keep the range capped to the values Vibepanel may
/// request. GTK will visually saturate over-cap values at the maximum, while
/// the tooltip preserves the true backend volume.
pub fn set_mic_slider_display(slider: &Scale, volume: u32) {
    let max_percent = AudioService::global().user_max_percent().max(1);
    slider.set_range(0.0, max_percent as f64);
    slider.set_value(volume as f64);
    slider.set_tooltip_text(Some(&format!("{volume}%")));
}

/// Container for mic details (source list) widgets.
pub struct MicDetailsWidgets {
    /// The revealer for accordion behavior.
    pub revealer: Revealer,
    /// The list box for sources.
    pub list_box: ListBox,
}

/// Build the mic details section with source list.
///
/// # CSS Classes Applied
///
/// - `.qs-audio-details` on the container (reusing audio styling)
/// - `.qs-section-header` on the header
/// - `.qs-list` on the list box
pub fn build_mic_details() -> MicDetailsWidgets {
    let container = GtkBox::new(Orientation::Vertical, 8);
    container.add_css_class(qs::AUDIO_DETAILS);

    // Section header
    let header = Label::new(Some("Input Devices"));
    header.set_xalign(0.0);
    header.add_css_class(qs::SECTION_HEADER);
    container.append(&header);

    // Source list
    let list_box = create_qs_list_box();
    container.append(&list_box);

    // Wrap in revealer
    let revealer = Revealer::new();
    revealer.set_transition_type(RevealerTransitionType::SlideDown);
    revealer.set_transition_duration(ConfigManager::global().animation_duration(200));
    revealer.set_reveal_child(false);
    revealer.set_child(Some(&container));

    MicDetailsWidgets { revealer, list_box }
}

/// Create a hint label for when mic control is unavailable.
pub fn build_mic_hint_label() -> Label {
    let label = Label::new(Some("Microphone unavailable. Check your audio settings."));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_max_width_chars(40);
    label.add_css_class(qs::MUTED_LABEL);
    label.add_css_class(qs::AUDIO_HINT);
    label.add_css_class(color::MUTED);
    label
}

/// Populate the mic source list with available sources.
///
/// Sources with unavailable ports are omitted.
fn populate_mic_source_list(list_box: &ListBox, sources: &[SourceInfoSnapshot]) {
    clear_list_box(list_box);

    if sources.is_empty() {
        add_placeholder_row(list_box, "No input devices");
        return;
    }

    // Count how many sources are actually available
    let available_count = sources
        .iter()
        .filter(|s| s.port_available != Some(false))
        .count();

    // If all sources are unavailable, show a message
    if available_count == 0 {
        add_placeholder_row(list_box, "No input devices available");
        return;
    }

    for source in sources {
        // Skip sources with unavailable ports entirely
        if source.port_available == Some(false) {
            continue;
        }

        let subtitle = device_subtitle(
            &source.description,
            source.port_description.as_deref(),
            source.form_factor.as_deref(),
        );
        let icon_name = audio_input_icon_name(source.form_factor.as_deref());
        let row = create_device_row(
            &source.description,
            subtitle.as_deref(),
            Some(icon_name),
            source.is_default,
        );
        list_box.append(&row);
    }
}

pub fn sync_mic_source_list(
    state: &MicCardState,
    list_box: &ListBox,
    sources: &[SourceInfoSnapshot],
) -> bool {
    if state.source_list_snapshot.borrow().as_deref() == Some(sources) {
        return false;
    }

    populate_mic_source_list(list_box, sources);
    *state.source_list_snapshot.borrow_mut() = Some(sources.to_vec());
    true
}

/// Handle Audio state changes from AudioService (mic-related fields).
pub fn on_mic_changed(state: &MicCardState, snapshot: &AudioSnapshot) {
    let mic_volume = snapshot.mic_volume.unwrap_or(0);
    let mic_muted = snapshot.mic_muted.unwrap_or(false);
    let control_ok = snapshot.available && snapshot.mic_control_available;

    // Update volume slider (with flag to prevent feedback loop)
    if let Some(slider) = state.slider.borrow().as_ref() {
        state.updating.set(true);
        set_mic_slider_display(slider, mic_volume);
        slider.set_sensitive(control_ok);
        state.updating.set(false);
    }

    // Update mute button sensitivity
    if let Some(mute_btn) = state.mute_button.borrow().as_ref() {
        mute_btn.set_sensitive(control_ok);
    }

    // Update mic row disabled styling
    if let Some(mic_row) = state.row.borrow().as_ref() {
        if control_ok {
            mic_row.remove_css_class(qs::AUDIO_ROW_DISABLED);
        } else {
            mic_row.add_css_class(qs::AUDIO_ROW_DISABLED);
        }
    }

    // Update hint label visibility (show when backend available but control is not)
    if let Some(hint_label) = state.hint_label.borrow().as_ref() {
        let should_show = snapshot.available && !snapshot.mic_control_available;
        hint_label.set_visible(should_show);
    }

    // Update mic icon based on volume and mute state
    if let Some(icon_handle) = state.icon_handle.borrow().as_ref() {
        let icon_name = mic_icon_name(mic_volume, mic_muted);
        icon_handle.set_icon(icon_name);

        // Toggle muted class for styling
        let widget = icon_handle.widget();
        if mic_muted {
            widget.add_css_class(state::MUTED);
        } else {
            widget.remove_css_class(state::MUTED);
        }
    }

    // Update source list only when device inputs change.
    if let Some(list_box) = state.list_box.borrow().as_ref()
        && sync_mic_source_list(state, list_box, &snapshot.sources)
    {
        SurfaceStyleManager::global().apply_pango_attrs_all(list_box);
    }
}

/// Handle mic source row activation.
pub fn on_mic_source_row_activated(row: &ListBoxRow, sources: &[SourceInfoSnapshot]) {
    // Get the row index and look up the source
    let index = row.index();
    if index < 0 {
        return;
    }

    // The row index corresponds to the Nth *available* source (since we skip unavailable ones)
    // Filter to only available sources and get the one at the requested index
    let available_sources: Vec<_> = sources
        .iter()
        .filter(|s| s.port_available != Some(false))
        .collect();

    if let Some(source) = available_sources.get(index as usize) {
        AudioService::global().set_default_source(&source.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalidating_mic_list_cache_clears_rendered_inputs() {
        let state = MicCardState::new();
        *state.source_list_snapshot.borrow_mut() = Some(Vec::new());

        state.invalidate_list_cache();

        assert!(state.source_list_snapshot.borrow().is_none());
    }
}
