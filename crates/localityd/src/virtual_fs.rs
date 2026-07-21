use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
#[cfg(target_os = "macos")]
use std::{io::Write, sync::Mutex, sync::OnceLock};

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
    EntityRecord, EntityRepository, FreshnessStateRecord, FreshnessStateRepository, MountConfig,
    MountRepository, ProjectionMode, ShadowRepository, StoreError, VirtualMoveRepository,
    VirtualMoveTransition, VirtualMutationKind, VirtualMutationRecord, VirtualMutationRepository,
};
use serde::{Deserialize, Serialize};

use crate::durable_fs::{
    create_dir_all_durable, remove_path_durable, rename_noreplace_durable, write_new_file_durable,
};
use crate::hydration::{
    HydrationExecutor, HydrationOutcome, HydrationSource, write_parent_database_schema_cache,
};
use crate::shadow_match::parsed_matches_shadow;
use crate::source::{
    VirtualRenamePolicy, source_create_decision_for_parent_path, source_descriptor,
    source_move_decision_for_parent_path, source_write_decision_for_path,
};

pub const ROOT_CONTAINER_IDENTIFIER: &str = "root";
pub const SOURCE_ROOT_PREFIX: &str = "source:";
pub const MOUNT_POINT_PREFIX: &str = "mount:";
const CHILDREN_PREFIX: &str = "children:";
const PATH_PREFIX: &str = "path:";
const LOCAL_PREFIX: &str = "local:";
const SCHEMA_PREFIX: &str = "schema:";
const ASSET_CACHE_PREFIX: &str = "asset-cache:";
const GUIDANCE_PREFIX: &str = "guidance:";
const LOC_CACHE_ROOT: &str = ".loc";
const AGENTS_FILE: &str = "AGENTS.md";
const CLAUDE_FILE: &str = "CLAUDE.md";
const AGENTS_GUIDANCE_IDENTIFIER: &str = "guidance:AGENTS.md";
const CLAUDE_GUIDANCE_IDENTIFIER: &str = "guidance:CLAUDE.md";

#[cfg(target_os = "macos")]
static LEGACY_MACOS_CONTENT_REPAIRS: OnceLock<Mutex<BTreeMap<PathBuf, LocalityResult<()>>>> =
    OnceLock::new();

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

pub fn validate_virtual_projection_root(mount: &MountConfig) -> LocalityResult<()> {
    if !matches!(
        mount.projection,
        ProjectionMode::LinuxFuse | ProjectionMode::WindowsCloudFiles
    ) {
        return Ok(());
    }

    let projection_root = virtual_projection_root(mount);
    if let Some(reason) = unsafe_virtual_projection_root_reason(&projection_root) {
        return Err(LocalityError::InvalidState(format!(
            "unsafe virtual projection root `{}` for mount `{}` at `{}`: {reason}; choose an explicit shared directory such as `~/Locality/{}` instead",
            projection_root.display(),
            mount.mount_id.0,
            mount.root.display(),
            mount_point_directory_name(mount)
        )));
    }

    Ok(())
}

pub fn virtual_projection_mount_point(mount: &MountConfig) -> PathBuf {
    virtual_projection_root(mount).join(mount_point_directory_name(mount))
}

fn unsafe_virtual_projection_root_reason(root: &Path) -> Option<&'static str> {
    if root.as_os_str().is_empty() {
        return Some("the shared provider root is empty");
    }
    if root.parent().is_none() {
        return Some("the shared provider root is a filesystem root");
    }
    if home_directories()
        .iter()
        .any(|home| paths_equal_for_platform(root, home))
    {
        return Some("the shared provider root is the user home directory");
    }
    if home_directories()
        .iter()
        .filter_map(|home| home.parent())
        .any(|home_parent| paths_equal_for_platform(root, home_parent))
    {
        return Some("the shared provider root is the user home container directory");
    }
    None
}

fn home_directories() -> Vec<PathBuf> {
    let mut homes = Vec::new();
    for key in ["HOME", "USERPROFILE"] {
        if let Some(home) = std::env::var_os(key).filter(|value| !value.is_empty()) {
            homes.push(PathBuf::from(home));
        }
    }
    if let (Some(drive), Some(path)) = (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH"))
        && !drive.is_empty()
        && !path.is_empty()
    {
        homes.push(PathBuf::from(format!(
            "{}{}",
            drive.to_string_lossy(),
            path.to_string_lossy()
        )));
    }
    homes
}

#[cfg(windows)]
fn paths_equal_for_platform(left: &Path, right: &Path) -> bool {
    path_key(left) == path_key(right)
}

#[cfg(not(windows))]
fn paths_equal_for_platform(left: &Path, right: &Path) -> bool {
    left == right
}

#[cfg(windows)]
fn path_key(path: &Path) -> String {
    let mut value = path.display().to_string().replace('/', "\\");
    while value.ends_with('\\') && value.len() > 3 {
        value.pop();
    }
    value.to_ascii_lowercase()
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
        EntityKind::Page if page_projects_as_directory(&entity.path) => {
            page_container_path(&entity.path)
        }
        EntityKind::Database | EntityKind::Directory => entity.path.clone(),
        EntityKind::Asset | EntityKind::Unknown(_) => parent_path(&entity.path).to_path_buf(),
        EntityKind::Page => parent_path(&entity.path).to_path_buf(),
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VirtualFsRefreshChildrenReport {
    pub saved: usize,
    pub changed: bool,
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
    #[serde(default)]
    pub read_only: bool,
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
    let mount = require_virtual_mount(store, mount_id)?;
    repair_legacy_macos_content_root_for_current_root(content_root, mount_id)?;
    if let Some(item) = loc_asset_cache_item_for_identifier(&mount, content_root, identifier)? {
        return Ok(VirtualFsItemReport {
            mount_id: mount_id.0.clone(),
            item,
        });
    }

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
            if entity.kind == EntityKind::Page && page_projects_as_directory(&entity.path) {
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
        if entity.kind == EntityKind::Page
            && page_projects_as_directory(&entity.path)
            && page_container_path(&entity.path) == container_path
        {
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
            && pending_created_directory_path(mutation) == Some(container_path.clone())
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
    repair_legacy_macos_content_root_for_current_root(content_root, mount_id)?;
    let mut report = virtual_fs_children(store, mount_id, container_identifier)?;
    let mount = require_mount(store, mount_id)?;
    let entities = store.list_entities(mount_id).map_err(LocalityError::from)?;
    let mutations = store
        .list_virtual_mutations(mount_id)
        .map_err(LocalityError::from)?;
    if let Some(schema) =
        schema_item_for_container(&mount, &entities, content_root, container_identifier)?
        && !report
            .children
            .iter()
            .any(|child| child.identifier == schema.identifier)
    {
        report.children.push(schema);
    }
    let container_path = container_path(&mount, &entities, &mutations, container_identifier)?;
    for child in loc_asset_cache_children(&mount, content_root, &container_path)? {
        if !report
            .children
            .iter()
            .any(|existing| existing.identifier == child.identifier)
        {
            report.children.push(child);
        }
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
) -> LocalityResult<VirtualFsRefreshChildrenReport>
where
    S: MountRepository + EntityRepository,
    C: Connector + ?Sized,
{
    let mount = require_virtual_mount(store, mount_id)?;
    let entities = store.list_entities(mount_id).map_err(LocalityError::from)?;
    let parent_path = container_path(&mount, &entities, &[], container_identifier)?;

    let Some(container) = child_container_for_identifier(&mount, &entities, container_identifier)?
    else {
        return Ok(VirtualFsRefreshChildrenReport::default());
    };

    let result = connector.list_children(ListChildrenRequest {
        mount_id: mount.mount_id.clone(),
        container,
        parent_path: parent_path.clone(),
    })?;
    let pruned = if result.is_complete() {
        let returned_remote_ids = result
            .entries
            .iter()
            .map(|entry| entry.remote_id.clone())
            .collect::<BTreeSet<_>>();
        prune_stale_virtual_children(store, mount_id, &parent_path, &returned_remote_ids)
            .map_err(LocalityError::from)?
    } else {
        0
    };

    let mut saved = 0;
    let mut changed = pruned > 0;
    for entry in result.entries {
        let existing = store
            .get_entity(&entry.mount_id, &entry.remote_id)
            .map_err(LocalityError::from)?;
        let record = refreshed_entity_record(entry, existing.as_ref());
        if existing.as_ref() != Some(&record) {
            changed = true;
        }
        store.save_entity(record).map_err(LocalityError::from)?;
        saved += 1;
    }

    Ok(VirtualFsRefreshChildrenReport { saved, changed })
}

pub(crate) fn prune_stale_virtual_children<S>(
    store: &mut S,
    mount_id: &MountId,
    parent_path: &Path,
    returned_remote_ids: &BTreeSet<RemoteId>,
) -> Result<usize, StoreError>
where
    S: EntityRepository,
{
    let entities = store.list_entities(mount_id)?;
    let mut delete_ids = BTreeSet::new();
    for entity in entities.iter().filter(|entity| {
        entity_listing_parent_path(entity) == parent_path
            && !returned_remote_ids.contains(&entity.remote_id)
    }) {
        let subtree = stale_virtual_child_subtree(&entities, entity);
        if subtree.iter().any(|entity| {
            matches!(
                entity.hydration,
                HydrationState::Dirty | HydrationState::Conflicted
            )
        }) {
            continue;
        }
        delete_ids.extend(subtree.into_iter().map(|entity| entity.remote_id.clone()));
    }

    let pruned = delete_ids.len();
    for remote_id in delete_ids {
        store.delete_entity(mount_id, &remote_id)?;
    }
    Ok(pruned)
}

fn stale_virtual_child_subtree<'a>(
    entities: &'a [EntityRecord],
    child: &EntityRecord,
) -> Vec<&'a EntityRecord> {
    let subtree_root = entity_subtree_root_path(child);
    entities
        .iter()
        .filter(|entity| {
            entity.remote_id == child.remote_id || entity.path.starts_with(&subtree_root)
        })
        .collect()
}

pub fn refresh_virtual_fs_children_with_content_root<S, C>(
    store: &mut S,
    connector: &C,
    content_root: &Path,
    mount_id: &MountId,
    container_identifier: &str,
) -> LocalityResult<VirtualFsRefreshChildrenReport>
where
    S: MountRepository + EntityRepository,
    C: Connector + HydrationSource + ?Sized,
{
    let _ = require_virtual_mount(store, mount_id)?;
    repair_legacy_macos_content_root_for_current_root(content_root, mount_id)?;
    let mut report = refresh_virtual_fs_children(store, connector, mount_id, container_identifier)?;
    if refresh_database_schema_cache_for_container(
        store,
        connector,
        content_root,
        mount_id,
        container_identifier,
    )? {
        report.changed = true;
    }
    Ok(report)
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
    match container_path(&mount, &entities, &[], container_identifier) {
        Ok(_) => {}
        Err(error) if is_missing_identifier_error(&error) => return Ok(false),
        Err(error) => return Err(error),
    }

    child_container_for_identifier(&mount, &entities, container_identifier)
        .map(|container| container.is_some())
}

pub fn virtual_fs_container_depth<S>(
    store: &S,
    mount_id: &MountId,
    container_identifier: &str,
) -> LocalityResult<u32>
where
    S: MountRepository + EntityRepository,
{
    let mount = require_virtual_mount(store, mount_id)?;
    let entities = store.list_entities(mount_id).map_err(LocalityError::from)?;
    let path = container_path(&mount, &entities, &[], container_identifier)?;
    Ok(u32::try_from(path.components().count()).unwrap_or(u32::MAX))
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
    if !is_hydratable_markdown_entity(&entity) {
        return Err(LocalityError::Unsupported(
            "only page.md and Markdown asset files can be materialized by the virtual filesystem",
        ));
    }

    let path = mount.root.join(&entity.path);
    let outcome = if is_materialized_hydration(&entity.hydration) && path.exists() {
        if entity.kind == EntityKind::Page {
            write_parent_database_schema_cache(store, source, &mount, &entity, &mount.root)?;
        }
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
    repair_legacy_macos_content_root_for_current_root(content_root, mount_id)?;
    if let Some(report) = materialize_guidance_item(&mount, content_root, identifier, true)? {
        return Ok(report);
    }
    if let Some(report) = materialize_loc_asset_cache_item(&mount, content_root, identifier)? {
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
            if mutation
                .content_path
                .as_ref()
                .is_some_and(|stored_path| stored_path != &path)
            {
                return Err(LocalityError::InvalidState(format!(
                    "pending create `{}` current content cache is missing at `{}`",
                    mutation.local_id,
                    path.display()
                )));
            } else {
                write_binary_atomic(&path, b"")?;
            }
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
    if !is_hydratable_markdown_entity(&entity) {
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
            "only page.md and Markdown asset files can be materialized by the virtual filesystem",
        ));
    }

    let path = content_path_for_relative(content_root, &entity.path)?;
    let outcome = if is_materialized_hydration(&entity.hydration) && path.exists() {
        if entity.kind == EntityKind::Page {
            write_parent_database_schema_cache(store, source, &mount, &entity, content_root)?;
        }
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

pub fn materialize_virtual_fs_guidance_with_content_root<S>(
    store: &S,
    content_root: &Path,
    mount_id: &MountId,
    identifier: &str,
) -> LocalityResult<Option<VirtualFsMaterializeReport>>
where
    S: MountRepository,
{
    if !is_guidance_identifier(identifier) {
        return Ok(None);
    }
    let Some(mount) = store.get_mount(mount_id).map_err(LocalityError::from)? else {
        return Ok(None);
    };
    if !mount.projection.uses_virtual_filesystem() {
        return Err(LocalityError::Unsupported(
            "plain-files mounts do not support virtual filesystem operations",
        ));
    }
    repair_legacy_macos_content_root_for_current_root(content_root, mount_id)?;
    materialize_guidance_item(&mount, content_root, identifier, true)
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
    repair_legacy_macos_content_root_for_current_root(content_root, mount_id)?;
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
    if identifier.starts_with(ASSET_CACHE_PREFIX) {
        return Err(LocalityError::Unsupported(
            "downloaded asset cache files are read-only",
        ));
    }
    if let Some(mut mutation) = local_mutation(store, mount_id, identifier)? {
        if mutation.mutation_kind != VirtualMutationKind::Create {
            return Err(LocalityError::Unsupported(
                "only pending-created local files can be written by local virtual identifier",
            ));
        }
        ensure_source_path_writable(&mount, &mutation.projected_path)?;
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
    ensure_source_path_writable(&mount, &entity.path)?;
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
    repair_legacy_macos_content_root_for_current_root(content_root, mount_id)?;
    if mount.read_only {
        return Err(LocalityError::Unsupported(
            "read-only mounts do not accept virtual filesystem creates",
        ));
    }
    let atomic_temp = is_atomic_temp_filename(filename);
    if !filename.ends_with(".md")
        && filename != "_schema.yaml"
        && !is_database_schema_atomic_temp_filename(filename)
        && !atomic_temp
    {
        return Err(LocalityError::Unsupported(
            "virtual filesystem creates currently support Markdown, database draft schemas, and atomic write temp files",
        ));
    }
    let entities = store.list_entities(mount_id).map_err(LocalityError::from)?;
    let mutations = store
        .list_virtual_mutations(mount_id)
        .map_err(LocalityError::from)?;
    if let Some(mutation) = pending_page_directory_mutation(&mutations, parent_identifier)? {
        let database_draft = is_database_draft_schema_path(&mutation.projected_path);
        let allowed = if database_draft {
            filename == "_schema.yaml" || is_database_schema_atomic_temp_filename(filename)
        } else {
            filename == locality_core::path_projection::PAGE_DOCUMENT_FILENAME
                || is_page_document_atomic_temp_filename(filename)
        };
        if !allowed {
            return Err(LocalityError::Unsupported(if database_draft {
                "pending database directories accept only _schema.yaml or its atomic temp files"
            } else {
                "pending page directories accept only page.md or its atomic temp files"
            }));
        }
        let index = ProviderIndex::new(&entities);
        let item = if filename == locality_core::path_projection::PAGE_DOCUMENT_FILENAME
            || filename == "_schema.yaml"
        {
            pending_item(&mount, mutation, &index)
        } else {
            pending_temp_item(&mount, mutation, filename)
        };
        return mutation_report_for_content_item(mount_id, content_root, item);
    }

    let parent_path = container_path(&mount, &entities, &mutations, parent_identifier)?;
    ensure_source_parent_accepts_create(&mount, &parent_path)?;
    let projected_path = parent_path.join(filename);
    if let Some(item) = existing_file_create_item(&mount, &entities, &mutations, &projected_path) {
        return Ok(mutation_report_for_existing_item(mount_id, item));
    }

    let parent_remote_id = create_parent_remote_id(&mount, &entities, parent_identifier)?;
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
    mutation_report_for_content_item(mount_id, content_root, item)
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
    repair_legacy_macos_content_root_for_current_root(content_root, mount_id)?;
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
    if pending_page_directory_mutation(&mutations, parent_identifier)?
        .is_some_and(|mutation| is_database_draft_schema_path(&mutation.projected_path))
    {
        return Err(LocalityError::Unsupported(
            "push the database draft before creating rows inside it",
        ));
    }
    let parent_container_path = container_path(&mount, &entities, &mutations, parent_identifier)?;
    ensure_source_parent_accepts_create(&mount, &parent_container_path)?;
    let page_dir = parent_container_path.join(dirname);
    let projected_path = page_document_path(&page_dir);
    ensure_source_parent_accepts_create(&mount, parent_path(&projected_path))?;
    if let Some(item) = existing_directory_create_item(&mount, &entities, &mutations, &page_dir) {
        return Ok(mutation_report_for_existing_item(mount_id, item));
    }

    let parent_remote_id = create_parent_remote_id(&mount, &entities, parent_identifier)?;
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

fn existing_file_create_item(
    mount: &MountConfig,
    entities: &[EntityRecord],
    mutations: &[VirtualMutationRecord],
    projected_path: &Path,
) -> Option<VirtualFsItem> {
    let index = ProviderIndex::new(entities);
    let deleted_remote_ids = pending_deleted_remote_ids(mutations);
    entities
        .iter()
        .find(|entity| {
            entity.path == projected_path && !deleted_remote_ids.contains(&entity.remote_id)
        })
        .map(|entity| entity_item(mount, entity, &index))
}

fn existing_directory_create_item(
    mount: &MountConfig,
    entities: &[EntityRecord],
    mutations: &[VirtualMutationRecord],
    page_dir: &Path,
) -> Option<VirtualFsItem> {
    let index = ProviderIndex::new(entities);
    let deleted_remote_ids = pending_deleted_remote_ids(mutations);
    let page_path = page_document_path(page_dir);
    entities
        .iter()
        .find(|entity| {
            !deleted_remote_ids.contains(&entity.remote_id)
                && ((entity.kind == EntityKind::Page && entity.path == page_path)
                    || (matches!(entity.kind, EntityKind::Database | EntityKind::Directory)
                        && entity.path == page_dir))
        })
        .map(|entity| {
            if entity.kind == EntityKind::Page {
                page_child_dir_item(mount, page_dir, &entity.remote_id, &index)
            } else {
                entity_item(mount, entity, &index)
            }
        })
}

fn mutation_report_for_existing_item(
    mount_id: &MountId,
    item: VirtualFsItem,
) -> VirtualFsMutationReport {
    VirtualFsMutationReport {
        mount_id: mount_id.0.clone(),
        identifier: item.identifier.clone(),
        path: item.path.clone(),
        item,
    }
}

fn mutation_report_for_content_item(
    mount_id: &MountId,
    content_root: &Path,
    mut item: VirtualFsItem,
) -> LocalityResult<VirtualFsMutationReport> {
    rewrite_item_materialized_path(content_root, &mut item)?;
    Ok(mutation_report_for_existing_item(mount_id, item))
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
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + VirtualMutationRepository
        + VirtualMoveRepository
        + FreshnessStateRepository,
{
    let mount = require_virtual_mount(store, mount_id)?;
    repair_legacy_macos_content_root_for_current_root(content_root, mount_id)?;
    if mount.read_only {
        return Err(LocalityError::Unsupported(
            "read-only mounts do not accept virtual filesystem renames",
        ));
    }
    let entities = store.list_entities(mount_id).map_err(LocalityError::from)?;
    let mutations = store
        .list_virtual_mutations(mount_id)
        .map_err(LocalityError::from)?;
    let rename_policy = source_descriptor(&mount.connector).virtual_rename_policy();
    let new_parent_path = container_path(&mount, &entities, &mutations, new_parent_identifier)?;

    if let Some(child_identifier) = identifier.strip_prefix(CHILDREN_PREFIX) {
        let new_page_dir = new_parent_path.join(new_filename);
        let new_path = page_document_path(&new_page_dir);
        let filename_title = title_from_filename(new_filename);
        ensure_source_path_writable(&mount, &new_path)?;
        let new_parent =
            move_parent_remote(&mount, &entities, new_parent_identifier, &new_parent_path)?;

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
            ensure_source_path_writable(&mount, &mutation.projected_path)?;
            ensure_virtual_page_directory_available_for_rename(
                store,
                mount_id,
                RenameOwner::Local(child_identifier),
                &new_page_dir,
                &new_path,
            )?;
            let old_projected_path = mutation.projected_path.clone();
            let old_path = mutation
                .content_path
                .clone()
                .unwrap_or(content_path_for_relative(
                    content_root,
                    &old_projected_path,
                )?);
            let new_content_path = content_path_for_relative(content_root, &new_path)?;
            ensure_pending_create_materializable(&mutation, &old_path, &new_content_path)?;
            let title = match rename_policy {
                VirtualRenamePolicy::FilenameDerived => filename_title.clone(),
                VirtualRenamePolicy::PreserveCanonical => mutation.title.clone(),
            };
            let retrying_published_move = old_projected_path == new_path;
            mutation.projected_path = new_path;
            mutation.title = title;
            mutation.parent_remote_id = Some(new_parent.remote_id);
            mutation.content_path = Some(old_path.clone());
            mutation.updated_at = now_string();
            mutation = persist_and_publish_virtual_move(
                store,
                VirtualMoveTransition {
                    mutation,
                    entity: None,
                    freshness: None,
                    superseded_local_ids: Vec::new(),
                },
                &old_path,
                &new_content_path,
                (rename_policy == VirtualRenamePolicy::FilenameDerived)
                    .then_some(filename_title.as_str()),
                retrying_published_move,
            )?;
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
        ensure_source_path_writable(&mount, &entity.path)?;
        ensure_virtual_page_directory_available_for_rename(
            store,
            mount_id,
            RenameOwner::Remote(&remote_id),
            &new_page_dir,
            &new_path,
        )?;
        let previous_entity_path = entity.path.clone();
        let existing_move = existing_remote_move_mutation(&mutations, &remote_id);
        let old_path = existing_move
            .and_then(|mutation| mutation.content_path.clone())
            .unwrap_or(content_path_for_relative(
                content_root,
                &previous_entity_path,
            )?);
        let new_content_path = content_path_for_relative(content_root, &new_path)?;
        ensure_remote_move_materializable(
            store,
            mount_id,
            &remote_id,
            &old_path,
            &new_content_path,
        )?;
        let title = match rename_policy {
            VirtualRenamePolicy::FilenameDerived => filename_title,
            VirtualRenamePolicy::PreserveCanonical => entity.title.clone(),
        };
        let original_path = existing_move_original_path(&mutations, &remote_id)
            .unwrap_or_else(|| previous_entity_path.clone());
        let retrying_published_move = previous_entity_path == new_path;
        entity.path = new_path.clone();
        entity.title = title;
        if entity.hydration.can_transition_to(&HydrationState::Dirty) {
            entity.hydration = HydrationState::Dirty;
        }
        let now = now_string();
        let mutation = VirtualMutationRecord {
            mount_id: mount_id.clone(),
            local_id: format!("move:{}", remote_id.0),
            mutation_kind: VirtualMutationKind::Move,
            target_remote_id: Some(remote_id.clone()),
            parent_remote_id: Some(new_parent.remote_id),
            original_path: Some(original_path),
            projected_path: new_path,
            title: entity.title.clone(),
            content_path: cache_move_source_pointer(existing_move, &old_path, &new_content_path),
            created_at: existing_move
                .map_or_else(|| now.clone(), |mutation| mutation.created_at.clone()),
            updated_at: now,
        };
        let freshness = freshness_after_virtual_local_change(store, &entity)?;
        persist_and_publish_virtual_move(
            store,
            VirtualMoveTransition {
                mutation,
                entity: Some(entity.clone()),
                freshness: Some(freshness),
                superseded_local_ids: remote_move_local_ids(&remote_id),
            },
            &old_path,
            &new_content_path,
            (rename_policy == VirtualRenamePolicy::FilenameDerived)
                .then_some(entity.title.as_str()),
            retrying_published_move,
        )?;
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
    ensure_source_path_writable(&mount, &new_path)?;
    if let Some(report) = reconcile_atomic_temp_rename(
        store,
        content_root,
        &mount,
        &entities,
        mount_id,
        identifier,
        new_parent_identifier,
        new_filename,
        &new_path,
    )? {
        return Ok(report);
    }
    let new_parent =
        move_parent_remote(&mount, &entities, new_parent_identifier, &new_parent_path)?;
    ensure_virtual_path_available_for_rename(store, mount_id, identifier, &new_path)?;

    if let Some(mut mutation) = local_mutation(store, mount_id, identifier)? {
        ensure_source_path_writable(&mount, &mutation.projected_path)?;
        let old_projected_path = mutation.projected_path.clone();
        let old_path = mutation
            .content_path
            .clone()
            .unwrap_or(content_path_for_relative(
                content_root,
                &old_projected_path,
            )?);
        let new_content_path = content_path_for_relative(content_root, &new_path)?;
        ensure_pending_create_materializable(&mutation, &old_path, &new_content_path)?;
        let retrying_published_move = old_projected_path == new_path;
        mutation.projected_path = new_path;
        if rename_policy == VirtualRenamePolicy::FilenameDerived {
            mutation.title = title_from_filename(new_filename);
        }
        mutation.parent_remote_id = Some(new_parent.remote_id);
        mutation.content_path = Some(old_path.clone());
        mutation.updated_at = now_string();
        mutation = persist_and_publish_virtual_move(
            store,
            VirtualMoveTransition {
                mutation,
                entity: None,
                freshness: None,
                superseded_local_ids: Vec::new(),
            },
            &old_path,
            &new_content_path,
            None,
            retrying_published_move,
        )?;
        let index = ProviderIndex::new(&entities);
        let item = pending_item(&mount, &mutation, &index);
        return mutation_report_for_content_item(mount_id, content_root, item);
    }

    let remote_id = RemoteId::new(entity_identifier(identifier)?);
    let mut entity = require_entity(store, mount_id, &remote_id)?;
    if entity.kind != EntityKind::Page {
        return Err(LocalityError::Unsupported(
            "only page.md files can be renamed by the virtual filesystem",
        ));
    }
    ensure_source_path_writable(&mount, &entity.path)?;
    let previous_entity_path = entity.path.clone();
    let existing_move = existing_remote_move_mutation(&mutations, &remote_id);
    let old_path = existing_move
        .and_then(|mutation| mutation.content_path.clone())
        .unwrap_or(content_path_for_relative(
            content_root,
            &previous_entity_path,
        )?);
    let new_content_path = content_path_for_relative(content_root, &new_path)?;
    ensure_remote_move_materializable(store, mount_id, &remote_id, &old_path, &new_content_path)?;
    let original_path =
        existing_move_original_path(&mutations, &remote_id).unwrap_or(previous_entity_path.clone());
    let retrying_published_move = previous_entity_path == new_path;
    entity.path = new_path.clone();
    if rename_policy == VirtualRenamePolicy::FilenameDerived {
        entity.title = title_from_filename(new_filename);
    }
    if entity.hydration.can_transition_to(&HydrationState::Dirty) {
        entity.hydration = HydrationState::Dirty;
    }
    let now = now_string();
    let mutation = VirtualMutationRecord {
        mount_id: mount_id.clone(),
        local_id: format!("move:{}", remote_id.0),
        mutation_kind: VirtualMutationKind::Move,
        target_remote_id: Some(remote_id.clone()),
        parent_remote_id: Some(new_parent.remote_id),
        original_path: Some(original_path),
        projected_path: new_path,
        title: entity.title.clone(),
        content_path: cache_move_source_pointer(existing_move, &old_path, &new_content_path),
        created_at: existing_move
            .map_or_else(|| now.clone(), |mutation| mutation.created_at.clone()),
        updated_at: now,
    };
    let freshness = freshness_after_virtual_local_change(store, &entity)?;
    persist_and_publish_virtual_move(
        store,
        VirtualMoveTransition {
            mutation,
            entity: Some(entity.clone()),
            freshness: Some(freshness),
            superseded_local_ids: remote_move_local_ids(&remote_id),
        },
        &old_path,
        &new_content_path,
        None,
        retrying_published_move,
    )?;
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

fn reconcile_atomic_temp_rename<S>(
    store: &mut S,
    content_root: &Path,
    mount: &MountConfig,
    entities: &[EntityRecord],
    mount_id: &MountId,
    identifier: &str,
    new_parent_identifier: &str,
    new_filename: &str,
    new_path: &Path,
) -> LocalityResult<Option<VirtualFsMutationReport>>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + VirtualMutationRepository
        + FreshnessStateRepository,
{
    if new_filename != locality_core::path_projection::PAGE_DOCUMENT_FILENAME {
        return Ok(None);
    }
    let Some(mutation) = local_mutation(store, mount_id, identifier)? else {
        return Ok(None);
    };

    if is_page_document_path(&mutation.projected_path)
        && new_parent_identifier == pending_page_child_dir_identifier(identifier)
        && mutation.projected_path == new_path
    {
        let index = ProviderIndex::new(entities);
        let item = pending_item(mount, &mutation, &index);
        return Ok(Some(mutation_report_for_content_item(
            mount_id,
            content_root,
            item,
        )?));
    }

    if !is_atomic_temp_filename(&filename(&mutation.projected_path)) {
        return Ok(None);
    }
    let Some(target) = store
        .find_entity_by_path(mount_id, new_path)
        .map_err(LocalityError::from)?
    else {
        return Ok(None);
    };
    if target.kind != EntityKind::Page {
        return Ok(None);
    }

    let temp_path = content_path_for_relative(content_root, &mutation.projected_path)?;
    let contents = match std::fs::read(&temp_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(LocalityError::Io(format!(
                "failed to read atomic temp file `{}`: {error}",
                temp_path.display()
            )));
        }
    };
    commit_virtual_fs_write(
        store,
        content_root,
        mount_id,
        &target.remote_id.0,
        &contents,
    )?;
    store
        .delete_virtual_mutation(mount_id, &mutation.local_id)
        .map_err(LocalityError::from)?;
    let _ = std::fs::remove_file(&temp_path);
    let refreshed = store.list_entities(mount_id).map_err(LocalityError::from)?;
    let index = ProviderIndex::new(&refreshed);
    let entity = refreshed
        .iter()
        .find(|entity| entity.remote_id == target.remote_id)
        .ok_or_else(|| missing_identifier(&target.remote_id.0))?;
    let item = entity_item(mount, entity, &index);
    Ok(Some(VirtualFsMutationReport {
        mount_id: mount_id.0.clone(),
        identifier: target.remote_id.0,
        path: item.path.clone(),
        item,
    }))
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
    repair_legacy_macos_content_root_for_current_root(content_root, mount_id)?;
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
        ensure_source_path_writable(&mount, &entity.path)?;
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
        return mutation_report_for_content_item(mount_id, content_root, item);
    }
    let remote_id = RemoteId::new(entity_identifier(identifier)?);
    let entity = require_entity(store, mount_id, &remote_id)?;
    if entity.kind != EntityKind::Page {
        return Err(LocalityError::Unsupported(
            "only page.md files can be deleted by the virtual filesystem",
        ));
    }
    ensure_source_path_writable(&mount, &entity.path)?;
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

pub fn repair_legacy_macos_content_root(
    state_root: &Path,
    mount_id: &MountId,
) -> LocalityResult<()> {
    repair_legacy_macos_content_root_for_paths(
        state_root,
        &virtual_fs_content_root(state_root, mount_id),
        mount_id,
    )
}

pub fn repair_legacy_macos_content_root_for_current_root(
    content_root: &Path,
    mount_id: &MountId,
) -> LocalityResult<()> {
    let Some(state_root) = state_root_for_current_content_root(content_root, mount_id) else {
        return Ok(());
    };
    repair_legacy_macos_content_root_for_paths(&state_root, content_root, mount_id)
}

#[cfg(target_os = "macos")]
pub fn virtual_fs_content_base(state_root: &Path) -> PathBuf {
    let override_root = std::env::var_os("LOCALITY_VIRTUAL_FS_CONTENT_ROOT").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);

    virtual_fs_content_base_from_config(
        state_root,
        override_root.as_deref(),
        home.as_deref(),
        macos_app_sandbox_enabled(),
    )
}

#[cfg(not(target_os = "macos"))]
pub fn virtual_fs_content_base(state_root: &Path) -> PathBuf {
    let override_root = std::env::var_os("LOCALITY_VIRTUAL_FS_CONTENT_ROOT").map(PathBuf::from);
    virtual_fs_content_base_from_config(state_root, override_root.as_deref())
}

#[cfg(target_os = "macos")]
fn virtual_fs_content_base_from_config(
    state_root: &Path,
    override_root: Option<&Path>,
    home: Option<&Path>,
    app_sandbox_enabled: bool,
) -> PathBuf {
    if let Some(root) = override_root {
        return root.to_path_buf();
    }

    if home.is_some_and(|home| is_default_state_root_for_home(state_root, home))
        && app_sandbox_enabled
    {
        return macos_app_group_container_for_home(home.expect("checked home")).join("content");
    }

    state_root.join("content")
}

#[cfg(not(target_os = "macos"))]
fn virtual_fs_content_base_from_config(state_root: &Path, override_root: Option<&Path>) -> PathBuf {
    override_root
        .map(Path::to_path_buf)
        .unwrap_or_else(|| state_root.join("content"))
}

#[cfg(target_os = "macos")]
fn is_default_state_root_for_home(state_root: &Path, home: &Path) -> bool {
    home.join(".loc") == state_root
}

#[cfg(target_os = "macos")]
fn macos_app_group_container_for_home(home: &Path) -> PathBuf {
    home.join("Library")
        .join("Group Containers")
        .join("C484HB7Q6S.group.ai.codeflash.locality")
}

#[cfg(target_os = "macos")]
fn macos_app_group_container_for_state_root(state_root: &Path) -> Option<PathBuf> {
    if state_root.file_name().and_then(|name| name.to_str()) != Some(".loc") {
        return None;
    }
    state_root.parent().map(macos_app_group_container_for_home)
}

#[cfg(target_os = "macos")]
fn macos_app_sandbox_enabled() -> bool {
    std::env::var_os("APP_SANDBOX_CONTAINER_ID").is_some_and(|value| !value.is_empty())
}

#[cfg(target_os = "macos")]
fn state_root_for_current_content_root(content_root: &Path, mount_id: &MountId) -> Option<PathBuf> {
    if content_root.file_name().and_then(OsStr::to_str) != Some("files") {
        return None;
    }
    let mount_dir = content_root.parent()?;
    if mount_dir.file_name().and_then(OsStr::to_str) != Some(mount_id.0.as_str()) {
        return None;
    }
    let content_base = mount_dir.parent()?;
    if content_base.file_name().and_then(OsStr::to_str) != Some("content") {
        return None;
    }
    content_base.parent().map(Path::to_path_buf)
}

#[cfg(not(target_os = "macos"))]
fn state_root_for_current_content_root(
    _content_root: &Path,
    _mount_id: &MountId,
) -> Option<PathBuf> {
    None
}

#[cfg(target_os = "macos")]
fn repair_legacy_macos_content_root_for_paths(
    state_root: &Path,
    current_content_root: &Path,
    mount_id: &MountId,
) -> LocalityResult<()> {
    let key = current_content_root.to_path_buf();
    if let Some(result) = legacy_macos_content_repair_result(&key)? {
        return result;
    }

    let result = repair_legacy_macos_content_root_for_paths_uncached(
        state_root,
        current_content_root,
        mount_id,
    );
    record_legacy_macos_content_repair_result(key, result.clone())?;
    result
}

#[cfg(target_os = "macos")]
fn repair_legacy_macos_content_root_for_paths_uncached(
    state_root: &Path,
    current_content_root: &Path,
    mount_id: &MountId,
) -> LocalityResult<()> {
    let Some(legacy_content_base) = macos_app_group_container_for_state_root(state_root)
        .map(|container| container.join("content"))
    else {
        return Ok(());
    };
    let current_content_base = virtual_fs_content_base(state_root);
    if current_content_base.starts_with(&legacy_content_base)
        || current_content_root.starts_with(&legacy_content_base)
    {
        return Ok(());
    }

    let legacy_content_root = legacy_content_base.join(&mount_id.0).join("files");
    let metadata = match std::fs::metadata(&legacy_content_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(error) => {
            return Err(LocalityError::Io(format!(
                "failed to inspect legacy macOS virtual filesystem content root `{}`: {error}",
                legacy_content_root.display()
            )));
        }
    };
    if !metadata.is_dir() {
        return Ok(());
    }

    copy_missing_legacy_content_files(&legacy_content_root, current_content_root)
}

#[cfg(not(target_os = "macos"))]
fn repair_legacy_macos_content_root_for_paths(
    _state_root: &Path,
    _current_content_root: &Path,
    _mount_id: &MountId,
) -> LocalityResult<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn legacy_macos_content_repair_result(key: &Path) -> LocalityResult<Option<LocalityResult<()>>> {
    LEGACY_MACOS_CONTENT_REPAIRS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .map(|repaired| repaired.get(key).cloned())
        .map_err(|_| {
            LocalityError::InvalidState(
                "legacy macOS virtual filesystem content repair lock poisoned".to_string(),
            )
        })
}

#[cfg(target_os = "macos")]
fn record_legacy_macos_content_repair_result(
    key: PathBuf,
    result: LocalityResult<()>,
) -> LocalityResult<()> {
    LEGACY_MACOS_CONTENT_REPAIRS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .map(|mut repaired| {
            repaired.insert(key, result);
        })
        .map_err(|_| {
            LocalityError::InvalidState(
                "legacy macOS virtual filesystem content repair lock poisoned".to_string(),
            )
        })
}

#[cfg(target_os = "macos")]
fn copy_missing_legacy_content_files(
    legacy_root: &Path,
    current_root: &Path,
) -> LocalityResult<()> {
    let mut pending_dirs = vec![PathBuf::new()];
    while let Some(relative_dir) = pending_dirs.pop() {
        let legacy_dir = legacy_root.join(&relative_dir);
        let entries = std::fs::read_dir(&legacy_dir).map_err(|error| {
            LocalityError::Io(format!(
                "failed to read legacy macOS virtual filesystem content directory `{}`: {error}",
                legacy_dir.display()
            ))
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                LocalityError::Io(format!(
                    "failed to read legacy macOS virtual filesystem content entry in `{}`: {error}",
                    legacy_dir.display()
                ))
            })?;
            let file_type = entry.file_type().map_err(|error| {
                LocalityError::Io(format!(
                    "failed to inspect legacy macOS virtual filesystem content entry `{}`: {error}",
                    entry.path().display()
                ))
            })?;
            let relative_path = relative_dir.join(entry.file_name());
            if file_type.is_dir() {
                pending_dirs.push(relative_path);
            } else if file_type.is_file() {
                copy_missing_legacy_content_file(&entry.path(), &current_root.join(relative_path))?;
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn copy_missing_legacy_content_file(legacy_path: &Path, current_path: &Path) -> LocalityResult<()> {
    if current_path.exists() {
        return Ok(());
    }
    let contents = std::fs::read(legacy_path).map_err(|error| {
        LocalityError::Io(format!(
            "failed to read legacy macOS virtual filesystem content `{}`: {error}",
            legacy_path.display()
        ))
    })?;
    if let Some(parent) = current_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            LocalityError::Io(format!(
                "failed to create virtual filesystem content directory `{}`: {error}",
                parent.display()
            ))
        })?;
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(current_path)
    {
        Ok(mut file) => file.write_all(&contents).map_err(|error| {
            LocalityError::Io(format!(
                "failed to write migrated virtual filesystem content `{}`: {error}",
                current_path.display()
            ))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(LocalityError::Io(format!(
            "failed to create migrated virtual filesystem content `{}`: {error}",
            current_path.display()
        ))),
    }
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

pub(crate) fn virtual_mutation_content_path_for_read(
    state_root: Option<&Path>,
    mount_id: &MountId,
    mutation: &VirtualMutationRecord,
) -> LocalityResult<Option<PathBuf>> {
    let current_path = state_root
        .map(|root| virtual_fs_content_path(root, mount_id, &mutation.projected_path))
        .transpose()?;

    if let Some(stored_path) = mutation.content_path.as_ref() {
        if let (Some(state_root), Some(current_path)) = (state_root, current_path.as_ref())
            && legacy_macos_app_group_content_path(stored_path, state_root)
        {
            return Ok(Some(current_path.clone()));
        }

        return Ok(Some(stored_path.clone()));
    }

    Ok(current_path)
}

#[cfg(target_os = "macos")]
fn legacy_macos_app_group_content_path(stored_path: &Path, state_root: &Path) -> bool {
    let Some(group_content_base) = macos_app_group_container_for_state_root(state_root)
        .map(|container| container.join("content"))
    else {
        return false;
    };
    stored_path.starts_with(&group_content_base)
        && !virtual_fs_content_base(state_root).starts_with(&group_content_base)
}

#[cfg(not(target_os = "macos"))]
fn legacy_macos_app_group_content_path(_stored_path: &Path, _state_root: &Path) -> bool {
    false
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

fn refresh_database_schema_cache_for_container<S, Source>(
    store: &S,
    source: &Source,
    content_root: &Path,
    mount_id: &MountId,
    container_identifier: &str,
) -> LocalityResult<bool>
where
    S: MountRepository + EntityRepository,
    Source: HydrationSource + ?Sized,
{
    let mount = require_virtual_mount(store, mount_id)?;
    if container_identifier == ROOT_CONTAINER_IDENTIFIER
        || container_identifier == mount_point_identifier(&mount)
        || container_identifier.starts_with(CHILDREN_PREFIX)
        || container_identifier.starts_with(PATH_PREFIX)
    {
        return Ok(false);
    }

    let remote_id = RemoteId::new(container_identifier);
    let Some(entity) = store
        .get_entity(mount_id, &remote_id)
        .map_err(LocalityError::from)?
    else {
        return Ok(false);
    };
    if entity.kind != EntityKind::Database {
        return Ok(false);
    }

    let Some(schema) = source.fetch_database_schema_yaml(&entity.remote_id)? else {
        return Ok(false);
    };
    let path = content_path_for_relative(content_root, &entity.path.join("_schema.yaml"))?;
    let existing = match std::fs::read_to_string(&path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(LocalityError::Io(format!(
                "failed to read database schema cache `{}`: {error}",
                path.display()
            )));
        }
    };
    if existing.as_deref() == Some(schema.as_str()) {
        return Ok(false);
    }
    write_binary_atomic(&path, schema.as_bytes())?;
    Ok(true)
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
        let remote_id = RemoteId::new(remote_id);
        let entity = entities
            .iter()
            .find(|entity| entity.remote_id == remote_id)
            .ok_or_else(|| missing_identifier(parent_identifier))?;
        return create_parent_remote_id_for_entity(mount, remote_id, entity);
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
    create_parent_remote_id_for_entity(mount, remote_id, entity)
}

fn create_parent_remote_id_for_entity(
    mount: &MountConfig,
    remote_id: RemoteId,
    entity: &EntityRecord,
) -> LocalityResult<RemoteId> {
    if source_accepts_create_parent_kind(mount, &entity.kind) {
        Ok(remote_id)
    } else {
        Err(LocalityError::Unsupported(
            "new virtual filesystem files cannot be created under this source item",
        ))
    }
}

#[derive(Clone, Debug)]
struct MoveParent {
    remote_id: RemoteId,
}

fn move_parent_remote(
    mount: &MountConfig,
    entities: &[EntityRecord],
    parent_identifier: &str,
    parent_path: &Path,
) -> LocalityResult<MoveParent> {
    ensure_source_move_parent_writable(mount, parent_path)?;

    if let Some(remote_id) = parent_identifier.strip_prefix(CHILDREN_PREFIX) {
        if remote_id.starts_with(LOCAL_PREFIX) {
            return Err(LocalityError::Unsupported(
                "virtual filesystem pages cannot be moved under an unpushed local page",
            ));
        }
        let remote_id = RemoteId::new(remote_id);
        let entity = entities
            .iter()
            .find(|entity| entity.remote_id == remote_id)
            .ok_or_else(|| missing_identifier(parent_identifier))?;
        if entity.kind != EntityKind::Page {
            return Err(LocalityError::Unsupported(
                "children containers can only target page parents",
            ));
        }
        let remote_id = move_parent_remote_id_for_entity(mount, remote_id, entity)?;
        return Ok(MoveParent { remote_id });
    }

    if parent_identifier == mount_point_identifier(mount) {
        if let Some(parent) = source_root_move_parent(mount) {
            return Ok(parent);
        }
        return Err(LocalityError::Unsupported(
            "virtual filesystem pages must be moved inside a page, database, or folder directory",
        ));
    }

    if parent_identifier == ROOT_CONTAINER_IDENTIFIER || parent_identifier.starts_with(PATH_PREFIX)
    {
        return Err(LocalityError::Unsupported(
            "virtual filesystem pages must be moved inside a page, database, or folder directory",
        ));
    }

    let remote_id = RemoteId::new(parent_identifier);
    let entity = entities
        .iter()
        .find(|entity| entity.remote_id == remote_id)
        .ok_or_else(|| missing_identifier(parent_identifier))?;
    let remote_id = move_parent_remote_id_for_entity(mount, remote_id, entity)?;
    Ok(MoveParent { remote_id })
}

fn source_root_move_parent(mount: &MountConfig) -> Option<MoveParent> {
    let descriptor = source_descriptor(&mount.connector);
    if descriptor.source_root_create_parent_kind().is_some()
        && let Some(remote_id) = mount.remote_root_id.clone()
    {
        return Some(MoveParent { remote_id });
    }
    None
}

fn move_parent_remote_id_for_entity(
    mount: &MountConfig,
    remote_id: RemoteId,
    entity: &EntityRecord,
) -> LocalityResult<RemoteId> {
    if source_accepts_move_parent_kind(mount, &entity.kind) {
        Ok(remote_id)
    } else {
        Err(LocalityError::Unsupported(
            "virtual filesystem pages cannot be moved under this source item",
        ))
    }
}

fn existing_move_original_path(
    mutations: &[VirtualMutationRecord],
    remote_id: &RemoteId,
) -> Option<PathBuf> {
    mutations
        .iter()
        .find(|mutation| {
            matches!(
                mutation.mutation_kind,
                VirtualMutationKind::Move | VirtualMutationKind::Rename
            ) && mutation.target_remote_id.as_ref() == Some(remote_id)
        })
        .and_then(|mutation| mutation.original_path.clone())
}

fn existing_remote_move_mutation<'a>(
    mutations: &'a [VirtualMutationRecord],
    remote_id: &RemoteId,
) -> Option<&'a VirtualMutationRecord> {
    mutations.iter().find(|mutation| {
        matches!(
            mutation.mutation_kind,
            VirtualMutationKind::Move | VirtualMutationKind::Rename
        ) && mutation.target_remote_id.as_ref() == Some(remote_id)
    })
}

fn remote_move_local_ids(remote_id: &RemoteId) -> Vec<String> {
    vec![
        format!("move:{}", remote_id.0),
        format!("rename:{}", remote_id.0),
    ]
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
        && mutation
            .target_remote_id
            .as_ref()
            .is_none_or(|remote_id| remote_id.0 != identifier)
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
        && !matches!(owner, RenameOwner::Remote(remote_id) if mutation.target_remote_id.as_ref() == Some(remote_id))
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
            && !matches!(owner, RenameOwner::Remote(remote_id) if mutation.target_remote_id.as_ref() == Some(remote_id))
        {
            return Err(LocalityError::InvalidState(format!(
                "virtual filesystem path `{}` already exists",
                page_dir.display()
            )));
        }
    }
    Ok(())
}

fn cache_move_source_pointer(
    existing_move: Option<&VirtualMutationRecord>,
    old_path: &Path,
    new_content_path: &Path,
) -> Option<PathBuf> {
    if let Some(existing) = existing_move
        && existing.content_path.as_deref() == Some(new_content_path)
    {
        return Some(new_content_path.to_path_buf());
    }
    Some(old_path.to_path_buf())
}

fn freshness_after_virtual_local_change<S>(
    store: &S,
    entity: &EntityRecord,
) -> LocalityResult<FreshnessStateRecord>
where
    S: FreshnessStateRepository,
{
    let mut state = store
        .get_freshness_state(&entity.mount_id, &entity.remote_id)
        .map_err(LocalityError::from)?
        .unwrap_or_else(|| {
            FreshnessStateRecord::new(
                entity.mount_id.clone(),
                entity.remote_id.clone(),
                FreshnessTier::Hot,
            )
        });
    if FreshnessTier::Hot.is_more_urgent_than(&state.tier) {
        state.tier = FreshnessTier::Hot;
    }
    state.last_local_change_at = Some(now_string());
    Ok(state)
}

fn persist_and_publish_virtual_move<S>(
    store: &mut S,
    transition: VirtualMoveTransition,
    old_path: &Path,
    new_content_path: &Path,
    retitle: Option<&str>,
    retrying_published_move: bool,
) -> LocalityResult<VirtualMutationRecord>
where
    S: VirtualMoveRepository,
{
    let mount_id = transition.mutation.mount_id.clone();
    let local_id = transition.mutation.local_id.clone();
    let expected_content_path = transition.mutation.content_path.clone();
    if old_path != new_content_path && new_content_path.exists() && !retrying_published_move {
        return Err(LocalityError::InvalidState(format!(
            "virtual filesystem content `{}` already exists",
            new_content_path.display()
        )));
    }

    store
        .begin_virtual_move(transition)
        .map_err(LocalityError::from)?;

    if old_path != new_content_path && !new_content_path.exists() {
        publish_virtual_move_cache(old_path, new_content_path, retitle)?;
    } else if let Some(title) = retitle {
        retitle_cached_page_if_present(new_content_path, title)?;
    }

    let finalized = store
        .finalize_virtual_move_content(
            &mount_id,
            &local_id,
            expected_content_path.as_deref(),
            new_content_path.to_path_buf(),
            &now_string(),
        )
        .map_err(LocalityError::from)?;

    if old_path != new_content_path && old_path.exists() {
        let _ = remove_path_durable(old_path);
    }
    Ok(finalized)
}

fn publish_virtual_move_cache(
    old_path: &Path,
    new_content_path: &Path,
    retitle: Option<&str>,
) -> LocalityResult<()> {
    if !old_path.is_file() {
        return Ok(());
    }
    let contents = read_virtual_move_cache(old_path, retitle)?;
    let parent = new_content_path.parent().ok_or_else(|| {
        LocalityError::InvalidState(format!(
            "virtual filesystem content `{}` has no parent",
            new_content_path.display()
        ))
    })?;
    create_dir_all_durable(parent).map_err(|error| {
        LocalityError::Io(format!(
            "failed to create virtual filesystem content directory `{}`: {error}",
            parent.display()
        ))
    })?;
    let temp_path = virtual_move_temp_path(new_content_path);
    write_new_file_durable(&temp_path, &contents).map_err(|error| {
        LocalityError::Io(format!(
            "failed to write virtual filesystem temp file `{}`: {error}",
            temp_path.display()
        ))
    })?;
    rename_noreplace_durable(&temp_path, new_content_path).map_err(|error| {
        let _ = remove_path_durable(&temp_path);
        LocalityError::Io(format!(
            "failed to publish virtual filesystem content `{}`: {error}",
            new_content_path.display()
        ))
    })
}

fn read_virtual_move_cache(path: &Path, retitle: Option<&str>) -> LocalityResult<Vec<u8>> {
    let contents = std::fs::read(path).map_err(|error| {
        LocalityError::Io(format!(
            "failed to read virtual filesystem content `{}`: {error}",
            path.display()
        ))
    })?;
    let Some(title) = retitle else {
        return Ok(contents);
    };
    let Ok(text) = String::from_utf8(contents.clone()) else {
        return Ok(contents);
    };
    if text.trim().is_empty() {
        return Ok(contents);
    }
    let Ok(parsed) = parse_canonical_markdown(&text) else {
        return Ok(contents);
    };
    if parsed.frontmatter.title.as_deref() == Some(title) {
        return Ok(contents);
    }
    let frontmatter = retitled_frontmatter(&parsed.document.frontmatter, title);
    Ok(
        render_canonical_markdown(&CanonicalDocument::new(frontmatter, parsed.document.body))
            .into_bytes(),
    )
}

fn virtual_move_temp_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("loc-virtual-fs");
    path.with_file_name(format!(".{file_name}.loc-move-{}", unique_suffix()))
}

fn ensure_remote_move_materializable<S>(
    store: &S,
    mount_id: &MountId,
    remote_id: &RemoteId,
    cached_path: &Path,
    target_path: &Path,
) -> LocalityResult<()>
where
    S: ShadowRepository,
{
    if cached_path.is_file() || target_path.is_file() {
        return Ok(());
    }
    match store.load_shadow(mount_id, remote_id) {
        Ok(_) => Ok(()),
        Err(StoreError::ShadowMissing { .. }) => Err(LocalityError::InvalidState(format!(
            "entity `{}` must be materialized before it can be moved or renamed",
            remote_id.0
        ))),
        Err(error) => Err(error.into()),
    }
}

fn ensure_pending_create_materializable(
    mutation: &VirtualMutationRecord,
    cached_path: &Path,
    target_path: &Path,
) -> LocalityResult<()> {
    if cached_path.is_file() || target_path.is_file() {
        return Ok(());
    }
    Err(LocalityError::InvalidState(format!(
        "pending create `{}` must be materialized before it can be moved or renamed",
        mutation.local_id
    )))
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
            .find(|entity| {
                entity.remote_id.0 == remote_id
                    && entity.kind == EntityKind::Page
                    && page_projects_as_directory(&entity.path)
            })
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
        read_only: source_root_read_only(mount),
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
        read_only: true,
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

pub(crate) fn source_root_read_only(mount: &MountConfig) -> bool {
    mount.read_only || source_root_move_parent(mount).is_none()
}

fn item_file_read_only(mount: &MountConfig, path: &Path) -> bool {
    !source_write_decision_for_path(mount, path).is_writable()
}

fn item_folder_read_only(
    mount: &MountConfig,
    path: &Path,
    entity_kind: Option<&EntityKind>,
) -> bool {
    item_file_read_only(mount, path)
        || !source_create_decision_for_parent_path(mount, path).is_writable()
        || entity_kind.is_some_and(|kind| !source_accepts_create_parent_kind(mount, kind))
}

fn source_accepts_create_parent_kind(mount: &MountConfig, kind: &EntityKind) -> bool {
    source_descriptor(&mount.connector)
        .create_entity_parent_kinds()
        .contains(kind)
}

fn source_accepts_move_parent_kind(mount: &MountConfig, kind: &EntityKind) -> bool {
    source_descriptor(&mount.connector)
        .move_entity_parent_kinds()
        .contains(kind)
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
    let read_only = match &kind {
        VirtualFsItemKind::File => item_file_read_only(mount, &entity.path),
        VirtualFsItemKind::Folder => item_folder_read_only(mount, &entity.path, Some(&entity.kind)),
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
        read_only,
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
    let parent_identifier = if pending_created_directory_path(mutation).is_some() {
        pending_page_child_dir_identifier(&mutation.local_id)
    } else {
        container_identifier_for_path(mount, parent_path(&mutation.projected_path), index)
    };
    VirtualFsItem {
        identifier: mutation.local_id.clone(),
        parent_identifier: Some(parent_identifier),
        filename: filename(&mutation.projected_path),
        kind: VirtualFsItemKind::File,
        read_only: item_file_read_only(mount, &mutation.projected_path),
        entity_kind: Some(if is_database_draft_schema_path(&mutation.projected_path) {
            EntityKind::Database
        } else {
            EntityKind::Page
        }),
        remote_id: None,
        path: path_string(&mutation.projected_path),
        hydration: Some(HydrationState::Dirty),
        content_type: if is_database_draft_schema_path(&mutation.projected_path) {
            "public.yaml"
        } else {
            "net.daringfireball.markdown"
        }
        .to_string(),
        remote_edited_at: None,
        materialized_path: None,
        byte_size: None,
    }
}

fn pending_listing_item(
    mount: &MountConfig,
    mutation: &VirtualMutationRecord,
    index: &ProviderIndex,
) -> VirtualFsItem {
    if pending_created_directory_path(mutation).is_some() {
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
    let database_draft = is_database_draft_schema_path(&mutation.projected_path);
    let path = pending_created_directory_path(mutation)
        .unwrap_or_else(|| page_container_path(&mutation.projected_path));
    let entity_kind = if database_draft {
        EntityKind::Database
    } else {
        EntityKind::Page
    };
    VirtualFsItem {
        identifier: pending_page_child_dir_identifier(&mutation.local_id),
        parent_identifier: Some(container_identifier_for_path(
            mount,
            parent_path(&path),
            index,
        )),
        filename: filename(&path),
        kind: VirtualFsItemKind::Folder,
        read_only: item_folder_read_only(mount, &path, Some(&entity_kind)),
        entity_kind: Some(entity_kind),
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
    let parent = pending_created_directory_path(mutation)
        .unwrap_or_else(|| page_container_path(&mutation.projected_path));
    let path = parent.join(filename);
    VirtualFsItem {
        identifier: mutation.local_id.clone(),
        parent_identifier: Some(pending_page_child_dir_identifier(&mutation.local_id)),
        filename: filename.to_string(),
        kind: VirtualFsItemKind::File,
        read_only: item_file_read_only(mount, &path),
        entity_kind: Some(if is_database_draft_schema_path(&mutation.projected_path) {
            EntityKind::Database
        } else {
            EntityKind::Page
        }),
        remote_id: None,
        path: path_string(&path),
        hydration: Some(HydrationState::Dirty),
        content_type: "public.data".to_string(),
        remote_edited_at: None,
        materialized_path: Some(mount.root.join(path).display().to_string()),
        byte_size: None,
    }
}

fn pending_page_child_dir_identifier(local_id: &str) -> String {
    format!("{CHILDREN_PREFIX}{local_id}")
}

fn pending_listing_parent_path(mutation: &VirtualMutationRecord) -> PathBuf {
    if let Some(directory) = pending_created_directory_path(mutation) {
        return parent_path(&directory).to_path_buf();
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
        || pending_created_directory_path(mutation).is_none()
    {
        return Err(LocalityError::Unsupported(
            "only pending-created page or database directories can be used as local containers",
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

fn is_database_schema_atomic_temp_filename(filename: &str) -> bool {
    filename.starts_with("_schema.yaml.tmp.")
}

fn is_database_draft_schema_path(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("_schema.yaml")
}

fn pending_created_directory_path(mutation: &VirtualMutationRecord) -> Option<PathBuf> {
    if is_page_document_path(&mutation.projected_path) {
        return Some(page_container_path(&mutation.projected_path));
    }
    if is_database_draft_schema_path(&mutation.projected_path) {
        return mutation.projected_path.parent().map(Path::to_path_buf);
    }
    None
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
        read_only: true,
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

fn loc_asset_cache_children(
    mount: &MountConfig,
    content_root: &Path,
    container_path: &Path,
) -> LocalityResult<Vec<VirtualFsItem>> {
    let mut children = Vec::new();
    if container_path.as_os_str().is_empty() {
        let loc_path = Path::new(LOC_CACHE_ROOT);
        let absolute_path = content_path_for_relative(content_root, loc_path)?;
        if absolute_path.is_dir() {
            children.push(loc_asset_cache_dir_item(mount, loc_path, &absolute_path));
        }
        return Ok(children);
    }

    if !is_loc_cache_path(container_path) {
        return Ok(children);
    }

    let absolute_dir = content_path_for_relative(content_root, container_path)?;
    let entries = match std::fs::read_dir(&absolute_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(children),
        Err(error) => {
            return Err(LocalityError::Io(format!(
                "failed to read downloaded asset cache directory `{}`: {error}",
                absolute_dir.display()
            )));
        }
    };

    for entry in entries {
        let entry = entry.map_err(|error| {
            LocalityError::Io(format!(
                "failed to read downloaded asset cache entry in `{}`: {error}",
                absolute_dir.display()
            ))
        })?;
        let child_name = entry.file_name();
        if is_loc_asset_cache_temp_name(&child_name) {
            continue;
        }
        let file_type = entry.file_type().map_err(|error| {
            LocalityError::Io(format!(
                "failed to inspect downloaded asset cache entry `{}`: {error}",
                entry.path().display()
            ))
        })?;
        let child_path = container_path.join(&child_name);
        if file_type.is_dir() {
            children.push(loc_asset_cache_dir_item(mount, &child_path, &entry.path()));
        } else if file_type.is_file() {
            let metadata = entry.metadata().map_err(|error| {
                LocalityError::Io(format!(
                    "failed to inspect downloaded asset cache file `{}`: {error}",
                    entry.path().display()
                ))
            })?;
            children.push(loc_asset_cache_file_item(
                mount,
                &child_path,
                &entry.path(),
                Some(metadata.len()),
            ));
        }
    }

    Ok(children)
}

fn loc_asset_cache_item_for_identifier(
    mount: &MountConfig,
    content_root: &Path,
    identifier: &str,
) -> LocalityResult<Option<VirtualFsItem>> {
    if let Some(path) = identifier.strip_prefix(ASSET_CACHE_PREFIX) {
        let path = PathBuf::from(path);
        ensure_loc_cache_path(identifier, &path)?;
        let absolute_path = content_path_for_relative(content_root, &path)?;
        let metadata = existing_metadata(identifier, &absolute_path)?;
        if !metadata.is_file() {
            return Err(missing_identifier(identifier));
        }
        return Ok(Some(loc_asset_cache_file_item(
            mount,
            &path,
            &absolute_path,
            Some(metadata.len()),
        )));
    }

    if let Some(path) = identifier.strip_prefix(PATH_PREFIX) {
        let path = PathBuf::from(path);
        if !is_loc_cache_path(&path) {
            return Ok(None);
        }
        let absolute_path = content_path_for_relative(content_root, &path)?;
        let metadata = existing_metadata(identifier, &absolute_path)?;
        if !metadata.is_dir() {
            return Err(missing_identifier(identifier));
        }
        return Ok(Some(loc_asset_cache_dir_item(mount, &path, &absolute_path)));
    }

    Ok(None)
}

fn materialize_loc_asset_cache_item(
    mount: &MountConfig,
    content_root: &Path,
    identifier: &str,
) -> LocalityResult<Option<VirtualFsMaterializeReport>> {
    let Some(path) = identifier.strip_prefix(ASSET_CACHE_PREFIX) else {
        return Ok(None);
    };
    let path = PathBuf::from(path);
    ensure_loc_cache_path(identifier, &path)?;
    let absolute_path = content_path_for_relative(content_root, &path)?;
    let metadata = existing_metadata(identifier, &absolute_path)?;
    if !metadata.is_file() {
        return Err(missing_identifier(identifier));
    }

    Ok(Some(VirtualFsMaterializeReport {
        mount_id: mount.mount_id.0.clone(),
        identifier: identifier.to_string(),
        remote_id: identifier.to_string(),
        path: absolute_path.display().to_string(),
        outcome: VirtualFsMaterializeOutcome::AlreadyMaterialized,
        hydration: HydrationState::Hydrated,
    }))
}

fn loc_asset_cache_dir_item(
    mount: &MountConfig,
    path: &Path,
    absolute_path: &Path,
) -> VirtualFsItem {
    VirtualFsItem {
        identifier: format!("{PATH_PREFIX}{}", path_string(path)),
        parent_identifier: Some(loc_asset_cache_parent_identifier(mount, path)),
        filename: filename(path),
        kind: VirtualFsItemKind::Folder,
        read_only: true,
        entity_kind: None,
        remote_id: None,
        path: path_string(path),
        hydration: None,
        content_type: "public.folder".to_string(),
        remote_edited_at: None,
        materialized_path: Some(absolute_path.display().to_string()),
        byte_size: None,
    }
}

fn loc_asset_cache_file_item(
    mount: &MountConfig,
    path: &Path,
    absolute_path: &Path,
    byte_size: Option<u64>,
) -> VirtualFsItem {
    VirtualFsItem {
        identifier: format!("{ASSET_CACHE_PREFIX}{}", path_string(path)),
        parent_identifier: Some(loc_asset_cache_parent_identifier(mount, path)),
        filename: filename(path),
        kind: VirtualFsItemKind::File,
        read_only: true,
        entity_kind: None,
        remote_id: None,
        path: path_string(path),
        hydration: Some(HydrationState::Hydrated),
        content_type: "public.data".to_string(),
        remote_edited_at: None,
        materialized_path: Some(absolute_path.display().to_string()),
        byte_size,
    }
}

fn loc_asset_cache_parent_identifier(mount: &MountConfig, path: &Path) -> String {
    let parent = parent_path(path);
    if parent.as_os_str().is_empty() {
        mount_point_identifier(mount)
    } else {
        format!("{PATH_PREFIX}{}", path_string(parent))
    }
}

fn ensure_loc_cache_path(identifier: &str, path: &Path) -> LocalityResult<()> {
    if is_loc_cache_path(path) {
        Ok(())
    } else {
        Err(LocalityError::InvalidState(format!(
            "downloaded asset cache identifier `{identifier}` does not target .loc"
        )))
    }
}

fn is_loc_cache_path(path: &Path) -> bool {
    validate_relative_path(path).is_ok()
        && matches!(
            path.components().next(),
            Some(std::path::Component::Normal(component))
                if component == OsStr::new(LOC_CACHE_ROOT)
        )
}

fn is_loc_asset_cache_temp_name(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name.starts_with('.') && name.ends_with(".loc-tmp"))
}

fn existing_metadata(identifier: &str, path: &Path) -> LocalityResult<std::fs::Metadata> {
    std::fs::metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            missing_identifier(identifier)
        } else {
            LocalityError::Io(format!(
                "failed to inspect downloaded asset cache path `{}`: {error}",
                path.display()
            ))
        }
    })
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

fn is_hydratable_markdown_entity(entity: &EntityRecord) -> bool {
    entity.kind == EntityKind::Page
        || (entity.kind == EntityKind::Asset
            && entity
                .path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("md")))
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
        read_only: item_folder_read_only(mount, path, Some(&EntityKind::Page)),
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
        read_only: item_folder_read_only(mount, path, None),
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
            return pending_created_directory_path(mutation)
                .ok_or_else(|| missing_identifier(identifier));
        }
        let entity = entities
            .iter()
            .find(|entity| {
                entity.remote_id.0 == remote_id
                    && entity.kind == EntityKind::Page
                    && page_projects_as_directory(&entity.path)
            })
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
    if entity.kind == EntityKind::Page && page_projects_as_directory(&entity.path) {
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
        EntityKind::Page if page_projects_as_directory(&entity.path) => {
            Some(ChildContainer::PageChildren(remote_id))
        }
        EntityKind::Database => Some(ChildContainer::DatabaseRows(remote_id)),
        EntityKind::Directory => Some(ChildContainer::DirectoryChildren(remote_id)),
        EntityKind::Page | EntityKind::Asset | EntityKind::Unknown(_) => None,
    })
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
        || identifier.starts_with(ASSET_CACHE_PREFIX)
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

fn ensure_source_path_writable(mount: &MountConfig, relative_path: &Path) -> LocalityResult<()> {
    match source_write_decision_for_path(mount, relative_path) {
        crate::source::SourceWriteDecision::Writable => Ok(()),
        crate::source::SourceWriteDecision::ReadOnly { reason } => {
            Err(LocalityError::Unsupported(reason))
        }
    }
}

fn ensure_source_parent_accepts_create(
    mount: &MountConfig,
    parent_path: &Path,
) -> LocalityResult<()> {
    match source_create_decision_for_parent_path(mount, parent_path) {
        crate::source::SourceWriteDecision::Writable => Ok(()),
        crate::source::SourceWriteDecision::ReadOnly { reason } => {
            Err(LocalityError::Unsupported(reason))
        }
    }
}

fn ensure_source_move_parent_writable(
    mount: &MountConfig,
    parent_path: &Path,
) -> LocalityResult<()> {
    match source_move_decision_for_parent_path(mount, parent_path) {
        crate::source::SourceWriteDecision::Writable => Ok(()),
        crate::source::SourceWriteDecision::ReadOnly { reason } => {
            Err(LocalityError::Unsupported(reason))
        }
    }
}

fn is_missing_identifier_error(error: &LocalityError) -> bool {
    matches!(
        error,
        LocalityError::InvalidState(message)
            if message.starts_with("virtual filesystem item `")
                && message.ends_with("` is not present in daemon state")
    )
}

fn parent_path(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| *parent != Path::new(""))
        .unwrap_or_else(|| Path::new(""))
}

fn entity_listing_parent_path(entity: &EntityRecord) -> PathBuf {
    match entity.kind {
        EntityKind::Page if page_projects_as_directory(&entity.path) => {
            page_listing_parent_path(&entity.path)
        }
        EntityKind::Database
        | EntityKind::Directory
        | EntityKind::Asset
        | EntityKind::Unknown(_)
        | EntityKind::Page => parent_path(&entity.path).to_path_buf(),
    }
}

fn entity_parent_container_path(entity: &EntityRecord) -> PathBuf {
    match entity.kind {
        EntityKind::Page if page_projects_as_directory(&entity.path) => {
            page_container_path(&entity.path)
        }
        EntityKind::Database
        | EntityKind::Directory
        | EntityKind::Asset
        | EntityKind::Unknown(_)
        | EntityKind::Page => parent_path(&entity.path).to_path_buf(),
    }
}

fn entity_subtree_root_path(entity: &EntityRecord) -> PathBuf {
    match entity.kind {
        EntityKind::Page if page_projects_as_directory(&entity.path) => {
            page_container_path(&entity.path)
        }
        EntityKind::Database
        | EntityKind::Directory
        | EntityKind::Asset
        | EntityKind::Unknown(_)
        | EntityKind::Page => entity.path.clone(),
    }
}

fn page_projects_as_directory(path: &Path) -> bool {
    is_page_document_path(path)
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
            if entity.kind == EntityKind::Page && page_projects_as_directory(&entity.path) {
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
    use std::fs;
    use std::path::{Path, PathBuf};

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
        shadow::ShadowDocument,
    };
    use locality_store::{
        EntityRecord, EntityRepository, FreshnessStateRepository, InMemoryStateStore, MountConfig,
        MountRepository, ProjectionMode, ShadowRepository, VirtualMutationKind,
        VirtualMutationRecord, VirtualMutationRepository,
    };

    use crate::hydration::{HydratedEntity, HydrationSource};

    use super::{
        AGENTS_GUIDANCE_IDENTIFIER, CLAUDE_GUIDANCE_IDENTIFIER, ROOT_CONTAINER_IDENTIFIER,
        VirtualFsItemKind, VirtualFsMaterializeOutcome, commit_virtual_fs_write,
        create_virtual_fs_directory, create_virtual_fs_file,
        materialize_virtual_fs_guidance_with_content_root,
        materialize_virtual_fs_item_with_content_root, mount_point_identifier,
        refresh_virtual_fs_children, refresh_virtual_fs_children_with_content_root,
        rename_virtual_fs_item, repair_legacy_macos_content_root, trash_virtual_fs_item,
        validate_virtual_projection_root, virtual_fs_ancestor_container_identifiers,
        virtual_fs_children, virtual_fs_children_with_content_root, virtual_fs_content_path,
        virtual_fs_content_root, virtual_fs_item, virtual_fs_item_with_content_root,
        virtual_projection_root,
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
    fn read_only_mount_reports_virtual_roots_as_read_only() {
        let mut store = InMemoryStateStore::new();
        let mount_id = MountId::new("notion-main");
        let mount = virtual_mount(&mount_id).read_only(true);
        store.save_mount(mount.clone()).expect("save mount");

        let root = virtual_fs_item(&store, &mount_id, ROOT_CONTAINER_IDENTIFIER)
            .expect("root item")
            .item;
        assert!(root.read_only);

        let mount_point = virtual_fs_item(&store, &mount_id, &mount_point_identifier(&mount))
            .expect("mount point item")
            .item;
        assert!(mount_point.read_only);

        let root_children = virtual_fs_children(&store, &mount_id, ROOT_CONTAINER_IDENTIFIER)
            .expect("root children");
        assert_eq!(root_children.children.len(), 1);
        assert!(root_children.children[0].read_only);
    }

    #[test]
    fn virtual_roots_report_writable_only_when_source_root_creates_are_supported() {
        let mut store = InMemoryStateStore::new();
        let notion_mount_id = MountId::new("notion-main");
        let notion_root_page_mount_id = MountId::new("notion-root-page");
        let google_mount_id = MountId::new("google-docs-main");
        let notion_mount = virtual_mount(&notion_mount_id);
        let notion_root_page_mount = virtual_mount(&notion_root_page_mount_id)
            .with_remote_root_id(RemoteId::new("page-root"));
        let google_mount = virtual_mount_with_connector(&google_mount_id, "google-docs")
            .with_remote_root_id(RemoteId::new("workspace-folder"));
        store.save_mount(notion_mount.clone()).expect("save notion");
        store
            .save_mount(notion_root_page_mount.clone())
            .expect("save notion root-page mount");
        store
            .save_mount(google_mount.clone())
            .expect("save google docs");

        let notion_root = virtual_fs_item(&store, &notion_mount_id, ROOT_CONTAINER_IDENTIFIER)
            .expect("notion root")
            .item;
        let notion_mount_point = virtual_fs_item(
            &store,
            &notion_mount_id,
            &mount_point_identifier(&notion_mount),
        )
        .expect("notion mount point")
        .item;
        assert!(notion_root.read_only);
        assert!(notion_mount_point.read_only);

        let notion_root_page_mount_point = virtual_fs_item(
            &store,
            &notion_root_page_mount_id,
            &mount_point_identifier(&notion_root_page_mount),
        )
        .expect("notion root-page mount point")
        .item;
        assert!(notion_root_page_mount_point.read_only);

        let google_root = virtual_fs_item(&store, &google_mount_id, ROOT_CONTAINER_IDENTIFIER)
            .expect("google docs root")
            .item;
        let google_mount_point = virtual_fs_item(
            &store,
            &google_mount_id,
            &mount_point_identifier(&google_mount),
        )
        .expect("google docs mount point")
        .item;
        assert!(!google_root.read_only);
        assert!(!google_mount_point.read_only);
    }

    #[test]
    fn mount_point_identifier_is_not_a_materializable_entity_identifier() {
        assert!(matches!(
            super::entity_identifier("mount:notion-main"),
            Err(LocalityError::InvalidState(_))
        ));
    }

    #[test]
    fn directory_entities_refresh_with_directory_child_container() {
        let mount_id = MountId::new("gmail-main");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount_with_connector(&mount_id, "gmail"))
            .expect("save mount");
        store
            .save_entity(EntityRecord {
                mount_id: mount_id.clone(),
                remote_id: RemoteId::new("gmail-folder:inbox"),
                kind: EntityKind::Directory,
                title: "inbox".to_string(),
                path: "inbox".into(),
                hydration: HydrationState::Stub,
                content_hash: None,
                remote_edited_at: Some("folder:inbox".to_string()),
            })
            .expect("save inbox");

        let entities = store.list_entities(&mount_id).expect("entities");
        let container = super::child_container_for_identifier(
            &store
                .get_mount(&mount_id)
                .expect("mount load")
                .expect("mount"),
            &entities,
            "gmail-folder:inbox",
        )
        .expect("child container");

        assert_eq!(
            container,
            Some(locality_connector::ChildContainer::DirectoryChildren(
                RemoteId::new("gmail-folder:inbox")
            ))
        );
    }

    #[test]
    fn gmail_inbox_message_rejects_virtual_write_without_dirtying_entity() {
        let mount_id = MountId::new("gmail-main");
        let content_root = temp_root("loc-gmail-readonly").join("content/gmail-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount_with_connector(&mount_id, "gmail"))
            .expect("save mount");
        store
            .save_entity(EntityRecord {
                mount_id: mount_id.clone(),
                remote_id: RemoteId::new("msg-inbox-1"),
                kind: EntityKind::Page,
                title: "Inbox One".to_string(),
                path: "inbox/2026-07-14-inbox-one-msg-inbox-1.md".into(),
                hydration: HydrationState::Hydrated,
                content_hash: None,
                remote_edited_at: Some("gmail:msg-inbox-1:1".to_string()),
            })
            .expect("save entity");

        let error = commit_virtual_fs_write(
            &mut store,
            &content_root,
            &mount_id,
            "msg-inbox-1",
            b"edited",
        )
        .expect_err("inbox writes are rejected");

        assert!(
            matches!(error, LocalityError::Unsupported(message) if message.contains("Gmail inbox and sent items are read-only"))
        );
        let entity = store
            .get_entity(&mount_id, &RemoteId::new("msg-inbox-1"))
            .expect("load entity")
            .expect("entity");
        assert_eq!(entity.hydration, HydrationState::Hydrated);
    }

    #[test]
    fn gmail_draft_folder_accepts_virtual_create() {
        let mount_id = MountId::new("gmail-main");
        let content_root = temp_root("loc-gmail-draft-create").join("content/gmail-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount_with_connector(&mount_id, "gmail"))
            .expect("save mount");
        store
            .save_entity(EntityRecord {
                mount_id: mount_id.clone(),
                remote_id: RemoteId::new("gmail-folder:draft"),
                kind: EntityKind::Directory,
                title: "draft".to_string(),
                path: "draft".into(),
                hydration: HydrationState::Stub,
                content_hash: None,
                remote_edited_at: Some("folder:draft".to_string()),
            })
            .expect("save draft folder");

        let report = create_virtual_fs_file(
            &mut store,
            &content_root,
            &mount_id,
            "gmail-folder:draft",
            "reply.md",
        )
        .expect("draft create");

        assert_eq!(report.item.path, "draft/reply.md");
        assert_eq!(report.item.entity_kind, Some(EntityKind::Page));
    }

    #[test]
    fn gmail_draft_folder_rejects_nested_directory_create_without_mutation_or_file() {
        let mount_id = MountId::new("gmail-main");
        let state_root = temp_root("loc-gmail-draft-nested-dir-create");
        let content_root = state_root.join("content/gmail-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount_with_connector(&mount_id, "gmail"))
            .expect("save mount");
        store
            .save_entity(EntityRecord {
                mount_id: mount_id.clone(),
                remote_id: RemoteId::new("gmail-folder:draft"),
                kind: EntityKind::Directory,
                title: "draft".to_string(),
                path: "draft".into(),
                hydration: HydrationState::Stub,
                content_hash: None,
                remote_edited_at: Some("folder:draft".to_string()),
            })
            .expect("save draft folder");

        let error = create_virtual_fs_directory(
            &mut store,
            &content_root,
            &mount_id,
            "gmail-folder:draft",
            "reply",
        )
        .expect_err("nested Gmail draft directories are rejected");

        assert!(
            matches!(error, LocalityError::Unsupported(message) if message.contains("Gmail creates are only supported directly inside draft/"))
        );
        assert!(
            store
                .list_virtual_mutations(&mount_id)
                .expect("list mutations")
                .is_empty()
        );
        assert!(!content_root.join("draft/reply/page.md").exists());
        let draft_folder = store
            .get_entity(&mount_id, &RemoteId::new("gmail-folder:draft"))
            .expect("load draft folder")
            .expect("draft folder");
        assert_eq!(draft_folder.hydration, HydrationState::Stub);

        let report = create_virtual_fs_file(
            &mut store,
            &content_root,
            &mount_id,
            "gmail-folder:draft",
            "reply.md",
        )
        .expect("direct Gmail draft files are still accepted");

        assert_eq!(report.item.path, "draft/reply.md");
        assert_eq!(
            std::fs::read(content_root.join("draft/reply.md")).expect("read pending draft"),
            b""
        );
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
    fn gmail_folder_read_only_metadata_reflects_direct_draft_create_policy() {
        let mount_id = MountId::new("gmail-main");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount_with_connector(&mount_id, "gmail"))
            .expect("save mount");
        for (remote_id, title, path) in [
            ("gmail-folder:draft", "draft", "draft"),
            ("gmail-folder:inbox", "inbox", "inbox"),
            ("gmail-folder:sent", "sent", "sent"),
            ("gmail-folder:draft/nested", "nested", "draft/nested"),
        ] {
            store
                .save_entity(EntityRecord {
                    mount_id: mount_id.clone(),
                    remote_id: RemoteId::new(remote_id),
                    kind: EntityKind::Directory,
                    title: title.to_string(),
                    path: path.into(),
                    hydration: HydrationState::Stub,
                    content_hash: None,
                    remote_edited_at: Some(format!("folder:{path}")),
                })
                .expect("save folder");
        }
        store
            .save_entity(EntityRecord {
                mount_id: mount_id.clone(),
                remote_id: RemoteId::new("msg-draft-1"),
                kind: EntityKind::Page,
                title: "Reply".to_string(),
                path: "draft/reply.md".into(),
                hydration: HydrationState::Hydrated,
                content_hash: None,
                remote_edited_at: Some("gmail:msg-draft-1:1".to_string()),
            })
            .expect("save draft message");

        assert!(
            !virtual_fs_item(&store, &mount_id, "gmail-folder:draft")
                .expect("draft item")
                .item
                .read_only
        );
        assert!(
            virtual_fs_item(&store, &mount_id, "gmail-folder:inbox")
                .expect("inbox item")
                .item
                .read_only
        );
        assert!(
            virtual_fs_item(&store, &mount_id, "gmail-folder:sent")
                .expect("sent item")
                .item
                .read_only
        );
        assert!(
            virtual_fs_item(&store, &mount_id, "gmail-folder:draft/nested")
                .expect("nested draft folder item")
                .item
                .read_only
        );

        let draft_children =
            virtual_fs_children(&store, &mount_id, "gmail-folder:draft").expect("draft children");
        let page_child = draft_children
            .children
            .iter()
            .find(|child| child.identifier == "msg-draft-1")
            .expect("draft page file");
        assert_eq!(page_child.filename, "reply.md");
        assert_eq!(page_child.kind, VirtualFsItemKind::File);
        assert!(!page_child.read_only);
    }

    #[test]
    fn google_calendar_folder_read_only_metadata_reflects_direct_draft_create_policy() {
        let mount_id = MountId::new("google-calendar-main");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount_with_connector(&mount_id, "google-calendar"))
            .expect("save mount");
        for (remote_id, title, path) in [
            ("google-calendar-folder:draft", "draft", "draft"),
            ("google-calendar-folder:events", "events", "events"),
            (
                "google-calendar-folder:draft/nested",
                "nested",
                "draft/nested",
            ),
        ] {
            store
                .save_entity(EntityRecord {
                    mount_id: mount_id.clone(),
                    remote_id: RemoteId::new(remote_id),
                    kind: EntityKind::Directory,
                    title: title.to_string(),
                    path: path.into(),
                    hydration: HydrationState::Stub,
                    content_hash: None,
                    remote_edited_at: Some(format!("folder:{path}")),
                })
                .expect("save folder");
        }

        assert!(
            !virtual_fs_item(&store, &mount_id, "google-calendar-folder:draft")
                .expect("draft item")
                .item
                .read_only
        );
        assert!(
            virtual_fs_item(&store, &mount_id, "google-calendar-folder:events")
                .expect("events item")
                .item
                .read_only
        );
        assert!(
            virtual_fs_item(&store, &mount_id, "google-calendar-folder:draft/nested")
                .expect("nested draft folder item")
                .item
                .read_only
        );
    }

    #[test]
    fn source_folder_read_only_metadata_reflects_create_parent_kinds() {
        let notion_mount_id = MountId::new("notion-main");
        let mut notion_store = InMemoryStateStore::new();
        notion_store
            .save_mount(virtual_mount(&notion_mount_id))
            .expect("save notion mount");
        notion_store
            .save_entity(EntityRecord::new(
                notion_mount_id.clone(),
                RemoteId::new("page-1"),
                EntityKind::Page,
                "Page",
                "Page/page.md",
            ))
            .expect("save notion page");
        notion_store
            .save_entity(EntityRecord::new(
                notion_mount_id.clone(),
                RemoteId::new("database-1"),
                EntityKind::Database,
                "Database",
                "Database",
            ))
            .expect("save notion database");

        let notion_page_children =
            virtual_fs_children(&notion_store, &notion_mount_id, "mount:notion-main")
                .expect("notion mount point children");
        let notion_page_folder = notion_page_children
            .children
            .iter()
            .find(|child| child.identifier == "children:page-1")
            .expect("notion page folder");
        assert!(!notion_page_folder.read_only);
        assert!(
            !virtual_fs_item(&notion_store, &notion_mount_id, "database-1")
                .expect("notion database item")
                .item
                .read_only
        );

        let google_mount_id = MountId::new("google-docs-main");
        let mut google_store = InMemoryStateStore::new();
        google_store
            .save_mount(virtual_mount_with_connector(
                &google_mount_id,
                "google-docs",
            ))
            .expect("save google docs mount");
        google_store
            .save_entity(EntityRecord::new(
                google_mount_id.clone(),
                RemoteId::new("folder-1"),
                EntityKind::Directory,
                "Folder",
                "Folder",
            ))
            .expect("save google docs folder");
        google_store
            .save_entity(EntityRecord::new(
                google_mount_id.clone(),
                RemoteId::new("doc-1"),
                EntityKind::Page,
                "Doc",
                "Doc/page.md",
            ))
            .expect("save google docs page");

        assert!(
            !virtual_fs_item(&google_store, &google_mount_id, "folder-1")
                .expect("google docs folder item")
                .item
                .read_only
        );
        let google_root_children =
            virtual_fs_children(&google_store, &google_mount_id, "mount:google-docs-main")
                .expect("google docs mount point children");
        let google_page_folder = google_root_children
            .children
            .iter()
            .find(|child| child.identifier == "children:doc-1")
            .expect("google docs page folder");
        assert!(google_page_folder.read_only);
    }

    #[test]
    fn gmail_draft_message_rejects_rename_or_move_to_non_draft_without_dirtying_entity() {
        for (folder_remote_id, folder_path, new_filename) in [
            ("gmail-folder:inbox", "inbox", "reply-msg-draft-1.md"),
            ("gmail-folder:sent", "sent", "sent-reply-msg-draft-1.md"),
            ("gmail-folder:archive", "archive", "reply-msg-draft-1.md"),
        ] {
            let mount_id = MountId::new("gmail-main");
            let state_root = temp_root(&format!("loc-gmail-rename-{folder_path}"));
            let content_root = state_root.join("content/gmail-main/files");
            let mut store = InMemoryStateStore::new();
            store
                .save_mount(virtual_mount_with_connector(&mount_id, "gmail"))
                .expect("save mount");
            store
                .save_entity(EntityRecord {
                    mount_id: mount_id.clone(),
                    remote_id: RemoteId::new("gmail-folder:draft"),
                    kind: EntityKind::Directory,
                    title: "draft".to_string(),
                    path: "draft".into(),
                    hydration: HydrationState::Stub,
                    content_hash: None,
                    remote_edited_at: Some("folder:draft".to_string()),
                })
                .expect("save draft folder");
            store
                .save_entity(EntityRecord {
                    mount_id: mount_id.clone(),
                    remote_id: RemoteId::new(folder_remote_id),
                    kind: EntityKind::Directory,
                    title: folder_path.to_string(),
                    path: folder_path.into(),
                    hydration: HydrationState::Stub,
                    content_hash: None,
                    remote_edited_at: Some(format!("folder:{folder_path}")),
                })
                .expect("save target folder");
            store
                .save_entity(EntityRecord {
                    mount_id: mount_id.clone(),
                    remote_id: RemoteId::new("msg-draft-1"),
                    kind: EntityKind::Page,
                    title: "Draft One".to_string(),
                    path: "draft/reply-msg-draft-1.md".into(),
                    hydration: HydrationState::Hydrated,
                    content_hash: None,
                    remote_edited_at: Some("gmail:msg-draft-1:1".to_string()),
                })
                .expect("save draft message");

            let error = rename_virtual_fs_item(
                &mut store,
                &content_root,
                &mount_id,
                "msg-draft-1",
                folder_remote_id,
                new_filename,
            )
            .expect_err("rename into non-draft Gmail folder is rejected");

            assert!(
                matches!(error, LocalityError::Unsupported(message) if message.contains("Gmail"))
            );
            let entity = store
                .get_entity(&mount_id, &RemoteId::new("msg-draft-1"))
                .expect("load entity")
                .expect("entity");
            assert_eq!(entity.path, PathBuf::from("draft/reply-msg-draft-1.md"));
            assert_eq!(entity.hydration, HydrationState::Hydrated);
            assert!(
                store
                    .list_virtual_mutations(&mount_id)
                    .expect("list mutations")
                    .is_empty()
            );

            let _ = std::fs::remove_dir_all(state_root);
        }
    }

    #[test]
    fn google_docs_page_child_container_rejects_virtual_create_parent_kind() {
        let mount_id = MountId::new("google-docs-main");
        let state_root = temp_root("loc-google-docs-page-child-create");
        let content_root = state_root.join("content/google-docs-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(
                MountConfig::new(mount_id.clone(), "google-docs", "/tmp/loc/google-docs")
                    .projection(ProjectionMode::LinuxFuse),
            )
            .expect("save mount");
        store
            .save_entity(EntityRecord {
                mount_id: mount_id.clone(),
                remote_id: RemoteId::new("doc-page"),
                kind: EntityKind::Page,
                title: "Doc".to_string(),
                path: "Doc/page.md".into(),
                hydration: HydrationState::Hydrated,
                content_hash: None,
                remote_edited_at: Some("google-docs:doc-page:1".to_string()),
            })
            .expect("save page");

        let error = create_virtual_fs_file(
            &mut store,
            &content_root,
            &mount_id,
            "children:doc-page",
            "Nested.md",
        )
        .expect_err("google docs page children are not create parents");

        assert!(matches!(
            error,
            LocalityError::Unsupported(
                "new virtual filesystem files cannot be created under this source item"
            )
        ));
        assert!(
            store
                .list_virtual_mutations(&mount_id)
                .expect("list mutations")
                .is_empty()
        );
        assert!(!content_root.join("Doc/Nested.md").exists());

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn rename_rejects_destination_parents_that_do_not_accept_creates_without_dirtying_entity() {
        for (connector, mount_name, source_path, target_parent_id, target_kind, target_path) in [
            (
                "gmail",
                "gmail-main-nested",
                "draft/reply.md",
                "target-parent",
                EntityKind::Directory,
                "draft/nested",
            ),
            (
                "gmail",
                "gmail-main-page-child",
                "draft/reply.md",
                "children:target-parent",
                EntityKind::Page,
                "draft/parent/page.md",
            ),
            (
                "google-docs",
                "google-docs-main-page-child",
                "Moving/page.md",
                "children:target-parent",
                EntityKind::Page,
                "Parent/page.md",
            ),
        ] {
            let mount_id = MountId::new(mount_name);
            let state_root = temp_root(&format!("loc-rename-destination-policy-{mount_name}"));
            let content_root = state_root.join(format!("content/{mount_name}/files"));
            let mut store = InMemoryStateStore::new();
            store
                .save_mount(virtual_mount_with_connector(&mount_id, connector))
                .expect("save mount");
            store
                .save_entity(EntityRecord {
                    mount_id: mount_id.clone(),
                    remote_id: RemoteId::new("target-parent"),
                    kind: target_kind,
                    title: "Target Parent".to_string(),
                    path: target_path.into(),
                    hydration: HydrationState::Hydrated,
                    content_hash: None,
                    remote_edited_at: Some(format!("{connector}:target-parent:1")),
                })
                .expect("save target parent");
            store
                .save_entity(EntityRecord {
                    mount_id: mount_id.clone(),
                    remote_id: RemoteId::new("moving-page"),
                    kind: EntityKind::Page,
                    title: "Moving".to_string(),
                    path: source_path.into(),
                    hydration: HydrationState::Hydrated,
                    content_hash: None,
                    remote_edited_at: Some(format!("{connector}:moving-page:1")),
                })
                .expect("save moving page");

            let error = rename_virtual_fs_item(
                &mut store,
                &content_root,
                &mount_id,
                "moving-page",
                target_parent_id,
                "Moved.md",
            )
            .expect_err("rename destination parent policy rejects move");

            assert!(matches!(error, LocalityError::Unsupported(_)));
            let entity = store
                .get_entity(&mount_id, &RemoteId::new("moving-page"))
                .expect("load entity")
                .expect("entity");
            assert_eq!(entity.path, PathBuf::from(source_path));
            assert_eq!(entity.hydration, HydrationState::Hydrated);
            assert!(
                store
                    .list_virtual_mutations(&mount_id)
                    .expect("list mutations")
                    .is_empty()
            );

            let _ = std::fs::remove_dir_all(state_root);
        }
    }

    #[test]
    fn google_docs_directory_accepts_virtual_move_destination_parent() {
        let mount_id = MountId::new("google-docs-main");
        let state_root = temp_root("loc-google-docs-directory-move");
        let content_root = state_root.join("content/google-docs-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount_with_connector(&mount_id, "google-docs"))
            .expect("save mount");
        store
            .save_entity(EntityRecord {
                mount_id: mount_id.clone(),
                remote_id: RemoteId::new("folder-parent"),
                kind: EntityKind::Directory,
                title: "Folder".to_string(),
                path: "Folder".into(),
                hydration: HydrationState::Stub,
                content_hash: None,
                remote_edited_at: Some("google-docs:folder-parent:1".to_string()),
            })
            .expect("save folder");
        store
            .save_entity(EntityRecord {
                mount_id: mount_id.clone(),
                remote_id: RemoteId::new("doc-moving"),
                kind: EntityKind::Page,
                title: "Moving".to_string(),
                path: "Moving/page.md".into(),
                hydration: HydrationState::Hydrated,
                content_hash: None,
                remote_edited_at: Some("google-docs:doc-moving:1".to_string()),
            })
            .expect("save moving doc");
        store
            .save_shadow(
                &mount_id,
                ShadowDocument::from_synced_body(
                    RemoteId::new("doc-moving"),
                    "",
                    1,
                    std::iter::empty(),
                )
                .expect("moving doc shadow"),
            )
            .expect("save moving doc shadow");

        let moved = rename_virtual_fs_item(
            &mut store,
            &content_root,
            &mount_id,
            "doc-moving",
            "folder-parent",
            "Moved.md",
        )
        .expect("google docs directory accepts moves");

        assert_eq!(moved.path, "Folder/Moved.md");
        let entity = store
            .get_entity(&mount_id, &RemoteId::new("doc-moving"))
            .expect("load entity")
            .expect("entity");
        assert_eq!(entity.path, PathBuf::from("Folder/Moved.md"));
        assert_eq!(entity.hydration, HydrationState::Dirty);
        let mutation = store
            .get_virtual_mutation(&mount_id, "move:doc-moving")
            .expect("load move mutation")
            .expect("move mutation");
        assert_eq!(
            mutation.parent_remote_id.as_ref().map(RemoteId::as_str),
            Some("folder-parent")
        );

        let _ = std::fs::remove_dir_all(state_root);
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

    #[cfg(target_os = "macos")]
    #[test]
    fn guidance_materialization_repairs_legacy_cache_before_writing_current_root() {
        let home = temp_root("loc-virtual-fs-guidance-legacy");
        let state_root = home.join(".loc");
        let mount_id = MountId::new("notion-main");
        let content_root = virtual_fs_content_root(&state_root, &mount_id);
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount(&mount_id))
            .expect("save mount");
        let legacy_path = home
            .join("Library")
            .join("Group Containers")
            .join("C484HB7Q6S.group.ai.codeflash.locality")
            .join("content")
            .join(&mount_id.0)
            .join("files")
            .join("Roadmap/page.md");
        fs::create_dir_all(legacy_path.parent().expect("legacy parent")).expect("legacy parent");
        fs::write(&legacy_path, b"legacy dirty bytes").expect("write legacy cache");

        let report = materialize_virtual_fs_guidance_with_content_root(
            &store,
            &content_root,
            &mount_id,
            AGENTS_GUIDANCE_IDENTIFIER,
        )
        .expect("materialize guidance")
        .expect("guidance report");

        assert!(PathBuf::from(&report.path).ends_with(".loc-guidance/AGENTS.md"));
        assert_eq!(
            fs::read(content_root.join("Roadmap/page.md")).expect("read repaired cache"),
            b"legacy dirty bytes"
        );
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn loc_asset_cache_files_are_projected_read_only_from_content_root() {
        let mount_id = MountId::new("gmail-main");
        let state_root = temp_root("loc-virtual-fs-loc-asset-cache");
        let content_root = state_root.join("content/gmail-main/files");
        let mut store = InMemoryStateStore::new();
        let mount = virtual_mount_with_connector(&mount_id, "gmail");
        store.save_mount(mount.clone()).expect("save mount");

        let attachment_path = PathBuf::from(".loc/gmail/attachments/msg-1/screenshot-attach-1.png");
        let absolute_attachment = content_root.join(&attachment_path);
        std::fs::create_dir_all(absolute_attachment.parent().expect("attachment parent"))
            .expect("create attachment parent");
        std::fs::write(&absolute_attachment, b"\x89PNG\r\n\x1a\nattachment bytes")
            .expect("write attachment");

        let mount_children = virtual_fs_children_with_content_root(
            &store,
            &content_root,
            &mount_id,
            &mount_point_identifier(&mount),
        )
        .expect("list mount children");
        let loc = mount_children
            .children
            .iter()
            .find(|child| child.filename == ".loc")
            .expect(".loc cache folder");
        assert_eq!(loc.identifier, "path:.loc");
        assert_eq!(loc.kind, VirtualFsItemKind::Folder);
        assert!(loc.read_only);

        let attachment_children = virtual_fs_children_with_content_root(
            &store,
            &content_root,
            &mount_id,
            "path:.loc/gmail/attachments/msg-1",
        )
        .expect("list attachment cache children");
        let attachment = attachment_children
            .children
            .iter()
            .find(|child| child.filename == "screenshot-attach-1.png")
            .expect("attachment file");
        assert_eq!(
            attachment.identifier,
            "asset-cache:.loc/gmail/attachments/msg-1/screenshot-attach-1.png"
        );
        assert_eq!(attachment.kind, VirtualFsItemKind::File);
        assert!(attachment.read_only);
        assert_eq!(attachment.hydration, Some(HydrationState::Hydrated));
        assert_eq!(
            attachment.materialized_path.as_deref(),
            Some(absolute_attachment.to_string_lossy().as_ref())
        );
        assert_eq!(attachment.byte_size, Some(24));

        let attachment_item = virtual_fs_item_with_content_root(
            &store,
            &content_root,
            &mount_id,
            &attachment.identifier,
        )
        .expect("look up attachment item");
        assert_eq!(
            attachment_item.item.materialized_path.as_deref(),
            Some(absolute_attachment.to_string_lossy().as_ref())
        );
        assert_eq!(attachment_item.item.byte_size, Some(24));

        let materialized = materialize_virtual_fs_item_with_content_root(
            &mut store,
            &FailingHydrationSource,
            &content_root,
            &mount_id,
            &attachment.identifier,
        )
        .expect("materialize cached attachment");
        assert_eq!(
            materialized.outcome,
            VirtualFsMaterializeOutcome::AlreadyMaterialized
        );
        assert_eq!(materialized.hydration, HydrationState::Hydrated);
        assert_eq!(PathBuf::from(&materialized.path), absolute_attachment);

        assert!(matches!(
            commit_virtual_fs_write(
                &mut store,
                &content_root,
                &mount_id,
                &attachment.identifier,
                b"edited",
            )
            .expect_err("cached asset writes are rejected"),
            LocalityError::Unsupported("downloaded asset cache files are read-only")
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
    fn flat_markdown_pages_list_as_files_in_virtual_fs() {
        let mount_id = MountId::new("slack-main");
        let content_root = temp_root("loc-virtual-fs-flat-pages").join("content/slack-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount_with_connector(&mount_id, "slack"))
            .expect("save mount");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("slack-folder:channels"),
                EntityKind::Directory,
                "channels",
                "channels",
            ))
            .expect("save channels folder");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("slack-conversation:C123"),
                EntityKind::Directory,
                "general-C123",
                "channels/general-C123",
            ))
            .expect("save conversation folder");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    RemoteId::new("slack-recent:C123"),
                    EntityKind::Page,
                    "recent",
                    "channels/general-C123/recent.md",
                )
                .with_hydration(HydrationState::Hydrated),
            )
            .expect("save recent page");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    RemoteId::new("slack-users"),
                    EntityKind::Page,
                    "users",
                    "users.md",
                )
                .with_hydration(HydrationState::Hydrated),
            )
            .expect("save users page");
        std::fs::create_dir_all(content_root.join("channels/general-C123"))
            .expect("content parent");
        std::fs::write(
            content_root.join("channels/general-C123/recent.md"),
            b"recent messages",
        )
        .expect("recent cache");

        let mount_children = virtual_fs_children(&store, &mount_id, "mount:slack-main")
            .expect("mount point children");
        let users = mount_children
            .children
            .iter()
            .find(|child| child.identifier == "slack-users")
            .expect("users file");
        assert_eq!(users.filename, "users.md");
        assert_eq!(users.kind, VirtualFsItemKind::File);
        assert!(
            !mount_children
                .children
                .iter()
                .any(|child| child.identifier == "children:slack-users")
        );

        let conversation_children =
            virtual_fs_children(&store, &mount_id, "slack-conversation:C123")
                .expect("conversation children");
        let recent = conversation_children
            .children
            .iter()
            .find(|child| child.identifier == "slack-recent:C123")
            .expect("recent file");
        assert_eq!(recent.filename, "recent.md");
        assert_eq!(recent.kind, VirtualFsItemKind::File);
        assert!(
            !conversation_children
                .children
                .iter()
                .any(|child| child.identifier == "children:slack-recent:C123")
        );

        let recent_with_cache = virtual_fs_item_with_content_root(
            &store,
            &content_root,
            &mount_id,
            "slack-recent:C123",
        )
        .expect("recent item with cache");
        let expected_cache_path = content_root
            .join("channels/general-C123/recent.md")
            .to_string_lossy()
            .to_string();
        assert_eq!(
            recent_with_cache.item.materialized_path.as_deref(),
            Some(expected_cache_path.as_str())
        );
    }

    #[test]
    fn database_children_with_schema_include_rows_and_schema_file() {
        let mount_id = MountId::new("notion-main");
        let state_root = temp_root("loc-virtual-fs-database-schema-children");
        let content_root = state_root.join("content/notion-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount(&mount_id))
            .expect("save mount");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("database-1"),
                EntityKind::Database,
                "Sales CRM",
                "sales-crm",
            ))
            .expect("save database");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("row-1"),
                EntityKind::Page,
                "Adrenaline",
                "sales-crm/adrenaline/page.md",
            ))
            .expect("save row");
        std::fs::create_dir_all(content_root.join("sales-crm")).expect("create schema parent");
        std::fs::write(
            content_root.join("sales-crm/_schema.yaml"),
            "type: notion_database_schema\n",
        )
        .expect("write schema");

        let report =
            virtual_fs_children_with_content_root(&store, &content_root, &mount_id, "database-1")
                .expect("list database children");

        assert!(
            report
                .children
                .iter()
                .any(|child| child.identifier == "schema:database-1"
                    && child.filename == "_schema.yaml"
                    && child.kind == VirtualFsItemKind::File),
            "schema file missing from database children: {:?}",
            report.children
        );
        assert!(
            report
                .children
                .iter()
                .any(|child| child.identifier == "children:row-1"
                    && child.filename == "adrenaline"
                    && child.kind == VirtualFsItemKind::Folder),
            "row folder missing from database children: {:?}",
            report.children
        );

        let _ = std::fs::remove_dir_all(state_root);
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
    fn refresh_children_updates_cached_mount_point_metadata() {
        let mount_id = MountId::new("notion-main");
        let mut store = InMemoryStateStore::new();
        let mount = virtual_mount(&mount_id);
        store.save_mount(mount.clone()).expect("save mount");
        let mut connector = StaticChildrenConnector {
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
            database_schema: None,
            complete: true,
        };
        let mount_point_root = mount_point_identifier(&mount);

        let saved =
            refresh_virtual_fs_children(&mut store, &connector, &mount_id, &mount_point_root)
                .expect("refresh children");
        assert_eq!(saved.saved, 1);
        assert!(saved.changed);

        let report = virtual_fs_children(&store, &mount_id, &mount_point_root)
            .expect("mount point children");
        assert_eq!(report.children.len(), 3);
        assert_eq!(report.children[0].identifier, AGENTS_GUIDANCE_IDENTIFIER);
        assert_eq!(report.children[0].kind, VirtualFsItemKind::File);
        assert_eq!(report.children[1].identifier, CLAUDE_GUIDANCE_IDENTIFIER);
        assert_eq!(report.children[1].kind, VirtualFsItemKind::File);
        assert_eq!(report.children[2].identifier, "children:page-root");
        assert_eq!(report.children[2].kind, VirtualFsItemKind::Folder);

        connector.entries.push(TreeEntry {
            mount_id: mount_id.clone(),
            remote_id: RemoteId::new("page-new"),
            kind: EntityKind::Page,
            title: "New Page".to_string(),
            path: "new-page/page.md".into(),
            hydration: HydrationState::Stub,
            content_hash: None,
            remote_edited_at: None,
            stub_frontmatter: None,
        });
        let saved =
            refresh_virtual_fs_children(&mut store, &connector, &mount_id, &mount_point_root)
                .expect("refresh cached children");
        assert_eq!(saved.saved, 2);
        assert!(saved.changed);

        let report = virtual_fs_children(&store, &mount_id, &mount_point_root)
            .expect("updated mount point children");
        assert!(report.children.iter().any(|child| {
            child.identifier == "children:page-new" && child.kind == VirtualFsItemKind::Folder
        }));
    }

    #[test]
    fn refresh_children_prunes_clean_stale_virtual_child_subtree() {
        let mount_id = MountId::new("notion-main");
        let mut store = InMemoryStateStore::new();
        let mount = virtual_mount(&mount_id);
        store.save_mount(mount.clone()).expect("save mount");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("old-page"),
                EntityKind::Page,
                "Old",
                "old/page.md",
            ))
            .expect("save old page");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("old-child"),
                EntityKind::Page,
                "Old Child",
                "old/child/page.md",
            ))
            .expect("save old child");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    RemoteId::new("dirty-page"),
                    EntityKind::Page,
                    "Dirty",
                    "dirty/page.md",
                )
                .with_hydration(HydrationState::Dirty),
            )
            .expect("save dirty page");
        let connector = StaticChildrenConnector {
            entries: vec![TreeEntry {
                mount_id: mount_id.clone(),
                remote_id: RemoteId::new("new-page"),
                kind: EntityKind::Page,
                title: "New Page".to_string(),
                path: "new/page.md".into(),
                hydration: HydrationState::Stub,
                content_hash: None,
                remote_edited_at: None,
                stub_frontmatter: None,
            }],
            expected_parent_path: PathBuf::new(),
            database_schema: None,
            complete: true,
        };

        let saved = refresh_virtual_fs_children(
            &mut store,
            &connector,
            &mount_id,
            &mount_point_identifier(&mount),
        )
        .expect("refresh children");

        assert_eq!(saved.saved, 1);
        assert!(saved.changed);
        assert!(
            store
                .get_entity(&mount_id, &RemoteId::new("old-page"))
                .expect("old page lookup")
                .is_none()
        );
        assert!(
            store
                .get_entity(&mount_id, &RemoteId::new("old-child"))
                .expect("old child lookup")
                .is_none()
        );
        assert!(
            store
                .get_entity(&mount_id, &RemoteId::new("dirty-page"))
                .expect("dirty page lookup")
                .is_some()
        );
    }

    #[test]
    fn incremental_refresh_merges_without_pruning_omitted_children() {
        let mount_id = MountId::new("granola-main");
        let mut store = InMemoryStateStore::new();
        let mount = virtual_mount_with_connector(&mount_id, "granola");
        store.save_mount(mount.clone()).expect("save mount");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("old-meeting"),
                EntityKind::Directory,
                "Old meeting",
                "Old meeting",
            ))
            .expect("save old meeting");
        let connector = StaticChildrenConnector {
            entries: vec![TreeEntry {
                mount_id: mount_id.clone(),
                remote_id: RemoteId::new("recent-meeting"),
                kind: EntityKind::Directory,
                title: "Recent meeting".to_string(),
                path: "Recent meeting".into(),
                hydration: HydrationState::Stub,
                content_hash: None,
                remote_edited_at: None,
                stub_frontmatter: None,
            }],
            expected_parent_path: PathBuf::new(),
            database_schema: None,
            complete: false,
        };

        let saved = refresh_virtual_fs_children(
            &mut store,
            &connector,
            &mount_id,
            &mount_point_identifier(&mount),
        )
        .expect("merge incremental root refresh");

        assert_eq!(saved.saved, 1);
        assert!(saved.changed);
        assert!(
            store
                .get_entity(&mount_id, &RemoteId::new("old-meeting"))
                .expect("old meeting lookup")
                .is_some()
        );
        assert!(
            store
                .get_entity(&mount_id, &RemoteId::new("recent-meeting"))
                .expect("recent meeting lookup")
                .is_some()
        );
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
                path: "Root/Tasks/Fix login bug/page.md".into(),
                hydration: HydrationState::Stub,
                content_hash: None,
                remote_edited_at: None,
                stub_frontmatter: None,
            }],
            expected_parent_path: PathBuf::from("Root/Tasks"),
            database_schema: None,
            complete: true,
        };

        let saved = refresh_virtual_fs_children(&mut store, &connector, &mount_id, "database-1")
            .expect("refresh database rows");

        assert_eq!(saved.saved, 1);
        assert!(saved.changed);
        let row = store
            .get_entity(&mount_id, &RemoteId::new("row-1"))
            .expect("get row")
            .expect("row");
        assert_eq!(row.path, PathBuf::from("Root/Tasks/Fix login bug/page.md"));
        let children = virtual_fs_children(&store, &mount_id, "database-1").expect("children");
        assert!(
            children
                .children
                .iter()
                .any(|child| child.identifier == "children:row-1"
                    && child.path == "Root/Tasks/Fix login bug")
        );
    }

    #[test]
    fn refresh_database_children_with_content_root_writes_schema_cache() {
        let mount_id = MountId::new("notion-main");
        let state_root = temp_root("loc-virtual-fs-refresh-database-schema-cache");
        let content_root = state_root.join("content/notion-main/files");
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
        let schema = "type: notion_database_schema\nproperties:\n  Name:\n    type: title\n";
        let connector = StaticChildrenConnector {
            entries: vec![TreeEntry {
                mount_id: mount_id.clone(),
                remote_id: RemoteId::new("row-1"),
                kind: EntityKind::Page,
                title: "Fix login bug".to_string(),
                path: "Root/Tasks/Fix login bug/page.md".into(),
                hydration: HydrationState::Stub,
                content_hash: None,
                remote_edited_at: None,
                stub_frontmatter: None,
            }],
            expected_parent_path: PathBuf::from("Root/Tasks"),
            database_schema: Some((RemoteId::new("database-1"), schema.to_string())),
            complete: true,
        };

        let saved = refresh_virtual_fs_children_with_content_root(
            &mut store,
            &connector,
            &content_root,
            &mount_id,
            "database-1",
        )
        .expect("refresh database rows and schema");

        assert_eq!(saved.saved, 1);
        assert!(saved.changed);
        assert_eq!(
            std::fs::read_to_string(content_root.join("Root/Tasks/_schema.yaml"))
                .expect("schema cache"),
            schema
        );
        let children =
            virtual_fs_children_with_content_root(&store, &content_root, &mount_id, "database-1")
                .expect("children");
        assert!(
            children
                .children
                .iter()
                .any(|child| child.identifier == "schema:database-1"
                    && child.filename == "_schema.yaml"
                    && child.kind == VirtualFsItemKind::File),
            "schema file missing from database children: {:?}",
            children.children
        );
        assert!(
            children
                .children
                .iter()
                .any(|child| child.identifier == "children:row-1"
                    && child.filename == "Fix login bug"
                    && child.kind == VirtualFsItemKind::Folder),
            "row folder missing from database children: {:?}",
            children.children
        );

        let saved_again = refresh_virtual_fs_children_with_content_root(
            &mut store,
            &connector,
            &content_root,
            &mount_id,
            "database-1",
        )
        .expect("refresh unchanged database rows and schema");
        assert_eq!(saved_again.saved, 1);
        assert!(!saved_again.changed);

        let _ = std::fs::remove_dir_all(state_root);
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
                path: "Root/Tasks/Fix login bug/page.md".into(),
                hydration: HydrationState::Stub,
                content_hash: None,
                remote_edited_at: None,
                stub_frontmatter: None,
            }],
            expected_parent_path: PathBuf::from("Root/Tasks"),
            database_schema: None,
            complete: true,
        };

        let saved = refresh_virtual_fs_children(&mut store, &connector, &mount_id, "database-1")
            .expect("refresh database rows");

        assert_eq!(saved.saved, 1);
        assert_eq!(saved.changed, false);
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
                path: "Root/Tasks/Fix login bug/page.md".into(),
                hydration: HydrationState::Stub,
                content_hash: None,
                remote_edited_at: None,
                stub_frontmatter: None,
            }],
            expected_parent_path: PathBuf::from("Root/Tasks"),
            database_schema: None,
            complete: true,
        };

        let saved = refresh_virtual_fs_children(&mut store, &connector, &mount_id, "database-1")
            .expect("refresh database rows");

        assert_eq!(saved.saved, 1);
        assert!(saved.changed);
        let row = store
            .get_entity(&mount_id, &RemoteId::new("row-1"))
            .expect("get row")
            .expect("row");
        assert_eq!(row.path, PathBuf::from("Root/Tasks/Fix login bug/page.md"));
        assert_eq!(row.hydration, HydrationState::Stub);
        assert_eq!(row.content_hash, None);
    }

    struct StaticChildrenConnector {
        entries: Vec<TreeEntry>,
        expected_parent_path: PathBuf,
        database_schema: Option<(RemoteId, String)>,
        complete: bool,
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
            Ok(if self.complete {
                ListChildrenResult::complete(self.entries.clone())
            } else {
                ListChildrenResult::incremental(self.entries.clone())
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

    impl HydrationSource for StaticChildrenConnector {
        fn fetch_render(
            &self,
            _request: &HydrationRequest,
        ) -> locality_core::LocalityResult<HydratedEntity> {
            Err(locality_core::LocalityError::NotImplemented(
                "fixture hydrate",
            ))
        }

        fn fetch_database_schema_yaml(
            &self,
            database_id: &RemoteId,
        ) -> locality_core::LocalityResult<Option<String>> {
            Ok(self
                .database_schema
                .as_ref()
                .filter(|(id, _)| id == database_id)
                .map(|(_, schema)| schema.clone()))
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

    struct MarkdownAssetHydrationSource;

    impl HydrationSource for MarkdownAssetHydrationSource {
        fn fetch_render(
            &self,
            request: &HydrationRequest,
        ) -> locality_core::LocalityResult<HydratedEntity> {
            let frontmatter = format!(
                "loc:\n  id: {}\n  type: page\ntitle: Summary\n",
                request.remote_id.0
            );
            let body = "Hydrated Markdown asset.\n".to_string();
            let shadow = ShadowDocument::from_synced_body(
                request.remote_id.clone(),
                body.clone(),
                frontmatter.lines().count() + 3,
                [RemoteId::new("block-1")],
            )
            .expect("asset shadow")
            .with_frontmatter(frontmatter.clone());
            Ok(HydratedEntity {
                document: CanonicalDocument::new(frontmatter, body),
                shadow,
                remote_edited_at: Some("2026-07-14T00:00:00Z".to_string()),
                assets: Vec::new(),
            })
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
    fn markdown_asset_materializes_as_a_file_on_open() {
        let mount_id = MountId::new("granola-main");
        let remote_id = RemoteId::new("note-1:summary");
        let state_root = temp_root("loc-virtual-fs-markdown-asset");
        let content_root = state_root.join("content/granola-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount_with_connector(&mount_id, "granola").read_only(true))
            .expect("save mount");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                remote_id.clone(),
                EntityKind::Asset,
                "Summary",
                "Meeting/summary.md",
            ))
            .expect("save asset");

        let report = materialize_virtual_fs_item_with_content_root(
            &mut store,
            &MarkdownAssetHydrationSource,
            &content_root,
            &mount_id,
            remote_id.as_str(),
        )
        .expect("materialize Markdown asset");

        assert_eq!(report.outcome, VirtualFsMaterializeOutcome::Hydrated);
        assert_eq!(report.hydration, HydrationState::Hydrated);
        assert_eq!(
            PathBuf::from(&report.path),
            content_root.join("Meeting/summary.md")
        );
        assert!(
            std::fs::read_to_string(&report.path)
                .expect("read materialized asset")
                .contains("Hydrated Markdown asset.")
        );
        assert_eq!(
            store
                .get_entity(&mount_id, &remote_id)
                .expect("load asset")
                .expect("asset exists")
                .kind,
            EntityKind::Asset
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

    #[cfg(target_os = "macos")]
    #[test]
    fn default_macos_content_cache_stays_in_state_root_outside_app_sandbox() {
        let home = std::env::temp_dir().join(format!("loc-virtual-fs-home-{}", std::process::id()));
        let state_root = home.join(".loc");

        assert_eq!(
            super::virtual_fs_content_base_from_config(&state_root, None, Some(&home), false),
            state_root.join("content")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn default_macos_content_cache_uses_app_group_inside_app_sandbox() {
        let home = std::env::temp_dir().join(format!(
            "loc-virtual-fs-sandbox-home-{}",
            std::process::id()
        ));
        let state_root = home.join(".loc");

        assert_eq!(
            super::virtual_fs_content_base_from_config(&state_root, None, Some(&home), true),
            home.join("Library")
                .join("Group Containers")
                .join("C484HB7Q6S.group.ai.codeflash.locality")
                .join("content")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn content_root_override_wins_inside_app_sandbox() {
        let home = std::env::temp_dir().join(format!(
            "loc-virtual-fs-override-home-{}",
            std::process::id()
        ));
        let state_root = home.join(".loc");
        let override_root = home.join("override-content");

        assert_eq!(
            super::virtual_fs_content_base_from_config(
                &state_root,
                Some(&override_root),
                Some(&home),
                true
            ),
            override_root
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pending_create_listing_does_not_expose_legacy_app_group_content_path_outside_sandbox() {
        let home = std::env::temp_dir().join(format!(
            "loc-virtual-fs-legacy-pending-home-{}",
            std::process::id()
        ));
        let state_root = home.join(".loc");

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
            .expect("save parent page");
        let legacy_path = home
            .join("Library")
            .join("Group Containers")
            .join("C484HB7Q6S.group.ai.codeflash.locality")
            .join("content")
            .join(&mount_id.0)
            .join("files")
            .join("Home/Draft.md");
        fs::create_dir_all(legacy_path.parent().expect("legacy parent")).expect("legacy parent");
        fs::write(&legacy_path, b"legacy app-group bytes").expect("write legacy cache");
        store
            .save_virtual_mutation(VirtualMutationRecord {
                mount_id: mount_id.clone(),
                local_id: "local:draft".to_string(),
                mutation_kind: VirtualMutationKind::Create,
                target_remote_id: None,
                parent_remote_id: Some(RemoteId::new("page-root")),
                original_path: None,
                projected_path: PathBuf::from("Home/Draft.md"),
                title: "Draft".to_string(),
                content_path: Some(legacy_path.clone()),
                created_at: "1".to_string(),
                updated_at: "1".to_string(),
            })
            .expect("save pending mutation");

        let children =
            virtual_fs_children(&store, &mount_id, "children:page-root").expect("children");
        let draft = children
            .children
            .iter()
            .find(|child| child.identifier == "local:draft")
            .expect("pending draft");
        assert_eq!(draft.materialized_path, None);
        assert_eq!(draft.byte_size, None);
        assert_ne!(
            draft.materialized_path.as_deref(),
            Some(legacy_path.to_string_lossy().as_ref())
        );

        let current_content_root = virtual_fs_content_root(&state_root, &mount_id);
        let rooted = virtual_fs_children_with_content_root(
            &store,
            &current_content_root,
            &mount_id,
            "children:page-root",
        )
        .expect("children with content root");
        let draft = rooted
            .children
            .iter()
            .find(|child| child.identifier == "local:draft")
            .expect("pending draft");
        assert_eq!(
            draft.materialized_path.as_deref(),
            Some(
                current_content_root
                    .join("Home/Draft.md")
                    .to_string_lossy()
                    .as_ref()
            )
        );
        assert_eq!(draft.byte_size, Some(22));
        assert_eq!(
            fs::read(current_content_root.join("Home/Draft.md")).expect("read repaired cache"),
            b"legacy app-group bytes"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn materialize_pending_create_uses_legacy_cache_without_reading_visible_provider_file() {
        let home = std::env::temp_dir().join(format!(
            "loc-virtual-fs-legacy-materialize-home-{}",
            std::process::id()
        ));
        let state_root = home.join(".loc");
        let visible_root = home.join("visible");
        let mount_id = MountId::new("notion-main");
        let current_content_root = virtual_fs_content_root(&state_root, &mount_id);
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(
                MountConfig::new(mount_id.clone(), "notion", &visible_root)
                    .projection(ProjectionMode::LinuxFuse),
            )
            .expect("save mount");
        let visible_path = visible_root.join("Home/Draft.md");
        fs::create_dir_all(visible_path.parent().expect("visible parent")).expect("visible parent");
        fs::write(&visible_path, b"visible pending body").expect("write visible file");
        let legacy_path = home
            .join("Library")
            .join("Group Containers")
            .join("C484HB7Q6S.group.ai.codeflash.locality")
            .join("content")
            .join(&mount_id.0)
            .join("files")
            .join("Home/Draft.md");
        fs::create_dir_all(legacy_path.parent().expect("legacy parent")).expect("legacy parent");
        fs::write(&legacy_path, b"legacy app-group bytes").expect("write legacy cache");
        store
            .save_virtual_mutation(VirtualMutationRecord {
                mount_id: mount_id.clone(),
                local_id: "local:draft".to_string(),
                mutation_kind: VirtualMutationKind::Create,
                target_remote_id: None,
                parent_remote_id: None,
                original_path: None,
                projected_path: PathBuf::from("Home/Draft.md"),
                title: "Draft".to_string(),
                content_path: Some(legacy_path),
                created_at: "1".to_string(),
                updated_at: "1".to_string(),
            })
            .expect("save pending mutation");

        let report = materialize_virtual_fs_item_with_content_root(
            &mut store,
            &FailingHydrationSource,
            &current_content_root,
            &mount_id,
            "local:draft",
        )
        .expect("materialize pending create");

        let current_path = current_content_root.join("Home/Draft.md");
        assert_eq!(PathBuf::from(&report.path), current_path);
        assert_eq!(
            report.outcome,
            VirtualFsMaterializeOutcome::AlreadyMaterialized
        );
        assert_eq!(report.hydration, HydrationState::Dirty);
        assert_eq!(
            fs::read(current_content_root.join("Home/Draft.md")).expect("read current cache"),
            b"legacy app-group bytes"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn materialize_dirty_entity_recovers_legacy_app_group_cache_outside_sandbox() {
        let home = std::env::temp_dir().join(format!(
            "loc-virtual-fs-legacy-dirty-home-{}",
            std::process::id()
        ));
        let state_root = home.join(".loc");
        let visible_root = home.join("visible");
        let mount_id = MountId::new("notion-main");
        let current_content_root = virtual_fs_content_root(&state_root, &mount_id);
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(
                MountConfig::new(mount_id.clone(), "notion", &visible_root)
                    .projection(ProjectionMode::LinuxFuse),
            )
            .expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    RemoteId::new("page-1"),
                    EntityKind::Page,
                    "Roadmap",
                    "Roadmap/page.md",
                )
                .with_hydration(HydrationState::Dirty),
            )
            .expect("save dirty entity");
        let legacy_path = home
            .join("Library")
            .join("Group Containers")
            .join("C484HB7Q6S.group.ai.codeflash.locality")
            .join("content")
            .join(&mount_id.0)
            .join("files")
            .join("Roadmap/page.md");
        fs::create_dir_all(legacy_path.parent().expect("legacy parent")).expect("legacy parent");
        fs::write(&legacy_path, b"legacy dirty entity bytes").expect("write legacy cache");

        let report = materialize_virtual_fs_item_with_content_root(
            &mut store,
            &FailingHydrationSource,
            &current_content_root,
            &mount_id,
            "page-1",
        )
        .expect("materialize dirty entity");

        let current_path = current_content_root.join("Roadmap/page.md");
        assert_eq!(PathBuf::from(&report.path), current_path);
        assert_eq!(
            report.outcome,
            VirtualFsMaterializeOutcome::AlreadyMaterialized
        );
        assert_eq!(report.hydration, HydrationState::Dirty);
        assert_eq!(
            fs::read(&current_path).expect("read migrated cache"),
            b"legacy dirty entity bytes"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn legacy_macos_content_repair_memoizes_failures_without_retrying_legacy_root() {
        let home = temp_root("loc-virtual-fs-legacy-repair-failure");
        let state_root = home.join(".loc");
        let mount_id = MountId::new("notion-main");
        let current_content_root = virtual_fs_content_root(&state_root, &mount_id);
        let legacy_path = home
            .join("Library")
            .join("Group Containers")
            .join("C484HB7Q6S.group.ai.codeflash.locality")
            .join("content")
            .join(&mount_id.0)
            .join("files")
            .join("Roadmap/page.md");
        fs::create_dir_all(legacy_path.parent().expect("legacy parent")).expect("legacy parent");
        fs::write(&legacy_path, b"legacy dirty bytes").expect("write legacy cache");
        let blocking_current_path = current_content_root.join("Roadmap");
        fs::create_dir_all(blocking_current_path.parent().expect("current parent"))
            .expect("current parent");
        fs::write(&blocking_current_path, b"not a directory").expect("write blocking file");

        let first = repair_legacy_macos_content_root(&state_root, &mount_id)
            .expect_err("first repair should fail");
        fs::remove_file(&blocking_current_path).expect("remove blocking file");
        fs::remove_dir_all(home.join("Library")).expect("remove legacy root");

        let second = repair_legacy_macos_content_root(&state_root, &mount_id)
            .expect_err("second repair should return cached failure");

        assert_eq!(second, first);
        assert!(!current_content_root.join("Roadmap/page.md").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pending_page_temp_create_uses_current_content_root_not_legacy_content_path() {
        let home = std::env::temp_dir().join(format!(
            "loc-virtual-fs-legacy-temp-home-{}",
            std::process::id()
        ));
        let state_root = home.join(".loc");

        let mount_id = MountId::new("notion-main");
        let current_content_root = virtual_fs_content_root(&state_root, &mount_id);
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
        let legacy_path = home
            .join("Library")
            .join("Group Containers")
            .join("C484HB7Q6S.group.ai.codeflash.locality")
            .join("content")
            .join(&mount_id.0)
            .join("files")
            .join("Home/Draft/page.md");
        fs::create_dir_all(legacy_path.parent().expect("legacy parent")).expect("legacy parent");
        fs::write(&legacy_path, b"legacy app-group bytes").expect("write legacy cache");
        store
            .save_virtual_mutation(VirtualMutationRecord {
                mount_id: mount_id.clone(),
                local_id: "local:draft".to_string(),
                mutation_kind: VirtualMutationKind::Create,
                target_remote_id: None,
                parent_remote_id: Some(RemoteId::new("page-root")),
                original_path: None,
                projected_path: PathBuf::from("Home/Draft/page.md"),
                title: "Draft".to_string(),
                content_path: Some(legacy_path.clone()),
                created_at: "1".to_string(),
                updated_at: "1".to_string(),
            })
            .expect("save pending mutation");

        let temp = create_virtual_fs_file(
            &mut store,
            &current_content_root,
            &mount_id,
            "children:local:draft",
            "page.md.tmp.1234.abcd",
        )
        .expect("create page temp");

        let expected = current_content_root.join("Home/Draft/page.md.tmp.1234.abcd");
        assert_eq!(
            temp.item.materialized_path.as_deref(),
            Some(expected.to_string_lossy().as_ref())
        );
        assert_eq!(temp.item.byte_size, None);
        assert_ne!(
            temp.item.materialized_path.as_deref(),
            Some(legacy_path.to_string_lossy().as_ref())
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

    #[cfg(target_os = "macos")]
    #[test]
    fn commit_write_repairs_legacy_cache_before_overwriting_current_root() {
        let home = temp_root("loc-virtual-fs-commit-legacy");
        let state_root = home.join(".loc");
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let content_root = virtual_fs_content_root(&state_root, &mount_id);
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
                .with_hydration(HydrationState::Dirty),
            )
            .expect("save entity");
        let legacy_path = home
            .join("Library")
            .join("Group Containers")
            .join("C484HB7Q6S.group.ai.codeflash.locality")
            .join("content")
            .join(&mount_id.0)
            .join("files")
            .join("Roadmap/page.md");
        fs::create_dir_all(legacy_path.parent().expect("legacy parent")).expect("legacy parent");
        fs::write(&legacy_path, b"legacy dirty bytes").expect("write legacy cache");
        let second_legacy_path = home
            .join("Library")
            .join("Group Containers")
            .join("C484HB7Q6S.group.ai.codeflash.locality")
            .join("content")
            .join(&mount_id.0)
            .join("files")
            .join("Other/page.md");
        fs::create_dir_all(second_legacy_path.parent().expect("second legacy parent"))
            .expect("second legacy parent");
        fs::write(&second_legacy_path, b"second legacy bytes").expect("write second legacy cache");
        let current_path = content_root.join("Roadmap/page.md");
        assert!(!current_path.exists());

        let report =
            commit_virtual_fs_write(&mut store, &content_root, &mount_id, "page-1", b"edited")
                .expect("commit write");

        assert_eq!(report.bytes_written, 6);
        assert_eq!(
            fs::read(&current_path).expect("read current cache"),
            b"edited"
        );
        assert_eq!(
            fs::read(content_root.join("Other/page.md")).expect("read second repaired cache"),
            b"second legacy bytes"
        );
        let _ = fs::remove_dir_all(home);
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
    fn pending_database_draft_projects_folder_and_writable_schema() {
        let mount_id = MountId::new("notion-main");
        let state_root = temp_root("loc-virtual-fs-database-draft");
        let content_root = state_root.join("content/notion-main/files");
        let schema_path = content_root.join("Home/Tasks/_schema.yaml");
        std::fs::create_dir_all(schema_path.parent().expect("schema parent"))
            .expect("create schema parent");
        std::fs::write(&schema_path, b"loc:\n  type: notion_database_schema\n")
            .expect("write schema");
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
            .save_virtual_mutation(VirtualMutationRecord {
                mount_id: mount_id.clone(),
                local_id: "local:database-draft".to_string(),
                mutation_kind: VirtualMutationKind::Create,
                target_remote_id: None,
                parent_remote_id: Some(RemoteId::new("page-root")),
                original_path: None,
                projected_path: PathBuf::from("Home/Tasks/_schema.yaml"),
                title: "Tasks".to_string(),
                content_path: Some(schema_path.clone()),
                created_at: "1".to_string(),
                updated_at: "1".to_string(),
            })
            .expect("save database draft");

        let parent = virtual_fs_children_with_content_root(
            &store,
            &content_root,
            &mount_id,
            "children:page-root",
        )
        .expect("parent children");
        let database = parent
            .children
            .iter()
            .find(|item| item.filename == "Tasks")
            .expect("database folder");
        assert_eq!(database.kind, VirtualFsItemKind::Folder);
        assert_eq!(database.entity_kind, Some(EntityKind::Database));

        let children = virtual_fs_children_with_content_root(
            &store,
            &content_root,
            &mount_id,
            &database.identifier,
        )
        .expect("database children");
        let schema = children
            .children
            .iter()
            .find(|item| item.filename == "_schema.yaml")
            .expect("schema item");
        assert_eq!(schema.identifier, "local:database-draft");
        assert!(!schema.read_only);
        assert_eq!(schema.content_type, "public.yaml");

        commit_virtual_fs_write(
            &mut store,
            &content_root,
            &mount_id,
            &schema.identifier,
            b"loc:\n  type: notion_database_schema\ntitle: Tasks\n",
        )
        .expect("edit draft schema");
        assert_eq!(
            std::fs::read_to_string(schema_path).expect("read schema"),
            "loc:\n  type: notion_database_schema\ntitle: Tasks\n"
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
        commit_virtual_fs_write(
            &mut store,
            &content_root,
            &mount_id,
            &temp.identifier,
            b"# Draft\n\nBody from an atomic save.",
        )
        .expect("write atomic temp contents");
        let renamed = rename_virtual_fs_item(
            &mut store,
            &content_root,
            &mount_id,
            &temp.identifier,
            &created.identifier,
            "page.md",
        )
        .expect("rename atomic temp to page.md");
        assert_eq!(renamed.identifier, page.identifier);
        assert_eq!(renamed.item.filename, "page.md");
        assert_eq!(renamed.item.path, "Home/Draft/page.md");
        assert_eq!(
            std::fs::read(content_root.join("Home/Draft/page.md"))
                .expect("read pending page cache"),
            b"# Draft\n\nBody from an atomic save."
        );
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
    fn atomic_temp_rename_over_existing_page_commits_page_write() {
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let state_root = temp_root("loc-virtual-fs-atomic-temp-rename");
        let content_root = state_root.join("content/notion-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount(&mount_id))
            .expect("save mount");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                remote_id.clone(),
                EntityKind::Page,
                "Home",
                "Home/page.md",
            ))
            .expect("save page");

        let temp = create_virtual_fs_file(
            &mut store,
            &content_root,
            &mount_id,
            "children:page-1",
            "page.md.tmp.1234.abcd",
        )
        .expect("create atomic temp");
        commit_virtual_fs_write(
            &mut store,
            &content_root,
            &mount_id,
            &temp.identifier,
            b"# Home\n\nEdited through atomic save.",
        )
        .expect("write atomic temp");

        let renamed = rename_virtual_fs_item(
            &mut store,
            &content_root,
            &mount_id,
            &temp.identifier,
            "children:page-1",
            "page.md",
        )
        .expect("rename atomic temp over page.md");

        assert_eq!(renamed.identifier, "page-1");
        assert_eq!(renamed.item.filename, "page.md");
        assert_eq!(
            std::fs::read(content_root.join("Home/page.md")).expect("read page cache"),
            b"# Home\n\nEdited through atomic save."
        );
        assert!(!content_root.join("Home/page.md.tmp.1234.abcd").exists());
        assert!(
            store
                .get_virtual_mutation(&mount_id, &temp.identifier)
                .expect("get temp mutation")
                .is_none()
        );
        assert_eq!(
            store
                .get_entity(&mount_id, &remote_id)
                .expect("get page")
                .expect("page")
                .hydration,
            HydrationState::Dirty
        );

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn create_directory_reconciles_existing_database_folder() {
        let mount_id = MountId::new("notion-main");
        let state_root = temp_root("loc-virtual-fs-reconcile-existing-database-dir");
        let content_root = state_root.join("content/notion-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount(&mount_id))
            .expect("save mount");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("database-1"),
                EntityKind::Database,
                "Sales CRM",
                "sales-crm",
            ))
            .expect("save database");

        let reconciled = create_virtual_fs_directory(
            &mut store,
            &content_root,
            &mount_id,
            "mount:notion-main",
            "sales-crm",
        )
        .expect("reconcile existing database directory");

        assert_eq!(reconciled.identifier, "database-1");
        assert_eq!(reconciled.item.filename, "sales-crm");
        assert_eq!(reconciled.item.entity_kind, Some(EntityKind::Database));
        assert_eq!(reconciled.item.kind, VirtualFsItemKind::Folder);
        assert!(
            store
                .list_virtual_mutations(&mount_id)
                .expect("list mutations")
                .is_empty()
        );

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn create_directory_reconciles_existing_page_folder() {
        let mount_id = MountId::new("notion-main");
        let state_root = temp_root("loc-virtual-fs-reconcile-existing-page-dir");
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
                "Home",
                "Home/page.md",
            ))
            .expect("save page");

        let reconciled = create_virtual_fs_directory(
            &mut store,
            &content_root,
            &mount_id,
            "mount:notion-main",
            "Home",
        )
        .expect("reconcile existing page directory");

        assert_eq!(reconciled.identifier, "children:page-1");
        assert_eq!(reconciled.item.filename, "Home");
        assert_eq!(reconciled.item.kind, VirtualFsItemKind::Folder);
        assert!(
            store
                .list_virtual_mutations(&mount_id)
                .expect("list mutations")
                .is_empty()
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
    fn move_pending_page_directory_to_notion_workspace_root_is_rejected() {
        let mount_id = MountId::new("notion-main");
        let state_root = temp_root("loc-virtual-fs-rename-pending-dir-root");
        let content_root = state_root.join("content/notion-main/files");
        let mut store = InMemoryStateStore::new();
        let mount = virtual_mount(&mount_id);
        store.save_mount(mount.clone()).expect("save mount");
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

        let error = rename_virtual_fs_item(
            &mut store,
            &content_root,
            &mount_id,
            &created.identifier,
            &mount_point_identifier(&mount),
            "Draft",
        )
        .expect_err("Notion workspace root pending create move is rejected");

        assert!(matches!(error, LocalityError::Unsupported(_)));
        assert!(content_root.join("Home/Draft/page.md").exists());
        assert!(!content_root.join("Draft/page.md").exists());
        let local_id = created
            .identifier
            .strip_prefix("children:")
            .expect("pending page directory id");
        let mutation = store
            .get_virtual_mutation(&mount_id, local_id)
            .expect("get mutation")
            .expect("mutation");
        assert_eq!(mutation.projected_path, PathBuf::from("Home/Draft/page.md"));
        assert_eq!(
            mutation.parent_remote_id.as_ref().map(RemoteId::as_str),
            Some("page-root")
        );

        let _ = std::fs::remove_dir_all(state_root);
    }

    fn seed_pending_linear_issue(
        store: &mut InMemoryStateStore,
        content_root: &Path,
        mount_id: &MountId,
        local_id: &str,
        parent_remote_id: &str,
        projected_path: &str,
        title: &str,
    ) -> String {
        let projected_path = PathBuf::from(projected_path);
        let content_path = content_root.join(&projected_path);
        if let Some(parent) = content_path.parent() {
            std::fs::create_dir_all(parent).expect("create pending content parent");
        }
        std::fs::write(&content_path, b"").expect("write pending issue cache");
        store
            .save_virtual_mutation(VirtualMutationRecord {
                mount_id: mount_id.clone(),
                local_id: local_id.to_string(),
                mutation_kind: VirtualMutationKind::Create,
                target_remote_id: None,
                parent_remote_id: Some(RemoteId::new(parent_remote_id)),
                original_path: None,
                projected_path: projected_path.clone(),
                title: title.to_string(),
                content_path: Some(content_path),
                created_at: "before".to_string(),
                updated_at: "before".to_string(),
            })
            .expect("save pending issue mutation");

        if projected_path.file_name().and_then(|name| name.to_str())
            == Some(locality_core::path_projection::PAGE_DOCUMENT_FILENAME)
        {
            format!("children:{local_id}")
        } else {
            local_id.to_string()
        }
    }

    fn save_linear_status_parents(store: &mut InMemoryStateStore, mount_id: &MountId) {
        for (id, title, path) in [
            ("team-state:team-a:todo", "Todo", "Teams/Team A/Issues/Todo"),
            ("team-state:team-b:done", "Done", "Teams/Team B/Issues/Done"),
        ] {
            store
                .save_entity(EntityRecord::new(
                    mount_id.clone(),
                    RemoteId::new(id),
                    EntityKind::Directory,
                    title,
                    path,
                ))
                .expect("save Linear status parent");
        }
    }

    #[test]
    fn linear_pending_page_directory_move_preserves_canonical_title_and_cached_bytes() {
        let mount_id = MountId::new("linear-main");
        let state_root = temp_root("loc-virtual-fs-linear-pending-dir-move");
        let content_root = state_root.join("content/linear-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount_with_connector(&mount_id, "linear"))
            .expect("save mount");
        save_linear_status_parents(&mut store, &mount_id);
        let local_id = "local:linear-dir-move";
        let created_identifier = seed_pending_linear_issue(
            &mut store,
            &content_root,
            &mount_id,
            local_id,
            "team-state:team-a:todo",
            "Teams/Team A/Issues/Todo/ENG-1-old-path/page.md",
            "ENG-1-old-path",
        );
        let mut mutation = store
            .get_virtual_mutation(&mount_id, local_id)
            .expect("get mutation")
            .expect("mutation");
        mutation.title = "Explicit issue title".to_string();
        store
            .save_virtual_mutation(mutation)
            .expect("save mutation");
        let bytes = b"---\ntitle: \"Edited title in cache\"\nstatus: Started\n---\nEdited body\n";
        std::fs::write(
            content_root.join("Teams/Team A/Issues/Todo/ENG-1-old-path/page.md"),
            bytes,
        )
        .expect("write cache");

        rename_virtual_fs_item(
            &mut store,
            &content_root,
            &mount_id,
            &created_identifier,
            "team-state:team-b:done",
            "ENG-1-new-path",
        )
        .expect("move pending issue");

        assert_eq!(
            std::fs::read(content_root.join("Teams/Team B/Issues/Done/ENG-1-new-path/page.md"))
                .expect("read moved cache"),
            bytes
        );
        let mutation = store
            .get_virtual_mutation(&mount_id, local_id)
            .expect("get mutation")
            .expect("mutation");
        assert_eq!(mutation.title, "Explicit issue title");
        assert_eq!(
            mutation.parent_remote_id,
            Some(RemoteId::new("team-state:team-b:done"))
        );
        assert_eq!(
            mutation.projected_path,
            PathBuf::from("Teams/Team B/Issues/Done/ENG-1-new-path/page.md")
        );
        assert_eq!(mutation.mutation_kind, VirtualMutationKind::Create);
        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn linear_pending_page_directory_rename_preserves_canonical_title_and_cached_bytes() {
        let mount_id = MountId::new("linear-main");
        let state_root = temp_root("loc-virtual-fs-linear-pending-dir-rename");
        let content_root = state_root.join("content/linear-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount_with_connector(&mount_id, "linear"))
            .expect("save mount");
        save_linear_status_parents(&mut store, &mount_id);
        let local_id = "local:linear-dir-rename";
        let created_identifier = seed_pending_linear_issue(
            &mut store,
            &content_root,
            &mount_id,
            local_id,
            "team-state:team-a:todo",
            "Teams/Team A/Issues/Todo/ENG-2-old/page.md",
            "ENG-2-old",
        );
        let mut mutation = store
            .get_virtual_mutation(&mount_id, local_id)
            .expect("get mutation")
            .expect("mutation");
        mutation.title = "Explicit directory title".to_string();
        store
            .save_virtual_mutation(mutation)
            .expect("save mutation");
        let bytes = b"---\ntitle: \"Cache title edit\"\n---\nBody edit\n";
        std::fs::write(
            content_root.join("Teams/Team A/Issues/Todo/ENG-2-old/page.md"),
            bytes,
        )
        .expect("write cache");

        rename_virtual_fs_item(
            &mut store,
            &content_root,
            &mount_id,
            &created_identifier,
            "team-state:team-a:todo",
            "ENG-2-new",
        )
        .expect("rename pending issue");

        assert_eq!(
            std::fs::read(content_root.join("Teams/Team A/Issues/Todo/ENG-2-new/page.md"))
                .expect("read cache"),
            bytes
        );
        let mutation = store
            .get_virtual_mutation(&mount_id, local_id)
            .expect("get mutation")
            .expect("mutation");
        assert_eq!(mutation.title, "Explicit directory title");
        assert_eq!(
            mutation.projected_path,
            PathBuf::from("Teams/Team A/Issues/Todo/ENG-2-new/page.md")
        );
        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn linear_pending_page_directory_rename_without_cache_fails_before_state_changes() {
        let mount_id = MountId::new("linear-main");
        let state_root = temp_root("loc-virtual-fs-linear-pending-dir-missing-cache");
        let content_root = state_root.join("content/linear-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount_with_connector(&mount_id, "linear"))
            .expect("save mount");
        save_linear_status_parents(&mut store, &mount_id);
        let local_id = "local:linear-dir-missing-cache";
        let created_identifier = seed_pending_linear_issue(
            &mut store,
            &content_root,
            &mount_id,
            local_id,
            "team-state:team-a:todo",
            "Teams/Team A/Issues/Todo/ENG-7-old/page.md",
            "ENG-7-old",
        );
        let old_cache = content_root.join("Teams/Team A/Issues/Todo/ENG-7-old/page.md");
        std::fs::remove_file(&old_cache).expect("remove pending cache");
        let before_entities = store.list_entities(&mount_id).expect("list entities");
        let before_mutation = store
            .get_virtual_mutation(&mount_id, local_id)
            .expect("get mutation")
            .expect("mutation");

        let error = rename_virtual_fs_item(
            &mut store,
            &content_root,
            &mount_id,
            &created_identifier,
            "team-state:team-a:todo",
            "ENG-7-new",
        )
        .expect_err("missing pending page cache must fail");

        assert_eq!(
            error,
            LocalityError::InvalidState(format!(
                "pending create `{local_id}` must be materialized before it can be moved or renamed"
            ))
        );
        assert_eq!(
            store.list_entities(&mount_id).expect("list entities"),
            before_entities
        );
        assert_eq!(
            store
                .get_virtual_mutation(&mount_id, local_id)
                .expect("get mutation"),
            Some(before_mutation)
        );
        assert!(!old_cache.exists());
        assert!(
            !content_root
                .join("Teams/Team A/Issues/Todo/ENG-7-new")
                .exists()
        );
        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn linear_pending_flat_rename_is_blocked_before_state_changes() {
        let mount_id = MountId::new("linear-main");
        let state_root = temp_root("loc-virtual-fs-linear-pending-flat-missing-cache");
        let content_root = state_root.join("content/linear-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount_with_connector(&mount_id, "linear"))
            .expect("save mount");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("team-a"),
                EntityKind::Page,
                "Team A",
                "Team A/page.md",
            ))
            .expect("save parent");
        let created_identifier = seed_pending_linear_issue(
            &mut store,
            &content_root,
            &mount_id,
            "local:linear-flat-missing-cache",
            "team-a",
            "Team A/ENG-8-old.md",
            "ENG-8-old",
        );
        let old_cache = content_root.join("Team A/ENG-8-old.md");
        std::fs::remove_file(&old_cache).expect("remove pending cache");
        let before_entities = store.list_entities(&mount_id).expect("list entities");
        let before_mutation = store
            .get_virtual_mutation(&mount_id, &created_identifier)
            .expect("get mutation")
            .expect("mutation");

        let error = rename_virtual_fs_item(
            &mut store,
            &content_root,
            &mount_id,
            &created_identifier,
            "children:team-a",
            "ENG-8-new.md",
        )
        .expect_err("flat pending Linear rename must fail");

        assert!(
            matches!(error, LocalityError::Unsupported(message) if message.contains("Linear writes are only supported")),
            "{error:?}"
        );
        assert_eq!(
            store.list_entities(&mount_id).expect("list entities"),
            before_entities
        );
        assert_eq!(
            store
                .get_virtual_mutation(&mount_id, &created_identifier)
                .expect("get mutation"),
            Some(before_mutation)
        );
        assert!(!old_cache.exists());
        assert!(!content_root.join("Team A/ENG-8-new.md").exists());
        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn linear_remote_page_directory_move_preserves_entity_title_and_cached_bytes() {
        let mount_id = MountId::new("linear-main");
        let state_root = temp_root("loc-virtual-fs-linear-remote-dir-move");
        let content_root = state_root.join("content/linear-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount_with_connector(&mount_id, "linear"))
            .expect("save mount");
        save_linear_status_parents(&mut store, &mount_id);
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    RemoteId::new("issue-3"),
                    EntityKind::Page,
                    "Canonical remote title",
                    "Teams/Team A/Issues/Todo/ENG-3-old/page.md",
                )
                .with_hydration(HydrationState::Hydrated),
            )
            .expect("save issue");
        std::fs::create_dir_all(content_root.join("Teams/Team A/Issues/Todo/ENG-3-old"))
            .expect("cache dir");
        let bytes = b"---\nloc:\n  id: issue-3\ntitle: \"Explicit cache title\"\n---\nBody edit\n";
        std::fs::write(
            content_root.join("Teams/Team A/Issues/Todo/ENG-3-old/page.md"),
            bytes,
        )
        .expect("cache");

        rename_virtual_fs_item(
            &mut store,
            &content_root,
            &mount_id,
            "children:issue-3",
            "team-state:team-b:done",
            "ENG-3-new",
        )
        .expect("move issue");

        assert_eq!(
            std::fs::read(content_root.join("Teams/Team B/Issues/Done/ENG-3-new/page.md"))
                .expect("read cache"),
            bytes
        );
        let entity = store
            .get_entity(&mount_id, &RemoteId::new("issue-3"))
            .expect("get entity")
            .expect("entity");
        assert_eq!(entity.title, "Canonical remote title");
        assert_eq!(
            entity.path,
            PathBuf::from("Teams/Team B/Issues/Done/ENG-3-new/page.md")
        );
        let mutation = store
            .get_virtual_mutation(&mount_id, "move:issue-3")
            .expect("get mutation")
            .expect("mutation");
        assert_eq!(mutation.title, "Canonical remote title");
        assert_eq!(
            mutation.original_path,
            Some(PathBuf::from("Teams/Team A/Issues/Todo/ENG-3-old/page.md"))
        );
        assert_eq!(
            mutation.parent_remote_id,
            Some(RemoteId::new("team-state:team-b:done"))
        );
        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn linear_remote_page_directory_rename_preserves_entity_title_and_cached_bytes() {
        let mount_id = MountId::new("linear-main");
        let state_root = temp_root("loc-virtual-fs-linear-remote-dir-rename");
        let content_root = state_root.join("content/linear-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount_with_connector(&mount_id, "linear"))
            .expect("save mount");
        save_linear_status_parents(&mut store, &mount_id);
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    RemoteId::new("issue-4"),
                    EntityKind::Page,
                    "Canonical directory title",
                    "Teams/Team A/Issues/Todo/ENG-4-old/page.md",
                )
                .with_hydration(HydrationState::Hydrated),
            )
            .expect("save issue");
        std::fs::create_dir_all(content_root.join("Teams/Team A/Issues/Todo/ENG-4-old"))
            .expect("cache dir");
        let bytes = b"---\ntitle: \"Edited cache title\"\n---\nEdited body\n";
        std::fs::write(
            content_root.join("Teams/Team A/Issues/Todo/ENG-4-old/page.md"),
            bytes,
        )
        .expect("cache");

        rename_virtual_fs_item(
            &mut store,
            &content_root,
            &mount_id,
            "children:issue-4",
            "team-state:team-a:todo",
            "ENG-4-new",
        )
        .expect("rename issue");

        assert_eq!(
            std::fs::read(content_root.join("Teams/Team A/Issues/Todo/ENG-4-new/page.md"))
                .expect("read cache"),
            bytes
        );
        let entity = store
            .get_entity(&mount_id, &RemoteId::new("issue-4"))
            .expect("get entity")
            .expect("entity");
        assert_eq!(entity.title, "Canonical directory title");
        assert_eq!(
            entity.path,
            PathBuf::from("Teams/Team A/Issues/Todo/ENG-4-new/page.md")
        );
        let mutation = store
            .get_virtual_mutation(&mount_id, "move:issue-4")
            .expect("get mutation")
            .expect("mutation");
        assert_eq!(mutation.title, "Canonical directory title");
        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn linear_pending_moves_remain_retryable_when_cache_publication_fails() {
        for page_directory in [true] {
            let shape = if page_directory { "dir" } else { "flat" };
            let mount_id = MountId::new(format!("linear-pending-{shape}"));
            let state_root = temp_root(&format!("loc-linear-pending-move-failure-{shape}"));
            let content_root = state_root.join(format!("content/{}/files", mount_id.0));
            let mut store = InMemoryStateStore::new();
            store
                .save_mount(virtual_mount_with_connector(&mount_id, "linear"))
                .expect("save mount");
            save_linear_status_parents(&mut store, &mount_id);

            let created_identifier = if page_directory {
                seed_pending_linear_issue(
                    &mut store,
                    &content_root,
                    &mount_id,
                    &format!("local:linear-pending-{shape}"),
                    "team-state:team-a:todo",
                    "Teams/Team A/Issues/Todo/ENG-20-old/page.md",
                    "ENG-20-old",
                )
            } else {
                seed_pending_linear_issue(
                    &mut store,
                    &content_root,
                    &mount_id,
                    &format!("local:linear-pending-{shape}"),
                    "team-a",
                    "Team A/ENG-20-old.md",
                    "ENG-20-old",
                )
            };
            let local_id = created_identifier
                .strip_prefix("children:")
                .unwrap_or(&created_identifier)
                .to_string();
            let source_relative = if page_directory {
                "Teams/Team A/Issues/Todo/ENG-20-old/page.md"
            } else {
                "Team A/ENG-20-old.md"
            };
            let target_relative = if page_directory {
                "Teams/Team B/Issues/Done/ENG-20-new/page.md"
            } else {
                "Team B/ENG-20-new.md"
            };
            let source_cache = content_root.join(source_relative);
            let target_cache = content_root.join(target_relative);
            let blocker = target_cache
                .parent()
                .expect("target issue dir")
                .to_path_buf();
            let bytes = format!("pending-{shape}-bytes").into_bytes();
            std::fs::write(&source_cache, &bytes).expect("write source cache");
            std::fs::create_dir_all(blocker.parent().expect("blocker parent"))
                .expect("create blocker parent");
            std::fs::write(&blocker, b"blocks target directory").expect("write blocker");

            let error = rename_virtual_fs_item(
                &mut store,
                &content_root,
                &mount_id,
                &created_identifier,
                "team-state:team-b:done",
                if page_directory {
                    "ENG-20-new"
                } else {
                    "ENG-20-new.md"
                },
            )
            .expect_err("cache publication fails");
            assert!(matches!(error, LocalityError::Io(_)));
            let interrupted = store
                .get_virtual_mutation(&mount_id, &local_id)
                .expect("get interrupted mutation")
                .expect("interrupted mutation");
            assert_eq!(interrupted.projected_path, PathBuf::from(target_relative));
            assert_eq!(
                interrupted.content_path.as_deref(),
                Some(source_cache.as_path())
            );
            assert_eq!(
                std::fs::read(&source_cache).expect("source survives"),
                bytes
            );
            assert!(!target_cache.exists());

            std::fs::remove_file(&blocker).expect("remove blocker");
            rename_virtual_fs_item(
                &mut store,
                &content_root,
                &mount_id,
                &created_identifier,
                "team-state:team-b:done",
                if page_directory {
                    "ENG-20-new"
                } else {
                    "ENG-20-new.md"
                },
            )
            .expect("retry interrupted move");

            assert!(!source_cache.exists());
            assert_eq!(std::fs::read(&target_cache).expect("target cache"), bytes);
            let finalized = store
                .get_virtual_mutation(&mount_id, &local_id)
                .expect("get finalized mutation")
                .expect("finalized mutation");
            assert_eq!(
                finalized.content_path.as_deref(),
                Some(target_cache.as_path())
            );
            let _ = std::fs::remove_dir_all(state_root);
        }
    }

    #[test]
    fn linear_remote_moves_commit_consistent_state_before_cache_publication() {
        for page_directory in [true] {
            let shape = if page_directory { "dir" } else { "flat" };
            let mount_id = MountId::new(format!("linear-remote-{shape}"));
            let state_root = temp_root(&format!("loc-linear-remote-move-failure-{shape}"));
            let content_root = state_root.join(format!("content/{}/files", mount_id.0));
            let mut store = InMemoryStateStore::new();
            store
                .save_mount(virtual_mount_with_connector(&mount_id, "linear"))
                .expect("save mount");
            save_linear_status_parents(&mut store, &mount_id);
            let remote_id = RemoteId::new(format!("issue-{shape}"));
            let source_relative = if page_directory {
                "Teams/Team A/Issues/Todo/ENG-21-old/page.md"
            } else {
                "Team A/ENG-21-old.md"
            };
            let target_relative = if page_directory {
                "Teams/Team B/Issues/Done/ENG-21-new/page.md"
            } else {
                "Team B/ENG-21-new.md"
            };
            let source_cache = content_root.join(source_relative);
            let target_cache = content_root.join(target_relative);
            let blocker = target_cache
                .parent()
                .expect("target issue dir")
                .to_path_buf();
            store
                .save_entity(
                    EntityRecord::new(
                        mount_id.clone(),
                        remote_id.clone(),
                        EntityKind::Page,
                        "Canonical title",
                        source_relative,
                    )
                    .with_hydration(HydrationState::Hydrated),
                )
                .expect("save issue");
            store
                .save_freshness_state(
                    locality_store::FreshnessStateRecord::new(
                        mount_id.clone(),
                        remote_id.clone(),
                        FreshnessTier::Warm,
                    )
                    .local_change_at("before"),
                )
                .expect("save freshness");
            store
                .save_virtual_mutation(VirtualMutationRecord {
                    mount_id: mount_id.clone(),
                    local_id: format!("rename:{}", remote_id.0),
                    mutation_kind: VirtualMutationKind::Rename,
                    target_remote_id: Some(remote_id.clone()),
                    parent_remote_id: Some(RemoteId::new("team-state:team-a:todo")),
                    original_path: Some(PathBuf::from(source_relative)),
                    projected_path: PathBuf::from(source_relative),
                    title: "Canonical title".to_string(),
                    content_path: Some(source_cache.clone()),
                    created_at: "before".to_string(),
                    updated_at: "before".to_string(),
                })
                .expect("save old rename");
            std::fs::create_dir_all(source_cache.parent().expect("source parent"))
                .expect("create source parent");
            let bytes = format!("remote-{shape}-bytes").into_bytes();
            std::fs::write(&source_cache, &bytes).expect("write source cache");
            std::fs::create_dir_all(blocker.parent().expect("blocker parent"))
                .expect("create blocker parent");
            std::fs::write(&blocker, b"blocks target directory").expect("write blocker");
            let identifier = if page_directory {
                format!("children:{}", remote_id.0)
            } else {
                remote_id.0.clone()
            };

            let error = rename_virtual_fs_item(
                &mut store,
                &content_root,
                &mount_id,
                &identifier,
                "team-state:team-b:done",
                if page_directory {
                    "ENG-21-new"
                } else {
                    "ENG-21-new.md"
                },
            )
            .expect_err("cache publication fails");
            assert!(matches!(error, LocalityError::Io(_)));

            let entity = store
                .get_entity(&mount_id, &remote_id)
                .expect("get entity")
                .expect("entity");
            assert_eq!(entity.path, PathBuf::from(target_relative));
            assert_eq!(entity.hydration, HydrationState::Dirty);
            assert_eq!(
                store
                    .get_freshness_state(&mount_id, &remote_id)
                    .expect("freshness")
                    .expect("freshness")
                    .tier,
                FreshnessTier::Hot
            );
            assert!(
                store
                    .get_virtual_mutation(&mount_id, &format!("rename:{}", remote_id.0))
                    .expect("old rename")
                    .is_none()
            );
            let interrupted = store
                .get_virtual_mutation(&mount_id, &format!("move:{}", remote_id.0))
                .expect("move")
                .expect("move");
            assert_eq!(
                interrupted.content_path.as_deref(),
                Some(source_cache.as_path())
            );
            assert_eq!(
                std::fs::read(&source_cache).expect("source survives"),
                bytes
            );

            std::fs::remove_file(&blocker).expect("remove blocker");
            rename_virtual_fs_item(
                &mut store,
                &content_root,
                &mount_id,
                &identifier,
                "team-state:team-b:done",
                if page_directory {
                    "ENG-21-new"
                } else {
                    "ENG-21-new.md"
                },
            )
            .expect("retry interrupted move");
            assert!(!source_cache.exists());
            assert_eq!(std::fs::read(&target_cache).expect("target cache"), bytes);
            assert_eq!(
                store
                    .get_virtual_mutation(&mount_id, &format!("move:{}", remote_id.0))
                    .expect("move")
                    .expect("move")
                    .content_path
                    .as_deref(),
                Some(target_cache.as_path())
            );
            let _ = std::fs::remove_dir_all(state_root);
        }
    }

    #[test]
    fn remote_move_without_cache_or_shadow_fails_before_state_changes() {
        let mount_id = MountId::new("linear-main");
        let state_root = temp_root("loc-virtual-fs-unmaterialized-move");
        let content_root = state_root.join("content/linear-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount_with_connector(&mount_id, "linear"))
            .expect("save mount");
        save_linear_status_parents(&mut store, &mount_id);
        let before = EntityRecord::new(
            mount_id.clone(),
            RemoteId::new("issue-stub"),
            EntityKind::Page,
            "Canonical title",
            "Teams/Team A/Issues/Todo/ENG-5/page.md",
        )
        .with_hydration(HydrationState::Stub);
        store.save_entity(before.clone()).expect("save issue");

        let error = rename_virtual_fs_item(
            &mut store,
            &content_root,
            &mount_id,
            "children:issue-stub",
            "team-state:team-a:todo",
            "ENG-5-new",
        )
        .expect_err("unmaterialized issue must be rejected");

        assert!(
            matches!(error, LocalityError::InvalidState(message) if message.contains("materialize"))
        );
        assert_eq!(
            store
                .get_entity(&mount_id, &RemoteId::new("issue-stub"))
                .unwrap(),
            Some(before)
        );
        assert!(store.list_virtual_mutations(&mount_id).unwrap().is_empty());
        assert!(
            !content_root
                .join("Teams/Team A/Issues/Todo/ENG-5-new/page.md")
                .exists()
        );
        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn remote_structural_move_without_cache_uses_complete_shadow() {
        let mount_id = MountId::new("linear-main");
        let state_root = temp_root("loc-virtual-fs-shadow-backed-move");
        let content_root = state_root.join("content/linear-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount_with_connector(&mount_id, "linear"))
            .expect("save mount");
        save_linear_status_parents(&mut store, &mount_id);
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    RemoteId::new("issue-shadow"),
                    EntityKind::Page,
                    "Shadow-backed title",
                    "Teams/Team A/Issues/Todo/ENG-6/page.md",
                )
                .with_hydration(HydrationState::Stub),
            )
            .expect("save issue");
        store
            .save_shadow(
                &mount_id,
                ShadowDocument::from_synced_body(
                    RemoteId::new("issue-shadow"),
                    "",
                    5,
                    std::iter::empty(),
                )
                .expect("shadow")
                .with_frontmatter("loc:\n  id: issue-shadow\ntitle: Shadow-backed title\n"),
            )
            .expect("save shadow");

        rename_virtual_fs_item(
            &mut store,
            &content_root,
            &mount_id,
            "children:issue-shadow",
            "team-state:team-b:done",
            "ENG-6-new",
        )
        .expect("shadow-backed structural move");

        let entity = store
            .get_entity(&mount_id, &RemoteId::new("issue-shadow"))
            .unwrap()
            .unwrap();
        assert_eq!(
            entity.path,
            PathBuf::from("Teams/Team B/Issues/Done/ENG-6-new/page.md")
        );
        assert_eq!(entity.title, "Shadow-backed title");
        assert!(
            store
                .get_virtual_mutation(&mount_id, "move:issue-shadow")
                .unwrap()
                .is_some()
        );
        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn rename_existing_page_directory_records_pending_remote_move_and_retitles_cache() {
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
            .get_virtual_mutation(&mount_id, "move:page-child")
            .expect("get move mutation")
            .expect("move mutation");
        assert_eq!(mutation.mutation_kind, VirtualMutationKind::Move);
        assert_eq!(
            mutation.parent_remote_id.as_ref().map(RemoteId::as_str),
            Some("page-root")
        );
        assert_eq!(
            mutation.projected_path,
            PathBuf::from("Home/Renamed Child/page.md")
        );
        assert_eq!(mutation.title, "Renamed Child");

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn move_existing_page_directory_records_pending_remote_move_and_retitles_cache() {
        let mount_id = MountId::new("notion-main");
        let state_root = temp_root("loc-virtual-fs-move-existing-dir");
        let content_root = state_root.join("content/notion-main/files");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(virtual_mount(&mount_id))
            .expect("save mount");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("page-home"),
                EntityKind::Page,
                "Home",
                "Home/page.md",
            ))
            .expect("save home page");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("page-archive"),
                EntityKind::Page,
                "Archive",
                "Archive/page.md",
            ))
            .expect("save archive page");
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

        let moved = rename_virtual_fs_item(
            &mut store,
            &content_root,
            &mount_id,
            "children:page-child",
            "children:page-archive",
            "Moved Child",
        )
        .expect("move existing virtual page directory");

        assert_eq!(moved.identifier, "children:page-child");
        assert_eq!(moved.item.kind, VirtualFsItemKind::Folder);
        assert_eq!(moved.item.filename, "Moved Child");
        assert_eq!(moved.item.path, "Archive/Moved Child");
        assert!(!content_root.join("Home/Child/page.md").exists());
        let moved_cache = std::fs::read_to_string(content_root.join("Archive/Moved Child/page.md"))
            .expect("read moved cache");
        assert!(moved_cache.contains("title: \"Moved Child\""));
        let entity = store
            .get_entity(&mount_id, &RemoteId::new("page-child"))
            .expect("get child entity")
            .expect("child entity");
        assert_eq!(entity.path, PathBuf::from("Archive/Moved Child/page.md"));
        assert_eq!(entity.title, "Moved Child");
        assert_eq!(entity.hydration, HydrationState::Dirty);
        let mutation = store
            .get_virtual_mutation(&mount_id, "move:page-child")
            .expect("get move mutation")
            .expect("move mutation");
        assert_eq!(mutation.mutation_kind, VirtualMutationKind::Move);
        assert_eq!(
            mutation.parent_remote_id.as_ref().map(RemoteId::as_str),
            Some("page-archive")
        );
        assert_eq!(
            mutation.original_path,
            Some(PathBuf::from("Home/Child/page.md"))
        );
        assert_eq!(
            mutation.projected_path,
            PathBuf::from("Archive/Moved Child/page.md")
        );
        assert_eq!(mutation.title, "Moved Child");

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn move_existing_page_directory_to_notion_workspace_root_is_rejected() {
        let mount_id = MountId::new("notion-main");
        let state_root = temp_root("loc-virtual-fs-move-existing-dir-root");
        let content_root = state_root.join("content/notion-main/files");
        let mut store = InMemoryStateStore::new();
        let mount = virtual_mount(&mount_id);
        store.save_mount(mount.clone()).expect("save mount");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("page-home"),
                EntityKind::Page,
                "Things to buy",
                "Things to buy/page.md",
            ))
            .expect("save parent page");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    RemoteId::new("page-grocery"),
                    EntityKind::Page,
                    "Grocery SF Checklist",
                    "Things to buy/Grocery SF Checklist/page.md",
                )
                .with_hydration(HydrationState::Hydrated),
            )
            .expect("save child page");
        std::fs::create_dir_all(content_root.join("Things to buy/Grocery SF Checklist"))
            .expect("create cache dir");
        std::fs::write(
            content_root.join("Things to buy/Grocery SF Checklist/page.md"),
            b"---\nloc:\n  id: page-grocery\n  type: page\ntitle: \"Grocery SF Checklist\"\n---\nBody",
        )
        .expect("write page cache");

        let error = rename_virtual_fs_item(
            &mut store,
            &content_root,
            &mount_id,
            "children:page-grocery",
            &mount_point_identifier(&mount),
            "Grocery SF Checklist",
        )
        .expect_err("Notion workspace root move is rejected");

        assert!(matches!(error, LocalityError::Unsupported(_)));
        assert!(
            content_root
                .join("Things to buy/Grocery SF Checklist/page.md")
                .exists()
        );
        assert!(!content_root.join("Grocery SF Checklist/page.md").exists());
        let entity = store
            .get_entity(&mount_id, &RemoteId::new("page-grocery"))
            .expect("get grocery page")
            .expect("grocery page");
        assert_eq!(
            entity.path,
            PathBuf::from("Things to buy/Grocery SF Checklist/page.md")
        );
        assert_eq!(entity.hydration, HydrationState::Hydrated);
        assert!(
            store
                .list_virtual_mutations(&mount_id)
                .expect("list mutations")
                .is_empty()
        );

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn root_level_page_entity_projects_in_mount_root_listing() {
        let mount_id = MountId::new("notion-main");
        let state_root = temp_root("loc-virtual-fs-root-page-listing");
        let content_root = state_root.join("content/notion-main/files");
        let mut store = InMemoryStateStore::new();
        let mount = virtual_mount(&mount_id);
        store.save_mount(mount.clone()).expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    RemoteId::new("page-grocery"),
                    EntityKind::Page,
                    "Grocery SF Checklist",
                    "Grocery SF Checklist/page.md",
                )
                .with_hydration(HydrationState::Hydrated),
            )
            .expect("save root page");
        std::fs::create_dir_all(content_root.join("Grocery SF Checklist"))
            .expect("create cache dir");
        std::fs::write(
            content_root.join("Grocery SF Checklist/page.md"),
            b"---\nloc:\n  id: page-grocery\n  type: page\ntitle: \"Grocery SF Checklist\"\n---\nBody",
        )
        .expect("write page cache");

        let root_children = virtual_fs_children_with_content_root(
            &store,
            &content_root,
            &mount_id,
            &mount_point_identifier(&mount),
        )
        .expect("list mount root");
        let grocery = root_children
            .children
            .iter()
            .find(|item| item.identifier == "children:page-grocery")
            .expect("root page directory appears");
        assert_eq!(grocery.filename, "Grocery SF Checklist");
        assert_eq!(grocery.path, "Grocery SF Checklist");
        let expected_materialized = mount
            .root
            .join("Grocery SF Checklist")
            .display()
            .to_string();
        assert_eq!(
            grocery.materialized_path.as_deref(),
            Some(expected_materialized.as_str())
        );

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
                database_schema: None,
                complete: true,
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

    #[test]
    fn virtual_projection_validation_rejects_filesystem_root_provider_root() {
        let mount = MountConfig::new(MountId::new("notion-main"), "notion", "/")
            .projection(ProjectionMode::LinuxFuse);

        let error = validate_virtual_projection_root(&mount).expect_err("filesystem root rejected");

        assert!(matches!(error, LocalityError::InvalidState(_)));
        assert!(error.to_string().contains("filesystem root"));
    }

    #[test]
    fn virtual_projection_validation_rejects_home_directory_provider_root() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            PathBuf::from(home).join("notion-main"),
        )
        .projection(ProjectionMode::LinuxFuse);

        let error = validate_virtual_projection_root(&mount).expect_err("home root rejected");

        assert!(matches!(error, LocalityError::InvalidState(_)));
        assert!(error.to_string().contains("home directory"));
    }

    #[test]
    fn virtual_projection_validation_rejects_home_container_provider_root() {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        let Some(home_parent) = home.parent().map(|path| path.to_path_buf()) else {
            return;
        };
        let mount = MountConfig::new(MountId::new("notion-main"), "notion", home)
            .projection(ProjectionMode::LinuxFuse);

        let error =
            validate_virtual_projection_root(&mount).expect_err("home container root rejected");

        assert_eq!(virtual_projection_root(&mount), home_parent);
        assert!(matches!(error, LocalityError::InvalidState(_)));
        assert!(error.to_string().contains("home container"));
    }

    #[test]
    fn virtual_projection_validation_accepts_explicit_shared_root() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            "/tmp/Locality/notion-main",
        )
        .projection(ProjectionMode::LinuxFuse);

        validate_virtual_projection_root(&mount).expect("explicit shared root is safe");
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

    fn virtual_mount_with_connector(mount_id: &MountId, connector: &str) -> MountConfig {
        MountConfig::new(
            mount_id.clone(),
            connector,
            format!("/tmp/Locality/{}", mount_id.0),
        )
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
