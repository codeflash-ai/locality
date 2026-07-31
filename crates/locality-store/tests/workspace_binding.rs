use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use locality_core::journal::{
    JournalApplyEffect, JournalEntry, JournalStatus, PushId, PushOperationId,
};
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use locality_core::planner::{PushOperation, PushPlan};
use locality_core::shadow::ShadowDocument;
use locality_store::{
    EntityRecord, EntityRepository, JournalRepository, MountConfig, MountRepository,
    ProjectionMode, ShadowRepository, SqliteStateStore, StateCompatibilityIssue,
    StateCompatibilityStatus, StoreError, WorkspaceBindingRepository, WorkspaceRebindBlocker,
};
use rusqlite::{Connection, params};

#[test]
fn legacy_v20_upgrade_preserves_dirty_shadow_apply_journal_and_mount_identity() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    let mount = fixture.mount_config(ProjectionMode::MacosFileProvider);
    let entity = dirty_entity(&fixture.mount_id);
    let shadow = synced_shadow();
    let journal = applying_journal(&fixture.mount_id);
    store.save_mount(mount.clone()).expect("save legacy mount");
    store.save_entity(entity.clone()).expect("save entity");
    store
        .save_shadow(&fixture.mount_id, shadow.clone())
        .expect("save synced shadow");
    store
        .append_journal(journal.clone())
        .expect("save applying journal");
    fs::write(
        fixture.mount_root.join("Roadmap.md"),
        "# Roadmap\n\nLocally edited and still dirty.\n",
    )
    .expect("write dirty projection");
    downgrade_to_v20(&store.db_path);
    drop(store);

    let before = SqliteStateStore::inspect_compatibility(fixture.state_root.clone())
        .expect("inspect legacy state");
    assert_eq!(before.status, StateCompatibilityStatus::Migratable);
    assert_eq!(
        before.issues,
        vec![StateCompatibilityIssue::OlderSchema {
            found: 20,
            current: 21,
        }]
    );

    let reopened = fixture.open();
    let binding = reopened
        .get_workspace_binding(&fixture.mount_id)
        .expect("read migrated binding")
        .expect("binding");
    assert_eq!(binding.mount_target().as_str(), "notion-main");
    assert_eq!(
        reopened
            .get_mount(&fixture.mount_id)
            .expect("read mount")
            .expect("mount"),
        mount
    );
    assert_eq!(
        reopened
            .get_entity(&fixture.mount_id, &RemoteId::new("page-1"))
            .expect("read entity"),
        Some(entity)
    );
    assert_eq!(
        reopened
            .load_shadow(&fixture.mount_id, &RemoteId::new("page-1"))
            .expect("read shadow"),
        shadow
    );
    assert_eq!(
        reopened
            .get_journal(&PushId("push-applying".to_string()))
            .expect("read journal"),
        Some(journal)
    );
    assert_eq!(
        fs::read_to_string(fixture.mount_root.join("Roadmap.md")).expect("read dirty file"),
        "# Roadmap\n\nLocally edited and still dirty.\n"
    );

    let raw = Connection::open(&reopened.db_path).expect("legacy metadata reader");
    let legacy_row: (String, String) = raw
        .query_row(
            "SELECT mount_id, root FROM mounts WHERE mount_id = ?1",
            params![fixture.mount_id.0.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("old mounts reader remains valid");
    assert_eq!(legacy_row.0, fixture.mount_id.0);
    assert_eq!(legacy_row.1, fixture.mount_root.to_string_lossy());
    let binding_json: String = raw
        .query_row(
            "SELECT binding_json FROM workspace_bindings WHERE mount_id = ?1",
            params![fixture.mount_id.0.as_str()],
            |row| row.get(0),
        )
        .expect("binding json");
    assert_eq!(
        binding_json,
        r#"{"binding_version":1,"layout_version":1,"mount_target":"notion-main"}"#
    );
    assert!(!binding_json.contains(fixture.root.to_string_lossy().as_ref()));
}

#[test]
fn migrated_binding_survives_restart_and_resolves_host_roots_without_mutating_mount() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    let mount = fixture.mount_config(ProjectionMode::LinuxFuse);
    let entity = dirty_entity(&fixture.mount_id);
    let shadow = synced_shadow();
    let journal = applying_journal(&fixture.mount_id);
    store.save_mount(mount).expect("save mount");
    store.save_entity(entity.clone()).expect("save entity");
    store
        .save_shadow(&fixture.mount_id, shadow.clone())
        .expect("save shadow");
    store.append_journal(journal.clone()).expect("save journal");

    let binding = store
        .get_workspace_binding(&fixture.mount_id)
        .expect("binding")
        .expect("binding exists");
    let logical = locality_core::portable::LogicalPath::new("Engineering/Roadmap/page.md")
        .expect("logical path");
    let mac_root = Path::new("/Users/alice/Library/CloudStorage/Locality");
    assert_eq!(
        binding.projected_path(mac_root, &logical),
        mac_root.join("notion-main/Engineering/Roadmap/page.md")
    );

    let linux_root = Path::new("/home/alice/Locality");
    assert_eq!(
        binding.projected_path(linux_root, &logical),
        linux_root.join("notion-main/Engineering/Roadmap/page.md")
    );
    assert_eq!(
        store
            .get_mount(&fixture.mount_id)
            .expect("mount")
            .expect("mount exists")
            .root,
        fixture.mount_root
    );
    drop(store);

    let restarted = fixture.open();
    assert_eq!(
        restarted
            .get_workspace_binding(&fixture.mount_id)
            .expect("binding after restart")
            .expect("binding")
            .mount_target()
            .as_str(),
        "notion-main"
    );
    assert_eq!(
        restarted
            .get_mount(&fixture.mount_id)
            .expect("mount after restart")
            .expect("mount")
            .root,
        fixture.mount_root
    );
    assert_eq!(
        restarted
            .get_entity(&fixture.mount_id, &RemoteId::new("page-1"))
            .expect("entity after rebind"),
        Some(entity)
    );
    assert_eq!(
        restarted
            .load_shadow(&fixture.mount_id, &RemoteId::new("page-1"))
            .expect("shadow after rebind"),
        shadow
    );
    assert_eq!(
        restarted
            .get_journal(&PushId("push-applying".to_string()))
            .expect("journal after rebind"),
        Some(journal)
    );
}

#[test]
fn workspace_rebind_preflight_rejects_dirty_state_without_changing_root() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    let mount = fixture.mount_config(ProjectionMode::PlainFiles);
    store.save_mount(mount.clone()).expect("save mount");
    store
        .save_entity(dirty_entity(&fixture.mount_id).with_hydration(HydrationState::Dirty))
        .expect("save dirty entity");

    assert_eq!(
        store.check_workspace_rebind(&fixture.mount_id),
        Err(StoreError::WorkspaceRebindBlocked {
            mount_id: fixture.mount_id.clone(),
            blocker: WorkspaceRebindBlocker::DirtyOrConflictedState,
        })
    );
    assert_eq!(
        store
            .get_mount(&fixture.mount_id)
            .expect("mount")
            .expect("mount exists")
            .root,
        mount.root
    );
}

#[test]
fn workspace_rebind_preflight_rejects_unsettled_apply_journal() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    store
        .save_mount(fixture.mount_config(ProjectionMode::PlainFiles))
        .expect("save mount");
    store
        .append_journal(applying_journal(&fixture.mount_id))
        .expect("save applying journal");

    assert_eq!(
        store.check_workspace_rebind(&fixture.mount_id),
        Err(StoreError::WorkspaceRebindBlocked {
            mount_id: fixture.mount_id.clone(),
            blocker: WorkspaceRebindBlocker::UnsettledApplyJournal,
        })
    );
}

#[test]
fn workspace_rebind_preflight_rejects_active_virtual_projection() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    store
        .save_mount(fixture.mount_config(ProjectionMode::MacosFileProvider))
        .expect("save mount");

    assert_eq!(
        store.check_workspace_rebind(&fixture.mount_id),
        Err(StoreError::WorkspaceRebindBlocked {
            mount_id: fixture.mount_id.clone(),
            blocker: WorkspaceRebindBlocker::ActiveProjection,
        })
    );
}

#[test]
fn clean_plain_mount_still_requires_an_owning_rebind_coordinator() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    store
        .save_mount(fixture.mount_config(ProjectionMode::PlainFiles))
        .expect("save mount");

    assert_eq!(
        store.check_workspace_rebind(&fixture.mount_id),
        Err(StoreError::WorkspaceRebindBlocked {
            mount_id: fixture.mount_id.clone(),
            blocker: WorkspaceRebindBlocker::RequiresOwningCoordinator,
        })
    );
}

#[test]
fn v20_linux_fuse_binding_uses_post_migration_mount_point() {
    assert_legacy_virtual_binding_uses_final_mount_point(ProjectionMode::LinuxFuse);
}

#[test]
fn v20_windows_cloud_files_binding_uses_post_migration_mount_point() {
    assert_legacy_virtual_binding_uses_final_mount_point(ProjectionMode::WindowsCloudFiles);
}

fn assert_legacy_virtual_binding_uses_final_mount_point(projection: ProjectionMode) {
    let fixture = Fixture::new();
    let shared_root = fixture.root.join("LegacyLocality");
    fs::create_dir_all(&shared_root).expect("shared root");
    let mut store = fixture.open();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", shared_root.clone())
                .projection(projection.clone()),
        )
        .expect("save legacy projection mount");
    downgrade_to_v20(&store.db_path);
    mark_projection_component_v1(&store.db_path, &projection);
    drop(store);

    let migrated = fixture.open();
    let mount = migrated
        .get_mount(&fixture.mount_id)
        .expect("mount")
        .expect("mount exists");
    let binding = migrated
        .get_workspace_binding(&fixture.mount_id)
        .expect("binding")
        .expect("binding exists");
    assert_eq!(mount.root, shared_root.join("notion"));
    assert_eq!(binding.mount_target().as_str(), "notion");
}

#[test]
fn current_v21_open_rejects_missing_binding_without_reconstructing_it() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    store
        .save_mount(fixture.mount_config(ProjectionMode::PlainFiles))
        .expect("save mount");
    let connection = Connection::open(&store.db_path).expect("raw connection");
    connection
        .execute(
            "DELETE FROM workspace_bindings WHERE mount_id = ?1",
            params![fixture.mount_id.0.as_str()],
        )
        .expect("remove required binding");
    drop(connection);
    drop(store);

    assert!(matches!(
        SqliteStateStore::open(fixture.state_root.clone()),
        Err(StoreError::StateCompatibility(message))
            if message.contains("missing required non-rebuildable workspace binding")
    ));
    let connection =
        Connection::open(fixture.state_root.join("state.sqlite3")).expect("raw failed-open state");
    let binding_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM workspace_bindings", [], |row| {
            row.get(0)
        })
        .expect("binding count");
    assert_eq!(binding_count, 0, "current state must not be reconstructed");
}

#[test]
fn current_v21_save_of_existing_mount_does_not_reconstruct_missing_binding() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    let mount = fixture.mount_config(ProjectionMode::PlainFiles);
    store.save_mount(mount.clone()).expect("save mount");
    let connection = Connection::open(&store.db_path).expect("raw connection");
    connection
        .execute(
            "DELETE FROM workspace_bindings WHERE mount_id = ?1",
            params![fixture.mount_id.0.as_str()],
        )
        .expect("remove required binding");
    drop(connection);

    assert_eq!(
        store.save_mount(mount),
        Err(StoreError::WorkspaceBindingMissing(
            fixture.mount_id.clone()
        ))
    );
    let connection = Connection::open(&store.db_path).expect("raw unchanged state");
    let binding_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM workspace_bindings", [], |row| {
            row.get(0)
        })
        .expect("binding count");
    assert_eq!(binding_count, 0);
}

#[test]
fn current_v21_open_rejects_missing_non_rebuildable_binding_component() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    store
        .save_mount(fixture.mount_config(ProjectionMode::PlainFiles))
        .expect("save mount");
    let connection = Connection::open(&store.db_path).expect("raw connection");
    connection
        .execute(
            "DELETE FROM state_components
             WHERE component_id = 'durable:workspace_bindings'",
            [],
        )
        .expect("remove required component");
    drop(connection);
    drop(store);

    assert!(matches!(
        SqliteStateStore::open(fixture.state_root.clone()),
        Err(StoreError::StateCompatibility(message))
            if message.contains("durable:workspace_bindings")
    ));
    let connection =
        Connection::open(fixture.state_root.join("state.sqlite3")).expect("raw failed-open state");
    let component_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM state_components
             WHERE component_id = 'durable:workspace_bindings'",
            [],
            |row| row.get(0),
        )
        .expect("component count");
    assert_eq!(component_count, 0, "component must not be repaired");
}

#[test]
fn v21_transition_rolls_back_table_roots_components_and_version_on_failure() {
    let fixture = Fixture::new();
    let shared_root = fixture.root.join("LegacyLocality");
    fs::create_dir_all(&shared_root).expect("shared root");
    let mut store = fixture.open();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", shared_root.clone())
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save legacy mount");
    downgrade_to_v20(&store.db_path);
    mark_projection_component_v1(&store.db_path, &ProjectionMode::LinuxFuse);
    let connection = Connection::open(&store.db_path).expect("raw failpoint connection");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_workspace_binding_component
             BEFORE INSERT ON state_components
             WHEN NEW.component_id = 'durable:workspace_bindings'
             BEGIN
                 SELECT RAISE(ABORT, 'injected workspace binding migration failure');
             END;",
        )
        .expect("install migration failpoint");
    drop(connection);
    drop(store);

    assert!(SqliteStateStore::open(fixture.state_root.clone()).is_err());
    let db_path = fixture.state_root.join("state.sqlite3");
    let connection = Connection::open(&db_path).expect("raw rolled-back state");
    let user_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user version");
    let binding_table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name = 'workspace_bindings'",
            [],
            |row| row.get(0),
        )
        .expect("binding table count");
    let stored_root: String = connection
        .query_row(
            "SELECT root FROM mounts WHERE mount_id = ?1",
            params![fixture.mount_id.0.as_str()],
            |row| row.get(0),
        )
        .expect("rolled-back mount root");
    let projection_version: i64 = connection
        .query_row(
            "SELECT version FROM state_components
             WHERE component_id = 'projection:linux_fuse'",
            [],
            |row| row.get(0),
        )
        .expect("rolled-back projection component");
    assert_eq!(user_version, 20);
    assert_eq!(binding_table_count, 0);
    assert_eq!(stored_root, shared_root.to_string_lossy());
    assert_eq!(projection_version, 1);
    connection
        .execute_batch("DROP TRIGGER fail_workspace_binding_component;")
        .expect("remove failpoint");
    drop(connection);

    let restarted = fixture.open();
    assert_eq!(
        restarted
            .get_mount(&fixture.mount_id)
            .expect("mount")
            .expect("mount exists")
            .root,
        shared_root.join("notion")
    );
    assert_eq!(
        restarted
            .get_workspace_binding(&fixture.mount_id)
            .expect("binding")
            .expect("binding exists")
            .mount_target()
            .as_str(),
        "notion"
    );
}

#[test]
fn legacy_unicode_target_collisions_are_disambiguated_in_mount_id_order() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    store
        .save_mount(MountConfig::new(
            MountId::new("z-mount"),
            "notion",
            fixture.root.join("one/Straße"),
        ))
        .expect("save first legacy mount");
    store
        .save_mount(MountConfig::new(
            MountId::new("a-mount"),
            "notion",
            fixture.root.join("two/STRASSE"),
        ))
        .expect("save second legacy mount");
    downgrade_to_v20(&store.db_path);
    drop(store);

    let migrated = fixture.open();
    let records = migrated
        .load_workspace_bindings()
        .expect("load migrated bindings");
    let targets = records
        .iter()
        .map(|record| {
            (
                record.mount_id.0.as_str(),
                record.binding.mount_target().as_str(),
                record.binding.mount_target().collision_key(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(targets[0].0, "a-mount");
    assert_eq!(targets[0].1, "STRASSE");
    assert_eq!(targets[1].0, "z-mount");
    assert_eq!(targets[1].1, "Straße-2");
    assert_ne!(targets[0].2, targets[1].2);
}

#[test]
fn current_reader_rejects_newer_binding_metadata_without_touching_mount() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    let mount = fixture.mount_config(ProjectionMode::PlainFiles);
    store.save_mount(mount.clone()).expect("save mount");
    let connection = Connection::open(&store.db_path).expect("raw connection");
    connection
        .execute(
            "UPDATE workspace_bindings
             SET binding_json = ?1
             WHERE mount_id = ?2",
            params![
                r#"{"binding_version":2,"layout_version":1,"mount_target":"notion-main"}"#,
                fixture.mount_id.0.as_str()
            ],
        )
        .expect("write newer metadata");
    drop(connection);

    assert!(matches!(
        store.get_workspace_binding(&fixture.mount_id),
        Err(StoreError::StateCompatibility(message))
            if message.contains("update required") && message.contains("unsupported")
    ));
    assert_eq!(
        store
            .get_mount(&fixture.mount_id)
            .expect("legacy mount read"),
        Some(mount)
    );
    drop(store);

    assert!(matches!(
        SqliteStateStore::open(fixture.state_root.clone()),
        Err(StoreError::StateCompatibility(message))
            if message.contains("update required") && message.contains("unsupported")
    ));
}

fn downgrade_to_v20(db_path: &Path) {
    let connection = Connection::open(db_path).expect("raw downgrade connection");
    connection
        .execute_batch(
            "DROP TABLE workspace_bindings;
             DELETE FROM state_components
             WHERE component_id = 'durable:workspace_bindings';
             UPDATE state_components SET version = 20
             WHERE component_id = 'core:schema';
             PRAGMA user_version = 20;",
        )
        .expect("downgrade to legacy v20 metadata");
}

fn mark_projection_component_v1(db_path: &Path, projection: &ProjectionMode) {
    let component_id = match projection {
        ProjectionMode::LinuxFuse => "projection:linux_fuse",
        ProjectionMode::WindowsCloudFiles => "projection:windows_cloud_files",
        other => panic!("unsupported legacy projection fixture: {other:?}"),
    };
    let connection = Connection::open(db_path).expect("raw projection downgrade connection");
    connection
        .execute(
            "UPDATE state_components
             SET version = 1, min_reader_version = 1
             WHERE component_id = ?1",
            params![component_id],
        )
        .expect("mark projection component v1");
}

fn dirty_entity(mount_id: &MountId) -> EntityRecord {
    EntityRecord::new(
        mount_id.clone(),
        RemoteId::new("page-1"),
        EntityKind::Page,
        "Roadmap",
        "Roadmap.md",
    )
    .with_hydration(HydrationState::Hydrated)
    .with_content_hash("synced-hash")
}

fn synced_shadow() -> ShadowDocument {
    ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        "# Roadmap\n\nSynced body.\n",
        9,
        [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
    )
    .expect("shadow")
}

fn applying_journal(mount_id: &MountId) -> JournalEntry {
    let push_id = PushId("push-applying".to_string());
    let operation = PushOperation::UpdateBlock {
        block_id: RemoteId::new("paragraph-1"),
        content: "Locally edited and still dirty.".to_string(),
    };
    let operation_id = PushOperationId::for_operation(&push_id, 0, &operation);
    JournalEntry::new(
        push_id,
        mount_id.clone(),
        vec![RemoteId::new("page-1")],
        PushPlan::new(vec![RemoteId::new("page-1")], vec![operation]),
        JournalStatus::Applying,
    )
    .with_apply_effects(vec![JournalApplyEffect::UpdatedBlock {
        operation_id,
        operation_index: 0,
        block_id: RemoteId::new("paragraph-1"),
    }])
}

struct Fixture {
    root: PathBuf,
    state_root: PathBuf,
    mount_root: PathBuf,
    mount_id: MountId,
}

impl Fixture {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "locality-workspace-binding-{}-{unique}-{suffix}",
            std::process::id()
        ));
        let state_root = root.join("state");
        let mount_root = root.join("Locality/notion-main");
        fs::create_dir_all(&mount_root).expect("mount root");
        Self {
            root,
            state_root,
            mount_root,
            mount_id: MountId::new("notion-main"),
        }
    }

    fn open(&self) -> SqliteStateStore {
        SqliteStateStore::open(self.state_root.clone()).expect("open sqlite store")
    }

    fn mount_config(&self, projection: ProjectionMode) -> MountConfig {
        MountConfig::new(self.mount_id.clone(), "notion", self.mount_root.clone())
            .with_remote_root_id(RemoteId::new("root-page"))
            .projection(projection)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
