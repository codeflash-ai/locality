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
    ConnectorProfileRecord, EntityRecord, FreshnessStateRecord, HydrationJobRecord, MountConfig,
    RemoteObservationRecord, ShadowSnapshotRecord, VirtualMutationRecord,
};

pub trait MountRepository {
    fn save_mount(&mut self, mount: MountConfig) -> StoreResult<()>;
    fn get_mount(&self, mount_id: &MountId) -> StoreResult<Option<MountConfig>>;
    fn load_mounts(&self) -> StoreResult<Vec<MountConfig>>;
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
