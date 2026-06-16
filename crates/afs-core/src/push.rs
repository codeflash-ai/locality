//! Explicit push pipeline coordination types.
//!
//! v1 keeps writes explicit by default. The first layer models the inspectable
//! validation, diff, and confirmation stages. The second layer executes an
//! already-approved plan through host-supplied concurrency, connector-apply, and
//! reconcile hooks while maintaining the write-ahead journal.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::canonical::ParsedCanonicalDocument;
use crate::diff::{BlockDiffEngine, DiffEngine};
use crate::journal::{
    JournalApplyEffect, JournalEntry, JournalPreimage, JournalStatus, JournalStore, PushId,
    PushOperationId,
};
use crate::model::{MountId, RemoteId};
use crate::planner::{GuardrailDecision, GuardrailPolicy, PushPlan};
use crate::shadow::ShadowDocument;
use crate::validation::{
    ValidationIssue, ValidationReport, validate_directive_syntax, validate_frontmatter_identity,
};
use crate::{AfsError, AfsResult};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushStage {
    /// Parsed the canonical file and ran local validation that does not require
    /// remote I/O.
    ParseAndValidate,
    /// Produced a connector-neutral push plan from the edited file and shadow
    /// snapshot.
    Diff,
    /// Evaluated the human/agent confirmation policy for the plan.
    PlanAndConfirm,
    /// Checked remote concurrency and applied the plan through connector code.
    ConcurrencyCheckAndApply,
    /// Wrote the journal and reconciled post-apply remote state.
    JournalAndReconcile,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushPipelineResult {
    /// Structured validation issues that should be fixable by an agent loop.
    pub validation: ValidationReport,
    /// Planned remote mutations, present only after validation and diffing
    /// succeed.
    pub plan: Option<PushPlan>,
    /// Destructive-change guardrail result for the planned mutations.
    pub guardrail: GuardrailDecision,
    /// The next action a CLI or daemon should take.
    pub action: PushPipelineAction,
    /// Stages completed before returning this result.
    pub completed_stages: Vec<PushStage>,
}

#[derive(Clone, Debug)]
pub struct PushPipelineRequest<'a> {
    /// Canonical file path used for diagnostics.
    pub target_path: PathBuf,
    /// Parsed current local document.
    pub edited: &'a ParsedCanonicalDocument,
    /// Last-synced body and block snapshot for this entity.
    pub shadow: &'a ShadowDocument,
    /// Confirmation thresholds for destructive or broad plans.
    pub guardrail_policy: GuardrailPolicy,
    /// Optional mount size used to evaluate broad-touch guardrails.
    pub total_mount_entities: Option<usize>,
    /// Caller approval flags such as `-y` and `--confirm`.
    pub approval: PushApproval,
    /// Whether this target belongs to a read-only mount.
    pub read_only: bool,
}

impl<'a> PushPipelineRequest<'a> {
    pub fn new(
        target_path: impl Into<PathBuf>,
        edited: &'a ParsedCanonicalDocument,
        shadow: &'a ShadowDocument,
    ) -> Self {
        Self {
            target_path: target_path.into(),
            edited,
            shadow,
            guardrail_policy: GuardrailPolicy::default(),
            total_mount_entities: None,
            approval: PushApproval::default(),
            read_only: false,
        }
    }

    pub fn with_guardrail_policy(mut self, policy: GuardrailPolicy) -> Self {
        self.guardrail_policy = policy;
        self
    }

    pub fn with_total_mount_entities(mut self, total: usize) -> Self {
        self.total_mount_entities = Some(total);
        self
    }

    pub fn with_approval(mut self, approval: PushApproval) -> Self {
        self.approval = approval;
        self
    }

    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushApproval {
    /// Equivalent to `afs push -y`: allow safe non-empty plans to proceed
    /// without an interactive prompt.
    pub assume_yes: bool,
    /// Equivalent to `afs push --confirm`: allow plans that tripped destructive
    /// guardrails to proceed.
    pub confirm_dangerous: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushPipelineAction {
    /// Validation and diffing succeeded, but there is nothing to apply.
    Noop,
    /// Stop and repair structured validation issues before retrying.
    FixValidation,
    /// Ask for normal approval, or rerun with `assume_yes`.
    ConfirmPlan,
    /// Ask for explicit dangerous-plan approval, or rerun with
    /// `confirm_dangerous`.
    ConfirmDangerousPlan,
    /// The plan is approved for connector apply.
    ProceedToApply,
    /// Stop because the mount is configured read-only.
    ReadOnlyBlocked,
    /// Stop because the active connector cannot apply one or more planned
    /// operation kinds.
    UnsupportedOperations {
        operations: Vec<String>,
        message: String,
        suggested_fix: String,
    },
}

impl PushPipelineAction {
    pub fn unsupported_operations(operations: Vec<String>) -> Self {
        let joined = operations.join(", ");
        Self::UnsupportedOperations {
            operations,
            message: format!("connector cannot apply: {joined}"),
            suggested_fix:
                "reorder edits to avoid unsupported operations, or wait for connector support"
                    .to_string(),
        }
    }
}

pub fn evaluate_guardrails(
    plan: &PushPlan,
    policy: &GuardrailPolicy,
    total_mount_entities: Option<usize>,
) -> GuardrailDecision {
    let mut reasons = Vec::new();
    let archive_count = plan.summary.destructive_archive_count();

    if archive_count > policy.max_archives_without_confirm {
        reasons.push(format!("{archive_count} blocks or pages would be archived"));
    }

    if let Some(total_mount_entities) = total_mount_entities
        && plan.touches_more_than_percent(
            total_mount_entities,
            policy.max_mount_touch_percent_without_confirm,
        )
    {
        reasons.push(format!(
            "plan touches more than {}% of the mount",
            policy.max_mount_touch_percent_without_confirm
        ));
    }

    if reasons.is_empty() {
        GuardrailDecision::Proceed
    } else {
        GuardrailDecision::ConfirmRequired { reasons }
    }
}

pub fn plan_push_pipeline(request: PushPipelineRequest<'_>) -> PushPipelineResult {
    if request.read_only {
        return PushPipelineResult {
            validation: ValidationReport::clean(),
            plan: None,
            guardrail: GuardrailDecision::Proceed,
            action: PushPipelineAction::ReadOnlyBlocked,
            completed_stages: Vec::new(),
        };
    }

    let mut completed_stages = Vec::new();
    let mut validation = ValidationReport::clean();
    validation.extend(validate_frontmatter_identity(
        request.edited,
        request.target_path.clone(),
    ));
    validation.extend(validate_directive_syntax(
        request.edited,
        request.target_path.clone(),
    ));
    completed_stages.push(PushStage::ParseAndValidate);

    if !validation.is_clean() {
        return PushPipelineResult {
            validation,
            plan: None,
            guardrail: GuardrailDecision::Proceed,
            action: PushPipelineAction::FixValidation,
            completed_stages,
        };
    }

    let diff_engine =
        BlockDiffEngine::new().with_edited_body_start_line(request.edited.body_start_line);
    let plan = match diff_engine.plan_push(request.shadow, &request.edited.document) {
        Ok(plan) => plan,
        Err(crate::AfsError::Validation(issues)) => {
            validation
                .issues
                .extend(issues.into_iter().map(|mut issue| {
                    if issue.file.as_os_str().is_empty() {
                        issue.file = request.target_path.clone();
                    }
                    issue
                }));
            return PushPipelineResult {
                validation,
                plan: None,
                guardrail: GuardrailDecision::Proceed,
                action: PushPipelineAction::FixValidation,
                completed_stages,
            };
        }
        Err(_) => {
            validation.push(ValidationIssue::new(
                "push_pipeline_diff_error",
                request.target_path.clone(),
                None,
                "diff planning failed unexpectedly",
                Some("retry after refreshing the shadow snapshot".to_string()),
            ));
            return PushPipelineResult {
                validation,
                plan: None,
                guardrail: GuardrailDecision::Proceed,
                action: PushPipelineAction::FixValidation,
                completed_stages,
            };
        }
    };
    completed_stages.push(PushStage::Diff);

    if plan.is_empty() {
        return PushPipelineResult {
            validation,
            plan: Some(plan),
            guardrail: GuardrailDecision::Proceed,
            action: PushPipelineAction::Noop,
            completed_stages,
        };
    }

    let guardrail = evaluate_guardrails(
        &plan,
        &request.guardrail_policy,
        request.total_mount_entities,
    );
    completed_stages.push(PushStage::PlanAndConfirm);

    let action = match &guardrail {
        GuardrailDecision::Proceed if request.approval.assume_yes => {
            PushPipelineAction::ProceedToApply
        }
        GuardrailDecision::Proceed => PushPipelineAction::ConfirmPlan,
        GuardrailDecision::ConfirmRequired { .. } if request.approval.confirm_dangerous => {
            PushPipelineAction::ProceedToApply
        }
        GuardrailDecision::ConfirmRequired { .. } => PushPipelineAction::ConfirmDangerousPlan,
    };

    PushPipelineResult {
        validation,
        plan: Some(plan),
        guardrail,
        action,
        completed_stages,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushExecutionRequest {
    /// Stable push identifier used by journals and connector idempotency keys.
    pub push_id: PushId,
    /// Mount being mutated.
    pub mount_id: MountId,
    /// Validated and approved pipeline result to execute.
    pub pipeline: PushPipelineResult,
    /// Pre-push canonical snapshots used by future resume and undo flows.
    pub preimages: Vec<JournalPreimage>,
    /// Synced Tree remote versions for entities that the connector should
    /// compare immediately before apply.
    pub remote_preconditions: Vec<RemotePrecondition>,
}

impl PushExecutionRequest {
    pub fn new(push_id: PushId, mount_id: MountId, pipeline: PushPipelineResult) -> Self {
        Self {
            push_id,
            mount_id,
            pipeline,
            preimages: Vec::new(),
            remote_preconditions: Vec::new(),
        }
    }

    pub fn with_preimages(mut self, preimages: Vec<JournalPreimage>) -> Self {
        self.preimages = preimages;
        self
    }

    pub fn with_remote_preconditions(mut self, preconditions: Vec<RemotePrecondition>) -> Self {
        self.remote_preconditions = preconditions;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushExecutionResult {
    /// Stable push identifier from the request.
    pub push_id: PushId,
    /// Terminal execution action.
    pub action: PushExecutionAction,
    /// Remote entities reported changed by the connector apply hook.
    pub changed_remote_ids: Vec<RemoteId>,
    /// Operation-level effects reported by the connector apply hook.
    pub apply_effects: Vec<JournalApplyEffect>,
    /// Remote entities reconciled from post-apply read-back.
    pub reconciled_remote_ids: Vec<RemoteId>,
    /// Final journal status, or `None` when execution did not start.
    pub journal_status: Option<JournalStatus>,
    /// Pipeline stages plus execution stages completed before returning.
    pub completed_stages: Vec<PushStage>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushExecutionAction {
    /// The validation/diff/confirmation pipeline has not approved remote apply.
    NotReady { pipeline_action: PushPipelineAction },
    /// The connector applied the plan and post-apply reconciliation completed.
    Reconciled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemotePrecondition {
    pub remote_id: RemoteId,
    pub remote_edited_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushConcurrencyRequest<'a> {
    /// Stable push identifier available for source-side request correlation.
    pub push_id: &'a PushId,
    /// Mount whose remote state should be checked.
    pub mount_id: &'a MountId,
    /// Approved plan that is about to be applied.
    pub plan: &'a PushPlan,
    /// Stable idempotency keys for each operation in `plan.operations`.
    pub operation_ids: &'a [PushOperationId],
    /// Remote entities covered by the journal entry.
    pub remote_ids: &'a [RemoteId],
    /// Synced Tree remote versions for compare-and-swap checks.
    pub remote_preconditions: &'a [RemotePrecondition],
}

/// Hook for compare-and-swap style remote freshness checks before apply.
pub trait PushConcurrencyCheck {
    fn check(&mut self, request: PushConcurrencyRequest<'_>) -> AfsResult<()>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushApplyRequest<'a> {
    /// Stable push identifier used for connector idempotency keys.
    pub push_id: &'a PushId,
    /// Mount receiving the mutation.
    pub mount_id: &'a MountId,
    /// Connector-neutral operations to apply remotely.
    pub plan: &'a PushPlan,
    /// Stable idempotency keys for each operation in `plan.operations`.
    pub operation_ids: &'a [PushOperationId],
    /// Remote entities covered by the journal entry.
    pub remote_ids: &'a [RemoteId],
    /// Synced Tree remote versions available to source-specific apply code.
    pub remote_preconditions: &'a [RemotePrecondition],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushApplyResult {
    /// Remote entities the connector reports as changed.
    pub changed_remote_ids: Vec<RemoteId>,
    /// Durable operation-level effects needed by resume and undo flows.
    pub effects: Vec<JournalApplyEffect>,
}

/// Hook that turns a validated push plan into source-specific remote writes.
pub trait PushApplier {
    fn apply(&mut self, request: PushApplyRequest<'_>) -> AfsResult<PushApplyResult>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushReconcileRequest<'a> {
    /// Stable push identifier from the apply step.
    pub push_id: &'a PushId,
    /// Mount whose post-apply state should be reconciled.
    pub mount_id: &'a MountId,
    /// Plan that was applied remotely.
    pub plan: &'a PushPlan,
    /// Remote entities changed by apply.
    pub changed_remote_ids: &'a [RemoteId],
    /// Durable operation-level effects returned by apply.
    pub apply_effects: &'a [JournalApplyEffect],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushReconcileResult {
    /// Remote entities whose post-apply state was read back and accepted.
    pub reconciled_remote_ids: Vec<RemoteId>,
}

/// Hook for post-apply read-back, shadow refresh, and divergence detection.
pub trait PushReconciler {
    fn reconcile(&mut self, request: PushReconcileRequest<'_>) -> AfsResult<PushReconcileResult>;
}

/// Combined host for daemon-owned push execution.
///
/// This avoids splitting journal writes and post-apply reconciliation across
/// distinct mutable objects. The same state owner prepares the journal, applies
/// source mutations through connector hooks, reconciles post-apply remote state,
/// and advances the journal to `Reconciled`.
pub trait PushExecutionHost:
    JournalStore + PushConcurrencyCheck + PushApplier + PushReconciler
{
}

impl<T> PushExecutionHost for T where
    T: JournalStore + PushConcurrencyCheck + PushApplier + PushReconciler
{
}

/// Executes an approved push plan through one daemon-owned host.
pub fn execute_journaled_push_with_host<H>(
    host: &mut H,
    request: PushExecutionRequest,
) -> AfsResult<PushExecutionResult>
where
    H: PushExecutionHost,
{
    if request.pipeline.action != PushPipelineAction::ProceedToApply {
        return Ok(PushExecutionResult {
            push_id: request.push_id,
            action: PushExecutionAction::NotReady {
                pipeline_action: request.pipeline.action,
            },
            changed_remote_ids: Vec::new(),
            apply_effects: Vec::new(),
            reconciled_remote_ids: Vec::new(),
            journal_status: None,
            completed_stages: request.pipeline.completed_stages,
        });
    }

    let Some(plan) = request.pipeline.plan else {
        return Err(AfsError::InvalidState(
            "push pipeline approved apply without a plan".to_string(),
        ));
    };
    let remote_ids = plan.affected_entities.clone();
    let operation_ids = plan
        .operations
        .iter()
        .enumerate()
        .map(|(index, operation)| {
            PushOperationId::for_operation(&request.push_id, index, operation)
        })
        .collect::<Vec<_>>();

    host.append(
        JournalEntry::new(
            request.push_id.clone(),
            request.mount_id.clone(),
            remote_ids.clone(),
            plan.clone(),
            JournalStatus::Prepared,
        )
        .with_preimages(request.preimages.clone()),
    )?;
    host.update_status(&request.push_id, JournalStatus::Applying)?;

    if let Err(error) = host.check(PushConcurrencyRequest {
        push_id: &request.push_id,
        mount_id: &request.mount_id,
        plan: &plan,
        operation_ids: &operation_ids,
        remote_ids: &remote_ids,
        remote_preconditions: &request.remote_preconditions,
    }) {
        mark_failed(host, &request.push_id, &error)?;
        return Err(error);
    }

    let apply_result = match host.apply(PushApplyRequest {
        push_id: &request.push_id,
        mount_id: &request.mount_id,
        plan: &plan,
        operation_ids: &operation_ids,
        remote_ids: &remote_ids,
        remote_preconditions: &request.remote_preconditions,
    }) {
        Ok(result) => result,
        Err(error) => {
            mark_failed(host, &request.push_id, &error)?;
            return Err(error);
        }
    };
    if let Err(error) = host.record_apply_effects(&request.push_id, apply_result.effects.clone()) {
        mark_failed(host, &request.push_id, &error)?;
        return Err(error);
    }
    host.update_status(&request.push_id, JournalStatus::Applied)?;

    let reconcile_result = match host.reconcile(PushReconcileRequest {
        push_id: &request.push_id,
        mount_id: &request.mount_id,
        plan: &plan,
        changed_remote_ids: &apply_result.changed_remote_ids,
        apply_effects: &apply_result.effects,
    }) {
        Ok(result) => result,
        Err(error) => {
            mark_failed(host, &request.push_id, &error)?;
            return Err(error);
        }
    };
    host.update_status(&request.push_id, JournalStatus::Reconciled)?;

    let mut completed_stages = request.pipeline.completed_stages;
    completed_stages.push(PushStage::ConcurrencyCheckAndApply);
    completed_stages.push(PushStage::JournalAndReconcile);

    Ok(PushExecutionResult {
        push_id: request.push_id,
        action: PushExecutionAction::Reconciled,
        changed_remote_ids: apply_result.changed_remote_ids,
        apply_effects: apply_result.effects,
        reconciled_remote_ids: reconcile_result.reconciled_remote_ids,
        journal_status: Some(JournalStatus::Reconciled),
        completed_stages,
    })
}

fn mark_failed<J>(journal: &mut J, push_id: &PushId, error: &AfsError) -> AfsResult<()>
where
    J: JournalStore,
{
    journal.update_status(push_id, JournalStatus::Failed(error.to_string()))
}
