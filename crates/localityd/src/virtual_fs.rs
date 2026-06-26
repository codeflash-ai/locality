use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use locality_connector::{ChildContainer, Connector, ListChildrenRequest};
use locality_core::canonical::{parse_canonical_markdown, render_canonical_markdown};
use locality_core::conflict::has_unresolved_conflict_markers;
use locality_core::freshness::FreshnessTier;
use locality_core::hydration::{HydrationReason, HydrationRequest};
use locality_core::model::{CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId};
use locality_core::path_projection::{
    is_page_document_path, page_container_path, page_document_path, page_listing_parent_path,
};
use locality_core::{LocalityError, LocalityResult};
use locality_store::{
    EntityRecord, EntityRepository, FreshnessStateRepository, MountConfig, MountRepository,
    ShadowRepository, StoreError, VirtualMutationKind, VirtualMutationRecord,
    VirtualMutationRepository,
};
use serde::{Deserialize, Serialize};

use crate::hydration::{
    HydrationExecutor, HydrationOutcome, HydrationSource, write_parent_database_schema_cache,
};
use crate::shadow_match::parsed_matches_shadow;
use crate::source::source_descriptor;

pub const ROOT_CONTAINER_IDENTIFIER: &str = "root";
pub const SOURCE_ROOT_PREFIX: &str = "source:";
pub const MOUNT_POINT_PREFIX: &str = "mount:";
const CHILDREN_PREFIX: &str = "children:";
const PATH_PREFIX: &str = "path:";
const LOCAL_PREFIX: &str = "local:";
const SCHEMA_PREFIX: &str = "schema:";
const GUIDANCE_PREFIX: &str = "guidance:";
const AGENTS_FILE: &str = "AGENTS.md";
const CLAUDE_FILE: &str = "CLAUDE.md";
const AGENTS_GUIDANCE_IDENTIFIER: &str = "guidance:AGENTS.md";
const CLAUDE_GUIDANCE_IDENTIFIER: &str = "guidance:CLAUDE.md";

pub fn source_root_identifier(connector: &str) -> String {
    format!(
        "{SOURCE_ROOT_PREFIX}{}",
        source_root_directory_name(connector)
    )
}

pub fn source_root_directory_name(connector: &str) -> String {
    let normalized = connector
        .chars()
        .filter_map(|character| {
            if character.is_ascii_alphanumeric() {
                Some(character.to_ascii_lowercase())
            } else if matches!(character, '-' | '_') {
                Some('-')
            } else {
                None
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if normalized.is_empty() {
        "source".to_string()
    } else {
        normalized
    }
}

pub fn mount_point_identifier(mount: &MountConfig) -> String {
    format!("{MOUNT_POINT_PREFIX}{}", mount.mount_id.0)
}

pub fn mount_point_directory_name(mount: &MountConfig) -> String {
    mount
        .root
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| source_root_directory_name(&mount.mount_id.0))
}

pub fn virtual_projection_root(mount: &MountConfig) -> PathBuf {
    mount
        .root
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| mount.root.clone())
}

pub fn virtual_projection_mount_point(mount: &MountConfig) -> PathBuf {
    virtual_projection_root(mount).join(mount_point_directory_name(mount))
}

pub fn virtual_fs_ancestor_container_identifiers<S>(
    store: &S,
    mount_id: &MountId,
    remote_id: &RemoteId,
) -> LocalityResult<Vec<String>>
where
    S: MountRepository + EntityRepository,
{
    let mount = require_virtual_mount(store, mount_id)?;
    let entities = store.list_entities(mount_id).map_err(LocalityError::from)?;
    let entity = entities
        .iter()
        .find(|entity| entity.remote_id == *remote_id)
        .ok_or_else(|| StoreError::EntityMissing {
            mount_id: mount_id.clone(),
            remote_id: remote_id.clone(),
        })?;
    let index = ProviderIndex::new(&entities);
    let target_container = match entity.kind {
        EntityKind::Page => page_container_path(&entity.path),
        EntityKind::Database | EntityKind::Directory => entity.path.clone(),
        EntityKind::Asset | EntityKind::Unknown(_) => parent_path(&entity.path).to_path_buf(),
    };

    let mut identifiers = vec![
        ROOT_CONTAINER_IDENTIFIER.to_string(),
        mount_point_identifier(&mount),
    ];
    let mut path = PathBuf::new();
    for component in target_container.components() {
        path.push(component.as_os_str());
        identifiers.push(container_identifier_for_path(&mount, &path, &index));
    }
    identifiers.dedup();

    Ok(identifiers)
}

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
) -> LocalityResult<VirtualFsItemReport>
where
    S: MountRepository + EntityRepository + VirtualMutationRepository,
{
    let mount = require_virtual_mount(store, mount_id)?;
    let entities = store.list_entities(mount_id).map_err(LocalityError::from)?;
    let mutations = store
        .list_virtual_mutations(mount_id)
        .map_err(LocalityError::from)?;
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
) -> LocalityResult<VirtualFsItemReport>
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
) -> LocalityResult<VirtualFsChildrenReport>
where
    S: MountRepository + EntityRepository + VirtualMutationRepository,
{
    let mount = require_virtual_mount(store, mount_id)?;
    let entities = store.list_entities(mount_id).map_err(LocalityError::from)?;
    let mutations = store
        .list_virtual_mutations(mount_id)
        .map_err(LocalityError::from)?;
    let index = ProviderIndex::new(&entities);
    if container_identifier == ROOT_CONTAINER_IDENTIFIER {
        return Ok(VirtualFsChildrenReport {
            mount_id: mount_id.0.clone(),
            container_identifier: container_identifier.to_string(),
            children: root_children(&mount),
        });
    }

    let container_path = container_path(&mount, &entities, &mutations, container_identifier)?;
    let mut children = Vec::new();
    if container_identifier == mount_point_identifier(&mount) {
        children.extend(source_root_guidance_items(&mount));
    }
    let deleted_remote_ids = pending_deleted_remote_ids(&mutations);

    for entity in &entities {
        if deleted_remote_ids.contains(&entity.remote_id) {
            continue;
        }
        if entity_listing_parent_path(entity) == container_path {
            if entity.kind == EntityKind::Page {
                children.push(page_child_dir_item(
                    &mount,
                    &page_container_path(&entity.path),
                    &entity.remote_id,
                    &index,
                ));
            } else {
                children.push(entity_item(&mount, entity, &index));
            }
        }
        if entity.kind == EntityKind::Page && page_container_path(&entity.path) == container_path {
            children.push(entity_item(&mount, entity, &index));
        }
    }
    for mutation in &mutations {
        if mutation.mutation_kind == VirtualMutationKind::Create
            && pending_listing_parent_path(mutation) == container_path
        {
            children.push(pending_listing_item(&mount, mutation, &index));
        }
        if mutation.mutation_kind == VirtualMutationKind::Create
            && is_page_document_path(&mutation.projected_path)
            && page_container_path(&mutation.projected_path) == container_path
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
) -> LocalityResult<VirtualFsChildrenReport>
where
    S: MountRepository + EntityRepository + VirtualMutationRepository,
{
    let mut report = virtual_fs_children(store, mount_id, container_identifier)?;
    let mount = require_mount(store, mount_id)?;
    let entities = store.list_entities(mount_id).map_err(LocalityError::from)?;
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
) -> LocalityResult<usize>
where
    S: MountRepository + EntityRepository,
    C: Connector + ?Sized,
{
    let mount = require_virtual_mount(store, mount_id)?;
    let entities = store.list_entities(mount_id).map_err(LocalityError::from)?;
    let parent_path = container_path(&mount, &entities, &[], container_identifier)?;
    if has_known_entity_child(&entities, &parent_path) {
        return Ok(0);
    }

    let Some(container) = child_container_for_identifier(&mount, &entities, container_identifier)?
    else {
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
            .map_err(LocalityError::from)?;
        let record = refreshed_entity_record(entry, existing.as_ref());
        store.save_entity(record).map_err(LocalityError::from)?;
        saved += 1;
    }

    Ok(saved)
}

pub fn virtual_fs_children_refresh_needed<S>(
    store: &S,
    mount_id: &MountId,
    container_identifier: &str,
) -> LocalityResult<bool>
where
    S: MountRepository + EntityRepository,
{
    let mount = require_virtual_mount(store, mount_id)?;
    let entities = store.list_entities(mount_id).map_err(LocalityError::from)?;
    let parent_path = container_path(&mount, &entities, &[], container_identifier)?;
    if has_known_entity_child(&entities, &parent_path) {
        return Ok(false);
    }

    child_container_for_identifier(&mount, &entities, container_identifier)
        .map(|container| container.is_some())
}

pub fn materialize_virtual_fs_item<S, Source>(
    store: &mut S,
    source: &Source,
    mount_id: &MountId,
    identifier: &str,
) -> LocalityResult<VirtualFsMaterializeReport>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + FreshnessStateRepository
        + locality_store::RemoteObservationRepository,
    Source: HydrationSource + ?Sized,
{
    let mount = require_virtual_mount(store, mount_id)?;
    if let Some(report) =
        materialize_guidance_item(&mount, mount.root.as_path(), identifier, false)?
    {
        return Ok(report);
    }
    let remote_id = RemoteId::new(entity_identifier(identifier)?);
    let entity = require_entity(store, mount_id, &remote_id)?;
    if entity.kind != EntityKind::Page {
        return Err(LocalityError::Unsupported(
            "only page.md files can be materialized by the virtual filesystem",
        ));
    }

    let path = mount.root.join(&entity.path);
    let outcome = if is_materialized_hydration(&entity.hydration) && path.exists() {
        write_parent_database_schema_cache(store, source, &mount, &entity, &mount.root)?;
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
            HydrationOutcome::RemoteDeleted => {
                return Err(LocalityError::RemoteNotFound(format!(
                    "remote item `{}` was deleted",
                    remote_id.0
                )));
            }
        }
    };

    let hydrated = require_entity(store, mount_id, &remote_id)?;
    if !path.exists() {
        return Err(LocalityError::InvalidState(format!(
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
) -> LocalityResult<VirtualFsMaterializeReport>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + VirtualMutationRepository
        + FreshnessStateRepository
        + locality_store::RemoteObservationRepository,
    Source: HydrationSource + ?Sized,
{
    let mount = require_virtual_mount(store, mount_id)?;
    if let Some(report) = materialize_guidance_item(&mount, content_root, identifier, true)? {
        return Ok(report);
    }
    if let Some(mutation) = local_mutation(store, mount_id, identifier)? {
        if mutation.mutation_kind != VirtualMutationKind::Create {
            return Err(LocalityError::Unsupported(
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
            return Err(LocalityError::Unsupported(
                "only database schemas can be materialized by the virtual filesystem",
            ));
        }
        let path = content_path_for_relative(content_root, &entity.path.join("_schema.yaml"))?;
        if !path.exists() {
            return Err(LocalityError::InvalidState(format!(
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
        return Err(LocalityError::Unsupported(
            "only page.md files can be materialized by the virtual filesystem",
        ));
    }

    let path = content_path_for_relative(content_root, &entity.path)?;
    let outcome = if is_materialized_hydration(&entity.hydration) && path.exists() {
        write_parent_database_schema_cache(store, source, &mount, &entity, content_root)?;
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
            HydrationOutcome::RemoteDeleted => {
                return Err(LocalityError::RemoteNotFound(format!(
                    "remote item `{}` was deleted",
                    remote_id.0
                )));
            }
        }
    };

    let hydrated = require_entity(store, mount_id, &remote_id)?;
    if !path.exists() {
        return Err(LocalityError::InvalidState(format!(
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
) -> LocalityResult<VirtualFsWriteReport>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + VirtualMutationRepository
        + FreshnessStateRepository,
{
    let mount = require_virtual_mount(store, mount_id)?;
    if mount.read_only {
        return Err(LocalityError::Unsupported(
            "read-only mounts do not accept virtual filesystem writes",
        ));
    }
    if is_guidance_identifier(identifier) {
        return Err(LocalityError::Unsupported(
            "agent guidance files are read-only in virtual filesystem mounts",
        ));
    }
    if let Some(mut mutation) = local_mutation(store, mount_id, identifier)? {
        if mutation.mutation_kind != VirtualMutationKind::Create {
            return Err(LocalityError::Unsupported(
                "only pending-created local files can be written by local virtual identifier",
            ));
        }
        let path = content_path_for_relative(content_root, &mutation.projected_path)?;
        write_binary_atomic(&path, contents)?;
        mutation.content_path = Some(path.clone());
        mutation.updated_at = now_string();
        store
            .save_virtual_mutation(mutation)
            .map_err(LocalityError::from)?;
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
        return Err(LocalityError::Unsupported(
            "database schema files are read-only in virtual filesystem mounts",
        ));
    }
    let remote_id = RemoteId::new(entity_identifier(identifier)?);
    let mut entity = require_entity(store, mount_id, &remote_id)?;
    if entity.kind != EntityKind::Page {
        return Err(LocalityError::Unsupported(
            "only page.md files can be written by the virtual filesystem",
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
                .map(|shadow| parsed_matches_shadow(&parsed, &shadow))
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
    store
        .save_entity(entity.clone())
        .map_err(LocalityError::from)?;
    if matches!(
        entity.hydration,
        HydrationState::Dirty | HydrationState::Conflicted
    ) {
        record_virtual_local_change(store, &entity)?;
    }

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
) -> LocalityResult<VirtualFsMutationReport>
where
    S: MountRepository + EntityRepository + VirtualMutationRepository + FreshnessStateRepository,
{
    let mount = require_virtual_mount(store, mount_id)?;
    if mount.read_only {
        return Err(LocalityError::Unsupported(
            "read-only mounts do not accept virtual filesystem creates",
        ));
    }
    let atomic_temp = is_atomic_temp_filename(filename);
    if !filename.ends_with(".md") && !atomic_temp {
        return Err(LocalityError::Unsupported(
            "virtual filesystem creates currently support only Markdown files and atomic write temp files",
        ));
    }
    let entities = store.list_entities(mount_id).map_err(LocalityError::from)?;
    let mutations = store
        .list_virtual_mutations(mount_id)
        .map_err(LocalityError::from)?;
    if let Some(mutation) = pending_page_directory_mutation(&mutations, parent_identifier)? {
        if filename != locality_core::path_projection::PAGE_DOCUMENT_FILENAME
            && !is_page_document_atomic_temp_filename(filename)
        {
            return Err(LocalityError::Unsupported(
                "pending page directories currently accept only page.md or page.md atomic temp files",
            ));
        }
        let index = ProviderIndex::new(&entities);
        let item = if filename == locality_core::path_projection::PAGE_DOCUMENT_FILENAME {
            pending_item(&mount, mutation, &index)
        } else {
            pending_temp_item(&mount, mutation, filename)
        };
        return Ok(VirtualFsMutationReport {
            mount_id: mount_id.0.clone(),
            identifier: mutation.local_id.clone(),
            path: item.path.clone(),
            item,
        });
    }

    let parent_path = container_path(&mount, &entities, &mutations, parent_identifier)?;
    let parent_remote_id = create_parent_remote_id(&mount, &entities, parent_identifier)?;
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
        .map_err(LocalityError::from)?;
    let index = ProviderIndex::new(&entities);
    let item = pending_item(&mount, &mutation, &index);
    Ok(VirtualFsMutationReport {
        mount_id: mount_id.0.clone(),
        identifier: mutation.local_id,
        path: item.path.clone(),
        item,
    })
}

pub fn create_virtual_fs_directory<S>(
    store: &mut S,
    content_root: &Path,
    mount_id: &MountId,
    parent_identifier: &str,
    dirname: &str,
) -> LocalityResult<VirtualFsMutationReport>
where
    S: MountRepository + EntityRepository + VirtualMutationRepository + FreshnessStateRepository,
{
    let mount = require_virtual_mount(store, mount_id)?;
    if mount.read_only {
        return Err(LocalityError::Unsupported(
            "read-only mounts do not accept virtual filesystem creates",
        ));
    }
    if dirname.trim().is_empty() || dirname.contains('/') {
        return Err(LocalityError::Unsupported(
            "virtual filesystem directory creates require a simple directory name",
        ));
    }
    let entities = store.list_entities(mount_id).map_err(LocalityError::from)?;
    let mutations = store
        .list_virtual_mutations(mount_id)
        .map_err(LocalityError::from)?;
    let parent_path = container_path(&mount, &entities, &mutations, parent_identifier)?;
    let parent_remote_id = create_parent_remote_id(&mount, &entities, parent_identifier)?;
    let page_dir = parent_path.join(dirname);
    let projected_path = page_document_path(&page_dir);
    ensure_virtual_page_directory_available(store, mount_id, &page_dir, &projected_path)?;
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
        title: title_from_filename(dirname),
        content_path: Some(path),
        created_at: now.clone(),
        updated_at: now,
    };
    store
        .save_virtual_mutation(mutation.clone())
        .map_err(LocalityError::from)?;
    let index = ProviderIndex::new(&entities);
    let item = pending_page_child_dir_item(&mount, &mutation, &index);
    Ok(VirtualFsMutationReport {
        mount_id: mount_id.0.clone(),
        identifier: pending_page_child_dir_identifier(&mutation.local_id),
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
) -> LocalityResult<VirtualFsMutationReport>
where
    S: MountRepository + EntityRepository + VirtualMutationRepository + FreshnessStateRepository,
{
    let mount = require_virtual_mount(store, mount_id)?;
    if mount.read_only {
        return Err(LocalityError::Unsupported(
            "read-only mounts do not accept virtual filesystem renames",
        ));
    }
    let entities = store.list_entities(mount_id).map_err(LocalityError::from)?;
    let mutations = store
        .list_virtual_mutations(mount_id)
        .map_err(LocalityError::from)?;
    let new_parent_path = container_path(&mount, &entities, &mutations, new_parent_identifier)?;

    if let Some(child_identifier) = identifier.strip_prefix(CHILDREN_PREFIX) {
        let new_page_dir = new_parent_path.join(new_filename);
        let new_path = page_document_path(&new_page_dir);
        let title = title_from_filename(new_filename);

        if child_identifier.starts_with(LOCAL_PREFIX) {
            let mut mutation = store
                .get_virtual_mutation(mount_id, child_identifier)
                .map_err(LocalityError::from)?
                .ok_or_else(|| missing_identifier(identifier))?;
            if mutation.mutation_kind != VirtualMutationKind::Create
                || !is_page_document_path(&mutation.projected_path)
            {
                return Err(LocalityError::Unsupported(
                    "only pending-created page directories can be renamed by the virtual filesystem",
                ));
            }
            let old_parent = page_listing_parent_path(&mutation.projected_path);
            if old_parent != new_parent_path {
                return Err(LocalityError::Unsupported(
                    "moving virtual filesystem page directories across parents is not supported yet",
                ));
            }
            ensure_virtual_page_directory_available_for_rename(
                store,
                mount_id,
                RenameOwner::Local(child_identifier),
                &new_page_dir,
                &new_path,
            )?;
            let old_path = content_path_for_relative(content_root, &mutation.projected_path)?;
            let new_content_path = content_path_for_relative(content_root, &new_path)?;
            rename_cached_page_if_present(&old_path, &new_content_path, &title)?;
            mutation.projected_path = new_path;
            mutation.title = title;
            mutation.content_path = Some(new_content_path);
            mutation.updated_at = now_string();
            store
                .save_virtual_mutation(mutation.clone())
                .map_err(LocalityError::from)?;
            let index = ProviderIndex::new(&entities);
            let item = pending_page_child_dir_item(&mount, &mutation, &index);
            return Ok(VirtualFsMutationReport {
                mount_id: mount_id.0.clone(),
                identifier: item.identifier.clone(),
                path: item.path.clone(),
                item,
            });
        }

        let remote_id = RemoteId::new(child_identifier);
        let mut entity = require_entity(store, mount_id, &remote_id)?;
        if entity.kind != EntityKind::Page {
            return Err(LocalityError::Unsupported(
                "only page directories can be renamed by the virtual filesystem",
            ));
        }
        let old_parent = page_listing_parent_path(&entity.path);
        if old_parent != new_parent_path {
            return Err(LocalityError::Unsupported(
                "moving virtual filesystem page directories across parents is not supported yet",
            ));
        }
        ensure_virtual_page_directory_available_for_rename(
            store,
            mount_id,
            RenameOwner::Remote(&remote_id),
            &new_page_dir,
            &new_path,
        )?;
        let old_path = content_path_for_relative(content_root, &entity.path)?;
        let new_content_path = content_path_for_relative(content_root, &new_path)?;
        rename_cached_page_if_present(&old_path, &new_content_path, &title)?;
        let original_path = entity.path.clone();
        entity.path = new_path.clone();
        entity.title = title;
        if entity.hydration.can_transition_to(&HydrationState::Dirty) {
            entity.hydration = HydrationState::Dirty;
        }
        store
            .save_entity(entity.clone())
            .map_err(LocalityError::from)?;
        record_virtual_local_change(store, &entity)?;
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
            .map_err(LocalityError::from)?;
        let refreshed = store.list_entities(mount_id).map_err(LocalityError::from)?;
        let index = ProviderIndex::new(&refreshed);
        let item = page_child_dir_item(
            &mount,
            &page_container_path(&entity.path),
            &entity.remote_id,
            &index,
        );
        return Ok(VirtualFsMutationReport {
            mount_id: mount_id.0.clone(),
            identifier: item.identifier.clone(),
            path: item.path.clone(),
            item,
        });
    }

    if !new_filename.ends_with(".md") {
        return Err(LocalityError::Unsupported(
            "virtual filesystem renames currently support Markdown files or page directories",
        ));
    }
    let new_path = new_parent_path.join(new_filename);
    ensure_virtual_path_available_for_rename(store, mount_id, identifier, &new_path)?;

    if let Some(mut mutation) = local_mutation(store, mount_id, identifier)? {
        let old_parent = parent_path(&mutation.projected_path).to_path_buf();
        if old_parent != new_parent_path {
            return Err(LocalityError::Unsupported(
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
            .map_err(LocalityError::from)?;
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
        return Err(LocalityError::Unsupported(
            "only page.md files can be renamed by the virtual filesystem",
        ));
    }
    let old_parent = parent_path(&entity.path).to_path_buf();
    if old_parent != new_parent_path {
        return Err(LocalityError::Unsupported(
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
    store
        .save_entity(entity.clone())
        .map_err(LocalityError::from)?;
    record_virtual_local_change(store, &entity)?;
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
        .map_err(LocalityError::from)?;
    let refreshed = store.list_entities(mount_id).map_err(LocalityError::from)?;
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
) -> LocalityResult<VirtualFsMutationReport>
where
    S: MountRepository + EntityRepository + VirtualMutationRepository + FreshnessStateRepository,
{
    let mount = require_virtual_mount(store, mount_id)?;
    if mount.read_only {
        return Err(LocalityError::Unsupported(
            "read-only mounts do not accept virtual filesystem deletes",
        ));
    }
    let entities = store.list_entities(mount_id).map_err(LocalityError::from)?;
    let mutations = store
        .list_virtual_mutations(mount_id)
        .map_err(LocalityError::from)?;
    if let Some(child_identifier) = identifier.strip_prefix(CHILDREN_PREFIX) {
        if child_identifier.starts_with(LOCAL_PREFIX) {
            let mutation = pending_page_directory_mutation(&mutations, identifier)?
                .ok_or_else(|| missing_identifier(identifier))?
                .clone();
            let path = content_path_for_relative(content_root, &mutation.projected_path)?;
            let _ = std::fs::remove_file(path);
            store
                .delete_virtual_mutation(mount_id, &mutation.local_id)
                .map_err(LocalityError::from)?;
            let index = ProviderIndex::new(&entities);
            let item = pending_page_child_dir_item(&mount, &mutation, &index);
            return Ok(VirtualFsMutationReport {
                mount_id: mount_id.0.clone(),
                identifier: identifier.to_string(),
                path: item.path.clone(),
                item,
            });
        }

        let remote_id = RemoteId::new(child_identifier);
        let entity = require_entity(store, mount_id, &remote_id)?;
        if entity.kind != EntityKind::Page {
            return Err(LocalityError::Unsupported(
                "only page directories can be deleted by the virtual filesystem",
            ));
        }
        return record_virtual_fs_page_delete(store, content_root, &mount, &entities, entity, true);
    }

    if let Some(mutation) = local_mutation(store, mount_id, identifier)? {
        let path = content_path_for_relative(content_root, &mutation.projected_path)?;
        let _ = std::fs::remove_file(path);
        store
            .delete_virtual_mutation(mount_id, &mutation.local_id)
            .map_err(LocalityError::from)?;
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
        return Err(LocalityError::Unsupported(
            "only page.md files can be deleted by the virtual filesystem",
        ));
    }
    record_virtual_fs_page_delete(store, content_root, &mount, &entities, entity, false)
}

fn record_virtual_fs_page_delete<S>(
    store: &mut S,
    content_root: &Path,
    mount: &MountConfig,
    entities: &[EntityRecord],
    entity: EntityRecord,
    directory_item: bool,
) -> LocalityResult<VirtualFsMutationReport>
where
    S: EntityRepository + VirtualMutationRepository + FreshnessStateRepository,
{
    let mount_id = &mount.mount_id;
    let remote_id = entity.remote_id.clone();
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
        .map_err(LocalityError::from)?;
    record_virtual_local_change(store, &entity)?;
    let index = ProviderIndex::new(entities);
    let item = if directory_item {
        page_child_dir_item(
            mount,
            &page_container_path(&entity.path),
            &entity.remote_id,
            &index,
        )
    } else {
        entity_item(mount, &entity, &index)
    };
    Ok(VirtualFsMutationReport {
        mount_id: mount_id.0.clone(),
        identifier: item.identifier.clone(),
        path: item.path.clone(),
        item,
    })
}

pub fn virtual_fs_content_root(state_root: &Path, mount_id: &MountId) -> PathBuf {
    virtual_fs_content_base(state_root)
        .join(&mount_id.0)
        .join("files")
}

pub fn virtual_fs_content_base(state_root: &Path) -> PathBuf {
    if let Ok(root) = std::env::var("LOCALITY_VIRTUAL_FS_CONTENT_ROOT") {
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
        .map(|home| PathBuf::from(home).join(".loc") == state_root)
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn macos_app_group_container() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(|home| {
        PathBuf::from(home)
            .join("Library")
            .join("Group Containers")
            .join("C484HB7Q6S.group.ai.codeflash.locality")
    })
}

pub fn virtual_fs_content_path(
    state_root: &Path,
    mount_id: &MountId,
    relative_path: &Path,
) -> LocalityResult<PathBuf> {
    content_path_for_relative(
        &virtual_fs_content_root(state_root, mount_id),
        relative_path,
    )
}

fn local_mutation<S>(
    store: &S,
    mount_id: &MountId,
    identifier: &str,
) -> LocalityResult<Option<VirtualMutationRecord>>
where
    S: VirtualMutationRepository,
{
    if !identifier.starts_with(LOCAL_PREFIX) {
        return Ok(None);
    }
    store
        .get_virtual_mutation(mount_id, identifier)
        .map_err(LocalityError::from)
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
) -> LocalityResult<Option<VirtualFsItem>> {
    if container_identifier == ROOT_CONTAINER_IDENTIFIER
        || container_identifier == mount_point_identifier(mount)
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
    mount: &MountConfig,
    entities: &[EntityRecord],
    parent_identifier: &str,
) -> LocalityResult<RemoteId> {
    if let Some(remote_id) = parent_identifier.strip_prefix(CHILDREN_PREFIX) {
        if remote_id.starts_with(LOCAL_PREFIX) {
            return Err(LocalityError::Unsupported(
                "new virtual filesystem files cannot be created under an unpushed local page",
            ));
        }
        return Ok(RemoteId::new(remote_id));
    }
    if parent_identifier == mount_point_identifier(mount) {
        if source_descriptor(&mount.connector)
            .source_root_create_parent_kind()
            .is_some()
            && let Some(remote_root_id) = &mount.remote_root_id
        {
            return Ok(remote_root_id.clone());
        }
        return Err(LocalityError::Unsupported(
            "new virtual filesystem files must be created inside a page or database directory",
        ));
    }
    if parent_identifier == ROOT_CONTAINER_IDENTIFIER || parent_identifier.starts_with(PATH_PREFIX)
    {
        return Err(LocalityError::Unsupported(
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
            Err(LocalityError::Unsupported(
                "new virtual filesystem files must be created inside a page or database directory",
            ))
        }
    }
}

fn ensure_virtual_path_available<S>(
    store: &S,
    mount_id: &MountId,
    path: &Path,
) -> LocalityResult<()>
where
    S: EntityRepository + VirtualMutationRepository,
{
    if store
        .find_entity_by_path(mount_id, path)
        .map_err(LocalityError::from)?
        .is_some()
        || store
            .find_virtual_mutation_by_path(mount_id, path)
            .map_err(LocalityError::from)?
            .is_some()
    {
        return Err(LocalityError::InvalidState(format!(
            "virtual filesystem path `{}` already exists",
            path.display()
        )));
    }
    Ok(())
}

fn ensure_virtual_page_directory_available<S>(
    store: &S,
    mount_id: &MountId,
    page_dir: &Path,
    page_path: &Path,
) -> LocalityResult<()>
where
    S: EntityRepository + VirtualMutationRepository,
{
    ensure_virtual_path_available(store, mount_id, page_path)?;
    if store
        .find_entity_by_path(mount_id, page_dir)
        .map_err(LocalityError::from)?
        .is_some()
        || store
            .find_virtual_mutation_by_path(mount_id, page_dir)
            .map_err(LocalityError::from)?
            .is_some()
    {
        return Err(LocalityError::InvalidState(format!(
            "virtual filesystem path `{}` already exists",
            page_dir.display()
        )));
    }
    for entity in store.list_entities(mount_id).map_err(LocalityError::from)? {
        if entity.kind == EntityKind::Page && page_container_path(&entity.path) == page_dir {
            return Err(LocalityError::InvalidState(format!(
                "virtual filesystem path `{}` already exists",
                page_dir.display()
            )));
        }
    }
    for mutation in store
        .list_virtual_mutations(mount_id)
        .map_err(LocalityError::from)?
    {
        if mutation.mutation_kind == VirtualMutationKind::Create
            && is_page_document_path(&mutation.projected_path)
            && page_container_path(&mutation.projected_path) == page_dir
        {
            return Err(LocalityError::InvalidState(format!(
                "virtual filesystem path `{}` already exists",
                page_dir.display()
            )));
        }
    }
    Ok(())
}

fn ensure_virtual_path_available_for_rename<S>(
    store: &S,
    mount_id: &MountId,
    identifier: &str,
    path: &Path,
) -> LocalityResult<()>
where
    S: EntityRepository + VirtualMutationRepository,
{
    if let Some(entity) = store
        .find_entity_by_path(mount_id, path)
        .map_err(LocalityError::from)?
        && entity.remote_id.0 != identifier
    {
        return Err(LocalityError::InvalidState(format!(
            "virtual filesystem path `{}` already exists",
            path.display()
        )));
    }
    if let Some(mutation) = store
        .find_virtual_mutation_by_path(mount_id, path)
        .map_err(LocalityError::from)?
        && mutation.local_id != identifier
    {
        return Err(LocalityError::InvalidState(format!(
            "virtual filesystem path `{}` already exists",
            path.display()
        )));
    }
    Ok(())
}

enum RenameOwner<'a> {
    Remote(&'a RemoteId),
    Local(&'a str),
}

fn ensure_virtual_page_directory_available_for_rename<S>(
    store: &S,
    mount_id: &MountId,
    owner: RenameOwner<'_>,
    page_dir: &Path,
    page_path: &Path,
) -> LocalityResult<()>
where
    S: EntityRepository + VirtualMutationRepository,
{
    if let Some(entity) = store
        .find_entity_by_path(mount_id, page_path)
        .map_err(LocalityError::from)?
        && !matches!(owner, RenameOwner::Remote(remote_id) if entity.remote_id == *remote_id)
    {
        return Err(LocalityError::InvalidState(format!(
            "virtual filesystem path `{}` already exists",
            page_path.display()
        )));
    }
    if let Some(mutation) = store
        .find_virtual_mutation_by_path(mount_id, page_path)
        .map_err(LocalityError::from)?
        && !matches!(owner, RenameOwner::Local(local_id) if mutation.local_id == local_id)
    {
        return Err(LocalityError::InvalidState(format!(
            "virtual filesystem path `{}` already exists",
            page_path.display()
        )));
    }
    for entity in store.list_entities(mount_id).map_err(LocalityError::from)? {
        if entity.kind == EntityKind::Page
            && page_container_path(&entity.path) == page_dir
            && !matches!(owner, RenameOwner::Remote(remote_id) if entity.remote_id == *remote_id)
        {
            return Err(LocalityError::InvalidState(format!(
                "virtual filesystem path `{}` already exists",
                page_dir.display()
            )));
        }
    }
    for mutation in store
        .list_virtual_mutations(mount_id)
        .map_err(LocalityError::from)?
    {
        if mutation.mutation_kind == VirtualMutationKind::Create
            && is_page_document_path(&mutation.projected_path)
            && page_container_path(&mutation.projected_path) == page_dir
            && !matches!(owner, RenameOwner::Local(local_id) if mutation.local_id == local_id)
        {
            return Err(LocalityError::InvalidState(format!(
                "virtual filesystem path `{}` already exists",
                page_dir.display()
            )));
        }
    }
    Ok(())
}

fn rename_cached_page_if_present(from: &Path, to: &Path, title: &str) -> LocalityResult<()> {
    rename_cached_file_if_present(from, to)?;
    retitle_cached_page_if_present(to, title)
}

fn rename_cached_file_if_present(from: &Path, to: &Path) -> LocalityResult<()> {
    if !from.exists() {
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            LocalityError::Io(format!(
                "failed to create virtual filesystem content directory `{}`: {error}",
                parent.display()
            ))
        })?;
    }
    std::fs::rename(from, to).map_err(|error| {
        LocalityError::Io(format!(
            "failed to rename virtual filesystem content `{}` to `{}`: {error}",
            from.display(),
            to.display()
        ))
    })
}

fn retitle_cached_page_if_present(path: &Path, title: &str) -> LocalityResult<()> {
    if !path.exists() {
        return Ok(());
    }
    let contents = std::fs::read_to_string(path).map_err(|error| {
        LocalityError::Io(format!(
            "failed to read virtual filesystem content `{}`: {error}",
            path.display()
        ))
    })?;
    if contents.trim().is_empty() {
        return Ok(());
    }
    let Ok(parsed) = parse_canonical_markdown(&contents) else {
        return Ok(());
    };
    if parsed.frontmatter.title.as_deref() == Some(title) {
        return Ok(());
    }
    let frontmatter = retitled_frontmatter(&parsed.document.frontmatter, title);
    let updated =
        render_canonical_markdown(&CanonicalDocument::new(frontmatter, parsed.document.body));
    write_binary_atomic(path, updated.as_bytes())
}

fn retitled_frontmatter(frontmatter: &str, title: &str) -> String {
    let title_line = format!("title: {}\n", yaml_string(title));
    let mut replaced = false;
    let mut out = String::new();
    for line in frontmatter.split_inclusive('\n') {
        let line_without_ending = line.trim_end_matches(['\r', '\n']);
        if !replaced && !line.starts_with([' ', '\t']) && line_without_ending.starts_with("title:")
        {
            out.push_str(&title_line);
            replaced = true;
        } else {
            out.push_str(line);
        }
    }
    if !replaced {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&title_line);
    }
    out
}

fn yaml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn new_local_id() -> String {
    format!("{}{}", LOCAL_PREFIX, unique_suffix())
}

fn record_virtual_local_change<S>(store: &mut S, entity: &EntityRecord) -> LocalityResult<()>
where
    S: FreshnessStateRepository,
{
    let mut state = store
        .get_freshness_state(&entity.mount_id, &entity.remote_id)
        .map_err(LocalityError::from)?
        .unwrap_or_else(|| {
            locality_store::FreshnessStateRecord::new(
                entity.mount_id.clone(),
                entity.remote_id.clone(),
                FreshnessTier::Hot,
            )
        });
    if FreshnessTier::Hot.is_more_urgent_than(&state.tier) {
        state.tier = FreshnessTier::Hot;
    }
    state.last_local_change_at = Some(now_string());
    store
        .save_freshness_state(state)
        .map_err(LocalityError::from)
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
) -> LocalityResult<VirtualFsItem> {
    if identifier == ROOT_CONTAINER_IDENTIFIER {
        return Ok(root_item(mount));
    }
    if identifier == mount_point_identifier(mount) {
        return Ok(source_root_item(mount));
    }
    if let Some(item) = guidance_item_for_identifier(mount, identifier) {
        return Ok(item);
    }

    if let Some(remote_id) = identifier.strip_prefix(CHILDREN_PREFIX) {
        if remote_id.starts_with(LOCAL_PREFIX) {
            let mutation = pending_page_directory_mutation(mutations, identifier)?
                .ok_or_else(|| missing_identifier(identifier))?;
            return Ok(pending_page_child_dir_item(mount, mutation, index));
        }
        let entity = entities
            .iter()
            .find(|entity| entity.remote_id.0 == remote_id && entity.kind == EntityKind::Page)
            .ok_or_else(|| missing_identifier(identifier))?;
        return Ok(page_child_dir_item(
            mount,
            &page_container_path(&entity.path),
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

fn root_children(mount: &MountConfig) -> Vec<VirtualFsItem> {
    vec![source_root_item(mount)]
}

fn source_root_guidance_items(mount: &MountConfig) -> Vec<VirtualFsItem> {
    vec![
        guidance_item(mount, AGENTS_FILE, AGENTS_GUIDANCE_IDENTIFIER),
        guidance_item(mount, CLAUDE_FILE, CLAUDE_GUIDANCE_IDENTIFIER),
    ]
}

fn guidance_item_for_identifier(mount: &MountConfig, identifier: &str) -> Option<VirtualFsItem> {
    match identifier {
        AGENTS_GUIDANCE_IDENTIFIER => Some(guidance_item(
            mount,
            AGENTS_FILE,
            AGENTS_GUIDANCE_IDENTIFIER,
        )),
        CLAUDE_GUIDANCE_IDENTIFIER => Some(guidance_item(
            mount,
            CLAUDE_FILE,
            CLAUDE_GUIDANCE_IDENTIFIER,
        )),
        _ => None,
    }
}

fn guidance_item(mount: &MountConfig, filename: &str, identifier: &str) -> VirtualFsItem {
    let contents = guidance_contents_for_connector(&mount.connector);
    VirtualFsItem {
        identifier: identifier.to_string(),
        parent_identifier: Some(mount_point_identifier(mount)),
        filename: filename.to_string(),
        kind: VirtualFsItemKind::File,
        entity_kind: None,
        remote_id: None,
        path: filename.to_string(),
        hydration: Some(HydrationState::Stub),
        content_type: "net.daringfireball.markdown".to_string(),
        remote_edited_at: None,
        materialized_path: None,
        byte_size: Some(contents.len() as u64),
    }
}

fn source_root_item(mount: &MountConfig) -> VirtualFsItem {
    let filename = mount_point_directory_name(mount);
    VirtualFsItem {
        identifier: mount_point_identifier(mount),
        parent_identifier: Some(ROOT_CONTAINER_IDENTIFIER.to_string()),
        filename: filename.clone(),
        kind: VirtualFsItemKind::Folder,
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
            mount,
            &entity_parent_container_path(entity),
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
    mount: &MountConfig,
    mutation: &VirtualMutationRecord,
    index: &ProviderIndex,
) -> VirtualFsItem {
    let parent_identifier = if is_page_document_path(&mutation.projected_path) {
        pending_page_child_dir_identifier(&mutation.local_id)
    } else {
        container_identifier_for_path(mount, parent_path(&mutation.projected_path), index)
    };
    VirtualFsItem {
        identifier: mutation.local_id.clone(),
        parent_identifier: Some(parent_identifier),
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

fn pending_listing_item(
    mount: &MountConfig,
    mutation: &VirtualMutationRecord,
    index: &ProviderIndex,
) -> VirtualFsItem {
    if is_page_document_path(&mutation.projected_path) {
        pending_page_child_dir_item(mount, mutation, index)
    } else {
        pending_item(mount, mutation, index)
    }
}

fn pending_page_child_dir_item(
    mount: &MountConfig,
    mutation: &VirtualMutationRecord,
    index: &ProviderIndex,
) -> VirtualFsItem {
    let path = page_container_path(&mutation.projected_path);
    VirtualFsItem {
        identifier: pending_page_child_dir_identifier(&mutation.local_id),
        parent_identifier: Some(container_identifier_for_path(
            mount,
            parent_path(&path),
            index,
        )),
        filename: filename(&path),
        kind: VirtualFsItemKind::Folder,
        entity_kind: Some(EntityKind::Page),
        remote_id: None,
        path: path_string(&path),
        hydration: Some(HydrationState::Dirty),
        content_type: "public.folder".to_string(),
        remote_edited_at: None,
        materialized_path: Some(mount.root.join(path).display().to_string()),
        byte_size: None,
    }
}

fn pending_temp_item(
    mount: &MountConfig,
    mutation: &VirtualMutationRecord,
    filename: &str,
) -> VirtualFsItem {
    let parent = page_container_path(&mutation.projected_path);
    let path = parent.join(filename);
    VirtualFsItem {
        identifier: mutation.local_id.clone(),
        parent_identifier: Some(pending_page_child_dir_identifier(&mutation.local_id)),
        filename: filename.to_string(),
        kind: VirtualFsItemKind::File,
        entity_kind: Some(EntityKind::Page),
        remote_id: None,
        path: path_string(&path),
        hydration: Some(HydrationState::Dirty),
        content_type: "public.data".to_string(),
        remote_edited_at: None,
        materialized_path: mutation
            .content_path
            .as_ref()
            .map(|path| path.display().to_string())
            .or_else(|| Some(mount.root.join(path).display().to_string())),
        byte_size: mutation
            .content_path
            .as_ref()
            .and_then(|path| path.metadata().ok())
            .map(|metadata| metadata.len()),
    }
}

fn pending_page_child_dir_identifier(local_id: &str) -> String {
    format!("{CHILDREN_PREFIX}{local_id}")
}

fn pending_listing_parent_path(mutation: &VirtualMutationRecord) -> PathBuf {
    if is_page_document_path(&mutation.projected_path) {
        return page_listing_parent_path(&mutation.projected_path);
    }
    parent_path(&mutation.projected_path).to_path_buf()
}

fn pending_page_directory_mutation<'a>(
    mutations: &'a [VirtualMutationRecord],
    identifier: &str,
) -> LocalityResult<Option<&'a VirtualMutationRecord>> {
    let Some(local_id) = identifier.strip_prefix(CHILDREN_PREFIX) else {
        return Ok(None);
    };
    if !local_id.starts_with(LOCAL_PREFIX) {
        return Ok(None);
    }
    let mutation = mutations
        .iter()
        .find(|mutation| mutation.local_id == local_id)
        .ok_or_else(|| missing_identifier(identifier))?;
    if mutation.mutation_kind != VirtualMutationKind::Create
        || !is_page_document_path(&mutation.projected_path)
    {
        return Err(LocalityError::Unsupported(
            "only pending-created page directories can be used as local page containers",
        ));
    }
    Ok(Some(mutation))
}

fn is_atomic_temp_filename(filename: &str) -> bool {
    filename.contains(".tmp.")
}

fn is_page_document_atomic_temp_filename(filename: &str) -> bool {
    filename.starts_with("page.md.tmp.")
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

fn rewrite_item_materialized_path(
    content_root: &Path,
    item: &mut VirtualFsItem,
) -> LocalityResult<()> {
    if let Some(file_name) = guidance_file_name(&item.identifier) {
        let path = content_path_for_relative(content_root, &guidance_cache_path(file_name))?;
        if let Some(byte_size) = path.metadata().ok().map(|metadata| metadata.len()) {
            item.byte_size = Some(byte_size);
        }
        item.hydration = Some(if path.exists() {
            HydrationState::Hydrated
        } else {
            HydrationState::Stub
        });
        item.materialized_path = path.exists().then(|| path.display().to_string());
        return Ok(());
    }
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

fn materialize_guidance_item(
    mount: &MountConfig,
    content_root: &Path,
    identifier: &str,
    use_cache_namespace: bool,
) -> LocalityResult<Option<VirtualFsMaterializeReport>> {
    let Some(file_name) = guidance_file_name(identifier) else {
        return Ok(None);
    };
    let contents = guidance_contents_for_connector(&mount.connector);
    let relative_path = if use_cache_namespace {
        guidance_cache_path(file_name)
    } else {
        PathBuf::from(file_name)
    };
    let path = content_path_for_relative(content_root, &relative_path)?;
    let needs_write = match std::fs::read(&path) {
        Ok(existing) => existing != contents.as_bytes(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => {
            return Err(LocalityError::Io(format!(
                "failed to read virtual filesystem guidance `{}`: {error}",
                path.display()
            )));
        }
    };
    let outcome = if needs_write {
        write_binary_atomic(&path, contents.as_bytes())?;
        VirtualFsMaterializeOutcome::Hydrated
    } else {
        VirtualFsMaterializeOutcome::AlreadyMaterialized
    };

    Ok(Some(VirtualFsMaterializeReport {
        mount_id: mount.mount_id.0.clone(),
        identifier: identifier.to_string(),
        remote_id: identifier.to_string(),
        path: path.display().to_string(),
        outcome,
        hydration: HydrationState::Hydrated,
    }))
}

fn guidance_file_name(identifier: &str) -> Option<&'static str> {
    match identifier {
        AGENTS_GUIDANCE_IDENTIFIER => Some(AGENTS_FILE),
        CLAUDE_GUIDANCE_IDENTIFIER => Some(CLAUDE_FILE),
        _ => None,
    }
}

fn is_guidance_identifier(identifier: &str) -> bool {
    guidance_file_name(identifier).is_some()
}

fn guidance_cache_path(file_name: &str) -> PathBuf {
    PathBuf::from(".loc-guidance").join(file_name)
}

fn guidance_contents_for_connector(connector: &str) -> String {
    source_descriptor(connector).mount_guidance().to_string()
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
        parent_identifier: Some(container_identifier_for_path(
            mount,
            parent_path(path),
            index,
        )),
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
        parent_identifier: Some(container_identifier_for_path(
            mount,
            parent_path(path),
            index,
        )),
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

fn container_path(
    mount: &MountConfig,
    entities: &[EntityRecord],
    mutations: &[VirtualMutationRecord],
    identifier: &str,
) -> LocalityResult<PathBuf> {
    if identifier == ROOT_CONTAINER_IDENTIFIER {
        return Ok(PathBuf::new());
    }
    if identifier == mount_point_identifier(mount) {
        return Ok(PathBuf::new());
    }

    if let Some(remote_id) = identifier.strip_prefix(CHILDREN_PREFIX) {
        if remote_id.starts_with(LOCAL_PREFIX) {
            let mutation = pending_page_directory_mutation(mutations, identifier)?
                .ok_or_else(|| missing_identifier(identifier))?;
            return Ok(page_container_path(&mutation.projected_path));
        }
        let entity = entities
            .iter()
            .find(|entity| entity.remote_id.0 == remote_id && entity.kind == EntityKind::Page)
            .ok_or_else(|| missing_identifier(identifier))?;
        return Ok(page_container_path(&entity.path));
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
        return Ok(page_container_path(&entity.path));
    }

    Err(LocalityError::InvalidState(format!(
        "virtual filesystem item `{identifier}` is not a container"
    )))
}

fn child_container_for_identifier(
    mount: &MountConfig,
    entities: &[EntityRecord],
    identifier: &str,
) -> LocalityResult<Option<ChildContainer>> {
    if identifier == ROOT_CONTAINER_IDENTIFIER {
        return Ok(None);
    }

    if identifier == mount_point_identifier(mount) {
        return Ok(Some(ChildContainer::Root));
    }

    if let Some(remote_id) = identifier.strip_prefix(CHILDREN_PREFIX) {
        return Ok(Some(ChildContainer::PageChildren(RemoteId::new(remote_id))));
    }

    if identifier.starts_with(PATH_PREFIX) {
        return Ok(None);
    }
    if identifier
        .strip_prefix(CHILDREN_PREFIX)
        .is_some_and(|local_id| local_id.starts_with(LOCAL_PREFIX))
    {
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
        .any(|entity| entity_listing_parent_path(entity) == parent)
}

fn refreshed_entity_record(
    entry: locality_core::model::TreeEntry,
    existing: Option<&EntityRecord>,
) -> EntityRecord {
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

fn entity_identifier(identifier: &str) -> LocalityResult<String> {
    if identifier == ROOT_CONTAINER_IDENTIFIER
        || identifier.starts_with(CHILDREN_PREFIX)
        || identifier.starts_with(PATH_PREFIX)
        || identifier.starts_with(MOUNT_POINT_PREFIX)
        || identifier.starts_with(SOURCE_ROOT_PREFIX)
        || identifier.starts_with(GUIDANCE_PREFIX)
    {
        return Err(LocalityError::InvalidState(format!(
            "virtual filesystem identifier `{identifier}` is not a materializable file"
        )));
    }
    Ok(identifier.to_string())
}

fn require_mount<S>(store: &S, mount_id: &MountId) -> LocalityResult<MountConfig>
where
    S: MountRepository,
{
    store
        .get_mount(mount_id)
        .map_err(LocalityError::from)?
        .ok_or_else(|| StoreError::MountMissing(mount_id.clone()).into())
}

fn require_virtual_mount<S>(store: &S, mount_id: &MountId) -> LocalityResult<MountConfig>
where
    S: MountRepository,
{
    let mount = require_mount(store, mount_id)?;
    if !mount.projection.uses_virtual_filesystem() {
        return Err(LocalityError::Unsupported(
            "plain-files mounts do not support virtual filesystem operations",
        ));
    }
    Ok(mount)
}

fn require_entity<S>(
    store: &S,
    mount_id: &MountId,
    remote_id: &RemoteId,
) -> LocalityResult<EntityRecord>
where
    S: EntityRepository,
{
    store
        .get_entity(mount_id, remote_id)
        .map_err(LocalityError::from)?
        .ok_or_else(|| {
            StoreError::EntityMissing {
                mount_id: mount_id.clone(),
                remote_id: remote_id.clone(),
            }
            .into()
        })
}

fn missing_identifier(identifier: &str) -> LocalityError {
    LocalityError::InvalidState(format!(
        "virtual filesystem item `{identifier}` is not present in daemon state"
    ))
}

fn parent_path(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| *parent != Path::new(""))
        .unwrap_or_else(|| Path::new(""))
}

fn entity_listing_parent_path(entity: &EntityRecord) -> PathBuf {
    match entity.kind {
        EntityKind::Page => page_listing_parent_path(&entity.path),
        EntityKind::Database
        | EntityKind::Directory
        | EntityKind::Asset
        | EntityKind::Unknown(_) => parent_path(&entity.path).to_path_buf(),
    }
}

fn entity_parent_container_path(entity: &EntityRecord) -> PathBuf {
    match entity.kind {
        EntityKind::Page => page_container_path(&entity.path),
        EntityKind::Database
        | EntityKind::Directory
        | EntityKind::Asset
        | EntityKind::Unknown(_) => parent_path(&entity.path).to_path_buf(),
    }
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
    locality_platform::logical_path_display(path)
}

fn content_path_for_relative(content_root: &Path, relative_path: &Path) -> LocalityResult<PathBuf> {
    validate_relative_path(relative_path)?;
    Ok(content_root.join(relative_path))
}

fn validate_relative_path(path: &Path) -> LocalityResult<()> {
    if path.as_os_str().is_empty() {
        return Err(LocalityError::InvalidState(
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
        return Err(LocalityError::InvalidState(format!(
            "virtual filesystem content path `{}` must be relative",
            path.display()
        )));
    }
    Ok(())
}

fn write_binary_atomic(path: &Path, contents: &[u8]) -> LocalityResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            LocalityError::Io(format!(
                "failed to create virtual filesystem content directory `{}`: {error}",
                parent.display()
            ))
        })?;
    }

    let file_name = path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("loc-virtual-fs");
    let temp_path = path.with_file_name(format!(".{file_name}.loc-tmp"));
    std::fs::write(&temp_path, contents).map_err(|error| {
        LocalityError::Io(format!(
            "failed to write virtual filesystem temp file `{}`: {error}",
            temp_path.display()
        ))
    })?;
    std::fs::rename(&temp_path, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp_path);
        LocalityError::Io(format!(
            "failed to replace virtual filesystem content `{}`: {error}",
            path.display()
        ))
    })
}

fn container_identifier_for_path(
    mount: &MountConfig,
    path: &Path,
    index: &ProviderIndex,
) -> String {
    if path.as_os_str().is_empty() {
        return mount_point_identifier(mount);
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
                page_child_dirs.insert(page_container_path(&entity.path), entity.remote_id.clone());
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

    use locality_connector::{
        ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, Connector,
        ConnectorCapabilities, ConnectorKind, EnumerateRequest, FetchRequest, ListChildrenRequest,
        ListChildrenResult, NativeEntity, ParsedEntity,
    };
    use locality_core::{
        LocalityError,
        freshness::FreshnessTier,
        hydration::HydrationRequest,
        model::{CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, TreeEntry},
    };
    use locality_store::{
        EntityRecord, EntityRepository, FreshnessStateRepository, InMemoryStateStore, MountConfig,
        MountRepository, ProjectionMode, VirtualMutationKind, VirtualMutationRepository,
    };

    use crate::hydration::{HydratedEntity, HydrationSource};

    use super::{
        AGENTS_GUIDANCE_IDENTIFIER, CLAUDE_GUIDANCE_IDENTIFIER, ROOT_CONTAINER_IDENTIFIER,
        VirtualFsItemKind, VirtualFsMaterializeOutcome, commit_virtual_fs_write,
        create_virtual_fs_directory, create_virtual_fs_file,
        materialize_virtual_fs_item_with_content_root, mount_point_identifier,
        refresh_virtual_fs_children, rename_virtual_fs_item, trash_virtual_fs_item,
        virtual_fs_ancestor_container_identifiers, virtual_fs_children,
        virtual_fs_children_with_content_root, virtual_fs_content_path, virtual_fs_item,
        virtual_fs_item_with_content_root,
    };

    #[test]
    fn virtual_fs_root_child_uses_mount_point_name_and_identifier() {
        let mut store = InMemoryStateStore::new();
        let mount_id = MountId::new("notion-main");
        store
            .save_mount(
                MountConfig::new(mount_id.clone(), "notion", "/tmp/Locality/notion-main")
                    .projection(ProjectionMode::LinuxFuse),
            )
            .expect("save mount");

        let report = virtual_fs_children(&store, &mount_id, ROOT_CONTAINER_IDENTIFIER)
            .expect("root children");

        assert_eq!(report.children.len(), 1);
        assert_eq!(report.children[0].filename, "notion-main");
        assert_eq!(report.children[0].identifier, "mount:notion-main");
        assert_eq!(
            report.children[0].parent_identifier.as_deref(),
            Some(ROOT_CONTAINER_IDENTIFIER)
        );
    }

    #[test]
    fn mount_point_root_lists_guidance_files() {
        let mut store = InMemoryStateStore::new();
        let mount_id = MountId::new("notion-main");
        let mount = MountConfig::new(mount_id.clone(), "notion", "/tmp/Locality/notion-main")
            .projection(ProjectionMode::LinuxFuse);
        store.save_mount(mount.clone()).expect("save mount");

        let report = virtual_fs_children(&store, &mount_id, &mount_point_identifier(&mount))
            .expect("mount point children");

        assert!(
            report
                .children
                .iter()
                .any(|item| item.filename == "AGENTS.md")
        );
        assert!(
            report
                .children
                .iter()
                .any(|item| item.filename == "CLAUDE.md")
        );
        assert!(
            report
                .children
                .iter()
                .all(|item| item.parent_identifier.as_deref() == Some("mount:notion-main"))
        );
    }

    #[test]
    fn mount_point_identifier_is_not_a_materializable_entity_identifier() {
        assert!(matches!(
            super::entity_identifier("mount:notion-main"),
            Err(LocalityError::InvalidState(_))
        ));
    }

    #[test]
    fn children_include_page_directories_with_page_body_files_inside() {
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
                path: "Home/page.md".into(),
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
                path: "Home/Child/page.md".into(),
                hydration: HydrationState::Stub,
                content_hash: None,
                remote_edited_at: None,
            })
            .expect("save child page");

        let root = virtual_fs_children(&store, &mount_id, ROOT_CONTAINER_IDENTIFIER)
            .expect("root children");
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].filename, "notion");
        assert_eq!(root.children[0].kind, VirtualFsItemKind::Folder);
        assert_eq!(root.children[0].identifier, "mount:notion-main");

        let report = virtual_fs_children(&store, &mount_id, "mount:notion-main")
            .expect("mount point children");

        assert_eq!(report.children.len(), 3);
        assert_eq!(report.children[0].filename, "AGENTS.md");
        assert_eq!(report.children[0].kind, VirtualFsItemKind::File);
        assert_eq!(report.children[0].identifier, AGENTS_GUIDANCE_IDENTIFIER);
        assert_eq!(
            report.children[0].parent_identifier.as_deref(),
            Some("mount:notion-main")
        );
        assert_eq!(report.children[1].filename, "CLAUDE.md");
        assert_eq!(report.children[1].kind, VirtualFsItemKind::File);
        assert_eq!(report.children[1].identifier, CLAUDE_GUIDANCE_IDENTIFIER);
        assert_eq!(
            report.children[1].parent_identifier.as_deref(),
            Some("mount:notion-main")
        );
        assert_eq!(report.children[2].filename, "Home");
        assert_eq!(report.children[2].kind, VirtualFsItemKind::Folder);
        assert_eq!(report.children[2].identifier, "children:page-root");
    }

    #[test]
    fn mount_point_guidance_files_materialize_read_only_agent_instructions() {
        let mount_id = MountId::new("notion-main");
        let state_root = temp_root("loc-virtual-fs-guidance");
        let content_root = state_root.join("content/notion-main/files");
        let mut store = InMemoryStateStore::new();
        let mount = virtual_mount(&mount_id);
        store.save_mount(mount.clone()).expect("save mount");

        let listed = virtual_fs_children_with_content_root(
            &store,
            &content_root,
            &mount_id,
            &mount_point_identifier(&mount),
        )
        .expect("list mount point root");
        let agents = listed
            .children
            .iter()
            .find(|child| child.identifier == AGENTS_GUIDANCE_IDENTIFIER)
            .expect("agents guidance");
        assert_eq!(agents.filename, "AGENTS.md");
        assert_eq!(
            agents.parent_identifier.as_deref(),
            Some("mount:notion-main")
        );
        assert_eq!(agents.kind, VirtualFsItemKind::File);
        assert_eq!(agents.entity_kind, None);
        assert_eq!(agents.hydration, Some(HydrationState::Stub));
        assert!(agents.byte_size.unwrap_or_default() > 0);

        let report = materialize_virtual_fs_item_with_content_root(
            &mut store,
            &FailingHydrationSource,
            &content_root,
            &mount_id,
            AGENTS_GUIDANCE_IDENTIFIER,
        )
        .expect("materialize guidance");

        assert_eq!(report.hydration, HydrationState::Hydrated);
        assert_eq!(report.outcome, VirtualFsMaterializeOutcome::Hydrated);
        assert_eq!(
            PathBuf::from(&report.path),
            content_root.join(".loc-guidance").join("AGENTS.md")
        );
        let contents = std::fs::read_to_string(&report.path).expect("read guidance");
        assert!(contents.contains("# Locality Notion Mount"));
        assert!(contents.contains("loc status"));
        assert!(contents.contains("loc push"));

        let materialized = virtual_fs_item_with_content_root(
            &store,
            &content_root,
            &mount_id,
            AGENTS_GUIDANCE_IDENTIFIER,
        )
        .expect("materialized item");
        assert_eq!(materialized.item.hydration, Some(HydrationState::Hydrated));
        assert_eq!(
            materialized.item.materialized_path.as_deref(),
            Some(report.path.as_str())
        );

        assert!(matches!(
            commit_virtual_fs_write(
                &mut store,
                &content_root,
                &mount_id,
                AGENTS_GUIDANCE_IDENTIFIER,
                b"edited",
            )
            .expect_err("guidance writes are rejected"),
            LocalityError::Unsupported(
                "agent guidance files are read-only in virtual filesystem mounts"
            )
        ));

        let _ = std::fs::remove_dir_all(state_root);
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
                "Home/page.md",
            ))
            .expect("save root page");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("page-child"),
                EntityKind::Page,
                "Child",
                "Home/Child/page.md",
            ))
            .expect("save child page");

        let report =
            virtual_fs_children(&store, &mount_id, "children:page-root").expect("children");

        assert_eq!(report.children.len(), 2);
        assert_eq!(report.children[0].identifier, "children:page-child");
        assert_eq!(report.children[0].kind, VirtualFsItemKind::Folder);
        assert_eq!(report.children[1].identifier, "page-root");
        assert_eq!(report.children[1].filename, "page.md");
        assert_eq!(
            report.children[1].parent_identifier.as_deref(),
            Some("children:page-root")
        );
    }

    #[test]
    fn ancestor_container_identifiers_follow_virtual_page_path() {
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
                "Home/page.md",
            ))
            .expect("save root page");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("page-child"),
                EntityKind::Page,
                "Child",
                "Home/Child/page.md",
            ))
            .expect("save child page");

        let identifiers = virtual_fs_ancestor_container_identifiers(
            &store,
            &mount_id,
            &RemoteId::new("page-child"),
        )
        .expect("ancestor identifiers");

        assert_eq!(
            identifiers,
            vec![
                "root".to_string(),
                "mount:notion-main".to_string(),
                "children:page-root".to_string(),
                "children:page-child".to_string(),
            ]
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
                path: "home/page.md".into(),
                hydration: HydrationState::Stub,
                content_hash: None,
                remote_edited_at: None,
                stub_frontmatter: None,
            }],
            expected_parent_path: PathBuf::new(),
        };

        let saved =
            refresh_virtual_fs_children(&mut store, &connector, &mount_id, "mount:notion-main")
                .expect("refresh children");
        assert_eq!(saved, 1);

        let report = virtual_fs_children(&store, &mount_id, "mount:notion-main")
            .expect("mount point children");
        assert_eq!(report.children.len(), 3);
        assert_eq!(report.children[0].identifier, AGENTS_GUIDANCE_IDENTIFIER);
        assert_eq!(report.children[0].kind, VirtualFsItemKind::File);
        assert_eq!(report.children[1].identifier, CLAUDE_GUIDANCE_IDENTIFIER);
        assert_eq!(report.children[1].kind, VirtualFsItemKind::File);
        assert_eq!(report.children[2].identifier, "children:page-root");
        assert_eq!(report.children[2].kind, VirtualFsItemKind::Folder);

        let saved =
            refresh_virtual_fs_children(&mut store, &connector, &mount_id, "mount:notion-main")
                .expect("refresh cached children");
        assert_eq!(saved, 0);
    }

    #[test]
    fn refresh_database_children_moves_existing_row_into_database_directory() {
        let mount_id = MountId::new("notion-main");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount(&mount_id))
            .expect("save mount");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("database-1"),
                EntityKind::Database,
                "Tasks",
                "Root/Tasks",
            ))
            .expect("save database");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("row-1"),
                EntityKind::Page,
                "Fix login bug",
                "fix-login-bug.md",
            ))
            .expect("save root-level row");
        let connector = StaticChildrenConnector {
            entries: vec![TreeEntry {
                mount_id: mount_id.clone(),
                remote_id: RemoteId::new("row-1"),
                kind: EntityKind::Page,
                title: "Fix login bug".to_string(),
                path: "Root/Tasks/fix-login-bug/page.md".into(),
                hydration: HydrationState::Stub,
                content_hash: None,
                remote_edited_at: None,
                stub_frontmatter: None,
            }],
            expected_parent_path: PathBuf::from("Root/Tasks"),
        };

        let saved = refresh_virtual_fs_children(&mut store, &connector, &mount_id, "database-1")
            .expect("refresh database rows");

        assert_eq!(saved, 1);
        let row = store
            .get_entity(&mount_id, &RemoteId::new("row-1"))
            .expect("get row")
            .expect("row");
        assert_eq!(row.path, PathBuf::from("Root/Tasks/fix-login-bug/page.md"));
        let children = virtual_fs_children(&store, &mount_id, "database-1").expect("children");
        assert!(
            children
                .children
                .iter()
                .any(|child| child.identifier == "children:row-1"
                    && child.path == "Root/Tasks/fix-login-bug")
        );
    }

    #[test]
    fn refresh_database_children_keeps_dirty_existing_row_at_cached_path() {
        let mount_id = MountId::new("notion-main");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount(&mount_id))
            .expect("save mount");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("database-1"),
                EntityKind::Database,
                "Tasks",
                "Root/Tasks",
            ))
            .expect("save database");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    RemoteId::new("row-1"),
                    EntityKind::Page,
                    "Fix login bug",
                    "fix-login-bug.md",
                )
                .with_hydration(HydrationState::Dirty),
            )
            .expect("save dirty root-level row");
        let connector = StaticChildrenConnector {
            entries: vec![TreeEntry {
                mount_id: mount_id.clone(),
                remote_id: RemoteId::new("row-1"),
                kind: EntityKind::Page,
                title: "Fix login bug".to_string(),
                path: "Root/Tasks/fix-login-bug/page.md".into(),
                hydration: HydrationState::Stub,
                content_hash: None,
                remote_edited_at: None,
                stub_frontmatter: None,
            }],
            expected_parent_path: PathBuf::from("Root/Tasks"),
        };

        let saved = refresh_virtual_fs_children(&mut store, &connector, &mount_id, "database-1")
            .expect("refresh database rows");

        assert_eq!(saved, 1);
        let row = store
            .get_entity(&mount_id, &RemoteId::new("row-1"))
            .expect("get row")
            .expect("row");
        assert_eq!(row.path, PathBuf::from("fix-login-bug.md"));
        assert_eq!(row.hydration, HydrationState::Dirty);
    }

    #[test]
    fn refresh_database_children_demotes_clean_moved_row_to_stub() {
        let mount_id = MountId::new("notion-main");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount(&mount_id))
            .expect("save mount");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("database-1"),
                EntityKind::Database,
                "Tasks",
                "Root/Tasks",
            ))
            .expect("save database");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    RemoteId::new("row-1"),
                    EntityKind::Page,
                    "Fix login bug",
                    "fix-login-bug.md",
                )
                .with_hydration(HydrationState::Hydrated)
                .with_content_hash("old-cache-hash"),
            )
            .expect("save hydrated root-level row");
        let connector = StaticChildrenConnector {
            entries: vec![TreeEntry {
                mount_id: mount_id.clone(),
                remote_id: RemoteId::new("row-1"),
                kind: EntityKind::Page,
                title: "Fix login bug".to_string(),
                path: "Root/Tasks/fix-login-bug/page.md".into(),
                hydration: HydrationState::Stub,
                content_hash: None,
                remote_edited_at: None,
                stub_frontmatter: None,
            }],
            expected_parent_path: PathBuf::from("Root/Tasks"),
        };

        let saved = refresh_virtual_fs_children(&mut store, &connector, &mount_id, "database-1")
            .expect("refresh database rows");

        assert_eq!(saved, 1);
        let row = store
            .get_entity(&mount_id, &RemoteId::new("row-1"))
            .expect("get row")
            .expect("row");
        assert_eq!(row.path, PathBuf::from("Root/Tasks/fix-login-bug/page.md"));
        assert_eq!(row.hydration, HydrationState::Stub);
        assert_eq!(row.content_hash, None);
    }

    struct StaticChildrenConnector {
        entries: Vec<TreeEntry>,
        expected_parent_path: PathBuf,
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
                supports_lazy_child_enumeration: true,
                ..ConnectorCapabilities::default()
            }
        }

        fn enumerate(
            &self,
            _request: EnumerateRequest,
        ) -> locality_core::LocalityResult<Vec<TreeEntry>> {
            Ok(self.entries.clone())
        }

        fn list_children(
            &self,
            request: ListChildrenRequest,
        ) -> locality_core::LocalityResult<ListChildrenResult> {
            assert_eq!(request.parent_path, self.expected_parent_path);
            Ok(ListChildrenResult {
                entries: self.entries.clone(),
            })
        }

        fn fetch(&self, request: FetchRequest) -> locality_core::LocalityResult<NativeEntity> {
            let _ = request;
            Err(locality_core::LocalityError::NotImplemented(
                "fixture fetch",
            ))
        }

        fn render(
            &self,
            _entity: &NativeEntity,
        ) -> locality_core::LocalityResult<CanonicalDocument> {
            Err(locality_core::LocalityError::NotImplemented(
                "fixture render",
            ))
        }

        fn parse(
            &self,
            _document: &CanonicalDocument,
        ) -> locality_core::LocalityResult<ParsedEntity> {
            Err(locality_core::LocalityError::NotImplemented(
                "fixture parse",
            ))
        }

        fn check_concurrency(
            &self,
            _request: ApplyPlanRequest<'_>,
        ) -> locality_core::LocalityResult<()> {
            Ok(())
        }

        fn apply(
            &self,
            _request: ApplyPlanRequest<'_>,
        ) -> locality_core::LocalityResult<ApplyPlanResult> {
            Err(locality_core::LocalityError::NotImplemented(
                "fixture apply",
            ))
        }

        fn apply_undo(
            &self,
            _request: ApplyUndoRequest<'_>,
        ) -> locality_core::LocalityResult<ApplyUndoResult> {
            Err(locality_core::LocalityError::NotImplemented("fixture undo"))
        }
    }

    struct FailingHydrationSource;

    impl HydrationSource for FailingHydrationSource {
        fn fetch_render(
            &self,
            _request: &HydrationRequest,
        ) -> locality_core::LocalityResult<HydratedEntity> {
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
                "Roadmap/page.md",
            ))
            .expect("save page");

        let report = virtual_fs_item(&store, &mount_id, "page-1").expect("item");

        assert_eq!(report.item.filename, "page.md");
        assert_eq!(report.item.kind, VirtualFsItemKind::File);
        assert_eq!(report.item.materialized_path, None);
    }

    #[test]
    fn conflicted_virtual_file_materializes_from_existing_cache_without_fetch() {
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let state_root = temp_root("loc-virtual-fs-conflicted-materialize");
        let content_root = state_root.join("content/notion-main/files");
        let conflicted_contents = b"<<<<<<< LOCAL\nlocal\n=======\nremote\n>>>>>>> REMOTE\n";
        std::fs::create_dir_all(content_root.join("Roadmap")).expect("content root");
        std::fs::write(content_root.join("Roadmap/page.md"), conflicted_contents)
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
                    "Roadmap/page.md",
                )
                .with_hydration(HydrationState::Conflicted),
            )
            .expect("save page");

        let item = virtual_fs_item_with_content_root(&store, &content_root, &mount_id, "page-1")
            .expect("item");
        let expected_path = content_root
            .join("Roadmap/page.md")
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
        let state_root = std::path::Path::new("/tmp/loc-state");

        let path =
            virtual_fs_content_path(state_root, &mount_id, std::path::Path::new("Roadmap.md"))
                .expect("content path");
        assert_eq!(
            path,
            std::path::Path::new("/tmp/loc-state/content/notion-main/files/Roadmap.md")
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
        let state_root = temp_root("loc-virtual-fs-commit");
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
                path: "Roadmap/page.md".into(),
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
            std::fs::read(content_root.join("Roadmap/page.md")).expect("read cache"),
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
        let freshness = store
            .get_freshness_state(&mount_id, &remote_id)
            .expect("get freshness")
            .expect("freshness");
        assert_eq!(freshness.tier, FreshnessTier::Hot);
        assert!(freshness.last_local_change_at.is_some());
        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn commit_write_marks_stub_entity_dirty() {
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let state_root = temp_root("loc-virtual-fs-commit-stub");
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
                    "Roadmap/page.md",
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
        let state_root = temp_root("loc-virtual-fs-commit-stub-conflict");
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
                    "Roadmap/page.md",
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
        let state_root = temp_root("loc-virtual-fs-create");
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
                "Home/page.md",
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
    fn create_directory_adds_pending_page_folder_with_page_document() {
        let mount_id = MountId::new("notion-main");
        let state_root = temp_root("loc-virtual-fs-create-dir");
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
                "Home/page.md",
            ))
            .expect("save parent page");

        let created = create_virtual_fs_directory(
            &mut store,
            &content_root,
            &mount_id,
            "children:page-root",
            "Draft",
        )
        .expect("create virtual directory");

        assert!(created.identifier.starts_with("children:local:"));
        assert_eq!(created.item.filename, "Draft");
        assert_eq!(created.item.kind, VirtualFsItemKind::Folder);
        assert_eq!(created.item.path, "Home/Draft");
        assert_eq!(
            std::fs::read(content_root.join("Home/Draft/page.md")).expect("read pending cache"),
            b""
        );

        let parent_children =
            virtual_fs_children(&store, &mount_id, "children:page-root").expect("parent children");
        assert!(parent_children.children.iter().any(|child| {
            child.identifier == created.identifier
                && child.kind == VirtualFsItemKind::Folder
                && child.path == "Home/Draft"
        }));

        let page_children =
            virtual_fs_children(&store, &mount_id, &created.identifier).expect("page children");
        let page = page_children
            .children
            .iter()
            .find(|child| child.filename == "page.md")
            .expect("pending page.md");
        assert!(page.identifier.starts_with("local:"));
        assert_eq!(page.path, "Home/Draft/page.md");
        assert_eq!(
            page.parent_identifier.as_deref(),
            Some(created.identifier.as_str())
        );

        let reused = create_virtual_fs_file(
            &mut store,
            &content_root,
            &mount_id,
            &created.identifier,
            "page.md",
        )
        .expect("create page.md in pending directory");
        assert_eq!(reused.identifier, page.identifier);
        assert_eq!(
            store
                .list_virtual_mutations(&mount_id)
                .expect("list mutations")
                .len(),
            1
        );
        let temp = create_virtual_fs_file(
            &mut store,
            &content_root,
            &mount_id,
            &created.identifier,
            "page.md.tmp.1234.abcd",
        )
        .expect("create atomic temp for page.md");
        assert_eq!(temp.identifier, page.identifier);
        assert_eq!(temp.item.filename, "page.md.tmp.1234.abcd");
        assert_eq!(temp.item.path, "Home/Draft/page.md.tmp.1234.abcd");
        assert_eq!(
            store
                .list_virtual_mutations(&mount_id)
                .expect("list mutations")
                .len(),
            1
        );

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn create_directory_under_google_docs_source_root_uses_workspace_folder_parent() {
        let mount_id = MountId::new("google-docs-main");
        let state_root = temp_root("loc-virtual-fs-google-docs-root-create-dir");
        let content_root = state_root.join("content/google-docs-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(
                MountConfig::new(mount_id.clone(), "google-docs", "/tmp/loc/google-docs")
                    .with_remote_root_id(RemoteId::new("workspace-folder"))
                    .projection(ProjectionMode::LinuxFuse),
            )
            .expect("save mount");

        let created = create_virtual_fs_directory(
            &mut store,
            &content_root,
            &mount_id,
            "mount:google-docs-main",
            "Scratch Hydration",
        )
        .expect("create google docs directory under mount point root");

        assert!(created.identifier.starts_with("children:local:"));
        assert_eq!(created.item.filename, "Scratch Hydration");
        assert_eq!(created.item.path, "Scratch Hydration");
        assert_eq!(
            std::fs::read(content_root.join("Scratch Hydration/page.md"))
                .expect("read pending page cache"),
            b""
        );
        let mutation = store
            .list_virtual_mutations(&mount_id)
            .expect("list mutations")
            .pop()
            .expect("pending create");
        assert_eq!(
            mutation.parent_remote_id.as_ref().map(|id| id.as_str()),
            Some("workspace-folder")
        );

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn rename_pending_file_updates_overlay_and_moves_cache() {
        let mount_id = MountId::new("notion-main");
        let state_root = temp_root("loc-virtual-fs-rename");
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
                "Home/page.md",
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
    fn rename_pending_page_directory_updates_overlay_and_moves_page_cache() {
        let mount_id = MountId::new("notion-main");
        let state_root = temp_root("loc-virtual-fs-rename-dir");
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
                "Home/page.md",
            ))
            .expect("save parent page");
        let created = create_virtual_fs_directory(
            &mut store,
            &content_root,
            &mount_id,
            "children:page-root",
            "Draft",
        )
        .expect("create virtual directory");
        std::fs::write(
            content_root.join("Home/Draft/page.md"),
            b"---\ntitle: \"Draft\"\n---\nBody",
        )
        .expect("write pending page cache");

        let renamed = rename_virtual_fs_item(
            &mut store,
            &content_root,
            &mount_id,
            &created.identifier,
            "children:page-root",
            "Published",
        )
        .expect("rename virtual page directory");

        assert_eq!(renamed.identifier, created.identifier);
        assert_eq!(renamed.item.kind, VirtualFsItemKind::Folder);
        assert_eq!(renamed.item.filename, "Published");
        assert_eq!(renamed.item.path, "Home/Published");
        assert!(!content_root.join("Home/Draft/page.md").exists());
        let renamed_cache = std::fs::read_to_string(content_root.join("Home/Published/page.md"))
            .expect("read renamed page cache");
        assert!(renamed_cache.contains("title: \"Published\""));
        let local_id = created
            .identifier
            .strip_prefix("children:")
            .expect("pending page directory id");
        let mutation = store
            .get_virtual_mutation(&mount_id, local_id)
            .expect("get mutation")
            .expect("mutation");
        assert_eq!(
            mutation.projected_path,
            PathBuf::from("Home/Published/page.md")
        );
        assert_eq!(mutation.title, "Published");

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn rename_existing_page_directory_records_pending_remote_rename_and_retitles_cache() {
        let mount_id = MountId::new("notion-main");
        let state_root = temp_root("loc-virtual-fs-rename-existing-dir");
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
                "Home/page.md",
            ))
            .expect("save parent page");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    RemoteId::new("page-child"),
                    EntityKind::Page,
                    "Child",
                    "Home/Child/page.md",
                )
                .with_hydration(HydrationState::Hydrated),
            )
            .expect("save child page");
        std::fs::create_dir_all(content_root.join("Home/Child")).expect("create cache dir");
        std::fs::write(
            content_root.join("Home/Child/page.md"),
            b"---\nloc:\n  id: page-child\n  type: page\ntitle: \"Child\"\n---\nBody",
        )
        .expect("write page cache");

        let renamed = rename_virtual_fs_item(
            &mut store,
            &content_root,
            &mount_id,
            "children:page-child",
            "children:page-root",
            "Renamed Child",
        )
        .expect("rename existing virtual page directory");

        assert_eq!(renamed.identifier, "children:page-child");
        assert_eq!(renamed.item.kind, VirtualFsItemKind::Folder);
        assert_eq!(renamed.item.filename, "Renamed Child");
        assert_eq!(renamed.item.path, "Home/Renamed Child");
        assert!(!content_root.join("Home/Child/page.md").exists());
        let renamed_cache =
            std::fs::read_to_string(content_root.join("Home/Renamed Child/page.md"))
                .expect("read renamed cache");
        assert!(renamed_cache.contains("title: \"Renamed Child\""));
        let entity = store
            .get_entity(&mount_id, &RemoteId::new("page-child"))
            .expect("get child entity")
            .expect("child entity");
        assert_eq!(entity.path, PathBuf::from("Home/Renamed Child/page.md"));
        assert_eq!(entity.title, "Renamed Child");
        assert_eq!(entity.hydration, HydrationState::Dirty);
        let mutation = store
            .get_virtual_mutation(&mount_id, "rename:page-child")
            .expect("get rename mutation")
            .expect("rename mutation");
        assert_eq!(mutation.mutation_kind, VirtualMutationKind::Rename);
        assert_eq!(
            mutation.projected_path,
            PathBuf::from("Home/Renamed Child/page.md")
        );
        assert_eq!(mutation.title, "Renamed Child");

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn trash_remote_page_records_tombstone_and_hides_child() {
        let mount_id = MountId::new("notion-main");
        let state_root = temp_root("loc-virtual-fs-trash");
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
                "Roadmap/page.md",
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
    fn trash_remote_page_directory_records_tombstone_and_hides_child() {
        let mount_id = MountId::new("notion-main");
        let state_root = temp_root("loc-virtual-fs-trash-page-dir");
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
                "Roadmap/page.md",
            ))
            .expect("save page");

        let trashed =
            trash_virtual_fs_item(&mut store, &content_root, &mount_id, "children:page-1")
                .expect("trash virtual page directory");

        assert_eq!(trashed.identifier, "children:page-1");
        assert_eq!(trashed.item.kind, VirtualFsItemKind::Folder);
        let children =
            virtual_fs_children(&store, &mount_id, ROOT_CONTAINER_IDENTIFIER).expect("children");
        assert!(
            !children
                .children
                .iter()
                .any(|child| child.identifier == "children:page-1")
        );
        let mutation = store
            .get_virtual_mutation(&mount_id, "delete:page-1")
            .expect("get mutation")
            .expect("mutation");
        assert_eq!(mutation.mutation_kind, VirtualMutationKind::Delete);

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn trash_pending_page_directory_discards_overlay_and_cache() {
        let mount_id = MountId::new("notion-main");
        let state_root = temp_root("loc-virtual-fs-trash-pending-dir");
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
                "Home/page.md",
            ))
            .expect("save parent page");
        let created = create_virtual_fs_directory(
            &mut store,
            &content_root,
            &mount_id,
            "children:page-root",
            "Draft",
        )
        .expect("create virtual directory");
        let cache_path = content_root.join("Home/Draft/page.md");
        std::fs::write(&cache_path, b"pending body").expect("write pending cache");

        let trashed =
            trash_virtual_fs_item(&mut store, &content_root, &mount_id, &created.identifier)
                .expect("trash pending virtual page directory");

        assert_eq!(trashed.identifier, created.identifier);
        assert_eq!(trashed.item.kind, VirtualFsItemKind::Folder);
        assert!(!cache_path.exists());
        assert!(
            store
                .list_virtual_mutations(&mount_id)
                .expect("list mutations")
                .is_empty()
        );

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn plain_files_mount_rejects_virtual_filesystem_operations() {
        let mount_id = MountId::new("notion-main");
        let state_root = temp_root("loc-virtual-fs-plain-rejects");
        let content_root = state_root.join("content/notion-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(MountConfig::new(
                mount_id.clone(),
                "notion",
                "/tmp/loc/notion",
            ))
            .expect("save plain mount");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("page-1"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap/page.md",
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
                expected_parent_path: PathBuf::new(),
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

    fn assert_plain_files_virtual_error<T: std::fmt::Debug>(
        result: locality_core::LocalityResult<T>,
    ) {
        assert!(matches!(
            result.expect_err("plain-files virtual filesystem rejection"),
            LocalityError::Unsupported(
                "plain-files mounts do not support virtual filesystem operations"
            )
        ));
    }

    fn virtual_mount(mount_id: &MountId) -> MountConfig {
        MountConfig::new(mount_id.clone(), "notion", "/tmp/loc/notion")
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
