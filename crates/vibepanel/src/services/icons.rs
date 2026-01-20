//! IconsService - process-wide icon management for the vibepanel bar.
//!
//! This service handles icon rendering based on the configured icon theme.
//! It supports multiple backends:
//!
//! - **Material**: Loads the Material Symbols Rounded font from assets/,
//!   registers it with fontconfig, applies CSS, and maps logical icon names
//!   to Material Symbols glyph ligatures.
//!
//! - **GTK**: Uses GTK's icon theme system (Adwaita, Breeze, etc.) to render
//!   icons as `Gtk.Image` widgets. Logical icon names are mapped to GTK
//!   symbolic icon names.
//!
//! - **Text fallback**: When neither Material nor GTK backends are available,
//!   displays the logical icon name as plain text.
//!
//! Widgets use `IconHandle` to display and update icons without knowing
//! the underlying theme implementation. The service supports live theme
//! switching via `reconfigure()`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::{Rc, Weak};

use gtk4::gio::{AppInfo, DesktopAppInfo, prelude::*};
use gtk4::prelude::*;
use gtk4::{IconTheme, Image, Label};
use pango::prelude::FontMapExt;
use tracing::{debug, info, warn};

use crate::styles::icon;

/// Font family name for Material Symbols (must match the TTF metadata).
const MATERIAL_FONT_FAMILY: &str = "Material Symbols Rounded";

/// Relative path to the Material Symbols font file from the project root.
const MATERIAL_FONT_FILE: &str = "assets/fonts/MaterialSymbolsRounded.ttf";

/// Embedded font data - included at compile time for standalone binary distribution.
/// This allows the binary to work without requiring external font files.
const EMBEDDED_FONT_DATA: &[u8] =
    include_bytes!("../../../../assets/fonts/MaterialSymbolsRounded.ttf");

// Thread-local singleton storage for IconsService
thread_local! {
    static ICONS_INSTANCE: RefCell<Option<Rc<IconsService>>> = const { RefCell::new(None) };

    // Caches for desktop app info lookups
    static APP_ICON_NAME_CACHE: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
    static APP_DESKTOP_CACHE: RefCell<HashMap<String, Option<DesktopAppInfo>>> = RefCell::new(HashMap::new());
    static ALL_APP_INFOS: RefCell<Option<Vec<AppInfo>>> = const { RefCell::new(None) };
}

/// Register a font file directly with Pango's font map.
///
/// This uses Pango 1.56+'s `add_font_file()` API which registers fonts
/// directly with Pango, bypassing fontconfig. This is cleaner and avoids
/// font cache timing issues that occur with fontconfig registration.
///
/// Returns true if the font was successfully registered.
fn register_font_with_pango(font_path: &std::path::Path) -> bool {
    if !font_path.exists() {
        warn!(
            "Material font missing at {}; icons may render as text",
            font_path.display()
        );
        return false;
    }

    // Create a temporary label to access Pango's font map
    let temp_label = Label::new(None);
    let Some(font_map) = temp_label.pango_context().font_map() else {
        warn!("Could not get Pango font map; Material Symbols may not render");
        return false;
    };

    match font_map.add_font_file(font_path) {
        Ok(()) => {
            debug!(
                "Registered Material Symbols font with Pango: {}",
                font_path.display()
            );
            true
        }
        Err(e) => {
            warn!(
                "Pango could not register font {}: {}; icons may render as text",
                font_path.display(),
                e
            );
            false
        }
    }
}

/// Maps logical icon names to Material Symbols glyph names.
///
/// Material Symbols uses ligatures: setting the label text to "battery_full"
/// renders the battery_full glyph. This mapping converts our canonical names
/// (e.g., "battery-full") to Material's naming convention.
///
/// Battery icons (8 levels for granular display):
///   - battery-full, battery-high, battery-medium-high, battery-medium
///   - battery-medium-low, battery-low, battery-critical
///   - Plus "-charging" variants for each level
///   - battery-missing for unknown state
pub fn material_symbol_name(icon_name: &str) -> &str {
    match icon_name {
        // Battery (discharging) - 8 levels for granular display
        "battery-full" => "battery_full",
        "battery-high" => "battery_6_bar",
        "battery-medium-high" => "battery_5_bar",
        "battery-medium" => "battery_4_bar",
        "battery-medium-low" => "battery_3_bar",
        "battery-low" => "battery_2_bar",
        "battery-critical" => "battery_1_bar",
        "battery-missing" => "battery_unknown",

        // Battery (charging) - matching 8 levels
        "battery-full-charging" => "battery_charging_full",
        "battery-high-charging" => "battery_charging_90",
        "battery-medium-high-charging" => "battery_charging_80",
        "battery-medium-charging" => "battery_charging_60",
        "battery-medium-low-charging" => "battery_charging_50",
        "battery-low-charging" => "battery_charging_30",
        "battery-critical-charging" => "battery_charging_20",

        // Notifications
        "notifications" => "notifications",
        "notifications-disabled" => "notifications_off",
        "notifications-active" => "notifications_active",

        // Brightness (for OSD)
        "display-brightness-off-symbolic" => "brightness_empty",
        "display-brightness-low-symbolic" => "brightness_empty",
        "display-brightness-medium-symbolic" => "brightness_medium",
        "display-brightness-high-symbolic" => "brightness_high",
        "display-brightness-symbolic" => "brightness_medium",

        // Audio volume (for OSD and quick settings)
        "audio-volume-muted-symbolic" => "volume_off",
        "audio-volume-low-symbolic" => "volume_down",
        "audio-volume-medium-symbolic" => "volume_down",
        "audio-volume-high-symbolic" => "volume_up",
        "audio-volume-muted" => "volume_off",
        "audio-volume-low" => "volume_down",
        "audio-volume-medium" => "volume_down",
        "audio-volume-high" => "volume_up",

        // Microphone sensitivity (for quick settings mic slider)
        "microphone-sensitivity-muted-symbolic" => "mic_off",
        "microphone-sensitivity-low-symbolic" => "mic",
        "microphone-sensitivity-medium-symbolic" => "mic",
        "microphone-sensitivity-high-symbolic" => "mic",
        "audio-input-microphone-symbolic" => "mic",
        "audio-input-microphone-muted-symbolic" => "mic_off",

        // Selection indicators (for sink list, etc.)
        "object-select-symbolic" => "check",
        "radio-symbolic" => "radio_button_unchecked",
        "radio-checked-symbolic" => "radio_button_checked",

        // Wi-Fi signal strength (for quick settings network list)
        // Material Symbols wifi line: wifi_1_bar, wifi_2_bar, wifi (3 bar)
        // Note: no wifi_0_bar or wifi_4_bar, wifi_off for disabled
        "network-wireless-signal-excellent-symbolic" => "wifi",
        "network-wireless-signal-good-symbolic" => "wifi",
        "network-wireless-signal-ok-symbolic" => "wifi_2_bar",
        "network-wireless-signal-weak-symbolic" => "wifi_1_bar",
        "network-wireless-signal-none-symbolic" => "wifi_1_bar",
        "network-wireless-offline-symbolic" => "wifi_off",

        // Simplified Wi-Fi names
        "wifi-off" => "wifi_off",
        "wifi" => "wifi",

        // Bluetooth icons
        "bluetooth-symbolic" => "bluetooth",
        "bluetooth-active-symbolic" => "bluetooth_connected",
        "bluetooth-disabled-symbolic" => "bluetooth_disabled",

        // Bluetooth device type icons (from BlueZ)
        "audio-headphones" => "headphones",
        "audio-headphones-symbolic" => "headphones",
        "audio-headset" => "headset_mic",
        "audio-headset-symbolic" => "headset_mic",
        "audio-card" => "speaker",
        "audio-card-symbolic" => "speaker",
        "audio-speakers" => "speaker",
        "audio-speakers-symbolic" => "speaker",
        "input-keyboard" => "keyboard",
        "input-keyboard-symbolic" => "keyboard",
        "input-mouse" => "mouse",
        "input-mouse-symbolic" => "mouse",
        "input-gaming" => "sports_esports",
        "input-gaming-symbolic" => "sports_esports",
        "phone" => "smartphone",
        "phone-symbolic" => "smartphone",
        "computer" => "computer",
        "computer-symbolic" => "computer",

        // VPN icons
        "network-vpn" => "vpn_key",
        "network-vpn-symbolic" => "vpn_key",
        "network-vpn-acquiring-symbolic" => "vpn_key",
        "network-vpn-connected-symbolic" => "vpn_lock",
        "network-vpn-disconnected-symbolic" => "vpn_key_off",

        // Idle inhibitor / night light icons
        "night-light-symbolic" => "coffee",
        "preferences-system-time-symbolic" => "coffee",

        // UI action icons (chevrons, menus, close buttons)
        "pan-down-symbolic" => "keyboard_arrow_down",
        "pan-up-symbolic" => "keyboard_arrow_up",
        "pan-left-symbolic" => "keyboard_arrow_left",
        "pan-right-symbolic" => "keyboard_arrow_right",
        "open-menu-symbolic" => "more_vert",
        "view-more-symbolic" => "more_horiz",
        "window-close-symbolic" => "close",
        "user-trash-symbolic" => "delete",

        // Software updates
        "software-update-available" => "download",
        "software-update-urgent" => "download",

        // Power menu icons
        "system-shutdown-symbolic" => "power_settings_new",
        "system-reboot-symbolic" => "restart_alt",
        "system-suspend-symbolic" => "bedtime",
        "system-lock-screen-symbolic" => "lock",
        "system-log-out-symbolic" => "logout",

        // Media playback controls
        "media-playback-start" => "play_arrow",
        "media-playback-pause" => "pause",
        "media-playback-stop" => "stop",
        "media-skip-backward" => "skip_previous",
        "media-skip-forward" => "skip_next",
        "media-seek-backward" => "fast_rewind",
        "media-seek-forward" => "fast_forward",
        "media-playlist-repeat" => "repeat",
        "media-playlist-shuffle" => "shuffle",
        "media-playback-start-symbolic" => "play_arrow",
        "media-playback-pause-symbolic" => "pause",
        "media-playback-stop-symbolic" => "stop",
        "media-skip-backward-symbolic" => "skip_previous",
        "media-skip-forward-symbolic" => "skip_next",
        "media-seek-backward-symbolic" => "fast_rewind",
        "media-seek-forward-symbolic" => "fast_forward",
        "media-playlist-repeat-symbolic" => "repeat",
        "media-playlist-shuffle-symbolic" => "shuffle",
        // Pop-out / open external window
        "window-new-symbolic" => "open_in_new",
        "view-fullscreen-symbolic" => "fullscreen",

        // Fallback: pass through unchanged (allows Material ligature names directly)
        _ => icon_name,
    }
}

/// Maps logical icon names to a list of GTK icon name candidates.
///
/// These names follow the freedesktop.org icon naming specification used by
/// GTK themes like Adwaita, Breeze, Papirus, etc. Multiple candidates are
/// provided in priority order so that if a theme doesn't implement one name,
/// we can fall back to alternatives that are more likely to exist.
///
/// The resolver will try each candidate in order via `IconTheme::has_icon()`
/// and use the first one that exists.
pub fn gtk_icon_candidates(logical: &str) -> &'static [&'static str] {
    match logical {
        // Battery (discharging) - Adwaita level icons, then GNOME/freedesktop fallbacks
        "battery-full" => &[
            "battery-level-100-symbolic",
            "battery-full-symbolic",
            "battery-good-symbolic",
            "battery-symbolic",
        ],
        "battery-high" => &[
            "battery-level-80-symbolic",
            "battery-good-symbolic",
            "battery-full-symbolic",
            "battery-symbolic",
        ],
        "battery-medium-high" => &[
            "battery-level-60-symbolic",
            "battery-good-symbolic",
            "battery-symbolic",
        ],
        "battery-medium" => &[
            "battery-level-50-symbolic",
            "battery-good-symbolic",
            "battery-symbolic",
        ],
        "battery-medium-low" => &[
            "battery-level-30-symbolic",
            "battery-caution-symbolic",
            "battery-low-symbolic",
            "battery-symbolic",
        ],
        "battery-low" => &[
            "battery-level-20-symbolic",
            "battery-low-symbolic",
            "battery-caution-symbolic",
            "battery-symbolic",
        ],
        "battery-critical" => &[
            "battery-level-10-symbolic",
            "battery-caution-symbolic",
            "battery-empty-symbolic",
            "battery-low-symbolic",
            "battery-symbolic",
        ],
        "battery-missing" => &[
            "battery-missing-symbolic",
            "battery-empty-symbolic",
            "battery-caution-symbolic",
            "battery-symbolic",
        ],

        // Battery (charging) - Adwaita level icons, then GNOME/freedesktop fallbacks
        "battery-full-charging" => &[
            "battery-level-100-charged-symbolic",
            "battery-full-charging-symbolic",
            "battery-good-charging-symbolic",
            "battery-full-symbolic",
            "battery-symbolic",
        ],
        "battery-high-charging" => &[
            "battery-level-80-charging-symbolic",
            "battery-good-charging-symbolic",
            "battery-full-charging-symbolic",
            "battery-good-symbolic",
            "battery-symbolic",
        ],
        "battery-medium-high-charging" => &[
            "battery-level-60-charging-symbolic",
            "battery-good-charging-symbolic",
            "battery-good-symbolic",
            "battery-symbolic",
        ],
        "battery-medium-charging" => &[
            "battery-level-50-charging-symbolic",
            "battery-good-charging-symbolic",
            "battery-good-symbolic",
            "battery-symbolic",
        ],
        "battery-medium-low-charging" => &[
            "battery-level-30-charging-symbolic",
            "battery-low-charging-symbolic",
            "battery-caution-symbolic",
            "battery-symbolic",
        ],
        "battery-low-charging" => &[
            "battery-level-20-charging-symbolic",
            "battery-low-charging-symbolic",
            "battery-caution-charging-symbolic",
            "battery-low-symbolic",
            "battery-symbolic",
        ],
        "battery-critical-charging" => &[
            "battery-level-10-charging-symbolic",
            "battery-caution-charging-symbolic",
            "battery-empty-charging-symbolic",
            "battery-caution-symbolic",
            "battery-symbolic",
        ],

        // Notifications
        "notifications" => &[
            "preferences-system-notifications-symbolic",
            "notification-symbolic",
            "bell-symbolic",
        ],
        "notifications-disabled" => &[
            "notifications-disabled-symbolic",
            "notification-disabled-symbolic",
            "preferences-system-notifications-symbolic",
        ],
        "notifications-active" => &[
            "preferences-system-notifications-symbolic",
            "notification-symbolic",
            "bell-symbolic",
        ],

        // Brightness (for OSD)
        "display-brightness-off-symbolic" => &[
            "display-brightness-off-symbolic",
            "display-brightness-symbolic",
            "brightness-display-symbolic",
        ],
        "display-brightness-low-symbolic" => &[
            "display-brightness-low-symbolic",
            "display-brightness-symbolic",
            "brightness-display-symbolic",
        ],
        "display-brightness-medium-symbolic" => &[
            "display-brightness-medium-symbolic",
            "display-brightness-symbolic",
            "brightness-display-symbolic",
        ],
        "display-brightness-high-symbolic" => &[
            "display-brightness-high-symbolic",
            "display-brightness-symbolic",
            "brightness-display-symbolic",
        ],
        "display-brightness-symbolic" => &[
            "display-brightness-symbolic",
            "display-brightness-medium-symbolic",
            "brightness-display-symbolic",
        ],

        // Audio volume (for OSD and quick settings)
        "audio-volume-muted-symbolic" => &[
            "audio-volume-muted-symbolic",
            "audio-volume-muted",
            "audio-volume-low-symbolic",
        ],
        "audio-volume-low-symbolic" => &[
            "audio-volume-low-symbolic",
            "audio-volume-low",
            "audio-volume-medium-symbolic",
        ],
        "audio-volume-medium-symbolic" => &[
            "audio-volume-medium-symbolic",
            "audio-volume-medium",
            "audio-volume-high-symbolic",
        ],
        "audio-volume-high-symbolic" => &[
            "audio-volume-high-symbolic",
            "audio-volume-high",
            "audio-volume-medium-symbolic",
        ],
        "audio-volume-muted" => &["audio-volume-muted", "audio-volume-muted-symbolic"],
        "audio-volume-low" => &["audio-volume-low", "audio-volume-low-symbolic"],
        "audio-volume-medium" => &["audio-volume-medium", "audio-volume-medium-symbolic"],
        "audio-volume-high" => &["audio-volume-high", "audio-volume-high-symbolic"],

        // Microphone sensitivity (for quick settings mic slider)
        "microphone-sensitivity-muted-symbolic" => &[
            "microphone-sensitivity-muted-symbolic",
            "audio-input-microphone-muted-symbolic",
            "microphone-disabled-symbolic",
        ],
        "microphone-sensitivity-low-symbolic" => &[
            "microphone-sensitivity-low-symbolic",
            "audio-input-microphone-symbolic",
            "microphone-symbolic",
        ],
        "microphone-sensitivity-medium-symbolic" => &[
            "microphone-sensitivity-medium-symbolic",
            "audio-input-microphone-symbolic",
            "microphone-symbolic",
        ],
        "microphone-sensitivity-high-symbolic" => &[
            "microphone-sensitivity-high-symbolic",
            "audio-input-microphone-symbolic",
            "microphone-symbolic",
        ],
        "audio-input-microphone-symbolic" => &[
            "audio-input-microphone-symbolic",
            "microphone-sensitivity-high-symbolic",
            "microphone-symbolic",
        ],
        "audio-input-microphone-muted-symbolic" => &[
            "audio-input-microphone-muted-symbolic",
            "microphone-sensitivity-muted-symbolic",
            "microphone-disabled-symbolic",
        ],

        // Selection indicators (for sink list, etc.)
        "object-select-symbolic" => &[
            "object-select-symbolic",
            "emblem-ok-symbolic",
            "emblem-default-symbolic",
        ],
        "radio-symbolic" => &["radio-symbolic", "radio-mixed-symbolic"],
        "radio-checked-symbolic" => &["radio-checked-symbolic", "radio-symbolic"],

        // Wi-Fi signal strength (for quick settings network list)
        "network-wireless-signal-excellent-symbolic" => &[
            "network-wireless-signal-excellent-symbolic",
            "network-wireless-connected-symbolic",
            "network-wireless-symbolic",
        ],
        "network-wireless-signal-good-symbolic" => &[
            "network-wireless-signal-good-symbolic",
            "network-wireless-signal-excellent-symbolic",
            "network-wireless-symbolic",
        ],
        "network-wireless-signal-ok-symbolic" => &[
            "network-wireless-signal-ok-symbolic",
            "network-wireless-signal-good-symbolic",
            "network-wireless-symbolic",
        ],
        "network-wireless-signal-weak-symbolic" => &[
            "network-wireless-signal-weak-symbolic",
            "network-wireless-signal-ok-symbolic",
            "network-wireless-symbolic",
        ],
        "network-wireless-signal-none-symbolic" => &[
            "network-wireless-signal-none-symbolic",
            "network-wireless-signal-weak-symbolic",
            "network-wireless-symbolic",
        ],
        "network-wireless-offline-symbolic" => &[
            "network-wireless-offline-symbolic",
            "network-wireless-disabled-symbolic",
            "network-wireless-signal-none-symbolic",
            "network-wireless-symbolic",
        ],

        // Simplified Wi-Fi names
        "wifi-off" => &[
            "network-wireless-offline-symbolic",
            "network-wireless-signal-none-symbolic",
            "network-wireless-symbolic",
        ],

        // Bluetooth icons
        "bluetooth-symbolic" => &[
            "bluetooth-symbolic",
            "bluetooth-active-symbolic",
            "bluetooth",
        ],
        "bluetooth-active-symbolic" => &[
            "bluetooth-active-symbolic",
            "bluetooth-symbolic",
            "bluetooth",
        ],
        "bluetooth-disabled-symbolic" => &[
            "bluetooth-disabled-symbolic",
            "bluetooth-symbolic",
            "bluetooth",
        ],

        // Bluetooth device type icons (from BlueZ)
        "audio-headphones" => &[
            "audio-headphones-symbolic",
            "audio-headphones",
            "audio-headset-symbolic",
        ],
        "audio-headphones-symbolic" => &[
            "audio-headphones-symbolic",
            "audio-headphones",
            "audio-headset-symbolic",
        ],
        "audio-headset" => &[
            "audio-headset-symbolic",
            "audio-headset",
            "audio-headphones-symbolic",
        ],
        "audio-headset-symbolic" => &[
            "audio-headset-symbolic",
            "audio-headset",
            "audio-headphones-symbolic",
        ],
        "audio-card" => &[
            "audio-card-symbolic",
            "audio-card",
            "audio-speakers-symbolic",
        ],
        "audio-card-symbolic" => &[
            "audio-card-symbolic",
            "audio-card",
            "audio-speakers-symbolic",
        ],
        "audio-speakers" => &[
            "audio-speakers-symbolic",
            "audio-speakers",
            "audio-card-symbolic",
        ],
        "audio-speakers-symbolic" => &[
            "audio-speakers-symbolic",
            "audio-speakers",
            "audio-card-symbolic",
        ],
        "input-keyboard" => &["input-keyboard-symbolic", "input-keyboard"],
        "input-keyboard-symbolic" => &["input-keyboard-symbolic", "input-keyboard"],
        "input-mouse" => &["input-mouse-symbolic", "input-mouse"],
        "input-mouse-symbolic" => &["input-mouse-symbolic", "input-mouse"],
        "input-gaming" => &["input-gaming-symbolic", "input-gaming"],
        "input-gaming-symbolic" => &["input-gaming-symbolic", "input-gaming"],
        "phone" => &["phone-symbolic", "phone", "smartphone-symbolic"],
        "phone-symbolic" => &["phone-symbolic", "phone", "smartphone-symbolic"],
        "computer" => &["computer-symbolic", "computer"],
        "computer-symbolic" => &["computer-symbolic", "computer"],

        // VPN icons
        "network-vpn" => &["network-vpn-symbolic", "network-vpn"],
        "network-vpn-symbolic" => &["network-vpn-symbolic", "network-vpn"],
        "network-vpn-acquiring-symbolic" => &[
            "network-vpn-acquiring-symbolic",
            "network-vpn-symbolic",
            "network-vpn",
        ],
        "network-vpn-connected-symbolic" => &["network-vpn-symbolic", "network-vpn"],
        "network-vpn-disconnected-symbolic" => &[
            "network-vpn-disconnected-symbolic",
            "network-vpn-no-route-symbolic",
            "network-vpn-symbolic",
            "network-vpn",
        ],

        // Idle inhibitor / night light icons
        "night-light-symbolic" => &[
            "night-light-symbolic",
            "preferences-system-time-symbolic",
            "alarm-symbolic",
        ],
        "preferences-system-time-symbolic" => &[
            "preferences-system-time-symbolic",
            "night-light-symbolic",
            "alarm-symbolic",
        ],

        // Software updates
        "software-update-available" => &[
            "software-update-available-symbolic",
            "software-update-available",
            "system-software-update-symbolic",
            "software-update-urgent-symbolic",
        ],
        "software-update-urgent" => &[
            "software-update-urgent-symbolic",
            "software-update-urgent",
            "software-update-available-symbolic",
            "system-software-update-symbolic",
        ],

        // Power menu icons
        "system-shutdown-symbolic" => &[
            "system-shutdown-symbolic",
            "system-shutdown",
            "gnome-shutdown",
        ],
        "system-reboot-symbolic" => &[
            "system-reboot-symbolic",
            "system-reboot",
            "view-refresh-symbolic",
        ],
        "system-suspend-symbolic" => &[
            "system-suspend-symbolic",
            "system-suspend",
            "weather-clear-night-symbolic",
        ],
        "system-lock-screen-symbolic" => &[
            "system-lock-screen-symbolic",
            "system-lock-screen",
            "changes-prevent-symbolic",
        ],
        "system-log-out-symbolic" => &[
            "system-log-out-symbolic",
            "system-log-out",
            "application-exit-symbolic",
        ],

        // Media playback controls
        "media-playback-start" => &["media-playback-start-symbolic", "media-playback-start"],
        "media-playback-pause" => &["media-playback-pause-symbolic", "media-playback-pause"],
        "media-playback-stop" => &["media-playback-stop-symbolic", "media-playback-stop"],
        "media-skip-backward" => &["media-skip-backward-symbolic", "media-skip-backward"],
        "media-skip-forward" => &["media-skip-forward-symbolic", "media-skip-forward"],
        "media-seek-backward" => &["media-seek-backward-symbolic", "media-seek-backward"],
        "media-seek-forward" => &["media-seek-forward-symbolic", "media-seek-forward"],
        "media-playlist-repeat" => &["media-playlist-repeat-symbolic", "media-playlist-repeat"],
        "media-playlist-shuffle" => &["media-playlist-shuffle-symbolic", "media-playlist-shuffle"],
        "media-playback-start-symbolic" => {
            &["media-playback-start-symbolic", "media-playback-start"]
        }
        "media-playback-pause-symbolic" => {
            &["media-playback-pause-symbolic", "media-playback-pause"]
        }
        "media-playback-stop-symbolic" => &["media-playback-stop-symbolic", "media-playback-stop"],
        "media-skip-backward-symbolic" => &["media-skip-backward-symbolic", "media-skip-backward"],
        "media-skip-forward-symbolic" => &["media-skip-forward-symbolic", "media-skip-forward"],
        "media-seek-backward-symbolic" => &["media-seek-backward-symbolic", "media-seek-backward"],
        "media-seek-forward-symbolic" => &["media-seek-forward-symbolic", "media-seek-forward"],
        "media-playlist-repeat-symbolic" => {
            &["media-playlist-repeat-symbolic", "media-playlist-repeat"]
        }
        "media-playlist-shuffle-symbolic" => {
            &["media-playlist-shuffle-symbolic", "media-playlist-shuffle"]
        }
        // Pop-out / open external window
        "window-new-symbolic" => &[
            "window-new-symbolic",
            "window-new",
            "view-fullscreen-symbolic",
        ],
        "view-fullscreen-symbolic" => &["view-fullscreen-symbolic", "view-fullscreen"],

        // Unknown: treat as already-a-GTK-name, return as single-element slice
        // We use a static slice with a placeholder that will be replaced at runtime
        _ => &[],
    }
}

/// Resolve a GTK icon name using the given theme, with fallback behavior.
///
/// Tries each candidate in the provided list via `IconTheme::has_icon()`.
/// If none of the candidates exist, falls back to "image-missing".
///
/// Returns the first candidate that exists in the theme.
fn resolve_gtk_icon(theme: &IconTheme, candidates: &[&str]) -> String {
    for candidate in candidates {
        if theme.has_icon(candidate) {
            return candidate.to_string();
        }
    }

    // None of the candidates found, try "image-missing" as final fallback
    "image-missing".to_string()
}

/// Resolve a single icon name (for passthrough/unknown icons).
///
/// Tries the name as-is, then with "-symbolic" suffix, then "image-missing".
fn resolve_gtk_icon_single(theme: &IconTheme, name: &str) -> String {
    if theme.has_icon(name) {
        return name.to_string();
    }

    if !name.ends_with("-symbolic") {
        let symbolic = format!("{}-symbolic", name);
        if theme.has_icon(&symbolic) {
            return symbolic;
        }
    }

    "image-missing".to_string()
}

/// Get the resolved GTK icon name for a logical icon name.
///
/// Uses the global IconsService's icon theme for resolution. Tries each
/// candidate from `gtk_icon_candidates()` in order, falling back to
/// "image-missing" if none are found.
pub fn gtk_icon_name(logical: &str) -> String {
    let candidates = gtk_icon_candidates(logical);

    ICONS_INSTANCE.with(|cell| {
        let opt = cell.borrow();
        if let Some(service) = opt.as_ref()
            && let Some(ref theme) = *service.icon_theme.borrow()
        {
            // If we have candidates from the mapping, use them
            if !candidates.is_empty() {
                return resolve_gtk_icon(theme, candidates);
            }
            // Otherwise treat the logical name as a direct GTK icon name
            return resolve_gtk_icon_single(theme, logical);
        }
        // No theme available, return logical name unchanged
        logical.to_string()
    })
}

/// Normalize an app_id by trimming whitespace and stripping leading @: characters.
fn normalize_app_id(app_id: &str) -> String {
    app_id
        .trim()
        .trim_start_matches(['@', ':', ' '])
        .to_string()
}

/// Get all DesktopAppInfo instances known to the system.
///
/// We go via `AppInfo::all()` so we don't depend on any DesktopAppInfo-specific
/// convenience helpers that may not be available in all environments.
///
/// Results are cached after first call.
fn iter_desktop_app_infos() -> Vec<DesktopAppInfo> {
    ALL_APP_INFOS.with(|cell| {
        let mut opt = cell.borrow_mut();
        if opt.is_none() {
            *opt = Some(AppInfo::all());
        }

        opt.as_ref()
            .unwrap()
            .iter()
            .filter_map(|info| info.clone().downcast::<DesktopAppInfo>().ok())
            .collect()
    })
}

/// Best-effort search for a DesktopAppInfo matching an app_id.
///
/// Scans all available desktop entries for matches in display names and
/// metadata fields (Exec, StartupWMClass, Icon). Used as a fallback when
/// direct desktop ID lookup fails.
fn search_desktop_appinfo_by_hint(app_id: &str) -> Option<DesktopAppInfo> {
    let base = normalize_app_id(app_id);
    if base.is_empty() {
        return None;
    }
    let base_lower = base.to_lowercase();

    let candidates = iter_desktop_app_infos();

    // First pass: exact match on display name or name.
    for info in &candidates {
        // display_name() and name() return GString directly, not Option
        let display = info.display_name().trim().to_string();
        let name = if display.is_empty() {
            info.name().trim().to_string()
        } else {
            display
        };
        if !name.is_empty() && name.to_lowercase() == base_lower {
            return Some(info.clone());
        }
    }

    // Second pass: partial match on display name or name.
    for info in &candidates {
        let display = info.display_name().trim().to_string();
        let name = if display.is_empty() {
            info.name().trim().to_string()
        } else {
            display
        };
        if !name.is_empty() && name.to_lowercase().contains(&base_lower) {
            return Some(info.clone());
        }
    }

    // Third pass: partial match on Exec / StartupWMClass / Icon keys.
    for info in &candidates {
        for key in ["Exec", "StartupWMClass", "Icon"] {
            if let Some(value) = info.string(key) {
                let value = value.trim().to_string();
                if !value.is_empty() && value.to_lowercase().contains(&base_lower) {
                    return Some(info.clone());
                }
            }
        }
    }

    None
}

/// Return a `DesktopAppInfo` for a compositor-style app_id, if possible.
///
/// Centralizes the logic for turning a window manager app_id into a desktop
/// entry so multiple widgets can share the same behavior and caches.
pub fn get_desktop_appinfo_for_app_id(app_id: &str) -> Option<DesktopAppInfo> {
    let base = normalize_app_id(app_id);
    if base.is_empty() {
        return None;
    }

    // Check cache first
    let cached = APP_DESKTOP_CACHE.with(|cell| cell.borrow().get(&base).cloned());
    if let Some(result) = cached {
        return result;
    }

    // Prefer resolving via the desktop ID ("foo.desktop").
    let candidate = if base.ends_with(".desktop") {
        base.clone()
    } else {
        format!("{}.desktop", base)
    };
    let mut info = DesktopAppInfo::new(&candidate);

    // Fallback: search all desktop entries for unconventional app_ids
    // (e.g., "zen" -> "zen-browser.desktop")
    if info.is_none() {
        info = search_desktop_appinfo_by_hint(&base);
    }

    // Cache the result (even if None, to avoid repeated searches)
    APP_DESKTOP_CACHE.with(|cell| {
        cell.borrow_mut().insert(base.clone(), info.clone());
    });

    info
}

/// Return the best-known themed icon name for a compositor app_id.
///
/// Uses `DesktopAppInfo` to resolve the corresponding desktop entry and
/// caches results keyed by the raw app_id string.
///
/// Returns an empty string if no icon could be found.
pub fn get_app_icon_name(app_id: &str) -> String {
    if app_id.is_empty() {
        return String::new();
    }

    // Check cache first
    let cached = APP_ICON_NAME_CACHE.with(|cell| cell.borrow().get(app_id).cloned());
    if let Some(result) = cached {
        return result;
    }

    let info = get_desktop_appinfo_for_app_id(app_id);
    let icon_name = if let Some(info) = info {
        // Try to get icon from the desktop entry
        if let Some(icon) = info.icon() {
            // Icon::to_string() returns Option<GString> representation
            // For ThemedIcon this is typically the icon name
            icon.to_string().map(|s| s.to_string()).unwrap_or_default()
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    // Cache the result
    APP_ICON_NAME_CACHE.with(|cell| {
        cell.borrow_mut()
            .insert(app_id.to_string(), icon_name.clone());
    });

    icon_name
}

/// Resolve an app ID to a themed icon name.
///
/// This function resolves the app ID to a themed icon name using desktop entries,
/// falling back to the app_id directly if it's a valid icon name, or to a generic
/// fallback icon if neither works.
///
/// # Arguments
/// * `app_id` - The application identifier (e.g., "firefox", "spotify")
/// * `fallback` - The icon name to use if no icon can be found
///
/// # Returns
/// The resolved icon name, or the fallback if no icon could be found.
pub fn resolve_app_icon_name(app_id: &str, fallback: &str) -> String {
    let icon_name = get_app_icon_name(app_id);
    if !icon_name.is_empty() {
        return icon_name;
    }
    // Try the app_id directly as an icon name (some apps use their name)
    let display = gtk4::gdk::Display::default().expect("No display");
    let icon_theme = IconTheme::for_display(&display);
    if icon_theme.has_icon(app_id) {
        return app_id.to_string();
    }
    fallback.to_string()
}

/// Set an Image widget's icon from an app ID (e.g., "firefox", "spotify").
///
/// This function resolves the app ID to a themed icon name using desktop entries,
/// then sets the icon on the provided Image widget. Unlike `IconHandle`, this
/// always uses GTK's icon theme system, which is appropriate for app icons.
///
/// Falls back to "audio-x-generic" if no icon can be found.
pub fn set_image_from_app_id(image: &Image, app_id: &str) {
    let icon_name = resolve_app_icon_name(app_id, "audio-x-generic");
    image.set_icon_name(Some(&icon_name));
}

/// Describes which backend type should be used for icons.
///
/// This enum is used to detect when the backend needs to change (e.g., when
/// switching between Material and GTK themes at runtime).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IconBackendKind {
    /// Material Symbols font (ligature-based icons)
    Material,
    /// GTK icon theme (Adwaita, Breeze, etc.)
    Gtk,
    /// Plain text fallback
    Text,
}

/// The concrete GTK widget type backing an icon.
///
/// Each variant holds a specific widget type optimized for the backend:
/// - `MaterialLabel`: A Label with Material Symbols font (ligature-based icons)
/// - `GtkImage`: A GTK Image using the system icon theme
/// - `TextLabel`: A plain Label showing the logical icon name as text
enum IconBackend {
    MaterialLabel(Label),
    GtkImage(Image),
    TextLabel(Label),
}

impl IconBackend {
    /// Get the kind of this backend.
    fn kind(&self) -> IconBackendKind {
        match self {
            IconBackend::MaterialLabel(_) => IconBackendKind::Material,
            IconBackend::GtkImage(_) => IconBackendKind::Gtk,
            IconBackend::TextLabel(_) => IconBackendKind::Text,
        }
    }

    /// Get the underlying widget.
    fn widget(&self) -> gtk4::Widget {
        match self {
            IconBackend::MaterialLabel(label) => label.clone().upcast(),
            IconBackend::GtkImage(image) => image.clone().upcast(),
            IconBackend::TextLabel(label) => label.clone().upcast(),
        }
    }
}

impl Clone for IconBackend {
    fn clone(&self) -> Self {
        match self {
            IconBackend::MaterialLabel(label) => IconBackend::MaterialLabel(label.clone()),
            IconBackend::GtkImage(image) => IconBackend::GtkImage(image.clone()),
            IconBackend::TextLabel(label) => IconBackend::TextLabel(label.clone()),
        }
    }
}

/// Internal state shared by IconHandle clones and tracked by IconsService.
///
/// This allows the service to reapply icons when the theme changes at runtime.
/// The `root` container is stable and exposed to widgets, while the `backend`
/// child widget can be swapped when switching between Material and GTK themes.
struct IconHandleInner {
    /// Stable root container exposed to widgets. The backend widget is a child.
    root: gtk4::Box,
    /// The concrete backend widget (Label or Image).
    backend: RefCell<IconBackend>,
    /// The last logical icon name set via `set_icon`.
    /// Stored so we can reapply after a theme change.
    logical_name: RefCell<String>,
    /// CSS classes passed at creation time, reapplied when recreating the backend widget.
    css_classes: RefCell<Vec<String>>,
    /// CSS classes added dynamically via `add_css_class()`, also reapplied on rebuild.
    dynamic_classes: RefCell<HashSet<String>>,
}

impl IconHandleInner {
    /// Update the displayed icon using the current backend.
    fn apply_icon(&self, name: &str) {
        *self.logical_name.borrow_mut() = name.to_string();

        match &*self.backend.borrow() {
            IconBackend::MaterialLabel(label) => {
                let glyph = material_symbol_name(name);
                label.set_label(glyph);
            }
            IconBackend::GtkImage(image) => {
                let gtk_name = gtk_icon_name(name);
                image.set_icon_name(Some(&gtk_name));
            }
            IconBackend::TextLabel(label) => {
                label.set_label(name);
            }
        }
    }

    /// Reapply the current logical icon name (called after theme change).
    fn reapply(&self) {
        let name = self.logical_name.borrow().clone();
        if !name.is_empty() {
            self.apply_icon(&name);
        }
    }

    /// Rebuild the backend widget if the backend kind has changed.
    ///
    /// This is called during theme reconfiguration to swap between Material
    /// (Label with ligature font) and GTK (Image) backends.
    fn rebuild_backend(&self, new_kind: IconBackendKind) {
        let current_kind = self.backend.borrow().kind();
        if current_kind == new_kind {
            // Same backend kind, just reapply the icon (handles GTK theme changes)
            self.reapply();
            return;
        }

        // Remove the old child widget from the root container
        if let Some(child) = self.root.first_child() {
            self.root.remove(&child);
        }

        // Create new backend widget with stored CSS classes
        let css_classes = self.css_classes.borrow();
        let css_refs: Vec<&str> = css_classes.iter().map(|s| s.as_str()).collect();
        let new_backend = create_backend_widget(new_kind, &css_refs);

        // Reapply dynamic CSS classes added via add_css_class()
        for class in self.dynamic_classes.borrow().iter() {
            new_backend.widget().add_css_class(class);
        }

        // Add the new child to the root container
        self.root.append(&new_backend.widget());

        // Update the backend
        *self.backend.borrow_mut() = new_backend;

        // Reapply the current icon
        self.reapply();
    }
}

/// A handle to an icon widget, allowing updates without direct GTK access.
///
/// The underlying widget type varies based on the configured icon theme:
/// - Material theme: `gtk4::Label` with Material Symbols font
/// - GTK theme: `gtk4::Image` using the system icon theme  
/// - Fallback: `gtk4::Label` showing the icon name as text
///
/// Call `set_icon` to change the displayed icon. The handle supports live
/// theme switching - when `IconsService::reconfigure` is called, all existing
/// handles automatically update to reflect the new theme.
#[derive(Clone)]
pub struct IconHandle {
    inner: Rc<IconHandleInner>,
}

impl IconHandle {
    /// Get a reference to the underlying GTK widget.
    ///
    /// The returned widget is a stable container that can be used for:
    /// - Appending to containers
    /// - Adding/removing CSS classes
    /// - Showing/hiding the icon
    /// - Setting tooltips
    ///
    /// The internal backend widget (Label or Image) may change when the icon
    /// theme is reconfigured, but this container remains stable.
    pub fn widget(&self) -> gtk4::Widget {
        self.inner.root.clone().upcast()
    }

    /// Add a CSS class to the backend widget.
    ///
    /// Unlike calling `backend_widget().add_css_class()` directly, this method
    /// tracks the class so it survives theme switches (when the backend widget
    /// is recreated).
    pub fn add_css_class(&self, class: &str) {
        self.inner.backend.borrow().widget().add_css_class(class);
        self.inner
            .dynamic_classes
            .borrow_mut()
            .insert(class.to_string());
    }

    /// Remove a CSS class from the backend widget.
    ///
    /// This removes the class from both the current widget and the tracked
    /// set, so it won't be reapplied on theme switches.
    pub fn remove_css_class(&self, class: &str) {
        self.inner.backend.borrow().widget().remove_css_class(class);
        self.inner.dynamic_classes.borrow_mut().remove(class);
    }

    /// Update the displayed icon by logical name.
    ///
    /// The `name` should be a logical icon identifier like "battery-full" or
    /// "battery-low-charging". The name is automatically mapped to the
    /// appropriate backend representation (Material glyph, GTK icon name, or
    /// plain text).
    ///
    /// # Examples
    ///
    /// ```ignore
    /// icon_handle.set_icon("battery-full");
    /// icon_handle.set_icon("battery-low-charging");
    /// icon_handle.set_icon("battery-missing");
    /// ```
    pub fn set_icon(&self, name: &str) {
        self.inner.apply_icon(name);
    }
}

/// Process-wide icon service singleton.
///
/// Handles icon theme initialization and provides `IconHandle` instances
/// for widgets to use. Supports live theme switching via `reconfigure()`.
///
/// # Backend Selection
///
/// The backend is chosen based on the configured theme name:
///
/// - "material" → Material Symbols font (ligature-based icons)
/// - Any other value (e.g. "gtk") → system GTK icon theme
///
/// If the Material font can't be loaded, the service automatically falls back
/// to the GTK backend. If GTK icons aren't available either, it falls back to
/// plain text display.
pub struct IconsService {
    /// The configured icon theme name (e.g., "material", "Adwaita").
    theme: RefCell<String>,
    /// Font weight for Material Symbols (100-700, default 400).
    weight: RefCell<u16>,
    /// Whether the Material Symbols font was successfully loaded.
    material_ready: RefCell<bool>,
    /// Whether we've attempted to load the font CSS.
    css_loaded: RefCell<bool>,
    /// GTK icon theme for non-Material backends (always created if display available).
    icon_theme: RefCell<Option<IconTheme>>,
    /// Weak references to all created icon handles for live reload.
    handles: RefCell<Vec<Weak<IconHandleInner>>>,
    /// CSS provider for Material Symbols (stored for replacement on weight change).
    material_css_provider: RefCell<Option<gtk4::CssProvider>>,
}

impl IconsService {
    /// Create a new IconsService with the given theme name and font weight.
    fn new(theme: String, weight: u16) -> Rc<Self> {
        let service = Rc::new(Self {
            theme: RefCell::new(theme.clone()),
            weight: RefCell::new(weight),
            material_ready: RefCell::new(false),
            css_loaded: RefCell::new(false),
            icon_theme: RefCell::new(None),
            handles: RefCell::new(Vec::new()),
            material_css_provider: RefCell::new(None),
        });

        IconsService::setup_backends(&service, &theme);
        service
    }

    /// Set up icon backends based on the theme name.
    ///
    /// This attaches to the display's icon theme for GTK lookups and sets up
    /// Material CSS when requested. For the GTK backend, we listen for the
    /// icon theme's `changed` signal so that icons are rebuilt live when the
    /// system icon theme changes.
    fn setup_backends(service: &Rc<Self>, theme: &str) {
        // Get the display's icon theme for lookups. We don't call set_theme_name()
        // on this because it's a singleton and GTK warns about modifying it.
        // Instead, we just use it for icon lookups with the system's configured theme.
        if let Some(display) = gtk4::gdk::Display::default() {
            let gtk_theme = IconTheme::for_display(&display);

            // Rebuild icons whenever the system icon theme changes.
            let weak = Rc::downgrade(service);
            gtk_theme.connect_changed(move |_| {
                if let Some(service) = weak.upgrade() {
                    // Only relevant when using the GTK backend (non-Material)
                    if !service.uses_material() {
                        service.reapply_all_icons();
                    }
                }
            });

            *service.icon_theme.borrow_mut() = Some(gtk_theme);
        } else {
            debug!("No display available; GTK icon backend will use fallback");
            *service.icon_theme.borrow_mut() = None;
        }

        // Initialize Material if configured
        if is_material_theme(theme) {
            service.ensure_material_css();
        }
    }

    /// Get the global IconsService singleton.
    ///
    /// On first call, initializes with the "material" theme by default.
    /// Use `init_global` to configure a different theme before first access.
    pub fn global() -> Rc<Self> {
        ICONS_INSTANCE.with(|cell| {
            let mut opt = cell.borrow_mut();
            if opt.is_none() {
                *opt = Some(IconsService::new("material".to_string(), 400));
            }
            opt.as_ref().unwrap().clone()
        })
    }

    /// Initialize the global IconsService with a specific theme and font weight.
    ///
    /// Must be called before `global()` is first accessed, typically
    /// during application startup after loading config.
    pub fn init_global(theme: &str, weight: u16) {
        ICONS_INSTANCE.with(|cell| {
            let mut opt = cell.borrow_mut();
            if opt.is_some() {
                warn!("IconsService already initialized, ignoring init_global call");
                return;
            }
            *opt = Some(IconsService::new(theme.to_string(), weight));
        });
    }

    /// Reconfigure the icon service with a new theme and/or font weight.
    ///
    /// This updates the backend and reapplies all existing icons to reflect
    /// the new theme. Use this for live config reload.
    ///
    /// # Arguments
    ///
    /// * `new_theme` - The new theme name ("material" for Material Symbols,
    ///   or a GTK theme name like "Adwaita", "Breeze", etc.)
    /// * `new_weight` - The font weight for Material Symbols (100-700)
    pub fn reconfigure(&self, new_theme: &str, new_weight: u16) {
        let old_theme = self.theme.borrow().clone();
        let old_weight = *self.weight.borrow();
        let theme_changed = old_theme != new_theme;
        let weight_changed = old_weight != new_weight;

        if !theme_changed && !weight_changed {
            debug!(
                "Icon theme and weight unchanged ({}, {}), skipping reconfigure",
                new_theme, new_weight
            );
            return;
        }

        if theme_changed {
            info!("Reconfiguring icon theme: {} -> {}", old_theme, new_theme);
        }
        if weight_changed {
            info!(
                "Reconfiguring icon weight: {} -> {}",
                old_weight, new_weight
            );
        }

        // Update theme name and weight
        *self.theme.borrow_mut() = new_theme.to_string();
        *self.weight.borrow_mut() = new_weight;

        // Reload Material CSS if switching to Material or if weight changed while using Material
        let switching_to_material = is_material_theme(new_theme) && !is_material_theme(&old_theme);
        if is_material_theme(new_theme) && (switching_to_material || weight_changed) {
            // Force CSS reload by resetting the flag
            *self.css_loaded.borrow_mut() = false;
            self.ensure_material_css();
        }

        // Rebuild all icons with the new theme/weight.
        // With Pango's add_font_file(), fonts are immediately available,
        // so we no longer need to defer this with idle_add_local_once.
        self.reapply_all_icons();
    }

    /// Check if we're using the Material Symbols theme.
    pub fn uses_material(&self) -> bool {
        is_material_theme(&self.theme.borrow())
    }

    /// Get the current theme name.
    #[cfg(test)]
    fn theme(&self) -> String {
        self.theme.borrow().clone()
    }

    /// Check if the Material backend is ready (font loaded, CSS applied).
    fn material_backend_ready(&self) -> bool {
        self.uses_material() && *self.material_ready.borrow()
    }

    /// Determine which backend kind should be used based on current state.
    ///
    /// This is used both for creating new icons and for rebuilding existing
    /// icons when the theme changes.
    fn current_backend_kind(&self) -> IconBackendKind {
        if self.material_backend_ready() {
            IconBackendKind::Material
        } else if self.icon_theme.borrow().is_some() {
            IconBackendKind::Gtk
        } else {
            IconBackendKind::Text
        }
    }

    /// Create an icon widget with the given initial icon name and CSS classes.
    ///
    /// Returns an `IconHandle` that can be used to update the icon later.
    /// The handle is registered for live theme updates.
    ///
    /// # Backend Selection
    ///
    /// 1. If theme is "material" and Material font is ready → Material backend
    /// 2. Else if GTK icon theme is available → GTK backend  
    /// 3. Else → Text fallback backend
    pub fn create_icon(&self, name: &str, css_classes: &[&str]) -> IconHandle {
        // Create stable root container - this defines the icon's bounding box
        let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        root.add_css_class(icon::ROOT);

        // Apply all CSS classes to the root container so that:
        // 1. Layout selectors like `.quick-settings .icon { margin: ... }` work
        // 2. State classes like `.expanded` can be added to root and match
        //    selectors like `.qs-toggle-more-icon.expanded`
        for class in css_classes {
            root.add_css_class(class);
        }

        // Create backend widget based on current theme state
        let backend_kind = self.current_backend_kind();
        let backend = create_backend_widget(backend_kind, css_classes);

        // Add the backend widget to the root container
        root.append(&backend.widget());

        let inner = Rc::new(IconHandleInner {
            root,
            backend: RefCell::new(backend),
            logical_name: RefCell::new(String::new()),
            css_classes: RefCell::new(css_classes.iter().map(|s| s.to_string()).collect()),
            dynamic_classes: RefCell::new(HashSet::new()),
        });

        // Register for live reload
        self.handles.borrow_mut().push(Rc::downgrade(&inner));

        let handle = IconHandle { inner };

        // Set the initial icon
        handle.set_icon(name);

        handle
    }

    /// Reapply icons on all registered handles (called after theme change).
    fn reapply_all_icons(&self) {
        let mut handles = self.handles.borrow_mut();
        let new_kind = self.current_backend_kind();

        // Clean up dead handles and rebuild/reapply live ones
        handles.retain(|weak| {
            if let Some(inner) = weak.upgrade() {
                inner.rebuild_backend(new_kind);
                true
            } else {
                false
            }
        });

        debug!(
            "Rebuilt icons for {} active handles (kind={:?})",
            handles.len(),
            new_kind
        );
    }

    /// Ensure Material Symbols CSS is loaded and font is registered.
    ///
    /// If a previous CSS provider exists (from a prior call), it is removed
    /// from the display before adding the new one. This allows live weight changes.
    fn ensure_material_css(&self) {
        if *self.css_loaded.borrow() {
            return;
        }
        *self.css_loaded.borrow_mut() = true;

        let Some(display) = gtk4::gdk::Display::default() else {
            warn!("No display available, cannot load Material Symbols CSS");
            return;
        };

        // Remove old CSS provider if it exists (for weight changes)
        if let Some(old_provider) = self.material_css_provider.borrow().as_ref() {
            gtk4::style_context_remove_provider_for_display(&display, old_provider);
            debug!("Removed old Material Symbols CSS provider");
        }

        // Try to find and register the font file
        let font_path = Self::find_font_path();
        let font_registered = if let Some(ref path) = font_path {
            debug!("Found Material Symbols font at: {}", path.display());
            register_font_with_pango(path)
        } else {
            warn!(
                "Material Symbols font not found (searched for {}); icons will render as text",
                MATERIAL_FONT_FILE
            );
            false
        };

        if !font_registered {
            // Font not registered - icons will render as text
            // Still mark as ready so we at least try to use the font CSS
            // (in case the font is installed system-wide)
            debug!("Font not registered with Pango, will try system fonts");
        }

        // Get the current weight setting
        let weight = *self.weight.borrow();

        // MINIMAL CSS - just the font setup for Material Symbols
        let css = format!(
            r#"
/* Material Symbols - just font family and ligatures */
.material-symbol {{
    font-family: '{}', 'Material Symbols Rounded', sans-serif;
    font-feature-settings: 'liga' 1;
    font-variation-settings: 'wght' {};
    font-size: inherit;
}}

/* Larger icon for media primary (play/pause) button */
.material-symbol.media-primary-icon {{
    font-size: calc(var(--icon-size) * 1.35);
}}
"#,
            MATERIAL_FONT_FAMILY, weight
        );

        let provider = gtk4::CssProvider::new();
        provider.load_from_string(&css);

        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_USER + 5,
        );

        // Store the provider for later removal if weight changes
        *self.material_css_provider.borrow_mut() = Some(provider);

        *self.material_ready.borrow_mut() = true;
        debug!(
            "Material Symbols CSS loaded (font_registered={}, weight={})",
            font_registered, weight
        );
    }

    /// Try to find the Material Symbols font file.
    ///
    /// Searches in order:
    /// 1. Relative to current working directory (for development)
    /// 2. Relative to executable location
    /// 3. Common system font paths
    /// 4. Extracts embedded font to cache directory as fallback
    fn find_font_path() -> Option<PathBuf> {
        // Try relative to CWD (development)
        let cwd_path = PathBuf::from(MATERIAL_FONT_FILE);
        if cwd_path.exists() {
            return Some(cwd_path);
        }

        // Try relative to executable
        if let Ok(exe_path) = std::env::current_exe()
            && let Some(exe_dir) = exe_path.parent()
        {
            // Check ../assets/fonts/ (typical install layout)
            let relative = exe_dir.join("../").join(MATERIAL_FONT_FILE);
            if relative.exists() {
                return Some(relative);
            }
            // Check ../../assets/fonts/ (running from rust/target/debug/)
            let relative2 = exe_dir.join("../../").join(MATERIAL_FONT_FILE);
            if relative2.exists() {
                return Some(relative2);
            }
            // Check ../../../assets/fonts/ (running from rust/target/debug/deps/)
            let relative3 = exe_dir.join("../../../").join(MATERIAL_FONT_FILE);
            if relative3.exists() {
                return Some(relative3);
            }
            // Check same directory as exe
            let same_dir = exe_dir.join("MaterialSymbolsRounded.ttf");
            if same_dir.exists() {
                return Some(same_dir);
            }
        }

        // Try common system font paths
        let system_paths = [
            "/usr/share/fonts/truetype/material-symbols/MaterialSymbolsRounded.ttf",
            "/usr/local/share/fonts/MaterialSymbolsRounded.ttf",
            "/usr/share/fonts/MaterialSymbolsRounded.ttf",
        ];
        for path in system_paths {
            let p = PathBuf::from(path);
            if p.exists() {
                return Some(p);
            }
        }

        // Fall back to extracting the embedded font to a cache directory
        Self::extract_embedded_font()
    }

    /// Extract the embedded font to a cache directory.
    ///
    /// The font is written to `$XDG_CACHE_HOME/vibepanel/fonts/MaterialSymbolsRounded.ttf`
    /// (or `~/.cache/vibepanel/fonts/` if XDG_CACHE_HOME is not set).
    /// This allows fontconfig to load it like any other font file.
    fn extract_embedded_font() -> Option<PathBuf> {
        // Determine cache directory
        let cache_dir = std::env::var("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .ok()
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".cache"))
            })?;

        let font_dir = cache_dir.join("vibepanel").join("fonts");
        let font_path = font_dir.join("MaterialSymbolsRounded.ttf");

        // If the font already exists and has the correct size, reuse it
        if font_path.exists()
            && let Ok(metadata) = std::fs::metadata(&font_path)
            && metadata.len() == EMBEDDED_FONT_DATA.len() as u64
        {
            debug!("Using cached embedded font at: {}", font_path.display());
            return Some(font_path);
        }

        // Create the directory if needed
        if let Err(e) = std::fs::create_dir_all(&font_dir) {
            warn!(
                "Failed to create font cache directory {}: {}",
                font_dir.display(),
                e
            );
            return None;
        }

        // Write the embedded font data
        match std::fs::write(&font_path, EMBEDDED_FONT_DATA) {
            Ok(()) => {
                info!("Extracted embedded font to: {}", font_path.display());
                Some(font_path)
            }
            Err(e) => {
                warn!(
                    "Failed to write embedded font to {}: {}",
                    font_path.display(),
                    e
                );
                None
            }
        }
    }
}

/// Check if a theme name refers to Material Symbols.
fn is_material_theme(theme: &str) -> bool {
    theme.trim().eq_ignore_ascii_case("material")
}

/// Create a backend widget for the given kind with CSS classes applied.
///
/// This is used both for initial icon creation and for rebuilding backends
/// when the theme changes at runtime.
fn create_backend_widget(kind: IconBackendKind, css_classes: &[&str]) -> IconBackend {
    match kind {
        IconBackendKind::Material => {
            let label = Label::new(None);
            for class in css_classes {
                label.add_css_class(class);
            }
            label.add_css_class(icon::MATERIAL_SYMBOL);
            IconBackend::MaterialLabel(label)
        }
        IconBackendKind::Gtk => {
            let image = Image::new();
            for class in css_classes {
                image.add_css_class(class);
            }
            image.add_css_class(icon::ICON);
            IconBackend::GtkImage(image)
        }
        IconBackendKind::Text => {
            let label = Label::new(None);
            for class in css_classes {
                label.add_css_class(class);
            }
            IconBackend::TextLabel(label)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Material Symbol Mapping Tests

    #[test]
    fn test_material_symbol_mapping() {
        assert_eq!(material_symbol_name("battery-full"), "battery_full");
        assert_eq!(material_symbol_name("battery-high"), "battery_6_bar");
        assert_eq!(material_symbol_name("battery-medium-high"), "battery_5_bar");
        assert_eq!(material_symbol_name("battery-medium"), "battery_4_bar");
        assert_eq!(material_symbol_name("battery-medium-low"), "battery_3_bar");
        assert_eq!(material_symbol_name("battery-low"), "battery_2_bar");
        assert_eq!(material_symbol_name("battery-critical"), "battery_1_bar");
        assert_eq!(material_symbol_name("battery-missing"), "battery_unknown");
    }

    #[test]
    fn test_material_symbol_mapping_charging() {
        assert_eq!(
            material_symbol_name("battery-full-charging"),
            "battery_charging_full"
        );
        assert_eq!(
            material_symbol_name("battery-high-charging"),
            "battery_charging_90"
        );
        assert_eq!(
            material_symbol_name("battery-medium-high-charging"),
            "battery_charging_80"
        );
        assert_eq!(
            material_symbol_name("battery-medium-charging"),
            "battery_charging_60"
        );
        assert_eq!(
            material_symbol_name("battery-medium-low-charging"),
            "battery_charging_50"
        );
        assert_eq!(
            material_symbol_name("battery-low-charging"),
            "battery_charging_30"
        );
        assert_eq!(
            material_symbol_name("battery-critical-charging"),
            "battery_charging_20"
        );
    }

    #[test]
    fn test_material_symbol_fallback() {
        // Unknown names should pass through unchanged
        assert_eq!(material_symbol_name("unknown-icon"), "unknown-icon");
        assert_eq!(material_symbol_name("wifi"), "wifi");
    }

    // GTK Icon Mapping Tests

    #[test]
    fn test_gtk_icon_candidates_battery_discharging() {
        // Adwaita level icons should be primary, with GNOME/freedesktop fallbacks
        let candidates = gtk_icon_candidates("battery-full");
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0], "battery-level-100-symbolic");
        assert!(candidates.contains(&"battery-full-symbolic"));

        let candidates = gtk_icon_candidates("battery-high");
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0], "battery-level-80-symbolic");
        assert!(candidates.contains(&"battery-good-symbolic"));

        let candidates = gtk_icon_candidates("battery-medium-high");
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0], "battery-level-60-symbolic");
        assert!(candidates.contains(&"battery-good-symbolic"));

        let candidates = gtk_icon_candidates("battery-medium");
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0], "battery-level-50-symbolic");
        assert!(candidates.contains(&"battery-good-symbolic"));

        let candidates = gtk_icon_candidates("battery-medium-low");
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0], "battery-level-30-symbolic");
        assert!(candidates.contains(&"battery-caution-symbolic"));

        let candidates = gtk_icon_candidates("battery-low");
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0], "battery-level-20-symbolic");
        assert!(candidates.contains(&"battery-low-symbolic"));

        let candidates = gtk_icon_candidates("battery-critical");
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0], "battery-level-10-symbolic");
        assert!(candidates.contains(&"battery-caution-symbolic"));

        let candidates = gtk_icon_candidates("battery-missing");
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0], "battery-missing-symbolic");
    }

    #[test]
    fn test_gtk_icon_candidates_battery_charging() {
        // Adwaita charging level icons should be primary
        let candidates = gtk_icon_candidates("battery-full-charging");
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0], "battery-level-100-charged-symbolic");
        assert!(candidates.contains(&"battery-full-charging-symbolic"));

        let candidates = gtk_icon_candidates("battery-high-charging");
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0], "battery-level-80-charging-symbolic");
        assert!(candidates.contains(&"battery-good-charging-symbolic"));

        let candidates = gtk_icon_candidates("battery-medium-high-charging");
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0], "battery-level-60-charging-symbolic");
        assert!(candidates.contains(&"battery-good-charging-symbolic"));

        let candidates = gtk_icon_candidates("battery-medium-low-charging");
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0], "battery-level-30-charging-symbolic");
        assert!(candidates.contains(&"battery-low-charging-symbolic"));

        let candidates = gtk_icon_candidates("battery-low-charging");
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0], "battery-level-20-charging-symbolic");
        assert!(candidates.contains(&"battery-low-charging-symbolic"));

        let candidates = gtk_icon_candidates("battery-critical-charging");
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0], "battery-level-10-charging-symbolic");
        assert!(candidates.contains(&"battery-caution-charging-symbolic"));
    }

    #[test]
    fn test_gtk_icon_candidates_unknown_returns_empty() {
        // Unknown names return empty slice (will be handled as passthrough)
        let candidates = gtk_icon_candidates("unknown-icon");
        assert!(candidates.is_empty());

        let candidates = gtk_icon_candidates("some-random-icon-name");
        assert!(candidates.is_empty());
    }

    // Theme Detection Tests

    #[test]
    fn test_is_material_theme() {
        assert!(is_material_theme("material"));
        assert!(is_material_theme("Material"));
        assert!(is_material_theme("MATERIAL"));
        assert!(is_material_theme("  material  "));

        assert!(!is_material_theme("adwaita"));
        assert!(!is_material_theme("Breeze"));
        assert!(!is_material_theme(""));
        assert!(!is_material_theme("material-symbols")); // Only exact "material" now
    }

    #[test]
    fn test_uses_material() {
        // Can't test singleton easily, but we can test via direct struct creation
        let service = IconsService {
            theme: RefCell::new("material".to_string()),
            weight: RefCell::new(400),
            material_ready: RefCell::new(false),
            css_loaded: RefCell::new(false),
            icon_theme: RefCell::new(None),
            handles: RefCell::new(Vec::new()),
            material_css_provider: RefCell::new(None),
        };
        assert!(service.uses_material());

        let service2 = IconsService {
            theme: RefCell::new("adwaita".to_string()),
            weight: RefCell::new(400),
            material_ready: RefCell::new(false),
            css_loaded: RefCell::new(false),
            icon_theme: RefCell::new(None),
            handles: RefCell::new(Vec::new()),
            material_css_provider: RefCell::new(None),
        };
        assert!(!service2.uses_material());
    }

    // Backend Kind Tests

    // Note: Tests that create GTK widgets (Label, Image) require GTK to be
    // initialized and are not suitable for unit tests. The backend kind logic
    // is tested via the current_backend_kind and reconfigure tests instead.

    #[test]
    fn test_current_backend_kind_material_ready() {
        // When material_ready is true and theme is "material", should return Material
        let service = IconsService {
            theme: RefCell::new("material".to_string()),
            weight: RefCell::new(400),
            material_ready: RefCell::new(true),
            css_loaded: RefCell::new(true),
            icon_theme: RefCell::new(None),
            handles: RefCell::new(Vec::new()),
            material_css_provider: RefCell::new(None),
        };
        assert_eq!(service.current_backend_kind(), IconBackendKind::Material);
    }

    #[test]
    fn test_current_backend_kind_material_not_ready() {
        // When material_ready is false but theme is "material", falls back to Text
        // (no icon_theme available in this test)
        let service = IconsService {
            theme: RefCell::new("material".to_string()),
            weight: RefCell::new(400),
            material_ready: RefCell::new(false),
            css_loaded: RefCell::new(false),
            icon_theme: RefCell::new(None),
            handles: RefCell::new(Vec::new()),
            material_css_provider: RefCell::new(None),
        };
        assert_eq!(service.current_backend_kind(), IconBackendKind::Text);
    }

    #[test]
    fn test_current_backend_kind_gtk_theme() {
        // When theme is not "material" but no icon_theme is available, falls back to Text
        let service = IconsService {
            theme: RefCell::new("Adwaita".to_string()),
            weight: RefCell::new(400),
            material_ready: RefCell::new(false),
            css_loaded: RefCell::new(false),
            icon_theme: RefCell::new(None),
            handles: RefCell::new(Vec::new()),
            material_css_provider: RefCell::new(None),
        };
        assert_eq!(service.current_backend_kind(), IconBackendKind::Text);
    }

    #[test]
    fn test_reconfigure_changes_theme_and_backend_kind() {
        // Test that reconfigure() updates theme and backend kind
        let service = IconsService {
            theme: RefCell::new("material".to_string()),
            weight: RefCell::new(400),
            material_ready: RefCell::new(true),
            css_loaded: RefCell::new(true),
            icon_theme: RefCell::new(None),
            handles: RefCell::new(Vec::new()),
            material_css_provider: RefCell::new(None),
        };

        assert_eq!(service.theme(), "material");
        assert!(service.uses_material());
        assert_eq!(service.current_backend_kind(), IconBackendKind::Material);

        // Reconfigure to a GTK theme
        service.reconfigure("Adwaita", 400);

        assert_eq!(service.theme(), "Adwaita");
        assert!(!service.uses_material());
        // Since no icon_theme is available, backend kind falls back to Text
        assert_eq!(service.current_backend_kind(), IconBackendKind::Text);
    }

    #[test]
    fn test_reconfigure_same_theme_is_noop() {
        // Reconfiguring to the same theme and weight should be a no-op
        let service = IconsService {
            theme: RefCell::new("material".to_string()),
            weight: RefCell::new(400),
            material_ready: RefCell::new(true),
            css_loaded: RefCell::new(true),
            icon_theme: RefCell::new(None),
            handles: RefCell::new(Vec::new()),
            material_css_provider: RefCell::new(None),
        };

        // This should not change anything
        service.reconfigure("material", 400);

        assert_eq!(service.theme(), "material");
        assert!(service.uses_material());
    }
}
