//! Durable, connector-neutral execution of plain-file discovery projections.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use locality_core::hydration::HydrationRequest;
use locality_core::model::{EntityKind, MountId, RemoteId};
use locality_core::path_projection::{is_page_document_path, page_container_path};
use locality_core::{LocalityError, LocalityResult};
use locality_store::{
    DiscoveryCommit, DiscoveryRepository, DiscoveryTransactionId, DiscoveryTransactionRecord,
    DiscoveryTransactionStatus, EntityRecord, HydrationJobRecord, HydrationJobRepository,
    PreparedDiscoveryTransaction, ProjectionMode, StoreError, TransactionalDiscoveryCommit,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::discovery::{
    DiscoveryPlan, DiscoveryPostCommitAction, DiscoveryProjectionAction,
    DiscoveryProjectionComponent, ProjectionStructuralChange,
    build_discovery_projection_components, projection_action_covers_change,
    projection_structural_change,
};
use crate::durable_fs::{
    create_dir_all_durable, remove_dir_all_durable, remove_path_durable, rename_noreplace_durable,
    same_volume, write_new_file_durable,
};

pub const DISCOVERY_EXECUTION_STATE_VERSION: i64 = 1;
pub const DISCOVERY_EXECUTION_MIN_READER_VERSION: i64 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryExecutionPlan {
    pub state_version: i64,
    pub min_reader_version: i64,
    pub transaction_id: DiscoveryTransactionId,
    pub mount_id: MountId,
    pub mount_root: PathBuf,
    pub recovery_root: PathBuf,
    pub components: Vec<DiscoveryExecutionComponent>,
    pub hydration_jobs: Vec<HydrationRequest>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryExecutionComponent {
    pub component: DiscoveryProjectionComponent,
    pub operations: Vec<DiscoveryExecutionOperation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DiscoveryExecutionOperation {
    Create {
        operation_id: String,
        action_index: u32,
        remote_id: RemoteId,
        kind: EntityKind,
        projected_path: PathBuf,
        destination: PathBuf,
        payload: PathBuf,
        temporary_payload: PathBuf,
        expected_fingerprint: DiscoveryPathFingerprint,
        materialization: DiscoveryCreateMaterialization,
    },
    CreateContainer {
        operation_id: String,
        action_index: u32,
        remote_id: RemoteId,
        projected_from: PathBuf,
        projected_to: PathBuf,
        destination: PathBuf,
        payload: PathBuf,
        temporary_payload: PathBuf,
        expected_fingerprint: DiscoveryPathFingerprint,
    },
    Move {
        operation_id: String,
        action_index: u32,
        remote_id: RemoteId,
        kind: EntityKind,
        projected_from: PathBuf,
        projected_to: PathBuf,
        source: PathBuf,
        stage: PathBuf,
        destination: PathBuf,
        expected_fingerprint: DiscoveryPathFingerprint,
    },
    Delete {
        operation_id: String,
        action_index: u32,
        remote_id: RemoteId,
        kind: EntityKind,
        projected_path: PathBuf,
        source: PathBuf,
        stage: PathBuf,
        expected_fingerprint: DiscoveryPathFingerprint,
    },
}

impl DiscoveryExecutionOperation {
    fn operation_id(&self) -> &str {
        match self {
            Self::Create { operation_id, .. }
            | Self::CreateContainer { operation_id, .. }
            | Self::Move { operation_id, .. }
            | Self::Delete { operation_id, .. } => operation_id,
        }
    }

    fn expected_fingerprint(&self) -> &DiscoveryPathFingerprint {
        match self {
            Self::Create {
                expected_fingerprint,
                ..
            }
            | Self::CreateContainer {
                expected_fingerprint,
                ..
            }
            | Self::Move {
                expected_fingerprint,
                ..
            }
            | Self::Delete {
                expected_fingerprint,
                ..
            } => expected_fingerprint,
        }
    }

    fn destination(&self) -> Option<&Path> {
        match self {
            Self::Create { destination, .. }
            | Self::CreateContainer { destination, .. }
            | Self::Move { destination, .. } => Some(destination),
            Self::Delete { .. } => None,
        }
    }

    fn source(&self) -> Option<&Path> {
        match self {
            Self::Move { source, .. } | Self::Delete { source, .. } => Some(source),
            Self::Create { .. } | Self::CreateContainer { .. } => None,
        }
    }

    fn action_index(&self) -> usize {
        match self {
            Self::Create { action_index, .. }
            | Self::CreateContainer { action_index, .. }
            | Self::Move { action_index, .. }
            | Self::Delete { action_index, .. } => *action_index as usize,
        }
    }

    fn set_expected_fingerprint(&mut self, fingerprint: DiscoveryPathFingerprint) {
        match self {
            Self::Create {
                expected_fingerprint,
                ..
            }
            | Self::CreateContainer {
                expected_fingerprint,
                ..
            }
            | Self::Move {
                expected_fingerprint,
                ..
            }
            | Self::Delete {
                expected_fingerprint,
                ..
            } => *expected_fingerprint = fingerprint,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryPathKind {
    File,
    Directory,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryPathFingerprint {
    pub kind: DiscoveryPathKind,
    pub sha256: String,
    pub entries: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DiscoveryFingerprintRecord {
    path: String,
    kind: DiscoveryPathKind,
    bytes: u64,
    content_sha256: Option<[u8; 32]>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiscoveryCreateMaterialization {
    Page {
        remote_id: RemoteId,
        document: String,
    },
    Database {
        remote_id: RemoteId,
        schema_yaml: Option<String>,
    },
    Directory {
        remote_id: RemoteId,
    },
}

impl DiscoveryCreateMaterialization {
    fn remote_id(&self) -> &RemoteId {
        match self {
            Self::Page { remote_id, .. }
            | Self::Database { remote_id, .. }
            | Self::Directory { remote_id } => remote_id,
        }
    }

    fn kind(&self) -> EntityKind {
        match self {
            Self::Page { .. } => EntityKind::Page,
            Self::Database { .. } => EntityKind::Database,
            Self::Directory { .. } => EntityKind::Directory,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryExecutionEffects {
    pub state_version: i64,
    pub min_reader_version: i64,
    pub operations: Vec<DiscoveryOperationEffect>,
    pub hydration_jobs: Vec<DiscoveryHydrationEffect>,
    #[serde(default)]
    pub projection_validated: bool,
    #[serde(default)]
    pub rollback_reason: Option<String>,
    pub cleanup_complete: bool,
    pub completion_recorded: bool,
}

impl Default for DiscoveryExecutionEffects {
    fn default() -> Self {
        Self {
            state_version: DISCOVERY_EXECUTION_STATE_VERSION,
            min_reader_version: DISCOVERY_EXECUTION_MIN_READER_VERSION,
            operations: Vec::new(),
            hydration_jobs: Vec::new(),
            projection_validated: false,
            rollback_reason: None,
            cleanup_complete: false,
            completion_recorded: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryOperationEffect {
    pub operation_id: String,
    pub state: DiscoveryOperationEffectState,
    pub fingerprint: DiscoveryPathFingerprint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryOperationEffectState {
    Prepared,
    Staged,
    Installed,
    RolledBack,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryHydrationEffect {
    pub remote_id: RemoteId,
    pub upserted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscoveryExecutionStep {
    Applying,
    RollbackStarted,
    OperationPrepared {
        operation_id: String,
    },
    FilesystemMutation {
        operation_id: String,
        mutation: DiscoveryFilesystemMutation,
    },
    OperationRecorded {
        operation_id: String,
        state: DiscoveryOperationEffectState,
    },
    Projected,
    Committed,
    ProjectionValidated,
    HydrationJobUpserted {
        remote_id: RemoteId,
    },
    RecoveryPayloadsRemoved,
    CleanupComplete,
    CompletionRecorded,
    Finalized,
    Aborted,
    RepairPending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscoveryFilesystemMutation {
    PayloadPublished,
    SourceStaged,
    DestinationInstalled,
    RolledBack,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscoveryExecutionTerminal {
    Finalized,
    Aborted,
    NeedsReview,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveryRepairResult {
    pub transaction_id: DiscoveryTransactionId,
    pub outcome: DiscoveryExecutionTerminal,
}

pub fn prepare_plain_files_discovery_transaction<S>(
    store: &mut S,
    plan: DiscoveryPlan,
    transaction_id: DiscoveryTransactionId,
    created_at: &str,
    materializations: Vec<DiscoveryCreateMaterialization>,
) -> LocalityResult<DiscoveryTransactionRecord>
where
    S: DiscoveryRepository,
{
    if build_discovery_projection_components(&plan.projection_actions) != plan.projection_components
    {
        return invalid("discovery projection components do not match projection actions");
    }
    let commit = plan.commit().clone();
    let reservation = store
        .capture_discovery_reservation(&commit.mount_id)
        .map_err(repository_error)?;
    validate_projection_commit_coverage(&plan.projection_actions, &commit, &reservation.entities)?;
    if reservation.mount.projection != ProjectionMode::PlainFiles {
        return invalid(format!(
            "discovery projection execution supports only plain_files, found {}",
            reservation.mount.projection.as_str()
        ));
    }
    validate_mount_root(&reservation.mount.root)?;
    let recovery_root = discovery_recovery_root(
        &reservation.mount.root,
        &reservation.mount.mount_id,
        &transaction_id,
    )?;
    require_recovery_root_absent(&recovery_root)?;
    let hydration_jobs = plain_files_hydration_actions(plan.post_commit)?;
    let components = prepare_components(
        &reservation.mount.root,
        plan.projection_components,
        materializations,
    )?;
    let execution = DiscoveryExecutionPlan {
        state_version: DISCOVERY_EXECUTION_STATE_VERSION,
        min_reader_version: DISCOVERY_EXECUTION_MIN_READER_VERSION,
        transaction_id: transaction_id.clone(),
        mount_id: commit.mount_id.clone(),
        mount_root: reservation.mount.root.clone(),
        recovery_root,
        components,
        hydration_jobs,
    };
    let prepared = PreparedDiscoveryTransaction::new(
        TransactionalDiscoveryCommit::new(transaction_id, commit),
        ProjectionMode::PlainFiles,
        serde_json::to_value(execution).map_err(invalid_json)?,
        reservation,
        created_at,
    )
    .with_effects(
        serde_json::to_value(DiscoveryExecutionEffects::default()).map_err(invalid_json)?,
    );
    store
        .reserve_discovery_transaction(prepared)
        .map_err(repository_error)
}

pub fn step_plain_files_discovery_transaction<S>(
    store: &mut S,
    transaction_id: &DiscoveryTransactionId,
    updated_at: &str,
) -> LocalityResult<DiscoveryExecutionStep>
where
    S: DiscoveryRepository + HydrationJobRepository,
{
    let record = store
        .get_discovery_transaction(transaction_id)
        .map_err(repository_error)?
        .ok_or_else(|| {
            LocalityError::Io(format!(
                "discovery transaction `{}` was not found",
                transaction_id.0
            ))
        })?;
    preflight_execution_record(&record)?;
    match record.status {
        DiscoveryTransactionStatus::Finalized => {
            return Ok(DiscoveryExecutionStep::Finalized);
        }
        DiscoveryTransactionStatus::Aborted => {
            return Ok(DiscoveryExecutionStep::Aborted);
        }
        _ => {}
    }
    let (execution, mut effects) = match decode_and_validate_execution(&record) {
        Ok(decoded) => decoded,
        Err(error) => {
            if record.projection == ProjectionMode::PlainFiles
                && matches!(
                    record.status,
                    DiscoveryTransactionStatus::Applying | DiscoveryTransactionStatus::Projected
                )
                && !matches!(error, LocalityError::UpdateRequired { .. })
            {
                store
                    .mark_discovery_transaction_repair_pending(
                        transaction_id,
                        record.status,
                        serde_json::json!({"reason": error.to_string()}),
                        updated_at,
                    )
                    .map_err(repository_error)?;
            }
            return Err(error);
        }
    };
    let result = (|| -> LocalityResult<DiscoveryExecutionStep> {
        match record.status {
            DiscoveryTransactionStatus::Reserved => {
                require_recovery_root_absent(&execution.recovery_root)?;
                if !effects.operations.is_empty()
                    || !effects.hydration_jobs.is_empty()
                    || effects.projection_validated
                    || effects.rollback_reason.is_some()
                    || effects.cleanup_complete
                    || effects.completion_recorded
                {
                    return invalid(format!(
                        "reserved discovery transaction `{}` has execution effects",
                        transaction_id.0
                    ));
                }
                store
                    .mark_discovery_transaction_applying(transaction_id, updated_at)
                    .map_err(repository_error)?;
                Ok(DiscoveryExecutionStep::Applying)
            }
            DiscoveryTransactionStatus::Applying => {
                if effects.rollback_reason.is_some() {
                    step_rollback(store, &record, &execution, &mut effects, updated_at)
                } else {
                    step_applying(store, &record, &execution, &mut effects, updated_at)
                }
            }
            DiscoveryTransactionStatus::Projected => {
                if effects.rollback_reason.is_some() {
                    step_rollback(store, &record, &execution, &mut effects, updated_at)
                } else {
                    validate_installed_projection(&execution, &effects)?;
                    validate_committed_recovery_tree(&execution, &effects, false)?;
                    store
                        .commit_discovery_transaction(transaction_id, updated_at)
                        .map_err(repository_error)?;
                    Ok(DiscoveryExecutionStep::Committed)
                }
            }
            DiscoveryTransactionStatus::Committed => {
                step_committed(store, &record, &execution, &mut effects, updated_at)
            }
            DiscoveryTransactionStatus::Finalized | DiscoveryTransactionStatus::Aborted => {
                Ok(if record.status == DiscoveryTransactionStatus::Finalized {
                    DiscoveryExecutionStep::Finalized
                } else {
                    DiscoveryExecutionStep::Aborted
                })
            }
            DiscoveryTransactionStatus::RepairPending => {
                if effects.rollback_reason.is_some() {
                    step_rollback(store, &record, &execution, &mut effects, updated_at)
                } else {
                    Ok(DiscoveryExecutionStep::RepairPending)
                }
            }
        }
    })();
    match result {
        Ok(step) => Ok(step),
        Err(error)
            if matches!(
                record.status,
                DiscoveryTransactionStatus::Applying | DiscoveryTransactionStatus::Projected
            ) =>
        {
            handle_precommit_failure(store, &record, &mut effects, error, updated_at)
        }
        Err(error) => Err(error),
    }
}

pub fn validate_plain_files_discovery_transaction_record(
    record: &DiscoveryTransactionRecord,
) -> LocalityResult<()> {
    decode_and_validate_execution(record).map(|_| ())
}

pub fn run_plain_files_discovery_transaction<S>(
    store: &mut S,
    transaction_id: &DiscoveryTransactionId,
    updated_at: &str,
) -> LocalityResult<DiscoveryExecutionTerminal>
where
    S: DiscoveryRepository + HydrationJobRepository,
{
    let record = get_discovery_transaction(store, transaction_id)?;
    preflight_execution_record(&record)?;
    match record.status {
        DiscoveryTransactionStatus::Finalized => {
            return Ok(DiscoveryExecutionTerminal::Finalized);
        }
        DiscoveryTransactionStatus::Aborted => {
            return Ok(DiscoveryExecutionTerminal::Aborted);
        }
        _ => {}
    }
    let Some((execution, _)) = decode_for_public_execution(&record)? else {
        return Ok(DiscoveryExecutionTerminal::NeedsReview);
    };
    drive_plain_files_discovery_transaction(store, transaction_id, updated_at, &execution)
}

pub fn repair_plain_files_discovery_transaction<S>(
    store: &mut S,
    transaction_id: &DiscoveryTransactionId,
    updated_at: &str,
) -> LocalityResult<DiscoveryExecutionTerminal>
where
    S: DiscoveryRepository + HydrationJobRepository,
{
    let record = get_discovery_transaction(store, transaction_id)?;
    preflight_execution_record(&record)?;
    match record.status {
        DiscoveryTransactionStatus::Finalized => Ok(DiscoveryExecutionTerminal::Finalized),
        DiscoveryTransactionStatus::Aborted => Ok(DiscoveryExecutionTerminal::Aborted),
        DiscoveryTransactionStatus::Reserved => {
            let effects = serde_json::from_value::<DiscoveryExecutionEffects>(record.effects)
                .map_err(invalid_json)?;
            if effects != DiscoveryExecutionEffects::default() {
                return invalid(format!(
                    "reserved discovery transaction `{}` has execution effects",
                    transaction_id.0
                ));
            }
            store
                .mark_discovery_transaction_aborted(
                    transaction_id,
                    DiscoveryTransactionStatus::Reserved,
                    updated_at,
                )
                .map_err(repository_error)?;
            Ok(DiscoveryExecutionTerminal::Aborted)
        }
        DiscoveryTransactionStatus::RepairPending => {
            let Some((execution, effects)) = decode_for_public_execution(&record)? else {
                return Ok(DiscoveryExecutionTerminal::NeedsReview);
            };
            if effects.rollback_reason.is_none() {
                return Ok(DiscoveryExecutionTerminal::NeedsReview);
            }
            drive_plain_files_discovery_transaction(store, transaction_id, updated_at, &execution)
        }
        DiscoveryTransactionStatus::Applying
        | DiscoveryTransactionStatus::Projected
        | DiscoveryTransactionStatus::Committed => {
            let Some((execution, _)) = decode_for_public_execution(&record)? else {
                return Ok(DiscoveryExecutionTerminal::NeedsReview);
            };
            drive_plain_files_discovery_transaction(store, transaction_id, updated_at, &execution)
        }
    }
}

pub fn repair_active_plain_files_discovery_transactions<S>(
    store: &mut S,
    updated_at: &str,
) -> LocalityResult<Vec<DiscoveryRepairResult>>
where
    S: DiscoveryRepository + HydrationJobRepository,
{
    let transactions = store
        .list_active_discovery_transactions()
        .map_err(repository_error)?;
    let mut results = Vec::new();
    for transaction in transactions
        .into_iter()
        .filter(|transaction| transaction.projection == ProjectionMode::PlainFiles)
    {
        let outcome = repair_plain_files_discovery_transaction(
            store,
            &transaction.transaction_id,
            updated_at,
        )?;
        results.push(DiscoveryRepairResult {
            transaction_id: transaction.transaction_id,
            outcome,
        });
    }
    Ok(results)
}

fn drive_plain_files_discovery_transaction<S>(
    store: &mut S,
    transaction_id: &DiscoveryTransactionId,
    updated_at: &str,
    execution: &DiscoveryExecutionPlan,
) -> LocalityResult<DiscoveryExecutionTerminal>
where
    S: DiscoveryRepository + HydrationJobRepository,
{
    for _ in 0..discovery_execution_step_bound(execution) {
        let step = match step_plain_files_discovery_transaction(store, transaction_id, updated_at) {
            Ok(step) => step,
            Err(LocalityError::InvalidState(_)) => {
                return Ok(DiscoveryExecutionTerminal::NeedsReview);
            }
            Err(error) => return Err(error),
        };
        match step {
            DiscoveryExecutionStep::Finalized => {
                return Ok(DiscoveryExecutionTerminal::Finalized);
            }
            DiscoveryExecutionStep::Aborted => {
                return Ok(DiscoveryExecutionTerminal::Aborted);
            }
            DiscoveryExecutionStep::RepairPending => {
                return Ok(DiscoveryExecutionTerminal::NeedsReview);
            }
            _ => {}
        }
    }
    Ok(DiscoveryExecutionTerminal::NeedsReview)
}

fn discovery_execution_step_bound(execution: &DiscoveryExecutionPlan) -> usize {
    let operations = execution
        .components
        .iter()
        .map(|component| component.operations.len())
        .sum::<usize>();
    operations
        .saturating_mul(12)
        .saturating_add(execution.hydration_jobs.len().saturating_mul(2))
        .saturating_add(32)
}

fn get_discovery_transaction<S>(
    store: &S,
    transaction_id: &DiscoveryTransactionId,
) -> LocalityResult<DiscoveryTransactionRecord>
where
    S: DiscoveryRepository,
{
    store
        .get_discovery_transaction(transaction_id)
        .map_err(repository_error)?
        .ok_or_else(|| {
            LocalityError::Io(format!(
                "discovery transaction `{}` was not found",
                transaction_id.0
            ))
        })
}

fn decode_for_public_execution(
    record: &DiscoveryTransactionRecord,
) -> LocalityResult<Option<(DiscoveryExecutionPlan, DiscoveryExecutionEffects)>> {
    match decode_and_validate_execution(record) {
        Ok(decoded) => Ok(Some(decoded)),
        Err(LocalityError::InvalidState(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

fn handle_precommit_failure<S>(
    store: &mut S,
    record: &DiscoveryTransactionRecord,
    effects: &mut DiscoveryExecutionEffects,
    error: LocalityError,
    updated_at: &str,
) -> LocalityResult<DiscoveryExecutionStep>
where
    S: DiscoveryRepository,
{
    if effects.rollback_reason.is_some() {
        store
            .mark_discovery_transaction_repair_pending(
                &record.transaction_id,
                record.status,
                serde_json::json!({"reason": error.to_string()}),
                updated_at,
            )
            .map_err(repository_error)?;
        return Ok(DiscoveryExecutionStep::RepairPending);
    }
    effects.rollback_reason = Some(error.to_string());
    record_effects(
        store,
        &record.transaction_id,
        record.status,
        effects,
        updated_at,
    )?;
    Ok(DiscoveryExecutionStep::RollbackStarted)
}

fn step_applying<S>(
    store: &mut S,
    record: &DiscoveryTransactionRecord,
    execution: &DiscoveryExecutionPlan,
    effects: &mut DiscoveryExecutionEffects,
    updated_at: &str,
) -> LocalityResult<DiscoveryExecutionStep>
where
    S: DiscoveryRepository + HydrationJobRepository,
{
    if effects.operations.is_empty() {
        require_recovery_root_absent(&execution.recovery_root)?;
    }
    for component in &execution.components {
        let mut sources = component
            .operations
            .iter()
            .filter(|operation| operation.source().is_some())
            .collect::<Vec<_>>();
        sources.sort_by(|left, right| {
            let left_source = left.source().expect("source operation");
            let right_source = right.source().expect("source operation");
            right_source
                .components()
                .count()
                .cmp(&left_source.components().count())
                .then_with(|| left_source.cmp(right_source))
                .then_with(|| left.operation_id().cmp(right.operation_id()))
        });
        for operation in sources {
            match operation_effect_state(effects, operation) {
                None => {
                    return prepare_operation(
                        store, record, execution, effects, operation, updated_at,
                    );
                }
                Some((effect_index, DiscoveryOperationEffectState::Prepared)) => {
                    return step_prepared_operation(
                        store,
                        record,
                        execution,
                        effects,
                        effect_index,
                        operation,
                        updated_at,
                    );
                }
                Some((_, DiscoveryOperationEffectState::Staged))
                | Some((_, DiscoveryOperationEffectState::Installed)) => {}
                Some((_, DiscoveryOperationEffectState::RolledBack)) => {
                    return invalid(format!(
                        "applying discovery operation `{}` is already rolled back",
                        operation.operation_id()
                    ));
                }
            }
        }

        let mut creates = component
            .operations
            .iter()
            .filter(|operation| {
                matches!(
                    operation,
                    DiscoveryExecutionOperation::Create { .. }
                        | DiscoveryExecutionOperation::CreateContainer { .. }
                )
            })
            .collect::<Vec<_>>();
        creates.sort_by(compare_install_operations);
        for operation in creates {
            match operation_effect_state(effects, operation) {
                None => {
                    return prepare_operation(
                        store, record, execution, effects, operation, updated_at,
                    );
                }
                Some((effect_index, DiscoveryOperationEffectState::Prepared)) => {
                    return step_prepared_operation(
                        store,
                        record,
                        execution,
                        effects,
                        effect_index,
                        operation,
                        updated_at,
                    );
                }
                Some((_, DiscoveryOperationEffectState::Staged))
                | Some((_, DiscoveryOperationEffectState::Installed)) => {}
                Some((_, DiscoveryOperationEffectState::RolledBack)) => {
                    return invalid(format!(
                        "applying discovery operation `{}` is already rolled back",
                        operation.operation_id()
                    ));
                }
            }
        }

        let mut destinations = component
            .operations
            .iter()
            .filter(|operation| operation.destination().is_some())
            .collect::<Vec<_>>();
        destinations.sort_by(compare_install_operations);
        for operation in destinations {
            match operation_effect_state(effects, operation) {
                Some((effect_index, DiscoveryOperationEffectState::Staged)) => {
                    return step_staged_operation(
                        store,
                        record,
                        execution,
                        effects,
                        effect_index,
                        operation,
                        updated_at,
                    );
                }
                Some((_, DiscoveryOperationEffectState::Installed)) => {}
                Some((_, state)) => {
                    return invalid(format!(
                        "discovery operation `{}` has invalid install state `{:?}`",
                        operation.operation_id(),
                        state
                    ));
                }
                None => {
                    return invalid(format!(
                        "discovery operation `{}` was not staged",
                        operation.operation_id()
                    ));
                }
            }
        }

        let mut deletes = component
            .operations
            .iter()
            .filter(|operation| matches!(operation, DiscoveryExecutionOperation::Delete { .. }))
            .collect::<Vec<_>>();
        deletes.sort_by_key(|operation| operation.operation_id());
        for operation in deletes {
            match operation_effect_state(effects, operation) {
                Some((effect_index, DiscoveryOperationEffectState::Staged)) => {
                    return step_staged_operation(
                        store,
                        record,
                        execution,
                        effects,
                        effect_index,
                        operation,
                        updated_at,
                    );
                }
                Some((_, DiscoveryOperationEffectState::Installed)) => {}
                Some((_, state)) => {
                    return invalid(format!(
                        "discovery delete `{}` has invalid finalization state `{:?}`",
                        operation.operation_id(),
                        state
                    ));
                }
                None => {
                    return invalid(format!(
                        "discovery delete `{}` was not staged",
                        operation.operation_id()
                    ));
                }
            }
        }
    }
    validate_installed_projection(execution, effects)?;
    store
        .mark_discovery_transaction_projected(
            &record.transaction_id,
            DiscoveryTransactionStatus::Applying,
            updated_at,
        )
        .map_err(repository_error)?;
    Ok(DiscoveryExecutionStep::Projected)
}

fn step_rollback<S>(
    store: &mut S,
    record: &DiscoveryTransactionRecord,
    execution: &DiscoveryExecutionPlan,
    effects: &mut DiscoveryExecutionEffects,
    updated_at: &str,
) -> LocalityResult<DiscoveryExecutionStep>
where
    S: DiscoveryRepository,
{
    let mut installed_destinations = execution
        .components
        .iter()
        .flat_map(|component| component.operations.iter())
        .filter(|operation| {
            matches!(
                operation,
                DiscoveryExecutionOperation::Create { .. }
                    | DiscoveryExecutionOperation::CreateContainer { .. }
                    | DiscoveryExecutionOperation::Move { .. }
            ) && matches!(
                operation_effect_state(effects, operation),
                Some((_, DiscoveryOperationEffectState::Staged))
                    | Some((_, DiscoveryOperationEffectState::Installed))
            )
        })
        .collect::<Vec<_>>();
    installed_destinations.sort_by(|left, right| {
        let left = left.destination().expect("destination operation");
        let right = right.destination().expect("destination operation");
        right
            .components()
            .count()
            .cmp(&left.components().count())
            .then_with(|| left.cmp(right))
    });
    for operation in installed_destinations {
        if let Some(step) = rollback_destination_to_recovery(
            store, record, execution, effects, operation, updated_at,
        )? {
            return Ok(step);
        }
    }

    let mut source_operations = execution
        .components
        .iter()
        .flat_map(|component| component.operations.iter())
        .filter(|operation| operation.source().is_some())
        .filter(|operation| {
            !matches!(
                operation_effect_state(effects, operation),
                None | Some((_, DiscoveryOperationEffectState::RolledBack))
            )
        })
        .collect::<Vec<_>>();
    source_operations.sort_by(|left, right| {
        let left = left.source().expect("source operation");
        let right = right.source().expect("source operation");
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });
    for operation in source_operations {
        if let Some(step) =
            rollback_source_operation(store, record, execution, effects, operation, updated_at)?
        {
            return Ok(step);
        }
    }

    for operation in execution
        .components
        .iter()
        .flat_map(|component| component.operations.iter())
        .filter(|operation| {
            matches!(
                operation,
                DiscoveryExecutionOperation::Create { .. }
                    | DiscoveryExecutionOperation::CreateContainer { .. }
            )
        })
    {
        if matches!(
            operation_effect_state(effects, operation),
            None | Some((_, DiscoveryOperationEffectState::RolledBack))
        ) {
            continue;
        }
        if let Some(step) =
            rollback_create_operation(store, record, execution, effects, operation, updated_at)?
        {
            return Ok(step);
        }
    }

    let recovery_root = expected_recovery_root(record)?;
    match fs::symlink_metadata(&recovery_root) {
        Ok(_) => {
            validate_empty_recovery_scaffolding(&recovery_root)?;
            remove_dir_all_durable(&recovery_root).map_err(LocalityError::from)?;
            return Ok(DiscoveryExecutionStep::RecoveryPayloadsRemoved);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(LocalityError::from(error)),
    }
    if !effects.cleanup_complete {
        effects.cleanup_complete = true;
        record_effects(
            store,
            &record.transaction_id,
            record.status,
            effects,
            updated_at,
        )?;
        return Ok(DiscoveryExecutionStep::CleanupComplete);
    }
    store
        .mark_discovery_transaction_aborted(&record.transaction_id, record.status, updated_at)
        .map_err(repository_error)?;
    Ok(DiscoveryExecutionStep::Aborted)
}

fn rollback_destination_to_recovery<S>(
    store: &mut S,
    record: &DiscoveryTransactionRecord,
    execution: &DiscoveryExecutionPlan,
    effects: &mut DiscoveryExecutionEffects,
    operation: &DiscoveryExecutionOperation,
    updated_at: &str,
) -> LocalityResult<Option<DiscoveryExecutionStep>>
where
    S: DiscoveryRepository,
{
    let (destination, recovery) = match operation {
        DiscoveryExecutionOperation::Create {
            destination,
            payload,
            ..
        }
        | DiscoveryExecutionOperation::CreateContainer {
            destination,
            payload,
            ..
        } => (destination, payload),
        DiscoveryExecutionOperation::Move {
            destination, stage, ..
        } => (destination, stage),
        DiscoveryExecutionOperation::Delete { .. } => return Ok(None),
    };
    let (effect_index, state) = operation_effect_state(effects, operation).expect("effect exists");
    let destination = checked_join(&execution.mount_root, destination, "rollback destination")?;
    let recovery = checked_join(&execution.recovery_root, recovery, "rollback recovery path")?;
    match (
        fingerprint_if_exists(&destination)?,
        fingerprint_if_exists(&recovery)?,
    ) {
        (None, Some(fingerprint)) if fingerprint == *operation.expected_fingerprint() => {
            if state == DiscoveryOperationEffectState::Installed {
                effects.operations[effect_index].state = DiscoveryOperationEffectState::Staged;
                record_effects(
                    store,
                    &record.transaction_id,
                    record.status,
                    effects,
                    updated_at,
                )?;
                return Ok(Some(DiscoveryExecutionStep::OperationRecorded {
                    operation_id: operation.operation_id().to_string(),
                    state: DiscoveryOperationEffectState::Staged,
                }));
            }
            Ok(None)
        }
        (Some(fingerprint), None) if fingerprint == *operation.expected_fingerprint() => {
            ensure_recovery_parent(
                execution.mount_root.parent().ok_or_else(|| {
                    LocalityError::InvalidState(format!(
                        "mount root `{}` has no recovery parent",
                        execution.mount_root.display()
                    ))
                })?,
                &recovery,
            )?;
            rename_noreplace_durable(&destination, &recovery).map_err(LocalityError::from)?;
            Ok(Some(DiscoveryExecutionStep::FilesystemMutation {
                operation_id: operation.operation_id().to_string(),
                mutation: DiscoveryFilesystemMutation::RolledBack,
            }))
        }
        (Some(_), Some(_)) => invalid(format!(
            "rollback destination `{}` and recovery path `{}` both exist",
            destination.display(),
            recovery.display()
        )),
        (None, None) => match operation {
            DiscoveryExecutionOperation::Move { source, .. } => {
                let source = checked_join(&execution.mount_root, source, "rollback source")?;
                if fingerprint_path(&source)? == *operation.expected_fingerprint() {
                    Ok(None)
                } else {
                    invalid(format!(
                        "rollback destination `{}` and recovery path `{}` are both missing",
                        destination.display(),
                        recovery.display()
                    ))
                }
            }
            DiscoveryExecutionOperation::Create { .. }
            | DiscoveryExecutionOperation::CreateContainer { .. } => Ok(None),
            DiscoveryExecutionOperation::Delete { .. } => unreachable!(),
        },
        _ => invalid(format!(
            "rollback destination `{}` is not fingerprint-owned",
            destination.display()
        )),
    }
}

fn rollback_source_operation<S>(
    store: &mut S,
    record: &DiscoveryTransactionRecord,
    execution: &DiscoveryExecutionPlan,
    effects: &mut DiscoveryExecutionEffects,
    operation: &DiscoveryExecutionOperation,
    updated_at: &str,
) -> LocalityResult<Option<DiscoveryExecutionStep>>
where
    S: DiscoveryRepository,
{
    let (source, stage) = match operation {
        DiscoveryExecutionOperation::Move { source, stage, .. }
        | DiscoveryExecutionOperation::Delete { source, stage, .. } => (source, stage),
        DiscoveryExecutionOperation::Create { .. }
        | DiscoveryExecutionOperation::CreateContainer { .. } => return Ok(None),
    };
    let (effect_index, _) = operation_effect_state(effects, operation).expect("effect exists");
    let source = checked_join(&execution.mount_root, source, "rollback source")?;
    let stage = checked_join(&execution.recovery_root, stage, "rollback stage")?;
    match (
        fingerprint_if_exists(&source)?,
        fingerprint_if_exists(&stage)?,
    ) {
        (Some(fingerprint), None) if fingerprint == *operation.expected_fingerprint() => {
            effects.operations[effect_index].state = DiscoveryOperationEffectState::RolledBack;
            record_effects(
                store,
                &record.transaction_id,
                record.status,
                effects,
                updated_at,
            )?;
            Ok(Some(DiscoveryExecutionStep::OperationRecorded {
                operation_id: operation.operation_id().to_string(),
                state: DiscoveryOperationEffectState::RolledBack,
            }))
        }
        (None, Some(fingerprint)) if fingerprint == *operation.expected_fingerprint() => {
            require_existing_safe_parent(&execution.mount_root, &source)?;
            rename_noreplace_durable(&stage, &source).map_err(LocalityError::from)?;
            Ok(Some(DiscoveryExecutionStep::FilesystemMutation {
                operation_id: operation.operation_id().to_string(),
                mutation: DiscoveryFilesystemMutation::RolledBack,
            }))
        }
        (Some(_), Some(_)) => invalid(format!(
            "rollback source `{}` and stage `{}` both exist",
            source.display(),
            stage.display()
        )),
        (None, None) => invalid(format!(
            "rollback source `{}` and stage `{}` are both missing",
            source.display(),
            stage.display()
        )),
        _ => invalid(format!(
            "rollback source `{}` is not fingerprint-owned",
            source.display()
        )),
    }
}

fn rollback_create_operation<S>(
    store: &mut S,
    record: &DiscoveryTransactionRecord,
    execution: &DiscoveryExecutionPlan,
    effects: &mut DiscoveryExecutionEffects,
    operation: &DiscoveryExecutionOperation,
    updated_at: &str,
) -> LocalityResult<Option<DiscoveryExecutionStep>>
where
    S: DiscoveryRepository,
{
    let (destination, payload, temporary_payload) = match operation {
        DiscoveryExecutionOperation::Create {
            destination,
            payload,
            temporary_payload,
            ..
        }
        | DiscoveryExecutionOperation::CreateContainer {
            destination,
            payload,
            temporary_payload,
            ..
        } => (destination, payload, temporary_payload),
        _ => return Ok(None),
    };
    let (effect_index, _) = operation_effect_state(effects, operation).expect("effect exists");
    let destination = checked_join(&execution.mount_root, destination, "rollback destination")?;
    if fingerprint_if_exists(&destination)?.is_some() {
        return invalid(format!(
            "rollback create destination `{}` still exists",
            destination.display()
        ));
    }
    let payload = checked_join(&execution.recovery_root, payload, "rollback payload")?;
    if let Some(fingerprint) = fingerprint_if_exists(&payload)? {
        if fingerprint != *operation.expected_fingerprint() {
            return invalid(format!(
                "rollback payload `{}` is not fingerprint-owned",
                payload.display()
            ));
        }
        remove_path_durable(&payload).map_err(LocalityError::from)?;
        return Ok(Some(DiscoveryExecutionStep::FilesystemMutation {
            operation_id: operation.operation_id().to_string(),
            mutation: DiscoveryFilesystemMutation::RolledBack,
        }));
    }
    let temporary = checked_join(
        &execution.recovery_root,
        temporary_payload,
        "rollback temporary payload",
    )?;
    if let Some(fingerprint) = fingerprint_if_exists(&temporary)? {
        if fingerprint != *operation.expected_fingerprint() {
            return invalid(format!(
                "rollback temporary payload `{}` is not fingerprint-owned",
                temporary.display()
            ));
        }
        remove_path_durable(&temporary).map_err(LocalityError::from)?;
        return Ok(Some(DiscoveryExecutionStep::FilesystemMutation {
            operation_id: operation.operation_id().to_string(),
            mutation: DiscoveryFilesystemMutation::RolledBack,
        }));
    }
    effects.operations[effect_index].state = DiscoveryOperationEffectState::RolledBack;
    record_effects(
        store,
        &record.transaction_id,
        record.status,
        effects,
        updated_at,
    )?;
    Ok(Some(DiscoveryExecutionStep::OperationRecorded {
        operation_id: operation.operation_id().to_string(),
        state: DiscoveryOperationEffectState::RolledBack,
    }))
}

fn validate_empty_recovery_scaffolding(recovery_root: &Path) -> LocalityResult<()> {
    reject_symlink(recovery_root)?;
    for entry in fs::read_dir(recovery_root).map_err(LocalityError::from)? {
        let entry = entry.map_err(LocalityError::from)?;
        let name = entry.file_name();
        if name != "payloads" && name != "temporary" && name != "stages" {
            return invalid(format!(
                "unexpected recovery entry `{}` prevents cleanup",
                entry.path().display()
            ));
        }
        reject_symlink(&entry.path())?;
        if fs::read_dir(entry.path())
            .map_err(LocalityError::from)?
            .next()
            .is_some()
        {
            return invalid(format!(
                "nonempty recovery scaffold `{}` prevents cleanup",
                entry.path().display()
            ));
        }
    }
    Ok(())
}

fn operation_effect_state(
    effects: &DiscoveryExecutionEffects,
    operation: &DiscoveryExecutionOperation,
) -> Option<(usize, DiscoveryOperationEffectState)> {
    effects
        .operations
        .iter()
        .position(|effect| effect.operation_id == operation.operation_id())
        .map(|index| (index, effects.operations[index].state))
}

fn prepare_operation<S>(
    store: &mut S,
    record: &DiscoveryTransactionRecord,
    execution: &DiscoveryExecutionPlan,
    effects: &mut DiscoveryExecutionEffects,
    operation: &DiscoveryExecutionOperation,
    updated_at: &str,
) -> LocalityResult<DiscoveryExecutionStep>
where
    S: DiscoveryRepository + HydrationJobRepository,
{
    ensure_operation_can_prepare(execution, operation)?;
    effects.operations.push(DiscoveryOperationEffect {
        operation_id: operation.operation_id().to_string(),
        state: DiscoveryOperationEffectState::Prepared,
        fingerprint: operation.expected_fingerprint().clone(),
    });
    record_effects(
        store,
        &record.transaction_id,
        DiscoveryTransactionStatus::Applying,
        effects,
        updated_at,
    )?;
    Ok(DiscoveryExecutionStep::OperationPrepared {
        operation_id: operation.operation_id().to_string(),
    })
}

fn compare_install_operations(
    left: &&DiscoveryExecutionOperation,
    right: &&DiscoveryExecutionOperation,
) -> std::cmp::Ordering {
    match (left.destination(), right.destination()) {
        (Some(left_destination), Some(right_destination)) => left_destination
            .components()
            .count()
            .cmp(&right_destination.components().count())
            .then_with(|| {
                matches!(
                    right.expected_fingerprint().kind,
                    DiscoveryPathKind::Directory
                )
                .cmp(&matches!(
                    left.expected_fingerprint().kind,
                    DiscoveryPathKind::Directory
                ))
            })
            .then_with(|| left.operation_id().cmp(right.operation_id()))
            .then_with(|| left_destination.cmp(right_destination)),
        (Some(_), None) => std::cmp::Ordering::Greater,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (None, None) => left.operation_id().cmp(right.operation_id()),
    }
}

fn step_prepared_operation<S>(
    store: &mut S,
    record: &DiscoveryTransactionRecord,
    _execution: &DiscoveryExecutionPlan,
    effects: &mut DiscoveryExecutionEffects,
    effect_index: usize,
    operation: &DiscoveryExecutionOperation,
    updated_at: &str,
) -> LocalityResult<DiscoveryExecutionStep>
where
    S: DiscoveryRepository,
{
    match operation {
        DiscoveryExecutionOperation::Create {
            operation_id,
            payload,
            temporary_payload,
            expected_fingerprint,
            materialization,
            ..
        } => {
            let recovery_root = expected_recovery_root(record)?;
            let payload = checked_join(&recovery_root, payload, "payload")?;
            let temporary = checked_join(&recovery_root, temporary_payload, "temporary payload")?;
            match fingerprint_if_exists(&payload)? {
                Some(fingerprint) if fingerprint == *expected_fingerprint => {
                    effects.operations[effect_index].state = DiscoveryOperationEffectState::Staged;
                    record_effects(
                        store,
                        &record.transaction_id,
                        DiscoveryTransactionStatus::Applying,
                        effects,
                        updated_at,
                    )?;
                    Ok(DiscoveryExecutionStep::OperationRecorded {
                        operation_id: operation_id.clone(),
                        state: DiscoveryOperationEffectState::Staged,
                    })
                }
                Some(_) => invalid(format!(
                    "discovery payload `{}` no longer matches its fingerprint",
                    payload.display()
                )),
                None => {
                    publish_create_payload(
                        record.reservation.mount.root.parent().ok_or_else(|| {
                            LocalityError::InvalidState(format!(
                                "mount root `{}` has no recovery parent",
                                record.reservation.mount.root.display()
                            ))
                        })?,
                        &temporary,
                        &payload,
                        materialization,
                        expected_fingerprint,
                    )?;
                    Ok(DiscoveryExecutionStep::FilesystemMutation {
                        operation_id: operation_id.clone(),
                        mutation: DiscoveryFilesystemMutation::PayloadPublished,
                    })
                }
            }
        }
        DiscoveryExecutionOperation::CreateContainer {
            operation_id,
            payload,
            temporary_payload,
            expected_fingerprint,
            ..
        } => {
            let recovery_root = expected_recovery_root(record)?;
            let payload = checked_join(&recovery_root, payload, "container payload")?;
            let temporary = checked_join(
                &recovery_root,
                temporary_payload,
                "temporary container payload",
            )?;
            match fingerprint_if_exists(&payload)? {
                Some(fingerprint) if fingerprint == *expected_fingerprint => {
                    effects.operations[effect_index].state = DiscoveryOperationEffectState::Staged;
                    record_effects(
                        store,
                        &record.transaction_id,
                        DiscoveryTransactionStatus::Applying,
                        effects,
                        updated_at,
                    )?;
                    Ok(DiscoveryExecutionStep::OperationRecorded {
                        operation_id: operation_id.clone(),
                        state: DiscoveryOperationEffectState::Staged,
                    })
                }
                Some(_) => invalid(format!(
                    "discovery container payload `{}` no longer matches its fingerprint",
                    payload.display()
                )),
                None => {
                    publish_create_container_payload(
                        record.reservation.mount.root.parent().ok_or_else(|| {
                            LocalityError::InvalidState(format!(
                                "mount root `{}` has no recovery parent",
                                record.reservation.mount.root.display()
                            ))
                        })?,
                        &temporary,
                        &payload,
                        expected_fingerprint,
                    )?;
                    Ok(DiscoveryExecutionStep::FilesystemMutation {
                        operation_id: operation_id.clone(),
                        mutation: DiscoveryFilesystemMutation::PayloadPublished,
                    })
                }
            }
        }
        DiscoveryExecutionOperation::Move {
            operation_id,
            source,
            stage,
            expected_fingerprint,
            ..
        }
        | DiscoveryExecutionOperation::Delete {
            operation_id,
            source,
            stage,
            expected_fingerprint,
            ..
        } => {
            let mount_root = &record.reservation.mount.root;
            let recovery_root = expected_recovery_root(record)?;
            let source = checked_join(mount_root, source, "source")?;
            let stage = checked_join(&recovery_root, stage, "stage")?;
            match (
                fingerprint_if_exists(&source)?,
                fingerprint_if_exists(&stage)?,
            ) {
                (None, Some(fingerprint)) if fingerprint == *expected_fingerprint => {
                    effects.operations[effect_index].state = DiscoveryOperationEffectState::Staged;
                    record_effects(
                        store,
                        &record.transaction_id,
                        DiscoveryTransactionStatus::Applying,
                        effects,
                        updated_at,
                    )?;
                    Ok(DiscoveryExecutionStep::OperationRecorded {
                        operation_id: operation_id.clone(),
                        state: DiscoveryOperationEffectState::Staged,
                    })
                }
                (Some(fingerprint), None) if fingerprint == *expected_fingerprint => {
                    ensure_recovery_parent(
                        mount_root.parent().ok_or_else(|| {
                            LocalityError::InvalidState(format!(
                                "mount root `{}` has no recovery parent",
                                mount_root.display()
                            ))
                        })?,
                        &stage,
                    )?;
                    if fingerprint_path(&source)? != *expected_fingerprint
                        || fingerprint_if_exists(&stage)?.is_some()
                    {
                        return invalid(format!(
                            "discovery source `{}` changed before staging",
                            source.display()
                        ));
                    }
                    rename_noreplace_durable(&source, &stage).map_err(LocalityError::from)?;
                    if fingerprint_path(&stage)? != *expected_fingerprint {
                        return invalid(format!(
                            "staged discovery source `{}` has the wrong fingerprint",
                            stage.display()
                        ));
                    }
                    Ok(DiscoveryExecutionStep::FilesystemMutation {
                        operation_id: operation_id.clone(),
                        mutation: DiscoveryFilesystemMutation::SourceStaged,
                    })
                }
                (Some(_), Some(_)) => invalid(format!(
                    "discovery source `{}` and stage `{}` both exist",
                    source.display(),
                    stage.display()
                )),
                (None, None) => invalid(format!(
                    "discovery source `{}` and stage `{}` are both missing",
                    source.display(),
                    stage.display()
                )),
                _ => invalid(format!(
                    "discovery source `{}` or stage `{}` fingerprint mismatch",
                    source.display(),
                    stage.display()
                )),
            }
        }
    }
}

fn step_staged_operation<S>(
    store: &mut S,
    record: &DiscoveryTransactionRecord,
    execution: &DiscoveryExecutionPlan,
    effects: &mut DiscoveryExecutionEffects,
    effect_index: usize,
    operation: &DiscoveryExecutionOperation,
    updated_at: &str,
) -> LocalityResult<DiscoveryExecutionStep>
where
    S: DiscoveryRepository,
{
    match operation {
        DiscoveryExecutionOperation::Create {
            operation_id,
            destination,
            payload,
            expected_fingerprint,
            ..
        }
        | DiscoveryExecutionOperation::CreateContainer {
            operation_id,
            destination,
            payload,
            expected_fingerprint,
            ..
        } => {
            let mount_root = &record.reservation.mount.root;
            let recovery_root = expected_recovery_root(record)?;
            let destination = checked_join(mount_root, destination, "destination")?;
            let payload = checked_join(&recovery_root, payload, "payload")?;
            let payload_fingerprint = fingerprint_if_exists(&payload)?;
            let destination_fingerprint = fingerprint_if_exists(&destination)?;
            match (payload_fingerprint, destination_fingerprint) {
                (None, Some(fingerprint)) if fingerprint == *expected_fingerprint => {
                    effects.operations[effect_index].state =
                        DiscoveryOperationEffectState::Installed;
                    record_effects(
                        store,
                        &record.transaction_id,
                        DiscoveryTransactionStatus::Applying,
                        effects,
                        updated_at,
                    )?;
                    Ok(DiscoveryExecutionStep::OperationRecorded {
                        operation_id: operation_id.clone(),
                        state: DiscoveryOperationEffectState::Installed,
                    })
                }
                (Some(fingerprint), None) if fingerprint == *expected_fingerprint => {
                    require_existing_safe_parent(mount_root, &destination)?;
                    if fingerprint_if_exists(&destination)?.is_some() {
                        return invalid(format!(
                            "discovery destination `{}` appeared before install",
                            destination.display()
                        ));
                    }
                    rename_noreplace_durable(&payload, &destination)
                        .map_err(LocalityError::from)?;
                    let installed = fingerprint_path(&destination)?;
                    if installed != *expected_fingerprint {
                        return invalid(format!(
                            "installed discovery destination `{}` has the wrong fingerprint",
                            destination.display()
                        ));
                    }
                    Ok(DiscoveryExecutionStep::FilesystemMutation {
                        operation_id: operation_id.clone(),
                        mutation: DiscoveryFilesystemMutation::DestinationInstalled,
                    })
                }
                (Some(_), Some(_)) => invalid(format!(
                    "discovery payload `{}` and destination `{}` both exist",
                    payload.display(),
                    destination.display()
                )),
                (None, None) => invalid(format!(
                    "discovery payload `{}` and destination `{}` are both missing",
                    payload.display(),
                    destination.display()
                )),
                _ => invalid(format!(
                    "discovery create `{operation_id}` fingerprint mismatch"
                )),
            }
        }
        DiscoveryExecutionOperation::Move {
            operation_id,
            stage,
            destination,
            expected_fingerprint,
            ..
        } => {
            let mount_root = &record.reservation.mount.root;
            let recovery_root = expected_recovery_root(record)?;
            let stage = checked_join(&recovery_root, stage, "stage")?;
            let destination = checked_join(mount_root, destination, "destination")?;
            match (
                fingerprint_if_exists(&stage)?,
                fingerprint_if_exists(&destination)?,
            ) {
                (None, Some(fingerprint)) if fingerprint == *expected_fingerprint => {
                    effects.operations[effect_index].state =
                        DiscoveryOperationEffectState::Installed;
                    record_effects(
                        store,
                        &record.transaction_id,
                        DiscoveryTransactionStatus::Applying,
                        effects,
                        updated_at,
                    )?;
                    Ok(DiscoveryExecutionStep::OperationRecorded {
                        operation_id: operation_id.clone(),
                        state: DiscoveryOperationEffectState::Installed,
                    })
                }
                (Some(fingerprint), None) if fingerprint == *expected_fingerprint => {
                    require_existing_safe_parent(mount_root, &destination)?;
                    if fingerprint_path(&stage)? != *expected_fingerprint
                        || fingerprint_if_exists(&destination)?.is_some()
                    {
                        return invalid(format!(
                            "discovery move `{operation_id}` changed before install"
                        ));
                    }
                    rename_noreplace_durable(&stage, &destination).map_err(LocalityError::from)?;
                    if fingerprint_path(&destination)? != *expected_fingerprint {
                        return invalid(format!(
                            "installed move destination `{}` has the wrong fingerprint",
                            destination.display()
                        ));
                    }
                    Ok(DiscoveryExecutionStep::FilesystemMutation {
                        operation_id: operation_id.clone(),
                        mutation: DiscoveryFilesystemMutation::DestinationInstalled,
                    })
                }
                (Some(_), Some(_)) => invalid(format!(
                    "discovery stage `{}` and destination `{}` both exist",
                    stage.display(),
                    destination.display()
                )),
                (None, None) => invalid(format!(
                    "discovery stage `{}` and destination `{}` are both missing",
                    stage.display(),
                    destination.display()
                )),
                _ => invalid(format!(
                    "discovery move `{operation_id}` fingerprint mismatch"
                )),
            }
        }
        DiscoveryExecutionOperation::Delete {
            operation_id,
            source,
            stage,
            expected_fingerprint,
            ..
        } => {
            let mount_root = &record.reservation.mount.root;
            let recovery_root = expected_recovery_root(record)?;
            let source_replaced_by_component = execution
                .components
                .iter()
                .find(|component| {
                    component
                        .operations
                        .iter()
                        .any(|candidate| candidate.operation_id() == operation_id)
                })
                .is_some_and(|component| {
                    component
                        .operations
                        .iter()
                        .filter_map(DiscoveryExecutionOperation::destination)
                        .any(|destination| destination == source)
                });
            let source = checked_join(mount_root, source, "source")?;
            let stage = checked_join(&recovery_root, stage, "stage")?;
            if (fingerprint_if_exists(&source)?.is_some() && !source_replaced_by_component)
                || fingerprint_path(&stage)? != *expected_fingerprint
            {
                return invalid(format!(
                    "staged delete `{operation_id}` no longer owns its paths"
                ));
            }
            effects.operations[effect_index].state = DiscoveryOperationEffectState::Installed;
            record_effects(
                store,
                &record.transaction_id,
                DiscoveryTransactionStatus::Applying,
                effects,
                updated_at,
            )?;
            Ok(DiscoveryExecutionStep::OperationRecorded {
                operation_id: operation_id.clone(),
                state: DiscoveryOperationEffectState::Installed,
            })
        }
    }
}

fn step_committed<S>(
    store: &mut S,
    record: &DiscoveryTransactionRecord,
    execution: &DiscoveryExecutionPlan,
    effects: &mut DiscoveryExecutionEffects,
    updated_at: &str,
) -> LocalityResult<DiscoveryExecutionStep>
where
    S: DiscoveryRepository + HydrationJobRepository,
{
    if !effects.projection_validated {
        validate_installed_projection(execution, effects)?;
        validate_committed_recovery_tree(execution, effects, false)?;
        effects.projection_validated = true;
        record_effects(
            store,
            &record.transaction_id,
            DiscoveryTransactionStatus::Committed,
            effects,
            updated_at,
        )?;
        return Ok(DiscoveryExecutionStep::ProjectionValidated);
    }
    for request in &execution.hydration_jobs {
        if effects
            .hydration_jobs
            .iter()
            .any(|effect| effect.remote_id == request.remote_id && effect.upserted)
        {
            continue;
        }
        store
            .upsert_hydration_job(HydrationJobRecord::from(request.clone()))
            .map_err(repository_error)?;
        effects.hydration_jobs.push(DiscoveryHydrationEffect {
            remote_id: request.remote_id.clone(),
            upserted: true,
        });
        record_effects(
            store,
            &record.transaction_id,
            DiscoveryTransactionStatus::Committed,
            effects,
            updated_at,
        )?;
        return Ok(DiscoveryExecutionStep::HydrationJobUpserted {
            remote_id: request.remote_id.clone(),
        });
    }
    let recovery_root = expected_recovery_root(record)?;
    if !effects.cleanup_complete {
        match fs::symlink_metadata(&recovery_root) {
            Ok(_) => {
                validate_committed_recovery_tree(execution, effects, true)?;
                remove_dir_all_durable(&recovery_root).map_err(LocalityError::from)?;
                return Ok(DiscoveryExecutionStep::RecoveryPayloadsRemoved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                validate_committed_recovery_tree(execution, effects, true)?;
            }
            Err(error) => return Err(LocalityError::from(error)),
        }
        effects.cleanup_complete = true;
        record_effects(
            store,
            &record.transaction_id,
            DiscoveryTransactionStatus::Committed,
            effects,
            updated_at,
        )?;
        return Ok(DiscoveryExecutionStep::CleanupComplete);
    }
    if !effects.completion_recorded {
        effects.completion_recorded = true;
        record_effects(
            store,
            &record.transaction_id,
            DiscoveryTransactionStatus::Committed,
            effects,
            updated_at,
        )?;
        return Ok(DiscoveryExecutionStep::CompletionRecorded);
    }
    store
        .mark_discovery_transaction_finalized(&record.transaction_id, updated_at)
        .map_err(repository_error)?;
    Ok(DiscoveryExecutionStep::Finalized)
}

fn validate_committed_recovery_tree(
    execution: &DiscoveryExecutionPlan,
    effects: &DiscoveryExecutionEffects,
    allow_missing_root: bool,
) -> LocalityResult<()> {
    let mut expected = BTreeMap::new();
    for component in &execution.components {
        for operation in &component.operations {
            let (_, state) = operation_effect_state(effects, operation).ok_or_else(|| {
                LocalityError::InvalidState(format!(
                    "committed discovery operation `{}` has no effect",
                    operation.operation_id()
                ))
            })?;
            if state != DiscoveryOperationEffectState::Installed {
                return invalid(format!(
                    "committed discovery operation `{}` is not installed",
                    operation.operation_id()
                ));
            }
            if let DiscoveryExecutionOperation::Delete { stage, .. } = operation {
                expected.insert(stage.clone(), operation.expected_fingerprint().clone());
            }
        }
    }

    let root_metadata = match fs::symlink_metadata(&execution.recovery_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if allow_missing_root || execution.components.iter().all(|c| c.operations.is_empty()) {
                // Cleanup may have completed immediately before its effect was recorded.
                return Ok(());
            }
            return invalid(format!(
                "discovery recovery root `{}` is missing before cleanup",
                execution.recovery_root.display()
            ));
        }
        Err(error) => return Err(LocalityError::from(error)),
    };
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return invalid(format!(
            "committed discovery recovery root `{}` is not an owned directory",
            execution.recovery_root.display()
        ));
    }

    let mut found = BTreeSet::new();
    validate_committed_recovery_directory(
        &execution.recovery_root,
        &execution.recovery_root,
        &expected,
        &mut found,
    )?;
    for path in expected.keys() {
        if !found.contains(path) {
            return invalid(format!(
                "committed discovery delete quarantine `{}` is missing",
                execution.recovery_root.join(path).display()
            ));
        }
    }
    Ok(())
}

fn validate_committed_recovery_directory(
    root: &Path,
    directory: &Path,
    expected: &BTreeMap<PathBuf, DiscoveryPathFingerprint>,
    found: &mut BTreeSet<PathBuf>,
) -> LocalityResult<()> {
    let mut entries = fs::read_dir(directory)
        .map_err(LocalityError::from)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(LocalityError::from)?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| LocalityError::InvalidState("recovery path escaped root".to_string()))?
            .to_path_buf();
        if let Some(fingerprint) = expected.get(&relative) {
            if fingerprint_path(&path)? != *fingerprint {
                return invalid(format!(
                    "committed discovery quarantine `{}` has changed",
                    path.display()
                ));
            }
            found.insert(relative);
            continue;
        }
        let is_scaffold = matches!(relative.to_str(), Some("payloads" | "temporary" | "stages"));
        let is_expected_parent = expected
            .keys()
            .any(|expected| expected.starts_with(&relative));
        if !is_scaffold && !is_expected_parent {
            return invalid(format!(
                "unexpected recovery entry `{}` prevents committed cleanup",
                path.display()
            ));
        }
        reject_symlink(&path)?;
        validate_committed_recovery_directory(root, &path, expected, found)?;
    }
    Ok(())
}

fn record_effects<S>(
    store: &mut S,
    transaction_id: &DiscoveryTransactionId,
    status: DiscoveryTransactionStatus,
    effects: &DiscoveryExecutionEffects,
    updated_at: &str,
) -> LocalityResult<DiscoveryTransactionRecord>
where
    S: DiscoveryRepository,
{
    store
        .record_discovery_transaction_effects(
            transaction_id,
            status,
            serde_json::to_value(effects).map_err(invalid_json)?,
            updated_at,
        )
        .map_err(repository_error)
}

fn decode_and_validate_execution(
    record: &DiscoveryTransactionRecord,
) -> LocalityResult<(DiscoveryExecutionPlan, DiscoveryExecutionEffects)> {
    preflight_execution_record(record)?;
    let execution = serde_json::from_value::<DiscoveryExecutionPlan>(record.plan.clone())
        .map_err(invalid_json)?;
    let effects = serde_json::from_value::<DiscoveryExecutionEffects>(record.effects.clone())
        .map_err(invalid_json)?;
    if execution.transaction_id != record.transaction_id
        || execution.transaction_id != record.commit.transaction_id
    {
        return invalid(format!(
            "discovery transaction `{}` execution identifier does not match its record",
            record.transaction_id.0
        ));
    }
    if execution.mount_id != record.mount_id
        || execution.mount_id != record.reservation.mount.mount_id
        || execution.mount_id != record.commit.commit.mount_id
    {
        return invalid(format!(
            "discovery transaction `{}` execution mount does not match its record",
            record.transaction_id.0
        ));
    }
    validate_mount_root(&record.reservation.mount.root)?;
    if execution.mount_root != record.reservation.mount.root {
        return invalid(format!(
            "discovery transaction `{}` execution mount root was tampered",
            record.transaction_id.0
        ));
    }
    let expected_recovery = expected_recovery_root(record)?;
    if execution.recovery_root != expected_recovery {
        return invalid(format!(
            "discovery transaction `{}` execution recovery root was tampered",
            record.transaction_id.0
        ));
    }
    let flattened = execution
        .components
        .iter()
        .flat_map(|component| component.component.actions.iter().cloned())
        .collect::<Vec<_>>();
    validate_projection_commit_coverage(
        &flattened,
        &record.commit.commit,
        &record.reservation.entities,
    )?;
    let normalized = build_discovery_projection_components(&flattened);
    let stored_components = execution
        .components
        .iter()
        .map(|component| component.component.clone())
        .collect::<Vec<_>>();
    if normalized != stored_components {
        return invalid(format!(
            "discovery transaction `{}` execution components are not normalized",
            record.transaction_id.0
        ));
    }
    let mut known_operations = BTreeMap::new();
    for (component_index, component) in execution.components.iter().enumerate() {
        validate_component_operations(component, component_index)?;
        for operation in &component.operations {
            if known_operations
                .insert(operation.operation_id(), operation)
                .is_some()
            {
                return invalid(format!(
                    "discovery transaction `{}` has duplicate operation `{}`",
                    record.transaction_id.0,
                    operation.operation_id()
                ));
            }
            validate_operation(operation)?;
        }
    }
    validate_execution_path_ancestry(&execution)?;
    if record.status == DiscoveryTransactionStatus::Reserved {
        validate_reserved_operation_fingerprints(&execution)?;
    }
    validate_hydration_jobs(&execution, &effects)?;
    let mut effect_ids = BTreeMap::new();
    for effect in &effects.operations {
        let operation = known_operations
            .get(effect.operation_id.as_str())
            .ok_or_else(|| {
                LocalityError::InvalidState(format!(
                    "discovery effect `{}` has no matching operation",
                    effect.operation_id
                ))
            })?;
        if effect_ids
            .insert(effect.operation_id.as_str(), ())
            .is_some()
        {
            return invalid(format!(
                "discovery effects contain duplicate operation `{}`",
                effect.operation_id
            ));
        }
        if effect.fingerprint != *operation.expected_fingerprint() {
            return invalid(format!(
                "discovery effect `{}` fingerprint does not match its plan",
                effect.operation_id
            ));
        }
    }
    Ok((execution, effects))
}

fn preflight_execution_record(record: &DiscoveryTransactionRecord) -> LocalityResult<()> {
    if record.projection != ProjectionMode::PlainFiles {
        return invalid(format!(
            "discovery transaction `{}` is not a plain-files projection",
            record.transaction_id.0
        ));
    }
    validate_raw_execution_version(&record.plan, "plan")?;
    validate_raw_execution_version(&record.effects, "effects")
}

fn validate_raw_execution_version(value: &serde_json::Value, label: &str) -> LocalityResult<()> {
    let object = value.as_object().ok_or_else(|| {
        LocalityError::InvalidState(format!("discovery execution {label} must be a JSON object"))
    })?;
    let state_version = object
        .get("state_version")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| {
            LocalityError::InvalidState(format!(
                "discovery execution {label} is missing integer state_version"
            ))
        })?;
    let min_reader_version = object
        .get("min_reader_version")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| {
            LocalityError::InvalidState(format!(
                "discovery execution {label} is missing integer min_reader_version"
            ))
        })?;
    validate_execution_version(state_version, min_reader_version, label)
}

fn validate_execution_version(
    state_version: i64,
    min_reader_version: i64,
    label: &str,
) -> LocalityResult<()> {
    if state_version <= 0 || min_reader_version <= 0 || min_reader_version > state_version {
        return invalid(format!(
            "discovery execution {label} has invalid version metadata"
        ));
    }
    if state_version > DISCOVERY_EXECUTION_STATE_VERSION
        || min_reader_version > DISCOVERY_EXECUTION_STATE_VERSION
    {
        return Err(LocalityError::UpdateRequired {
            component: format!("daemon:discovery_execution_{label}"),
            found: state_version,
            supported: DISCOVERY_EXECUTION_STATE_VERSION,
        });
    }
    if state_version != DISCOVERY_EXECUTION_STATE_VERSION {
        return invalid(format!(
            "discovery execution {label} version {state_version} requires migration"
        ));
    }
    Ok(())
}

fn validate_projection_commit_coverage(
    actions: &[DiscoveryProjectionAction],
    commit: &DiscoveryCommit,
    existing_entities: &[EntityRecord],
) -> LocalityResult<()> {
    let existing_by_id = existing_entities
        .iter()
        .map(|entity| (&entity.remote_id, entity))
        .collect::<BTreeMap<_, _>>();
    let mut expected = Vec::new();
    for entity in &commit.entity_upserts {
        match existing_by_id.get(&entity.remote_id) {
            None => expected.push(ProjectionStructuralChange::Create {
                remote_id: entity.remote_id.clone(),
                kind: entity.kind.clone(),
                path: entity.path.clone(),
            }),
            Some(existing) if existing.path != entity.path => {
                expected.push(ProjectionStructuralChange::Move {
                    remote_id: entity.remote_id.clone(),
                    kind: entity.kind.clone(),
                    from: existing.path.clone(),
                    to: entity.path.clone(),
                });
            }
            Some(_) => {}
        }
    }
    for remote_id in &commit.entity_deletes {
        if let Some(existing) = existing_by_id.get(remote_id) {
            expected.push(ProjectionStructuralChange::Delete {
                remote_id: remote_id.clone(),
                kind: existing.kind.clone(),
                path: existing.path.clone(),
            });
        }
    }
    let every_commit_change_is_covered = expected.iter().all(|change| {
        actions
            .iter()
            .any(|action| projection_action_covers_change(action, change))
    });
    let every_action_has_an_exact_commit_change = actions.iter().all(|action| {
        let action_change = projection_structural_change(action);
        expected.iter().any(|change| change == &action_change)
    });
    if !every_commit_change_is_covered || !every_action_has_an_exact_commit_change {
        return invalid("discovery projection actions do not cover structural commit changes");
    }
    Ok(())
}

fn validate_operation(operation: &DiscoveryExecutionOperation) -> LocalityResult<()> {
    match operation {
        DiscoveryExecutionOperation::Create {
            projected_path,
            destination,
            payload,
            temporary_payload,
            kind,
            remote_id,
            materialization,
            expected_fingerprint,
            ..
        } => {
            validate_relative_path(projected_path)?;
            validate_relative_path(destination)?;
            validate_relative_path(payload)?;
            validate_relative_path(temporary_payload)?;
            if materialization.kind() != *kind || materialization.remote_id() != remote_id {
                return invalid("discovery create materialization kind was tampered");
            }
            if *destination != create_destination_path(kind, projected_path)
                || &materialization_fingerprint(kind, projected_path, materialization)?
                    != expected_fingerprint
            {
                return invalid("discovery create materialization fingerprint was tampered");
            }
        }
        DiscoveryExecutionOperation::CreateContainer {
            projected_from,
            projected_to,
            destination,
            payload,
            temporary_payload,
            expected_fingerprint,
            ..
        } => {
            for path in [
                projected_from,
                projected_to,
                destination,
                payload,
                temporary_payload,
            ] {
                validate_relative_path(path)?;
            }
            if !move_requires_destination_container(&EntityKind::Page, projected_from, projected_to)
                || *destination != page_container_path(projected_to)
                || *expected_fingerprint != fingerprint_virtual_directory(&[])
            {
                return invalid("discovery move container operation was tampered");
            }
        }
        DiscoveryExecutionOperation::Move {
            projected_from,
            projected_to,
            source,
            stage,
            destination,
            ..
        } => {
            for path in [projected_from, projected_to, source, stage, destination] {
                validate_relative_path(path)?;
            }
        }
        DiscoveryExecutionOperation::Delete {
            projected_path,
            source,
            stage,
            ..
        } => {
            for path in [projected_path, source, stage] {
                validate_relative_path(path)?;
            }
        }
    }
    Ok(())
}

fn validate_component_operations(
    component: &DiscoveryExecutionComponent,
    component_index: usize,
) -> LocalityResult<()> {
    for operation in &component.operations {
        if operation.action_index() >= component.component.actions.len() {
            return invalid(format!(
                "discovery operation `{}` refers to missing action {}",
                operation.operation_id(),
                operation.action_index()
            ));
        }
    }

    for (action_index, action) in component.component.actions.iter().enumerate() {
        let operations = component
            .operations
            .iter()
            .filter(|operation| operation.action_index() == action_index)
            .collect::<Vec<_>>();
        let base_id = format!("component-{component_index:08}-action-{action_index:08}");
        match action {
            DiscoveryProjectionAction::Create { entry } => {
                let [operation] = operations.as_slice() else {
                    return invalid(format!(
                        "discovery create `{}` does not have exactly one operation",
                        entry.remote_id.0
                    ));
                };
                match operation {
                    DiscoveryExecutionOperation::Create {
                        operation_id,
                        remote_id,
                        kind,
                        projected_path,
                        destination,
                        payload,
                        temporary_payload,
                        materialization,
                        ..
                    } if operation_id == &base_id
                        && remote_id == &entry.remote_id
                        && kind == &entry.kind
                        && projected_path == &entry.path
                        && destination == &create_destination_path(&entry.kind, &entry.path)
                        && payload == &PathBuf::from("payloads").join(&base_id)
                        && temporary_payload == &PathBuf::from("temporary").join(&base_id)
                        && materialization.remote_id() == &entry.remote_id => {}
                    _ => {
                        return invalid(format!(
                            "discovery create `{}` operation does not match its action",
                            entry.remote_id.0
                        ));
                    }
                }
            }
            DiscoveryProjectionAction::Move {
                remote_id,
                kind,
                from,
                to,
            } => {
                let (required, optional) = allowed_move_unit_paths(kind, from, to)?;
                validate_move_operations(
                    &operations,
                    &base_id,
                    remote_id,
                    kind,
                    from,
                    to,
                    &required,
                    optional.as_ref(),
                )?;
            }
            DiscoveryProjectionAction::Delete {
                remote_id,
                kind,
                path,
            } => {
                let (required, optional) = allowed_delete_unit_paths(kind, path)?;
                validate_delete_operations(
                    &operations,
                    &base_id,
                    remote_id,
                    kind,
                    path,
                    &required,
                    optional.as_ref(),
                )?;
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_move_operations(
    operations: &[&DiscoveryExecutionOperation],
    base_id: &str,
    remote_id: &RemoteId,
    kind: &EntityKind,
    from: &Path,
    to: &Path,
    required: &[(PathBuf, PathBuf)],
    optional: Option<&(PathBuf, PathBuf)>,
) -> LocalityResult<()> {
    let container_required = move_requires_destination_container(kind, from, to);
    let minimum = required.len() + usize::from(container_required);
    let maximum = required.len() + usize::from(optional.is_some());
    if operations.len() < minimum || operations.len() > maximum {
        return invalid(format!(
            "discovery move `{}` has the wrong operation count",
            remote_id.0
        ));
    }
    for (unit_index, (operation, (source, destination))) in
        operations.iter().zip(required).enumerate()
    {
        match operation {
            DiscoveryExecutionOperation::Move {
                operation_id,
                remote_id: operation_remote_id,
                kind: operation_kind,
                projected_from,
                projected_to,
                source: operation_source,
                stage,
                destination: operation_destination,
                ..
            } if operation_id == &unit_operation_id(base_id, unit_index)
                && operation_remote_id == remote_id
                && operation_kind == kind
                && projected_from == from
                && projected_to == to
                && operation_source == source
                && operation_destination == destination
                && stage
                    == &PathBuf::from("stages").join(unit_operation_id(base_id, unit_index)) => {}
            _ => {
                return invalid(format!(
                    "discovery move `{}` operation {unit_index} does not match its action",
                    remote_id.0
                ));
            }
        }
    }
    let Some(operation) = operations.get(required.len()) else {
        return Ok(());
    };
    let Some((optional_source, optional_destination)) = optional else {
        return invalid(format!(
            "discovery move `{}` operation {} does not match its action",
            remote_id.0,
            required.len()
        ));
    };
    let optional_matches = match operation {
        DiscoveryExecutionOperation::Move {
            operation_id,
            remote_id: operation_remote_id,
            kind: operation_kind,
            projected_from,
            projected_to,
            source,
            stage,
            destination,
            ..
        } => {
            operation_id == &unit_operation_id(base_id, required.len())
                && operation_remote_id == remote_id
                && operation_kind == kind
                && projected_from == from
                && projected_to == to
                && source == optional_source
                && destination == optional_destination
                && stage
                    == &PathBuf::from("stages").join(unit_operation_id(base_id, required.len()))
        }
        DiscoveryExecutionOperation::CreateContainer {
            operation_id,
            remote_id: operation_remote_id,
            projected_from,
            projected_to,
            destination,
            payload,
            temporary_payload,
            ..
        } => {
            container_required
                && operation_id == &container_operation_id(base_id)
                && operation_remote_id == remote_id
                && projected_from == from
                && projected_to == to
                && destination == optional_destination
                && payload == &PathBuf::from("payloads").join(container_operation_id(base_id))
                && temporary_payload
                    == &PathBuf::from("temporary").join(container_operation_id(base_id))
        }
        _ => false,
    };
    if !optional_matches {
        return invalid(format!(
            "discovery move `{}` operation {} does not match its action",
            remote_id.0,
            required.len()
        ));
    }
    Ok(())
}

fn validate_delete_operations(
    operations: &[&DiscoveryExecutionOperation],
    base_id: &str,
    remote_id: &RemoteId,
    kind: &EntityKind,
    path: &Path,
    required: &[PathBuf],
    optional: Option<&PathBuf>,
) -> LocalityResult<()> {
    if operations.len() < required.len()
        || operations.len() > required.len() + usize::from(optional.is_some())
    {
        return invalid(format!(
            "discovery delete `{}` has the wrong operation count",
            remote_id.0
        ));
    }
    let expected = required.iter().chain(optional).take(operations.len());
    for (unit_index, (operation, source)) in operations.iter().zip(expected).enumerate() {
        match operation {
            DiscoveryExecutionOperation::Delete {
                operation_id,
                remote_id: operation_remote_id,
                kind: operation_kind,
                projected_path,
                source: operation_source,
                stage,
                ..
            } if operation_id == &unit_operation_id(base_id, unit_index)
                && operation_remote_id == remote_id
                && operation_kind == kind
                && projected_path == path
                && operation_source == source
                && stage
                    == &PathBuf::from("stages").join(unit_operation_id(base_id, unit_index)) => {}
            _ => {
                return invalid(format!(
                    "discovery delete `{}` operation {unit_index} does not match its action",
                    remote_id.0
                ));
            }
        }
    }
    Ok(())
}

fn validate_reserved_operation_fingerprints(
    execution: &DiscoveryExecutionPlan,
) -> LocalityResult<()> {
    for component in &execution.components {
        validate_reserved_operation_units(&execution.mount_root, component)?;
        validate_prepared_destinations(&execution.mount_root, &component.operations)?;
        let mut expected = component.operations.clone();
        apply_nested_source_fingerprints(&execution.mount_root, &mut expected)?;
        for (operation, expected_operation) in component.operations.iter().zip(expected) {
            if operation.expected_fingerprint() != expected_operation.expected_fingerprint() {
                return invalid(format!(
                    "discovery operation `{}` prepared fingerprint was tampered",
                    operation.operation_id()
                ));
            }
        }
    }
    Ok(())
}

fn validate_reserved_operation_units(
    mount_root: &Path,
    component: &DiscoveryExecutionComponent,
) -> LocalityResult<()> {
    for (action_index, action) in component.component.actions.iter().enumerate() {
        let operations = component
            .operations
            .iter()
            .filter(|operation| operation.action_index() == action_index)
            .collect::<Vec<_>>();
        match action {
            DiscoveryProjectionAction::Create { .. } => {}
            DiscoveryProjectionAction::Move { kind, from, to, .. } => {
                let expected = move_execution_units(mount_root, kind, from, to)?;
                let actual = operations
                    .iter()
                    .map(|operation| match operation {
                        DiscoveryExecutionOperation::Move {
                            source,
                            destination,
                            ..
                        } => Ok(MoveExecutionUnit::Move {
                            source: source.clone(),
                            destination: destination.clone(),
                        }),
                        DiscoveryExecutionOperation::CreateContainer { destination, .. } => {
                            Ok(MoveExecutionUnit::CreateContainer {
                                destination: destination.clone(),
                            })
                        }
                        _ => invalid("move action contains a non-move operation"),
                    })
                    .collect::<LocalityResult<Vec<_>>>()?;
                if actual != expected {
                    return invalid("reserved discovery move operation set was tampered");
                }
            }
            DiscoveryProjectionAction::Delete { kind, path, .. } => {
                let expected = delete_unit_paths(mount_root, kind, path)?;
                let actual = operations
                    .iter()
                    .map(|operation| match operation {
                        DiscoveryExecutionOperation::Delete { source, .. } => Ok(source.clone()),
                        _ => invalid("delete action contains a non-delete operation"),
                    })
                    .collect::<LocalityResult<Vec<_>>>()?;
                if actual != expected {
                    return invalid("reserved discovery delete operation set was tampered");
                }
            }
        }
    }
    Ok(())
}

fn validate_hydration_jobs(
    execution: &DiscoveryExecutionPlan,
    effects: &DiscoveryExecutionEffects,
) -> LocalityResult<()> {
    let mut jobs = BTreeMap::new();
    for request in &execution.hydration_jobs {
        if request.mount_id != execution.mount_id {
            return invalid(format!(
                "discovery hydration job `{}` has the wrong mount",
                request.remote_id.0
            ));
        }
        let relative = request
            .path
            .strip_prefix(&execution.mount_root)
            .map_err(|_| {
                LocalityError::InvalidState(format!(
                    "discovery hydration path `{}` is outside mount `{}`",
                    request.path.display(),
                    execution.mount_root.display()
                ))
            })?;
        validate_relative_path(relative)?;
        if jobs.insert(&request.remote_id, request).is_some() {
            return invalid(format!(
                "discovery hydration job `{}` is duplicated",
                request.remote_id.0
            ));
        }
    }
    let mut effect_ids = BTreeMap::new();
    for effect in &effects.hydration_jobs {
        if !effect.upserted || !jobs.contains_key(&effect.remote_id) {
            return invalid(format!(
                "discovery hydration effect `{}` has no matching completed job",
                effect.remote_id.0
            ));
        }
        if effect_ids.insert(&effect.remote_id, ()).is_some() {
            return invalid(format!(
                "discovery hydration effect `{}` is duplicated",
                effect.remote_id.0
            ));
        }
    }
    Ok(())
}

fn expected_recovery_root(record: &DiscoveryTransactionRecord) -> LocalityResult<PathBuf> {
    discovery_recovery_root(
        &record.reservation.mount.root,
        &record.reservation.mount.mount_id,
        &record.transaction_id,
    )
}

fn ensure_operation_can_prepare(
    execution: &DiscoveryExecutionPlan,
    operation: &DiscoveryExecutionOperation,
) -> LocalityResult<()> {
    match operation {
        DiscoveryExecutionOperation::Create {
            destination,
            payload,
            temporary_payload,
            ..
        }
        | DiscoveryExecutionOperation::CreateContainer {
            destination,
            payload,
            temporary_payload,
            ..
        } => {
            let destination = checked_join(&execution.mount_root, destination, "destination")?;
            let payload = checked_join(&execution.recovery_root, payload, "payload")?;
            let temporary = checked_join(
                &execution.recovery_root,
                temporary_payload,
                "temporary payload",
            )?;
            for path in [&destination, &payload, &temporary] {
                if fingerprint_if_exists(path)?.is_some() {
                    return invalid(format!(
                        "discovery create path `{}` already exists",
                        path.display()
                    ));
                }
            }
        }
        DiscoveryExecutionOperation::Move {
            source,
            stage,
            expected_fingerprint,
            ..
        }
        | DiscoveryExecutionOperation::Delete {
            source,
            stage,
            expected_fingerprint,
            ..
        } => {
            let source = checked_join(&execution.mount_root, source, "source")?;
            let stage = checked_join(&execution.recovery_root, stage, "stage")?;
            if fingerprint_path(&source)? != *expected_fingerprint {
                return invalid(format!(
                    "discovery source `{}` does not match its prepared fingerprint",
                    source.display()
                ));
            }
            if fingerprint_if_exists(&stage)?.is_some() {
                return invalid(format!(
                    "discovery stage `{}` already exists",
                    stage.display()
                ));
            }
        }
    }
    Ok(())
}

fn validate_installed_projection(
    execution: &DiscoveryExecutionPlan,
    effects: &DiscoveryExecutionEffects,
) -> LocalityResult<()> {
    for component in &execution.components {
        for operation in &component.operations {
            let effect = effects
                .operations
                .iter()
                .find(|effect| effect.operation_id == operation.operation_id())
                .ok_or_else(|| {
                    LocalityError::InvalidState(format!(
                        "discovery operation `{}` has no effect",
                        operation.operation_id()
                    ))
                })?;
            if effect.state != DiscoveryOperationEffectState::Installed {
                return invalid(format!(
                    "discovery operation `{}` is not installed",
                    operation.operation_id()
                ));
            }
            match operation {
                DiscoveryExecutionOperation::Create { destination, .. }
                | DiscoveryExecutionOperation::CreateContainer { destination, .. }
                | DiscoveryExecutionOperation::Move { destination, .. } => {
                    let destination =
                        checked_join(&execution.mount_root, destination, "destination")?;
                    let exclusions = component
                        .operations
                        .iter()
                        .filter_map(DiscoveryExecutionOperation::destination)
                        .filter_map(|candidate| {
                            candidate
                                .strip_prefix(operation.destination().expect("create destination"))
                                .ok()
                        })
                        .filter(|relative| !relative.as_os_str().is_empty())
                        .map(Path::to_path_buf)
                        .collect::<Vec<_>>();
                    if fingerprint_if_exists_excluding(&destination, &exclusions)?.as_ref()
                        != Some(operation.expected_fingerprint())
                    {
                        return invalid(format!(
                            "discovery destination `{}` no longer matches its fingerprint",
                            destination.display()
                        ));
                    }
                }
                DiscoveryExecutionOperation::Delete { source, stage, .. } => {
                    let source_replaced_by_component = component
                        .operations
                        .iter()
                        .filter_map(DiscoveryExecutionOperation::destination)
                        .any(|destination| destination == source);
                    let source = checked_join(&execution.mount_root, source, "source")?;
                    let stage = checked_join(&execution.recovery_root, stage, "stage")?;
                    let stage_fingerprint = fingerprint_if_exists(&stage)?;
                    let recovery_root_missing = match fs::symlink_metadata(&execution.recovery_root)
                    {
                        Ok(_) => false,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                        Err(error) => return Err(LocalityError::from(error)),
                    };
                    if (fingerprint_if_exists(&source)?.is_some() && !source_replaced_by_component)
                        || (stage_fingerprint.as_ref() != Some(operation.expected_fingerprint())
                            && !(stage_fingerprint.is_none() && recovery_root_missing))
                    {
                        return invalid(format!(
                            "staged delete `{}` no longer matches its fingerprint",
                            operation.operation_id()
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn create_destination_path(kind: &EntityKind, entry_path: &Path) -> PathBuf {
    if *kind == EntityKind::Page && is_page_document_path(entry_path) {
        page_container_path(entry_path)
    } else {
        entry_path.to_path_buf()
    }
}

fn materialization_fingerprint(
    kind: &EntityKind,
    entry_path: &Path,
    materialization: &DiscoveryCreateMaterialization,
) -> LocalityResult<DiscoveryPathFingerprint> {
    match (kind, materialization) {
        (EntityKind::Page, DiscoveryCreateMaterialization::Page { document, .. }) => {
            if is_page_document_path(entry_path) {
                Ok(fingerprint_virtual_directory(&[virtual_file_record(
                    "page.md",
                    document.as_bytes(),
                )]))
            } else {
                Ok(fingerprint_virtual_file(document.as_bytes()))
            }
        }
        (EntityKind::Database, DiscoveryCreateMaterialization::Database { schema_yaml, .. }) => {
            Ok(fingerprint_virtual_directory(
                &schema_yaml
                    .as_ref()
                    .map(|schema| vec![virtual_file_record("_schema.yaml", schema.as_bytes())])
                    .unwrap_or_default(),
            ))
        }
        (EntityKind::Directory, DiscoveryCreateMaterialization::Directory { .. }) => {
            Ok(fingerprint_virtual_directory(&[]))
        }
        _ => invalid("discovery create materialization kind mismatch"),
    }
}

fn publish_create_payload(
    recovery_parent: &Path,
    temporary: &Path,
    payload: &Path,
    materialization: &DiscoveryCreateMaterialization,
    expected_fingerprint: &DiscoveryPathFingerprint,
) -> LocalityResult<()> {
    ensure_recovery_parent(recovery_parent, temporary)?;
    ensure_recovery_parent(recovery_parent, payload)?;
    match fingerprint_if_exists(temporary)? {
        Some(fingerprint) if fingerprint == *expected_fingerprint => {}
        Some(_) => {
            return invalid(format!(
                "temporary discovery payload `{}` is not fingerprint-owned",
                temporary.display()
            ));
        }
        None => {
            match (expected_fingerprint.kind, materialization) {
                (
                    DiscoveryPathKind::File,
                    DiscoveryCreateMaterialization::Page { document, .. },
                ) => {
                    write_new_file_durable(temporary, document.as_bytes())
                        .map_err(LocalityError::from)?;
                }
                (DiscoveryPathKind::Directory, _) => {
                    create_dir_all_durable(temporary).map_err(LocalityError::from)?;
                    match materialization {
                        DiscoveryCreateMaterialization::Page { document, .. } => {
                            write_new_file_durable(&temporary.join("page.md"), document.as_bytes())
                                .map_err(LocalityError::from)?;
                        }
                        DiscoveryCreateMaterialization::Database { schema_yaml, .. } => {
                            if let Some(schema_yaml) = schema_yaml {
                                write_new_file_durable(
                                    &temporary.join("_schema.yaml"),
                                    schema_yaml.as_bytes(),
                                )
                                .map_err(LocalityError::from)?;
                            }
                        }
                        DiscoveryCreateMaterialization::Directory { .. } => {}
                    }
                }
                _ => return invalid("discovery payload kind does not match its materialization"),
            }
            if fingerprint_path(temporary)? != *expected_fingerprint {
                return invalid(format!(
                    "temporary discovery payload `{}` has the wrong fingerprint",
                    temporary.display()
                ));
            }
        }
    }
    if fingerprint_if_exists(payload)?.is_some() {
        return invalid(format!(
            "discovery payload `{}` appeared before publication",
            payload.display()
        ));
    }
    rename_noreplace_durable(temporary, payload).map_err(LocalityError::from)?;
    if fingerprint_path(payload)? != *expected_fingerprint {
        return invalid(format!(
            "published discovery payload `{}` has the wrong fingerprint",
            payload.display()
        ));
    }
    Ok(())
}

fn publish_create_container_payload(
    recovery_parent: &Path,
    temporary: &Path,
    payload: &Path,
    expected_fingerprint: &DiscoveryPathFingerprint,
) -> LocalityResult<()> {
    ensure_recovery_parent(recovery_parent, temporary)?;
    ensure_recovery_parent(recovery_parent, payload)?;
    match fingerprint_if_exists(temporary)? {
        Some(fingerprint) if fingerprint == *expected_fingerprint => {}
        Some(_) => {
            return invalid(format!(
                "temporary discovery container payload `{}` is not fingerprint-owned",
                temporary.display()
            ));
        }
        None => {
            create_dir_all_durable(temporary).map_err(LocalityError::from)?;
            if fingerprint_path(temporary)? != *expected_fingerprint {
                return invalid(format!(
                    "temporary discovery container payload `{}` has the wrong fingerprint",
                    temporary.display()
                ));
            }
        }
    }
    if fingerprint_if_exists(payload)?.is_some() {
        return invalid(format!(
            "discovery container payload `{}` appeared before publication",
            payload.display()
        ));
    }
    rename_noreplace_durable(temporary, payload).map_err(LocalityError::from)?;
    if fingerprint_path(payload)? != *expected_fingerprint {
        return invalid(format!(
            "published discovery container payload `{}` has the wrong fingerprint",
            payload.display()
        ));
    }
    Ok(())
}

fn fingerprint_if_exists(path: &Path) -> LocalityResult<Option<DiscoveryPathFingerprint>> {
    fingerprint_if_exists_excluding(path, &[])
}

fn fingerprint_if_exists_excluding(
    path: &Path,
    exclusions: &[PathBuf],
) -> LocalityResult<Option<DiscoveryPathFingerprint>> {
    match fs::symlink_metadata(path) {
        Ok(_) => fingerprint_path_excluding(path, exclusions).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(LocalityError::from(error)),
    }
}

fn fingerprint_path(path: &Path) -> LocalityResult<DiscoveryPathFingerprint> {
    fingerprint_path_excluding(path, &[])
}

fn fingerprint_path_excluding(
    path: &Path,
    exclusions: &[PathBuf],
) -> LocalityResult<DiscoveryPathFingerprint> {
    let metadata = fs::symlink_metadata(path).map_err(LocalityError::from)?;
    if metadata.file_type().is_symlink() {
        return invalid(format!("discovery path `{}` is a symlink", path.display()));
    }
    if metadata.is_file() {
        let (bytes, content_sha256) = fingerprint_file_contents(path)?;
        return Ok(fingerprint_file_digest(bytes, content_sha256));
    }
    if !metadata.is_dir() {
        return invalid(format!(
            "discovery path `{}` is neither a file nor directory",
            path.display()
        ));
    }
    let mut records = Vec::new();
    collect_fingerprint_records(path, path, exclusions, &mut records)?;
    Ok(fingerprint_virtual_directory(&records))
}

fn collect_fingerprint_records(
    root: &Path,
    directory: &Path,
    exclusions: &[PathBuf],
    records: &mut Vec<DiscoveryFingerprintRecord>,
) -> LocalityResult<()> {
    let mut entries = fs::read_dir(directory)
        .map_err(LocalityError::from)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(LocalityError::from)?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let relative_path = path.strip_prefix(root).map_err(|_| {
            LocalityError::InvalidState(format!(
                "discovery fingerprint path `{}` escaped `{}`",
                path.display(),
                root.display()
            ))
        })?;
        if exclusions
            .iter()
            .any(|excluded| relative_path == excluded || relative_path.starts_with(excluded))
        {
            continue;
        }
        let relative = path_key(relative_path)?;
        let metadata = fs::symlink_metadata(&path).map_err(LocalityError::from)?;
        if metadata.file_type().is_symlink() {
            return invalid(format!(
                "discovery path `{}` contains a symlink",
                path.display()
            ));
        }
        if metadata.is_dir() {
            records.push(DiscoveryFingerprintRecord {
                path: relative,
                kind: DiscoveryPathKind::Directory,
                bytes: 0,
                content_sha256: None,
            });
            collect_fingerprint_records(root, &path, exclusions, records)?;
        } else if metadata.is_file() {
            let (bytes, content_sha256) = fingerprint_file_contents(&path)?;
            records.push(DiscoveryFingerprintRecord {
                path: relative,
                kind: DiscoveryPathKind::File,
                bytes,
                content_sha256: Some(content_sha256),
            });
        } else {
            return invalid(format!(
                "discovery path `{}` contains an unsupported node",
                path.display()
            ));
        }
    }
    Ok(())
}

fn fingerprint_virtual_file(bytes: &[u8]) -> DiscoveryPathFingerprint {
    fingerprint_file_digest(bytes.len() as u64, Sha256::digest(bytes).into())
}

fn fingerprint_file_digest(bytes: u64, content_sha256: [u8; 32]) -> DiscoveryPathFingerprint {
    let mut hasher = Sha256::new();
    hasher.update(b"file\0");
    hasher.update(bytes.to_be_bytes());
    hasher.update(content_sha256);
    DiscoveryPathFingerprint {
        kind: DiscoveryPathKind::File,
        sha256: hex_digest(&hasher.finalize()),
        entries: 1,
        bytes,
    }
}

fn fingerprint_virtual_directory(
    records: &[DiscoveryFingerprintRecord],
) -> DiscoveryPathFingerprint {
    let mut records = records.iter().collect::<Vec<_>>();
    records.sort_by(|left, right| left.path.cmp(&right.path));
    let mut hasher = Sha256::new();
    hasher.update(b"directory\0");
    let mut bytes = 0_u64;
    for record in &records {
        hasher.update(match record.kind {
            DiscoveryPathKind::File => &b"file\0"[..],
            DiscoveryPathKind::Directory => &b"directory\0"[..],
        });
        hasher.update((record.path.len() as u64).to_be_bytes());
        hasher.update(record.path.as_bytes());
        hasher.update(record.bytes.to_be_bytes());
        if let Some(content_sha256) = record.content_sha256 {
            hasher.update(content_sha256);
        }
        bytes += record.bytes;
    }
    DiscoveryPathFingerprint {
        kind: DiscoveryPathKind::Directory,
        sha256: hex_digest(&hasher.finalize()),
        entries: records.len() as u64,
        bytes,
    }
}

fn virtual_file_record(path: impl Into<String>, bytes: &[u8]) -> DiscoveryFingerprintRecord {
    DiscoveryFingerprintRecord {
        path: path.into(),
        kind: DiscoveryPathKind::File,
        bytes: bytes.len() as u64,
        content_sha256: Some(Sha256::digest(bytes).into()),
    }
}

fn fingerprint_file_contents(path: &Path) -> LocalityResult<(u64, [u8; 32])> {
    let mut file = fs::File::open(path).map_err(LocalityError::from)?;
    if !file.metadata().map_err(LocalityError::from)?.is_file() {
        return invalid(format!(
            "discovery fingerprint path `{}` is not a regular file",
            path.display()
        ));
    }
    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(LocalityError::from)?;
        if read == 0 {
            break;
        }
        bytes = bytes.checked_add(read as u64).ok_or_else(|| {
            LocalityError::InvalidState(format!(
                "discovery fingerprint path `{}` is too large",
                path.display()
            ))
        })?;
        hasher.update(&buffer[..read]);
    }
    Ok((bytes, hasher.finalize().into()))
}

fn path_key(path: &Path) -> LocalityResult<String> {
    let mut components = Vec::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            return invalid(format!(
                "discovery fingerprint path `{}` is not normalized",
                path.display()
            ));
        };
        components.push(
            component
                .to_str()
                .ok_or_else(|| {
                    LocalityError::InvalidState(format!(
                        "discovery fingerprint path `{}` is not UTF-8",
                        path.display()
                    ))
                })?
                .to_string(),
        );
    }
    Ok(components.join("/"))
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

fn checked_join(base: &Path, relative: &Path, label: &str) -> LocalityResult<PathBuf> {
    validate_relative_path(relative)?;
    let joined = base.join(relative);
    if !joined.starts_with(base) {
        return invalid(format!(
            "discovery {label} `{}` escapes `{}`",
            relative.display(),
            base.display()
        ));
    }
    Ok(joined)
}

fn ensure_recovery_parent(base: &Path, path: &Path) -> LocalityResult<()> {
    let parent = path.parent().ok_or_else(|| {
        LocalityError::InvalidState(format!("path `{}` has no parent", path.display()))
    })?;
    let relative = parent.strip_prefix(base).map_err(|_| {
        LocalityError::InvalidState(format!(
            "path `{}` is outside `{}`",
            parent.display(),
            base.display()
        ))
    })?;
    reject_symlink(base)?;
    let mut cursor = base.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return invalid(format!(
                "path `{}` is not normalized below `{}`",
                parent.display(),
                base.display()
            ));
        };
        cursor.push(component);
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return invalid(format!("path `{}` is a symlink", cursor.display()));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return invalid(format!("path `{}` is not a directory", cursor.display()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(LocalityError::from(error)),
        }
    }
    create_dir_all_durable(parent).map_err(LocalityError::from)
}

fn require_existing_safe_parent(base: &Path, path: &Path) -> LocalityResult<()> {
    let parent = path.parent().ok_or_else(|| {
        LocalityError::InvalidState(format!("path `{}` has no parent", path.display()))
    })?;
    let relative = parent.strip_prefix(base).map_err(|_| {
        LocalityError::InvalidState(format!(
            "path `{}` is outside `{}`",
            parent.display(),
            base.display()
        ))
    })?;
    reject_symlink(base)?;
    let mut cursor = base.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return invalid(format!(
                "path `{}` is not normalized below `{}`",
                parent.display(),
                base.display()
            ));
        };
        cursor.push(component);
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return invalid(format!("path `{}` is a symlink", cursor.display()));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return invalid(format!("path `{}` is not a directory", cursor.display()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return invalid(format!(
                    "required destination parent `{}` does not exist",
                    cursor.display()
                ));
            }
            Err(error) => return Err(LocalityError::from(error)),
        }
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> LocalityResult<()> {
    let metadata = fs::symlink_metadata(path).map_err(LocalityError::from)?;
    if metadata.file_type().is_symlink() {
        return invalid(format!("path `{}` is a symlink", path.display()));
    }
    if !metadata.is_dir() {
        return invalid(format!("path `{}` is not a directory", path.display()));
    }
    Ok(())
}

fn require_recovery_root_absent(path: &Path) -> LocalityResult<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => invalid(format!(
            "discovery recovery root `{}` already exists",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(LocalityError::from(error)),
    }
}

fn validate_mount_root(path: &Path) -> LocalityResult<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return invalid(format!(
            "discovery mount root `{}` must be absolute and normalized",
            path.display()
        ));
    }
    reject_symlink(path)?;
    let parent = path.parent().ok_or_else(|| {
        LocalityError::InvalidState(format!("mount root `{}` has no parent", path.display()))
    })?;
    reject_symlink(parent)?;
    if !same_volume(path, parent).map_err(LocalityError::from)? {
        return invalid(format!(
            "discovery mount root `{}` and recovery parent `{}` are on different volumes",
            path.display(),
            parent.display()
        ));
    }
    Ok(())
}

fn validate_relative_path_ancestry(base: &Path, relative: &Path) -> LocalityResult<()> {
    validate_relative_path(relative)?;
    reject_symlink(base)?;
    let mut cursor = base.to_path_buf();
    let component_count = relative.components().count();
    for (index, component) in relative.components().enumerate() {
        let Component::Normal(component) = component else {
            return invalid(format!("path `{}` is not normalized", relative.display()));
        };
        cursor.push(component);
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return invalid(format!("path `{}` is a symlink", cursor.display()));
            }
            Ok(metadata) if index + 1 < component_count && !metadata.is_dir() => {
                return invalid(format!(
                    "path ancestor `{}` is not a directory",
                    cursor.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(LocalityError::from(error)),
        }
    }
    Ok(())
}

fn validate_execution_path_ancestry(execution: &DiscoveryExecutionPlan) -> LocalityResult<()> {
    let recovery_parent = execution.mount_root.parent().ok_or_else(|| {
        LocalityError::InvalidState(format!(
            "mount root `{}` has no recovery parent",
            execution.mount_root.display()
        ))
    })?;
    let recovery_relative = execution
        .recovery_root
        .strip_prefix(recovery_parent)
        .map_err(|_| {
            LocalityError::InvalidState("recovery root escaped mount parent".to_string())
        })?;
    validate_relative_path_ancestry(recovery_parent, recovery_relative)?;
    for operation in execution
        .components
        .iter()
        .flat_map(|component| component.operations.iter())
    {
        if let Some(source) = operation.source() {
            validate_relative_path_ancestry(&execution.mount_root, source)?;
        }
        if let Some(destination) = operation.destination() {
            validate_relative_path_ancestry(&execution.mount_root, destination)?;
        }
        match operation {
            DiscoveryExecutionOperation::Create {
                payload,
                temporary_payload,
                ..
            }
            | DiscoveryExecutionOperation::CreateContainer {
                payload,
                temporary_payload,
                ..
            } => {
                validate_relative_path_ancestry(&execution.recovery_root, payload).or_else(
                    |error| {
                        if fs::symlink_metadata(&execution.recovery_root)
                            .is_err_and(|io| io.kind() == std::io::ErrorKind::NotFound)
                        {
                            Ok(())
                        } else {
                            Err(error)
                        }
                    },
                )?;
                validate_relative_path_ancestry(&execution.recovery_root, temporary_payload)
                    .or_else(|error| {
                        if fs::symlink_metadata(&execution.recovery_root)
                            .is_err_and(|io| io.kind() == std::io::ErrorKind::NotFound)
                        {
                            Ok(())
                        } else {
                            Err(error)
                        }
                    })?;
            }
            DiscoveryExecutionOperation::Move { stage, .. }
            | DiscoveryExecutionOperation::Delete { stage, .. } => {
                validate_relative_path_ancestry(&execution.recovery_root, stage).or_else(
                    |error| {
                        if fs::symlink_metadata(&execution.recovery_root)
                            .is_err_and(|io| io.kind() == std::io::ErrorKind::NotFound)
                        {
                            Ok(())
                        } else {
                            Err(error)
                        }
                    },
                )?;
            }
        }
    }
    Ok(())
}

fn prepare_components(
    mount_root: &Path,
    components: Vec<DiscoveryProjectionComponent>,
    materializations: Vec<DiscoveryCreateMaterialization>,
) -> LocalityResult<Vec<DiscoveryExecutionComponent>> {
    let mut materializations_by_id = BTreeMap::new();
    for materialization in materializations {
        let remote_id = materialization.remote_id().clone();
        if materializations_by_id
            .insert(remote_id.clone(), materialization)
            .is_some()
        {
            return invalid(format!(
                "duplicate discovery create materialization for `{}`",
                remote_id.0
            ));
        }
    }
    let mut prepared = Vec::with_capacity(components.len());
    for (component_index, component) in components.into_iter().enumerate() {
        let mut operations = Vec::new();
        for (action_index, action) in component.actions.iter().enumerate() {
            let base_id = format!("component-{component_index:08}-action-{action_index:08}");
            match action {
                DiscoveryProjectionAction::Create { entry } => {
                    let expected_kind = match entry.kind {
                        EntityKind::Page => EntityKind::Page,
                        EntityKind::Database => EntityKind::Database,
                        EntityKind::Directory => EntityKind::Directory,
                        EntityKind::Asset | EntityKind::Unknown(_) => {
                            return invalid(format!(
                                "discovery create `{}` has unsupported plain-files kind `{:?}`",
                                entry.remote_id.0, entry.kind
                            ));
                        }
                    };
                    let materialization = materializations_by_id
                        .remove(&entry.remote_id)
                        .ok_or_else(|| {
                            LocalityError::InvalidState(format!(
                                "discovery create `{}` is missing its materialization",
                                entry.remote_id.0
                            ))
                        })?;
                    if materialization.kind() != expected_kind {
                        return invalid(format!(
                            "discovery create `{}` materialization kind does not match `{:?}`",
                            entry.remote_id.0, entry.kind
                        ));
                    }
                    validate_relative_path(&entry.path)?;
                    let destination = create_destination_path(&entry.kind, &entry.path);
                    let expected_fingerprint =
                        materialization_fingerprint(&entry.kind, &entry.path, &materialization)?;
                    operations.push(DiscoveryExecutionOperation::Create {
                        operation_id: base_id.clone(),
                        action_index: action_index as u32,
                        remote_id: entry.remote_id.clone(),
                        kind: entry.kind.clone(),
                        projected_path: entry.path.clone(),
                        destination,
                        payload: PathBuf::from("payloads").join(&base_id),
                        temporary_payload: PathBuf::from("temporary").join(&base_id),
                        expected_fingerprint,
                        materialization,
                    });
                }
                DiscoveryProjectionAction::Move {
                    remote_id,
                    kind,
                    from,
                    to,
                } => {
                    let units = move_execution_units(mount_root, kind, from, to)?;
                    let mut move_unit_index = 0;
                    for unit in units {
                        match unit {
                            MoveExecutionUnit::Move {
                                source,
                                destination,
                            } => {
                                validate_relative_path_ancestry(mount_root, &source)?;
                                validate_relative_path_ancestry(mount_root, &destination)?;
                                let operation_id = unit_operation_id(&base_id, move_unit_index);
                                move_unit_index += 1;
                                operations.push(DiscoveryExecutionOperation::Move {
                                    operation_id: operation_id.clone(),
                                    action_index: action_index as u32,
                                    remote_id: remote_id.clone(),
                                    kind: kind.clone(),
                                    projected_from: from.clone(),
                                    projected_to: to.clone(),
                                    expected_fingerprint: fingerprint_path(
                                        &mount_root.join(&source),
                                    )?,
                                    source,
                                    stage: PathBuf::from("stages").join(operation_id),
                                    destination,
                                });
                            }
                            MoveExecutionUnit::CreateContainer { destination } => {
                                validate_relative_path_ancestry(mount_root, &destination)?;
                                let operation_id = container_operation_id(&base_id);
                                operations.push(DiscoveryExecutionOperation::CreateContainer {
                                    operation_id: operation_id.clone(),
                                    action_index: action_index as u32,
                                    remote_id: remote_id.clone(),
                                    projected_from: from.clone(),
                                    projected_to: to.clone(),
                                    destination,
                                    payload: PathBuf::from("payloads").join(&operation_id),
                                    temporary_payload: PathBuf::from("temporary")
                                        .join(&operation_id),
                                    expected_fingerprint: fingerprint_virtual_directory(&[]),
                                });
                            }
                        }
                    }
                }
                DiscoveryProjectionAction::Delete {
                    remote_id,
                    kind,
                    path,
                } => {
                    let units = delete_unit_paths(mount_root, kind, path)?;
                    for (unit_index, source) in units.into_iter().enumerate() {
                        validate_relative_path_ancestry(mount_root, &source)?;
                        let operation_id = unit_operation_id(&base_id, unit_index);
                        operations.push(DiscoveryExecutionOperation::Delete {
                            operation_id: operation_id.clone(),
                            action_index: action_index as u32,
                            remote_id: remote_id.clone(),
                            kind: kind.clone(),
                            projected_path: path.clone(),
                            expected_fingerprint: fingerprint_path(&mount_root.join(&source))?,
                            source,
                            stage: PathBuf::from("stages").join(operation_id),
                        });
                    }
                }
            }
        }
        apply_nested_source_fingerprints(mount_root, &mut operations)?;
        validate_prepared_destinations(mount_root, &operations)?;
        prepared.push(DiscoveryExecutionComponent {
            component,
            operations,
        });
    }
    if let Some((remote_id, _)) = materializations_by_id.into_iter().next() {
        return invalid(format!(
            "discovery create materialization `{}` has no matching create action",
            remote_id.0
        ));
    }
    Ok(prepared)
}

fn unit_operation_id(base: &str, unit_index: usize) -> String {
    if unit_index == 0 {
        base.to_string()
    } else {
        format!("{base}-unit-{unit_index:08}")
    }
}

fn container_operation_id(base: &str) -> String {
    format!("{base}-container")
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MoveExecutionUnit {
    Move {
        source: PathBuf,
        destination: PathBuf,
    },
    CreateContainer {
        destination: PathBuf,
    },
}

fn move_execution_units(
    mount_root: &Path,
    kind: &EntityKind,
    from: &Path,
    to: &Path,
) -> LocalityResult<Vec<MoveExecutionUnit>> {
    let (required, optional) = allowed_move_unit_paths(kind, from, to)?;
    let mut units = required
        .into_iter()
        .map(|(source, destination)| MoveExecutionUnit::Move {
            source,
            destination,
        })
        .collect::<Vec<_>>();
    if let Some((source, destination)) = optional {
        validate_relative_path_ancestry(mount_root, &source)?;
        match fs::symlink_metadata(mount_root.join(&source)) {
            Ok(_) => units.push(MoveExecutionUnit::Move {
                source,
                destination,
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if move_requires_destination_container(kind, from, to) {
                    units.push(MoveExecutionUnit::CreateContainer { destination });
                }
            }
            Err(error) => return Err(LocalityError::from(error)),
        }
    }
    Ok(units)
}

fn delete_unit_paths(
    mount_root: &Path,
    kind: &EntityKind,
    path: &Path,
) -> LocalityResult<Vec<PathBuf>> {
    let (mut required, optional) = allowed_delete_unit_paths(kind, path)?;
    let Some(child_container) = optional else {
        return Ok(required);
    };
    validate_relative_path_ancestry(mount_root, &child_container)?;
    match fs::symlink_metadata(mount_root.join(&child_container)) {
        Ok(_) => required.push(child_container),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(LocalityError::from(error)),
    }
    Ok(required)
}

type MoveUnit = (PathBuf, PathBuf);

fn move_requires_destination_container(kind: &EntityKind, from: &Path, to: &Path) -> bool {
    *kind == EntityKind::Page && !is_page_document_path(from) && is_page_document_path(to)
}

fn allowed_move_unit_paths(
    kind: &EntityKind,
    from: &Path,
    to: &Path,
) -> LocalityResult<(Vec<MoveUnit>, Option<MoveUnit>)> {
    validate_relative_path(from)?;
    validate_relative_path(to)?;
    if *kind != EntityKind::Page {
        return Ok((vec![(from.to_path_buf(), to.to_path_buf())], None));
    }

    let from_container = page_container_path(from);
    let to_container = page_container_path(to);
    match (is_page_document_path(from), is_page_document_path(to)) {
        (true, true) => Ok((vec![(from_container, to_container)], None)),
        (true, false) => Ok((
            vec![
                (from.to_path_buf(), to.to_path_buf()),
                (from_container, to_container),
            ],
            None,
        )),
        (false, _) => Ok((
            vec![(from.to_path_buf(), to.to_path_buf())],
            Some((from_container, to_container)),
        )),
    }
}

fn allowed_delete_unit_paths(
    kind: &EntityKind,
    path: &Path,
) -> LocalityResult<(Vec<PathBuf>, Option<PathBuf>)> {
    validate_relative_path(path)?;
    if *kind != EntityKind::Page {
        return Ok((vec![path.to_path_buf()], None));
    }
    if is_page_document_path(path) {
        return Ok((vec![page_container_path(path)], None));
    }
    Ok((vec![path.to_path_buf()], Some(page_container_path(path))))
}

fn apply_nested_source_fingerprints(
    mount_root: &Path,
    operations: &mut [DiscoveryExecutionOperation],
) -> LocalityResult<()> {
    let sources = operations
        .iter()
        .filter_map(DiscoveryExecutionOperation::source)
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    for operation in operations {
        let Some(source) = operation.source().map(Path::to_path_buf) else {
            continue;
        };
        let exclusions = sources
            .iter()
            .filter_map(|candidate| candidate.strip_prefix(&source).ok())
            .filter(|relative| !relative.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
        let fingerprint = fingerprint_path_excluding(&mount_root.join(&source), &exclusions)?;
        operation.set_expected_fingerprint(fingerprint);
    }
    Ok(())
}

fn validate_prepared_destinations(
    mount_root: &Path,
    operations: &[DiscoveryExecutionOperation],
) -> LocalityResult<()> {
    let mut sources = BTreeMap::new();
    let mut destinations = BTreeMap::new();
    let mut recovery_paths = BTreeMap::new();

    for operation in operations {
        if let Some(source) = operation.source() {
            validate_relative_path(source)?;
            validate_relative_path_ancestry(mount_root, source)?;
            if sources
                .insert(source.to_path_buf(), operation.operation_id())
                .is_some()
            {
                return invalid(format!(
                    "multiple discovery operations own source `{}`",
                    source.display()
                ));
            }
        }
        match operation {
            DiscoveryExecutionOperation::Create {
                payload,
                temporary_payload,
                ..
            }
            | DiscoveryExecutionOperation::CreateContainer {
                payload,
                temporary_payload,
                ..
            } => {
                for path in [payload, temporary_payload] {
                    if recovery_paths
                        .insert(path.to_path_buf(), operation.operation_id())
                        .is_some()
                    {
                        return invalid(format!(
                            "multiple discovery operations own recovery path `{}`",
                            path.display()
                        ));
                    }
                }
            }
            DiscoveryExecutionOperation::Move { stage, .. }
            | DiscoveryExecutionOperation::Delete { stage, .. } => {
                if recovery_paths
                    .insert(stage.to_path_buf(), operation.operation_id())
                    .is_some()
                {
                    return invalid(format!(
                        "multiple discovery operations own recovery path `{}`",
                        stage.display()
                    ));
                }
            }
        }
    }

    for operation in operations {
        let Some(destination) = operation.destination() else {
            continue;
        };
        validate_relative_path(destination)?;
        validate_relative_path_ancestry(mount_root, destination)?;
        validate_destination_ancestor_coverage(mount_root, destination, operations)?;
        if destinations
            .insert(destination.to_path_buf(), operation.operation_id())
            .is_some()
        {
            return invalid(format!(
                "multiple discovery operations target destination `{}`",
                destination.display()
            ));
        }
        let destination_path = mount_root.join(destination);
        if fingerprint_if_exists(&destination_path)?.is_some()
            && !sources.keys().any(|source| destination.starts_with(source))
        {
            return invalid(format!(
                "discovery destination `{}` already exists outside a staged source",
                destination_path.display()
            ));
        }
    }
    Ok(())
}

fn validate_destination_ancestor_coverage(
    mount_root: &Path,
    destination: &Path,
    operations: &[DiscoveryExecutionOperation],
) -> LocalityResult<()> {
    let parent = destination.parent().ok_or_else(|| {
        LocalityError::InvalidState(format!(
            "discovery destination `{}` has no parent",
            destination.display()
        ))
    })?;
    let mut relative = PathBuf::new();
    for component in parent.components() {
        let Component::Normal(component) = component else {
            return invalid(format!(
                "discovery destination `{}` is not normalized",
                destination.display()
            ));
        };
        relative.push(component);
        let path = mount_root.join(&relative);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return invalid(format!("path `{}` is a symlink", path.display()));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return invalid(format!("path `{}` is not a directory", path.display()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let provided = operations.iter().any(|candidate| {
                    candidate.destination() == Some(relative.as_path())
                        && candidate.expected_fingerprint().kind == DiscoveryPathKind::Directory
                });
                if !provided {
                    return invalid(format!(
                        "discovery destination ancestor `{}` is absent and no directory operation provides it",
                        path.display()
                    ));
                }
            }
            Err(error) => return Err(LocalityError::from(error)),
        }
    }
    Ok(())
}

fn plain_files_hydration_actions(
    actions: Vec<DiscoveryPostCommitAction>,
) -> LocalityResult<Vec<HydrationRequest>> {
    let mut hydration = Vec::new();
    for action in actions {
        match action {
            DiscoveryPostCommitAction::QueueHydration(request) => hydration.push(request),
            DiscoveryPostCommitAction::InvalidateProvider { .. } => {
                return invalid(
                    "plain-files discovery execution does not accept provider invalidation actions",
                );
            }
        }
    }
    Ok(hydration)
}

fn discovery_recovery_root(
    mount_root: &Path,
    mount_id: &MountId,
    transaction_id: &DiscoveryTransactionId,
) -> LocalityResult<PathBuf> {
    let parent = mount_root.parent().ok_or_else(|| {
        LocalityError::InvalidState(format!(
            "mount root `{}` has no same-volume recovery parent",
            mount_root.display()
        ))
    })?;
    Ok(parent
        .join(".locality-recovery")
        .join("discovery")
        .join(identifier_hash(&mount_id.0))
        .join(identifier_hash(transaction_id.as_str())))
}

fn identifier_hash(value: &str) -> String {
    hex_digest(Sha256::digest(value.as_bytes()).as_slice())
}

fn validate_relative_path(path: &Path) -> LocalityResult<()> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return invalid(format!(
            "discovery execution path `{}` must be a nonempty relative path",
            path.display()
        ));
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return invalid(format!(
            "discovery execution path `{}` must be normalized",
            path.display()
        ));
    }
    Ok(())
}

fn invalid<T>(message: impl Into<String>) -> LocalityResult<T> {
    Err(LocalityError::InvalidState(message.into()))
}

fn repository_error(error: StoreError) -> LocalityError {
    match error {
        StoreError::InvalidState(message) => {
            LocalityError::Io(format!("discovery repository invalid state: {message}"))
        }
        error => LocalityError::from(error),
    }
}

fn invalid_json(error: serde_json::Error) -> LocalityError {
    LocalityError::InvalidState(error.to_string())
}
