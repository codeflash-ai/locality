//! Repository traits for durable Locality state.
//!
//! The traits are split by lookup responsibility so orchestration code can ask
//! for only the state it needs. `loc diff` and `loc push` primarily need mount
//! config, entity lookup by local path, and the last synced shadow.

use std::path::Path;

use locality_core::journal::{JournalApplyEffect, JournalEntry, JournalStatus, PushId};
use locality_core::model::{MountId, RemoteId};
use locality_core::shadow::ShadowDocument;

use crate::error::{StoreError, StoreResult};
use crate::records::{
    AutoSaveEnrollmentRecord, ConnectionId, ConnectionRecord, ConnectorProfileId,
    ConnectorProfileRecord, ConnectorStateRecord, EntityRecord, FreshnessStateRecord,
    HydrationJobRecord, MetadataDiscoveryJobRecord, MountConfig, MountLiveModeRecord,
    RemoteObservationRecord, ShadowSnapshotRecord, VirtualMutationRecord,
};

pub trait MountRepository {
    fn save_mount(&mut self, mount: MountConfig) -> StoreResult<()>;
    fn get_mount(&self, mount_id: &MountId) -> StoreResult<Option<MountConfig>>;
    fn load_mounts(&self) -> StoreResult<Vec<MountConfig>>;
}

pub trait MountLiveModeRepository {
    fn save_mount_live_mode(&mut self, live_mode: MountLiveModeRecord) -> StoreResult<()>;
    fn get_mount_live_mode(&self, mount_id: &MountId) -> StoreResult<Option<MountLiveModeRecord>>;
    fn list_mount_live_modes(&self) -> StoreResult<Vec<MountLiveModeRecord>>;
    fn delete_mount_live_mode(&mut self, mount_id: &MountId) -> StoreResult<()>;
}

pub trait ConnectionRepository {
    fn save_connection(&mut self, connection: ConnectionRecord) -> StoreResult<()>;
    fn get_connection(&self, connection_id: &ConnectionId)
    -> StoreResult<Option<ConnectionRecord>>;
    fn list_connections(&self) -> StoreResult<Vec<ConnectionRecord>>;
    fn delete_connection(&mut self, connection_id: &ConnectionId) -> StoreResult<()>;
}

pub trait ConnectorProfileRepository {
    fn save_connector_profile(&mut self, profile: ConnectorProfileRecord) -> StoreResult<()>;
    fn get_connector_profile(
        &self,
        profile_id: &ConnectorProfileId,
    ) -> StoreResult<Option<ConnectorProfileRecord>>;
    fn list_connector_profiles(&self) -> StoreResult<Vec<ConnectorProfileRecord>>;
}

pub trait ConnectorStateRepository {
    fn save_connector_state(&mut self, state: ConnectorStateRecord) -> StoreResult<()>;
    fn get_connector_state(
        &self,
        connector: &str,
        scope_kind: &str,
        scope_id: &str,
    ) -> StoreResult<Option<ConnectorStateRecord>>;
}

pub trait EntityRepository {
    fn save_entity(&mut self, entity: EntityRecord) -> StoreResult<()>;
    fn get_entity(
        &self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<Option<EntityRecord>>;
    fn find_entity_by_path(
        &self,
        mount_id: &MountId,
        path: &Path,
    ) -> StoreResult<Option<EntityRecord>>;
    fn list_entities(&self, mount_id: &MountId) -> StoreResult<Vec<EntityRecord>>;
    fn delete_entity(&mut self, mount_id: &MountId, remote_id: &RemoteId) -> StoreResult<()>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntitySearchCandidate {
    pub entity: EntityRecord,
    pub observation: Option<RemoteObservationRecord>,
    pub search_document: Option<EntitySearchDocument>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EntitySearchDocument {
    pub title: Option<String>,
    pub path: Option<String>,
    pub observed_title: Option<String>,
    pub observed_path: Option<String>,
    pub frontmatter: Option<String>,
    pub body: Option<String>,
    pub metadata_text: Option<String>,
    pub breadcrumbs: Option<String>,
    pub aliases: Option<String>,
    pub source_url: Option<String>,
}

pub trait EntitySearchRepository {
    fn list_entity_search_candidates(
        &self,
        mount_id: &MountId,
        query: &str,
        compact_remote_id: Option<&str>,
    ) -> StoreResult<Option<Vec<EntitySearchCandidate>>>;
}

pub trait VirtualMutationRepository {
    fn save_virtual_mutation(&mut self, mutation: VirtualMutationRecord) -> StoreResult<()>;
    fn get_virtual_mutation(
        &self,
        mount_id: &MountId,
        local_id: &str,
    ) -> StoreResult<Option<VirtualMutationRecord>>;
    fn find_virtual_mutation_by_path(
        &self,
        mount_id: &MountId,
        path: &Path,
    ) -> StoreResult<Option<VirtualMutationRecord>>;
    fn list_virtual_mutations(&self, mount_id: &MountId)
    -> StoreResult<Vec<VirtualMutationRecord>>;
    fn delete_virtual_mutation(&mut self, mount_id: &MountId, local_id: &str) -> StoreResult<()>;
}

/// One durable state transition for a virtual filesystem move.
///
/// `mutation.content_path` points at the source cache until the filesystem
/// publishes the destination cache. This makes an interrupted move readable
/// through either its durable source pointer or its projected destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualMoveTransition {
    pub mutation: VirtualMutationRecord,
    pub entity: Option<EntityRecord>,
    pub freshness: Option<FreshnessStateRecord>,
    pub superseded_local_ids: Vec<String>,
}

pub trait VirtualMoveRepository {
    fn begin_virtual_move(&mut self, transition: VirtualMoveTransition) -> StoreResult<()>;

    fn finalize_virtual_move_content(
        &mut self,
        mount_id: &MountId,
        local_id: &str,
        expected_content_path: Option<&Path>,
        content_path: std::path::PathBuf,
        updated_at: &str,
    ) -> StoreResult<VirtualMutationRecord>;
}

pub(crate) fn validate_virtual_move_transition(
    transition: &VirtualMoveTransition,
) -> StoreResult<()> {
    let mount_id = &transition.mutation.mount_id;
    let target_id = transition.mutation.target_remote_id.as_ref();
    if let Some(entity) = &transition.entity
        && (&entity.mount_id != mount_id || target_id != Some(&entity.remote_id))
    {
        return Err(StoreError::InvalidState(
            "virtual move entity does not match its mutation target".to_string(),
        ));
    }
    if let Some(freshness) = &transition.freshness
        && (&freshness.mount_id != mount_id
            || target_id != Some(&freshness.remote_id)
            || transition.entity.is_none())
    {
        return Err(StoreError::InvalidState(
            "virtual move freshness does not match its entity".to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn virtual_move_missing(mount_id: &MountId, local_id: &str) -> StoreError {
    StoreError::InvalidState(format!(
        "virtual move `{local_id}` is missing from mount `{}`",
        mount_id.0
    ))
}

pub(crate) fn virtual_move_content_changed(
    mount_id: &MountId,
    local_id: &str,
    expected: Option<&Path>,
    actual: Option<&Path>,
) -> StoreError {
    StoreError::InvalidState(format!(
        "virtual move `{local_id}` in mount `{}` changed content path from `{}` to `{}`",
        mount_id.0,
        expected.map_or_else(|| "<none>".to_string(), |path| path.display().to_string()),
        actual.map_or_else(|| "<none>".to_string(), |path| path.display().to_string()),
    ))
}

pub trait AutoSaveRepository {
    fn save_auto_save_enrollment(
        &mut self,
        enrollment: AutoSaveEnrollmentRecord,
    ) -> StoreResult<()>;
    fn get_auto_save_enrollment(
        &self,
        mount_id: &MountId,
        path: &Path,
    ) -> StoreResult<Option<AutoSaveEnrollmentRecord>>;
    fn find_auto_save_enrollment_by_remote_id(
        &self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<Option<AutoSaveEnrollmentRecord>>;
    fn list_auto_save_enrollments(
        &self,
        mount_id: &MountId,
    ) -> StoreResult<Vec<AutoSaveEnrollmentRecord>>;
    fn delete_auto_save_enrollment(&mut self, mount_id: &MountId, path: &Path) -> StoreResult<()>;
}

pub trait RemoteObservationRepository {
    fn save_remote_observation(&mut self, observation: RemoteObservationRecord) -> StoreResult<()>;
    fn get_remote_observation(
        &self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<Option<RemoteObservationRecord>>;
    fn list_remote_observations(
        &self,
        mount_id: &MountId,
    ) -> StoreResult<Vec<RemoteObservationRecord>>;
    fn delete_remote_observation(
        &mut self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<()>;
}

pub trait FreshnessStateRepository {
    fn save_freshness_state(&mut self, state: FreshnessStateRecord) -> StoreResult<()>;
    fn get_freshness_state(
        &self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<Option<FreshnessStateRecord>>;
    fn list_freshness_states(&self, mount_id: &MountId) -> StoreResult<Vec<FreshnessStateRecord>>;
    fn delete_freshness_state(
        &mut self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<()>;
}

pub trait HydrationJobRepository {
    fn upsert_hydration_job(&mut self, job: HydrationJobRecord) -> StoreResult<()>;
    fn list_hydration_jobs(&self) -> StoreResult<Vec<HydrationJobRecord>>;
    fn delete_hydration_job(&mut self, mount_id: &MountId, remote_id: &RemoteId)
    -> StoreResult<()>;
    fn record_hydration_job_failure(
        &mut self,
        mount_id: &MountId,
        remote_id: &RemoteId,
        message: String,
    ) -> StoreResult<()>;
}

pub trait MetadataDiscoveryJobRepository {
    fn upsert_metadata_discovery_job(&mut self, job: MetadataDiscoveryJobRecord)
    -> StoreResult<()>;
    fn list_metadata_discovery_jobs(&self) -> StoreResult<Vec<MetadataDiscoveryJobRecord>>;
    fn delete_metadata_discovery_job(
        &mut self,
        mount_id: &MountId,
        container_identifier: &str,
    ) -> StoreResult<()>;
    fn record_metadata_discovery_job_failure(
        &mut self,
        mount_id: &MountId,
        container_identifier: &str,
        message: String,
    ) -> StoreResult<()>;
}

pub trait ShadowRepository {
    fn save_shadow(&mut self, mount_id: &MountId, shadow: ShadowDocument) -> StoreResult<()>;
    fn load_shadow(&self, mount_id: &MountId, entity_id: &RemoteId) -> StoreResult<ShadowDocument>;
    fn get_shadow_record(
        &self,
        mount_id: &MountId,
        entity_id: &RemoteId,
    ) -> StoreResult<Option<ShadowSnapshotRecord>>;
}

pub trait JournalRepository {
    fn append_journal(&mut self, entry: JournalEntry) -> StoreResult<()>;
    fn record_journal_apply_effects(
        &mut self,
        push_id: &PushId,
        effects: Vec<JournalApplyEffect>,
    ) -> StoreResult<()>;
    fn update_journal_status(&mut self, push_id: &PushId, status: JournalStatus)
    -> StoreResult<()>;
    fn get_journal(&self, push_id: &PushId) -> StoreResult<Option<JournalEntry>>;
    fn list_journal(&self) -> StoreResult<Vec<JournalEntry>>;

    fn latest_journal_for_entities(
        &self,
        mount_id: &MountId,
        remote_ids: &[RemoteId],
    ) -> StoreResult<Option<PushId>> {
        let mut latest: Option<JournalEntry> = None;
        for journal in self.list_journal()? {
            if journal.mount_id != *mount_id {
                continue;
            }
            if matches!(journal.status, JournalStatus::Reverted) {
                continue;
            }
            if !journal.touches_any_entity(remote_ids) {
                continue;
            }
            if latest
                .as_ref()
                .is_none_or(|current| journal_is_newer(&journal, current))
            {
                latest = Some(journal);
            }
        }
        Ok(latest.map(|journal| journal.push_id))
    }

    fn latest_failed_journal_for_entity(
        &self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<Option<String>> {
        let mut latest = None;
        for journal in self.list_journal()? {
            if journal.mount_id != *mount_id {
                continue;
            }
            if !journal.touches_any_entity(std::slice::from_ref(remote_id)) {
                continue;
            }
            if let JournalStatus::Failed(message) = journal.status {
                latest = Some((journal.push_id.0, message));
            }
        }

        Ok(latest.map(|(_, message)| message))
    }
}

fn journal_is_newer(candidate: &JournalEntry, current: &JournalEntry) -> bool {
    match (
        candidate.metadata.created_at_unix_ms,
        current.metadata.created_at_unix_ms,
    ) {
        (Some(candidate_created_at), Some(current_created_at)) => {
            candidate_created_at > current_created_at
                || (candidate_created_at == current_created_at
                    && candidate.push_id.0 > current.push_id.0)
        }
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => candidate.push_id.0 > current.push_id.0,
    }
}
