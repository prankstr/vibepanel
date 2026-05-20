//! Window title widget - displays the focused window's title.
//!
//! Shows the title of the currently focused window with optional app icon.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, CenterBox, Image, Label, Orientation};
use tracing::{debug, trace};
use vibepanel_core::config::WidgetEntry;

use crate::services::callbacks::CallbackId;
use crate::services::config_manager::ConfigManager;
use crate::services::icons::get_app_icon_name;
use crate::services::tooltip::TooltipManager;
use crate::services::window_title::{WindowTitleService, WindowTitleSnapshot};
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

        Self {
            empty_text,
            template,
            show_app_fallback,
            max_chars,
            show_icon,
            icon_size,
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
            icon_size: None,
            uppercase: DEFAULT_UPPERCASE,
        }
    }
}

/// Window title widget that displays the focused window's title.
pub struct WindowTitleWidget {
    /// Shared base widget container.
    base: BaseWidget,
    /// Callback ID for WindowTitleService, used to disconnect on drop.
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
    /// used to filter window title updates to only show windows on this monitor.
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

        // Clone output_id for debug log (the original moves into the closure)
        let output_id_for_log = output_id.clone();

        // Connect to window title service.
        // The callback owns clones of the GTK widgets and config.
        // Each widget remembers its last state - we only update when a window
        // on THIS monitor gains focus, otherwise we keep showing the last value.
        let window_title_callback_id = WindowTitleService::global().connect(move |snapshot| {
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
        WindowTitleService::global().disconnect(self.window_title_callback_id);
    }
}

/// Update the widget with new window info.
fn update_window_title(
    title_label: &WindowTitleLabel,
    icon_widgets: Option<&(Image, GtkBox)>,
    base_widget: &GtkBox,
    config: &WindowTitleConfig,
    app_name_cache: &Rc<RefCell<HashMap<String, String>>>,
    snapshot: &WindowTitleSnapshot,
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
fn update_icon(icon: &Image, icon_root: &GtkBox, snapshot: &WindowTitleSnapshot) -> bool {
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
        assert_eq!(config.empty_text, "");
        assert_eq!(config.template, "{display}");
        assert!(config.show_app_fallback);
        assert_eq!(config.max_chars, 0);
        assert!(config.show_icon);
        assert_eq!(config.icon_size, None);
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
        options.insert("icon_size".to_string(), Value::Integer(24));
        options.insert("uppercase".to_string(), Value::Boolean(true));
        let entry = make_widget_entry("window_title", options);
        let config = WindowTitleConfig::from_entry(&entry);
        assert_eq!(config.empty_text, "No window");
        assert_eq!(config.template, "{app}: {title}");
        assert_eq!(config.max_chars, 50);
        assert_eq!(config.icon_size, Some(24));
        assert!(config.uppercase);
    }

    #[test]
    fn test_window_title_config_icon_size_min_clamp() {
        let mut options = HashMap::new();
        options.insert("icon_size".to_string(), Value::Integer(2));

        let entry = make_widget_entry("window_title", options);
        let config = WindowTitleConfig::from_entry(&entry);

        assert_eq!(config.icon_size, Some(8));
    }

    #[test]
    fn test_titlecase() {
        assert_eq!(titlecase("firefox"), "Firefox");
        assert_eq!(titlecase("FIREFOX"), "FIREFOX");
        assert_eq!(titlecase(""), "");
        assert_eq!(titlecase("a"), "A");
    }

    #[test]
    fn test_window_title_icon_size_uses_same_default_for_both_orientations() {
        // Even bar size — even pixmap/icon stays even.
        assert_eq!(
            window_title_icon_size_from_values(false, None, 20, 26, 32),
            20
        );
        assert_eq!(
            window_title_icon_size_from_values(true, None, 20, 26, 32),
            20
        );
        assert_eq!(
            window_title_icon_size_from_values(true, None, 24, 26, 32),
            24
        );
    }

    #[test]
    fn test_window_title_icon_size_matches_bar_parity() {
        // Odd bar size — icon parity flipped to odd so centering lands on
        // a whole pixel rather than a half-pixel.
        assert_eq!(
            window_title_icon_size_from_values(false, None, 20, 26, 33),
            19
        );
        assert_eq!(
            window_title_icon_size_from_values(true, None, 20, 26, 33),
            19
        );
    }

    #[test]
    fn test_window_title_icon_size_override_applies_to_both_orientations() {
        assert_eq!(
            window_title_icon_size_from_values(false, Some(18), 20, 26, 32),
            18
        );
        assert_eq!(
            window_title_icon_size_from_values(true, Some(18), 20, 26, 32),
            18
        );
    }

    #[test]
    fn test_window_title_icon_size_override_caps_to_widget_height() {
        assert_eq!(
            window_title_icon_size_from_values(false, Some(40), 20, 26, 32),
            26
        );
        assert_eq!(
            window_title_icon_size_from_values(true, Some(40), 20, 26, 32),
            26
        );
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
        // Bare "-" is not a delimiter, so it's treated as a segment.
        // Splits on "|" → ["-", "-"], dedup → "-"
        assert_eq!(clean_title("- | -", "Firefox"), "-");
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
        // Multiple segments, first matches app — original delimiter preserved
        let cleaned = clean_title("Firefox — Tab 1 — mozilla.org", "Firefox");
        assert_eq!(cleaned, "Tab 1 \u{2014} mozilla.org");

        // Multiple segments, middle matches app — left delimiter preserved
        let cleaned_mid = clean_title("Tab 1 — Firefox — mozilla.org", "Firefox");
        assert_eq!(cleaned_mid, "Tab 1 \u{2014} mozilla.org");

        // Multiple segments, last matches app — original delimiter preserved
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

    #[test]
    fn test_clean_title_preserves_hyphenated_words() {
        // Bare hyphens inside words should NOT be treated as delimiters
        assert_eq!(clean_title("A-B", "SomeApp"), "A-B");
        assert_eq!(clean_title("my-project", "SomeApp"), "my-project");
        assert_eq!(clean_title("v1-beta-2", "SomeApp"), "v1-beta-2");

        // Hyphenated word with app name removal
        assert_eq!(clean_title("A-B — Firefox", "Firefox"), "A-B");
        assert_eq!(
            clean_title("my-project — Visual Studio Code", "Code"),
            "my-project"
        );
    }

    #[test]
    fn test_clean_title_spaced_hyphen_delimiter() {
        // " - " (space-hyphen-space) should still work as a delimiter
        assert_eq!(clean_title("Firefox - Some Page", "Firefox"), "Some Page");
        assert_eq!(
            clean_title("some-file.rs - VS Code", "Code"),
            "some-file.rs"
        );
        assert_eq!(clean_title("A-B - Firefox", "Firefox"), "A-B");
    }

    #[test]
    fn test_clean_title_preserves_original_delimiters() {
        // Pipe delimiter should be preserved, not replaced with em-dash
        assert_eq!(
            clean_title("Firefox | Tab 1 | mozilla.org", "Firefox"),
            "Tab 1 | mozilla.org"
        );

        // Mixed delimiters should each be preserved
        assert_eq!(
            clean_title("Firefox \u{2014} Tab 1 | mozilla.org", "Firefox"),
            "Tab 1 | mozilla.org"
        );

        // When removing a middle segment, the left delimiter is used
        assert_eq!(
            clean_title("Tab 1 | Firefox \u{2014} mozilla.org", "Firefox"),
            "Tab 1 | mozilla.org"
        );

        // Bullet delimiter preserved
        assert_eq!(
            clean_title("Tab 1 \u{2022} mozilla.org", ""),
            "Tab 1 \u{2022} mozilla.org"
        );
    }

    #[test]
    fn test_clean_title_all_segments_removed() {
        // Single segment that matches app
        assert_eq!(clean_title("Firefox", "Firefox"), "");

        // All segments match app (duplicates collapsed then removed)
        assert_eq!(clean_title("Firefox \u{2014} Firefox", "Firefox"), "");
    }

    #[test]
    fn test_clean_title_consecutive_removals() {
        // First two consecutive segments removed, third kept
        assert_eq!(
            clean_title("Firefox \u{2014} Firefox \u{2014} Page", "Firefox"),
            "Page"
        );
    }

    #[test]
    fn test_clean_title_no_matches_full_preservation() {
        // No segments match — title returned verbatim with original delimiters
        assert_eq!(
            clean_title("A \u{2014} B \u{2014} C", "SomeApp"),
            "A \u{2014} B \u{2014} C"
        );

        assert_eq!(
            clean_title("Tab 1 | Page 2 \u{2022} Info", "SomeApp"),
            "Tab 1 | Page 2 \u{2022} Info"
        );
    }

    #[test]
    fn test_clean_title_no_whitespace_around_delimiter() {
        // Delimiters without surrounding spaces should still work
        assert_eq!(clean_title("Firefox|Page", "Firefox"), "Page");
        assert_eq!(clean_title("A|B|C", "SomeApp"), "A|B|C");
    }

    #[test]
    fn test_clean_title_delimiter_at_boundaries() {
        // Leading delimiter — empty first segment is skipped
        assert_eq!(clean_title("\u{2014} Firefox", "Firefox"), "");
        assert_eq!(clean_title("\u{2014} Page", "Firefox"), "Page");

        // Trailing delimiter — empty last segment is skipped
        assert_eq!(clean_title("Firefox \u{2014}", "Firefox"), "");
        assert_eq!(clean_title("Page \u{2014}", "Firefox"), "Page");
    }
}
