//! Media pop-out window - standalone draggable media player controls.
//!
//! This creates a regular GTK window (NOT layer-shell) that:
//! - Can be dragged around by the user (click and drag anywhere on the window)
//! - Persists when switching focus (doesn't auto-close like popovers)
//! - Is borderless/undecorated matching panel theme
//! - Closed via compositor keybindings (e.g., super+q)
//!
//! Note: Always-on-top behavior depends on the compositor/window manager.
//! On Wayland, this is typically controlled by the compositor, not the app.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::gdk_pixbuf::Pixbuf;
use gtk4::gio;
use gtk4::glib::{self, clone};
use gtk4::prelude::*;
use gtk4::{
    Align, ApplicationWindow, Box as GtkBox, Button, EventControllerLegacy, GestureClick, Label,
    Orientation, Scale, Window,
};
use tracing::debug;

use crate::services::callbacks::CallbackId;
use crate::services::config_manager::ConfigManager;
use crate::services::icons::{IconHandle, IconsService};
use crate::services::media::{MediaService, MediaSnapshot, PlaybackStatus, format_duration};
use crate::services::surfaces::SurfaceStyleManager;
use crate::styles::{button, color, icon, media};
use crate::widgets::marquee_label::{MarqueeLabel, ScrollMode};
use crate::widgets::rounded_picture::RoundedPicture;

/// Album art size in the pop-out window.
const WINDOW_ART_SIZE: i32 = 100;

/// State for async album art loading.
struct ArtState {
    /// Current art URL being displayed (or loading).
    current_url: Option<String>,
    /// Generation counter to handle race conditions in async art loading.
    generation: u64,
    /// Cancellable for in-flight art loading operations.
    cancellable: gio::Cancellable,
}

/// Handle to the media pop-out window.
///
/// Keeps the window and update logic alive. Drop this to close the window.
#[allow(dead_code)] // Part of public API for planned pop-out feature
pub struct MediaWindowHandle {
    window: Window,
    /// Callback ID for MediaService updates (stored for cleanup).
    _callback_id: Rc<RefCell<Option<CallbackId>>>,
}

#[allow(dead_code)] // Methods are part of public API for planned pop-out feature
impl MediaWindowHandle {
    /// Show the window.
    pub fn show(&self) {
        self.window.present();
    }

    /// Hide the window.
    pub fn hide(&self) {
        self.window.set_visible(false);
    }

    /// Check if the window is visible.
    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    /// Close and destroy the window.
    pub fn close(&self) {
        self.window.close();
    }
}

/// Controller for updating the pop-out window UI.
#[derive(Clone)]
struct MediaWindowController {
    // Track info
    title_label: Rc<MarqueeLabel>,
    artist_label: Label,
    album_label: Label,

    // Album art
    art_picture: RoundedPicture,
    art_placeholder_box: GtkBox,
    art_state: Rc<RefCell<ArtState>>,

    // Playback controls
    play_pause_btn: Button,
    play_pause_icon: IconHandle,
    prev_btn: Button,
    next_btn: Button,

    // Seek bar
    seek_scale: Scale,
    position_label: Label,
    duration_label: Label,

    // State
    is_seeking: Rc<RefCell<bool>>,
}

impl MediaWindowController {
    /// Update all UI elements from the latest media snapshot.
    fn update_from_snapshot(&self, snapshot: &MediaSnapshot) {
        // Track info
        let title = snapshot
            .metadata
            .title
            .as_deref()
            .unwrap_or("No track playing");
        self.title_label.set_text(title);

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

        // Album art
        self.update_album_art(snapshot);

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
    }

    /// Update album art from snapshot, loading asynchronously if URL changed.
    fn update_album_art(&self, snapshot: &MediaSnapshot) {
        let art_url = snapshot.metadata.art_url.as_deref();

        let mut state = self.art_state.borrow_mut();

        // Check if URL changed
        if state.current_url.as_deref() == art_url {
            return; // No change
        }

        // Cancel any in-flight loading
        state.cancellable.cancel();
        state.cancellable = gio::Cancellable::new();
        state.generation += 1;
        state.current_url = art_url.map(String::from);

        let generation = state.generation;
        let cancellable = state.cancellable.clone();
        drop(state); // Release borrow before async work

        if let Some(url) = art_url {
            // Load album art
            self.load_album_art(url, generation, &cancellable);
        } else {
            // No art URL - show placeholder
            self.show_placeholder();
        }
    }

    /// Load album art from URL asynchronously.
    fn load_album_art(&self, url: &str, generation: u64, cancellable: &gio::Cancellable) {
        let url_string = url.to_string();
        let art_picture = self.art_picture.clone();
        let art_placeholder_box = self.art_placeholder_box.clone();
        let art_state = self.art_state.clone();
        let cancellable = cancellable.clone();

        if url.starts_with("file://") || url.starts_with("http://") || url.starts_with("https://") {
            let file = gio::File::for_uri(url);
            let cancellable_for_read = cancellable.clone();

            file.read_async(
                glib::Priority::DEFAULT,
                Some(&cancellable_for_read),
                move |result| {
                    // Validate generation before processing
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
                            // Show placeholder on error
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

    /// Load texture from stream and apply to picture widget.
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
        let cancellable = cancellable.clone();

        Pixbuf::from_stream_async(&stream, Some(&cancellable), move |result| {
            // Validate generation before applying
            if art_state.borrow().generation != generation {
                return;
            }

            match result {
                Ok(pixbuf) => {
                    let texture = gtk4::gdk::Texture::for_pixbuf(&pixbuf);
                    art_picture.set_paintable(Some(&texture));
                    art_picture.set_visible(true);
                    art_placeholder_box.set_visible(false);
                    debug!("Loaded album art from {} (window)", url);
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

    /// Show the placeholder and hide the picture.
    fn show_placeholder(&self) {
        self.art_picture.set_visible(false);
        self.art_placeholder_box.set_visible(true);
    }
}

/// Create a new media pop-out window.
///
/// The window is created but not shown. Call `handle.show()` to display it.
///
/// # Arguments
/// * `app` - Optional GTK Application to associate with the window
/// * `on_close` - Callback invoked when the window is closed (by any means: close button or programmatically)
#[allow(dead_code)] // Part of public API for planned pop-out feature
pub fn create_media_window<G>(app: Option<&gtk4::Application>, on_close: G) -> MediaWindowHandle
where
    G: Fn() + 'static,
{
    let icons = IconsService::global();
    let media_service = MediaService::global();
    let snapshot = media_service.snapshot();

    // Create window - regular GTK window, not layer-shell
    // Note: resizable(false) helps compositors treat this as a floating window
    let window = if let Some(app) = app {
        ApplicationWindow::builder()
            .application(app)
            .decorated(false)
            .resizable(false)
            .deletable(true)
            .build()
            .upcast::<Window>()
    } else {
        Window::builder()
            .decorated(false)
            .resizable(false)
            .deletable(true)
            .build()
    };

    window.add_css_class(media::WINDOW);
    window.set_title(Some("Media Player"));

    // Set fixed size to prevent resizing when label content changes
    window.set_default_size(280, 150);

    // Main container
    let main_box = GtkBox::new(Orientation::Vertical, 0);
    main_box.add_css_class(media::CONTENT);
    main_box.set_size_request(280, 150);

    // Apply surface styling
    SurfaceStyleManager::global().apply_surface_styles(&main_box, true, None);

    // Set up drag gesture on the main container so window can be dragged from anywhere
    {
        let gesture = GestureClick::new();
        gesture.set_button(1); // Left mouse button

        gesture.connect_pressed(clone!(
            #[weak]
            window,
            move |gesture, _n_press, x, y| {
                if let Some(surface) = window.surface()
                    && let Some(toplevel) = surface.downcast_ref::<gtk4::gdk::Toplevel>()
                {
                    // Get the widget that received the event
                    if let Some(widget) = gesture.widget() {
                        // Use compute_point to translate coordinates (non-deprecated)
                        if let Some(point) = widget
                            .compute_point(&window, &gtk4::graphene::Point::new(x as f32, y as f32))
                        {
                            // Start the move operation
                            toplevel.begin_move(
                                gesture.device().as_ref().unwrap(),
                                gesture.current_button() as i32,
                                point.x() as f64,
                                point.y() as f64,
                                gesture.current_event_time(),
                            );
                        }
                    }
                }
            }
        ));

        main_box.add_controller(gesture);
    }

    // ===== Content area - horizontal layout like popover =====
    let content = GtkBox::new(Orientation::Vertical, 4);
    content.set_margin_top(0);
    content.set_margin_bottom(4);
    content.set_margin_start(8);
    content.set_margin_end(8);

    // Main content row - horizontal: album art (left) + info section (right)
    let content_row = GtkBox::new(Orientation::Horizontal, 12);
    content_row.add_css_class(media::CONTENT);

    // Album art container (holds both picture and placeholder, stacked)
    let art_container = GtkBox::new(Orientation::Vertical, 0);
    art_container.set_size_request(WINDOW_ART_SIZE, WINDOW_ART_SIZE);
    art_container.set_valign(Align::Center);

    // Album art picture (initially hidden until art loads)
    // Use 80% of widget_border_radius for inner element (slightly smaller than window corners)
    let config_mgr = ConfigManager::global();
    let corner_radius = config_mgr.widget_border_radius() as f32 * 0.8;

    let art_picture = RoundedPicture::new();
    art_picture.set_pixel_size(WINDOW_ART_SIZE);
    art_picture.set_corner_radius(corner_radius);
    art_picture.set_visible(false);
    art_container.append(&art_picture);

    // Placeholder icon (visible when no art)
    let art_placeholder_box = GtkBox::new(Orientation::Vertical, 0);
    art_placeholder_box.add_css_class(media::ART);
    art_placeholder_box.add_css_class(media::ART_PLACEHOLDER);
    art_placeholder_box.set_size_request(WINDOW_ART_SIZE, WINDOW_ART_SIZE);

    let art_icon = icons.create_icon("album", &[media::EMPTY_ICON]);
    art_icon.widget().set_valign(Align::Center);
    art_icon.widget().set_vexpand(true);
    art_icon.widget().set_halign(Align::Center);
    art_icon.widget().set_hexpand(true);
    art_placeholder_box.append(&art_icon.widget());
    art_container.append(&art_placeholder_box);

    content_row.append(&art_container);

    // Right side: info section with track info and controls
    let info_section = GtkBox::new(Orientation::Vertical, 0);

    // Track info (near bottom, close to controls)
    let track_info = GtkBox::new(Orientation::Vertical, 2);
    track_info.set_valign(Align::End);
    track_info.set_vexpand(true);
    track_info.set_hexpand(true);
    track_info.set_halign(Align::Center);
    track_info.set_margin_bottom(4);

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

    // Playback controls (bottom of info section, centered)
    let controls = GtkBox::new(Orientation::Horizontal, 8);
    controls.add_css_class(media::CONTROLS);
    controls.set_halign(Align::Center);

    // Previous button
    let prev_icon = icons.create_icon("skip_previous", &[icon::ICON]);
    prev_icon.widget().set_halign(Align::Center);
    prev_icon.widget().set_valign(Align::Center);
    let prev_btn = Button::new();
    prev_btn.set_child(Some(&prev_icon.widget()));
    prev_btn.add_css_class(media::CONTROL_BTN);
    prev_btn.add_css_class(media::WINDOW_CONTROL_BTN);
    prev_btn.add_css_class(button::COMPACT);
    prev_btn.set_tooltip_text(Some("Previous"));
    prev_btn.set_valign(Align::Center); // Prevent vertical stretching
    prev_btn.connect_clicked(|_| {
        MediaService::global().previous();
    });
    controls.append(&prev_btn);

    // Play/pause button
    let play_pause_icon =
        icons.create_icon("media-playback-start", &[icon::ICON, media::PRIMARY_ICON]);
    play_pause_icon.widget().set_halign(Align::Center);
    play_pause_icon.widget().set_valign(Align::Center);
    let play_pause_btn = Button::new();
    play_pause_btn.set_child(Some(&play_pause_icon.widget()));
    play_pause_btn.add_css_class(media::CONTROL_BTN);
    play_pause_btn.add_css_class(media::CONTROL_BTN_PRIMARY);
    play_pause_btn.add_css_class(media::WINDOW_CONTROL_BTN);
    play_pause_btn.add_css_class(button::COMPACT);
    play_pause_btn.set_tooltip_text(Some("Play/Pause"));
    play_pause_btn.set_valign(Align::Center); // Prevent vertical stretching
    play_pause_btn.connect_clicked(|_| {
        MediaService::global().play_pause();
    });
    controls.append(&play_pause_btn);

    // Next button
    let next_icon = icons.create_icon("skip_next", &[icon::ICON]);
    next_icon.widget().set_halign(Align::Center);
    next_icon.widget().set_valign(Align::Center);
    let next_btn = Button::new();
    next_btn.set_child(Some(&next_icon.widget()));
    next_btn.add_css_class(media::CONTROL_BTN);
    next_btn.add_css_class(media::WINDOW_CONTROL_BTN);
    next_btn.add_css_class(button::COMPACT);
    next_btn.set_tooltip_text(Some("Next"));
    next_btn.set_valign(Align::Center); // Prevent vertical stretching
    next_btn.connect_clicked(|_| {
        MediaService::global().next();
    });
    controls.append(&next_btn);

    info_section.append(&controls);
    content_row.append(&info_section);
    content.append(&content_row);

    // Seek bar section (under content row)
    let seek_section = GtkBox::new(Orientation::Vertical, 0);
    seek_section.add_css_class(media::SEEK);

    let is_pressed = Rc::new(RefCell::new(false));
    let pending_seek = Rc::new(RefCell::new(None::<i64>));
    let is_seeking = Rc::new(RefCell::new(false));
    let seek_scale = Scale::with_range(Orientation::Horizontal, 0.0, 1.0, 1.0);
    seek_scale.add_css_class(media::SEEK_SLIDER);
    seek_scale.add_css_class(media::WINDOW_SEEK_SLIDER);
    seek_scale.set_draw_value(false);
    seek_scale.set_hexpand(true);

    // Time labels row (defined before seek handler so we can update position label during drag)
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

    // Use EventControllerLegacy to catch raw button press/release events
    // This works reliably for both clicks and drags
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
                    // Apply the pending seek position on release
                    if let Some(position) = pending_seek_clone.borrow_mut().take() {
                        MediaService::global().set_position(position);
                        // Keep is_seeking true briefly to avoid UI jitter from stale updates
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

    // Handle value changes - store pending seek during press, don't actually seek until release
    {
        let is_pressed_clone = is_pressed.clone();
        let is_seeking_clone = is_seeking.clone();
        let pending_seek_clone = pending_seek.clone();
        let position_label_clone = position_label.clone();
        seek_scale.connect_change_value(move |_, _, value| {
            if *is_pressed_clone.borrow() {
                // Mouse is pressed - store the value but don't seek yet
                *is_seeking_clone.borrow_mut() = true;
                *pending_seek_clone.borrow_mut() = Some(value as i64);
                position_label_clone.set_label(&format_duration(value as i64));
            } else {
                // Not pressed (keyboard, etc.) - seek immediately
                MediaService::global().set_position(value as i64);
            }
            // Always allow the visual slider to update
            glib::Propagation::Proceed
        });
    }

    seek_section.append(&seek_scale);

    seek_section.append(&time_row);
    content.append(&seek_section);

    main_box.append(&content);
    window.set_child(Some(&main_box));

    // Build controller
    let art_state = Rc::new(RefCell::new(ArtState {
        current_url: None,
        generation: 0,
        cancellable: gio::Cancellable::new(),
    }));

    let controller = MediaWindowController {
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

    // Initial update
    controller.update_from_snapshot(&snapshot);

    // Subscribe to media service updates
    let callback_id_cell: Rc<RefCell<Option<CallbackId>>> = Rc::new(RefCell::new(None));
    {
        let controller = controller.clone();
        let callback_id = media_service.connect(move |snapshot| {
            controller.update_from_snapshot(snapshot);
        });
        *callback_id_cell.borrow_mut() = Some(callback_id);
    }

    // Unsubscribe when window is destroyed to prevent memory leak
    window.connect_destroy(clone!(
        #[strong]
        callback_id_cell,
        move |_| {
            if let Some(id) = callback_id_cell.borrow_mut().take() {
                MediaService::global().disconnect(id);
            }
        }
    ));

    // Notify when window is closed (for any reason)
    window.connect_close_request(move |_| {
        on_close();
        glib::Propagation::Proceed
    });

    MediaWindowHandle {
        window,
        _callback_id: callback_id_cell,
    }
}
