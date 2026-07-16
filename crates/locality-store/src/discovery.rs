//! Atomic durable boundary for connector batch discovery.
//!
//! Discovery policy belongs to the daemon. This module only validates a fully
//! prepared commit and requires implementations to publish its checkpoint with
//! the associated entity state atomically.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use locality_core::model::{MountId, RemoteId};

use crate::error::{StoreError, StoreResult};
use crate::records::{
    AutoSaveEnrollmentRecord, ConnectorStateRecord, EntityRecord, FreshnessStateRecord,
    RemoteObservationRecord, VirtualMutationRecord,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveryCommit {
    pub mount_id: MountId,
    pub entity_upserts: Vec<EntityRecord>,
    pub entity_deletes: Vec<RemoteId>,
    pub observation_upserts: Vec<RemoteObservationRecord>,
    pub freshness_upserts: Vec<FreshnessStateRecord>,
    pub auto_save_upserts: Vec<AutoSaveEnrollmentRecord>,
    /// Opaque daemon-owned metadata queue identifiers invalidated by this batch.
    pub metadata_discovery_deletes: Vec<String>,
    /// Mutation IDs the daemon has proved stale for affected remote entities.
    pub virtual_mutation_deletes: Vec<String>,
    pub checkpoint: ConnectorStateRecord,
}

pub trait DiscoveryRepository {
    fn commit_discovery(&mut self, commit: DiscoveryCommit) -> StoreResult<()>;
}

impl DiscoveryCommit {
    pub(crate) fn validate(&self) -> StoreResult<()> {
        let mut entity_upserts = BTreeSet::new();
        let mut entity_paths = BTreeSet::new();
        for entity in &self.entity_upserts {
            validate_mount(
                "entity",
                &entity.remote_id,
                &entity.mount_id,
                &self.mount_id,
            )?;
            if !entity_upserts.insert(entity.remote_id.clone()) {
                return invalid(format!(
                    "discovery commit contains duplicate entity upsert `{}`",
                    entity.remote_id.0
                ));
            }
            if !entity_paths.insert(entity.path.clone()) {
                return invalid(format!(
                    "discovery commit contains duplicate entity path `{}`",
                    entity.path.display()
                ));
            }
        }

        let mut entity_deletes = BTreeSet::new();
        for remote_id in &self.entity_deletes {
            if !entity_deletes.insert(remote_id.clone()) {
                return invalid(format!(
                    "discovery commit contains duplicate entity delete `{}`",
                    remote_id.0
                ));
            }
        }

        let mut observation_upserts = BTreeSet::new();
        for observation in &self.observation_upserts {
            validate_mount(
                "observation",
                &observation.remote_id,
                &observation.mount_id,
                &self.mount_id,
            )?;
            if !observation_upserts.insert(observation.remote_id.clone()) {
                return invalid(format!(
                    "discovery commit contains duplicate observation upsert `{}`",
                    observation.remote_id.0
                ));
            }
        }

        let mut freshness_upserts = BTreeSet::new();
        for freshness in &self.freshness_upserts {
            validate_mount(
                "freshness state",
                &freshness.remote_id,
                &freshness.mount_id,
                &self.mount_id,
            )?;
            if !freshness_upserts.insert(freshness.remote_id.clone()) {
                return invalid(format!(
                    "discovery commit contains duplicate freshness upsert `{}`",
                    freshness.remote_id.0
                ));
            }
        }

        let mut auto_save_paths = BTreeSet::new();
        let mut auto_save_remote_ids = BTreeSet::new();
        for enrollment in &self.auto_save_upserts {
            if enrollment.mount_id != self.mount_id {
                return invalid(format!(
                    "discovery auto-save enrollment `{}` belongs to mount `{}`, expected `{}`",
                    enrollment.path.display(),
                    enrollment.mount_id.0,
                    self.mount_id.0
                ));
            }
            if !auto_save_paths.insert(enrollment.path.clone()) {
                return invalid(format!(
                    "discovery commit contains duplicate auto-save path `{}`",
                    enrollment.path.display()
                ));
            }
            if let Some(remote_id) = &enrollment.remote_id
                && !auto_save_remote_ids.insert(remote_id.clone())
            {
                return invalid(format!(
                    "discovery commit contains duplicate auto-save owner `{}`",
                    remote_id.0
                ));
            }
        }

        for remote_id in &entity_deletes {
            if entity_upserts.contains(remote_id)
                || observation_upserts.contains(remote_id)
                || freshness_upserts.contains(remote_id)
            {
                return invalid(format!(
                    "discovery commit both deletes and upserts `{}`",
                    remote_id.0
                ));
            }
        }

        let mut metadata_deletes = BTreeSet::new();
        for identifier in &self.metadata_discovery_deletes {
            if identifier.is_empty() {
                return invalid("discovery metadata job identifier cannot be empty");
            }
            if !metadata_deletes.insert(identifier) {
                return invalid(format!(
                    "discovery commit contains duplicate metadata job delete `{identifier}`"
                ));
            }
        }

        let mut mutation_deletes = BTreeSet::new();
        for local_id in &self.virtual_mutation_deletes {
            if local_id.is_empty() {
                return invalid("discovery virtual mutation identifier cannot be empty");
            }
            if !mutation_deletes.insert(local_id) {
                return invalid(format!(
                    "discovery commit contains duplicate virtual mutation delete `{local_id}`"
                ));
            }
        }

        if self.checkpoint.connector.is_empty() {
            return invalid("discovery checkpoint connector cannot be empty");
        }
        if self.checkpoint.scope_kind != "mount" || self.checkpoint.scope_id != self.mount_id.0 {
            return invalid(format!(
                "discovery checkpoint must use mount scope `{}`",
                self.mount_id.0
            ));
        }
        if self.checkpoint.state_version <= 0 || self.checkpoint.min_reader_version <= 0 {
            return invalid("discovery checkpoint versions must be positive");
        }
        if self.checkpoint.min_reader_version > self.checkpoint.state_version {
            return invalid(format!(
                "discovery checkpoint minimum reader version {} exceeds state version {}",
                self.checkpoint.min_reader_version, self.checkpoint.state_version
            ));
        }
        serde_json::from_str::<serde_json::Value>(&self.checkpoint.state_json).map_err(
            |error| {
                StoreError::InvalidState(format!("discovery checkpoint JSON is invalid: {error}"))
            },
        )?;
        Ok(())
    }

    pub(crate) fn validate_connector(&self, connector: &str) -> StoreResult<()> {
        if self.checkpoint.connector != connector {
            return invalid(format!(
                "discovery checkpoint connector `{}` does not match mount connector `{connector}`",
                self.checkpoint.connector
            ));
        }
        Ok(())
    }

    pub(crate) fn final_entity_map(
        &self,
        existing: &[EntityRecord],
    ) -> StoreResult<BTreeMap<RemoteId, EntityRecord>> {
        let mut by_id = existing
            .iter()
            .cloned()
            .map(|entity| (entity.remote_id.clone(), entity))
            .collect::<BTreeMap<_, _>>();
        for remote_id in &self.entity_deletes {
            by_id.remove(remote_id);
        }
        for entity in &self.entity_upserts {
            by_id.insert(entity.remote_id.clone(), entity.clone());
        }

        let mut by_path = BTreeMap::new();
        for entity in by_id.values() {
            if let Some(existing_remote_id) =
                by_path.insert(entity.path.clone(), entity.remote_id.clone())
                && existing_remote_id != entity.remote_id
            {
                return Err(StoreError::DuplicateEntityPath {
                    mount_id: self.mount_id.clone(),
                    path: entity.path.clone(),
                });
            }
        }
        Ok(by_id)
    }

    pub(crate) fn validate_virtual_mutation_changes(
        &self,
        mutations: &[VirtualMutationRecord],
        affected_remote_ids: &BTreeSet<RemoteId>,
        affected_paths: &BTreeSet<PathBuf>,
    ) -> StoreResult<()> {
        let explicit_deletes = self
            .virtual_mutation_deletes
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();

        for mutation in mutations {
            let affected = mutation
                .target_remote_id
                .as_ref()
                .is_some_and(|remote_id| affected_remote_ids.contains(remote_id))
                || mutation
                    .parent_remote_id
                    .as_ref()
                    .is_some_and(|remote_id| affected_remote_ids.contains(remote_id))
                || mutation
                    .original_path
                    .as_ref()
                    .is_some_and(|path| path_is_affected(path, affected_paths))
                || path_is_affected(&mutation.projected_path, affected_paths);
            let explicitly_deleted = explicit_deletes.contains(mutation.local_id.as_str());
            if affected && !explicitly_deleted {
                return invalid(format!(
                    "discovery cannot change discovered entity state with pending virtual mutation `{}`",
                    mutation.local_id
                ));
            }
            if explicitly_deleted && !affected {
                return invalid(format!(
                    "discovery virtual mutation `{}` is not related to an affected entity",
                    mutation.local_id
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn validate_auto_save_ownership(
        &self,
        enrollments: &[AutoSaveEnrollmentRecord],
        affected_entities: &[(RemoteId, Option<PathBuf>)],
    ) -> StoreResult<()> {
        for (remote_id, old_path) in affected_entities {
            if let Some(enrollment) = old_path.as_ref().and_then(|path| {
                enrollments
                    .iter()
                    .find(|enrollment| enrollment.path == *path)
            }) && let Some(owner) = &enrollment.remote_id
                && owner != remote_id
            {
                return invalid(format!(
                    "auto-save enrollment at `{}` belongs to `{}` instead of `{}`",
                    enrollment.path.display(),
                    owner.0,
                    remote_id.0
                ));
            }

            let owned_count = enrollments
                .iter()
                .filter(|enrollment| {
                    enrollment.remote_id.as_ref() == Some(remote_id)
                        || old_path
                            .as_ref()
                            .is_some_and(|path| enrollment.path == *path)
                })
                .count();
            if owned_count > 1 {
                return invalid(format!(
                    "multiple auto-save enrollments belong to entity `{}`",
                    remote_id.0
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn plan_auto_save_changes(
        &self,
        existing: &[AutoSaveEnrollmentRecord],
        affected_entities: &[(RemoteId, Option<PathBuf>)],
        path_moves: &[(RemoteId, PathBuf, PathBuf)],
        final_entities: &BTreeMap<RemoteId, EntityRecord>,
    ) -> StoreResult<Vec<AutoSaveRehome>> {
        self.validate_auto_save_ownership(existing, affected_entities)?;

        let deleted_remote_ids = self.entity_deletes.iter().cloned().collect::<BTreeSet<_>>();
        let deleted_paths = affected_entities
            .iter()
            .filter(|(remote_id, _)| deleted_remote_ids.contains(remote_id))
            .filter_map(|(_, path)| path.clone())
            .collect::<BTreeSet<_>>();
        let reassigned_paths = self
            .entity_upserts
            .iter()
            .filter(|entity| !deleted_remote_ids.contains(&entity.remote_id))
            .map(|entity| entity.path.clone())
            .collect::<BTreeSet<_>>();

        let mut final_enrollments = existing
            .iter()
            .filter(|enrollment| {
                !enrollment
                    .remote_id
                    .as_ref()
                    .is_some_and(|remote_id| deleted_remote_ids.contains(remote_id))
                    && !deleted_paths.contains(&enrollment.path)
            })
            .map(|enrollment| (enrollment.path.clone(), enrollment.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut rehomes = Vec::new();
        let mut selected_paths = BTreeSet::new();
        for (remote_id, old_path, new_path) in path_moves {
            let candidates = existing
                .iter()
                .filter(|enrollment| {
                    !enrollment
                        .remote_id
                        .as_ref()
                        .is_some_and(|owner| deleted_remote_ids.contains(owner))
                        && !deleted_paths.contains(&enrollment.path)
                        && (enrollment.remote_id.as_ref() == Some(remote_id)
                            || enrollment.path == *old_path)
                })
                .collect::<Vec<_>>();
            if candidates.len() > 1 {
                return invalid(format!(
                    "multiple auto-save enrollments belong to entity `{}`",
                    remote_id.0
                ));
            }
            if let Some(enrollment) = candidates.first() {
                if !selected_paths.insert(enrollment.path.clone()) {
                    return invalid(format!(
                        "auto-save enrollment `{}` belongs to multiple entity moves",
                        enrollment.path.display()
                    ));
                }
                let mut enrollment = (*enrollment).clone();
                let old_path = enrollment.path.clone();
                enrollment.path = new_path.clone();
                rehomes.push(AutoSaveRehome {
                    old_path,
                    enrollment,
                });
            }
        }
        for rehome in &rehomes {
            final_enrollments.remove(&rehome.old_path);
        }
        for rehome in &rehomes {
            if let Some(occupied) = final_enrollments.get(&rehome.enrollment.path) {
                return invalid(format!(
                    "cannot rehome auto-save enrollment to `{}` owned by `{}`",
                    rehome.enrollment.path.display(),
                    auto_save_owner(occupied)
                ));
            }
            final_enrollments.insert(rehome.enrollment.path.clone(), rehome.enrollment.clone());
        }

        for enrollment in &self.auto_save_upserts {
            if let Some(remote_id) = &enrollment.remote_id {
                let Some(entity) = final_entities.get(remote_id) else {
                    return invalid(format!(
                        "auto-save enrollment `{}` references entity `{}` outside the final mount tree",
                        enrollment.path.display(),
                        remote_id.0
                    ));
                };
                if enrollment.path != entity.path {
                    let occupied_by = final_entities
                        .values()
                        .find(|candidate| candidate.path == enrollment.path)
                        .map(|candidate| candidate.remote_id.as_str());
                    return invalid(match occupied_by {
                        Some(occupied_by) => format!(
                            "auto-save enrollment at `{}` belongs to `{}` but that path is occupied by `{occupied_by}`",
                            enrollment.path.display(),
                            remote_id.0
                        ),
                        None => format!(
                            "auto-save enrollment for `{}` must use final path `{}`",
                            remote_id.0,
                            entity.path.display()
                        ),
                    });
                }
            }
            if let Some(remote_id) = &enrollment.remote_id
                && deleted_remote_ids.contains(remote_id)
            {
                return invalid(format!(
                    "auto-save enrollment `{}` references deleted entity `{}`",
                    enrollment.path.display(),
                    remote_id.0
                ));
            }
            if deleted_paths.contains(&enrollment.path)
                && !reassigned_paths.contains(&enrollment.path)
            {
                return invalid(format!(
                    "auto-save enrollment path `{}` belongs to a deleted entity",
                    enrollment.path.display()
                ));
            }
            if let Some(owner) = &enrollment.remote_id
                && let Some(reassigned) = self.entity_upserts.iter().find(|entity| {
                    entity.path == enrollment.path
                        && !deleted_remote_ids.contains(&entity.remote_id)
                })
                && owner != &reassigned.remote_id
            {
                return invalid(format!(
                    "auto-save enrollment at `{}` belongs to `{}` instead of reassigned entity `{}`",
                    enrollment.path.display(),
                    owner.0,
                    reassigned.remote_id.0
                ));
            }
            if let Some(remote_id) = &enrollment.remote_id
                && let Some((_, _, new_path)) = path_moves
                    .iter()
                    .find(|(moving_id, _, _)| moving_id == remote_id)
                && enrollment.path != *new_path
            {
                return invalid(format!(
                    "auto-save enrollment for moved entity `{}` must use `{}`",
                    remote_id.0,
                    new_path.display()
                ));
            }
            if let Some(occupied) = final_enrollments.get(&enrollment.path)
                && occupied.remote_id != enrollment.remote_id
            {
                return invalid(format!(
                    "auto-save enrollment path `{}` is owned by `{}`",
                    enrollment.path.display(),
                    auto_save_owner(occupied)
                ));
            }
            if let Some(remote_id) = &enrollment.remote_id
                && let Some((path, _)) = final_enrollments.iter().find(|(path, existing)| {
                    **path != enrollment.path && existing.remote_id.as_ref() == Some(remote_id)
                })
            {
                return invalid(format!(
                    "auto-save enrollment for `{}` already exists at `{}`",
                    remote_id.0,
                    path.display()
                ));
            }
            final_enrollments.insert(enrollment.path.clone(), enrollment.clone());
        }
        Ok(rehomes)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AutoSaveRehome {
    pub old_path: PathBuf,
    pub enrollment: AutoSaveEnrollmentRecord,
}

fn auto_save_owner(enrollment: &AutoSaveEnrollmentRecord) -> &str {
    enrollment
        .remote_id
        .as_ref()
        .map_or("path", RemoteId::as_str)
}

fn path_is_affected(path: &std::path::Path, affected_paths: &BTreeSet<PathBuf>) -> bool {
    affected_paths.iter().any(|affected| {
        path == affected || path.starts_with(affected) || affected.starts_with(path)
    })
}

fn validate_mount(
    record_kind: &str,
    remote_id: &RemoteId,
    actual: &MountId,
    expected: &MountId,
) -> StoreResult<()> {
    if actual != expected {
        return invalid(format!(
            "discovery {record_kind} `{}` belongs to mount `{}`, expected `{}`",
            remote_id.0, actual.0, expected.0
        ));
    }
    Ok(())
}

fn invalid<T>(message: impl Into<String>) -> StoreResult<T> {
    Err(StoreError::InvalidState(message.into()))
}
