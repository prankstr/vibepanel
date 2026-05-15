//! Media pop-out window - standalone draggable media player controls.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::glib;
use gtk4::glib::clone;
use gtk4::prelude::*;
use gtk4::{Align, ApplicationWindow, Box as GtkBox, GestureDrag, Orientation, Window};

use crate::services::background_effect::attach_blur_surface_lifecycle;
use crate::services::callbacks::CallbackId;
use crate::services::config_manager::{ConfigManager, ThemeCallbackGuard};
use crate::services::media::MediaService;
use crate::styles::{media, surface};
use crate::widgets::media_components::{
    MediaViewController, build_album_art, build_media_controls, build_seek_section,
    build_track_info,
};

const WINDOW_ART_SIZE: i32 = 75;

const WINDOW_BLOB_MARGIN: i32 = 12;

/// Smaller blob displacement for the compact window layout.
const WINDOW_BLOB_MAX_DISPLACEMENT: f64 = 8.0;

/// Handle to the media pop-out window. Dropping disconnects the theme callback.
pub struct MediaWindowHandle {
    window: Window,
    _callback_id: Rc<RefCell<Option<CallbackId>>>,
    /// Theme-change callback guard; disconnected automatically on `Drop`.
    _theme_callback_guard: ThemeCallbackGuard,
    opacity_provider: gtk4::CssProvider,
}

impl MediaWindowHandle {
    pub fn show(&self) {
        self.window.present();
    }

    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    /// Update the window opacity (0.0 = fully transparent, 1.0 = fully opaque).
    pub fn set_opacity(&self, opacity: f64) {
        let opacity = opacity.clamp(0.0, 1.0);
        let css = format!("box {{ opacity: {}; }}", opacity);
        self.opacity_provider.load_from_string(&css);
    }
}

/// Create a new media pop-out window (not shown by default).
pub fn create_media_window<G>(
    app: Option<&gtk4::Application>,
    opacity: f64,
    on_close: G,
) -> MediaWindowHandle
where
    G: Fn() + 'static,
{
    let media_service = MediaService::global();

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
    window.set_default_size(260, 150);

    // Make the window itself transparent so only main_box background shows
    let window_css =
        "window.media-window { background: transparent; background-color: transparent; }";
    let window_provider = gtk4::CssProvider::new();
    window_provider.load_from_string(window_css);
    #[allow(deprecated)]
    window
        .style_context()
        .add_provider(&window_provider, gtk4::STYLE_PROVIDER_PRIORITY_USER + 20);

    let main_box = GtkBox::new(Orientation::Vertical, 0);
    main_box.add_css_class(media::CONTENT);
    main_box.add_css_class(surface::NO_FOCUS);
    main_box.set_size_request(260, 150);

    // Apply opacity to the entire window content (background + children)
    // We use CSS opacity on the main_box since Wayland doesn't support window-level opacity
    let opacity_provider = gtk4::CssProvider::new();
    let opacity_css = format!("box {{ opacity: {}; }}", opacity.clamp(0.0, 1.0));
    opacity_provider.load_from_string(&opacity_css);
    #[allow(deprecated)]
    main_box
        .style_context()
        .add_provider(&opacity_provider, gtk4::STYLE_PROVIDER_PRIORITY_USER + 20);

    // Drag gesture: use GestureDrag so that simple clicks on interactive
    // children (buttons, seek slider) are not hijacked into a window move.
    // begin_move is only invoked once the pointer actually moves past the
    let gesture = GestureDrag::new();
    gesture.set_button(1);
    let drag_started: Rc<Cell<bool>> = Rc::new(Cell::new(false));

    gesture.connect_drag_begin(clone!(
        #[strong]
        drag_started,
        move |_gesture, _x, _y| {
            drag_started.set(false);
        }
    ));

    gesture.connect_drag_update(clone!(
        #[weak]
        window,
        #[strong]
        drag_started,
        move |gesture, offset_x, offset_y| {
            if drag_started.get() {
                return;
            }
            // GTK drag threshold (default 8px).
            let threshold = window.settings().gtk_dnd_drag_threshold().max(1) as f64;
            if offset_x.abs() < threshold || offset_y.abs() < threshold {
                return;
            }

            let Some((start_x, start_y)) = gesture.start_point() else {
                return;
            };
            let Some(widget) = gesture.widget() else {
                return;
            };
            let Some(surface) = window.surface() else {
                return;
            };
            let Some(toplevel) = surface.downcast_ref::<gtk4::gdk::Toplevel>() else {
                return;
            };
            let Some(point) = widget.compute_point(
                &window,
                &gtk4::graphene::Point::new(start_x as f32, start_y as f32),
            ) else {
                return;
            };
            let Some(device) = gesture.device() else {
                return;
            };

            drag_started.set(true);
            toplevel.begin_move(
                &device,
                gesture.current_button() as i32,
                point.x() as f64,
                point.y() as f64,
                gesture.current_event_time(),
            );
        }
    ));
    main_box.add_controller(gesture);

    let content = GtkBox::new(Orientation::Vertical, 2);
    content.set_margin_top(0);
    content.set_margin_bottom(2);
    content.set_margin_start(4);
    content.set_margin_end(6);

    let content_row = GtkBox::new(Orientation::Horizontal, 8);
    content_row.add_css_class(media::CONTENT);
    content_row.set_size_request(-1, WINDOW_ART_SIZE);

    // Album art
    let visualizer_enabled = ConfigManager::global()
        .get_widget_option("media", "visualizer")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let (art_container, art_picture, art_placeholder_box, art_state, visualizer) = build_album_art(
        WINDOW_ART_SIZE,
        WINDOW_BLOB_MARGIN,
        WINDOW_BLOB_MAX_DISPLACEMENT,
        visualizer_enabled,
    );
    content_row.append(&art_container);

    // Info section
    let info_section = GtkBox::new(Orientation::Vertical, 0);
    info_section.set_valign(Align::End);
    info_section.set_halign(Align::End);
    info_section.set_hexpand(true);

    let (track_info_container, title_label, artist_label, album_label) = build_track_info(15, 2);
    track_info_container.set_margin_bottom(2);
    info_section.append(&track_info_container);

    let (controls_container, prev_btn, play_pause_btn, play_pause_icon, next_btn) =
        build_media_controls(&[media::WINDOW_CONTROL_BTN]);
    info_section.append(&controls_container);

    content_row.append(&info_section);
    content.append(&content_row);

    // Seek section
    let (seek_container, seek_scale, position_label, duration_label, is_seeking) =
        build_seek_section(&[media::WINDOW_SEEK_SLIDER]);
    content.append(&seek_container);

    main_box.append(&content);
    window.set_child(Some(&main_box));

    let controller = MediaViewController {
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
        visualizer,
    };

    // connect() fires immediately, so the first update_from_snapshot runs here.
    let callback_id_cell: Rc<RefCell<Option<CallbackId>>> = Rc::new(RefCell::new(None));
    {
        let callback_id = media_service.connect(move |snapshot| {
            controller.update_from_snapshot(snapshot);
        });
        *callback_id_cell.borrow_mut() = Some(callback_id);
    }

    let theme_callback_guard = attach_blur_surface_lifecycle(
        &window,
        move |_| Some(main_box.clone().upcast::<gtk4::Widget>()),
        || ConfigManager::global().surface_border_radius() as i32,
    );

    // Disconnect media updates when the GTK window is destroyed.
    // Blur cleanup is handled by attach_blur_surface_lifecycle.
    window.connect_destroy(clone!(
        #[strong]
        callback_id_cell,
        move |_| {
            if let Some(id) = callback_id_cell.borrow_mut().take() {
                MediaService::global().disconnect(id);
            }
        }
    ));

    window.connect_close_request(move |_| {
        on_close();
        gtk4::glib::Propagation::Proceed
    });

    MediaWindowHandle {
        window,
        _callback_id: callback_id_cell,
        _theme_callback_guard: theme_callback_guard,
        opacity_provider,
    }
}
