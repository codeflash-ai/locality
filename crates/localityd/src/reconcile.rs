//! Scheduled remote reconciliation for daemon-managed mounts.
//!
//! The daemon keeps scheduling policy separate from reconciliation mechanics:
//! a strategy decides what to fetch for a mount, and this module executes that
//! decision by enumerating, refreshing local projections, and queueing hydration.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
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
use crate::virtual_fs::{repair_legacy_macos_content_root, virtual_fs_content_root};

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

        let remote_move_plan = remote_move_plan(store, mount, &entries)?;
        for entry in &entries {
            let existing = store.get_entity(&entry.mount_id, &entry.remote_id)?;
            let observed_at = observation_timestamp();
            record_remote_observation(store, entry, existing.as_ref(), &observed_at)?;
            let preserve_existing_path = remote_move_plan.should_preserve(&entry.remote_id);
            let entity_plan = if preserve_existing_path {
                EntityFetchPlan::default()
            } else {
                strategy.entity_plan(EntityFetchSchedule {
                    mount,
                    entry,
                    existing: existing.as_ref(),
                    page_count,
                    tick,
                    policy,
                })
            };

            let record = merged_entity_record(entry, existing.as_ref(), preserve_existing_path);
            let projected_entry = TreeEntry {
                path: record.path.clone(),
                ..entry.clone()
            };
            store.save_entity(record)?;
            rename_projection_if_needed(mount, existing.as_ref(), &projected_entry)?;

            match refresh_projection(source, mount, &projected_entry, state_root)? {
                ProjectionWrite::Stub => report.stubbed += 1,
                ProjectionWrite::Schema => report.schemas_written += 1,
                ProjectionWrite::None => {}
            }

            if let Some(reason) = entity_plan.queue_hydration {
                hydration.queue(HydrationRequest::new(
                    mount.mount_id.clone(),
                    projected_entry.remote_id.clone(),
                    mount.root.join(&projected_entry.path),
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

fn merged_entity_record(
    entry: &TreeEntry,
    existing: Option<&EntityRecord>,
    preserve_existing_path: bool,
) -> EntityRecord {
    let mut record = EntityRecord::from(entry.clone());

    if let Some(existing) = existing {
        if preserve_existing_path {
            record.path = existing.path.clone();
        }
        record.hydration = existing.hydration.clone();
        if preserve_existing_path && record.hydration.can_transition_to(&HydrationState::Dirty) {
            record.hydration = HydrationState::Dirty;
        }
        record.content_hash = existing.content_hash.clone();
        if remote_precondition_belongs_to_shadow(existing) {
            record.remote_edited_at = existing.remote_edited_at.clone();
        }
    }

    record
}

#[derive(Debug, Default)]
struct RemoteMovePlan {
    preserve_remote_ids: BTreeSet<RemoteId>,
}

impl RemoteMovePlan {
    fn should_preserve(&self, remote_id: &RemoteId) -> bool {
        self.preserve_remote_ids.contains(remote_id)
    }
}

#[derive(Debug)]
struct BlockedProjectionMove {
    source_root: PathBuf,
    destination_root: PathBuf,
}

fn remote_move_plan<S>(
    store: &S,
    mount: &MountConfig,
    entries: &[TreeEntry],
) -> LocalityResult<RemoteMovePlan>
where
    S: EntityRepository,
{
    let existing_entities = store.list_entities(&mount.mount_id)?;
    let existing_by_remote_id = existing_entities
        .iter()
        .map(|entity| (entity.remote_id.clone(), entity))
        .collect::<BTreeMap<_, _>>();
    let mut blocked_moves = Vec::new();

    for entry in entries {
        let Some(existing) = existing_by_remote_id.get(&entry.remote_id) else {
            continue;
        };
        if existing.path == entry.path {
            continue;
        }

        let source_root = projection_subtree_path(&existing.kind, &existing.path);
        if scheduled_move_subtree_is_dirty(&existing_entities, &source_root) {
            blocked_moves.push(BlockedProjectionMove {
                source_root,
                destination_root: projection_subtree_path(&entry.kind, &entry.path),
            });
        }
    }

    let mut preserve_remote_ids = BTreeSet::new();
    for entry in entries {
        let Some(existing) = existing_by_remote_id.get(&entry.remote_id) else {
            continue;
        };
        if should_preserve_for_blocked_move(existing, entry, &blocked_moves) {
            preserve_remote_ids.insert(entry.remote_id.clone());
        }
    }

    Ok(RemoteMovePlan {
        preserve_remote_ids,
    })
}

fn scheduled_move_subtree_is_dirty(existing_entities: &[EntityRecord], source_root: &Path) -> bool {
    existing_entities.iter().any(|entity| {
        path_in_projection_subtree(
            &projection_subtree_path(&entity.kind, &entity.path),
            source_root,
        ) && matches!(
            entity.hydration,
            HydrationState::Dirty | HydrationState::Conflicted
        )
    })
}

fn should_preserve_for_blocked_move(
    existing: &EntityRecord,
    entry: &TreeEntry,
    blocked_moves: &[BlockedProjectionMove],
) -> bool {
    if existing.path == entry.path {
        return false;
    }

    let existing_root = projection_subtree_path(&existing.kind, &existing.path);
    let entry_root = projection_subtree_path(&entry.kind, &entry.path);
    blocked_moves.iter().any(|blocked| {
        path_in_projection_subtree(&existing_root, &blocked.source_root)
            && path_in_projection_subtree(&entry_root, &blocked.destination_root)
    })
}

fn projection_subtree_path(kind: &EntityKind, path: &Path) -> PathBuf {
    match kind {
        EntityKind::Page => page_container_path(path),
        EntityKind::Database
        | EntityKind::Directory
        | EntityKind::Asset
        | EntityKind::Unknown(_) => path.to_path_buf(),
    }
}

fn path_in_projection_subtree(path: &Path, subtree: &Path) -> bool {
    subtree.as_os_str().is_empty() || path == subtree || path.starts_with(subtree)
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
        {
            repair_legacy_macos_content_root(state_root, &mount.mount_id)?;
            let directory = virtual_fs_content_root(state_root, &mount.mount_id).join(&entry.path);
            if directory.join("_schema.yaml").exists() {
                return Ok(ProjectionWrite::Schema);
            }
            if let Some(schema) = source.database_schema_yaml(mount, &entry.remote_id)? {
                create_dir_all(&directory)?;
                write_atomic(&directory.join("_schema.yaml"), schema)?;
                return Ok(ProjectionWrite::Schema);
            }
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
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn virtual_schema_refresh_repairs_legacy_app_group_cache_before_write() {
        use locality_core::model::MountId;
        use locality_store::ProjectionMode;

        struct SchemaSource;

        impl ScheduledPullSource for SchemaSource {
            fn enumerate_mount(&self, _mount: &MountConfig) -> LocalityResult<Vec<TreeEntry>> {
                Ok(Vec::new())
            }

            fn database_schema_yaml(
                &self,
                _mount: &MountConfig,
                _remote_id: &RemoteId,
            ) -> LocalityResult<Option<String>> {
                Ok(Some("remote schema\n".to_string()))
            }
        }

        let home = std::env::temp_dir().join(format!(
            "loc-reconcile-schema-legacy-home-{}",
            std::process::id()
        ));
        let state_root = home.join(".loc");
        let visible_root = home.join("visible");
        let mount_id = MountId::new("notion-main");
        let mount = MountConfig::new(mount_id.clone(), "notion", &visible_root)
            .projection(ProjectionMode::LinuxFuse);
        let entry = TreeEntry {
            mount_id: mount_id.clone(),
            remote_id: RemoteId::new("database-1"),
            kind: EntityKind::Database,
            title: "Tasks".to_string(),
            path: PathBuf::from("Tasks"),
            hydration: HydrationState::Stub,
            content_hash: None,
            remote_edited_at: None,
            stub_frontmatter: None,
        };
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

        let write = refresh_projection(&SchemaSource, &mount, &entry, Some(&state_root))
            .expect("refresh projection");

        assert_eq!(write, ProjectionWrite::Schema);
        assert_eq!(
            std::fs::read_to_string(&current_schema_path).expect("read current schema"),
            "legacy schema\n"
        );
        let _ = std::fs::remove_dir_all(home);
    }
}
