//! State persistence service for vibepanel
//!
//! Persists runtime state to $XDG_STATE_HOME/vibepanel/state.json
//! This includes:
//! - VPN last used connection UUID
//! - Notification muted (DND) state
//! - Notification history
//! - Media window open state

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Maximum number of notifications to persist to disk
const MAX_PERSISTED_NOTIFICATIONS: usize = 50;

/// Root state structure containing all persisted state
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PersistedState {
    #[serde(default)]
    pub vpn: VpnState,
    #[serde(default)]
    pub notifications: NotificationState,
    #[serde(default)]
    pub media: MediaState,
}

/// VPN-related persisted state
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct VpnState {
    /// UUID of the last successfully connected VPN
    pub last_used_uuid: Option<String>,
}

/// Media-related persisted state
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct MediaState {
    /// Whether the media window was open when vibepanel last closed
    pub window_open: bool,
}

/// Notification-related persisted state
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct NotificationState {
    /// Whether notifications are muted (Do Not Disturb mode)
    pub muted: bool,
    /// Next notification ID to assign
    pub next_id: u32,
    /// Notification history (most recent first)
    pub history: Vec<PersistedNotification>,
}

/// A notification suitable for JSON serialization
///
/// This mirrors the `Notification` struct but omits `image_data`
/// which contains binary data not suitable for JSON persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedNotification {
    pub id: u32,
    pub app_name: String,
    pub app_icon: String,
    pub summary: String,
    pub body: String,
    pub actions: Vec<(String, String)>,
    pub urgency: u8,
    pub timestamp: f64,
    pub expire_timeout: i32,
    pub desktop_entry: Option<String>,
    pub image_path: Option<String>,
    // Note: image_data intentionally omitted (binary data, not suitable for JSON)
}

/// Returns the path to the state file
///
/// Location: `$XDG_STATE_HOME/vibepanel/state.json`
/// Default: `~/.local/state/vibepanel/state.json`
fn state_file_path() -> PathBuf {
    let state_home = std::env::var("XDG_STATE_HOME").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        format!("{}/.local/state", home)
    });
    PathBuf::from(state_home)
        .join("vibepanel")
        .join("state.json")
}

/// Load persisted state from disk
///
/// Returns `PersistedState::default()` if the file doesn't exist or is invalid.
pub fn load() -> PersistedState {
    let path = state_file_path();
    match std::fs::read_to_string(&path) {
        Ok(contents) => match serde_json::from_str(&contents) {
            Ok(state) => {
                tracing::debug!("Loaded state from {:?}", path);
                state
            }
            Err(e) => {
                tracing::warn!("Failed to parse state file {:?}: {}", path, e);
                PersistedState::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!("No state file found at {:?}, using defaults", path);
            PersistedState::default()
        }
        Err(e) => {
            tracing::warn!("Failed to read state file {:?}: {}", path, e);
            PersistedState::default()
        }
    }
}

/// Save persisted state to disk
///
/// Creates the parent directory if it doesn't exist.
/// Enforces the notification history limit before saving.
pub fn save(state: &PersistedState) {
    let path = state_file_path();

    // Ensure directory exists
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!("Failed to create state directory {:?}: {}", parent, e);
        return;
    }

    // Enforce notification limit before saving
    let mut state = state.clone();
    if state.notifications.history.len() > MAX_PERSISTED_NOTIFICATIONS {
        state
            .notifications
            .history
            .truncate(MAX_PERSISTED_NOTIFICATIONS);
    }

    match serde_json::to_string_pretty(&state) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                tracing::warn!("Failed to save state to {:?}: {}", path, e);
            } else {
                tracing::debug!("Saved state to {:?}", path);
            }
        }
        Err(e) => {
            tracing::warn!("Failed to serialize state: {}", e);
        }
    }
}
