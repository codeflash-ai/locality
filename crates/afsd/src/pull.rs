//! `afs pull` orchestration.
//!
//! Pull is the read-side bridge between connector output, store state, and the
//! real file tree. Mount-root pulls enumerate the remote projection and write
//! stubs; page-file pulls hydrate one entity and persist its shadow snapshot.

use std::path::{Path, PathBuf};

use afs_connector::{Connector, EnumerateRequest, FetchRequest};
use afs_core::canonical::{parse_canonical_markdown, render_canonical_markdown};
use afs_core::model::{CanonicalDocument, EntityKind, HydrationState, TreeEntry};
use afs_core::shadow::ShadowDocument;
use afs_notion::NotionConnector;
use afs_store::{
    EntityRecord, EntityRepository, MountConfig, MountRepository, ProjectionMode, ShadowRepository,
    StoreError,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullReport {
    pub ok: bool,
    pub command: String,
    pub via: String,
    pub mount_id: String,
    pub root: String,
    pub target: String,
    pub enumerated: usize,
    pub stubbed: usize,
    pub hydrated: usize,
    pub skipped_dirty: usize,
}

pub fn run_pull<S>(
    store: &mut S,
    connector: &NotionConnector,
    target_path: impl AsRef<Path>,
) -> Result<PullReport, PullError>
where
    S: MountRepository + EntityRepository + ShadowRepository,
{
    let target_path = absolute_path(target_path.as_ref())?;
    let mounts = store.load_mounts().map_err(PullError::Store)?;
    let mount = find_mount_for_path(&mounts, &target_path)
        .cloned()
        .ok_or_else(|| PullError::MountNotFound(target_path.clone()))?;
    let relative_path = relative_target_path(&mount, &target_path)?;
    let mounted_connector = match &mount.remote_root_id {
        Some(root_page_id) => connector.with_root_page_id(root_page_id.clone()),
        None => connector.clone(),
    };

    if relative_path.as_os_str().is_empty() || target_path.is_dir() {
        pull_mount_root(store, &mounted_connector, &mount, target_path)
    } else {
        pull_entity_path(
            store,
            &mounted_connector,
            &mount,
            &relative_path,
            target_path,
        )
    }
}

fn pull_mount_root<S>(
    store: &mut S,
    connector: &NotionConnector,
    mount: &MountConfig,
    target_path: PathBuf,
) -> Result<PullReport, PullError>
where
    S: EntityRepository + ShadowRepository,
{
    let entries = connector
        .enumerate(EnumerateRequest {
            mount_id: mount.mount_id.clone(),
            cursor: None,
        })
        .map_err(PullError::Connector)?;
    let mut stubbed = 0;

    for entry in &entries {
        let record = merged_entity_record(store, entry)?;
        store.save_entity(record).map_err(PullError::Store)?;
        if write_stub_if_needed(connector, mount, entry)? {
            stubbed += 1;
        }
    }

    let mut hydrated = 0;
    let mut skipped_dirty = 0;
    if mount.remote_root_id.is_some()
        && let Some(root_entry) = entries.first()
    {
        let root_entity = store
            .get_entity(&mount.mount_id, &root_entry.remote_id)
            .map_err(PullError::Store)?
            .ok_or_else(|| {
                PullError::Store(StoreError::EntityMissing {
                    mount_id: mount.mount_id.clone(),
                    remote_id: root_entry.remote_id.clone(),
                })
            })?;
        match hydrate_entity(store, connector, mount, root_entity)? {
            HydrationOutcome::Hydrated => hydrated += 1,
            HydrationOutcome::SkippedDirty => skipped_dirty += 1,
        }
    }

    Ok(PullReport {
        ok: skipped_dirty == 0,
        command: "pull".to_string(),
        via: "cli".to_string(),
        mount_id: mount.mount_id.0.clone(),
        root: mount.root.display().to_string(),
        target: target_path.display().to_string(),
        enumerated: entries.len(),
        stubbed,
        hydrated,
        skipped_dirty,
    })
}

fn pull_entity_path<S>(
    store: &mut S,
    connector: &NotionConnector,
    mount: &MountConfig,
    relative_path: &Path,
    target_path: PathBuf,
) -> Result<PullReport, PullError>
where
    S: EntityRepository + ShadowRepository,
{
    let entity = store
        .find_entity_by_path(&mount.mount_id, relative_path)
        .map_err(PullError::Store)?
        .ok_or_else(|| {
            PullError::Store(StoreError::EntityPathMissing {
                mount_id: mount.mount_id.clone(),
                path: relative_path.to_path_buf(),
            })
        })?;

    let outcome = hydrate_entity(store, connector, mount, entity)?;
    let (hydrated, skipped_dirty) = match outcome {
        HydrationOutcome::Hydrated => (1, 0),
        HydrationOutcome::SkippedDirty => (0, 1),
    };

    Ok(PullReport {
        ok: skipped_dirty == 0,
        command: "pull".to_string(),
        via: "cli".to_string(),
        mount_id: mount.mount_id.0.clone(),
        root: mount.root.display().to_string(),
        target: target_path.display().to_string(),
        enumerated: 0,
        stubbed: 0,
        hydrated,
        skipped_dirty,
    })
}

fn merged_entity_record<S>(store: &S, entry: &TreeEntry) -> Result<EntityRecord, PullError>
where
    S: EntityRepository,
{
    let existing = store
        .get_entity(&entry.mount_id, &entry.remote_id)
        .map_err(PullError::Store)?;
    let mut record = EntityRecord::from(entry.clone());

    if let Some(existing) = existing {
        record.hydration = existing.hydration;
        record.content_hash = existing.content_hash;
    }

    Ok(record)
}

fn write_stub_if_needed(
    connector: &NotionConnector,
    mount: &MountConfig,
    entry: &TreeEntry,
) -> Result<bool, PullError> {
    if mount.projection == ProjectionMode::MacosFileProvider {
        return Ok(false);
    }

    match entry.kind {
        EntityKind::Page => {
            let path = mount.root.join(&entry.path);
            if path.exists() && !is_stub_file(&path)? {
                return Ok(false);
            }
            write_atomic(&path, stub_markdown(entry)?)?;
            Ok(true)
        }
        EntityKind::Database => {
            let directory = mount.root.join(&entry.path);
            std::fs::create_dir_all(&directory).map_err(|error| PullError::WriteFile {
                path: directory.clone(),
                message: error.to_string(),
            })?;
            let schema = connector
                .database_schema_yaml(&entry.remote_id)
                .map_err(PullError::Connector)?;
            write_atomic(&directory.join("_schema.yaml"), schema)?;
            Ok(false)
        }
        EntityKind::Directory => {
            let directory = mount.root.join(&entry.path);
            std::fs::create_dir_all(&directory).map_err(|error| PullError::WriteFile {
                path: directory,
                message: error.to_string(),
            })?;
            Ok(false)
        }
        EntityKind::Asset | EntityKind::Unknown(_) => Ok(false),
    }
}

fn hydrate_entity<S>(
    store: &mut S,
    connector: &NotionConnector,
    mount: &MountConfig,
    entity: EntityRecord,
) -> Result<HydrationOutcome, PullError>
where
    S: EntityRepository + ShadowRepository,
{
    let path = mount.root.join(&entity.path);
    if !can_replace_file(store, mount, &entity, &path)? {
        return Ok(HydrationOutcome::SkippedDirty);
    }

    let native = connector
        .fetch(FetchRequest {
            remote_id: entity.remote_id.clone(),
        })
        .map_err(PullError::Connector)?;
    let rendered = connector
        .render_native_entity_for_path(&native, &entity.path)
        .map_err(PullError::Connector)?;
    connector
        .download_rendered_media(&rendered, &mount.root)
        .map_err(PullError::Connector)?;
    let markdown = render_canonical_markdown(&rendered.document);

    write_atomic(&path, markdown)?;
    store
        .save_shadow(&mount.mount_id, rendered.shadow.clone())
        .map_err(PullError::Store)?;
    store
        .save_entity(hydrated_record(entity, rendered.shadow))
        .map_err(PullError::Store)?;

    Ok(HydrationOutcome::Hydrated)
}

fn hydrated_record(mut entity: EntityRecord, shadow: ShadowDocument) -> EntityRecord {
    entity.hydration = HydrationState::Hydrated;
    entity.content_hash = Some(shadow.body_hash);
    entity
}

fn can_replace_file<S>(
    store: &S,
    mount: &MountConfig,
    entity: &EntityRecord,
    path: &Path,
) -> Result<bool, PullError>
where
    S: ShadowRepository,
{
    if !path.exists() {
        return Ok(true);
    }

    if is_stub_file(path)? {
        return Ok(true);
    }

    if entity.hydration != HydrationState::Hydrated {
        return Ok(false);
    }

    let contents = std::fs::read_to_string(path).map_err(|error| PullError::ReadFile {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let parsed = parse_canonical_markdown(&contents).map_err(|error| PullError::ReadFile {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let shadow = store
        .load_shadow(&mount.mount_id, &entity.remote_id)
        .map_err(PullError::Store)?;

    Ok(parsed.document.body == shadow.rendered_body)
}

fn is_stub_file(path: &Path) -> Result<bool, PullError> {
    if !path.exists() {
        return Ok(false);
    }

    let contents = std::fs::read_to_string(path).map_err(|error| PullError::ReadFile {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    Ok(contents.contains(CanonicalDocument::STUB_MARKER))
}

fn stub_markdown(entry: &TreeEntry) -> Result<String, PullError> {
    let document = CanonicalDocument::new(
        entry
            .stub_frontmatter
            .clone()
            .unwrap_or_else(|| stub_frontmatter(entry)),
        stub_body(),
    );
    Ok(render_canonical_markdown(&document))
}

fn stub_frontmatter(entry: &TreeEntry) -> String {
    format!(
        "afs:\n  id: {}\n  type: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\n",
        entry.remote_id.0,
        entity_type_name(&entry.kind),
        yaml_string(entry.remote_edited_at.as_deref().unwrap_or("unknown")),
        yaml_string(entry.remote_edited_at.as_deref().unwrap_or("unknown")),
        yaml_string(&entry.title)
    )
}

fn stub_body() -> String {
    format!("{}\n", CanonicalDocument::STUB_MARKER)
}

fn entity_type_name(kind: &EntityKind) -> &'static str {
    match kind {
        EntityKind::Page => "page",
        EntityKind::Database => "database",
        EntityKind::Directory => "directory",
        EntityKind::Asset => "asset",
        EntityKind::Unknown(_) => "unknown",
    }
}

fn write_atomic(path: &Path, contents: String) -> Result<(), PullError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| PullError::WriteFile {
            path: parent.to_path_buf(),
            message: error.to_string(),
        })?;
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("afs-write");
    let temp_path = path.with_file_name(format!(".{file_name}.afs-tmp"));
    std::fs::write(&temp_path, contents).map_err(|error| PullError::WriteFile {
        path: temp_path.clone(),
        message: error.to_string(),
    })?;
    std::fs::rename(&temp_path, path).map_err(|error| PullError::WriteFile {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf, PullError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| PullError::CurrentDir(error.to_string()))
    }
}

fn find_mount_for_path<'a>(mounts: &'a [MountConfig], path: &Path) -> Option<&'a MountConfig> {
    mounts
        .iter()
        .filter(|mount| path.starts_with(&mount.root))
        .max_by_key(|mount| mount.root.components().count())
}

fn relative_target_path(mount: &MountConfig, absolute_path: &Path) -> Result<PathBuf, PullError> {
    absolute_path
        .strip_prefix(&mount.root)
        .map(Path::to_path_buf)
        .map_err(|_| PullError::MountNotFound(absolute_path.to_path_buf()))
}

fn yaml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum HydrationOutcome {
    Hydrated,
    SkippedDirty,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PullError {
    Connector(afs_core::AfsError),
    CurrentDir(String),
    MountNotFound(PathBuf),
    ReadFile { path: PathBuf, message: String },
    Store(StoreError),
    WriteFile { path: PathBuf, message: String },
}

impl PullError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Connector(afs_core::AfsError::NotImplemented(_)) => "not_implemented",
            Self::Connector(_) => "connector_error",
            Self::CurrentDir(_) => "current_dir_failed",
            Self::MountNotFound(_) => "mount_not_found",
            Self::ReadFile { .. } => "read_file_failed",
            Self::Store(StoreError::EntityPathMissing { .. }) => "entity_path_missing",
            Self::Store(_) => "store_error",
            Self::WriteFile { .. } => "write_file_failed",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Connector(error) => error.to_string(),
            Self::CurrentDir(message) => format!("failed to resolve current directory: {message}"),
            Self::MountNotFound(path) => {
                format!("no AgentFS mount contains `{}`", path.display())
            }
            Self::ReadFile { path, message } => {
                format!("failed to read `{}`: {message}", path.display())
            }
            Self::Store(error) => error.to_string(),
            Self::WriteFile { path, message } => {
                format!("failed to write `{}`: {message}", path.display())
            }
        }
    }
}
