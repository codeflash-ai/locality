//! Repository traits for durable Locality state.
//!
//! The traits are split by lookup responsibility so orchestration code can ask
//! for only the state it needs. `loc diff` and `loc push` primarily need mount
//! config, entity lookup by local path, and the last synced shadow.

use std::path::Path;

use locality_core::journal::{JournalApplyEffect, JournalEntry, JournalStatus, PushId};
use locality_core::model::{MountId, RemoteId};
use locality_core::shadow::ShadowDocument;

use crate::error::StoreResult;
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
            if !journal_touches_any_entity(&journal, remote_ids) {
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
            if !journal.remote_ids.iter().any(|id| id == remote_id)
                && !journal
                    .plan
                    .affected_entities
                    .iter()
                    .any(|id| id == remote_id)
            {
                continue;
            }
            if let JournalStatus::Failed(message) = journal.status {
                latest = Some((journal.push_id.0, message));
            }
        }

        Ok(latest.map(|(_, message)| message))
    }
}

fn journal_touches_any_entity(journal: &JournalEntry, remote_ids: &[RemoteId]) -> bool {
    journal
        .remote_ids
        .iter()
        .any(|id| remote_ids.iter().any(|target| target == id))
        || journal
            .plan
            .affected_entities
            .iter()
            .any(|id| remote_ids.iter().any(|target| target == id))
        || journal
            .apply_effects
            .iter()
            .any(|effect| apply_effect_touches_any_entity(effect, remote_ids))
}

fn apply_effect_touches_any_entity(effect: &JournalApplyEffect, remote_ids: &[RemoteId]) -> bool {
    match effect {
        JournalApplyEffect::ArchivedEntity { entity_id, .. }
        | JournalApplyEffect::UpdatedProperties { entity_id, .. }
        | JournalApplyEffect::MovedEntity { entity_id, .. }
        | JournalApplyEffect::CreatedEntity { entity_id, .. } => {
            remote_ids.iter().any(|target| target == entity_id)
        }
        JournalApplyEffect::UpdatedBlock { .. }
        | JournalApplyEffect::CreatedBlock { .. }
        | JournalApplyEffect::MovedBlock { .. }
        | JournalApplyEffect::ArchivedBlock { .. } => false,
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
