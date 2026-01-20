//! Media popover - detailed media player controls and track information.
//!
//! Provides a popover with:
//! - Album art display
//! - Track metadata (title, artist, album)
//! - Playback controls (prev, play/pause, next)
//! - Seek bar with position/duration
//! - Volume slider
//! - Pop-out button to open standalone window
//! - Player selector when multiple players available

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, Button, Label, Orientation, Scale, Separator, Widget};

use crate::services::icons::{IconHandle, IconsService};
use crate::services::media::{MediaService, MediaSnapshot, PlaybackStatus, format_duration};
use crate::styles::{button, color, icon, media, surface};

/// Controller owning the media popover UI elements and update logic.
#[derive(Clone)]
pub struct MediaPopoverController {
    // Track info
    title_label: Label,
    artist_label: Label,
    album_label: Label,
    player_name_label: Label,

    // Playback controls
    play_pause_btn: Button,
    play_pause_icon: IconHandle,
    prev_btn: Button,
    next_btn: Button,

    // Seek bar
    seek_scale: Scale,
    position_label: Label,
    duration_label: Label,

    // Volume
    volume_scale: Scale,
    volume_icon: IconHandle,

    // State
    is_seeking: Rc<RefCell<bool>>,
    is_volume_changing: Rc<RefCell<bool>>,
}

impl MediaPopoverController {
    /// Update all UI elements from the latest media snapshot.
    pub fn update_from_snapshot(&self, snapshot: &MediaSnapshot) {
        // Track info
        self.title_label.set_label(
            snapshot
                .metadata
                .title
                .as_deref()
                .unwrap_or("No track playing"),
        );
        self.artist_label.set_label(
            snapshot
                .metadata
                .artist
                .as_deref()
                .unwrap_or("Unknown artist"),
        );
        self.album_label
            .set_label(snapshot.metadata.album.as_deref().unwrap_or(""));
        self.player_name_label
            .set_label(snapshot.player_name.as_deref().unwrap_or("No player"));

        // Play/pause button icon
        let icon_name = match snapshot.playback_status {
            PlaybackStatus::Playing => "media-playback-pause",
            PlaybackStatus::Paused | PlaybackStatus::Stopped => "media-playback-start",
        };
        self.play_pause_icon.set_icon(icon_name);

        // Enable/disable controls based on capabilities
        self.play_pause_btn
            .set_sensitive(snapshot.can_play || snapshot.can_pause);
        self.prev_btn.set_sensitive(snapshot.can_go_previous);
        self.next_btn.set_sensitive(snapshot.can_go_next);
        self.seek_scale.set_sensitive(snapshot.can_seek);

        // Seek bar - only update if not currently being dragged
        if !*self.is_seeking.borrow() {
            let length = snapshot.metadata.length.unwrap_or(0);
            let position = snapshot.position;

            if length > 0 {
                self.seek_scale.set_range(0.0, length as f64);
                self.seek_scale.set_value(position as f64);
            } else {
                self.seek_scale.set_range(0.0, 1.0);
                self.seek_scale.set_value(0.0);
            }

            self.position_label.set_label(&format_duration(position));
            self.duration_label.set_label(&format_duration(length));
        }

        // Volume - only update if not currently being dragged
        if !*self.is_volume_changing.borrow() {
            self.volume_scale.set_value(snapshot.volume);

            // Update volume icon based on level
            let volume_icon_name = if snapshot.volume <= 0.0 {
                "audio-volume-muted"
            } else if snapshot.volume < 0.33 {
                "audio-volume-low"
            } else if snapshot.volume < 0.66 {
                "audio-volume-medium"
            } else {
                "audio-volume-high"
            };
            self.volume_icon.set_icon(volume_icon_name);
        }
    }
}

/// Build a media popover content widget bound to global services.
///
/// Returns both the root widget and a controller that can be used to
/// push live updates while the popover is open.
pub fn build_media_popover_with_controller() -> (Widget, MediaPopoverController) {
    let media_service = MediaService::global();
    let snapshot = media_service.snapshot();
    let icons = IconsService::global();

    // Main container
    let container = GtkBox::new(Orientation::Vertical, 12);
    container.add_css_class(media::POPOVER);
    container.add_css_class(surface::POPOVER);
    container.add_css_class(surface::NO_FOCUS);

    // Header with player name
    let header = GtkBox::new(Orientation::Horizontal, 8);
    let player_name_label = Label::new(None);
    player_name_label.add_css_class(media::PLAYER_NAME);
    player_name_label.add_css_class(color::MUTED);
    player_name_label.set_halign(Align::Start);
    player_name_label.set_hexpand(true);
    header.append(&player_name_label);

    // Pop-out button (placeholder for now)
    let popout_icon = icons.create_icon("open_in_new", &[icon::ICON]);
    let popout_btn = Button::new();
    popout_btn.set_child(Some(&popout_icon.widget()));
    popout_btn.add_css_class(media::POPOUT_BTN);
    popout_btn.add_css_class(button::GHOST);
    popout_btn.set_tooltip_text(Some("Open in window"));
    header.append(&popout_btn);

    container.append(&header);

    // Album art placeholder (we'll add actual art loading later)
    let art_box = GtkBox::new(Orientation::Vertical, 0);
    art_box.add_css_class(media::ART);
    art_box.add_css_class(media::ART_PLACEHOLDER);
    art_box.set_size_request(200, 200);
    art_box.set_halign(Align::Center);

    let art_icon = icons.create_icon("album", &[media::EMPTY_ICON]);
    art_icon.widget().set_valign(Align::Center);
    art_icon.widget().set_vexpand(true);
    art_box.append(&art_icon.widget());

    container.append(&art_box);

    // Track info section
    let info_section = GtkBox::new(Orientation::Vertical, 4);
    info_section.set_halign(Align::Center);

    let title_label = Label::new(Some("No track playing"));
    title_label.add_css_class(media::TRACK_TITLE);
    title_label.set_halign(Align::Center);
    title_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    title_label.set_max_width_chars(30);
    info_section.append(&title_label);

    let artist_label = Label::new(Some("Unknown artist"));
    artist_label.add_css_class(media::ARTIST);
    artist_label.add_css_class(color::MUTED);
    artist_label.set_halign(Align::Center);
    artist_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    artist_label.set_max_width_chars(30);
    info_section.append(&artist_label);

    let album_label = Label::new(Some(""));
    album_label.add_css_class(media::ALBUM);
    album_label.add_css_class(color::MUTED);
    album_label.set_halign(Align::Center);
    album_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    album_label.set_max_width_chars(30);
    info_section.append(&album_label);

    container.append(&info_section);

    // Seek bar section
    let seek_section = GtkBox::new(Orientation::Vertical, 4);
    seek_section.add_css_class(media::SEEK);

    let is_seeking = Rc::new(RefCell::new(false));
    let seek_scale = Scale::with_range(Orientation::Horizontal, 0.0, 1.0, 1.0);
    seek_scale.add_css_class(media::SEEK_SLIDER);
    seek_scale.set_draw_value(false);
    seek_scale.set_hexpand(true);

    // Track seek start/end to avoid updating while dragging
    {
        let is_seeking_clone = is_seeking.clone();
        seek_scale.connect_change_value(move |_, _, _| {
            *is_seeking_clone.borrow_mut() = true;
            glib::Propagation::Proceed
        });
    }

    // Apply seek when released
    {
        let is_seeking_clone = is_seeking.clone();
        let seek_scale_clone = seek_scale.clone();
        seek_scale.connect_value_changed(move |_| {
            if *is_seeking_clone.borrow() {
                let position = seek_scale_clone.value() as i64;
                let service = MediaService::global();
                service.set_position(position);
                // Reset seeking flag after a short delay to allow UI to update
                let is_seeking_for_timeout = is_seeking_clone.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(100), move || {
                    *is_seeking_for_timeout.borrow_mut() = false;
                });
            }
        });
    }

    seek_section.append(&seek_scale);

    // Time labels row
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

    seek_section.append(&time_row);
    container.append(&seek_section);

    // Playback controls
    let controls = GtkBox::new(Orientation::Horizontal, 16);
    controls.add_css_class(media::CONTROLS);
    controls.set_halign(Align::Center);

    // Previous button
    let prev_icon = icons.create_icon("skip_previous", &[icon::ICON]);
    let prev_btn = Button::new();
    prev_btn.set_child(Some(&prev_icon.widget()));
    prev_btn.add_css_class(media::CONTROL_BTN);
    prev_btn.add_css_class(button::GHOST);
    prev_btn.set_tooltip_text(Some("Previous"));
    prev_btn.connect_clicked(|_| {
        MediaService::global().previous();
    });
    controls.append(&prev_btn);

    // Play/pause button
    let play_pause_icon = icons.create_icon("media-playback-start", &[icon::ICON]);
    let play_pause_btn = Button::new();
    play_pause_btn.set_child(Some(&play_pause_icon.widget()));
    play_pause_btn.add_css_class(media::CONTROL_BTN);
    play_pause_btn.add_css_class(media::CONTROL_BTN_PRIMARY);
    play_pause_btn.add_css_class(button::ACCENT);
    play_pause_btn.set_tooltip_text(Some("Play/Pause"));
    play_pause_btn.connect_clicked(|_| {
        MediaService::global().play_pause();
    });
    controls.append(&play_pause_btn);

    // Next button
    let next_icon = icons.create_icon("skip_next", &[icon::ICON]);
    let next_btn = Button::new();
    next_btn.set_child(Some(&next_icon.widget()));
    next_btn.add_css_class(media::CONTROL_BTN);
    next_btn.add_css_class(button::GHOST);
    next_btn.set_tooltip_text(Some("Next"));
    next_btn.connect_clicked(|_| {
        MediaService::global().next();
    });
    controls.append(&next_btn);

    container.append(&controls);

    // Separator
    let separator = Separator::new(Orientation::Horizontal);
    container.append(&separator);

    // Volume section
    let volume_section = GtkBox::new(Orientation::Horizontal, 8);
    volume_section.add_css_class(media::VOLUME);

    let volume_icon = icons.create_icon("audio-volume-high", &[media::VOLUME_ICON]);
    volume_section.append(&volume_icon.widget());

    let is_volume_changing = Rc::new(RefCell::new(false));
    let volume_scale = Scale::with_range(Orientation::Horizontal, 0.0, 1.0, 0.01);
    volume_scale.add_css_class(media::VOLUME_SLIDER);
    volume_scale.set_draw_value(false);
    volume_scale.set_hexpand(true);
    volume_scale.set_value(1.0);

    // Track volume change start
    {
        let is_vol_clone = is_volume_changing.clone();
        volume_scale.connect_change_value(move |_, _, _| {
            *is_vol_clone.borrow_mut() = true;
            glib::Propagation::Proceed
        });
    }

    // Apply volume when changed
    {
        let is_vol_clone = is_volume_changing.clone();
        let volume_scale_clone = volume_scale.clone();
        volume_scale.connect_value_changed(move |_| {
            if *is_vol_clone.borrow() {
                let volume = volume_scale_clone.value();
                MediaService::global().set_volume(volume);
                // Reset flag after short delay
                let is_vol_for_timeout = is_vol_clone.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(100), move || {
                    *is_vol_for_timeout.borrow_mut() = false;
                });
            }
        });
    }

    volume_section.append(&volume_scale);
    container.append(&volume_section);

    // Build controller
    let controller = MediaPopoverController {
        title_label,
        artist_label,
        album_label,
        player_name_label,
        play_pause_btn,
        play_pause_icon,
        prev_btn,
        next_btn,
        seek_scale,
        position_label,
        duration_label,
        volume_scale,
        volume_icon,
        is_seeking,
        is_volume_changing,
    };

    // Initial update
    controller.update_from_snapshot(&snapshot);

    (container.upcast::<Widget>(), controller)
}
