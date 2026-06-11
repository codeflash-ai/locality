//! `afs resolve` orchestration.
//!
//! Resolve finishes a materialized conflict locally. `--theirs` accepts the
//! remote sidecar as the new main file, while `--ours` and `--edited` keep the
//! main file and clear the conflicted state so a later push can proceed.

use std::path::{Path, PathBuf};

use afs_core::conflict::remote_variant_path;
use afs_core::model::{EntityKind, HydrationState};
use afs_store::{EntityRepository, MountConfig, MountRepository, ShadowRepository, StoreError};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveChoice {
    Ours,
    Theirs,
    Edited,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolveOptions {
    pub choice: ResolveChoice,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ResolveReport {
    pub ok: bool,
    pub command: &'static str,
    pub path: String,
    pub mount_id: String,
    pub entity_id: String,
    pub action: String,
    pub message: String,
}

pub fn run_resolve<S>(
    store: &mut S,
    target_path: impl AsRef<Path>,
    options: ResolveOptions,
) -> Result<ResolveReport, ResolveError>
where
    S: MountRepository + EntityRepository + ShadowRepository,
{
    let absolute_target = absolute_path(target_path.as_ref())?;
    let mounts = store.load_mounts().map_err(ResolveError::Store)?;
    let mount = find_mount_for_path(&mounts, &absolute_target)
        .cloned()
        .ok_or_else(|| ResolveError::MountNotFound(absolute_target.clone()))?;
    let relative_target = relative_entity_path(&mount, &absolute_target)?;
    let relative_path = normalize_target_path(store, &mount, &relative_target)?;
    let mut entity = store
        .find_entity_by_path(&mount.mount_id, &relative_path)
        .map_err(ResolveError::Store)?
        .ok_or_else(|| {
            ResolveError::Store(StoreError::EntityPathMissing {
                mount_id: mount.mount_id.clone(),
                path: relative_path.clone(),
            })
        })?;

    if entity.kind != EntityKind::Page {
        return Err(ResolveError::UnsupportedEntity(entity.kind));
    }
    if entity.hydration != HydrationState::Conflicted {
        return Err(ResolveError::EntityNotConflicted(relative_path));
    }

    let main_path = mount.root.join(&entity.path);
    let remote_path = remote_variant_path(&main_path);
    if !remote_path.exists() {
        return Err(ResolveError::RemoteSidecarMissing(remote_path));
    }

    match options.choice {
        ResolveChoice::Theirs => {
            let remote = std::fs::read(&remote_path).map_err(|error| ResolveError::ReadFile {
                path: remote_path.clone(),
                message: error.to_string(),
            })?;
            write_atomic(&main_path, &remote).map_err(|error| ResolveError::WriteFile {
                path: main_path.clone(),
                message: error.to_string(),
            })?;
            remove_file(&remote_path).map_err(|error| ResolveError::WriteFile {
                path: remote_path.clone(),
                message: error.to_string(),
            })?;
            let shadow = store
                .load_shadow(&mount.mount_id, &entity.remote_id)
                .map_err(ResolveError::Store)?;
            entity.hydration = HydrationState::Hydrated;
            entity.content_hash = Some(shadow.body_hash);
            store
                .save_entity(entity.clone())
                .map_err(ResolveError::Store)?;

            Ok(ResolveReport {
                ok: true,
                command: "resolve",
                path: main_path.display().to_string(),
                mount_id: mount.mount_id.0,
                entity_id: entity.remote_id.0,
                action: "resolved_theirs".to_string(),
                message: "accepted the remote sidecar as the local projection".to_string(),
            })
        }
        ResolveChoice::Ours | ResolveChoice::Edited => {
            remove_file(&remote_path).map_err(|error| ResolveError::WriteFile {
                path: remote_path.clone(),
                message: error.to_string(),
            })?;
            entity.hydration = HydrationState::Dirty;
            store
                .save_entity(entity.clone())
                .map_err(ResolveError::Store)?;

            let (action, message) = match options.choice {
                ResolveChoice::Ours => (
                    "resolved_ours",
                    "kept the local file and cleared the remote sidecar",
                ),
                ResolveChoice::Edited => (
                    "resolved_edited",
                    "kept the edited local file and cleared the remote sidecar",
                ),
                ResolveChoice::Theirs => unreachable!(),
            };

            Ok(ResolveReport {
                ok: true,
                command: "resolve",
                path: main_path.display().to_string(),
                mount_id: mount.mount_id.0,
                entity_id: entity.remote_id.0,
                action: action.to_string(),
                message: message.to_string(),
            })
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveError {
    CurrentDir(String),
    EntityNotConflicted(PathBuf),
    MountNotFound(PathBuf),
    ReadFile { path: PathBuf, message: String },
    RemoteSidecarMissing(PathBuf),
    Store(StoreError),
    UnsupportedEntity(EntityKind),
    WriteFile { path: PathBuf, message: String },
}

impl ResolveError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::CurrentDir(_) => "current_dir_failed",
            Self::EntityNotConflicted(_) => "resolve_entity_not_conflicted",
            Self::MountNotFound(_) => "mount_not_found",
            Self::ReadFile { .. } => "read_file_failed",
            Self::RemoteSidecarMissing(_) => "resolve_remote_sidecar_missing",
            Self::Store(StoreError::EntityPathMissing { .. }) => "entity_path_missing",
            Self::Store(StoreError::ShadowMissing { .. }) => "shadow_missing",
            Self::Store(_) => "store_error",
            Self::UnsupportedEntity(_) => "resolve_unsupported_entity",
            Self::WriteFile { .. } => "write_file_failed",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::CurrentDir(message) => format!("failed to resolve current directory: {message}"),
            Self::EntityNotConflicted(path) => {
                format!("`{}` is not currently conflicted", path.display())
            }
            Self::MountNotFound(path) => {
                format!("no AgentFS mount contains `{}`", path.display())
            }
            Self::ReadFile { path, message } => {
                format!("failed to read `{}`: {message}", path.display())
            }
            Self::RemoteSidecarMissing(path) => {
                format!("conflict sidecar `{}` is missing", path.display())
            }
            Self::Store(error) => error.to_string(),
            Self::UnsupportedEntity(kind) => {
                format!("resolve only supports page files, not `{kind:?}`")
            }
            Self::WriteFile { path, message } => {
                format!("failed to write `{}`: {message}", path.display())
            }
        }
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, ResolveError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| ResolveError::CurrentDir(error.to_string()))
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
) -> Result<PathBuf, ResolveError> {
    absolute_path
        .strip_prefix(&mount.root)
        .map(Path::to_path_buf)
        .map_err(|_| ResolveError::MountNotFound(absolute_path.to_path_buf()))
}

fn normalize_target_path<S>(
    store: &S,
    mount: &MountConfig,
    relative_path: &Path,
) -> Result<PathBuf, ResolveError>
where
    S: EntityRepository,
{
    if store
        .find_entity_by_path(&mount.mount_id, relative_path)
        .map_err(ResolveError::Store)?
        .is_some()
    {
        return Ok(relative_path.to_path_buf());
    }

    if let Some(base_path) = strip_remote_suffix(relative_path)
        && store
            .find_entity_by_path(&mount.mount_id, &base_path)
            .map_err(ResolveError::Store)?
            .is_some()
    {
        return Ok(base_path);
    }

    Ok(relative_path.to_path_buf())
}

fn strip_remote_suffix(path: &Path) -> Option<PathBuf> {
    let file_name = path.file_name()?.to_str()?;
    let stripped = file_name
        .strip_suffix(".remote.md")
        .map(|name| format!("{name}.md"))
        .or_else(|| {
            file_name
                .strip_suffix(".remote")
                .map(|name| name.to_string())
        })?;

    let mut normalized = path.to_path_buf();
    normalized.set_file_name(stripped);
    Some(normalized)
}

fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let temp_path = path.with_extension(format!(
        "{}tmp",
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| format!("{extension}."))
            .unwrap_or_default()
    ));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&temp_path, contents)?;
    std::fs::rename(temp_path, path)
}

fn remove_file(path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }

    std::fs::remove_file(path)
}
