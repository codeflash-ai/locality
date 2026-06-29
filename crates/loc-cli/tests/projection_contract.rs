//! Shared virtual projection contract tests.
//!
//! These tests intentionally run below the OS kernel adapters. macOS File
//! Provider, Linux FUSE, and Windows Cloud Files must all preserve these daemon
//! semantics when their platform callbacks enumerate, hydrate, write, create,
//! rename, and delete.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, ChildContainer,
    Connector, ConnectorCapabilities, ConnectorKind, EnumerateRequest, FetchRequest,
    ListChildrenRequest, ListChildrenResult, NativeEntity, ParsedEntity,
};
use locality_core::canonical::render_canonical_markdown;
use locality_core::hydration::HydrationRequest;
use locality_core::model::{
    CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, TreeEntry,
};
use locality_core::planner::PushOperationKind;
use locality_core::shadow::ShadowDocument;
use locality_core::{LocalityError, LocalityResult};
use locality_store::{
    EntityRecord, EntityRepository, MountConfig, MountRepository, ProjectionMode, SqliteStateStore,
    VirtualMutationKind, VirtualMutationRepository,
};
use localityd::hydration::{HydratedEntity, HydrationSource};
use localityd::virtual_fs::{
    ROOT_CONTAINER_IDENTIFIER, commit_virtual_fs_write, create_virtual_fs_directory,
    create_virtual_fs_file, materialize_virtual_fs_item_with_content_root,
    refresh_virtual_fs_children, rename_virtual_fs_item, source_root_identifier,
    trash_virtual_fs_item, virtual_fs_children_with_content_root, virtual_fs_content_path,
    virtual_fs_content_root,
};

#[test]
fn virtual_projection_modes_share_browse_hydrate_write_contract() {
    for projection in virtual_projection_modes() {
        let fixture = ProjectionFixture::new(projection.clone());
        let mut store = fixture.store();
        fixture.seed_home_page(&mut store);
        let source = ContractSource::default();
        let content_root = fixture.content_root();

        let root = virtual_fs_children_with_content_root(
            &store,
            &content_root,
            &fixture.mount_id,
            ROOT_CONTAINER_IDENTIFIER,
        )
        .expect("browse virtual root");
        assert_child_folder(&root.children, "notion");

        let source_root = virtual_fs_children_with_content_root(
            &store,
            &content_root,
            &fixture.mount_id,
            &source_root_identifier("notion"),
        )
        .expect("browse connector root");
        assert_child_folder(&source_root.children, "Home");
        assert!(
            !fixture.content_path("Home/page.md").exists(),
            "{projection:?} source-root enumeration must stay metadata-only"
        );

        let refreshed = refresh_virtual_fs_children(
            &mut store,
            &source,
            &fixture.mount_id,
            "children:page-home",
        )
        .expect("refresh page children");
        assert_eq!(refreshed, 1, "{projection:?}");

        let home_children = virtual_fs_children_with_content_root(
            &store,
            &content_root,
            &fixture.mount_id,
            "children:page-home",
        )
        .expect("browse home page children");
        assert_child_file(&home_children.children, "page.md");
        assert_child_folder(&home_children.children, "Launch Plan");
        assert!(
            !fixture.content_path("Home/Launch Plan/page.md").exists(),
            "{projection:?} child enumeration must not hydrate page bodies"
        );

        let materialized = materialize_virtual_fs_item_with_content_root(
            &mut store,
            &source,
            &content_root,
            &fixture.mount_id,
            "page-launch",
        )
        .expect("hydrate child page");
        assert_eq!(materialized.hydration, HydrationState::Hydrated);
        let child_path = fixture.content_path("Home/Launch Plan/page.md");
        let child = fs::read_to_string(&child_path).expect("read hydrated child");
        assert!(child.contains("Original launch plan."));

        commit_virtual_fs_write(
            &mut store,
            &content_root,
            &fixture.mount_id,
            "page-launch",
            render_page("page-launch", "Launch Plan", "Updated launch plan.").as_bytes(),
        )
        .expect("commit provider write");
        let entity = store
            .get_entity(&fixture.mount_id, &RemoteId::new("page-launch"))
            .expect("get entity")
            .expect("entity");
        assert_eq!(entity.hydration, HydrationState::Dirty, "{projection:?}");
    }
}

#[test]
fn virtual_projection_modes_share_create_rename_delete_contract() {
    for projection in virtual_projection_modes() {
        let fixture = ProjectionFixture::new(projection.clone());
        let mut store = fixture.store();
        fixture.seed_home_page(&mut store);
        let content_root = fixture.content_root();

        let created = create_virtual_fs_file(
            &mut store,
            &content_root,
            &fixture.mount_id,
            "children:page-home",
            "Draft.md",
        )
        .expect("create draft");
        commit_virtual_fs_write(
            &mut store,
            &content_root,
            &fixture.mount_id,
            &created.identifier,
            b"# Draft\n\nInitial draft.",
        )
        .expect("write draft");
        assert!(created.identifier.starts_with("local:"));
        assert!(fixture.content_path("Home/Draft.md").exists());

        let renamed = rename_virtual_fs_item(
            &mut store,
            &content_root,
            &fixture.mount_id,
            &created.identifier,
            "children:page-home",
            "Renamed.md",
        )
        .expect("rename draft");
        assert_eq!(renamed.identifier, created.identifier);
        assert!(fixture.content_path("Home/Renamed.md").exists());
        assert!(!fixture.content_path("Home/Draft.md").exists());

        let trashed = trash_virtual_fs_item(
            &mut store,
            &content_root,
            &fixture.mount_id,
            &renamed.identifier,
        )
        .expect("delete pending draft");
        assert_eq!(trashed.identifier, renamed.identifier);
        assert!(
            store
                .get_virtual_mutation(&fixture.mount_id, &renamed.identifier)
                .expect("get create mutation")
                .is_none(),
            "{projection:?} deleting a pending create should remove the overlay mutation"
        );

        let local_page = create_virtual_fs_directory(
            &mut store,
            &content_root,
            &fixture.mount_id,
            "children:page-home",
            "Local Child",
        )
        .expect("create local page directory");
        assert!(local_page.identifier.starts_with("children:local:"));
        let mutation = store
            .get_virtual_mutation(
                &fixture.mount_id,
                local_page
                    .identifier
                    .strip_prefix("children:")
                    .expect("local page id"),
            )
            .expect("get local page mutation")
            .expect("local page mutation");
        assert_eq!(mutation.mutation_kind, VirtualMutationKind::Create);

        let deleted = trash_virtual_fs_item(
            &mut store,
            &content_root,
            &fixture.mount_id,
            &local_page.identifier,
        )
        .expect("delete local page directory");
        assert_eq!(deleted.identifier, local_page.identifier);
    }
}

fn virtual_projection_modes() -> [ProjectionMode; 3] {
    [
        ProjectionMode::MacosFileProvider,
        ProjectionMode::LinuxFuse,
        ProjectionMode::WindowsCloudFiles,
    ]
}

struct ProjectionFixture {
    root: PathBuf,
    state_root: PathBuf,
    mount_id: MountId,
    projection: ProjectionMode,
}

impl ProjectionFixture {
    fn new(projection: ProjectionMode) -> Self {
        let root = temp_path("loc-projection-contract-root");
        let state_root = temp_path("loc-projection-contract-state");
        fs::create_dir_all(&root).expect("root");
        fs::create_dir_all(&state_root).expect("state root");
        Self {
            root,
            state_root,
            mount_id: MountId::new(format!("notion-{}", projection.as_str())),
            projection,
        }
    }

    fn store(&self) -> SqliteStateStore {
        SqliteStateStore::open(self.state_root.clone()).expect("open store")
    }

    fn content_root(&self) -> PathBuf {
        virtual_fs_content_root(&self.state_root, &self.mount_id)
    }

    fn content_path(&self, relative_path: &str) -> PathBuf {
        virtual_fs_content_path(&self.state_root, &self.mount_id, Path::new(relative_path))
            .expect("content path")
    }

    fn seed_home_page(&self, store: &mut SqliteStateStore) {
        store
            .save_mount(
                MountConfig::new(self.mount_id.clone(), "notion", self.root.clone())
                    .projection(self.projection.clone()),
            )
            .expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    self.mount_id.clone(),
                    RemoteId::new("page-home"),
                    EntityKind::Page,
                    "Home",
                    "Home/page.md",
                )
                .with_hydration(HydrationState::Stub),
            )
            .expect("save entity");
    }
}

impl Drop for ProjectionFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
        let _ = fs::remove_dir_all(&self.state_root);
    }
}

#[derive(Default)]
struct ContractSource {
    bodies: RefCell<std::collections::BTreeMap<RemoteId, String>>,
}

impl HydrationSource for ContractSource {
    fn fetch_render(&self, request: &HydrationRequest) -> LocalityResult<HydratedEntity> {
        let body = self
            .bodies
            .borrow()
            .get(&request.remote_id)
            .cloned()
            .unwrap_or_else(|| "Original launch plan.".to_string());
        Ok(HydratedEntity {
            document: CanonicalDocument::new(
                frontmatter(request.remote_id.as_str(), "Launch Plan"),
                body.clone(),
            ),
            shadow: shadow(&request.remote_id, "Launch Plan", &body),
            remote_edited_at: Some("2026-06-20T00:00:00Z".to_string()),
            assets: Vec::new(),
        })
    }
}

impl Connector for ContractSource {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind("contract")
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supports_lazy_child_enumeration: true,
            supports_block_updates: true,
            ..ConnectorCapabilities::default()
        }
    }

    fn supported_push_operations(&self) -> BTreeSet<PushOperationKind> {
        PushOperationKind::all().into_iter().collect()
    }

    fn enumerate(&self, _request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        Err(LocalityError::NotImplemented("contract enumerate"))
    }

    fn list_children(&self, request: ListChildrenRequest) -> LocalityResult<ListChildrenResult> {
        match request.container {
            ChildContainer::PageChildren(remote_id) if remote_id == RemoteId::new("page-home") => {
                Ok(ListChildrenResult {
                    entries: vec![TreeEntry {
                        mount_id: request.mount_id,
                        remote_id: RemoteId::new("page-launch"),
                        kind: EntityKind::Page,
                        title: "Launch Plan".to_string(),
                        path: request.parent_path.join("Launch Plan/page.md"),
                        hydration: HydrationState::Stub,
                        content_hash: None,
                        remote_edited_at: Some("2026-06-20T00:00:00Z".to_string()),
                        stub_frontmatter: Some(frontmatter("page-launch", "Launch Plan")),
                    }],
                })
            }
            _ => Ok(ListChildrenResult::default()),
        }
    }

    fn fetch(&self, _request: FetchRequest) -> LocalityResult<NativeEntity> {
        Err(LocalityError::NotImplemented("contract fetch"))
    }

    fn render(&self, _entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        Err(LocalityError::NotImplemented("contract render"))
    }

    fn parse(&self, _document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
        Err(LocalityError::NotImplemented("contract parse"))
    }

    fn check_concurrency(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
        Ok(())
    }

    fn apply(&self, request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        Ok(ApplyPlanResult {
            changed_remote_ids: request.plan.affected_entities.clone(),
            effects: Vec::new(),
        })
    }

    fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
        Err(LocalityError::NotImplemented("contract undo"))
    }
}

fn assert_child_folder(children: &[localityd::virtual_fs::VirtualFsItem], filename: &str) {
    let child = children
        .iter()
        .find(|child| child.filename == filename)
        .unwrap_or_else(|| panic!("missing folder `{filename}`"));
    assert_eq!(child.kind, localityd::virtual_fs::VirtualFsItemKind::Folder);
}

fn assert_child_file(children: &[localityd::virtual_fs::VirtualFsItem], filename: &str) {
    let child = children
        .iter()
        .find(|child| child.filename == filename)
        .unwrap_or_else(|| panic!("missing file `{filename}`"));
    assert_eq!(child.kind, localityd::virtual_fs::VirtualFsItemKind::File);
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
    ShadowDocument::from_synced_body(remote_id.clone(), body, 1, vec![RemoteId::new("block-1")])
        .expect("shadow")
        .with_frontmatter(frontmatter(remote_id.as_str(), title))
}

fn temp_path(prefix: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{prefix}-{}-{unique}-{suffix}", std::process::id()))
}
