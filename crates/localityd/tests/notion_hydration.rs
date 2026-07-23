use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use locality_core::hydration::{HydrationReason, HydrationRequest};
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use locality_notion::client::NotionApi;
use locality_notion::dto::{
    BlockDto, BlockListDto, ExternalFileDto, FileBlockDto, HostedFileDto, PageDto, PageListDto,
    PagePropertyDto, PaginatedListDto, RichTextBlockDto, RichTextDto, TextRichTextDto,
};
use locality_notion::media::{
    HostedMediaCaptureOutcome, PortableMediaCapture, PortableMediaCaptureFetcher,
    PortableMediaCapturePolicy,
};
use locality_notion::{NotionConfig, NotionConnector};
use locality_store::{
    EntityRecord, EntityRepository, InMemoryStateStore, MountConfig, MountRepository,
    ShadowRepository,
};
use localityd::hydration::{HydrationExecutor, HydrationOutcome};

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
fn notion_hydration_publishes_mixed_hosted_omission_and_external_reference() {
    let hosted_ok = "https://secure.notion-static.com/ok.png?X-Amz-Signature=ok-secret";
    let hosted_missing =
        "https://secure.notion-static.com/missing.png?X-Amz-Signature=missing-secret";
    let external = "https://cdn.example.com/public.png?version=exact#image";
    let connector = NotionConnector::with_api(
        NotionConfig::default(),
        Arc::new(FixtureNotionApi::page_with_blocks(
            "page-1",
            "Mixed Media",
            vec![
                hosted_media_block("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", hosted_ok, "Available"),
                hosted_media_block(
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    hosted_missing,
                    "Missing",
                ),
                external_media_block("cccccccccccccccccccccccccccccccc", external, "Public"),
            ],
        )),
    )
    .with_portable_media_capture_fetcher(
        PortableMediaCapturePolicy::HostedPilot,
        Arc::new(FixtureMediaFetcher(BTreeMap::from([
            (
                hosted_ok.to_string(),
                HostedMediaCaptureOutcome::Captured(PortableMediaCapture {
                    bytes: b"png".to_vec(),
                    media_type: "image/png".to_string(),
                }),
            ),
            (
                hosted_missing.to_string(),
                HostedMediaCaptureOutcome::Unavailable,
            ),
        ]))),
    );
    let request = HydrationRequest::new(
        MountId::new("notion-main"),
        RemoteId::new("page-1"),
        "Docs/Coverage/page.md",
        HydrationState::Stub,
        HydrationReason::StubRead,
    );

    let hydrated = localityd::hydration::HydrationSource::fetch_render(&connector, &request)
        .expect("mixed media hydration");
    assert_eq!(hydrated.assets.len(), 1);
    assert_eq!(hydrated.assets[0].bytes, b"png");
    assert_eq!(
        hydrated.assets[0].path,
        PathBuf::from(".loc/media/Docs/Coverage/image-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.png")
    );
    let markdown = locality_core::canonical::render_canonical_markdown(&hydrated.document);
    assert!(markdown.contains(
        "![Available](../../.loc/media/Docs/Coverage/image-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.png)"
    ));
    assert!(
        markdown
            .contains("::loc{id=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb type=image title=\"Missing\"}")
    );
    assert!(markdown.contains(&format!("![Public]({external})")));
    assert!(!markdown.contains(hosted_ok));
    assert!(!markdown.contains(hosted_missing));
    assert!(!markdown.contains("missing-secret"));
}

#[test]
fn notion_hydration_fails_closed_for_unsafe_hosted_media() {
    let hosted = "https://secure.notion-static.com/unsafe.png?X-Amz-Signature=secret";
    let connector = NotionConnector::with_api(
        NotionConfig::default(),
        Arc::new(FixtureNotionApi::page_with_blocks(
            "page-1",
            "Unsafe Media",
            vec![hosted_media_block("unsafe", hosted, "Unsafe")],
        )),
    )
    .with_portable_media_capture_fetcher(
        PortableMediaCapturePolicy::HostedPilot,
        Arc::new(FixtureMediaFetcher(BTreeMap::from([(
            hosted.to_string(),
            HostedMediaCaptureOutcome::Unsafe,
        )]))),
    );
    let request = HydrationRequest::new(
        MountId::new("notion-main"),
        RemoteId::new("page-1"),
        "Unsafe/page.md",
        HydrationState::Stub,
        HydrationReason::StubRead,
    );

    let error = localityd::hydration::HydrationSource::fetch_render(&connector, &request)
        .expect_err("unsafe hosted media must fail closed");
    assert_eq!(
        error.to_string(),
        "invalid state: Notion hosted media failed safety validation"
    );
    assert!(!format!("{error:?}").contains("X-Amz"));
    assert!(!format!("{error:?}").contains("secret"));
}

#[test]
fn notion_hydration_rejects_invalid_hosted_origin_before_injected_fetcher() {
    struct NeverCalledFetcher;
    impl PortableMediaCaptureFetcher for NeverCalledFetcher {
        fn fetch(
            &self,
            _hosted_url: &str,
            _max_bytes: usize,
        ) -> locality_core::LocalityResult<PortableMediaCapture> {
            panic!("invalid hosted origin reached injected fetcher")
        }
    }

    let connector = NotionConnector::with_api(
        NotionConfig::default(),
        Arc::new(FixtureNotionApi::page_with_blocks(
            "page-1",
            "Invalid Hosted Origin",
            vec![hosted_media_block(
                "unsafe-origin",
                "https://example.com/masquerading-as-hosted.png",
                "Unsafe",
            )],
        )),
    )
    .with_portable_media_capture_fetcher(
        PortableMediaCapturePolicy::HostedPilot,
        Arc::new(NeverCalledFetcher),
    );
    let request = HydrationRequest::new(
        MountId::new("notion-main"),
        RemoteId::new("page-1"),
        "Unsafe/page.md",
        HydrationState::Stub,
        HydrationReason::StubRead,
    );

    let error = localityd::hydration::HydrationSource::fetch_render(&connector, &request)
        .expect_err("invalid hosted origin must fail closed");
    assert_eq!(
        error.to_string(),
        "invalid state: Notion hosted media failed safety validation"
    );
}

#[test]
fn notion_hydration_fails_closed_for_malformed_hosted_expiry() {
    let connector = NotionConnector::with_api(
        NotionConfig::default(),
        Arc::new(FixtureNotionApi::page_with_blocks(
            "page-1",
            "Malformed Expiry",
            vec![hosted_media_block_with_expiry(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "https://secure.notion-static.com/image.png?X-Amz-Signature=secret",
                "Image",
                Some("2099-01-01T00:00:00+01:00"),
            )],
        )),
    );
    let request = HydrationRequest::new(
        MountId::new("notion-main"),
        RemoteId::new("page-1"),
        "Unsafe/page.md",
        HydrationState::Stub,
        HydrationReason::StubRead,
    );

    let error = localityd::hydration::HydrationSource::fetch_render(&connector, &request)
        .expect_err("malformed expiry must fail closed");
    assert_eq!(
        error.to_string(),
        "invalid state: Notion hosted media expiry failed safety validation"
    );
    assert!(!format!("{error:?}").contains("X-Amz"));
    assert!(!format!("{error:?}").contains("secret"));
}

#[test]
fn notion_hydration_omits_already_expired_hosted_media_without_fetching() {
    struct NeverCalledFetcher;
    impl PortableMediaCaptureFetcher for NeverCalledFetcher {
        fn fetch(
            &self,
            _hosted_url: &str,
            _max_bytes: usize,
        ) -> locality_core::LocalityResult<PortableMediaCapture> {
            panic!("expired hosted media reached fetcher")
        }
    }

    let connector = NotionConnector::with_api(
        NotionConfig::default(),
        Arc::new(FixtureNotionApi::page_with_blocks(
            "page-1",
            "Expired Media",
            vec![hosted_media_block_with_expiry(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "https://secure.notion-static.com/image.png?X-Amz-Signature=secret",
                "Expired",
                Some("2000-01-01T00:00:00.000Z"),
            )],
        )),
    )
    .with_portable_media_capture_fetcher(
        PortableMediaCapturePolicy::HostedPilot,
        Arc::new(NeverCalledFetcher),
    );
    let request = HydrationRequest::new(
        MountId::new("notion-main"),
        RemoteId::new("page-1"),
        "Expired/page.md",
        HydrationState::Stub,
        HydrationReason::StubRead,
    );

    let hydrated = localityd::hydration::HydrationSource::fetch_render(&connector, &request)
        .expect("expired hosted media is an omission");
    assert!(hydrated.assets.is_empty());
    let markdown = locality_core::canonical::render_canonical_markdown(&hydrated.document);
    assert!(
        markdown
            .contains("::loc{id=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa type=image title=\"Expired\"}")
    );
    assert!(!markdown.contains("https://secure.notion-static.com"));
    assert!(!markdown.contains("X-Amz"));
}

#[test]
#[ignore = "requires NOTION_TOKEN and access to the target Notion page"]
fn live_notion_hydration_source_fetches_codeflash_home_page() {
    let page_id = std::env::var("LOCALITY_NOTION_PAGE_ID").expect("LOCALITY_NOTION_PAGE_ID");
    let request = HydrationRequest::new(
        MountId::new("notion-live"),
        RemoteId::new(page_id),
        "live.md",
        HydrationState::Hydrated,
        HydrationReason::ExplicitPull,
    );
    let connector = NotionConnector::new(NotionConfig::default());

    let rendered = localityd::hydration::HydrationSource::fetch_render(&connector, &request)
        .expect("live Notion fetch/render");

    assert_notion_ids_match(&rendered.shadow.entity_id, &request.remote_id);
    assert!(!rendered.document.frontmatter.is_empty());
}

fn assert_notion_ids_match(actual: &RemoteId, expected: &RemoteId) {
    assert_eq!(
        actual.as_str().replace('-', ""),
        expected.as_str().replace('-', "")
    );
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
            "locality-notion-hydration-{}-{unique}",
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
                "---\nloc:\n  id: page-1\n  type: page\ntitle: Codeflash Home\n---\n{}\n",
                locality_core::model::CanonicalDocument::STUB_MARKER
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
    fn retrieve_page(&self, page_id: &str) -> locality_core::LocalityResult<PageDto> {
        self.pages.get(page_id).cloned().ok_or_else(|| {
            locality_core::LocalityError::InvalidState(format!("missing fixture page {page_id}"))
        })
    }

    fn retrieve_block_children(
        &self,
        block_id: &str,
        start_cursor: Option<&str>,
    ) -> locality_core::LocalityResult<BlockListDto> {
        Ok(self
            .children
            .get(&(block_id.to_string(), start_cursor.map(str::to_string)))
            .cloned()
            .unwrap_or_default())
    }

    fn search_pages(
        &self,
        _start_cursor: Option<&str>,
    ) -> locality_core::LocalityResult<PageListDto> {
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
    ) -> locality_core::LocalityResult<BlockDto> {
        Err(locality_core::LocalityError::NotImplemented(
            "fixture update block",
        ))
    }

    fn append_block_children(
        &self,
        _block_id: &str,
        _body: serde_json::Value,
    ) -> locality_core::LocalityResult<BlockListDto> {
        Err(locality_core::LocalityError::NotImplemented(
            "fixture append block children",
        ))
    }

    fn delete_block(&self, _block_id: &str) -> locality_core::LocalityResult<BlockDto> {
        Err(locality_core::LocalityError::NotImplemented(
            "fixture delete block",
        ))
    }
}

fn page(id: &str, title: &str) -> PageDto {
    PageDto {
        id: id.to_string(),
        parent: None,
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
                ..Default::default()
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

fn hosted_media_block(id: &str, url: &str, caption: &str) -> BlockDto {
    hosted_media_block_with_expiry(id, url, caption, Some("2099-01-01T00:00:00.000Z"))
}

fn hosted_media_block_with_expiry(
    id: &str,
    url: &str,
    caption: &str,
    expiry_time: Option<&str>,
) -> BlockDto {
    BlockDto {
        id: id.to_string(),
        kind: "image".to_string(),
        image: Some(FileBlockDto {
            kind: "file".to_string(),
            external: None,
            file: Some(HostedFileDto {
                url: url.to_string(),
                expiry_time: expiry_time.map(str::to_string),
            }),
            caption: vec![rich_text(caption)],
        }),
        ..BlockDto::default()
    }
}

fn external_media_block(id: &str, url: &str, caption: &str) -> BlockDto {
    BlockDto {
        id: id.to_string(),
        kind: "image".to_string(),
        image: Some(FileBlockDto {
            kind: "external".to_string(),
            external: Some(ExternalFileDto {
                url: url.to_string(),
            }),
            file: None,
            caption: vec![rich_text(caption)],
        }),
        ..BlockDto::default()
    }
}

struct FixtureMediaFetcher(BTreeMap<String, HostedMediaCaptureOutcome>);

impl PortableMediaCaptureFetcher for FixtureMediaFetcher {
    fn fetch(
        &self,
        hosted_url: &str,
        _max_bytes: usize,
    ) -> locality_core::LocalityResult<PortableMediaCapture> {
        match self.fetch_outcome(hosted_url, usize::MAX) {
            HostedMediaCaptureOutcome::Captured(capture) => Ok(capture),
            _ => Err(locality_core::LocalityError::Io(
                "fixture hosted media unavailable".to_string(),
            )),
        }
    }

    fn fetch_outcome(&self, hosted_url: &str, _max_bytes: usize) -> HostedMediaCaptureOutcome {
        self.0
            .get(hosted_url)
            .cloned()
            .unwrap_or(HostedMediaCaptureOutcome::Unavailable)
    }
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
