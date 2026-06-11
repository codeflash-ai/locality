use std::path::{Path, PathBuf};
use std::time::Duration;

use afs_core::hydration::{HydrationReason, HydrationRequest};
use afs_core::model::HydrationState;
use afs_core::{AfsError, AfsResult};
use afs_store::{EntityRecord, EntityRepository, MountConfig, MountRepository, ShadowRepository};

use crate::execution::{
    AdvanceScheduledPullJob, DaemonEventReport, DaemonExecutor, HydrationDrainJob,
    HydrationRequestJob, HydrationRequestReport, PushJob, PushJobReport, ScheduledPullJob,
};
use crate::hydration::{HydrationEngine, HydrationExecutor, HydrationQueue, HydrationSource};
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
    S: MountRepository + EntityRepository,
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

    pub fn start(&mut self) -> AfsResult<DaemonStartReport> {
        self.mounts = self.store.load_mounts()?;

        for mount in &self.mounts {
            self.watcher.watch_mount(mount.root.clone())?;
        }

        Ok(DaemonStartReport {
            watched_mounts: self.mounts.len(),
        })
    }

    fn apply_file_event(&mut self, event: FileEvent) -> AfsResult<DaemonEventReport> {
        let mut report = DaemonEventReport::default();
        let Some((mount, entity)) = self.resolve_event_entity(&event.path)? else {
            report.ignored_events = 1;
            return Ok(report);
        };

        match event.kind {
            FileEventKind::Read => {
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
                if entity.hydration.can_transition_to(&HydrationState::Dirty) {
                    let mut updated = entity;
                    updated.hydration = HydrationState::Dirty;
                    self.store.save_entity(updated)?;
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

    pub fn tick_scheduler(&mut self, elapsed: Duration) -> AfsResult<PullSchedulerTick> {
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
    ) -> AfsResult<Option<(MountConfig, EntityRecord)>> {
        for mount in &self.mounts {
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
            && let Some(entity) = self
                .store
                .find_entity_by_path(&mount.mount_id, event_path)?
        {
            return Ok(Some((mount.clone(), entity)));
        }

        Ok(None)
    }
}

impl<S, W> DaemonExecutor for DaemonSupervisor<S, W, HydrationQueue>
where
    S: MountRepository + EntityRepository + ShadowRepository,
    W: FileWatcher,
{
    fn execute_file_event(&mut self, event: FileEvent) -> AfsResult<DaemonEventReport> {
        self.apply_file_event(event)
    }

    fn execute_scheduled_pull<Source, Strategy>(
        &mut self,
        job: ScheduledPullJob,
        source: &Source,
        strategy: &Strategy,
    ) -> AfsResult<ScheduledPullReport>
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
    ) -> AfsResult<ScheduledPullReport>
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
    ) -> AfsResult<HydrationRequestReport>
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
    ) -> AfsResult<crate::hydration::HydrationDrainReport>
    where
        Source: HydrationSource + ?Sized,
    {
        let mut executor = HydrationExecutor::new(&mut self.store, source);
        executor.drain_queue(&mut self.hydration)
    }

    fn execute_push(&mut self, _job: PushJob) -> AfsResult<PushJobReport> {
        Err(AfsError::NotImplemented("daemon push execution"))
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
    matches!(
        entity.hydration,
        HydrationState::Virtual | HydrationState::Stub
    )
}
