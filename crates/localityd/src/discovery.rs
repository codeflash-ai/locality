//! Pure planning for checkpointed connector discovery batches.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use locality_connector::{
    BatchObservationChange, BatchObservationCompleteness, BatchObserveResult,
};
use locality_core::freshness::{FreshnessTier, RemoteVersion};
use locality_core::hydration::{HydrationReason, HydrationRequest};
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId, TreeEntry};
use locality_core::{LocalityError, LocalityResult};
use locality_store::{
    AutoSaveEnrollmentRecord, AutoSaveRepository, ConnectorStateRecord, DiscoveryCommit,
    EntityRecord, EntityRepository, FreshnessStateRecord, FreshnessStateRepository, MountConfig,
    RemoteObservationRecord, RemoteObservationRepository,
};
use serde::{Deserialize, Serialize};

const DISCOVERY_REPLAY_TAG: &str = "localityd.discovery_replay";
const DISCOVERY_REPLAY_VERSION: i64 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveryPlan {
    commit: DiscoveryCommit,
    pub projection_actions: Vec<DiscoveryProjectionAction>,
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

#[derive(Clone, Debug, PartialEq, Eq)]
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
) -> LocalityResult<DiscoveryPlan>
where
    S: EntityRepository
        + RemoteObservationRepository
        + FreshnessStateRepository
        + AutoSaveRepository,
{
    if metadata_job_id.is_some_and(str::is_empty) {
        return invalid("discovery metadata job identifier cannot be empty");
    }
    validate_batch(mount, &batch)?;
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
                let hold_reason = remote_changed
                    .then(|| entity_hold_reason(current))
                    .flatten()
                    .or_else(|| kind_change_hold_reason(current, &entry))
                    .or_else(|| {
                        structural
                            .then(|| assessment_hold_reason(assessments.get(&remote_id)))
                            .flatten()
                    });
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
                    );
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
                    );
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
                let hold_reason = entity_hold_reason(Some(current))
                    .or_else(|| assessment_hold_reason(assessments.get(&remote_id)));
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
                    );
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

    Ok(DiscoveryPlan {
        commit: DiscoveryCommit {
            mount_id: mount.mount_id.clone(),
            entity_upserts,
            entity_deletes,
            observation_upserts,
            freshness_upserts,
            auto_save_upserts,
            metadata_discovery_deletes: metadata_job_id.map(str::to_string).into_iter().collect(),
            virtual_mutation_deletes: Vec::new(),
            checkpoint,
        },
        projection_actions,
        held,
        post_commit,
    })
}

#[derive(Clone, Debug)]
enum DiscoveryIntent {
    Upsert(TreeEntry),
    Tombstone,
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

fn assessment_hold_reason(
    assessment: Option<&ProjectionAssessment>,
) -> Option<DiscoveryHoldReason> {
    match assessment {
        Some(ProjectionAssessment::Safe) => None,
        Some(ProjectionAssessment::Blocked(reason)) => Some(reason.clone()),
        None => Some(DiscoveryHoldReason::UnknownProjection),
    }
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
) {
    let Some(current) = current else {
        return;
    };
    if let Some(enrollment) = enrollments.iter().find(|enrollment| {
        enrollment.remote_id.as_ref() == Some(&current.remote_id) || enrollment.path == current.path
    }) && enrollment.enabled
    {
        let mut paused = enrollment.clone();
        if let Some(final_path) = final_path {
            paused.path = final_path.to_path_buf();
        }
        paused = paused.paused_remote_changed(reason, observed_at);
        output.push(paused);
    }
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
