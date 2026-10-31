//! The generic backend, for compositors without a native IPC backend (labwc,
//! COSMIC, river, ...).
//!
//! Workspaces come from `ext-workspace-v1` and window titles from
//! wlr-foreign-toplevel-management; either may be missing. ext-workspace has
//! no notion of windows, so occupancy is reported as unknown
//! (`occupancy_known = false`) and there is no window list.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use tracing::{debug, info};

use super::ext_workspace::client::{
    ExtWorkspace, ExtWorkspaceClient, ExtWorkspaceModel, MANAGER_INTERFACE,
};
use super::foreign_toplevel::ForeignToplevelClient;
use crate::services::compositor::{
    CompositorBackend, WindowCallback, WindowInfo, WorkspaceCallback, WorkspaceMeta,
    WorkspaceSnapshot,
};

/// Convert an ext-workspace model into vibepanel's workspace types.
///
/// - Hidden workspaces are left out entirely.
/// - Workspaces in a group bound to exactly one output belong to that output
///   (niri-like); groups spanning several or no outputs are global.
/// - ext-workspace has no window information: occupancy is reported as
///   unknown, and no window counts are reported.
/// - Order: by group, then coordinates (row-major), then numeric name, then
///   name.
fn workspaces_from_model(model: &ExtWorkspaceModel) -> (Vec<WorkspaceMeta>, WorkspaceSnapshot) {
    let mut visible: Vec<&ExtWorkspace> = model.workspaces.iter().filter(|w| !w.hidden).collect();
    visible.sort_by(|a, b| {
        a.group
            .cmp(&b.group)
            // Coordinates are [x, y, ...] (e.g. Wayfire sends [column, row]);
            // compare the last dimension first to flatten grids row by row.
            .then_with(|| a.coordinates.iter().rev().cmp(b.coordinates.iter().rev()))
            .then_with(|| a.name.parse::<i64>().ok().cmp(&b.name.parse::<i64>().ok()))
            .then_with(|| a.name.cmp(&b.name))
    });

    let single_output = |group: Option<usize>| -> Option<String> {
        let outputs = &model.groups.get(group?)?.outputs;
        (outputs.len() == 1).then(|| outputs[0].clone())
    };

    let mut metas = Vec::new();
    let mut snapshot = WorkspaceSnapshot {
        occupancy_known: false,
        ..Default::default()
    };
    let numeric_name = |ws: &ExtWorkspace| ws.name.parse::<i32>().ok().filter(|n| *n > 0);
    let numbered_groups: HashSet<Option<usize>> = visible
        .iter()
        .filter(|ws| numeric_name(ws).is_some())
        .map(|ws| ws.group)
        .collect();
    let mut position_in_group: HashMap<Option<usize>, i32> = HashMap::new();

    for ws in visible {
        let id = ws.id;
        let position = position_in_group.entry(ws.group).or_insert(0);
        *position += 1;
        // Mixing name numbers and positions could produce duplicates, so a
        // group with numeric names labels the others by name (idx -1).
        let idx = match numeric_name(ws) {
            Some(n) => n,
            None if numbered_groups.contains(&ws.group) => -1,
            None => *position,
        };

        metas.push(WorkspaceMeta {
            id,
            idx,
            name: ws.name.clone(),
            output: single_output(ws.group),
        });

        if ws.active {
            snapshot.active_workspace.insert(id);
        }
        if ws.urgent {
            snapshot.urgent_workspaces.insert(id);
        }

        let outputs: &[String] = ws
            .group
            .and_then(|g| model.groups.get(g))
            .map(|g| g.outputs.as_slice())
            .unwrap_or_default();
        for output in outputs {
            let per_output = snapshot.per_output.entry(output.clone()).or_default();
            if ws.active {
                per_output.active_workspace.insert(id);
            }
        }
    }

    // Make sure every known output has an entry, even without workspaces.
    for group in &model.groups {
        for output in &group.outputs {
            snapshot.per_output.entry(output.clone()).or_default();
        }
    }

    (metas, snapshot)
}

#[derive(Default)]
struct BackendState {
    workspaces: Vec<WorkspaceMeta>,
    snapshot: WorkspaceSnapshot,
    /// Last focused update, including empty ones. Its output is the one that
    /// last had a focused window: a guess, as the protocol has no output focus.
    focused_window: Option<WindowInfo>,
}

/// The generic backend: ext-workspace workspaces and foreign-toplevel titles.
pub struct GenericBackend {
    state: Arc<RwLock<BackendState>>,
    client: Mutex<Option<ExtWorkspaceClient>>,
    toplevels: Mutex<Option<ForeignToplevelClient>>,
}

impl GenericBackend {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(BackendState::default())),
            client: Mutex::new(None),
            toplevels: Mutex::new(None),
        }
    }
}

impl Default for GenericBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl CompositorBackend for GenericBackend {
    fn start(&self, on_workspace_update: WorkspaceCallback, on_window_update: WindowCallback) {
        if self.client.lock().is_some() || self.toplevels.lock().is_some() {
            return;
        }
        let state = self.state.clone();
        let client = ExtWorkspaceClient::start(Arc::new(move |model| {
            let snapshot = {
                let mut state = state.write();
                let (workspaces, snapshot) = workspaces_from_model(&model);
                state.workspaces = workspaces;
                state.snapshot = snapshot.clone();
                snapshot
            };
            on_workspace_update(snapshot);
        }));
        if client.is_none() {
            info!("generic backend: compositor does not offer {MANAGER_INTERFACE}, no workspaces");
        }
        *self.client.lock() = client;

        let state = self.state.clone();
        let toplevels = ForeignToplevelClient::start(Arc::new(move |info| {
            if info.focused {
                state.write().focused_window = Some(info.clone());
            }
            on_window_update(info);
        }));
        *self.toplevels.lock() = toplevels;
    }

    fn stop(&self) {
        if let Some(client) = self.client.lock().take() {
            client.stop();
        }
        if let Some(toplevels) = self.toplevels.lock().take() {
            toplevels.stop();
        }
    }

    fn list_workspaces(&self) -> Vec<WorkspaceMeta> {
        self.state.read().workspaces.clone()
    }

    fn get_workspace_snapshot(&self) -> WorkspaceSnapshot {
        self.state.read().snapshot.clone()
    }

    fn get_focused_window(&self) -> Option<WindowInfo> {
        self.state
            .read()
            .focused_window
            .clone()
            .filter(|w| !w.title.is_empty() || !w.app_id.is_empty())
    }

    fn get_focused_output(&self) -> Option<String> {
        self.state.read().focused_window.as_ref()?.output.clone()
    }

    fn switch_workspace(&self, workspace_id: i32) {
        debug!("ext-workspace: switch to workspace {workspace_id}");
        if let Some(client) = self.client.lock().as_ref() {
            client.activate(workspace_id);
        }
    }

    fn name(&self) -> &'static str {
        "generic"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::compositor::ext_workspace::client::ExtWorkspaceGroup;

    fn ws(id: i32, name: &str, group: Option<usize>) -> ExtWorkspace {
        ExtWorkspace {
            id,
            name: name.into(),
            group,
            ..Default::default()
        }
    }

    fn model(workspaces: Vec<ExtWorkspace>, groups: &[&[&str]]) -> ExtWorkspaceModel {
        ExtWorkspaceModel {
            workspaces,
            groups: groups
                .iter()
                .map(|outputs| ExtWorkspaceGroup {
                    outputs: outputs.iter().map(|o| o.to_string()).collect(),
                })
                .collect(),
        }
    }

    #[test]
    fn backend_skips_hidden_and_marks_active_per_output() {
        let mut a = ws(1, "1", Some(0));
        a.active = true;
        let b = ws(2, "2", Some(0));
        let mut c = ws(3, "3", Some(0));
        c.hidden = true;
        let m = model(vec![b, c, a], &[&["DP-1"]]);

        let (metas, snap) = workspaces_from_model(&m);

        assert_eq!(
            metas.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec!["1", "2"]
        );
        assert!(metas.iter().all(|m| m.output.as_deref() == Some("DP-1")));
        let id1 = metas[0].id;
        assert_eq!(snap.active_workspace, HashSet::from([id1]));
        assert_eq!(
            snap.per_output["DP-1"].active_workspace,
            HashSet::from([id1])
        );
        assert!(snap.occupied_workspaces.is_empty());
        assert!(!snap.occupancy_known);
    }

    #[test]
    fn backend_group_spanning_outputs_is_global() {
        let mut a = ws(1, "1", Some(0));
        a.active = true;
        let m = model(vec![a], &[&["DP-1", "HDMI-A-1"]]);
        let (metas, snap) = workspaces_from_model(&m);

        assert_eq!(metas[0].output, None);
        assert!(
            snap.per_output["DP-1"]
                .active_workspace
                .contains(&metas[0].id)
        );
        assert!(
            snap.per_output["HDMI-A-1"]
                .active_workspace
                .contains(&metas[0].id)
        );
    }

    #[test]
    fn backend_flattens_grid_row_major() {
        // Wayfire-style [column, row] coordinates for a 2x2 grid.
        let cell = |name: &str, x: u32, y: u32| {
            let mut w = ws((2 * y + x) as i32, name, Some(0));
            w.coordinates = vec![x, y];
            w
        };
        let m = model(
            vec![
                cell("d", 1, 1),
                cell("c", 0, 1),
                cell("b", 1, 0),
                cell("a", 0, 0),
            ],
            &[&["DP-1"]],
        );
        let (metas, _) = workspaces_from_model(&m);
        assert_eq!(
            metas.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c", "d"]
        );
    }

    #[test]
    fn backend_index_from_name_or_position() {
        let m = model(
            vec![
                ws(1, "web", Some(0)),
                ws(2, "1", Some(0)),
                ws(3, "mail", Some(1)),
                ws(4, "chat", Some(1)),
            ],
            &[&["A"], &["B"]],
        );
        let (metas, _) = workspaces_from_model(&m);
        let idx: HashMap<&str, i32> = metas.iter().map(|m| (m.name.as_str(), m.idx)).collect();
        assert_eq!((idx["1"], idx["web"]), (1, -1));
        assert_eq!((idx["chat"], idx["mail"]), (1, 2));
    }
}
