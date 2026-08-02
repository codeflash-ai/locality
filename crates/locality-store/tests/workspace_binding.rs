use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use locality_core::journal::{
    JournalApplyEffect, JournalEntry, JournalStatus, PushId, PushOperationId,
};
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use locality_core::planner::{PushOperation, PushPlan};
use locality_core::shadow::ShadowDocument;
use locality_core::workspace_layout::MountTarget;
use locality_store::{
    EntityRecord, EntityRepository, EntitySearchRepository, InMemoryStateStore, JournalRepository,
    LegacyLayout0Reason, LegacyWorkspaceMount, MountConfig, MountRepository, ProjectionMode,
    ShadowRepository, SqliteStateStore, StateCompatibilityIssue, StateCompatibilityStatus,
    StoreError, WorkspaceBinding, WorkspaceBindingRecord, WorkspaceBindingRepository,
    WorkspaceHostBinding, WorkspaceHostBindingError, WorkspaceHostBindingResolver,
    WorkspaceHostPlatform, WorkspaceId, WorkspaceProjectionIdentity, WorkspaceRebindBlocker,
    WorkspaceRemountRecoveryOutcome,
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
            current: 27,
        }]
    );

    let reopened = fixture.open();
    assert_eq!(
        reopened
            .get_workspace_binding(&fixture.mount_id)
            .expect("read layout zero binding"),
        None
    );
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
    let binding_count: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM workspace_bindings WHERE mount_id = ?1",
            params![fixture.mount_id.0.as_str()],
            |row| row.get(0),
        )
        .expect("binding count");
    assert_eq!(binding_count, 0);
}

#[test]
fn coordinator_binding_survives_restart_and_resolves_host_roots_without_mutating_mount() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    let mount = fixture.mount_config(ProjectionMode::LinuxFuse);
    let entity = dirty_entity(&fixture.mount_id);
    let shadow = synced_shadow();
    let journal = applying_journal(&fixture.mount_id);
    store.save_mount(mount).expect("save mount");
    save_trusted_fixture_binding(&mut store, &fixture);
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
fn layout1_host_binding_persists_identity_root_domain_sequence_and_resolves_mount() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    let mount = fixture.mount_config(ProjectionMode::LinuxFuse);
    store.save_mount(mount.clone()).expect("save mount");
    let workspace_id = WorkspaceId::new("locality.workspace.linux_fuse").expect("workspace ID");
    let host = WorkspaceHostBinding::new(
        WorkspaceHostPlatform::current(),
        workspace_id.clone(),
        fixture.mount_root.parent().expect("workspace root"),
        WorkspaceProjectionIdentity::new("linux-fuse:locality-shared-root")
            .expect("projection identity"),
        1,
    )
    .expect("host binding");
    let binding = WorkspaceBinding::for_workspace(
        workspace_id.clone(),
        MountTarget::new("notion-main").expect("target"),
    );
    store
        .commit_workspace_binding(
            host.clone(),
            WorkspaceBindingRecord::new(fixture.mount_id.clone(), binding),
        )
        .expect("commit workspace binding");

    assert_eq!(
        store
            .resolve_workspace_mount_root(&mount)
            .expect("resolve root"),
        fixture.mount_root
    );
    drop(store);

    let reopened = fixture.open();
    assert_eq!(
        reopened
            .get_workspace_host_binding(&workspace_id)
            .expect("read host binding"),
        Some(host)
    );
    assert_eq!(
        reopened
            .resolve_workspace_mount_root(&mount)
            .expect("resolve after restart"),
        fixture.mount_root
    );
}

#[test]
fn layout1_commit_rejects_a_host_root_that_would_move_the_mount() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    store
        .save_mount(fixture.mount_config(ProjectionMode::LinuxFuse))
        .expect("save mount");
    let workspace_id = WorkspaceId::new("locality.workspace.linux_fuse").expect("workspace ID");
    let host = WorkspaceHostBinding::new(
        WorkspaceHostPlatform::current(),
        workspace_id.clone(),
        fixture.root.join("DifferentLocality"),
        WorkspaceProjectionIdentity::new("linux-fuse:locality-shared-root")
            .expect("projection identity"),
        1,
    )
    .expect("host binding");
    let result = store.commit_workspace_binding(
        host,
        WorkspaceBindingRecord::new(
            fixture.mount_id.clone(),
            WorkspaceBinding::for_workspace(
                workspace_id.clone(),
                MountTarget::new("notion-main").expect("target"),
            ),
        ),
    );

    assert!(
        matches!(result, Err(StoreError::InvalidState(message)) if message.contains("preserved root"))
    );
    assert_eq!(
        store
            .get_workspace_binding(&fixture.mount_id)
            .expect("binding lookup"),
        None
    );
    assert_eq!(
        store
            .get_workspace_host_binding(&workspace_id)
            .expect("host lookup"),
        None
    );
}

#[test]
fn failed_atomic_remount_preserves_mount_root_and_source_state() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    let original = fixture.mount_config(ProjectionMode::LinuxFuse);
    let workspace_id = WorkspaceId::new("locality.workspace.linux_fuse").expect("workspace ID");
    let projection_identity = WorkspaceProjectionIdentity::new("linux-fuse:locality-shared-root")
        .expect("projection identity");
    let host = WorkspaceHostBinding::new(
        WorkspaceHostPlatform::current(),
        workspace_id.clone(),
        fixture.mount_root.parent().expect("workspace root"),
        projection_identity.clone(),
        1,
    )
    .expect("host binding");
    let record = WorkspaceBindingRecord::new(
        fixture.mount_id.clone(),
        WorkspaceBinding::for_workspace(
            workspace_id.clone(),
            MountTarget::new("notion-main").expect("target"),
        ),
    );
    store
        .save_mount_with_workspace_binding(original.clone(), host, record.clone())
        .expect("initial atomic mount");
    let entity = dirty_entity(&fixture.mount_id);
    store
        .save_entity(entity.clone())
        .expect("save source state");

    let moved_parent = fixture.root.join("MovedLocality");
    fs::create_dir_all(&moved_parent).expect("moved parent");
    let requested = MountConfig::new(
        fixture.mount_id.clone(),
        "different-connector",
        moved_parent.join("notion-main"),
    )
    .projection(ProjectionMode::LinuxFuse)
    .with_settings_json("{\"changed\":true}");
    let requested_host = WorkspaceHostBinding::new(
        WorkspaceHostPlatform::current(),
        workspace_id,
        moved_parent,
        projection_identity,
        1,
    )
    .expect("requested host");

    assert!(matches!(
        store.save_mount_with_workspace_binding(requested, requested_host, record),
        Err(StoreError::InvalidState(message))
            if message.contains("immutable outside an owning coordinator")
    ));
    assert_eq!(
        store
            .get_mount(&fixture.mount_id)
            .expect("mount after failure"),
        Some(original)
    );
    assert_eq!(
        store
            .get_entity(&fixture.mount_id, &RemoteId::new("page-1"))
            .expect("source state after failure"),
        Some(entity)
    );
}

#[cfg(unix)]
#[test]
fn atomic_binding_rejects_mount_symlink_that_escapes_trusted_root() {
    let fixture = Fixture::new();
    let trusted_root = fixture.root.join("TrustedLocality");
    let outside = fixture.root.join("Outside/notion-main");
    fs::create_dir_all(&trusted_root).expect("trusted root");
    fs::create_dir_all(&outside).expect("outside target");
    let mount_root = trusted_root.join("notion-main");
    symlink(&outside, &mount_root).expect("escaping mount symlink");
    let workspace_id = WorkspaceId::new("locality.workspace.symlink-test").expect("workspace ID");
    let host = WorkspaceHostBinding::new(
        WorkspaceHostPlatform::current(),
        workspace_id.clone(),
        &trusted_root,
        WorkspaceProjectionIdentity::new("linux-fuse:symlink-test").expect("projection identity"),
        1,
    )
    .expect("host binding");
    let mount_id = MountId::new("symlink-mount");
    let mount = MountConfig::new(mount_id.clone(), "notion", &mount_root)
        .projection(ProjectionMode::LinuxFuse);
    let record = WorkspaceBindingRecord::new(
        mount_id.clone(),
        WorkspaceBinding::for_workspace(
            workspace_id,
            MountTarget::new("notion-main").expect("target"),
        ),
    );
    let mut store = fixture.open();

    assert!(matches!(
        store.save_mount_with_workspace_binding(mount, host, record),
        Err(StoreError::InvalidState(message)) if message.contains("escapes")
    ));
    assert_eq!(store.get_mount(&mount_id).expect("mount lookup"), None);
}

#[test]
fn identical_targets_are_unique_per_workspace_not_globally() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    for (ordinal, workspace_name) in ["alpha", "beta"].into_iter().enumerate() {
        let workspace_root = fixture.root.join(format!("Locality-{workspace_name}"));
        fs::create_dir_all(&workspace_root).expect("workspace root");
        let mount_id = MountId::new(format!("mount-{workspace_name}"));
        let workspace_id =
            WorkspaceId::new(format!("locality.workspace.{workspace_name}")).expect("workspace");
        let host = WorkspaceHostBinding::new(
            WorkspaceHostPlatform::current(),
            workspace_id.clone(),
            &workspace_root,
            WorkspaceProjectionIdentity::new(format!("linux-fuse:{workspace_name}"))
                .expect("projection identity"),
            1,
        )
        .expect("host binding");
        let mount = MountConfig::new(
            mount_id.clone(),
            "notion",
            workspace_root.join("shared-target"),
        )
        .projection(ProjectionMode::LinuxFuse);
        let record = WorkspaceBindingRecord::new(
            mount_id,
            WorkspaceBinding::for_workspace(
                workspace_id,
                MountTarget::new("shared-target").expect("target"),
            ),
        );
        store
            .save_mount_with_workspace_binding(mount, host, record)
            .unwrap_or_else(|error| panic!("workspace {ordinal} commit: {error}"));
    }

    let connection = Connection::open(&store.db_path).expect("raw connection");
    let scopes: Vec<String> = connection
        .prepare(
            "SELECT workspace_id FROM workspace_bindings
             WHERE target_collision_key = 'shared-target' ORDER BY workspace_id",
        )
        .expect("prepare scopes")
        .query_map([], |row| row.get(0))
        .expect("query scopes")
        .collect::<Result<_, _>>()
        .expect("collect scopes");
    assert_eq!(
        scopes,
        vec![
            "locality.workspace.alpha".to_string(),
            "locality.workspace.beta".to_string(),
        ]
    );
}

#[test]
fn current_v2_component_migration_preserves_existing_v1_sqlite_binding() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    let mount = fixture.mount_config(ProjectionMode::LinuxFuse);
    store.save_mount(mount.clone()).expect("save mount");
    save_trusted_fixture_binding(&mut store, &fixture);
    drop(store);

    let connection = Connection::open(fixture.state_root.join("state.sqlite3"))
        .expect("raw current schema connection");
    connection
        .execute_batch(
            "DROP TABLE workspace_host_bindings;
             UPDATE state_components
             SET version = 2, min_reader_version = 2,
                 data_json = '{\"format\":\"workspace_binding.v1\",\"layout_0_without_binding\":true}'
             WHERE component_id = 'durable:workspace_bindings';",
        )
        .expect("downgrade workspace component");
    drop(connection);

    let reopened = fixture.open();
    let binding = reopened
        .get_workspace_binding(&fixture.mount_id)
        .expect("read migrated binding")
        .expect("binding preserved");
    assert_eq!(binding.mount_target().as_str(), "notion-main");
    assert_eq!(binding.workspace_id(), None);
    assert_eq!(
        reopened
            .resolve_workspace_mount_root(&mount)
            .expect("legacy binding fallback"),
        fixture.mount_root
    );
    let connection = Connection::open(fixture.state_root.join("state.sqlite3"))
        .expect("raw migrated connection");
    let component_version: i64 = connection
        .query_row(
            "SELECT version FROM state_components WHERE component_id = 'durable:workspace_bindings'",
            [],
            |row| row.get(0),
        )
        .expect("component version");
    assert_eq!(component_version, 4);
}

#[test]
fn current_v3_component_migration_adds_remount_outcome_table() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let connection = Connection::open(&store.db_path).expect("raw current schema connection");
    connection
        .execute_batch(
            "DROP TABLE workspace_remount_recoveries;
             UPDATE state_components
             SET version = 3, min_reader_version = 3,
                 data_json = '{\"format\":\"workspace_binding.v2\",\"layout_0_without_binding\":true,\"legacy_v1_readable\":true,\"target_scope\":\"workspace_id\"}'
             WHERE component_id = 'durable:workspace_bindings';",
        )
        .expect("downgrade workspace component");
    drop(connection);
    drop(store);

    let reopened = fixture.open();
    let connection = Connection::open(&reopened.db_path).expect("raw migrated connection");
    let migrated: (i64, bool, bool) = connection
        .query_row(
            "SELECT
                (SELECT version FROM state_components
                 WHERE component_id = 'durable:workspace_bindings'),
                EXISTS(SELECT 1 FROM sqlite_master
                       WHERE type = 'table' AND name = 'workspace_remount_recoveries'),
                EXISTS(SELECT 1 FROM sqlite_master
                       WHERE type = 'index'
                         AND name = 'workspace_remount_recoveries_mount_unique')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("migration state");
    assert_eq!(migrated, (4, true, true));
}

#[test]
fn identical_mount_commit_persists_atomic_remount_outcome_across_reopen() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    let mount = fixture.mount_config(ProjectionMode::LinuxFuse);
    let (host, record) = fixture_atomic_workspace_binding(&fixture);
    store
        .save_mount_with_workspace_binding(mount.clone(), host.clone(), record.clone())
        .expect("save atomic mount");

    store
        .begin_workspace_remount_recovery("identical-commit", &fixture.mount_id)
        .expect("prepare outcome");
    let mut cleanup = || Ok(());
    store
        .save_mount_with_workspace_binding_and_cleanup(mount.clone(), host, record, &mut cleanup)
        .expect("commit identical mount");
    assert_eq!(
        store
            .get_workspace_remount_recovery("identical-commit")
            .expect("outcome"),
        Some((
            fixture.mount_id.clone(),
            WorkspaceRemountRecoveryOutcome::Committed
        ))
    );
    drop(store);

    let reopened = fixture.open();
    assert_eq!(
        reopened
            .get_workspace_remount_recovery("identical-commit")
            .expect("reopened outcome"),
        Some((
            fixture.mount_id.clone(),
            WorkspaceRemountRecoveryOutcome::Committed
        ))
    );
    assert_eq!(
        reopened.get_mount(&fixture.mount_id).expect("mount"),
        Some(mount)
    );
}

#[test]
fn identical_mount_failed_cleanup_stays_prepared_and_stale_token_is_rejected() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    let mount = fixture.mount_config(ProjectionMode::LinuxFuse);
    let (host, record) = fixture_atomic_workspace_binding(&fixture);
    store
        .save_mount_with_workspace_binding(mount.clone(), host.clone(), record.clone())
        .expect("save atomic mount");

    store
        .begin_workspace_remount_recovery("identical-prepared", &fixture.mount_id)
        .expect("prepare outcome");
    assert!(matches!(
        store.begin_workspace_remount_recovery("stale-second-token", &fixture.mount_id),
        Err(StoreError::InvalidState(message)) if message.contains("already has")
    ));
    let mut cleanup = || {
        Err(StoreError::InvalidState(
            "injected cleanup failure".to_string(),
        ))
    };
    assert!(matches!(
        store.save_mount_with_workspace_binding_and_cleanup(
            mount.clone(), host, record, &mut cleanup
        ),
        Err(StoreError::InvalidState(message)) if message == "injected cleanup failure"
    ));
    assert_eq!(
        store
            .get_workspace_remount_recovery("identical-prepared")
            .expect("outcome"),
        Some((
            fixture.mount_id.clone(),
            WorkspaceRemountRecoveryOutcome::Prepared
        ))
    );
    assert_eq!(
        store.get_mount(&fixture.mount_id).expect("mount"),
        Some(mount)
    );
}

#[test]
fn workspace_component_upgrade_preflights_global_compatibility_before_schema_mutation() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let connection = Connection::open(&store.db_path).expect("raw connection");
    connection
        .execute_batch(
            "DROP TABLE workspace_bindings;
             DROP TABLE workspace_host_bindings;
             CREATE TABLE workspace_bindings (
                 mount_id TEXT PRIMARY KEY,
                 binding_json TEXT NOT NULL,
                 target_collision_key TEXT NOT NULL UNIQUE,
                 FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
             );
             UPDATE state_components
             SET version = 2, min_reader_version = 2
             WHERE component_id = 'durable:workspace_bindings';
             INSERT INTO state_components (
                 component_id, component_kind, version, min_reader_version,
                 required, rebuildable, data_json, updated_at
             ) VALUES (
                 'future:incompatible', 'durable_json', 1, 99,
                 1, 0, '{}', '2026-08-02T00:00:00Z'
             );",
        )
        .expect("prepare incompatible component fixture");
    drop(connection);
    let db_path = store.db_path.clone();
    drop(store);

    assert!(matches!(
        SqliteStateStore::open(fixture.state_root.clone()),
        Err(StoreError::StateCompatibility(message))
            if message.contains("future:incompatible")
    ));
    let connection = Connection::open(db_path).expect("raw failed-upgrade connection");
    let host_table_exists: bool = connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_master
                WHERE type = 'table' AND name = 'workspace_host_bindings'
             )",
            [],
            |row| row.get(0),
        )
        .expect("host table existence");
    let workspace_column_exists: bool = connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('workspace_bindings')
                WHERE name = 'workspace_id'
             )",
            [],
            |row| row.get(0),
        )
        .expect("workspace column existence");
    let component_version: i64 = connection
        .query_row(
            "SELECT version FROM state_components
             WHERE component_id = 'durable:workspace_bindings'",
            [],
            |row| row.get(0),
        )
        .expect("workspace component version");
    assert!(!host_table_exists);
    assert!(!workspace_column_exists);
    assert_eq!(component_version, 2);
}

#[test]
fn workspace_component_upgrade_allows_other_supported_component_migrations() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let connection = Connection::open(&store.db_path).expect("raw connection");
    connection
        .execute_batch(
            "DROP TABLE workspace_bindings;
             DROP TABLE workspace_host_bindings;
             CREATE TABLE workspace_bindings (
                 mount_id TEXT PRIMARY KEY,
                 binding_json TEXT NOT NULL,
                 target_collision_key TEXT NOT NULL UNIQUE,
                 FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
             );
             UPDATE state_components
             SET version = 2, min_reader_version = 2
             WHERE component_id = 'durable:workspace_bindings';
             UPDATE state_components
             SET version = 2, min_reader_version = 1
             WHERE component_id = 'durable:journals';",
        )
        .expect("prepare mixed supported component fixture");
    drop(connection);
    drop(store);

    let reopened = fixture.open();
    let connection = Connection::open(&reopened.db_path).expect("raw migrated connection");
    let versions = connection
        .prepare(
            "SELECT component_id, version
             FROM state_components
             WHERE component_id IN ('durable:journals', 'durable:workspace_bindings')
             ORDER BY component_id",
        )
        .expect("prepare component versions")
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .expect("query component versions")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect component versions");
    assert_eq!(
        versions,
        vec![
            ("durable:journals".to_string(), 3),
            ("durable:workspace_bindings".to_string(), 4),
        ]
    );
    let workspace_column_exists: bool = connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('workspace_bindings')
                WHERE name = 'workspace_id'
             )",
            [],
            |row| row.get(0),
        )
        .expect("workspace scope column");
    assert!(workspace_column_exists);
}

#[test]
fn workspace_rebind_preflight_rejects_dirty_state_without_changing_root() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    let mount = fixture.mount_config(ProjectionMode::PlainFiles);
    store.save_mount(mount.clone()).expect("save mount");
    save_trusted_fixture_binding(&mut store, &fixture);
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
    save_trusted_fixture_binding(&mut store, &fixture);
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
    save_trusted_fixture_binding(&mut store, &fixture);

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
    save_trusted_fixture_binding(&mut store, &fixture);

    assert_eq!(
        store.check_workspace_rebind(&fixture.mount_id),
        Err(StoreError::WorkspaceRebindBlocked {
            mount_id: fixture.mount_id.clone(),
            blocker: WorkspaceRebindBlocker::RequiresOwningCoordinator,
        })
    );
}

#[test]
fn existing_binding_target_is_immutable_with_dirty_journal_or_projection_state() {
    for active_state in [
        ActiveMountState::Dirty,
        ActiveMountState::Journal,
        ActiveMountState::Projection,
    ] {
        let fixture = Fixture::new();
        assert_existing_binding_target_is_immutable(
            InMemoryStateStore::new(),
            &fixture,
            active_state,
        );
        assert_existing_binding_target_is_immutable(fixture.open(), &fixture, active_state);
    }
}

#[test]
fn v20_linux_fuse_binding_uses_post_migration_mount_point() {
    assert_legacy_virtual_binding_uses_final_mount_point(ProjectionMode::LinuxFuse);
}

#[test]
fn v20_windows_cloud_files_binding_uses_post_migration_mount_point() {
    assert_legacy_virtual_binding_uses_final_mount_point(ProjectionMode::WindowsCloudFiles);
}

#[test]
fn v20_entity_search_v1_rebuilds_fts_before_component_seeding() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    store
        .save_mount(fixture.mount_config(ProjectionMode::PlainFiles))
        .expect("save mount");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("page-search"),
            EntityKind::Page,
            "Stale index title",
            "Search.md",
        ))
        .expect("save indexed entity");
    let connection = Connection::open(&store.db_path).expect("raw legacy search state");
    connection
        .execute(
            "UPDATE entities
             SET title = 'Quasar migration result'
             WHERE mount_id = ?1 AND remote_id = 'page-search'",
            params![fixture.mount_id.0.as_str()],
        )
        .expect("make search index stale");
    drop(connection);
    assert!(
        store
            .list_entity_search_candidates(&fixture.mount_id, "quasar", None)
            .expect("search stale index")
            .expect("sqlite search")
            .is_empty()
    );
    downgrade_to_v20(&store.db_path);
    mark_entity_search_component_v1(&store.db_path);
    drop(store);

    let migrated = fixture.open();
    let matches = migrated
        .list_entity_search_candidates(&fixture.mount_id, "quasar", None)
        .expect("search rebuilt index")
        .expect("sqlite search");
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].entity.remote_id, RemoteId::new("page-search"));
    let connection = Connection::open(&migrated.db_path).expect("raw migrated state");
    let component_version: i64 = connection
        .query_row(
            "SELECT version FROM state_components
             WHERE component_id = 'cache:entity_search'",
            [],
            |row| row.get(0),
        )
        .expect("entity search component version");
    assert_eq!(component_version, 2);
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
    assert_eq!(mount.root, shared_root.join("notion"));
    assert_eq!(
        migrated
            .get_workspace_binding(&fixture.mount_id)
            .expect("layout zero binding"),
        None
    );
}

#[test]
fn current_layout_zero_mount_opens_without_reconstructing_a_binding() {
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

    let reopened = SqliteStateStore::open(fixture.state_root.clone())
        .expect("layout zero state remains readable");
    assert_eq!(
        reopened
            .get_workspace_binding(&fixture.mount_id)
            .expect("layout zero binding lookup"),
        None
    );
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
fn current_layout_zero_mount_save_does_not_reconstruct_missing_binding() {
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

    store
        .save_mount(mount)
        .expect("layout zero mount remains writable without migration");
    let connection = Connection::open(&store.db_path).expect("raw unchanged state");
    let binding_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM workspace_bindings", [], |row| {
            row.get(0)
        })
        .expect("binding count");
    assert_eq!(binding_count, 0);
}

#[test]
fn new_mount_remains_layout_zero_until_a_trusted_coordinator_saves_binding() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    let mount_id = MountId::new("google-docs-main");
    let mount = MountConfig::new(
        mount_id.clone(),
        "google-docs",
        fixture.root.join("Locality/google-docs-main"),
    )
    .projection(ProjectionMode::LinuxFuse);
    store.save_mount(mount.clone()).expect("save mount");
    assert_eq!(
        store
            .get_workspace_binding(&mount_id)
            .expect("read binding"),
        None,
        "save_mount must not infer trust from a common parent"
    );

    let legacy = LegacyWorkspaceMount::new(mount_id.clone(), mount.root.clone());
    let plan = WorkspaceHostBindingResolver::current()
        .plan_legacy_migration(
            mount.root.parent().expect("trusted workspace root"),
            std::slice::from_ref(&legacy),
        )
        .expect("trusted coordinator plan");
    let binding = plan.layout1_bindings()[0].clone();
    store
        .save_workspace_binding(binding)
        .expect("save coordinator-approved binding");
    assert_eq!(
        store
            .get_workspace_binding(&mount_id)
            .expect("read explicit binding")
            .expect("binding exists")
            .mount_target()
            .as_str(),
        "google-docs-main"
    );
    drop(store);

    let restarted = fixture.open();
    assert_eq!(
        restarted.get_mount(&mount_id).expect("read mount"),
        Some(mount.clone())
    );
    assert!(
        restarted
            .get_workspace_binding(&mount_id)
            .expect("read restarted binding")
            .is_some()
    );

    let mut memory = InMemoryStateStore::new();
    memory.save_mount(mount).expect("save memory mount");
    assert_eq!(
        memory
            .get_workspace_binding(&mount_id)
            .expect("read memory binding"),
        None,
        "in-memory save_mount must follow the same layout-0 contract"
    );
}

#[test]
fn workspace_binding_collision_includes_unbound_layout_zero_mounts() {
    assert_layout_zero_collision_is_reserved(InMemoryStateStore::default());

    let fixture = Fixture::new();
    assert_layout_zero_collision_is_reserved(fixture.open());
}

#[test]
fn in_memory_v1_upgrade_still_scans_workspace_collisions() {
    let fixture = Fixture::new();
    assert_v1_upgrade_scans_workspace_collisions(InMemoryStateStore::default(), &fixture.root);
}

#[test]
fn sqlite_v1_upgrade_still_scans_workspace_collisions() {
    let fixture = Fixture::new();
    assert_v1_upgrade_scans_workspace_collisions(fixture.open(), &fixture.root);
}

#[test]
fn in_memory_stale_v1_target_still_reserves_authoritative_legacy_root() {
    let fixture = Fixture::new();
    assert_stale_v1_target_reserves_authoritative_legacy_root(
        InMemoryStateStore::default(),
        &fixture.root,
    );
}

#[test]
fn sqlite_stale_v1_target_still_reserves_authoritative_legacy_root() {
    let fixture = Fixture::new();
    assert_stale_v1_target_reserves_authoritative_legacy_root(fixture.open(), &fixture.root);
}

#[test]
fn exact_replay_skips_workspace_collision_scan() {
    let memory_fixture = Fixture::new();
    assert_exact_replay_skips_workspace_collision_scan(
        InMemoryStateStore::default(),
        &memory_fixture.root,
    );
    let sqlite_fixture = Fixture::new();
    assert_exact_replay_skips_workspace_collision_scan(sqlite_fixture.open(), &sqlite_fixture.root);
}

fn assert_exact_replay_skips_workspace_collision_scan<S>(mut store: S, root: &Path)
where
    S: MountRepository + WorkspaceBindingRepository,
{
    let workspace_root = root.join("ExactReplayWorkspace");
    fs::create_dir_all(&workspace_root).expect("workspace root");
    let mount_id = MountId::new("exact-replay");
    let collision_mount_id = MountId::new("later-layout-zero-collision");
    let target = MountTarget::new("shared-target").expect("target");
    let workspace_id = WorkspaceId::new("locality.workspace.exact-replay").expect("workspace");
    let host = WorkspaceHostBinding::new(
        WorkspaceHostPlatform::current(),
        workspace_id.clone(),
        &workspace_root,
        WorkspaceProjectionIdentity::new("test:exact-replay").expect("projection identity"),
        1,
    )
    .expect("host binding");
    let record = WorkspaceBindingRecord::new(
        mount_id.clone(),
        WorkspaceBinding::for_workspace(workspace_id, target.clone()),
    );

    store
        .save_mount(MountConfig::new(
            mount_id,
            "notion",
            workspace_root.join(target.as_str()),
        ))
        .expect("save bound mount");
    store
        .commit_workspace_binding(host.clone(), record.clone())
        .expect("initial binding commit");
    store
        .save_mount(MountConfig::new(
            collision_mount_id,
            "google-docs",
            workspace_root.join(target.as_str()),
        ))
        .expect("introduce later layout-zero collision");

    store
        .commit_workspace_binding(host, record)
        .expect("exact replay must not rescan later collisions");
}

fn assert_stale_v1_target_reserves_authoritative_legacy_root<S>(mut store: S, root: &Path)
where
    S: MountRepository + WorkspaceBindingRepository,
{
    let workspace_root = root.join("StaleV1Workspace");
    fs::create_dir_all(&workspace_root).expect("workspace root");
    let legacy_mount_id = MountId::new("legacy-stale-v1");
    let candidate_mount_id = MountId::new("candidate-layout-1");
    let authoritative_target = MountTarget::new("authoritative-root").expect("target");
    let stale_target = MountTarget::new("stale-v1-metadata").expect("stale target");
    let authoritative_root = workspace_root.join(authoritative_target.as_str());

    store
        .save_mount(MountConfig::new(
            legacy_mount_id.clone(),
            "notion",
            authoritative_root.clone(),
        ))
        .expect("save legacy mount");
    store
        .save_workspace_binding(WorkspaceBindingRecord::new(
            legacy_mount_id.clone(),
            WorkspaceBinding::new(stale_target),
        ))
        .expect("save stale v1 binding");
    store
        .save_mount(MountConfig::new(
            candidate_mount_id.clone(),
            "google-docs",
            authoritative_root,
        ))
        .expect("save layout-1 candidate");

    let workspace_id = WorkspaceId::new("locality.workspace.stale-v1").expect("workspace");
    let error = store
        .commit_workspace_binding(
            WorkspaceHostBinding::new(
                WorkspaceHostPlatform::current(),
                workspace_id.clone(),
                &workspace_root,
                WorkspaceProjectionIdentity::new("test:stale-v1-root")
                    .expect("projection identity"),
                1,
            )
            .expect("host binding"),
            WorkspaceBindingRecord::new(
                candidate_mount_id,
                WorkspaceBinding::for_workspace(workspace_id, authoritative_target.clone()),
            ),
        )
        .expect_err("authoritative legacy root must remain reserved");

    assert_eq!(
        error,
        StoreError::WorkspaceMountTargetCollision {
            target: authoritative_target.as_str().to_string(),
            existing_mount_id: legacy_mount_id,
        }
    );
}

fn assert_v1_upgrade_scans_workspace_collisions<S>(mut store: S, root: &Path)
where
    S: MountRepository + WorkspaceBindingRepository,
{
    let legacy_mount_id = MountId::new("legacy-v1");
    let existing_mount_id = MountId::new("existing-v2");
    let legacy_workspace_root = root.join("LegacyWorkspace");
    let target_workspace_root = root.join("TargetWorkspace");
    fs::create_dir_all(&legacy_workspace_root).expect("legacy workspace root");
    fs::create_dir_all(&target_workspace_root).expect("target workspace root");
    let target = MountTarget::new("shared-target").expect("shared target");

    store
        .save_mount(MountConfig::new(
            legacy_mount_id.clone(),
            "notion",
            legacy_workspace_root.join(target.as_str()),
        ))
        .expect("save legacy mount");
    store
        .save_workspace_binding(WorkspaceBindingRecord::new(
            legacy_mount_id.clone(),
            WorkspaceBinding::new(target.clone()),
        ))
        .expect("save v1 binding");

    store
        .save_mount(MountConfig::new(
            existing_mount_id.clone(),
            "google-docs",
            target_workspace_root.join(target.as_str()),
        ))
        .expect("save existing v2 mount");
    let workspace_id = WorkspaceId::new("locality.workspace.v1-upgrade").expect("workspace");
    let projection_identity =
        WorkspaceProjectionIdentity::new("test:v1-upgrade-collision").expect("projection identity");
    store
        .commit_workspace_binding(
            WorkspaceHostBinding::new(
                WorkspaceHostPlatform::current(),
                workspace_id.clone(),
                &target_workspace_root,
                projection_identity.clone(),
                1,
            )
            .expect("initial host binding"),
            WorkspaceBindingRecord::new(
                existing_mount_id.clone(),
                WorkspaceBinding::for_workspace(workspace_id.clone(), target.clone()),
            ),
        )
        .expect("save existing v2 binding");

    store
        .save_mount(MountConfig::new(
            legacy_mount_id.clone(),
            "notion",
            target_workspace_root.join(target.as_str()),
        ))
        .expect("move legacy mount under target workspace");
    let error = store
        .commit_workspace_binding(
            WorkspaceHostBinding::new(
                WorkspaceHostPlatform::current(),
                workspace_id.clone(),
                &target_workspace_root,
                projection_identity,
                2,
            )
            .expect("next host binding"),
            WorkspaceBindingRecord::new(
                legacy_mount_id.clone(),
                WorkspaceBinding::for_workspace(workspace_id.clone(), target.clone()),
            ),
        )
        .expect_err("v1 upgrade must scan the target workspace");

    assert_eq!(
        error,
        StoreError::WorkspaceMountTargetCollision {
            target: target.as_str().to_string(),
            existing_mount_id,
        }
    );
    assert_eq!(
        store
            .get_workspace_binding(&legacy_mount_id)
            .expect("legacy binding after collision")
            .expect("legacy binding remains")
            .workspace_id(),
        None
    );
    assert_eq!(
        store
            .get_workspace_host_binding(&workspace_id)
            .expect("host after collision")
            .expect("host remains")
            .layout_sequence(),
        1
    );
}

fn assert_layout_zero_collision_is_reserved<S>(mut store: S)
where
    S: MountRepository + WorkspaceBindingRepository,
{
    let bound = MountId::new("bound");
    let layout_zero = MountId::new("layout-zero");
    let candidate = MountId::new("candidate");
    store
        .save_mount(MountConfig::new(
            bound,
            "notion",
            "/tmp/workspace-one/Alpha",
        ))
        .expect("save bound mount");
    store
        .save_mount(MountConfig::new(
            layout_zero.clone(),
            "notion",
            "/tmp/workspace-two/Beta",
        ))
        .expect("save layout zero mount");
    store
        .save_mount(MountConfig::new(
            candidate.clone(),
            "notion",
            "/tmp/workspace-three/Gamma",
        ))
        .expect("save candidate mount");
    assert_eq!(
        store
            .get_workspace_binding(&layout_zero)
            .expect("layout zero lookup"),
        None
    );

    let binding = WorkspaceBinding::new(MountTarget::new("BETA").expect("target"));
    assert_eq!(
        store.save_workspace_binding(WorkspaceBindingRecord::new(candidate, binding)),
        Err(StoreError::WorkspaceMountTargetCollision {
            target: "BETA".to_string(),
            existing_mount_id: layout_zero,
        })
    );
}

#[test]
fn current_schema_open_rejects_missing_non_rebuildable_binding_component() {
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
fn mount_root_inspection_reads_legacy_schema_without_migrating_it() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    store
        .save_mount(fixture.mount_config(ProjectionMode::PlainFiles))
        .expect("save mount");
    downgrade_to_v20(&store.db_path);
    drop(store);

    let mounts = SqliteStateStore::inspect_mount_roots_read_only(&fixture.state_root)
        .expect("inspect legacy roots");
    assert_eq!(
        mounts,
        vec![LegacyWorkspaceMount::new(
            fixture.mount_id.clone(),
            fixture.mount_root.clone(),
        )]
    );
    let connection =
        Connection::open(fixture.state_root.join("state.sqlite3")).expect("reopen legacy state");
    let user_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("legacy version");
    let binding_table: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name = 'workspace_bindings'",
            [],
            |row| row.get(0),
        )
        .expect("binding table count");
    assert_eq!((user_version, binding_table), (20, 0));
}

#[test]
fn mount_root_inspection_does_not_initialize_unrelated_sqlite() {
    let fixture = Fixture::new();
    fs::create_dir_all(&fixture.state_root).expect("create state root");
    let db_path = fixture.state_root.join("state.sqlite3");
    let connection = Connection::open(&db_path).expect("create unrelated sqlite");
    connection
        .execute_batch(
            "CREATE TABLE sentinel (value TEXT NOT NULL);
             INSERT INTO sentinel (value) VALUES ('unchanged');
             PRAGMA user_version = 7;",
        )
        .expect("seed unrelated sqlite");
    drop(connection);

    assert!(matches!(
        SqliteStateStore::inspect_mount_roots_read_only(&fixture.state_root),
        Err(StoreError::StateCompatibility(message)) if message.contains("mounts table")
    ));
    let connection = Connection::open(db_path).expect("reopen unrelated sqlite");
    let sentinel: String = connection
        .query_row("SELECT value FROM sentinel", [], |row| row.get(0))
        .expect("sentinel");
    let user_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("unrelated version");
    let locality_tables: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name IN ('mounts', 'workspace_bindings', 'state_components')",
            [],
            |row| row.get(0),
        )
        .expect("Locality table count");
    assert_eq!(
        (sentinel.as_str(), user_version, locality_tables),
        ("unchanged", 7, 0)
    );
}

#[test]
fn workspace_binding_transition_rolls_back_roots_components_and_version_on_failure() {
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
            "INSERT INTO state_components (
                component_id, component_kind, version, min_reader_version,
                required, rebuildable, data_json, updated_at
             ) VALUES (
                'projection:notion_workspace_roots', 'projection_layout', 2, 1,
                1, 0, '{}', 'legacy'
             );
             INSERT INTO entities (
                mount_id, remote_id, kind_json, title, path, hydration_json,
                content_hash, remote_edited_at
             ) VALUES
                ('notion-main', 'notion-root:workspace', '\"directory\"',
                 'Workspace', 'Workspace', '\"virtual\"', NULL, NULL),
                ('notion-main', 'legacy-child', '\"page\"',
                 'Legacy child', 'Workspace/Legacy/page.md', '\"hydrated\"', NULL, NULL);
             INSERT INTO entity_search_fts (
                mount_id, remote_id, title, path, observed_title, observed_path
             ) VALUES (
                'notion-main', 'legacy-child', 'Rollback sentinel',
                'Workspace/Legacy/page.md', NULL, NULL
             );
             INSERT INTO search_documents_fts (
                mount_id, remote_id, connector, kind, title, path
             ) VALUES (
                'notion-main', 'legacy-child', 'notion', '\"page\"',
                'Rollback sentinel', 'Workspace/Legacy/page.md'
             );
             CREATE TRIGGER fail_workspace_binding_component
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
    let retired_component_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM state_components
             WHERE component_id = 'projection:notion_workspace_roots'",
            [],
            |row| row.get(0),
        )
        .expect("rolled-back retired component");
    let retired_root_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM entities
             WHERE remote_id = 'notion-root:workspace'",
            [],
            |row| row.get(0),
        )
        .expect("rolled-back retired root");
    let legacy_child_path: String = connection
        .query_row(
            "SELECT path FROM entities WHERE remote_id = 'legacy-child'",
            [],
            |row| row.get(0),
        )
        .expect("rolled-back legacy child path");
    let legacy_fts_row: (String, String) = connection
        .query_row(
            "SELECT title, path FROM entity_search_fts
             WHERE mount_id = 'notion-main' AND remote_id = 'legacy-child'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("rolled-back legacy FTS row");
    let current_fts_row: (String, String) = connection
        .query_row(
            "SELECT title, path FROM search_documents_fts
             WHERE mount_id = 'notion-main' AND remote_id = 'legacy-child'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("rolled-back current FTS row");
    let partial_flattened_fts_rows: i64 = connection
        .query_row(
            "SELECT
                (SELECT COUNT(*) FROM entity_search_fts
                 WHERE remote_id = 'legacy-child' AND path = 'Legacy/page.md')
              + (SELECT COUNT(*) FROM search_documents_fts
                 WHERE remote_id = 'legacy-child' AND path = 'Legacy/page.md')",
            [],
            |row| row.get(0),
        )
        .expect("partial flattened FTS rows");
    assert_eq!(user_version, 20);
    assert_eq!(binding_table_count, 0);
    assert_eq!(stored_root, shared_root.to_string_lossy());
    assert_eq!(projection_version, 1);
    assert_eq!(retired_component_count, 1);
    assert_eq!(retired_root_count, 1);
    assert_eq!(legacy_child_path, "Workspace/Legacy/page.md");
    assert_eq!(
        legacy_fts_row,
        (
            "Rollback sentinel".to_string(),
            "Workspace/Legacy/page.md".to_string(),
        )
    );
    assert_eq!(current_fts_row, legacy_fts_row);
    assert_eq!(partial_flattened_fts_rows, 0);
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
            .expect("layout zero binding"),
        None
    );
    let connection = Connection::open(&restarted.db_path).expect("raw restarted state");
    let retired_state: (i64, i64, String) = connection
        .query_row(
            "SELECT
                (SELECT COUNT(*) FROM state_components
                 WHERE component_id = 'projection:notion_workspace_roots'),
                (SELECT COUNT(*) FROM entities
                 WHERE remote_id = 'notion-root:workspace'),
                (SELECT path FROM entities WHERE remote_id = 'legacy-child')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("retired state after successful restart");
    assert_eq!(retired_state, (0, 0, "Legacy/page.md".to_string()));
    let rebuilt_fts_rows: ((String, String), (String, String)) = (
        connection
            .query_row(
                "SELECT title, path FROM entity_search_fts
                 WHERE mount_id = 'notion-main' AND remote_id = 'legacy-child'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("rebuilt legacy FTS row"),
        connection
            .query_row(
                "SELECT title, path FROM search_documents_fts
                 WHERE mount_id = 'notion-main' AND remote_id = 'legacy-child'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("rebuilt current FTS row"),
    );
    assert_eq!(
        rebuilt_fts_rows,
        (
            ("Legacy child".to_string(), "Legacy/page.md".to_string()),
            ("Legacy child".to_string(), "Legacy/page.md".to_string()),
        )
    );
}

#[test]
fn legacy_unicode_target_collisions_remain_layout_zero_without_root_changes() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    store
        .save_mount(MountConfig::new(
            MountId::new("z-mount"),
            "notion",
            fixture.root.join("Locality/Straße"),
        ))
        .expect("save first legacy mount");
    store
        .save_mount(MountConfig::new(
            MountId::new("a-mount"),
            "notion",
            fixture.root.join("Locality/STRASSE"),
        ))
        .expect("save second legacy mount");
    downgrade_to_v20(&store.db_path);
    drop(store);

    let migrated = fixture.open();
    let records = migrated
        .load_workspace_bindings()
        .expect("load migrated bindings");
    assert!(records.is_empty());
    let mounts = migrated.load_mounts().expect("load unchanged mounts");
    assert_eq!(mounts[0].root, fixture.root.join("Locality/STRASSE"));
    assert_eq!(mounts[1].root, fixture.root.join("Locality/Straße"));
}

#[test]
fn legacy_roots_with_ambiguous_parents_all_remain_layout_zero() {
    let fixture = Fixture::new();
    let first_root = fixture.root.join("first/notion");
    let second_root = fixture.root.join("second/drive");
    let mut store = fixture.open();
    store
        .save_mount(MountConfig::new(
            MountId::new("notion"),
            "notion",
            &first_root,
        ))
        .expect("save first mount");
    store
        .save_mount(MountConfig::new(
            MountId::new("drive"),
            "google-docs",
            &second_root,
        ))
        .expect("save second mount");
    downgrade_to_v20(&store.db_path);
    drop(store);

    let migrated = fixture.open();
    assert!(
        migrated
            .load_workspace_bindings()
            .expect("load bindings")
            .is_empty()
    );
    let mounts = migrated.load_mounts().expect("load mounts");
    assert_eq!(mounts[0].root, second_root);
    assert_eq!(mounts[1].root, first_root);
}

#[test]
fn v26_invalid_synthesized_target_is_removed_without_changing_legacy_root() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    store
        .save_mount(fixture.mount_config(ProjectionMode::PlainFiles))
        .expect("save mount");
    let invalid_root = fixture.root.join("Locality/trailing.");
    let synthesized = WorkspaceBinding::new(MountTarget::new("mount-notion-main").unwrap());
    store
        .save_workspace_binding(WorkspaceBindingRecord::new(
            fixture.mount_id.clone(),
            synthesized.clone(),
        ))
        .expect("seed prerelease synthesized binding");
    let connection = Connection::open(&store.db_path).expect("raw v26 state");
    connection
        .execute(
            "UPDATE mounts SET root = ?1 WHERE mount_id = ?2",
            params![invalid_root.to_string_lossy(), fixture.mount_id.0.as_str()],
        )
        .expect("write invalid legacy root");
    connection
        .execute(
            "UPDATE workspace_bindings
             SET binding_json = ?1, target_collision_key = ?2
             WHERE mount_id = ?3",
            params![
                serde_json::to_string(&synthesized).unwrap(),
                synthesized.mount_target().collision_key(),
                fixture.mount_id.0.as_str()
            ],
        )
        .expect("write synthesized binding");
    mark_workspace_binding_v1(&connection);
    drop(connection);
    drop(store);

    let migrated = fixture.open();
    assert_eq!(
        migrated
            .get_workspace_binding(&fixture.mount_id)
            .expect("binding lookup"),
        None
    );
    assert_eq!(
        migrated
            .get_mount(&fixture.mount_id)
            .expect("mount lookup")
            .expect("mount")
            .root,
        invalid_root
    );
}

#[test]
fn v26_suffixed_collision_bindings_are_removed_without_renaming_roots() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    for (mount_id, target) in [("a-mount", "Alpha"), ("z-mount", "Beta")] {
        store
            .save_mount(MountConfig::new(
                MountId::new(mount_id),
                "notion",
                fixture.root.join("Locality").join(target),
            ))
            .expect("save mount");
    }
    let first = WorkspaceBinding::new(MountTarget::new("STRASSE").unwrap());
    let suffixed = WorkspaceBinding::new(MountTarget::new("Straße-2").unwrap());
    store
        .save_workspace_binding(WorkspaceBindingRecord::new(
            MountId::new("a-mount"),
            first.clone(),
        ))
        .expect("seed first prerelease binding");
    store
        .save_workspace_binding(WorkspaceBindingRecord::new(
            MountId::new("z-mount"),
            suffixed.clone(),
        ))
        .expect("seed suffixed prerelease binding");
    let connection = Connection::open(&store.db_path).expect("raw v26 state");
    for (mount_id, root, binding) in [
        ("a-mount", fixture.root.join("Locality/STRASSE"), first),
        ("z-mount", fixture.root.join("Locality/Straße"), suffixed),
    ] {
        connection
            .execute(
                "UPDATE mounts SET root = ?1 WHERE mount_id = ?2",
                params![root.to_string_lossy(), mount_id],
            )
            .expect("write colliding root");
        connection
            .execute(
                "UPDATE workspace_bindings
                 SET binding_json = ?1, target_collision_key = ?2
                 WHERE mount_id = ?3",
                params![
                    serde_json::to_string(&binding).unwrap(),
                    binding.mount_target().collision_key(),
                    mount_id
                ],
            )
            .expect("write v26 binding");
    }
    mark_workspace_binding_v1(&connection);
    drop(connection);
    drop(store);

    let migrated = fixture.open();
    assert!(
        migrated
            .load_workspace_bindings()
            .expect("load bindings")
            .is_empty()
    );
    let mounts = migrated.load_mounts().expect("load mounts");
    assert_eq!(mounts[0].root, fixture.root.join("Locality/STRASSE"));
    assert_eq!(mounts[1].root, fixture.root.join("Locality/Straße"));
}

#[test]
fn host_binding_resolver_has_explicit_macos_linux_and_windows_semantics() {
    for (platform, workspace_root, mount_root, target) in [
        (
            WorkspaceHostPlatform::Macos,
            "/Users/Ada/Library/CloudStorage/Locality",
            "/users/ada/library/cloudstorage/locality/Engineering",
            "Engineering",
        ),
        (
            WorkspaceHostPlatform::Linux,
            "/home/ada/Locality",
            "/home/ada/Locality/engineering",
            "engineering",
        ),
        (
            WorkspaceHostPlatform::Windows,
            r"C:\Users\Ada\Locality",
            r"c:/users/ada/locality/Engineering",
            "Engineering",
        ),
    ] {
        let mount = LegacyWorkspaceMount::new(MountId::new("mount-1"), mount_root);
        let plan = WorkspaceHostBindingResolver::new(platform)
            .plan_legacy_migration(Path::new(workspace_root), &[mount])
            .expect("cross-platform plan");
        assert_eq!(plan.layout1_bindings().len(), 1);
        assert_eq!(
            plan.layout1_bindings()[0].binding.mount_target().as_str(),
            target
        );
        assert!(plan.layout0_mounts().is_empty());
    }

    let case_mismatch =
        LegacyWorkspaceMount::new(MountId::new("mount-1"), "/home/Ada/locality/Engineering");
    let plan = WorkspaceHostBindingResolver::new(WorkspaceHostPlatform::Linux)
        .plan_legacy_migration(Path::new("/home/Ada/Locality"), &[case_mismatch])
        .expect("Linux plan");
    assert_eq!(
        plan.layout0_mounts()[0].reason,
        LegacyLayout0Reason::OutsideTrustedWorkspaceRoot
    );
}

#[test]
fn sandbox_publication_is_whole_root_and_rejects_platform_overlap() {
    for (platform, requested, active) in [
        (
            WorkspaceHostPlatform::Macos,
            "/Users/Ada/Library/CloudStorage/Locality/notion/sandbox",
            "/Users/Ada/Library/CloudStorage/Locality/notion",
        ),
        (
            WorkspaceHostPlatform::Linux,
            "/home/ada/Locality",
            "/home/ada/Locality/notion",
        ),
        (
            WorkspaceHostPlatform::Windows,
            r"c:\users\ada\locality\NOTION\sandbox",
            r"C:\Users\Ada\Locality\notion",
        ),
    ] {
        let active = [LegacyWorkspaceMount::new(MountId::new("notion"), active)];
        assert_eq!(
            WorkspaceHostBindingResolver::new(platform)
                .resolve_ephemeral_publication_root(Path::new(requested), &active),
            Err(WorkspaceHostBindingError::PublicationOverlapsActiveMount {
                mount_id: MountId::new("notion"),
            })
        );
    }

    let requested = Path::new("/mnt/locality");
    assert_eq!(
        WorkspaceHostBindingResolver::new(WorkspaceHostPlatform::Linux)
            .resolve_ephemeral_publication_root(requested, &[])
            .expect("ephemeral whole root"),
        requested
    );

    let windows = WorkspaceHostBindingResolver::new(WorkspaceHostPlatform::Windows);
    let active = [LegacyWorkspaceMount::new(
        MountId::new("notion"),
        r"C:\Users\Ada\Locality\notion",
    )];
    for alias in [
        r"\\?\C:\Users\Ada\Locality\notion\sandbox",
        r"C:\Users\Ada\Locality\notion.\sandbox",
    ] {
        assert_eq!(
            windows.resolve_ephemeral_publication_root(Path::new(alias), &active),
            Err(WorkspaceHostBindingError::PublicationOverlapsActiveMount {
                mount_id: MountId::new("notion"),
            }),
            "Windows alias {alias}"
        );
    }

    let unc_active = [LegacyWorkspaceMount::new(
        MountId::new("unc"),
        r"\\server\share\Locality\notion",
    )];
    assert_eq!(
        windows.resolve_ephemeral_publication_root(
            Path::new(r"\\?\unc\SERVER\SHARE\Locality\notion\sandbox"),
            &unc_active,
        ),
        Err(WorkspaceHostBindingError::PublicationOverlapsActiveMount {
            mount_id: MountId::new("unc"),
        })
    );
}

#[cfg(unix)]
#[test]
fn sandbox_publication_rejects_symlink_alias_of_active_mount() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let real_parent = fixture.root.join("real");
    let active_root = real_parent.join("notion");
    let alias_parent = fixture.root.join("alias");
    fs::create_dir_all(&active_root).expect("create active root");
    symlink(&real_parent, &alias_parent).expect("create parent alias");

    let active = [LegacyWorkspaceMount::new(
        MountId::new("notion"),
        &active_root,
    )];
    assert_eq!(
        WorkspaceHostBindingResolver::current().resolve_ephemeral_publication_root_on_current_host(
            &alias_parent.join("notion/sandbox"),
            &active,
        ),
        Err(WorkspaceHostBindingError::PublicationOverlapsActiveMount {
            mount_id: MountId::new("notion"),
        })
    );
}

#[test]
fn current_reader_rejects_newer_binding_metadata_without_touching_mount() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    let mount = fixture.mount_config(ProjectionMode::PlainFiles);
    store.save_mount(mount.clone()).expect("save mount");
    save_trusted_fixture_binding(&mut store, &fixture);
    let connection = Connection::open(&store.db_path).expect("raw connection");
    connection
        .execute(
            "UPDATE workspace_bindings
             SET binding_json = ?1
             WHERE mount_id = ?2",
            params![
                r#"{"binding_version":3,"layout_version":1,"mount_target":"notion-main"}"#,
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

fn mark_workspace_binding_v1(connection: &Connection) {
    connection
        .execute_batch(
            "UPDATE state_components
             SET version = 1, min_reader_version = 1,
                 data_json = '{\"format\":\"workspace_binding.v1\"}'
             WHERE component_id = 'durable:workspace_bindings';
             UPDATE state_components SET version = 26
             WHERE component_id = 'core:schema';
             PRAGMA user_version = 26;",
        )
        .expect("mark workspace binding v1 state");
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

fn mark_entity_search_component_v1(db_path: &Path) {
    let connection = Connection::open(db_path).expect("raw entity search downgrade connection");
    let changed = connection
        .execute(
            "UPDATE state_components
             SET version = 1, min_reader_version = 1
             WHERE component_id = 'cache:entity_search'",
            [],
        )
        .expect("mark entity search component v1");
    assert_eq!(changed, 1);
}

#[derive(Clone, Copy, Debug)]
enum ActiveMountState {
    Dirty,
    Journal,
    Projection,
}

fn assert_existing_binding_target_is_immutable<S>(
    mut store: S,
    fixture: &Fixture,
    active_state: ActiveMountState,
) where
    S: MountRepository + WorkspaceBindingRepository + EntityRepository + JournalRepository,
{
    let projection = match active_state {
        ActiveMountState::Projection => ProjectionMode::MacosFileProvider,
        ActiveMountState::Dirty | ActiveMountState::Journal => ProjectionMode::PlainFiles,
    };
    store
        .save_mount(fixture.mount_config(projection))
        .expect("save mount");
    save_trusted_fixture_binding(&mut store, fixture);
    match active_state {
        ActiveMountState::Dirty => store
            .save_entity(dirty_entity(&fixture.mount_id).with_hydration(HydrationState::Dirty))
            .expect("save dirty entity"),
        ActiveMountState::Journal => store
            .append_journal(applying_journal(&fixture.mount_id))
            .expect("save applying journal"),
        ActiveMountState::Projection => {}
    }

    let existing = store
        .get_workspace_binding(&fixture.mount_id)
        .expect("read existing binding")
        .expect("binding exists");
    store
        .save_workspace_binding(WorkspaceBindingRecord::new(
            fixture.mount_id.clone(),
            existing.clone(),
        ))
        .expect("exact binding replay");
    let requested = WorkspaceBinding::new(
        MountTarget::new(format!("moved-{active_state:?}"))
            .expect("valid replacement mount target"),
    );
    assert_eq!(
        store.save_workspace_binding(WorkspaceBindingRecord::new(
            fixture.mount_id.clone(),
            requested.clone(),
        )),
        Err(StoreError::WorkspaceBindingTargetImmutable {
            mount_id: fixture.mount_id.clone(),
            existing_target: existing.mount_target().as_str().to_string(),
            requested_target: requested.mount_target().as_str().to_string(),
        })
    );
    assert_eq!(
        store
            .get_workspace_binding(&fixture.mount_id)
            .expect("read unchanged binding"),
        Some(existing)
    );
}

fn save_trusted_fixture_binding<S>(store: &mut S, fixture: &Fixture)
where
    S: WorkspaceBindingRepository,
{
    let trusted_root = fixture
        .mount_root
        .parent()
        .expect("fixture mount has a trusted workspace root");
    let legacy = LegacyWorkspaceMount::new(fixture.mount_id.clone(), fixture.mount_root.clone());
    let plan = WorkspaceHostBindingResolver::current()
        .plan_legacy_migration(trusted_root, std::slice::from_ref(&legacy))
        .expect("trusted fixture migration plan");
    let binding = plan
        .layout1_bindings()
        .first()
        .expect("fixture is a valid trusted direct child")
        .clone();
    store
        .save_workspace_binding(binding)
        .expect("save trusted fixture binding");
}

fn fixture_atomic_workspace_binding(
    fixture: &Fixture,
) -> (WorkspaceHostBinding, WorkspaceBindingRecord) {
    let workspace_id = WorkspaceId::new("locality.workspace.remount-test").expect("workspace ID");
    let host = WorkspaceHostBinding::new(
        WorkspaceHostPlatform::current(),
        workspace_id.clone(),
        fixture.mount_root.parent().expect("workspace root"),
        WorkspaceProjectionIdentity::new("linux-fuse:remount-test").expect("projection identity"),
        1,
    )
    .expect("host binding");
    let record = WorkspaceBindingRecord::new(
        fixture.mount_id.clone(),
        WorkspaceBinding::for_workspace(
            workspace_id,
            MountTarget::new("notion-main").expect("target"),
        ),
    );
    (host, record)
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
