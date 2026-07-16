//! Daemon-side mount pre-hydration enumeration.
//!
//! Pre-hydration uses scheduled-pull reconciliation for durable entity writes,
//! projection stubs, database schema, and hydration queue insertion. This
//! module only supplies the strategy that broadens enumeration and queues page
//! bodies as low-priority prefetch work.

use std::cell::Cell;
use std::path::Path;

use locality_core::hydration::HydrationPolicy;
use locality_core::hydration::HydrationReason;
use locality_core::model::{EntityKind, RemoteId, TreeEntry};
use locality_core::{LocalityError, LocalityResult};
use locality_store::{
    ConnectorStateRepository, EntityRepository, FreshnessStateRepository, MountConfig,
    MountPreHydrationStatus, RemoteObservationRepository, mark_mount_pre_hydration_enumerating,
    mark_mount_pre_hydration_error, mark_mount_pre_hydration_hydrating,
};

use crate::hydration::HydrationEngine;
use crate::reconcile::{
    EntityFetchPlan, EntityFetchSchedule, FetchScheduleStrategy, MountFetchPlan,
    MountFetchSchedule, ScheduledPullReport, ScheduledPullSource,
    reconcile_scheduled_pull_with_state_root,
};
use crate::scheduler::PullSchedulerTick;

#[derive(Debug, Default)]
pub struct PreHydrationFetchScheduleStrategy;

impl FetchScheduleStrategy for PreHydrationFetchScheduleStrategy {
    fn mount_plan(&self, _request: MountFetchSchedule<'_>) -> MountFetchPlan {
        MountFetchPlan { enumerate: true }
    }

    fn entity_plan(&self, request: EntityFetchSchedule<'_>) -> EntityFetchPlan {
        if request.entry.kind == EntityKind::Page {
            return EntityFetchPlan {
                queue_hydration: Some(HydrationReason::Prefetch),
            };
        }

        EntityFetchPlan::default()
    }
}

pub fn execute_mount_pre_hydration<S, H, Source>(
    store: &mut S,
    hydration: &mut H,
    mount: &MountConfig,
    source: &Source,
    state_root: Option<&Path>,
    now: &str,
) -> LocalityResult<ScheduledPullReport>
where
    S: ConnectorStateRepository
        + EntityRepository
        + RemoteObservationRepository
        + FreshnessStateRepository,
    H: HydrationEngine,
    Source: ScheduledPullSource + ?Sized,
{
    mark_mount_pre_hydration_enumerating(store, &mount.connector, &mount.mount_id, now)?;

    let strategy = PreHydrationFetchScheduleStrategy::default();
    let counting_source = PreHydrationCountingSource::new(source);
    let tick = PullSchedulerTick {
        poll_active: true,
        poll_cold: true,
    };
    let report = match reconcile_scheduled_pull_with_state_root(
        store,
        hydration,
        std::slice::from_ref(mount),
        &tick,
        &counting_source,
        &strategy,
        &HydrationPolicy::default(),
        state_root,
    ) {
        Ok(report) => report,
        Err(error) => {
            let _ = mark_mount_pre_hydration_error(
                store,
                &mount.connector,
                &mount.mount_id,
                &error.to_string(),
                now,
            );
            return Err(error);
        }
    };

    let queued_pages = u64::try_from(report.queued_hydrations).map_err(|_| {
        LocalityError::InvalidState(
            "pre-hydration queued hydration count does not fit in u64".to_string(),
        )
    })?;
    mark_mount_pre_hydration_hydrating(
        store,
        &mount.connector,
        &mount.mount_id,
        counting_source.discovered_pages(),
        queued_pages,
        now,
    )?;

    Ok(report)
}

pub fn status_is_finished(status: &MountPreHydrationStatus) -> bool {
    matches!(
        status,
        MountPreHydrationStatus::Complete | MountPreHydrationStatus::Error
    )
}

struct PreHydrationCountingSource<'a, Source: ?Sized> {
    inner: &'a Source,
    discovered_pages: Cell<u64>,
}

impl<'a, Source: ?Sized> PreHydrationCountingSource<'a, Source> {
    fn new(inner: &'a Source) -> Self {
        Self {
            inner,
            discovered_pages: Cell::new(0),
        }
    }

    fn discovered_pages(&self) -> u64 {
        self.discovered_pages.get()
    }
}

impl<Source> ScheduledPullSource for PreHydrationCountingSource<'_, Source>
where
    Source: ScheduledPullSource + ?Sized,
{
    fn enumerate_mount(&self, mount: &MountConfig) -> LocalityResult<Vec<TreeEntry>> {
        let entries = self.inner.enumerate_mount(mount)?;
        let page_count = entries
            .iter()
            .filter(|entry| entry.kind == EntityKind::Page)
            .count();
        let page_count = u64::try_from(page_count).map_err(|_| {
            LocalityError::InvalidState(
                "pre-hydration discovered page count does not fit in u64".to_string(),
            )
        })?;
        self.discovered_pages.set(page_count);
        Ok(entries)
    }

    fn database_schema_yaml(
        &self,
        mount: &MountConfig,
        remote_id: &RemoteId,
    ) -> LocalityResult<Option<String>> {
        self.inner.database_schema_yaml(mount, remote_id)
    }
}
