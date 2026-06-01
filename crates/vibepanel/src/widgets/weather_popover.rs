//! Weather popover - current conditions hero, key metrics, and a daily
//! forecast strip.
//!
//! The content is refreshed from the latest `WeatherService` snapshot each time
//! the popover opens, so it always reflects current data.

use chrono::NaiveDate;
use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, Label, Orientation, Widget};
use std::rc::Rc;

use crate::services::icons::IconsService;
use crate::services::weather::{CurrentWeather, DailyForecast, WeatherService, WeatherSnapshot};
use crate::styles::{color, weather_popover as wp};
use crate::widgets::weather::{format_temperature, format_wind, icon_for_current};
use vibepanel_core::config::WeatherUnits;

const HERO_DETAIL_MAX_WIDTH_CHARS: i32 = 16;

/// Build reusable weather content plus a refresh callback for reused popovers.
pub fn build_weather_content_reactive() -> (Widget, Rc<dyn Fn()>) {
    let snapshot = WeatherService::global().snapshot();
    let container = build_weather_content_box(&snapshot);

    let refresh_container = container.clone();
    let refresh = Rc::new(move || {
        while let Some(child) = refresh_container.first_child() {
            refresh_container.remove(&child);
        }

        let snapshot = WeatherService::global().snapshot();
        populate_weather_content(&refresh_container, &snapshot);
    });

    (container.upcast::<Widget>(), refresh)
}

fn build_weather_content_box(snapshot: &WeatherSnapshot) -> GtkBox {
    let container = GtkBox::new(Orientation::Vertical, 12);
    populate_weather_content(&container, snapshot);
    container
}

fn populate_weather_content(container: &GtkBox, snapshot: &WeatherSnapshot) {
    if !snapshot.available || (snapshot.current.is_none() && snapshot.error.is_some()) {
        container.append(&build_empty_state(snapshot));
        return;
    }

    let Some(current) = snapshot.current.as_ref() else {
        // Available but no data yet (first fetch in flight, no cache).
        container.append(&build_empty_state(snapshot));
        return;
    };

    container.append(&build_hero(snapshot, current));

    if !snapshot.daily.is_empty() {
        container.append(&build_forecast_strip(snapshot));
    }

    if let Some(banner) = build_status_banner(snapshot) {
        container.append(&banner);
    }
}

fn build_hero(snapshot: &WeatherSnapshot, current: &CurrentWeather) -> GtkBox {
    let hero = GtkBox::new(Orientation::Horizontal, 24);
    hero.add_css_class(wp::HERO);

    // Left column: big icon + temperature/condition/detail.
    let left = GtkBox::new(Orientation::Horizontal, 12);
    left.set_hexpand(true);
    left.set_valign(Align::Center);

    let icons = IconsService::global();
    let weather_icon = icons.create_icon(icon_for_current(current), &[wp::HERO_ICON]);
    weather_icon.widget().set_valign(Align::Center);
    left.append(&weather_icon.widget());

    let text_column = GtkBox::new(Orientation::Vertical, 2);
    text_column.set_valign(Align::Center);

    let temp = Label::new(Some(&format_temperature(
        current.temperature,
        snapshot.units,
        false,
    )));
    temp.add_css_class(wp::TEMP);
    temp.set_halign(Align::Start);
    text_column.append(&temp);

    let condition = Label::new(Some(&current.condition));
    condition.add_css_class(wp::CONDITION);
    condition.set_halign(Align::Start);
    text_column.append(&condition);

    if let Some(feels_like) = current.feels_like {
        let detail = build_hero_detail_label(&format!(
            "Feels like {}",
            format_temperature(feels_like, snapshot.units, false)
        ));
        text_column.append(&detail);
    }

    if let Some(location) = &snapshot.location {
        let name = fit_location_label(location.name.trim());
        if !name.is_empty() {
            let location_label = build_hero_detail_label(&name);
            text_column.append(&location_label);
        }
    }

    left.append(&text_column);
    hero.append(&left);

    // Right column: key metrics grid.
    hero.append(&build_metrics(snapshot, current));

    hero
}

fn build_hero_detail_label(text: &str) -> Label {
    let label = Label::new(Some(text));
    label.add_css_class(wp::DETAIL);
    label.add_css_class(color::MUTED);
    label.set_halign(Align::Start);
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    label.set_max_width_chars(HERO_DETAIL_MAX_WIDTH_CHARS);
    label
}

/// Choose the widest location label that fits the hero detail budget:
/// 1. full `City, Country` if it fits,
/// 2. otherwise the leading segment (city) alone,
/// 3. otherwise the city, left to ellipsize at render time.
fn fit_location_label(name: &str) -> String {
    let max = HERO_DETAIL_MAX_WIDTH_CHARS as usize;

    if name.chars().count() <= max {
        return name.to_string();
    }

    // Labels are formatted as "City, Country"; the city is the first segment.
    let city = name.split(',').next().map(str::trim).unwrap_or(name);
    city.to_string()
}

fn build_metrics(snapshot: &WeatherSnapshot, current: &CurrentWeather) -> GtkBox {
    // Nested GtkBox layout (not GtkGrid): GtkGrid's height-for-width negotiation
    // reported an unstable natural height across measure passes, which produced
    // a one-frame too-tall popover outline on open (animations on) and a
    // layer-shell surface that failed to re-map on reopen (animations off).
    let metrics = GtkBox::new(Orientation::Horizontal, 12);
    metrics.add_css_class(wp::METRICS);
    metrics.set_valign(Align::Center);
    metrics.set_halign(Align::End);

    let wind = format_wind(current.wind_speed, snapshot.wind_units);
    let humidity = current
        .humidity
        .map(|value| format!("{value}%"))
        .unwrap_or_else(|| "—".to_string());
    let today = snapshot.daily.first();
    let sunrise = format_sun_time(today.and_then(|day| day.sunrise.as_deref()));
    let sunset = format_sun_time(today.and_then(|day| day.sunset.as_deref()));
    let uv_index = format_uv_index(today.and_then(|day| day.uv_index_max));
    let precipitation_amount =
        format_precipitation_amount(today.and_then(|day| day.precipitation_sum), snapshot.units);
    let precipitation_chance = today
        .and_then(|day| day.precipitation_probability)
        .map(|value| format!("{value}%"))
        .unwrap_or_else(|| "—".to_string());
    let precipitation = format!("{precipitation_amount} / {precipitation_chance}");

    // Two vertical columns keep each column's icon/text origin aligned while the
    // whole metrics block stays right-aligned in the hero card.
    metrics.append(&build_metric_column([
        build_metric("weather-humidity", "Humidity", &humidity),
        build_metric("weather-wind", "Wind", &wind),
        build_metric("weather-precipitation", "Precipitation", &precipitation),
    ]));
    metrics.append(&build_metric_column([
        build_metric("weather-sunrise", "Sunrise", &sunrise),
        build_metric("weather-sunset", "Sunset", &sunset),
        build_metric("weather-uv-index", "UV Index", &uv_index),
    ]));

    metrics
}

fn build_metric_column(items: [GtkBox; 3]) -> GtkBox {
    let column = GtkBox::new(Orientation::Vertical, 6);
    column.set_halign(Align::Start);
    for item in items {
        column.append(&item);
    }
    column
}

fn build_metric(icon_name: &str, label_text: &str, value_text: &str) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 8);
    row.add_css_class(wp::METRIC);

    let icons = IconsService::global();
    let metric_icon = icons.create_icon(icon_name, &[wp::METRIC_ICON]);
    metric_icon.widget().set_valign(Align::Center);
    row.append(&metric_icon.widget());

    let text_column = GtkBox::new(Orientation::Vertical, 0);
    text_column.set_valign(Align::Center);

    let label = Label::new(Some(label_text));
    label.add_css_class(wp::METRIC_LABEL);
    label.add_css_class(color::MUTED);
    label.set_halign(Align::Start);
    text_column.append(&label);

    let value = Label::new(Some(value_text));
    value.add_css_class(wp::METRIC_VALUE);
    value.set_halign(Align::Start);
    text_column.append(&value);

    row.append(&text_column);
    row
}

fn build_forecast_strip(snapshot: &WeatherSnapshot) -> GtkBox {
    let strip = GtkBox::new(Orientation::Horizontal, 6);
    strip.add_css_class(wp::FORECAST);
    strip.set_homogeneous(true);

    for (index, forecast) in snapshot.daily.iter().enumerate() {
        strip.append(&build_forecast_day(index, forecast, snapshot));
    }

    strip
}

fn build_forecast_day(
    index: usize,
    forecast: &DailyForecast,
    snapshot: &WeatherSnapshot,
) -> GtkBox {
    let day = GtkBox::new(Orientation::Vertical, 6);
    day.add_css_class(wp::DAY);
    day.set_hexpand(true);

    let name = Label::new(Some(&day_name(index, &forecast.date)));
    name.add_css_class(wp::DAY_NAME);
    name.add_css_class(color::MUTED);
    name.set_halign(Align::Center);
    day.append(&name);

    let icons = IconsService::global();
    let day_icon = icons.create_icon(icon_for_forecast(forecast), &[wp::DAY_ICON]);
    day_icon.widget().set_halign(Align::Center);
    day.append(&day_icon.widget());

    let temps = GtkBox::new(Orientation::Vertical, 1);
    temps.add_css_class(wp::DAY_TEMPS);
    temps.set_halign(Align::Center);

    let high = Label::new(Some(&forecast_degree_temp(forecast.temperature_max)));
    high.add_css_class(wp::DAY_HIGH);
    high.set_halign(Align::Center);
    temps.append(&high);

    let low = Label::new(Some(&forecast_degree_temp(forecast.temperature_min)));
    low.add_css_class(wp::DAY_LOW);
    low.add_css_class(color::MUTED);
    low.set_halign(Align::Center);
    temps.append(&low);

    day.append(&temps);

    let metrics = GtkBox::new(Orientation::Vertical, 0);
    metrics.add_css_class(wp::DAY_METRICS);
    metrics.set_halign(Align::Center);
    let wind = format_wind(forecast.wind_speed_max, snapshot.wind_units);
    let precipitation = format_precipitation_amount(forecast.precipitation_sum, snapshot.units);
    let metric_width = forecast_metric_width(&wind, &precipitation);
    metrics.append(&build_forecast_metric("weather-wind", &wind, metric_width));
    metrics.append(&build_forecast_metric(
        "weather-precipitation",
        &precipitation,
        metric_width,
    ));
    day.append(&metrics);

    day
}

fn build_forecast_metric(icon_name: &str, value: &str, width_chars: i32) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 4);
    row.add_css_class(wp::DAY_METRIC);
    row.set_hexpand(false);
    row.set_halign(Align::Center);

    let icons = IconsService::global();
    let icon = icons.create_icon(icon_name, &[wp::DAY_METRIC_ICON]);
    icon.widget().set_halign(Align::Center);
    icon.widget().set_valign(Align::Center);
    row.append(&icon.widget());

    let label = Label::new(Some(value));
    label.add_css_class(wp::DAY_METRIC_VALUE);
    label.add_css_class(color::MUTED);
    label.set_hexpand(false);
    label.set_halign(Align::Center);
    label.set_valign(Align::Center);
    label.set_justify(gtk4::Justification::Left);
    label.set_width_chars(width_chars);
    label.set_max_width_chars(width_chars);
    label.set_xalign(0.0);
    row.append(&label);

    row
}

fn forecast_metric_width(wind: &str, precipitation: &str) -> i32 {
    wind.chars()
        .count()
        .max(precipitation.chars().count())
        .clamp(1, 8) as i32
}

fn build_status_banner(snapshot: &WeatherSnapshot) -> Option<GtkBox> {
    let text = if let Some(error) = &snapshot.error {
        format!("Error: {error}")
    } else {
        return None;
    };

    let banner = GtkBox::new(Orientation::Horizontal, 0);
    banner.add_css_class(wp::BANNER);
    let label = Label::new(Some(&text));
    label.add_css_class(color::MUTED);
    label.set_halign(Align::Start);
    label.set_wrap(true);
    banner.append(&label);
    Some(banner)
}

fn build_empty_state(snapshot: &WeatherSnapshot) -> GtkBox {
    let empty = GtkBox::new(Orientation::Vertical, 8);
    empty.add_css_class(wp::EMPTY);
    empty.set_halign(Align::Center);

    let icons = IconsService::global();
    let empty_icon = icons.create_icon("weather-unknown", &[wp::EMPTY_ICON]);
    empty_icon.widget().set_halign(Align::Center);
    empty.append(&empty_icon.widget());

    let text = if let Some(error) = &snapshot.error {
        error.clone()
    } else if snapshot.loading {
        "Loading weather…".to_string()
    } else {
        "No weather data available".to_string()
    };

    let label = Label::new(Some(&text));
    label.add_css_class(wp::EMPTY_LABEL);
    label.add_css_class(color::MUTED);
    label.set_halign(Align::Center);
    label.set_justify(gtk4::Justification::Center);
    label.set_wrap(true);
    empty.append(&label);

    empty
}

fn forecast_degree_temp(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.0}°"))
        .unwrap_or_else(|| "—".to_string())
}

fn format_precipitation_amount(value: Option<f64>, units: WeatherUnits) -> String {
    let Some(value) = value else {
        return "—".to_string();
    };

    match units {
        WeatherUnits::Metric => format!("{value:.0} mm"),
        WeatherUnits::Imperial if value > 0.0 && value < 0.1 => "<0.1 in".to_string(),
        WeatherUnits::Imperial => format!("{value:.1} in"),
    }
}

fn format_sun_time(value: Option<&str>) -> String {
    let Some(value) = value else {
        return "—".to_string();
    };
    let time = value
        .rsplit_once('T')
        .map(|(_, time)| time)
        .unwrap_or(value);
    time.get(..5).unwrap_or(time).to_string()
}

fn format_uv_index(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.1}"))
        .unwrap_or_else(|| "—".to_string())
}

/// Map an Open-Meteo daily forecast to a logical icon name. Forecast rows have
/// no day/night flag, so daytime variants are used.
fn icon_for_forecast(forecast: &DailyForecast) -> &'static str {
    match forecast.weather_code {
        Some(0) => "weather-clear",
        Some(1 | 2) => "weather-partly-cloudy",
        Some(3) => "weather-cloudy",
        Some(45 | 48) => "weather-fog",
        Some(51 | 53 | 55 | 56 | 57) => "weather-drizzle",
        Some(61 | 63 | 65 | 66 | 67 | 80 | 81 | 82) => "weather-rain",
        Some(71 | 73 | 75 | 77 | 85 | 86) => "weather-snow",
        Some(95 | 96 | 99) => "weather-thunderstorm",
        _ => "weather-unknown",
    }
}

/// Render a forecast date label. The first API result represents today in the
/// weather location's timezone; later dates render as short weekday names.
fn day_name(index: usize, date: &str) -> String {
    if index == 0 {
        return "Today".to_string();
    }

    NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|parsed| parsed.format("%a").to_string())
        .unwrap_or_else(|_| date.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forecast_with_code(code: Option<i32>) -> DailyForecast {
        DailyForecast {
            date: "2026-05-30".to_string(),
            condition: "Test".to_string(),
            weather_code: code,
            temperature_min: Some(10.0),
            temperature_max: Some(20.0),
            wind_speed_max: Some(12.0),
            precipitation_sum: Some(3.0),
            precipitation_probability: Some(40),
            uv_index_max: Some(6.4),
            sunrise: Some("2026-05-30T05:31".to_string()),
            sunset: Some("2026-05-30T20:42".to_string()),
        }
    }

    #[test]
    fn fit_location_label_keeps_full_when_short() {
        // "City, Country" is within the 16-char budget.
        assert_eq!(fit_location_label("City, Country"), "City, Country");
    }

    #[test]
    fn fit_location_label_drops_country_when_too_long() {
        // "LongCity, LongCountry" is over the budget, so country is dropped.
        assert_eq!(fit_location_label("LongCity, LongCountry"), "LongCity");
    }

    #[test]
    fn fit_location_label_keeps_long_city_for_ellipsize() {
        // Single long segment is kept as-is; ellipsizing happens at render time.
        assert_eq!(
            fit_location_label("Llanfairpwllgwyngyll"),
            "Llanfairpwllgwyngyll"
        );
    }

    #[test]
    fn forecast_icons_use_daytime_variants() {
        assert_eq!(
            icon_for_forecast(&forecast_with_code(Some(0))),
            "weather-clear"
        );
        assert_eq!(
            icon_for_forecast(&forecast_with_code(Some(2))),
            "weather-partly-cloudy"
        );
        assert_eq!(
            icon_for_forecast(&forecast_with_code(Some(3))),
            "weather-cloudy"
        );
        assert_eq!(
            icon_for_forecast(&forecast_with_code(Some(61))),
            "weather-rain"
        );
        assert_eq!(
            icon_for_forecast(&forecast_with_code(Some(95))),
            "weather-thunderstorm"
        );
        assert_eq!(
            icon_for_forecast(&forecast_with_code(None)),
            "weather-unknown"
        );
    }

    #[test]
    fn day_name_parses_weekday() {
        assert_eq!(day_name(1, "2024-01-01"), "Mon");
    }

    #[test]
    fn day_name_marks_first_forecast_today() {
        assert_eq!(day_name(0, "2099-01-01"), "Today");
    }

    #[test]
    fn day_name_falls_back_on_invalid_input() {
        assert_eq!(day_name(1, "not-a-date"), "not-a-date");
    }

    #[test]
    fn precipitation_amount_uses_config_units() {
        assert_eq!(
            format_precipitation_amount(Some(3.2), WeatherUnits::Metric),
            "3 mm"
        );
        assert_eq!(
            format_precipitation_amount(Some(0.05), WeatherUnits::Imperial),
            "<0.1 in"
        );
        assert_eq!(
            format_precipitation_amount(Some(0.2), WeatherUnits::Imperial),
            "0.2 in"
        );
        assert_eq!(format_precipitation_amount(None, WeatherUnits::Metric), "—");
    }

    #[test]
    fn forecast_metric_width_tracks_longest_value() {
        assert_eq!(forecast_metric_width("5 m/s", "2 mm"), 5);
        assert_eq!(forecast_metric_width("22 m/s", "2 mm"), 6);
        assert_eq!(forecast_metric_width("—", "—"), 1);
        assert_eq!(forecast_metric_width("123456789", "2 mm"), 8);
    }

    #[test]
    fn sun_time_formats_open_meteo_local_time() {
        assert_eq!(format_sun_time(Some("2026-05-30T05:31")), "05:31");
        assert_eq!(format_sun_time(Some("05:31")), "05:31");
        assert_eq!(format_sun_time(None), "—");
    }

    #[test]
    fn uv_index_formats_one_decimal() {
        assert_eq!(format_uv_index(Some(6.44)), "6.4");
        assert_eq!(format_uv_index(None), "—");
    }
}
