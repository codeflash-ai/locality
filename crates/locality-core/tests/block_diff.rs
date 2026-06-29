use locality_core::LocalityError;
use locality_core::diff::{BlockDiffEngine, DiffEngine};
use locality_core::model::{CanonicalDocument, RemoteId};
use locality_core::planner::{PlanDegradationKind, PushOperation};
use locality_core::shadow::{MarkdownBlockKind, ShadowDocument, segment_markdown_body};

#[test]
fn segments_common_markdown_blocks_with_source_spans() {
    let body = "# Title\n\nParagraph text.\n\n- one\n- two\n\n```rust\nfn main() {}\n```\n\n| A | B |\n| - | - |\n| 1 | 2 |\n\n::loc{id=media-1 type=image title=\"Diagram\"}\n";
    let blocks = segment_markdown_body(body, 10);

    assert_eq!(blocks.len(), 7);
    assert_eq!(blocks[0].kind, MarkdownBlockKind::Heading);
    assert_eq!(blocks[0].source_span.start_line, 10);
    assert_eq!(blocks[1].kind, MarkdownBlockKind::Paragraph);
    assert_eq!(blocks[2].kind, MarkdownBlockKind::List);
    assert_eq!(blocks[2].text, "- one");
    assert_eq!(blocks[3].kind, MarkdownBlockKind::List);
    assert_eq!(blocks[3].text, "- two");
    assert_eq!(blocks[4].kind, MarkdownBlockKind::CodeFence);
    assert_eq!(blocks[5].kind, MarkdownBlockKind::Table);
    assert!(matches!(
        blocks[6].kind,
        MarkdownBlockKind::Directive {
            directive_type: Some(_),
            malformed: false,
            ..
        }
    ));
    assert_eq!(blocks[6].remote_id, Some(RemoteId::new("media-1")));
}

#[test]
fn segments_code_fence_using_opening_fence_length() {
    let body =
        "````markdown\nBefore\n```python\nprint('nested')\n```\nAfter\n````\n\nNext paragraph.";
    let blocks = segment_markdown_body(body, 1);

    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].kind, MarkdownBlockKind::CodeFence);
    assert_eq!(blocks[0].source_span.start_line, 1);
    assert_eq!(blocks[0].source_span.end_line, 7);
    assert_eq!(blocks[1].kind, MarkdownBlockKind::Paragraph);
}

#[test]
fn segments_code_fence_ignores_marker_with_trailing_text() {
    let body = "```markdown\nBefore\n```not a closing fence\nAfter\n```\n\nNext paragraph.";
    let blocks = segment_markdown_body(body, 1);

    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].kind, MarkdownBlockKind::CodeFence);
    assert_eq!(
        blocks[0].text,
        "```markdown\nBefore\n```not a closing fence\nAfter\n```"
    );
    assert_eq!(blocks[0].source_span.start_line, 1);
    assert_eq!(blocks[0].source_span.end_line, 5);
    assert_eq!(blocks[1].kind, MarkdownBlockKind::Paragraph);
}

#[test]
fn editing_one_paragraph_produces_one_block_update() {
    let shadow = shadow("# Roadmap\n\nOld paragraph.", ["heading-1", "paragraph-1"]);
    let edited = CanonicalDocument::new("", "# Roadmap\n\nNew paragraph.");

    let plan = BlockDiffEngine::new()
        .plan_push(&shadow, &edited)
        .expect("plan");

    assert_eq!(
        plan.operations,
        vec![PushOperation::UpdateBlock {
            block_id: RemoteId::new("paragraph-1"),
            content: "New paragraph.".to_string(),
        }]
    );
    assert_eq!(plan.summary.blocks_updated, 1);
    assert!(plan.degradations.is_empty());
}

#[test]
fn changing_paragraph_to_list_item_produces_block_replace() {
    let shadow = shadow("Old paragraph.", ["paragraph-1"]);
    let edited = CanonicalDocument::new("", "- New bullet");

    let plan = BlockDiffEngine::new()
        .plan_push(&shadow, &edited)
        .expect("plan");

    assert_eq!(
        plan.operations,
        vec![PushOperation::ReplaceBlock {
            block_id: RemoteId::new("paragraph-1"),
            content: "- New bullet".to_string(),
        }]
    );
    assert_eq!(plan.summary.blocks_replaced, 1);
    assert_eq!(plan.summary.blocks_updated, 0);
    assert_eq!(plan.summary.blocks_created, 0);
    assert_eq!(plan.summary.blocks_archived, 0);
    assert!(plan.degradations.is_empty());
}

#[test]
fn changing_rendered_line_break_checklist_to_bullet_produces_block_replace() {
    let shadow = shadow(
        "- [ ] Define launch positioning<br>- [ ] Finalize platform install paths<br>- [ ] Validate auto-update end to end",
        ["todo-list-1"],
    );
    let edited = CanonicalDocument::new(
        "",
        "- Safety: diff, push confirmation, undo, and prompt-injection guidance",
    );

    let plan = BlockDiffEngine::new()
        .plan_push(&shadow, &edited)
        .expect("plan");

    assert_eq!(
        plan.operations,
        vec![PushOperation::ReplaceBlock {
            block_id: RemoteId::new("todo-list-1"),
            content: "- Safety: diff, push confirmation, undo, and prompt-injection guidance"
                .to_string(),
        }]
    );
    assert_eq!(plan.summary.blocks_replaced, 1);
    assert_eq!(plan.summary.blocks_updated, 0);
}

#[test]
fn changing_heading_level_produces_block_replace() {
    let shadow = shadow("# Heading", ["heading-1"]);
    let edited = CanonicalDocument::new("", "## Heading");

    let plan = BlockDiffEngine::new()
        .plan_push(&shadow, &edited)
        .expect("plan");

    assert_eq!(
        plan.operations,
        vec![PushOperation::ReplaceBlock {
            block_id: RemoteId::new("heading-1"),
            content: "## Heading".to_string(),
        }]
    );
    assert_eq!(plan.summary.blocks_replaced, 1);
}

#[test]
fn appending_a_paragraph_produces_append_after_last_existing_block() {
    let shadow = shadow(
        "# Roadmap\n\nExisting paragraph.",
        ["heading-1", "paragraph-1"],
    );
    let edited = CanonicalDocument::new("", "# Roadmap\n\nExisting paragraph.\n\nAdded paragraph.");

    let plan = BlockDiffEngine::new()
        .plan_push(&shadow, &edited)
        .expect("plan");

    assert_eq!(
        plan.operations,
        vec![PushOperation::AppendBlock {
            parent_id: RemoteId::new("page-1"),
            after: Some(RemoteId::new("paragraph-1")),
            content: "Added paragraph.".to_string(),
        }]
    );
    assert_eq!(plan.summary.blocks_created, 1);
}

#[test]
fn inserting_paragraph_before_equivalent_local_media_keeps_media_block_identity() {
    let shadow = shadow(
        "```python\nprint(\"hi\")\n```\n\n[cars](.loc/media/Roadmap/cars.mp4)\n\nfaah from remote",
        ["code-1", "video-1", "paragraph-1"],
    );
    let edited = CanonicalDocument::new(
        "",
        "```python\nprint(\"hi\")\n```\n\nfaaah\n\n[cars](/tmp/loc-content/notion-main/files/.loc/media/Roadmap/cars.mp4)\n\nfaah from remote",
    );

    let plan = BlockDiffEngine::new()
        .plan_push(&shadow, &edited)
        .expect("plan");

    assert_eq!(
        plan.operations,
        vec![PushOperation::AppendBlock {
            parent_id: RemoteId::new("page-1"),
            after: Some(RemoteId::new("code-1")),
            content: "faaah".to_string(),
        }]
    );
}

#[test]
fn appending_consecutive_list_items_produces_one_append_per_item() {
    let shadow = shadow("Existing paragraph.", ["paragraph-1"]);
    let edited =
        CanonicalDocument::new("", "Existing paragraph.\n\n- First\n- Second\n- [ ] Third");

    let plan = BlockDiffEngine::new()
        .plan_push(&shadow, &edited)
        .expect("plan");

    assert_eq!(
        plan.operations,
        vec![
            PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("paragraph-1")),
                content: "- First".to_string(),
            },
            PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("paragraph-1")),
                content: "- Second".to_string(),
            },
            PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("paragraph-1")),
                content: "- [ ] Third".to_string(),
            },
        ]
    );
    assert_eq!(plan.summary.blocks_created, 3);
}

#[test]
fn list_item_continuation_lines_stay_with_their_item() {
    let body = "- first\n  continuation\n- second";
    let blocks = segment_markdown_body(body, 1);

    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].text, "- first\n  continuation");
    assert_eq!(blocks[1].text, "- second");
}

#[test]
fn compacting_blank_lines_between_list_items_is_not_a_remote_change() {
    let shadow = shadow("- First\n\n- Second", ["list-1", "list-2"]);
    let edited = CanonicalDocument::new("", "- First\n- Second");

    let plan = BlockDiffEngine::new()
        .plan_push(&shadow, &edited)
        .expect("plan");

    assert!(plan.operations.is_empty());
}

#[test]
fn appending_after_repeated_blocks_does_not_archive_unmodified_duplicates() {
    let shadow = shadow(
        "---\n\n---\n\n- Repeat\n\n- Repeat",
        ["divider-1", "divider-2", "list-1", "list-2"],
    );
    let edited =
        CanonicalDocument::new("", "---\n\n---\n\n- Repeat\n\n- Repeat\n\nAdded paragraph.");

    let plan = BlockDiffEngine::new()
        .plan_push(&shadow, &edited)
        .expect("plan");

    assert_eq!(
        plan.operations,
        vec![PushOperation::AppendBlock {
            parent_id: RemoteId::new("page-1"),
            after: Some(RemoteId::new("list-2")),
            content: "Added paragraph.".to_string(),
        }]
    );
    assert_eq!(plan.summary.blocks_created, 1);
    assert_eq!(plan.summary.blocks_archived, 0);
    assert!(plan.degradations.is_empty());
}

#[test]
fn inserting_before_unchanged_directives_does_not_plan_redundant_moves() {
    let shadow = shadow(
        "Intro paragraph.\n\n::loc{id=column-list-1 type=column_list}\n\n::loc{id=column-1 type=column}",
        ["paragraph-1"],
    );
    let edited = CanonicalDocument::new(
        "",
        "Intro paragraph.\n\nInserted paragraph.\n\n::loc{id=column-list-1 type=column_list}\n\n::loc{id=column-1 type=column}",
    );

    let plan = BlockDiffEngine::new()
        .plan_push(&shadow, &edited)
        .expect("plan");

    assert_eq!(
        plan.operations,
        vec![PushOperation::AppendBlock {
            parent_id: RemoteId::new("page-1"),
            after: Some(RemoteId::new("paragraph-1")),
            content: "Inserted paragraph.".to_string(),
        }]
    );
    assert_eq!(plan.summary.blocks_created, 1);
    assert_eq!(plan.summary.blocks_moved, 0);
}

#[test]
fn deleting_a_normal_paragraph_produces_archive() {
    let shadow = shadow(
        "# Roadmap\n\nParagraph to delete.",
        ["heading-1", "paragraph-1"],
    );
    let edited = CanonicalDocument::new("", "# Roadmap");

    let plan = BlockDiffEngine::new()
        .plan_push(&shadow, &edited)
        .expect("plan");

    assert_eq!(
        plan.operations,
        vec![PushOperation::ArchiveBlock {
            block_id: RemoteId::new("paragraph-1"),
        }]
    );
    assert_eq!(plan.summary.blocks_archived, 1);
}

#[test]
fn moving_an_unchanged_directive_produces_move_not_validation_error() {
    let shadow = shadow(
        "Intro paragraph.\n\n::loc{id=media-1 type=image title=\"Diagram\"}",
        ["paragraph-1"],
    );
    let edited = CanonicalDocument::new(
        "",
        "::loc{id=media-1 type=image title=\"Diagram\"}\n\nIntro paragraph.",
    );

    let plan = BlockDiffEngine::new()
        .plan_push(&shadow, &edited)
        .expect("plan");

    assert_eq!(
        plan.operations,
        vec![PushOperation::MoveBlock {
            block_id: RemoteId::new("media-1"),
            after: None,
        }]
    );
}

#[test]
fn moving_native_blocks_recreates_them_for_connectors_without_native_move() {
    let shadow = shadow(
        "- A\n\n## Section\n\n- B\n\n- C",
        ["a", "section", "b", "c"],
    );
    let edited = CanonicalDocument::new("", "- A\n\n- B\n\n- C\n\n## Section");

    let plan = BlockDiffEngine::new()
        .plan_push(&shadow, &edited)
        .expect("plan");

    assert_eq!(
        plan.operations,
        vec![
            PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("a")),
                content: "- B".to_string(),
            },
            PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("a")),
                content: "- C".to_string(),
            },
            PushOperation::ArchiveBlock {
                block_id: RemoteId::new("b"),
            },
            PushOperation::ArchiveBlock {
                block_id: RemoteId::new("c"),
            },
        ]
    );
    assert_eq!(plan.summary.blocks_created, 2);
    assert_eq!(plan.summary.blocks_archived, 2);
    assert_eq!(plan.summary.blocks_moved, 0);
}

#[test]
fn editing_a_directive_fails_validation_instead_of_planning() {
    let shadow = shadow(
        "Intro paragraph.\n\n::loc{id=media-1 type=image title=\"Diagram\"}",
        ["paragraph-1"],
    );
    let edited = CanonicalDocument::new(
        "",
        "Intro paragraph.\n\n::loc{id=media-1 type=image title=\"Edited\"}",
    );

    let error = BlockDiffEngine::new()
        .plan_push(&shadow, &edited)
        .expect_err("directive edit should fail");

    let LocalityError::Validation(issues) = error else {
        panic!("expected validation error");
    };
    assert_eq!(issues[0].code, "directive_mangled");
}

#[test]
fn ambiguous_residual_alignment_is_explicitly_degraded() {
    let shadow = shadow("First paragraph.\n\n- Second item", ["block-1", "block-2"]);
    let edited = CanonicalDocument::new("", "- First rewrite.\n\nSecond rewrite.");

    let plan = BlockDiffEngine::new()
        .plan_push(&shadow, &edited)
        .expect("plan");

    assert_eq!(plan.summary.blocks_updated, 0);
    assert_eq!(plan.summary.blocks_created, 2);
    assert_eq!(plan.summary.blocks_archived, 2);
    assert_eq!(plan.degradations.len(), 1);
    assert_eq!(
        plan.degradations[0].kind,
        PlanDegradationKind::AmbiguousBlockAlignment
    );
}

#[test]
fn bounded_section_rewrite_aligns_by_order_instead_of_append_archive() {
    let shadow = shadow(
        "## Framer Copy Draft\n\n### Hero\n\nBadge: Old badge\n\n- Product\n\n- How it works\n\n### Workflow Strip",
        [
            "draft",
            "hero",
            "badge",
            "product",
            "how-it-works",
            "workflow",
        ],
    );
    let edited = CanonicalDocument::new(
        "",
        "## Framer Copy Draft\n\n### Hero\n\nBadge: Local workspace layer for AI agents\n\nProduct: Download for Mac\n\nHow it works: See how it works\n\n### Workflow Strip",
    );

    let plan = BlockDiffEngine::new()
        .plan_push(&shadow, &edited)
        .expect("plan");

    assert_eq!(
        plan.operations,
        vec![
            PushOperation::UpdateBlock {
                block_id: RemoteId::new("badge"),
                content: "Badge: Local workspace layer for AI agents".to_string(),
            },
            PushOperation::ReplaceBlock {
                block_id: RemoteId::new("product"),
                content: "Product: Download for Mac".to_string(),
            },
            PushOperation::ReplaceBlock {
                block_id: RemoteId::new("how-it-works"),
                content: "How it works: See how it works".to_string(),
            },
        ]
    );
    assert_eq!(plan.summary.blocks_updated, 1);
    assert_eq!(plan.summary.blocks_replaced, 2);
    assert_eq!(plan.summary.blocks_created, 0);
    assert_eq!(plan.summary.blocks_archived, 0);
    assert!(plan.degradations.is_empty());
}

#[test]
fn heading_bounded_rewrite_with_count_change_stays_degraded() {
    let shadow = shadow(
        "## Framer Copy Draft\n\n### Hero\n\nBadge: Old badge\n\n- Product\n\n### Workflow Strip",
        ["draft", "hero", "badge", "product", "workflow"],
    );
    let edited = CanonicalDocument::new(
        "",
        "## Framer Copy Draft\n\n### Hero\n\nBadge: New badge\n\nProduct: Download for Mac\n\nHow it works: See how it works\n\n### Workflow Strip",
    );

    let plan = BlockDiffEngine::new()
        .plan_push(&shadow, &edited)
        .expect("plan");

    assert_eq!(plan.summary.blocks_updated, 0);
    assert_eq!(plan.summary.blocks_replaced, 0);
    assert_eq!(plan.summary.blocks_created, 3);
    assert_eq!(plan.summary.blocks_archived, 2);
    assert_eq!(plan.degradations.len(), 1);
    assert_eq!(
        plan.degradations[0].kind,
        PlanDegradationKind::AmbiguousBlockAlignment
    );
}

#[test]
fn residual_alignment_updates_same_kind_sequence_without_archive_recreate() {
    let shadow = shadow(
        "# Heading\n\nParagraph.\n\n- Item\n\n```rust\nfn old() {}\n```",
        ["heading-1", "paragraph-1", "list-1", "code-1"],
    );
    let edited = CanonicalDocument::new(
        "",
        "# Heading changed\n\nParagraph changed.\n\n- Item changed\n\n```rust\nfn new() {}\n```",
    );

    let plan = BlockDiffEngine::new()
        .plan_push(&shadow, &edited)
        .expect("plan");

    assert_eq!(plan.summary.blocks_updated, 4);
    assert_eq!(plan.summary.blocks_created, 0);
    assert_eq!(plan.summary.blocks_archived, 0);
    assert!(plan.degradations.is_empty());
    assert_eq!(
        plan.operations,
        vec![
            PushOperation::UpdateBlock {
                block_id: RemoteId::new("heading-1"),
                content: "# Heading changed".to_string(),
            },
            PushOperation::UpdateBlock {
                block_id: RemoteId::new("paragraph-1"),
                content: "Paragraph changed.".to_string(),
            },
            PushOperation::UpdateBlock {
                block_id: RemoteId::new("list-1"),
                content: "- Item changed".to_string(),
            },
            PushOperation::UpdateBlock {
                block_id: RemoteId::new("code-1"),
                content: "```rust\nfn new() {}\n```".to_string(),
            },
        ]
    );
}

#[test]
fn editing_a_rendered_table_produces_table_block_update() {
    let mut shadow = shadow(
        "| Name | Status |\n| --- | --- |\n| Old task | Todo |",
        ["table-1"],
    );
    shadow.blocks[0].kind = MarkdownBlockKind::TableWithRows {
        row_ids: vec![RemoteId::new("row-1"), RemoteId::new("row-2")],
        has_column_header: true,
        has_row_header: false,
    };
    let edited =
        CanonicalDocument::new("", "| Name | Status |\n| --- | --- |\n| New task | Done |");

    let plan = BlockDiffEngine::new()
        .plan_push(&shadow, &edited)
        .expect("plan");

    assert_eq!(
        plan.operations,
        vec![PushOperation::UpdateBlock {
            block_id: RemoteId::new("table-1"),
            content: "| Name | Status |\n| --- | --- |\n| New task | Done |".to_string(),
        }]
    );
}

fn shadow<const N: usize>(body: &str, ids: [&str; N]) -> ShadowDocument {
    ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        body,
        1,
        ids.into_iter().map(RemoteId::new),
    )
    .expect("shadow")
}
