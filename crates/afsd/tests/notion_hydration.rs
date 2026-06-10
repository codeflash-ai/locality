use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use afs_core::hydration::{HydrationReason, HydrationRequest};
use afs_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use afs_notion::client::NotionApi;
use afs_notion::dto::{
    BlockDto, BlockListDto, PageDto, PageListDto, PagePropertyDto, PaginatedListDto,
    RichTextBlockDto, RichTextDto, TextRichTextDto,
};
use afs_notion::{NotionConfig, NotionConnector};
use afs_store::{
    EntityRecord, EntityRepository, InMemoryStateStore, MountConfig, MountRepository,
    ShadowRepository,
};
use afsd::hydration::{HydrationExecutor, HydrationOutcome};

const CODEFLASH_HOME_PAGE_ID: &str = "37b3ac0ebb8880d3a863fba3a5e41915";

#[test]
fn notion_connector_hydrates_stub_through_daemon_executor() {
    let fixture = NotionHydrationFixture::new();
    fixture.write_stub();
    let mut store = fixture.store();
    let connector = NotionConnector::with_api(
        NotionConfig::default(),
        Arc::new(FixtureNotionApi::page_with_blocks(
            "page-1",
            "Codeflash Home",
            vec![
                rich_text_block("heading-1", "heading_1", "Codeflash Home"),
                rich_text_block("paragraph-1", "paragraph", "Daemon hydration works."),
            ],
        )),
    );

    let mut executor = HydrationExecutor::new(&mut store, &connector);
    let outcome = executor
        .hydrate_request(fixture.request("page-1"))
        .expect("hydrate notion request");

    assert_eq!(outcome, HydrationOutcome::Hydrated);
    let contents = fs::read_to_string(fixture.page_path()).expect("hydrated page");
    assert!(contents.contains("# Codeflash Home"));
    assert!(contents.contains("Daemon hydration works."));

    let shadow = store
        .load_shadow(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("shadow");
    assert_eq!(shadow.entity_id, RemoteId::new("page-1"));
    assert_eq!(
        shadow
            .blocks
            .iter()
            .map(|block| block.remote_id.as_str())
            .collect::<Vec<_>>(),
        vec!["heading-1", "paragraph-1"]
    );
    let entity = store
        .get_entity(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Hydrated);
    assert_eq!(entity.content_hash, Some(shadow.body_hash));
}

#[test]
#[ignore = "requires NOTION_TOKEN and access to the target Notion page"]
fn live_notion_hydration_source_fetches_codeflash_home_page() {
    let page_id =
        std::env::var("AFS_NOTION_PAGE_ID").unwrap_or_else(|_| CODEFLASH_HOME_PAGE_ID.to_string());
    let request = HydrationRequest::new(
        MountId::new("notion-live"),
        RemoteId::new(page_id),
        "live.md",
        HydrationState::Hydrated,
        HydrationReason::ExplicitPull,
    );
    let connector = NotionConnector::new(NotionConfig::default());

    let rendered = afsd::hydration::HydrationSource::fetch_render(&connector, &request)
        .expect("live Notion fetch/render");

    assert_eq!(rendered.shadow.entity_id, request.remote_id);
    assert!(!rendered.document.frontmatter.is_empty());
}

#[derive(Clone, Debug)]
struct NotionHydrationFixture {
    root: PathBuf,
    mount_id: MountId,
}

impl NotionHydrationFixture {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "afs-notion-hydration-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("fixture root");

        Self {
            root,
            mount_id: MountId::new("notion-main"),
        }
    }

    fn store(&self) -> InMemoryStateStore {
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(MountConfig::new(
                self.mount_id.clone(),
                "notion",
                self.root.clone(),
            ))
            .expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    self.mount_id.clone(),
                    RemoteId::new("page-1"),
                    EntityKind::Page,
                    "Codeflash Home",
                    "Codeflash Home.md",
                )
                .with_hydration(HydrationState::Stub),
            )
            .expect("save entity");
        store
    }

    fn request(&self, remote_id: &str) -> HydrationRequest {
        HydrationRequest::new(
            self.mount_id.clone(),
            RemoteId::new(remote_id),
            self.page_path(),
            HydrationState::Hydrated,
            HydrationReason::StubRead,
        )
    }

    fn page_path(&self) -> PathBuf {
        self.root.join("Codeflash Home.md")
    }

    fn write_stub(&self) {
        fs::write(
            self.page_path(),
            format!(
                "---\nafs:\n  id: page-1\n  type: page\ntitle: Codeflash Home\n---\n{}\n",
                afs_core::model::CanonicalDocument::STUB_MARKER
            ),
        )
        .expect("write stub");
    }
}

impl Drop for NotionHydrationFixture {
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
    fn page_with_blocks(page_id: &str, title: &str, blocks: Vec<BlockDto>) -> Self {
        Self {
            pages: BTreeMap::from([(page_id.to_string(), page(page_id, title))]),
            children: BTreeMap::from([(
                (page_id.to_string(), None),
                PaginatedListDto {
                    results: blocks,
                    next_cursor: None,
                    has_more: false,
                },
            )]),
        }
    }
}

impl NotionApi for FixtureNotionApi {
    fn retrieve_page(&self, page_id: &str) -> afs_core::AfsResult<PageDto> {
        self.pages.get(page_id).cloned().ok_or_else(|| {
            afs_core::AfsError::InvalidState(format!("missing fixture page {page_id}"))
        })
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

fn rich_text_block(id: &str, kind: &str, text: &str) -> BlockDto {
    let mut block = BlockDto {
        id: id.to_string(),
        kind: kind.to_string(),
        ..BlockDto::default()
    };
    let content = Some(RichTextBlockDto {
        rich_text: vec![rich_text(text)],
        color: None,
    });

    match kind {
        "paragraph" => block.paragraph = content,
        "heading_1" => block.heading_1 = content,
        "heading_2" => block.heading_2 = content,
        "heading_3" => block.heading_3 = content,
        _ => panic!("unsupported fixture rich text kind: {kind}"),
    }

    block
}

fn rich_text(text: &str) -> RichTextDto {
    RichTextDto {
        kind: "text".to_string(),
        text: Some(TextRichTextDto {
            content: text.to_string(),
            link: None,
        }),
        plain_text: text.to_string(),
        ..RichTextDto::default()
    }
}
