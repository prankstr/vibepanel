//! Shared components for media widgets (popover and pop-out window).

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::gdk_pixbuf::Pixbuf;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, Button, EventControllerLegacy, Label, Orientation, Scale};
use tracing::debug;

use crate::services::config_manager::ConfigManager;
use crate::services::icons::{IconHandle, IconsService};
use crate::services::media::{MediaService, MediaSnapshot, PlaybackStatus, format_duration};
use crate::styles::{button, color, icon, media};
use crate::widgets::marquee_label::MarqueeLabel;
use crate::widgets::rounded_picture::RoundedPicture;

// ============================================================================
// Shared Controller
// ============================================================================

/// Shared controller for media UI views (popover and pop-out window).
///
/// Owns references to UI elements and provides a unified `update_from_snapshot()`
/// method to keep the view in sync with media state.
#[derive(Clone)]
pub struct MediaViewController {
    pub title_label: Rc<MarqueeLabel>,
    pub artist_label: Label,
    pub album_label: Label,
    pub art_picture: RoundedPicture,
    pub art_placeholder_box: GtkBox,
    pub art_state: Rc<RefCell<ArtState>>,
    pub play_pause_btn: Button,
    pub play_pause_icon: IconHandle,
    pub prev_btn: Button,
    pub next_btn: Button,
    pub seek_scale: Scale,
    pub position_label: Label,
    pub duration_label: Label,
    pub is_seeking: Rc<RefCell<bool>>,
}

impl MediaViewController {
    /// Update all UI elements from a media snapshot.
    pub fn update_from_snapshot(&self, snapshot: &MediaSnapshot) {
        update_track_info(
            &self.title_label,
            &self.artist_label,
            &self.album_label,
            snapshot,
        );
        load_album_art(
            snapshot.metadata.art_url.as_deref(),
            &self.art_picture,
            &self.art_placeholder_box,
            &self.art_state,
        );
        update_playback_controls(
            &self.play_pause_icon,
            &self.play_pause_btn,
            &self.prev_btn,
            &self.next_btn,
            &self.seek_scale,
            snapshot,
        );
        update_seek_position(
            &self.seek_scale,
            &self.position_label,
            &self.duration_label,
            &self.is_seeking,
            snapshot,
        );
    }
}

// ============================================================================
// Art State
// ============================================================================

/// State for tracking album art loading with cancellation support.
pub struct ArtState {
    pub current_url: Option<String>,
    pub generation: u64,
    pub cancellable: gio::Cancellable,
}

impl ArtState {
    pub fn new() -> Self {
        Self {
            current_url: None,
            generation: 0,
            cancellable: gio::Cancellable::new(),
        }
    }
}

impl Default for ArtState {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Create a simple media control button with an icon.
pub fn create_media_control_button<F>(
    icons: &IconsService,
    icon_name: &str,
    tooltip: &str,
    classes: &[&str],
    on_click: F,
) -> Button
where
    F: Fn() + 'static,
{
    let icon_handle = icons.create_icon(icon_name, &[icon::ICON]);
    let btn = Button::new();
    btn.set_has_frame(false);
    btn.set_valign(Align::Center);
    btn.set_child(Some(&icon_handle.widget()));
    for class in classes {
        btn.add_css_class(class);
    }
    btn.set_tooltip_text(Some(tooltip));
    btn.connect_clicked(move |_| on_click());
    btn
}

// ============================================================================
// Build Functions
// ============================================================================

/// Build media control buttons (prev, play/pause, next).
/// Returns (container, prev_btn, play_pause_btn, play_pause_icon, next_btn)
pub fn build_media_controls(
    extra_classes: &[&str],
) -> (GtkBox, Button, Button, IconHandle, Button) {
    let icons = IconsService::global();

    let container = GtkBox::new(Orientation::Horizontal, 8);
    container.add_css_class(media::CONTROLS);
    container.set_halign(Align::Center);

    // Previous button
    let prev_icon = icons.create_icon("skip_previous", &[icon::ICON]);
    prev_icon.widget().set_halign(Align::Center);
    prev_icon.widget().set_valign(Align::Center);
    let prev_btn = Button::new();
    prev_btn.set_child(Some(&prev_icon.widget()));
    prev_btn.add_css_class(media::CONTROL_BTN);
    prev_btn.add_css_class(button::COMPACT);
    for class in extra_classes {
        prev_btn.add_css_class(class);
    }
    prev_btn.set_tooltip_text(Some("Previous"));
    prev_btn.set_valign(Align::Center);
    prev_btn.connect_clicked(|_| MediaService::global().previous());
    container.append(&prev_btn);

    // Play/pause button
    let play_pause_icon =
        icons.create_icon("media-playback-start", &[icon::ICON, media::PRIMARY_ICON]);
    play_pause_icon.widget().set_halign(Align::Center);
    play_pause_icon.widget().set_valign(Align::Center);
    let play_pause_btn = Button::new();
    play_pause_btn.set_child(Some(&play_pause_icon.widget()));
    play_pause_btn.add_css_class(media::CONTROL_BTN);
    play_pause_btn.add_css_class(media::CONTROL_BTN_PRIMARY);
    play_pause_btn.add_css_class(button::COMPACT);
    for class in extra_classes {
        play_pause_btn.add_css_class(class);
    }
    play_pause_btn.set_tooltip_text(Some("Play/Pause"));
    play_pause_btn.set_valign(Align::Center);
    play_pause_btn.connect_clicked(|_| MediaService::global().play_pause());
    container.append(&play_pause_btn);

    // Next button
    let next_icon = icons.create_icon("skip_next", &[icon::ICON]);
    next_icon.widget().set_halign(Align::Center);
    next_icon.widget().set_valign(Align::Center);
    let next_btn = Button::new();
    next_btn.set_child(Some(&next_icon.widget()));
    next_btn.add_css_class(media::CONTROL_BTN);
    next_btn.add_css_class(button::COMPACT);
    for class in extra_classes {
        next_btn.add_css_class(class);
    }
    next_btn.set_tooltip_text(Some("Next"));
    next_btn.set_valign(Align::Center);
    next_btn.connect_clicked(|_| MediaService::global().next());
    container.append(&next_btn);

    (
        container,
        prev_btn,
        play_pause_btn,
        play_pause_icon,
        next_btn,
    )
}

/// Build seek bar with time labels.
/// Returns (container, scale, position_label, duration_label, is_seeking)
pub fn build_seek_section(
    extra_slider_classes: &[&str],
) -> (GtkBox, Scale, Label, Label, Rc<RefCell<bool>>) {
    let container = GtkBox::new(Orientation::Vertical, 0);
    container.add_css_class(media::SEEK);

    let is_pressed = Rc::new(RefCell::new(false));
    let pending_seek = Rc::new(RefCell::new(None::<i64>));
    let is_seeking = Rc::new(RefCell::new(false));

    let scale = Scale::with_range(Orientation::Horizontal, 0.0, 1.0, 1.0);
    scale.add_css_class(media::SEEK_SLIDER);
    for class in extra_slider_classes {
        scale.add_css_class(class);
    }
    scale.set_draw_value(false);
    scale.set_hexpand(true);

    let time_row = GtkBox::new(Orientation::Horizontal, 0);
    time_row.add_css_class(media::TIME);

    let position_label = Label::new(Some("0:00"));
    position_label.add_css_class(media::POSITION);
    position_label.add_css_class(color::MUTED);
    position_label.set_halign(Align::Start);
    position_label.set_hexpand(true);
    time_row.append(&position_label);

    let duration_label = Label::new(Some("0:00"));
    duration_label.add_css_class(media::DURATION);
    duration_label.add_css_class(color::MUTED);
    duration_label.set_halign(Align::End);
    time_row.append(&duration_label);

    // Event handling for drag-to-seek
    let legacy_controller = EventControllerLegacy::new();
    {
        let is_pressed = is_pressed.clone();
        let is_seeking = is_seeking.clone();
        let pending_seek = pending_seek.clone();
        legacy_controller.connect_event(move |_, event| {
            use gtk4::gdk::EventType;
            match event.event_type() {
                EventType::ButtonPress => {
                    *is_pressed.borrow_mut() = true;
                    glib::Propagation::Proceed
                }
                EventType::ButtonRelease => {
                    *is_pressed.borrow_mut() = false;
                    if let Some(position) = pending_seek.borrow_mut().take() {
                        MediaService::global().set_position(position);
                        let is_seeking = is_seeking.clone();
                        glib::timeout_add_local_once(
                            std::time::Duration::from_millis(150),
                            move || *is_seeking.borrow_mut() = false,
                        );
                    }
                    glib::Propagation::Proceed
                }
                _ => glib::Propagation::Proceed,
            }
        });
    }
    scale.add_controller(legacy_controller);

    {
        let is_pressed = is_pressed.clone();
        let is_seeking = is_seeking.clone();
        let pending_seek = pending_seek.clone();
        let position_label = position_label.clone();
        scale.connect_change_value(move |_, _, value| {
            if *is_pressed.borrow() {
                *is_seeking.borrow_mut() = true;
                *pending_seek.borrow_mut() = Some(value as i64);
                position_label.set_label(&format_duration(value as i64));
            } else {
                MediaService::global().set_position(value as i64);
            }
            glib::Propagation::Proceed
        });
    }

    container.append(&scale);
    container.append(&time_row);

    (container, scale, position_label, duration_label, is_seeking)
}

/// Build album art container with placeholder.
/// Returns (container, picture, placeholder_box, art_state)
pub fn build_album_art(size: i32) -> (GtkBox, RoundedPicture, GtkBox, Rc<RefCell<ArtState>>) {
    let icons = IconsService::global();
    let config_mgr = ConfigManager::global();
    let corner_radius = config_mgr.widget_border_radius() as f32;

    let container = GtkBox::new(Orientation::Vertical, 0);
    container.set_size_request(size, size);
    container.set_valign(Align::Center);

    let picture = RoundedPicture::new();
    picture.set_pixel_size(size);
    picture.set_corner_radius(corner_radius);
    picture.set_visible(false);
    container.append(&picture);

    let placeholder_box = GtkBox::new(Orientation::Vertical, 0);
    placeholder_box.add_css_class(media::ART);
    placeholder_box.add_css_class(media::ART_PLACEHOLDER);
    placeholder_box.set_size_request(size, size);

    let art_icon = icons.create_icon("album", &[media::EMPTY_ICON]);
    art_icon.widget().set_valign(Align::Center);
    art_icon.widget().set_vexpand(true);
    art_icon.widget().set_halign(Align::Center);
    art_icon.widget().set_hexpand(true);
    placeholder_box.append(&art_icon.widget());
    container.append(&placeholder_box);

    let art_state = Rc::new(RefCell::new(ArtState::new()));

    (container, picture, placeholder_box, art_state)
}

/// Build track info labels (title, artist, album).
/// Returns (container, title_label, artist_label, album_label)
pub fn build_track_info(
    max_width_chars: i32,
    spacing: i32,
) -> (GtkBox, Rc<MarqueeLabel>, Label, Label) {
    let container = GtkBox::new(Orientation::Vertical, spacing);
    container.set_halign(Align::Fill);
    container.set_hexpand(true);

    let title_label = Rc::new(MarqueeLabel::new());
    title_label.set_text("No track playing");
    title_label.set_max_width_chars(max_width_chars);
    title_label.label().add_css_class(media::TRACK_TITLE);
    title_label.widget().set_halign(Align::Center);
    title_label.widget().set_hexpand(true);
    container.append(title_label.widget());

    let artist_label = Label::new(Some("Unknown artist"));
    artist_label.add_css_class(media::ARTIST);
    artist_label.add_css_class(color::MUTED);
    artist_label.set_halign(Align::Center);
    artist_label.set_hexpand(true);
    artist_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    artist_label.set_max_width_chars(max_width_chars);
    container.append(&artist_label);

    let album_label = Label::new(Some(""));
    album_label.add_css_class(media::ALBUM);
    album_label.add_css_class(color::MUTED);
    album_label.set_halign(Align::Center);
    album_label.set_hexpand(true);
    album_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    album_label.set_max_width_chars(max_width_chars);
    container.append(&album_label);

    (container, title_label, artist_label, album_label)
}

// ============================================================================
// Update Functions
// ============================================================================

/// Update track info labels from a media snapshot.
pub fn update_track_info(
    title_label: &MarqueeLabel,
    artist_label: &Label,
    album_label: &Label,
    snapshot: &MediaSnapshot,
) {
    title_label.set_text(
        snapshot
            .metadata
            .title
            .as_deref()
            .unwrap_or("No track playing"),
    );

    let artist = snapshot
        .metadata
        .artist
        .as_deref()
        .unwrap_or("Unknown artist");
    artist_label.set_label(artist);
    artist_label.set_tooltip_text(Some(artist));

    let album = snapshot.metadata.album.as_deref().unwrap_or("");
    album_label.set_label(album);
    album_label.set_tooltip_text(if album.is_empty() { None } else { Some(album) });
}

/// Update playback control states from a media snapshot.
pub fn update_playback_controls(
    play_pause_icon: &IconHandle,
    play_pause_btn: &Button,
    prev_btn: &Button,
    next_btn: &Button,
    seek_scale: &Scale,
    snapshot: &MediaSnapshot,
) {
    play_pause_icon.set_icon(match snapshot.playback_status {
        PlaybackStatus::Playing => "media-playback-pause",
        PlaybackStatus::Paused | PlaybackStatus::Stopped => "media-playback-start",
    });
    play_pause_btn.set_sensitive(snapshot.can_play || snapshot.can_pause);
    prev_btn.set_sensitive(snapshot.can_go_previous);
    next_btn.set_sensitive(snapshot.can_go_next);
    seek_scale.set_sensitive(snapshot.can_seek);
}

/// Update seek bar position from a media snapshot.
pub fn update_seek_position(
    seek_scale: &Scale,
    position_label: &Label,
    duration_label: &Label,
    is_seeking: &Rc<RefCell<bool>>,
    snapshot: &MediaSnapshot,
) {
    if *is_seeking.borrow() {
        return;
    }

    let length = snapshot.metadata.length.unwrap_or(0);
    let position = snapshot.position;

    if length > 0 {
        seek_scale.set_range(0.0, length as f64);
        seek_scale.set_value(position as f64);
    } else {
        seek_scale.set_range(0.0, 1.0);
        seek_scale.set_value(0.0);
    }

    position_label.set_label(&format_duration(position));
    duration_label.set_label(&format_duration(length));
}

// ============================================================================
// Album Art Loading
// ============================================================================

/// Load album art, handling URL changes and cancellation.
///
/// Shows placeholder box on failure, hides it on success.
pub fn load_album_art(
    art_url: Option<&str>,
    picture: &RoundedPicture,
    placeholder_box: &GtkBox,
    art_state: &Rc<RefCell<ArtState>>,
) {
    let mut state = art_state.borrow_mut();

    if state.current_url.as_deref() == art_url {
        return;
    }

    state.cancellable.cancel();
    state.cancellable = gio::Cancellable::new();
    state.generation += 1;
    state.current_url = art_url.map(String::from);

    let generation = state.generation;
    let cancellable = state.cancellable.clone();
    drop(state);

    let placeholder_for_success = placeholder_box.clone();
    let on_success = move || {
        placeholder_for_success.set_visible(false);
    };

    let picture_for_failure = picture.clone();
    let placeholder_box = placeholder_box.clone();
    let on_failure = move || {
        picture_for_failure.set_visible(false);
        placeholder_box.set_visible(true);
    };

    match art_url {
        Some(url) => load_art_from_url(
            url,
            picture.clone(),
            art_state,
            generation,
            &cancellable,
            on_success,
            on_failure,
        ),
        None => on_failure(),
    }
}

/// Load album art from URL, calling `on_success` or `on_failure` callbacks.
///
/// This is the shared implementation used by both the bar widget and popover/window.
/// - `on_success` is called after the picture is set (e.g., to hide placeholder)
/// - `on_failure` is called when loading fails (e.g., to show placeholder or fallback icon)
pub fn load_art_from_url<S, F>(
    url: &str,
    picture: RoundedPicture,
    art_state: &Rc<RefCell<ArtState>>,
    generation: u64,
    cancellable: &gio::Cancellable,
    on_success: S,
    on_failure: F,
) where
    S: Fn() + Clone + 'static,
    F: Fn() + Clone + 'static,
{
    let url_string = url.to_string();
    let art_state = art_state.clone();
    let cancellable = cancellable.clone();

    if url.starts_with("file://") {
        let file = gio::File::for_uri(url);
        let on_success_clone = on_success.clone();
        let on_failure_clone = on_failure.clone();
        file.read_async(
            glib::Priority::DEFAULT,
            Some(&cancellable.clone()),
            move |result| {
                if art_state.borrow().generation != generation {
                    return;
                }
                match result {
                    Ok(stream) => load_texture_from_stream(
                        stream.upcast(),
                        &picture,
                        &art_state,
                        &url_string,
                        generation,
                        &cancellable,
                        on_success_clone,
                        on_failure_clone,
                    ),
                    Err(e) => {
                        if !e.matches(gio::IOErrorEnum::Cancelled) {
                            debug!("Failed to load album art from {}: {}", url_string, e);
                        }
                        on_failure_clone();
                    }
                }
            },
        );
    } else if url.starts_with("http://") || url.starts_with("https://") {
        let url_for_fetch = url.to_string();

        // minreq is blocking, so spawn in thread pool
        glib::spawn_future_local(async move {
            let fetch_result = gio::spawn_blocking(move || {
                minreq::get(&url_for_fetch)
                    .with_timeout(10)
                    .send()
                    .ok()
                    .filter(|r| r.status_code >= 200 && r.status_code < 300)
                    .map(|r| r.into_bytes())
            })
            .await;

            // Check if still relevant after async work
            if art_state.borrow().generation != generation {
                return;
            }

            match fetch_result {
                Ok(Some(bytes)) => {
                    load_texture_from_bytes(
                        &bytes,
                        &picture,
                        &url_string,
                        &on_success,
                        &on_failure,
                    );
                }
                Ok(None) => {
                    debug!("Failed to fetch album art from {}", url_string);
                    on_failure();
                }
                Err(e) => {
                    debug!("Failed to fetch album art from {}: {:?}", url_string, e);
                    on_failure();
                }
            }
        });
    } else {
        debug!("Unknown album art URL scheme: {}", url);
        on_failure();
    }
}

#[allow(clippy::too_many_arguments)]
fn load_texture_from_stream<S, F>(
    stream: gio::InputStream,
    picture: &RoundedPicture,
    art_state: &Rc<RefCell<ArtState>>,
    url: &str,
    generation: u64,
    cancellable: &gio::Cancellable,
    on_success: S,
    on_failure: F,
) where
    S: Fn() + 'static,
    F: Fn() + 'static,
{
    let picture = picture.clone();
    let art_state = art_state.clone();
    let url = url.to_string();

    Pixbuf::from_stream_async(&stream, Some(cancellable), move |result| {
        if art_state.borrow().generation != generation {
            return;
        }
        match result {
            Ok(pixbuf) => {
                picture.set_paintable(Some(&gtk4::gdk::Texture::for_pixbuf(&pixbuf)));
                picture.set_visible(true);
                on_success();
                debug!("Loaded album art from {}", url);
            }
            Err(e) => {
                if !e.matches(gio::IOErrorEnum::Cancelled) {
                    debug!("Failed to decode album art from {}: {}", url, e);
                }
                on_failure();
            }
        }
    });
}

fn load_texture_from_bytes<S, F>(
    bytes: &[u8],
    picture: &RoundedPicture,
    url: &str,
    on_success: &S,
    on_failure: &F,
) where
    S: Fn(),
    F: Fn(),
{
    let glib_bytes = glib::Bytes::from(bytes);
    match Pixbuf::from_stream(
        &gio::MemoryInputStream::from_bytes(&glib_bytes),
        None::<&gio::Cancellable>,
    ) {
        Ok(pixbuf) => {
            picture.set_paintable(Some(&gtk4::gdk::Texture::for_pixbuf(&pixbuf)));
            picture.set_visible(true);
            on_success();
            debug!("Loaded album art from {}", url);
        }
        Err(e) => {
            debug!("Failed to decode album art from {}: {}", url, e);
            on_failure();
        }
    }
}
