use std::collections::BTreeMap;

use afs_core::canonical::{ParsedCanonicalDocument, parse_canonical_markdown};
use afs_core::model::RemoteId;
use afs_core::planner::{
    GuardrailDecision, GuardrailPolicy, PlanDegradationKind, PropertyValue, PushOperation,
};
use afs_core::push::{
    PushApproval, PushPipelineAction, PushPipelineRequest, PushStage, plan_push_pipeline,
};
use afs_core::shadow::ShadowDocument;
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
    let parsed = parsed_doc(
        "![Image](/home/mohammed/.afs/content/notion-main/files/.afs/media/Roadmap/image-1.png)",
    );
    let shadow = shadow("![Image](.afs/media/Roadmap/image-1.png)", ["image-1"]);

    let output = plan_push_pipeline(request(&parsed, &shadow));

    assert_eq!(output.action, PushPipelineAction::Noop);
    assert_eq!(output.plan.unwrap().operations, Vec::new());
}

#[test]
fn validation_failure_blocks_diff_and_apply() {
    let parsed =
        parse_canonical_markdown("---\ntitle: Missing AFS\n---\n# Body").expect("parseable");
    let shadow = shadow("# Body", ["heading-1"]);

    let output = plan_push_pipeline(request(&parsed, &shadow));

    assert_eq!(output.action, PushPipelineAction::FixValidation);
    assert!(output.plan.is_none());
    assert_eq!(output.validation.issues[0].code, "frontmatter_missing_afs");
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
    let parsed = parsed_doc("Intro.\n\n::afs{id=media-1 type=image title=\"Edited\"}");
    let shadow = shadow(
        "Intro.\n\n::afs{id=media-1 type=image title=\"Original\"}",
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
        "# Roadmap\n\nOld paragraph.\n\n::afs{id=media-1 type=image title=\"Archived\"}",
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
        "---\nafs:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n\"Status\": \"Done\"\n\"Points\": 3\n---\nSame body.",
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
        "afs:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n\"Status\": \"Todo\"\n\"Points\": 2\n",
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

fn request<'a>(
    parsed: &'a ParsedCanonicalDocument,
    shadow: &'a ShadowDocument,
) -> PushPipelineRequest<'a> {
    PushPipelineRequest::new("Roadmap.md", parsed, shadow)
}

fn parsed_doc(body: &str) -> ParsedCanonicalDocument {
    parse_canonical_markdown(&format!(
        "---\nafs:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n---\n{body}"
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
