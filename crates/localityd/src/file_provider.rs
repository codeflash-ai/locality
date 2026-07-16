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

use crate::durable_fs::{
    create_dir_all_durable, remove_path_durable, rename_noreplace_durable, write_new_file_durable,
};
use crate::hydration::HydrationSource;
use crate::shadow_match::{
    parsed_changes_retain_current_shadow_blocks, parsed_documents_match_ignoring_sync_metadata,
    parsed_matches_shadow,
};
use crate::virtual_fs;
use crate::virtual_fs::{
    mount_point_directory_name, mount_point_identifier, source_root_read_only,
    virtual_projection_mount_point,
};
use crate::virtual_projection::wrap_identifier;

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

pub fn macos_file_provider_item_identifier(mount_id: &str, identifier: &str) -> String {
    if identifier == ROOT_CONTAINER_IDENTIFIER {
        return ROOT_CONTAINER_IDENTIFIER.to_string();
    }
    wrap_identifier(&MountId::new(mount_id), identifier)
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
        read_only: source_root_read_only(mount),
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
    pub recovery_paths: Vec<PathBuf>,
}

const WINDOWS_CLOUD_FILES_PROJECTION_ACK_VERSION: u32 = 1;
const WINDOWS_CLOUD_FILES_PROJECTION_ACK_MAX_AGE_MS: u64 = 5 * 60 * 1_000;
const WINDOWS_CLOUD_FILES_RECOVERY_STATE_VERSION: u32 = 2;
const WINDOWS_CLOUD_FILES_RECOVERY_MIN_READER_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowsCloudFilesProjectionEvent {
    CloudFilesRenameTarget,
    CloudFilesDeleteMoveSource,
    WatcherRemoveMoveSource,
    CloudFilesDeleteArchivedEntity,
    WatcherRemoveArchivedEntity,
    CloudFilesQuarantineMoveSource,
    WatcherQuarantineMoveSource,
    CloudFilesQuarantineArchiveSource,
    WatcherQuarantineArchiveSource,
}

impl WindowsCloudFilesProjectionEvent {
    fn as_str(self) -> &'static str {
        match self {
            Self::CloudFilesRenameTarget => "cloud_files_rename_target",
            Self::CloudFilesDeleteMoveSource => "cloud_files_delete_move_source",
            Self::WatcherRemoveMoveSource => "watcher_remove_move_source",
            Self::CloudFilesDeleteArchivedEntity => "cloud_files_delete_archived_entity",
            Self::WatcherRemoveArchivedEntity => "watcher_remove_archived_entity",
            Self::CloudFilesQuarantineMoveSource => "cloud_files_quarantine_move_source",
            Self::WatcherQuarantineMoveSource => "watcher_quarantine_move_source",
            Self::CloudFilesQuarantineArchiveSource => "cloud_files_quarantine_archive_source",
            Self::WatcherQuarantineArchiveSource => "watcher_quarantine_archive_source",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct WindowsCloudFilesProjectionAcknowledgement {
    version: u32,
    mount_id: MountId,
    entity_id: RemoteId,
    provider_identifier: String,
    access_root_key: String,
    relative_path_key: String,
    event: WindowsCloudFilesProjectionEvent,
    expected_entity_path_key: Option<String>,
    #[serde(default)]
    quarantine_path: Option<PathBuf>,
    created_at_unix_ms: u64,
}

#[derive(Clone, Copy)]
struct WindowsCloudFilesProjectionAcknowledgementSpec<'a> {
    provider_identifier: &'a str,
    relative_path: &'a Path,
    event: WindowsCloudFilesProjectionEvent,
    expected_entity_path: Option<&'a Path>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowsCloudFilesProjectionRecoveryOperation {
    Move,
    Archive,
    Orphan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowsCloudFilesProjectionRecoveryPayloadKind {
    File,
    PageContainer,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowsCloudFilesProjectionRecoveryStatus {
    Prepared,
    QuarantinedClean,
    NeedsReview,
    SourcePresent,
    Missing,
    Orphaned,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowsCloudFilesProjectionRecovery {
    pub state_version: u32,
    pub min_reader_version: u32,
    pub recovery_id: String,
    pub record_revision: u32,
    pub mount_id: Option<MountId>,
    pub entity_id: Option<RemoteId>,
    pub operation: WindowsCloudFilesProjectionRecoveryOperation,
    pub payload_kind: WindowsCloudFilesProjectionRecoveryPayloadKind,
    pub status: WindowsCloudFilesProjectionRecoveryStatus,
    #[serde(default)]
    pub provider_root: Option<PathBuf>,
    pub source_access_root: Option<PathBuf>,
    pub source_relative_path: Option<PathBuf>,
    pub source_path: Option<PathBuf>,
    pub intended_entity_path: Option<PathBuf>,
    pub quarantine_path: PathBuf,
    pub payload_document_relative_path: Option<PathBuf>,
    pub payload_byte_size: Option<u64>,
    pub payload_hash: Option<String>,
    pub unexpected_entries: Vec<PathBuf>,
    pub review_reason: Option<String>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
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
        let target_remote_id = target_visible_remote_id(&target);
        let entities = scoped_page_entities(
            store,
            &mount,
            Some(&target_match),
            target_remote_id.as_ref(),
        )?;
        for mut entity in entities {
            if let Some(rehomed) = rehome_visible_entity_path_if_safe(
                store,
                &mount,
                &entity,
                &target,
                &target_match.relative_path,
                target_remote_id.as_ref(),
            )? {
                entity = rehomed;
            }
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

/// Quarantines an already-materialized Windows Cloud Files replica after durable
/// entity state has moved. Provider enumeration rematerializes the new path from
/// authoritative entity and cache state.
pub fn reconcile_windows_cloud_files_entity_projection_if_clean<S>(
    store: &S,
    state_root: &Path,
    mount_id: &MountId,
    remote_id: &RemoteId,
    previous_path: &Path,
    previous_shadow: &ShadowDocument,
) -> LocalityResult<ProjectionRefreshReport>
where
    S: MountRepository + EntityRepository,
{
    let Some(mount) = store.get_mount(mount_id).map_err(LocalityError::from)? else {
        return Ok(ProjectionRefreshReport::default());
    };
    if mount.projection != ProjectionMode::WindowsCloudFiles {
        return Ok(ProjectionRefreshReport::default());
    }
    let Some(entity) = store
        .get_entity(mount_id, remote_id)
        .map_err(LocalityError::from)?
    else {
        return Ok(ProjectionRefreshReport::default());
    };
    if entity.kind != EntityKind::Page {
        return Ok(ProjectionRefreshReport::default());
    }
    if entity.path == previous_path {
        let content_root = virtual_fs::virtual_fs_content_root(state_root, mount_id);
        let mut report = ProjectionRefreshReport::default();
        let candidates = existing_projection_paths(&mount, &entity.path);
        if candidates.is_empty() {
            report.skipped_missing_projection += 1;
            return Ok(report);
        }
        for candidate in candidates {
            record_windows_projection_refresh_outcome(
                &mut report,
                refresh_windows_projection_candidate_if_clean(
                    &entity,
                    &content_root,
                    &candidate,
                    previous_shadow,
                )?,
            );
        }
        return Ok(report);
    }

    let content_root = virtual_fs::virtual_fs_content_root(state_root, mount_id);
    let mut report = ProjectionRefreshReport::default();
    for root in source_projection_roots(&mount) {
        let previous_projection_path = root.join(previous_path);
        let previous_contents = match std::fs::read(&previous_projection_path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                report.skipped_missing_projection += 1;
                continue;
            }
            Err(error) => return Err(LocalityError::from(error)),
        };
        report.checked += 1;
        if !projection_contents_are_replaceable(&previous_contents, Some(previous_shadow)) {
            report.skipped_local_changes += 1;
            continue;
        }
        let restored_projection_path = root.join(&entity.path);
        let previous_namespace_path = projection_namespace_path(&previous_projection_path);
        let restored_namespace_path = projection_namespace_path(&restored_projection_path);
        if restored_namespace_path.exists() {
            return Err(LocalityError::InvalidState(format!(
                "Windows Cloud Files undo destination `{}` already exists",
                restored_namespace_path.display()
            )));
        }
        let refresh_outcome = prepare_windows_projection_refresh(
            &entity,
            &content_root,
            &restored_projection_path,
            previous_shadow,
        )?;
        let Some(_cache_contents) = refresh_outcome else {
            report.skipped_missing_cache += 1;
            continue;
        };
        let page_container = previous_path
            .file_name()
            .is_some_and(|filename| filename == "page.md");
        if page_container
            && !windows_projection_container_contains_only(
                &previous_namespace_path,
                &previous_projection_path,
            )
            .map_err(LocalityError::from)?
        {
            return Err(LocalityError::InvalidState(format!(
                "Windows Cloud Files undo cannot quarantine nonempty page container `{}`",
                previous_namespace_path.display()
            )));
        }
        let previous_namespace_relative_path = projection_namespace_path(previous_path);
        let provider_identifier =
            windows_cloud_files_projection_identifier(remote_id, previous_path);
        let mut acknowledgements = vec![
            WindowsCloudFilesProjectionAcknowledgementSpec {
                provider_identifier: &provider_identifier,
                relative_path: &previous_namespace_relative_path,
                event: WindowsCloudFilesProjectionEvent::CloudFilesQuarantineMoveSource,
                expected_entity_path: Some(&entity.path),
            },
            WindowsCloudFilesProjectionAcknowledgementSpec {
                provider_identifier: &provider_identifier,
                relative_path: &previous_namespace_relative_path,
                event: WindowsCloudFilesProjectionEvent::WatcherQuarantineMoveSource,
                expected_entity_path: Some(&entity.path),
            },
        ];
        if page_container {
            acknowledgements.extend([
                WindowsCloudFilesProjectionAcknowledgementSpec {
                    provider_identifier: &remote_id.0,
                    relative_path: previous_path,
                    event: WindowsCloudFilesProjectionEvent::CloudFilesQuarantineMoveSource,
                    expected_entity_path: Some(&entity.path),
                },
                WindowsCloudFilesProjectionAcknowledgementSpec {
                    provider_identifier: &remote_id.0,
                    relative_path: previous_path,
                    event: WindowsCloudFilesProjectionEvent::WatcherQuarantineMoveSource,
                    expected_entity_path: Some(&entity.path),
                },
            ]);
        }
        let recovery = quarantine_windows_cloud_files_projection_namespace(
            state_root,
            &root,
            mount_id,
            remote_id,
            WindowsCloudFilesProjectionRecoveryOperation::Move,
            if page_container {
                WindowsCloudFilesProjectionRecoveryPayloadKind::PageContainer
            } else {
                WindowsCloudFilesProjectionRecoveryPayloadKind::File
            },
            &previous_namespace_path,
            &previous_namespace_relative_path,
            page_container.then(|| Path::new("page.md")),
            Some(&entity.path),
            previous_shadow,
            &acknowledgements,
        )?;
        report.recovery_paths.push(recovery.quarantine_path);
        report.refreshed += 1;
    }
    Ok(report)
}

/// Quarantines an already-materialized Windows Cloud Files replica after its
/// created remote entity has been archived and removed from durable state.
pub fn remove_windows_cloud_files_entity_projection_if_clean(
    state_root: &Path,
    mount: &MountConfig,
    entity_id: &RemoteId,
    previous_path: &Path,
    previous_shadow: &ShadowDocument,
) -> LocalityResult<ProjectionRefreshReport> {
    if mount.projection != ProjectionMode::WindowsCloudFiles {
        return Ok(ProjectionRefreshReport::default());
    }

    let mut report = ProjectionRefreshReport::default();
    for root in source_projection_roots(mount) {
        let projection_path = root.join(previous_path);
        let contents = match std::fs::read(&projection_path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                report.skipped_missing_projection += 1;
                continue;
            }
            Err(error) => return Err(LocalityError::from(error)),
        };
        report.checked += 1;
        if !projection_contents_are_replaceable(&contents, Some(previous_shadow)) {
            report.skipped_local_changes += 1;
            continue;
        }
        let page_container = previous_path
            .file_name()
            .is_some_and(|filename| filename == "page.md");
        let namespace_path = projection_namespace_path(&projection_path);
        let namespace_relative_path = projection_namespace_path(previous_path);
        if page_container
            && !windows_projection_container_contains_only(&namespace_path, &projection_path)
                .map_err(LocalityError::from)?
        {
            return Err(LocalityError::InvalidState(format!(
                "Windows Cloud Files undo cannot quarantine nonempty page container `{}`",
                namespace_path.display()
            )));
        }
        let mut acknowledgements = vec![
            WindowsCloudFilesProjectionAcknowledgementSpec {
                provider_identifier: &entity_id.0,
                relative_path: previous_path,
                event: WindowsCloudFilesProjectionEvent::CloudFilesQuarantineArchiveSource,
                expected_entity_path: None,
            },
            WindowsCloudFilesProjectionAcknowledgementSpec {
                provider_identifier: &entity_id.0,
                relative_path: previous_path,
                event: WindowsCloudFilesProjectionEvent::WatcherQuarantineArchiveSource,
                expected_entity_path: None,
            },
        ];
        let container_identifier = format!("children:{}", entity_id.0);
        if page_container {
            acknowledgements.extend([
                WindowsCloudFilesProjectionAcknowledgementSpec {
                    provider_identifier: &container_identifier,
                    relative_path: &namespace_relative_path,
                    event: WindowsCloudFilesProjectionEvent::CloudFilesQuarantineArchiveSource,
                    expected_entity_path: None,
                },
                WindowsCloudFilesProjectionAcknowledgementSpec {
                    provider_identifier: &container_identifier,
                    relative_path: &namespace_relative_path,
                    event: WindowsCloudFilesProjectionEvent::WatcherQuarantineArchiveSource,
                    expected_entity_path: None,
                },
            ]);
        }
        let recovery = quarantine_windows_cloud_files_projection_namespace(
            state_root,
            &root,
            &mount.mount_id,
            entity_id,
            WindowsCloudFilesProjectionRecoveryOperation::Archive,
            if page_container {
                WindowsCloudFilesProjectionRecoveryPayloadKind::PageContainer
            } else {
                WindowsCloudFilesProjectionRecoveryPayloadKind::File
            },
            &namespace_path,
            &namespace_relative_path,
            page_container.then(|| Path::new("page.md")),
            None,
            previous_shadow,
            &acknowledgements,
        )?;
        report.recovery_paths.push(recovery.quarantine_path);
        report.refreshed += 1;
    }
    Ok(report)
}

pub fn record_windows_cloud_files_projection_acknowledgement(
    state_root: &Path,
    access_root: &Path,
    mount_id: &MountId,
    entity_id: &RemoteId,
    provider_identifier: &str,
    relative_path: &Path,
    event: WindowsCloudFilesProjectionEvent,
    expected_entity_path: Option<&Path>,
) -> LocalityResult<()> {
    record_windows_cloud_files_projection_acknowledgement_at(
        state_root,
        access_root,
        mount_id,
        entity_id,
        provider_identifier,
        relative_path,
        event,
        expected_entity_path,
        None,
        current_unix_millis(),
    )
}

#[cfg(test)]
fn record_windows_cloud_files_projection_acknowledgements(
    state_root: &Path,
    access_root: &Path,
    mount_id: &MountId,
    entity_id: &RemoteId,
    acknowledgements: &[WindowsCloudFilesProjectionAcknowledgementSpec<'_>],
) -> LocalityResult<()> {
    let mut recorded = Vec::with_capacity(acknowledgements.len());
    for acknowledgement in acknowledgements {
        if let Err(error) = record_windows_cloud_files_projection_acknowledgement(
            state_root,
            access_root,
            mount_id,
            entity_id,
            acknowledgement.provider_identifier,
            acknowledgement.relative_path,
            acknowledgement.event,
            acknowledgement.expected_entity_path,
        ) {
            revoke_windows_cloud_files_projection_acknowledgements(
                state_root,
                access_root,
                mount_id,
                &recorded,
            );
            return Err(error);
        }
        recorded.push(*acknowledgement);
    }
    Ok(())
}

fn record_windows_cloud_files_projection_acknowledgements_for_quarantine(
    state_root: &Path,
    access_root: &Path,
    mount_id: &MountId,
    entity_id: &RemoteId,
    acknowledgements: &[WindowsCloudFilesProjectionAcknowledgementSpec<'_>],
    quarantine_path: &Path,
) -> LocalityResult<()> {
    let mut recorded = Vec::with_capacity(acknowledgements.len());
    for acknowledgement in acknowledgements {
        let acknowledgement_quarantine_path = if acknowledgement
            .relative_path
            .file_name()
            .is_some_and(|filename| filename == "page.md")
        {
            quarantine_path.join("page.md")
        } else {
            quarantine_path.to_path_buf()
        };
        if let Err(error) = record_windows_cloud_files_projection_acknowledgement_at(
            state_root,
            access_root,
            mount_id,
            entity_id,
            acknowledgement.provider_identifier,
            acknowledgement.relative_path,
            acknowledgement.event,
            acknowledgement.expected_entity_path,
            Some(&acknowledgement_quarantine_path),
            current_unix_millis(),
        ) {
            revoke_windows_cloud_files_projection_acknowledgements(
                state_root,
                access_root,
                mount_id,
                &recorded,
            );
            return Err(error);
        }
        recorded.push(*acknowledgement);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn record_windows_cloud_files_projection_acknowledgement_at(
    state_root: &Path,
    access_root: &Path,
    mount_id: &MountId,
    entity_id: &RemoteId,
    provider_identifier: &str,
    relative_path: &Path,
    event: WindowsCloudFilesProjectionEvent,
    expected_entity_path: Option<&Path>,
    quarantine_path: Option<&Path>,
    created_at_unix_ms: u64,
) -> LocalityResult<()> {
    validate_windows_projection_relative_path(relative_path)?;
    if let Some(expected_entity_path) = expected_entity_path {
        validate_windows_projection_relative_path(expected_entity_path)?;
    }
    if let Some(quarantine_path) = quarantine_path
        && (!quarantine_path.is_absolute() || quarantine_path.starts_with(access_root))
    {
        return Err(LocalityError::InvalidState(format!(
            "Windows projection quarantine target `{}` must be absolute and outside access root `{}`",
            quarantine_path.display(),
            access_root.display()
        )));
    }
    repair_windows_cloud_files_projection_acknowledgements(state_root, current_unix_millis());
    let acknowledgement = WindowsCloudFilesProjectionAcknowledgement {
        version: WINDOWS_CLOUD_FILES_PROJECTION_ACK_VERSION,
        mount_id: mount_id.clone(),
        entity_id: entity_id.clone(),
        provider_identifier: normalize_windows_cloud_files_provider_identifier(provider_identifier),
        access_root_key: windows_projection_path_key(access_root),
        relative_path_key: windows_projection_path_key(relative_path),
        event,
        expected_entity_path_key: expected_entity_path.map(windows_projection_path_key),
        quarantine_path: quarantine_path.map(Path::to_path_buf),
        created_at_unix_ms,
    };
    let path = windows_cloud_files_projection_acknowledgement_path(
        state_root,
        access_root,
        mount_id,
        provider_identifier,
        relative_path,
        event,
    );
    let parent = path.parent().ok_or_else(|| {
        LocalityError::InvalidState("Windows projection acknowledgement path has no parent".into())
    })?;
    std::fs::create_dir_all(parent).map_err(LocalityError::from)?;
    let bytes = serde_json::to_vec(&acknowledgement)
        .map_err(|error| LocalityError::InvalidState(error.to_string()))?;
    let temporary = windows_projection_acknowledgement_temporary_path(&path, "write");
    std::fs::write(&temporary, bytes).map_err(LocalityError::from)?;
    if path.exists() {
        std::fs::remove_file(&path).map_err(LocalityError::from)?;
    }
    if let Err(error) = std::fs::rename(&temporary, &path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(LocalityError::from(error));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn consume_windows_cloud_files_projection_acknowledgement(
    state_root: &Path,
    access_root: &Path,
    mount_id: &MountId,
    entity_id: &RemoteId,
    provider_identifier: &str,
    relative_path: &Path,
    event: WindowsCloudFilesProjectionEvent,
    current_entity: Option<&EntityRecord>,
) -> bool {
    consume_windows_cloud_files_projection_acknowledgement_inner(
        state_root,
        access_root,
        mount_id,
        entity_id,
        provider_identifier,
        relative_path,
        event,
        current_entity,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn consume_windows_cloud_files_quarantine_acknowledgement(
    state_root: &Path,
    access_root: &Path,
    mount_id: &MountId,
    entity_id: &RemoteId,
    provider_identifier: &str,
    relative_path: &Path,
    event: WindowsCloudFilesProjectionEvent,
    current_entity: Option<&EntityRecord>,
    observed_quarantine_path: Option<&Path>,
) -> bool {
    consume_windows_cloud_files_projection_acknowledgement_inner(
        state_root,
        access_root,
        mount_id,
        entity_id,
        provider_identifier,
        relative_path,
        event,
        current_entity,
        observed_quarantine_path,
    )
}

#[allow(clippy::too_many_arguments)]
fn consume_windows_cloud_files_projection_acknowledgement_inner(
    state_root: &Path,
    access_root: &Path,
    mount_id: &MountId,
    entity_id: &RemoteId,
    provider_identifier: &str,
    relative_path: &Path,
    event: WindowsCloudFilesProjectionEvent,
    current_entity: Option<&EntityRecord>,
    observed_quarantine_path: Option<&Path>,
) -> bool {
    let now = current_unix_millis();
    repair_windows_cloud_files_projection_acknowledgements(state_root, now);
    let path = windows_cloud_files_projection_acknowledgement_path(
        state_root,
        access_root,
        mount_id,
        provider_identifier,
        relative_path,
        event,
    );
    let acknowledgement_before_claim = std::fs::read(&path).ok().and_then(|bytes| {
        serde_json::from_slice::<WindowsCloudFilesProjectionAcknowledgement>(&bytes).ok()
    });
    if acknowledgement_before_claim
        .as_ref()
        .is_some_and(|acknowledgement| acknowledgement.quarantine_path.is_some())
        && !acknowledgement_before_claim
            .as_ref()
            .is_some_and(|acknowledgement| {
                windows_projection_quarantine_acknowledgement_proof_matches(
                    acknowledgement,
                    event,
                    observed_quarantine_path,
                )
            })
    {
        return false;
    }
    let claimed = windows_projection_acknowledgement_temporary_path(&path, "claim");
    if std::fs::rename(&path, &claimed).is_err() {
        return false;
    }
    let acknowledgement = std::fs::read(&claimed).ok().and_then(|bytes| {
        serde_json::from_slice::<WindowsCloudFilesProjectionAcknowledgement>(&bytes).ok()
    });
    let Some(acknowledgement) = acknowledgement else {
        let _ = std::fs::remove_file(&claimed);
        return false;
    };
    let durable_state_matches = match acknowledgement.expected_entity_path_key.as_deref() {
        Some(expected_path_key) => current_entity.is_some_and(|entity| {
            entity.mount_id == *mount_id
                && entity.remote_id == *entity_id
                && windows_projection_path_key(&entity.path) == expected_path_key
        }),
        None => current_entity.is_none(),
    };
    let valid = windows_projection_acknowledgement_is_current(&acknowledgement, now)
        && acknowledgement.version == WINDOWS_CLOUD_FILES_PROJECTION_ACK_VERSION
        && acknowledgement.mount_id == *mount_id
        && acknowledgement.entity_id == *entity_id
        && acknowledgement.provider_identifier
            == normalize_windows_cloud_files_provider_identifier(provider_identifier)
        && acknowledgement.access_root_key == windows_projection_path_key(access_root)
        && acknowledgement.relative_path_key == windows_projection_path_key(relative_path)
        && acknowledgement.event == event
        && durable_state_matches
        && windows_projection_quarantine_acknowledgement_proof_matches(
            &acknowledgement,
            event,
            observed_quarantine_path,
        );
    if !valid && acknowledgement.quarantine_path.is_some() {
        if std::fs::rename(&claimed, &path).is_err() {
            let _ = std::fs::remove_file(&claimed);
        }
        return false;
    }
    let _ = std::fs::remove_file(&claimed);
    valid
}

fn windows_projection_quarantine_acknowledgement_proof_matches(
    acknowledgement: &WindowsCloudFilesProjectionAcknowledgement,
    event: WindowsCloudFilesProjectionEvent,
    observed_quarantine_path: Option<&Path>,
) -> bool {
    let Some(quarantine_path) = acknowledgement.quarantine_path.as_deref() else {
        return !matches!(
            event,
            WindowsCloudFilesProjectionEvent::CloudFilesQuarantineMoveSource
                | WindowsCloudFilesProjectionEvent::WatcherQuarantineMoveSource
                | WindowsCloudFilesProjectionEvent::CloudFilesQuarantineArchiveSource
                | WindowsCloudFilesProjectionEvent::WatcherQuarantineArchiveSource
        );
    };
    if matches!(
        event,
        WindowsCloudFilesProjectionEvent::CloudFilesQuarantineMoveSource
            | WindowsCloudFilesProjectionEvent::CloudFilesQuarantineArchiveSource
    ) && observed_quarantine_path.is_none()
    {
        return false;
    }
    if observed_quarantine_path.is_some_and(|observed| {
        windows_projection_path_key(observed) != windows_projection_path_key(quarantine_path)
    }) {
        return false;
    }
    quarantine_path.try_exists().unwrap_or(false)
}

pub fn list_windows_cloud_files_projection_recoveries(
    state_root: &Path,
) -> LocalityResult<Vec<WindowsCloudFilesProjectionRecovery>> {
    let directory = windows_cloud_files_projection_recovery_manifest_dir(state_root);
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(LocalityError::from(error)),
    };
    let mut latest =
        std::collections::BTreeMap::<String, WindowsCloudFilesProjectionRecovery>::new();
    for entry in entries {
        let entry = entry.map_err(LocalityError::from)?;
        let path = entry.path();
        if !entry.file_type().map_err(LocalityError::from)?.is_file()
            || path.extension().and_then(|extension| extension.to_str()) != Some("json")
        {
            continue;
        }
        let bytes = std::fs::read(&path).map_err(LocalityError::from)?;
        let recovery = serde_json::from_slice::<WindowsCloudFilesProjectionRecovery>(&bytes)
            .map_err(|error| {
                LocalityError::InvalidState(format!(
                    "Windows Cloud Files recovery manifest `{}` is malformed: {error}",
                    path.display()
                ))
            })?;
        validate_windows_cloud_files_projection_recovery_version(&recovery, &path)?;
        let replace = latest
            .get(&recovery.recovery_id)
            .is_none_or(|current| recovery.record_revision > current.record_revision);
        if replace {
            latest.insert(recovery.recovery_id.clone(), recovery);
        }
    }
    Ok(latest.into_values().collect())
}

pub fn repair_windows_cloud_files_projection_recoveries(
    state_root: &Path,
    access_root: &Path,
) -> LocalityResult<Vec<WindowsCloudFilesProjectionRecovery>> {
    let recoveries = list_windows_cloud_files_projection_recoveries(state_root)?;
    let now = current_unix_millis();
    let access_root_key = windows_projection_path_key(access_root);
    for recovery in &recoveries {
        if recovery
            .source_access_root
            .as_deref()
            .is_some_and(|root| windows_projection_path_key(root) != access_root_key)
        {
            continue;
        }
        let source_exists = recovery
            .source_path
            .as_deref()
            .map(Path::try_exists)
            .transpose()
            .map_err(LocalityError::from)?
            .unwrap_or(false);
        let quarantine_exists = recovery
            .quarantine_path
            .try_exists()
            .map_err(LocalityError::from)?;
        if recovery.status == WindowsCloudFilesProjectionRecoveryStatus::Prepared {
            let mut repaired = recovery.clone();
            repaired.record_revision = repaired.record_revision.saturating_add(1);
            repaired.updated_at_unix_ms = now;
            match (source_exists, quarantine_exists) {
                (true, false) => {
                    repaired.status = WindowsCloudFilesProjectionRecoveryStatus::SourcePresent;
                    repaired.review_reason =
                        Some("prepared recovery did not rename the source namespace".to_string());
                }
                (_, true) => {
                    repaired.status = WindowsCloudFilesProjectionRecoveryStatus::NeedsReview;
                    repaired.review_reason = Some(
                        "recovered a quarantine payload after an interrupted namespace rename"
                            .to_string(),
                    );
                    inspect_windows_cloud_files_projection_recovery(&mut repaired);
                }
                (false, false) => {
                    repaired.status = WindowsCloudFilesProjectionRecoveryStatus::Missing;
                    repaired.review_reason = Some(
                        "prepared recovery has neither its source nor quarantine payload"
                            .to_string(),
                    );
                }
            }
            persist_windows_cloud_files_projection_recovery(state_root, &repaired)?;
            continue;
        }
        if recovery.status == WindowsCloudFilesProjectionRecoveryStatus::QuarantinedClean {
            let mut inspected = recovery.clone();
            if !quarantine_exists {
                inspected.status = WindowsCloudFilesProjectionRecoveryStatus::Missing;
                inspected.review_reason =
                    Some("quarantine payload is missing from durable recovery storage".to_string());
            } else {
                let previous_hash = inspected.payload_hash.clone();
                let previous_size = inspected.payload_byte_size;
                let previous_unexpected = inspected.unexpected_entries.clone();
                inspect_windows_cloud_files_projection_recovery(&mut inspected);
                if inspected.payload_hash == previous_hash
                    && inspected.payload_byte_size == previous_size
                    && inspected.unexpected_entries == previous_unexpected
                {
                    continue;
                }
                inspected.status = WindowsCloudFilesProjectionRecoveryStatus::NeedsReview;
                inspected.review_reason = Some(
                    "quarantine payload changed after its clean recovery record was written"
                        .to_string(),
                );
            }
            inspected.record_revision = inspected.record_revision.saturating_add(1);
            inspected.updated_at_unix_ms = now;
            persist_windows_cloud_files_projection_recovery(state_root, &inspected)?;
        }
    }

    let indexed = list_windows_cloud_files_projection_recoveries(state_root)?
        .into_iter()
        .map(|recovery| windows_projection_path_key(&recovery.quarantine_path))
        .collect::<std::collections::BTreeSet<_>>();
    let provider_root = windows_cloud_files_projection_provider_root(access_root)?;
    let quarantine_root = windows_cloud_files_projection_quarantine_root(access_root)?;
    match std::fs::read_dir(&quarantine_root) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(LocalityError::from)?;
                let path = entry.path();
                if indexed.contains(&windows_projection_path_key(&path)) {
                    continue;
                }
                let metadata = entry.metadata().map_err(LocalityError::from)?;
                let mut orphan = WindowsCloudFilesProjectionRecovery {
                    state_version: WINDOWS_CLOUD_FILES_RECOVERY_STATE_VERSION,
                    min_reader_version: WINDOWS_CLOUD_FILES_RECOVERY_MIN_READER_VERSION,
                    recovery_id: format!(
                        "orphan-{:016x}",
                        stable_projection_ack_hash(&windows_projection_path_key(&path))
                    ),
                    record_revision: 1,
                    mount_id: None,
                    entity_id: None,
                    operation: WindowsCloudFilesProjectionRecoveryOperation::Orphan,
                    payload_kind: if metadata.is_dir() {
                        WindowsCloudFilesProjectionRecoveryPayloadKind::PageContainer
                    } else {
                        WindowsCloudFilesProjectionRecoveryPayloadKind::Unknown
                    },
                    status: WindowsCloudFilesProjectionRecoveryStatus::Orphaned,
                    provider_root: Some(provider_root.clone()),
                    source_access_root: None,
                    source_relative_path: None,
                    source_path: None,
                    intended_entity_path: None,
                    quarantine_path: path,
                    payload_document_relative_path: None,
                    payload_byte_size: None,
                    payload_hash: None,
                    unexpected_entries: Vec::new(),
                    review_reason: Some(
                        "quarantine payload had no durable recovery manifest".to_string(),
                    ),
                    created_at_unix_ms: now,
                    updated_at_unix_ms: now,
                };
                inspect_windows_cloud_files_projection_recovery(&mut orphan);
                persist_windows_cloud_files_projection_recovery(state_root, &orphan)?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(LocalityError::from(error)),
    }
    list_windows_cloud_files_projection_recoveries(state_root)
}

#[allow(clippy::too_many_arguments)]
fn quarantine_windows_cloud_files_projection_namespace(
    state_root: &Path,
    access_root: &Path,
    mount_id: &MountId,
    entity_id: &RemoteId,
    operation: WindowsCloudFilesProjectionRecoveryOperation,
    payload_kind: WindowsCloudFilesProjectionRecoveryPayloadKind,
    source_path: &Path,
    source_relative_path: &Path,
    payload_document_relative_path: Option<&Path>,
    intended_entity_path: Option<&Path>,
    previous_shadow: &ShadowDocument,
    acknowledgements: &[WindowsCloudFilesProjectionAcknowledgementSpec<'_>],
) -> LocalityResult<WindowsCloudFilesProjectionRecovery> {
    let _ = repair_windows_cloud_files_projection_recoveries(state_root, access_root)?;
    let mut recovery = prepare_windows_cloud_files_projection_recovery(
        state_root,
        access_root,
        mount_id,
        entity_id,
        operation,
        payload_kind,
        source_path,
        source_relative_path,
        payload_document_relative_path,
        intended_entity_path,
    )?;
    if let Err(error) = record_windows_cloud_files_projection_acknowledgements_for_quarantine(
        state_root,
        access_root,
        mount_id,
        entity_id,
        acknowledgements,
        &recovery.quarantine_path,
    ) {
        record_windows_projection_recovery_source_present(
            state_root,
            &mut recovery,
            "provider acknowledgement recording failed before quarantine rename",
        );
        return Err(error);
    }
    if let Err(error) = std::fs::rename(source_path, &recovery.quarantine_path) {
        revoke_windows_cloud_files_projection_acknowledgements(
            state_root,
            access_root,
            mount_id,
            acknowledgements,
        );
        record_windows_projection_recovery_source_present(
            state_root,
            &mut recovery,
            "atomic quarantine rename failed; source namespace remains in place",
        );
        return Err(LocalityError::from(error));
    }

    recovery.record_revision = recovery.record_revision.saturating_add(1);
    recovery.updated_at_unix_ms = current_unix_millis();
    inspect_windows_cloud_files_projection_recovery(&mut recovery);
    let payload_contents = windows_cloud_files_projection_recovery_document_contents(&recovery);
    match payload_contents {
        Ok(contents)
            if recovery.unexpected_entries.is_empty()
                && projection_contents_are_replaceable(&contents, Some(previous_shadow)) =>
        {
            recovery.status = WindowsCloudFilesProjectionRecoveryStatus::QuarantinedClean;
            recovery.review_reason = None;
        }
        Ok(_) => {
            recovery.status = WindowsCloudFilesProjectionRecoveryStatus::NeedsReview;
            recovery.review_reason =
                Some("quarantined projection changed after the initial clean check".to_string());
        }
        Err(error) => {
            recovery.status = WindowsCloudFilesProjectionRecoveryStatus::NeedsReview;
            recovery.review_reason = Some(format!(
                "quarantined projection could not be inspected after rename: {error}"
            ));
        }
    }
    if let Err(error) = persist_windows_cloud_files_projection_recovery(state_root, &recovery) {
        return Err(LocalityError::InvalidState(format!(
            "Windows Cloud Files projection was preserved at `{}` but final recovery metadata could not be written: {error}",
            recovery.quarantine_path.display()
        )));
    }
    if recovery.status == WindowsCloudFilesProjectionRecoveryStatus::NeedsReview {
        return Err(LocalityError::InvalidState(format!(
            "Windows Cloud Files projection changed during reconciliation and was preserved for recovery at `{}`",
            recovery.quarantine_path.display()
        )));
    }
    Ok(recovery)
}

#[allow(clippy::too_many_arguments)]
fn prepare_windows_cloud_files_projection_recovery(
    state_root: &Path,
    access_root: &Path,
    mount_id: &MountId,
    entity_id: &RemoteId,
    operation: WindowsCloudFilesProjectionRecoveryOperation,
    payload_kind: WindowsCloudFilesProjectionRecoveryPayloadKind,
    source_path: &Path,
    source_relative_path: &Path,
    payload_document_relative_path: Option<&Path>,
    intended_entity_path: Option<&Path>,
) -> LocalityResult<WindowsCloudFilesProjectionRecovery> {
    static RECOVERY_COUNTER: AtomicU64 = AtomicU64::new(0);
    let provider_root = windows_cloud_files_projection_provider_root(access_root)?;
    let quarantine_root = windows_cloud_files_projection_quarantine_root(access_root)?;
    std::fs::create_dir_all(&quarantine_root).map_err(LocalityError::from)?;
    let now = current_unix_millis();
    let recovery_id = format!(
        "{:016x}-{}-{}",
        now,
        std::process::id(),
        RECOVERY_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let suffix = match payload_kind {
        WindowsCloudFilesProjectionRecoveryPayloadKind::PageContainer => "page-container",
        WindowsCloudFilesProjectionRecoveryPayloadKind::File => "file",
        WindowsCloudFilesProjectionRecoveryPayloadKind::Unknown => "payload",
    };
    let recovery = WindowsCloudFilesProjectionRecovery {
        state_version: WINDOWS_CLOUD_FILES_RECOVERY_STATE_VERSION,
        min_reader_version: WINDOWS_CLOUD_FILES_RECOVERY_MIN_READER_VERSION,
        recovery_id: recovery_id.clone(),
        record_revision: 1,
        mount_id: Some(mount_id.clone()),
        entity_id: Some(entity_id.clone()),
        operation,
        payload_kind,
        status: WindowsCloudFilesProjectionRecoveryStatus::Prepared,
        provider_root: Some(provider_root),
        source_access_root: Some(access_root.to_path_buf()),
        source_relative_path: Some(source_relative_path.to_path_buf()),
        source_path: Some(source_path.to_path_buf()),
        intended_entity_path: intended_entity_path.map(Path::to_path_buf),
        quarantine_path: quarantine_root.join(format!("{recovery_id}.{suffix}")),
        payload_document_relative_path: payload_document_relative_path.map(Path::to_path_buf),
        payload_byte_size: None,
        payload_hash: None,
        unexpected_entries: Vec::new(),
        review_reason: None,
        created_at_unix_ms: now,
        updated_at_unix_ms: now,
    };
    persist_windows_cloud_files_projection_recovery(state_root, &recovery)?;
    Ok(recovery)
}

fn record_windows_projection_recovery_source_present(
    state_root: &Path,
    recovery: &mut WindowsCloudFilesProjectionRecovery,
    reason: &str,
) {
    recovery.record_revision = recovery.record_revision.saturating_add(1);
    recovery.status = WindowsCloudFilesProjectionRecoveryStatus::SourcePresent;
    recovery.review_reason = Some(reason.to_string());
    recovery.updated_at_unix_ms = current_unix_millis();
    let _ = persist_windows_cloud_files_projection_recovery(state_root, recovery);
}

fn windows_cloud_files_projection_recovery_manifest_dir(state_root: &Path) -> PathBuf {
    state_root.join("provider-recovery/windows-cloud-files")
}

fn windows_cloud_files_projection_quarantine_root(access_root: &Path) -> LocalityResult<PathBuf> {
    let provider_root = windows_cloud_files_projection_provider_root(access_root)?;
    let outside_parent = provider_root.parent().ok_or_else(|| {
        LocalityError::InvalidState(format!(
            "Windows Cloud Files provider root `{}` has no same-volume recovery parent",
            provider_root.display()
        ))
    })?;
    let provider_key = windows_projection_path_key(&provider_root);
    Ok(outside_parent
        .join(".locality-recovery")
        .join("windows-cloud-files")
        .join(format!(
            "{:016x}",
            stable_projection_ack_hash(&provider_key)
        )))
}

fn windows_cloud_files_projection_provider_root(access_root: &Path) -> LocalityResult<PathBuf> {
    access_root.parent().map(Path::to_path_buf).ok_or_else(|| {
        LocalityError::InvalidState(format!(
            "Windows Cloud Files access root `{}` has no provider root",
            access_root.display()
        ))
    })
}

fn persist_windows_cloud_files_projection_recovery(
    state_root: &Path,
    recovery: &WindowsCloudFilesProjectionRecovery,
) -> LocalityResult<()> {
    persist_windows_cloud_files_projection_recovery_with_durable_publish(
        state_root,
        recovery,
        durably_publish_recovery_manifest,
    )
}

fn persist_windows_cloud_files_projection_recovery_with_durable_publish(
    state_root: &Path,
    recovery: &WindowsCloudFilesProjectionRecovery,
    durable_publish: impl FnOnce(&Path, &Path, &Path) -> std::io::Result<()>,
) -> LocalityResult<()> {
    static MANIFEST_COUNTER: AtomicU64 = AtomicU64::new(0);
    let directory = windows_cloud_files_projection_recovery_manifest_dir(state_root);
    create_dir_all_durable(&directory).map_err(LocalityError::from)?;
    let sequence = MANIFEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = directory.join(format!(
        "{}-r{:08}-{sequence}.json",
        recovery.recovery_id, recovery.record_revision
    ));
    let temporary = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let bytes = serde_json::to_vec_pretty(recovery)
        .map_err(|error| LocalityError::InvalidState(error.to_string()))?;
    if let Err(error) = write_new_file_durable(&temporary, &bytes) {
        let _ = remove_path_durable(&temporary);
        return Err(LocalityError::from(error));
    }
    if let Err(error) = durable_publish(&temporary, &path, &directory) {
        let _ = remove_path_durable(&temporary);
        return Err(LocalityError::from(error));
    }
    Ok(())
}

fn durably_publish_recovery_manifest(
    temporary: &Path,
    destination: &Path,
    directory: &Path,
) -> std::io::Result<()> {
    if destination.parent() != Some(directory) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "recovery manifest destination is outside its directory",
        ));
    }
    rename_noreplace_durable(temporary, destination)
}

fn validate_windows_cloud_files_projection_recovery_version(
    recovery: &WindowsCloudFilesProjectionRecovery,
    path: &Path,
) -> LocalityResult<()> {
    if recovery.state_version > WINDOWS_CLOUD_FILES_RECOVERY_STATE_VERSION
        || recovery.min_reader_version > WINDOWS_CLOUD_FILES_RECOVERY_STATE_VERSION
    {
        return Err(LocalityError::InvalidState(format!(
            "update required to read Windows Cloud Files recovery manifest `{}` version {}",
            path.display(),
            recovery.state_version
        )));
    }
    if recovery.state_version == 0
        || recovery.min_reader_version == 0
        || recovery.min_reader_version > recovery.state_version
    {
        return Err(LocalityError::InvalidState(format!(
            "Windows Cloud Files recovery manifest `{}` has invalid version metadata",
            path.display()
        )));
    }
    Ok(())
}

fn inspect_windows_cloud_files_projection_recovery(
    recovery: &mut WindowsCloudFilesProjectionRecovery,
) {
    match windows_projection_recovery_unexpected_entries(recovery) {
        Ok(entries) => recovery.unexpected_entries = entries,
        Err(error) => {
            recovery.unexpected_entries.clear();
            recovery.review_reason = Some(error.to_string());
        }
    }
    match windows_cloud_files_projection_recovery_document_contents(recovery) {
        Ok(contents) => {
            recovery.payload_byte_size = Some(contents.len() as u64);
            recovery.payload_hash =
                Some(format!("{:016x}", stable_projection_bytes_hash(&contents)));
        }
        Err(error) => {
            recovery.payload_byte_size = None;
            recovery.payload_hash = None;
            if recovery.review_reason.is_none() {
                recovery.review_reason = Some(error.to_string());
            }
        }
    }
}

fn windows_cloud_files_projection_recovery_document_contents(
    recovery: &WindowsCloudFilesProjectionRecovery,
) -> std::io::Result<Vec<u8>> {
    match recovery.payload_kind {
        WindowsCloudFilesProjectionRecoveryPayloadKind::File
        | WindowsCloudFilesProjectionRecoveryPayloadKind::Unknown => {
            std::fs::read(&recovery.quarantine_path)
        }
        WindowsCloudFilesProjectionRecoveryPayloadKind::PageContainer => {
            let document_relative_path = recovery
                .payload_document_relative_path
                .as_deref()
                .unwrap_or(Path::new("page.md"));
            std::fs::read(recovery.quarantine_path.join(document_relative_path))
        }
    }
}

fn windows_projection_recovery_unexpected_entries(
    recovery: &WindowsCloudFilesProjectionRecovery,
) -> std::io::Result<Vec<PathBuf>> {
    if recovery.payload_kind != WindowsCloudFilesProjectionRecoveryPayloadKind::PageContainer {
        return Ok(Vec::new());
    }
    let expected = recovery
        .payload_document_relative_path
        .as_deref()
        .unwrap_or(Path::new("page.md"));
    let mut unexpected = Vec::new();
    for entry in std::fs::read_dir(&recovery.quarantine_path)? {
        let entry = entry?;
        let entry_path = entry.path();
        let relative = entry_path
            .strip_prefix(&recovery.quarantine_path)
            .unwrap_or(&entry_path)
            .to_path_buf();
        if windows_projection_path_key(&relative) != windows_projection_path_key(expected) {
            unexpected.push(relative);
        }
    }
    unexpected.sort();
    Ok(unexpected)
}

fn stable_projection_bytes_hash(value: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in value {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
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
        let target_remote_id = target_visible_remote_id(&target);
        let entities = scoped_page_entities(
            store,
            &mount,
            Some(&target_match),
            target_remote_id.as_ref(),
        )?;
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

        let target_remote_id = target_visible_remote_id(&target);
        let entities = scoped_page_entities(
            store,
            &mount,
            Some(&target_match),
            target_remote_id.as_ref(),
        )?;
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
    target_remote_id: Option<&RemoteId>,
) -> LocalityResult<Vec<EntityRecord>>
where
    S: EntityRepository,
{
    let target_relative = target_match.map(|matched| matched.relative_path.as_path());
    let mut entities = store
        .list_entities(&mount.mount_id)
        .map_err(LocalityError::from)?
        .into_iter()
        .filter(|entity| entity.kind == EntityKind::Page)
        .filter(|entity| match target_relative {
            None => true,
            Some(relative) if relative.as_os_str().is_empty() => true,
            Some(relative) => entity.path == relative || entity.path.starts_with(relative),
        })
        .collect::<Vec<_>>();

    if let Some(remote_id) = target_remote_id
        && !entities.iter().any(|entity| &entity.remote_id == remote_id)
        && let Some(entity) = store
            .get_entity(&mount.mount_id, remote_id)
            .map_err(LocalityError::from)?
            .filter(|entity| entity.kind == EntityKind::Page)
    {
        entities.push(entity);
    }

    Ok(entities)
}

fn target_visible_remote_id(target: &Path) -> Option<RemoteId> {
    target_visible_remote_id_with(target, projection_path_is_dataless_placeholder)
}

fn target_visible_remote_id_with(
    target: &Path,
    is_dataless_placeholder: impl Fn(&Path) -> bool,
) -> Option<RemoteId> {
    // Reading a dataless File Provider placeholder from the daemon can recurse
    // through fetchContents back into the daemon runtime.
    if is_dataless_placeholder(target) {
        return None;
    }
    if !target.is_file() {
        return None;
    }
    let contents = std::fs::read_to_string(target).ok()?;
    let parsed = parse_canonical_markdown(&contents).ok()?;
    parsed.remote_id().cloned()
}

fn rehome_visible_entity_path_if_safe<S>(
    store: &mut S,
    mount: &MountConfig,
    entity: &EntityRecord,
    target: &Path,
    target_relative_path: &Path,
    target_remote_id: Option<&RemoteId>,
) -> LocalityResult<Option<EntityRecord>>
where
    S: EntityRepository + ShadowRepository,
{
    if !target.is_file() || target_remote_id != Some(&entity.remote_id) {
        return Ok(None);
    }
    if matches!(
        entity.hydration,
        HydrationState::Dirty | HydrationState::Conflicted
    ) {
        return Ok(None);
    }
    if entity.path != target_relative_path {
        if let Some(colliding) = store
            .find_entity_by_path(&mount.mount_id, target_relative_path)
            .map_err(LocalityError::from)?
            && colliding.remote_id != entity.remote_id
        {
            return Ok(None);
        }
    }

    let mut repaired = entity.clone();
    if repaired.path != target_relative_path {
        repaired.path = target_relative_path.to_path_buf();
    }
    if matches!(
        repaired.hydration,
        HydrationState::Stub | HydrationState::Virtual
    ) {
        match store.load_shadow(&mount.mount_id, &repaired.remote_id) {
            Ok(_) => repaired.hydration = HydrationState::Hydrated,
            Err(StoreError::ShadowMissing { .. }) => {}
            Err(error) => return Err(LocalityError::from(error)),
        }
    }
    if &repaired == entity {
        return Ok(None);
    }
    store
        .save_entity(repaired.clone())
        .map_err(LocalityError::from)?;
    Ok(Some(repaired))
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

fn projection_namespace_path(path: &Path) -> PathBuf {
    if path
        .file_name()
        .is_some_and(|filename| filename == "page.md")
    {
        return path.parent().unwrap_or(path).to_path_buf();
    }
    path.to_path_buf()
}

fn windows_projection_container_contains_only(
    container: &Path,
    expected_file: &Path,
) -> std::io::Result<bool> {
    let expected_key = windows_projection_path_key(expected_file);
    let mut found_expected_file = false;
    for entry in std::fs::read_dir(container)? {
        let entry = entry?;
        if windows_projection_path_key(&entry.path()) != expected_key {
            return Ok(false);
        }
        found_expected_file = true;
    }
    Ok(found_expected_file)
}

fn windows_cloud_files_projection_identifier(remote_id: &RemoteId, path: &Path) -> String {
    if path
        .file_name()
        .is_some_and(|filename| filename == "page.md")
    {
        format!("children:{}", remote_id.0)
    } else {
        remote_id.0.clone()
    }
}

fn normalize_windows_cloud_files_provider_identifier(identifier: &str) -> String {
    crate::virtual_projection::unwrap_identifier(identifier)
        .map(|unwrapped| unwrapped.daemon_identifier)
        .unwrap_or_else(|_| identifier.to_string())
}

fn validate_windows_projection_relative_path(path: &Path) -> LocalityResult<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(LocalityError::InvalidState(format!(
            "Windows projection acknowledgement path `{}` is not a safe relative path",
            path.display()
        )));
    }
    Ok(())
}

fn windows_projection_path_key(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .trim_matches('/')
        .to_ascii_lowercase()
}

fn windows_cloud_files_projection_acknowledgement_dir(state_root: &Path) -> PathBuf {
    state_root.join("provider-reconciliation/windows-cloud-files")
}

fn windows_cloud_files_projection_acknowledgement_path(
    state_root: &Path,
    access_root: &Path,
    mount_id: &MountId,
    provider_identifier: &str,
    relative_path: &Path,
    event: WindowsCloudFilesProjectionEvent,
) -> PathBuf {
    let key = format!(
        "{}\n{}\n{}\n{}\n{}",
        mount_id.0,
        normalize_windows_cloud_files_provider_identifier(provider_identifier),
        windows_projection_path_key(access_root),
        event.as_str(),
        windows_projection_path_key(relative_path),
    );
    windows_cloud_files_projection_acknowledgement_dir(state_root)
        .join(format!("{:016x}.json", stable_projection_ack_hash(&key)))
}

fn revoke_windows_cloud_files_projection_acknowledgement(
    state_root: &Path,
    access_root: &Path,
    mount_id: &MountId,
    provider_identifier: &str,
    relative_path: &Path,
    event: WindowsCloudFilesProjectionEvent,
) {
    let path = windows_cloud_files_projection_acknowledgement_path(
        state_root,
        access_root,
        mount_id,
        provider_identifier,
        relative_path,
        event,
    );
    let _ = std::fs::remove_file(path);
}

fn revoke_windows_cloud_files_projection_acknowledgements(
    state_root: &Path,
    access_root: &Path,
    mount_id: &MountId,
    acknowledgements: &[WindowsCloudFilesProjectionAcknowledgementSpec<'_>],
) {
    for acknowledgement in acknowledgements {
        revoke_windows_cloud_files_projection_acknowledgement(
            state_root,
            access_root,
            mount_id,
            acknowledgement.provider_identifier,
            acknowledgement.relative_path,
            acknowledgement.event,
        );
    }
}

fn stable_projection_ack_hash(value: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn windows_projection_acknowledgement_temporary_path(path: &Path, purpose: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let filename = path
        .file_name()
        .and_then(|filename| filename.to_str())
        .unwrap_or("acknowledgement.json");
    path.with_file_name(format!(
        ".{filename}.{purpose}.{}.{counter}.tmp",
        std::process::id()
    ))
}

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn windows_projection_acknowledgement_is_current(
    acknowledgement: &WindowsCloudFilesProjectionAcknowledgement,
    now: u64,
) -> bool {
    acknowledgement.created_at_unix_ms <= now
        && now.saturating_sub(acknowledgement.created_at_unix_ms)
            <= WINDOWS_CLOUD_FILES_PROJECTION_ACK_MAX_AGE_MS
}

fn repair_windows_cloud_files_projection_acknowledgements(state_root: &Path, now: u64) {
    let directory = windows_cloud_files_projection_acknowledgement_dir(state_root);
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|file_type| file_type.is_file()) {
            continue;
        }
        let valid = std::fs::read(&path)
            .ok()
            .and_then(|bytes| {
                serde_json::from_slice::<WindowsCloudFilesProjectionAcknowledgement>(&bytes).ok()
            })
            .is_some_and(|acknowledgement| {
                acknowledgement.version == WINDOWS_CLOUD_FILES_PROJECTION_ACK_VERSION
                    && windows_projection_acknowledgement_is_current(&acknowledgement, now)
            });
        if !valid {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn prepare_windows_projection_refresh(
    entity: &EntityRecord,
    content_root: &Path,
    projection_path: &Path,
    previous_shadow: &ShadowDocument,
) -> LocalityResult<Option<Vec<u8>>> {
    let content_path = content_cache_path(content_root, &entity.path)?;
    let Ok(cache_contents) = std::fs::read(content_path) else {
        return Ok(None);
    };
    if projection_path.exists() {
        let projection_contents = std::fs::read(projection_path).map_err(LocalityError::from)?;
        if !projection_contents_are_replaceable(&projection_contents, Some(previous_shadow)) {
            return Err(LocalityError::InvalidState(format!(
                "Windows Cloud Files projection `{}` changed during refresh",
                projection_path.display()
            )));
        }
    }
    Ok(Some(cache_contents))
}

fn refresh_windows_projection_candidate_if_clean(
    entity: &EntityRecord,
    content_root: &Path,
    projection_path: &Path,
    previous_shadow: &ShadowDocument,
) -> LocalityResult<ProjectionRefreshOutcome> {
    let Some(cache_contents) =
        prepare_windows_projection_refresh(entity, content_root, projection_path, previous_shadow)?
    else {
        return Ok(ProjectionRefreshOutcome::MissingCache);
    };
    let projection_contents = match std::fs::read(projection_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProjectionRefreshOutcome::MissingProjection);
        }
        Err(error) => return Err(LocalityError::from(error)),
    };
    if projection_contents == cache_contents {
        return Ok(ProjectionRefreshOutcome::Unchanged);
    }
    std::fs::write(projection_path, cache_contents).map_err(LocalityError::from)?;
    Ok(ProjectionRefreshOutcome::Refreshed)
}

fn record_windows_projection_refresh_outcome(
    report: &mut ProjectionRefreshReport,
    outcome: ProjectionRefreshOutcome,
) {
    report.checked += 1;
    match outcome {
        ProjectionRefreshOutcome::MissingCache => report.skipped_missing_cache += 1,
        ProjectionRefreshOutcome::MissingProjection => report.skipped_missing_projection += 1,
        ProjectionRefreshOutcome::Unchanged => report.skipped_unchanged += 1,
        ProjectionRefreshOutcome::SkippedLocalChanges => report.skipped_local_changes += 1,
        ProjectionRefreshOutcome::Refreshed => report.refreshed += 1,
    }
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

    let commit_has_conflict_markers =
        std::str::from_utf8(&commit_contents).is_ok_and(has_unresolved_conflict_markers);
    if commit_has_conflict_markers && projection_contents.as_bytes() != commit_contents {
        write_binary_atomic(&candidate.path, &commit_contents).map_err(LocalityError::from)?;
    }

    virtual_fs::commit_virtual_fs_write(
        store,
        content_root,
        &mount.mount_id,
        &entity.remote_id.0,
        &commit_contents,
    )?;
    if !commit_has_conflict_markers && projection_contents.as_bytes() != commit_contents {
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

    remote_version_is_ordered_before(visible_remote_edited_at, &shadow_remote_edited_at)
}

fn shadow_remote_edited_at(shadow: &ShadowDocument) -> Option<String> {
    parse_canonical_markdown(&render_canonical_markdown(&CanonicalDocument::new(
        shadow.frontmatter.clone(),
        "",
    )))
    .ok()
    .and_then(|parsed| parsed.frontmatter.loc.and_then(|loc| loc.remote_edited_at))
}

fn remote_version_is_ordered_before(left: &str, right: &str) -> bool {
    if left == right {
        return false;
    }

    if looks_like_rfc3339_utc(left) && looks_like_rfc3339_utc(right) {
        return left < right;
    }

    let Some((left_prefix, left_number)) = split_trailing_number(left) else {
        return false;
    };
    let Some((right_prefix, right_number)) = split_trailing_number(right) else {
        return false;
    };

    left_prefix == right_prefix && left_number < right_number
}

fn looks_like_rfc3339_utc(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= "0000-00-00T00:00:00".len()
        && bytes
            .get(0..4)
            .is_some_and(|year| year.iter().all(u8::is_ascii_digit))
        && bytes.get(4) == Some(&b'-')
        && bytes.get(7) == Some(&b'-')
        && bytes.get(10) == Some(&b'T')
}

fn split_trailing_number(value: &str) -> Option<(&str, u64)> {
    let digit_start = value
        .char_indices()
        .rev()
        .find_map(|(index, character)| (!character.is_ascii_digit()).then_some(index + 1))?;
    if digit_start == value.len() {
        return None;
    }

    let (prefix, digits) = value.split_at(digit_start);
    digits.parse::<u64>().ok().map(|number| (prefix, number))
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
    if let Err(error) = std::fs::write(&temp_path, contents) {
        if path.exists() {
            return std::fs::write(path, contents).map_err(|_| error);
        }
        return Err(error);
    }
    std::fs::rename(&temp_path, path).or_else(|error| {
        let _ = std::fs::remove_file(&temp_path);
        if path.exists() {
            std::fs::write(path, contents).map_err(|_| error)
        } else {
            Err(error)
        }
    })
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
    fn windows_projection_acknowledgements_are_exact_one_shot_and_identity_normalized() {
        let state_root = temp_root("loc-windows-projection-ack");
        let access_root = PathBuf::from(r"C:\Locality\notion-main");
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let entity = EntityRecord::new(
            mount_id.clone(),
            remote_id.clone(),
            EntityKind::Page,
            "Roadmap",
            "teams/old/Roadmap/page.md",
        );
        let wrapped_directory =
            crate::virtual_projection::wrap_identifier(&mount_id, "children:page-1");

        record_windows_cloud_files_projection_acknowledgement(
            &state_root,
            &access_root,
            &mount_id,
            &remote_id,
            "children:page-1",
            Path::new("teams/old/Roadmap"),
            WindowsCloudFilesProjectionEvent::CloudFilesRenameTarget,
            Some(Path::new("teams/old/Roadmap/page.md")),
        )
        .expect("record move target acknowledgement");
        record_windows_cloud_files_projection_acknowledgement(
            &state_root,
            &access_root,
            &mount_id,
            &remote_id,
            "children:page-1",
            Path::new("teams/new/Roadmap"),
            WindowsCloudFilesProjectionEvent::CloudFilesDeleteMoveSource,
            Some(Path::new("teams/old/Roadmap/page.md")),
        )
        .expect("record Cloud Files move source acknowledgement");
        record_windows_cloud_files_projection_acknowledgement(
            &state_root,
            &access_root,
            &mount_id,
            &remote_id,
            "children:page-1",
            Path::new("teams/new/Roadmap"),
            WindowsCloudFilesProjectionEvent::WatcherRemoveMoveSource,
            Some(Path::new("teams/old/Roadmap/page.md")),
        )
        .expect("record watcher move source acknowledgement");

        assert!(!consume_windows_cloud_files_projection_acknowledgement(
            &state_root,
            &access_root,
            &mount_id,
            &remote_id,
            &wrapped_directory,
            Path::new("teams/pending/Roadmap"),
            WindowsCloudFilesProjectionEvent::WatcherRemoveMoveSource,
            Some(&entity),
        ));
        assert!(!consume_windows_cloud_files_projection_acknowledgement(
            &state_root,
            Path::new(r"C:\Locality\other-access-root"),
            &mount_id,
            &remote_id,
            &wrapped_directory,
            Path::new("teams/old/Roadmap"),
            WindowsCloudFilesProjectionEvent::CloudFilesRenameTarget,
            Some(&entity),
        ));
        assert!(consume_windows_cloud_files_projection_acknowledgement(
            &state_root,
            &access_root,
            &mount_id,
            &remote_id,
            &wrapped_directory,
            Path::new("teams/old/Roadmap"),
            WindowsCloudFilesProjectionEvent::CloudFilesRenameTarget,
            Some(&entity),
        ));
        assert!(!consume_windows_cloud_files_projection_acknowledgement(
            &state_root,
            &access_root,
            &mount_id,
            &remote_id,
            &wrapped_directory,
            Path::new("teams/old/Roadmap"),
            WindowsCloudFilesProjectionEvent::CloudFilesRenameTarget,
            Some(&entity),
        ));
        assert!(consume_windows_cloud_files_projection_acknowledgement(
            &state_root,
            &access_root,
            &mount_id,
            &remote_id,
            &wrapped_directory,
            Path::new("teams/new/Roadmap"),
            WindowsCloudFilesProjectionEvent::CloudFilesDeleteMoveSource,
            Some(&entity),
        ));
        assert!(consume_windows_cloud_files_projection_acknowledgement(
            &state_root,
            &access_root,
            &mount_id,
            &remote_id,
            &wrapped_directory,
            Path::new("teams/new/Roadmap"),
            WindowsCloudFilesProjectionEvent::WatcherRemoveMoveSource,
            Some(&entity),
        ));

        record_windows_cloud_files_projection_acknowledgement(
            &state_root,
            &access_root,
            &mount_id,
            &remote_id,
            "page-1",
            Path::new("teams/old/Roadmap/page.md"),
            WindowsCloudFilesProjectionEvent::CloudFilesDeleteArchivedEntity,
            None,
        )
        .expect("record Cloud Files archive acknowledgement");
        record_windows_cloud_files_projection_acknowledgement(
            &state_root,
            &access_root,
            &mount_id,
            &remote_id,
            "page-1",
            Path::new("teams/old/Roadmap/page.md"),
            WindowsCloudFilesProjectionEvent::WatcherRemoveArchivedEntity,
            None,
        )
        .expect("record watcher archive acknowledgement");
        assert!(consume_windows_cloud_files_projection_acknowledgement(
            &state_root,
            &access_root,
            &mount_id,
            &remote_id,
            "page-1",
            Path::new("teams/old/Roadmap/page.md"),
            WindowsCloudFilesProjectionEvent::CloudFilesDeleteArchivedEntity,
            None,
        ));
        assert!(consume_windows_cloud_files_projection_acknowledgement(
            &state_root,
            &access_root,
            &mount_id,
            &remote_id,
            "page-1",
            Path::new("teams/old/Roadmap/page.md"),
            WindowsCloudFilesProjectionEvent::WatcherRemoveArchivedEntity,
            None,
        ));

        let _ = fs::remove_dir_all(state_root);
    }

    #[test]
    fn windows_projection_acknowledgements_expire_when_provider_is_stopped_and_repair_malformed_state()
     {
        let state_root = temp_root("loc-windows-projection-ack-repair");
        let access_root = PathBuf::from(r"C:\Locality\notion-main");
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let relative_path = Path::new("teams/new/Roadmap.md");
        let event = WindowsCloudFilesProjectionEvent::WatcherRemoveMoveSource;
        let expired_at =
            current_unix_millis().saturating_sub(WINDOWS_CLOUD_FILES_PROJECTION_ACK_MAX_AGE_MS + 1);
        record_windows_cloud_files_projection_acknowledgement_at(
            &state_root,
            &access_root,
            &mount_id,
            &remote_id,
            "page-1",
            relative_path,
            event,
            Some(Path::new("teams/old/Roadmap.md")),
            None,
            expired_at,
        )
        .expect("record expired acknowledgement");

        assert!(!consume_windows_cloud_files_projection_acknowledgement(
            &state_root,
            &access_root,
            &mount_id,
            &remote_id,
            "page-1",
            relative_path,
            event,
            None,
        ));

        let malformed_path = windows_cloud_files_projection_acknowledgement_path(
            &state_root,
            &access_root,
            &mount_id,
            "page-1",
            relative_path,
            event,
        );
        fs::create_dir_all(malformed_path.parent().expect("ack parent"))
            .expect("create ack parent");
        fs::write(&malformed_path, b"{not-json").expect("write malformed acknowledgement");
        assert!(!consume_windows_cloud_files_projection_acknowledgement(
            &state_root,
            &access_root,
            &mount_id,
            &remote_id,
            "page-1",
            relative_path,
            event,
            None,
        ));
        assert!(!malformed_path.exists());

        let _ = fs::remove_dir_all(state_root);
    }

    #[test]
    fn windows_projection_acknowledgement_group_revokes_prior_records_on_failure() {
        let state_root = temp_root("loc-windows-projection-ack-revoke");
        let access_root = PathBuf::from(r"C:\Locality\notion-main");
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let valid_path = Path::new("teams/new/Roadmap.md");
        let acknowledgements = [
            WindowsCloudFilesProjectionAcknowledgementSpec {
                provider_identifier: "page-1",
                relative_path: valid_path,
                event: WindowsCloudFilesProjectionEvent::CloudFilesDeleteMoveSource,
                expected_entity_path: Some(Path::new("teams/old/Roadmap.md")),
            },
            WindowsCloudFilesProjectionAcknowledgementSpec {
                provider_identifier: "page-1",
                relative_path: Path::new("../outside.md"),
                event: WindowsCloudFilesProjectionEvent::WatcherRemoveMoveSource,
                expected_entity_path: Some(Path::new("teams/old/Roadmap.md")),
            },
        ];

        assert!(
            record_windows_cloud_files_projection_acknowledgements(
                &state_root,
                &access_root,
                &mount_id,
                &remote_id,
                &acknowledgements,
            )
            .is_err()
        );
        assert!(!consume_windows_cloud_files_projection_acknowledgement(
            &state_root,
            &access_root,
            &mount_id,
            &remote_id,
            "page-1",
            valid_path,
            WindowsCloudFilesProjectionEvent::CloudFilesDeleteMoveSource,
            None,
        ));

        let _ = fs::remove_dir_all(state_root);
    }

    #[test]
    fn windows_quarantine_acknowledgement_requires_materialized_exact_target_before_consumption() {
        let root = temp_root("loc-windows-quarantine-ack-order");
        let state_root = root.join("state");
        let access_root = root.join("provider/notion-main");
        let quarantine_path = root.join("recovery/page-1.file");
        let wrong_path = root.join("recovery/wrong.file");
        fs::create_dir_all(&access_root).expect("create access root");
        fs::create_dir_all(quarantine_path.parent().expect("quarantine parent"))
            .expect("create quarantine parent");
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let relative_path = Path::new("teams/new/Roadmap.md");
        let entity = EntityRecord::new(
            mount_id.clone(),
            remote_id.clone(),
            EntityKind::Page,
            "Roadmap",
            "teams/old/Roadmap.md",
        );

        record_windows_cloud_files_projection_acknowledgement_at(
            &state_root,
            &access_root,
            &mount_id,
            &remote_id,
            "page-1",
            relative_path,
            WindowsCloudFilesProjectionEvent::CloudFilesQuarantineMoveSource,
            Some(&entity.path),
            Some(&quarantine_path),
            current_unix_millis(),
        )
        .expect("record Cloud Files quarantine acknowledgement");
        assert!(!consume_windows_cloud_files_quarantine_acknowledgement(
            &state_root,
            &access_root,
            &mount_id,
            &remote_id,
            "page-1",
            relative_path,
            WindowsCloudFilesProjectionEvent::CloudFilesQuarantineMoveSource,
            Some(&entity),
            Some(&quarantine_path),
        ));
        fs::write(&quarantine_path, "preserved").expect("materialize quarantine target");
        assert!(!consume_windows_cloud_files_quarantine_acknowledgement(
            &state_root,
            &access_root,
            &mount_id,
            &remote_id,
            "page-1",
            relative_path,
            WindowsCloudFilesProjectionEvent::CloudFilesQuarantineMoveSource,
            Some(&entity),
            Some(&wrong_path),
        ));
        assert!(consume_windows_cloud_files_quarantine_acknowledgement(
            &state_root,
            &access_root,
            &mount_id,
            &remote_id,
            "page-1",
            relative_path,
            WindowsCloudFilesProjectionEvent::CloudFilesQuarantineMoveSource,
            Some(&entity),
            Some(&quarantine_path),
        ));

        let watcher_quarantine_path = root.join("recovery/page-1-watcher.file");
        record_windows_cloud_files_projection_acknowledgement_at(
            &state_root,
            &access_root,
            &mount_id,
            &remote_id,
            "page-1",
            relative_path,
            WindowsCloudFilesProjectionEvent::WatcherQuarantineMoveSource,
            Some(&entity.path),
            Some(&watcher_quarantine_path),
            current_unix_millis(),
        )
        .expect("record watcher quarantine acknowledgement");
        assert!(!consume_windows_cloud_files_quarantine_acknowledgement(
            &state_root,
            &access_root,
            &mount_id,
            &remote_id,
            "page-1",
            relative_path,
            WindowsCloudFilesProjectionEvent::WatcherQuarantineMoveSource,
            Some(&entity),
            None,
        ));
        fs::write(&watcher_quarantine_path, "preserved")
            .expect("materialize watcher quarantine target");
        assert!(consume_windows_cloud_files_quarantine_acknowledgement(
            &state_root,
            &access_root,
            &mount_id,
            &remote_id,
            "page-1",
            relative_path,
            WindowsCloudFilesProjectionEvent::WatcherQuarantineMoveSource,
            Some(&entity),
            None,
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn windows_projection_recovery_repairs_interrupted_and_source_present_manifests() {
        let root = temp_root("loc-windows-recovery-repair");
        let state_root = root.join("state");
        let access_root = root.join("provider/notion-main");
        fs::create_dir_all(&access_root).expect("create access root");
        let mount_id = MountId::new("notion-main");
        let first_id = RemoteId::new("page-1");
        let first_source = access_root.join("first.md");
        fs::write(&first_source, "first").expect("write first source");
        let first = prepare_windows_cloud_files_projection_recovery(
            &state_root,
            &access_root,
            &mount_id,
            &first_id,
            WindowsCloudFilesProjectionRecoveryOperation::Move,
            WindowsCloudFilesProjectionRecoveryPayloadKind::File,
            &first_source,
            Path::new("first.md"),
            None,
            Some(Path::new("restored/first.md")),
        )
        .expect("prepare source-present recovery");

        let second_id = RemoteId::new("page-2");
        let second_source = access_root.join("second.md");
        fs::write(&second_source, "second").expect("write second source");
        let second = prepare_windows_cloud_files_projection_recovery(
            &state_root,
            &access_root,
            &mount_id,
            &second_id,
            WindowsCloudFilesProjectionRecoveryOperation::Move,
            WindowsCloudFilesProjectionRecoveryPayloadKind::File,
            &second_source,
            Path::new("second.md"),
            None,
            Some(Path::new("restored/second.md")),
        )
        .expect("prepare interrupted recovery");
        fs::rename(&second_source, &second.quarantine_path).expect("simulate quarantine rename");

        let repaired = repair_windows_cloud_files_projection_recoveries(&state_root, &access_root)
            .expect("repair recovery manifests");
        let first = repaired
            .iter()
            .find(|recovery| recovery.recovery_id == first.recovery_id)
            .expect("first recovery");
        assert_eq!(
            first.status,
            WindowsCloudFilesProjectionRecoveryStatus::SourcePresent
        );
        let second = repaired
            .iter()
            .find(|recovery| recovery.recovery_id == second.recovery_id)
            .expect("second recovery");
        assert_eq!(
            second.status,
            WindowsCloudFilesProjectionRecoveryStatus::NeedsReview
        );
        assert!(second.quarantine_path.is_file());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn windows_projection_recovery_manifest_syncs_directory_after_rename_and_propagates_failure() {
        let root = temp_root("loc-windows-recovery-directory-sync");
        let state_root = root.join("state");
        let access_root = root.join("provider/notion-main");
        fs::create_dir_all(&access_root).expect("create access root");
        let source = access_root.join("page.md");
        fs::write(&source, "page").expect("write source");
        let recovery = prepare_windows_cloud_files_projection_recovery(
            &state_root,
            &access_root,
            &MountId::new("notion-main"),
            &RemoteId::new("page-1"),
            WindowsCloudFilesProjectionRecoveryOperation::Move,
            WindowsCloudFilesProjectionRecoveryPayloadKind::File,
            &source,
            Path::new("page.md"),
            None,
            Some(Path::new("restored/page.md")),
        )
        .expect("prepare recovery fixture");
        let manifest_dir = windows_cloud_files_projection_recovery_manifest_dir(&state_root);
        fs::remove_dir_all(&manifest_dir).expect("remove fixture manifest");
        let sync_calls = std::cell::Cell::new(0);

        let error = persist_windows_cloud_files_projection_recovery_with_durable_publish(
            &state_root,
            &recovery,
            |temporary, destination, directory| {
                fs::rename(temporary, destination).expect("rename manifest before directory sync");
                sync_calls.set(sync_calls.get() + 1);
                assert_eq!(directory, manifest_dir);
                let entries = fs::read_dir(directory)
                    .expect("manifest directory exists before sync")
                    .map(|entry| entry.expect("read manifest entry").path())
                    .collect::<Vec<_>>();
                assert_eq!(entries.len(), 1);
                assert_eq!(
                    entries[0].extension().and_then(|value| value.to_str()),
                    Some("json")
                );
                Err(std::io::Error::other("injected directory sync failure"))
            },
        )
        .expect_err("directory sync failure must fail manifest publication");

        assert_eq!(sync_calls.get(), 1);
        assert!(
            error
                .to_string()
                .contains("injected directory sync failure")
        );
        assert_eq!(
            list_windows_cloud_files_projection_recoveries(&state_root)
                .expect("renamed manifest remains inspectable"),
            vec![recovery]
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn windows_projection_recovery_manifest_directory_uses_shared_durable_creation() {
        let root = temp_root("loc-windows-recovery-directory-ancestry");
        let state_root = root.join("state");
        fs::create_dir_all(&state_root).expect("create durable state root");
        let manifest_dir = windows_cloud_files_projection_recovery_manifest_dir(&state_root);
        let synced = std::cell::RefCell::new(Vec::new());

        crate::durable_fs::create_dir_all_durable_with_sync(&manifest_dir, |directory| {
            assert!(directory.is_dir());
            synced.borrow_mut().push(directory.to_path_buf());
            Ok(())
        })
        .expect("create and sync manifest directory ancestry");

        assert_eq!(
            synced.into_inner(),
            vec![
                state_root.clone(),
                state_root.join("provider-recovery"),
                state_root.join("provider-recovery"),
                manifest_dir,
            ]
        );

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn windows_projection_recovery_manifest_publish_never_replaces_existing_revision() {
        let root = temp_root("loc-windows-recovery-manifest-collision");
        fs::create_dir_all(&root).expect("create manifest directory");
        let temporary = root.join("manifest.tmp");
        let destination = root.join("manifest.json");
        fs::write(&temporary, "new revision").expect("write temporary manifest");
        fs::write(&destination, "existing revision").expect("write existing manifest");

        let error = durably_publish_recovery_manifest(&temporary, &destination, &root)
            .expect_err("immutable manifest collision must fail");

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read_to_string(&destination).expect("read existing manifest"),
            "existing revision"
        );
        assert_eq!(
            fs::read_to_string(&temporary).expect("read unpublished temporary manifest"),
            "new revision"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn windows_projection_recovery_detects_payload_rename_rollback_after_clean_record() {
        let root = temp_root("loc-windows-recovery-payload-rollback");
        let state_root = root.join("state");
        let access_root = root.join("provider/notion-main");
        fs::create_dir_all(&access_root).expect("create access root");
        let source = access_root.join("page.md");
        fs::write(&source, "page").expect("write source");
        let mut recovery = prepare_windows_cloud_files_projection_recovery(
            &state_root,
            &access_root,
            &MountId::new("notion-main"),
            &RemoteId::new("page-1"),
            WindowsCloudFilesProjectionRecoveryOperation::Move,
            WindowsCloudFilesProjectionRecoveryPayloadKind::File,
            &source,
            Path::new("page.md"),
            None,
            Some(Path::new("restored/page.md")),
        )
        .expect("prepare recovery");
        fs::rename(&source, &recovery.quarantine_path).expect("move payload to quarantine");
        inspect_windows_cloud_files_projection_recovery(&mut recovery);
        recovery.record_revision += 1;
        recovery.status = WindowsCloudFilesProjectionRecoveryStatus::QuarantinedClean;
        persist_windows_cloud_files_projection_recovery(&state_root, &recovery)
            .expect("persist clean recovery record");
        fs::rename(&recovery.quarantine_path, &source).expect("simulate namespace rollback");

        let repaired = repair_windows_cloud_files_projection_recoveries(&state_root, &access_root)
            .expect("repair rolled-back payload rename");
        let repaired = repaired
            .iter()
            .find(|candidate| candidate.recovery_id == recovery.recovery_id)
            .expect("repaired recovery");
        assert_eq!(
            repaired.status,
            WindowsCloudFilesProjectionRecoveryStatus::Missing
        );
        assert!(source.is_file());
        assert!(
            repaired
                .review_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("quarantine payload is missing"))
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn windows_projection_recovery_discovers_orphan_and_rejects_newer_state() {
        let root = temp_root("loc-windows-recovery-orphan");
        let state_root = root.join("state");
        let access_root = root.join("provider/notion-main");
        fs::create_dir_all(&access_root).expect("create access root");
        let quarantine_root =
            windows_cloud_files_projection_quarantine_root(&access_root).expect("quarantine root");
        fs::create_dir_all(&quarantine_root).expect("create quarantine root");
        let orphan_path = quarantine_root.join("orphan.file");
        fs::write(&orphan_path, "orphan bytes").expect("write orphan payload");

        let repaired = repair_windows_cloud_files_projection_recoveries(&state_root, &access_root)
            .expect("discover orphan payload");
        let orphan = repaired
            .iter()
            .find(|recovery| recovery.quarantine_path == orphan_path)
            .expect("orphan recovery record");
        assert_eq!(
            orphan.status,
            WindowsCloudFilesProjectionRecoveryStatus::Orphaned
        );
        assert_eq!(orphan.source_access_root, None);
        assert_eq!(
            orphan.provider_root,
            access_root.parent().map(Path::to_path_buf)
        );

        let mut newer = orphan.clone();
        newer.recovery_id = "newer-state".to_string();
        newer.state_version = WINDOWS_CLOUD_FILES_RECOVERY_STATE_VERSION + 1;
        newer.min_reader_version = newer.state_version;
        persist_windows_cloud_files_projection_recovery(&state_root, &newer)
            .expect("persist newer recovery state");
        let error = list_windows_cloud_files_projection_recoveries(&state_root)
            .expect_err("newer recovery state must require an update");
        assert!(error.to_string().contains("update required"));

        let _ = fs::remove_dir_all(root);
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
    fn macos_file_provider_item_identifier_wraps_shared_domain_containers() {
        assert_eq!(
            macos_file_provider_item_identifier("notion-main", ROOT_CONTAINER_IDENTIFIER),
            ROOT_CONTAINER_IDENTIFIER
        );
        assert_eq!(
            macos_file_provider_item_identifier("notion-main", "mount:notion-main"),
            "m:bm90aW9uLW1haW4:bW91bnQ6bm90aW9uLW1haW4"
        );
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
    fn shared_macos_file_provider_domain_children_reflect_source_root_create_policy() {
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
                    MountId::new("google-docs-main"),
                    "google-docs",
                    "/tmp/Locality/google-docs-main",
                )
                .with_remote_root_id(RemoteId::new("workspace-folder"))
                .projection(ProjectionMode::MacosFileProvider),
            )
            .expect("save google docs mount");

        let report =
            file_provider_domain_children(&store, MACOS_FILE_PROVIDER_DOMAIN_ID).expect("children");

        let notion = report
            .children
            .iter()
            .find(|child| child.mount_id == "notion-main")
            .expect("notion mount");
        let google_docs = report
            .children
            .iter()
            .find(|child| child.mount_id == "google-docs-main")
            .expect("google docs mount");
        assert!(notion.item.read_only);
        assert!(!google_docs.item.read_only);
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
    fn target_visible_remote_id_skips_dataless_placeholder_before_reading_identity() {
        let root = temp_root("loc-file-provider-target-dataless-id");
        let path = root.join("page.md");
        let remote_id = RemoteId::new("page-1");
        fs::create_dir_all(&root).expect("create temp root");
        fs::write(
            &path,
            render_canonical_markdown(&CanonicalDocument::new(
                versioned_frontmatter(&remote_id, "remote-v1"),
                "Body.\n",
            )),
        )
        .expect("write visible page");

        assert_eq!(
            target_visible_remote_id_with(&path, |_| false),
            Some(remote_id)
        );
        assert_eq!(target_visible_remote_id_with(&path, |_| true), None);

        let _ = fs::remove_dir_all(root);
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
    fn reconcile_visible_projection_rehomes_clean_entity_by_visible_identity() {
        let root = temp_root("loc-file-provider-rehome-root");
        let state_root = temp_root("loc-file-provider-rehome-state");
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("standup-2026-07-02");
        let stale_path = PathBuf::from("engineering-wiki/2026-07-02/page.md");
        let correct_path =
            PathBuf::from("engineering-wiki/standups-with-locality/2026-07-02/page.md");
        let mount_root = root.join("notion");
        let visible_path = mount_root.join(&correct_path);
        let body = "ALI:\n\n- Synced standup notes.\n";
        fs::create_dir_all(visible_path.parent().expect("visible parent")).expect("visible parent");
        fs::write(
            &visible_path,
            render_canonical_markdown(&CanonicalDocument::new(
                versioned_frontmatter(&remote_id, "remote-v1"),
                body,
            )),
        )
        .expect("write visible file");

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
                    "2026-07-02",
                    stale_path.clone(),
                )
                .with_hydration(HydrationState::Stub),
            )
            .expect("save stale entity path");
        store
            .save_shadow(
                &mount_id,
                ShadowDocument::from_synced_body(
                    remote_id.clone(),
                    body,
                    8,
                    [RemoteId::new("block-1"), RemoteId::new("block-2")],
                )
                .expect("shadow")
                .with_frontmatter(versioned_frontmatter(&remote_id, "remote-v1")),
            )
            .expect("save shadow");

        let report = reconcile_visible_projection(&mut store, &state_root, Some(&visible_path))
            .expect("reconcile visible projection");

        assert_eq!(report.reconciled, 1);
        let entity = store
            .get_entity(&mount_id, &remote_id)
            .expect("get entity")
            .expect("entity");
        assert_eq!(entity.path, correct_path);
        assert_eq!(entity.hydration, HydrationState::Hydrated);
        let content_path =
            crate::virtual_fs::virtual_fs_content_root(&state_root, &mount_id).join(&entity.path);
        assert!(
            fs::read_to_string(content_path)
                .expect("read repaired content cache")
                .contains("Synced standup notes.")
        );

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(state_root);
    }

    #[test]
    fn reconcile_visible_projection_does_not_rehome_dirty_entity() {
        let root = temp_root("loc-file-provider-rehome-dirty-root");
        let state_root = temp_root("loc-file-provider-rehome-dirty-state");
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("standup-2026-07-02");
        let stale_path = PathBuf::from("engineering-wiki/2026-07-02/page.md");
        let correct_path =
            PathBuf::from("engineering-wiki/standups-with-locality/2026-07-02/page.md");
        let mount_root = root.join("notion");
        let visible_path = mount_root.join(&correct_path);
        fs::create_dir_all(visible_path.parent().expect("visible parent")).expect("visible parent");
        fs::write(
            &visible_path,
            render_canonical_markdown(&CanonicalDocument::new(
                versioned_frontmatter(&remote_id, "remote-v1"),
                "Local pending notes.\n",
            )),
        )
        .expect("write visible file");

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
                    "2026-07-02",
                    stale_path.clone(),
                )
                .with_hydration(HydrationState::Dirty),
            )
            .expect("save dirty entity path");

        let report = reconcile_visible_projection(&mut store, &state_root, Some(&visible_path))
            .expect("reconcile visible projection");

        assert_eq!(report.reconciled, 0);
        let entity = store
            .get_entity(&mount_id, &remote_id)
            .expect("get entity")
            .expect("entity");
        assert_eq!(entity.path, stale_path);
        assert_eq!(entity.hydration, HydrationState::Dirty);

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(state_root);
    }

    #[test]
    fn reconcile_visible_projection_promotes_stub_with_shadow_at_visible_path() {
        let root = temp_root("loc-file-provider-promote-stub-root");
        let state_root = temp_root("loc-file-provider-promote-stub-state");
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("standup-2026-07-02");
        let relative_path =
            PathBuf::from("engineering-wiki/standups-with-locality/2026-07-02/page.md");
        let mount_root = root.join("notion");
        let visible_path = mount_root.join(&relative_path);
        let content_path =
            crate::virtual_fs::virtual_fs_content_root(&state_root, &mount_id).join(&relative_path);
        let body = "ALI:\n\n- Synced standup notes.\n";
        let markdown = render_canonical_markdown(&CanonicalDocument::new(
            versioned_frontmatter(&remote_id, "remote-v1"),
            body,
        ));
        fs::create_dir_all(visible_path.parent().expect("visible parent")).expect("visible parent");
        fs::create_dir_all(content_path.parent().expect("content parent")).expect("content parent");
        fs::write(&visible_path, &markdown).expect("write visible file");
        fs::write(&content_path, &markdown).expect("write cache file");

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
                    "2026-07-02",
                    relative_path.clone(),
                )
                .with_hydration(HydrationState::Stub),
            )
            .expect("save stub entity");
        store
            .save_shadow(
                &mount_id,
                ShadowDocument::from_synced_body(
                    remote_id.clone(),
                    body,
                    8,
                    [RemoteId::new("block-1"), RemoteId::new("block-2")],
                )
                .expect("shadow")
                .with_frontmatter(versioned_frontmatter(&remote_id, "remote-v1")),
            )
            .expect("save shadow");

        let report = reconcile_visible_projection(&mut store, &state_root, Some(&visible_path))
            .expect("reconcile visible projection");

        assert_eq!(report.skipped_unchanged, 1);
        let entity = store
            .get_entity(&mount_id, &remote_id)
            .expect("get entity")
            .expect("entity");
        assert_eq!(entity.path, relative_path);
        assert_eq!(entity.hydration, HydrationState::Hydrated);

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
    fn reconcile_visible_projection_imports_newer_edit_with_unknown_version_metadata() {
        let root = temp_root("loc-file-provider-unknown-visible-version-root");
        let state_root = temp_root("loc-file-provider-unknown-visible-version-state");
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
                    .projection(ProjectionMode::MacosFileProvider),
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
                .with_remote_edited_at("2026-06-10T00:00:00.000Z"),
            )
            .expect("save entity");
        let shadow = ShadowDocument::from_synced_body(
            remote_id.clone(),
            "Root body.\n",
            8,
            [RemoteId::new("block-1")],
        )
        .expect("shadow")
        .with_frontmatter(versioned_frontmatter(
            &remote_id,
            "2026-06-10T00:00:00.000Z",
        ));
        store.save_shadow(&mount_id, shadow).expect("save shadow");
        fs::write(
            &content_path,
            render_canonical_markdown(&CanonicalDocument::new(
                versioned_frontmatter(&remote_id, "2026-06-10T00:00:00.000Z"),
                "Root body.\n",
            )),
        )
        .expect("write cache");
        fs::write(
            &visible_path,
            render_canonical_markdown(&CanonicalDocument::new(
                versioned_frontmatter(&remote_id, "now"),
                "Local visible edit.\n",
            )),
        )
        .expect("write visible edit");
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(
            &visible_path,
            render_canonical_markdown(&CanonicalDocument::new(
                versioned_frontmatter(&remote_id, "now"),
                "Local visible edit.\n",
            )),
        )
        .expect("refresh visible mtime");

        let report = reconcile_newer_macos_file_provider_projection(
            &mut store,
            &state_root,
            Some(&visible_path),
        )
        .expect("reconcile visible projection");

        assert_eq!(report.reconciled, 1);
        let cached = fs::read_to_string(&content_path).expect("read cache");
        assert!(cached.contains("Local visible edit."), "{cached}");
        assert!(!has_unresolved_conflict_markers(&cached), "{cached}");
        let entity = store
            .get_entity(&mount_id, &remote_id)
            .expect("get entity")
            .expect("entity");
        assert_eq!(entity.hydration, HydrationState::Dirty);

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

    #[cfg(unix)]
    #[test]
    fn write_binary_atomic_falls_back_to_existing_file_overwrite() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root("loc-file-provider-atomic-write-fallback");
        fs::create_dir_all(&root).expect("create root");
        let path = root.join("page.md");
        fs::write(&path, "old").expect("write existing file");
        let mut readonly = fs::metadata(&root).expect("metadata").permissions();
        readonly.set_mode(0o555);
        fs::set_permissions(&root, readonly).expect("make parent readonly");

        let result = super::write_binary_atomic(&path, b"new");

        let mut writable = fs::metadata(&root).expect("metadata").permissions();
        writable.set_mode(0o755);
        fs::set_permissions(&root, writable).expect("restore parent permissions");
        result.expect("overwrite existing file");
        assert_eq!(fs::read_to_string(&path).expect("read file"), "new");
        let _ = fs::remove_dir_all(root);
    }
}
