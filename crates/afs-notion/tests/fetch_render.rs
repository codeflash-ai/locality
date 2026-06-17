use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use afs_connector::{
    ChildContainer, Connector, EnumerateRequest, FetchRequest, ListChildrenRequest,
};
use afs_core::model::{EntityKind, MountId, RemoteId};
use afs_core::shadow::MarkdownBlockKind;
use afs_notion::client::NotionApi;
use afs_notion::dto::{
    BlockDto, BlockListDto, BlockTreeDto, ColorOnlyBlockDto, DataSourceDto, DataSourcePropertyDto,
    DataSourceSummaryDto, DatabaseDto, DatabaseListDto, DateMentionDto, EmptyBlockDto,
    EquationBlockDto, EquationRichTextDto, ExternalFileDto, FileBlockDto, FilePropertyDto,
    HostedFileDto, IdRefDto, LinkDto, LinkToPageBlockDto, MeetingNotesBlockDto, MentionRichTextDto,
    PageDto, PageListDto, PagePropertyDto, PaginatedListDto, ParentDto, RichTextAnnotationsDto,
    RichTextBlockDto, RichTextDto, SelectOptionDto, SelectPropertySchemaDto, SyncedBlockDto,
    SyncedFromDto, TableBlockDto, TableRowBlockDto, TextRichTextDto, TitleBlockDto,
    UniqueIdPropertyDto, UrlBlockDto, VerificationPropertyDto,
};
use afs_notion::{NotionConfig, NotionConnector};
use serde_json::json;

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
fn render_empty_paragraph_as_blank_line_without_shadow_marker() {
    let bundle = afs_notion::dto::NotionPageBundle {
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

    let rendered = afs_notion::render::render_page_bundle(&bundle).expect("render");

    assert_eq!(
        rendered.document.body,
        "First paragraph.\n\n\n\nSecond paragraph.\n"
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
fn fetch_does_not_inline_child_page_or_database_content_into_parent_body() {
    let api = FixtureNotionApi::parent_with_child_boundaries();
    let connector = NotionConnector::with_api(NotionConfig::default(), Arc::new(api));

    let native = connector
        .fetch(FetchRequest {
            remote_id: RemoteId::new("parent-page"),
        })
        .expect("fetch parent");
    let bundle: afs_notion::dto::NotionPageBundle =
        serde_json::from_slice(&native.raw).expect("native bundle");

    assert_eq!(bundle.blocks.len(), 3);
    assert!(bundle.blocks.iter().all(|tree| tree.children.is_empty()));

    let rendered = connector
        .render_native_entity(&native)
        .expect("render parent");

    assert_eq!(
        rendered.document.body,
        "Parent body.\n\n[Child Page](https://www.notion.so/child-page)\n\n::afs{id=child-db type=child_database title=\"Tasks\"}\n"
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
        "::afs{id=future-1 type=unsupported_future_block}\n"
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
fn render_toggle_children_as_nested_markdown() {
    let bundle = afs_notion::dto::NotionPageBundle {
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

    let rendered = afs_notion::render::render_page_bundle(&bundle).expect("render");

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
    let bundle = afs_notion::dto::NotionPageBundle {
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

    let rendered = afs_notion::render::render_page_bundle(&bundle).expect("render");

    assert_eq!(
        rendered.document.body,
        concat!(
            "#### Heading four\n\n",
            "- Toggle summary\n\n",
            "$$\nE=mc^2\n$$\n\n",
            "[Embed caption](https://example.com/embed)\n\n",
            "[Bookmark caption](https://example.com/bookmark)\n\n",
            "![Image caption](https://example.com/image.png)\n\n",
            "::afs{id=synced-1 type=synced_block source_block_id=\"source-block-1\"}\n\n",
            "[Linked page](https://www.notion.so/target-page-1)\n\n",
            "::afs{id=toc-1 type=table_of_contents color=\"default\"}\n\n",
            "::afs{id=breadcrumb-1 type=breadcrumb}\n\n",
            "::afs{id=column-list-1 type=column_list}\n\n",
            "::afs{id=column-1 type=column}\n\n",
            "::afs{id=meeting-1 type=meeting_notes title=\"Weekly sync\"}\n"
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
    let bundle = afs_notion::dto::NotionPageBundle {
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

    let rendered = afs_notion::render::render_page_bundle_with_options(
        &bundle,
        &afs_notion::render::RenderOptions::with_page_path("Docs/Coverage.md"),
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
        "::afs{id=orphan-row-1 type=unsupported_table_row}",
        "[Child Page](https://www.notion.so/child-page-1)",
        "::afs{id=child-db-1 type=child_database title=\"Child DB\"}",
        "- Toggle summary",
        "    Toggle child",
        "$$\nE=mc^2\n$$",
        "[Embed](https://example.com/embed)",
        "[Bookmark](https://example.com/bookmark)",
        "[Preview](https://example.com/preview)",
        "![Image](https://example.com/image.png)",
        "[Video](https://example.com/video.mp4)",
        "[File](https://example.com/file.txt)",
        "[PDF](https://example.com/file.pdf)",
        "[Audio](https://example.com/audio.mp3)",
        "::afs{id=synced-original-1 type=synced_block}",
        "::afs{id=synced-copy-1 type=synced_block source_block_id=\"source-block-1\"}",
        "[Linked page](https://www.notion.so/target-page-1)",
        "[Linked database](https://www.notion.so/target-db-1)",
        "::afs{id=toc-1 type=table_of_contents color=\"default\"}",
        "::afs{id=breadcrumb-1 type=breadcrumb}",
        "::afs{id=column-list-1 type=column_list}",
        "::afs{id=column-1 type=column}",
        "Column one",
        "::afs{id=column-2 type=column}",
        "Column two",
        "::afs{id=template-1 type=template title=\"Template\"}",
        "::afs{id=meeting-1 type=meeting_notes title=\"Meeting\"}",
        "::afs{id=transcription-1 type=transcription title=\"Transcript\"}",
        "::afs{id=tab-1 type=tab}",
        "::afs{id=ai-1 type=ai_block}",
        "::afs{id=custom-1 type=custom_block}",
        "::afs{id=button-1 type=button}",
        "::afs{id=future-1 type=unsupported_future_block}",
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
}

#[test]
fn render_table_as_markdown_table_with_row_shadow_metadata() {
    let bundle = afs_notion::dto::NotionPageBundle {
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
fn render_malformed_table_as_directives() {
    let bundle = afs_notion::dto::NotionPageBundle {
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
        "::afs{id=table-1 type=unsupported_table}\n\n::afs{id=row-1 type=unsupported_table_row}\n"
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
    let bundle = afs_notion::dto::NotionPageBundle {
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

    let rendered = afs_notion::render::render_page_bundle_with_options(
        &bundle,
        &afs_notion::render::RenderOptions::with_page_path("Docs/Coverage/page.md"),
    )
    .expect("render");

    assert_eq!(rendered.media_assets.len(), 1);
    assert_eq!(
        rendered.media_assets[0].local_path,
        Path::new("media/Docs/Coverage/image-0123456789ab.png")
    );
    assert_eq!(
        rendered.document.body,
        "![Image caption](https://example.com/image.PNG?download=1)\n"
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
    let bundle = afs_notion::dto::NotionPageBundle {
        page: page("page-1", "Coverage"),
        blocks: vec![BlockTreeDto {
            block,
            children: Vec::new(),
        }],
    };

    let rendered = afs_notion::render::render_page_bundle(&bundle).expect("render");

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
    let bundle = afs_notion::dto::NotionPageBundle {
        page: page("page-1", "Coverage"),
        blocks: vec![BlockTreeDto {
            block,
            children: Vec::new(),
        }],
    };

    let rendered = afs_notion::render::render_page_bundle(&bundle).expect("render");

    assert_eq!(
        rendered.document.body,
        "::afs{id=image-without-url type=image title=\"Image caption\"}\n"
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

    let bundle = afs_notion::dto::NotionPageBundle {
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
        "**Bold** _italic_ ~~strike~~ <u>underline</u> `code` [external link](https://example.com/) after link. 2026-06-10 and inline equation $E=mc^2$ plus page mention [Roadmap](https://www.notion.so/page-1) database mention [Tasks](https://www.notion.so/database-1) user mention @Ada preview [Example](https://example.com/preview) unknown Fallback\n"
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
    let bundle = afs_notion::dto::NotionPageBundle {
        page: row,
        blocks: Vec::new(),
    };

    let rendered = afs_notion::render::render_page_bundle(&bundle).expect("render");

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
                email: Some("agentfs@example.com".to_string()),
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
                    prefix: Some("AFS".to_string()),
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
    let bundle = afs_notion::dto::NotionPageBundle {
        page: row,
        blocks: Vec::new(),
    };

    let rendered = afs_notion::render::render_page_bundle(&bundle).expect("render");
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
        "\"Email\": \"agentfs@example.com\"",
        "\"Phone\": \"+1 415 555 0100\"",
        "\"Files\":\n  - \"Spec <https://example.com/spec.pdf>\"\n  - \"https://example.com/hosted.png\"",
        "\"People\":\n  - \"Ada <user-1>\"",
        "\"Relation\":\n  - \"related-page-1\"",
        "\"Created Time\": \"2026-06-10T00:00:00.000Z\"",
        "\"Last Edited Time\": \"2026-06-11T00:00:00.000Z\"",
        "\"Created By\": \"Creator\"",
        "\"Last Edited By\": \"Editor\"",
        "\"Formula String\": \"computed\"",
        "\"Formula Number\": 9",
        "\"Formula Boolean\": true",
        "\"Formula Date\": \"2026-06-12\"",
        "\"Rollup Number\": 11",
        "\"Rollup Array\":",
        "\"Unique ID\": \"AFS-12\"",
        "\"Verification\":\n  \"state\": \"verified\"\n  \"verified_by\": \"Verifier\"\n  \"date\": \"2026-06-10\"",
    ] {
        assert!(
            frontmatter.contains(expected),
            "missing frontmatter coverage: {expected}\n{frontmatter}"
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
    assert_eq!(entries[0].path, Path::new("roadmap/page.md"));
    assert_eq!(entries[0].kind, EntityKind::Page);
    assert_eq!(entries[1].path, Path::new("roadmap/design-notes/page.md"));
    assert_eq!(entries[1].kind, EntityKind::Page);
    assert_eq!(entries[2].path, Path::new("roadmap/tasks"));
    assert_eq!(entries[2].kind, EntityKind::Database);
    assert_eq!(
        entries[3].path,
        Path::new("roadmap/tasks/fix-login-bug/page.md")
    );
    assert_eq!(entries[3].kind, EntityKind::Page);
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
    assert_eq!(entries[0].path, Path::new("root/page.md"));
    assert_eq!(entries[1].path, Path::new("root/notes bbbbbb/page.md"));
    assert_eq!(entries[2].path, Path::new("root/notes cccccc/page.md"));
    assert_eq!(entries[3].path, Path::new("root/notes dddddd"));
    assert_eq!(
        entries[4].path,
        Path::new("root/notes dddddd/fix-login/page.md")
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
    assert_eq!(result.entries[0].path, Path::new("root/page.md"));
    assert_eq!(result.entries[1].remote_id, RemoteId::new("root-db"));
    assert_eq!(result.entries[1].kind, EntityKind::Database);
    assert_eq!(result.entries[1].path, Path::new("tasks"));
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
            parent_path: Path::new("roadmap").to_path_buf(),
        })
        .expect("list page children");

    assert_eq!(result.entries.len(), 2);
    assert_eq!(
        result.entries[0].path,
        Path::new("roadmap/design-notes/page.md")
    );
    assert_eq!(result.entries[0].kind, EntityKind::Page);
    assert_eq!(result.entries[1].path, Path::new("roadmap/tasks"));
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
            parent_path: Path::new("roadmap/tasks").to_path_buf(),
        })
        .expect("list database rows");

    assert_eq!(result.entries.len(), 1);
    assert_eq!(
        result.entries[0].path,
        Path::new("roadmap/tasks/fix-login-bug/page.md")
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
    assert_eq!(entries[0].path, Path::new("root/page.md"));
    assert_eq!(entries[0].kind, EntityKind::Page);
    assert_eq!(entries[1].path, Path::new("root/design-notes/page.md"));
    assert_eq!(entries[1].kind, EntityKind::Page);
    assert_eq!(entries[2].path, Path::new("root/toggle-child/page.md"));
    assert_eq!(entries[2].kind, EntityKind::Page);
    assert_eq!(entries[3].path, Path::new("root/tasks"));
    assert_eq!(entries[3].kind, EntityKind::Database);
    assert_eq!(
        entries[4].path,
        Path::new("root/tasks/fix-login-bug/page.md")
    );
    assert_eq!(entries[4].kind, EntityKind::Page);
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
    assert_eq!(entries[0].path, Path::new("shared-row/page.md"));
    assert_eq!(entries[0].kind, EntityKind::Page);
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
                name: Some("Tasks".to_string()),
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
    fn retrieve_page(&self, page_id: &str) -> afs_core::AfsResult<PageDto> {
        self.pages.get(page_id).cloned().ok_or_else(|| {
            afs_core::AfsError::InvalidState(format!("missing fixture page {page_id}"))
        })
    }

    fn retrieve_database(&self, database_id: &str) -> afs_core::AfsResult<DatabaseDto> {
        self.databases.get(database_id).cloned().ok_or_else(|| {
            afs_core::AfsError::InvalidState(format!("missing fixture database {database_id}"))
        })
    }

    fn retrieve_data_source(&self, data_source_id: &str) -> afs_core::AfsResult<DataSourceDto> {
        self.data_sources
            .get(data_source_id)
            .cloned()
            .ok_or_else(|| {
                afs_core::AfsError::InvalidState(format!(
                    "missing fixture data source {data_source_id}"
                ))
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

    fn search_databases(
        &self,
        _start_cursor: Option<&str>,
    ) -> afs_core::AfsResult<DatabaseListDto> {
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

fn user(id: &str, name: Option<&str>) -> afs_notion::dto::UserMentionDto {
    afs_notion::dto::UserMentionDto {
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
    block.to_do = Some(afs_notion::dto::ToDoBlockDto {
        rich_text: vec![rich_text(text)],
        checked,
        color: None,
    });
    block
}

fn code_block(id: &str, language: &str, text: &str) -> BlockDto {
    let mut block = block(id, "code");
    block.code = Some(afs_notion::dto::CodeBlockDto {
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
