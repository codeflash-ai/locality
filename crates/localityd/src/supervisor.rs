use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use locality_core::LocalityResult;
use locality_core::conflict::has_unresolved_conflict_markers;
use locality_core::freshness::FreshnessTier;
use locality_core::hydration::{HydrationReason, HydrationRequest};
use locality_core::journal::JournalStore;
use locality_core::model::{EntityKind, HydrationState};
use locality_store::{
    EntityRecord, EntityRepository, FreshnessStateRepository, JournalRepository, MountConfig,
    MountRepository, RemoteObservationRepository, ShadowRepository, VirtualMutationRepository,
};

use crate::execution::{
    AdvanceScheduledPullJob, DaemonEventReport, DaemonExecutor, HydrationDrainJob,
    HydrationRequestJob, HydrationRequestReport, PushJob, PushJobReport, ScheduledPullJob,
};
use crate::hydration::{HydrationEngine, HydrationExecutor, HydrationQueue, HydrationSource};
use crate::push::execute_push_job;
use crate::reconcile::{
    FetchScheduleStrategy, ScheduledPullReport, ScheduledPullSource, reconcile_scheduled_pull,
};
use crate::scheduler::{PullScheduler, PullSchedulerTick};
use crate::watcher::{FileEvent, FileEventKind, FileWatcher};

#[derive(Clone, Debug)]
pub struct DaemonSupervisor<S, W, H> {
    store: S,
    watcher: W,
    hydration: H,
    scheduler: PullScheduler,
    mounts: Vec<MountConfig>,
}

impl<S, W, H> DaemonSupervisor<S, W, H>
where
    S: MountRepository + EntityRepository + FreshnessStateRepository,
    W: FileWatcher,
    H: HydrationEngine,
{
    pub fn new(store: S, watcher: W, hydration: H, scheduler: PullScheduler) -> Self {
        Self {
            store,
            watcher,
            hydration,
            scheduler,
            mounts: Vec::new(),
        }
    }

    pub fn start(&mut self) -> LocalityResult<DaemonStartReport> {
        self.mounts = self.store.load_mounts()?;

        let mut watched_mounts = 0;
        for mount in &self.mounts {
            if !should_watch_mount(mount) {
                continue;
            }
            self.watcher.watch_mount(mount.root.clone())?;
            watched_mounts += 1;
        }

        Ok(DaemonStartReport { watched_mounts })
    }

    fn apply_file_event(&mut self, event: FileEvent) -> LocalityResult<DaemonEventReport> {
        let mut report = DaemonEventReport::default();
        let Some((mount, entity)) = self.resolve_event_entity(&event.path)? else {
            report.ignored_events = 1;
            return Ok(report);
        };

        match event.kind {
            FileEventKind::Read => {
                record_file_opened(&mut self.store, &entity)?;
                if should_hydrate_on_read(&entity) {
                    let request = HydrationRequest::new(
                        mount.mount_id.clone(),
                        entity.remote_id,
                        mount.root.join(&entity.path),
                        HydrationState::Hydrated,
                        HydrationReason::StubRead,
                    );
                    self.hydration.queue(request)?;
                    report.queued_hydrations = 1;
                } else {
                    report.ignored_events = 1;
                }
            }
            FileEventKind::Write => {
                let next_state = write_event_hydration_state(&event.path, &entity);
                if entity.hydration.can_transition_to(&next_state) {
                    let mut updated = entity;
                    updated.hydration = next_state;
                    self.store.save_entity(updated.clone())?;
                    record_local_change(&mut self.store, &updated)?;
                    report.marked_dirty = 1;
                } else {
                    report.ignored_events = 1;
                }
            }
            FileEventKind::Rename | FileEventKind::Remove => {
                report.ignored_events = 1;
            }
        }

        Ok(report)
    }

    pub fn tick_scheduler(&mut self, elapsed: Duration) -> LocalityResult<PullSchedulerTick> {
        self.scheduler.advance_by(elapsed)
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    pub fn watcher(&self) -> &W {
        &self.watcher
    }

    pub fn hydration(&self) -> &H {
        &self.hydration
    }

    pub fn mounts(&self) -> &[MountConfig] {
        &self.mounts
    }

    pub fn into_parts(self) -> (S, W, H, PullScheduler) {
        (self.store, self.watcher, self.hydration, self.scheduler)
    }

    fn resolve_event_entity(
        &self,
        event_path: &Path,
    ) -> LocalityResult<Option<(MountConfig, EntityRecord)>> {
        for mount in &self.mounts {
            if !should_watch_mount(mount) {
                continue;
            }
            let Some(relative_path) = event_relative_path(&mount.root, event_path) else {
                continue;
            };
            if relative_path.as_os_str().is_empty() {
                continue;
            }

            if let Some(entity) = self
                .store
                .find_entity_by_path(&mount.mount_id, &relative_path)?
            {
                return Ok(Some((mount.clone(), entity)));
            }
        }

        if self.mounts.len() == 1
            && event_path.is_relative()
            && let Some(mount) = self.mounts.first()
            && should_watch_mount(mount)
            && let Some(entity) = self
                .store
                .find_entity_by_path(&mount.mount_id, event_path)?
        {
            return Ok(Some((mount.clone(), entity)));
        }

        Ok(None)
    }
}

fn write_event_hydration_state(path: &Path, entity: &EntityRecord) -> HydrationState {
    if entity.kind != EntityKind::Page {
        return HydrationState::Dirty;
    }

    match std::fs::read_to_string(path) {
        Ok(contents) if has_unresolved_conflict_markers(&contents) => HydrationState::Conflicted,
        _ => HydrationState::Dirty,
    }
}

fn record_file_opened<S>(store: &mut S, entity: &EntityRecord) -> LocalityResult<()>
where
    S: FreshnessStateRepository,
{
    update_freshness_state(store, entity, |state, now| {
        promote_tier(state, FreshnessTier::Hot);
        state.last_opened_at = Some(now);
    })
}

fn record_local_change<S>(store: &mut S, entity: &EntityRecord) -> LocalityResult<()>
where
    S: FreshnessStateRepository,
{
    update_freshness_state(store, entity, |state, now| {
        promote_tier(state, FreshnessTier::Hot);
        state.last_local_change_at = Some(now);
    })
}

fn update_freshness_state<S, F>(
    store: &mut S,
    entity: &EntityRecord,
    update: F,
) -> LocalityResult<()>
where
    S: FreshnessStateRepository,
    F: FnOnce(&mut locality_store::FreshnessStateRecord, String),
{
    let mut state = store
        .get_freshness_state(&entity.mount_id, &entity.remote_id)?
        .unwrap_or_else(|| {
            locality_store::FreshnessStateRecord::new(
                entity.mount_id.clone(),
                entity.remote_id.clone(),
                default_freshness_tier(entity),
            )
        });
    update(&mut state, freshness_timestamp());
    store.save_freshness_state(state)?;
    Ok(())
}

fn promote_tier(state: &mut locality_store::FreshnessStateRecord, tier: FreshnessTier) {
    if tier.is_more_urgent_than(&state.tier) {
        state.tier = tier;
    }
}

fn default_freshness_tier(entity: &EntityRecord) -> FreshnessTier {
    match entity.hydration {
        HydrationState::Dirty | HydrationState::Conflicted => FreshnessTier::Hot,
        HydrationState::Hydrated => FreshnessTier::Warm,
        HydrationState::Virtual | HydrationState::Stub => FreshnessTier::Cold,
    }
}

fn freshness_timestamp() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => format!("unix_ms:{}", duration.as_millis()),
        Err(_) => "unix_ms:0".to_string(),
    }
}

impl<S, W> DaemonExecutor for DaemonSupervisor<S, W, HydrationQueue>
where
    S: MountRepository
        + EntityRepository
        + RemoteObservationRepository
        + FreshnessStateRepository
        + ShadowRepository
        + JournalRepository
        + JournalStore
        + VirtualMutationRepository,
    W: FileWatcher,
{
    fn execute_file_event(&mut self, event: FileEvent) -> LocalityResult<DaemonEventReport> {
        self.apply_file_event(event)
    }

    fn execute_scheduled_pull<Source, Strategy>(
        &mut self,
        job: ScheduledPullJob,
        source: &Source,
        strategy: &Strategy,
    ) -> LocalityResult<ScheduledPullReport>
    where
        Source: ScheduledPullSource + ?Sized,
        Strategy: FetchScheduleStrategy + ?Sized,
    {
        let mounts = self.mounts.clone();
        let policy = self.scheduler.config.hydration_policy.clone();
        reconcile_scheduled_pull(
            &mut self.store,
            &mut self.hydration,
            &mounts,
            &job.tick,
            source,
            strategy,
            &policy,
        )
    }

    fn advance_and_execute_scheduled_pull<Source, Strategy>(
        &mut self,
        job: AdvanceScheduledPullJob,
        source: &Source,
        strategy: &Strategy,
    ) -> LocalityResult<ScheduledPullReport>
    where
        Source: ScheduledPullSource + ?Sized,
        Strategy: FetchScheduleStrategy + ?Sized,
    {
        let tick = self.tick_scheduler(job.elapsed)?;
        self.execute_scheduled_pull(ScheduledPullJob::new(tick), source, strategy)
    }

    fn execute_hydration_request<Source>(
        &mut self,
        job: HydrationRequestJob,
        source: &Source,
    ) -> LocalityResult<HydrationRequestReport>
    where
        Source: HydrationSource + ?Sized,
    {
        let mut executor = HydrationExecutor::new(&mut self.store, source);
        let outcome = executor.hydrate_request(job.request)?;
        Ok(HydrationRequestReport { outcome })
    }

    fn execute_hydration_drain<Source>(
        &mut self,
        _job: HydrationDrainJob,
        source: &Source,
    ) -> LocalityResult<crate::hydration::HydrationDrainReport>
    where
        Source: HydrationSource + ?Sized,
    {
        let mut executor = HydrationExecutor::new(&mut self.store, source);
        executor.drain_queue(&mut self.hydration)
    }

    fn execute_push<Source>(
        &mut self,
        job: PushJob,
        source: &Source,
    ) -> LocalityResult<PushJobReport>
    where
        Source: locality_connector::Connector + HydrationSource + ?Sized,
    {
        execute_push_job(&mut self.store, job, source)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonStartReport {
    pub watched_mounts: usize,
}

fn event_relative_path(root: &Path, event_path: &Path) -> Option<PathBuf> {
    event_path.strip_prefix(root).ok().map(Path::to_path_buf)
}

fn should_hydrate_on_read(entity: &EntityRecord) -> bool {
    if entity.kind != EntityKind::Page {
        return false;
    }

    matches!(
        entity.hydration,
        HydrationState::Virtual | HydrationState::Stub
    )
}

fn should_watch_mount(mount: &MountConfig) -> bool {
    !mount.projection.uses_virtual_filesystem()
}
