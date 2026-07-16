use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

use locality_connector::{BatchObservationChange, BatchObserveResult, ConnectorCheckpoint};
use locality_core::LocalityError;
use locality_core::freshness::{FreshnessTier, RemoteVersion};
use locality_core::hydration::{HydrationReason, HydrationRequest};
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId, TreeEntry};
use locality_store::{
    AutoSaveEnrollmentRecord, AutoSaveOrigin, AutoSaveRepository, AutoSaveState,
    ConnectorStateRecord, ConnectorStateRepository, DiscoveryRepository, EntityRecord,
    EntityRepository, FreshnessStateRecord, FreshnessStateRepository, InMemoryStateStore,
    MetadataDiscoveryJobRecord, MetadataDiscoveryJobRepository, MetadataDiscoveryPriority,
    MountConfig, MountRepository, RemoteObservationRepository, SqliteStateStore,
};
use localityd::discovery::{
    DiscoveryChangeKind, DiscoveryHoldReason, DiscoveryPostCommitAction, DiscoveryProjectionAction,
    HeldDiscoveryItem, ProjectionAssessment, plan_batch_discovery,
};

const NOW: &str = "unix_ms:100000";

#[test]
fn safe_create_is_planned_without_side_effects() {
    let mount = mount();
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");
    let entry = page_entry("issue-1", "teams/ENG/ENG-1/page.md", "remote-v1");
    let assessments = BTreeMap::from([(entry.remote_id.clone(), ProjectionAssessment::Safe)]);

    let plan = plan_batch_discovery(
        &store,
        &mount,
        BatchObserveResult::incremental(
            vec![BatchObservationChange::Upsert(entry.clone())],
            checkpoint(1, r#"{"cursor":"one"}"#),
        ),
        NOW,
        Some("batch:linear-main"),
        &assessments,
    )
    .expect("plan discovery");

    assert_eq!(
        plan.projection_actions,
        vec![DiscoveryProjectionAction::Create {
            entry: entry.clone(),
        }]
    );
    assert!(plan.held.is_empty());
    assert!(plan.post_commit.is_empty());
    assert!(
        store
            .list_entities(&mount.mount_id)
            .expect("entities remain readable")
            .is_empty(),
        "planning must not mutate the store"
    );

    let commit = plan.into_commit();
    assert_eq!(commit.entity_upserts, vec![entry.into()]);
    assert!(commit.entity_deletes.is_empty());
    assert_eq!(commit.freshness_upserts.len(), 1);
    assert!(!commit.freshness_upserts[0].remote_hint_pending);
    assert_eq!(
        commit.metadata_discovery_deletes,
        vec!["batch:linear-main".to_string()]
    );
    assert_eq!(commit.checkpoint.state_json, r#"{"cursor":"one"}"#);
}

#[test]
fn invalid_batches_are_rejected_before_any_state_change() {
    let mount = mount();
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");
    let original = entity(
        "existing",
        "existing/page.md",
        HydrationState::Stub,
        "remote-v1",
    );
    store.save_entity(original.clone()).expect("save entity");

    let mut wrong_mount = page_entry("issue-1", "one/page.md", "remote-v1");
    wrong_mount.mount_id = MountId::new("other");
    let duplicate_id = page_entry("issue-1", "two/page.md", "remote-v1");
    let duplicate_path = page_entry("issue-2", "one/page.md", "remote-v1");
    let unsafe_path = page_entry("issue-3", "../outside/page.md", "remote-v1");
    let mut empty_path = page_entry("issue-4", "empty/page.md", "remote-v1");
    empty_path.path = PathBuf::new();
    let absolute_path = page_entry("issue-5", "/outside/page.md", "remote-v1");
    let dotted_path = page_entry("issue-6", "one/./page.md", "remote-v1");
    let backslash_path = page_entry("issue-7", r"one\page.md", "remote-v1");
    let mut cases = vec![
        BatchObserveResult::incremental(
            vec![BatchObservationChange::Upsert(wrong_mount)],
            checkpoint(1, "{}"),
        ),
        BatchObserveResult::incremental(
            vec![
                BatchObservationChange::Upsert(page_entry("issue-1", "one/page.md", "remote-v1")),
                BatchObservationChange::Upsert(duplicate_id),
            ],
            checkpoint(1, "{}"),
        ),
        BatchObserveResult::incremental(
            vec![
                BatchObservationChange::Upsert(page_entry("issue-1", "one/page.md", "remote-v1")),
                BatchObservationChange::Upsert(duplicate_path),
            ],
            checkpoint(1, "{}"),
        ),
        BatchObserveResult::incremental(
            vec![
                BatchObservationChange::Upsert(page_entry("issue-1", "one/page.md", "remote-v1")),
                BatchObservationChange::Tombstone {
                    remote_id: RemoteId::new("issue-1"),
                },
            ],
            checkpoint(1, "{}"),
        ),
        BatchObserveResult::incremental(
            vec![BatchObservationChange::Upsert(unsafe_path)],
            checkpoint(1, "{}"),
        ),
        BatchObserveResult::incremental(
            vec![BatchObservationChange::Upsert(empty_path)],
            checkpoint(1, "{}"),
        ),
        BatchObserveResult::incremental(
            vec![BatchObservationChange::Upsert(absolute_path)],
            checkpoint(1, "{}"),
        ),
        BatchObserveResult::incremental(
            vec![BatchObservationChange::Upsert(dotted_path)],
            checkpoint(1, "{}"),
        ),
        BatchObserveResult::incremental(
            vec![BatchObservationChange::Upsert(backslash_path)],
            checkpoint(1, "{}"),
        ),
        BatchObserveResult::incremental(vec![], checkpoint(0, "{}")),
        BatchObserveResult::incremental(vec![], checkpoint(1, "not-json")),
    ];
    #[cfg(unix)]
    {
        let mut non_utf8 = page_entry("issue-8", "placeholder/page.md", "remote-v1");
        non_utf8.path = PathBuf::from(std::ffi::OsString::from_vec(vec![b'a', 0xff, b'b']));
        cases.push(BatchObserveResult::incremental(
            vec![BatchObservationChange::Upsert(non_utf8)],
            checkpoint(1, "{}"),
        ));
    }

    for batch in cases {
        assert!(plan_batch_discovery(&store, &mount, batch, NOW, None, &BTreeMap::new()).is_err());
        assert_eq!(
            store
                .list_entities(&mount.mount_id)
                .expect("unchanged entities"),
            vec![original.clone()]
        );
    }
}

#[test]
fn empty_metadata_job_id_is_rejected_during_planning() {
    let mount = mount();
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");

    let error = plan_batch_discovery(
        &store,
        &mount,
        BatchObserveResult::incremental(vec![], checkpoint(1, "{}")),
        NOW,
        Some(""),
        &BTreeMap::new(),
    )
    .expect_err("empty metadata job id must fail before projection");

    assert_eq!(
        error,
        LocalityError::InvalidState(
            "discovery metadata job identifier cannot be empty".to_string()
        )
    );
}

#[test]
fn new_entity_with_materialized_hydration_state_is_rejected() {
    let mount = mount();
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");
    let mut entry = page_entry("issue-1", "one/page.md", "remote-v1");
    entry.hydration = HydrationState::Hydrated;

    let error = plan_batch_discovery(
        &store,
        &mount,
        BatchObserveResult::incremental(
            vec![BatchObservationChange::Upsert(entry)],
            checkpoint(1, "{}"),
        ),
        NOW,
        None,
        &BTreeMap::from([(RemoteId::new("issue-1"), ProjectionAssessment::Safe)]),
    )
    .expect_err("metadata discovery cannot create a hydrated entity");

    assert_eq!(
        error,
        LocalityError::InvalidState(
            "discovery create `issue-1` has unsupported hydration state `Hydrated`".to_string()
        )
    );
}

#[test]
fn incremental_omission_preserves_and_complete_omission_deletes() {
    let mount = mount();
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");
    let original = entity("issue-1", "one/page.md", HydrationState::Stub, "remote-v1");
    store.save_entity(original.clone()).expect("save entity");
    let assessments = BTreeMap::from([(original.remote_id.clone(), ProjectionAssessment::Safe)]);

    let incremental = plan_batch_discovery(
        &store,
        &mount,
        BatchObserveResult::incremental(vec![], checkpoint(1, r#"{"cursor":"i"}"#)),
        NOW,
        None,
        &assessments,
    )
    .expect("incremental plan");
    assert!(incremental.projection_actions.is_empty());
    assert!(incremental.commit().entity_deletes.is_empty());

    let complete = plan_batch_discovery(
        &store,
        &mount,
        BatchObserveResult::complete(vec![], checkpoint(1, r#"{"cursor":"c"}"#)),
        NOW,
        None,
        &assessments,
    )
    .expect("complete plan");
    assert_eq!(
        complete.projection_actions,
        vec![DiscoveryProjectionAction::Delete {
            remote_id: original.remote_id.clone(),
            kind: EntityKind::Page,
            path: original.path.clone(),
        }]
    );
    assert_eq!(complete.commit().entity_deletes, vec![original.remote_id]);
}

#[test]
fn unknown_explicit_tombstone_is_a_noop() {
    let mount = mount();
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");

    let plan = plan_batch_discovery(
        &store,
        &mount,
        BatchObserveResult::incremental(
            vec![BatchObservationChange::Tombstone {
                remote_id: RemoteId::new("unknown"),
            }],
            checkpoint(1, "{}"),
        ),
        NOW,
        None,
        &BTreeMap::new(),
    )
    .expect("unknown tombstone plan");

    assert!(plan.projection_actions.is_empty());
    assert!(plan.held.is_empty());
    assert!(plan.commit().entity_deletes.is_empty());
    assert!(plan.commit().observation_upserts.is_empty());
}

#[test]
fn missing_or_blocked_projection_assessment_holds_structural_changes() {
    let mount = mount();
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");
    let existing = entity(
        "issue-move",
        "old/page.md",
        HydrationState::Stub,
        "remote-v1",
    );
    store.save_entity(existing.clone()).expect("save entity");
    let create = page_entry("issue-create", "created/page.md", "remote-v1");
    let moved = page_entry("issue-move", "new/page.md", "remote-v2");
    let assessments = BTreeMap::from([(
        moved.remote_id.clone(),
        ProjectionAssessment::Blocked(DiscoveryHoldReason::UntrackedDestination(
            moved.path.clone(),
        )),
    )]);

    let plan = plan_batch_discovery(
        &store,
        &mount,
        BatchObserveResult::incremental(
            vec![
                BatchObservationChange::Upsert(create.clone()),
                BatchObservationChange::Upsert(moved.clone()),
            ],
            checkpoint(1, "{}"),
        ),
        NOW,
        None,
        &assessments,
    )
    .expect("held plan");

    assert!(plan.projection_actions.is_empty());
    assert_eq!(
        plan.held,
        vec![
            HeldDiscoveryItem {
                remote_id: create.remote_id.clone(),
                change: DiscoveryChangeKind::Create,
                reason: DiscoveryHoldReason::UnknownProjection,
            },
            HeldDiscoveryItem {
                remote_id: moved.remote_id.clone(),
                change: DiscoveryChangeKind::Move,
                reason: DiscoveryHoldReason::UntrackedDestination(moved.path.clone()),
            },
        ]
    );
    assert!(plan.commit().entity_upserts.is_empty());
    assert_eq!(plan.commit().observation_upserts.len(), 2);
    assert!(
        plan.commit()
            .freshness_upserts
            .iter()
            .all(|freshness| freshness.remote_hint_pending)
    );
}

#[test]
fn dirty_and_conflicted_entities_hold_move_and_delete_even_when_projection_is_safe() {
    let mount = mount();
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");
    let dirty = entity("dirty", "dirty/page.md", HydrationState::Dirty, "remote-v1");
    let conflicted = entity(
        "conflicted",
        "conflicted/page.md",
        HydrationState::Conflicted,
        "remote-v1",
    );
    store.save_entity(dirty.clone()).expect("save dirty");
    store
        .save_entity(conflicted.clone())
        .expect("save conflicted");
    let assessments = BTreeMap::from([
        (dirty.remote_id.clone(), ProjectionAssessment::Safe),
        (conflicted.remote_id.clone(), ProjectionAssessment::Safe),
    ]);

    let plan = plan_batch_discovery(
        &store,
        &mount,
        BatchObserveResult::incremental(
            vec![
                BatchObservationChange::Upsert(page_entry(
                    "dirty",
                    "dirty-moved/page.md",
                    "remote-v2",
                )),
                BatchObservationChange::Tombstone {
                    remote_id: conflicted.remote_id.clone(),
                },
            ],
            checkpoint(2, "{}"),
        ),
        NOW,
        Some("batch:held"),
        &assessments,
    )
    .expect("held plan");

    assert_eq!(
        plan.held,
        vec![
            HeldDiscoveryItem {
                remote_id: conflicted.remote_id.clone(),
                change: DiscoveryChangeKind::Delete,
                reason: DiscoveryHoldReason::Conflicted,
            },
            HeldDiscoveryItem {
                remote_id: dirty.remote_id.clone(),
                change: DiscoveryChangeKind::Move,
                reason: DiscoveryHoldReason::Dirty,
            },
        ]
    );
    assert!(plan.commit().entity_upserts.is_empty());
    assert!(plan.commit().entity_deletes.is_empty());
    assert_eq!(
        plan.commit().metadata_discovery_deletes,
        vec!["batch:held".to_string()]
    );
}

#[test]
fn unchanged_dirty_and_conflicted_upserts_do_not_hold_or_pause_auto_save() {
    let mount = mount();
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");
    let dirty = entity("dirty", "dirty/page.md", HydrationState::Dirty, "remote-v1");
    let conflicted = entity(
        "conflicted",
        "conflicted/page.md",
        HydrationState::Conflicted,
        "remote-v1",
    );
    for current in [&dirty, &conflicted] {
        store.save_entity(current.clone()).expect("save entity");
        let mut enrollment = AutoSaveEnrollmentRecord::new(
            mount.mount_id.clone(),
            current.path.clone(),
            AutoSaveOrigin::UserEnabled,
            "created",
        );
        enrollment.remote_id = Some(current.remote_id.clone());
        store
            .save_auto_save_enrollment(enrollment)
            .expect("save auto-save");
    }
    let unchanged = [&dirty, &conflicted]
        .into_iter()
        .map(|current| {
            BatchObservationChange::Upsert(TreeEntry {
                mount_id: current.mount_id.clone(),
                remote_id: current.remote_id.clone(),
                kind: current.kind.clone(),
                title: current.title.clone(),
                path: current.path.clone(),
                hydration: HydrationState::Stub,
                content_hash: current.content_hash.clone(),
                remote_edited_at: current.remote_edited_at.clone(),
                stub_frontmatter: None,
            })
        })
        .collect();

    let plan = plan_batch_discovery(
        &store,
        &mount,
        BatchObserveResult::complete(unchanged, checkpoint(1, "{}")),
        NOW,
        None,
        &BTreeMap::new(),
    )
    .expect("unchanged metadata plan");

    assert!(plan.held.is_empty());
    assert!(plan.projection_actions.is_empty());
    assert!(plan.commit().entity_upserts.is_empty());
    assert!(plan.commit().auto_save_upserts.is_empty());
    assert!(
        plan.commit()
            .freshness_upserts
            .iter()
            .all(|freshness| !freshness.remote_hint_pending)
    );
}

#[test]
fn hydrated_remote_drift_preserves_synced_state_and_queues_hydration() {
    let mount = mount();
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");
    let current = entity(
        "issue-1",
        "teams/ENG/ENG-1/page.md",
        HydrationState::Hydrated,
        "remote-v1",
    );
    store.save_entity(current.clone()).expect("save entity");
    let mut enrollment = AutoSaveEnrollmentRecord::new(
        mount.mount_id.clone(),
        current.path.clone(),
        AutoSaveOrigin::UserEnabled,
        "created",
    );
    enrollment.remote_id = Some(current.remote_id.clone());
    store
        .save_auto_save_enrollment(enrollment.clone())
        .expect("save auto-save");
    let mut remote = page_entry("issue-1", "teams/ENG/ENG-1/page.md", "remote-v2");
    remote.title = "Remote title".to_string();

    let plan = plan_batch_discovery(
        &store,
        &mount,
        BatchObserveResult::incremental(
            vec![BatchObservationChange::Upsert(remote.clone())],
            checkpoint(2, "{}"),
        ),
        NOW,
        None,
        &BTreeMap::new(),
    )
    .expect("remote drift plan");

    assert!(plan.commit().entity_upserts.is_empty());
    assert_eq!(plan.commit().observation_upserts.len(), 1);
    assert_eq!(
        plan.commit().observation_upserts[0].remote_version,
        Some(RemoteVersion::new("remote-v2"))
    );
    assert_eq!(plan.commit().freshness_upserts[0].tier, FreshnessTier::Warm);
    assert!(plan.commit().freshness_upserts[0].remote_hint_pending);
    assert_eq!(
        plan.commit().auto_save_upserts,
        vec![
            enrollment
                .clone()
                .paused_remote_changed("remote discovery is awaiting hydration", NOW)
        ]
    );
    assert_eq!(
        plan.post_commit,
        vec![DiscoveryPostCommitAction::QueueHydration(
            HydrationRequest::new(
                mount.mount_id.clone(),
                current.remote_id.clone(),
                mount.root.join(&current.path),
                HydrationState::Hydrated,
                HydrationReason::RemoteFastForward,
            )
        )]
    );
    assert_eq!(
        store
            .get_entity(&mount.mount_id, &current.remote_id)
            .expect("entity"),
        Some(current)
    );
    assert_eq!(
        store
            .get_auto_save_enrollment(&mount.mount_id, &enrollment.path)
            .expect("auto-save"),
        Some(enrollment)
    );
}

#[test]
fn safe_hydrated_move_preserves_synced_fields() {
    let mount = mount();
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");
    let current = entity(
        "issue-1",
        "old/page.md",
        HydrationState::Hydrated,
        "remote-v1",
    );
    store.save_entity(current.clone()).expect("save entity");
    let mut enrollment = AutoSaveEnrollmentRecord::new(
        mount.mount_id.clone(),
        current.path.clone(),
        AutoSaveOrigin::UserEnabled,
        "created",
    );
    enrollment.remote_id = Some(current.remote_id.clone());
    store
        .save_auto_save_enrollment(enrollment.clone())
        .expect("save auto-save");
    let mut moved = page_entry("issue-1", "new/page.md", "remote-v2");
    moved.title = "Moved title".to_string();
    let assessments = BTreeMap::from([(current.remote_id.clone(), ProjectionAssessment::Safe)]);

    let plan = plan_batch_discovery(
        &store,
        &mount,
        BatchObserveResult::incremental(
            vec![BatchObservationChange::Upsert(moved.clone())],
            checkpoint(2, "{}"),
        ),
        NOW,
        None,
        &assessments,
    )
    .expect("move plan");

    let mut expected = current.clone();
    expected.path = moved.path.clone();
    expected.title = moved.title.clone();
    assert_eq!(plan.commit().entity_upserts, vec![expected]);
    assert_eq!(
        plan.projection_actions,
        vec![DiscoveryProjectionAction::Move {
            remote_id: current.remote_id,
            kind: EntityKind::Page,
            from: current.path,
            to: moved.path,
        }]
    );
    assert_eq!(plan.post_commit.len(), 1);
    enrollment.path = PathBuf::from("new/page.md");
    assert_eq!(
        plan.commit().auto_save_upserts,
        vec![enrollment.paused_remote_changed("remote discovery is awaiting hydration", NOW,)]
    );
}

#[test]
fn existing_entity_kind_change_is_held_as_unsupported() {
    let mount = mount();
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");
    let current = entity("issue-1", "same/page.md", HydrationState::Stub, "remote-v1");
    store.save_entity(current.clone()).expect("save entity");
    let mut changed = page_entry("issue-1", "same/page.md", "remote-v2");
    changed.kind = EntityKind::Directory;

    let plan = plan_batch_discovery(
        &store,
        &mount,
        BatchObserveResult::incremental(
            vec![BatchObservationChange::Upsert(changed)],
            checkpoint(2, "{}"),
        ),
        NOW,
        None,
        &BTreeMap::new(),
    )
    .expect("kind change hold");

    assert_eq!(
        plan.held,
        vec![HeldDiscoveryItem {
            remote_id: current.remote_id,
            change: DiscoveryChangeKind::RemoteDrift,
            reason: DiscoveryHoldReason::UnsupportedKindChange {
                from: EntityKind::Page,
                to: EntityKind::Directory,
            },
        }]
    );
    assert!(plan.projection_actions.is_empty());
    assert!(plan.commit().entity_upserts.is_empty());
    assert!(plan.commit().freshness_upserts[0].remote_hint_pending);
}

#[test]
fn empty_incremental_replays_held_move_and_delete() {
    let mount = mount();
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");
    let moving = entity("move", "old/page.md", HydrationState::Stub, "remote-v1");
    let deleting = entity(
        "delete",
        "delete/page.md",
        HydrationState::Stub,
        "remote-v1",
    );
    store.save_entity(moving.clone()).expect("save moving");
    store.save_entity(deleting.clone()).expect("save deleting");
    let moved = page_entry("move", "new/page.md", "remote-v2");

    let held = plan_batch_discovery(
        &store,
        &mount,
        BatchObserveResult::incremental(
            vec![
                BatchObservationChange::Upsert(moved.clone()),
                BatchObservationChange::Tombstone {
                    remote_id: deleting.remote_id.clone(),
                },
            ],
            checkpoint(2, r#"{"cursor":"held"}"#),
        ),
        NOW,
        None,
        &BTreeMap::new(),
    )
    .expect("held plan");
    assert_eq!(held.held.len(), 2);
    store
        .commit_discovery(held.into_commit())
        .expect("persist held intents");

    let assessments = BTreeMap::from([
        (moving.remote_id.clone(), ProjectionAssessment::Safe),
        (deleting.remote_id.clone(), ProjectionAssessment::Safe),
    ]);
    let replayed = plan_batch_discovery(
        &store,
        &mount,
        BatchObserveResult::incremental(vec![], checkpoint(2, r#"{"cursor":"next"}"#)),
        "unix_ms:100001",
        None,
        &assessments,
    )
    .expect("replayed plan");

    assert_eq!(
        replayed.projection_actions,
        vec![
            DiscoveryProjectionAction::Delete {
                remote_id: deleting.remote_id.clone(),
                kind: EntityKind::Page,
                path: deleting.path.clone(),
            },
            DiscoveryProjectionAction::Move {
                remote_id: moving.remote_id.clone(),
                kind: EntityKind::Page,
                from: moving.path.clone(),
                to: moved.path.clone(),
            },
        ]
    );
    assert_eq!(replayed.commit().entity_deletes, vec![deleting.remote_id]);
    assert_eq!(replayed.commit().entity_upserts[0].path, moved.path);
}

#[test]
fn newer_replay_requires_update_but_incoming_change_wins() {
    let mount = mount();
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");
    let current = entity("issue-1", "old/page.md", HydrationState::Stub, "remote-v1");
    store.save_entity(current.clone()).expect("save entity");
    let held = plan_batch_discovery(
        &store,
        &mount,
        BatchObserveResult::incremental(
            vec![BatchObservationChange::Upsert(page_entry(
                "issue-1",
                "new/page.md",
                "remote-v2",
            ))],
            checkpoint(2, "{}"),
        ),
        NOW,
        None,
        &BTreeMap::new(),
    )
    .expect("held plan");
    store
        .commit_discovery(held.into_commit())
        .expect("persist held intent");
    let mut observation = store
        .get_remote_observation(&mount.mount_id, &current.remote_id)
        .expect("observation")
        .expect("held observation");
    let mut envelope: serde_json::Value =
        serde_json::from_str(&observation.raw_metadata_json).expect("replay JSON");
    envelope["state_version"] = serde_json::json!(2);
    envelope["min_reader_version"] = serde_json::json!(2);
    observation.raw_metadata_json = serde_json::to_string(&envelope).expect("encode newer");
    store
        .save_remote_observation(observation)
        .expect("save newer replay");

    let error = plan_batch_discovery(
        &store,
        &mount,
        BatchObserveResult::incremental(vec![], checkpoint(2, "{}")),
        "unix_ms:100001",
        None,
        &BTreeMap::new(),
    )
    .expect_err("newer replay must fail");
    assert_eq!(
        error,
        LocalityError::UpdateRequired {
            component: "daemon:discovery_replay".to_string(),
            found: 2,
            supported: 1,
        }
    );

    let incoming = page_entry("issue-1", "latest/page.md", "remote-v3");
    let plan = plan_batch_discovery(
        &store,
        &mount,
        BatchObserveResult::incremental(
            vec![BatchObservationChange::Upsert(incoming.clone())],
            checkpoint(3, "{}"),
        ),
        "unix_ms:100002",
        None,
        &BTreeMap::from([(incoming.remote_id.clone(), ProjectionAssessment::Safe)]),
    )
    .expect("incoming change supersedes replay");
    assert_eq!(plan.commit().entity_upserts[0].path, incoming.path);
}

#[test]
fn complete_omission_cleans_up_a_recognized_pending_create() {
    let mount = mount();
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount.clone()).expect("save mount");
    let pending = page_entry("pending", "pending/page.md", "remote-v1");
    let held = plan_batch_discovery(
        &store,
        &mount,
        BatchObserveResult::incremental(
            vec![BatchObservationChange::Upsert(pending.clone())],
            checkpoint(1, r#"{"cursor":"held"}"#),
        ),
        NOW,
        None,
        &BTreeMap::new(),
    )
    .expect("held create");
    store
        .commit_discovery(held.into_commit())
        .expect("persist held create");
    let mut observation = store
        .get_remote_observation(&mount.mount_id, &pending.remote_id)
        .expect("observation")
        .expect("held observation");
    let mut envelope: serde_json::Value =
        serde_json::from_str(&observation.raw_metadata_json).expect("replay JSON");
    envelope["state_version"] = serde_json::json!(2);
    envelope["min_reader_version"] = serde_json::json!(2);
    observation.raw_metadata_json = serde_json::to_string(&envelope).expect("encode newer");
    store
        .save_remote_observation(observation)
        .expect("save newer replay");

    let complete = plan_batch_discovery(
        &store,
        &mount,
        BatchObserveResult::complete(vec![], checkpoint(1, r#"{"cursor":"complete"}"#)),
        "unix_ms:100001",
        None,
        &BTreeMap::new(),
    )
    .expect("complete cleanup");

    assert!(complete.projection_actions.is_empty());
    assert_eq!(complete.commit().entity_deletes, vec![pending.remote_id]);
    assert!(complete.commit().observation_upserts.is_empty());
    assert!(complete.commit().freshness_upserts.is_empty());
}

#[test]
fn held_move_commits_pause_observation_freshness_and_checkpoint_atomically() {
    let mut memory = InMemoryStateStore::new();
    exercise_atomic_hold(&mut memory);

    let root = temp_root("discovery-atomic-hold");
    let mut sqlite = SqliteStateStore::open(root.clone()).expect("open sqlite");
    exercise_atomic_hold(&mut sqlite);
    drop(sqlite);
    fs::remove_dir_all(root).expect("remove sqlite fixture");
}

fn exercise_atomic_hold<S>(store: &mut S)
where
    S: MountRepository
        + EntityRepository
        + FreshnessStateRepository
        + RemoteObservationRepository
        + AutoSaveRepository
        + MetadataDiscoveryJobRepository
        + ConnectorStateRepository
        + DiscoveryRepository,
{
    let mount = mount();
    store.save_mount(mount.clone()).expect("save mount");
    let current = entity("issue-1", "old/page.md", HydrationState::Dirty, "remote-v1");
    store.save_entity(current.clone()).expect("save entity");
    let mut freshness = FreshnessStateRecord::new(
        mount.mount_id.clone(),
        current.remote_id.clone(),
        FreshnessTier::Hot,
    );
    freshness.next_check_at = Some("unix_ms:200000".to_string());
    freshness.last_opened_at = Some("unix_ms:90000".to_string());
    freshness.last_local_change_at = Some("unix_ms:95000".to_string());
    store
        .save_freshness_state(freshness.clone())
        .expect("save freshness");
    let mut enrollment = AutoSaveEnrollmentRecord::new(
        mount.mount_id.clone(),
        current.path.clone(),
        AutoSaveOrigin::UserEnabled,
        "created",
    );
    enrollment.remote_id = Some(current.remote_id.clone());
    store
        .save_auto_save_enrollment(enrollment.clone())
        .expect("save auto-save");
    store
        .upsert_metadata_discovery_job(MetadataDiscoveryJobRecord {
            mount_id: mount.mount_id.clone(),
            container_identifier: "batch:linear-main".to_string(),
            priority: MetadataDiscoveryPriority::Background,
            depth: 0,
            attempts: 3,
            last_error: Some("retry".to_string()),
            created_at: "created".to_string(),
            updated_at: "updated".to_string(),
        })
        .expect("save metadata job");
    let old_checkpoint = connector_state(1, r#"{"cursor":"old"}"#, "unix_ms:1");
    store
        .save_connector_state(old_checkpoint.clone())
        .expect("save checkpoint");
    let moved = page_entry("issue-1", "new/page.md", "remote-v2");

    let plan = plan_batch_discovery(
        store,
        &mount,
        BatchObserveResult::incremental(
            vec![BatchObservationChange::Upsert(moved.clone())],
            checkpoint(2, r#"{"cursor":"new"}"#),
        ),
        NOW,
        Some("batch:linear-main"),
        &BTreeMap::from([(current.remote_id.clone(), ProjectionAssessment::Safe)]),
    )
    .expect("held plan");
    assert_eq!(
        store
            .get_connector_state("linear", "mount", "linear-main")
            .expect("old checkpoint"),
        Some(old_checkpoint)
    );

    store
        .commit_discovery(plan.into_commit())
        .expect("commit held discovery");

    assert_eq!(
        store
            .get_entity(&mount.mount_id, &current.remote_id)
            .expect("entity"),
        Some(current.clone())
    );
    let observation = store
        .get_remote_observation(&mount.mount_id, &current.remote_id)
        .expect("observation")
        .expect("held observation");
    assert_eq!(observation.projected_path, moved.path);
    assert!(!observation.deleted);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&observation.raw_metadata_json)
            .expect("replay envelope"),
        serde_json::json!({
            "tag": "localityd.discovery_replay",
            "state_version": 1,
            "min_reader_version": 1,
            "change": {
                "type": "upsert",
                "entry": moved,
            }
        })
    );
    let persisted_freshness = store
        .get_freshness_state(&mount.mount_id, &current.remote_id)
        .expect("freshness")
        .expect("persisted freshness");
    assert_eq!(persisted_freshness.next_check_at, freshness.next_check_at);
    assert_eq!(persisted_freshness.last_opened_at, freshness.last_opened_at);
    assert_eq!(
        persisted_freshness.last_local_change_at,
        freshness.last_local_change_at
    );
    assert_eq!(persisted_freshness.last_checked_at.as_deref(), Some(NOW));
    assert!(persisted_freshness.remote_hint_pending);
    let paused = store
        .get_auto_save_enrollment(&mount.mount_id, &current.path)
        .expect("auto-save")
        .expect("paused auto-save");
    assert_eq!(paused.state, AutoSaveState::PausedRemoteChanged);
    assert_eq!(
        paused.last_reason.as_deref(),
        Some("remote discovery is held for local review")
    );
    assert_eq!(paused.updated_at, NOW);
    assert!(
        store
            .list_metadata_discovery_jobs()
            .expect("metadata jobs")
            .is_empty()
    );
    assert_eq!(
        store
            .get_connector_state("linear", "mount", "linear-main")
            .expect("new checkpoint"),
        Some(connector_state(2, r#"{"cursor":"new"}"#, NOW,))
    );
}

fn mount() -> MountConfig {
    MountConfig::new(MountId::new("linear-main"), "linear", "/tmp/linear-main")
}

fn page_entry(remote_id: &str, path: &str, remote_version: &str) -> TreeEntry {
    TreeEntry {
        mount_id: MountId::new("linear-main"),
        remote_id: RemoteId::new(remote_id),
        kind: EntityKind::Page,
        title: remote_id.to_string(),
        path: PathBuf::from(path),
        hydration: HydrationState::Stub,
        content_hash: Some(format!("hash:{remote_id}")),
        remote_edited_at: Some(remote_version.to_string()),
        stub_frontmatter: Some(format!("title: {remote_id}\n")),
    }
}

fn entity(
    remote_id: &str,
    path: &str,
    hydration: HydrationState,
    remote_version: &str,
) -> EntityRecord {
    EntityRecord::new(
        MountId::new("linear-main"),
        RemoteId::new(remote_id),
        EntityKind::Page,
        remote_id,
        path,
    )
    .with_hydration(hydration)
    .with_content_hash(format!("synced:{remote_id}"))
    .with_synced_tree_remote_version(remote_version)
}

fn checkpoint(state_version: i64, state_json: &str) -> ConnectorCheckpoint {
    ConnectorCheckpoint {
        state_version,
        min_reader_version: 1,
        state_json: state_json.to_string(),
    }
}

fn connector_state(state_version: i64, state_json: &str, updated_at: &str) -> ConnectorStateRecord {
    ConnectorStateRecord {
        connector: "linear".to_string(),
        scope_kind: "mount".to_string(),
        scope_id: "linear-main".to_string(),
        state_version,
        min_reader_version: 1,
        state_json: state_json.to_string(),
        updated_at: updated_at.to_string(),
    }
}

fn temp_root(label: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("loc-{label}-{}-{suffix}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create temp root");
    root
}
