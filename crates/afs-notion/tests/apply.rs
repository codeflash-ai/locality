use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

use afs_connector::{ApplyPlanRequest, Connector};
use afs_core::journal::{JournalApplyEffect, PushId, PushOperationId};
use afs_core::model::{MountId, RemoteId};
use afs_core::planner::{PropertyValue, PushOperation, PushPlan};
use afs_core::push::RemotePrecondition;
use afs_core::{AfsError, AfsResult};
use afs_notion::client::NotionApi;
use afs_notion::dto::{
    BlockDto, BlockListDto, DataSourceDto, DataSourcePropertyDto, DataSourceSummaryDto,
    DatabaseDto, DateMentionDto, EquationBlockDto, LinkDto, MentionRichTextDto, PageDto,
    PageListDto, PagePropertyDto, PaginatedListDto, RichTextAnnotationsDto, RichTextBlockDto,
    RichTextDto, SelectOptionDto, TextRichTextDto,
};
use afs_notion::{NotionConfig, NotionConnector};
use serde_json::{Value, json};

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
        })
        .expect("concurrency");
    let result = connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &preconditions,
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
fn apply_moves_existing_blocks_after_mid_page_append() {
    let api = Arc::new(RecordingNotionApi::with_blocks(
        "2026-06-10T00:00:00.000Z",
        vec![
            paragraph_block("paragraph-1", "First.", false),
            paragraph_block("paragraph-2", "Second.", false),
            paragraph_block("directive-1", "Directive placeholder.", false),
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

    let result = connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
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
            JournalApplyEffect::MovedBlock {
                operation_id: operation_ids[1].clone(),
                operation_index: 1,
                block_id: RemoteId::new("directive-1"),
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
                        "type": "paragraph",
                        "paragraph": {
                            "rich_text": rich_text_json("Inserted paragraph."),
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
            WriteCall::Move {
                block_id: "directive-1".to_string(),
                parent_id: "page-1".to_string(),
                after: Some("created-1".to_string()),
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
        })
        .expect_err("stale remote");

    assert!(matches!(error, AfsError::Guardrail(_)));
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
            rich_text_part("."),
        ],
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![PushOperation::UpdateBlock {
            block_id: RemoteId::new("paragraph-1"),
            content: "**Boldly** and 2026-06-10 plus [Docs](https://example.com/) and $E=mc^2$ [Roadmap](afs://page-2)".to_string(),
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
                            "type": "mention",
                            "mention": {
                                "type": "page",
                                "page": {
                                    "id": "page-2",
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
fn apply_updates_supported_page_properties() {
    let api = Arc::new(RecordingNotionApi::with_page_properties(
        "2026-06-10T00:00:00.000Z",
        BTreeMap::from([
            ("Name".to_string(), page_property("title")),
            ("Status".to_string(), page_property("select")),
            ("Tags".to_string(), page_property("multi_select")),
            ("Done".to_string(), page_property("checkbox")),
            ("Points".to_string(), page_property("number")),
            ("Due".to_string(), page_property("date")),
            ("URL".to_string(), page_property("url")),
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
                    "Due".to_string(),
                    PropertyValue::String("2026-06-10".to_string()),
                ),
                (
                    "URL".to_string(),
                    PropertyValue::String("https://example.com/afs".to_string()),
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
                "Points".to_string(),
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
                    "Due": {
                        "date": {
                            "start": "2026-06-10",
                        },
                    },
                    "URL": {
                        "url": "https://example.com/afs",
                    },
                },
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
        ]),
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("database-1")],
        vec![PushOperation::CreateEntity {
            parent_id: RemoteId::new("database-1"),
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
                    "Points": {
                        "number": 5.0,
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
            created_time: Some("2026-06-10T00:00:00.000Z".to_string()),
            last_edited_time: Some(last_edited_time.to_string()),
            archived: false,
            in_trash: false,
            properties: BTreeMap::new(),
        };
        Self::with_page_and_block_results(page, blocks)
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

fn equation_block(id: &str, expression: &str) -> BlockDto {
    let mut block = block(id, "equation");
    block.equation = Some(EquationBlockDto {
        expression: expression.to_string(),
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
