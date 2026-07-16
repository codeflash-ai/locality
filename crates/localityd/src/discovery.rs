//! Pure planning for checkpointed connector discovery batches.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use locality_connector::{
    BatchObservationChange, BatchObservationCompleteness, BatchObserveResult,
};
use locality_core::freshness::{FreshnessTier, RemoteVersion};
use locality_core::hydration::{HydrationReason, HydrationRequest};
use locality_core::journal::{JournalEntry, PushId};
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId, TreeEntry};
use locality_core::path_projection::{page_container_path, projection_namespace_root};
use locality_core::{LocalityError, LocalityResult};
use locality_store::{
    AutoSaveEnrollmentRecord, AutoSaveRepository, ConnectorStateRecord, DiscoveryCommit,
    EntityRecord, EntityRepository, FreshnessStateRecord, FreshnessStateRepository,
    JournalRepository, MountConfig, RemoteObservationRecord, RemoteObservationRepository,
    VirtualMutationRecord, VirtualMutationRepository, discovery_auto_save_candidate,
};
use serde::{Deserialize, Serialize};

use crate::freshness::parse_freshness_timestamp;

const DISCOVERY_REPLAY_TAG: &str = "localityd.discovery_replay";
const DISCOVERY_REPLAY_VERSION: i64 = 1;
const RECENT_LOCAL_EDIT_WINDOW_MS: u64 = 30_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveryPlan {
    commit: DiscoveryCommit,
    pub projection_actions: Vec<DiscoveryProjectionAction>,
    pub projection_components: Vec<DiscoveryProjectionComponent>,
    pub held: Vec<HeldDiscoveryItem>,
    pub post_commit: Vec<DiscoveryPostCommitAction>,
}

impl DiscoveryPlan {
    pub fn commit(&self) -> &DiscoveryCommit {
        &self.commit
    }

    pub fn into_commit(self) -> DiscoveryCommit {
        self.commit
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiscoveryProjectionAction {
    Create {
        entry: TreeEntry,
    },
    Move {
        remote_id: RemoteId,
        kind: EntityKind,
        from: PathBuf,
        to: PathBuf,
    },
    Delete {
        remote_id: RemoteId,
        kind: EntityKind,
        path: PathBuf,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProjectionStructuralChange {
    Create {
        remote_id: RemoteId,
        kind: EntityKind,
        path: PathBuf,
    },
    Move {
        remote_id: RemoteId,
        kind: EntityKind,
        from: PathBuf,
        to: PathBuf,
    },
    Delete {
        remote_id: RemoteId,
        kind: EntityKind,
        path: PathBuf,
    },
}

pub(crate) fn projection_structural_change(
    action: &DiscoveryProjectionAction,
) -> ProjectionStructuralChange {
    match action {
        DiscoveryProjectionAction::Create { entry } => ProjectionStructuralChange::Create {
            remote_id: entry.remote_id.clone(),
            kind: entry.kind.clone(),
            path: entry.path.clone(),
        },
        DiscoveryProjectionAction::Move {
            remote_id,
            kind,
            from,
            to,
        } => ProjectionStructuralChange::Move {
            remote_id: remote_id.clone(),
            kind: kind.clone(),
            from: from.clone(),
            to: to.clone(),
        },
        DiscoveryProjectionAction::Delete {
            remote_id,
            kind,
            path,
        } => ProjectionStructuralChange::Delete {
            remote_id: remote_id.clone(),
            kind: kind.clone(),
            path: path.clone(),
        },
    }
}

pub(crate) fn projection_action_covers_change(
    action: &DiscoveryProjectionAction,
    change: &ProjectionStructuralChange,
) -> bool {
    match (action, change) {
        (
            DiscoveryProjectionAction::Create { entry },
            ProjectionStructuralChange::Create {
                remote_id,
                kind,
                path,
            },
        ) => entry.remote_id == *remote_id && entry.kind == *kind && entry.path == *path,
        (
            DiscoveryProjectionAction::Delete {
                remote_id,
                kind,
                path,
            },
            ProjectionStructuralChange::Delete {
                remote_id: change_remote_id,
                kind: change_kind,
                path: change_path,
            },
        ) => {
            if remote_id == change_remote_id && kind == change_kind && path == change_path {
                return true;
            }
            let root = projection_namespace_root(change_kind, change_path);
            let candidate_root = projection_namespace_root(kind, path);
            candidate_root != root && root.starts_with(candidate_root)
        }
        (
            DiscoveryProjectionAction::Move {
                remote_id,
                kind,
                from,
                to,
            },
            ProjectionStructuralChange::Move {
                remote_id: change_remote_id,
                kind: change_kind,
                from: change_from,
                to: change_to,
            },
        ) => {
            if remote_id == change_remote_id
                && kind == change_kind
                && from == change_from
                && to == change_to
            {
                return true;
            }
            let source = projection_namespace_root(change_kind, change_from);
            let destination = projection_namespace_root(change_kind, change_to);
            let candidate_source = projection_namespace_root(kind, from);
            let candidate_destination = projection_namespace_root(kind, to);
            if paths_overlap(&candidate_source, &candidate_destination)
                || candidate_source == source
            {
                return false;
            }
            let namespace_maps_exactly =
                source.strip_prefix(&candidate_source).is_ok_and(|suffix| {
                    !suffix.as_os_str().is_empty()
                        && candidate_destination.join(suffix) == destination
                });
            let projected_path_maps_exactly = change_from
                .strip_prefix(&candidate_source)
                .is_ok_and(|suffix| {
                    !suffix.as_os_str().is_empty()
                        && candidate_destination.join(suffix) == *change_to
                });
            namespace_maps_exactly && projected_path_maps_exactly
        }
        _ => false,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryProjectionComponent {
    pub namespace_roots: Vec<PathBuf>,
    pub actions: Vec<DiscoveryProjectionAction>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscoverySafetySnapshot {
    pub active_child_refresh_namespace_roots: BTreeSet<PathBuf>,
}

pub fn build_discovery_projection_components(
    actions: &[DiscoveryProjectionAction],
) -> Vec<DiscoveryProjectionComponent> {
    group_discovery_projection_actions(actions)
        .into_iter()
        .map(|component| DiscoveryProjectionComponent {
            namespace_roots: component.namespace_roots,
            actions: component
                .actions
                .iter()
                .filter(|action| !projection_action_is_redundant(action, &component.actions))
                .cloned()
                .collect(),
        })
        .collect()
}

fn group_discovery_projection_actions(
    actions: &[DiscoveryProjectionAction],
) -> Vec<DiscoveryProjectionComponent> {
    let mut actions = actions.to_vec();
    actions.sort_by(compare_projection_actions);
    let action_roots = actions
        .iter()
        .map(projection_action_namespace_roots)
        .collect::<Vec<_>>();
    let mut parents = (0..actions.len()).collect::<Vec<_>>();

    for left in 0..actions.len() {
        for right in (left + 1)..actions.len() {
            if action_roots[left].iter().any(|left_root| {
                action_roots[right]
                    .iter()
                    .any(|right_root| paths_overlap(left_root, right_root))
            }) {
                union_components(&mut parents, left, right);
            }
        }
    }

    let mut grouped = BTreeMap::<usize, Vec<usize>>::new();
    for index in 0..actions.len() {
        let root = find_component(&mut parents, index);
        grouped.entry(root).or_default().push(index);
    }

    let mut components = grouped
        .into_values()
        .map(|indices| {
            let namespace_roots = minimal_namespace_roots(
                indices
                    .iter()
                    .flat_map(|index| action_roots[*index].iter().cloned())
                    .collect(),
            );
            let actions = indices
                .into_iter()
                .map(|index| actions[index].clone())
                .collect::<Vec<_>>();
            DiscoveryProjectionComponent {
                namespace_roots,
                actions,
            }
        })
        .collect::<Vec<_>>();
    components.sort_by(|left, right| {
        left.namespace_roots
            .cmp(&right.namespace_roots)
            .then_with(|| compare_action_slices(&left.actions, &right.actions))
    });
    components
}

fn projection_action_namespace_roots(action: &DiscoveryProjectionAction) -> Vec<PathBuf> {
    let mut roots = match action {
        DiscoveryProjectionAction::Create { entry } => {
            vec![projection_namespace_root(&entry.kind, &entry.path)]
        }
        DiscoveryProjectionAction::Move { kind, from, to, .. } => vec![
            projection_namespace_root(kind, from),
            projection_namespace_root(kind, to),
        ],
        DiscoveryProjectionAction::Delete { kind, path, .. } => {
            vec![projection_namespace_root(kind, path)]
        }
    };
    roots.sort();
    roots.dedup();
    roots
}

fn projection_action_remote_id(action: &DiscoveryProjectionAction) -> &RemoteId {
    match action {
        DiscoveryProjectionAction::Create { entry } => &entry.remote_id,
        DiscoveryProjectionAction::Move { remote_id, .. }
        | DiscoveryProjectionAction::Delete { remote_id, .. } => remote_id,
    }
}

fn compare_projection_actions(
    left: &DiscoveryProjectionAction,
    right: &DiscoveryProjectionAction,
) -> std::cmp::Ordering {
    projection_action_remote_id(left)
        .0
        .cmp(&projection_action_remote_id(right).0)
        .then_with(|| projection_action_rank(left).cmp(&projection_action_rank(right)))
        .then_with(|| {
            projection_action_namespace_roots(left).cmp(&projection_action_namespace_roots(right))
        })
}

fn projection_action_rank(action: &DiscoveryProjectionAction) -> u8 {
    match action {
        DiscoveryProjectionAction::Create { .. } => 0,
        DiscoveryProjectionAction::Move { .. } => 1,
        DiscoveryProjectionAction::Delete { .. } => 2,
    }
}

fn compare_action_slices(
    left: &[DiscoveryProjectionAction],
    right: &[DiscoveryProjectionAction],
) -> std::cmp::Ordering {
    for (left, right) in left.iter().zip(right) {
        let ordering = compare_projection_actions(left, right);
        if !ordering.is_eq() {
            return ordering;
        }
    }
    left.len().cmp(&right.len())
}

fn find_component(parents: &mut [usize], index: usize) -> usize {
    if parents[index] != index {
        parents[index] = find_component(parents, parents[index]);
    }
    parents[index]
}

fn union_components(parents: &mut [usize], left: usize, right: usize) {
    let left_root = find_component(parents, left);
    let right_root = find_component(parents, right);
    if left_root != right_root {
        parents[right_root] = left_root.min(right_root);
        parents[left_root] = left_root.min(right_root);
    }
}

fn minimal_namespace_roots(mut roots: Vec<PathBuf>) -> Vec<PathBuf> {
    roots.sort();
    roots.dedup();
    roots
        .iter()
        .filter(|root| {
            !roots
                .iter()
                .any(|candidate| candidate != *root && root.starts_with(candidate))
        })
        .cloned()
        .collect()
}

fn projection_action_is_redundant(
    action: &DiscoveryProjectionAction,
    component: &[DiscoveryProjectionAction],
) -> bool {
    let change = projection_structural_change(action);
    component
        .iter()
        .any(|candidate| candidate != action && projection_action_covers_change(candidate, &change))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscoveryPostCommitAction {
    QueueHydration(HydrationRequest),
    InvalidateProvider {
        mount_id: MountId,
        paths: Vec<PathBuf>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldDiscoveryItem {
    pub remote_id: RemoteId,
    pub change: DiscoveryChangeKind,
    pub reason: DiscoveryHoldReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscoveryChangeKind {
    Create,
    Move,
    Delete,
    RemoteDrift,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscoveryHoldReason {
    Dirty,
    Conflicted,
    PendingVirtualMutation { local_id: String },
    UnsettledJournal { push_id: PushId },
    RecentLocalEdit { remote_id: RemoteId },
    ActiveChildRefresh { namespace_root: PathBuf },
    UnsupportedKindChange { from: EntityKind, to: EntityKind },
    UntrackedSource(PathBuf),
    UntrackedDestination(PathBuf),
    UnknownSubtree(PathBuf),
    IncompleteSubtree(PathBuf),
    HostCollision(PathBuf),
    UnknownProjection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectionAssessment {
    Safe,
    Blocked(DiscoveryHoldReason),
}

#[allow(clippy::too_many_arguments)]
pub fn plan_batch_discovery<S>(
    store: &S,
    mount: &MountConfig,
    batch: BatchObserveResult,
    observed_at: &str,
    metadata_job_id: Option<&str>,
    assessments: &BTreeMap<RemoteId, ProjectionAssessment>,
    safety_snapshot: &DiscoverySafetySnapshot,
) -> LocalityResult<DiscoveryPlan>
where
    S: EntityRepository
        + RemoteObservationRepository
        + FreshnessStateRepository
        + AutoSaveRepository
        + VirtualMutationRepository
        + JournalRepository,
{
    if metadata_job_id.is_some_and(str::is_empty) {
        return invalid("discovery metadata job identifier cannot be empty");
    }
    validate_batch(mount, &batch)?;
    let observed_at_ms = parse_freshness_timestamp(observed_at).ok_or_else(|| {
        LocalityError::InvalidState(
            "discovery observed_at must be a parseable freshness timestamp".to_string(),
        )
    })?;
    let existing = store
        .list_entities(&mount.mount_id)
        .map_err(LocalityError::from)?
        .into_iter()
        .map(|entity| (entity.remote_id.clone(), entity))
        .collect::<BTreeMap<_, _>>();
    let existing_freshness = store
        .list_freshness_states(&mount.mount_id)
        .map_err(LocalityError::from)?
        .into_iter()
        .map(|freshness| (freshness.remote_id.clone(), freshness))
        .collect::<BTreeMap<_, _>>();
    let existing_observations = store
        .list_remote_observations(&mount.mount_id)
        .map_err(LocalityError::from)?
        .into_iter()
        .map(|observation| (observation.remote_id.clone(), observation))
        .collect::<BTreeMap<_, _>>();
    let auto_save_enrollments = store
        .list_auto_save_enrollments(&mount.mount_id)
        .map_err(LocalityError::from)?;
    let mut virtual_mutations = store
        .list_virtual_mutations(&mount.mount_id)
        .map_err(LocalityError::from)?;
    virtual_mutations.sort_by(|left, right| left.local_id.cmp(&right.local_id));
    let mut journals = store.list_journal().map_err(LocalityError::from)?;
    journals.sort_by(|left, right| left.push_id.0.cmp(&right.push_id.0));
    let mut intents = BTreeMap::new();
    let mut incoming_ids = BTreeSet::new();
    for change in batch.changes {
        match change {
            BatchObservationChange::Upsert(entry) => {
                incoming_ids.insert(entry.remote_id.clone());
                intents.insert(entry.remote_id.clone(), DiscoveryIntent::Upsert(entry));
            }
            BatchObservationChange::Tombstone { remote_id } => {
                incoming_ids.insert(remote_id.clone());
                intents.insert(remote_id, DiscoveryIntent::Tombstone);
            }
        }
    }
    let mut recognized_pending_ids = BTreeSet::new();
    for (remote_id, freshness) in &existing_freshness {
        if !freshness.remote_hint_pending {
            continue;
        }
        let Some(observation) = existing_observations.get(remote_id) else {
            continue;
        };
        if !is_replay_envelope(&observation.raw_metadata_json) {
            continue;
        }
        recognized_pending_ids.insert(remote_id.clone());
        if incoming_ids.contains(remote_id) {
            continue;
        }
        if batch.completeness == BatchObservationCompleteness::Complete {
            continue;
        }
        let Some(change) = decode_replay(&observation.raw_metadata_json)? else {
            continue;
        };
        let intent = match change {
            ReplayChange::Upsert { entry } => {
                validate_replayed_entry(mount, remote_id, &entry)?;
                DiscoveryIntent::Upsert(entry)
            }
            ReplayChange::Tombstone {
                remote_id: replayed_id,
            } => {
                if &replayed_id != remote_id {
                    return invalid(format!(
                        "discovery replay tombstone `{}` does not match observation `{}`",
                        replayed_id.0, remote_id.0
                    ));
                }
                DiscoveryIntent::Tombstone
            }
        };
        intents.insert(remote_id.clone(), intent);
    }
    if batch.completeness == BatchObservationCompleteness::Complete {
        for remote_id in existing.keys().chain(recognized_pending_ids.iter()) {
            if !incoming_ids.contains(remote_id) {
                intents.insert(remote_id.clone(), DiscoveryIntent::Tombstone);
            }
        }
    }
    validate_merged_intent_paths(&intents)?;
    let prospective_actions = prospective_projection_actions(&intents, &existing);
    let component_holds = projection_component_holds(
        &prospective_actions,
        &intents,
        &existing,
        &existing_freshness,
        &virtual_mutations,
        &journals,
        assessments,
        safety_snapshot,
        &mount.mount_id,
        observed_at_ms,
    );

    let mut entity_upserts = Vec::new();
    let mut entity_deletes = Vec::new();
    let mut observation_upserts = Vec::new();
    let mut freshness_upserts = Vec::new();
    let mut auto_save_upserts = Vec::new();
    let mut projection_actions = Vec::new();
    let mut held = Vec::new();
    let mut post_commit = Vec::new();
    for (remote_id, intent) in intents {
        let current = existing.get(&remote_id);
        match intent {
            DiscoveryIntent::Upsert(entry) => {
                if current.is_none()
                    && !matches!(
                        entry.hydration,
                        HydrationState::Virtual | HydrationState::Stub
                    )
                {
                    return invalid(format!(
                        "discovery create `{}` has unsupported hydration state `{:?}`",
                        entry.remote_id.0, entry.hydration
                    ));
                }
                let change_kind = match current {
                    None => DiscoveryChangeKind::Create,
                    Some(current) if current.path != entry.path => DiscoveryChangeKind::Move,
                    Some(_) => DiscoveryChangeKind::RemoteDrift,
                };
                let structural = change_kind != DiscoveryChangeKind::RemoteDrift;
                let remote_changed =
                    current.is_none_or(|current| entity_remote_changed(current, &entry));
                if structural {
                    discovery_auto_save_candidate(
                        &auto_save_enrollments,
                        &remote_id,
                        Some(current.map_or(entry.path.as_path(), |entity| entity.path.as_path())),
                    )
                    .map_err(LocalityError::from)?;
                }
                let changed_paths = match current {
                    Some(current) => {
                        vec![current.path.as_path(), entry.path.as_path()]
                    }
                    None => vec![entry.path.as_path()],
                };
                let hold_reason = if structural {
                    component_holds.get(&remote_id).cloned()
                } else {
                    remote_changed
                        .then(|| {
                            pending_virtual_mutation_hold_reason(
                                &virtual_mutations,
                                &remote_id,
                                current.map_or(&entry.kind, |entity| &entity.kind),
                                &changed_paths,
                            )
                        })
                        .flatten()
                        .or_else(|| {
                            remote_changed
                                .then(|| entity_hold_reason(current))
                                .flatten()
                        })
                        .or_else(|| kind_change_hold_reason(current, &entry))
                };
                observation_upserts.push(observation_for_entry(&entry, observed_at)?);
                let accepted_remote_metadata = hold_reason.is_none()
                    && current.is_none_or(|current| {
                        matches!(
                            current.hydration,
                            HydrationState::Virtual | HydrationState::Stub
                        )
                    });
                let remote_hint_pending =
                    hold_reason.is_some() || (remote_changed && !accepted_remote_metadata);
                freshness_upserts.push(next_freshness(
                    &mount.mount_id,
                    &remote_id,
                    current,
                    existing_freshness.get(&remote_id),
                    observed_at,
                    remote_hint_pending,
                ));

                if let Some(reason) = hold_reason {
                    held.push(HeldDiscoveryItem {
                        remote_id: remote_id.clone(),
                        change: change_kind,
                        reason,
                    });
                    pause_auto_save(
                        &auto_save_enrollments,
                        current,
                        current.map(|entity| entity.path.as_path()),
                        observed_at,
                        "remote discovery is held for local review",
                        &mut auto_save_upserts,
                    )?;
                    continue;
                }

                if remote_hint_pending
                    && current.is_some_and(|entity| entity.hydration == HydrationState::Hydrated)
                {
                    let current = current.expect("hydrated entity is present");
                    let final_path = if current.path == entry.path {
                        current.path.as_path()
                    } else {
                        entry.path.as_path()
                    };
                    pause_auto_save(
                        &auto_save_enrollments,
                        Some(current),
                        Some(final_path),
                        observed_at,
                        "remote discovery is awaiting hydration",
                        &mut auto_save_upserts,
                    )?;
                }

                match current {
                    None => {
                        entity_upserts.push(EntityRecord::from(entry.clone()));
                        projection_actions.push(DiscoveryProjectionAction::Create { entry });
                    }
                    Some(current) if current.path != entry.path => {
                        let updated = accepted_entity_record(current, &entry);
                        projection_actions.push(DiscoveryProjectionAction::Move {
                            remote_id: remote_id.clone(),
                            kind: current.kind.clone(),
                            from: current.path.clone(),
                            to: entry.path.clone(),
                        });
                        if current.hydration == HydrationState::Hydrated && remote_changed {
                            post_commit.push(queue_hydration(mount, &updated));
                        }
                        entity_upserts.push(updated);
                    }
                    Some(current)
                        if matches!(
                            current.hydration,
                            HydrationState::Virtual | HydrationState::Stub
                        ) =>
                    {
                        entity_upserts.push(accepted_entity_record(current, &entry));
                    }
                    Some(current) => {
                        if current.hydration == HydrationState::Hydrated && remote_changed {
                            post_commit.push(queue_hydration(mount, current));
                        }
                    }
                }
            }
            DiscoveryIntent::Tombstone => {
                let Some(current) = current else {
                    if recognized_pending_ids.contains(&remote_id) {
                        entity_deletes.push(remote_id);
                    }
                    continue;
                };
                discovery_auto_save_candidate(
                    &auto_save_enrollments,
                    &remote_id,
                    Some(&current.path),
                )
                .map_err(LocalityError::from)?;
                let hold_reason = component_holds.get(&remote_id).cloned();
                if let Some(reason) = hold_reason {
                    observation_upserts.push(observation_for_tombstone(current, observed_at)?);
                    freshness_upserts.push(next_freshness(
                        &mount.mount_id,
                        &remote_id,
                        Some(current),
                        existing_freshness.get(&remote_id),
                        observed_at,
                        true,
                    ));
                    pause_auto_save(
                        &auto_save_enrollments,
                        Some(current),
                        Some(current.path.as_path()),
                        observed_at,
                        "remote discovery is held for local review",
                        &mut auto_save_upserts,
                    )?;
                    held.push(HeldDiscoveryItem {
                        remote_id,
                        change: DiscoveryChangeKind::Delete,
                        reason,
                    });
                    continue;
                }
                entity_deletes.push(remote_id.clone());
                projection_actions.push(DiscoveryProjectionAction::Delete {
                    remote_id,
                    kind: current.kind.clone(),
                    path: current.path.clone(),
                });
            }
        }
    }
    let projection_components = build_discovery_projection_components(&projection_actions);
    let projection_actions = projection_components
        .iter()
        .flat_map(|component| component.actions.iter().cloned())
        .collect();

    let checkpoint = ConnectorStateRecord {
        connector: mount.connector.clone(),
        scope_kind: "mount".to_string(),
        scope_id: mount.mount_id.0.clone(),
        state_version: batch.next_checkpoint.state_version,
        min_reader_version: batch.next_checkpoint.min_reader_version,
        state_json: batch.next_checkpoint.state_json,
        updated_at: observed_at.to_string(),
    };
    if checkpoint.state_version <= 0 {
        return Err(LocalityError::InvalidState(
            "discovery checkpoint state version must be positive".to_string(),
        ));
    }

    let commit = DiscoveryCommit {
        mount_id: mount.mount_id.clone(),
        entity_upserts,
        entity_deletes,
        observation_upserts,
        freshness_upserts,
        auto_save_upserts,
        metadata_discovery_deletes: metadata_job_id.map(str::to_string).into_iter().collect(),
        virtual_mutation_deletes: Vec::new(),
        checkpoint,
    };
    let existing_entities = existing.values().cloned().collect::<Vec<_>>();
    commit
        .preflight(
            &mount.connector,
            &existing_entities,
            &auto_save_enrollments,
            &virtual_mutations,
        )
        .map_err(LocalityError::from)?;

    Ok(DiscoveryPlan {
        commit,
        projection_actions,
        projection_components,
        held,
        post_commit,
    })
}

#[derive(Clone, Debug)]
enum DiscoveryIntent {
    Upsert(TreeEntry),
    Tombstone,
}

fn prospective_projection_actions(
    intents: &BTreeMap<RemoteId, DiscoveryIntent>,
    existing: &BTreeMap<RemoteId, EntityRecord>,
) -> Vec<DiscoveryProjectionAction> {
    intents
        .iter()
        .filter_map(|(remote_id, intent)| match intent {
            DiscoveryIntent::Upsert(entry) => match existing.get(remote_id) {
                None => Some(DiscoveryProjectionAction::Create {
                    entry: entry.clone(),
                }),
                Some(current) if current.path != entry.path => {
                    Some(DiscoveryProjectionAction::Move {
                        remote_id: remote_id.clone(),
                        kind: current.kind.clone(),
                        from: current.path.clone(),
                        to: entry.path.clone(),
                    })
                }
                Some(_) => None,
            },
            DiscoveryIntent::Tombstone => {
                existing
                    .get(remote_id)
                    .map(|current| DiscoveryProjectionAction::Delete {
                        remote_id: remote_id.clone(),
                        kind: current.kind.clone(),
                        path: current.path.clone(),
                    })
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn projection_component_holds(
    actions: &[DiscoveryProjectionAction],
    intents: &BTreeMap<RemoteId, DiscoveryIntent>,
    existing: &BTreeMap<RemoteId, EntityRecord>,
    freshness: &BTreeMap<RemoteId, FreshnessStateRecord>,
    mutations: &[VirtualMutationRecord],
    journals: &[JournalEntry],
    assessments: &BTreeMap<RemoteId, ProjectionAssessment>,
    safety_snapshot: &DiscoverySafetySnapshot,
    mount_id: &MountId,
    observed_at_ms: u64,
) -> BTreeMap<RemoteId, DiscoveryHoldReason> {
    let mut holds = BTreeMap::new();
    for component in group_discovery_projection_actions(actions) {
        let action_ids = component
            .actions
            .iter()
            .map(projection_action_remote_id)
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut expanded_ids = action_ids.clone();
        for entity in existing.values() {
            let entity_root = projection_namespace_root(&entity.kind, &entity.path);
            if component
                .namespace_roots
                .iter()
                .any(|root| paths_overlap(root, &entity_root))
            {
                expanded_ids.insert(entity.remote_id.clone());
            }
        }
        let expanded_ids = expanded_ids.into_iter().collect::<Vec<_>>();
        let reason = projection_component_hold_reason(
            &component,
            &action_ids,
            &expanded_ids,
            intents,
            existing,
            freshness,
            mutations,
            journals,
            assessments,
            safety_snapshot,
            mount_id,
            observed_at_ms,
        );
        if let Some(reason) = reason {
            for remote_id in action_ids {
                holds.insert(remote_id, reason.clone());
            }
        }
    }
    holds
}

#[allow(clippy::too_many_arguments)]
fn projection_component_hold_reason(
    component: &DiscoveryProjectionComponent,
    action_ids: &BTreeSet<RemoteId>,
    expanded_ids: &[RemoteId],
    intents: &BTreeMap<RemoteId, DiscoveryIntent>,
    existing: &BTreeMap<RemoteId, EntityRecord>,
    freshness: &BTreeMap<RemoteId, FreshnessStateRecord>,
    mutations: &[VirtualMutationRecord],
    journals: &[JournalEntry],
    assessments: &BTreeMap<RemoteId, ProjectionAssessment>,
    safety_snapshot: &DiscoverySafetySnapshot,
    mount_id: &MountId,
    observed_at_ms: u64,
) -> Option<DiscoveryHoldReason> {
    if let Some(mutation) = mutations.iter().find(|mutation| {
        mutation_intersects_projection_component(mutation, expanded_ids, &component.namespace_roots)
    }) {
        return Some(DiscoveryHoldReason::PendingVirtualMutation {
            local_id: mutation.local_id.clone(),
        });
    }

    if let Some(journal) = journals.iter().find(|journal| {
        journal.mount_id == *mount_id
            && journal.status.is_unsettled()
            && journal.touches_any_entity(expanded_ids)
    }) {
        return Some(DiscoveryHoldReason::UnsettledJournal {
            push_id: journal.push_id.clone(),
        });
    }

    if let Some(remote_id) = expanded_ids.iter().find(|remote_id| {
        freshness
            .get(*remote_id)
            .and_then(|state| state.last_local_change_at.as_deref())
            .is_some_and(|timestamp| local_edit_is_recent_or_uncertain(timestamp, observed_at_ms))
    }) {
        return Some(DiscoveryHoldReason::RecentLocalEdit {
            remote_id: remote_id.clone(),
        });
    }

    if let Some(namespace_root) = safety_snapshot
        .active_child_refresh_namespace_roots
        .iter()
        .find(|active_root| {
            component
                .namespace_roots
                .iter()
                .any(|root| paths_overlap(root, active_root))
        })
    {
        return Some(DiscoveryHoldReason::ActiveChildRefresh {
            namespace_root: namespace_root.clone(),
        });
    }

    if expanded_ids.iter().any(|remote_id| {
        existing
            .get(remote_id)
            .is_some_and(|entity| entity.hydration == HydrationState::Dirty)
    }) {
        return Some(DiscoveryHoldReason::Dirty);
    }
    if expanded_ids.iter().any(|remote_id| {
        existing
            .get(remote_id)
            .is_some_and(|entity| entity.hydration == HydrationState::Conflicted)
    }) {
        return Some(DiscoveryHoldReason::Conflicted);
    }

    for remote_id in action_ids {
        let Some(DiscoveryIntent::Upsert(entry)) = intents.get(remote_id) else {
            continue;
        };
        if let Some(reason) = kind_change_hold_reason(existing.get(remote_id), entry) {
            return Some(reason);
        }
    }

    for remote_id in action_ids {
        if let Some(ProjectionAssessment::Blocked(reason)) = assessments.get(remote_id) {
            return Some(reason.clone());
        }
    }
    if action_ids
        .iter()
        .any(|remote_id| !assessments.contains_key(remote_id))
    {
        return Some(DiscoveryHoldReason::UnknownProjection);
    }
    None
}

fn mutation_intersects_projection_component(
    mutation: &VirtualMutationRecord,
    expanded_ids: &[RemoteId],
    namespace_roots: &[PathBuf],
) -> bool {
    mutation
        .target_remote_id
        .as_ref()
        .is_some_and(|remote_id| expanded_ids.contains(remote_id))
        || mutation
            .parent_remote_id
            .as_ref()
            .is_some_and(|remote_id| expanded_ids.contains(remote_id))
        || mutation
            .original_path
            .as_ref()
            .is_some_and(|path| mutation_path_overlaps_component(path, namespace_roots))
        || mutation_path_overlaps_component(&mutation.projected_path, namespace_roots)
}

fn mutation_path_overlaps_component(path: &Path, namespace_roots: &[PathBuf]) -> bool {
    let mutation_root = page_container_path(path);
    namespace_roots
        .iter()
        .any(|root| paths_overlap(root, path) || paths_overlap(root, &mutation_root))
}

fn mutation_path_overlaps_projection(
    kind: &EntityKind,
    projection_path: &Path,
    mutation_path: &Path,
) -> bool {
    let namespace_root = projection_namespace_root(kind, projection_path);
    mutation_path_overlaps_component(mutation_path, std::slice::from_ref(&namespace_root))
}

fn local_edit_is_recent_or_uncertain(timestamp: &str, observed_at_ms: u64) -> bool {
    parse_freshness_timestamp(timestamp).is_none_or(|last_local_change_ms| {
        last_local_change_ms > observed_at_ms
            || observed_at_ms - last_local_change_ms < RECENT_LOCAL_EDIT_WINDOW_MS
    })
}

fn validate_merged_intent_paths(
    intents: &BTreeMap<RemoteId, DiscoveryIntent>,
) -> LocalityResult<()> {
    let mut paths = BTreeSet::new();
    for intent in intents.values() {
        let DiscoveryIntent::Upsert(entry) = intent else {
            continue;
        };
        if !paths.insert(entry.path.clone()) {
            return invalid(format!(
                "discovery merged intents contain duplicate projected path `{}`",
                entry.path.display()
            ));
        }
    }
    Ok(())
}

fn validate_batch(mount: &MountConfig, batch: &BatchObserveResult) -> LocalityResult<()> {
    let checkpoint = &batch.next_checkpoint;
    if checkpoint.state_version <= 0 || checkpoint.min_reader_version <= 0 {
        return invalid("discovery checkpoint versions must be positive");
    }
    if checkpoint.min_reader_version > checkpoint.state_version {
        return invalid("discovery checkpoint minimum reader version exceeds state version");
    }
    serde_json::from_str::<serde_json::Value>(&checkpoint.state_json).map_err(|error| {
        LocalityError::InvalidState(format!("discovery checkpoint JSON is invalid: {error}"))
    })?;

    let mut remote_ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for change in &batch.changes {
        let remote_id = match change {
            BatchObservationChange::Upsert(entry) => {
                if entry.mount_id != mount.mount_id {
                    return invalid(format!(
                        "discovery entry `{}` belongs to mount `{}`, expected `{}`",
                        entry.remote_id.0, entry.mount_id.0, mount.mount_id.0
                    ));
                }
                validate_projected_path(&entry.path)?;
                if !paths.insert(entry.path.clone()) {
                    return invalid(format!(
                        "discovery batch contains duplicate projected path `{}`",
                        entry.path.display()
                    ));
                }
                &entry.remote_id
            }
            BatchObservationChange::Tombstone { remote_id } => remote_id,
        };
        if remote_id.0.is_empty() {
            return invalid("discovery remote id cannot be empty");
        }
        if !remote_ids.insert(remote_id.clone()) {
            return invalid(format!(
                "discovery batch contains duplicate remote id `{}`",
                remote_id.0
            ));
        }
    }
    Ok(())
}

fn validate_projected_path(path: &Path) -> LocalityResult<()> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return invalid(format!(
            "discovery projected path `{}` must be a nonempty relative path",
            path.display()
        ));
    }
    let path_text = path.to_str().ok_or_else(|| {
        LocalityError::InvalidState(format!(
            "discovery projected path `{}` must be valid UTF-8",
            path.display()
        ))
    })?;
    if path_text.contains('\\')
        || path_text
            .split('/')
            .any(|component| matches!(component, "" | "." | ".."))
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return invalid(format!(
            "discovery projected path `{}` must be normalized and relative",
            path.display()
        ));
    }
    Ok(())
}

fn invalid<T>(message: impl Into<String>) -> LocalityResult<T> {
    Err(LocalityError::InvalidState(message.into()))
}

fn entity_hold_reason(current: Option<&EntityRecord>) -> Option<DiscoveryHoldReason> {
    match current.map(|entity| &entity.hydration) {
        Some(HydrationState::Dirty) => Some(DiscoveryHoldReason::Dirty),
        Some(HydrationState::Conflicted) => Some(DiscoveryHoldReason::Conflicted),
        _ => None,
    }
}

fn pending_virtual_mutation_hold_reason(
    mutations: &[VirtualMutationRecord],
    remote_id: &RemoteId,
    kind: &EntityKind,
    paths: &[&Path],
) -> Option<DiscoveryHoldReason> {
    mutations
        .iter()
        .find(|mutation| {
            mutation.target_remote_id.as_ref() == Some(remote_id)
                || mutation.parent_remote_id.as_ref() == Some(remote_id)
                || mutation
                    .original_path
                    .as_ref()
                    .is_some_and(|mutation_path| {
                        paths.iter().any(|path| {
                            mutation_path_overlaps_projection(kind, path, mutation_path)
                        })
                    })
                || paths.iter().any(|path| {
                    mutation_path_overlaps_projection(kind, path, &mutation.projected_path)
                })
        })
        .map(|mutation| DiscoveryHoldReason::PendingVirtualMutation {
            local_id: mutation.local_id.clone(),
        })
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

fn kind_change_hold_reason(
    current: Option<&EntityRecord>,
    entry: &TreeEntry,
) -> Option<DiscoveryHoldReason> {
    let current = current?;
    (current.kind != entry.kind).then(|| DiscoveryHoldReason::UnsupportedKindChange {
        from: current.kind.clone(),
        to: entry.kind.clone(),
    })
}

fn entity_remote_changed(current: &EntityRecord, entry: &TreeEntry) -> bool {
    current.kind != entry.kind
        || current.title != entry.title
        || current.path != entry.path
        || current.content_hash != entry.content_hash
        || current.remote_edited_at != entry.remote_edited_at
}

fn accepted_entity_record(current: &EntityRecord, entry: &TreeEntry) -> EntityRecord {
    if matches!(
        current.hydration,
        HydrationState::Virtual | HydrationState::Stub
    ) {
        let mut record = EntityRecord::from(entry.clone());
        record.hydration = current.hydration.clone();
        return record;
    }

    let mut record = current.clone();
    record.kind = entry.kind.clone();
    record.title = entry.title.clone();
    record.path = entry.path.clone();
    record
}

fn observation_for_entry(
    entry: &TreeEntry,
    observed_at: &str,
) -> LocalityResult<RemoteObservationRecord> {
    let mut observation = RemoteObservationRecord::new(
        entry.mount_id.clone(),
        entry.remote_id.clone(),
        entry.kind.clone(),
        entry.title.clone(),
        entry.path.clone(),
        observed_at,
    );
    if let Some(version) = &entry.remote_edited_at {
        observation = observation.with_remote_version(RemoteVersion::new(version.clone()));
    }
    Ok(
        observation.with_raw_metadata_json(encode_replay(ReplayChange::Upsert {
            entry: entry.clone(),
        })?),
    )
}

fn observation_for_tombstone(
    current: &EntityRecord,
    observed_at: &str,
) -> LocalityResult<RemoteObservationRecord> {
    Ok(RemoteObservationRecord::new(
        current.mount_id.clone(),
        current.remote_id.clone(),
        current.kind.clone(),
        current.title.clone(),
        current.path.clone(),
        observed_at,
    )
    .deleted(true)
    .with_raw_metadata_json(encode_replay(ReplayChange::Tombstone {
        remote_id: current.remote_id.clone(),
    })?))
}

fn next_freshness(
    mount_id: &MountId,
    remote_id: &RemoteId,
    current: Option<&EntityRecord>,
    previous: Option<&FreshnessStateRecord>,
    observed_at: &str,
    remote_hint_pending: bool,
) -> FreshnessStateRecord {
    let mut freshness = previous.cloned().unwrap_or_else(|| {
        FreshnessStateRecord::new(
            mount_id.clone(),
            remote_id.clone(),
            match current.map(|entity| &entity.hydration) {
                Some(HydrationState::Dirty | HydrationState::Conflicted) => FreshnessTier::Hot,
                Some(HydrationState::Hydrated) => FreshnessTier::Warm,
                Some(HydrationState::Virtual | HydrationState::Stub) | None => FreshnessTier::Cold,
            },
        )
    });
    freshness.last_checked_at = Some(observed_at.to_string());
    freshness.remote_hint_pending = remote_hint_pending;
    freshness
}

fn pause_auto_save(
    enrollments: &[AutoSaveEnrollmentRecord],
    current: Option<&EntityRecord>,
    final_path: Option<&Path>,
    observed_at: &str,
    reason: &str,
    output: &mut Vec<AutoSaveEnrollmentRecord>,
) -> LocalityResult<()> {
    let Some(current) = current else {
        return Ok(());
    };
    if let Some(enrollment) =
        discovery_auto_save_candidate(enrollments, &current.remote_id, Some(&current.path))
            .map_err(LocalityError::from)?
        && enrollment.enabled
    {
        let mut paused = enrollment.clone();
        if let Some(final_path) = final_path {
            paused.path = final_path.to_path_buf();
        }
        paused = paused.paused_remote_changed(reason, observed_at);
        output.push(paused);
    }
    Ok(())
}

fn queue_hydration(mount: &MountConfig, entity: &EntityRecord) -> DiscoveryPostCommitAction {
    DiscoveryPostCommitAction::QueueHydration(HydrationRequest::new(
        mount.mount_id.clone(),
        entity.remote_id.clone(),
        mount.root.join(&entity.path),
        HydrationState::Hydrated,
        HydrationReason::RemoteFastForward,
    ))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DiscoveryReplayEnvelope {
    tag: String,
    state_version: i64,
    min_reader_version: i64,
    change: ReplayChange,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ReplayChange {
    Upsert { entry: TreeEntry },
    Tombstone { remote_id: RemoteId },
}

fn encode_replay(change: ReplayChange) -> LocalityResult<String> {
    serde_json::to_string(&DiscoveryReplayEnvelope {
        tag: DISCOVERY_REPLAY_TAG.to_string(),
        state_version: DISCOVERY_REPLAY_VERSION,
        min_reader_version: DISCOVERY_REPLAY_VERSION,
        change,
    })
    .map_err(|error| {
        LocalityError::InvalidState(format!("cannot encode discovery replay: {error}"))
    })
}

fn is_replay_envelope(raw: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|value| {
            value
                .get("tag")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .is_some_and(|tag| tag == DISCOVERY_REPLAY_TAG)
}

fn decode_replay(raw: &str) -> LocalityResult<Option<ReplayChange>> {
    let value = match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    if value.get("tag").and_then(serde_json::Value::as_str) != Some(DISCOVERY_REPLAY_TAG) {
        return Ok(None);
    }
    let state_version = value
        .get("state_version")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| {
            LocalityError::InvalidState(
                "discovery replay state_version must be an integer".to_string(),
            )
        })?;
    let min_reader_version = value
        .get("min_reader_version")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| {
            LocalityError::InvalidState(
                "discovery replay min_reader_version must be an integer".to_string(),
            )
        })?;
    if state_version <= 0 || min_reader_version <= 0 || min_reader_version > state_version {
        return invalid("discovery replay versions are invalid");
    }
    if state_version > DISCOVERY_REPLAY_VERSION || min_reader_version > DISCOVERY_REPLAY_VERSION {
        return Err(LocalityError::UpdateRequired {
            component: "daemon:discovery_replay".to_string(),
            found: state_version,
            supported: DISCOVERY_REPLAY_VERSION,
        });
    }
    let envelope = serde_json::from_value::<DiscoveryReplayEnvelope>(value).map_err(|error| {
        LocalityError::InvalidState(format!("discovery replay JSON is invalid: {error}"))
    })?;
    Ok(Some(envelope.change))
}

fn validate_replayed_entry(
    mount: &MountConfig,
    remote_id: &RemoteId,
    entry: &TreeEntry,
) -> LocalityResult<()> {
    if entry.mount_id != mount.mount_id || &entry.remote_id != remote_id {
        return invalid(format!(
            "discovery replay entry `{}/{}` does not match `{}/{}`",
            entry.mount_id.0, entry.remote_id.0, mount.mount_id.0, remote_id.0
        ));
    }
    validate_projected_path(&entry.path)
}
