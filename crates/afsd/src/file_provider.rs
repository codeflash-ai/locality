use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
pub struct FileProviderItemReport {
    pub mount_id: String,
    pub item: FileProviderItem,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileProviderChildrenReport {
    pub mount_id: String,
    pub container_identifier: String,
    pub children: Vec<FileProviderItem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileProviderMaterializeReport {
    pub mount_id: String,
    pub identifier: String,
    pub remote_id: String,
    pub path: String,
    pub outcome: FileProviderMaterializeOutcome,
    pub hydration: HydrationState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileProviderMaterializeOutcome {
    AlreadyMaterialized,
    Hydrated,
    SkippedDirty,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileProviderItem {
    pub identifier: String,
    pub parent_identifier: Option<String>,
    pub filename: String,
    pub kind: FileProviderItemKind,
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
pub enum FileProviderItemKind {
    File,
    Folder,
}

pub fn file_provider_item<S>(
    store: &S,
    mount_id: &MountId,
    identifier: &str,
) -> AfsResult<FileProviderItemReport>
where
    S: MountRepository + EntityRepository,
{
    let mount = require_mount(store, mount_id)?;
    let entities = store.list_entities(mount_id).map_err(AfsError::from)?;
    let index = ProviderIndex::new(&entities);
    let item = resolve_item(&mount, &entities, &index, identifier)?;

    Ok(FileProviderItemReport {
        mount_id: mount_id.0.clone(),
        item,
    })
}

pub fn file_provider_children<S>(
    store: &S,
    mount_id: &MountId,
    container_identifier: &str,
) -> AfsResult<FileProviderChildrenReport>
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

    Ok(FileProviderChildrenReport {
        mount_id: mount_id.0.clone(),
        container_identifier: container_identifier.to_string(),
        children,
    })
}

pub fn materialize_file_provider_item<S, Source>(
    store: &mut S,
    source: &Source,
    mount_id: &MountId,
    identifier: &str,
) -> AfsResult<FileProviderMaterializeReport>
where
    S: MountRepository + EntityRepository + ShadowRepository,
    Source: HydrationSource + ?Sized,
{
    let mount = require_mount(store, mount_id)?;
    let remote_id = RemoteId::new(entity_identifier(identifier)?);
    let entity = require_entity(store, mount_id, &remote_id)?;
    if entity.kind != EntityKind::Page {
        return Err(AfsError::Unsupported(
            "only page files can be materialized by the file provider",
        ));
    }

    let path = mount.root.join(&entity.path);
    let outcome = if entity.hydration == HydrationState::Hydrated && path.exists() {
        FileProviderMaterializeOutcome::AlreadyMaterialized
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
            HydrationOutcome::Hydrated => FileProviderMaterializeOutcome::Hydrated,
            HydrationOutcome::SkippedDirty => FileProviderMaterializeOutcome::SkippedDirty,
        }
    };

    let hydrated = require_entity(store, mount_id, &remote_id)?;
    if !path.exists() {
        return Err(AfsError::InvalidState(format!(
            "file provider materialization finished without local contents for `{}`",
            path.display()
        )));
    }

    Ok(FileProviderMaterializeReport {
        mount_id: mount_id.0.clone(),
        identifier: identifier.to_string(),
        remote_id: remote_id.0,
        path: path.display().to_string(),
        outcome,
        hydration: hydrated.hydration,
    })
}

fn resolve_item(
    mount: &MountConfig,
    entities: &[EntityRecord],
    index: &ProviderIndex,
    identifier: &str,
) -> AfsResult<FileProviderItem> {
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

fn root_item(mount: &MountConfig) -> FileProviderItem {
    FileProviderItem {
        identifier: ROOT_CONTAINER_IDENTIFIER.to_string(),
        parent_identifier: None,
        filename: mount
            .root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| mount.mount_id.0.clone()),
        kind: FileProviderItemKind::Folder,
        entity_kind: None,
        remote_id: None,
        path: String::new(),
        hydration: None,
        content_type: "public.folder".to_string(),
        remote_edited_at: None,
        materialized_path: None,
    }
}

fn entity_item(
    mount: &MountConfig,
    entity: &EntityRecord,
    index: &ProviderIndex,
) -> FileProviderItem {
    let kind = match &entity.kind {
        EntityKind::Page | EntityKind::Asset | EntityKind::Unknown(_) => FileProviderItemKind::File,
        EntityKind::Database | EntityKind::Directory => FileProviderItemKind::Folder,
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

    FileProviderItem {
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

fn page_child_dir_item(
    mount: &MountConfig,
    path: &Path,
    remote_id: &RemoteId,
    index: &ProviderIndex,
) -> FileProviderItem {
    FileProviderItem {
        identifier: format!("{CHILDREN_PREFIX}{}", remote_id.0),
        parent_identifier: Some(container_identifier_for_path(parent_path(path), index)),
        filename: filename(path),
        kind: FileProviderItemKind::Folder,
        entity_kind: None,
        remote_id: Some(remote_id.0.clone()),
        path: path_string(path),
        hydration: None,
        content_type: "public.folder".to_string(),
        remote_edited_at: None,
        materialized_path: Some(mount.root.join(path).display().to_string()),
    }
}

fn path_dir_item(mount: &MountConfig, path: &Path, index: &ProviderIndex) -> FileProviderItem {
    FileProviderItem {
        identifier: format!("{PATH_PREFIX}{}", path_string(path)),
        parent_identifier: Some(container_identifier_for_path(parent_path(path), index)),
        filename: filename(path),
        kind: FileProviderItemKind::Folder,
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
        "file provider item `{identifier}` is not a container"
    )))
}

fn entity_identifier(identifier: &str) -> AfsResult<String> {
    if identifier == ROOT_CONTAINER_IDENTIFIER
        || identifier.starts_with(CHILDREN_PREFIX)
        || identifier.starts_with(PATH_PREFIX)
    {
        return Err(AfsError::InvalidState(format!(
            "file provider identifier `{identifier}` is not a materializable file"
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
        "file provider item `{identifier}` is not present in daemon state"
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
    use afs_core::model::{EntityKind, HydrationState, MountId, RemoteId};
    use afs_store::{
        EntityRecord, EntityRepository, InMemoryStateStore, MountConfig, MountRepository,
    };

    use super::{
        FileProviderItemKind, ROOT_CONTAINER_IDENTIFIER, file_provider_children, file_provider_item,
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

        let report = file_provider_children(&store, &mount_id, ROOT_CONTAINER_IDENTIFIER)
            .expect("root children");

        assert_eq!(report.children.len(), 2);
        assert_eq!(report.children[0].filename, "Home");
        assert_eq!(report.children[0].kind, FileProviderItemKind::Folder);
        assert_eq!(report.children[0].identifier, "children:page-root");
        assert_eq!(report.children[1].filename, "Home.md");
        assert_eq!(report.children[1].kind, FileProviderItemKind::File);
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
            file_provider_children(&store, &mount_id, "children:page-root").expect("children");

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

        let report = file_provider_item(&store, &mount_id, "page-1").expect("item");

        assert_eq!(report.item.filename, "Roadmap.md");
        assert_eq!(report.item.kind, FileProviderItemKind::File);
        assert_eq!(report.item.materialized_path, None);
    }
}
