//! Window title widget - displays the focused window's title.
//!
//! Shows the title of the currently focused window with optional app icon.
//!
//! - `filter_by_output`: `true` (default) shows the window on this bar's
//!   output, keeping it while focus is on another output; `false` shows the
//!   globally focused window on every bar.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, CenterBox, Image, Label, Orientation};
use tr::tr;
use tracing::{debug, trace};
use vibepanel_core::config::WidgetEntry;

use crate::services::callbacks::CallbackId;
use crate::services::compositor::{CompositorManager, WindowInfo};
use crate::services::config_manager::ConfigManager;
use crate::services::icons::get_app_icon_name;
use crate::services::tooltip::set_tooltip;
use crate::styles::{icon, widget as wgt};
use crate::widgets::WidgetConfig;
use crate::widgets::base::BaseWidget;
use crate::widgets::warn_unknown_options;

const DEFAULT_EMPTY_TEXT: &str = "";
const DEFAULT_TEMPLATE: &str = "{display}";
const DEFAULT_SHOW_APP_FALLBACK: bool = true;
const DEFAULT_MAX_CHARS: i32 = 0;
const DEFAULT_SHOW_ICON: bool = true;
const DEFAULT_UPPERCASE: bool = false;
const DEFAULT_FILTER_BY_OUTPUT: bool = true;

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
    /// Icon size in pixels. `None` uses the theme/default sizing behavior.
    pub icon_size: Option<i32>,
    /// Whether to uppercase the title.
    pub uppercase: bool,
    /// Whether to show this bar output's window (`true`) or the globally
    /// focused window (`false`).
    pub filter_by_output: bool,
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
                "icon_size",
                "uppercase",
                "filter_by_output",
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

        let icon_size = entry
            .options
            .get("icon_size")
            .and_then(|v| v.as_integer())
            .map(|v| (v as i32).max(8));

        let uppercase = entry
            .options
            .get("uppercase")
            .and_then(|v| v.as_bool())
            .unwrap_or(DEFAULT_UPPERCASE);

        let filter_by_output = entry
            .options
            .get("filter_by_output")
            .and_then(|v| v.as_bool())
            .unwrap_or(DEFAULT_FILTER_BY_OUTPUT);

        Self {
            empty_text,
            template,
            show_app_fallback,
            max_chars,
            show_icon,
            icon_size,
            uppercase,
            filter_by_output,
        }
    }
}

/// Window title widget that displays the focused window's title.
pub struct WindowTitleWidget {
    /// Shared base widget container.
    base: BaseWidget,
    /// Callback ID for CompositorManager window updates, used to disconnect on drop.
    window_title_callback_id: CallbackId,
}

#[derive(Clone)]
struct WindowTitleLabel {
    wrapper: CenterBox,
    label: Label,
}

impl WindowTitleWidget {
    /// Create a new window title widget with the given configuration.
    ///
    /// The `output_id` parameter is the monitor connector name (e.g., "eDP-1")
    /// used to show only this monitor's window when `filter_by_output` is set.
    /// If `None`, the widget shows the globally focused window regardless of monitor.
    pub fn new(config: WindowTitleConfig, output_id: Option<String>) -> Self {
        let base = BaseWidget::new(&[wgt::WINDOW_TITLE]);
        let is_vertical = ConfigManager::global().bar_position().is_vertical();

        // Use the content box provided by BaseWidget (has .content CSS class)
        let content = base.content();
        if !is_vertical {
            content.set_halign(Align::Fill);
            content.set_hexpand(true);
        }

        // Create optional icon (icon + container tuple)
        let icon_widgets = if config.show_icon {
            let icon_img = Image::from_icon_name("application-default-icon");
            icon_img.add_css_class(icon::TEXT);
            icon_img.add_css_class(wgt::WINDOW_TITLE_APP_ICON);
            // Mirror IconsService Image backend: valign only, no halign override.
            icon_img.set_valign(Align::Center);

            let icon_size = window_title_icon_size(is_vertical, config.icon_size);
            icon_img.set_pixel_size(icon_size);

            // Wrap in icon-root container — mirror IconsService.create_icon and
            // BaseWidget::add_icon (Center + hexpand keeps it centered in the
            // content's available width without depending on intrinsic metrics).
            let icon_root = GtkBox::new(Orientation::Horizontal, 0);
            icon_root.add_css_class(icon::ROOT);
            icon_root.set_valign(Align::Center);
            icon_root.set_halign(Align::Center);
            icon_root.set_hexpand(true);
            if is_vertical {
                icon_root.set_vexpand(true);
                // Constant 1px right nudge for residual centering asymmetry.
                icon_root.set_margin_start(1);
            }
            icon_root.set_visible(false); // Start hidden (container controls visibility)
            icon_root.append(&icon_img);

            content.append(&icon_root);
            Some((icon_img, icon_root))
        } else {
            None
        };

        // Create label
        let title_label = WindowTitleLabel {
            wrapper: CenterBox::new(),
            label: Label::new(Some(&config.empty_text)),
        };
        title_label.label.add_css_class(wgt::WINDOW_TITLE_LABEL);
        if is_vertical {
            title_label.label.set_visible(false);
        }
        set_label_alignment(&title_label, is_vertical, false);
        // Always use ellipsization at the end so long titles
        // show "…" instead of being hard-clipped by section bounds.
        title_label
            .label
            .set_ellipsize(gtk4::pango::EllipsizeMode::End);
        title_label.label.set_single_line_mode(true);
        if config.max_chars > 0 {
            title_label.label.set_max_width_chars(config.max_chars);
        }
        if !is_vertical {
            title_label
                .wrapper
                .set_center_widget(Some(&title_label.label));
            content.append(&title_label.wrapper);
        }

        let initial_visible = should_show_window_title(&config.empty_text, false, is_vertical);
        base.widget().set_visible(initial_visible);

        // State owned by the callback.
        let app_name_cache = Rc::new(RefCell::new(HashMap::<String, String>::new()));
        let base_widget = base.widget().clone();

        // Only follow this bar's output when filtering by output.
        let output_id = output_id.filter(|_| config.filter_by_output);
        let output_id_for_log = output_id.clone();

        // Connect to window title service.
        // The callback owns clones of the GTK widgets and config.
        // Backends emit one update per output; keep the ones this widget follows.
        let window_title_callback_id =
            CompositorManager::global().register_window_callback(move |snapshot| {
                if !accepts_update(output_id.as_deref(), snapshot) {
                    trace!(
                        "WindowTitle: ignoring update for {:?} (focused={}), widget follows {:?}",
                        snapshot.output, snapshot.focused, output_id
                    );
                    return;
                }

                // Update the widget with the new window info
                update_window_title(
                    &title_label,
                    icon_widgets.as_ref(),
                    &base_widget,
                    &config,
                    &app_name_cache,
                    snapshot,
                    is_vertical,
                );
            });

        debug!(
            "WindowTitleWidget created (output_id={:?})",
            output_id_for_log
        );
        Self {
            base,
            window_title_callback_id,
        }
    }

    /// Get the root GTK widget for embedding in the bar.
    pub fn widget(&self) -> &gtk4::Box {
        self.base.widget()
    }
}

/// Whether a widget following `output_id` (or the focused output when `None`)
/// should apply this update. Unscoped updates apply everywhere.
fn accepts_update(output_id: Option<&str>, snapshot: &WindowInfo) -> bool {
    match (output_id, snapshot.output.as_deref()) {
        (_, None) => true,
        (Some(target), Some(output)) => target == output,
        (None, Some(_)) => snapshot.focused,
    }
}

fn window_title_icon_size(is_vertical: bool, configured_icon_size: Option<i32>) -> i32 {
    let sizes = ConfigManager::global().theme_sizes();
    window_title_icon_size_from_values(
        is_vertical,
        configured_icon_size,
        sizes.pixmap_icon_size,
        sizes.widget_height,
        sizes.bar_height,
    )
}

fn window_title_icon_size_from_values(
    _is_vertical: bool,
    configured_icon_size: Option<i32>,
    pixmap_icon_size: u32,
    widget_height: u32,
    bar_size: u32,
) -> i32 {
    let requested_icon_size = configured_icon_size
        .map(|size| size.max(1) as u32)
        .unwrap_or(pixmap_icon_size);
    let icon_size = requested_icon_size.min(widget_height);
    // Match parity to bar_size: content width inherits bar_size parity, and
    // matched parity makes (content - icon) even so centering lands on a whole
    // pixel instead of a half-pixel that biases left or right.
    let aligned = if icon_size % 2 == bar_size % 2 {
        icon_size
    } else {
        icon_size.saturating_sub(1)
    };
    aligned as i32
}

impl Drop for WindowTitleWidget {
    fn drop(&mut self) {
        CompositorManager::global().unregister_window_callback(self.window_title_callback_id);
    }
}

/// Update the widget with new window info.
fn update_window_title(
    title_label: &WindowTitleLabel,
    icon_widgets: Option<&(Image, GtkBox)>,
    base_widget: &GtkBox,
    config: &WindowTitleConfig,
    app_name_cache: &Rc<RefCell<HashMap<String, String>>>,
    snapshot: &WindowInfo,
    is_vertical: bool,
) {
    let text = render_title(config, app_name_cache, snapshot);
    title_label.label.set_label(&text);

    // Update icon if enabled
    let mut icon_visible = false;
    if let Some((icon, icon_root)) = icon_widgets {
        icon_visible = update_icon(icon, icon_root, snapshot);
    }
    set_label_alignment(title_label, is_vertical, icon_visible);

    base_widget.set_visible(should_show_window_title(&text, icon_visible, is_vertical));

    // Update tooltip
    update_tooltip(base_widget, config, app_name_cache, snapshot);
}

fn should_show_window_title(text: &str, icon_visible: bool, is_vertical: bool) -> bool {
    if is_vertical {
        icon_visible
    } else {
        !text.trim().is_empty() || icon_visible
    }
}

fn set_label_alignment(title_label: &WindowTitleLabel, is_vertical: bool, icon_visible: bool) {
    let label_wrapper = &title_label.wrapper;
    let label = &title_label.label;
    if is_vertical {
        label_wrapper.set_halign(Align::Center);
        label_wrapper.set_hexpand(false);
        label.set_halign(Align::Center);
        label.set_hexpand(false);
        label.set_xalign(0.5);
        label.set_margin_start(0);
    } else if icon_visible {
        label_wrapper.set_halign(Align::Fill);
        label_wrapper.set_hexpand(false);
        label.set_halign(Align::Fill);
        label.set_hexpand(false);
        label.set_xalign(0.0);
        label.set_margin_start(0);
    } else {
        // CenterBox claims the square floor while the label keeps its natural
        // width, so centering is layout-driven rather than a pixel nudge.
        label_wrapper.set_halign(Align::Fill);
        label_wrapper.set_hexpand(true);
        label.set_halign(Align::Center);
        label.set_hexpand(false);
        label.set_xalign(0.5);
        label.set_margin_start(0);
    }
}

/// Render the title text from the snapshot.
fn render_title(
    config: &WindowTitleConfig,
    app_name_cache: &Rc<RefCell<HashMap<String, String>>>,
    snapshot: &WindowInfo,
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
///
/// Original delimiters between segments are preserved so the title
/// is not visually modified beyond removing matched segments.
fn clean_title(title: &str, friendly_app: &str) -> String {
    if title.is_empty() {
        return String::new();
    }

    // Common title delimiters: en-dash, em-dash, pipe, bullet, middle dot
    // Note: bare hyphen '-' is NOT a delimiter — it appears inside words
    // like "my-project". Spaced hyphens " - " are pre-normalized to em-dash.
    const DELIMITERS: &[char] = &['\u{2013}', '\u{2014}', '|', '\u{2022}', '\u{00b7}'];

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

    // Pre-normalize " - " (spaced hyphen) to " — " so it is recognized
    // as a delimiter while bare hyphens inside words are left alone.
    let title = title.replace(" - ", " \u{2014} ");

    // Parse into alternating (segment_text, following_delimiter) pairs,
    // preserving the original delimiter strings between segments.
    let mut parts: Vec<(&str, Option<&str>)> = Vec::new();
    let mut rest = title.as_str();
    loop {
        if let Some(pos) = rest.find(|c: char| DELIMITERS.contains(&c)) {
            let delim_char = rest[pos..].chars().next().unwrap();
            let delim_end = pos + delim_char.len_utf8();
            // Segment text is everything before the delimiter (including its
            // surrounding whitespace — we'll trim when we inspect it).
            let seg = &rest[..pos];
            // Capture the delimiter with its surrounding whitespace so we
            // can reproduce it verbatim when stitching the title back.
            let after = &rest[delim_end..];
            let ws_after = after.len() - after.trim_start().len();
            let ws_before = seg.len() - seg.trim_end().len();
            let delim_span = &rest[pos - ws_before..delim_end + ws_after];
            let seg_text = &rest[..pos - ws_before];
            parts.push((seg_text, Some(delim_span)));
            rest = &rest[delim_end + ws_after..];
        } else {
            parts.push((rest, None));
            break;
        }
    }

    // Filter: remove app-name matches and duplicates, tracking which
    // parts to keep by index.
    let mut kept: Vec<usize> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (i, (seg_text, _)) in parts.iter().enumerate() {
        let segment = seg_text.trim();
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
        kept.push(i);
    }

    // Reconstruct using original delimiters between kept segments.
    // When a removed segment sits between two kept segments, use the
    // delimiter that follows the left kept segment (its original
    // right-hand delimiter).
    let mut result = String::new();
    for (k, &idx) in kept.iter().enumerate() {
        result.push_str(parts[idx].0.trim());
        if k + 1 < kept.len() {
            // Find the delimiter to place between this kept segment and
            // the next one. Walk from the current kept index forward and
            // use the first available delimiter we encounter.
            let next_idx = kept[k + 1];
            let delim = (idx..next_idx)
                .find_map(|j| parts[j].1)
                .expect("delimiter must exist between consecutive kept segments");
            result.push_str(delim);
        }
    }

    result
}

/// Get a friendly app name from the app_id.
pub(crate) fn friendly_app_name(
    cache: &Rc<RefCell<HashMap<String, String>>>,
    app_id: &str,
) -> String {
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
fn update_icon(icon: &Image, icon_root: &GtkBox, snapshot: &WindowInfo) -> bool {
    if snapshot.app_id.is_empty() {
        icon_root.set_visible(false);
        return false;
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
    true
}

/// Update the tooltip.
fn update_tooltip(
    base_widget: &GtkBox,
    config: &WindowTitleConfig,
    app_name_cache: &Rc<RefCell<HashMap<String, String>>>,
    snapshot: &WindowInfo,
) {
    let friendly = friendly_app_name(app_name_cache, &snapshot.app_id);

    let mut lines = Vec::new();
    if !friendly.is_empty() || !snapshot.app_id.is_empty() {
        let app_label = if !friendly.is_empty() {
            &friendly
        } else {
            &snapshot.app_id
        };
        lines.push(tr!("App: {app_label}", app_label));
    }
    if !snapshot.app_id.is_empty() {
        lines.push(tr!("ID: {app_id}", app_id = snapshot.app_id));
    }
    if !snapshot.title.is_empty() {
        lines.push(tr!("Title: {title}", title = snapshot.title));
    }
    if let Some(output) = &snapshot.output {
        lines.push(tr!("Output: {output}", output));
    }

    let tooltip_text = if lines.is_empty() {
        config.empty_text.clone()
    } else {
        lines.join("\n")
    };

    set_tooltip(base_widget, Some(&tooltip_text));
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
    fn test_window_title_config() {
        let entry = make_widget_entry("window_title", HashMap::new());
        let config = WindowTitleConfig::from_entry(&entry);
        assert_eq!(config.empty_text, "");
        assert_eq!(config.template, "{display}");
        assert!(config.show_app_fallback);
        assert_eq!(config.max_chars, 0);
        assert!(config.show_icon);
        assert_eq!(config.icon_size, None);
        assert!(!config.uppercase);
        assert!(config.filter_by_output);

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
        options.insert("icon_size".to_string(), Value::Integer(24));
        options.insert("uppercase".to_string(), Value::Boolean(true));
        let entry = make_widget_entry("window_title", options);
        let config = WindowTitleConfig::from_entry(&entry);
        assert_eq!(config.empty_text, "No window");
        assert_eq!(config.template, "{app}: {title}");
        assert_eq!(config.max_chars, 50);
        assert_eq!(config.icon_size, Some(24));
        assert!(config.uppercase);

        let mut options = HashMap::new();
        options.insert("icon_size".to_string(), Value::Integer(2));
        let entry = make_widget_entry("window_title", options);
        let config = WindowTitleConfig::from_entry(&entry);
        assert_eq!(config.icon_size, Some(8), "icon_size clamps to minimum");
    }

    #[test]
    fn accepts_update_follows_output_or_focus() {
        let snapshot = |output: Option<&str>, focused| WindowInfo {
            output: output.map(str::to_string),
            focused,
            ..Default::default()
        };

        // (widget's output, update's output, update focused, accepted)
        let cases = [
            (Some("DP-1"), Some("DP-1"), false, true), // own output
            (Some("DP-1"), Some("DP-2"), true, false), // other output, even focused
            (None, Some("DP-2"), true, true),          // following focus
            (None, Some("DP-1"), false, false),        // following focus, unfocused
            (Some("DP-1"), None, false, true),         // unscoped update
            (None, None, false, true),                 // unscoped update
        ];
        for (widget, output, focused, expected) in cases {
            assert_eq!(
                accepts_update(widget, &snapshot(output, focused)),
                expected,
                "widget={widget:?} output={output:?} focused={focused}"
            );
        }
    }

    #[test]
    fn test_titlecase() {
        assert_eq!(titlecase("firefox"), "Firefox");
        assert_eq!(titlecase("FIREFOX"), "FIREFOX");
        assert_eq!(titlecase(""), "");
        assert_eq!(titlecase("a"), "A");
    }

    #[test]
    fn test_window_title_icon_size() {
        // (name, configured, pixmap, widget_height, bar_size, expected)
        let cases = [
            ("default on even bar", None, 20, 26, 32, 20),
            ("larger default on even bar", None, 24, 26, 32, 24),
            // Parity follows the bar so centering lands on a whole pixel.
            ("default on odd bar flips parity", None, 20, 26, 33, 19),
            ("override", Some(18), 20, 26, 32, 18),
            ("override capped to widget height", Some(40), 20, 26, 32, 26),
        ];

        for (name, configured, pixmap, widget_height, bar_size, expected) in cases {
            assert_eq!(
                window_title_icon_size_from_values(
                    false,
                    configured,
                    pixmap,
                    widget_height,
                    bar_size
                ),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn test_should_show_window_title_visibility_rules() {
        assert!(!should_show_window_title("", false, false));
        assert!(should_show_window_title("—", false, false));
        assert!(should_show_window_title("", true, false));
        assert!(!should_show_window_title("anything", false, true));
        assert!(should_show_window_title("", true, true));
    }

    #[test]
    fn test_clean_title() {
        // (name, title, friendly app, expected)
        #[rustfmt::skip]
        let cases = [
            // App segment removal
            ("exact app segment", "Firefox — Some Page", "Firefox", "Some Page"),
            ("variant: title has longer app name", "Mozilla Firefox — Some Page", "Firefox", "Some Page"),
            ("variant: app has longer name", "Firefox — Some Page", "Mozilla Firefox", "Some Page"),
            ("case-insensitive, upper title", "FIREFOX — Some Page", "firefox", "Some Page"),
            ("case-insensitive, upper app", "firefox — Some Page", "FIREFOX", "Some Page"),
            ("leading @ ignored when matching", "@Firefox — Some Page", "Firefox", "Some Page"),
            ("leading : ignored when matching", ":Firefox — Some Page", "Firefox", "Some Page"),
            ("preserves original case", "Firefox — SoMe WeIrD CaSe", "Firefox", "SoMe WeIrD CaSe"),
            ("first of several segments", "Firefox — Tab 1 — mozilla.org", "Firefox", "Tab 1 \u{2014} mozilla.org"),
            ("middle of several segments", "Tab 1 — Firefox — mozilla.org", "Firefox", "Tab 1 \u{2014} mozilla.org"),
            ("last of several segments", "Tab 1 — mozilla.org — Firefox", "Firefox", "Tab 1 \u{2014} mozilla.org"),
            ("duplicate segments deduplicated", "Page — Page — Firefox", "Firefox", "Page"),
            // Empty and delimiter-only input
            ("empty title", "", "Firefox", ""),
            ("empty app keeps all segments", "Firefox — Some Page", "", "Firefox \u{2014} Some Page"),
            ("both empty", "", "", ""),
            ("only delimiter", "—", "Firefox", ""),
            ("only spaced delimiter", " — ", "Firefox", ""),
            // Bare "-" is not a delimiter: splits on "|" into ["-", "-"], deduped.
            ("bare hyphens around pipe", "- | -", "Firefox", "-"),
            // Delimiters
            ("en dash", "Firefox \u{2013} Some Page", "Firefox", "Some Page"),
            ("pipe", "Firefox | Some Page", "Firefox", "Some Page"),
            ("bullet", "Firefox \u{2022} Some Page", "Firefox", "Some Page"),
            ("middle dot", "Firefox \u{00b7} Some Page", "Firefox", "Some Page"),
            // Regression: hyphens inside words are not delimiters
            ("hyphenated word kept", "A-B", "SomeApp", "A-B"),
            ("hyphenated project kept", "my-project", "SomeApp", "my-project"),
            ("multi-hyphen word kept", "v1-beta-2", "SomeApp", "v1-beta-2"),
            ("hyphenated word with app removed", "A-B — Firefox", "Firefox", "A-B"),
            ("hyphenated word with variant app removed", "my-project — Visual Studio Code", "Code", "my-project"),
            // Regression: " - " still delimits
            ("spaced hyphen delimiter", "Firefox - Some Page", "Firefox", "Some Page"),
            ("spaced hyphen after hyphenated file", "some-file.rs - VS Code", "Code", "some-file.rs"),
            ("spaced hyphen after hyphenated word", "A-B - Firefox", "Firefox", "A-B"),
            // Regression: original delimiters are preserved, not replaced with em dash
            ("pipes preserved", "Firefox | Tab 1 | mozilla.org", "Firefox", "Tab 1 | mozilla.org"),
            ("mixed delimiters preserved", "Firefox \u{2014} Tab 1 | mozilla.org", "Firefox", "Tab 1 | mozilla.org"),
            ("middle removal keeps left delimiter", "Tab 1 | Firefox \u{2014} mozilla.org", "Firefox", "Tab 1 | mozilla.org"),
            ("bullet preserved", "Tab 1 \u{2022} mozilla.org", "", "Tab 1 \u{2022} mozilla.org"),
            ("no match keeps em dashes", "A \u{2014} B \u{2014} C", "SomeApp", "A \u{2014} B \u{2014} C"),
            ("no match keeps mixed delimiters", "Tab 1 | Page 2 \u{2022} Info", "SomeApp", "Tab 1 | Page 2 \u{2022} Info"),
            // Regression: removing every segment
            ("single segment is app", "Firefox", "Firefox", ""),
            ("all segments are app", "Firefox \u{2014} Firefox", "Firefox", ""),
            ("consecutive app segments removed", "Firefox \u{2014} Firefox \u{2014} Page", "Firefox", "Page"),
            // Regression: delimiters without spaces and at the boundaries
            ("unspaced pipe", "Firefox|Page", "Firefox", "Page"),
            ("unspaced pipes kept", "A|B|C", "SomeApp", "A|B|C"),
            ("leading delimiter, app only", "\u{2014} Firefox", "Firefox", ""),
            ("leading delimiter", "\u{2014} Page", "Firefox", "Page"),
            ("trailing delimiter, app only", "Firefox \u{2014}", "Firefox", ""),
            ("trailing delimiter", "Page \u{2014}", "Firefox", "Page"),
        ];

        for (name, title, app, expected) in cases {
            assert_eq!(
                clean_title(title, app),
                expected,
                "{name}: {title:?} / {app:?}"
            );
        }
    }
}
