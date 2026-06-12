//! `afs status` orchestration.
//!
//! Status is a local read-only view over mounted projections. It classifies the
//! filesystem state for stored entities without calling remote connectors.

use std::path::{Path, PathBuf};

use afs_core::canonical::parse_canonical_markdown;
use afs_core::conflict::unresolved_conflict_marker_line;
use afs_core::journal::{JournalEntry, JournalStatus};
use afs_core::model::{CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId};
use afs_store::{
    EntityRecord, EntityRepository, JournalRepository, MountConfig, MountRepository,
    ShadowRepository, StoreError, VirtualMutationKind, VirtualMutationRecord,
    VirtualMutationRepository,
};
use afsd::virtual_fs::virtual_fs_content_path;
use serde::Serialize;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusOptions {
    pub path: Option<PathBuf>,
    pub state_root: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StatusReport {
    pub ok: bool,
    pub clean: bool,
    pub command: &'static str,
    pub target: Option<String>,
    pub summary: StatusSummary,
    pub mounts: Vec<StatusMountReport>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct StatusSummary {
    pub total: usize,
    pub clean: usize,
    pub stub: usize,
    pub dirty: usize,
    pub conflicted: usize,
    pub missing: usize,
    pub error: usize,
    pub pending_journals: usize,
    pub failed_journals: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StatusMountReport {
    pub mount_id: String,
    pub connector: String,
    pub root: String,
    pub entries: Vec<StatusEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StatusEntry {
    pub path: String,
    pub absolute_path: String,
    pub entity_id: String,
    pub kind: String,
    pub title: String,
    pub hydration: String,
    pub state: StatusState,
    pub issues: Vec<StatusIssue>,
    pub pending_journal_count: usize,
    pub failed_journal_count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusState {
    Clean,
    Stub,
    Dirty,
    Conflicted,
    Missing,
    Error,
}

impl StatusState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Stub => "stub",
            Self::Dirty => "dirty",
            Self::Conflicted => "conflicted",
            Self::Missing => "missing",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StatusIssue {
    pub code: String,
    pub message: String,
}

pub fn run_status<S>(store: &S, options: StatusOptions) -> Result<StatusReport, StatusError>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + JournalRepository
        + VirtualMutationRepository,
{
    let mounts = store.load_mounts().map_err(StatusError::Store)?;
    let target = options.path.as_deref().map(absolute_path).transpose()?;
    let state_root = options.state_root.unwrap_or_else(default_state_root);
    let scopes = resolve_scopes(store, &mounts, target.as_deref())?;
    let journals = store.list_journal().map_err(StatusError::Store)?;
    let mut summary = StatusSummary::default();
    let mut mount_reports = Vec::new();

    for scope in scopes {
        let mutations = scoped_virtual_mutations(store, &scope)?;
        let deleted = mutations
            .iter()
            .filter(|mutation| mutation.mutation_kind == VirtualMutationKind::Delete)
            .filter_map(|mutation| mutation.target_remote_id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let mut entries = scoped_entities(store, &scope)?
            .into_iter()
            .filter(|entity| !deleted.contains(&entity.remote_id))
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.path.cmp(&right.path));

        let mut status_entries = entries
            .into_iter()
            .map(|entity| classify_entity(store, &scope.mount, entity, &journals, &state_root))
            .collect::<Vec<_>>();
        status_entries.extend(
            mutations
                .into_iter()
                .filter(|mutation| mutation.mutation_kind != VirtualMutationKind::Rename)
                .map(|mutation| classify_virtual_mutation(&scope.mount, mutation, &state_root)),
        );
        status_entries.sort_by(|left, right| left.path.cmp(&right.path));

        for entry in &status_entries {
            summary.record(entry);
        }

        mount_reports.push(StatusMountReport {
            mount_id: scope.mount.mount_id.0.clone(),
            connector: scope.mount.connector.clone(),
            root: scope.mount.root.display().to_string(),
            entries: status_entries,
        });
    }

    let clean = summary.dirty == 0
        && summary.conflicted == 0
        && summary.missing == 0
        && summary.error == 0
        && summary.stub == 0
        && summary.pending_journals == 0
        && summary.failed_journals == 0;

    Ok(StatusReport {
        ok: true,
        clean,
        command: "status",
        target: target.map(|path| path.display().to_string()),
        summary,
        mounts: mount_reports,
    })
}

impl StatusSummary {
    fn record(&mut self, entry: &StatusEntry) {
        self.total += 1;
        self.pending_journals += entry.pending_journal_count;
        self.failed_journals += entry.failed_journal_count;

        match entry.state {
            StatusState::Clean => self.clean += 1,
            StatusState::Stub => self.stub += 1,
            StatusState::Dirty => self.dirty += 1,
            StatusState::Conflicted => self.conflicted += 1,
            StatusState::Missing => self.missing += 1,
            StatusState::Error => self.error += 1,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatusError {
    CurrentDir(String),
    MountNotFound(PathBuf),
    Store(StoreError),
}

impl StatusError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::CurrentDir(_) => "current_dir_failed",
            Self::MountNotFound(_) => "mount_not_found",
            Self::Store(StoreError::EntityPathMissing { .. }) => "entity_path_missing",
            Self::Store(_) => "store_error",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::CurrentDir(message) => format!("failed to resolve current directory: {message}"),
            Self::MountNotFound(path) => {
                format!("no AgentFS mount contains `{}`", path.display())
            }
            Self::Store(error) => error.to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StatusScope {
    mount: MountConfig,
    filter: ScopeFilter,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ScopeFilter {
    All,
    Exact(PathBuf),
    Subtree(PathBuf),
}

fn resolve_scopes<S>(
    store: &S,
    mounts: &[MountConfig],
    target: Option<&Path>,
) -> Result<Vec<StatusScope>, StatusError>
where
    S: EntityRepository + VirtualMutationRepository,
{
    match target {
        Some(target) => resolve_target_scope(store, mounts, target).map(|scope| vec![scope]),
        None => resolve_default_scopes(store, mounts),
    }
}

fn resolve_default_scopes<S>(
    store: &S,
    mounts: &[MountConfig],
) -> Result<Vec<StatusScope>, StatusError>
where
    S: EntityRepository + VirtualMutationRepository,
{
    if mounts.is_empty() {
        return Ok(Vec::new());
    }

    let cwd =
        std::env::current_dir().map_err(|error| StatusError::CurrentDir(error.to_string()))?;
    if let Some(mount) = find_mount_for_path(mounts, &cwd) {
        let relative_path = relative_entity_path(mount, &cwd)?;
        let filter = scope_filter_for_relative_path(store, mount, &relative_path)?;

        return Ok(vec![StatusScope {
            mount: mount.clone(),
            filter,
        }]);
    }

    Ok(mounts
        .iter()
        .cloned()
        .map(|mount| StatusScope {
            mount,
            filter: ScopeFilter::All,
        })
        .collect())
}

fn resolve_target_scope<S>(
    store: &S,
    mounts: &[MountConfig],
    target: &Path,
) -> Result<StatusScope, StatusError>
where
    S: EntityRepository + VirtualMutationRepository,
{
    let mount = find_mount_for_path(mounts, target)
        .cloned()
        .ok_or_else(|| StatusError::MountNotFound(target.to_path_buf()))?;
    let relative_path = relative_entity_path(&mount, target)?;
    let filter = scope_filter_for_relative_path(store, &mount, &relative_path)?;

    Ok(StatusScope { mount, filter })
}

fn scope_filter_for_relative_path<S>(
    store: &S,
    mount: &MountConfig,
    relative_path: &Path,
) -> Result<ScopeFilter, StatusError>
where
    S: EntityRepository + VirtualMutationRepository,
{
    if relative_path.as_os_str().is_empty() {
        Ok(ScopeFilter::All)
    } else {
        target_filter(store, mount, relative_path)
    }
}

fn target_filter<S>(
    store: &S,
    mount: &MountConfig,
    relative_path: &Path,
) -> Result<ScopeFilter, StatusError>
where
    S: EntityRepository + VirtualMutationRepository,
{
    let exact = store
        .find_entity_by_path(&mount.mount_id, relative_path)
        .map_err(StatusError::Store)?;
    if let Some(entity) = exact {
        if matches!(entity.kind, EntityKind::Database | EntityKind::Directory) {
            return Ok(ScopeFilter::Subtree(relative_path.to_path_buf()));
        }

        return Ok(ScopeFilter::Exact(relative_path.to_path_buf()));
    }

    if store
        .find_virtual_mutation_by_path(&mount.mount_id, relative_path)
        .map_err(StatusError::Store)?
        .is_some()
    {
        return Ok(ScopeFilter::Exact(relative_path.to_path_buf()));
    }

    let has_children = store
        .list_entities(&mount.mount_id)
        .map_err(StatusError::Store)?
        .iter()
        .any(|entity| entity.path.starts_with(relative_path));
    let has_pending_children = store
        .list_virtual_mutations(&mount.mount_id)
        .map_err(StatusError::Store)?
        .iter()
        .any(|mutation| mutation.projected_path.starts_with(relative_path));

    if has_children || has_pending_children {
        Ok(ScopeFilter::Subtree(relative_path.to_path_buf()))
    } else {
        Err(StatusError::Store(StoreError::EntityPathMissing {
            mount_id: mount.mount_id.clone(),
            path: relative_path.to_path_buf(),
        }))
    }
}

fn scoped_entities<S>(store: &S, scope: &StatusScope) -> Result<Vec<EntityRecord>, StatusError>
where
    S: EntityRepository,
{
    let entities = store
        .list_entities(&scope.mount.mount_id)
        .map_err(StatusError::Store)?;
    let filtered = entities
        .into_iter()
        .filter(|entity| match &scope.filter {
            ScopeFilter::All => true,
            ScopeFilter::Exact(path) => &entity.path == path,
            ScopeFilter::Subtree(path) => entity.path.starts_with(path),
        })
        .collect::<Vec<_>>();

    Ok(filtered)
}

fn scoped_virtual_mutations<S>(
    store: &S,
    scope: &StatusScope,
) -> Result<Vec<VirtualMutationRecord>, StatusError>
where
    S: VirtualMutationRepository,
{
    Ok(store
        .list_virtual_mutations(&scope.mount.mount_id)
        .map_err(StatusError::Store)?
        .into_iter()
        .filter(|mutation| match &scope.filter {
            ScopeFilter::All => true,
            ScopeFilter::Exact(path) => &mutation.projected_path == path,
            ScopeFilter::Subtree(path) => mutation.projected_path.starts_with(path),
        })
        .collect())
}

fn classify_virtual_mutation(
    mount: &MountConfig,
    mutation: VirtualMutationRecord,
    _state_root: &Path,
) -> StatusEntry {
    let (code, message) = match mutation.mutation_kind {
        VirtualMutationKind::Create => {
            ("pending_virtual_create", "file is pending remote creation")
        }
        VirtualMutationKind::Rename => (
            "pending_virtual_rename",
            "file rename is pending remote update",
        ),
        VirtualMutationKind::Delete => ("pending_virtual_delete", "file is pending remote archive"),
    };
    let entity_id = mutation
        .target_remote_id
        .as_ref()
        .map(|remote_id| remote_id.0.clone())
        .unwrap_or_else(|| mutation.local_id.clone());

    StatusEntry {
        absolute_path: mount
            .root
            .join(&mutation.projected_path)
            .display()
            .to_string(),
        path: mutation.projected_path.display().to_string(),
        entity_id,
        kind: "page".to_string(),
        title: mutation.title,
        hydration: "dirty".to_string(),
        state: StatusState::Dirty,
        issues: vec![StatusIssue::new(code, message)],
        pending_journal_count: 0,
        failed_journal_count: 0,
    }
}

fn classify_entity<S>(
    store: &S,
    mount: &MountConfig,
    entity: EntityRecord,
    journals: &[JournalEntry],
    state_root: &Path,
) -> StatusEntry
where
    S: ShadowRepository + JournalRepository,
{
    let absolute_path = mount.root.join(&entity.path);
    let mut issues = Vec::new();
    let (state, mut state_issues) =
        classify_local_state(store, mount, &entity, &absolute_path, state_root);
    issues.append(&mut state_issues);

    let (pending_journal_count, failed_journal_count) =
        journal_counts(journals, &mount.mount_id, &entity.remote_id);
    if pending_journal_count > 0 {
        issues.push(StatusIssue::new(
            "pending_journal",
            format!("{pending_journal_count} push journal(s) are not reconciled"),
        ));
    }
    if failed_journal_count > 0 {
        issues.push(StatusIssue::new(
            "failed_journal",
            format!("{failed_journal_count} push journal(s) failed"),
        ));
        if let Ok(Some(message)) =
            store.latest_failed_journal_for_entity(&mount.mount_id, &entity.remote_id)
        {
            issues.push(StatusIssue::new("last_failure", message));
        }
    }

    StatusEntry {
        path: entity.path.display().to_string(),
        absolute_path: absolute_path.display().to_string(),
        entity_id: entity.remote_id.0,
        kind: entity_kind_name(&entity.kind).to_string(),
        title: entity.title,
        hydration: hydration_name(&entity.hydration).to_string(),
        state,
        issues,
        pending_journal_count,
        failed_journal_count,
    }
}

fn classify_local_state<S>(
    store: &S,
    mount: &MountConfig,
    entity: &EntityRecord,
    absolute_path: &Path,
    state_root: &Path,
) -> (StatusState, Vec<StatusIssue>)
where
    S: ShadowRepository,
{
    if mount.projection.uses_virtual_filesystem() {
        return classify_virtual_state(store, mount, entity, state_root);
    }

    if !absolute_path.exists() {
        return (
            StatusState::Missing,
            vec![StatusIssue::new(
                "local_projection_missing",
                "local projected path is missing",
            )],
        );
    }

    match entity.hydration {
        HydrationState::Conflicted if entity.kind == EntityKind::Page => {
            return classify_conflicted_page_state(store, mount, entity, absolute_path);
        }
        HydrationState::Dirty if entity.kind == EntityKind::Page => {
            return classify_dirty_page_state(store, mount, entity, absolute_path);
        }
        HydrationState::Dirty => {
            return (
                StatusState::Dirty,
                vec![StatusIssue::new("entity_dirty", "entity is marked dirty")],
            );
        }
        _ => {}
    }

    match entity.kind {
        EntityKind::Page => classify_page_state(store, mount, entity, absolute_path),
        EntityKind::Database | EntityKind::Directory => {
            if absolute_path.is_dir() {
                hydration_state_without_file_read(&entity.hydration)
            } else {
                (
                    StatusState::Missing,
                    vec![StatusIssue::new(
                        "local_projection_not_directory",
                        "projected directory path is not a directory",
                    )],
                )
            }
        }
        EntityKind::Asset | EntityKind::Unknown(_) => {
            hydration_state_without_file_read(&entity.hydration)
        }
    }
}

fn classify_virtual_state<S>(
    store: &S,
    mount: &MountConfig,
    entity: &EntityRecord,
    state_root: &Path,
) -> (StatusState, Vec<StatusIssue>)
where
    S: ShadowRepository,
{
    match entity.hydration {
        HydrationState::Conflicted if entity.kind == EntityKind::Page => {
            return classify_conflicted_virtual_page_state(store, mount, entity, state_root);
        }
        HydrationState::Dirty if entity.kind == EntityKind::Page => {
            return classify_dirty_virtual_page_state(store, mount, entity, state_root);
        }
        HydrationState::Dirty => {
            return (
                StatusState::Dirty,
                vec![StatusIssue::new("entity_dirty", "entity is marked dirty")],
            );
        }
        _ => {}
    }

    match entity.kind {
        EntityKind::Page => classify_virtual_page_state(store, mount, entity, state_root),
        EntityKind::Database
        | EntityKind::Directory
        | EntityKind::Asset
        | EntityKind::Unknown(_) => hydration_state_without_file_read(&entity.hydration),
    }
}

fn classify_virtual_page_state<S>(
    store: &S,
    mount: &MountConfig,
    entity: &EntityRecord,
    state_root: &Path,
) -> (StatusState, Vec<StatusIssue>)
where
    S: ShadowRepository,
{
    if matches!(
        entity.hydration,
        HydrationState::Virtual | HydrationState::Stub
    ) {
        return (StatusState::Stub, Vec::new());
    }

    let content_path = match virtual_fs_content_path(state_root, &mount.mount_id, &entity.path) {
        Ok(path) => path,
        Err(error) => {
            return (
                StatusState::Error,
                vec![StatusIssue::new(
                    "content_cache_path_invalid",
                    format!("invalid virtual content path: {error}"),
                )],
            );
        }
    };
    if !content_path.exists() {
        return (
            StatusState::Missing,
            vec![StatusIssue::new(
                "content_cache_missing",
                "daemon content cache path is missing",
            )],
        );
    }

    let contents = match std::fs::read_to_string(&content_path) {
        Ok(contents) => contents,
        Err(error) => {
            return (
                StatusState::Error,
                vec![StatusIssue::new(
                    "read_content_cache_failed",
                    format!("failed to read daemon content cache: {error}"),
                )],
            );
        }
    };

    classify_page_contents(store, mount, entity, &contents)
}

fn classify_page_state<S>(
    store: &S,
    mount: &MountConfig,
    entity: &EntityRecord,
    absolute_path: &Path,
) -> (StatusState, Vec<StatusIssue>)
where
    S: ShadowRepository,
{
    let contents = match std::fs::read_to_string(absolute_path) {
        Ok(contents) => contents,
        Err(error) => {
            return (
                StatusState::Error,
                vec![StatusIssue::new(
                    "read_file_failed",
                    format!("failed to read local file: {error}"),
                )],
            );
        }
    };

    classify_page_contents(store, mount, entity, &contents)
}

fn classify_dirty_page_state<S>(
    store: &S,
    mount: &MountConfig,
    entity: &EntityRecord,
    absolute_path: &Path,
) -> (StatusState, Vec<StatusIssue>)
where
    S: ShadowRepository,
{
    let (state, issues) = classify_page_state(store, mount, entity, absolute_path);
    dirty_state_with_entity_issue(state, issues)
}

fn classify_conflicted_page_state<S>(
    store: &S,
    mount: &MountConfig,
    entity: &EntityRecord,
    absolute_path: &Path,
) -> (StatusState, Vec<StatusIssue>)
where
    S: ShadowRepository,
{
    let (state, issues) = classify_page_state(store, mount, entity, absolute_path);
    conflicted_state_with_entity_issue(state, issues)
}

fn classify_dirty_virtual_page_state<S>(
    store: &S,
    mount: &MountConfig,
    entity: &EntityRecord,
    state_root: &Path,
) -> (StatusState, Vec<StatusIssue>)
where
    S: ShadowRepository,
{
    let (state, issues) = classify_virtual_page_state(store, mount, entity, state_root);
    dirty_state_with_entity_issue(state, issues)
}

fn classify_conflicted_virtual_page_state<S>(
    store: &S,
    mount: &MountConfig,
    entity: &EntityRecord,
    state_root: &Path,
) -> (StatusState, Vec<StatusIssue>)
where
    S: ShadowRepository,
{
    let (state, issues) = classify_virtual_page_state(store, mount, entity, state_root);
    conflicted_state_with_entity_issue(state, issues)
}

fn dirty_state_with_entity_issue(
    state: StatusState,
    mut issues: Vec<StatusIssue>,
) -> (StatusState, Vec<StatusIssue>) {
    match state {
        StatusState::Clean | StatusState::Dirty => {
            if !issues.iter().any(|issue| issue.code == "entity_dirty") {
                issues.insert(
                    0,
                    StatusIssue::new("entity_dirty", "entity is marked dirty"),
                );
            }
            (StatusState::Dirty, issues)
        }
        StatusState::Stub | StatusState::Conflicted | StatusState::Missing | StatusState::Error => {
            (state, issues)
        }
    }
}

fn conflicted_state_with_entity_issue(
    state: StatusState,
    mut issues: Vec<StatusIssue>,
) -> (StatusState, Vec<StatusIssue>) {
    match state {
        StatusState::Conflicted => {
            if !issues.iter().any(|issue| issue.code == "entity_conflicted") {
                issues.insert(
                    0,
                    StatusIssue::new("entity_conflicted", "entity is marked conflicted"),
                );
            }
            (StatusState::Conflicted, issues)
        }
        StatusState::Clean | StatusState::Dirty => dirty_state_with_entity_issue(state, issues),
        StatusState::Stub | StatusState::Missing | StatusState::Error => (state, issues),
    }
}

fn classify_page_contents<S>(
    store: &S,
    mount: &MountConfig,
    entity: &EntityRecord,
    contents: &str,
) -> (StatusState, Vec<StatusIssue>)
where
    S: ShadowRepository,
{
    if let Some(line) = unresolved_conflict_marker_line(contents) {
        return (
            StatusState::Conflicted,
            vec![StatusIssue::new(
                "unresolved_conflict_markers",
                format!("file contains unresolved conflict markers starting at line {line}"),
            )],
        );
    }

    if matches!(
        entity.hydration,
        HydrationState::Virtual | HydrationState::Stub
    ) {
        return if contents.contains(CanonicalDocument::STUB_MARKER) {
            (StatusState::Stub, Vec::new())
        } else {
            (
                StatusState::Dirty,
                vec![StatusIssue::new(
                    "stub_content_changed",
                    "stub file has local content changes",
                )],
            )
        };
    }

    let parsed = match parse_canonical_markdown(contents) {
        Ok(parsed) => parsed,
        Err(error) => {
            return (
                StatusState::Dirty,
                vec![StatusIssue::new(
                    "canonical_parse_failed",
                    format!("canonical Markdown parse failed: {}", error.message),
                )],
            );
        }
    };

    if parsed
        .remote_id()
        .is_some_and(|remote_id| remote_id != &entity.remote_id)
    {
        return (
            StatusState::Dirty,
            vec![StatusIssue::new(
                "frontmatter_remote_id_mismatch",
                "frontmatter `afs.id` does not match the stored entity",
            )],
        );
    }

    let shadow = match store.get_shadow_record(&mount.mount_id, &entity.remote_id) {
        Ok(Some(shadow)) => shadow,
        Ok(None) => {
            return (
                StatusState::Error,
                vec![StatusIssue::new(
                    "shadow_missing",
                    "stored shadow snapshot is missing",
                )],
            );
        }
        Err(error) => {
            return (
                StatusState::Error,
                vec![StatusIssue::new(
                    "shadow_read_failed",
                    format!("failed to read stored shadow: {error}"),
                )],
            );
        }
    };

    if parsed.document.body == shadow.rendered_body {
        (StatusState::Clean, Vec::new())
    } else {
        (
            StatusState::Dirty,
            vec![StatusIssue::new(
                "local_body_changed",
                "local body differs from the last synced shadow",
            )],
        )
    }
}

fn hydration_state_without_file_read(
    hydration: &HydrationState,
) -> (StatusState, Vec<StatusIssue>) {
    match hydration {
        HydrationState::Hydrated => (StatusState::Clean, Vec::new()),
        HydrationState::Virtual | HydrationState::Stub => (StatusState::Stub, Vec::new()),
        HydrationState::Dirty => (
            StatusState::Dirty,
            vec![StatusIssue::new("entity_dirty", "entity is marked dirty")],
        ),
        HydrationState::Conflicted => (
            StatusState::Conflicted,
            vec![StatusIssue::new(
                "entity_conflicted",
                "entity is marked conflicted",
            )],
        ),
    }
}

fn journal_counts(
    journals: &[JournalEntry],
    mount_id: &MountId,
    remote_id: &RemoteId,
) -> (usize, usize) {
    let mut pending = 0;
    let mut failed = 0;

    for journal in journals {
        if !journal_matches_entity(journal, mount_id, remote_id) {
            continue;
        }

        match journal.status {
            JournalStatus::Prepared | JournalStatus::Applying | JournalStatus::Applied => {
                pending += 1;
            }
            JournalStatus::Failed(_) => failed += 1,
            JournalStatus::Reconciled | JournalStatus::Reverted => {}
        }
    }

    (pending, failed)
}

fn journal_matches_entity(
    journal: &JournalEntry,
    mount_id: &MountId,
    remote_id: &RemoteId,
) -> bool {
    journal.mount_id == *mount_id
        && (journal.remote_ids.iter().any(|id| id == remote_id)
            || journal
                .plan
                .affected_entities
                .iter()
                .any(|id| id == remote_id))
}

fn absolute_path(path: &Path) -> Result<PathBuf, StatusError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| StatusError::CurrentDir(error.to_string()))
    }
}

fn default_state_root() -> PathBuf {
    std::env::var("AFS_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|home| PathBuf::from(home).join(".afs")))
        .unwrap_or_else(|_| PathBuf::from(".afs"))
}

fn find_mount_for_path<'a>(mounts: &'a [MountConfig], path: &Path) -> Option<&'a MountConfig> {
    mounts
        .iter()
        .filter(|mount| path_starts_with_mount(path, mount))
        .max_by_key(|mount| mount.root.components().count())
}

fn path_starts_with_mount(path: &Path, mount: &MountConfig) -> bool {
    if path.starts_with(&mount.root) {
        return true;
    }

    let Some(canonical_path) = canonicalize_existing_prefix(path) else {
        return false;
    };
    let Ok(canonical_root) = std::fs::canonicalize(&mount.root) else {
        return false;
    };

    canonical_path.starts_with(canonical_root)
}

fn canonicalize_existing_prefix(path: &Path) -> Option<PathBuf> {
    let mut current = path;
    let mut suffix = PathBuf::new();

    loop {
        if let Ok(canonical_current) = std::fs::canonicalize(current) {
            return Some(canonical_current.join(suffix));
        }

        let file_name = current.file_name()?;
        suffix = PathBuf::from(file_name).join(suffix);
        current = current.parent()?;
    }
}

fn relative_entity_path(mount: &MountConfig, absolute_path: &Path) -> Result<PathBuf, StatusError> {
    if let Ok(relative_path) = absolute_path.strip_prefix(&mount.root) {
        return Ok(relative_path.to_path_buf());
    }

    let Some(canonical_path) = canonicalize_existing_prefix(absolute_path) else {
        return Err(StatusError::MountNotFound(absolute_path.to_path_buf()));
    };
    let canonical_root = std::fs::canonicalize(&mount.root)
        .map_err(|_| StatusError::MountNotFound(absolute_path.to_path_buf()))?;

    canonical_path
        .strip_prefix(canonical_root)
        .map(Path::to_path_buf)
        .map_err(|_| StatusError::MountNotFound(absolute_path.to_path_buf()))
}

fn entity_kind_name(kind: &EntityKind) -> &str {
    match kind {
        EntityKind::Page => "page",
        EntityKind::Database => "database",
        EntityKind::Directory => "directory",
        EntityKind::Asset => "asset",
        EntityKind::Unknown(value) => value.as_str(),
    }
}

fn hydration_name(hydration: &HydrationState) -> &'static str {
    match hydration {
        HydrationState::Virtual => "virtual",
        HydrationState::Stub => "stub",
        HydrationState::Hydrated => "hydrated",
        HydrationState::Dirty => "dirty",
        HydrationState::Conflicted => "conflicted",
    }
}

impl StatusIssue {
    fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}
