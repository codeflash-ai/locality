//! Scheduled remote reconciliation for daemon-managed mounts.
//!
//! The daemon keeps scheduling policy separate from reconciliation mechanics:
//! a strategy decides what to fetch for a mount, and this module executes that
//! decision by enumerating, refreshing local projections, and queueing hydration.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use afs_connector::{Connector, EnumerateRequest};
use afs_core::canonical::{parse_canonical_markdown, render_canonical_markdown};
use afs_core::freshness::{FreshnessTier, RemoteVersion};
use afs_core::hydration::{
    HydrationPolicy, HydrationReason, HydrationRequest, should_eager_hydrate,
};
use afs_core::model::{CanonicalDocument, EntityKind, HydrationState, RemoteId, TreeEntry};
use afs_core::path_projection::{is_page_document_path, page_container_path};
use afs_core::{AfsError, AfsResult};
use afs_notion::NotionConnector;
use afs_store::{
    EntityRecord, EntityRepository, FreshnessStateRecord, FreshnessStateRepository, MountConfig,
    RemoteObservationRecord, RemoteObservationRepository,
};

use crate::hydration::HydrationEngine;
use crate::scheduler::PullSchedulerTick;
use crate::virtual_fs::virtual_fs_content_root;

pub trait ScheduledPullSource {
    fn enumerate_mount(&self, mount: &MountConfig) -> AfsResult<Vec<TreeEntry>>;

    fn database_schema_yaml(
        &self,
        _mount: &MountConfig,
        _remote_id: &RemoteId,
    ) -> AfsResult<Option<String>> {
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
) -> AfsResult<ScheduledPullReport>
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
) -> AfsResult<ScheduledPullReport>
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

        for entry in &entries {
            let existing = store.get_entity(&entry.mount_id, &entry.remote_id)?;
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
            store.save_entity(record)?;
            rename_projection_if_needed(mount, existing.as_ref(), entry)?;

            match refresh_projection(source, mount, entry, state_root)? {
                ProjectionWrite::Stub => report.stubbed += 1,
                ProjectionWrite::Schema => report.schemas_written += 1,
                ProjectionWrite::None => {}
            }

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
) -> AfsResult<()>
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

impl ScheduledPullSource for NotionConnector {
    fn enumerate_mount(&self, mount: &MountConfig) -> AfsResult<Vec<TreeEntry>> {
        let connector = match &mount.remote_root_id {
            Some(root_page_id) => self.with_root_page_id(root_page_id.clone()),
            None => self.clone(),
        };

        connector.enumerate(EnumerateRequest {
            mount_id: mount.mount_id.clone(),
            cursor: None,
        })
    }

    fn database_schema_yaml(
        &self,
        _mount: &MountConfig,
        remote_id: &RemoteId,
    ) -> AfsResult<Option<String>> {
        self.database_schema_yaml(remote_id).map(Some)
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
) -> AfsResult<ProjectionWrite>
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
) -> AfsResult<()> {
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

fn rename_page_projection_if_needed(
    mount: &MountConfig,
    existing_path: &Path,
    entry_path: &Path,
) -> AfsResult<()> {
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

fn is_stub_file(path: &Path) -> AfsResult<bool> {
    let contents = read_to_string(path)?;
    let Ok(parsed) = parse_canonical_markdown(&contents) else {
        return Ok(false);
    };

    Ok(parsed.document.is_stub())
}

fn stub_markdown(entry: &TreeEntry) -> AfsResult<String> {
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
        "afs:\n  id: {}\n  type: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\n",
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

fn write_atomic(path: &Path, contents: String) -> AfsResult<()> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("afs-write");
    let temp_path = path.with_file_name(format!(".{file_name}.afs-tmp"));

    std::fs::write(&temp_path, contents).map_err(|error| {
        AfsError::Io(format!(
            "failed to write scheduled pull temp file `{}`: {error}",
            temp_path.display()
        ))
    })?;
    std::fs::rename(&temp_path, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp_path);
        AfsError::Io(format!(
            "failed to replace scheduled pull projection `{}`: {error}",
            path.display()
        ))
    })?;

    Ok(())
}

fn create_dir_all(path: &Path) -> AfsResult<()> {
    std::fs::create_dir_all(path).map_err(|error| {
        AfsError::Io(format!(
            "failed to create scheduled pull directory `{}`: {error}",
            path.display()
        ))
    })
}

fn rename_projected_path(from: &Path, to: &Path) -> AfsResult<()> {
    if from == to || !from.exists() || to.exists() {
        return Ok(());
    }

    if let Some(parent) = to.parent() {
        create_dir_all(parent)?;
    }

    std::fs::rename(from, to).map_err(|error| {
        AfsError::Io(format!(
            "failed to rename scheduled pull projection `{}` to `{}`: {error}",
            from.display(),
            to.display(),
        ))
    })
}

fn read_to_string(path: &Path) -> AfsResult<String> {
    std::fs::read_to_string(path)
        .map_err(|error| AfsError::Io(format!("failed to read `{}`: {error}", path.display())))
}
