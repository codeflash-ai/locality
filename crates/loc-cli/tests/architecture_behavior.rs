//! Architecture-level behavior tests.
//!
//! These tests avoid live provider APIs and assert the user-visible invariants
//! Locality should keep across connectors: browse metadata lazily, materialize on
//! open, surface local edits for review, push through the daemon pipeline, and
//! return the edited file to clean after reconciliation.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use loc_cli::diff::run_diff_with_state_root;
use loc_cli::status::{StatusOptions, StatusState, run_status};
use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, ChildContainer,
    Connector, ConnectorCapabilities, ConnectorKind, EnumerateRequest, FetchRequest,
    ListChildrenRequest, ListChildrenResult, NativeEntity, ParsedEntity,
};
use locality_core::canonical::render_canonical_markdown;
use locality_core::journal::JournalStatus;
use locality_core::model::{
    CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, TreeEntry,
};
use locality_core::planner::PushOperationKind;
use locality_core::shadow::ShadowDocument;
use locality_core::{LocalityError, LocalityResult};
use locality_store::{
    EntityRecord, EntityRepository, JournalRepository, MountConfig, MountRepository,
    ProjectionMode, SqliteStateStore,
};
use localityd::execution::PushJob;
use localityd::hydration::{HydratedEntity, HydrationSource};
use localityd::push::{PushJobAction, execute_push_job_with_content_root};
use localityd::virtual_fs::{
    ROOT_CONTAINER_IDENTIFIER, commit_virtual_fs_write,
    materialize_virtual_fs_item_with_content_root, mount_point_directory_name,
    mount_point_identifier, refresh_virtual_fs_children, virtual_fs_children_refresh_needed,
    virtual_fs_children_with_content_root, virtual_fs_content_path, virtual_fs_content_root,
};

#[test]
fn local_virtual_mount_supports_browse_open_edit_review_push_round_trip() {
    let fixture = BehaviorFixture::new();
    let mut store = fixture.store();
    fixture.seed_workspace(&mut store);
    let source = FakeSource::new();
    let content_root = virtual_fs_content_root(&fixture.state_root, &fixture.mount_id);

    let root_children = virtual_fs_children_with_content_root(
        &store,
        &content_root,
        &fixture.mount_id,
        ROOT_CONTAINER_IDENTIFIER,
    )
    .expect("browse mount root");
    let mount = fixture.mount_config();
    let mount_point_root = mount_point_identifier(&mount);
    assert_folder(&root_children.children, &mount_point_directory_name(&mount));
    assert!(
        root_children
            .children
            .iter()
            .all(|child| child.filename != "AGENTS.md" && child.filename != "CLAUDE.md"),
        "agent guidance belongs under the mount point root, not the shared Locality root"
    );

    let mount_point_children = virtual_fs_children_with_content_root(
        &store,
        &content_root,
        &fixture.mount_id,
        &mount_point_root,
    )
    .expect("browse notion mount point root");
    assert_readonly_guidance(
        &mount_point_children.children,
        "AGENTS.md",
        &mount_point_root,
    );
    assert_readonly_guidance(
        &mount_point_children.children,
        "CLAUDE.md",
        &mount_point_root,
    );
    assert_folder(&mount_point_children.children, "Teamspace Home");
    assert!(
        !mount_point_children
            .children
            .iter()
            .any(|child| child.filename == "Teamspace Home.md"),
        "page directories should not be paired with sibling Markdown files"
    );
    assert!(
        !fixture.content_path("Teamspace Home/page.md").exists(),
        "browsing the mount point root must not hydrate page bodies"
    );

    let parent_container = "children:page-1";
    assert!(
        virtual_fs_children_refresh_needed(&store, &fixture.mount_id, parent_container)
            .expect("check child refresh"),
        "opening a page directory with no known children should ask the source for metadata"
    );
    assert_eq!(
        refresh_virtual_fs_children(&mut store, &source, &fixture.mount_id, parent_container)
            .expect("refresh child metadata"),
        1
    );
    source.assert_listed_once(ChildContainer::PageChildren(RemoteId::new("page-1")));

    let page_children = virtual_fs_children_with_content_root(
        &store,
        &content_root,
        &fixture.mount_id,
        parent_container,
    )
    .expect("browse page directory");
    assert_child(&page_children.children, "page.md", EntityKind::Page);
    assert_folder(&page_children.children, "Launch Plan");
    assert!(
        !fixture
            .content_path("Teamspace Home/Launch Plan/page.md")
            .exists(),
        "directory navigation should still be metadata-only until the file is opened"
    );

    let materialized = materialize_virtual_fs_item_with_content_root(
        &mut store,
        &source,
        &content_root,
        &fixture.mount_id,
        "child-1",
    )
    .expect("open child page");
    assert_eq!(materialized.hydration, HydrationState::Hydrated);
    let local_file = fixture.content_path("Teamspace Home/Launch Plan/page.md");
    let original = fs::read_to_string(&local_file).expect("read materialized file");
    assert!(original.contains("Original launch plan."));

    let target_path = fixture.root.join("Teamspace Home/Launch Plan/page.md");
    let clean = status_for(&store, &fixture, &target_path);
    assert!(clean.clean);
    assert_eq!(
        entry_state(&clean, "Teamspace Home/Launch Plan/page.md"),
        StatusState::Clean
    );

    let edited_body = "## Launch\n\nUpdated launch plan.\n";
    commit_virtual_fs_write(
        &mut store,
        &content_root,
        &fixture.mount_id,
        "child-1",
        render_page("child-1", "Launch Plan", edited_body).as_bytes(),
    )
    .expect("local file provider write");
    source.set_body_after_apply("child-1", edited_body);

    let pending = status_for(&store, &fixture, &target_path);
    assert!(!pending.clean);
    assert_eq!(
        entry_state(&pending, "Teamspace Home/Launch Plan/page.md"),
        StatusState::Dirty
    );
    assert!(
        entry_issue_codes(&pending, "Teamspace Home/Launch Plan/page.md")
            .iter()
            .any(|code| code == "local_body_changed"),
        "local edits must show up as pending review before push"
    );

    let pushed = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: target_path.clone(),
            assume_yes: true,
            confirm_dangerous: false,
        },
        &source,
        Some(&fixture.state_root),
    )
    .expect("push through daemon pipeline");

    assert_eq!(pushed.action, PushJobAction::Reconciled);
    assert_eq!(source.apply_count(), 1);
    assert_eq!(source.remote_body("child-1"), edited_body);
    assert_eq!(
        store
            .list_journal()
            .expect("list journals")
            .pop()
            .expect("journal")
            .status,
        JournalStatus::Reconciled
    );

    let after_push = status_for(&store, &fixture, &target_path);
    assert!(after_push.clean);
    assert_eq!(
        entry_state(&after_push, "Teamspace Home/Launch Plan/page.md"),
        StatusState::Clean
    );
    let reconciled = fs::read_to_string(local_file).expect("read reconciled file");
    assert!(reconciled.contains("Updated launch plan."));
    assert!(!reconciled.contains("Original launch plan."));
}

#[test]
fn virtual_database_row_open_caches_schema_before_local_title_diff() {
    let fixture = BehaviorFixture::new();
    let mut store = fixture.store();
    fixture.seed_workspace(&mut store);
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("database-1"),
            EntityKind::Database,
            "Tasks",
            "Teamspace Home/Tasks",
        ))
        .expect("save database");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("row-1"),
            EntityKind::Page,
            "Fix login bug",
            "Teamspace Home/Tasks/Fix Login Bug/page.md",
        ))
        .expect("save database row");

    let source = FakeSource::new();
    source.insert_remote_body("row-1", "Original row body.\n");
    source.set_schema("database-1", tasks_schema());
    let content_root = virtual_fs_content_root(&fixture.state_root, &fixture.mount_id);

    let materialized = materialize_virtual_fs_item_with_content_root(
        &mut store,
        &source,
        &content_root,
        &fixture.mount_id,
        "row-1",
    )
    .expect("open database row");
    assert_eq!(materialized.hydration, HydrationState::Hydrated);
    assert_eq!(
        fs::read_to_string(fixture.content_path("Teamspace Home/Tasks/_schema.yaml"))
            .expect("schema cache"),
        tasks_schema()
    );

    let local_file = fixture.content_path("Teamspace Home/Tasks/Fix Login Bug/page.md");
    let edited = fs::read_to_string(&local_file)
        .expect("read row")
        .replace("title: Launch Plan", "title: Fix login bug renamed");
    commit_virtual_fs_write(
        &mut store,
        &content_root,
        &fixture.mount_id,
        "row-1",
        edited.as_bytes(),
    )
    .expect("local row title edit");

    let diff = run_diff_with_state_root(
        &store,
        fixture
            .root
            .join("Teamspace Home/Tasks/Fix Login Bug/page.md"),
        Some(&fixture.state_root),
    )
    .expect("diff edited row");
    assert!(
        diff.validation
            .iter()
            .all(|issue| issue.code != "notion_schema_missing"),
        "{diff:#?}"
    );
    assert!(diff.ok, "{diff:#?}");
}

#[test]
fn unknown_local_path_reports_no_matching_mount() {
    let fixture = BehaviorFixture::new();
    let store = SqliteStateStore::open(fixture.state_root.clone()).expect("open sqlite store");
    let missing_path = fixture.root.join("No Mount Here.md");

    let error = run_status(
        &store,
        StatusOptions {
            path: Some(missing_path),
            state_root: Some(fixture.state_root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect_err("unknown path should not silently pick a mount");

    assert_eq!(error.code(), "mount_not_found");
}

struct BehaviorFixture {
    root: PathBuf,
    state_root: PathBuf,
    mount_id: MountId,
}

impl BehaviorFixture {
    fn new() -> Self {
        let root = unique_temp_path("loc-architecture-behavior-root");
        let state_root = unique_temp_path("loc-architecture-behavior-state");
        fs::create_dir_all(&root).expect("fixture root");
        fs::create_dir_all(&state_root).expect("fixture state root");
        Self {
            root,
            state_root,
            mount_id: MountId::new("notion-main"),
        }
    }

    fn store(&self) -> SqliteStateStore {
        SqliteStateStore::open(self.state_root.clone()).expect("open sqlite store")
    }

    fn mount_config(&self) -> MountConfig {
        MountConfig::new(self.mount_id.clone(), "notion", self.root.clone())
            .projection(ProjectionMode::LinuxFuse)
    }

    fn seed_workspace(&self, store: &mut SqliteStateStore) {
        store.save_mount(self.mount_config()).expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    self.mount_id.clone(),
                    RemoteId::new("page-1"),
                    EntityKind::Page,
                    "Teamspace Home",
                    "Teamspace Home/page.md",
                )
                .with_hydration(HydrationState::Stub),
            )
            .expect("save top-level page");
    }

    fn content_path(&self, relative_path: &str) -> PathBuf {
        virtual_fs_content_path(&self.state_root, &self.mount_id, Path::new(relative_path))
            .expect("content path")
    }
}

impl Drop for BehaviorFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
        let _ = fs::remove_dir_all(&self.state_root);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ListRequestSnapshot {
    container: ChildContainer,
    parent_path: PathBuf,
}

struct FakeSource {
    remote_bodies: RefCell<BTreeMap<RemoteId, String>>,
    after_apply: RefCell<BTreeMap<RemoteId, String>>,
    schemas: RefCell<BTreeMap<RemoteId, String>>,
    list_requests: RefCell<Vec<ListRequestSnapshot>>,
    apply_count: Cell<usize>,
}

impl FakeSource {
    fn new() -> Self {
        Self {
            remote_bodies: RefCell::new(BTreeMap::from([(
                RemoteId::new("child-1"),
                "## Launch\n\nOriginal launch plan.\n".to_string(),
            )])),
            after_apply: RefCell::new(BTreeMap::new()),
            schemas: RefCell::new(BTreeMap::new()),
            list_requests: RefCell::new(Vec::new()),
            apply_count: Cell::new(0),
        }
    }

    fn insert_remote_body(&self, remote_id: &str, body: &str) {
        self.remote_bodies
            .borrow_mut()
            .insert(RemoteId::new(remote_id), body.to_string());
    }

    fn set_body_after_apply(&self, remote_id: &str, body: &str) {
        self.after_apply
            .borrow_mut()
            .insert(RemoteId::new(remote_id), body.to_string());
    }

    fn set_schema(&self, database_id: &str, schema: &str) {
        self.schemas
            .borrow_mut()
            .insert(RemoteId::new(database_id), schema.to_string());
    }

    fn remote_body(&self, remote_id: &str) -> String {
        self.remote_bodies
            .borrow()
            .get(&RemoteId::new(remote_id))
            .expect("remote body")
            .clone()
    }

    fn apply_count(&self) -> usize {
        self.apply_count.get()
    }

    fn assert_listed_once(&self, container: ChildContainer) {
        let requests = self.list_requests.borrow();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].container, container);
        assert_eq!(requests[0].parent_path, PathBuf::from("Teamspace Home"));
    }
}

impl HydrationSource for FakeSource {
    fn fetch_render(
        &self,
        request: &locality_core::hydration::HydrationRequest,
    ) -> LocalityResult<HydratedEntity> {
        let body = self
            .remote_bodies
            .borrow()
            .get(&request.remote_id)
            .cloned()
            .ok_or_else(|| LocalityError::InvalidState("missing fake remote body".to_string()))?;
        Ok(hydrated_entity(&request.remote_id, "Launch Plan", &body))
    }

    fn fetch_database_schema_yaml(&self, database_id: &RemoteId) -> LocalityResult<Option<String>> {
        Ok(self.schemas.borrow().get(database_id).cloned())
    }
}

impl Connector for FakeSource {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind("fake")
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supports_block_updates: true,
            supports_databases: true,
            supports_oauth: false,
            supports_remote_observation: true,
            supports_lazy_child_enumeration: true,
            ..ConnectorCapabilities::default()
        }
    }

    fn supported_push_operations(&self) -> BTreeSet<PushOperationKind> {
        PushOperationKind::all().into_iter().collect()
    }

    fn enumerate(&self, _request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        Err(LocalityError::NotImplemented("fake enumerate"))
    }

    fn list_children(&self, request: ListChildrenRequest) -> LocalityResult<ListChildrenResult> {
        self.list_requests.borrow_mut().push(ListRequestSnapshot {
            container: request.container.clone(),
            parent_path: request.parent_path.clone(),
        });
        match request.container {
            ChildContainer::PageChildren(remote_id) if remote_id == RemoteId::new("page-1") => {
                Ok(ListChildrenResult {
                    entries: vec![TreeEntry {
                        mount_id: request.mount_id,
                        remote_id: RemoteId::new("child-1"),
                        kind: EntityKind::Page,
                        title: "Launch Plan".to_string(),
                        path: request.parent_path.join("Launch Plan/page.md"),
                        hydration: HydrationState::Stub,
                        content_hash: None,
                        remote_edited_at: Some("2026-06-12T00:00:00Z".to_string()),
                        stub_frontmatter: Some(frontmatter("child-1", "Launch Plan")),
                    }],
                })
            }
            _ => Ok(ListChildrenResult::default()),
        }
    }

    fn fetch(&self, _request: FetchRequest) -> LocalityResult<NativeEntity> {
        Err(LocalityError::NotImplemented("fake fetch"))
    }

    fn render(&self, _entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        Err(LocalityError::NotImplemented("fake render"))
    }

    fn parse(&self, _document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
        Err(LocalityError::NotImplemented("fake parse"))
    }

    fn check_concurrency(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
        Ok(())
    }

    fn apply(&self, request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        self.apply_count.set(self.apply_count.get() + 1);
        for remote_id in &request.plan.affected_entities {
            if let Some(body) = self.after_apply.borrow().get(remote_id).cloned() {
                self.remote_bodies
                    .borrow_mut()
                    .insert(remote_id.clone(), body);
            }
        }
        Ok(ApplyPlanResult {
            changed_remote_ids: request.plan.affected_entities.clone(),
            effects: Vec::new(),
        })
    }

    fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
        Err(LocalityError::NotImplemented("fake undo"))
    }
}

fn status_for(
    store: &SqliteStateStore,
    fixture: &BehaviorFixture,
    path: &Path,
) -> loc_cli::status::StatusReport {
    run_status(
        store,
        StatusOptions {
            path: Some(path.to_path_buf()),
            state_root: Some(fixture.state_root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status report")
}

fn assert_child(
    children: &[localityd::virtual_fs::VirtualFsItem],
    filename: &str,
    kind: EntityKind,
) {
    let child = children
        .iter()
        .find(|child| child.filename == filename)
        .unwrap_or_else(|| panic!("missing child `{filename}`"));
    assert_eq!(child.kind, localityd::virtual_fs::VirtualFsItemKind::File);
    assert_eq!(child.entity_kind.as_ref(), Some(&kind));
}

fn assert_folder(children: &[localityd::virtual_fs::VirtualFsItem], filename: &str) {
    let child = children
        .iter()
        .find(|child| child.filename == filename)
        .unwrap_or_else(|| panic!("missing folder `{filename}`"));
    assert_eq!(child.kind, localityd::virtual_fs::VirtualFsItemKind::Folder);
}

fn assert_readonly_guidance(
    children: &[localityd::virtual_fs::VirtualFsItem],
    filename: &str,
    expected_parent_identifier: &str,
) {
    let child = children
        .iter()
        .find(|child| child.filename == filename)
        .unwrap_or_else(|| panic!("missing guidance `{filename}`"));
    assert_eq!(child.kind, localityd::virtual_fs::VirtualFsItemKind::File);
    assert_eq!(child.entity_kind, None);
    assert_eq!(
        child.parent_identifier.as_deref(),
        Some(expected_parent_identifier)
    );
}

fn entry_state(report: &loc_cli::status::StatusReport, path: &str) -> StatusState {
    report
        .mounts
        .iter()
        .flat_map(|mount| &mount.entries)
        .find(|entry| entry.path == path)
        .unwrap_or_else(|| panic!("missing status entry `{path}`"))
        .state
}

fn entry_issue_codes(report: &loc_cli::status::StatusReport, path: &str) -> Vec<String> {
    report
        .mounts
        .iter()
        .flat_map(|mount| &mount.entries)
        .find(|entry| entry.path == path)
        .unwrap_or_else(|| panic!("missing status entry `{path}`"))
        .issues
        .iter()
        .map(|issue| issue.code.clone())
        .collect()
}

fn hydrated_entity(remote_id: &RemoteId, title: &str, body: &str) -> HydratedEntity {
    HydratedEntity {
        document: CanonicalDocument::new(frontmatter(remote_id.as_str(), title), body.to_string()),
        shadow: shadow(remote_id, title, body),
        remote_edited_at: Some("2026-06-12T00:00:00Z".to_string()),
        assets: Vec::new(),
    }
}

fn render_page(remote_id: &str, title: &str, body: &str) -> String {
    render_canonical_markdown(&CanonicalDocument::new(frontmatter(remote_id, title), body))
}

fn frontmatter(remote_id: &str, title: &str) -> String {
    format!(
        "loc:\n  id: {remote_id}\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: {title}\n"
    )
}

fn shadow(remote_id: &RemoteId, title: &str, body: &str) -> ShadowDocument {
    let block_ids = (0..block_count(body))
        .map(|index| RemoteId::new(format!("{}-block-{index}", remote_id.as_str())))
        .collect::<Vec<_>>();
    ShadowDocument::from_synced_body(remote_id.clone(), body, 9, block_ids)
        .expect("shadow")
        .with_frontmatter(frontmatter(remote_id.as_str(), title))
}

fn block_count(body: &str) -> usize {
    body.split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .count()
}

fn tasks_schema() -> &'static str {
    r#"loc:
  type: notion_database_schema
  database_id: "database-1"
title: "Tasks"
data_sources:
  - id: "source-1"
    name: "Tasks"
    properties:
      Name:
        id: "name-id"
        type: "title"
"#
}

fn unique_temp_path(prefix: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{prefix}-{}-{unique}-{suffix}", std::process::id()))
}
