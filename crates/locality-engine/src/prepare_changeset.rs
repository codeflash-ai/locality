//! Deterministic changeset preparation.

use std::collections::BTreeSet;

use locality_core::canonical::{parse_canonical_markdown, render_canonical_markdown};
use locality_core::model::CanonicalDocument;
use locality_core::planner::{GuardrailPolicy, PushOperationKind};
use locality_core::portable::{LogicalPath, SourceOperationPlan};
use locality_core::push::{
    PushApproval, PushPipelineAction, PushPipelineRequest, PushPipelineResult, plan_push_pipeline,
};
use locality_core::readable_diff::{ReadableDiffOutput, readable_diff_for_file};
use locality_core::shadow::ShadowDocument;
use locality_core::validation::ValidationIssue;
use locality_core::{LocalityError, LocalityResult};

#[derive(Clone, Debug)]
pub struct PrepareChangesetRequest<'a> {
    pub logical_path: &'a LogicalPath,
    pub edited_markdown: &'a str,
    pub delivered_shadow: &'a ShadowDocument,
    pub supported_operations: &'a BTreeSet<PushOperationKind>,
    pub guardrail_policy: GuardrailPolicy,
    pub total_scope_entities: Option<usize>,
    pub approval: PushApproval,
    pub read_only: bool,
}

impl<'a> PrepareChangesetRequest<'a> {
    pub fn new(
        logical_path: &'a LogicalPath,
        edited_markdown: &'a str,
        delivered_shadow: &'a ShadowDocument,
        supported_operations: &'a BTreeSet<PushOperationKind>,
    ) -> Self {
        Self {
            logical_path,
            edited_markdown,
            delivered_shadow,
            supported_operations,
            guardrail_policy: GuardrailPolicy::default(),
            total_scope_entities: None,
            approval: PushApproval::default(),
            read_only: false,
        }
    }

    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    pub fn with_guardrail_policy(mut self, policy: GuardrailPolicy) -> Self {
        self.guardrail_policy = policy;
        self
    }

    pub fn with_total_scope_entities(mut self, total: usize) -> Self {
        self.total_scope_entities = Some(total);
        self
    }

    pub fn with_approval(mut self, approval: PushApproval) -> Self {
        self.approval = approval;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedChangeset {
    pub pipeline: PushPipelineResult,
    pub readable_diff: Option<ReadableDiffOutput>,
    pub source_operation_plan: Option<SourceOperationPlan>,
}

pub fn prepare_changeset(
    request: PrepareChangesetRequest<'_>,
) -> LocalityResult<PreparedChangeset> {
    let parsed = parse_canonical_markdown(request.edited_markdown).map_err(|error| {
        LocalityError::Validation(vec![ValidationIssue::new(
            "canonical_parse_error",
            request.logical_path.to_relative_path_buf(),
            error.line,
            error.message,
            Some("repair the canonical Markdown envelope before submitting".to_string()),
        )])
    })?;

    let mut pipeline_request = PushPipelineRequest::new(
        request.logical_path.to_relative_path_buf(),
        &parsed,
        request.delivered_shadow,
    )
    .with_guardrail_policy(request.guardrail_policy)
    .with_approval(request.approval)
    .read_only(request.read_only);
    if let Some(total) = request.total_scope_entities {
        pipeline_request = pipeline_request.with_total_mount_entities(total);
    }

    let mut pipeline = plan_push_pipeline(pipeline_request);
    classify_supported_operations(&mut pipeline, request.supported_operations);

    let source_operation_plan = pipeline
        .plan
        .as_ref()
        .map(SourceOperationPlan::try_from)
        .transpose()
        .map_err(|error| {
            LocalityError::Validation(vec![ValidationIssue::new(
                "portable_operation_path_invalid",
                request.logical_path.to_relative_path_buf(),
                None,
                error.to_string(),
                Some("use a normalized relative path for every changeset operation".to_string()),
            )])
        })?;
    let readable_diff = readable_diff(
        request.logical_path,
        request.edited_markdown,
        request.delivered_shadow,
        &pipeline,
    );

    Ok(PreparedChangeset {
        pipeline,
        readable_diff,
        source_operation_plan,
    })
}

pub fn classify_supported_operations(
    pipeline: &mut PushPipelineResult,
    supported: &BTreeSet<PushOperationKind>,
) {
    let Some(plan) = pipeline.plan.as_ref() else {
        return;
    };
    if !matches!(
        pipeline.action,
        PushPipelineAction::ProceedToApply
            | PushPipelineAction::ConfirmPlan
            | PushPipelineAction::ConfirmDangerousPlan
    ) {
        return;
    }

    let unsupported = plan
        .operations
        .iter()
        .map(|operation| operation.kind())
        .filter(|kind| !supported.contains(kind))
        .map(|kind| kind.as_str().to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if !unsupported.is_empty() {
        pipeline.action = PushPipelineAction::unsupported_operations(unsupported);
    }
}

fn readable_diff(
    logical_path: &LogicalPath,
    edited_markdown: &str,
    delivered_shadow: &ShadowDocument,
    pipeline: &PushPipelineResult,
) -> Option<ReadableDiffOutput> {
    if pipeline.plan.as_ref()?.is_empty() {
        return None;
    }
    let base = render_canonical_markdown(&CanonicalDocument::new(
        delivered_shadow.frontmatter.clone(),
        delivered_shadow.rendered_body.clone(),
    ));
    readable_diff_for_file(logical_path.as_str(), Some(&base), Some(edited_markdown))
}
