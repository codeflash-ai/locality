//! `afs diff` orchestration.
//!
//! This module is intentionally thin: it resolves a local path through
//! `afs-store`, reads the canonical file from disk, and delegates validation,
//! diffing, and guardrail evaluation to `afs-core`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use afs_core::canonical::{
    CanonicalParseError, CanonicalParseErrorKind, ParsedCanonicalDocument, parse_canonical_markdown,
};
use afs_core::diff::property_value_from_frontmatter;
use afs_core::model::{EntityKind, RemoteId};
use afs_core::planner::{
    GuardrailDecision, GuardrailPolicy, PlanDegradation, PlanDegradationKind, PlanSummary,
    PropertyValue, PushOperation, PushPlan,
};
use afs_core::push::{
    PushApproval, PushPipelineAction, PushPipelineRequest, PushPipelineResult, PushStage,
    evaluate_guardrails, plan_push_pipeline,
};
use afs_core::shadow::ShadowDocument;
use afs_core::validation::{ValidationIssue, ValidationReport};
use afs_store::{
    EntityRecord, EntityRepository, MountConfig, MountRepository, ShadowRepository, StoreError,
};
use serde::Serialize;

pub fn run_diff<S>(store: &S, target_path: impl AsRef<Path>) -> Result<DiffReport, DiffError>
where
    S: MountRepository + EntityRepository + ShadowRepository,
{
    run_preview(store, target_path, PreviewOptions::new("diff"))
}

pub fn run_preview<S>(
    store: &S,
    target_path: impl AsRef<Path>,
    options: PreviewOptions,
) -> Result<DiffReport, DiffError>
where
    S: MountRepository + EntityRepository + ShadowRepository,
{
    run_preview_artifacts(store, target_path, options).map(|artifacts| artifacts.report)
}

pub fn run_preview_artifacts<S>(
    store: &S,
    target_path: impl AsRef<Path>,
    options: PreviewOptions,
) -> Result<PreviewArtifacts, DiffError>
where
    S: MountRepository + EntityRepository + ShadowRepository,
{
    let target_path = target_path.as_ref();
    let absolute_path = absolute_path(target_path)?;
    let mounts = store.load_mounts().map_err(DiffError::Store)?;
    let mount = find_mount_for_path(&mounts, &absolute_path)
        .ok_or_else(|| DiffError::MountNotFound(absolute_path.clone()))?;
    let relative_path = relative_entity_path(mount, &absolute_path)?;
    let entity = store
        .find_entity_by_path(&mount.mount_id, &relative_path)
        .map_err(DiffError::Store)?;
    let file = std::fs::read_to_string(&absolute_path).map_err(|error| DiffError::ReadFile {
        path: absolute_path.clone(),
        message: error.to_string(),
    })?;

    let Some(entity) = entity else {
        return create_entity_preview(store, absolute_path, mount, relative_path, file, options);
    };

    let parsed = match parse_canonical_markdown(&file) {
        Ok(parsed) => parsed,
        Err(error) => {
            let report = DiffReport::validation_failure(
                options.command,
                absolute_path,
                mount,
                entity.remote_id,
                vec![parse_error_issue(&relative_path, error)],
            );
            return Ok(PreviewArtifacts::report_only(report));
        }
    };

    if parsed
        .remote_id()
        .is_some_and(|remote_id| remote_id != &entity.remote_id)
    {
        let report = DiffReport::validation_failure(
            options.command,
            absolute_path,
            mount,
            entity.remote_id.clone(),
            vec![ValidationIssue::new(
                "frontmatter_remote_id_mismatch",
                relative_path,
                Some(1),
                "frontmatter `afs.id` does not match the entity mapped to this path",
                Some("restore the generated `afs.id` for this file before pushing".to_string()),
            )],
        );
        return Ok(PreviewArtifacts::report_only(report));
    }

    let shadow = store
        .load_shadow(&mount.mount_id, &entity.remote_id)
        .map_err(DiffError::Store)?;
    let schema_validation =
        notion_changed_row_schema_validation(store, mount, &relative_path, &parsed, &shadow)?;
    let output = if schema_validation.is_clean() {
        plan_push_pipeline(
            PushPipelineRequest::new(relative_path, &parsed, &shadow)
                .with_approval(options.approval)
                .read_only(mount.read_only),
        )
    } else {
        validation_pipeline(schema_validation)
    };

    let report = DiffReport::from_pipeline(
        options.command,
        absolute_path,
        mount,
        entity.remote_id.clone(),
        output.clone(),
    );

    Ok(PreviewArtifacts {
        report,
        mount: Some(mount.clone()),
        entity_id: Some(entity.remote_id.clone()),
        entity: Some(entity),
        shadow: Some(shadow),
        pipeline: Some(output),
    })
}

fn create_entity_preview<S>(
    store: &S,
    absolute_path: PathBuf,
    mount: &MountConfig,
    relative_path: PathBuf,
    file: String,
    options: PreviewOptions,
) -> Result<PreviewArtifacts, DiffError>
where
    S: EntityRepository,
{
    let parent = create_parent_entity(store, mount, &relative_path)?;
    if parent.kind != EntityKind::Database {
        let report = DiffReport::validation_failure(
            options.command,
            absolute_path,
            mount,
            parent.remote_id.clone(),
            vec![ValidationIssue::new(
                "create_entity_parent_not_database",
                relative_path,
                None,
                "new files can currently be pushed only as rows inside a projected database directory",
                Some(
                    "move the file into a database directory or pull the target page first"
                        .to_string(),
                ),
            )],
        );
        return Ok(PreviewArtifacts::report_only(report));
    }

    let parsed = match parse_canonical_markdown(&file) {
        Ok(parsed) => parsed,
        Err(error) => {
            let report = DiffReport::validation_failure(
                options.command,
                absolute_path,
                mount,
                parent.remote_id.clone(),
                vec![parse_error_issue(&relative_path, error)],
            );
            return Ok(PreviewArtifacts::report_only(report));
        }
    };

    let schema_validation =
        notion_create_row_schema_validation(mount, &parent, &relative_path, &parsed);
    let pipeline = create_entity_pipeline(
        &relative_path,
        &parsed,
        &parent,
        mount,
        options.approval,
        schema_validation,
    );
    let report = DiffReport::from_pipeline(
        options.command,
        absolute_path,
        mount,
        parent.remote_id.clone(),
        pipeline.clone(),
    );

    Ok(PreviewArtifacts {
        report,
        mount: Some(mount.clone()),
        entity_id: Some(parent.remote_id.clone()),
        entity: None,
        shadow: None,
        pipeline: Some(pipeline),
    })
}

fn create_parent_entity<S>(
    store: &S,
    mount: &MountConfig,
    relative_path: &Path,
) -> Result<EntityRecord, DiffError>
where
    S: EntityRepository,
{
    let Some(parent_path) = relative_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    else {
        return Err(DiffError::Store(StoreError::EntityPathMissing {
            mount_id: mount.mount_id.clone(),
            path: relative_path.to_path_buf(),
        }));
    };

    store
        .find_entity_by_path(&mount.mount_id, parent_path)
        .map_err(DiffError::Store)?
        .ok_or_else(|| {
            DiffError::Store(StoreError::EntityPathMissing {
                mount_id: mount.mount_id.clone(),
                path: relative_path.to_path_buf(),
            })
        })
}

fn create_entity_pipeline(
    relative_path: &Path,
    parsed: &ParsedCanonicalDocument,
    parent: &EntityRecord,
    mount: &MountConfig,
    approval: PushApproval,
    schema_validation: ValidationReport,
) -> PushPipelineResult {
    if mount.read_only {
        return PushPipelineResult {
            validation: ValidationReport::clean(),
            plan: None,
            guardrail: GuardrailDecision::Proceed,
            action: PushPipelineAction::ReadOnlyBlocked,
            completed_stages: Vec::new(),
        };
    }

    let mut validation = ValidationReport::clean();
    validation.extend(schema_validation);
    if parsed.remote_id().is_some() {
        validation.push(ValidationIssue::new(
            "create_entity_has_remote_id",
            relative_path,
            Some(1),
            "new row files must not carry an existing `afs.id`",
            Some(
                "remove the generated `afs.id`, or pull the existing row before editing"
                    .to_string(),
            ),
        ));
    }
    if parsed
        .frontmatter
        .afs
        .as_ref()
        .and_then(|afs| afs.entity_type.as_ref())
        .is_some_and(|kind| kind != &EntityKind::Page)
    {
        validation.push(ValidationIssue::new(
            "create_entity_type_not_page",
            relative_path,
            Some(1),
            "database row creation requires `afs.type: page` when an `afs` block is present",
            Some("remove the `afs` block or set `afs.type` to `page`".to_string()),
        ));
    }
    if parsed
        .frontmatter
        .title
        .as_ref()
        .is_none_or(|title| title.trim().is_empty())
    {
        validation.push(ValidationIssue::new(
            "create_entity_missing_title",
            relative_path,
            Some(1),
            "new row files require a non-empty `title` frontmatter value",
            Some("add `title: \"...\"` to the YAML frontmatter".to_string()),
        ));
    }
    if parsed.is_stub() {
        validation.push(ValidationIssue::new(
            "create_entity_stub_body",
            relative_path,
            None,
            "new row files cannot use the generated AFS stub marker as their body",
            Some("replace the stub marker with the row body, or leave the body empty".to_string()),
        ));
    }
    for directive in &parsed.directives {
        validation.push(ValidationIssue::new(
            "create_entity_directive_unsupported",
            relative_path,
            Some(directive.line),
            "new row creation does not support pre-seeded AFS directive blocks",
            Some(
                "remove the directive and create only directly supported Markdown blocks"
                    .to_string(),
            ),
        ));
    }

    let mut completed_stages = vec![PushStage::ParseAndValidate];
    if !validation.is_clean() {
        return PushPipelineResult {
            validation,
            plan: None,
            guardrail: GuardrailDecision::Proceed,
            action: PushPipelineAction::FixValidation,
            completed_stages,
        };
    }

    let properties = parsed
        .frontmatter
        .properties
        .iter()
        .map(|(key, value)| (key.clone(), property_value_from_frontmatter(value)))
        .collect::<BTreeMap<_, _>>();
    let plan = PushPlan::new(
        vec![parent.remote_id.clone()],
        vec![PushOperation::CreateEntity {
            parent_id: parent.remote_id.clone(),
            title: parsed.frontmatter.title.clone().unwrap_or_default(),
            properties,
            body: parsed.document.body.clone(),
            source_path: relative_path.to_path_buf(),
        }],
    );
    completed_stages.push(PushStage::Diff);

    let guardrail = evaluate_guardrails(&plan, &GuardrailPolicy::default(), None);
    completed_stages.push(PushStage::PlanAndConfirm);

    let action = match &guardrail {
        GuardrailDecision::Proceed if approval.assume_yes => PushPipelineAction::ProceedToApply,
        GuardrailDecision::Proceed => PushPipelineAction::ConfirmPlan,
        GuardrailDecision::ConfirmRequired { .. } if approval.confirm_dangerous => {
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

fn notion_changed_row_schema_validation<S>(
    store: &S,
    mount: &MountConfig,
    relative_path: &Path,
    parsed: &ParsedCanonicalDocument,
    shadow: &ShadowDocument,
) -> Result<ValidationReport, DiffError>
where
    S: EntityRepository,
{
    if mount.read_only {
        return Ok(ValidationReport::clean());
    }
    let Some(parent) = notion_database_parent(store, mount, relative_path)? else {
        return Ok(ValidationReport::clean());
    };
    Ok(
        match notion_schema_yaml_or_issue(mount, &parent, relative_path) {
            Ok(schema) => afs_notion::schema::validate_changed_row_frontmatter(
                &schema,
                shadow,
                parsed,
                relative_path,
            ),
            Err(report) => report,
        },
    )
}

fn notion_create_row_schema_validation(
    mount: &MountConfig,
    parent: &EntityRecord,
    relative_path: &Path,
    parsed: &ParsedCanonicalDocument,
) -> ValidationReport {
    if !is_notion_database(mount, parent) {
        return ValidationReport::clean();
    }

    match notion_schema_yaml_or_issue(mount, parent, relative_path) {
        Ok(schema) => {
            afs_notion::schema::validate_create_row_frontmatter(&schema, parsed, relative_path)
        }
        Err(report) => report,
    }
}

fn notion_database_parent<S>(
    store: &S,
    mount: &MountConfig,
    relative_path: &Path,
) -> Result<Option<EntityRecord>, DiffError>
where
    S: EntityRepository,
{
    if mount.connector != "notion" {
        return Ok(None);
    }
    let Some(parent_path) = relative_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    else {
        return Ok(None);
    };

    let parent = store
        .find_entity_by_path(&mount.mount_id, parent_path)
        .map_err(DiffError::Store)?;
    Ok(parent.filter(|entity| entity.kind == EntityKind::Database))
}

fn is_notion_database(mount: &MountConfig, entity: &EntityRecord) -> bool {
    mount.connector == "notion" && entity.kind == EntityKind::Database
}

fn notion_schema_yaml_or_issue(
    mount: &MountConfig,
    database: &EntityRecord,
    relative_path: &Path,
) -> Result<String, ValidationReport> {
    let schema_path = mount.root.join(&database.path).join("_schema.yaml");
    match std::fs::read_to_string(&schema_path) {
        Ok(schema) => Ok(schema),
        Err(error) => {
            let code = if error.kind() == std::io::ErrorKind::NotFound {
                "notion_schema_missing"
            } else {
                "notion_schema_unreadable"
            };
            let mut report = ValidationReport::clean();
            report.push(ValidationIssue::new(
                code,
                relative_path,
                Some(1),
                format!(
                    "Notion database row writes require readable schema file `{}`",
                    schema_path.display()
                ),
                Some(
                    "run `afs pull` on the database directory to regenerate `_schema.yaml`"
                        .to_string(),
                ),
            ));
            Err(report)
        }
    }
}

fn validation_pipeline(validation: ValidationReport) -> PushPipelineResult {
    PushPipelineResult {
        validation,
        plan: None,
        guardrail: GuardrailDecision::Proceed,
        action: PushPipelineAction::FixValidation,
        completed_stages: vec![PushStage::ParseAndValidate],
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, DiffError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| DiffError::ReadFile {
                path: path.to_path_buf(),
                message: error.to_string(),
            })
    }
}

fn find_mount_for_path<'a>(mounts: &'a [MountConfig], path: &Path) -> Option<&'a MountConfig> {
    mounts
        .iter()
        .filter(|mount| path.starts_with(&mount.root))
        .max_by_key(|mount| mount.root.components().count())
}

fn relative_entity_path(mount: &MountConfig, absolute_path: &Path) -> Result<PathBuf, DiffError> {
    absolute_path
        .strip_prefix(&mount.root)
        .map(Path::to_path_buf)
        .map_err(|_| DiffError::MountNotFound(absolute_path.to_path_buf()))
}

fn parse_error_issue(path: &Path, error: CanonicalParseError) -> ValidationIssue {
    let code = match error.kind {
        CanonicalParseErrorKind::MissingFrontmatter => "canonical_missing_frontmatter",
        CanonicalParseErrorKind::UnterminatedFrontmatter => "canonical_unterminated_frontmatter",
        CanonicalParseErrorKind::InvalidFrontmatterYaml => "canonical_invalid_frontmatter_yaml",
    };

    ValidationIssue::new(
        code,
        path.to_path_buf(),
        error.line,
        error.message,
        Some("restore the generated canonical Markdown frontmatter".to_string()),
    )
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

impl PreviewArtifacts {
    fn report_only(report: DiffReport) -> Self {
        Self {
            report,
            mount: None,
            entity_id: None,
            entity: None,
            shadow: None,
            pipeline: None,
        }
    }
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
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::MountNotFound(path) => {
                format!("no AgentFS mount contains `{}`", path.display())
            }
            Self::ReadFile { path, message } => {
                format!("failed to read `{}`: {message}", path.display())
            }
            Self::Store(error) => error.to_string(),
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
    pub completed_stages: Vec<String>,
}

impl DiffReport {
    fn validation_failure(
        command: &'static str,
        absolute_path: PathBuf,
        mount: &MountConfig,
        entity_id: RemoteId,
        issues: Vec<ValidationIssue>,
    ) -> Self {
        Self {
            ok: false,
            command,
            path: absolute_path.display().to_string(),
            mount_id: mount.mount_id.0.clone(),
            entity_id: entity_id.0,
            validation: issues
                .into_iter()
                .map(ValidationIssueOutput::from)
                .collect(),
            plan: None,
            guardrail: GuardrailOutput::proceed(),
            action: action_name(&PushPipelineAction::FixValidation).to_string(),
            unsupported: Vec::new(),
            message: None,
            suggested_fix: None,
            completed_stages: vec![stage_name(&PushStage::ParseAndValidate).to_string()],
        }
    }

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
            file: value.file.display().to_string(),
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
    pub blocks_moved: usize,
    pub blocks_archived: usize,
    pub entities_created: usize,
    pub entities_archived: usize,
    pub properties_updated: usize,
}

impl From<PlanSummary> for PlanSummaryOutput {
    fn from(value: PlanSummary) -> Self {
        Self {
            blocks_created: value.blocks_created,
            blocks_updated: value.blocks_updated,
            blocks_moved: value.blocks_moved,
            blocks_archived: value.blocks_archived,
            entities_created: value.entities_created,
            entities_archived: value.entities_archived,
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
    AppendBlock {
        parent_id: String,
        after: Option<String>,
        content: String,
    },
    MoveBlock {
        block_id: String,
        after: Option<String>,
    },
    ArchiveBlock {
        block_id: String,
    },
    ArchiveEntity {
        entity_id: String,
    },
    UpdateProperties {
        entity_id: String,
        keys: Vec<String>,
        properties: Vec<PropertyUpdateOutput>,
    },
    CreateEntity {
        parent_id: String,
        title: String,
        keys: Vec<String>,
        properties: Vec<PropertyUpdateOutput>,
        body: String,
        source_path: String,
    },
}

impl From<PushOperation> for PushOperationOutput {
    fn from(value: PushOperation) -> Self {
        match value {
            PushOperation::UpdateBlock { block_id, content } => Self::UpdateBlock {
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
            PushOperation::ArchiveBlock { block_id } => Self::ArchiveBlock {
                block_id: block_id.0,
            },
            PushOperation::ArchiveEntity { entity_id } => Self::ArchiveEntity {
                entity_id: entity_id.0,
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
            PushOperation::CreateEntity {
                parent_id,
                title,
                properties,
                body,
                source_path,
            } => Self::CreateEntity {
                parent_id: parent_id.0,
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
                source_path: source_path.display().to_string(),
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
