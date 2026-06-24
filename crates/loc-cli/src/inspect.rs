//! `loc inspect` orchestration.
//!
//! Inspect is an explicit, lazy remote-change explanation barrier. Unlike
//! `loc status`, it is allowed to fetch the current remote render so humans and
//! agents can understand drift before deciding whether to push, pull, or review.

use std::path::{Path, PathBuf};

use locality_core::LocalityError;
use locality_core::canonical::{CanonicalParseError, parse_canonical_markdown};
use locality_core::explain::{
    RemoteChangeExplanation, RemoteChangeInput, RemoteChangeIssue, explain_remote_change,
};
use locality_core::hydration::{HydrationReason, HydrationRequest};
use locality_core::model::{EntityKind, HydrationState};
use locality_core::path_projection::page_document_path;
use locality_store::{
    EntityRecord, EntityRepository, MountConfig, MountRepository, ShadowRepository, StoreError,
};
use localityd::file_provider as daemon_file_provider;
use localityd::hydration::HydrationSource;
use localityd::virtual_fs::virtual_fs_content_path;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectOptions {
    pub path: PathBuf,
    pub state_root: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InspectReport {
    pub ok: bool,
    pub command: &'static str,
    pub path: String,
    pub local_read_path: String,
    pub mount_id: String,
    pub entity_id: String,
    pub title: String,
    pub synced_tree_version: Option<String>,
    pub remote_tree_version: Option<String>,
    pub explanation: RemoteChangeExplanation,
}

pub fn run_inspect<S, Source>(
    store: &S,
    source: &Source,
    options: InspectOptions,
) -> Result<InspectReport, InspectError>
where
    S: MountRepository + EntityRepository + ShadowRepository,
    Source: HydrationSource + ?Sized,
{
    let mut target = absolute_path(&options.path)?;
    let mounts = store.load_mounts().map_err(InspectError::Store)?;
    let mount = find_mount_for_path(&mounts, &target)
        .cloned()
        .ok_or_else(|| InspectError::MountNotFound(target.clone()))?;
    let mut relative_path = relative_entity_path(&mount, &target)?;
    let mut entity = store
        .find_entity_by_path(&mount.mount_id, &relative_path)
        .map_err(InspectError::Store)?;
    if entity.is_none() && target.is_dir() {
        let page_relative_path = page_document_path(&relative_path);
        if let Some(page_entity) = store
            .find_entity_by_path(&mount.mount_id, &page_relative_path)
            .map_err(InspectError::Store)?
        {
            target = page_document_path(&target);
            relative_path = page_relative_path;
            entity = Some(page_entity);
        }
    }
    let entity = entity.ok_or_else(|| {
        InspectError::Store(StoreError::EntityPathMissing {
            mount_id: mount.mount_id.clone(),
            path: relative_path.clone(),
        })
    })?;
    if entity.kind != EntityKind::Page {
        return Err(InspectError::UnsupportedEntity {
            path: target,
            kind: entity_kind_name(&entity.kind).to_string(),
        });
    }

    let shadow = store
        .load_shadow(&mount.mount_id, &entity.remote_id)
        .map_err(InspectError::Store)?;
    let local_read_path = projection_read_path(
        options.state_root.as_deref(),
        &mount,
        &relative_path,
        &target,
    )?;
    let local_contents =
        std::fs::read_to_string(&local_read_path).map_err(|error| InspectError::ReadFile {
            path: local_read_path.clone(),
            message: error.to_string(),
        })?;
    let local_parsed = parse_canonical_markdown(&local_contents);

    let remote = source
        .fetch_render(&HydrationRequest::new(
            mount.mount_id.clone(),
            entity.remote_id.clone(),
            entity.path.clone(),
            HydrationState::Hydrated,
            HydrationReason::ExplicitPull,
        ))
        .map_err(InspectError::RemoteFetch)?;

    let local_input = local_input(&entity, &relative_path, local_parsed.as_ref());
    let remote_body_start_line = remote_body_start_line(&remote.shadow);
    let explanation = explain_remote_change(
        &shadow,
        local_input,
        RemoteChangeInput::available(&remote.document, remote_body_start_line),
    );
    let ok = explanation.issues.is_empty();

    Ok(InspectReport {
        ok,
        command: "inspect",
        path: target.display().to_string(),
        local_read_path: local_read_path.display().to_string(),
        mount_id: mount.mount_id.0,
        entity_id: entity.remote_id.0,
        title: entity.title,
        synced_tree_version: entity.remote_edited_at,
        remote_tree_version: remote.remote_edited_at,
        explanation,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InspectError {
    CurrentDir(String),
    MountNotFound(PathBuf),
    ProjectionReadPath { path: PathBuf, message: String },
    ReadFile { path: PathBuf, message: String },
    Store(StoreError),
    RemoteFetch(LocalityError),
    UnsupportedEntity { path: PathBuf, kind: String },
}

impl InspectError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::CurrentDir(_) => "current_dir_failed",
            Self::MountNotFound(_) => "mount_not_found",
            Self::ProjectionReadPath { .. } => "projection_read_path_failed",
            Self::ReadFile { .. } => "read_file_failed",
            Self::Store(StoreError::ShadowMissing { .. }) => "shadow_missing",
            Self::Store(StoreError::EntityPathMissing { .. }) => "entity_path_missing",
            Self::Store(_) => "store_error",
            Self::RemoteFetch(LocalityError::RemoteNotFound(_)) => "remote_fetch_not_found",
            Self::RemoteFetch(LocalityError::Unsupported(_)) => "remote_fetch_unsupported",
            Self::RemoteFetch(_) => "remote_fetch_failed",
            Self::UnsupportedEntity { .. } => "unsupported_entity",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::CurrentDir(message) => format!("failed to resolve current directory: {message}"),
            Self::MountNotFound(path) => {
                format!("no Locality mount contains `{}`", path.display())
            }
            Self::ProjectionReadPath { path, message } => {
                format!(
                    "failed to resolve virtual content cache for `{}`: {message}",
                    path.display()
                )
            }
            Self::ReadFile { path, message } => {
                format!("failed to read `{}`: {message}", path.display())
            }
            Self::Store(error) => error.to_string(),
            Self::RemoteFetch(error) => format!("failed to fetch remote render: {error}"),
            Self::UnsupportedEntity { path, kind } => {
                format!(
                    "cannot inspect `{}` because `{kind}` is not a page",
                    path.display()
                )
            }
        }
    }
}

fn local_input<'a>(
    entity: &EntityRecord,
    relative_path: &Path,
    parsed: Result<&'a locality_core::canonical::ParsedCanonicalDocument, &'a CanonicalParseError>,
) -> RemoteChangeInput<'a> {
    match parsed {
        Ok(parsed) => {
            match parsed.remote_id() {
                Some(remote_id) if remote_id == &entity.remote_id => {}
                Some(_) => {
                    return RemoteChangeInput::Unavailable(RemoteChangeIssue::new(
                        "frontmatter_remote_id_mismatch",
                        format!(
                            "frontmatter `loc.id` does not match the entity mapped to `{}`",
                            relative_path.display()
                        ),
                    ));
                }
                None => {
                    return RemoteChangeInput::Unavailable(RemoteChangeIssue::new(
                        "frontmatter_remote_id_missing",
                        format!(
                            "frontmatter `loc.id` is missing for the entity mapped to `{}`",
                            relative_path.display()
                        ),
                    ));
                }
            }

            RemoteChangeInput::available(&parsed.document, parsed.body_start_line)
        }
        Err(error) => RemoteChangeInput::Unavailable(RemoteChangeIssue::new(
            "local_parse_failed",
            format!("canonical Markdown parse failed: {}", error.message),
        )),
    }
}

fn projection_read_path(
    state_root: Option<&Path>,
    mount: &MountConfig,
    relative_path: &Path,
    absolute_path: &Path,
) -> Result<PathBuf, InspectError> {
    if mount.projection.uses_virtual_filesystem() {
        let Some(state_root) = state_root else {
            return Err(InspectError::ProjectionReadPath {
                path: absolute_path.to_path_buf(),
                message: "virtual filesystem inspection requires a state root".to_string(),
            });
        };
        return virtual_fs_content_path(state_root, &mount.mount_id, relative_path).map_err(
            |error| InspectError::ProjectionReadPath {
                path: absolute_path.to_path_buf(),
                message: error.to_string(),
            },
        );
    }

    Ok(absolute_path.to_path_buf())
}

fn remote_body_start_line(shadow: &locality_core::shadow::ShadowDocument) -> usize {
    shadow
        .blocks
        .iter()
        .map(|block| block.source_span.start_line)
        .min()
        .unwrap_or(1)
}

fn absolute_path(path: &Path) -> Result<PathBuf, InspectError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| InspectError::CurrentDir(error.to_string()))
    }
}

fn find_mount_for_path<'a>(mounts: &'a [MountConfig], path: &Path) -> Option<&'a MountConfig> {
    daemon_file_provider::find_mount_for_path(mounts, path).map(|(mount, _)| mount)
}

fn relative_entity_path(
    mount: &MountConfig,
    absolute_path: &Path,
) -> Result<PathBuf, InspectError> {
    daemon_file_provider::match_mount_path(mount, absolute_path)
        .map(|matched| matched.relative_path)
        .ok_or_else(|| InspectError::MountNotFound(absolute_path.to_path_buf()))
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
