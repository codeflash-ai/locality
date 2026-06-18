use afs_core::AfsError;
use afs_core::diff::{BlockDiffEngine, DiffEngine};
use afs_core::model::{CanonicalDocument, RemoteId};
use afs_core::planner::{PlanDegradationKind, PushOperation};
use afs_core::shadow::{MarkdownBlockKind, ShadowDocument, segment_markdown_body};

#[test]
fn segments_common_markdown_blocks_with_source_spans() {
    let body = "# Title\n\nParagraph text.\n\n- one\n- two\n\n```rust\nfn main() {}\n```\n\n| A | B |\n| - | - |\n| 1 | 2 |\n\n::afs{id=media-1 type=image title=\"Diagram\"}\n";
    let blocks = segment_markdown_body(body, 10);

    assert_eq!(blocks.len(), 6);
    assert_eq!(blocks[0].kind, MarkdownBlockKind::Heading);
    assert_eq!(blocks[0].source_span.start_line, 10);
    assert_eq!(blocks[1].kind, MarkdownBlockKind::Paragraph);
    assert_eq!(blocks[2].kind, MarkdownBlockKind::List);
    assert_eq!(blocks[3].kind, MarkdownBlockKind::CodeFence);
    assert_eq!(blocks[4].kind, MarkdownBlockKind::Table);
    assert!(matches!(
        blocks[5].kind,
        MarkdownBlockKind::Directive {
            directive_type: Some(_),
            malformed: false,
            ..
        }
    ));
    assert_eq!(blocks[5].remote_id, Some(RemoteId::new("media-1")));
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
fn inserting_before_unchanged_directives_does_not_plan_redundant_moves() {
    let shadow = shadow(
        "Intro paragraph.\n\n::afs{id=column-list-1 type=column_list}\n\n::afs{id=column-1 type=column}",
        ["paragraph-1"],
    );
    let edited = CanonicalDocument::new(
        "",
        "Intro paragraph.\n\nInserted paragraph.\n\n::afs{id=column-list-1 type=column_list}\n\n::afs{id=column-1 type=column}",
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
        "Intro paragraph.\n\n::afs{id=media-1 type=image title=\"Diagram\"}",
        ["paragraph-1"],
    );
    let edited = CanonicalDocument::new(
        "",
        "::afs{id=media-1 type=image title=\"Diagram\"}\n\nIntro paragraph.",
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
fn editing_a_directive_fails_validation_instead_of_planning() {
    let shadow = shadow(
        "Intro paragraph.\n\n::afs{id=media-1 type=image title=\"Diagram\"}",
        ["paragraph-1"],
    );
    let edited = CanonicalDocument::new(
        "",
        "Intro paragraph.\n\n::afs{id=media-1 type=image title=\"Edited\"}",
    );

    let error = BlockDiffEngine::new()
        .plan_push(&shadow, &edited)
        .expect_err("directive edit should fail");

    let AfsError::Validation(issues) = error else {
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
