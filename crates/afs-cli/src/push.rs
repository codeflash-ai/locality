//! `afs push` orchestration.
//!
//! This push surface renders daemon execution reports into CLI output. The
//! apply/reconcile path itself lives in `afsd`.

use std::path::Path;

use afs_connector::Connector;
use afs_core::AfsResult;
use afs_core::journal::{JournalStatus, JournalStore};
use afs_core::model::RemoteId;
use afs_core::push::{PushApproval, PushExecutionAction, PushExecutionResult};
use afs_store::{
    EntityRepository, JournalRepository, MountRepository, ShadowRepository,
    VirtualMutationRepository,
};
use afsd::execution::{PushJob, PushJobError, PushJobReport};
use afsd::hydration::HydrationSource;
use afsd::push::{PushJobAction, execute_push_job};
use serde::Serialize;

use crate::diff::{
    DiffError, GuardrailOutput, PreviewOptions, PushPlanOutput, ValidationIssueOutput, action_name,
    run_preview, unsupported_action_fields,
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

pub fn run_push_with_daemon<S, Source>(
    store: &mut S,
    source: &Source,
    target_path: impl AsRef<Path>,
    options: PushOptions,
) -> AfsResult<PushReport>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + JournalRepository
        + JournalStore
        + VirtualMutationRepository,
    Source: Connector + HydrationSource + ?Sized,
{
    let job = PushJob {
        target_path: target_path.as_ref().to_path_buf(),
        assume_yes: options.assume_yes,
        confirm_dangerous: options.confirm_dangerous,
    };
    execute_push_job(store, job, source).map(PushReport::from_daemon)
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
    pub via: String,
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
    pub unsupported: Vec<String>,
    pub suggested_fix: Option<String>,
}

impl PushReport {
    pub fn from_daemon(report: PushJobReport) -> Self {
        let PushJobReport {
            target_path,
            mount_id,
            entity_id,
            pipeline,
            action,
            execution,
            push_id,
            journal_status,
            error,
        } = report;

        let pipeline_action = action_name(&pipeline.action).to_string();
        let completed_stages = pipeline
            .completed_stages
            .iter()
            .map(push_stage_name)
            .map(str::to_string)
            .collect();
        let (unsupported, message, suggested_fix) = unsupported_action_fields(&pipeline.action);
        let mut cli_report = Self {
            ok: false,
            command: "push",
            via: "daemon".to_string(),
            path: target_path.display().to_string(),
            mount_id: mount_id.0,
            entity_id: entity_id.0,
            validation: pipeline
                .validation
                .issues
                .into_iter()
                .map(ValidationIssueOutput::from)
                .collect(),
            plan: pipeline.plan.map(PushPlanOutput::from),
            guardrail: GuardrailOutput::from(pipeline.guardrail),
            action: daemon_action_name(&action, &pipeline_action, error.as_ref()).to_string(),
            pipeline_action,
            push_id: None,
            journal_status: journal_status.as_ref().map(journal_status_name),
            changed_remote_ids: Vec::new(),
            reconciled_remote_ids: Vec::new(),
            apply_effect_count: 0,
            completed_stages,
            message,
            unsupported,
            suggested_fix,
        };

        if let Some(result) = execution {
            cli_report = cli_report.with_execution(result);
        } else if let Some(error) = error {
            cli_report.ok = false;
            cli_report.push_id = push_id.map(|push_id| push_id.0);
            if cli_report.message.is_none() {
                cli_report.message = Some(error.message);
            }
        } else {
            cli_report.ok = cli_report.action == "noop";
        }

        cli_report
    }

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
            via: "cli".to_string(),
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
            unsupported: preview.unsupported,
            suggested_fix: preview.suggested_fix,
        }
    }

    fn with_execution(mut self, result: PushExecutionResult) -> Self {
        match &result.action {
            PushExecutionAction::Reconciled => {
                self.ok = true;
                self.action = "reconciled".to_string();
                self.message = Some("connector apply and reconcile completed".to_string());
            }
            PushExecutionAction::NotReady { pipeline_action } => {
                let action = action_name(pipeline_action);
                self.ok = action == "noop";
                self.action = action.to_string();
                self.message = None;
            }
        }
        if result.journal_status.is_some()
            || matches!(&result.action, PushExecutionAction::Reconciled)
        {
            self.push_id = Some(result.push_id.0);
        }
        self.journal_status = result.journal_status.as_ref().map(journal_status_name);
        self.changed_remote_ids = remote_ids_to_strings(result.changed_remote_ids);
        self.reconciled_remote_ids = remote_ids_to_strings(result.reconciled_remote_ids);
        self.apply_effect_count = result.apply_effects.len();
        self.completed_stages = result
            .completed_stages
            .iter()
            .map(push_stage_name)
            .map(str::to_string)
            .collect();
        self
    }
}

pub fn push_report_exit_code(report: &PushReport) -> i32 {
    match report.action.as_str() {
        "noop" | "reconciled" => 0,
        "fix_validation" => 3,
        "confirm_plan" | "confirm_dangerous_plan" | "read_only_blocked" => 4,
        "apply_not_implemented" | "unsupported_operations" => 5,
        _ => 1,
    }
}

fn daemon_action_name<'a>(
    action: &PushJobAction,
    pipeline_action: &'a str,
    error: Option<&PushJobError>,
) -> &'a str {
    match action {
        PushJobAction::NotReady => pipeline_action,
        PushJobAction::Reconciled => "reconciled",
        PushJobAction::Failed => match error {
            Some(error) if error.code == "not_implemented" => "apply_not_implemented",
            _ => "apply_failed",
        },
    }
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
