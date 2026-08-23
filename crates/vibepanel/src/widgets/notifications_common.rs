//! Common utilities shared between notification widget modules.
//!
//! This module contains constants and helper functions used by both
//! notifications_toast.rs and notifications_popover.rs.

use gtk4::gdk;
use gtk4::gdk_pixbuf::Pixbuf;
use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, Button, Image, Label, Orientation, PolicyType, ScrolledWindow, Widget, pango,
};

use crate::services::battery::normalize_battery_icon_name;
use crate::services::icons::{IconsService, get_app_icon_name};
use crate::services::notification::{Notification, NotificationImage};
use crate::styles::{button, color, notification as notif};
use std::cell::Cell;
use std::time::{SystemTime, UNIX_EPOCH};

/// Toast display duration in ms
pub const TOAST_TIMEOUT_MS: u32 = 5000;
/// Critical notifications don't auto-dismiss
pub const TOAST_TIMEOUT_CRITICAL_MS: u32 = 0;

/// Estimated height per toast (including padding/margins) for stack positioning
pub const TOAST_ESTIMATED_HEIGHT: i32 = 85;
pub const TOAST_GAP: i32 = 4;
pub const TOAST_EDGE_MARGIN: i32 = 10;
pub const TOAST_SIDE_MARGIN: i32 = 10;

/// Popover dimensions
pub const POPOVER_WIDTH: i32 = 400;

/// Shadow margin for freely-floating surfaces (toast, OSD).
/// Applied uniformly on all four sides so the CSS `box-shadow` is not clipped
/// at the layer-shell surface boundary.
pub const SURFACE_SHADOW_MARGIN: i32 = 8;

const PREVIEW_LINES: usize = 2;

/// Split sanitized markup into standalone physical lines. Pango applies label
/// line limits per paragraph, so separate labels enforce one shared preview limit.
fn split_markup_lines(markup: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    // (tag name, opening tag with attributes)
    let mut open: Vec<(String, String)> = Vec::new();
    let mut idx = 0;

    while let Some(ch) = markup[idx..].chars().next() {
        match ch {
            '\n' => {
                for (name, _) in open.iter().rev() {
                    current.push_str(&format!("</{}>", name));
                }
                lines.push(std::mem::take(&mut current));
                for (_, opening) in &open {
                    current.push_str(opening);
                }
                idx += ch.len_utf8();
            }
            '<' => {
                let rest = &markup[idx..];
                let Some(end) = rest.find('>') else {
                    current.push(ch);
                    idx += ch.len_utf8();
                    continue;
                };
                let tag = &rest[..=end];
                let inner = &rest[1..end];

                if let Some(name) = inner.strip_prefix('/') {
                    let name = name.trim().to_ascii_lowercase();
                    if let Some(pos) = open.iter().rposition(|(open_name, _)| *open_name == name) {
                        open.remove(pos);
                    }
                } else {
                    let name_end = inner
                        .find(|c: char| !c.is_ascii_alphabetic())
                        .unwrap_or(inner.len());
                    open.push((inner[..name_end].to_ascii_lowercase(), tag.to_string()));
                }

                current.push_str(tag);
                idx += end + 1;
            }
            _ => {
                current.push(ch);
                idx += ch.len_utf8();
            }
        }
    }

    lines.push(current);
    lines
}

pub struct NotificationBody {
    pub root: GtkBox,
    /// Visible when structural or measured layout overflow requires expansion.
    pub expand_button: Button,
}

/// Open label links without GTK's default URI launcher, which can violate
/// layer-shell focus constraints on Wayland.
fn connect_notification_link(label: &Label, after_open: impl Fn() + 'static) {
    label.connect_activate_link(move |_, uri| {
        let command = format!("xdg-open '{}'", uri.replace("'", "'\\''"));
        let _ = gtk4::glib::spawn_command_line_async(&command);
        after_open();
        gtk4::glib::Propagation::Stop
    });
}

fn body_label(css_class: &str) -> Label {
    let label = Label::new(None);
    label.add_css_class(css_class);
    label.add_css_class(color::MUTED);
    label.set_xalign(0.0);
    label
}

/// Build a notification body showing its first two lines until expanded.
///
/// The preview preserves text, formatting, and links. With
/// `expanded_max_height`, the expanded body scrolls beyond that height instead
/// of growing.
pub fn create_notification_body(
    body: &str,
    label_css_class: &str,
    expanded_max_height: Option<i32>,
    after_link_open: impl Fn() + Clone + 'static,
) -> NotificationBody {
    let markup = sanitize_body_markup(body);

    let root = GtkBox::new(Orientation::Vertical, 0);
    root.add_css_class(notif::BODY_CONTAINER);

    let full_label = body_label(label_css_class);
    full_label.set_markup(&markup);
    full_label.set_wrap(true);
    full_label.set_wrap_mode(pango::WrapMode::WordChar);
    connect_notification_link(&full_label, after_link_open.clone());

    let preview = GtkBox::new(Orientation::Vertical, 0);
    let lines = split_markup_lines(markup.trim_end());
    let structural_overflow = lines.len() > PREVIEW_LINES;
    let mut preview_labels = Vec::new();

    for (index, line) in lines.iter().take(PREVIEW_LINES).enumerate() {
        let line_label = body_label(label_css_class);
        line_label.set_markup(if line.is_empty() { "\u{2060}" } else { line });
        line_label.set_ellipsize(pango::EllipsizeMode::End);

        if index == 0 && !line.is_empty() {
            line_label.set_wrap(true);
            line_label.set_wrap_mode(pango::WrapMode::WordChar);
            line_label.set_lines(PREVIEW_LINES as i32);
        } else {
            line_label.set_single_line_mode(true);
        }
        connect_notification_link(&line_label, after_link_open.clone());

        preview.append(&line_label);
        preview_labels.push(line_label);
    }

    let mut scroll_limit = None;
    let expanded: Widget = match expanded_max_height {
        Some(max_height) => {
            // Keep short expanded bodies outside the scroller. GTK can allocate
            // max-content dead space for wrapped labels, pushing actions down.
            let container = GtkBox::new(Orientation::Vertical, 0);
            container.append(&full_label);

            let scroll = ScrolledWindow::new();
            scroll.set_policy(PolicyType::Never, PolicyType::Automatic);
            scroll.set_propagate_natural_height(false);
            scroll.add_css_class(notif::SCROLL);
            scroll.set_height_request(max_height);
            scroll_limit = Some((container.clone(), scroll, full_label.clone(), max_height));
            container.upcast()
        }
        None => full_label.clone().upcast(),
    };
    expanded.set_visible(false);

    root.append(&preview);
    root.append(&expanded);

    let expand_button = crate::widgets::base::vp_button_with_label("Show more");
    expand_button.add_css_class(notif::ACTION_BTN);
    expand_button.add_css_class(button::GHOST);
    expand_button.set_visible(structural_overflow);

    let mapped_labels = preview_labels.clone();
    let mapped_button = expand_button.downgrade();
    preview.connect_map(move |_| {
        let labels = mapped_labels.clone();
        let button = mapped_button.clone();
        gtk4::glib::idle_add_local_once(move || {
            let first_uses_budget =
                labels.len() > 1 && labels[0].layout().line_count() >= PREVIEW_LINES as i32;
            if first_uses_budget && let Some(second) = labels.get(1) {
                second.set_visible(false);
            }
            if (first_uses_budget || labels.iter().any(|label| label.layout().is_ellipsized()))
                && let Some(button) = button.upgrade()
            {
                button.set_visible(true);
            }
        });
    });

    let is_expanded = Cell::new(false);
    expand_button.connect_clicked(move |btn| {
        let expanded_now = !is_expanded.get();
        is_expanded.set(expanded_now);

        if let Some((container, scroll, label, max_height)) = &scroll_limit {
            if expanded_now && scroll.child().is_none() {
                // The unconstrained label layout does not include wrapping at the
                // toast width, so measure it using the allocated preview width.
                let layout = label.layout().copy();
                layout.set_width(preview.width() * pango::SCALE);
                let (_, height) = layout.pixel_size();

                if height > *max_height {
                    container.remove(label);
                    scroll.set_child(Some(label));
                    container.append(scroll);
                }
            } else if !expanded_now && scroll.child().is_some() {
                scroll.vadjustment().set_value(0.0);
            }
        }

        preview.set_visible(!expanded_now);
        expanded.set_visible(expanded_now);
        btn.set_label(if expanded_now {
            "Show less"
        } else {
            "Show more"
        });
    });

    NotificationBody {
        root,
        expand_button,
    }
}

/// Format a timestamp as a human-readable relative time.
pub fn format_timestamp(timestamp: f64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let diff = now - timestamp;

    if diff < 60.0 {
        "Just now".to_string()
    } else if diff < 3600.0 {
        let mins = (diff / 60.0) as i32;
        format!("{}m ago", mins)
    } else if diff < 86400.0 {
        let hours = (diff / 3600.0) as i32;
        format!("{}h ago", hours)
    } else {
        let days = (diff / 86400.0) as i32;
        format!("{}d ago", days)
    }
}

#[derive(Debug, PartialEq)]
enum TagBalance {
    Open(String),
    Close(String),
    None,
}

/// Sanitize notification body text for Pango markup rendering.
/// Returns markup safe for use with `Label::set_markup()`.
fn sanitize_body_markup(body: &str) -> String {
    let mut result = String::with_capacity(body.len());
    let mut chars = body.char_indices().peekable();
    let mut open_tags: Vec<String> = Vec::new();

    while let Some((i, c)) = chars.next() {
        match c {
            '&' => {
                // Check if this is an existing XML entity - preserve it
                if let Some(entity) = try_parse_entity(&body[i..]) {
                    if let Some(separator) = numeric_line_separator(entity) {
                        result.push(separator);
                    } else {
                        result.push_str(entity);
                    }
                    // Skip past the entity
                    for _ in 0..entity.len() - 1 {
                        chars.next();
                    }
                } else {
                    result.push_str("&amp;");
                }
            }
            '<' => {
                // Try to parse as an allowed tag
                if let Some((tag_output, skip_len, balance)) = try_parse_tag(&body[i..]) {
                    // Handle balancing
                    match balance {
                        TagBalance::Open(tag) => {
                            result.push_str(&tag_output);
                            open_tags.push(tag);
                        }
                        TagBalance::Close(tag) => {
                            // Check if this closes the most recent tag
                            if let Some(last) = open_tags.last() {
                                if last == &tag {
                                    result.push_str(&tag_output);
                                    open_tags.pop();
                                } else {
                                    // Mismatch!
                                    // Check if 'tag' is open deeper in the stack.
                                    if let Some(pos) = open_tags.iter().rposition(|t| t == &tag) {
                                        // Close intermediate tags
                                        while open_tags.len() > pos + 1 {
                                            if let Some(popped) = open_tags.pop() {
                                                result.push_str(&format!("</{}>", popped));
                                            }
                                        }
                                        // Now we can close the target tag
                                        open_tags.pop();
                                        result.push_str(&tag_output);
                                    } else {
                                        // Tag not open. Ignore this closing tag.
                                    }
                                }
                            } else {
                                // Stack empty, ignore closing tag
                            }
                        }
                        TagBalance::None => {
                            result.push_str(&tag_output);
                        }
                    }

                    // Skip past the tag (minus the '<' we already consumed)
                    for _ in 0..skip_len - 1 {
                        chars.next();
                    }
                } else {
                    result.push_str("&lt;");
                }
            }
            '>' => result.push_str("&gt;"),
            _ => result.push(c),
        }
    }

    // Close any remaining open tags
    while let Some(tag) = open_tags.pop() {
        result.push_str(&format!("</{}>", tag));
    }

    result
        .replace("\r\n", "\n")
        .replace(['\r', '\u{2028}', '\u{2029}'], "\n")
}

/// Try to parse an XML entity at the start of `s`.
/// Returns the entity string if valid, None otherwise.
fn try_parse_entity(s: &str) -> Option<&str> {
    if !s.starts_with('&') {
        return None;
    }

    // Find the semicolon
    let end = s.find(';')?;
    if end > 10 {
        // Entity too long, probably not valid
        return None;
    }

    let entity = &s[..=end];
    let name = &s[1..end];

    // Check for valid entity names
    let valid = matches!(name, "amp" | "lt" | "gt" | "quot" | "apos")
        || (name.starts_with('#')
            && name.len() > 1
            && (name[1..].chars().all(|c| c.is_ascii_digit())
                || (name.starts_with("#x")
                    && name.len() > 2
                    && name[2..].chars().all(|c| c.is_ascii_hexdigit()))));

    if valid { Some(entity) } else { None }
}

fn numeric_line_separator(entity: &str) -> Option<char> {
    let value = entity.strip_prefix("&#")?.strip_suffix(';')?;
    let value = if let Some(hex) = value.strip_prefix('x') {
        u32::from_str_radix(hex, 16).ok()
    } else {
        value.parse::<u32>().ok()
    }?;

    matches!(value, 0x0a | 0x0d | 0x2028 | 0x2029)
        .then(|| char::from_u32(value))
        .flatten()
}

/// Try to parse an allowed HTML tag at the start of `s`.
/// Returns (output_string, bytes_consumed, TagBalance) if valid, None otherwise.
fn try_parse_tag(s: &str) -> Option<(String, usize, TagBalance)> {
    if !s.starts_with('<') {
        return None;
    }

    // Find the closing >
    let end = s.find('>')?;
    let tag_content = &s[1..end]; // Content between < and >
    let full_len = end + 1;

    // Parse the tag name (may start with /)
    let (is_closing, tag_rest) = if let Some(rest) = tag_content.strip_prefix('/') {
        (true, rest.trim())
    } else {
        (false, tag_content.trim())
    };

    // Extract tag name (letters only, stop at space or end)
    let tag_name_end = tag_rest
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(tag_rest.len());
    let tag_name = &tag_rest[..tag_name_end];
    let tag_name_lower = tag_name.to_ascii_lowercase();

    match tag_name_lower.as_str() {
        "b" | "i" | "u" => {
            // Simple formatting tags - normalize to lowercase
            let output = if is_closing {
                format!("</{}>", tag_name_lower)
            } else {
                format!("<{}>", tag_name_lower)
            };
            let balance = if is_closing {
                TagBalance::Close(tag_name_lower)
            } else {
                TagBalance::Open(tag_name_lower)
            };
            Some((output, full_len, balance))
        }
        "a" => {
            if is_closing {
                Some((
                    "</a>".to_string(),
                    full_len,
                    TagBalance::Close("a".to_string()),
                ))
            } else {
                // Preserve <a> with its attributes (href, etc.)
                let attrs = &tag_rest[tag_name_end..];
                Some((
                    format!("<a{}>", attrs),
                    full_len,
                    TagBalance::Open("a".to_string()),
                ))
            }
        }
        "br" => Some(("\n".to_string(), full_len, TagBalance::None)),
        "img" => {
            // Strip <img> tags entirely
            Some((String::new(), full_len, TagBalance::None))
        }
        _ => None, // Not an allowed tag
    }
}

/// Load an Image widget from a filesystem path, decoding scaled to `size` so we
/// never pull a multi-megapixel source (e.g. a 4K wallpaper passed as --icon)
/// through the GTK main thread at full resolution.
fn load_scaled_image_from_path(path: &str, size: i32) -> Image {
    if let Ok(pixbuf) = Pixbuf::from_file_at_scale(path, size, size, true) {
        let texture = gdk::Texture::for_pixbuf(&pixbuf);
        let image = Image::from_paintable(Some(&texture));
        image.set_pixel_size(size);
        image
    } else {
        // Fall back to GTK's own loader; matches original behavior on failure.
        let image = Image::from_file(path);
        image.set_pixel_size(size);
        image
    }
}

/// Create an Image widget for a notification, preferring avatar data
/// from image-data/image-path hints when available.
pub fn create_notification_image_widget(notification: &Notification) -> Widget {
    // Fixed size for notification avatars/icons (larger than theme default)
    const NOTIFICATION_ICON_SIZE: i32 = 48;

    // Try raw image-data first (e.g. chat avatar from Telegram)
    if let Some(ref img) = notification.image_data
        && let Some(texture) = notification_image_to_texture(img)
    {
        let image = Image::from_paintable(Some(&texture));
        image.set_pixel_size(NOTIFICATION_ICON_SIZE);
        return image.upcast();
    }

    // Note: image-path can be either an actual file path OR an icon theme name
    if let Some(ref path) = notification.image_path {
        if let Some(file_path) = path.strip_prefix("file://") {
            // file:// URI - load from filesystem
            return load_scaled_image_from_path(file_path, NOTIFICATION_ICON_SIZE).upcast();
        } else if path.starts_with('/') {
            // Absolute path - load from filesystem
            return load_scaled_image_from_path(path, NOTIFICATION_ICON_SIZE).upcast();
        } else {
            // Icon theme name - use icon theme lookup
            let image = Image::from_icon_name(path);
            image.set_pixel_size(NOTIFICATION_ICON_SIZE);
            return image.upcast();
        }
    }

    // Finally, fall back to icon theme / desktop entry logic
    create_notification_icon(
        &notification.app_icon,
        &notification.app_name,
        notification.desktop_entry.as_deref(),
    )
}

/// Convert raw NotificationImage data into a gdk Texture.
fn notification_image_to_texture(img: &NotificationImage) -> Option<gtk4::gdk::Texture> {
    use gtk4::gdk;
    use gtk4::glib::Bytes;
    use gtk4::prelude::*;

    if img.width <= 0 || img.height <= 0 || img.data.is_empty() {
        return None;
    }

    // The freedesktop notification spec uses RGBA format (not ARGB like StatusNotifierItem).
    // Pass the raw bytes directly without conversion.
    let bytes = Bytes::from(&img.data[..]);

    let format = if img.has_alpha && img.channels == 4 {
        gdk::MemoryFormat::R8g8b8a8
    } else {
        // 3-channel RGB (rare, but handle it)
        gdk::MemoryFormat::R8g8b8
    };

    let texture = gdk::MemoryTexture::new(
        img.width,
        img.height,
        format,
        &bytes,
        img.rowstride as usize,
    );

    Some(texture.upcast())
}

/// Create an icon widget for a notification.
///
/// Resolution precedence:
///   1. app_icon (if non-empty)
///   2. desktop_entry hint (e.g. "org.telegram.desktop")
///   3. app_name via desktop entry lookup
///   4. generic fallback icon
fn create_notification_icon(app_icon: &str, app_name: &str, desktop_entry: Option<&str>) -> Widget {
    // Fixed size for notification icons (larger than theme default)
    const NOTIFICATION_ICON_SIZE: i32 = 48;

    let fallback = "dialog-information-symbolic";

    // Determine which icon to use:
    // 1. If app_icon is provided (non-empty), use it
    // 2. Otherwise, try to resolve from desktop_entry via icons service
    // 3. Otherwise, try to resolve from app_name via desktop entry lookup
    // 4. Fall back to generic icon
    let icon_name = if !app_icon.is_empty() {
        app_icon.to_string()
    } else if let Some(desktop) = desktop_entry {
        let resolved = get_app_icon_name(desktop);
        if resolved.is_empty() {
            fallback.to_string()
        } else {
            resolved
        }
    } else if !app_name.is_empty() {
        let resolved = get_app_icon_name(app_name);
        if resolved.is_empty() {
            fallback.to_string()
        } else {
            resolved
        }
    } else {
        fallback.to_string()
    };

    // Handle file:// URIs
    if let Some(file_path) = icon_name.strip_prefix("file://") {
        return load_scaled_image_from_path(file_path, NOTIFICATION_ICON_SIZE).upcast();
    }

    // Handle absolute file paths
    if icon_name.starts_with('/') {
        return load_scaled_image_from_path(&icon_name, NOTIFICATION_ICON_SIZE).upcast();
    }

    if let Some(logical_icon) = notification_logical_icon_name(&icon_name) {
        let handle = IconsService::global().create_icon(logical_icon, &[]);
        let widget = handle.widget();
        widget.set_size_request(NOTIFICATION_ICON_SIZE, NOTIFICATION_ICON_SIZE);
        widget.add_css_class(notif::BATTERY_ICON);
        // Keep the handle alive so the icon can survive theme backend rebuilds
        // while the notification row/toast is still visible.
        unsafe {
            widget.set_data("vibepanel-notification-icon-handle", handle);
        }
        return widget;
    }

    // It's an icon theme name
    let icon = Image::from_icon_name(&icon_name);
    icon.set_pixel_size(NOTIFICATION_ICON_SIZE);
    icon.upcast()
}

fn notification_logical_icon_name(icon_name: &str) -> Option<&'static str> {
    normalize_battery_icon_name(icon_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_plain_text() {
        assert_eq!(sanitize_body_markup("Hello World"), "Hello World");
    }

    #[test]
    fn split_markup_lines_keeps_blank_lines_and_tag_contents() {
        assert_eq!(split_markup_lines("a\n\nb"), ["a", "", "b"]);
        assert_eq!(split_markup_lines("a\nb\n".trim_end()), ["a", "b"]);
        assert_eq!(
            split_markup_lines("<a\n href=\"https://example.com\">link</a>"),
            ["<a\n href=\"https://example.com\">link</a>"]
        );
    }

    #[test]
    fn split_markup_lines_reopens_formatting_and_links() {
        assert_eq!(
            split_markup_lines("<a href=\"https://x.test\">link\ntext</a>"),
            [
                "<a href=\"https://x.test\">link</a>",
                "<a href=\"https://x.test\">text</a>"
            ]
        );
    }

    #[test]
    fn test_notification_battery_icon_normalization() {
        assert_eq!(
            notification_logical_icon_name("battery-low-symbolic"),
            Some("battery-low")
        );
        assert_eq!(
            notification_logical_icon_name("battery-very-low"),
            Some("battery-very-low")
        );
        assert_eq!(
            notification_logical_icon_name("battery-caution-symbolic"),
            Some("battery-critical-alert")
        );
        assert_eq!(
            notification_logical_icon_name("battery-empty-symbolic"),
            Some("battery-very-low")
        );
        assert_eq!(
            notification_logical_icon_name("dialog-information-symbolic"),
            None
        );
    }

    #[test]
    fn test_sanitize_allowed_tags() {
        assert_eq!(
            sanitize_body_markup("<b>Bold</b> <i>Italic</i> <u>Underline</u>"),
            "<b>Bold</b> <i>Italic</i> <u>Underline</u>"
        );
    }

    #[test]
    fn test_sanitize_links() {
        assert_eq!(
            sanitize_body_markup(r#"<a href="https://example.com">Link</a>"#),
            r#"<a href="https://example.com">Link</a>"#
        );
    }

    #[test]
    fn test_sanitize_br() {
        assert_eq!(sanitize_body_markup("Line 1<br>Line 2"), "Line 1\nLine 2");
        assert_eq!(sanitize_body_markup("Line 1<br/>Line 2"), "Line 1\nLine 2");
        assert_eq!(sanitize_body_markup("Line 1<br />Line 2"), "Line 1\nLine 2");
    }

    #[test]
    fn test_sanitize_line_boundaries() {
        let markup =
            sanitize_body_markup("one\r\ntwo\rthree\u{2028}four\u{2029}five&#10;six&#xA;seven");
        assert_eq!(markup, "one\ntwo\nthree\nfour\nfive\nsix\nseven");
        assert_eq!(
            split_markup_lines(&sanitize_body_markup("one&#10;two&#10;three")),
            ["one", "two", "three"]
        );
    }

    #[test]
    fn test_sanitize_strip_img() {
        assert_eq!(
            sanitize_body_markup(r#"Image: <img src="test.png" alt="test"/>"#),
            "Image: "
        );
    }

    #[test]
    fn test_sanitize_escape_invalid_tags() {
        assert_eq!(
            sanitize_body_markup("<script>alert('xss')</script>"),
            "&lt;script&gt;alert('xss')&lt;/script&gt;"
        );
    }

    #[test]
    fn test_sanitize_entities() {
        // Valid entities preserved
        assert_eq!(sanitize_body_markup("Fish &amp; Chips"), "Fish &amp; Chips");
        assert_eq!(sanitize_body_markup("A &lt; B"), "A &lt; B");

        // Invalid/Bare ampersand escaped
        assert_eq!(sanitize_body_markup("A & B"), "A &amp; B");

        // Decimal entity
        assert_eq!(sanitize_body_markup("&#1234;"), "&#1234;");
        // Hex entity
        assert_eq!(sanitize_body_markup("&#x1F600;"), "&#x1F600;");
    }

    #[test]
    fn test_sanitize_malformed_tags() {
        // Unclosed tag
        assert_eq!(sanitize_body_markup("Foo <b"), "Foo &lt;b");
        // Nested unclosed - the first < fails parsing, becomes &lt;
        // The second < starts a valid <b> tag
        assert_eq!(sanitize_body_markup("<<b"), "&lt;&lt;b");
    }

    #[test]
    fn test_case_insensitive_tags() {
        assert_eq!(sanitize_body_markup("<B>BOLD</B>"), "<b>BOLD</b>");
        assert_eq!(sanitize_body_markup("<BR>"), "\n");
    }

    #[test]
    fn test_sanitize_auto_close() {
        // Unclosed <b>
        assert_eq!(sanitize_body_markup("<b>Bold"), "<b>Bold</b>");
        // Nested unclosed
        assert_eq!(
            sanitize_body_markup("<b><i>Bold Italic"),
            "<b><i>Bold Italic</i></b>"
        );
    }

    #[test]
    fn test_sanitize_nesting_fix() {
        // Bad nesting
        assert_eq!(sanitize_body_markup("<b><i>Text</b>"), "<b><i>Text</i></b>");
        // Extra closing tag
        assert_eq!(sanitize_body_markup("Text</b>"), "Text");
    }
}
