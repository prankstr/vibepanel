//! Media pop-out window - standalone draggable media player controls.
//!
//! This creates a regular GTK window (NOT layer-shell) that:
//! - Can be dragged around by the user
//! - Persists when switching focus (doesn't auto-close like popovers)
//! - Is borderless/undecorated with custom header matching panel theme
//! - Has custom close/dock button to return to popover mode
//!
//! Note: Always-on-top behavior depends on the compositor/window manager.
//! On Wayland, this is typically controlled by the compositor, not the app.

// This module is scaffolding for a planned "pop-out" feature where the media
// widget can be detached from the bar into a standalone always-on-top window.
// The public API is intentionally exposed for future integration.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::glib::{self, clone};
use gtk4::prelude::*;
use gtk4::{
    Align, ApplicationWindow, Box as GtkBox, Button, EventControllerMotion, GestureClick, Label,
    Orientation, Scale, Separator, Window,
};

use crate::services::callbacks::CallbackId;
use crate::services::icons::{IconHandle, IconsService};
use crate::services::media::{MediaService, MediaSnapshot, PlaybackStatus, format_duration};
use crate::services::surfaces::SurfaceStyleManager;
use crate::styles::{button, color, icon, media};
use crate::widgets::media_utils::{create_media_control_button, volume_icon_name};

/// Seek step for forward/backward buttons in microseconds (10 seconds).
const SEEK_STEP_MICROSECONDS: i64 = 10_000_000;

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
    title_label: Label,
    artist_label: Label,
    album_label: Label,
    player_name_label: Label,

    // Playback controls
    play_pause_btn: Button,
    play_pause_icon: IconHandle,
    prev_btn: Button,
    next_btn: Button,
    seek_back_btn: Button,
    seek_fwd_btn: Button,

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

impl MediaWindowController {
    /// Update all UI elements from the latest media snapshot.
    fn update_from_snapshot(&self, snapshot: &MediaSnapshot) {
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
        self.seek_back_btn.set_sensitive(snapshot.can_seek);
        self.seek_fwd_btn.set_sensitive(snapshot.can_seek);

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

            // Note: GTK's Label internally checks if the text changed before
            // triggering a redraw, so we don't need to track previous values here.
            self.position_label.set_label(&format_duration(position));
            self.duration_label.set_label(&format_duration(length));
        }

        // Volume - only update if not currently being dragged
        if !*self.is_volume_changing.borrow() {
            self.volume_scale.set_value(snapshot.volume);

            // Update volume icon based on level
            self.volume_icon.set_icon(volume_icon_name(snapshot.volume));
        }
    }
}

/// Create a new media pop-out window.
///
/// The window is created but not shown. Call `handle.show()` to display it.
///
/// # Arguments
/// * `app` - Optional GTK Application to associate with the window
/// * `on_dock` - Callback invoked when the user clicks the dock button to return to popover mode
#[allow(dead_code)] // Part of public API for planned pop-out feature
pub fn create_media_window<F>(app: Option<&gtk4::Application>, on_dock: F) -> MediaWindowHandle
where
    F: Fn() + 'static,
{
    let icons = IconsService::global();
    let media_service = MediaService::global();
    let snapshot = media_service.snapshot();

    // Create window - regular GTK window, not layer-shell
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

    // Main container
    let main_box = GtkBox::new(Orientation::Vertical, 0);
    main_box.add_css_class(media::CONTENT);

    // Apply surface styling
    SurfaceStyleManager::global().apply_surface_styles(&main_box, true, None);

    // ===== Header with drag area and buttons =====
    let header = GtkBox::new(Orientation::Horizontal, 8);
    header.add_css_class(media::WINDOW_HEADER);

    // Drag handle area (most of the header)
    let drag_area = GtkBox::new(Orientation::Horizontal, 8);
    drag_area.add_css_class(media::WINDOW_DRAG);
    drag_area.set_hexpand(true);

    let player_name_label = Label::new(None);
    player_name_label.add_css_class(media::PLAYER_NAME);
    player_name_label.add_css_class(color::MUTED);
    player_name_label.set_halign(Align::Start);
    drag_area.append(&player_name_label);

    // Set up drag gesture on the drag area
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

        drag_area.add_controller(gesture);
    }

    // Also allow dragging by moving the cursor while pressed
    {
        let motion = EventControllerMotion::new();
        motion.connect_enter(clone!(
            #[weak]
            window,
            move |_, _, _| {
                window.set_cursor_from_name(Some("grab"));
            }
        ));
        motion.connect_leave(clone!(
            #[weak]
            window,
            move |_| {
                window.set_cursor(gtk4::gdk::Cursor::from_name("default", None).as_ref());
            }
        ));
        drag_area.add_controller(motion);
    }

    header.append(&drag_area);

    // Dock button (return to popover)
    let dock_icon = icons.create_icon("dock_to_bottom", &[icon::ICON]);
    let dock_btn = Button::new();
    dock_btn.set_child(Some(&dock_icon.widget()));
    dock_btn.add_css_class(media::WINDOW_DOCK);
    dock_btn.add_css_class(button::GHOST);
    dock_btn.set_tooltip_text(Some("Dock to panel"));
    dock_btn.connect_clicked(clone!(
        #[weak]
        window,
        move |_| {
            window.close();
            on_dock();
        }
    ));
    header.append(&dock_btn);

    // Close button
    let close_icon = icons.create_icon("close", &[icon::ICON]);
    let close_btn = Button::new();
    close_btn.set_child(Some(&close_icon.widget()));
    close_btn.add_css_class(media::WINDOW_CLOSE);
    close_btn.add_css_class(button::GHOST);
    close_btn.set_tooltip_text(Some("Close"));
    close_btn.connect_clicked(clone!(
        #[weak]
        window,
        move |_| {
            window.close();
        }
    ));
    header.append(&close_btn);

    main_box.append(&header);

    // Separator after header
    let header_sep = Separator::new(Orientation::Horizontal);
    main_box.append(&header_sep);

    // ===== Content area (similar to popover) =====
    let content = GtkBox::new(Orientation::Vertical, 12);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(16);
    content.set_margin_end(16);

    // Album art placeholder
    let art_box = GtkBox::new(Orientation::Vertical, 0);
    art_box.add_css_class(media::ART);
    art_box.add_css_class(media::ART_PLACEHOLDER);
    art_box.set_size_request(200, 200);
    art_box.set_halign(Align::Center);

    let art_icon = icons.create_icon("album", &[media::EMPTY_ICON]);
    art_icon.widget().set_valign(Align::Center);
    art_icon.widget().set_vexpand(true);
    art_box.append(&art_icon.widget());

    content.append(&art_box);

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

    content.append(&info_section);

    // Seek bar section
    let seek_section = GtkBox::new(Orientation::Vertical, 4);
    seek_section.add_css_class(media::SEEK);

    let is_seeking = Rc::new(RefCell::new(false));
    let seek_scale = Scale::with_range(Orientation::Horizontal, 0.0, 1.0, 1.0);
    seek_scale.add_css_class(media::SEEK_SLIDER);
    seek_scale.set_draw_value(false);
    seek_scale.set_hexpand(true);

    // Track seek start/end
    seek_scale.connect_change_value(clone!(
        #[strong]
        is_seeking,
        move |_, _, _| {
            *is_seeking.borrow_mut() = true;
            glib::Propagation::Proceed
        }
    ));

    // Apply seek when released
    seek_scale.connect_value_changed(clone!(
        #[strong]
        is_seeking,
        #[weak]
        seek_scale,
        move |_| {
            if *is_seeking.borrow() {
                let position = seek_scale.value() as i64;
                MediaService::global().set_position(position);
                let is_seeking_for_timeout = is_seeking.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(100), move || {
                    *is_seeking_for_timeout.borrow_mut() = false;
                });
            }
        }
    ));

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
    content.append(&seek_section);

    // Playback controls
    let controls = GtkBox::new(Orientation::Horizontal, 16);
    controls.add_css_class(media::CONTROLS);
    controls.set_halign(Align::Center);

    // Previous track button
    let prev_btn = create_media_control_button(
        &icons,
        "skip_previous",
        "Previous track",
        &[media::CONTROL_BTN, button::GHOST],
        || MediaService::global().previous(),
    );
    controls.append(&prev_btn);

    // Seek backward button (-10 seconds)
    let seek_back_btn = create_media_control_button(
        &icons,
        "replay_10",
        "Seek -10s",
        &[media::CONTROL_BTN, button::GHOST],
        || MediaService::global().seek(-SEEK_STEP_MICROSECONDS),
    );
    controls.append(&seek_back_btn);

    // Play/pause button (special styling, needs icon handle for updates)
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

    // Seek forward button (+10 seconds)
    let seek_fwd_btn = create_media_control_button(
        &icons,
        "forward_10",
        "Seek +10s",
        &[media::CONTROL_BTN, button::GHOST],
        || MediaService::global().seek(SEEK_STEP_MICROSECONDS),
    );
    controls.append(&seek_fwd_btn);

    // Next track button
    let next_btn = create_media_control_button(
        &icons,
        "skip_next",
        "Next track",
        &[media::CONTROL_BTN, button::GHOST],
        || MediaService::global().next(),
    );
    controls.append(&next_btn);

    content.append(&controls);

    // Separator
    let separator = Separator::new(Orientation::Horizontal);
    content.append(&separator);

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
    volume_scale.connect_change_value(clone!(
        #[strong]
        is_volume_changing,
        move |_, _, _| {
            *is_volume_changing.borrow_mut() = true;
            glib::Propagation::Proceed
        }
    ));

    // Apply volume when changed
    volume_scale.connect_value_changed(clone!(
        #[strong]
        is_volume_changing,
        #[weak]
        volume_scale,
        move |_| {
            if *is_volume_changing.borrow() {
                let volume = volume_scale.value();
                MediaService::global().set_volume(volume);
                let is_vol_for_timeout = is_volume_changing.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(100), move || {
                    *is_vol_for_timeout.borrow_mut() = false;
                });
            }
        }
    ));

    volume_section.append(&volume_scale);
    content.append(&volume_section);

    main_box.append(&content);
    window.set_child(Some(&main_box));

    // Build controller
    let controller = MediaWindowController {
        title_label,
        artist_label,
        album_label,
        player_name_label,
        play_pause_btn,
        play_pause_icon,
        prev_btn,
        next_btn,
        seek_back_btn,
        seek_fwd_btn,
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

    MediaWindowHandle {
        window,
        _callback_id: callback_id_cell,
    }
}
