//! Media popover - detailed media player controls and track information.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, Button, Label, Orientation, Overlay, Scale, Widget};

use crate::services::icons::{IconHandle, IconsService};
use crate::services::media::{MediaService, MediaSnapshot};
use crate::services::tooltip::TooltipManager;
use crate::styles::{icon, media, surface};
use crate::widgets::marquee_label::MarqueeLabel;
use crate::widgets::media_components::{
    ArtState, build_album_art, build_media_controls, build_seek_section, build_track_info,
    load_album_art, update_playback_controls, update_seek_position, update_track_info,
};
use crate::widgets::rounded_picture::RoundedPicture;

const POPOVER_ART_SIZE: i32 = 140;

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

/// Build a media popover content widget.
/// Returns both the root widget and a controller for live updates.
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

    // Album art
    let (art_container, art_picture, art_placeholder_box, art_state) =
        build_album_art(POPOVER_ART_SIZE);
    content_row.append(&art_container);

    // Info section
    let info_section = GtkBox::new(Orientation::Vertical, 0);
    info_section.set_valign(Align::Center);
    info_section.set_margin_start(12);

    let (track_info_container, title_label, artist_label, album_label) = build_track_info(18, 4);
    track_info_container.set_margin_bottom(16);
    info_section.append(&track_info_container);

    let (controls_container, prev_btn, play_pause_btn, play_pause_icon, next_btn) =
        build_media_controls(&[]);
    info_section.append(&controls_container);

    content_row.append(&info_section);
    container.append(&content_row);

    // Seek section
    let (seek_container, seek_scale, position_label, duration_label, is_seeking) =
        build_seek_section(&[]);
    container.append(&seek_container);

    // Overlay with pop-out button
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

    TooltipManager::global().set_styled_tooltip(&popout_btn, "Pop out");
    popout_btn.connect_clicked(move |_| on_popout());
    overlay.add_overlay(&popout_btn);

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
