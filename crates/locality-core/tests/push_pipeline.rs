use std::collections::BTreeMap;

use locality_core::canonical::{ParsedCanonicalDocument, parse_canonical_markdown};
use locality_core::journal::{JournalApplyEffect, PushId, PushOperationId};
use locality_core::model::RemoteId;
use locality_core::planner::{
    GuardrailDecision, GuardrailPolicy, PlanDegradationKind, PlanSummary, PropertyValue,
    PushOperation, PushOperationKind, PushPlan,
};
use locality_core::push::{
    BodyDiffMode, PushApproval, PushPipelineAction, PushPipelineRequest, PushStage,
    plan_push_pipeline,
};
use locality_core::shadow::ShadowDocument;
use serde_json::json;

#[test]
fn clean_noop_push_finishes_without_apply() {
    let parsed = parsed_doc("# Roadmap\n\nSame paragraph.");
    let shadow = shadow("# Roadmap\n\nSame paragraph.", ["heading-1", "paragraph-1"]);

    let output = plan_push_pipeline(request(&parsed, &shadow));

    assert_eq!(output.action, PushPipelineAction::Noop);
    assert!(output.validation.is_clean());
    assert_eq!(output.plan.unwrap().operations, Vec::new());
    assert_eq!(
        output.completed_stages,
        vec![PushStage::ParseAndValidate, PushStage::Diff]
    );
}

#[test]
fn local_media_href_shape_only_change_is_noop() {
    let parsed =
        parsed_doc("![Image](/tmp/loc-content/notion-main/files/.loc/media/Roadmap/image-1.png)");
    let shadow = shadow("![Image](.loc/media/Roadmap/image-1.png)", ["image-1"]);

    let output = plan_push_pipeline(request(&parsed, &shadow));

    assert_eq!(output.action, PushPipelineAction::Noop);
    assert_eq!(output.plan.unwrap().operations, Vec::new());
}

#[test]
fn validation_failure_blocks_diff_and_apply() {
    let parsed =
        parse_canonical_markdown("---\ntitle: Missing Locality\n---\n# Body").expect("parseable");
    let shadow = shadow("# Body", ["heading-1"]);

    let output = plan_push_pipeline(request(&parsed, &shadow));

    assert_eq!(output.action, PushPipelineAction::FixValidation);
    assert!(output.plan.is_none());
    assert_eq!(output.validation.issues[0].code, "frontmatter_missing_loc");
    assert_eq!(output.completed_stages, vec![PushStage::ParseAndValidate]);
}

#[test]
fn read_only_mount_blocks_push_before_planning() {
    let parsed = parsed_doc("# Roadmap\n\nChanged paragraph.");
    let shadow = shadow("# Roadmap\n\nOld paragraph.", ["heading-1", "paragraph-1"]);

    let output = plan_push_pipeline(request(&parsed, &shadow).read_only(true));

    assert_eq!(output.action, PushPipelineAction::ReadOnlyBlocked);
    assert!(output.plan.is_none());
    assert!(output.completed_stages.is_empty());
}

#[test]
fn dangerous_plan_requires_confirm_without_confirm_flag() {
    let parsed = parsed_doc("");
    let shadow_body = (0..11)
        .map(|index| format!("Paragraph {index}."))
        .collect::<Vec<_>>()
        .join("\n\n");
    let shadow_ids: Vec<_> = (0..11).map(|index| format!("block-{index}")).collect();
    let shadow = shadow_from_ids(&shadow_body, &shadow_ids);

    let output = plan_push_pipeline(request(&parsed, &shadow));

    assert_eq!(output.action, PushPipelineAction::ConfirmDangerousPlan);
    assert!(matches!(
        output.guardrail,
        GuardrailDecision::ConfirmRequired { .. }
    ));
    assert_eq!(output.plan.as_ref().unwrap().summary.blocks_archived, 11);
}

#[test]
fn safe_plan_requires_yes_without_assume_yes() {
    let parsed = parsed_doc("# Roadmap\n\nChanged paragraph.");
    let shadow = shadow("# Roadmap\n\nOld paragraph.", ["heading-1", "paragraph-1"]);

    let output = plan_push_pipeline(request(&parsed, &shadow));

    assert_eq!(output.action, PushPipelineAction::ConfirmPlan);
    assert_eq!(output.plan.as_ref().unwrap().summary.blocks_updated, 1);
}

#[test]
fn safe_plan_with_assume_yes_proceeds_to_apply() {
    let parsed = parsed_doc("# Roadmap\n\nChanged paragraph.");
    let shadow = shadow("# Roadmap\n\nOld paragraph.", ["heading-1", "paragraph-1"]);

    let output = plan_push_pipeline(request(&parsed, &shadow).with_approval(PushApproval {
        assume_yes: true,
        confirm_dangerous: false,
    }));

    assert_eq!(output.action, PushPipelineAction::ProceedToApply);
}

#[test]
fn confirmed_dangerous_plan_proceeds_to_apply() {
    let parsed = parsed_doc("");
    let shadow_body = (0..11)
        .map(|index| format!("Paragraph {index}."))
        .collect::<Vec<_>>()
        .join("\n\n");
    let shadow_ids: Vec<_> = (0..11).map(|index| format!("block-{index}")).collect();
    let shadow = shadow_from_ids(&shadow_body, &shadow_ids);

    let output = plan_push_pipeline(request(&parsed, &shadow).with_approval(PushApproval {
        assume_yes: true,
        confirm_dangerous: true,
    }));

    assert_eq!(output.action, PushPipelineAction::ProceedToApply);
    assert!(matches!(
        output.guardrail,
        GuardrailDecision::ConfirmRequired { .. }
    ));
}

#[test]
fn mount_touch_guardrail_requires_confirm() {
    let parsed = parsed_doc("# Roadmap\n\nChanged paragraph.");
    let shadow = shadow("# Roadmap\n\nOld paragraph.", ["heading-1", "paragraph-1"]);
    let policy = GuardrailPolicy {
        max_archives_without_confirm: 100,
        max_mount_touch_percent_without_confirm: 5,
    };

    let output = plan_push_pipeline(
        request(&parsed, &shadow)
            .with_guardrail_policy(policy)
            .with_total_mount_entities(10),
    );

    assert_eq!(output.action, PushPipelineAction::ConfirmDangerousPlan);
    assert!(matches!(
        output.guardrail,
        GuardrailDecision::ConfirmRequired { .. }
    ));
}

#[test]
fn plan_degradations_surface_in_output() {
    let parsed = parsed_doc("- First rewrite.\n\nSecond rewrite.");
    let shadow = shadow("First paragraph.\n\n- Second item", ["block-1", "block-2"]);

    let output = plan_push_pipeline(request(&parsed, &shadow).with_approval(PushApproval {
        assume_yes: true,
        confirm_dangerous: false,
    }));

    assert_eq!(output.action, PushPipelineAction::ProceedToApply);
    assert_eq!(
        output.plan.unwrap().degradations[0].kind,
        PlanDegradationKind::AmbiguousBlockAlignment
    );
}

#[test]
fn directive_edit_fails_before_plan_output() {
    let parsed = parsed_doc("Intro.\n\n::loc{id=media-1 type=image title=\"Edited\"}");
    let shadow = shadow(
        "Intro.\n\n::loc{id=media-1 type=image title=\"Original\"}",
        ["block-1"],
    );

    let output = plan_push_pipeline(request(&parsed, &shadow));

    assert_eq!(output.action, PushPipelineAction::FixValidation);
    assert!(output.plan.is_none());
    assert_eq!(output.validation.issues[0].code, "directive_mangled");
    assert_eq!(
        output.validation.issues[0].file.to_string_lossy(),
        "Roadmap.md"
    );
}

#[test]
fn update_append_and_delete_plans_are_wrapped() {
    let parsed = parsed_doc("# Roadmap\n\nChanged paragraph.\n\nAdded paragraph.");
    let shadow = shadow(
        "# Roadmap\n\nOld paragraph.\n\n::loc{id=media-1 type=image title=\"Archived\"}",
        ["heading-1", "paragraph-1"],
    );

    let output = plan_push_pipeline(request(&parsed, &shadow).with_approval(PushApproval {
        assume_yes: true,
        confirm_dangerous: false,
    }));
    let plan = output.plan.expect("plan");

    assert_eq!(output.action, PushPipelineAction::ProceedToApply);
    assert_eq!(plan.summary.blocks_updated, 1);
    assert_eq!(plan.summary.blocks_created, 1);
    assert_eq!(plan.summary.blocks_archived, 1);
    assert_eq!(
        plan.operations,
        vec![
            PushOperation::UpdateBlock {
                block_id: RemoteId::new("paragraph-1"),
                content: "Changed paragraph.".to_string(),
            },
            PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("paragraph-1")),
                content: "Added paragraph.".to_string(),
            },
            PushOperation::ArchiveBlock {
                block_id: RemoteId::new("media-1"),
            },
        ]
    );
}

#[test]
fn frontmatter_property_edits_plan_property_update() {
    let parsed = parse_canonical_markdown(
        "---\nloc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n\"Status\": \"Done\"\n\"Points\": 3\n---\nSame body.",
    )
    .expect("canonical document");
    let shadow = ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        "Same body.",
        10,
        [RemoteId::new("paragraph-1")],
    )
    .expect("shadow")
    .with_frontmatter(
        "loc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n\"Status\": \"Todo\"\n\"Points\": 2\n",
    );

    let output = plan_push_pipeline(request(&parsed, &shadow).with_approval(PushApproval {
        assume_yes: true,
        confirm_dangerous: false,
    }));
    let plan = output.plan.expect("plan");

    assert_eq!(output.action, PushPipelineAction::ProceedToApply);
    assert_eq!(plan.summary.properties_updated, 2);
    assert_eq!(
        plan.operations,
        vec![PushOperation::UpdateProperties {
            entity_id: RemoteId::new("page-1"),
            properties: [
                ("Points".to_string(), PropertyValue::Number("3".to_string())),
                (
                    "Status".to_string(),
                    PropertyValue::String("Done".to_string())
                ),
            ]
            .into_iter()
            .collect(),
        }]
    );
}

#[test]
fn whole_entity_body_mode_emits_one_body_update_and_keeps_property_updates() {
    let parsed = parse_canonical_markdown(
        "---\nloc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n\"Status\": \"Done\"\n---\nFirst changed paragraph.\n\nSecond changed paragraph.",
    )
    .expect("canonical document");
    let shadow = ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        "Old paragraph.",
        9,
        [RemoteId::new("paragraph-1")],
    )
    .expect("shadow")
    .with_frontmatter(
        "loc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n\"Status\": \"Todo\"\n",
    );

    let output = plan_push_pipeline(
        request(&parsed, &shadow).with_body_diff_mode(BodyDiffMode::WholeEntity),
    );
    let plan = output.plan.expect("plan");

    assert_eq!(
        plan.operations,
        vec![
            PushOperation::UpdateProperties {
                entity_id: RemoteId::new("page-1"),
                properties: [(
                    "Status".to_string(),
                    PropertyValue::String("Done".to_string()),
                )]
                .into_iter()
                .collect(),
            },
            PushOperation::UpdateEntityBody {
                entity_id: RemoteId::new("page-1"),
                body: "First changed paragraph.\n\nSecond changed paragraph.".to_string(),
            },
        ]
    );
    assert_eq!(plan.summary.entity_bodies_updated, 1);
    assert_eq!(plan.summary.blocks_created, 0);
    assert_eq!(plan.summary.blocks_updated, 0);
    assert_eq!(plan.summary.blocks_archived, 0);
}

#[test]
fn whole_entity_body_mode_omits_body_operation_when_body_is_unchanged() {
    let parsed = parsed_doc("Same body.");
    let shadow = shadow("Same body.", ["paragraph-1"]);

    let output = plan_push_pipeline(
        request(&parsed, &shadow).with_body_diff_mode(BodyDiffMode::WholeEntity),
    );

    assert_eq!(output.action, PushPipelineAction::Noop);
    assert_eq!(output.plan.expect("plan").operations, Vec::new());
}

#[test]
fn whole_entity_body_erase_requires_destructive_confirmation() {
    let parsed = parsed_doc("");
    let shadow = shadow("Existing description.", ["paragraph-1"]);

    let output = plan_push_pipeline(
        request(&parsed, &shadow)
            .with_body_diff_mode(BodyDiffMode::WholeEntity)
            .with_approval(PushApproval {
                assume_yes: true,
                confirm_dangerous: false,
            }),
    );

    assert_eq!(output.action, PushPipelineAction::ConfirmDangerousPlan);
    assert_eq!(
        output.guardrail,
        GuardrailDecision::ConfirmRequired {
            reasons: vec!["1 entity body would be cleared".to_string()],
        }
    );
    assert!(matches!(
        output.plan.expect("plan").operations.as_slice(),
        [PushOperation::UpdateEntityBody { body, .. }] if body.is_empty()
    ));
}

#[test]
fn whole_entity_body_mode_treats_directive_looking_lines_as_opaque_markdown() {
    let body = "Description text.\n\n::loc{id=literal type=not-closed\n\n::afs{missing-fields}";
    let parsed = parsed_doc(body);
    let shadow = shadow("Old description.", ["paragraph-1"]);

    let output = plan_push_pipeline(
        request(&parsed, &shadow).with_body_diff_mode(BodyDiffMode::WholeEntity),
    );

    assert!(output.validation.is_clean());
    assert_eq!(
        output.plan.expect("plan").operations,
        vec![PushOperation::UpdateEntityBody {
            entity_id: RemoteId::new("page-1"),
            body: body.to_string(),
        }]
    );
}

#[test]
fn push_pipeline_request_defaults_to_block_body_diff_mode() {
    let parsed = parsed_doc("Changed body.");
    let shadow = shadow("Old body.", ["paragraph-1"]);

    let plan = plan_push_pipeline(request(&parsed, &shadow))
        .plan
        .expect("plan");

    assert!(matches!(
        plan.operations.as_slice(),
        [PushOperation::UpdateBlock { .. }]
    ));
}

#[test]
fn legacy_property_update_operation_json_stays_readable() {
    let operation: PushOperation = serde_json::from_value(json!({
        "type": "update_properties",
        "entity_id": "page-1",
        "keys": ["Status"],
    }))
    .expect("legacy operation");

    assert_eq!(
        operation,
        PushOperation::UpdateProperties {
            entity_id: RemoteId::new("page-1"),
            properties: BTreeMap::new(),
        }
    );
}

#[test]
fn move_entity_operation_json_and_summary_are_stable() {
    let operation = PushOperation::MoveEntity {
        entity_id: RemoteId::new("page-child"),
        new_parent_id: RemoteId::new("page-parent"),
        new_parent_kind: locality_core::model::EntityKind::Page,
        new_title: "Renamed Child".to_string(),
        projected_path: "Parent/Renamed Child/page.md".into(),
    };
    let json = serde_json::to_value(&operation).expect("serialize move entity");

    assert_eq!(
        json,
        json!({
            "type": "move_entity",
            "entity_id": "page-child",
            "new_parent_id": "page-parent",
            "new_parent_kind": "page",
            "new_title": "Renamed Child",
            "projected_path": "Parent/Renamed Child/page.md",
        })
    );
    assert_eq!(
        serde_json::from_value::<PushOperation>(json).expect("deserialize move entity"),
        operation
    );
    let plan =
        locality_core::planner::PushPlan::new(vec![RemoteId::new("page-child")], vec![operation]);

    assert_eq!(PushOperationKind::MoveEntity.as_str(), "move_entity");
    assert_eq!(plan.summary.entities_moved, 1);
    assert_eq!(plan.summary.properties_updated, 0);
}

#[test]
fn update_entity_body_durable_names_and_summary_are_stable() {
    let operation = PushOperation::UpdateEntityBody {
        entity_id: RemoteId::new("issue-1"),
        body: "First.\n\nSecond.".to_string(),
    };
    let operation_json = serde_json::to_value(&operation).expect("serialize operation");

    assert_eq!(
        operation_json,
        json!({
            "type": "update_entity_body",
            "entity_id": "issue-1",
            "body": "First.\n\nSecond.",
        })
    );
    assert_eq!(operation.kind(), PushOperationKind::UpdateEntityBody);
    assert_eq!(
        PushOperationKind::UpdateEntityBody.as_str(),
        "update_entity_body"
    );
    assert_eq!(
        PushOperationId::for_operation(&PushId("push-1".to_string()), 2, &operation).0,
        "push-1:2:update_entity_body:issue-1"
    );
    assert_eq!(
        PushPlan::new(vec![RemoteId::new("issue-1")], vec![operation])
            .summary
            .entity_bodies_updated,
        1
    );

    let effect = JournalApplyEffect::UpdatedEntityBody {
        operation_id: PushOperationId("push-1:2:update_entity_body:issue-1".to_string()),
        operation_index: 2,
        entity_id: RemoteId::new("issue-1"),
    };
    assert_eq!(
        serde_json::to_value(effect).expect("serialize apply effect"),
        json!({
            "type": "updated_entity_body",
            "operation_id": "push-1:2:update_entity_body:issue-1",
            "operation_index": 2,
            "entity_id": "issue-1",
        })
    );
}

#[test]
fn legacy_plan_summary_defaults_entity_body_update_count() {
    let summary: PlanSummary = serde_json::from_value(json!({
        "blocks_created": 0,
        "blocks_updated": 0,
        "blocks_replaced": 0,
        "blocks_moved": 0,
        "media_updated": 0,
        "blocks_archived": 0,
        "entities_created": 0,
        "entities_archived": 0,
        "entities_moved": 0,
        "properties_updated": 0,
    }))
    .expect("legacy summary");

    assert_eq!(summary.entity_bodies_updated, 0);
}

fn request<'a>(
    parsed: &'a ParsedCanonicalDocument,
    shadow: &'a ShadowDocument,
) -> PushPipelineRequest<'a> {
    PushPipelineRequest::new("Roadmap.md", parsed, shadow)
}

fn parsed_doc(body: &str) -> ParsedCanonicalDocument {
    parse_canonical_markdown(&format!(
        "---\nloc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n---\n{body}"
    ))
    .expect("canonical document")
}

fn shadow<const N: usize>(body: &str, ids: [&str; N]) -> ShadowDocument {
    ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        body,
        8,
        ids.into_iter().map(RemoteId::new),
    )
    .expect("shadow")
}

fn shadow_from_ids(body: &str, ids: &[String]) -> ShadowDocument {
    ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        body,
        8,
        ids.iter().cloned().map(RemoteId::new),
    )
    .expect("shadow")
}
