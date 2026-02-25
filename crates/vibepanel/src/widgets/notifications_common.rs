//! Common utilities shared between notification widget modules.
//!
//! This module contains constants and helper functions used by both
//! notifications_toast.rs and notifications_popover.rs.

use gtk4::Image;

use crate::services::icons::get_app_icon_name;
use crate::services::notification::{Notification, NotificationImage};
use std::time::{SystemTime, UNIX_EPOCH};

/// Toast display duration in ms
pub const TOAST_TIMEOUT_MS: u32 = 5000;
/// Critical notifications don't auto-dismiss
pub const TOAST_TIMEOUT_CRITICAL_MS: u32 = 0;

/// Estimated height per toast (including padding/margins) for stack positioning
pub const TOAST_ESTIMATED_HEIGHT: i32 = 85;
pub const TOAST_GAP: i32 = 4;
pub const TOAST_BAR_MARGIN: i32 = 10;
pub const TOAST_MARGIN_RIGHT: i32 = 10;

/// Popover dimensions
pub const POPOVER_WIDTH: i32 = 400;
pub const POPOVER_ROW_HEIGHT: i32 = 100;
pub const POPOVER_MAX_VISIBLE_ROWS: i32 = 3;

/// Threshold for body text length before we show the expand button.
/// Bodies shorter than this are shown in full without expand/collapse UI.
pub const BODY_TRUNCATE_THRESHOLD: usize = 80;

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
pub fn sanitize_body_markup(body: &str) -> String {
    let mut result = String::with_capacity(body.len());
    let mut chars = body.char_indices().peekable();
    let mut open_tags: Vec<String> = Vec::new();

    while let Some((i, c)) = chars.next() {
        match c {
            '&' => {
                // Check if this is an existing XML entity - preserve it
                if let Some(entity) = try_parse_entity(&body[i..]) {
                    result.push_str(entity);
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
        "br" => {
            // Convert <br> to space
            Some((" ".to_string(), full_len, TagBalance::None))
        }
        "img" => {
            // Strip <img> tags entirely
            Some((String::new(), full_len, TagBalance::None))
        }
        _ => None, // Not an allowed tag
    }
}

/// Create an Image widget for a notification, preferring avatar data
/// from image-data/image-path hints when available.
pub fn create_notification_image_widget(notification: &Notification) -> Image {
    // Fixed size for notification avatars/icons (larger than theme default)
    const NOTIFICATION_ICON_SIZE: i32 = 48;

    // Try raw image-data first (e.g. chat avatar from Telegram)
    if let Some(ref img) = notification.image_data
        && let Some(texture) = notification_image_to_texture(img)
    {
        let image = Image::from_paintable(Some(&texture));
        image.set_pixel_size(NOTIFICATION_ICON_SIZE);
        return image;
    }

    // Note: image-path can be either an actual file path OR an icon theme name
    if let Some(ref path) = notification.image_path {
        let image = if let Some(file_path) = path.strip_prefix("file://") {
            // file:// URI - load from filesystem
            Image::from_file(file_path)
        } else if path.starts_with('/') {
            // Absolute path - load from filesystem
            Image::from_file(path)
        } else {
            // Icon theme name - use icon theme lookup
            Image::from_icon_name(path)
        };

        image.set_pixel_size(NOTIFICATION_ICON_SIZE);
        return image;
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
fn create_notification_icon(app_icon: &str, app_name: &str, desktop_entry: Option<&str>) -> Image {
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
        let icon = Image::from_file(file_path);
        icon.set_pixel_size(NOTIFICATION_ICON_SIZE);
        return icon;
    }

    // Handle absolute file paths
    if icon_name.starts_with('/') {
        let icon = Image::from_file(&icon_name);
        icon.set_pixel_size(NOTIFICATION_ICON_SIZE);
        return icon;
    }

    // It's an icon theme name
    let icon = Image::from_icon_name(&icon_name);
    icon.set_pixel_size(NOTIFICATION_ICON_SIZE);
    icon
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_plain_text() {
        assert_eq!(sanitize_body_markup("Hello World"), "Hello World");
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
        assert_eq!(sanitize_body_markup("Line 1<br>Line 2"), "Line 1 Line 2");
        assert_eq!(sanitize_body_markup("Line 1<br/>Line 2"), "Line 1 Line 2");
        assert_eq!(sanitize_body_markup("Line 1<br />Line 2"), "Line 1 Line 2");
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
        assert_eq!(sanitize_body_markup("<BR>"), " ");
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
