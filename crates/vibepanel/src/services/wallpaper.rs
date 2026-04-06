//! Wallpaper detection and Material You color extraction.
//!
//! Handles IPC with wallpaper daemons (hyprpaper) and extracts a
//! `material_colors::theme::Theme` from a wallpaper image for use by
//! the theming system in `vibepanel-core`.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use material_colors::color::Argb;
use material_colors::dynamic_color::Variant;
use material_colors::quantize::{Quantizer, QuantizerCelebi};
use material_colors::score::Score;
use material_colors::theme::ThemeBuilder;
use tracing::{debug, warn};

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

    let resized = img.resize(128, 128, image::imageops::FilterType::Lanczos3);
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
