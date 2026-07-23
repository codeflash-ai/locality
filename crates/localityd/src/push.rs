//! Daemon-owned push execution.
//!
//! This module keeps the explicit push pipeline under the daemon execution
//! boundary. Planning remains core logic; connector calls and local state
//! mutation happen through one host so journal, apply, and reconcile cannot
//! drift across different store handles.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use locality_connector::{ApplyPlanRequest, ApplyPlanResult, Connector, ObserveRequest};

use locality_core::canonical::{
    CanonicalParseError, CanonicalParseErrorKind, parse_canonical_markdown,
    render_canonical_markdown,
};
use locality_core::conflict::unresolved_conflict_marker_line;
use locality_core::diff::property_value_from_frontmatter;
use locality_core::freshness::RemoteVersion;
use locality_core::journal::{
    JournalApplyEffect, JournalEntry, JournalLocalProjectionItem, JournalMetadata, JournalPreimage,
    JournalStatus, JournalStore, PushId,
};
use locality_core::model::{CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId};
use locality_core::path_projection::{
    is_page_document_path, page_container_path, page_document_path,
};
use locality_core::planner::GuardrailDecision;
use locality_core::planner::{GuardrailPolicy, PropertyValue, PushOperation, PushPlan};
use locality_core::push::{
    PushApplier, PushApplyRequest, PushApplyResult, PushApproval, PushConcurrencyCheck,
    PushConcurrencyRequest, PushExecutionRequest, PushExecutionResult, PushPipelineAction,
    PushPipelineRequest, PushPipelineResult, PushReconcileRequest, PushReconcileResult,
    PushReconciler, PushStage, RemotePrecondition, execute_journaled_push_with_host,
    plan_push_pipeline,
};
use locality_core::shadow::{
    MarkdownBlockKind, ShadowBlock, ShadowDocument, rendered_bodies_equivalent,
    segment_markdown_body,
};
use locality_core::validation::{ValidationIssue, ValidationReport};
use locality_core::{LocalityError, LocalityResult};
use locality_google_docs::render::{
    GOOGLE_DOCS_INLINE_OBJECT_NATIVE_KIND, GOOGLE_DOCS_TABLE_NATIVE_KIND,
};
use locality_linear::LINEAR_CONNECTOR_ID;
use locality_notion::database_create::parse_database_draft;
use locality_notion::markdown_table::parse_markdown_table_shape;
use locality_notion::media::{
    load_media_manifest, media_manifest_entry, resolve_media_href_with_content_root, sha256_hex,
};
use locality_store::{
    AutoSaveRepository, EntityRecord, EntityRepository, FreshnessStateRepository,
    JournalRepository, MountConfig, MountRepository, RemoteObservationRecord,
    RemoteObservationRepository, ShadowRepository, StoreError, VirtualMutationKind,
    VirtualMutationRecord, VirtualMutationRepository,
};
use serde::{Deserialize, Serialize};

use crate::autosave::{
    auto_save_block_reason, mark_auto_save_active, mark_auto_save_blocked,
    mark_auto_save_paused_failure, mark_auto_save_paused_remote_changed,
};
use crate::execution::{PushJob, PushJobError, PushJobReport};
use crate::file_provider;
use crate::hydration::{HydratedEntity, HydrationSource};
use crate::linear::linear_shadow_matches_with_legacy_lifecycle_frontmatter;
use crate::media::{render_document_with_absolute_media_hrefs, replace_hydrated_media_manifest};
use crate::projection_state;
use crate::shadow_match::shadows_match;
use crate::source::{
    LocalSourceValidator, SourcePushValidator, SourceValidationContext,
    source_create_decision_for_parent_path, source_descriptor, source_write_decision_for_path,
};
use crate::virtual_fs::{
    repair_legacy_macos_content_root, virtual_fs_content_path, virtual_fs_content_root,
    virtual_mutation_content_path_for_read,
};

pub fn execute_push_job<S, Source>(
    store: &mut S,
    job: PushJob,
    source: &Source,
) -> LocalityResult<PushJobReport>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + JournalRepository
        + JournalStore
        + RemoteObservationRepository
        + FreshnessStateRepository
        + AutoSaveRepository
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
) -> LocalityResult<PushJobReport>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + JournalRepository
        + JournalStore
        + RemoteObservationRepository
        + FreshnessStateRepository
        + AutoSaveRepository
        + VirtualMutationRepository,
    Source: Connector + HydrationSource + ?Sized,
{
    let validator = LocalSourceValidator;
    if let Some(state_root) = state_root {
        file_provider::reconcile_visible_projection(store, state_root, Some(&job.target_path))?;
    }
    projection_state::reconcile_projection_state_for_target(
        store,
        state_root,
        Some(&job.target_path),
    )?;
    repair_missing_database_schema_for_target(store, source, &job.target_path, state_root)?;
    let prepared = preflight_push(source, prepare_push(store, &job, state_root, &validator)?);
    let relative_path = auto_save_relative_path(&prepared);
    let report = execute_prepared_push(store, source, prepared, state_root)?;
    reactivate_auto_save_after_successful_explicit_push(store, &report, &relative_path)?;
    Ok(report)
}

pub fn execute_auto_save_push_job_with_content_root<S, Source>(
    store: &mut S,
    mut job: PushJob,
    source: &Source,
    state_root: Option<&Path>,
) -> LocalityResult<PushJobReport>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + JournalRepository
        + JournalStore
        + RemoteObservationRepository
        + FreshnessStateRepository
        + VirtualMutationRepository
        + AutoSaveRepository,
    Source: Connector + HydrationSource + ?Sized,
{
    job.assume_yes = true;
    job.confirm_dangerous = false;

    let validator = LocalSourceValidator;
    if let Some(state_root) = state_root {
        file_provider::reconcile_visible_projection(store, state_root, Some(&job.target_path))?;
    }
    projection_state::reconcile_projection_state_for_target(
        store,
        state_root,
        Some(&job.target_path),
    )?;
    let prepared = preflight_push(source, prepare_push(store, &job, state_root, &validator)?);
    let relative_path = auto_save_relative_path(&prepared);

    if let Some(reason) = auto_save_block_reason_for_prepared(&prepared) {
        mark_auto_save_blocked(
            store,
            &prepared.mount.mount_id,
            &relative_path,
            reason.clone(),
        )?;
        return Ok(PushJobReport {
            target_path: prepared.absolute_path,
            mount_id: prepared.mount.mount_id,
            entity_id: prepared.entity.remote_id,
            pipeline: prepared.pipeline,
            readable_diff: prepared.readable_diff,
            action: PushJobAction::NotReady,
            execution: None,
            push_id: None,
            journal_status: None,
            error: Some(PushJobError {
                code: "auto_save_blocked".to_string(),
                message: reason,
            }),
        });
    }

    let report = execute_prepared_push(store, source, prepared, state_root)?;
    match report.action {
        PushJobAction::Reconciled => {
            let created_remote_id = created_entity_id(&report).or_else(|| {
                report
                    .execution
                    .as_ref()
                    .and_then(|execution| execution.changed_remote_ids.first().cloned())
            });
            let remote_id = created_remote_id.or_else(|| Some(report.entity_id.clone()));
            mark_auto_save_active(
                store,
                &report.mount_id,
                &relative_path,
                remote_id,
                report.push_id.as_ref(),
            )?;
        }
        PushJobAction::NotReady if report.pipeline.action == PushPipelineAction::Noop => {
            mark_auto_save_active(
                store,
                &report.mount_id,
                &relative_path,
                Some(report.entity_id.clone()),
                report.push_id.as_ref(),
            )?;
        }
        PushJobAction::NotReady => {
            mark_auto_save_blocked(
                store,
                &report.mount_id,
                &relative_path,
                "push plan needs review before auto-save",
            )?;
        }
        PushJobAction::Failed => {
            let reason = report
                .error
                .as_ref()
                .map(|error| error.message.clone())
                .unwrap_or_else(|| "auto-save push failed".to_string());
            if auto_save_failed_due_remote_change(&report) {
                mark_auto_save_paused_remote_changed(
                    store,
                    &report.mount_id,
                    &relative_path,
                    reason,
                )?;
            } else {
                mark_auto_save_paused_failure(store, &report.mount_id, &relative_path, reason)?;
            }
        }
    }

    Ok(report)
}

fn reactivate_auto_save_after_successful_explicit_push<S>(
    store: &mut S,
    report: &PushJobReport,
    relative_path: &Path,
) -> LocalityResult<()>
where
    S: AutoSaveRepository,
{
    match report.action {
        PushJobAction::Reconciled => {
            let created_remote_id = created_entity_id(report).or_else(|| {
                report
                    .execution
                    .as_ref()
                    .and_then(|execution| execution.changed_remote_ids.first().cloned())
            });
            let remote_id = created_remote_id.or_else(|| Some(report.entity_id.clone()));
            mark_auto_save_active(
                store,
                &report.mount_id,
                relative_path,
                remote_id,
                report.push_id.as_ref(),
            )?;
        }
        PushJobAction::NotReady if report.pipeline.action == PushPipelineAction::Noop => {
            mark_auto_save_active(
                store,
                &report.mount_id,
                relative_path,
                Some(report.entity_id.clone()),
                report.push_id.as_ref(),
            )?;
        }
        PushJobAction::NotReady | PushJobAction::Failed => {}
    }

    Ok(())
}

fn auto_save_failed_due_remote_change(report: &PushJobReport) -> bool {
    report.error.as_ref().is_some_and(|error| {
        error.code == "guardrail"
            && (error.message.contains("changed since")
                || error.message.contains("pull before pushing"))
    })
}

fn execute_prepared_push<S, Source>(
    store: &mut S,
    source: &Source,
    prepared: PreparedPush,
    state_root: Option<&Path>,
) -> LocalityResult<PushJobReport>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + JournalRepository
        + JournalStore
        + RemoteObservationRepository
        + FreshnessStateRepository
        + VirtualMutationRepository,
    Source: Connector + HydrationSource + ?Sized,
{
    if let Some(report) = block_ambiguous_gmail_send_replay(store, &prepared)? {
        return Ok(report);
    }
    if let Some(report) =
        resume_failed_applied_reconciliation(store, source, &prepared, state_root)?
    {
        return Ok(report);
    }

    let push_id = generate_push_id();
    let remote_preconditions = remote_preconditions_for_plan(
        store,
        &prepared.mount,
        &prepared.entity,
        &prepared.pipeline,
    )?;
    let remote_ids = prepared
        .pipeline
        .plan
        .as_ref()
        .map(|plan| plan.affected_entities.clone())
        .unwrap_or_else(|| vec![prepared.entity.remote_id.clone()]);
    let previous_push_id =
        store.latest_journal_for_entities(&prepared.mount.mount_id, &remote_ids)?;
    let created_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis());
    let readable_diff = prepared.readable_diff.clone();
    let local_projection_items =
        local_projection_items_for_plan(store, &prepared.mount, prepared.pipeline.plan.as_ref())?;
    let mut execution_request = PushExecutionRequest::new(
        push_id.clone(),
        prepared.mount.mount_id.clone(),
        prepared.pipeline.clone(),
    )
    .with_remote_preconditions(remote_preconditions)
    .with_metadata(
        JournalMetadata::anonymous(previous_push_id, created_at_unix_ms)
            .with_local_projection_items(local_projection_items),
    )
    .with_readable_diff(readable_diff.clone());

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
        && !prepared
            .pipeline
            .plan
            .as_ref()
            .is_some_and(|plan| plan.operations.iter().all(is_create_operation))
    {
        return Err(LocalityError::InvalidState(
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
            readable_diff: readable_diff.clone(),
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
            readable_diff,
            action: PushJobAction::Failed,
            execution: None,
            push_id: Some(push_id.clone()),
            journal_status: journal_status_after_error(store, &push_id),
            error: Some(PushJobError::from(error)),
        }),
    }
}

fn local_projection_items_for_plan<S>(
    store: &S,
    mount: &MountConfig,
    plan: Option<&PushPlan>,
) -> LocalityResult<Vec<JournalLocalProjectionItem>>
where
    S: VirtualMutationRepository,
{
    if !mount.projection.uses_virtual_filesystem() {
        return Ok(Vec::new());
    }
    let Some(plan) = plan else {
        return Ok(Vec::new());
    };

    let mut items = Vec::new();
    for (operation_index, operation) in plan.operations.iter().enumerate() {
        let source_path = match operation {
            PushOperation::CreateEntity { source_path, .. }
            | PushOperation::CreateDatabase { source_path, .. } => source_path,
            _ => continue,
        };
        if let Some(mutation) = store
            .find_virtual_mutation_by_path(&mount.mount_id, source_path)
            .map_err(LocalityError::from)?
        {
            items.push(JournalLocalProjectionItem {
                operation_index,
                local_id: mutation.local_id,
            });
        }
    }
    Ok(items)
}

fn block_ambiguous_gmail_send_replay<S>(
    store: &S,
    prepared: &PreparedPush,
) -> LocalityResult<Option<PushJobReport>>
where
    S: JournalRepository,
{
    if prepared.mount.connector != "gmail"
        || prepared.pipeline.action != PushPipelineAction::ProceedToApply
    {
        return Ok(None);
    }
    let Some(plan) = prepared.pipeline.plan.as_ref() else {
        return Ok(None);
    };
    if plan.operations.is_empty()
        || !plan
            .operations
            .iter()
            .all(|operation| matches!(operation, PushOperation::CreateEntity { .. }))
    {
        return Ok(None);
    }

    let Some(journal) = latest_ambiguous_gmail_send_journal(store, &prepared.mount.mount_id, plan)?
    else {
        return Ok(None);
    };
    let error = LocalityError::Guardrail(
        "a previous Gmail send for this draft has an ambiguous result and may have already sent; inspect Gmail Sent Mail and the Locality journal before retrying"
            .to_string(),
    );

    Ok(Some(PushJobReport {
        target_path: prepared.absolute_path.clone(),
        mount_id: prepared.mount.mount_id.clone(),
        entity_id: prepared.entity.remote_id.clone(),
        pipeline: prepared.pipeline.clone(),
        readable_diff: prepared.readable_diff.clone(),
        action: PushJobAction::Failed,
        execution: None,
        push_id: Some(journal.push_id),
        journal_status: Some(journal.status),
        error: Some(PushJobError::from(error)),
    }))
}

fn latest_ambiguous_gmail_send_journal<S>(
    store: &S,
    mount_id: &MountId,
    plan: &PushPlan,
) -> LocalityResult<Option<JournalEntry>>
where
    S: JournalRepository,
{
    let mut latest = None;
    for journal in store.list_journal().map_err(LocalityError::from)? {
        if journal.mount_id != *mount_id
            || !journal_created_entity_source_paths_match(&journal.plan, plan)
        {
            continue;
        }
        if latest
            .as_ref()
            .is_none_or(|current| journal_is_newer(&journal, current))
        {
            latest = Some(journal);
        }
    }

    Ok(latest.filter(|journal: &JournalEntry| {
        journal.apply_effects.is_empty() && ambiguous_gmail_send_status(&journal.status)
    }))
}

fn ambiguous_gmail_send_status(status: &JournalStatus) -> bool {
    match status {
        JournalStatus::Applying => true,
        JournalStatus::Failed(message) => message.contains("gmail draft send"),
        _ => false,
    }
}

fn resume_failed_applied_reconciliation<S, Source>(
    store: &mut S,
    source: &Source,
    prepared: &PreparedPush,
    state_root: Option<&Path>,
) -> LocalityResult<Option<PushJobReport>>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + JournalRepository
        + JournalStore
        + RemoteObservationRepository
        + FreshnessStateRepository
        + VirtualMutationRepository,
    Source: Connector + HydrationSource + ?Sized,
{
    if prepared.pipeline.action != PushPipelineAction::ProceedToApply {
        return Ok(None);
    }
    let Some(plan) = prepared.pipeline.plan.as_ref() else {
        return Ok(None);
    };
    let Some(journal) = latest_resumable_entity_journal(
        store,
        &prepared.mount.mount_id,
        &prepared.mount.connector,
        plan,
    )?
    else {
        return Ok(None);
    };

    let changed_remote_ids = resumable_entity_effect_ids(&journal, &prepared.mount.connector)
        .ok_or_else(|| {
            LocalityError::InvalidState(format!(
                "journal `{}` no longer has complete entity-operation effects",
                journal.push_id.0
            ))
        })?;
    let mut completed_stages = prepared.pipeline.completed_stages.clone();
    completed_stages.push(PushStage::ConcurrencyCheckAndApply);
    completed_stages.push(PushStage::JournalAndReconcile);

    let reconcile_result = {
        let mut host = DaemonPushHost {
            store,
            source,
            state_root: state_root.map(Path::to_path_buf),
        };
        host.update_status(&journal.push_id, JournalStatus::Applied)?;
        match host.reconcile(PushReconcileRequest {
            push_id: &journal.push_id,
            mount_id: &journal.mount_id,
            plan: &journal.plan,
            changed_remote_ids: &changed_remote_ids,
            apply_effects: &journal.apply_effects,
        }) {
            Ok(result) => {
                host.update_status(&journal.push_id, JournalStatus::Reconciled)?;
                Ok(result)
            }
            Err(error) => {
                host.update_status(&journal.push_id, JournalStatus::Failed(error.to_string()))?;
                Err(error)
            }
        }
    };

    match reconcile_result {
        Ok(result) => Ok(Some(PushJobReport {
            target_path: prepared.absolute_path.clone(),
            mount_id: prepared.mount.mount_id.clone(),
            entity_id: prepared.entity.remote_id.clone(),
            pipeline: prepared.pipeline.clone(),
            readable_diff: prepared.readable_diff.clone(),
            action: PushJobAction::Reconciled,
            execution: Some(PushExecutionResult {
                push_id: journal.push_id.clone(),
                action: locality_core::push::PushExecutionAction::Reconciled,
                changed_remote_ids,
                apply_effects: journal.apply_effects.clone(),
                reconciled_remote_ids: result.reconciled_remote_ids,
                journal_status: Some(JournalStatus::Reconciled),
                completed_stages,
            }),
            push_id: Some(journal.push_id),
            journal_status: Some(JournalStatus::Reconciled),
            error: None,
        })),
        Err(error) => Ok(Some(PushJobReport {
            target_path: prepared.absolute_path.clone(),
            mount_id: prepared.mount.mount_id.clone(),
            entity_id: prepared.entity.remote_id.clone(),
            pipeline: prepared.pipeline.clone(),
            readable_diff: prepared.readable_diff.clone(),
            action: PushJobAction::Failed,
            execution: None,
            push_id: Some(journal.push_id.clone()),
            journal_status: journal_status_after_error(store, &journal.push_id),
            error: Some(PushJobError::from(error)),
        })),
    }
}

fn latest_resumable_entity_journal<S>(
    store: &S,
    mount_id: &MountId,
    connector: &str,
    plan: &PushPlan,
) -> LocalityResult<Option<JournalEntry>>
where
    S: JournalRepository,
{
    let mut latest = None;
    for journal in store.list_journal().map_err(LocalityError::from)? {
        if journal.mount_id != *mount_id
            || !matches!(
                journal.status,
                JournalStatus::Failed(_) | JournalStatus::Applied
            )
            || !resumable_plan_matches(&journal, plan, connector)
        {
            continue;
        }
        if latest
            .as_ref()
            .is_none_or(|current| journal_is_newer(&journal, current))
        {
            latest = Some(journal);
        }
    }

    let Some(journal) = latest else {
        return Ok(None);
    };
    if resumable_entity_effect_ids(&journal, connector).is_some() {
        return Ok(Some(journal));
    }
    if journal.status == JournalStatus::Applied || !journal.apply_effects.is_empty() {
        return Err(LocalityError::InvalidState(format!(
            "journal `{}` was applied but its effects do not safely cover the prepared plan; refusing to apply again",
            journal.push_id.0
        )));
    }
    Ok(None)
}

fn resumable_plan_matches(journal: &JournalEntry, plan: &PushPlan, connector: &str) -> bool {
    if journal.plan == *plan {
        return true;
    }

    create_only_effects_may_change_parent_for_connector(connector)
        && resumable_created_entity_effects(journal)
        && plan.operations.iter().all(is_create_operation)
        && journal.plan.operations.iter().all(is_create_operation)
        && journal_created_entity_sources_match(journal, plan)
}

fn resumable_created_entity_effects(journal: &JournalEntry) -> bool {
    !journal.apply_effects.is_empty()
        && journal
            .apply_effects
            .iter()
            .all(|effect| matches!(effect, JournalApplyEffect::CreatedEntity { .. }))
}

fn resumable_entity_effect_ids(journal: &JournalEntry, connector: &str) -> Option<Vec<RemoteId>> {
    if journal.plan.operations.is_empty()
        || journal.apply_effects.len() != journal.plan.operations.len()
    {
        return None;
    }

    let mut seen_operations = BTreeSet::new();
    let mut changed_remote_ids = Vec::new();
    for effect in &journal.apply_effects {
        let (operation_index, changed_remote_id) = match effect {
            JournalApplyEffect::CreatedEntity {
                operation_index,
                parent_id,
                entity_id,
                ..
            } => {
                let Some((planned_parent_id, _source_path)) = journal
                    .plan
                    .operations
                    .get(*operation_index)
                    .and_then(create_operation_source)
                else {
                    return None;
                };
                if entity_id.0.trim().is_empty() {
                    return None;
                }
                if parent_id != planned_parent_id
                    && !create_only_effects_may_change_parent(connector, &journal.plan)
                {
                    return None;
                }
                (*operation_index, entity_id)
            }
            JournalApplyEffect::MovedEntity {
                operation_index,
                entity_id,
                parent_id,
                ..
            } => {
                let Some(PushOperation::MoveEntity {
                    entity_id: planned_entity_id,
                    new_parent_id,
                    ..
                }) = journal.plan.operations.get(*operation_index)
                else {
                    return None;
                };
                if entity_id != planned_entity_id || parent_id != new_parent_id {
                    return None;
                }
                (*operation_index, entity_id)
            }
            JournalApplyEffect::ArchivedEntity {
                operation_index,
                entity_id,
                ..
            } => {
                let Some(PushOperation::ArchiveEntity {
                    entity_id: planned_entity_id,
                }) = journal.plan.operations.get(*operation_index)
                else {
                    return None;
                };
                if entity_id != planned_entity_id {
                    return None;
                }
                (*operation_index, entity_id)
            }
            _ => return None,
        };
        if !seen_operations.insert(operation_index) {
            return None;
        }
        if !changed_remote_ids.contains(changed_remote_id) {
            changed_remote_ids.push(changed_remote_id.clone());
        }
    }

    (seen_operations.len() == journal.plan.operations.len()).then_some(changed_remote_ids)
}

fn create_only_effects_may_change_parent(connector: &str, plan: &PushPlan) -> bool {
    create_only_effects_may_change_parent_for_connector(connector)
        && plan
            .operations
            .iter()
            .all(|operation| matches!(operation, PushOperation::CreateEntity { .. }))
}

fn create_only_effects_may_change_parent_for_connector(connector: &str) -> bool {
    matches!(connector, "gmail" | "google-calendar")
}

fn journal_created_entity_sources_match(journal: &JournalEntry, plan: &PushPlan) -> bool {
    let mut current_sources = plan_create_entity_sources(plan);

    for effect in &journal.apply_effects {
        let JournalApplyEffect::CreatedEntity {
            operation_index, ..
        } = effect
        else {
            return false;
        };
        let Some((parent_id, source_path)) = journal
            .plan
            .operations
            .get(*operation_index)
            .and_then(create_operation_source)
        else {
            return false;
        };

        let Some(matched_index) =
            current_sources
                .iter()
                .position(|(current_parent_id, current_source_path)| {
                    *current_parent_id == parent_id && *current_source_path == source_path
                })
        else {
            return false;
        };
        current_sources.remove(matched_index);
    }

    current_sources.is_empty()
}

fn journal_created_entity_source_paths_match(left: &PushPlan, right: &PushPlan) -> bool {
    let mut left_sources = plan_create_entity_sources(left);
    let mut right_sources = plan_create_entity_sources(right);
    left_sources.sort();
    right_sources.sort();
    left_sources == right_sources
}

fn plan_create_entity_sources(plan: &PushPlan) -> Vec<(&RemoteId, &PathBuf)> {
    plan.operations
        .iter()
        .filter_map(|operation| match operation {
            PushOperation::CreateEntity {
                parent_id,
                source_path,
                ..
            } => Some((parent_id, source_path)),
            PushOperation::CreateDatabase {
                parent_id,
                source_path,
                ..
            } => Some((parent_id, source_path)),
            _ => None,
        })
        .collect()
}

fn journal_is_newer(candidate: &JournalEntry, current: &JournalEntry) -> bool {
    match (
        candidate.metadata.created_at_unix_ms,
        current.metadata.created_at_unix_ms,
    ) {
        (Some(candidate_time), Some(current_time)) if candidate_time != current_time => {
            candidate_time > current_time
        }
        (Some(_), None) => true,
        (None, Some(_)) => false,
        _ => candidate.push_id.0 > current.push_id.0,
    }
}

fn auto_save_relative_path(prepared: &PreparedPush) -> PathBuf {
    prepared
        .pipeline
        .plan
        .as_ref()
        .and_then(|plan| {
            plan.operations
                .iter()
                .find_map(|operation| match operation {
                    PushOperation::CreateEntity { source_path, .. }
                    | PushOperation::CreateDatabase { source_path, .. } => {
                        Some(source_path.clone())
                    }
                    _ => None,
                })
        })
        .unwrap_or_else(|| prepared.entity.path.clone())
}

fn is_create_operation(operation: &PushOperation) -> bool {
    matches!(
        operation,
        PushOperation::CreateEntity { .. } | PushOperation::CreateDatabase { .. }
    )
}

fn create_operation_source(operation: &PushOperation) -> Option<(&RemoteId, &PathBuf)> {
    match operation {
        PushOperation::CreateEntity {
            parent_id,
            source_path,
            ..
        }
        | PushOperation::CreateDatabase {
            parent_id,
            source_path,
            ..
        } => Some((parent_id, source_path)),
        _ => None,
    }
}

fn auto_save_block_reason_for_prepared(prepared: &PreparedPush) -> Option<String> {
    draft_create_auto_save_block_reason(prepared)
        .or_else(|| auto_save_block_reason(&prepared.pipeline))
}

fn draft_create_auto_save_block_reason(prepared: &PreparedPush) -> Option<String> {
    let plan = prepared.pipeline.plan.as_ref()?;
    if !plan
        .operations
        .iter()
        .any(|operation| matches!(operation, PushOperation::CreateEntity { .. }))
    {
        return None;
    }
    match prepared.mount.connector.as_str() {
        "gmail" => Some("Gmail draft sends require review".to_string()),
        "google-calendar" => Some("Google Calendar event creates require review".to_string()),
        _ => None,
    }
}

fn created_entity_id(report: &PushJobReport) -> Option<RemoteId> {
    report
        .execution
        .as_ref()?
        .apply_effects
        .iter()
        .find_map(|effect| match effect {
            JournalApplyEffect::CreatedEntity { entity_id, .. } => Some(entity_id.clone()),
            _ => None,
        })
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

fn augment_notion_media_plan(
    mount: &MountConfig,
    state_root: Option<&Path>,
    relative_path: &Path,
    parsed: &locality_core::canonical::ParsedCanonicalDocument,
    shadow: &ShadowDocument,
    approval: PushApproval,
    pipeline: &mut PushPipelineResult,
) {
    if mount.connector != "notion" {
        return;
    }
    let Some(plan) = pipeline.plan.as_mut() else {
        return;
    };
    if !matches!(
        pipeline.action,
        PushPipelineAction::Noop
            | PushPipelineAction::ProceedToApply
            | PushPipelineAction::ConfirmPlan
            | PushPipelineAction::ConfirmDangerousPlan
    ) {
        return;
    }
    let Ok(root) = projection_output_root(state_root, mount) else {
        return;
    };
    let Ok(manifest) = load_media_manifest(&root) else {
        return;
    };
    let edited_blocks = segment_markdown_body(&parsed.document.body, parsed.body_start_line);
    let mut media_updates = BTreeMap::<RemoteId, PushOperation>::new();

    for (index, shadow_block) in shadow.blocks.iter().enumerate() {
        let Some((shadow_shape, shadow_caption, shadow_href)) =
            parse_file_like_media_markdown(&shadow_block.text)
        else {
            continue;
        };
        let Some(shadow_path) =
            resolve_media_href_with_content_root(relative_path, shadow_href, &root)
        else {
            continue;
        };
        let Some(entry) = media_manifest_entry(&manifest, &shadow_path) else {
            continue;
        };
        if entry.block_id != shadow_block.remote_id.as_str()
            || !is_file_like_media_kind(&entry.kind)
            || !media_markdown_shape_matches_kind(shadow_shape, &entry.kind)
        {
            continue;
        }
        let Some(edited_block) = edited_blocks.get(index) else {
            continue;
        };
        let Some((edited_shape, caption, edited_href)) =
            parse_file_like_media_markdown(&edited_block.text)
        else {
            continue;
        };
        if !media_markdown_shape_matches_kind(edited_shape, &entry.kind) {
            continue;
        }
        let Some(local_path) =
            resolve_media_href_with_content_root(relative_path, edited_href, &root)
        else {
            continue;
        };
        let markdown_changed = shadow_caption != caption || shadow_path != local_path;
        let bytes_changed = std::fs::read(root.join(&local_path))
            .map(|bytes| sha256_hex(&bytes) != entry.sha256)
            .unwrap_or(false);
        if bytes_changed || markdown_changed {
            media_updates.insert(
                shadow_block.remote_id.clone(),
                PushOperation::UpdateMedia {
                    block_id: shadow_block.remote_id.clone(),
                    local_path,
                    caption: caption.to_string(),
                },
            );
        }
    }

    if media_updates.is_empty() {
        return;
    }

    let mut operations = Vec::with_capacity(plan.operations.len() + media_updates.len());
    for operation in plan.operations.drain(..) {
        match &operation {
            PushOperation::UpdateBlock { block_id, .. } if media_updates.contains_key(block_id) => {
            }
            _ => operations.push(operation),
        }
    }
    operations.extend(media_updates.into_values());
    let degradations = plan.degradations.clone();
    *plan =
        PushPlan::new(plan.affected_entities.clone(), operations).with_degradations(degradations);

    let guardrail =
        locality_core::push::evaluate_guardrails(plan, &GuardrailPolicy::default(), None);
    pipeline.action = match &guardrail {
        GuardrailDecision::Proceed if approval.assume_yes => PushPipelineAction::ProceedToApply,
        GuardrailDecision::Proceed => PushPipelineAction::ConfirmPlan,
        GuardrailDecision::ConfirmRequired { .. } if approval.confirm_dangerous => {
            PushPipelineAction::ProceedToApply
        }
        GuardrailDecision::ConfirmRequired { .. } => PushPipelineAction::ConfirmDangerousPlan,
    };
    pipeline.guardrail = guardrail;
    if !pipeline
        .completed_stages
        .contains(&PushStage::PlanAndConfirm)
    {
        pipeline.completed_stages.push(PushStage::PlanAndConfirm);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaMarkdownShape {
    Image,
    Link,
}

fn parse_file_like_media_markdown(input: &str) -> Option<(MediaMarkdownShape, &str, &str)> {
    let trimmed = input.trim();
    if let Some(link) = trimmed.strip_prefix('!') {
        let (label, href) = parse_markdown_link_exact(link)?;
        return Some((MediaMarkdownShape::Image, label, href));
    }

    let (label, href) = parse_markdown_link_exact(trimmed)?;
    Some((MediaMarkdownShape::Link, label, href))
}

fn parse_markdown_link_exact(input: &str) -> Option<(&str, &str)> {
    if !input.starts_with('[') {
        return None;
    }
    let label_end = find_markdown_link_label_end(input)?;
    let href_start = label_end + 2;
    let href_end = find_markdown_link_href_end(input, href_start)?;
    if href_end + 1 != input.len() {
        return None;
    }
    Some((&input[1..label_end], &input[href_start..href_end]))
}

fn find_markdown_link_label_end(input: &str) -> Option<usize> {
    let mut escaped = false;
    for (index, ch) in input.char_indices().skip(1) {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == ']' && input[index + ch.len_utf8()..].starts_with('(') {
            return Some(index);
        }
    }
    None
}

fn find_markdown_link_href_end(input: &str, href_start: usize) -> Option<usize> {
    let mut escaped = false;
    let mut paren_depth = 0usize;

    for (offset, ch) in input[href_start..].char_indices() {
        let index = href_start + offset;
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        match ch {
            '(' => paren_depth += 1,
            ')' if paren_depth == 0 => return Some(index),
            ')' => paren_depth -= 1,
            _ => {}
        }
    }

    None
}

fn validate_notion_pre_apply_semantics(
    mount: &MountConfig,
    relative_path: &Path,
    shadow: &ShadowDocument,
    pipeline: &mut PushPipelineResult,
) {
    if mount.connector != "notion" {
        return;
    }
    if !matches!(
        pipeline.action,
        PushPipelineAction::ProceedToApply
            | PushPipelineAction::ConfirmPlan
            | PushPipelineAction::ConfirmDangerousPlan
    ) {
        return;
    }
    let Some(plan) = pipeline.plan.as_ref() else {
        return;
    };
    let validation = notion_pre_apply_semantic_validation(relative_path, shadow, plan);
    if validation.is_clean() {
        return;
    }

    pipeline.validation.extend(validation);
    pipeline.plan = None;
    pipeline.guardrail = GuardrailDecision::Proceed;
    pipeline.action = PushPipelineAction::FixValidation;
}

fn validate_google_docs_pre_apply_semantics(
    mount: &MountConfig,
    relative_path: &Path,
    shadow: &ShadowDocument,
    pipeline: &mut PushPipelineResult,
) {
    if mount.connector != "google-docs" {
        return;
    }
    if !matches!(
        pipeline.action,
        PushPipelineAction::ProceedToApply
            | PushPipelineAction::ConfirmPlan
            | PushPipelineAction::ConfirmDangerousPlan
    ) {
        return;
    }
    let Some(plan) = pipeline.plan.as_ref() else {
        return;
    };
    let validation = google_docs_pre_apply_semantic_validation(relative_path, shadow, plan);
    if validation.is_clean() {
        return;
    }

    pipeline.validation.extend(validation);
    pipeline.plan = None;
    pipeline.guardrail = GuardrailDecision::Proceed;
    pipeline.action = PushPipelineAction::FixValidation;
}

fn google_docs_pre_apply_semantic_validation(
    relative_path: &Path,
    shadow: &ShadowDocument,
    plan: &PushPlan,
) -> ValidationReport {
    let shadow_blocks = shadow
        .blocks
        .iter()
        .map(|block| (&block.remote_id, block))
        .collect::<BTreeMap<_, _>>();
    let archived_block_ids = plan
        .operations
        .iter()
        .filter_map(|operation| match operation {
            PushOperation::ArchiveBlock { block_id } => Some(block_id.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let moved_inline_object_block_ids = plan
        .operations
        .iter()
        .filter_map(|operation| match operation {
            PushOperation::AppendBlock { content, .. } => moved_rendered_native_block(
                &shadow_blocks,
                &archived_block_ids,
                content,
                GOOGLE_DOCS_INLINE_OBJECT_NATIVE_KIND,
            )
            .map(|block| block.remote_id.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let mut validation = ValidationReport::clean();
    for operation in &plan.operations {
        match operation {
            PushOperation::UpdateBlock { block_id, .. }
            | PushOperation::ReplaceBlock { block_id, .. } => {
                let Some(shadow_block) = shadow_blocks.get(block_id).copied() else {
                    continue;
                };
                if rendered_native_kind(shadow_block, GOOGLE_DOCS_INLINE_OBJECT_NATIVE_KIND) {
                    validation.push(ValidationIssue::new(
                        "google_docs_inline_object_edit_unsupported",
                        relative_path,
                        Some(shadow_block.source_span.start_line),
                        "editing rendered Google Docs inline images is not supported yet",
                        Some(
                            "restore the rendered image Markdown or edit the image in Google Docs"
                                .to_string(),
                        ),
                    ));
                }
            }
            PushOperation::AppendBlock { content, .. } => {
                if let Some(shadow_block) = moved_rendered_native_block(
                    &shadow_blocks,
                    &archived_block_ids,
                    content,
                    GOOGLE_DOCS_INLINE_OBJECT_NATIVE_KIND,
                ) {
                    validation.push(ValidationIssue::new(
                        "google_docs_inline_object_move_unsupported",
                        relative_path,
                        Some(shadow_block.source_span.start_line),
                        "moving rendered Google Docs inline images is not supported yet",
                        Some(
                            "restore the rendered image Markdown to its original position"
                                .to_string(),
                        ),
                    ));
                }
                if let Some(shadow_block) = moved_rendered_native_block(
                    &shadow_blocks,
                    &archived_block_ids,
                    content,
                    GOOGLE_DOCS_TABLE_NATIVE_KIND,
                ) {
                    validation.push(ValidationIssue::new(
                        "google_docs_table_move_unsupported",
                        relative_path,
                        Some(shadow_block.source_span.start_line),
                        "moving rendered Google Docs tables is not supported yet",
                        Some(
                            "restore the rendered Markdown table to its original position"
                                .to_string(),
                        ),
                    ));
                }
            }
            PushOperation::ArchiveBlock { block_id } => {
                if moved_inline_object_block_ids.contains(block_id) {
                    continue;
                }
                let Some(shadow_block) = shadow_blocks.get(block_id).copied() else {
                    continue;
                };
                if rendered_native_kind(shadow_block, GOOGLE_DOCS_INLINE_OBJECT_NATIVE_KIND) {
                    validation.push(ValidationIssue::new(
                        "google_docs_inline_object_delete_unsupported",
                        relative_path,
                        Some(shadow_block.source_span.start_line),
                        "deleting rendered Google Docs inline images is not supported yet",
                        Some("restore the rendered image Markdown".to_string()),
                    ));
                }
            }
            _ => {}
        }
    }

    validation
}

fn notion_pre_apply_semantic_validation(
    relative_path: &Path,
    shadow: &ShadowDocument,
    plan: &PushPlan,
) -> ValidationReport {
    let shadow_blocks = shadow
        .blocks
        .iter()
        .map(|block| (&block.remote_id, block))
        .collect::<BTreeMap<_, _>>();
    let archived_block_ids = plan
        .operations
        .iter()
        .filter_map(|operation| match operation {
            PushOperation::ArchiveBlock { block_id } => Some(block_id.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let moved_child_page_block_ids = plan
        .operations
        .iter()
        .filter_map(|operation| match operation {
            PushOperation::AppendBlock { content, .. } => {
                moved_rendered_child_page_link_block(&shadow_blocks, &archived_block_ids, content)
                    .map(|block| block.remote_id.clone())
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let moved_link_preview_block_ids = plan
        .operations
        .iter()
        .filter_map(|operation| match operation {
            PushOperation::AppendBlock { content, .. } => moved_rendered_native_block(
                &shadow_blocks,
                &archived_block_ids,
                content,
                "link_preview",
            )
            .map(|block| block.remote_id.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let mut validation = ValidationReport::clean();
    for operation in &plan.operations {
        match operation {
            PushOperation::UpdateBlock { block_id, content }
            | PushOperation::ReplaceBlock { block_id, content } => {
                let Some(shadow_block) = shadow_blocks.get(block_id).copied() else {
                    continue;
                };
                if rendered_native_kind(shadow_block, "link_preview") {
                    validation.push(ValidationIssue::new(
                        "notion_link_preview_edit_unsupported",
                        relative_path,
                        Some(shadow_block.source_span.start_line),
                        "editing a rendered Notion link-preview block is not supported from Markdown",
                        Some("restore the link-preview line exactly".to_string()),
                    ));
                }
                if rendered_child_page_link_block(shadow_block) {
                    validation.push(ValidationIssue::new(
                        "notion_child_page_link_edit_unsupported",
                        relative_path,
                        Some(shadow_block.source_span.start_line),
                        "editing a rendered Notion child-page link is not supported from parent Markdown",
                        Some(
                            "edit, move, rename, or delete the child page through its projected page directory instead"
                                .to_string(),
                        ),
                    ));
                }
                if let Some(issue) =
                    rendered_link_to_page_edit_validation(relative_path, shadow_block, content)
                {
                    validation.push(issue);
                }
                if let Some(issue) =
                    notion_table_shape_change_validation(relative_path, shadow_block, content)
                {
                    validation.push(issue);
                }
            }
            PushOperation::AppendBlock { content, .. } => {
                if let Some(issue) = moved_rendered_link_to_page_edit_validation(
                    relative_path,
                    &shadow_blocks,
                    &archived_block_ids,
                    content,
                ) {
                    validation.push(issue);
                    continue;
                }

                if let Some(shadow_block) = moved_rendered_native_block(
                    &shadow_blocks,
                    &archived_block_ids,
                    content,
                    "link_preview",
                ) {
                    validation.push(ValidationIssue::new(
                        "notion_link_preview_move_unsupported",
                        relative_path,
                        Some(shadow_block.source_span.start_line),
                        "moving a rendered Notion link-preview block is not supported from Markdown",
                        Some("restore the link-preview line to its original position".to_string()),
                    ));
                    continue;
                }

                let Some(shadow_block) = moved_rendered_child_page_link_block(
                    &shadow_blocks,
                    &archived_block_ids,
                    content,
                ) else {
                    continue;
                };

                validation.push(ValidationIssue::new(
                    "notion_child_page_link_move_unsupported",
                    relative_path,
                    Some(shadow_block.source_span.start_line),
                    "moving a rendered Notion child-page link is not supported from parent Markdown",
                    Some(
                        "move, rename, or delete the child page through its projected page directory instead"
                            .to_string(),
                    ),
                ));
            }
            PushOperation::ArchiveBlock { block_id } => {
                if moved_child_page_block_ids.contains(block_id)
                    || moved_link_preview_block_ids.contains(block_id)
                {
                    continue;
                }
                let Some(shadow_block) = shadow_blocks.get(block_id).copied() else {
                    continue;
                };
                if rendered_native_kind(shadow_block, "link_preview") {
                    validation.push(ValidationIssue::new(
                        "notion_link_preview_delete_unsupported",
                        relative_path,
                        Some(shadow_block.source_span.start_line),
                        "deleting a rendered Notion link-preview block is not supported from Markdown",
                        Some("restore the link-preview line".to_string()),
                    ));
                }
                if rendered_child_page_link_block(shadow_block) {
                    validation.push(ValidationIssue::new(
                        "notion_child_page_link_delete_unsupported",
                        relative_path,
                        Some(shadow_block.source_span.start_line),
                        "deleting a rendered Notion child-page link is not supported from parent Markdown",
                        Some(
                            "delete the child page through its projected page directory instead"
                                .to_string(),
                        ),
                    ));
                }
            }
            _ => {}
        }
    }

    validation
}

fn rendered_native_kind(block: &ShadowBlock, native_kind: &str) -> bool {
    block.native_kind.as_deref() == Some(native_kind)
}

fn notion_table_shape_change_validation(
    relative_path: &Path,
    shadow_block: &ShadowBlock,
    content: &str,
) -> Option<ValidationIssue> {
    let MarkdownBlockKind::TableWithRows {
        has_column_header, ..
    } = &shadow_block.kind
    else {
        return None;
    };
    let original = parse_markdown_table_shape(&shadow_block.text).ok()?;
    let edited = parse_markdown_table_shape(content).ok()?;

    if edited.width != original.width
        || edited
            .row_widths
            .iter()
            .any(|row_width| *row_width != original.width)
    {
        return Some(ValidationIssue::new(
            "notion_table_width_change_unsupported",
            relative_path,
            Some(shadow_block.source_span.start_line),
            "changing the width of an existing Notion table is not supported from Markdown",
            Some(
                "restore the original table column count; edit cells or append/remove rows instead"
                    .to_string(),
            ),
        ));
    }

    if !has_column_header && edited.header.iter().any(|cell| !cell.trim().is_empty()) {
        return Some(ValidationIssue::new(
            "notion_table_header_mode_change_unsupported",
            relative_path,
            Some(shadow_block.source_span.start_line),
            "changing the header mode of an existing Notion table is not supported from Markdown",
            Some("keep the rendered blank header row, or change table header settings directly in Notion".to_string()),
        ));
    }

    if table_non_trailing_row_delete_detected(*has_column_header, &original, &edited) {
        return Some(ValidationIssue::new(
            "notion_table_middle_row_delete_unsupported",
            relative_path,
            Some(shadow_block.source_span.start_line),
            "deleting non-trailing Notion table rows is not supported from Markdown",
            Some(
                "restore the shifted row and only remove rows from the end of the table"
                    .to_string(),
            ),
        ));
    }

    None
}

fn table_non_trailing_row_delete_detected(
    has_column_header: bool,
    original: &locality_notion::markdown_table::MarkdownTableShape,
    edited: &locality_notion::markdown_table::MarkdownTableShape,
) -> bool {
    let original_rows = table_identity_rows(has_column_header, original);
    let edited_rows = table_identity_rows(has_column_header, edited);
    if edited_rows.len() >= original_rows.len() {
        return false;
    }

    edited_rows.iter().enumerate().any(|(edited_index, row)| {
        if original_rows.get(edited_index) == Some(row) {
            return false;
        }
        let matching_original_indexes = original_rows
            .iter()
            .enumerate()
            .filter_map(|(original_index, original_row)| {
                (original_row == row).then_some(original_index)
            })
            .collect::<Vec<_>>();
        matches!(
            matching_original_indexes.as_slice(),
            [original_index] if *original_index > edited_index
        )
    })
}

fn table_identity_rows(
    has_column_header: bool,
    shape: &locality_notion::markdown_table::MarkdownTableShape,
) -> Vec<Vec<String>> {
    if has_column_header {
        let mut rows = Vec::with_capacity(shape.data_rows.len() + 1);
        rows.push(shape.header.clone());
        rows.extend(shape.data_rows.clone());
        rows
    } else {
        shape.data_rows.clone()
    }
}

fn moved_rendered_child_page_link_block<'a>(
    shadow_blocks: &BTreeMap<&RemoteId, &'a ShadowBlock>,
    archived_block_ids: &BTreeSet<RemoteId>,
    content: &str,
) -> Option<&'a ShadowBlock> {
    let (_label, href) = parse_markdown_link_exact(content.trim())?;
    let linked_notion_id = notion_id_from_href(href)?;
    shadow_blocks.values().copied().find(|block| {
        archived_block_ids.contains(&block.remote_id)
            && compact_notion_id(block.remote_id.as_str()) == linked_notion_id
            && rendered_bodies_equivalent(&block.text, content)
    })
}

fn moved_rendered_native_block<'a>(
    shadow_blocks: &BTreeMap<&RemoteId, &'a ShadowBlock>,
    archived_block_ids: &BTreeSet<RemoteId>,
    content: &str,
    native_kind: &str,
) -> Option<&'a ShadowBlock> {
    shadow_blocks.values().copied().find(|block| {
        archived_block_ids.contains(&block.remote_id)
            && rendered_native_kind(block, native_kind)
            && rendered_bodies_equivalent(&block.text, content)
    })
}

fn rendered_child_page_link_block(block: &ShadowBlock) -> bool {
    let Some((_label, href)) = parse_markdown_link_exact(block.text.trim()) else {
        return false;
    };
    notion_id_from_href(href)
        .is_some_and(|notion_id| compact_notion_id(block.remote_id.as_str()) == notion_id)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RenderedReadOnlyLink {
    label: String,
    target_id: String,
}

fn rendered_link_to_page_edit_validation(
    relative_path: &Path,
    shadow_block: &ShadowBlock,
    content: &str,
) -> Option<ValidationIssue> {
    let shadow_link = rendered_link_to_page_block_info(shadow_block)?;
    let edited_link = rendered_link_to_page_markdown_info(content.trim());
    let retargeted = edited_link.as_ref().is_some_and(|edited_link| {
        edited_link.label == shadow_link.label && edited_link.target_id != shadow_link.target_id
    });
    let (code, message, suggested_fix) = if retargeted {
        (
            "notion_link_to_page_retarget_unsupported",
            "retargeting a rendered Notion link-to-page block is not supported from Markdown",
            "restore the original Notion link target; move or delete the link block instead",
        )
    } else {
        (
            "notion_link_to_page_edit_unsupported",
            "editing a rendered Notion link-to-page block is not supported from Markdown",
            "restore the rendered link exactly; move or delete the link block instead",
        )
    };

    Some(ValidationIssue::new(
        code,
        relative_path,
        Some(shadow_block.source_span.start_line),
        message,
        Some(suggested_fix.to_string()),
    ))
}

fn moved_rendered_link_to_page_edit_validation(
    relative_path: &Path,
    shadow_blocks: &BTreeMap<&RemoteId, &ShadowBlock>,
    archived_block_ids: &BTreeSet<RemoteId>,
    content: &str,
) -> Option<ValidationIssue> {
    let (_label, href) = parse_markdown_link_exact(content.trim())?;
    let linked_notion_id = notion_id_from_href(href)?;
    let content_link = rendered_link_to_page_markdown_info(content.trim());
    let shadow_block = shadow_blocks.values().copied().find(|block| {
        archived_block_ids.contains(&block.remote_id)
            && rendered_link_to_page_block_info(block).is_some_and(|shadow_link| {
                content_link
                    .as_ref()
                    .is_some_and(|content_link| content_link.label == shadow_link.label)
                    || shadow_link.target_id == linked_notion_id
            })
    })?;
    if rendered_bodies_equivalent(&shadow_block.text, content) {
        return None;
    }

    rendered_link_to_page_edit_validation(relative_path, shadow_block, content)
}

fn rendered_link_to_page_block_info(block: &ShadowBlock) -> Option<RenderedReadOnlyLink> {
    if let Some(native_kind) = block.native_kind.as_deref()
        && native_kind != "link_to_page"
    {
        return None;
    }

    let info = rendered_link_to_page_markdown_info(block.text.trim())?;
    if compact_notion_id(block.remote_id.as_str()) == info.target_id {
        return None;
    }
    Some(info)
}

fn rendered_link_to_page_markdown_info(input: &str) -> Option<RenderedReadOnlyLink> {
    let (label, href) = parse_markdown_link_exact(input)?;
    if !matches!(label, "Linked page" | "Linked database") {
        return None;
    }
    Some(RenderedReadOnlyLink {
        label: label.to_string(),
        target_id: notion_id_from_href(href)?,
    })
}

fn notion_id_from_href(href: &str) -> Option<String> {
    let trimmed = href.trim();
    let lower = trimmed.to_ascii_lowercase();
    if !(lower.starts_with("https://www.notion.so/")
        || lower.starts_with("https://notion.so/")
        || lower.starts_with("https://app.notion.com/"))
    {
        return None;
    }

    let without_query = trimmed
        .split(['?', '#'])
        .next()
        .unwrap_or(trimmed)
        .trim_end_matches('/');
    without_query.rsplit('/').find_map(compact_notion_id_suffix)
}

fn compact_notion_id_suffix(value: &str) -> Option<String> {
    let compact = compact_notion_id(value);
    if compact.len() < 32 {
        return None;
    }

    Some(compact[compact.len() - 32..].to_string())
}

fn compact_notion_id(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .collect::<String>()
        .to_lowercase()
}

fn media_markdown_shape_matches_kind(shape: MediaMarkdownShape, kind: &str) -> bool {
    matches!(
        (shape, kind),
        (MediaMarkdownShape::Image, "image")
            | (MediaMarkdownShape::Link, "video" | "file" | "pdf" | "audio")
    )
}

fn is_file_like_media_kind(kind: &str) -> bool {
    matches!(kind, "image" | "video" | "file" | "pdf" | "audio")
}

fn remote_preconditions_for_plan<S>(
    store: &S,
    mount: &MountConfig,
    prepared_entity: &EntityRecord,
    pipeline: &PushPipelineResult,
) -> LocalityResult<Vec<RemotePrecondition>>
where
    S: EntityRepository,
{
    let remote_ids = pipeline
        .plan
        .as_ref()
        .map(|plan| plan.affected_entities.iter().collect::<BTreeSet<_>>())
        .unwrap_or_else(|| BTreeSet::from([&prepared_entity.remote_id]));
    let mut preconditions = Vec::with_capacity(remote_ids.len());

    for remote_id in remote_ids {
        let entity = if remote_id == &prepared_entity.remote_id {
            Some(prepared_entity.clone())
        } else {
            store
                .get_entity(&mount.mount_id, remote_id)
                .map_err(LocalityError::from)?
        };
        if let Some(entity) = entity {
            preconditions.push(RemotePrecondition {
                remote_id: remote_id.clone(),
                remote_edited_at: entity.synced_tree_remote_version().map(str::to_string),
            });
        }
    }

    Ok(preconditions)
}

#[derive(Clone, Debug)]
pub struct PreparedPush {
    pub absolute_path: PathBuf,
    pub mount: MountConfig,
    pub entity: EntityRecord,
    pub shadows: Vec<ShadowDocument>,
    pub pipeline: PushPipelineResult,
    pub readable_diff: Option<locality_core::readable_diff::ReadableDiffOutput>,
}

fn readable_diff_for_existing_entity(
    relative_path: &Path,
    shadow: &ShadowDocument,
    local_text: &str,
    pipeline: &PushPipelineResult,
) -> Option<locality_core::readable_diff::ReadableDiffOutput> {
    let plan = pipeline.plan.as_ref()?;
    if plan.operations.is_empty() {
        return None;
    }
    let old = render_canonical_markdown(&CanonicalDocument::new(
        shadow.frontmatter.clone(),
        shadow.rendered_body.clone(),
    ));
    locality_core::readable_diff::readable_diff_for_file(
        locality_platform::logical_path_display(relative_path),
        Some(&old),
        Some(local_text),
    )
}

fn readable_diff_for_created_entity(
    source_path: &Path,
    local_text: &str,
    pipeline: &PushPipelineResult,
) -> Option<locality_core::readable_diff::ReadableDiffOutput> {
    let plan = pipeline.plan.as_ref()?;
    if plan.operations.is_empty() {
        return None;
    }
    locality_core::readable_diff::readable_diff_for_file(
        locality_platform::logical_path_display(source_path),
        None,
        Some(local_text),
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushPrepareError {
    MountNotFound(PathBuf),
    ReadFile { path: PathBuf, message: String },
    Store(StoreError),
    Core(LocalityError),
}

impl From<StoreError> for PushPrepareError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

impl From<LocalityError> for PushPrepareError {
    fn from(value: LocalityError) -> Self {
        Self::Core(value)
    }
}

impl From<PushPrepareError> for LocalityError {
    fn from(value: PushPrepareError) -> Self {
        match value {
            PushPrepareError::MountNotFound(path) => {
                Self::InvalidState(format!("no mount contains `{}`", path.display()))
            }
            PushPrepareError::ReadFile { path, message } => {
                Self::Io(format!("failed to read `{}`: {message}", path.display()))
            }
            PushPrepareError::Store(error) => error.into(),
            PushPrepareError::Core(error) => error,
        }
    }
}

pub fn prepare_push<S, Validator>(
    store: &S,
    job: &PushJob,
    state_root: Option<&Path>,
    validator: &Validator,
) -> Result<PreparedPush, PushPrepareError>
where
    S: MountRepository + EntityRepository + ShadowRepository + VirtualMutationRepository,
    Validator: SourcePushValidator + ?Sized,
{
    let mut absolute_path = absolute_path(&job.target_path)?;
    let mounts = store.load_mounts().map_err(PushPrepareError::Store)?;
    let mount = find_mount_for_path(&mounts, &absolute_path)
        .cloned()
        .ok_or_else(|| PushPrepareError::MountNotFound(absolute_path.clone()))?;
    let mut relative_path = relative_entity_path(&mount, &absolute_path)?;
    if let Some(pending) = store
        .find_virtual_mutation_by_path(&mount.mount_id, &relative_path)
        .map_err(PushPrepareError::Store)?
    {
        if pending.mutation_kind != VirtualMutationKind::Create
            || projection_state::redundant_pending_create_entity(
                store, state_root, &mount, &pending,
            )
            .map_err(PushPrepareError::Core)?
            .is_none()
        {
            match pending.mutation_kind {
                VirtualMutationKind::Create => {
                    return prepare_pending_create(
                        store,
                        job,
                        state_root,
                        absolute_path,
                        mount,
                        pending,
                        validator,
                    );
                }
                VirtualMutationKind::Delete
                    if projection_state::stale_pending_delete_target_present(&mount, &pending) => {}
                VirtualMutationKind::Delete => {
                    return prepare_pending_scope(
                        store,
                        job,
                        state_root,
                        absolute_path,
                        mount,
                        relative_path,
                        validator,
                    );
                }
                VirtualMutationKind::Move => {
                    return prepare_pending_scope(
                        store,
                        job,
                        state_root,
                        absolute_path,
                        mount,
                        relative_path,
                        validator,
                    );
                }
                VirtualMutationKind::Rename => {}
            }
        }
    }
    if absolute_path.is_dir()
        && scope_has_virtual_mutations(store, &mount.mount_id, &relative_path)?
    {
        return prepare_pending_scope(
            store,
            job,
            state_root,
            absolute_path,
            mount,
            relative_path,
            validator,
        );
    }
    if absolute_path.is_dir() {
        let schema_path = absolute_path.join("_schema.yaml");
        if schema_path.is_file() {
            absolute_path = schema_path;
            relative_path = relative_path.join("_schema.yaml");
        }
    }
    let mut entity = store
        .find_entity_by_path(&mount.mount_id, &relative_path)
        .map_err(PushPrepareError::Store)?;
    if entity.is_none() && absolute_path.is_dir() {
        let page_relative_path = page_document_path(&relative_path);
        if let Some(page_entity) = store
            .find_entity_by_path(&mount.mount_id, &page_relative_path)
            .map_err(PushPrepareError::Store)?
        {
            absolute_path = page_document_path(&absolute_path);
            relative_path = page_relative_path;
            entity = Some(page_entity);
        }
    }

    let Some(entity) = entity else {
        if relative_path.as_os_str().is_empty() || absolute_path.is_dir() {
            return prepare_pending_scope(
                store,
                job,
                state_root,
                absolute_path,
                mount,
                relative_path,
                validator,
            );
        }
        if is_database_draft_schema_path(&relative_path) {
            if let Some(database_dir) = relative_path.parent()
                && let Some(existing_entity) =
                    existing_entity_for_database_dir(store, &mount.mount_id, database_dir)?
            {
                let (code, message, remediation) = if existing_entity.kind == EntityKind::Database {
                    (
                        "notion_database_schema_read_only",
                        "schemas for existing Notion databases are generated and read-only",
                        Some(
                            "run `loc create database` under an existing page to create a new database"
                                .to_string(),
                        ),
                    )
                } else {
                    (
                        "notion_database_draft_path_conflict",
                        "the database draft directory already belongs to another tracked entity",
                        Some("choose a new database title or directory name".to_string()),
                    )
                };
                return Ok(PreparedPush {
                    absolute_path,
                    mount,
                    entity: existing_entity,
                    shadows: Vec::new(),
                    pipeline: validation_pipeline(ValidationIssue::new(
                        code,
                        &relative_path,
                        Some(1),
                        message,
                        remediation,
                    )),
                    readable_diff: None,
                });
            }
            let parent = required_database_parent_entity(store, &mount, &relative_path)?;
            return prepare_database_create(job, absolute_path, mount, relative_path, parent);
        }
        return prepare_direct_create(
            store,
            job,
            state_root,
            absolute_path,
            mount,
            relative_path,
            validator,
        );
    };

    let read_path = projection_read_path(state_root, &mount, &relative_path, &absolute_path)?;
    let contents = read_to_string(&read_path)?;
    let planned = plan_existing_document(
        store,
        job,
        state_root,
        &mount,
        &entity,
        &relative_path,
        &contents,
        validator,
    )?;

    Ok(PreparedPush {
        absolute_path,
        mount,
        entity,
        shadows: planned.shadow.into_iter().collect(),
        pipeline: planned.pipeline,
        readable_diff: planned.readable_diff,
    })
}

struct ExistingDocumentPreparation {
    shadow: Option<ShadowDocument>,
    pipeline: PushPipelineResult,
    readable_diff: Option<locality_core::readable_diff::ReadableDiffOutput>,
}

#[allow(clippy::too_many_arguments)]
fn plan_existing_document<S, Validator>(
    store: &S,
    job: &PushJob,
    state_root: Option<&Path>,
    mount: &MountConfig,
    entity: &EntityRecord,
    relative_path: &Path,
    contents: &str,
    validator: &Validator,
) -> Result<ExistingDocumentPreparation, PushPrepareError>
where
    S: EntityRepository + ShadowRepository,
    Validator: SourcePushValidator + ?Sized,
{
    if let Some(line) = unresolved_conflict_marker_line(&contents) {
        return Ok(ExistingDocumentPreparation {
            shadow: None,
            pipeline: validation_pipeline(unresolved_conflict_marker_issue(&relative_path, line)),
            readable_diff: None,
        });
    }

    let parsed = match parse_canonical_markdown(&contents) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Ok(ExistingDocumentPreparation {
                shadow: None,
                pipeline: validation_pipeline(parse_error_issue(&relative_path, error)),
                readable_diff: None,
            });
        }
    };

    if parsed
        .remote_id()
        .is_some_and(|remote_id| remote_id != &entity.remote_id)
    {
        return Ok(ExistingDocumentPreparation {
            shadow: None,
            pipeline: validation_pipeline(ValidationIssue::new(
                "frontmatter_remote_id_mismatch",
                relative_path,
                Some(1),
                "frontmatter `loc.id` does not match the entity mapped to this path",
                Some("restore the generated `loc.id` for this file before pushing".to_string()),
            )),
            readable_diff: None,
        });
    }

    let shadow = store
        .load_shadow(&mount.mount_id, &entity.remote_id)
        .map_err(PushPrepareError::Store)?;
    let parent = parent_entity(store, &mount, &relative_path)?;
    let schema_validation = validator.validate_changed_frontmatter(SourceValidationContext {
        state_root,
        mount: &mount,
        parent: parent.as_ref(),
        relative_path: &relative_path,
        parsed: &parsed,
        shadow: Some(&shadow),
    })?;
    let mut pipeline = if schema_validation.is_clean() {
        plan_push_pipeline(
            PushPipelineRequest::new(&relative_path, &parsed, &shadow)
                .with_body_diff_mode(source_descriptor(&mount.connector).body_diff_mode())
                .with_approval(PushApproval {
                    assume_yes: job.assume_yes,
                    confirm_dangerous: job.confirm_dangerous,
                })
                .read_only(mount.read_only),
        )
    } else {
        validation_report_pipeline(schema_validation)
    };
    augment_notion_media_plan(
        &mount,
        state_root,
        &relative_path,
        &parsed,
        &shadow,
        PushApproval {
            assume_yes: job.assume_yes,
            confirm_dangerous: job.confirm_dangerous,
        },
        &mut pipeline,
    );
    validate_notion_pre_apply_semantics(&mount, &relative_path, &shadow, &mut pipeline);
    validate_google_docs_pre_apply_semantics(&mount, &relative_path, &shadow, &mut pipeline);
    let readable_diff =
        readable_diff_for_existing_entity(&relative_path, &shadow, &contents, &pipeline);

    Ok(ExistingDocumentPreparation {
        shadow: Some(shadow),
        pipeline,
        readable_diff,
    })
}

fn is_database_draft_schema_path(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("_schema.yaml")
}

fn existing_entity_for_database_dir<S>(
    store: &S,
    mount_id: &MountId,
    database_dir: &Path,
) -> Result<Option<EntityRecord>, PushPrepareError>
where
    S: EntityRepository,
{
    if let Some(entity) = store
        .find_entity_by_path(mount_id, database_dir)
        .map_err(PushPrepareError::Store)?
    {
        return Ok(Some(entity));
    }
    store
        .find_entity_by_path(mount_id, &page_document_path(database_dir))
        .map_err(PushPrepareError::Store)
}

fn required_database_parent_entity<S>(
    store: &S,
    mount: &MountConfig,
    schema_path: &Path,
) -> Result<EntityRecord, PushPrepareError>
where
    S: EntityRepository,
{
    let database_dir = schema_path.parent().ok_or_else(|| {
        PushPrepareError::Core(LocalityError::InvalidState(
            "database draft schema must be inside a database directory".to_string(),
        ))
    })?;
    let parent_container = database_dir.parent().unwrap_or_else(|| Path::new(""));
    let parent = if parent_container.as_os_str().is_empty() {
        let descriptor = source_descriptor(&mount.connector);
        mount.remote_root_id.as_ref().and_then(|remote_id| {
            descriptor.source_root_create_parent_kind().map(|kind| {
                EntityRecord::new(
                    mount.mount_id.clone(),
                    remote_id.clone(),
                    kind,
                    descriptor.display_name(),
                    PathBuf::new(),
                )
            })
        })
    } else {
        store
            .find_entity_by_path(&mount.mount_id, &page_document_path(parent_container))
            .map_err(PushPrepareError::Store)?
            .or(store
                .find_entity_by_path(&mount.mount_id, parent_container)
                .map_err(PushPrepareError::Store)?)
    };
    let Some(parent) = parent else {
        return Err(PushPrepareError::Store(StoreError::EntityPathMissing {
            mount_id: mount.mount_id.clone(),
            path: parent_container.to_path_buf(),
        }));
    };
    if parent.kind != EntityKind::Page {
        return Err(PushPrepareError::Core(LocalityError::InvalidState(
            format!(
                "Notion databases must be created inside an existing page directory; `{}` is {:?}",
                parent_container.display(),
                parent.kind
            ),
        )));
    }
    Ok(parent)
}

fn prepare_database_create(
    job: &PushJob,
    absolute_path: PathBuf,
    mount: MountConfig,
    relative_path: PathBuf,
    parent: EntityRecord,
) -> Result<PreparedPush, PushPrepareError> {
    let schema = read_to_string(&absolute_path)?;
    let draft = match parse_database_draft(&schema) {
        Ok(draft) => draft,
        Err(error) => {
            return Ok(PreparedPush {
                absolute_path,
                mount,
                entity: parent,
                shadows: Vec::new(),
                pipeline: validation_pipeline(ValidationIssue::new(
                    error.code,
                    &relative_path,
                    Some(1),
                    error.message,
                    Some(
                        "edit the untracked `_schema.yaml` draft and rerun `loc diff`".to_string(),
                    ),
                )),
                readable_diff: None,
            });
        }
    };
    if mount.connector != "notion" {
        return Ok(PreparedPush {
            absolute_path,
            mount,
            entity: parent,
            shadows: Vec::new(),
            pipeline: validation_pipeline(ValidationIssue::new(
                "database_create_connector_unsupported",
                &relative_path,
                Some(1),
                "database drafts are currently supported only for Notion mounts",
                None,
            )),
            readable_diff: None,
        });
    }
    if mount.read_only {
        return Ok(PreparedPush {
            absolute_path,
            mount,
            entity: parent,
            shadows: Vec::new(),
            pipeline: PushPipelineResult {
                validation: ValidationReport::clean(),
                plan: None,
                guardrail: GuardrailDecision::Proceed,
                action: PushPipelineAction::ReadOnlyBlocked,
                completed_stages: Vec::new(),
            },
            readable_diff: None,
        });
    }

    let plan = PushPlan::new(
        vec![parent.remote_id.clone()],
        vec![PushOperation::CreateDatabase {
            parent_id: parent.remote_id.clone(),
            title: draft.title,
            schema: schema.clone(),
            source_path: relative_path.clone(),
        }],
    );
    let guardrail =
        locality_core::push::evaluate_guardrails(&plan, &GuardrailPolicy::default(), None);
    let action = match &guardrail {
        GuardrailDecision::Proceed if job.assume_yes => PushPipelineAction::ProceedToApply,
        GuardrailDecision::Proceed => PushPipelineAction::ConfirmPlan,
        GuardrailDecision::ConfirmRequired { .. } if job.confirm_dangerous => {
            PushPipelineAction::ProceedToApply
        }
        GuardrailDecision::ConfirmRequired { .. } => PushPipelineAction::ConfirmDangerousPlan,
    };
    let pipeline = PushPipelineResult {
        validation: ValidationReport::clean(),
        plan: Some(plan),
        guardrail,
        action,
        completed_stages: vec![
            PushStage::ParseAndValidate,
            PushStage::Diff,
            PushStage::PlanAndConfirm,
        ],
    };
    let readable_diff = locality_core::readable_diff::readable_diff_for_file(
        locality_platform::logical_path_display(&relative_path),
        None,
        Some(&schema),
    );
    Ok(PreparedPush {
        absolute_path,
        mount,
        entity: parent,
        shadows: Vec::new(),
        pipeline,
        readable_diff,
    })
}

fn prepare_direct_create<S, Validator>(
    store: &S,
    job: &PushJob,
    state_root: Option<&Path>,
    absolute_path: PathBuf,
    mount: MountConfig,
    relative_path: PathBuf,
    validator: &Validator,
) -> Result<PreparedPush, PushPrepareError>
where
    S: EntityRepository,
    Validator: SourcePushValidator + ?Sized,
{
    let contents = read_to_string(&absolute_path)?;
    let parsed = parse_canonical_markdown(&contents);
    let private_create = parsed.as_ref().is_ok_and(private_create_requested);
    let parent = if private_create {
        workspace_create_parent(&mount)
    } else {
        required_parent_entity(store, &mount, &relative_path)?
    };
    if let Some(line) = unresolved_conflict_marker_line(&contents) {
        return Ok(PreparedPush {
            absolute_path,
            mount,
            entity: parent,
            shadows: Vec::new(),
            pipeline: validation_pipeline(unresolved_conflict_marker_issue(&relative_path, line)),
            readable_diff: None,
        });
    }
    let parsed = match parsed {
        Ok(parsed) => parsed,
        Err(error) => {
            return Ok(PreparedPush {
                absolute_path,
                mount,
                entity: parent,
                shadows: Vec::new(),
                pipeline: validation_pipeline(parse_error_issue(&relative_path, error)),
                readable_diff: None,
            });
        }
    };
    let schema_validation = validator.validate_create_frontmatter(SourceValidationContext {
        state_root,
        mount: &mount,
        parent: if private_create { None } else { Some(&parent) },
        relative_path: &relative_path,
        parsed: &parsed,
        shadow: None,
    })?;
    let pipeline = create_entity_pipeline(
        &relative_path,
        &parsed,
        &parent,
        &mount,
        PushApproval {
            assume_yes: job.assume_yes,
            confirm_dangerous: job.confirm_dangerous,
        },
        schema_validation,
    );
    let local_text = render_canonical_markdown(&parsed.document);
    let readable_diff = readable_diff_for_created_entity(&relative_path, &local_text, &pipeline);
    Ok(PreparedPush {
        absolute_path,
        mount,
        entity: parent,
        shadows: Vec::new(),
        pipeline,
        readable_diff,
    })
}

fn repair_missing_database_schema_for_target<S, Source>(
    store: &S,
    source: &Source,
    target_path: &Path,
    state_root: Option<&Path>,
) -> LocalityResult<()>
where
    S: MountRepository + EntityRepository,
    Source: HydrationSource + ?Sized,
{
    let absolute_path = absolute_path(target_path).map_err(LocalityError::from)?;
    let mounts = store.load_mounts().map_err(LocalityError::from)?;
    let Some(mount) = find_mount_for_path(&mounts, &absolute_path).cloned() else {
        return Ok(());
    };
    if mount.connector != "notion" {
        return Ok(());
    }
    let mut relative_path =
        relative_entity_path(&mount, &absolute_path).map_err(LocalityError::from)?;
    let mut entity = store
        .find_entity_by_path(&mount.mount_id, &relative_path)
        .map_err(LocalityError::from)?;
    if entity.is_none() && absolute_path.is_dir() {
        let page_relative_path = page_document_path(&relative_path);
        if let Some(page_entity) = store
            .find_entity_by_path(&mount.mount_id, &page_relative_path)
            .map_err(LocalityError::from)?
        {
            relative_path = page_relative_path;
            entity = Some(page_entity);
        }
    }
    let database = match entity.filter(|entity| entity.kind == EntityKind::Page) {
        Some(_) => database_parent_for_existing_page_path(store, &mount, &relative_path)?,
        None => database_parent_for_create_path(store, &mount, &relative_path)?,
    };
    let Some(database) = database else {
        return Ok(());
    };
    let output_root = if mount.projection.uses_virtual_filesystem() {
        let Some(state_root) = state_root else {
            return Ok(());
        };
        repair_legacy_macos_content_root(state_root, &mount.mount_id)?;
        virtual_fs_content_root(state_root, &mount.mount_id)
    } else {
        mount.root.clone()
    };
    if output_root
        .join(&database.path)
        .join("_schema.yaml")
        .exists()
    {
        return Ok(());
    }
    if let Some(schema) = source.fetch_database_schema_yaml(&database.remote_id)? {
        write_atomic(
            &output_root.join(&database.path).join("_schema.yaml"),
            schema,
        )?;
    }
    Ok(())
}

fn database_parent_for_existing_page_path<S>(
    store: &S,
    mount: &MountConfig,
    relative_path: &Path,
) -> LocalityResult<Option<EntityRecord>>
where
    S: EntityRepository,
{
    let parent_path = page_container_path(relative_path)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    Ok(store
        .find_entity_by_path(&mount.mount_id, &parent_path)
        .map_err(LocalityError::from)?
        .filter(|entity| entity.kind == EntityKind::Database))
}

fn database_parent_for_create_path<S>(
    store: &S,
    mount: &MountConfig,
    relative_path: &Path,
) -> LocalityResult<Option<EntityRecord>>
where
    S: EntityRepository,
{
    let Some(parent_path) = relative_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    else {
        return Ok(None);
    };

    if let Some(parent) = store
        .find_entity_by_path(&mount.mount_id, parent_path)
        .map_err(LocalityError::from)?
        .filter(|entity| entity.kind == EntityKind::Database)
    {
        return Ok(Some(parent));
    }

    if is_page_document_path(relative_path)
        && let Some(container_path) = parent_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
    {
        return Ok(store
            .find_entity_by_path(&mount.mount_id, container_path)
            .map_err(LocalityError::from)?
            .filter(|entity| entity.kind == EntityKind::Database));
    }

    Ok(None)
}

fn prepare_pending_create<S, Validator>(
    store: &S,
    job: &PushJob,
    state_root: Option<&Path>,
    absolute_path: PathBuf,
    mount: MountConfig,
    pending: VirtualMutationRecord,
    validator: &Validator,
) -> Result<PreparedPush, PushPrepareError>
where
    S: EntityRepository + ShadowRepository + VirtualMutationRepository,
    Validator: SourcePushValidator + ?Sized,
{
    if pending.mutation_kind != VirtualMutationKind::Create {
        return prepare_pending_scope(
            store,
            job,
            state_root,
            absolute_path,
            mount,
            pending.projected_path.clone(),
            validator,
        );
    }
    let read_path = pending_create_read_path(&mount, &pending, state_root, &absolute_path)?;
    if is_database_draft_schema_path(&pending.projected_path) {
        let parent = pending_create_parent_for_private_marker(store, &mount, &pending, false)?;
        return prepare_database_create(job, read_path, mount, pending.projected_path, parent);
    }
    let contents = read_to_string(&read_path)?;
    let parsed = match parse_canonical_markdown(&contents) {
        Ok(parsed) => parsed,
        Err(error) => match pending_create_projected_parse_fallback(&read_path, &absolute_path) {
            Some(parsed) => {
                let parent = pending_create_parent_for_private_marker(
                    store,
                    &mount,
                    &pending,
                    private_create_requested(&parsed),
                )?;
                return prepare_pending_create_from_parsed(
                    job,
                    state_root,
                    absolute_path,
                    mount,
                    pending,
                    parent,
                    parsed,
                    validator,
                );
            }
            None => {
                let parent =
                    pending_create_parent_for_private_marker(store, &mount, &pending, false)?;
                return Ok(PreparedPush {
                    absolute_path,
                    mount,
                    entity: parent,
                    shadows: Vec::new(),
                    pipeline: validation_pipeline(parse_error_issue(
                        &pending.projected_path,
                        error,
                    )),
                    readable_diff: None,
                });
            }
        },
    };
    let parent = pending_create_parent_for_private_marker(
        store,
        &mount,
        &pending,
        private_create_requested(&parsed),
    )?;
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
            readable_diff: None,
        });
    }
    prepare_pending_create_from_parsed(
        job,
        state_root,
        absolute_path,
        mount,
        pending,
        parent,
        parsed,
        validator,
    )
}

fn pending_create_parent_for_private_marker<S>(
    store: &S,
    mount: &MountConfig,
    pending: &VirtualMutationRecord,
    private_create: bool,
) -> Result<EntityRecord, PushPrepareError>
where
    S: EntityRepository,
{
    if private_create {
        return Ok(workspace_create_parent(mount));
    }

    let parent_id = pending.parent_remote_id.clone().ok_or_else(|| {
        PushPrepareError::Core(LocalityError::InvalidState(format!(
            "pending create `{}` is missing a parent remote id",
            pending.local_id
        )))
    })?;
    pending_create_parent_entity(store, mount, &parent_id)
}

fn pending_create_parent_entity<S>(
    store: &S,
    mount: &MountConfig,
    parent_id: &RemoteId,
) -> Result<EntityRecord, PushPrepareError>
where
    S: EntityRepository,
{
    if is_unsupported_notion_workspace_root_parent(mount, parent_id) {
        return Err(PushPrepareError::Core(LocalityError::Unsupported(
            "Notion pages cannot be moved to the workspace root through the Notion move API",
        )));
    }

    if let Some(parent) = store
        .get_entity(&mount.mount_id, parent_id)
        .map_err(PushPrepareError::Store)?
    {
        return Ok(parent);
    }

    if mount.remote_root_id.as_ref() == Some(parent_id) {
        let descriptor = source_descriptor(&mount.connector);
        if let Some(kind) = descriptor.source_root_create_parent_kind() {
            return Ok(EntityRecord::new(
                mount.mount_id.clone(),
                parent_id.clone(),
                kind,
                descriptor.display_name(),
                PathBuf::new(),
            ));
        }
    }

    Err(PushPrepareError::Store(StoreError::EntityMissing {
        mount_id: mount.mount_id.clone(),
        remote_id: parent_id.clone(),
    }))
}

fn is_unsupported_notion_workspace_root_parent(mount: &MountConfig, parent_id: &RemoteId) -> bool {
    mount.connector == "notion"
        && mount.remote_root_id.is_none()
        && parent_id == &workspace_parent_id(mount)
}

fn move_parent_entity_for_mutation<S>(
    store: &S,
    mount: &MountConfig,
    mutation: &VirtualMutationRecord,
) -> Result<EntityRecord, PushPrepareError>
where
    S: EntityRepository,
{
    if let Some(parent_id) = mutation.parent_remote_id.as_ref() {
        return pending_create_parent_entity(store, mount, parent_id);
    }
    required_parent_entity(store, mount, &mutation.projected_path)
}

#[allow(clippy::too_many_arguments)]
fn prepare_pending_create_from_parsed<Validator>(
    job: &PushJob,
    state_root: Option<&Path>,
    absolute_path: PathBuf,
    mount: MountConfig,
    pending: VirtualMutationRecord,
    parent: EntityRecord,
    parsed: locality_core::canonical::ParsedCanonicalDocument,
    validator: &Validator,
) -> Result<PreparedPush, PushPrepareError>
where
    Validator: SourcePushValidator + ?Sized,
{
    let private_create = private_create_requested(&parsed);
    let parent = if private_create {
        workspace_create_parent(&mount)
    } else {
        parent
    };
    let schema_validation = validator.validate_create_frontmatter(SourceValidationContext {
        state_root,
        mount: &mount,
        parent: if private_create { None } else { Some(&parent) },
        relative_path: &pending.projected_path,
        parsed: &parsed,
        shadow: None,
    })?;
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
    let local_text = render_canonical_markdown(&parsed.document);
    let readable_diff =
        readable_diff_for_created_entity(&pending.projected_path, &local_text, &pipeline);
    Ok(PreparedPush {
        absolute_path,
        mount,
        entity: parent,
        shadows: Vec::new(),
        pipeline,
        readable_diff,
    })
}

fn pending_create_projected_parse_fallback(
    read_path: &Path,
    absolute_path: &Path,
) -> Option<locality_core::canonical::ParsedCanonicalDocument> {
    if same_path_text(read_path, absolute_path) {
        return None;
    }
    let contents = std::fs::read_to_string(absolute_path).ok()?;
    if unresolved_conflict_marker_line(&contents).is_some() {
        return None;
    }
    parse_canonical_markdown(&contents).ok()
}

fn same_path_text(left: &Path, right: &Path) -> bool {
    let left = left.to_string_lossy();
    let right = right.to_string_lossy();
    if cfg!(windows) {
        left.eq_ignore_ascii_case(&right)
    } else {
        left == right
    }
}

fn pending_create_read_path(
    mount: &MountConfig,
    pending: &VirtualMutationRecord,
    state_root: Option<&Path>,
    absolute_path: &Path,
) -> Result<PathBuf, PushPrepareError> {
    if mount.projection.uses_virtual_filesystem()
        && let Some(state_root) = state_root
    {
        repair_legacy_macos_content_root(state_root, &mount.mount_id)
            .map_err(PushPrepareError::Core)?;
    }
    let content_path = virtual_mutation_content_path_for_read(state_root, &mount.mount_id, pending)
        .map_err(PushPrepareError::Core)?;

    if mount.projection.uses_virtual_filesystem()
        && absolute_path.is_file()
        && (state_root.is_none() || content_path.as_ref().is_some_and(|path| !path.is_file()))
    {
        return Ok(absolute_path.to_path_buf());
    }

    content_path.ok_or_else(|| {
        PushPrepareError::Core(LocalityError::InvalidState(format!(
            "pending create `{}` has no cached content path",
            pending.local_id
        )))
    })
}

fn scope_has_virtual_mutations<S>(
    store: &S,
    mount_id: &MountId,
    relative_scope: &Path,
) -> Result<bool, PushPrepareError>
where
    S: VirtualMutationRepository,
{
    Ok(store
        .list_virtual_mutations(mount_id)
        .map_err(PushPrepareError::Store)?
        .iter()
        .any(|mutation| {
            relative_scope.as_os_str().is_empty()
                || mutation.projected_path.starts_with(relative_scope)
        }))
}

fn prepare_pending_scope<S, Validator>(
    store: &S,
    job: &PushJob,
    state_root: Option<&Path>,
    absolute_path: PathBuf,
    mount: MountConfig,
    relative_scope: PathBuf,
    validator: &Validator,
) -> Result<PreparedPush, PushPrepareError>
where
    S: EntityRepository + ShadowRepository + VirtualMutationRepository,
    Validator: SourcePushValidator + ?Sized,
{
    let mutations = store
        .list_virtual_mutations(&mount.mount_id)
        .map_err(PushPrepareError::Store)?
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
    let mut degradations = Vec::new();
    let mut shadows = BTreeMap::new();
    let mut readable_diffs = Vec::new();
    let mut validation = ValidationReport::clean();
    let mut seen_targets = BTreeSet::new();
    let mut representative = None;
    for mutation in &mutations {
        if let Some(remote_id) = mutation.target_remote_id.as_ref()
            && !seen_targets.insert(remote_id.clone())
        {
            validation.push(ValidationIssue::new(
                "duplicate_virtual_mutation_target",
                mutation.projected_path.clone(),
                None,
                format!(
                    "multiple pending virtual filesystem mutations target entity `{}`",
                    remote_id.0
                ),
                Some("resolve the duplicate pending move/delete before pushing".to_string()),
            ));
            continue;
        }

        match mutation.mutation_kind {
            VirtualMutationKind::Delete => {
                let Some(remote_id) = mutation.target_remote_id.clone() else {
                    continue;
                };
                append_source_write_validation(
                    &mut validation,
                    &mount,
                    mutation
                        .original_path
                        .as_deref()
                        .unwrap_or(&mutation.projected_path),
                );
                let entity = store
                    .get_entity(&mount.mount_id, &remote_id)
                    .map_err(PushPrepareError::Store)?
                    .ok_or_else(|| StoreError::EntityMissing {
                        mount_id: mount.mount_id.clone(),
                        remote_id: remote_id.clone(),
                    })
                    .map_err(PushPrepareError::Store)?;
                if representative.is_none() {
                    representative = Some(entity.clone());
                }
                if let Ok(shadow) = store.load_shadow(&mount.mount_id, &remote_id) {
                    let old = render_canonical_markdown(&CanonicalDocument::new(
                        shadow.frontmatter.clone(),
                        shadow.rendered_body.clone(),
                    ));
                    if let Some(diff) = locality_core::readable_diff::readable_diff_for_file(
                        locality_platform::logical_path_display(
                            mutation
                                .original_path
                                .as_deref()
                                .unwrap_or(&mutation.projected_path),
                        ),
                        Some(&old),
                        None,
                    ) {
                        readable_diffs.push(diff);
                    }
                    shadows.insert(remote_id.clone(), shadow);
                }
                operations.push(PushOperation::ArchiveEntity {
                    entity_id: remote_id.clone(),
                });
                affected.push(remote_id);
            }
            VirtualMutationKind::Create => {
                append_source_create_validation(&mut validation, &mount, mutation);
                let pending_path = mount.root.join(&mutation.projected_path);
                let prepared = prepare_pending_create(
                    store,
                    job,
                    state_root,
                    pending_path,
                    mount.clone(),
                    mutation.clone(),
                    validator,
                )?;
                if representative.is_none() {
                    representative = Some(prepared.entity.clone());
                }
                validation.extend(prepared.pipeline.validation);
                if let Some(plan) = prepared.pipeline.plan {
                    for remote_id in plan.affected_entities {
                        push_unique_remote_id(&mut affected, remote_id);
                    }
                    operations.extend(plan.operations);
                    degradations.extend(plan.degradations);
                }
                for shadow in prepared.shadows {
                    shadows.insert(shadow.entity_id.clone(), shadow);
                }
                if let Some(diff) = prepared.readable_diff {
                    readable_diffs.push(diff);
                }
            }
            VirtualMutationKind::Move | VirtualMutationKind::Rename => {
                let remote_id = mutation.target_remote_id.clone().ok_or_else(|| {
                    PushPrepareError::Core(LocalityError::InvalidState(format!(
                        "pending rename `{}` is missing a target remote id",
                        mutation.local_id
                    )))
                })?;
                let entity = store
                    .get_entity(&mount.mount_id, &remote_id)
                    .map_err(PushPrepareError::Store)?
                    .ok_or_else(|| StoreError::EntityMissing {
                        mount_id: mount.mount_id.clone(),
                        remote_id: remote_id.clone(),
                    })
                    .map_err(PushPrepareError::Store)?;
                if representative.is_none() {
                    representative = Some(entity.clone());
                }
                append_source_write_validation(
                    &mut validation,
                    &mount,
                    mutation.original_path.as_deref().unwrap_or(&entity.path),
                );
                append_source_write_validation(&mut validation, &mount, &mutation.projected_path);
                let parent = move_parent_entity_for_mutation(store, &mount, mutation)?;
                let mut new_title = mutation.title.clone();
                let mut remaining_operations = Vec::new();
                if let Some(contents) = pending_move_contents(&mount, mutation, state_root)? {
                    let planned = plan_existing_document(
                        store,
                        job,
                        state_root,
                        &mount,
                        &entity,
                        &mutation.projected_path,
                        &contents,
                        validator,
                    )?;
                    validation.extend(planned.pipeline.validation);
                    if let Some(plan) = planned.pipeline.plan {
                        degradations.extend(plan.degradations);
                        lower_move_document_operations(
                            &mutation.projected_path,
                            &remote_id,
                            plan.operations,
                            &mut new_title,
                            &mut remaining_operations,
                            &mut validation,
                        );
                    }
                    if let Some(shadow) = planned.shadow {
                        shadows.insert(remote_id.clone(), shadow);
                    }
                    if let Some(diff) = planned.readable_diff {
                        readable_diffs.push(diff);
                    }
                } else {
                    let shadow = store
                        .load_shadow(&mount.mount_id, &remote_id)
                        .map_err(|error| match error {
                            StoreError::ShadowMissing { .. } => PushPrepareError::Core(
                                LocalityError::InvalidState(format!(
                                    "pending move `{}` must be materialized before it can be pushed",
                                    mutation.local_id
                                )),
                            ),
                            error => PushPrepareError::Store(error),
                        })?;
                    shadows.insert(remote_id.clone(), shadow);
                }
                operations.push(PushOperation::MoveEntity {
                    entity_id: remote_id.clone(),
                    new_parent_id: parent.remote_id,
                    new_parent_kind: parent.kind,
                    new_title,
                    projected_path: mutation.projected_path.clone(),
                });
                operations.extend(remaining_operations);
                push_unique_remote_id(&mut affected, remote_id);
            }
        }
    }
    let entity = representative.ok_or_else(|| {
        PushPrepareError::Core(LocalityError::InvalidState(format!(
            "no pushable pending virtual filesystem mutations under `{}`",
            first.projected_path.display()
        )))
    })?;
    let readable_diff = locality_core::readable_diff::join_readable_diffs(readable_diffs);
    if mount.read_only {
        return Ok(PreparedPush {
            absolute_path,
            mount,
            entity,
            shadows: shadows.into_values().collect(),
            pipeline: PushPipelineResult {
                validation,
                plan: None,
                guardrail: GuardrailDecision::Proceed,
                action: PushPipelineAction::ReadOnlyBlocked,
                completed_stages: Vec::new(),
            },
            readable_diff,
        });
    }
    if !validation.is_clean() {
        return Ok(PreparedPush {
            absolute_path,
            mount,
            entity,
            shadows: shadows.into_values().collect(),
            pipeline: validation_report_pipeline(validation),
            readable_diff,
        });
    }

    let plan = PushPlan::new(affected, operations).with_degradations(degradations);
    let guardrail =
        locality_core::push::evaluate_guardrails(&plan, &GuardrailPolicy::default(), None);
    let action = if plan.operations.is_empty() {
        PushPipelineAction::Noop
    } else {
        match &guardrail {
            GuardrailDecision::Proceed if job.assume_yes => PushPipelineAction::ProceedToApply,
            GuardrailDecision::Proceed => PushPipelineAction::ConfirmPlan,
            GuardrailDecision::ConfirmRequired { .. } if job.confirm_dangerous => {
                PushPipelineAction::ProceedToApply
            }
            GuardrailDecision::ConfirmRequired { .. } => PushPipelineAction::ConfirmDangerousPlan,
        }
    };
    Ok(PreparedPush {
        absolute_path,
        mount,
        entity,
        shadows: shadows.into_values().collect(),
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
        readable_diff,
    })
}

fn push_unique_remote_id(affected: &mut Vec<RemoteId>, remote_id: RemoteId) {
    if !affected.contains(&remote_id) {
        affected.push(remote_id);
    }
}

fn append_source_write_validation(
    validation: &mut ValidationReport,
    mount: &MountConfig,
    path: &Path,
) {
    if let crate::source::SourceWriteDecision::ReadOnly { reason } =
        source_write_decision_for_path(mount, path)
    {
        validation.push(ValidationIssue::new(
            "source_path_read_only",
            path,
            None,
            reason,
            Some("remove the stale pending mutation or choose a writable source path".to_string()),
        ));
    }
}

fn append_source_create_validation(
    validation: &mut ValidationReport,
    mount: &MountConfig,
    mutation: &VirtualMutationRecord,
) {
    let parent = mutation
        .projected_path
        .parent()
        .unwrap_or_else(|| Path::new(""));
    if let crate::source::SourceWriteDecision::ReadOnly { reason } =
        source_create_decision_for_parent_path(mount, parent)
    {
        validation.push(ValidationIssue::new(
            "source_parent_read_only",
            parent,
            None,
            reason,
            Some("remove the stale pending create or choose a writable parent".to_string()),
        ));
    }
}

fn pending_move_contents(
    mount: &MountConfig,
    mutation: &VirtualMutationRecord,
    state_root: Option<&Path>,
) -> Result<Option<String>, PushPrepareError> {
    let mut candidates = Vec::new();
    if let Some(path) = mutation.content_path.clone() {
        candidates.push(path);
    }
    if let Some(state_root) = state_root {
        candidates.push(
            virtual_fs_content_root(state_root, &mount.mount_id).join(&mutation.projected_path),
        );
    }
    candidates.push(mount.root.join(&mutation.projected_path));
    candidates.dedup();

    for path in candidates {
        if path.is_file() {
            return read_to_string(&path).map(Some);
        }
    }
    Ok(None)
}

fn lower_move_document_operations(
    path: &Path,
    remote_id: &RemoteId,
    document_operations: Vec<PushOperation>,
    new_title: &mut String,
    remaining: &mut Vec<PushOperation>,
    validation: &mut ValidationReport,
) {
    for operation in document_operations {
        match operation {
            PushOperation::UpdateProperties {
                entity_id,
                mut properties,
            } if entity_id == *remote_id => {
                if let Some(title) = properties.remove("title") {
                    match title {
                        PropertyValue::String(title) if !title.trim().is_empty() => {
                            *new_title = title
                        }
                        PropertyValue::String(_) => validation.push(ValidationIssue::new(
                            "move_title_invalid",
                            path,
                            None,
                            "a moved entity title cannot be blank",
                            Some("set a non-empty string title before pushing".to_string()),
                        )),
                        PropertyValue::Null => validation.push(ValidationIssue::new(
                            "move_title_invalid",
                            path,
                            None,
                            "a moved entity title cannot be removed",
                            Some("set a non-empty string title before pushing".to_string()),
                        )),
                        _ => validation.push(ValidationIssue::new(
                            "move_title_invalid",
                            path,
                            None,
                            "a moved entity title must be a string",
                            Some("set a string title before pushing".to_string()),
                        )),
                    }
                }
                if !properties.is_empty() {
                    remaining.push(PushOperation::UpdateProperties {
                        entity_id,
                        properties,
                    });
                }
            }
            operation => remaining.push(operation),
        }
    }
}

fn create_entity_pipeline(
    relative_path: &Path,
    parsed: &locality_core::canonical::ParsedCanonicalDocument,
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
    let private_create = private_create_requested(parsed);
    if private_create && mount.connector != "notion" {
        validation.push(ValidationIssue::new(
            "create_entity_private_unsupported",
            relative_path,
            Some(1),
            "`loc.private: true` is only supported for Notion page creation",
            Some("remove `loc.private`, or create this page inside a Notion mount".to_string()),
        ));
    }
    if parsed.remote_id().is_some() {
        validation.push(ValidationIssue::new(
            "create_entity_has_remote_id",
            relative_path,
            Some(1),
            "new files must not carry an existing `loc.id`",
            Some(
                "remove the generated `loc.id`, or pull the existing page before editing"
                    .to_string(),
            ),
        ));
    }
    if parsed
        .frontmatter
        .loc
        .as_ref()
        .and_then(|loc| loc.entity_type.as_ref())
        .is_some_and(|kind| kind != &EntityKind::Page)
    {
        validation.push(ValidationIssue::new(
            "create_entity_type_not_page",
            relative_path,
            Some(1),
            "new files require `loc.type: page` when an `loc` block is present",
            Some("remove the `loc` block or set `loc.type` to `page`".to_string()),
        ));
    }
    if create_entity_title_required(parsed, mount) {
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
            "new files cannot use the generated Locality stub marker as their body",
            Some("replace the stub marker with the page body, or leave the body empty".to_string()),
        ));
    }
    for directive in &parsed.directives {
        validation.push(ValidationIssue::new(
            "create_entity_directive_unsupported",
            relative_path,
            Some(directive.line),
            "new page creation does not support pre-seeded Locality directive blocks",
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
    let affected_entities = if private_create {
        Vec::new()
    } else {
        vec![parent.remote_id.clone()]
    };
    let parent_id = if private_create {
        workspace_parent_id(mount)
    } else {
        parent.remote_id.clone()
    };
    let parent_kind = if private_create {
        None
    } else {
        Some(parent.kind.clone())
    };
    let title = create_entity_title(relative_path, parsed, mount);
    let plan = PushPlan::new(
        affected_entities,
        vec![PushOperation::CreateEntity {
            parent_id,
            parent_kind,
            parent_workspace: private_create,
            title,
            properties,
            body: parsed.document.body.clone(),
            source_path: relative_path.to_path_buf(),
        }],
    );
    completed_stages.push(PushStage::Diff);
    let guardrail =
        locality_core::push::evaluate_guardrails(&plan, &GuardrailPolicy::default(), None);
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

fn create_entity_title_required(
    parsed: &locality_core::canonical::ParsedCanonicalDocument,
    mount: &MountConfig,
) -> bool {
    if matches!(mount.connector.as_str(), "gmail" | "google-calendar") {
        return false;
    }
    parsed
        .frontmatter
        .title
        .as_ref()
        .is_none_or(|title| title.trim().is_empty())
}

fn create_entity_title(
    relative_path: &Path,
    parsed: &locality_core::canonical::ParsedCanonicalDocument,
    mount: &MountConfig,
) -> String {
    if let Some(title) = parsed.frontmatter.title.as_ref()
        && !title.trim().is_empty()
    {
        return title.clone();
    }
    if mount.connector == "gmail"
        && let Some(subject) = frontmatter_string(&parsed.frontmatter.properties, "subject")
        && !subject.trim().is_empty()
    {
        return subject.trim().to_string();
    }
    if mount.connector == "google-calendar"
        && let Some(summary) = frontmatter_string(&parsed.frontmatter.properties, "summary")
        && !summary.trim().is_empty()
    {
        return summary.trim().to_string();
    }
    relative_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
        .to_string()
}

fn frontmatter_string(
    properties: &locality_core::canonical::FrontmatterProperties,
    key: &str,
) -> Option<String> {
    properties
        .get(key)
        .map(property_value_from_frontmatter)
        .and_then(|value| match value {
            PropertyValue::String(value) => Some(value),
            _ => None,
        })
}

fn private_create_requested(parsed: &locality_core::canonical::ParsedCanonicalDocument) -> bool {
    parsed
        .frontmatter
        .loc
        .as_ref()
        .is_some_and(|metadata| metadata.private)
}

fn workspace_create_parent(mount: &MountConfig) -> EntityRecord {
    EntityRecord::new(
        mount.mount_id.clone(),
        workspace_parent_id(mount),
        EntityKind::Directory,
        "Workspace",
        PathBuf::new(),
    )
}

fn workspace_parent_id(mount: &MountConfig) -> RemoteId {
    mount
        .remote_root_id
        .clone()
        .unwrap_or_else(|| RemoteId::new("workspace"))
}

fn projection_read_path(
    state_root: Option<&Path>,
    mount: &MountConfig,
    relative_path: &Path,
    absolute_path: &Path,
) -> LocalityResult<PathBuf> {
    if mount.projection.uses_virtual_filesystem()
        && let Some(state_root) = state_root
    {
        repair_legacy_macos_content_root(state_root, &mount.mount_id)?;
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
    fn append(&mut self, entry: JournalEntry) -> LocalityResult<()> {
        self.store.append(entry)
    }

    fn record_apply_effects(
        &mut self,
        push_id: &PushId,
        effects: Vec<JournalApplyEffect>,
    ) -> LocalityResult<()> {
        self.store.record_apply_effects(push_id, effects)
    }

    fn update_status(&mut self, push_id: &PushId, status: JournalStatus) -> LocalityResult<()> {
        self.store.update_status(push_id, status)
    }
}

impl<S, Source> PushConcurrencyCheck for DaemonPushHost<'_, S, Source>
where
    S: MountRepository + EntityRepository + ShadowRepository,
    Source: Connector + HydrationSource + ?Sized,
{
    fn check(&mut self, request: PushConcurrencyRequest<'_>) -> LocalityResult<()> {
        self.source.check_concurrency(ApplyPlanRequest {
            push_id: request.push_id,
            mount_id: request.mount_id,
            plan: request.plan,
            operation_ids: request.operation_ids,
            remote_preconditions: request.remote_preconditions,
            local_root: None,
        })?;
        self.check_remote_tree_content(request)
    }
}

impl<S, Source> DaemonPushHost<'_, S, Source>
where
    S: MountRepository + EntityRepository + ShadowRepository,
    Source: Connector + HydrationSource + ?Sized,
{
    fn check_remote_tree_content(
        &mut self,
        request: PushConcurrencyRequest<'_>,
    ) -> LocalityResult<()> {
        let mount = self
            .store
            .get_mount(request.mount_id)
            .map_err(LocalityError::from)?
            .ok_or_else(|| StoreError::MountMissing(request.mount_id.clone()))
            .map_err(LocalityError::from)?;

        for precondition in request.remote_preconditions {
            let Some(entity) = self
                .store
                .get_entity(request.mount_id, &precondition.remote_id)
                .map_err(LocalityError::from)?
            else {
                continue;
            };
            let synced_tree_shadow = match self
                .store
                .load_shadow(request.mount_id, &precondition.remote_id)
            {
                Ok(shadow) => shadow,
                Err(StoreError::ShadowMissing { .. }) => continue,
                Err(error) => return Err(error.into()),
            };
            let remote_tree_render = self.source.fetch_render_with_repository(
                &locality_core::hydration::HydrationRequest::new(
                    request.mount_id.clone(),
                    precondition.remote_id.clone(),
                    entity.path.clone(),
                    HydrationState::Hydrated,
                    locality_core::hydration::HydrationReason::ExplicitPull,
                ),
                &*self.store,
            )?;

            if remote_tree_matches_synced_tree(&synced_tree_shadow, &remote_tree_render.shadow) {
                continue;
            }
            if mount.connector == LINEAR_CONNECTOR_ID
                && linear_shadow_matches_with_legacy_lifecycle_frontmatter(
                    &synced_tree_shadow,
                    &remote_tree_render.shadow,
                )
            {
                continue;
            }
            return Err(LocalityError::Guardrail(format!(
                "remote entity `{}` changed since the Synced Tree shadow; inspect or pull before pushing local edits",
                precondition.remote_id.0
            )));
        }

        Ok(())
    }
}

fn remote_tree_matches_synced_tree(
    synced_tree_shadow: &ShadowDocument,
    remote_tree_shadow: &ShadowDocument,
) -> bool {
    shadows_match(synced_tree_shadow, remote_tree_shadow)
}

impl<S, Source> PushApplier for DaemonPushHost<'_, S, Source>
where
    S: MountRepository,
    Source: Connector + ?Sized,
{
    fn apply(&mut self, request: PushApplyRequest<'_>) -> LocalityResult<PushApplyResult> {
        let mount = self
            .store
            .get_mount(request.mount_id)
            .map_err(LocalityError::from)?
            .ok_or_else(|| {
                LocalityError::InvalidState(format!("missing mount `{}`", request.mount_id.0))
            })?;
        let local_root = projection_output_root(self.state_root.as_deref(), &mount)?;
        let result: ApplyPlanResult = self.source.apply(ApplyPlanRequest {
            push_id: request.push_id,
            mount_id: request.mount_id,
            plan: request.plan,
            operation_ids: request.operation_ids,
            remote_preconditions: request.remote_preconditions,
            local_root: Some(local_root.as_path()),
        })?;

        Ok(PushApplyResult {
            changed_remote_ids: result.changed_remote_ids,
            effects: result.effects,
        })
    }
}

impl<S, Source> PushReconciler for DaemonPushHost<'_, S, Source>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + RemoteObservationRepository
        + FreshnessStateRepository
        + VirtualMutationRepository,
    Source: Connector + HydrationSource + ?Sized,
{
    fn reconcile(
        &mut self,
        request: PushReconcileRequest<'_>,
    ) -> LocalityResult<PushReconcileResult> {
        let mount = self
            .store
            .get_mount(request.mount_id)
            .map_err(LocalityError::from)?
            .ok_or_else(|| StoreError::MountMissing(request.mount_id.clone()))
            .map_err(LocalityError::from)?;
        let planned_moves = request
            .plan
            .operations
            .iter()
            .enumerate()
            .filter_map(|(operation_index, operation)| match operation {
                PushOperation::MoveEntity {
                    entity_id,
                    new_parent_id,
                    ..
                } => Some((operation_index, entity_id, new_parent_id)),
                _ => None,
            })
            .collect::<Vec<_>>();
        for (operation_index, entity_id, new_parent_id) in planned_moves.iter().copied() {
            if !request.changed_remote_ids.contains(entity_id) {
                return Err(LocalityError::InvalidState(format!(
                    "move operation for `{}` did not report the entity as changed",
                    entity_id.0
                )));
            }
            let has_matching_effect = request.apply_effects.iter().any(|effect| {
                matches!(
                    effect,
                    JournalApplyEffect::MovedEntity {
                        operation_index: effect_index,
                        entity_id: effect_entity_id,
                        parent_id,
                        ..
                    } if *effect_index == operation_index
                        && effect_entity_id == entity_id
                        && parent_id == new_parent_id
                )
            });
            if !has_matching_effect {
                return Err(LocalityError::InvalidState(format!(
                    "move operation for `{}` did not report a matching moved-entity effect",
                    entity_id.0
                )));
            }
        }
        let planned_move_ids = planned_moves
            .into_iter()
            .map(|(_, entity_id, _)| entity_id.clone())
            .collect::<BTreeSet<_>>();
        let mut effect_readback_ids = BTreeSet::new();
        let mut created_readbacks = Vec::new();
        let mut database_reconciled_ids = Vec::new();
        for effect in request.apply_effects {
            match effect {
                JournalApplyEffect::CreatedEntity {
                    operation_index,
                    parent_id,
                    entity_id,
                    ..
                } => {
                    if let Some(PushOperation::CreateDatabase {
                        title, source_path, ..
                    }) = request.plan.operations.get(*operation_index)
                    {
                        let entity_path =
                            source_path.parent().map(Path::to_path_buf).ok_or_else(|| {
                                LocalityError::InvalidState(format!(
                                    "created database schema `{}` has no containing directory",
                                    source_path.display()
                                ))
                            })?;
                        let entity = EntityRecord::new(
                            request.mount_id.clone(),
                            entity_id.clone(),
                            EntityKind::Database,
                            title.clone(),
                            entity_path.clone(),
                        )
                        .with_hydration(HydrationState::Hydrated);
                        self.store
                            .save_entity(entity)
                            .map_err(LocalityError::from)?;
                        let schema = self
                            .source
                            .fetch_database_schema_yaml(entity_id)?
                            .ok_or_else(|| {
                                LocalityError::InvalidState(format!(
                                    "created database `{}` did not return a schema",
                                    entity_id.0
                                ))
                            })?;
                        let output_root =
                            projection_output_root(self.state_root.as_deref(), &mount)?;
                        write_atomic(&output_root.join(&entity_path).join("_schema.yaml"), schema)?;
                        if let Some(mutation) = self
                            .store
                            .find_virtual_mutation_by_path(request.mount_id, source_path)
                            .map_err(LocalityError::from)?
                        {
                            self.store
                                .delete_virtual_mutation(request.mount_id, &mutation.local_id)
                                .map_err(LocalityError::from)?;
                        }
                        effect_readback_ids.insert(entity_id.clone());
                        database_reconciled_ids.push(entity_id.clone());
                        continue;
                    }
                    let Some(PushOperation::CreateEntity {
                        title,
                        properties: _,
                        body: _,
                        source_path,
                        parent_kind,
                        ..
                    }) = request.plan.operations.get(*operation_index)
                    else {
                        continue;
                    };
                    let entity_path = created_entity_reconcile_path(
                        self.store,
                        request.mount_id,
                        source_path,
                        parent_kind,
                        parent_id,
                    )?;
                    let mut entity = EntityRecord::new(
                        request.mount_id.clone(),
                        entity_id.clone(),
                        EntityKind::Page,
                        title.clone(),
                        entity_path,
                    )
                    .with_hydration(HydrationState::Stub);
                    let mut rendered = self.source.fetch_render_with_repository(
                        &locality_core::hydration::HydrationRequest::new(
                            request.mount_id.clone(),
                            entity_id.clone(),
                            entity.path.clone(),
                            HydrationState::Hydrated,
                            locality_core::hydration::HydrationReason::ExplicitPull,
                        ),
                        &*self.store,
                    )?;
                    let rendered_path = created_entity_reconcile_path_from_rendered(
                        self.store,
                        request.mount_id,
                        &entity.path,
                        parent_kind,
                        parent_id,
                        entity_id,
                        &rendered,
                    )?;
                    if rendered_path != entity.path {
                        entity.path = rendered_path;
                        rendered = self.source.fetch_render_with_repository(
                            &locality_core::hydration::HydrationRequest::new(
                                request.mount_id.clone(),
                                entity_id.clone(),
                                entity.path.clone(),
                                HydrationState::Hydrated,
                                locality_core::hydration::HydrationReason::ExplicitPull,
                            ),
                            &*self.store,
                        )?;
                    } else {
                        entity.path = rendered_path;
                    }
                    effect_readback_ids.insert(entity_id.clone());
                    created_readbacks.push((*operation_index, entity_id.clone(), entity, rendered));
                }
                JournalApplyEffect::ArchivedEntity { entity_id, .. } => {
                    effect_readback_ids.insert(entity_id.clone());
                }
                _ => {}
            }
        }

        let mut readback_ids = BTreeSet::new();
        let mut entity_readbacks = Vec::new();
        for remote_id in request.changed_remote_ids {
            if effect_readback_ids.contains(remote_id) {
                continue;
            }
            if !readback_ids.insert(remote_id.clone()) {
                continue;
            }
            let mut entity = self
                .store
                .get_entity(request.mount_id, remote_id)
                .map_err(LocalityError::from)?
                .ok_or_else(|| StoreError::EntityMissing {
                    mount_id: request.mount_id.clone(),
                    remote_id: remote_id.clone(),
                })
                .map_err(LocalityError::from)?;
            if let Some(PushOperation::MoveEntity {
                new_title,
                projected_path,
                ..
            }) = request.plan.operations.iter().find(|operation| {
                matches!(
                    operation,
                    PushOperation::MoveEntity { entity_id, .. } if entity_id == remote_id
                )
            }) {
                entity.title = new_title.clone();
                entity.path = projected_path.clone();
                if mount.connector == LINEAR_CONNECTOR_ID
                    && self.source.kind().0 == LINEAR_CONNECTOR_ID
                {
                    let observed = self.source.observe(ObserveRequest {
                        mount_id: request.mount_id.clone(),
                        remote_id: remote_id.clone(),
                    })?;
                    if !observed.deleted {
                        entity.title = observed.title;
                        entity.path = observed.projected_path;
                    }
                }
            }
            let path = projection_write_path(self.state_root.as_deref(), &mount, &entity.path);
            let rendered = self.source.fetch_render_with_repository(
                &locality_core::hydration::HydrationRequest::new(
                    request.mount_id.clone(),
                    remote_id.clone(),
                    entity.path.clone(),
                    HydrationState::Hydrated,
                    locality_core::hydration::HydrationReason::ExplicitPull,
                ),
                &*self.store,
            )?;
            entity_readbacks.push((remote_id.clone(), entity, path, rendered));
        }

        let output_root = projection_output_root(self.state_root.as_deref(), &mount)?;
        let mut reconciled_remote_ids = database_reconciled_ids;
        for (_, entity_id, entity, rendered) in &mut created_readbacks {
            let path = projection_write_path(self.state_root.as_deref(), &mount, &entity.path);
            accept_post_apply_remote(
                self.store,
                &mount,
                entity,
                &path,
                &output_root,
                rendered.clone(),
            )?;
            reconciled_remote_ids.push(entity_id.clone());
        }
        for (remote_id, mut entity, path, rendered) in entity_readbacks {
            let output_root = projection_output_root(self.state_root.as_deref(), &mount)?;
            accept_post_apply_remote(
                self.store,
                &mount,
                &mut entity,
                &path,
                &output_root,
                rendered,
            )?;
            reconciled_remote_ids.push(remote_id);
        }

        for (operation_index, _, entity, _) in &created_readbacks {
            let Some(PushOperation::CreateEntity {
                title,
                properties,
                body,
                source_path,
                ..
            }) = request.plan.operations.get(*operation_index)
            else {
                continue;
            };
            let clear_source_mutation = remove_stale_created_entity_source_path(
                self.state_root.as_deref(),
                &mount,
                title,
                properties,
                body,
                source_path,
                &entity.path,
            )?;
            if clear_source_mutation
                && let Some(mutation) = self
                    .store
                    .find_virtual_mutation_by_path(request.mount_id, source_path)
                    .map_err(LocalityError::from)?
            {
                self.store
                    .delete_virtual_mutation(request.mount_id, &mutation.local_id)
                    .map_err(LocalityError::from)?;
            }
        }
        for effect in request.apply_effects {
            if let JournalApplyEffect::ArchivedEntity { entity_id, .. } = effect {
                self.store
                    .delete_entity(request.mount_id, entity_id)
                    .map_err(LocalityError::from)?;
                self.store
                    .delete_virtual_mutation(request.mount_id, &format!("delete:{}", entity_id.0))
                    .map_err(LocalityError::from)?;
                reconciled_remote_ids.push(entity_id.clone());
            }
        }
        for remote_id in request.changed_remote_ids {
            if effect_readback_ids.contains(remote_id) {
                continue;
            }
            if planned_move_ids.contains(remote_id) {
                for local_id in [
                    format!("move:{}", remote_id.0),
                    format!("rename:{}", remote_id.0),
                ] {
                    self.store
                        .delete_virtual_mutation(request.mount_id, &local_id)
                        .map_err(LocalityError::from)?;
                }
            } else {
                self.store
                    .delete_virtual_mutation(request.mount_id, &format!("rename:{}", remote_id.0))
                    .map_err(LocalityError::from)?;
            }
        }

        Ok(PushReconcileResult {
            reconciled_remote_ids,
        })
    }
}

fn created_entity_reconcile_path<S>(
    store: &S,
    mount_id: &MountId,
    source_path: &Path,
    parent_kind: &Option<EntityKind>,
    parent_id: &RemoteId,
) -> LocalityResult<PathBuf>
where
    S: EntityRepository,
{
    if matches!(parent_kind, Some(EntityKind::Database)) && !is_page_document_path(source_path) {
        return Ok(page_document_path(&page_container_path(source_path)));
    }

    if matches!(parent_kind, Some(EntityKind::Directory))
        && let Some(parent) = store
            .get_entity(mount_id, parent_id)
            .map_err(LocalityError::from)?
        && parent.kind == EntityKind::Directory
        && let Some(filename) = source_path.file_name()
    {
        return Ok(parent.path.join(filename));
    }

    Ok(source_path.to_path_buf())
}

fn created_entity_reconcile_path_from_rendered<S>(
    store: &S,
    mount_id: &MountId,
    current_path: &Path,
    parent_kind: &Option<EntityKind>,
    parent_id: &RemoteId,
    entity_id: &RemoteId,
    rendered: &HydratedEntity,
) -> LocalityResult<PathBuf>
where
    S: EntityRepository,
{
    if !matches!(parent_kind, Some(EntityKind::Directory)) {
        return Ok(current_path.to_path_buf());
    }

    let Some(parent) = store
        .get_entity(mount_id, parent_id)
        .map_err(LocalityError::from)?
    else {
        return Ok(current_path.to_path_buf());
    };
    if parent.kind != EntityKind::Directory {
        return Ok(current_path.to_path_buf());
    }

    if let Some(filename) = gmail_rendered_message_filename(entity_id, rendered) {
        return Ok(parent.path.join(filename));
    }
    if let Some(filename) = google_calendar_rendered_event_filename(entity_id, rendered) {
        return Ok(parent.path.join(filename));
    }

    Ok(current_path.to_path_buf())
}

fn gmail_rendered_message_filename(
    entity_id: &RemoteId,
    rendered: &HydratedEntity,
) -> Option<String> {
    let rendered_markdown = render_canonical_markdown(&rendered.document);
    let parsed = parse_canonical_markdown(&rendered_markdown).ok()?;
    let frontmatter_remote_edited_at = parsed
        .frontmatter
        .loc
        .as_ref()
        .and_then(|loc| loc.remote_edited_at.as_deref());
    let remote_edited_at = rendered
        .remote_edited_at
        .as_deref()
        .or(frontmatter_remote_edited_at)?;
    let internal_date = gmail_internal_date(entity_id, remote_edited_at)?;
    let subject = frontmatter_string(&parsed.frontmatter.properties, "subject");
    let title = parsed
        .frontmatter
        .title
        .as_deref()
        .or(subject.as_deref())
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or("(no subject)");

    Some(format!(
        "{}-{}-{}.md",
        created_entity_safe_slug(internal_date),
        created_entity_safe_slug(title),
        created_entity_safe_slug(entity_id.as_str())
    ))
}

fn gmail_internal_date<'a>(entity_id: &RemoteId, remote_version: &'a str) -> Option<&'a str> {
    let rest = remote_version.strip_prefix("gmail:")?;
    let rest = rest.strip_prefix(entity_id.as_str())?;
    let rest = rest.strip_prefix(':')?;
    let internal_date = rest.split(':').next().unwrap_or("unknown");
    Some(if internal_date.is_empty() {
        "unknown"
    } else {
        internal_date
    })
}

fn google_calendar_rendered_event_filename(
    entity_id: &RemoteId,
    rendered: &HydratedEntity,
) -> Option<String> {
    let rendered_markdown = render_canonical_markdown(&rendered.document);
    let parsed = parse_canonical_markdown(&rendered_markdown).ok()?;
    let event_id = google_calendar_frontmatter_event_id(&parsed.frontmatter.properties)
        .or_else(|| google_calendar_event_id_from_remote_id(entity_id))?;
    let start = google_calendar_start_value(&parsed.frontmatter.properties)?;
    let summary = frontmatter_string(&parsed.frontmatter.properties, "summary");
    let title = parsed
        .frontmatter
        .title
        .as_deref()
        .or(summary.as_deref())
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or("untitled");

    Some(format!(
        "{}-{}-{}.md",
        google_calendar_compact_start_prefix(&start),
        created_entity_safe_slug(title),
        created_entity_safe_slug(&event_id)
    ))
}

fn google_calendar_frontmatter_event_id(
    properties: &locality_core::canonical::FrontmatterProperties,
) -> Option<String> {
    let PropertyValue::Object(google_calendar) = properties
        .get("google_calendar")
        .map(property_value_from_frontmatter)?
    else {
        return None;
    };
    match google_calendar.get("event_id") {
        Some(PropertyValue::String(value)) if !value.trim().is_empty() => {
            Some(value.trim().to_string())
        }
        _ => None,
    }
}

fn google_calendar_start_value(
    properties: &locality_core::canonical::FrontmatterProperties,
) -> Option<String> {
    let PropertyValue::Object(start) = properties
        .get("start")
        .map(property_value_from_frontmatter)?
    else {
        return None;
    };
    match start.get("dateTime").or_else(|| start.get("date")) {
        Some(PropertyValue::String(value)) if !value.trim().is_empty() => {
            Some(value.trim().to_string())
        }
        _ => None,
    }
}

fn google_calendar_event_id_from_remote_id(entity_id: &RemoteId) -> Option<String> {
    entity_id
        .as_str()
        .strip_prefix("google-calendar-event:")
        .and_then(|rest| rest.split_once(':'))
        .and_then(|(_, event_id)| (!event_id.is_empty()).then(|| event_id.to_string()))
}

fn google_calendar_compact_start_prefix(sort_start_key: &str) -> String {
    let mut digits = sort_start_key
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .take(14)
        .collect::<String>();
    while digits.len() < 14 {
        digits.push('0');
    }
    format!("{}-{}", &digits[..8], &digits[8..14])
}

fn created_entity_safe_slug(value: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "untitled".to_string()
    } else {
        slug.to_string()
    }
}

fn remove_stale_created_entity_source_path(
    state_root: Option<&Path>,
    mount: &MountConfig,
    title: &str,
    properties: &BTreeMap<String, PropertyValue>,
    body: &str,
    source_path: &Path,
    entity_path: &Path,
) -> LocalityResult<bool> {
    if source_path == entity_path {
        return Ok(true);
    }

    let stale_path = projection_write_path(state_root, mount, source_path);
    let canonical_path = projection_write_path(state_root, mount, entity_path);
    if stale_path == canonical_path {
        return Ok(true);
    }

    // Draft creates may have been edited after apply succeeded but before
    // reconciliation finished. Preserve edited drafts as new pending creates.
    if preserves_edited_created_entity_source(mount)
        && !created_entity_source_matches_plan(
            mount,
            source_path,
            &stale_path,
            title,
            properties,
            body,
        )
    {
        return Ok(false);
    }

    match std::fs::remove_file(&stale_path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
    }
}

fn preserves_edited_created_entity_source(mount: &MountConfig) -> bool {
    matches!(mount.connector.as_str(), "gmail" | "google-calendar")
}

fn created_entity_source_matches_plan(
    mount: &MountConfig,
    source_path: &Path,
    stale_path: &Path,
    title: &str,
    properties: &BTreeMap<String, PropertyValue>,
    body: &str,
) -> bool {
    let Ok(contents) = std::fs::read_to_string(stale_path) else {
        return true;
    };
    let Ok(parsed) = parse_canonical_markdown(&contents) else {
        return false;
    };
    let parsed_properties = parsed
        .frontmatter
        .properties
        .iter()
        .map(|(key, value)| (key.clone(), property_value_from_frontmatter(value)))
        .collect::<BTreeMap<_, _>>();

    create_entity_title(source_path, &parsed, mount) == title
        && parsed_properties == *properties
        && parsed.document.body == body
}

fn accept_post_apply_remote<S>(
    store: &mut S,
    mount: &MountConfig,
    entity: &mut EntityRecord,
    path: &Path,
    output_root: &Path,
    rendered: HydratedEntity,
) -> LocalityResult<()>
where
    S: EntityRepository + ShadowRepository + RemoteObservationRepository + FreshnessStateRepository,
{
    for asset in &rendered.assets {
        let path = mount_relative_path(output_root, &asset.path)?;
        write_binary_atomic(&path, &asset.bytes)?;
    }
    replace_hydrated_media_manifest(output_root, &rendered.assets)?;
    write_atomic(
        path,
        render_document_with_absolute_media_hrefs(&rendered.document, &entity.path, output_root),
    )?;
    store
        .save_shadow(&mount.mount_id, rendered.shadow.clone())
        .map_err(LocalityError::from)?;

    entity.hydration = HydrationState::Hydrated;
    entity.content_hash = Some(rendered.shadow.body_hash.clone());
    let remote_edited_at = rendered.remote_edited_at.clone();
    if let Some(remote_edited_at) = remote_edited_at.clone() {
        entity.set_synced_tree_remote_version(Some(remote_edited_at));
    }
    store
        .save_entity(entity.clone())
        .map_err(LocalityError::from)?;
    record_post_apply_remote_tree_observation(store, mount, entity, remote_edited_at)?;
    clear_remote_hint(store, &mount.mount_id, &entity.remote_id)?;
    Ok(())
}

fn record_post_apply_remote_tree_observation<S>(
    store: &mut S,
    mount: &MountConfig,
    entity: &EntityRecord,
    remote_edited_at: Option<String>,
) -> LocalityResult<()>
where
    S: RemoteObservationRepository,
{
    let mut observation = RemoteObservationRecord::new(
        mount.mount_id.clone(),
        entity.remote_id.clone(),
        entity.kind.clone(),
        entity.title.clone(),
        entity.path.clone(),
        push_timestamp(),
    );
    if let Some(remote_edited_at) = remote_edited_at {
        observation = observation.with_remote_version(RemoteVersion::new(remote_edited_at));
    }

    store
        .save_remote_observation(observation)
        .map_err(LocalityError::from)
}

fn clear_remote_hint<S>(
    store: &mut S,
    mount_id: &MountId,
    remote_id: &RemoteId,
) -> LocalityResult<()>
where
    S: FreshnessStateRepository,
{
    if let Some(mut freshness) = store
        .get_freshness_state(mount_id, remote_id)
        .map_err(LocalityError::from)?
    {
        freshness.remote_hint_pending = false;
        freshness.last_checked_at = Some(push_timestamp());
        store
            .save_freshness_state(freshness)
            .map_err(LocalityError::from)?;
    }
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
            locality_core::push::PushExecutionAction::NotReady { .. } => Self::NotReady,
            locality_core::push::PushExecutionAction::Reconciled => Self::Reconciled,
        }
    }
}

impl From<LocalityError> for PushJobError {
    fn from(value: LocalityError) -> Self {
        Self {
            code: locality_error_code(&value).to_string(),
            message: value.to_string(),
        }
    }
}

fn locality_error_code(error: &LocalityError) -> &'static str {
    match error {
        LocalityError::Validation(_) => "validation_failed",
        LocalityError::Conflict(_) => "conflict",
        LocalityError::Guardrail(_) => "guardrail",
        LocalityError::RemoteNotFound(_) => "remote_not_found",
        LocalityError::RateLimited { .. } => "rate_limited",
        LocalityError::InvalidState(_) => "invalid_state",
        LocalityError::UpdateRequired { .. } => "update_required",
        LocalityError::Unsupported(_) => "unsupported",
        LocalityError::NotImplemented(_) => "not_implemented",
        LocalityError::Io(_) => "io_error",
    }
}

#[cfg(test)]
mod locality_error_code_tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use locality_core::LocalityError;
    use locality_core::model::RemoteId;
    use locality_core::planner::{PropertyValue, PushOperation};
    use locality_core::validation::ValidationReport;

    use super::{locality_error_code, lower_move_document_operations};

    #[test]
    fn update_required_has_stable_push_error_code() {
        assert_eq!(
            locality_error_code(&LocalityError::UpdateRequired {
                component: "linear:discovery".to_string(),
                found: 2,
                supported: 1,
            }),
            "update_required"
        );
    }

    #[test]
    fn move_lowering_rejects_null_non_string_and_blank_title_values() {
        for (value, message) in [
            (
                PropertyValue::Null,
                "a moved entity title cannot be removed",
            ),
            (
                PropertyValue::Bool(true),
                "a moved entity title must be a string",
            ),
            (
                PropertyValue::String("   \t".to_string()),
                "a moved entity title cannot be blank",
            ),
        ] {
            let remote_id = RemoteId::new("issue-1");
            let mut title = "Canonical title".to_string();
            let mut remaining = Vec::new();
            let mut validation = ValidationReport::clean();

            lower_move_document_operations(
                Path::new("Team/ENG-1/page.md"),
                &remote_id,
                vec![PushOperation::UpdateProperties {
                    entity_id: remote_id.clone(),
                    properties: BTreeMap::from([("title".to_string(), value)]),
                }],
                &mut title,
                &mut remaining,
                &mut validation,
            );

            assert_eq!(title, "Canonical title");
            assert!(remaining.is_empty());
            assert_eq!(validation.issues.len(), 1);
            assert_eq!(validation.issues[0].code, "move_title_invalid");
            assert_eq!(validation.issues[0].message, message);
        }
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
            "edit the file to choose the intended content, remove every conflict marker line, then rerun `loc diff` or `loc push`"
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

fn parent_entity<S>(
    store: &S,
    mount: &MountConfig,
    relative_path: &Path,
) -> Result<Option<EntityRecord>, PushPrepareError>
where
    S: EntityRepository,
{
    let Some(parent_path) = relative_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    else {
        return Ok(None);
    };

    let parent = store
        .find_entity_by_path(&mount.mount_id, parent_path)
        .map_err(PushPrepareError::Store)?;
    if parent.is_some() {
        return Ok(parent);
    }

    if is_page_document_path(relative_path)
        && let Some(container_path) = parent_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
    {
        if let Some(parent) = store
            .find_entity_by_path(&mount.mount_id, container_path)
            .map_err(PushPrepareError::Store)?
        {
            return Ok(Some(parent));
        }
        let parent_page_path = page_document_path(container_path);
        if parent_page_path != relative_path {
            return store
                .find_entity_by_path(&mount.mount_id, &parent_page_path)
                .map_err(PushPrepareError::Store);
        }
    }

    Ok(None)
}

fn required_parent_entity<S>(
    store: &S,
    mount: &MountConfig,
    relative_path: &Path,
) -> Result<EntityRecord, PushPrepareError>
where
    S: EntityRepository,
{
    if let Some(parent) = parent_entity(store, mount, relative_path)? {
        return Ok(parent);
    }

    if let Some(parent) = source_root_create_parent_entity(mount, relative_path) {
        return Ok(parent);
    }

    if let Some(error) = direct_source_root_create_diagnostic(mount, relative_path) {
        return Err(error);
    }

    Err(PushPrepareError::Store(StoreError::EntityPathMissing {
        mount_id: mount.mount_id.clone(),
        path: relative_path.to_path_buf(),
    }))
}

/// Explains why a direct (non-private) create at the mount root has no parent
/// to attach to, instead of surfacing the generic "path was not found in
/// mount" store error. `loc create page --parent <mount-root>` happily writes
/// a local draft for any connector, but connectors such as Notion have no
/// concept of an anonymous root page: a page must either live under an
/// existing page/database or be created explicitly as a workspace ("private")
/// page. Without this, the unsupported case surfaced late, at push time, as a
/// confusing `EntityPathMissing` error that gave no indication of what to do.
fn direct_source_root_create_diagnostic(
    mount: &MountConfig,
    relative_path: &Path,
) -> Option<PushPrepareError> {
    if !is_direct_source_root_create_path(relative_path) {
        return None;
    }

    let descriptor = source_descriptor(&mount.connector);
    if descriptor.source_root_create_parent_kind().is_none() {
        let hint = if mount.connector == "notion" {
            " Pass --private to `loc create page` to create it as a workspace page instead."
        } else {
            ""
        };
        return Some(PushPrepareError::Core(LocalityError::InvalidState(
            format!(
                "`{connector}` does not support creating `{path}` directly at the mount root; create it under an existing page or database directory.{hint}",
                connector = mount.connector,
                path = relative_path.display(),
            ),
        )));
    }

    if mount.remote_root_id.is_none() {
        return Some(PushPrepareError::Core(LocalityError::InvalidState(
            format!(
                "mount `{mount_id}` has no known remote root id, so `{path}` cannot be created at the mount root; reconnect or re-mount `{connector}` to refresh mount metadata.",
                mount_id = mount.mount_id.0,
                path = relative_path.display(),
                connector = mount.connector,
            ),
        )));
    }

    None
}

fn source_root_create_parent_entity(
    mount: &MountConfig,
    relative_path: &Path,
) -> Option<EntityRecord> {
    if !is_direct_source_root_create_path(relative_path) {
        return None;
    }
    let remote_id = mount.remote_root_id.as_ref()?;
    let descriptor = source_descriptor(&mount.connector);
    let kind = descriptor.source_root_create_parent_kind()?;
    Some(EntityRecord::new(
        mount.mount_id.clone(),
        remote_id.clone(),
        kind,
        descriptor.display_name(),
        PathBuf::new(),
    ))
}

fn is_direct_source_root_create_path(relative_path: &Path) -> bool {
    if relative_path.as_os_str().is_empty() {
        return false;
    }

    if is_page_document_path(relative_path) {
        let container_path = page_container_path(relative_path);
        return container_path
            .parent()
            .is_none_or(|parent| parent.as_os_str().is_empty());
    }

    relative_path
        .parent()
        .is_none_or(|parent| parent.as_os_str().is_empty())
}

fn absolute_path(path: &Path) -> Result<PathBuf, PushPrepareError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| PushPrepareError::ReadFile {
                path: path.to_path_buf(),
                message: error.to_string(),
            })
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

fn find_mount_for_path<'a>(mounts: &'a [MountConfig], path: &Path) -> Option<&'a MountConfig> {
    file_provider::find_mount_for_path(mounts, path).map(|(mount, _)| mount)
}

fn relative_entity_path(
    mount: &MountConfig,
    absolute_path: &Path,
) -> Result<PathBuf, PushPrepareError> {
    file_provider::match_mount_path(mount, absolute_path)
        .map(|matched| matched.relative_path)
        .ok_or_else(|| PushPrepareError::MountNotFound(absolute_path.to_path_buf()))
}

fn read_to_string(path: &Path) -> Result<String, PushPrepareError> {
    std::fs::read_to_string(path).map_err(|error| PushPrepareError::ReadFile {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

fn write_atomic(path: &Path, contents: String) -> LocalityResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("loc-write");
    let temp_path = path.with_file_name(format!(".{file_name}.loc-tmp"));

    std::fs::write(&temp_path, contents)?;
    std::fs::rename(&temp_path, path)?;
    Ok(())
}

fn write_binary_atomic(path: &Path, contents: &[u8]) -> LocalityResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("loc-media");
    let temp_path = path.with_file_name(format!(".{file_name}.loc-tmp"));
    std::fs::write(&temp_path, contents)?;
    std::fs::rename(&temp_path, path)?;
    Ok(())
}

fn mount_relative_path(root: &Path, path: &Path) -> LocalityResult<PathBuf> {
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::Prefix(_)
                | std::path::Component::RootDir
                | std::path::Component::ParentDir
        )
    }) {
        return Err(LocalityError::InvalidState(format!(
            "hydrated asset path `{}` is not mount-relative",
            path.display()
        )));
    }
    Ok(root.join(path))
}

fn projection_output_root(
    state_root: Option<&Path>,
    mount: &MountConfig,
) -> LocalityResult<PathBuf> {
    if mount.projection.uses_virtual_filesystem()
        && let Some(state_root) = state_root
    {
        repair_legacy_macos_content_root(state_root, &mount.mount_id)?;
        return Ok(virtual_fs_content_root(state_root, &mount.mount_id));
    }

    Ok(mount.root.clone())
}

fn generate_push_id() -> PushId {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    PushId(format!("push-{timestamp}-{}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn schema_preflight_repairs_legacy_app_group_cache_before_remote_fetch() {
        use locality_core::hydration::HydrationRequest;
        use locality_store::{InMemoryStateStore, ProjectionMode};

        struct SchemaSource;

        impl HydrationSource for SchemaSource {
            fn fetch_render(
                &self,
                _request: &HydrationRequest,
            ) -> locality_core::LocalityResult<HydratedEntity> {
                panic!("schema preflight should not hydrate")
            }

            fn fetch_database_schema_yaml(
                &self,
                _database_id: &RemoteId,
            ) -> locality_core::LocalityResult<Option<String>> {
                Ok(Some("remote schema\n".to_string()))
            }
        }

        let home = std::env::temp_dir().join(format!(
            "loc-push-schema-legacy-home-{}",
            std::process::id()
        ));
        let state_root = home.join(".loc");
        let visible_root = home.join("visible");
        let mount_id = MountId::new("notion-main");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(
                MountConfig::new(mount_id.clone(), "notion", &visible_root)
                    .projection(ProjectionMode::LinuxFuse),
            )
            .expect("save mount");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("database-1"),
                EntityKind::Database,
                "Tasks",
                "Tasks",
            ))
            .expect("save database");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("page-1"),
                EntityKind::Page,
                "Existing task",
                "Tasks/Existing task.md",
            ))
            .expect("save page");
        let legacy_schema_path = home
            .join("Library")
            .join("Group Containers")
            .join("C484HB7Q6S.group.ai.codeflash.locality")
            .join("content")
            .join(&mount_id.0)
            .join("files")
            .join("Tasks/_schema.yaml");
        std::fs::create_dir_all(legacy_schema_path.parent().expect("legacy parent"))
            .expect("legacy parent");
        std::fs::write(&legacy_schema_path, "legacy schema\n").expect("write legacy schema");
        let current_schema_path =
            virtual_fs_content_root(&state_root, &mount_id).join("Tasks/_schema.yaml");
        assert!(!current_schema_path.exists());

        repair_missing_database_schema_for_target(
            &store,
            &SchemaSource,
            &visible_root.join("Tasks/Existing task.md"),
            Some(&state_root),
        )
        .expect("schema preflight");

        assert_eq!(
            std::fs::read_to_string(&current_schema_path).expect("read current schema"),
            "legacy schema\n"
        );

        let _ = std::fs::remove_dir_all(home);
    }
}

fn push_timestamp() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => format!("unix_ms:{}", duration.as_millis()),
        Err(_) => "unix_ms:0".to_string(),
    }
}
