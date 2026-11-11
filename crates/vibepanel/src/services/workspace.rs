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
use super::compositor::{CompositorManager, PerOutputState, WorkspaceMeta, WorkspaceSnapshot};

/// Enriched workspace object for widget consumption.
///
/// Combines static metadata with dynamic state for convenient widget rendering.
#[derive(Debug, Clone)]
pub struct Workspace {
    /// Stable unique workspace ID (for identity, switching, and HashMap keys).
    pub id: i32,
    /// Meaningful user-facing numeric index when one exists.
    ///
    /// Negative values mean no meaningful index exists; widgets should then
    /// fall back to `name`.
    pub idx: i32,
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
}

impl Workspace {
    /// Create a workspace from metadata using global state.
    fn from_meta(meta: &WorkspaceMeta, snapshot: &WorkspaceSnapshot) -> Self {
        Self {
            id: meta.id,
            idx: meta.idx,
            name: meta.name.clone(),
            active: snapshot.active_workspace.contains(&meta.id),
            occupied: snapshot.occupied_workspaces.contains(&meta.id),
            urgent: snapshot.urgent_workspaces.contains(&meta.id),
            window_count: snapshot.window_counts.get(&meta.id).copied(),
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
        let (active, occupied, urgent, window_count) = if let Some(state) = per_output {
            (
                state.active_workspace.contains(&meta.id),
                state.occupied_workspaces.contains(&meta.id),
                state
                    .urgent_workspaces
                    .as_ref()
                    .unwrap_or(&snapshot.urgent_workspaces)
                    .contains(&meta.id),
                state.window_counts.get(&meta.id).copied(),
            )
        } else {
            (
                snapshot.active_workspace.contains(&meta.id),
                snapshot.occupied_workspaces.contains(&meta.id),
                snapshot.urgent_workspaces.contains(&meta.id),
                snapshot.window_counts.get(&meta.id).copied(),
            )
        };

        Self {
            id: meta.id,
            idx: meta.idx,
            name: meta.name.clone(),
            active,
            occupied,
            urgent,
            window_count,
        }
    }
}

fn active_output_overview_workspace<'a>(
    workspaces_meta: &'a [WorkspaceMeta],
    active_workspace: &HashSet<i32>,
) -> Option<&'a WorkspaceMeta> {
    workspaces_meta
        .iter()
        .find(|meta| is_overview_workspace_meta(meta) && active_workspace.contains(&meta.id))
}

fn is_overview_workspace_meta(meta: &WorkspaceMeta) -> bool {
    meta.id == 0 && meta.name.eq_ignore_ascii_case("overview")
}

fn workspaces_for_output(
    workspaces_meta: &[WorkspaceMeta],
    snapshot: &WorkspaceSnapshot,
    output_name: &str,
    output_state: &PerOutputState,
) -> Vec<Workspace> {
    if let Some(active_output_meta) =
        active_output_overview_workspace(workspaces_meta, &output_state.active_workspace)
    {
        return vec![Workspace::from_meta_per_output(
            active_output_meta,
            snapshot,
            output_name,
        )];
    }

    workspaces_meta
        .iter()
        .filter(|meta| {
            if is_overview_workspace_meta(meta) {
                return false;
            }

            // Include if workspace is global (output is None) or belongs to this output
            meta.output.is_none() || meta.output.as_deref() == Some(output_name)
        })
        .map(|meta| Workspace::from_meta_per_output(meta, snapshot, output_name))
        .collect()
}

/// Per-output workspace state for widget consumption.
///
/// Contains the workspace state specific to a single output/monitor,
/// with window counts and active state tailored to that output.
#[derive(Debug, Clone)]
pub struct PerOutputWorkspaces {
    /// Currently active workspace IDs on this output.
    /// Most compositors have a single active workspace, but MangoWC
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
    /// Most compositors have a single active workspace, but MangoWC
    /// supports viewing multiple tags simultaneously.
    pub active_workspace: HashSet<i32>,
    /// All workspaces with their current state.
    pub workspaces: Vec<Workspace>,
    /// Per-output workspace state for multi-monitor setups.
    /// Key is the output/monitor connector name (e.g., "eDP-1", "DP-1").
    pub per_output: HashMap<String, PerOutputWorkspaces>,
    /// Whether `occupied` / `window_count` reflect real window data. When
    /// false, widgets should show every listed workspace instead of
    /// filtering by occupancy.
    pub occupancy_known: bool,
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

    #[cfg(test)]
    pub(crate) fn replace_state_for_test(
        workspaces: Vec<WorkspaceMeta>,
        snapshot: WorkspaceSnapshot,
    ) {
        let service = Self::global();
        *service.workspaces.borrow_mut() = workspaces;
        *service.snapshot.borrow_mut() = snapshot;
        let service_snapshot = service.build_snapshot();
        service.callbacks.notify(&service_snapshot);
    }

    /// Register a callback to be invoked when workspace state changes.
    /// The callback is always executed on the GLib main loop.
    pub fn connect<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&WorkspaceServiceSnapshot) + 'static,
    {
        let id = self.callbacks.register(callback);

        // Immediately send current state so widgets can render.
        let snapshot = self.build_snapshot();
        self.callbacks.notify_single(id, &snapshot);
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
            let output_workspaces =
                workspaces_for_output(&workspaces_meta, &snapshot, output_name, output_state);

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
            workspaces,
            per_output,
            occupancy_known: snapshot.occupancy_known,
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
            idx: id,
            name: id.to_string(),
            output: None,
        }
    }

    fn make_meta_with_name(id: i32, name: &str) -> WorkspaceMeta {
        WorkspaceMeta {
            id,
            idx: id,
            name: name.to_string(),
            output: None,
        }
    }

    fn workspace_ids(workspaces: &[Workspace]) -> Vec<i32> {
        workspaces.iter().map(|workspace| workspace.id).collect()
    }

    fn ids(ids: &[i32]) -> HashSet<i32> {
        ids.iter().copied().collect()
    }

    fn global(active: &[i32], occupied: &[i32], urgent: &[i32]) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            active_workspace: ids(active),
            occupied_workspaces: ids(occupied),
            urgent_workspaces: ids(urgent),
            ..Default::default()
        }
    }

    fn output(active: &[i32], occupied: &[i32], urgent: Option<&[i32]>) -> PerOutputState {
        PerOutputState {
            active_workspace: ids(active),
            occupied_workspaces: ids(occupied),
            urgent_workspaces: urgent.map(ids),
            ..Default::default()
        }
    }

    fn with_outputs<const N: usize>(
        mut snapshot: WorkspaceSnapshot,
        outputs: [(&str, PerOutputState); N],
    ) -> WorkspaceSnapshot {
        for (name, state) in outputs {
            snapshot.per_output.insert(name.to_string(), state);
        }
        snapshot
    }

    #[test]
    fn from_meta_picks_output_or_global_state() {
        let mut counted = global(&[], &[1, 2], &[2]);
        counted.window_counts = HashMap::from([(1, 3), (2, 1)]);
        let mut counted_output = output(&[2], &[2], None);
        counted_output.window_counts = HashMap::from([(2, 5)]);
        // Global urgency on 1, but each output reports its own.
        let output_urgency = || {
            with_outputs(
                global(&[], &[], &[1]),
                [
                    ("eDP-1", output(&[], &[], Some(&[]))),
                    ("HDMI-A-1", output(&[], &[], Some(&[1]))),
                ],
            )
        };

        // (name, snapshot, output, [(id, active, occupied, urgent, window_count)])
        let cases = [
            (
                "no active",
                global(&[], &[], &[]),
                None,
                vec![(1, false, false, false, None)],
            ),
            (
                "single active",
                global(&[2], &[], &[]),
                None,
                vec![
                    (1, false, false, false, None),
                    (2, true, false, false, None),
                    (3, false, false, false, None),
                ],
            ),
            (
                "multiple active",
                global(&[1, 3, 5], &[], &[]),
                None,
                vec![
                    (1, true, false, false, None),
                    (2, false, false, false, None),
                    (3, true, false, false, None),
                    (4, false, false, false, None),
                    (5, true, false, false, None),
                ],
            ),
            (
                "occupied and urgent",
                counted,
                None,
                vec![
                    (1, false, true, false, Some(3)),
                    (2, false, true, true, Some(1)),
                    (3, false, false, false, None),
                ],
            ),
            (
                "output single active",
                with_outputs(WorkspaceSnapshot::default(), [("eDP-1", counted_output)]),
                Some("eDP-1"),
                vec![
                    (1, false, false, false, None),
                    (2, true, true, false, Some(5)),
                ],
            ),
            (
                "output without urgency ignores global urgency",
                output_urgency(),
                Some("eDP-1"),
                vec![(1, false, false, false, None)],
            ),
            (
                "output uses its own urgency",
                output_urgency(),
                Some("HDMI-A-1"),
                vec![(1, false, false, true, None)],
            ),
            (
                "output without urgency data falls back to global urgency",
                with_outputs(
                    global(&[], &[], &[1]),
                    [("eDP-1", PerOutputState::default())],
                ),
                Some("eDP-1"),
                vec![(1, false, false, true, None)],
            ),
            (
                "output multiple active",
                with_outputs(
                    WorkspaceSnapshot::default(),
                    [("DP-1", output(&[1, 3, 5], &[], None))],
                ),
                Some("DP-1"),
                vec![
                    (1, true, false, false, None),
                    (2, false, false, false, None),
                    (3, true, false, false, None),
                    (4, false, false, false, None),
                    (5, true, false, false, None),
                ],
            ),
            (
                "unknown output falls back to global",
                global(&[1], &[1], &[]),
                Some("HDMI-1"),
                vec![(1, true, true, false, None)],
            ),
        ];

        for (name, snapshot, output, expected) in cases {
            let actual: Vec<_> = expected
                .iter()
                .map(|&(id, ..)| {
                    let meta = make_meta(id);
                    let ws = match output {
                        Some(output) => Workspace::from_meta_per_output(&meta, &snapshot, output),
                        None => Workspace::from_meta(&meta, &snapshot),
                    };
                    (ws.id, ws.active, ws.occupied, ws.urgent, ws.window_count)
                })
                .collect();
            assert_eq!(actual, expected, "{name}");
        }
    }

    #[test]
    fn test_workspaces_for_output_replaces_tags_with_active_overview() {
        let workspaces_meta = vec![
            make_meta(1),
            make_meta(2),
            make_meta_with_name(0, "Overview"),
        ];
        let mut snapshot = WorkspaceSnapshot::default();
        let mut per_output_state = PerOutputState::default();
        per_output_state.active_workspace.insert(0);
        snapshot
            .per_output
            .insert("eDP-1".to_string(), per_output_state.clone());

        let workspaces =
            workspaces_for_output(&workspaces_meta, &snapshot, "eDP-1", &per_output_state);

        assert_eq!(workspace_ids(&workspaces), vec![0]);
        assert_eq!(workspaces[0].name, "Overview");
        assert!(workspaces[0].active);
    }

    #[test]
    fn test_workspaces_for_output_keeps_normal_tags_on_other_outputs() {
        let workspaces_meta = vec![
            make_meta(1),
            make_meta(2),
            make_meta_with_name(0, "Overview"),
        ];
        let mut snapshot = WorkspaceSnapshot::default();
        let mut per_output_state = PerOutputState::default();
        per_output_state.active_workspace.insert(2);
        snapshot
            .per_output
            .insert("DP-1".to_string(), per_output_state.clone());

        let workspaces =
            workspaces_for_output(&workspaces_meta, &snapshot, "DP-1", &per_output_state);

        assert_eq!(workspace_ids(&workspaces), vec![1, 2]);
        assert!(workspaces[1].active);
    }
}
