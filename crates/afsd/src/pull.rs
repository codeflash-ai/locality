//! `afs pull` orchestration.
//!
//! Pull is the read-side bridge between connector output, store state, and the
//! real file tree. Mount-root pulls enumerate the remote projection and write
//! stubs; page-file pulls hydrate one entity and persist its shadow snapshot.

use std::path::{Component, Path, PathBuf};

use afs_connector::{ChildContainer, EnumerateRequest, ListChildrenRequest};
use afs_core::canonical::{parse_canonical_markdown, render_canonical_markdown};
use afs_core::conflict::{
    has_unresolved_conflict_markers, render_inline_conflict_markdown_with_base,
};
use afs_core::freshness::RemoteVersion;
use afs_core::hydration::{HydrationReason, HydrationRequest};
use afs_core::model::{CanonicalDocument, EntityKind, HydrationState, TreeEntry};
use afs_core::path_projection::{is_page_document_path, page_container_path};
use afs_core::shadow::ShadowDocument;
use afs_store::{
    EntityRecord, EntityRepository, MountConfig, MountRepository, ProjectionMode, ShadowRepository,
    StoreError,
};
use serde::{Deserialize, Serialize};

use crate::file_provider::{self, ProjectionRefreshBase};
use crate::hydration::{HydratedAsset, HydratedEntity};
use crate::media::{
    document_with_absolute_media_hrefs, render_document_with_absolute_media_hrefs,
    update_hydrated_media_manifest,
};
use crate::shadow_match::{parsed_matches_shadow, shadows_match};
use crate::source::SourceAdapter;
use crate::virtual_fs::{virtual_fs_content_path, virtual_fs_content_root};

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
        + afs_store::VirtualMutationRepository
        + afs_store::FreshnessStateRepository
        + afs_store::RemoteObservationRepository,
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
        + afs_store::VirtualMutationRepository
        + afs_store::FreshnessStateRepository
        + afs_store::RemoteObservationRepository,
    Source: SourceAdapter + Clone,
{
    let target_path = absolute_path(target_path.as_ref())?;
    let mounts = store.load_mounts().map_err(PullError::Store)?;
    let (mount, matched) = find_mount_for_path(&mounts, &target_path)
        .ok_or_else(|| PullError::MountNotFound(target_path.clone()))?;
    let mount = mount.clone();
    let relative_path = matched.relative_path;
    let source = source.scoped_to_mount(&mount);
    let refresh_bases =
        prepare_macos_file_provider_projection_pull(store, state_root, &mount, &target_path)?;

    let report = if should_pull_mount_root(&mount, &relative_path, &target_path) {
        pull_mount_root(store, &source, &mount, target_path.clone(), state_root)
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

    refresh_macos_file_provider_projection_after_pull(
        store,
        state_root,
        &target_path,
        &report,
        &refresh_bases,
    )?;
    Ok(report)
}

fn prepare_macos_file_provider_projection_pull<S>(
    store: &mut S,
    state_root: Option<&Path>,
    mount: &MountConfig,
    target_path: &Path,
) -> Result<Vec<ProjectionRefreshBase>, PullError>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + afs_store::VirtualMutationRepository
        + afs_store::FreshnessStateRepository,
{
    let Some(state_root) = state_root else {
        return Ok(Vec::new());
    };
    if mount.projection != afs_store::ProjectionMode::MacosFileProvider {
        return Ok(Vec::new());
    }

    let refresh_bases =
        file_provider::macos_file_provider_projection_refresh_bases(store, Some(target_path))
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

fn refresh_macos_file_provider_projection_after_pull<S>(
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

    file_provider::refresh_macos_file_provider_projection(
        store,
        state_root,
        Some(target_path),
        refresh_bases,
    )
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
        + afs_store::FreshnessStateRepository
        + afs_store::RemoteObservationRepository,
    Source: SourceAdapter,
{
    let entries = source
        .enumerate(EnumerateRequest {
            mount_id: mount.mount_id.clone(),
            cursor: None,
        })
        .map_err(PullError::Connector)?;
    let mut stubbed = 0;

    for entry in &entries {
        let existing = store
            .get_entity(&entry.mount_id, &entry.remote_id)
            .map_err(PullError::Store)?;
        let record = merged_entity_record(entry, existing.as_ref());
        store.save_entity(record).map_err(PullError::Store)?;
        rename_projection_if_needed(mount, existing.as_ref(), entry)?;
        if write_stub_if_needed(source, mount, entry, state_root)? {
            stubbed += 1;
        }
    }

    let mut hydrated = 0;
    let mut skipped_dirty = 0;
    let mut conflicts = Vec::new();
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
        match hydrate_entity(store, source, mount, root_entity, state_root)? {
            HydrationOutcome::Hydrated => hydrated += 1,
            HydrationOutcome::SkippedDirty => skipped_dirty += 1,
            HydrationOutcome::Conflicted(conflict) => {
                skipped_dirty += 1;
                conflicts.push(conflict);
            }
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
    S: EntityRepository,
    Source: SourceAdapter,
{
    if !mount.projection.uses_virtual_filesystem() {
        return Ok(None);
    }

    let Some(target) = virtual_directory_target(store, mount, relative_path)? else {
        return Ok(None);
    };

    let mut enumerated = 0;
    if let Some(container) = target.container {
        let result = source
            .list_children(ListChildrenRequest {
                mount_id: mount.mount_id.clone(),
                container,
                parent_path: target.parent_path.clone(),
            })
            .map_err(PullError::Connector)?;
        enumerated = result.entries.len();
        for entry in result.entries {
            let existing = store
                .get_entity(&entry.mount_id, &entry.remote_id)
                .map_err(PullError::Store)?;
            let record = virtual_child_entity_record(entry, existing.as_ref());
            store.save_entity(record).map_err(PullError::Store)?;
        }
    }

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

    Ok(Some(PullReport {
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
    }))
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
    schema_database_id: Option<afs_core::model::RemoteId>,
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
                container: None,
                schema_database_id: None,
            }),
            EntityKind::Page | EntityKind::Asset | EntityKind::Unknown(_) => None,
        });
    }

    let entities = store
        .list_entities(&mount.mount_id)
        .map_err(PullError::Store)?;
    Ok(entities.into_iter().find_map(|entity| {
        if entity.kind == EntityKind::Page && page_container_path(&entity.path) == relative_path {
            Some(VirtualDirectoryTarget {
                parent_path: relative_path.to_path_buf(),
                container: Some(ChildContainer::PageChildren(entity.remote_id)),
                schema_database_id: None,
            })
        } else {
            None
        }
    }))
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
        + afs_store::FreshnessStateRepository
        + afs_store::RemoteObservationRepository,
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
        HydrationOutcome::Hydrated => (1, 0, Vec::new()),
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

fn merged_entity_record(entry: &TreeEntry, existing: Option<&EntityRecord>) -> EntityRecord {
    let mut record = EntityRecord::from(entry.clone());

    if let Some(existing) = existing {
        record.hydration = existing.hydration.clone();
        record.content_hash = existing.content_hash.clone();
        if remote_precondition_belongs_to_shadow(existing) {
            record.remote_edited_at = existing.remote_edited_at.clone();
        }
    }

    record
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
        + afs_store::FreshnessStateRepository
        + afs_store::RemoteObservationRepository,
    Source: SourceAdapter,
{
    let path = projection_content_path(state_root, mount, &entity.path)?;
    let can_replace = can_replace_file(store, mount, &entity, &path)?;
    let rendered = source
        .fetch_render(&HydrationRequest::new(
            mount.mount_id.clone(),
            entity.remote_id.clone(),
            entity.path.clone(),
            HydrationState::Hydrated,
            HydrationReason::ExplicitPull,
        ))
        .map_err(PullError::Connector)?;
    let media_root = projection_output_root(state_root, mount)?;
    write_assets(&media_root, &rendered.assets)?;

    if can_replace {
        accept_remote_projection(store, mount, entity, &path, &media_root, rendered)?;
        return Ok(HydrationOutcome::Hydrated);
    }

    if file_has_unresolved_conflict_markers(&path)? {
        let conflict = pull_conflict(mount, &entity);
        store
            .save_entity(mark_conflicted_if_allowed(entity))
            .map_err(PullError::Store)?;
        return Ok(HydrationOutcome::Conflicted(conflict));
    } else if !remote_matches_shadow(store, mount, &entity, &rendered.shadow)? {
        let conflict = pull_conflict(mount, &entity);
        materialize_conflict(store, mount, entity, &path, &media_root, rendered)?;
        return Ok(HydrationOutcome::Conflicted(conflict));
    } else {
        store
            .save_entity(mark_dirty_if_allowed(entity))
            .map_err(PullError::Store)?;
    }

    Ok(HydrationOutcome::SkippedDirty)
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

fn write_assets(root: &Path, assets: &[HydratedAsset]) -> Result<(), PullError> {
    for asset in assets {
        let path = mount_relative_path(root, &asset.path)?;
        write_binary_atomic(&path, &asset.bytes)?;
    }
    update_hydrated_media_manifest(root, assets).map_err(PullError::Connector)?;
    Ok(())
}

fn should_pull_mount_root(mount: &MountConfig, relative_path: &Path, target_path: &Path) -> bool {
    if relative_path.as_os_str().is_empty() {
        return true;
    }
    if mount.projection.uses_virtual_filesystem() {
        return false;
    }

    target_path.is_dir()
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
        + afs_store::FreshnessStateRepository
        + afs_store::RemoteObservationRepository,
{
    let markdown =
        render_document_with_absolute_media_hrefs(&rendered.document, &entity.path, output_root);
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
    S: afs_store::FreshnessStateRepository + afs_store::RemoteObservationRepository,
{
    let observed_at = crate::freshness::freshness_timestamp();
    let mut observation = afs_store::RemoteObservationRecord::new(
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

fn materialize_conflict<S>(
    store: &mut S,
    mount: &MountConfig,
    entity: EntityRecord,
    path: &Path,
    output_root: &Path,
    rendered: HydratedEntity,
) -> Result<(), PullError>
where
    S: EntityRepository + ShadowRepository,
{
    let local_contents = std::fs::read_to_string(path).map_err(|error| PullError::ReadFile {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let base_shadow = match store.load_shadow(&mount.mount_id, &entity.remote_id) {
        Ok(shadow) => Some(shadow),
        Err(StoreError::ShadowMissing { .. }) => None,
        Err(error) => return Err(PullError::Store(error)),
    };
    let remote_document =
        document_with_absolute_media_hrefs(&rendered.document, &entity.path, output_root);
    let conflict_markdown = render_inline_conflict_markdown_with_base(
        &local_contents,
        base_shadow
            .as_ref()
            .map(|shadow| shadow.rendered_body.as_str()),
        &remote_document,
    );
    write_atomic(path, conflict_markdown)?;
    store
        .save_shadow(&mount.mount_id, rendered.shadow.clone())
        .map_err(PullError::Store)?;
    store
        .save_entity(conflicted_record(
            entity,
            &rendered.shadow,
            rendered.remote_edited_at,
        ))
        .map_err(PullError::Store)?;

    Ok(())
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
        return mount
            .root
            .join(crate::virtual_fs::source_root_directory_name(
                &mount.connector,
            ))
            .join(relative_path);
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

fn write_binary_atomic(path: &Path, contents: &[u8]) -> Result<(), PullError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| PullError::WriteFile {
            path: parent.to_path_buf(),
            message: error.to_string(),
        })?;
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("afs-asset");
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

fn mount_relative_path(root: &Path, path: &Path) -> Result<PathBuf, PullError> {
    if path.components().any(|component| {
        matches!(
            component,
            Component::Prefix(_) | Component::RootDir | Component::ParentDir
        )
    }) {
        return Err(PullError::WriteFile {
            path: path.to_path_buf(),
            message: "hydrated asset path is not mount-relative".to_string(),
        });
    }

    Ok(root.join(path))
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
    SkippedDirty,
    Conflicted(PullConflict),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PullError {
    Connector(afs_core::AfsError),
    CurrentDir(String),
    MountNotFound(PathBuf),
    Projection(afs_core::AfsError),
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
                format!("no AgentFS mount contains `{}`", path.display())
            }
            Self::Projection(error) => {
                format!("macOS File Provider projection refresh failed: {error}")
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use afs_core::canonical::render_canonical_markdown;
    use afs_core::model::{CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId};
    use afs_core::shadow::{ShadowDocument, segment_markdown_body};
    use afs_store::{EntityRecord, EntityRepository, InMemoryStateStore, ShadowRepository};

    use super::{can_replace_file, write_atomic};
    use afs_store::MountConfig;

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
                .join(format!("afs-pull-check-{}-{unique}", std::process::id()));
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
                    "afs:\n  id: {}\n  type: page\ntitle: {title}\n",
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
                    "afs:\n  id: {}\n  type: page\n  synced_at: \"{synced_at}\"\n  remote_edited_at: \"{remote_edited_at}\"\ntitle: {title}\n",
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

    impl Drop for PullFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}
