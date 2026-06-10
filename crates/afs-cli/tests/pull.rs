use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use afs_cli::mount::{MountOptions, run_mount};
use afs_cli::pull::run_pull;
use afs_core::model::{HydrationState, MountId, RemoteId};
use afs_notion::client::NotionApi;
use afs_notion::dto::{
    BlockDto, BlockListDto, PageDto, PageListDto, PagePropertyDto, PaginatedListDto,
    RichTextBlockDto, RichTextDto, TitleBlockDto,
};
use afs_notion::{NotionConfig, NotionConnector};
use afs_store::{EntityRepository, InMemoryStateStore, MountRepository, ShadowRepository};

#[test]
fn pull_mount_root_enumerates_stubs_and_hydrates_root_page() {
    let fixture = PullFixture::new();
    let mut store = InMemoryStateStore::new();
    fixture.mount(&mut store);
    let connector = fixture.connector();

    let report = run_pull(&mut store, &connector, &fixture.root).expect("pull root");

    assert!(report.ok);
    assert_eq!(report.enumerated, 2);
    assert_eq!(report.stubbed, 2);
    assert_eq!(report.hydrated, 1);
    assert!(fixture.root_file().exists());
    assert!(fixture.child_file().exists());
    assert!(
        !fs::read_to_string(fixture.root_file())
            .expect("root file")
            .contains(afs_core::model::CanonicalDocument::STUB_MARKER)
    );
    assert!(
        fs::read_to_string(fixture.child_file())
            .expect("child file")
            .contains(afs_core::model::CanonicalDocument::STUB_MARKER)
    );

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
fn pull_file_skips_dirty_hydrated_file() {
    let fixture = PullFixture::new();
    let mut store = InMemoryStateStore::new();
    fixture.mount(&mut store);
    let connector = fixture.connector();
    run_pull(&mut store, &connector, &fixture.root).expect("initial pull");
    fs::write(fixture.root_file(), "---\nafs:\n  id: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n---\nLocal edit.\n")
        .expect("dirty write");

    let report = run_pull(&mut store, &connector, fixture.root_file()).expect("pull dirty file");

    assert!(!report.ok);
    assert_eq!(report.hydrated, 0);
    assert_eq!(report.skipped_dirty, 1);
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
        run_mount(
            store,
            MountOptions {
                mount_id: self.mount_id.clone(),
                connector: "notion".to_string(),
                root: self.root.clone(),
                remote_root_id: Some(self.root_page_id.clone()),
                read_only: false,
            },
        )
        .expect("mount");
        assert_eq!(store.load_mounts().expect("mounts").len(), 1);
    }

    fn connector(&self) -> NotionConnector {
        NotionConnector::with_api(
            NotionConfig::default(),
            Arc::new(FixtureNotionApi::new(
                self.root_page_id.as_str(),
                self.canonical_root_page_id.as_str(),
            )),
        )
    }

    fn root_file(&self) -> PathBuf {
        self.root.join("roadmap ~aaaaaa.md")
    }

    fn child_file(&self) -> PathBuf {
        self.root
            .join("roadmap ~aaaaaa")
            .join("design-notes ~bbbbbb.md")
    }
}

impl Drop for PullFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Debug)]
struct FixtureNotionApi {
    pages: BTreeMap<String, PageDto>,
    children: BTreeMap<(String, Option<String>), BlockListDto>,
}

impl FixtureNotionApi {
    fn new(requested_root_page_id: &str, returned_root_page_id: &str) -> Self {
        let child_page_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let pages = BTreeMap::from([
            (
                requested_root_page_id.to_string(),
                page(returned_root_page_id, "Roadmap"),
            ),
            (
                returned_root_page_id.to_string(),
                page(returned_root_page_id, "Roadmap"),
            ),
            (
                child_page_id.to_string(),
                page(child_page_id, "Design Notes"),
            ),
        ]);
        let children = BTreeMap::from([
            (
                (returned_root_page_id.to_string(), None),
                PaginatedListDto {
                    results: vec![
                        paragraph_block("paragraph-1", "Root body."),
                        child_page_block(child_page_id, "Design Notes"),
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
        ]);

        Self { pages, children }
    }
}

impl NotionApi for FixtureNotionApi {
    fn retrieve_page(&self, page_id: &str) -> afs_core::AfsResult<PageDto> {
        self.pages
            .get(page_id)
            .cloned()
            .ok_or_else(|| afs_core::AfsError::InvalidState(format!("missing page {page_id}")))
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
}

fn page(id: &str, title: &str) -> PageDto {
    PageDto {
        id: id.to_string(),
        created_time: Some("2026-06-10T00:00:00.000Z".to_string()),
        last_edited_time: Some("2026-06-10T00:00:00.000Z".to_string()),
        archived: false,
        in_trash: false,
        properties: BTreeMap::from([(
            "title".to_string(),
            PagePropertyDto {
                kind: "title".to_string(),
                title: vec![rich_text(title)],
                rich_text: Vec::new(),
            },
        )]),
    }
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

fn block(id: &str, kind: &str) -> BlockDto {
    BlockDto {
        id: id.to_string(),
        kind: kind.to_string(),
        has_children: false,
        archived: false,
        in_trash: false,
        paragraph: None,
        heading_1: None,
        heading_2: None,
        heading_3: None,
        bulleted_list_item: None,
        numbered_list_item: None,
        to_do: None,
        quote: None,
        callout: None,
        code: None,
        child_page: None,
        child_database: None,
    }
}

fn rich_text(text: &str) -> RichTextDto {
    RichTextDto {
        plain_text: text.to_string(),
        href: None,
        annotations: Default::default(),
    }
}
