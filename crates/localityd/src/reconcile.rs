//! Scheduled remote reconciliation for daemon-managed mounts.
//!
//! The daemon keeps scheduling policy separate from reconciliation mechanics:
//! a strategy decides what to fetch for a mount, and this module executes that
//! decision by enumerating, refreshing local projections, and queueing hydration.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use locality_core::canonical::{parse_canonical_markdown, render_canonical_markdown};
use locality_core::freshness::{FreshnessTier, RemoteVersion};
use locality_core::hydration::{
    HydrationPolicy, HydrationReason, HydrationRequest, should_eager_hydrate,
};
use locality_core::model::{CanonicalDocument, EntityKind, HydrationState, RemoteId, TreeEntry};
use locality_core::path_projection::{is_page_document_path, page_container_path};
use locality_core::{LocalityError, LocalityResult};
use locality_store::{
    EntityRecord, EntityRepository, FreshnessStateRecord, FreshnessStateRepository, MountConfig,
    RemoteObservationRecord, RemoteObservationRepository,
};

use crate::hydration::HydrationEngine;
use crate::scheduler::PullSchedulerTick;
use crate::virtual_fs::virtual_fs_content_root;

const NOTION_CONNECTOR: &str = "notion";
const NOTION_PRIVATE_ROOT: &str = "Private";
const NOTION_WORKSPACE_ROOT: &str = "Workspace";
const UPGRADE_STAGE_PREFIX: &str = ".loc-upgrade-stage";

pub trait ScheduledPullSource {
    fn enumerate_mount(&self, mount: &MountConfig) -> LocalityResult<Vec<TreeEntry>>;

    fn database_schema_yaml(
        &self,
        _mount: &MountConfig,
        _remote_id: &RemoteId,
    ) -> LocalityResult<Option<String>> {
        Ok(None)
    }
}

pub trait FetchScheduleStrategy {
    fn mount_plan(&self, request: MountFetchSchedule<'_>) -> MountFetchPlan;
    fn entity_plan(&self, request: EntityFetchSchedule<'_>) -> EntityFetchPlan;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountFetchSchedule<'a> {
    pub mount: &'a MountConfig,
    pub tick: &'a PullSchedulerTick,
    pub policy: &'a HydrationPolicy,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MountFetchPlan {
    pub enumerate: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityFetchSchedule<'a> {
    pub mount: &'a MountConfig,
    pub entry: &'a TreeEntry,
    pub existing: Option<&'a EntityRecord>,
    pub page_count: usize,
    pub tick: &'a PullSchedulerTick,
    pub policy: &'a HydrationPolicy,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EntityFetchPlan {
    pub queue_hydration: Option<HydrationReason>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DefaultFetchScheduleStrategy;

impl FetchScheduleStrategy for DefaultFetchScheduleStrategy {
    fn mount_plan(&self, request: MountFetchSchedule<'_>) -> MountFetchPlan {
        if request.mount.projection.uses_virtual_filesystem()
            && request.mount.remote_root_id.is_none()
        {
            return MountFetchPlan::default();
        }

        MountFetchPlan {
            enumerate: !request.tick.is_idle(),
        }
    }

    fn entity_plan(&self, request: EntityFetchSchedule<'_>) -> EntityFetchPlan {
        if request.entry.kind != EntityKind::Page {
            return EntityFetchPlan::default();
        }

        if is_remote_root_entry(request.mount, request.entry) {
            return policy_hydration();
        }

        if should_eager_hydrate(request.page_count as u32, request.policy) {
            return policy_hydration();
        }

        if request
            .existing
            .is_some_and(|existing| should_refresh_hydrated_entity(existing, request.entry))
        {
            return remote_fast_forward_hydration();
        }

        EntityFetchPlan::default()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScheduledPullReport {
    pub mounts_checked: usize,
    pub mounts_polled: usize,
    pub enumerated: usize,
    pub stubbed: usize,
    pub schemas_written: usize,
    pub queued_hydrations: usize,
}

pub fn reconcile_scheduled_pull<S, H, Source, Strategy>(
    store: &mut S,
    hydration: &mut H,
    mounts: &[MountConfig],
    tick: &PullSchedulerTick,
    source: &Source,
    strategy: &Strategy,
    policy: &HydrationPolicy,
) -> LocalityResult<ScheduledPullReport>
where
    S: EntityRepository + RemoteObservationRepository + FreshnessStateRepository,
    H: HydrationEngine,
    Source: ScheduledPullSource + ?Sized,
    Strategy: FetchScheduleStrategy + ?Sized,
{
    reconcile_scheduled_pull_with_state_root(
        store, hydration, mounts, tick, source, strategy, policy, None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn reconcile_scheduled_pull_with_state_root<S, H, Source, Strategy>(
    store: &mut S,
    hydration: &mut H,
    mounts: &[MountConfig],
    tick: &PullSchedulerTick,
    source: &Source,
    strategy: &Strategy,
    policy: &HydrationPolicy,
    state_root: Option<&Path>,
) -> LocalityResult<ScheduledPullReport>
where
    S: EntityRepository + RemoteObservationRepository + FreshnessStateRepository,
    H: HydrationEngine,
    Source: ScheduledPullSource + ?Sized,
    Strategy: FetchScheduleStrategy + ?Sized,
{
    let mut report = ScheduledPullReport::default();

    for mount in mounts {
        report.mounts_checked += 1;

        let mount_plan = strategy.mount_plan(MountFetchSchedule {
            mount,
            tick,
            policy,
        });
        if !mount_plan.enumerate {
            continue;
        }

        let entries = source.enumerate_mount(mount)?;
        let page_count = entries
            .iter()
            .filter(|entry| entry.kind == EntityKind::Page)
            .count();

        report.mounts_polled += 1;
        report.enumerated += entries.len();

        let existing_entities = entries
            .iter()
            .map(|entry| {
                store
                    .get_entity(&entry.mount_id, &entry.remote_id)
                    .map_err(LocalityError::from)
            })
            .collect::<LocalityResult<Vec<_>>>()?;
        let reserved_root_projection_moves =
            plan_reserved_notion_root_projection_moves(mount, &entries, &existing_entities)?;
        let mut reserved_root_projection_moves_applied = reserved_root_projection_moves.is_empty();

        for (entry, existing) in entries.iter().zip(existing_entities.iter()) {
            if !reserved_root_projection_moves_applied
                && reserved_root_projection_move_affects_record(
                    entry,
                    existing.as_ref(),
                    &reserved_root_projection_moves,
                )
            {
                apply_reserved_notion_root_projection_moves(
                    mount,
                    &reserved_root_projection_moves,
                )?;
                save_reserved_notion_root_projection_move_records(
                    store,
                    &entries,
                    &existing_entities,
                    &reserved_root_projection_moves,
                )?;
                reserved_root_projection_moves_applied = true;
            }

            let observed_at = observation_timestamp();
            record_remote_observation(store, entry, existing.as_ref(), &observed_at)?;
            let entity_plan = strategy.entity_plan(EntityFetchSchedule {
                mount,
                entry,
                existing: existing.as_ref(),
                page_count,
                tick,
                policy,
            });

            let record = merged_entity_record(entry, existing.as_ref());
            rename_projection_if_needed(mount, existing.as_ref(), entry)?;

            match refresh_projection(source, mount, entry, state_root)? {
                ProjectionWrite::Stub => report.stubbed += 1,
                ProjectionWrite::Schema => report.schemas_written += 1,
                ProjectionWrite::None => {}
            }
            store.save_entity(record)?;

            if let Some(reason) = entity_plan.queue_hydration {
                hydration.queue(HydrationRequest::new(
                    mount.mount_id.clone(),
                    entry.remote_id.clone(),
                    mount.root.join(&entry.path),
                    HydrationState::Hydrated,
                    reason,
                ))?;
                report.queued_hydrations += 1;
            }
        }
    }

    Ok(report)
}

fn record_remote_observation<S>(
    store: &mut S,
    entry: &TreeEntry,
    existing: Option<&EntityRecord>,
    observed_at: &str,
) -> LocalityResult<()>
where
    S: RemoteObservationRepository + FreshnessStateRepository,
{
    let mut observation = RemoteObservationRecord::new(
        entry.mount_id.clone(),
        entry.remote_id.clone(),
        entry.kind.clone(),
        entry.title.clone(),
        entry.path.clone(),
        observed_at,
    );
    if let Some(remote_version) = entry.remote_edited_at.clone() {
        observation = observation.with_remote_version(RemoteVersion::new(remote_version));
    }
    store.save_remote_observation(observation)?;

    let mut freshness = store
        .get_freshness_state(&entry.mount_id, &entry.remote_id)?
        .unwrap_or_else(|| {
            FreshnessStateRecord::new(
                entry.mount_id.clone(),
                entry.remote_id.clone(),
                initial_freshness_tier(existing),
            )
        });
    freshness.last_checked_at = Some(observed_at.to_string());
    freshness.remote_hint_pending =
        freshness.remote_hint_pending || remote_version_changed(existing, entry);
    store.save_freshness_state(freshness)?;

    Ok(())
}

fn initial_freshness_tier(existing: Option<&EntityRecord>) -> FreshnessTier {
    match existing.map(|entity| &entity.hydration) {
        Some(HydrationState::Dirty | HydrationState::Conflicted) => FreshnessTier::Hot,
        Some(HydrationState::Hydrated) => FreshnessTier::Warm,
        Some(HydrationState::Virtual | HydrationState::Stub) | None => FreshnessTier::Cold,
    }
}

fn remote_version_changed(existing: Option<&EntityRecord>, entry: &TreeEntry) -> bool {
    match (
        existing.and_then(|record| record.remote_edited_at.as_ref()),
        entry.remote_edited_at.as_ref(),
    ) {
        (Some(base), Some(observed)) => base != observed,
        _ => false,
    }
}

fn observation_timestamp() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => format!("unix_ms:{}", duration.as_millis()),
        Err(_) => "unix_ms:0".to_string(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectionWrite {
    Stub,
    Schema,
    None,
}

fn merged_entity_record(entry: &TreeEntry, existing: Option<&EntityRecord>) -> EntityRecord {
    let mut record = EntityRecord::from(entry.clone());

    if let Some(existing) = existing {
        record.hydration = existing.hydration.clone();
        record.content_hash = existing.content_hash.clone();
        if remote_precondition_belongs_to_shadow(existing) {
            record.remote_edited_at = existing.remote_edited_at.clone();
        }
    }

    record
}

fn refresh_projection<Source>(
    source: &Source,
    mount: &MountConfig,
    entry: &TreeEntry,
    state_root: Option<&Path>,
) -> LocalityResult<ProjectionWrite>
where
    Source: ScheduledPullSource + ?Sized,
{
    if mount.projection.uses_virtual_filesystem() {
        if entry.kind == EntityKind::Database
            && let Some(state_root) = state_root
            && let Some(schema) = source.database_schema_yaml(mount, &entry.remote_id)?
        {
            let directory = virtual_fs_content_root(state_root, &mount.mount_id).join(&entry.path);
            create_dir_all(&directory)?;
            write_atomic(&directory.join("_schema.yaml"), schema)?;
            return Ok(ProjectionWrite::Schema);
        }
        return Ok(ProjectionWrite::None);
    }

    match entry.kind {
        EntityKind::Page => {
            let path = mount.root.join(&entry.path);
            if path.exists() && !is_stub_file(&path)? {
                return Ok(ProjectionWrite::None);
            }

            write_atomic(&path, stub_markdown(entry)?)?;
            Ok(ProjectionWrite::Stub)
        }
        EntityKind::Database => {
            let directory = mount.root.join(&entry.path);
            create_dir_all(&directory)?;
            if let Some(schema) = source.database_schema_yaml(mount, &entry.remote_id)? {
                write_atomic(&directory.join("_schema.yaml"), schema)?;
                return Ok(ProjectionWrite::Schema);
            }
            Ok(ProjectionWrite::None)
        }
        EntityKind::Directory => {
            create_dir_all(&mount.root.join(&entry.path))?;
            Ok(ProjectionWrite::None)
        }
        EntityKind::Asset | EntityKind::Unknown(_) => Ok(ProjectionWrite::None),
    }
}

fn rename_projection_if_needed(
    mount: &MountConfig,
    existing: Option<&EntityRecord>,
    entry: &TreeEntry,
) -> LocalityResult<()> {
    if mount.projection.uses_virtual_filesystem() {
        return Ok(());
    }

    let Some(existing) = existing else {
        return Ok(());
    };
    if existing.path == entry.path {
        return Ok(());
    }

    match entry.kind {
        EntityKind::Page => {
            rename_page_projection_if_needed(mount, &existing.path, &entry.path)?;
        }
        EntityKind::Database
        | EntityKind::Directory
        | EntityKind::Asset
        | EntityKind::Unknown(_) => {
            rename_projected_path(
                &mount.root.join(&existing.path),
                &mount.root.join(&entry.path),
            )?;
        }
    }

    Ok(())
}

fn plan_reserved_notion_root_projection_moves(
    mount: &MountConfig,
    entries: &[TreeEntry],
    existing_entities: &[Option<EntityRecord>],
) -> LocalityResult<Vec<ReservedRootProjectionMove>> {
    if mount.projection.uses_virtual_filesystem() || !is_notion_workspace_mount(mount) {
        return Ok(Vec::new());
    }

    let mut reserved_stage_paths = BTreeSet::new();
    let mut reserved_source_paths = BTreeSet::new();
    let mut reserved_destination_paths = BTreeSet::new();
    let mut moves = Vec::new();
    for (entry, existing) in entries.iter().zip(existing_entities.iter()) {
        if entry.kind != EntityKind::Page {
            continue;
        }
        let Some(existing) = existing else {
            continue;
        };

        let Some(steps) = reserved_notion_root_page_move_steps(
            &existing.path,
            &entry.path,
            PathCaseComparison::CaseInsensitive,
        ) else {
            continue;
        };
        if !mount.root.join(&steps[0].0).join("page.md").exists() {
            continue;
        }
        if mount.root.join(&steps[1].1).exists() {
            return Err(LocalityError::Io(format!(
                "reserved Notion root projection destination `{}` already exists while moving `{}`",
                mount.root.join(&steps[1].1).display(),
                mount.root.join(&steps[0].0).display()
            )));
        }

        let stage_path =
            unique_upgrade_stage_path(&mount.root, &steps[0].1, &mut reserved_stage_paths)?;
        let source = steps[0].0.clone();
        let destination = steps[1].1.clone();
        if !reserved_source_paths.insert(source.clone())
            || !reserved_destination_paths.insert(destination.clone())
        {
            continue;
        }
        moves.push(ReservedRootProjectionMove {
            remote_id: entry.remote_id.clone(),
            source,
            stage: stage_path,
            destination,
        });
    }

    Ok(moves)
}

fn apply_reserved_notion_root_projection_moves(
    mount: &MountConfig,
    moves: &[ReservedRootProjectionMove],
) -> LocalityResult<()> {
    for planned_move in moves {
        if mount.root.join(&planned_move.destination).exists() {
            return Err(LocalityError::Io(format!(
                "reserved Notion root projection destination `{}` already exists while moving `{}`",
                mount.root.join(&planned_move.destination).display(),
                mount.root.join(&planned_move.source).display()
            )));
        }
    }

    for (index, planned_move) in moves.iter().enumerate() {
        if let Err(error) = rename_projected_path(
            &mount.root.join(&planned_move.source),
            &mount.root.join(&planned_move.stage),
        ) {
            rollback_staged_reserved_root_projection_moves(&mount.root, &moves[..index]);
            return Err(error);
        }
    }

    for (index, planned_move) in moves.iter().enumerate() {
        if let Err(error) = rename_projected_path(
            &mount.root.join(&planned_move.stage),
            &mount.root.join(&planned_move.destination),
        ) {
            rollback_reserved_root_projection_final_failure(&mount.root, &moves, index);
            return Err(error);
        }
    }

    ensure_reserved_notion_root_directories_after_projection_moves(mount, moves)?;

    Ok(())
}

fn ensure_reserved_notion_root_directories_after_projection_moves(
    mount: &MountConfig,
    moves: &[ReservedRootProjectionMove],
) -> LocalityResult<()> {
    if moves.is_empty()
        || mount.projection.uses_virtual_filesystem()
        || !is_notion_workspace_mount(mount)
    {
        return Ok(());
    }

    for root_name in [NOTION_PRIVATE_ROOT, NOTION_WORKSPACE_ROOT] {
        create_dir_all(&mount.root.join(root_name))?;
    }

    Ok(())
}

fn save_reserved_notion_root_projection_move_records<S>(
    store: &mut S,
    entries: &[TreeEntry],
    existing_entities: &[Option<EntityRecord>],
    moves: &[ReservedRootProjectionMove],
) -> LocalityResult<()>
where
    S: EntityRepository,
{
    for (entry, existing) in entries.iter().zip(existing_entities.iter()) {
        if reserved_root_projection_move_affects_record(entry, existing.as_ref(), moves) {
            store.save_entity(merged_entity_record(entry, existing.as_ref()))?;
        }
    }
    Ok(())
}

fn reserved_root_projection_move_affects_record(
    entry: &TreeEntry,
    existing: Option<&EntityRecord>,
    moves: &[ReservedRootProjectionMove],
) -> bool {
    if moves
        .iter()
        .any(|planned_move| planned_move.remote_id == entry.remote_id)
    {
        return true;
    }

    let Some(existing) = existing else {
        return false;
    };

    moves.iter().any(|planned_move| {
        path_is_within_subtree(
            &existing.path,
            &planned_move.source,
            PathCaseComparison::CaseInsensitive,
        ) && path_is_within_subtree(
            &entry.path,
            &planned_move.destination,
            PathCaseComparison::CaseInsensitive,
        )
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReservedRootProjectionMove {
    remote_id: RemoteId,
    source: PathBuf,
    stage: PathBuf,
    destination: PathBuf,
}

fn rollback_staged_reserved_root_projection_moves(
    root: &Path,
    moves: &[ReservedRootProjectionMove],
) {
    for planned_move in moves.iter().rev() {
        rollback_projected_path(root, &planned_move.stage, &planned_move.source);
    }
}

fn rollback_reserved_root_projection_final_failure(
    root: &Path,
    moves: &[ReservedRootProjectionMove],
    failed_final_index: usize,
) {
    for planned_move in moves[..failed_final_index].iter().rev() {
        rollback_projected_path(root, &planned_move.destination, &planned_move.source);
    }
    rollback_staged_reserved_root_projection_moves(root, &moves[failed_final_index..]);
}

fn rollback_projected_path(root: &Path, from: &Path, to: &Path) {
    let from = root.join(from);
    let to = root.join(to);
    if std::fs::rename(&from, &to).is_err() && to.is_dir() {
        let _ = std::fs::remove_dir(&to);
        let _ = std::fs::rename(from, to);
    }
}

fn is_notion_workspace_mount(mount: &MountConfig) -> bool {
    mount.connector == NOTION_CONNECTOR && mount.remote_root_id.is_none()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathCaseComparison {
    CaseInsensitive,
}

fn reserved_notion_root_page_move_steps(
    existing_path: &Path,
    entry_path: &Path,
    comparison: PathCaseComparison,
) -> Option<Vec<(PathBuf, PathBuf)>> {
    if !is_page_document_path(existing_path) || !is_page_document_path(entry_path) {
        return None;
    }

    let existing_container = page_container_path(existing_path);
    let entry_container = page_container_path(entry_path);
    if let Some(existing_root) =
        reserved_root_component(&existing_container, comparison).map(str::to_owned)
        && is_workspace_child_container(&entry_container, comparison)
    {
        return Some(reserved_notion_root_page_move_steps_from_containers(
            existing_container,
            &existing_root,
            entry_container,
        ));
    }

    if existing_path == entry_path {
        let existing_root =
            workspace_reserved_child_component(&entry_container, comparison)?.to_owned();
        return Some(reserved_notion_root_page_move_steps_from_containers(
            PathBuf::from(&existing_root),
            &existing_root,
            entry_container,
        ));
    }

    None
}

fn reserved_notion_root_page_move_steps_from_containers(
    existing_container: PathBuf,
    existing_root: &str,
    entry_container: PathBuf,
) -> Vec<(PathBuf, PathBuf)> {
    let stage_path = PathBuf::from(format!(
        "{UPGRADE_STAGE_PREFIX}-{}",
        existing_root.to_ascii_lowercase()
    ));
    vec![
        (existing_container, stage_path.clone()),
        (stage_path, entry_container.to_path_buf()),
    ]
}

fn reserved_root_component(path: &Path, comparison: PathCaseComparison) -> Option<&str> {
    let component = single_normal_component(path)?;
    if component_eq(component, NOTION_PRIVATE_ROOT, comparison)
        || component_eq(component, NOTION_WORKSPACE_ROOT, comparison)
    {
        return Some(component);
    }
    None
}

fn is_workspace_child_container(path: &Path, comparison: PathCaseComparison) -> bool {
    workspace_child_component(path, comparison).is_some()
}

fn workspace_reserved_child_component(path: &Path, comparison: PathCaseComparison) -> Option<&str> {
    let child = workspace_child_component(path, comparison)?;
    if component_eq(child, NOTION_PRIVATE_ROOT, comparison)
        || component_eq(child, NOTION_WORKSPACE_ROOT, comparison)
    {
        return Some(child);
    }
    None
}

fn workspace_child_component(path: &Path, comparison: PathCaseComparison) -> Option<&str> {
    let mut components = path.components();
    let Some(Component::Normal(first)) = components.next() else {
        return None;
    };
    let Some(first) = first.to_str() else {
        return None;
    };
    if !component_eq(first, NOTION_WORKSPACE_ROOT, comparison) {
        return None;
    }
    let Some(Component::Normal(child)) = components.next() else {
        return None;
    };
    if components.next().is_some() {
        return None;
    }
    child.to_str()
}

fn single_normal_component(path: &Path) -> Option<&str> {
    let mut components = path.components();
    let Some(Component::Normal(component)) = components.next() else {
        return None;
    };
    if components.next().is_some() {
        return None;
    }
    component.to_str()
}

fn component_eq(left: &str, right: &str, comparison: PathCaseComparison) -> bool {
    match comparison {
        PathCaseComparison::CaseInsensitive => left.eq_ignore_ascii_case(right),
    }
}

fn path_is_within_subtree(path: &Path, subtree: &Path, comparison: PathCaseComparison) -> bool {
    let mut path_components = path.components();
    for subtree_component in subtree.components() {
        let Some(path_component) = path_components.next() else {
            return false;
        };
        if !path_component_eq(path_component, subtree_component, comparison) {
            return false;
        }
    }
    true
}

fn path_component_eq(
    left: Component<'_>,
    right: Component<'_>,
    comparison: PathCaseComparison,
) -> bool {
    match (left, right) {
        (Component::Normal(left), Component::Normal(right)) => {
            let (Some(left), Some(right)) = (left.to_str(), right.to_str()) else {
                return left == right;
            };
            component_eq(left, right, comparison)
        }
        (left, right) => left == right,
    }
}

fn unique_upgrade_stage_path(
    root: &Path,
    base: &Path,
    reserved: &mut BTreeSet<PathBuf>,
) -> LocalityResult<PathBuf> {
    if !root.join(base).exists() && reserved.insert(base.to_path_buf()) {
        return Ok(base.to_path_buf());
    }

    for index in 1..1000 {
        let candidate = PathBuf::from(format!("{}-{index}", base.display()));
        if !root.join(&candidate).exists() && reserved.insert(candidate.clone()) {
            return Ok(candidate);
        }
    }

    Err(LocalityError::Io(format!(
        "failed to choose a temporary projection upgrade path for `{}`",
        base.display()
    )))
}

fn rename_page_projection_if_needed(
    mount: &MountConfig,
    existing_path: &Path,
    entry_path: &Path,
) -> LocalityResult<()> {
    if existing_path == entry_path {
        return Ok(());
    }

    if is_page_document_path(existing_path) {
        let existing_container = page_container_path(existing_path);
        let entry_container = page_container_path(entry_path);
        if existing_container != entry_container {
            rename_projected_path(
                &mount.root.join(existing_container),
                &mount.root.join(entry_container),
            )?;
        } else {
            rename_projected_path(
                &mount.root.join(existing_path),
                &mount.root.join(entry_path),
            )?;
        }
        return Ok(());
    }

    let existing_file = mount.root.join(existing_path);
    let legacy_child_dir = mount.root.join(page_container_path(existing_path));
    let entry_container = mount.root.join(page_container_path(entry_path));
    let entry_file = mount.root.join(entry_path);

    if legacy_child_dir.exists() && legacy_child_dir != entry_container {
        rename_projected_path(&legacy_child_dir, &entry_container)?;
    } else if !entry_container.exists() {
        create_dir_all(&entry_container)?;
    }

    rename_projected_path(&existing_file, &entry_file)?;
    Ok(())
}

fn policy_hydration() -> EntityFetchPlan {
    EntityFetchPlan {
        queue_hydration: Some(HydrationReason::Policy),
    }
}

fn remote_fast_forward_hydration() -> EntityFetchPlan {
    EntityFetchPlan {
        queue_hydration: Some(HydrationReason::RemoteFastForward),
    }
}

fn is_remote_root_entry(mount: &MountConfig, entry: &TreeEntry) -> bool {
    mount
        .remote_root_id
        .as_ref()
        .is_some_and(|remote_root_id| remote_root_id == &entry.remote_id)
}

fn should_refresh_hydrated_entity(existing: &EntityRecord, entry: &TreeEntry) -> bool {
    existing.hydration == HydrationState::Hydrated
        && existing.remote_edited_at.is_some()
        && entry.remote_edited_at.is_some()
        && existing.remote_edited_at != entry.remote_edited_at
}

fn remote_precondition_belongs_to_shadow(existing: &EntityRecord) -> bool {
    matches!(
        existing.hydration,
        HydrationState::Hydrated | HydrationState::Dirty | HydrationState::Conflicted
    )
}

fn is_stub_file(path: &Path) -> LocalityResult<bool> {
    let contents = read_to_string(path)?;
    let Ok(parsed) = parse_canonical_markdown(&contents) else {
        return Ok(false);
    };

    Ok(parsed.document.is_stub())
}

fn stub_markdown(entry: &TreeEntry) -> LocalityResult<String> {
    let document = CanonicalDocument::new(
        entry
            .stub_frontmatter
            .clone()
            .unwrap_or_else(|| stub_frontmatter(entry)),
        format!("{}\n", CanonicalDocument::STUB_MARKER),
    );
    Ok(render_canonical_markdown(&document))
}

fn stub_frontmatter(entry: &TreeEntry) -> String {
    format!(
        "loc:\n  id: {}\n  type: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\n",
        entry.remote_id.0,
        entity_type_name(&entry.kind),
        yaml_string(entry.remote_edited_at.as_deref().unwrap_or("unknown")),
        yaml_string(entry.remote_edited_at.as_deref().unwrap_or("unknown")),
        yaml_string(&entry.title)
    )
}

fn entity_type_name(kind: &EntityKind) -> &'static str {
    match kind {
        EntityKind::Page => "page",
        EntityKind::Database => "database",
        EntityKind::Directory => "directory",
        EntityKind::Asset => "asset",
        EntityKind::Unknown(_) => "unknown",
    }
}

fn yaml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn write_atomic(path: &Path, contents: String) -> LocalityResult<()> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("loc-write");
    let temp_path = path.with_file_name(format!(".{file_name}.loc-tmp"));

    std::fs::write(&temp_path, contents).map_err(|error| {
        LocalityError::Io(format!(
            "failed to write scheduled pull temp file `{}`: {error}",
            temp_path.display()
        ))
    })?;
    std::fs::rename(&temp_path, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp_path);
        LocalityError::Io(format!(
            "failed to replace scheduled pull projection `{}`: {error}",
            path.display()
        ))
    })?;

    Ok(())
}

fn create_dir_all(path: &Path) -> LocalityResult<()> {
    std::fs::create_dir_all(path).map_err(|error| {
        LocalityError::Io(format!(
            "failed to create scheduled pull directory `{}`: {error}",
            path.display()
        ))
    })
}

fn rename_projected_path(from: &Path, to: &Path) -> LocalityResult<()> {
    if from == to || !from.exists() || to.exists() {
        return Ok(());
    }

    if let Some(parent) = to.parent() {
        create_dir_all(parent)?;
    }

    std::fs::rename(from, to).map_err(|error| {
        LocalityError::Io(format!(
            "failed to rename scheduled pull projection `{}` to `{}`: {error}",
            from.display(),
            to.display(),
        ))
    })
}

fn read_to_string(path: &Path) -> LocalityResult<String> {
    std::fs::read_to_string(path)
        .map_err(|error| LocalityError::Io(format!("failed to read `{}`: {error}", path.display())))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use locality_core::model::{MountId, RemoteId};
    use locality_store::MountConfig;

    #[test]
    fn reserved_root_move_recreates_synthetic_root_directories_after_batch() {
        let root = temp_root("reconcile-reserved-root-synthetic-roots");
        let mount_id = MountId::new("notion-main");
        let mount = MountConfig::new(mount_id, "notion", root.clone());
        std::fs::create_dir_all(root.join("private")).expect("old container");
        std::fs::write(root.join("private/page.md"), "local body").expect("old page");
        let moves = vec![super::ReservedRootProjectionMove {
            remote_id: RemoteId::new("page-1"),
            source: PathBuf::from("private"),
            stage: PathBuf::from(".loc-upgrade-stage-private"),
            destination: PathBuf::from("Workspace/private"),
        }];

        super::apply_reserved_notion_root_projection_moves(&mount, &moves).expect("apply moves");

        assert_eq!(
            std::fs::read_to_string(root.join("Workspace/private/page.md")).expect("moved page"),
            "local body"
        );
        assert!(root.join("Private").is_dir());
        assert!(root.join("Workspace").is_dir());

        let _ = std::fs::remove_dir_all(root);
    }

    fn temp_root(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("loc-{label}-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp root");
        root
    }
}
