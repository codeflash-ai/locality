//! `afs push` orchestration.
//!
//! This push surface runs validation, diff, plan, guardrail, and the journaled
//! connector-apply spine. Real source mutation is still connector-dependent, but
//! the CLI path now exercises the same write-ahead executor that production
//! connectors will use.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use afs_core::journal::{JournalPreimage, JournalStatus, JournalStore, PushId};
use afs_core::model::RemoteId;
use afs_core::push::{
    PushApproval, PushExecutionAction, PushExecutionRequest, PushExecutionResult,
    PushReconcileRequest, PushReconcileResult, PushReconciler, execute_journaled_push,
};
use afs_core::{AfsError, AfsResult};
use afs_store::{EntityRepository, JournalRepository, MountRepository, ShadowRepository};
use serde::Serialize;

use crate::diff::{
    DiffError, GuardrailOutput, PreviewOptions, PushPlanOutput, ValidationIssueOutput, run_preview,
    run_preview_artifacts,
};

pub fn run_push<S>(
    store: &S,
    target_path: impl AsRef<Path>,
    options: PushOptions,
) -> Result<PushReport, DiffError>
where
    S: MountRepository + EntityRepository + ShadowRepository,
{
    let preview = run_preview(
        store,
        target_path,
        PreviewOptions::new("push").with_approval(PushApproval {
            assume_yes: options.assume_yes,
            confirm_dangerous: options.confirm_dangerous,
        }),
    )?;

    Ok(PushReport::from_preview(preview))
}

pub fn run_push_with_executor<S, C, A, R>(
    store: &mut S,
    target_path: impl AsRef<Path>,
    options: PushOptions,
    concurrency: &mut C,
    applier: &mut A,
    reconciler: &mut R,
) -> Result<PushReport, DiffError>
where
    S: MountRepository + EntityRepository + ShadowRepository + JournalRepository + JournalStore,
    C: afs_core::push::PushConcurrencyCheck,
    A: afs_core::push::PushApplier,
    R: PushReconciler,
{
    let artifacts = run_preview_artifacts(
        store,
        target_path,
        PreviewOptions::new("push").with_approval(PushApproval {
            assume_yes: options.assume_yes,
            confirm_dangerous: options.confirm_dangerous,
        }),
    )?;
    let report = PushReport::from_preview(artifacts.report);

    if report.pipeline_action != "proceed_to_apply" {
        return Ok(report);
    }

    let (Some(mount), Some(shadow), Some(pipeline)) =
        (artifacts.mount, artifacts.shadow, artifacts.pipeline)
    else {
        return Ok(report);
    };
    let push_id = generate_push_id();
    let execution_request = PushExecutionRequest::new(push_id.clone(), mount.mount_id, pipeline)
        .with_preimages(vec![JournalPreimage::from_shadow(shadow)]);

    match execute_journaled_push(store, concurrency, applier, reconciler, execution_request) {
        Ok(result) => Ok(PushReport::from_execution(report, result)),
        Err(error) => Ok(PushReport::from_execution_error(
            report,
            push_id.clone(),
            journal_status_after_error(store, &push_id),
            error,
        )),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PushOptions {
    pub assume_yes: bool,
    pub confirm_dangerous: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PushReport {
    pub ok: bool,
    pub command: &'static str,
    pub path: String,
    pub mount_id: String,
    pub entity_id: String,
    pub validation: Vec<ValidationIssueOutput>,
    pub plan: Option<PushPlanOutput>,
    pub guardrail: GuardrailOutput,
    pub action: String,
    pub pipeline_action: String,
    pub push_id: Option<String>,
    pub journal_status: Option<String>,
    pub changed_remote_ids: Vec<String>,
    pub reconciled_remote_ids: Vec<String>,
    pub apply_effect_count: usize,
    pub completed_stages: Vec<String>,
    pub message: Option<String>,
}

impl PushReport {
    fn from_preview(preview: crate::diff::DiffReport) -> Self {
        let (action, message) = match preview.action.as_str() {
            "proceed_to_apply" => (
                "apply_not_implemented".to_string(),
                Some("connector apply and journaled mutation are not implemented yet".to_string()),
            ),
            action => (action.to_string(), None),
        };
        let ok = action == "noop";

        Self {
            ok,
            command: "push",
            path: preview.path,
            mount_id: preview.mount_id,
            entity_id: preview.entity_id,
            validation: preview.validation,
            plan: preview.plan,
            guardrail: preview.guardrail,
            pipeline_action: preview.action,
            action,
            push_id: None,
            journal_status: None,
            changed_remote_ids: Vec::new(),
            reconciled_remote_ids: Vec::new(),
            apply_effect_count: 0,
            completed_stages: preview.completed_stages,
            message,
        }
    }

    fn from_execution(mut report: Self, result: PushExecutionResult) -> Self {
        match result.action {
            PushExecutionAction::Reconciled => {
                report.ok = true;
                report.action = "reconciled".to_string();
                report.message = Some("connector apply and reconcile completed".to_string());
            }
            PushExecutionAction::NotReady { pipeline_action } => {
                report.ok = false;
                report.action = "not_ready".to_string();
                report.message = Some(format!(
                    "push executor stopped before apply: {pipeline_action:?}"
                ));
            }
        }
        report.push_id = Some(result.push_id.0);
        report.journal_status = result.journal_status.as_ref().map(journal_status_name);
        report.changed_remote_ids = remote_ids_to_strings(result.changed_remote_ids);
        report.reconciled_remote_ids = remote_ids_to_strings(result.reconciled_remote_ids);
        report.apply_effect_count = result.apply_effects.len();
        report.completed_stages = result
            .completed_stages
            .iter()
            .map(push_stage_name)
            .map(str::to_string)
            .collect();
        report
    }

    fn from_execution_error(
        mut report: Self,
        push_id: PushId,
        journal_status: Option<JournalStatus>,
        error: AfsError,
    ) -> Self {
        report.ok = false;
        report.push_id = Some(push_id.0);
        report.journal_status = journal_status.as_ref().map(journal_status_name);
        report.action = match &error {
            AfsError::NotImplemented(_) => "apply_not_implemented".to_string(),
            _ => "apply_failed".to_string(),
        };
        report.message = Some(error.to_string());
        report
    }
}

pub fn push_report_exit_code(report: &PushReport) -> i32 {
    match report.action.as_str() {
        "noop" | "reconciled" => 0,
        "fix_validation" => 3,
        "confirm_plan" | "confirm_dangerous_plan" | "read_only_blocked" => 4,
        "apply_not_implemented" => 5,
        _ => 1,
    }
}

#[derive(Debug, Default)]
pub struct NotImplementedReconciler;

impl PushReconciler for NotImplementedReconciler {
    fn reconcile(&mut self, _request: PushReconcileRequest<'_>) -> AfsResult<PushReconcileResult> {
        Err(AfsError::NotImplemented("post-apply reconcile"))
    }
}

fn generate_push_id() -> PushId {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    PushId(format!("push-{timestamp}-{}", std::process::id()))
}

fn journal_status_after_error<S>(store: &S, push_id: &PushId) -> Option<JournalStatus>
where
    S: JournalRepository,
{
    store
        .get_journal(push_id)
        .ok()
        .flatten()
        .map(|entry| entry.status)
}

fn remote_ids_to_strings(remote_ids: Vec<RemoteId>) -> Vec<String> {
    remote_ids
        .into_iter()
        .map(|remote_id| remote_id.0)
        .collect()
}

fn journal_status_name(status: &JournalStatus) -> String {
    match status {
        JournalStatus::Prepared => "prepared".to_string(),
        JournalStatus::Applying => "applying".to_string(),
        JournalStatus::Applied => "applied".to_string(),
        JournalStatus::Reconciled => "reconciled".to_string(),
        JournalStatus::Reverted => "reverted".to_string(),
        JournalStatus::Failed(_) => "failed".to_string(),
    }
}

fn push_stage_name(stage: &afs_core::push::PushStage) -> &'static str {
    match stage {
        afs_core::push::PushStage::ParseAndValidate => "parse_and_validate",
        afs_core::push::PushStage::Diff => "diff",
        afs_core::push::PushStage::PlanAndConfirm => "plan_and_confirm",
        afs_core::push::PushStage::ConcurrencyCheckAndApply => "concurrency_check_and_apply",
        afs_core::push::PushStage::JournalAndReconcile => "journal_and_reconcile",
    }
}
