use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use afs_connector::{Connector, EnumerateRequest, FetchRequest};
use afs_core::model::{EntityKind, MountId, RemoteId};
use afs_core::shadow::MarkdownBlockKind;
use afs_notion::client::NotionApi;
use afs_notion::dto::{
    BlockDto, BlockListDto, BlockTreeDto, PageDto, PageListDto, PagePropertyDto, PaginatedListDto,
    RichTextBlockDto, RichTextDto, TitleBlockDto,
};
use afs_notion::{NotionConfig, NotionConnector};

#[test]
fn fetch_recurses_paginated_block_children_and_render_preserves_shadow_ids() {
    let api = FixtureNotionApi::new();
    let connector = NotionConnector::with_api(NotionConfig::default(), Arc::new(api));

    let native = connector
        .fetch(FetchRequest {
            remote_id: RemoteId::new("page-1"),
        })
        .expect("fetch");
    let bundle: afs_notion::dto::NotionPageBundle =
        serde_json::from_slice(&native.raw).expect("native bundle");

    assert_eq!(bundle.blocks.len(), 3);
    assert_eq!(bundle.blocks[1].children.len(), 1);

    let rendered = connector
        .render_native_entity(&native)
        .expect("render native entity");

    assert!(rendered.document.frontmatter.contains("id: page-1"));
    assert!(rendered.document.frontmatter.contains("title: \"Roadmap\""));
    assert_eq!(
        rendered.document.body,
        "# Roadmap\n\nPlan paragraph.\n\nNested detail.\n\n---\n"
    );
    assert_eq!(
        rendered
            .shadow
            .blocks
            .iter()
            .map(|block| block.remote_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "heading-1",
            "paragraph-1",
            "nested-paragraph-1",
            "divider-1"
        ]
    );
}

#[test]
fn render_unsupported_block_as_directive_without_consuming_native_shadow_id() {
    let page = page("page-1", "Roadmap");
    let block = BlockTreeDto {
        block: unsupported_block("bookmark-1", "bookmark"),
        children: Vec::new(),
    };
    let bundle = afs_notion::dto::NotionPageBundle {
        page,
        blocks: vec![block],
    };
    let raw = serde_json::to_vec(&bundle).expect("raw");
    let native = afs_connector::NativeEntity {
        remote_id: RemoteId::new("page-1"),
        kind: "notion_page".to_string(),
        raw,
    };
    let connector =
        NotionConnector::with_api(NotionConfig::default(), Arc::new(FixtureNotionApi::new()));

    let rendered = connector
        .render_native_entity(&native)
        .expect("render native entity");

    assert_eq!(
        rendered.document.body,
        "::afs{id=bookmark-1 type=unsupported_bookmark}\n"
    );
    assert_eq!(rendered.shadow.blocks.len(), 1);
    assert_eq!(
        rendered.shadow.blocks[0].remote_id,
        RemoteId::new("bookmark-1")
    );
    assert!(matches!(
        rendered.shadow.blocks[0].kind,
        MarkdownBlockKind::Directive { .. }
    ));
}

#[test]
fn enumerate_projects_root_page_tree_to_stable_paths() {
    let root_page_id = RemoteId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let api = FixtureNotionApi::tree(root_page_id.as_str());
    let connector = NotionConnector::with_api(
        NotionConfig::default().with_root_page_id(root_page_id),
        Arc::new(api),
    );

    let entries = connector
        .enumerate(EnumerateRequest {
            mount_id: MountId::new("notion-main"),
            cursor: None,
        })
        .expect("enumerate");

    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].path, Path::new("roadmap ~aaaaaa.md"));
    assert_eq!(entries[0].kind, EntityKind::Page);
    assert_eq!(
        entries[1].path,
        Path::new("roadmap ~aaaaaa/design-notes ~bbbbbb.md")
    );
    assert_eq!(entries[1].kind, EntityKind::Page);
    assert_eq!(entries[2].path, Path::new("roadmap ~aaaaaa/tasks ~cccccc"));
    assert_eq!(entries[2].kind, EntityKind::Database);
}

#[test]
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_PAGE_ID"]
fn live_fetch_and_render_page_from_environment() {
    let page_id = std::env::var("AFS_NOTION_PAGE_ID").expect("AFS_NOTION_PAGE_ID");
    let connector = NotionConnector::new(NotionConfig::default());

    let native = connector
        .fetch(FetchRequest {
            remote_id: RemoteId::new(page_id),
        })
        .expect("live fetch");
    let rendered = connector
        .render_native_entity(&native)
        .expect("live render");

    assert!(!rendered.document.frontmatter.is_empty());
    assert_eq!(rendered.shadow.entity_id, native.remote_id);
}

#[derive(Debug)]
struct FixtureNotionApi {
    pages: BTreeMap<String, PageDto>,
    children: BTreeMap<(String, Option<String>), BlockListDto>,
}

impl FixtureNotionApi {
    fn new() -> Self {
        let pages = BTreeMap::from([("page-1".to_string(), page("page-1", "Roadmap"))]);
        let mut children = BTreeMap::new();
        children.insert(
            ("page-1".to_string(), None),
            PaginatedListDto {
                results: vec![
                    rich_text_block("heading-1", "heading_1", "Roadmap"),
                    rich_text_block("paragraph-1", "paragraph", "Plan paragraph.").with_children(),
                ],
                next_cursor: Some("page-1-cursor-2".to_string()),
                has_more: true,
            },
        );
        children.insert(
            ("page-1".to_string(), Some("page-1-cursor-2".to_string())),
            PaginatedListDto {
                results: vec![block("divider-1", "divider")],
                next_cursor: None,
                has_more: false,
            },
        );
        children.insert(
            ("paragraph-1".to_string(), None),
            PaginatedListDto {
                results: vec![rich_text_block(
                    "nested-paragraph-1",
                    "paragraph",
                    "Nested detail.",
                )],
                next_cursor: None,
                has_more: false,
            },
        );

        Self { pages, children }
    }

    fn tree(root_page_id: &str) -> Self {
        let child_page_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let database_id = "cccccccccccccccccccccccccccccccc";
        let pages = BTreeMap::from([
            (root_page_id.to_string(), page(root_page_id, "Roadmap")),
            (
                child_page_id.to_string(),
                page(child_page_id, "Design Notes"),
            ),
        ]);
        let children = BTreeMap::from([
            (
                (root_page_id.to_string(), None),
                PaginatedListDto {
                    results: vec![
                        child_page_block(child_page_id, "Design Notes"),
                        child_database_block(database_id, "Tasks"),
                    ],
                    next_cursor: None,
                    has_more: false,
                },
            ),
            (
                (child_page_id.to_string(), None),
                PaginatedListDto::default(),
            ),
        ]);

        Self { pages, children }
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
}

trait WithChildren {
    fn with_children(self) -> Self;
}

impl WithChildren for BlockDto {
    fn with_children(mut self) -> Self {
        self.has_children = true;
        self
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
    let mut block = block(id, kind);
    let value = Some(RichTextBlockDto {
        rich_text: vec![rich_text(text)],
        color: None,
    });

    match kind {
        "paragraph" => block.paragraph = value,
        "heading_1" => block.heading_1 = value,
        "heading_2" => block.heading_2 = value,
        "heading_3" => block.heading_3 = value,
        _ => panic!("unsupported fixture rich text kind: {kind}"),
    }

    block
}

fn unsupported_block(id: &str, kind: &str) -> BlockDto {
    block(id, kind)
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
