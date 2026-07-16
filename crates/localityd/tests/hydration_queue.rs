use std::path::PathBuf;

use locality_core::LocalityError;
use locality_core::hydration::{HydrationReason, HydrationRequest};
use locality_core::model::{HydrationState, MountId, RemoteId};
use localityd::hydration::{HydrationPriority, HydrationQueue, hydration_priority};

#[test]
fn hydration_reasons_map_to_expected_priorities() {
    assert_eq!(
        hydration_priority(&HydrationReason::ExplicitPull),
        HydrationPriority::High
    );
    assert_eq!(
        hydration_priority(&HydrationReason::StubRead),
        HydrationPriority::High
    );
    assert_eq!(
        hydration_priority(&HydrationReason::Policy),
        HydrationPriority::Normal
    );
    assert_eq!(
        hydration_priority(&HydrationReason::RemoteFastForward),
        HydrationPriority::Normal
    );
    assert_eq!(
        hydration_priority(&HydrationReason::LiveModeRemoteFastForward),
        HydrationPriority::High
    );
    assert_eq!(
        hydration_priority(&HydrationReason::Prefetch),
        HydrationPriority::Low
    );
}

#[test]
fn queue_drains_high_priority_before_policy_and_prefetch() {
    let mut queue = HydrationQueue::new();
    queue.queue_request(request("mount", "prefetch", HydrationReason::Prefetch));
    queue.queue_request(request("mount", "policy", HydrationReason::Policy));
    queue.queue_request(request(
        "mount",
        "live",
        HydrationReason::LiveModeRemoteFastForward,
    ));
    queue.queue_request(request("mount", "read", HydrationReason::StubRead));

    assert_eq!(
        queue.pop_ready().expect("live mode request").remote_id,
        RemoteId::new("live")
    );
    assert_eq!(
        queue.pop_ready().expect("read request").remote_id,
        RemoteId::new("read")
    );
    assert_eq!(
        queue.pop_ready().expect("policy request").remote_id,
        RemoteId::new("policy")
    );
    assert_eq!(
        queue.pop_ready().expect("prefetch request").remote_id,
        RemoteId::new("prefetch")
    );
    assert!(queue.is_empty());
}

#[test]
fn debug_requests_follow_drain_priority_without_mutating_queue() {
    let mut queue = HydrationQueue::new();
    queue.queue_request(request("mount", "prefetch", HydrationReason::Prefetch));
    queue.queue_request(request("mount", "policy", HydrationReason::Policy));
    queue.queue_request(request("mount", "read", HydrationReason::StubRead));

    let ids = queue
        .debug_requests(2)
        .into_iter()
        .map(|request| request.remote_id)
        .collect::<Vec<_>>();
    assert_eq!(ids, vec![RemoteId::new("read"), RemoteId::new("policy")]);
    assert_eq!(queue.len(), 3);
}

#[test]
fn queue_uses_priority_buckets_for_large_prefetch_backlog_and_promotion() {
    let mut queue = HydrationQueue::new();
    for index in 0..128 {
        queue.queue_request(request(
            "mount",
            &format!("prefetch-{index:03}"),
            HydrationReason::Prefetch,
        ));
    }

    queue.queue_request(request("mount", "interactive", HydrationReason::FileOpen));

    assert_eq!(queue.debug_priority_bucket_lengths(), (1, 0, 128));
    assert_eq!(
        queue.peek_ready().expect("interactive peek").remote_id,
        RemoteId::new("interactive")
    );
    assert_eq!(
        queue.pop_ready().expect("interactive pop").remote_id,
        RemoteId::new("interactive")
    );

    for index in 0..128 {
        assert_eq!(
            queue.pop_ready().expect("prefetch pop").remote_id,
            RemoteId::new(format!("prefetch-{index:03}"))
        );
    }
    assert!(queue.is_empty());

    let mut prefetch = request("mount", "promoted", HydrationReason::Prefetch);
    prefetch.path = PathBuf::from("old.md");
    let mut explicit = request("mount", "promoted", HydrationReason::ExplicitPull);
    explicit.path = PathBuf::from("new.md");

    assert!(queue.queue_request(prefetch));
    assert!(!queue.queue_request(explicit));
    assert_eq!(queue.debug_priority_bucket_lengths(), (1, 0, 0));

    let promoted = queue.pop_ready().expect("promoted request");
    assert_eq!(promoted.reason, HydrationReason::ExplicitPull);
    assert_eq!(promoted.path, PathBuf::from("new.md"));
    assert!(queue.is_empty());
    assert_eq!(queue.debug_priority_bucket_lengths(), (0, 0, 0));
}

#[test]
fn duplicate_entity_request_is_deduped_and_promoted() {
    let mut queue = HydrationQueue::new();
    let mut low = request("mount", "page-1", HydrationReason::Prefetch);
    low.path = PathBuf::from("old.md");
    low.target_state = HydrationState::Stub;
    let mut high = request("mount", "page-1", HydrationReason::ExplicitPull);
    high.path = PathBuf::from("new.md");
    high.target_state = HydrationState::Hydrated;

    assert!(queue.queue_request(low));
    assert!(!queue.queue_request(high));

    assert_eq!(queue.len(), 1);
    let queued = queue.pop_ready().expect("promoted request");
    assert_eq!(queued.reason, HydrationReason::ExplicitPull);
    assert_eq!(queued.path, PathBuf::from("new.md"));
    assert_eq!(queued.target_state, HydrationState::Hydrated);
}

#[test]
fn same_remote_id_can_be_queued_for_different_mounts() {
    let mut queue = HydrationQueue::new();

    queue.queue_request(request("mount-a", "page-1", HydrationReason::Policy));
    queue.queue_request(request("mount-b", "page-1", HydrationReason::Policy));

    assert_eq!(queue.len(), 2);
}

#[test]
fn failed_drain_requeues_the_failed_request() {
    let mut queue = HydrationQueue::new();
    queue.queue_request(request("mount", "page-1", HydrationReason::ExplicitPull));

    let error = queue
        .drain_ready_with(|_| Err(LocalityError::InvalidState("hydrate failed".to_string())))
        .expect_err("drain failure");

    assert_eq!(
        error,
        LocalityError::InvalidState("hydrate failed".to_string())
    );
    assert_eq!(queue.len(), 1);
    assert_eq!(
        queue.peek_ready().expect("requeued request").remote_id,
        RemoteId::new("page-1")
    );
}

fn request(mount_id: &str, remote_id: &str, reason: HydrationReason) -> HydrationRequest {
    HydrationRequest::new(
        MountId::new(mount_id),
        RemoteId::new(remote_id),
        format!("{remote_id}.md"),
        HydrationState::Hydrated,
        reason,
    )
}
