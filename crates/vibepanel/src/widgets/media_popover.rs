//! Media popover - detailed media player controls and track information.
//!
//! Provides a popover with:
//! - Album art display (left side)
//! - Track metadata (title, artist, album) on the right
//! - Playback controls (prev, play/pause, next)
//! - Seek bar with position/duration

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::gdk::Texture;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, EventControllerLegacy, Label, Orientation, Overlay, Scale, Widget,
};
use tracing::debug;

use crate::services::config_manager::ConfigManager;
use crate::services::icons::{IconHandle, IconsService};
use crate::services::media::{MediaService, MediaSnapshot, PlaybackStatus, format_duration};
use crate::services::tooltip::TooltipManager;
use crate::styles::{button, color, icon, media, surface};
use crate::widgets::marquee_label::{MarqueeLabel, ScrollMode};
use crate::widgets::rounded_picture::RoundedPicture;

const POPOVER_ART_SIZE: i32 = 140;

struct ArtState {
    current_url: Option<String>,
    generation: u64,
    cancellable: gio::Cancellable,
}

/// Controller owning the media popover UI elements and update logic.
#[derive(Clone)]
pub struct MediaPopoverController {
    title_label: Rc<MarqueeLabel>,
    artist_label: Label,
    album_label: Label,

    art_picture: RoundedPicture,
    art_placeholder_box: GtkBox,
    art_state: Rc<RefCell<ArtState>>,

    play_pause_btn: Button,
    play_pause_icon: IconHandle,
    prev_btn: Button,
    next_btn: Button,

    seek_scale: Scale,
    position_label: Label,
    duration_label: Label,

    is_seeking: Rc<RefCell<bool>>,
}

impl MediaPopoverController {
    pub fn update_from_snapshot(&self, snapshot: &MediaSnapshot) {
        self.title_label.set_text(
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
        self.artist_label.set_label(artist);
        self.artist_label.set_tooltip_text(Some(artist));

        let album = snapshot.metadata.album.as_deref().unwrap_or("");
        self.album_label.set_label(album);
        if !album.is_empty() {
            self.album_label.set_tooltip_text(Some(album));
        } else {
            self.album_label.set_tooltip_text(None);
        }

        self.update_album_art(snapshot);

        let icon_name = match snapshot.playback_status {
            PlaybackStatus::Playing => "media-playback-pause",
            PlaybackStatus::Paused | PlaybackStatus::Stopped => "media-playback-start",
        };
        self.play_pause_icon.set_icon(icon_name);

        self.play_pause_btn
            .set_sensitive(snapshot.can_play || snapshot.can_pause);
        self.prev_btn.set_sensitive(snapshot.can_go_previous);
        self.next_btn.set_sensitive(snapshot.can_go_next);
        self.seek_scale.set_sensitive(snapshot.can_seek);

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
    }

    fn update_album_art(&self, snapshot: &MediaSnapshot) {
        let art_url = snapshot.metadata.art_url.as_deref();

        let mut state = self.art_state.borrow_mut();

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

        if let Some(url) = art_url {
            self.load_album_art(url, generation, &cancellable);
        } else {
            self.show_placeholder();
        }
    }

    fn load_album_art(&self, url: &str, generation: u64, cancellable: &gio::Cancellable) {
        let url_string = url.to_string();
        let art_picture = self.art_picture.clone();
        let art_placeholder_box = self.art_placeholder_box.clone();
        let art_state = self.art_state.clone();
        let cancellable = cancellable.clone();

        if url.starts_with("file://") {
            let file = gio::File::for_uri(url);
            let cancellable_for_read = cancellable.clone();

            file.read_async(
                glib::Priority::DEFAULT,
                Some(&cancellable_for_read),
                move |result| {
                    if art_state.borrow().generation != generation {
                        return;
                    }

                    match result {
                        Ok(stream) => {
                            Self::load_texture_from_stream(
                                stream.upcast(),
                                &art_picture,
                                &art_placeholder_box,
                                &art_state,
                                &url_string,
                                generation,
                                &cancellable,
                            );
                        }
                        Err(e) => {
                            if !e.matches(gio::IOErrorEnum::Cancelled) {
                                debug!("Failed to load album art from {}: {}", url_string, e);
                            }
                            art_picture.set_visible(false);
                            art_placeholder_box.set_visible(true);
                        }
                    }
                },
            );
        } else if url.starts_with("http://") || url.starts_with("https://") {
            // soup3 needed for HTTP(S) since gio::File requires GVfs
            use soup::prelude::*;

            let session = soup::Session::new();
            let Ok(message) = soup::Message::new("GET", url) else {
                debug!("Failed to create HTTP request for album art: {}", url);
                self.show_placeholder();
                return;
            };

            let cancellable_for_callback = cancellable.clone();
            session.send_async(
                &message,
                glib::Priority::DEFAULT,
                Some(&cancellable),
                move |result: Result<gio::InputStream, glib::Error>| {
                    if art_state.borrow().generation != generation {
                        return;
                    }

                    match result {
                        Ok(stream) => {
                            Self::load_texture_from_stream(
                                stream,
                                &art_picture,
                                &art_placeholder_box,
                                &art_state,
                                &url_string,
                                generation,
                                &cancellable_for_callback,
                            );
                        }
                        Err(e) => {
                            if !e.matches(gio::IOErrorEnum::Cancelled) {
                                debug!("Failed to fetch album art from {}: {}", url_string, e);
                            }
                            art_picture.set_visible(false);
                            art_placeholder_box.set_visible(true);
                        }
                    }
                },
            );
        } else {
            debug!("Unknown album art URL scheme: {}", url);
            self.show_placeholder();
        }
    }

    fn load_texture_from_stream(
        stream: gio::InputStream,
        art_picture: &RoundedPicture,
        art_placeholder_box: &GtkBox,
        art_state: &Rc<RefCell<ArtState>>,
        url: &str,
        generation: u64,
        cancellable: &gio::Cancellable,
    ) {
        let art_picture = art_picture.clone();
        let art_placeholder_box = art_placeholder_box.clone();
        let art_state = art_state.clone();
        let url = url.to_string();

        gtk4::gdk_pixbuf::Pixbuf::from_stream_async(&stream, Some(cancellable), move |result| {
            if art_state.borrow().generation != generation {
                return;
            }

            match result {
                Ok(pixbuf) => {
                    let texture = Texture::for_pixbuf(&pixbuf);
                    art_picture.set_paintable(Some(&texture));
                    art_picture.set_visible(true);
                    art_placeholder_box.set_visible(false);
                    debug!("Loaded popover album art from {}", url);
                }
                Err(e) => {
                    if !e.matches(gio::IOErrorEnum::Cancelled) {
                        debug!("Failed to decode album art from {}: {}", url, e);
                    }
                    art_picture.set_visible(false);
                    art_placeholder_box.set_visible(true);
                }
            }
        });
    }

    fn show_placeholder(&self) {
        self.art_picture.set_visible(false);
        self.art_placeholder_box.set_visible(true);
    }
}

/// Build a media popover content widget bound to global services.
///
/// Returns both the root widget and a controller that can be used to
/// push live updates while the popover is open.
///
/// # Arguments
/// * `on_popout` - Callback invoked when the user clicks the pop-out button
pub fn build_media_popover_with_controller<F>(on_popout: F) -> (Widget, MediaPopoverController)
where
    F: Fn() + 'static,
{
    let media_service = MediaService::global();
    let snapshot = media_service.snapshot();
    let icons = IconsService::global();

    let container = GtkBox::new(Orientation::Vertical, 8);

    let content_row = GtkBox::new(Orientation::Horizontal, 12);
    content_row.add_css_class(media::CONTENT);

    let art_container = GtkBox::new(Orientation::Vertical, 0);
    art_container.set_size_request(POPOVER_ART_SIZE, POPOVER_ART_SIZE);
    art_container.set_valign(Align::Center);

    let config_mgr = ConfigManager::global();
    let corner_radius = config_mgr.widget_border_radius() as f32 * 0.8; // Slightly smaller than popover corners

    let art_picture = RoundedPicture::new();
    art_picture.set_pixel_size(POPOVER_ART_SIZE);
    art_picture.set_corner_radius(corner_radius);
    art_picture.set_visible(false);
    art_container.append(&art_picture);

    let art_placeholder_box = GtkBox::new(Orientation::Vertical, 0);
    art_placeholder_box.add_css_class(media::ART);
    art_placeholder_box.add_css_class(media::ART_PLACEHOLDER);
    art_placeholder_box.set_size_request(POPOVER_ART_SIZE, POPOVER_ART_SIZE);

    let art_placeholder_icon = icons.create_icon("album", &[media::EMPTY_ICON]);
    art_placeholder_icon.widget().set_valign(Align::Center);
    art_placeholder_icon.widget().set_vexpand(true);
    art_placeholder_icon.widget().set_halign(Align::Center);
    art_placeholder_icon.widget().set_hexpand(true);
    art_placeholder_box.append(&art_placeholder_icon.widget());
    art_container.append(&art_placeholder_box);

    content_row.append(&art_container);

    let info_section = GtkBox::new(Orientation::Vertical, 0);
    info_section.set_margin_start(12);

    let track_info = GtkBox::new(Orientation::Vertical, 4);
    track_info.set_valign(Align::End);
    track_info.set_vexpand(true);
    track_info.set_hexpand(true);
    track_info.set_halign(Align::Center);
    track_info.set_margin_bottom(16);

    let title_label = Rc::new(MarqueeLabel::with_scroll_mode(ScrollMode::Loop));
    title_label.set_text("No track playing");
    title_label.set_max_width_chars(18);
    title_label.label().add_css_class(media::TRACK_TITLE);
    title_label.widget().set_halign(Align::Center);
    track_info.append(title_label.widget());

    let artist_label = Label::new(Some("Unknown artist"));
    artist_label.add_css_class(media::ARTIST);
    artist_label.add_css_class(color::MUTED);
    artist_label.set_halign(Align::Center);
    artist_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    artist_label.set_max_width_chars(18);
    track_info.append(&artist_label);

    let album_label = Label::new(Some(""));
    album_label.add_css_class(media::ALBUM);
    album_label.add_css_class(color::MUTED);
    album_label.set_halign(Align::Center);
    album_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    album_label.set_max_width_chars(18);
    track_info.append(&album_label);

    info_section.append(&track_info);

    let controls = GtkBox::new(Orientation::Horizontal, 8);
    controls.add_css_class(media::CONTROLS);
    controls.set_halign(Align::Center);

    let prev_icon = icons.create_icon("skip_previous", &[icon::ICON]);
    prev_icon.widget().set_halign(Align::Center);
    prev_icon.widget().set_valign(Align::Center);
    let prev_btn = Button::new();
    prev_btn.set_child(Some(&prev_icon.widget()));
    prev_btn.add_css_class(media::CONTROL_BTN);
    prev_btn.add_css_class(button::COMPACT);
    prev_btn.set_tooltip_text(Some("Previous"));
    prev_btn.set_valign(Align::Center);
    prev_btn.connect_clicked(|_| {
        MediaService::global().previous();
    });
    controls.append(&prev_btn);

    let play_pause_icon =
        icons.create_icon("media-playback-start", &[icon::ICON, media::PRIMARY_ICON]);
    play_pause_icon.widget().set_halign(Align::Center);
    play_pause_icon.widget().set_valign(Align::Center);
    let play_pause_btn = Button::new();
    play_pause_btn.set_child(Some(&play_pause_icon.widget()));
    play_pause_btn.add_css_class(media::CONTROL_BTN);
    play_pause_btn.add_css_class(media::CONTROL_BTN_PRIMARY);
    play_pause_btn.add_css_class(button::COMPACT);
    play_pause_btn.set_tooltip_text(Some("Play/Pause"));
    play_pause_btn.set_valign(Align::Center);
    play_pause_btn.connect_clicked(|_| {
        MediaService::global().play_pause();
    });
    controls.append(&play_pause_btn);

    let next_icon = icons.create_icon("skip_next", &[icon::ICON]);
    next_icon.widget().set_halign(Align::Center);
    next_icon.widget().set_valign(Align::Center);
    let next_btn = Button::new();
    next_btn.set_child(Some(&next_icon.widget()));
    next_btn.add_css_class(media::CONTROL_BTN);
    next_btn.add_css_class(button::COMPACT);
    next_btn.set_tooltip_text(Some("Next"));
    next_btn.set_valign(Align::Center);
    next_btn.connect_clicked(|_| {
        MediaService::global().next();
    });
    controls.append(&next_btn);

    info_section.append(&controls);
    content_row.append(&info_section);
    container.append(&content_row);

    let seek_section = GtkBox::new(Orientation::Vertical, 0);
    seek_section.add_css_class(media::SEEK);

    let is_pressed = Rc::new(RefCell::new(false));
    let pending_seek = Rc::new(RefCell::new(None::<i64>));
    let is_seeking = Rc::new(RefCell::new(false));
    let seek_scale = Scale::with_range(Orientation::Horizontal, 0.0, 1.0, 1.0);
    seek_scale.add_css_class(media::SEEK_SLIDER);
    seek_scale.set_draw_value(false);
    seek_scale.set_hexpand(true);

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

    let legacy_controller = EventControllerLegacy::new();
    {
        let is_pressed_clone = is_pressed.clone();
        let is_seeking_clone = is_seeking.clone();
        let pending_seek_clone = pending_seek.clone();
        legacy_controller.connect_event(move |_, event| {
            use gtk4::gdk::EventType;
            match event.event_type() {
                EventType::ButtonPress => {
                    *is_pressed_clone.borrow_mut() = true;
                    glib::Propagation::Proceed
                }
                EventType::ButtonRelease => {
                    *is_pressed_clone.borrow_mut() = false;
                    if let Some(position) = pending_seek_clone.borrow_mut().take() {
                        MediaService::global().set_position(position);
                        // Brief delay to avoid UI jitter from stale updates
                        let is_seeking_for_timeout = is_seeking_clone.clone();
                        glib::timeout_add_local_once(
                            std::time::Duration::from_millis(150),
                            move || {
                                *is_seeking_for_timeout.borrow_mut() = false;
                            },
                        );
                    }
                    glib::Propagation::Proceed
                }
                _ => glib::Propagation::Proceed,
            }
        });
    }
    seek_scale.add_controller(legacy_controller);

    {
        let is_pressed_clone = is_pressed.clone();
        let is_seeking_clone = is_seeking.clone();
        let pending_seek_clone = pending_seek.clone();
        let position_label_clone = position_label.clone();
        seek_scale.connect_change_value(move |_, _, value| {
            if *is_pressed_clone.borrow() {
                *is_seeking_clone.borrow_mut() = true;
                *pending_seek_clone.borrow_mut() = Some(value as i64);
                position_label_clone.set_label(&format_duration(value as i64));
            } else {
                MediaService::global().set_position(value as i64);
            }
            glib::Propagation::Proceed
        });
    }

    seek_section.append(&seek_scale);
    seek_section.append(&time_row);
    container.append(&seek_section);

    let overlay = Overlay::new();
    overlay.add_css_class(media::POPOVER);
    overlay.set_child(Some(&container));

    let popout_btn = Button::new();
    popout_btn.set_has_frame(false);
    popout_btn.set_focusable(false);
    popout_btn.set_focus_on_click(false);
    popout_btn.add_css_class(surface::POPOVER_ICON_BTN);
    popout_btn.add_css_class(media::POPOUT_BTN);
    popout_btn.set_halign(Align::End);
    popout_btn.set_valign(Align::Start);

    let popout_icon = icons.create_icon("open_in_new", &[icon::ICON, media::POPOUT_ICON]);
    popout_icon.widget().set_halign(Align::Center);
    popout_icon.widget().set_valign(Align::Center);
    popout_btn.set_child(Some(&popout_icon.widget()));

    let tooltip_manager = TooltipManager::global();
    tooltip_manager.set_styled_tooltip(&popout_btn, "Pop out");

    popout_btn.connect_clicked(move |_| {
        on_popout();
    });

    overlay.add_overlay(&popout_btn);

    let art_state = Rc::new(RefCell::new(ArtState {
        current_url: None,
        generation: 0,
        cancellable: gio::Cancellable::new(),
    }));

    let controller = MediaPopoverController {
        title_label,
        artist_label,
        album_label,
        art_picture,
        art_placeholder_box,
        art_state,
        play_pause_btn,
        play_pause_icon,
        prev_btn,
        next_btn,
        seek_scale,
        position_label,
        duration_label,
        is_seeking,
    };

    controller.update_from_snapshot(&snapshot);

    (overlay.upcast::<Widget>(), controller)
}
