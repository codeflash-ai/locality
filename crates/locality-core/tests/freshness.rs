use locality_core::freshness::{
    ChangeHintKind, FreshnessActivity, FreshnessDecision, FreshnessOptimizationPolicy,
    FreshnessTier, RemoteVersion, SyncJob, SyncJobCost, SyncJobKind, WorkingCopyState,
    classify_working_copy,
};
use locality_core::model::{HydrationState, MountId, RemoteId};

#[test]
fn remote_versions_are_opaque_stable_values() {
    let version = RemoteVersion::new("2026-06-15T00:00:00.000Z");

    assert_eq!(version.as_str(), "2026-06-15T00:00:00.000Z");
    assert_eq!(
        serde_json::to_string(&version).expect("serialize version"),
        "\"2026-06-15T00:00:00.000Z\""
    );
}

#[test]
fn freshness_tiers_order_by_scheduling_urgency() {
    assert!(FreshnessTier::Immediate.is_more_urgent_than(&FreshnessTier::Hot));
    assert!(FreshnessTier::Hot.is_more_urgent_than(&FreshnessTier::Warm));
    assert!(FreshnessTier::Warm.is_more_urgent_than(&FreshnessTier::Cold));
    assert!(FreshnessTier::Cold.is_more_urgent_than(&FreshnessTier::Dormant));
    assert!(!FreshnessTier::Dormant.is_more_urgent_than(&FreshnessTier::Immediate));
}

#[test]
fn working_copy_state_tracks_local_and_remote_drift() {
    assert_eq!(classify_working_copy(false, false), WorkingCopyState::Clean);
    assert_eq!(
        classify_working_copy(false, true),
        WorkingCopyState::RemoteChanged
    );
    assert_eq!(
        classify_working_copy(true, false),
        WorkingCopyState::LocalPending
    );
    assert_eq!(
        classify_working_copy(true, true),
        WorkingCopyState::Diverged
    );
}

#[test]
fn change_hints_map_to_default_freshness_tiers() {
    assert_eq!(
        ChangeHintKind::PushRequested.recommended_tier(),
        FreshnessTier::Immediate
    );
    assert_eq!(
        ChangeHintKind::LocalEdited.recommended_tier(),
        FreshnessTier::Hot
    );
    assert_eq!(
        ChangeHintKind::DirectoryListed.recommended_tier(),
        FreshnessTier::Warm
    );
    assert_eq!(
        ChangeHintKind::BackgroundPoll.recommended_tier(),
        FreshnessTier::Cold
    );
}

#[test]
fn activity_scoring_promotes_pending_and_recent_activity() {
    let policy = FreshnessOptimizationPolicy::default();

    let pending = FreshnessActivity {
        local_pending: true,
        ..FreshnessActivity::default()
    };
    let recent_open = FreshnessActivity {
        last_opened_age_ms: Some(policy.hot_after_activity_ms),
        ..FreshnessActivity::default()
    };
    let older_open = FreshnessActivity {
        last_opened_age_ms: Some(policy.hot_after_activity_ms + 1),
        ..FreshnessActivity::default()
    };

    assert_eq!(pending.recommended_tier(&policy), FreshnessTier::Hot);
    assert_eq!(recent_open.recommended_tier(&policy), FreshnessTier::Hot);
    assert_eq!(older_open.recommended_tier(&policy), FreshnessTier::Warm);
    assert!(
        pending.score(&policy)
            > FreshnessActivity::for_hydration(HydrationState::Stub).score(&policy)
    );
}

#[test]
fn inactive_deep_subtrees_decay_to_dormant() {
    let policy = FreshnessOptimizationPolicy {
        cold_after_check_ms: 10,
        dormant_subtree_depth: 3,
        ..FreshnessOptimizationPolicy::default()
    };
    let activity = FreshnessActivity {
        hydration: Some(HydrationState::Virtual),
        last_checked_age_ms: Some(11),
        subtree_depth: 3,
        ..FreshnessActivity::default()
    };
    let decision = FreshnessDecision::from_activity(&activity, &policy);

    assert_eq!(decision.tier, FreshnessTier::Dormant);
    assert_eq!(decision.activity_score, 0);
}

#[test]
fn hydrated_entities_remain_warm_without_recent_activity() {
    let policy = FreshnessOptimizationPolicy::default();
    let activity = FreshnessActivity::for_hydration(HydrationState::Hydrated);

    assert_eq!(activity.recommended_tier(&policy), FreshnessTier::Warm);
}

#[test]
fn sync_jobs_carry_cost_and_stable_dedupe_key() {
    let job = SyncJob::new(
        MountId::new("notion-main"),
        Some(RemoteId::new("page-1")),
        SyncJobKind::ObserveEntity,
        ChangeHintKind::LocalEdited,
    );

    assert_eq!(job.tier, FreshnessTier::Hot);
    assert_eq!(job.estimated_cost, SyncJobCost::Cheap);
    assert_eq!(
        job.dedupe_key(),
        "notion-main:page-1:ObserveEntity".to_string()
    );
    assert_eq!(
        SyncJobKind::HydrateEntity.estimated_cost().budget_units(),
        20
    );
}
