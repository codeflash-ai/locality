use std::collections::BTreeSet;

use locality_core::model::RemoteId;
use locality_core::planner::{GuardrailPolicy, PushOperationKind};
use locality_core::portable::{LogicalPath, SourceOperation};
use locality_core::push::PushPipelineAction;
use locality_core::shadow::ShadowDocument;
use locality_engine::prepare_changeset::{PrepareChangesetRequest, prepare_changeset};

#[test]
fn prepares_the_same_deterministic_operation_and_readable_diff() {
    let path = LogicalPath::new("Projects/Roadmap/page.md").expect("path");
    let shadow = shadow("Old paragraph.\n");
    let supported = PushOperationKind::all().into_iter().collect();
    let edited = canonical("Changed paragraph.\n");

    let prepared = prepare_changeset(PrepareChangesetRequest::new(
        &path, &edited, &shadow, &supported,
    ))
    .expect("prepare");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::ConfirmPlan);
    assert_eq!(
        prepared
            .source_operation_plan
            .expect("portable plan")
            .operations,
        vec![SourceOperation::UpdateBlock {
            block_id: RemoteId::new("block-1"),
            content: "Changed paragraph.".to_string(),
        }]
    );
    let diff = prepared.readable_diff.expect("readable diff");
    assert_eq!(diff.files[0].path, "Projects/Roadmap/page.md");
    assert!(diff.text.contains("-Old paragraph."), "{}", diff.text);
    assert!(diff.text.contains("+Changed paragraph."), "{}", diff.text);
}

#[test]
fn read_only_entries_never_produce_a_changeset_plan() {
    let path = LogicalPath::new("Projects/Readonly/page.md").expect("path");
    let shadow = shadow("Old paragraph.\n");
    let supported = PushOperationKind::all().into_iter().collect();
    let edited = canonical("Changed paragraph.\n");

    let prepared = prepare_changeset(
        PrepareChangesetRequest::new(&path, &edited, &shadow, &supported).read_only(true),
    )
    .expect("prepare");

    assert_eq!(
        prepared.pipeline.action,
        PushPipelineAction::ReadOnlyBlocked
    );
    assert_eq!(prepared.source_operation_plan, None);
    assert_eq!(prepared.readable_diff, None);
}

#[test]
fn unsupported_source_operations_are_classified_before_apply() {
    let path = LogicalPath::new("Projects/Roadmap/page.md").expect("path");
    let shadow = shadow("Old paragraph.\n");
    let edited = canonical("Changed paragraph.\n");

    let prepared = prepare_changeset(PrepareChangesetRequest::new(
        &path,
        &edited,
        &shadow,
        &BTreeSet::new(),
    ))
    .expect("prepare");

    assert_eq!(
        prepared.pipeline.action,
        PushPipelineAction::unsupported_operations(vec!["update_block".to_string()])
    );
}

#[test]
fn validation_failures_do_not_emit_an_operation_plan() {
    let path = LogicalPath::new("Projects/Roadmap/page.md").expect("path");
    let shadow = shadow("Old paragraph.\n");
    let supported = PushOperationKind::all().into_iter().collect();
    let edited = "---\ntitle: Roadmap\n---\nChanged paragraph.\n";

    let prepared = prepare_changeset(PrepareChangesetRequest::new(
        &path, edited, &shadow, &supported,
    ))
    .expect("structured validation result");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::FixValidation);
    assert_eq!(prepared.source_operation_plan, None);
    assert_eq!(
        prepared.pipeline.validation.issues[0].code,
        "frontmatter_missing_loc"
    );
}

#[test]
fn destructive_plan_uses_existing_guardrail_policy() {
    let path = LogicalPath::new("Projects/Roadmap/page.md").expect("path");
    let shadow = shadow("Old paragraph.\n");
    let supported = PushOperationKind::all().into_iter().collect();
    let edited = canonical("");
    let policy = GuardrailPolicy {
        max_archives_without_confirm: 0,
        max_mount_touch_percent_without_confirm: 100,
    };

    let prepared = prepare_changeset(
        PrepareChangesetRequest::new(&path, &edited, &shadow, &supported)
            .with_guardrail_policy(policy),
    )
    .expect("prepare");

    assert_eq!(
        prepared.pipeline.action,
        PushPipelineAction::ConfirmDangerousPlan
    );
}

fn shadow(body: &str) -> ShadowDocument {
    ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        body,
        9,
        [RemoteId::new("block-1")],
    )
    .expect("shadow")
    .with_frontmatter(
        "loc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: v1\ntitle: Roadmap\n",
    )
}

fn canonical(body: &str) -> String {
    format!(
        "---\nloc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: v1\ntitle: Roadmap\n---\n{body}"
    )
}
