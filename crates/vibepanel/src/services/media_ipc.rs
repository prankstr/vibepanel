//! State file for CLI ↔ MediaService communication.
//!
//! The panel writes the currently active player to a state file so that
//! CLI commands (`vibepanel media play-pause`, etc.) can control the same
//! player that's shown in the UI.
//!
//! State file: `$XDG_RUNTIME_DIR/vibepanel-media-state` contains:
//! - Line 1: active player bus name (or empty if none)

use std::path::PathBuf;
use tracing::debug;

/// Get the state file path.
fn state_file_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("vibepanel-media-state")
    } else {
        PathBuf::from("/tmp/vibepanel-media-state")
    }
}

/// Write current media state to the state file.
///
/// Called by MediaService when the active player changes.
pub fn write_state(active_bus_name: Option<&str>) {
    let path = state_file_path();
    let content = active_bus_name.unwrap_or("");
    if let Err(e) = std::fs::write(&path, content) {
        debug!("Media state: failed to write state file: {}", e);
    }
}

/// Read current active player from the state file.
///
/// Returns `None` if file doesn't exist or is empty.
pub fn read_state() -> Option<String> {
    let path = state_file_path();
    std::fs::read_to_string(&path).ok().and_then(|s| {
        let s = s.trim();
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    })
}
