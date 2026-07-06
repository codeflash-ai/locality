# Notion Workspace Root Hierarchy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add explicit `Private/` and `Workspace/` synthetic directories to workspace-level Notion mounts so private page creation is unambiguous and shared or team-level pages browse below `Workspace/`.

**Architecture:** Represent the synthetic Notion roots as virtual `Directory` tree entries with reserved Notion connector IDs. Push planning keeps existing remote-parent fields for compatibility and adds a defaulted `CreateParentScope` so the Notion apply layer can create private workspace pages without ever sending a synthetic ID to the Notion API. Lazy virtual filesystem listing uses a new source-root child container variant for connector-defined synthetic roots.

**Tech Stack:** Rust workspace, serde, SQLite-backed Locality state, Notion connector DTO/API layer, loc CLI JSON reports, Cargo tests, optional Linux FUSE smoke test.

---

## File Structure

- Modify `crates/locality-core/src/planner.rs`: add `CreateParentScope`, default it to `Remote`, add it to `PushOperation::CreateEntity`, and test old serialized plans still deserialize.
- Modify `crates/locality-connector/src/lib.rs`: add `ChildContainer::SourceRoot(RemoteId)` for connector-defined synthetic root directories.
- Modify `crates/locality-notion/src/projection.rs`: define Notion synthetic root constants/helpers, project workspace objects under `Workspace/`, list synthetic roots lazily, and test projection behavior with the existing fake Notion API.
- Modify `crates/locality-notion/src/apply.rs`: branch create-page request building by `CreateParentScope`, skip synthetic root concurrency/fetch work, and reject unsupported source-root create scopes before issuing API requests.
- Modify `crates/locality-notion/tests/apply.rs`: assert private workspace page create payloads omit `parent`, and assert synthetic-root preconditions are ignored.
- Modify `crates/localityd/src/virtual_fs.rs`: map Notion synthetic root directories to `ChildContainer::SourceRoot`, allow pending creates under `Private/`, and reject pending creates directly under `Workspace/`.
- Modify `crates/loc-cli/src/diff.rs`: include `parent_scope` in `PushOperationOutput::CreateEntity` for inspectable CLI plans.
- Modify `crates/loc-cli/tests/push.rs`: cover private-root create planning, workspace-root create rejection, and child-page create under an existing page below `Workspace/`.
- Modify `crates/loc-cli/tests/projection_contract.rs`: cover virtual projection listing of synthetic directories as folders without `page.md`.
- Modify docs: `docs/notion-connector.md`, `docs/cli.md`, `docs/linux-fuse.md`, and `templates/mount/AGENTS.md` so agents know to create private pages under `Private/` and child pages under existing page directories.

## Constants And Terms

Use these exact IDs and directory names:

```rust
pub const NOTION_PRIVATE_ROOT_ID: &str = "notion-root:private";
pub const NOTION_WORKSPACE_ROOT_ID: &str = "notion-root:workspace";
pub const NOTION_PRIVATE_ROOT_DIR: &str = "Private";
pub const NOTION_WORKSPACE_ROOT_DIR: &str = "Workspace";
```

Use these parent scopes:

```rust
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CreateParentScope {
    #[default]
    Remote,
    PrivateWorkspace,
    WorkspaceRoot,
}
```

`Remote` preserves current behavior. `PrivateWorkspace` is the only supported source-root create scope in this change. `WorkspaceRoot` exists so validation and diagnostics can be explicit, but Notion apply must reject it before making an API request.

## Tasks

### Task 1: Core Plan And Connector Types

**Files:**
- Modify: `crates/locality-core/src/planner.rs`
- Modify: `crates/locality-connector/src/lib.rs`

- [ ] **Step 1: Add failing parent-scope serialization tests**

Add this test module to the bottom of `crates/locality-core/src/planner.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::{CreateParentScope, PushOperation};
    use crate::model::{EntityKind, RemoteId};

    #[test]
    fn create_entity_parent_scope_defaults_to_remote_when_missing_from_json() {
        let value = serde_json::json!({
            "type": "create_entity",
            "parent_id": "page-parent",
            "parent_kind": "page",
            "title": "Child",
            "properties": {},
            "body": "",
            "source_path": "Roadmap/Child/page.md"
        });

        let operation: PushOperation =
            serde_json::from_value(value).expect("deserialize old create entity plan");

        let PushOperation::CreateEntity {
            parent_scope,
            parent_id,
            parent_kind,
            ..
        } = operation
        else {
            panic!("expected create entity");
        };
        assert_eq!(parent_scope, CreateParentScope::Remote);
        assert_eq!(parent_id, RemoteId::new("page-parent"));
        assert_eq!(parent_kind, Some(EntityKind::Page));
    }

    #[test]
    fn create_entity_parent_scope_serializes_private_workspace() {
        let operation = PushOperation::CreateEntity {
            parent_id: RemoteId::new("notion-root:private"),
            parent_kind: Some(EntityKind::Directory),
            parent_scope: CreateParentScope::PrivateWorkspace,
            title: "Private Draft".to_string(),
            properties: Default::default(),
            body: "Draft body.".to_string(),
            source_path: "Private/Private Draft/page.md".into(),
        };

        let value = serde_json::to_value(operation).expect("serialize create entity plan");

        assert_eq!(value["type"], "create_entity");
        assert_eq!(value["parent_scope"], "private_workspace");
        assert_eq!(value["parent_id"], "notion-root:private");
    }
}
```

- [ ] **Step 2: Run the focused core test and verify red**

Run:

```bash
cargo test -p locality-core create_entity_parent_scope -- --nocapture
```

Expected: fail to compile because `CreateParentScope` and the `parent_scope` field do not exist.

- [ ] **Step 3: Add `CreateParentScope` and the defaulted field**

In `crates/locality-core/src/planner.rs`, insert the enum immediately before `PushOperation` and add the field to `CreateEntity`:

```rust
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CreateParentScope {
    #[default]
    Remote,
    PrivateWorkspace,
    WorkspaceRoot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PushOperation {
    // existing variants stay unchanged
    CreateEntity {
        parent_id: RemoteId,
        #[serde(default)]
        parent_kind: Option<EntityKind>,
        #[serde(default)]
        parent_scope: CreateParentScope,
        title: String,
        #[serde(default)]
        properties: BTreeMap<String, PropertyValue>,
        #[serde(default)]
        body: String,
        #[serde(default)]
        source_path: std::path::PathBuf,
    },
}
```

- [ ] **Step 4: Add the connector source-root child container**

In `crates/locality-connector/src/lib.rs`, extend `ChildContainer`:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChildContainer {
    /// The mount root. For workspace mounts, this is the visible workspace root;
    /// for scoped mounts, this is the configured remote root.
    Root,
    /// Connector-defined synthetic source root, such as Notion's Private or
    /// Workspace directory.
    SourceRoot(RemoteId),
    /// Child pages/databases under a page.
    PageChildren(RemoteId),
    /// Row pages under a database-like collection.
    DatabaseRows(RemoteId),
}
```

- [ ] **Step 5: Update existing create-operation construction sites to compile**

Run:

```bash
rg "PushOperation::CreateEntity" crates -n
```

For every `PushOperation::CreateEntity { ... }` literal that represents current remote-parent behavior, add:

```rust
parent_scope: locality_core::planner::CreateParentScope::Remote,
```

When the file already imports planner types, prefer extending the import:

```rust
use locality_core::planner::{CreateParentScope, PropertyValue, PushOperation, PushPlan};
```

and use:

```rust
parent_scope: CreateParentScope::Remote,
```

For pattern matches that do not use the new field, keep or add `..`:

```rust
PushOperation::CreateEntity { parent_id, parent_kind, .. } => {
    // existing body
}
```

- [ ] **Step 6: Run the focused core test and connector compile check**

Run:

```bash
cargo test -p locality-core create_entity_parent_scope -- --nocapture
cargo check -p locality-connector
```

Expected: both pass.

- [ ] **Step 7: Commit Task 1**

```bash
git add crates/locality-core/src/planner.rs crates/locality-connector/src/lib.rs
git commit -m "feat: add create parent scope"
```

### Task 2: Notion Synthetic Root Projection

**Files:**
- Modify: `crates/locality-notion/src/projection.rs`

- [ ] **Step 1: Add failing projection tests**

Inside the existing `#[cfg(test)] mod tests` in `crates/locality-notion/src/projection.rs`, extend the `use super::{...}` import to include the functions and constants under test:

```rust
use super::{
    NOTION_PRIVATE_ROOT_DIR, NOTION_PRIVATE_ROOT_ID, NOTION_WORKSPACE_ROOT_DIR,
    NOTION_WORKSPACE_ROOT_ID, ProjectedChild, allocate_child_paths, allocate_page_path,
    enumerate_root_page_tree, enumerate_shared_pages, list_container_children,
    resolve_page_path_entries, slugify_title,
};
use locality_connector::ChildContainer;
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId, TreeEntry};
```

Then add these tests before `notion_id_matching_ignores_hyphen_formatting_for_exact_url_resolution`:

```rust
#[test]
fn workspace_enumeration_adds_synthetic_roots_and_places_workspace_objects_under_workspace() {
    let api = FakeNotionApi::new()
        .with_page(page_with_title(
            "shared-launch",
            "Shared Launch",
            workspace_parent(),
        ))
        .with_page(page_with_title(
            "title-private",
            "Private",
            workspace_parent(),
        ))
        .with_database(database_with_title(
            "tasks-db",
            "Tasks",
            workspace_parent(),
            Vec::new(),
        ));

    let entries = enumerate_shared_pages(&api, MountId::new("notion-main"))
        .expect("enumerate shared pages");

    assert_tree_entry(
        &entries,
        NOTION_PRIVATE_ROOT_ID,
        EntityKind::Directory,
        NOTION_PRIVATE_ROOT_DIR,
        NOTION_PRIVATE_ROOT_DIR,
        HydrationState::Virtual,
    );
    assert_tree_entry(
        &entries,
        NOTION_WORKSPACE_ROOT_ID,
        EntityKind::Directory,
        NOTION_WORKSPACE_ROOT_DIR,
        NOTION_WORKSPACE_ROOT_DIR,
        HydrationState::Virtual,
    );
    assert_tree_entry(
        &entries,
        "shared-launch",
        EntityKind::Page,
        "Shared Launch",
        "Workspace/shared-launch/page.md",
        HydrationState::Stub,
    );
    assert_tree_entry(
        &entries,
        "title-private",
        EntityKind::Page,
        "Private",
        "Workspace/private/page.md",
        HydrationState::Stub,
    );
    assert_tree_entry(
        &entries,
        "tasks-db",
        EntityKind::Database,
        "Tasks",
        "Workspace/tasks",
        HydrationState::Stub,
    );
}

#[test]
fn root_page_enumeration_does_not_add_workspace_synthetic_roots() {
    let api = FakeNotionApi::new().with_page(page_with_title(
        "root-page",
        "Root Page",
        workspace_parent(),
    ));

    let entries = enumerate_root_page_tree(
        &api,
        MountId::new("notion-main"),
        &RemoteId::new("root-page"),
    )
    .expect("enumerate root page tree");

    assert!(entries.iter().all(|entry| entry.remote_id.as_str() != NOTION_PRIVATE_ROOT_ID));
    assert!(entries.iter().all(|entry| entry.remote_id.as_str() != NOTION_WORKSPACE_ROOT_ID));
    assert_tree_entry(
        &entries,
        "root-page",
        EntityKind::Page,
        "Root Page",
        "root-page/page.md",
        HydrationState::Stub,
    );
}

#[test]
fn workspace_root_children_list_synthetic_roots_and_workspace_source_root_lists_remote_children() {
    let api = FakeNotionApi::new().with_page(page_with_title(
        "team-page",
        "Team Page",
        workspace_parent(),
    ));

    let root_children = list_container_children(
        &api,
        MountId::new("notion-main"),
        None,
        ChildContainer::Root,
        Path::new(""),
    )
    .expect("list mount root children");

    assert_tree_entry(
        &root_children,
        NOTION_PRIVATE_ROOT_ID,
        EntityKind::Directory,
        NOTION_PRIVATE_ROOT_DIR,
        NOTION_PRIVATE_ROOT_DIR,
        HydrationState::Virtual,
    );
    assert_tree_entry(
        &root_children,
        NOTION_WORKSPACE_ROOT_ID,
        EntityKind::Directory,
        NOTION_WORKSPACE_ROOT_DIR,
        NOTION_WORKSPACE_ROOT_DIR,
        HydrationState::Virtual,
    );

    let workspace_children = list_container_children(
        &api,
        MountId::new("notion-main"),
        None,
        ChildContainer::SourceRoot(RemoteId::new(NOTION_WORKSPACE_ROOT_ID)),
        Path::new(NOTION_WORKSPACE_ROOT_DIR),
    )
    .expect("list workspace synthetic root");

    assert_tree_entry(
        &workspace_children,
        "team-page",
        EntityKind::Page,
        "Team Page",
        "Workspace/team-page/page.md",
        HydrationState::Stub,
    );

    let private_children = list_container_children(
        &api,
        MountId::new("notion-main"),
        None,
        ChildContainer::SourceRoot(RemoteId::new(NOTION_PRIVATE_ROOT_ID)),
        Path::new(NOTION_PRIVATE_ROOT_DIR),
    )
    .expect("list private synthetic root");

    assert!(private_children.is_empty());
}

fn assert_tree_entry(
    entries: &[TreeEntry],
    remote_id: &str,
    kind: EntityKind,
    title: &str,
    path: &str,
    hydration: HydrationState,
) {
    let entry = entries
        .iter()
        .find(|entry| entry.remote_id.as_str() == remote_id)
        .unwrap_or_else(|| panic!("missing entry `{remote_id}`"));
    assert_eq!(entry.kind, kind, "{remote_id}");
    assert_eq!(entry.title, title, "{remote_id}");
    assert_eq!(entry.path, Path::new(path), "{remote_id}");
    assert_eq!(entry.hydration, hydration, "{remote_id}");
}
```

- [ ] **Step 2: Update the fake API search fixture**

Change `FakeNotionApi::search_pages` in the same test module from returning `PageListDto::default()` to:

```rust
fn search_pages(&self, _start_cursor: Option<&str>) -> LocalityResult<PageListDto> {
    Ok(PageListDto {
        results: self.pages.values().cloned().collect(),
        next_cursor: None,
        has_more: false,
    })
}
```

- [ ] **Step 3: Run the projection tests and verify red**

Run:

```bash
cargo test -p locality-notion projection::tests::workspace_enumeration_adds_synthetic_roots_and_places_workspace_objects_under_workspace -- --nocapture
cargo test -p locality-notion projection::tests::workspace_root_children_list_synthetic_roots_and_workspace_source_root_lists_remote_children -- --nocapture
```

Expected: fail to compile or fail assertions because the constants, `SourceRoot` handling, and root remapping do not exist yet.

- [ ] **Step 4: Add synthetic root constants and helpers**

Near the top of `crates/locality-notion/src/projection.rs`, after the imports, add:

```rust
pub const NOTION_PRIVATE_ROOT_ID: &str = "notion-root:private";
pub const NOTION_WORKSPACE_ROOT_ID: &str = "notion-root:workspace";
pub const NOTION_PRIVATE_ROOT_DIR: &str = "Private";
pub const NOTION_WORKSPACE_ROOT_DIR: &str = "Workspace";

pub fn is_notion_private_root_id(remote_id: &RemoteId) -> bool {
    remote_id.as_str() == NOTION_PRIVATE_ROOT_ID
}

pub fn is_notion_workspace_root_id(remote_id: &RemoteId) -> bool {
    remote_id.as_str() == NOTION_WORKSPACE_ROOT_ID
}

pub fn is_notion_synthetic_root_id(remote_id: &RemoteId) -> bool {
    is_notion_private_root_id(remote_id) || is_notion_workspace_root_id(remote_id)
}

pub fn notion_private_root_remote_id() -> RemoteId {
    RemoteId::new(NOTION_PRIVATE_ROOT_ID)
}

pub fn notion_workspace_root_remote_id() -> RemoteId {
    RemoteId::new(NOTION_WORKSPACE_ROOT_ID)
}
```

Add helper functions below `resolve_page_path_entries`:

```rust
fn synthetic_workspace_root_entries(mount_id: &MountId) -> Vec<TreeEntry> {
    vec![
        synthetic_root_entry(
            mount_id,
            NOTION_PRIVATE_ROOT_ID,
            NOTION_PRIVATE_ROOT_DIR,
            NOTION_PRIVATE_ROOT_DIR,
        ),
        synthetic_root_entry(
            mount_id,
            NOTION_WORKSPACE_ROOT_ID,
            NOTION_WORKSPACE_ROOT_DIR,
            NOTION_WORKSPACE_ROOT_DIR,
        ),
    ]
}

fn synthetic_root_entry(
    mount_id: &MountId,
    remote_id: &str,
    title: &str,
    path: &str,
) -> TreeEntry {
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: RemoteId::new(remote_id),
        kind: EntityKind::Directory,
        title: title.to_string(),
        path: PathBuf::from(path),
        hydration: HydrationState::Virtual,
        content_hash: None,
        remote_edited_at: None,
        stub_frontmatter: None,
    }
}
```

- [ ] **Step 5: Remap workspace enumeration under `Workspace/`**

In `enumerate_shared_pages`, insert synthetic roots before remote children and change both `allocate_child_paths(Path::new(""), ...)` calls for workspace root/fallback objects to use `Path::new(NOTION_WORKSPACE_ROOT_DIR)`:

```rust
entries.extend(synthetic_workspace_root_entries(&mount_id));

for projected in allocate_child_paths(
    Path::new(NOTION_WORKSPACE_ROOT_DIR),
    root_children,
    &mut used_paths,
) {
    push_projected_tree_entry(api, &mount_id, projected, &mut used_paths, &mut entries)?;
}

// fallback pages also allocate below Workspace/
for projected in allocate_child_paths(
    Path::new(NOTION_WORKSPACE_ROOT_DIR),
    fallback_pages,
    &mut used_paths,
) {
    push_projected_listing_entry(&mount_id, projected, &mut entries);
}
```

Keep `enumerate_root_page_tree` unchanged so root-page mounts do not emit synthetic roots.

- [ ] **Step 6: Add lazy listing for synthetic roots**

Change `list_container_children`:

```rust
match container {
    ChildContainer::Root => list_root_children(api, mount_id, root_page_id, parent_path),
    ChildContainer::SourceRoot(remote_id) if is_notion_private_root_id(&remote_id) => {
        Ok(Vec::new())
    }
    ChildContainer::SourceRoot(remote_id) if is_notion_workspace_root_id(&remote_id) => {
        list_workspace_root_children(api, mount_id, parent_path)
    }
    ChildContainer::SourceRoot(remote_id) => Err(LocalityError::InvalidState(format!(
        "unknown Notion source root `{}`",
        remote_id.as_str()
    ))),
    ChildContainer::PageChildren(page_id) => {
        list_page_children(api, &mount_id, page_id.as_str(), parent_path)
    }
    ChildContainer::DatabaseRows(database_id) => {
        let database = api.retrieve_database(database_id.as_str())?;
        list_database_rows(api, &mount_id, &database, parent_path)
    }
}
```

Split the workspace-root search portion of `list_root_children` into a new helper:

```rust
fn list_workspace_root_children(
    api: &dyn NotionApi,
    mount_id: MountId,
    parent_path: &Path,
) -> LocalityResult<Vec<TreeEntry>> {
    let mut used_paths = BTreeSet::new();
    let pages = search_all_pages(api)?;
    let accessible_page_ids = pages
        .iter()
        .map(|page| page.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut root_children = Vec::new();
    for page in pages
        .iter()
        .filter(|page| is_workspace_root_page(page, &accessible_page_ids))
    {
        root_children.push(ProjectedChild::Page {
            page: page.clone(),
            title: page_title(page),
        });
    }
    for database in search_all_databases(api)?
        .iter()
        .filter(|database| is_workspace_root_parent(database.parent.as_ref(), &accessible_page_ids))
    {
        root_children.push(ProjectedChild::Database {
            database: database.clone(),
            title: database_title(database).unwrap_or_else(|| "Untitled database".to_string()),
        });
    }
    let mut entries = Vec::new();
    for projected in allocate_child_paths(parent_path, root_children, &mut used_paths) {
        push_projected_listing_entry(&mount_id, projected, &mut entries);
    }
    Ok(entries)
}
```

Then make workspace-level `list_root_children` return only the synthetic roots:

```rust
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

    Ok(synthetic_workspace_root_entries(&mount_id))
}
```

- [ ] **Step 7: Resolve exact paths for workspace-parent objects below `Workspace/`**

In `ExactPathResolver::parent_listing`, change the workspace-parent cases from an empty root path to the workspace synthetic root path:

```rust
if parent.workspace == Some(true) || parent.kind == "workspace" {
    return Ok(ParentListing::Root(PathBuf::from(NOTION_WORKSPACE_ROOT_DIR)));
}
```

For `None`, keep the same workspace fallback:

```rust
let Some(parent) = parent else {
    return Ok(ParentListing::Root(PathBuf::from(NOTION_WORKSPACE_ROOT_DIR)));
};
```

- [ ] **Step 8: Run projection tests green**

Run:

```bash
cargo test -p locality-notion projection::tests::workspace_enumeration_adds_synthetic_roots_and_places_workspace_objects_under_workspace -- --nocapture
cargo test -p locality-notion projection::tests::root_page_enumeration_does_not_add_workspace_synthetic_roots -- --nocapture
cargo test -p locality-notion projection::tests::workspace_root_children_list_synthetic_roots_and_workspace_source_root_lists_remote_children -- --nocapture
cargo test -p locality-notion projection::tests::exact_page_resolution_uses_real_parent_hierarchy_before_hydration -- --nocapture
```

Expected: all pass, with the exact-page resolution expectation updated from `engineering-wiki/...` to `Workspace/engineering-wiki/...` if the target parent is workspace-root.

- [ ] **Step 9: Commit Task 2**

```bash
git add crates/locality-notion/src/projection.rs
git commit -m "feat: project notion workspace roots"
```

### Task 3: Virtual Filesystem Source Roots

**Files:**
- Modify: `crates/localityd/src/virtual_fs.rs`
- Modify: `crates/loc-cli/tests/projection_contract.rs`

- [ ] **Step 1: Add failing virtual filesystem tests**

In `crates/localityd/src/virtual_fs.rs`, inside its existing test module, import the Notion constants:

```rust
use locality_notion::projection::{
    NOTION_PRIVATE_ROOT_DIR, NOTION_PRIVATE_ROOT_ID, NOTION_WORKSPACE_ROOT_DIR,
    NOTION_WORKSPACE_ROOT_ID,
};
```

Add this helper in the test module:

```rust
fn notion_synthetic_root_entity(
    mount_id: &MountId,
    remote_id: &str,
    title: &str,
    path: &str,
) -> EntityRecord {
    EntityRecord::new(
        mount_id.clone(),
        RemoteId::new(remote_id),
        EntityKind::Directory,
        title,
        path,
    )
    .with_hydration(HydrationState::Virtual)
}
```

Add these tests near the existing create-directory tests:

```rust
#[test]
fn notion_private_synthetic_root_accepts_pending_page_directory_create() {
    let mount_id = MountId::new("notion-main");
    let state_root = temp_root("loc-virtual-fs-notion-private-root-create");
    let content_root = state_root.join("content/notion-main/files");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(mount_id.clone(), "notion", "/tmp/loc/notion")
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    store
        .save_entity(notion_synthetic_root_entity(
            &mount_id,
            NOTION_PRIVATE_ROOT_ID,
            NOTION_PRIVATE_ROOT_DIR,
            NOTION_PRIVATE_ROOT_DIR,
        ))
        .expect("save private root");

    let created = create_virtual_fs_directory(
        &mut store,
        &content_root,
        &mount_id,
        NOTION_PRIVATE_ROOT_ID,
        "Scratch Idea",
    )
    .expect("create under private root");

    assert!(created.identifier.starts_with("children:local:"));
    assert_eq!(created.item.path, "Private/Scratch Idea");
    assert_eq!(
        std::fs::read(content_root.join("Private/Scratch Idea/page.md"))
            .expect("read pending private page cache"),
        b""
    );
    let mutation = store
        .list_virtual_mutations(&mount_id)
        .expect("list mutations")
        .pop()
        .expect("pending create");
    assert_eq!(mutation.projected_path, Path::new("Private/Scratch Idea/page.md"));
    assert_eq!(
        mutation.parent_remote_id.as_ref().map(|id| id.as_str()),
        Some(NOTION_PRIVATE_ROOT_ID)
    );

    let _ = std::fs::remove_dir_all(state_root);
}

#[test]
fn notion_workspace_synthetic_root_rejects_pending_page_directory_create() {
    let mount_id = MountId::new("notion-main");
    let state_root = temp_root("loc-virtual-fs-notion-workspace-root-create");
    let content_root = state_root.join("content/notion-main/files");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(mount_id.clone(), "notion", "/tmp/loc/notion")
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    store
        .save_entity(notion_synthetic_root_entity(
            &mount_id,
            NOTION_WORKSPACE_ROOT_ID,
            NOTION_WORKSPACE_ROOT_DIR,
            NOTION_WORKSPACE_ROOT_DIR,
        ))
        .expect("save workspace root");

    let error = create_virtual_fs_directory(
        &mut store,
        &content_root,
        &mount_id,
        NOTION_WORKSPACE_ROOT_ID,
        "New Team Page",
    )
    .expect_err("workspace root create must be rejected");

    assert!(error.to_string().contains("New root workspace pages are ambiguous"));
    assert!(
        store
            .list_virtual_mutations(&mount_id)
            .expect("list mutations")
            .is_empty()
    );

    let _ = std::fs::remove_dir_all(state_root);
}
```

- [ ] **Step 2: Add a shared virtual projection contract test**

In `crates/loc-cli/tests/projection_contract.rs`, import the Notion constants:

```rust
use locality_notion::projection::{
    NOTION_PRIVATE_ROOT_DIR, NOTION_PRIVATE_ROOT_ID, NOTION_WORKSPACE_ROOT_DIR,
    NOTION_WORKSPACE_ROOT_ID,
};
```

Add this test after `virtual_projection_modes_share_browse_hydrate_write_contract`:

```rust
#[test]
fn virtual_projection_modes_show_notion_synthetic_roots_as_folders_without_page_documents() {
    for projection in virtual_projection_modes() {
        let fixture = ProjectionFixture::new(projection.clone());
        let mut store = fixture.store();
        fixture.seed_notion_workspace_roots(&mut store);
        let content_root = fixture.content_root();

        let mount = fixture.mount_config();
        let mount_point_root = virtual_fs_children_with_content_root(
            &store,
            &content_root,
            &fixture.mount_id,
            &mount_point_identifier(&mount),
        )
        .expect("browse mount point root");

        assert_child_folder(&mount_point_root.children, NOTION_PRIVATE_ROOT_DIR);
        assert_child_folder(&mount_point_root.children, NOTION_WORKSPACE_ROOT_DIR);
        assert!(
            !fixture.content_path("Private/page.md").exists(),
            "{projection:?} synthetic Private root must not expose page.md"
        );
        assert!(
            !fixture.content_path("Workspace/page.md").exists(),
            "{projection:?} synthetic Workspace root must not expose page.md"
        );
    }
}
```

Add this method to `impl ProjectionFixture`:

```rust
fn seed_notion_workspace_roots<S>(&self, store: &mut S)
where
    S: MountRepository + EntityRepository,
{
    store
        .save_mount(self.mount_config())
        .expect("save mount");
    store
        .save_entity(
            EntityRecord::new(
                self.mount_id.clone(),
                RemoteId::new(NOTION_PRIVATE_ROOT_ID),
                EntityKind::Directory,
                NOTION_PRIVATE_ROOT_DIR,
                NOTION_PRIVATE_ROOT_DIR,
            )
            .with_hydration(HydrationState::Virtual),
        )
        .expect("save private root");
    store
        .save_entity(
            EntityRecord::new(
                self.mount_id.clone(),
                RemoteId::new(NOTION_WORKSPACE_ROOT_ID),
                EntityKind::Directory,
                NOTION_WORKSPACE_ROOT_DIR,
                NOTION_WORKSPACE_ROOT_DIR,
            )
            .with_hydration(HydrationState::Virtual),
        )
        .expect("save workspace root");
}
```

- [ ] **Step 3: Run the virtual filesystem tests and verify red**

Run:

```bash
cargo test -p localityd notion_private_synthetic_root_accepts_pending_page_directory_create -- --nocapture
cargo test -p localityd notion_workspace_synthetic_root_rejects_pending_page_directory_create -- --nocapture
cargo test -p loc-cli --test projection_contract virtual_projection_modes_show_notion_synthetic_roots_as_folders_without_page_documents -- --nocapture
```

Expected: fail because `Directory` containers are not recognized as lazy source roots or creatable private roots.

- [ ] **Step 4: Teach virtual FS to identify Notion synthetic roots**

In `crates/localityd/src/virtual_fs.rs`, import:

```rust
use locality_notion::projection::{
    is_notion_private_root_id, is_notion_workspace_root_id, NOTION_PRIVATE_ROOT_ID,
    NOTION_WORKSPACE_ROOT_ID,
};
```

Add helpers near `child_container_for_identifier`:

```rust
fn is_notion_private_root_entity(mount: &MountConfig, entity: &EntityRecord) -> bool {
    mount.connector == "notion" && is_notion_private_root_id(&entity.remote_id)
}

fn is_notion_workspace_root_entity(mount: &MountConfig, entity: &EntityRecord) -> bool {
    mount.connector == "notion" && is_notion_workspace_root_id(&entity.remote_id)
}

fn notion_workspace_root_create_error() -> LocalityError {
    LocalityError::Unsupported(
        "New root workspace pages are ambiguous because Notion does not expose a stable teamspace parent through this API. Create under Private/ for a private page, or create below an existing page that Locality can use as the parent."
    )
}
```

Change `child_container_for_identifier` so synthetic Notion directories are listable:

```rust
Ok(match entity.kind {
    EntityKind::Page => Some(ChildContainer::PageChildren(remote_id)),
    EntityKind::Database => Some(ChildContainer::DatabaseRows(remote_id)),
    EntityKind::Directory
        if is_notion_private_root_entity(mount, entity)
            || is_notion_workspace_root_entity(mount, entity) =>
    {
        Some(ChildContainer::SourceRoot(remote_id))
    }
    EntityKind::Directory | EntityKind::Asset | EntityKind::Unknown(_) => None,
})
```

- [ ] **Step 5: Allow `Private/` and reject `Workspace/` in create parent resolution**

In `create_parent_remote_id`, change the `EntityKind::Directory` match arm:

```rust
match entity.kind {
    EntityKind::Page | EntityKind::Database => Ok(remote_id),
    EntityKind::Directory if is_notion_private_root_entity(mount, entity) => Ok(remote_id),
    EntityKind::Directory if is_notion_workspace_root_entity(mount, entity) => {
        Err(notion_workspace_root_create_error())
    }
    EntityKind::Directory | EntityKind::Asset | EntityKind::Unknown(_) => {
        Err(LocalityError::Unsupported(
            "new virtual filesystem files must be created inside a page or database directory",
        ))
    }
}
```

- [ ] **Step 6: Run virtual filesystem tests green**

Run:

```bash
cargo test -p localityd notion_private_synthetic_root_accepts_pending_page_directory_create -- --nocapture
cargo test -p localityd notion_workspace_synthetic_root_rejects_pending_page_directory_create -- --nocapture
cargo test -p loc-cli --test projection_contract virtual_projection_modes_show_notion_synthetic_roots_as_folders_without_page_documents -- --nocapture
```

Expected: all pass.

- [ ] **Step 7: Commit Task 3**

```bash
git add crates/localityd/src/virtual_fs.rs crates/loc-cli/tests/projection_contract.rs
git commit -m "feat: support notion synthetic roots in virtual fs"
```

### Task 4: Push Planning For Private And Workspace Roots

**Files:**
- Modify: `crates/localityd/src/push.rs`
- Modify: `crates/loc-cli/src/diff.rs`
- Modify: `crates/loc-cli/tests/push.rs`

- [ ] **Step 1: Add failing CLI push planning tests**

In `crates/loc-cli/tests/push.rs`, import:

```rust
use locality_notion::projection::{
    NOTION_PRIVATE_ROOT_DIR, NOTION_PRIVATE_ROOT_ID, NOTION_WORKSPACE_ROOT_DIR,
    NOTION_WORKSPACE_ROOT_ID,
};
```

Add these tests after `push_safe_plan_with_yes_stops_at_apply_boundary`:

```rust
#[test]
fn push_create_under_notion_private_root_plans_private_workspace_parent_scope() {
    let fixture = PushFixture::new();
    let mut store = fixture.store_with_notion_workspace_roots();
    let path = fixture.write_raw(
        "Private/Scratch Idea/page.md",
        "---\ntitle: Scratch Idea\n---\nPrivate body.",
    );

    let report = run_push(&store, &path, PushOptions::default()).expect("push report");

    assert!(!report.ok);
    assert_eq!(report.action, "confirm_plan");
    let plan = report.plan.expect("plan");
    assert_eq!(plan.affected_entities, vec![NOTION_PRIVATE_ROOT_ID.to_string()]);
    let [operation] = plan.operations.as_slice() else {
        panic!("expected one create operation");
    };
    let loc_cli::diff::PushOperationOutput::CreateEntity {
        parent_id,
        parent_scope,
        title,
        source_path,
        ..
    } = operation
    else {
        panic!("expected create operation");
    };
    assert_eq!(parent_id, NOTION_PRIVATE_ROOT_ID);
    assert_eq!(parent_scope, "private_workspace");
    assert_eq!(title, "Scratch Idea");
    assert_eq!(source_path, "Private/Scratch Idea/page.md");
}

#[test]
fn push_create_directly_under_notion_workspace_root_returns_ambiguity_validation() {
    let fixture = PushFixture::new();
    let store = fixture.store_with_notion_workspace_roots();
    let path = fixture.write_raw(
        "Workspace/New Team Page/page.md",
        "---\ntitle: New Team Page\n---\nAmbiguous body.",
    );

    let report = run_push(
        &store,
        &path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push report");

    assert!(!report.ok);
    assert_eq!(report.action, "fix_validation");
    assert_eq!(report.validation[0].code, "notion_workspace_root_create_ambiguous");
    assert!(
        report.validation[0]
            .message
            .contains("New root workspace pages are ambiguous")
    );
    assert!(report.plan.is_none());
}

#[test]
fn push_create_below_existing_page_under_workspace_uses_real_page_parent_scope() {
    let fixture = PushFixture::new();
    let mut store = fixture.store_with_notion_workspace_roots();
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("shared-launch"),
                EntityKind::Page,
                "Shared Launch",
                "Workspace/Shared Launch/page.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save shared page");
    let path = fixture.write_raw(
        "Workspace/Shared Launch/Follow-up/page.md",
        "---\ntitle: Follow-up\n---\nChild body.",
    );

    let report = run_push(&store, &path, PushOptions::default()).expect("push report");

    assert!(!report.ok);
    assert_eq!(report.action, "confirm_plan");
    let plan = report.plan.expect("plan");
    assert_eq!(plan.affected_entities, vec!["shared-launch".to_string()]);
    let [operation] = plan.operations.as_slice() else {
        panic!("expected one create operation");
    };
    let loc_cli::diff::PushOperationOutput::CreateEntity {
        parent_id,
        parent_scope,
        title,
        ..
    } = operation
    else {
        panic!("expected create operation");
    };
    assert_eq!(parent_id, "shared-launch");
    assert_eq!(parent_scope, "remote");
    assert_eq!(title, "Follow-up");
}
```

Add this fixture method to `impl PushFixture`:

```rust
fn store_with_notion_workspace_roots(&self) -> InMemoryStateStore {
    let mut store = self.store();
    store
        .save_entity(
            EntityRecord::new(
                self.mount_id.clone(),
                RemoteId::new(NOTION_PRIVATE_ROOT_ID),
                EntityKind::Directory,
                NOTION_PRIVATE_ROOT_DIR,
                NOTION_PRIVATE_ROOT_DIR,
            )
            .with_hydration(HydrationState::Virtual),
        )
        .expect("save private root");
    store
        .save_entity(
            EntityRecord::new(
                self.mount_id.clone(),
                RemoteId::new(NOTION_WORKSPACE_ROOT_ID),
                EntityKind::Directory,
                NOTION_WORKSPACE_ROOT_DIR,
                NOTION_WORKSPACE_ROOT_DIR,
            )
            .with_hydration(HydrationState::Virtual),
        )
        .expect("save workspace root");
    store
}
```

- [ ] **Step 2: Run push planning tests and verify red**

Run:

```bash
cargo test -p loc-cli --test push push_create_under_notion_private_root_plans_private_workspace_parent_scope -- --nocapture
cargo test -p loc-cli --test push push_create_directly_under_notion_workspace_root_returns_ambiguity_validation -- --nocapture
cargo test -p loc-cli --test push push_create_below_existing_page_under_workspace_uses_real_page_parent_scope -- --nocapture
```

Expected: fail because `parent_scope` is not present in CLI output and push planning treats both synthetic roots as ordinary directories.

- [ ] **Step 3: Include `parent_scope` in CLI plan output**

In `crates/loc-cli/src/diff.rs`, import:

```rust
use locality_core::planner::CreateParentScope;
```

Add the field to `PushOperationOutput::CreateEntity`:

```rust
CreateEntity {
    parent_id: String,
    parent_scope: String,
    title: String,
    keys: Vec<String>,
    properties: Vec<PropertyUpdateOutput>,
    body: String,
    source_path: String,
},
```

Add this helper near `impl From<PushOperation> for PushOperationOutput`:

```rust
fn create_parent_scope_name(scope: &CreateParentScope) -> &'static str {
    match scope {
        CreateParentScope::Remote => "remote",
        CreateParentScope::PrivateWorkspace => "private_workspace",
        CreateParentScope::WorkspaceRoot => "workspace_root",
    }
}
```

In the `PushOperation::CreateEntity` match arm, bind and emit the scope:

```rust
PushOperation::CreateEntity {
    parent_id,
    parent_scope,
    title,
    properties,
    body,
    source_path,
    ..
} => Self::CreateEntity {
    parent_id: parent_id.0,
    parent_scope: create_parent_scope_name(&parent_scope).to_string(),
    title,
    keys: properties.keys().cloned().collect(),
    properties: properties
        .into_iter()
        .map(|(key, value)| PropertyUpdateOutput {
            key,
            value: PropertyValueOutput::from(value),
        })
        .collect(),
    body,
    source_path: locality_platform::logical_path_display(&source_path),
},
```

- [ ] **Step 4: Add push parent-scope helpers**

In `crates/localityd/src/push.rs`, import:

```rust
use locality_core::planner::CreateParentScope;
use locality_notion::projection::{is_notion_private_root_id, is_notion_workspace_root_id};
```

Add helpers near `create_entity_pipeline`:

```rust
fn create_parent_scope_for_entity(mount: &MountConfig, parent: &EntityRecord) -> CreateParentScope {
    if mount.connector == "notion" && is_notion_private_root_id(&parent.remote_id) {
        return CreateParentScope::PrivateWorkspace;
    }
    if mount.connector == "notion" && is_notion_workspace_root_id(&parent.remote_id) {
        return CreateParentScope::WorkspaceRoot;
    }
    CreateParentScope::Remote
}

fn notion_workspace_root_create_validation(relative_path: &Path) -> ValidationIssue {
    ValidationIssue::new(
        "notion_workspace_root_create_ambiguous",
        relative_path,
        None,
        "New root workspace pages are ambiguous because Notion does not expose a stable teamspace parent through this API.",
        Some("Create under Private/ for a private page, or create below an existing page that Locality can use as the parent.".to_string()),
    )
}
```

- [ ] **Step 5: Set scope and reject direct `Workspace/` creates**

In `create_entity_pipeline`, after create-frontmatter validation and before the `!validation.is_clean()` return, compute the scope:

```rust
let parent_scope = create_parent_scope_for_entity(mount, parent);
if parent_scope == CreateParentScope::WorkspaceRoot {
    validation.push(notion_workspace_root_create_validation(relative_path));
}
```

Use the scope in the plan:

```rust
let plan = PushPlan::new(
    vec![parent.remote_id.clone()],
    vec![PushOperation::CreateEntity {
        parent_id: parent.remote_id.clone(),
        parent_kind: Some(parent.kind.clone()),
        parent_scope,
        title: parsed.frontmatter.title.clone().unwrap_or_default(),
        properties,
        body: parsed.document.body.clone(),
        source_path: relative_path.to_path_buf(),
    }],
);
```

- [ ] **Step 6: Run push planning tests green**

Run:

```bash
cargo test -p loc-cli --test push push_create_under_notion_private_root_plans_private_workspace_parent_scope -- --nocapture
cargo test -p loc-cli --test push push_create_directly_under_notion_workspace_root_returns_ambiguity_validation -- --nocapture
cargo test -p loc-cli --test push push_create_below_existing_page_under_workspace_uses_real_page_parent_scope -- --nocapture
```

Expected: all pass.

- [ ] **Step 7: Commit Task 4**

```bash
git add crates/localityd/src/push.rs crates/loc-cli/src/diff.rs crates/loc-cli/tests/push.rs
git commit -m "feat: plan notion private root creates"
```

### Task 5: Notion Apply For Private Workspace Creates

**Files:**
- Modify: `crates/locality-notion/src/apply.rs`
- Modify: `crates/locality-notion/tests/apply.rs`

- [ ] **Step 1: Add failing apply tests**

In `crates/locality-notion/tests/apply.rs`, change the planner import to include `CreateParentScope`:

```rust
use locality_core::planner::{
    CreateParentScope, PropertyValue, PushOperation, PushOperationKind, PushPlan,
};
```

Import the private root ID:

```rust
use locality_notion::projection::NOTION_PRIVATE_ROOT_ID;
```

Add these tests before `apply_creates_child_page_and_marks_parent_changed`:

```rust
#[test]
fn apply_creates_private_workspace_page_without_parent_payload() {
    let api = Arc::new(RecordingNotionApi::new("2026-06-10T00:00:00.000Z", false));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new(NOTION_PRIVATE_ROOT_ID)],
        vec![PushOperation::CreateEntity {
            parent_id: RemoteId::new(NOTION_PRIVATE_ROOT_ID),
            parent_kind: Some(EntityKind::Directory),
            parent_scope: CreateParentScope::PrivateWorkspace,
            title: "Private Draft".to_string(),
            properties: BTreeMap::new(),
            body: "# Private body\n\nCreated from Locality.".to_string(),
            source_path: "Private/Private Draft/page.md".into(),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    let result = connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect("apply private workspace create");

    assert_eq!(result.changed_remote_ids, vec![RemoteId::new("created-page-1")]);
    assert_eq!(
        result.effects,
        vec![JournalApplyEffect::CreatedEntity {
            operation_id: operation_ids[0].clone(),
            operation_index: 0,
            parent_id: RemoteId::new(NOTION_PRIVATE_ROOT_ID),
            entity_id: RemoteId::new("created-page-1"),
        }]
    );
    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [WriteCall::CreatePage {
            body: json!({
                "properties": {
                    "title": {
                        "title": rich_text_json("Private Draft"),
                    },
                },
                "children": [{
                    "object": "block",
                    "type": "heading_1",
                    "heading_1": {
                        "rich_text": rich_text_json("Private body"),
                    },
                }, {
                    "object": "block",
                    "type": "paragraph",
                    "paragraph": {
                        "rich_text": rich_text_json("Created from Locality."),
                    },
                }],
            }),
        }]
    );
}

#[test]
fn check_concurrency_skips_private_workspace_synthetic_create_parent() {
    let api = Arc::new(RecordingNotionApi::new("2026-06-10T00:00:00.000Z", false));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new(NOTION_PRIVATE_ROOT_ID)],
        vec![PushOperation::CreateEntity {
            parent_id: RemoteId::new(NOTION_PRIVATE_ROOT_ID),
            parent_kind: Some(EntityKind::Directory),
            parent_scope: CreateParentScope::PrivateWorkspace,
            title: "Private Draft".to_string(),
            properties: BTreeMap::new(),
            body: String::new(),
            source_path: "Private/Private Draft/page.md".into(),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let mount_id = MountId::new("notion-main");
    let preconditions = vec![RemotePrecondition {
        remote_id: RemoteId::new(NOTION_PRIVATE_ROOT_ID),
        remote_edited_at: Some("synthetic".to_string()),
    }];

    connector
        .check_concurrency(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &[],
            remote_preconditions: &preconditions,
            local_root: None,
        })
        .expect("synthetic root precondition is skipped");
}
```

- [ ] **Step 2: Run apply tests and verify red**

Run:

```bash
cargo test -p locality-notion --test apply apply_creates_private_workspace_page_without_parent_payload -- --nocapture
cargo test -p locality-notion --test apply check_concurrency_skips_private_workspace_synthetic_create_parent -- --nocapture
```

Expected: fail because the apply layer retrieves a database or page for the synthetic root and includes the wrong parent shape.

- [ ] **Step 3: Import and use `CreateParentScope` in apply**

In `crates/locality-notion/src/apply.rs`, change:

```rust
use locality_core::planner::{PropertyValue, PushOperation};
```

to:

```rust
use locality_core::planner::{CreateParentScope, PropertyValue, PushOperation};
```

Update the `PushOperation::CreateEntity` match arm:

```rust
PushOperation::CreateEntity {
    parent_id,
    parent_kind,
    parent_scope,
    title,
    properties,
    body,
    ..
} => {
    let request_body = create_page_body(
        api,
        parent_id,
        parent_kind.as_ref(),
        parent_scope,
        title,
        properties,
        body,
    )?;
    let created = api.create_page(request_body)?;
    let created_id = RemoteId::new(created.id);
    if !changed_remote_ids.contains(&created_id) {
        changed_remote_ids.push(created_id.clone());
    }
    if *parent_scope == CreateParentScope::Remote
        && matches!(parent_kind, Some(locality_core::model::EntityKind::Page))
        && !changed_remote_ids.contains(parent_id)
    {
        changed_remote_ids.push(parent_id.clone());
    }
    effects.push(JournalApplyEffect::CreatedEntity {
        operation_id: request.operation_ids[operation_index].clone(),
        operation_index,
        parent_id: parent_id.clone(),
        entity_id: created_id,
    });
}
```

- [ ] **Step 4: Skip synthetic roots during concurrency and affected fetch**

Add this helper next to `create_parent_ids`:

```rust
fn source_root_create_parent_ids(operations: &[PushOperation]) -> BTreeSet<RemoteId> {
    operations
        .iter()
        .filter_map(|operation| match operation {
            PushOperation::CreateEntity {
                parent_id,
                parent_scope,
                ..
            } if *parent_scope != CreateParentScope::Remote => Some(parent_id.clone()),
            _ => None,
        })
        .collect()
}
```

At the start of `check_concurrency`, compute it and skip those preconditions:

```rust
let source_root_create_parent_ids = source_root_create_parent_ids(&request.plan.operations);
for precondition in request.remote_preconditions {
    if source_root_create_parent_ids.contains(&precondition.remote_id) {
        continue;
    }
    // existing logic
}
```

Change `database_create_parent_ids` to only include remote non-page parents:

```rust
fn database_create_parent_ids(operations: &[PushOperation]) -> BTreeSet<RemoteId> {
    operations
        .iter()
        .filter_map(|operation| match operation {
            PushOperation::CreateEntity {
                parent_id,
                parent_kind,
                parent_scope,
                ..
            } if *parent_scope == CreateParentScope::Remote
                && !matches!(parent_kind, Some(locality_core::model::EntityKind::Page)) =>
            {
                Some(parent_id.clone())
            }
            _ => None,
        })
        .collect()
}
```

Keep `create_parent_ids` returning every create parent ID so `fetch_affected_bundles` does not try to fetch source-root parents from Notion.

- [ ] **Step 5: Build create-page request bodies by parent scope**

Change the `create_page_body` signature:

```rust
fn create_page_body(
    api: &dyn NotionApi,
    parent_id: &RemoteId,
    parent_kind: Option<&locality_core::model::EntityKind>,
    parent_scope: &CreateParentScope,
    title: &str,
    properties: &BTreeMap<String, PropertyValue>,
    body: &str,
) -> LocalityResult<Value> {
```

Add this branch at the top:

```rust
if *parent_scope == CreateParentScope::PrivateWorkspace {
    let mut request = json!({
        "properties": {
            "title": {
                "title": rich_text(title),
            }
        },
    });
    let children = create_page_children(body)?;
    if !children.is_empty() {
        request["children"] = Value::Array(children);
    }
    return Ok(request);
}

if *parent_scope == CreateParentScope::WorkspaceRoot {
    return Err(LocalityError::Unsupported(
        "creating a Notion page directly under Workspace/ is ambiguous; create under Private/ or below an existing page",
    ));
}
```

Leave the existing page-parent and database/data-source-parent logic under the `Remote` case.

- [ ] **Step 6: Run apply tests green**

Run:

```bash
cargo test -p locality-notion --test apply apply_creates_private_workspace_page_without_parent_payload -- --nocapture
cargo test -p locality-notion --test apply check_concurrency_skips_private_workspace_synthetic_create_parent -- --nocapture
cargo test -p locality-notion --test apply apply_creates_child_page_and_marks_parent_changed -- --nocapture
cargo test -p locality-notion --test apply check_concurrency_uses_database_metadata_for_row_create_parent -- --nocapture
```

Expected: all pass.

- [ ] **Step 7: Commit Task 5**

```bash
git add crates/locality-notion/src/apply.rs crates/locality-notion/tests/apply.rs
git commit -m "feat: apply notion private workspace creates"
```

### Task 6: Docs And Agent Guidance

**Files:**
- Modify: `docs/notion-connector.md`
- Modify: `docs/cli.md`
- Modify: `docs/linux-fuse.md`
- Modify: `templates/mount/AGENTS.md`

- [ ] **Step 1: Update Notion connector docs**

Add this section to `docs/notion-connector.md` near the mount layout description:

````markdown
### Workspace Mount Root Layout

Workspace-level Notion mounts expose two synthetic root directories:

```text
Private/
Workspace/
```

`Private/` is a Locality-created directory for private workspace-level page
creation. Creating `Private/New Page/page.md` and pushing it creates a private
Notion page without a page or database parent.

`Workspace/` contains accessible Notion pages and databases whose Notion parent
is `workspace`, including team-level pages that the Notion public API currently
reports as workspace-parent pages. Creating a new page directly under
`Workspace/` is rejected because Locality cannot infer a stable teamspace
parent. Create below an existing page under `Workspace/` when the page should
become a child of that existing page.
````

- [ ] **Step 2: Update CLI docs**

Add this example to `docs/cli.md` in the `push` or Notion examples section:

````markdown
Create a private Notion page from a workspace mount:

```bash
mkdir -p notion-main/Private/Scratch
cat > notion-main/Private/Scratch/page.md <<'EOF'
---
title: Scratch
---
Initial private note.
EOF
loc push notion-main/Private/Scratch/page.md -y
```

Direct creates under `notion-main/Workspace/` are rejected. To create a child
page in shared workspace content, create it below an existing page directory,
for example `notion-main/Workspace/Roadmap/Follow-up/page.md`.
````

- [ ] **Step 3: Update Linux FUSE docs**

Add this note to `docs/linux-fuse.md` in the Notion mount section:

```markdown
For Notion workspace mounts, the FUSE root includes synthetic `Private/` and
`Workspace/` directories. They are folders only and do not contain `page.md`.
Private page creation goes under `Private/<title>/page.md`. Shared or
team-level pages that the Notion API reports as workspace-parent pages appear
under `Workspace/`.
```

- [ ] **Step 4: Update mounted agent guidance template**

Add this paragraph to `templates/mount/AGENTS.md`:

```markdown
For workspace-level Notion mounts, create private pages under `Private/<page
title>/page.md`. Do not create new pages directly under `Workspace/`; Locality
cannot infer the intended Notion teamspace there. To create a shared child page,
create it below an existing page directory, such as
`Workspace/Existing Page/New Child/page.md`.
```

- [ ] **Step 5: Run docs spelling and markdown-sensitive checks**

Run:

```bash
! rg "Teamspaces/" docs templates -g '!docs/superpowers/specs/**'
git diff --check
```

Expected: no `Teamspaces/` guidance outside the future-looking design spec; `git diff --check` exits 0.

- [ ] **Step 6: Commit Task 6**

```bash
git add docs/notion-connector.md docs/cli.md docs/linux-fuse.md templates/mount/AGENTS.md
git commit -m "docs: document notion workspace roots"
```

### Task 7: End-To-End Verification

**Files:**
- No new files expected.

- [ ] **Step 1: Run focused regression tests**

Run:

```bash
cargo test -p locality-core create_entity_parent_scope -- --nocapture
cargo test -p locality-notion projection::tests::workspace_enumeration_adds_synthetic_roots_and_places_workspace_objects_under_workspace -- --nocapture
cargo test -p locality-notion projection::tests::root_page_enumeration_does_not_add_workspace_synthetic_roots -- --nocapture
cargo test -p locality-notion projection::tests::workspace_root_children_list_synthetic_roots_and_workspace_source_root_lists_remote_children -- --nocapture
cargo test -p localityd notion_private_synthetic_root_accepts_pending_page_directory_create -- --nocapture
cargo test -p localityd notion_workspace_synthetic_root_rejects_pending_page_directory_create -- --nocapture
cargo test -p loc-cli --test push push_create_under_notion_private_root_plans_private_workspace_parent_scope -- --nocapture
cargo test -p loc-cli --test push push_create_directly_under_notion_workspace_root_returns_ambiguity_validation -- --nocapture
cargo test -p loc-cli --test push push_create_below_existing_page_under_workspace_uses_real_page_parent_scope -- --nocapture
cargo test -p locality-notion --test apply apply_creates_private_workspace_page_without_parent_payload -- --nocapture
cargo test -p locality-notion --test apply check_concurrency_skips_private_workspace_synthetic_create_parent -- --nocapture
cargo test -p loc-cli --test projection_contract virtual_projection_modes_show_notion_synthetic_roots_as_folders_without_page_documents -- --nocapture
```

Expected: all pass.

- [ ] **Step 2: Run broader local suites touched by the change**

Run:

```bash
cargo test -p locality-core
cargo test -p locality-connector
cargo test -p locality-notion
cargo test -p localityd
cargo test -p loc-cli --test push
cargo test -p loc-cli --test projection_contract
cargo test -p loc-cli --test mount
```

Expected: all pass. If unrelated ignored live tests are listed, they remain ignored.

- [ ] **Step 3: Run Linux FUSE smoke test on Linux hosts with FUSE available**

Run:

```bash
if [ "$(uname -s)" = "Linux" ] && [ -e /dev/fuse ]; then
  LOCALITY_FUSE_SMOKE=1 LOCALITY_FUSE_SMOKE_REQUIRED=1 tests/linux_fuse_smoke.sh
else
  echo "skip linux fuse smoke: this host is not Linux with /dev/fuse"
fi
```

Expected on Linux with FUSE: script exits 0. Expected on hosts without FUSE: explicit skip message above.

- [ ] **Step 4: Verify live private-create path when credentials are available**

Run this only when `NOTION_TOKEN` or a stored Notion credential is available:

```bash
cargo test -p locality-notion --test apply apply_creates_private_workspace_page_without_parent_payload -- --nocapture
```

This recording-API test is the default CI proof that the private create request omits `parent`. If a live workspace mount is available, manually create and push one scratch page under `Private/`, then archive it from Notion after verification:

```bash
scratch="Locality private scratch $(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$NOTION_WORKSPACE_MOUNT/Private/$scratch"
cat > "$NOTION_WORKSPACE_MOUNT/Private/$scratch/page.md" <<EOF
---
title: "$scratch"
---
Created to verify private workspace-root creation.
EOF
./target/debug/loc push "$NOTION_WORKSPACE_MOUNT/Private/$scratch/page.md" -y --json
```

Expected: push succeeds, the created page appears in Notion as a private workspace-level page, and no `parent.page_id` or `parent.data_source_id` is used in the create request. Teamspace create verification for this change is the default rejection test `push_create_directly_under_notion_workspace_root_returns_ambiguity_validation`, because this implementation intentionally does not create teamspace root pages without a stable Notion teamspace parent API.

- [ ] **Step 5: Run final repository checks**

Run:

```bash
git diff --check
git status --short
```

Expected: no whitespace errors. `git status --short` shows only intentional changes that have not yet been committed, or is clean after the final commit.

- [ ] **Step 6: Commit final verification fixes if any were required**

If verification required code or docs edits, commit them:

```bash
git status --short
git add crates/locality-core/src/planner.rs crates/locality-connector/src/lib.rs crates/locality-notion/src/projection.rs crates/locality-notion/src/apply.rs crates/locality-notion/tests/apply.rs crates/localityd/src/virtual_fs.rs crates/loc-cli/src/diff.rs crates/loc-cli/tests/push.rs crates/loc-cli/tests/projection_contract.rs docs/notion-connector.md docs/cli.md docs/linux-fuse.md templates/mount/AGENTS.md
git commit -m "test: cover notion workspace root hierarchy"
```

If no files changed during verification, do not create an empty commit.

## Self-Review

- Spec coverage: `Private/` and `Workspace/` projection is covered by Task 2. Private create planning and apply are covered by Tasks 4 and 5. Direct `Workspace/` create rejection is covered by Tasks 3 and 4. Existing child-page creates under pages inside `Workspace/` are covered by Task 4. Virtual filesystem folder-only behavior is covered by Task 3. Root-page mounts avoiding synthetic roots are covered by Task 2. Documentation is covered by Task 6.
- Placeholder scan: the plan uses concrete file paths, test names, commands, constants, validation code, and expected outcomes. It avoids open-ended implementation markers.
- Type consistency: `CreateParentScope` values are `Remote`, `PrivateWorkspace`, and `WorkspaceRoot`; CLI output names are `remote`, `private_workspace`, and `workspace_root`; Notion constants use `notion-root:private` and `notion-root:workspace` throughout.
