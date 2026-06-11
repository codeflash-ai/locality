//! In-memory repository implementation.
//!
//! This store is intentionally deterministic and cloneable. It is suitable for
//! unit tests and for wiring CLI/daemon flows before the SQLite schema is built.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use afs_core::AfsResult;
use afs_core::journal::{JournalApplyEffect, JournalEntry, JournalStatus, JournalStore, PushId};
use afs_core::model::{MountId, RemoteId};
use afs_core::shadow::ShadowDocument;

use crate::error::{StoreError, StoreResult};
use crate::records::{
    ConnectionId, ConnectionRecord, ConnectorProfileId, ConnectorProfileRecord, EntityRecord,
    HydrationJobRecord, MountConfig, ShadowSnapshotRecord,
};
use crate::repository::{
    ConnectionRepository, ConnectorProfileRepository, EntityRepository, HydrationJobRepository,
    JournalRepository, MountRepository, ShadowRepository,
};

type EntityKey = (MountId, RemoteId);
type PathKey = (MountId, PathBuf);
type ShadowKey = (MountId, RemoteId);
type HydrationJobKey = (MountId, RemoteId);

#[derive(Clone, Debug, Default)]
pub struct InMemoryStateStore {
    mounts: BTreeMap<MountId, MountConfig>,
    connections: BTreeMap<ConnectionId, ConnectionRecord>,
    connector_profiles: BTreeMap<ConnectorProfileId, ConnectorProfileRecord>,
    entities: BTreeMap<EntityKey, EntityRecord>,
    entities_by_path: BTreeMap<PathKey, RemoteId>,
    shadows: BTreeMap<ShadowKey, ShadowSnapshotRecord>,
    hydration_jobs: BTreeMap<HydrationJobKey, HydrationJobRecord>,
    journals: BTreeMap<String, JournalEntry>,
}

impl InMemoryStateStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn entity_key(mount_id: &MountId, remote_id: &RemoteId) -> EntityKey {
        (mount_id.clone(), remote_id.clone())
    }

    fn path_key(mount_id: &MountId, path: &Path) -> PathKey {
        (mount_id.clone(), path.to_path_buf())
    }

    fn shadow_key(mount_id: &MountId, entity_id: &RemoteId) -> ShadowKey {
        (mount_id.clone(), entity_id.clone())
    }

    fn hydration_job_key(mount_id: &MountId, remote_id: &RemoteId) -> HydrationJobKey {
        (mount_id.clone(), remote_id.clone())
    }
}

impl MountRepository for InMemoryStateStore {
    fn save_mount(&mut self, mount: MountConfig) -> StoreResult<()> {
        self.mounts.insert(mount.mount_id.clone(), mount);
        Ok(())
    }

    fn get_mount(&self, mount_id: &MountId) -> StoreResult<Option<MountConfig>> {
        Ok(self.mounts.get(mount_id).cloned())
    }

    fn load_mounts(&self) -> StoreResult<Vec<MountConfig>> {
        Ok(self.mounts.values().cloned().collect())
    }
}

impl ConnectionRepository for InMemoryStateStore {
    fn save_connection(&mut self, connection: ConnectionRecord) -> StoreResult<()> {
        self.connections
            .insert(connection.connection_id.clone(), connection);
        Ok(())
    }

    fn get_connection(
        &self,
        connection_id: &ConnectionId,
    ) -> StoreResult<Option<ConnectionRecord>> {
        Ok(self.connections.get(connection_id).cloned())
    }

    fn list_connections(&self) -> StoreResult<Vec<ConnectionRecord>> {
        Ok(self.connections.values().cloned().collect())
    }

    fn delete_connection(&mut self, connection_id: &ConnectionId) -> StoreResult<()> {
        self.connections.remove(connection_id);
        Ok(())
    }
}

impl ConnectorProfileRepository for InMemoryStateStore {
    fn save_connector_profile(&mut self, profile: ConnectorProfileRecord) -> StoreResult<()> {
        self.connector_profiles
            .insert(profile.profile_id.clone(), profile);
        Ok(())
    }

    fn get_connector_profile(
        &self,
        profile_id: &ConnectorProfileId,
    ) -> StoreResult<Option<ConnectorProfileRecord>> {
        Ok(self.connector_profiles.get(profile_id).cloned())
    }

    fn list_connector_profiles(&self) -> StoreResult<Vec<ConnectorProfileRecord>> {
        Ok(self.connector_profiles.values().cloned().collect())
    }
}

impl EntityRepository for InMemoryStateStore {
    fn save_entity(&mut self, entity: EntityRecord) -> StoreResult<()> {
        let entity_key = Self::entity_key(&entity.mount_id, &entity.remote_id);
        let path_key = Self::path_key(&entity.mount_id, &entity.path);

        if let Some(existing_remote_id) = self.entities_by_path.get(&path_key)
            && existing_remote_id != &entity.remote_id
        {
            return Err(StoreError::DuplicateEntityPath {
                mount_id: entity.mount_id,
                path: entity.path,
            });
        }

        if let Some(previous) = self.entities.get(&entity_key)
            && previous.path != entity.path
        {
            self.entities_by_path
                .remove(&Self::path_key(&previous.mount_id, &previous.path));
        }

        self.entities_by_path
            .insert(path_key, entity.remote_id.clone());
        self.entities.insert(entity_key, entity);
        Ok(())
    }

    fn get_entity(
        &self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<Option<EntityRecord>> {
        Ok(self
            .entities
            .get(&Self::entity_key(mount_id, remote_id))
            .cloned())
    }

    fn find_entity_by_path(
        &self,
        mount_id: &MountId,
        path: &Path,
    ) -> StoreResult<Option<EntityRecord>> {
        let Some(remote_id) = self
            .entities_by_path
            .get(&Self::path_key(mount_id, path))
            .cloned()
        else {
            return Ok(None);
        };

        self.get_entity(mount_id, &remote_id)
    }

    fn list_entities(&self, mount_id: &MountId) -> StoreResult<Vec<EntityRecord>> {
        Ok(self
            .entities
            .iter()
            .filter(|((entry_mount_id, _), _)| entry_mount_id == mount_id)
            .map(|(_, entity)| entity.clone())
            .collect())
    }
}

impl ShadowRepository for InMemoryStateStore {
    fn save_shadow(&mut self, mount_id: &MountId, shadow: ShadowDocument) -> StoreResult<()> {
        let record = ShadowSnapshotRecord::from_document(mount_id.clone(), &shadow);
        self.shadows
            .insert(Self::shadow_key(mount_id, &shadow.entity_id), record);
        Ok(())
    }

    fn load_shadow(&self, mount_id: &MountId, entity_id: &RemoteId) -> StoreResult<ShadowDocument> {
        self.get_shadow_record(mount_id, entity_id)?
            .map(ShadowSnapshotRecord::into_document)
            .ok_or_else(|| StoreError::ShadowMissing {
                mount_id: mount_id.clone(),
                entity_id: entity_id.clone(),
            })
    }

    fn get_shadow_record(
        &self,
        mount_id: &MountId,
        entity_id: &RemoteId,
    ) -> StoreResult<Option<ShadowSnapshotRecord>> {
        Ok(self
            .shadows
            .get(&Self::shadow_key(mount_id, entity_id))
            .cloned())
    }
}

impl HydrationJobRepository for InMemoryStateStore {
    fn upsert_hydration_job(&mut self, job: HydrationJobRecord) -> StoreResult<()> {
        let key = Self::hydration_job_key(&job.mount_id, &job.remote_id);
        if let Some(existing) = self.hydration_jobs.get_mut(&key) {
            existing.path = job.path;
            existing.target_state = job.target_state;
            existing.reason = job.reason;
        } else {
            self.hydration_jobs.insert(key, job);
        }

        Ok(())
    }

    fn list_hydration_jobs(&self) -> StoreResult<Vec<HydrationJobRecord>> {
        Ok(self.hydration_jobs.values().cloned().collect())
    }

    fn delete_hydration_job(
        &mut self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<()> {
        self.hydration_jobs
            .remove(&Self::hydration_job_key(mount_id, remote_id));
        Ok(())
    }

    fn record_hydration_job_failure(
        &mut self,
        mount_id: &MountId,
        remote_id: &RemoteId,
        message: String,
    ) -> StoreResult<()> {
        if let Some(job) = self
            .hydration_jobs
            .get_mut(&Self::hydration_job_key(mount_id, remote_id))
        {
            job.attempts = job.attempts.saturating_add(1);
            job.last_error = Some(message);
        }

        Ok(())
    }
}

impl JournalRepository for InMemoryStateStore {
    fn append_journal(&mut self, entry: JournalEntry) -> StoreResult<()> {
        if self.journals.contains_key(&entry.push_id.0) {
            return Err(StoreError::JournalAlreadyExists(entry.push_id));
        }

        self.journals.insert(entry.push_id.0.clone(), entry);
        Ok(())
    }

    fn record_journal_apply_effects(
        &mut self,
        push_id: &PushId,
        effects: Vec<JournalApplyEffect>,
    ) -> StoreResult<()> {
        let Some(entry) = self.journals.get_mut(&push_id.0) else {
            return Err(StoreError::JournalMissing(push_id.clone()));
        };

        entry.apply_effects = effects;
        Ok(())
    }

    fn update_journal_status(
        &mut self,
        push_id: &PushId,
        status: JournalStatus,
    ) -> StoreResult<()> {
        let Some(entry) = self.journals.get_mut(&push_id.0) else {
            return Err(StoreError::JournalMissing(push_id.clone()));
        };

        entry.status = status;
        Ok(())
    }

    fn get_journal(&self, push_id: &PushId) -> StoreResult<Option<JournalEntry>> {
        Ok(self.journals.get(&push_id.0).cloned())
    }

    fn list_journal(&self) -> StoreResult<Vec<JournalEntry>> {
        Ok(self.journals.values().cloned().collect())
    }
}

impl JournalStore for InMemoryStateStore {
    fn append(&mut self, entry: JournalEntry) -> AfsResult<()> {
        self.append_journal(entry).map_err(Into::into)
    }

    fn record_apply_effects(
        &mut self,
        push_id: &PushId,
        effects: Vec<JournalApplyEffect>,
    ) -> AfsResult<()> {
        self.record_journal_apply_effects(push_id, effects)
            .map_err(Into::into)
    }

    fn update_status(&mut self, push_id: &PushId, status: JournalStatus) -> AfsResult<()> {
        self.update_journal_status(push_id, status)
            .map_err(Into::into)
    }
}
