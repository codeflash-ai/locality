use std::path::{Path, PathBuf};

use locality_core::freshness::{FreshnessTier, RemoteVersion};
use locality_core::journal::{
    JournalApplyEffect, JournalEntry, JournalStatus, JournalStore, PushId, PushOperationId,
};
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use locality_core::planner::{PushOperation, PushPlan};
use locality_core::shadow::ShadowDocument;
use locality_store::{
    AutoSaveEnrollmentRecord, AutoSaveOrigin, AutoSaveRepository, AutoSaveState, ConnectionId,
    EntityRecord, EntityRepository, FreshnessStateRecord, FreshnessStateRepository,
    InMemoryStateStore, JournalRepository, MountConfig, MountRepository, RemoteObservationRecord,
    RemoteObservationRepository, ShadowRepository, StoreError, VirtualMutationKind,
    VirtualMutationRecord, VirtualMutationRepository,
};

#[test]
fn mount_config_round_trips_with_read_only_flag() {
    let mut store = InMemoryStateStore::new();
    let mount = MountConfig::new(mount_id(), "notion", "/Users/saurabh/loc/notion")
        .with_remote_root_id(RemoteId::new("root-page"))
        .read_only(true);

    store.save_mount(mount.clone()).expect("save mount");

    assert_eq!(
        store.get_mount(&mount_id()).expect("get mount"),
        Some(mount)
    );
    assert_eq!(store.load_mounts().expect("load mounts").len(), 1);
}

#[test]
fn remounting_same_mount_id_to_different_connection_clears_source_scoped_state() {
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(mount_id(), "notion", "/tmp/loc/notion")
                .with_connection_id(ConnectionId::new("old-workspace")),
        )
        .expect("save original mount");
    seed_source_scoped_state(&mut store);

    store
        .save_mount(
            MountConfig::new(mount_id(), "notion", "/tmp/loc/notion")
                .with_connection_id(ConnectionId::new("new-workspace")),
        )
        .expect("remount with new connection");

    assert_eq!(
        store
            .get_mount(&mount_id())
            .expect("get mount")
            .expect("mount")
            .connection_id,
        Some(ConnectionId::new("new-workspace"))
    );
    assert!(
        store
            .list_entities(&mount_id())
            .expect("list entities")
            .is_empty()
    );
    assert!(
        store
            .list_remote_observations(&mount_id())
            .expect("list observations")
            .is_empty()
    );
    assert!(
        store
            .list_virtual_mutations(&mount_id())
            .expect("list mutations")
            .is_empty()
    );
    assert!(
        store
            .list_auto_save_enrollments(&mount_id())
            .expect("list auto-save")
            .is_empty()
    );
    assert!(
        store
            .list_freshness_states(&mount_id())
            .expect("list freshness")
            .is_empty()
    );
    assert!(store.list_journal().expect("list journal").is_empty());
    assert!(matches!(
        store.load_shadow(&mount_id(), &RemoteId::new("page-1")),
        Err(StoreError::ShadowMissing { .. })
    ));
}

#[test]
fn remounting_same_mount_id_to_different_remote_root_clears_source_scoped_state() {
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(mount_id(), "notion", "/tmp/loc/notion")
                .with_connection_id(ConnectionId::new("workspace"))
                .with_remote_root_id(RemoteId::new("old-root")),
        )
        .expect("save original mount");
    seed_source_scoped_state(&mut store);

    store
        .save_mount(
            MountConfig::new(mount_id(), "notion", "/tmp/loc/notion")
                .with_connection_id(ConnectionId::new("workspace"))
                .with_remote_root_id(RemoteId::new("new-root")),
        )
        .expect("remount with new root");

    assert_eq!(
        store
            .get_mount(&mount_id())
            .expect("get mount")
            .expect("mount")
            .remote_root_id,
        Some(RemoteId::new("new-root"))
    );
    assert!(
        store
            .list_entities(&mount_id())
            .expect("list entities")
            .is_empty()
    );
}

#[test]
fn remounting_same_source_keeps_source_scoped_state() {
    let mut store = InMemoryStateStore::new();
    let mount = MountConfig::new(mount_id(), "notion", "/tmp/loc/notion")
        .with_connection_id(ConnectionId::new("workspace"));
    store.save_mount(mount.clone()).expect("save mount");
    seed_source_scoped_state(&mut store);

    store.save_mount(mount).expect("remount same source");

    assert_eq!(
        store
            .list_entities(&mount_id())
            .expect("list entities")
            .len(),
        1
    );
    assert_eq!(store.list_journal().expect("list journal").len(), 1);
    assert!(
        store
            .load_shadow(&mount_id(), &RemoteId::new("page-1"))
            .is_ok()
    );
}

#[test]
fn entity_records_can_be_looked_up_by_id_or_path() {
    let mut store = InMemoryStateStore::new();
    let entity = entity_record("page-1", "Roadmap.md")
        .with_hydration(HydrationState::Hydrated)
        .with_content_hash("body-hash")
        .with_remote_edited_at("2026-06-10T00:00:00Z");

    store.save_entity(entity.clone()).expect("save entity");

    assert_eq!(
        store
            .get_entity(&mount_id(), &RemoteId::new("page-1"))
            .expect("get entity"),
        Some(entity.clone())
    );
    assert_eq!(
        store
            .find_entity_by_path(&mount_id(), Path::new("Roadmap.md"))
            .expect("find entity"),
        Some(entity)
    );
}

#[test]
fn updating_entity_path_replaces_the_old_path_index() {
    let mut store = InMemoryStateStore::new();
    store
        .save_entity(entity_record("page-1", "Roadmap.md"))
        .expect("save entity");

    store
        .save_entity(entity_record("page-1", "Roadmap 2026.md"))
        .expect("update entity path");

    assert!(
        store
            .find_entity_by_path(&mount_id(), Path::new("Roadmap.md"))
            .expect("old path lookup")
            .is_none()
    );
    assert_eq!(
        store
            .find_entity_by_path(&mount_id(), Path::new("Roadmap 2026.md"))
            .expect("new path lookup")
            .unwrap()
            .remote_id,
        RemoteId::new("page-1")
    );
}

#[test]
fn duplicate_entity_path_in_same_mount_is_rejected() {
    let mut store = InMemoryStateStore::new();
    store
        .save_entity(entity_record("page-1", "Roadmap.md"))
        .expect("save first entity");

    let error = store
        .save_entity(entity_record("page-2", "Roadmap.md"))
        .expect_err("duplicate path");

    assert_eq!(
        error,
        StoreError::DuplicateEntityPath {
            mount_id: mount_id(),
            path: "Roadmap.md".into(),
        }
    );
}

#[test]
fn entity_lists_are_ordered_by_projected_path() {
    let mut store = InMemoryStateStore::new();
    store
        .save_entity(entity_record("page-a", "Zebra.md"))
        .expect("save first entity");
    store
        .save_entity(entity_record("page-b", "Alpha.md"))
        .expect("save second entity");

    let paths = store
        .list_entities(&mount_id())
        .expect("list entities")
        .into_iter()
        .map(|entity| entity.path)
        .collect::<Vec<_>>();

    assert_eq!(
        paths,
        vec![PathBuf::from("Alpha.md"), PathBuf::from("Zebra.md")]
    );
}

#[test]
fn auto_save_enrollments_round_trip_by_path_and_remote_id() {
    let mut store = InMemoryStateStore::new();
    let mut enrollment = AutoSaveEnrollmentRecord::new(
        mount_id(),
        "Draft/page.md",
        AutoSaveOrigin::LocalityCreated,
        "1",
    );
    enrollment.remote_id = Some(RemoteId::new("page-2"));
    enrollment.state = AutoSaveState::Blocked;
    enrollment.last_reason = Some("deletions require review".to_string());

    store
        .save_auto_save_enrollment(enrollment.clone())
        .expect("save enrollment");

    assert_eq!(
        store
            .get_auto_save_enrollment(&mount_id(), Path::new("Draft/page.md"))
            .expect("get enrollment"),
        Some(enrollment.clone())
    );
    assert_eq!(
        store
            .find_auto_save_enrollment_by_remote_id(&mount_id(), &RemoteId::new("page-2"))
            .expect("find enrollment"),
        Some(enrollment)
    );
}

#[test]
fn shadow_document_round_trips_through_snapshot_record() {
    let mut store = InMemoryStateStore::new();
    let shadow = shadow_document();

    store
        .save_shadow(&mount_id(), shadow.clone())
        .expect("save shadow");

    assert_eq!(
        store
            .load_shadow(&mount_id(), &RemoteId::new("page-1"))
            .expect("load shadow"),
        shadow
    );
    assert_eq!(
        store
            .get_shadow_record(&mount_id(), &RemoteId::new("page-1"))
            .expect("shadow record")
            .unwrap()
            .blocks
            .len(),
        2
    );
}

#[test]
fn remote_observations_track_latest_seen_remote_metadata() {
    let mut store = InMemoryStateStore::new();
    let observation = RemoteObservationRecord::new(
        mount_id(),
        RemoteId::new("page-1"),
        EntityKind::Page,
        "Roadmap",
        "Roadmap.md",
        "2026-06-15T00:00:00Z",
    )
    .with_parent(RemoteId::new("root"))
    .with_remote_version(RemoteVersion::new("remote-v1"))
    .with_raw_metadata_json("{\"source\":\"test\"}");

    store
        .save_remote_observation(observation.clone())
        .expect("save observation");

    assert_eq!(
        store
            .get_remote_observation(&mount_id(), &RemoteId::new("page-1"))
            .expect("get observation"),
        Some(observation.clone())
    );
    assert_eq!(
        store
            .list_remote_observations(&mount_id())
            .expect("list observations"),
        vec![observation]
    );
}

#[test]
fn remote_observations_are_ordered_by_projected_path() {
    let mut store = InMemoryStateStore::new();
    store
        .save_remote_observation(remote_observation("page-a", "Zebra.md"))
        .expect("save first observation");
    store
        .save_remote_observation(remote_observation("page-b", "Alpha.md"))
        .expect("save second observation");

    let paths = store
        .list_remote_observations(&mount_id())
        .expect("list observations")
        .into_iter()
        .map(|observation| observation.projected_path)
        .collect::<Vec<_>>();

    assert_eq!(
        paths,
        vec![PathBuf::from("Alpha.md"), PathBuf::from("Zebra.md")]
    );
}

#[test]
fn virtual_mutations_are_ordered_by_projected_path() {
    let mut store = InMemoryStateStore::new();
    store
        .save_virtual_mutation(virtual_mutation("local:a", "Zebra.md"))
        .expect("save first mutation");
    store
        .save_virtual_mutation(virtual_mutation("local:z", "Alpha.md"))
        .expect("save second mutation");

    let paths = store
        .list_virtual_mutations(&mount_id())
        .expect("list mutations")
        .into_iter()
        .map(|mutation| mutation.projected_path)
        .collect::<Vec<_>>();

    assert_eq!(
        paths,
        vec![PathBuf::from("Alpha.md"), PathBuf::from("Zebra.md")]
    );
}

#[test]
fn freshness_state_tracks_scheduling_priority_and_hints() {
    let mut store = InMemoryStateStore::new();
    let state = FreshnessStateRecord::new(mount_id(), RemoteId::new("page-1"), FreshnessTier::Hot)
        .checked_at("2026-06-15T00:00:00Z")
        .next_check_at("2026-06-15T00:01:00Z")
        .opened_at("2026-06-15T00:00:05Z")
        .local_change_at("2026-06-15T00:00:10Z")
        .remote_hint_pending(true);

    store
        .save_freshness_state(state.clone())
        .expect("save freshness");

    assert_eq!(
        store
            .get_freshness_state(&mount_id(), &RemoteId::new("page-1"))
            .expect("get freshness"),
        Some(state.clone())
    );
    assert_eq!(
        store
            .list_freshness_states(&mount_id())
            .expect("list freshness"),
        vec![state]
    );
}

#[test]
fn missing_shadow_returns_structured_error() {
    let store = InMemoryStateStore::new();

    let error = store
        .load_shadow(&mount_id(), &RemoteId::new("missing-page"))
        .expect_err("missing shadow");

    assert_eq!(
        error,
        StoreError::ShadowMissing {
            mount_id: mount_id(),
            entity_id: RemoteId::new("missing-page"),
        }
    );
}

#[test]
fn journal_repository_tracks_status_updates() {
    let mut store = InMemoryStateStore::new();
    let entry = journal_entry("push-1", JournalStatus::Prepared);

    store.append_journal(entry.clone()).expect("append journal");
    store
        .update_journal_status(&PushId("push-1".to_string()), JournalStatus::Applied)
        .expect("update journal");
    store
        .record_journal_apply_effects(&PushId("push-1".to_string()), apply_effects())
        .expect("record effects");

    let entry = store
        .get_journal(&PushId("push-1".to_string()))
        .expect("get journal")
        .unwrap();
    assert_eq!(entry.status, JournalStatus::Applied);
    assert_eq!(entry.apply_effects, apply_effects());
    assert_eq!(store.list_journal().expect("list journal").len(), 1);
}

#[test]
fn in_memory_store_satisfies_core_journal_store_contract() {
    let mut store = InMemoryStateStore::new();
    let entry = journal_entry("push-2", JournalStatus::Prepared);

    JournalStore::append(&mut store, entry).expect("core append");
    JournalStore::update_status(
        &mut store,
        &PushId("push-2".to_string()),
        JournalStatus::Reconciled,
    )
    .expect("core update");
    JournalStore::record_apply_effects(&mut store, &PushId("push-2".to_string()), apply_effects())
        .expect("core effects");

    let entry = store
        .get_journal(&PushId("push-2".to_string()))
        .expect("get journal")
        .unwrap();
    assert_eq!(entry.status, JournalStatus::Reconciled);
    assert_eq!(entry.apply_effects, apply_effects());
}

fn mount_id() -> MountId {
    MountId::new("notion-main")
}

fn entity_record(remote_id: &str, path: &str) -> EntityRecord {
    EntityRecord::new(
        mount_id(),
        RemoteId::new(remote_id),
        EntityKind::Page,
        "Roadmap",
        path,
    )
}

fn remote_observation(remote_id: &str, path: &str) -> RemoteObservationRecord {
    RemoteObservationRecord::new(
        mount_id(),
        RemoteId::new(remote_id),
        EntityKind::Page,
        "Roadmap",
        path,
        "2026-06-15T00:00:00Z",
    )
}

fn virtual_mutation(local_id: &str, path: &str) -> VirtualMutationRecord {
    VirtualMutationRecord {
        mount_id: mount_id(),
        local_id: local_id.to_string(),
        mutation_kind: VirtualMutationKind::Create,
        target_remote_id: None,
        parent_remote_id: None,
        original_path: None,
        projected_path: PathBuf::from(path),
        title: "Roadmap".to_string(),
        content_path: None,
        created_at: "2026-06-15T00:00:00Z".to_string(),
        updated_at: "2026-06-15T00:00:00Z".to_string(),
    }
}

fn shadow_document() -> ShadowDocument {
    ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        "# Roadmap\n\nSame paragraph.",
        9,
        [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
    )
    .expect("shadow")
}

fn journal_entry(push_id: &str, status: JournalStatus) -> JournalEntry {
    JournalEntry::new(
        PushId(push_id.to_string()),
        mount_id(),
        vec![RemoteId::new("page-1")],
        PushPlan::new(
            vec![RemoteId::new("page-1")],
            vec![PushOperation::UpdateBlock {
                block_id: RemoteId::new("paragraph-1"),
                content: "Updated paragraph.".to_string(),
            }],
        ),
        status,
    )
}

fn seed_source_scoped_state(store: &mut InMemoryStateStore) {
    store
        .save_entity(entity_record("page-1", "Roadmap.md"))
        .expect("save entity");
    store
        .save_shadow(&mount_id(), shadow_document())
        .expect("save shadow");
    store
        .save_remote_observation(remote_observation("page-1", "Roadmap.md"))
        .expect("save observation");
    store
        .save_virtual_mutation(virtual_mutation("local:1", "Draft.md"))
        .expect("save mutation");
    store
        .save_auto_save_enrollment(AutoSaveEnrollmentRecord::new(
            mount_id(),
            "Draft.md",
            AutoSaveOrigin::LocalityCreated,
            "2026-06-15T00:00:00Z",
        ))
        .expect("save auto-save");
    store
        .save_freshness_state(FreshnessStateRecord::new(
            mount_id(),
            RemoteId::new("page-1"),
            FreshnessTier::Hot,
        ))
        .expect("save freshness");
    store
        .append_journal(journal_entry("push-1", JournalStatus::Prepared))
        .expect("append journal");
}

fn apply_effects() -> Vec<JournalApplyEffect> {
    vec![JournalApplyEffect::UpdatedBlock {
        operation_id: PushOperationId("push-1:0:update_block:paragraph-1".to_string()),
        operation_index: 0,
        block_id: RemoteId::new("paragraph-1"),
    }]
}
