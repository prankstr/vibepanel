//! Spacer widget - a flexible or fixed-width empty space.
//!
//! The spacer widget is used to push other widgets apart in a section.
//! It can either expand to fill available space (default) or have a fixed width.
//!
//! # Configuration
//!
//! The spacer supports inline width syntax:
//! - `"spacer"` - expands to fill available space
//! - `"spacer:50"` - fixed 50px width
//!
//! Or via options section:
//! ```toml
//! [widgets.spacer]
//! width = 50
//! ```
//!
//! # Example Usage
//!
//! Push a widget to the right edge of a section:
//! ```toml
//! [widgets]
//! left = ["workspaces", "spacer", "clock"]  # clock ends up at right edge of left section
//! ```
//!
//! Create a gap in the center (e.g., for a display notch):
//! ```toml
//! [widgets]
//! center = ["spacer:200"]  # 200px fixed-width spacer in center
//! ```

use gtk4::prelude::*;
use vibepanel_core::config::WidgetEntry;

use crate::styles::widget as wgt;
use crate::widgets::{WidgetConfig, warn_unknown_options};

/// Configuration for the spacer widget.
///
/// Note: Unlike other widgets, SpacerConfig intentionally omits the `color` field
/// since the spacer is invisible by design and cannot be styled.
#[derive(Debug, Clone, Default)]
pub struct SpacerConfig {
    /// Fixed width in pixels, or None for flexible (expand to fill).
    pub width: Option<u32>,
}

impl WidgetConfig for SpacerConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options("spacer", entry, &["width"]);

        let width = entry
            .options
            .get("width")
            .and_then(|v| v.as_integer())
            .and_then(|n| u32::try_from(n).ok());

        SpacerConfig { width }
    }
}

/// Spacer widget - either expands to fill space or has a fixed width.
///
/// Note: This widget intentionally does not use `BaseWidget` because it has no
/// visible content, styling, tooltips, or click interactions - it's purely a
/// layout primitive.
pub struct SpacerWidget {
    widget: gtk4::Box,
}

impl SpacerWidget {
    /// Create a new spacer widget with the given configuration.
    pub fn new(config: SpacerConfig) -> Self {
        let widget = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        widget.add_css_class(wgt::SPACER);

        match config.width {
            Some(fixed_width) => {
                // Fixed width: set exact size, no expansion
                widget.set_size_request(fixed_width as i32, -1);
                widget.set_hexpand(false);
            }
            None => {
                // Flexible: expand to fill available space
                widget.set_hexpand(true);
                // Minimum width of 0 so it can shrink completely if needed
                widget.set_size_request(0, -1);
            }
        }

        SpacerWidget { widget }
    }

    /// Get the GTK widget.
    pub fn widget(&self) -> &gtk4::Box {
        &self.widget
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_entry(options: HashMap<String, toml::Value>) -> WidgetEntry {
        WidgetEntry {
            name: "spacer".to_string(),
            options,
        }
    }

    #[test]
    fn test_spacer_config_default() {
        let entry = make_entry(HashMap::new());
        let config = SpacerConfig::from_entry(&entry);
        assert_eq!(config.width, None);
    }

    #[test]
    fn test_spacer_config_options() {
        let mut options = HashMap::new();
        options.insert("width".to_string(), toml::Value::Integer(100));
        let entry = make_entry(options);
        let config = SpacerConfig::from_entry(&entry);
        assert_eq!(config.width, Some(100));
    }
}
