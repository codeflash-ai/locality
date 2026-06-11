//! Daemon-owned push execution.
//!
//! This module keeps the explicit push pipeline under the daemon execution
//! boundary. Planning remains core logic; connector calls and local state
//! mutation happen through one host so journal, apply, and reconcile cannot
//! drift across different store handles.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use afs_connector::{ApplyPlanRequest, ApplyPlanResult, Connector};
use afs_core::canonical::{
    CanonicalParseError, CanonicalParseErrorKind, parse_canonical_markdown,
    render_canonical_markdown,
};
use afs_core::journal::{
    JournalApplyEffect, JournalEntry, JournalPreimage, JournalStatus, JournalStore, PushId,
};
use afs_core::model::{EntityKind, HydrationState};
use afs_core::planner::GuardrailDecision;
use afs_core::push::{
    PushApplier, PushApplyRequest, PushApplyResult, PushApproval, PushConcurrencyCheck,
    PushConcurrencyRequest, PushExecutionRequest, PushExecutionResult, PushPipelineAction,
    PushPipelineRequest, PushPipelineResult, PushReconcileRequest, PushReconcileResult,
    PushReconciler, PushStage, RemotePrecondition, execute_journaled_push_with_host,
    plan_push_pipeline,
};
use afs_core::shadow::ShadowDocument;
use afs_core::validation::{ValidationIssue, ValidationReport};
use afs_core::{AfsError, AfsResult};
use afs_store::{
    EntityRecord, EntityRepository, JournalRepository, MountConfig, MountRepository,
    ShadowRepository, StoreError,
};
use serde::{Deserialize, Serialize};

use crate::execution::{PushJob, PushJobError, PushJobReport};
use crate::hydration::{HydratedEntity, HydrationSource};
use crate::virtual_fs::{virtual_fs_content_path, virtual_fs_content_root};

pub fn execute_push_job<S, Source>(
    store: &mut S,
    job: PushJob,
    source: &Source,
) -> AfsResult<PushJobReport>
where
    S: MountRepository + EntityRepository + ShadowRepository + JournalRepository + JournalStore,
    Source: Connector + HydrationSource + ?Sized,
{
    execute_push_job_with_content_root(store, job, source, None)
}

pub fn execute_push_job_with_content_root<S, Source>(
    store: &mut S,
    job: PushJob,
    source: &Source,
    state_root: Option<&Path>,
) -> AfsResult<PushJobReport>
where
    S: MountRepository + EntityRepository + ShadowRepository + JournalRepository + JournalStore,
    Source: Connector + HydrationSource + ?Sized,
{
    let prepared = preflight_push(source, prepare_push(store, &job, state_root)?);
    let push_id = generate_push_id();
    let mut execution_request = PushExecutionRequest::new(
        push_id.clone(),
        prepared.mount.mount_id.clone(),
        prepared.pipeline.clone(),
    )
    .with_remote_preconditions(vec![RemotePrecondition {
        remote_id: prepared.entity.remote_id.clone(),
        remote_edited_at: prepared.entity.remote_edited_at.clone(),
    }]);

    if let Some(shadow) = prepared.shadow.clone() {
        execution_request =
            execution_request.with_preimages(vec![JournalPreimage::from_shadow(shadow)]);
    } else if prepared.pipeline.action == PushPipelineAction::ProceedToApply {
        return Err(AfsError::InvalidState(
            "push pipeline approved apply without a shadow preimage".to_string(),
        ));
    }

    let execution_result = {
        let mut host = DaemonPushHost {
            store,
            source,
            state_root: state_root.map(Path::to_path_buf),
        };
        execute_journaled_push_with_host(&mut host, execution_request)
    };

    match execution_result {
        Ok(result) => Ok(PushJobReport {
            target_path: prepared.absolute_path,
            mount_id: prepared.mount.mount_id,
            entity_id: prepared.entity.remote_id,
            pipeline: prepared.pipeline,
            action: PushJobAction::from_execution(&result),
            push_id: Some(result.push_id.clone()),
            journal_status: result.journal_status.clone(),
            execution: Some(result),
            error: None,
        }),
        Err(error) => Ok(PushJobReport {
            target_path: prepared.absolute_path,
            mount_id: prepared.mount.mount_id,
            entity_id: prepared.entity.remote_id,
            pipeline: prepared.pipeline,
            action: PushJobAction::Failed,
            execution: None,
            push_id: Some(push_id.clone()),
            journal_status: journal_status_after_error(store, &push_id),
            error: Some(PushJobError::from(error)),
        }),
    }
}

fn preflight_push<Source>(source: &Source, mut prepared: PreparedPush) -> PreparedPush
where
    Source: Connector + ?Sized,
{
    let Some(plan) = prepared.pipeline.plan.as_ref() else {
        return prepared;
    };
    if !matches!(
        prepared.pipeline.action,
        PushPipelineAction::ProceedToApply
            | PushPipelineAction::ConfirmPlan
            | PushPipelineAction::ConfirmDangerousPlan
    ) {
        return prepared;
    }

    let supported = source.supported_push_operations();
    let unsupported = plan
        .operations
        .iter()
        .map(|operation| operation.kind())
        .filter(|kind| !supported.contains(kind))
        .map(|kind| kind.as_str().to_string())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();

    if !unsupported.is_empty() {
        prepared.pipeline.action = PushPipelineAction::unsupported_operations(unsupported);
    }

    prepared
}

#[derive(Clone, Debug)]
struct PreparedPush {
    absolute_path: PathBuf,
    mount: MountConfig,
    entity: EntityRecord,
    shadow: Option<ShadowDocument>,
    pipeline: PushPipelineResult,
}

fn prepare_push<S>(store: &S, job: &PushJob, state_root: Option<&Path>) -> AfsResult<PreparedPush>
where
    S: MountRepository + EntityRepository + ShadowRepository,
{
    let absolute_path = absolute_path(&job.target_path)?;
    let mounts = store.load_mounts().map_err(AfsError::from)?;
    let mount = find_mount_for_path(&mounts, &absolute_path)
        .cloned()
        .ok_or_else(|| {
            AfsError::InvalidState(format!("no mount contains `{}`", absolute_path.display()))
        })?;
    let relative_path = relative_entity_path(&mount, &absolute_path)?;
    let entity = store
        .find_entity_by_path(&mount.mount_id, &relative_path)
        .map_err(AfsError::from)?
        .ok_or_else(|| StoreError::EntityPathMissing {
            mount_id: mount.mount_id.clone(),
            path: relative_path.clone(),
        })
        .map_err(AfsError::from)?;

    if entity.hydration == HydrationState::Conflicted {
        return Ok(PreparedPush {
            absolute_path,
            mount,
            entity,
            shadow: None,
            pipeline: validation_pipeline(ValidationIssue::new(
                "entity_conflicted_requires_resolve",
                relative_path,
                None,
                "entity is conflicted; resolve it before pushing",
                Some("run `afs resolve --ours|--theirs|--edited <path>` first".to_string()),
            )),
        });
    }

    let read_path = projection_read_path(state_root, &mount, &relative_path, &absolute_path)?;
    let contents = read_to_string(&read_path)?;
    let parsed = match parse_canonical_markdown(&contents) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Ok(PreparedPush {
                absolute_path,
                mount,
                entity,
                shadow: None,
                pipeline: validation_pipeline(parse_error_issue(&relative_path, error)),
            });
        }
    };

    if parsed
        .remote_id()
        .is_some_and(|remote_id| remote_id != &entity.remote_id)
    {
        return Ok(PreparedPush {
            absolute_path,
            mount,
            entity,
            shadow: None,
            pipeline: validation_pipeline(ValidationIssue::new(
                "frontmatter_remote_id_mismatch",
                relative_path,
                Some(1),
                "frontmatter `afs.id` does not match the entity mapped to this path",
                Some("restore the generated `afs.id` for this file before pushing".to_string()),
            )),
        });
    }

    let shadow = store
        .load_shadow(&mount.mount_id, &entity.remote_id)
        .map_err(AfsError::from)?;
    let schema_validation =
        notion_changed_row_schema_validation(store, &mount, &relative_path, &parsed, &shadow)?;
    let pipeline = if schema_validation.is_clean() {
        plan_push_pipeline(
            PushPipelineRequest::new(relative_path, &parsed, &shadow)
                .with_approval(PushApproval {
                    assume_yes: job.assume_yes,
                    confirm_dangerous: job.confirm_dangerous,
                })
                .read_only(mount.read_only),
        )
    } else {
        validation_report_pipeline(schema_validation)
    };

    Ok(PreparedPush {
        absolute_path,
        mount,
        entity,
        shadow: Some(shadow),
        pipeline,
    })
}

fn projection_read_path(
    state_root: Option<&Path>,
    mount: &MountConfig,
    relative_path: &Path,
    absolute_path: &Path,
) -> AfsResult<PathBuf> {
    if mount.projection.uses_virtual_filesystem()
        && let Some(state_root) = state_root
    {
        return virtual_fs_content_path(state_root, &mount.mount_id, relative_path);
    }

    Ok(absolute_path.to_path_buf())
}

struct DaemonPushHost<'a, S, Source: ?Sized> {
    store: &'a mut S,
    source: &'a Source,
    state_root: Option<PathBuf>,
}

impl<S, Source> JournalStore for DaemonPushHost<'_, S, Source>
where
    S: JournalStore,
    Source: ?Sized,
{
    fn append(&mut self, entry: JournalEntry) -> AfsResult<()> {
        self.store.append(entry)
    }

    fn record_apply_effects(
        &mut self,
        push_id: &PushId,
        effects: Vec<JournalApplyEffect>,
    ) -> AfsResult<()> {
        self.store.record_apply_effects(push_id, effects)
    }

    fn update_status(&mut self, push_id: &PushId, status: JournalStatus) -> AfsResult<()> {
        self.store.update_status(push_id, status)
    }
}

impl<S, Source> PushConcurrencyCheck for DaemonPushHost<'_, S, Source>
where
    Source: Connector + ?Sized,
{
    fn check(&mut self, request: PushConcurrencyRequest<'_>) -> AfsResult<()> {
        self.source.check_concurrency(ApplyPlanRequest {
            push_id: request.push_id,
            mount_id: request.mount_id,
            plan: request.plan,
            operation_ids: request.operation_ids,
            remote_preconditions: request.remote_preconditions,
        })
    }
}

impl<S, Source> PushApplier for DaemonPushHost<'_, S, Source>
where
    Source: Connector + ?Sized,
{
    fn apply(&mut self, request: PushApplyRequest<'_>) -> AfsResult<PushApplyResult> {
        let result: ApplyPlanResult = self.source.apply(ApplyPlanRequest {
            push_id: request.push_id,
            mount_id: request.mount_id,
            plan: request.plan,
            operation_ids: request.operation_ids,
            remote_preconditions: request.remote_preconditions,
        })?;

        Ok(PushApplyResult {
            changed_remote_ids: result.changed_remote_ids,
            effects: result.effects,
        })
    }
}

impl<S, Source> PushReconciler for DaemonPushHost<'_, S, Source>
where
    S: MountRepository + EntityRepository + ShadowRepository,
    Source: HydrationSource + ?Sized,
{
    fn reconcile(&mut self, request: PushReconcileRequest<'_>) -> AfsResult<PushReconcileResult> {
        let mount = self
            .store
            .get_mount(request.mount_id)
            .map_err(AfsError::from)?
            .ok_or_else(|| StoreError::MountMissing(request.mount_id.clone()))
            .map_err(AfsError::from)?;
        let mut reconciled_remote_ids = Vec::new();

        for remote_id in request.changed_remote_ids {
            let mut entity = self
                .store
                .get_entity(request.mount_id, remote_id)
                .map_err(AfsError::from)?
                .ok_or_else(|| StoreError::EntityMissing {
                    mount_id: request.mount_id.clone(),
                    remote_id: remote_id.clone(),
                })
                .map_err(AfsError::from)?;
            let path = projection_write_path(self.state_root.as_deref(), &mount, &entity.path);
            let rendered =
                self.source
                    .fetch_render(&afs_core::hydration::HydrationRequest::new(
                        request.mount_id.clone(),
                        remote_id.clone(),
                        path.clone(),
                        HydrationState::Hydrated,
                        afs_core::hydration::HydrationReason::ExplicitPull,
                    ))?;

            accept_post_apply_remote(self.store, &mount, &mut entity, &path, rendered)?;
            reconciled_remote_ids.push(remote_id.clone());
        }

        Ok(PushReconcileResult {
            reconciled_remote_ids,
        })
    }
}

fn accept_post_apply_remote<S>(
    store: &mut S,
    mount: &MountConfig,
    entity: &mut EntityRecord,
    path: &Path,
    rendered: HydratedEntity,
) -> AfsResult<()>
where
    S: EntityRepository + ShadowRepository,
{
    write_atomic(path, render_canonical_markdown(&rendered.document))?;
    store
        .save_shadow(&mount.mount_id, rendered.shadow.clone())
        .map_err(AfsError::from)?;

    entity.hydration = HydrationState::Hydrated;
    entity.content_hash = Some(rendered.shadow.body_hash);
    if rendered.remote_edited_at.is_some() {
        entity.remote_edited_at = rendered.remote_edited_at;
    }
    store.save_entity(entity.clone()).map_err(AfsError::from)?;
    Ok(())
}

fn projection_write_path(
    state_root: Option<&Path>,
    mount: &MountConfig,
    relative_path: &Path,
) -> PathBuf {
    if mount.projection.uses_virtual_filesystem()
        && let Some(state_root) = state_root
    {
        return virtual_fs_content_root(state_root, &mount.mount_id).join(relative_path);
    }
    mount.root.join(relative_path)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushJobAction {
    NotReady,
    Failed,
    Reconciled,
}

impl PushJobAction {
    fn from_execution(result: &PushExecutionResult) -> Self {
        match result.action {
            afs_core::push::PushExecutionAction::NotReady { .. } => Self::NotReady,
            afs_core::push::PushExecutionAction::Reconciled => Self::Reconciled,
        }
    }
}

impl From<AfsError> for PushJobError {
    fn from(value: AfsError) -> Self {
        Self {
            code: afs_error_code(&value).to_string(),
            message: value.to_string(),
        }
    }
}

fn afs_error_code(error: &AfsError) -> &'static str {
    match error {
        AfsError::Validation(_) => "validation_failed",
        AfsError::Conflict(_) => "conflict",
        AfsError::Guardrail(_) => "guardrail",
        AfsError::InvalidState(_) => "invalid_state",
        AfsError::Unsupported(_) => "unsupported",
        AfsError::NotImplemented(_) => "not_implemented",
        AfsError::Io(_) => "io_error",
    }
}

fn validation_pipeline(issue: ValidationIssue) -> PushPipelineResult {
    let mut validation = ValidationReport::clean();
    validation.push(issue);
    validation_report_pipeline(validation)
}

fn validation_report_pipeline(validation: ValidationReport) -> PushPipelineResult {
    PushPipelineResult {
        validation,
        plan: None,
        guardrail: GuardrailDecision::Proceed,
        action: PushPipelineAction::FixValidation,
        completed_stages: vec![PushStage::ParseAndValidate],
    }
}

fn notion_changed_row_schema_validation<S>(
    store: &S,
    mount: &MountConfig,
    relative_path: &Path,
    parsed: &afs_core::canonical::ParsedCanonicalDocument,
    shadow: &ShadowDocument,
) -> AfsResult<ValidationReport>
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

fn notion_database_parent<S>(
    store: &S,
    mount: &MountConfig,
    relative_path: &Path,
) -> AfsResult<Option<EntityRecord>>
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
        .map_err(AfsError::from)?;
    Ok(parent.filter(|entity| entity.kind == EntityKind::Database))
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

fn absolute_path(path: &Path) -> AfsResult<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| AfsError::Io(format!("failed to resolve current directory: {error}")))
    }
}

fn find_mount_for_path<'a>(mounts: &'a [MountConfig], path: &Path) -> Option<&'a MountConfig> {
    mounts
        .iter()
        .filter(|mount| path.starts_with(&mount.root))
        .max_by_key(|mount| mount.root.components().count())
}

fn relative_entity_path(mount: &MountConfig, absolute_path: &Path) -> AfsResult<PathBuf> {
    absolute_path
        .strip_prefix(&mount.root)
        .map(Path::to_path_buf)
        .map_err(|_| {
            AfsError::InvalidState(format!("no mount contains `{}`", absolute_path.display()))
        })
}

fn read_to_string(path: &Path) -> AfsResult<String> {
    std::fs::read_to_string(path)
        .map_err(|error| AfsError::Io(format!("failed to read `{}`: {error}", path.display())))
}

fn write_atomic(path: &Path, contents: String) -> AfsResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("afs-write");
    let temp_path = path.with_file_name(format!(".{file_name}.afs-tmp"));

    std::fs::write(&temp_path, contents)?;
    std::fs::rename(&temp_path, path)?;
    Ok(())
}

fn generate_push_id() -> PushId {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    PushId(format!("push-{timestamp}-{}", std::process::id()))
}
