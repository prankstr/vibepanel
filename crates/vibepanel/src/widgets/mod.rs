//! Widget implementations for the vibepanel bar.
//!
//! Each widget is a self-contained GTK4 component that displays
//! some piece of information (time, battery status, etc.).
//!
//! The `WidgetFactory` is used to construct widgets from config entries,
//! and `BarState` owns the widget handles to keep them alive.
//!
//! # Widget Configuration Pattern
//!
//! All widget configurations implement the `WidgetConfig` trait, which provides
//! a standard interface for parsing configuration from TOML entries:
//!
//! ```ignore
//! pub struct MyWidgetConfig {
//!     pub some_option: bool,
//!     /// Custom background color for this widget (inherited from WidgetEntry).
//!     pub color: Option<String>,
//! }
//!
//! impl WidgetConfig for MyWidgetConfig {
//!     fn from_entry(entry: &WidgetEntry) -> Self {
//!         warn_unknown_options("my_widget", entry, &["some_option"]);
//!         let some_option = entry
//!             .options
//!             .get("some_option")
//!             .and_then(|v| v.as_bool())
//!             .unwrap_or(true);
//!         Self {
//!             some_option,
//!             color: entry.color.clone(),  // Always clone color from entry
//!         }
//!     }
//! }
//!
//! impl Default for MyWidgetConfig {
//!     fn default() -> Self {
//!         Self {
//!             some_option: true,
//!             color: None,
//!         }
//!     }
//! }
//! ```
//!
//! When constructing the widget, pass the color to `BaseWidget::new()`:
//!
//! ```ignore
//! impl MyWidget {
//!     pub fn new(config: MyWidgetConfig) -> Self {
//!         let base = BaseWidget::new(&[widget::MY_WIDGET], config.color.clone());
//!         // ... rest of widget construction
//!     }
//! }
//! ```

mod base;
mod battery;
mod battery_popover;
mod calendar_popover;
mod clock;
mod cpu;
mod memory;
mod notification;
mod notification_common;
mod notification_popover;
mod notification_toast;
mod osd;
mod spacer;
mod system_popover;
mod system_tray;
mod updates;
mod updates_common;
mod window_title;
mod workspace;

pub mod css;

pub mod quick_settings;

pub use base::BaseWidget;
pub use base::apply_widget_color;
pub use battery::{BatteryConfig, BatteryWidget};
pub use clock::{ClockConfig, ClockWidget};
pub use notification::{NotificationConfig, NotificationWidget};
pub use osd::OsdOverlay;
pub use quick_settings::QuickSettingsWindowHandle;
pub use quick_settings::{QuickSettingsConfig, QuickSettingsWidget};
pub use spacer::{SpacerConfig, SpacerWidget};
pub use system_tray::{SystemTrayConfig, SystemTrayWidget};
pub use updates::{UpdatesConfig, UpdatesWidget};
pub use window_title::{WindowTitleConfig, WindowTitleWidget};
pub use workspace::{WorkspaceConfig, WorkspaceWidget};

pub use cpu::{CpuConfig, CpuWidget};
pub use memory::{MemoryConfig, MemoryWidget};

use gtk4::Widget;
use gtk4::prelude::*;
use std::any::Any;
use tracing::warn;
use vibepanel_core::config::WidgetEntry;

/// Trait for widget configuration types.
///
/// All widget configs should implement this trait to provide a consistent
/// interface for constructing configuration from TOML entries and defaulting.
///
/// # Color Field
///
/// All widget configs should include a `color: Option<String>` field and copy it
/// from `entry.color.clone()` in `from_entry()`. This enables per-widget background
/// color customization. The color should be passed to `BaseWidget::new()` during
/// widget construction so it applies to both the widget and its popovers.
///
/// # Example
///
/// ```ignore
/// #[derive(Debug, Clone)]
/// pub struct MyWidgetConfig {
///     pub enabled: bool,
///     pub color: Option<String>,
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
///         Self {
///             enabled,
///             color: entry.color.clone(),
///         }
///     }
/// }
///
/// impl Default for MyWidgetConfig {
///     fn default() -> Self {
///         Self {
///             enabled: true,
///             color: None,
///         }
///     }
/// }
/// ```
pub trait WidgetConfig: Sized + Default {
    /// Create configuration from a widget entry.
    ///
    /// Implementations should extract options from `entry.options` and
    /// fall back to sensible defaults for missing or invalid values.
    /// Always include `color: entry.color.clone()` to support per-widget colors.
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
                let cfg = BatteryConfig::from_entry(entry);
                let battery = BatteryWidget::new(cfg);
                let root = battery.widget().clone().upcast::<Widget>();
                Some(BuiltWidget {
                    widget: root,
                    handle: Box::new(battery),
                })
            }
            "workspace" => {
                let cfg = WorkspaceConfig::from_entry(entry);
                let workspace = WorkspaceWidget::new(cfg, output_id.map(|s| s.to_string()));
                let root = workspace.widget().clone().upcast::<Widget>();
                Some(BuiltWidget {
                    widget: root,
                    handle: Box::new(workspace),
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
            "system_tray" => {
                let cfg = SystemTrayConfig::from_entry(entry);
                let system_tray = SystemTrayWidget::new(cfg);
                let root = system_tray.widget().clone().upcast::<Widget>();
                Some(BuiltWidget {
                    widget: root,
                    handle: Box::new(system_tray),
                })
            }
            "notifications" => {
                let cfg = NotificationConfig::from_entry(entry);
                let notification = NotificationWidget::new(cfg);
                let root = notification.widget().clone().upcast::<Widget>();
                Some(BuiltWidget {
                    widget: root,
                    handle: Box::new(notification),
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
