//! Workspace backend built on `ext-workspace-v1`.
//!
//! For compositors without a native IPC backend (labwc, COSMIC, ...). Maps the
//! client's model onto vibepanel's workspace types.
//!
//! ext-workspace has no notion of windows, so this backend reports occupancy
//! as unknown (`occupancy_known = false`) and has no focused window or window
//! list.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use tracing::{debug, warn};

use super::client::{ExtWorkspace, ExtWorkspaceClient, ExtWorkspaceModel, MANAGER_INTERFACE};
use crate::services::compositor::{
    CompositorBackend, WindowCallback, WindowInfo, WorkspaceCallback, WorkspaceMeta,
    WorkspaceSnapshot,
};

/// Assigns numeric IDs to model keys. IDs are never reused; keys of
/// workspaces that no longer exist are pruned.
#[derive(Debug, Default)]
struct IdMap {
    by_key: HashMap<u64, i32>,
    next: i32,
}

impl IdMap {
    fn id(&mut self, key: u64) -> i32 {
        *self.by_key.entry(key).or_insert_with(|| {
            self.next += 1;
            self.next
        })
    }

    fn retain_keys(&mut self, keys: impl IntoIterator<Item = u64>) {
        let keep: HashSet<u64> = keys.into_iter().collect();
        self.by_key.retain(|key, _| keep.contains(key));
    }

    fn key(&self, id: i32) -> Option<u64> {
        self.by_key.iter().find(|(_, v)| **v == id).map(|(k, _)| *k)
    }
}

/// Convert an ext-workspace model into vibepanel's workspace types.
///
/// - Hidden workspaces are left out entirely.
/// - Workspaces in a group bound to exactly one output belong to that output
///   (niri-like); groups spanning several or no outputs are global.
/// - ext-workspace has no window information: occupancy is reported as
///   unknown, and no window counts are reported.
/// - Order: by group, then coordinates (row-major), then numeric name, then
///   name.
fn workspaces_from_model(
    model: &ExtWorkspaceModel,
    ids: &mut IdMap,
) -> (Vec<WorkspaceMeta>, WorkspaceSnapshot) {
    ids.retain_keys(model.workspaces.iter().map(|w| w.key));
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
        let id = ids.id(ws.key);
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
    ids: IdMap,
    workspaces: Vec<WorkspaceMeta>,
    snapshot: WorkspaceSnapshot,
}

/// Workspace backend for compositors that only offer ext-workspace-v1.
pub struct ExtWorkspaceBackend {
    state: Arc<RwLock<BackendState>>,
    client: Mutex<Option<ExtWorkspaceClient>>,
}

impl ExtWorkspaceBackend {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(BackendState::default())),
            client: Mutex::new(None),
        }
    }
}

impl Default for ExtWorkspaceBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl CompositorBackend for ExtWorkspaceBackend {
    fn start(&self, on_workspace_update: WorkspaceCallback, _on_window_update: WindowCallback) {
        if self.client.lock().is_some() {
            return;
        }
        let state = self.state.clone();
        let client = ExtWorkspaceClient::start(Arc::new(move |model| {
            let snapshot = {
                let mut state = state.write();
                let (workspaces, snapshot) = workspaces_from_model(&model, &mut state.ids);
                state.workspaces = workspaces;
                state.snapshot = snapshot.clone();
                snapshot
            };
            on_workspace_update(snapshot);
        }));
        if client.is_none() {
            warn!("ext-workspace backend: compositor does not offer {MANAGER_INTERFACE}");
        }
        *self.client.lock() = client;
    }

    fn stop(&self) {
        if let Some(client) = self.client.lock().take() {
            client.stop();
        }
    }

    fn list_workspaces(&self) -> Vec<WorkspaceMeta> {
        self.state.read().workspaces.clone()
    }

    fn get_workspace_snapshot(&self) -> WorkspaceSnapshot {
        self.state.read().snapshot.clone()
    }

    fn get_focused_window(&self) -> Option<WindowInfo> {
        None
    }

    fn switch_workspace(&self, workspace_id: i32) {
        let key = self.state.read().ids.key(workspace_id);
        debug!("ext-workspace: switch to workspace {workspace_id} (key {key:?})");
        if let (Some(key), Some(client)) = (key, self.client.lock().as_ref()) {
            client.activate(key);
        }
    }

    fn name(&self) -> &'static str {
        "ext-workspace"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::compositor::ext_workspace::client::ExtWorkspaceGroup;

    fn ws(key: u64, name: &str, group: Option<usize>) -> ExtWorkspace {
        ExtWorkspace {
            key,
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

        let mut ids = IdMap::default();
        let (metas, snap) = workspaces_from_model(&m, &mut ids);

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
    fn backend_ids_are_stable_and_never_reused() {
        let mut ids = IdMap::default();
        let m1 = model(vec![ws(1, "a", None), ws(2, "a", None)], &[]);
        let (first, _) = workspaces_from_model(&m1, &mut ids);
        let m2 = model(vec![ws(2, "a", None), ws(3, "a", None)], &[]);
        let (second, _) = workspaces_from_model(&m2, &mut ids);

        assert_ne!(first[0].id, first[1].id);
        assert_eq!(first[1].id, second[0].id);
        assert!(!first.iter().any(|m| m.id == second[1].id));
        assert_eq!(ids.key(first[0].id), None);
        assert_eq!(ids.key(second[1].id), Some(3));
    }

    #[test]
    fn backend_group_spanning_outputs_is_global() {
        let mut a = ws(1, "1", Some(0));
        a.active = true;
        let m = model(vec![a], &[&["DP-1", "HDMI-A-1"]]);
        let (metas, snap) = workspaces_from_model(&m, &mut IdMap::default());

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
            let mut w = ws((2 * y + x).into(), name, Some(0));
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
        let (metas, _) = workspaces_from_model(&m, &mut IdMap::default());
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
        let (metas, _) = workspaces_from_model(&m, &mut IdMap::default());
        let idx: HashMap<&str, i32> = metas.iter().map(|m| (m.name.as_str(), m.idx)).collect();
        assert_eq!((idx["1"], idx["web"]), (1, -1));
        assert_eq!((idx["chat"], idx["mail"]), (1, 2));
    }
}
