//! Weather widget - displays current weather via the shared `WeatherService`.
//!
//! The widget is a pure service subscriber. Top-level `[weather]` owns data
//! configuration; `[widgets.weather]` only controls presentation.

use gtk4::Label;
use gtk4::prelude::*;
use std::time::{Duration, SystemTime};
use vibepanel_core::config::{WeatherUnits, WeatherWindUnits, WidgetEntry};

use crate::services::callbacks::CallbackId;
use crate::services::config_manager::ConfigManager;
use crate::services::icons::IconHandle;
use crate::services::tooltip::TooltipManager;
use crate::services::weather::{CurrentWeather, WeatherService, WeatherSnapshot};
use crate::styles::{class, widget};
use crate::widgets::base::BaseWidget;
use crate::widgets::{WidgetConfig, warn_unknown_options};

const DEFAULT_SHOW_ICON: bool = true;
const DEFAULT_FORMAT: &str = "{temperature}";

/// Presentation configuration for the weather widget.
#[derive(Debug, Clone)]
pub struct WeatherConfig {
    pub show_icon: bool,
    pub format: String,
}

impl WidgetConfig for WeatherConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options("weather", entry, &["show_icon", "format"]);

        let show_icon = entry
            .options
            .get("show_icon")
            .and_then(|v| v.as_bool())
            .unwrap_or(DEFAULT_SHOW_ICON);

        let format = entry
            .options
            .get("format")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| DEFAULT_FORMAT.to_string());

        Self { show_icon, format }
    }
}

impl Default for WeatherConfig {
    fn default() -> Self {
        Self {
            show_icon: DEFAULT_SHOW_ICON,
            format: DEFAULT_FORMAT.to_string(),
        }
    }
}

/// Weather widget that renders current conditions from `WeatherService`.
pub struct WeatherWidget {
    base: BaseWidget,
    weather_callback_id: CallbackId,
}

impl WeatherWidget {
    /// Create a new weather widget with the given presentation config.
    pub fn new(config: WeatherConfig) -> Self {
        let base = BaseWidget::new(&[widget::WEATHER]);
        Self::build(config, base, true)
    }

    /// Create a passive weather widget for use in a merge group.
    pub fn new_passive(config: WeatherConfig) -> Self {
        let base = BaseWidget::new_passive(&[widget::WEATHER]);
        Self::build(config, base, false)
    }

    fn build(config: WeatherConfig, base: BaseWidget, create_popover: bool) -> Self {
        base.set_tooltip("Weather: loading...");

        let icon_handle = base.add_icon("weather-unknown", &[widget::WEATHER_ICON]);
        let label = base.add_label(None, &[widget::WEATHER_LABEL, class::VCENTER_CAPS]);
        let is_vertical = ConfigManager::global().bar_position().is_vertical();

        icon_handle.widget().set_visible(config.show_icon);

        let menu_handle = if create_popover {
            let menu_handle = base.create_menu(move || {
                crate::widgets::weather_popover::build_weather_content_reactive().0
            });
            menu_handle.set_reuse_content(true);
            Some(menu_handle)
        } else {
            None
        };

        let service = WeatherService::global();
        let weather_callback_id = {
            let container = base.widget().clone();
            let icon_handle = icon_handle.clone();
            let label = label.clone();
            let show_icon = config.show_icon;
            let format = config.format.clone();
            let menu_handle = menu_handle.clone();

            service.connect(move |snapshot: &WeatherSnapshot| {
                update_weather_widget(
                    &container,
                    &icon_handle,
                    &label,
                    show_icon,
                    &format,
                    is_vertical,
                    snapshot,
                );
                if let Some(menu_handle) = menu_handle.as_ref() {
                    menu_handle.refresh_if_visible();
                }
            })
        };

        Self {
            base,
            weather_callback_id,
        }
    }

    /// Get the root GTK widget for embedding in the bar.
    pub fn widget(&self) -> &gtk4::Box {
        self.base.widget()
    }

    pub(crate) fn edge_interaction(&self) -> Option<crate::widgets::EdgeInteraction> {
        self.base.edge_interaction()
    }
}

impl Drop for WeatherWidget {
    fn drop(&mut self) {
        WeatherService::global().disconnect(self.weather_callback_id);
    }
}

fn update_weather_widget(
    container: &gtk4::Box,
    icon_handle: &IconHandle,
    label: &Label,
    show_icon: bool,
    format: &str,
    is_vertical: bool,
    snapshot: &WeatherSnapshot,
) {
    if !snapshot.available {
        container.set_visible(false);
        return;
    }

    container.set_visible(true);
    container.remove_css_class(widget::WEATHER_ERROR);
    container.remove_css_class(widget::WEATHER_LOADING);
    container.remove_css_class(widget::WEATHER_STALE);

    if snapshot.error.is_some() {
        container.add_css_class(widget::WEATHER_ERROR);
    }
    if snapshot.loading {
        container.add_css_class(widget::WEATHER_LOADING);
    }
    if snapshot.stale {
        container.add_css_class(widget::WEATHER_STALE);
    }

    if show_icon {
        icon_handle.widget().set_visible(true);
        if let Some(current) = &snapshot.current {
            icon_handle.set_icon(icon_for_current(current));
        } else {
            icon_handle.set_icon("weather-unknown");
        }
    } else {
        icon_handle.widget().set_visible(false);
    }

    let label_text = format_label(snapshot, format, is_vertical);
    label.set_visible(!label_text.is_empty());
    label.set_label(&label_text);

    let tooltip = format_tooltip(snapshot);
    TooltipManager::global().set_styled_tooltip(container, &tooltip);
}

/// Placeholder shown when no current weather data is available (no cached
/// snapshot yet and the first fetch has not landed). Kept minimal per design.
const PLACEHOLDER_LABEL: &str = "-";

fn format_label(snapshot: &WeatherSnapshot, format: &str, is_vertical: bool) -> String {
    let Some(current) = snapshot.current.as_ref() else {
        return PLACEHOLDER_LABEL.to_string();
    };

    if is_vertical {
        return format_temperature(current.temperature, snapshot.units, true);
    }

    format
        .replace(
            "{temperature}",
            &format_temperature(current.temperature, snapshot.units, is_vertical),
        )
        .replace("{condition}", &current.condition)
        .replace(
            "{feels_like}",
            &current
                .feels_like
                .map(|value| format_temperature(value, snapshot.units, is_vertical))
                .unwrap_or_else(|| "—".to_string()),
        )
        .replace(
            "{humidity}",
            &current
                .humidity
                .map(|value| format!("{value}%"))
                .unwrap_or_else(|| "—".to_string()),
        )
        .replace(
            "{wind}",
            &format_wind(current.wind_speed, snapshot.wind_units),
        )
}

fn format_tooltip(snapshot: &WeatherSnapshot) -> String {
    let mut lines = Vec::new();
    let location = snapshot
        .location
        .as_ref()
        .map(|location| location.name.as_str())
        .unwrap_or("Unknown location");

    if let Some(current) = &snapshot.current {
        lines.push(format!(
            "Weather: {} · {}",
            location,
            format_temperature(current.temperature, snapshot.units, false)
        ));
        lines.push(format!("Condition: {}", current.condition));
        if let Some(feels_like) = current.feels_like {
            lines.push(format!(
                "Feels like: {}",
                format_temperature(feels_like, snapshot.units, false)
            ));
        }
        if let Some(humidity) = current.humidity {
            lines.push(format!("Humidity: {humidity}%"));
        }
        if current.wind_speed.is_some() {
            lines.push(format!(
                "Wind: {}",
                format_wind(current.wind_speed, snapshot.wind_units)
            ));
        }
        if let Some(is_day) = current.is_day {
            lines.push(format!("Light: {}", if is_day { "day" } else { "night" }));
        }

        if let Some(forecast) = snapshot.daily.first() {
            lines.push(format_daily_forecast(forecast, snapshot.units));
        }
    } else if snapshot.loading {
        lines.push("Weather: loading...".to_string());
    } else {
        lines.push("Weather: unavailable".to_string());
    }

    if let Some(last_update) = snapshot.last_update {
        lines.push(format!("Updated: {}", format_last_update(last_update)));
    }

    if snapshot.stale {
        lines.push("Data may be stale".to_string());
    }
    if let Some(error) = &snapshot.error {
        lines.push(format!("Error: {error}"));
    }

    lines.join("\n")
}

fn format_daily_forecast(
    forecast: &crate::services::weather::DailyForecast,
    units: WeatherUnits,
) -> String {
    let low = forecast
        .temperature_min
        .map(|value| format_temperature(value, units, false))
        .unwrap_or_else(|| "—".to_string());
    let high = forecast
        .temperature_max
        .map(|value| format_temperature(value, units, false))
        .unwrap_or_else(|| "—".to_string());
    let precip = forecast
        .precipitation_probability
        .map(|value| format!(", rain {value}%"))
        .unwrap_or_default();

    format!(
        "Forecast {}: {}, {} / {}{}",
        forecast.date, forecast.condition, low, high, precip
    )
}

fn format_last_update(last_update: SystemTime) -> String {
    match SystemTime::now().duration_since(last_update) {
        Ok(age) if age < Duration::from_secs(60) => "just now".to_string(),
        Ok(age) if age < Duration::from_secs(60 * 60) => format!("{}m ago", age.as_secs() / 60),
        Ok(age) => format!("{}h ago", age.as_secs() / (60 * 60)),
        Err(_) => "just now".to_string(),
    }
}

pub(crate) fn format_temperature(value: f64, units: WeatherUnits, is_vertical: bool) -> String {
    if is_vertical {
        format!("{value:.0}")
    } else {
        format!("{:.0}{}", value, temperature_unit_symbol(units))
    }
}

fn temperature_unit_symbol(units: WeatherUnits) -> &'static str {
    match units {
        WeatherUnits::Metric => "°C",
        WeatherUnits::Imperial => "°F",
    }
}

pub(crate) fn format_wind(value: Option<f64>, units: WeatherWindUnits) -> String {
    match value {
        Some(value) => format!("{value:.0} {}", wind_unit_label(units)),
        None => "—".to_string(),
    }
}

fn wind_unit_label(units: WeatherWindUnits) -> &'static str {
    match units {
        WeatherWindUnits::Kmh => "km/h",
        WeatherWindUnits::Mph => "mph",
        WeatherWindUnits::MetersPerSecond => "m/s",
    }
}

pub(crate) fn icon_for_current(current: &CurrentWeather) -> &'static str {
    let night = current.is_day == Some(false);

    match current.weather_code {
        Some(0) => {
            if night {
                "weather-clear-night"
            } else {
                "weather-clear"
            }
        }
        Some(1 | 2) => {
            if night {
                "weather-partly-cloudy-night"
            } else {
                "weather-partly-cloudy"
            }
        }
        Some(3) => "weather-cloudy",
        Some(45 | 48) => "weather-fog",
        Some(51 | 53 | 55 | 56 | 57) => "weather-drizzle",
        Some(61 | 63 | 65 | 66 | 67 | 80 | 81 | 82) => "weather-rain",
        Some(71 | 73 | 75 | 77 | 85 | 86) => "weather-snow",
        Some(95 | 96 | 99) => "weather-thunderstorm",
        _ => "weather-unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::weather::WeatherLocation;

    fn test_snapshot() -> WeatherSnapshot {
        WeatherSnapshot {
            available: true,
            loading: false,
            stale: false,
            error: None,
            location: Some(WeatherLocation {
                name: "Berlin".to_string(),
                latitude: 52.52,
                longitude: 13.405,
            }),
            current: Some(CurrentWeather {
                temperature: 21.4,
                feels_like: Some(20.6),
                humidity: Some(55),
                wind_speed: Some(13.2),
                condition: "Partly cloudy".to_string(),
                weather_code: Some(2),
                is_day: Some(true),
            }),
            daily: Vec::new(),
            last_update: None,
            units: WeatherUnits::Metric,
            wind_units: WeatherWindUnits::Kmh,
        }
    }

    #[test]
    fn test_weather_config_defaults() {
        let entry = WidgetEntry {
            name: "weather".to_string(),
            options: Default::default(),
        };
        let config = WeatherConfig::from_entry(&entry);

        assert!(config.show_icon);
        assert_eq!(config.format, DEFAULT_FORMAT);
    }

    #[test]
    fn test_weather_config_custom() {
        let mut options = std::collections::HashMap::new();
        options.insert("show_icon".to_string(), toml::Value::Boolean(false));
        options.insert(
            "format".to_string(),
            toml::Value::String("{temperature} {condition}".to_string()),
        );

        let entry = WidgetEntry {
            name: "weather".to_string(),
            options,
        };
        let config = WeatherConfig::from_entry(&entry);

        assert!(!config.show_icon);
        assert_eq!(config.format, "{temperature} {condition}");
    }

    #[test]
    fn test_format_label_replaces_supported_tokens() {
        assert_eq!(
            format_label(
                &test_snapshot(),
                "{temperature} {condition} {feels_like} {humidity} {wind}",
                false,
            ),
            "21°C Partly cloudy 21°C 55% 13 km/h"
        );
    }

    #[test]
    fn test_format_label_compacts_temperature_in_vertical_mode() {
        assert_eq!(format_label(&test_snapshot(), "{temperature}", true), "21");
    }

    #[test]
    fn test_format_label_ignores_user_format_in_vertical_mode() {
        let snapshot = test_snapshot();

        // Wide or compound format strings collapse to just the temperature
        // in vertical mode to prevent them from breaking the vertical bar.
        assert_eq!(
            format_label(&snapshot, "{temperature} {feels_like}", true),
            "21"
        );
        assert_eq!(format_label(&snapshot, "{condition}", true), "21");
        assert_eq!(format_label(&snapshot, "{wind}", true), "21");
        assert_eq!(format_label(&snapshot, "{humidity}", true), "21");
        assert_eq!(
            format_label(
                &snapshot,
                "{temperature} {condition} {feels_like} {humidity} {wind}",
                true
            ),
            "21"
        );
    }

    #[test]
    fn test_format_label_placeholder_when_no_current() {
        let mut snapshot = test_snapshot();
        snapshot.current = None;
        snapshot.loading = true;
        assert_eq!(format_label(&snapshot, DEFAULT_FORMAT, false), "-");

        snapshot.loading = false;
        snapshot.error = Some("Weather location is not configured".to_string());
        assert_eq!(format_label(&snapshot, DEFAULT_FORMAT, false), "-");
    }

    #[test]
    fn test_icon_for_weather_code() {
        let current = test_snapshot().current.unwrap();
        assert_eq!(icon_for_current(&current), "weather-partly-cloudy");
    }

    #[test]
    fn test_icon_for_weather_code_night() {
        let mut current = test_snapshot().current.unwrap();
        current.is_day = Some(false);
        assert_eq!(icon_for_current(&current), "weather-partly-cloudy-night");

        current.weather_code = Some(0);
        assert_eq!(icon_for_current(&current), "weather-clear-night");
    }
}
