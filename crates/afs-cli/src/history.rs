//! Journal-backed `afs log` and `afs undo` orchestration.
//!
//! The log surface is a read-only view over durable push journals. The initial
//! undo surface only cancels a journal entry that is still `prepared`, because
//! current journal entries do not yet contain the pre-push remote state required
//! to safely reverse already-applied connector mutations.

use std::path::{Path, PathBuf};

use afs_core::journal::{JournalEntry, JournalStatus, PushId};
use afs_core::model::{MountId, RemoteId};
use afs_store::{EntityRepository, JournalRepository, MountConfig, MountRepository, StoreError};
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
    pub entry: Option<JournalEntryOutput>,
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
                entry: Some(JournalEntryOutput::from(reverted)),
            })
        }
        JournalStatus::Reverted => Ok(UndoReport {
            ok: true,
            command: "undo",
            push_id: push_id.0,
            status: "reverted".to_string(),
            action: "already_reverted".to_string(),
            message: "journal entry was already reverted".to_string(),
            entry: Some(JournalEntryOutput::from(entry)),
        }),
        status => Ok(UndoReport {
            ok: false,
            command: "undo",
            push_id: push_id.0,
            status: status_name(&status).to_string(),
            action: "undo_not_implemented".to_string(),
            message: undo_boundary_message(&status).to_string(),
            entry: Some(JournalEntryOutput::from(entry)),
        }),
    }
}

pub fn undo_report_exit_code(report: &UndoReport) -> i32 {
    if report.ok { 0 } else { 5 }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct JournalEntryOutput {
    pub push_id: String,
    pub mount_id: String,
    pub remote_ids: Vec<String>,
    pub status: String,
    pub failure: Option<String>,
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
            plan_summary: PlanSummaryOutput::from(value.plan.summary),
            operation_count,
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
    mounts
        .iter()
        .filter(|mount| path.starts_with(&mount.root))
        .max_by_key(|mount| mount.root.components().count())
}

fn relative_entity_path(
    mount: &MountConfig,
    absolute_path: &Path,
) -> Result<PathBuf, HistoryError> {
    absolute_path
        .strip_prefix(&mount.root)
        .map(Path::to_path_buf)
        .map_err(|_| HistoryError::MountNotFound(absolute_path.to_path_buf()))
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
        JournalStatus::Applying | JournalStatus::Applied | JournalStatus::Reconciled => {
            "remote undo requires pre-push snapshots and connector reverse-apply support"
        }
        JournalStatus::Failed(_) => {
            "failed journals may have partial remote effects; remote undo requires pre-push snapshots"
        }
        JournalStatus::Prepared | JournalStatus::Reverted => {
            "journal entry does not need remote undo"
        }
    }
}
