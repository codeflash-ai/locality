//! `afs info` orchestration.
//!
//! Info is a local read-only view over mount metadata. It explains what a path
//! means in the projected workspace without hydrating files or calling a remote
//! connector.

use std::path::{Path, PathBuf};

use afs_core::journal::{JournalEntry, JournalStatus};
use afs_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use afs_core::path_projection::{page_container_path, page_listing_parent_path};
use afs_store::{
    EntityRecord, EntityRepository, JournalRepository, MountConfig, MountRepository, StoreError,
};
use afsd::{file_provider, source::source_display_name};
use serde::Serialize;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InfoOptions {
    pub path: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InfoReport {
    pub ok: bool,
    pub command: &'static str,
    pub target: String,
    pub mount: InfoMount,
    pub subject: InfoSubject,
    pub children: InfoChildSummary,
    pub journals: InfoJournalSummary,
    pub suggestions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InfoMount {
    pub mount_id: String,
    pub connector: String,
    pub root: String,
    pub remote_root_id: Option<String>,
    pub read_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InfoSubject {
    pub role: InfoRole,
    pub source: String,
    pub path: String,
    pub absolute_path: String,
    pub exists: bool,
    pub entity: Option<InfoEntity>,
    pub schema_path: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InfoRole {
    MountRoot,
    PageFile,
    PageWorkspace,
    DatabaseDirectory,
    ProjectedDirectory,
    Asset,
    UnknownEntity,
    UntrackedPath,
}

impl InfoRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::MountRoot => "mount root",
            Self::PageFile => "projected page.md file",
            Self::PageWorkspace => "child workspace for a projected page",
            Self::DatabaseDirectory => "projected database directory",
            Self::ProjectedDirectory => "projected directory",
            Self::Asset => "projected asset",
            Self::UnknownEntity => "projected entity",
            Self::UntrackedPath => "untracked path inside mount",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InfoEntity {
    pub entity_id: String,
    pub kind: String,
    pub title: String,
    pub path: String,
    pub absolute_path: String,
    pub hydration: String,
    pub content_hash: Option<String>,
    pub remote_edited_at: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct InfoChildSummary {
    pub immediate: usize,
    pub subtree: usize,
    pub pages: usize,
    pub databases: usize,
    pub directories: usize,
    pub assets: usize,
    pub unknown: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct InfoJournalSummary {
    pub pending: usize,
    pub failed: usize,
}

pub fn run_info<S>(store: &S, options: InfoOptions) -> Result<InfoReport, InfoError>
where
    S: MountRepository + EntityRepository + JournalRepository,
{
    let target = match options.path {
        Some(path) => absolute_path(&path)?,
        None => {
            std::env::current_dir().map_err(|error| InfoError::CurrentDir(error.to_string()))?
        }
    };
    let mounts = store.load_mounts().map_err(InfoError::Store)?;
    let mount = find_mount_for_path(&mounts, &target)
        .cloned()
        .ok_or_else(|| InfoError::MountNotFound(target.clone()))?;
    let relative_path = relative_entity_path(&mount, &target)?;
    let entities = store
        .list_entities(&mount.mount_id)
        .map_err(InfoError::Store)?;
    let journals = store.list_journal().map_err(InfoError::Store)?;
    let subject_context = resolve_subject(&mount, &relative_path, &target, &entities);
    let children = child_summary(&entities, &subject_context.child_context);
    let journal_scope = journal_scope_entities(&entities, &subject_context);
    let journals = journal_summary(&journals, &mount.mount_id, &journal_scope);
    let suggestions = suggestions(&subject_context);

    Ok(InfoReport {
        ok: true,
        command: "info",
        target: target.display().to_string(),
        mount: InfoMount {
            mount_id: mount.mount_id.0.clone(),
            connector: mount.connector.clone(),
            root: mount.root.display().to_string(),
            remote_root_id: mount
                .remote_root_id
                .as_ref()
                .map(|remote_id| remote_id.0.clone()),
            read_only: mount.read_only,
        },
        subject: InfoSubject {
            role: subject_context.role,
            source: source_name(&mount.connector, subject_context.entity.as_ref()),
            path: relative_path.display().to_string(),
            absolute_path: target.display().to_string(),
            exists: target.exists(),
            entity: subject_context
                .entity
                .as_ref()
                .map(|entity| info_entity(&mount, entity)),
            schema_path: subject_context
                .schema_path
                .map(|path| path.display().to_string()),
        },
        children,
        journals,
        suggestions,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InfoError {
    CurrentDir(String),
    MountNotFound(PathBuf),
    Store(StoreError),
}

impl InfoError {
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
struct SubjectContext {
    role: InfoRole,
    entity: Option<EntityRecord>,
    child_context: PathBuf,
    schema_path: Option<PathBuf>,
}

fn resolve_subject(
    mount: &MountConfig,
    relative_path: &Path,
    _absolute_path: &Path,
    entities: &[EntityRecord],
) -> SubjectContext {
    if relative_path.as_os_str().is_empty() {
        return SubjectContext {
            role: InfoRole::MountRoot,
            entity: None,
            child_context: PathBuf::new(),
            schema_path: None,
        };
    }

    if let Some(entity) = entities.iter().find(|entity| entity.path == relative_path) {
        let role = role_for_exact_entity(&entity.kind);
        let child_context = child_context_for_entity(entity);
        let schema_path = if matches!(entity.kind, EntityKind::Database) {
            Some(mount.root.join(&entity.path).join("_schema.yaml"))
        } else {
            None
        };

        return SubjectContext {
            role,
            entity: Some(entity.clone()),
            child_context,
            schema_path,
        };
    }

    if let Some(entity) = nearest_page_workspace(relative_path, entities) {
        return SubjectContext {
            role: InfoRole::PageWorkspace,
            entity: Some(entity.clone()),
            child_context: relative_path.to_path_buf(),
            schema_path: None,
        };
    }

    if let Some(entity) = nearest_directory_entity(relative_path, entities) {
        let role = role_for_exact_entity(&entity.kind);
        let schema_path = if matches!(entity.kind, EntityKind::Database) {
            Some(mount.root.join(&entity.path).join("_schema.yaml"))
        } else {
            None
        };

        return SubjectContext {
            role,
            entity: Some(entity.clone()),
            child_context: relative_path.to_path_buf(),
            schema_path,
        };
    }

    SubjectContext {
        role: InfoRole::UntrackedPath,
        entity: None,
        child_context: relative_path.to_path_buf(),
        schema_path: None,
    }
}

fn role_for_exact_entity(kind: &EntityKind) -> InfoRole {
    match kind {
        EntityKind::Page => InfoRole::PageFile,
        EntityKind::Database => InfoRole::DatabaseDirectory,
        EntityKind::Directory => InfoRole::ProjectedDirectory,
        EntityKind::Asset => InfoRole::Asset,
        EntityKind::Unknown(_) => InfoRole::UnknownEntity,
    }
}

fn child_context_for_entity(entity: &EntityRecord) -> PathBuf {
    match entity.kind {
        EntityKind::Page => page_container_path(&entity.path),
        EntityKind::Database | EntityKind::Directory => entity.path.clone(),
        EntityKind::Asset | EntityKind::Unknown(_) => entity
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default(),
    }
}

fn nearest_page_workspace<'a>(
    relative_path: &Path,
    entities: &'a [EntityRecord],
) -> Option<&'a EntityRecord> {
    entities
        .iter()
        .filter(|entity| matches!(entity.kind, EntityKind::Page))
        .filter(|entity| relative_path.starts_with(page_container_path(&entity.path)))
        .max_by_key(|entity| entity.path.components().count())
}

fn nearest_directory_entity<'a>(
    relative_path: &Path,
    entities: &'a [EntityRecord],
) -> Option<&'a EntityRecord> {
    entities
        .iter()
        .filter(|entity| matches!(entity.kind, EntityKind::Database | EntityKind::Directory))
        .filter(|entity| relative_path.starts_with(&entity.path))
        .max_by_key(|entity| entity.path.components().count())
}

fn child_summary(entities: &[EntityRecord], context: &Path) -> InfoChildSummary {
    let mut summary = InfoChildSummary {
        subtree: subtree_count(entities, context),
        ..InfoChildSummary::default()
    };

    for entity in entities {
        if entity_listing_parent_path(entity) == context {
            summary.immediate += 1;
            match entity.kind {
                EntityKind::Page => summary.pages += 1,
                EntityKind::Database => summary.databases += 1,
                EntityKind::Directory => summary.directories += 1,
                EntityKind::Asset => summary.assets += 1,
                EntityKind::Unknown(_) => summary.unknown += 1,
            }
        }
    }

    summary
}

fn entity_listing_parent_path(entity: &EntityRecord) -> PathBuf {
    match entity.kind {
        EntityKind::Page => page_listing_parent_path(&entity.path),
        EntityKind::Database
        | EntityKind::Directory
        | EntityKind::Asset
        | EntityKind::Unknown(_) => entity
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default(),
    }
}

fn subtree_count(entities: &[EntityRecord], context: &Path) -> usize {
    if context.as_os_str().is_empty() {
        return entities.len();
    }

    entities
        .iter()
        .filter(|entity| {
            if entity.kind == EntityKind::Page && page_container_path(&entity.path) == context {
                return false;
            }
            entity.path != context && entity.path.starts_with(context)
        })
        .count()
}

fn journal_scope_entities(
    entities: &[EntityRecord],
    subject_context: &SubjectContext,
) -> Vec<RemoteId> {
    let mut remote_ids = Vec::new();
    if let Some(entity) = &subject_context.entity {
        remote_ids.push(entity.remote_id.clone());
    }

    for entity in entities {
        if entity.path.starts_with(&subject_context.child_context)
            && !remote_ids
                .iter()
                .any(|remote_id| remote_id == &entity.remote_id)
        {
            remote_ids.push(entity.remote_id.clone());
        }
    }

    remote_ids
}

fn journal_summary(
    journals: &[JournalEntry],
    mount_id: &MountId,
    remote_ids: &[RemoteId],
) -> InfoJournalSummary {
    let mut summary = InfoJournalSummary::default();

    for journal in journals {
        if journal.mount_id != *mount_id {
            continue;
        }
        if !remote_ids.iter().any(|remote_id| {
            journal.remote_ids.iter().any(|id| id == remote_id)
                || journal
                    .plan
                    .affected_entities
                    .iter()
                    .any(|id| id == remote_id)
        }) {
            continue;
        }

        match journal.status {
            JournalStatus::Prepared | JournalStatus::Applying | JournalStatus::Applied => {
                summary.pending += 1;
            }
            JournalStatus::Failed(_) => summary.failed += 1,
            JournalStatus::Reconciled | JournalStatus::Reverted => {}
        }
    }

    summary
}

fn info_entity(mount: &MountConfig, entity: &EntityRecord) -> InfoEntity {
    InfoEntity {
        entity_id: entity.remote_id.0.clone(),
        kind: entity_kind_name(&entity.kind).to_string(),
        title: entity.title.clone(),
        path: entity.path.display().to_string(),
        absolute_path: mount.root.join(&entity.path).display().to_string(),
        hydration: hydration_name(&entity.hydration).to_string(),
        content_hash: entity.content_hash.clone(),
        remote_edited_at: entity.remote_edited_at.clone(),
    }
}

fn suggestions(subject_context: &SubjectContext) -> Vec<String> {
    match subject_context.role {
        InfoRole::MountRoot | InfoRole::PageWorkspace | InfoRole::DatabaseDirectory => vec![
            "afs status .".to_string(),
            "afs pull <path-to-stub>".to_string(),
            "afs diff <path-before-push>".to_string(),
        ],
        InfoRole::PageFile => vec![
            "afs status <path>".to_string(),
            "afs pull <path>".to_string(),
            "afs diff <path>".to_string(),
        ],
        InfoRole::UntrackedPath => vec!["afs status .".to_string()],
        InfoRole::ProjectedDirectory | InfoRole::Asset | InfoRole::UnknownEntity => {
            vec!["afs status <path>".to_string()]
        }
    }
}

fn source_name(connector: &str, entity: Option<&EntityRecord>) -> String {
    match entity {
        Some(entity) => format!(
            "{} {}",
            connector_label(connector),
            entity_kind_name(&entity.kind)
        ),
        None => format!("{} mount", connector_label(connector)),
    }
}

fn connector_label(connector: &str) -> String {
    source_display_name(connector)
}

fn absolute_path(path: &Path) -> Result<PathBuf, InfoError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| InfoError::CurrentDir(error.to_string()))
    }
}

fn find_mount_for_path<'a>(mounts: &'a [MountConfig], path: &Path) -> Option<&'a MountConfig> {
    file_provider::find_mount_for_path(mounts, path).map(|(mount, _)| mount)
}

fn relative_entity_path(mount: &MountConfig, absolute_path: &Path) -> Result<PathBuf, InfoError> {
    file_provider::match_mount_path(mount, absolute_path)
        .map(|matched| matched.relative_path)
        .ok_or_else(|| InfoError::MountNotFound(absolute_path.to_path_buf()))
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
