//! `loc diff` orchestration.
//!
//! This module is intentionally thin: it resolves a local path through
//! `locality-store`, reads the canonical file from disk, and delegates validation,
//! diffing, and guardrail evaluation to `locality-core`.

use std::path::{Path, PathBuf};

use locality_core::LocalityError;
use locality_core::model::RemoteId;
use locality_core::planner::{
    GuardrailDecision, PlanDegradation, PlanDegradationKind, PlanSummary, PropertyValue,
    PushOperation, PushPlan,
};
use locality_core::push::{PushApproval, PushPipelineAction, PushPipelineResult, PushStage};
use locality_core::readable_diff::ReadableDiffOutput;
use locality_core::shadow::ShadowDocument;
use locality_core::validation::ValidationIssue;
use locality_store::{
    EntityRecord, EntityRepository, MountConfig, MountRepository, ShadowRepository, StoreError,
    VirtualMutationRepository,
};
use localityd::execution::PushJob;
use localityd::push::{PushPrepareError, prepare_push};
use localityd::source::LocalSourceValidator;
use serde::Serialize;

pub fn run_diff<S>(store: &S, target_path: impl AsRef<Path>) -> Result<DiffReport, DiffError>
where
    S: MountRepository + EntityRepository + ShadowRepository + VirtualMutationRepository,
{
    run_preview(store, target_path, PreviewOptions::new("diff"))
}

pub fn run_diff_with_state_root<S>(
    store: &S,
    target_path: impl AsRef<Path>,
    state_root: Option<&Path>,
) -> Result<DiffReport, DiffError>
where
    S: MountRepository + EntityRepository + ShadowRepository + VirtualMutationRepository,
{
    run_preview_with_state_root(store, target_path, PreviewOptions::new("diff"), state_root)
}

pub fn run_preview<S>(
    store: &S,
    target_path: impl AsRef<Path>,
    options: PreviewOptions,
) -> Result<DiffReport, DiffError>
where
    S: MountRepository + EntityRepository + ShadowRepository + VirtualMutationRepository,
{
    run_preview_with_state_root(store, target_path, options, None)
}

pub fn run_preview_with_state_root<S>(
    store: &S,
    target_path: impl AsRef<Path>,
    options: PreviewOptions,
    state_root: Option<&Path>,
) -> Result<DiffReport, DiffError>
where
    S: MountRepository + EntityRepository + ShadowRepository + VirtualMutationRepository,
{
    run_preview_artifacts_with_state_root(store, target_path, options, state_root)
        .map(|artifacts| artifacts.report)
}

pub fn run_preview_artifacts<S>(
    store: &S,
    target_path: impl AsRef<Path>,
    options: PreviewOptions,
) -> Result<PreviewArtifacts, DiffError>
where
    S: MountRepository + EntityRepository + ShadowRepository + VirtualMutationRepository,
{
    run_preview_artifacts_with_state_root(store, target_path, options, None)
}

pub fn run_preview_artifacts_with_state_root<S>(
    store: &S,
    target_path: impl AsRef<Path>,
    options: PreviewOptions,
    state_root: Option<&Path>,
) -> Result<PreviewArtifacts, DiffError>
where
    S: MountRepository + EntityRepository + ShadowRepository + VirtualMutationRepository,
{
    let job = PushJob {
        target_path: target_path.as_ref().to_path_buf(),
        assume_yes: options.approval.assume_yes,
        confirm_dangerous: options.approval.confirm_dangerous,
    };
    let validator = LocalSourceValidator;
    let prepared = prepare_push(store, &job, state_root, &validator).map_err(DiffError::from)?;
    let entity_id = prepared.entity.remote_id.clone();
    let pipeline = prepared.pipeline.clone();
    let mut report = DiffReport::from_pipeline(
        options.command,
        prepared.absolute_path.clone(),
        &prepared.mount,
        entity_id.clone(),
        pipeline.clone(),
    );
    report.readable_diff = prepared.readable_diff.clone();
    let shadow = prepared.shadows.first().cloned();

    Ok(PreviewArtifacts {
        report,
        mount: Some(prepared.mount),
        entity_id: Some(entity_id),
        entity: Some(prepared.entity),
        shadow,
        pipeline: Some(pipeline),
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreviewOptions {
    pub command: &'static str,
    pub approval: PushApproval,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewArtifacts {
    pub report: DiffReport,
    pub mount: Option<MountConfig>,
    pub entity_id: Option<RemoteId>,
    pub entity: Option<EntityRecord>,
    pub shadow: Option<ShadowDocument>,
    pub pipeline: Option<PushPipelineResult>,
}

impl PreviewOptions {
    pub fn new(command: &'static str) -> Self {
        Self {
            command,
            approval: PushApproval::default(),
        }
    }

    pub fn with_approval(mut self, approval: PushApproval) -> Self {
        self.approval = approval;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffError {
    MountNotFound(PathBuf),
    ReadFile { path: PathBuf, message: String },
    Store(StoreError),
    Prepare(LocalityError),
}

impl DiffError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::MountNotFound(_) => "mount_not_found",
            Self::ReadFile { .. } => "read_file_failed",
            Self::Store(StoreError::NotImplemented(_)) => "not_implemented",
            Self::Store(StoreError::ShadowMissing { .. }) => "shadow_missing",
            Self::Store(StoreError::EntityPathMissing { .. }) => "entity_path_missing",
            Self::Store(_) => "store_error",
            Self::Prepare(LocalityError::NotImplemented(_)) => "not_implemented",
            Self::Prepare(LocalityError::Validation(_)) => "validation_failed",
            Self::Prepare(LocalityError::Conflict(_)) => "conflict",
            Self::Prepare(LocalityError::Guardrail(_)) => "guardrail",
            Self::Prepare(LocalityError::RemoteNotFound(_)) => "remote_not_found",
            Self::Prepare(LocalityError::RateLimited { .. }) => "rate_limited",
            Self::Prepare(LocalityError::InvalidState(_)) => "invalid_state",
            Self::Prepare(LocalityError::UpdateRequired { .. }) => "update_required",
            Self::Prepare(LocalityError::Unsupported(_)) => "unsupported",
            Self::Prepare(LocalityError::Io(_)) => "io_error",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::MountNotFound(path) => {
                format!("no Locality mount contains `{}`", path.display())
            }
            Self::ReadFile { path, message } => {
                format!("failed to read `{}`: {message}", path.display())
            }
            Self::Store(error) => error.to_string(),
            Self::Prepare(error) => error.to_string(),
        }
    }
}

#[cfg(test)]
mod update_required_error_code_tests {
    use locality_core::LocalityError;

    use super::DiffError;

    #[test]
    fn update_required_has_stable_diff_error_code() {
        let error = DiffError::Prepare(LocalityError::UpdateRequired {
            component: "linear:discovery".to_string(),
            found: 2,
            supported: 1,
        });

        assert_eq!(error.code(), "update_required");
    }
}

impl From<PushPrepareError> for DiffError {
    fn from(value: PushPrepareError) -> Self {
        match value {
            PushPrepareError::MountNotFound(path) => Self::MountNotFound(path),
            PushPrepareError::ReadFile { path, message } => Self::ReadFile { path, message },
            PushPrepareError::Store(error) => Self::Store(error),
            PushPrepareError::Core(error) => Self::Prepare(error),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DiffReport {
    pub ok: bool,
    pub command: &'static str,
    pub path: String,
    pub mount_id: String,
    pub entity_id: String,
    pub validation: Vec<ValidationIssueOutput>,
    pub plan: Option<PushPlanOutput>,
    pub guardrail: GuardrailOutput,
    pub action: String,
    pub unsupported: Vec<String>,
    pub message: Option<String>,
    pub suggested_fix: Option<String>,
    #[serde(skip_serializing)]
    pub readable_diff: Option<ReadableDiffOutput>,
    pub completed_stages: Vec<String>,
}

impl DiffReport {
    fn from_pipeline(
        command: &'static str,
        absolute_path: PathBuf,
        mount: &MountConfig,
        entity_id: RemoteId,
        result: PushPipelineResult,
    ) -> Self {
        let (unsupported, message, suggested_fix) = unsupported_action_fields(&result.action);
        let ok = result.validation.is_clean() && unsupported.is_empty();
        Self {
            ok,
            command,
            path: absolute_path.display().to_string(),
            mount_id: mount.mount_id.0.clone(),
            entity_id: entity_id.0,
            validation: result
                .validation
                .issues
                .into_iter()
                .map(ValidationIssueOutput::from)
                .collect(),
            plan: result.plan.map(PushPlanOutput::from),
            guardrail: GuardrailOutput::from(result.guardrail),
            action: action_name(&result.action).to_string(),
            unsupported,
            message,
            suggested_fix,
            readable_diff: None,
            completed_stages: result
                .completed_stages
                .iter()
                .map(stage_name)
                .map(str::to_string)
                .collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ValidationIssueOutput {
    pub code: String,
    pub file: String,
    pub line: Option<usize>,
    pub message: String,
    pub suggested_fix: Option<String>,
}

impl From<ValidationIssue> for ValidationIssueOutput {
    fn from(value: ValidationIssue) -> Self {
        Self {
            code: value.code,
            file: locality_platform::logical_path_display(&value.file),
            line: value.line,
            message: value.message,
            suggested_fix: value.suggested_fix,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PushPlanOutput {
    pub summary: PlanSummaryOutput,
    pub affected_entities: Vec<String>,
    pub operations: Vec<PushOperationOutput>,
    pub degradations: Vec<PlanDegradationOutput>,
}

impl From<PushPlan> for PushPlanOutput {
    fn from(value: PushPlan) -> Self {
        Self {
            summary: PlanSummaryOutput::from(value.summary),
            affected_entities: value
                .affected_entities
                .into_iter()
                .map(|remote_id| remote_id.0)
                .collect(),
            operations: value
                .operations
                .into_iter()
                .map(PushOperationOutput::from)
                .collect(),
            degradations: value
                .degradations
                .into_iter()
                .map(PlanDegradationOutput::from)
                .collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PlanSummaryOutput {
    pub blocks_created: usize,
    pub blocks_updated: usize,
    pub blocks_replaced: usize,
    pub blocks_moved: usize,
    pub media_updated: usize,
    pub blocks_archived: usize,
    pub entities_created: usize,
    pub entities_archived: usize,
    pub entity_bodies_updated: usize,
    pub entities_moved: usize,
    pub properties_updated: usize,
}

impl From<PlanSummary> for PlanSummaryOutput {
    fn from(value: PlanSummary) -> Self {
        Self {
            blocks_created: value.blocks_created,
            blocks_updated: value.blocks_updated,
            blocks_replaced: value.blocks_replaced,
            blocks_moved: value.blocks_moved,
            media_updated: value.media_updated,
            blocks_archived: value.blocks_archived,
            entities_created: value.entities_created,
            entities_archived: value.entities_archived,
            entity_bodies_updated: value.entity_bodies_updated,
            entities_moved: value.entities_moved,
            properties_updated: value.properties_updated,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PushOperationOutput {
    UpdateBlock {
        block_id: String,
        content: String,
    },
    ReplaceBlock {
        block_id: String,
        content: String,
    },
    AppendBlock {
        parent_id: String,
        after: Option<String>,
        content: String,
    },
    MoveBlock {
        block_id: String,
        after: Option<String>,
    },
    UpdateMedia {
        block_id: String,
        local_path: String,
        caption: String,
    },
    ArchiveBlock {
        block_id: String,
    },
    ArchiveEntity {
        entity_id: String,
    },
    UpdateEntityBody {
        entity_id: String,
        body: String,
    },
    UpdateProperties {
        entity_id: String,
        keys: Vec<String>,
        properties: Vec<PropertyUpdateOutput>,
    },
    MoveEntity {
        entity_id: String,
        new_parent_id: String,
        new_parent_kind: locality_core::model::EntityKind,
        new_title: String,
        projected_path: String,
    },
    CreateEntity {
        parent_id: String,
        #[serde(skip_serializing_if = "is_false")]
        parent_workspace: bool,
        title: String,
        keys: Vec<String>,
        properties: Vec<PropertyUpdateOutput>,
        body: String,
        source_path: String,
    },
    CreateDatabase {
        parent_id: String,
        title: String,
        schema: String,
        source_path: String,
    },
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl From<PushOperation> for PushOperationOutput {
    fn from(value: PushOperation) -> Self {
        match value {
            PushOperation::UpdateBlock { block_id, content } => Self::UpdateBlock {
                block_id: block_id.0,
                content,
            },
            PushOperation::ReplaceBlock { block_id, content } => Self::ReplaceBlock {
                block_id: block_id.0,
                content,
            },
            PushOperation::AppendBlock {
                parent_id,
                after,
                content,
            } => Self::AppendBlock {
                parent_id: parent_id.0,
                after: after.map(|remote_id| remote_id.0),
                content,
            },
            PushOperation::MoveBlock { block_id, after } => Self::MoveBlock {
                block_id: block_id.0,
                after: after.map(|remote_id| remote_id.0),
            },
            PushOperation::UpdateMedia {
                block_id,
                local_path,
                caption,
            } => Self::UpdateMedia {
                block_id: block_id.0,
                local_path: locality_platform::logical_path_display(&local_path),
                caption,
            },
            PushOperation::ArchiveBlock { block_id } => Self::ArchiveBlock {
                block_id: block_id.0,
            },
            PushOperation::ArchiveEntity { entity_id } => Self::ArchiveEntity {
                entity_id: entity_id.0,
            },
            PushOperation::UpdateEntityBody { entity_id, body } => Self::UpdateEntityBody {
                entity_id: entity_id.0,
                body,
            },
            PushOperation::UpdateProperties {
                entity_id,
                properties,
            } => Self::UpdateProperties {
                entity_id: entity_id.0,
                keys: properties.keys().cloned().collect(),
                properties: properties
                    .into_iter()
                    .map(|(key, value)| PropertyUpdateOutput {
                        key,
                        value: PropertyValueOutput::from(value),
                    })
                    .collect(),
            },
            PushOperation::MoveEntity {
                entity_id,
                new_parent_id,
                new_parent_kind,
                new_title,
                projected_path,
            } => Self::MoveEntity {
                entity_id: entity_id.0,
                new_parent_id: new_parent_id.0,
                new_parent_kind,
                new_title,
                projected_path: locality_platform::logical_path_display(&projected_path),
            },
            PushOperation::CreateEntity {
                parent_id,
                parent_workspace,
                title,
                properties,
                body,
                source_path,
                ..
            } => Self::CreateEntity {
                parent_id: parent_id.0,
                parent_workspace,
                title,
                keys: properties.keys().cloned().collect(),
                properties: properties
                    .into_iter()
                    .map(|(key, value)| PropertyUpdateOutput {
                        key,
                        value: PropertyValueOutput::from(value),
                    })
                    .collect(),
                body,
                source_path: locality_platform::logical_path_display(&source_path),
            },
            PushOperation::CreateDatabase {
                parent_id,
                title,
                schema,
                source_path,
            } => Self::CreateDatabase {
                parent_id: parent_id.0,
                title,
                schema,
                source_path: locality_platform::logical_path_display(&source_path),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PropertyUpdateOutput {
    pub key: String,
    pub value: PropertyValueOutput,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum PropertyValueOutput {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    List(Vec<String>),
    Array(Vec<PropertyValueOutput>),
    Object(Vec<PropertyUpdateOutput>),
}

impl From<PropertyValue> for PropertyValueOutput {
    fn from(value: PropertyValue) -> Self {
        match value {
            PropertyValue::Null => Self::Null,
            PropertyValue::Bool(value) => Self::Bool(value),
            PropertyValue::Number(value) => Self::Number(value),
            PropertyValue::String(value) => Self::String(value),
            PropertyValue::List(value) => Self::List(value),
            PropertyValue::Array(value) => {
                Self::Array(value.into_iter().map(PropertyValueOutput::from).collect())
            }
            PropertyValue::Object(value) => Self::Object(
                value
                    .into_iter()
                    .map(|(key, value)| PropertyUpdateOutput {
                        key,
                        value: PropertyValueOutput::from(value),
                    })
                    .collect(),
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PlanDegradationOutput {
    pub kind: String,
    pub message: String,
}

impl From<PlanDegradation> for PlanDegradationOutput {
    fn from(value: PlanDegradation) -> Self {
        Self {
            kind: degradation_kind_name(&value.kind).to_string(),
            message: value.message,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GuardrailOutput {
    pub decision: String,
    pub reasons: Vec<String>,
}

impl GuardrailOutput {
    fn proceed() -> Self {
        Self {
            decision: "proceed".to_string(),
            reasons: Vec::new(),
        }
    }
}

impl From<GuardrailDecision> for GuardrailOutput {
    fn from(value: GuardrailDecision) -> Self {
        match value {
            GuardrailDecision::Proceed => Self::proceed(),
            GuardrailDecision::ConfirmRequired { reasons } => Self {
                decision: "confirm_required".to_string(),
                reasons,
            },
        }
    }
}

pub fn action_name(action: &PushPipelineAction) -> &'static str {
    match action {
        PushPipelineAction::Noop => "noop",
        PushPipelineAction::FixValidation => "fix_validation",
        PushPipelineAction::ConfirmPlan => "confirm_plan",
        PushPipelineAction::ConfirmDangerousPlan => "confirm_dangerous_plan",
        PushPipelineAction::ProceedToApply => "proceed_to_apply",
        PushPipelineAction::ReadOnlyBlocked => "read_only_blocked",
        PushPipelineAction::UnsupportedOperations { .. } => "unsupported_operations",
    }
}

pub fn unsupported_action_fields(
    action: &PushPipelineAction,
) -> (Vec<String>, Option<String>, Option<String>) {
    match action {
        PushPipelineAction::UnsupportedOperations {
            operations,
            message,
            suggested_fix,
        } => (
            operations.clone(),
            Some(message.clone()),
            Some(suggested_fix.clone()),
        ),
        _ => (Vec::new(), None, None),
    }
}

fn stage_name(stage: &PushStage) -> &'static str {
    match stage {
        PushStage::ParseAndValidate => "parse_and_validate",
        PushStage::Diff => "diff",
        PushStage::PlanAndConfirm => "plan_and_confirm",
        PushStage::ConcurrencyCheckAndApply => "concurrency_check_and_apply",
        PushStage::JournalAndReconcile => "journal_and_reconcile",
    }
}

fn degradation_kind_name(kind: &PlanDegradationKind) -> &'static str {
    match kind {
        PlanDegradationKind::AmbiguousBlockAlignment => "ambiguous_block_alignment",
    }
}
