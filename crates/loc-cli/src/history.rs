//! Journal-backed `loc log` and `loc undo` orchestration.
//!
//! The log surface is a read-only view over durable push journals. Undo uses the
//! journaled preimage snapshots and apply effects to derive a connector-neutral
//! reverse plan, then applies it through a connector hook when the plan is
//! complete.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use locality_core::LocalityError;
use locality_core::canonical::{parse_canonical_markdown, render_canonical_markdown};
use locality_core::freshness::RemoteObservation;
use locality_core::journal::{JournalApplyEffect, JournalEntry, JournalStatus, PushId};
use locality_core::model::{CanonicalDocument, HydrationState, MountId, RemoteId};
use locality_core::path_projection::{page_container_path, page_document_path};
use locality_core::undo::{
    EntityUndoState, UndoApplier, UndoApplyRequest, UndoOperation, UndoPlan, UndoPlanStatus,
    UnsupportedUndoOperation, plan_journal_undo,
};
use locality_store::{
    EntityRecord, EntityRepository, JournalRepository, MountConfig, MountRepository,
    ShadowRepository, StoreError, VirtualMutationRepository,
};
use localityd::contents_match_shadow;
use localityd::file_provider;
use localityd::virtual_fs::{virtual_fs_ancestor_container_identifiers, virtual_fs_content_path};
use serde::Serialize;

use crate::diff::{PlanSummaryOutput, PropertyUpdateOutput, PropertyValueOutput};
use crate::file_provider as file_provider_helper;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogOptions {
    pub path: Option<PathBuf>,
    pub push_id: Option<PushId>,
    pub include_diff: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LogReport {
    pub ok: bool,
    pub command: &'static str,
    pub entries: Vec<JournalEntryOutput>,
}

pub fn run_log<S>(store: &S, options: LogOptions) -> Result<LogReport, HistoryError>
where
    S: JournalRepository + MountRepository + EntityRepository,
{
    let filter = options
        .path
        .as_deref()
        .map(|path| resolve_path_filter(store, path))
        .transpose()?;
    let mut entries = store.list_journal().map_err(HistoryError::Store)?;

    if let Some(filter) = filter {
        entries.retain(|entry| entry_matches_filter(entry, &filter));
    }

    if let Some(push_id) = &options.push_id {
        entries.retain(|entry| &entry.push_id == push_id);
    }

    entries.sort_by(|left, right| right.push_id.0.cmp(&left.push_id.0));

    Ok(LogReport {
        ok: true,
        command: "log",
        entries: entries
            .into_iter()
            .map(|entry| JournalEntryOutput::from_entry(entry, options.include_diff))
            .collect(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UndoReport {
    pub ok: bool,
    pub command: &'static str,
    pub push_id: String,
    pub status: String,
    pub action: String,
    pub message: String,
    pub changed_remote_ids: Vec<String>,
    pub entry: Option<JournalEntryOutput>,
    pub undo_plan: Option<UndoPlanOutput>,
}

pub fn run_undo<S>(store: &mut S, push_id: impl Into<String>) -> Result<UndoReport, HistoryError>
where
    S: JournalRepository,
{
    let push_id = PushId(push_id.into());
    let entry = store
        .get_journal(&push_id)
        .map_err(HistoryError::Store)?
        .ok_or_else(|| HistoryError::JournalNotFound(push_id.clone()))?;

    match entry.status.clone() {
        JournalStatus::Prepared => {
            store
                .update_journal_status(&push_id, JournalStatus::Reverted)
                .map_err(HistoryError::Store)?;
            let mut reverted = entry;
            reverted.status = JournalStatus::Reverted;

            Ok(UndoReport {
                ok: true,
                command: "undo",
                push_id: push_id.0,
                status: "reverted".to_string(),
                action: "reverted_local_journal".to_string(),
                message: "journal entry reverted before remote apply".to_string(),
                changed_remote_ids: Vec::new(),
                entry: Some(JournalEntryOutput::from_entry(reverted, false)),
                undo_plan: None,
            })
        }
        JournalStatus::Reverted => Ok(UndoReport {
            ok: true,
            command: "undo",
            push_id: push_id.0,
            status: "reverted".to_string(),
            action: "already_reverted".to_string(),
            message: "journal entry was already reverted".to_string(),
            changed_remote_ids: Vec::new(),
            entry: Some(JournalEntryOutput::from_entry(entry, false)),
            undo_plan: None,
        }),
        JournalStatus::Failed(_) if entry.apply_effects.is_empty() => {
            store
                .update_journal_status(&push_id, JournalStatus::Reverted)
                .map_err(HistoryError::Store)?;
            let mut reverted = entry;
            reverted.status = JournalStatus::Reverted;

            Ok(UndoReport {
                ok: true,
                command: "undo",
                push_id: push_id.0,
                status: "reverted".to_string(),
                action: "reverted_empty_failed_journal".to_string(),
                message: "failed journal had no recorded remote effects and was marked reverted"
                    .to_string(),
                changed_remote_ids: Vec::new(),
                entry: Some(JournalEntryOutput::from_entry(reverted, false)),
                undo_plan: None,
            })
        }
        JournalStatus::Applied | JournalStatus::Reconciled => {
            let undo_plan = plan_journal_undo(&entry);
            let (action, message) = undo_boundary(&undo_plan);
            Ok(UndoReport {
                ok: false,
                command: "undo",
                push_id: push_id.0,
                status: status_name(&entry.status).to_string(),
                action: action.to_string(),
                message: message.to_string(),
                changed_remote_ids: Vec::new(),
                undo_plan: Some(UndoPlanOutput::from(undo_plan)),
                entry: Some(JournalEntryOutput::from_entry(entry, false)),
            })
        }
        status => Ok(UndoReport {
            ok: false,
            command: "undo",
            push_id: push_id.0,
            status: status_name(&status).to_string(),
            action: "undo_unsafe_journal_status".to_string(),
            message: undo_boundary_message(&status).to_string(),
            changed_remote_ids: Vec::new(),
            entry: Some(JournalEntryOutput::from_entry(entry, false)),
            undo_plan: None,
        }),
    }
}

pub fn run_undo_with_applier<S, A>(
    store: &mut S,
    push_id: impl Into<String>,
    applier: &mut A,
) -> Result<UndoReport, HistoryError>
where
    S: JournalRepository
        + MountRepository
        + EntityRepository
        + ShadowRepository
        + VirtualMutationRepository,
    A: UndoApplier,
{
    run_undo_with_applier_at_state_root(store, push_id, applier, None)
}

pub fn run_undo_with_applier_at_state_root<S, A>(
    store: &mut S,
    push_id: impl Into<String>,
    applier: &mut A,
    state_root: Option<&Path>,
) -> Result<UndoReport, HistoryError>
where
    S: JournalRepository
        + MountRepository
        + EntityRepository
        + ShadowRepository
        + VirtualMutationRepository,
    A: UndoApplier,
{
    let push_id = PushId(push_id.into());
    let entry = store
        .get_journal(&push_id)
        .map_err(HistoryError::Store)?
        .ok_or_else(|| HistoryError::JournalNotFound(push_id.clone()))?;

    if !matches!(
        entry.status,
        JournalStatus::Applied | JournalStatus::Reconciled
    ) {
        return run_undo(store, push_id.0);
    }

    let undo_plan = plan_journal_undo(&entry);
    if undo_plan.status != UndoPlanStatus::Complete {
        let (action, message) = undo_boundary(&undo_plan);
        return Ok(UndoReport {
            ok: false,
            command: "undo",
            push_id: push_id.0,
            status: status_name(&entry.status).to_string(),
            action: action.to_string(),
            message: message.to_string(),
            changed_remote_ids: Vec::new(),
            undo_plan: Some(UndoPlanOutput::from(undo_plan)),
            entry: Some(JournalEntryOutput::from_entry(entry, false)),
        });
    }

    ensure_undo_target_is_latest(store, &entry, &undo_plan)?;
    preflight_undo_local_state(store, &entry, &undo_plan, state_root)?;

    let apply_result = match applier.apply_undo(UndoApplyRequest {
        target_push_id: &push_id,
        mount_id: &entry.mount_id,
        plan: &undo_plan,
    }) {
        Ok(result) => result,
        Err(error) => {
            let action = match &error {
                LocalityError::NotImplemented(_) => "reverse_apply_not_implemented",
                LocalityError::RemoteNotFound(_) => "reverse_apply_remote_not_found",
                LocalityError::UpdateRequired { .. } => "update_required",
                _ => "reverse_apply_failed",
            };
            return Ok(UndoReport {
                ok: false,
                command: "undo",
                push_id: push_id.0,
                status: status_name(&entry.status).to_string(),
                action: action.to_string(),
                message: error.to_string(),
                changed_remote_ids: Vec::new(),
                undo_plan: Some(UndoPlanOutput::from(undo_plan)),
                entry: Some(JournalEntryOutput::from_entry(entry, false)),
            });
        }
    };

    ensure_complete_undo_apply_result(&entry, &undo_plan, &apply_result.changed_remote_ids)?;

    let projection_refreshes = reconcile_undo_preimages(
        store,
        &entry,
        &undo_plan,
        &apply_result.changed_remote_ids,
        &apply_result.observations,
        state_root,
    )?;

    finalize_undo_after_reconcile(
        store,
        &push_id,
        &projection_refreshes,
        apply_undo_projection_refresh,
    )?;
    let mut reverted = entry;
    reverted.status = JournalStatus::Reverted;

    Ok(UndoReport {
        ok: true,
        command: "undo",
        push_id: push_id.0,
        status: "reverted".to_string(),
        action: "reverse_applied".to_string(),
        message: "remote undo applied and journal entry marked reverted".to_string(),
        changed_remote_ids: apply_result
            .changed_remote_ids
            .into_iter()
            .map(|remote_id| remote_id.0)
            .collect(),
        undo_plan: Some(UndoPlanOutput::from(undo_plan)),
        entry: Some(JournalEntryOutput::from_entry(reverted, false)),
    })
}

#[derive(Clone, Debug)]
enum UndoProjectionRefresh {
    WindowsCloudFilesEntity {
        state_root: PathBuf,
        mount_id: MountId,
        entity_id: RemoteId,
        previous_path: PathBuf,
        previous_shadow: locality_core::shadow::ShadowDocument,
    },
    WindowsCloudFilesRemovedEntity {
        state_root: PathBuf,
        mount: MountConfig,
        entity_id: RemoteId,
        previous_path: PathBuf,
        previous_shadow: locality_core::shadow::ShadowDocument,
    },
    MacosContainers {
        mount: MountConfig,
        identifiers: Vec<String>,
    },
}

fn finalize_undo_after_reconcile<S, F>(
    store: &mut S,
    push_id: &PushId,
    refreshes: &[UndoProjectionRefresh],
    mut refresh: F,
) -> Result<(), HistoryError>
where
    S: JournalRepository,
    F: FnMut(&S, &UndoProjectionRefresh) -> Result<(), HistoryError>,
{
    store
        .update_journal_status(push_id, JournalStatus::Reverted)
        .map_err(HistoryError::Store)?;

    let mut first_error = None;
    for request in refreshes {
        if let Err(error) = refresh(store, request)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn apply_undo_projection_refresh<S>(
    store: &S,
    refresh: &UndoProjectionRefresh,
) -> Result<(), HistoryError>
where
    S: MountRepository + EntityRepository,
{
    match refresh {
        UndoProjectionRefresh::WindowsCloudFilesEntity {
            state_root,
            mount_id,
            entity_id,
            previous_path,
            previous_shadow,
        } => {
            let report = file_provider::reconcile_windows_cloud_files_entity_projection_if_clean(
                store,
                state_root,
                mount_id,
                entity_id,
                previous_path,
                previous_shadow,
            )
            .map_err(|error| HistoryError::Store(StoreError::Io(error.to_string())))?;
            if report.skipped_local_changes > 0 {
                return Err(unsafe_undo_local_state(
                    entity_id,
                    "visible provider replica changed while remote undo was applying",
                ));
            }
            Ok(())
        }
        UndoProjectionRefresh::WindowsCloudFilesRemovedEntity {
            state_root,
            mount,
            entity_id,
            previous_path,
            previous_shadow,
        } => {
            let report = file_provider::remove_windows_cloud_files_entity_projection_if_clean(
                state_root,
                mount,
                entity_id,
                previous_path,
                previous_shadow,
            )
            .map_err(|error| HistoryError::Store(StoreError::Io(error.to_string())))?;
            if report.skipped_local_changes > 0 {
                return Err(unsafe_undo_local_state(
                    &previous_shadow.entity_id,
                    "visible provider replica changed while remote undo was applying",
                ));
            }
            Ok(())
        }
        UndoProjectionRefresh::MacosContainers { mount, identifiers } => {
            signal_macos_projection_identifiers(mount, identifiers.clone())
        }
    }
}

fn ensure_undo_target_is_latest<S>(
    store: &S,
    entry: &JournalEntry,
    undo_plan: &UndoPlan,
) -> Result<(), HistoryError>
where
    S: JournalRepository,
{
    for entity_id in undo_touched_entity_ids(entry, undo_plan) {
        let latest_push_id = store
            .latest_journal_for_entities(&entry.mount_id, std::slice::from_ref(&entity_id))
            .map_err(HistoryError::Store)?;
        if latest_push_id.as_ref() != Some(&entry.push_id) {
            return Err(HistoryError::UndoNotLatest {
                push_id: entry.push_id.clone(),
                entity_id,
                latest_push_id,
            });
        }
    }
    Ok(())
}

fn undo_touched_entity_ids(entry: &JournalEntry, undo_plan: &UndoPlan) -> BTreeSet<RemoteId> {
    let mut entity_ids = entry.remote_ids.iter().cloned().collect::<BTreeSet<_>>();
    entity_ids.extend(entry.plan.affected_entities.iter().cloned());
    entity_ids.extend(
        entry
            .preimages
            .iter()
            .map(|preimage| preimage.entity_id.clone()),
    );
    entity_ids.extend(
        undo_plan
            .operations
            .iter()
            .filter_map(entity_undo_operation_id),
    );
    entity_ids
}

fn entity_undo_operation_id(operation: &UndoOperation) -> Option<RemoteId> {
    match operation {
        UndoOperation::ArchiveCreatedEntity { entity_id, .. }
        | UndoOperation::RestoreEntityBody { entity_id, .. }
        | UndoOperation::RestoreProperties { entity_id, .. }
        | UndoOperation::RestoreEntityLocation { entity_id, .. }
        | UndoOperation::RestoreArchivedEntity { entity_id, .. } => Some(entity_id.clone()),
        UndoOperation::RestoreBlockContent { .. }
        | UndoOperation::MoveBlock { .. }
        | UndoOperation::RestoreArchivedBlock { .. }
        | UndoOperation::ArchiveCreatedBlock { .. } => None,
    }
}

fn ensure_complete_undo_apply_result(
    entry: &JournalEntry,
    undo_plan: &UndoPlan,
    changed_remote_ids: &[RemoteId],
) -> Result<(), HistoryError> {
    let mut required = entry
        .preimages
        .iter()
        .map(|preimage| preimage.entity_id.clone())
        .collect::<BTreeSet<_>>();
    required.extend(
        undo_plan
            .operations
            .iter()
            .filter_map(entity_undo_operation_id),
    );
    let changed = changed_remote_ids.iter().collect::<BTreeSet<_>>();
    let missing = required
        .into_iter()
        .filter(|entity_id| !changed.contains(entity_id))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(HistoryError::IncompleteUndoApplyResult { missing })
    }
}

fn preflight_undo_local_state<S>(
    store: &S,
    entry: &JournalEntry,
    undo_plan: &UndoPlan,
    state_root: Option<&Path>,
) -> Result<(), HistoryError>
where
    S: MountRepository + EntityRepository + ShadowRepository + VirtualMutationRepository,
{
    let mut entity_ids = entry
        .preimages
        .iter()
        .map(|preimage| preimage.entity_id.clone())
        .collect::<BTreeSet<_>>();
    entity_ids.extend(
        undo_plan
            .operations
            .iter()
            .filter_map(|operation| match operation {
                UndoOperation::ArchiveCreatedEntity { entity_id, .. } => Some(entity_id.clone()),
                _ => None,
            }),
    );
    if entity_ids.is_empty() {
        return Ok(());
    }

    let mount = store
        .get_mount(&entry.mount_id)
        .map_err(HistoryError::Store)?
        .ok_or_else(|| HistoryError::MountNotFound(PathBuf::from(entry.mount_id.0.clone())))?;
    let mutations = store
        .list_virtual_mutations(&entry.mount_id)
        .map_err(HistoryError::Store)?;

    for entity_id in entity_ids {
        if mutations
            .iter()
            .any(|mutation| mutation.target_remote_id.as_ref() == Some(&entity_id))
        {
            return Err(unsafe_undo_local_state(
                &entity_id,
                "entity has a pending local filesystem mutation",
            ));
        }
        let entity = store
            .get_entity(&entry.mount_id, &entity_id)
            .map_err(HistoryError::Store)?;
        let Some(entity) = entity else {
            if undo_plan.operations.iter().any(|operation| {
                matches!(
                    operation,
                    UndoOperation::RestoreArchivedEntity {
                        entity_id: restored_id,
                        ..
                    }
                        if restored_id == &entity_id
                )
            }) {
                continue;
            }
            return Err(unsafe_undo_local_state(
                &entity_id,
                "entity is not indexed locally",
            ));
        };
        if entity.kind != locality_core::model::EntityKind::Page {
            return Err(unsafe_undo_local_state(
                &entity_id,
                "entity is not a page projection",
            ));
        }
        if entity.hydration != HydrationState::Hydrated {
            return Err(unsafe_undo_local_state(
                &entity_id,
                "entity is dirty, conflicted, or not fully hydrated",
            ));
        }
        if mutations
            .iter()
            .any(|mutation| mutation.projected_path == entity.path)
        {
            return Err(unsafe_undo_local_state(
                &entity_id,
                "entity has a pending local filesystem mutation",
            ));
        }
        let shadow = store
            .load_shadow(&entry.mount_id, &entity_id)
            .map_err(|_| unsafe_undo_local_state(&entity_id, "entity shadow is unavailable"))?;
        if entity
            .content_hash
            .as_ref()
            .is_some_and(|content_hash| content_hash != &shadow.body_hash)
        {
            return Err(unsafe_undo_local_state(
                &entity_id,
                "entity content hash does not match its synced shadow",
            ));
        }
        let (content_path, visible_paths) =
            undo_entity_projection_paths(state_root, &mount, &entity)?;
        verify_projection_matches_shadow(&entity_id, &content_path, &shadow)?;
        for visible_path in visible_paths {
            if visible_path.exists() {
                verify_projection_matches_shadow(&entity_id, &visible_path, &shadow)?;
            }
        }
    }

    Ok(())
}

fn undo_entity_projection_paths(
    state_root: Option<&Path>,
    mount: &MountConfig,
    entity: &EntityRecord,
) -> Result<(PathBuf, Vec<PathBuf>), HistoryError> {
    let content_path = undo_projection_write_path(state_root, mount, &entity.path)?;
    let mut visible_paths = if mount.projection.uses_virtual_filesystem() {
        file_provider::mount_access_roots(mount)
            .into_iter()
            .map(|root| root.join(&entity.path))
            .filter(|path| path != &content_path)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    visible_paths.sort();
    visible_paths.dedup();
    Ok((content_path, visible_paths))
}

fn verify_projection_matches_shadow(
    entity_id: &RemoteId,
    path: &Path,
    shadow: &locality_core::shadow::ShadowDocument,
) -> Result<(), HistoryError> {
    let contents = std::fs::read_to_string(path).map_err(|_| {
        unsafe_undo_local_state(
            entity_id,
            format!("projection `{}` cannot be read", path.display()),
        )
    })?;
    if !contents_match_shadow(&contents, shadow) {
        return Err(unsafe_undo_local_state(
            entity_id,
            format!(
                "projection `{}` diverges from its synced shadow",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn unsafe_undo_local_state(entity_id: &RemoteId, reason: impl Into<String>) -> HistoryError {
    HistoryError::UnsafeUndoLocalState {
        entity_id: entity_id.clone(),
        reason: reason.into(),
    }
}

fn reconcile_undo_preimages<S>(
    store: &mut S,
    entry: &JournalEntry,
    undo_plan: &UndoPlan,
    changed_remote_ids: &[RemoteId],
    observations: &[RemoteObservation],
    state_root: Option<&Path>,
) -> Result<Vec<UndoProjectionRefresh>, HistoryError>
where
    S: MountRepository + EntityRepository + ShadowRepository + VirtualMutationRepository,
{
    let mut projection_refreshes = Vec::new();
    let entity_observations = validate_entity_undo_observations(
        store,
        entry,
        undo_plan,
        changed_remote_ids,
        observations,
    )?;
    preflight_undo_local_state(store, entry, undo_plan, state_root)?;
    if changed_remote_ids.is_empty() {
        return Ok(projection_refreshes);
    }
    let mounts = store.load_mounts().map_err(HistoryError::Store)?;
    let mount = mounts
        .iter()
        .find(|mount| mount.mount_id == entry.mount_id)
        .ok_or_else(|| HistoryError::MountNotFound(PathBuf::from(entry.mount_id.0.clone())))?
        .clone();
    preflight_undo_reconciliation_paths(
        store,
        entry,
        changed_remote_ids,
        &entity_observations,
        state_root,
        &mount,
    )?;

    for preimage in entry
        .preimages
        .iter()
        .filter(|preimage| changed_remote_ids.contains(&preimage.entity_id))
    {
        let existing = store
            .get_entity(&entry.mount_id, &preimage.entity_id)
            .map_err(HistoryError::Store)?;
        let previous_path = existing.as_ref().map(|entity| entity.path.clone());
        let previous_shadow = existing
            .as_ref()
            .map(|_| store.load_shadow(&entry.mount_id, &preimage.entity_id))
            .transpose()
            .map_err(HistoryError::Store)?;
        let previous_refresh_identifiers = if mount.projection.uses_virtual_filesystem()
            && existing.is_some()
        {
            virtual_fs_ancestor_container_identifiers(store, &entry.mount_id, &preimage.entity_id)
                .map_err(|error| HistoryError::Store(StoreError::Io(error.to_string())))?
        } else {
            Vec::new()
        };
        let mut entity = match existing {
            Some(entity) => entity,
            None if undo_plan.operations.iter().any(|operation| {
                matches!(
                    operation,
                    UndoOperation::RestoreArchivedEntity { entity_id, .. }
                        if entity_id == &preimage.entity_id
                )
            }) =>
            {
                let (observation, restored_path) = entity_observations
                    .get(&preimage.entity_id)
                    .ok_or_else(|| {
                        invalid_undo_observation(
                            &preimage.entity_id,
                            "restored archived entity has no validated observation",
                        )
                    })?;
                EntityRecord::new(
                    entry.mount_id.clone(),
                    preimage.entity_id.clone(),
                    observation.kind.clone(),
                    observation.title.clone(),
                    restored_path.clone(),
                )
            }
            None => continue,
        };
        if let Some((observation, restored_path)) = entity_observations.get(&preimage.entity_id) {
            entity.title = observation.title.clone();
            entity.kind = observation.kind.clone();
            entity.path = restored_path.clone();
            entity.set_synced_tree_remote_version(
                observation
                    .remote_version
                    .as_ref()
                    .map(|version| version.0.clone()),
            );
        }
        let write_path = undo_projection_write_path(state_root, &mount, &entity.path)?;
        let frontmatter = if preimage.shadow.frontmatter.trim().is_empty() {
            frontmatter_from_entity(&entity)
        } else {
            preimage.shadow.frontmatter.clone()
        };
        let document = CanonicalDocument::new(frontmatter, preimage.shadow.rendered_body.clone());
        write_atomic(&write_path, render_canonical_markdown(&document).as_bytes())?;
        if previous_path
            .as_ref()
            .is_some_and(|path| path != &entity.path)
        {
            let previous_write_path =
                undo_projection_write_path(state_root, &mount, previous_path.as_ref().unwrap())?;
            match std::fs::remove_file(&previous_write_path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(HistoryError::Store(StoreError::Io(error.to_string()))),
            }
            if !mount.projection.uses_virtual_filesystem()
                && previous_path.as_ref().unwrap().components().count() >= 2
                && previous_path
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .is_some_and(|name| name == "page.md")
                && let Some(previous_container) = previous_write_path.parent()
            {
                let _ = std::fs::remove_dir(previous_container);
            }
        }

        entity.hydration = HydrationState::Hydrated;
        entity.content_hash = Some(preimage.shadow.body_hash.clone());
        store
            .save_shadow(&entry.mount_id, preimage.shadow.clone())
            .map_err(HistoryError::Store)?;
        store.save_entity(entity).map_err(HistoryError::Store)?;
        if let (Some(state_root), Some(previous_path), Some(previous_shadow)) =
            (state_root, previous_path.as_ref(), previous_shadow.as_ref())
            && windows_undo_projection_refresh_enabled(&mount.projection)
        {
            projection_refreshes.push(UndoProjectionRefresh::WindowsCloudFilesEntity {
                state_root: state_root.to_path_buf(),
                mount_id: entry.mount_id.clone(),
                entity_id: preimage.entity_id.clone(),
                previous_path: previous_path.clone(),
                previous_shadow: previous_shadow.clone(),
            });
        }
        if let Some(refresh) = undo_projection_refresh_request(
            store,
            &mount,
            &preimage.entity_id,
            previous_refresh_identifiers,
        )? {
            projection_refreshes.push(refresh);
        }
    }

    for operation in &undo_plan.operations {
        let UndoOperation::ArchiveCreatedEntity { entity_id, .. } = operation else {
            continue;
        };
        let entity = store
            .get_entity(&entry.mount_id, entity_id)
            .map_err(HistoryError::Store)?
            .ok_or_else(|| {
                unsafe_undo_local_state(entity_id, "created entity disappeared during undo")
            })?;
        let refresh_identifiers = if mount.projection.uses_virtual_filesystem() {
            virtual_fs_ancestor_container_identifiers(store, &entry.mount_id, entity_id)
                .map_err(|error| HistoryError::Store(StoreError::Io(error.to_string())))?
        } else {
            Vec::new()
        };
        let previous_shadow = store
            .load_shadow(&entry.mount_id, entity_id)
            .map_err(HistoryError::Store)?;
        let (content_path, visible_paths) =
            undo_entity_projection_paths(state_root, &mount, &entity)?;
        remove_undo_projection(&content_path)?;
        if !mount.projection.uses_virtual_filesystem() {
            for visible_path in visible_paths {
                if visible_path.exists() {
                    remove_undo_projection(&visible_path)?;
                }
            }
        }
        store
            .delete_entity(&entry.mount_id, entity_id)
            .map_err(HistoryError::Store)?;
        if mount.projection == locality_store::ProjectionMode::MacosFileProvider {
            projection_refreshes.push(UndoProjectionRefresh::MacosContainers {
                mount: mount.clone(),
                identifiers: refresh_identifiers,
            });
        } else if mount.projection == locality_store::ProjectionMode::WindowsCloudFiles {
            projection_refreshes.push(UndoProjectionRefresh::WindowsCloudFilesRemovedEntity {
                state_root: state_root
                    .ok_or_else(|| {
                        HistoryError::Store(StoreError::Io(
                            "Windows Cloud Files undo reconciliation requires a state root"
                                .to_string(),
                        ))
                    })?
                    .to_path_buf(),
                mount: mount.clone(),
                entity_id: entity_id.clone(),
                previous_path: entity.path,
                previous_shadow,
            });
        }
    }

    Ok(projection_refreshes)
}

fn windows_undo_projection_refresh_enabled(projection: &locality_store::ProjectionMode) -> bool {
    matches!(
        projection,
        locality_store::ProjectionMode::WindowsCloudFiles
    )
}

fn preflight_undo_reconciliation_paths<S>(
    store: &S,
    entry: &JournalEntry,
    changed_remote_ids: &[RemoteId],
    entity_observations: &BTreeMap<RemoteId, (RemoteObservation, PathBuf)>,
    state_root: Option<&Path>,
    mount: &MountConfig,
) -> Result<(), HistoryError>
where
    S: EntityRepository + VirtualMutationRepository,
{
    let entities = store
        .list_entities(&entry.mount_id)
        .map_err(HistoryError::Store)?;
    let mutations = store
        .list_virtual_mutations(&entry.mount_id)
        .map_err(HistoryError::Store)?;

    for preimage in entry
        .preimages
        .iter()
        .filter(|preimage| changed_remote_ids.contains(&preimage.entity_id))
    {
        let existing = store
            .get_entity(&entry.mount_id, &preimage.entity_id)
            .map_err(HistoryError::Store)?;
        let destination = entity_observations
            .get(&preimage.entity_id)
            .map(|(_, path)| path.clone())
            .or_else(|| existing.as_ref().map(|entity| entity.path.clone()));
        let Some(destination) = destination else {
            continue;
        };
        if existing
            .as_ref()
            .is_some_and(|entity| entity.path == destination)
        {
            continue;
        }

        if let Some(mutation) = mutations
            .iter()
            .find(|mutation| mutation.projected_path == destination)
        {
            return Err(invalid_undo_observation(
                &preimage.entity_id,
                format!(
                    "restored projection path `{}` is reserved by local mutation `{}`",
                    destination.display(),
                    mutation.local_id
                ),
            ));
        }
        if is_page_document_destination(&destination)
            && page_container_owned_by_another(
                &entities,
                &mutations,
                &preimage.entity_id,
                &destination,
            )
        {
            return Err(invalid_undo_observation(
                &preimage.entity_id,
                format!(
                    "restored page container `{}` is already owned",
                    page_container_path(&destination).display()
                ),
            ));
        }

        let backing_path = undo_projection_write_path(state_root, mount, &destination)?;
        preflight_undo_destination_path(&preimage.entity_id, &destination, &backing_path)?;
        if mount.projection.uses_virtual_filesystem() {
            for root in file_provider::mount_access_roots(mount) {
                let visible_path = root.join(&destination);
                if visible_path != backing_path {
                    preflight_undo_destination_path(
                        &preimage.entity_id,
                        &destination,
                        &visible_path,
                    )?;
                }
            }
        }
    }

    Ok(())
}

fn is_page_document_destination(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == "page.md")
}

fn page_container_owned_by_another(
    entities: &[EntityRecord],
    mutations: &[locality_store::VirtualMutationRecord],
    entity_id: &RemoteId,
    destination: &Path,
) -> bool {
    let container = page_container_path(destination);
    entities.iter().any(|entity| {
        entity.remote_id != *entity_id
            && entity.kind == locality_core::model::EntityKind::Page
            && page_container_path(&entity.path) == container
    }) || mutations.iter().any(|mutation| {
        is_page_document_destination(&mutation.projected_path)
            && page_container_path(&mutation.projected_path) == container
    })
}

fn preflight_undo_destination_path(
    entity_id: &RemoteId,
    relative_path: &Path,
    path: &Path,
) -> Result<(), HistoryError> {
    let page_container_exists = is_page_document_destination(relative_path)
        && path.parent().is_some_and(|parent| parent.exists());
    if path.exists() || page_container_exists {
        return Err(invalid_undo_observation(
            entity_id,
            format!(
                "restored projection path `{}` already exists at `{}`",
                relative_path.display(),
                path.display()
            ),
        ));
    }
    Ok(())
}

fn undo_projection_refresh_request<S>(
    store: &S,
    mount: &MountConfig,
    entity_id: &RemoteId,
    mut identifiers: Vec<String>,
) -> Result<Option<UndoProjectionRefresh>, HistoryError>
where
    S: MountRepository + EntityRepository,
{
    if mount.projection != locality_store::ProjectionMode::MacosFileProvider {
        return Ok(None);
    }
    let current = virtual_fs_ancestor_container_identifiers(store, &mount.mount_id, entity_id)
        .map_err(|error| HistoryError::Store(StoreError::Io(error.to_string())))?;
    identifiers.extend(current);
    Ok(Some(UndoProjectionRefresh::MacosContainers {
        mount: mount.clone(),
        identifiers,
    }))
}

fn signal_macos_projection_identifiers(
    mount: &MountConfig,
    identifiers: Vec<String>,
) -> Result<(), HistoryError> {
    signal_macos_projection_identifiers_with(mount, identifiers, |mount_id, identifier| {
        file_provider_helper::refresh_macos_file_provider_container(mount_id, identifier)
            .map(|_| ())
            .map_err(|error| error.message())
    })
}

fn signal_macos_projection_identifiers_with<F>(
    mount: &MountConfig,
    mut identifiers: Vec<String>,
    mut refresh: F,
) -> Result<(), HistoryError>
where
    F: FnMut(&str, &str) -> Result<(), String>,
{
    if mount.projection != locality_store::ProjectionMode::MacosFileProvider {
        return Ok(());
    }
    identifiers.push("working-set".to_string());
    identifiers.sort();
    identifiers.dedup();
    for identifier in identifiers {
        refresh(&mount.mount_id.0, &identifier).map_err(|reason| {
            HistoryError::UndoProjectionRefreshFailed {
                mount_id: mount.mount_id.clone(),
                identifier,
                reason,
            }
        })?;
    }
    Ok(())
}

fn validate_entity_undo_observations<S>(
    store: &S,
    entry: &JournalEntry,
    undo_plan: &UndoPlan,
    changed_remote_ids: &[RemoteId],
    observations: &[RemoteObservation],
) -> Result<BTreeMap<RemoteId, (RemoteObservation, PathBuf)>, HistoryError>
where
    S: EntityRepository,
{
    let mut validated = BTreeMap::new();
    for operation in &undo_plan.operations {
        let (entity_id, expected_deleted, expected_parent_id, expected_title, expected_kind) =
            match operation {
                UndoOperation::RestoreEntityLocation {
                    entity_id,
                    previous_parent_id,
                    previous_title,
                    ..
                } => {
                    let kind = indexed_entity_kind(store, &entry.mount_id, entity_id)?;
                    (
                        entity_id,
                        false,
                        Some(previous_parent_id.clone()),
                        Some(previous_title.as_str()),
                        kind,
                    )
                }
                UndoOperation::RestoreArchivedEntity { entity_id, .. } => (
                    entity_id,
                    false,
                    journal_preimage_parent_id(entry, entity_id),
                    None,
                    journal_preimage_entity_kind(entry, entity_id).ok_or_else(|| {
                        invalid_undo_observation(entity_id, "entity preimage has no valid kind")
                    })?,
                ),
                UndoOperation::ArchiveCreatedEntity {
                    entity_id,
                    expected,
                } => {
                    let kind = indexed_entity_kind(store, &entry.mount_id, entity_id)?;
                    (
                        entity_id,
                        true,
                        archive_created_expected_parent_id(entry, entity_id, expected.as_ref()),
                        None,
                        kind,
                    )
                }
                _ => continue,
            };

        if !changed_remote_ids.contains(entity_id) {
            return Err(invalid_undo_observation(
                entity_id,
                "undo apply result did not report the entity as changed",
            ));
        }
        let candidates = observations
            .iter()
            .filter(|observation| observation.remote_id == *entity_id)
            .collect::<Vec<_>>();
        if candidates.len() != 1 {
            return Err(invalid_undo_observation(
                entity_id,
                "undo apply result must contain exactly one matching observation",
            ));
        }
        let observation = candidates[0];
        if observation.mount_id != entry.mount_id {
            return Err(invalid_undo_observation(
                entity_id,
                "observation belongs to a different mount",
            ));
        }
        if observation.deleted != expected_deleted {
            return Err(invalid_undo_observation(
                entity_id,
                if expected_deleted {
                    "observation does not report the created entity as deleted"
                } else {
                    "observation still reports the restored entity as deleted"
                },
            ));
        }
        if observation.parent_remote_id != expected_parent_id {
            return Err(invalid_undo_observation(
                entity_id,
                "observation does not report the expected parent",
            ));
        }
        if observation.kind != expected_kind {
            return Err(invalid_undo_observation(
                entity_id,
                "observation does not report the expected entity kind",
            ));
        }
        if expected_title.is_some_and(|expected_title| observation.title != expected_title) {
            return Err(invalid_undo_observation(
                entity_id,
                "observation does not report the restored title",
            ));
        }
        let path = validated_undo_observation_path(
            store,
            &entry.mount_id,
            entity_id,
            observation.parent_remote_id.as_ref(),
            &observation.kind,
            &observation.projected_path,
        )?;
        validated.insert(entity_id.clone(), (observation.clone(), path));
    }
    Ok(validated)
}

fn indexed_entity_kind<S>(
    store: &S,
    mount_id: &MountId,
    entity_id: &RemoteId,
) -> Result<locality_core::model::EntityKind, HistoryError>
where
    S: EntityRepository,
{
    store
        .get_entity(mount_id, entity_id)
        .map_err(HistoryError::Store)?
        .map(|entity| entity.kind)
        .ok_or_else(|| invalid_undo_observation(entity_id, "entity is not indexed locally"))
}

fn validated_undo_observation_path<S>(
    store: &S,
    mount_id: &MountId,
    entity_id: &RemoteId,
    parent_id: Option<&RemoteId>,
    kind: &locality_core::model::EntityKind,
    projected_path: &Path,
) -> Result<PathBuf, HistoryError>
where
    S: EntityRepository,
{
    if projected_path.as_os_str().is_empty()
        || projected_path.is_absolute()
        || projected_path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(invalid_undo_observation(
            entity_id,
            "observation contains an invalid projected path",
        ));
    }
    let resolved_path = if let Some(parent_id) = parent_id {
        let parent = store
            .get_entity(mount_id, parent_id)
            .map_err(HistoryError::Store)?
            .ok_or_else(|| invalid_undo_observation(entity_id, "observed parent is not indexed"))?;
        let parent_path = if parent.kind == locality_core::model::EntityKind::Page {
            page_container_path(&parent.path)
        } else {
            parent.path
        };

        if let Ok(relative) = projected_path.strip_prefix(&parent_path) {
            if !valid_observation_leaf_shape(relative, kind) {
                return Err(invalid_undo_observation(
                    entity_id,
                    "observation projected path is nested too deeply below the restored parent",
                ));
            }
            projected_path.to_path_buf()
        } else if valid_observation_leaf_shape(projected_path, kind) {
            parent_path.join(projected_path)
        } else {
            Err(invalid_undo_observation(
                entity_id,
                "observation projected path does not belong to the restored parent",
            ))?
        }
    } else if valid_observation_leaf_shape(projected_path, kind) {
        projected_path.to_path_buf()
    } else {
        return Err(invalid_undo_observation(
            entity_id,
            "root observation contains an unrelated multi-component path",
        ));
    };

    if let Some(owner) = store
        .find_entity_by_path(mount_id, &resolved_path)
        .map_err(HistoryError::Store)?
        && owner.remote_id != *entity_id
    {
        return Err(invalid_undo_observation(
            entity_id,
            format!(
                "observation projected path is already owned by entity `{}`",
                owner.remote_id.0
            ),
        ));
    }
    Ok(resolved_path)
}

fn valid_observation_leaf_shape(path: &Path, kind: &locality_core::model::EntityKind) -> bool {
    path.components().count() == 1
        || (kind == &locality_core::model::EntityKind::Page
            && path.components().count() == 2
            && path.file_name().is_some_and(|name| name == "page.md"))
}

fn journal_preimage_parent_id(entry: &JournalEntry, entity_id: &RemoteId) -> Option<RemoteId> {
    let shadow = entry
        .preimages
        .iter()
        .find(|preimage| &preimage.entity_id == entity_id)
        .map(|preimage| &preimage.shadow)?;
    parse_canonical_markdown(&render_canonical_markdown(&CanonicalDocument::new(
        shadow.frontmatter.clone(),
        shadow.rendered_body.clone(),
    )))
    .ok()?
    .frontmatter
    .loc?
    .parent
}

fn journal_preimage_entity_kind(
    entry: &JournalEntry,
    entity_id: &RemoteId,
) -> Option<locality_core::model::EntityKind> {
    let shadow = entry
        .preimages
        .iter()
        .find(|preimage| &preimage.entity_id == entity_id)
        .map(|preimage| &preimage.shadow)?;
    parse_canonical_markdown(&render_canonical_markdown(&CanonicalDocument::new(
        shadow.frontmatter.clone(),
        shadow.rendered_body.clone(),
    )))
    .ok()?
    .frontmatter
    .loc?
    .entity_type
}

fn archive_created_expected_parent_id(
    entry: &JournalEntry,
    entity_id: &RemoteId,
    expected: Option<&EntityUndoState>,
) -> Option<RemoteId> {
    let operation_index = entry.apply_effects.iter().find_map(|effect| match effect {
        JournalApplyEffect::CreatedEntity {
            operation_index,
            entity_id: created_entity_id,
            ..
        } if created_entity_id == entity_id => Some(*operation_index),
        _ => None,
    });
    if let Some(locality_core::planner::PushOperation::CreateEntity {
        parent_id,
        parent_workspace,
        ..
    }) = operation_index.and_then(|index| entry.plan.operations.get(index))
    {
        return (!parent_workspace).then(|| parent_id.clone());
    }
    expected.map(|state| state.parent_id.clone())
}

fn remove_undo_projection(path: &Path) -> Result<(), HistoryError> {
    std::fs::remove_file(path)
        .map_err(|error| HistoryError::Store(StoreError::Io(error.to_string())))?;
    if path.file_name().is_some_and(|name| name == "page.md")
        && let Some(container) = path.parent()
    {
        let _ = std::fs::remove_dir(container);
    }
    Ok(())
}

fn invalid_undo_observation(entity_id: &RemoteId, reason: impl Into<String>) -> HistoryError {
    HistoryError::InvalidUndoObservation {
        entity_id: entity_id.clone(),
        reason: reason.into(),
    }
}

fn undo_projection_write_path(
    state_root: Option<&Path>,
    mount: &MountConfig,
    relative_path: &Path,
) -> Result<PathBuf, HistoryError> {
    if mount.projection.uses_virtual_filesystem() {
        let Some(state_root) = state_root else {
            return Err(HistoryError::Store(StoreError::Io(
                "virtual filesystem undo reconciliation requires a state root".to_string(),
            )));
        };
        return virtual_fs_content_path(state_root, &mount.mount_id, relative_path)
            .map_err(|error| HistoryError::Store(StoreError::Io(error.to_string())));
    }

    Ok(mount.root.join(relative_path))
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), HistoryError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| HistoryError::Store(StoreError::Io(error.to_string())))?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("undo");
    let temp_path = path.with_file_name(format!(".{file_name}.loc-undo-tmp"));
    std::fs::write(&temp_path, contents)
        .map_err(|error| HistoryError::Store(StoreError::Io(error.to_string())))?;
    #[cfg(windows)]
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(HistoryError::Store(StoreError::Io(error.to_string()))),
    }
    std::fs::rename(&temp_path, path)
        .map_err(|error| HistoryError::Store(StoreError::Io(error.to_string())))
}

fn frontmatter_from_entity(entity: &EntityRecord) -> String {
    let mut frontmatter = format!("loc:\n  id: {}\n  type: page\n", entity.remote_id.0);
    if let Some(remote_edited_at) = &entity.remote_edited_at {
        frontmatter.push_str(&format!("  remote_edited_at: {remote_edited_at}\n"));
    }
    frontmatter.push_str(&format!("title: {}\n", yaml_string(&entity.title)));
    frontmatter
}

fn yaml_string(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ' ' | '-' | '_' | '.'))
    {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

pub fn undo_report_exit_code(report: &UndoReport) -> i32 {
    if report.ok {
        0
    } else if report.action == "reverse_apply_failed" {
        1
    } else {
        5
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct JournalEntryOutput {
    pub push_id: String,
    pub mount_id: String,
    pub remote_ids: Vec<String>,
    pub status: String,
    pub failure: Option<String>,
    pub author: String,
    pub previous_push_id: Option<String>,
    pub created_at_unix_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readable_diff: Option<locality_core::readable_diff::ReadableDiffOutput>,
    pub preimage_count: usize,
    pub apply_effect_count: usize,
    pub plan_summary: PlanSummaryOutput,
    pub operation_count: usize,
}

impl JournalEntryOutput {
    pub fn from_entry(value: JournalEntry, include_diff: bool) -> Self {
        let (status, failure) = status_parts(value.status);
        let operation_count = value.plan.operations.len();
        let readable_diff = if include_diff {
            value.readable_diff
        } else {
            None
        };

        Self {
            push_id: value.push_id.0,
            mount_id: value.mount_id.0,
            remote_ids: value
                .remote_ids
                .into_iter()
                .map(|remote_id| remote_id.0)
                .collect(),
            status,
            failure,
            author: value.metadata.author.display_name,
            previous_push_id: value.metadata.previous_push_id.map(|push_id| push_id.0),
            created_at_unix_ms: value.metadata.created_at_unix_ms,
            readable_diff,
            preimage_count: value.preimages.len(),
            apply_effect_count: value.apply_effects.len(),
            plan_summary: PlanSummaryOutput::from(value.plan.summary),
            operation_count,
        }
    }
}

impl From<JournalEntry> for JournalEntryOutput {
    fn from(value: JournalEntry) -> Self {
        Self::from_entry(value, false)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UndoPlanOutput {
    pub target_push_id: String,
    pub mount_id: String,
    pub affected_entities: Vec<String>,
    pub status: String,
    pub operations: Vec<UndoOperationOutput>,
    pub unsupported: Vec<UnsupportedUndoOutput>,
}

impl From<UndoPlan> for UndoPlanOutput {
    fn from(value: UndoPlan) -> Self {
        Self {
            target_push_id: value.target_push_id.0,
            mount_id: value.mount_id.0,
            affected_entities: value
                .affected_entities
                .into_iter()
                .map(|remote_id| remote_id.0)
                .collect(),
            status: undo_plan_status_name(&value.status).to_string(),
            operations: value
                .operations
                .into_iter()
                .map(UndoOperationOutput::from)
                .collect(),
            unsupported: value
                .unsupported
                .into_iter()
                .map(UnsupportedUndoOutput::from)
                .collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UndoOperationOutput {
    RestoreBlockContent {
        block_id: String,
        content: String,
    },
    MoveBlock {
        block_id: String,
        after: Option<String>,
    },
    RestoreArchivedBlock {
        block_id: String,
        parent_id: String,
        after: Option<String>,
        content: String,
    },
    ArchiveCreatedBlock {
        block_id: String,
    },
    ArchiveCreatedEntity {
        entity_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        expected: Option<EntityUndoStateOutput>,
    },
    RestoreEntityBody {
        entity_id: String,
        expected_current: String,
        previous: String,
    },
    RestoreProperties {
        entity_id: String,
        expected_current: Vec<PropertyUpdateOutput>,
        previous: Vec<PropertyUpdateOutput>,
    },
    RestoreEntityLocation {
        entity_id: String,
        expected_parent_id: String,
        expected_title: String,
        previous_parent_id: String,
        previous_title: String,
    },
    RestoreArchivedEntity {
        entity_id: String,
        expected: EntityUndoStateOutput,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EntityUndoStateOutput {
    pub parent_id: String,
    pub title: String,
    pub properties: Vec<PropertyUpdateOutput>,
    pub body: String,
    pub archived: bool,
}

impl From<EntityUndoState> for EntityUndoStateOutput {
    fn from(value: EntityUndoState) -> Self {
        Self {
            parent_id: value.parent_id.0,
            title: value.title,
            properties: property_updates(value.properties),
            body: value.body,
            archived: value.archived,
        }
    }
}

impl From<UndoOperation> for UndoOperationOutput {
    fn from(value: UndoOperation) -> Self {
        match value {
            UndoOperation::RestoreBlockContent { block_id, content } => Self::RestoreBlockContent {
                block_id: block_id.0,
                content,
            },
            UndoOperation::MoveBlock { block_id, after } => Self::MoveBlock {
                block_id: block_id.0,
                after: after.map(|remote_id| remote_id.0),
            },
            UndoOperation::RestoreArchivedBlock {
                block_id,
                parent_id,
                after,
                content,
                native_kind: _,
            } => Self::RestoreArchivedBlock {
                block_id: block_id.0,
                parent_id: parent_id.0,
                after: after.map(|remote_id| remote_id.0),
                content,
            },
            UndoOperation::ArchiveCreatedBlock { block_id } => Self::ArchiveCreatedBlock {
                block_id: block_id.0,
            },
            UndoOperation::ArchiveCreatedEntity {
                entity_id,
                expected,
            } => Self::ArchiveCreatedEntity {
                entity_id: entity_id.0,
                expected: expected.map(EntityUndoStateOutput::from),
            },
            UndoOperation::RestoreEntityBody {
                entity_id,
                expected_current,
                previous,
            } => Self::RestoreEntityBody {
                entity_id: entity_id.0,
                expected_current,
                previous,
            },
            UndoOperation::RestoreProperties {
                entity_id,
                expected_current,
                previous,
            } => Self::RestoreProperties {
                entity_id: entity_id.0,
                expected_current: property_updates(expected_current),
                previous: property_updates(previous),
            },
            UndoOperation::RestoreEntityLocation {
                entity_id,
                expected_parent_id,
                expected_title,
                previous_parent_id,
                previous_title,
            } => Self::RestoreEntityLocation {
                entity_id: entity_id.0,
                expected_parent_id: expected_parent_id.0,
                expected_title,
                previous_parent_id: previous_parent_id.0,
                previous_title,
            },
            UndoOperation::RestoreArchivedEntity {
                entity_id,
                expected,
            } => Self::RestoreArchivedEntity {
                entity_id: entity_id.0,
                expected: EntityUndoStateOutput::from(expected),
            },
        }
    }
}

fn property_updates(
    properties: std::collections::BTreeMap<String, locality_core::planner::PropertyValue>,
) -> Vec<PropertyUpdateOutput> {
    properties
        .into_iter()
        .map(|(key, value)| PropertyUpdateOutput {
            key,
            value: PropertyValueOutput::from(value),
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UnsupportedUndoOutput {
    pub operation_index: usize,
    pub code: String,
    pub message: String,
}

impl From<UnsupportedUndoOperation> for UnsupportedUndoOutput {
    fn from(value: UnsupportedUndoOperation) -> Self {
        Self {
            operation_index: value.operation_index,
            code: value.code,
            message: value.message,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryError {
    MountNotFound(PathBuf),
    JournalNotFound(PushId),
    UndoNotLatest {
        push_id: PushId,
        entity_id: RemoteId,
        latest_push_id: Option<PushId>,
    },
    IncompleteUndoApplyResult {
        missing: Vec<RemoteId>,
    },
    UndoProjectionRefreshFailed {
        mount_id: MountId,
        identifier: String,
        reason: String,
    },
    InvalidUndoObservation {
        entity_id: RemoteId,
        reason: String,
    },
    UnsafeUndoLocalState {
        entity_id: RemoteId,
        reason: String,
    },
    Store(StoreError),
}

impl HistoryError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::MountNotFound(_) => "mount_not_found",
            Self::JournalNotFound(_) => "journal_not_found",
            Self::UndoNotLatest { .. } => "undo_not_latest",
            Self::IncompleteUndoApplyResult { .. } => "incomplete_undo_apply_result",
            Self::UndoProjectionRefreshFailed { .. } => "undo_projection_refresh_failed",
            Self::InvalidUndoObservation { .. } => "invalid_undo_observation",
            Self::UnsafeUndoLocalState { .. } => "unsafe_undo_local_state",
            Self::Store(StoreError::EntityPathMissing { .. }) => "entity_path_missing",
            Self::Store(_) => "store_error",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::MountNotFound(path) => {
                format!("no Locality mount contains `{}`", path.display())
            }
            Self::JournalNotFound(push_id) => {
                format!("journal entry `{}` was not found", push_id.0)
            }
            Self::UndoNotLatest {
                push_id,
                entity_id,
                latest_push_id,
            } => match latest_push_id {
                Some(latest_push_id) => format!(
                    "cannot undo journal `{}` because later journal `{}` touches entity `{}`",
                    push_id.0, latest_push_id.0, entity_id.0
                ),
                None => format!(
                    "cannot undo journal `{}` because it is not the active latest journal for entity `{}`",
                    push_id.0, entity_id.0
                ),
            },
            Self::IncompleteUndoApplyResult { missing } => format!(
                "undo apply result omitted changed entity ids required for reconciliation: {}",
                missing
                    .iter()
                    .map(|entity_id| entity_id.0.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::UndoProjectionRefreshFailed {
                mount_id,
                identifier,
                reason,
            } => format!(
                "undo updated provider state for mount `{}` but could not refresh container `{identifier}`: {reason}",
                mount_id.0
            ),
            Self::InvalidUndoObservation { entity_id, reason } => {
                format!(
                    "undo observation for entity `{}` is invalid: {reason}",
                    entity_id.0
                )
            }
            Self::UnsafeUndoLocalState { entity_id, reason } => {
                format!(
                    "local state for undo entity `{}` is unsafe: {reason}",
                    entity_id.0
                )
            }
            Self::Store(error) => error.to_string(),
        }
    }
}

struct PathFilter {
    mount_id: MountId,
    remote_id: RemoteId,
}

fn resolve_path_filter<S>(store: &S, path: &Path) -> Result<PathFilter, HistoryError>
where
    S: MountRepository + EntityRepository,
{
    let absolute_path = absolute_path(path)?;
    let mounts = store.load_mounts().map_err(HistoryError::Store)?;
    let mount = find_mount_for_path(&mounts, &absolute_path)
        .ok_or_else(|| HistoryError::MountNotFound(absolute_path.clone()))?;
    let mut relative_path = relative_entity_path(mount, &absolute_path)?;
    let mut entity = store
        .find_entity_by_path(&mount.mount_id, &relative_path)
        .map_err(HistoryError::Store)?;
    if entity.is_none() && absolute_path.is_dir() {
        let page_relative_path = page_document_path(&relative_path);
        if let Some(page_entity) = store
            .find_entity_by_path(&mount.mount_id, &page_relative_path)
            .map_err(HistoryError::Store)?
        {
            relative_path = page_relative_path;
            entity = Some(page_entity);
        }
    }
    let entity = entity.ok_or_else(|| {
        HistoryError::Store(StoreError::EntityPathMissing {
            mount_id: mount.mount_id.clone(),
            path: relative_path,
        })
    })?;

    Ok(PathFilter {
        mount_id: mount.mount_id.clone(),
        remote_id: entity.remote_id,
    })
}

fn entry_matches_filter(entry: &JournalEntry, filter: &PathFilter) -> bool {
    entry.mount_id == filter.mount_id
        && (entry
            .remote_ids
            .iter()
            .any(|remote_id| remote_id == &filter.remote_id)
            || entry
                .plan
                .affected_entities
                .iter()
                .any(|remote_id| remote_id == &filter.remote_id)
            || entry
                .apply_effects
                .iter()
                .any(|effect| apply_effect_matches_remote(effect, &filter.remote_id)))
}

fn apply_effect_matches_remote(effect: &JournalApplyEffect, remote_id: &RemoteId) -> bool {
    match effect {
        JournalApplyEffect::ArchivedEntity { entity_id, .. }
        | JournalApplyEffect::UpdatedEntityBody { entity_id, .. }
        | JournalApplyEffect::UpdatedProperties { entity_id, .. }
        | JournalApplyEffect::MovedEntity { entity_id, .. }
        | JournalApplyEffect::CreatedEntity { entity_id, .. } => entity_id == remote_id,
        JournalApplyEffect::UpdatedBlock { .. }
        | JournalApplyEffect::CreatedBlock { .. }
        | JournalApplyEffect::MovedBlock { .. }
        | JournalApplyEffect::ArchivedBlock { .. } => false,
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, HistoryError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| HistoryError::Store(StoreError::Io(error.to_string())))
    }
}

fn find_mount_for_path<'a>(mounts: &'a [MountConfig], path: &Path) -> Option<&'a MountConfig> {
    file_provider::find_mount_for_path(mounts, path).map(|(mount, _)| mount)
}

fn relative_entity_path(
    mount: &MountConfig,
    absolute_path: &Path,
) -> Result<PathBuf, HistoryError> {
    file_provider::match_mount_path(mount, absolute_path)
        .map(|matched| matched.relative_path)
        .ok_or_else(|| HistoryError::MountNotFound(absolute_path.to_path_buf()))
}

fn status_parts(status: JournalStatus) -> (String, Option<String>) {
    match status {
        JournalStatus::Prepared => ("prepared".to_string(), None),
        JournalStatus::Applying => ("applying".to_string(), None),
        JournalStatus::Applied => ("applied".to_string(), None),
        JournalStatus::Reconciled => ("reconciled".to_string(), None),
        JournalStatus::Reverted => ("reverted".to_string(), None),
        JournalStatus::Failed(message) => ("failed".to_string(), Some(message)),
    }
}

fn status_name(status: &JournalStatus) -> &'static str {
    match status {
        JournalStatus::Prepared => "prepared",
        JournalStatus::Applying => "applying",
        JournalStatus::Applied => "applied",
        JournalStatus::Reconciled => "reconciled",
        JournalStatus::Reverted => "reverted",
        JournalStatus::Failed(_) => "failed",
    }
}

fn undo_boundary_message(status: &JournalStatus) -> &'static str {
    match status {
        JournalStatus::Applying => {
            "journal is currently applying; wait for it to finish before undoing"
        }
        JournalStatus::Failed(_) => {
            "failed journals may have partial remote effects; remote undo requires pre-push snapshots"
        }
        JournalStatus::Applied | JournalStatus::Reconciled => {
            "remote undo requires connector reverse-apply support"
        }
        JournalStatus::Prepared | JournalStatus::Reverted => {
            "journal entry does not need remote undo"
        }
    }
}

fn undo_boundary(plan: &UndoPlan) -> (&'static str, &'static str) {
    match plan.status {
        UndoPlanStatus::Complete => (
            "reverse_apply_not_implemented",
            "reverse apply is not implemented yet",
        ),
        UndoPlanStatus::Partial => (
            "undo_plan_partial",
            "undo plan is partial; some operations cannot be reversed safely",
        ),
        UndoPlanStatus::Blocked => (
            "undo_plan_blocked",
            "no reversible operations can be derived from the journal preimages",
        ),
    }
}

fn undo_plan_status_name(status: &UndoPlanStatus) -> &'static str {
    match status {
        UndoPlanStatus::Complete => "complete",
        UndoPlanStatus::Partial => "partial",
        UndoPlanStatus::Blocked => "blocked",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use locality_core::journal::JournalMetadata;
    use locality_core::model::{MountId, RemoteId};
    use locality_core::planner::{PushOperation, PushPlan};
    use locality_core::readable_diff::{
        ReadableDiffFileOutput, ReadableDiffFileStatus, ReadableDiffOutput,
    };
    use locality_store::{InMemoryStateStore, JournalRepository};

    #[test]
    fn macos_undo_projection_refresh_failure_is_not_silenced() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            "/tmp/Locality/notion-main",
        )
        .projection(locality_store::ProjectionMode::MacosFileProvider);

        let error = signal_macos_projection_identifiers_with(
            &mount,
            vec!["children:page-1".to_string()],
            |_, _| Err("injected provider failure".to_string()),
        )
        .expect_err("missing platform helper must fail the correctness path");

        assert_eq!(error.code(), "undo_projection_refresh_failed");
        assert!(error.message().contains("children:page-1"));
    }

    #[test]
    fn provider_refresh_failure_happens_after_journal_finalization() {
        let mut store = InMemoryStateStore::new();
        let push_id = PushId("push-1".to_string());
        store
            .append_journal(journal_entry("push-1"))
            .expect("append journal");
        let refreshes = vec![UndoProjectionRefresh::MacosContainers {
            mount: MountConfig::new(
                MountId::new("notion-main"),
                "notion",
                "/tmp/Locality/notion-main",
            )
            .projection(locality_store::ProjectionMode::MacosFileProvider),
            identifiers: vec!["children:page-1".to_string()],
        }];

        let error = finalize_undo_after_reconcile(&mut store, &push_id, &refreshes, |_, _| {
            Err(HistoryError::UndoProjectionRefreshFailed {
                mount_id: MountId::new("notion-main"),
                identifier: "children:page-1".to_string(),
                reason: "injected".to_string(),
            })
        })
        .expect_err("refresh failure must be reported");

        assert_eq!(error.code(), "undo_projection_refresh_failed");
        assert_eq!(
            store
                .get_journal(&push_id)
                .expect("get journal")
                .expect("journal")
                .status,
            JournalStatus::Reverted
        );
    }

    #[test]
    fn macos_undo_never_directly_rewrites_visible_replica() {
        assert!(!windows_undo_projection_refresh_enabled(
            &locality_store::ProjectionMode::MacosFileProvider
        ));
        assert!(windows_undo_projection_refresh_enabled(
            &locality_store::ProjectionMode::WindowsCloudFiles
        ));
        assert!(!windows_undo_projection_refresh_enabled(
            &locality_store::ProjectionMode::LinuxFuse
        ));
    }

    #[test]
    fn log_report_can_include_readable_diff_for_single_push() {
        let mut store = InMemoryStateStore::new();
        let expected_readable_diff =
            readable_diff("Roadmap.md", "diff --locality a/Roadmap.md b/Roadmap.md\n");
        store
            .append_journal(
                journal_entry("push-1")
                    .with_metadata(JournalMetadata::anonymous(
                        Some(PushId("push-0".to_string())),
                        Some(1_783_612_800_000),
                    ))
                    .with_readable_diff(Some(expected_readable_diff.clone())),
            )
            .expect("append first journal");
        store
            .append_journal(
                journal_entry("push-2")
                    .with_readable_diff(Some(readable_diff("Roadmap.md", "ignored\n"))),
            )
            .expect("append second journal");

        let report = run_log(
            &store,
            LogOptions {
                path: None,
                push_id: Some(PushId("push-1".to_string())),
                include_diff: true,
            },
        )
        .expect("log report with readable diff");

        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].push_id, "push-1");
        assert_eq!(report.entries[0].author, "anonymous");
        assert_eq!(
            report.entries[0].previous_push_id.as_deref(),
            Some("push-0")
        );
        assert_eq!(
            report.entries[0].created_at_unix_ms,
            Some(1_783_612_800_000)
        );
        assert_eq!(
            report.entries[0].readable_diff,
            Some(expected_readable_diff)
        );

        let report = run_log(
            &store,
            LogOptions {
                path: None,
                push_id: Some(PushId("push-1".to_string())),
                include_diff: false,
            },
        )
        .expect("log report without readable diff");

        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].push_id, "push-1");
        assert_eq!(report.entries[0].readable_diff, None);
    }

    #[test]
    fn undo_report_does_not_include_readable_diff() {
        let mut store = InMemoryStateStore::new();
        let mut entry = journal_entry("push-1")
            .with_readable_diff(Some(readable_diff("Roadmap.md", "large diff\n")));
        entry.status = JournalStatus::Prepared;
        store.append_journal(entry).expect("append journal");

        let report = run_undo(&mut store, "push-1").expect("undo report");

        assert!(report.ok);
        assert_eq!(
            report.entry.expect("undo entry").readable_diff,
            None,
            "undo reports must not leak saved readable diffs"
        );
    }

    fn journal_entry(push_id: &str) -> JournalEntry {
        JournalEntry::new(
            PushId(push_id.to_string()),
            MountId::new("notion-main"),
            vec![RemoteId::new("page-1")],
            PushPlan::new(
                vec![RemoteId::new("page-1")],
                vec![PushOperation::UpdateBlock {
                    block_id: RemoteId::new("page-1-paragraph-1"),
                    content: "Updated paragraph.".to_string(),
                }],
            ),
            JournalStatus::Reconciled,
        )
    }

    fn readable_diff(path: &str, text: &str) -> ReadableDiffOutput {
        ReadableDiffOutput {
            files: vec![ReadableDiffFileOutput {
                path: path.to_string(),
                old_label: format!("a/{path}"),
                new_label: format!("b/{path}"),
                status: ReadableDiffFileStatus::Modified,
                patch: text.to_string(),
            }],
            text: text.to_string(),
        }
    }
}
