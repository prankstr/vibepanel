//! WorkspaceService - shared, event-driven workspace state via CompositorManager.
//!
//! This provides a GTK-friendly API for workspace state:
//! - Uses the shared CompositorManager singleton
//! - Provides snapshot-based state access
//! - Supports callback registration for reactive updates

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use tracing::debug;

use super::callbacks::{CallbackId, Callbacks};
use super::compositor::{CompositorManager, WorkspaceMeta, WorkspaceSnapshot};

/// Enriched workspace object for widget consumption.
///
/// Combines static metadata with dynamic state for convenient widget rendering.
#[derive(Debug, Clone)]
pub struct Workspace {
    /// Unique workspace ID.
    pub id: i32,
    /// Display name for the workspace.
    pub name: String,
    /// Whether this is the currently active workspace.
    pub active: bool,
    /// Whether this workspace has windows.
    pub occupied: bool,
    /// Whether this workspace is marked urgent.
    pub urgent: bool,
    /// Number of windows on this workspace (if available from backend).
    pub window_count: Option<u32>,
    /// Output/monitor this workspace belongs to.
    /// - For Niri: always set (workspaces are per-monitor).
    /// - For MangoWC/Hyprland: always None (workspaces are global).
    #[allow(dead_code)] // Part of public API for future use
    pub output: Option<String>,
}

impl Workspace {
    /// Create a workspace from metadata using global state.
    fn from_meta(meta: &WorkspaceMeta, snapshot: &WorkspaceSnapshot) -> Self {
        Self {
            id: meta.id,
            name: meta.name.clone(),
            active: snapshot.active_workspace.contains(&meta.id),
            occupied: snapshot.occupied_workspaces.contains(&meta.id),
            urgent: snapshot.urgent_workspaces.contains(&meta.id),
            window_count: snapshot.window_counts.get(&meta.id).copied(),
            output: meta.output.clone(),
        }
    }

    /// Create a workspace from metadata using per-output state.
    ///
    /// This uses the per-output window counts/occupied state instead of global,
    /// which is needed for multi-monitor setups where each bar should show
    /// the correct window count for its own output.
    fn from_meta_per_output(
        meta: &WorkspaceMeta,
        snapshot: &WorkspaceSnapshot,
        output: &str,
    ) -> Self {
        let per_output = snapshot.per_output.get(output);

        // Use per-output state if available, otherwise fall back to global
        let (active, occupied, window_count) = if let Some(state) = per_output {
            (
                state.active_workspace.contains(&meta.id),
                state.occupied_workspaces.contains(&meta.id),
                state.window_counts.get(&meta.id).copied(),
            )
        } else {
            (
                snapshot.active_workspace.contains(&meta.id),
                snapshot.occupied_workspaces.contains(&meta.id),
                snapshot.window_counts.get(&meta.id).copied(),
            )
        };

        Self {
            id: meta.id,
            name: meta.name.clone(),
            active,
            occupied,
            urgent: snapshot.urgent_workspaces.contains(&meta.id),
            window_count,
            output: meta.output.clone(),
        }
    }
}

/// Per-output workspace state for widget consumption.
///
/// Contains the workspace state specific to a single output/monitor,
/// with window counts and active state tailored to that output.
#[derive(Debug, Clone)]
pub struct PerOutputWorkspaces {
    /// Currently active workspace IDs on this output.
    /// Most compositors have a single active workspace, but MangoWC/DWL
    /// supports viewing multiple tags simultaneously.
    pub active_workspace: HashSet<i32>,
    /// Workspaces relevant to this output with per-output state.
    /// For MangoWC: all workspaces with per-output window counts.
    /// For Niri: only workspaces that belong to this output.
    pub workspaces: Vec<Workspace>,
}

/// Snapshot of workspace service state for callbacks.
///
/// This is a GTK-friendly view of the workspace state.
#[derive(Debug, Clone)]
pub struct WorkspaceServiceSnapshot {
    /// Currently active workspace IDs.
    /// Most compositors have a single active workspace, but MangoWC/DWL
    /// supports viewing multiple tags simultaneously.
    pub active_workspace: HashSet<i32>,
    /// Set of occupied workspace IDs.
    #[allow(dead_code)] // Part of public API for future use
    pub occupied_workspaces: HashSet<i32>,
    /// Window count per workspace (workspace_id -> count).
    #[allow(dead_code)] // Part of public API for future use
    pub window_counts: HashMap<i32, u32>,
    /// All workspaces with their current state.
    pub workspaces: Vec<Workspace>,
    /// Per-output workspace state for multi-monitor setups.
    /// Key is the output/monitor connector name (e.g., "eDP-1", "DP-1").
    pub per_output: HashMap<String, PerOutputWorkspaces>,
}

/// Shared, process-wide workspace service.
///
/// Provides reactive workspace state with GTK main loop integration.
/// Widgets should call `connect()` to receive updates when state changes.
pub struct WorkspaceService {
    /// Reference to the compositor manager.
    manager: Rc<CompositorManager>,
    /// Current workspace snapshot.
    snapshot: RefCell<WorkspaceSnapshot>,
    /// Static workspace metadata.
    workspaces: RefCell<Vec<WorkspaceMeta>>,
    /// Registered callbacks.
    callbacks: Callbacks<WorkspaceServiceSnapshot>,
    /// Whether the service has received at least one update.
    ready: RefCell<bool>,
}

impl WorkspaceService {
    fn new() -> Rc<Self> {
        // Get the shared compositor manager
        let manager = CompositorManager::global();

        // Get initial state from manager
        let initial_snapshot = manager.get_workspace_snapshot();
        let workspaces = manager.list_workspaces();

        let service = Rc::new(Self {
            manager,
            snapshot: RefCell::new(initial_snapshot),
            workspaces: RefCell::new(workspaces),
            callbacks: Callbacks::new(),
            ready: RefCell::new(true), // Ready immediately since manager handles startup
        });

        // Register with compositor manager
        Self::register_with_manager(&service);

        debug!("WorkspaceService initialized (using CompositorManager)");
        service
    }

    /// Get the global WorkspaceService singleton.
    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<WorkspaceService> = WorkspaceService::new();
        }

        INSTANCE.with(|s| s.clone())
    }

    /// Register a callback to be invoked when workspace state changes.
    /// The callback is always executed on the GLib main loop.
    pub fn connect<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&WorkspaceServiceSnapshot) + 'static,
    {
        let id = self.callbacks.register(callback);

        // Immediately send current state so widgets can render.
        if *self.ready.borrow() {
            let snapshot = self.build_snapshot();
            self.callbacks.notify_single(id, &snapshot);
        }
        id
    }

    /// Unregister a callback by its ID.
    pub fn disconnect(&self, id: CallbackId) -> bool {
        self.callbacks.unregister(id)
    }

    /// Request the compositor to switch to a workspace.
    pub fn switch_workspace(&self, workspace_id: i32) {
        self.manager.switch_workspace(workspace_id);
    }

    fn handle_update(&self, snapshot: WorkspaceSnapshot) {
        // Update stored snapshot
        *self.snapshot.borrow_mut() = snapshot;
        *self.ready.borrow_mut() = true;

        // Also refresh workspace list (in case of dynamic workspaces)
        *self.workspaces.borrow_mut() = self.manager.list_workspaces();

        // Build enriched snapshot and notify callbacks.
        let service_snapshot = self.build_snapshot();
        self.callbacks.notify(&service_snapshot);
    }

    fn register_with_manager(this: &Rc<Self>) {
        // Create callback that handles updates
        let service_weak = Rc::downgrade(this);
        this.manager.register_workspace_callback(move |snapshot| {
            if let Some(service) = service_weak.upgrade() {
                service.handle_update(snapshot.clone());
            }
        });
    }

    fn build_snapshot(&self) -> WorkspaceServiceSnapshot {
        let snapshot = self.snapshot.borrow();
        let workspaces_meta = self.workspaces.borrow();

        // Build global workspace list
        let workspaces: Vec<Workspace> = workspaces_meta
            .iter()
            .map(|meta| Workspace::from_meta(meta, &snapshot))
            .collect();

        // Build per-output workspace lists
        let mut per_output = HashMap::new();

        for (output_name, output_state) in &snapshot.per_output {
            // Filter workspaces for this output:
            // - For Niri: only include workspaces that belong to this output
            // - For MangoWC: include all workspaces (tags are global) but with per-output state
            let output_workspaces: Vec<Workspace> = workspaces_meta
                .iter()
                .filter(|meta| {
                    // Include if workspace is global (output is None) or belongs to this output
                    meta.output.is_none() || meta.output.as_ref() == Some(output_name)
                })
                .map(|meta| Workspace::from_meta_per_output(meta, &snapshot, output_name))
                .collect();

            per_output.insert(
                output_name.clone(),
                PerOutputWorkspaces {
                    active_workspace: output_state.active_workspace.clone(),
                    workspaces: output_workspaces,
                },
            );
        }

        WorkspaceServiceSnapshot {
            active_workspace: snapshot.active_workspace.clone(),
            occupied_workspaces: snapshot.occupied_workspaces.clone(),
            window_counts: snapshot.window_counts.clone(),
            workspaces,
            per_output,
        }
    }
}

impl Drop for WorkspaceService {
    fn drop(&mut self) {
        debug!("WorkspaceService dropped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::compositor::types::PerOutputState;

    fn make_meta(id: i32) -> WorkspaceMeta {
        WorkspaceMeta {
            id,
            name: id.to_string(),
            output: None,
        }
    }

    #[test]
    fn test_workspace_from_meta_no_active() {
        let snapshot = WorkspaceSnapshot::default();
        let meta = make_meta(1);

        let ws = Workspace::from_meta(&meta, &snapshot);

        assert_eq!(ws.id, 1);
        assert!(!ws.active);
        assert!(!ws.occupied);
        assert!(!ws.urgent);
    }

    #[test]
    fn test_workspace_from_meta_single_active() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot.active_workspace.insert(2);

        // Workspace 2 should be active
        let ws2 = Workspace::from_meta(&make_meta(2), &snapshot);
        assert!(ws2.active);

        // Workspace 1 should not be active
        let ws1 = Workspace::from_meta(&make_meta(1), &snapshot);
        assert!(!ws1.active);

        // Workspace 3 should not be active
        let ws3 = Workspace::from_meta(&make_meta(3), &snapshot);
        assert!(!ws3.active);
    }

    #[test]
    fn test_workspace_from_meta_multiple_active() {
        // Multi-tag view: workspaces 1, 3, 5 are all active
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot.active_workspace.insert(1);
        snapshot.active_workspace.insert(3);
        snapshot.active_workspace.insert(5);

        // All three should be marked active
        assert!(Workspace::from_meta(&make_meta(1), &snapshot).active);
        assert!(Workspace::from_meta(&make_meta(3), &snapshot).active);
        assert!(Workspace::from_meta(&make_meta(5), &snapshot).active);

        // Others should not be active
        assert!(!Workspace::from_meta(&make_meta(2), &snapshot).active);
        assert!(!Workspace::from_meta(&make_meta(4), &snapshot).active);
    }

    #[test]
    fn test_workspace_from_meta_occupied_and_urgent() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot.occupied_workspaces.insert(1);
        snapshot.occupied_workspaces.insert(2);
        snapshot.urgent_workspaces.insert(2);
        snapshot.window_counts.insert(1, 3);
        snapshot.window_counts.insert(2, 1);

        let ws1 = Workspace::from_meta(&make_meta(1), &snapshot);
        assert!(ws1.occupied);
        assert!(!ws1.urgent);
        assert_eq!(ws1.window_count, Some(3));

        let ws2 = Workspace::from_meta(&make_meta(2), &snapshot);
        assert!(ws2.occupied);
        assert!(ws2.urgent);
        assert_eq!(ws2.window_count, Some(1));

        let ws3 = Workspace::from_meta(&make_meta(3), &snapshot);
        assert!(!ws3.occupied);
        assert!(!ws3.urgent);
        assert_eq!(ws3.window_count, None);
    }

    #[test]
    fn test_workspace_from_meta_per_output_single_active() {
        let mut snapshot = WorkspaceSnapshot::default();

        // Set up per-output state for "eDP-1"
        let mut per_output_state = PerOutputState::default();
        per_output_state.active_workspace.insert(2);
        per_output_state.occupied_workspaces.insert(2);
        per_output_state.window_counts.insert(2, 5);
        snapshot
            .per_output
            .insert("eDP-1".to_string(), per_output_state);

        // Workspace 2 should be active on eDP-1
        let ws2 = Workspace::from_meta_per_output(&make_meta(2), &snapshot, "eDP-1");
        assert!(ws2.active);
        assert!(ws2.occupied);
        assert_eq!(ws2.window_count, Some(5));

        // Workspace 1 should not be active on eDP-1
        let ws1 = Workspace::from_meta_per_output(&make_meta(1), &snapshot, "eDP-1");
        assert!(!ws1.active);
    }

    #[test]
    fn test_workspace_from_meta_per_output_multiple_active() {
        // Multi-tag view on a specific output
        let mut snapshot = WorkspaceSnapshot::default();

        let mut per_output_state = PerOutputState::default();
        per_output_state.active_workspace.insert(1);
        per_output_state.active_workspace.insert(3);
        per_output_state.active_workspace.insert(5);
        snapshot
            .per_output
            .insert("DP-1".to_string(), per_output_state);

        // All three should be active on DP-1
        assert!(Workspace::from_meta_per_output(&make_meta(1), &snapshot, "DP-1").active);
        assert!(Workspace::from_meta_per_output(&make_meta(3), &snapshot, "DP-1").active);
        assert!(Workspace::from_meta_per_output(&make_meta(5), &snapshot, "DP-1").active);

        // Others should not be active
        assert!(!Workspace::from_meta_per_output(&make_meta(2), &snapshot, "DP-1").active);
        assert!(!Workspace::from_meta_per_output(&make_meta(4), &snapshot, "DP-1").active);
    }

    #[test]
    fn test_workspace_from_meta_per_output_fallback_to_global() {
        // When per-output state doesn't exist, should fall back to global
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot.active_workspace.insert(1);
        snapshot.occupied_workspaces.insert(1);

        // No per-output state for "HDMI-1", should use global
        let ws = Workspace::from_meta_per_output(&make_meta(1), &snapshot, "HDMI-1");
        assert!(ws.active);
        assert!(ws.occupied);
    }
}
