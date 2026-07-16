use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use locality_core::freshness::{FreshnessTier, RemoteVersion};
use locality_core::hydration::HydrationReason;
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use locality_core::shadow::ShadowDocument;
use locality_store::{
    AutoSaveEnrollmentRecord, AutoSaveOrigin, AutoSaveRepository, ConnectorStateRecord,
    ConnectorStateRepository, DiscoveryCommit, DiscoveryRepository, EntityRecord, EntityRepository,
    EntitySearchRepository, FreshnessStateRecord, FreshnessStateRepository, HydrationJobRecord,
    HydrationJobRepository, InMemoryStateStore, MetadataDiscoveryJobRecord,
    MetadataDiscoveryJobRepository, MetadataDiscoveryPriority, MountConfig, MountRepository,
    RemoteObservationRecord, RemoteObservationRepository, ShadowRepository, SqliteStateStore,
    StoreError, VirtualMutationKind, VirtualMutationRecord, VirtualMutationRepository,
};
use rusqlite::Connection;

#[test]
fn discovery_commit_round_trips_in_memory_and_sqlite() {
    let mut memory = InMemoryStateStore::new();
    exercise_round_trip(&mut memory);

    let fixture = SqliteFixture::new();
    let mut sqlite = fixture.open();
    exercise_round_trip(&mut sqlite);
    drop(sqlite);

    let reopened = fixture.open();
    assert_round_trip(&reopened);
}

#[test]
fn discovery_commit_allows_entity_path_swaps_and_cycles() {
    let mut memory = InMemoryStateStore::new();
    exercise_path_cycle(&mut memory);

    let fixture = SqliteFixture::new();
    let mut sqlite = fixture.open();
    exercise_path_cycle(&mut sqlite);
}

#[test]
fn discovery_commit_rehomes_path_addressed_state_without_resetting_it() {
    let mut memory = InMemoryStateStore::new();
    exercise_move_rehome(&mut memory);

    let fixture = SqliteFixture::new();
    let mut sqlite = fixture.open();
    exercise_move_rehome(&mut sqlite);
}

#[test]
fn discovery_delete_removes_only_entity_owned_state() {
    let mut memory = InMemoryStateStore::new();
    exercise_delete_cleanup(&mut memory);

    let fixture = SqliteFixture::new();
    let mut sqlite = fixture.open();
    exercise_delete_cleanup(&mut sqlite);
}

#[test]
fn discovery_validation_errors_leave_both_stores_unchanged() {
    let mut memory = InMemoryStateStore::new();
    exercise_validation_rollback(&mut memory);

    let fixture = SqliteFixture::new();
    let mut sqlite = fixture.open();
    exercise_validation_rollback(&mut sqlite);
}

#[test]
fn discovery_moves_and_deletes_hold_pending_virtual_mutations() {
    let mut memory = InMemoryStateStore::new();
    exercise_pending_mutation_guards(&mut memory);

    let fixture = SqliteFixture::new();
    let mut sqlite = fixture.open();
    exercise_pending_mutation_guards(&mut sqlite);
}

#[test]
fn discovery_rejects_inconsistent_auto_save_ownership() {
    let mut memory = InMemoryStateStore::new();
    exercise_auto_save_ownership_guards(&mut memory);

    let fixture = SqliteFixture::new();
    let mut sqlite = fixture.open();
    exercise_auto_save_ownership_guards(&mut sqlite);
}

#[test]
fn sqlite_checkpoint_failure_rolls_back_all_discovery_changes() {
    let fixture = SqliteFixture::new();
    let mut store = fixture.open();
    seed_mount(&mut store, MAIN_MOUNT);

    let original = entity("issue-1", "Old/page.md", "Original");
    let deleted = entity("issue-2", "Deleted/page.md", "Must survive rollback");
    store.save_entity(original.clone()).expect("save original");
    store
        .save_entity(deleted.clone())
        .expect("save deleted candidate");
    let original_observation = observation("issue-1", "Old/page.md", "remote-v1");
    store
        .save_remote_observation(original_observation.clone())
        .expect("save original observation");
    let original_freshness = freshness("issue-1", "2026-07-15T00:00:00Z");
    store
        .save_freshness_state(original_freshness.clone())
        .expect("save original freshness");
    store
        .save_shadow(&mount_id(MAIN_MOUNT), shadow("issue-2", "body"))
        .expect("save shadow");
    store
        .upsert_hydration_job(hydration_job("issue-2", "Deleted/page.md"))
        .expect("save hydration job");
    let old_checkpoint = checkpoint(1, r#"{"watermark":"old"}"#);
    store
        .save_connector_state(old_checkpoint.clone())
        .expect("save old checkpoint");

    let connection = Connection::open(&store.db_path).expect("open raw sqlite");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_discovery_checkpoint
             BEFORE INSERT ON connector_state
             WHEN NEW.connector = 'linear'
              AND NEW.scope_kind = 'mount'
              AND NEW.scope_id = 'linear-main'
              AND EXISTS (
                  SELECT 1 FROM entities
                  WHERE mount_id = 'linear-main'
                    AND remote_id = 'issue-1'
                    AND path = 'New/page.md'
              )
             BEGIN
                 SELECT RAISE(ABORT, 'injected checkpoint failure');
             END;",
        )
        .expect("install failure trigger");
    drop(connection);

    let paused_after_move = paused_auto_save("issue-1", "New/page.md");
    let error = store
        .commit_discovery(DiscoveryCommit {
            mount_id: mount_id(MAIN_MOUNT),
            entity_upserts: vec![entity("issue-1", "New/page.md", "Changed")],
            entity_deletes: vec![RemoteId::new("issue-2")],
            observation_upserts: vec![observation("issue-1", "New/page.md", "remote-v2")],
            freshness_upserts: vec![freshness("issue-1", "2026-07-15T01:00:00Z")],
            auto_save_upserts: vec![paused_after_move],
            metadata_discovery_deletes: vec![],
            virtual_mutation_deletes: vec![],
            checkpoint: checkpoint(2, r#"{"watermark":"new"}"#),
        })
        .expect_err("checkpoint trigger must abort commit");

    assert!(
        matches!(error, StoreError::Database(message) if message.contains("injected checkpoint failure"))
    );
    assert_eq!(
        store
            .get_entity(&mount_id(MAIN_MOUNT), &RemoteId::new("issue-1"))
            .expect("load original"),
        Some(original)
    );
    assert_eq!(
        store
            .get_entity(&mount_id(MAIN_MOUNT), &RemoteId::new("issue-2"))
            .expect("load deletion candidate"),
        Some(deleted)
    );
    assert_eq!(
        store
            .get_remote_observation(&mount_id(MAIN_MOUNT), &RemoteId::new("issue-1"))
            .expect("load observation"),
        Some(original_observation)
    );
    assert_eq!(
        store
            .get_freshness_state(&mount_id(MAIN_MOUNT), &RemoteId::new("issue-1"))
            .expect("load freshness"),
        Some(original_freshness)
    );
    assert_eq!(
        store
            .load_shadow(&mount_id(MAIN_MOUNT), &RemoteId::new("issue-2"))
            .expect("shadow survived"),
        shadow("issue-2", "body")
    );
    assert_eq!(
        store.list_hydration_jobs().expect("hydration jobs").len(),
        1
    );
    assert!(
        store
            .get_auto_save_enrollment(&mount_id(MAIN_MOUNT), Path::new("New/page.md"))
            .expect("rolled back auto-save")
            .is_none()
    );
    assert_eq!(
        store
            .get_connector_state("linear", "mount", MAIN_MOUNT)
            .expect("load checkpoint"),
        Some(old_checkpoint)
    );
    assert_eq!(
        store
            .list_entity_search_candidates(&mount_id(MAIN_MOUNT), "survive", None)
            .expect("search")
            .expect("sqlite index")
            .len(),
        1
    );
}

const MAIN_MOUNT: &str = "linear-main";
const OTHER_MOUNT: &str = "linear-other";

fn exercise_round_trip<S>(store: &mut S)
where
    S: MountRepository
        + DiscoveryRepository
        + EntityRepository
        + RemoteObservationRepository
        + FreshnessStateRepository
        + AutoSaveRepository
        + ConnectorStateRepository,
{
    seed_mount(store, MAIN_MOUNT);
    let entity = entity("issue-1", "Teams/ENG/ENG-1/page.md", "Fix login");
    let observation = observation("issue-1", "Teams/ENG/ENG-1/page.md", "remote-v1");
    let freshness = freshness("issue-1", "2026-07-15T00:00:00Z");
    let auto_save = paused_auto_save("issue-1", "Teams/ENG/ENG-1/page.md");
    let checkpoint = checkpoint(3, r#"{"watermark":"2026-07-15T00:00:00Z"}"#);

    store
        .commit_discovery(DiscoveryCommit {
            mount_id: mount_id(MAIN_MOUNT),
            entity_upserts: vec![entity.clone()],
            entity_deletes: vec![],
            observation_upserts: vec![observation.clone()],
            freshness_upserts: vec![freshness.clone()],
            auto_save_upserts: vec![auto_save.clone()],
            metadata_discovery_deletes: vec![],
            virtual_mutation_deletes: vec![],
            checkpoint: checkpoint.clone(),
        })
        .expect("commit discovery");

    assert_eq!(
        store
            .get_entity(&mount_id(MAIN_MOUNT), &RemoteId::new("issue-1"))
            .expect("load entity"),
        Some(entity)
    );
    assert_eq!(
        store
            .get_remote_observation(&mount_id(MAIN_MOUNT), &RemoteId::new("issue-1"))
            .expect("load observation"),
        Some(observation)
    );
    assert_eq!(
        store
            .get_freshness_state(&mount_id(MAIN_MOUNT), &RemoteId::new("issue-1"))
            .expect("load freshness"),
        Some(freshness)
    );
    assert_eq!(
        store
            .get_auto_save_enrollment(&mount_id(MAIN_MOUNT), Path::new("Teams/ENG/ENG-1/page.md"),)
            .expect("load auto-save"),
        Some(auto_save)
    );
    assert_eq!(
        store
            .get_connector_state("linear", "mount", MAIN_MOUNT)
            .expect("load checkpoint"),
        Some(checkpoint)
    );
}

fn assert_round_trip<S>(store: &S)
where
    S: EntityRepository
        + RemoteObservationRepository
        + FreshnessStateRepository
        + AutoSaveRepository
        + ConnectorStateRepository,
{
    assert_eq!(
        store
            .list_entities(&mount_id(MAIN_MOUNT))
            .expect("entities"),
        vec![entity("issue-1", "Teams/ENG/ENG-1/page.md", "Fix login")]
    );
    assert_eq!(
        store
            .list_remote_observations(&mount_id(MAIN_MOUNT))
            .expect("observations"),
        vec![observation(
            "issue-1",
            "Teams/ENG/ENG-1/page.md",
            "remote-v1"
        )]
    );
    assert_eq!(
        store
            .list_freshness_states(&mount_id(MAIN_MOUNT))
            .expect("freshness"),
        vec![freshness("issue-1", "2026-07-15T00:00:00Z")]
    );
    assert_eq!(
        store
            .list_auto_save_enrollments(&mount_id(MAIN_MOUNT))
            .expect("auto-save"),
        vec![paused_auto_save("issue-1", "Teams/ENG/ENG-1/page.md")]
    );
    assert_eq!(
        store
            .get_connector_state("linear", "mount", MAIN_MOUNT)
            .expect("checkpoint"),
        Some(checkpoint(3, r#"{"watermark":"2026-07-15T00:00:00Z"}"#))
    );
}

fn exercise_path_cycle<S>(store: &mut S)
where
    S: MountRepository + EntityRepository + DiscoveryRepository + ConnectorStateRepository,
{
    seed_mount(store, MAIN_MOUNT);
    for (id, path, title) in [
        ("issue-a", "A/page.md", "A"),
        ("issue-b", "B/page.md", "B"),
        ("issue-c", "C/page.md", "C"),
    ] {
        store
            .save_entity(entity(id, path, title))
            .expect("seed entity");
    }

    store
        .commit_discovery(DiscoveryCommit {
            mount_id: mount_id(MAIN_MOUNT),
            entity_upserts: vec![
                entity("issue-a", "B/page.md", "A moved"),
                entity("issue-b", "C/page.md", "B moved"),
                entity("issue-c", "A/page.md", "C moved"),
            ],
            entity_deletes: vec![],
            observation_upserts: vec![],
            freshness_upserts: vec![],
            auto_save_upserts: vec![],
            metadata_discovery_deletes: vec![],
            virtual_mutation_deletes: vec![],
            checkpoint: checkpoint(1, "{}"),
        })
        .expect("rotate entity paths");

    assert_eq!(
        store
            .list_entities(&mount_id(MAIN_MOUNT))
            .expect("list rotated entities"),
        vec![
            entity("issue-c", "A/page.md", "C moved"),
            entity("issue-a", "B/page.md", "A moved"),
            entity("issue-b", "C/page.md", "B moved"),
        ]
    );
}

fn exercise_move_rehome<S>(store: &mut S)
where
    S: MountRepository
        + EntityRepository
        + DiscoveryRepository
        + ConnectorStateRepository
        + HydrationJobRepository
        + AutoSaveRepository,
{
    seed_mount(store, MAIN_MOUNT);
    store
        .save_entity(entity("issue-1", "Old/page.md", "Issue"))
        .expect("seed entity");
    store
        .save_entity(entity("issue-deleted", "Vacated/page.md", "Deleted issue"))
        .expect("seed deleted entity");
    let mut job = hydration_job("issue-1", "Old/page.md");
    job.attempts = 4;
    job.last_error = Some("temporary failure".to_string());
    store
        .upsert_hydration_job(job.clone())
        .expect("seed hydration job");
    let mut enrollment = AutoSaveEnrollmentRecord::new(
        mount_id(MAIN_MOUNT),
        "Old/page.md",
        AutoSaveOrigin::UserEnabled,
        "created",
    )
    .paused_failure("waiting", "updated");
    enrollment.remote_id = Some(RemoteId::new("issue-1"));
    enrollment.last_push_id = Some("push-7".to_string());
    store
        .save_auto_save_enrollment(enrollment.clone())
        .expect("seed enrollment");
    let mut deleted_enrollment = AutoSaveEnrollmentRecord::new(
        mount_id(MAIN_MOUNT),
        "Vacated/page.md",
        AutoSaveOrigin::UserEnabled,
        "created",
    );
    deleted_enrollment.remote_id = Some(RemoteId::new("issue-deleted"));
    store
        .save_auto_save_enrollment(deleted_enrollment)
        .expect("seed deleted enrollment");
    let mut paused_after_move = enrollment.clone();
    paused_after_move.path = PathBuf::from("Vacated/page.md");
    paused_after_move = paused_after_move.paused_remote_changed("remote move held", "discovered");

    store
        .commit_discovery(DiscoveryCommit {
            mount_id: mount_id(MAIN_MOUNT),
            entity_upserts: vec![entity("issue-1", "Vacated/page.md", "Issue")],
            entity_deletes: vec![RemoteId::new("issue-deleted")],
            observation_upserts: vec![],
            freshness_upserts: vec![],
            auto_save_upserts: vec![paused_after_move.clone()],
            metadata_discovery_deletes: vec![],
            virtual_mutation_deletes: vec![],
            checkpoint: checkpoint(1, "{}"),
        })
        .expect("commit move");

    job.path = PathBuf::from("Vacated/page.md");
    assert_eq!(store.list_hydration_jobs().expect("jobs"), vec![job]);
    assert!(
        store
            .get_auto_save_enrollment(&mount_id(MAIN_MOUNT), Path::new("Old/page.md"))
            .expect("old enrollment")
            .is_none()
    );
    assert_eq!(
        store
            .get_auto_save_enrollment(&mount_id(MAIN_MOUNT), Path::new("Vacated/page.md"))
            .expect("new enrollment"),
        Some(paused_after_move)
    );
}

fn exercise_delete_cleanup<S>(store: &mut S)
where
    S: MountRepository
        + EntityRepository
        + DiscoveryRepository
        + ConnectorStateRepository
        + RemoteObservationRepository
        + FreshnessStateRepository
        + HydrationJobRepository
        + AutoSaveRepository
        + ShadowRepository
        + MetadataDiscoveryJobRepository
        + VirtualMutationRepository
        + EntitySearchRepository,
{
    seed_mount(store, MAIN_MOUNT);
    seed_mount(store, OTHER_MOUNT);
    seed_owned_state(
        store,
        MAIN_MOUNT,
        "issue-delete",
        "Delete/page.md",
        "Delete target",
    );
    seed_owned_state(
        store,
        MAIN_MOUNT,
        "issue-keep",
        "Keep/page.md",
        "Keep target",
    );
    seed_owned_state(
        store,
        OTHER_MOUNT,
        "issue-delete",
        "Other/page.md",
        "Other target",
    );

    let commit = DiscoveryCommit {
        mount_id: mount_id(MAIN_MOUNT),
        entity_upserts: vec![],
        entity_deletes: vec![RemoteId::new("issue-delete")],
        observation_upserts: vec![],
        freshness_upserts: vec![],
        auto_save_upserts: vec![],
        metadata_discovery_deletes: vec!["job:issue-delete".to_string()],
        virtual_mutation_deletes: vec!["mutation:issue-delete".to_string()],
        checkpoint: checkpoint(1, r#"{"complete":true}"#),
    };
    let blocked = DiscoveryCommit {
        virtual_mutation_deletes: vec![],
        ..commit.clone()
    };
    assert!(matches!(
        store.commit_discovery(blocked),
        Err(StoreError::InvalidState(_))
    ));
    assert!(
        store
            .get_entity(&mount_id(MAIN_MOUNT), &RemoteId::new("issue-delete"))
            .expect("held entity")
            .is_some()
    );
    assert!(
        store
            .get_virtual_mutation(&mount_id(MAIN_MOUNT), "mutation:issue-delete")
            .expect("held mutation")
            .is_some()
    );

    store
        .commit_discovery(commit)
        .expect("delete discovered entity");

    assert_owned_state_absent(store, MAIN_MOUNT, "issue-delete", "Delete/page.md");
    assert_owned_state_present(store, MAIN_MOUNT, "issue-keep", "Keep/page.md");
    assert_owned_state_present(store, OTHER_MOUNT, "issue-delete", "Other/page.md");
    let jobs = store
        .list_metadata_discovery_jobs()
        .expect("list metadata jobs");
    assert_eq!(
        jobs.into_iter()
            .map(|job| (job.mount_id.0, job.container_identifier))
            .collect::<Vec<_>>(),
        vec![
            (MAIN_MOUNT.to_string(), "job:issue-keep".to_string()),
            (OTHER_MOUNT.to_string(), "job:issue-delete".to_string()),
        ]
    );
    let main_mutations = store
        .list_virtual_mutations(&mount_id(MAIN_MOUNT))
        .expect("main mutations");
    assert_eq!(main_mutations.len(), 1);
    assert_eq!(main_mutations[0].local_id, "mutation:issue-keep");
    let other_mutations = store
        .list_virtual_mutations(&mount_id(OTHER_MOUNT))
        .expect("other mutations");
    assert_eq!(other_mutations.len(), 1);
    assert_eq!(other_mutations[0].local_id, "mutation:issue-delete");

    if let Some(deleted_matches) = store
        .list_entity_search_candidates(&mount_id(MAIN_MOUNT), "delete", None)
        .expect("search deleted")
    {
        assert!(deleted_matches.is_empty());
        assert_eq!(
            store
                .list_entity_search_candidates(&mount_id(MAIN_MOUNT), "keep", None)
                .expect("search kept")
                .expect("sqlite search")
                .len(),
            1
        );
        assert_eq!(
            store
                .list_entity_search_candidates(&mount_id(OTHER_MOUNT), "other", None)
                .expect("search other")
                .expect("sqlite search")
                .len(),
            1
        );
    }
}

fn exercise_pending_mutation_guards<S>(store: &mut S)
where
    S: MountRepository
        + EntityRepository
        + DiscoveryRepository
        + ConnectorStateRepository
        + VirtualMutationRepository,
{
    seed_mount(store, MAIN_MOUNT);
    let moving = entity("issue-move", "Move/page.md", "Move");
    let deleted = entity("issue-delete", "Delete/page.md", "Delete");
    let directory = EntityRecord::new(
        mount_id(MAIN_MOUNT),
        RemoteId::new("directory-move"),
        EntityKind::Directory,
        "Directory",
        "Directory",
    );
    store
        .save_entity(moving.clone())
        .expect("save moving entity");
    store
        .save_entity(deleted.clone())
        .expect("save deleted entity");
    store
        .save_entity(directory.clone())
        .expect("save directory entity");
    let old_checkpoint = checkpoint(1, r#"{"watermark":"old"}"#);
    store
        .save_connector_state(old_checkpoint.clone())
        .expect("save checkpoint");
    store
        .save_virtual_mutation(pending_mutation(
            "mutation:move",
            "issue-move",
            "Move/page.md",
        ))
        .expect("save move mutation");
    store
        .save_virtual_mutation(pending_mutation(
            "mutation:delete",
            "issue-delete",
            "Delete/page.md",
        ))
        .expect("save delete mutation");
    let mut directory_mutation = pending_mutation(
        "mutation:directory-child",
        "unused",
        "Directory/Draft/page.md",
    );
    directory_mutation.target_remote_id = None;
    store
        .save_virtual_mutation(directory_mutation)
        .expect("save directory child mutation");

    let move_error = store
        .commit_discovery(DiscoveryCommit {
            mount_id: mount_id(MAIN_MOUNT),
            entity_upserts: vec![entity("issue-move", "Moved/page.md", "Move")],
            entity_deletes: vec![],
            observation_upserts: vec![],
            freshness_upserts: vec![],
            auto_save_upserts: vec![],
            metadata_discovery_deletes: vec![],
            virtual_mutation_deletes: vec![],
            checkpoint: checkpoint(2, r#"{"watermark":"move"}"#),
        })
        .expect_err("pending mutation must hold move");
    assert!(matches!(move_error, StoreError::InvalidState(_)));

    let delete_error = store
        .commit_discovery(DiscoveryCommit {
            mount_id: mount_id(MAIN_MOUNT),
            entity_upserts: vec![],
            entity_deletes: vec![RemoteId::new("issue-delete")],
            observation_upserts: vec![],
            freshness_upserts: vec![],
            auto_save_upserts: vec![],
            metadata_discovery_deletes: vec![],
            virtual_mutation_deletes: vec![],
            checkpoint: checkpoint(2, r#"{"watermark":"delete"}"#),
        })
        .expect_err("pending mutation must hold delete");
    assert!(matches!(delete_error, StoreError::InvalidState(_)));

    let directory_error = store
        .commit_discovery(DiscoveryCommit {
            mount_id: mount_id(MAIN_MOUNT),
            entity_upserts: vec![EntityRecord {
                path: PathBuf::from("Moved Directory"),
                ..directory.clone()
            }],
            entity_deletes: vec![],
            observation_upserts: vec![],
            freshness_upserts: vec![],
            auto_save_upserts: vec![],
            metadata_discovery_deletes: vec![],
            virtual_mutation_deletes: vec![],
            checkpoint: checkpoint(2, r#"{"watermark":"directory"}"#),
        })
        .expect_err("pending descendant mutation must hold directory move");
    assert!(matches!(directory_error, StoreError::InvalidState(_)));

    assert_eq!(
        store
            .get_entity(&mount_id(MAIN_MOUNT), &RemoteId::new("issue-move"))
            .expect("moving entity"),
        Some(moving)
    );
    assert_eq!(
        store
            .get_entity(&mount_id(MAIN_MOUNT), &RemoteId::new("issue-delete"))
            .expect("deleted entity"),
        Some(deleted)
    );
    assert_eq!(
        store
            .list_virtual_mutations(&mount_id(MAIN_MOUNT))
            .expect("mutations")
            .len(),
        3
    );
    assert_eq!(
        store
            .get_connector_state("linear", "mount", MAIN_MOUNT)
            .expect("checkpoint"),
        Some(old_checkpoint)
    );
}

fn exercise_auto_save_ownership_guards<S>(store: &mut S)
where
    S: MountRepository
        + EntityRepository
        + DiscoveryRepository
        + ConnectorStateRepository
        + AutoSaveRepository,
{
    seed_mount(store, MAIN_MOUNT);
    let moving = entity("issue-move", "Move/page.md", "Move");
    let deleted = entity("issue-delete", "Delete/page.md", "Delete");
    store
        .save_entity(moving.clone())
        .expect("save moving entity");
    store
        .save_entity(deleted.clone())
        .expect("save deleted entity");
    let old_checkpoint = checkpoint(1, r#"{"watermark":"old"}"#);
    store
        .save_connector_state(old_checkpoint.clone())
        .expect("save checkpoint");
    let mut move_enrollment = AutoSaveEnrollmentRecord::new(
        mount_id(MAIN_MOUNT),
        "Move/page.md",
        AutoSaveOrigin::UserEnabled,
        "created",
    );
    move_enrollment.remote_id = Some(RemoteId::new("other-move"));
    store
        .save_auto_save_enrollment(move_enrollment.clone())
        .expect("save inconsistent move enrollment");
    let mut delete_enrollment = AutoSaveEnrollmentRecord::new(
        mount_id(MAIN_MOUNT),
        "Delete/page.md",
        AutoSaveOrigin::UserEnabled,
        "created",
    );
    delete_enrollment.remote_id = Some(RemoteId::new("other-delete"));
    store
        .save_auto_save_enrollment(delete_enrollment.clone())
        .expect("save inconsistent delete enrollment");

    for commit in [
        DiscoveryCommit {
            mount_id: mount_id(MAIN_MOUNT),
            entity_upserts: vec![entity("issue-move", "Moved/page.md", "Move")],
            entity_deletes: vec![],
            observation_upserts: vec![],
            freshness_upserts: vec![],
            auto_save_upserts: vec![],
            metadata_discovery_deletes: vec![],
            virtual_mutation_deletes: vec![],
            checkpoint: checkpoint(2, r#"{"watermark":"move"}"#),
        },
        DiscoveryCommit {
            mount_id: mount_id(MAIN_MOUNT),
            entity_upserts: vec![],
            entity_deletes: vec![RemoteId::new("issue-delete")],
            observation_upserts: vec![],
            freshness_upserts: vec![],
            auto_save_upserts: vec![],
            metadata_discovery_deletes: vec![],
            virtual_mutation_deletes: vec![],
            checkpoint: checkpoint(2, r#"{"watermark":"delete"}"#),
        },
    ] {
        assert!(matches!(
            store.commit_discovery(commit),
            Err(StoreError::InvalidState(_))
        ));
    }

    assert_eq!(
        store
            .get_entity(&mount_id(MAIN_MOUNT), &RemoteId::new("issue-move"))
            .expect("moving entity"),
        Some(moving)
    );
    assert_eq!(
        store
            .get_entity(&mount_id(MAIN_MOUNT), &RemoteId::new("issue-delete"))
            .expect("deleted entity"),
        Some(deleted)
    );
    assert_eq!(
        store
            .list_auto_save_enrollments(&mount_id(MAIN_MOUNT))
            .expect("enrollments"),
        vec![delete_enrollment, move_enrollment]
    );
    assert_eq!(
        store
            .get_connector_state("linear", "mount", MAIN_MOUNT)
            .expect("checkpoint"),
        Some(old_checkpoint)
    );
}

fn exercise_validation_rollback<S>(store: &mut S)
where
    S: MountRepository + EntityRepository + DiscoveryRepository + ConnectorStateRepository,
{
    seed_mount(store, MAIN_MOUNT);
    let original = entity("issue-1", "Original/page.md", "Original");
    store.save_entity(original.clone()).expect("seed entity");
    let old_checkpoint = checkpoint(1, r#"{"watermark":"old"}"#);
    store
        .save_connector_state(old_checkpoint.clone())
        .expect("seed checkpoint");

    let cases = vec![
        (
            "foreign entity mount",
            DiscoveryCommit {
                mount_id: mount_id(MAIN_MOUNT),
                entity_upserts: vec![EntityRecord::new(
                    mount_id(OTHER_MOUNT),
                    RemoteId::new("issue-2"),
                    EntityKind::Page,
                    "Wrong",
                    "Wrong/page.md",
                )],
                entity_deletes: vec![],
                observation_upserts: vec![],
                freshness_upserts: vec![],
                auto_save_upserts: vec![],
                metadata_discovery_deletes: vec![],
                virtual_mutation_deletes: vec![],
                checkpoint: checkpoint(2, "{}"),
            },
        ),
        (
            "duplicate and contradictory ids",
            DiscoveryCommit {
                mount_id: mount_id(MAIN_MOUNT),
                entity_upserts: vec![
                    entity("issue-1", "Changed/page.md", "Changed"),
                    entity("issue-1", "Changed-again/page.md", "Changed again"),
                ],
                entity_deletes: vec![RemoteId::new("issue-1")],
                observation_upserts: vec![],
                freshness_upserts: vec![],
                auto_save_upserts: vec![],
                metadata_discovery_deletes: vec![],
                virtual_mutation_deletes: vec![],
                checkpoint: checkpoint(2, "{}"),
            },
        ),
        (
            "foreign observation and freshness mounts",
            DiscoveryCommit {
                mount_id: mount_id(MAIN_MOUNT),
                entity_upserts: vec![entity("issue-1", "Changed/page.md", "Changed")],
                entity_deletes: vec![],
                observation_upserts: vec![RemoteObservationRecord::new(
                    mount_id(OTHER_MOUNT),
                    RemoteId::new("issue-1"),
                    EntityKind::Page,
                    "Wrong",
                    "Wrong/page.md",
                    "now",
                )],
                freshness_upserts: vec![FreshnessStateRecord::new(
                    mount_id(OTHER_MOUNT),
                    RemoteId::new("issue-1"),
                    FreshnessTier::Hot,
                )],
                auto_save_upserts: vec![],
                metadata_discovery_deletes: vec![],
                virtual_mutation_deletes: vec![],
                checkpoint: checkpoint(2, "{}"),
            },
        ),
        (
            "invalid checkpoint scope",
            DiscoveryCommit {
                mount_id: mount_id(MAIN_MOUNT),
                entity_upserts: vec![entity("issue-1", "Changed/page.md", "Changed")],
                entity_deletes: vec![],
                observation_upserts: vec![],
                freshness_upserts: vec![],
                auto_save_upserts: vec![],
                metadata_discovery_deletes: vec![],
                virtual_mutation_deletes: vec![],
                checkpoint: ConnectorStateRecord {
                    scope_id: OTHER_MOUNT.to_string(),
                    ..checkpoint(2, "{}")
                },
            },
        ),
        (
            "invalid checkpoint versions",
            DiscoveryCommit {
                mount_id: mount_id(MAIN_MOUNT),
                entity_upserts: vec![entity("issue-1", "Changed/page.md", "Changed")],
                entity_deletes: vec![],
                observation_upserts: vec![],
                freshness_upserts: vec![],
                auto_save_upserts: vec![],
                metadata_discovery_deletes: vec![],
                virtual_mutation_deletes: vec![],
                checkpoint: ConnectorStateRecord {
                    state_version: 1,
                    min_reader_version: 2,
                    ..checkpoint(2, "{}")
                },
            },
        ),
        (
            "invalid checkpoint json",
            DiscoveryCommit {
                mount_id: mount_id(MAIN_MOUNT),
                entity_upserts: vec![entity("issue-1", "Changed/page.md", "Changed")],
                entity_deletes: vec![],
                observation_upserts: vec![],
                freshness_upserts: vec![],
                auto_save_upserts: vec![],
                metadata_discovery_deletes: vec![],
                virtual_mutation_deletes: vec![],
                checkpoint: checkpoint(2, "not-json"),
            },
        ),
        (
            "wrong checkpoint connector",
            DiscoveryCommit {
                mount_id: mount_id(MAIN_MOUNT),
                entity_upserts: vec![entity("issue-1", "Changed/page.md", "Changed")],
                entity_deletes: vec![],
                observation_upserts: vec![],
                freshness_upserts: vec![],
                auto_save_upserts: vec![],
                metadata_discovery_deletes: vec![],
                virtual_mutation_deletes: vec![],
                checkpoint: ConnectorStateRecord {
                    connector: "notion".to_string(),
                    ..checkpoint(2, "{}")
                },
            },
        ),
        (
            "duplicate metadata job deletes",
            DiscoveryCommit {
                mount_id: mount_id(MAIN_MOUNT),
                entity_upserts: vec![entity("issue-1", "Changed/page.md", "Changed")],
                entity_deletes: vec![],
                observation_upserts: vec![],
                freshness_upserts: vec![],
                auto_save_upserts: vec![],
                metadata_discovery_deletes: vec!["job:1".to_string(), "job:1".to_string()],
                virtual_mutation_deletes: vec![],
                checkpoint: checkpoint(2, "{}"),
            },
        ),
    ];

    for (label, commit) in cases {
        assert!(
            matches!(
                store.commit_discovery(commit),
                Err(StoreError::InvalidState(_))
            ),
            "{label}"
        );
        assert_eq!(
            store
                .get_entity(&mount_id(MAIN_MOUNT), &RemoteId::new("issue-1"))
                .expect("load unchanged entity"),
            Some(original.clone()),
            "{label}"
        );
        assert_eq!(
            store
                .get_connector_state("linear", "mount", MAIN_MOUNT)
                .expect("load unchanged checkpoint"),
            Some(old_checkpoint.clone()),
            "{label}"
        );
    }
}

fn seed_owned_state<S>(store: &mut S, mount: &str, remote_id: &str, path: &str, title: &str)
where
    S: EntityRepository
        + RemoteObservationRepository
        + FreshnessStateRepository
        + HydrationJobRepository
        + AutoSaveRepository
        + ShadowRepository
        + MetadataDiscoveryJobRepository
        + VirtualMutationRepository,
{
    store
        .save_entity(EntityRecord::new(
            mount_id(mount),
            RemoteId::new(remote_id),
            EntityKind::Page,
            title,
            path,
        ))
        .expect("save entity");
    store
        .save_remote_observation(RemoteObservationRecord::new(
            mount_id(mount),
            RemoteId::new(remote_id),
            EntityKind::Page,
            title,
            path,
            "observed",
        ))
        .expect("save observation");
    store
        .save_freshness_state(FreshnessStateRecord::new(
            mount_id(mount),
            RemoteId::new(remote_id),
            FreshnessTier::Warm,
        ))
        .expect("save freshness");
    store
        .upsert_hydration_job(HydrationJobRecord {
            mount_id: mount_id(mount),
            remote_id: RemoteId::new(remote_id),
            path: PathBuf::from(path),
            target_state: HydrationState::Hydrated,
            reason: HydrationReason::Policy,
            attempts: 2,
            last_error: Some("preserve unrelated".to_string()),
        })
        .expect("save hydration");
    let mut enrollment = AutoSaveEnrollmentRecord::new(
        mount_id(mount),
        path,
        AutoSaveOrigin::UserEnabled,
        "created",
    );
    enrollment.remote_id = Some(RemoteId::new(remote_id));
    store
        .save_auto_save_enrollment(enrollment)
        .expect("save enrollment");
    store
        .save_shadow(&mount_id(mount), shadow(remote_id, "body"))
        .expect("save shadow");
    store
        .upsert_metadata_discovery_job(MetadataDiscoveryJobRecord {
            mount_id: mount_id(mount),
            container_identifier: format!("job:{remote_id}"),
            priority: MetadataDiscoveryPriority::Background,
            depth: 1,
            attempts: 2,
            last_error: Some("preserve unrelated".to_string()),
            created_at: "created".to_string(),
            updated_at: "updated".to_string(),
        })
        .expect("save metadata job");
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: mount_id(mount),
            local_id: format!("mutation:{remote_id}"),
            mutation_kind: VirtualMutationKind::Move,
            target_remote_id: Some(RemoteId::new(remote_id)),
            parent_remote_id: None,
            original_path: Some(PathBuf::from(path)),
            projected_path: PathBuf::from(format!("Moved/{remote_id}/page.md")),
            title: title.to_string(),
            content_path: None,
            created_at: "created".to_string(),
            updated_at: "updated".to_string(),
        })
        .expect("save virtual mutation");
}

fn assert_owned_state_absent<S>(store: &S, mount: &str, remote_id: &str, path: &str)
where
    S: EntityRepository
        + RemoteObservationRepository
        + FreshnessStateRepository
        + HydrationJobRepository
        + AutoSaveRepository
        + ShadowRepository,
{
    let mount_id = mount_id(mount);
    let remote_id = RemoteId::new(remote_id);
    assert!(
        store
            .get_entity(&mount_id, &remote_id)
            .expect("entity")
            .is_none()
    );
    assert!(
        store
            .get_remote_observation(&mount_id, &remote_id)
            .expect("observation")
            .is_none()
    );
    assert!(
        store
            .get_freshness_state(&mount_id, &remote_id)
            .expect("freshness")
            .is_none()
    );
    assert!(
        store
            .list_hydration_jobs()
            .expect("hydration")
            .into_iter()
            .all(|job| job.mount_id != mount_id || job.remote_id != remote_id)
    );
    assert!(
        store
            .get_auto_save_enrollment(&mount_id, Path::new(path))
            .expect("auto-save")
            .is_none()
    );
    assert!(matches!(
        store.load_shadow(&mount_id, &remote_id),
        Err(StoreError::ShadowMissing { .. })
    ));
}

fn assert_owned_state_present<S>(store: &S, mount: &str, remote_id: &str, path: &str)
where
    S: EntityRepository
        + RemoteObservationRepository
        + FreshnessStateRepository
        + HydrationJobRepository
        + AutoSaveRepository
        + ShadowRepository,
{
    let mount_id = mount_id(mount);
    let remote_id = RemoteId::new(remote_id);
    assert!(
        store
            .get_entity(&mount_id, &remote_id)
            .expect("entity")
            .is_some()
    );
    assert!(
        store
            .get_remote_observation(&mount_id, &remote_id)
            .expect("observation")
            .is_some()
    );
    assert!(
        store
            .get_freshness_state(&mount_id, &remote_id)
            .expect("freshness")
            .is_some()
    );
    assert!(
        store
            .list_hydration_jobs()
            .expect("hydration")
            .into_iter()
            .any(|job| job.mount_id == mount_id && job.remote_id == remote_id)
    );
    assert!(
        store
            .get_auto_save_enrollment(&mount_id, Path::new(path))
            .expect("auto-save")
            .is_some()
    );
    store
        .load_shadow(&mount_id, &remote_id)
        .expect("shadow present");
}

fn seed_mount<S: MountRepository>(store: &mut S, mount: &str) {
    store
        .save_mount(MountConfig::new(
            mount_id(mount),
            "linear",
            format!("/tmp/{mount}"),
        ))
        .expect("save mount");
}

fn mount_id(value: &str) -> MountId {
    MountId::new(value)
}

fn entity(remote_id: &str, path: &str, title: &str) -> EntityRecord {
    EntityRecord::new(
        mount_id(MAIN_MOUNT),
        RemoteId::new(remote_id),
        EntityKind::Page,
        title,
        path,
    )
    .with_hydration(HydrationState::Stub)
    .with_content_hash(format!("hash:{remote_id}"))
    .with_synced_tree_remote_version(format!("synced:{remote_id}"))
}

fn observation(remote_id: &str, path: &str, version: &str) -> RemoteObservationRecord {
    RemoteObservationRecord::new(
        mount_id(MAIN_MOUNT),
        RemoteId::new(remote_id),
        EntityKind::Page,
        format!("Observed {remote_id}"),
        path,
        "2026-07-15T00:00:00Z",
    )
    .with_remote_version(RemoteVersion::new(version))
    .with_raw_metadata_json(format!(r#"{{"id":"{remote_id}"}}"#))
}

fn freshness(remote_id: &str, checked_at: &str) -> FreshnessStateRecord {
    FreshnessStateRecord::new(
        mount_id(MAIN_MOUNT),
        RemoteId::new(remote_id),
        FreshnessTier::Hot,
    )
    .checked_at(checked_at)
    .next_check_at("2026-07-15T02:00:00Z")
    .remote_hint_pending(false)
}

fn checkpoint(state_version: i64, state_json: &str) -> ConnectorStateRecord {
    ConnectorStateRecord {
        connector: "linear".to_string(),
        scope_kind: "mount".to_string(),
        scope_id: MAIN_MOUNT.to_string(),
        state_version,
        min_reader_version: 1,
        state_json: state_json.to_string(),
        updated_at: format!("version:{state_version}"),
    }
}

fn hydration_job(remote_id: &str, path: &str) -> HydrationJobRecord {
    HydrationJobRecord {
        mount_id: mount_id(MAIN_MOUNT),
        remote_id: RemoteId::new(remote_id),
        path: PathBuf::from(path),
        target_state: HydrationState::Hydrated,
        reason: HydrationReason::Policy,
        attempts: 0,
        last_error: None,
    }
}

fn paused_auto_save(remote_id: &str, path: &str) -> AutoSaveEnrollmentRecord {
    let mut enrollment = AutoSaveEnrollmentRecord::new(
        mount_id(MAIN_MOUNT),
        path,
        AutoSaveOrigin::UserEnabled,
        "created",
    )
    .paused_remote_changed("remote discovery held", "updated");
    enrollment.remote_id = Some(RemoteId::new(remote_id));
    enrollment
}

fn pending_mutation(local_id: &str, remote_id: &str, path: &str) -> VirtualMutationRecord {
    VirtualMutationRecord {
        mount_id: mount_id(MAIN_MOUNT),
        local_id: local_id.to_string(),
        mutation_kind: VirtualMutationKind::Move,
        target_remote_id: Some(RemoteId::new(remote_id)),
        parent_remote_id: None,
        original_path: Some(PathBuf::from(path)),
        projected_path: PathBuf::from(path),
        title: local_id.to_string(),
        content_path: None,
        created_at: "created".to_string(),
        updated_at: "updated".to_string(),
    }
}

fn shadow(remote_id: &str, body: &str) -> ShadowDocument {
    ShadowDocument::from_synced_body(
        RemoteId::new(remote_id),
        body,
        0,
        [RemoteId::new(format!("block:{remote_id}"))],
    )
    .expect("shadow")
}

struct SqliteFixture {
    state_root: PathBuf,
}

impl SqliteFixture {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        Self {
            state_root: std::env::temp_dir().join(format!(
                "locality-store-discovery-{}-{unique}-{suffix}",
                std::process::id()
            )),
        }
    }

    fn open(&self) -> SqliteStateStore {
        SqliteStateStore::open(self.state_root.clone()).expect("open sqlite store")
    }
}

impl Drop for SqliteFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.state_root);
    }
}
