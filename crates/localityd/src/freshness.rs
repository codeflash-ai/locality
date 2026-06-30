//! Bounded daemon freshness queue.
//!
//! This queue is intentionally connector-neutral. Runtime integration can feed
//! it from file events, directory listings, remote hints, and push requests,
//! while workers drain a small budget into observation/enumeration/hydration
//! jobs.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use locality_core::LocalityResult;
use locality_core::freshness::{
    FreshnessActivity, FreshnessDecision, FreshnessOptimizationPolicy, FreshnessTier, SyncJob,
};
use locality_core::model::HydrationState;
use locality_store::{EntityRecord, FreshnessStateRecord, FreshnessStateRepository};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FreshnessQueue {
    jobs: BTreeMap<String, SyncJob>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FreshnessQueueMetrics {
    pub total_jobs: usize,
    pub ready_jobs: usize,
    pub deferred_jobs: usize,
    pub total_budget_units: u16,
    pub ready_budget_units: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FreshnessBatch {
    pub jobs: Vec<SyncJob>,
    pub metrics_before: FreshnessQueueMetrics,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FreshnessQueueDebugJob {
    pub job: SyncJob,
    pub ready: bool,
}

impl FreshnessQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    pub fn upsert(&mut self, job: SyncJob) {
        let key = job.dedupe_key();
        let Some(existing) = self.jobs.get_mut(&key) else {
            self.jobs.insert(key, job);
            return;
        };

        if job.tier.is_more_urgent_than(&existing.tier) {
            existing.tier = job.tier.clone();
            existing.reason = job.reason.clone();
        }
        if earlier_next_eligible(
            job.next_eligible_at.as_ref(),
            existing.next_eligible_at.as_ref(),
        ) {
            existing.next_eligible_at = job.next_eligible_at;
        }
    }

    pub fn drain_budget(&mut self, budget_units: u16) -> Vec<SyncJob> {
        self.drain_ready_budget(None, budget_units)
    }

    pub fn pop_ready(&mut self, budget_units: u16) -> Option<SyncJob> {
        self.pop_ready_at(None, budget_units)
    }

    pub fn pop_ready_at(&mut self, now: Option<&str>, budget_units: u16) -> Option<SyncJob> {
        let mut keys = self
            .jobs
            .iter()
            .filter(|(_, job)| is_ready(job, now))
            .map(|(key, job)| (key.clone(), job.clone()))
            .collect::<Vec<_>>();
        keys.sort_by(|(_, left), (_, right)| compare_jobs(left, right));

        for (key, job) in keys {
            if job.estimated_cost.budget_units() <= budget_units {
                self.jobs.remove(&key);
                return Some(job);
            }
        }

        None
    }

    pub fn drain_ready_batch(
        &mut self,
        now: Option<&str>,
        budget_units: u16,
        max_jobs: usize,
    ) -> FreshnessBatch {
        let metrics_before = self.metrics(now);
        let mut keys = self
            .jobs
            .iter()
            .filter(|(_, job)| is_ready(job, now))
            .map(|(key, job)| (key.clone(), job.clone()))
            .collect::<Vec<_>>();
        keys.sort_by(|(_, left), (_, right)| compare_jobs(left, right));

        let mut remaining = budget_units;
        let mut jobs = Vec::new();
        for (key, job) in keys {
            if jobs.len() >= max_jobs {
                break;
            }

            let cost = job.estimated_cost.budget_units();
            if cost > remaining {
                continue;
            }
            remaining -= cost;
            self.jobs.remove(&key);
            jobs.push(job);
        }

        FreshnessBatch {
            jobs,
            metrics_before,
        }
    }

    pub fn drain_ready_budget(&mut self, now: Option<&str>, budget_units: u16) -> Vec<SyncJob> {
        let mut keys = self
            .jobs
            .iter()
            .filter(|(_, job)| is_ready(job, now))
            .map(|(key, job)| (key.clone(), job.clone()))
            .collect::<Vec<_>>();
        keys.sort_by(|(_, left), (_, right)| compare_jobs(left, right));

        let mut remaining = budget_units;
        let mut drained = Vec::new();
        for (key, job) in keys {
            let cost = job.estimated_cost.budget_units();
            if cost > remaining {
                continue;
            }
            remaining -= cost;
            self.jobs.remove(&key);
            drained.push(job);
        }

        drained
    }

    pub fn metrics(&self, now: Option<&str>) -> FreshnessQueueMetrics {
        let mut metrics = FreshnessQueueMetrics {
            total_jobs: self.jobs.len(),
            ..FreshnessQueueMetrics::default()
        };
        for job in self.jobs.values() {
            let units = job.estimated_cost.budget_units();
            metrics.total_budget_units = metrics.total_budget_units.saturating_add(units);
            if is_ready(job, now) {
                metrics.ready_jobs += 1;
                metrics.ready_budget_units = metrics.ready_budget_units.saturating_add(units);
            } else {
                metrics.deferred_jobs += 1;
            }
        }
        metrics
    }

    pub fn debug_jobs(&self, now: Option<&str>, limit: usize) -> Vec<FreshnessQueueDebugJob> {
        let mut jobs = self.jobs.values().cloned().collect::<Vec<_>>();
        jobs.sort_by(compare_jobs);
        jobs.into_iter()
            .take(limit)
            .map(|job| FreshnessQueueDebugJob {
                ready: is_ready(&job, now),
                job,
            })
            .collect()
    }
}

/// Record that a local file became user-visible.
///
/// This affects scheduling only. It does not imply local content changed.
pub fn record_file_opened<S>(store: &mut S, entity: &EntityRecord) -> LocalityResult<()>
where
    S: FreshnessStateRepository,
{
    update_freshness_state(store, entity, |state, now| {
        promote_tier(state, FreshnessTier::Hot);
        state.last_opened_at = Some(now);
    })
}

/// Record that local content may differ from the last accepted shadow.
pub fn record_local_change<S>(store: &mut S, entity: &EntityRecord) -> LocalityResult<()>
where
    S: FreshnessStateRepository,
{
    update_freshness_state(store, entity, |state, now| {
        promote_tier(state, FreshnessTier::Hot);
        state.last_local_change_at = Some(now);
    })
}

pub fn optimized_freshness_decision(
    state: &FreshnessStateRecord,
    entity: Option<&EntityRecord>,
    now_ms: u64,
    policy: &FreshnessOptimizationPolicy,
) -> FreshnessDecision {
    let activity = activity_from_state(state, entity, now_ms);
    FreshnessDecision::from_activity(&activity, policy)
}

pub fn refresh_optimized_tier(
    state: &mut FreshnessStateRecord,
    entity: Option<&EntityRecord>,
    now_ms: u64,
    policy: &FreshnessOptimizationPolicy,
) -> FreshnessDecision {
    let decision = optimized_freshness_decision(state, entity, now_ms, policy);
    state.tier = decision.tier.clone();
    decision
}

fn activity_from_state(
    state: &FreshnessStateRecord,
    entity: Option<&EntityRecord>,
    now_ms: u64,
) -> FreshnessActivity {
    let hydration = entity.map(|entity| entity.hydration.clone());
    let local_pending = matches!(
        hydration,
        Some(HydrationState::Dirty | HydrationState::Conflicted)
    );
    FreshnessActivity {
        hydration,
        local_pending,
        remote_hint_pending: state.remote_hint_pending,
        last_opened_age_ms: age_ms(state.last_opened_at.as_deref(), now_ms),
        last_local_change_age_ms: age_ms(state.last_local_change_at.as_deref(), now_ms),
        last_checked_age_ms: age_ms(state.last_checked_at.as_deref(), now_ms),
        subtree_depth: entity.map_or(0, |entity| path_depth(&entity.path)),
    }
}

fn update_freshness_state<S, F>(
    store: &mut S,
    entity: &EntityRecord,
    update: F,
) -> LocalityResult<()>
where
    S: FreshnessStateRepository,
    F: FnOnce(&mut FreshnessStateRecord, String),
{
    let mut state = store
        .get_freshness_state(&entity.mount_id, &entity.remote_id)?
        .unwrap_or_else(|| {
            FreshnessStateRecord::new(
                entity.mount_id.clone(),
                entity.remote_id.clone(),
                default_freshness_tier(entity),
            )
        });
    update(&mut state, freshness_timestamp());
    store.save_freshness_state(state)?;
    Ok(())
}

fn promote_tier(state: &mut FreshnessStateRecord, tier: FreshnessTier) {
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

pub fn freshness_timestamp() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => format!("unix_ms:{}", duration.as_millis()),
        Err(_) => "unix_ms:0".to_string(),
    }
}

pub fn parse_freshness_timestamp(value: &str) -> Option<u64> {
    value.strip_prefix("unix_ms:")?.parse().ok()
}

pub fn freshness_unix_ms() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis().try_into().unwrap_or(u64::MAX),
        Err(_) => 0,
    }
}

fn age_ms(timestamp: Option<&str>, now_ms: u64) -> Option<u64> {
    timestamp
        .and_then(parse_freshness_timestamp)
        .map(|then| now_ms.saturating_sub(then))
}

fn path_depth(path: &Path) -> usize {
    path.components().count()
}

fn compare_jobs(left: &SyncJob, right: &SyncJob) -> Ordering {
    left.tier
        .cmp(&right.tier)
        .then_with(|| {
            left.estimated_cost
                .budget_units()
                .cmp(&right.estimated_cost.budget_units())
        })
        .then_with(|| left.dedupe_key().cmp(&right.dedupe_key()))
}

fn is_ready(job: &SyncJob, now: Option<&str>) -> bool {
    match (job.next_eligible_at.as_deref(), now) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(next), Some(now)) => next <= now,
    }
}

fn earlier_next_eligible(candidate: Option<&String>, current: Option<&String>) -> bool {
    match (candidate, current) {
        (None, Some(_)) => true,
        (Some(candidate), Some(current)) => candidate < current,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use locality_core::freshness::{
        ChangeHintKind, FreshnessOptimizationPolicy, FreshnessTier, SyncJob, SyncJobKind,
    };
    use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId};
    use locality_store::{EntityRecord, FreshnessStateRecord};

    use super::{FreshnessQueue, optimized_freshness_decision, refresh_optimized_tier};

    #[test]
    fn queue_drains_urgent_and_cheap_jobs_within_budget() {
        let mut queue = FreshnessQueue::new();
        queue.upsert(job(
            "page-cold",
            SyncJobKind::HydrateEntity,
            ChangeHintKind::BackgroundPoll,
        ));
        queue.upsert(job(
            "page-hot",
            SyncJobKind::ObserveEntity,
            ChangeHintKind::LocalEdited,
        ));
        queue.upsert(job(
            "page-warm",
            SyncJobKind::EnumerateChildren,
            ChangeHintKind::DirectoryListed,
        ));

        let drained = queue.drain_budget(6);

        assert_eq!(
            drained
                .iter()
                .map(|job| job.remote_id.as_ref().expect("remote id").as_str())
                .collect::<Vec<_>>(),
            vec!["page-hot", "page-warm"]
        );
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn duplicate_jobs_are_promoted_instead_of_repeated() {
        let mut queue = FreshnessQueue::new();
        queue.upsert(job(
            "page-1",
            SyncJobKind::ObserveEntity,
            ChangeHintKind::BackgroundPoll,
        ));
        queue.upsert(job(
            "page-1",
            SyncJobKind::ObserveEntity,
            ChangeHintKind::PushRequested,
        ));

        let drained = queue.drain_budget(1);

        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].tier, FreshnessTier::Immediate);
        assert!(queue.is_empty());
    }

    #[test]
    fn future_jobs_wait_until_eligible() {
        let mut queue = FreshnessQueue::new();
        queue.upsert(
            job(
                "page-1",
                SyncJobKind::ObserveEntity,
                ChangeHintKind::LocalEdited,
            )
            .next_eligible_at("2026-06-15T00:10:00Z"),
        );

        assert!(
            queue
                .drain_ready_budget(Some("2026-06-15T00:09:59Z"), 10)
                .is_empty()
        );
        assert_eq!(
            queue
                .drain_ready_budget(Some("2026-06-15T00:10:00Z"), 10)
                .len(),
            1
        );
    }

    #[test]
    fn pop_ready_returns_one_job_without_drain_side_effects() {
        let mut queue = FreshnessQueue::new();
        queue.upsert(job(
            "page-hot",
            SyncJobKind::ObserveEntity,
            ChangeHintKind::LocalEdited,
        ));
        queue.upsert(job(
            "page-cold",
            SyncJobKind::HydrateEntity,
            ChangeHintKind::BackgroundPoll,
        ));

        let popped = queue.pop_ready(1).expect("cheap hot job");

        assert_eq!(popped.remote_id.expect("remote id").as_str(), "page-hot");
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn queue_metrics_report_ready_and_deferred_work() {
        let mut queue = FreshnessQueue::new();
        queue.upsert(job(
            "page-ready",
            SyncJobKind::ObserveEntity,
            ChangeHintKind::LocalEdited,
        ));
        queue.upsert(
            job(
                "page-later",
                SyncJobKind::HydrateEntity,
                ChangeHintKind::BackgroundPoll,
            )
            .next_eligible_at("unix_ms:20"),
        );

        let metrics = queue.metrics(Some("unix_ms:10"));

        assert_eq!(metrics.total_jobs, 2);
        assert_eq!(metrics.ready_jobs, 1);
        assert_eq!(metrics.deferred_jobs, 1);
        assert_eq!(metrics.ready_budget_units, 1);
        assert_eq!(metrics.total_budget_units, 21);
    }

    #[test]
    fn queue_drains_bounded_batches() {
        let mut queue = FreshnessQueue::new();
        queue.upsert(job(
            "page-1",
            SyncJobKind::ObserveEntity,
            ChangeHintKind::LocalEdited,
        ));
        queue.upsert(job(
            "page-2",
            SyncJobKind::ObserveEntity,
            ChangeHintKind::LocalEdited,
        ));

        let batch = queue.drain_ready_batch(Some("unix_ms:10"), 10, 1);

        assert_eq!(batch.metrics_before.total_jobs, 2);
        assert_eq!(batch.jobs.len(), 1);
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn freshness_decision_keeps_dirty_entities_hot() {
        let entity = entity("Projects/Q4/Roadmap.md", HydrationState::Dirty);
        let state = FreshnessStateRecord::new(
            entity.mount_id.clone(),
            entity.remote_id.clone(),
            FreshnessTier::Cold,
        )
        .local_change_at("unix_ms:1000");

        let decision = optimized_freshness_decision(
            &state,
            Some(&entity),
            10_000,
            &FreshnessOptimizationPolicy::default(),
        );

        assert_eq!(decision.tier, FreshnessTier::Hot);
    }

    #[test]
    fn refresh_optimized_tier_decays_deep_inactive_virtual_entities() {
        let entity = entity("A/B/C/D/Page.md", HydrationState::Virtual);
        let mut state = FreshnessStateRecord::new(
            entity.mount_id.clone(),
            entity.remote_id.clone(),
            FreshnessTier::Cold,
        )
        .checked_at("unix_ms:0");
        let policy = FreshnessOptimizationPolicy {
            cold_after_check_ms: 10,
            dormant_subtree_depth: 4,
            ..FreshnessOptimizationPolicy::default()
        };

        let decision = refresh_optimized_tier(&mut state, Some(&entity), 11, &policy);

        assert_eq!(decision.tier, FreshnessTier::Dormant);
        assert_eq!(state.tier, FreshnessTier::Dormant);
    }

    fn job(remote_id: &str, kind: SyncJobKind, reason: ChangeHintKind) -> SyncJob {
        SyncJob::new(
            MountId::new("notion-main"),
            Some(RemoteId::new(remote_id)),
            kind,
            reason,
        )
    }

    fn entity(path: &str, hydration: HydrationState) -> EntityRecord {
        EntityRecord::new(
            MountId::new("notion-main"),
            RemoteId::new("page-1"),
            EntityKind::Page,
            "Roadmap",
            path,
        )
        .with_hydration(hydration)
    }
}
