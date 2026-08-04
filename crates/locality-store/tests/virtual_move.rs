use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use locality_core::freshness::FreshnessTier;
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use locality_store::{
    EntityRecord, EntityRepository, FreshnessStateRecord, FreshnessStateRepository,
    InMemoryStateStore, MountConfig, MountRepository, ProjectionMode, SqliteStateStore, StoreError,
    VirtualMoveRepository, VirtualMoveTransition, VirtualMutationKind, VirtualMutationRecord,
    VirtualMutationRepository,
};

#[test]
fn virtual_move_transition_is_atomic_in_memory() {
    exercise_virtual_move_transition(&mut InMemoryStateStore::new());
}

#[test]
fn virtual_move_transition_is_atomic_in_sqlite() {
    let root = temp_root("loc-store-virtual-move");
    let mut store = SqliteStateStore::open(root.clone()).expect("open store");
    exercise_virtual_move_transition(&mut store);
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn sqlite_virtual_move_pointer_round_trips_non_utf8_across_restarts() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let root = temp_root("loc-store-virtual-move-non-utf8");
    let mount_id = MountId::new("linear-non-utf8");
    let old_path = root.join(OsString::from_vec(b"old-\xff.md".to_vec()));
    let new_path = root.join(OsString::from_vec(b"new-\xfe.md".to_vec()));
    let mutation = VirtualMutationRecord {
        mount_id: mount_id.clone(),
        local_id: "move:non-utf8".to_string(),
        mutation_kind: VirtualMutationKind::Move,
        target_remote_id: Some(RemoteId::new("issue-non-utf8")),
        parent_remote_id: None,
        original_path: Some(PathBuf::from("old.md")),
        projected_path: PathBuf::from("new.md"),
        title: "Non UTF-8".to_string(),
        content_path: Some(old_path.clone()),
        created_at: "1".to_string(),
        updated_at: "1".to_string(),
    };

    let mut store = SqliteStateStore::open(root.clone()).expect("open store");
    store
        .save_mount(MountConfig::new(
            mount_id.clone(),
            "linear",
            root.join("mount"),
        ))
        .expect("save mount");
    store
        .save_virtual_mutation(mutation)
        .expect("save exact native pointer");
    drop(store);

    let mut restarted = SqliteStateStore::open(root.clone()).expect("restart store");
    assert_eq!(
        restarted
            .get_virtual_mutation(&mount_id, "move:non-utf8")
            .expect("read pointer")
            .expect("mutation")
            .content_path,
        Some(old_path.clone())
    );
    restarted
        .finalize_virtual_move_content(
            &mount_id,
            "move:non-utf8",
            Some(&old_path),
            new_path.clone(),
            "2",
        )
        .expect("finalize exact native pointer");
    drop(restarted);

    let restarted = SqliteStateStore::open(root.clone()).expect("restart finalized store");
    assert_eq!(
        restarted
            .get_virtual_mutation(&mount_id, "move:non-utf8")
            .expect("read finalized pointer")
            .expect("mutation")
            .content_path,
        Some(new_path)
    );
    let _ = std::fs::remove_dir_all(root);
}

fn exercise_virtual_move_transition<S>(store: &mut S)
where
    S: EntityRepository
        + FreshnessStateRepository
        + MountRepository
        + VirtualMutationRepository
        + VirtualMoveRepository,
{
    let mount_id = MountId::new("linear-main");
    store
        .save_mount(
            MountConfig::new(mount_id.clone(), "linear", "/tmp/linear-main")
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    let remote_id = RemoteId::new("issue-1");
    let old_entity = EntityRecord::new(
        mount_id.clone(),
        remote_id.clone(),
        EntityKind::Page,
        "Canonical title",
        "Team A/ENG-1-old/page.md",
    )
    .with_hydration(HydrationState::Hydrated);
    let old_freshness =
        FreshnessStateRecord::new(mount_id.clone(), remote_id.clone(), FreshnessTier::Warm)
            .checked_at("before")
            .local_change_at("older");
    let old_rename = mutation(
        &mount_id,
        "rename:issue-1",
        VirtualMutationKind::Rename,
        Some(remote_id.clone()),
        "Team A/ENG-1-old/page.md",
        "/cache/Team A/ENG-1-old/page.md",
    );
    store.save_entity(old_entity.clone()).expect("save entity");
    store
        .save_freshness_state(old_freshness.clone())
        .expect("save freshness");
    store
        .save_virtual_mutation(old_rename.clone())
        .expect("save old rename");

    let mut moved_entity = old_entity.clone();
    moved_entity.path = PathBuf::from("Team B/ENG-1-new/page.md");
    moved_entity.hydration = HydrationState::Dirty;
    let moved_freshness =
        FreshnessStateRecord::new(mount_id.clone(), remote_id.clone(), FreshnessTier::Hot)
            .checked_at("before")
            .local_change_at("now");
    let moved_mutation = mutation(
        &mount_id,
        "move:issue-1",
        VirtualMutationKind::Move,
        Some(remote_id.clone()),
        "Team B/ENG-1-new/page.md",
        "/cache/Team A/ENG-1-old/page.md",
    );
    store
        .begin_virtual_move(VirtualMoveTransition {
            mutation: moved_mutation.clone(),
            entity: Some(moved_entity.clone()),
            freshness: Some(moved_freshness.clone()),
            superseded_local_ids: vec!["move:issue-1".to_string(), "rename:issue-1".to_string()],
        })
        .expect("begin move");

    assert_eq!(
        store.get_entity(&mount_id, &remote_id).expect("entity"),
        Some(moved_entity)
    );
    assert_eq!(
        store
            .get_freshness_state(&mount_id, &remote_id)
            .expect("freshness"),
        Some(moved_freshness)
    );
    assert_eq!(
        store
            .get_virtual_mutation(&mount_id, "rename:issue-1")
            .expect("old rename"),
        None
    );
    assert_eq!(
        store
            .get_virtual_mutation(&mount_id, "move:issue-1")
            .expect("move"),
        Some(moved_mutation.clone())
    );

    let finalized = store
        .finalize_virtual_move_content(
            &mount_id,
            "move:issue-1",
            moved_mutation.content_path.as_deref(),
            PathBuf::from("/cache/Team B/ENG-1-new/page.md"),
            "later",
        )
        .expect("finalize move");
    assert_eq!(
        finalized.content_path,
        Some(PathBuf::from("/cache/Team B/ENG-1-new/page.md"))
    );
    assert_eq!(finalized.updated_at, "later");
}

#[test]
fn virtual_move_transition_rolls_back_every_record_in_memory() {
    exercise_virtual_move_rollback(&mut InMemoryStateStore::new());
}

#[test]
fn virtual_move_transition_rolls_back_every_record_in_sqlite() {
    let root = temp_root("loc-store-virtual-move-rollback");
    let mut store = SqliteStateStore::open(root.clone()).expect("open store");
    exercise_virtual_move_rollback(&mut store);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn virtual_move_finalize_rejects_stale_content_pointer_in_memory() {
    exercise_virtual_move_finalize_cas(&mut InMemoryStateStore::new());
}

#[test]
fn virtual_move_finalize_rejects_stale_content_pointer_in_sqlite() {
    let root = temp_root("loc-store-virtual-move-cas");
    let mut store = SqliteStateStore::open(root.clone()).expect("open store");
    exercise_virtual_move_finalize_cas(&mut store);
    let _ = std::fs::remove_dir_all(root);
}

fn exercise_virtual_move_finalize_cas<S>(store: &mut S)
where
    S: MountRepository + VirtualMutationRepository + VirtualMoveRepository,
{
    let mount_id = MountId::new("linear-main");
    store
        .save_mount(
            MountConfig::new(mount_id.clone(), "linear", "/tmp/linear-main")
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    let moved_mutation = mutation(
        &mount_id,
        "local:issue-1",
        VirtualMutationKind::Create,
        None,
        "Team B/ENG-1-new.md",
        "/cache/Team A/ENG-1-old.md",
    );
    store
        .begin_virtual_move(VirtualMoveTransition {
            mutation: moved_mutation.clone(),
            entity: None,
            freshness: None,
            superseded_local_ids: Vec::new(),
        })
        .expect("begin move");

    let error = store
        .finalize_virtual_move_content(
            &mount_id,
            "local:issue-1",
            Some(PathBuf::from("/cache/other.md").as_path()),
            PathBuf::from("/cache/Team B/ENG-1-new.md"),
            "later",
        )
        .expect_err("stale pointer is rejected");

    assert!(matches!(error, StoreError::InvalidState(_)));
    assert_eq!(
        store
            .get_virtual_mutation(&mount_id, "local:issue-1")
            .expect("move")
            .expect("move")
            .content_path,
        moved_mutation.content_path
    );
}

fn exercise_virtual_move_rollback<S>(store: &mut S)
where
    S: EntityRepository
        + FreshnessStateRepository
        + MountRepository
        + VirtualMutationRepository
        + VirtualMoveRepository,
{
    let mount_id = MountId::new("linear-main");
    store
        .save_mount(
            MountConfig::new(mount_id.clone(), "linear", "/tmp/linear-main")
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    let remote_id = RemoteId::new("issue-1");
    let old_entity = EntityRecord::new(
        mount_id.clone(),
        remote_id.clone(),
        EntityKind::Page,
        "Original",
        "Team A/ENG-1.md",
    );
    let occupied = EntityRecord::new(
        mount_id.clone(),
        RemoteId::new("issue-2"),
        EntityKind::Page,
        "Occupied",
        "Team B/ENG-1.md",
    );
    let old_freshness =
        FreshnessStateRecord::new(mount_id.clone(), remote_id.clone(), FreshnessTier::Warm)
            .local_change_at("before");
    let old_rename = mutation(
        &mount_id,
        "rename:issue-1",
        VirtualMutationKind::Rename,
        Some(remote_id.clone()),
        "Team A/ENG-1.md",
        "/cache/Team A/ENG-1.md",
    );
    store.save_entity(old_entity.clone()).expect("old entity");
    store.save_entity(occupied).expect("occupied entity");
    store
        .save_freshness_state(old_freshness.clone())
        .expect("freshness");
    store
        .save_virtual_mutation(old_rename.clone())
        .expect("old rename");

    let mut conflicting = old_entity.clone();
    conflicting.path = PathBuf::from("Team B/ENG-1.md");
    let error = store
        .begin_virtual_move(VirtualMoveTransition {
            mutation: mutation(
                &mount_id,
                "move:issue-1",
                VirtualMutationKind::Move,
                Some(remote_id.clone()),
                "Team B/ENG-1.md",
                "/cache/Team A/ENG-1.md",
            ),
            entity: Some(conflicting),
            freshness: Some(
                FreshnessStateRecord::new(mount_id.clone(), remote_id.clone(), FreshnessTier::Hot)
                    .local_change_at("after"),
            ),
            superseded_local_ids: vec!["rename:issue-1".to_string()],
        })
        .expect_err("duplicate path rolls back");

    assert!(matches!(error, StoreError::DuplicateEntityPath { .. }));
    assert_eq!(
        store.get_entity(&mount_id, &remote_id).expect("entity"),
        Some(old_entity)
    );
    assert_eq!(
        store
            .get_freshness_state(&mount_id, &remote_id)
            .expect("freshness"),
        Some(old_freshness)
    );
    assert_eq!(
        store
            .get_virtual_mutation(&mount_id, "rename:issue-1")
            .expect("old rename"),
        Some(old_rename)
    );
    assert_eq!(
        store
            .get_virtual_mutation(&mount_id, "move:issue-1")
            .expect("new move"),
        None
    );
}

fn mutation(
    mount_id: &MountId,
    local_id: &str,
    mutation_kind: VirtualMutationKind,
    target_remote_id: Option<RemoteId>,
    projected_path: &str,
    content_path: &str,
) -> VirtualMutationRecord {
    VirtualMutationRecord {
        mount_id: mount_id.clone(),
        local_id: local_id.to_string(),
        mutation_kind,
        target_remote_id,
        parent_remote_id: Some(RemoteId::new("team-b")),
        original_path: Some(PathBuf::from("Team A/ENG-1.md")),
        projected_path: PathBuf::from(projected_path),
        title: "Canonical title".to_string(),
        content_path: Some(PathBuf::from(content_path)),
        created_at: "created".to_string(),
        updated_at: "updated".to_string(),
    }
}

fn temp_root(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!(
        "{label}-{}-{nanos}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}
