//! Media popover - detailed media player controls and track information.

use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, Button, Label, Orientation, Overlay, Popover, Widget};

use crate::services::icons::IconsService;
use crate::services::media::{MediaService, PlaybackStatus};
use crate::services::surfaces::SurfaceStyleManager;
use crate::services::tooltip::TooltipManager;
use crate::styles::{button, color, icon, media, qs, surface};
use crate::widgets::base::configure_popover;
use crate::widgets::media_components::{
    MediaViewController, build_album_art, build_media_controls, build_seek_section,
    build_track_info,
};

const POPOVER_ART_SIZE: i32 = 140;

/// Type alias for the shared media view controller.
pub type MediaPopoverController = MediaViewController;

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
    info_section.set_valign(Align::End);
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

    // Overlay with header buttons
    let overlay = Overlay::new();
    overlay.add_css_class(media::POPOVER);
    overlay.set_child(Some(&container));

    // Header buttons container (player selector + popout)
    let header_btns = GtkBox::new(Orientation::Horizontal, 0);
    header_btns.set_halign(Align::End);
    header_btns.set_valign(Align::Start);

    // Player selector button
    let player_btn = Button::new();
    player_btn.set_has_frame(false);
    player_btn.set_focusable(false);
    player_btn.set_focus_on_click(false);
    player_btn.add_css_class(surface::POPOVER_ICON_BTN);
    player_btn.add_css_class(media::PLAYER_SELECTOR_BTN);

    let player_icon = icons.create_icon("audio-speakers", &[icon::ICON]);
    player_icon.widget().set_halign(Align::Center);
    player_icon.widget().set_valign(Align::Center);
    player_btn.set_child(Some(&player_icon.widget()));

    TooltipManager::global().set_styled_tooltip(&player_btn, "Select player");
    player_btn.connect_clicked(|btn| {
        show_player_menu(btn);
    });
    header_btns.append(&player_btn);

    // Pop-out button
    let popout_btn = Button::new();
    popout_btn.set_has_frame(false);
    popout_btn.set_focusable(false);
    popout_btn.set_focus_on_click(false);
    popout_btn.add_css_class(surface::POPOVER_ICON_BTN);
    popout_btn.add_css_class(media::POPOUT_BTN);

    let popout_icon = icons.create_icon("open_in_new", &[icon::ICON, media::POPOUT_ICON]);
    popout_icon.widget().set_halign(Align::Center);
    popout_icon.widget().set_valign(Align::Center);
    popout_btn.set_child(Some(&popout_icon.widget()));

    TooltipManager::global().set_styled_tooltip(&popout_btn, "Pop out");
    popout_btn.connect_clicked(move |_| on_popout());
    header_btns.append(&popout_btn);

    overlay.add_overlay(&header_btns);

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

/// Show the player selector menu.
fn show_player_menu(parent: &Button) {
    let media_service = MediaService::global();
    let players = media_service.available_players();
    let is_auto = media_service.is_auto_selection();

    // Find the currently active player name for Auto subtitle
    let active_player_name = players
        .iter()
        .find(|p| p.is_active)
        .map(|p| p.player_name.as_str());

    let popover = Popover::new();
    configure_popover(&popover);

    // Outer panel for surface styling
    let panel = GtkBox::new(Orientation::Vertical, 0);
    panel.add_css_class(surface::WIDGET_MENU_CONTENT);
    panel.add_css_class(media::PLAYER_MENU);

    // Inner content box for menu items
    let content = GtkBox::new(Orientation::Vertical, 2);
    content.add_css_class(qs::ROW_MENU_CONTENT);
    content.set_margin_top(4);
    content.set_margin_bottom(4);
    content.set_margin_start(4);
    content.set_margin_end(4);

    // Auto option - show current auto-selected player as subtitle
    let auto_btn = create_player_menu_item("Auto", active_player_name, is_auto);
    auto_btn.connect_clicked({
        let popover = popover.clone();
        move |_| {
            let media_service = MediaService::global();
            media_service.set_auto_selection();
            popover.popdown();
        }
    });
    content.append(&auto_btn);

    // Player list
    for player in players {
        let is_selected = !is_auto && player.is_active;
        let status_text = match player.playback_status {
            PlaybackStatus::Playing => Some("Playing"),
            PlaybackStatus::Paused => Some("Paused"),
            PlaybackStatus::Stopped => Some("Stopped"),
        };

        let btn = create_player_menu_item(&player.player_name, status_text, is_selected);
        let bus_name = player.bus_name.clone();
        btn.connect_clicked({
            let popover = popover.clone();
            move |_| {
                let media_service = MediaService::global();
                media_service.set_active_player(&bus_name);
                popover.popdown();
            }
        });
        content.append(&btn);
    }

    panel.append(&content);

    popover.set_child(Some(&panel));

    // Apply surface styling to the popover itself - this applies the popover-specific
    // CSS rules that set font-family on `popover.widget-menu > contents`
    SurfaceStyleManager::global().apply_surface_styles(&popover, false, None);

    // Also apply to panel for the content-specific rules
    SurfaceStyleManager::global().apply_surface_styles(&panel, true, None);

    // Apply Pango font attributes to all labels if enabled
    SurfaceStyleManager::global().apply_pango_attrs_all(&panel);

    popover.set_parent(parent);
    popover.popup();

    // Unparent popover when closed
    popover.connect_closed(|p| {
        p.unparent();
    });
}

/// Create a player menu item button.
fn create_player_menu_item(name: &str, subtitle: Option<&str>, is_active: bool) -> Button {
    let btn = Button::new();
    btn.set_has_frame(false);
    btn.add_css_class(qs::ROW_MENU_ITEM);
    btn.add_css_class(media::PLAYER_MENU_ITEM);
    btn.add_css_class(button::GHOST);

    let hbox = GtkBox::new(Orientation::Horizontal, 8);
    hbox.set_margin_start(4);
    hbox.set_margin_end(8);

    // Check icon for active item (accent colored, bold)
    let icons = IconsService::global();
    if is_active {
        let check_icon = icons.create_icon(
            "check",
            &[icon::ICON, color::ACCENT, media::PLAYER_MENU_CHECK],
        );
        hbox.append(&check_icon.widget());
    } else {
        // Spacer for alignment
        let spacer = Label::new(None);
        spacer.set_width_request(16);
        hbox.append(&spacer);
    }

    // Label container
    let label_box = GtkBox::new(Orientation::Vertical, 0);
    label_box.set_hexpand(true);

    let name_label = Label::new(Some(name));
    name_label.set_xalign(0.0);
    name_label.add_css_class(color::PRIMARY);
    name_label.add_css_class(media::PLAYER_MENU_TITLE);
    label_box.append(&name_label);

    // Subtitle (status for players, current player for Auto)
    if let Some(subtitle_text) = subtitle {
        let subtitle_label = Label::new(Some(subtitle_text));
        subtitle_label.set_xalign(0.0);
        subtitle_label.add_css_class(color::MUTED);
        subtitle_label.add_css_class(media::PLAYER_MENU_SUBTITLE);
        label_box.append(&subtitle_label);
    }

    hbox.append(&label_box);
    btn.set_child(Some(&hbox));

    btn
}
