//! `loc pull` orchestration.
//!
//! Pull is the read-side bridge between connector output, store state, and the
//! real file tree. Mount-root pulls enumerate the remote projection and write
//! stubs; page-file pulls hydrate one entity and persist its shadow snapshot.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use locality_connector::{ChildContainer, EnumerateRequest, ListChildrenRequest};
use locality_core::canonical::{parse_canonical_markdown, render_canonical_markdown};
use locality_core::conflict::{
    has_unresolved_conflict_markers, local_version_from_conflict_markers,
    render_inline_conflict_markdown_with_base,
};
use locality_core::freshness::RemoteVersion;
use locality_core::hydration::{HydrationReason, HydrationRequest};
use locality_core::model::{CanonicalDocument, EntityKind, HydrationState, RemoteId, TreeEntry};
use locality_core::path_projection::{
    is_page_document_path, named_markdown_page_workspace_entity_path, page_container_path,
    page_listing_parent_path,
};
use locality_core::shadow::ShadowDocument;
use locality_store::{
    EntityRecord, EntityRepository, MountConfig, MountRepository, ProjectionMode,
    RemoteObservationRecord, ShadowRepository, StoreError,
};
use serde::{Deserialize, Serialize};

use crate::file_provider::{self, ProjectionRefreshBase};
use crate::hydration::{HydratedAsset, HydratedEntity, write_hydrated_asset_files};
use crate::media::{
    document_with_absolute_media_hrefs, has_missing_local_media_hrefs,
    render_document_with_absolute_media_hrefs, replace_hydrated_media_manifest,
    update_hydrated_media_manifest,
};
use crate::shadow_match::{
    contents_changes_retain_current_shadow_blocks, parsed_matches_shadow, shadows_match,
};
use crate::source::SourceAdapter;
use crate::virtual_fs::{virtual_fs_content_path, virtual_fs_content_root};

const DATABASE_DIRECTORY_ROW_HYDRATION_LIMIT: isize = 5;

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
    #[serde(default)]
    pub conflicts: Vec<PullConflict>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullConflict {
    pub path: String,
    pub remote_id: String,
}

pub fn run_pull<S, Source>(
    store: &mut S,
    source: &Source,
    target_path: impl AsRef<Path>,
) -> Result<PullReport, PullError>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + locality_store::VirtualMutationRepository
        + locality_store::FreshnessStateRepository
        + locality_store::RemoteObservationRepository,
    Source: SourceAdapter + Clone,
{
    run_pull_with_state_root(store, source, target_path, None)
}

pub fn run_pull_with_state_root<S, Source>(
    store: &mut S,
    source: &Source,
    target_path: impl AsRef<Path>,
    state_root: Option<&Path>,
) -> Result<PullReport, PullError>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + locality_store::VirtualMutationRepository
        + locality_store::FreshnessStateRepository
        + locality_store::RemoteObservationRepository,
    Source: SourceAdapter + Clone,
{
    let target_path = absolute_path(target_path.as_ref())?;
    let mounts = store.load_mounts().map_err(PullError::Store)?;
    let (mount, matched) = find_mount_for_path(&mounts, &target_path)
        .ok_or_else(|| PullError::MountNotFound(target_path.clone()))?;
    let mount = mount.clone();
    let relative_path =
        resolve_virtual_named_markdown_page_workspace_path(store, &mount, &matched.relative_path)?;
    let source = source.scoped_to_mount(&mount);
    let page_directory = page_directory_target(store, &mount, &relative_path)?;
    let refresh_bases = prepare_visible_projection_pull(
        store,
        state_root,
        &mount,
        &relative_path,
        &target_path,
        page_directory.is_some(),
    )?;

    let report = if should_pull_workspace_virtual_mount_root(&mount, &relative_path) {
        pull_workspace_virtual_mount_root(store, &source, &mount, target_path.clone())
    } else if should_pull_mount_root(
        &mount,
        &relative_path,
        &target_path,
        page_directory.is_some(),
    ) {
        pull_mount_root(store, &source, &mount, target_path.clone(), state_root)
    } else if let Some(page_directory) = page_directory {
        pull_page_directory_path(
            store,
            &source,
            &mount,
            page_directory,
            target_path.clone(),
            state_root,
        )
    } else if let Some(report) = pull_virtual_directory_path(
        store,
        &source,
        &mount,
        &relative_path,
        target_path.clone(),
        state_root,
    )? {
        Ok(report)
    } else {
        pull_entity_path(
            store,
            &source,
            &mount,
            &relative_path,
            target_path.clone(),
            state_root,
        )
    }?;

    refresh_visible_projection_after_pull(
        store,
        state_root,
        &target_path,
        &report,
        &refresh_bases,
    )?;
    Ok(report)
}

fn pull_workspace_virtual_mount_root<S, Source>(
    store: &mut S,
    source: &Source,
    mount: &MountConfig,
    target_path: PathBuf,
) -> Result<PullReport, PullError>
where
    S: EntityRepository,
    Source: SourceAdapter,
{
    let result = source
        .list_children(ListChildrenRequest {
            mount_id: mount.mount_id.clone(),
            container: ChildContainer::Root,
            parent_path: PathBuf::new(),
        })
        .map_err(PullError::Connector)?;
    let enumerated = result.entries.len();
    for entry in result.entries {
        let existing = store
            .get_entity(&entry.mount_id, &entry.remote_id)
            .map_err(PullError::Store)?;
        let record = virtual_child_entity_record(entry, existing.as_ref());
        store.save_entity(record).map_err(PullError::Store)?;
    }

    Ok(PullReport {
        ok: true,
        command: "pull".to_string(),
        via: "cli".to_string(),
        mount_id: mount.mount_id.0.clone(),
        root: mount.root.display().to_string(),
        target: target_path.display().to_string(),
        enumerated,
        stubbed: 0,
        hydrated: 0,
        skipped_dirty: 0,
        conflicts: Vec::new(),
    })
}

fn prepare_visible_projection_pull<S>(
    store: &mut S,
    state_root: Option<&Path>,
    mount: &MountConfig,
    relative_path: &Path,
    target_path: &Path,
    is_page_directory_target: bool,
) -> Result<Vec<ProjectionRefreshBase>, PullError>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + locality_store::VirtualMutationRepository
        + locality_store::FreshnessStateRepository,
{
    let Some(state_root) = state_root else {
        return Ok(Vec::new());
    };
    if !matches!(
        mount.projection,
        locality_store::ProjectionMode::MacosFileProvider
            | locality_store::ProjectionMode::WindowsCloudFiles
    ) {
        return Ok(Vec::new());
    }
    if mount.projection.uses_virtual_filesystem()
        && !is_page_directory_target
        && !is_page_document_path(relative_path)
    {
        return Ok(Vec::new());
    }

    let refresh_bases = file_provider::visible_projection_refresh_bases(store, Some(target_path))
        .map_err(PullError::Projection)?;
    if !refresh_bases.is_empty() {
        file_provider::reconcile_newer_macos_file_provider_projection(
            store,
            state_root,
            Some(target_path),
        )
        .map_err(PullError::Projection)?;
    }
    Ok(refresh_bases)
}

fn refresh_visible_projection_after_pull<S>(
    store: &S,
    state_root: Option<&Path>,
    target_path: &Path,
    report: &PullReport,
    refresh_bases: &[ProjectionRefreshBase],
) -> Result<(), PullError>
where
    S: MountRepository + EntityRepository,
{
    if report.hydrated == 0 && report.conflicts.is_empty() {
        return Ok(());
    }
    let Some(state_root) = state_root else {
        return Ok(());
    };

    file_provider::refresh_visible_projection(store, state_root, Some(target_path), refresh_bases)
        .map(|_| ())
        .map_err(PullError::Projection)
}

fn pull_mount_root<S, Source>(
    store: &mut S,
    source: &Source,
    mount: &MountConfig,
    target_path: PathBuf,
    state_root: Option<&Path>,
) -> Result<PullReport, PullError>
where
    S: EntityRepository
        + ShadowRepository
        + locality_store::FreshnessStateRepository
        + locality_store::RemoteObservationRepository,
    Source: SourceAdapter,
{
    let entries = source
        .enumerate(EnumerateRequest {
            mount_id: mount.mount_id.clone(),
            cursor: None,
        })
        .map_err(PullError::Connector)?;
    let remote_move_plan = remote_move_plan(store, mount, &entries, state_root)?;
    let mut stubbed = 0;

    for entry in &entries {
        let existing = store
            .get_entity(&entry.mount_id, &entry.remote_id)
            .map_err(PullError::Store)?;
        let preserve_existing_path = remote_move_plan.should_preserve(&entry.remote_id);
        let record = merged_entity_record(entry, existing.as_ref(), preserve_existing_path);
        let projected_entry = TreeEntry {
            path: record.path.clone(),
            ..entry.clone()
        };
        store.save_entity(record).map_err(PullError::Store)?;
        rename_projection_if_needed(mount, existing.as_ref(), &projected_entry)?;
        if write_stub_if_needed(source, mount, &projected_entry, state_root)? {
            stubbed += 1;
        }
    }

    let mut hydrated = 0;
    let mut skipped_dirty = remote_move_plan.conflicts.len();
    let mut conflicts = remote_move_plan.conflicts.clone();
    if let Some(root_entry) = entries
        .first()
        .filter(|entry| should_hydrate_mount_root_entry(mount, entry))
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
        if !remote_move_plan.should_preserve(&root_entry.remote_id) {
            match hydrate_entity(store, source, mount, root_entity, state_root)? {
                HydrationOutcome::Hydrated | HydrationOutcome::MergedDirty => hydrated += 1,
                HydrationOutcome::RemoteDeleted => {}
                HydrationOutcome::SkippedDirty => skipped_dirty += 1,
                HydrationOutcome::Conflicted(conflict) => {
                    skipped_dirty += 1;
                    conflicts.push(conflict);
                }
            }
        }
    }
    let repair =
        repair_missing_media_for_hydrated_entries(store, source, mount, &entries, state_root)?;
    hydrated += repair.hydrated;
    skipped_dirty += repair.skipped_dirty;
    conflicts.extend(repair.conflicts);

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
        conflicts,
    })
}

struct MissingMediaRepairReport {
    hydrated: usize,
    skipped_dirty: usize,
    conflicts: Vec<PullConflict>,
}

fn repair_missing_media_for_hydrated_entries<S, Source>(
    store: &mut S,
    source: &Source,
    mount: &MountConfig,
    entries: &[TreeEntry],
    state_root: Option<&Path>,
) -> Result<MissingMediaRepairReport, PullError>
where
    S: EntityRepository
        + ShadowRepository
        + locality_store::FreshnessStateRepository
        + locality_store::RemoteObservationRepository,
    Source: SourceAdapter,
{
    let output_root = projection_output_root(state_root, mount)?;
    let mut report = MissingMediaRepairReport {
        hydrated: 0,
        skipped_dirty: 0,
        conflicts: Vec::new(),
    };

    for entry in entries {
        let Some(entity) = store
            .get_entity(&mount.mount_id, &entry.remote_id)
            .map_err(PullError::Store)?
        else {
            continue;
        };
        if !should_repair_missing_media_for_entity(&entity) {
            continue;
        }

        let path = projection_content_path(state_root, mount, &entity.path)?;
        if !projection_has_missing_media(&path, &entity.path, &output_root)? {
            continue;
        }

        match hydrate_entity(store, source, mount, entity, state_root)? {
            HydrationOutcome::Hydrated | HydrationOutcome::MergedDirty => report.hydrated += 1,
            HydrationOutcome::RemoteDeleted => {}
            HydrationOutcome::SkippedDirty => report.skipped_dirty += 1,
            HydrationOutcome::Conflicted(conflict) => {
                report.skipped_dirty += 1;
                report.conflicts.push(conflict);
            }
        }
    }

    Ok(report)
}

fn should_hydrate_mount_root_entry(mount: &MountConfig, entry: &TreeEntry) -> bool {
    entry.kind == EntityKind::Page
        && mount
            .remote_root_id
            .as_ref()
            .is_some_and(|remote_root_id| remote_ids_match(mount, remote_root_id, &entry.remote_id))
}

fn remote_ids_match(mount: &MountConfig, left: &RemoteId, right: &RemoteId) -> bool {
    if left == right {
        return true;
    }
    mount.connector == "notion"
        && compact_remote_id(left.as_str()) == compact_remote_id(right.as_str())
}

fn compact_remote_id(remote_id: &str) -> String {
    remote_id
        .chars()
        .filter(|character| *character != '-')
        .collect()
}

fn should_repair_missing_media_for_entity(entity: &EntityRecord) -> bool {
    entity.kind == EntityKind::Page && entity.hydration == HydrationState::Hydrated
}

fn projection_has_missing_media(
    path: &Path,
    page_path: &Path,
    output_root: &Path,
) -> Result<bool, PullError> {
    if !path.exists() {
        return Ok(false);
    }
    let markdown = std::fs::read_to_string(path).map_err(|error| PullError::ReadFile {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    Ok(has_missing_local_media_hrefs(
        &markdown,
        page_path,
        output_root,
    ))
}

#[derive(Debug)]
struct PageDirectoryTarget {
    page: EntityRecord,
}

fn page_directory_target<S>(
    store: &S,
    mount: &MountConfig,
    relative_path: &Path,
) -> Result<Option<PageDirectoryTarget>, PullError>
where
    S: EntityRepository,
{
    if relative_path.as_os_str().is_empty() {
        return Ok(None);
    }

    let page_path = relative_path.join("page.md");
    let Some(page) = store
        .find_entity_by_path(&mount.mount_id, &page_path)
        .map_err(PullError::Store)?
    else {
        return Ok(None);
    };
    if page.kind != EntityKind::Page {
        return Ok(None);
    }

    Ok(Some(PageDirectoryTarget { page }))
}

fn resolve_virtual_named_markdown_page_workspace_path<S>(
    store: &S,
    mount: &MountConfig,
    relative_path: &Path,
) -> Result<PathBuf, PullError>
where
    S: EntityRepository,
{
    if !mount.projection.uses_virtual_filesystem() {
        return Ok(relative_path.to_path_buf());
    }
    if store
        .find_entity_by_path(&mount.mount_id, relative_path)
        .map_err(PullError::Store)?
        .is_some()
    {
        return Ok(relative_path.to_path_buf());
    }

    let Some(entity_path) = named_markdown_page_workspace_entity_path(relative_path) else {
        return Ok(relative_path.to_path_buf());
    };
    if store
        .find_entity_by_path(&mount.mount_id, &entity_path)
        .map_err(PullError::Store)?
        .is_some()
    {
        return Ok(entity_path);
    }

    Ok(relative_path.to_path_buf())
}

fn pull_page_directory_path<S, Source>(
    store: &mut S,
    source: &Source,
    mount: &MountConfig,
    target: PageDirectoryTarget,
    target_path: PathBuf,
    state_root: Option<&Path>,
) -> Result<PullReport, PullError>
where
    S: EntityRepository
        + ShadowRepository
        + locality_store::FreshnessStateRepository
        + locality_store::RemoteObservationRepository,
    Source: SourceAdapter,
{
    let PageDirectoryTarget { page } = target;
    let mut hydrated = 0;
    let mut skipped_dirty = 0;
    let mut conflicts = Vec::new();

    match hydrate_entity(store, source, mount, page.clone(), state_root)? {
        HydrationOutcome::Hydrated | HydrationOutcome::MergedDirty => hydrated += 1,
        HydrationOutcome::RemoteDeleted => {
            return Ok(PullReport {
                ok: true,
                command: "pull".to_string(),
                via: "cli".to_string(),
                mount_id: mount.mount_id.0.clone(),
                root: mount.root.display().to_string(),
                target: target_path.display().to_string(),
                enumerated: 0,
                stubbed: 0,
                hydrated: 0,
                skipped_dirty: 0,
                conflicts,
            });
        }
        HydrationOutcome::SkippedDirty => skipped_dirty += 1,
        HydrationOutcome::Conflicted(conflict) => {
            skipped_dirty += 1;
            conflicts.push(conflict);
        }
    }

    let result = source
        .list_children(ListChildrenRequest {
            mount_id: mount.mount_id.clone(),
            container: ChildContainer::PageChildren(page.remote_id.clone()),
            parent_path: page_container_path(&page.path).to_path_buf(),
        })
        .map_err(PullError::Connector)?;
    let mut enumerated = result.entries.len();
    let mut child_page_ids = Vec::new();
    for entry in result.entries {
        let child_id = entry.remote_id.clone();
        let is_page = entry.kind == EntityKind::Page;
        let existing = store
            .get_entity(&entry.mount_id, &entry.remote_id)
            .map_err(PullError::Store)?;
        let record = virtual_child_entity_record(entry, existing.as_ref());
        store.save_entity(record).map_err(PullError::Store)?;
        if is_page {
            child_page_ids.push(child_id);
        }
    }

    let mut visited = BTreeSet::new();
    let recursive_report = hydrate_page_descendants(
        store,
        source,
        mount,
        child_page_ids,
        state_root,
        &mut visited,
    )?;
    enumerated += recursive_report.enumerated;
    hydrated += recursive_report.hydrated;
    skipped_dirty += recursive_report.skipped_dirty;
    conflicts.extend(recursive_report.conflicts);

    Ok(PullReport {
        ok: skipped_dirty == 0,
        command: "pull".to_string(),
        via: "cli".to_string(),
        mount_id: mount.mount_id.0.clone(),
        root: mount.root.display().to_string(),
        target: target_path.display().to_string(),
        enumerated,
        stubbed: 0,
        hydrated,
        skipped_dirty,
        conflicts,
    })
}

fn pull_virtual_directory_path<S, Source>(
    store: &mut S,
    source: &Source,
    mount: &MountConfig,
    relative_path: &Path,
    target_path: PathBuf,
    state_root: Option<&Path>,
) -> Result<Option<PullReport>, PullError>
where
    S: EntityRepository
        + ShadowRepository
        + locality_store::FreshnessStateRepository
        + locality_store::RemoteObservationRepository,
    Source: SourceAdapter,
{
    if !mount.projection.uses_virtual_filesystem() {
        return Ok(None);
    }

    let Some(target) = virtual_directory_target(store, mount, relative_path)? else {
        return Ok(None);
    };

    let mut enumerated = 0;
    let mut row_ids = Vec::new();
    let mut page_ids = Vec::new();
    let is_database_directory = target.schema_database_id.is_some();
    let recursive_page_hydration =
        matches!(target.container, Some(ChildContainer::PageChildren(_)));
    if let Some(container) = target.container {
        let result = source
            .list_children(ListChildrenRequest {
                mount_id: mount.mount_id.clone(),
                container,
                parent_path: target.parent_path.clone(),
            })
            .map_err(PullError::Connector)?;
        if result.is_complete() {
            let returned_remote_ids = result
                .entries
                .iter()
                .map(|entry| entry.remote_id.clone())
                .collect::<BTreeSet<_>>();
            crate::virtual_fs::prune_stale_virtual_children(
                store,
                &mount.mount_id,
                &target.parent_path,
                &returned_remote_ids,
            )
            .map_err(PullError::Store)?;
        }
        enumerated = result.entries.len();
        let should_hydrate_rows = is_database_directory
            && state_root.is_some()
            && should_hydrate_database_directory_rows(
                enumerated,
                DATABASE_DIRECTORY_ROW_HYDRATION_LIMIT,
            );
        for entry in result.entries {
            let row_id = entry.remote_id.clone();
            let is_row = entry.kind == EntityKind::Page;
            let existing = store
                .get_entity(&entry.mount_id, &entry.remote_id)
                .map_err(PullError::Store)?;
            let record = virtual_child_entity_record(entry, existing.as_ref());
            store.save_entity(record).map_err(PullError::Store)?;
            if should_hydrate_rows && is_row {
                row_ids.push(row_id.clone());
            }
            if recursive_page_hydration && is_row {
                page_ids.push(row_id);
            }
        }
    }

    let mut hydrated = 0;
    let mut skipped_dirty = 0;
    let mut conflicts = Vec::new();
    if let Some(database_id) = target.schema_database_id
        && let Some(state_root) = state_root
        && let Some(schema) = source
            .database_schema_yaml(&database_id)
            .map_err(PullError::Connector)?
    {
        let directory =
            virtual_fs_content_root(state_root, &mount.mount_id).join(&target.parent_path);
        write_atomic(&directory.join("_schema.yaml"), schema)?;
    }

    for row_id in row_ids {
        let Some(row) = store
            .get_entity(&mount.mount_id, &row_id)
            .map_err(PullError::Store)?
        else {
            continue;
        };
        if matches!(
            row.hydration,
            HydrationState::Dirty | HydrationState::Conflicted
        ) {
            continue;
        }
        match hydrate_entity(store, source, mount, row, state_root)? {
            HydrationOutcome::Hydrated | HydrationOutcome::MergedDirty => hydrated += 1,
            HydrationOutcome::RemoteDeleted => {}
            HydrationOutcome::SkippedDirty => skipped_dirty += 1,
            HydrationOutcome::Conflicted(conflict) => {
                skipped_dirty += 1;
                conflicts.push(conflict);
            }
        }
    }

    if recursive_page_hydration {
        let mut visited = BTreeSet::new();
        let recursive_report =
            hydrate_page_descendants(store, source, mount, page_ids, state_root, &mut visited)?;
        enumerated += recursive_report.enumerated;
        hydrated += recursive_report.hydrated;
        skipped_dirty += recursive_report.skipped_dirty;
        conflicts.extend(recursive_report.conflicts);
    }

    Ok(Some(PullReport {
        ok: skipped_dirty == 0,
        command: "pull".to_string(),
        via: "cli".to_string(),
        mount_id: mount.mount_id.0.clone(),
        root: mount.root.display().to_string(),
        target: target_path.display().to_string(),
        enumerated,
        stubbed: 0,
        hydrated,
        skipped_dirty,
        conflicts,
    }))
}

fn should_hydrate_database_directory_rows(row_count: usize, limit: isize) -> bool {
    limit >= 0 && row_count <= limit as usize
}

#[derive(Debug, Default)]
struct RecursivePageHydrationReport {
    enumerated: usize,
    hydrated: usize,
    skipped_dirty: usize,
    conflicts: Vec<PullConflict>,
}

fn hydrate_page_descendants<S, Source>(
    store: &mut S,
    source: &Source,
    mount: &MountConfig,
    page_ids: Vec<locality_core::model::RemoteId>,
    state_root: Option<&Path>,
    visited: &mut BTreeSet<locality_core::model::RemoteId>,
) -> Result<RecursivePageHydrationReport, PullError>
where
    S: EntityRepository
        + ShadowRepository
        + locality_store::FreshnessStateRepository
        + locality_store::RemoteObservationRepository,
    Source: SourceAdapter,
{
    let mut report = RecursivePageHydrationReport::default();

    for page_id in page_ids {
        if !visited.insert(page_id.clone()) {
            continue;
        }
        let Some(page) = store
            .get_entity(&mount.mount_id, &page_id)
            .map_err(PullError::Store)?
        else {
            continue;
        };
        if page.kind != EntityKind::Page {
            continue;
        }

        match hydrate_entity(store, source, mount, page.clone(), state_root)? {
            HydrationOutcome::Hydrated | HydrationOutcome::MergedDirty => report.hydrated += 1,
            HydrationOutcome::RemoteDeleted => continue,
            HydrationOutcome::SkippedDirty => {
                report.skipped_dirty += 1;
                continue;
            }
            HydrationOutcome::Conflicted(conflict) => {
                report.skipped_dirty += 1;
                report.conflicts.push(conflict);
                continue;
            }
        }

        let result = source
            .list_children(ListChildrenRequest {
                mount_id: mount.mount_id.clone(),
                container: ChildContainer::PageChildren(page.remote_id.clone()),
                parent_path: page_container_path(&page.path).to_path_buf(),
            })
            .map_err(PullError::Connector)?;
        report.enumerated += result.entries.len();

        let mut child_page_ids = Vec::new();
        for entry in result.entries {
            let child_id = entry.remote_id.clone();
            let is_page = entry.kind == EntityKind::Page;
            let existing = store
                .get_entity(&entry.mount_id, &entry.remote_id)
                .map_err(PullError::Store)?;
            let record = virtual_child_entity_record(entry, existing.as_ref());
            store.save_entity(record).map_err(PullError::Store)?;
            if is_page {
                child_page_ids.push(child_id);
            }
        }

        let child_report =
            hydrate_page_descendants(store, source, mount, child_page_ids, state_root, visited)?;
        report.enumerated += child_report.enumerated;
        report.hydrated += child_report.hydrated;
        report.skipped_dirty += child_report.skipped_dirty;
        report.conflicts.extend(child_report.conflicts);
    }

    Ok(report)
}

fn virtual_child_entity_record(entry: TreeEntry, existing: Option<&EntityRecord>) -> EntityRecord {
    let mut record = EntityRecord::from(entry);
    if let Some(existing) = existing {
        let path_changed = record.path != existing.path;
        if matches!(
            existing.hydration,
            HydrationState::Dirty | HydrationState::Conflicted
        ) {
            record.path = existing.path.clone();
            record.hydration = existing.hydration.clone();
            record.content_hash = existing.content_hash.clone();
        } else if !path_changed {
            record.hydration = existing.hydration.clone();
            record.content_hash = existing.content_hash.clone();
        }
    }
    record
}

#[derive(Debug)]
struct VirtualDirectoryTarget {
    parent_path: PathBuf,
    container: Option<ChildContainer>,
    schema_database_id: Option<locality_core::model::RemoteId>,
}

fn virtual_directory_target<S>(
    store: &S,
    mount: &MountConfig,
    relative_path: &Path,
) -> Result<Option<VirtualDirectoryTarget>, PullError>
where
    S: EntityRepository,
{
    if relative_path.as_os_str().is_empty() {
        return Ok(None);
    }

    let page_container_target = store
        .list_entities(&mount.mount_id)
        .map_err(PullError::Store)?
        .into_iter()
        .find_map(|entity| {
            if entity.kind == EntityKind::Page && page_container_path(&entity.path) == relative_path
            {
                Some(VirtualDirectoryTarget {
                    parent_path: relative_path.to_path_buf(),
                    container: Some(ChildContainer::PageChildren(entity.remote_id)),
                    schema_database_id: None,
                })
            } else {
                None
            }
        });
    if page_container_target.is_some() {
        return Ok(page_container_target);
    }

    if let Some(entity) = store
        .find_entity_by_path(&mount.mount_id, relative_path)
        .map_err(PullError::Store)?
    {
        return Ok(match entity.kind {
            EntityKind::Database => Some(VirtualDirectoryTarget {
                parent_path: entity.path,
                container: Some(ChildContainer::DatabaseRows(entity.remote_id.clone())),
                schema_database_id: Some(entity.remote_id),
            }),
            EntityKind::Directory => Some(VirtualDirectoryTarget {
                parent_path: entity.path,
                container: Some(ChildContainer::DirectoryChildren(entity.remote_id)),
                schema_database_id: None,
            }),
            EntityKind::Page | EntityKind::Asset | EntityKind::Unknown(_) => None,
        });
    }

    Ok(None)
}

fn pull_entity_path<S, Source>(
    store: &mut S,
    source: &Source,
    mount: &MountConfig,
    relative_path: &Path,
    target_path: PathBuf,
    state_root: Option<&Path>,
) -> Result<PullReport, PullError>
where
    S: EntityRepository
        + ShadowRepository
        + locality_store::FreshnessStateRepository
        + locality_store::RemoteObservationRepository,
    Source: SourceAdapter,
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

    let outcome = hydrate_entity(store, source, mount, entity, state_root)?;
    let (hydrated, skipped_dirty, conflicts) = match outcome {
        HydrationOutcome::Hydrated | HydrationOutcome::MergedDirty => (1, 0, Vec::new()),
        HydrationOutcome::RemoteDeleted => (0, 0, Vec::new()),
        HydrationOutcome::SkippedDirty => (0, 1, Vec::new()),
        HydrationOutcome::Conflicted(conflict) => (0, 1, vec![conflict]),
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
        conflicts,
    })
}

fn merged_entity_record(
    entry: &TreeEntry,
    existing: Option<&EntityRecord>,
    preserve_existing_path: bool,
) -> EntityRecord {
    let mut record = EntityRecord::from(entry.clone());

    if let Some(existing) = existing {
        if preserve_existing_path
            || matches!(
                existing.hydration,
                HydrationState::Dirty | HydrationState::Conflicted
            )
        {
            record.path = existing.path.clone();
        }
        record.hydration = existing.hydration.clone();
        if preserve_existing_path && record.hydration.can_transition_to(&HydrationState::Dirty) {
            record.hydration = HydrationState::Dirty;
        }
        record.content_hash = existing.content_hash.clone();
        if remote_precondition_belongs_to_shadow(existing) {
            record.remote_edited_at = existing.remote_edited_at.clone();
        }
    }

    record
}

#[derive(Debug, Default)]
struct RemoteMovePlan {
    preserve_remote_ids: BTreeSet<RemoteId>,
    conflicts: Vec<PullConflict>,
}

impl RemoteMovePlan {
    fn should_preserve(&self, remote_id: &RemoteId) -> bool {
        self.preserve_remote_ids.contains(remote_id)
    }
}

#[derive(Debug)]
struct BlockedProjectionMove {
    source_root: PathBuf,
    destination_root: PathBuf,
}

fn remote_move_plan<S>(
    store: &S,
    mount: &MountConfig,
    entries: &[TreeEntry],
    state_root: Option<&Path>,
) -> Result<RemoteMovePlan, PullError>
where
    S: EntityRepository + ShadowRepository,
{
    let existing_entities = store
        .list_entities(&mount.mount_id)
        .map_err(PullError::Store)?;
    let existing_by_remote_id = existing_entities
        .iter()
        .map(|entity| (entity.remote_id.clone(), entity))
        .collect::<BTreeMap<_, _>>();
    let mut blocked_moves = Vec::new();
    let mut conflict_ids = BTreeSet::new();
    let mut conflicts = Vec::new();

    for entry in entries {
        let Some(existing) = existing_by_remote_id.get(&entry.remote_id) else {
            continue;
        };
        if existing.path == entry.path {
            continue;
        }

        let source_root = projection_subtree_path(&existing.kind, &existing.path);
        let destination_root = projection_subtree_path(&entry.kind, &entry.path);
        let blockers =
            remote_move_blockers(store, mount, state_root, &existing_entities, &source_root)?;
        if blockers.is_empty() {
            continue;
        }

        blocked_moves.push(BlockedProjectionMove {
            source_root,
            destination_root,
        });
        for blocker in blockers {
            if conflict_ids.insert(blocker.remote_id.clone()) {
                conflicts.push(pull_conflict(mount, blocker));
            }
        }
    }

    let mut preserve_remote_ids = BTreeSet::new();
    for entry in entries {
        let Some(existing) = existing_by_remote_id.get(&entry.remote_id) else {
            continue;
        };
        if should_preserve_for_blocked_move(existing, entry, &blocked_moves) {
            preserve_remote_ids.insert(entry.remote_id.clone());
        }
    }

    Ok(RemoteMovePlan {
        preserve_remote_ids,
        conflicts,
    })
}

fn remote_move_blockers<'a, S>(
    store: &S,
    mount: &MountConfig,
    state_root: Option<&Path>,
    existing_entities: &'a [EntityRecord],
    source_root: &Path,
) -> Result<Vec<&'a EntityRecord>, PullError>
where
    S: ShadowRepository,
{
    let mut blockers = Vec::new();
    for entity in existing_entities {
        let candidate_root = projection_subtree_path(&entity.kind, &entity.path);
        if !path_in_projection_subtree(&candidate_root, source_root) {
            continue;
        }
        if can_move_local_projection(store, mount, state_root, entity)? {
            continue;
        }
        blockers.push(entity);
    }
    Ok(blockers)
}

fn can_move_local_projection<S>(
    store: &S,
    mount: &MountConfig,
    state_root: Option<&Path>,
    entity: &EntityRecord,
) -> Result<bool, PullError>
where
    S: ShadowRepository,
{
    if matches!(
        entity.hydration,
        HydrationState::Dirty | HydrationState::Conflicted
    ) {
        return Ok(false);
    }
    if entity.kind != EntityKind::Page {
        return Ok(true);
    }
    let path = projection_content_path(state_root, mount, &entity.path)?;
    can_replace_file(store, mount, entity, &path)
}

fn should_preserve_for_blocked_move(
    existing: &EntityRecord,
    entry: &TreeEntry,
    blocked_moves: &[BlockedProjectionMove],
) -> bool {
    if existing.path == entry.path {
        return false;
    }

    let existing_root = projection_subtree_path(&existing.kind, &existing.path);
    let entry_root = projection_subtree_path(&entry.kind, &entry.path);
    blocked_moves.iter().any(|blocked| {
        path_in_projection_subtree(&existing_root, &blocked.source_root)
            && path_in_projection_subtree(&entry_root, &blocked.destination_root)
    })
}

fn projection_subtree_path(kind: &EntityKind, path: &Path) -> PathBuf {
    match kind {
        EntityKind::Page => page_container_path(path),
        EntityKind::Database
        | EntityKind::Directory
        | EntityKind::Asset
        | EntityKind::Unknown(_) => path.to_path_buf(),
    }
}

fn path_in_projection_subtree(path: &Path, subtree: &Path) -> bool {
    subtree.as_os_str().is_empty() || path == subtree || path.starts_with(subtree)
}

fn write_stub_if_needed<Source>(
    source: &Source,
    mount: &MountConfig,
    entry: &TreeEntry,
    state_root: Option<&Path>,
) -> Result<bool, PullError>
where
    Source: SourceAdapter,
{
    if mount.projection.uses_virtual_filesystem() {
        if entry.kind == EntityKind::Database
            && let Some(state_root) = state_root
        {
            let directory = virtual_fs_content_root(state_root, &mount.mount_id).join(&entry.path);
            if let Some(schema) = source
                .database_schema_yaml(&entry.remote_id)
                .map_err(PullError::Connector)?
            {
                write_atomic(&directory.join("_schema.yaml"), schema)?;
            }
        }
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
            if let Some(schema) = source
                .database_schema_yaml(&entry.remote_id)
                .map_err(PullError::Connector)?
            {
                write_atomic(&directory.join("_schema.yaml"), schema)?;
            }
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

fn rename_projection_if_needed(
    mount: &MountConfig,
    existing: Option<&EntityRecord>,
    entry: &TreeEntry,
) -> Result<(), PullError> {
    if mount.projection.uses_virtual_filesystem() {
        return Ok(());
    }

    let Some(existing) = existing else {
        return Ok(());
    };
    if existing.path == entry.path {
        return Ok(());
    }

    match entry.kind {
        EntityKind::Page => {
            rename_page_projection_if_needed(mount, &existing.path, &entry.path)?;
        }
        EntityKind::Database
        | EntityKind::Directory
        | EntityKind::Asset
        | EntityKind::Unknown(_) => {
            rename_projected_path(
                &mount.root.join(&existing.path),
                &mount.root.join(&entry.path),
            )?;
        }
    }

    Ok(())
}

fn rename_page_projection_if_needed(
    mount: &MountConfig,
    existing_path: &Path,
    entry_path: &Path,
) -> Result<(), PullError> {
    if existing_path == entry_path {
        return Ok(());
    }

    if is_page_document_path(existing_path) {
        let existing_container = page_container_path(existing_path);
        let entry_container = page_container_path(entry_path);
        if existing_container != entry_container {
            rename_projected_path(
                &mount.root.join(existing_container),
                &mount.root.join(entry_container),
            )?;
        } else {
            rename_projected_path(
                &mount.root.join(existing_path),
                &mount.root.join(entry_path),
            )?;
        }
        return Ok(());
    }

    let existing_file = mount.root.join(existing_path);
    let legacy_child_dir = mount.root.join(page_container_path(existing_path));
    let entry_container = mount.root.join(page_container_path(entry_path));
    let entry_file = mount.root.join(entry_path);

    if legacy_child_dir.exists() && legacy_child_dir != entry_container {
        rename_projected_path(&legacy_child_dir, &entry_container)?;
    } else if !entry_container.exists() {
        std::fs::create_dir_all(&entry_container).map_err(|error| PullError::WriteFile {
            path: entry_container.clone(),
            message: error.to_string(),
        })?;
    }

    rename_projected_path(&existing_file, &entry_file)?;
    Ok(())
}

fn hydrate_entity<S, Source>(
    store: &mut S,
    source: &Source,
    mount: &MountConfig,
    entity: EntityRecord,
    state_root: Option<&Path>,
) -> Result<HydrationOutcome, PullError>
where
    S: EntityRepository
        + ShadowRepository
        + locality_store::FreshnessStateRepository
        + locality_store::RemoteObservationRepository,
    Source: SourceAdapter,
{
    let path = projection_content_path(state_root, mount, &entity.path)?;
    let can_replace = can_replace_file(store, mount, &entity, &path)?;
    let rendered = match source.fetch_render(&HydrationRequest::new(
        mount.mount_id.clone(),
        entity.remote_id.clone(),
        entity.path.clone(),
        HydrationState::Hydrated,
        HydrationReason::ExplicitPull,
    )) {
        Ok(rendered) => rendered,
        Err(error) if is_remote_not_found(&error) => {
            return reconcile_remote_not_found(store, mount, entity, &path, can_replace);
        }
        Err(error) => return Err(PullError::Connector(error)),
    };
    let media_root = projection_output_root(state_root, mount)?;
    write_parent_database_schema_cache(store, source, mount, &entity, &media_root)?;
    write_assets(&media_root, &rendered.assets)?;

    if can_replace {
        accept_remote_projection(store, mount, entity, &path, &media_root, rendered)?;
        return Ok(HydrationOutcome::Hydrated);
    }

    if file_has_unresolved_conflict_markers(&path)? {
        let conflict = pull_conflict(mount, &entity);
        if same_version_shadow_drifted(store, mount, &entity, &rendered)? {
            return match refresh_existing_conflict(
                store,
                mount,
                entity,
                &path,
                &media_root,
                rendered,
                true,
            )? {
                DirtyRemoteDriftOutcome::Merged => Ok(HydrationOutcome::MergedDirty),
                DirtyRemoteDriftOutcome::Conflicted => Ok(HydrationOutcome::Conflicted(conflict)),
            };
        } else if same_remote_version(&entity, &rendered) {
            return match refresh_existing_conflict(
                store,
                mount,
                entity,
                &path,
                &media_root,
                rendered,
                false,
            )? {
                DirtyRemoteDriftOutcome::Merged => Ok(HydrationOutcome::MergedDirty),
                DirtyRemoteDriftOutcome::Conflicted => Ok(HydrationOutcome::Conflicted(conflict)),
            };
        }
        store
            .save_entity(mark_conflicted_if_allowed(entity))
            .map_err(PullError::Store)?;
        return Ok(HydrationOutcome::Conflicted(conflict));
    } else if !remote_matches_shadow(store, mount, &entity, &rendered.shadow)? {
        let conflict = pull_conflict(mount, &entity);
        return match materialize_conflict(store, mount, entity, &path, &media_root, rendered)? {
            DirtyRemoteDriftOutcome::Merged => Ok(HydrationOutcome::MergedDirty),
            DirtyRemoteDriftOutcome::Conflicted => Ok(HydrationOutcome::Conflicted(conflict)),
        };
    } else {
        store
            .save_entity(mark_dirty_if_allowed(entity))
            .map_err(PullError::Store)?;
    }

    Ok(HydrationOutcome::SkippedDirty)
}

fn reconcile_remote_not_found<S>(
    store: &mut S,
    mount: &MountConfig,
    entity: EntityRecord,
    path: &Path,
    can_replace: bool,
) -> Result<HydrationOutcome, PullError>
where
    S: EntityRepository
        + locality_store::FreshnessStateRepository
        + locality_store::RemoteObservationRepository,
{
    record_deleted_remote_observation(store, mount, &entity)?;
    if !can_replace {
        store
            .save_entity(mark_dirty_if_allowed(entity))
            .map_err(PullError::Store)?;
        return Ok(HydrationOutcome::SkippedDirty);
    }

    remove_clean_projection(path)?;
    store
        .delete_entity(&mount.mount_id, &entity.remote_id)
        .map_err(PullError::Store)?;
    Ok(HydrationOutcome::RemoteDeleted)
}

fn record_deleted_remote_observation<S>(
    store: &mut S,
    mount: &MountConfig,
    entity: &EntityRecord,
) -> Result<(), PullError>
where
    S: locality_store::FreshnessStateRepository + locality_store::RemoteObservationRepository,
{
    let observed_at = crate::freshness::freshness_timestamp();
    let observation = RemoteObservationRecord::new(
        mount.mount_id.clone(),
        entity.remote_id.clone(),
        entity.kind.clone(),
        entity.title.clone(),
        entity.path.clone(),
        observed_at.clone(),
    )
    .deleted(true);
    store
        .save_remote_observation(observation)
        .map_err(PullError::Store)?;

    if let Some(mut freshness) = store
        .get_freshness_state(&mount.mount_id, &entity.remote_id)
        .map_err(PullError::Store)?
    {
        freshness.remote_hint_pending = true;
        freshness.last_checked_at = Some(observed_at);
        store
            .save_freshness_state(freshness)
            .map_err(PullError::Store)?;
    }

    Ok(())
}

fn remove_clean_projection(path: &Path) -> Result<(), PullError> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(PullError::WriteFile {
                path: path.to_path_buf(),
                message: error.to_string(),
            });
        }
    }

    if path.file_name().is_some_and(|name| name == "page.md")
        && let Some(directory) = path.parent()
    {
        let _ = std::fs::remove_dir(directory);
    }
    Ok(())
}

fn is_remote_not_found(error: &locality_core::LocalityError) -> bool {
    match error {
        locality_core::LocalityError::RemoteNotFound(_) => true,
        locality_core::LocalityError::Io(message) => {
            message.contains("HTTP 404") && message.contains("object_not_found")
        }
        _ => false,
    }
}

fn projection_content_path(
    state_root: Option<&Path>,
    mount: &MountConfig,
    relative_path: &Path,
) -> Result<PathBuf, PullError> {
    if mount.projection.uses_virtual_filesystem()
        && let Some(state_root) = state_root
    {
        return virtual_fs_content_path(state_root, &mount.mount_id, relative_path).map_err(
            |error| PullError::WriteFile {
                path: relative_path.to_path_buf(),
                message: error.to_string(),
            },
        );
    }

    Ok(mount.root.join(relative_path))
}

fn projection_output_root(
    state_root: Option<&Path>,
    mount: &MountConfig,
) -> Result<PathBuf, PullError> {
    if mount.projection.uses_virtual_filesystem()
        && let Some(state_root) = state_root
    {
        return Ok(virtual_fs_content_root(state_root, &mount.mount_id));
    }

    Ok(mount.root.clone())
}

fn write_parent_database_schema_cache<S, Source>(
    store: &S,
    source: &Source,
    mount: &MountConfig,
    entity: &EntityRecord,
    output_root: &Path,
) -> Result<(), PullError>
where
    S: EntityRepository,
    Source: SourceAdapter,
{
    let Some(database) = parent_database_entity(store, mount, entity)? else {
        return Ok(());
    };
    let Some(schema) = source
        .database_schema_yaml(&database.remote_id)
        .map_err(PullError::Connector)?
    else {
        return Ok(());
    };
    write_atomic(
        &output_root.join(&database.path).join("_schema.yaml"),
        schema,
    )
}

fn parent_database_entity<S>(
    store: &S,
    mount: &MountConfig,
    entity: &EntityRecord,
) -> Result<Option<EntityRecord>, PullError>
where
    S: EntityRepository,
{
    if entity.kind != EntityKind::Page {
        return Ok(None);
    }
    let parent_path = page_listing_parent_path(&entity.path);
    if parent_path.as_os_str().is_empty() {
        return Ok(None);
    }
    Ok(store
        .find_entity_by_path(&mount.mount_id, &parent_path)
        .map_err(PullError::Store)?
        .filter(|entity| entity.kind == EntityKind::Database))
}

fn write_assets(root: &Path, assets: &[HydratedAsset]) -> Result<(), PullError> {
    write_hydrated_asset_files(root, assets).map_err(PullError::Connector)?;
    update_hydrated_media_manifest(root, assets).map_err(PullError::Connector)?;
    Ok(())
}

fn should_pull_mount_root(
    mount: &MountConfig,
    relative_path: &Path,
    target_path: &Path,
    page_directory_target: bool,
) -> bool {
    if relative_path.as_os_str().is_empty() {
        return true;
    }
    if mount.projection.uses_virtual_filesystem() || page_directory_target {
        return false;
    }

    target_path.is_dir()
}

fn should_pull_workspace_virtual_mount_root(mount: &MountConfig, relative_path: &Path) -> bool {
    relative_path.as_os_str().is_empty()
        && mount.projection.uses_virtual_filesystem()
        && mount.remote_root_id.is_none()
}

fn accept_remote_projection<S>(
    store: &mut S,
    mount: &MountConfig,
    entity: EntityRecord,
    path: &Path,
    output_root: &Path,
    rendered: HydratedEntity,
) -> Result<(), PullError>
where
    S: EntityRepository
        + ShadowRepository
        + locality_store::FreshnessStateRepository
        + locality_store::RemoteObservationRepository,
{
    let markdown =
        render_document_with_absolute_media_hrefs(&rendered.document, &entity.path, output_root);
    replace_hydrated_media_manifest(output_root, &rendered.assets).map_err(PullError::Connector)?;
    write_atomic(path, markdown)?;
    store
        .save_shadow(&mount.mount_id, rendered.shadow.clone())
        .map_err(PullError::Store)?;
    let remote_edited_at = rendered.remote_edited_at.clone();
    let entity = hydrated_record(entity, rendered.shadow, remote_edited_at.clone());
    store
        .save_entity(entity.clone())
        .map_err(PullError::Store)?;
    record_synced_remote_observation(store, mount, &entity, remote_edited_at)?;

    Ok(())
}

fn record_synced_remote_observation<S>(
    store: &mut S,
    mount: &MountConfig,
    entity: &EntityRecord,
    remote_edited_at: Option<String>,
) -> Result<(), PullError>
where
    S: locality_store::FreshnessStateRepository + locality_store::RemoteObservationRepository,
{
    let observed_at = crate::freshness::freshness_timestamp();
    let mut observation = locality_store::RemoteObservationRecord::new(
        mount.mount_id.clone(),
        entity.remote_id.clone(),
        entity.kind.clone(),
        entity.title.clone(),
        entity.path.clone(),
        observed_at.clone(),
    );
    if let Some(remote_edited_at) = remote_edited_at {
        observation = observation.with_remote_version(RemoteVersion::new(remote_edited_at));
    }
    store
        .save_remote_observation(observation)
        .map_err(PullError::Store)?;

    if let Some(mut freshness) = store
        .get_freshness_state(&mount.mount_id, &entity.remote_id)
        .map_err(PullError::Store)?
    {
        freshness.remote_hint_pending = false;
        freshness.last_checked_at = Some(observed_at);
        store
            .save_freshness_state(freshness)
            .map_err(PullError::Store)?;
    }

    Ok(())
}

fn same_version_shadow_drifted<S>(
    store: &S,
    mount: &MountConfig,
    entity: &EntityRecord,
    rendered: &HydratedEntity,
) -> Result<bool, PullError>
where
    S: ShadowRepository,
{
    if !same_remote_version(entity, rendered) {
        return Ok(false);
    }

    Ok(!remote_matches_shadow(
        store,
        mount,
        entity,
        &rendered.shadow,
    )?)
}

fn same_remote_version(entity: &EntityRecord, rendered: &HydratedEntity) -> bool {
    rendered.remote_edited_at.is_some()
        && rendered.remote_edited_at.as_deref() == entity.remote_edited_at.as_deref()
}

fn refresh_existing_conflict<S>(
    store: &mut S,
    mount: &MountConfig,
    entity: EntityRecord,
    path: &Path,
    output_root: &Path,
    rendered: HydratedEntity,
    use_base_shadow: bool,
) -> Result<DirtyRemoteDriftOutcome, PullError>
where
    S: EntityRepository
        + ShadowRepository
        + locality_store::FreshnessStateRepository
        + locality_store::RemoteObservationRepository,
{
    let contents = std::fs::read_to_string(path).map_err(|error| PullError::ReadFile {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let Some(local_contents) = local_version_from_conflict_markers(&contents) else {
        store
            .save_entity(mark_conflicted_if_allowed(entity))
            .map_err(PullError::Store)?;
        return Ok(DirtyRemoteDriftOutcome::Conflicted);
    };
    materialize_conflict_from_contents(
        store,
        mount,
        entity,
        path,
        output_root,
        rendered,
        local_contents,
        use_base_shadow,
    )
}

fn materialize_conflict<S>(
    store: &mut S,
    mount: &MountConfig,
    entity: EntityRecord,
    path: &Path,
    output_root: &Path,
    rendered: HydratedEntity,
) -> Result<DirtyRemoteDriftOutcome, PullError>
where
    S: EntityRepository
        + ShadowRepository
        + locality_store::FreshnessStateRepository
        + locality_store::RemoteObservationRepository,
{
    let local_contents = std::fs::read_to_string(path).map_err(|error| PullError::ReadFile {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    materialize_conflict_from_contents(
        store,
        mount,
        entity,
        path,
        output_root,
        rendered,
        local_contents,
        true,
    )
}

fn materialize_conflict_from_contents<S>(
    store: &mut S,
    mount: &MountConfig,
    entity: EntityRecord,
    path: &Path,
    output_root: &Path,
    rendered: HydratedEntity,
    local_contents: String,
    use_base_shadow: bool,
) -> Result<DirtyRemoteDriftOutcome, PullError>
where
    S: EntityRepository
        + ShadowRepository
        + locality_store::FreshnessStateRepository
        + locality_store::RemoteObservationRepository,
{
    let base_shadow = if use_base_shadow {
        match store.load_shadow(&mount.mount_id, &entity.remote_id) {
            Ok(shadow) => Some(shadow),
            Err(StoreError::ShadowMissing { .. }) => None,
            Err(error) => return Err(PullError::Store(error)),
        }
    } else {
        None
    };
    let remote_document =
        document_with_absolute_media_hrefs(&rendered.document, &entity.path, output_root);
    let conflict_markdown = if !use_base_shadow
        && contents_changes_retain_current_shadow_blocks(&local_contents, &rendered.shadow)
    {
        local_contents
    } else {
        render_inline_conflict_markdown_with_base(
            &local_contents,
            base_shadow
                .as_ref()
                .map(|shadow| shadow.rendered_body.as_str()),
            &remote_document,
        )
    };
    let has_conflict_markers = has_unresolved_conflict_markers(&conflict_markdown);
    write_atomic(path, conflict_markdown)?;
    store
        .save_shadow(&mount.mount_id, rendered.shadow.clone())
        .map_err(PullError::Store)?;
    let remote_edited_at = rendered.remote_edited_at.clone();
    let entity = if has_conflict_markers {
        conflicted_record(entity, &rendered.shadow, remote_edited_at.clone())
    } else {
        dirty_record(entity, &rendered.shadow, remote_edited_at.clone())
    };
    store
        .save_entity(entity.clone())
        .map_err(PullError::Store)?;
    if !has_conflict_markers {
        record_synced_remote_observation(store, mount, &entity, remote_edited_at)?;
    }

    Ok(if has_conflict_markers {
        DirtyRemoteDriftOutcome::Conflicted
    } else {
        DirtyRemoteDriftOutcome::Merged
    })
}

fn pull_conflict(mount: &MountConfig, entity: &EntityRecord) -> PullConflict {
    PullConflict {
        path: projected_report_path(mount, &entity.path)
            .display()
            .to_string(),
        remote_id: entity.remote_id.0.clone(),
    }
}

fn projected_report_path(mount: &MountConfig, relative_path: &Path) -> PathBuf {
    if matches!(
        mount.projection,
        ProjectionMode::LinuxFuse | ProjectionMode::WindowsCloudFiles
    ) {
        return crate::virtual_fs::virtual_projection_mount_point(mount).join(relative_path);
    }

    mount.root.join(relative_path)
}

fn hydrated_record(
    mut entity: EntityRecord,
    shadow: ShadowDocument,
    remote_edited_at: Option<String>,
) -> EntityRecord {
    entity.hydration = HydrationState::Hydrated;
    entity.content_hash = Some(shadow.body_hash);
    if remote_edited_at.is_some() {
        entity.remote_edited_at = remote_edited_at;
    }
    entity
}

fn conflicted_record(
    mut entity: EntityRecord,
    shadow: &ShadowDocument,
    remote_edited_at: Option<String>,
) -> EntityRecord {
    if entity.hydration.can_transition_to(&HydrationState::Dirty) {
        entity.hydration = HydrationState::Dirty;
    }
    if entity
        .hydration
        .can_transition_to(&HydrationState::Conflicted)
    {
        entity.hydration = HydrationState::Conflicted;
    }
    entity.content_hash = Some(shadow.body_hash.clone());
    if remote_edited_at.is_some() {
        entity.remote_edited_at = remote_edited_at;
    }
    entity
}

fn dirty_record(
    mut entity: EntityRecord,
    shadow: &ShadowDocument,
    remote_edited_at: Option<String>,
) -> EntityRecord {
    if entity.hydration != HydrationState::Conflicted
        && entity.hydration.can_transition_to(&HydrationState::Dirty)
    {
        entity.hydration = HydrationState::Dirty;
    }
    entity.content_hash = Some(shadow.body_hash.clone());
    if remote_edited_at.is_some() {
        entity.remote_edited_at = remote_edited_at;
    }
    entity
}

fn mark_dirty_if_allowed(mut entity: EntityRecord) -> EntityRecord {
    if entity.hydration != HydrationState::Conflicted
        && entity.hydration.can_transition_to(&HydrationState::Dirty)
    {
        entity.hydration = HydrationState::Dirty;
    }
    entity
}

fn mark_conflicted_if_allowed(mut entity: EntityRecord) -> EntityRecord {
    if entity.hydration.can_transition_to(&HydrationState::Dirty) {
        entity.hydration = HydrationState::Dirty;
    }
    if entity
        .hydration
        .can_transition_to(&HydrationState::Conflicted)
    {
        entity.hydration = HydrationState::Conflicted;
    }
    entity
}

fn file_has_unresolved_conflict_markers(path: &Path) -> Result<bool, PullError> {
    let contents = std::fs::read_to_string(path).map_err(|error| PullError::ReadFile {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    Ok(has_unresolved_conflict_markers(&contents))
}

fn remote_matches_shadow<S>(
    store: &S,
    mount: &MountConfig,
    entity: &EntityRecord,
    rendered: &ShadowDocument,
) -> Result<bool, PullError>
where
    S: ShadowRepository,
{
    let shadow = match store.load_shadow(&mount.mount_id, &entity.remote_id) {
        Ok(shadow) => shadow,
        Err(StoreError::ShadowMissing { .. }) => return Ok(false),
        Err(error) => return Err(PullError::Store(error)),
    };

    Ok(shadows_match(&shadow, rendered))
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

    let contents = std::fs::read_to_string(path).map_err(|error| PullError::ReadFile {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let parsed = match parse_canonical_markdown(&contents) {
        Ok(parsed) => parsed,
        Err(_) => return Ok(false),
    };
    let shadow = match store.load_shadow(&mount.mount_id, &entity.remote_id) {
        Ok(shadow) => shadow,
        Err(StoreError::ShadowMissing { .. }) => return Ok(false),
        Err(error) => return Err(PullError::Store(error)),
    };

    Ok(parsed_matches_shadow(&parsed, &shadow))
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
        "loc:\n  id: {}\n  type: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\n",
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
        .unwrap_or("loc-write");
    let temp_path = path.with_file_name(format!(".{file_name}.loc-tmp"));
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

fn rename_projected_path(from: &Path, to: &Path) -> Result<(), PullError> {
    if from == to || !from.exists() || to.exists() {
        return Ok(());
    }

    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|error| PullError::WriteFile {
            path: parent.to_path_buf(),
            message: error.to_string(),
        })?;
    }

    std::fs::rename(from, to).map_err(|error| PullError::WriteFile {
        path: to.to_path_buf(),
        message: format!(
            "failed to rename projected path `{}` to `{}`: {error}",
            from.display(),
            to.display(),
        ),
    })
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

fn find_mount_for_path<'a>(
    mounts: &'a [MountConfig],
    path: &Path,
) -> Option<(&'a MountConfig, file_provider::MountPathMatch)> {
    file_provider::find_mount_for_path(mounts, path)
}

fn yaml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn remote_precondition_belongs_to_shadow(existing: &EntityRecord) -> bool {
    matches!(
        existing.hydration,
        HydrationState::Hydrated | HydrationState::Dirty | HydrationState::Conflicted
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum HydrationOutcome {
    Hydrated,
    MergedDirty,
    RemoteDeleted,
    SkippedDirty,
    Conflicted(PullConflict),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirtyRemoteDriftOutcome {
    Merged,
    Conflicted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PullError {
    Connector(locality_core::LocalityError),
    CurrentDir(String),
    MountNotFound(PathBuf),
    Projection(locality_core::LocalityError),
    ReadFile { path: PathBuf, message: String },
    Store(StoreError),
    WriteFile { path: PathBuf, message: String },
}

impl PullError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Connector(locality_core::LocalityError::NotImplemented(_)) => "not_implemented",
            Self::Connector(locality_core::LocalityError::RemoteNotFound(_)) => "remote_not_found",
            Self::Connector(_) => "connector_error",
            Self::CurrentDir(_) => "current_dir_failed",
            Self::MountNotFound(_) => "mount_not_found",
            Self::Projection(_) => "projection_refresh_failed",
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
                format!("no Locality mount contains `{}`", path.display())
            }
            Self::Projection(error) => format!("visible projection refresh failed: {error}"),
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use locality_connector::{
        ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, ChildContainer,
        Connector, ConnectorCapabilities, ConnectorKind, EnumerateRequest, FetchRequest,
        ListChildrenRequest, ListChildrenResult, NativeEntity, ObserveRequest, ParsedEntity,
    };
    use locality_core::LocalityResult;
    use locality_core::canonical::render_canonical_markdown;
    use locality_core::freshness::RemoteObservation;
    use locality_core::hydration::{HydrationReason, HydrationRequest};
    use locality_core::model::{CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId};
    use locality_core::planner::PushOperationKind;
    use locality_core::shadow::{ShadowDocument, segment_markdown_body};
    use locality_store::{
        EntityRecord, EntityRepository, InMemoryStateStore, MountRepository, ProjectionMode,
        ShadowRepository,
    };

    use super::{can_replace_file, write_atomic};
    use crate::hydration::{HydratedAsset, HydratedAssetMedia, HydratedEntity, HydrationSource};
    use crate::source::{SourceAdapter, SourcePushValidator};
    use locality_store::MountConfig;

    #[test]
    fn can_replace_stale_dirty_file_when_projection_matches_shadow() {
        let fixture = PullFixture::new();
        let store = fixture.store_with_shadow(
            HydrationState::Dirty,
            fixture.document("Roadmap", "# Roadmap\n\nOriginal body.\n"),
        );

        assert!(
            can_replace_file(
                &store,
                &fixture.mount,
                &fixture.entity(HydrationState::Dirty),
                &fixture.page_path,
            )
            .expect("check replace")
        );
    }

    #[test]
    fn can_replace_stale_dirty_file_when_block_diff_is_noop() {
        let fixture = PullFixture::new();
        let store = fixture.store_with_shadow(
            HydrationState::Dirty,
            fixture.document("Roadmap", "- One\n\n- Two\n"),
        );
        write_atomic(
            &fixture.page_path,
            render_canonical_markdown(&fixture.document("Roadmap", "- One\n- Two\n")),
        )
        .expect("write compacted projection");

        assert!(
            can_replace_file(
                &store,
                &fixture.mount,
                &fixture.entity(HydrationState::Dirty),
                &fixture.page_path,
            )
            .expect("check replace")
        );
    }

    #[test]
    fn can_replace_stale_dirty_file_when_only_sync_metadata_drifted() {
        let fixture = PullFixture::new();
        let body = "# Roadmap\n\nOriginal body.\n";
        let shadow_document = fixture.document_with_sync(
            "Roadmap",
            body,
            "2026-06-18T07:06:00.000Z",
            "2026-06-18T07:06:00.000Z",
        );
        let store = fixture.store_with_shadow(HydrationState::Dirty, shadow_document);
        write_atomic(
            &fixture.page_path,
            render_canonical_markdown(&fixture.document_with_sync(
                "Roadmap",
                body,
                "2026-06-10T23:03:00.000Z",
                "2026-06-10T23:03:00.000Z",
            )),
        )
        .expect("write metadata-drifted projection");

        assert!(
            can_replace_file(
                &store,
                &fixture.mount,
                &fixture.entity(HydrationState::Dirty),
                &fixture.page_path,
            )
            .expect("check replace")
        );
    }

    #[test]
    fn can_replace_rejects_frontmatter_only_edits() {
        let fixture = PullFixture::new();
        let store = fixture.store_with_shadow(
            HydrationState::Hydrated,
            fixture.document("Roadmap", "# Roadmap\n\nOriginal body.\n"),
        );
        write_atomic(
            &fixture.page_path,
            render_canonical_markdown(
                &fixture.document("Updated Roadmap", "# Roadmap\n\nOriginal body.\n"),
            ),
        )
        .expect("write edited projection");

        assert!(
            !can_replace_file(
                &store,
                &fixture.mount,
                &fixture.entity(HydrationState::Hydrated),
                &fixture.page_path,
            )
            .expect("check replace")
        );
    }

    #[test]
    fn database_directory_row_hydration_limit_can_be_disabled() {
        assert!(!super::should_hydrate_database_directory_rows(1, -1));
        assert!(super::should_hydrate_database_directory_rows(5, 5));
        assert!(!super::should_hydrate_database_directory_rows(6, 5));
    }

    #[test]
    fn mount_root_pull_repairs_missing_media_for_hydrated_child() {
        let fixture = PullFixture::new();
        let mut store = InMemoryStateStore::new();
        let root_id = RemoteId::new("root-page");
        let child_id = RemoteId::new("child-page");
        let mount = MountConfig::new(fixture.mount_id.clone(), "notion", fixture.root.clone())
            .with_remote_root_id(root_id.clone());
        store.save_mount(mount.clone()).expect("save mount");

        let media_path = PathBuf::from(".loc/media/Roadmap/Design Notes/image-child.png");
        let source = FakePullSource::new(
            vec![
                tree_entry(
                    &fixture.mount_id,
                    &root_id,
                    "Roadmap",
                    "Roadmap/page.md",
                    HydrationState::Stub,
                ),
                tree_entry(
                    &fixture.mount_id,
                    &child_id,
                    "Design Notes",
                    "Roadmap/Design Notes/page.md",
                    HydrationState::Stub,
                ),
            ],
            vec![
                hydrated_entity(&root_id, "Roadmap", "# Roadmap\n\nRoot body.\n", Vec::new()),
                hydrated_entity(
                    &child_id,
                    "Design Notes",
                    "![Sketch](../../.loc/media/Roadmap/Design Notes/image-child.png)\n",
                    vec![HydratedAsset {
                        path: media_path.clone(),
                        bytes: b"image bytes".to_vec(),
                        media: Some(HydratedAssetMedia {
                            block_id: "image-child".to_string(),
                            kind: "image".to_string(),
                            source_url: "https://example.com/image-child.png".to_string(),
                        }),
                    }],
                ),
            ],
        );

        super::pull_mount_root(&mut store, &source, &mount, fixture.root.clone(), None)
            .expect("initial root pull");
        let child_entity = store
            .get_entity(&fixture.mount_id, &child_id)
            .expect("load child")
            .expect("child entity");
        assert_eq!(
            super::hydrate_entity(&mut store, &source, &mount, child_entity, None)
                .expect("hydrate child"),
            super::HydrationOutcome::Hydrated
        );
        let absolute_media_path = fixture.root.join(&media_path);
        assert!(absolute_media_path.exists());
        std::fs::remove_file(&absolute_media_path).expect("remove media");

        let report =
            super::pull_mount_root(&mut store, &source, &mount, fixture.root.clone(), None)
                .expect("repair root pull");

        assert_eq!(report.hydrated, 2);
        assert_eq!(
            std::fs::read(&absolute_media_path).expect("repaired media"),
            b"image bytes"
        );
    }

    #[test]
    fn pull_virtual_database_row_writes_parent_schema_cache() {
        let fixture = PullFixture::new();
        let state_root = fixture.root.join("state");
        let mut store = InMemoryStateStore::new();
        let mount = MountConfig::new(fixture.mount_id.clone(), "notion", fixture.root.clone())
            .projection(ProjectionMode::LinuxFuse);
        store.save_mount(mount.clone()).expect("save mount");
        let database_id = RemoteId::new("database-1");
        let row_id = RemoteId::new("row-1");
        store
            .save_entity(EntityRecord::new(
                fixture.mount_id.clone(),
                database_id.clone(),
                EntityKind::Database,
                "Tasks",
                "Tasks",
            ))
            .expect("save database");
        store
            .save_entity(EntityRecord::new(
                fixture.mount_id.clone(),
                row_id.clone(),
                EntityKind::Page,
                "Fix login bug",
                "Tasks/Fix Login Bug/page.md",
            ))
            .expect("save row");
        let source = FakePullSource::new(
            Vec::new(),
            vec![hydrated_entity(
                &row_id,
                "Fix login bug",
                "Original row body.\n",
                Vec::new(),
            )],
        )
        .with_schema(&database_id, "title: Tasks\nproperties: {}\n");

        let report = super::pull_entity_path(
            &mut store,
            &source,
            &mount,
            PathBuf::from("Tasks/Fix Login Bug/page.md").as_path(),
            fixture.root.join("Tasks/Fix Login Bug/page.md"),
            Some(&state_root),
        )
        .expect("pull row");

        assert_eq!(report.hydrated, 1);
        let schema_path =
            crate::virtual_fs::virtual_fs_content_root(&state_root, &fixture.mount_id)
                .join("Tasks/_schema.yaml");
        assert_eq!(
            std::fs::read_to_string(schema_path).expect("schema cache"),
            "title: Tasks\nproperties: {}\n"
        );
    }

    #[test]
    fn pull_virtual_directory_prunes_clean_stale_child_subtree() {
        let fixture = PullFixture::new();
        let mut store = InMemoryStateStore::new();
        let mount = MountConfig::new(fixture.mount_id.clone(), "gmail", fixture.root.clone())
            .projection(ProjectionMode::LinuxFuse);
        store.save_mount(mount.clone()).expect("save mount");
        let inbox_id = RemoteId::new("gmail-folder:inbox");
        store
            .save_entity(EntityRecord::new(
                fixture.mount_id.clone(),
                inbox_id.clone(),
                EntityKind::Directory,
                "Inbox",
                "inbox",
            ))
            .expect("save inbox");
        store
            .save_entity(EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("gmail-thread:inbox:old-thread"),
                EntityKind::Page,
                "Old Thread",
                "inbox/old-thread/page.md",
            ))
            .expect("save old thread");
        store
            .save_entity(EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("gmail-thread-message:inbox:old-thread:old-msg"),
                EntityKind::Page,
                "Old Message",
                "inbox/old-thread/old-msg.md",
            ))
            .expect("save old thread child");
        store
            .save_entity(
                EntityRecord::new(
                    fixture.mount_id.clone(),
                    RemoteId::new("gmail-thread:inbox:dirty-thread"),
                    EntityKind::Page,
                    "Dirty Thread",
                    "inbox/dirty-thread/page.md",
                )
                .with_hydration(HydrationState::Dirty),
            )
            .expect("save dirty thread");
        let new_thread_id = RemoteId::new("gmail-thread:inbox:new-thread");
        let source = FakePullSource::new(Vec::new(), Vec::new()).with_children(
            &inbox_id,
            vec![tree_entry(
                &fixture.mount_id,
                &new_thread_id,
                "New Thread",
                "inbox/new-thread/page.md",
                HydrationState::Stub,
            )],
        );

        let report = super::pull_virtual_directory_path(
            &mut store,
            &source,
            &mount,
            Path::new("inbox"),
            fixture.root.join("inbox"),
            None,
        )
        .expect("pull virtual directory")
        .expect("virtual directory report");

        assert_eq!(report.enumerated, 1);
        assert!(
            store
                .get_entity(
                    &fixture.mount_id,
                    &RemoteId::new("gmail-thread:inbox:old-thread")
                )
                .expect("old thread lookup")
                .is_none()
        );
        assert!(
            store
                .get_entity(
                    &fixture.mount_id,
                    &RemoteId::new("gmail-thread-message:inbox:old-thread:old-msg")
                )
                .expect("old message lookup")
                .is_none()
        );
        assert!(
            store
                .get_entity(
                    &fixture.mount_id,
                    &RemoteId::new("gmail-thread:inbox:dirty-thread")
                )
                .expect("dirty thread lookup")
                .is_some()
        );
        assert!(
            store
                .get_entity(&fixture.mount_id, &new_thread_id)
                .expect("new thread lookup")
                .is_some()
        );
    }

    struct PullFixture {
        mount: MountConfig,
        mount_id: MountId,
        remote_id: RemoteId,
        page_path: PathBuf,
        root: PathBuf,
    }

    impl PullFixture {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("loc-pull-check-{}-{unique}", std::process::id()));
            std::fs::create_dir_all(&root).expect("fixture root");
            let mount_id = MountId::new("notion-main");
            let page_path = root.join("Roadmap.md");

            Self {
                mount: MountConfig::new(mount_id.clone(), "notion", root.clone()),
                mount_id,
                remote_id: RemoteId::new("page-1"),
                page_path,
                root,
            }
        }

        fn entity(&self, hydration: HydrationState) -> EntityRecord {
            EntityRecord::new(
                self.mount_id.clone(),
                self.remote_id.clone(),
                EntityKind::Page,
                "Roadmap",
                "Roadmap.md",
            )
            .with_hydration(hydration)
        }

        fn document(&self, title: &str, body: &str) -> CanonicalDocument {
            CanonicalDocument::new(
                format!(
                    "loc:\n  id: {}\n  type: page\ntitle: {title}\n",
                    self.remote_id.0
                ),
                body.to_string(),
            )
        }

        fn document_with_sync(
            &self,
            title: &str,
            body: &str,
            synced_at: &str,
            remote_edited_at: &str,
        ) -> CanonicalDocument {
            CanonicalDocument::new(
                format!(
                    "loc:\n  id: {}\n  type: page\n  synced_at: \"{synced_at}\"\n  remote_edited_at: \"{remote_edited_at}\"\ntitle: {title}\n",
                    self.remote_id.0
                ),
                body.to_string(),
            )
        }

        fn store_with_shadow(
            &self,
            hydration: HydrationState,
            document: CanonicalDocument,
        ) -> InMemoryStateStore {
            let mut store = InMemoryStateStore::new();
            let body_start_line = document.frontmatter.lines().count() + 3;
            let native_block_count = segment_markdown_body(&document.body, body_start_line)
                .into_iter()
                .filter(|block| !block.is_directive())
                .count();
            let block_ids =
                (0..native_block_count).map(|index| RemoteId::new(format!("block-{index}")));
            let shadow = ShadowDocument::from_synced_body(
                self.remote_id.clone(),
                document.body.clone(),
                body_start_line,
                block_ids,
            )
            .expect("shadow")
            .with_frontmatter(document.frontmatter.clone());

            store
                .save_shadow(&self.mount_id, shadow)
                .expect("save shadow");
            write_atomic(&self.page_path, render_canonical_markdown(&document))
                .expect("write projection");
            store
                .save_entity(self.entity(hydration))
                .expect("save entity");

            store
        }
    }

    #[derive(Clone)]
    struct FakePullSource {
        entries: Vec<locality_core::model::TreeEntry>,
        children: BTreeMap<RemoteId, Vec<locality_core::model::TreeEntry>>,
        rendered: BTreeMap<RemoteId, HydratedEntity>,
        schemas: BTreeMap<RemoteId, String>,
    }

    impl FakePullSource {
        fn new(
            entries: Vec<locality_core::model::TreeEntry>,
            rendered: Vec<HydratedEntity>,
        ) -> Self {
            Self {
                entries,
                children: BTreeMap::new(),
                rendered: rendered
                    .into_iter()
                    .map(|entity| (entity.shadow.entity_id.clone(), entity))
                    .collect(),
                schemas: BTreeMap::new(),
            }
        }

        fn with_schema(mut self, database_id: &RemoteId, schema: &str) -> Self {
            self.schemas.insert(database_id.clone(), schema.to_string());
            self
        }

        fn with_children(
            mut self,
            parent_id: &RemoteId,
            entries: Vec<locality_core::model::TreeEntry>,
        ) -> Self {
            self.children.insert(parent_id.clone(), entries);
            self
        }
    }

    impl Connector for FakePullSource {
        fn kind(&self) -> ConnectorKind {
            ConnectorKind("fake")
        }

        fn capabilities(&self) -> ConnectorCapabilities {
            ConnectorCapabilities::default()
        }

        fn supported_push_operations(&self) -> BTreeSet<PushOperationKind> {
            BTreeSet::new()
        }

        fn enumerate(
            &self,
            _request: EnumerateRequest,
        ) -> LocalityResult<Vec<locality_core::model::TreeEntry>> {
            Ok(self.entries.clone())
        }

        fn observe(&self, _request: ObserveRequest) -> LocalityResult<RemoteObservation> {
            Err(locality_core::LocalityError::NotImplemented("fake observe"))
        }

        fn list_children(
            &self,
            request: ListChildrenRequest,
        ) -> LocalityResult<ListChildrenResult> {
            let key = match request.container {
                ChildContainer::DirectoryChildren(remote_id)
                | ChildContainer::DatabaseRows(remote_id)
                | ChildContainer::PageChildren(remote_id) => remote_id,
                ChildContainer::Root => RemoteId::new("root"),
            };
            Ok(ListChildrenResult::complete(
                self.children.get(&key).cloned().unwrap_or_default(),
            ))
        }

        fn fetch(&self, _request: FetchRequest) -> LocalityResult<NativeEntity> {
            Err(locality_core::LocalityError::NotImplemented("fake fetch"))
        }

        fn render(&self, _entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
            Err(locality_core::LocalityError::NotImplemented("fake render"))
        }

        fn parse(&self, _document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
            Err(locality_core::LocalityError::NotImplemented("fake parse"))
        }

        fn check_concurrency(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
            Err(locality_core::LocalityError::NotImplemented(
                "fake check concurrency",
            ))
        }

        fn apply(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
            Err(locality_core::LocalityError::NotImplemented("fake apply"))
        }

        fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
            Err(locality_core::LocalityError::NotImplemented(
                "fake apply undo",
            ))
        }
    }

    impl HydrationSource for FakePullSource {
        fn fetch_render(&self, request: &HydrationRequest) -> LocalityResult<HydratedEntity> {
            assert_eq!(request.reason, HydrationReason::ExplicitPull);
            self.rendered
                .get(&request.remote_id)
                .cloned()
                .ok_or_else(|| {
                    locality_core::LocalityError::InvalidState("missing rendered entity".into())
                })
        }
    }

    impl SourcePushValidator for FakePullSource {}

    impl SourceAdapter for FakePullSource {
        fn database_schema_yaml(&self, database_id: &RemoteId) -> LocalityResult<Option<String>> {
            Ok(self.schemas.get(database_id).cloned())
        }
    }

    fn tree_entry(
        mount_id: &MountId,
        remote_id: &RemoteId,
        title: &str,
        path: &str,
        hydration: HydrationState,
    ) -> locality_core::model::TreeEntry {
        locality_core::model::TreeEntry {
            mount_id: mount_id.clone(),
            remote_id: remote_id.clone(),
            kind: EntityKind::Page,
            title: title.to_string(),
            path: PathBuf::from(path),
            hydration,
            content_hash: None,
            remote_edited_at: Some("2026-06-11T00:00:00.000Z".to_string()),
            stub_frontmatter: None,
        }
    }

    fn hydrated_entity(
        remote_id: &RemoteId,
        title: &str,
        body: &str,
        assets: Vec<HydratedAsset>,
    ) -> HydratedEntity {
        let document = CanonicalDocument::new(
            format!(
                "loc:\n  id: {}\n  type: page\ntitle: {title}\n",
                remote_id.0
            ),
            body.to_string(),
        );
        let body_start_line = document.frontmatter.lines().count() + 3;
        let native_block_count = segment_markdown_body(&document.body, body_start_line)
            .into_iter()
            .filter(|block| !block.is_directive())
            .count();
        let block_ids =
            (0..native_block_count).map(|index| RemoteId::new(format!("{}-{index}", remote_id.0)));
        let shadow = ShadowDocument::from_synced_body(
            remote_id.clone(),
            document.body.clone(),
            body_start_line,
            block_ids,
        )
        .expect("shadow")
        .with_frontmatter(document.frontmatter.clone());

        HydratedEntity {
            document,
            shadow,
            remote_edited_at: Some("2026-06-11T00:00:00.000Z".to_string()),
            assets,
        }
    }

    impl Drop for PullFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}
