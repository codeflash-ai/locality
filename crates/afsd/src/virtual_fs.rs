use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use afs_core::canonical::parse_canonical_markdown;
use afs_core::hydration::{HydrationReason, HydrationRequest};
use afs_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use afs_core::{AfsError, AfsResult};
use afs_store::{
    EntityRecord, EntityRepository, MountConfig, MountRepository, ShadowRepository, StoreError,
};
use serde::{Deserialize, Serialize};

use crate::hydration::{HydrationExecutor, HydrationOutcome, HydrationSource};

pub const ROOT_CONTAINER_IDENTIFIER: &str = "root";
const CHILDREN_PREFIX: &str = "children:";
const PATH_PREFIX: &str = "path:";

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
    S: MountRepository + EntityRepository,
{
    let mount = require_mount(store, mount_id)?;
    let entities = store.list_entities(mount_id).map_err(AfsError::from)?;
    let index = ProviderIndex::new(&entities);
    let item = resolve_item(&mount, &entities, &index, identifier)?;

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
    S: MountRepository + EntityRepository,
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
    S: MountRepository + EntityRepository,
{
    let mount = require_mount(store, mount_id)?;
    let entities = store.list_entities(mount_id).map_err(AfsError::from)?;
    let index = ProviderIndex::new(&entities);
    let container_path = container_path(&entities, &index, container_identifier)?;
    let mut children = Vec::new();

    for entity in &entities {
        if parent_path(&entity.path) == container_path {
            children.push(entity_item(&mount, entity, &index));
        }
    }

    for (path, remote_id) in &index.page_child_dirs {
        if parent_path(path) == container_path && index.has_descendant(path) {
            children.push(page_child_dir_item(&mount, path, remote_id, &index));
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
    S: MountRepository + EntityRepository,
{
    let mut report = virtual_fs_children(store, mount_id, container_identifier)?;
    for child in &mut report.children {
        rewrite_item_materialized_path(content_root, child)?;
    }
    Ok(report)
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
    let mount = require_mount(store, mount_id)?;
    let remote_id = RemoteId::new(entity_identifier(identifier)?);
    let entity = require_entity(store, mount_id, &remote_id)?;
    if entity.kind != EntityKind::Page {
        return Err(AfsError::Unsupported(
            "only page files can be materialized by the virtual filesystem",
        ));
    }

    let path = mount.root.join(&entity.path);
    let outcome = if entity.hydration == HydrationState::Hydrated && path.exists() {
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
    S: MountRepository + EntityRepository + ShadowRepository,
    Source: HydrationSource + ?Sized,
{
    let mount = require_mount(store, mount_id)?;
    let remote_id = RemoteId::new(entity_identifier(identifier)?);
    let entity = require_entity(store, mount_id, &remote_id)?;
    if entity.kind != EntityKind::Page {
        return Err(AfsError::Unsupported(
            "only page files can be materialized by the virtual filesystem",
        ));
    }

    let path = content_path_for_relative(content_root, &entity.path)?;
    let outcome = if entity.hydration == HydrationState::Hydrated && path.exists() {
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
    S: MountRepository + EntityRepository + ShadowRepository,
{
    let mount = require_mount(store, mount_id)?;
    if mount.read_only {
        return Err(AfsError::Unsupported(
            "read-only mounts do not accept virtual filesystem writes",
        ));
    }
    let remote_id = RemoteId::new(entity_identifier(identifier)?);
    let mut entity = require_entity(store, mount_id, &remote_id)?;
    if entity.kind != EntityKind::Page {
        return Err(AfsError::Unsupported(
            "only page files can be written by the virtual filesystem",
        ));
    }
    if entity.hydration == HydrationState::Conflicted {
        return Err(AfsError::InvalidState(
            "conflicted virtual filesystem files must be resolved before writing".to_string(),
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

    if matches_shadow {
        if entity
            .hydration
            .can_transition_to(&HydrationState::Hydrated)
        {
            entity.hydration = HydrationState::Hydrated;
        }
    } else if entity.hydration.can_transition_to(&HydrationState::Dirty) {
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

pub fn virtual_fs_content_root(state_root: &Path, mount_id: &MountId) -> PathBuf {
    state_root.join("content").join(&mount_id.0).join("files")
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

fn resolve_item(
    mount: &MountConfig,
    entities: &[EntityRecord],
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

    let remote_id = RemoteId::new(identifier);
    let entity = entities
        .iter()
        .find(|entity| entity.remote_id == remote_id)
        .ok_or_else(|| missing_identifier(identifier))?;
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
    }
}

fn rewrite_item_materialized_path(content_root: &Path, item: &mut VirtualFsItem) -> AfsResult<()> {
    if item.kind == VirtualFsItemKind::File && item.hydration == Some(HydrationState::Hydrated) {
        item.materialized_path = Some(
            content_path_for_relative(content_root, Path::new(&item.path))?
                .display()
                .to_string(),
        );
    }
    Ok(())
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
    }
}

fn container_path(
    entities: &[EntityRecord],
    index: &ProviderIndex,
    identifier: &str,
) -> AfsResult<PathBuf> {
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
    let child_path = entity.path.with_extension("");
    if index.has_descendant(&child_path) {
        return Ok(child_path);
    }

    Err(AfsError::InvalidState(format!(
        "virtual filesystem item `{identifier}` is not a container"
    )))
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

    fn has_descendant(&self, path: &Path) -> bool {
        self.entities_by_path.keys().any(|entity_path| {
            entity_path.starts_with(path)
                && entity_path != path
                && entity_path
                    .strip_prefix(path)
                    .is_ok_and(|suffix| suffix.components().count() > 0)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use afs_core::model::{EntityKind, HydrationState, MountId, RemoteId};
    use afs_store::{
        EntityRecord, EntityRepository, InMemoryStateStore, MountConfig, MountRepository,
    };

    use super::{
        ROOT_CONTAINER_IDENTIFIER, VirtualFsItemKind, commit_virtual_fs_write, virtual_fs_children,
        virtual_fs_content_path, virtual_fs_item,
    };

    #[test]
    fn children_include_page_files_and_synthetic_page_child_folders() {
        let mount_id = MountId::new("notion-main");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(MountConfig::new(
                mount_id.clone(),
                "notion",
                "/tmp/afs/notion",
            ))
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
            .save_mount(MountConfig::new(
                mount_id.clone(),
                "notion",
                "/tmp/afs/notion",
            ))
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

        assert_eq!(report.children.len(), 1);
        assert_eq!(report.children[0].identifier, "page-child");
        assert_eq!(
            report.children[0].parent_identifier.as_deref(),
            Some("children:page-root")
        );
    }

    #[test]
    fn item_metadata_is_store_only_for_online_only_files() {
        let mount_id = MountId::new("notion-main");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(MountConfig::new(
                mount_id.clone(),
                "notion",
                "/tmp/afs/notion",
            ))
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
            .save_mount(MountConfig::new(
                mount_id.clone(),
                "notion",
                "/tmp/afs/notion",
            ))
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

    fn temp_root(prefix: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{unique}", std::process::id()))
    }
}
