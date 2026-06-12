//! Daemon-owned push execution.
//!
//! This module keeps the explicit push pipeline under the daemon execution
//! boundary. Planning remains core logic; connector calls and local state
//! mutation happen through one host so journal, apply, and reconcile cannot
//! drift across different store handles.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use afs_connector::{ApplyPlanRequest, ApplyPlanResult, Connector};
use std::collections::BTreeMap;

use afs_core::canonical::{
    CanonicalParseError, CanonicalParseErrorKind, parse_canonical_markdown,
    render_canonical_markdown,
};
use afs_core::conflict::unresolved_conflict_marker_line;
use afs_core::diff::property_value_from_frontmatter;
use afs_core::journal::{
    JournalApplyEffect, JournalEntry, JournalPreimage, JournalStatus, JournalStore, PushId,
};
use afs_core::model::{EntityKind, HydrationState};
use afs_core::planner::GuardrailDecision;
use afs_core::planner::{GuardrailPolicy, PushOperation, PushPlan};
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
    ShadowRepository, StoreError, VirtualMutationKind, VirtualMutationRecord,
    VirtualMutationRepository,
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
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + JournalRepository
        + JournalStore
        + VirtualMutationRepository,
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
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + JournalRepository
        + JournalStore
        + VirtualMutationRepository,
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

    if !prepared.shadows.is_empty() {
        execution_request = execution_request.with_preimages(
            prepared
                .shadows
                .clone()
                .into_iter()
                .map(JournalPreimage::from_shadow)
                .collect(),
        );
    } else if prepared.pipeline.action == PushPipelineAction::ProceedToApply
        && !prepared.pipeline.plan.as_ref().is_some_and(|plan| {
            plan.operations
                .iter()
                .all(|operation| matches!(operation, PushOperation::CreateEntity { .. }))
        })
    {
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
    shadows: Vec<ShadowDocument>,
    pipeline: PushPipelineResult,
}

fn prepare_push<S>(store: &S, job: &PushJob, state_root: Option<&Path>) -> AfsResult<PreparedPush>
where
    S: MountRepository + EntityRepository + ShadowRepository + VirtualMutationRepository,
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
        .map_err(AfsError::from)?;

    let Some(entity) = entity else {
        if let Some(pending) = store
            .find_virtual_mutation_by_path(&mount.mount_id, &relative_path)
            .map_err(AfsError::from)?
        {
            return prepare_pending_create(store, job, state_root, absolute_path, mount, pending);
        }
        if relative_path.as_os_str().is_empty() || absolute_path.is_dir() {
            return prepare_pending_scope(
                store,
                job,
                state_root,
                absolute_path,
                mount,
                relative_path,
            );
        }
        return Err(StoreError::EntityPathMissing {
            mount_id: mount.mount_id.clone(),
            path: relative_path.clone(),
        }
        .into());
    };

    let read_path = projection_read_path(state_root, &mount, &relative_path, &absolute_path)?;
    let contents = read_to_string(&read_path)?;
    if let Some(line) = unresolved_conflict_marker_line(&contents) {
        return Ok(PreparedPush {
            absolute_path,
            mount,
            entity,
            shadows: Vec::new(),
            pipeline: validation_pipeline(unresolved_conflict_marker_issue(&relative_path, line)),
        });
    }

    let parsed = match parse_canonical_markdown(&contents) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Ok(PreparedPush {
                absolute_path,
                mount,
                entity,
                shadows: Vec::new(),
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
            shadows: Vec::new(),
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
    let schema_validation = notion_changed_row_schema_validation(
        store,
        state_root,
        &mount,
        &relative_path,
        &parsed,
        &shadow,
    )?;
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
        shadows: vec![shadow],
        pipeline,
    })
}

fn prepare_pending_create<S>(
    store: &S,
    job: &PushJob,
    state_root: Option<&Path>,
    absolute_path: PathBuf,
    mount: MountConfig,
    pending: VirtualMutationRecord,
) -> AfsResult<PreparedPush>
where
    S: EntityRepository + ShadowRepository + VirtualMutationRepository,
{
    if pending.mutation_kind != VirtualMutationKind::Create {
        return prepare_pending_scope(
            store,
            job,
            state_root,
            absolute_path,
            mount,
            pending.projected_path.clone(),
        );
    }
    let parent_id = pending.parent_remote_id.clone().ok_or_else(|| {
        AfsError::InvalidState(format!(
            "pending create `{}` is missing a parent remote id",
            pending.local_id
        ))
    })?;
    let parent = store
        .get_entity(&mount.mount_id, &parent_id)
        .map_err(AfsError::from)?
        .ok_or_else(|| StoreError::EntityMissing {
            mount_id: mount.mount_id.clone(),
            remote_id: parent_id.clone(),
        })
        .map_err(AfsError::from)?;
    let read_path = pending
        .content_path
        .clone()
        .or_else(|| {
            state_root.map(|root| {
                virtual_fs_content_root(root, &mount.mount_id).join(&pending.projected_path)
            })
        })
        .ok_or_else(|| {
            AfsError::InvalidState(format!(
                "pending create `{}` has no cached content path",
                pending.local_id
            ))
        })?;
    let contents = read_to_string(&read_path)?;
    if let Some(line) = unresolved_conflict_marker_line(&contents) {
        return Ok(PreparedPush {
            absolute_path,
            mount,
            entity: parent,
            shadows: Vec::new(),
            pipeline: validation_pipeline(unresolved_conflict_marker_issue(
                &pending.projected_path,
                line,
            )),
        });
    }
    let parsed = match parse_canonical_markdown(&contents) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Ok(PreparedPush {
                absolute_path,
                mount,
                entity: parent,
                shadows: Vec::new(),
                pipeline: validation_pipeline(parse_error_issue(&pending.projected_path, error)),
            });
        }
    };
    let schema_validation = if parent.kind == EntityKind::Database {
        match notion_schema_yaml_or_issue(state_root, &mount, &parent, &pending.projected_path) {
            Ok(schema) => afs_notion::schema::validate_create_row_frontmatter(
                &schema,
                &parsed,
                &pending.projected_path,
            ),
            Err(report) => report,
        }
    } else {
        ValidationReport::clean()
    };
    let pipeline = create_entity_pipeline(
        &pending.projected_path,
        &parsed,
        &parent,
        &mount,
        PushApproval {
            assume_yes: job.assume_yes,
            confirm_dangerous: job.confirm_dangerous,
        },
        schema_validation,
    );
    Ok(PreparedPush {
        absolute_path,
        mount,
        entity: parent,
        shadows: Vec::new(),
        pipeline,
    })
}

fn prepare_pending_scope<S>(
    store: &S,
    job: &PushJob,
    state_root: Option<&Path>,
    absolute_path: PathBuf,
    mount: MountConfig,
    relative_scope: PathBuf,
) -> AfsResult<PreparedPush>
where
    S: EntityRepository + ShadowRepository + VirtualMutationRepository,
{
    let mutations = store
        .list_virtual_mutations(&mount.mount_id)
        .map_err(AfsError::from)?
        .into_iter()
        .filter(|mutation| {
            relative_scope.as_os_str().is_empty()
                || mutation.projected_path.starts_with(&relative_scope)
        })
        .collect::<Vec<_>>();
    let Some(first) = mutations.first() else {
        return Err(StoreError::EntityPathMissing {
            mount_id: mount.mount_id.clone(),
            path: relative_scope,
        }
        .into());
    };
    let mut operations = Vec::new();
    let mut affected = Vec::new();
    let mut shadows = Vec::new();
    let mut representative = None;
    for mutation in &mutations {
        match mutation.mutation_kind {
            VirtualMutationKind::Delete => {
                let Some(remote_id) = mutation.target_remote_id.clone() else {
                    continue;
                };
                let entity = store
                    .get_entity(&mount.mount_id, &remote_id)
                    .map_err(AfsError::from)?
                    .ok_or_else(|| StoreError::EntityMissing {
                        mount_id: mount.mount_id.clone(),
                        remote_id: remote_id.clone(),
                    })
                    .map_err(AfsError::from)?;
                if representative.is_none() {
                    representative = Some(entity);
                }
                if let Ok(shadow) = store.load_shadow(&mount.mount_id, &remote_id) {
                    shadows.push(shadow);
                }
                operations.push(PushOperation::ArchiveEntity {
                    entity_id: remote_id.clone(),
                });
                affected.push(remote_id);
            }
            VirtualMutationKind::Create => {
                let pending_path = mount.root.join(&mutation.projected_path);
                return prepare_pending_create(
                    store,
                    job,
                    state_root,
                    pending_path,
                    mount,
                    mutation.clone(),
                );
            }
            VirtualMutationKind::Rename => {}
        }
    }
    let entity = representative.ok_or_else(|| {
        AfsError::InvalidState(format!(
            "no pushable pending virtual filesystem mutations under `{}`",
            first.projected_path.display()
        ))
    })?;
    let plan = PushPlan::new(affected, operations);
    let guardrail = afs_core::push::evaluate_guardrails(&plan, &GuardrailPolicy::default(), None);
    let action = match &guardrail {
        GuardrailDecision::Proceed if job.assume_yes => PushPipelineAction::ProceedToApply,
        GuardrailDecision::Proceed => PushPipelineAction::ConfirmPlan,
        GuardrailDecision::ConfirmRequired { .. } if job.confirm_dangerous => {
            PushPipelineAction::ProceedToApply
        }
        GuardrailDecision::ConfirmRequired { .. } => PushPipelineAction::ConfirmDangerousPlan,
    };
    Ok(PreparedPush {
        absolute_path,
        mount,
        entity,
        shadows,
        pipeline: PushPipelineResult {
            validation: ValidationReport::clean(),
            plan: Some(plan),
            guardrail,
            action,
            completed_stages: vec![
                PushStage::ParseAndValidate,
                PushStage::Diff,
                PushStage::PlanAndConfirm,
            ],
        },
    })
}

fn create_entity_pipeline(
    relative_path: &Path,
    parsed: &afs_core::canonical::ParsedCanonicalDocument,
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
            "new files must not carry an existing `afs.id`",
            Some(
                "remove the generated `afs.id`, or pull the existing page before editing"
                    .to_string(),
            ),
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
            "new files require a non-empty `title` frontmatter value",
            Some("add `title: \"...\"` to the YAML frontmatter".to_string()),
        ));
    }
    if parsed.is_stub() {
        validation.push(ValidationIssue::new(
            "create_entity_stub_body",
            relative_path,
            None,
            "new files cannot use the generated AFS stub marker as their body",
            Some("replace the stub marker with the page body, or leave the body empty".to_string()),
        ));
    }
    for directive in &parsed.directives {
        validation.push(ValidationIssue::new(
            "create_entity_directive_unsupported",
            relative_path,
            Some(directive.line),
            "new page creation does not support pre-seeded AFS directive blocks",
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
            parent_kind: Some(parent.kind.clone()),
            title: parsed.frontmatter.title.clone().unwrap_or_default(),
            properties,
            body: parsed.document.body.clone(),
            source_path: relative_path.to_path_buf(),
        }],
    );
    completed_stages.push(PushStage::Diff);
    let guardrail = afs_core::push::evaluate_guardrails(&plan, &GuardrailPolicy::default(), None);
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
    S: MountRepository + EntityRepository + ShadowRepository + VirtualMutationRepository,
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

        for effect in request.apply_effects {
            match effect {
                JournalApplyEffect::CreatedEntity {
                    operation_index,
                    entity_id,
                    ..
                } => {
                    let Some(PushOperation::CreateEntity {
                        title, source_path, ..
                    }) = request.plan.operations.get(*operation_index)
                    else {
                        continue;
                    };
                    let mut entity = EntityRecord::new(
                        request.mount_id.clone(),
                        entity_id.clone(),
                        EntityKind::Page,
                        title.clone(),
                        source_path.clone(),
                    )
                    .with_hydration(HydrationState::Stub);
                    self.store
                        .save_entity(entity.clone())
                        .map_err(AfsError::from)?;
                    let path =
                        projection_write_path(self.state_root.as_deref(), &mount, &entity.path);
                    let rendered =
                        self.source
                            .fetch_render(&afs_core::hydration::HydrationRequest::new(
                                request.mount_id.clone(),
                                entity_id.clone(),
                                path.clone(),
                                HydrationState::Hydrated,
                                afs_core::hydration::HydrationReason::ExplicitPull,
                            ))?;
                    accept_post_apply_remote(self.store, &mount, &mut entity, &path, rendered)?;
                    if let Some(mutation) = self
                        .store
                        .find_virtual_mutation_by_path(request.mount_id, source_path)
                        .map_err(AfsError::from)?
                    {
                        self.store
                            .delete_virtual_mutation(request.mount_id, &mutation.local_id)
                            .map_err(AfsError::from)?;
                    }
                    reconciled_remote_ids.push(entity_id.clone());
                }
                JournalApplyEffect::ArchivedEntity { entity_id, .. } => {
                    self.store
                        .delete_entity(request.mount_id, entity_id)
                        .map_err(AfsError::from)?;
                    self.store
                        .delete_virtual_mutation(
                            request.mount_id,
                            &format!("delete:{}", entity_id.0),
                        )
                        .map_err(AfsError::from)?;
                    reconciled_remote_ids.push(entity_id.clone());
                }
                _ => {}
            }
        }

        for remote_id in request.changed_remote_ids {
            if reconciled_remote_ids.iter().any(|id| id == remote_id) {
                continue;
            }
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
            self.store
                .delete_virtual_mutation(request.mount_id, &format!("rename:{}", remote_id.0))
                .map_err(AfsError::from)?;
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

fn unresolved_conflict_marker_issue(path: &Path, line: usize) -> ValidationIssue {
    ValidationIssue::new(
        "unresolved_conflict_markers",
        path,
        Some(line),
        "file contains unresolved conflict markers",
        Some(
            "edit the file to choose the intended content, remove every conflict marker line, then rerun `afs diff` or `afs push`"
                .to_string(),
        ),
    )
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
    state_root: Option<&Path>,
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
        match notion_schema_yaml_or_issue(state_root, mount, &parent, relative_path) {
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
    state_root: Option<&Path>,
    mount: &MountConfig,
    database: &EntityRecord,
    relative_path: &Path,
) -> Result<String, ValidationReport> {
    let schema_path = if mount.projection.uses_virtual_filesystem() {
        state_root
            .map(|root| virtual_fs_content_root(root, &mount.mount_id))
            .unwrap_or_else(|| mount.root.clone())
            .join(&database.path)
            .join("_schema.yaml")
    } else {
        mount.root.join(&database.path).join("_schema.yaml")
    };
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
