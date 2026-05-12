//! Clock widget - displays the current time.
//!
//! Updates on minute boundaries to minimize CPU usage.

use std::cell::RefCell;
use std::rc::Rc;

use chrono::Timelike;
use gtk4::Label;
use gtk4::glib::{self, SourceId};
use gtk4::prelude::*;
use tracing::debug;
use vibepanel_core::config::WidgetEntry;

use crate::services::config_manager::ConfigManager;
use crate::styles::widget as wgt;
use crate::widgets::WidgetConfig;
use crate::widgets::base::BaseWidget;
use crate::widgets::calendar_popover::build_clock_calendar_popover;
use crate::widgets::warn_unknown_options;

/// Default format string for the clock display.
const DEFAULT_FORMAT: &str = "%a %d %H:%M";
/// Default compact format string for side-bar clock display.
const DEFAULT_VERTICAL_FORMAT: &str = "%H\n%M";

/// Configuration for the clock widget.

#[derive(Debug, Clone)]
pub struct ClockConfig {
    /// strftime format string for the clock display.
    pub format: String,
    /// Optional strftime format string for side-bar clock display.
    pub format_vertical: Option<String>,
    /// Whether to show week numbers in the calendar popover.
    pub show_week_numbers: bool,
}

impl WidgetConfig for ClockConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options(
            "clock",
            entry,
            &["format", "format_vertical", "show_week_numbers"],
        );

        let format = entry
            .options
            .get("format")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_FORMAT)
            .to_string();

        let format_vertical = entry
            .options
            .get("format_vertical")
            .and_then(|v| v.as_str())
            .map(String::from);

        let show_week_numbers = entry
            .options
            .get("show_week_numbers")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        Self {
            format,
            format_vertical,
            show_week_numbers,
        }
    }
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self {
            format: DEFAULT_FORMAT.to_string(),
            format_vertical: None,
            show_week_numbers: true,
        }
    }
}

/// Clock widget that displays and updates the current time.
pub struct ClockWidget {
    /// Shared base widget container.
    base: BaseWidget,
    /// The label displaying the time.
    label: Label,
    /// The format string for strftime.
    format: String,
    /// Optional side-bar format string for strftime.
    format_vertical: Option<String>,
    /// Whether the bar is on a side edge.
    is_vertical: bool,
    /// Active timer source ID for cancellation on drop.
    /// The Rc<RefCell<>> allows the closure to update the ID when
    /// it transitions from the one-shot to the repeating timer.
    timer_source: Rc<RefCell<Option<SourceId>>>,
}

impl ClockWidget {
    /// Create a new clock widget with the given configuration.
    pub fn new(config: ClockConfig) -> Self {
        let base = BaseWidget::new(&[wgt::CLOCK]);
        let is_vertical = ConfigManager::global().bar_position().is_vertical();

        let label = base.add_label(Some("--:--"), &[wgt::CLOCK_LABEL]);
        if is_vertical {
            label.set_wrap(true);
            label.set_single_line_mode(false);
            label.set_justify(gtk4::Justification::Center);
            label.set_yalign(0.5);
        }

        let show_week_numbers = config.show_week_numbers;

        // Shared slot for the calendar refresh callback. Populated by the
        // builder on first open, invoked by on_show on every subsequent open.
        type RefreshSlot = Rc<RefCell<Option<Rc<dyn Fn()>>>>;
        let refresh_slot: RefreshSlot = Rc::new(RefCell::new(None));

        let refresh_for_builder = refresh_slot.clone();
        let menu_handle = base.create_menu(move || {
            let (widget, refresh) = build_clock_calendar_popover(show_week_numbers);
            *refresh_for_builder.borrow_mut() = Some(refresh);
            widget
        });

        // Reuse the calendar widget across open/close cycles to avoid
        // unbounded memory growth from GTK4.
        menu_handle.set_reuse_content(true);
        menu_handle.set_on_show(move || {
            if let Some(ref cb) = *refresh_slot.borrow() {
                cb();
            }
        });

        let timer_source = Rc::new(RefCell::new(None));

        let widget = Self {
            base,
            label,
            format: config.format,
            format_vertical: config.format_vertical,
            is_vertical,
            timer_source,
        };

        widget.update_time();
        widget.schedule_minute_tick();

        widget
    }

    /// Get the root GTK widget for embedding in the bar.
    pub fn widget(&self) -> &gtk4::Box {
        self.base.widget()
    }

    /// Update the displayed time.
    fn update_time(&self) {
        let now = chrono::Local::now();
        update_label(
            &self.label,
            &self.format,
            self.format_vertical.as_deref(),
            self.is_vertical,
            now,
        );
        let text = formatted_clock_text(
            &self.format,
            self.format_vertical.as_deref(),
            self.is_vertical,
            now,
        );
        debug!("Clock updated: {}", text.replace('\n', " "));
    }

    fn schedule_minute_tick(&self) {
        let now = chrono::Local::now();
        let delay_seconds = 60 - now.second();

        let label = self.label.clone();
        let format = self.format.clone();
        let format_vertical = self.format_vertical.clone();
        let is_vertical = self.is_vertical;
        let timer_source = Rc::clone(&self.timer_source);

        let source_id = glib::timeout_add_seconds_local_once(delay_seconds, move || {
            let now = chrono::Local::now();
            update_label(
                &label,
                &format,
                format_vertical.as_deref(),
                is_vertical,
                now,
            );

            let label_clone = label.clone();
            let format_clone = format.clone();
            let format_vertical_clone = format_vertical.clone();
            let timer_source_clone = Rc::clone(&timer_source);
            let repeating_id = glib::timeout_add_seconds_local(60, move || {
                let now = chrono::Local::now();
                update_label(
                    &label_clone,
                    &format_clone,
                    format_vertical_clone.as_deref(),
                    is_vertical,
                    now,
                );
                glib::ControlFlow::Continue
            });

            *timer_source_clone.borrow_mut() = Some(repeating_id);
        });

        *self.timer_source.borrow_mut() = Some(source_id);

        debug!("Clock tick scheduled in {} seconds", delay_seconds);
    }
}

fn formatted_clock_text(
    format: &str,
    format_vertical: Option<&str>,
    is_vertical: bool,
    now: chrono::DateTime<chrono::Local>,
) -> String {
    if is_vertical {
        now.format(format_vertical.unwrap_or(DEFAULT_VERTICAL_FORMAT))
            .to_string()
    } else {
        now.format(format).to_string()
    }
}

fn update_label(
    label: &Label,
    format: &str,
    format_vertical: Option<&str>,
    is_vertical: bool,
    now: chrono::DateTime<chrono::Local>,
) {
    let text = formatted_clock_text(format, format_vertical, is_vertical, now);
    label.set_label(&text);
    if is_vertical {
        label.set_tooltip_text(Some(&now.format(format).to_string()));
    }
}

impl Drop for ClockWidget {
    fn drop(&mut self) {
        // Cancel any active timer to prevent callbacks after widget is dropped
        if let Some(source_id) = self.timer_source.borrow_mut().take() {
            source_id.remove();
            debug!("Clock timer cancelled on drop");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use toml::Value;

    fn make_widget_entry(name: &str, options: HashMap<String, Value>) -> WidgetEntry {
        WidgetEntry {
            name: name.to_string(),
            options,
        }
    }

    #[test]
    fn test_clock_config_default_format() {
        let entry = make_widget_entry("clock", HashMap::new());
        let config = ClockConfig::from_entry(&entry);
        assert_eq!(config.format, "%a %d %H:%M");
        assert_eq!(config.format_vertical, None);
    }

    #[test]
    fn test_clock_config_custom_format() {
        let mut options = HashMap::new();
        options.insert("format".to_string(), Value::String("%H:%M".to_string()));
        let entry = make_widget_entry("clock", options);
        let config = ClockConfig::from_entry(&entry);
        assert_eq!(config.format, "%H:%M");
    }

    #[test]
    fn test_clock_config_custom_vertical_format() {
        let mut options = HashMap::new();
        options.insert(
            "format_vertical".to_string(),
            Value::String("%H\n%M".to_string()),
        );
        let entry = make_widget_entry("clock", options);
        let config = ClockConfig::from_entry(&entry);
        assert_eq!(config.format, "%a %d %H:%M");
        assert_eq!(config.format_vertical.as_deref(), Some("%H\n%M"));
    }

    #[test]
    fn test_clock_config_ignores_non_string_format() {
        let mut options = HashMap::new();
        options.insert("format".to_string(), Value::Integer(123));
        let entry = make_widget_entry("clock", options);
        let config = ClockConfig::from_entry(&entry);
        // Falls back to default when format is not a string
        assert_eq!(config.format, "%a %d %H:%M");
    }

    #[test]
    fn test_clock_config_ignores_non_string_vertical_format() {
        let mut options = HashMap::new();
        options.insert("format_vertical".to_string(), Value::Integer(123));
        let entry = make_widget_entry("clock", options);
        let config = ClockConfig::from_entry(&entry);
        assert_eq!(config.format_vertical, None);
    }

    #[test]
    fn test_clock_config_default_impl() {
        let config = ClockConfig::default();
        assert_eq!(config.format, "%a %d %H:%M");
        assert_eq!(config.format_vertical, None);
    }

    #[test]
    fn test_formatted_clock_text_vertical_uses_compact_time() {
        let now = chrono::Local::now();
        assert_eq!(
            formatted_clock_text("%a %d %H:%M", None, true, now),
            now.format("%H\n%M").to_string()
        );
    }

    #[test]
    fn test_formatted_clock_text_vertical_uses_vertical_format_when_set() {
        let now = chrono::Local::now();
        assert_eq!(
            formatted_clock_text("%a %d %H:%M", Some("%a\n%H:%M"), true, now),
            now.format("%a\n%H:%M").to_string()
        );
    }

    #[test]
    fn test_formatted_clock_text_horizontal_uses_config_format() {
        let now = chrono::Local::now();
        assert_eq!(
            formatted_clock_text("%a %d %H:%M", Some("%H\n%M"), false, now),
            now.format("%a %d %H:%M").to_string()
        );
    }
}
