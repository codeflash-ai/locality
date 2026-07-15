use locality_core::journal::{
    JournalApplyEffect, JournalEntry, JournalPreimage, JournalStatus, PushId, PushOperationId,
};
use locality_core::model::{MountId, RemoteId};
use locality_core::planner::{PropertyValue, PushOperation, PushPlan};
use locality_core::shadow::ShadowDocument;
use locality_core::undo::{EntityUndoState, UndoOperation, UndoPlanStatus, plan_journal_undo};

#[test]
fn update_block_reverses_to_preimage_content() {
    let entry = journal_entry(vec![PushOperation::UpdateBlock {
        block_id: RemoteId::new("paragraph-1"),
        content: "New paragraph.".to_string(),
    }]);

    let plan = plan_journal_undo(&entry);

    assert_eq!(plan.status, UndoPlanStatus::Complete);
    assert_eq!(
        plan.operations,
        vec![UndoOperation::RestoreBlockContent {
            block_id: RemoteId::new("paragraph-1"),
            content: "Old paragraph.".to_string(),
        }]
    );
    assert!(plan.unsupported.is_empty());
}

#[test]
fn update_media_reverses_to_preimage_markdown() {
    let entry = journal_entry(vec![PushOperation::UpdateMedia {
        block_id: RemoteId::new("paragraph-1"),
        local_path: ".loc/media/Roadmap/image-paragraph1.png".into(),
        caption: "New image".to_string(),
    }]);

    let plan = plan_journal_undo(&entry);

    assert_eq!(plan.status, UndoPlanStatus::Complete);
    assert_eq!(
        plan.operations,
        vec![UndoOperation::RestoreBlockContent {
            block_id: RemoteId::new("paragraph-1"),
            content: "Old paragraph.".to_string(),
        }]
    );
    assert!(plan.unsupported.is_empty());
}

#[test]
fn archive_block_reverses_to_restore_with_original_position() {
    let entry = journal_entry(vec![PushOperation::ArchiveBlock {
        block_id: RemoteId::new("paragraph-1"),
    }]);

    let plan = plan_journal_undo(&entry);

    assert_eq!(plan.status, UndoPlanStatus::Complete);
    assert_eq!(
        plan.operations,
        vec![UndoOperation::RestoreArchivedBlock {
            block_id: RemoteId::new("paragraph-1"),
            parent_id: RemoteId::new("page-1"),
            after: Some(RemoteId::new("heading-1")),
            content: "Old paragraph.".to_string(),
            native_kind: None,
        }]
    );
}

#[test]
fn archive_block_restore_carries_native_kind_from_preimage() {
    let mut shadow = shadow();
    shadow.blocks[1].native_kind = Some("paragraph".to_string());
    let entry = journal_entry_with_shadow(
        vec![PushOperation::ArchiveBlock {
            block_id: RemoteId::new("paragraph-1"),
        }],
        shadow,
    );

    let plan = plan_journal_undo(&entry);

    assert_eq!(plan.status, UndoPlanStatus::Complete);
    assert_eq!(
        plan.operations,
        vec![UndoOperation::RestoreArchivedBlock {
            block_id: RemoteId::new("paragraph-1"),
            parent_id: RemoteId::new("page-1"),
            after: Some(RemoteId::new("heading-1")),
            content: "Old paragraph.".to_string(),
            native_kind: Some("paragraph".to_string()),
        }]
    );
}

#[test]
fn move_block_reverses_to_original_position() {
    let entry = journal_entry(vec![PushOperation::MoveBlock {
        block_id: RemoteId::new("paragraph-1"),
        after: None,
    }]);

    let plan = plan_journal_undo(&entry);

    assert_eq!(plan.status, UndoPlanStatus::Complete);
    assert_eq!(
        plan.operations,
        vec![UndoOperation::MoveBlock {
            block_id: RemoteId::new("paragraph-1"),
            after: Some(RemoteId::new("heading-1")),
        }]
    );
}

#[test]
fn copy_archive_move_reverses_to_archive_copy_then_restore_original() {
    let mut entry = journal_entry(vec![PushOperation::MoveBlock {
        block_id: RemoteId::new("paragraph-1"),
        after: None,
    }]);
    entry.apply_effects = vec![
        JournalApplyEffect::CreatedBlock {
            operation_id: PushOperationId::for_operation(
                &entry.push_id,
                0,
                &entry.plan.operations[0],
            ),
            operation_index: 0,
            parent_id: RemoteId::new("page-1"),
            block_id: RemoteId::new("moved-copy-1"),
        },
        JournalApplyEffect::ArchivedBlock {
            operation_id: PushOperationId::for_operation(
                &entry.push_id,
                0,
                &entry.plan.operations[0],
            ),
            operation_index: 0,
            block_id: RemoteId::new("paragraph-1"),
        },
    ];

    let plan = plan_journal_undo(&entry);

    assert_eq!(plan.status, UndoPlanStatus::Complete);
    assert_eq!(
        plan.operations,
        vec![
            UndoOperation::ArchiveCreatedBlock {
                block_id: RemoteId::new("moved-copy-1"),
            },
            UndoOperation::RestoreArchivedBlock {
                block_id: RemoteId::new("paragraph-1"),
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("heading-1")),
                content: "Old paragraph.".to_string(),
                native_kind: None,
            },
        ]
    );
}

#[test]
fn reverse_plan_orders_dependent_moves_for_safe_apply() {
    let entry = journal_entry_with_shadow(
        vec![
            PushOperation::MoveBlock {
                block_id: RemoteId::new("c"),
                after: Some(RemoteId::new("a")),
            },
            PushOperation::MoveBlock {
                block_id: RemoteId::new("b"),
                after: Some(RemoteId::new("d")),
            },
        ],
        multi_block_shadow(),
    );

    let plan = plan_journal_undo(&entry);

    assert_eq!(plan.status, UndoPlanStatus::Complete);
    assert_eq!(
        plan.operations,
        vec![
            UndoOperation::MoveBlock {
                block_id: RemoteId::new("b"),
                after: Some(RemoteId::new("a")),
            },
            UndoOperation::MoveBlock {
                block_id: RemoteId::new("c"),
                after: Some(RemoteId::new("b")),
            },
        ]
    );
}

#[test]
fn append_block_is_unsupported_until_apply_records_created_id() {
    let entry = journal_entry(vec![PushOperation::AppendBlock {
        parent_id: RemoteId::new("page-1"),
        after: Some(RemoteId::new("paragraph-1")),
        content: "New paragraph.".to_string(),
    }]);

    let plan = plan_journal_undo(&entry);

    assert_eq!(plan.status, UndoPlanStatus::Blocked);
    assert!(plan.operations.is_empty());
    assert_eq!(plan.unsupported[0].code, "append_block_missing_created_id");
}

#[test]
fn append_block_reverses_to_archive_created_block_when_effect_is_journaled() {
    let mut entry = journal_entry(vec![PushOperation::AppendBlock {
        parent_id: RemoteId::new("page-1"),
        after: Some(RemoteId::new("paragraph-1")),
        content: "New paragraph.".to_string(),
    }]);
    entry.apply_effects = vec![JournalApplyEffect::CreatedBlock {
        operation_id: PushOperationId::for_operation(&entry.push_id, 0, &entry.plan.operations[0]),
        operation_index: 0,
        parent_id: RemoteId::new("page-1"),
        block_id: RemoteId::new("created-block-1"),
    }];

    let plan = plan_journal_undo(&entry);

    assert_eq!(plan.status, UndoPlanStatus::Complete);
    assert_eq!(
        plan.operations,
        vec![UndoOperation::ArchiveCreatedBlock {
            block_id: RemoteId::new("created-block-1"),
        }]
    );
}

#[test]
fn replace_block_reverses_to_archive_replacement_then_restore_original() {
    let mut entry = journal_entry(vec![PushOperation::ReplaceBlock {
        block_id: RemoteId::new("paragraph-1"),
        content: "- Replacement".to_string(),
    }]);
    entry.apply_effects = vec![
        JournalApplyEffect::CreatedBlock {
            operation_id: PushOperationId::for_operation(
                &entry.push_id,
                0,
                &entry.plan.operations[0],
            ),
            operation_index: 0,
            parent_id: RemoteId::new("page-1"),
            block_id: RemoteId::new("replacement-block-1"),
        },
        JournalApplyEffect::ArchivedBlock {
            operation_id: PushOperationId::for_operation(
                &entry.push_id,
                0,
                &entry.plan.operations[0],
            ),
            operation_index: 0,
            block_id: RemoteId::new("paragraph-1"),
        },
    ];

    let plan = plan_journal_undo(&entry);

    assert_eq!(plan.status, UndoPlanStatus::Complete);
    assert_eq!(
        plan.operations,
        vec![
            UndoOperation::ArchiveCreatedBlock {
                block_id: RemoteId::new("replacement-block-1"),
            },
            UndoOperation::RestoreArchivedBlock {
                block_id: RemoteId::new("paragraph-1"),
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("heading-1")),
                content: "Old paragraph.".to_string(),
                native_kind: None,
            },
        ]
    );
}

#[test]
fn replace_block_is_unsupported_until_apply_records_replacement_id() {
    let entry = journal_entry(vec![PushOperation::ReplaceBlock {
        block_id: RemoteId::new("paragraph-1"),
        content: "- Replacement".to_string(),
    }]);

    let plan = plan_journal_undo(&entry);

    assert_eq!(plan.status, UndoPlanStatus::Blocked);
    assert_eq!(plan.unsupported[0].code, "replace_block_missing_created_id");
}

#[test]
fn create_entity_reverses_to_archive_created_entity_when_effect_is_journaled() {
    let mut entry = journal_entry(vec![PushOperation::CreateEntity {
        parent_id: RemoteId::new("page-1"),
        parent_kind: None,
        parent_workspace: false,
        title: "New page".to_string(),
        properties: Default::default(),
        body: String::new(),
        source_path: "new-page.md".into(),
    }]);
    entry.apply_effects = vec![JournalApplyEffect::CreatedEntity {
        operation_id: PushOperationId::for_operation(&entry.push_id, 0, &entry.plan.operations[0]),
        operation_index: 0,
        parent_id: RemoteId::new("page-1"),
        entity_id: RemoteId::new("created-page-1"),
    }];

    let plan = plan_journal_undo(&entry);

    assert_eq!(plan.status, UndoPlanStatus::Complete);
    assert_eq!(
        plan.operations,
        vec![UndoOperation::ArchiveCreatedEntity {
            entity_id: RemoteId::new("created-page-1"),
            expected: Some(EntityUndoState {
                parent_id: RemoteId::new("page-1"),
                title: "New page".to_string(),
                properties: BTreeMap::new(),
                body: String::new(),
                archived: false,
            }),
        }]
    );
}

#[test]
fn update_entity_body_reverses_with_expected_current_and_previous_body() {
    let entry = journal_entry(vec![PushOperation::UpdateEntityBody {
        entity_id: RemoteId::new("page-1"),
        body: "New first paragraph.\n\nNew second paragraph.".to_string(),
    }]);

    let plan = plan_journal_undo(&entry);

    assert_eq!(plan.status, UndoPlanStatus::Complete);
    assert_eq!(
        plan.operations,
        vec![UndoOperation::RestoreEntityBody {
            entity_id: RemoteId::new("page-1"),
            expected_current: "New first paragraph.\n\nNew second paragraph.".to_string(),
            previous: "# Roadmap\n\nOld paragraph.".to_string(),
        }]
    );
}

#[test]
fn update_properties_reverses_from_shadow_frontmatter_and_nulls_absent_keys() {
    let properties = BTreeMap::from([
        (
            "Status".to_string(),
            PropertyValue::String("Done".to_string()),
        ),
        (
            "New field".to_string(),
            PropertyValue::String("Added".to_string()),
        ),
        (
            "title".to_string(),
            PropertyValue::String("Renamed".to_string()),
        ),
    ]);
    let entry = journal_entry_with_shadow(
        vec![PushOperation::UpdateProperties {
            entity_id: RemoteId::new("page-1"),
            properties: properties.clone(),
        }],
        shadow_with_frontmatter(),
    );

    let plan = plan_journal_undo(&entry);

    assert_eq!(plan.status, UndoPlanStatus::Complete);
    assert_eq!(
        plan.operations,
        vec![UndoOperation::RestoreProperties {
            entity_id: RemoteId::new("page-1"),
            expected_current: properties,
            previous: BTreeMap::from([
                ("New field".to_string(), PropertyValue::Null),
                (
                    "Status".to_string(),
                    PropertyValue::String("Todo".to_string()),
                ),
                (
                    "title".to_string(),
                    PropertyValue::String("Roadmap".to_string()),
                ),
            ]),
        }]
    );
}

#[test]
fn legacy_property_update_without_current_values_is_blocked() {
    let entry = journal_entry_with_shadow(
        vec![PushOperation::UpdateProperties {
            entity_id: RemoteId::new("page-1"),
            properties: BTreeMap::new(),
        }],
        shadow_with_frontmatter(),
    );

    let plan = plan_journal_undo(&entry);

    assert_eq!(plan.status, UndoPlanStatus::Blocked);
    assert!(plan.operations.is_empty());
    assert_eq!(
        plan.unsupported[0].code,
        "update_properties_missing_current_values"
    );
}

#[test]
fn move_entity_reverses_from_shadow_parent_and_title() {
    let entry = journal_entry_with_shadow(
        vec![PushOperation::MoveEntity {
            entity_id: RemoteId::new("page-1"),
            new_parent_id: RemoteId::new("new-parent"),
            new_parent_kind: locality_core::model::EntityKind::Page,
            new_title: "Renamed".to_string(),
            projected_path: "New Parent/Renamed/page.md".into(),
        }],
        shadow_with_frontmatter(),
    );

    let plan = plan_journal_undo(&entry);

    assert_eq!(plan.status, UndoPlanStatus::Complete);
    assert_eq!(
        plan.operations,
        vec![UndoOperation::RestoreEntityLocation {
            entity_id: RemoteId::new("page-1"),
            expected_parent_id: RemoteId::new("new-parent"),
            expected_title: "Renamed".to_string(),
            previous_parent_id: RemoteId::new("old-parent"),
            previous_title: "Roadmap".to_string(),
        }]
    );
}

#[test]
fn archive_entity_reverses_to_restore_archived_entity() {
    let entry = journal_entry(vec![PushOperation::ArchiveEntity {
        entity_id: RemoteId::new("page-1"),
    }]);

    let plan = plan_journal_undo(&entry);

    assert_eq!(plan.status, UndoPlanStatus::Complete);
    assert_eq!(
        plan.operations,
        vec![UndoOperation::RestoreArchivedEntity {
            entity_id: RemoteId::new("page-1"),
        }]
    );
    assert_eq!(
        serde_json::to_value(&plan.operations[0]).expect("serialize archive restore"),
        serde_json::json!({
            "type": "restore_archived_entity",
            "entity_id": "page-1",
        })
    );
}

#[test]
fn archive_entity_without_entity_preimage_is_blocked() {
    let mut entry = journal_entry(vec![PushOperation::ArchiveEntity {
        entity_id: RemoteId::new("page-1"),
    }]);
    entry.preimages.clear();

    let plan = plan_journal_undo(&entry);

    assert_eq!(plan.status, UndoPlanStatus::Blocked);
    assert!(plan.operations.is_empty());
    assert_eq!(plan.unsupported[0].code, "missing_entity_preimage");
}

#[test]
fn move_entity_blocks_when_parent_or_title_preimage_is_missing() {
    let operation = PushOperation::MoveEntity {
        entity_id: RemoteId::new("page-1"),
        new_parent_id: RemoteId::new("new-parent"),
        new_parent_kind: locality_core::model::EntityKind::Page,
        new_title: "Renamed".to_string(),
        projected_path: "New Parent/Renamed/page.md".into(),
    };
    let incomplete_frontmatter = [
        "loc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n",
        "loc:\n  id: page-1\n  type: page\n  parent: old-parent\n  synced_at: now\n  remote_edited_at: now\n",
    ];

    for frontmatter in incomplete_frontmatter {
        let entry = journal_entry_with_shadow(
            vec![operation.clone()],
            shadow().with_frontmatter(frontmatter),
        );
        let plan = plan_journal_undo(&entry);

        assert_eq!(plan.status, UndoPlanStatus::Blocked);
        assert!(plan.operations.is_empty());
        assert_eq!(plan.unsupported[0].code, "missing_entity_preimage");
    }
}

#[test]
fn entity_undo_that_needs_missing_preimages_is_blocked() {
    let cases = [
        PushOperation::UpdateEntityBody {
            entity_id: RemoteId::new("page-1"),
            body: "New body".to_string(),
        },
        PushOperation::UpdateProperties {
            entity_id: RemoteId::new("page-1"),
            properties: BTreeMap::from([(
                "Status".to_string(),
                PropertyValue::String("Done".to_string()),
            )]),
        },
        PushOperation::MoveEntity {
            entity_id: RemoteId::new("page-1"),
            new_parent_id: RemoteId::new("new-parent"),
            new_parent_kind: locality_core::model::EntityKind::Page,
            new_title: "Renamed".to_string(),
            projected_path: "New Parent/Renamed/page.md".into(),
        },
    ];

    for operation in cases {
        let mut entry = journal_entry(vec![operation]);
        entry.preimages.clear();
        let plan = plan_journal_undo(&entry);

        assert_eq!(plan.status, UndoPlanStatus::Blocked);
        assert!(plan.operations.is_empty());
        assert_eq!(plan.unsupported.len(), 1);
    }
}

#[test]
fn archive_created_entity_legacy_json_defaults_expected_state() {
    let operation: UndoOperation =
        serde_json::from_str(r#"{"type":"archive_created_entity","entity_id":"created-page-1"}"#)
            .expect("legacy undo operation");

    assert_eq!(
        operation,
        UndoOperation::ArchiveCreatedEntity {
            entity_id: RemoteId::new("created-page-1"),
            expected: None,
        }
    );
}

#[test]
fn mixed_plan_reports_partial_undo() {
    let entry = journal_entry(vec![
        PushOperation::UpdateBlock {
            block_id: RemoteId::new("paragraph-1"),
            content: "New paragraph.".to_string(),
        },
        PushOperation::AppendBlock {
            parent_id: RemoteId::new("page-1"),
            after: Some(RemoteId::new("paragraph-1")),
            content: "New paragraph.".to_string(),
        },
    ]);

    let plan = plan_journal_undo(&entry);

    assert_eq!(plan.status, UndoPlanStatus::Partial);
    assert_eq!(plan.operations.len(), 1);
    assert_eq!(plan.unsupported.len(), 1);
}

#[test]
fn missing_preimage_blocks_undo_for_preimage_dependent_operation() {
    let mut entry = journal_entry(vec![PushOperation::UpdateBlock {
        block_id: RemoteId::new("paragraph-1"),
        content: "New paragraph.".to_string(),
    }]);
    entry.preimages.clear();

    let plan = plan_journal_undo(&entry);

    assert_eq!(plan.status, UndoPlanStatus::Blocked);
    assert_eq!(plan.unsupported[0].code, "missing_block_preimage");
}

fn journal_entry(operations: Vec<PushOperation>) -> JournalEntry {
    journal_entry_with_shadow(operations, shadow())
}

fn journal_entry_with_shadow(
    operations: Vec<PushOperation>,
    shadow: ShadowDocument,
) -> JournalEntry {
    JournalEntry::new(
        PushId("push-1".to_string()),
        MountId::new("notion-main"),
        vec![RemoteId::new("page-1")],
        PushPlan::new(vec![RemoteId::new("page-1")], operations),
        JournalStatus::Reconciled,
    )
    .with_preimages(vec![JournalPreimage::from_shadow(shadow)])
}

fn shadow() -> ShadowDocument {
    ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        "# Roadmap\n\nOld paragraph.",
        9,
        [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
    )
    .expect("shadow")
}

fn shadow_with_frontmatter() -> ShadowDocument {
    shadow().with_frontmatter(
        "loc:\n  id: page-1\n  type: page\n  parent: old-parent\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n\"Status\": \"Todo\"\n\"Points\": 2\n",
    )
}

fn multi_block_shadow() -> ShadowDocument {
    ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        "A\n\nB\n\nC\n\nD",
        9,
        [
            RemoteId::new("a"),
            RemoteId::new("b"),
            RemoteId::new("c"),
            RemoteId::new("d"),
        ],
    )
    .expect("shadow")
}
use std::collections::BTreeMap;
