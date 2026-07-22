//! Project Notion pages, child-page blocks, and databases into Locality tree entries.
//!
//! Paths are a local projection, not identity. The remote ID remains the stable
//! key. Clean names are used when possible, and short remote ID suffixes are
//! added only when sibling names would otherwise collide.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use locality_connector::ChildContainer;
use locality_core::freshness::{RemoteObservation, RemoteVersion};
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId, TreeEntry};
use locality_core::path_projection::{page_container_path, page_document_path};
use locality_core::{LocalityError, LocalityResult};

use crate::client::NotionApi;
use crate::dto::{BlockDto, DatabaseDto, PageDto, ParentDto};
use crate::render::{page_frontmatter, page_title};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExplicitRootTreeEntry {
    pub entry: TreeEntry,
    pub scope_root_remote_id: RemoteId,
}

pub fn enumerate_root_page_tree(
    api: &dyn NotionApi,
    mount_id: MountId,
    root_page_id: &RemoteId,
) -> LocalityResult<Vec<TreeEntry>> {
    Ok(
        enumerate_explicit_root_trees(api, mount_id, std::slice::from_ref(root_page_id))?
            .into_iter()
            .map(|projected| projected.entry)
            .collect(),
    )
}

pub(crate) fn enumerate_explicit_root_trees(
    api: &dyn NotionApi,
    mount_id: MountId,
    root_page_ids: &[RemoteId],
) -> LocalityResult<Vec<ExplicitRootTreeEntry>> {
    let mut used_paths = BTreeSet::new();
    let mut entries = Vec::new();
    let mut owners = BTreeMap::new();
    let mut root_children = root_page_ids
        .iter()
        .map(|root_id| retrieve_explicit_root(api, root_id))
        .collect::<LocalityResult<Vec<_>>>()?;
    root_children.sort_by(|left, right| {
        explicit_root_identity_key(left.remote_id())
            .cmp(&explicit_root_identity_key(right.remote_id()))
    });

    for projected in allocate_child_paths(Path::new(""), root_children, &mut used_paths) {
        let scope_root_remote_id = RemoteId::new(projected.child.remote_id().to_string());
        let mut sink = ExplicitRootSink {
            scope_root_remote_id: &scope_root_remote_id,
            entries: &mut entries,
            owners: &mut owners,
        };
        push_projected_tree_entry(api, &mount_id, projected, &mut used_paths, &mut sink)?;
    }

    Ok(entries)
}

fn retrieve_explicit_root(
    api: &dyn NotionApi,
    root_id: &RemoteId,
) -> LocalityResult<ProjectedChild> {
    match api.retrieve_page(root_id.as_str()) {
        Ok(page) => {
            validate_explicit_root_identity(root_id, &page.id, "page")?;
            let title = page_title(&page);
            Ok(ProjectedChild::Page { page, title })
        }
        Err(LocalityError::RemoteNotFound(_)) => {
            let database = api.retrieve_database(root_id.as_str())?;
            validate_explicit_root_identity(root_id, &database.id, "database")?;
            let title =
                database_title(&database).unwrap_or_else(|| "Untitled database".to_string());
            Ok(ProjectedChild::Database { database, title })
        }
        Err(error) => Err(error),
    }
}

fn validate_explicit_root_identity(
    requested: &RemoteId,
    returned: &str,
    kind: &str,
) -> LocalityResult<()> {
    if explicit_root_identity_key(requested.as_str()) != explicit_root_identity_key(returned) {
        return Err(LocalityError::InvalidState(format!(
            "Notion explicit root request `{}` returned {kind} `{returned}`",
            requested.as_str()
        )));
    }
    Ok(())
}

pub fn enumerate_shared_pages(
    api: &dyn NotionApi,
    mount_id: MountId,
) -> LocalityResult<Vec<TreeEntry>> {
    let mut used_paths = BTreeSet::new();
    let mut entries = Vec::new();

    let pages = search_all_pages(api)?;
    let databases = search_all_databases(api)?;
    let accessible_page_ids = pages
        .iter()
        .map(|page| page.id.as_str())
        .collect::<BTreeSet<_>>();

    let mut root_children = Vec::new();
    for page in pages
        .iter()
        .filter(|page| is_workspace_root_page(page, &accessible_page_ids))
    {
        let title = page_title(page);
        root_children.push(ProjectedChild::Page {
            page: page.clone(),
            title,
        });
    }

    for database in databases
        .iter()
        .filter(|database| is_workspace_root_parent(database.parent.as_ref(), &accessible_page_ids))
    {
        let title = database_title(database).unwrap_or_else(|| "Untitled database".to_string());
        root_children.push(ProjectedChild::Database {
            database: database.clone(),
            title,
        });
    }

    for projected in allocate_child_paths(Path::new(""), root_children, &mut used_paths) {
        push_projected_tree_entry(api, &mount_id, projected, &mut used_paths, &mut entries)?;
    }

    let projected_page_ids = entries
        .iter()
        .filter(|entry| entry.kind == EntityKind::Page)
        .map(|entry| entry.remote_id.0.clone())
        .collect::<BTreeSet<_>>();
    let fallback_pages = pages
        .iter()
        .filter(|page| !projected_page_ids.contains(&page.id))
        .map(|page| ProjectedChild::Page {
            page: page.clone(),
            title: page_title(page),
        })
        .collect::<Vec<_>>();
    for projected in allocate_child_paths(Path::new(""), fallback_pages, &mut used_paths) {
        push_projected_listing_entry(&mount_id, projected, &mut entries);
    }

    Ok(entries)
}

pub fn list_container_children(
    api: &dyn NotionApi,
    mount_id: MountId,
    root_page_id: Option<&RemoteId>,
    container: ChildContainer,
    parent_path: &Path,
) -> LocalityResult<Vec<TreeEntry>> {
    match container {
        ChildContainer::Root => list_root_children(api, mount_id, root_page_id, parent_path),
        ChildContainer::PageChildren(page_id) => {
            list_page_children(api, &mount_id, page_id.as_str(), parent_path)
        }
        ChildContainer::DatabaseRows(database_id) => {
            let database = api.retrieve_database(database_id.as_str())?;
            list_database_rows(api, &mount_id, &database, parent_path)
        }
        ChildContainer::DirectoryChildren(_) => Err(LocalityError::Unsupported(
            "listing directory children with the Notion connector",
        )),
    }
}

pub fn observe_entity(
    api: &dyn NotionApi,
    mount_id: MountId,
    remote_id: &RemoteId,
) -> LocalityResult<RemoteObservation> {
    match api.retrieve_page(remote_id.as_str()) {
        Ok(page) => Ok(page_observation(mount_id, &page)),
        Err(page_error) => match api.retrieve_database(remote_id.as_str()) {
            Ok(database) => Ok(database_observation(mount_id, &database)),
            Err(database_error) => Err(LocalityError::InvalidState(format!(
                "notion object `{}` could not be observed as page ({page_error}) or database ({database_error})",
                remote_id.as_str()
            ))),
        },
    }
}

pub fn resolve_page_path_entries(
    api: &dyn NotionApi,
    mount_id: MountId,
    root_page_id: Option<&RemoteId>,
    page_id: &RemoteId,
) -> LocalityResult<Vec<TreeEntry>> {
    let mut resolver = exact_path_resolver(api, mount_id, root_page_id);
    resolver.resolve_page(page_id.as_str())?;
    Ok(resolver.entries)
}

pub fn resolve_notion_object_path_entries(
    api: &dyn NotionApi,
    mount_id: MountId,
    root_page_id: Option<&RemoteId>,
    object_id: &RemoteId,
) -> LocalityResult<Vec<TreeEntry>> {
    let mut page_resolver = exact_path_resolver(api, mount_id.clone(), root_page_id);
    match page_resolver.resolve_page(object_id.as_str()) {
        Ok(_) => return Ok(page_resolver.entries),
        Err(page_error) => {
            let mut database_resolver = exact_path_resolver(api, mount_id, root_page_id);
            database_resolver
                .resolve_database(object_id.as_str())
                .map(|_| database_resolver.entries)
                .map_err(|database_error| {
                    LocalityError::InvalidState(format!(
                        "notion object `{}` could not be resolved as page ({page_error}) or database ({database_error})",
                        object_id.as_str()
                    ))
                })
        }
    }
}

fn exact_path_resolver<'a>(
    api: &'a dyn NotionApi,
    mount_id: MountId,
    root_page_id: Option<&'a RemoteId>,
) -> ExactPathResolver<'a> {
    ExactPathResolver {
        api,
        mount_id,
        root_page_id,
        resolved: BTreeMap::new(),
        resolving: BTreeSet::new(),
        entries: Vec::new(),
    }
}

struct ExactPathResolver<'a> {
    api: &'a dyn NotionApi,
    mount_id: MountId,
    root_page_id: Option<&'a RemoteId>,
    resolved: BTreeMap<String, TreeEntry>,
    resolving: BTreeSet<String>,
    entries: Vec<TreeEntry>,
}

#[derive(Clone, Debug)]
enum ParentListing {
    Root(PathBuf),
    PageChildren {
        page_id: RemoteId,
        parent_path: PathBuf,
    },
    DatabaseRows {
        database_id: RemoteId,
        parent_path: PathBuf,
    },
}

impl ExactPathResolver<'_> {
    fn resolve_page(&mut self, page_id: &str) -> LocalityResult<TreeEntry> {
        if let Some(entry) = self.resolved.get(page_id) {
            return Ok(entry.clone());
        }
        if !self.resolving.insert(page_id.to_string()) {
            return Err(LocalityError::InvalidState(format!(
                "notion hierarchy for page `{page_id}` contains a parent cycle"
            )));
        }

        let page = self.api.retrieve_page(page_id)?;
        let entry = if self
            .root_page_id
            .is_some_and(|root_page_id| notion_ids_equal(root_page_id.as_str(), page_id))
        {
            let title = page_title(&page);
            let mut used_paths = BTreeSet::new();
            let path = allocate_page_path(Path::new(""), &title, &page.id, &mut used_paths);
            page_entry(self.mount_id.clone(), &page, title, path)
        } else {
            let parent = self.parent_listing(page.parent.as_ref())?;
            self.find_projected_child(parent, page_id, EntityKind::Page)?
        };

        self.resolving.remove(page_id);
        Ok(self.remember(entry))
    }

    fn resolve_database(&mut self, database_id: &str) -> LocalityResult<TreeEntry> {
        if let Some(entry) = self.resolved.get(database_id) {
            return Ok(entry.clone());
        }
        if !self.resolving.insert(database_id.to_string()) {
            return Err(LocalityError::InvalidState(format!(
                "notion hierarchy for database `{database_id}` contains a parent cycle"
            )));
        }

        let database = self.api.retrieve_database(database_id)?;
        let parent = self.parent_listing(database.parent.as_ref())?;
        let entry = self.find_projected_child(parent, database_id, EntityKind::Database)?;

        self.resolving.remove(database_id);
        Ok(self.remember(entry))
    }

    fn parent_listing(&mut self, parent: Option<&ParentDto>) -> LocalityResult<ParentListing> {
        let Some(parent) = parent else {
            return Ok(ParentListing::Root(PathBuf::new()));
        };
        if parent.workspace == Some(true) || parent.kind == "workspace" {
            return Ok(ParentListing::Root(PathBuf::new()));
        }
        if let Some(page_id) = parent.page_id.as_deref() {
            let parent_entry = self.resolve_page(page_id)?;
            return Ok(ParentListing::PageChildren {
                page_id: RemoteId::new(page_id.to_string()),
                parent_path: page_child_dir(&parent_entry.path),
            });
        }
        if let Some(database_id) = parent.database_id.as_deref() {
            let parent_entry = self.resolve_database(database_id)?;
            return Ok(ParentListing::DatabaseRows {
                database_id: RemoteId::new(database_id.to_string()),
                parent_path: parent_entry.path,
            });
        }
        if let Some(data_source_id) = parent.data_source_id.as_deref() {
            let data_source = self.api.retrieve_data_source(data_source_id)?;
            let database_id = data_source
                .parent
                .as_ref()
                .and_then(|parent| parent.database_id.as_deref())
                .ok_or_else(|| {
                    LocalityError::InvalidState(format!(
                        "notion data source `{data_source_id}` did not expose a parent database"
                    ))
                })?;
            let parent_entry = self.resolve_database(database_id)?;
            return Ok(ParentListing::DatabaseRows {
                database_id: RemoteId::new(database_id.to_string()),
                parent_path: parent_entry.path,
            });
        }
        if let Some(block_id) = parent.block_id.as_deref() {
            return self.block_parent_listing(block_id);
        }

        Err(LocalityError::InvalidState(format!(
            "cannot resolve a stable local path for Notion parent `{}`",
            parent.kind
        )))
    }

    fn block_parent_listing(&mut self, block_id: &str) -> LocalityResult<ParentListing> {
        let resolving_key = format!("block:{block_id}");
        if !self.resolving.insert(resolving_key.clone()) {
            return Err(LocalityError::InvalidState(format!(
                "notion hierarchy for block `{block_id}` contains a parent cycle"
            )));
        }

        let block = self.api.retrieve_block(block_id)?;
        let listing = self.parent_listing(block.parent.as_ref());
        self.resolving.remove(&resolving_key);
        listing
    }

    fn find_projected_child(
        &mut self,
        parent: ParentListing,
        remote_id: &str,
        kind: EntityKind,
    ) -> LocalityResult<TreeEntry> {
        let entries = match parent {
            ParentListing::Root(parent_path) => list_root_children(
                self.api,
                self.mount_id.clone(),
                self.root_page_id,
                &parent_path,
            )?,
            ParentListing::PageChildren {
                page_id,
                parent_path,
            } => list_page_children(self.api, &self.mount_id, page_id.as_str(), &parent_path)?,
            ParentListing::DatabaseRows {
                database_id,
                parent_path,
            } => {
                let database = self.api.retrieve_database(database_id.as_str())?;
                list_database_rows(self.api, &self.mount_id, &database, &parent_path)?
            }
        };

        entries
            .into_iter()
            .find(|entry| {
                notion_ids_equal(entry.remote_id.as_str(), remote_id) && entry.kind == kind
            })
            .ok_or_else(|| {
                LocalityError::RemoteNotFound(format!(
                    "notion object `{remote_id}` was not found in its resolved parent"
                ))
            })
    }

    fn remember(&mut self, entry: TreeEntry) -> TreeEntry {
        if !self.resolved.contains_key(entry.remote_id.as_str()) {
            self.entries.push(entry.clone());
        }
        self.resolved
            .insert(entry.remote_id.0.clone(), entry.clone());
        entry
    }
}

fn notion_ids_equal(left: &str, right: &str) -> bool {
    compact_notion_id(left) == compact_notion_id(right)
}

fn compact_notion_id(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

fn push_projected_listing_entry(
    mount_id: &MountId,
    projected: ProjectedChildWithPath,
    entries: &mut Vec<TreeEntry>,
) {
    match projected.child {
        ProjectedChild::Page { page, title } => {
            entries.push(page_entry(mount_id.clone(), &page, title, projected.path));
        }
        ProjectedChild::Database { database, title } => {
            entries.push(database_entry(
                mount_id.clone(),
                &database,
                title,
                projected.path,
            ));
        }
    }
}

trait TreeEntrySink {
    fn push_entry(&mut self, entry: TreeEntry) -> LocalityResult<()>;
}

impl TreeEntrySink for Vec<TreeEntry> {
    fn push_entry(&mut self, entry: TreeEntry) -> LocalityResult<()> {
        self.push(entry);
        Ok(())
    }
}

struct ExplicitRootSink<'a> {
    scope_root_remote_id: &'a RemoteId,
    entries: &'a mut Vec<ExplicitRootTreeEntry>,
    owners: &'a mut BTreeMap<String, RemoteId>,
}

impl TreeEntrySink for ExplicitRootSink<'_> {
    fn push_entry(&mut self, entry: TreeEntry) -> LocalityResult<()> {
        let key = explicit_root_identity_key(entry.remote_id.as_str());
        if let Some(existing_owner) = self.owners.insert(key, self.scope_root_remote_id.clone()) {
            return Err(LocalityError::InvalidState(format!(
                "Notion explicit roots overlap or project object `{}` ambiguously between `{}` and `{}`",
                entry.remote_id.as_str(),
                existing_owner.as_str(),
                self.scope_root_remote_id.as_str()
            )));
        }
        self.entries.push(ExplicitRootTreeEntry {
            entry,
            scope_root_remote_id: self.scope_root_remote_id.clone(),
        });
        Ok(())
    }
}

fn explicit_root_identity_key(value: &str) -> String {
    value
        .chars()
        .filter(|character| *character != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

fn push_projected_tree_entry<S: TreeEntrySink>(
    api: &dyn NotionApi,
    mount_id: &MountId,
    projected: ProjectedChildWithPath,
    used_paths: &mut BTreeSet<PathBuf>,
    entries: &mut S,
) -> LocalityResult<()> {
    match projected.child {
        ProjectedChild::Page { page, title } => {
            entries.push_entry(page_entry(
                mount_id.clone(),
                &page,
                title,
                projected.path.clone(),
            ))?;
            enumerate_page_children(
                api,
                mount_id,
                page.id.as_str(),
                page_child_dir(&projected.path),
                used_paths,
                entries,
            )?;
        }
        ProjectedChild::Database { database, title } => {
            entries.push_entry(database_entry(
                mount_id.clone(),
                &database,
                title,
                projected.path.clone(),
            ))?;
            enumerate_database_rows(
                api,
                mount_id,
                &database,
                &projected.path,
                used_paths,
                entries,
            )?;
        }
    }

    Ok(())
}

fn list_root_children(
    api: &dyn NotionApi,
    mount_id: MountId,
    root_page_id: Option<&RemoteId>,
    parent_path: &Path,
) -> LocalityResult<Vec<TreeEntry>> {
    let mut used_paths = BTreeSet::new();
    if let Some(root_page_id) = root_page_id {
        let page = api.retrieve_page(root_page_id.as_str())?;
        let title = page_title(&page);
        let path = allocate_page_path(parent_path, &title, &page.id, &mut used_paths);
        return Ok(vec![page_entry(mount_id, &page, title, path)]);
    }

    let pages = search_all_pages(api)?;
    let accessible_page_ids = pages
        .iter()
        .map(|page| page.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut entries = Vec::new();

    let mut root_children = Vec::new();
    for page in pages
        .iter()
        .filter(|page| is_workspace_root_page(page, &accessible_page_ids))
    {
        let title = page_title(page);
        root_children.push(ProjectedChild::Page {
            page: page.clone(),
            title,
        });
    }

    for database in search_all_databases(api)?
        .iter()
        .filter(|database| is_workspace_root_parent(database.parent.as_ref(), &accessible_page_ids))
    {
        let title = database_title(database).unwrap_or_else(|| "Untitled database".to_string());
        root_children.push(ProjectedChild::Database {
            database: database.clone(),
            title,
        });
    }

    for projected in allocate_child_paths(parent_path, root_children, &mut used_paths) {
        push_projected_listing_entry(&mount_id, projected, &mut entries);
    }

    Ok(entries)
}

fn search_all_pages(api: &dyn NotionApi) -> LocalityResult<Vec<PageDto>> {
    let mut cursor = None;
    let mut pages = Vec::new();

    loop {
        let page = api.search_pages(cursor.as_deref())?;
        pages.extend(page.results);

        if !page.has_more {
            break;
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            return Err(LocalityError::InvalidState(
                "notion search page had has_more without next_cursor".to_string(),
            ));
        }
    }

    Ok(pages)
}

fn is_workspace_root_page(page: &PageDto, accessible_page_ids: &BTreeSet<&str>) -> bool {
    is_workspace_root_parent(page.parent.as_ref(), accessible_page_ids)
}

fn is_workspace_root_parent(
    parent: Option<&ParentDto>,
    accessible_page_ids: &BTreeSet<&str>,
) -> bool {
    match parent {
        None => true,
        Some(parent) if parent.kind == "workspace" => true,
        Some(ParentDto {
            page_id: Some(parent_page_id),
            ..
        }) => !accessible_page_ids.contains(parent_page_id.as_str()),
        Some(parent) if parent.kind == "page_id" => parent
            .page_id
            .as_deref()
            .is_none_or(|parent_id| !accessible_page_ids.contains(parent_id)),
        _ => false,
    }
}

fn search_all_databases(api: &dyn NotionApi) -> LocalityResult<Vec<DatabaseDto>> {
    let mut cursor = None;
    let mut databases = Vec::new();

    loop {
        let page = api.search_databases(cursor.as_deref())?;
        databases.extend(page.results);

        if !page.has_more {
            break;
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            return Err(LocalityError::InvalidState(
                "notion search database page had has_more without next_cursor".to_string(),
            ));
        }
    }

    Ok(databases)
}

enum ProjectedChild {
    Page {
        page: PageDto,
        title: String,
    },
    Database {
        database: DatabaseDto,
        title: String,
    },
}

impl ProjectedChild {
    fn kind(&self) -> ProjectedPathKind {
        match self {
            ProjectedChild::Page { .. } => ProjectedPathKind::Page,
            ProjectedChild::Database { .. } => ProjectedPathKind::Directory,
        }
    }

    fn remote_id(&self) -> &str {
        match self {
            ProjectedChild::Page { page, .. } => &page.id,
            ProjectedChild::Database { database, .. } => &database.id,
        }
    }

    fn title(&self) -> &str {
        match self {
            ProjectedChild::Page { title, .. } | ProjectedChild::Database { title, .. } => title,
        }
    }
}

struct ProjectedChildWithPath {
    child: ProjectedChild,
    path: PathBuf,
}

#[derive(Clone, Copy)]
enum ProjectedPathKind {
    Page,
    Directory,
}

struct ProjectedPathReservation {
    path: PathBuf,
    reserved: Vec<PathBuf>,
}

fn list_page_children(
    api: &dyn NotionApi,
    mount_id: &MountId,
    block_id: &str,
    parent_dir: &Path,
) -> LocalityResult<Vec<TreeEntry>> {
    let mut entries = Vec::new();
    let mut used_paths = BTreeSet::new();
    let children = collect_page_child_projections(api, block_id)?;
    for projected in allocate_child_paths(parent_dir, children, &mut used_paths) {
        push_projected_listing_entry(mount_id, projected, &mut entries);
    }

    Ok(entries)
}

fn collect_page_child_projections(
    api: &dyn NotionApi,
    block_id: &str,
) -> LocalityResult<Vec<ProjectedChild>> {
    let mut cursor = None;
    let mut children = Vec::new();

    loop {
        let page = api.retrieve_block_children(block_id, cursor.as_deref())?;
        for block in page.results {
            collect_child_block_projection(api, block, &mut children)?;
        }

        if !page.has_more {
            break;
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            return Err(LocalityError::InvalidState(
                "notion block children page had has_more without next_cursor".to_string(),
            ));
        }
    }

    Ok(children)
}

fn collect_child_block_projection(
    api: &dyn NotionApi,
    block: BlockDto,
    children: &mut Vec<ProjectedChild>,
) -> LocalityResult<()> {
    match block.kind.as_str() {
        "child_page" => {
            let page = api.retrieve_page(block.id.as_str())?;
            let title = block
                .child_page
                .as_ref()
                .map(|child| child.title.clone())
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| page_title(&page));
            children.push(ProjectedChild::Page { page, title });
        }
        "child_database" => {
            let database = api.retrieve_database(block.id.as_str())?;
            let title = block
                .child_database
                .as_ref()
                .map(|child| child.title.clone())
                .filter(|title| !title.trim().is_empty())
                .or_else(|| database_title(&database))
                .unwrap_or_else(|| "Untitled database".to_string());
            children.push(ProjectedChild::Database { database, title });
        }
        _ if block.has_children => {
            children.extend(collect_page_child_projections(api, block.id.as_str())?);
        }
        _ => {}
    }

    Ok(())
}

fn list_database_rows(
    api: &dyn NotionApi,
    mount_id: &MountId,
    database: &DatabaseDto,
    database_dir: &Path,
) -> LocalityResult<Vec<TreeEntry>> {
    let mut entries = Vec::new();
    let mut used_paths = BTreeSet::new();
    let rows = collect_database_row_projections(api, database)?;
    for projected in allocate_child_paths(database_dir, rows, &mut used_paths) {
        push_projected_listing_entry(mount_id, projected, &mut entries);
    }

    Ok(entries)
}

fn enumerate_page_children<S: TreeEntrySink>(
    api: &dyn NotionApi,
    mount_id: &MountId,
    block_id: &str,
    parent_dir: PathBuf,
    used_paths: &mut BTreeSet<PathBuf>,
    entries: &mut S,
) -> LocalityResult<()> {
    let children = collect_page_child_projections(api, block_id)?;
    for projected in allocate_child_paths(&parent_dir, children, used_paths) {
        push_projected_tree_entry(api, mount_id, projected, used_paths, entries)?;
    }

    Ok(())
}

fn enumerate_database_rows<S: TreeEntrySink>(
    api: &dyn NotionApi,
    mount_id: &MountId,
    database: &DatabaseDto,
    database_dir: &Path,
    used_paths: &mut BTreeSet<PathBuf>,
    entries: &mut S,
) -> LocalityResult<()> {
    let rows = collect_database_row_projections(api, database)?;
    for projected in allocate_child_paths(database_dir, rows, used_paths) {
        if let ProjectedChild::Page { page, title } = projected.child {
            entries.push_entry(page_entry(
                mount_id.clone(),
                &page,
                title,
                projected.path.clone(),
            ))?;
            enumerate_page_children(
                api,
                mount_id,
                page.id.as_str(),
                page_child_dir(&projected.path),
                used_paths,
                entries,
            )?;
        }
    }

    Ok(())
}

fn collect_database_row_projections(
    api: &dyn NotionApi,
    database: &DatabaseDto,
) -> LocalityResult<Vec<ProjectedChild>> {
    let mut rows = Vec::new();
    let data_source_ids = database
        .data_sources
        .iter()
        .map(|data_source| data_source.id.as_str())
        .collect::<BTreeSet<_>>();
    for data_source in &database.data_sources {
        let mut cursor = None;

        loop {
            let page = api.query_data_source(&data_source.id, cursor.as_deref())?;
            for row in page.results {
                if !is_database_row_for_database(&row, database, &data_source_ids) {
                    continue;
                }
                let title = page_title(&row);
                rows.push(ProjectedChild::Page { page: row, title });
            }

            if !page.has_more {
                break;
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Err(LocalityError::InvalidState(
                    "notion data source query page had has_more without next_cursor".to_string(),
                ));
            }
        }
    }

    Ok(rows)
}

fn is_database_row_for_database(
    page: &PageDto,
    database: &DatabaseDto,
    data_source_ids: &BTreeSet<&str>,
) -> bool {
    let Some(parent) = page.parent.as_ref() else {
        return true;
    };

    if parent
        .data_source_id
        .as_deref()
        .is_some_and(|data_source_id| data_source_ids.contains(data_source_id))
    {
        return true;
    }

    if parent
        .database_id
        .as_deref()
        .is_some_and(|database_id| database_id == database.id)
    {
        return true;
    }

    matches!(parent.kind.as_str(), "data_source_id" | "database_id")
        && parent.data_source_id.is_none()
        && parent.database_id.is_none()
}

fn page_entry(mount_id: MountId, page: &PageDto, title: String, path: PathBuf) -> TreeEntry {
    let stub_frontmatter = page_frontmatter(page, &title);
    TreeEntry {
        mount_id,
        remote_id: RemoteId::new(page.id.clone()),
        kind: EntityKind::Page,
        title,
        path,
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: page.last_edited_time.clone(),
        stub_frontmatter: Some(stub_frontmatter),
    }
}

fn page_observation(mount_id: MountId, page: &PageDto) -> RemoteObservation {
    let title = page_title(page);
    let mut used_paths = BTreeSet::new();
    let path = allocate_page_path(Path::new(""), &title, &page.id, &mut used_paths);
    let mut observation = RemoteObservation::new(
        mount_id,
        RemoteId::new(page.id.clone()),
        EntityKind::Page,
        title,
        path,
    )
    .deleted(page.archived || page.in_trash)
    .with_raw_metadata_json(metadata_json(page));

    if let Some(parent_id) = parent_remote_id(page.parent.as_ref()) {
        observation = observation.with_parent(parent_id);
    }
    if let Some(remote_version) = &page.last_edited_time {
        observation = observation.with_remote_version(RemoteVersion::new(remote_version.clone()));
    }

    observation
}

fn database_entry(
    mount_id: MountId,
    database: &DatabaseDto,
    title: String,
    path: PathBuf,
) -> TreeEntry {
    TreeEntry {
        mount_id,
        remote_id: RemoteId::new(database.id.clone()),
        kind: EntityKind::Database,
        title,
        path,
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: database.last_edited_time.clone(),
        stub_frontmatter: None,
    }
}

fn database_observation(mount_id: MountId, database: &DatabaseDto) -> RemoteObservation {
    let title = database_title(database).unwrap_or_else(|| "Untitled database".to_string());
    let mut used_paths = BTreeSet::new();
    let path = allocate_directory_path(Path::new(""), &title, &database.id, &mut used_paths);
    let mut observation = RemoteObservation::new(
        mount_id,
        RemoteId::new(database.id.clone()),
        EntityKind::Database,
        title,
        path,
    )
    .deleted(database.archived || database.in_trash)
    .with_raw_metadata_json(metadata_json(database));

    if let Some(parent_id) = parent_remote_id(database.parent.as_ref()) {
        observation = observation.with_parent(parent_id);
    }
    if let Some(remote_version) = &database.last_edited_time {
        observation = observation.with_remote_version(RemoteVersion::new(remote_version.clone()));
    }

    observation
}

fn parent_remote_id(parent: Option<&ParentDto>) -> Option<RemoteId> {
    let parent = parent?;
    parent
        .page_id
        .as_ref()
        .or(parent.database_id.as_ref())
        .or(parent.data_source_id.as_ref())
        .or(parent.block_id.as_ref())
        .map(|id| RemoteId::new(id.clone()))
}

fn metadata_json<T>(value: &T) -> String
where
    T: serde::Serialize,
{
    serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
}

fn database_title(database: &DatabaseDto) -> Option<String> {
    let title = crate::render::rich_text_plain_text(&database.title);
    if title.trim().is_empty() {
        None
    } else {
        Some(title)
    }
}

pub fn allocate_page_path(
    parent_dir: &Path,
    title: &str,
    remote_id: &str,
    used_paths: &mut BTreeSet<PathBuf>,
) -> PathBuf {
    allocate_single_path(
        parent_dir,
        ProjectedPathKind::Page,
        title,
        remote_id,
        used_paths,
    )
}

fn allocate_child_paths(
    parent_dir: &Path,
    children: Vec<ProjectedChild>,
    used_paths: &mut BTreeSet<PathBuf>,
) -> Vec<ProjectedChildWithPath> {
    let bases = children
        .iter()
        .map(|child| projected_title_stem(child.title()))
        .collect::<Vec<_>>();
    let base_collision_keys = bases
        .iter()
        .map(|base| projected_path_collision_key(Path::new(base)))
        .collect::<Vec<_>>();
    let mut base_counts = BTreeMap::new();
    for key in &base_collision_keys {
        *base_counts.entry(key.clone()).or_insert(0usize) += 1;
    }

    let mut paths = (0..children.len()).map(|_| None).collect::<Vec<_>>();
    let mut suffix_groups = BTreeMap::<String, Vec<usize>>::new();
    for (index, child) in children.iter().enumerate() {
        let base = &bases[index];
        let base_collision_key = &base_collision_keys[index];
        let clean = projection_reservation(parent_dir, child.kind(), base);
        if base_counts
            .get(base_collision_key)
            .copied()
            .unwrap_or_default()
            == 1
            && projection_available(used_paths, &clean)
        {
            paths[index] = Some(reserve_projection(used_paths, clean));
        } else {
            suffix_groups
                .entry(base_collision_key.clone())
                .or_default()
                .push(index);
        }
    }

    for (_collision_key, indexes) in suffix_groups {
        for (index, path) in
            allocate_suffixed_group(parent_dir, &children, &bases, &indexes, used_paths)
        {
            paths[index] = Some(path);
        }
    }

    children
        .into_iter()
        .zip(paths)
        .map(|(child, path)| ProjectedChildWithPath {
            child,
            path: path.expect("projection path allocated"),
        })
        .collect()
}

fn allocate_suffixed_group(
    parent_dir: &Path,
    children: &[ProjectedChild],
    bases: &[String],
    indexes: &[usize],
    used_paths: &mut BTreeSet<PathBuf>,
) -> Vec<(usize, PathBuf)> {
    for short_len in [6, 8, 10, 12, 32] {
        let mut staged = BTreeSet::new();
        let mut projections = Vec::new();
        let mut available = true;

        for index in indexes {
            let child = &children[*index];
            let base = &bases[*index];
            let stem = suffixed_stem(base, child.remote_id(), short_len);
            let projection = projection_reservation(parent_dir, child.kind(), &stem);
            if projection
                .reserved
                .iter()
                .any(|path| path_collides(used_paths, path) || path_collides(&staged, path))
            {
                available = false;
                break;
            }
            staged.extend(projection.reserved.iter().cloned());
            projections.push((*index, projection));
        }

        if available {
            return projections
                .into_iter()
                .map(|(index, projection)| (index, reserve_projection(used_paths, projection)))
                .collect();
        }
    }

    unreachable!("32-character remote IDs should make sibling projected paths unique")
}

fn allocate_single_path(
    parent_dir: &Path,
    kind: ProjectedPathKind,
    title: &str,
    remote_id: &str,
    used_paths: &mut BTreeSet<PathBuf>,
) -> PathBuf {
    let base = projected_title_stem(title);
    let clean = projection_reservation(parent_dir, kind, &base);
    if projection_available(used_paths, &clean) {
        return reserve_projection(used_paths, clean);
    }

    allocate_suffixed_path_for(parent_dir, kind, remote_id, &base, used_paths)
}

fn allocate_suffixed_path_for(
    parent_dir: &Path,
    kind: ProjectedPathKind,
    remote_id: &str,
    base: &str,
    used_paths: &mut BTreeSet<PathBuf>,
) -> PathBuf {
    for short_len in [6, 8, 10, 12, 32] {
        let stem = suffixed_stem(base, remote_id, short_len);
        let projection = projection_reservation(parent_dir, kind, &stem);
        if projection_available(used_paths, &projection) {
            return reserve_projection(used_paths, projection);
        }
    }

    unreachable!("32-character remote IDs should make projected paths unique")
}

fn allocate_directory_path(
    parent_dir: &Path,
    title: &str,
    remote_id: &str,
    used_paths: &mut BTreeSet<PathBuf>,
) -> PathBuf {
    allocate_single_path(
        parent_dir,
        ProjectedPathKind::Directory,
        title,
        remote_id,
        used_paths,
    )
}

fn page_child_dir(page_path: &Path) -> PathBuf {
    page_container_path(page_path)
}

fn projection_reservation(
    parent_dir: &Path,
    kind: ProjectedPathKind,
    stem: &str,
) -> ProjectedPathReservation {
    match kind {
        ProjectedPathKind::Page => {
            let page_dir = parent_dir.join(stem);
            let file_path = page_document_path(&page_dir);
            let legacy_file_path = parent_dir.join(format!("{stem}.md"));
            ProjectedPathReservation {
                path: file_path.clone(),
                reserved: vec![file_path, page_dir, legacy_file_path],
            }
        }
        ProjectedPathKind::Directory => {
            let path = parent_dir.join(stem);
            ProjectedPathReservation {
                path: path.clone(),
                reserved: vec![path],
            }
        }
    }
}

fn projection_available(
    used_paths: &BTreeSet<PathBuf>,
    projection: &ProjectedPathReservation,
) -> bool {
    projection
        .reserved
        .iter()
        .all(|path| !path_collides(used_paths, path))
}

fn reserve_projection(
    used_paths: &mut BTreeSet<PathBuf>,
    projection: ProjectedPathReservation,
) -> PathBuf {
    for path in projection.reserved {
        used_paths.insert(path);
    }
    projection.path
}

fn path_collides(used_paths: &BTreeSet<PathBuf>, candidate: &Path) -> bool {
    if used_paths.contains(candidate) {
        return true;
    }
    let candidate_key = projected_path_collision_key(candidate);
    used_paths
        .iter()
        .any(|used| projected_path_collision_key(used) == candidate_key)
}

fn projected_path_collision_key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

fn suffixed_stem(base: &str, remote_id: &str, short_len: usize) -> String {
    format!("{} {}", base, short_id(remote_id, short_len))
}

fn projected_title_stem(title: &str) -> String {
    let mut stem = String::new();
    let mut previous_replacement = false;

    for ch in title.chars() {
        if is_invalid_path_segment_char(ch) {
            if !previous_replacement {
                stem.push('-');
                previous_replacement = true;
            }
        } else {
            stem.push(ch);
            previous_replacement = false;
        }
    }

    let stem = stem
        .trim_start()
        .trim_end_matches(|ch: char| ch.is_whitespace() || ch == '.')
        .to_string();

    if stem.is_empty() {
        return "Untitled".to_string();
    }
    if is_windows_reserved_device_basename(&stem) {
        format!("{stem}-page")
    } else {
        stem
    }
}

fn is_invalid_path_segment_char(ch: char) -> bool {
    matches!(
        ch,
        '/' | '\\' | '\0' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
    ) || ch.is_control()
}

fn is_windows_reserved_device_basename(stem: &str) -> bool {
    let basename = stem
        .split_once('.')
        .map_or(stem, |(basename, _)| basename)
        .trim_end_matches(|ch: char| ch.is_whitespace() || ch == '.');
    let upper = basename.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN" | "CONOUT"
    ) || upper
        .strip_prefix("COM")
        .and_then(|suffix| suffix.parse::<u8>().ok())
        .is_some_and(|number| (1..=9).contains(&number))
        || upper
            .strip_prefix("LPT")
            .and_then(|suffix| suffix.parse::<u8>().ok())
            .is_some_and(|number| (1..=9).contains(&number))
}

fn short_id(remote_id: &str, len: usize) -> String {
    let short = remote_id
        .chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .take(len)
        .collect::<String>();
    if short.len() >= len {
        return short;
    }

    let fallback = remote_id
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .take(len)
        .collect::<String>();
    if fallback.is_empty() {
        "id".to_string()
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    use super::{
        ProjectedChild, allocate_child_paths, allocate_page_path, enumerate_explicit_root_trees,
        projected_title_stem, resolve_notion_object_path_entries, resolve_page_path_entries,
    };
    use locality_core::model::{EntityKind, MountId, RemoteId};
    use locality_core::path_projection::PAGE_DOCUMENT_FILENAME;
    use locality_core::{LocalityError, LocalityResult};

    use crate::client::NotionApi;
    use crate::dto::{
        BlockDto, BlockListDto, DataSourceDto, DataSourceSummaryDto, DatabaseDto, DatabaseListDto,
        PageDto, PageListDto, PagePropertyDto, ParentDto, RichTextDto,
    };

    #[test]
    fn title_stem_preserves_representable_title_text() {
        assert_eq!(projected_title_stem("Roadmap 2026!"), "Roadmap 2026!");
        assert_eq!(projected_title_stem("Cycle Planning"), "Cycle Planning");
        assert_eq!(projected_title_stem("  Cycle Planning  "), "Cycle Planning");
        assert_eq!(projected_title_stem("Café/Notes"), "Café-Notes");
        assert_eq!(projected_title_stem("/Cycle Planning/"), "-Cycle Planning-");
        assert_eq!(projected_title_stem("Launch Plan."), "Launch Plan");
        assert_eq!(projected_title_stem("  Launch Plan .  "), "Launch Plan");
        assert_eq!(projected_title_stem("...\n\t"), "...-");
        assert_eq!(projected_title_stem("."), "Untitled");
        assert_eq!(projected_title_stem(".."), "Untitled");
        assert_eq!(projected_title_stem(""), "Untitled");
    }

    #[test]
    fn title_stem_replaces_windows_reserved_filename_characters() {
        assert_eq!(
            projected_title_stem(r#"Q&A: Launch? "Alpha" <Beta>|*"#),
            "Q&A- Launch- -Alpha- -Beta-"
        );
    }

    #[test]
    fn title_stem_sanitizes_windows_reserved_device_basenames() {
        assert_eq!(projected_title_stem("CON"), "CON-page");
        assert_eq!(projected_title_stem("AUX.txt"), "AUX.txt-page");
        assert_eq!(projected_title_stem("lpt1.md"), "lpt1.md-page");
        assert_eq!(projected_title_stem("COM10"), "COM10");
    }

    #[test]
    fn path_projection_reserves_page_child_directory() {
        let mut used = BTreeSet::new();
        let first = allocate_page_path(Path::new(""), "Roadmap", "abcdef123456", &mut used);
        let second = allocate_page_path(Path::new(""), "Roadmap", "abcdef999999", &mut used);

        assert_eq!(first, Path::new("Roadmap").join(PAGE_DOCUMENT_FILENAME));
        assert_eq!(
            second,
            Path::new("Roadmap abcdef").join(PAGE_DOCUMENT_FILENAME)
        );
        assert!(used.contains(Path::new("Roadmap")));
        assert!(used.contains(Path::new("Roadmap.md")));
    }

    #[test]
    fn sibling_projection_uses_clean_name_without_collision() {
        let mut used = BTreeSet::new();
        let paths = allocate_child_paths(
            Path::new(""),
            vec![ProjectedChild::Page {
                page: page("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                title: "Roadmap".to_string(),
            }],
            &mut used,
        );

        assert_eq!(
            paths[0].path,
            Path::new("Roadmap").join(PAGE_DOCUMENT_FILENAME)
        );
        assert!(used.contains(Path::new("Roadmap")));
        assert!(used.contains(Path::new("Roadmap.md")));
    }

    #[test]
    fn sibling_projection_suffixes_every_title_collision() {
        let mut used = BTreeSet::new();
        let paths = allocate_child_paths(
            Path::new(""),
            vec![
                ProjectedChild::Page {
                    page: page("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                    title: "Roadmap".to_string(),
                },
                ProjectedChild::Page {
                    page: page("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
                    title: "Roadmap".to_string(),
                },
            ],
            &mut used,
        );

        assert_eq!(
            paths[0].path,
            Path::new("Roadmap aaaaaa").join(PAGE_DOCUMENT_FILENAME)
        );
        assert_eq!(
            paths[1].path,
            Path::new("Roadmap bbbbbb").join(PAGE_DOCUMENT_FILENAME)
        );
    }

    #[test]
    fn sibling_projection_suffixes_case_only_title_collision() {
        let mut used = BTreeSet::new();
        let paths = allocate_child_paths(
            Path::new(""),
            vec![
                ProjectedChild::Page {
                    page: page("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                    title: "Roadmap".to_string(),
                },
                ProjectedChild::Page {
                    page: page("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
                    title: "roadmap".to_string(),
                },
            ],
            &mut used,
        );

        assert_eq!(
            paths[0].path,
            Path::new("Roadmap aaaaaa").join(PAGE_DOCUMENT_FILENAME)
        );
        assert_eq!(
            paths[1].path,
            Path::new("roadmap bbbbbb").join(PAGE_DOCUMENT_FILENAME)
        );
    }

    #[test]
    fn path_projection_suffixes_case_only_existing_path_collision() {
        let mut used = BTreeSet::new();
        let first = allocate_page_path(Path::new(""), "Roadmap", "aaaaaaaa", &mut used);
        let second = allocate_page_path(Path::new(""), "roadmap", "bbbbbbbb", &mut used);

        assert_eq!(first, Path::new("Roadmap").join(PAGE_DOCUMENT_FILENAME));
        assert_eq!(
            second,
            Path::new("roadmap bbbbbb").join(PAGE_DOCUMENT_FILENAME)
        );
    }

    #[test]
    fn sibling_projection_resolves_page_database_collision() {
        let mut used = BTreeSet::new();
        let paths = allocate_child_paths(
            Path::new(""),
            vec![
                ProjectedChild::Page {
                    page: page("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                    title: "Tasks".to_string(),
                },
                ProjectedChild::Database {
                    database: database("cccccccccccccccccccccccccccccccc"),
                    title: "Tasks".to_string(),
                },
            ],
            &mut used,
        );

        assert_eq!(
            paths[0].path,
            Path::new("Tasks aaaaaa").join(PAGE_DOCUMENT_FILENAME)
        );
        assert_eq!(paths[1].path, Path::new("Tasks cccccc"));
    }

    #[test]
    fn sibling_projection_lengthens_shared_short_prefixes() {
        let mut used = BTreeSet::new();
        let paths = allocate_child_paths(
            Path::new(""),
            vec![
                ProjectedChild::Page {
                    page: page("abcdef11111111111111111111111111"),
                    title: "Roadmap".to_string(),
                },
                ProjectedChild::Page {
                    page: page("abcdef22222222222222222222222222"),
                    title: "Roadmap".to_string(),
                },
            ],
            &mut used,
        );

        assert_eq!(
            paths[0].path,
            Path::new("Roadmap abcdef11").join(PAGE_DOCUMENT_FILENAME)
        );
        assert_eq!(
            paths[1].path,
            Path::new("Roadmap abcdef22").join(PAGE_DOCUMENT_FILENAME)
        );
    }

    #[test]
    fn sibling_projection_uses_alphanumeric_suffix_for_non_hex_ids() {
        let mut used = BTreeSet::new();
        let paths = allocate_child_paths(
            Path::new(""),
            vec![
                ProjectedChild::Page {
                    page: page("page-one"),
                    title: "Roadmap".to_string(),
                },
                ProjectedChild::Page {
                    page: page("page-two"),
                    title: "Roadmap".to_string(),
                },
            ],
            &mut used,
        );

        assert_eq!(
            paths[0].path,
            Path::new("Roadmap pageon").join(PAGE_DOCUMENT_FILENAME)
        );
        assert_eq!(
            paths[1].path,
            Path::new("Roadmap pagetw").join(PAGE_DOCUMENT_FILENAME)
        );
    }

    #[test]
    fn exact_page_resolution_uses_real_parent_hierarchy_before_hydration() {
        let api = FakeNotionApi::new()
            .with_database(database_with_title(
                "engineering-db",
                "Engineering Wiki",
                workspace_parent(),
                vec![DataSourceSummaryDto {
                    id: "engineering-ds".to_string(),
                    name: Some("Engineering Wiki".to_string()),
                }],
            ))
            .with_data_source(data_source(
                "engineering-ds",
                database_parent("engineering-db"),
            ))
            .with_page(page_with_title(
                "standups",
                "Standups with Locality",
                data_source_parent("engineering-ds"),
            ))
            .with_page(page_with_title(
                "standup-2026-06-26",
                "2026-06-26",
                page_parent("standups"),
            ))
            .with_database_rows("engineering-ds", vec!["standups"])
            .with_page_children(
                "standups",
                vec![child_page("standup-2026-06-26", "2026-06-26")],
            );

        let entries = resolve_page_path_entries(
            &api,
            MountId::new("notion-main"),
            None,
            &RemoteId::new("standup-2026-06-26"),
        )
        .expect("resolve exact page hierarchy");

        let resolved = entries
            .iter()
            .find(|entry| entry.remote_id.as_str() == "standup-2026-06-26")
            .expect("resolved target entry");

        assert_eq!(
            resolved.path,
            Path::new("Engineering Wiki")
                .join("Standups with Locality")
                .join("2026-06-26")
                .join(PAGE_DOCUMENT_FILENAME)
        );
        assert_ne!(
            resolved.path,
            Path::new("Engineering Wiki")
                .join("2026-06-26")
                .join(PAGE_DOCUMENT_FILENAME)
        );
    }

    #[test]
    fn exact_page_resolution_walks_block_id_parent_to_containing_page() {
        let api = FakeNotionApi::new()
            .with_page(page_with_title(
                "launch-root",
                "Launch Root",
                workspace_parent(),
            ))
            .with_page(page_with_title(
                "technical-launch-post",
                "Longer Technical Launch Post",
                block_parent("nested-section"),
            ))
            .with_block(block_with_parent(
                "nested-section",
                "toggle",
                page_parent("launch-root"),
            ))
            .with_page_children(
                "launch-root",
                vec![container_block("nested-section", "toggle")],
            )
            .with_page_children(
                "nested-section",
                vec![child_page(
                    "technical-launch-post",
                    "Longer Technical Launch Post",
                )],
            );

        let entries = resolve_page_path_entries(
            &api,
            MountId::new("notion-main"),
            None,
            &RemoteId::new("technical-launch-post"),
        )
        .expect("resolve exact page hierarchy through block parent");

        let resolved = entries
            .iter()
            .find(|entry| entry.remote_id.as_str() == "technical-launch-post")
            .expect("resolved target entry");

        assert_eq!(
            resolved.path,
            Path::new("Launch Root")
                .join("Longer Technical Launch Post")
                .join(PAGE_DOCUMENT_FILENAME)
        );
    }

    #[test]
    fn exact_object_resolution_accepts_database_ids() {
        let api = FakeNotionApi::new().with_database(database_with_title(
            "engineering-db",
            "Engineering Wiki",
            workspace_parent(),
            vec![DataSourceSummaryDto {
                id: "engineering-ds".to_string(),
                name: Some("Engineering Wiki".to_string()),
            }],
        ));

        let entries = resolve_notion_object_path_entries(
            &api,
            MountId::new("notion-main"),
            None,
            &RemoteId::new("engineering-db"),
        )
        .expect("resolve exact database hierarchy");

        let resolved = entries
            .iter()
            .find(|entry| entry.remote_id.as_str() == "engineering-db")
            .expect("resolved target database entry");

        assert_eq!(resolved.kind, EntityKind::Database);
        assert_eq!(resolved.path, Path::new("Engineering Wiki"));
    }

    #[test]
    fn notion_id_matching_ignores_hyphen_formatting_for_exact_url_resolution() {
        assert!(super::notion_ids_equal(
            "38e3ac0e-bb88-8140-94e2-d9ff17e60faa",
            "38e3ac0ebb88814094e2d9ff17e60faa"
        ));
    }

    #[test]
    fn explicit_root_rejects_page_identity_mismatch() {
        let mut api = FakeNotionApi::new();
        api.pages.insert(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            page("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        );

        let error = enumerate_explicit_root_trees(
            &api,
            MountId::new("notion-main"),
            &[RemoteId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")],
        )
        .expect_err("mismatched page identity");

        assert_eq!(
            error,
            LocalityError::InvalidState(
                "Notion explicit root request `aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa` returned page `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb`"
                    .to_string()
            )
        );
    }

    #[test]
    fn explicit_root_rejects_database_identity_mismatch() {
        let mut api = FakeNotionApi::new();
        api.databases.insert(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            database("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        );

        let error = enumerate_explicit_root_trees(
            &api,
            MountId::new("notion-main"),
            &[RemoteId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")],
        )
        .expect_err("mismatched database identity");

        assert_eq!(
            error,
            LocalityError::InvalidState(
                "Notion explicit root request `aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa` returned database `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb`"
                    .to_string()
            )
        );
    }

    fn page(id: &str) -> PageDto {
        PageDto {
            id: id.to_string(),
            parent: None,
            created_time: None,
            last_edited_time: None,
            archived: false,
            in_trash: false,
            properties: BTreeMap::new(),
        }
    }

    fn database(id: &str) -> DatabaseDto {
        DatabaseDto {
            id: id.to_string(),
            ..Default::default()
        }
    }

    fn page_with_title(id: &str, title: &str, parent: ParentDto) -> PageDto {
        let mut page = page(id);
        page.parent = Some(parent);
        page.properties
            .insert("Name".to_string(), title_property(title));
        page
    }

    fn database_with_title(
        id: &str,
        title: &str,
        parent: ParentDto,
        data_sources: Vec<DataSourceSummaryDto>,
    ) -> DatabaseDto {
        DatabaseDto {
            id: id.to_string(),
            parent: Some(parent),
            title: vec![plain_text(title)],
            data_sources,
            ..Default::default()
        }
    }

    fn data_source(id: &str, parent: ParentDto) -> DataSourceDto {
        DataSourceDto {
            id: id.to_string(),
            parent: Some(parent),
            ..Default::default()
        }
    }

    fn child_page(id: &str, title: &str) -> BlockDto {
        BlockDto {
            id: id.to_string(),
            kind: "child_page".to_string(),
            child_page: Some(crate::dto::TitleBlockDto {
                title: title.to_string(),
            }),
            ..Default::default()
        }
    }

    fn title_property(title: &str) -> PagePropertyDto {
        PagePropertyDto {
            kind: "title".to_string(),
            title: vec![plain_text(title)],
            ..Default::default()
        }
    }

    fn plain_text(value: &str) -> RichTextDto {
        RichTextDto {
            kind: "text".to_string(),
            plain_text: value.to_string(),
            ..Default::default()
        }
    }

    fn workspace_parent() -> ParentDto {
        ParentDto {
            kind: "workspace".to_string(),
            workspace: Some(true),
            ..Default::default()
        }
    }

    fn page_parent(page_id: &str) -> ParentDto {
        ParentDto {
            kind: "page_id".to_string(),
            page_id: Some(page_id.to_string()),
            ..Default::default()
        }
    }

    fn database_parent(database_id: &str) -> ParentDto {
        ParentDto {
            kind: "database_id".to_string(),
            database_id: Some(database_id.to_string()),
            ..Default::default()
        }
    }

    fn data_source_parent(data_source_id: &str) -> ParentDto {
        ParentDto {
            kind: "data_source_id".to_string(),
            data_source_id: Some(data_source_id.to_string()),
            ..Default::default()
        }
    }

    fn block_parent(block_id: &str) -> ParentDto {
        ParentDto {
            kind: "block_id".to_string(),
            block_id: Some(block_id.to_string()),
            ..Default::default()
        }
    }

    fn block_with_parent(id: &str, kind: &str, parent: ParentDto) -> BlockDto {
        BlockDto {
            id: id.to_string(),
            kind: kind.to_string(),
            parent: Some(parent),
            has_children: true,
            ..Default::default()
        }
    }

    fn container_block(id: &str, kind: &str) -> BlockDto {
        BlockDto {
            id: id.to_string(),
            kind: kind.to_string(),
            has_children: true,
            ..Default::default()
        }
    }

    #[derive(Default)]
    struct FakeNotionApi {
        pages: BTreeMap<String, PageDto>,
        blocks: BTreeMap<String, BlockDto>,
        databases: BTreeMap<String, DatabaseDto>,
        data_sources: BTreeMap<String, DataSourceDto>,
        database_rows: BTreeMap<String, Vec<String>>,
        page_children: BTreeMap<String, Vec<BlockDto>>,
    }

    impl FakeNotionApi {
        fn new() -> Self {
            Self::default()
        }

        fn with_page(mut self, page: PageDto) -> Self {
            self.pages.insert(page.id.clone(), page);
            self
        }

        fn with_block(mut self, block: BlockDto) -> Self {
            self.blocks.insert(block.id.clone(), block);
            self
        }

        fn with_database(mut self, database: DatabaseDto) -> Self {
            self.databases.insert(database.id.clone(), database);
            self
        }

        fn with_data_source(mut self, data_source: DataSourceDto) -> Self {
            self.data_sources
                .insert(data_source.id.clone(), data_source);
            self
        }

        fn with_database_rows(mut self, data_source_id: &str, rows: Vec<&str>) -> Self {
            self.database_rows.insert(
                data_source_id.to_string(),
                rows.into_iter().map(str::to_string).collect(),
            );
            self
        }

        fn with_page_children(mut self, page_id: &str, children: Vec<BlockDto>) -> Self {
            self.page_children.insert(page_id.to_string(), children);
            self
        }
    }

    impl std::fmt::Debug for FakeNotionApi {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("FakeNotionApi").finish_non_exhaustive()
        }
    }

    impl NotionApi for FakeNotionApi {
        fn retrieve_page(&self, page_id: &str) -> LocalityResult<PageDto> {
            self.pages
                .get(page_id)
                .cloned()
                .ok_or_else(|| LocalityError::RemoteNotFound(page_id.to_string()))
        }

        fn retrieve_block(&self, block_id: &str) -> LocalityResult<BlockDto> {
            self.blocks
                .get(block_id)
                .cloned()
                .ok_or_else(|| LocalityError::RemoteNotFound(block_id.to_string()))
        }

        fn retrieve_database(&self, database_id: &str) -> LocalityResult<DatabaseDto> {
            self.databases
                .get(database_id)
                .cloned()
                .ok_or_else(|| LocalityError::RemoteNotFound(database_id.to_string()))
        }

        fn retrieve_data_source(&self, data_source_id: &str) -> LocalityResult<DataSourceDto> {
            self.data_sources
                .get(data_source_id)
                .cloned()
                .ok_or_else(|| LocalityError::RemoteNotFound(data_source_id.to_string()))
        }

        fn query_data_source(
            &self,
            data_source_id: &str,
            _start_cursor: Option<&str>,
        ) -> LocalityResult<PageListDto> {
            Ok(PageListDto {
                results: self
                    .database_rows
                    .get(data_source_id)
                    .into_iter()
                    .flatten()
                    .filter_map(|page_id| self.pages.get(page_id).cloned())
                    .collect(),
                next_cursor: None,
                has_more: false,
            })
        }

        fn retrieve_block_children(
            &self,
            block_id: &str,
            _start_cursor: Option<&str>,
        ) -> LocalityResult<BlockListDto> {
            Ok(BlockListDto {
                results: self
                    .page_children
                    .get(block_id)
                    .cloned()
                    .unwrap_or_default(),
                next_cursor: None,
                has_more: false,
            })
        }

        fn search_pages(&self, _start_cursor: Option<&str>) -> LocalityResult<PageListDto> {
            Ok(PageListDto {
                results: self.pages.values().cloned().collect(),
                next_cursor: None,
                has_more: false,
            })
        }

        fn search_databases(&self, _start_cursor: Option<&str>) -> LocalityResult<DatabaseListDto> {
            Ok(DatabaseListDto {
                results: self.databases.values().cloned().collect(),
                next_cursor: None,
                has_more: false,
            })
        }

        fn update_block(
            &self,
            _block_id: &str,
            _body: serde_json::Value,
        ) -> LocalityResult<BlockDto> {
            Err(LocalityError::NotImplemented("update fake block"))
        }

        fn append_block_children(
            &self,
            _block_id: &str,
            _body: serde_json::Value,
        ) -> LocalityResult<BlockListDto> {
            Err(LocalityError::NotImplemented("append fake block children"))
        }

        fn delete_block(&self, _block_id: &str) -> LocalityResult<BlockDto> {
            Err(LocalityError::NotImplemented("delete fake block"))
        }
    }
}
