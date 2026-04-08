//! Wallpaper detection and Material You color extraction.
//!
//! Handles IPC with wallpaper daemons (hyprpaper, awww/swww, wpaperd, waypaper)
//! and extracts a `material_colors::theme::Theme` from a wallpaper image for
//! use by the theming system in `vibepanel-core`.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::time::Duration;

use material_colors::color::Argb;
use material_colors::dynamic_color::Variant;
use material_colors::quantize::{Quantizer, QuantizerCelebi};
use material_colors::score::Score;
use material_colors::theme::ThemeBuilder;
use tracing::{debug, warn};
use vibepanel_core::expand_tilde;

/// Reject wallpaper images larger than this to avoid excessive memory use.
const MAX_WALLPAPER_FILE_SIZE: u64 = 50 * 1024 * 1024; // 50 MB

/// Detect the current wallpaper path from hyprpaper via its IPC socket.
///
/// Tries the instance-specific path (`hypr/$HYPRLAND_INSTANCE_SIGNATURE/.hyprpaper.sock`)
/// first, then falls back to the legacy path (`hypr/.hyprpaper.sock`).
///
/// If `monitor` is provided, returns that monitor's wallpaper. Falls back to the
/// first listed monitor if the target isn't found (e.g. unplugged, name mismatch).
pub fn detect_hyprpaper_wallpaper(monitor: Option<&str>) -> Option<String> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").ok()?;

    // Instance-specific path (Hyprland 0.40+), fall back to legacy
    let socket_path = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
        .ok()
        .map(|sig| format!("{}/hypr/{}/.hyprpaper.sock", runtime_dir, sig))
        .filter(|p| std::path::Path::new(p).exists())
        .unwrap_or_else(|| format!("{}/hypr/.hyprpaper.sock", runtime_dir));

    let mut stream = UnixStream::connect(&socket_path).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .ok();
    stream
        .set_write_timeout(Some(Duration::from_millis(500)))
        .ok();
    stream.write_all(b"listactive").ok()?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;

    // Response format: "eDP-1 = /path/to/image\nMONITOR2 = /path/to/image2\n"
    // If a target monitor was specified, try to match it first
    if let Some(target) = monitor {
        if let Some(path) = response.lines().find_map(|line| {
            let (name, path) = line.split_once('=')?;
            (name.trim() == target)
                .then(|| path.trim().to_string())
                .filter(|p| !p.is_empty())
        }) {
            debug!("Using wallpaper from target monitor '{}'", target);
            return Some(path);
        }
        debug!(
            "Target monitor '{}' not found in hyprpaper, using first available",
            target
        );
    }

    response.lines().find_map(|line| {
        let (_, path) = line.split_once('=')?;
        let path = path.trim().to_string();
        (!path.is_empty()).then_some(path)
    })
}

/// Detect the current wallpaper path from awww (or legacy swww) via CLI.
///
/// Output format: `{namespace}: {output}: {w}x{h}, scale: {s}, currently displaying: image: {path}`
fn detect_awww_wallpaper(monitor: Option<&str>) -> Option<String> {
    let output = Command::new("awww")
        .arg("query")
        .output()
        .or_else(|_| Command::new("swww").arg("query").output())
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Try to match the target monitor first
    if let Some(target) = monitor {
        for line in stdout.lines() {
            let Some(after_ns) = line.split_once(": ").map(|(_, rest)| rest) else {
                continue;
            };
            let Some((output_name, _)) = after_ns.split_once(':') else {
                continue;
            };
            if output_name.trim() == target {
                return extract_awww_image_path(line);
            }
        }
    }

    // Fall back to first line with an image path
    stdout.lines().find_map(extract_awww_image_path)
}

/// Extract the image path from a single awww/swww query output line.
fn extract_awww_image_path(line: &str) -> Option<String> {
    let marker = "currently displaying: image: ";
    let idx = line.find(marker)?;
    let path = line[idx + marker.len()..].trim();
    (!path.is_empty()).then(|| path.to_string())
}

/// Detect the current wallpaper path from wpaperd via its state symlinks.
///
/// wpaperd creates symlinks at `$XDG_STATE_HOME/wpaperd/wallpapers/<OUTPUT>`
/// pointing to the current wallpaper image.
fn detect_wpaperd_wallpaper(monitor: Option<&str>) -> Option<String> {
    let state_dir = std::env::var("XDG_STATE_HOME").unwrap_or_else(|_| {
        std::env::var("HOME")
            .map(|h| format!("{}/.local/state", h))
            .unwrap_or_default()
    });
    if state_dir.is_empty() {
        return None;
    }

    let wallpapers_dir = std::path::PathBuf::from(&state_dir).join("wpaperd/wallpapers");

    // Try the target monitor symlink first
    if let Some(target) = monitor {
        let symlink = wallpapers_dir.join(target);
        if let Ok(path) = std::fs::read_link(&symlink) {
            return Some(path.to_string_lossy().into_owned());
        }
    }

    // Fall back to first symlink in the directory
    let entries = std::fs::read_dir(&wallpapers_dir).ok()?;
    for entry in entries.flatten() {
        if let Ok(path) = std::fs::read_link(entry.path()) {
            return Some(path.to_string_lossy().into_owned());
        }
    }
    None
}

/// Detect the current wallpaper path from waypaper's INI config.
///
/// Parses `wallpaper = ` under `[Settings]` in `~/.config/waypaper/config.ini`.
/// Handles per-monitor parallel lists and `~` path expansion.
fn detect_waypaper_wallpaper(monitor: Option<&str>) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let config_path =
        std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!("{}/.config", home));
    let ini_path = std::path::PathBuf::from(&config_path).join("waypaper/config.ini");

    let content = std::fs::read_to_string(&ini_path).ok()?;

    let mut in_settings = false;
    let mut wallpaper = None;
    let mut monitors_line = None;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_settings = trimmed.eq_ignore_ascii_case("[settings]");
            continue;
        }
        if !in_settings {
            continue;
        }

        if let Some((key, value)) = trimmed.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            match key {
                "wallpaper" => wallpaper = Some(value.to_string()),
                "monitors" => monitors_line = Some(value.to_string()),
                _ => {}
            }
        }
    }

    let wallpaper = wallpaper?;

    // waypaper supports per-monitor parallel lists: monitors = eDP-1,DP-1 / wallpaper = path1,path2
    if let Some(target) = monitor
        && let Some(ref monitors_csv) = monitors_line
    {
        let monitors: Vec<&str> = monitors_csv.split(',').map(|s| s.trim()).collect();
        let paths: Vec<&str> = wallpaper.split(',').map(|s| s.trim()).collect();
        if let Some(idx) = monitors.iter().position(|m| *m == target)
            && let Some(path) = paths.get(idx)
        {
            return Some(expand_tilde(path, &home));
        }
    }

    // Single wallpaper or no monitor match — use the first (or only) path
    let first_path = wallpaper.split(',').next()?.trim();
    Some(expand_tilde(first_path, &home))
}

/// Detect the current wallpaper from any supported daemon.
///
/// Cascade order: hyprpaper → wpaperd → waypaper → awww/swww.
/// Lightweight checks (socket, filesystem) run first; subprocess-based
/// detection (awww/swww CLI) is last to avoid unnecessary fork+exec.
pub fn detect_wallpaper(monitor: Option<&str>) -> Option<String> {
    if let Some(path) = detect_hyprpaper_wallpaper(monitor) {
        debug!("Wallpaper detected via hyprpaper: {}", path);
        return Some(path);
    }
    if let Some(path) = detect_wpaperd_wallpaper(monitor) {
        debug!("Wallpaper detected via wpaperd: {}", path);
        return Some(path);
    }
    if let Some(path) = detect_waypaper_wallpaper(monitor) {
        debug!("Wallpaper detected via waypaper: {}", path);
        return Some(path);
    }
    if let Some(path) = detect_awww_wallpaper(monitor) {
        debug!("Wallpaper detected via awww/swww: {}", path);
        return Some(path);
    }

    debug!("No wallpaper daemon detected");
    None
}

/// Rebuild a Material You theme from a previously extracted source color.
///
/// This is cheap (pure math, no I/O) and used when only the light/dark preference
/// changes without the wallpaper itself changing.
pub fn theme_from_source_color(source: Argb) -> material_colors::theme::Theme {
    ThemeBuilder::with_source(source)
        .variant(Variant::Content)
        .build()
}

/// Extract a Material You theme from a wallpaper image.
///
/// Returns the full `Theme` (with light/dark schemes, tonal palettes, and source color)
/// using the `Content` variant (same as matugen's default), or `None` on failure.
pub fn extract_theme_from_image(path: &str) -> Option<material_colors::theme::Theme> {
    let file_size = std::fs::metadata(path)
        .inspect_err(|e| warn!("Failed to stat wallpaper '{}': {}", path, e))
        .ok()?
        .len();
    if file_size > MAX_WALLPAPER_FILE_SIZE {
        warn!(
            "Wallpaper '{}' too large ({} MB, max {} MB)",
            path,
            file_size / (1024 * 1024),
            MAX_WALLPAPER_FILE_SIZE / (1024 * 1024)
        );
        return None;
    }

    let image_bytes = std::fs::read(path)
        .inspect_err(|e| warn!("Failed to read wallpaper image '{}': {}", path, e))
        .ok()?;

    let img = image::load_from_memory(&image_bytes)
        .inspect_err(|e| warn!("Failed to decode wallpaper image '{}': {}", path, e))
        .ok()?;

    let resized = img.resize(128, 128, image::imageops::FilterType::Triangle);
    let rgba = resized.to_rgba8();

    let pixels: Vec<Argb> = rgba
        .pixels()
        .map(|p| Argb::new(p[3], p[0], p[1], p[2]))
        .collect();

    let result = QuantizerCelebi::quantize(&pixels, 128);
    let ranked = Score::score(&result.color_to_count, None, None, None);
    let source_color = *ranked.first()?;

    Some(
        ThemeBuilder::with_source(source_color)
            .variant(Variant::Content)
            .build(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_awww_image_path_standard() {
        let line =
            "default: eDP-1: 1920x1080, scale: 1, currently displaying: image: /home/user/wall.png";
        assert_eq!(
            extract_awww_image_path(line),
            Some("/home/user/wall.png".to_string())
        );
    }

    #[test]
    fn test_extract_awww_image_path_spaces_in_path() {
        let line = "default: DP-2: 2560x1440, scale: 1, currently displaying: image: /home/user/My Wallpapers/wall.jpg";
        assert_eq!(
            extract_awww_image_path(line),
            Some("/home/user/My Wallpapers/wall.jpg".to_string())
        );
    }

    #[test]
    fn test_extract_awww_image_path_no_marker() {
        assert_eq!(extract_awww_image_path("some random line"), None);
    }

    #[test]
    fn test_extract_awww_image_path_empty_after_marker() {
        let line = "default: eDP-1: 1920x1080, scale: 1, currently displaying: image:   ";
        assert_eq!(extract_awww_image_path(line), None);
    }
}
