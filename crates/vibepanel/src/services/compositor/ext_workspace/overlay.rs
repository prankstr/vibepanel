//! Merging ext-workspace state over a native compositor backend.
//!
//! Native IPC stays the primary source of workspace data. ext-workspace only
//! contributes what native IPC cannot express:
//!
//! - **Several active workspaces per output** (e.g. dwl/Mango-style tags on
//!   Hyprland via a plugin).
//! - **Hidden workspaces**, which are removed even if IPC reports them as
//!   occupied or active.
//!
//! Workspaces are matched by name (Hyprland sends no ext-workspace ids), and
//! always within a scope: per output, only groups shown on that output are
//! considered; globally, only an unambiguous group is used. Names that are
//! ambiguous within a scope are ignored rather than guessed.
//!
//! Everything here is pure; [`ExtOverlayState`] keeps native input separate
//! from the merged output so updates never accumulate.

use std::collections::{HashMap, HashSet};

use super::client::ExtWorkspaceModel;
use crate::services::compositor::{WorkspaceMeta, WorkspaceSnapshot};

/// ext-workspace state of one workspace group, by workspace name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverlayGroup {
    /// Connector names of the outputs the group is shown on.
    pub outputs: Vec<String>,
    /// Names of the group's workspaces that are not hidden.
    pub visible: HashSet<String>,
    /// Names of the group's active workspaces.
    pub active: HashSet<String>,
    /// Names of the group's hidden workspaces.
    pub hidden: HashSet<String>,
}

/// The part of the ext-workspace state that is merged over native IPC.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtWorkspaceOverlay {
    pub groups: Vec<OverlayGroup>,
}

impl ExtWorkspaceOverlay {
    pub fn from_model(model: &ExtWorkspaceModel) -> Self {
        let mut groups: Vec<OverlayGroup> = model
            .groups
            .iter()
            .map(|g| OverlayGroup {
                outputs: g.outputs.clone(),
                ..Default::default()
            })
            .collect();
        // Workspaces outside any group get their own output-less group.
        let mut ungrouped = OverlayGroup::default();

        for ws in model.workspaces.iter().filter(|w| !w.name.is_empty()) {
            let group = match ws.group.and_then(|g| groups.get_mut(g)) {
                Some(group) => group,
                None => &mut ungrouped,
            };
            if ws.hidden {
                group.hidden.insert(ws.name.clone());
            } else {
                group.visible.insert(ws.name.clone());
            }
            if ws.active {
                group.active.insert(ws.name.clone());
            }
        }
        if ungrouped != OverlayGroup::default() {
            groups.push(ungrouped);
        }
        Self { groups }
    }

    pub fn is_empty(&self) -> bool {
        self.groups
            .iter()
            .all(|g| g.active.is_empty() && g.hidden.is_empty())
    }
}

/// Map workspace names to IDs for the workspaces visible from `output`
/// (`None` = global scope), dropping names that are ambiguous in that scope.
fn name_to_id_map(metas: &[WorkspaceMeta], output: Option<&str>) -> HashMap<String, i32> {
    let mut map: HashMap<&str, Option<i32>> = HashMap::new();
    for meta in metas {
        if let Some(output) = output
            && meta.output.as_deref().is_some_and(|o| o != output)
        {
            continue;
        }
        map.entry(meta.name.as_str())
            .and_modify(|id| *id = None)
            .or_insert(Some(meta.id));
    }
    map.into_iter()
        .filter_map(|(name, id)| id.map(|id| (name.to_string(), id)))
        .collect()
}

#[derive(Debug, Default, PartialEq)]
struct Scope {
    /// Replacement active set, if ext-workspace agrees with IPC.
    active: Option<HashSet<i32>>,
    /// Workspaces to hide in this scope.
    hidden: HashSet<i32>,
}

/// Merge the given groups over one scope's IPC state.
///
/// - **Hidden:** a name counts as hidden if some group hides it and no group
///   in the scope shows it.
/// - **Active:** the ext active set of the one group that contains the
///   IPC-reported active workspace. If no group or several groups match, IPC
///   state is kept. This is a conservative heuristic for two independent
///   event streams, not synchronization: while both briefly disagree, the
///   IPC state wins.
fn merge_scope(
    name_to_id: &HashMap<String, i32>,
    groups: &[&OverlayGroup],
    ipc_active: &HashSet<i32>,
) -> Scope {
    let id_of = |name: &String| name_to_id.get(name).copied();

    let hidden: HashSet<i32> = groups
        .iter()
        .flat_map(|g| &g.hidden)
        .filter(|name| !groups.iter().any(|g| g.visible.contains(*name)))
        .filter_map(id_of)
        .collect();

    let ipc_active_names: Vec<&String> = name_to_id
        .iter()
        .filter(|(_, id)| ipc_active.contains(id))
        .map(|(name, _)| name)
        .collect();
    let mut matching = groups
        .iter()
        .filter(|g| ipc_active_names.iter().any(|name| g.active.contains(*name)));
    let group = match (matching.next(), matching.next()) {
        (Some(group), None) => Some(group),
        _ => None,
    };

    let active = group.map(|g| {
        g.active
            .iter()
            .filter_map(id_of)
            .chain(ipc_active.iter().copied())
            .filter(|id| !hidden.contains(id))
            .collect()
    });

    Scope { active, hidden }
}

fn remove_ids(
    snapshot_sets: [&mut HashSet<i32>; 2],
    counts: &mut HashMap<i32, u32>,
    ids: &HashSet<i32>,
) {
    for set in snapshot_sets {
        set.retain(|id| !ids.contains(id));
    }
    counts.retain(|id, _| !ids.contains(id));
}

/// Merge ext-workspace state over a native backend's metadata and snapshot.
///
/// Returns the effective metadata (hidden workspaces removed) and snapshot.
/// With an empty overlay, or one that agrees with IPC, the native input is
/// returned unchanged. Occupancy knowledge is always the native backend's.
pub fn merge(
    metas: &[WorkspaceMeta],
    snapshot: &WorkspaceSnapshot,
    overlay: &ExtWorkspaceOverlay,
) -> (Vec<WorkspaceMeta>, WorkspaceSnapshot) {
    let mut snap = snapshot.clone();
    if overlay.is_empty() {
        return (metas.to_vec(), snap);
    }

    // Global scope: all groups, unambiguous names only.
    let all_groups: Vec<&OverlayGroup> = overlay.groups.iter().collect();
    let global = merge_scope(
        &name_to_id_map(metas, None),
        &all_groups,
        &snapshot.active_workspace,
    );
    if let Some(active) = &global.active {
        snap.active_workspace = active.clone();
    }
    remove_ids(
        [&mut snap.active_workspace, &mut snap.occupied_workspaces],
        &mut snap.window_counts,
        &global.hidden,
    );
    snap.urgent_workspaces
        .retain(|id| !global.hidden.contains(id));

    // Per-output scopes: only groups shown on that output.
    let mut hidden_per_output: HashMap<String, HashSet<i32>> = HashMap::new();
    for (output, state) in snap.per_output.iter_mut() {
        let groups: Vec<&OverlayGroup> = overlay
            .groups
            .iter()
            .filter(|g| g.outputs.iter().any(|o| o == output))
            .collect();
        if groups.is_empty() {
            continue;
        }
        let scope = merge_scope(
            &name_to_id_map(metas, Some(output)),
            &groups,
            &state.active_workspace,
        );
        if let Some(active) = &scope.active {
            state.active_workspace = active.clone();
        }
        remove_ids(
            [&mut state.active_workspace, &mut state.occupied_workspaces],
            &mut state.window_counts,
            &scope.hidden,
        );
        if let Some(urgent) = state.urgent_workspaces.as_mut() {
            urgent.retain(|id| !scope.hidden.contains(id));
        }
        hidden_per_output.insert(output.clone(), scope.hidden);
    }

    let effective_metas = metas
        .iter()
        .filter(|meta| {
            let hidden = match meta.output.as_deref() {
                Some(output) => hidden_per_output.get(output).unwrap_or(&global.hidden),
                None => &global.hidden,
            };
            !hidden.contains(&meta.id)
        })
        .cloned()
        .collect();

    (effective_metas, snap)
}

/// Native input plus overlay, and the merged result derived from both.
///
/// Every update recomputes from the latest native input, never from the
/// previous merged result, so clearing the overlay restores native state.
#[derive(Debug, Default)]
pub struct ExtOverlayState {
    native_metas: Vec<WorkspaceMeta>,
    native_snapshot: Option<WorkspaceSnapshot>,
    overlay: ExtWorkspaceOverlay,
    effective: Option<(Vec<WorkspaceMeta>, WorkspaceSnapshot)>,
}

impl ExtOverlayState {
    pub fn set_native(&mut self, metas: Vec<WorkspaceMeta>, snapshot: WorkspaceSnapshot) {
        self.native_metas = metas;
        self.native_snapshot = Some(snapshot);
        self.recompute();
    }

    /// Returns whether the overlay changed.
    pub fn set_overlay(&mut self, overlay: ExtWorkspaceOverlay) -> bool {
        if self.overlay == overlay {
            return false;
        }
        self.overlay = overlay;
        self.recompute();
        true
    }

    fn recompute(&mut self) {
        self.effective = self
            .native_snapshot
            .as_ref()
            .map(|snapshot| merge(&self.native_metas, snapshot, &self.overlay));
    }

    /// Merged metadata, if native state has been received.
    pub fn metas(&self) -> Option<&[WorkspaceMeta]> {
        self.effective.as_ref().map(|(metas, _)| metas.as_slice())
    }

    /// Merged snapshot, if native state has been received.
    pub fn snapshot(&self) -> Option<&WorkspaceSnapshot> {
        self.effective.as_ref().map(|(_, snapshot)| snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::compositor::PerOutputState;
    use crate::services::compositor::ext_workspace::client::{ExtWorkspace, ExtWorkspaceGroup};

    fn names(list: &[&str]) -> HashSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn meta(id: i32, name: &str, output: Option<&str>) -> WorkspaceMeta {
        WorkspaceMeta {
            id,
            idx: id,
            name: name.into(),
            output: output.map(Into::into),
        }
    }

    fn global_metas(ids: &[i32]) -> Vec<WorkspaceMeta> {
        ids.iter()
            .map(|id| meta(*id, &id.to_string(), None))
            .collect()
    }

    /// IPC state like Hyprland reports it: one active workspace on `output`.
    fn ipc(output: &str, active: i32, occupied: &[i32]) -> WorkspaceSnapshot {
        let mut snap = WorkspaceSnapshot::default();
        snap.active_workspace.insert(active);
        let mut per_output = PerOutputState::default();
        per_output.active_workspace.insert(active);
        for id in occupied {
            snap.occupied_workspaces.insert(*id);
            snap.window_counts.insert(*id, 1);
            per_output.occupied_workspaces.insert(*id);
            per_output.window_counts.insert(*id, 1);
        }
        snap.per_output.insert(output.into(), per_output);
        snap
    }

    fn group(outputs: &[&str], visible: &[&str], active: &[&str], hidden: &[&str]) -> OverlayGroup {
        OverlayGroup {
            outputs: outputs.iter().map(|o| o.to_string()).collect(),
            visible: names(visible),
            active: names(active),
            hidden: names(hidden),
        }
    }

    fn ids(metas: &[WorkspaceMeta]) -> Vec<i32> {
        metas.iter().map(|m| m.id).collect()
    }

    #[test]
    fn marks_multiple_active_globally_and_per_output() {
        let metas = global_metas(&[1, 2, 3]);
        let overlay = ExtWorkspaceOverlay {
            groups: vec![group(&["DP-1"], &["1", "2", "3"], &["1", "2"], &[])],
        };
        let (_, snap) = merge(&metas, &ipc("DP-1", 2, &[2]), &overlay);

        assert_eq!(snap.active_workspace, HashSet::from([1, 2]));
        assert_eq!(
            snap.per_output["DP-1"].active_workspace,
            HashSet::from([1, 2])
        );
    }

    #[test]
    fn ignored_while_streams_disagree() {
        // IPC already switched to 3, ext-workspace still reports 1+2.
        let metas = global_metas(&[1, 2, 3]);
        let overlay = ExtWorkspaceOverlay {
            groups: vec![group(&["DP-1"], &["1", "2", "3"], &["1", "2"], &[])],
        };
        let (_, snap) = merge(&metas, &ipc("DP-1", 3, &[]), &overlay);

        assert_eq!(snap.active_workspace, HashSet::from([3]));
        assert_eq!(snap.per_output["DP-1"].active_workspace, HashSet::from([3]));
    }

    #[test]
    fn agreeing_overlay_is_noop() {
        let metas = global_metas(&[1, 2, 3]);
        let native = ipc("DP-1", 2, &[1, 2]);
        let overlay = ExtWorkspaceOverlay {
            groups: vec![group(&["DP-1"], &["1", "2"], &["2"], &[])],
        };
        let (out_metas, snap) = merge(&metas, &native, &overlay);

        assert_eq!(out_metas, metas);
        assert_eq!(snap, native);
    }

    #[test]
    fn hidden_wins_over_occupied_and_active() {
        let metas = global_metas(&[1, 2, 3]);
        let overlay = ExtWorkspaceOverlay {
            groups: vec![group(&["DP-1"], &["2"], &["2", "3"], &["1", "3"])],
        };
        let (out_metas, snap) = merge(&metas, &ipc("DP-1", 2, &[1, 2, 3]), &overlay);

        assert_eq!(ids(&out_metas), vec![2]);
        assert_eq!(snap.active_workspace, HashSet::from([2]));
        assert_eq!(snap.occupied_workspaces, HashSet::from([2]));
        assert!(!snap.window_counts.contains_key(&1));
        let out = &snap.per_output["DP-1"];
        assert_eq!(out.active_workspace, HashSet::from([2]));
        assert_eq!(out.occupied_workspaces, HashSet::from([2]));
    }

    #[test]
    fn hidden_name_on_one_output_does_not_hide_same_name_on_another() {
        // Per-output naming (niri-like): "1" exists on both outputs, hidden
        // only on A.
        let metas = vec![
            meta(10, "1", Some("A")),
            meta(11, "2", Some("A")),
            meta(20, "1", Some("B")),
        ];
        let mut native = ipc("A", 11, &[]);
        let mut b = PerOutputState::default();
        b.active_workspace.insert(20);
        native.per_output.insert("B".into(), b);
        let overlay = ExtWorkspaceOverlay {
            groups: vec![
                group(&["A"], &["2"], &["2"], &["1"]),
                group(&["B"], &["1"], &["1"], &[]),
            ],
        };
        let (out_metas, _) = merge(&metas, &native, &overlay);

        assert_eq!(ids(&out_metas), vec![11, 20]);
    }

    #[test]
    fn ambiguous_active_groups_keep_ipc_state() {
        // Two groups on the same output both claim the IPC-active workspace.
        let metas = global_metas(&[1, 2, 3]);
        let overlay = ExtWorkspaceOverlay {
            groups: vec![
                group(&["DP-1"], &["1", "2"], &["1", "2"], &[]),
                group(&["DP-1"], &["1", "3"], &["1", "3"], &[]),
            ],
        };
        let (_, snap) = merge(&metas, &ipc("DP-1", 1, &[]), &overlay);

        assert_eq!(snap.active_workspace, HashSet::from([1]));
        assert_eq!(snap.per_output["DP-1"].active_workspace, HashSet::from([1]));
    }

    #[test]
    fn preserves_native_occupancy_knowledge() {
        let metas = global_metas(&[1, 2]);
        let overlay = ExtWorkspaceOverlay {
            groups: vec![group(&["DP-1"], &["1", "2"], &["1", "2"], &[])],
        };
        let (_, snap) = merge(&metas, &ipc("DP-1", 1, &[1]), &overlay);
        assert!(snap.occupancy_known);
    }

    #[test]
    fn state_recomputes_from_native_input() {
        let mut state = ExtOverlayState::default();
        state.set_native(global_metas(&[1, 2, 3]), ipc("DP-1", 2, &[2]));
        state.set_overlay(ExtWorkspaceOverlay {
            groups: vec![group(&["DP-1"], &["1", "2"], &["1", "2"], &["3"])],
        });
        assert_eq!(
            state.snapshot().unwrap().active_workspace,
            HashSet::from([1, 2])
        );
        assert_eq!(ids(state.metas().unwrap()), vec![1, 2]);

        // New native update merges against the current overlay, not the old result.
        state.set_native(global_metas(&[1, 2, 3]), ipc("DP-1", 1, &[1]));
        assert_eq!(
            state.snapshot().unwrap().active_workspace,
            HashSet::from([1, 2])
        );

        // Clearing the overlay restores native state, including hidden metadata.
        assert!(state.set_overlay(ExtWorkspaceOverlay::default()));
        assert_eq!(
            state.snapshot().unwrap().active_workspace,
            HashSet::from([1])
        );
        assert_eq!(ids(state.metas().unwrap()), vec![1, 2, 3]);
        assert!(!state.set_overlay(ExtWorkspaceOverlay::default()));
    }

    #[test]
    fn from_model_keeps_groups_and_their_outputs() {
        let ws = |name: &str, group: Option<usize>, active: bool, hidden: bool| ExtWorkspace {
            key: format!("tok:{name}"),
            name: name.into(),
            active,
            hidden,
            group,
            ..Default::default()
        };
        let model = ExtWorkspaceModel {
            workspaces: vec![
                ws("1", Some(0), true, true),
                ws("1", Some(1), true, false),
                ws("x", None, false, true),
            ],
            groups: vec![
                ExtWorkspaceGroup {
                    outputs: vec!["A".into()],
                },
                ExtWorkspaceGroup {
                    outputs: vec!["B".into()],
                },
            ],
        };
        let overlay = ExtWorkspaceOverlay::from_model(&model);

        assert_eq!(overlay.groups.len(), 3);
        assert_eq!(overlay.groups[0].outputs, vec!["A".to_string()]);
        assert_eq!(overlay.groups[0].hidden, names(&["1"]));
        assert_eq!(overlay.groups[1].visible, names(&["1"]));
        assert!(overlay.groups[2].outputs.is_empty());
        assert_eq!(overlay.groups[2].hidden, names(&["x"]));
    }
}
