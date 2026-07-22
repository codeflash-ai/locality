use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use locality_connector::{
    ChildContainer, Connector, ConnectorExecutionPolicy, EnumerateRequest, FetchRequest,
    ListChildrenRequest, NativeEntity, PORTABLE_SCOPE_ROOT_RELATIONSHIP, PortableBootstrapRequest,
    PortableFetchReason, PortableFetchRequest, PortableIncompleteReason, PortableRenderRequest,
    PortableSourceScope, PortableSyncRequest,
};
use locality_core::canonical::render_canonical_markdown;
use locality_core::model::{EntityKind, MountId, RemoteId};
use locality_core::portable::{
    LogicalPath, ProjectionFileKind, SourceAction, SourceConnectionId, SourceEdge,
};
use locality_core::shadow::MarkdownBlockKind;
use locality_notion::client::NotionApi;
use locality_notion::dto::{
    BlockDto, BlockListDto, BlockTreeDto, ColorOnlyBlockDto, DataSourceDto, DataSourcePropertyDto,
    DataSourceSummaryDto, DatabaseDto, DatabaseListDto, DateMentionDto, EmptyBlockDto,
    EquationBlockDto, EquationRichTextDto, ExternalFileDto, FileBlockDto, FilePropertyDto,
    HostedFileDto, IdRefDto, LinkDto, LinkToPageBlockDto, MeetingNotesBlockDto, MentionRichTextDto,
    NotionDatabaseBundle, NotionPortableIncompleteMediaV1, NotionPortablePageBundleV1, PageDto,
    PageListDto, PagePropertyDto, PaginatedListDto, ParentDto, RichTextAnnotationsDto,
    RichTextBlockDto, RichTextDto, SelectOptionDto, SelectPropertySchemaDto, SyncedBlockDto,
    SyncedFromDto, TableBlockDto, TableRowBlockDto, TextRichTextDto, TitleBlockDto,
    UniqueIdPropertyDto, UrlBlockDto, VerificationPropertyDto,
};
use locality_notion::media::{
    PORTABLE_MEDIA_MAX_ASSET_BYTES, PortableMediaCapture, PortableMediaCaptureFetcher,
    PortableMediaCapturePolicy,
};
use locality_notion::{NotionConfig, NotionConnector};
use serde_json::json;

mod support;

const PAGE_ID_ENV: &str = "LOCALITY_NOTION_PAGE_ID";
const LIVE_PARENT_ENV: &str = "LOCALITY_NOTION_LIVE_PARENT_PAGE";

#[test]
fn fetch_recurses_paginated_block_children_and_render_preserves_shadow_ids() {
    let api = FixtureNotionApi::new();
    let connector = NotionConnector::with_api(NotionConfig::default(), Arc::new(api));

    let native = connector
        .fetch(FetchRequest {
            remote_id: RemoteId::new("page-1"),
        })
        .expect("fetch");
    let bundle: locality_notion::dto::NotionPageBundle =
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
fn render_empty_paragraph_as_blank_line_without_shadow_marker() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Roadmap"),
        blocks: vec![
            BlockTreeDto {
                block: paragraph_block("paragraph-1", vec![rich_text("First paragraph.")]),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: paragraph_block("empty-paragraph", Vec::new()),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: paragraph_block("paragraph-2", vec![rich_text("Second paragraph.")]),
                children: Vec::new(),
            },
        ],
    };

    let rendered = locality_notion::render::render_page_bundle(&bundle).expect("render");

    assert_eq!(
        rendered.document.body,
        "First paragraph.\n\nSecond paragraph.\n"
    );
    assert!(!rendered.document.body.contains("empty_paragraph"));
    assert_eq!(
        rendered
            .shadow
            .blocks
            .iter()
            .map(|block| block.remote_id.as_str())
            .collect::<Vec<_>>(),
        vec!["paragraph-1", "paragraph-2"]
    );
}

#[test]
fn render_consecutive_empty_paragraphs_as_one_blank_line_each() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Roadmap"),
        blocks: vec![
            BlockTreeDto {
                block: paragraph_block("paragraph-1", vec![rich_text("Before.")]),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: paragraph_block("empty-paragraph-1", Vec::new()),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: paragraph_block("empty-paragraph-2", Vec::new()),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: code_block("code-1", "python", "def hello():\n    print(\"hi\")"),
                children: Vec::new(),
            },
        ],
    };

    let rendered = locality_notion::render::render_page_bundle(&bundle).expect("render");

    assert_eq!(
        rendered.document.body,
        "Before.\n\n\n```python\ndef hello():\n    print(\"hi\")\n```\n"
    );
    assert_eq!(
        rendered
            .shadow
            .blocks
            .iter()
            .map(|block| block.remote_id.as_str())
            .collect::<Vec<_>>(),
        vec!["paragraph-1", "code-1"]
    );
}

#[test]
fn render_consecutive_list_blocks_as_tight_markdown_list() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Roadmap"),
        blocks: vec![
            BlockTreeDto {
                block: paragraph_block("intro", vec![rich_text("Intro.")]),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: rich_text_block("bullet-1", "bulleted_list_item", "First bullet"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: rich_text_block("bullet-2", "bulleted_list_item", "Second bullet"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: to_do_block("todo-1", "First task", false),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: to_do_block("todo-2", "Done task", true),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: rich_text_block("number-1", "numbered_list_item", "First number"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: rich_text_block("number-2", "numbered_list_item", "Second number"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: paragraph_block("after", vec![rich_text("After.")]),
                children: Vec::new(),
            },
        ],
    };

    let rendered = locality_notion::render::render_page_bundle(&bundle).expect("render");

    assert_eq!(
        rendered.document.body,
        "Intro.\n\n- First bullet\n- Second bullet\n- [ ] First task\n- [x] Done task\n1. First number\n2. Second number\n\nAfter.\n"
    );
    assert_eq!(
        rendered
            .shadow
            .blocks
            .iter()
            .map(|block| block.remote_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "intro", "bullet-1", "bullet-2", "todo-1", "todo-2", "number-1", "number-2", "after",
        ]
    );
    for block in &rendered.shadow.blocks[1..7] {
        assert_eq!(block.kind, MarkdownBlockKind::List);
    }
}

#[test]
fn render_empty_bulleted_list_item_as_empty_markdown_bullet() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Roadmap"),
        blocks: vec![BlockTreeDto {
            block: rich_text_block("empty-bullet", "bulleted_list_item", ""),
            children: Vec::new(),
        }],
    };

    let rendered = locality_notion::render::render_page_bundle(&bundle).expect("render");

    assert_eq!(rendered.document.body, "-\n");
    assert_eq!(
        rendered
            .shadow
            .blocks
            .iter()
            .map(|block| (block.remote_id.as_str(), block.kind.clone()))
            .collect::<Vec<_>>(),
        vec![("empty-bullet", MarkdownBlockKind::List)]
    );
}

#[test]
fn render_rich_text_line_breaks_inside_one_shadow_block() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Roadmap"),
        blocks: vec![BlockTreeDto {
            block: paragraph_block(
                "paragraph-1",
                vec![rich_text(
                    "First line.\n\n# Still paragraph text\n- Also text",
                )],
            ),
            children: Vec::new(),
        }],
    };

    let rendered = locality_notion::render::render_page_bundle(&bundle).expect("render");

    assert_eq!(
        rendered.document.body,
        "First line.<br><br># Still paragraph text<br>- Also text\n"
    );
    assert_eq!(
        rendered
            .shadow
            .blocks
            .iter()
            .map(|block| block.remote_id.as_str())
            .collect::<Vec<_>>(),
        vec!["paragraph-1"]
    );
}

#[test]
fn render_paragraph_escapes_literal_block_markers_at_start() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Roadmap"),
        blocks: vec![
            BlockTreeDto {
                block: paragraph_block("heading-marker", vec![rich_text("# Literal heading")]),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: paragraph_block("bullet-marker", vec![rich_text("- Literal bullet")]),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: paragraph_block("number-marker", vec![rich_text("1. Literal number")]),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: paragraph_block("quote-marker", vec![rich_text("> Literal quote")]),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: paragraph_block("divider-marker", vec![rich_text("---")]),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: paragraph_block(
                    "directive-marker",
                    vec![rich_text("::loc{id=literal type=paragraph}")],
                ),
                children: Vec::new(),
            },
        ],
    };

    let rendered = locality_notion::render::render_page_bundle(&bundle).expect("render");

    assert_eq!(
        rendered.document.body,
        "\\# Literal heading\n\n\\- Literal bullet\n\n\\1. Literal number\n\n\\> Literal quote\n\n\\---\n\n\\::loc{id=literal type=paragraph}\n"
    );
    assert_eq!(
        rendered
            .shadow
            .blocks
            .iter()
            .map(|block| (&block.remote_id, &block.kind))
            .collect::<Vec<_>>(),
        vec![
            (
                &RemoteId::new("heading-marker"),
                &MarkdownBlockKind::Paragraph
            ),
            (
                &RemoteId::new("bullet-marker"),
                &MarkdownBlockKind::Paragraph
            ),
            (
                &RemoteId::new("number-marker"),
                &MarkdownBlockKind::Paragraph
            ),
            (
                &RemoteId::new("quote-marker"),
                &MarkdownBlockKind::Paragraph
            ),
            (
                &RemoteId::new("divider-marker"),
                &MarkdownBlockKind::Paragraph
            ),
            (
                &RemoteId::new("directive-marker"),
                &MarkdownBlockKind::Paragraph
            ),
        ]
    );
}

#[test]
fn render_rich_text_escapes_literal_break_tags() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Roadmap"),
        blocks: vec![BlockTreeDto {
            block: paragraph_block("paragraph-1", vec![rich_text("Literal <br> tag")]),
            children: Vec::new(),
        }],
    };

    let rendered = locality_notion::render::render_page_bundle(&bundle).expect("render");

    assert_eq!(rendered.document.body, "Literal \\<br> tag\n");
    assert_eq!(rendered.shadow.blocks.len(), 1);
}

#[test]
fn render_rich_text_escapes_literal_underline_tags() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Roadmap"),
        blocks: vec![BlockTreeDto {
            block: paragraph_block("paragraph-1", vec![rich_text("Literal <u>tag</u>")]),
            children: Vec::new(),
        }],
    };

    let rendered = locality_notion::render::render_page_bundle(&bundle).expect("render");

    assert_eq!(rendered.document.body, "Literal \\<u>tag\\</u>\n");
    assert_eq!(rendered.shadow.blocks.len(), 1);
}

#[test]
fn render_rich_text_escapes_literal_equation_markers() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Roadmap"),
        blocks: vec![BlockTreeDto {
            block: paragraph_block("paragraph-1", vec![rich_text("Literal $E=mc^2$ text")]),
            children: Vec::new(),
        }],
    };

    let rendered = locality_notion::render::render_page_bundle(&bundle).expect("render");

    assert_eq!(rendered.document.body, "Literal \\$E=mc^2\\$ text\n");
    assert_eq!(rendered.shadow.blocks.len(), 1);
}

#[test]
fn render_rich_text_escapes_literal_currency_dollars() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Roadmap"),
        blocks: vec![BlockTreeDto {
            block: paragraph_block(
                "paragraph-1",
                vec![rich_text("Raised $500k at a $16,000,000 cap.")],
            ),
            children: Vec::new(),
        }],
    };

    let rendered = locality_notion::render::render_page_bundle(&bundle).expect("render");

    assert_eq!(
        rendered.document.body,
        "Raised \\$500k at a \\$16,000,000 cap.\n"
    );
    assert_eq!(rendered.shadow.blocks.len(), 1);
}

#[test]
fn render_rich_text_escapes_literal_explicit_mention_markers() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Roadmap"),
        blocks: vec![BlockTreeDto {
            block: paragraph_block(
                "paragraph-1",
                vec![rich_text(
                    "Literal @date(2026-06-14), @page(22222222-2222-2222-2222-222222222222), @database(33333333-3333-3333-3333-333333333333), and @user(44444444-4444-4444-4444-444444444444)",
                )],
            ),
            children: Vec::new(),
        }],
    };

    let rendered = locality_notion::render::render_page_bundle(&bundle).expect("render");

    assert_eq!(
        rendered.document.body,
        "Literal \\@date(2026-06-14), \\@page(22222222-2222-2222-2222-222222222222), \\@database(33333333-3333-3333-3333-333333333333), and \\@user(44444444-4444-4444-4444-444444444444)\n"
    );
    assert_eq!(rendered.shadow.blocks.len(), 1);
}

#[test]
fn render_rich_text_escapes_literal_markdown_inline_markers() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Roadmap"),
        blocks: vec![BlockTreeDto {
            block: paragraph_block(
                "paragraph-1",
                vec![rich_text(
                    "Literal **bold** _italic_ ~~strike~~ `code` [link](https://example.com)",
                )],
            ),
            children: Vec::new(),
        }],
    };

    let rendered = locality_notion::render::render_page_bundle(&bundle).expect("render");

    assert_eq!(
        rendered.document.body,
        "Literal \\**bold\\** \\_italic\\_ \\~~strike\\~~ \\`code\\` \\[link](https://example.com)\n"
    );
    assert_eq!(rendered.shadow.blocks.len(), 1);
}

#[test]
fn fetch_does_not_inline_child_page_or_database_content_into_parent_body() {
    let api = FixtureNotionApi::parent_with_child_boundaries();
    let connector = NotionConnector::with_api(NotionConfig::default(), Arc::new(api));

    let native = connector
        .fetch(FetchRequest {
            remote_id: RemoteId::new("parent-page"),
        })
        .expect("fetch parent");
    let bundle: locality_notion::dto::NotionPageBundle =
        serde_json::from_slice(&native.raw).expect("native bundle");

    assert_eq!(bundle.blocks.len(), 3);
    assert!(bundle.blocks.iter().all(|tree| tree.children.is_empty()));

    let rendered = connector
        .render_native_entity(&native)
        .expect("render parent");

    assert_eq!(
        rendered.document.body,
        "Parent body.\n\n[Child Page](https://www.notion.so/child-page)\n\n::loc{id=child-db type=child_database title=\"Tasks\"}\n"
    );
    assert_eq!(
        rendered
            .shadow
            .blocks
            .iter()
            .map(|block| block.remote_id.as_str())
            .collect::<Vec<_>>(),
        vec!["parent-paragraph", "child-page", "child-db"]
    );
    assert!(!rendered.document.body.contains("Child body."));
    assert!(!rendered.document.body.contains("Database body."));
}

#[test]
fn render_unsupported_block_as_directive_without_consuming_native_shadow_id() {
    let page = page("page-1", "Roadmap");
    let block = BlockTreeDto {
        block: unsupported_block("future-1", "future_block"),
        children: Vec::new(),
    };
    let bundle = locality_notion::dto::NotionPageBundle {
        page,
        blocks: vec![block],
    };
    let raw = serde_json::to_vec(&bundle).expect("raw");
    let native = locality_connector::NativeEntity {
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
        "::loc{id=future-1 type=unsupported_future_block}\n"
    );
    assert_eq!(rendered.shadow.blocks.len(), 1);
    assert_eq!(
        rendered.shadow.blocks[0].remote_id,
        RemoteId::new("future-1")
    );
    assert!(matches!(
        rendered.shadow.blocks[0].kind,
        MarkdownBlockKind::Directive { .. }
    ));
}

#[test]
fn render_notion_unsupported_block_as_labeled_directive() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Unsupported"),
        blocks: vec![BlockTreeDto {
            block: unsupported_block("unsupported-1", "unsupported"),
            children: Vec::new(),
        }],
    };

    let rendered = locality_notion::render::render_page_bundle(&bundle).expect("render");

    assert_eq!(
        rendered.document.body,
        "::loc{id=unsupported-1 type=unsupported title=\"Unsupported Notion block\"}\n"
    );
    assert_eq!(rendered.shadow.blocks.len(), 1);
    assert_eq!(
        rendered.shadow.blocks[0].remote_id,
        RemoteId::new("unsupported-1")
    );
    assert!(matches!(
        rendered.shadow.blocks[0].kind,
        MarkdownBlockKind::Directive { .. }
    ));
}

#[test]
fn render_subtype_only_unsupported_artifacts_as_empty_markdown() {
    let raw = serde_json::to_vec(&json!({
        "page": page("page-1", "Unsupported Artifacts"),
        "blocks": [
            {
                "block": rich_text_block("paragraph-1", "paragraph", "Before"),
                "children": [],
            },
            {
                "block": {
                    "id": "copy-indicator-1",
                    "type": "unsupported",
                    "unsupported": { "block_type": "copy_indicator" }
                },
                "children": [],
            },
            {
                "block": {
                    "id": "button-1",
                    "type": "unsupported",
                    "unsupported": { "block_type": "button" }
                },
                "children": [],
            },
            {
                "block": {
                    "id": "alias-1",
                    "type": "unsupported",
                    "unsupported": { "block_type": "alias" }
                },
                "children": [],
            },
            {
                "block": rich_text_block("paragraph-2", "paragraph", "After"),
                "children": [],
            },
        ],
    }))
    .expect("raw bundle");
    let native = locality_connector::NativeEntity {
        remote_id: RemoteId::new("page-1"),
        kind: "notion_page".to_string(),
        raw,
    };

    let rendered = locality_notion::render::render_native_entity(&native).expect("render");

    assert_eq!(rendered.document.body, "Before\n\nAfter\n");
    assert!(!rendered.document.body.contains("::loc"));
    assert_eq!(
        rendered
            .shadow
            .blocks
            .iter()
            .map(|block| block.remote_id.as_str())
            .collect::<Vec<_>>(),
        vec!["paragraph-1", "paragraph-2"]
    );
}

#[test]
fn render_code_block_uses_fence_longer_than_embedded_backticks() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Roadmap"),
        blocks: vec![BlockTreeDto {
            block: code_block(
                "code-1",
                "markdown",
                "Before\n```python\nprint('nested')\n```\nAfter",
            ),
            children: Vec::new(),
        }],
    };
    let raw = serde_json::to_vec(&bundle).expect("raw");
    let native = locality_connector::NativeEntity {
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
        "````markdown\nBefore\n```python\nprint('nested')\n```\nAfter\n````\n"
    );
    assert_eq!(rendered.shadow.blocks.len(), 1);
    assert_eq!(rendered.shadow.blocks[0].remote_id, RemoteId::new("code-1"));
    assert!(matches!(
        rendered.shadow.blocks[0].kind,
        MarkdownBlockKind::CodeFence
    ));
}

#[test]
fn render_toggle_children_as_nested_markdown() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Roadmap"),
        blocks: vec![BlockTreeDto {
            block: toggle_block("toggle-1", "toggle heading"),
            children: vec![
                BlockTreeDto {
                    block: rich_text_block("toggle-heading-1", "heading_3", "Toggle body"),
                    children: Vec::new(),
                },
                BlockTreeDto {
                    block: rich_text_block(
                        "toggle-paragraph-1",
                        "paragraph",
                        "continue toggle body",
                    ),
                    children: Vec::new(),
                },
            ],
        }],
    };

    let rendered = locality_notion::render::render_page_bundle(&bundle).expect("render");

    assert_eq!(
        rendered.document.body,
        "- toggle heading\n\n    ### Toggle body\n\n    continue toggle body\n"
    );
    assert_eq!(
        rendered
            .shadow
            .blocks
            .iter()
            .map(|block| block.remote_id.as_str())
            .collect::<Vec<_>>(),
        vec!["toggle-1", "toggle-heading-1", "toggle-paragraph-1"]
    );
}

#[test]
fn render_richer_notion_block_coverage() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Coverage"),
        blocks: vec![
            BlockTreeDto {
                block: rich_text_block("heading-4", "heading_4", "Heading four"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: toggle_block("toggle-1", "Toggle summary"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: equation_block("equation-1", "E=mc^2"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: url_block(
                    "embed-1",
                    "embed",
                    "https://example.com/embed",
                    "Embed caption",
                ),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: url_block(
                    "bookmark-1",
                    "bookmark",
                    "https://example.com/bookmark",
                    "Bookmark caption",
                ),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: file_block(
                    "image-1",
                    "image",
                    "https://example.com/image.png",
                    "Image caption",
                ),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: synced_block("synced-1", "source-block-1"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: link_to_page_block("link-to-page-1", "target-page-1"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: table_of_contents_block("toc-1"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: block("breadcrumb-1", "breadcrumb"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: block("column-list-1", "column_list"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: block("column-1", "column"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: meeting_notes_block("meeting-1", "Weekly sync"),
                children: Vec::new(),
            },
        ],
    };

    let rendered = locality_notion::render::render_page_bundle(&bundle).expect("render");

    assert_eq!(
        rendered.document.body,
        concat!(
            "#### Heading four\n\n",
            "- Toggle summary\n\n",
            "$$\nE=mc^2\n$$\n\n",
            "[Embed caption](https://example.com/embed)\n\n",
            "[Bookmark caption](https://example.com/bookmark)\n\n",
            "![Image caption](https://example.com/image.png)\n\n",
            "::loc{id=synced-1 type=synced_block source_block_id=\"source-block-1\"}\n\n",
            "[Linked page](https://www.notion.so/target-page-1)\n\n",
            "::loc{id=toc-1 type=table_of_contents color=\"default\"}\n\n",
            "::loc{id=breadcrumb-1 type=breadcrumb}\n\n",
            "::loc{id=column-list-1 type=column_list}\n\n",
            "::loc{id=column-1 type=column}\n\n",
            "::loc{id=meeting-1 type=meeting_notes title=\"Weekly sync\"}\n"
        )
    );
    assert_eq!(
        rendered
            .shadow
            .blocks
            .iter()
            .map(|block| block.remote_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "heading-4",
            "toggle-1",
            "equation-1",
            "embed-1",
            "bookmark-1",
            "image-1",
            "synced-1",
            "link-to-page-1",
            "toc-1",
            "breadcrumb-1",
            "column-list-1",
            "column-1",
            "meeting-1",
        ]
    );
}

#[test]
fn render_all_known_notion_block_objects_into_markdown_or_directives() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Coverage"),
        blocks: vec![
            BlockTreeDto {
                block: rich_text_block("paragraph-1", "paragraph", "Paragraph"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: rich_text_block("heading-1", "heading_1", "Heading one"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: rich_text_block("heading-2", "heading_2", "Heading two"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: rich_text_block("heading-3", "heading_3", "Heading three"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: rich_text_block("heading-4", "heading_4", "Heading four"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: rich_text_block("bullet-1", "bulleted_list_item", "Bullet"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: rich_text_block("number-1", "numbered_list_item", "Number"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: to_do_block("todo-1", "Todo", true),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: rich_text_block("quote-1", "quote", "Quote"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: rich_text_block("callout-1", "callout", "Callout"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: code_block("code-1", "rust", "fn main() {}"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: block("divider-1", "divider"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: table_block("table-1", 2, true),
                children: vec![
                    BlockTreeDto {
                        block: table_row_block("row-1", ["Left", "Right"]),
                        children: Vec::new(),
                    },
                    BlockTreeDto {
                        block: table_row_block("row-2", ["A", "B"]),
                        children: Vec::new(),
                    },
                ],
            },
            BlockTreeDto {
                block: table_row_block("orphan-row-1", ["Orphan"]),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: child_page_block("child-page-1", "Child Page"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: child_database_block("child-db-1", "Child DB"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: toggle_block("toggle-1", "Toggle summary"),
                children: vec![BlockTreeDto {
                    block: rich_text_block("toggle-child-1", "paragraph", "Toggle child"),
                    children: Vec::new(),
                }],
            },
            BlockTreeDto {
                block: equation_block("equation-1", "E=mc^2"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: url_block("embed-1", "embed", "https://example.com/embed", "Embed"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: url_block(
                    "bookmark-1",
                    "bookmark",
                    "https://example.com/bookmark",
                    "Bookmark",
                ),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: url_block(
                    "link-preview-1",
                    "link_preview",
                    "https://example.com/preview",
                    "Preview",
                ),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: file_block(
                    "111111111111aaaa",
                    "image",
                    "https://example.com/image.png",
                    "Image",
                ),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: file_block(
                    "222222222222bbbb",
                    "video",
                    "https://example.com/video.mp4",
                    "Video",
                ),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: file_block(
                    "333333333333cccc",
                    "file",
                    "https://example.com/file.txt",
                    "File",
                ),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: file_block(
                    "444444444444dddd",
                    "pdf",
                    "https://example.com/file.pdf",
                    "PDF",
                ),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: file_block(
                    "555555555555eeee",
                    "audio",
                    "https://example.com/audio.mp3",
                    "Audio",
                ),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: synced_block("synced-original-1", ""),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: synced_block("synced-copy-1", "source-block-1"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: link_to_page_block("link-page-1", "target-page-1"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: link_to_database_block("link-db-1", "target-db-1"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: table_of_contents_block("toc-1"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: empty_payload_block("breadcrumb-1", "breadcrumb"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: empty_payload_block("column-list-1", "column_list"),
                children: vec![
                    BlockTreeDto {
                        block: empty_payload_block("column-1", "column"),
                        children: vec![BlockTreeDto {
                            block: rich_text_block("column-child-1", "paragraph", "Column one"),
                            children: Vec::new(),
                        }],
                    },
                    BlockTreeDto {
                        block: empty_payload_block("column-2", "column"),
                        children: vec![BlockTreeDto {
                            block: rich_text_block("column-child-2", "paragraph", "Column two"),
                            children: Vec::new(),
                        }],
                    },
                ],
            },
            BlockTreeDto {
                block: rich_text_block("template-1", "template", "Template"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: meeting_notes_block("meeting-1", "Meeting"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: transcription_block("transcription-1", "Transcript"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: raw_payload_block("tab-1", "tab"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: raw_payload_block("ai-1", "ai_block"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: raw_payload_block("custom-1", "custom_block"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: raw_payload_block("button-1", "button"),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: unsupported_block("future-1", "future_block"),
                children: Vec::new(),
            },
        ],
    };

    let rendered = locality_notion::render::render_page_bundle_with_options(
        &bundle,
        &locality_notion::render::RenderOptions::with_page_path("Docs/Coverage.md"),
    )
    .expect("render");
    let body = &rendered.document.body;

    for expected in [
        "Paragraph",
        "# Heading one",
        "## Heading two",
        "### Heading three",
        "#### Heading four",
        "- Bullet",
        "1. Number",
        "- [x] Todo",
        "> Quote",
        "> [!NOTE]\n> Callout",
        "```rust\nfn main() {}\n```",
        "| Left | Right |",
        "::loc{id=orphan-row-1 type=unsupported_table_row}",
        "[Child Page](https://www.notion.so/child-page-1)",
        "::loc{id=child-db-1 type=child_database title=\"Child DB\"}",
        "- Toggle summary",
        "    Toggle child",
        "$$\nE=mc^2\n$$",
        "[Embed](https://example.com/embed)",
        "[Bookmark](https://example.com/bookmark)",
        "[Preview](https://example.com/preview)",
        "![Image](../.loc/media/Docs/Coverage/image-111111111111aaaa.png)",
        "[Video](../.loc/media/Docs/Coverage/video-222222222222bbbb.mp4)",
        "[File](../.loc/media/Docs/Coverage/file-333333333333cccc.txt)",
        "[PDF](../.loc/media/Docs/Coverage/pdf-444444444444dddd.pdf)",
        "[Audio](../.loc/media/Docs/Coverage/audio-555555555555eeee.mp3)",
        "::loc{id=synced-original-1 type=synced_block}",
        "::loc{id=synced-copy-1 type=synced_block source_block_id=\"source-block-1\"}",
        "[Linked page](https://www.notion.so/target-page-1)",
        "[Linked database](https://www.notion.so/target-db-1)",
        "::loc{id=toc-1 type=table_of_contents color=\"default\"}",
        "::loc{id=breadcrumb-1 type=breadcrumb}",
        "::loc{id=column-list-1 type=column_list}",
        "::loc{id=column-1 type=column}",
        "Column one",
        "::loc{id=column-2 type=column}",
        "Column two",
        "::loc{id=template-1 type=template title=\"Template\"}",
        "::loc{id=meeting-1 type=meeting_notes title=\"Meeting\"}",
        "::loc{id=transcription-1 type=transcription title=\"Transcript\"}",
        "::loc{id=tab-1 type=tab}",
        "::loc{id=ai-1 type=ai_block}",
        "::loc{id=custom-1 type=custom_block}",
        "::loc{id=button-1 type=button}",
        "::loc{id=future-1 type=unsupported_future_block}",
    ] {
        assert!(
            body.contains(expected),
            "missing rendered coverage: {expected}"
        );
    }

    assert_eq!(
        rendered
            .media_assets
            .iter()
            .map(|asset| asset.kind.as_str())
            .collect::<Vec<_>>(),
        vec!["image", "video", "file", "pdf", "audio"]
    );
    let link_preview_shadow = rendered
        .shadow
        .blocks
        .iter()
        .find(|block| block.remote_id == RemoteId::new("link-preview-1"))
        .expect("link_preview shadow block");
    assert_eq!(
        link_preview_shadow.native_kind.as_deref(),
        Some("link_preview")
    );
}

#[test]
fn render_table_as_markdown_table_with_row_shadow_metadata() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Roadmap"),
        blocks: vec![BlockTreeDto {
            block: table_block("table-1", 2, true),
            children: vec![
                BlockTreeDto {
                    block: table_row_block("row-1", ["Decision", "Choice"]),
                    children: Vec::new(),
                },
                BlockTreeDto {
                    block: table_row_block("row-2", ["First connector", "Notion"]),
                    children: Vec::new(),
                },
            ],
        }],
    };
    let raw = serde_json::to_vec(&bundle).expect("raw");
    let native = locality_connector::NativeEntity {
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
        "| Decision | Choice |\n| --- | --- |\n| First connector | Notion |\n"
    );
    assert_eq!(rendered.shadow.blocks.len(), 1);
    assert_eq!(
        rendered.shadow.blocks[0].remote_id,
        RemoteId::new("table-1")
    );
    assert_eq!(
        rendered.shadow.blocks[0].kind,
        MarkdownBlockKind::TableWithRows {
            row_ids: vec![RemoteId::new("row-1"), RemoteId::new("row-2")],
            has_column_header: true,
            has_row_header: false,
        }
    );
}

#[test]
fn render_table_metadata_skips_blank_blocks_when_matching_shadow_blocks() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Roadmap"),
        blocks: vec![
            BlockTreeDto {
                block: paragraph_block("empty-paragraph", Vec::new()),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: table_block("table-1", 2, true),
                children: vec![
                    BlockTreeDto {
                        block: table_row_block("row-1", ["Decision", "Choice"]),
                        children: Vec::new(),
                    },
                    BlockTreeDto {
                        block: table_row_block("row-2", ["First connector", "Notion"]),
                        children: Vec::new(),
                    },
                ],
            },
            BlockTreeDto {
                block: block("divider-1", "divider"),
                children: Vec::new(),
            },
        ],
    };

    let rendered = locality_notion::render::render_page_bundle(&bundle).expect("render");

    assert_eq!(
        rendered
            .shadow
            .blocks
            .iter()
            .map(|block| block.remote_id.as_str())
            .collect::<Vec<_>>(),
        vec!["table-1", "divider-1"]
    );
    assert_eq!(
        rendered.shadow.blocks[0].kind,
        MarkdownBlockKind::TableWithRows {
            row_ids: vec![RemoteId::new("row-1"), RemoteId::new("row-2")],
            has_column_header: true,
            has_row_header: false,
        }
    );
    assert_eq!(rendered.shadow.blocks[1].kind, MarkdownBlockKind::Paragraph);
}

#[test]
fn render_malformed_table_as_directives() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Roadmap"),
        blocks: vec![BlockTreeDto {
            block: table_block("table-1", 3, true),
            children: vec![BlockTreeDto {
                block: table_row_block("row-1", ["Decision", "Choice"]),
                children: Vec::new(),
            }],
        }],
    };
    let raw = serde_json::to_vec(&bundle).expect("raw");
    let native = locality_connector::NativeEntity {
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
        "::loc{id=table-1 type=unsupported_table}\n\n::loc{id=row-1 type=unsupported_table_row}\n"
    );
    assert!(
        rendered
            .shadow
            .blocks
            .iter()
            .all(|block| matches!(block.kind, MarkdownBlockKind::Directive { .. }))
    );
}

#[test]
fn render_media_blocks_as_markdown_links_and_tracks_local_paths() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Coverage"),
        blocks: vec![BlockTreeDto {
            block: file_block(
                "0123456789abcdef",
                "image",
                "https://example.com/image.PNG?download=1",
                "Image caption",
            ),
            children: Vec::new(),
        }],
    };

    let rendered = locality_notion::render::render_page_bundle_with_options(
        &bundle,
        &locality_notion::render::RenderOptions::with_page_path("Docs/Coverage/page.md"),
    )
    .expect("render");

    assert_eq!(rendered.media_assets.len(), 1);
    assert_eq!(
        rendered.media_assets[0].local_path,
        Path::new(".loc/media/Docs/Coverage/image-0123456789abcdef.png")
    );
    assert_eq!(
        rendered.document.body,
        "![Image caption](../../.loc/media/Docs/Coverage/image-0123456789abcdef.png)\n"
    );
}

#[test]
fn render_file_like_media_blocks_as_local_links_when_downloaded() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Coverage"),
        blocks: vec![
            BlockTreeDto {
                block: file_block(
                    "1111111111111111",
                    "video",
                    "https://example.com/cars.MP4?download=1",
                    "Cars",
                ),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: file_block(
                    "2222222222222222",
                    "pdf",
                    "https://example.com/brief.PDF?download=1",
                    "Brief",
                ),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: file_block(
                    "3333333333333333",
                    "audio",
                    "https://example.com/theme.MP3?download=1",
                    "Theme",
                ),
                children: Vec::new(),
            },
            BlockTreeDto {
                block: file_block(
                    "4444444444444444",
                    "file",
                    "https://example.com/index.HTML?download=1",
                    "Index",
                ),
                children: Vec::new(),
            },
        ],
    };

    let rendered = locality_notion::render::render_page_bundle_with_options(
        &bundle,
        &locality_notion::render::RenderOptions::with_page_path("Docs/Coverage/page.md"),
    )
    .expect("render");

    assert_eq!(
        rendered
            .media_assets
            .iter()
            .map(|asset| (&asset.kind, asset.local_path.as_path()))
            .collect::<Vec<_>>(),
        vec![
            (
                &"video".to_string(),
                Path::new(".loc/media/Docs/Coverage/video-1111111111111111.mp4")
            ),
            (
                &"pdf".to_string(),
                Path::new(".loc/media/Docs/Coverage/pdf-2222222222222222.pdf")
            ),
            (
                &"audio".to_string(),
                Path::new(".loc/media/Docs/Coverage/audio-3333333333333333.mp3")
            ),
            (
                &"file".to_string(),
                Path::new(".loc/media/Docs/Coverage/file-4444444444444444.html")
            ),
        ]
    );
    assert_eq!(
        rendered.document.body,
        "[Cars](../../.loc/media/Docs/Coverage/video-1111111111111111.mp4)\n\n[Brief](../../.loc/media/Docs/Coverage/pdf-2222222222222222.pdf)\n\n[Theme](../../.loc/media/Docs/Coverage/audio-3333333333333333.mp3)\n\n[Index](../../.loc/media/Docs/Coverage/file-4444444444444444.html)\n"
    );
}

#[test]
fn render_media_blocks_can_keep_failed_downloads_as_remote_urls() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Coverage"),
        blocks: vec![BlockTreeDto {
            block: file_block(
                "0123456789abcdef",
                "image",
                "https://example.com/image.PNG?download=1",
                "Image caption",
            ),
            children: Vec::new(),
        }],
    };

    let rendered = locality_notion::render::render_page_bundle_with_options(
        &bundle,
        &locality_notion::render::RenderOptions::with_page_path("Docs/Coverage/page.md")
            .with_local_media_block_ids(Vec::<String>::new()),
    )
    .expect("render");

    assert_eq!(rendered.media_assets.len(), 1);
    assert_eq!(
        rendered.media_assets[0].local_path,
        Path::new(".loc/media/Docs/Coverage/image-0123456789abcdef.png")
    );
    assert_eq!(
        rendered.document.body,
        "![Image caption](https://example.com/image.PNG?download=1)\n"
    );
    assert_eq!(rendered.shadow.rendered_body, rendered.document.body);
}

#[test]
fn render_relative_media_url_without_local_download_asset() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Coverage"),
        blocks: vec![BlockTreeDto {
            block: file_block("image-1", "image", "img_2.png", ""),
            children: Vec::new(),
        }],
    };

    let rendered = locality_notion::render::render_page_bundle_with_options(
        &bundle,
        &locality_notion::render::RenderOptions::with_page_path("Docs/Coverage/page.md"),
    )
    .expect("render");

    assert!(rendered.media_assets.is_empty());
    assert_eq!(rendered.document.body, "![Image](img_2.png)\n");
    assert_eq!(rendered.shadow.blocks.len(), 1);
    assert_eq!(
        rendered.shadow.blocks[0].remote_id,
        RemoteId::new("image-1")
    );
}

#[test]
fn render_notion_hosted_media_file_url_as_markdown_image() {
    let mut block = block("hosted-image-1", "image");
    block.image = Some(FileBlockDto {
        kind: "file".to_string(),
        external: None,
        file: Some(HostedFileDto {
            url: "https://s3.us-west-2.amazonaws.com/secure.notion-static.com/image.png?X-Amz-Signature=abc"
                .to_string(),
            expiry_time: Some("2026-06-12T10:00:00.000Z".to_string()),
        }),
        caption: Vec::new(),
    });
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Coverage"),
        blocks: vec![BlockTreeDto {
            block,
            children: Vec::new(),
        }],
    };

    let rendered = locality_notion::render::render_page_bundle(&bundle).expect("render");

    assert_eq!(
        rendered.document.body,
        "![Image](https://s3.us-west-2.amazonaws.com/secure.notion-static.com/image.png?X-Amz-Signature=abc)\n"
    );
}

#[test]
fn render_url_less_media_payload_as_directive() {
    let mut block = block("image-without-url", "image");
    block.image = Some(FileBlockDto {
        kind: "file".to_string(),
        external: None,
        file: None,
        caption: vec![rich_text("Image caption")],
    });
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Coverage"),
        blocks: vec![BlockTreeDto {
            block,
            children: Vec::new(),
        }],
    };

    let rendered = locality_notion::render::render_page_bundle(&bundle).expect("render");

    assert_eq!(
        rendered.document.body,
        "::loc{id=image-without-url type=image title=\"Image caption\"}\n"
    );
}

#[test]
fn render_rich_text_annotations_links_mentions_and_equations() {
    let mut bold = rich_text("Bold");
    bold.annotations.bold = true;

    let mut italic = rich_text(" italic");
    italic.annotations.italic = true;

    let mut strikethrough = rich_text(" strike");
    strikethrough.annotations.strikethrough = true;

    let mut underline = rich_text(" underline");
    underline.annotations.underline = true;

    let mut code = rich_text(" code");
    code.annotations.code = true;

    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Roadmap"),
        blocks: vec![BlockTreeDto {
            block: paragraph_block(
                "paragraph-1",
                vec![
                    bold,
                    italic,
                    strikethrough,
                    underline,
                    code,
                    linked_text(" external link", "https://example.com/"),
                    rich_text(" after link."),
                    rich_text(" "),
                    date_mention("2026-06-10", "2026-06-10"),
                    rich_text(" and inline equation "),
                    equation("E=mc^2"),
                    rich_text(" plus page mention "),
                    page_mention("Roadmap", "page-1"),
                    rich_text(" database mention "),
                    database_mention("Tasks", "database-1"),
                    rich_text(" user mention "),
                    user_mention("", "user-1", "Ada"),
                    rich_text(" preview "),
                    link_preview_mention("Example", "https://example.com/preview"),
                    rich_text(" unknown "),
                    unknown_mention("Fallback"),
                ],
            ),
            children: Vec::new(),
        }],
    };
    let raw = serde_json::to_vec(&bundle).expect("raw");
    let native = locality_connector::NativeEntity {
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
        "**Bold** _italic_ ~~strike~~ <u>underline</u> `code` [external link](https://example.com/) after link. 2026-06-10 and inline equation $E=mc^2$ plus page mention [Roadmap](https://www.notion.so/page-1) database mention [Tasks](https://www.notion.so/database-1) user mention @Ada preview [Example](https://example.com/preview) unknown Fallback\n"
    );
}

#[test]
fn render_rich_text_link_escapes_unbalanced_href_parentheses() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-1", "Roadmap"),
        blocks: vec![BlockTreeDto {
            block: paragraph_block(
                "paragraph-1",
                vec![linked_text("Paren link", "https://example.com/docs/foo)")],
            ),
            children: Vec::new(),
        }],
    };

    let rendered = locality_notion::render::render_page_bundle(&bundle).expect("render");

    assert_eq!(
        rendered.document.body,
        "[Paren link](https://example.com/docs/foo\\))\n"
    );
}

#[test]
fn render_database_row_properties_as_frontmatter() {
    let mut row = page("row-1", "Fix login bug");
    row.properties.insert(
        "Status".to_string(),
        PagePropertyDto {
            kind: "select".to_string(),
            select: Some(select_option("status-id", "In progress")),
            ..Default::default()
        },
    );
    row.properties.insert(
        "Points".to_string(),
        PagePropertyDto {
            kind: "number".to_string(),
            number: Some(serde_json::Number::from(3)),
            ..Default::default()
        },
    );
    row.properties.insert(
        "Tags".to_string(),
        PagePropertyDto {
            kind: "multi_select".to_string(),
            multi_select: vec![select_option("backend-id", "Backend")],
            ..Default::default()
        },
    );
    row.properties.insert(
        "Done".to_string(),
        PagePropertyDto {
            kind: "checkbox".to_string(),
            checkbox: Some(false),
            ..Default::default()
        },
    );
    row.properties.insert(
        "Due".to_string(),
        PagePropertyDto {
            kind: "date".to_string(),
            date: Some(DateMentionDto {
                start: "2026-06-10".to_string(),
                end: None,
                time_zone: None,
            }),
            ..Default::default()
        },
    );
    let bundle = locality_notion::dto::NotionPageBundle {
        page: row,
        blocks: Vec::new(),
    };

    let rendered = locality_notion::render::render_page_bundle(&bundle).expect("render");

    assert!(
        rendered
            .document
            .frontmatter
            .contains("title: \"Fix login bug\"")
    );
    assert!(
        rendered
            .document
            .frontmatter
            .contains("\"Status\": \"In progress\"")
    );
    assert!(rendered.document.frontmatter.contains("\"Points\": 3"));
    assert!(rendered.document.frontmatter.contains("\"Done\": false"));
    assert!(
        rendered
            .document
            .frontmatter
            .contains("\"Due\": \"2026-06-10\"")
    );
    assert!(
        rendered
            .document
            .frontmatter
            .contains("\"Tags\":\n  - \"Backend\"")
    );
}

#[test]
fn render_all_supported_page_property_values_as_frontmatter() {
    let mut row = page("row-1", "Property Coverage");
    let mut rich_property = rich_text("Notes");
    rich_property.annotations = RichTextAnnotationsDto {
        bold: true,
        ..Default::default()
    };
    row.properties.extend(BTreeMap::from([
        (
            "Rich Text".to_string(),
            PagePropertyDto {
                kind: "rich_text".to_string(),
                rich_text: vec![
                    rich_property,
                    rich_text(" and "),
                    linked_text("docs", "https://example.com/docs"),
                ],
                ..Default::default()
            },
        ),
        (
            "Number".to_string(),
            PagePropertyDto {
                kind: "number".to_string(),
                number: Some(serde_json::Number::from(7)),
                ..Default::default()
            },
        ),
        (
            "Select".to_string(),
            PagePropertyDto {
                kind: "select".to_string(),
                select: Some(select_option("select-id", "Selected")),
                ..Default::default()
            },
        ),
        (
            "Multi Select".to_string(),
            PagePropertyDto {
                kind: "multi_select".to_string(),
                multi_select: vec![
                    select_option("alpha-id", "Alpha"),
                    select_option("beta-id", "Beta"),
                ],
                ..Default::default()
            },
        ),
        (
            "Status".to_string(),
            PagePropertyDto {
                kind: "status".to_string(),
                status: Some(select_option("status-id", "In progress")),
                ..Default::default()
            },
        ),
        (
            "Checkbox".to_string(),
            PagePropertyDto {
                kind: "checkbox".to_string(),
                checkbox: Some(true),
                ..Default::default()
            },
        ),
        (
            "Date Range".to_string(),
            PagePropertyDto {
                kind: "date".to_string(),
                date: Some(DateMentionDto {
                    start: "2026-06-10".to_string(),
                    end: Some("2026-06-11".to_string()),
                    time_zone: Some("America/Los_Angeles".to_string()),
                }),
                ..Default::default()
            },
        ),
        (
            "URL".to_string(),
            PagePropertyDto {
                kind: "url".to_string(),
                url: Some("https://example.com".to_string()),
                ..Default::default()
            },
        ),
        (
            "Email".to_string(),
            PagePropertyDto {
                kind: "email".to_string(),
                email: Some("locality@example.com".to_string()),
                ..Default::default()
            },
        ),
        (
            "Phone".to_string(),
            PagePropertyDto {
                kind: "phone_number".to_string(),
                phone_number: Some("+1 415 555 0100".to_string()),
                ..Default::default()
            },
        ),
        (
            "Files".to_string(),
            PagePropertyDto {
                kind: "files".to_string(),
                files: vec![
                    FilePropertyDto {
                        name: Some("Spec".to_string()),
                        kind: "external".to_string(),
                        external: Some(ExternalFileDto {
                            url: "https://example.com/spec.pdf".to_string(),
                        }),
                        file: None,
                    },
                    FilePropertyDto {
                        name: None,
                        kind: "file".to_string(),
                        external: None,
                        file: Some(HostedFileDto {
                            url: "https://example.com/hosted.png".to_string(),
                            expiry_time: Some("2026-06-10T00:00:00.000Z".to_string()),
                        }),
                    },
                ],
                ..Default::default()
            },
        ),
        (
            "People".to_string(),
            PagePropertyDto {
                kind: "people".to_string(),
                people: vec![user("user-1", Some("Ada"))],
                ..Default::default()
            },
        ),
        (
            "Relation".to_string(),
            PagePropertyDto {
                kind: "relation".to_string(),
                relation: vec![IdRefDto {
                    id: "related-page-1".to_string(),
                }],
                ..Default::default()
            },
        ),
        (
            "Created Time".to_string(),
            PagePropertyDto {
                kind: "created_time".to_string(),
                created_time: Some("2026-06-10T00:00:00.000Z".to_string()),
                ..Default::default()
            },
        ),
        (
            "Last Edited Time".to_string(),
            PagePropertyDto {
                kind: "last_edited_time".to_string(),
                last_edited_time: Some("2026-06-11T00:00:00.000Z".to_string()),
                ..Default::default()
            },
        ),
        (
            "Created By".to_string(),
            PagePropertyDto {
                kind: "created_by".to_string(),
                created_by: Some(user("creator-1", Some("Creator"))),
                ..Default::default()
            },
        ),
        (
            "Last Edited By".to_string(),
            PagePropertyDto {
                kind: "last_edited_by".to_string(),
                last_edited_by: Some(user("editor-1", Some("Editor"))),
                ..Default::default()
            },
        ),
        (
            "Formula String".to_string(),
            PagePropertyDto {
                kind: "formula".to_string(),
                formula: Some(json!({ "type": "string", "string": "computed" })),
                ..Default::default()
            },
        ),
        (
            "Formula Number".to_string(),
            PagePropertyDto {
                kind: "formula".to_string(),
                formula: Some(json!({ "type": "number", "number": 9 })),
                ..Default::default()
            },
        ),
        (
            "Formula Boolean".to_string(),
            PagePropertyDto {
                kind: "formula".to_string(),
                formula: Some(json!({ "type": "boolean", "boolean": true })),
                ..Default::default()
            },
        ),
        (
            "Formula Date".to_string(),
            PagePropertyDto {
                kind: "formula".to_string(),
                formula: Some(json!({
                    "type": "date",
                    "date": { "start": "2026-06-12" }
                })),
                ..Default::default()
            },
        ),
        (
            "Rollup Number".to_string(),
            PagePropertyDto {
                kind: "rollup".to_string(),
                rollup: Some(json!({ "type": "number", "number": 11 })),
                ..Default::default()
            },
        ),
        (
            "Rollup Array".to_string(),
            PagePropertyDto {
                kind: "rollup".to_string(),
                rollup: Some(json!({
                    "type": "array",
                    "array": [
                        { "type": "title", "title": [{ "type": "text", "plain_text": "Item" }] }
                    ]
                })),
                ..Default::default()
            },
        ),
        (
            "Unique ID".to_string(),
            PagePropertyDto {
                kind: "unique_id".to_string(),
                unique_id: Some(UniqueIdPropertyDto {
                    prefix: Some("Locality".to_string()),
                    number: Some(12),
                }),
                ..Default::default()
            },
        ),
        (
            "Verification".to_string(),
            PagePropertyDto {
                kind: "verification".to_string(),
                verification: Some(VerificationPropertyDto {
                    state: Some("verified".to_string()),
                    verified_by: Some(user("verifier-1", Some("Verifier"))),
                    date: Some(DateMentionDto {
                        start: "2026-06-10".to_string(),
                        end: None,
                        time_zone: None,
                    }),
                }),
                ..Default::default()
            },
        ),
    ]));
    let bundle = locality_notion::dto::NotionPageBundle {
        page: row,
        blocks: Vec::new(),
    };

    let rendered = locality_notion::render::render_page_bundle(&bundle).expect("render");
    let frontmatter = &rendered.document.frontmatter;

    for expected in [
        "\"Rich Text\": \"**Notes** and [docs](https://example.com/docs)\"",
        "\"Number\": 7",
        "\"Select\": \"Selected\"",
        "\"Multi Select\":\n  - \"Alpha\"\n  - \"Beta\"",
        "\"Status\": \"In progress\"",
        "\"Checkbox\": true",
        "\"Date Range\":\n  \"start\": \"2026-06-10\"\n  \"end\": \"2026-06-11\"\n  \"time_zone\": \"America/Los_Angeles\"",
        "\"URL\": \"https://example.com\"",
        "\"Email\": \"locality@example.com\"",
        "\"Phone\": \"+1 415 555 0100\"",
        "\"Files\":\n  - \"Spec <https://example.com/spec.pdf>\"\n  - \"https://example.com/hosted.png\"",
        "\"People\":\n  - \"Ada <user-1>\"",
        "\"Relation\":\n  - \"related-page-1\"",
    ] {
        assert!(
            frontmatter.contains(expected),
            "missing frontmatter coverage: {expected}\n{frontmatter}"
        );
    }

    for omitted in [
        "\"Created Time\"",
        "\"Last Edited Time\"",
        "\"Created By\"",
        "\"Last Edited By\"",
        "\"Formula String\"",
        "\"Formula Number\"",
        "\"Formula Boolean\"",
        "\"Formula Date\"",
        "\"Rollup Number\"",
        "\"Rollup Array\"",
        "\"Unique ID\"",
        "\"Verification\"",
    ] {
        assert!(
            !frontmatter.contains(omitted),
            "read-only/computed property should not be editable frontmatter: {omitted}\n{frontmatter}"
        );
    }
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

    assert_eq!(entries.len(), 4);
    assert_eq!(entries[0].path, Path::new("Roadmap/page.md"));
    assert_eq!(entries[0].kind, EntityKind::Page);
    assert_eq!(entries[1].path, Path::new("Roadmap/Design Notes/page.md"));
    assert_eq!(entries[1].kind, EntityKind::Page);
    assert_eq!(entries[2].path, Path::new("Roadmap/Tasks"));
    assert_eq!(entries[2].kind, EntityKind::Database);
    assert_eq!(
        entries[3].path,
        Path::new("Roadmap/Tasks/Fix login bug/page.md")
    );
    assert_eq!(entries[3].kind, EntityKind::Page);
}

#[test]
fn portable_bootstrap_resumes_and_completes_database_coverage() {
    let root_page_id = RemoteId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let connector = NotionConnector::with_api(
        NotionConfig::default().with_root_page_id(root_page_id.clone()),
        Arc::new(NoSearchNotionApi(FixtureNotionApi::tree(
            root_page_id.as_str(),
        ))),
    );

    let first = connector
        .bootstrap_portable(PortableBootstrapRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            scope: PortableSourceScope::explicit_roots([root_page_id.clone()]),
            checkpoint: None,
            max_changes: 2,
        })
        .expect("first checkpoint");
    assert_eq!(first.changes.len(), 2);
    assert!(!first.completeness.is_complete());
    assert!(
        first
            .completeness
            .incomplete_reasons()
            .contains(&PortableIncompleteReason::CheckpointContinuation)
    );

    let second = connector
        .bootstrap_portable(PortableBootstrapRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            scope: PortableSourceScope::explicit_roots([root_page_id]),
            checkpoint: Some(first.next_checkpoint),
            max_changes: 2,
        })
        .expect("resumed checkpoint");
    assert_eq!(second.changes.len(), 2);
    assert!(second.completeness.is_complete());
    assert!(
        !second
            .completeness
            .incomplete_reasons()
            .contains(&PortableIncompleteReason::CheckpointContinuation)
    );
    assert!(second.completeness.incomplete_reasons().is_empty());

    let synchronized = connector
        .sync_portable(PortableSyncRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            scope: PortableSourceScope::explicit_roots([RemoteId::new(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )]),
            checkpoint: second.next_checkpoint,
            hints: Vec::new(),
            max_changes: 10,
        })
        .expect("scheduled explicit-root synchronization");
    assert_eq!(synchronized.changes.len(), 4);
    assert!(synchronized.completeness.is_complete());
}

#[test]
fn portable_bootstrap_requires_the_configured_explicit_root_without_search_fallback() {
    let root_page_id = RemoteId::new("page-1");
    let connector = NotionConnector::with_api(
        NotionConfig::default().with_root_page_id(root_page_id.clone()),
        Arc::new(NoSearchNotionApi(FixtureNotionApi::new())),
    );

    let batch = connector
        .bootstrap_portable(PortableBootstrapRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            scope: PortableSourceScope::explicit_roots([root_page_id]),
            checkpoint: None,
            max_changes: 100,
        })
        .expect("explicit-root bootstrap");
    let direct = connector
        .enumerate(EnumerateRequest {
            mount_id: MountId::new("notion-main"),
            cursor: None,
        })
        .expect("direct explicit-root enumeration");
    assert!(batch.completeness.is_complete());
    assert_eq!(batch.changes.len(), 1);
    let direct_logical_path = direct[0]
        .path
        .iter()
        .map(|component| component.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    assert_eq!(
        batch.changes[0]
            .logical_path
            .as_ref()
            .expect("portable path")
            .as_str(),
        direct_logical_path
    );

    let workspace_connector = NotionConnector::with_api(
        NotionConfig::default(),
        Arc::new(NoSearchNotionApi(FixtureNotionApi::new())),
    );
    let error = workspace_connector
        .bootstrap_portable(PortableBootstrapRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            scope: PortableSourceScope::explicit_roots([RemoteId::new("page-1")]),
            checkpoint: None,
            max_changes: 100,
        })
        .expect_err("workspace search must not be a portable fallback");
    assert!(error.to_string().contains("configured root page"));
}

#[test]
fn explicit_multi_root_paths_match_workspace_projection_and_are_order_invariant() {
    let first_root = RemoteId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let second_root = RemoteId::new("ffffffffffffffffffffffffffffffff");
    let workspace = NotionConnector::with_api(
        NotionConfig::default(),
        Arc::new(FixtureNotionApi::multi_root_workspace()),
    );
    let workspace_entries = workspace
        .enumerate(EnumerateRequest {
            mount_id: MountId::new("notion-main"),
            cursor: None,
        })
        .expect("workspace enumerate");

    let explicit = NotionConnector::with_api(
        NotionConfig::default(),
        Arc::new(NoSearchNotionApi(FixtureNotionApi::multi_root_workspace())),
    )
    .with_root_ids([second_root.clone(), first_root.clone()]);
    let explicit_entries = explicit
        .enumerate(EnumerateRequest {
            mount_id: MountId::new("notion-main"),
            cursor: None,
        })
        .expect("explicit enumerate");
    let paths = |entries: Vec<locality_core::model::TreeEntry>| {
        entries
            .into_iter()
            .map(|entry| (entry.remote_id, entry.path))
            .collect::<BTreeMap<_, _>>()
    };
    assert_eq!(paths(explicit_entries), paths(workspace_entries));

    let request = |roots| PortableBootstrapRequest {
        source_connection_id: SourceConnectionId::new("source-notion"),
        scope: PortableSourceScope::explicit_roots(roots),
        checkpoint: None,
        max_changes: 2,
    };
    let first = explicit
        .bootstrap_portable(request([first_root.clone(), second_root.clone()]))
        .expect("multi-root bootstrap");
    let reversed = NotionConnector::with_api(
        NotionConfig::default(),
        Arc::new(NoSearchNotionApi(FixtureNotionApi::multi_root_workspace())),
    )
    .with_root_ids([first_root.clone(), second_root.clone()])
    .bootstrap_portable(request([second_root, first_root]))
    .expect("reversed multi-root bootstrap");

    assert_eq!(first, reversed);
    assert_eq!(first.next_checkpoint.format_version, 2);
    assert_eq!(first.changes.len(), 2, "max_changes is aggregate");
    assert!(first.changes.iter().all(|change| {
        change.source_object.edges.len() == 1
            && change.source_object.edges[0].relationship == PORTABLE_SCOPE_ROOT_RELATIONSHIP
    }));
    assert_eq!(
        first
            .changes
            .iter()
            .map(|change| (
                change.source_object.remote_id.as_str(),
                change
                    .source_object
                    .edges
                    .first()
                    .expect("owning-root edge")
                    .target_remote_id
                    .as_str(),
                change.logical_path.as_ref().expect("logical path").as_str(),
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "Roadmap aaaaaa/page.md",
            ),
            (
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "Roadmap aaaaaa/Notes/page.md",
            ),
        ]
    );
    let checkpoint: serde_json::Value =
        serde_json::from_str(&first.next_checkpoint.opaque).expect("v2 checkpoint json");
    assert_eq!(checkpoint["component_version"], 2);
    assert_eq!(
        checkpoint["root_remote_ids"],
        json!([
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "ffffffffffffffffffffffffffffffff"
        ])
    );

    let one_root = NotionConnector::with_api(
        NotionConfig::default(),
        Arc::new(NoSearchNotionApi(FixtureNotionApi::multi_root_workspace())),
    )
    .with_root_ids([RemoteId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")]);
    let mismatch = one_root
        .bootstrap_portable(PortableBootstrapRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            scope: PortableSourceScope::explicit_roots([RemoteId::new(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )]),
            checkpoint: Some(first.next_checkpoint.clone()),
            max_changes: 2,
        })
        .expect_err("checkpoint root set must match exactly");
    assert!(mismatch.to_string().contains("does not match"));

    let mut newer_checkpoint = first.next_checkpoint;
    let mut newer_opaque: serde_json::Value =
        serde_json::from_str(&newer_checkpoint.opaque).expect("v2 checkpoint json");
    newer_opaque["component_version"] = json!(3);
    newer_checkpoint.opaque = serde_json::to_string(&newer_opaque).expect("checkpoint json");
    let newer_error = explicit
        .bootstrap_portable(PortableBootstrapRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            scope: PortableSourceScope::explicit_roots([
                RemoteId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                RemoteId::new("ffffffffffffffffffffffffffffffff"),
            ]),
            checkpoint: Some(newer_checkpoint),
            max_changes: 2,
        })
        .expect_err("newer component version must fail cleanly");
    assert!(newer_error.to_string().contains("requires an update"));
}

#[test]
fn explicit_multi_root_rejects_overlap_duplicates_empty_and_scope_mismatch() {
    let first_root = RemoteId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let nested_root = RemoteId::new("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    let overlapping = NotionConnector::with_api(
        NotionConfig::default(),
        Arc::new(NoSearchNotionApi(FixtureNotionApi::multi_root_workspace())),
    )
    .with_root_ids([first_root.clone(), nested_root]);
    let overlap_error = overlapping
        .enumerate(EnumerateRequest {
            mount_id: MountId::new("notion-main"),
            cursor: None,
        })
        .expect_err("nested roots must be rejected");
    assert!(overlap_error.to_string().contains("overlap"));

    let duplicate = NotionConnector::with_api(
        NotionConfig::default(),
        Arc::new(NoSearchNotionApi(FixtureNotionApi::new())),
    )
    .with_root_ids([RemoteId::new("page-1"), RemoteId::new("PAGE1")]);
    assert!(
        duplicate
            .enumerate(EnumerateRequest {
                mount_id: MountId::new("notion-main"),
                cursor: None,
            })
            .expect_err("duplicates must fail")
            .to_string()
            .contains("duplicate")
    );

    let empty = NotionConnector::with_api(
        NotionConfig::default(),
        Arc::new(NoSearchNotionApi(FixtureNotionApi::new())),
    )
    .with_root_ids([]);
    assert!(
        empty
            .enumerate(EnumerateRequest {
                mount_id: MountId::new("notion-main"),
                cursor: None,
            })
            .expect_err("empty set must fail")
            .to_string()
            .contains("must not be empty")
    );

    let mismatch = NotionConnector::with_api(
        NotionConfig::default(),
        Arc::new(NoSearchNotionApi(FixtureNotionApi::multi_root_workspace())),
    )
    .with_root_ids([first_root]);
    assert!(
        mismatch
            .bootstrap_portable(PortableBootstrapRequest {
                source_connection_id: SourceConnectionId::new("source-notion"),
                scope: PortableSourceScope::explicit_roots([RemoteId::new(
                    "ffffffffffffffffffffffffffffffff",
                )]),
                checkpoint: None,
                max_changes: 10,
            })
            .expect_err("scope mismatch must fail")
            .to_string()
            .contains("exactly match")
    );

    let sixteen_ids = (0..16)
        .map(|index| RemoteId::new(format!("root-{index:02}")))
        .collect::<Vec<_>>();
    let sixteen = NotionConnector::with_api(
        NotionConfig::default(),
        Arc::new(NoSearchNotionApi(FixtureNotionApi::many_roots(16))),
    )
    .with_root_ids(sixteen_ids.clone());
    assert_eq!(
        sixteen
            .enumerate(EnumerateRequest {
                mount_id: MountId::new("notion-main"),
                cursor: None,
            })
            .expect("sixteen roots are supported")
            .len(),
        16
    );
    let seventeen =
        sixteen.with_root_ids(sixteen_ids.into_iter().chain([RemoteId::new("root-16")]));
    assert_eq!(
        seventeen
            .enumerate(EnumerateRequest {
                mount_id: MountId::new("notion-main"),
                cursor: None,
            })
            .expect_err("seventeen roots exceed the bound")
            .to_string(),
        "invalid state: Notion explicit root set exceeds the limit of 16"
    );
}

#[test]
fn explicit_database_root_matches_workspace_projection_without_search() {
    let database_id = RemoteId::new("root-db");
    let workspace = NotionConnector::with_api(
        NotionConfig::default(),
        Arc::new(FixtureNotionApi::workspace()),
    );
    let workspace_database = workspace
        .enumerate(EnumerateRequest {
            mount_id: MountId::new("notion-main"),
            cursor: None,
        })
        .expect("workspace enumerate")
        .into_iter()
        .find(|entry| entry.remote_id == database_id)
        .expect("workspace database root");

    let explicit = NotionConnector::with_api(
        NotionConfig::default(),
        Arc::new(NoSearchNotionApi(FixtureNotionApi::workspace())),
    )
    .with_root_ids([database_id.clone()]);
    let entries = explicit
        .enumerate(EnumerateRequest {
            mount_id: MountId::new("notion-main"),
            cursor: None,
        })
        .expect("explicit database enumerate");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, EntityKind::Database);
    assert_eq!(entries[0].path, workspace_database.path);
    assert_eq!(entries[0].path, Path::new("Tasks"));

    let batch = explicit
        .bootstrap_portable(PortableBootstrapRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            scope: PortableSourceScope::explicit_roots([database_id.clone()]),
            checkpoint: None,
            max_changes: 10,
        })
        .expect("explicit database bootstrap");
    assert_eq!(batch.changes.len(), 1);
    assert_eq!(batch.changes[0].source_object.remote_id, database_id);
    assert_eq!(
        batch.changes[0].source_object.edges,
        vec![SourceEdge {
            relationship: PORTABLE_SCOPE_ROOT_RELATIONSHIP.to_string(),
            target_remote_id: RemoteId::new("rootdb"),
        }]
    );
    assert!(batch.changes[0].requires_fetch);
    assert_eq!(
        batch.changes[0]
            .logical_path
            .as_ref()
            .expect("database schema path")
            .as_str(),
        "Tasks/_schema.yaml"
    );
    assert!(batch.completeness.is_complete());
}

#[test]
fn explicit_root_page_non_not_found_error_does_not_fall_back_to_database() {
    let connector =
        NotionConnector::with_api(NotionConfig::default(), Arc::new(NonNotFoundPageErrorApi))
            .with_root_ids([RemoteId::new("database-id")]);
    let error = connector
        .enumerate(EnumerateRequest {
            mount_id: MountId::new("notion-main"),
            cursor: None,
        })
        .expect_err("non-not-found page failure must be preserved");
    assert_eq!(
        error.to_string(),
        "invalid state: injected page retrieval failure"
    );
}

#[test]
fn legacy_single_root_keeps_v1_checkpoint_and_v2_set_mode_rejects_it() {
    let root = RemoteId::new("page-1");
    let legacy = NotionConnector::with_api(
        NotionConfig::default().with_root_page_id(root.clone()),
        Arc::new(NoSearchNotionApi(FixtureNotionApi::new())),
    );
    let legacy_batch = legacy
        .bootstrap_portable(PortableBootstrapRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            scope: PortableSourceScope::explicit_roots([root.clone()]),
            checkpoint: None,
            max_changes: 10,
        })
        .expect("legacy bootstrap");
    assert_eq!(legacy_batch.next_checkpoint.format_version, 1);
    assert_eq!(
        legacy_batch.next_checkpoint.opaque,
        "{\"operation\":\"bootstrap\",\"root_remote_id\":\"page-1\",\"inventory_sha256\":\"sha256:a870998d440a30dccfac26d79f3b2345e1bbec1aa1bcdb26e3682ac92680b3dc\",\"offset\":1,\"complete\":true}"
    );
    assert!(legacy_batch.changes[0].source_object.edges.is_empty());

    let set_mode = NotionConnector::with_api(
        NotionConfig::default(),
        Arc::new(NoSearchNotionApi(FixtureNotionApi::new())),
    )
    .with_root_page_ids([root.clone()]);
    let error = set_mode
        .bootstrap_portable(PortableBootstrapRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            scope: PortableSourceScope::explicit_roots([root]),
            checkpoint: Some(legacy_batch.next_checkpoint),
            max_changes: 10,
        })
        .expect_err("set mode must not accept legacy checkpoints");
    assert!(error.to_string().contains("does not match"));
}

#[test]
fn portable_render_matches_direct_canonical_bytes_and_keys_survive_rename() {
    let root_page_id = RemoteId::new("page-1");
    let connector = NotionConnector::with_api(
        NotionConfig::default().with_root_page_id(root_page_id.clone()),
        Arc::new(NoSearchNotionApi(FixtureNotionApi::new())),
    );
    let fetched = connector
        .fetch_portable(PortableFetchRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            remote_id: root_page_id,
            reason: PortableFetchReason::Bootstrap,
        })
        .expect("portable fetch");
    let direct = connector.render(&fetched.native).expect("direct render");

    let before = connector
        .render_portable(&PortableRenderRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            logical_path: LogicalPath::new("Roadmap/page.md").expect("path"),
            native: fetched.native.clone(),
            format_version: 1,
        })
        .expect("portable render");
    let after = connector
        .render_portable(&PortableRenderRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            logical_path: LogicalPath::new("Renamed Roadmap/page.md").expect("path"),
            native: fetched.native,
            format_version: 1,
        })
        .expect("portable renamed render");

    assert_eq!(
        before.canonical.body,
        render_canonical_markdown(&direct).into_bytes()
    );
    assert_eq!(before.canonical.artifact_key, after.canonical.artifact_key);
    assert_eq!(
        before.projections[0].artifact.artifact_key,
        after.projections[0].artifact.artifact_key
    );
    assert_eq!(
        before.projections[0].artifact.body,
        after.projections[0].artifact.body
    );
    assert_ne!(
        before.projections[0].logical_path,
        after.projections[0].logical_path
    );
}

#[test]
fn portable_database_root_fetches_and_renders_exact_shared_schema_projection() {
    let root_page_id = RemoteId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let compact_database_id = "cccccccccccccccccccccccccccccccc";
    let provider_database_id = "CCCCCCCC-CCCC-CCCC-CCCC-CCCCCCCCCCCC";
    let compact_data_source_id = "dddddddddddddddddddddddddddddddd";
    let provider_data_source_id = "DDDDDDDD-DDDD-DDDD-DDDD-DDDDDDDDDDDD";
    let mut api = FixtureNotionApi::tree(root_page_id.as_str());
    let mut database = api
        .databases
        .get(compact_database_id)
        .cloned()
        .expect("database fixture");
    database.id = provider_database_id.to_string();
    api.databases
        .insert(compact_database_id.to_string(), database.clone());
    api.databases
        .insert(provider_database_id.to_string(), database.clone());
    let data_source = api
        .data_sources
        .get_mut(compact_data_source_id)
        .expect("data-source fixture");
    data_source.id = provider_data_source_id.to_string();
    data_source.parent = Some(ParentDto {
        kind: "database_id".to_string(),
        database_id: Some(provider_database_id.to_string()),
        ..Default::default()
    });
    let expected_bundle = NotionDatabaseBundle {
        database: database.clone(),
        data_sources: vec![data_source.clone()],
    };
    let connector = NotionConnector::with_api(
        NotionConfig::default().with_root_page_id(root_page_id.clone()),
        Arc::new(NoSearchNotionApi(api)),
    );

    let direct_entries = connector
        .enumerate(EnumerateRequest {
            mount_id: MountId::new("notion-main"),
            cursor: None,
        })
        .expect("direct enumerate");
    let direct_database = direct_entries
        .iter()
        .find(|entry| entry.kind == EntityKind::Database)
        .expect("direct database");
    let direct_row = direct_entries
        .iter()
        .find(|entry| entry.remote_id == RemoteId::new("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"))
        .expect("direct row");

    let batch = connector
        .bootstrap_portable(PortableBootstrapRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            scope: PortableSourceScope::explicit_roots([root_page_id]),
            checkpoint: None,
            max_changes: 10,
        })
        .expect("portable bootstrap");
    assert!(batch.completeness.is_complete());
    let database_change = batch
        .changes
        .iter()
        .find(|change| change.source_object.kind == EntityKind::Database)
        .expect("portable database change");
    let row_change = batch
        .changes
        .iter()
        .find(|change| {
            change.source_object.remote_id == RemoteId::new("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee")
        })
        .expect("portable row change");
    let portable_path = |path: &Path| {
        path.iter()
            .map(|component| component.to_str().expect("UTF-8 direct path component"))
            .collect::<Vec<_>>()
            .join("/")
    };
    assert!(database_change.requires_fetch);
    assert_eq!(
        database_change
            .logical_path
            .as_ref()
            .expect("database schema path")
            .as_str(),
        format!("{}/_schema.yaml", portable_path(&direct_database.path))
    );
    assert_eq!(
        row_change.logical_path.as_ref().expect("row path").as_str(),
        portable_path(&direct_row.path)
    );

    let fetched = connector
        .fetch_portable(PortableFetchRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            remote_id: database_change.source_object.remote_id.clone(),
            reason: PortableFetchReason::Bootstrap,
        })
        .expect("portable database fetch");
    assert_eq!(fetched.native.kind, "notion_database");
    assert_eq!(
        fetched.native.remote_id,
        RemoteId::new(provider_database_id)
    );
    assert_eq!(
        fetched.native.raw,
        serde_json::to_vec(&expected_bundle).expect("exact native fixture")
    );
    assert_eq!(
        fetched.provider_version.as_deref(),
        Some(concat!(
            "{\"format_version\":1,",
            "\"database\":{\"id\":\"cccccccccccccccccccccccccccccccc\",",
            "\"last_edited_time\":\"2026-06-10T01:00:00.000Z\"},",
            "\"data_sources\":[{",
            "\"id\":\"dddddddddddddddddddddddddddddddd\",",
            "\"last_edited_time\":\"2026-06-10T01:01:00.000Z\"}]}"
        ))
    );

    let rendered = connector
        .render_portable(&PortableRenderRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            logical_path: database_change
                .logical_path
                .clone()
                .expect("database schema path"),
            native: fetched.native.clone(),
            format_version: 1,
        })
        .expect("portable database render");
    let exact_schema = concat!(
        "loc:\n",
        "  type: notion_database_schema\n",
        "  database_id: \"CCCCCCCC-CCCC-CCCC-CCCC-CCCCCCCCCCCC\"\n",
        "title: \"Tasks\"\n",
        "data_sources:\n",
        "  - id: \"DDDDDDDD-DDDD-DDDD-DDDD-DDDDDDDDDDDD\"\n",
        "    name: \"Tasks\"\n",
        "    properties:\n",
        "      \"Name\":\n",
        "        id: \"title\"\n",
        "        type: \"title\"\n",
        "      \"Status\":\n",
        "        id: \"status-id\"\n",
        "        type: \"select\"\n",
        "        options:\n",
        "          - name: \"Todo\"\n",
        "            id: \"todo-id\"\n",
    );
    assert_eq!(rendered.canonical.body, exact_schema.as_bytes());
    assert_eq!(
        rendered.projections[0].artifact.body,
        exact_schema.as_bytes()
    );
    assert_eq!(
        connector
            .database_schema_yaml(&RemoteId::new(provider_database_id))
            .expect("direct schema"),
        exact_schema
    );
    assert_eq!(
        rendered.canonical.artifact_key.as_str(),
        "notion:database:cccccccccccccccccccccccccccccccc:canonical_schema:v1"
    );
    assert_eq!(
        rendered.projections[0].artifact.artifact_key.as_str(),
        "notion:database:cccccccccccccccccccccccccccccccc:database_schema:v1"
    );
    assert_eq!(
        rendered.canonical.media_type,
        "application/yaml; charset=utf-8"
    );
    assert_eq!(rendered.projections[0].file_kind, ProjectionFileKind::Yaml);
    assert_eq!(rendered.projections[0].supported_actions.len(), 2);
    assert!(
        rendered.projections[0]
            .supported_actions
            .contains(&SourceAction::Read)
    );
    assert!(
        rendered.projections[0]
            .supported_actions
            .contains(&SourceAction::Search)
    );
    assert!(rendered.completeness.is_complete());

    let renamed = connector
        .render_portable(&PortableRenderRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            logical_path: LogicalPath::new("Renamed Tasks/_schema.yaml").expect("renamed path"),
            native: fetched.native,
            format_version: 1,
        })
        .expect("renamed database render");
    assert_eq!(
        rendered.canonical.artifact_key,
        renamed.canonical.artifact_key
    );
    assert_eq!(
        rendered.projections[0].artifact.artifact_key,
        renamed.projections[0].artifact.artifact_key
    );
    assert_ne!(
        rendered.projections[0].logical_path,
        renamed.projections[0].logical_path
    );
}

#[test]
fn portable_database_fetch_and_render_fail_closed_on_identity_and_ownership_mismatch() {
    let database_id = "cccccccccccccccccccccccccccccccc";
    let data_source_id = "dddddddddddddddddddddddddddddddd";
    let mut mismatched_api = FixtureNotionApi::tree("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    mismatched_api
        .databases
        .get_mut(database_id)
        .expect("database")
        .id = "ffffffffffffffffffffffffffffffff".to_string();
    let mismatched_connector = NotionConnector::with_api(
        NotionConfig::default(),
        Arc::new(NoSearchNotionApi(mismatched_api)),
    );
    let returned_id_error = mismatched_connector
        .fetch_portable(PortableFetchRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            remote_id: RemoteId::new(database_id),
            reason: PortableFetchReason::Bootstrap,
        })
        .expect_err("different returned database ID");
    assert_eq!(
        returned_id_error.to_string(),
        concat!(
            "invalid state: Notion database bundle returned database ",
            "`ffffffffffffffffffffffffffffffff` for requested database ",
            "`cccccccccccccccccccccccccccccccc`"
        )
    );

    let mut api = FixtureNotionApi::tree("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    api.data_sources
        .get_mut(data_source_id)
        .expect("data source")
        .parent = Some(ParentDto {
        kind: "database_id".to_string(),
        database_id: Some("ffffffffffffffffffffffffffffffff".to_string()),
        ..Default::default()
    });
    let connector =
        NotionConnector::with_api(NotionConfig::default(), Arc::new(NoSearchNotionApi(api)));

    let ownership_error = connector
        .fetch_portable(PortableFetchRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            remote_id: RemoteId::new(database_id),
            reason: PortableFetchReason::Bootstrap,
        })
        .expect_err("foreign data-source owner");
    assert_eq!(
        ownership_error.to_string(),
        concat!(
            "invalid state: Notion data source `dddddddddddddddddddddddddddddddd` ",
            "belongs to database `ffffffffffffffffffffffffffffffff`, ",
            "not `cccccccccccccccccccccccccccccccc`"
        )
    );

    let valid_api = FixtureNotionApi::tree("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let valid_connector = NotionConnector::with_api(
        NotionConfig::default(),
        Arc::new(NoSearchNotionApi(valid_api)),
    );
    let fetched = valid_connector
        .fetch_portable(PortableFetchRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            remote_id: RemoteId::new(database_id),
            reason: PortableFetchReason::Bootstrap,
        })
        .expect("valid database fetch");
    let identity_error = valid_connector
        .render_portable(&PortableRenderRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            logical_path: LogicalPath::new("Tasks/_schema.yaml").expect("path"),
            native: NativeEntity {
                remote_id: RemoteId::new("ffffffffffffffffffffffffffffffff"),
                ..fetched.native.clone()
            },
            format_version: 1,
        })
        .expect_err("mismatched native remote ID");
    assert_eq!(
        identity_error.to_string(),
        "invalid state: Notion portable database native payload does not match its remote ID"
    );

    let unsupported_kind = valid_connector
        .render_portable(&PortableRenderRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            logical_path: LogicalPath::new("Tasks/_schema.yaml").expect("path"),
            native: NativeEntity {
                remote_id: RemoteId::new(database_id),
                kind: "notion_database_view".to_string(),
                raw: Vec::new(),
            },
            format_version: 1,
        })
        .expect_err("unsupported native kind");
    assert_eq!(
        unsupported_kind.to_string(),
        "invalid state: Notion portable render received unsupported native kind `notion_database_view`"
    );

    let malformed = valid_connector
        .render_portable(&PortableRenderRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            logical_path: LogicalPath::new("Tasks/_schema.yaml").expect("path"),
            native: NativeEntity {
                remote_id: RemoteId::new(database_id),
                kind: "notion_database".to_string(),
                raw: b"{}".to_vec(),
            },
            format_version: 1,
        })
        .expect_err("malformed database bundle");
    assert!(
        malformed
            .to_string()
            .starts_with("io error: notion database native decode failed:")
    );
}

#[test]
fn portable_fetch_does_not_fall_back_to_database_after_non_not_found_page_error() {
    let connector =
        NotionConnector::with_api(NotionConfig::default(), Arc::new(NonNotFoundPageErrorApi));

    let error = connector
        .fetch_portable(PortableFetchRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            remote_id: RemoteId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            reason: PortableFetchReason::Bootstrap,
        })
        .expect_err("page error must not trigger database fallback");

    assert_eq!(
        error.to_string(),
        "invalid state: injected page retrieval failure"
    );
}

#[test]
fn portable_fetch_preserves_descendant_not_found_without_database_fallback() {
    let retrieve_page_calls = Arc::new(AtomicUsize::new(0));
    let connector = NotionConnector::with_api(
        NotionConfig::default(),
        Arc::new(DescendantNotFoundApi {
            retrieve_page_calls: Arc::clone(&retrieve_page_calls),
        }),
    );

    let error = connector
        .fetch_portable(PortableFetchRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            remote_id: RemoteId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            reason: PortableFetchReason::Bootstrap,
        })
        .expect_err("descendant block lookup must fail as a page fetch");

    assert_eq!(
        error,
        locality_core::LocalityError::RemoteNotFound(
            "missing descendant block `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb`".to_string()
        )
    );
    assert_eq!(retrieve_page_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn portable_render_marks_media_incomplete_instead_of_silently_omitting_it() {
    let bundle = locality_notion::dto::NotionPageBundle {
        page: page("page-media", "Media"),
        blocks: vec![BlockTreeDto {
            block: file_block("image-1", "image", "https://example.com/image.png", "Image"),
            children: Vec::new(),
        }],
    };
    let connector = NotionConnector::with_api(
        NotionConfig::default().with_root_page_id(RemoteId::new("page-media")),
        Arc::new(NoSearchNotionApi(FixtureNotionApi::new())),
    );
    let rendered = connector
        .render_portable(&PortableRenderRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            logical_path: LogicalPath::new("Media/page.md").expect("path"),
            native: NativeEntity {
                remote_id: RemoteId::new("page-media"),
                kind: "notion_page".to_string(),
                raw: serde_json::to_vec(&bundle).expect("native fixture"),
            },
            format_version: 1,
        })
        .expect("render media page");

    assert!(!rendered.completeness.is_complete());
    assert!(rendered.completeness.incomplete_reasons().iter().any(
        |reason| matches!(reason, PortableIncompleteReason::UnsupportedArtifact {
            artifact_kind,
            ..
        } if artifact_kind == "notion_media")
    ));
}

#[test]
fn portable_hosted_media_capture_is_sanitized_local_and_exact() {
    let page_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let block_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let signed_url = concat!(
        "https://secure.notion-static.com/assets/Cover.PNG?",
        "X-Amz-Signature=signature-secret&token=token-secret&",
        "Authorization=Bearer%20authorization-secret#fragment-secret"
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let fetcher = Arc::new(FixturePortableMediaFetcher {
        outcomes: BTreeMap::from([(
            signed_url.to_string(),
            FixturePortableMediaOutcome::Success(PortableMediaCapture {
                bytes: vec![0x89, b'P', b'N', b'G'],
                media_type: "image/png; charset=binary".to_string(),
            }),
        )]),
        calls: Arc::clone(&calls),
    });
    let connector = portable_media_connector(
        page_id,
        vec![hosted_file_block(
            block_id,
            "image",
            signed_url,
            Some("2099-06-12T10:00:00.000Z"),
        )],
    )
    .with_portable_media_capture_fetcher(PortableMediaCapturePolicy::HostedPilot, fetcher.clone());

    assert_eq!(
        connector.portable_media_capture_policy(),
        PortableMediaCapturePolicy::HostedPilot
    );
    let fetcher_references = Arc::strong_count(&fetcher);
    let rooted = connector.with_root_page_id(RemoteId::new(page_id));
    assert_eq!(Arc::strong_count(&fetcher), fetcher_references + 1);
    assert_eq!(
        rooted.portable_media_capture_policy(),
        PortableMediaCapturePolicy::HostedPilot
    );
    drop(rooted);
    let deferred = connector.with_execution_policy(ConnectorExecutionPolicy::DeferProviderCooldown);
    assert_eq!(Arc::strong_count(&fetcher), fetcher_references + 1);
    assert_eq!(
        deferred.portable_media_capture_policy(),
        PortableMediaCapturePolicy::HostedPilot
    );
    drop(deferred);

    let fetched = connector
        .fetch_portable(portable_fetch_request(page_id))
        .expect("portable media fetch");
    assert!(fetched.completeness.is_complete());
    assert_eq!(fetched.native.kind, "notion_page_portable_media_v1");
    assert_eq!(
        calls.lock().expect("calls").as_slice(),
        [(signed_url.to_string(), PORTABLE_MEDIA_MAX_ASSET_BYTES)]
    );
    let native: locality_notion::dto::NotionPortablePageBundleV1 =
        serde_json::from_slice(&fetched.native.raw).expect("portable media native");
    assert_eq!(native.format_version, 1);
    assert_eq!(
        native.captured_media,
        vec![locality_notion::dto::NotionPortableCapturedMediaV1 {
            block_id: block_id.to_string(),
            kind: "image".to_string(),
            media_type: "image/png".to_string(),
            bytes: vec![0x89, b'P', b'N', b'G'],
        }]
    );
    assert!(native.incomplete_media.is_empty());
    let hosted = native.page.blocks[0]
        .block
        .image
        .as_ref()
        .and_then(|file| file.file.as_ref())
        .expect("sanitized hosted file");
    assert_eq!(
        hosted.url,
        "https://secure.notion-static.com/assets/Cover.PNG"
    );
    assert_eq!(hosted.expiry_time, None);
    let native_raw = String::from_utf8_lossy(&fetched.native.raw);
    assert!(native_raw.contains(r#""bytes":"iVBORw==""#));
    assert!(!native_raw.contains(signed_url));
    let desktop_paths = locality_notion::render::render_page_bundle_with_options(
        &native.page,
        &locality_notion::render::RenderOptions::with_page_path("Docs/Coverage/page.md"),
    )
    .expect("desktop path render");
    assert_eq!(
        desktop_paths.media_assets[0].local_path,
        Path::new(".loc/media/Docs/Coverage/image-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.png")
    );
    for forbidden in [
        "X-Amz-Signature",
        "signature-secret",
        "token-secret",
        "Authorization",
        "authorization-secret",
        "fragment-secret",
        "2099-06-12",
    ] {
        assert!(!native_raw.contains(forbidden));
        assert!(!format!("{fetched:?}").contains(forbidden));
    }

    let rendered = connector
        .render_portable(&PortableRenderRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            logical_path: LogicalPath::new("Docs/Coverage/page.md").expect("path"),
            native: fetched.native,
            format_version: 1,
        })
        .expect("portable media render");
    let exact_markdown = concat!(
        "---\n",
        "loc:\n",
        "  id: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
        "  type: page\n",
        "  synced_at: \"2026-06-10T00:00:00.000Z\"\n",
        "  remote_edited_at: \"2026-06-10T00:00:00.000Z\"\n",
        "title: \"Coverage\"\n",
        "---\n",
        "![Image](../../.loc/media/Docs/Coverage/image-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.png)\n",
    );
    assert_eq!(rendered.canonical.body, exact_markdown.as_bytes());
    assert_eq!(rendered.projections.len(), 2);
    assert_eq!(
        rendered.projections[0].artifact.body,
        exact_markdown.as_bytes()
    );
    let binary = &rendered.projections[1];
    assert_eq!(
        binary.artifact.artifact_key.as_str(),
        concat!(
            "notion:page:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:",
            "block:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb:media:v1"
        )
    );
    assert_eq!(binary.artifact.media_type, "image/png");
    assert_eq!(binary.artifact.body, vec![0x89, b'P', b'N', b'G']);
    assert_eq!(
        binary.logical_path.as_str(),
        ".loc/media/Docs/Coverage/image-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.png"
    );
    assert_eq!(binary.file_kind, ProjectionFileKind::Binary);
    assert_eq!(
        binary.supported_actions,
        [SourceAction::Read, SourceAction::DownloadAttachment]
            .into_iter()
            .collect()
    );
    assert!(rendered.completeness.is_complete());
    for projection in &rendered.projections {
        let body = String::from_utf8_lossy(&projection.artifact.body);
        assert!(!body.contains("X-Amz-Signature"));
        assert!(!body.contains("token-secret"));
        assert!(!body.contains("fragment-secret"));
    }
}

#[test]
fn portable_media_default_preserves_native_and_remains_incomplete() {
    let page_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let signed_url = "https://secure.notion-static.com/image.png?X-Amz-Signature=direct";
    let block = hosted_file_block(
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "image",
        signed_url,
        Some("2099-06-12T10:00:00.000Z"),
    );
    let connector = portable_media_connector(page_id, vec![block.clone()]);
    let direct = connector
        .fetch(FetchRequest {
            remote_id: RemoteId::new(page_id),
        })
        .expect("direct fetch");
    let fetched = connector
        .fetch_portable(portable_fetch_request(page_id))
        .expect("default portable fetch");
    assert_eq!(fetched.native.kind, "notion_page");
    assert_eq!(fetched.native.raw, direct.raw);
    assert!(String::from_utf8_lossy(&fetched.native.raw).contains(signed_url));

    let rendered = connector
        .render_portable(&PortableRenderRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            logical_path: LogicalPath::new("Coverage/page.md").expect("path"),
            native: fetched.native,
            format_version: 1,
        })
        .expect("default portable render");
    assert!(!rendered.completeness.is_complete());
    assert_eq!(rendered.projections.len(), 1);
}

#[test]
fn portable_capture_keeps_pages_without_media_byte_exact() {
    let page_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let blocks = vec![paragraph_block(
        "paragraph-1",
        vec![rich_text("No media here.")],
    )];
    let direct_connector = portable_media_connector(page_id, blocks.clone());
    let direct_native = direct_connector
        .fetch(FetchRequest {
            remote_id: RemoteId::new(page_id),
        })
        .expect("direct fetch");
    let direct_document = direct_connector
        .render(&direct_native)
        .expect("direct render");
    let connector = portable_media_connector(page_id, blocks)
        .with_portable_media_capture(PortableMediaCapturePolicy::HostedPilot);
    let fetched = connector
        .fetch_portable(portable_fetch_request(page_id))
        .expect("portable fetch");
    assert!(fetched.completeness.is_complete());
    let rendered = connector
        .render_portable(&PortableRenderRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            logical_path: LogicalPath::new("Coverage/page.md").expect("path"),
            native: fetched.native,
            format_version: 1,
        })
        .expect("portable render");
    let direct_bytes = render_canonical_markdown(&direct_document).into_bytes();
    assert_eq!(rendered.canonical.body, direct_bytes);
    assert_eq!(rendered.projections.len(), 1);
    assert_eq!(rendered.projections[0].artifact.body, direct_bytes);
    assert!(rendered.completeness.is_complete());
}

#[test]
fn portable_media_denials_are_incomplete_and_never_publish_remote_urls() {
    let denied = [
        ("external", "https://example.com/external.png"),
        ("http", "http://secure.notion-static.com/image.png"),
        ("ip", "https://127.0.0.1/image.png"),
        (
            "userinfo",
            "https://user:pass@secure.notion-static.com/image.png",
        ),
        ("bad_port", "https://secure.notion-static.com:444/image.png"),
        ("unlisted", "https://notion-static.com/image.png"),
        (
            "s3_prefix",
            "https://s3.us-west-2.amazonaws.com/other/image.png",
        ),
    ];
    for (index, (case, url)) in denied.into_iter().enumerate() {
        let page_id = format!("page-denied-{index}");
        let block_id = format!("block-denied-{index}");
        let block = if case == "external" {
            file_block(&block_id, "image", url, "Image")
        } else {
            hosted_file_block(&block_id, "image", url, None)
        };
        let connector = portable_media_connector(&page_id, vec![block])
            .with_portable_media_capture(PortableMediaCapturePolicy::HostedPilot);
        let fetched = connector
            .fetch_portable(portable_fetch_request(&page_id))
            .unwrap_or_else(|error| panic!("{case} fetch failed closed unexpectedly: {error}"));
        assert!(!fetched.completeness.is_complete(), "{case}");
        let raw = String::from_utf8_lossy(&fetched.native.raw);
        assert!(!raw.contains(url), "{case}: {raw}");
        let rendered = connector
            .render_portable(&PortableRenderRequest {
                source_connection_id: SourceConnectionId::new("source-notion"),
                logical_path: LogicalPath::new(format!("Denied {index}/page.md")).expect("path"),
                native: fetched.native,
                format_version: 1,
            })
            .unwrap_or_else(|error| panic!("{case} render: {error}"));
        assert!(!rendered.completeness.is_complete(), "{case}");
        assert_eq!(rendered.projections.len(), 1, "{case}");
        assert!(!String::from_utf8_lossy(&rendered.canonical.body).contains(url));
    }
}

#[test]
fn portable_media_expired_failed_and_oversized_captures_are_redacted() {
    let cases = [
        (
            "expired",
            Some("2000-01-01T00:00:00.000Z"),
            FixturePortableMediaOutcome::Success(PortableMediaCapture {
                bytes: b"unused".to_vec(),
                media_type: "image/png".to_string(),
            }),
        ),
        (
            "failed",
            Some("2099-01-01T00:00:00.000Z"),
            FixturePortableMediaOutcome::Failure(
                "X-Amz-Signature=must-not-escape token-secret".to_string(),
            ),
        ),
        (
            "oversized",
            Some("2099-01-01T00:00:00.000Z"),
            FixturePortableMediaOutcome::Success(PortableMediaCapture {
                bytes: vec![0; PORTABLE_MEDIA_MAX_ASSET_BYTES + 1],
                media_type: "image/png".to_string(),
            }),
        ),
    ];
    for (case, expiry, outcome) in cases {
        let page_id = format!("page-{case}");
        let block_id = format!("block-{case}");
        let url =
            format!("https://secure.notion-static.com/{case}.png?X-Amz-Signature=signature-secret");
        let fetcher = Arc::new(FixturePortableMediaFetcher {
            outcomes: BTreeMap::from([(url.clone(), outcome)]),
            calls: Arc::new(Mutex::new(Vec::new())),
        });
        let connector = portable_media_connector(
            &page_id,
            vec![hosted_file_block(&block_id, "image", &url, expiry)],
        )
        .with_portable_media_capture_fetcher(PortableMediaCapturePolicy::HostedPilot, fetcher);
        let fetched = connector
            .fetch_portable(portable_fetch_request(&page_id))
            .unwrap_or_else(|error| panic!("{case}: {error}"));
        assert!(!fetched.completeness.is_complete(), "{case}");
        let raw = String::from_utf8_lossy(&fetched.native.raw);
        assert!(!raw.contains("X-Amz-Signature"), "{case}");
        assert!(!raw.contains("signature-secret"), "{case}");
        assert!(!raw.contains("token-secret"), "{case}");
        let rendered = connector
            .render_portable(&PortableRenderRequest {
                source_connection_id: SourceConnectionId::new("source-notion"),
                logical_path: LogicalPath::new(format!("{case}/page.md")).expect("path"),
                native: fetched.native,
                format_version: 1,
            })
            .expect("render incomplete media");
        assert_eq!(rendered.projections.len(), 1, "{case}");
        assert!(!String::from_utf8_lossy(&rendered.canonical.body).contains("https://"));
    }
}

#[test]
fn portable_media_sanitizes_nested_arbitrary_json_and_preserves_ordinary_values() {
    let page_id = "arbitrary-json-page";
    let signed_url = concat!(
        "https://secure.notion-static.com/nested/file.pdf?",
        "X-Amz-Credential=credential-secret&X-Amz-Signature=signature-secret&",
        "token=token-secret#fragment-secret"
    );
    let ordinary_formula = json!({
        "type": "string",
        "string": "ordinary formula result",
        "nested": [1, true, { "status": "ready" }]
    });
    let mut fixture_page = page(page_id, "Arbitrary JSON");
    fixture_page.properties.insert(
        "formula".to_string(),
        PagePropertyDto {
            kind: "formula".to_string(),
            formula: Some(ordinary_formula.clone()),
            ..Default::default()
        },
    );
    fixture_page.properties.insert(
        "rollup".to_string(),
        PagePropertyDto {
            kind: "rollup".to_string(),
            rollup: Some(json!({
                "type": "array",
                "array": [{
                    "type": "files",
                    "files": [{
                        "name": "private.pdf",
                        "type": "file",
                        "file": {
                            "url": signed_url,
                            "expiry_time": "2099-01-01T00:00:00.000Z",
                            "authorization": "Bearer authorization-secret",
                            "secret": "nested-secret"
                        }
                    }]
                }]
            })),
            ..Default::default()
        },
    );
    let mut custom = block("custom-secret", "custom_block");
    custom.custom_block = Some(json!({
        "payload": [{
            "file": {
                "url": signed_url,
                "expiry_time": "2099-01-01T00:00:00.000Z",
                "token": "custom-token-secret"
            }
        }]
    }));
    let connector = portable_media_connector_with_page(fixture_page, vec![custom])
        .with_portable_media_capture(PortableMediaCapturePolicy::HostedPilot);

    let fetched = connector
        .fetch_portable(portable_fetch_request(page_id))
        .expect("sanitized arbitrary JSON fetch");
    assert!(!fetched.completeness.is_complete());
    let native: NotionPortablePageBundleV1 =
        serde_json::from_slice(&fetched.native.raw).expect("portable native");
    assert_eq!(
        native.page.page.properties["formula"].formula.as_ref(),
        Some(&ordinary_formula)
    );
    assert_eq!(
        native.page.page.properties["rollup"].rollup,
        Some(json!({
            "type": "array",
            "array": [{
                "type": "files",
                "files": [{
                    "name": "private.pdf",
                    "type": "file",
                    "file": { "url": "" }
                }]
            }],
            "_locality_portable_media_sanitized_v1": true
        }))
    );
    assert_eq!(
        native.page.blocks[0].block.custom_block,
        Some(json!({
            "payload": [{ "file": { "url": "" } }],
            "_locality_portable_media_sanitized_v1": true
        }))
    );
    assert_eq!(native.incomplete_media.len(), 2);
    assert!(native.incomplete_media.iter().all(|outcome| {
        outcome.kind == "arbitrary_json" && outcome.code == "sanitized_embedded_media_secret"
    }));
    let raw = String::from_utf8_lossy(&fetched.native.raw);
    for forbidden in [
        signed_url,
        "secure.notion-static.com",
        "X-Amz",
        "credential-secret",
        "signature-secret",
        "token-secret",
        "authorization-secret",
        "nested-secret",
        "fragment-secret",
        "expiry_time",
        "2099-01-01",
    ] {
        assert!(!raw.contains(forbidden), "native retained {forbidden}");
        assert!(!format!("{fetched:?}").contains(forbidden));
    }
    assert_eq!(
        raw.matches("_locality_portable_media_sanitized_v1").count(),
        2
    );

    let mut tampered = native.clone();
    tampered
        .page
        .page
        .properties
        .get_mut("rollup")
        .and_then(|property| property.rollup.as_mut())
        .and_then(serde_json::Value::as_object_mut)
        .expect("rollup object")
        .insert("leaked_url".to_string(), json!(signed_url));
    let tampered_native = NativeEntity {
        remote_id: RemoteId::new(page_id),
        kind: "notion_page_portable_media_v1".to_string(),
        raw: serde_json::to_vec(&tampered).expect("tampered arbitrary native"),
    };
    let error = connector
        .render_portable(&portable_render_request(page_id, tampered_native))
        .expect_err("render must revalidate arbitrary JSON");
    assert_eq!(
        error.to_string(),
        "invalid state: Notion portable arbitrary JSON retained media credentials"
    );
    for forbidden in [signed_url, "signature-secret", "token-secret"] {
        assert!(!format!("{error:?}").contains(forbidden));
    }

    let rendered = connector
        .render_portable(&portable_render_request(page_id, fetched.native))
        .expect("render sanitized arbitrary JSON");
    assert!(!rendered.completeness.is_complete());
    assert_eq!(rendered.projections.len(), 1);
    let rendered_debug = format!("{rendered:?}");
    for forbidden in ["https://", "X-Amz", "token-secret", "authorization-secret"] {
        assert!(!rendered_debug.contains(forbidden));
    }
}

#[test]
fn portable_media_rejects_extra_typed_payloads_without_echoing_credentials() {
    let first_url = "https://secure.notion-static.com/image.png?X-Amz-Signature=first-secret";
    let second_url = "https://secure.notion-static.com/video.mp4?X-Amz-Signature=second-secret";
    let mut malformed = hosted_file_block("malformed", "image", first_url, None);
    malformed.video = hosted_file_block("unused", "video", second_url, None).video;
    let connector = portable_media_connector("malformed-page", vec![malformed])
        .with_portable_media_capture(PortableMediaCapturePolicy::HostedPilot);
    let error = connector
        .fetch_portable(portable_fetch_request("malformed-page"))
        .expect_err("extra typed media payload must fail");
    assert_eq!(
        error.to_string(),
        "invalid state: Notion portable media block must contain exactly its selected typed payload"
    );
    let error_debug = format!("{error:?}");
    for forbidden in [first_url, second_url, "first-secret", "second-secret"] {
        assert!(!error_debug.contains(forbidden));
    }

    let mut non_media = paragraph_block("paragraph", vec![rich_text("hello")]);
    non_media.image = hosted_file_block("unused", "image", first_url, None).image;
    let connector = portable_media_connector("non-media-page", vec![non_media])
        .with_portable_media_capture(PortableMediaCapturePolicy::HostedPilot);
    assert_eq!(
        connector
            .fetch_portable(portable_fetch_request("non-media-page"))
            .expect_err("typed media on non-media block must fail")
            .to_string(),
        "invalid state: Notion portable non-media block contains a typed media payload"
    );
}

#[test]
fn portable_media_render_binds_every_incomplete_outcome_exactly() {
    let page_id = "outcome-binding-page";
    let mut fixture_page = page(page_id, "Outcome Binding");
    fixture_page.properties.insert(
        "attachments".to_string(),
        PagePropertyDto {
            kind: "files".to_string(),
            files: vec![FilePropertyDto {
                name: Some("private.pdf".to_string()),
                kind: "file".to_string(),
                external: None,
                file: Some(HostedFileDto {
                    url: "https://secure.notion-static.com/private.pdf?X-Amz-Signature=secret"
                        .to_string(),
                    expiry_time: Some("2099-01-01T00:00:00.000Z".to_string()),
                }),
            }],
            ..Default::default()
        },
    );
    let connector = portable_media_connector_with_page(fixture_page, Vec::new())
        .with_portable_media_capture(PortableMediaCapturePolicy::HostedPilot);
    let fetched = connector
        .fetch_portable(portable_fetch_request(page_id))
        .expect("property media fetch");
    let native: NotionPortablePageBundleV1 =
        serde_json::from_slice(&fetched.native.raw).expect("portable native");
    assert_eq!(
        native.incomplete_media,
        vec![NotionPortableIncompleteMediaV1 {
            block_id: "page-property-file-1".to_string(),
            kind: "file_property".to_string(),
            code: "unsupported_page_property_media".to_string(),
        }]
    );

    let mut variants = Vec::new();
    let mut missing = native.clone();
    missing.incomplete_media.clear();
    variants.push(missing);
    let mut wrong_kind = native.clone();
    wrong_kind.incomplete_media[0].kind = "image".to_string();
    variants.push(wrong_kind);
    let mut wrong_code = native.clone();
    wrong_code.incomplete_media[0].code = "external_media".to_string();
    variants.push(wrong_code);
    let mut spurious = native.clone();
    spurious
        .incomplete_media
        .push(NotionPortableIncompleteMediaV1 {
            block_id: "spurious".to_string(),
            kind: "file_property".to_string(),
            code: "unsupported_page_property_media".to_string(),
        });
    variants.push(spurious);
    for variant in variants {
        let native = NativeEntity {
            remote_id: RemoteId::new(page_id),
            kind: "notion_page_portable_media_v1".to_string(),
            raw: serde_json::to_vec(&variant).expect("tampered native"),
        };
        assert_eq!(
            connector
                .render_portable(&portable_render_request(page_id, native))
                .expect_err("tampered outcome must fail")
                .to_string(),
            "invalid state: Notion portable media native payload has invalid incomplete outcomes"
        );
    }

    let mut duplicate = native.clone();
    duplicate
        .incomplete_media
        .push(duplicate.incomplete_media[0].clone());
    let duplicate_native = NativeEntity {
        remote_id: RemoteId::new(page_id),
        kind: "notion_page_portable_media_v1".to_string(),
        raw: serde_json::to_vec(&duplicate).expect("duplicate outcome native"),
    };
    assert_eq!(
        connector
            .render_portable(&portable_render_request(page_id, duplicate_native))
            .expect_err("duplicate outcome must fail")
            .to_string(),
        "invalid state: Notion portable media native payload has duplicate incomplete outcomes"
    );

    let mut retained_secret = native;
    retained_secret
        .page
        .page
        .properties
        .get_mut("attachments")
        .expect("attachments")
        .files[0]
        .file
        .as_mut()
        .expect("hosted")
        .url = "https://secure.notion-static.com/private.pdf?X-Amz-Signature=secret".to_string();
    let tampered = NativeEntity {
        remote_id: RemoteId::new(page_id),
        kind: "notion_page_portable_media_v1".to_string(),
        raw: serde_json::to_vec(&retained_secret).expect("tampered secret native"),
    };
    let error = connector
        .render_portable(&portable_render_request(page_id, tampered))
        .expect_err("render must independently reject retained credentials");
    assert!(!format!("{error:?}").contains("X-Amz-Signature"));
    assert!(!format!("{error:?}").contains("secret"));
}

#[test]
fn portable_media_duplicate_and_count_limits_fail_closed() {
    let duplicate = hosted_file_block(
        "duplicate-block",
        "image",
        "https://secure.notion-static.com/image.png",
        None,
    );
    let duplicate_connector =
        portable_media_connector("duplicate-page", vec![duplicate.clone(), duplicate]);
    let duplicate_calls = Arc::new(Mutex::new(Vec::new()));
    let duplicate_connector = duplicate_connector.with_portable_media_capture_fetcher(
        PortableMediaCapturePolicy::HostedPilot,
        Arc::new(FixturePortableMediaFetcher {
            outcomes: BTreeMap::new(),
            calls: Arc::clone(&duplicate_calls),
        }),
    );
    assert_eq!(
        duplicate_connector
            .fetch_portable(portable_fetch_request("duplicate-page"))
            .expect_err("duplicate media must fail")
            .to_string(),
        "invalid state: Notion portable media contains a duplicate block identity"
    );
    assert!(duplicate_calls.lock().expect("duplicate calls").is_empty());

    let too_many = (0..=128)
        .map(|index| {
            hosted_file_block(
                &format!("media-{index}"),
                "image",
                &format!(
                    "https://secure.notion-static.com/{index}.png?X-Amz-Signature=secret-{index}"
                ),
                Some("2099-01-01T00:00:00.000Z"),
            )
        })
        .collect();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let fetcher = Arc::new(FixturePortableMediaFetcher {
        outcomes: BTreeMap::new(),
        calls: Arc::clone(&calls),
    });
    let count_connector = portable_media_connector("count-page", too_many)
        .with_portable_media_capture_fetcher(PortableMediaCapturePolicy::HostedPilot, fetcher);
    let fetched = count_connector
        .fetch_portable(portable_fetch_request("count-page"))
        .expect("over-limit capture must become explicitly incomplete");
    assert!(calls.lock().expect("fetch calls").is_empty());
    assert!(!fetched.completeness.is_complete());
    let raw = String::from_utf8_lossy(&fetched.native.raw);
    for forbidden in ["secure.notion-static.com", "X-Amz", "secret-", "2099-01-01"] {
        assert!(!raw.contains(forbidden));
    }
    let native: NotionPortablePageBundleV1 =
        serde_json::from_slice(&fetched.native.raw).expect("over-limit native");
    assert!(native.captured_media.is_empty());
    assert_eq!(native.incomplete_media.len(), 130);
    assert!(native.incomplete_media.iter().any(|outcome| {
        outcome.block_id == "__locality_portable_media_limit_v1"
            && outcome.kind == "page"
            && outcome.code == "asset_limit_exceeded"
    }));
    let rendered = count_connector
        .render_portable(&portable_render_request("count-page", fetched.native))
        .expect("render over-limit incomplete page");
    assert!(!rendered.completeness.is_complete());
    assert_eq!(rendered.projections.len(), 1);
    assert_eq!(
        rendered.projections[0].file_kind,
        ProjectionFileKind::Markdown
    );
    assert!(!format!("{rendered:?}").contains("X-Amz"));
}

#[test]
fn enumerate_suffixes_every_colliding_sibling_name() {
    let root_page_id = RemoteId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let api = FixtureNotionApi::colliding_tree(root_page_id.as_str());
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

    assert_eq!(entries.len(), 5);
    assert_eq!(entries[0].path, Path::new("Root/page.md"));
    assert_eq!(entries[1].path, Path::new("Root/Notes bbbbbb/page.md"));
    assert_eq!(entries[2].path, Path::new("Root/Notes cccccc/page.md"));
    assert_eq!(entries[3].path, Path::new("Root/Notes dddddd"));
    assert_eq!(
        entries[4].path,
        Path::new("Root/Notes dddddd/Fix login/page.md")
    );
}

#[test]
fn list_children_returns_workspace_root_pages_without_nested_duplicates() {
    let api = FixtureNotionApi::workspace();
    let connector = NotionConnector::with_api(NotionConfig::default(), Arc::new(api));

    let result = connector
        .list_children(ListChildrenRequest {
            mount_id: MountId::new("notion-main"),
            container: ChildContainer::Root,
            parent_path: Path::new("").to_path_buf(),
        })
        .expect("list workspace root");

    assert_eq!(result.entries.len(), 2);
    assert_eq!(result.entries[0].remote_id, RemoteId::new("root-page"));
    assert_eq!(result.entries[0].kind, EntityKind::Page);
    assert_eq!(result.entries[0].path, Path::new("Root/page.md"));
    assert_eq!(result.entries[1].remote_id, RemoteId::new("root-db"));
    assert_eq!(result.entries[1].kind, EntityKind::Database);
    assert_eq!(result.entries[1].path, Path::new("Tasks"));
}

#[test]
fn list_children_fetches_one_page_container_without_hydrating_descendants() {
    let root_page_id = RemoteId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let api = FixtureNotionApi::tree(root_page_id.as_str());
    let connector = NotionConnector::with_api(
        NotionConfig::default().with_root_page_id(root_page_id.clone()),
        Arc::new(api),
    );

    let result = connector
        .list_children(ListChildrenRequest {
            mount_id: MountId::new("notion-main"),
            container: ChildContainer::PageChildren(root_page_id),
            parent_path: Path::new("Roadmap").to_path_buf(),
        })
        .expect("list page children");

    assert_eq!(result.entries.len(), 2);
    assert_eq!(
        result.entries[0].path,
        Path::new("Roadmap/Design Notes/page.md")
    );
    assert_eq!(result.entries[0].kind, EntityKind::Page);
    assert_eq!(result.entries[1].path, Path::new("Roadmap/Tasks"));
    assert_eq!(result.entries[1].kind, EntityKind::Database);
}

#[test]
fn list_children_fetches_database_rows_under_database_directory() {
    let root_page_id = RemoteId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let api = FixtureNotionApi::tree(root_page_id.as_str());
    let connector = NotionConnector::with_api(
        NotionConfig::default().with_root_page_id(root_page_id),
        Arc::new(api),
    );

    let result = connector
        .list_children(ListChildrenRequest {
            mount_id: MountId::new("notion-main"),
            container: ChildContainer::DatabaseRows(RemoteId::new(
                "cccccccccccccccccccccccccccccccc",
            )),
            parent_path: Path::new("Roadmap/Tasks").to_path_buf(),
        })
        .expect("list database rows");

    assert_eq!(result.entries.len(), 1);
    assert_eq!(
        result.entries[0].path,
        Path::new("Roadmap/Tasks/Fix login bug/page.md")
    );
    assert_eq!(result.entries[0].kind, EntityKind::Page);
}

#[test]
fn enumerate_shared_workspace_projects_nested_pages_and_database_rows_under_parent_pages() {
    let api = FixtureNotionApi::workspace_nested_tree();
    let connector = NotionConnector::with_api(NotionConfig::default(), Arc::new(api));

    let entries = connector
        .enumerate(EnumerateRequest {
            mount_id: MountId::new("notion-main"),
            cursor: None,
        })
        .expect("enumerate workspace tree");

    assert_eq!(entries.len(), 5);
    assert_eq!(entries[0].path, Path::new("Root/page.md"));
    assert_eq!(entries[0].kind, EntityKind::Page);
    assert_eq!(entries[1].path, Path::new("Root/Design Notes/page.md"));
    assert_eq!(entries[1].kind, EntityKind::Page);
    assert_eq!(entries[2].path, Path::new("Root/Toggle Child/page.md"));
    assert_eq!(entries[2].kind, EntityKind::Page);
    assert_eq!(entries[3].path, Path::new("Root/Tasks"));
    assert_eq!(entries[3].kind, EntityKind::Database);
    assert_eq!(
        entries[4].path,
        Path::new("Root/Tasks/Fix login bug/page.md")
    );
    assert_eq!(entries[4].kind, EntityKind::Page);
}

#[test]
fn enumerate_shared_workspace_does_not_project_page_children_as_database_rows() {
    let api =
        FixtureNotionApi::workspace_database_row_with_page_child_also_returned_by_data_source();
    let connector = NotionConnector::with_api(NotionConfig::default(), Arc::new(api));

    let entries = connector
        .enumerate(EnumerateRequest {
            mount_id: MountId::new("notion-main"),
            cursor: None,
        })
        .expect("enumerate workspace tree");

    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].path, Path::new("Engineering Wiki"));
    assert_eq!(entries[0].kind, EntityKind::Database);
    assert_eq!(
        entries[1].path,
        Path::new("Engineering Wiki/Standups with Locality/page.md")
    );
    assert_eq!(entries[1].kind, EntityKind::Page);
    assert_eq!(
        entries[2].path,
        Path::new("Engineering Wiki/Standups with Locality/2026-06-26/page.md")
    );
    assert_eq!(entries[2].kind, EntityKind::Page);
}

#[test]
fn enumerate_shared_workspace_keeps_shared_database_row_when_database_is_not_shared() {
    let api = FixtureNotionApi::shared_orphan_database_row();
    let connector = NotionConnector::with_api(NotionConfig::default(), Arc::new(api));

    let entries = connector
        .enumerate(EnumerateRequest {
            mount_id: MountId::new("notion-main"),
            cursor: None,
        })
        .expect("enumerate shared orphan row");

    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].remote_id,
        RemoteId::new("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee")
    );
    assert_eq!(entries[0].path, Path::new("Shared Row/page.md"));
    assert_eq!(entries[0].kind, EntityKind::Page);
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_PAGE_ID or LOCALITY_NOTION_LIVE_PARENT_PAGE"]
fn live_fetch_and_render_page_from_environment() {
    let page_id = live_fetch_render_page_id();
    let connector = NotionConnector::new(support::live_notion_config());

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

fn live_fetch_render_page_id() -> String {
    let page_id = std::env::var(PAGE_ID_ENV).or_else(|_| std::env::var(LIVE_PARENT_ENV));
    normalize_notion_id(&page_id.unwrap_or_else(|_| panic!("{PAGE_ID_ENV} or {LIVE_PARENT_ENV}")))
}

fn normalize_notion_id(input: &str) -> String {
    let mut value = input.trim();
    if let Some((before_query, _)) = value.split_once('?') {
        value = before_query;
    }
    if let Some((before_hash, _)) = value.split_once('#') {
        value = before_hash;
    }
    let candidate = value
        .rsplit(['/', '-'])
        .find(|part| part.chars().filter(|ch| ch.is_ascii_hexdigit()).count() >= 32)
        .unwrap_or(value);
    candidate
        .chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .collect::<String>()
}

#[derive(Clone)]
enum FixturePortableMediaOutcome {
    Success(PortableMediaCapture),
    Failure(String),
}

struct FixturePortableMediaFetcher {
    outcomes: BTreeMap<String, FixturePortableMediaOutcome>,
    calls: Arc<Mutex<Vec<(String, usize)>>>,
}

impl PortableMediaCaptureFetcher for FixturePortableMediaFetcher {
    fn fetch(
        &self,
        hosted_url: &str,
        max_bytes: usize,
    ) -> locality_core::LocalityResult<PortableMediaCapture> {
        self.calls
            .lock()
            .expect("media fetch calls")
            .push((hosted_url.to_string(), max_bytes));
        match self.outcomes.get(hosted_url) {
            Some(FixturePortableMediaOutcome::Success(capture)) => Ok(capture.clone()),
            Some(FixturePortableMediaOutcome::Failure(error)) => {
                Err(locality_core::LocalityError::Io(error.clone()))
            }
            None => Err(locality_core::LocalityError::InvalidState(
                "unexpected fixture media URL".to_string(),
            )),
        }
    }
}

fn portable_media_connector(page_id: &str, blocks: Vec<BlockDto>) -> NotionConnector {
    portable_media_connector_with_page(page(page_id, "Coverage"), blocks)
}

fn portable_media_connector_with_page(
    fixture_page: PageDto,
    blocks: Vec<BlockDto>,
) -> NotionConnector {
    let page_id = fixture_page.id.clone();
    let api = FixtureNotionApi {
        pages: BTreeMap::from([(page_id.clone(), fixture_page)]),
        children: BTreeMap::from([(
            (page_id.clone(), None),
            PaginatedListDto {
                results: blocks,
                next_cursor: None,
                has_more: false,
            },
        )]),
        databases: BTreeMap::new(),
        data_sources: BTreeMap::new(),
        data_source_pages: BTreeMap::new(),
    };
    NotionConnector::with_api(
        NotionConfig::default().with_root_page_id(RemoteId::new(page_id)),
        Arc::new(NoSearchNotionApi(api)),
    )
}

fn portable_fetch_request(page_id: &str) -> PortableFetchRequest {
    PortableFetchRequest {
        source_connection_id: SourceConnectionId::new("source-notion"),
        remote_id: RemoteId::new(page_id),
        reason: PortableFetchReason::Bootstrap,
    }
}

fn portable_render_request(page_id: &str, native: NativeEntity) -> PortableRenderRequest {
    PortableRenderRequest {
        source_connection_id: SourceConnectionId::new("source-notion"),
        logical_path: LogicalPath::new(format!("{page_id}/page.md")).expect("portable path"),
        native,
        format_version: 1,
    }
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

        Self {
            pages,
            children,
            databases: BTreeMap::new(),
            data_sources: BTreeMap::new(),
            data_source_pages: BTreeMap::new(),
        }
    }

    fn many_roots(count: usize) -> Self {
        let pages = (0..count)
            .map(|index| {
                let id = format!("root-{index:02}");
                (id.clone(), page(&id, &format!("Root {index:02}")))
            })
            .collect::<BTreeMap<_, _>>();
        let children = pages
            .keys()
            .map(|id| ((id.clone(), None), PaginatedListDto::default()))
            .collect();
        Self {
            pages,
            children,
            databases: BTreeMap::new(),
            data_sources: BTreeMap::new(),
            data_source_pages: BTreeMap::new(),
        }
    }

    fn tree(root_page_id: &str) -> Self {
        let child_page_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let database_id = "cccccccccccccccccccccccccccccccc";
        let data_source_id = "dddddddddddddddddddddddddddddddd";
        let row_page_id = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
        let pages = BTreeMap::from([
            (root_page_id.to_string(), page(root_page_id, "Roadmap")),
            (
                child_page_id.to_string(),
                page(child_page_id, "Design Notes"),
            ),
            (row_page_id.to_string(), page(row_page_id, "Fix login bug")),
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
            ((row_page_id.to_string(), None), PaginatedListDto::default()),
        ]);
        let databases = BTreeMap::from([(
            database_id.to_string(),
            DatabaseDto {
                id: database_id.to_string(),
                last_edited_time: Some("2026-06-10T01:00:00.000Z".to_string()),
                title: vec![rich_text("Tasks")],
                data_sources: vec![DataSourceSummaryDto {
                    id: data_source_id.to_string(),
                    name: Some("Tasks".to_string()),
                }],
                ..Default::default()
            },
        )]);
        let data_sources = BTreeMap::from([(
            data_source_id.to_string(),
            DataSourceDto {
                id: data_source_id.to_string(),
                parent: Some(ParentDto {
                    kind: "database_id".to_string(),
                    database_id: Some(database_id.to_string()),
                    ..Default::default()
                }),
                name: Some("Tasks".to_string()),
                last_edited_time: Some("2026-06-10T01:01:00.000Z".to_string()),
                properties: BTreeMap::from([
                    (
                        "Name".to_string(),
                        DataSourcePropertyDto {
                            id: "title".to_string(),
                            kind: "title".to_string(),
                            ..Default::default()
                        },
                    ),
                    (
                        "Status".to_string(),
                        DataSourcePropertyDto {
                            id: "status-id".to_string(),
                            kind: "select".to_string(),
                            select: Some(SelectPropertySchemaDto {
                                options: vec![select_option("todo-id", "Todo")],
                            }),
                            ..Default::default()
                        },
                    ),
                ]),
                ..Default::default()
            },
        )]);
        let data_source_pages = BTreeMap::from([(
            (data_source_id.to_string(), None),
            PaginatedListDto {
                results: vec![page(row_page_id, "Fix login bug")],
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

    fn colliding_tree(root_page_id: &str) -> Self {
        let first_child_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let second_child_id = "cccccccccccccccccccccccccccccccc";
        let database_id = "dddddddddddddddddddddddddddddddd";
        let data_source_id = "99999999999999999999999999999999";
        let row_page_id = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
        let pages = BTreeMap::from([
            (root_page_id.to_string(), page(root_page_id, "Root")),
            (first_child_id.to_string(), page(first_child_id, "Notes")),
            (second_child_id.to_string(), page(second_child_id, "Notes")),
            (row_page_id.to_string(), page(row_page_id, "Fix login")),
        ]);
        let children = BTreeMap::from([
            (
                (root_page_id.to_string(), None),
                PaginatedListDto {
                    results: vec![
                        child_page_block(first_child_id, "Notes"),
                        child_page_block(second_child_id, "Notes"),
                        child_database_block(database_id, "Notes"),
                    ],
                    next_cursor: None,
                    has_more: false,
                },
            ),
            (
                (first_child_id.to_string(), None),
                PaginatedListDto::default(),
            ),
            (
                (second_child_id.to_string(), None),
                PaginatedListDto::default(),
            ),
            ((row_page_id.to_string(), None), PaginatedListDto::default()),
        ]);
        let databases = BTreeMap::from([(
            database_id.to_string(),
            DatabaseDto {
                id: database_id.to_string(),
                title: vec![rich_text("Notes")],
                data_sources: vec![DataSourceSummaryDto {
                    id: data_source_id.to_string(),
                    name: Some("Notes".to_string()),
                }],
                ..Default::default()
            },
        )]);
        let data_source_pages = BTreeMap::from([(
            (data_source_id.to_string(), None),
            PaginatedListDto {
                results: vec![page(row_page_id, "Fix login")],
                next_cursor: None,
                has_more: false,
            },
        )]);

        Self {
            pages,
            children,
            databases,
            data_sources: BTreeMap::new(),
            data_source_pages,
        }
    }

    fn workspace() -> Self {
        let root_page = page_with_parent(
            "root-page",
            "Root",
            Some(ParentDto {
                kind: "workspace".to_string(),
                workspace: Some(true),
                ..Default::default()
            }),
        );
        let nested_page = page_with_parent(
            "nested-page",
            "Nested",
            Some(ParentDto {
                kind: "page_id".to_string(),
                page_id: Some("root-page".to_string()),
                ..Default::default()
            }),
        );
        let root_database = DatabaseDto {
            id: "root-db".to_string(),
            parent: Some(ParentDto {
                kind: "workspace".to_string(),
                workspace: Some(true),
                ..Default::default()
            }),
            title: vec![rich_text("Tasks")],
            ..Default::default()
        };

        Self {
            pages: BTreeMap::from([
                (root_page.id.clone(), root_page),
                (nested_page.id.clone(), nested_page),
            ]),
            children: BTreeMap::new(),
            databases: BTreeMap::from([(root_database.id.clone(), root_database)]),
            data_sources: BTreeMap::new(),
            data_source_pages: BTreeMap::new(),
        }
    }

    fn multi_root_workspace() -> Self {
        let first_root_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let nested_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let second_root_id = "ffffffffffffffffffffffffffffffff";
        let first_root = page_with_parent(
            first_root_id,
            "Roadmap",
            Some(ParentDto {
                kind: "workspace".to_string(),
                workspace: Some(true),
                ..Default::default()
            }),
        );
        let nested = page_with_parent(
            nested_id,
            "Notes",
            Some(ParentDto {
                kind: "page_id".to_string(),
                page_id: Some(first_root_id.to_string()),
                ..Default::default()
            }),
        );
        let second_root = page_with_parent(
            second_root_id,
            "Roadmap",
            Some(ParentDto {
                kind: "workspace".to_string(),
                workspace: Some(true),
                ..Default::default()
            }),
        );
        let children = BTreeMap::from([
            (
                (first_root_id.to_string(), None),
                PaginatedListDto {
                    results: vec![child_page_block(nested_id, "Notes")],
                    next_cursor: None,
                    has_more: false,
                },
            ),
            ((nested_id.to_string(), None), PaginatedListDto::default()),
            (
                (second_root_id.to_string(), None),
                PaginatedListDto::default(),
            ),
        ]);
        Self {
            pages: BTreeMap::from([
                (first_root.id.clone(), first_root),
                (nested.id.clone(), nested),
                (second_root.id.clone(), second_root),
            ]),
            children,
            databases: BTreeMap::new(),
            data_sources: BTreeMap::new(),
            data_source_pages: BTreeMap::new(),
        }
    }

    fn workspace_nested_tree() -> Self {
        let root_page_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let child_page_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let database_id = "cccccccccccccccccccccccccccccccc";
        let data_source_id = "dddddddddddddddddddddddddddddddd";
        let row_page_id = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
        let toggle_child_page_id = "abababababababababababababababab";
        let root_page = page_with_parent(
            root_page_id,
            "Root",
            Some(ParentDto {
                kind: "workspace".to_string(),
                workspace: Some(true),
                ..Default::default()
            }),
        );
        let child_page = page_with_parent(
            child_page_id,
            "Design Notes",
            Some(ParentDto {
                kind: "page_id".to_string(),
                page_id: Some(root_page_id.to_string()),
                ..Default::default()
            }),
        );
        let toggle_child_page = page_with_parent(
            toggle_child_page_id,
            "Toggle Child",
            Some(ParentDto {
                kind: "block_id".to_string(),
                block_id: Some("toggle-1".to_string()),
                ..Default::default()
            }),
        );
        let row_page = page_with_parent(
            row_page_id,
            "Fix login bug",
            Some(ParentDto {
                kind: "data_source_id".to_string(),
                data_source_id: Some(data_source_id.to_string()),
                database_id: Some(database_id.to_string()),
                ..Default::default()
            }),
        );
        let database = DatabaseDto {
            id: database_id.to_string(),
            parent: Some(ParentDto {
                kind: "page_id".to_string(),
                page_id: Some(root_page_id.to_string()),
                ..Default::default()
            }),
            title: vec![rich_text("Tasks")],
            data_sources: vec![DataSourceSummaryDto {
                id: data_source_id.to_string(),
                name: Some("Tasks".to_string()),
            }],
            ..Default::default()
        };
        let children = BTreeMap::from([
            (
                (root_page_id.to_string(), None),
                PaginatedListDto {
                    results: vec![
                        child_page_block(child_page_id, "Design Notes"),
                        toggle_block("toggle-1", "Details").with_children(),
                        child_database_block(database_id, "Tasks"),
                    ],
                    next_cursor: None,
                    has_more: false,
                },
            ),
            (
                ("toggle-1".to_string(), None),
                PaginatedListDto {
                    results: vec![child_page_block(toggle_child_page_id, "Toggle Child")],
                    next_cursor: None,
                    has_more: false,
                },
            ),
            (
                (child_page_id.to_string(), None),
                PaginatedListDto::default(),
            ),
            (
                (toggle_child_page_id.to_string(), None),
                PaginatedListDto::default(),
            ),
            ((row_page_id.to_string(), None), PaginatedListDto::default()),
        ]);
        let data_source_pages = BTreeMap::from([(
            (data_source_id.to_string(), None),
            PaginatedListDto {
                results: vec![row_page.clone()],
                next_cursor: None,
                has_more: false,
            },
        )]);

        Self {
            pages: BTreeMap::from([
                (root_page.id.clone(), root_page),
                (child_page.id.clone(), child_page),
                (toggle_child_page.id.clone(), toggle_child_page),
                (row_page.id.clone(), row_page),
            ]),
            children,
            databases: BTreeMap::from([(database.id.clone(), database)]),
            data_sources: BTreeMap::new(),
            data_source_pages,
        }
    }

    fn shared_orphan_database_row() -> Self {
        let row_page = page_with_parent(
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "Shared Row",
            Some(ParentDto {
                kind: "data_source_id".to_string(),
                data_source_id: Some("private-data-source".to_string()),
                database_id: Some("private-database".to_string()),
                ..Default::default()
            }),
        );

        Self {
            pages: BTreeMap::from([(row_page.id.clone(), row_page)]),
            children: BTreeMap::new(),
            databases: BTreeMap::new(),
            data_sources: BTreeMap::new(),
            data_source_pages: BTreeMap::new(),
        }
    }

    fn workspace_database_row_with_page_child_also_returned_by_data_source() -> Self {
        let database_id = "engineering-db";
        let data_source_id = "engineering-ds";
        let standups_id = "standups-page";
        let standup_date_id = "standup-2026-06-26";
        let standups = page_with_parent(
            standups_id,
            "Standups with Locality",
            Some(ParentDto {
                kind: "data_source_id".to_string(),
                data_source_id: Some(data_source_id.to_string()),
                database_id: Some(database_id.to_string()),
                ..Default::default()
            }),
        );
        let standup_date = page_with_parent(
            standup_date_id,
            "2026-06-26",
            Some(ParentDto {
                kind: "page_id".to_string(),
                page_id: Some(standups_id.to_string()),
                ..Default::default()
            }),
        );
        let database = DatabaseDto {
            id: database_id.to_string(),
            parent: Some(ParentDto {
                kind: "workspace".to_string(),
                workspace: Some(true),
                ..Default::default()
            }),
            title: vec![rich_text("Engineering Wiki")],
            data_sources: vec![DataSourceSummaryDto {
                id: data_source_id.to_string(),
                name: Some("Engineering Wiki".to_string()),
            }],
            ..Default::default()
        };
        let data_source_pages = BTreeMap::from([(
            (data_source_id.to_string(), None),
            PaginatedListDto {
                results: vec![standups.clone(), standup_date.clone()],
                next_cursor: None,
                has_more: false,
            },
        )]);
        let children = BTreeMap::from([
            (
                (standups_id.to_string(), None),
                PaginatedListDto {
                    results: vec![child_page_block(standup_date_id, "2026-06-26")],
                    next_cursor: None,
                    has_more: false,
                },
            ),
            (
                (standup_date_id.to_string(), None),
                PaginatedListDto::default(),
            ),
        ]);

        Self {
            pages: BTreeMap::from([
                (standups.id.clone(), standups),
                (standup_date.id.clone(), standup_date),
            ]),
            children,
            databases: BTreeMap::from([(database.id.clone(), database)]),
            data_sources: BTreeMap::new(),
            data_source_pages,
        }
    }

    fn parent_with_child_boundaries() -> Self {
        let pages = BTreeMap::from([
            ("parent-page".to_string(), page("parent-page", "Parent")),
            ("child-page".to_string(), page("child-page", "Child Page")),
        ]);
        let databases = BTreeMap::from([(
            "child-db".to_string(),
            DatabaseDto {
                id: "child-db".to_string(),
                title: vec![rich_text("Tasks")],
                data_sources: vec![DataSourceSummaryDto {
                    id: "data-source-1".to_string(),
                    name: Some("Tasks".to_string()),
                }],
                ..Default::default()
            },
        )]);
        let children = BTreeMap::from([
            (
                ("parent-page".to_string(), None),
                PaginatedListDto {
                    results: vec![
                        paragraph_block("parent-paragraph", vec![rich_text("Parent body.")]),
                        child_page_block("child-page", "Child Page").with_children(),
                        child_database_block("child-db", "Tasks").with_children(),
                    ],
                    next_cursor: None,
                    has_more: false,
                },
            ),
            (
                ("child-page".to_string(), None),
                PaginatedListDto {
                    results: vec![paragraph_block(
                        "child-paragraph",
                        vec![rich_text("Child body.")],
                    )],
                    next_cursor: None,
                    has_more: false,
                },
            ),
            (
                ("child-db".to_string(), None),
                PaginatedListDto {
                    results: vec![paragraph_block(
                        "database-paragraph",
                        vec![rich_text("Database body.")],
                    )],
                    next_cursor: None,
                    has_more: false,
                },
            ),
        ]);

        Self {
            pages,
            children,
            databases,
            data_sources: BTreeMap::new(),
            data_source_pages: BTreeMap::new(),
        }
    }
}

impl NotionApi for FixtureNotionApi {
    fn retrieve_page(&self, page_id: &str) -> locality_core::LocalityResult<PageDto> {
        self.pages.get(page_id).cloned().ok_or_else(|| {
            locality_core::LocalityError::RemoteNotFound(format!("missing fixture page {page_id}"))
        })
    }

    fn retrieve_database(&self, database_id: &str) -> locality_core::LocalityResult<DatabaseDto> {
        self.databases.get(database_id).cloned().ok_or_else(|| {
            locality_core::LocalityError::InvalidState(format!(
                "missing fixture database {database_id}"
            ))
        })
    }

    fn retrieve_data_source(
        &self,
        data_source_id: &str,
    ) -> locality_core::LocalityResult<DataSourceDto> {
        self.data_sources
            .get(data_source_id)
            .cloned()
            .ok_or_else(|| {
                locality_core::LocalityError::InvalidState(format!(
                    "missing fixture data source {data_source_id}"
                ))
            })
    }

    fn query_data_source(
        &self,
        data_source_id: &str,
        start_cursor: Option<&str>,
    ) -> locality_core::LocalityResult<PageListDto> {
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

    fn search_databases(
        &self,
        _start_cursor: Option<&str>,
    ) -> locality_core::LocalityResult<DatabaseListDto> {
        Ok(PaginatedListDto {
            results: self.databases.values().cloned().collect(),
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

#[derive(Debug)]
struct NonNotFoundPageErrorApi;

impl NotionApi for NonNotFoundPageErrorApi {
    fn retrieve_page(&self, _page_id: &str) -> locality_core::LocalityResult<PageDto> {
        Err(locality_core::LocalityError::InvalidState(
            "injected page retrieval failure".to_string(),
        ))
    }

    fn retrieve_database(&self, _database_id: &str) -> locality_core::LocalityResult<DatabaseDto> {
        panic!("database fallback must only follow RemoteNotFound")
    }

    fn retrieve_block_children(
        &self,
        _block_id: &str,
        _start_cursor: Option<&str>,
    ) -> locality_core::LocalityResult<BlockListDto> {
        unreachable!("page failure must stop traversal")
    }

    fn search_pages(
        &self,
        _start_cursor: Option<&str>,
    ) -> locality_core::LocalityResult<PageListDto> {
        panic!("explicit-root traversal must not call search")
    }

    fn update_block(
        &self,
        _block_id: &str,
        _body: serde_json::Value,
    ) -> locality_core::LocalityResult<BlockDto> {
        unreachable!("page failure must stop traversal")
    }

    fn append_block_children(
        &self,
        _block_id: &str,
        _body: serde_json::Value,
    ) -> locality_core::LocalityResult<BlockListDto> {
        unreachable!("page failure must stop traversal")
    }

    fn delete_block(&self, _block_id: &str) -> locality_core::LocalityResult<BlockDto> {
        unreachable!("page failure must stop traversal")
    }
}

#[derive(Debug)]
struct DescendantNotFoundApi {
    retrieve_page_calls: Arc<AtomicUsize>,
}

impl NotionApi for DescendantNotFoundApi {
    fn retrieve_page(&self, page_id: &str) -> locality_core::LocalityResult<PageDto> {
        self.retrieve_page_calls.fetch_add(1, Ordering::SeqCst);
        Ok(page(page_id, "Known page"))
    }

    fn retrieve_database(&self, _database_id: &str) -> locality_core::LocalityResult<DatabaseDto> {
        panic!("a descendant block error must not trigger database fallback")
    }

    fn retrieve_block_children(
        &self,
        block_id: &str,
        _start_cursor: Option<&str>,
    ) -> locality_core::LocalityResult<BlockListDto> {
        if block_id == "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" {
            return Ok(PaginatedListDto {
                results: vec![
                    toggle_block("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "Nested").with_children(),
                ],
                next_cursor: None,
                has_more: false,
            });
        }
        Err(locality_core::LocalityError::RemoteNotFound(format!(
            "missing descendant block `{block_id}`"
        )))
    }

    fn search_pages(
        &self,
        _start_cursor: Option<&str>,
    ) -> locality_core::LocalityResult<PageListDto> {
        panic!("portable fetch must not search")
    }

    fn update_block(
        &self,
        _block_id: &str,
        _body: serde_json::Value,
    ) -> locality_core::LocalityResult<BlockDto> {
        unreachable!("not used by portable read tests")
    }

    fn append_block_children(
        &self,
        _block_id: &str,
        _body: serde_json::Value,
    ) -> locality_core::LocalityResult<BlockListDto> {
        unreachable!("not used by portable read tests")
    }

    fn delete_block(&self, _block_id: &str) -> locality_core::LocalityResult<BlockDto> {
        unreachable!("not used by portable read tests")
    }
}

#[derive(Debug)]
struct NoSearchNotionApi(FixtureNotionApi);

impl NotionApi for NoSearchNotionApi {
    fn retrieve_page(&self, page_id: &str) -> locality_core::LocalityResult<PageDto> {
        self.0.retrieve_page(page_id)
    }

    fn retrieve_database(&self, database_id: &str) -> locality_core::LocalityResult<DatabaseDto> {
        self.0.retrieve_database(database_id)
    }

    fn retrieve_data_source(
        &self,
        data_source_id: &str,
    ) -> locality_core::LocalityResult<DataSourceDto> {
        self.0.retrieve_data_source(data_source_id)
    }

    fn query_data_source(
        &self,
        data_source_id: &str,
        start_cursor: Option<&str>,
    ) -> locality_core::LocalityResult<PageListDto> {
        self.0.query_data_source(data_source_id, start_cursor)
    }

    fn retrieve_block_children(
        &self,
        block_id: &str,
        start_cursor: Option<&str>,
    ) -> locality_core::LocalityResult<BlockListDto> {
        self.0.retrieve_block_children(block_id, start_cursor)
    }

    fn search_pages(
        &self,
        _start_cursor: Option<&str>,
    ) -> locality_core::LocalityResult<PageListDto> {
        panic!("portable explicit-root traversal must not call Notion search")
    }

    fn search_databases(
        &self,
        _start_cursor: Option<&str>,
    ) -> locality_core::LocalityResult<DatabaseListDto> {
        panic!("portable explicit-root traversal must not call Notion search")
    }

    fn update_block(
        &self,
        _block_id: &str,
        _body: serde_json::Value,
    ) -> locality_core::LocalityResult<BlockDto> {
        unreachable!("not used by portable read tests")
    }

    fn append_block_children(
        &self,
        _block_id: &str,
        _body: serde_json::Value,
    ) -> locality_core::LocalityResult<BlockListDto> {
        unreachable!("not used by portable read tests")
    }

    fn delete_block(&self, _block_id: &str) -> locality_core::LocalityResult<BlockDto> {
        unreachable!("not used by portable read tests")
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
    page_with_parent(id, title, None)
}

fn page_with_parent(id: &str, title: &str, parent: Option<ParentDto>) -> PageDto {
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
                rich_text: Vec::new(),
                ..Default::default()
            },
        )]),
    }
}

fn select_option(id: &str, name: &str) -> SelectOptionDto {
    SelectOptionDto {
        id: id.to_string(),
        name: name.to_string(),
        color: None,
    }
}

fn user(id: &str, name: Option<&str>) -> locality_notion::dto::UserMentionDto {
    locality_notion::dto::UserMentionDto {
        id: id.to_string(),
        name: name.map(str::to_string),
    }
}

fn rich_text_block(id: &str, kind: &str, text: &str) -> BlockDto {
    let mut block = block(id, kind);
    let value = Some(rich_text_block_content(vec![rich_text(text)]));

    match kind {
        "paragraph" => block.paragraph = value,
        "heading_1" => block.heading_1 = value,
        "heading_2" => block.heading_2 = value,
        "heading_3" => block.heading_3 = value,
        "heading_4" => block.heading_4 = value,
        "bulleted_list_item" => block.bulleted_list_item = value,
        "numbered_list_item" => block.numbered_list_item = value,
        "quote" => block.quote = value,
        "callout" => block.callout = value,
        "toggle" => block.toggle = value,
        "template" => block.template = value,
        _ => panic!("unsupported fixture rich text kind: {kind}"),
    }

    block
}

fn paragraph_block(id: &str, rich_text: Vec<RichTextDto>) -> BlockDto {
    let mut block = block(id, "paragraph");
    block.paragraph = Some(rich_text_block_content(rich_text));
    block
}

fn rich_text_block_content(rich_text: Vec<RichTextDto>) -> RichTextBlockDto {
    RichTextBlockDto {
        rich_text,
        color: None,
    }
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
        ..Default::default()
    }
}

fn toggle_block(id: &str, text: &str) -> BlockDto {
    let mut block = block(id, "toggle");
    block.toggle = Some(rich_text_block_content(vec![rich_text(text)]));
    block
}

fn to_do_block(id: &str, text: &str, checked: bool) -> BlockDto {
    let mut block = block(id, "to_do");
    block.to_do = Some(locality_notion::dto::ToDoBlockDto {
        rich_text: vec![rich_text(text)],
        checked,
        color: None,
    });
    block
}

fn code_block(id: &str, language: &str, text: &str) -> BlockDto {
    let mut block = block(id, "code");
    block.code = Some(locality_notion::dto::CodeBlockDto {
        rich_text: vec![rich_text(text)],
        language: Some(language.to_string()),
    });
    block
}

fn equation_block(id: &str, expression: &str) -> BlockDto {
    let mut block = block(id, "equation");
    block.equation = Some(EquationBlockDto {
        expression: expression.to_string(),
    });
    block
}

fn url_block(id: &str, kind: &str, url: &str, caption: &str) -> BlockDto {
    let mut block = block(id, kind);
    let payload = Some(UrlBlockDto {
        url: url.to_string(),
        caption: vec![rich_text(caption)],
    });
    match kind {
        "embed" => block.embed = payload,
        "bookmark" => block.bookmark = payload,
        "link_preview" => block.link_preview = payload,
        _ => panic!("unsupported fixture url block kind: {kind}"),
    }
    block
}

fn file_block(id: &str, kind: &str, url: &str, caption: &str) -> BlockDto {
    let mut block = block(id, kind);
    let payload = Some(FileBlockDto {
        kind: "external".to_string(),
        external: Some(ExternalFileDto {
            url: url.to_string(),
        }),
        file: None,
        caption: vec![rich_text(caption)],
    });
    match kind {
        "image" => block.image = payload,
        "video" => block.video = payload,
        "file" => block.file = payload,
        "pdf" => block.pdf = payload,
        "audio" => block.audio = payload,
        _ => panic!("unsupported fixture file block kind: {kind}"),
    }
    block
}

fn hosted_file_block(id: &str, kind: &str, url: &str, expiry_time: Option<&str>) -> BlockDto {
    let mut block = block(id, kind);
    let payload = Some(FileBlockDto {
        kind: "file".to_string(),
        external: None,
        file: Some(HostedFileDto {
            url: url.to_string(),
            expiry_time: expiry_time.map(str::to_string),
        }),
        caption: Vec::new(),
    });
    match kind {
        "image" => block.image = payload,
        "video" => block.video = payload,
        "file" => block.file = payload,
        "pdf" => block.pdf = payload,
        "audio" => block.audio = payload,
        _ => panic!("unsupported hosted fixture file block kind: {kind}"),
    }
    block
}

fn synced_block(id: &str, source_block_id: &str) -> BlockDto {
    let mut block = block(id, "synced_block");
    block.synced_block = Some(SyncedBlockDto {
        synced_from: Some(SyncedFromDto {
            kind: "block_id".to_string(),
            block_id: Some(source_block_id.to_string()),
        }),
    });
    block
}

fn link_to_page_block(id: &str, page_id: &str) -> BlockDto {
    let mut block = block(id, "link_to_page");
    block.link_to_page = Some(LinkToPageBlockDto {
        kind: "page_id".to_string(),
        page_id: Some(page_id.to_string()),
        database_id: None,
    });
    block
}

fn link_to_database_block(id: &str, database_id: &str) -> BlockDto {
    let mut block = block(id, "link_to_page");
    block.link_to_page = Some(LinkToPageBlockDto {
        kind: "database_id".to_string(),
        page_id: None,
        database_id: Some(database_id.to_string()),
    });
    block
}

fn table_of_contents_block(id: &str) -> BlockDto {
    let mut block = block(id, "table_of_contents");
    block.table_of_contents = Some(ColorOnlyBlockDto {
        color: Some("default".to_string()),
    });
    block
}

fn empty_payload_block(id: &str, kind: &str) -> BlockDto {
    let mut block = block(id, kind);
    match kind {
        "breadcrumb" => block.breadcrumb = Some(EmptyBlockDto {}),
        "column_list" => block.column_list = Some(EmptyBlockDto {}),
        "column" => block.column = Some(EmptyBlockDto {}),
        _ => panic!("unsupported fixture empty payload block kind: {kind}"),
    }
    block
}

fn meeting_notes_block(id: &str, title: &str) -> BlockDto {
    let mut block = block(id, "meeting_notes");
    block.meeting_notes = Some(MeetingNotesBlockDto {
        title: Some(title.to_string()),
    });
    block
}

fn transcription_block(id: &str, title: &str) -> BlockDto {
    let mut block = block(id, "transcription");
    block.transcription = Some(MeetingNotesBlockDto {
        title: Some(title.to_string()),
    });
    block
}

fn raw_payload_block(id: &str, kind: &str) -> BlockDto {
    let mut block = block(id, kind);
    let payload = Some(json!({ "placeholder": true }));
    match kind {
        "tab" => block.tab = payload,
        "ai_block" => block.ai_block = payload,
        "custom_block" => block.custom_block = payload,
        "button" => block.button = payload,
        _ => panic!("unsupported fixture raw payload block kind: {kind}"),
    }
    block
}

fn table_block(id: &str, width: u16, has_column_header: bool) -> BlockDto {
    let mut block = block(id, "table");
    block.table = Some(TableBlockDto {
        table_width: width,
        has_column_header,
        has_row_header: false,
    });
    block
}

fn table_row_block<const N: usize>(id: &str, cells: [&str; N]) -> BlockDto {
    let mut block = block(id, "table_row");
    block.table_row = Some(TableRowBlockDto {
        cells: cells
            .into_iter()
            .map(|cell| vec![rich_text(cell)])
            .collect::<Vec<_>>(),
    });
    block
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

fn linked_text(text: &str, url: &str) -> RichTextDto {
    RichTextDto {
        href: Some(url.to_string()),
        text: Some(TextRichTextDto {
            content: text.to_string(),
            link: Some(LinkDto {
                url: url.to_string(),
            }),
        }),
        ..rich_text(text)
    }
}

fn date_mention(text: &str, start: &str) -> RichTextDto {
    RichTextDto {
        kind: "mention".to_string(),
        text: None,
        mention: Some(MentionRichTextDto {
            kind: "date".to_string(),
            date: Some(DateMentionDto {
                start: start.to_string(),
                end: None,
                time_zone: None,
            }),
            ..Default::default()
        }),
        plain_text: text.to_string(),
        annotations: RichTextAnnotationsDto::default(),
        ..Default::default()
    }
}

fn equation(expression: &str) -> RichTextDto {
    RichTextDto {
        kind: "equation".to_string(),
        equation: Some(EquationRichTextDto {
            expression: expression.to_string(),
        }),
        plain_text: expression.to_string(),
        ..Default::default()
    }
}

fn page_mention(text: &str, id: &str) -> RichTextDto {
    RichTextDto {
        kind: "mention".to_string(),
        mention: Some(MentionRichTextDto {
            kind: "page".to_string(),
            page: Some(IdRefDto { id: id.to_string() }),
            ..Default::default()
        }),
        plain_text: text.to_string(),
        ..Default::default()
    }
}

fn database_mention(text: &str, id: &str) -> RichTextDto {
    RichTextDto {
        kind: "mention".to_string(),
        mention: Some(MentionRichTextDto {
            kind: "database".to_string(),
            database: Some(IdRefDto { id: id.to_string() }),
            ..Default::default()
        }),
        plain_text: text.to_string(),
        ..Default::default()
    }
}

fn user_mention(text: &str, id: &str, name: &str) -> RichTextDto {
    RichTextDto {
        kind: "mention".to_string(),
        mention: Some(MentionRichTextDto {
            kind: "user".to_string(),
            user: Some(user(id, Some(name))),
            ..Default::default()
        }),
        plain_text: text.to_string(),
        ..Default::default()
    }
}

fn link_preview_mention(text: &str, url: &str) -> RichTextDto {
    RichTextDto {
        kind: "mention".to_string(),
        mention: Some(MentionRichTextDto {
            kind: "link_preview".to_string(),
            link_preview: Some(LinkDto {
                url: url.to_string(),
            }),
            ..Default::default()
        }),
        plain_text: text.to_string(),
        ..Default::default()
    }
}

fn unknown_mention(text: &str) -> RichTextDto {
    RichTextDto {
        kind: "mention".to_string(),
        mention: Some(MentionRichTextDto {
            kind: "template_mention".to_string(),
            ..Default::default()
        }),
        plain_text: text.to_string(),
        ..Default::default()
    }
}
