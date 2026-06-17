//! Project Notion pages, child-page blocks, and databases into AgentFS tree entries.
//!
//! Paths are a local projection, not identity. The remote ID remains the stable
//! key; the filename suffix makes title changes and sibling collisions
//! recoverable without treating them as deletes.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use afs_connector::ChildContainer;
use afs_core::freshness::{RemoteObservation, RemoteVersion};
use afs_core::model::{EntityKind, HydrationState, MountId, RemoteId, TreeEntry};
use afs_core::path_projection::{page_container_path, page_document_path};
use afs_core::{AfsError, AfsResult};

use crate::client::NotionApi;
use crate::dto::{BlockDto, DatabaseDto, PageDto, ParentDto};
use crate::render::{page_frontmatter, page_title};

pub fn enumerate_root_page_tree(
    api: &dyn NotionApi,
    mount_id: MountId,
    root_page_id: &RemoteId,
) -> AfsResult<Vec<TreeEntry>> {
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

pub fn enumerate_shared_pages(api: &dyn NotionApi, mount_id: MountId) -> AfsResult<Vec<TreeEntry>> {
    let mut used_paths = BTreeSet::new();
    let mut entries = Vec::new();

    let pages = search_all_pages(api)?;
    let databases = search_all_databases(api)?;
    let accessible_page_ids = pages
        .iter()
        .map(|page| page.id.as_str())
        .collect::<BTreeSet<_>>();

    for page in pages
        .iter()
        .filter(|page| is_workspace_root_page(page, &accessible_page_ids))
    {
        let title = page_title(page);
        let path = allocate_page_path(Path::new(""), &title, &page.id, &mut used_paths);
        entries.push(page_entry(mount_id.clone(), page, title, path.clone()));
        enumerate_page_children(
            api,
            &mount_id,
            page.id.as_str(),
            page_child_dir(&path),
            &mut used_paths,
            &mut entries,
        )?;
    }

    for database in databases
        .iter()
        .filter(|database| is_workspace_root_parent(database.parent.as_ref(), &accessible_page_ids))
    {
        let title = database_title(database).unwrap_or_else(|| "Untitled database".to_string());
        let path = allocate_directory_path(Path::new(""), &title, &database.id, &mut used_paths);
        entries.push(database_entry(
            mount_id.clone(),
            database,
            title,
            path.clone(),
        ));
        enumerate_database_rows(
            api,
            &mount_id,
            database,
            &path,
            &mut used_paths,
            &mut entries,
        )?;
    }

    let projected_page_ids = entries
        .iter()
        .filter(|entry| entry.kind == EntityKind::Page)
        .map(|entry| entry.remote_id.0.clone())
        .collect::<BTreeSet<_>>();
    for page in pages
        .iter()
        .filter(|page| !projected_page_ids.contains(&page.id))
    {
        let title = page_title(page);
        let path = allocate_page_path(Path::new(""), &title, &page.id, &mut used_paths);
        entries.push(page_entry(mount_id.clone(), page, title, path));
    }

    Ok(entries)
}

pub fn list_container_children(
    api: &dyn NotionApi,
    mount_id: MountId,
    root_page_id: Option<&RemoteId>,
    container: ChildContainer,
    parent_path: &Path,
) -> AfsResult<Vec<TreeEntry>> {
    let mut used_paths = BTreeSet::new();
    match container {
        ChildContainer::Root => list_root_children(api, mount_id, root_page_id, parent_path),
        ChildContainer::PageChildren(page_id) => list_page_children(
            api,
            &mount_id,
            page_id.as_str(),
            parent_path,
            &mut used_paths,
        ),
        ChildContainer::DatabaseRows(database_id) => {
            let database = api.retrieve_database(database_id.as_str())?;
            list_database_rows(api, &mount_id, &database, parent_path, &mut used_paths)
        }
    }
}

pub fn observe_entity(
    api: &dyn NotionApi,
    mount_id: MountId,
    remote_id: &RemoteId,
) -> AfsResult<RemoteObservation> {
    match api.retrieve_page(remote_id.as_str()) {
        Ok(page) => Ok(page_observation(mount_id, &page)),
        Err(page_error) => match api.retrieve_database(remote_id.as_str()) {
            Ok(database) => Ok(database_observation(mount_id, &database)),
            Err(database_error) => Err(AfsError::InvalidState(format!(
                "notion object `{}` could not be observed as page ({page_error}) or database ({database_error})",
                remote_id.as_str()
            ))),
        },
    }
}

fn list_root_children(
    api: &dyn NotionApi,
    mount_id: MountId,
    root_page_id: Option<&RemoteId>,
    parent_path: &Path,
) -> AfsResult<Vec<TreeEntry>> {
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

    for page in pages
        .iter()
        .filter(|page| is_workspace_root_page(page, &accessible_page_ids))
    {
        let title = page_title(page);
        let path = allocate_page_path(parent_path, &title, &page.id, &mut used_paths);
        entries.push(page_entry(mount_id.clone(), page, title, path));
    }

    for database in search_all_databases(api)?
        .iter()
        .filter(|database| is_workspace_root_parent(database.parent.as_ref(), &accessible_page_ids))
    {
        let title = database_title(database).unwrap_or_else(|| "Untitled database".to_string());
        let path = allocate_directory_path(parent_path, &title, &database.id, &mut used_paths);
        entries.push(database_entry(mount_id.clone(), database, title, path));
    }

    Ok(entries)
}

fn search_all_pages(api: &dyn NotionApi) -> AfsResult<Vec<PageDto>> {
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
            return Err(AfsError::InvalidState(
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

fn search_all_databases(api: &dyn NotionApi) -> AfsResult<Vec<DatabaseDto>> {
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
            return Err(AfsError::InvalidState(
                "notion search database page had has_more without next_cursor".to_string(),
            ));
        }
    }

    Ok(databases)
}

fn list_page_children(
    api: &dyn NotionApi,
    mount_id: &MountId,
    block_id: &str,
    parent_dir: &Path,
    used_paths: &mut BTreeSet<PathBuf>,
) -> AfsResult<Vec<TreeEntry>> {
    let mut cursor = None;
    let mut entries = Vec::new();

    loop {
        let page = api.retrieve_block_children(block_id, cursor.as_deref())?;
        for block in page.results {
            project_direct_child_block(api, mount_id, block, parent_dir, used_paths, &mut entries)?;
        }

        if !page.has_more {
            break;
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            return Err(AfsError::InvalidState(
                "notion block children page had has_more without next_cursor".to_string(),
            ));
        }
    }

    Ok(entries)
}

fn project_direct_child_block(
    api: &dyn NotionApi,
    mount_id: &MountId,
    block: BlockDto,
    parent_dir: &Path,
    used_paths: &mut BTreeSet<PathBuf>,
    entries: &mut Vec<TreeEntry>,
) -> AfsResult<()> {
    match block.kind.as_str() {
        "child_page" => {
            let page = api.retrieve_page(block.id.as_str())?;
            let title = block
                .child_page
                .as_ref()
                .map(|child| child.title.clone())
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| page_title(&page));
            let path = allocate_page_path(parent_dir, &title, &page.id, used_paths);
            entries.push(page_entry(mount_id.clone(), &page, title, path));
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
            let path = allocate_directory_path(parent_dir, &title, &database.id, used_paths);
            entries.push(database_entry(mount_id.clone(), &database, title, path));
        }
        _ if block.has_children => {
            let nested =
                list_page_children(api, mount_id, block.id.as_str(), parent_dir, used_paths)?;
            entries.extend(nested);
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
    used_paths: &mut BTreeSet<PathBuf>,
) -> AfsResult<Vec<TreeEntry>> {
    let mut entries = Vec::new();
    for data_source in &database.data_sources {
        let mut cursor = None;

        loop {
            let page = api.query_data_source(&data_source.id, cursor.as_deref())?;
            for row in page.results {
                let title = page_title(&row);
                let path = allocate_page_path(database_dir, &title, &row.id, used_paths);
                entries.push(page_entry(mount_id.clone(), &row, title, path));
            }

            if !page.has_more {
                break;
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Err(AfsError::InvalidState(
                    "notion data source query page had has_more without next_cursor".to_string(),
                ));
            }
        }
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
) -> AfsResult<()> {
    let mut cursor = None;

    loop {
        let page = api.retrieve_block_children(block_id, cursor.as_deref())?;
        for block in page.results {
            project_child_block(api, mount_id, block, &parent_dir, used_paths, entries)?;
        }

        if !page.has_more {
            break;
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            return Err(AfsError::InvalidState(
                "notion block children page had has_more without next_cursor".to_string(),
            ));
        }
    }

    Ok(())
}

fn project_child_block(
    api: &dyn NotionApi,
    mount_id: &MountId,
    block: BlockDto,
    parent_dir: &Path,
    used_paths: &mut BTreeSet<PathBuf>,
    entries: &mut Vec<TreeEntry>,
) -> AfsResult<()> {
    match block.kind.as_str() {
        "child_page" => {
            let page = api.retrieve_page(block.id.as_str())?;
            let title = block
                .child_page
                .as_ref()
                .map(|child| child.title.clone())
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| page_title(&page));
            let path = allocate_page_path(parent_dir, &title, &page.id, used_paths);
            entries.push(page_entry(mount_id.clone(), &page, title, path.clone()));
            enumerate_page_children(
                api,
                mount_id,
                page.id.as_str(),
                page_child_dir(&path),
                used_paths,
                entries,
            )?;
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
            let path = allocate_directory_path(parent_dir, &title, &database.id, used_paths);
            entries.push(database_entry(
                mount_id.clone(),
                &database,
                title,
                path.clone(),
            ));
            enumerate_database_rows(api, mount_id, &database, &path, used_paths, entries)?;
        }
        _ if block.has_children => {
            enumerate_page_children(
                api,
                mount_id,
                block.id.as_str(),
                parent_dir.to_path_buf(),
                used_paths,
                entries,
            )?;
        }
        _ => {}
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
) -> AfsResult<()> {
    for data_source in &database.data_sources {
        let mut cursor = None;

        loop {
            let page = api.query_data_source(&data_source.id, cursor.as_deref())?;
            for row in page.results {
                let title = page_title(&row);
                let path = allocate_page_path(database_dir, &title, &row.id, used_paths);
                entries.push(page_entry(mount_id.clone(), &row, title, path.clone()));
                enumerate_page_children(
                    api,
                    mount_id,
                    row.id.as_str(),
                    page_child_dir(&path),
                    used_paths,
                    entries,
                )?;
            }

            if !page.has_more {
                break;
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Err(AfsError::InvalidState(
                    "notion data source query page had has_more without next_cursor".to_string(),
                ));
            }
        }
    }

    Ok(())
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
    for short_len in [6, 8, 10, 12, 32] {
        let stem = projected_stem(title, remote_id, short_len);
        let page_dir = parent_dir.join(&stem);
        let file_path = page_document_path(&page_dir);
        let legacy_file_path = parent_dir.join(format!("{stem}.md"));
        if !used_paths.contains(&file_path)
            && !used_paths.contains(&page_dir)
            && !used_paths.contains(&legacy_file_path)
        {
            used_paths.insert(file_path.clone());
            used_paths.insert(page_dir);
            used_paths.insert(legacy_file_path);
            return file_path;
        }
    }

    unreachable!("32 hex chars should make Notion page projection paths unique")
}

fn allocate_directory_path(
    parent_dir: &Path,
    title: &str,
    remote_id: &str,
    used_paths: &mut BTreeSet<PathBuf>,
) -> PathBuf {
    for short_len in [6, 8, 10, 12, 32] {
        let path = parent_dir.join(projected_stem(title, remote_id, short_len));
        if !used_paths.contains(&path) {
            used_paths.insert(path.clone());
            return path;
        }
    }

    unreachable!("32 hex chars should make Notion database projection paths unique")
}

fn page_child_dir(page_path: &Path) -> PathBuf {
    page_container_path(page_path)
}

fn projected_stem(title: &str, remote_id: &str, short_len: usize) -> String {
    format!(
        "{} ~{}",
        slugify_title(title),
        short_id(remote_id, short_len)
    )
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
    remote_id
        .chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .take(len)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{allocate_page_path, slugify_title};
    use afs_core::path_projection::PAGE_DOCUMENT_FILENAME;
    use std::collections::BTreeSet;
    use std::path::Path;

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

        assert_eq!(
            first,
            Path::new("roadmap ~abcdef").join(PAGE_DOCUMENT_FILENAME)
        );
        assert_eq!(
            second,
            Path::new("roadmap ~abcdef99").join(PAGE_DOCUMENT_FILENAME)
        );
        assert!(used.contains(Path::new("roadmap ~abcdef")));
        assert!(used.contains(Path::new("roadmap ~abcdef.md")));
    }
}
