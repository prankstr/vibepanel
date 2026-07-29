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
    Align, Box as GtkBox, Button, CenterBox, EventControllerScroll, EventControllerScrollFlags,
    Image, Label, ListBox, ListBoxRow, Orientation, Revealer, RevealerTransitionType, Scale,
};

use super::components::SliderRow;
use super::ui_helpers::{
    add_placeholder_row, audio_output_icon_name, clear_list_box, create_device_row,
    create_qs_list_box, device_subtitle,
};
use crate::services::audio::{AppVolumeSnapshot, AudioService, AudioSnapshot, SinkInfoSnapshot};
use crate::services::config_manager::ConfigManager;
use crate::services::icons::{IconHandle, IconsService, resolve_app_icon_name};
use crate::services::surfaces::SurfaceStyleManager;
use crate::styles::{color, qs, row, state};
use crate::widgets::base::vp_button;

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
    /// Last sink-list inputs rendered into `list_box`.
    sink_list_snapshot: RefCell<Option<(bool, Vec<SinkInfoSnapshot>)>>,
    /// Application volume list box.
    pub app_list_box: RefCell<Option<ListBox>>,
    /// Last application-list inputs rendered; `None` means the list has not rendered yet.
    app_volume_list_key: RefCell<Option<AppVolumeListKey>>,
    /// Current application volume row widgets, aligned with the rendered stream keys.
    app_volume_rows: RefCell<Vec<AppVolumeRowState>>,
    /// Flag to prevent slider feedback loop.
    pub updating: Cell<bool>,
    /// Audio row container (for CSS class toggling).
    pub row: RefCell<Option<GtkBox>>,
    /// Hint label shown when audio control is unavailable.
    pub hint_label: RefCell<Option<Label>>,
    /// Volume delta used for application slider scroll.
    pub app_scroll_step: Cell<i32>,
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
            sink_list_snapshot: RefCell::new(None),
            app_list_box: RefCell::new(None),
            app_volume_list_key: RefCell::new(None),
            app_volume_rows: RefCell::new(Vec::new()),
            updating: Cell::new(false),
            row: RefCell::new(None),
            hint_label: RefCell::new(None),
            app_scroll_step: Cell::new(5),
        }
    }

    pub fn invalidate_list_caches(&self) {
        *self.sink_list_snapshot.borrow_mut() = None;
        *self.app_volume_list_key.borrow_mut() = None;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppVolumeKey {
    index: u32,
    app_name: String,
    app_id: String,
    app_icon_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppVolumeListKey {
    available: bool,
    streams: Vec<AppVolumeKey>,
}

impl From<&AppVolumeSnapshot> for AppVolumeKey {
    fn from(stream: &AppVolumeSnapshot) -> Self {
        Self {
            index: stream.index,
            app_name: stream.app_name.clone(),
            app_id: stream.app_id.clone(),
            app_icon_name: stream.app_icon_name.clone(),
        }
    }
}

struct AppVolumeRowState {
    row: ListBoxRow,
    index: u32,
    description_label: Label,
    value_label: Label,
    slider: Scale,
    mute_button: Button,
    mute_icon: IconHandle,
    updating: Rc<Cell<bool>>,
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
        // The slider is an interactive control, so keep its range capped to
        // what Vibepanel is allowed to request. Programmatic updates are
        // guarded to avoid writing external over-cap values back to Pulse.
        .range(0.0, AudioService::global().user_max_percent() as f64)
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

/// Update the slider from the backend state without causing write-back.
///
/// External volume can exceed Vibepanel's configured cap, but this is an
/// interactive control: keep the range capped to the values Vibepanel may
/// request. GTK will visually saturate over-cap values at the maximum, while
/// the tooltip preserves the true backend volume.
pub fn set_volume_slider_display(slider: &Scale, volume: u32) {
    let max_percent = AudioService::global().user_max_percent().max(1);
    slider.set_range(0.0, max_percent as f64);
    slider.set_value(volume as f64);
    slider.set_tooltip_text(Some(&format!("{volume}%")));
}

/// Container for audio details (sink list) widgets.
pub struct AudioDetailsWidgets {
    /// The revealer for accordion behavior.
    pub revealer: Revealer,
    /// The list box for sinks.
    pub list_box: ListBox,
    /// The list box for application streams.
    pub app_list_box: ListBox,
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
    let header = Label::new(Some("Output Devices"));
    header.set_xalign(0.0);
    header.add_css_class(qs::SECTION_HEADER);
    container.append(&header);

    // Sink list
    let list_box = create_qs_list_box();
    container.append(&list_box);

    let app_header = Label::new(Some("Applications"));
    app_header.set_xalign(0.0);
    app_header.add_css_class(qs::SECTION_HEADER);
    container.append(&app_header);

    let app_list_box = create_qs_list_box();
    app_list_box.add_css_class(qs::APP_VOLUME_LIST);
    container.append(&app_list_box);

    // Wrap in revealer
    let revealer = Revealer::new();
    revealer.set_transition_type(RevealerTransitionType::SlideDown);
    revealer.set_transition_duration(ConfigManager::global().animation_duration(200));
    revealer.set_reveal_child(false);
    revealer.set_child(Some(&container));

    AudioDetailsWidgets {
        revealer,
        list_box,
        app_list_box,
    }
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

/// Populate the audio sink list with available sinks.
///
/// Sinks with unavailable ports (e.g., headphones not plugged in) are omitted.
fn populate_audio_sink_list(list_box: &ListBox, snapshot: &AudioSnapshot) {
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

        let subtitle = device_subtitle(
            &sink.description,
            sink.port_description.as_deref(),
            sink.form_factor.as_deref(),
        );
        let icon_name = audio_output_icon_name(
            sink.device_icon_name.as_deref(),
            sink.form_factor.as_deref(),
        );
        let row = create_device_row(
            &sink.description,
            subtitle.as_deref(),
            Some(icon_name),
            sink.is_default,
        );
        list_box.append(&row);
    }
}

pub fn sync_audio_sink_list(
    state: &AudioCardState,
    list_box: &ListBox,
    snapshot: &AudioSnapshot,
) -> bool {
    let unchanged = state
        .sink_list_snapshot
        .borrow()
        .as_ref()
        .is_some_and(|(available, sinks)| {
            *available == snapshot.available && sinks == &snapshot.sinks
        });
    if unchanged {
        return false;
    }

    populate_audio_sink_list(list_box, snapshot);
    *state.sink_list_snapshot.borrow_mut() = Some((snapshot.available, snapshot.sinks.clone()));
    true
}

/// Populate application stream volume controls.
fn populate_app_volume_list(state: &AudioCardState, list_box: &ListBox, snapshot: &AudioSnapshot) {
    clear_list_box(list_box);
    state.app_volume_rows.borrow_mut().clear();
    *state.app_volume_list_key.borrow_mut() = Some(app_volume_list_key(snapshot));

    if !snapshot.available {
        add_placeholder_row(list_box, "Audio unavailable");
        return;
    }

    if snapshot.app_volumes.is_empty() {
        add_placeholder_row(list_box, "No application audio");
        return;
    }

    let mut rows = Vec::with_capacity(snapshot.app_volumes.len());
    for stream in &snapshot.app_volumes {
        let row = create_app_volume_row(stream, state.app_scroll_step.get());
        list_box.append(&row.row);
        rows.push(row);
    }
    *state.app_volume_rows.borrow_mut() = rows;
}

pub fn sync_app_volume_list(
    state: &AudioCardState,
    list_box: &ListBox,
    snapshot: &AudioSnapshot,
) -> bool {
    let key = app_volume_list_key(snapshot);
    if state.app_volume_list_key.borrow().as_ref() != Some(&key) {
        populate_app_volume_list(state, list_box, snapshot);
        return true;
    }

    let rows = state.app_volume_rows.borrow();
    for (row, stream) in rows.iter().zip(&snapshot.app_volumes) {
        row.update(stream);
    }
    false
}

fn app_volume_list_key(snapshot: &AudioSnapshot) -> AppVolumeListKey {
    AppVolumeListKey {
        available: snapshot.available,
        streams: if snapshot.available {
            snapshot
                .app_volumes
                .iter()
                .map(AppVolumeKey::from)
                .collect()
        } else {
            Vec::new()
        },
    }
}

fn create_app_volume_row(stream: &AppVolumeSnapshot, scroll_step: i32) -> AppVolumeRowState {
    let list_row = ListBoxRow::new();
    list_row.add_css_class(row::QS);
    list_row.add_css_class(row::BASE);
    list_row.add_css_class(qs::APP_VOLUME_ROW);
    list_row.set_activatable(false);
    list_row.set_focusable(false);

    let outer = GtkBox::new(Orientation::Horizontal, 0);
    outer.add_css_class(row::QS_CONTENT);
    outer.set_margin_top(6);
    outer.set_margin_bottom(6);
    outer.set_margin_start(10);
    outer.set_margin_end(10);

    append_app_volume_icon(&outer, stream);

    let content = GtkBox::new(Orientation::Vertical, 2);
    content.set_hexpand(true);

    let top_row = GtkBox::new(Orientation::Horizontal, 6);

    let title_label = Label::new(Some(&stream.app_name));
    title_label.add_css_class(qs::APP_VOLUME_TITLE);
    title_label.add_css_class(color::PRIMARY);
    title_label.set_xalign(0.0);
    title_label.set_ellipsize(EllipsizeMode::End);
    title_label.set_single_line_mode(true);
    title_label.set_tooltip_text(Some(&stream.app_name));
    top_row.append(&title_label);

    let description_label = Label::new(None);
    description_label.add_css_class(qs::APP_VOLUME_DESCRIPTION);
    description_label.add_css_class(color::MUTED);
    description_label.set_xalign(0.0);
    description_label.set_hexpand(true);
    description_label.set_ellipsize(EllipsizeMode::End);
    description_label.set_single_line_mode(true);
    top_row.append(&description_label);

    let value_label = Label::new(None);
    value_label.add_css_class(qs::APP_VOLUME_VALUE);
    value_label.set_halign(Align::End);
    value_label.add_css_class(color::PRIMARY);
    top_row.append(&value_label);

    content.append(&top_row);

    let slider = Scale::with_range(
        Orientation::Horizontal,
        0.0,
        AudioService::global().user_max_percent() as f64,
        1.0,
    );
    slider.add_css_class(qs::APP_VOLUME_SLIDER);
    slider.set_draw_value(false);
    slider.set_hexpand(true);
    slider.set_round_digits(0);
    attach_app_volume_scroll_controller(&slider, scroll_step);

    let index = stream.index;
    let updating = Rc::new(Cell::new(false));
    let updating_for_slider = Rc::clone(&updating);
    slider.connect_value_changed(move |slider| {
        if updating_for_slider.get() {
            return;
        }
        AudioService::global().set_app_volume(index, slider.value().round() as u32);
    });

    content.append(&slider);
    outer.append(&content);
    let (mute_button, mute_icon) = build_app_volume_mute_button(stream.index);
    outer.append(&mute_button);
    list_row.set_child(Some(&outer));

    let row = AppVolumeRowState {
        row: list_row,
        index: stream.index,
        description_label,
        value_label,
        slider,
        mute_button,
        mute_icon,
        updating,
    };
    row.update(stream);
    row
}

impl AppVolumeRowState {
    fn update(&self, stream: &AppVolumeSnapshot) {
        debug_assert_eq!(self.index, stream.index);
        let description = format!("- {}", stream.media_description);
        if self.description_label.text() != description {
            self.description_label.set_text(&description);
            self.description_label
                .set_tooltip_text(Some(&stream.media_description));
        }
        let tooltip = format!("{}%", stream.volume);
        self.value_label.set_text(&tooltip);
        self.value_label.set_tooltip_text(Some(&tooltip));
        self.updating.set(true);
        self.slider
            .set_range(0.0, AudioService::global().user_max_percent().max(1) as f64);
        self.slider.set_value(stream.volume as f64);
        self.slider.set_tooltip_text(Some(&tooltip));
        self.updating.set(false);

        let icon_name = volume_icon_name(stream.volume, stream.muted);
        self.mute_icon.set_icon(icon_name);
        let icon = self.mute_icon.widget();
        if stream.muted {
            icon.add_css_class(state::MUTED);
        } else {
            icon.remove_css_class(state::MUTED);
        }
        let mute_tooltip = if stream.muted { "Unmute" } else { "Mute" };
        if self.mute_button.tooltip_text().as_deref() != Some(mute_tooltip) {
            self.mute_button.set_tooltip_text(Some(mute_tooltip));
        }
    }
}

fn build_app_volume_mute_button(index: u32) -> (Button, IconHandle) {
    let button = vp_button();
    button.set_has_frame(false);
    button.add_css_class(qs::APP_VOLUME_MUTE);
    button.set_valign(Align::Center);

    let icon_handle =
        IconsService::global().create_icon("audio-volume-muted-symbolic", &[color::PRIMARY]);
    icon_handle.widget().set_halign(Align::Center);
    icon_handle.widget().set_valign(Align::Center);
    button.set_child(Some(&icon_handle.widget()));

    button.connect_clicked(move |_| {
        AudioService::global().toggle_app_mute(index);
    });

    (button, icon_handle)
}

fn append_app_volume_icon(container: &GtkBox, stream: &AppVolumeSnapshot) {
    let icon_slot = CenterBox::new();
    icon_slot.add_css_class(qs::APP_VOLUME_ICON_SLOT);
    icon_slot.set_halign(Align::Start);
    icon_slot.set_valign(Align::Center);
    icon_slot.set_hexpand(false);

    if let Some(icon_name) = resolve_stream_icon_name(stream) {
        let app_icon = Image::from_icon_name(&icon_name);
        app_icon.add_css_class(qs::APP_VOLUME_ICON);
        let icon_size = ConfigManager::global().theme_sizes().pixmap_icon_size as i32;
        app_icon.set_pixel_size((icon_size as f32 * 1.25).round() as i32);
        app_icon.set_halign(Align::Center);
        app_icon.set_valign(Align::Center);
        icon_slot.set_center_widget(Some(&app_icon));
        container.append(&icon_slot);
        return;
    }

    let fallback = IconsService::global().create_icon(
        "audio-speakers-symbolic",
        &[qs::APP_VOLUME_ICON, color::PRIMARY],
    );
    fallback.add_css_class(qs::APP_VOLUME_ICON_FALLBACK);
    fallback.widget().set_valign(Align::Center);
    fallback.widget().set_halign(Align::Center);
    fallback.widget().set_hexpand(false);
    icon_slot.set_center_widget(Some(&fallback.widget()));
    container.append(&icon_slot);
}

fn resolve_stream_icon_name(stream: &AppVolumeSnapshot) -> Option<String> {
    for candidate in [
        stream.app_icon_name.as_str(),
        stream.app_id.as_str(),
        stream.app_name.as_str(),
    ] {
        let candidate = candidate.trim();
        if candidate.is_empty() || candidate.eq_ignore_ascii_case("application") {
            continue;
        }

        let icon_name = resolve_app_icon_name(candidate, "");
        if !icon_name.is_empty() {
            return Some(icon_name);
        }
    }

    None
}

/// Handle Audio state changes from AudioService.
pub fn on_audio_changed(state: &AudioCardState, snapshot: &AudioSnapshot) {
    let control_ok = snapshot.available && snapshot.control_available;

    // Update volume slider (with flag to prevent feedback loop)
    if let Some(slider) = state.slider.borrow().as_ref() {
        state.updating.set(true);
        set_volume_slider_display(slider, snapshot.volume);
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

    // Update sink list only when device inputs change.
    if let Some(list_box) = state.list_box.borrow().as_ref()
        && sync_audio_sink_list(state, list_box, snapshot)
    {
        SurfaceStyleManager::global().apply_pango_attrs_all(list_box);
    }

    if let Some(app_list_box) = state.app_list_box.borrow().as_ref()
        && sync_app_volume_list(state, app_list_box, snapshot)
    {
        SurfaceStyleManager::global().apply_pango_attrs_all(app_list_box);
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
    attach_volume_scroll_controller_inner(
        widget,
        step,
        move || {
            let snapshot = AudioService::global().current();
            if !snapshot.available || !snapshot.control_available {
                return false;
            }

            true
        },
        |direction, step| {
            AudioService::global().set_volume_relative(direction * step);
        },
    );
}

fn attach_app_volume_scroll_controller(slider: &Scale, step: i32) {
    let slider_for_apply = slider.downgrade();

    attach_volume_scroll_controller_inner(
        slider,
        step,
        || true,
        move |direction, step| {
            let Some(slider) = slider_for_apply.upgrade() else {
                return;
            };
            let adjustment = slider.adjustment();
            let value = (slider.value() + f64::from(direction * step))
                .clamp(adjustment.lower(), adjustment.upper());
            slider.set_value(value);
        },
    );
}

fn attach_volume_scroll_controller_inner(
    widget: &impl IsA<gtk4::Widget>,
    step: i32,
    can_scroll: impl Fn() -> bool + 'static,
    apply_step: impl Fn(i32, i32) + 'static,
) {
    let scroll = EventControllerScroll::new(EventControllerScrollFlags::VERTICAL);
    scroll.set_propagation_phase(gtk4::PropagationPhase::Capture);

    let accumulated = Rc::new(Cell::new(0.0f64));

    scroll.connect_scroll(move |_controller, _dx, dy| {
        if !can_scroll() {
            accumulated.set(0.0);
            return gtk4::glib::Propagation::Proceed;
        }

        let mut acc = accumulated.get();

        // Reset accumulator on direction change to avoid a "dead zone"
        // when reversing scroll direction.
        if (acc > 0.0 && dy < 0.0) || (acc < 0.0 && dy > 0.0) {
            acc = 0.0;
        }

        acc += dy;
        let step = step.abs();

        while acc.abs() >= 1.0 {
            let direction = if acc < 0.0 { 1 } else { -1 };
            apply_step(direction, step);
            acc -= acc.signum();
        }

        accumulated.set(acc);
        gtk4::glib::Propagation::Stop
    });

    widget.add_controller(scroll);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(
        volume: u32,
        muted: bool,
        media_description: &str,
        channel_count: u8,
    ) -> AppVolumeSnapshot {
        AppVolumeSnapshot {
            index: 7,
            app_name: "App".to_string(),
            app_id: "app".to_string(),
            app_icon_name: "app-icon".to_string(),
            media_description: media_description.to_string(),
            volume,
            muted,
            channel_count,
        }
    }

    fn snapshot(app_volumes: Vec<AppVolumeSnapshot>) -> AudioSnapshot {
        AudioSnapshot {
            available: true,
            app_volumes,
            ..Default::default()
        }
    }

    #[test]
    fn app_volume_list_key_ignores_dynamic_stream_fields() {
        let first = snapshot(vec![stream(25, false, "Playback", 2)]);
        let second = snapshot(vec![stream(80, true, "Next track", 6)]);

        assert_eq!(app_volume_list_key(&first), app_volume_list_key(&second));
    }

    #[test]
    fn app_volume_list_key_distinguishes_unavailable_from_available_empty() {
        let unavailable = AudioSnapshot::default();
        let available = snapshot(Vec::new());

        assert_ne!(
            app_volume_list_key(&unavailable),
            app_volume_list_key(&available)
        );
    }

    #[test]
    fn app_volume_list_key_changes_when_streams_appear() {
        let empty = snapshot(Vec::new());
        let populated = snapshot(vec![stream(25, false, "Playback", 2)]);

        assert_ne!(app_volume_list_key(&empty), app_volume_list_key(&populated));
    }

    #[test]
    fn uninitialized_app_volume_list_differs_from_rendered_unavailable() {
        let state = AudioCardState::new();
        let unavailable_key = app_volume_list_key(&AudioSnapshot::default());

        assert_ne!(
            state.app_volume_list_key.borrow().as_ref(),
            Some(&unavailable_key)
        );

        *state.app_volume_list_key.borrow_mut() = Some(unavailable_key.clone());
        assert_eq!(
            state.app_volume_list_key.borrow().as_ref(),
            Some(&unavailable_key)
        );
    }

    #[test]
    fn invalidating_audio_list_caches_clears_rendered_inputs() {
        let state = AudioCardState::new();
        *state.sink_list_snapshot.borrow_mut() = Some((true, Vec::new()));
        *state.app_volume_list_key.borrow_mut() = Some(AppVolumeListKey {
            available: true,
            streams: Vec::new(),
        });

        state.invalidate_list_caches();

        assert!(state.sink_list_snapshot.borrow().is_none());
        assert!(state.app_volume_list_key.borrow().is_none());
    }
}
