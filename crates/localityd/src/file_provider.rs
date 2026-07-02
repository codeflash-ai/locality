//! macOS File Provider compatibility aliases.
//!
//! The daemon-owned virtual filesystem contract lives in `virtual_fs`. macOS
//! File Provider, Linux FUSE, and future platform projections should bind to that
//! generic API instead of growing platform-specific daemon semantics.

use locality_core::canonical::{parse_canonical_markdown, render_canonical_markdown};
use locality_core::conflict::{
    has_unresolved_conflict_markers, local_version_from_conflict_markers,
    render_inline_conflict_markdown,
};
use locality_core::model::{CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId};
use locality_core::shadow::ShadowDocument;
use locality_core::{LocalityError, LocalityResult};
use locality_store::{
    EntityRecord, EntityRepository, FreshnessStateRepository, MountConfig, MountRepository,
    ProjectionMode, ShadowRepository, StoreError, VirtualMutationRepository,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::hydration::HydrationSource;
use crate::shadow_match::{
    parsed_changes_retain_current_shadow_blocks, parsed_documents_match_ignoring_sync_metadata,
    parsed_matches_shadow,
};
use crate::virtual_fs;
use crate::virtual_fs::{
    mount_point_directory_name, mount_point_identifier, virtual_projection_mount_point,
};

pub use crate::virtual_fs::{
    ROOT_CONTAINER_IDENTIFIER, VirtualFsChildrenReport as FileProviderChildrenReport,
    VirtualFsItem as FileProviderItem, VirtualFsItemKind as FileProviderItemKind,
    VirtualFsItemReport as FileProviderItemReport,
    VirtualFsMaterializeOutcome as FileProviderMaterializeOutcome,
    VirtualFsMaterializeReport as FileProviderMaterializeReport,
};

pub const MACOS_FILE_PROVIDER_DOMAIN_ID: &str = "loc";
pub const MACOS_FILE_PROVIDER_DISPLAY_NAME: &str = "";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileProviderReadReport {
    pub mount_id: String,
    pub identifier: String,
    pub remote_id: String,
    pub path: String,
    pub outcome: FileProviderMaterializeOutcome,
    pub hydration: HydrationState,
    pub item: FileProviderItem,
    pub contents_base64: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileProviderDomainChildrenReport {
    pub domain_id: String,
    pub children: Vec<FileProviderDomainChild>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileProviderDomainChild {
    pub mount_id: String,
    pub item: FileProviderItem,
}

pub fn file_provider_domain_children<S>(
    store: &S,
    domain_id: &str,
) -> LocalityResult<FileProviderDomainChildrenReport>
where
    S: MountRepository,
{
    let mut mounts = store
        .load_mounts()
        .map_err(LocalityError::from)?
        .into_iter()
        .filter(|mount| mount.projection == ProjectionMode::MacosFileProvider)
        .collect::<Vec<_>>();
    mounts.sort_by(|left, right| {
        left.connector
            .cmp(&right.connector)
            .then_with(|| left.mount_id.0.cmp(&right.mount_id.0))
    });

    let children = mounts
        .into_iter()
        .map(|mount| FileProviderDomainChild {
            mount_id: mount.mount_id.0.clone(),
            item: shared_domain_mount_point_item(&mount),
        })
        .collect();

    Ok(FileProviderDomainChildrenReport {
        domain_id: domain_id.to_string(),
        children,
    })
}

pub fn file_provider_item<S>(
    store: &S,
    mount_id: &MountId,
    identifier: &str,
) -> LocalityResult<FileProviderItemReport>
where
    S: MountRepository + EntityRepository + VirtualMutationRepository,
{
    virtual_fs::virtual_fs_item(store, mount_id, identifier)
}

fn shared_domain_mount_point_item(mount: &MountConfig) -> FileProviderItem {
    let filename = mount_point_directory_name(mount);
    FileProviderItem {
        identifier: mount_point_identifier(mount),
        parent_identifier: Some(ROOT_CONTAINER_IDENTIFIER.to_string()),
        filename: filename.clone(),
        kind: FileProviderItemKind::Folder,
        entity_kind: None,
        remote_id: None,
        path: filename,
        hydration: None,
        content_type: "public.folder".to_string(),
        remote_edited_at: None,
        materialized_path: Some(virtual_projection_mount_point(mount).display().to_string()),
        byte_size: None,
    }
}

pub fn file_provider_children<S>(
    store: &S,
    mount_id: &MountId,
    container_identifier: &str,
) -> LocalityResult<FileProviderChildrenReport>
where
    S: MountRepository + EntityRepository + VirtualMutationRepository,
{
    virtual_fs::virtual_fs_children(store, mount_id, container_identifier)
}

pub fn materialize_file_provider_item<S, Source>(
    store: &mut S,
    source: &Source,
    mount_id: &MountId,
    identifier: &str,
) -> LocalityResult<FileProviderMaterializeReport>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + VirtualMutationRepository
        + FreshnessStateRepository
        + locality_store::RemoteObservationRepository,
    Source: HydrationSource + ?Sized,
{
    virtual_fs::materialize_virtual_fs_item(store, source, mount_id, identifier)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountPathMatch {
    pub access_root: PathBuf,
    pub relative_path: PathBuf,
}

pub fn mount_access_roots(mount: &MountConfig) -> Vec<PathBuf> {
    let mut roots = Vec::new();

    roots.push(mount.root.clone());

    if matches!(
        mount.projection,
        ProjectionMode::LinuxFuse | ProjectionMode::WindowsCloudFiles
    ) {
        roots.push(virtual_projection_mount_point(mount));
    }

    #[cfg(target_os = "macos")]
    if mount.projection == ProjectionMode::MacosFileProvider {
        roots.extend(macos_file_provider_access_roots(mount));
    }

    dedupe_paths(roots)
}

pub fn match_mount_path(mount: &MountConfig, path: &Path) -> Option<MountPathMatch> {
    mount_access_roots(mount)
        .into_iter()
        .filter_map(|access_root| {
            relative_to_access_root(path, &access_root).map(|relative_path| MountPathMatch {
                access_root,
                relative_path,
            })
        })
        .max_by_key(|matched| matched.access_root.components().count())
}

pub fn find_mount_for_path<'a>(
    mounts: &'a [MountConfig],
    path: &Path,
) -> Option<(&'a MountConfig, MountPathMatch)> {
    mounts
        .iter()
        .filter_map(|mount| match_mount_path(mount, path).map(|matched| (mount, matched)))
        .max_by_key(|(_, matched)| matched.access_root.components().count())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectionReconcileReport {
    pub checked: usize,
    pub reconciled: usize,
    pub skipped_unchanged: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectionRefreshReport {
    pub checked: usize,
    pub refreshed: usize,
    pub skipped_missing_cache: usize,
    pub skipped_missing_projection: usize,
    pub skipped_unchanged: usize,
    pub skipped_local_changes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionRefreshBase {
    pub mount_id: MountId,
    pub remote_id: RemoteId,
    pub previous_shadow: ShadowDocument,
}

/// Imports visible virtual-provider replica edits that did not arrive through
/// the provider callback path.
///
/// This is intentionally a narrow command-boundary fallback, not a background
/// scanner: it reads only an explicit target. The daemon cache remains the
/// durable source used by diff and push after this reconciliation step.
pub fn reconcile_visible_projection<S>(
    store: &mut S,
    state_root: &Path,
    target: Option<&Path>,
) -> LocalityResult<ProjectionReconcileReport>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + VirtualMutationRepository
        + FreshnessStateRepository,
{
    reconcile_visible_projection_with_mode(store, state_root, target, true)
}

/// Imports only visible virtual-provider replicas newer than daemon content.
///
/// This is used before explicit pull so a missed local edit is preserved, while
/// an older stale visible replica does not get mistaken for a local edit.
pub fn reconcile_newer_macos_file_provider_projection<S>(
    store: &mut S,
    state_root: &Path,
    target: Option<&Path>,
) -> LocalityResult<ProjectionReconcileReport>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + VirtualMutationRepository
        + FreshnessStateRepository,
{
    reconcile_visible_projection_with_mode(store, state_root, target, false)
}

fn reconcile_visible_projection_with_mode<S>(
    store: &mut S,
    state_root: &Path,
    target: Option<&Path>,
    force_explicit_target_read: bool,
) -> LocalityResult<ProjectionReconcileReport>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + VirtualMutationRepository
        + FreshnessStateRepository,
{
    let Some(target) = target.map(absolute_reconcile_path).transpose()? else {
        return Ok(ProjectionReconcileReport::default());
    };
    let mounts = store.load_mounts().map_err(LocalityError::from)?;
    let mut report = ProjectionReconcileReport::default();

    for mount in mounts {
        if !supports_visible_projection_refresh(&mount.projection) {
            continue;
        }

        let Some(target_match) = match_mount_path(&mount, &target) else {
            continue;
        };

        let content_root = virtual_fs::virtual_fs_content_root(state_root, &mount.mount_id);
        let entities = scoped_page_entities(store, &mount, Some(&target_match))?;
        for entity in entities {
            let Some(candidate) = reconcile_candidate_path(
                &mount,
                &entity,
                Some(&target),
                Some(&target_match),
                force_explicit_target_read,
            ) else {
                continue;
            };

            match reconcile_projection_candidate(store, &mount, &entity, &content_root, candidate)?
            {
                ProjectionCandidateOutcome::Skipped => {}
                ProjectionCandidateOutcome::Unchanged => {
                    report.checked += 1;
                    report.skipped_unchanged += 1;
                }
                ProjectionCandidateOutcome::Reconciled => {
                    report.checked += 1;
                    report.reconciled += 1;
                }
            }
        }
    }

    Ok(report)
}

/// Copies daemon-materialized content back into a visible virtual-provider
/// replica after an explicit remote refresh.
///
/// Some virtual providers may keep an already-materialized visible file stale
/// even after the daemon content cache has accepted newer remote content. This
/// repair is deliberately target-scoped and only writes visible files that
/// already exist.
pub fn refresh_visible_projection<S>(
    store: &S,
    state_root: &Path,
    target: Option<&Path>,
    refresh_bases: &[ProjectionRefreshBase],
) -> LocalityResult<ProjectionRefreshReport>
where
    S: MountRepository + EntityRepository,
{
    refresh_projection_for(
        store,
        state_root,
        target,
        refresh_bases,
        supports_visible_projection_refresh,
    )
}

pub fn visible_projection_refresh_bases<S>(
    store: &S,
    target: Option<&Path>,
) -> LocalityResult<Vec<ProjectionRefreshBase>>
where
    S: MountRepository + EntityRepository + ShadowRepository,
{
    projection_refresh_bases_for(store, target, supports_visible_projection_refresh)
}

/// Repairs already-materialized visible virtual-provider replicas after a
/// background remote fast-forward.
///
/// Unlike explicit `loc pull <path>`, this path runs without direct user intent,
/// so it only replaces visible replica contents that are still equal to the
/// previous synced shadow. If the visible file diverged, the repair is skipped
/// so a missed provider write is not silently overwritten.
pub fn refresh_visible_entity_projection_if_clean<S>(
    store: &S,
    state_root: &Path,
    mount_id: &MountId,
    remote_id: &RemoteId,
    previous_shadow: &ShadowDocument,
) -> LocalityResult<ProjectionRefreshReport>
where
    S: MountRepository + EntityRepository,
{
    let Some(mount) = store.get_mount(mount_id).map_err(LocalityError::from)? else {
        return Ok(ProjectionRefreshReport::default());
    };
    if !supports_visible_projection_refresh(&mount.projection) {
        return Ok(ProjectionRefreshReport::default());
    }

    refresh_entity_projection_if_clean(store, state_root, &mount, remote_id, previous_shadow)
}

/// Repairs already-materialized macOS File Provider replicas after a background
/// remote fast-forward.
///
/// Unlike explicit `loc pull <path>`, this path runs without direct user intent,
/// so it only replaces visible replica contents that are still equal to the
/// previous synced shadow. If the visible file diverged, the repair is skipped
/// so a missed File Provider write is not silently overwritten.
pub fn refresh_macos_file_provider_entity_projection_if_clean<S>(
    store: &S,
    state_root: &Path,
    mount_id: &MountId,
    remote_id: &RemoteId,
    previous_shadow: &ShadowDocument,
) -> LocalityResult<ProjectionRefreshReport>
where
    S: MountRepository + EntityRepository,
{
    let Some(mount) = store.get_mount(mount_id).map_err(LocalityError::from)? else {
        return Ok(ProjectionRefreshReport::default());
    };
    if mount.projection != ProjectionMode::MacosFileProvider {
        return Ok(ProjectionRefreshReport::default());
    }

    refresh_entity_projection_if_clean(store, state_root, &mount, remote_id, previous_shadow)
}

fn refresh_entity_projection_if_clean<S>(
    store: &S,
    state_root: &Path,
    mount: &MountConfig,
    remote_id: &RemoteId,
    previous_shadow: &ShadowDocument,
) -> LocalityResult<ProjectionRefreshReport>
where
    S: EntityRepository,
{
    let Some(entity) = store
        .get_entity(&mount.mount_id, remote_id)
        .map_err(LocalityError::from)?
    else {
        return Ok(ProjectionRefreshReport::default());
    };
    if entity.kind != EntityKind::Page {
        return Ok(ProjectionRefreshReport::default());
    }

    let content_root = virtual_fs::virtual_fs_content_root(state_root, &mount.mount_id);
    let mut report = ProjectionRefreshReport::default();
    let candidates = existing_projection_paths(&mount, &entity.path);
    if candidates.is_empty() {
        report.skipped_missing_projection += 1;
        return Ok(report);
    }

    for candidate in candidates {
        match refresh_projection_candidate_if_clean(
            &entity,
            &content_root,
            candidate,
            Some(previous_shadow),
        )? {
            ProjectionRefreshOutcome::MissingCache => {
                report.checked += 1;
                report.skipped_missing_cache += 1;
            }
            ProjectionRefreshOutcome::MissingProjection => {
                report.checked += 1;
                report.skipped_missing_projection += 1;
            }
            ProjectionRefreshOutcome::Unchanged => {
                report.checked += 1;
                report.skipped_unchanged += 1;
            }
            ProjectionRefreshOutcome::SkippedLocalChanges => {
                report.checked += 1;
                report.skipped_local_changes += 1;
            }
            ProjectionRefreshOutcome::Refreshed => {
                report.checked += 1;
                report.refreshed += 1;
            }
        }
    }

    Ok(report)
}

pub fn refresh_macos_file_provider_projection<S>(
    store: &S,
    state_root: &Path,
    target: Option<&Path>,
    refresh_bases: &[ProjectionRefreshBase],
) -> LocalityResult<ProjectionRefreshReport>
where
    S: MountRepository + EntityRepository,
{
    refresh_projection_for(store, state_root, target, refresh_bases, |projection| {
        matches!(projection, ProjectionMode::MacosFileProvider)
    })
}

pub fn macos_file_provider_projection_refresh_bases<S>(
    store: &S,
    target: Option<&Path>,
) -> LocalityResult<Vec<ProjectionRefreshBase>>
where
    S: MountRepository + EntityRepository + ShadowRepository,
{
    projection_refresh_bases_for(store, target, |projection| {
        matches!(projection, ProjectionMode::MacosFileProvider)
    })
}

fn refresh_projection_for<S>(
    store: &S,
    state_root: &Path,
    target: Option<&Path>,
    refresh_bases: &[ProjectionRefreshBase],
    include_projection: impl Fn(&ProjectionMode) -> bool,
) -> LocalityResult<ProjectionRefreshReport>
where
    S: MountRepository + EntityRepository,
{
    let Some(target) = target.map(absolute_reconcile_path).transpose()? else {
        return Ok(ProjectionRefreshReport::default());
    };
    let mounts = store.load_mounts().map_err(LocalityError::from)?;
    let mut report = ProjectionRefreshReport::default();

    for mount in mounts {
        if !include_projection(&mount.projection) {
            continue;
        }

        let Some(target_match) = match_mount_path(&mount, &target) else {
            continue;
        };

        let content_root = virtual_fs::virtual_fs_content_root(state_root, &mount.mount_id);
        let entities = scoped_page_entities(store, &mount, Some(&target_match))?;
        for entity in entities {
            let Some(candidate) =
                refresh_candidate_path(&mount, &entity, Some(&target), Some(&target_match))
            else {
                report.skipped_missing_projection += 1;
                continue;
            };

            match refresh_projection_candidate_if_clean(
                &entity,
                &content_root,
                candidate,
                refresh_base_for_entity(refresh_bases, &mount.mount_id, &entity.remote_id),
            )? {
                ProjectionRefreshOutcome::MissingCache => {
                    report.checked += 1;
                    report.skipped_missing_cache += 1;
                }
                ProjectionRefreshOutcome::MissingProjection => {
                    report.checked += 1;
                    report.skipped_missing_projection += 1;
                }
                ProjectionRefreshOutcome::Unchanged => {
                    report.checked += 1;
                    report.skipped_unchanged += 1;
                }
                ProjectionRefreshOutcome::SkippedLocalChanges => {
                    report.checked += 1;
                    report.skipped_local_changes += 1;
                }
                ProjectionRefreshOutcome::Refreshed => {
                    report.checked += 1;
                    report.refreshed += 1;
                }
            }
        }
    }

    Ok(report)
}

fn projection_refresh_bases_for<S>(
    store: &S,
    target: Option<&Path>,
    include_projection: impl Fn(&ProjectionMode) -> bool,
) -> LocalityResult<Vec<ProjectionRefreshBase>>
where
    S: MountRepository + EntityRepository + ShadowRepository,
{
    let Some(target) = target.map(absolute_reconcile_path).transpose()? else {
        return Ok(Vec::new());
    };
    let mounts = store.load_mounts().map_err(LocalityError::from)?;
    let mut bases = Vec::new();

    for mount in mounts {
        if !include_projection(&mount.projection) {
            continue;
        }

        let Some(target_match) = match_mount_path(&mount, &target) else {
            continue;
        };

        let entities = scoped_page_entities(store, &mount, Some(&target_match))?;
        for entity in entities {
            if refresh_candidate_path(&mount, &entity, Some(&target), Some(&target_match)).is_none()
            {
                continue;
            }

            match store.load_shadow(&mount.mount_id, &entity.remote_id) {
                Ok(previous_shadow) => bases.push(ProjectionRefreshBase {
                    mount_id: mount.mount_id.clone(),
                    remote_id: entity.remote_id,
                    previous_shadow,
                }),
                Err(StoreError::ShadowMissing { .. }) => {}
                Err(error) => return Err(LocalityError::from(error)),
            }
        }
    }

    Ok(bases)
}

fn supports_visible_projection_refresh(projection: &ProjectionMode) -> bool {
    matches!(
        projection,
        ProjectionMode::MacosFileProvider | ProjectionMode::WindowsCloudFiles
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProjectionCandidate {
    path: PathBuf,
    force_read: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectionCandidateOutcome {
    Skipped,
    Unchanged,
    Reconciled,
}

fn scoped_page_entities<S>(
    store: &S,
    mount: &MountConfig,
    target_match: Option<&MountPathMatch>,
) -> LocalityResult<Vec<EntityRecord>>
where
    S: EntityRepository,
{
    let target_relative = target_match.map(|matched| matched.relative_path.as_path());
    Ok(store
        .list_entities(&mount.mount_id)
        .map_err(LocalityError::from)?
        .into_iter()
        .filter(|entity| entity.kind == EntityKind::Page)
        .filter(|entity| match target_relative {
            None => true,
            Some(relative) if relative.as_os_str().is_empty() => true,
            Some(relative) => entity.path == relative || entity.path.starts_with(relative),
        })
        .collect())
}

fn reconcile_candidate_path(
    mount: &MountConfig,
    entity: &EntityRecord,
    target: Option<&Path>,
    target_match: Option<&MountPathMatch>,
    force_explicit_target_read: bool,
) -> Option<ProjectionCandidate> {
    if let (Some(target), Some(target_match)) = (target, target_match)
        && target_match.relative_path == entity.path
        && target.is_file()
    {
        return Some(ProjectionCandidate {
            path: target.to_path_buf(),
            force_read: force_explicit_target_read,
        });
    }

    newest_existing_projection_path(mount, &entity.path).map(|path| ProjectionCandidate {
        path,
        force_read: false,
    })
}

fn refresh_candidate_path(
    mount: &MountConfig,
    entity: &EntityRecord,
    target: Option<&Path>,
    target_match: Option<&MountPathMatch>,
) -> Option<PathBuf> {
    if let (Some(target), Some(target_match)) = (target, target_match)
        && target_match.relative_path == entity.path
        && target.is_file()
    {
        return Some(target.to_path_buf());
    }

    if let Some(target_match) = target_match
        && target_match.relative_path != entity.path
        && entity.path.starts_with(&target_match.relative_path)
    {
        return source_projection_root_for_match(mount, target_match)
            .map(|root| root.join(&entity.path));
    }

    newest_existing_projection_path(mount, &entity.path)
}

fn source_projection_root_for_match(
    mount: &MountConfig,
    target_match: &MountPathMatch,
) -> Option<PathBuf> {
    source_projection_roots(mount)
        .into_iter()
        .filter(|root| root.starts_with(&target_match.access_root))
        .max_by_key(|root| root.components().count())
}

fn newest_existing_projection_path(mount: &MountConfig, relative_path: &Path) -> Option<PathBuf> {
    existing_projection_path_entries(mount, relative_path)
        .into_iter()
        .max_by_key(|(_, modified)| *modified)
        .map(|(path, _)| path)
}

fn existing_projection_paths(mount: &MountConfig, relative_path: &Path) -> Vec<PathBuf> {
    existing_projection_path_entries(mount, relative_path)
        .into_iter()
        .map(|(path, _)| path)
        .collect()
}

fn existing_projection_path_entries(
    mount: &MountConfig,
    relative_path: &Path,
) -> Vec<(PathBuf, SystemTime)> {
    source_projection_roots(mount)
        .into_iter()
        .filter_map(|root| {
            let path = root.join(relative_path);
            let metadata = std::fs::metadata(&path).ok()?;
            metadata
                .is_file()
                .then_some((path, metadata_modified(&metadata)))
        })
        .collect()
}

fn source_projection_roots(mount: &MountConfig) -> Vec<PathBuf> {
    let mount_point_dir = mount_point_directory_name(mount);
    mount_access_roots(mount)
        .into_iter()
        .filter(|root| {
            root.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == mount_point_dir.as_str())
        })
        .collect()
}

fn reconcile_projection_candidate<S>(
    store: &mut S,
    mount: &MountConfig,
    entity: &EntityRecord,
    content_root: &Path,
    candidate: ProjectionCandidate,
) -> LocalityResult<ProjectionCandidateOutcome>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + VirtualMutationRepository
        + FreshnessStateRepository,
{
    let content_path = content_cache_path(content_root, &entity.path)?;
    if !projection_needs_read(&candidate.path, &content_path, candidate.force_read) {
        return Ok(ProjectionCandidateOutcome::Skipped);
    }

    let projection_contents =
        std::fs::read_to_string(&candidate.path).map_err(LocalityError::from)?;
    if has_unresolved_conflict_markers(&projection_contents)
        && let Ok(cache_contents) = std::fs::read(&content_path)
        && cache_contents != projection_contents.as_bytes()
        && projection_is_not_newer_than_cache(&candidate.path, &content_path) == Some(true)
    {
        write_binary_atomic(&candidate.path, &cache_contents).map_err(LocalityError::from)?;
        return Ok(ProjectionCandidateOutcome::Reconciled);
    }

    let commit_contents =
        projection_contents_for_existing_page(store, mount, entity, &projection_contents)?;

    if std::fs::read(&content_path).is_ok_and(|existing| existing == commit_contents) {
        if projection_contents.as_bytes() != commit_contents {
            write_binary_atomic(&candidate.path, &commit_contents).map_err(LocalityError::from)?;
            return Ok(ProjectionCandidateOutcome::Reconciled);
        }
        return Ok(ProjectionCandidateOutcome::Unchanged);
    }

    virtual_fs::commit_virtual_fs_write(
        store,
        content_root,
        &mount.mount_id,
        &entity.remote_id.0,
        &commit_contents,
    )?;
    if projection_contents.as_bytes() != commit_contents {
        write_binary_atomic(&candidate.path, &commit_contents).map_err(LocalityError::from)?;
    }
    Ok(ProjectionCandidateOutcome::Reconciled)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectionRefreshOutcome {
    MissingCache,
    MissingProjection,
    Unchanged,
    SkippedLocalChanges,
    Refreshed,
}

fn refresh_projection_candidate_if_clean(
    entity: &EntityRecord,
    content_root: &Path,
    projection_path: PathBuf,
    previous_shadow: Option<&ShadowDocument>,
) -> LocalityResult<ProjectionRefreshOutcome> {
    let content_path = content_cache_path(content_root, &entity.path)?;
    let Ok(cache_contents) = std::fs::read(&content_path) else {
        return Ok(ProjectionRefreshOutcome::MissingCache);
    };
    // Reading a macOS dataless File Provider placeholder from localityd can
    // request hydration from localityd itself and wedge the runtime thread.
    if projection_path_is_dataless_placeholder(&projection_path) {
        return Ok(ProjectionRefreshOutcome::MissingProjection);
    }
    let projection_contents = match std::fs::read(&projection_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            write_binary_atomic(&projection_path, &cache_contents).map_err(LocalityError::from)?;
            return Ok(ProjectionRefreshOutcome::Refreshed);
        }
        Err(_) => return Ok(ProjectionRefreshOutcome::MissingProjection),
    };

    if projection_contents == cache_contents {
        return Ok(ProjectionRefreshOutcome::Unchanged);
    }

    if projection_conflict_local_matches_cache(&projection_contents, &cache_contents) {
        write_binary_atomic(&projection_path, &cache_contents).map_err(LocalityError::from)?;
        return Ok(ProjectionRefreshOutcome::Refreshed);
    }

    if entity.hydration == HydrationState::Conflicted
        && std::str::from_utf8(&cache_contents).is_ok_and(has_unresolved_conflict_markers)
    {
        write_binary_atomic(&projection_path, &cache_contents).map_err(LocalityError::from)?;
        return Ok(ProjectionRefreshOutcome::Refreshed);
    }

    let can_refresh_stale_replica = previous_shadow.is_none()
        && projection_is_not_newer_than_cache(&projection_path, &content_path) == Some(true);

    if !can_refresh_stale_replica
        && !projection_contents_are_replaceable(&projection_contents, previous_shadow)
    {
        return Ok(ProjectionRefreshOutcome::SkippedLocalChanges);
    }

    write_binary_atomic(&projection_path, &cache_contents).map_err(LocalityError::from)?;
    Ok(ProjectionRefreshOutcome::Refreshed)
}

fn refresh_base_for_entity<'a>(
    refresh_bases: &'a [ProjectionRefreshBase],
    mount_id: &MountId,
    remote_id: &RemoteId,
) -> Option<&'a ShadowDocument> {
    refresh_bases
        .iter()
        .find(|base| &base.mount_id == mount_id && &base.remote_id == remote_id)
        .map(|base| &base.previous_shadow)
}

fn projection_contents_are_replaceable(
    contents: &[u8],
    previous_shadow: Option<&ShadowDocument>,
) -> bool {
    if let Some(previous_shadow) = previous_shadow {
        return projection_contents_match_shadow(contents, previous_shadow);
    }

    std::str::from_utf8(contents)
        .is_ok_and(|contents| contents.contains(CanonicalDocument::STUB_MARKER))
}

fn projection_conflict_local_matches_cache(
    projection_contents: &[u8],
    cache_contents: &[u8],
) -> bool {
    let Ok(projection_contents) = std::str::from_utf8(projection_contents) else {
        return false;
    };
    if !has_unresolved_conflict_markers(projection_contents) {
        return false;
    }

    let Some(local_contents) = local_version_from_conflict_markers(projection_contents) else {
        return false;
    };
    if local_contents.as_bytes() == cache_contents {
        return true;
    }

    let Ok(cache_contents) = std::str::from_utf8(cache_contents) else {
        return false;
    };
    let Ok(local_parsed) = parse_canonical_markdown(&local_contents) else {
        return false;
    };
    let Ok(cache_parsed) = parse_canonical_markdown(cache_contents) else {
        return false;
    };

    parsed_documents_match_ignoring_sync_metadata(&local_parsed, &cache_parsed)
}

fn projection_is_not_newer_than_cache(projection_path: &Path, content_path: &Path) -> Option<bool> {
    let projection_metadata = std::fs::metadata(projection_path).ok()?;
    let content_metadata = std::fs::metadata(content_path).ok()?;
    Some(metadata_modified(&projection_metadata) <= metadata_modified(&content_metadata))
}

fn projection_contents_match_shadow(contents: &[u8], shadow: &ShadowDocument) -> bool {
    std::str::from_utf8(contents)
        .ok()
        .and_then(|contents| parse_canonical_markdown(contents).ok())
        .is_some_and(|parsed| parsed_matches_shadow(&parsed, shadow))
}

fn projection_needs_read(projection_path: &Path, content_path: &Path, force_read: bool) -> bool {
    if projection_path_is_dataless_placeholder(projection_path) {
        return false;
    }

    if force_read {
        return true;
    }

    let Ok(projection_metadata) = std::fs::metadata(projection_path) else {
        return false;
    };
    if !projection_metadata.is_file() {
        return false;
    }

    let Ok(content_metadata) = std::fs::metadata(content_path) else {
        return true;
    };

    metadata_modified(&projection_metadata) > metadata_modified(&content_metadata)
}

fn projection_contents_for_existing_page<S>(
    store: &S,
    mount: &MountConfig,
    entity: &EntityRecord,
    contents: &str,
) -> LocalityResult<Vec<u8>>
where
    S: ShadowRepository,
{
    if has_unresolved_conflict_markers(contents) {
        return Ok(contents.as_bytes().to_vec());
    }

    let Ok(parsed) = parse_canonical_markdown(contents) else {
        return Ok(contents.as_bytes().to_vec());
    };
    if parsed.frontmatter.loc.is_some() {
        let shadow = store
            .load_shadow(&mount.mount_id, &entity.remote_id)
            .map_err(LocalityError::from)?;
        if visible_projection_base_is_stale(&parsed, &shadow) {
            if parsed_changes_retain_current_shadow_blocks(&parsed, &shadow) {
                return Ok(contents.as_bytes().to_vec());
            }
            let remote_document =
                CanonicalDocument::new(shadow.frontmatter.clone(), shadow.rendered_body.clone());
            return Ok(render_inline_conflict_markdown(contents, &remote_document).into_bytes());
        }
        return Ok(contents.as_bytes().to_vec());
    }

    let shadow = store
        .load_shadow(&mount.mount_id, &entity.remote_id)
        .map_err(LocalityError::from)?;
    let frontmatter = merge_identity_frontmatter(entity, &shadow, &parsed.document.frontmatter);
    Ok(
        render_canonical_markdown(&CanonicalDocument::new(frontmatter, parsed.document.body))
            .into_bytes(),
    )
}

fn visible_projection_base_is_stale(
    parsed: &locality_core::canonical::ParsedCanonicalDocument,
    shadow: &ShadowDocument,
) -> bool {
    let Some(visible_remote_edited_at) = parsed
        .frontmatter
        .loc
        .as_ref()
        .and_then(|loc| loc.remote_edited_at.as_deref())
    else {
        return false;
    };
    let Some(shadow_remote_edited_at) = shadow_remote_edited_at(shadow) else {
        return false;
    };

    visible_remote_edited_at != shadow_remote_edited_at
}

fn shadow_remote_edited_at(shadow: &ShadowDocument) -> Option<String> {
    parse_canonical_markdown(&render_canonical_markdown(&CanonicalDocument::new(
        shadow.frontmatter.clone(),
        "",
    )))
    .ok()
    .and_then(|parsed| parsed.frontmatter.loc.and_then(|loc| loc.remote_edited_at))
}

fn merge_identity_frontmatter(
    entity: &EntityRecord,
    shadow: &ShadowDocument,
    visible_frontmatter: &str,
) -> String {
    let mut merged = locality_identity_frontmatter(entity, shadow);
    let visible = visible_frontmatter.trim_start_matches('\n');
    if !visible.trim().is_empty() {
        if !merged.ends_with('\n') {
            merged.push('\n');
        }
        merged.push_str(visible);
        if !merged.ends_with('\n') {
            merged.push('\n');
        }
    }
    merged
}

fn locality_identity_frontmatter(entity: &EntityRecord, shadow: &ShadowDocument) -> String {
    let shadow_parsed = parse_canonical_markdown(&render_canonical_markdown(
        &CanonicalDocument::new(shadow.frontmatter.clone(), ""),
    ))
    .ok();
    let shadow_loc = shadow_parsed
        .as_ref()
        .and_then(|parsed| parsed.frontmatter.loc.as_ref());

    let id = shadow_loc
        .and_then(|loc| loc.id.as_ref())
        .unwrap_or(&entity.remote_id);
    let entity_type = shadow_loc
        .and_then(|loc| loc.raw_entity_type.as_deref())
        .map(str::to_string)
        .unwrap_or_else(|| entity_kind_frontmatter_name(&entity.kind));
    let synced_at = shadow_loc
        .and_then(|loc| loc.synced_at.as_deref())
        .or(entity.remote_edited_at.as_deref())
        .unwrap_or("unknown");
    let remote_edited_at = shadow_loc
        .and_then(|loc| loc.remote_edited_at.as_deref())
        .or(entity.remote_edited_at.as_deref())
        .unwrap_or("unknown");

    let mut frontmatter = String::new();
    frontmatter.push_str("loc:\n");
    frontmatter.push_str(&format!("  id: {}\n", yaml_quoted(&id.0)));
    frontmatter.push_str(&format!("  type: {}\n", yaml_quoted(&entity_type)));
    if let Some(parent) = shadow_loc.and_then(|loc| loc.parent.as_ref()) {
        frontmatter.push_str(&format!("  parent: {}\n", yaml_quoted(&parent.0)));
    }
    frontmatter.push_str(&format!("  synced_at: {}\n", yaml_quoted(synced_at)));
    frontmatter.push_str(&format!(
        "  remote_edited_at: {}\n",
        yaml_quoted(remote_edited_at)
    ));
    frontmatter
}

fn entity_kind_frontmatter_name(kind: &EntityKind) -> String {
    match kind {
        EntityKind::Page => "page".to_string(),
        EntityKind::Database => "database".to_string(),
        EntityKind::Directory => "directory".to_string(),
        EntityKind::Asset => "asset".to_string(),
        EntityKind::Unknown(value) => value.clone(),
    }
}

fn yaml_quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn metadata_modified(metadata: &std::fs::Metadata) -> SystemTime {
    metadata.modified().unwrap_or(UNIX_EPOCH)
}

#[cfg(target_os = "macos")]
const SF_DATALESS: u32 = 0x40000000;

#[cfg(target_os = "macos")]
fn projection_path_is_dataless_placeholder(path: &Path) -> bool {
    use std::os::darwin::fs::MetadataExt;

    std::fs::metadata(path)
        .is_ok_and(|metadata| projection_metadata_flags_are_dataless(metadata.st_flags()))
}

#[cfg(target_os = "macos")]
fn projection_metadata_flags_are_dataless(flags: u32) -> bool {
    flags & SF_DATALESS != 0
}

#[cfg(not(target_os = "macos"))]
fn projection_path_is_dataless_placeholder(_path: &Path) -> bool {
    false
}

fn content_cache_path(content_root: &Path, relative_path: &Path) -> LocalityResult<PathBuf> {
    let mut path = content_root.to_path_buf();
    for component in relative_path.components() {
        match component {
            std::path::Component::Normal(part) => path.push(part),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {
                return Err(LocalityError::InvalidState(format!(
                    "virtual content path `{}` escapes the mount root",
                    relative_path.display()
                )));
            }
        }
    }
    Ok(path)
}

fn write_binary_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("loc-file-provider-refresh");
    let temp_path = file_provider_atomic_temp_path(path, file_name);
    std::fs::write(&temp_path, contents)?;
    std::fs::rename(&temp_path, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp_path);
    })?;
    Ok(())
}

fn file_provider_atomic_temp_path(path: &Path, file_name: &str) -> PathBuf {
    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    let suffix = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(
        "{file_name}.tmp.loc-refresh-{}-{suffix}",
        std::process::id()
    ))
}

fn absolute_reconcile_path(path: &Path) -> LocalityResult<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(LocalityError::from)
    }
}

fn relative_to_access_root(path: &Path, access_root: &Path) -> Option<PathBuf> {
    if let Ok(relative_path) = path.strip_prefix(access_root) {
        let relative_path = safe_mount_relative_path(relative_path)?;
        if canonicalized_path_escapes_access_root(path, access_root) {
            return None;
        }
        return Some(relative_path);
    }

    let canonical_path = canonicalize_existing_prefix(path)?;
    let canonical_root = canonicalize_existing_prefix(access_root)?;
    canonical_path
        .strip_prefix(canonical_root)
        .ok()
        .and_then(safe_mount_relative_path)
}

fn safe_mount_relative_path(path: &Path) -> Option<PathBuf> {
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => safe.push(part),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => return None,
        }
    }
    Some(safe)
}

fn canonicalized_path_escapes_access_root(path: &Path, access_root: &Path) -> bool {
    let Some(canonical_path) = canonicalize_existing_prefix(path) else {
        return false;
    };
    let Some(canonical_root) = canonicalize_existing_prefix(access_root) else {
        return false;
    };
    !canonical_path.starts_with(canonical_root)
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

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut deduped = Vec::new();
    for path in paths {
        if !deduped.iter().any(|existing| existing == &path) {
            deduped.push(path);
        }
    }
    deduped
}

#[cfg(target_os = "macos")]
fn macos_file_provider_access_roots(mount: &MountConfig) -> Vec<PathBuf> {
    if !macos_file_provider_path_is_under_cloud_storage(&mount.root) {
        return Vec::new();
    }
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let cloud_storage = home.join("Library").join("CloudStorage");
    vec![
        cloud_storage
            .join("Locality")
            .join(mount_point_directory_name(mount)),
    ]
}

#[cfg(target_os = "macos")]
fn macos_file_provider_path_is_under_cloud_storage(path: &Path) -> bool {
    path.ancestors().any(|ancestor| {
        ancestor
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "CloudStorage")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtual_fs::virtual_projection_root;
    #[cfg(target_os = "macos")]
    use locality_core::canonical::parse_canonical_markdown;
    use locality_core::canonical::render_canonical_markdown;
    use locality_core::conflict::{
        CONFLICT_LOCAL_MARKER, CONFLICT_REMOTE_MARKER, CONFLICT_SEPARATOR_MARKER,
    };
    use locality_core::model::CanonicalDocument;
    use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId};
    use locality_core::shadow::ShadowDocument;
    use locality_store::{
        EntityRecord, EntityRepository, InMemoryStateStore, MountRepository, ProjectionMode,
        ShadowRepository,
    };
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    #[cfg(target_os = "macos")]
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn match_mount_path_resolves_relative_path_under_mount_root() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            "/tmp/Locality/Notion",
        );
        let matched = match_mount_path(&mount, Path::new("/tmp/Locality/Notion/Page.md"))
            .expect("path matches mount");

        assert_eq!(matched.access_root, PathBuf::from("/tmp/Locality/Notion"));
        assert_eq!(matched.relative_path, PathBuf::from("Page.md"));
    }

    #[test]
    fn find_mount_for_path_prefers_longest_access_root() {
        let broad = MountConfig::new(MountId::new("broad"), "notion", "/tmp/Locality");
        let narrow = MountConfig::new(MountId::new("narrow"), "notion", "/tmp/Locality/Notion");
        let mounts = vec![broad, narrow];

        let (mount, matched) =
            find_mount_for_path(&mounts, Path::new("/tmp/Locality/Notion/Page.md"))
                .expect("path matches mount");

        assert_eq!(mount.mount_id, MountId::new("narrow"));
        assert_eq!(matched.relative_path, PathBuf::from("Page.md"));
    }

    #[test]
    fn linux_fuse_mount_point_directory_is_an_access_root() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            "/tmp/Locality/notion-main",
        )
        .projection(ProjectionMode::LinuxFuse);

        let matched = match_mount_path(
            &mount,
            Path::new("/tmp/Locality/notion-main/roadmap/page.md"),
        )
        .expect("path matches mount point directory");

        assert_eq!(
            matched.access_root,
            PathBuf::from("/tmp/Locality/notion-main")
        );
        assert_eq!(matched.relative_path, PathBuf::from("roadmap/page.md"));
    }

    #[test]
    fn windows_cloud_files_mount_point_directory_is_an_access_root() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            "/tmp/Locality/notion-main",
        )
        .projection(ProjectionMode::WindowsCloudFiles);

        let matched = match_mount_path(
            &mount,
            Path::new("/tmp/Locality/notion-main/roadmap/page.md"),
        )
        .expect("path matches mount point directory");

        assert_eq!(
            matched.access_root,
            PathBuf::from("/tmp/Locality/notion-main")
        );
        assert_eq!(matched.relative_path, PathBuf::from("roadmap/page.md"));
    }

    #[test]
    fn mount_point_directory_name_uses_mount_root_basename() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            "/tmp/Locality/notion-main",
        )
        .projection(ProjectionMode::LinuxFuse);

        assert_eq!(mount_point_directory_name(&mount), "notion-main");
        assert_eq!(
            virtual_projection_root(&mount),
            PathBuf::from("/tmp/Locality")
        );
        assert_eq!(
            virtual_projection_mount_point(&mount),
            PathBuf::from("/tmp/Locality/notion-main")
        );
    }

    #[test]
    fn mount_point_directory_name_falls_back_to_mount_id_for_root_path() {
        let mount = MountConfig::new(MountId::new("notion-main"), "notion", "/")
            .projection(ProjectionMode::LinuxFuse);

        assert_eq!(mount_point_directory_name(&mount), "notion-main");
        assert_eq!(virtual_projection_root(&mount), PathBuf::from("/"));
    }

    #[test]
    fn mount_point_directory_name_preserves_space_sensitive_basenames() {
        for basename in [" .. ", " . ", " notion-main ", "notion-main "] {
            let root = PathBuf::from("/tmp/Locality").join(basename);
            let mount = MountConfig::new(MountId::new("notion-main"), "notion", &root)
                .projection(ProjectionMode::LinuxFuse);

            assert_eq!(mount_point_directory_name(&mount), basename);
            assert_eq!(virtual_projection_mount_point(&mount), root);
            assert!(mount_access_roots(&mount).contains(&root));
        }
    }

    #[test]
    fn virtual_projection_mount_point_is_access_root() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            "/tmp/Locality/notion-main",
        )
        .projection(ProjectionMode::LinuxFuse);

        let matched = match_mount_path(
            &mount,
            Path::new("/tmp/Locality/notion-main/roadmap/page.md"),
        )
        .expect("path matches mount point");

        assert_eq!(
            matched.access_root,
            PathBuf::from("/tmp/Locality/notion-main")
        );
        assert_eq!(matched.relative_path, PathBuf::from("roadmap/page.md"));
    }

    #[test]
    fn new_virtual_mount_keeps_connector_named_child_in_relative_path() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            "/tmp/Locality/notion-main",
        )
        .projection(ProjectionMode::LinuxFuse);

        let matched = match_mount_path(
            &mount,
            Path::new("/tmp/Locality/notion-main/notion/roadmap/page.md"),
        )
        .expect("path matches mount point");

        assert_eq!(
            matched.access_root,
            PathBuf::from("/tmp/Locality/notion-main")
        );
        assert_eq!(
            matched.relative_path,
            PathBuf::from("notion/roadmap/page.md")
        );
    }

    #[test]
    fn shared_macos_file_provider_domain_children_lists_virtual_mount_roots() {
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(
                MountConfig::new(
                    MountId::new("notion-main"),
                    "notion",
                    "/tmp/Locality/notion-main",
                )
                .projection(ProjectionMode::MacosFileProvider),
            )
            .expect("save notion mount");
        store
            .save_mount(
                MountConfig::new(
                    MountId::new("linear-main"),
                    "linear",
                    "/tmp/Locality/linear-main",
                )
                .projection(ProjectionMode::MacosFileProvider),
            )
            .expect("save linear mount");
        store
            .save_mount(MountConfig::new(
                MountId::new("plain"),
                "notes",
                "/tmp/Locality/notes",
            ))
            .expect("save plain mount");

        let report =
            file_provider_domain_children(&store, MACOS_FILE_PROVIDER_DOMAIN_ID).expect("children");

        assert_eq!(report.domain_id, MACOS_FILE_PROVIDER_DOMAIN_ID);
        assert_eq!(report.children.len(), 2);
        assert_eq!(report.children[0].mount_id, "linear-main");
        assert_eq!(report.children[0].item.filename, "linear-main");
        assert_eq!(report.children[0].item.identifier, "mount:linear-main");
        assert_eq!(
            report.children[0].item.parent_identifier.as_deref(),
            Some(ROOT_CONTAINER_IDENTIFIER)
        );
        assert_eq!(report.children[1].mount_id, "notion-main");
        assert_eq!(report.children[1].item.filename, "notion-main");
        assert_eq!(report.children[1].item.identifier, "mount:notion-main");
    }

    #[test]
    fn shared_macos_file_provider_domain_children_distinguish_same_connector_mount_points() {
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(
                MountConfig::new(
                    MountId::new("notion-main"),
                    "notion",
                    "/tmp/Locality/notion-main",
                )
                .projection(ProjectionMode::MacosFileProvider),
            )
            .expect("save main notion mount");
        store
            .save_mount(
                MountConfig::new(
                    MountId::new("notion-work"),
                    "notion",
                    "/tmp/Locality/notion-work",
                )
                .projection(ProjectionMode::MacosFileProvider),
            )
            .expect("save work notion mount");

        let report =
            file_provider_domain_children(&store, MACOS_FILE_PROVIDER_DOMAIN_ID).expect("children");

        let filenames = report
            .children
            .iter()
            .map(|child| child.item.filename.as_str())
            .collect::<Vec<_>>();

        assert_eq!(filenames, vec!["notion-main", "notion-work"]);
    }

    #[test]
    fn refresh_atomic_temp_name_is_supported_by_file_provider_writes() {
        let temp_path = file_provider_atomic_temp_path(Path::new("/tmp/page.md"), "page.md");
        let file_name = temp_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("temp filename");

        assert!(file_name.starts_with("page.md.tmp."));
        assert!(!file_name.starts_with('.'));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_dataless_flag_identifies_file_provider_placeholders() {
        assert!(projection_metadata_flags_are_dataless(SF_DATALESS));
        assert!(projection_metadata_flags_are_dataless(SF_DATALESS | 0x1));
        assert!(!projection_metadata_flags_are_dataless(0));
    }

    #[test]
    fn plain_mount_keeps_source_named_directory_in_relative_path() {
        let mount = MountConfig::new(MountId::new("notion-main"), "notion", "/tmp/Locality");

        let matched = match_mount_path(&mount, Path::new("/tmp/Locality/notion/roadmap/page.md"))
            .expect("path matches mount root");

        assert_eq!(matched.access_root, PathBuf::from("/tmp/Locality"));
        assert_eq!(
            matched.relative_path,
            PathBuf::from("notion/roadmap/page.md")
        );
    }

    #[test]
    fn refresh_windows_projection_from_mount_root_writes_under_mount_point_directory() {
        let root = temp_root("loc-file-provider-windows-root-refresh");
        let state_root = temp_root("loc-file-provider-windows-root-refresh-state");
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let content_path = crate::virtual_fs::virtual_fs_content_root(&state_root, &mount_id)
            .join("roadmap/page.md");
        fs::create_dir_all(content_path.parent().expect("content parent")).expect("content parent");
        fs::write(
            &content_path,
            "---\ntitle: Roadmap\n---\nPulled remote body.\n",
        )
        .expect("write cache");

        let mut store = InMemoryStateStore::new();
        store
            .save_mount(
                MountConfig::new(mount_id.clone(), "notion", &root)
                    .projection(ProjectionMode::WindowsCloudFiles),
            )
            .expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id,
                    remote_id,
                    EntityKind::Page,
                    "Roadmap",
                    "roadmap/page.md",
                )
                .with_hydration(HydrationState::Hydrated),
            )
            .expect("save entity");

        let report = refresh_visible_projection(&store, &state_root, Some(&root), &[])
            .expect("refresh projection");

        let visible_path = root.join("roadmap/page.md");
        let obsolete_connector_child_path = root.join("notion/roadmap/page.md");
        assert_eq!(report.checked, 1);
        assert_eq!(report.refreshed, 1);
        assert!(
            fs::read_to_string(visible_path)
                .expect("read mount point projection")
                .contains("Pulled remote body.")
        );
        assert!(!obsolete_connector_child_path.exists());

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(state_root);
    }

    #[test]
    fn reconcile_visible_projection_conflicts_stale_visible_edit_after_cache_fast_forward() {
        let root = temp_root("loc-file-provider-stale-visible-root");
        let state_root = temp_root("loc-file-provider-stale-visible-state");
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let relative_path = PathBuf::from("roadmap/page.md");
        let mount_root = root.join("notion");
        let visible_path = mount_root.join(&relative_path);
        let content_path =
            crate::virtual_fs::virtual_fs_content_root(&state_root, &mount_id).join(&relative_path);
        fs::create_dir_all(visible_path.parent().expect("visible parent")).expect("visible parent");
        fs::create_dir_all(content_path.parent().expect("content parent")).expect("content parent");

        let mut store = InMemoryStateStore::new();
        store
            .save_mount(
                MountConfig::new(mount_id.clone(), "notion", &mount_root)
                    .projection(ProjectionMode::WindowsCloudFiles),
            )
            .expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    remote_id.clone(),
                    EntityKind::Page,
                    "Roadmap",
                    relative_path.clone(),
                )
                .with_hydration(HydrationState::Hydrated)
                .with_remote_edited_at("remote-v2"),
            )
            .expect("save entity");
        let current_shadow = ShadowDocument::from_synced_body(
            remote_id.clone(),
            "Intro.\n\n---\n\nFooter.\n",
            8,
            [
                RemoteId::new("intro"),
                RemoteId::new("divider"),
                RemoteId::new("footer"),
            ],
        )
        .expect("current shadow")
        .with_frontmatter(versioned_frontmatter(&remote_id, "remote-v2"));
        store
            .save_shadow(&mount_id, current_shadow)
            .expect("save shadow");
        fs::write(
            &content_path,
            render_canonical_markdown(&CanonicalDocument::new(
                versioned_frontmatter(&remote_id, "remote-v2"),
                "Intro.\n\n---\n\nFooter.\n",
            )),
        )
        .expect("write fast-forwarded cache");
        let stale_visible = render_canonical_markdown(&CanonicalDocument::new(
            versioned_frontmatter(&remote_id, "remote-v1"),
            "Intro.\n\nFooter.\n\nLocal visible edit.\n",
        ));
        fs::write(&visible_path, &stale_visible).expect("write stale visible edit");

        let report = reconcile_visible_projection(&mut store, &state_root, Some(&visible_path))
            .expect("reconcile visible projection");

        assert_eq!(report.reconciled, 1);
        let cached = fs::read_to_string(&content_path).expect("read cache");
        assert!(cached.contains("Local visible edit."), "{cached}");
        assert!(cached.contains("Intro."), "{cached}");
        assert!(cached.contains("---\n\nFooter."), "{cached}");
        assert!(cached.contains(CONFLICT_LOCAL_MARKER), "{cached}");
        assert!(cached.contains(CONFLICT_SEPARATOR_MARKER), "{cached}");
        assert!(cached.contains(CONFLICT_REMOTE_MARKER), "{cached}");
        assert!(has_unresolved_conflict_markers(&cached), "{cached}");
        let visible = fs::read_to_string(&visible_path).expect("read visible");
        assert_eq!(visible, cached);
        fs::write(&visible_path, &stale_visible).expect("restore stale visible replica");
        let report = reconcile_visible_projection(&mut store, &state_root, Some(&visible_path))
            .expect("reconcile already-conflicted cache projection");
        assert_eq!(report.reconciled, 1);
        let visible = fs::read_to_string(&visible_path).expect("read visible after repair");
        assert_eq!(visible, cached);
        let report = reconcile_visible_projection(&mut store, &state_root, Some(&visible_path))
            .expect("reconcile existing conflict markers");
        assert_eq!(report.skipped_unchanged, 1);
        let cached = fs::read_to_string(&content_path).expect("read cache after marker no-op");
        assert_eq!(cached.matches(CONFLICT_LOCAL_MARKER).count(), 1, "{cached}");
        fs::write(&visible_path, &cached).expect("write older conflicted visible replica");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let clean_remote = render_canonical_markdown(&CanonicalDocument::new(
            versioned_frontmatter(&remote_id, "remote-v2"),
            "Intro.\n\n---\n\nFooter.\n",
        ));
        fs::write(&content_path, &clean_remote).expect("write newer clean cache");
        let report = reconcile_visible_projection(&mut store, &state_root, Some(&visible_path))
            .expect("refresh stale visible conflict from cache");
        assert_eq!(report.reconciled, 1);
        let visible = fs::read_to_string(&visible_path).expect("read refreshed visible");
        assert_eq!(visible, clean_remote);
        let cached = fs::read_to_string(&content_path).expect("read clean cache");
        assert_eq!(cached, clean_remote);
        let entity = store
            .get_entity(&mount_id, &remote_id)
            .expect("get entity")
            .expect("entity");
        assert_eq!(entity.hydration, HydrationState::Conflicted);

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(state_root);
    }

    #[test]
    fn reconcile_visible_projection_imports_stale_metadata_edit_when_current_blocks_are_retained() {
        let root = temp_root("loc-file-provider-stale-visible-retained-root");
        let state_root = temp_root("loc-file-provider-stale-visible-retained-state");
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let relative_path = PathBuf::from("roadmap/page.md");
        let mount_root = root.join("notion");
        let visible_path = mount_root.join(&relative_path);
        let content_path =
            crate::virtual_fs::virtual_fs_content_root(&state_root, &mount_id).join(&relative_path);
        fs::create_dir_all(visible_path.parent().expect("visible parent")).expect("visible parent");
        fs::create_dir_all(content_path.parent().expect("content parent")).expect("content parent");

        let mut store = InMemoryStateStore::new();
        store
            .save_mount(
                MountConfig::new(mount_id.clone(), "notion", &mount_root)
                    .projection(ProjectionMode::WindowsCloudFiles),
            )
            .expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    remote_id.clone(),
                    EntityKind::Page,
                    "Roadmap",
                    relative_path.clone(),
                )
                .with_hydration(HydrationState::Hydrated)
                .with_remote_edited_at("remote-v2"),
            )
            .expect("save entity");
        let current_shadow = ShadowDocument::from_synced_body(
            remote_id.clone(),
            "Intro.\n\n---<br>---\n\nFooter.\n",
            8,
            [
                RemoteId::new("intro"),
                RemoteId::new("plain-divider-text"),
                RemoteId::new("footer"),
            ],
        )
        .expect("current shadow")
        .with_frontmatter(versioned_frontmatter(&remote_id, "remote-v2"));
        store
            .save_shadow(&mount_id, current_shadow)
            .expect("save shadow");
        fs::write(
            &content_path,
            render_canonical_markdown(&CanonicalDocument::new(
                versioned_frontmatter(&remote_id, "remote-v2"),
                "Intro.\n\n---<br>---\n\nFooter.\n",
            )),
        )
        .expect("write current cache");
        fs::write(
            &visible_path,
            render_canonical_markdown(&CanonicalDocument::new(
                versioned_frontmatter(&remote_id, "remote-v1"),
                "Intro.\n\n---\n\n---\n\nFooter.\n",
            )),
        )
        .expect("write stale metadata visible edit");

        let report = reconcile_visible_projection(&mut store, &state_root, Some(&visible_path))
            .expect("reconcile visible projection");

        assert_eq!(report.reconciled, 1);
        let cached = fs::read_to_string(&content_path).expect("read cache");
        assert!(
            cached.contains("Intro.\n\n---\n\n---\n\nFooter."),
            "{cached}"
        );
        assert!(!has_unresolved_conflict_markers(&cached), "{cached}");
        let visible = fs::read_to_string(&visible_path).expect("read visible");
        assert_eq!(visible, cached);

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(state_root);
    }

    #[test]
    fn match_mount_path_rejects_parent_traversal() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            "/tmp/Locality/notion",
        );

        assert!(
            match_mount_path(&mount, Path::new("/tmp/Locality/notion/../linear/page.md")).is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn match_mount_path_rejects_symlink_escape() {
        let root = temp_root("loc-file-provider-symlink-root");
        let outside = temp_root("loc-file-provider-symlink-outside");
        let mount_root = root.join("notion");
        fs::create_dir_all(&mount_root).expect("mount root");
        fs::create_dir_all(&outside).expect("outside root");
        std::os::unix::fs::symlink(&outside, mount_root.join("escape")).expect("symlink");

        let mount = MountConfig::new(MountId::new("notion-main"), "notion", &mount_root);

        assert!(match_mount_path(&mount, &mount_root.join("escape/page.md")).is_none());

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_file_provider_access_roots_include_system_assigned_mount_point_roots() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            "/Users/test/Library/CloudStorage/Locality/notion-main",
        )
        .projection(ProjectionMode::MacosFileProvider);
        let roots = mount_access_roots(&mount);
        let home = std::env::var_os("HOME").map(PathBuf::from).expect("home");

        assert!(roots.contains(&PathBuf::from(
            "/Users/test/Library/CloudStorage/Locality/notion-main"
        )));
        assert!(
            roots.contains(
                &home
                    .join("Library")
                    .join("CloudStorage")
                    .join("Locality")
                    .join("notion-main")
            )
        );
        let matched = match_mount_path(
            &mount,
            &home
                .join("Library")
                .join("CloudStorage")
                .join("Locality")
                .join("notion-main")
                .join("Page.md"),
        )
        .expect("canonical mount point path matches");
        assert_eq!(matched.relative_path, PathBuf::from("Page.md"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reconcile_macos_projection_without_target_is_noop() {
        let fixture = ProjectionFixture::new("no-target");
        fixture.write_cache("Original body.\n");
        std::thread::sleep(Duration::from_millis(5));
        fixture.write_projection_without_identity("Original body.\n\nLocal edit.\n");

        let mut store = fixture.store();
        let report = reconcile_visible_projection(&mut store, &fixture.state_root, None)
            .expect("reconcile projection");

        assert_eq!(report, ProjectionReconcileReport::default());
        let cached = fs::read_to_string(fixture.content_path()).expect("read cache");
        assert!(!cached.contains("Local edit."));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reconcile_macos_projection_imports_explicit_visible_file_with_missing_identity() {
        let fixture = ProjectionFixture::new("newer-visible");
        fixture.write_cache("Original body.\n");
        std::thread::sleep(Duration::from_millis(5));
        fixture.write_projection_without_identity("Original body.\n\nLocal edit.\n");

        let mut store = fixture.store();
        let report = reconcile_visible_projection(
            &mut store,
            &fixture.state_root,
            Some(&fixture.projection_path()),
        )
        .expect("reconcile projection");

        assert_eq!(report.reconciled, 1);
        let cached = fs::read_to_string(fixture.content_path()).expect("read cache");
        let parsed = parse_canonical_markdown(&cached).expect("canonical cache");
        assert_eq!(parsed.remote_id(), Some(&fixture.remote_id));
        assert!(cached.contains("Local edit."));
        assert!(cached.contains("loc:"));
        let entity = store
            .get_entity(&fixture.mount_id, &fixture.remote_id)
            .expect("read entity")
            .expect("entity");
        assert_eq!(entity.hydration, HydrationState::Dirty);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reconcile_macos_projection_explicit_target_reads_even_when_cache_is_newer() {
        let fixture = ProjectionFixture::new("explicit-target");
        fixture.write_projection_without_identity("Edited body.\n");
        std::thread::sleep(Duration::from_millis(5));
        fixture.write_cache("Original body.\n");

        let mut store = fixture.store();
        let report = reconcile_visible_projection(
            &mut store,
            &fixture.state_root,
            Some(&fixture.projection_path()),
        )
        .expect("reconcile projection");

        assert_eq!(report.reconciled, 1);
        let cached = fs::read_to_string(fixture.content_path()).expect("read cache");
        assert!(cached.contains("Edited body."));
        assert!(cached.contains("loc:"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn refresh_macos_projection_copies_cache_to_visible_replica() {
        let fixture = ProjectionFixture::new("refresh-visible");
        let refresh_bases = fixture.refresh_bases();
        fixture.write_projection_from_shadow(&refresh_bases[0].previous_shadow);
        fixture.write_cache("Pulled remote body.\n");

        let store = fixture.store();
        let report = refresh_macos_file_provider_projection(
            &store,
            &fixture.state_root,
            Some(&fixture.projection_path()),
            &refresh_bases,
        )
        .expect("refresh projection");

        assert_eq!(report.checked, 1);
        assert_eq!(report.refreshed, 1);
        assert_eq!(report.skipped_unchanged, 0);
        let visible = fs::read_to_string(fixture.projection_path()).expect("read visible");
        assert!(visible.contains("Pulled remote body."));
        assert!(!visible.contains("Original body."));
        assert!(visible.contains("loc:"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn refresh_macos_projection_leaves_matching_visible_replica_unchanged() {
        let fixture = ProjectionFixture::new("refresh-unchanged");
        fixture.write_cache("Pulled remote body.\n");
        fs::copy(fixture.content_path(), fixture.projection_path()).expect("seed visible");

        let store = fixture.store();
        let report = refresh_macos_file_provider_projection(
            &store,
            &fixture.state_root,
            Some(&fixture.projection_path()),
            &fixture.refresh_bases(),
        )
        .expect("refresh projection");

        assert_eq!(report.checked, 1);
        assert_eq!(report.refreshed, 0);
        assert_eq!(report.skipped_unchanged, 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn refresh_macos_projection_skips_visible_local_changes() {
        let fixture = ProjectionFixture::new("refresh-visible-local-change");
        let refresh_bases = fixture.refresh_bases();
        fixture.write_cache("Pulled remote body.\n");
        std::thread::sleep(Duration::from_millis(5));
        fixture.write_projection_without_identity("Local visible edit.\n");

        let store = fixture.store();
        let report = refresh_macos_file_provider_projection(
            &store,
            &fixture.state_root,
            Some(&fixture.projection_path()),
            &refresh_bases,
        )
        .expect("refresh projection");

        assert_eq!(report.checked, 1);
        assert_eq!(report.refreshed, 0);
        assert_eq!(report.skipped_local_changes, 1);
        let visible = fs::read_to_string(fixture.projection_path()).expect("read visible");
        assert!(visible.contains("Local visible edit."));
        assert!(!visible.contains("Pulled remote body."));
    }

    #[test]
    fn refresh_projection_replaces_empty_conflict_when_local_matches_cache_ignoring_sync_metadata()
    {
        let root = temp_root("refresh-empty-conflict");
        let content_root = root.join("content");
        let projection_path = root.join("visible/page/page.md");
        let entity = EntityRecord::new(
            MountId::new("notion-main"),
            RemoteId::new("page-1"),
            EntityKind::Page,
            "Locality Launch",
            "page/page.md",
        )
        .with_hydration(HydrationState::Dirty);
        fs::create_dir_all(content_root.join("page")).expect("content parent");
        fs::create_dir_all(projection_path.parent().expect("projection parent"))
            .expect("projection parent");
        let body = "Shared body.\n";
        let cache = render_canonical_markdown(&CanonicalDocument::new(
            versioned_frontmatter(&entity.remote_id, "remote-v2"),
            body,
        ));
        fs::write(content_root.join("page/page.md"), &cache).expect("write cache");
        let visible = render_canonical_markdown(&CanonicalDocument::new(
            versioned_frontmatter(&entity.remote_id, "remote-v1"),
            format!(
                "{body}{CONFLICT_LOCAL_MARKER}\n{CONFLICT_SEPARATOR_MARKER}\n{CONFLICT_REMOTE_MARKER}\n"
            ),
        ));
        fs::write(&projection_path, visible).expect("write visible conflict");
        let previous_shadow = ShadowDocument::from_synced_body(
            entity.remote_id.clone(),
            body.to_string(),
            8,
            [RemoteId::new("block-1")],
        )
        .expect("shadow")
        .with_frontmatter(versioned_frontmatter(&entity.remote_id, "remote-v1"));

        let outcome = refresh_projection_candidate_if_clean(
            &entity,
            &content_root,
            projection_path.clone(),
            Some(&previous_shadow),
        )
        .expect("refresh projection");

        assert_eq!(outcome, ProjectionRefreshOutcome::Refreshed);
        assert_eq!(
            fs::read_to_string(projection_path).expect("read visible"),
            cache
        );

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn refresh_macos_entity_projection_if_clean_copies_cache_to_visible_replica() {
        let fixture = ProjectionFixture::new("refresh-entity-clean");
        let previous_shadow = fixture.previous_shadow();
        fixture.write_projection_from_shadow(&previous_shadow);
        fixture.write_cache("Pulled remote body.\n");

        let store = fixture.store();
        let report = refresh_macos_file_provider_entity_projection_if_clean(
            &store,
            &fixture.state_root,
            &fixture.mount_id,
            &fixture.remote_id,
            &previous_shadow,
        )
        .expect("refresh projection");

        assert_eq!(report.checked, 1);
        assert_eq!(report.refreshed, 1);
        assert_eq!(report.skipped_local_changes, 0);
        let visible = fs::read_to_string(fixture.projection_path()).expect("read visible");
        assert!(visible.contains("Pulled remote body."));
        assert!(!visible.contains("Original body."));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn refresh_macos_entity_projection_if_clean_skips_visible_local_changes() {
        let fixture = ProjectionFixture::new("refresh-entity-local-change");
        let previous_shadow = fixture.previous_shadow();
        fixture.write_cache("Pulled remote body.\n");
        std::thread::sleep(Duration::from_millis(5));
        fixture.write_projection_without_identity("Local visible edit.\n");

        let store = fixture.store();
        let report = refresh_macos_file_provider_entity_projection_if_clean(
            &store,
            &fixture.state_root,
            &fixture.mount_id,
            &fixture.remote_id,
            &previous_shadow,
        )
        .expect("refresh projection");

        assert_eq!(report.checked, 1);
        assert_eq!(report.refreshed, 0);
        assert_eq!(report.skipped_local_changes, 1);
        let visible = fs::read_to_string(fixture.projection_path()).expect("read visible");
        assert!(visible.contains("Local visible edit."));
        assert!(!visible.contains("Pulled remote body."));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn refresh_macos_entity_projection_if_clean_skips_older_visible_local_changes() {
        let fixture = ProjectionFixture::new("refresh-entity-older-local-change");
        let previous_shadow = fixture.previous_shadow();
        fixture.write_projection_without_identity("Local visible edit.\n");
        std::thread::sleep(Duration::from_millis(5));
        fixture.write_cache("Pulled remote body.\n");

        let store = fixture.store();
        let report = refresh_macos_file_provider_entity_projection_if_clean(
            &store,
            &fixture.state_root,
            &fixture.mount_id,
            &fixture.remote_id,
            &previous_shadow,
        )
        .expect("refresh projection");

        assert_eq!(report.checked, 1);
        assert_eq!(report.refreshed, 0);
        assert_eq!(report.skipped_local_changes, 1);
        let visible = fs::read_to_string(fixture.projection_path()).expect("read visible");
        assert!(visible.contains("Local visible edit."));
        assert!(!visible.contains("Pulled remote body."));
    }

    #[cfg(target_os = "macos")]
    struct ProjectionFixture {
        root: PathBuf,
        state_root: PathBuf,
        mount_id: MountId,
        remote_id: RemoteId,
    }

    #[cfg(target_os = "macos")]
    impl ProjectionFixture {
        fn new(name: &str) -> Self {
            let root = temp_root(&format!("loc-file-provider-reconcile-{name}"));
            let state_root = temp_root(&format!("loc-file-provider-reconcile-state-{name}"));
            let source_root = root.join("notion");
            fs::create_dir_all(source_root.join("go-to-market/loc-launch"))
                .expect("projection directories");
            fs::create_dir_all(
                crate::virtual_fs::virtual_fs_content_root(
                    &state_root,
                    &MountId::new("notion-main"),
                )
                .join("go-to-market/loc-launch"),
            )
            .expect("content directories");
            Self {
                root,
                state_root,
                mount_id: MountId::new("notion-main"),
                remote_id: RemoteId::new("page-1"),
            }
        }

        fn store(&self) -> InMemoryStateStore {
            let mut store = InMemoryStateStore::new();
            store
                .save_mount(
                    MountConfig::new(self.mount_id.clone(), "notion", self.root.join("notion"))
                        .projection(ProjectionMode::MacosFileProvider),
                )
                .expect("save mount");
            store
                .save_entity(
                    EntityRecord::new(
                        self.mount_id.clone(),
                        self.remote_id.clone(),
                        EntityKind::Page,
                        "Locality Launch",
                        "go-to-market/loc-launch/page.md",
                    )
                    .with_hydration(HydrationState::Hydrated)
                    .with_remote_edited_at("remote-v1"),
                )
                .expect("save entity");
            store
                .save_shadow(
                    &self.mount_id,
                    ShadowDocument::from_synced_body(
                        self.remote_id.clone(),
                        "Original body.\n",
                        8,
                        [RemoteId::new("block-1")],
                    )
                    .expect("shadow")
                    .with_frontmatter(frontmatter(&self.remote_id)),
                )
                .expect("save shadow");
            store
        }

        fn projection_path(&self) -> PathBuf {
            self.root
                .join("notion")
                .join("go-to-market/loc-launch/page.md")
        }

        fn content_path(&self) -> PathBuf {
            crate::virtual_fs::virtual_fs_content_root(&self.state_root, &self.mount_id)
                .join("go-to-market/loc-launch/page.md")
        }

        fn write_projection_without_identity(&self, body: &str) {
            fs::write(
                self.projection_path(),
                format!("---\ntitle: \"Locality Launch\"\n---\n{body}"),
            )
            .expect("write projection");
        }

        fn write_projection_from_shadow(&self, shadow: &ShadowDocument) {
            fs::write(
                self.projection_path(),
                render_canonical_markdown(&CanonicalDocument::new(
                    shadow.frontmatter.clone(),
                    shadow.rendered_body.clone(),
                )),
            )
            .expect("write projection");
        }

        fn write_cache(&self, body: &str) {
            fs::write(
                self.content_path(),
                render_canonical_markdown(&CanonicalDocument::new(
                    frontmatter(&self.remote_id),
                    body,
                )),
            )
            .expect("write cache");
        }

        fn previous_shadow(&self) -> ShadowDocument {
            self.store()
                .load_shadow(&self.mount_id, &self.remote_id)
                .expect("load shadow")
        }

        fn refresh_bases(&self) -> Vec<ProjectionRefreshBase> {
            vec![ProjectionRefreshBase {
                mount_id: self.mount_id.clone(),
                remote_id: self.remote_id.clone(),
                previous_shadow: self.previous_shadow(),
            }]
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for ProjectionFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
            let _ = fs::remove_dir_all(&self.state_root);
        }
    }

    #[cfg(target_os = "macos")]
    fn frontmatter(remote_id: &RemoteId) -> String {
        versioned_frontmatter(remote_id, "remote-v1")
    }

    fn versioned_frontmatter(remote_id: &RemoteId, version: &str) -> String {
        format!(
            "loc:\n  id: {}\n  type: page\n  synced_at: {version}\n  remote_edited_at: {version}\ntitle: \"Locality Launch\"\n",
            remote_id.0,
        )
    }

    fn temp_root(prefix: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("{prefix}-{}-{unique}-{suffix}", std::process::id()))
    }
}
