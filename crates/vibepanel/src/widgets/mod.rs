//! Widget implementations for the vibepanel bar.
//!
//! Each widget is a self-contained GTK4 component that displays
//! some piece of information (time, battery status, etc.).
//!
//! The `WidgetFactory` constructs widgets from config entries,
//! and `BarState` owns the widget handles to keep them alive.
//!
//! # Widget Configuration
//!
//! Widget configs implement the `WidgetConfig` trait for parsing from TOML.
//! The first CSS class passed to `BaseWidget::new()` determines the widget's
//! identity for per-widget styling (e.g., `[widgets.clock].background_color`).
//! This class is also used to generate popover class names like `clock-popover`.

mod base;
mod battery;
mod battery_popover;
mod calendar_popover;
mod clock;
mod cpu;
mod marquee_label;
mod media;
mod media_components;
mod media_popover;
mod media_window;
mod memory;
mod notifications;
mod notifications_common;
mod notifications_popover;
mod notifications_toast;
mod osd;
mod rounded_picture;
mod spacer;
mod system_popover;
mod tray;
mod updates;
mod updates_common;
mod window_title;
mod workspaces;

pub mod css;

pub mod quick_settings;

pub use base::BaseWidget;
pub use battery::{BatteryConfig, BatteryWidget};
pub use clock::{ClockConfig, ClockWidget};
pub use media::{MediaConfig, MediaWidget};
pub use notifications::{NotificationsConfig, NotificationsWidget};
pub use osd::OsdOverlay;
pub use quick_settings::QuickSettingsWindowHandle;
pub use quick_settings::{QuickSettingsConfig, QuickSettingsWidget};
pub use spacer::{SpacerConfig, SpacerWidget};
pub use tray::{TrayConfig, TrayWidget};
pub use updates::{UpdatesConfig, UpdatesWidget};
pub use window_title::{WindowTitleConfig, WindowTitleWidget};
pub use workspaces::{WorkspacesConfig, WorkspacesWidget};

pub use cpu::{CpuConfig, CpuWidget};
pub use memory::{MemoryConfig, MemoryWidget};

use gtk4::Widget;
use gtk4::prelude::*;
use std::any::Any;
use tracing::{debug, warn};
use vibepanel_core::config::WidgetEntry;

use crate::services::battery::BatteryService;

/// Trait for widget configuration types.
///
/// All widget configs should implement this trait to provide a consistent
/// interface for constructing configuration from TOML entries and defaulting.
///
/// # Example
///
/// ```ignore
/// #[derive(Debug, Clone)]
/// pub struct MyWidgetConfig {
///     pub enabled: bool,
/// }
///
/// impl WidgetConfig for MyWidgetConfig {
///     fn from_entry(entry: &WidgetEntry) -> Self {
///         warn_unknown_options("my_widget", entry, &["enabled"]);
///         let enabled = entry
///             .options
///             .get("enabled")
///             .and_then(|v| v.as_bool())
///             .unwrap_or(true);
///         Self { enabled }
///     }
/// }
///
/// impl Default for MyWidgetConfig {
///     fn default() -> Self {
///         Self { enabled: true }
///     }
/// }
/// ```
pub trait WidgetConfig: Sized + Default {
    /// Create configuration from a widget entry.
    ///
    /// Implementations should extract options from `entry.options` and
    /// fall back to sensible defaults for missing or invalid values.
    fn from_entry(entry: &WidgetEntry) -> Self;
}

/// Log warnings for unknown options in a widget entry.
///
/// Call this at the start of `from_entry()` implementations to warn users
/// about potential typos in their configuration.
///
/// # Example
///
/// ```ignore
/// impl WidgetConfig for MyWidgetConfig {
///     fn from_entry(entry: &WidgetEntry) -> Self {
///         warn_unknown_options("my_widget", entry, &["option_a", "option_b"]);
///         // ... parse options ...
///     }
/// }
/// ```
pub fn warn_unknown_options(widget_name: &str, entry: &WidgetEntry, known_keys: &[&str]) {
    for key in entry.options.keys() {
        if !known_keys.contains(&key.as_str()) {
            warn!(
                "Unknown option '{}' for widget '{}' - possible typo?",
                key, widget_name
            );
        }
    }
}

/// A built widget with its GTK widget and ownership handle.
pub struct BuiltWidget {
    /// The GTK widget to add to the container.
    pub widget: Widget,
    /// Opaque handle to keep the Rust-side state alive (timers, callbacks, etc.).
    pub handle: Box<dyn Any>,
}

/// Factory for constructing widgets from configuration entries.
pub struct WidgetFactory;

impl WidgetFactory {
    /// Build a widget from a config entry.
    ///
    /// Returns `None` if the widget type is not recognized.
    ///
    /// The `output_id` parameter is the monitor connector name (e.g., "eDP-1")
    /// used for per-monitor filtering in widgets like window_title.
    pub fn build(
        entry: &WidgetEntry,
        qs_handle: Option<&QuickSettingsWindowHandle>,
        output_id: Option<&str>,
    ) -> Option<BuiltWidget> {
        match entry.name.as_str() {
            "clock" => {
                let cfg = ClockConfig::from_entry(entry);
                let clock = ClockWidget::new(cfg);
                let root = clock.widget().clone().upcast::<Widget>();
                Some(BuiltWidget {
                    widget: root,
                    handle: Box::new(clock),
                })
            }
            "battery" => {
                if !BatteryService::global().snapshot().available {
                    debug!("Skipping battery widget: no battery available");
                    return None;
                }
                let cfg = BatteryConfig::from_entry(entry);
                let battery = BatteryWidget::new(cfg);
                let root = battery.widget().clone().upcast::<Widget>();
                Some(BuiltWidget {
                    widget: root,
                    handle: Box::new(battery),
                })
            }
            "workspaces" => {
                let cfg = WorkspacesConfig::from_entry(entry);
                let workspaces = WorkspacesWidget::new(cfg, output_id.map(|s| s.to_string()));
                let root = workspaces.widget().clone().upcast::<Widget>();
                Some(BuiltWidget {
                    widget: root,
                    handle: Box::new(workspaces),
                })
            }
            "window_title" => {
                let cfg = WindowTitleConfig::from_entry(entry);
                let window_title = WindowTitleWidget::new(cfg, output_id.map(|s| s.to_string()));
                let root = window_title.widget().clone().upcast::<Widget>();
                Some(BuiltWidget {
                    widget: root,
                    handle: Box::new(window_title),
                })
            }
            "tray" => {
                let cfg = TrayConfig::from_entry(entry);
                let tray = TrayWidget::new(cfg);
                let root = tray.widget().clone().upcast::<Widget>();
                Some(BuiltWidget {
                    widget: root,
                    handle: Box::new(tray),
                })
            }
            "notifications" => {
                let cfg = NotificationsConfig::from_entry(entry);
                let notifications = NotificationsWidget::new(cfg);
                let root = notifications.widget().clone().upcast::<Widget>();
                Some(BuiltWidget {
                    widget: root,
                    handle: Box::new(notifications),
                })
            }
            "quick_settings" => {
                let cfg = QuickSettingsConfig::from_entry(entry);

                let qs_handle = match qs_handle {
                    Some(handle) => handle.clone(),
                    None => {
                        warn!(
                            "quick_settings widget requested but no QuickSettingsWindowHandle was provided; skipping"
                        );
                        return None;
                    }
                };

                let widget = QuickSettingsWidget::new(cfg, qs_handle);
                let root = widget.widget().clone().upcast::<Widget>();
                Some(BuiltWidget {
                    widget: root,
                    handle: Box::new(widget),
                })
            }
            "updates" => {
                let cfg = UpdatesConfig::from_entry(entry);
                let updates = UpdatesWidget::new(cfg);
                let root = updates.widget().clone().upcast::<Widget>();
                Some(BuiltWidget {
                    widget: root,
                    handle: Box::new(updates),
                })
            }
            "cpu" => {
                let cfg = CpuConfig::from_entry(entry);
                let cpu = CpuWidget::new(cfg);
                let root = cpu.widget().clone().upcast::<Widget>();
                Some(BuiltWidget {
                    widget: root,
                    handle: Box::new(cpu),
                })
            }
            "memory" => {
                let cfg = MemoryConfig::from_entry(entry);
                let memory = MemoryWidget::new(cfg);
                let root = memory.widget().clone().upcast::<Widget>();
                Some(BuiltWidget {
                    widget: root,
                    handle: Box::new(memory),
                })
            }
            "media" => {
                let cfg = MediaConfig::from_entry(entry);
                let media = MediaWidget::new(cfg);
                let root = media.widget().clone().upcast::<Widget>();
                Some(BuiltWidget {
                    widget: root,
                    handle: Box::new(media),
                })
            }
            "spacer" => {
                let cfg = SpacerConfig::from_entry(entry);
                let spacer = SpacerWidget::new(cfg);
                let root = spacer.widget().clone().upcast::<Widget>();
                Some(BuiltWidget {
                    widget: root,
                    handle: Box::new(spacer),
                })
            }
            name => {
                warn!("Unknown widget type: '{}', skipping", name);
                None
            }
        }
    }
}

/// Holds widget handles to keep them alive for the lifetime of the bar.
///
/// When widgets are created, their Rust-side state (timers, callbacks, etc.)
/// must be kept alive. This struct owns those handles.
pub struct BarState {
    /// Widget handles that must be kept alive.
    widget_handles: Vec<Box<dyn Any>>,
}

impl BarState {
    /// Create a new empty bar state.
    pub fn new() -> Self {
        Self {
            widget_handles: Vec::new(),
        }
    }

    /// Add a widget handle to be kept alive.
    pub fn add_handle(&mut self, handle: Box<dyn Any>) {
        self.widget_handles.push(handle);
    }

    /// Get the number of widget handles being held.
    pub fn handle_count(&self) -> usize {
        self.widget_handles.len()
    }
}

impl Default for BarState {
    fn default() -> Self {
        Self::new()
    }
}
