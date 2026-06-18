//! Journal-backed `afs log` and `afs undo` orchestration.
//!
//! The log surface is a read-only view over durable push journals. Undo uses the
//! journaled preimage snapshots and apply effects to derive a connector-neutral
//! reverse plan, then applies it through a connector hook when the plan is
//! complete.

use std::path::{Path, PathBuf};

use afs_core::AfsError;
use afs_core::journal::{JournalEntry, JournalStatus, PushId};
use afs_core::model::{MountId, RemoteId};
use afs_core::undo::{
    UndoApplier, UndoApplyRequest, UndoOperation, UndoPlan, UndoPlanStatus,
    UnsupportedUndoOperation, plan_journal_undo,
};
use afs_store::{EntityRepository, JournalRepository, MountConfig, MountRepository, StoreError};
use afsd::file_provider;
use serde::Serialize;

use crate::diff::PlanSummaryOutput;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogOptions {
    pub path: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LogReport {
    pub ok: bool,
    pub command: &'static str,
    pub entries: Vec<JournalEntryOutput>,
}

pub fn run_log<S>(store: &S, options: LogOptions) -> Result<LogReport, HistoryError>
where
    S: JournalRepository + MountRepository + EntityRepository,
{
    let filter = options
        .path
        .as_deref()
        .map(|path| resolve_path_filter(store, path))
        .transpose()?;
    let mut entries = store.list_journal().map_err(HistoryError::Store)?;

    if let Some(filter) = filter {
        entries.retain(|entry| entry_matches_filter(entry, &filter));
    }

    entries.sort_by(|left, right| right.push_id.0.cmp(&left.push_id.0));

    Ok(LogReport {
        ok: true,
        command: "log",
        entries: entries.into_iter().map(JournalEntryOutput::from).collect(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UndoReport {
    pub ok: bool,
    pub command: &'static str,
    pub push_id: String,
    pub status: String,
    pub action: String,
    pub message: String,
    pub changed_remote_ids: Vec<String>,
    pub entry: Option<JournalEntryOutput>,
    pub undo_plan: Option<UndoPlanOutput>,
}

pub fn run_undo<S>(store: &mut S, push_id: impl Into<String>) -> Result<UndoReport, HistoryError>
where
    S: JournalRepository,
{
    let push_id = PushId(push_id.into());
    let entry = store
        .get_journal(&push_id)
        .map_err(HistoryError::Store)?
        .ok_or_else(|| HistoryError::JournalNotFound(push_id.clone()))?;

    match entry.status.clone() {
        JournalStatus::Prepared => {
            store
                .update_journal_status(&push_id, JournalStatus::Reverted)
                .map_err(HistoryError::Store)?;
            let mut reverted = entry;
            reverted.status = JournalStatus::Reverted;

            Ok(UndoReport {
                ok: true,
                command: "undo",
                push_id: push_id.0,
                status: "reverted".to_string(),
                action: "reverted_local_journal".to_string(),
                message: "journal entry reverted before remote apply".to_string(),
                changed_remote_ids: Vec::new(),
                entry: Some(JournalEntryOutput::from(reverted)),
                undo_plan: None,
            })
        }
        JournalStatus::Reverted => Ok(UndoReport {
            ok: true,
            command: "undo",
            push_id: push_id.0,
            status: "reverted".to_string(),
            action: "already_reverted".to_string(),
            message: "journal entry was already reverted".to_string(),
            changed_remote_ids: Vec::new(),
            entry: Some(JournalEntryOutput::from(entry)),
            undo_plan: None,
        }),
        JournalStatus::Failed(_) if entry.apply_effects.is_empty() => {
            store
                .update_journal_status(&push_id, JournalStatus::Reverted)
                .map_err(HistoryError::Store)?;
            let mut reverted = entry;
            reverted.status = JournalStatus::Reverted;

            Ok(UndoReport {
                ok: true,
                command: "undo",
                push_id: push_id.0,
                status: "reverted".to_string(),
                action: "reverted_empty_failed_journal".to_string(),
                message: "failed journal had no recorded remote effects and was marked reverted"
                    .to_string(),
                changed_remote_ids: Vec::new(),
                entry: Some(JournalEntryOutput::from(reverted)),
                undo_plan: None,
            })
        }
        JournalStatus::Applied | JournalStatus::Reconciled => {
            let undo_plan = plan_journal_undo(&entry);
            let (action, message) = undo_boundary(&undo_plan);
            Ok(UndoReport {
                ok: false,
                command: "undo",
                push_id: push_id.0,
                status: status_name(&entry.status).to_string(),
                action: action.to_string(),
                message: message.to_string(),
                changed_remote_ids: Vec::new(),
                undo_plan: Some(UndoPlanOutput::from(undo_plan)),
                entry: Some(JournalEntryOutput::from(entry)),
            })
        }
        status => Ok(UndoReport {
            ok: false,
            command: "undo",
            push_id: push_id.0,
            status: status_name(&status).to_string(),
            action: "undo_unsafe_journal_status".to_string(),
            message: undo_boundary_message(&status).to_string(),
            changed_remote_ids: Vec::new(),
            entry: Some(JournalEntryOutput::from(entry)),
            undo_plan: None,
        }),
    }
}

pub fn run_undo_with_applier<S, A>(
    store: &mut S,
    push_id: impl Into<String>,
    applier: &mut A,
) -> Result<UndoReport, HistoryError>
where
    S: JournalRepository,
    A: UndoApplier,
{
    let push_id = PushId(push_id.into());
    let entry = store
        .get_journal(&push_id)
        .map_err(HistoryError::Store)?
        .ok_or_else(|| HistoryError::JournalNotFound(push_id.clone()))?;

    if !matches!(
        entry.status,
        JournalStatus::Applied | JournalStatus::Reconciled
    ) {
        return run_undo(store, push_id.0);
    }

    let undo_plan = plan_journal_undo(&entry);
    if undo_plan.status != UndoPlanStatus::Complete {
        let (action, message) = undo_boundary(&undo_plan);
        return Ok(UndoReport {
            ok: false,
            command: "undo",
            push_id: push_id.0,
            status: status_name(&entry.status).to_string(),
            action: action.to_string(),
            message: message.to_string(),
            changed_remote_ids: Vec::new(),
            undo_plan: Some(UndoPlanOutput::from(undo_plan)),
            entry: Some(JournalEntryOutput::from(entry)),
        });
    }

    let apply_result = match applier.apply_undo(UndoApplyRequest {
        target_push_id: &push_id,
        mount_id: &entry.mount_id,
        plan: &undo_plan,
    }) {
        Ok(result) => result,
        Err(error) => {
            let action = match &error {
                AfsError::NotImplemented(_) => "reverse_apply_not_implemented",
                _ => "reverse_apply_failed",
            };
            return Ok(UndoReport {
                ok: false,
                command: "undo",
                push_id: push_id.0,
                status: status_name(&entry.status).to_string(),
                action: action.to_string(),
                message: error.to_string(),
                changed_remote_ids: Vec::new(),
                undo_plan: Some(UndoPlanOutput::from(undo_plan)),
                entry: Some(JournalEntryOutput::from(entry)),
            });
        }
    };

    store
        .update_journal_status(&push_id, JournalStatus::Reverted)
        .map_err(HistoryError::Store)?;
    let mut reverted = entry;
    reverted.status = JournalStatus::Reverted;

    Ok(UndoReport {
        ok: true,
        command: "undo",
        push_id: push_id.0,
        status: "reverted".to_string(),
        action: "reverse_applied".to_string(),
        message: "remote undo applied and journal entry marked reverted".to_string(),
        changed_remote_ids: apply_result
            .changed_remote_ids
            .into_iter()
            .map(|remote_id| remote_id.0)
            .collect(),
        undo_plan: Some(UndoPlanOutput::from(undo_plan)),
        entry: Some(JournalEntryOutput::from(reverted)),
    })
}

pub fn undo_report_exit_code(report: &UndoReport) -> i32 {
    if report.ok {
        0
    } else if report.action == "reverse_apply_failed" {
        1
    } else {
        5
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct JournalEntryOutput {
    pub push_id: String,
    pub mount_id: String,
    pub remote_ids: Vec<String>,
    pub status: String,
    pub failure: Option<String>,
    pub preimage_count: usize,
    pub apply_effect_count: usize,
    pub plan_summary: PlanSummaryOutput,
    pub operation_count: usize,
}

impl From<JournalEntry> for JournalEntryOutput {
    fn from(value: JournalEntry) -> Self {
        let (status, failure) = status_parts(value.status);
        let operation_count = value.plan.operations.len();

        Self {
            push_id: value.push_id.0,
            mount_id: value.mount_id.0,
            remote_ids: value
                .remote_ids
                .into_iter()
                .map(|remote_id| remote_id.0)
                .collect(),
            status,
            failure,
            preimage_count: value.preimages.len(),
            apply_effect_count: value.apply_effects.len(),
            plan_summary: PlanSummaryOutput::from(value.plan.summary),
            operation_count,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UndoPlanOutput {
    pub target_push_id: String,
    pub mount_id: String,
    pub affected_entities: Vec<String>,
    pub status: String,
    pub operations: Vec<UndoOperationOutput>,
    pub unsupported: Vec<UnsupportedUndoOutput>,
}

impl From<UndoPlan> for UndoPlanOutput {
    fn from(value: UndoPlan) -> Self {
        Self {
            target_push_id: value.target_push_id.0,
            mount_id: value.mount_id.0,
            affected_entities: value
                .affected_entities
                .into_iter()
                .map(|remote_id| remote_id.0)
                .collect(),
            status: undo_plan_status_name(&value.status).to_string(),
            operations: value
                .operations
                .into_iter()
                .map(UndoOperationOutput::from)
                .collect(),
            unsupported: value
                .unsupported
                .into_iter()
                .map(UnsupportedUndoOutput::from)
                .collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UndoOperationOutput {
    RestoreBlockContent {
        block_id: String,
        content: String,
    },
    MoveBlock {
        block_id: String,
        after: Option<String>,
    },
    RestoreArchivedBlock {
        block_id: String,
        parent_id: String,
        after: Option<String>,
        content: String,
    },
    ArchiveCreatedBlock {
        block_id: String,
    },
    ArchiveCreatedEntity {
        entity_id: String,
    },
}

impl From<UndoOperation> for UndoOperationOutput {
    fn from(value: UndoOperation) -> Self {
        match value {
            UndoOperation::RestoreBlockContent { block_id, content } => Self::RestoreBlockContent {
                block_id: block_id.0,
                content,
            },
            UndoOperation::MoveBlock { block_id, after } => Self::MoveBlock {
                block_id: block_id.0,
                after: after.map(|remote_id| remote_id.0),
            },
            UndoOperation::RestoreArchivedBlock {
                block_id,
                parent_id,
                after,
                content,
            } => Self::RestoreArchivedBlock {
                block_id: block_id.0,
                parent_id: parent_id.0,
                after: after.map(|remote_id| remote_id.0),
                content,
            },
            UndoOperation::ArchiveCreatedBlock { block_id } => Self::ArchiveCreatedBlock {
                block_id: block_id.0,
            },
            UndoOperation::ArchiveCreatedEntity { entity_id } => Self::ArchiveCreatedEntity {
                entity_id: entity_id.0,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UnsupportedUndoOutput {
    pub operation_index: usize,
    pub code: String,
    pub message: String,
}

impl From<UnsupportedUndoOperation> for UnsupportedUndoOutput {
    fn from(value: UnsupportedUndoOperation) -> Self {
        Self {
            operation_index: value.operation_index,
            code: value.code,
            message: value.message,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryError {
    MountNotFound(PathBuf),
    JournalNotFound(PushId),
    Store(StoreError),
}

impl HistoryError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::MountNotFound(_) => "mount_not_found",
            Self::JournalNotFound(_) => "journal_not_found",
            Self::Store(StoreError::EntityPathMissing { .. }) => "entity_path_missing",
            Self::Store(_) => "store_error",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::MountNotFound(path) => {
                format!("no AgentFS mount contains `{}`", path.display())
            }
            Self::JournalNotFound(push_id) => {
                format!("journal entry `{}` was not found", push_id.0)
            }
            Self::Store(error) => error.to_string(),
        }
    }
}

struct PathFilter {
    mount_id: MountId,
    remote_id: RemoteId,
}

fn resolve_path_filter<S>(store: &S, path: &Path) -> Result<PathFilter, HistoryError>
where
    S: MountRepository + EntityRepository,
{
    let absolute_path = absolute_path(path)?;
    let mounts = store.load_mounts().map_err(HistoryError::Store)?;
    let mount = find_mount_for_path(&mounts, &absolute_path)
        .ok_or_else(|| HistoryError::MountNotFound(absolute_path.clone()))?;
    let relative_path = relative_entity_path(mount, &absolute_path)?;
    let entity = store
        .find_entity_by_path(&mount.mount_id, &relative_path)
        .map_err(HistoryError::Store)?
        .ok_or_else(|| {
            HistoryError::Store(StoreError::EntityPathMissing {
                mount_id: mount.mount_id.clone(),
                path: relative_path,
            })
        })?;

    Ok(PathFilter {
        mount_id: mount.mount_id.clone(),
        remote_id: entity.remote_id,
    })
}

fn entry_matches_filter(entry: &JournalEntry, filter: &PathFilter) -> bool {
    entry.mount_id == filter.mount_id
        && (entry
            .remote_ids
            .iter()
            .any(|remote_id| remote_id == &filter.remote_id)
            || entry
                .plan
                .affected_entities
                .iter()
                .any(|remote_id| remote_id == &filter.remote_id))
}

fn absolute_path(path: &Path) -> Result<PathBuf, HistoryError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| HistoryError::Store(StoreError::Io(error.to_string())))
    }
}

fn find_mount_for_path<'a>(mounts: &'a [MountConfig], path: &Path) -> Option<&'a MountConfig> {
    file_provider::find_mount_for_path(mounts, path).map(|(mount, _)| mount)
}

fn relative_entity_path(
    mount: &MountConfig,
    absolute_path: &Path,
) -> Result<PathBuf, HistoryError> {
    file_provider::match_mount_path(mount, absolute_path)
        .map(|matched| matched.relative_path)
        .ok_or_else(|| HistoryError::MountNotFound(absolute_path.to_path_buf()))
}

fn status_parts(status: JournalStatus) -> (String, Option<String>) {
    match status {
        JournalStatus::Prepared => ("prepared".to_string(), None),
        JournalStatus::Applying => ("applying".to_string(), None),
        JournalStatus::Applied => ("applied".to_string(), None),
        JournalStatus::Reconciled => ("reconciled".to_string(), None),
        JournalStatus::Reverted => ("reverted".to_string(), None),
        JournalStatus::Failed(message) => ("failed".to_string(), Some(message)),
    }
}

fn status_name(status: &JournalStatus) -> &'static str {
    match status {
        JournalStatus::Prepared => "prepared",
        JournalStatus::Applying => "applying",
        JournalStatus::Applied => "applied",
        JournalStatus::Reconciled => "reconciled",
        JournalStatus::Reverted => "reverted",
        JournalStatus::Failed(_) => "failed",
    }
}

fn undo_boundary_message(status: &JournalStatus) -> &'static str {
    match status {
        JournalStatus::Applying => {
            "journal is currently applying; wait for it to finish before undoing"
        }
        JournalStatus::Failed(_) => {
            "failed journals may have partial remote effects; remote undo requires pre-push snapshots"
        }
        JournalStatus::Applied | JournalStatus::Reconciled => {
            "remote undo requires connector reverse-apply support"
        }
        JournalStatus::Prepared | JournalStatus::Reverted => {
            "journal entry does not need remote undo"
        }
    }
}

fn undo_boundary(plan: &UndoPlan) -> (&'static str, &'static str) {
    match plan.status {
        UndoPlanStatus::Complete => (
            "reverse_apply_not_implemented",
            "reverse apply is not implemented yet",
        ),
        UndoPlanStatus::Partial => (
            "undo_plan_partial",
            "undo plan is partial; some operations cannot be reversed safely",
        ),
        UndoPlanStatus::Blocked => (
            "undo_plan_blocked",
            "no reversible operations can be derived from the journal preimages",
        ),
    }
}

fn undo_plan_status_name(status: &UndoPlanStatus) -> &'static str {
    match status {
        UndoPlanStatus::Complete => "complete",
        UndoPlanStatus::Partial => "partial",
        UndoPlanStatus::Blocked => "blocked",
    }
}
