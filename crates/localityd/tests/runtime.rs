use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use locality_core::canonical::render_canonical_markdown;
use locality_core::freshness::{ChangeHintKind, FreshnessTier, SyncJob, SyncJobKind};
use locality_core::hydration::{HydrationPolicy, HydrationReason, HydrationRequest};
use locality_core::model::{CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId};
use locality_core::pull::PullMode;
use locality_core::shadow::ShadowDocument;
use locality_core::{LocalityError, LocalityResult};
use locality_store::{
    AutoSaveEnrollmentRecord, AutoSaveOrigin, AutoSaveRepository, EntityRecord, EntityRepository,
    FreshnessStateRecord, FreshnessStateRepository, HydrationJobRecord, HydrationJobRepository,
    InMemoryStateStore, MountConfig, MountLiveModeRecord, MountLiveModeRepository, MountRepository,
    ProjectionMode, ShadowRepository, SqliteStateStore, live_mode_state_change_signal_path,
};
use localityd::DaemonConfig;
use localityd::execution::{DaemonEventReport, PushJob};
use localityd::freshness::freshness_timestamp;
use localityd::hydration::HydrationOutcome;
use localityd::ipc::{DaemonDebugQueueStatus, DaemonRequest, DaemonResponse, DaemonRuntimeStatus};
use localityd::runtime::{
    DaemonRuntime, DaemonRuntimeHandle, DefaultRuntimeJobRunner, FileEventRuntimeReport,
    FreshnessRuntimeReport, RuntimeJobRunner, ScheduledPullRuntimeReport,
    workspace_virtual_freshness_jobs,
};
use localityd::scheduler::PullSchedulerTick;
use localityd::virtual_fs::{
    ROOT_CONTAINER_IDENTIFIER, VirtualFsChildrenReport, VirtualFsRefreshChildrenReport,
    virtual_fs_content_root,
};
use localityd::watcher::{FileEvent, FileEventKind};
use serde_json::json;

#[test]
fn runtime_answers_ping_while_pull_worker_is_blocked() {
    let (started_tx, started_rx) = mpsc::channel();
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let runtime = DaemonRuntime::spawn_with_runner(
        relay_config("ping-while-blocked"),
        BlockingPullRunner {
            started: started_tx,
            release: Arc::clone(&release),
        },
    )
    .expect("spawn runtime");
    let pull_handle = runtime.handle();

    let pull_thread = thread::spawn(move || {
        pull_handle.request(DaemonRequest::Pull {
            path: PathBuf::from("Roadmap.md"),
        })
    });
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("pull started");

    let ping = runtime.handle().request(DaemonRequest::Ping);
    assert_eq!(ping, DaemonResponse::ok(json!({ "status": "ok" })));

    let status = runtime.handle().status().expect("runtime status");
    assert!(status.active_job);
    let active = status.active_job_detail.expect("active job detail");
    assert_eq!(active.kind, "pull");
    assert_eq!(active.target.as_deref(), Some("Roadmap.md"));

    release_blocked_runner(&release);
    let pull = pull_thread.join().expect("pull thread");
    assert!(pull.ok);
    runtime.shutdown();
}

#[test]
fn runtime_answers_virtual_fs_children_while_pull_worker_is_blocked() {
    let (started_tx, started_rx) = mpsc::channel();
    let (children_tx, children_rx) = mpsc::channel();
    let (refresh_tx, refresh_rx) = mpsc::channel();
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let runtime = DaemonRuntime::spawn_with_runner(
        relay_config("children-while-blocked"),
        BlockingVirtualFsRunner {
            started: started_tx,
            release: Arc::clone(&release),
            children_tx,
            refresh_tx,
        },
    )
    .expect("spawn runtime");
    let pull_handle = runtime.handle();

    let pull_thread = thread::spawn(move || {
        pull_handle.request(DaemonRequest::Pull {
            path: PathBuf::from("Roadmap.md"),
        })
    });
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("pull started");

    let metadata_handle = runtime.handle();
    let (response_tx, response_rx) = mpsc::channel();
    let metadata_thread = thread::spawn(move || {
        let response = metadata_handle.request(DaemonRequest::VirtualFsChildren {
            mount_id: "notion-main".to_string(),
            container_identifier: ROOT_CONTAINER_IDENTIFIER.to_string(),
        });
        response_tx.send(response).expect("send metadata response");
    });

    let children_call = children_rx.recv_timeout(Duration::from_secs(1));
    let response = response_rx.recv_timeout(Duration::from_secs(1));
    let refresh_before_release = refresh_rx.recv_timeout(Duration::from_millis(100));

    release_blocked_runner(&release);
    assert!(pull_thread.join().expect("pull thread").ok);
    metadata_thread.join().expect("metadata thread");

    assert_eq!(
        children_call.expect("virtual fs children should bypass active pull"),
        (
            "notion-main".to_string(),
            ROOT_CONTAINER_IDENTIFIER.to_string()
        )
    );
    assert!(
        response
            .expect("virtual fs children response should not wait for pull")
            .ok
    );
    assert!(
        refresh_before_release.is_err(),
        "background child refresh should remain serialized behind active mutating work"
    );
    assert_eq!(
        refresh_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("queued child refresh"),
        (
            "notion-main".to_string(),
            ROOT_CONTAINER_IDENTIFIER.to_string()
        )
    );
    runtime.shutdown();
}

#[test]
fn runtime_answers_cached_file_provider_children_while_pull_worker_is_blocked() {
    let (started_tx, started_rx) = mpsc::channel();
    let (children_tx, children_rx) = mpsc::channel();
    let (refresh_tx, refresh_rx) = mpsc::channel();
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let runtime = DaemonRuntime::spawn_with_runner(
        relay_config("file-provider-children-while-blocked"),
        BlockingVirtualFsRunner {
            started: started_tx,
            release: Arc::clone(&release),
            children_tx,
            refresh_tx,
        },
    )
    .expect("spawn runtime");
    let pull_handle = runtime.handle();

    let pull_thread = thread::spawn(move || {
        pull_handle.request(DaemonRequest::Pull {
            path: PathBuf::from("Roadmap.md"),
        })
    });
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("pull started");

    let metadata_handle = runtime.handle();
    let (response_tx, response_rx) = mpsc::channel();
    let metadata_thread = thread::spawn(move || {
        let response = metadata_handle.request(DaemonRequest::FileProviderChildren {
            mount_id: "notion-main".to_string(),
            container_identifier: ROOT_CONTAINER_IDENTIFIER.to_string(),
        });
        response_tx.send(response).expect("send metadata response");
    });

    let children_call = children_rx.recv_timeout(Duration::from_secs(1));
    let response = response_rx.recv_timeout(Duration::from_secs(1));
    let refresh_before_release = refresh_rx.recv_timeout(Duration::from_millis(100));

    assert_eq!(
        children_call.expect("file provider children should read cache while pull is active"),
        (
            "notion-main".to_string(),
            ROOT_CONTAINER_IDENTIFIER.to_string()
        )
    );
    assert!(
        response
            .expect("file provider children response should not wait for pull")
            .ok
    );
    assert!(
        refresh_before_release.is_err(),
        "cached file provider children should not force an interactive refresh"
    );

    release_blocked_runner(&release);
    assert!(pull_thread.join().expect("pull thread").ok);
    metadata_thread.join().expect("metadata thread");

    assert!(
        refresh_rx.recv_timeout(Duration::from_millis(100)).is_err(),
        "cached file provider children should not refresh after the pull completes"
    );
    runtime.shutdown();
}

#[test]
fn runtime_file_provider_children_bypasses_active_background_refreshes() {
    let config = relay_config("file-provider-bypasses-background");
    let mut store = SqliteStateStore::open(config.state_root.clone()).expect("open store");
    store
        .save_mount(
            MountConfig::new(
                MountId::new("notion-main"),
                "notion",
                temp_root("file-provider-bypasses-background-root"),
            )
            .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    drop(store);

    let (background_tx, background_rx) = mpsc::channel();
    let (foreground_tx, foreground_rx) = mpsc::channel();
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let runtime = DaemonRuntime::spawn_with_runner(
        config,
        BlockingBackgroundRefreshRunner {
            background_tx,
            foreground_tx,
            release: Arc::clone(&release),
        },
    )
    .expect("spawn runtime");

    runtime
        .handle()
        .prime_virtual_mounts()
        .expect("prime virtual mounts");
    background_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("background refresh started");

    let foreground_handle = runtime.handle();
    let response_thread = thread::spawn(move || {
        foreground_handle.request(DaemonRequest::FileProviderChildren {
            mount_id: "notion-main".to_string(),
            container_identifier: "mount:notion-main".to_string(),
        })
    });

    assert_eq!(
        background_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("foreground interactive refresh"),
        ("notion-main".to_string(), "mount:notion-main".to_string())
    );
    assert!(
        foreground_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err(),
        "children should wait until the interactive refresh completes"
    );
    release_blocked_runner(&release);
    assert_eq!(
        foreground_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("foreground children request"),
        ("notion-main".to_string(), "mount:notion-main".to_string())
    );
    assert!(response_thread.join().expect("foreground response").ok);
    runtime.shutdown();
}

#[test]
fn runtime_serializes_mutating_requests() {
    let state = Arc::new(SerialState::default());
    let runtime = DaemonRuntime::spawn_with_runner(
        relay_config("serial-mutating"),
        SerialRunner {
            state: Arc::clone(&state),
        },
    )
    .expect("spawn runtime");

    let first = runtime.handle();
    let first_thread = thread::spawn(move || {
        first.request(DaemonRequest::Pull {
            path: PathBuf::from("First.md"),
        })
    });
    let second = runtime.handle();
    let second_thread = thread::spawn(move || {
        second.request(DaemonRequest::Pull {
            path: PathBuf::from("Second.md"),
        })
    });

    state.wait_started(1);
    thread::sleep(Duration::from_millis(50));
    assert_eq!(state.started_count(), 1);

    state.release(1);
    state.wait_started(2);
    assert_eq!(state.max_active.load(Ordering::SeqCst), 1);

    state.release(2);
    assert!(first_thread.join().expect("first response").ok);
    assert!(second_thread.join().expect("second response").ok);
    runtime.shutdown();
}

#[test]
fn runtime_scheduler_queues_and_drains_hydration() {
    let (scheduled_tx, scheduled_rx) = mpsc::channel();
    let (hydrated_tx, hydrated_rx) = mpsc::channel();
    let runtime = DaemonRuntime::spawn_with_runner(
        polling_config("scheduled-hydration"),
        SchedulingRunner {
            scheduled: scheduled_tx,
            hydrated: hydrated_tx,
            scheduled_count: AtomicUsize::new(0),
        },
    )
    .expect("spawn runtime");

    scheduled_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("scheduled pull ran");
    let request = hydrated_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("hydration drained");

    assert_eq!(request.mount_id, MountId::new("notion-main"));
    assert_eq!(request.remote_id, RemoteId::new("page-1"));
    assert_eq!(request.reason, HydrationReason::Policy);
    runtime.shutdown();
}

#[test]
fn runtime_drains_freshness_queued_by_scheduled_pull() {
    let (scheduled_tx, scheduled_rx) = mpsc::channel();
    let (freshness_tx, freshness_rx) = mpsc::channel();
    let runtime = DaemonRuntime::spawn_with_runner(
        polling_config("scheduled-freshness"),
        ScheduledFreshnessRunner {
            scheduled: scheduled_tx,
            freshness: freshness_tx,
            scheduled_count: AtomicUsize::new(0),
        },
    )
    .expect("spawn runtime");

    scheduled_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("scheduled pull ran");
    let job = freshness_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("freshness drained");

    assert_eq!(job.mount_id, MountId::new("notion-main"));
    assert_eq!(job.remote_id, Some(RemoteId::new("page-1")));
    assert_eq!(job.kind, SyncJobKind::ObserveEntity);
    assert_eq!(job.reason, ChangeHintKind::BackgroundPoll);
    runtime.shutdown();
}

#[test]
fn workspace_virtual_mount_queues_hydrated_page_on_cold_tick() {
    let mount_id = MountId::new("notion-main");
    let mount = workspace_virtual_mount(&mount_id, "workspace-cold");
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");
    save_workspace_page(
        &mut store,
        &mount_id,
        "page-1",
        "Roadmap",
        "Roadmap.md",
        HydrationState::Hydrated,
    );

    let jobs = workspace_virtual_freshness_jobs(
        &store,
        &[mount],
        &PullSchedulerTick {
            poll_active: false,
            poll_cold: true,
        },
    )
    .expect("workspace freshness jobs");

    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].mount_id, mount_id);
    assert_eq!(jobs[0].remote_id, Some(RemoteId::new("page-1")));
    assert_eq!(jobs[0].kind, SyncJobKind::ObserveEntity);
    assert_eq!(jobs[0].reason, ChangeHintKind::BackgroundPoll);
    assert_eq!(jobs[0].tier, FreshnessTier::Warm);
}

#[test]
fn workspace_virtual_mount_queues_hot_dirty_and_conflicted_pages_on_active_tick() {
    let mount_id = MountId::new("notion-main");
    let mount = workspace_virtual_mount(&mount_id, "workspace-active");
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");
    save_workspace_page(
        &mut store,
        &mount_id,
        "hot-page",
        "Hot",
        "Hot.md",
        HydrationState::Hydrated,
    );
    save_workspace_page(
        &mut store,
        &mount_id,
        "dirty-page",
        "Dirty",
        "Dirty.md",
        HydrationState::Dirty,
    );
    save_workspace_page(
        &mut store,
        &mount_id,
        "conflicted-page",
        "Conflicted",
        "Conflicted.md",
        HydrationState::Conflicted,
    );
    store
        .save_freshness_state(
            FreshnessStateRecord::new(
                mount_id.clone(),
                RemoteId::new("hot-page"),
                FreshnessTier::Hot,
            )
            .opened_at(freshness_timestamp())
            .checked_at("unix_ms:100"),
        )
        .expect("save hot freshness");

    let jobs = workspace_virtual_freshness_jobs(
        &store,
        &[mount],
        &PullSchedulerTick {
            poll_active: true,
            poll_cold: false,
        },
    )
    .expect("workspace freshness jobs");

    assert_eq!(jobs.len(), 3);
    let job_facts = jobs
        .iter()
        .map(|job| {
            (
                job.remote_id
                    .as_ref()
                    .expect("remote id")
                    .as_str()
                    .to_string(),
                format!("{:?}", job.reason),
                job.tier.as_str().to_string(),
            )
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        job_facts,
        BTreeSet::from([
            (
                "conflicted-page".to_string(),
                "LocalEdited".to_string(),
                "hot".to_string(),
            ),
            (
                "dirty-page".to_string(),
                "LocalEdited".to_string(),
                "hot".to_string(),
            ),
            (
                "hot-page".to_string(),
                "FileOpened".to_string(),
                "hot".to_string(),
            ),
        ])
    );
}

#[test]
fn workspace_virtual_mount_promotes_live_mode_active_page_to_immediate() {
    let mount_id = MountId::new("notion-main");
    let mount = workspace_virtual_mount(&mount_id, "workspace-live-mode-active");
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");
    store
        .save_mount_live_mode(MountLiveModeRecord::new(
            mount_id.clone(),
            true,
            freshness_timestamp(),
        ))
        .expect("save live mode");
    save_workspace_page(
        &mut store,
        &mount_id,
        "active-page",
        "Active",
        "Active.md",
        HydrationState::Hydrated,
    );
    store
        .save_freshness_state(
            FreshnessStateRecord::new(
                mount_id.clone(),
                RemoteId::new("active-page"),
                FreshnessTier::Hot,
            )
            .opened_at(freshness_timestamp())
            .checked_at("unix_ms:100"),
        )
        .expect("save hot freshness");

    let jobs = workspace_virtual_freshness_jobs(
        &store,
        &[mount],
        &PullSchedulerTick {
            poll_active: true,
            poll_cold: false,
        },
    )
    .expect("workspace freshness jobs");

    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].remote_id, Some(RemoteId::new("active-page")));
    assert_eq!(jobs[0].reason, ChangeHintKind::FileOpened);
    assert_eq!(jobs[0].tier, FreshnessTier::Immediate);
}

#[test]
fn workspace_virtual_mount_promotes_file_live_mode_enrollment_to_immediate_on_active_tick() {
    let mount_id = MountId::new("notion-main");
    let mount = workspace_virtual_mount(&mount_id, "workspace-file-live-mode-active");
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");
    save_workspace_page(
        &mut store,
        &mount_id,
        "active-page",
        "Active",
        "Active.md",
        HydrationState::Hydrated,
    );
    store
        .save_freshness_state(
            FreshnessStateRecord::new(
                mount_id.clone(),
                RemoteId::new("active-page"),
                FreshnessTier::Warm,
            )
            .next_check_at("unix_ms:18446744073709551615"),
        )
        .expect("save deferred freshness");
    let mut enrollment = AutoSaveEnrollmentRecord::new(
        mount_id.clone(),
        "Active.md",
        AutoSaveOrigin::UserEnabled,
        freshness_timestamp(),
    );
    enrollment.remote_id = Some(RemoteId::new("active-page"));
    store
        .save_auto_save_enrollment(enrollment)
        .expect("save auto-save enrollment");

    let jobs = workspace_virtual_freshness_jobs(
        &store,
        &[mount],
        &PullSchedulerTick {
            poll_active: true,
            poll_cold: false,
        },
    )
    .expect("workspace freshness jobs");

    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].remote_id, Some(RemoteId::new("active-page")));
    assert_eq!(jobs[0].reason, ChangeHintKind::FileOpened);
    assert_eq!(jobs[0].tier, FreshnessTier::Immediate);
}

#[test]
fn workspace_virtual_mount_caps_live_mode_active_pages_without_starving_other_freshness() {
    let mount_id = MountId::new("notion-main");
    let mount = workspace_virtual_mount(&mount_id, "workspace-live-mode-fairness");
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");
    store
        .save_mount_live_mode(MountLiveModeRecord::new(
            mount_id.clone(),
            true,
            freshness_timestamp(),
        ))
        .expect("save live mode");

    for index in 0..7 {
        let remote_id = format!("active-page-{index}");
        save_workspace_page(
            &mut store,
            &mount_id,
            &remote_id,
            &format!("Active {index}"),
            &format!("Active {index}.md"),
            HydrationState::Hydrated,
        );
        store
            .save_freshness_state(
                FreshnessStateRecord::new(
                    mount_id.clone(),
                    RemoteId::new(remote_id),
                    FreshnessTier::Hot,
                )
                .opened_at(freshness_timestamp())
                .checked_at("unix_ms:100"),
            )
            .expect("save active freshness");
    }

    save_workspace_page(
        &mut store,
        &mount_id,
        "dirty-page",
        "Dirty",
        "Dirty.md",
        HydrationState::Dirty,
    );
    store
        .save_freshness_state(
            FreshnessStateRecord::new(
                mount_id.clone(),
                RemoteId::new("dirty-page"),
                FreshnessTier::Hot,
            )
            .local_change_at(freshness_timestamp())
            .checked_at("unix_ms:100"),
        )
        .expect("save dirty freshness");

    save_workspace_page(
        &mut store,
        &mount_id,
        "background-page",
        "Background",
        "Background.md",
        HydrationState::Hydrated,
    );
    store
        .save_freshness_state(
            FreshnessStateRecord::new(
                mount_id.clone(),
                RemoteId::new("background-page"),
                FreshnessTier::Warm,
            )
            .checked_at("unix_ms:100"),
        )
        .expect("save background freshness");

    let jobs = workspace_virtual_freshness_jobs(
        &store,
        &[mount],
        &PullSchedulerTick {
            poll_active: true,
            poll_cold: true,
        },
    )
    .expect("workspace freshness jobs");

    let live_mode_active_jobs = jobs
        .iter()
        .filter(|job| {
            job.reason == ChangeHintKind::FileOpened && job.tier == FreshnessTier::Immediate
        })
        .count();
    assert_eq!(live_mode_active_jobs, 5);
    assert!(jobs.iter().any(|job| {
        job.remote_id.as_ref() == Some(&RemoteId::new("dirty-page"))
            && job.reason == ChangeHintKind::LocalEdited
    }));
    assert!(jobs.iter().any(|job| {
        job.remote_id.as_ref() == Some(&RemoteId::new("background-page"))
            && job.reason == ChangeHintKind::BackgroundPoll
    }));
}

#[test]
fn workspace_virtual_mount_does_not_queue_stale_hot_page_on_active_tick() {
    let mount_id = MountId::new("notion-main");
    let mount = workspace_virtual_mount(&mount_id, "workspace-stale-hot");
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");
    save_workspace_page(
        &mut store,
        &mount_id,
        "stale-hot-page",
        "Stale Hot",
        "Stale Hot.md",
        HydrationState::Hydrated,
    );
    store
        .save_freshness_state(
            FreshnessStateRecord::new(
                mount_id.clone(),
                RemoteId::new("stale-hot-page"),
                FreshnessTier::Hot,
            )
            .opened_at("unix_ms:1")
            .checked_at("unix_ms:2"),
        )
        .expect("save stale hot freshness");

    let jobs = workspace_virtual_freshness_jobs(
        &store,
        &[mount],
        &PullSchedulerTick {
            poll_active: true,
            poll_cold: false,
        },
    )
    .expect("workspace freshness jobs");

    assert!(jobs.is_empty());
}

#[test]
fn workspace_virtual_mount_skips_pages_deferred_until_future_check() {
    let mount_id = MountId::new("notion-main");
    let mount = workspace_virtual_mount(&mount_id, "workspace-future-check");
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");
    save_workspace_page(
        &mut store,
        &mount_id,
        "deferred-page",
        "Deferred",
        "Deferred.md",
        HydrationState::Hydrated,
    );
    store
        .save_freshness_state(
            FreshnessStateRecord::new(
                mount_id.clone(),
                RemoteId::new("deferred-page"),
                FreshnessTier::Warm,
            )
            .next_check_at("unix_ms:18446744073709551615"),
        )
        .expect("save deferred freshness");

    let jobs = workspace_virtual_freshness_jobs(
        &store,
        &[mount],
        &PullSchedulerTick {
            poll_active: false,
            poll_cold: true,
        },
    )
    .expect("workspace freshness jobs");

    assert!(jobs.is_empty());
}

#[test]
fn workspace_virtual_mount_does_not_queue_stub_or_virtual_pages() {
    let mount_id = MountId::new("notion-main");
    let mount = workspace_virtual_mount(&mount_id, "workspace-skip-stub");
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");
    save_workspace_page(
        &mut store,
        &mount_id,
        "stub-page",
        "Stub",
        "Stub.md",
        HydrationState::Stub,
    );
    save_workspace_page(
        &mut store,
        &mount_id,
        "virtual-page",
        "Virtual",
        "Virtual.md",
        HydrationState::Virtual,
    );
    store
        .save_entity(
            EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("database-1"),
                EntityKind::Database,
                "Tasks",
                "Tasks",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save database");

    let jobs = workspace_virtual_freshness_jobs(
        &store,
        &[mount],
        &PullSchedulerTick {
            poll_active: true,
            poll_cold: true,
        },
    )
    .expect("workspace freshness jobs");

    assert!(jobs.is_empty());
}

#[test]
fn root_page_virtual_mount_is_not_queued_by_workspace_freshness_path() {
    let mount_id = MountId::new("notion-main");
    let mount = workspace_virtual_mount(&mount_id, "root-page-virtual")
        .with_remote_root_id(RemoteId::new("root-page"));
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");
    save_workspace_page(
        &mut store,
        &mount_id,
        "root-page",
        "Root",
        "Root/page.md",
        HydrationState::Hydrated,
    );

    let jobs = workspace_virtual_freshness_jobs(
        &store,
        &[mount],
        &PullSchedulerTick {
            poll_active: false,
            poll_cold: true,
        },
    )
    .expect("workspace freshness jobs");

    assert!(jobs.is_empty());
}

#[test]
fn workspace_freshness_cap_and_order_prefers_hot_then_oldest_checks() {
    let mount_id = MountId::new("notion-main");
    let mount = workspace_virtual_mount(&mount_id, "workspace-cap");
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");
    save_workspace_page(
        &mut store,
        &mount_id,
        "hot-recent",
        "Hot Recent",
        "z-hot-recent.md",
        HydrationState::Hydrated,
    );
    store
        .save_freshness_state(
            FreshnessStateRecord::new(
                mount_id.clone(),
                RemoteId::new("hot-recent"),
                FreshnessTier::Hot,
            )
            .opened_at(freshness_timestamp())
            .checked_at("unix_ms:999999"),
        )
        .expect("save hot freshness");
    save_workspace_page(
        &mut store,
        &mount_id,
        "warm-never",
        "Warm Never",
        "a-warm-never.md",
        HydrationState::Hydrated,
    );
    for index in 0..98 {
        let remote_id = format!("warm-old-{index:03}");
        save_workspace_page(
            &mut store,
            &mount_id,
            &remote_id,
            format!("Warm Old {index:03}"),
            format!("b-warm-old-{index:03}.md"),
            HydrationState::Hydrated,
        );
        store
            .save_freshness_state(
                FreshnessStateRecord::new(
                    mount_id.clone(),
                    RemoteId::new(remote_id),
                    FreshnessTier::Warm,
                )
                .checked_at(format!("unix_ms:{}", index + 1)),
            )
            .expect("save old freshness");
    }
    save_workspace_page(
        &mut store,
        &mount_id,
        "warm-newest",
        "Warm Newest",
        "c-warm-newest.md",
        HydrationState::Hydrated,
    );
    store
        .save_freshness_state(
            FreshnessStateRecord::new(
                mount_id.clone(),
                RemoteId::new("warm-newest"),
                FreshnessTier::Warm,
            )
            .checked_at("unix_ms:9999999"),
        )
        .expect("save newest freshness");

    let jobs = workspace_virtual_freshness_jobs(
        &store,
        &[mount],
        &PullSchedulerTick {
            poll_active: true,
            poll_cold: true,
        },
    )
    .expect("workspace freshness jobs");

    assert_eq!(jobs.len(), 100);
    assert_eq!(jobs[0].remote_id, Some(RemoteId::new("hot-recent")));
    assert_eq!(jobs[1].remote_id, Some(RemoteId::new("warm-never")));
    assert_eq!(jobs[2].remote_id, Some(RemoteId::new("warm-old-000")));
    assert!(
        !jobs
            .iter()
            .any(|job| job.remote_id == Some(RemoteId::new("warm-newest")))
    );
}

#[test]
fn runtime_routes_file_events_through_worker_queue() {
    let (event_tx, event_rx) = mpsc::channel();
    let runtime = DaemonRuntime::spawn_with_runner(
        relay_config("file-event-routing"),
        EventRunner { event_tx },
    )
    .expect("spawn runtime");

    runtime
        .handle()
        .file_event(FileEvent {
            path: PathBuf::from("Roadmap.md"),
            kind: FileEventKind::Write,
        })
        .expect("submit file event");

    let event = event_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("file event ran");
    assert_eq!(event.path, PathBuf::from("Roadmap.md"));
    assert_eq!(event.kind, FileEventKind::Write);
    runtime.shutdown();
}

#[test]
fn runtime_reports_status_snapshot() {
    let runtime = DaemonRuntime::spawn_with_runner(
        relay_config("status-snapshot"),
        EventRunner {
            event_tx: mpsc::channel().0,
        },
    )
    .expect("spawn runtime");

    let status = runtime.handle().status().expect("runtime status");
    assert!(!status.active_job);
    assert_eq!(status.pending_requests, 0);
    assert_eq!(status.pending_hydrations, 0);
    assert_eq!(status.scheduler_mode, "relay");

    let response = runtime.handle().request(DaemonRequest::Status);
    assert!(response.ok);
    let payload = response.payload.expect("status payload");
    let from_ipc: DaemonRuntimeStatus = serde_json::from_value(payload).expect("decode status");
    assert_eq!(from_ipc, status);
    runtime.shutdown();
}

#[test]
fn runtime_reports_debug_queue_snapshot() {
    let runtime = DaemonRuntime::spawn_with_runner(
        relay_config("debug-queue-snapshot"),
        EventRunner {
            event_tx: mpsc::channel().0,
        },
    )
    .expect("spawn runtime");

    let response = runtime.handle().request(DaemonRequest::DebugQueueStatus);
    assert!(response.ok);
    let payload = response.payload.expect("debug queue payload");
    let snapshot: localityd::ipc::DaemonDebugQueueStatus =
        serde_json::from_value(payload).expect("decode debug queue");
    assert_eq!(snapshot.scheduler_mode, "relay");
    assert!(snapshot.active.is_empty());
    assert!(
        snapshot
            .sections
            .iter()
            .any(|section| section.name == "notion_transport")
    );
    assert!(
        snapshot
            .sections
            .iter()
            .any(|section| section.name == "hydrations")
    );
    runtime.shutdown();
}

#[test]
fn runtime_shutdown_request_stops_runtime() {
    let runtime = DaemonRuntime::spawn_with_runner(
        relay_config("shutdown-request"),
        EventRunner {
            event_tx: mpsc::channel().0,
        },
    )
    .expect("spawn runtime");
    let handle = runtime.handle();

    let response = handle.request(DaemonRequest::Shutdown);
    assert_eq!(
        response,
        DaemonResponse::ok(json!({ "status": "shutting_down" }))
    );

    let ping = handle.request(DaemonRequest::Ping);
    assert!(!ping.ok);
    assert_eq!(
        ping.error.expect("runtime stopped error").code,
        "runtime_stopped"
    );
    runtime.shutdown();
}

#[test]
fn runtime_routes_push_request_through_runner() {
    let (push_tx, push_rx) = mpsc::channel();
    let runtime = DaemonRuntime::spawn_with_runner(
        relay_config("push-request-routing"),
        PushRequestRunner { push_tx },
    )
    .expect("spawn runtime");

    let response = runtime.handle().request(DaemonRequest::Push {
        path: PathBuf::from("Roadmap.md"),
        assume_yes: true,
        confirm_dangerous: false,
    });
    let job = push_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("push job");

    assert!(response.ok);
    assert_eq!(job.target_path, PathBuf::from("Roadmap.md"));
    assert!(job.assume_yes);
    assert!(!job.confirm_dangerous);
    runtime.shutdown();
}

#[test]
fn runtime_prime_virtual_mounts_queues_root_and_mount_point_refreshes() {
    let config = relay_config("prime-virtual-mount");
    let mut store = SqliteStateStore::open(config.state_root.clone()).expect("open store");
    store
        .save_mount(
            MountConfig::new(
                MountId::new("notion-main"),
                "notion",
                temp_root("prime-virtual-root"),
            )
            .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    drop(store);

    let (refresh_tx, refresh_rx) = mpsc::channel();
    let runtime = DaemonRuntime::spawn_with_runner(config, RefreshRecordingRunner { refresh_tx })
        .expect("spawn runtime");

    runtime
        .handle()
        .prime_virtual_mounts()
        .expect("prime virtual mounts");

    let mut refreshes = vec![
        refresh_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first refresh"),
        refresh_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("second refresh"),
    ];
    refreshes.sort();

    assert_eq!(
        refreshes,
        vec![
            ("notion-main".to_string(), "mount:notion-main".to_string()),
            (
                "notion-main".to_string(),
                ROOT_CONTAINER_IDENTIFIER.to_string()
            ),
        ]
    );
    runtime.shutdown();
}

#[test]
fn runtime_background_virtual_refreshes_walk_breadth_first() {
    let config = relay_config("prime-virtual-breadth-first");
    let mount_id = MountId::new("notion-main");
    let mut store = SqliteStateStore::open(config.state_root.clone()).expect("open store");
    store
        .save_mount(
            MountConfig::new(
                mount_id.clone(),
                "notion",
                temp_root("prime-virtual-breadth-first-root"),
            )
            .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    drop(store);

    let (refresh_tx, refresh_rx) = mpsc::channel();
    let runtime = DaemonRuntime::spawn_with_runner(
        config,
        DescendantSeedingRefreshRunner {
            refresh_tx,
            mount_id,
        },
    )
    .expect("spawn runtime");

    runtime
        .handle()
        .prime_virtual_mounts()
        .expect("prime virtual mounts");

    let root = (
        "notion-main".to_string(),
        ROOT_CONTAINER_IDENTIFIER.to_string(),
    );
    let source = ("notion-main".to_string(), "mount:notion-main".to_string());
    let page_a = ("notion-main".to_string(), "children:page-a".to_string());
    let page_b = ("notion-main".to_string(), "children:page-b".to_string());
    let page_a1 = ("notion-main".to_string(), "children:page-a1".to_string());
    let page_b1 = ("notion-main".to_string(), "children:page-b1".to_string());
    let expected_refreshes = BTreeSet::from([
        root.clone(),
        source.clone(),
        page_a.clone(),
        page_b.clone(),
        page_a1.clone(),
        page_b1.clone(),
    ]);
    let refreshed =
        collect_refreshes_until(&refresh_rx, &expected_refreshes, Duration::from_secs(10));

    for expected in [&root, &source, &page_a, &page_b, &page_a1, &page_b1] {
        assert!(
            refreshed.contains(expected),
            "missing refresh {expected:?}; saw {refreshed:?}"
        );
    }
    assert!(
        max_position(&refreshed, [&root, &source]) < min_position(&refreshed, [&page_a, &page_b]),
        "depth-1 containers must wait for depth-0 refreshes: {refreshed:?}"
    );
    assert!(
        max_position(&refreshed, [&page_a, &page_b])
            < min_position(&refreshed, [&page_a1, &page_b1]),
        "depth-2 containers must wait for depth-1 refreshes: {refreshed:?}"
    );
    runtime.shutdown();
}

#[test]
fn runtime_scheduler_simulates_mixed_interactive_and_background_workload() {
    let mut simulation = SchedulerPolicySimulation::spawn("mixed-scheduler-policy");

    simulation.advance_to(0, SchedulerInputAction::InitialEnumeration);
    simulation.prime_virtual_mounts();
    let [mount_point_refresh, root_refresh] = simulation.expect_started_set([
        SchedulerExpectedStart::new(SchedulerOpKind::RefreshChildren, ROOT_CONTAINER_IDENTIFIER),
        SchedulerExpectedStart::new(SchedulerOpKind::RefreshChildren, "mount:notion-main"),
    ]);

    simulation.advance_to(
        5,
        SchedulerInputAction::Complete(SchedulerOpKind::RefreshChildren, "mount:notion-main"),
    );
    simulation.release(mount_point_refresh);
    simulation.expect_no_start();
    simulation.wait_for_child_refresh_queued("children:page-a");

    simulation.advance_to(
        10,
        SchedulerInputAction::InteractiveDirectoryOpen("children:page-a"),
    );
    let directory_open = simulation.request(DaemonRequest::FileProviderChildren {
        mount_id: "notion-main".to_string(),
        container_identifier: "children:page-a".to_string(),
    });
    let page_a_children_refresh = simulation.expect_started(SchedulerExpectedStart::new(
        SchedulerOpKind::RefreshChildren,
        "children:page-a",
    ));

    simulation.advance_to(20, SchedulerInputAction::InteractiveFileOpen("page-open"));
    let file_open = simulation.request(DaemonRequest::FileProviderRead {
        mount_id: "notion-main".to_string(),
        identifier: "page-open".to_string(),
    });
    let page_open = simulation.expect_started(SchedulerExpectedStart::new(
        SchedulerOpKind::FileProviderRead,
        "page-open",
    ));

    simulation.advance_to(
        30,
        SchedulerInputAction::Complete(SchedulerOpKind::RefreshChildren, "children:page-a"),
    );
    simulation.release(page_a_children_refresh);
    simulation.assert_response_ok(directory_open);
    simulation.expect_no_start();

    simulation.advance_to(40, SchedulerInputAction::OtherOperation("ManualSync.md"));
    let pull = simulation.request(DaemonRequest::Pull {
        path: PathBuf::from("ManualSync.md"),
    });
    simulation.expect_no_start();

    simulation.advance_to(
        50,
        SchedulerInputAction::Complete(SchedulerOpKind::FileProviderRead, "page-open"),
    );
    simulation.release(page_open);
    simulation.assert_response_ok(file_open);
    let manual_pull = simulation.expect_started(SchedulerExpectedStart::new(
        SchedulerOpKind::Pull,
        "ManualSync.md",
    ));

    simulation.advance_to(
        60,
        SchedulerInputAction::Complete(SchedulerOpKind::Pull, "ManualSync.md"),
    );
    simulation.release(manual_pull);
    simulation.assert_response_ok(pull);
    simulation.expect_no_start();

    simulation.advance_to(
        80,
        SchedulerInputAction::Complete(SchedulerOpKind::RefreshChildren, ROOT_CONTAINER_IDENTIFIER),
    );
    simulation.release(root_refresh);
    let page_b_refresh = simulation.expect_started(SchedulerExpectedStart::new(
        SchedulerOpKind::RefreshChildren,
        "children:page-b",
    ));

    simulation.advance_to(
        100,
        SchedulerInputAction::Complete(SchedulerOpKind::RefreshChildren, "children:page-b"),
    );
    simulation.release(page_b_refresh);
    let [page_a1_refresh, page_b1_refresh] = simulation.expect_started_set([
        SchedulerExpectedStart::new(SchedulerOpKind::RefreshChildren, "children:page-a1"),
        SchedulerExpectedStart::new(SchedulerOpKind::RefreshChildren, "children:page-b1"),
    ]);

    simulation.advance_to(
        110,
        SchedulerInputAction::Complete(SchedulerOpKind::RefreshChildren, "children:page-a1"),
    );
    simulation.release(page_a1_refresh);
    simulation.expect_no_start();

    simulation.advance_to(
        120,
        SchedulerInputAction::Complete(SchedulerOpKind::RefreshChildren, "children:page-b1"),
    );
    simulation.release(page_b1_refresh);
    simulation.expect_no_start();

    simulation.assert_timeline([
        SchedulerTimelineEntry::new(0, SchedulerOpKind::RefreshChildren, "mount:notion-main"),
        SchedulerTimelineEntry::new(
            0,
            SchedulerOpKind::RefreshChildren,
            ROOT_CONTAINER_IDENTIFIER,
        ),
        SchedulerTimelineEntry::new(10, SchedulerOpKind::RefreshChildren, "children:page-a"),
        SchedulerTimelineEntry::new(20, SchedulerOpKind::FileProviderRead, "page-open"),
        SchedulerTimelineEntry::new(50, SchedulerOpKind::Pull, "ManualSync.md"),
        SchedulerTimelineEntry::new(80, SchedulerOpKind::RefreshChildren, "children:page-b"),
        SchedulerTimelineEntry::new(100, SchedulerOpKind::RefreshChildren, "children:page-a1"),
        SchedulerTimelineEntry::new(100, SchedulerOpKind::RefreshChildren, "children:page-b1"),
    ]);
    simulation.shutdown();
}

#[test]
fn default_runner_virtual_fs_children_is_cache_only() {
    let state_root = temp_root("cache-only-children-state");
    let mount_root = temp_root("cache-only-children-mount");
    let mount_id = MountId::new("notion-main");
    let mut store = SqliteStateStore::open(state_root.clone()).expect("open store");
    store
        .save_mount(
            MountConfig::new(mount_id.clone(), "notion", mount_root)
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    store
        .save_entity(EntityRecord::new(
            mount_id.clone(),
            RemoteId::new("page-1"),
            EntityKind::Page,
            "Roadmap",
            "Roadmap/page.md",
        ))
        .expect("save entity");
    drop(store);

    let response = DefaultRuntimeJobRunner.run_virtual_fs_children(
        state_root,
        mount_id.0.clone(),
        "mount:notion-main".to_string(),
    );

    assert!(
        response.ok,
        "cached child listing should not require connector credentials: {:?}",
        response.error
    );
    let payload = response.payload.expect("children payload");
    let report: VirtualFsChildrenReport =
        serde_json::from_value(payload).expect("decode children report");
    assert!(
        report
            .children
            .iter()
            .any(|child| child.filename == "Roadmap")
    );
}

#[test]
fn default_runner_virtual_fs_children_rejects_plain_files_mount() {
    let state_root = temp_root("plain-files-children-state");
    let mount_root = temp_root("plain-files-children-mount");
    let mount_id = MountId::new("notion-main");
    let mut store = SqliteStateStore::open(state_root.clone()).expect("open store");
    store
        .save_mount(MountConfig::new(mount_id.clone(), "notion", mount_root))
        .expect("save mount");
    drop(store);

    let response = DefaultRuntimeJobRunner.run_virtual_fs_children(
        state_root,
        mount_id.0.clone(),
        ROOT_CONTAINER_IDENTIFIER.to_string(),
    );

    assert!(!response.ok);
    let error = response.error.expect("plain-files error");
    assert_eq!(error.code, "unsupported");
    assert_eq!(
        error.message,
        "unsupported feature: plain-files mounts do not support virtual filesystem operations"
    );
}

#[test]
fn virtual_projection_root_children_lists_mount_points_for_shared_root() {
    use locality_core::model::MountId;
    use locality_store::{InMemoryStateStore, MountConfig, MountRepository, ProjectionMode};
    use localityd::virtual_projection::virtual_projection_root_children;

    let mut store = InMemoryStateStore::new();
    let root = std::env::temp_dir().join("locality-shared-root-test");
    store
        .save_mount(
            MountConfig::new(
                MountId::new("notion-main"),
                "notion",
                root.join("notion-main"),
            )
            .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save notion");
    store
        .save_mount(
            MountConfig::new(
                MountId::new("notion-my-company"),
                "notion",
                root.join("notion-my-company"),
            )
            .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save notion company");
    store
        .save_mount(
            MountConfig::new(
                MountId::new("google-docs-main"),
                "google-docs",
                root.join("google-docs-main"),
            )
            .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save google docs");

    let report = virtual_projection_root_children(&store, &root, ProjectionMode::LinuxFuse)
        .expect("root children");

    let filenames = report
        .children
        .iter()
        .map(|child| child.filename.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        filenames,
        vec!["google-docs-main", "notion-main", "notion-my-company"]
    );
    assert!(
        report
            .children
            .iter()
            .all(|child| child.identifier.starts_with("m:"))
    );
}

#[test]
fn runtime_virtual_projection_root_children_lists_mount_points_for_shared_root() {
    let config = relay_config("shared-root-runtime-children");
    let root = temp_root("shared-root-runtime-projection");
    let mut store = SqliteStateStore::open(config.state_root.clone()).expect("open store");
    store
        .save_mount(
            MountConfig::new(
                MountId::new("notion-main"),
                "notion",
                root.join("notion-main"),
            )
            .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save notion");
    store
        .save_mount(
            MountConfig::new(
                MountId::new("notion-my-company"),
                "notion",
                root.join("notion-my-company"),
            )
            .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save notion company");
    store
        .save_mount(
            MountConfig::new(
                MountId::new("google-docs-main"),
                "google-docs",
                root.join("google-docs-main"),
            )
            .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save google docs");
    drop(store);

    let runtime = DaemonRuntime::spawn(config).expect("spawn runtime");
    let response = runtime
        .handle()
        .request(DaemonRequest::VirtualProjectionRootChildren {
            projection_root: root,
            projection: ProjectionMode::LinuxFuse,
        });
    runtime.shutdown();

    assert!(
        response.ok,
        "shared projection root children request failed: {:?}",
        response.error
    );
    let payload = response.payload.expect("children payload");
    let report: VirtualFsChildrenReport =
        serde_json::from_value(payload).expect("decode children report");
    let filenames = report
        .children
        .iter()
        .map(|child| child.filename.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        filenames,
        vec!["google-docs-main", "notion-main", "notion-my-company"]
    );
    assert!(
        report
            .children
            .iter()
            .all(|child| child.identifier.starts_with("m:"))
    );
}

#[test]
fn default_runner_marks_hydrated_write_dirty() {
    let fixture = EventFixture::new("dirty-write");
    fixture.write_hydrated_page("Original body.");
    fixture.write_hydrated_page("Edited body.");

    let report = DefaultRuntimeJobRunner
        .run_file_event(fixture.state_root.clone(), fixture.write_event())
        .expect("run file event");

    assert_eq!(report.report.marked_dirty, 1);
    assert_eq!(report.freshness_jobs.len(), 1);
    assert_eq!(
        report.freshness_jobs[0].remote_id,
        Some(fixture.remote_id.clone())
    );
    assert_eq!(report.freshness_jobs[0].kind, SyncJobKind::ObserveEntity);
    assert_eq!(report.freshness_jobs[0].reason, ChangeHintKind::LocalEdited);
    let store = SqliteStateStore::open(fixture.state_root).expect("open store");
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Dirty);
    let freshness = store
        .get_freshness_state(&fixture.mount_id, &fixture.remote_id)
        .expect("get freshness")
        .expect("freshness");
    assert_eq!(freshness.tier, FreshnessTier::Hot);
    assert!(freshness.last_local_change_at.is_some());
}

#[test]
fn default_runner_marks_frontmatter_only_write_dirty() {
    let fixture = EventFixture::new("frontmatter-write");
    fixture.write_hydrated_page("Original body.");
    fixture.write_hydrated_page_with_frontmatter(
        "loc:\n  id: page-1\n  type: page\ntitle: Updated Roadmap\n",
        "Original body.",
    );

    let report = DefaultRuntimeJobRunner
        .run_file_event(fixture.state_root.clone(), fixture.write_event())
        .expect("run file event");

    assert_eq!(report.report.marked_dirty, 1);
    let store = SqliteStateStore::open(fixture.state_root).expect("open store");
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Dirty);
}

#[test]
fn default_runner_ignores_clean_daemon_projection_write() {
    let fixture = EventFixture::new("clean-write");
    fixture.write_hydrated_page("Original body.");

    let report = DefaultRuntimeJobRunner
        .run_file_event(fixture.state_root.clone(), fixture.write_event())
        .expect("run file event");

    assert_eq!(report.report.ignored_events, 1);
    let store = SqliteStateStore::open(fixture.state_root).expect("open store");
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Hydrated);
}

#[test]
fn default_runner_queues_stub_read_hydration() {
    let fixture = EventFixture::new_with_state("stub-read", HydrationState::Stub);

    let report = DefaultRuntimeJobRunner
        .run_file_event(fixture.state_root.clone(), fixture.read_event())
        .expect("run file event");

    assert_eq!(report.report.queued_hydrations, 1);
    assert_eq!(report.queued_hydrations.len(), 1);
    assert_eq!(report.freshness_jobs.len(), 1);
    assert_eq!(
        report.freshness_jobs[0].remote_id,
        Some(fixture.remote_id.clone())
    );
    assert_eq!(report.freshness_jobs[0].reason, ChangeHintKind::FileOpened);
    let request = &report.queued_hydrations[0];
    assert_eq!(request.mount_id, fixture.mount_id);
    assert_eq!(request.remote_id, fixture.remote_id);
    assert_eq!(request.path, fixture.page_path());
    assert_eq!(request.target_state, HydrationState::Hydrated);
    assert_eq!(request.reason, HydrationReason::StubRead);
    let store = SqliteStateStore::open(fixture.state_root).expect("open store");
    let freshness = store
        .get_freshness_state(&fixture.mount_id, &fixture.remote_id)
        .expect("get freshness")
        .expect("freshness");
    assert_eq!(freshness.tier, FreshnessTier::Hot);
    assert!(freshness.last_opened_at.is_some());
}

#[test]
fn default_runner_ignores_database_directory_read() {
    let fixture = EventFixture::new("database-read");
    let database_id = RemoteId::new("database-1");
    let database_path = PathBuf::from("Tasks");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).expect("open store");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                database_id,
                EntityKind::Database,
                "Tasks",
                database_path.clone(),
            )
            .with_hydration(HydrationState::Stub),
        )
        .expect("save database entity");

    let report = DefaultRuntimeJobRunner
        .run_file_event(
            fixture.state_root.clone(),
            FileEvent {
                path: fixture.mount_root.join(database_path),
                kind: FileEventKind::Read,
            },
        )
        .expect("run file event");

    assert_eq!(report.report.ignored_events, 1);
    assert!(report.queued_hydrations.is_empty());
}

#[test]
fn runtime_drains_hydration_queued_by_read_event() {
    let (hydrated_tx, hydrated_rx) = mpsc::channel();
    let runtime = DaemonRuntime::spawn_with_runner(
        relay_config("read-event-hydration"),
        ReadHydrationRunner {
            hydrated: hydrated_tx,
        },
    )
    .expect("spawn runtime");

    runtime
        .handle()
        .file_event(FileEvent {
            path: PathBuf::from("Roadmap.md"),
            kind: FileEventKind::Read,
        })
        .expect("submit read event");

    let request = hydrated_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("hydration drained");
    assert_eq!(request.mount_id, MountId::new("notion-main"));
    assert_eq!(request.remote_id, RemoteId::new("page-1"));
    assert_eq!(request.reason, HydrationReason::StubRead);
    runtime.shutdown();
}

#[test]
fn runtime_queues_explicit_hydration_request() {
    let (hydrated_tx, hydrated_rx) = mpsc::channel();
    let runtime = DaemonRuntime::spawn_with_runner(
        relay_config("explicit-hydration"),
        ReadHydrationRunner {
            hydrated: hydrated_tx,
        },
    )
    .expect("spawn runtime");

    let response = runtime.handle().request(DaemonRequest::Hydrate {
        mount_id: "notion-main".to_string(),
        remote_id: "page-1".to_string(),
        path: PathBuf::from("Roadmap.md"),
    });

    assert!(response.ok);
    assert_eq!(
        response
            .payload
            .as_ref()
            .and_then(|payload| payload.get("queued"))
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    let request = hydrated_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("hydration drained");
    assert_eq!(request.mount_id, MountId::new("notion-main"));
    assert_eq!(request.remote_id, RemoteId::new("page-1"));
    assert_eq!(request.path, PathBuf::from("Roadmap.md"));
    assert_eq!(request.reason, HydrationReason::FileOpen);
    runtime.shutdown();
}

#[test]
fn runtime_drains_freshness_queued_by_file_event() {
    let (freshness_tx, freshness_rx) = mpsc::channel();
    let runtime = DaemonRuntime::spawn_with_runner(
        relay_config("file-event-freshness"),
        FreshnessFromEventRunner { freshness_tx },
    )
    .expect("spawn runtime");

    runtime
        .handle()
        .file_event(FileEvent {
            path: PathBuf::from("Roadmap.md"),
            kind: FileEventKind::Write,
        })
        .expect("submit write event");

    let job = freshness_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("freshness drained");
    assert_eq!(job.mount_id, MountId::new("notion-main"));
    assert_eq!(job.remote_id, Some(RemoteId::new("page-1")));
    assert_eq!(job.kind, SyncJobKind::ObserveEntity);
    assert_eq!(job.reason, ChangeHintKind::LocalEdited);
    runtime.shutdown();
}

#[test]
fn runtime_drains_freshness_queued_by_virtual_write() {
    let (freshness_tx, freshness_rx) = mpsc::channel();
    let config = relay_config("virtual-write-freshness");
    let signal_path = live_mode_state_change_signal_path(&config.state_root);
    let runtime =
        DaemonRuntime::spawn_with_runner(config, FreshnessFromEventRunner { freshness_tx })
            .expect("spawn runtime");

    let response = runtime
        .handle()
        .request(DaemonRequest::VirtualFsCommitWrite {
            mount_id: "notion-main".to_string(),
            identifier: "page-1".to_string(),
            contents_base64: "ZWRpdGVk".to_string(),
        });

    assert!(response.ok);
    let job = freshness_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("freshness drained");
    assert_eq!(job.mount_id, MountId::new("notion-main"));
    assert_eq!(job.remote_id, Some(RemoteId::new("page-1")));
    assert_eq!(job.kind, SyncJobKind::ObserveEntity);
    assert_eq!(job.reason, ChangeHintKind::LocalEdited);
    assert!(
        signal_path.exists(),
        "virtual writes should publish the explicit Live Mode wake signal"
    );
    runtime.shutdown();
}

#[test]
fn runtime_queues_auto_push_for_enrolled_virtual_write() {
    let config = relay_config("virtual-write-auto-push");
    let mount_root = temp_root("virtual-write-auto-push-root");
    let mut store = SqliteStateStore::open(config.state_root.clone()).expect("open store");
    store
        .save_mount(
            MountConfig::new(MountId::new("notion-main"), "notion", mount_root.clone())
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    let mut enrollment = AutoSaveEnrollmentRecord::new(
        MountId::new("notion-main"),
        "Roadmap.md",
        AutoSaveOrigin::LocalityCreated,
        "now",
    );
    enrollment.remote_id = Some(RemoteId::new("page-1"));
    store
        .save_auto_save_enrollment(enrollment)
        .expect("save enrollment");
    drop(store);

    let (push_tx, push_rx) = mpsc::channel();
    let runtime = DaemonRuntime::spawn_with_runner(config, VirtualWriteAutoPushRunner { push_tx })
        .expect("spawn runtime");

    let response = runtime
        .handle()
        .request(DaemonRequest::VirtualFsCommitWrite {
            mount_id: "notion-main".to_string(),
            identifier: "page-1".to_string(),
            contents_base64: "ZWRpdGVk".to_string(),
        });

    assert!(response.ok);
    let job = push_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("auto-push job");
    assert_eq!(job.target_path, mount_root.join("Roadmap.md"));
    assert!(job.assume_yes);
    assert!(!job.confirm_dangerous);
    runtime.shutdown();
}

#[test]
fn runtime_prioritizes_auto_push_from_virtual_write_over_older_pending_requests() {
    let config = relay_config("virtual-write-auto-push-priority");
    let mount_root = temp_root("virtual-write-auto-push-priority-root");
    let mut store = SqliteStateStore::open(config.state_root.clone()).expect("open store");
    store
        .save_mount(
            MountConfig::new(MountId::new("notion-main"), "notion", mount_root.clone())
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    let mut enrollment = AutoSaveEnrollmentRecord::new(
        MountId::new("notion-main"),
        "Roadmap.md",
        AutoSaveOrigin::LocalityCreated,
        "now",
    );
    enrollment.remote_id = Some(RemoteId::new("page-1"));
    store
        .save_auto_save_enrollment(enrollment)
        .expect("save enrollment");
    drop(store);

    let (started_tx, started_rx) = mpsc::channel();
    let (order_tx, order_rx) = mpsc::channel();
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let runtime = DaemonRuntime::spawn_with_runner(
        config,
        BlockingVirtualWriteAutoPushRunner {
            started: started_tx,
            order_tx,
            release: Arc::clone(&release),
        },
    )
    .expect("spawn runtime");

    let write_handle = runtime.handle();
    let write_thread = thread::spawn(move || {
        write_handle.request(DaemonRequest::VirtualFsCommitWrite {
            mount_id: "notion-main".to_string(),
            identifier: "page-1".to_string(),
            contents_base64: "ZWRpdGVk".to_string(),
        })
    });
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("virtual write started");

    let pull_handle = runtime.handle();
    let pull_thread = thread::spawn(move || {
        pull_handle.request(DaemonRequest::Pull {
            path: PathBuf::from("Roadmap.md"),
        })
    });
    wait_until_pending_requests(&runtime.handle(), 1);

    release_blocked_runner(&release);
    assert!(write_thread.join().expect("write thread").ok);

    assert_eq!(
        order_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first queued job"),
        "auto_push:Roadmap.md"
    );
    assert_eq!(
        order_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("second queued job"),
        "pull:Roadmap.md"
    );
    assert!(pull_thread.join().expect("pull thread").ok);
    runtime.shutdown();
}

#[test]
fn runtime_queues_remote_fast_forward_from_freshness_report() {
    let config = relay_config("remote-fast-forward");
    let mount_root = temp_root("remote-fast-forward-mount");
    seed_clean_remote_changed_page(&config.state_root, &mount_root);
    let (hydrated_tx, hydrated_rx) = mpsc::channel();
    let runtime = DaemonRuntime::spawn_with_runner(
        config,
        AutoFastForwardRunner {
            hydrated: hydrated_tx,
        },
    )
    .expect("spawn runtime");

    runtime
        .handle()
        .file_event(FileEvent {
            path: mount_root.join("Roadmap.md"),
            kind: FileEventKind::Write,
        })
        .expect("submit freshness event");

    let request = hydrated_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("auto fast-forward hydration drained");
    assert_eq!(request.mount_id, MountId::new("notion-main"));
    assert_eq!(request.remote_id, RemoteId::new("page-1"));
    assert_eq!(request.reason, HydrationReason::RemoteFastForward);
    runtime.shutdown();
}

#[test]
fn runtime_remote_fast_forward_request_uses_daemon_hydration_queue() {
    let config = relay_config("remote-fast-forward-request");
    let mount_root = temp_root("remote-fast-forward-request-mount");
    seed_clean_remote_changed_page(&config.state_root, &mount_root);
    let (hydrated_tx, hydrated_rx) = mpsc::channel();
    let runtime = DaemonRuntime::spawn_with_runner(
        config,
        AutoFastForwardRunner {
            hydrated: hydrated_tx,
        },
    )
    .expect("spawn runtime");

    let response = runtime.handle().request(DaemonRequest::RemoteFastForward {
        mount_id: "notion-main".to_string(),
        remote_id: "page-1".to_string(),
        path: PathBuf::from("Roadmap.md"),
    });

    assert!(
        response.ok,
        "remote fast-forward request failed: {response:?}"
    );
    let request = hydrated_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("remote fast-forward hydration drained");
    assert_eq!(request.mount_id, MountId::new("notion-main"));
    assert_eq!(request.remote_id, RemoteId::new("page-1"));
    assert_eq!(request.reason, HydrationReason::LiveModeRemoteFastForward);
    runtime.shutdown();
}

#[test]
fn runtime_queues_child_refresh_when_remote_fast_forward_discovers_child_link_diff() {
    let config = relay_config("remote-fast-forward-discovery");
    let mount_root = temp_root("remote-fast-forward-discovery-mount");
    seed_clean_page_with_child_links(
        &config.state_root,
        &mount_root,
        ProjectionMode::LinuxFuse,
        ["child-a"],
    );
    let (hydrated_tx, hydrated_rx) = mpsc::channel();
    let (refresh_tx, refresh_rx) = mpsc::channel();
    let runtime = DaemonRuntime::spawn_with_runner(
        config,
        DiscoveryHintRunner {
            hydrated: hydrated_tx,
            refresh_tx,
            next_child_ids: vec!["child-a".to_string(), "child-b".to_string()],
        },
    )
    .expect("spawn runtime");

    let response = runtime.handle().request(DaemonRequest::RemoteFastForward {
        mount_id: "notion-main".to_string(),
        remote_id: "page-1".to_string(),
        path: PathBuf::from("Roadmap/page.md"),
    });

    assert!(
        response.ok,
        "remote fast-forward request failed: {response:?}"
    );
    let request = hydrated_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("remote fast-forward hydration drained");
    assert_eq!(request.reason, HydrationReason::LiveModeRemoteFastForward);
    assert_eq!(
        refresh_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("child refresh queued from discovery hint"),
        ("notion-main".to_string(), "children:page-1".to_string())
    );
    runtime.shutdown();
}

#[cfg(target_os = "macos")]
#[test]
fn runtime_refreshes_macos_visible_replica_after_remote_fast_forward() {
    let config = relay_config("remote-fast-forward-refresh-visible");
    let mount_root = temp_root("remote-fast-forward-refresh-visible-mount").join("notion");
    let visible_path =
        seed_clean_remote_changed_macos_file_provider_page(&config.state_root, &mount_root);
    let (hydrated_tx, hydrated_rx) = mpsc::channel();
    let runtime = DaemonRuntime::spawn_with_runner(
        config,
        MacosProjectionFastForwardRunner {
            hydrated: hydrated_tx,
        },
    )
    .expect("spawn runtime");

    runtime
        .handle()
        .file_event(FileEvent {
            path: visible_path.clone(),
            kind: FileEventKind::Write,
        })
        .expect("submit freshness event");

    let request = hydrated_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("auto fast-forward hydration drained");
    assert_eq!(request.mount_id, MountId::new("notion-main"));
    assert_eq!(request.remote_id, RemoteId::new("page-1"));
    assert_eq!(request.reason, HydrationReason::RemoteFastForward);

    wait_for_file_contains(&visible_path, "Remote body.");
    let visible = std::fs::read_to_string(&visible_path).expect("read visible");
    assert!(!visible.contains("Original body."));
    runtime.shutdown();
}

#[test]
fn runtime_delays_remote_fast_forward_for_recently_opened_file() {
    let config = relay_config("remote-fast-forward-active");
    let mount_root = temp_root("remote-fast-forward-active-mount");
    seed_clean_remote_changed_page(&config.state_root, &mount_root);
    mark_page_recently_opened(&config.state_root);
    let (hydrated_tx, hydrated_rx) = mpsc::channel();
    let runtime = DaemonRuntime::spawn_with_runner(
        config,
        AutoFastForwardRunner {
            hydrated: hydrated_tx,
        },
    )
    .expect("spawn runtime");

    runtime
        .handle()
        .file_event(FileEvent {
            path: mount_root.join("Roadmap.md"),
            kind: FileEventKind::Write,
        })
        .expect("submit freshness event");

    assert!(
        hydrated_rx
            .recv_timeout(Duration::from_millis(150))
            .is_err(),
        "recently opened files should not be auto-replaced immediately"
    );
    runtime.shutdown();
}

#[test]
fn runtime_drains_persisted_hydration_on_startup() {
    let config = relay_config("persisted-hydration");
    let mount_root = temp_root("persisted-hydration-mount");
    let mut store = SqliteStateStore::open(config.state_root.clone()).expect("open store");
    store
        .save_mount(MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            mount_root.clone(),
        ))
        .expect("save mount");
    store
        .upsert_hydration_job(HydrationJobRecord {
            mount_id: MountId::new("notion-main"),
            remote_id: RemoteId::new("page-1"),
            path: mount_root.join("Roadmap.md"),
            target_state: HydrationState::Hydrated,
            reason: HydrationReason::Policy,
            attempts: 0,
            last_error: None,
        })
        .expect("save hydration job");
    drop(store);

    let (hydrated_tx, hydrated_rx) = mpsc::channel();
    let runtime = DaemonRuntime::spawn_with_runner(
        config.clone(),
        ReadHydrationRunner {
            hydrated: hydrated_tx,
        },
    )
    .expect("spawn runtime");

    let request = hydrated_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("persisted hydration drained");
    assert_eq!(request.mount_id, MountId::new("notion-main"));
    assert_eq!(request.remote_id, RemoteId::new("page-1"));
    assert_eq!(request.reason, HydrationReason::Policy);

    assert_hydration_jobs_drained(config.state_root);
    runtime.shutdown();
}

fn assert_hydration_jobs_drained(state_root: PathBuf) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let store = SqliteStateStore::open(state_root.clone()).expect("reopen store");
        if store
            .list_hydration_jobs()
            .expect("list hydration jobs")
            .is_empty()
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "hydration jobs did not drain"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "macos")]
fn wait_for_file_contains(path: &Path, needle: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if std::fs::read_to_string(path).is_ok_and(|contents| contents.contains(needle)) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "`{}` did not contain `{needle}`",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

struct SchedulerPolicySimulation {
    runtime: DaemonRuntime,
    handle: DaemonRuntimeHandle,
    state: Arc<ScriptedSchedulerState>,
    started_rx: mpsc::Receiver<SchedulerStartedOperation>,
    current_tick: u64,
    input_log: Vec<SchedulerInputEvent>,
    timeline: Vec<SchedulerTimelineEntry>,
}

impl SchedulerPolicySimulation {
    fn spawn(name: &str) -> Self {
        let config = relay_config(name);
        let mount_id = MountId::new("notion-main");
        let mut store = SqliteStateStore::open(config.state_root.clone()).expect("open store");
        store
            .save_mount(
                MountConfig::new(
                    mount_id.clone(),
                    "notion",
                    temp_root(&format!("{name}-mount")),
                )
                .projection(ProjectionMode::LinuxFuse),
            )
            .expect("save mount");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("page-a"),
                EntityKind::Page,
                "A",
                "A/page.md",
            ))
            .expect("save cached page");
        drop(store);

        let (started_tx, started_rx) = mpsc::channel();
        let state = Arc::new(ScriptedSchedulerState {
            started_tx,
            next_sequence: AtomicUsize::new(0),
            released: Mutex::new(BTreeSet::new()),
            release_condvar: Condvar::new(),
            mount_id,
        });
        let runtime = DaemonRuntime::spawn_with_runner(
            config,
            ScriptedSchedulerRunner {
                state: Arc::clone(&state),
            },
        )
        .expect("spawn runtime");
        let handle = runtime.handle();

        Self {
            runtime,
            handle,
            state,
            started_rx,
            current_tick: 0,
            input_log: Vec::new(),
            timeline: Vec::new(),
        }
    }

    fn advance_to(&mut self, tick: u64, action: SchedulerInputAction) {
        assert!(
            tick >= self.current_tick,
            "scheduler simulation ticks must be monotonic: {} -> {}",
            self.current_tick,
            tick
        );
        self.current_tick = tick;
        self.input_log.push(SchedulerInputEvent {
            tick,
            action: action.describe(),
        });
    }

    fn prime_virtual_mounts(&self) {
        self.handle
            .prime_virtual_mounts()
            .expect("prime virtual mounts");
    }

    fn request(&self, request: DaemonRequest) -> thread::JoinHandle<DaemonResponse> {
        let handle = self.handle.clone();
        thread::spawn(move || handle.request(request))
    }

    fn expect_started(&mut self, expected: SchedulerExpectedStart) -> SchedulerStartedOperation {
        let operation = self
            .started_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap_or_else(|error| {
                panic!(
                    "expected scheduler operation {expected:?}; input log: {:?}; error: {error}",
                    self.input_log
                )
            });
        assert_eq!(
            operation.expected_start(),
            expected,
            "unexpected scheduler operation; input log: {:?}",
            self.input_log
        );
        self.record_started(std::slice::from_ref(&operation));
        operation
    }

    fn expect_started_set<const N: usize>(
        &mut self,
        expected: [SchedulerExpectedStart; N],
    ) -> [SchedulerStartedOperation; N] {
        let expected_set = expected
            .into_iter()
            .map(|expected| (expected.kind, expected.target))
            .collect::<BTreeSet<_>>();
        let mut operations = Vec::with_capacity(N);
        for _ in 0..N {
            operations.push(
                self.started_rx
                    .recv_timeout(Duration::from_secs(1))
                    .unwrap_or_else(|error| {
                        panic!(
                            "expected scheduler operations {expected_set:?}; input log: {:?}; error: {error}",
                            self.input_log
                        )
                    }),
            );
        }
        let actual_set = operations
            .iter()
            .map(|operation| (operation.kind, operation.target.clone()))
            .collect::<BTreeSet<_>>();
        assert_eq!(
            actual_set, expected_set,
            "unexpected scheduler operation set; input log: {:?}",
            self.input_log
        );
        operations.sort_by(|left, right| {
            left.kind
                .cmp(&right.kind)
                .then_with(|| left.target.cmp(&right.target))
                .then_with(|| left.sequence.cmp(&right.sequence))
        });
        self.record_started(&operations);
        operations
            .try_into()
            .unwrap_or_else(|_| panic!("operation count should match expected count"))
    }

    fn expect_no_start(&self) {
        match self.started_rx.recv_timeout(Duration::from_millis(75)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("scheduler operation channel disconnected")
            }
            Ok(operation) => panic!(
                "unexpected scheduler operation {operation:?}; input log: {:?}",
                self.input_log
            ),
        }
    }

    fn wait_for_child_refresh_queued(&self, container_identifier: &str) {
        let target = format!("notion-main:{container_identifier}");
        for _ in 0..40 {
            let response = self.handle.request(DaemonRequest::DebugQueueStatus);
            assert!(response.ok, "debug queue request failed: {response:?}");
            let payload = response.payload.expect("debug queue payload");
            let snapshot: DaemonDebugQueueStatus =
                serde_json::from_value(payload).expect("decode debug queue");
            if snapshot.sections.iter().any(|section| {
                section.name == "child_refreshes"
                    && section
                        .items
                        .iter()
                        .any(|item| item.target.as_deref() == Some(target.as_str()))
            }) {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!(
            "child refresh {target} was not queued; input log: {:?}",
            self.input_log
        );
    }

    fn release(&self, operation: SchedulerStartedOperation) {
        self.state.release(operation.sequence);
    }

    fn assert_response_ok(&self, response: thread::JoinHandle<DaemonResponse>) {
        let response = response.join().expect("request thread");
        assert!(response.ok, "request failed: {response:?}");
    }

    fn assert_timeline<const N: usize>(&self, expected: [SchedulerTimelineEntry; N]) {
        assert_eq!(
            self.timeline.as_slice(),
            &expected,
            "unexpected scheduler timeline; input log: {:?}",
            self.input_log
        );
    }

    fn shutdown(self) {
        self.runtime.shutdown();
    }

    fn record_started(&mut self, operations: &[SchedulerStartedOperation]) {
        self.timeline.extend(operations.iter().map(|operation| {
            SchedulerTimelineEntry::new(
                self.current_tick,
                operation.kind,
                operation.target.as_str(),
            )
        }));
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SchedulerInputEvent {
    tick: u64,
    action: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SchedulerInputAction {
    InitialEnumeration,
    InteractiveDirectoryOpen(&'static str),
    InteractiveFileOpen(&'static str),
    OtherOperation(&'static str),
    Complete(SchedulerOpKind, &'static str),
}

impl SchedulerInputAction {
    fn describe(&self) -> String {
        match self {
            Self::InitialEnumeration => "initial enumeration".to_string(),
            Self::InteractiveDirectoryOpen(container) => {
                format!("interactive directory open {container}")
            }
            Self::InteractiveFileOpen(identifier) => {
                format!("interactive file open {identifier}")
            }
            Self::OtherOperation(path) => format!("other operation {path}"),
            Self::Complete(kind, target) => format!("complete {kind:?} {target}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SchedulerOpKind {
    RefreshChildren,
    FileProviderRead,
    Pull,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SchedulerExpectedStart {
    kind: SchedulerOpKind,
    target: String,
}

impl SchedulerExpectedStart {
    fn new(kind: SchedulerOpKind, target: &str) -> Self {
        Self {
            kind,
            target: target.to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SchedulerStartedOperation {
    sequence: usize,
    kind: SchedulerOpKind,
    target: String,
}

impl SchedulerStartedOperation {
    fn expected_start(&self) -> SchedulerExpectedStart {
        SchedulerExpectedStart {
            kind: self.kind,
            target: self.target.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SchedulerTimelineEntry {
    tick: u64,
    kind: SchedulerOpKind,
    target: String,
}

impl SchedulerTimelineEntry {
    fn new(tick: u64, kind: SchedulerOpKind, target: &str) -> Self {
        Self {
            tick,
            kind,
            target: target.to_string(),
        }
    }
}

struct ScriptedSchedulerState {
    started_tx: mpsc::Sender<SchedulerStartedOperation>,
    next_sequence: AtomicUsize,
    released: Mutex<BTreeSet<usize>>,
    release_condvar: Condvar,
    mount_id: MountId,
}

impl ScriptedSchedulerState {
    fn start_blocking(&self, kind: SchedulerOpKind, target: String) {
        let sequence = self.next_sequence.fetch_add(1, Ordering::SeqCst);
        self.started_tx
            .send(SchedulerStartedOperation {
                sequence,
                kind,
                target,
            })
            .expect("send scheduler operation");

        let mut released = self.released.lock().expect("release lock");
        while !released.remove(&sequence) {
            released = self
                .release_condvar
                .wait(released)
                .expect("wait operation release");
        }
    }

    fn release(&self, sequence: usize) {
        let mut released = self.released.lock().expect("release lock");
        released.insert(sequence);
        self.release_condvar.notify_all();
    }

    fn save_refresh_results(
        &self,
        state_root: PathBuf,
        container_identifier: &str,
    ) -> LocalityResult<VirtualFsRefreshChildrenReport> {
        let entries = match container_identifier {
            "mount:notion-main" => vec![
                EntityRecord::new(
                    self.mount_id.clone(),
                    RemoteId::new("page-a"),
                    EntityKind::Page,
                    "A",
                    "A/page.md",
                ),
                EntityRecord::new(
                    self.mount_id.clone(),
                    RemoteId::new("page-b"),
                    EntityKind::Page,
                    "B",
                    "B/page.md",
                ),
            ],
            "children:page-a" => vec![EntityRecord::new(
                self.mount_id.clone(),
                RemoteId::new("page-a1"),
                EntityKind::Page,
                "A1",
                "A/A1/page.md",
            )],
            "children:page-b" => vec![EntityRecord::new(
                self.mount_id.clone(),
                RemoteId::new("page-b1"),
                EntityKind::Page,
                "B1",
                "B/B1/page.md",
            )],
            _ => Vec::new(),
        };
        self.save_entities(state_root, entries)
    }

    fn save_entities(
        &self,
        state_root: PathBuf,
        entries: Vec<EntityRecord>,
    ) -> LocalityResult<VirtualFsRefreshChildrenReport> {
        let saved = entries.len();
        let mut store = SqliteStateStore::open(state_root).map_err(LocalityError::from)?;
        for entry in entries {
            store.save_entity(entry).map_err(LocalityError::from)?;
        }
        Ok(VirtualFsRefreshChildrenReport {
            saved,
            changed: saved > 0,
        })
    }
}

#[derive(Clone)]
struct ScriptedSchedulerRunner {
    state: Arc<ScriptedSchedulerState>,
}

impl RuntimeJobRunner for ScriptedSchedulerRunner {
    fn run_pull(&self, _state_root: PathBuf, path: PathBuf) -> DaemonResponse {
        self.state
            .start_blocking(SchedulerOpKind::Pull, path.display().to_string());
        DaemonResponse::ok(json!({ "command": "pull" }))
    }

    fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> DaemonResponse {
        DaemonResponse::error("unexpected_push", "push should not run")
    }

    fn run_scheduled_pull(
        &self,
        _state_root: PathBuf,
        _tick: PullSchedulerTick,
        _policy: HydrationPolicy,
    ) -> LocalityResult<ScheduledPullRuntimeReport> {
        Err(LocalityError::InvalidState(
            "scheduled pull should not run".to_string(),
        ))
    }

    fn run_hydration(
        &self,
        _state_root: PathBuf,
        _request: HydrationRequest,
    ) -> LocalityResult<HydrationOutcome> {
        Err(LocalityError::InvalidState(
            "hydration should not run".to_string(),
        ))
    }

    fn run_virtual_fs_refresh_children(
        &self,
        state_root: PathBuf,
        _mount_id: String,
        container_identifier: String,
    ) -> LocalityResult<VirtualFsRefreshChildrenReport> {
        self.state.start_blocking(
            SchedulerOpKind::RefreshChildren,
            container_identifier.clone(),
        );
        self.state
            .save_refresh_results(state_root, &container_identifier)
    }

    fn run_virtual_fs_children(
        &self,
        _state_root: PathBuf,
        mount_id: String,
        container_identifier: String,
    ) -> DaemonResponse {
        DaemonResponse::ok(json!({
            "mount_id": mount_id,
            "container_identifier": container_identifier,
            "children": []
        }))
    }

    fn run_file_provider_read(
        &self,
        _state_root: PathBuf,
        mount_id: String,
        identifier: String,
    ) -> DaemonResponse {
        self.state
            .start_blocking(SchedulerOpKind::FileProviderRead, identifier.clone());
        DaemonResponse::ok(json!({
            "mount_id": mount_id,
            "path": format!("{identifier}.md"),
            "contents_base64": ""
        }))
    }
}

#[derive(Clone)]
struct BlockingPullRunner {
    started: mpsc::Sender<()>,
    release: Arc<(Mutex<bool>, Condvar)>,
}

impl RuntimeJobRunner for BlockingPullRunner {
    fn run_pull(&self, _state_root: PathBuf, _path: PathBuf) -> DaemonResponse {
        self.started.send(()).expect("notify started");
        let (lock, condvar) = &*self.release;
        let mut released = lock.lock().expect("lock release");
        while !*released {
            released = condvar.wait(released).expect("wait release");
        }
        DaemonResponse::ok(json!({ "command": "pull" }))
    }

    fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> DaemonResponse {
        DaemonResponse::error("unexpected_push", "push should not run")
    }

    fn run_scheduled_pull(
        &self,
        _state_root: PathBuf,
        _tick: PullSchedulerTick,
        _policy: HydrationPolicy,
    ) -> locality_core::LocalityResult<ScheduledPullRuntimeReport> {
        Err(LocalityError::InvalidState(
            "scheduled pull should not run".to_string(),
        ))
    }

    fn run_hydration(
        &self,
        _state_root: PathBuf,
        _request: HydrationRequest,
    ) -> locality_core::LocalityResult<HydrationOutcome> {
        Err(LocalityError::InvalidState(
            "hydration should not run".to_string(),
        ))
    }
}

#[derive(Clone)]
struct BlockingVirtualFsRunner {
    started: mpsc::Sender<()>,
    release: Arc<(Mutex<bool>, Condvar)>,
    children_tx: mpsc::Sender<(String, String)>,
    refresh_tx: mpsc::Sender<(String, String)>,
}

struct BlockingBackgroundRefreshRunner {
    background_tx: mpsc::Sender<(String, String)>,
    foreground_tx: mpsc::Sender<(String, String)>,
    release: Arc<(Mutex<bool>, Condvar)>,
}

impl RuntimeJobRunner for BlockingVirtualFsRunner {
    fn run_pull(&self, _state_root: PathBuf, _path: PathBuf) -> DaemonResponse {
        self.started.send(()).expect("notify started");
        let (lock, condvar) = &*self.release;
        let mut released = lock.lock().expect("lock release");
        while !*released {
            released = condvar.wait(released).expect("wait release");
        }
        DaemonResponse::ok(json!({ "command": "pull" }))
    }

    fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> DaemonResponse {
        DaemonResponse::error("unexpected_push", "push should not run")
    }

    fn run_scheduled_pull(
        &self,
        _state_root: PathBuf,
        _tick: PullSchedulerTick,
        _policy: HydrationPolicy,
    ) -> locality_core::LocalityResult<ScheduledPullRuntimeReport> {
        Err(LocalityError::InvalidState(
            "scheduled pull should not run".to_string(),
        ))
    }

    fn run_hydration(
        &self,
        _state_root: PathBuf,
        _request: HydrationRequest,
    ) -> locality_core::LocalityResult<HydrationOutcome> {
        Err(LocalityError::InvalidState(
            "hydration should not run".to_string(),
        ))
    }

    fn run_virtual_fs_children(
        &self,
        _state_root: PathBuf,
        mount_id: String,
        container_identifier: String,
    ) -> DaemonResponse {
        self.children_tx
            .send((mount_id.clone(), container_identifier.clone()))
            .expect("notify children");
        DaemonResponse::ok(json!({
            "mount_id": mount_id,
            "container_identifier": container_identifier,
            "children": []
        }))
    }

    fn run_virtual_fs_refresh_children(
        &self,
        _state_root: PathBuf,
        mount_id: String,
        container_identifier: String,
    ) -> locality_core::LocalityResult<VirtualFsRefreshChildrenReport> {
        self.refresh_tx
            .send((mount_id, container_identifier))
            .expect("notify refresh");
        Ok(VirtualFsRefreshChildrenReport::default())
    }
}

impl RuntimeJobRunner for BlockingBackgroundRefreshRunner {
    fn run_pull(&self, _state_root: PathBuf, _path: PathBuf) -> DaemonResponse {
        DaemonResponse::error("unexpected_pull", "pull should not run")
    }

    fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> DaemonResponse {
        DaemonResponse::error("unexpected_push", "push should not run")
    }

    fn run_scheduled_pull(
        &self,
        _state_root: PathBuf,
        _tick: PullSchedulerTick,
        _policy: HydrationPolicy,
    ) -> locality_core::LocalityResult<ScheduledPullRuntimeReport> {
        Err(LocalityError::InvalidState(
            "scheduled pull should not run".to_string(),
        ))
    }

    fn run_hydration(
        &self,
        _state_root: PathBuf,
        _request: HydrationRequest,
    ) -> locality_core::LocalityResult<HydrationOutcome> {
        Err(LocalityError::InvalidState(
            "hydration should not run".to_string(),
        ))
    }

    fn run_virtual_fs_refresh_children(
        &self,
        _state_root: PathBuf,
        mount_id: String,
        container_identifier: String,
    ) -> locality_core::LocalityResult<VirtualFsRefreshChildrenReport> {
        self.background_tx
            .send((mount_id, container_identifier))
            .expect("notify background refresh");
        let (lock, condvar) = &*self.release;
        let mut released = lock.lock().expect("lock release");
        while !*released {
            released = condvar.wait(released).expect("wait release");
        }
        Ok(VirtualFsRefreshChildrenReport::default())
    }

    fn run_virtual_fs_children(
        &self,
        _state_root: PathBuf,
        mount_id: String,
        container_identifier: String,
    ) -> DaemonResponse {
        self.foreground_tx
            .send((mount_id.clone(), container_identifier.clone()))
            .expect("notify foreground children");
        DaemonResponse::ok(json!({
            "mount_id": mount_id,
            "container_identifier": container_identifier,
            "children": []
        }))
    }
}

#[derive(Default)]
struct SerialState {
    started: Mutex<usize>,
    started_condvar: Condvar,
    released: Mutex<usize>,
    released_condvar: Condvar,
    active: AtomicUsize,
    max_active: AtomicUsize,
}

impl SerialState {
    fn mark_started(&self) -> usize {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.update_max_active(active);

        let mut started = self.started.lock().expect("started lock");
        *started += 1;
        let index = *started;
        self.started_condvar.notify_all();
        index
    }

    fn wait_started(&self, expected: usize) {
        let mut started = self.started.lock().expect("started lock");
        while *started < expected {
            started = self.started_condvar.wait(started).expect("wait started");
        }
    }

    fn started_count(&self) -> usize {
        *self.started.lock().expect("started lock")
    }

    fn release(&self, count: usize) {
        let mut released = self.released.lock().expect("released lock");
        *released = count;
        self.released_condvar.notify_all();
    }

    fn wait_released(&self, index: usize) {
        let mut released = self.released.lock().expect("released lock");
        while *released < index {
            released = self.released_condvar.wait(released).expect("wait released");
        }
    }

    fn mark_finished(&self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }

    fn update_max_active(&self, active: usize) {
        let mut current = self.max_active.load(Ordering::SeqCst);
        while active > current {
            match self.max_active.compare_exchange(
                current,
                active,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(next) => current = next,
            }
        }
    }
}

#[derive(Clone)]
struct SerialRunner {
    state: Arc<SerialState>,
}

impl RuntimeJobRunner for SerialRunner {
    fn run_pull(&self, _state_root: PathBuf, _path: PathBuf) -> DaemonResponse {
        let index = self.state.mark_started();
        self.state.wait_released(index);
        self.state.mark_finished();
        DaemonResponse::ok(json!({ "command": "pull", "index": index }))
    }

    fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> DaemonResponse {
        DaemonResponse::error("unexpected_push", "push should not run")
    }

    fn run_scheduled_pull(
        &self,
        _state_root: PathBuf,
        _tick: PullSchedulerTick,
        _policy: HydrationPolicy,
    ) -> locality_core::LocalityResult<ScheduledPullRuntimeReport> {
        Err(LocalityError::InvalidState(
            "scheduled pull should not run".to_string(),
        ))
    }

    fn run_hydration(
        &self,
        _state_root: PathBuf,
        _request: HydrationRequest,
    ) -> locality_core::LocalityResult<HydrationOutcome> {
        Err(LocalityError::InvalidState(
            "hydration should not run".to_string(),
        ))
    }
}

struct SchedulingRunner {
    scheduled: mpsc::Sender<()>,
    hydrated: mpsc::Sender<HydrationRequest>,
    scheduled_count: AtomicUsize,
}

struct ScheduledFreshnessRunner {
    scheduled: mpsc::Sender<()>,
    freshness: mpsc::Sender<SyncJob>,
    scheduled_count: AtomicUsize,
}

struct EventRunner {
    event_tx: mpsc::Sender<FileEvent>,
}

struct ReadHydrationRunner {
    hydrated: mpsc::Sender<HydrationRequest>,
}

struct FreshnessFromEventRunner {
    freshness_tx: mpsc::Sender<SyncJob>,
}

struct VirtualWriteAutoPushRunner {
    push_tx: mpsc::Sender<PushJob>,
}

struct BlockingVirtualWriteAutoPushRunner {
    started: mpsc::Sender<()>,
    order_tx: mpsc::Sender<String>,
    release: Arc<(Mutex<bool>, Condvar)>,
}

struct AutoFastForwardRunner {
    hydrated: mpsc::Sender<HydrationRequest>,
}

struct DiscoveryHintRunner {
    hydrated: mpsc::Sender<HydrationRequest>,
    refresh_tx: mpsc::Sender<(String, String)>,
    next_child_ids: Vec<String>,
}

#[cfg(target_os = "macos")]
struct MacosProjectionFastForwardRunner {
    hydrated: mpsc::Sender<HydrationRequest>,
}

struct PushRequestRunner {
    push_tx: mpsc::Sender<PushJob>,
}

struct RefreshRecordingRunner {
    refresh_tx: mpsc::Sender<(String, String)>,
}

struct DescendantSeedingRefreshRunner {
    refresh_tx: mpsc::Sender<(String, String)>,
    mount_id: MountId,
}

impl RuntimeJobRunner for PushRequestRunner {
    fn run_pull(&self, _state_root: PathBuf, _path: PathBuf) -> DaemonResponse {
        DaemonResponse::error("unexpected_pull", "pull should not run")
    }

    fn run_push(&self, _state_root: PathBuf, job: PushJob) -> DaemonResponse {
        self.push_tx.send(job).expect("send push job");
        DaemonResponse::ok(json!({ "command": "push" }))
    }

    fn run_scheduled_pull(
        &self,
        _state_root: PathBuf,
        _tick: PullSchedulerTick,
        _policy: HydrationPolicy,
    ) -> locality_core::LocalityResult<ScheduledPullRuntimeReport> {
        Err(LocalityError::InvalidState(
            "scheduled pull should not run".to_string(),
        ))
    }

    fn run_hydration(
        &self,
        _state_root: PathBuf,
        _request: HydrationRequest,
    ) -> locality_core::LocalityResult<HydrationOutcome> {
        Err(LocalityError::InvalidState(
            "hydration should not run".to_string(),
        ))
    }
}

impl RuntimeJobRunner for RefreshRecordingRunner {
    fn run_pull(&self, _state_root: PathBuf, _path: PathBuf) -> DaemonResponse {
        DaemonResponse::error("unexpected_pull", "pull should not run")
    }

    fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> DaemonResponse {
        DaemonResponse::error("unexpected_push", "push should not run")
    }

    fn run_scheduled_pull(
        &self,
        _state_root: PathBuf,
        _tick: PullSchedulerTick,
        _policy: HydrationPolicy,
    ) -> locality_core::LocalityResult<ScheduledPullRuntimeReport> {
        Err(LocalityError::InvalidState(
            "scheduled pull should not run".to_string(),
        ))
    }

    fn run_hydration(
        &self,
        _state_root: PathBuf,
        _request: HydrationRequest,
    ) -> locality_core::LocalityResult<HydrationOutcome> {
        Err(LocalityError::InvalidState(
            "hydration should not run".to_string(),
        ))
    }

    fn run_virtual_fs_refresh_children(
        &self,
        _state_root: PathBuf,
        mount_id: String,
        container_identifier: String,
    ) -> locality_core::LocalityResult<VirtualFsRefreshChildrenReport> {
        self.refresh_tx
            .send((mount_id, container_identifier))
            .expect("send refresh");
        Ok(VirtualFsRefreshChildrenReport::default())
    }
}

impl RuntimeJobRunner for DescendantSeedingRefreshRunner {
    fn run_pull(&self, _state_root: PathBuf, _path: PathBuf) -> DaemonResponse {
        DaemonResponse::error("unexpected_pull", "pull should not run")
    }

    fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> DaemonResponse {
        DaemonResponse::error("unexpected_push", "push should not run")
    }

    fn run_scheduled_pull(
        &self,
        _state_root: PathBuf,
        _tick: PullSchedulerTick,
        _policy: HydrationPolicy,
    ) -> locality_core::LocalityResult<ScheduledPullRuntimeReport> {
        Err(LocalityError::InvalidState(
            "scheduled pull should not run".to_string(),
        ))
    }

    fn run_hydration(
        &self,
        _state_root: PathBuf,
        _request: HydrationRequest,
    ) -> locality_core::LocalityResult<HydrationOutcome> {
        Err(LocalityError::InvalidState(
            "hydration should not run".to_string(),
        ))
    }

    fn run_virtual_fs_refresh_children(
        &self,
        state_root: PathBuf,
        mount_id: String,
        container_identifier: String,
    ) -> locality_core::LocalityResult<VirtualFsRefreshChildrenReport> {
        self.refresh_tx
            .send((mount_id.clone(), container_identifier.clone()))
            .expect("send refresh");

        let mut store = SqliteStateStore::open(state_root).map_err(LocalityError::from)?;
        let entries = match container_identifier.as_str() {
            "mount:notion-main" => vec![
                EntityRecord::new(
                    self.mount_id.clone(),
                    RemoteId::new("page-a"),
                    EntityKind::Page,
                    "A",
                    "A/page.md",
                ),
                EntityRecord::new(
                    self.mount_id.clone(),
                    RemoteId::new("page-b"),
                    EntityKind::Page,
                    "B",
                    "B/page.md",
                ),
            ],
            "children:page-a" => vec![EntityRecord::new(
                self.mount_id.clone(),
                RemoteId::new("page-a1"),
                EntityKind::Page,
                "A1",
                "A/A1/page.md",
            )],
            "children:page-b" => vec![EntityRecord::new(
                self.mount_id.clone(),
                RemoteId::new("page-b1"),
                EntityKind::Page,
                "B1",
                "B/B1/page.md",
            )],
            _ => Vec::new(),
        };
        let saved = entries.len();
        for entry in entries {
            store.save_entity(entry).map_err(LocalityError::from)?;
        }
        Ok(VirtualFsRefreshChildrenReport {
            saved,
            changed: saved > 0,
        })
    }
}

impl RuntimeJobRunner for EventRunner {
    fn run_pull(&self, _state_root: PathBuf, _path: PathBuf) -> DaemonResponse {
        DaemonResponse::error("unexpected_pull", "pull should not run")
    }

    fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> DaemonResponse {
        DaemonResponse::error("unexpected_push", "push should not run")
    }

    fn run_scheduled_pull(
        &self,
        _state_root: PathBuf,
        _tick: PullSchedulerTick,
        _policy: HydrationPolicy,
    ) -> locality_core::LocalityResult<ScheduledPullRuntimeReport> {
        Err(LocalityError::InvalidState(
            "scheduled pull should not run".to_string(),
        ))
    }

    fn run_hydration(
        &self,
        _state_root: PathBuf,
        _request: HydrationRequest,
    ) -> locality_core::LocalityResult<HydrationOutcome> {
        Err(LocalityError::InvalidState(
            "hydration should not run".to_string(),
        ))
    }

    fn run_file_event(
        &self,
        _state_root: PathBuf,
        event: FileEvent,
    ) -> locality_core::LocalityResult<FileEventRuntimeReport> {
        self.event_tx.send(event).expect("send file event");
        Ok(FileEventRuntimeReport {
            report: DaemonEventReport {
                ignored_events: 1,
                ..Default::default()
            },
            ..Default::default()
        })
    }
}

impl RuntimeJobRunner for ReadHydrationRunner {
    fn run_pull(&self, _state_root: PathBuf, _path: PathBuf) -> DaemonResponse {
        DaemonResponse::error("unexpected_pull", "pull should not run")
    }

    fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> DaemonResponse {
        DaemonResponse::error("unexpected_push", "push should not run")
    }

    fn run_scheduled_pull(
        &self,
        _state_root: PathBuf,
        _tick: PullSchedulerTick,
        _policy: HydrationPolicy,
    ) -> locality_core::LocalityResult<ScheduledPullRuntimeReport> {
        Err(LocalityError::InvalidState(
            "scheduled pull should not run".to_string(),
        ))
    }

    fn run_hydration(
        &self,
        _state_root: PathBuf,
        request: HydrationRequest,
    ) -> locality_core::LocalityResult<HydrationOutcome> {
        self.hydrated.send(request).expect("notify hydrated");
        Ok(HydrationOutcome::Hydrated)
    }

    fn run_file_event(
        &self,
        _state_root: PathBuf,
        _event: FileEvent,
    ) -> locality_core::LocalityResult<FileEventRuntimeReport> {
        Ok(FileEventRuntimeReport {
            report: DaemonEventReport {
                queued_hydrations: 1,
                ..Default::default()
            },
            queued_hydrations: vec![HydrationRequest::new(
                MountId::new("notion-main"),
                RemoteId::new("page-1"),
                PathBuf::from("Roadmap.md"),
                HydrationState::Hydrated,
                HydrationReason::StubRead,
            )],
            ..Default::default()
        })
    }
}

impl RuntimeJobRunner for FreshnessFromEventRunner {
    fn run_pull(&self, _state_root: PathBuf, _path: PathBuf) -> DaemonResponse {
        DaemonResponse::error("unexpected_pull", "pull should not run")
    }

    fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> DaemonResponse {
        DaemonResponse::error("unexpected_push", "push should not run")
    }

    fn run_scheduled_pull(
        &self,
        _state_root: PathBuf,
        _tick: PullSchedulerTick,
        _policy: HydrationPolicy,
    ) -> locality_core::LocalityResult<ScheduledPullRuntimeReport> {
        Err(LocalityError::InvalidState(
            "scheduled pull should not run".to_string(),
        ))
    }

    fn run_hydration(
        &self,
        _state_root: PathBuf,
        _request: HydrationRequest,
    ) -> locality_core::LocalityResult<HydrationOutcome> {
        Err(LocalityError::InvalidState(
            "hydration should not run".to_string(),
        ))
    }

    fn run_file_event(
        &self,
        _state_root: PathBuf,
        _event: FileEvent,
    ) -> locality_core::LocalityResult<FileEventRuntimeReport> {
        Ok(FileEventRuntimeReport {
            freshness_jobs: vec![SyncJob::new(
                MountId::new("notion-main"),
                Some(RemoteId::new("page-1")),
                SyncJobKind::ObserveEntity,
                ChangeHintKind::LocalEdited,
            )],
            ..Default::default()
        })
    }

    fn run_virtual_fs_commit_write(
        &self,
        _state_root: PathBuf,
        mount_id: String,
        identifier: String,
        _contents_base64: String,
    ) -> DaemonResponse {
        DaemonResponse::ok(json!({
            "mount_id": mount_id,
            "identifier": identifier,
            "remote_id": "page-1",
            "path": "Roadmap.md",
            "bytes_written": 6,
            "hydration": "dirty"
        }))
    }

    fn run_freshness_job(
        &self,
        _state_root: PathBuf,
        job: SyncJob,
    ) -> locality_core::LocalityResult<FreshnessRuntimeReport> {
        self.freshness_tx
            .send(job.clone())
            .expect("notify freshness");
        Ok(FreshnessRuntimeReport {
            job,
            remote_hint_pending: false,
            queued_hydrations: Vec::new(),
            follow_up_jobs: Vec::new(),
        })
    }
}

impl RuntimeJobRunner for VirtualWriteAutoPushRunner {
    fn run_pull(&self, _state_root: PathBuf, _path: PathBuf) -> DaemonResponse {
        DaemonResponse::error("unexpected_pull", "pull should not run")
    }

    fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> DaemonResponse {
        DaemonResponse::error("unexpected_push", "push should not run")
    }

    fn run_auto_push(&self, _state_root: PathBuf, job: PushJob) -> DaemonResponse {
        self.push_tx.send(job).expect("send auto-push job");
        DaemonResponse::ok(json!({ "command": "auto_push" }))
    }

    fn run_scheduled_pull(
        &self,
        _state_root: PathBuf,
        _tick: PullSchedulerTick,
        _policy: HydrationPolicy,
    ) -> locality_core::LocalityResult<ScheduledPullRuntimeReport> {
        Err(LocalityError::InvalidState(
            "scheduled pull should not run".to_string(),
        ))
    }

    fn run_hydration(
        &self,
        _state_root: PathBuf,
        _request: HydrationRequest,
    ) -> locality_core::LocalityResult<HydrationOutcome> {
        Err(LocalityError::InvalidState(
            "hydration should not run".to_string(),
        ))
    }

    fn run_virtual_fs_commit_write(
        &self,
        _state_root: PathBuf,
        mount_id: String,
        identifier: String,
        _contents_base64: String,
    ) -> DaemonResponse {
        DaemonResponse::ok(json!({
            "mount_id": mount_id,
            "identifier": identifier,
            "remote_id": "page-1",
            "path": "Roadmap.md",
            "bytes_written": 6,
            "hydration": "dirty"
        }))
    }

    fn run_freshness_job(
        &self,
        _state_root: PathBuf,
        job: SyncJob,
    ) -> locality_core::LocalityResult<FreshnessRuntimeReport> {
        Ok(FreshnessRuntimeReport {
            job,
            remote_hint_pending: false,
            queued_hydrations: Vec::new(),
            follow_up_jobs: Vec::new(),
        })
    }
}

impl RuntimeJobRunner for BlockingVirtualWriteAutoPushRunner {
    fn run_pull(&self, _state_root: PathBuf, path: PathBuf) -> DaemonResponse {
        self.order_tx
            .send(format!("pull:{}", path.display()))
            .expect("send pull order");
        DaemonResponse::ok(json!({ "command": "pull" }))
    }

    fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> DaemonResponse {
        DaemonResponse::error("unexpected_push", "push should not run")
    }

    fn run_auto_push(&self, _state_root: PathBuf, job: PushJob) -> DaemonResponse {
        let filename = job
            .target_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("<unknown>");
        self.order_tx
            .send(format!("auto_push:{filename}"))
            .expect("send auto-push order");
        DaemonResponse::ok(json!({ "command": "auto_push" }))
    }

    fn run_scheduled_pull(
        &self,
        _state_root: PathBuf,
        _tick: PullSchedulerTick,
        _policy: HydrationPolicy,
    ) -> locality_core::LocalityResult<ScheduledPullRuntimeReport> {
        Err(LocalityError::InvalidState(
            "scheduled pull should not run".to_string(),
        ))
    }

    fn run_hydration(
        &self,
        _state_root: PathBuf,
        _request: HydrationRequest,
    ) -> locality_core::LocalityResult<HydrationOutcome> {
        Err(LocalityError::InvalidState(
            "hydration should not run".to_string(),
        ))
    }

    fn run_virtual_fs_commit_write(
        &self,
        _state_root: PathBuf,
        mount_id: String,
        identifier: String,
        _contents_base64: String,
    ) -> DaemonResponse {
        self.started.send(()).expect("notify started");
        let (lock, condvar) = &*self.release;
        let mut released = lock.lock().expect("lock release");
        while !*released {
            released = condvar.wait(released).expect("wait release");
        }
        DaemonResponse::ok(json!({
            "mount_id": mount_id,
            "identifier": identifier,
            "remote_id": "page-1",
            "path": "Roadmap.md",
            "bytes_written": 6,
            "hydration": "dirty"
        }))
    }
}

impl RuntimeJobRunner for AutoFastForwardRunner {
    fn run_pull(&self, _state_root: PathBuf, _path: PathBuf) -> DaemonResponse {
        DaemonResponse::error("unexpected_pull", "pull should not run")
    }

    fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> DaemonResponse {
        DaemonResponse::error("unexpected_push", "push should not run")
    }

    fn run_scheduled_pull(
        &self,
        _state_root: PathBuf,
        _tick: PullSchedulerTick,
        _policy: HydrationPolicy,
    ) -> locality_core::LocalityResult<ScheduledPullRuntimeReport> {
        Err(LocalityError::InvalidState(
            "scheduled pull should not run".to_string(),
        ))
    }

    fn run_hydration(
        &self,
        _state_root: PathBuf,
        request: HydrationRequest,
    ) -> locality_core::LocalityResult<HydrationOutcome> {
        self.hydrated.send(request).expect("notify hydrated");
        Ok(HydrationOutcome::Hydrated)
    }

    fn run_file_event(
        &self,
        _state_root: PathBuf,
        _event: FileEvent,
    ) -> locality_core::LocalityResult<FileEventRuntimeReport> {
        Ok(FileEventRuntimeReport {
            freshness_jobs: vec![SyncJob::new(
                MountId::new("notion-main"),
                Some(RemoteId::new("page-1")),
                SyncJobKind::ObserveEntity,
                ChangeHintKind::RemoteMaybeChanged,
            )],
            ..Default::default()
        })
    }

    fn run_freshness_job(
        &self,
        _state_root: PathBuf,
        job: SyncJob,
    ) -> locality_core::LocalityResult<FreshnessRuntimeReport> {
        Ok(FreshnessRuntimeReport {
            job,
            remote_hint_pending: true,
            queued_hydrations: vec![HydrationRequest::new(
                MountId::new("notion-main"),
                RemoteId::new("page-1"),
                PathBuf::from("Roadmap.md"),
                HydrationState::Hydrated,
                HydrationReason::RemoteFastForward,
            )],
            follow_up_jobs: Vec::new(),
        })
    }
}

impl RuntimeJobRunner for DiscoveryHintRunner {
    fn run_pull(&self, _state_root: PathBuf, _path: PathBuf) -> DaemonResponse {
        DaemonResponse::error("unexpected_pull", "pull should not run")
    }

    fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> DaemonResponse {
        DaemonResponse::error("unexpected_push", "push should not run")
    }

    fn run_scheduled_pull(
        &self,
        _state_root: PathBuf,
        _tick: PullSchedulerTick,
        _policy: HydrationPolicy,
    ) -> locality_core::LocalityResult<ScheduledPullRuntimeReport> {
        Err(LocalityError::InvalidState(
            "scheduled pull should not run".to_string(),
        ))
    }

    fn run_hydration(
        &self,
        state_root: PathBuf,
        request: HydrationRequest,
    ) -> locality_core::LocalityResult<HydrationOutcome> {
        let mut store = SqliteStateStore::open(state_root).map_err(LocalityError::from)?;
        let shadow = child_link_shadow("page-1", self.next_child_ids.iter().map(String::as_str));
        store
            .save_shadow(&request.mount_id, shadow.clone())
            .map_err(LocalityError::from)?;
        let mut entity = store
            .get_entity(&request.mount_id, &request.remote_id)
            .map_err(LocalityError::from)?
            .expect("entity");
        entity.hydration = HydrationState::Hydrated;
        entity.content_hash = Some(shadow.body_hash);
        entity.remote_edited_at = Some("remote-v2".to_string());
        store.save_entity(entity).map_err(LocalityError::from)?;

        self.hydrated.send(request).expect("notify hydrated");
        Ok(HydrationOutcome::Hydrated)
    }

    fn run_virtual_fs_refresh_children(
        &self,
        _state_root: PathBuf,
        mount_id: String,
        container_identifier: String,
    ) -> locality_core::LocalityResult<VirtualFsRefreshChildrenReport> {
        self.refresh_tx
            .send((mount_id, container_identifier))
            .expect("send child refresh");
        Ok(VirtualFsRefreshChildrenReport::default())
    }
}

#[cfg(target_os = "macos")]
impl RuntimeJobRunner for MacosProjectionFastForwardRunner {
    fn run_pull(&self, _state_root: PathBuf, _path: PathBuf) -> DaemonResponse {
        DaemonResponse::error("unexpected_pull", "pull should not run")
    }

    fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> DaemonResponse {
        DaemonResponse::error("unexpected_push", "push should not run")
    }

    fn run_scheduled_pull(
        &self,
        _state_root: PathBuf,
        _tick: PullSchedulerTick,
        _policy: HydrationPolicy,
    ) -> locality_core::LocalityResult<ScheduledPullRuntimeReport> {
        Err(LocalityError::InvalidState(
            "scheduled pull should not run".to_string(),
        ))
    }

    fn run_hydration(
        &self,
        state_root: PathBuf,
        request: HydrationRequest,
    ) -> locality_core::LocalityResult<HydrationOutcome> {
        let body = markdown_body("Remote body.");
        let shadow = ShadowDocument::from_synced_body(
            request.remote_id.clone(),
            body.clone(),
            8,
            [RemoteId::new("heading-1"), RemoteId::new("paragraph-2")],
        )
        .expect("shadow")
        .with_frontmatter(frontmatter());
        let document = CanonicalDocument::new(frontmatter(), body);
        let content_path =
            virtual_fs_content_root(&state_root, &request.mount_id).join("Roadmap.md");
        std::fs::create_dir_all(content_path.parent().expect("content parent"))
            .map_err(LocalityError::from)?;
        std::fs::write(&content_path, render_canonical_markdown(&document))
            .map_err(LocalityError::from)?;

        let mut store = SqliteStateStore::open(state_root).map_err(LocalityError::from)?;
        store
            .save_shadow(&request.mount_id, shadow.clone())
            .map_err(LocalityError::from)?;
        let mut entity = store
            .get_entity(&request.mount_id, &request.remote_id)
            .map_err(LocalityError::from)?
            .expect("entity");
        entity.hydration = HydrationState::Hydrated;
        entity.content_hash = Some(shadow.body_hash);
        entity.remote_edited_at = Some("remote-v2".to_string());
        store.save_entity(entity).map_err(LocalityError::from)?;

        self.hydrated.send(request).expect("notify hydrated");
        Ok(HydrationOutcome::Hydrated)
    }

    fn run_file_event(
        &self,
        _state_root: PathBuf,
        _event: FileEvent,
    ) -> locality_core::LocalityResult<FileEventRuntimeReport> {
        Ok(FileEventRuntimeReport {
            freshness_jobs: vec![SyncJob::new(
                MountId::new("notion-main"),
                Some(RemoteId::new("page-1")),
                SyncJobKind::ObserveEntity,
                ChangeHintKind::RemoteMaybeChanged,
            )],
            ..Default::default()
        })
    }

    fn run_freshness_job(
        &self,
        _state_root: PathBuf,
        job: SyncJob,
    ) -> locality_core::LocalityResult<FreshnessRuntimeReport> {
        Ok(FreshnessRuntimeReport {
            job,
            remote_hint_pending: true,
            queued_hydrations: vec![HydrationRequest::new(
                MountId::new("notion-main"),
                RemoteId::new("page-1"),
                PathBuf::from("Roadmap.md"),
                HydrationState::Hydrated,
                HydrationReason::RemoteFastForward,
            )],
            follow_up_jobs: Vec::new(),
        })
    }
}

impl RuntimeJobRunner for SchedulingRunner {
    fn run_pull(&self, _state_root: PathBuf, _path: PathBuf) -> DaemonResponse {
        DaemonResponse::error("unexpected_pull", "pull should not run")
    }

    fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> DaemonResponse {
        DaemonResponse::error("unexpected_push", "push should not run")
    }

    fn run_scheduled_pull(
        &self,
        _state_root: PathBuf,
        _tick: PullSchedulerTick,
        _policy: HydrationPolicy,
    ) -> locality_core::LocalityResult<ScheduledPullRuntimeReport> {
        self.scheduled.send(()).expect("notify scheduled");
        let queued_hydrations = if self.scheduled_count.fetch_add(1, Ordering::SeqCst) == 0 {
            vec![HydrationRequest::new(
                MountId::new("notion-main"),
                RemoteId::new("page-1"),
                PathBuf::from("Roadmap.md"),
                HydrationState::Hydrated,
                HydrationReason::Policy,
            )]
        } else {
            Vec::new()
        };

        Ok(ScheduledPullRuntimeReport {
            report: Default::default(),
            queued_hydrations,
            freshness_jobs: Vec::new(),
        })
    }

    fn run_hydration(
        &self,
        _state_root: PathBuf,
        request: HydrationRequest,
    ) -> locality_core::LocalityResult<HydrationOutcome> {
        self.hydrated.send(request).expect("notify hydrated");
        Ok(HydrationOutcome::Hydrated)
    }
}

impl RuntimeJobRunner for ScheduledFreshnessRunner {
    fn run_pull(&self, _state_root: PathBuf, _path: PathBuf) -> DaemonResponse {
        DaemonResponse::error("unexpected_pull", "pull should not run")
    }

    fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> DaemonResponse {
        DaemonResponse::error("unexpected_push", "push should not run")
    }

    fn run_scheduled_pull(
        &self,
        _state_root: PathBuf,
        _tick: PullSchedulerTick,
        _policy: HydrationPolicy,
    ) -> locality_core::LocalityResult<ScheduledPullRuntimeReport> {
        self.scheduled.send(()).expect("notify scheduled");
        let freshness_jobs = if self.scheduled_count.fetch_add(1, Ordering::SeqCst) == 0 {
            vec![SyncJob::new(
                MountId::new("notion-main"),
                Some(RemoteId::new("page-1")),
                SyncJobKind::ObserveEntity,
                ChangeHintKind::BackgroundPoll,
            )]
        } else {
            Vec::new()
        };

        Ok(ScheduledPullRuntimeReport {
            report: Default::default(),
            queued_hydrations: Vec::new(),
            freshness_jobs,
        })
    }

    fn run_hydration(
        &self,
        _state_root: PathBuf,
        _request: HydrationRequest,
    ) -> locality_core::LocalityResult<HydrationOutcome> {
        Err(LocalityError::InvalidState(
            "hydration should not run".to_string(),
        ))
    }

    fn run_freshness_job(
        &self,
        _state_root: PathBuf,
        job: SyncJob,
    ) -> locality_core::LocalityResult<FreshnessRuntimeReport> {
        self.freshness.send(job.clone()).expect("notify freshness");
        Ok(FreshnessRuntimeReport {
            job,
            remote_hint_pending: false,
            queued_hydrations: Vec::new(),
            follow_up_jobs: Vec::new(),
        })
    }
}

fn release_blocked_runner(release: &Arc<(Mutex<bool>, Condvar)>) {
    let (lock, condvar) = &**release;
    let mut released = lock.lock().expect("lock release");
    *released = true;
    condvar.notify_all();
}

fn wait_until_pending_requests(handle: &DaemonRuntimeHandle, minimum: usize) {
    for _ in 0..20 {
        if handle.status().expect("runtime status").pending_requests >= minimum {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("runtime did not queue {minimum} pending request(s)");
}

fn min_position<const N: usize>(
    values: &[(String, String)],
    needles: [&(String, String); N],
) -> usize {
    needles
        .into_iter()
        .map(|needle| {
            values
                .iter()
                .position(|value| value == needle)
                .expect("needle present")
        })
        .min()
        .expect("at least one needle")
}

fn max_position<const N: usize>(
    values: &[(String, String)],
    needles: [&(String, String); N],
) -> usize {
    needles
        .into_iter()
        .map(|needle| {
            values
                .iter()
                .position(|value| value == needle)
                .expect("needle present")
        })
        .max()
        .expect("at least one needle")
}

fn collect_refreshes_until(
    refresh_rx: &mpsc::Receiver<(String, String)>,
    expected: &BTreeSet<(String, String)>,
    timeout: Duration,
) -> Vec<(String, String)> {
    let deadline = Instant::now() + timeout;
    let mut refreshed = Vec::new();

    while !expected.iter().all(|refresh| refreshed.contains(refresh)) {
        let now = Instant::now();
        if now >= deadline {
            let missing = expected
                .iter()
                .filter(|refresh| !refreshed.contains(refresh))
                .collect::<Vec<_>>();
            panic!(
                "timed out waiting for background refreshes {missing:?} after {timeout:?}; saw {refreshed:?}"
            );
        }

        let wait = deadline
            .saturating_duration_since(now)
            .min(Duration::from_millis(250));
        match refresh_rx.recv_timeout(wait) {
            Ok(refresh) => refreshed.push(refresh),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("background refresh channel closed; saw {refreshed:?}");
            }
        }
    }

    refreshed
}

fn relay_config(name: &str) -> DaemonConfig {
    let mut config = test_config(name);
    config.pull_scheduler.mode = PullMode::Relay;
    config
}

fn polling_config(name: &str) -> DaemonConfig {
    let mut config = test_config(name);
    config.pull_scheduler.mode = PullMode::Polling;
    config.pull_scheduler.active_interval = Duration::from_millis(5);
    config.pull_scheduler.cold_interval = Duration::from_millis(5);
    config.runtime_tick_interval = Duration::from_millis(5);
    config
}

fn test_config(name: &str) -> DaemonConfig {
    DaemonConfig {
        state_root: temp_root(name),
        tcp_addr: None,
        mcp_addr: None,
        runtime_tick_interval: Duration::from_millis(10),
        hydration_retry_delay: Duration::from_millis(25),
        ..Default::default()
    }
}

fn temp_root(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "loc-runtime-{name}-{}-{unique}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp root");
    root
}

fn workspace_virtual_mount(mount_id: &MountId, name: &str) -> MountConfig {
    MountConfig::new(mount_id.clone(), "notion", temp_root(name))
        .projection(ProjectionMode::MacosFileProvider)
}

fn save_workspace_page(
    store: &mut InMemoryStateStore,
    mount_id: &MountId,
    remote_id: &str,
    title: impl Into<String>,
    path: impl Into<PathBuf>,
    hydration: HydrationState,
) {
    store
        .save_entity(
            EntityRecord::new(
                mount_id.clone(),
                RemoteId::new(remote_id),
                EntityKind::Page,
                title,
                path,
            )
            .with_hydration(hydration),
        )
        .expect("save workspace page");
}

fn seed_clean_remote_changed_page(state_root: &Path, mount_root: &Path) {
    let mount_id = MountId::new("notion-main");
    let remote_id = RemoteId::new("page-1");
    let body = markdown_body("Original body.");
    let shadow = ShadowDocument::from_synced_body(
        remote_id.clone(),
        body.clone(),
        7,
        [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
    )
    .expect("shadow")
    .with_frontmatter(frontmatter());
    let mut store = SqliteStateStore::open(state_root.to_path_buf()).expect("open store");
    store
        .save_mount(MountConfig::new(
            mount_id.clone(),
            "notion",
            mount_root.to_path_buf(),
        ))
        .expect("save mount");
    store
        .save_shadow(&mount_id, shadow.clone())
        .expect("save shadow");
    store
        .save_entity(
            EntityRecord::new(
                mount_id.clone(),
                remote_id.clone(),
                EntityKind::Page,
                "Roadmap",
                "Roadmap.md",
            )
            .with_hydration(HydrationState::Hydrated)
            .with_content_hash(shadow.body_hash)
            .with_remote_edited_at("remote-v1"),
        )
        .expect("save entity");
    store
        .save_freshness_state(
            FreshnessStateRecord::new(mount_id.clone(), remote_id.clone(), FreshnessTier::Hot)
                .remote_hint_pending(true),
        )
        .expect("save freshness");
    let document = CanonicalDocument::new(frontmatter(), body);
    std::fs::write(
        mount_root.join("Roadmap.md"),
        render_canonical_markdown(&document),
    )
    .expect("write clean page");
}

fn seed_clean_page_with_child_links<const N: usize>(
    state_root: &Path,
    mount_root: &Path,
    projection: ProjectionMode,
    child_ids: [&str; N],
) {
    let mount_id = MountId::new("notion-main");
    let remote_id = RemoteId::new("page-1");
    let shadow = child_link_shadow("page-1", child_ids);
    let mut store = SqliteStateStore::open(state_root.to_path_buf()).expect("open store");
    store
        .save_mount(
            MountConfig::new(mount_id.clone(), "notion", mount_root.to_path_buf())
                .projection(projection),
        )
        .expect("save mount");
    store
        .save_shadow(&mount_id, shadow.clone())
        .expect("save shadow");
    store
        .save_entity(
            EntityRecord::new(
                mount_id.clone(),
                remote_id.clone(),
                EntityKind::Page,
                "Roadmap",
                "Roadmap/page.md",
            )
            .with_hydration(HydrationState::Hydrated)
            .with_content_hash(shadow.body_hash)
            .with_remote_edited_at("remote-v1"),
        )
        .expect("save entity");
    store
        .save_freshness_state(
            FreshnessStateRecord::new(mount_id.clone(), remote_id.clone(), FreshnessTier::Hot)
                .remote_hint_pending(true),
        )
        .expect("save freshness");
    let content_path = virtual_fs_content_root(state_root, &mount_id).join("Roadmap/page.md");
    std::fs::create_dir_all(content_path.parent().expect("content parent"))
        .expect("create content parent");
    std::fs::write(
        content_path,
        render_canonical_markdown(&CanonicalDocument::new(
            shadow.frontmatter.clone(),
            shadow.rendered_body.clone(),
        )),
    )
    .expect("write clean virtual page");
}

fn child_link_shadow<'a>(
    entity_id: &str,
    child_ids: impl IntoIterator<Item = &'a str>,
) -> ShadowDocument {
    let mut body = "# Roadmap\n".to_string();
    let mut shadow = ShadowDocument::from_synced_body(
        RemoteId::new(entity_id),
        "# Roadmap\n",
        1,
        [RemoteId::new("heading-1")],
    )
    .expect("base shadow")
    .with_frontmatter(frontmatter());
    shadow.blocks[0].native_kind = Some("heading_1".to_string());

    for (index, child_id) in child_ids.into_iter().enumerate() {
        let text = format!("[Child {index}](https://www.notion.so/{child_id})");
        body.push_str("\n");
        body.push_str(&text);
        body.push('\n');
        shadow.blocks.push(locality_core::shadow::ShadowBlock {
            remote_id: RemoteId::new(child_id),
            kind: locality_core::shadow::MarkdownBlockKind::Paragraph,
            source_span: locality_core::model::SourceSpan {
                start_line: 3 + index * 2,
                end_line: 3 + index * 2,
            },
            content_hash: format!("child-{child_id}"),
            text,
            native_kind: Some("child_page".to_string()),
        });
    }
    shadow.rendered_body = body;
    shadow.body_hash = format!("child-link-count-{}", shadow.blocks.len());
    shadow
}

#[cfg(target_os = "macos")]
fn seed_clean_remote_changed_macos_file_provider_page(
    state_root: &Path,
    mount_root: &Path,
) -> PathBuf {
    let mount_id = MountId::new("notion-main");
    let remote_id = RemoteId::new("page-1");
    let body = markdown_body("Original body.");
    let shadow = ShadowDocument::from_synced_body(
        remote_id.clone(),
        body.clone(),
        7,
        [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
    )
    .expect("shadow")
    .with_frontmatter(frontmatter());
    let mut store = SqliteStateStore::open(state_root.to_path_buf()).expect("open store");
    store
        .save_mount(
            MountConfig::new(mount_id.clone(), "notion", mount_root.to_path_buf())
                .projection(ProjectionMode::MacosFileProvider),
        )
        .expect("save mount");
    store
        .save_shadow(&mount_id, shadow.clone())
        .expect("save shadow");
    store
        .save_entity(
            EntityRecord::new(
                mount_id.clone(),
                remote_id.clone(),
                EntityKind::Page,
                "Roadmap",
                "Roadmap.md",
            )
            .with_hydration(HydrationState::Hydrated)
            .with_content_hash(shadow.body_hash)
            .with_remote_edited_at("remote-v1"),
        )
        .expect("save entity");
    store
        .save_freshness_state(
            FreshnessStateRecord::new(mount_id.clone(), remote_id.clone(), FreshnessTier::Hot)
                .remote_hint_pending(true),
        )
        .expect("save freshness");

    let rendered = render_canonical_markdown(&CanonicalDocument::new(frontmatter(), body));
    std::fs::create_dir_all(mount_root).expect("create mount root");
    let visible_path = mount_root.join("Roadmap.md");
    std::fs::write(&visible_path, &rendered).expect("write visible page");
    let content_path = virtual_fs_content_root(state_root, &mount_id).join("Roadmap.md");
    std::fs::create_dir_all(content_path.parent().expect("content parent"))
        .expect("create content root");
    std::fs::write(content_path, rendered).expect("write content cache");
    visible_path
}

fn mark_page_recently_opened(state_root: &Path) {
    let mut store = SqliteStateStore::open(state_root.to_path_buf()).expect("open store");
    let mount_id = MountId::new("notion-main");
    let remote_id = RemoteId::new("page-1");
    let mut freshness = store
        .get_freshness_state(&mount_id, &remote_id)
        .expect("get freshness")
        .expect("freshness");
    freshness.last_opened_at = Some(freshness_timestamp());
    store
        .save_freshness_state(freshness)
        .expect("save freshness");
}

struct EventFixture {
    state_root: PathBuf,
    mount_root: PathBuf,
    mount_id: MountId,
    remote_id: RemoteId,
}

impl EventFixture {
    fn new(name: &str) -> Self {
        Self::new_with_state(name, HydrationState::Hydrated)
    }

    fn new_with_state(name: &str, hydration: HydrationState) -> Self {
        let state_root = temp_root(&format!("{name}-state"));
        let mount_root = temp_root(&format!("{name}-mount"));
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let body = markdown_body("Original body.");
        let shadow = ShadowDocument::from_synced_body(
            remote_id.clone(),
            body,
            7,
            [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
        )
        .expect("shadow")
        .with_frontmatter(frontmatter());

        let mut store = SqliteStateStore::open(state_root.clone()).expect("open store");
        store
            .save_mount(MountConfig::new(
                mount_id.clone(),
                "notion",
                mount_root.clone(),
            ))
            .expect("save mount");
        store
            .save_shadow(&mount_id, shadow.clone())
            .expect("save shadow");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    remote_id.clone(),
                    EntityKind::Page,
                    "Roadmap",
                    "Roadmap.md",
                )
                .with_hydration(hydration)
                .with_content_hash(shadow.body_hash),
            )
            .expect("save entity");

        Self {
            state_root,
            mount_root,
            mount_id,
            remote_id,
        }
    }

    fn page_path(&self) -> PathBuf {
        self.mount_root.join("Roadmap.md")
    }

    fn write_event(&self) -> FileEvent {
        FileEvent {
            path: self.page_path(),
            kind: FileEventKind::Write,
        }
    }

    fn read_event(&self) -> FileEvent {
        FileEvent {
            path: self.page_path(),
            kind: FileEventKind::Read,
        }
    }

    fn write_hydrated_page(&self, body: &str) {
        let document = CanonicalDocument::new(frontmatter(), markdown_body(body));
        std::fs::write(self.page_path(), render_canonical_markdown(&document)).expect("write page");
    }

    fn write_hydrated_page_with_frontmatter(&self, frontmatter: &str, body: &str) {
        let document = CanonicalDocument::new(frontmatter, markdown_body(body));
        std::fs::write(self.page_path(), render_canonical_markdown(&document)).expect("write page");
    }
}

fn frontmatter() -> String {
    "loc:\n  id: page-1\n  type: page\ntitle: Roadmap\n".to_string()
}

fn markdown_body(body: &str) -> String {
    format!("# Roadmap\n\n{body}\n")
}
