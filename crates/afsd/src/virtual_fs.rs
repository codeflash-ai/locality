use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use afs_connector::{ChildContainer, Connector, ListChildrenRequest};
use afs_core::canonical::parse_canonical_markdown;
use afs_core::conflict::has_unresolved_conflict_markers;
use afs_core::hydration::{HydrationReason, HydrationRequest};
use afs_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use afs_core::{AfsError, AfsResult};
use afs_store::{
    EntityRecord, EntityRepository, MountConfig, MountRepository, ShadowRepository, StoreError,
    VirtualMutationKind, VirtualMutationRecord, VirtualMutationRepository,
};
use serde::{Deserialize, Serialize};

use crate::hydration::{HydrationExecutor, HydrationOutcome, HydrationSource};

pub const ROOT_CONTAINER_IDENTIFIER: &str = "root";
const CHILDREN_PREFIX: &str = "children:";
const PATH_PREFIX: &str = "path:";
const LOCAL_PREFIX: &str = "local:";
const SCHEMA_PREFIX: &str = "schema:";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualFsItemReport {
    pub mount_id: String,
    pub item: VirtualFsItem,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualFsChildrenReport {
    pub mount_id: String,
    pub container_identifier: String,
    pub children: Vec<VirtualFsItem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualFsMaterializeReport {
    pub mount_id: String,
    pub identifier: String,
    pub remote_id: String,
    pub path: String,
    pub outcome: VirtualFsMaterializeOutcome,
    pub hydration: HydrationState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualFsWriteReport {
    pub mount_id: String,
    pub identifier: String,
    pub remote_id: String,
    pub path: String,
    pub bytes_written: usize,
    pub hydration: HydrationState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualFsMutationReport {
    pub mount_id: String,
    pub identifier: String,
    pub path: String,
    pub item: VirtualFsItem,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VirtualFsMaterializeOutcome {
    AlreadyMaterialized,
    Hydrated,
    SkippedDirty,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualFsItem {
    pub identifier: String,
    pub parent_identifier: Option<String>,
    pub filename: String,
    pub kind: VirtualFsItemKind,
    pub entity_kind: Option<EntityKind>,
    pub remote_id: Option<String>,
    pub path: String,
    pub hydration: Option<HydrationState>,
    pub content_type: String,
    pub remote_edited_at: Option<String>,
    pub materialized_path: Option<String>,
    pub byte_size: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VirtualFsItemKind {
    File,
    Folder,
}

pub fn virtual_fs_item<S>(
    store: &S,
    mount_id: &MountId,
    identifier: &str,
) -> AfsResult<VirtualFsItemReport>
where
    S: MountRepository + EntityRepository + VirtualMutationRepository,
{
    let mount = require_virtual_mount(store, mount_id)?;
    let entities = store.list_entities(mount_id).map_err(AfsError::from)?;
    let mutations = store
        .list_virtual_mutations(mount_id)
        .map_err(AfsError::from)?;
    let index = ProviderIndex::new(&entities);
    let item = resolve_item(&mount, &entities, &mutations, &index, identifier)?;

    Ok(VirtualFsItemReport {
        mount_id: mount_id.0.clone(),
        item,
    })
}

pub fn virtual_fs_item_with_content_root<S>(
    store: &S,
    content_root: &Path,
    mount_id: &MountId,
    identifier: &str,
) -> AfsResult<VirtualFsItemReport>
where
    S: MountRepository + EntityRepository + VirtualMutationRepository,
{
    let mut report = virtual_fs_item(store, mount_id, identifier)?;
    rewrite_item_materialized_path(content_root, &mut report.item)?;
    Ok(report)
}

pub fn virtual_fs_children<S>(
    store: &S,
    mount_id: &MountId,
    container_identifier: &str,
) -> AfsResult<VirtualFsChildrenReport>
where
    S: MountRepository + EntityRepository + VirtualMutationRepository,
{
    let mount = require_virtual_mount(store, mount_id)?;
    let entities = store.list_entities(mount_id).map_err(AfsError::from)?;
    let mutations = store
        .list_virtual_mutations(mount_id)
        .map_err(AfsError::from)?;
    let index = ProviderIndex::new(&entities);
    let container_path = container_path(&entities, container_identifier)?;
    let mut children = Vec::new();
    let deleted_remote_ids = pending_deleted_remote_ids(&mutations);

    for entity in &entities {
        if deleted_remote_ids.contains(&entity.remote_id) {
            continue;
        }
        if parent_path(&entity.path) == container_path {
            children.push(entity_item(&mount, entity, &index));
            if entity.kind == EntityKind::Page {
                children.push(page_child_dir_item(
                    &mount,
                    &entity.path.with_extension(""),
                    &entity.remote_id,
                    &index,
                ));
            }
        }
    }
    for mutation in &mutations {
        if mutation.mutation_kind == VirtualMutationKind::Create
            && parent_path(&mutation.projected_path) == container_path
        {
            children.push(pending_item(&mount, mutation, &index));
        }
    }

    children.sort_by(|left, right| {
        left.filename
            .to_lowercase()
            .cmp(&right.filename.to_lowercase())
            .then_with(|| left.identifier.cmp(&right.identifier))
    });

    Ok(VirtualFsChildrenReport {
        mount_id: mount_id.0.clone(),
        container_identifier: container_identifier.to_string(),
        children,
    })
}

pub fn virtual_fs_children_with_content_root<S>(
    store: &S,
    content_root: &Path,
    mount_id: &MountId,
    container_identifier: &str,
) -> AfsResult<VirtualFsChildrenReport>
where
    S: MountRepository + EntityRepository + VirtualMutationRepository,
{
    let mut report = virtual_fs_children(store, mount_id, container_identifier)?;
    let mount = require_mount(store, mount_id)?;
    let entities = store.list_entities(mount_id).map_err(AfsError::from)?;
    if let Some(schema) =
        schema_item_for_container(&mount, &entities, content_root, container_identifier)?
        && !report
            .children
            .iter()
            .any(|child| child.identifier == schema.identifier)
    {
        report.children.push(schema);
    }
    for child in &mut report.children {
        rewrite_item_materialized_path(content_root, child)?;
    }
    report.children.sort_by(|left, right| {
        left.filename
            .to_lowercase()
            .cmp(&right.filename.to_lowercase())
            .then_with(|| left.identifier.cmp(&right.identifier))
    });
    Ok(report)
}

pub fn refresh_virtual_fs_children<S, C>(
    store: &mut S,
    connector: &C,
    mount_id: &MountId,
    container_identifier: &str,
) -> AfsResult<usize>
where
    S: MountRepository + EntityRepository,
    C: Connector + ?Sized,
{
    let mount = require_virtual_mount(store, mount_id)?;
    let entities = store.list_entities(mount_id).map_err(AfsError::from)?;
    let parent_path = container_path(&entities, container_identifier)?;
    if has_known_entity_child(&entities, &parent_path) {
        return Ok(0);
    }

    let Some(container) = child_container_for_identifier(&entities, container_identifier)? else {
        return Ok(0);
    };

    let result = connector.list_children(ListChildrenRequest {
        mount_id: mount.mount_id.clone(),
        container,
        parent_path,
    })?;

    let mut saved = 0;
    for entry in result.entries {
        let existing = store
            .get_entity(&entry.mount_id, &entry.remote_id)
            .map_err(AfsError::from)?;
        let record = refreshed_entity_record(entry, existing.as_ref());
        store.save_entity(record).map_err(AfsError::from)?;
        saved += 1;
    }

    Ok(saved)
}

pub fn virtual_fs_children_refresh_needed<S>(
    store: &S,
    mount_id: &MountId,
    container_identifier: &str,
) -> AfsResult<bool>
where
    S: MountRepository + EntityRepository,
{
    require_virtual_mount(store, mount_id)?;
    let entities = store.list_entities(mount_id).map_err(AfsError::from)?;
    let parent_path = container_path(&entities, container_identifier)?;
    if has_known_entity_child(&entities, &parent_path) {
        return Ok(false);
    }

    child_container_for_identifier(&entities, container_identifier)
        .map(|container| container.is_some())
}

pub fn materialize_virtual_fs_item<S, Source>(
    store: &mut S,
    source: &Source,
    mount_id: &MountId,
    identifier: &str,
) -> AfsResult<VirtualFsMaterializeReport>
where
    S: MountRepository + EntityRepository + ShadowRepository,
    Source: HydrationSource + ?Sized,
{
    let mount = require_virtual_mount(store, mount_id)?;
    let remote_id = RemoteId::new(entity_identifier(identifier)?);
    let entity = require_entity(store, mount_id, &remote_id)?;
    if entity.kind != EntityKind::Page {
        return Err(AfsError::Unsupported(
            "only page files can be materialized by the virtual filesystem",
        ));
    }

    let path = mount.root.join(&entity.path);
    let outcome = if is_materialized_hydration(&entity.hydration) && path.exists() {
        VirtualFsMaterializeOutcome::AlreadyMaterialized
    } else {
        let request = HydrationRequest::new(
            mount.mount_id.clone(),
            entity.remote_id.clone(),
            path.clone(),
            HydrationState::Hydrated,
            HydrationReason::FileOpen,
        );
        let mut executor = HydrationExecutor::new(store, source);
        match executor.hydrate_request(request)? {
            HydrationOutcome::Hydrated => VirtualFsMaterializeOutcome::Hydrated,
            HydrationOutcome::SkippedDirty => VirtualFsMaterializeOutcome::SkippedDirty,
        }
    };

    let hydrated = require_entity(store, mount_id, &remote_id)?;
    if !path.exists() {
        return Err(AfsError::InvalidState(format!(
            "virtual filesystem materialization finished without local contents for `{}`",
            path.display()
        )));
    }

    Ok(VirtualFsMaterializeReport {
        mount_id: mount_id.0.clone(),
        identifier: identifier.to_string(),
        remote_id: remote_id.0,
        path: path.display().to_string(),
        outcome,
        hydration: hydrated.hydration,
    })
}

pub fn materialize_virtual_fs_item_with_content_root<S, Source>(
    store: &mut S,
    source: &Source,
    content_root: &Path,
    mount_id: &MountId,
    identifier: &str,
) -> AfsResult<VirtualFsMaterializeReport>
where
    S: MountRepository + EntityRepository + ShadowRepository + VirtualMutationRepository,
    Source: HydrationSource + ?Sized,
{
    let mount = require_virtual_mount(store, mount_id)?;
    if let Some(mutation) = local_mutation(store, mount_id, identifier)? {
        if mutation.mutation_kind != VirtualMutationKind::Create {
            return Err(AfsError::Unsupported(
                "only pending-created local files can be materialized",
            ));
        }
        let path = content_path_for_relative(content_root, &mutation.projected_path)?;
        if !path.exists() {
            write_binary_atomic(&path, b"")?;
        }
        return Ok(VirtualFsMaterializeReport {
            mount_id: mount_id.0.clone(),
            identifier: identifier.to_string(),
            remote_id: mutation.local_id,
            path: path.display().to_string(),
            outcome: VirtualFsMaterializeOutcome::AlreadyMaterialized,
            hydration: HydrationState::Dirty,
        });
    }
    if let Some(remote_id) = identifier.strip_prefix(SCHEMA_PREFIX) {
        let entity = require_entity(store, mount_id, &RemoteId::new(remote_id))?;
        if entity.kind != EntityKind::Database {
            return Err(AfsError::Unsupported(
                "only database schemas can be materialized by the virtual filesystem",
            ));
        }
        let path = content_path_for_relative(content_root, &entity.path.join("_schema.yaml"))?;
        if !path.exists() {
            return Err(AfsError::InvalidState(format!(
                "database schema cache is missing for `{}`",
                entity.path.display()
            )));
        }
        return Ok(VirtualFsMaterializeReport {
            mount_id: mount_id.0.clone(),
            identifier: identifier.to_string(),
            remote_id: remote_id.to_string(),
            path: path.display().to_string(),
            outcome: VirtualFsMaterializeOutcome::AlreadyMaterialized,
            hydration: HydrationState::Hydrated,
        });
    }
    let remote_id = RemoteId::new(entity_identifier(identifier)?);
    let entity = require_entity(store, mount_id, &remote_id)?;
    if entity.kind != EntityKind::Page {
        let path = content_path_for_relative(content_root, &entity.path)?;
        if path.exists() {
            return Ok(VirtualFsMaterializeReport {
                mount_id: mount_id.0.clone(),
                identifier: identifier.to_string(),
                remote_id: remote_id.0,
                path: path.display().to_string(),
                outcome: VirtualFsMaterializeOutcome::AlreadyMaterialized,
                hydration: entity.hydration,
            });
        }
        return Err(AfsError::Unsupported(
            "only page files can be materialized by the virtual filesystem",
        ));
    }

    let path = content_path_for_relative(content_root, &entity.path)?;
    let outcome = if is_materialized_hydration(&entity.hydration) && path.exists() {
        VirtualFsMaterializeOutcome::AlreadyMaterialized
    } else {
        let request = HydrationRequest::new(
            mount.mount_id.clone(),
            entity.remote_id.clone(),
            path.clone(),
            HydrationState::Hydrated,
            HydrationReason::FileOpen,
        );
        let mut executor =
            HydrationExecutor::new_with_output_root(store, source, content_root.to_path_buf());
        match executor.hydrate_request(request)? {
            HydrationOutcome::Hydrated => VirtualFsMaterializeOutcome::Hydrated,
            HydrationOutcome::SkippedDirty => VirtualFsMaterializeOutcome::SkippedDirty,
        }
    };

    let hydrated = require_entity(store, mount_id, &remote_id)?;
    if !path.exists() {
        return Err(AfsError::InvalidState(format!(
            "virtual filesystem materialization finished without local contents for `{}`",
            path.display()
        )));
    }

    Ok(VirtualFsMaterializeReport {
        mount_id: mount_id.0.clone(),
        identifier: identifier.to_string(),
        remote_id: remote_id.0,
        path: path.display().to_string(),
        outcome,
        hydration: hydrated.hydration,
    })
}

pub fn commit_virtual_fs_write<S>(
    store: &mut S,
    content_root: &Path,
    mount_id: &MountId,
    identifier: &str,
    contents: &[u8],
) -> AfsResult<VirtualFsWriteReport>
where
    S: MountRepository + EntityRepository + ShadowRepository + VirtualMutationRepository,
{
    let mount = require_virtual_mount(store, mount_id)?;
    if mount.read_only {
        return Err(AfsError::Unsupported(
            "read-only mounts do not accept virtual filesystem writes",
        ));
    }
    if let Some(mut mutation) = local_mutation(store, mount_id, identifier)? {
        if mutation.mutation_kind != VirtualMutationKind::Create {
            return Err(AfsError::Unsupported(
                "only pending-created local files can be written by local virtual identifier",
            ));
        }
        let path = content_path_for_relative(content_root, &mutation.projected_path)?;
        write_binary_atomic(&path, contents)?;
        mutation.content_path = Some(path.clone());
        mutation.updated_at = now_string();
        store
            .save_virtual_mutation(mutation)
            .map_err(AfsError::from)?;
        return Ok(VirtualFsWriteReport {
            mount_id: mount_id.0.clone(),
            identifier: identifier.to_string(),
            remote_id: identifier.to_string(),
            path: path.display().to_string(),
            bytes_written: contents.len(),
            hydration: HydrationState::Dirty,
        });
    }
    if identifier.starts_with(SCHEMA_PREFIX) {
        return Err(AfsError::Unsupported(
            "database schema files are read-only in virtual filesystem mounts",
        ));
    }
    let remote_id = RemoteId::new(entity_identifier(identifier)?);
    let mut entity = require_entity(store, mount_id, &remote_id)?;
    if entity.kind != EntityKind::Page {
        return Err(AfsError::Unsupported(
            "only page files can be written by the virtual filesystem",
        ));
    }
    let path = content_path_for_relative(content_root, &entity.path)?;
    write_binary_atomic(&path, contents)?;

    let matches_shadow = std::str::from_utf8(contents)
        .ok()
        .and_then(|contents| parse_canonical_markdown(contents).ok())
        .and_then(|parsed| {
            store
                .load_shadow(&mount.mount_id, &entity.remote_id)
                .ok()
                .map(|shadow| {
                    parsed.document.frontmatter == shadow.frontmatter
                        && parsed.document.body == shadow.rendered_body
                })
        })
        .unwrap_or(false);

    let contains_conflict_markers = std::str::from_utf8(contents)
        .ok()
        .is_some_and(has_unresolved_conflict_markers);

    if contains_conflict_markers {
        entity.hydration = HydrationState::Conflicted;
    } else if matches_shadow {
        entity.hydration = HydrationState::Hydrated;
    } else {
        entity.hydration = HydrationState::Dirty;
    }
    store.save_entity(entity.clone()).map_err(AfsError::from)?;

    Ok(VirtualFsWriteReport {
        mount_id: mount_id.0.clone(),
        identifier: identifier.to_string(),
        remote_id: remote_id.0,
        path: path.display().to_string(),
        bytes_written: contents.len(),
        hydration: entity.hydration,
    })
}

pub fn create_virtual_fs_file<S>(
    store: &mut S,
    content_root: &Path,
    mount_id: &MountId,
    parent_identifier: &str,
    filename: &str,
) -> AfsResult<VirtualFsMutationReport>
where
    S: MountRepository + EntityRepository + VirtualMutationRepository,
{
    let mount = require_virtual_mount(store, mount_id)?;
    if mount.read_only {
        return Err(AfsError::Unsupported(
            "read-only mounts do not accept virtual filesystem creates",
        ));
    }
    if !filename.ends_with(".md") {
        return Err(AfsError::Unsupported(
            "virtual filesystem creates currently support only Markdown page files",
        ));
    }
    let entities = store.list_entities(mount_id).map_err(AfsError::from)?;
    let parent_path = container_path(&entities, parent_identifier)?;
    let parent_remote_id = create_parent_remote_id(&entities, parent_identifier)?;
    let projected_path = parent_path.join(filename);
    ensure_virtual_path_available(store, mount_id, &projected_path)?;
    let path = content_path_for_relative(content_root, &projected_path)?;
    write_binary_atomic(&path, b"")?;

    let now = now_string();
    let mutation = VirtualMutationRecord {
        mount_id: mount_id.clone(),
        local_id: new_local_id(),
        mutation_kind: VirtualMutationKind::Create,
        target_remote_id: None,
        parent_remote_id: Some(parent_remote_id),
        original_path: None,
        projected_path,
        title: title_from_filename(filename),
        content_path: Some(path),
        created_at: now.clone(),
        updated_at: now,
    };
    store
        .save_virtual_mutation(mutation.clone())
        .map_err(AfsError::from)?;
    let index = ProviderIndex::new(&entities);
    let item = pending_item(&mount, &mutation, &index);
    Ok(VirtualFsMutationReport {
        mount_id: mount_id.0.clone(),
        identifier: mutation.local_id,
        path: item.path.clone(),
        item,
    })
}

pub fn rename_virtual_fs_item<S>(
    store: &mut S,
    content_root: &Path,
    mount_id: &MountId,
    identifier: &str,
    new_parent_identifier: &str,
    new_filename: &str,
) -> AfsResult<VirtualFsMutationReport>
where
    S: MountRepository + EntityRepository + VirtualMutationRepository,
{
    let mount = require_virtual_mount(store, mount_id)?;
    if mount.read_only {
        return Err(AfsError::Unsupported(
            "read-only mounts do not accept virtual filesystem renames",
        ));
    }
    if !new_filename.ends_with(".md") {
        return Err(AfsError::Unsupported(
            "virtual filesystem renames currently support only Markdown page files",
        ));
    }
    let entities = store.list_entities(mount_id).map_err(AfsError::from)?;
    let new_parent_path = container_path(&entities, new_parent_identifier)?;
    let new_path = new_parent_path.join(new_filename);
    ensure_virtual_path_available_for_rename(store, mount_id, identifier, &new_path)?;

    if let Some(mut mutation) = local_mutation(store, mount_id, identifier)? {
        let old_parent = parent_path(&mutation.projected_path).to_path_buf();
        if old_parent != new_parent_path {
            return Err(AfsError::Unsupported(
                "moving virtual filesystem files across parents is not supported yet",
            ));
        }
        let old_path = content_path_for_relative(content_root, &mutation.projected_path)?;
        let new_content_path = content_path_for_relative(content_root, &new_path)?;
        rename_cached_file_if_present(&old_path, &new_content_path)?;
        mutation.projected_path = new_path;
        mutation.title = title_from_filename(new_filename);
        mutation.content_path = Some(new_content_path);
        mutation.updated_at = now_string();
        store
            .save_virtual_mutation(mutation.clone())
            .map_err(AfsError::from)?;
        let index = ProviderIndex::new(&entities);
        let item = pending_item(&mount, &mutation, &index);
        return Ok(VirtualFsMutationReport {
            mount_id: mount_id.0.clone(),
            identifier: mutation.local_id,
            path: item.path.clone(),
            item,
        });
    }

    let remote_id = RemoteId::new(entity_identifier(identifier)?);
    let mut entity = require_entity(store, mount_id, &remote_id)?;
    if entity.kind != EntityKind::Page {
        return Err(AfsError::Unsupported(
            "only page files can be renamed by the virtual filesystem",
        ));
    }
    let old_parent = parent_path(&entity.path).to_path_buf();
    if old_parent != new_parent_path {
        return Err(AfsError::Unsupported(
            "moving virtual filesystem files across parents is not supported yet",
        ));
    }
    let old_path = content_path_for_relative(content_root, &entity.path)?;
    let new_content_path = content_path_for_relative(content_root, &new_path)?;
    rename_cached_file_if_present(&old_path, &new_content_path)?;
    let original_path = entity.path.clone();
    entity.path = new_path.clone();
    entity.title = title_from_filename(new_filename);
    if entity.hydration.can_transition_to(&HydrationState::Dirty) {
        entity.hydration = HydrationState::Dirty;
    }
    store.save_entity(entity.clone()).map_err(AfsError::from)?;
    let now = now_string();
    let mutation = VirtualMutationRecord {
        mount_id: mount_id.clone(),
        local_id: format!("rename:{}", remote_id.0),
        mutation_kind: VirtualMutationKind::Rename,
        target_remote_id: Some(remote_id.clone()),
        parent_remote_id: None,
        original_path: Some(original_path),
        projected_path: new_path,
        title: entity.title.clone(),
        content_path: Some(new_content_path),
        created_at: now.clone(),
        updated_at: now,
    };
    store
        .save_virtual_mutation(mutation)
        .map_err(AfsError::from)?;
    let refreshed = store.list_entities(mount_id).map_err(AfsError::from)?;
    let index = ProviderIndex::new(&refreshed);
    let item = entity_item(&mount, &entity, &index);
    Ok(VirtualFsMutationReport {
        mount_id: mount_id.0.clone(),
        identifier: remote_id.0,
        path: item.path.clone(),
        item,
    })
}

pub fn trash_virtual_fs_item<S>(
    store: &mut S,
    content_root: &Path,
    mount_id: &MountId,
    identifier: &str,
) -> AfsResult<VirtualFsMutationReport>
where
    S: MountRepository + EntityRepository + VirtualMutationRepository,
{
    let mount = require_virtual_mount(store, mount_id)?;
    if mount.read_only {
        return Err(AfsError::Unsupported(
            "read-only mounts do not accept virtual filesystem deletes",
        ));
    }
    let entities = store.list_entities(mount_id).map_err(AfsError::from)?;
    if let Some(mutation) = local_mutation(store, mount_id, identifier)? {
        let path = content_path_for_relative(content_root, &mutation.projected_path)?;
        let _ = std::fs::remove_file(path);
        store
            .delete_virtual_mutation(mount_id, &mutation.local_id)
            .map_err(AfsError::from)?;
        let index = ProviderIndex::new(&entities);
        let item = pending_item(&mount, &mutation, &index);
        return Ok(VirtualFsMutationReport {
            mount_id: mount_id.0.clone(),
            identifier: mutation.local_id,
            path: item.path.clone(),
            item,
        });
    }
    let remote_id = RemoteId::new(entity_identifier(identifier)?);
    let entity = require_entity(store, mount_id, &remote_id)?;
    if entity.kind != EntityKind::Page {
        return Err(AfsError::Unsupported(
            "only page files can be deleted by the virtual filesystem",
        ));
    }
    let now = now_string();
    let mutation = VirtualMutationRecord {
        mount_id: mount_id.clone(),
        local_id: format!("delete:{}", remote_id.0),
        mutation_kind: VirtualMutationKind::Delete,
        target_remote_id: Some(remote_id.clone()),
        parent_remote_id: None,
        original_path: Some(entity.path.clone()),
        projected_path: entity.path.clone(),
        title: entity.title.clone(),
        content_path: Some(content_path_for_relative(content_root, &entity.path)?),
        created_at: now.clone(),
        updated_at: now,
    };
    store
        .save_virtual_mutation(mutation.clone())
        .map_err(AfsError::from)?;
    let index = ProviderIndex::new(&entities);
    let item = entity_item(&mount, &entity, &index);
    Ok(VirtualFsMutationReport {
        mount_id: mount_id.0.clone(),
        identifier: remote_id.0,
        path: item.path.clone(),
        item,
    })
}

pub fn virtual_fs_content_root(state_root: &Path, mount_id: &MountId) -> PathBuf {
    virtual_fs_content_base(state_root)
        .join(&mount_id.0)
        .join("files")
}

fn virtual_fs_content_base(state_root: &Path) -> PathBuf {
    if let Ok(root) = std::env::var("AFS_VIRTUAL_FS_CONTENT_ROOT") {
        return PathBuf::from(root);
    }

    #[cfg(target_os = "macos")]
    if is_default_state_root(state_root)
        && let Some(group_container) = macos_app_group_container()
    {
        return group_container.join("content");
    }

    state_root.join("content")
}

#[cfg(target_os = "macos")]
fn is_default_state_root(state_root: &Path) -> bool {
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".afs") == state_root)
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn macos_app_group_container() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(|home| {
        PathBuf::from(home)
            .join("Library")
            .join("Group Containers")
            .join("group.ai.codeflash.afs")
    })
}

pub fn virtual_fs_content_path(
    state_root: &Path,
    mount_id: &MountId,
    relative_path: &Path,
) -> AfsResult<PathBuf> {
    content_path_for_relative(
        &virtual_fs_content_root(state_root, mount_id),
        relative_path,
    )
}

fn local_mutation<S>(
    store: &S,
    mount_id: &MountId,
    identifier: &str,
) -> AfsResult<Option<VirtualMutationRecord>>
where
    S: VirtualMutationRepository,
{
    if !identifier.starts_with(LOCAL_PREFIX) {
        return Ok(None);
    }
    store
        .get_virtual_mutation(mount_id, identifier)
        .map_err(AfsError::from)
}

fn pending_deleted_remote_ids(mutations: &[VirtualMutationRecord]) -> BTreeSet<RemoteId> {
    mutations
        .iter()
        .filter(|mutation| mutation.mutation_kind == VirtualMutationKind::Delete)
        .filter_map(|mutation| mutation.target_remote_id.clone())
        .collect()
}

fn schema_item_for_container(
    mount: &MountConfig,
    entities: &[EntityRecord],
    content_root: &Path,
    container_identifier: &str,
) -> AfsResult<Option<VirtualFsItem>> {
    if container_identifier == ROOT_CONTAINER_IDENTIFIER
        || container_identifier.starts_with(CHILDREN_PREFIX)
        || container_identifier.starts_with(PATH_PREFIX)
    {
        return Ok(None);
    }
    let remote_id = RemoteId::new(container_identifier);
    let Some(entity) = entities
        .iter()
        .find(|entity| entity.remote_id == remote_id && entity.kind == EntityKind::Database)
    else {
        return Ok(None);
    };
    let path = content_path_for_relative(content_root, &entity.path.join("_schema.yaml"))?;
    Ok(path
        .exists()
        .then(|| schema_item(mount, entity, Some(path))))
}

fn create_parent_remote_id(
    entities: &[EntityRecord],
    parent_identifier: &str,
) -> AfsResult<RemoteId> {
    if let Some(remote_id) = parent_identifier.strip_prefix(CHILDREN_PREFIX) {
        return Ok(RemoteId::new(remote_id));
    }
    if parent_identifier == ROOT_CONTAINER_IDENTIFIER || parent_identifier.starts_with(PATH_PREFIX)
    {
        return Err(AfsError::Unsupported(
            "new virtual filesystem files must be created inside a page or database directory",
        ));
    }
    let remote_id = RemoteId::new(parent_identifier);
    let entity = entities
        .iter()
        .find(|entity| entity.remote_id == remote_id)
        .ok_or_else(|| missing_identifier(parent_identifier))?;
    match entity.kind {
        EntityKind::Page | EntityKind::Database => Ok(remote_id),
        EntityKind::Directory | EntityKind::Asset | EntityKind::Unknown(_) => {
            Err(AfsError::Unsupported(
                "new virtual filesystem files must be created inside a page or database directory",
            ))
        }
    }
}

fn ensure_virtual_path_available<S>(store: &S, mount_id: &MountId, path: &Path) -> AfsResult<()>
where
    S: EntityRepository + VirtualMutationRepository,
{
    if store
        .find_entity_by_path(mount_id, path)
        .map_err(AfsError::from)?
        .is_some()
        || store
            .find_virtual_mutation_by_path(mount_id, path)
            .map_err(AfsError::from)?
            .is_some()
    {
        return Err(AfsError::InvalidState(format!(
            "virtual filesystem path `{}` already exists",
            path.display()
        )));
    }
    Ok(())
}

fn ensure_virtual_path_available_for_rename<S>(
    store: &S,
    mount_id: &MountId,
    identifier: &str,
    path: &Path,
) -> AfsResult<()>
where
    S: EntityRepository + VirtualMutationRepository,
{
    if let Some(entity) = store
        .find_entity_by_path(mount_id, path)
        .map_err(AfsError::from)?
        && entity.remote_id.0 != identifier
    {
        return Err(AfsError::InvalidState(format!(
            "virtual filesystem path `{}` already exists",
            path.display()
        )));
    }
    if let Some(mutation) = store
        .find_virtual_mutation_by_path(mount_id, path)
        .map_err(AfsError::from)?
        && mutation.local_id != identifier
    {
        return Err(AfsError::InvalidState(format!(
            "virtual filesystem path `{}` already exists",
            path.display()
        )));
    }
    Ok(())
}

fn rename_cached_file_if_present(from: &Path, to: &Path) -> AfsResult<()> {
    if !from.exists() {
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            AfsError::Io(format!(
                "failed to create virtual filesystem content directory `{}`: {error}",
                parent.display()
            ))
        })?;
    }
    std::fs::rename(from, to).map_err(|error| {
        AfsError::Io(format!(
            "failed to rename virtual filesystem content `{}` to `{}`: {error}",
            from.display(),
            to.display()
        ))
    })
}

fn new_local_id() -> String {
    format!("{}{}", LOCAL_PREFIX, unique_suffix())
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{}-{nanos}", std::process::id())
}

fn now_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

fn resolve_item(
    mount: &MountConfig,
    entities: &[EntityRecord],
    mutations: &[VirtualMutationRecord],
    index: &ProviderIndex,
    identifier: &str,
) -> AfsResult<VirtualFsItem> {
    if identifier == ROOT_CONTAINER_IDENTIFIER {
        return Ok(root_item(mount));
    }

    if let Some(remote_id) = identifier.strip_prefix(CHILDREN_PREFIX) {
        let entity = entities
            .iter()
            .find(|entity| entity.remote_id.0 == remote_id && entity.kind == EntityKind::Page)
            .ok_or_else(|| missing_identifier(identifier))?;
        return Ok(page_child_dir_item(
            mount,
            &entity.path.with_extension(""),
            &entity.remote_id,
            index,
        ));
    }

    if let Some(path) = identifier.strip_prefix(PATH_PREFIX) {
        let path = PathBuf::from(path);
        return Ok(path_dir_item(mount, &path, index));
    }

    if identifier.starts_with(LOCAL_PREFIX) {
        let mutation = mutations
            .iter()
            .find(|mutation| mutation.local_id == identifier)
            .ok_or_else(|| missing_identifier(identifier))?;
        return Ok(pending_item(mount, mutation, index));
    }

    if let Some(remote_id) = identifier.strip_prefix(SCHEMA_PREFIX) {
        let entity = entities
            .iter()
            .find(|entity| entity.remote_id.0 == remote_id && entity.kind == EntityKind::Database)
            .ok_or_else(|| missing_identifier(identifier))?;
        return Ok(schema_item(mount, entity, None));
    }

    let remote_id = RemoteId::new(identifier);
    let entity = entities
        .iter()
        .find(|entity| entity.remote_id == remote_id)
        .ok_or_else(|| missing_identifier(identifier))?;
    if mutations.iter().any(|mutation| {
        mutation.mutation_kind == VirtualMutationKind::Delete
            && mutation.target_remote_id.as_ref() == Some(&remote_id)
    }) {
        return Err(missing_identifier(identifier));
    }
    Ok(entity_item(mount, entity, index))
}

fn root_item(mount: &MountConfig) -> VirtualFsItem {
    VirtualFsItem {
        identifier: ROOT_CONTAINER_IDENTIFIER.to_string(),
        parent_identifier: None,
        filename: mount
            .root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| mount.mount_id.0.clone()),
        kind: VirtualFsItemKind::Folder,
        entity_kind: None,
        remote_id: None,
        path: String::new(),
        hydration: None,
        content_type: "public.folder".to_string(),
        remote_edited_at: None,
        materialized_path: None,
        byte_size: None,
    }
}

fn entity_item(mount: &MountConfig, entity: &EntityRecord, index: &ProviderIndex) -> VirtualFsItem {
    let kind = match &entity.kind {
        EntityKind::Page | EntityKind::Asset | EntityKind::Unknown(_) => VirtualFsItemKind::File,
        EntityKind::Database | EntityKind::Directory => VirtualFsItemKind::Folder,
    };
    let content_type = match &entity.kind {
        EntityKind::Page => "net.daringfireball.markdown",
        EntityKind::Database | EntityKind::Directory => "public.folder",
        EntityKind::Asset | EntityKind::Unknown(_) => "public.data",
    };
    let materialized_path = if entity.hydration == HydrationState::Hydrated {
        Some(mount.root.join(&entity.path).display().to_string())
    } else {
        None
    };

    VirtualFsItem {
        identifier: entity.remote_id.0.clone(),
        parent_identifier: Some(container_identifier_for_path(
            parent_path(&entity.path),
            index,
        )),
        filename: filename(&entity.path),
        kind,
        entity_kind: Some(entity.kind.clone()),
        remote_id: Some(entity.remote_id.0.clone()),
        path: path_string(&entity.path),
        hydration: Some(entity.hydration.clone()),
        content_type: content_type.to_string(),
        remote_edited_at: entity.remote_edited_at.clone(),
        materialized_path,
        byte_size: None,
    }
}

fn pending_item(
    _mount: &MountConfig,
    mutation: &VirtualMutationRecord,
    index: &ProviderIndex,
) -> VirtualFsItem {
    VirtualFsItem {
        identifier: mutation.local_id.clone(),
        parent_identifier: Some(container_identifier_for_path(
            parent_path(&mutation.projected_path),
            index,
        )),
        filename: filename(&mutation.projected_path),
        kind: VirtualFsItemKind::File,
        entity_kind: Some(EntityKind::Page),
        remote_id: None,
        path: path_string(&mutation.projected_path),
        hydration: Some(HydrationState::Dirty),
        content_type: "net.daringfireball.markdown".to_string(),
        remote_edited_at: None,
        materialized_path: mutation
            .content_path
            .as_ref()
            .map(|path| path.display().to_string()),
        byte_size: mutation
            .content_path
            .as_ref()
            .and_then(|path| path.metadata().ok())
            .map(|metadata| metadata.len()),
    }
}

fn schema_item(
    mount: &MountConfig,
    entity: &EntityRecord,
    materialized_path: Option<PathBuf>,
) -> VirtualFsItem {
    let path = entity.path.join("_schema.yaml");
    VirtualFsItem {
        identifier: format!("{SCHEMA_PREFIX}{}", entity.remote_id.0),
        parent_identifier: Some(entity.remote_id.0.clone()),
        filename: "_schema.yaml".to_string(),
        kind: VirtualFsItemKind::File,
        entity_kind: None,
        remote_id: Some(entity.remote_id.0.clone()),
        path: path_string(&path),
        hydration: Some(HydrationState::Hydrated),
        content_type: "public.yaml".to_string(),
        remote_edited_at: entity.remote_edited_at.clone(),
        materialized_path: materialized_path
            .or_else(|| Some(mount.root.join(path).to_path_buf()))
            .map(|path| path.display().to_string()),
        byte_size: None,
    }
}

fn rewrite_item_materialized_path(content_root: &Path, item: &mut VirtualFsItem) -> AfsResult<()> {
    if item.kind == VirtualFsItemKind::File
        && item
            .hydration
            .as_ref()
            .is_some_and(is_materialized_hydration)
    {
        let path = content_path_for_relative(content_root, Path::new(&item.path))?;
        item.byte_size = path.metadata().ok().map(|metadata| metadata.len());
        item.materialized_path = Some(path.display().to_string());
    }
    Ok(())
}

fn is_materialized_hydration(hydration: &HydrationState) -> bool {
    matches!(
        hydration,
        HydrationState::Hydrated | HydrationState::Dirty | HydrationState::Conflicted
    )
}

fn page_child_dir_item(
    mount: &MountConfig,
    path: &Path,
    remote_id: &RemoteId,
    index: &ProviderIndex,
) -> VirtualFsItem {
    VirtualFsItem {
        identifier: format!("{CHILDREN_PREFIX}{}", remote_id.0),
        parent_identifier: Some(container_identifier_for_path(parent_path(path), index)),
        filename: filename(path),
        kind: VirtualFsItemKind::Folder,
        entity_kind: None,
        remote_id: Some(remote_id.0.clone()),
        path: path_string(path),
        hydration: None,
        content_type: "public.folder".to_string(),
        remote_edited_at: None,
        materialized_path: Some(mount.root.join(path).display().to_string()),
        byte_size: None,
    }
}

fn path_dir_item(mount: &MountConfig, path: &Path, index: &ProviderIndex) -> VirtualFsItem {
    VirtualFsItem {
        identifier: format!("{PATH_PREFIX}{}", path_string(path)),
        parent_identifier: Some(container_identifier_for_path(parent_path(path), index)),
        filename: filename(path),
        kind: VirtualFsItemKind::Folder,
        entity_kind: None,
        remote_id: None,
        path: path_string(path),
        hydration: None,
        content_type: "public.folder".to_string(),
        remote_edited_at: None,
        materialized_path: Some(mount.root.join(path).display().to_string()),
        byte_size: None,
    }
}

fn container_path(entities: &[EntityRecord], identifier: &str) -> AfsResult<PathBuf> {
    if identifier == ROOT_CONTAINER_IDENTIFIER {
        return Ok(PathBuf::new());
    }

    if let Some(remote_id) = identifier.strip_prefix(CHILDREN_PREFIX) {
        let entity = entities
            .iter()
            .find(|entity| entity.remote_id.0 == remote_id && entity.kind == EntityKind::Page)
            .ok_or_else(|| missing_identifier(identifier))?;
        return Ok(entity.path.with_extension(""));
    }

    if let Some(path) = identifier.strip_prefix(PATH_PREFIX) {
        return Ok(PathBuf::from(path));
    }

    let remote_id = RemoteId::new(identifier);
    let entity = entities
        .iter()
        .find(|entity| entity.remote_id == remote_id)
        .ok_or_else(|| missing_identifier(identifier))?;
    if matches!(entity.kind, EntityKind::Database | EntityKind::Directory) {
        return Ok(entity.path.clone());
    }
    if entity.kind == EntityKind::Page {
        return Ok(entity.path.with_extension(""));
    }

    Err(AfsError::InvalidState(format!(
        "virtual filesystem item `{identifier}` is not a container"
    )))
}

fn child_container_for_identifier(
    entities: &[EntityRecord],
    identifier: &str,
) -> AfsResult<Option<ChildContainer>> {
    if identifier == ROOT_CONTAINER_IDENTIFIER {
        return Ok(Some(ChildContainer::Root));
    }

    if let Some(remote_id) = identifier.strip_prefix(CHILDREN_PREFIX) {
        return Ok(Some(ChildContainer::PageChildren(RemoteId::new(remote_id))));
    }

    if identifier.starts_with(PATH_PREFIX) {
        return Ok(None);
    }

    let remote_id = RemoteId::new(identifier);
    let Some(entity) = entities.iter().find(|entity| entity.remote_id == remote_id) else {
        return Err(missing_identifier(identifier));
    };

    Ok(match entity.kind {
        EntityKind::Page => Some(ChildContainer::PageChildren(remote_id)),
        EntityKind::Database => Some(ChildContainer::DatabaseRows(remote_id)),
        EntityKind::Directory | EntityKind::Asset | EntityKind::Unknown(_) => None,
    })
}

fn has_known_entity_child(entities: &[EntityRecord], parent: &Path) -> bool {
    entities
        .iter()
        .any(|entity| parent_path(&entity.path) == parent)
}

fn refreshed_entity_record(
    entry: afs_core::model::TreeEntry,
    existing: Option<&EntityRecord>,
) -> EntityRecord {
    let mut record = EntityRecord::from(entry);
    if let Some(existing) = existing {
        record.path = existing.path.clone();
        record.hydration = existing.hydration.clone();
        record.content_hash = existing.content_hash.clone();
    }
    record
}

fn entity_identifier(identifier: &str) -> AfsResult<String> {
    if identifier == ROOT_CONTAINER_IDENTIFIER
        || identifier.starts_with(CHILDREN_PREFIX)
        || identifier.starts_with(PATH_PREFIX)
    {
        return Err(AfsError::InvalidState(format!(
            "virtual filesystem identifier `{identifier}` is not a materializable file"
        )));
    }
    Ok(identifier.to_string())
}

fn require_mount<S>(store: &S, mount_id: &MountId) -> AfsResult<MountConfig>
where
    S: MountRepository,
{
    store
        .get_mount(mount_id)
        .map_err(AfsError::from)?
        .ok_or_else(|| StoreError::MountMissing(mount_id.clone()).into())
}

fn require_virtual_mount<S>(store: &S, mount_id: &MountId) -> AfsResult<MountConfig>
where
    S: MountRepository,
{
    let mount = require_mount(store, mount_id)?;
    if !mount.projection.uses_virtual_filesystem() {
        return Err(AfsError::Unsupported(
            "plain-files mounts do not support virtual filesystem operations",
        ));
    }
    Ok(mount)
}

fn require_entity<S>(store: &S, mount_id: &MountId, remote_id: &RemoteId) -> AfsResult<EntityRecord>
where
    S: EntityRepository,
{
    store
        .get_entity(mount_id, remote_id)
        .map_err(AfsError::from)?
        .ok_or_else(|| {
            StoreError::EntityMissing {
                mount_id: mount_id.clone(),
                remote_id: remote_id.clone(),
            }
            .into()
        })
}

fn missing_identifier(identifier: &str) -> AfsError {
    AfsError::InvalidState(format!(
        "virtual filesystem item `{identifier}` is not present in daemon state"
    ))
}

fn parent_path(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| *parent != Path::new(""))
        .unwrap_or_else(|| Path::new(""))
}

fn filename(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path_string(path))
}

fn title_from_filename(filename: &str) -> String {
    filename
        .strip_suffix(".md")
        .unwrap_or(filename)
        .trim()
        .to_string()
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn content_path_for_relative(content_root: &Path, relative_path: &Path) -> AfsResult<PathBuf> {
    validate_relative_path(relative_path)?;
    Ok(content_root.join(relative_path))
}

fn validate_relative_path(path: &Path) -> AfsResult<()> {
    if path.as_os_str().is_empty() {
        return Err(AfsError::InvalidState(
            "virtual filesystem content path cannot be empty".to_string(),
        ));
    }
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::Prefix(_)
                | std::path::Component::RootDir
                | std::path::Component::ParentDir
        )
    }) {
        return Err(AfsError::InvalidState(format!(
            "virtual filesystem content path `{}` must be relative",
            path.display()
        )));
    }
    Ok(())
}

fn write_binary_atomic(path: &Path, contents: &[u8]) -> AfsResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            AfsError::Io(format!(
                "failed to create virtual filesystem content directory `{}`: {error}",
                parent.display()
            ))
        })?;
    }

    let file_name = path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("afs-virtual-fs");
    let temp_path = path.with_file_name(format!(".{file_name}.afs-tmp"));
    std::fs::write(&temp_path, contents).map_err(|error| {
        AfsError::Io(format!(
            "failed to write virtual filesystem temp file `{}`: {error}",
            temp_path.display()
        ))
    })?;
    std::fs::rename(&temp_path, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp_path);
        AfsError::Io(format!(
            "failed to replace virtual filesystem content `{}`: {error}",
            path.display()
        ))
    })
}

fn container_identifier_for_path(path: &Path, index: &ProviderIndex) -> String {
    if path.as_os_str().is_empty() {
        return ROOT_CONTAINER_IDENTIFIER.to_string();
    }

    if let Some(entity) = index.entities_by_path.get(path)
        && matches!(&entity.kind, EntityKind::Database | EntityKind::Directory)
    {
        return entity.remote_id.0.clone();
    }

    if let Some(remote_id) = index.page_child_dirs.get(path) {
        return format!("{CHILDREN_PREFIX}{}", remote_id.0);
    }

    format!("{PATH_PREFIX}{}", path_string(path))
}

struct ProviderIndex {
    entities_by_path: BTreeMap<PathBuf, EntityRecord>,
    page_child_dirs: BTreeMap<PathBuf, RemoteId>,
}

impl ProviderIndex {
    fn new(entities: &[EntityRecord]) -> Self {
        let mut entities_by_path = BTreeMap::new();
        let mut page_child_dirs = BTreeMap::new();
        for entity in entities {
            entities_by_path.insert(entity.path.clone(), entity.clone());
            if entity.kind == EntityKind::Page {
                page_child_dirs.insert(entity.path.with_extension(""), entity.remote_id.clone());
            }
        }

        Self {
            entities_by_path,
            page_child_dirs,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use afs_connector::{
        ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, Connector,
        ConnectorCapabilities, ConnectorKind, EnumerateRequest, FetchRequest, ListChildrenRequest,
        ListChildrenResult, NativeEntity, ParsedEntity,
    };
    use afs_core::{
        AfsError,
        hydration::HydrationRequest,
        model::{CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, TreeEntry},
    };
    use afs_store::{
        EntityRecord, EntityRepository, InMemoryStateStore, MountConfig, MountRepository,
        ProjectionMode, VirtualMutationKind, VirtualMutationRepository,
    };

    use crate::hydration::{HydratedEntity, HydrationSource};

    use super::{
        ROOT_CONTAINER_IDENTIFIER, VirtualFsItemKind, VirtualFsMaterializeOutcome,
        commit_virtual_fs_write, create_virtual_fs_file,
        materialize_virtual_fs_item_with_content_root, refresh_virtual_fs_children,
        rename_virtual_fs_item, trash_virtual_fs_item, virtual_fs_children,
        virtual_fs_content_path, virtual_fs_item, virtual_fs_item_with_content_root,
    };

    #[test]
    fn children_include_page_files_and_synthetic_page_child_folders() {
        let mount_id = MountId::new("notion-main");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount(&mount_id))
            .expect("save mount");
        store
            .save_entity(EntityRecord {
                mount_id: mount_id.clone(),
                remote_id: RemoteId::new("page-root"),
                kind: EntityKind::Page,
                title: "Home".to_string(),
                path: "Home.md".into(),
                hydration: HydrationState::Stub,
                content_hash: None,
                remote_edited_at: None,
            })
            .expect("save root page");
        store
            .save_entity(EntityRecord {
                mount_id: mount_id.clone(),
                remote_id: RemoteId::new("page-child"),
                kind: EntityKind::Page,
                title: "Child".to_string(),
                path: "Home/Child.md".into(),
                hydration: HydrationState::Stub,
                content_hash: None,
                remote_edited_at: None,
            })
            .expect("save child page");

        let report = virtual_fs_children(&store, &mount_id, ROOT_CONTAINER_IDENTIFIER)
            .expect("root children");

        assert_eq!(report.children.len(), 2);
        assert_eq!(report.children[0].filename, "Home");
        assert_eq!(report.children[0].kind, VirtualFsItemKind::Folder);
        assert_eq!(report.children[0].identifier, "children:page-root");
        assert_eq!(report.children[1].filename, "Home.md");
        assert_eq!(report.children[1].kind, VirtualFsItemKind::File);
    }

    #[test]
    fn child_folder_lists_nested_pages_under_stable_page_identifier() {
        let mount_id = MountId::new("notion-main");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount(&mount_id))
            .expect("save mount");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("page-root"),
                EntityKind::Page,
                "Home",
                "Home.md",
            ))
            .expect("save root page");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("page-child"),
                EntityKind::Page,
                "Child",
                "Home/Child.md",
            ))
            .expect("save child page");

        let report =
            virtual_fs_children(&store, &mount_id, "children:page-root").expect("children");

        assert_eq!(report.children.len(), 2);
        assert_eq!(report.children[0].identifier, "children:page-child");
        assert_eq!(report.children[0].kind, VirtualFsItemKind::Folder);
        assert_eq!(report.children[1].identifier, "page-child");
        assert_eq!(
            report.children[1].parent_identifier.as_deref(),
            Some("children:page-root")
        );
    }

    #[test]
    fn refresh_children_fetches_missing_container_metadata_once() {
        let mount_id = MountId::new("notion-main");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount(&mount_id))
            .expect("save mount");
        let connector = StaticChildrenConnector {
            entries: vec![TreeEntry {
                mount_id: mount_id.clone(),
                remote_id: RemoteId::new("page-root"),
                kind: EntityKind::Page,
                title: "Home".to_string(),
                path: "home ~pagero.md".into(),
                hydration: HydrationState::Stub,
                content_hash: None,
                remote_edited_at: None,
                stub_frontmatter: None,
            }],
        };

        let saved = refresh_virtual_fs_children(
            &mut store,
            &connector,
            &mount_id,
            ROOT_CONTAINER_IDENTIFIER,
        )
        .expect("refresh children");
        assert_eq!(saved, 1);

        let report = virtual_fs_children(&store, &mount_id, ROOT_CONTAINER_IDENTIFIER)
            .expect("root children");
        assert_eq!(report.children.len(), 2);
        assert_eq!(report.children[0].identifier, "children:page-root");
        assert_eq!(report.children[0].kind, VirtualFsItemKind::Folder);
        assert_eq!(report.children[1].identifier, "page-root");
        assert_eq!(report.children[1].kind, VirtualFsItemKind::File);

        let saved = refresh_virtual_fs_children(
            &mut store,
            &connector,
            &mount_id,
            ROOT_CONTAINER_IDENTIFIER,
        )
        .expect("refresh cached children");
        assert_eq!(saved, 0);
    }

    struct StaticChildrenConnector {
        entries: Vec<TreeEntry>,
    }

    impl Connector for StaticChildrenConnector {
        fn kind(&self) -> ConnectorKind {
            ConnectorKind("test")
        }

        fn capabilities(&self) -> ConnectorCapabilities {
            ConnectorCapabilities {
                supports_block_updates: false,
                supports_databases: false,
                supports_oauth: false,
            }
        }

        fn enumerate(&self, _request: EnumerateRequest) -> afs_core::AfsResult<Vec<TreeEntry>> {
            Ok(self.entries.clone())
        }

        fn list_children(
            &self,
            request: ListChildrenRequest,
        ) -> afs_core::AfsResult<ListChildrenResult> {
            assert_eq!(request.parent_path, PathBuf::new());
            Ok(ListChildrenResult {
                entries: self.entries.clone(),
            })
        }

        fn fetch(&self, request: FetchRequest) -> afs_core::AfsResult<NativeEntity> {
            let _ = request;
            Err(afs_core::AfsError::NotImplemented("fixture fetch"))
        }

        fn render(&self, _entity: &NativeEntity) -> afs_core::AfsResult<CanonicalDocument> {
            Err(afs_core::AfsError::NotImplemented("fixture render"))
        }

        fn parse(&self, _document: &CanonicalDocument) -> afs_core::AfsResult<ParsedEntity> {
            Err(afs_core::AfsError::NotImplemented("fixture parse"))
        }

        fn check_concurrency(&self, _request: ApplyPlanRequest<'_>) -> afs_core::AfsResult<()> {
            Ok(())
        }

        fn apply(&self, _request: ApplyPlanRequest<'_>) -> afs_core::AfsResult<ApplyPlanResult> {
            Err(afs_core::AfsError::NotImplemented("fixture apply"))
        }

        fn apply_undo(
            &self,
            _request: ApplyUndoRequest<'_>,
        ) -> afs_core::AfsResult<ApplyUndoResult> {
            Err(afs_core::AfsError::NotImplemented("fixture undo"))
        }
    }

    struct FailingHydrationSource;

    impl HydrationSource for FailingHydrationSource {
        fn fetch_render(&self, _request: &HydrationRequest) -> afs_core::AfsResult<HydratedEntity> {
            panic!("conflicted cache should not fetch remote content")
        }
    }

    #[test]
    fn item_metadata_is_store_only_for_online_only_files() {
        let mount_id = MountId::new("notion-main");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount(&mount_id))
            .expect("save mount");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("page-1"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap.md",
            ))
            .expect("save page");

        let report = virtual_fs_item(&store, &mount_id, "page-1").expect("item");

        assert_eq!(report.item.filename, "Roadmap.md");
        assert_eq!(report.item.kind, VirtualFsItemKind::File);
        assert_eq!(report.item.materialized_path, None);
    }

    #[test]
    fn conflicted_virtual_file_materializes_from_existing_cache_without_fetch() {
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let state_root = temp_root("afs-virtual-fs-conflicted-materialize");
        let content_root = state_root.join("content/notion-main/files");
        let conflicted_contents = b"<<<<<<< LOCAL\nlocal\n=======\nremote\n>>>>>>> REMOTE\n";
        std::fs::create_dir_all(&content_root).expect("content root");
        std::fs::write(content_root.join("Roadmap.md"), conflicted_contents)
            .expect("write conflicted cache");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount(&mount_id))
            .expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    remote_id,
                    EntityKind::Page,
                    "Roadmap",
                    "Roadmap.md",
                )
                .with_hydration(HydrationState::Conflicted),
            )
            .expect("save page");

        let item = virtual_fs_item_with_content_root(&store, &content_root, &mount_id, "page-1")
            .expect("item");
        let expected_path = content_root
            .join("Roadmap.md")
            .to_string_lossy()
            .to_string();
        assert_eq!(
            item.item.materialized_path.as_deref(),
            Some(expected_path.as_str())
        );
        assert_eq!(item.item.byte_size, Some(conflicted_contents.len() as u64));

        let report = materialize_virtual_fs_item_with_content_root(
            &mut store,
            &FailingHydrationSource,
            &content_root,
            &mount_id,
            "page-1",
        )
        .expect("materialize conflicted cache");

        assert_eq!(
            report.outcome,
            VirtualFsMaterializeOutcome::AlreadyMaterialized
        );
        assert_eq!(report.hydration, HydrationState::Conflicted);
        assert_eq!(
            std::fs::read(report.path).expect("read materialized cache"),
            conflicted_contents
        );
        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn content_cache_paths_reject_escape_components() {
        let mount_id = MountId::new("notion-main");
        let state_root = std::path::Path::new("/tmp/afs-state");

        let path =
            virtual_fs_content_path(state_root, &mount_id, std::path::Path::new("Roadmap.md"))
                .expect("content path");
        assert_eq!(
            path,
            std::path::Path::new("/tmp/afs-state/content/notion-main/files/Roadmap.md")
        );

        assert!(
            virtual_fs_content_path(state_root, &mount_id, std::path::Path::new("../escape.md"))
                .is_err()
        );
        assert!(
            virtual_fs_content_path(state_root, &mount_id, std::path::Path::new("/escape.md"))
                .is_err()
        );
    }

    #[test]
    fn commit_write_records_cache_bytes_and_marks_dirty() {
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let state_root = temp_root("afs-virtual-fs-commit");
        let content_root = state_root.join("content/notion-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount(&mount_id))
            .expect("save mount");
        store
            .save_entity(EntityRecord {
                mount_id: mount_id.clone(),
                remote_id: remote_id.clone(),
                kind: EntityKind::Page,
                title: "Roadmap".to_string(),
                path: "Roadmap.md".into(),
                hydration: HydrationState::Hydrated,
                content_hash: None,
                remote_edited_at: None,
            })
            .expect("save entity");

        let report =
            commit_virtual_fs_write(&mut store, &content_root, &mount_id, "page-1", b"edited")
                .expect("commit write");

        assert_eq!(report.bytes_written, 6);
        assert_eq!(
            std::fs::read(content_root.join("Roadmap.md")).expect("read cache"),
            b"edited"
        );
        assert_eq!(
            store
                .get_entity(&mount_id, &remote_id)
                .expect("get entity")
                .expect("entity")
                .hydration,
            HydrationState::Dirty
        );
        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn commit_write_marks_stub_entity_dirty() {
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let state_root = temp_root("afs-virtual-fs-commit-stub");
        let content_root = state_root.join("content/notion-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount(&mount_id))
            .expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    remote_id.clone(),
                    EntityKind::Page,
                    "Roadmap",
                    "Roadmap.md",
                )
                .with_hydration(HydrationState::Stub),
            )
            .expect("save entity");

        let report =
            commit_virtual_fs_write(&mut store, &content_root, &mount_id, "page-1", b"edited")
                .expect("commit write");

        assert_eq!(report.hydration, HydrationState::Dirty);
        assert_eq!(
            store
                .get_entity(&mount_id, &remote_id)
                .expect("get entity")
                .expect("entity")
                .hydration,
            HydrationState::Dirty
        );
        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn commit_write_marks_stub_conflict_conflicted() {
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let state_root = temp_root("afs-virtual-fs-commit-stub-conflict");
        let content_root = state_root.join("content/notion-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount(&mount_id))
            .expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    remote_id.clone(),
                    EntityKind::Page,
                    "Roadmap",
                    "Roadmap.md",
                )
                .with_hydration(HydrationState::Stub),
            )
            .expect("save entity");

        let report = commit_virtual_fs_write(
            &mut store,
            &content_root,
            &mount_id,
            "page-1",
            b"<<<<<<< LOCAL\nlocal\n=======\nremote\n>>>>>>> REMOTE\n",
        )
        .expect("commit write");

        assert_eq!(report.hydration, HydrationState::Conflicted);
        assert_eq!(
            store
                .get_entity(&mount_id, &remote_id)
                .expect("get entity")
                .expect("entity")
                .hydration,
            HydrationState::Conflicted
        );
        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn create_file_adds_pending_child_and_cache_file() {
        let mount_id = MountId::new("notion-main");
        let state_root = temp_root("afs-virtual-fs-create");
        let content_root = state_root.join("content/notion-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount(&mount_id))
            .expect("save mount");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("page-root"),
                EntityKind::Page,
                "Home",
                "Home.md",
            ))
            .expect("save parent page");

        let created = create_virtual_fs_file(
            &mut store,
            &content_root,
            &mount_id,
            "children:page-root",
            "Draft.md",
        )
        .expect("create virtual file");

        assert!(created.identifier.starts_with("local:"));
        assert_eq!(created.item.filename, "Draft.md");
        assert_eq!(
            std::fs::read(content_root.join("Home/Draft.md")).expect("read pending cache"),
            b""
        );
        let children =
            virtual_fs_children(&store, &mount_id, "children:page-root").expect("children");
        assert!(
            children.children.iter().any(
                |child| child.identifier == created.identifier && child.path == "Home/Draft.md"
            )
        );
        assert_eq!(
            store
                .list_virtual_mutations(&mount_id)
                .expect("list mutations")[0]
                .mutation_kind,
            VirtualMutationKind::Create
        );

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn rename_pending_file_updates_overlay_and_moves_cache() {
        let mount_id = MountId::new("notion-main");
        let state_root = temp_root("afs-virtual-fs-rename");
        let content_root = state_root.join("content/notion-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount(&mount_id))
            .expect("save mount");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("page-root"),
                EntityKind::Page,
                "Home",
                "Home.md",
            ))
            .expect("save parent page");
        let created = create_virtual_fs_file(
            &mut store,
            &content_root,
            &mount_id,
            "children:page-root",
            "Draft.md",
        )
        .expect("create virtual file");
        std::fs::write(content_root.join("Home/Draft.md"), b"pending body").expect("write cache");

        let renamed = rename_virtual_fs_item(
            &mut store,
            &content_root,
            &mount_id,
            &created.identifier,
            "children:page-root",
            "Updated.md",
        )
        .expect("rename virtual file");

        assert_eq!(renamed.identifier, created.identifier);
        assert!(!content_root.join("Home/Draft.md").exists());
        assert_eq!(
            std::fs::read(content_root.join("Home/Updated.md")).expect("read renamed cache"),
            b"pending body"
        );
        let mutation = store
            .get_virtual_mutation(&mount_id, &created.identifier)
            .expect("get mutation")
            .expect("mutation");
        assert_eq!(mutation.projected_path, PathBuf::from("Home/Updated.md"));

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn trash_remote_page_records_tombstone_and_hides_child() {
        let mount_id = MountId::new("notion-main");
        let state_root = temp_root("afs-virtual-fs-trash");
        let content_root = state_root.join("content/notion-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount(&mount_id))
            .expect("save mount");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("page-1"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap.md",
            ))
            .expect("save page");

        trash_virtual_fs_item(&mut store, &content_root, &mount_id, "page-1")
            .expect("trash virtual file");

        let children =
            virtual_fs_children(&store, &mount_id, ROOT_CONTAINER_IDENTIFIER).expect("children");
        assert!(
            !children
                .children
                .iter()
                .any(|child| child.identifier == "page-1")
        );
        let mutation = store
            .get_virtual_mutation(&mount_id, "delete:page-1")
            .expect("get mutation")
            .expect("mutation");
        assert_eq!(mutation.mutation_kind, VirtualMutationKind::Delete);

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn plain_files_mount_rejects_virtual_filesystem_operations() {
        let mount_id = MountId::new("notion-main");
        let state_root = temp_root("afs-virtual-fs-plain-rejects");
        let content_root = state_root.join("content/notion-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(MountConfig::new(
                mount_id.clone(),
                "notion",
                "/tmp/afs/notion",
            ))
            .expect("save plain mount");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("page-1"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap.md",
            ))
            .expect("save page");

        assert_plain_files_virtual_error(virtual_fs_item(&store, &mount_id, "page-1"));
        assert_plain_files_virtual_error(virtual_fs_children(
            &store,
            &mount_id,
            ROOT_CONTAINER_IDENTIFIER,
        ));
        assert_plain_files_virtual_error(commit_virtual_fs_write(
            &mut store,
            &content_root,
            &mount_id,
            "page-1",
            b"edited",
        ));
        assert_plain_files_virtual_error(create_virtual_fs_file(
            &mut store,
            &content_root,
            &mount_id,
            ROOT_CONTAINER_IDENTIFIER,
            "Draft.md",
        ));
        assert_plain_files_virtual_error(refresh_virtual_fs_children(
            &mut store,
            &StaticChildrenConnector {
                entries: Vec::new(),
            },
            &mount_id,
            ROOT_CONTAINER_IDENTIFIER,
        ));
        assert!(
            store
                .list_virtual_mutations(&mount_id)
                .expect("list mutations")
                .is_empty()
        );
        assert!(!content_root.exists());

        let _ = std::fs::remove_dir_all(state_root);
    }

    fn assert_plain_files_virtual_error<T: std::fmt::Debug>(result: afs_core::AfsResult<T>) {
        assert!(matches!(
            result.expect_err("plain-files virtual filesystem rejection"),
            AfsError::Unsupported(
                "plain-files mounts do not support virtual filesystem operations"
            )
        ));
    }

    fn virtual_mount(mount_id: &MountId) -> MountConfig {
        MountConfig::new(mount_id.clone(), "notion", "/tmp/afs/notion")
            .projection(ProjectionMode::LinuxFuse)
    }

    fn temp_root(prefix: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{unique}", std::process::id()))
    }
}
