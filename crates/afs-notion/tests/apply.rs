use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;
use std::sync::Mutex;

use afs_connector::{ApplyPlanRequest, ApplyUndoRequest, Connector};
use afs_core::journal::{JournalApplyEffect, PushId, PushOperationId};
use afs_core::model::{EntityKind, MountId, RemoteId};
use afs_core::planner::{PropertyValue, PushOperation, PushOperationKind, PushPlan};
use afs_core::push::RemotePrecondition;
use afs_core::undo::{UndoOperation, UndoPlan, UndoPlanStatus};
use afs_core::{AfsError, AfsResult};
use afs_notion::client::NotionApi;
use afs_notion::dto::{
    BlockDto, BlockListDto, CodeBlockDto, ColorOnlyBlockDto, DataSourceDto, DataSourcePropertyDto,
    DataSourceSummaryDto, DatabaseDto, DateMentionDto, EquationBlockDto, ExternalFileDto,
    FileBlockDto, IdRefDto, LinkDto, LinkToPageBlockDto, MentionRichTextDto, PageDto, PageListDto,
    PagePropertyDto, PaginatedListDto, RichTextAnnotationsDto, RichTextBlockDto, RichTextDto,
    SelectOptionDto, TableBlockDto, TableRowBlockDto, TextRichTextDto, TitleBlockDto, ToDoBlockDto,
    UrlBlockDto,
};
use afs_notion::{NotionConfig, NotionConnector};
use serde_json::{Value, json};
use tempfile::tempdir;

#[test]
fn apply_updates_appends_and_archives_supported_blocks() {
    let api = Arc::new(RecordingNotionApi::new("2026-06-10T00:00:00.000Z", false));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![
            PushOperation::UpdateBlock {
                block_id: RemoteId::new("paragraph-1"),
                content: "Changed paragraph.".to_string(),
            },
            PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("paragraph-1")),
                content: "- New bullet".to_string(),
            },
            PushOperation::ArchiveBlock {
                block_id: RemoteId::new("old-block"),
            },
        ],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");
    let preconditions = vec![RemotePrecondition {
        remote_id: RemoteId::new("page-1"),
        remote_edited_at: Some("2026-06-10T00:00:00.000Z".to_string()),
    }];

    connector
        .check_concurrency(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &preconditions,
            local_root: None,
        })
        .expect("concurrency");
    let result = connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &preconditions,
            local_root: None,
        })
        .expect("apply");

    assert_eq!(result.changed_remote_ids, vec![RemoteId::new("page-1")]);
    assert_eq!(
        result.effects,
        vec![
            JournalApplyEffect::UpdatedBlock {
                operation_id: operation_ids[0].clone(),
                operation_index: 0,
                block_id: RemoteId::new("paragraph-1"),
            },
            JournalApplyEffect::CreatedBlock {
                operation_id: operation_ids[1].clone(),
                operation_index: 1,
                parent_id: RemoteId::new("page-1"),
                block_id: RemoteId::new("created-1"),
            },
            JournalApplyEffect::ArchivedBlock {
                operation_id: operation_ids[2].clone(),
                operation_index: 2,
                block_id: RemoteId::new("old-block"),
            },
        ]
    );

    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [
            WriteCall::Update {
                block_id: "paragraph-1".to_string(),
                body: json!({
                    "paragraph": {
                        "rich_text": rich_text_json("Changed paragraph."),
                    },
                }),
            },
            WriteCall::Append {
                block_id: "page-1".to_string(),
                body: json!({
                    "children": [{
                        "object": "block",
                        "type": "bulleted_list_item",
                        "bulleted_list_item": {
                            "rich_text": rich_text_json("New bullet"),
                        },
                    }],
                    "position": {
                        "type": "after_block",
                        "after_block": {
                            "id": "paragraph-1",
                        },
                    },
                }),
            },
            WriteCall::Delete {
                block_id: "old-block".to_string(),
            },
        ]
    );
}

#[test]
fn apply_replaces_block_by_appending_after_old_block_then_archiving_old_block() {
    let api = Arc::new(RecordingNotionApi::new("2026-06-10T00:00:00.000Z", false));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![
            PushOperation::ReplaceBlock {
                block_id: RemoteId::new("paragraph-1"),
                content: "- Replacement bullet".to_string(),
            },
            PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("paragraph-1")),
                content: "After replacement.".to_string(),
            },
        ],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    let result = connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect("apply");

    assert_eq!(result.changed_remote_ids, vec![RemoteId::new("page-1")]);
    assert_eq!(
        result.effects,
        vec![
            JournalApplyEffect::CreatedBlock {
                operation_id: operation_ids[0].clone(),
                operation_index: 0,
                parent_id: RemoteId::new("page-1"),
                block_id: RemoteId::new("created-1"),
            },
            JournalApplyEffect::ArchivedBlock {
                operation_id: operation_ids[0].clone(),
                operation_index: 0,
                block_id: RemoteId::new("paragraph-1"),
            },
            JournalApplyEffect::CreatedBlock {
                operation_id: operation_ids[1].clone(),
                operation_index: 1,
                parent_id: RemoteId::new("page-1"),
                block_id: RemoteId::new("created-2"),
            },
        ]
    );

    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [
            WriteCall::Append {
                block_id: "page-1".to_string(),
                body: json!({
                    "children": [{
                        "object": "block",
                        "type": "bulleted_list_item",
                        "bulleted_list_item": {
                            "rich_text": rich_text_json("Replacement bullet"),
                        },
                    }],
                    "position": {
                        "type": "after_block",
                        "after_block": {
                            "id": "paragraph-1",
                        },
                    },
                }),
            },
            WriteCall::Delete {
                block_id: "paragraph-1".to_string(),
            },
            WriteCall::Append {
                block_id: "page-1".to_string(),
                body: json!({
                    "children": [{
                        "object": "block",
                        "type": "paragraph",
                        "paragraph": {
                            "rich_text": rich_text_json("After replacement."),
                        },
                    }],
                    "position": {
                        "type": "after_block",
                        "after_block": {
                            "id": "created-1",
                        },
                    },
                }),
            },
        ]
    );
}

#[test]
fn apply_decodes_rendered_rich_text_line_breaks() {
    let api = Arc::new(RecordingNotionApi::new("2026-06-10T00:00:00.000Z", false));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![PushOperation::UpdateBlock {
            block_id: RemoteId::new("paragraph-1"),
            content: "First line.<br><br># Still paragraph text<br>- Also text".to_string(),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect("apply");

    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [WriteCall::Update {
            block_id: "paragraph-1".to_string(),
            body: json!({
                "paragraph": {
                    "rich_text": rich_text_json("First line.\n\n# Still paragraph text\n- Also text"),
                },
            }),
        }]
    );
}

#[test]
fn apply_uses_start_position_and_chains_adjacent_new_blocks() {
    let api = Arc::new(RecordingNotionApi::new("2026-06-10T00:00:00.000Z", false));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![
            PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: None,
                content: "First new paragraph.".to_string(),
            },
            PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: None,
                content: "Second new paragraph.".to_string(),
            },
        ],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect("apply");

    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes[0],
        WriteCall::Append {
            block_id: "page-1".to_string(),
            body: json!({
                "children": [{
                    "object": "block",
                    "type": "paragraph",
                    "paragraph": {
                        "rich_text": rich_text_json("First new paragraph."),
                    },
                }],
                "position": {
                    "type": "start",
                },
            }),
        }
    );
    assert_eq!(
        writes[1],
        WriteCall::Append {
            block_id: "page-1".to_string(),
            body: json!({
                "children": [{
                    "object": "block",
                    "type": "paragraph",
                    "paragraph": {
                        "rich_text": rich_text_json("Second new paragraph."),
                    },
                }],
                "position": {
                    "type": "after_block",
                    "after_block": {
                        "id": "created-1",
                    },
                },
            }),
        }
    );
}

#[test]
fn apply_normalizes_append_after_nested_block_to_direct_child_ancestor() {
    let mut callout = callout_block("callout-1", "Notion Tip");
    callout.has_children = true;
    let mut api = RecordingNotionApi::with_blocks(
        "2026-06-10T00:00:00.000Z",
        vec![paragraph_block("paragraph-1", "Intro.", false), callout],
    );
    api.children.insert(
        ("callout-1".to_string(), None),
        PaginatedListDto {
            results: vec![
                paragraph_block("nested-1", "Nested one.", false),
                paragraph_block("nested-2", "Nested two.", false),
            ],
            next_cursor: None,
            has_more: false,
        },
    );
    let api = Arc::new(api);
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![PushOperation::AppendBlock {
            parent_id: RemoteId::new("page-1"),
            after: Some(RemoteId::new("nested-2")),
            content: "Appended at page level.".to_string(),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect("apply");

    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [WriteCall::Append {
            block_id: "page-1".to_string(),
            body: json!({
                "children": [{
                    "object": "block",
                    "type": "paragraph",
                    "paragraph": {
                        "rich_text": rich_text_json("Appended at page level."),
                    },
                }],
                "position": {
                    "type": "after_block",
                    "after_block": {
                        "id": "callout-1",
                    },
                },
            }),
        }]
    );
}

#[test]
fn apply_appends_tier_one_markdown_block_shapes() {
    let api = Arc::new(RecordingNotionApi::new("2026-06-10T00:00:00.000Z", false));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![
            PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("anchor-1")),
                content: "## Section".to_string(),
            },
            PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("anchor-2")),
                content: "1. Numbered".to_string(),
            },
            PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("anchor-3")),
                content: "- [x] Done".to_string(),
            },
            PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("anchor-4")),
                content: "> Quoted\n> Text".to_string(),
            },
            PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("anchor-5")),
                content: "```rust\nfn main() {}\n```".to_string(),
            },
            PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("anchor-6")),
                content: "---".to_string(),
            },
        ],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect("apply");

    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [
            append_call(
                "anchor-1",
                json!({
                    "object": "block",
                    "type": "heading_2",
                    "heading_2": {
                        "rich_text": rich_text_json("Section"),
                    },
                }),
            ),
            append_call(
                "anchor-2",
                json!({
                    "object": "block",
                    "type": "numbered_list_item",
                    "numbered_list_item": {
                        "rich_text": rich_text_json("Numbered"),
                    },
                }),
            ),
            append_call(
                "anchor-3",
                json!({
                    "object": "block",
                    "type": "to_do",
                    "to_do": {
                        "rich_text": rich_text_json("Done"),
                        "checked": true,
                    },
                }),
            ),
            append_call(
                "anchor-4",
                json!({
                    "object": "block",
                    "type": "quote",
                    "quote": {
                        "rich_text": rich_text_json("Quoted\nText"),
                    },
                }),
            ),
            append_call(
                "anchor-5",
                json!({
                    "object": "block",
                    "type": "code",
                    "code": {
                        "rich_text": rich_text_json("fn main() {}"),
                        "language": "rust",
                    },
                }),
            ),
            append_call(
                "anchor-6",
                json!({
                    "object": "block",
                    "type": "divider",
                    "divider": {},
                }),
            ),
        ]
    );
}

#[test]
fn apply_parses_long_code_fences_from_rendered_code_blocks() {
    let api = Arc::new(RecordingNotionApi::with_blocks(
        "2026-06-10T00:00:00.000Z",
        vec![code_block(
            "code-1",
            "markdown",
            "Before\n```python\nprint('nested')\n```\nAfter",
        )],
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![PushOperation::UpdateBlock {
            block_id: RemoteId::new("code-1"),
            content: "````markdown\nBefore\n```python\nprint('nested')\n```\nAfter updated\n````"
                .to_string(),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect("apply");

    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [WriteCall::Update {
            block_id: "code-1".to_string(),
            body: json!({
                "code": {
                    "rich_text": rich_text_json("Before\n```python\nprint('nested')\n```\nAfter updated"),
                    "language": "markdown",
                },
            }),
        }]
    );
}

#[test]
fn apply_moves_existing_blocks() {
    let api = Arc::new(RecordingNotionApi::with_blocks(
        "2026-06-10T00:00:00.000Z",
        vec![
            paragraph_block("paragraph-1", "First.", false),
            paragraph_block("paragraph-2", "Second.", false),
            table_of_contents_block("directive-1"),
        ],
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![
            PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("paragraph-1")),
                content: "Inserted paragraph.".to_string(),
            },
            PushOperation::MoveBlock {
                block_id: RemoteId::new("directive-1"),
                after: Some(RemoteId::new("paragraph-1")),
            },
        ],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    assert!(
        connector
            .supported_push_operations()
            .contains(&PushOperationKind::MoveBlock)
    );

    let result = connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect("apply");

    assert_eq!(result.changed_remote_ids, vec![RemoteId::new("page-1")]);
    assert_eq!(
        result.effects,
        vec![
            JournalApplyEffect::CreatedBlock {
                operation_id: operation_ids[0].clone(),
                operation_index: 0,
                parent_id: RemoteId::new("page-1"),
                block_id: RemoteId::new("created-1"),
            },
            JournalApplyEffect::CreatedBlock {
                operation_id: operation_ids[1].clone(),
                operation_index: 1,
                parent_id: RemoteId::new("page-1"),
                block_id: RemoteId::new("created-2"),
            },
            JournalApplyEffect::ArchivedBlock {
                operation_id: operation_ids[1].clone(),
                operation_index: 1,
                block_id: RemoteId::new("directive-1"),
            },
        ]
    );
    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        *writes,
        vec![
            append_call(
                "paragraph-1",
                json!({
                    "object": "block",
                    "type": "paragraph",
                    "paragraph": {
                        "rich_text": rich_text_json("Inserted paragraph."),
                    },
                }),
            ),
            append_call(
                "created-1",
                json!({
                    "object": "block",
                    "type": "table_of_contents",
                    "table_of_contents": {
                        "color": Value::Null,
                    },
                }),
            ),
            WriteCall::Delete {
                block_id: "directive-1".to_string(),
            },
        ]
    );
}

#[test]
fn apply_updates_equation_blocks_from_display_math() {
    let api = Arc::new(RecordingNotionApi::with_blocks(
        "2026-06-10T00:00:00.000Z",
        vec![equation_block("equation-1", "E=mc^2")],
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![PushOperation::UpdateBlock {
            block_id: RemoteId::new("equation-1"),
            content: "$$\nF=ma\n$$".to_string(),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect("apply");

    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [WriteCall::Update {
            block_id: "equation-1".to_string(),
            body: json!({
                "equation": {
                    "expression": "F=ma",
                },
            }),
        }]
    );
}

#[test]
fn apply_updates_toggle_summary_from_rendered_list_item() {
    let api = Arc::new(RecordingNotionApi::with_blocks(
        "2026-06-10T00:00:00.000Z",
        vec![toggle_block("toggle-1", "Old toggle")],
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![PushOperation::UpdateBlock {
            block_id: RemoteId::new("toggle-1"),
            content: "- Updated toggle".to_string(),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect("apply");

    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [WriteCall::Update {
            block_id: "toggle-1".to_string(),
            body: json!({
                "toggle": {
                    "rich_text": rich_text_json("Updated toggle"),
                },
            }),
        }]
    );
}

#[test]
fn apply_updates_todo_from_empty_checkbox_shorthand() {
    let api = Arc::new(RecordingNotionApi::with_blocks(
        "2026-06-10T00:00:00.000Z",
        vec![to_do_block("todo-1", "Call Mom", true)],
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![PushOperation::UpdateBlock {
            block_id: RemoteId::new("todo-1"),
            content: "- [] Call Mom".to_string(),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect("apply");

    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [WriteCall::Update {
            block_id: "todo-1".to_string(),
            body: json!({
                "to_do": {
                    "rich_text": rich_text_json("Call Mom"),
                    "checked": false,
                },
            }),
        }]
    );
}

#[test]
fn apply_updates_and_appends_callouts_from_markdown() {
    let api = Arc::new(RecordingNotionApi::with_blocks(
        "2026-06-10T00:00:00.000Z",
        vec![callout_block("callout-1", "Old callout")],
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![
            PushOperation::UpdateBlock {
                block_id: RemoteId::new("callout-1"),
                content: "> [!NOTE]\n> Updated callout".to_string(),
            },
            PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("callout-1")),
                content: "> [!NOTE]\n> New callout".to_string(),
            },
        ],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect("apply");

    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [
            WriteCall::Update {
                block_id: "callout-1".to_string(),
                body: json!({
                    "callout": {
                        "rich_text": rich_text_json("Updated callout"),
                    },
                }),
            },
            WriteCall::Append {
                block_id: "page-1".to_string(),
                body: json!({
                    "children": [{
                        "object": "block",
                        "type": "callout",
                        "callout": {
                            "rich_text": rich_text_json("New callout"),
                        },
                    }],
                    "position": {
                        "type": "after_block",
                        "after_block": {
                            "id": "callout-1",
                        },
                    },
                }),
            },
        ]
    );
}

#[test]
fn check_concurrency_rejects_remote_timestamp_mismatch() {
    let api = Arc::new(RecordingNotionApi::new("2026-06-10T01:00:00.000Z", false));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(vec![RemoteId::new("page-1")], Vec::new());
    let push_id = PushId("push-1".to_string());
    let mount_id = MountId::new("notion-main");
    let preconditions = vec![RemotePrecondition {
        remote_id: RemoteId::new("page-1"),
        remote_edited_at: Some("2026-06-10T00:00:00.000Z".to_string()),
    }];

    let error = connector
        .check_concurrency(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &[],
            remote_preconditions: &preconditions,
            local_root: None,
        })
        .expect_err("stale remote");

    assert!(matches!(error, AfsError::Guardrail(_)));
}

#[test]
fn check_concurrency_uses_database_metadata_for_row_create_parent() {
    let api = Arc::new(RecordingNotionApi::new("2026-06-10T00:00:00.000Z", false));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("database-1")],
        vec![PushOperation::CreateEntity {
            parent_id: RemoteId::new("database-1"),
            parent_kind: Some(EntityKind::Database),
            title: "New row".to_string(),
            properties: BTreeMap::new(),
            body: String::new(),
            source_path: "Rows/new-row.md".into(),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let mount_id = MountId::new("notion-main");
    let preconditions = vec![RemotePrecondition {
        remote_id: RemoteId::new("database-1"),
        remote_edited_at: Some("2026-06-10T00:00:00.000Z".to_string()),
    }];

    connector
        .check_concurrency(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &[],
            remote_preconditions: &preconditions,
            local_root: None,
        })
        .expect("database parent concurrency check");
}

#[test]
fn apply_preserves_unchanged_mentions_and_parses_edited_rich_spans() {
    let api = Arc::new(RecordingNotionApi::with_paragraph_rich_text(
        "2026-06-10T00:00:00.000Z",
        vec![
            annotated_text("Bold", |annotations| annotations.bold = true),
            rich_text_part(" and "),
            date_mention("2026-06-10", "2026-06-10"),
            rich_text_part(" plus "),
            linked_text("Docs", "https://example.com/"),
            rich_text_part(" and database "),
            database_mention("Tasks", "33333333-3333-3333-3333-333333333333"),
            rich_text_part("."),
        ],
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![PushOperation::UpdateBlock {
            block_id: RemoteId::new("paragraph-1"),
            content: "**Boldly** and 2026-06-10 plus [Docs](https://example.com/) and database [Tasks updated](https://www.notion.so/33333333333333333333333333333333) and @date(2026-06-11 to 2026-06-12, tz=America/Chicago) and @user(Ada <55555555-5555-5555-5555-555555555555>) and @page(Roadmap <44444444-4444-4444-4444-444444444444>) and @database(66666666666666666666666666666666) and $E=mc^2$ [Hex docs](https://example.com/22222222222222222222222222222222) [Roadmap](https://www.notion.so/Project-22222222222222222222222222222222)".to_string(),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    let result = connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect("apply");

    assert_eq!(result.changed_remote_ids, vec![RemoteId::new("page-1")]);
    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [WriteCall::Update {
            block_id: "paragraph-1".to_string(),
            body: json!({
                "paragraph": {
                    "rich_text": [
                        {
                            "type": "text",
                            "text": {
                                "content": "Boldly",
                            },
                            "annotations": {
                                "bold": true,
                                "italic": false,
                                "strikethrough": false,
                                "underline": false,
                                "code": false,
                                "color": "default",
                            },
                        },
                        {
                            "type": "text",
                            "text": {
                                "content": " and ",
                            },
                        },
                        {
                            "type": "mention",
                            "mention": {
                                "type": "date",
                                "date": {
                                    "start": "2026-06-10",
                                },
                            },
                        },
                        {
                            "type": "text",
                            "text": {
                                "content": " plus ",
                            },
                        },
                        {
                            "type": "text",
                            "text": {
                                "content": "Docs",
                                "link": {
                                    "url": "https://example.com/",
                                },
                            },
                        },
                        {
                            "type": "text",
                            "text": {
                                "content": " and database ",
                            },
                        },
                        {
                            "type": "mention",
                            "mention": {
                                "type": "database",
                                "database": {
                                    "id": "33333333333333333333333333333333",
                                },
                            },
                        },
                        {
                            "type": "text",
                            "text": {
                                "content": " and ",
                            },
                        },
                        {
                            "type": "mention",
                            "mention": {
                                "type": "date",
                                "date": {
                                    "start": "2026-06-11",
                                    "end": "2026-06-12",
                                    "time_zone": "America/Chicago",
                                },
                            },
                        },
                        {
                            "type": "text",
                            "text": {
                                "content": " and ",
                            },
                        },
                        {
                            "type": "mention",
                            "mention": {
                                "type": "user",
                                "user": {
                                    "id": "55555555-5555-5555-5555-555555555555",
                                },
                            },
                        },
                        {
                            "type": "text",
                            "text": {
                                "content": " and ",
                            },
                        },
                        {
                            "type": "mention",
                            "mention": {
                                "type": "page",
                                "page": {
                                    "id": "44444444-4444-4444-4444-444444444444",
                                },
                            },
                        },
                        {
                            "type": "text",
                            "text": {
                                "content": " and ",
                            },
                        },
                        {
                            "type": "mention",
                            "mention": {
                                "type": "database",
                                "database": {
                                    "id": "66666666666666666666666666666666",
                                },
                            },
                        },
                        {
                            "type": "text",
                            "text": {
                                "content": " and ",
                            },
                        },
                        {
                            "type": "equation",
                            "equation": {
                                "expression": "E=mc^2",
                            },
                        },
                        {
                            "type": "text",
                            "text": {
                                "content": " ",
                            },
                        },
                        {
                            "type": "text",
                            "text": {
                                "content": "Hex docs",
                                "link": {
                                    "url": "https://example.com/22222222222222222222222222222222",
                                },
                            },
                        },
                        {
                            "type": "text",
                            "text": {
                                "content": " ",
                            },
                        },
                        {
                            "type": "mention",
                            "mention": {
                                "type": "page",
                                "page": {
                                    "id": "22222222222222222222222222222222",
                                },
                            },
                        },
                    ],
                },
            }),
        }]
    );
}

#[test]
fn apply_rejects_link_to_page_retargeting_before_api_mutation() {
    let api = Arc::new(RecordingNotionApi::with_blocks(
        "2026-06-10T00:00:00.000Z",
        vec![link_to_page_block(
            "page-link-1",
            "page_id",
            "11111111-1111-1111-1111-111111111111",
        )],
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![PushOperation::UpdateBlock {
            block_id: RemoteId::new("page-link-1"),
            content:
                "[Updated page](https://www.notion.so/Project-22222222222222222222222222222222)"
                    .to_string(),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    let error = connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect_err("link_to_page retargeting is intentionally unsupported");

    assert!(
        matches!(error, AfsError::Unsupported(message) if message.contains("link_to_page")),
        "{error:?}"
    );
    assert!(api.writes.lock().expect("writes").is_empty());
}

#[test]
fn apply_preserves_link_to_page_when_native_move_is_planned_as_append_archive() {
    let target_id = "11111111-1111-1111-1111-111111111111";
    let api = Arc::new(RecordingNotionApi::with_blocks(
        "2026-06-10T00:00:00.000Z",
        vec![
            paragraph_block("paragraph-1", "Anchor.", false),
            link_to_page_block("page-link-1", "page_id", target_id),
        ],
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![
            PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: None,
                content: "[Linked page](https://www.notion.so/11111111111111111111111111111111)"
                    .to_string(),
            },
            PushOperation::ArchiveBlock {
                block_id: RemoteId::new("page-link-1"),
            },
        ],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    let result = connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect("apply");

    assert_eq!(
        result.effects,
        vec![
            JournalApplyEffect::CreatedBlock {
                operation_id: operation_ids[0].clone(),
                operation_index: 0,
                parent_id: RemoteId::new("page-1"),
                block_id: RemoteId::new("created-1"),
            },
            JournalApplyEffect::ArchivedBlock {
                operation_id: operation_ids[1].clone(),
                operation_index: 1,
                block_id: RemoteId::new("page-link-1"),
            },
        ]
    );
    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        *writes,
        vec![
            WriteCall::Append {
                block_id: "page-1".to_string(),
                body: json!({
                    "children": [{
                        "object": "block",
                        "type": "link_to_page",
                        "link_to_page": {
                            "type": "page_id",
                            "page_id": target_id,
                        },
                    }],
                    "position": {
                        "type": "start",
                    },
                }),
            },
            WriteCall::Delete {
                block_id: "page-link-1".to_string(),
            },
        ]
    );
}

#[test]
fn apply_undo_restores_archived_block_by_appending_replacement() {
    let api = Arc::new(RecordingNotionApi::with_blocks(
        "2026-06-10T00:00:00.000Z",
        vec![paragraph_block("paragraph-1", "Old paragraph.", false)],
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let push_id = PushId("push-1".to_string());
    let mount_id = MountId::new("notion-main");
    let undo_plan = UndoPlan {
        target_push_id: push_id.clone(),
        mount_id: mount_id.clone(),
        affected_entities: vec![RemoteId::new("page-1")],
        operations: vec![UndoOperation::RestoreArchivedBlock {
            block_id: RemoteId::new("paragraph-1"),
            parent_id: RemoteId::new("page-1"),
            after: None,
            content: "Old paragraph.".to_string(),
            native_kind: None,
        }],
        unsupported: vec![],
        status: UndoPlanStatus::Complete,
    };

    let result = connector
        .apply_undo(ApplyUndoRequest {
            target_push_id: &push_id,
            mount_id: &mount_id,
            plan: &undo_plan,
        })
        .expect("apply undo");

    assert_eq!(result.changed_remote_ids, vec![RemoteId::new("page-1")]);
    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        *writes,
        vec![WriteCall::Append {
            block_id: "page-1".to_string(),
            body: json!({
                "children": [{
                    "object": "block",
                    "type": "paragraph",
                    "paragraph": {
                        "rich_text": rich_text_json("Old paragraph."),
                    },
                }],
                "position": {
                    "type": "start",
                },
            }),
        }]
    );
}

#[test]
fn apply_undo_restores_archived_paragraph_link_labeled_like_link_to_page_as_paragraph() {
    let target_id = "11111111111111111111111111111111";
    let api = Arc::new(RecordingNotionApi::with_blocks(
        "2026-06-10T00:00:00.000Z",
        vec![paragraph_block("paragraph-1", "Anchor.", false)],
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let push_id = PushId("push-1".to_string());
    let mount_id = MountId::new("notion-main");
    let undo_plan = UndoPlan {
        target_push_id: push_id.clone(),
        mount_id: mount_id.clone(),
        affected_entities: vec![RemoteId::new("page-1")],
        operations: vec![UndoOperation::RestoreArchivedBlock {
            block_id: RemoteId::new("paragraph-1"),
            parent_id: RemoteId::new("page-1"),
            after: None,
            content: format!("[Linked page](https://www.notion.so/{target_id})"),
            native_kind: Some("paragraph".to_string()),
        }],
        unsupported: vec![],
        status: UndoPlanStatus::Complete,
    };

    connector
        .apply_undo(ApplyUndoRequest {
            target_push_id: &push_id,
            mount_id: &mount_id,
            plan: &undo_plan,
        })
        .expect("apply undo");

    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        *writes,
        vec![WriteCall::Append {
            block_id: "page-1".to_string(),
            body: json!({
                "children": [{
                    "object": "block",
                    "type": "paragraph",
                    "paragraph": {
                        "rich_text": [{
                            "type": "mention",
                            "mention": {
                                "type": "page",
                                "page": { "id": target_id },
                            },
                        }],
                    },
                }],
                "position": {
                    "type": "start",
                },
            }),
        }]
    );
}

#[test]
fn apply_undo_restores_archived_directive_by_appending_replacement() {
    let api = Arc::new(RecordingNotionApi::with_blocks(
        "2026-06-10T00:00:00.000Z",
        vec![paragraph_block("paragraph-1", "Anchor.", false)],
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let push_id = PushId("push-1".to_string());
    let mount_id = MountId::new("notion-main");
    let undo_plan = UndoPlan {
        target_push_id: push_id.clone(),
        mount_id: mount_id.clone(),
        affected_entities: vec![RemoteId::new("page-1")],
        operations: vec![UndoOperation::RestoreArchivedBlock {
            block_id: RemoteId::new("toc-1"),
            parent_id: RemoteId::new("page-1"),
            after: Some(RemoteId::new("paragraph-1")),
            content: "::afs{id=toc-1 type=table_of_contents color=\"default\"}".to_string(),
            native_kind: None,
        }],
        unsupported: vec![],
        status: UndoPlanStatus::Complete,
    };

    connector
        .apply_undo(ApplyUndoRequest {
            target_push_id: &push_id,
            mount_id: &mount_id,
            plan: &undo_plan,
        })
        .expect("apply undo");

    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        *writes,
        vec![append_call(
            "paragraph-1",
            json!({
                "object": "block",
                "type": "table_of_contents",
                "table_of_contents": {
                    "color": "default",
                },
            }),
        )]
    );
}

#[test]
fn apply_rejects_child_page_link_edits_before_api_mutation() {
    let api = Arc::new(RecordingNotionApi::with_blocks(
        "2026-06-10T00:00:00.000Z",
        vec![child_page_block("child-page-1", "Child Page")],
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![PushOperation::UpdateBlock {
            block_id: RemoteId::new("child-page-1"),
            content: "[Renamed child](https://www.notion.so/child-page-1)".to_string(),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    let error = connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect_err("child_page link edits are intentionally unsupported");

    assert!(
        matches!(error, AfsError::Unsupported(message) if message.contains("child_page")),
        "{error:?}"
    );
    assert!(api.writes.lock().expect("writes").is_empty());
}

#[test]
fn apply_updates_bookmark_and_embed_blocks_from_markdown_links() {
    let api = Arc::new(RecordingNotionApi::with_blocks(
        "2026-06-10T00:00:00.000Z",
        vec![
            url_block(
                "bookmark-1",
                "bookmark",
                "https://example.com/original-bookmark",
                "Original bookmark",
            ),
            url_block(
                "embed-1",
                "embed",
                "https://example.com/original-embed",
                "Original embed",
            ),
        ],
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![
            PushOperation::UpdateBlock {
                block_id: RemoteId::new("bookmark-1"),
                content: "[Updated bookmark](https://example.com/updated-bookmark)".to_string(),
            },
            PushOperation::UpdateBlock {
                block_id: RemoteId::new("embed-1"),
                content: "[Updated embed](https://example.com/updated-embed)".to_string(),
            },
        ],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect("apply URL block updates");

    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [
            WriteCall::Update {
                block_id: "bookmark-1".to_string(),
                body: json!({
                    "bookmark": {
                        "url": "https://example.com/updated-bookmark",
                        "caption": rich_text_json("Updated bookmark"),
                    },
                }),
            },
            WriteCall::Update {
                block_id: "embed-1".to_string(),
                body: json!({
                    "embed": {
                        "url": "https://example.com/updated-embed",
                        "caption": rich_text_json("Updated embed"),
                    },
                }),
            },
        ]
    );
}

#[test]
fn apply_unescapes_rendered_link_labels_before_url_and_media_updates() {
    let api = Arc::new(RecordingNotionApi::with_blocks(
        "2026-06-10T00:00:00.000Z",
        vec![
            url_block(
                "bookmark-1",
                "bookmark",
                "https://example.com/original-bookmark",
                "Original [bookmark](draft)",
            ),
            media_block(
                "image-1",
                "image",
                "https://example.com/original-image.png",
                "Original [image](draft)",
            ),
        ],
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![
            PushOperation::UpdateBlock {
                block_id: RemoteId::new("bookmark-1"),
                content: "[Original \\[bookmark\\](draft)](https://example.com/updated-bookmark)"
                    .to_string(),
            },
            PushOperation::UpdateBlock {
                block_id: RemoteId::new("image-1"),
                content: "![Original \\[image\\](draft)](https://example.com/updated-image.png)"
                    .to_string(),
            },
        ],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect("apply escaped link-label updates");

    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [
            WriteCall::Update {
                block_id: "bookmark-1".to_string(),
                body: json!({
                    "bookmark": {
                        "url": "https://example.com/updated-bookmark",
                        "caption": rich_text_json("Original [bookmark](draft)"),
                    },
                }),
            },
            WriteCall::Update {
                block_id: "image-1".to_string(),
                body: json!({
                    "image": {
                        "external": {
                            "url": "https://example.com/updated-image.png",
                        },
                        "caption": rich_text_json("Original [image](draft)"),
                    },
                }),
            },
        ]
    );
}

#[test]
fn apply_updates_external_media_blocks_from_markdown_links() {
    let api = Arc::new(RecordingNotionApi::with_blocks(
        "2026-06-10T00:00:00.000Z",
        vec![
            media_block(
                "image-1",
                "image",
                "https://example.com/original-image.png",
                "Original image",
            ),
            media_block(
                "video-1",
                "video",
                "https://example.com/original-video.mp4",
                "Original video",
            ),
            media_block(
                "file-1",
                "file",
                "https://example.com/original-file.pdf",
                "Original file",
            ),
            media_block(
                "pdf-1",
                "pdf",
                "https://example.com/original.pdf",
                "Original PDF",
            ),
            media_block(
                "audio-1",
                "audio",
                "https://example.com/original-audio.mp3",
                "Original audio",
            ),
        ],
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![
            PushOperation::UpdateBlock {
                block_id: RemoteId::new("image-1"),
                content: "![Updated image](https://example.com/updated-image.png)".to_string(),
            },
            PushOperation::UpdateBlock {
                block_id: RemoteId::new("video-1"),
                content: "[Updated video](https://example.com/updated-video.mp4)".to_string(),
            },
            PushOperation::UpdateBlock {
                block_id: RemoteId::new("file-1"),
                content: "[Updated file](https://example.com/updated-file.pdf)".to_string(),
            },
            PushOperation::UpdateBlock {
                block_id: RemoteId::new("pdf-1"),
                content: "[Updated PDF](https://example.com/updated.pdf)".to_string(),
            },
            PushOperation::UpdateBlock {
                block_id: RemoteId::new("audio-1"),
                content: "[Updated audio](https://example.com/updated-audio.mp3)".to_string(),
            },
        ],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect("apply external media block updates");

    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [
            WriteCall::Update {
                block_id: "image-1".to_string(),
                body: json!({
                    "image": {
                        "external": {
                            "url": "https://example.com/updated-image.png",
                        },
                        "caption": rich_text_json("Updated image"),
                    },
                }),
            },
            WriteCall::Update {
                block_id: "video-1".to_string(),
                body: json!({
                    "video": {
                        "external": {
                            "url": "https://example.com/updated-video.mp4",
                        },
                        "caption": rich_text_json("Updated video"),
                    },
                }),
            },
            WriteCall::Update {
                block_id: "file-1".to_string(),
                body: json!({
                    "file": {
                        "external": {
                            "url": "https://example.com/updated-file.pdf",
                        },
                        "caption": rich_text_json("Updated file"),
                    },
                }),
            },
            WriteCall::Update {
                block_id: "pdf-1".to_string(),
                body: json!({
                    "pdf": {
                        "external": {
                            "url": "https://example.com/updated.pdf",
                        },
                        "caption": rich_text_json("Updated PDF"),
                    },
                }),
            },
            WriteCall::Update {
                block_id: "audio-1".to_string(),
                body: json!({
                    "audio": {
                        "external": {
                            "url": "https://example.com/updated-audio.mp3",
                        },
                        "caption": rich_text_json("Updated audio"),
                    },
                }),
            },
        ]
    );
}

#[test]
fn apply_uploads_local_image_media_before_block_update() {
    let temp = tempdir().expect("tempdir");
    let media_path = temp.path().join(".afs/media/Roadmap/image-1.png");
    fs::create_dir_all(media_path.parent().expect("parent")).expect("mkdir");
    fs::write(&media_path, b"new image bytes").expect("write image");
    let api = Arc::new(RecordingNotionApi::with_blocks(
        "2026-06-10T00:00:00.000Z",
        vec![media_block(
            "image-1",
            "image",
            "https://example.com/original-image.png",
            "Original image",
        )],
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![PushOperation::UpdateMedia {
            block_id: RemoteId::new("image-1"),
            local_path: ".afs/media/Roadmap/image-1.png".into(),
            caption: "Updated local image".to_string(),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: Some(temp.path()),
        })
        .expect("apply local media upload");

    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [
            WriteCall::UploadFile {
                filename: "image-1.png".to_string(),
                content_type: "image/png".to_string(),
                bytes: b"new image bytes".to_vec(),
            },
            WriteCall::Update {
                block_id: "image-1".to_string(),
                body: json!({
                    "image": {
                        "file_upload": {
                            "id": "upload-1",
                        },
                        "caption": rich_text_json("Updated local image"),
                    },
                }),
            },
        ]
    );
}

#[test]
fn apply_uploads_local_file_like_media_before_block_update() {
    let temp = tempdir().expect("tempdir");
    let video_path = temp.path().join(".afs/media/Roadmap/cars.mp4");
    let pdf_path = temp.path().join(".afs/media/Roadmap/brief.pdf");
    let audio_path = temp.path().join(".afs/media/Roadmap/theme.mp3");
    let html_path = temp.path().join(".afs/media/Roadmap/index.html");
    fs::create_dir_all(video_path.parent().expect("parent")).expect("mkdir");
    fs::write(&video_path, b"video bytes").expect("write video");
    fs::write(&pdf_path, b"pdf bytes").expect("write pdf");
    fs::write(&audio_path, b"audio bytes").expect("write audio");
    fs::write(&html_path, b"html bytes").expect("write html");
    let api = Arc::new(RecordingNotionApi::with_blocks(
        "2026-06-10T00:00:00.000Z",
        vec![
            media_block(
                "video-1",
                "video",
                "https://example.com/original-video.mp4",
                "Original video",
            ),
            media_block(
                "pdf-1",
                "pdf",
                "https://example.com/original.pdf",
                "Original PDF",
            ),
            media_block(
                "audio-1",
                "audio",
                "https://example.com/original-audio.mp3",
                "Original audio",
            ),
            media_block(
                "file-1",
                "file",
                "https://example.com/original.html",
                "Original file",
            ),
        ],
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![
            PushOperation::UpdateMedia {
                block_id: RemoteId::new("video-1"),
                local_path: ".afs/media/Roadmap/cars.mp4".into(),
                caption: "Updated video".to_string(),
            },
            PushOperation::UpdateMedia {
                block_id: RemoteId::new("pdf-1"),
                local_path: ".afs/media/Roadmap/brief.pdf".into(),
                caption: "Updated PDF".to_string(),
            },
            PushOperation::UpdateMedia {
                block_id: RemoteId::new("audio-1"),
                local_path: ".afs/media/Roadmap/theme.mp3".into(),
                caption: "Updated audio".to_string(),
            },
            PushOperation::UpdateMedia {
                block_id: RemoteId::new("file-1"),
                local_path: ".afs/media/Roadmap/index.html".into(),
                caption: "Updated file".to_string(),
            },
        ],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: Some(temp.path()),
        })
        .expect("apply local file-like media upload");

    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [
            WriteCall::UploadFile {
                filename: "cars.mp4".to_string(),
                content_type: "video/mp4".to_string(),
                bytes: b"video bytes".to_vec(),
            },
            WriteCall::Update {
                block_id: "video-1".to_string(),
                body: json!({
                    "video": {
                        "file_upload": {
                            "id": "upload-1",
                        },
                        "caption": rich_text_json("Updated video"),
                    },
                }),
            },
            WriteCall::UploadFile {
                filename: "brief.pdf".to_string(),
                content_type: "application/pdf".to_string(),
                bytes: b"pdf bytes".to_vec(),
            },
            WriteCall::Update {
                block_id: "pdf-1".to_string(),
                body: json!({
                    "pdf": {
                        "file_upload": {
                            "id": "upload-1",
                        },
                        "caption": rich_text_json("Updated PDF"),
                    },
                }),
            },
            WriteCall::UploadFile {
                filename: "theme.mp3".to_string(),
                content_type: "audio/mpeg".to_string(),
                bytes: b"audio bytes".to_vec(),
            },
            WriteCall::Update {
                block_id: "audio-1".to_string(),
                body: json!({
                    "audio": {
                        "file_upload": {
                            "id": "upload-1",
                        },
                        "caption": rich_text_json("Updated audio"),
                    },
                }),
            },
            WriteCall::UploadFile {
                filename: "index.html".to_string(),
                content_type: "text/html".to_string(),
                bytes: b"html bytes".to_vec(),
            },
            WriteCall::Update {
                block_id: "file-1".to_string(),
                body: json!({
                    "file": {
                        "file_upload": {
                            "id": "upload-1",
                        },
                        "caption": rich_text_json("Updated file"),
                    },
                }),
            },
        ]
    );
}

#[test]
fn apply_uploads_local_image_media_before_block_append() {
    let temp = tempdir().expect("tempdir");
    let media_path = temp.path().join(".afs/media/Roadmap/night-sky.jpg");
    fs::create_dir_all(media_path.parent().expect("parent")).expect("mkdir");
    fs::write(&media_path, b"new image bytes").expect("write image");
    let api = Arc::new(RecordingNotionApi::new("2026-06-10T00:00:00.000Z", false));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![PushOperation::AppendBlock {
            parent_id: RemoteId::new("page-1"),
            after: Some(RemoteId::new("paragraph-1")),
            content: format!("![night sky]({})", media_path.display()),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    let result = connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: Some(temp.path()),
        })
        .expect("apply local media append");

    assert_eq!(
        result.effects,
        vec![JournalApplyEffect::CreatedBlock {
            operation_id: operation_ids[0].clone(),
            operation_index: 0,
            parent_id: RemoteId::new("page-1"),
            block_id: RemoteId::new("created-1"),
        }]
    );
    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [
            WriteCall::UploadFile {
                filename: "night-sky.jpg".to_string(),
                content_type: "image/jpeg".to_string(),
                bytes: b"new image bytes".to_vec(),
            },
            WriteCall::Append {
                block_id: "page-1".to_string(),
                body: json!({
                    "children": [{
                        "object": "block",
                        "type": "image",
                        "image": {
                            "file_upload": {
                                "id": "upload-1",
                            },
                            "caption": rich_text_json("night sky"),
                        },
                    }],
                    "position": {
                        "type": "after_block",
                        "after_block": {
                            "id": "paragraph-1",
                        },
                    },
                }),
            },
        ]
    );
}

#[test]
fn apply_uploads_local_video_media_before_block_append() {
    let temp = tempdir().expect("tempdir");
    let media_path = temp.path().join(".afs/media/Roadmap/cars.mp4");
    fs::create_dir_all(media_path.parent().expect("parent")).expect("mkdir");
    fs::write(&media_path, b"new video bytes").expect("write video");
    let api = Arc::new(RecordingNotionApi::new("2026-06-10T00:00:00.000Z", false));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![PushOperation::AppendBlock {
            parent_id: RemoteId::new("page-1"),
            after: Some(RemoteId::new("paragraph-1")),
            content: format!("[cars]({})", media_path.display()),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    let result = connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: Some(temp.path()),
        })
        .expect("apply local video append");

    assert_eq!(
        result.effects,
        vec![JournalApplyEffect::CreatedBlock {
            operation_id: operation_ids[0].clone(),
            operation_index: 0,
            parent_id: RemoteId::new("page-1"),
            block_id: RemoteId::new("created-1"),
        }]
    );
    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [
            WriteCall::UploadFile {
                filename: "cars.mp4".to_string(),
                content_type: "video/mp4".to_string(),
                bytes: b"new video bytes".to_vec(),
            },
            WriteCall::Append {
                block_id: "page-1".to_string(),
                body: json!({
                    "children": [{
                        "object": "block",
                        "type": "video",
                        "video": {
                            "file_upload": {
                                "id": "upload-1",
                            },
                            "caption": rich_text_json("cars"),
                        },
                    }],
                    "position": {
                        "type": "after_block",
                        "after_block": {
                            "id": "paragraph-1",
                        },
                    },
                }),
            },
        ]
    );
}

#[test]
fn apply_uploads_common_local_file_media_before_block_append() {
    let temp = tempdir().expect("tempdir");
    let pdf_path = temp.path().join(".afs/media/Roadmap/brief.pdf");
    let audio_path = temp.path().join(".afs/media/Roadmap/theme.mp3");
    let html_path = temp.path().join(".afs/media/Roadmap/index.html");
    let slides_path = temp.path().join(".afs/media/Roadmap/slides.pptx");
    fs::create_dir_all(pdf_path.parent().expect("parent")).expect("mkdir");
    fs::write(&pdf_path, b"pdf bytes").expect("write pdf");
    fs::write(&audio_path, b"audio bytes").expect("write audio");
    fs::write(&html_path, b"html bytes").expect("write html");
    fs::write(&slides_path, b"slides bytes").expect("write slides");
    let api = Arc::new(RecordingNotionApi::new("2026-06-10T00:00:00.000Z", false));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![
            PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("paragraph-1")),
                content: format!("[Brief]({})", pdf_path.display()),
            },
            PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("paragraph-1")),
                content: format!("[Theme]({})", audio_path.display()),
            },
            PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("paragraph-1")),
                content: format!("[Index]({})", html_path.display()),
            },
            PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("paragraph-1")),
                content: format!("[Slides]({})", slides_path.display()),
            },
        ],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    let result = connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: Some(temp.path()),
        })
        .expect("apply local file media append");

    assert_eq!(
        result.effects,
        vec![
            JournalApplyEffect::CreatedBlock {
                operation_id: operation_ids[0].clone(),
                operation_index: 0,
                parent_id: RemoteId::new("page-1"),
                block_id: RemoteId::new("created-1"),
            },
            JournalApplyEffect::CreatedBlock {
                operation_id: operation_ids[1].clone(),
                operation_index: 1,
                parent_id: RemoteId::new("page-1"),
                block_id: RemoteId::new("created-2"),
            },
            JournalApplyEffect::CreatedBlock {
                operation_id: operation_ids[2].clone(),
                operation_index: 2,
                parent_id: RemoteId::new("page-1"),
                block_id: RemoteId::new("created-3"),
            },
            JournalApplyEffect::CreatedBlock {
                operation_id: operation_ids[3].clone(),
                operation_index: 3,
                parent_id: RemoteId::new("page-1"),
                block_id: RemoteId::new("created-4"),
            },
        ]
    );
    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [
            WriteCall::UploadFile {
                filename: "brief.pdf".to_string(),
                content_type: "application/pdf".to_string(),
                bytes: b"pdf bytes".to_vec(),
            },
            WriteCall::Append {
                block_id: "page-1".to_string(),
                body: json!({
                    "children": [{
                        "object": "block",
                        "type": "pdf",
                        "pdf": {
                            "file_upload": {
                                "id": "upload-1",
                            },
                            "caption": rich_text_json("Brief"),
                        },
                    }],
                    "position": {
                        "type": "after_block",
                        "after_block": {
                            "id": "paragraph-1",
                        },
                    },
                }),
            },
            WriteCall::UploadFile {
                filename: "theme.mp3".to_string(),
                content_type: "audio/mpeg".to_string(),
                bytes: b"audio bytes".to_vec(),
            },
            WriteCall::Append {
                block_id: "page-1".to_string(),
                body: json!({
                    "children": [{
                        "object": "block",
                        "type": "audio",
                        "audio": {
                            "file_upload": {
                                "id": "upload-1",
                            },
                            "caption": rich_text_json("Theme"),
                        },
                    }],
                    "position": {
                        "type": "after_block",
                        "after_block": {
                            "id": "created-1",
                        },
                    },
                }),
            },
            WriteCall::UploadFile {
                filename: "index.html".to_string(),
                content_type: "text/html".to_string(),
                bytes: b"html bytes".to_vec(),
            },
            WriteCall::Append {
                block_id: "page-1".to_string(),
                body: json!({
                    "children": [{
                        "object": "block",
                        "type": "file",
                        "file": {
                            "file_upload": {
                                "id": "upload-1",
                            },
                            "caption": rich_text_json("Index"),
                        },
                    }],
                    "position": {
                        "type": "after_block",
                        "after_block": {
                            "id": "created-2",
                        },
                    },
                }),
            },
            WriteCall::UploadFile {
                filename: "slides.pptx".to_string(),
                content_type:
                    "application/vnd.openxmlformats-officedocument.presentationml.presentation"
                        .to_string(),
                bytes: b"slides bytes".to_vec(),
            },
            WriteCall::Append {
                block_id: "page-1".to_string(),
                body: json!({
                    "children": [{
                        "object": "block",
                        "type": "file",
                        "file": {
                            "file_upload": {
                                "id": "upload-1",
                            },
                            "caption": rich_text_json("Slides"),
                        },
                    }],
                    "position": {
                        "type": "after_block",
                        "after_block": {
                            "id": "created-3",
                        },
                    },
                }),
            },
        ]
    );
}

#[test]
fn apply_updates_simple_table_rows_from_markdown_table() {
    let api = Arc::new(RecordingNotionApi::with_table(
        "2026-06-10T00:00:00.000Z",
        table_block("table-1", 2, true),
        vec![
            table_row_block("row-1", &["Name", "Status"]),
            table_row_block("row-2", &["Old task", "Todo"]),
        ],
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![PushOperation::UpdateBlock {
            block_id: RemoteId::new("table-1"),
            content: "| Name | Status |\n| --- | --- |\n| New task | Done |".to_string(),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect("apply table row updates");

    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [
            WriteCall::Update {
                block_id: "row-1".to_string(),
                body: json!({
                    "table_row": {
                        "cells": [
                            rich_text_json("Name"),
                            rich_text_json("Status"),
                        ],
                    },
                }),
            },
            WriteCall::Update {
                block_id: "row-2".to_string(),
                body: json!({
                    "table_row": {
                        "cells": [
                            rich_text_json("New task"),
                            rich_text_json("Done"),
                        ],
                    },
                }),
            },
        ]
    );
}

#[test]
fn apply_adds_table_rows_from_markdown_table() {
    let api = Arc::new(RecordingNotionApi::with_table(
        "2026-06-10T00:00:00.000Z",
        table_block("table-1", 2, true),
        vec![
            table_row_block("row-1", &["Name", "Status"]),
            table_row_block("row-2", &["Old task", "Todo"]),
        ],
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![PushOperation::UpdateBlock {
            block_id: RemoteId::new("table-1"),
            content: "| Name | Status |\n| --- | --- |\n| New task | Done |\n| Added task | Next |"
                .to_string(),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect("apply table row addition");

    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [
            WriteCall::Update {
                block_id: "row-1".to_string(),
                body: json!({
                    "table_row": {
                        "cells": [
                            rich_text_json("Name"),
                            rich_text_json("Status"),
                        ],
                    },
                }),
            },
            WriteCall::Update {
                block_id: "row-2".to_string(),
                body: json!({
                    "table_row": {
                        "cells": [
                            rich_text_json("New task"),
                            rich_text_json("Done"),
                        ],
                    },
                }),
            },
            WriteCall::Append {
                block_id: "table-1".to_string(),
                body: json!({
                    "children": [{
                        "object": "block",
                        "type": "table_row",
                        "table_row": {
                            "cells": [
                                rich_text_json("Added task"),
                                rich_text_json("Next"),
                            ],
                        },
                    }],
                    "position": {
                        "type": "after_block",
                        "after_block": {
                            "id": "row-2",
                        },
                    },
                }),
            },
        ]
    );
}

#[test]
fn apply_deletes_table_rows_from_markdown_table() {
    let api = Arc::new(RecordingNotionApi::with_table(
        "2026-06-10T00:00:00.000Z",
        table_block("table-1", 2, true),
        vec![
            table_row_block("row-1", &["Name", "Status"]),
            table_row_block("row-2", &["Keep task", "Todo"]),
            table_row_block("row-3", &["Drop task", "Later"]),
        ],
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![PushOperation::UpdateBlock {
            block_id: RemoteId::new("table-1"),
            content: "| Name | Status |\n| --- | --- |\n| Kept task | Done |".to_string(),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect("apply table row deletion");

    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [
            WriteCall::Update {
                block_id: "row-1".to_string(),
                body: json!({
                    "table_row": {
                        "cells": [
                            rich_text_json("Name"),
                            rich_text_json("Status"),
                        ],
                    },
                }),
            },
            WriteCall::Update {
                block_id: "row-2".to_string(),
                body: json!({
                    "table_row": {
                        "cells": [
                            rich_text_json("Kept task"),
                            rich_text_json("Done"),
                        ],
                    },
                }),
            },
            WriteCall::Delete {
                block_id: "row-3".to_string(),
            },
        ]
    );
}

#[test]
fn apply_updates_supported_page_properties() {
    let api = Arc::new(RecordingNotionApi::with_page_properties(
        "2026-06-10T00:00:00.000Z",
        BTreeMap::from([
            ("Name".to_string(), page_property("title")),
            ("Status".to_string(), page_property("select")),
            ("Tags".to_string(), page_property("multi_select")),
            ("Done".to_string(), page_property("checkbox")),
            ("Points".to_string(), page_property("number")),
            ("Notes".to_string(), page_property("rich_text")),
            ("Due".to_string(), page_property("date")),
            ("URL".to_string(), page_property("url")),
            ("Files".to_string(), page_property("files")),
            ("People".to_string(), page_property("people")),
            ("Relation".to_string(), page_property("relation")),
        ]),
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![PushOperation::UpdateProperties {
            entity_id: RemoteId::new("page-1"),
            properties: [
                (
                    "title".to_string(),
                    PropertyValue::String("Fix login bug".to_string()),
                ),
                (
                    "Status".to_string(),
                    PropertyValue::String("In progress".to_string()),
                ),
                (
                    "Tags".to_string(),
                    PropertyValue::List(vec!["Backend".to_string(), "Docs".to_string()]),
                ),
                ("Done".to_string(), PropertyValue::Bool(false)),
                ("Points".to_string(), PropertyValue::Number("3".to_string())),
                (
                    "Notes".to_string(),
                    PropertyValue::String("**Updated** notes and @date(2026-06-14)".to_string()),
                ),
                (
                    "Due".to_string(),
                    PropertyValue::String("2026-06-10".to_string()),
                ),
                (
                    "URL".to_string(),
                    PropertyValue::String("https://example.com/afs".to_string()),
                ),
                (
                    "Files".to_string(),
                    PropertyValue::List(vec![
                        "Spec <https://example.com/spec.pdf>".to_string(),
                        "https://example.com/diagram.png".to_string(),
                    ]),
                ),
                (
                    "People".to_string(),
                    PropertyValue::List(vec![
                        "Ada <aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa>".to_string(),
                    ]),
                ),
                (
                    "Relation".to_string(),
                    PropertyValue::List(vec![
                        "11111111111111111111111111111111".to_string(),
                        "22222222-2222-2222-2222-222222222222".to_string(),
                    ]),
                ),
            ]
            .into_iter()
            .collect(),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    let result = connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect("apply");

    assert_eq!(result.changed_remote_ids, vec![RemoteId::new("page-1")]);
    assert_eq!(
        result.effects,
        vec![JournalApplyEffect::UpdatedProperties {
            operation_id: operation_ids[0].clone(),
            operation_index: 0,
            entity_id: RemoteId::new("page-1"),
            keys: vec![
                "Done".to_string(),
                "Due".to_string(),
                "Files".to_string(),
                "Notes".to_string(),
                "People".to_string(),
                "Points".to_string(),
                "Relation".to_string(),
                "Status".to_string(),
                "Tags".to_string(),
                "URL".to_string(),
                "title".to_string(),
            ],
        }]
    );
    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [WriteCall::UpdatePage {
            page_id: "page-1".to_string(),
            body: json!({
                "properties": {
                    "Name": {
                        "title": rich_text_json("Fix login bug"),
                    },
                    "Status": {
                        "select": {
                            "name": "In progress",
                        },
                    },
                    "Tags": {
                        "multi_select": [
                            { "name": "Backend" },
                            { "name": "Docs" },
                        ],
                    },
                    "Done": {
                        "checkbox": false,
                    },
                    "Points": {
                        "number": 3.0,
                    },
                    "Notes": {
                        "rich_text": [
                            {
                                "type": "text",
                                "text": {
                                    "content": "Updated",
                                },
                                "annotations": {
                                    "bold": true,
                                    "italic": false,
                                    "strikethrough": false,
                                    "underline": false,
                                    "code": false,
                                    "color": "default",
                                },
                            },
                            {
                                "type": "text",
                                "text": {
                                    "content": " notes and ",
                                },
                            },
                            {
                                "type": "mention",
                                "mention": {
                                    "type": "date",
                                    "date": {
                                        "start": "2026-06-14",
                                    },
                                },
                            },
                        ],
                    },
                    "Due": {
                        "date": {
                            "start": "2026-06-10",
                        },
                    },
                    "URL": {
                        "url": "https://example.com/afs",
                    },
                    "Files": {
                        "files": [
                            {
                                "name": "Spec",
                                "type": "external",
                                "external": {
                                    "url": "https://example.com/spec.pdf",
                                },
                            },
                            {
                                "name": "diagram.png",
                                "type": "external",
                                "external": {
                                    "url": "https://example.com/diagram.png",
                                },
                            },
                        ],
                    },
                    "People": {
                        "people": [
                            { "id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa" },
                        ],
                    },
                    "Relation": {
                        "relation": [
                            { "id": "11111111111111111111111111111111" },
                            { "id": "22222222-2222-2222-2222-222222222222" },
                        ],
                    },
                },
            }),
        }]
    );
}

#[test]
fn apply_creates_child_page_and_marks_parent_changed() {
    let api = Arc::new(RecordingNotionApi::new("2026-06-10T00:00:00.000Z", false));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-parent")],
        vec![PushOperation::CreateEntity {
            parent_id: RemoteId::new("page-parent"),
            parent_kind: Some(EntityKind::Page),
            title: "New child".to_string(),
            properties: BTreeMap::new(),
            body: "# Child body\n\nCreated from AFS.".to_string(),
            source_path: "Roadmap/New child/page.md".into(),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    let result = connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect("apply");

    assert_eq!(
        result.changed_remote_ids,
        vec![
            RemoteId::new("created-page-1"),
            RemoteId::new("page-parent")
        ]
    );
    assert_eq!(
        result.effects,
        vec![JournalApplyEffect::CreatedEntity {
            operation_id: operation_ids[0].clone(),
            operation_index: 0,
            parent_id: RemoteId::new("page-parent"),
            entity_id: RemoteId::new("created-page-1"),
        }]
    );
    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [WriteCall::CreatePage {
            body: json!({
                "parent": {
                    "type": "page_id",
                    "page_id": "page-parent",
                },
                "properties": {
                    "title": {
                        "title": rich_text_json("New child"),
                    },
                },
                "children": [{
                    "object": "block",
                    "type": "heading_1",
                    "heading_1": {
                        "rich_text": rich_text_json("Child body"),
                    },
                }, {
                    "object": "block",
                    "type": "paragraph",
                    "paragraph": {
                        "rich_text": rich_text_json("Created from AFS."),
                    },
                }],
            }),
        }]
    );
}

#[test]
fn apply_creates_child_page_with_consecutive_markdown_list_items_as_separate_blocks() {
    let api = Arc::new(RecordingNotionApi::new("2026-06-10T00:00:00.000Z", false));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-parent")],
        vec![PushOperation::CreateEntity {
            parent_id: RemoteId::new("page-parent"),
            parent_kind: Some(EntityKind::Page),
            title: "New child".to_string(),
            properties: BTreeMap::new(),
            body: "# Child body\n\n- First\n- Second\n- [ ] Third".to_string(),
            source_path: "Roadmap/New child/page.md".into(),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect("apply");

    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [WriteCall::CreatePage {
            body: json!({
                "parent": {
                    "type": "page_id",
                    "page_id": "page-parent",
                },
                "properties": {
                    "title": {
                        "title": rich_text_json("New child"),
                    },
                },
                "children": [{
                    "object": "block",
                    "type": "heading_1",
                    "heading_1": {
                        "rich_text": rich_text_json("Child body"),
                    },
                }, {
                    "object": "block",
                    "type": "bulleted_list_item",
                    "bulleted_list_item": {
                        "rich_text": rich_text_json("First"),
                    },
                }, {
                    "object": "block",
                    "type": "bulleted_list_item",
                    "bulleted_list_item": {
                        "rich_text": rich_text_json("Second"),
                    },
                }, {
                    "object": "block",
                    "type": "to_do",
                    "to_do": {
                        "rich_text": rich_text_json("Third"),
                        "checked": false,
                    },
                }],
            }),
        }]
    );
}

#[test]
fn apply_creates_database_row_with_properties_and_children() {
    let api = Arc::new(RecordingNotionApi::with_data_source_properties(
        "2026-06-10T00:00:00.000Z",
        BTreeMap::from([
            ("Name".to_string(), data_source_property("title")),
            ("Status".to_string(), data_source_property("select")),
            ("Tags".to_string(), data_source_property("multi_select")),
            ("Done".to_string(), data_source_property("checkbox")),
            ("Points".to_string(), data_source_property("number")),
            ("Notes".to_string(), data_source_property("rich_text")),
            ("Files".to_string(), data_source_property("files")),
            ("People".to_string(), data_source_property("people")),
            ("Relation".to_string(), data_source_property("relation")),
        ]),
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("database-1")],
        vec![PushOperation::CreateEntity {
            parent_id: RemoteId::new("database-1"),
            parent_kind: Some(afs_core::model::EntityKind::Database),
            title: "New task".to_string(),
            properties: [
                (
                    "Status".to_string(),
                    PropertyValue::String("In progress".to_string()),
                ),
                (
                    "Tags".to_string(),
                    PropertyValue::List(vec!["Backend".to_string(), "Docs".to_string()]),
                ),
                ("Done".to_string(), PropertyValue::Bool(false)),
                ("Points".to_string(), PropertyValue::Number("5".to_string())),
                (
                    "Notes".to_string(),
                    PropertyValue::String("Created **rich** notes".to_string()),
                ),
                (
                    "Files".to_string(),
                    PropertyValue::List(vec![
                        "Design <https://example.com/design.pdf>".to_string(),
                    ]),
                ),
                (
                    "People".to_string(),
                    PropertyValue::List(vec!["bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string()]),
                ),
                (
                    "Relation".to_string(),
                    PropertyValue::List(vec!["33333333333333333333333333333333".to_string()]),
                ),
            ]
            .into_iter()
            .collect(),
            body: "# Notes\n\n- [ ] Wire create".to_string(),
            source_path: "tasks/new-task.md".into(),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    let result = connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect("apply");

    assert_eq!(
        result.changed_remote_ids,
        vec![RemoteId::new("created-page-1")]
    );
    assert_eq!(
        result.effects,
        vec![JournalApplyEffect::CreatedEntity {
            operation_id: operation_ids[0].clone(),
            operation_index: 0,
            parent_id: RemoteId::new("database-1"),
            entity_id: RemoteId::new("created-page-1"),
        }]
    );
    let writes = api.writes.lock().expect("writes");
    assert_eq!(
        writes.as_slice(),
        [WriteCall::CreatePage {
            body: json!({
                "parent": {
                    "type": "data_source_id",
                    "data_source_id": "source-1",
                },
                "properties": {
                    "Name": {
                        "title": rich_text_json("New task"),
                    },
                    "Status": {
                        "select": {
                            "name": "In progress",
                        },
                    },
                    "Tags": {
                        "multi_select": [
                            { "name": "Backend" },
                            { "name": "Docs" },
                        ],
                    },
                    "Done": {
                        "checkbox": false,
                    },
                    "Notes": {
                        "rich_text": [
                            {
                                "type": "text",
                                "text": {
                                    "content": "Created ",
                                },
                            },
                            {
                                "type": "text",
                                "text": {
                                    "content": "rich",
                                },
                                "annotations": {
                                    "bold": true,
                                    "italic": false,
                                    "strikethrough": false,
                                    "underline": false,
                                    "code": false,
                                    "color": "default",
                                },
                            },
                            {
                                "type": "text",
                                "text": {
                                    "content": " notes",
                                },
                            },
                        ],
                    },
                    "Points": {
                        "number": 5.0,
                    },
                    "Files": {
                        "files": [
                            {
                                "name": "Design",
                                "type": "external",
                                "external": {
                                    "url": "https://example.com/design.pdf",
                                },
                            },
                        ],
                    },
                    "People": {
                        "people": [
                            { "id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" },
                        ],
                    },
                    "Relation": {
                        "relation": [
                            { "id": "33333333333333333333333333333333" },
                        ],
                    },
                },
                "children": [
                    {
                        "object": "block",
                        "type": "heading_1",
                        "heading_1": {
                            "rich_text": rich_text_json("Notes"),
                        },
                    },
                    {
                        "object": "block",
                        "type": "to_do",
                        "to_do": {
                            "rich_text": rich_text_json("Wire create"),
                            "checked": false,
                        },
                    },
                ],
            }),
        }]
    );
}

#[test]
fn apply_rejects_legacy_property_update_without_values() {
    let api = Arc::new(RecordingNotionApi::with_page_properties(
        "2026-06-10T00:00:00.000Z",
        BTreeMap::from([("Name".to_string(), page_property("title"))]),
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api);
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![PushOperation::UpdateProperties {
            entity_id: RemoteId::new("page-1"),
            properties: BTreeMap::new(),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = operation_ids(&push_id, &plan);
    let mount_id = MountId::new("notion-main");

    let error = connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
            local_root: None,
        })
        .expect_err("legacy property update");

    assert!(matches!(error, AfsError::Unsupported(_)));
}

fn operation_ids(push_id: &PushId, plan: &PushPlan) -> Vec<PushOperationId> {
    plan.operations
        .iter()
        .enumerate()
        .map(|(index, operation)| PushOperationId::for_operation(push_id, index, operation))
        .collect()
}

#[derive(Debug)]
struct RecordingNotionApi {
    page: PageDto,
    database: DatabaseDto,
    data_source: DataSourceDto,
    children: BTreeMap<(String, Option<String>), BlockListDto>,
    writes: Mutex<Vec<WriteCall>>,
    append_count: Mutex<usize>,
}

impl RecordingNotionApi {
    fn new(last_edited_time: &str, rich_paragraph: bool) -> Self {
        let rich_text = if rich_paragraph {
            vec![annotated_text("Old paragraph.", |annotations| {
                annotations.bold = true;
            })]
        } else {
            rich_text("Old paragraph.")
        };
        Self::with_paragraph_rich_text(last_edited_time, rich_text)
    }

    fn with_paragraph_rich_text(last_edited_time: &str, rich_text: Vec<RichTextDto>) -> Self {
        Self::with_page_and_children(
            PageDto {
                id: "page-1".to_string(),
                parent: None,
                created_time: Some("2026-06-10T00:00:00.000Z".to_string()),
                last_edited_time: Some(last_edited_time.to_string()),
                archived: false,
                in_trash: false,
                properties: BTreeMap::new(),
            },
            rich_text,
        )
    }

    fn with_page_properties(
        last_edited_time: &str,
        properties: BTreeMap<String, PagePropertyDto>,
    ) -> Self {
        let page = PageDto {
            id: "page-1".to_string(),
            parent: None,
            created_time: Some("2026-06-10T00:00:00.000Z".to_string()),
            last_edited_time: Some(last_edited_time.to_string()),
            archived: false,
            in_trash: false,
            properties,
        };
        Self::with_page_and_children(page, rich_text("Old paragraph."))
    }

    fn with_data_source_properties(
        last_edited_time: &str,
        properties: BTreeMap<String, DataSourcePropertyDto>,
    ) -> Self {
        let mut api = Self::new(last_edited_time, false);
        api.database = DatabaseDto {
            id: "database-1".to_string(),
            data_sources: vec![DataSourceSummaryDto {
                id: "source-1".to_string(),
                name: Some("Tasks".to_string()),
            }],
            last_edited_time: Some(last_edited_time.to_string()),
            ..Default::default()
        };
        api.data_source = DataSourceDto {
            id: "source-1".to_string(),
            name: Some("Tasks".to_string()),
            properties,
            ..Default::default()
        };
        api
    }

    fn with_blocks(last_edited_time: &str, blocks: Vec<BlockDto>) -> Self {
        let page = PageDto {
            id: "page-1".to_string(),
            parent: None,
            created_time: Some("2026-06-10T00:00:00.000Z".to_string()),
            last_edited_time: Some(last_edited_time.to_string()),
            archived: false,
            in_trash: false,
            properties: BTreeMap::new(),
        };
        Self::with_page_and_block_results(page, blocks)
    }

    fn with_table(last_edited_time: &str, table: BlockDto, rows: Vec<BlockDto>) -> Self {
        let page = PageDto {
            id: "page-1".to_string(),
            parent: None,
            created_time: Some("2026-06-10T00:00:00.000Z".to_string()),
            last_edited_time: Some(last_edited_time.to_string()),
            archived: false,
            in_trash: false,
            properties: BTreeMap::new(),
        };
        let children = BTreeMap::from([
            (
                ("page-1".to_string(), None),
                PaginatedListDto {
                    results: vec![table.clone()],
                    next_cursor: None,
                    has_more: false,
                },
            ),
            (
                (table.id, None),
                PaginatedListDto {
                    results: rows,
                    next_cursor: None,
                    has_more: false,
                },
            ),
        ]);
        Self {
            page,
            database: DatabaseDto {
                id: "database-1".to_string(),
                data_sources: vec![DataSourceSummaryDto {
                    id: "source-1".to_string(),
                    name: Some("Tasks".to_string()),
                }],
                last_edited_time: Some("2026-06-10T00:00:00.000Z".to_string()),
                ..Default::default()
            },
            data_source: DataSourceDto {
                id: "source-1".to_string(),
                name: Some("Tasks".to_string()),
                properties: BTreeMap::new(),
                ..Default::default()
            },
            children,
            writes: Mutex::new(Vec::new()),
            append_count: Mutex::new(0),
        }
    }

    fn with_page_and_children(page: PageDto, rich_text: Vec<RichTextDto>) -> Self {
        Self::with_page_and_block_results(
            page,
            vec![
                paragraph_block_with_rich_text("paragraph-1", rich_text),
                paragraph_block("old-block", "Old block.", false),
            ],
        )
    }

    fn with_page_and_block_results(page: PageDto, results: Vec<BlockDto>) -> Self {
        let children = BTreeMap::from([(
            ("page-1".to_string(), None),
            PaginatedListDto {
                results,
                next_cursor: None,
                has_more: false,
            },
        )]);
        Self {
            page,
            database: DatabaseDto {
                id: "database-1".to_string(),
                data_sources: vec![DataSourceSummaryDto {
                    id: "source-1".to_string(),
                    name: Some("Tasks".to_string()),
                }],
                last_edited_time: Some("2026-06-10T00:00:00.000Z".to_string()),
                ..Default::default()
            },
            data_source: DataSourceDto {
                id: "source-1".to_string(),
                name: Some("Tasks".to_string()),
                properties: BTreeMap::new(),
                ..Default::default()
            },
            children,
            writes: Mutex::new(Vec::new()),
            append_count: Mutex::new(0),
        }
    }
}

impl NotionApi for RecordingNotionApi {
    fn retrieve_page(&self, page_id: &str) -> AfsResult<PageDto> {
        if page_id == self.page.id {
            Ok(self.page.clone())
        } else if page_id == "created-page-1" {
            Ok(PageDto {
                id: "created-page-1".to_string(),
                parent: None,
                created_time: Some("2026-06-10T00:00:00.000Z".to_string()),
                last_edited_time: Some("2026-06-10T00:00:00.000Z".to_string()),
                archived: false,
                in_trash: false,
                properties: BTreeMap::from([("Name".to_string(), page_property("title"))]),
            })
        } else {
            Err(AfsError::InvalidState(format!("missing page {page_id}")))
        }
    }

    fn retrieve_database(&self, database_id: &str) -> AfsResult<DatabaseDto> {
        if database_id == self.database.id {
            Ok(self.database.clone())
        } else {
            Err(AfsError::InvalidState(format!(
                "missing database {database_id}"
            )))
        }
    }

    fn retrieve_data_source(&self, data_source_id: &str) -> AfsResult<DataSourceDto> {
        if data_source_id == self.data_source.id {
            Ok(self.data_source.clone())
        } else {
            Err(AfsError::InvalidState(format!(
                "missing data source {data_source_id}"
            )))
        }
    }

    fn retrieve_block_children(
        &self,
        block_id: &str,
        start_cursor: Option<&str>,
    ) -> AfsResult<BlockListDto> {
        Ok(self
            .children
            .get(&(block_id.to_string(), start_cursor.map(str::to_string)))
            .cloned()
            .unwrap_or_default())
    }

    fn search_pages(&self, _start_cursor: Option<&str>) -> AfsResult<PageListDto> {
        Ok(PaginatedListDto {
            results: vec![self.page.clone()],
            next_cursor: None,
            has_more: false,
        })
    }

    fn update_page(&self, page_id: &str, body: Value) -> AfsResult<PageDto> {
        self.writes
            .lock()
            .expect("writes")
            .push(WriteCall::UpdatePage {
                page_id: page_id.to_string(),
                body,
            });
        Ok(self.page.clone())
    }

    fn create_page(&self, body: Value) -> AfsResult<PageDto> {
        self.writes
            .lock()
            .expect("writes")
            .push(WriteCall::CreatePage { body });
        Ok(PageDto {
            id: "created-page-1".to_string(),
            parent: None,
            created_time: Some("2026-06-10T00:00:00.000Z".to_string()),
            last_edited_time: Some("2026-06-10T00:00:00.000Z".to_string()),
            archived: false,
            in_trash: false,
            properties: BTreeMap::new(),
        })
    }

    fn update_block(&self, block_id: &str, body: Value) -> AfsResult<BlockDto> {
        self.writes.lock().expect("writes").push(WriteCall::Update {
            block_id: block_id.to_string(),
            body,
        });
        Ok(block(block_id, "paragraph"))
    }

    fn move_block(
        &self,
        block_id: &str,
        parent_id: &str,
        after: Option<&str>,
    ) -> AfsResult<BlockDto> {
        self.writes.lock().expect("writes").push(WriteCall::Move {
            block_id: block_id.to_string(),
            parent_id: parent_id.to_string(),
            after: after.map(str::to_string),
        });
        Ok(block(block_id, "paragraph"))
    }

    fn append_block_children(&self, block_id: &str, body: Value) -> AfsResult<BlockListDto> {
        self.writes.lock().expect("writes").push(WriteCall::Append {
            block_id: block_id.to_string(),
            body,
        });
        let mut append_count = self.append_count.lock().expect("append count");
        *append_count += 1;
        Ok(PaginatedListDto {
            results: vec![paragraph_block(
                &format!("created-{}", *append_count),
                "Created.",
                false,
            )],
            next_cursor: None,
            has_more: false,
        })
    }

    fn delete_block(&self, block_id: &str) -> AfsResult<BlockDto> {
        self.writes.lock().expect("writes").push(WriteCall::Delete {
            block_id: block_id.to_string(),
        });
        Ok(block(block_id, "paragraph"))
    }

    fn upload_file(&self, filename: &str, content_type: &str, bytes: Vec<u8>) -> AfsResult<String> {
        self.writes
            .lock()
            .expect("writes")
            .push(WriteCall::UploadFile {
                filename: filename.to_string(),
                content_type: content_type.to_string(),
                bytes,
            });
        Ok("upload-1".to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum WriteCall {
    UpdatePage {
        page_id: String,
        body: Value,
    },
    CreatePage {
        body: Value,
    },
    Update {
        block_id: String,
        body: Value,
    },
    Move {
        block_id: String,
        parent_id: String,
        after: Option<String>,
    },
    Append {
        block_id: String,
        body: Value,
    },
    Delete {
        block_id: String,
    },
    UploadFile {
        filename: String,
        content_type: String,
        bytes: Vec<u8>,
    },
}

fn page_property(kind: &str) -> PagePropertyDto {
    let mut property = PagePropertyDto {
        kind: kind.to_string(),
        ..Default::default()
    };
    match kind {
        "title" => property.title = rich_text("Old title"),
        "select" => {
            property.select = Some(SelectOptionDto {
                id: "todo-id".to_string(),
                name: "Todo".to_string(),
                color: None,
            });
        }
        _ => {}
    }
    property
}

fn data_source_property(kind: &str) -> DataSourcePropertyDto {
    DataSourcePropertyDto {
        id: format!("{kind}-id"),
        kind: kind.to_string(),
        ..Default::default()
    }
}

fn block(id: &str, kind: &str) -> BlockDto {
    BlockDto {
        id: id.to_string(),
        kind: kind.to_string(),
        ..Default::default()
    }
}

fn paragraph_block(id: &str, text: &str, rich: bool) -> BlockDto {
    let mut block = block(id, "paragraph");
    let mut rich_text = rich_text(text);
    if rich {
        rich_text[0].annotations = RichTextAnnotationsDto {
            bold: true,
            ..Default::default()
        };
    }
    block.paragraph = Some(RichTextBlockDto {
        rich_text,
        color: None,
    });
    block
}

fn paragraph_block_with_rich_text(id: &str, rich_text: Vec<RichTextDto>) -> BlockDto {
    let mut block = block(id, "paragraph");
    block.paragraph = Some(RichTextBlockDto {
        rich_text,
        color: None,
    });
    block
}

fn toggle_block(id: &str, text: &str) -> BlockDto {
    let mut block = block(id, "toggle");
    block.toggle = Some(RichTextBlockDto {
        rich_text: rich_text(text),
        color: None,
    });
    block
}

fn callout_block(id: &str, text: &str) -> BlockDto {
    let mut block = block(id, "callout");
    block.callout = Some(RichTextBlockDto {
        rich_text: rich_text(text),
        color: None,
    });
    block
}

fn to_do_block(id: &str, text: &str, checked: bool) -> BlockDto {
    let mut block = block(id, "to_do");
    block.to_do = Some(ToDoBlockDto {
        rich_text: rich_text(text),
        checked,
        color: None,
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

fn code_block(id: &str, language: &str, text: &str) -> BlockDto {
    let mut block = block(id, "code");
    block.code = Some(CodeBlockDto {
        rich_text: rich_text(text),
        language: Some(language.to_string()),
    });
    block
}

fn url_block(id: &str, kind: &str, url: &str, caption: &str) -> BlockDto {
    let mut block = block(id, kind);
    let payload = Some(UrlBlockDto {
        url: url.to_string(),
        caption: rich_text(caption),
    });
    match kind {
        "bookmark" => block.bookmark = payload,
        "embed" => block.embed = payload,
        _ => {}
    }
    block
}

fn media_block(id: &str, kind: &str, url: &str, caption: &str) -> BlockDto {
    let mut block = block(id, kind);
    let payload = Some(FileBlockDto {
        kind: "external".to_string(),
        external: Some(ExternalFileDto {
            url: url.to_string(),
        }),
        file: None,
        caption: rich_text(caption),
    });
    match kind {
        "image" => block.image = payload,
        "video" => block.video = payload,
        "file" => block.file = payload,
        "pdf" => block.pdf = payload,
        "audio" => block.audio = payload,
        _ => {}
    }
    block
}

fn table_block(id: &str, width: u16, has_column_header: bool) -> BlockDto {
    let mut block = block(id, "table");
    block.has_children = true;
    block.table = Some(TableBlockDto {
        table_width: width,
        has_column_header,
        has_row_header: false,
    });
    block
}

fn table_of_contents_block(id: &str) -> BlockDto {
    let mut block = block(id, "table_of_contents");
    block.table_of_contents = Some(ColorOnlyBlockDto { color: None });
    block
}

fn table_row_block(id: &str, cells: &[&str]) -> BlockDto {
    let mut block = block(id, "table_row");
    block.table_row = Some(TableRowBlockDto {
        cells: cells.iter().map(|cell| rich_text(cell)).collect(),
    });
    block
}

fn link_to_page_block(id: &str, kind: &str, target_id: &str) -> BlockDto {
    let mut block = block(id, "link_to_page");
    let mut payload = LinkToPageBlockDto {
        kind: kind.to_string(),
        ..Default::default()
    };
    match kind {
        "page_id" => payload.page_id = Some(target_id.to_string()),
        "database_id" => payload.database_id = Some(target_id.to_string()),
        _ => {}
    }
    block.link_to_page = Some(payload);
    block
}

fn child_page_block(id: &str, title: &str) -> BlockDto {
    let mut block = block(id, "child_page");
    block.child_page = Some(TitleBlockDto {
        title: title.to_string(),
    });
    block
}

fn rich_text(text: &str) -> Vec<RichTextDto> {
    vec![rich_text_part(text)]
}

fn rich_text_part(text: &str) -> RichTextDto {
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

fn annotated_text(text: &str, apply: impl FnOnce(&mut RichTextAnnotationsDto)) -> RichTextDto {
    let mut part = rich_text_part(text);
    apply(&mut part.annotations);
    part
}

fn linked_text(text: &str, href: &str) -> RichTextDto {
    RichTextDto {
        href: Some(href.to_string()),
        text: Some(TextRichTextDto {
            content: text.to_string(),
            link: Some(LinkDto {
                url: href.to_string(),
            }),
        }),
        ..rich_text_part(text)
    }
}

fn date_mention(text: &str, start: &str) -> RichTextDto {
    RichTextDto {
        kind: "mention".to_string(),
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

fn rich_text_json(text: &str) -> Value {
    json!([
        {
            "type": "text",
            "text": {
                "content": text,
            },
        }
    ])
}

fn append_call(anchor_id: &str, child: Value) -> WriteCall {
    WriteCall::Append {
        block_id: "page-1".to_string(),
        body: json!({
            "children": [child],
            "position": {
                "type": "after_block",
                "after_block": {
                    "id": anchor_id,
                },
            },
        }),
    }
}
