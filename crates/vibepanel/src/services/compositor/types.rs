//! Compositor backend types and traits.
//!
//! These types are designed to be generic across different Wayland compositors.
//!
//! # Output Name Contract
//!
//! For multi-monitor support to work correctly, backend output names must align
//! with GTK's `Monitor::connector()` names:
//!
//! - `WorkspaceSnapshot.per_output` keys should be connector names (e.g., "eDP-1", "DP-1").
//! - `WindowInfo.output` should use the same connector names.
//! - `BarManager` uses `monitor.connector()` to key bars and passes this as `output_id`
//!   to widgets for per-monitor filtering.
//!
//! When connector names are unavailable:
//! - `BarManager` falls back to `"unknown-{index}"`.
//! - Backends should use a consistent fallback (e.g., `"output-{id}"`) for both
//!   `per_output` keys and `WindowInfo.output` to ensure widget filtering works.
//!
//! Note: The `bar.outputs` config option only reliably targets monitors with real
//! connector names; fallback names are inherently unstable across hot-plug events.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Static metadata for a workspace/tag.
///
/// This represents the compositor's view of a workspace that exists,
/// independent of whether it's active or has windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMeta {
    /// Unique identifier (typically 1-based index).
    pub id: i32,
    /// Display name for the workspace.
    pub name: String,
    /// Output/monitor name this workspace belongs to.
    /// - For Niri: workspaces are per-monitor, so this is always set.
    /// - For MangoWC/Hyprland: workspaces are global, so this is None.
    pub output: Option<String>,
}

/// Per-output workspace state for multi-monitor setups.
///
/// This contains workspace state specific to a single output/monitor,
/// used for compositors where workspace state varies per-output (like
/// MangoWC's per-output window counts, or Niri's per-monitor workspaces).
#[derive(Debug, Clone, Default)]
pub struct PerOutputState {
    /// Active workspace IDs on this output.
    /// Most compositors have a single active workspace, but MangoWC/DWL
    /// supports viewing multiple tags simultaneously.
    pub active_workspace: HashSet<i32>,
    /// Set of workspace IDs that have windows on this output.
    pub occupied_workspaces: HashSet<i32>,
    /// Number of windows per workspace on this output.
    pub window_counts: HashMap<i32, u32>,
}

/// Point-in-time snapshot of workspace state.
///
/// This represents the current state across all workspaces,
/// updated atomically when the compositor signals changes.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceSnapshot {
    /// Currently active/focused workspace IDs.
    /// Most compositors have a single active workspace, but MangoWC/DWL
    /// supports viewing multiple tags simultaneously.
    pub active_workspace: HashSet<i32>,
    /// Set of workspace IDs that have windows.
    pub occupied_workspaces: HashSet<i32>,
    /// Set of workspace IDs marked as urgent.
    pub urgent_workspaces: HashSet<i32>,
    /// Number of windows per workspace (workspace_id -> count).
    /// Not all backends provide this information.
    pub window_counts: HashMap<i32, u32>,
    /// Per-output workspace state for multi-monitor setups.
    /// Key is the output/monitor connector name (e.g., "eDP-1", "DP-1").
    pub per_output: HashMap<String, PerOutputState>,
}

/// Information about a focused window.
///
/// Represents the currently focused window's metadata.
#[derive(Debug, Clone, Default)]
pub struct WindowInfo {
    /// Window title (may be empty).
    pub title: String,
    /// Application ID (e.g., "firefox", "org.gnome.Nautilus").
    pub app_id: String,
    /// Workspace ID the window is on (None if unavailable).
    pub workspace_id: Option<i32>,
    /// Output/monitor name the window is on (None if unavailable).
    pub output: Option<String>,
}

impl WindowInfo {
    /// Returns true if this window info has no meaningful content.
    #[allow(dead_code)] // Used by tests and part of public API
    pub fn is_empty(&self) -> bool {
        self.title.is_empty() && self.app_id.is_empty()
    }
}

/// Callback type for workspace state updates.
pub type WorkspaceCallback = Arc<dyn Fn(WorkspaceSnapshot) + Send + Sync>;

/// Callback type for focused window updates.
pub type WindowCallback = Arc<dyn Fn(WindowInfo) + Send + Sync>;

/// Trait for compositor backend implementations.
///
/// Each backend is responsible for:
/// - Connecting to the compositor's IPC mechanism.
/// - Monitoring workspace/tag and window state changes.
/// - Invoking callbacks when state changes.
/// - Providing query methods for current state.
///
/// Implementations must be Send + Sync as they may be accessed from multiple threads.
///
/// # Lifecycle
///
/// 1. Create backend via factory (`create_backend`).
/// 2. Call `start()` with callbacks for workspace and window updates.
/// 3. Backend runs a monitoring loop (typically in background thread).
/// 4. Call `stop()` to terminate monitoring.
///
/// # Threading Model
///
/// Callbacks will be invoked from the backend's monitoring thread.
/// Services should marshal updates to the GTK main loop as needed.
pub trait CompositorBackend: Send + Sync {
    /// Start the backend monitoring loop.
    ///
    /// # Arguments
    ///
    /// * `on_workspace_update` - Called when workspace state changes.
    /// * `on_window_update` - Called when focused window changes.
    fn start(&self, on_workspace_update: WorkspaceCallback, on_window_update: WindowCallback);

    /// Stop the backend monitoring loop.
    ///
    /// This should cleanly shut down any background threads and close
    /// IPC connections.
    fn stop(&self);

    /// Get the list of known workspaces.
    ///
    /// Returns static workspace metadata. For compositors with fixed
    /// workspace counts (like DWL's tags), this returns all possible
    /// workspaces. For dynamic compositors (like Niri), this returns
    /// currently existing workspaces.
    fn list_workspaces(&self) -> Vec<WorkspaceMeta>;

    /// Get the current workspace state snapshot.
    ///
    /// Returns the last known state. May be stale if called before
    /// `start()` or if the backend hasn't received updates yet.
    fn get_workspace_snapshot(&self) -> WorkspaceSnapshot;

    /// Get the currently focused window.
    ///
    /// Returns the last known focused window info, or None if
    /// no window is focused or state is unknown.
    fn get_focused_window(&self) -> Option<WindowInfo>;

    /// Switch to a workspace.
    ///
    /// Requests the compositor to activate the specified workspace.
    /// This is typically called in response to user interaction.
    fn switch_workspace(&self, workspace_id: i32);

    /// Get the backend's name for debugging.
    fn name(&self) -> &'static str;

    /// Request the compositor to quit/exit.
    ///
    /// This sends a quit command to the compositor via its native IPC.
    /// Used for logout functionality. Default implementation is a no-op
    /// for compositors that don't support this.
    fn quit_compositor(&self) {
        // Default no-op
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workspace_meta_equality() {
        let ws1 = WorkspaceMeta {
            id: 1,
            name: "1".to_string(),
            output: None,
        };
        let ws2 = WorkspaceMeta {
            id: 1,
            name: "1".to_string(),
            output: None,
        };
        let ws3 = WorkspaceMeta {
            id: 2,
            name: "2".to_string(),
            output: None,
        };

        assert_eq!(ws1, ws2);
        assert_ne!(ws1, ws3);
    }

    #[test]
    fn test_workspace_snapshot_default() {
        let snapshot = WorkspaceSnapshot::default();
        assert!(snapshot.active_workspace.is_empty());
        assert!(snapshot.occupied_workspaces.is_empty());
        assert!(snapshot.urgent_workspaces.is_empty());
    }

    #[test]
    fn test_window_info_is_empty() {
        let empty = WindowInfo::default();
        assert!(empty.is_empty());

        let with_title = WindowInfo {
            title: "Test".to_string(),
            ..Default::default()
        };
        assert!(!with_title.is_empty());

        let with_app_id = WindowInfo {
            app_id: "test".to_string(),
            ..Default::default()
        };
        assert!(!with_app_id.is_empty());
    }

    #[test]
    fn test_per_output_state_no_active() {
        let state = PerOutputState::default();
        assert!(state.active_workspace.is_empty());
        assert!(!state.active_workspace.contains(&1));
    }

    #[test]
    fn test_per_output_state_single_active() {
        // Single active workspace (typical Niri/Hyprland case)
        let mut state = PerOutputState::default();
        state.active_workspace.insert(2);

        assert!(state.active_workspace.contains(&2));
        assert!(!state.active_workspace.contains(&1));
        assert!(!state.active_workspace.contains(&3));
        assert_eq!(state.active_workspace.len(), 1);
    }

    #[test]
    fn test_per_output_state_multiple_active() {
        // Multiple active workspaces (Mango/DWL multi-tag case)
        let mut state = PerOutputState::default();
        state.active_workspace.insert(1);
        state.active_workspace.insert(3);
        state.active_workspace.insert(5);

        assert!(state.active_workspace.contains(&1));
        assert!(state.active_workspace.contains(&3));
        assert!(state.active_workspace.contains(&5));
        assert!(!state.active_workspace.contains(&2));
        assert!(!state.active_workspace.contains(&4));
        assert_eq!(state.active_workspace.len(), 3);
    }

    #[test]
    fn test_workspace_snapshot_single_active() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot.active_workspace.insert(1);

        assert!(snapshot.active_workspace.contains(&1));
        assert!(!snapshot.active_workspace.contains(&2));
        assert_eq!(snapshot.active_workspace.len(), 1);
    }

    #[test]
    fn test_workspace_snapshot_multiple_active() {
        // Multi-tag view scenario
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot.active_workspace.insert(1);
        snapshot.active_workspace.insert(3);

        assert!(snapshot.active_workspace.contains(&1));
        assert!(snapshot.active_workspace.contains(&3));
        assert!(!snapshot.active_workspace.contains(&2));
        assert_eq!(snapshot.active_workspace.len(), 2);
    }
}
