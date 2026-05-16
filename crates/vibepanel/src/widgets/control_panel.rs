//! Combined clock control panel.
//!
//! This reuses the existing notification, media, and calendar popover
//! components so the clock can open a GNOME-like overview panel.

use std::cell::Cell;
use std::process::{Command, Stdio};
use std::rc::Rc;

use chrono::Local;
use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, Label, Orientation, Widget};
use gtk4::{gio, glib};

use crate::services::config_manager::ConfigManager;
use crate::services::media::MediaService;
use crate::styles::surface;
use crate::widgets::calendar_popover::build_clock_calendar_popover;
use crate::widgets::media_popover::build_media_popover_with_controller;
use crate::widgets::notifications_popover::build_popover_content as build_notifications_content;

/// Build the clock control panel content and return a refresh callback.
pub fn build_clock_control_panel(
    show_week_numbers: bool,
    weather_widget_name: Option<String>,
) -> (Widget, Rc<dyn Fn()>) {
    let root = GtkBox::new(Orientation::Horizontal, 12);
    root.add_css_class("control-panel");

    let left_col = GtkBox::new(Orientation::Vertical, 10);
    left_col.add_css_class("control-panel-left");
    left_col.set_size_request(380, -1);
    left_col.set_vexpand(true);

    let suppress_rebuild = Rc::new(Cell::new(false));
    let notifications = build_notifications_content(None, suppress_rebuild);
    notifications.add_css_class("control-panel-notifications");
    left_col.append(&notifications);

    let right_col = GtkBox::new(Orientation::Vertical, 10);
    right_col.add_css_class("control-panel-right");
    right_col.set_size_request(360, -1);
    right_col.set_vexpand(true);

    let time_card = build_time_weather_card(weather_widget_name);
    right_col.append(&time_card.container);

    let (media_widget, media_controller) = build_media_popover_with_controller(|| {});
    media_widget.add_css_class("control-panel-media");
    right_col.append(&media_widget);

    let (calendar_widget, calendar_refresh) = build_clock_calendar_popover(show_week_numbers);
    calendar_widget.add_css_class("control-panel-calendar");
    right_col.append(&calendar_widget);

    root.append(&left_col);
    root.append(&right_col);

    let refresh = {
        let time_label = time_card.time_label.clone();
        let date_label = time_card.date_label.clone();
        let weather_label = time_card.weather_label.clone();
        Rc::new(move || {
            refresh_time_labels(&time_label, &date_label);
            refresh_weather_label(&weather_label);
            media_controller.update_from_snapshot(&MediaService::global().snapshot());
            calendar_refresh();
        }) as Rc<dyn Fn()>
    };

    refresh();
    (root.upcast(), refresh)
}

struct TimeWeatherCard {
    container: GtkBox,
    time_label: Label,
    date_label: Label,
    weather_label: Option<(Label, String)>,
}

fn build_time_weather_card(weather_widget_name: Option<String>) -> TimeWeatherCard {
    let container = GtkBox::new(Orientation::Vertical, 2);
    container.add_css_class("control-panel-card");
    container.add_css_class("control-panel-time-weather");

    let time_label = Label::new(None);
    time_label.add_css_class("control-panel-time");
    time_label.set_xalign(0.0);
    time_label.set_halign(Align::Fill);

    let date_label = Label::new(None);
    date_label.add_css_class(surface::POPOVER_TITLE);
    date_label.add_css_class("control-panel-date");
    date_label.set_xalign(0.0);
    date_label.set_halign(Align::Fill);

    let weather_label = weather_widget_name.map(|widget_name| {
        let label = Label::new(None);
        label.add_css_class("control-panel-weather");
        label.set_xalign(0.0);
        label.set_halign(Align::Fill);
        (label, widget_name)
    });

    container.append(&time_label);
    container.append(&date_label);
    if let Some((ref label, _)) = weather_label {
        container.append(label);
    }

    TimeWeatherCard {
        container,
        time_label,
        date_label,
        weather_label,
    }
}

fn refresh_time_labels(time_label: &Label, date_label: &Label) {
    let now = Local::now();
    time_label.set_label(&now.format("%H:%M").to_string());
    date_label.set_label(&now.format("%A, %B %-d").to_string());
}

fn refresh_weather_label(label: &Option<(Label, String)>) {
    let Some((label, widget_name)) = label else {
        return;
    };

    let cmd = ConfigManager::global()
        .get_widget_option(widget_name, "exec")
        .and_then(|v| v.as_str().map(str::to_string));

    let Some(cmd) = cmd else {
        return;
    };

    let label = label.clone();
    glib::spawn_future_local(async move {
        let result = gio::spawn_blocking(move || {
            Command::new("sh")
                .args(["-c", &cmd])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()
                .and_then(|output| {
                    if output.status.success() {
                        String::from_utf8(output.stdout).ok()
                    } else {
                        None
                    }
                })
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .await;

        if let Ok(Some(text)) = result {
            label.set_label(&text);
        }
    });
}
