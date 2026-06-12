use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use afs_cli::mount::{MountOptions, run_mount};
use afs_cli::pull::{run_pull, run_pull_with_state_root};
use afs_core::conflict::{
    CONFLICT_LOCAL_MARKER, CONFLICT_REMOTE_MARKER, CONFLICT_SEPARATOR_MARKER,
    has_unresolved_conflict_markers,
};
use afs_core::model::{HydrationState, MountId, RemoteId};
use afs_notion::client::NotionApi;
use afs_notion::dto::{
    BlockDto, BlockListDto, DataSourceDto, DataSourcePropertyDto, DataSourceSummaryDto,
    DatabaseDto, PageDto, PageListDto, PagePropertyDto, PaginatedListDto, RichTextBlockDto,
    RichTextDto, SelectOptionDto, SelectPropertySchemaDto, TextRichTextDto, TitleBlockDto,
};
use afs_notion::{NotionConfig, NotionConnector};
use afs_store::{
    EntityRepository, InMemoryStateStore, MountRepository, ProjectionMode, ShadowRepository,
};
use afsd::virtual_fs::virtual_fs_content_root;

#[test]
fn pull_mount_root_enumerates_stubs_and_hydrates_root_page() {
    let fixture = PullFixture::new();
    let mut store = InMemoryStateStore::new();
    fixture.mount(&mut store);
    let connector = fixture.connector("Roadmap");

    let report = run_pull(&mut store, &connector, &fixture.root).expect("pull root");

    assert!(report.ok);
    assert_eq!(report.enumerated, 4);
    assert_eq!(report.stubbed, 3);
    assert_eq!(report.hydrated, 1);
    assert!(fixture.root_file("roadmap").exists());
    assert!(fixture.child_file("roadmap").exists());
    assert!(fixture.database_schema_file().exists());
    assert!(fixture.row_file().exists());
    assert!(
        !fs::read_to_string(fixture.root_file("roadmap"))
            .expect("root file")
            .contains(afs_core::model::CanonicalDocument::STUB_MARKER)
    );
    assert!(
        fs::read_to_string(fixture.child_file("roadmap"))
            .expect("child file")
            .contains(afs_core::model::CanonicalDocument::STUB_MARKER)
    );
    let schema = fs::read_to_string(fixture.database_schema_file()).expect("schema file");
    assert!(schema.contains("type: notion_database_schema"));
    assert!(schema.contains("\"Status\":"));
    let row = fs::read_to_string(fixture.row_file()).expect("row file");
    assert!(row.contains("\"Status\": \"Todo\""));
    assert!(row.contains(afs_core::model::CanonicalDocument::STUB_MARKER));

    assert!(
        store
            .get_entity(&fixture.mount_id, &fixture.root_page_id)
            .expect("compact root entity lookup")
            .is_none()
    );
    let root_entity = store
        .get_entity(&fixture.mount_id, &fixture.canonical_root_page_id)
        .expect("get root entity")
        .expect("root entity");
    assert_eq!(root_entity.hydration, HydrationState::Hydrated);
    assert!(
        store
            .load_shadow(&fixture.mount_id, &fixture.canonical_root_page_id)
            .is_ok()
    );
}

#[test]
fn pull_virtual_mount_writes_content_and_schema_to_daemon_cache() {
    let fixture = PullFixture::new();
    let state_root = unique_temp_path("afs-cli-pull-state");
    let mut store = InMemoryStateStore::new();
    fixture.mount_with_projection(&mut store, ProjectionMode::LinuxFuse);
    let connector = fixture.connector("Roadmap");

    let report = run_pull_with_state_root(&mut store, &connector, &fixture.root, Some(&state_root))
        .expect("pull virtual root");

    assert!(report.ok);
    assert_eq!(report.stubbed, 0);
    assert_eq!(report.hydrated, 1);
    assert!(!fixture.root_file("roadmap").exists());
    let content_root = virtual_fs_content_root(&state_root, &fixture.mount_id);
    assert!(content_root.join("roadmap ~aaaaaa.md").exists());
    assert!(
        content_root
            .join("roadmap ~aaaaaa")
            .join("tasks ~cccccc")
            .join("_schema.yaml")
            .exists()
    );

    let _ = fs::remove_dir_all(state_root);
}

#[test]
fn pull_virtual_file_target_does_not_stat_projection_path_as_directory() {
    let fixture = PullFixture::new();
    let state_root = unique_temp_path("afs-cli-pull-state");
    let mut store = InMemoryStateStore::new();
    fixture.mount_with_projection(&mut store, ProjectionMode::LinuxFuse);
    let connector = fixture.connector("Roadmap");
    run_pull_with_state_root(&mut store, &connector, &fixture.root, Some(&state_root))
        .expect("pull virtual root");

    fs::create_dir_all(fixture.root_file("roadmap")).expect("sentinel directory at VFS file path");

    let report = run_pull_with_state_root(
        &mut store,
        &connector,
        fixture.root_file("roadmap"),
        Some(&state_root),
    )
    .expect("pull virtual file target");

    assert!(report.ok);
    assert_eq!(report.enumerated, 0);
    assert_eq!(report.hydrated, 1);
    assert_eq!(report.stubbed, 0);

    let _ = fs::remove_dir_all(state_root);
    let _ = fs::remove_dir_all(&fixture.root);
}

#[test]
fn pull_file_skips_dirty_hydrated_file() {
    let fixture = PullFixture::new();
    let mut store = InMemoryStateStore::new();
    fixture.mount(&mut store);
    let connector = fixture.connector("Roadmap");
    run_pull(&mut store, &connector, &fixture.root).expect("initial pull");
    fs::write(fixture.root_file("roadmap"), "---\nafs:\n  id: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n---\nLocal edit.\n")
        .expect("dirty write");

    let report =
        run_pull(&mut store, &connector, fixture.root_file("roadmap")).expect("pull dirty file");

    assert!(!report.ok);
    assert_eq!(report.hydrated, 0);
    assert_eq!(report.skipped_dirty, 1);
}

#[test]
fn pull_mount_root_renames_existing_projection_when_remote_title_changes() {
    let fixture = PullFixture::new();
    let mut store = InMemoryStateStore::new();
    fixture.mount(&mut store);

    run_pull(&mut store, &fixture.connector("Roadmap"), &fixture.root).expect("initial pull");

    assert!(fixture.root_file("roadmap").exists());
    assert!(fixture.child_file("roadmap").exists());

    let report = run_pull(&mut store, &fixture.connector("Strategy"), &fixture.root)
        .expect("pull renamed root");

    assert!(report.ok);
    assert!(fixture.root_file("strategy").exists());
    assert!(fixture.child_file("strategy").exists());
    assert!(!fixture.root_file("roadmap").exists());
    assert!(!fixture.child_file("roadmap").exists());

    let root_entity = store
        .get_entity(&fixture.mount_id, &fixture.canonical_root_page_id)
        .expect("get root entity")
        .expect("root entity");
    assert_eq!(root_entity.path, PathBuf::from("strategy ~aaaaaa.md"));
}

#[test]
fn pull_mount_root_preserves_shadow_remote_timestamp_for_non_rehydrated_pages() {
    let fixture = PullFixture::new();
    let mut store = InMemoryStateStore::new();
    fixture.mount(&mut store);

    run_pull(&mut store, &fixture.connector("Roadmap"), &fixture.root).expect("initial pull");
    run_pull(
        &mut store,
        &fixture.connector("Roadmap"),
        fixture.child_file("roadmap"),
    )
    .expect("hydrate child");

    let child_entity = store
        .get_entity(
            &fixture.mount_id,
            &RemoteId::new("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        )
        .expect("get child entity")
        .expect("child entity");
    assert_eq!(
        child_entity.remote_edited_at.as_deref(),
        Some("2026-06-10T00:00:00.000Z")
    );

    run_pull(
        &mut store,
        &fixture.connector_with("Roadmap", "Root body.", "2026-06-11T00:00:00.000Z"),
        &fixture.root,
    )
    .expect("refresh root");

    let child_entity = store
        .get_entity(
            &fixture.mount_id,
            &RemoteId::new("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        )
        .expect("get child entity")
        .expect("child entity");
    assert_eq!(
        child_entity.remote_edited_at.as_deref(),
        Some("2026-06-10T00:00:00.000Z")
    );
}

#[test]
fn pull_file_writes_inline_conflict_markers_and_marks_conflicted_when_remote_changed() {
    let fixture = PullFixture::new();
    let mut store = InMemoryStateStore::new();
    fixture.mount(&mut store);
    run_pull(&mut store, &fixture.connector("Roadmap"), &fixture.root).expect("initial pull");
    fs::write(
        fixture.root_file("roadmap"),
        "---\nafs:\n  id: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n---\nLocal edit.\n",
    )
    .expect("dirty write");

    let report = run_pull(
        &mut store,
        &fixture.connector_with("Roadmap", "Remote body.", "2026-06-11T00:00:00.000Z"),
        fixture.root_file("roadmap"),
    )
    .expect("pull conflicted file");

    assert!(!report.ok);
    assert_eq!(report.hydrated, 0);
    assert_eq!(report.skipped_dirty, 1);
    let contents = fs::read_to_string(fixture.root_file("roadmap")).expect("local file");
    assert!(contents.contains("Local edit."));
    assert!(contents.contains("Remote body."));
    assert!(contents.contains(CONFLICT_LOCAL_MARKER));
    assert!(contents.contains(CONFLICT_SEPARATOR_MARKER));
    assert!(contents.contains(CONFLICT_REMOTE_MARKER));
    assert!(has_unresolved_conflict_markers(&contents));
    assert!(
        !fixture
            .root_file("roadmap")
            .with_extension("remote.md")
            .exists()
    );
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.canonical_root_page_id)
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Conflicted);
    assert_eq!(
        entity.remote_edited_at.as_deref(),
        Some("2026-06-11T00:00:00.000Z")
    );
    let shadow = store
        .load_shadow(&fixture.mount_id, &fixture.canonical_root_page_id)
        .expect("load shadow");
    assert!(shadow.rendered_body.contains("Remote body."));
}

#[test]
fn pull_file_leaves_inline_conflict_unchanged_when_remote_changes_again() {
    let fixture = PullFixture::new();
    let mut store = InMemoryStateStore::new();
    fixture.mount(&mut store);
    run_pull(&mut store, &fixture.connector("Roadmap"), &fixture.root).expect("initial pull");
    fs::write(
        fixture.root_file("roadmap"),
        "---\nafs:\n  id: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n---\nLocal edit.\n",
    )
    .expect("dirty write");

    let conflicted_connector =
        fixture.connector_with("Roadmap", "Remote body.", "2026-06-11T00:00:00.000Z");
    run_pull(
        &mut store,
        &conflicted_connector,
        fixture.root_file("roadmap"),
    )
    .expect("pull conflicted file");
    let conflicted_contents =
        fs::read_to_string(fixture.root_file("roadmap")).expect("conflict file");

    let report = run_pull(
        &mut store,
        &fixture.connector_with("Roadmap", "Remote body v2.", "2026-06-12T00:00:00.000Z"),
        fixture.root_file("roadmap"),
    )
    .expect("pull unresolved conflict");

    assert!(!report.ok);
    assert_eq!(report.hydrated, 0);
    assert_eq!(report.skipped_dirty, 1);
    assert_eq!(
        fs::read_to_string(fixture.root_file("roadmap")).expect("conflict file"),
        conflicted_contents
    );
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.canonical_root_page_id)
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Conflicted);
}

struct PullFixture {
    root: PathBuf,
    mount_id: MountId,
    root_page_id: RemoteId,
    canonical_root_page_id: RemoteId,
}

impl PullFixture {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "afs-cli-pull-{}-{unique}-{suffix}",
            std::process::id()
        ));

        Self {
            root,
            mount_id: MountId::new("notion-main"),
            root_page_id: RemoteId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            canonical_root_page_id: RemoteId::new("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
        }
    }

    fn mount(&self, store: &mut InMemoryStateStore) {
        self.mount_with_projection(store, ProjectionMode::PlainFiles);
    }

    fn mount_with_projection(&self, store: &mut InMemoryStateStore, projection: ProjectionMode) {
        run_mount(
            store,
            MountOptions {
                mount_id: self.mount_id.clone(),
                connector: "notion".to_string(),
                root: self.root.clone(),
                remote_root_id: Some(self.root_page_id.clone()),
                connection_id: None,
                read_only: false,
                projection,
            },
        )
        .expect("mount");
        assert_eq!(store.load_mounts().expect("mounts").len(), 1);
    }

    fn connector(&self, root_title: &str) -> NotionConnector {
        self.connector_with(root_title, "Root body.", "2026-06-10T00:00:00.000Z")
    }

    fn connector_with(
        &self,
        root_title: &str,
        root_body: &str,
        last_edited_time: &str,
    ) -> NotionConnector {
        NotionConnector::with_api(
            NotionConfig::default(),
            Arc::new(FixtureNotionApi::new(
                self.root_page_id.as_str(),
                self.canonical_root_page_id.as_str(),
                root_title,
                root_body,
                last_edited_time,
            )),
        )
    }

    fn root_file(&self, slug: &str) -> PathBuf {
        self.root.join(format!("{slug} ~aaaaaa.md"))
    }

    fn child_file(&self, root_slug: &str) -> PathBuf {
        self.root
            .join(format!("{root_slug} ~aaaaaa"))
            .join("design-notes ~bbbbbb.md")
    }

    fn database_schema_file(&self) -> PathBuf {
        self.root
            .join("roadmap ~aaaaaa")
            .join("tasks ~cccccc")
            .join("_schema.yaml")
    }

    fn row_file(&self) -> PathBuf {
        self.root
            .join("roadmap ~aaaaaa")
            .join("tasks ~cccccc")
            .join("fix-login-bug ~eeeeee.md")
    }
}

impl Drop for PullFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
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

#[derive(Debug)]
struct FixtureNotionApi {
    pages: BTreeMap<String, PageDto>,
    children: BTreeMap<(String, Option<String>), BlockListDto>,
    databases: BTreeMap<String, DatabaseDto>,
    data_sources: BTreeMap<String, DataSourceDto>,
    data_source_pages: BTreeMap<(String, Option<String>), PageListDto>,
}

impl FixtureNotionApi {
    fn new(
        requested_root_page_id: &str,
        returned_root_page_id: &str,
        root_title: &str,
        root_body: &str,
        last_edited_time: &str,
    ) -> Self {
        let child_page_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let database_id = "cccccccccccccccccccccccccccccccc";
        let data_source_id = "dddddddddddddddddddddddddddddddd";
        let row_page_id = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
        let pages = BTreeMap::from([
            (
                requested_root_page_id.to_string(),
                page(returned_root_page_id, root_title, last_edited_time),
            ),
            (
                returned_root_page_id.to_string(),
                page(returned_root_page_id, root_title, last_edited_time),
            ),
            (
                child_page_id.to_string(),
                page(child_page_id, "Design Notes", last_edited_time),
            ),
            (
                row_page_id.to_string(),
                database_row_page(row_page_id, "Fix login bug", last_edited_time),
            ),
        ]);
        let children = BTreeMap::from([
            (
                (returned_root_page_id.to_string(), None),
                PaginatedListDto {
                    results: vec![
                        paragraph_block("paragraph-1", root_body),
                        child_page_block(child_page_id, "Design Notes"),
                        child_database_block(database_id, "Tasks"),
                    ],
                    next_cursor: None,
                    has_more: false,
                },
            ),
            (
                (child_page_id.to_string(), None),
                PaginatedListDto {
                    results: vec![paragraph_block("paragraph-2", "Child body.")],
                    next_cursor: None,
                    has_more: false,
                },
            ),
            ((row_page_id.to_string(), None), PaginatedListDto::default()),
        ]);
        let databases = BTreeMap::from([(
            database_id.to_string(),
            DatabaseDto {
                id: database_id.to_string(),
                title: vec![rich_text("Tasks")],
                data_sources: vec![DataSourceSummaryDto {
                    id: data_source_id.to_string(),
                    name: Some("Tasks".to_string()),
                }],
                last_edited_time: Some(last_edited_time.to_string()),
                ..Default::default()
            },
        )]);
        let data_sources = BTreeMap::from([(
            data_source_id.to_string(),
            DataSourceDto {
                id: data_source_id.to_string(),
                name: Some("Tasks".to_string()),
                properties: BTreeMap::from([(
                    "Status".to_string(),
                    DataSourcePropertyDto {
                        id: "status-id".to_string(),
                        kind: "select".to_string(),
                        select: Some(SelectPropertySchemaDto {
                            options: vec![SelectOptionDto {
                                id: "todo-id".to_string(),
                                name: "Todo".to_string(),
                                color: None,
                            }],
                        }),
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            },
        )]);
        let data_source_pages = BTreeMap::from([(
            (data_source_id.to_string(), None),
            PaginatedListDto {
                results: vec![database_row_page(
                    row_page_id,
                    "Fix login bug",
                    last_edited_time,
                )],
                next_cursor: None,
                has_more: false,
            },
        )]);

        Self {
            pages,
            children,
            databases,
            data_sources,
            data_source_pages,
        }
    }
}

impl NotionApi for FixtureNotionApi {
    fn retrieve_page(&self, page_id: &str) -> afs_core::AfsResult<PageDto> {
        self.pages
            .get(page_id)
            .cloned()
            .ok_or_else(|| afs_core::AfsError::InvalidState(format!("missing page {page_id}")))
    }

    fn retrieve_database(&self, database_id: &str) -> afs_core::AfsResult<DatabaseDto> {
        self.databases.get(database_id).cloned().ok_or_else(|| {
            afs_core::AfsError::InvalidState(format!("missing database {database_id}"))
        })
    }

    fn retrieve_data_source(&self, data_source_id: &str) -> afs_core::AfsResult<DataSourceDto> {
        self.data_sources
            .get(data_source_id)
            .cloned()
            .ok_or_else(|| {
                afs_core::AfsError::InvalidState(format!("missing data source {data_source_id}"))
            })
    }

    fn query_data_source(
        &self,
        data_source_id: &str,
        start_cursor: Option<&str>,
    ) -> afs_core::AfsResult<PageListDto> {
        Ok(self
            .data_source_pages
            .get(&(data_source_id.to_string(), start_cursor.map(str::to_string)))
            .cloned()
            .unwrap_or_default())
    }

    fn retrieve_block_children(
        &self,
        block_id: &str,
        start_cursor: Option<&str>,
    ) -> afs_core::AfsResult<BlockListDto> {
        Ok(self
            .children
            .get(&(block_id.to_string(), start_cursor.map(str::to_string)))
            .cloned()
            .unwrap_or_default())
    }

    fn search_pages(&self, _start_cursor: Option<&str>) -> afs_core::AfsResult<PageListDto> {
        Ok(PaginatedListDto {
            results: self.pages.values().cloned().collect(),
            next_cursor: None,
            has_more: false,
        })
    }

    fn update_block(
        &self,
        _block_id: &str,
        _body: serde_json::Value,
    ) -> afs_core::AfsResult<BlockDto> {
        Err(afs_core::AfsError::NotImplemented("fixture update block"))
    }

    fn append_block_children(
        &self,
        _block_id: &str,
        _body: serde_json::Value,
    ) -> afs_core::AfsResult<BlockListDto> {
        Err(afs_core::AfsError::NotImplemented(
            "fixture append block children",
        ))
    }

    fn delete_block(&self, _block_id: &str) -> afs_core::AfsResult<BlockDto> {
        Err(afs_core::AfsError::NotImplemented("fixture delete block"))
    }
}

fn page(id: &str, title: &str, last_edited_time: &str) -> PageDto {
    PageDto {
        id: id.to_string(),
        parent: None,
        created_time: Some("2026-06-10T00:00:00.000Z".to_string()),
        last_edited_time: Some(last_edited_time.to_string()),
        archived: false,
        in_trash: false,
        properties: BTreeMap::from([(
            "title".to_string(),
            PagePropertyDto {
                kind: "title".to_string(),
                title: vec![rich_text(title)],
                rich_text: Vec::new(),
                ..Default::default()
            },
        )]),
    }
}

fn database_row_page(id: &str, title: &str, last_edited_time: &str) -> PageDto {
    let mut page = page(id, title, last_edited_time);
    page.properties.insert(
        "Status".to_string(),
        PagePropertyDto {
            kind: "select".to_string(),
            select: Some(SelectOptionDto {
                id: "todo-id".to_string(),
                name: "Todo".to_string(),
                color: None,
            }),
            ..Default::default()
        },
    );
    page
}

fn paragraph_block(id: &str, text: &str) -> BlockDto {
    let mut block = block(id, "paragraph");
    block.paragraph = Some(RichTextBlockDto {
        rich_text: vec![rich_text(text)],
        color: None,
    });
    block
}

fn child_page_block(id: &str, title: &str) -> BlockDto {
    let mut block = block(id, "child_page");
    block.child_page = Some(TitleBlockDto {
        title: title.to_string(),
    });
    block
}

fn child_database_block(id: &str, title: &str) -> BlockDto {
    let mut block = block(id, "child_database");
    block.child_database = Some(TitleBlockDto {
        title: title.to_string(),
    });
    block
}

fn block(id: &str, kind: &str) -> BlockDto {
    BlockDto {
        id: id.to_string(),
        kind: kind.to_string(),
        ..Default::default()
    }
}

fn rich_text(text: &str) -> RichTextDto {
    RichTextDto {
        kind: "text".to_string(),
        text: Some(TextRichTextDto {
            content: text.to_string(),
            link: None,
        }),
        mention: None,
        equation: None,
        plain_text: text.to_string(),
        href: None,
        annotations: Default::default(),
    }
}
