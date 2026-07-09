use std::path::{Path, PathBuf};

use locality_core::journal::PushId;
use locality_core::model::{MountId, RemoteId};
use locality_core::planner::{GuardrailDecision, PushOperation, PushPlan};
use locality_core::push::{PushPipelineAction, PushPipelineResult};
use locality_core::{LocalityError, LocalityResult};
use locality_store::{
    AutoSaveEnrollmentRecord, AutoSaveRepository, AutoSaveState, EntityRecord, MountConfig,
    MountLiveModeRepository,
};

pub fn auto_save_enabled_for_path<S>(
    store: &S,
    mount_id: &MountId,
    relative_path: &Path,
) -> LocalityResult<Option<AutoSaveEnrollmentRecord>>
where
    S: AutoSaveRepository,
{
    let Some(enrollment) = store
        .get_auto_save_enrollment(mount_id, relative_path)
        .map_err(LocalityError::from)?
    else {
        return Ok(None);
    };

    if !enrollment.enabled {
        return Ok(None);
    }
    if matches!(
        enrollment.state,
        AutoSaveState::PausedRemoteChanged | AutoSaveState::PausedFailure
    ) {
        return Ok(None);
    }

    Ok(Some(enrollment))
}

pub fn auto_save_target_for_write<S>(
    store: &S,
    mount: &MountConfig,
    entity: &EntityRecord,
    event_path: &Path,
) -> LocalityResult<Option<PathBuf>>
where
    S: AutoSaveRepository + MountLiveModeRepository,
{
    if auto_save_enabled_for_path(store, &mount.mount_id, &entity.path)?.is_none()
        && !mount_live_mode_enabled_for_write(store, &mount.mount_id)?
    {
        return Ok(None);
    }

    Ok(Some(event_path.to_path_buf()))
}

fn mount_live_mode_enabled_for_write<S>(store: &S, mount_id: &MountId) -> LocalityResult<bool>
where
    S: MountLiveModeRepository,
{
    Ok(store
        .get_mount_live_mode(mount_id)
        .map_err(LocalityError::from)?
        .is_some_and(|record| record.enabled))
}

pub fn auto_save_block_reason(pipeline: &PushPipelineResult) -> Option<String> {
    if !pipeline.validation.is_clean() {
        return Some("local Markdown needs review before auto-save".to_string());
    }

    match &pipeline.action {
        PushPipelineAction::ProceedToApply | PushPipelineAction::Noop => {}
        PushPipelineAction::ConfirmPlan => {
            return Some("push plan needs explicit review".to_string());
        }
        PushPipelineAction::ConfirmDangerousPlan => {
            return Some("destructive push plan needs explicit review".to_string());
        }
        PushPipelineAction::FixValidation => {
            return Some("local Markdown needs review before auto-save".to_string());
        }
        PushPipelineAction::ReadOnlyBlocked => {
            return Some("mount is read-only".to_string());
        }
        PushPipelineAction::UnsupportedOperations { message, .. } => {
            return Some(message.clone());
        }
    }

    if let GuardrailDecision::ConfirmRequired { reasons } = &pipeline.guardrail {
        return Some(format!("guardrail requires review: {}", reasons.join(", ")));
    }

    let Some(plan) = pipeline.plan.as_ref() else {
        return Some("push plan is missing".to_string());
    };
    auto_save_plan_block_reason(plan)
}

pub fn auto_save_plan_block_reason(plan: &PushPlan) -> Option<String> {
    if !plan.degradations.is_empty() {
        return Some("diff needs review before auto-save".to_string());
    }

    for operation in &plan.operations {
        match operation {
            PushOperation::CreateEntity { .. }
            | PushOperation::UpdateBlock { .. }
            | PushOperation::AppendBlock { .. }
            | PushOperation::UpdateProperties { .. } => {}
            PushOperation::MoveEntity { .. } => {
                return Some("entity moves require review".to_string());
            }
            PushOperation::ReplaceBlock { .. } => {
                return Some("block replacements require review".to_string());
            }
            PushOperation::MoveBlock { .. } => {
                return Some("block moves require review".to_string());
            }
            PushOperation::UpdateMedia { .. } => {
                return Some("media updates require review".to_string());
            }
            PushOperation::ArchiveBlock { .. } | PushOperation::ArchiveEntity { .. } => {
                return Some("deletions require review".to_string());
            }
        }
    }

    None
}

pub fn mark_auto_save_active<S>(
    store: &mut S,
    mount_id: &MountId,
    path: &Path,
    remote_id: Option<RemoteId>,
    push_id: Option<&PushId>,
) -> LocalityResult<()>
where
    S: AutoSaveRepository,
{
    let Some(mut enrollment) = store
        .get_auto_save_enrollment(mount_id, path)
        .map_err(LocalityError::from)?
    else {
        return Ok(());
    };

    if let Some(remote_id) = remote_id {
        enrollment.remote_id = Some(remote_id);
    }
    enrollment.state = AutoSaveState::Active;
    enrollment.last_reason = None;
    enrollment.last_push_id = push_id.map(|push_id| push_id.0.clone());
    enrollment.updated_at = auto_save_timestamp();
    store
        .save_auto_save_enrollment(enrollment)
        .map_err(LocalityError::from)
}

pub fn mark_auto_save_blocked<S>(
    store: &mut S,
    mount_id: &MountId,
    path: &Path,
    reason: impl Into<String>,
) -> LocalityResult<()>
where
    S: AutoSaveRepository,
{
    update_auto_save_state(store, mount_id, path, |enrollment| {
        enrollment.state = AutoSaveState::Blocked;
        enrollment.last_reason = Some(reason.into());
        enrollment.updated_at = auto_save_timestamp();
    })
}

pub fn mark_auto_save_paused_failure<S>(
    store: &mut S,
    mount_id: &MountId,
    path: &Path,
    reason: impl Into<String>,
) -> LocalityResult<()>
where
    S: AutoSaveRepository,
{
    update_auto_save_state(store, mount_id, path, |enrollment| {
        enrollment.state = AutoSaveState::PausedFailure;
        enrollment.last_reason = Some(reason.into());
        enrollment.updated_at = auto_save_timestamp();
    })
}

pub fn mark_auto_save_paused_remote_changed<S>(
    store: &mut S,
    mount_id: &MountId,
    path: &Path,
    reason: impl Into<String>,
) -> LocalityResult<()>
where
    S: AutoSaveRepository,
{
    update_auto_save_state(store, mount_id, path, |enrollment| {
        enrollment.state = AutoSaveState::PausedRemoteChanged;
        enrollment.last_reason = Some(reason.into());
        enrollment.updated_at = auto_save_timestamp();
    })
}

pub fn pause_auto_save_for_remote_change<S>(
    store: &mut S,
    mount_id: &MountId,
    remote_id: &RemoteId,
) -> LocalityResult<()>
where
    S: AutoSaveRepository,
{
    let Some(mut enrollment) = store
        .find_auto_save_enrollment_by_remote_id(mount_id, remote_id)
        .map_err(LocalityError::from)?
    else {
        return Ok(());
    };
    if !enrollment.enabled {
        return Ok(());
    }

    enrollment.state = AutoSaveState::PausedRemoteChanged;
    enrollment.last_reason = Some("Notion changed externally".to_string());
    enrollment.updated_at = auto_save_timestamp();
    store
        .save_auto_save_enrollment(enrollment)
        .map_err(LocalityError::from)
}

fn update_auto_save_state<S>(
    store: &mut S,
    mount_id: &MountId,
    path: &Path,
    update: impl FnOnce(&mut AutoSaveEnrollmentRecord),
) -> LocalityResult<()>
where
    S: AutoSaveRepository,
{
    let Some(mut enrollment) = store
        .get_auto_save_enrollment(mount_id, path)
        .map_err(LocalityError::from)?
    else {
        return Ok(());
    };
    update(&mut enrollment);
    store
        .save_auto_save_enrollment(enrollment)
        .map_err(LocalityError::from)
}

pub fn auto_save_timestamp() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("unix_ms:{millis}")
}

#[cfg(test)]
mod tests {
    use locality_core::model::RemoteId;
    use locality_core::planner::{PlanDegradation, PlanDegradationKind, PushOperation, PushPlan};

    use super::auto_save_plan_block_reason;

    #[test]
    fn auto_save_policy_allows_create_and_safe_edits() {
        let plan = PushPlan::new(
            vec![RemoteId::new("page-1")],
            vec![
                PushOperation::UpdateBlock {
                    block_id: RemoteId::new("block-1"),
                    content: "Updated".to_string(),
                },
                PushOperation::AppendBlock {
                    parent_id: RemoteId::new("page-1"),
                    after: None,
                    content: "New".to_string(),
                },
            ],
        );

        assert_eq!(auto_save_plan_block_reason(&plan), None);
    }

    #[test]
    fn auto_save_policy_rejects_destructive_or_ambiguous_plans() {
        let archive = PushPlan::new(
            vec![RemoteId::new("page-1")],
            vec![PushOperation::ArchiveBlock {
                block_id: RemoteId::new("block-1"),
            }],
        );
        assert_eq!(
            auto_save_plan_block_reason(&archive),
            Some("deletions require review".to_string())
        );

        let replace = PushPlan::new(
            vec![RemoteId::new("page-1")],
            vec![PushOperation::ReplaceBlock {
                block_id: RemoteId::new("block-1"),
                content: "- Replacement".to_string(),
            }],
        );
        assert_eq!(
            auto_save_plan_block_reason(&replace),
            Some("block replacements require review".to_string())
        );

        let degraded =
            PushPlan::new(Vec::new(), Vec::new()).with_degradations(vec![PlanDegradation::new(
                PlanDegradationKind::AmbiguousBlockAlignment,
                "ambiguous",
            )]);
        assert_eq!(
            auto_save_plan_block_reason(&degraded),
            Some("diff needs review before auto-save".to_string())
        );
    }
}
