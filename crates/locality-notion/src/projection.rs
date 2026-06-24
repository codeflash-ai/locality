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

pub fn enumerate_root_page_tree(
    api: &dyn NotionApi,
    mount_id: MountId,
    root_page_id: &RemoteId,
) -> LocalityResult<Vec<TreeEntry>> {
    let root_page = api.retrieve_page(root_page_id.as_str())?;
    let mut used_paths = BTreeSet::new();
    let mut entries = Vec::new();
    let root_title = page_title(&root_page);
    let root_path = allocate_page_path(Path::new(""), &root_title, &root_page.id, &mut used_paths);

    entries.push(page_entry(
        mount_id.clone(),
        &root_page,
        root_title,
        root_path.clone(),
    ));
    enumerate_page_children(
        api,
        &mount_id,
        root_page.id.as_str(),
        page_child_dir(&root_path),
        &mut used_paths,
        &mut entries,
    )?;

    Ok(entries)
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

fn push_projected_tree_entry(
    api: &dyn NotionApi,
    mount_id: &MountId,
    projected: ProjectedChildWithPath,
    used_paths: &mut BTreeSet<PathBuf>,
    entries: &mut Vec<TreeEntry>,
) -> LocalityResult<()> {
    match projected.child {
        ProjectedChild::Page { page, title } => {
            entries.push(page_entry(
                mount_id.clone(),
                &page,
                title,
                projected.path.clone(),
            ));
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
            entries.push(database_entry(
                mount_id.clone(),
                &database,
                title,
                projected.path.clone(),
            ));
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

fn enumerate_page_children(
    api: &dyn NotionApi,
    mount_id: &MountId,
    block_id: &str,
    parent_dir: PathBuf,
    used_paths: &mut BTreeSet<PathBuf>,
    entries: &mut Vec<TreeEntry>,
) -> LocalityResult<()> {
    let children = collect_page_child_projections(api, block_id)?;
    for projected in allocate_child_paths(&parent_dir, children, used_paths) {
        push_projected_tree_entry(api, mount_id, projected, used_paths, entries)?;
    }

    Ok(())
}

fn enumerate_database_rows(
    api: &dyn NotionApi,
    mount_id: &MountId,
    database: &DatabaseDto,
    database_dir: &Path,
    used_paths: &mut BTreeSet<PathBuf>,
    entries: &mut Vec<TreeEntry>,
) -> LocalityResult<()> {
    let rows = collect_database_row_projections(api, database)?;
    for projected in allocate_child_paths(database_dir, rows, used_paths) {
        if let ProjectedChild::Page { page, title } = projected.child {
            entries.push(page_entry(
                mount_id.clone(),
                &page,
                title,
                projected.path.clone(),
            ));
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
    for data_source in &database.data_sources {
        let mut cursor = None;

        loop {
            let page = api.query_data_source(&data_source.id, cursor.as_deref())?;
            for row in page.results {
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
        .map(|child| slugify_title(child.title()))
        .collect::<Vec<_>>();
    let mut base_counts = BTreeMap::new();
    for base in &bases {
        *base_counts.entry(base.clone()).or_insert(0usize) += 1;
    }

    let mut paths = (0..children.len()).map(|_| None).collect::<Vec<_>>();
    let mut suffix_groups = BTreeMap::<String, Vec<usize>>::new();
    for (index, child) in children.iter().enumerate() {
        let base = &bases[index];
        let clean = projection_reservation(parent_dir, child.kind(), base);
        if base_counts.get(base).copied().unwrap_or_default() == 1
            && projection_available(used_paths, &clean)
        {
            paths[index] = Some(reserve_projection(used_paths, clean));
        } else {
            suffix_groups.entry(base.clone()).or_default().push(index);
        }
    }

    for (base, indexes) in suffix_groups {
        for (index, path) in
            allocate_suffixed_group(parent_dir, &children, &indexes, &base, used_paths)
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
    indexes: &[usize],
    base: &str,
    used_paths: &mut BTreeSet<PathBuf>,
) -> Vec<(usize, PathBuf)> {
    for short_len in [6, 8, 10, 12, 32] {
        let mut staged = BTreeSet::new();
        let mut projections = Vec::new();
        let mut available = true;

        for index in indexes {
            let child = &children[*index];
            let stem = suffixed_stem(base, child.remote_id(), short_len);
            let projection = projection_reservation(parent_dir, child.kind(), &stem);
            if projection
                .reserved
                .iter()
                .any(|path| used_paths.contains(path) || staged.contains(path))
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
    let base = slugify_title(title);
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
        .all(|path| !used_paths.contains(path))
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

fn suffixed_stem(base: &str, remote_id: &str, short_len: usize) -> String {
    format!("{} {}", base, short_id(remote_id, short_len))
}

fn slugify_title(title: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;

    for ch in title.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            previous_dash = false;
        } else if !previous_dash && !slug.is_empty() {
            slug.push('-');
            previous_dash = true;
        }
    }

    while slug.ends_with('-') {
        slug.pop();
    }

    if slug.is_empty() {
        "untitled".to_string()
    } else {
        slug
    }
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

    use super::{ProjectedChild, allocate_child_paths, allocate_page_path, slugify_title};
    use locality_core::path_projection::PAGE_DOCUMENT_FILENAME;

    use crate::dto::{DatabaseDto, PageDto};

    #[test]
    fn slugifies_titles_for_stable_paths() {
        assert_eq!(slugify_title("Roadmap 2026!"), "roadmap-2026");
        assert_eq!(slugify_title("..."), "untitled");
    }

    #[test]
    fn path_projection_reserves_page_child_directory() {
        let mut used = BTreeSet::new();
        let first = allocate_page_path(Path::new(""), "Roadmap", "abcdef123456", &mut used);
        let second = allocate_page_path(Path::new(""), "Roadmap", "abcdef999999", &mut used);

        assert_eq!(first, Path::new("roadmap").join(PAGE_DOCUMENT_FILENAME));
        assert_eq!(
            second,
            Path::new("roadmap abcdef").join(PAGE_DOCUMENT_FILENAME)
        );
        assert!(used.contains(Path::new("roadmap")));
        assert!(used.contains(Path::new("roadmap.md")));
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
            Path::new("roadmap").join(PAGE_DOCUMENT_FILENAME)
        );
        assert!(used.contains(Path::new("roadmap")));
        assert!(used.contains(Path::new("roadmap.md")));
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
                    title: "Roadmap!".to_string(),
                },
            ],
            &mut used,
        );

        assert_eq!(
            paths[0].path,
            Path::new("roadmap aaaaaa").join(PAGE_DOCUMENT_FILENAME)
        );
        assert_eq!(
            paths[1].path,
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
            Path::new("tasks aaaaaa").join(PAGE_DOCUMENT_FILENAME)
        );
        assert_eq!(paths[1].path, Path::new("tasks cccccc"));
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
            Path::new("roadmap abcdef11").join(PAGE_DOCUMENT_FILENAME)
        );
        assert_eq!(
            paths[1].path,
            Path::new("roadmap abcdef22").join(PAGE_DOCUMENT_FILENAME)
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
            Path::new("roadmap pageon").join(PAGE_DOCUMENT_FILENAME)
        );
        assert_eq!(
            paths[1].path,
            Path::new("roadmap pagetw").join(PAGE_DOCUMENT_FILENAME)
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
}
