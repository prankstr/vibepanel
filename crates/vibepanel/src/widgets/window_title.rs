//! Window title widget - displays the focused window's title.
//!
//! Shows the title of the currently focused window with optional app icon.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Box as GtkBox, Image, Label, Orientation};
use tracing::{debug, trace};
use vibepanel_core::config::WidgetEntry;

use crate::services::config_manager::ConfigManager;
use crate::services::icons::get_app_icon_name;
use crate::services::tooltip::TooltipManager;
use crate::services::window_title::{WindowTitleService, WindowTitleSnapshot};
use crate::styles::{icon, widget as wgt};
use crate::widgets::WidgetConfig;
use crate::widgets::base::BaseWidget;
use crate::widgets::warn_unknown_options;

const DEFAULT_EMPTY_TEXT: &str = "—";
const DEFAULT_TEMPLATE: &str = "{display}";
const DEFAULT_SHOW_APP_FALLBACK: bool = true;
const DEFAULT_MAX_CHARS: i32 = 0;
const DEFAULT_SHOW_ICON: bool = true;
const DEFAULT_UPPERCASE: bool = false;

/// Configuration for the window title widget.
#[derive(Debug, Clone)]
pub struct WindowTitleConfig {
    /// Text to show when no window is focused.
    pub empty_text: String,
    /// Template string for rendering the title.
    /// Supports {title}, {app_id}, {app}, {display}, {content}.
    pub template: String,
    /// Whether to show the app name as fallback.
    pub show_app_fallback: bool,
    /// Maximum characters to display (0 = unlimited).
    pub max_chars: i32,
    /// Whether to show the app icon.
    pub show_icon: bool,
    /// Whether to uppercase the title.
    pub uppercase: bool,
}

impl WidgetConfig for WindowTitleConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options(
            "window_title",
            entry,
            &[
                "empty_text",
                "template",
                "show_app_fallback",
                "max_chars",
                "show_icon",
                "uppercase",
            ],
        );

        let empty_text = entry
            .options
            .get("empty_text")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_EMPTY_TEXT)
            .to_string();

        let template = entry
            .options
            .get("template")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_TEMPLATE)
            .to_string();

        let show_app_fallback = entry
            .options
            .get("show_app_fallback")
            .and_then(|v| v.as_bool())
            .unwrap_or(DEFAULT_SHOW_APP_FALLBACK);

        let max_chars = entry
            .options
            .get("max_chars")
            .and_then(|v| v.as_integer())
            .map(|v| v as i32)
            .unwrap_or(DEFAULT_MAX_CHARS);

        let show_icon = entry
            .options
            .get("show_icon")
            .and_then(|v| v.as_bool())
            .unwrap_or(DEFAULT_SHOW_ICON);

        let uppercase = entry
            .options
            .get("uppercase")
            .and_then(|v| v.as_bool())
            .unwrap_or(DEFAULT_UPPERCASE);

        Self {
            empty_text,
            template,
            show_app_fallback,
            max_chars,
            show_icon,
            uppercase,
        }
    }
}

impl Default for WindowTitleConfig {
    fn default() -> Self {
        Self {
            empty_text: DEFAULT_EMPTY_TEXT.to_string(),
            template: DEFAULT_TEMPLATE.to_string(),
            show_app_fallback: DEFAULT_SHOW_APP_FALLBACK,
            max_chars: DEFAULT_MAX_CHARS,
            show_icon: DEFAULT_SHOW_ICON,
            uppercase: DEFAULT_UPPERCASE,
        }
    }
}

/// Window title widget that displays the focused window's title.
pub struct WindowTitleWidget {
    /// Shared base widget container.
    base: BaseWidget,
}

impl WindowTitleWidget {
    /// Create a new window title widget with the given configuration.
    ///
    /// The `output_id` parameter is the monitor connector name (e.g., "eDP-1")
    /// used to filter window title updates to only show windows on this monitor.
    /// If `None`, the widget shows the globally focused window regardless of monitor.
    pub fn new(config: WindowTitleConfig, output_id: Option<String>) -> Self {
        let base = BaseWidget::new(&[wgt::WINDOW_TITLE]);

        // Use the content box provided by BaseWidget (has .content CSS class)
        let content = base.content();

        // Create optional icon (icon + container tuple)
        let icon_widgets = if config.show_icon {
            let icon_img = Image::from_icon_name("application-default-icon");
            icon_img.add_css_class(icon::TEXT);
            icon_img.add_css_class(wgt::WINDOW_TITLE_APP_ICON);

            // Set pixel size to scale with bar size (same as system tray icons)
            let icon_size = ConfigManager::global().theme_sizes().pixmap_icon_size as i32;
            icon_img.set_pixel_size(icon_size);

            // Wrap in icon-root container for consistent sizing with other icons
            let icon_root = GtkBox::new(Orientation::Horizontal, 0);
            icon_root.add_css_class(icon::ROOT);
            icon_root.set_visible(false); // Start hidden (container controls visibility)
            icon_root.append(&icon_img);

            content.append(&icon_root);
            Some((icon_img, icon_root))
        } else {
            None
        };

        // Create label
        let label = Label::new(Some(&config.empty_text));
        label.add_css_class(wgt::WINDOW_TITLE_LABEL);
        label.set_xalign(0.0);
        // Always use ellipsization at the end so long titles
        // show "…" instead of being hard-clipped by section bounds.
        label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        label.set_single_line_mode(true);
        if config.max_chars > 0 {
            label.set_max_width_chars(config.max_chars);
        }
        content.append(&label);

        // State owned by the callback.
        let app_name_cache = Rc::new(RefCell::new(HashMap::<String, String>::new()));
        let base_widget = base.widget().clone();

        // Clone output_id for debug log (the original moves into the closure)
        let output_id_for_log = output_id.clone();

        // Connect to window title service.
        // The callback owns clones of the GTK widgets and config.
        // Each widget remembers its last state - we only update when a window
        // on THIS monitor gains focus, otherwise we keep showing the last value.
        WindowTitleService::global().connect(move |snapshot| {
            // Filter by output_id if specified
            if let Some(ref target_output) = output_id {
                // Only update if window is on this monitor
                if let Some(ref window_output) = snapshot.output
                    && window_output != target_output
                {
                    // Window is on a different monitor - keep current display, don't update
                    trace!(
                        "WindowTitle: ignoring update for {}, window is on {}",
                        target_output, window_output
                    );
                    return;
                }
                // If snapshot.output is None, we show it (compositor doesn't report output)
            }

            // Update the widget with the new window info
            update_window_title(
                &label,
                icon_widgets.as_ref(),
                &base_widget,
                &config,
                &app_name_cache,
                snapshot,
            );
        });

        debug!(
            "WindowTitleWidget created (output_id={:?})",
            output_id_for_log
        );
        Self { base }
    }

    /// Get the root GTK widget for embedding in the bar.
    pub fn widget(&self) -> &gtk4::Box {
        self.base.widget()
    }
}

/// Update the widget with new window info.
fn update_window_title(
    label: &Label,
    icon_widgets: Option<&(Image, GtkBox)>,
    base_widget: &GtkBox,
    config: &WindowTitleConfig,
    app_name_cache: &Rc<RefCell<HashMap<String, String>>>,
    snapshot: &WindowTitleSnapshot,
) {
    let text = render_title(config, app_name_cache, snapshot);
    label.set_label(&text);

    // Update icon if enabled
    if let Some((icon, icon_root)) = icon_widgets {
        update_icon(icon, icon_root, snapshot);
    }

    // Update tooltip
    update_tooltip(base_widget, config, app_name_cache, snapshot);
}

/// Render the title text from the snapshot.
fn render_title(
    config: &WindowTitleConfig,
    app_name_cache: &Rc<RefCell<HashMap<String, String>>>,
    snapshot: &WindowTitleSnapshot,
) -> String {
    let friendly_app = friendly_app_name(app_name_cache, &snapshot.app_id);

    // Build display text
    let title = snapshot.title.trim();
    let content = clean_title(title, &friendly_app);

    // Determine display text
    let display = if content.is_empty() && config.show_app_fallback {
        friendly_app.clone()
    } else if config.show_app_fallback
        && !friendly_app.is_empty()
        && !content.starts_with(&friendly_app)
    {
        format!("{} — {}", friendly_app, content)
    } else {
        content.clone()
    };

    // Render template using a fixed array (avoids HashMap allocation)
    let mut result = config.template.clone();
    for (key, value) in [
        ("title", title),
        ("app_id", snapshot.app_id.as_str()),
        ("appid", snapshot.app_id.as_str()),
        ("app", friendly_app.as_str()),
        ("friendly_app", friendly_app.as_str()),
        ("content", content.as_str()),
        ("display", display.as_str()),
    ] {
        result = result.replace(&format!("{{{}}}", key), value);
    }

    // Apply transformations
    let text = if result.trim().is_empty() {
        if config.show_app_fallback && !friendly_app.is_empty() {
            friendly_app
        } else if !title.is_empty() {
            title.to_string()
        } else {
            config.empty_text.clone()
        }
    } else {
        result.trim().to_string()
    };

    if config.uppercase {
        text.to_uppercase()
    } else {
        text
    }
}

/// Clean the title by removing app name duplicates.
///
/// Removes app name duplicates from the title by:
/// - Normalizing both the friendly app name and title segments
/// - Tokenizing on common separators ("_-. ")
/// - Treating segments as duplicates when token sets overlap
fn clean_title(title: &str, friendly_app: &str) -> String {
    if title.is_empty() {
        return String::new();
    }

    // Common title delimiters: hyphen, en-dash, em-dash, pipe, bullet, middle dot
    const DELIMITERS: &[char] = &['-', '\u{2013}', '\u{2014}', '|', '\u{2022}', '\u{00b7}'];

    // Normalize: trim, lowercase, strip leading @: and spaces
    fn normalize(value: &str) -> String {
        let trimmed = value.trim().to_lowercase();
        trimmed.trim_start_matches(['@', ':', ' ']).to_string()
    }

    fn tokenize(normalized: &str) -> std::collections::HashSet<&str> {
        normalized
            .split(['_', '-', '.', ' '])
            .filter(|t| !t.is_empty())
            .collect()
    }

    let friendly_norm = normalize(friendly_app);
    let friendly_tokens = if friendly_norm.is_empty() {
        std::collections::HashSet::new()
    } else {
        tokenize(&friendly_norm)
    };

    fn matches_friendly(
        normalized_segment: &str,
        friendly_norm: &str,
        friendly_tokens: &std::collections::HashSet<&str>,
    ) -> bool {
        if normalized_segment.is_empty() {
            return false;
        }
        if !friendly_norm.is_empty() && normalized_segment == friendly_norm {
            return true;
        }
        if friendly_tokens.is_empty() {
            return false;
        }
        let segment_tokens = tokenize(normalized_segment);
        if segment_tokens.is_empty() {
            return false;
        }
        segment_tokens.is_subset(friendly_tokens) || friendly_tokens.is_subset(&segment_tokens)
    }

    let mut segments: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for raw in title.split(|c| DELIMITERS.contains(&c)) {
        let segment = raw.trim();
        if segment.is_empty() {
            continue;
        }
        let normalized = normalize(segment);
        if normalized.is_empty()
            || seen.contains(&normalized)
            || matches_friendly(&normalized, &friendly_norm, &friendly_tokens)
        {
            continue;
        }
        seen.insert(normalized);
        segments.push(segment.to_string());
    }

    segments.join(" \u{2014} ")
}

/// Get a friendly app name from the app_id.
fn friendly_app_name(cache: &Rc<RefCell<HashMap<String, String>>>, app_id: &str) -> String {
    if app_id.is_empty() {
        return String::new();
    }

    // Check cache
    if let Some(cached) = cache.borrow().get(app_id) {
        return cached.clone();
    }

    // Try to derive from app_id
    let base = app_id.trim().trim_start_matches(['@', ':', ' ']);
    if base.is_empty() {
        cache.borrow_mut().insert(app_id.to_string(), String::new());
        return String::new();
    }

    // Split by common delimiters and get last meaningful token
    let stop_words = ["desktop", "client", "app", "bin"];
    let tokens: Vec<&str> = base
        .split(['_', '-', '.', ' '])
        .filter(|t| !t.is_empty() && !stop_words.contains(&t.to_lowercase().as_str()))
        .collect();

    let friendly = tokens
        .last()
        .map(|t| titlecase(t))
        .unwrap_or_else(|| titlecase(base));

    cache
        .borrow_mut()
        .insert(app_id.to_string(), friendly.clone());
    friendly
}

/// Update the icon based on current app_id.
fn update_icon(icon: &Image, icon_root: &GtkBox, snapshot: &WindowTitleSnapshot) {
    if snapshot.app_id.is_empty() {
        icon_root.set_visible(false);
        return;
    }

    // Use the desktop app info lookup to find the correct icon name.
    // This handles cases like "zen" -> "zen-browser" via StartupWMClass matching.
    let icon_name = get_app_icon_name(&snapshot.app_id);

    if icon_name.is_empty() {
        // Fallback: try the app_id as a direct icon name
        let fallback = snapshot.app_id.to_lowercase();
        icon.set_icon_name(Some(&fallback));
    } else {
        icon.set_icon_name(Some(&icon_name));
    }

    icon_root.set_visible(true);
}

/// Update the tooltip.
fn update_tooltip(
    base_widget: &GtkBox,
    config: &WindowTitleConfig,
    app_name_cache: &Rc<RefCell<HashMap<String, String>>>,
    snapshot: &WindowTitleSnapshot,
) {
    let friendly = friendly_app_name(app_name_cache, &snapshot.app_id);

    let mut lines = Vec::new();
    if !friendly.is_empty() || !snapshot.app_id.is_empty() {
        let app_label = if !friendly.is_empty() {
            &friendly
        } else {
            &snapshot.app_id
        };
        lines.push(format!("App: {}", app_label));
    }
    if !snapshot.app_id.is_empty() {
        lines.push(format!("ID: {}", snapshot.app_id));
    }
    if !snapshot.title.is_empty() {
        lines.push(format!("Title: {}", snapshot.title));
    }
    if let Some(output) = &snapshot.output {
        lines.push(format!("Output: {}", output));
    }

    let tooltip_text = if lines.is_empty() {
        config.empty_text.clone()
    } else {
        lines.join("\n")
    };

    TooltipManager::global().set_styled_tooltip(base_widget, &tooltip_text);
}

/// Convert a string to title case.
fn titlecase(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
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
    fn test_window_title_config_default() {
        let entry = make_widget_entry("window_title", HashMap::new());
        let config = WindowTitleConfig::from_entry(&entry);
        assert_eq!(config.empty_text, "—");
        assert_eq!(config.template, "{display}");
        assert!(config.show_app_fallback);
        assert_eq!(config.max_chars, 0);
        assert!(config.show_icon);
        assert!(!config.uppercase);
    }

    #[test]
    fn test_window_title_config_custom() {
        let mut options = HashMap::new();
        options.insert(
            "empty_text".to_string(),
            Value::String("No window".to_string()),
        );
        options.insert(
            "template".to_string(),
            Value::String("{app}: {title}".to_string()),
        );
        options.insert("max_chars".to_string(), Value::Integer(50));
        options.insert("uppercase".to_string(), Value::Boolean(true));
        let entry = make_widget_entry("window_title", options);
        let config = WindowTitleConfig::from_entry(&entry);
        assert_eq!(config.empty_text, "No window");
        assert_eq!(config.template, "{app}: {title}");
        assert_eq!(config.max_chars, 50);
        assert!(config.uppercase);
    }

    #[test]
    fn test_titlecase() {
        assert_eq!(titlecase("firefox"), "Firefox");
        assert_eq!(titlecase("FIREFOX"), "FIREFOX");
        assert_eq!(titlecase(""), "");
        assert_eq!(titlecase("a"), "A");
    }

    #[test]
    fn test_clean_title_removes_exact_and_variant_app_segments() {
        // Exact match
        let cleaned = clean_title("Firefox — Some Page", "Firefox");
        assert_eq!(cleaned, "Some Page");

        // Variant: title contains "Mozilla Firefox", friendly app is "Firefox"
        let cleaned_variant = clean_title("Mozilla Firefox — Some Page", "Firefox");
        assert_eq!(cleaned_variant, "Some Page");

        // Variant: friendly app "Mozilla Firefox", title segment "Firefox"
        let cleaned_variant2 = clean_title("Firefox — Some Page", "Mozilla Firefox");
        assert_eq!(cleaned_variant2, "Some Page");
    }

    #[test]
    fn test_clean_title_empty_inputs() {
        // Empty title returns empty string
        assert_eq!(clean_title("", "Firefox"), "");

        // Empty friendly app - should keep all segments
        assert_eq!(
            clean_title("Firefox — Some Page", ""),
            "Firefox \u{2014} Some Page"
        );

        // Both empty
        assert_eq!(clean_title("", ""), "");
    }

    #[test]
    fn test_clean_title_only_delimiters() {
        // Title with only delimiters/whitespace
        assert_eq!(clean_title("—", "Firefox"), "");
        assert_eq!(clean_title(" — ", "Firefox"), "");
        assert_eq!(clean_title("- | -", "Firefox"), "");
    }

    #[test]
    fn test_clean_title_unicode_delimiters() {
        // En-dash (U+2013)
        let cleaned_endash = clean_title("Firefox \u{2013} Some Page", "Firefox");
        assert_eq!(cleaned_endash, "Some Page");

        // Em-dash (U+2014)
        let cleaned_emdash = clean_title("Firefox \u{2014} Some Page", "Firefox");
        assert_eq!(cleaned_emdash, "Some Page");

        // Pipe
        let cleaned_pipe = clean_title("Firefox | Some Page", "Firefox");
        assert_eq!(cleaned_pipe, "Some Page");

        // Bullet (U+2022)
        let cleaned_bullet = clean_title("Firefox \u{2022} Some Page", "Firefox");
        assert_eq!(cleaned_bullet, "Some Page");

        // Middle dot (U+00B7)
        let cleaned_middot = clean_title("Firefox \u{00b7} Some Page", "Firefox");
        assert_eq!(cleaned_middot, "Some Page");
    }

    #[test]
    fn test_clean_title_multiple_segments() {
        // Multiple segments, first matches app
        let cleaned = clean_title("Firefox — Tab 1 — mozilla.org", "Firefox");
        assert_eq!(cleaned, "Tab 1 \u{2014} mozilla.org");

        // Multiple segments, middle matches app
        let cleaned_mid = clean_title("Tab 1 — Firefox — mozilla.org", "Firefox");
        assert_eq!(cleaned_mid, "Tab 1 \u{2014} mozilla.org");

        // Multiple segments, last matches app
        let cleaned_last = clean_title("Tab 1 — mozilla.org — Firefox", "Firefox");
        assert_eq!(cleaned_last, "Tab 1 \u{2014} mozilla.org");
    }

    #[test]
    fn test_clean_title_duplicate_segments() {
        // Duplicate segments should be deduplicated
        let cleaned = clean_title("Page — Page — Firefox", "Firefox");
        assert_eq!(cleaned, "Page");
    }

    #[test]
    fn test_clean_title_case_insensitive() {
        // Case should not matter for matching
        let cleaned = clean_title("FIREFOX — Some Page", "firefox");
        assert_eq!(cleaned, "Some Page");

        let cleaned_rev = clean_title("firefox — Some Page", "FIREFOX");
        assert_eq!(cleaned_rev, "Some Page");
    }

    #[test]
    fn test_clean_title_leading_special_chars() {
        // Leading @, :, space should be stripped during normalization
        let cleaned = clean_title("@Firefox — Some Page", "Firefox");
        assert_eq!(cleaned, "Some Page");

        let cleaned_colon = clean_title(":Firefox — Some Page", "Firefox");
        assert_eq!(cleaned_colon, "Some Page");
    }

    #[test]
    fn test_clean_title_preserves_original_case() {
        // Output should preserve the original casing of non-app segments
        let cleaned = clean_title("Firefox — SoMe WeIrD CaSe", "Firefox");
        assert_eq!(cleaned, "SoMe WeIrD CaSe");
    }
}
