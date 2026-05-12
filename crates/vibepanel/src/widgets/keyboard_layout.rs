//! Keyboard Layout widget — displays the active keyboard layout.
//!
//! Shows a short code or full name with optional icon. Click to cycle layouts.

use gtk4::prelude::*;
use gtk4::{GestureClick, Label};
use tracing::debug;
use vibepanel_core::config::WidgetEntry;

use crate::services::callbacks::CallbackId;
use crate::services::compositor::CompositorManager;
use crate::services::compositor::KeyboardLayoutInfo;
use crate::services::compositor::xkb_names;
use crate::services::config_manager::ConfigManager;
use crate::services::tooltip::TooltipManager;
use crate::styles::{class, state, widget};
use crate::widgets::base::BaseWidget;
use crate::widgets::{WidgetConfig, warn_unknown_options};

/// Default configuration values.
const DEFAULT_SHOW_ICON: bool = true;
const DEFAULT_SHOW_LABEL: bool = true;

/// Layout label format options.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum LayoutFormat {
    /// Short code extracted from the layout name: "English (US)" -> "US"
    #[default]
    Short,
    /// Full layout name as reported by the compositor.
    Long,
}

impl LayoutFormat {
    fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "long" => Self::Long,
            _ => Self::Short,
        }
    }
}

/// Configuration for the keyboard layout widget.
#[derive(Debug, Clone)]
pub struct KeyboardLayoutConfig {
    pub show_icon: bool,
    pub show_label: bool,
    pub format: LayoutFormat,
}

impl WidgetConfig for KeyboardLayoutConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options(
            "keyboard_layout",
            entry,
            &["show_icon", "show_label", "format"],
        );

        let show_icon = entry
            .options
            .get("show_icon")
            .and_then(|v| v.as_bool())
            .unwrap_or(DEFAULT_SHOW_ICON);

        let show_label = entry
            .options
            .get("show_label")
            .and_then(|v| v.as_bool())
            .unwrap_or(DEFAULT_SHOW_LABEL);

        let format = entry
            .options
            .get("format")
            .and_then(|v| v.as_str())
            .map(LayoutFormat::parse)
            .unwrap_or_default();

        Self {
            show_icon,
            show_label,
            format,
        }
    }
}

impl Default for KeyboardLayoutConfig {
    fn default() -> Self {
        Self {
            show_icon: DEFAULT_SHOW_ICON,
            show_label: DEFAULT_SHOW_LABEL,
            format: LayoutFormat::default(),
        }
    }
}

/// Keyboard layout widget that displays the active layout and supports click-to-cycle.
pub struct KeyboardLayoutWidget {
    base: BaseWidget,
    callback_id: CallbackId,
}

impl KeyboardLayoutWidget {
    /// Create a new keyboard layout widget with the given configuration.
    pub fn new(config: KeyboardLayoutConfig) -> Self {
        let base = BaseWidget::new(&[widget::KEYBOARD_LAYOUT]);

        base.set_tooltip("Keyboard layout");

        let icon_handle = base.add_icon("input-keyboard", &[widget::KEYBOARD_LAYOUT_ICON]);
        let label = base.add_label(None, &[widget::KEYBOARD_LAYOUT_LABEL, class::VCENTER_CAPS]);

        icon_handle.widget().set_visible(config.show_icon);
        label.set_visible(config.show_label);

        // Click to cycle layouts
        {
            let click = GestureClick::new();
            let container = base.widget().clone();
            click.connect_released(move |_, _, _, _| {
                debug!("Keyboard layout widget clicked, cycling to next layout");
                CompositorManager::global().switch_keyboard_layout_next();
            });
            container.add_controller(click);
        }

        // Subscribe to keyboard layout changes
        let manager = CompositorManager::global();
        let callback_id = {
            let container = base.widget().clone();
            let label = label.clone();
            let format = config.format.clone();
            let is_vertical = ConfigManager::global().bar_position().is_vertical();

            manager.register_keyboard_layout_callback(move |info: &KeyboardLayoutInfo| {
                update_keyboard_layout_widget(&container, &label, info, &format, is_vertical);
            })
        };

        Self { base, callback_id }
    }

    /// Get the root GTK widget for embedding in the bar.
    pub fn widget(&self) -> &gtk4::Box {
        self.base.widget()
    }
}

impl Drop for KeyboardLayoutWidget {
    fn drop(&mut self) {
        CompositorManager::global().unregister_keyboard_layout_callback(self.callback_id);
    }
}

/// Extract a short display code from a full layout name.
///
/// Strategies: parenthesized code (`"English (US)"` → `"US"`), short string
/// uppercasing (`"us"` → `"US"`), xkb_names lookup (`"Swedish"` → `"SE"`),
/// or fall back to the full name.
fn extract_short_name(layout_name: &str) -> String {
    if layout_name.is_empty() {
        return "?".to_string();
    }

    // If there's a parenthesized part, extract it: "English (US)" -> "US"
    if let Some(start) = layout_name.rfind('(')
        && let Some(rel_end) = layout_name[start + 1..].find(')')
    {
        let inner = layout_name[start + 1..start + 1 + rel_end].trim();
        if !inner.is_empty() {
            // Spaces in the paren suggest a variant description ("no dead keys")
            // rather than a short code ("US") — fall through to use the base name.
            if !inner.contains(' ') {
                return inner.to_string();
            }

            let base = layout_name[..start].trim();
            if !base.is_empty() {
                // Try the XKB names table for the base language name
                if let Some(code) = xkb_names::short_code_from_language(base) {
                    return code.to_string();
                }
                if base.len() <= 3 && base.chars().all(|c| c.is_ascii_alphabetic()) {
                    return base.to_uppercase();
                }
                return base.to_string();
            }
        }
    }

    // Short strings (2-3 chars) are likely already codes — uppercase them
    if layout_name.len() <= 3 && layout_name.chars().all(|c| c.is_ascii_alphabetic()) {
        return layout_name.to_uppercase();
    }

    // Try the XKB names table for bare language names like "Swedish", "German"
    if let Some(code) = xkb_names::short_code_from_language(layout_name) {
        return code.to_string();
    }

    // Fall back to full name
    layout_name.to_string()
}

fn compact_layout_label(label: &str) -> String {
    label.chars().take(2).collect()
}

/// Update the keyboard layout widget from a layout info update.
fn update_keyboard_layout_widget(
    container: &gtk4::Box,
    label: &Label,
    info: &KeyboardLayoutInfo,
    format: &LayoutFormat,
    is_vertical: bool,
) {
    // Toggle clickable styling based on whether layout cycling is possible
    let is_cyclable = info.layout_count != Some(1);
    if is_cyclable {
        container.add_css_class(state::CLICKABLE);
    } else {
        container.remove_css_class(state::CLICKABLE);
    }

    let display_text = match format {
        LayoutFormat::Long => {
            if info.layout_name.is_empty() {
                "?".to_string()
            } else {
                info.layout_name.clone()
            }
        }
        LayoutFormat::Short => {
            if !info.short_name.is_empty() {
                // Backend provided an XKB code (e.g. "swe") — look up a
                // normalized display code, fall back to uppercasing.
                xkb_names::short_code_from_xkb(&info.short_name)
                    .map(String::from)
                    .unwrap_or_else(|| info.short_name.to_uppercase())
            } else {
                extract_short_name(&info.layout_name)
            }
        }
    };

    let label_text = if is_vertical {
        compact_layout_label(&display_text)
    } else {
        display_text
    };

    label.set_label(&label_text);

    // Set tooltip to full layout name
    let tooltip = if info.layout_name.is_empty() {
        "Keyboard layout: unknown".to_string()
    } else {
        format!("Keyboard layout: {}", info.layout_name)
    };
    let tooltip_manager = TooltipManager::global();
    tooltip_manager.set_styled_tooltip(container, &tooltip);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_short_name_parenthesized() {
        assert_eq!(extract_short_name("English (US)"), "US");
        assert_eq!(extract_short_name("German (Dvorak)"), "Dvorak");
        assert_eq!(extract_short_name("Russian (phonetic)"), "phonetic");
    }

    #[test]
    fn test_extract_short_name_short_codes() {
        assert_eq!(extract_short_name("us"), "US");
        assert_eq!(extract_short_name("de"), "DE");
        assert_eq!(extract_short_name("fr"), "FR");
    }

    #[test]
    fn test_extract_short_name_full_name() {
        // Known language names now resolve to 2-letter codes via xkb_names
        assert_eq!(extract_short_name("French"), "FR");
        assert_eq!(extract_short_name("Japanese"), "JP");
        assert_eq!(extract_short_name("Swedish"), "SE");
        assert_eq!(extract_short_name("German"), "DE");
        // Unknown names pass through unchanged
        assert_eq!(extract_short_name("Klingon"), "Klingon");
    }

    #[test]
    fn test_extract_short_name_empty() {
        assert_eq!(extract_short_name(""), "?");
    }

    #[test]
    fn test_compact_layout_label_takes_first_two_chars() {
        assert_eq!(compact_layout_label("US"), "US");
        assert_eq!(compact_layout_label("Dvorak"), "Dv");
        assert_eq!(compact_layout_label("?"), "?");
    }

    #[test]
    fn test_extract_short_name_edge_cases() {
        // Empty parens
        assert_eq!(extract_short_name("English ()"), "English ()");
        // Nested
        assert_eq!(extract_short_name("Foo (Bar (Baz))"), "Baz");
    }

    #[test]
    fn test_extract_short_name_variant_descriptions() {
        // Variant descriptions with spaces should fall back to the base name,
        // which then resolves via xkb_names to a 2-letter code
        assert_eq!(extract_short_name("Swedish (no dead keys)"), "SE");
        assert_eq!(extract_short_name("German (with AltGr dead keys)"), "DE");
        assert_eq!(extract_short_name("French (alt.)"), "alt.");
        // Single-word variants are still treated as short codes
        assert_eq!(extract_short_name("German (Dvorak)"), "Dvorak");
        assert_eq!(extract_short_name("Russian (phonetic)"), "phonetic");
    }

    #[test]
    fn test_keyboard_layout_config_defaults() {
        let entry = WidgetEntry {
            name: "keyboard_layout".to_string(),
            options: Default::default(),
        };
        let config = KeyboardLayoutConfig::from_entry(&entry);
        assert!(config.show_icon);
        assert!(config.show_label);
        assert_eq!(config.format, LayoutFormat::Short);
    }

    #[test]
    fn test_keyboard_layout_config_custom() {
        let mut options = std::collections::HashMap::new();
        options.insert("show_icon".to_string(), toml::Value::Boolean(false));
        options.insert("show_label".to_string(), toml::Value::Boolean(true));
        options.insert(
            "format".to_string(),
            toml::Value::String("long".to_string()),
        );

        let entry = WidgetEntry {
            name: "keyboard_layout".to_string(),
            options,
        };
        let config = KeyboardLayoutConfig::from_entry(&entry);
        assert!(!config.show_icon);
        assert!(config.show_label);
        assert_eq!(config.format, LayoutFormat::Long);
    }
}
