//! `loc restore` orchestration.
//!
//! Restore is a local recovery command. It discards local edits by rewriting the
//! projected file from the last synced shadow, then marks the entity hydrated.
//! It does not call connectors and it leaves failed journals intact for audit.

use std::path::{Path, PathBuf};

use locality_core::canonical::render_canonical_markdown;
use locality_core::model::{CanonicalDocument, EntityKind, HydrationState};
use locality_core::path_projection::page_document_path;
use locality_store::{
    EntityRecord, EntityRepository, MountConfig, MountRepository, ShadowRepository, StoreError,
};
use localityd::file_provider;
use localityd::virtual_fs::virtual_fs_content_path;
use serde::Serialize;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RestoreOptions {
    pub force: bool,
    pub state_root: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RestoreReport {
    pub ok: bool,
    pub command: &'static str,
    pub path: String,
    pub mount_id: String,
    pub entity_id: String,
    pub action: String,
    pub message: String,
}

pub fn run_restore<S>(
    store: &mut S,
    target_path: impl AsRef<Path>,
    options: RestoreOptions,
) -> Result<RestoreReport, RestoreError>
where
    S: MountRepository + EntityRepository + ShadowRepository,
{
    let mut absolute_path = absolute_path(target_path.as_ref())?;
    let mounts = store.load_mounts().map_err(RestoreError::Store)?;
    let mount = find_mount_for_path(&mounts, &absolute_path)
        .cloned()
        .ok_or_else(|| RestoreError::MountNotFound(absolute_path.clone()))?;
    let mut relative_path = relative_entity_path(&mount, &absolute_path)?;
    let mut entity = store
        .find_entity_by_path(&mount.mount_id, &relative_path)
        .map_err(RestoreError::Store)?;
    if entity.is_none() && absolute_path.is_dir() {
        let page_relative_path = page_document_path(&relative_path);
        if let Some(page_entity) = store
            .find_entity_by_path(&mount.mount_id, &page_relative_path)
            .map_err(RestoreError::Store)?
        {
            absolute_path = page_document_path(&absolute_path);
            relative_path = page_relative_path;
            entity = Some(page_entity);
        }
    }
    let mut entity = entity.ok_or_else(|| {
        RestoreError::Store(StoreError::EntityPathMissing {
            mount_id: mount.mount_id.clone(),
            path: relative_path.clone(),
        })
    })?;

    if entity.kind != EntityKind::Page {
        return Err(RestoreError::UnsupportedEntity(entity.kind));
    }
    if entity.hydration == HydrationState::Conflicted && !options.force {
        return Err(RestoreError::ConflictedRequiresForce(relative_path));
    }

    let shadow = store
        .load_shadow(&mount.mount_id, &entity.remote_id)
        .map_err(RestoreError::Store)?;
    let frontmatter = if shadow.frontmatter.trim().is_empty() {
        frontmatter_from_entity(&entity)
    } else {
        shadow.frontmatter.clone()
    };
    let document = CanonicalDocument::new(frontmatter, shadow.rendered_body.clone());
    let write_path = restore_write_path(
        options.state_root.as_deref(),
        &mount,
        &relative_path,
        &absolute_path,
    )?;
    write_atomic(&write_path, render_canonical_markdown(&document).as_bytes()).map_err(
        |error| RestoreError::WriteFile {
            path: write_path.clone(),
            message: error.to_string(),
        },
    )?;

    entity.hydration = HydrationState::Hydrated;
    entity.content_hash = Some(shadow.body_hash);
    store
        .save_entity(entity.clone())
        .map_err(RestoreError::Store)?;

    Ok(RestoreReport {
        ok: true,
        command: "restore",
        path: absolute_path.display().to_string(),
        mount_id: mount.mount_id.0,
        entity_id: entity.remote_id.0,
        action: "restored".to_string(),
        message: "local file restored from last synced shadow".to_string(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestoreError {
    ConflictedRequiresForce(PathBuf),
    CurrentDir(String),
    MountNotFound(PathBuf),
    Store(StoreError),
    UnsupportedEntity(EntityKind),
    WriteFile { path: PathBuf, message: String },
}

impl RestoreError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::ConflictedRequiresForce(_) => "restore_conflicted_requires_force",
            Self::CurrentDir(_) => "current_dir_failed",
            Self::MountNotFound(_) => "mount_not_found",
            Self::Store(StoreError::EntityPathMissing { .. }) => "entity_path_missing",
            Self::Store(StoreError::ShadowMissing { .. }) => "shadow_missing",
            Self::Store(_) => "store_error",
            Self::UnsupportedEntity(_) => "restore_unsupported_entity",
            Self::WriteFile { .. } => "write_file_failed",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::ConflictedRequiresForce(path) => {
                format!(
                    "`{}` is conflicted; rerun with --force to restore from shadow",
                    path.display()
                )
            }
            Self::CurrentDir(message) => format!("failed to resolve current directory: {message}"),
            Self::MountNotFound(path) => {
                format!("no Locality mount contains `{}`", path.display())
            }
            Self::Store(error) => error.to_string(),
            Self::UnsupportedEntity(kind) => {
                format!("restore only supports page.md files, not `{kind:?}`")
            }
            Self::WriteFile { path, message } => {
                format!("failed to write `{}`: {message}", path.display())
            }
        }
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, RestoreError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| RestoreError::CurrentDir(error.to_string()))
    }
}

fn find_mount_for_path<'a>(mounts: &'a [MountConfig], path: &Path) -> Option<&'a MountConfig> {
    file_provider::find_mount_for_path(mounts, path).map(|(mount, _)| mount)
}

fn relative_entity_path(
    mount: &MountConfig,
    absolute_path: &Path,
) -> Result<PathBuf, RestoreError> {
    file_provider::match_mount_path(mount, absolute_path)
        .map(|matched| matched.relative_path)
        .ok_or_else(|| RestoreError::MountNotFound(absolute_path.to_path_buf()))
}

fn restore_write_path(
    state_root: Option<&Path>,
    mount: &MountConfig,
    relative_path: &Path,
    absolute_path: &Path,
) -> Result<PathBuf, RestoreError> {
    if mount.projection.uses_virtual_filesystem() {
        let Some(state_root) = state_root else {
            return Err(RestoreError::WriteFile {
                path: absolute_path.to_path_buf(),
                message: "virtual filesystem restore requires a state root".to_string(),
            });
        };
        return virtual_fs_content_path(state_root, &mount.mount_id, relative_path).map_err(
            |error| RestoreError::WriteFile {
                path: absolute_path.to_path_buf(),
                message: error.to_string(),
            },
        );
    }

    Ok(absolute_path.to_path_buf())
}

fn frontmatter_from_entity(entity: &EntityRecord) -> String {
    let mut frontmatter = format!("loc:\n  id: {}\n  type: page\n", entity.remote_id.0);
    if let Some(remote_edited_at) = &entity.remote_edited_at {
        frontmatter.push_str(&format!("  remote_edited_at: {remote_edited_at}\n"));
    }
    frontmatter.push_str(&format!("title: {}\n", yaml_string(&entity.title)));
    frontmatter
}

fn yaml_string(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ' ' | '-' | '_' | '.'))
    {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
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
