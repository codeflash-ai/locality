//! In-memory repository implementation.
//!
//! This store is intentionally deterministic and cloneable. It is suitable for
//! unit tests and for wiring CLI/daemon flows before the SQLite schema is built.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use locality_core::LocalityResult;
use locality_core::journal::{
    JournalApplyEffect, JournalEntry, JournalStatus, JournalStore, PushId,
};
use locality_core::model::{MountId, RemoteId};
use locality_core::shadow::ShadowDocument;

use crate::error::{StoreError, StoreResult};
use crate::pre_hydration::PRE_HYDRATION_SCOPE_KIND;
use crate::records::{
    AutoSaveEnrollmentRecord, ConnectionId, ConnectionRecord, ConnectorProfileId,
    ConnectorProfileRecord, ConnectorStateRecord, EntityRecord, FreshnessStateRecord,
    HydrationJobRecord, MetadataDiscoveryJobRecord, MountConfig, MountLiveModeRecord,
    RemoteObservationRecord, ShadowSnapshotRecord, VirtualMutationRecord,
};
use crate::repository::{
    AutoSaveRepository, ConnectionRepository, ConnectorProfileRepository, ConnectorStateRepository,
    EntityRepository, EntitySearchRepository, FreshnessStateRepository, HydrationJobRepository,
    JournalRepository, MetadataDiscoveryJobRepository, MountLiveModeRepository, MountRepository,
    RemoteObservationRepository, ShadowRepository, VirtualMutationRepository,
};

type EntityKey = (MountId, RemoteId);
type PathKey = (MountId, PathBuf);
type ShadowKey = (MountId, RemoteId);
type HydrationJobKey = (MountId, RemoteId);
type VirtualMutationKey = (MountId, String);
type AutoSaveKey = (MountId, PathBuf);
type RemoteObservationKey = (MountId, RemoteId);
type FreshnessStateKey = (MountId, RemoteId);
type MetadataDiscoveryJobKey = (MountId, String);
type ConnectorStateKey = (String, String, String);

#[derive(Clone, Debug, Default)]
pub struct InMemoryStateStore {
    mounts: BTreeMap<MountId, MountConfig>,
    mount_live_modes: BTreeMap<MountId, MountLiveModeRecord>,
    connections: BTreeMap<ConnectionId, ConnectionRecord>,
    connector_profiles: BTreeMap<ConnectorProfileId, ConnectorProfileRecord>,
    connector_states: BTreeMap<ConnectorStateKey, ConnectorStateRecord>,
    entities: BTreeMap<EntityKey, EntityRecord>,
    entities_by_path: BTreeMap<PathKey, RemoteId>,
    shadows: BTreeMap<ShadowKey, ShadowSnapshotRecord>,
    hydration_jobs: BTreeMap<HydrationJobKey, HydrationJobRecord>,
    virtual_mutations: BTreeMap<VirtualMutationKey, VirtualMutationRecord>,
    auto_save_enrollments: BTreeMap<AutoSaveKey, AutoSaveEnrollmentRecord>,
    remote_observations: BTreeMap<RemoteObservationKey, RemoteObservationRecord>,
    freshness_states: BTreeMap<FreshnessStateKey, FreshnessStateRecord>,
    metadata_discovery_jobs: BTreeMap<MetadataDiscoveryJobKey, MetadataDiscoveryJobRecord>,
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

    fn virtual_mutation_key(mount_id: &MountId, local_id: &str) -> VirtualMutationKey {
        (mount_id.clone(), local_id.to_string())
    }

    fn auto_save_key(mount_id: &MountId, path: &Path) -> AutoSaveKey {
        (mount_id.clone(), path.to_path_buf())
    }

    fn remote_observation_key(mount_id: &MountId, remote_id: &RemoteId) -> RemoteObservationKey {
        (mount_id.clone(), remote_id.clone())
    }

    fn freshness_state_key(mount_id: &MountId, remote_id: &RemoteId) -> FreshnessStateKey {
        (mount_id.clone(), remote_id.clone())
    }

    fn metadata_discovery_job_key(
        mount_id: &MountId,
        container_identifier: &str,
    ) -> MetadataDiscoveryJobKey {
        (mount_id.clone(), container_identifier.to_string())
    }

    fn clear_mount_source_state(&mut self, mount_id: &MountId) {
        self.entities
            .retain(|(entry_mount_id, _), _| entry_mount_id != mount_id);
        self.entities_by_path
            .retain(|(entry_mount_id, _), _| entry_mount_id != mount_id);
        self.shadows
            .retain(|(entry_mount_id, _), _| entry_mount_id != mount_id);
        self.hydration_jobs
            .retain(|(entry_mount_id, _), _| entry_mount_id != mount_id);
        self.virtual_mutations
            .retain(|(entry_mount_id, _), _| entry_mount_id != mount_id);
        self.mount_live_modes
            .retain(|entry_mount_id, _| entry_mount_id != mount_id);
        self.auto_save_enrollments
            .retain(|(entry_mount_id, _), _| entry_mount_id != mount_id);
        self.remote_observations
            .retain(|(entry_mount_id, _), _| entry_mount_id != mount_id);
        self.freshness_states
            .retain(|(entry_mount_id, _), _| entry_mount_id != mount_id);
        self.metadata_discovery_jobs
            .retain(|(entry_mount_id, _), _| entry_mount_id != mount_id);
        self.connector_states
            .retain(|(_, scope_kind, scope_id), _| {
                !matches!(scope_kind.as_str(), "mount" | PRE_HYDRATION_SCOPE_KIND)
                    || scope_id != mount_id.as_str()
            });
        self.journals.retain(|_, entry| entry.mount_id != *mount_id);
    }
}

impl MountRepository for InMemoryStateStore {
    fn save_mount(&mut self, mount: MountConfig) -> StoreResult<()> {
        if self
            .mounts
            .get(&mount.mount_id)
            .is_some_and(|existing| mount_source_identity_changed(existing, &mount))
        {
            self.clear_mount_source_state(&mount.mount_id);
        }
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

impl MountLiveModeRepository for InMemoryStateStore {
    fn save_mount_live_mode(&mut self, live_mode: MountLiveModeRecord) -> StoreResult<()> {
        self.mount_live_modes
            .insert(live_mode.mount_id.clone(), live_mode);
        Ok(())
    }

    fn get_mount_live_mode(&self, mount_id: &MountId) -> StoreResult<Option<MountLiveModeRecord>> {
        Ok(self.mount_live_modes.get(mount_id).cloned())
    }

    fn list_mount_live_modes(&self) -> StoreResult<Vec<MountLiveModeRecord>> {
        Ok(self.mount_live_modes.values().cloned().collect())
    }

    fn delete_mount_live_mode(&mut self, mount_id: &MountId) -> StoreResult<()> {
        self.mount_live_modes.remove(mount_id);
        Ok(())
    }
}

fn mount_source_identity_changed(existing: &MountConfig, next: &MountConfig) -> bool {
    existing.connector != next.connector
        || existing.remote_root_id != next.remote_root_id
        || existing.connection_id != next.connection_id
        || existing.settings_json != next.settings_json
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

impl ConnectorStateRepository for InMemoryStateStore {
    fn save_connector_state(&mut self, state: ConnectorStateRecord) -> StoreResult<()> {
        self.connector_states.insert(
            (
                state.connector.clone(),
                state.scope_kind.clone(),
                state.scope_id.clone(),
            ),
            state,
        );
        Ok(())
    }

    fn get_connector_state(
        &self,
        connector: &str,
        scope_kind: &str,
        scope_id: &str,
    ) -> StoreResult<Option<ConnectorStateRecord>> {
        Ok(self
            .connector_states
            .get(&(
                connector.to_string(),
                scope_kind.to_string(),
                scope_id.to_string(),
            ))
            .cloned())
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
        let mut entities = self
            .entities
            .iter()
            .filter(|((entry_mount_id, _), _)| entry_mount_id == mount_id)
            .map(|(_, entity)| entity.clone())
            .collect::<Vec<_>>();
        entities.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.remote_id.0.cmp(&right.remote_id.0))
        });
        Ok(entities)
    }

    fn delete_entity(&mut self, mount_id: &MountId, remote_id: &RemoteId) -> StoreResult<()> {
        let key = Self::entity_key(mount_id, remote_id);
        if let Some(entity) = self.entities.remove(&key) {
            self.entities_by_path
                .remove(&Self::path_key(&entity.mount_id, &entity.path));
        }
        Ok(())
    }
}

impl EntitySearchRepository for InMemoryStateStore {
    fn list_entity_search_candidates(
        &self,
        _mount_id: &MountId,
        _query: &str,
        _compact_remote_id: Option<&str>,
    ) -> StoreResult<Option<Vec<crate::repository::EntitySearchCandidate>>> {
        Ok(None)
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

impl VirtualMutationRepository for InMemoryStateStore {
    fn save_virtual_mutation(&mut self, mutation: VirtualMutationRecord) -> StoreResult<()> {
        self.virtual_mutations.insert(
            Self::virtual_mutation_key(&mutation.mount_id, &mutation.local_id),
            mutation,
        );
        Ok(())
    }

    fn get_virtual_mutation(
        &self,
        mount_id: &MountId,
        local_id: &str,
    ) -> StoreResult<Option<VirtualMutationRecord>> {
        Ok(self
            .virtual_mutations
            .get(&Self::virtual_mutation_key(mount_id, local_id))
            .cloned())
    }

    fn find_virtual_mutation_by_path(
        &self,
        mount_id: &MountId,
        path: &Path,
    ) -> StoreResult<Option<VirtualMutationRecord>> {
        Ok(self
            .virtual_mutations
            .values()
            .find(|mutation| mutation.mount_id == *mount_id && mutation.projected_path == path)
            .cloned())
    }

    fn list_virtual_mutations(
        &self,
        mount_id: &MountId,
    ) -> StoreResult<Vec<VirtualMutationRecord>> {
        let mut mutations = self
            .virtual_mutations
            .values()
            .filter(|mutation| mutation.mount_id == *mount_id)
            .cloned()
            .collect::<Vec<_>>();
        mutations.sort_by(|left, right| {
            left.projected_path
                .cmp(&right.projected_path)
                .then_with(|| left.local_id.cmp(&right.local_id))
        });
        Ok(mutations)
    }

    fn delete_virtual_mutation(&mut self, mount_id: &MountId, local_id: &str) -> StoreResult<()> {
        self.virtual_mutations
            .remove(&Self::virtual_mutation_key(mount_id, local_id));
        Ok(())
    }
}

impl AutoSaveRepository for InMemoryStateStore {
    fn save_auto_save_enrollment(
        &mut self,
        enrollment: AutoSaveEnrollmentRecord,
    ) -> StoreResult<()> {
        self.auto_save_enrollments.insert(
            Self::auto_save_key(&enrollment.mount_id, &enrollment.path),
            enrollment,
        );
        Ok(())
    }

    fn get_auto_save_enrollment(
        &self,
        mount_id: &MountId,
        path: &Path,
    ) -> StoreResult<Option<AutoSaveEnrollmentRecord>> {
        Ok(self
            .auto_save_enrollments
            .get(&Self::auto_save_key(mount_id, path))
            .cloned())
    }

    fn find_auto_save_enrollment_by_remote_id(
        &self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<Option<AutoSaveEnrollmentRecord>> {
        Ok(self
            .auto_save_enrollments
            .values()
            .find(|enrollment| {
                enrollment.mount_id == *mount_id && enrollment.remote_id.as_ref() == Some(remote_id)
            })
            .cloned())
    }

    fn list_auto_save_enrollments(
        &self,
        mount_id: &MountId,
    ) -> StoreResult<Vec<AutoSaveEnrollmentRecord>> {
        let mut enrollments = self
            .auto_save_enrollments
            .values()
            .filter(|enrollment| enrollment.mount_id == *mount_id)
            .cloned()
            .collect::<Vec<_>>();
        enrollments.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(enrollments)
    }

    fn delete_auto_save_enrollment(&mut self, mount_id: &MountId, path: &Path) -> StoreResult<()> {
        self.auto_save_enrollments
            .remove(&Self::auto_save_key(mount_id, path));
        Ok(())
    }
}

impl RemoteObservationRepository for InMemoryStateStore {
    fn save_remote_observation(&mut self, observation: RemoteObservationRecord) -> StoreResult<()> {
        self.remote_observations.insert(
            Self::remote_observation_key(&observation.mount_id, &observation.remote_id),
            observation,
        );
        Ok(())
    }

    fn get_remote_observation(
        &self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<Option<RemoteObservationRecord>> {
        Ok(self
            .remote_observations
            .get(&Self::remote_observation_key(mount_id, remote_id))
            .cloned())
    }

    fn list_remote_observations(
        &self,
        mount_id: &MountId,
    ) -> StoreResult<Vec<RemoteObservationRecord>> {
        let mut observations = self
            .remote_observations
            .values()
            .filter(|observation| observation.mount_id == *mount_id)
            .cloned()
            .collect::<Vec<_>>();
        observations.sort_by(|left, right| {
            left.projected_path
                .cmp(&right.projected_path)
                .then_with(|| left.remote_id.0.cmp(&right.remote_id.0))
        });
        Ok(observations)
    }

    fn delete_remote_observation(
        &mut self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<()> {
        self.remote_observations
            .remove(&Self::remote_observation_key(mount_id, remote_id));
        Ok(())
    }
}

impl FreshnessStateRepository for InMemoryStateStore {
    fn save_freshness_state(&mut self, state: FreshnessStateRecord) -> StoreResult<()> {
        self.freshness_states.insert(
            Self::freshness_state_key(&state.mount_id, &state.remote_id),
            state,
        );
        Ok(())
    }

    fn get_freshness_state(
        &self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<Option<FreshnessStateRecord>> {
        Ok(self
            .freshness_states
            .get(&Self::freshness_state_key(mount_id, remote_id))
            .cloned())
    }

    fn list_freshness_states(&self, mount_id: &MountId) -> StoreResult<Vec<FreshnessStateRecord>> {
        Ok(self
            .freshness_states
            .values()
            .filter(|state| state.mount_id == *mount_id)
            .cloned()
            .collect())
    }

    fn delete_freshness_state(
        &mut self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<()> {
        self.freshness_states
            .remove(&Self::freshness_state_key(mount_id, remote_id));
        Ok(())
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

impl MetadataDiscoveryJobRepository for InMemoryStateStore {
    fn upsert_metadata_discovery_job(
        &mut self,
        job: MetadataDiscoveryJobRecord,
    ) -> StoreResult<()> {
        let key = Self::metadata_discovery_job_key(&job.mount_id, &job.container_identifier);
        if let Some(existing) = self.metadata_discovery_jobs.get_mut(&key) {
            existing.priority = existing.priority.max(job.priority);
            existing.depth = job.depth;
            existing.updated_at = job.updated_at;
        } else {
            self.metadata_discovery_jobs.insert(key, job);
        }
        Ok(())
    }

    fn list_metadata_discovery_jobs(&self) -> StoreResult<Vec<MetadataDiscoveryJobRecord>> {
        let mut jobs = self
            .metadata_discovery_jobs
            .values()
            .cloned()
            .collect::<Vec<_>>();
        jobs.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.depth.cmp(&right.depth))
                .then_with(|| left.attempts.cmp(&right.attempts))
                .then_with(|| left.mount_id.0.cmp(&right.mount_id.0))
                .then_with(|| left.container_identifier.cmp(&right.container_identifier))
        });
        Ok(jobs)
    }

    fn delete_metadata_discovery_job(
        &mut self,
        mount_id: &MountId,
        container_identifier: &str,
    ) -> StoreResult<()> {
        self.metadata_discovery_jobs
            .remove(&Self::metadata_discovery_job_key(
                mount_id,
                container_identifier,
            ));
        Ok(())
    }

    fn record_metadata_discovery_job_failure(
        &mut self,
        mount_id: &MountId,
        container_identifier: &str,
        message: String,
    ) -> StoreResult<()> {
        if let Some(job) = self
            .metadata_discovery_jobs
            .get_mut(&Self::metadata_discovery_job_key(
                mount_id,
                container_identifier,
            ))
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
    fn append(&mut self, entry: JournalEntry) -> LocalityResult<()> {
        self.append_journal(entry).map_err(Into::into)
    }

    fn record_apply_effects(
        &mut self,
        push_id: &PushId,
        effects: Vec<JournalApplyEffect>,
    ) -> LocalityResult<()> {
        self.record_journal_apply_effects(push_id, effects)
            .map_err(Into::into)
    }

    fn update_status(&mut self, push_id: &PushId, status: JournalStatus) -> LocalityResult<()> {
        self.update_journal_status(push_id, status)
            .map_err(Into::into)
    }
}
