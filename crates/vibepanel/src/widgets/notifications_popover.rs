//! Notification popover content for displaying the notification list.
//!
//! This module handles the popover that appears when clicking the notification
//! bell icon, showing a scrollable list of notifications with dismiss controls.

use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, Image, Label, Orientation, PolicyType, ScrolledWindow, glib,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::services::icons::IconsService;
use crate::services::notification::{
    Notification, NotificationService, URGENCY_CRITICAL, URGENCY_LOW,
};
use crate::services::tooltip::TooltipManager;
use crate::styles::{button, card, color, notification as notif, surface};

use super::notifications_common::{
    BODY_TRUNCATE_THRESHOLD, POPOVER_MAX_VISIBLE_ROWS, POPOVER_ROW_HEIGHT, POPOVER_WIDTH,
    create_notification_image_widget, format_timestamp, sanitize_body_markup,
};

/// Callback type for closing the popover from within the content.
pub type ClosePopoverCallback = Rc<dyn Fn()>;

// Buffer values to account for CSS padding/margins not included in measure().
// These mirror the rules in widgets/css.rs:
//
//   .notification-list { padding: 8px 0 0 0; }
//   .notification-row  { padding: 6px 6px; margin-bottom: 4px; }
//
// If you change those CSS values, update these constants to match.

/// Top padding on .notification-list
const LIST_PADDING_TOP: i32 = 8;

/// Per-row: vertical padding (6px * 2) + margin-bottom (4px)
const ROW_PADDING_AND_MARGIN: i32 = 16;

/// Extra slop per row for rounding / fractional scaling
const ROW_SLOP: i32 = 4;

/// Base slop for the container
const BASE_SLOP: i32 = 8;

/// Build the full popover content widget.
///
/// # Arguments
/// * `on_close` - Optional callback to close the popover. Called when user clicks
///   action buttons (like "Open") that should dismiss the popover. Dismissing a
///   single notification does NOT close the popover.
pub(super) fn build_popover_content(on_close: Option<ClosePopoverCallback>) -> gtk4::Widget {
    let root = GtkBox::new(Orientation::Vertical, 0);
    root.add_css_class(notif::POPOVER);
    root.set_size_request(POPOVER_WIDTH, -1);

    let header = build_header(on_close.clone());
    root.append(&header);

    let notification_list = GtkBox::new(Orientation::Vertical, 0);
    notification_list.add_css_class(notif::LIST);

    populate_notification_list(&notification_list, on_close);

    let max_height = POPOVER_MAX_VISIBLE_ROWS * POPOVER_ROW_HEIGHT;

    // Measure the natural height of the notification list content.
    // This captures variable row heights (actions, long bodies, etc.) but
    // doesn't include CSS padding/margins, so we add a buffer derived from
    // the known CSS rules (see constants at top of file).
    let (_, natural_height, _, _) = notification_list.measure(Orientation::Vertical, -1);
    let child_count = notification_list.observe_children().n_items() as i32;

    let css_buffer =
        LIST_PADDING_TOP + BASE_SLOP + child_count * (ROW_PADDING_AND_MARGIN + ROW_SLOP);

    let content_height = (natural_height + css_buffer).min(max_height);

    let scrolled = ScrolledWindow::new();
    scrolled.set_policy(PolicyType::Never, PolicyType::Automatic);
    scrolled.set_min_content_height(content_height);
    scrolled.set_max_content_height(max_height);
    scrolled.add_css_class(notif::SCROLL);

    scrolled.set_child(Some(&notification_list));
    root.append(&scrolled);

    root.upcast()
}

fn build_header(on_close: Option<ClosePopoverCallback>) -> GtkBox {
    let header = GtkBox::new(Orientation::Horizontal, 8);
    header.add_css_class(notif::HEADER);

    let title = Label::new(Some("Notifications"));
    title.add_css_class(surface::POPOVER_TITLE);
    title.set_hexpand(true);
    title.set_xalign(0.0);
    title.set_valign(Align::Start);
    header.append(&title);

    let service = NotificationService::global();
    let tooltip_manager = TooltipManager::global();
    let icons = IconsService::global();

    // Mute toggle button (always visible)
    let mute_btn = crate::widgets::base::vp_button();
    mute_btn.set_has_frame(false);
    mute_btn.set_focusable(false);
    mute_btn.set_focus_on_click(false);
    mute_btn.add_css_class(surface::POPOVER_ICON_BTN);
    mute_btn.set_valign(Align::Start);

    let is_muted = service.is_muted();
    let mute_icon_handle = icons.create_icon(
        if is_muted {
            "notifications-disabled"
        } else {
            "notifications"
        },
        &[color::PRIMARY, notif::HEADER_ICON],
    );
    let mute_icon_widget = mute_icon_handle.widget();
    mute_icon_widget.set_halign(Align::Center);
    mute_icon_widget.set_valign(Align::Center);
    mute_btn.set_child(Some(&mute_icon_widget));
    tooltip_manager.set_styled_tooltip(
        &mute_btn,
        if is_muted {
            "Unmute notifications"
        } else {
            "Mute notifications"
        },
    );

    // Store icon handle in RefCell for the click handler
    let mute_icon_handle = Rc::new(RefCell::new(mute_icon_handle));
    let mute_icon_handle_clone = Rc::clone(&mute_icon_handle);

    mute_btn.connect_clicked(move |btn| {
        let service = NotificationService::global();
        let tooltip_manager = TooltipManager::global();
        tooltip_manager.cancel_and_hide();
        service.toggle_muted();

        // Update icon and tooltip
        let is_muted = service.is_muted();
        mute_icon_handle_clone.borrow().set_icon(if is_muted {
            "notifications-disabled"
        } else {
            "notifications"
        });
        tooltip_manager.set_styled_tooltip(
            btn,
            if is_muted {
                "Unmute notifications"
            } else {
                "Mute notifications"
            },
        );
    });

    header.append(&mute_btn);

    // Clear all button (only when there are notifications)
    let count = service.count();

    if count > 0 {
        let clear_btn = crate::widgets::base::vp_button();
        clear_btn.set_has_frame(false);
        clear_btn.set_focusable(false);
        clear_btn.set_focus_on_click(false);
        clear_btn.add_css_class(surface::POPOVER_ICON_BTN);
        clear_btn.set_valign(Align::Start);
        tooltip_manager.set_styled_tooltip(&clear_btn, "Clear all notifications");

        let clear_icon =
            icons.create_icon("user-trash-symbolic", &[color::PRIMARY, notif::HEADER_ICON]);
        let clear_icon_widget = clear_icon.widget();
        clear_icon_widget.set_halign(Align::Center);
        clear_icon_widget.set_valign(Align::Center);
        clear_btn.set_child(Some(&clear_icon_widget));

        clear_btn.connect_clicked(move |_| {
            TooltipManager::global().cancel_and_hide();
            NotificationService::global().close_all();
            // Close popover after clearing all - user is done with notifications
            if let Some(ref close_cb) = on_close {
                close_cb();
            }
        });

        header.append(&clear_btn);
    }

    header
}

/// Populate the notification list with current notifications or empty state.
fn populate_notification_list(list: &GtkBox, on_close: Option<ClosePopoverCallback>) {
    let service = NotificationService::global();

    if !service.backend_available() {
        add_empty_state(
            list,
            "Another notification daemon is running.\nDisable it to use this notification center.",
        );
        return;
    }

    let mut notifications = service.notifications();

    if notifications.is_empty() {
        add_empty_state(list, "No notifications");
        return;
    }

    // Sort by timestamp (newest first)
    notifications.sort_by(|a, b| {
        b.timestamp
            .partial_cmp(&a.timestamp)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    for notification in &notifications {
        let row = build_notification_row(notification, on_close.clone());
        list.append(&row);
    }
}

fn add_empty_state(list: &GtkBox, message: &str) {
    let empty = GtkBox::new(Orientation::Vertical, 8);
    empty.add_css_class(notif::EMPTY);
    empty.set_valign(Align::Center);
    empty.set_halign(Align::Center);
    empty.set_vexpand(true);

    // Icon
    let empty_icon = Image::from_icon_name("notifications-disabled-symbolic");
    empty_icon.set_pixel_size(32);
    empty_icon.add_css_class(notif::EMPTY_ICON);
    empty_icon.add_css_class(color::MUTED);
    empty_icon.set_opacity(0.5);
    empty.append(&empty_icon);

    // Message
    let label = Label::new(Some(message));
    label.add_css_class(notif::EMPTY_LABEL);
    label.add_css_class(color::MUTED);
    label.set_justify(gtk4::Justification::Center);
    label.set_wrap(true);
    label.set_max_width_chars(50);
    empty.append(&label);

    list.append(&empty);
}

fn build_notification_row(
    notification: &Notification,
    on_close: Option<ClosePopoverCallback>,
) -> GtkBox {
    let card = GtkBox::new(Orientation::Vertical, 0);
    card.add_css_class(notif::ROW);
    card.add_css_class(card::BASE);

    // Add urgency class
    if notification.urgency == URGENCY_CRITICAL {
        card.add_css_class(notif::CRITICAL);
    } else if notification.urgency == URGENCY_LOW {
        card.add_css_class(notif::LOW);
    }

    // Main content row: icon + text + dismiss
    let main_row = GtkBox::new(Orientation::Horizontal, 8);
    card.append(&main_row);

    // App icon / avatar in a centered column
    let icon_container = GtkBox::new(Orientation::Vertical, 0);
    icon_container.set_halign(Align::Center);
    icon_container.set_valign(Align::Start);
    icon_container.set_width_request(56);

    let icon = create_notification_image_widget(notification);
    icon.add_css_class(notif::ROW_ICON);
    icon.set_halign(Align::Center);
    icon_container.append(&icon);

    main_row.append(&icon_container);

    // Content area
    let content = GtkBox::new(Orientation::Vertical, 2);
    content.set_hexpand(true);
    content.add_css_class(notif::ROW_CONTENT);

    // Top row: app name + timestamp
    let top_row = GtkBox::new(Orientation::Horizontal, 4);

    let app_label = Label::new(Some(&notification.app_name));
    app_label.add_css_class(notif::APP_NAME);
    app_label.add_css_class(color::MUTED);
    app_label.set_xalign(0.0);
    app_label.set_hexpand(true);
    app_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    top_row.append(&app_label);

    let time_label = Label::new(Some(&format_timestamp(notification.timestamp)));
    time_label.add_css_class(notif::TIMESTAMP);
    time_label.add_css_class(color::MUTED);
    top_row.append(&time_label);

    content.append(&top_row);

    // Summary
    if !notification.summary.is_empty() {
        let summary_label = Label::new(Some(&notification.summary));
        summary_label.add_css_class(notif::SUMMARY);
        summary_label.set_xalign(0.0);
        summary_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        summary_label.set_single_line_mode(true);
        content.append(&summary_label);
    }

    // Body with expandable support for long text
    // Use a single label with dynamic line limiting to avoid breaking markup tags
    let mut body_label_opt: Option<Label> = None;

    if !notification.body.is_empty() {
        // Sanitize markup and clean up for display
        let body_markup = sanitize_body_markup(&notification.body);
        let body_clean = body_markup.replace('\n', " ");
        let body_clean = body_clean.trim();
        let needs_expansion = body_clean.chars().count() > BODY_TRUNCATE_THRESHOLD;

        let body_label = Label::new(None);
        body_label.set_markup(body_clean);
        body_label.add_css_class(notif::BODY);
        body_label.add_css_class(color::MUTED);
        body_label.set_xalign(0.0);
        body_label.set_wrap(true);
        body_label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);

        if needs_expansion {
            // Start collapsed: limit to 2 lines with ellipsis
            body_label.set_lines(2);
            body_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            body_label.set_vexpand(false);
            body_label_opt = Some(body_label.clone());
        } else {
            // Short body - no line limit
            body_label.set_lines(-1);
            body_label.set_ellipsize(gtk4::pango::EllipsizeMode::None);
        }

        // Handle link activation manually to avoid Wayland protocol errors.
        // Protocol error 71 often occurs when gtk_show_uri triggers a focus switch or
        // interaction that conflicts with the layer shell surface state.
        let on_close_link = on_close.clone();
        body_label.connect_activate_link(move |_, uri| {
            // Use xdg-open via spawn_command_line_async for a detached process
            let cmd = format!("xdg-open '{}'", uri.replace("'", "'\\''"));
            // We ignore the result here because this is a fire-and-forget operation
            // and we can't do much if xdg-open fails to launch from here anyway.
            let _ = glib::spawn_command_line_async(&cmd);

            // Close popover when user navigates away via link
            if let Some(ref close_cb) = on_close_link {
                close_cb();
            }

            glib::Propagation::Stop // Stop propagation to default handler
        });

        content.append(&body_label);
    }

    main_row.append(&content);

    let dismiss_btn = Button::new();
    dismiss_btn.set_has_frame(false);
    dismiss_btn.add_css_class(notif::DISMISS_BTN);
    dismiss_btn.add_css_class(button::RESET);
    dismiss_btn.set_valign(Align::Start);
    dismiss_btn.set_tooltip_text(Some("Dismiss"));

    let dismiss_icon = Image::from_icon_name("window-close-symbolic");
    dismiss_icon.add_css_class(notif::DISMISS_ICON);
    dismiss_icon.set_halign(Align::Center);
    dismiss_icon.set_valign(Align::Center);
    dismiss_btn.set_child(Some(&dismiss_icon));

    let notification_id = notification.id;
    dismiss_btn.connect_clicked(move |_| {
        NotificationService::global().close(notification_id);
    });

    main_row.append(&dismiss_btn);

    // Actions at the bottom (non-default actions) and optional expand button
    let non_default_actions: Vec<_> = notification
        .actions
        .iter()
        .filter(|(id, _)| id != "default")
        .collect();

    let has_expand = body_label_opt.is_some();

    // Determine primary action (default or explicit "Open")
    let mut default_action: Option<String> = None;
    let mut open_action: Option<String> = None;

    for (id, label) in &notification.actions {
        if id == "default" {
            default_action = Some(id.clone());
        } else if label == "Open" && open_action.is_none() {
            open_action = Some(id.clone());
        }
    }

    let primary_action = default_action.clone().or(open_action.clone());

    if !non_default_actions.is_empty() || has_expand || primary_action.is_some() {
        let actions_row = GtkBox::new(Orientation::Horizontal, 8);
        actions_row.add_css_class(notif::ACTIONS);

        // Optional expand button on the left
        if let Some(body_label) = body_label_opt {
            let expand_btn = crate::widgets::base::vp_button_with_label("Show more");
            expand_btn.add_css_class(notif::ACTION_BTN);
            expand_btn.add_css_class(button::GHOST);

            // Store expanded state in a Cell
            let is_expanded = Rc::new(Cell::new(false));
            let is_expanded_clone = Rc::clone(&is_expanded);

            expand_btn.connect_clicked(move |btn| {
                let expanded = is_expanded_clone.get();
                let new_state = !expanded;
                is_expanded_clone.set(new_state);

                if new_state {
                    // Expanded: remove line limit and ellipsis
                    body_label.set_lines(-1);
                    body_label.set_ellipsize(gtk4::pango::EllipsizeMode::None);
                    // Ensure the label can expand vertically in the container
                    body_label.set_vexpand(true);
                    btn.set_label("Show less");
                } else {
                    // Collapsed: limit to 2 lines with ellipsis
                    body_label.set_lines(2);
                    body_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                    body_label.set_vexpand(false);
                    btn.set_label("Show more");
                }
            });

            actions_row.append(&expand_btn);
        }

        // Spacer between expand button and actions
        if has_expand && (!non_default_actions.is_empty() || primary_action.is_some()) {
            let spacer = GtkBox::new(Orientation::Horizontal, 0);
            spacer.set_hexpand(true);
            actions_row.append(&spacer);
        } else if !has_expand {
            actions_row.set_halign(Align::End);
        }

        // Primary "Open" action button, if available
        if let Some(primary_id) = primary_action {
            let open_btn = crate::widgets::base::vp_button_with_label("Open");
            open_btn.add_css_class(notif::ACTION_BTN);
            open_btn.add_css_class(button::GHOST);

            let notification_id = notification.id;
            let on_close_for_open = on_close.clone();
            open_btn.connect_clicked(move |_| {
                NotificationService::global().invoke_action(notification_id, &primary_id);
                // Close popover when user opens/activates a notification
                if let Some(ref close_cb) = on_close_for_open {
                    close_cb();
                }
            });

            actions_row.append(&open_btn);
        }

        // Action buttons on the right (non-default actions like "Mark as Read", "Reply", etc.)
        // These do NOT close the popover - user may be processing multiple notifications
        for (action_id, action_label) in non_default_actions {
            let action_btn = crate::widgets::base::vp_button_with_label(action_label);
            action_btn.add_css_class(notif::ACTION_BTN);
            action_btn.add_css_class(button::GHOST);

            let notification_id = notification.id;
            let action_id = action_id.clone();
            action_btn.connect_clicked(move |_| {
                NotificationService::global().invoke_action(notification_id, &action_id);
            });

            actions_row.append(&action_btn);
        }

        card.append(&actions_row);
    }

    card
}
