//! Atomic durable boundary for connector batch discovery.
//!
//! Discovery policy belongs to the daemon. This module only validates a fully
//! prepared commit and requires implementations to publish its checkpoint with
//! the associated entity state atomically.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use locality_core::journal::JournalEntry;
use locality_core::model::{MountId, RemoteId};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{StoreError, StoreResult};
use crate::records::{
    AutoSaveEnrollmentRecord, ConnectorStateRecord, EntityRecord, FreshnessStateRecord,
    HydrationJobRecord, MetadataDiscoveryJobRecord, MountConfig, MountLiveModeRecord,
    ProjectionMode, RemoteObservationRecord, ShadowSnapshotRecord, VirtualMutationRecord,
};

pub const DISCOVERY_TRANSACTION_STATE_VERSION: i64 = 1;
pub const DISCOVERY_TRANSACTION_MIN_READER_VERSION: i64 = 1;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DiscoveryTransactionId(pub String);

impl DiscoveryTransactionId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryTransactionEnvelope<T> {
    pub state_version: i64,
    pub min_reader_version: i64,
    pub payload: T,
}

impl<T> DiscoveryTransactionEnvelope<T> {
    pub fn current(payload: T) -> Self {
        Self {
            state_version: DISCOVERY_TRANSACTION_STATE_VERSION,
            min_reader_version: DISCOVERY_TRANSACTION_MIN_READER_VERSION,
            payload,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionalDiscoveryCommit {
    pub transaction_id: DiscoveryTransactionId,
    pub commit: DiscoveryCommit,
}

impl TransactionalDiscoveryCommit {
    pub fn new(transaction_id: DiscoveryTransactionId, commit: DiscoveryCommit) -> Self {
        Self {
            transaction_id,
            commit,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryReservation {
    pub mount: MountConfig,
    pub mount_live_mode: Option<MountLiveModeRecord>,
    pub checkpoint: Option<ConnectorStateRecord>,
    pub entities: Vec<EntityRecord>,
    pub shadows: Vec<ShadowSnapshotRecord>,
    pub hydration_jobs: Vec<HydrationJobRecord>,
    pub virtual_mutations: Vec<VirtualMutationRecord>,
    pub auto_save_enrollments: Vec<AutoSaveEnrollmentRecord>,
    pub remote_observations: Vec<RemoteObservationRecord>,
    pub freshness_states: Vec<FreshnessStateRecord>,
    pub metadata_discovery_jobs: Vec<MetadataDiscoveryJobRecord>,
    pub unsettled_journals: Vec<JournalEntry>,
}

impl DiscoveryReservation {
    pub(crate) fn changed_category(&self, current: &Self) -> Option<&'static str> {
        if self.mount != current.mount {
            Some("mount")
        } else if self.mount_live_mode != current.mount_live_mode {
            Some("mount_live_mode")
        } else if self.checkpoint != current.checkpoint {
            Some("checkpoint")
        } else if self.entities != current.entities {
            Some("entities")
        } else if self.shadows != current.shadows {
            Some("shadows")
        } else if self.hydration_jobs != current.hydration_jobs {
            Some("hydration_jobs")
        } else if self.virtual_mutations != current.virtual_mutations {
            Some("virtual_mutations")
        } else if self.auto_save_enrollments != current.auto_save_enrollments {
            Some("auto_save_enrollments")
        } else if self.remote_observations != current.remote_observations {
            Some("remote_observations")
        } else if self.freshness_states != current.freshness_states {
            Some("freshness_states")
        } else if self.metadata_discovery_jobs != current.metadata_discovery_jobs {
            Some("metadata_discovery_jobs")
        } else if self.unsettled_journals != current.unsettled_journals {
            Some("unsettled_journals")
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryTransactionStatus {
    Reserved,
    Applying,
    Projected,
    Committed,
    RepairPending,
    Aborted,
    Finalized,
}

impl DiscoveryTransactionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Applying => "applying",
            Self::Projected => "projected",
            Self::Committed => "committed",
            Self::RepairPending => "repair_pending",
            Self::Aborted => "aborted",
            Self::Finalized => "finalized",
        }
    }

    pub(crate) fn parse(value: &str) -> StoreResult<Self> {
        match value {
            "reserved" => Ok(Self::Reserved),
            "applying" => Ok(Self::Applying),
            "projected" => Ok(Self::Projected),
            "committed" => Ok(Self::Committed),
            "repair_pending" => Ok(Self::RepairPending),
            "aborted" => Ok(Self::Aborted),
            "finalized" => Ok(Self::Finalized),
            _ => Err(StoreError::InvalidState(format!(
                "unknown discovery transaction status `{value}`"
            ))),
        }
    }

    pub(crate) fn is_active(self) -> bool {
        !matches!(self, Self::Aborted | Self::Finalized)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedDiscoveryTransaction {
    pub commit: TransactionalDiscoveryCommit,
    pub projection: ProjectionMode,
    pub plan: Value,
    pub reservation: DiscoveryReservation,
    pub effects: Value,
    pub created_at: String,
}

impl PreparedDiscoveryTransaction {
    pub fn new(
        commit: TransactionalDiscoveryCommit,
        projection: ProjectionMode,
        plan: Value,
        reservation: DiscoveryReservation,
        created_at: impl Into<String>,
    ) -> Self {
        Self {
            commit,
            projection,
            plan: canonicalize_json_value(plan),
            reservation,
            effects: Value::Array(Vec::new()),
            created_at: created_at.into(),
        }
    }

    pub fn with_effects(mut self, effects: Value) -> Self {
        self.effects = canonicalize_json_value(effects);
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveryTransactionRecord {
    pub transaction_id: DiscoveryTransactionId,
    pub mount_id: MountId,
    pub projection: ProjectionMode,
    pub status: DiscoveryTransactionStatus,
    pub active: bool,
    pub plan: Value,
    pub commit: TransactionalDiscoveryCommit,
    pub reservation: DiscoveryReservation,
    pub effects: Value,
    pub error: Option<Value>,
    pub created_at: String,
    pub updated_at: String,
    pub committed_at: Option<String>,
    pub finalized_at: Option<String>,
}

pub trait DiscoveryRepository {
    fn capture_discovery_reservation(
        &self,
        mount_id: &MountId,
    ) -> StoreResult<DiscoveryReservation>;
    fn reserve_discovery_transaction(
        &mut self,
        prepared: PreparedDiscoveryTransaction,
    ) -> StoreResult<DiscoveryTransactionRecord>;
    fn get_discovery_transaction(
        &self,
        transaction_id: &DiscoveryTransactionId,
    ) -> StoreResult<Option<DiscoveryTransactionRecord>>;
    fn list_active_discovery_transactions(&self) -> StoreResult<Vec<DiscoveryTransactionRecord>>;
    fn mark_discovery_transaction_applying(
        &mut self,
        transaction_id: &DiscoveryTransactionId,
        updated_at: &str,
    ) -> StoreResult<DiscoveryTransactionRecord>;
    fn record_discovery_transaction_effects(
        &mut self,
        transaction_id: &DiscoveryTransactionId,
        expected_status: DiscoveryTransactionStatus,
        effects: Value,
        updated_at: &str,
    ) -> StoreResult<DiscoveryTransactionRecord>;
    fn mark_discovery_transaction_projected(
        &mut self,
        transaction_id: &DiscoveryTransactionId,
        expected_status: DiscoveryTransactionStatus,
        updated_at: &str,
    ) -> StoreResult<DiscoveryTransactionRecord>;
    fn commit_discovery_transaction(
        &mut self,
        transaction_id: &DiscoveryTransactionId,
        committed_at: &str,
    ) -> StoreResult<DiscoveryTransactionRecord>;
    fn mark_discovery_transaction_repair_pending(
        &mut self,
        transaction_id: &DiscoveryTransactionId,
        expected_status: DiscoveryTransactionStatus,
        error: Value,
        updated_at: &str,
    ) -> StoreResult<DiscoveryTransactionRecord>;
    fn mark_discovery_transaction_aborted(
        &mut self,
        transaction_id: &DiscoveryTransactionId,
        expected_status: DiscoveryTransactionStatus,
        updated_at: &str,
    ) -> StoreResult<DiscoveryTransactionRecord>;
    fn mark_discovery_transaction_finalized(
        &mut self,
        transaction_id: &DiscoveryTransactionId,
        finalized_at: &str,
    ) -> StoreResult<DiscoveryTransactionRecord>;
}

pub(crate) fn canonical_envelope_json<T: Serialize>(payload: &T) -> StoreResult<String> {
    let envelope = DiscoveryTransactionEnvelope::current(payload);
    canonical_json(&envelope)
}

pub(crate) fn decode_envelope<T: DeserializeOwned>(value: &str, label: &str) -> StoreResult<T> {
    let envelope: DiscoveryTransactionEnvelope<T> = serde_json::from_str(value)?;
    validate_envelope_version(envelope.state_version, envelope.min_reader_version, label)?;
    Ok(envelope.payload)
}

pub(crate) fn validate_envelope_version(
    state_version: i64,
    min_reader_version: i64,
    label: &str,
) -> StoreResult<()> {
    if state_version <= 0 || min_reader_version <= 0 || min_reader_version > state_version {
        return Err(StoreError::InvalidState(format!(
            "discovery transaction {label} envelope has invalid version metadata"
        )));
    }
    if state_version > DISCOVERY_TRANSACTION_STATE_VERSION
        || min_reader_version > DISCOVERY_TRANSACTION_STATE_VERSION
    {
        return Err(StoreError::StateCompatibility(format!(
            "discovery transaction {label} envelope requires version {state_version}, supported {}",
            DISCOVERY_TRANSACTION_STATE_VERSION
        )));
    }
    if state_version != DISCOVERY_TRANSACTION_STATE_VERSION {
        return Err(StoreError::StateCompatibility(format!(
            "discovery transaction {label} envelope version {state_version} requires migration"
        )));
    }
    Ok(())
}

pub(crate) fn canonical_json<T: Serialize>(value: &T) -> StoreResult<String> {
    let value = serde_json::to_value(value)?;
    serde_json::to_string(&canonicalize_json_value(value)).map_err(Into::into)
}

pub(crate) fn canonicalize_json_value(value: Value) -> Value {
    match value {
        Value::Array(values) => {
            Value::Array(values.into_iter().map(canonicalize_json_value).collect())
        }
        Value::Object(values) => {
            let mut entries = values.into_iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, canonicalize_json_value(value)))
                    .collect(),
            )
        }
        value => value,
    }
}

pub(crate) fn record_from_prepared(
    mut prepared: PreparedDiscoveryTransaction,
    current: &DiscoveryReservation,
) -> StoreResult<DiscoveryTransactionRecord> {
    prepared.plan = canonicalize_json_value(prepared.plan);
    prepared.effects = canonicalize_json_value(prepared.effects);
    let transaction_id = &prepared.commit.transaction_id;
    if transaction_id.0.is_empty() {
        return Err(StoreError::InvalidState(
            "discovery transaction identifier cannot be empty".to_string(),
        ));
    }
    if prepared.created_at.is_empty() {
        return Err(StoreError::InvalidState(format!(
            "discovery transaction `{}` created_at cannot be empty",
            transaction_id.0
        )));
    }
    let commit = &prepared.commit.commit;
    if commit.mount_id != prepared.reservation.mount.mount_id {
        return Err(StoreError::InvalidState(format!(
            "discovery transaction `{}` commit mount does not match its reservation",
            transaction_id.0
        )));
    }
    if prepared.projection != prepared.reservation.mount.projection {
        return Err(StoreError::InvalidState(format!(
            "discovery transaction `{}` projection does not match its reservation",
            transaction_id.0
        )));
    }
    if prepared.projection != current.mount.projection {
        return Err(StoreError::InvalidState(format!(
            "discovery transaction `{}` projection does not match current mount projection",
            transaction_id.0
        )));
    }
    if let Some(category) = prepared.reservation.changed_category(current) {
        return Err(reservation_changed(transaction_id, category));
    }
    commit.preflight_details(
        &current.mount.connector,
        &current.entities,
        &current.auto_save_enrollments,
        &current.virtual_mutations,
    )?;

    Ok(DiscoveryTransactionRecord {
        transaction_id: transaction_id.clone(),
        mount_id: commit.mount_id.clone(),
        projection: prepared.projection,
        status: DiscoveryTransactionStatus::Reserved,
        active: true,
        plan: prepared.plan,
        commit: prepared.commit,
        reservation: prepared.reservation,
        effects: prepared.effects,
        error: None,
        created_at: prepared.created_at.clone(),
        updated_at: prepared.created_at,
        committed_at: None,
        finalized_at: None,
    })
}

pub(crate) fn prepared_matches_record(
    prepared: &PreparedDiscoveryTransaction,
    record: &DiscoveryTransactionRecord,
) -> bool {
    prepared.commit == record.commit
        && prepared.projection == record.projection
        && canonicalize_json_value(prepared.plan.clone()) == record.plan
        && prepared.reservation == record.reservation
        && prepared.created_at == record.created_at
}

pub(crate) fn reservation_changed(
    transaction_id: &DiscoveryTransactionId,
    category: &str,
) -> StoreError {
    StoreError::InvalidState(format!(
        "discovery transaction `{}` reservation changed: {category}",
        transaction_id.0
    ))
}

pub(crate) fn transaction_missing(transaction_id: &DiscoveryTransactionId) -> StoreError {
    StoreError::InvalidState(format!(
        "discovery transaction `{}` was not found",
        transaction_id.0
    ))
}

pub(crate) fn require_transaction_status(
    record: &DiscoveryTransactionRecord,
    expected: DiscoveryTransactionStatus,
) -> StoreResult<()> {
    if record.status != expected || !record.active {
        return Err(StoreError::InvalidState(format!(
            "discovery transaction `{}` expected active status `{}`, found `{}`",
            record.transaction_id.0,
            expected.as_str(),
            record.status.as_str()
        )));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DiscoveryPreflight {
    pub final_entities: BTreeMap<RemoteId, EntityRecord>,
    pub entity_deletes: BTreeSet<RemoteId>,
    pub deleted_paths: BTreeMap<RemoteId, PathBuf>,
    pub path_moves: Vec<(RemoteId, PathBuf, PathBuf)>,
    pub auto_save_rehomes: Vec<AutoSaveRehome>,
}

impl DiscoveryCommit {
    /// Validates this commit against a caller-provided snapshot without mutating state.
    ///
    /// Every snapshot row must belong to the commit mount. Repository implementations
    /// use the same preflight calculation immediately before applying a commit.
    pub fn preflight(
        &self,
        connector: &str,
        existing_entities: &[EntityRecord],
        auto_save_enrollments: &[AutoSaveEnrollmentRecord],
        virtual_mutations: &[VirtualMutationRecord],
    ) -> StoreResult<()> {
        self.preflight_details(
            connector,
            existing_entities,
            auto_save_enrollments,
            virtual_mutations,
        )
        .map(|_| ())
    }

    pub(crate) fn preflight_details(
        &self,
        connector: &str,
        existing_entities: &[EntityRecord],
        auto_save_enrollments: &[AutoSaveEnrollmentRecord],
        virtual_mutations: &[VirtualMutationRecord],
    ) -> StoreResult<DiscoveryPreflight> {
        self.validate()?;
        self.validate_connector(connector)?;
        for entity in existing_entities {
            validate_mount(
                "preflight entity",
                &entity.remote_id,
                &entity.mount_id,
                &self.mount_id,
            )?;
        }
        for enrollment in auto_save_enrollments {
            if enrollment.mount_id != self.mount_id {
                return invalid(format!(
                    "discovery preflight auto-save enrollment `{}` belongs to mount `{}`, expected `{}`",
                    enrollment.path.display(),
                    enrollment.mount_id.0,
                    self.mount_id.0
                ));
            }
        }
        for mutation in virtual_mutations {
            if mutation.mount_id != self.mount_id {
                return invalid(format!(
                    "discovery preflight virtual mutation `{}` belongs to mount `{}`, expected `{}`",
                    mutation.local_id, mutation.mount_id.0, self.mount_id.0
                ));
            }
        }

        let final_entities = self.final_entity_map(existing_entities)?;
        let existing_by_id = existing_entities
            .iter()
            .map(|entity| (entity.remote_id.clone(), entity))
            .collect::<BTreeMap<_, _>>();
        let entity_deletes = self.entity_deletes.iter().cloned().collect::<BTreeSet<_>>();
        let deleted_paths = self
            .entity_deletes
            .iter()
            .filter_map(|remote_id| {
                existing_by_id
                    .get(remote_id)
                    .map(|entity| (remote_id.clone(), entity.path.clone()))
            })
            .collect::<BTreeMap<_, _>>();
        let path_moves = self
            .entity_upserts
            .iter()
            .filter_map(|entity| {
                let existing = existing_by_id.get(&entity.remote_id)?;
                (existing.path != entity.path).then(|| {
                    (
                        entity.remote_id.clone(),
                        existing.path.clone(),
                        entity.path.clone(),
                    )
                })
            })
            .collect::<Vec<_>>();
        let mut affected_entities = self
            .entity_deletes
            .iter()
            .map(|remote_id| {
                (
                    remote_id.clone(),
                    existing_by_id
                        .get(remote_id)
                        .map(|entity| entity.path.clone()),
                )
            })
            .collect::<Vec<_>>();
        affected_entities.extend(
            path_moves
                .iter()
                .map(|(remote_id, old_path, _)| (remote_id.clone(), Some(old_path.clone()))),
        );
        let mut affected_remote_ids = entity_deletes.clone();
        let mut affected_paths = deleted_paths.values().cloned().collect::<BTreeSet<_>>();
        for (remote_id, old_path, new_path) in &path_moves {
            affected_remote_ids.insert(remote_id.clone());
            affected_paths.insert(old_path.clone());
            affected_paths.insert(new_path.clone());
        }
        self.validate_virtual_mutation_changes(
            virtual_mutations,
            &affected_remote_ids,
            &affected_paths,
        )?;
        let auto_save_rehomes = self.plan_auto_save_changes(
            auto_save_enrollments,
            &affected_entities,
            &path_moves,
            &final_entities,
        )?;

        Ok(DiscoveryPreflight {
            final_entities,
            entity_deletes,
            deleted_paths,
            path_moves,
            auto_save_rehomes,
        })
    }

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
            discovery_auto_save_candidate(enrollments, remote_id, old_path.as_deref())?;
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

/// Resolves the only auto-save enrollment that can belong to an entity change.
///
/// A path bound to another remote entity or multiple ID/path candidates is invalid
/// durable state and must be rejected before projection work begins.
pub fn discovery_auto_save_candidate<'a>(
    enrollments: &'a [AutoSaveEnrollmentRecord],
    remote_id: &RemoteId,
    owned_path: Option<&Path>,
) -> StoreResult<Option<&'a AutoSaveEnrollmentRecord>> {
    if let Some(enrollment) = owned_path.and_then(|path| {
        enrollments
            .iter()
            .find(|enrollment| enrollment.path == path)
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

    let mut candidates = enrollments.iter().filter(|enrollment| {
        enrollment.remote_id.as_ref() == Some(remote_id)
            || owned_path.is_some_and(|path| enrollment.path == path)
    });
    let candidate = candidates.next();
    if candidates.next().is_some() {
        return invalid(format!(
            "multiple auto-save enrollments belong to entity `{}`",
            remote_id.0
        ));
    }
    Ok(candidate)
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
