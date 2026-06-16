use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use afs_connector::{Connector, ObserveRequest};
use afs_core::freshness::RemoteVersion;
use afs_core::model::{EntityKind, MountId, RemoteId};
use afs_core::{AfsError, AfsResult};
use afs_notion::client::NotionApi;
use afs_notion::dto::{
    BlockDto, BlockListDto, DatabaseDto, DatabaseListDto, PageDto, PageListDto, PagePropertyDto,
    PaginatedListDto, ParentDto, RichTextDto, TextRichTextDto,
};
use afs_notion::{NotionConfig, NotionConnector};

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

#[derive(Debug, Default)]
struct ObserveFixtureApi {
    pages: BTreeMap<String, PageDto>,
    databases: BTreeMap<String, DatabaseDto>,
    block_children_calls: AtomicUsize,
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
    fn retrieve_page(&self, page_id: &str) -> AfsResult<PageDto> {
        self.pages
            .get(page_id)
            .cloned()
            .ok_or_else(|| AfsError::InvalidState(format!("missing fixture page {page_id}")))
    }

    fn retrieve_database(&self, database_id: &str) -> AfsResult<DatabaseDto> {
        self.databases.get(database_id).cloned().ok_or_else(|| {
            AfsError::InvalidState(format!("missing fixture database {database_id}"))
        })
    }

    fn retrieve_block_children(
        &self,
        _block_id: &str,
        _start_cursor: Option<&str>,
    ) -> AfsResult<BlockListDto> {
        self.block_children_calls.fetch_add(1, Ordering::SeqCst);
        Ok(PaginatedListDto::default())
    }

    fn search_pages(&self, _start_cursor: Option<&str>) -> AfsResult<PageListDto> {
        Ok(PaginatedListDto::default())
    }

    fn search_databases(&self, _start_cursor: Option<&str>) -> AfsResult<DatabaseListDto> {
        Ok(PaginatedListDto::default())
    }

    fn update_block(&self, _block_id: &str, _body: serde_json::Value) -> AfsResult<BlockDto> {
        Err(AfsError::NotImplemented("fixture update block"))
    }

    fn append_block_children(
        &self,
        _block_id: &str,
        _body: serde_json::Value,
    ) -> AfsResult<BlockListDto> {
        Err(AfsError::NotImplemented("fixture append block children"))
    }

    fn delete_block(&self, _block_id: &str) -> AfsResult<BlockDto> {
        Err(AfsError::NotImplemented("fixture delete block"))
    }
}

fn page(id: &str, title: &str, parent: Option<ParentDto>) -> PageDto {
    PageDto {
        id: id.to_string(),
        parent,
        created_time: Some("2026-06-10T00:00:00.000Z".to_string()),
        last_edited_time: Some("2026-06-10T00:00:00.000Z".to_string()),
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
