//! Clock widget - displays the current time.
//!
//! Updates on minute boundaries to minimize CPU usage.

use std::cell::RefCell;
use std::rc::Rc;

use chrono::Timelike;
use gtk4::Label;
use gtk4::glib::{self, SourceId};
use tracing::debug;
use vibepanel_core::config::WidgetEntry;

use crate::styles::widget as wgt;
use crate::widgets::WidgetConfig;
use crate::widgets::base::BaseWidget;
use crate::widgets::calendar_popover::build_clock_calendar_popover;
use crate::widgets::warn_unknown_options;

/// Default format string for the clock display.
const DEFAULT_FORMAT: &str = "%a %d %H:%M";

/// Configuration for the clock widget.

#[derive(Debug, Clone)]
pub struct ClockConfig {
    /// strftime format string for the clock display.
    pub format: String,
    /// Whether to show week numbers in the calendar popover.
    pub show_week_numbers: bool,
}

impl WidgetConfig for ClockConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options("clock", entry, &["format", "show_week_numbers"]);

        let format = entry
            .options
            .get("format")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_FORMAT)
            .to_string();

        let show_week_numbers = entry
            .options
            .get("show_week_numbers")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        Self {
            format,
            show_week_numbers,
        }
    }
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self {
            format: DEFAULT_FORMAT.to_string(),
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
    /// Active timer source ID for cancellation on drop.
    /// The Rc<RefCell<>> allows the closure to update the ID when
    /// it transitions from the one-shot to the repeating timer.
    timer_source: Rc<RefCell<Option<SourceId>>>,
}

impl ClockWidget {
    /// Create a new clock widget with the given configuration.
    pub fn new(config: ClockConfig) -> Self {
        let base = BaseWidget::new(&[wgt::CLOCK]);

        let label = base.add_label(Some("--:--"), &[wgt::CLOCK_LABEL]);

        let show_week_numbers = config.show_week_numbers;
        base.create_menu(move || build_clock_calendar_popover(show_week_numbers));

        let timer_source = Rc::new(RefCell::new(None));

        let widget = Self {
            base,
            label,
            format: config.format,
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
        let text = now.format(&self.format).to_string();
        self.label.set_label(&text);
        debug!("Clock updated: {}", text);
    }

    /// Schedule the next tick on the next minute boundary.
    fn schedule_minute_tick(&self) {
        let now = chrono::Local::now();
        let delay_seconds = 60 - now.second();

        let label = self.label.clone();
        let format = self.format.clone();
        let timer_source = Rc::clone(&self.timer_source);

        let source_id = glib::timeout_add_seconds_local_once(delay_seconds, move || {
            let now = chrono::Local::now();
            let text = now.format(&format).to_string();
            label.set_label(&text);

            let label_clone = label.clone();
            let format_clone = format.clone();
            let timer_source_clone = Rc::clone(&timer_source);
            let repeating_id = glib::timeout_add_seconds_local(60, move || {
                let now = chrono::Local::now();
                let text = now.format(&format_clone).to_string();
                label_clone.set_label(&text);
                glib::ControlFlow::Continue
            });

            *timer_source_clone.borrow_mut() = Some(repeating_id);
        });

        *self.timer_source.borrow_mut() = Some(source_id);

        debug!("Clock tick scheduled in {} seconds", delay_seconds);
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
    fn test_clock_config_ignores_non_string_format() {
        let mut options = HashMap::new();
        options.insert("format".to_string(), Value::Integer(123));
        let entry = make_widget_entry("clock", options);
        let config = ClockConfig::from_entry(&entry);
        // Falls back to default when format is not a string
        assert_eq!(config.format, "%a %d %H:%M");
    }

    #[test]
    fn test_clock_config_default_impl() {
        let config = ClockConfig::default();
        assert_eq!(config.format, "%a %d %H:%M");
    }
}
