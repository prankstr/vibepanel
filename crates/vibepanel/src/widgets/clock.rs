//! Clock widget - displays the current time.
//!
//! Uses second-resolution ticks only for formats that display seconds, and
//! otherwise updates on minute boundaries to minimize wakeups.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use chrono::Timelike;
use chrono::format::{Fixed, Item, Numeric, StrftimeItems};
use gtk4::Label;
use gtk4::glib::{self, SourceId};
use gtk4::prelude::*;
use tracing::{debug, trace, warn};
use vibepanel_core::config::WidgetEntry;

use crate::services::callbacks::CallbackId;
use crate::services::config_manager::ConfigManager;
use crate::services::sleep_watcher::SleepWatcher;
use crate::styles::widget as wgt;
use crate::widgets::WidgetConfig;
use crate::widgets::base::BaseWidget;
use crate::widgets::calendar_popover::build_clock_calendar_popover;
use crate::widgets::warn_unknown_options;

/// Default format string for the clock display.
const DEFAULT_FORMAT: &str = "%a %d %H:%M";
/// Default compact format string for side-bar clock display.
const DEFAULT_VERTICAL_FORMAT: &str = "%H\n%M";
const CLOCK_BOUNDARY_BUFFER_MS: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchedulingMode {
    /// Tick once per second for formats containing sub-minute specifiers.
    /// Fractional seconds refresh at this cadence, not subsecond cadence.
    Second,
    MinuteBoundary,
}

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
    scheduling_mode: SchedulingMode,
    /// Active timer source ID for cancellation on drop.
    /// The Rc<RefCell<>> lets self-rescheduling callbacks replace the ID.
    timer_source: Rc<RefCell<Option<SourceId>>>,
    resume_callback_id: Option<CallbackId>,
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

        let format = validated_clock_format(config.format, DEFAULT_FORMAT, "format");
        let format_vertical = config.format_vertical.map(|format| {
            validated_clock_format(format, DEFAULT_VERTICAL_FORMAT, "format_vertical")
        });
        let scheduling_mode =
            clock_scheduling_mode(&format, format_vertical.as_deref(), is_vertical);
        let timer_source = Rc::new(RefCell::new(None));

        let resume_callback_id = {
            let label = label.clone();
            let format = format.clone();
            let format_vertical = format_vertical.clone();
            let timer_source = Rc::clone(&timer_source);
            Some(SleepWatcher::global().on_resume(move || {
                reset_clock_timer(
                    &label,
                    &format,
                    format_vertical.as_deref(),
                    is_vertical,
                    scheduling_mode,
                    &timer_source,
                );
            }))
        };

        let widget = Self {
            base,
            label,
            format,
            format_vertical,
            is_vertical,
            scheduling_mode,
            timer_source,
            resume_callback_id,
        };

        widget.update_time();
        widget.schedule_tick();

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
        trace!("Clock updated: {}", text.replace('\n', " "));
    }

    fn schedule_tick(&self) {
        schedule_clock_tick(
            self.label.clone(),
            self.format.clone(),
            self.format_vertical.clone(),
            self.is_vertical,
            self.scheduling_mode,
            Rc::clone(&self.timer_source),
        );
    }
}

fn clock_scheduling_mode(
    format: &str,
    format_vertical: Option<&str>,
    is_vertical: bool,
) -> SchedulingMode {
    let active_format = if is_vertical {
        format_vertical.unwrap_or(DEFAULT_VERTICAL_FORMAT)
    } else {
        format
    };

    if clock_format_has_seconds(active_format) || (is_vertical && clock_format_has_seconds(format))
    {
        SchedulingMode::Second
    } else {
        SchedulingMode::MinuteBoundary
    }
}

fn validated_clock_format(format: String, fallback: &str, field_name: &str) -> String {
    if analyze_clock_format(&format).is_some() {
        return format;
    }

    warn!("Invalid clock {field_name} {format:?}; falling back to {fallback:?}");
    fallback.to_string()
}

fn schedule_clock_tick(
    label: Label,
    format: String,
    format_vertical: Option<String>,
    is_vertical: bool,
    mode: SchedulingMode,
    timer_source: Rc<RefCell<Option<SourceId>>>,
) {
    let delay = next_tick_delay(mode);
    let timer_source_for_callback = Rc::clone(&timer_source);

    let source_id = glib::timeout_add_local_once(delay, move || {
        let now = chrono::Local::now();
        update_label(
            &label,
            &format,
            format_vertical.as_deref(),
            is_vertical,
            now,
        );
        schedule_clock_tick(
            label,
            format,
            format_vertical,
            is_vertical,
            mode,
            timer_source_for_callback,
        );
    });

    *timer_source.borrow_mut() = Some(source_id);
    trace!("Clock tick scheduled in {:?}", delay);
}

fn next_tick_delay(mode: SchedulingMode) -> Duration {
    let now = chrono::Local::now();
    let millis_into_second = u64::from(now.nanosecond() / 1_000_000);

    let boundary_delay_ms = match mode {
        SchedulingMode::Second => 1_000 - millis_into_second,
        SchedulingMode::MinuteBoundary => {
            let millis_into_minute = u64::from(now.second()) * 1_000 + millis_into_second;
            60_000 - millis_into_minute
        }
    };

    Duration::from_millis(boundary_delay_ms + CLOCK_BOUNDARY_BUFFER_MS)
}

fn cancel_clock_timer(timer_source: &Rc<RefCell<Option<SourceId>>>) {
    if let Some(source_id) = timer_source.borrow_mut().take() {
        source_id.remove();
    }
}

fn reset_clock_timer(
    label: &Label,
    format: &str,
    format_vertical: Option<&str>,
    is_vertical: bool,
    mode: SchedulingMode,
    timer_source: &Rc<RefCell<Option<SourceId>>>,
) {
    cancel_clock_timer(timer_source);
    let now = chrono::Local::now();
    update_label(label, format, format_vertical, is_vertical, now);
    schedule_clock_tick(
        label.clone(),
        format.to_string(),
        format_vertical.map(String::from),
        is_vertical,
        mode,
        Rc::clone(timer_source),
    );
}

fn analyze_clock_format(format: &str) -> Option<SchedulingMode> {
    let mut has_seconds = false;

    for item in StrftimeItems::new(format) {
        match item {
            Item::Error => return None,
            Item::Numeric(Numeric::Second | Numeric::Nanosecond | Numeric::Timestamp, _)
            | Item::Fixed(
                Fixed::Nanosecond
                | Fixed::Nanosecond3
                | Fixed::Nanosecond6
                | Fixed::Nanosecond9
                | Fixed::RFC2822
                | Fixed::RFC3339
                | Fixed::Internal(_),
            ) => has_seconds = true,
            _ => {}
        }
    }

    Some(if has_seconds {
        SchedulingMode::Second
    } else {
        SchedulingMode::MinuteBoundary
    })
}

fn clock_format_has_seconds(format: &str) -> bool {
    analyze_clock_format(format) == Some(SchedulingMode::Second)
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
        cancel_clock_timer(&self.timer_source);
        if let Some(id) = self.resume_callback_id.take() {
            SleepWatcher::global().disconnect(id);
        }
        debug!("Clock timer cancelled on drop");
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

    #[test]
    fn test_analyze_clock_format_detects_second_resolution_formats() {
        for format in ["%H:%M:%S", "%T", "%r", "%X", "%c", "%+", "%s"] {
            assert_eq!(
                analyze_clock_format(format),
                Some(SchedulingMode::Second),
                "{format} should need seconds"
            );
        }
    }

    #[test]
    fn test_analyze_clock_format_detects_fractional_seconds() {
        for format in ["%f", "%.f", "%.3f", "%3f", "%H:%M:%.6f"] {
            assert_eq!(
                analyze_clock_format(format),
                Some(SchedulingMode::Second),
                "{format} should need seconds"
            );
        }
    }

    #[test]
    fn test_analyze_clock_format_detects_valid_padded_seconds() {
        for format in ["%-S", "%_S", "%0S"] {
            assert_eq!(
                analyze_clock_format(format),
                Some(SchedulingMode::Second),
                "{format} should need seconds"
            );
        }
    }

    #[test]
    fn test_analyze_clock_format_rejects_invalid_chrono_formats() {
        for format in ["%3S", "%OS", "%ES", "%^S", "%#S"] {
            assert_eq!(
                analyze_clock_format(format),
                None,
                "{format} should be rejected"
            );
        }
    }

    #[test]
    fn test_analyze_clock_format_uses_minute_boundary_for_minute_formats() {
        for format in ["%H:%M", "%a %d %H:%M", "%Y", "literal S"] {
            assert_eq!(
                analyze_clock_format(format),
                Some(SchedulingMode::MinuteBoundary),
                "{format} should not need seconds"
            );
        }
    }

    #[test]
    fn test_validated_clock_format_falls_back_for_invalid_format() {
        assert_eq!(
            validated_clock_format("%OS".to_string(), DEFAULT_FORMAT, "format"),
            DEFAULT_FORMAT
        );
        assert_eq!(
            validated_clock_format("%H:%M:%S".to_string(), DEFAULT_FORMAT, "format"),
            "%H:%M:%S"
        );
    }

    #[test]
    fn test_clock_scheduling_mode_considers_rendered_formats() {
        assert_eq!(
            clock_scheduling_mode("%H:%M:%S", Some("%H\n%M"), true),
            SchedulingMode::Second
        );
        assert_eq!(
            clock_scheduling_mode("%H:%M", Some("%H\n%M\n%S"), true),
            SchedulingMode::Second
        );
        assert_eq!(
            clock_scheduling_mode("%H:%M:%S", Some("%H\n%M"), false),
            SchedulingMode::Second
        );
    }
}
