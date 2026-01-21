//! Media pop-out window - standalone draggable media player controls.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::glib;
use gtk4::glib::clone;
use gtk4::prelude::*;
use gtk4::{
    Align, ApplicationWindow, Box as GtkBox, GestureClick, Label, Orientation, Scale, Window,
};

use crate::services::callbacks::CallbackId;
use crate::services::icons::IconHandle;
use crate::services::media::{MediaService, MediaSnapshot};
use crate::services::surfaces::SurfaceStyleManager;
use crate::styles::media;
use crate::widgets::marquee_label::MarqueeLabel;
use crate::widgets::media_components::{
    ArtState, build_album_art, build_media_controls, build_seek_section, build_track_info,
    load_album_art, update_playback_controls, update_seek_position, update_track_info,
};
use crate::widgets::rounded_picture::RoundedPicture;

const WINDOW_ART_SIZE: i32 = 100;

/// Handle to the media pop-out window. Drop this to close the window.
pub struct MediaWindowHandle {
    window: Window,
    _callback_id: Rc<RefCell<Option<CallbackId>>>,
    opacity_provider: gtk4::CssProvider,
}

#[allow(dead_code)]
impl MediaWindowHandle {
    pub fn show(&self) {
        self.window.present();
    }

    pub fn hide(&self) {
        self.window.set_visible(false);
    }

    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    pub fn close(&self) {
        self.window.close();
    }

    /// Update the window opacity (0.0 = fully transparent, 1.0 = fully opaque).
    pub fn set_opacity(&self, opacity: f64) {
        let opacity = opacity.clamp(0.0, 1.0);
        let css = format!("box {{ opacity: {}; }}", opacity);
        self.opacity_provider.load_from_string(&css);
    }
}

#[derive(Clone)]
struct MediaWindowController {
    title_label: Rc<MarqueeLabel>,
    artist_label: Label,
    album_label: Label,
    art_picture: RoundedPicture,
    art_placeholder_box: GtkBox,
    art_state: Rc<RefCell<ArtState>>,
    play_pause_btn: gtk4::Button,
    play_pause_icon: IconHandle,
    prev_btn: gtk4::Button,
    next_btn: gtk4::Button,
    seek_scale: Scale,
    position_label: Label,
    duration_label: Label,
    is_seeking: Rc<RefCell<bool>>,
}

impl MediaWindowController {
    fn update_from_snapshot(&self, snapshot: &MediaSnapshot) {
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
    let snapshot = media_service.snapshot();

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
    window.set_default_size(280, 150);

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
    main_box.set_size_request(280, 150);

    // Apply surface styles for consistent theming
    SurfaceStyleManager::global().apply_surface_styles(&main_box, true, None);

    // Apply opacity to the entire window content (background + children)
    // We use CSS opacity on the main_box since Wayland doesn't support window-level opacity
    let opacity_provider = gtk4::CssProvider::new();
    let opacity_css = format!("box {{ opacity: {}; }}", opacity.clamp(0.0, 1.0));
    opacity_provider.load_from_string(&opacity_css);
    #[allow(deprecated)]
    main_box
        .style_context()
        .add_provider(&opacity_provider, gtk4::STYLE_PROVIDER_PRIORITY_USER + 20);

    // Drag gesture
    let gesture = GestureClick::new();
    gesture.set_button(1);
    gesture.connect_pressed(clone!(
        #[weak]
        window,
        move |gesture, _n_press, x, y| {
            if let Some(surface) = window.surface()
                && let Some(toplevel) = surface.downcast_ref::<gtk4::gdk::Toplevel>()
                && let Some(widget) = gesture.widget()
                && let Some(point) =
                    widget.compute_point(&window, &gtk4::graphene::Point::new(x as f32, y as f32))
            {
                toplevel.begin_move(
                    gesture.device().as_ref().unwrap(),
                    gesture.current_button() as i32,
                    point.x() as f64,
                    point.y() as f64,
                    gesture.current_event_time(),
                );
            }
        }
    ));
    main_box.add_controller(gesture);

    let content = GtkBox::new(Orientation::Vertical, 4);
    content.set_margin_top(0);
    content.set_margin_bottom(4);
    content.set_margin_start(8);
    content.set_margin_end(8);

    let content_row = GtkBox::new(Orientation::Horizontal, 12);
    content_row.add_css_class(media::CONTENT);

    // Album art
    let (art_container, art_picture, art_placeholder_box, art_state) =
        build_album_art(WINDOW_ART_SIZE);
    content_row.append(&art_container);

    // Info section
    let info_section = GtkBox::new(Orientation::Vertical, 0);
    info_section.set_valign(Align::Center);
    info_section.set_size_request(160, -1);

    let (track_info_container, title_label, artist_label, album_label) = build_track_info(15, 2);
    track_info_container.set_margin_bottom(4);
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

    controller.update_from_snapshot(&snapshot);

    let callback_id_cell: Rc<RefCell<Option<CallbackId>>> = Rc::new(RefCell::new(None));
    {
        let controller = controller.clone();
        let callback_id = media_service.connect(move |snapshot| {
            controller.update_from_snapshot(snapshot);
        });
        *callback_id_cell.borrow_mut() = Some(callback_id);
    }

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
        opacity_provider,
    }
}
