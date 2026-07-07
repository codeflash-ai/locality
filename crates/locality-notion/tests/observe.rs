use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use locality_connector::{Connector, ObserveRequest};
use locality_core::freshness::RemoteVersion;
use locality_core::model::{EntityKind, MountId, RemoteId};
use locality_core::{LocalityError, LocalityResult};
use locality_notion::client::{HttpNotionApi, NotionApi};
use locality_notion::dto::{
    BlockDto, BlockListDto, DatabaseDto, DatabaseListDto, PageDto, PageListDto, PagePropertyDto,
    PaginatedListDto, ParentDto, RichTextDto, TextRichTextDto,
};
use locality_notion::{NotionConfig, NotionConnector};
use serde_json::{Value, json};

mod support;

const LIVE_PARENT_ENV: &str = "LOCALITY_NOTION_LIVE_PARENT_PAGE";

#[test]
fn notion_observe_page_reads_metadata_without_hydrating_blocks() {
    let api = Arc::new(ObserveFixtureApi::with_page(page(
        "page-1",
        "Roadmap",
        Some(ParentDto {
            kind: "page_id".to_string(),
            page_id: Some("parent-page".to_string()),
            ..Default::default()
        }),
    )));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

    let observation = connector
        .observe(ObserveRequest {
            mount_id: MountId::new("notion-main"),
            remote_id: RemoteId::new("page-1"),
        })
        .expect("observe page");

    assert_eq!(observation.mount_id, MountId::new("notion-main"));
    assert_eq!(observation.remote_id, RemoteId::new("page-1"));
    assert_eq!(observation.kind, EntityKind::Page);
    assert_eq!(observation.title, "Roadmap");
    assert_eq!(
        observation.parent_remote_id,
        Some(RemoteId::new("parent-page"))
    );
    assert_eq!(
        observation.remote_version,
        Some(RemoteVersion::new("2026-06-10T00:00:00.000Z"))
    );
    assert!(!observation.deleted);
    assert_eq!(api.block_children_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn notion_observe_database_falls_back_to_database_metadata() {
    let api = Arc::new(ObserveFixtureApi::with_database(database(
        "database-1",
        "Tasks",
    )));
    let connector = NotionConnector::with_api(NotionConfig::default(), api);

    let observation = connector
        .observe(ObserveRequest {
            mount_id: MountId::new("notion-main"),
            remote_id: RemoteId::new("database-1"),
        })
        .expect("observe database");

    assert_eq!(observation.remote_id, RemoteId::new("database-1"));
    assert_eq!(observation.kind, EntityKind::Database);
    assert_eq!(observation.title, "Tasks");
    assert_eq!(
        observation.remote_version,
        Some(RemoteVersion::new("2026-06-10T00:00:00.000Z"))
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_notion_observe_page_reads_metadata_without_hydrating_blocks() {
    let parent_page_id =
        normalize_notion_id(&std::env::var(LIVE_PARENT_ENV).unwrap_or_else(|_| {
            panic!("set {LIVE_PARENT_ENV} to a writable Notion page ID or URL")
        }));
    let api = Arc::new(LiveObserveApi::new(support::live_notion_config()));
    let mut cleanup = LiveObserveCleanup::new(api.clone());
    let title = format!("Locality live observe {}", unique_suffix());
    let page = cleanup.create_page(&parent_page_id, &title);
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

    let observed = connector
        .observe(ObserveRequest {
            mount_id: MountId::new("live-notion"),
            remote_id: RemoteId::new(page.id.clone()),
        })
        .expect("observe live page");

    assert_eq!(observed.mount_id, MountId::new("live-notion"));
    assert_eq!(observed.remote_id, RemoteId::new(page.id.clone()));
    assert_eq!(observed.kind, EntityKind::Page);
    assert_eq!(observed.title, title);
    assert_eq!(
        observed.parent_remote_id,
        Some(RemoteId::new(parent_page_id.clone()))
    );
    assert!(observed.remote_version.is_some(), "{observed:#?}");
    assert!(!observed.deleted);
    assert_eq!(api.block_children_calls.load(Ordering::SeqCst), 0);

    std::thread::sleep(std::time::Duration::from_millis(1200));
    let updated_title = format!("{title} updated");
    api.update_page(
        &page.id,
        json!({
            "properties": {
                "title": {
                    "title": rich_text_json(&updated_title)
                }
            }
        }),
    )
    .expect("update live page metadata");
    let updated = connector
        .observe(ObserveRequest {
            mount_id: MountId::new("live-notion"),
            remote_id: RemoteId::new(page.id.clone()),
        })
        .expect("observe updated live page");

    assert_eq!(updated.title, updated_title);
    assert!(updated.remote_version.is_some(), "{updated:#?}");
    assert_ne!(updated.raw_metadata_json, observed.raw_metadata_json);
    assert_eq!(api.block_children_calls.load(Ordering::SeqCst), 0);
}

#[derive(Debug, Default)]
struct ObserveFixtureApi {
    pages: BTreeMap<String, PageDto>,
    databases: BTreeMap<String, DatabaseDto>,
    block_children_calls: AtomicUsize,
}

#[derive(Debug)]
struct LiveObserveApi {
    inner: HttpNotionApi,
    block_children_calls: AtomicUsize,
}

impl LiveObserveApi {
    fn new(config: NotionConfig) -> Self {
        Self {
            inner: HttpNotionApi::new(config),
            block_children_calls: AtomicUsize::new(0),
        }
    }
}

impl NotionApi for LiveObserveApi {
    fn retrieve_page(&self, page_id: &str) -> LocalityResult<PageDto> {
        self.inner.retrieve_page(page_id)
    }

    fn retrieve_database(&self, database_id: &str) -> LocalityResult<DatabaseDto> {
        self.inner.retrieve_database(database_id)
    }

    fn create_page(&self, body: Value) -> LocalityResult<PageDto> {
        self.inner.create_page(body)
    }

    fn update_page(&self, page_id: &str, body: Value) -> LocalityResult<PageDto> {
        self.inner.update_page(page_id, body)
    }

    fn retrieve_block_children(
        &self,
        block_id: &str,
        start_cursor: Option<&str>,
    ) -> LocalityResult<BlockListDto> {
        self.block_children_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.retrieve_block_children(block_id, start_cursor)
    }

    fn search_pages(&self, start_cursor: Option<&str>) -> LocalityResult<PageListDto> {
        self.inner.search_pages(start_cursor)
    }

    fn search_databases(&self, start_cursor: Option<&str>) -> LocalityResult<DatabaseListDto> {
        self.inner.search_databases(start_cursor)
    }

    fn update_block(&self, block_id: &str, body: Value) -> LocalityResult<BlockDto> {
        self.inner.update_block(block_id, body)
    }

    fn append_block_children(&self, block_id: &str, body: Value) -> LocalityResult<BlockListDto> {
        self.inner.append_block_children(block_id, body)
    }

    fn delete_block(&self, block_id: &str) -> LocalityResult<BlockDto> {
        self.inner.delete_block(block_id)
    }
}

struct LiveObserveCleanup {
    api: Arc<LiveObserveApi>,
    block_ids: Vec<String>,
}

impl LiveObserveCleanup {
    fn new(api: Arc<LiveObserveApi>) -> Self {
        Self {
            api,
            block_ids: Vec::new(),
        }
    }

    fn create_page(&mut self, parent_page_id: &str, title: &str) -> PageDto {
        let page = self
            .api
            .create_page(json!({
                "parent": {
                    "type": "page_id",
                    "page_id": parent_page_id
                },
                "properties": {
                    "title": {
                        "title": rich_text_json(title)
                    }
                },
                "children": [
                    {
                        "object": "block",
                        "type": "paragraph",
                        "paragraph": {
                            "rich_text": rich_text_json("Observation child block must not be hydrated.")
                        }
                    }
                ]
            }))
            .expect("create live observe page");
        self.block_ids.push(page.id.clone());
        page
    }
}

impl Drop for LiveObserveCleanup {
    fn drop(&mut self) {
        for block_id in self.block_ids.iter().rev() {
            let _ = self.api.delete_block(block_id);
        }
    }
}

impl ObserveFixtureApi {
    fn with_page(page: PageDto) -> Self {
        Self {
            pages: BTreeMap::from([(page.id.clone(), page)]),
            ..Default::default()
        }
    }

    fn with_database(database: DatabaseDto) -> Self {
        Self {
            databases: BTreeMap::from([(database.id.clone(), database)]),
            ..Default::default()
        }
    }
}

impl NotionApi for ObserveFixtureApi {
    fn retrieve_page(&self, page_id: &str) -> LocalityResult<PageDto> {
        self.pages
            .get(page_id)
            .cloned()
            .ok_or_else(|| LocalityError::InvalidState(format!("missing fixture page {page_id}")))
    }

    fn retrieve_database(&self, database_id: &str) -> LocalityResult<DatabaseDto> {
        self.databases.get(database_id).cloned().ok_or_else(|| {
            LocalityError::InvalidState(format!("missing fixture database {database_id}"))
        })
    }

    fn retrieve_block_children(
        &self,
        _block_id: &str,
        _start_cursor: Option<&str>,
    ) -> LocalityResult<BlockListDto> {
        self.block_children_calls.fetch_add(1, Ordering::SeqCst);
        Ok(PaginatedListDto::default())
    }

    fn search_pages(&self, _start_cursor: Option<&str>) -> LocalityResult<PageListDto> {
        Ok(PaginatedListDto::default())
    }

    fn search_databases(&self, _start_cursor: Option<&str>) -> LocalityResult<DatabaseListDto> {
        Ok(PaginatedListDto::default())
    }

    fn update_block(&self, _block_id: &str, _body: serde_json::Value) -> LocalityResult<BlockDto> {
        Err(LocalityError::NotImplemented("fixture update block"))
    }

    fn append_block_children(
        &self,
        _block_id: &str,
        _body: serde_json::Value,
    ) -> LocalityResult<BlockListDto> {
        Err(LocalityError::NotImplemented(
            "fixture append block children",
        ))
    }

    fn delete_block(&self, _block_id: &str) -> LocalityResult<BlockDto> {
        Err(LocalityError::NotImplemented("fixture delete block"))
    }
}

fn page(id: &str, title: &str, parent: Option<ParentDto>) -> PageDto {
    PageDto {
        id: id.to_string(),
        parent,
        created_time: Some("2026-06-10T00:00:00.000Z".to_string()),
        last_edited_time: Some("2026-06-10T00:00:00.000Z".to_string()),
        created_by: None,
        archived: false,
        in_trash: false,
        properties: BTreeMap::from([(
            "title".to_string(),
            PagePropertyDto {
                kind: "title".to_string(),
                title: vec![rich_text(title)],
                ..Default::default()
            },
        )]),
    }
}

fn database(id: &str, title: &str) -> DatabaseDto {
    DatabaseDto {
        id: id.to_string(),
        last_edited_time: Some("2026-06-10T00:00:00.000Z".to_string()),
        title: vec![rich_text(title)],
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
        plain_text: text.to_string(),
        ..Default::default()
    }
}

fn rich_text_json(text: &str) -> Vec<Value> {
    vec![json!({
        "type": "text",
        "text": {
            "content": text
        }
    })]
}

fn normalize_notion_id(value: &str) -> String {
    let tail = value
        .trim()
        .trim_end_matches('/')
        .rsplit(['/', '-'])
        .next()
        .unwrap_or(value)
        .trim();
    let compact = tail.replace('-', "");
    if compact.len() == 32 {
        format!(
            "{}-{}-{}-{}-{}",
            &compact[0..8],
            &compact[8..12],
            &compact[12..16],
            &compact[16..20],
            &compact[20..32]
        )
    } else {
        value.to_string()
    }
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    format!("{}-{nanos}", std::process::id())
}
