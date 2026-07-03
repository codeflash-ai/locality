use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use locality_core::freshness::{FreshnessTier, RemoteVersion};
use locality_core::hydration::HydrationReason;
use locality_core::journal::{
    JournalApplyEffect, JournalEntry, JournalPreimage, JournalStatus, PushId, PushOperationId,
};
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use locality_core::planner::{PushOperation, PushPlan};
use locality_core::shadow::{MarkdownBlockKind, ShadowDocument};
use locality_core::undo::{UndoOperation, UndoPlanStatus, plan_journal_undo};
use locality_store::{
    AutoSaveEnrollmentRecord, AutoSaveOrigin, AutoSaveRepository, AutoSaveState, ConnectionId,
    ConnectionRecord, ConnectionRepository, ConnectorProfileId, ConnectorProfileRecord,
    ConnectorProfileRepository, EntityRecord, EntityRepository, EntitySearchRepository,
    FreshnessStateRecord, FreshnessStateRepository, HydrationJobRecord, HydrationJobRepository,
    JournalRepository, MountConfig, MountLiveModeRecord, MountLiveModeRepository,
    MountLiveModeState, MountRepository, ProjectionMode, RemoteObservationRecord,
    RemoteObservationRepository, ShadowRepository, SqliteStateStore, StateCompatibilityIssue,
    StateCompatibilityStatus, StoreError, VirtualMutationKind, VirtualMutationRecord,
    VirtualMutationRepository,
};
use rusqlite::{Connection, params};
use serde_json::json;

const DEFAULT_NOTION_CAPABILITIES_JSON: &str = "{\"supports_block_updates\":true,\"supports_databases\":true,\"supports_oauth\":true,\"supports_remote_observation\":true,\"supports_lazy_child_enumeration\":true,\"supports_media_download\":true,\"supports_undo\":true,\"supports_batch_observation\":false}";

#[test]
fn sqlite_store_initializes_idempotently() {
    let fixture = SqliteFixture::new();

    let first = SqliteStateStore::open(fixture.state_root.clone()).expect("open first");
    let second = SqliteStateStore::open(fixture.state_root.clone()).expect("open second");
    let connection = Connection::open(&first.db_path).expect("raw connection");
    let user_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user version");
    let journal_mode: String = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("journal mode");

    assert!(first.db_path.exists());
    assert_eq!(first.db_path, second.db_path);
    assert_eq!(user_version, 15);
    assert_eq!(journal_mode, "wal");
}

#[test]
fn sqlite_store_seeds_state_compatibility_components() {
    let fixture = SqliteFixture::new();
    let store = fixture.open();
    let connection = Connection::open(&store.db_path).expect("raw connection");

    let components = query_state_components(&connection);

    assert_eq!(
        components,
        vec![
            (
                "auth:connections".to_string(),
                "secret_binding".to_string(),
                1,
                1,
                1,
                0
            ),
            (
                "cache:entity_search".to_string(),
                "rebuildable_cache".to_string(),
                1,
                1,
                0,
                1
            ),
            (
                "connector:notion".to_string(),
                "connector_state".to_string(),
                1,
                1,
                1,
                0
            ),
            ("core:schema".to_string(), "schema".to_string(), 15, 1, 1, 0),
            (
                "durable:auto_save".to_string(),
                "durable_json".to_string(),
                1,
                1,
                1,
                0
            ),
            (
                "durable:journals".to_string(),
                "durable_json".to_string(),
                1,
                1,
                1,
                0
            ),
            (
                "durable:live_mode".to_string(),
                "durable_json".to_string(),
                1,
                1,
                1,
                0
            ),
            (
                "durable:virtual_mutations".to_string(),
                "durable_json".to_string(),
                1,
                1,
                1,
                0,
            ),
            (
                "projection:linux_fuse".to_string(),
                "projection_layout".to_string(),
                2,
                1,
                1,
                0,
            ),
            (
                "projection:macos_file_provider".to_string(),
                "projection_layout".to_string(),
                1,
                1,
                1,
                0,
            ),
            (
                "projection:notion_workspace_roots".to_string(),
                "projection_layout".to_string(),
                1,
                1,
                1,
                0,
            ),
            (
                "projection:plain_files".to_string(),
                "projection_layout".to_string(),
                1,
                1,
                1,
                0,
            ),
            (
                "projection:windows_cloud_files".to_string(),
                "projection_layout".to_string(),
                2,
                1,
                1,
                0,
            ),
        ]
    );

    let report =
        SqliteStateStore::inspect_compatibility(fixture.state_root.clone()).expect("inspect state");
    assert_eq!(report.status, StateCompatibilityStatus::Ready);
    assert!(report.issues.is_empty());
}

#[test]
fn sqlite_store_repairs_missing_current_state_components() {
    let fixture = SqliteFixture::new();
    let store = fixture.open();
    let connection = Connection::open(&store.db_path).expect("raw connection");
    connection
        .execute(
            "DELETE FROM state_components WHERE component_id = 'durable:live_mode'",
            [],
        )
        .expect("delete live mode component");
    drop(connection);
    drop(store);

    let before =
        SqliteStateStore::inspect_compatibility(fixture.state_root.clone()).expect("inspect state");
    assert_eq!(before.status, StateCompatibilityStatus::Migratable);
    assert_eq!(
        before.issues,
        vec![StateCompatibilityIssue::MissingComponent {
            component_id: "durable:live_mode".to_string(),
        }]
    );

    fixture.open();

    let after =
        SqliteStateStore::inspect_compatibility(fixture.state_root.clone()).expect("inspect state");
    assert_eq!(after.status, StateCompatibilityStatus::Ready);
    assert!(after.issues.is_empty());
}

#[test]
fn sqlite_schema_snapshot_matches_v15_contract() {
    let fixture = SqliteFixture::new();
    let store = fixture.open();
    let connection = Connection::open(&store.db_path).expect("raw connection");

    assert_eq!(SqliteStateStore::current_schema_version(), 15);
    assert_eq!(
        schema_column_snapshot(&connection),
        "\
auto_save_enrollments: mount_id, path, remote_id, enabled, origin_json, state_json, last_reason, last_push_id, created_at, updated_at
connections: connection_id, profile_id, connector, display_name, account_label, workspace_id, workspace_name, auth_kind, secret_ref, scopes_json, capabilities_json, status, created_at, updated_at, expires_at
connector_profiles: profile_id, connector, display_name, auth_kind, scopes_json, capabilities_json, enabled_actions_json, connector_version, status, created_at, updated_at
connector_state: connector, scope_kind, scope_id, state_version, min_reader_version, state_json, updated_at
entities: mount_id, remote_id, kind_json, title, path, hydration_json, content_hash, remote_edited_at
entity_search_fts: mount_id, remote_id, title, path, observed_title, observed_path
freshness_states: mount_id, remote_id, tier_json, last_checked_at, next_check_at, last_opened_at, last_local_change_at, remote_hint_pending
hydration_jobs: mount_id, remote_id, path, target_state_json, reason_json, attempts, last_error
journals: push_id, mount_id, remote_ids_json, plan_json, preimages_json, apply_effects_json, status_json
mount_live_modes: mount_id, enabled, state_json, last_reason, last_run_at, created_at, updated_at
mounts: mount_id, connector, root, remote_root_id, read_only, projection_json, connection_id
projection_state: mount_id, projection, layout_version, min_reader_version, os_domain_id, root_item_id, repair_generation, state_json, updated_at
remote_observations: mount_id, remote_id, kind_json, title, parent_remote_id, projected_path, remote_version_json, observed_at, deleted, raw_metadata_json
shadows: mount_id, entity_id, frontmatter, body_hash, rendered_body, blocks_json
state_components: component_id, component_kind, version, min_reader_version, required, rebuildable, data_json, updated_at
state_migrations: migration_id, from_schema_version, to_schema_version, app_version, app_build_id, daemon_build_id, started_at, finished_at, status, error_json
virtual_mutations: mount_id, local_id, mutation_kind_json, target_remote_id, parent_remote_id, original_path, projected_path, title, content_path, created_at, updated_at"
    );
}

#[test]
fn sqlite_store_reports_v12_state_as_migratable_then_migrates() {
    let fixture = SqliteFixture::new();
    create_minimal_v12_state(&fixture);

    let before =
        SqliteStateStore::inspect_compatibility(fixture.state_root.clone()).expect("inspect v12");
    assert_eq!(before.status, StateCompatibilityStatus::Migratable);
    assert_eq!(
        before.issues,
        vec![StateCompatibilityIssue::OlderSchema {
            found: 12,
            current: 15,
        }]
    );

    let store = fixture.open();
    let connection = Connection::open(&store.db_path).expect("raw migrated connection");
    let user_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user version");
    let migration_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM state_migrations WHERE migration_id = 'schema-12-to-15'",
            [],
            |row| row.get(0),
        )
        .expect("migration row count");

    assert_eq!(user_version, 15);
    assert_eq!(migration_count, 1);
    assert_eq!(
        store.get_mount(&fixture.mount_id).expect("get mount"),
        Some(fixture.mount_config())
    );
    assert_eq!(
        store
            .find_entity_by_path(&fixture.mount_id, "Roadmap.md".as_ref())
            .expect("find entity")
            .map(|entity| entity.remote_id),
        Some(RemoteId::new("page-1"))
    );

    let after =
        SqliteStateStore::inspect_compatibility(fixture.state_root.clone()).expect("inspect v15");
    assert_eq!(after.status, StateCompatibilityStatus::Ready);
}

#[test]
fn sqlite_store_rejects_newer_schema_version() {
    let fixture = SqliteFixture::new();
    fs::create_dir_all(&fixture.state_root).expect("state root");
    let db_path = fixture.state_root.join("state.sqlite3");
    let connection = Connection::open(db_path).expect("raw connection");
    connection
        .execute_batch("PRAGMA user_version = 999;")
        .expect("set user version");

    let error = SqliteStateStore::open(fixture.state_root.clone()).expect_err("schema version");

    assert_eq!(
        error,
        StoreError::SchemaVersion {
            found: 999,
            supported: 15,
        }
    );
}

#[test]
fn sqlite_store_reports_newer_schema_as_needing_update() {
    let fixture = SqliteFixture::new();
    fs::create_dir_all(&fixture.state_root).expect("state root");
    let db_path = fixture.state_root.join("state.sqlite3");
    let connection = Connection::open(db_path).expect("raw connection");
    connection
        .execute_batch("PRAGMA user_version = 999;")
        .expect("set user version");

    let report =
        SqliteStateStore::inspect_compatibility(fixture.state_root.clone()).expect("inspect state");

    assert_eq!(report.status, StateCompatibilityStatus::NeedsUpdate);
    assert_eq!(
        report.issues,
        vec![StateCompatibilityIssue::NewerSchema {
            found: 999,
            supported: 15,
        }]
    );
}

#[test]
fn sqlite_store_migrates_linux_fuse_projection_layout_v1_mount_roots() {
    let fixture = SqliteFixture::new();
    let old_shared_root = fixture
        .state_root
        .parent()
        .expect("fixture root")
        .join("Locality");
    let mut store = fixture.open();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", &old_shared_root)
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save linux fuse mount");
    let connection = Connection::open(&store.db_path).expect("raw connection");
    connection
        .execute(
            "UPDATE state_components
             SET version = 1
             WHERE component_id = 'projection:linux_fuse'",
            [],
        )
        .expect("downgrade linux fuse component");
    drop(connection);
    drop(store);

    let reopened = fixture.open();
    let mounts = reopened.load_mounts().expect("load mounts");
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].root, old_shared_root.join("notion"));

    let connection = Connection::open(&reopened.db_path).expect("raw connection");
    let version: i64 = connection
        .query_row(
            "SELECT version
             FROM state_components
             WHERE component_id = 'projection:linux_fuse'",
            [],
            |row| row.get(0),
        )
        .expect("linux fuse component version");
    assert_eq!(version, 2);
}

#[test]
fn sqlite_store_does_not_rewrite_v2_linux_fuse_mount_point_roots() {
    let fixture = SqliteFixture::new();
    let mount_point_root = fixture
        .state_root
        .parent()
        .expect("fixture root")
        .join("Locality")
        .join("notion-main");
    let mut store = fixture.open();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", &mount_point_root)
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save linux fuse mount");
    drop(store);

    let reopened = fixture.open();
    let mounts = reopened.load_mounts().expect("load mounts");
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].root, mount_point_root);
}

#[test]
fn sqlite_store_v13_missing_linux_fuse_component_is_not_seeded_or_rewritten() {
    let fixture = SqliteFixture::new();
    let old_shared_root = fixture
        .state_root
        .parent()
        .expect("fixture root")
        .join("Locality");
    create_minimal_v13_linux_fuse_state(&fixture, &old_shared_root);
    let db_path = fixture.state_root.join("state.sqlite3");
    let connection = Connection::open(&db_path).expect("raw connection");
    insert_current_state_components_for_v13(&connection, None);
    drop(connection);

    let error = SqliteStateStore::open(fixture.state_root.clone()).expect_err("open blocked");
    assert!(matches!(error, StoreError::StateCompatibility(_)));

    let connection = Connection::open(db_path).expect("raw connection");
    let root: String = connection
        .query_row(
            "SELECT root
             FROM mounts
             WHERE mount_id = ?1",
            params![fixture.mount_id.0],
            |row| row.get(0),
        )
        .expect("mount root");
    let component_count: i64 = connection
        .query_row(
            "SELECT COUNT(*)
             FROM state_components
             WHERE component_id = 'projection:linux_fuse'",
            [],
            |row| row.get(0),
        )
        .expect("linux fuse component count");
    let migration_count = query_state_migration_count(&connection);
    assert_eq!(PathBuf::from(root), old_shared_root);
    assert_eq!(component_count, 0);
    assert_eq!(migration_count, 0);
}

#[test]
fn sqlite_store_v13_newer_linux_fuse_reader_is_not_seeded_or_rewritten() {
    let fixture = SqliteFixture::new();
    let old_shared_root = fixture
        .state_root
        .parent()
        .expect("fixture root")
        .join("Locality");
    create_minimal_v13_linux_fuse_state(&fixture, &old_shared_root);
    let db_path = fixture.state_root.join("state.sqlite3");
    let connection = Connection::open(&db_path).expect("raw connection");
    insert_current_state_components_for_v13(&connection, Some((1, 999)));
    drop(connection);

    let error = SqliteStateStore::open(fixture.state_root.clone()).expect_err("open blocked");
    assert!(matches!(error, StoreError::StateCompatibility(_)));

    let connection = Connection::open(db_path).expect("raw connection");
    let (root, min_reader_version): (String, i64) = connection
        .query_row(
            "SELECT mounts.root, state_components.min_reader_version
             FROM mounts
             JOIN state_components ON state_components.component_id = 'projection:linux_fuse'
             WHERE mounts.mount_id = ?1",
            params![fixture.mount_id.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("mount root and component reader");
    let migration_count = query_state_migration_count(&connection);
    assert_eq!(PathBuf::from(root), old_shared_root);
    assert_eq!(min_reader_version, 999);
    assert_eq!(migration_count, 0);
}

#[test]
fn sqlite_store_v13_non_linux_newer_reader_is_not_seeded_or_rewritten() {
    let fixture = SqliteFixture::new();
    let old_shared_root = fixture
        .state_root
        .parent()
        .expect("fixture root")
        .join("Locality");
    create_minimal_v13_linux_fuse_state(&fixture, &old_shared_root);
    let db_path = fixture.state_root.join("state.sqlite3");
    let connection = Connection::open(&db_path).expect("raw connection");
    insert_current_state_components_for_v13(&connection, Some((1, 1)));
    connection
        .execute(
            "UPDATE state_components
             SET min_reader_version = 999
             WHERE component_id = 'durable:journals'",
            [],
        )
        .expect("bump journals reader");
    drop(connection);

    let error = SqliteStateStore::open(fixture.state_root.clone()).expect_err("open blocked");
    assert!(matches!(error, StoreError::StateCompatibility(_)));

    let connection = Connection::open(db_path).expect("raw connection");
    let (root, min_reader_version): (String, i64) = connection
        .query_row(
            "SELECT mounts.root, state_components.min_reader_version
             FROM mounts
             JOIN state_components ON state_components.component_id = 'durable:journals'
             WHERE mounts.mount_id = ?1",
            params![fixture.mount_id.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("mount root and component reader");
    let migration_count = query_state_migration_count(&connection);
    assert_eq!(PathBuf::from(root), old_shared_root);
    assert_eq!(min_reader_version, 999);
    assert_eq!(migration_count, 0);
}

#[test]
fn sqlite_store_v13_unknown_required_component_leaves_no_schema_side_effects() {
    let fixture = SqliteFixture::new();
    let old_shared_root = fixture
        .state_root
        .parent()
        .expect("fixture root")
        .join("Locality");
    create_minimal_v13_linux_fuse_state(&fixture, &old_shared_root);
    let db_path = fixture.state_root.join("state.sqlite3");
    let connection = Connection::open(&db_path).expect("raw connection");
    insert_current_state_components_for_v13(&connection, Some((1, 1)));
    insert_state_component(
        &connection,
        "durable:future",
        "durable_json",
        1,
        1,
        true,
        false,
        "{}",
    );
    drop(connection);

    let error = SqliteStateStore::open(fixture.state_root.clone()).expect_err("open blocked");
    assert!(matches!(error, StoreError::StateCompatibility(_)));

    let connection = Connection::open(db_path).expect("raw connection");
    let root: String = connection
        .query_row(
            "SELECT root
             FROM mounts
             WHERE mount_id = ?1",
            params![fixture.mount_id.0],
            |row| row.get(0),
        )
        .expect("mount root");
    let user_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user version");
    assert_eq!(PathBuf::from(root), old_shared_root);
    assert_eq!(user_version, 13);
    assert_eq!(query_state_migration_count(&connection), 0);
    assert!(!sqlite_table_exists(&connection, "auto_save_enrollments"));
}

#[test]
fn sqlite_store_v13_valid_linux_fuse_v1_component_migrates_to_v2() {
    let fixture = SqliteFixture::new();
    let old_shared_root = fixture
        .state_root
        .parent()
        .expect("fixture root")
        .join("Locality");
    create_minimal_v13_linux_fuse_state(&fixture, &old_shared_root);
    let db_path = fixture.state_root.join("state.sqlite3");
    let connection = Connection::open(&db_path).expect("raw connection");
    insert_current_state_components_for_v13(&connection, Some((1, 1)));
    drop(connection);

    let store = fixture.open();
    let mounts = store.load_mounts().expect("load mounts");
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].root, old_shared_root.join("notion"));

    let connection = Connection::open(&store.db_path).expect("raw connection");
    let version: i64 = connection
        .query_row(
            "SELECT version
             FROM state_components
             WHERE component_id = 'projection:linux_fuse'",
            [],
            |row| row.get(0),
        )
        .expect("linux fuse component version");
    assert_eq!(version, 2);
    assert_eq!(query_state_migration_count(&connection), 1);
}

#[test]
fn sqlite_store_v14_missing_live_mode_component_migrates_to_v15() {
    let fixture = SqliteFixture::new();
    let mount_point_root = fixture
        .state_root
        .parent()
        .expect("fixture root")
        .join("Locality")
        .join("notion-main");
    create_minimal_v13_linux_fuse_state(&fixture, &mount_point_root);
    let db_path = fixture.state_root.join("state.sqlite3");
    let connection = Connection::open(&db_path).expect("raw connection");
    insert_current_state_components_for_v13(&connection, Some((2, 1)));
    connection
        .execute(
            "DELETE FROM state_components
             WHERE component_id = 'durable:live_mode'",
            [],
        )
        .expect("delete live mode component");
    connection
        .execute(
            "UPDATE state_components
             SET version = 14
             WHERE component_id = 'core:schema'",
            [],
        )
        .expect("mark v14 schema component");
    connection
        .execute_batch("PRAGMA user_version = 14;")
        .expect("mark v14 state");
    drop(connection);

    let store = fixture.open();
    let connection = Connection::open(&store.db_path).expect("raw migrated connection");
    let user_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user version");
    let component_version: i64 = connection
        .query_row(
            "SELECT version
             FROM state_components
             WHERE component_id = 'durable:live_mode'",
            [],
            |row| row.get(0),
        )
        .expect("live mode component version");

    assert_eq!(user_version, 15);
    assert!(sqlite_table_exists(&connection, "mount_live_modes"));
    assert_eq!(component_version, 1);
    assert_eq!(query_state_migration_count(&connection), 1);
}

#[test]
fn sqlite_store_pre_component_linux_fuse_mounts_migrate_from_v1_layout() {
    let fixture = SqliteFixture::new();
    let old_shared_root = fixture
        .state_root
        .parent()
        .expect("fixture root")
        .join("Locality");
    create_minimal_v12_linux_fuse_state(&fixture, &old_shared_root);

    let store = fixture.open();
    let mounts = store.load_mounts().expect("load mounts");
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].root, old_shared_root.join("notion"));

    let connection = Connection::open(&store.db_path).expect("raw connection");
    let version: i64 = connection
        .query_row(
            "SELECT version
             FROM state_components
             WHERE component_id = 'projection:linux_fuse'",
            [],
            |row| row.get(0),
        )
        .expect("linux fuse component version");
    assert_eq!(version, 2);
}

#[test]
fn sqlite_store_missing_linux_fuse_component_does_not_rewrite_mount_root() {
    let fixture = SqliteFixture::new();
    let old_shared_root = fixture
        .state_root
        .parent()
        .expect("fixture root")
        .join("Locality");
    let mut store = fixture.open();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", &old_shared_root)
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save linux fuse mount");
    let db_path = store.db_path.clone();
    let connection = Connection::open(&db_path).expect("raw connection");
    connection
        .execute(
            "DELETE FROM state_components
             WHERE component_id = 'projection:linux_fuse'",
            [],
        )
        .expect("delete linux fuse component");
    drop(connection);
    drop(store);

    let error = SqliteStateStore::open(fixture.state_root.clone()).expect_err("open blocked");
    assert!(matches!(error, StoreError::StateCompatibility(_)));

    let connection = Connection::open(db_path).expect("raw connection");
    let root: String = connection
        .query_row(
            "SELECT root
             FROM mounts
             WHERE mount_id = ?1",
            params![fixture.mount_id.0],
            |row| row.get(0),
        )
        .expect("mount root");
    assert_eq!(PathBuf::from(root), old_shared_root);
}

#[test]
fn sqlite_store_newer_linux_fuse_reader_requirement_does_not_rewrite_mount_root() {
    let fixture = SqliteFixture::new();
    let old_shared_root = fixture
        .state_root
        .parent()
        .expect("fixture root")
        .join("Locality");
    let mut store = fixture.open();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", &old_shared_root)
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save linux fuse mount");
    let db_path = store.db_path.clone();
    let connection = Connection::open(&db_path).expect("raw connection");
    connection
        .execute(
            "UPDATE state_components
             SET version = 1, min_reader_version = 999
             WHERE component_id = 'projection:linux_fuse'",
            [],
        )
        .expect("bump linux fuse minimum reader");
    drop(connection);
    drop(store);

    let error = SqliteStateStore::open(fixture.state_root.clone()).expect_err("open blocked");
    assert!(matches!(error, StoreError::StateCompatibility(_)));

    let connection = Connection::open(db_path).expect("raw connection");
    let root: String = connection
        .query_row(
            "SELECT root
             FROM mounts
             WHERE mount_id = ?1",
            params![fixture.mount_id.0],
            |row| row.get(0),
        )
        .expect("mount root");
    assert_eq!(PathBuf::from(root), old_shared_root);
}

#[test]
fn sqlite_store_linux_fuse_layout_migration_does_not_double_append_connector_directory() {
    let fixture = SqliteFixture::new();
    let already_migrated_root = fixture
        .state_root
        .parent()
        .expect("fixture root")
        .join("Locality")
        .join("notion");
    let mut store = fixture.open();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", &already_migrated_root)
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save linux fuse mount");
    let connection = Connection::open(&store.db_path).expect("raw connection");
    connection
        .execute(
            "UPDATE state_components
             SET version = 1
             WHERE component_id = 'projection:linux_fuse'",
            [],
        )
        .expect("downgrade linux fuse component");
    drop(connection);
    drop(store);

    let reopened = fixture.open();
    let mounts = reopened.load_mounts().expect("load mounts");
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].root, already_migrated_root);

    let connection = Connection::open(&reopened.db_path).expect("raw connection");
    let version: i64 = connection
        .query_row(
            "SELECT version
             FROM state_components
             WHERE component_id = 'projection:linux_fuse'",
            [],
            |row| row.get(0),
        )
        .expect("linux fuse component version");
    assert_eq!(version, 2);
}

#[test]
fn sqlite_store_linux_fuse_layout_migration_uses_frozen_connector_directory_names() {
    let fixture = SqliteFixture::new();
    let root = fixture.state_root.parent().expect("fixture root");
    let google_docs_root = root.join("Locality");
    let google_docs_alt_root = root.join("LocalityAlt");
    let mut store = fixture.open();
    store
        .save_mount(
            MountConfig::new(
                MountId::new("google-docs-main"),
                "Google Docs",
                &google_docs_root,
            )
            .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save google docs mount");
    store
        .save_mount(
            MountConfig::new(
                MountId::new("google-docs-alt"),
                "google_docs",
                &google_docs_alt_root,
            )
            .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save google docs alt mount");
    let connection = Connection::open(&store.db_path).expect("raw connection");
    connection
        .execute(
            "UPDATE state_components
             SET version = 1
             WHERE component_id = 'projection:linux_fuse'",
            [],
        )
        .expect("downgrade linux fuse component");
    drop(connection);
    drop(store);

    let reopened = fixture.open();
    let mut roots = reopened
        .load_mounts()
        .expect("load mounts")
        .into_iter()
        .map(|mount| (mount.mount_id.0, mount.root))
        .collect::<Vec<_>>();
    roots.sort_by(|left, right| left.0.cmp(&right.0));

    assert_eq!(
        roots,
        vec![
            (
                "google-docs-alt".to_string(),
                google_docs_alt_root.join("google-docs")
            ),
            (
                "google-docs-main".to_string(),
                google_docs_root.join("googledocs")
            ),
        ]
    );
}

#[test]
fn sqlite_store_migrates_windows_cloud_files_projection_layout_v1_mount_roots() {
    let fixture = SqliteFixture::new();
    let old_shared_root = fixture
        .state_root
        .parent()
        .expect("fixture root")
        .join("Locality");
    let mut store = fixture.open();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", &old_shared_root)
                .projection(ProjectionMode::WindowsCloudFiles),
        )
        .expect("save windows cloud files mount");
    let connection = Connection::open(&store.db_path).expect("raw connection");
    connection
        .execute(
            "UPDATE state_components
             SET version = 1
             WHERE component_id = 'projection:windows_cloud_files'",
            [],
        )
        .expect("downgrade windows cloud files component");
    drop(connection);
    drop(store);

    let reopened = fixture.open();
    let mounts = reopened.load_mounts().expect("load mounts");
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].root, old_shared_root.join("notion"));

    let connection = Connection::open(&reopened.db_path).expect("raw connection");
    let version: i64 = connection
        .query_row(
            "SELECT version
             FROM state_components
             WHERE component_id = 'projection:windows_cloud_files'",
            [],
            |row| row.get(0),
        )
        .expect("windows cloud files component version");
    assert_eq!(version, 2);
}

#[test]
fn sqlite_store_does_not_rewrite_v2_windows_cloud_files_mount_point_roots() {
    let fixture = SqliteFixture::new();
    let mount_point_root = fixture
        .state_root
        .parent()
        .expect("fixture root")
        .join("Locality")
        .join("notion-main");
    let mut store = fixture.open();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", &mount_point_root)
                .projection(ProjectionMode::WindowsCloudFiles),
        )
        .expect("save windows cloud files mount");
    drop(store);

    let reopened = fixture.open();
    let mounts = reopened.load_mounts().expect("load mounts");
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].root, mount_point_root);
}

#[test]
fn sqlite_store_missing_windows_cloud_files_component_does_not_rewrite_mount_point_roots() {
    let fixture = SqliteFixture::new();
    let mount_point_root = fixture
        .state_root
        .parent()
        .expect("fixture root")
        .join("Locality")
        .join("notion-main");
    let mut store = fixture.open();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", &mount_point_root)
                .projection(ProjectionMode::WindowsCloudFiles),
        )
        .expect("save windows cloud files mount");
    let db_path = store.db_path.clone();
    let connection = Connection::open(&db_path).expect("raw connection");
    connection
        .execute(
            "DELETE FROM state_components
             WHERE component_id = 'projection:windows_cloud_files'",
            [],
        )
        .expect("delete windows cloud files component");
    drop(connection);
    drop(store);

    let reopened = fixture.open();
    let mounts = reopened.load_mounts().expect("load mounts");
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].root, mount_point_root);

    let connection = Connection::open(&reopened.db_path).expect("raw connection");
    let version: i64 = connection
        .query_row(
            "SELECT version
             FROM state_components
             WHERE component_id = 'projection:windows_cloud_files'",
            [],
            |row| row.get(0),
        )
        .expect("windows cloud files component version");
    assert_eq!(version, 2);
}

#[test]
fn sqlite_store_newer_windows_cloud_files_reader_requirement_does_not_rewrite_mount_root() {
    let fixture = SqliteFixture::new();
    let old_shared_root = fixture
        .state_root
        .parent()
        .expect("fixture root")
        .join("Locality");
    let mut store = fixture.open();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", &old_shared_root)
                .projection(ProjectionMode::WindowsCloudFiles),
        )
        .expect("save windows cloud files mount");
    let db_path = store.db_path.clone();
    let connection = Connection::open(&db_path).expect("raw connection");
    connection
        .execute(
            "UPDATE state_components
             SET version = 1, min_reader_version = 999
             WHERE component_id = 'projection:windows_cloud_files'",
            [],
        )
        .expect("bump windows cloud files minimum reader");
    drop(connection);
    drop(store);

    let error = SqliteStateStore::open(fixture.state_root.clone()).expect_err("open blocked");
    assert!(matches!(error, StoreError::StateCompatibility(_)));

    let connection = Connection::open(db_path).expect("raw connection");
    let root: String = connection
        .query_row(
            "SELECT root
             FROM mounts
             WHERE mount_id = ?1",
            params![fixture.mount_id.0],
            |row| row.get(0),
        )
        .expect("mount root");
    assert_eq!(PathBuf::from(root), old_shared_root);
}

#[test]
fn sqlite_store_repairs_missing_notion_workspace_roots_component() {
    let fixture = SqliteFixture::new();
    let db_path = seed_notion_workspace_roots_legacy_state(&fixture);
    let connection = Connection::open(&db_path).expect("raw connection");
    connection
        .execute(
            "DELETE FROM state_components
             WHERE component_id = 'projection:notion_workspace_roots'",
            [],
        )
        .expect("delete notion workspace roots component");
    drop(connection);

    let before =
        SqliteStateStore::inspect_compatibility(fixture.state_root.clone()).expect("inspect state");
    assert_eq!(before.status, StateCompatibilityStatus::Migratable);
    assert_eq!(
        before.issues,
        vec![StateCompatibilityIssue::MissingComponent {
            component_id: "projection:notion_workspace_roots".to_string(),
        }]
    );

    let reopened = fixture.open();

    let after =
        SqliteStateStore::inspect_compatibility(fixture.state_root.clone()).expect("inspect state");
    assert_eq!(after.status, StateCompatibilityStatus::Ready);
    assert!(after.issues.is_empty());

    let connection = Connection::open(&reopened.db_path).expect("raw reopened connection");
    let synthetic_roots = query_entities_for_mount(&connection, &fixture.mount_id);
    assert_eq!(
        synthetic_roots,
        vec![
            (
                "notion-root:private".to_string(),
                "directory".to_string(),
                "Private".to_string(),
                "Private".to_string(),
                "virtual".to_string(),
            ),
            (
                "notion-root:workspace".to_string(),
                "directory".to_string(),
                "Workspace".to_string(),
                "Workspace".to_string(),
                "virtual".to_string(),
            ),
            (
                "already-workspace".to_string(),
                "page".to_string(),
                "Already".to_string(),
                "Workspace/Already/page.md".to_string(),
                "stub".to_string(),
            ),
            (
                "workspace-page".to_string(),
                "page".to_string(),
                "Roadmap".to_string(),
                "Workspace/Roadmap/page.md".to_string(),
                "stub".to_string(),
            ),
        ]
    );

    let root_page_path: String = connection
        .query_row(
            "SELECT path
             FROM entities
             WHERE mount_id = 'notion-root-page'
               AND remote_id = 'root-child'",
            [],
            |row| row.get(0),
        )
        .expect("root-page entity path");
    let non_notion_path: String = connection
        .query_row(
            "SELECT path
             FROM entities
             WHERE mount_id = 'google-docs-main'
               AND remote_id = 'doc-1'",
            [],
            |row| row.get(0),
        )
        .expect("non-notion entity path");
    assert_eq!(root_page_path, "Root Child/page.md");
    assert_eq!(non_notion_path, "Doc/page.md");

    let observation_path: String = connection
        .query_row(
            "SELECT projected_path
             FROM remote_observations
             WHERE mount_id = ?1
               AND remote_id = 'workspace-page'",
            params![fixture.mount_id.0.as_str()],
            |row| row.get(0),
        )
        .expect("remote observation path");
    let hydration_path: String = connection
        .query_row(
            "SELECT path
             FROM hydration_jobs
             WHERE mount_id = ?1
               AND remote_id = 'workspace-page'",
            params![fixture.mount_id.0.as_str()],
            |row| row.get(0),
        )
        .expect("hydration job path");
    let mutation_paths: (String, String, String) = connection
        .query_row(
            "SELECT original_path, projected_path, content_path
             FROM virtual_mutations
             WHERE mount_id = ?1
               AND local_id = 'local:rename'",
            params![fixture.mount_id.0.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("virtual mutation paths");
    let auto_save_path: String = connection
        .query_row(
            "SELECT path
             FROM auto_save_enrollments
             WHERE mount_id = ?1
               AND remote_id = 'workspace-page'",
            params![fixture.mount_id.0.as_str()],
            |row| row.get(0),
        )
        .expect("auto-save path");
    let indexed_path: String = connection
        .query_row(
            "SELECT path
             FROM entity_search_fts
             WHERE mount_id = ?1
               AND remote_id = 'workspace-page'",
            params![fixture.mount_id.0.as_str()],
            |row| row.get(0),
        )
        .expect("indexed path");
    assert_eq!(observation_path, "Workspace/Roadmap/page.md");
    assert_eq!(hydration_path, "Workspace/Roadmap/page.md");
    assert_eq!(
        mutation_paths,
        (
            "Workspace/Roadmap/page.md".to_string(),
            "Workspace/Roadmap Renamed/page.md".to_string(),
            "Workspace/Roadmap Renamed/page.md".to_string(),
        )
    );
    assert_eq!(auto_save_path, "Workspace/Roadmap/page.md");
    assert_eq!(indexed_path, "Workspace/Roadmap/page.md");

    let component_version: i64 = connection
        .query_row(
            "SELECT version
             FROM state_components
             WHERE component_id = 'projection:notion_workspace_roots'",
            [],
            |row| row.get(0),
        )
        .expect("component version");
    let migration_count: i64 = connection
        .query_row(
            "SELECT COUNT(*)
             FROM state_migrations
             WHERE migration_id = 'component-projection-notion_workspace_roots-0-to-1'",
            [],
            |row| row.get(0),
        )
        .expect("component migration count");
    assert_eq!(component_version, 1);
    assert_eq!(migration_count, 1);
}

#[test]
fn sqlite_store_repairs_exact_notion_workspace_root_name_collisions() {
    let fixture = SqliteFixture::new();
    let db_path = seed_notion_workspace_root_name_collision_state(&fixture);
    let connection = Connection::open(&db_path).expect("raw connection");
    connection
        .execute(
            "DELETE FROM state_components
             WHERE component_id = 'projection:notion_workspace_roots'",
            [],
        )
        .expect("delete notion workspace roots component");
    drop(connection);

    let reopened = fixture.open();
    let connection = Connection::open(&reopened.db_path).expect("raw reopened connection");

    assert_eq!(
        query_entities_for_mount(&connection, &fixture.mount_id),
        vec![
            (
                "notion-root:private".to_string(),
                "directory".to_string(),
                "Private".to_string(),
                "Private".to_string(),
                "virtual".to_string(),
            ),
            (
                "notion-root:workspace".to_string(),
                "directory".to_string(),
                "Workspace".to_string(),
                "Workspace".to_string(),
                "virtual".to_string(),
            ),
            (
                "private-database".to_string(),
                "database".to_string(),
                "Private Database".to_string(),
                "Workspace/Private".to_string(),
                "stub".to_string(),
            ),
            (
                "workspace-database".to_string(),
                "database".to_string(),
                "Workspace Database".to_string(),
                "Workspace/Workspace".to_string(),
                "stub".to_string(),
            ),
        ]
    );
}

#[test]
fn sqlite_store_repairs_page_shaped_notion_workspace_root_name_collisions() {
    let fixture = SqliteFixture::new();
    let db_path = seed_notion_workspace_root_page_name_collision_state(&fixture);
    let connection = Connection::open(&db_path).expect("raw connection");
    connection
        .execute(
            "DELETE FROM state_components
             WHERE component_id = 'projection:notion_workspace_roots'",
            [],
        )
        .expect("delete notion workspace roots component");
    drop(connection);

    let reopened = fixture.open();
    let connection = Connection::open(&reopened.db_path).expect("raw reopened connection");

    assert_eq!(
        query_entities_for_mount(&connection, &fixture.mount_id),
        vec![
            (
                "notion-root:private".to_string(),
                "directory".to_string(),
                "Private".to_string(),
                "Private".to_string(),
                "virtual".to_string(),
            ),
            (
                "notion-root:workspace".to_string(),
                "directory".to_string(),
                "Workspace".to_string(),
                "Workspace".to_string(),
                "virtual".to_string(),
            ),
            (
                "already-workspace-child".to_string(),
                "page".to_string(),
                "Already Child".to_string(),
                "Workspace/Already/Child/page.md".to_string(),
                "stub".to_string(),
            ),
            (
                "already-workspace".to_string(),
                "page".to_string(),
                "Already".to_string(),
                "Workspace/Already/page.md".to_string(),
                "stub".to_string(),
            ),
            (
                "private-page-child".to_string(),
                "page".to_string(),
                "Private Child".to_string(),
                "Workspace/Private/Child/page.md".to_string(),
                "stub".to_string(),
            ),
            (
                "private-page".to_string(),
                "page".to_string(),
                "Private".to_string(),
                "Workspace/Private/page.md".to_string(),
                "stub".to_string(),
            ),
            (
                "workspace-page-child".to_string(),
                "page".to_string(),
                "Workspace Child".to_string(),
                "Workspace/Workspace/Child/page.md".to_string(),
                "stub".to_string(),
            ),
            (
                "workspace-page".to_string(),
                "page".to_string(),
                "Workspace".to_string(),
                "Workspace/Workspace/page.md".to_string(),
                "stub".to_string(),
            ),
        ]
    );
    assert_eq!(
        query_paths_by_remote_id(
            &connection,
            "remote_observations",
            "projected_path",
            &fixture.mount_id
        ),
        vec![
            (
                "already-workspace".to_string(),
                "Workspace/Already/page.md".to_string()
            ),
            (
                "already-workspace-child".to_string(),
                "Workspace/Already/Child/page.md".to_string()
            ),
            (
                "private-page".to_string(),
                "Workspace/Private/page.md".to_string()
            ),
            (
                "private-page-child".to_string(),
                "Workspace/Private/Child/page.md".to_string()
            ),
            (
                "workspace-page".to_string(),
                "Workspace/Workspace/page.md".to_string()
            ),
            (
                "workspace-page-child".to_string(),
                "Workspace/Workspace/Child/page.md".to_string()
            ),
        ]
    );
    assert_eq!(
        query_paths_by_remote_id(&connection, "hydration_jobs", "path", &fixture.mount_id),
        vec![
            (
                "already-workspace".to_string(),
                "Workspace/Already/page.md".to_string()
            ),
            (
                "already-workspace-child".to_string(),
                "Workspace/Already/Child/page.md".to_string()
            ),
            (
                "private-page".to_string(),
                "Workspace/Private/page.md".to_string()
            ),
            (
                "private-page-child".to_string(),
                "Workspace/Private/Child/page.md".to_string()
            ),
            (
                "workspace-page".to_string(),
                "Workspace/Workspace/page.md".to_string()
            ),
            (
                "workspace-page-child".to_string(),
                "Workspace/Workspace/Child/page.md".to_string()
            ),
        ]
    );
    assert_eq!(
        query_paths_by_remote_id(
            &connection,
            "auto_save_enrollments",
            "path",
            &fixture.mount_id
        ),
        vec![
            (
                "already-workspace".to_string(),
                "Workspace/Already/page.md".to_string()
            ),
            (
                "already-workspace-child".to_string(),
                "Workspace/Already/Child/page.md".to_string()
            ),
            (
                "private-page".to_string(),
                "Workspace/Private/page.md".to_string()
            ),
            (
                "private-page-child".to_string(),
                "Workspace/Private/Child/page.md".to_string()
            ),
            (
                "workspace-page".to_string(),
                "Workspace/Workspace/page.md".to_string()
            ),
            (
                "workspace-page-child".to_string(),
                "Workspace/Workspace/Child/page.md".to_string()
            ),
        ]
    );
    assert_eq!(
        query_virtual_mutation_paths(&connection, &fixture.mount_id),
        vec![
            (
                "local:already-workspace".to_string(),
                Some("Workspace/Already/page.md".to_string()),
                "Workspace/Already/page.md".to_string(),
                Some("Workspace/Already/page.md".to_string()),
            ),
            (
                "local:already-workspace-child".to_string(),
                Some("Workspace/Already/Child/page.md".to_string()),
                "Workspace/Already/Child/page.md".to_string(),
                Some("Workspace/Already/Child/page.md".to_string()),
            ),
            (
                "local:private-page".to_string(),
                Some("Workspace/Private/page.md".to_string()),
                "Workspace/Private/page.md".to_string(),
                Some("Workspace/Private/page.md".to_string()),
            ),
            (
                "local:private-page-child".to_string(),
                Some("Workspace/Private/Child/page.md".to_string()),
                "Workspace/Private/Child/page.md".to_string(),
                Some("Workspace/Private/Child/page.md".to_string()),
            ),
            (
                "local:workspace-page".to_string(),
                Some("Workspace/Workspace/page.md".to_string()),
                "Workspace/Workspace/page.md".to_string(),
                Some("Workspace/Workspace/page.md".to_string()),
            ),
            (
                "local:workspace-page-child".to_string(),
                Some("Workspace/Workspace/Child/page.md".to_string()),
                "Workspace/Workspace/Child/page.md".to_string(),
                Some("Workspace/Workspace/Child/page.md".to_string()),
            ),
        ]
    );
}

#[test]
fn sqlite_store_blocks_newer_notion_workspace_roots_component_without_rewriting() {
    let fixture = SqliteFixture::new();
    let db_path = seed_notion_workspace_roots_legacy_state(&fixture);
    let connection = Connection::open(&db_path).expect("raw connection");
    connection
        .execute(
            "UPDATE state_components
             SET version = 999
             WHERE component_id = 'projection:notion_workspace_roots'",
            [],
        )
        .expect("bump notion workspace roots component");
    drop(connection);

    let report =
        SqliteStateStore::inspect_compatibility(fixture.state_root.clone()).expect("inspect state");
    assert_eq!(report.status, StateCompatibilityStatus::NeedsUpdate);
    assert_eq!(
        report.issues,
        vec![StateCompatibilityIssue::NewerComponent {
            component_id: "projection:notion_workspace_roots".to_string(),
            found: 999,
            supported: 1,
        }]
    );

    let error = SqliteStateStore::open(fixture.state_root.clone()).expect_err("open blocked");
    assert!(matches!(error, StoreError::StateCompatibility(_)));

    assert_notion_workspace_roots_state_not_rewritten(&db_path, &fixture.mount_id);
}

#[test]
fn sqlite_store_blocks_newer_notion_workspace_roots_reader_without_rewriting() {
    let fixture = SqliteFixture::new();
    let db_path = seed_notion_workspace_roots_legacy_state(&fixture);
    let connection = Connection::open(&db_path).expect("raw connection");
    connection
        .execute(
            "UPDATE state_components
             SET min_reader_version = 999
             WHERE component_id = 'projection:notion_workspace_roots'",
            [],
        )
        .expect("bump notion workspace roots reader");
    drop(connection);

    let report =
        SqliteStateStore::inspect_compatibility(fixture.state_root.clone()).expect("inspect state");
    assert_eq!(report.status, StateCompatibilityStatus::NeedsUpdate);
    assert_eq!(
        report.issues,
        vec![StateCompatibilityIssue::ComponentRequiresNewerReader {
            component_id: "projection:notion_workspace_roots".to_string(),
            min_reader_version: 999,
            supported: 1,
        }]
    );

    let error = SqliteStateStore::open(fixture.state_root.clone()).expect_err("open blocked");
    assert!(matches!(error, StoreError::StateCompatibility(_)));

    assert_notion_workspace_roots_state_not_rewritten(&db_path, &fixture.mount_id);
}

#[test]
fn sqlite_store_blocks_newer_component_versions() {
    let fixture = SqliteFixture::new();
    let store = fixture.open();
    let connection = Connection::open(&store.db_path).expect("raw connection");
    connection
        .execute(
            "UPDATE state_components SET version = 999 WHERE component_id = 'connector:notion'",
            [],
        )
        .expect("bump component");
    drop(connection);
    drop(store);

    let report =
        SqliteStateStore::inspect_compatibility(fixture.state_root.clone()).expect("inspect state");
    assert_eq!(report.status, StateCompatibilityStatus::NeedsUpdate);
    assert_eq!(
        report.issues,
        vec![StateCompatibilityIssue::NewerComponent {
            component_id: "connector:notion".to_string(),
            found: 999,
            supported: 1,
        }]
    );

    let error = SqliteStateStore::open(fixture.state_root.clone()).expect_err("open blocked");
    assert!(matches!(error, StoreError::StateCompatibility(_)));
}

#[test]
fn sqlite_store_blocks_components_that_require_newer_readers() {
    let fixture = SqliteFixture::new();
    let store = fixture.open();
    let connection = Connection::open(&store.db_path).expect("raw connection");
    connection
        .execute(
            "UPDATE state_components
             SET min_reader_version = 999
             WHERE component_id = 'durable:journals'",
            [],
        )
        .expect("bump minimum reader");
    drop(connection);
    drop(store);

    let report =
        SqliteStateStore::inspect_compatibility(fixture.state_root.clone()).expect("inspect state");
    assert_eq!(report.status, StateCompatibilityStatus::NeedsUpdate);
    assert_eq!(
        report.issues,
        vec![StateCompatibilityIssue::ComponentRequiresNewerReader {
            component_id: "durable:journals".to_string(),
            min_reader_version: 999,
            supported: 1,
        }]
    );

    let error = SqliteStateStore::open(fixture.state_root.clone()).expect_err("open blocked");
    assert!(matches!(error, StoreError::StateCompatibility(_)));
}

#[test]
fn sqlite_store_blocks_unknown_required_components() {
    let fixture = SqliteFixture::new();
    let store = fixture.open();
    let connection = Connection::open(&store.db_path).expect("raw connection");
    connection
        .execute(
            "INSERT INTO state_components (
                component_id, component_kind, version, min_reader_version,
                required, rebuildable, data_json, updated_at
             )
             VALUES ('connector:future', 'connector_state', 1, 1, 1, 0, '{}', '0')",
            [],
        )
        .expect("insert unknown component");
    drop(connection);
    drop(store);

    let report =
        SqliteStateStore::inspect_compatibility(fixture.state_root.clone()).expect("inspect state");
    assert_eq!(report.status, StateCompatibilityStatus::NeedsUpdate);
    assert_eq!(
        report.issues,
        vec![StateCompatibilityIssue::UnknownRequiredComponent {
            component_id: "connector:future".to_string(),
            version: 1,
        }]
    );
}

#[test]
fn sqlite_store_allows_unknown_rebuildable_components() {
    let fixture = SqliteFixture::new();
    let store = fixture.open();
    let connection = Connection::open(&store.db_path).expect("raw connection");
    connection
        .execute(
            "INSERT INTO state_components (
                component_id, component_kind, version, min_reader_version,
                required, rebuildable, data_json, updated_at
             )
             VALUES ('cache:future', 'rebuildable_cache', 1, 1, 0, 1, '{}', '0')",
            [],
        )
        .expect("insert unknown cache");
    drop(connection);

    let report =
        SqliteStateStore::inspect_compatibility(fixture.state_root.clone()).expect("inspect state");
    assert_eq!(report.status, StateCompatibilityStatus::Ready);
    assert!(report.issues.is_empty());
    SqliteStateStore::open(fixture.state_root.clone()).expect("open with optional cache");
}

#[test]
fn persisted_json_uses_stable_snake_case_names() {
    assert_eq!(
        serde_json::to_string(&HydrationState::Hydrated).expect("hydration json"),
        "\"hydrated\""
    );
    assert_eq!(
        serde_json::to_string(&HydrationReason::FileOpen).expect("hydration reason json"),
        "\"file_open\""
    );
    assert_eq!(
        serde_json::to_string(&HydrationReason::RemoteFastForward).expect("hydration reason json"),
        "\"remote_fast_forward\""
    );
    assert_eq!(
        serde_json::to_string(&HydrationReason::LiveModeRemoteFastForward)
            .expect("hydration reason json"),
        "\"live_mode_remote_fast_forward\""
    );
    assert_eq!(
        serde_json::to_string(&ProjectionMode::MacosFileProvider).expect("projection json"),
        "\"macos_file_provider\""
    );
    assert_eq!(
        serde_json::to_string(&ProjectionMode::LinuxFuse).expect("projection json"),
        "\"linux_fuse\""
    );
    assert_eq!(
        serde_json::to_string(&ProjectionMode::WindowsCloudFiles).expect("projection json"),
        "\"windows_cloud_files\""
    );
    assert_eq!(
        serde_json::to_string(&FreshnessTier::Hot).expect("freshness tier json"),
        "\"hot\""
    );
    assert_eq!(
        serde_json::to_string(&MarkdownBlockKind::Paragraph).expect("block kind json"),
        "{\"kind\":\"paragraph\"}"
    );
    assert_eq!(
        serde_json::to_string(&PushOperation::ArchiveBlock {
            block_id: RemoteId::new("block-1"),
        })
        .expect("operation json"),
        "{\"type\":\"archive_block\",\"block_id\":\"block-1\"}"
    );
}

#[test]
fn mount_entity_and_shadow_round_trip_after_reopen() {
    let fixture = SqliteFixture::new();
    let mut store = fixture.open();
    let mount = fixture.mount_config();
    let entity = entity_record();
    let mut shadow = shadow_document("# Roadmap\n\nSame paragraph.");
    shadow.blocks[1].native_kind = Some("paragraph".to_string());

    store.save_mount(mount.clone()).expect("save mount");
    store.save_entity(entity.clone()).expect("save entity");
    store
        .save_shadow(&fixture.mount_id, shadow.clone())
        .expect("save shadow");
    drop(store);

    let reopened = fixture.open();

    assert_eq!(
        reopened.get_mount(&fixture.mount_id).expect("get mount"),
        Some(mount)
    );
    assert_eq!(
        reopened
            .find_entity_by_path(&fixture.mount_id, "Roadmap.md".as_ref())
            .expect("find entity"),
        Some(entity)
    );
    assert_eq!(
        reopened
            .load_shadow(&fixture.mount_id, &RemoteId::new("page-1"))
            .expect("load shadow"),
        shadow
    );
}

#[test]
fn entity_search_candidates_use_sqlite_index() {
    let fixture = SqliteFixture::new();
    let mut store = fixture.open();
    store
        .save_mount(fixture.mount_config())
        .expect("save mount");
    store.save_entity(entity_record()).expect("save entity");

    let title_matches = store
        .list_entity_search_candidates(&fixture.mount_id, "road", None)
        .expect("search title")
        .expect("sqlite search");
    assert_eq!(title_matches.len(), 1);
    assert_eq!(title_matches[0].entity.remote_id, RemoteId::new("page-1"));
    assert!(title_matches[0].observation.is_none());

    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("page-2"),
            EntityKind::Page,
            "1:1 Notes",
            "Meetings/1-1 Notes/page.md",
        ))
        .expect("save numbered entity");
    let numeric_matches = store
        .list_entity_search_candidates(&fixture.mount_id, "1", None)
        .expect("search numeric title")
        .expect("sqlite search");
    assert_eq!(numeric_matches.len(), 1);
    assert_eq!(numeric_matches[0].entity.remote_id, RemoteId::new("page-2"));

    store
        .save_remote_observation(RemoteObservationRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("page-1"),
            EntityKind::Page,
            "Launch Plan",
            "Planning/Launch Plan.md",
            "2026-06-16T00:00:00Z",
        ))
        .expect("save observation");
    let observed_matches = store
        .list_entity_search_candidates(&fixture.mount_id, "launch", None)
        .expect("search observed title")
        .expect("sqlite search");
    assert_eq!(observed_matches.len(), 1);
    assert_eq!(
        observed_matches[0]
            .observation
            .as_ref()
            .map(|observation| observation.title.as_str()),
        Some("Launch Plan")
    );

    store
        .delete_remote_observation(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("delete observation");
    assert!(
        store
            .list_entity_search_candidates(&fixture.mount_id, "launch", None)
            .expect("search stale observed title")
            .expect("sqlite search")
            .is_empty()
    );

    let id_matches = store
        .list_entity_search_candidates(&fixture.mount_id, "ignored", Some("page1"))
        .expect("search compact id")
        .expect("sqlite search");
    assert_eq!(id_matches.len(), 1);

    store
        .delete_entity(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("delete entity");
    assert!(
        store
            .list_entity_search_candidates(&fixture.mount_id, "road", None)
            .expect("search deleted entity")
            .expect("sqlite search")
            .is_empty()
    );
}

#[test]
fn remounting_same_mount_id_to_different_connection_clears_source_scoped_state() {
    let fixture = SqliteFixture::new();
    let mut store = fixture.open();
    store
        .save_mount(
            fixture
                .mount_config()
                .with_connection_id(ConnectionId::new("old-workspace")),
        )
        .expect("save original mount");
    seed_source_scoped_state(&mut store, &fixture.mount_id);

    store
        .save_mount(
            fixture
                .mount_config()
                .with_connection_id(ConnectionId::new("new-workspace")),
        )
        .expect("remount with new connection");
    drop(store);

    let reopened = fixture.open();
    assert_eq!(
        reopened
            .get_mount(&fixture.mount_id)
            .expect("get mount")
            .expect("mount")
            .connection_id,
        Some(ConnectionId::new("new-workspace"))
    );
    assert!(
        reopened
            .list_entities(&fixture.mount_id)
            .expect("list entities")
            .is_empty()
    );
    assert!(
        reopened
            .list_remote_observations(&fixture.mount_id)
            .expect("list observations")
            .is_empty()
    );
    assert!(
        reopened
            .list_virtual_mutations(&fixture.mount_id)
            .expect("list mutations")
            .is_empty()
    );
    assert!(
        reopened
            .list_auto_save_enrollments(&fixture.mount_id)
            .expect("list auto-save")
            .is_empty()
    );
    assert!(
        reopened
            .list_freshness_states(&fixture.mount_id)
            .expect("list freshness")
            .is_empty()
    );
    assert!(
        reopened
            .list_hydration_jobs()
            .expect("list hydration jobs")
            .is_empty()
    );
    assert!(reopened.list_journal().expect("list journal").is_empty());
    assert!(matches!(
        reopened.load_shadow(&fixture.mount_id, &RemoteId::new("page-1")),
        Err(StoreError::ShadowMissing { .. })
    ));
    assert_eq!(
        reopened
            .list_entity_search_candidates(&fixture.mount_id, "Roadmap", None)
            .expect("search candidates"),
        Some(Vec::new())
    );
}

#[test]
fn remounting_same_mount_id_to_different_remote_root_clears_source_scoped_state() {
    let fixture = SqliteFixture::new();
    let mut store = fixture.open();
    store
        .save_mount(
            fixture
                .mount_config()
                .with_connection_id(ConnectionId::new("workspace"))
                .with_remote_root_id(RemoteId::new("old-root")),
        )
        .expect("save original mount");
    seed_source_scoped_state(&mut store, &fixture.mount_id);

    store
        .save_mount(
            fixture
                .mount_config()
                .with_connection_id(ConnectionId::new("workspace"))
                .with_remote_root_id(RemoteId::new("new-root")),
        )
        .expect("remount with new root");
    drop(store);

    let reopened = fixture.open();
    assert_eq!(
        reopened
            .get_mount(&fixture.mount_id)
            .expect("get mount")
            .expect("mount")
            .remote_root_id,
        Some(RemoteId::new("new-root"))
    );
    assert!(
        reopened
            .list_entities(&fixture.mount_id)
            .expect("list entities")
            .is_empty()
    );
}

#[test]
fn virtual_mutations_round_trip_and_delete_after_reopen() {
    let fixture = SqliteFixture::new();
    let mut store = fixture.open();
    let mutation = virtual_mutation_record();

    store
        .save_mount(fixture.mount_config())
        .expect("save mount");
    store
        .save_virtual_mutation(mutation.clone())
        .expect("save mutation");
    drop(store);

    let mut reopened = fixture.open();
    assert_eq!(
        reopened
            .get_virtual_mutation(&fixture.mount_id, "local:draft")
            .expect("get mutation"),
        Some(mutation.clone())
    );
    assert_eq!(
        reopened
            .find_virtual_mutation_by_path(&fixture.mount_id, "Roadmap/Draft.md".as_ref())
            .expect("find mutation by path"),
        Some(mutation.clone())
    );
    assert_eq!(
        reopened
            .list_virtual_mutations(&fixture.mount_id)
            .expect("list mutations"),
        vec![mutation]
    );

    reopened
        .delete_virtual_mutation(&fixture.mount_id, "local:draft")
        .expect("delete mutation");
    assert!(
        reopened
            .list_virtual_mutations(&fixture.mount_id)
            .expect("list after delete")
            .is_empty()
    );
}

#[test]
fn auto_save_enrollments_round_trip_and_delete_after_reopen() {
    let fixture = SqliteFixture::new();
    let mut store = fixture.open();
    let mut enrollment = AutoSaveEnrollmentRecord::new(
        fixture.mount_id.clone(),
        "Roadmap/Draft.md",
        AutoSaveOrigin::LocalityCreated,
        "1",
    );
    enrollment.remote_id = Some(RemoteId::new("page-2"));
    enrollment.state = AutoSaveState::PausedRemoteChanged;
    enrollment.last_reason = Some("Notion changed externally".to_string());

    store
        .save_mount(fixture.mount_config())
        .expect("save mount");
    store
        .save_auto_save_enrollment(enrollment.clone())
        .expect("save enrollment");
    drop(store);

    let mut reopened = fixture.open();
    assert_eq!(
        reopened
            .get_auto_save_enrollment(&fixture.mount_id, "Roadmap/Draft.md".as_ref())
            .expect("get enrollment"),
        Some(enrollment.clone())
    );
    assert_eq!(
        reopened
            .find_auto_save_enrollment_by_remote_id(&fixture.mount_id, &RemoteId::new("page-2"))
            .expect("find enrollment"),
        Some(enrollment.clone())
    );
    assert_eq!(
        reopened
            .list_auto_save_enrollments(&fixture.mount_id)
            .expect("list enrollments"),
        vec![enrollment]
    );

    reopened
        .delete_auto_save_enrollment(&fixture.mount_id, "Roadmap/Draft.md".as_ref())
        .expect("delete enrollment");
    assert!(
        reopened
            .list_auto_save_enrollments(&fixture.mount_id)
            .expect("list after delete")
            .is_empty()
    );
}

#[test]
fn mount_live_mode_round_trips_and_delete_after_reopen() {
    let fixture = SqliteFixture::new();
    let mut store = fixture.open();
    let mut live_mode = MountLiveModeRecord::new(fixture.mount_id.clone(), true, "1");
    live_mode.state = MountLiveModeState::Syncing;
    live_mode.last_reason = Some("checking for changes".to_string());
    live_mode.last_run_at = Some("2".to_string());
    live_mode.updated_at = "2".to_string();

    store
        .save_mount(fixture.mount_config())
        .expect("save mount");
    store
        .save_mount_live_mode(live_mode.clone())
        .expect("save live mode");
    drop(store);

    let mut reopened = fixture.open();
    assert_eq!(
        reopened
            .get_mount_live_mode(&fixture.mount_id)
            .expect("get live mode"),
        Some(live_mode.clone())
    );
    assert_eq!(
        reopened.list_mount_live_modes().expect("list live modes"),
        vec![live_mode]
    );

    reopened
        .delete_mount_live_mode(&fixture.mount_id)
        .expect("delete live mode");
    assert!(
        reopened
            .list_mount_live_modes()
            .expect("list after delete")
            .is_empty()
    );
}

#[test]
fn remote_observations_round_trip_and_delete_after_reopen() {
    let fixture = SqliteFixture::new();
    let mut store = fixture.open();
    let observation = remote_observation_record();

    store
        .save_mount(fixture.mount_config())
        .expect("save mount");
    store
        .save_remote_observation(observation.clone())
        .expect("save observation");
    drop(store);

    let mut reopened = fixture.open();
    assert_eq!(
        reopened
            .get_remote_observation(&fixture.mount_id, &RemoteId::new("page-1"))
            .expect("get observation"),
        Some(observation.clone())
    );
    assert_eq!(
        reopened
            .list_remote_observations(&fixture.mount_id)
            .expect("list observations"),
        vec![observation]
    );

    reopened
        .delete_remote_observation(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("delete observation");
    assert!(
        reopened
            .list_remote_observations(&fixture.mount_id)
            .expect("list after delete")
            .is_empty()
    );
}

#[test]
fn freshness_state_round_trips_and_delete_after_reopen() {
    let fixture = SqliteFixture::new();
    let mut store = fixture.open();
    let state = freshness_state_record();

    store
        .save_mount(fixture.mount_config())
        .expect("save mount");
    store
        .save_freshness_state(state.clone())
        .expect("save freshness");
    drop(store);

    let mut reopened = fixture.open();
    assert_eq!(
        reopened
            .get_freshness_state(&fixture.mount_id, &RemoteId::new("page-1"))
            .expect("get freshness"),
        Some(state.clone())
    );
    assert_eq!(
        reopened
            .list_freshness_states(&fixture.mount_id)
            .expect("list freshness"),
        vec![state]
    );

    reopened
        .delete_freshness_state(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("delete freshness");
    assert!(
        reopened
            .list_freshness_states(&fixture.mount_id)
            .expect("list after delete")
            .is_empty()
    );
}

#[test]
fn connections_round_trip_without_storing_secret_value() {
    let fixture = SqliteFixture::new();
    let mut store = fixture.open();
    let connection = connection_record("notion-work");

    store
        .save_connection(connection.clone())
        .expect("save connection");
    drop(store);

    let reopened = fixture.open();
    assert_eq!(
        reopened
            .get_connection(&ConnectionId::new("notion-work"))
            .expect("get connection"),
        Some(connection.clone())
    );
    assert_eq!(
        reopened.list_connections().expect("list connections"),
        vec![connection]
    );
    assert_eq!(
        reopened
            .get_connector_profile(&ConnectorProfileId::new("notion-token-default"))
            .expect("get profile"),
        Some(default_profile())
    );

    let sqlite_bytes = fs::read(fixture.state_root.join("state.sqlite3")).expect("read db");
    let sqlite_text = String::from_utf8_lossy(&sqlite_bytes);
    assert!(!sqlite_text.contains("ntn_secret_test_token"));
}

#[test]
fn hydration_jobs_round_trip_and_preserve_failure_metadata() {
    let fixture = SqliteFixture::new();
    let mut store = fixture.open();
    store
        .save_mount(fixture.mount_config())
        .expect("save mount");
    store
        .upsert_hydration_job(hydration_job_record())
        .expect("save hydration job");
    store
        .record_hydration_job_failure(
            &fixture.mount_id,
            &RemoteId::new("page-1"),
            "network timeout".to_string(),
        )
        .expect("record failure");
    store
        .upsert_hydration_job(HydrationJobRecord {
            path: fixture.mount_root.join("Roadmap renamed.md"),
            reason: HydrationReason::FileOpen,
            ..hydration_job_record()
        })
        .expect("update hydration job");
    drop(store);

    let mut reopened = fixture.open();
    let jobs = reopened.list_hydration_jobs().expect("list hydration jobs");

    assert_eq!(
        jobs,
        vec![HydrationJobRecord {
            path: fixture.mount_root.join("Roadmap renamed.md"),
            reason: HydrationReason::FileOpen,
            attempts: 1,
            last_error: Some("network timeout".to_string()),
            ..hydration_job_record()
        }]
    );

    reopened
        .delete_hydration_job(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("delete hydration job");
    assert!(
        reopened
            .list_hydration_jobs()
            .expect("list hydration jobs after delete")
            .is_empty()
    );
}

#[test]
fn sqlite_store_migrates_v5_projection_and_connections_schema() {
    let fixture = SqliteFixture::new();
    fs::create_dir_all(&fixture.state_root).expect("state root");
    let db_path = fixture.state_root.join("state.sqlite3");
    let connection = Connection::open(&db_path).expect("raw connection");
    connection
        .execute_batch(
            "
            PRAGMA user_version = 5;
            CREATE TABLE mounts (
                mount_id TEXT PRIMARY KEY,
                connector TEXT NOT NULL,
                root TEXT NOT NULL,
                remote_root_id TEXT,
                read_only INTEGER NOT NULL CHECK (read_only IN (0, 1))
            );
            ",
        )
        .expect("create v5 schema");
    connection
        .execute(
            "INSERT INTO mounts (mount_id, connector, root, remote_root_id, read_only)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                fixture.mount_id.0.as_str(),
                "notion",
                fixture.mount_root.to_string_lossy(),
                "root-page",
                0
            ],
        )
        .expect("insert mount");
    drop(connection);

    let store = fixture.open();
    let connection = Connection::open(&store.db_path).expect("raw reopened connection");
    let user_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user version");
    let connection_column_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('mounts') WHERE name = 'connection_id'",
            [],
            |row| row.get(0),
        )
        .expect("connection_id column");
    let projection_column_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('mounts') WHERE name = 'projection_json'",
            [],
            |row| row.get(0),
        )
        .expect("projection_json column");
    let connection_table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'connections'",
            [],
            |row| row.get(0),
        )
        .expect("connections table");

    assert_eq!(user_version, 15);
    assert_eq!(connection_column_count, 1);
    assert_eq!(projection_column_count, 1);
    assert_eq!(connection_table_count, 1);
    assert_eq!(
        store
            .list_connector_profiles()
            .expect("list connector profiles"),
        vec![default_profile()]
    );
    let mount = store
        .get_mount(&fixture.mount_id)
        .expect("get migrated mount")
        .expect("mount");
    assert_eq!(mount.connection_id, None);
    assert_eq!(mount.projection, ProjectionMode::PlainFiles);
}

#[test]
fn sqlite_store_migrates_v6_projection_schema_to_connections() {
    let fixture = SqliteFixture::new();
    fs::create_dir_all(&fixture.state_root).expect("state root");
    let db_path = fixture.state_root.join("state.sqlite3");
    let connection = Connection::open(&db_path).expect("raw connection");
    connection
        .execute_batch(
            "
            PRAGMA user_version = 6;
            CREATE TABLE mounts (
                mount_id TEXT PRIMARY KEY,
                connector TEXT NOT NULL,
                root TEXT NOT NULL,
                remote_root_id TEXT,
                read_only INTEGER NOT NULL CHECK (read_only IN (0, 1)),
                projection_json TEXT NOT NULL DEFAULT '\"plain_files\"'
            );
            ",
        )
        .expect("create v6 schema");
    connection
        .execute(
            "INSERT INTO mounts (mount_id, connector, root, remote_root_id, read_only, projection_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                fixture.mount_id.0.as_str(),
                "notion",
                fixture.mount_root.to_string_lossy(),
                "root-page",
                0,
                serde_json::to_string(&ProjectionMode::MacosFileProvider).expect("projection json"),
            ],
        )
        .expect("insert mount");
    drop(connection);

    let store = fixture.open();
    let connection = Connection::open(&store.db_path).expect("raw reopened connection");
    let user_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user version");
    let connection_column_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('mounts') WHERE name = 'connection_id'",
            [],
            |row| row.get(0),
        )
        .expect("connection_id column");
    let connection_table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'connections'",
            [],
            |row| row.get(0),
        )
        .expect("connections table");

    assert_eq!(user_version, 15);
    assert_eq!(connection_column_count, 1);
    assert_eq!(connection_table_count, 1);
    assert_eq!(
        store
            .list_connector_profiles()
            .expect("list connector profiles"),
        vec![default_profile()]
    );
    let mount = store
        .get_mount(&fixture.mount_id)
        .expect("get migrated mount")
        .expect("mount");
    assert_eq!(mount.connection_id, None);
    assert_eq!(mount.projection, ProjectionMode::MacosFileProvider);
}

#[test]
fn sqlite_store_migrates_v7_hydration_jobs_schema() {
    let fixture = SqliteFixture::new();
    fs::create_dir_all(&fixture.state_root).expect("state root");
    let db_path = fixture.state_root.join("state.sqlite3");
    let connection = Connection::open(&db_path).expect("raw connection");
    connection
        .execute_batch(
            "
            PRAGMA user_version = 7;
            CREATE TABLE mounts (
                mount_id TEXT PRIMARY KEY,
                connector TEXT NOT NULL,
                root TEXT NOT NULL,
                remote_root_id TEXT,
                read_only INTEGER NOT NULL CHECK (read_only IN (0, 1)),
                projection_json TEXT NOT NULL DEFAULT '\"plain_files\"',
                connection_id TEXT
            );
            CREATE TABLE connections (
                connection_id TEXT PRIMARY KEY,
                connector TEXT NOT NULL,
                display_name TEXT NOT NULL,
                account_label TEXT,
                workspace_id TEXT,
                workspace_name TEXT,
                auth_kind TEXT NOT NULL,
                secret_ref TEXT NOT NULL,
                scopes_json TEXT NOT NULL DEFAULT '[]',
                capabilities_json TEXT NOT NULL DEFAULT '{}',
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                expires_at TEXT
            );
            ",
        )
        .expect("create v7 schema");
    connection
        .execute(
            "INSERT INTO mounts (mount_id, connector, root, remote_root_id, read_only, projection_json, connection_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                fixture.mount_id.0.as_str(),
                "notion",
                fixture.mount_root.to_string_lossy(),
                "root-page",
                0,
                serde_json::to_string(&ProjectionMode::MacosFileProvider).expect("projection json"),
                Option::<String>::None,
            ],
        )
        .expect("insert mount");
    drop(connection);

    let store = fixture.open();
    let connection = Connection::open(&store.db_path).expect("raw reopened connection");
    let user_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user version");
    let hydration_jobs_table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'hydration_jobs'",
            [],
            |row| row.get(0),
        )
        .expect("hydration_jobs table");

    assert_eq!(user_version, 15);
    assert_eq!(hydration_jobs_table_count, 1);
    assert!(
        store
            .list_hydration_jobs()
            .expect("list hydration jobs")
            .is_empty()
    );
}

#[test]
fn sqlite_store_migrates_v8_connections_to_default_connector_profile() {
    let fixture = SqliteFixture::new();
    fs::create_dir_all(&fixture.state_root).expect("state root");
    let db_path = fixture.state_root.join("state.sqlite3");
    let connection = Connection::open(&db_path).expect("raw connection");
    connection
        .execute_batch(
            "
            PRAGMA user_version = 8;
            CREATE TABLE mounts (
                mount_id TEXT PRIMARY KEY,
                connector TEXT NOT NULL,
                root TEXT NOT NULL,
                remote_root_id TEXT,
                read_only INTEGER NOT NULL CHECK (read_only IN (0, 1)),
                projection_json TEXT NOT NULL DEFAULT '\"plain_files\"',
                connection_id TEXT
            );
            CREATE TABLE connections (
                connection_id TEXT PRIMARY KEY,
                connector TEXT NOT NULL,
                display_name TEXT NOT NULL,
                account_label TEXT,
                workspace_id TEXT,
                workspace_name TEXT,
                auth_kind TEXT NOT NULL,
                secret_ref TEXT NOT NULL,
                scopes_json TEXT NOT NULL DEFAULT '[]',
                capabilities_json TEXT NOT NULL DEFAULT '{}',
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                expires_at TEXT
            );
            CREATE TABLE hydration_jobs (
                mount_id TEXT NOT NULL,
                remote_id TEXT NOT NULL,
                path TEXT NOT NULL,
                target_state_json TEXT NOT NULL,
                reason_json TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                last_error TEXT,
                PRIMARY KEY (mount_id, remote_id),
                FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
            );
            ",
        )
        .expect("create v8 schema");
    connection
        .execute(
            "INSERT INTO connections (
                connection_id, connector, display_name, account_label, workspace_id,
                workspace_name, auth_kind, secret_ref, scopes_json, capabilities_json,
                status, created_at, updated_at, expires_at
             )
             VALUES (?1, 'notion', 'work', 'Locality Workspace', 'workspace-1', 'Locality',
                     'token', 'connection:notion-work', '[]', '{}', 'active',
                     '2026-06-11T00:00:00Z', '2026-06-11T00:00:00Z', NULL)",
            params!["notion-work"],
        )
        .expect("insert connection");
    drop(connection);

    let store = fixture.open();
    let connection = Connection::open(&store.db_path).expect("raw reopened connection");
    let user_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user version");
    let profile_column_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('connections') WHERE name = 'profile_id'",
            [],
            |row| row.get(0),
        )
        .expect("profile_id column");

    assert_eq!(user_version, 15);
    assert_eq!(profile_column_count, 1);
    let migrated_connection = store
        .get_connection(&ConnectionId::new("notion-work"))
        .expect("get connection")
        .expect("connection");
    assert_eq!(
        migrated_connection.profile_id,
        Some(ConnectorProfileId::new("notion-token-default"))
    );
    assert_eq!(
        store
            .get_connector_profile(&ConnectorProfileId::new("notion-token-default"))
            .expect("get profile"),
        Some(default_profile())
    );
}

#[test]
fn sqlite_store_migrates_v11_entity_search_index() {
    let fixture = SqliteFixture::new();
    fs::create_dir_all(&fixture.state_root).expect("state root");
    let db_path = fixture.state_root.join("state.sqlite3");
    let connection = Connection::open(&db_path).expect("raw connection");
    connection
        .execute_batch(
            "
            PRAGMA user_version = 11;
            CREATE TABLE mounts (
                mount_id TEXT PRIMARY KEY,
                connector TEXT NOT NULL,
                root TEXT NOT NULL,
                remote_root_id TEXT,
                read_only INTEGER NOT NULL CHECK (read_only IN (0, 1)),
                projection_json TEXT NOT NULL DEFAULT '\"plain_files\"',
                connection_id TEXT
            );
            CREATE TABLE entities (
                mount_id TEXT NOT NULL,
                remote_id TEXT NOT NULL,
                kind_json TEXT NOT NULL,
                title TEXT NOT NULL,
                path TEXT NOT NULL,
                hydration_json TEXT NOT NULL,
                content_hash TEXT,
                remote_edited_at TEXT,
                PRIMARY KEY (mount_id, remote_id),
                UNIQUE (mount_id, path)
            );
            CREATE TABLE remote_observations (
                mount_id TEXT NOT NULL,
                remote_id TEXT NOT NULL,
                kind_json TEXT NOT NULL,
                title TEXT NOT NULL,
                parent_remote_id TEXT,
                projected_path TEXT NOT NULL,
                remote_version_json TEXT NOT NULL DEFAULT 'null',
                observed_at TEXT NOT NULL,
                deleted INTEGER NOT NULL CHECK (deleted IN (0, 1)),
                raw_metadata_json TEXT NOT NULL DEFAULT '{}',
                PRIMARY KEY (mount_id, remote_id)
            );
            ",
        )
        .expect("create v11 schema");
    connection
        .execute(
            "INSERT INTO mounts (mount_id, connector, root, remote_root_id, read_only, projection_json, connection_id)
             VALUES (?1, 'notion', ?2, 'root-page', 0, ?3, NULL)",
            params![
                fixture.mount_id.0.as_str(),
                fixture.mount_root.to_string_lossy(),
                serde_json::to_string(&ProjectionMode::MacosFileProvider).expect("projection json"),
            ],
        )
        .expect("insert mount");
    connection
        .execute(
            "INSERT INTO entities (
                mount_id, remote_id, kind_json, title, path, hydration_json,
                content_hash, remote_edited_at
             )
             VALUES (?1, 'page-1', ?2, 'Roadmap', 'Roadmap.md', ?3, NULL, NULL)",
            params![
                fixture.mount_id.0.as_str(),
                serde_json::to_string(&EntityKind::Page).expect("kind json"),
                serde_json::to_string(&HydrationState::Stub).expect("hydration json"),
            ],
        )
        .expect("insert entity");
    connection
        .execute(
            "INSERT INTO remote_observations (
                mount_id, remote_id, kind_json, title, parent_remote_id,
                projected_path, remote_version_json, observed_at, deleted, raw_metadata_json
             )
             VALUES (?1, 'page-1', ?2, 'Launch Plan', NULL, 'Planning/Launch Plan.md',
                     'null', '2026-06-16T00:00:00Z', 0, '{}')",
            params![
                fixture.mount_id.0.as_str(),
                serde_json::to_string(&EntityKind::Page).expect("kind json"),
            ],
        )
        .expect("insert observation");
    drop(connection);

    let store = fixture.open();
    let connection = Connection::open(&store.db_path).expect("raw reopened connection");
    let user_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user version");
    let search_table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'entity_search_fts'",
            [],
            |row| row.get(0),
        )
        .expect("entity search table");

    assert_eq!(user_version, 15);
    assert_eq!(search_table_count, 1);
    let matches = store
        .list_entity_search_candidates(&fixture.mount_id, "launch", None)
        .expect("search migrated index")
        .expect("sqlite search");
    assert_eq!(matches.len(), 1);
    assert_eq!(
        matches[0]
            .observation
            .as_ref()
            .map(|observation| observation.title.as_str()),
        Some("Launch Plan")
    );
}

#[test]
fn existing_schema_reads_do_not_need_write_lock() {
    let fixture = SqliteFixture::new();
    let mut store = fixture.open();
    let mount = fixture.mount_config();
    store.save_mount(mount.clone()).expect("save mount");
    drop(store);

    let writer = Connection::open(fixture.state_root.join("state.sqlite3")).expect("raw writer");
    writer
        .execute_batch(
            "
            PRAGMA busy_timeout = 50;
            PRAGMA journal_mode = WAL;
            BEGIN IMMEDIATE;
            ",
        )
        .expect("hold writer transaction");

    let reader = SqliteStateStore::open(fixture.state_root.clone()).expect("open reader");
    let mounts = reader
        .load_mounts()
        .expect("load mounts while writer active");

    assert_eq!(mounts, vec![mount]);

    writer.execute_batch("ROLLBACK").expect("rollback writer");
}

#[test]
fn duplicate_entity_path_is_rejected() {
    let fixture = SqliteFixture::new();
    let mut store = fixture.open();
    store
        .save_mount(fixture.mount_config())
        .expect("save mount");
    store
        .save_entity(entity_record())
        .expect("save first entity");

    let error = store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("page-2"),
            EntityKind::Page,
            "Duplicate",
            "Roadmap.md",
        ))
        .expect_err("duplicate path");

    assert_eq!(
        error,
        StoreError::DuplicateEntityPath {
            mount_id: fixture.mount_id.clone(),
            path: "Roadmap.md".into(),
        }
    );
}

#[test]
fn missing_shadow_returns_structured_error() {
    let fixture = SqliteFixture::new();
    let mut store = fixture.open();
    store
        .save_mount(fixture.mount_config())
        .expect("save mount");

    let error = store
        .load_shadow(&fixture.mount_id, &RemoteId::new("missing-page"))
        .expect_err("missing shadow");

    assert_eq!(
        error,
        StoreError::ShadowMissing {
            mount_id: fixture.mount_id.clone(),
            entity_id: RemoteId::new("missing-page"),
        }
    );
}

#[test]
fn journal_status_survives_reopen() {
    let fixture = SqliteFixture::new();
    let mut store = fixture.open();
    store
        .save_mount(fixture.mount_config())
        .expect("save mount");
    store
        .append_journal(journal_entry("push-1", JournalStatus::Prepared))
        .expect("append journal");
    store
        .record_journal_apply_effects(&PushId("push-1".to_string()), apply_effects("push-1"))
        .expect("record effects");
    store
        .update_journal_status(&PushId("push-1".to_string()), JournalStatus::Reconciled)
        .expect("update journal");
    drop(store);

    let reopened = fixture.open();
    let entry = reopened
        .get_journal(&PushId("push-1".to_string()))
        .expect("get journal")
        .expect("journal entry");

    assert_eq!(entry.status, JournalStatus::Reconciled);
    assert_eq!(entry.plan.summary.blocks_updated, 1);
    assert_eq!(entry.preimages.len(), 1);
    assert_eq!(entry.apply_effects, apply_effects("push-1"));
    assert_eq!(reopened.list_journal().expect("list journal").len(), 1);
}

#[test]
fn journal_reader_tolerates_legacy_entity_content_operations() {
    let fixture = SqliteFixture::new();
    let mut store = fixture.open();
    store
        .save_mount(fixture.mount_config())
        .expect("save mount");
    let connection = Connection::open(&store.db_path).expect("raw connection");
    connection
        .execute(
            "INSERT INTO journals (
                push_id,
                mount_id,
                remote_ids_json,
                plan_json,
                preimages_json,
                apply_effects_json,
                status_json
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                "push-legacy",
                "notion-main",
                serde_json::to_string(&vec![RemoteId::new("page-1")]).expect("remote ids json"),
                json!({
                    "affected_entities": ["page-1"],
                    "operations": [{
                        "type": "update_entity_content",
                        "entity_id": "page-1",
                        "content": "legacy replacement body"
                    }],
                    "summary": {
                        "blocks_created": 0,
                        "blocks_updated": 1,
                        "blocks_moved": 0,
                        "blocks_archived": 0,
                        "entities_created": 0,
                        "entities_archived": 0,
                        "properties_updated": 0
                    },
                    "degradations": []
                })
                .to_string(),
                "[]",
                json!([{
                    "type": "updated_entity_content",
                    "operation_id": "push-legacy:0:update_entity_content:page-1",
                    "operation_index": 0,
                    "entity_id": "page-1"
                }])
                .to_string(),
                serde_json::to_string(&JournalStatus::Reconciled).expect("status json"),
            ],
        )
        .expect("insert legacy journal");

    let journal = store
        .get_journal(&PushId("push-legacy".to_string()))
        .expect("get legacy journal")
        .expect("legacy journal");

    assert_eq!(journal.remote_ids, vec![RemoteId::new("page-1")]);
    assert_eq!(
        journal.plan.affected_entities,
        vec![RemoteId::new("page-1")]
    );
    assert!(journal.plan.operations.is_empty());
    assert!(journal.apply_effects.is_empty());
    assert_eq!(store.list_journal().expect("list journals").len(), 1);
}

#[test]
fn journal_reader_remaps_mixed_legacy_operation_indexes_for_undo() {
    let fixture = SqliteFixture::new();
    let mut store = fixture.open();
    store
        .save_mount(fixture.mount_config())
        .expect("save mount");
    let legacy_plan = json!({
        "affected_entities": ["page-1"],
        "operations": [
            {
                "type": "update_entity_content",
                "entity_id": "page-1",
                "content": "legacy replacement body"
            },
            {
                "type": "append_block",
                "parent_id": "page-1",
                "after": null,
                "content": "New paragraph."
            }
        ],
        "summary": {
            "blocks_created": 1,
            "blocks_updated": 1,
            "blocks_moved": 0,
            "blocks_archived": 0,
            "entities_created": 0,
            "entities_archived": 0,
            "properties_updated": 0
        },
        "degradations": []
    });
    let effects = vec![JournalApplyEffect::CreatedBlock {
        operation_id: PushOperationId("push-mixed:1:append_block:page-1".to_string()),
        operation_index: 1,
        parent_id: RemoteId::new("page-1"),
        block_id: RemoteId::new("created-block-1"),
    }];
    let connection = Connection::open(&store.db_path).expect("raw connection");
    connection
        .execute(
            "INSERT INTO journals (
                push_id,
                mount_id,
                remote_ids_json,
                plan_json,
                preimages_json,
                apply_effects_json,
                status_json
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                "push-mixed",
                "notion-main",
                serde_json::to_string(&vec![RemoteId::new("page-1")]).expect("remote ids json"),
                legacy_plan.to_string(),
                "[]",
                serde_json::to_string(&effects).expect("effects json"),
                serde_json::to_string(&JournalStatus::Reconciled).expect("status json"),
            ],
        )
        .expect("insert mixed legacy journal");

    let journal = store
        .get_journal(&PushId("push-mixed".to_string()))
        .expect("get mixed legacy journal")
        .expect("mixed legacy journal");

    assert_eq!(journal.plan.operations.len(), 1);
    assert_eq!(journal.plan.summary.blocks_created, 1);
    assert_eq!(journal.plan.summary.blocks_updated, 0);
    assert_eq!(journal.apply_effects.len(), 1);
    assert!(matches!(
        journal.apply_effects[0],
        JournalApplyEffect::CreatedBlock {
            operation_index: 0,
            ..
        }
    ));

    let undo = plan_journal_undo(&journal);

    assert_eq!(undo.status, UndoPlanStatus::Complete);
    assert_eq!(
        undo.operations,
        vec![UndoOperation::ArchiveCreatedBlock {
            block_id: RemoteId::new("created-block-1")
        }]
    );
}

#[test]
fn sqlite_store_migrates_v1_journals_with_empty_preimages() {
    let fixture = SqliteFixture::new();
    fs::create_dir_all(&fixture.state_root).expect("state root");
    let db_path = fixture.state_root.join("state.sqlite3");
    let connection = Connection::open(&db_path).expect("raw connection");
    connection
        .execute_batch(
            "
            PRAGMA user_version = 1;
            CREATE TABLE mounts (
                mount_id TEXT PRIMARY KEY,
                connector TEXT NOT NULL,
                root TEXT NOT NULL,
                read_only INTEGER NOT NULL CHECK (read_only IN (0, 1))
            );
            CREATE TABLE journals (
                push_id TEXT PRIMARY KEY,
                mount_id TEXT NOT NULL,
                remote_ids_json TEXT NOT NULL,
                plan_json TEXT NOT NULL,
                status_json TEXT NOT NULL,
                FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
            );
            ",
        )
        .expect("create v1 schema");
    connection
        .execute(
            "INSERT INTO mounts (mount_id, connector, root, read_only)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                fixture.mount_id.0.as_str(),
                "notion",
                fixture.mount_root.to_string_lossy(),
                0
            ],
        )
        .expect("insert mount");
    connection
        .execute(
            "INSERT INTO journals (push_id, mount_id, remote_ids_json, plan_json, status_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                "push-1",
                "notion-main",
                serde_json::to_string(&vec![RemoteId::new("page-1")]).expect("remote ids json"),
                serde_json::to_string(&push_plan()).expect("plan json"),
                serde_json::to_string(&JournalStatus::Reconciled).expect("status json"),
            ],
        )
        .expect("insert v1 journal");
    drop(connection);

    let store = fixture.open();
    let connection = Connection::open(&store.db_path).expect("raw reopened connection");
    let user_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user version");
    let entry = store
        .get_journal(&PushId("push-1".to_string()))
        .expect("get migrated journal")
        .expect("journal");

    assert_eq!(user_version, 15);
    assert!(entry.preimages.is_empty());
    assert!(entry.apply_effects.is_empty());
}

#[test]
fn sqlite_store_migrates_v2_journals_with_empty_apply_effects() {
    let fixture = SqliteFixture::new();
    fs::create_dir_all(&fixture.state_root).expect("state root");
    let db_path = fixture.state_root.join("state.sqlite3");
    let connection = Connection::open(&db_path).expect("raw connection");
    connection
        .execute_batch(
            "
            PRAGMA user_version = 2;
            CREATE TABLE mounts (
                mount_id TEXT PRIMARY KEY,
                connector TEXT NOT NULL,
                root TEXT NOT NULL,
                read_only INTEGER NOT NULL CHECK (read_only IN (0, 1))
            );
            CREATE TABLE journals (
                push_id TEXT PRIMARY KEY,
                mount_id TEXT NOT NULL,
                remote_ids_json TEXT NOT NULL,
                plan_json TEXT NOT NULL,
                preimages_json TEXT NOT NULL DEFAULT '[]',
                status_json TEXT NOT NULL,
                FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
            );
            ",
        )
        .expect("create v2 schema");
    connection
        .execute(
            "INSERT INTO mounts (mount_id, connector, root, read_only)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                fixture.mount_id.0.as_str(),
                "notion",
                fixture.mount_root.to_string_lossy(),
                0
            ],
        )
        .expect("insert mount");
    connection
        .execute(
            "INSERT INTO journals (push_id, mount_id, remote_ids_json, plan_json, preimages_json, status_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                "push-2",
                "notion-main",
                serde_json::to_string(&vec![RemoteId::new("page-1")]).expect("remote ids json"),
                serde_json::to_string(&push_plan()).expect("plan json"),
                "[]",
                serde_json::to_string(&JournalStatus::Reconciled).expect("status json"),
            ],
        )
        .expect("insert v2 journal");
    drop(connection);

    let store = fixture.open();
    let connection = Connection::open(&store.db_path).expect("raw reopened connection");
    let user_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user version");
    let entry = store
        .get_journal(&PushId("push-2".to_string()))
        .expect("get migrated journal")
        .expect("journal");

    assert_eq!(user_version, 15);
    assert!(entry.apply_effects.is_empty());
}

fn query_state_components(connection: &Connection) -> Vec<(String, String, i64, i64, i64, i64)> {
    let mut statement = connection
        .prepare(
            "SELECT component_id, component_kind, version, min_reader_version, required, rebuildable
             FROM state_components
             ORDER BY component_id",
        )
        .expect("prepare state component query");
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .expect("query state components");

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect components")
}

fn schema_column_snapshot(connection: &Connection) -> String {
    let mut table_statement = connection
        .prepare(
            "SELECT name
             FROM sqlite_master
             WHERE type = 'table'
               AND name NOT LIKE 'sqlite_%'
               AND name NOT GLOB 'entity_search_fts_*'
             ORDER BY name",
        )
        .expect("prepare table snapshot query");
    let table_rows = table_statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query schema tables");
    let table_names = table_rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect table names");

    let mut lines = Vec::new();
    for table_name in table_names {
        let mut column_statement = connection
            .prepare(&format!("PRAGMA table_info({table_name})"))
            .expect("prepare column snapshot query");
        let column_rows = column_statement
            .query_map([], |row| row.get::<_, String>(1))
            .expect("query schema columns");
        let columns = column_rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect column names");
        lines.push(format!("{table_name}: {}", columns.join(", ")));
    }

    lines.join("\n")
}

fn create_minimal_v12_state(fixture: &SqliteFixture) {
    fs::create_dir_all(&fixture.state_root).expect("state root");
    let db_path = fixture.state_root.join("state.sqlite3");
    let connection = Connection::open(db_path).expect("raw connection");
    connection
        .execute_batch(
            "
            PRAGMA user_version = 12;
            CREATE TABLE mounts (
                mount_id TEXT PRIMARY KEY,
                connector TEXT NOT NULL,
                root TEXT NOT NULL,
                remote_root_id TEXT,
                read_only INTEGER NOT NULL CHECK (read_only IN (0, 1)),
                projection_json TEXT NOT NULL DEFAULT '\"plain_files\"',
                connection_id TEXT
            );
            CREATE TABLE entities (
                mount_id TEXT NOT NULL,
                remote_id TEXT NOT NULL,
                kind_json TEXT NOT NULL,
                title TEXT NOT NULL,
                path TEXT NOT NULL,
                hydration_json TEXT NOT NULL,
                content_hash TEXT,
                remote_edited_at TEXT,
                PRIMARY KEY (mount_id, remote_id),
                UNIQUE (mount_id, path)
            );
            ",
        )
        .expect("create v12 schema");
    connection
        .execute(
            "INSERT INTO mounts (mount_id, connector, root, remote_root_id, read_only, projection_json, connection_id)
             VALUES (?1, 'notion', ?2, 'root-page', 0, ?3, NULL)",
            params![
                fixture.mount_id.0.as_str(),
                fixture.mount_root.to_string_lossy(),
                serde_json::to_string(&ProjectionMode::PlainFiles).expect("projection json"),
            ],
        )
        .expect("insert mount");
    connection
        .execute(
            "INSERT INTO entities (
                mount_id, remote_id, kind_json, title, path, hydration_json,
                content_hash, remote_edited_at
             )
             VALUES (?1, 'page-1', ?2, 'Roadmap', 'Roadmap.md', ?3, NULL, NULL)",
            params![
                fixture.mount_id.0.as_str(),
                serde_json::to_string(&EntityKind::Page).expect("kind json"),
                serde_json::to_string(&HydrationState::Stub).expect("hydration json"),
            ],
        )
        .expect("insert entity");
}

fn create_minimal_v12_linux_fuse_state(fixture: &SqliteFixture, mount_root: &PathBuf) {
    fs::create_dir_all(&fixture.state_root).expect("state root");
    let db_path = fixture.state_root.join("state.sqlite3");
    let connection = Connection::open(db_path).expect("raw connection");
    connection
        .execute_batch(
            "
            PRAGMA user_version = 12;
            CREATE TABLE mounts (
                mount_id TEXT PRIMARY KEY,
                connector TEXT NOT NULL,
                root TEXT NOT NULL,
                remote_root_id TEXT,
                read_only INTEGER NOT NULL CHECK (read_only IN (0, 1)),
                projection_json TEXT NOT NULL DEFAULT '\"plain_files\"',
                connection_id TEXT
            );
            ",
        )
        .expect("create v12 schema");
    connection
        .execute(
            "INSERT INTO mounts (mount_id, connector, root, remote_root_id, read_only, projection_json, connection_id)
             VALUES (?1, 'notion', ?2, 'root-page', 0, ?3, NULL)",
            params![
                fixture.mount_id.0.as_str(),
                mount_root.to_string_lossy(),
                serde_json::to_string(&ProjectionMode::LinuxFuse).expect("projection json"),
            ],
        )
        .expect("insert mount");
}

fn create_minimal_v13_linux_fuse_state(fixture: &SqliteFixture, mount_root: &PathBuf) {
    fs::create_dir_all(&fixture.state_root).expect("state root");
    let db_path = fixture.state_root.join("state.sqlite3");
    let connection = Connection::open(db_path).expect("raw connection");
    connection
        .execute_batch(
            "
            PRAGMA user_version = 13;
            CREATE TABLE mounts (
                mount_id TEXT PRIMARY KEY,
                connector TEXT NOT NULL,
                root TEXT NOT NULL,
                remote_root_id TEXT,
                read_only INTEGER NOT NULL CHECK (read_only IN (0, 1)),
                projection_json TEXT NOT NULL DEFAULT '\"plain_files\"',
                connection_id TEXT
            );
            CREATE TABLE state_components (
                component_id TEXT PRIMARY KEY,
                component_kind TEXT NOT NULL,
                version INTEGER NOT NULL,
                min_reader_version INTEGER NOT NULL DEFAULT 1,
                required INTEGER NOT NULL CHECK (required IN (0, 1)),
                rebuildable INTEGER NOT NULL CHECK (rebuildable IN (0, 1)),
                data_json TEXT NOT NULL DEFAULT '{}',
                updated_at TEXT NOT NULL
            );
            ",
        )
        .expect("create v13 schema");
    connection
        .execute(
            "INSERT INTO mounts (mount_id, connector, root, remote_root_id, read_only, projection_json, connection_id)
             VALUES (?1, 'notion', ?2, 'root-page', 0, ?3, NULL)",
            params![
                fixture.mount_id.0.as_str(),
                mount_root.to_string_lossy(),
                serde_json::to_string(&ProjectionMode::LinuxFuse).expect("projection json"),
            ],
        )
        .expect("insert mount");
}

fn insert_current_state_components_for_v13(
    connection: &Connection,
    linux_fuse_component: Option<(i64, i64)>,
) {
    for definition in SqliteStateStore::current_component_definitions() {
        if definition.component_id == "projection:linux_fuse" {
            if let Some((version, min_reader_version)) = linux_fuse_component {
                insert_state_component(
                    connection,
                    definition.component_id,
                    definition.component_kind,
                    version,
                    min_reader_version,
                    definition.required,
                    definition.rebuildable,
                    definition.data_json,
                );
            }
        } else {
            insert_state_component(
                connection,
                definition.component_id,
                definition.component_kind,
                definition.current_version,
                definition.min_reader_version,
                definition.required,
                definition.rebuildable,
                definition.data_json,
            );
        }
    }
}

fn insert_state_component(
    connection: &Connection,
    component_id: &str,
    component_kind: &str,
    version: i64,
    min_reader_version: i64,
    required: bool,
    rebuildable: bool,
    data_json: &str,
) {
    connection
        .execute(
            "INSERT INTO state_components (
                component_id, component_kind, version, min_reader_version,
                required, rebuildable, data_json, updated_at
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, '0')",
            params![
                component_id,
                component_kind,
                version,
                min_reader_version,
                if required { 1 } else { 0 },
                if rebuildable { 1 } else { 0 },
                data_json,
            ],
        )
        .expect("insert state component");
}

fn query_state_migration_count(connection: &Connection) -> i64 {
    if !sqlite_table_exists(connection, "state_migrations") {
        return 0;
    }

    connection
        .query_row("SELECT COUNT(*) FROM state_migrations", [], |row| {
            row.get(0)
        })
        .expect("state migration count")
}

fn sqlite_table_exists(connection: &Connection, table_name: &str) -> bool {
    let table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*)
             FROM sqlite_master
             WHERE type = 'table' AND name = ?1",
            params![table_name],
            |row| row.get(0),
        )
        .expect("sqlite table count");
    table_count > 0
}

fn seed_notion_workspace_roots_legacy_state(fixture: &SqliteFixture) -> PathBuf {
    let mut store = fixture.open();
    let root_page_mount = MountId::new("notion-root-page");
    let non_notion_mount = MountId::new("google-docs-main");
    store
        .save_mount(MountConfig::new(
            fixture.mount_id.clone(),
            "notion",
            fixture.mount_root.join("workspace"),
        ))
        .expect("save workspace notion mount");
    store
        .save_mount(
            MountConfig::new(
                root_page_mount.clone(),
                "notion",
                fixture.mount_root.join("root-page"),
            )
            .with_remote_root_id(RemoteId::new("root-page")),
        )
        .expect("save root-page notion mount");
    store
        .save_mount(MountConfig::new(
            non_notion_mount.clone(),
            "google-docs",
            fixture.mount_root.join("google-docs"),
        ))
        .expect("save non-notion mount");

    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("workspace-page"),
            EntityKind::Page,
            "Roadmap",
            "Roadmap/page.md",
        ))
        .expect("save workspace entity");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("already-workspace"),
            EntityKind::Page,
            "Already",
            "Workspace/Already/page.md",
        ))
        .expect("save already migrated workspace entity");
    store
        .save_entity(EntityRecord::new(
            root_page_mount,
            RemoteId::new("root-child"),
            EntityKind::Page,
            "Root Child",
            "Root Child/page.md",
        ))
        .expect("save root-page entity");
    store
        .save_entity(EntityRecord::new(
            non_notion_mount,
            RemoteId::new("doc-1"),
            EntityKind::Page,
            "Doc",
            "Doc/page.md",
        ))
        .expect("save non-notion entity");
    store
        .save_remote_observation(RemoteObservationRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("workspace-page"),
            EntityKind::Page,
            "Roadmap",
            "Roadmap/page.md",
            "2026-07-03T00:00:00Z",
        ))
        .expect("save workspace observation");
    store
        .upsert_hydration_job(HydrationJobRecord {
            mount_id: fixture.mount_id.clone(),
            remote_id: RemoteId::new("workspace-page"),
            path: PathBuf::from("Roadmap/page.md"),
            target_state: HydrationState::Hydrated,
            reason: HydrationReason::Policy,
            attempts: 0,
            last_error: None,
        })
        .expect("save workspace hydration job");
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: fixture.mount_id.clone(),
            local_id: "local:rename".to_string(),
            mutation_kind: VirtualMutationKind::Rename,
            target_remote_id: Some(RemoteId::new("workspace-page")),
            parent_remote_id: None,
            original_path: Some(PathBuf::from("Roadmap/page.md")),
            projected_path: PathBuf::from("Roadmap Renamed/page.md"),
            title: "Roadmap Renamed".to_string(),
            content_path: Some(PathBuf::from("Roadmap Renamed/page.md")),
            created_at: "2026-07-03T00:00:00Z".to_string(),
            updated_at: "2026-07-03T00:00:00Z".to_string(),
        })
        .expect("save workspace virtual mutation");
    let mut enrollment = AutoSaveEnrollmentRecord::new(
        fixture.mount_id.clone(),
        "Roadmap/page.md",
        AutoSaveOrigin::UserEnabled,
        "2026-07-03T00:00:00Z",
    );
    enrollment.remote_id = Some(RemoteId::new("workspace-page"));
    store
        .save_auto_save_enrollment(enrollment)
        .expect("save workspace auto-save");

    store.db_path.clone()
}

fn seed_notion_workspace_root_name_collision_state(fixture: &SqliteFixture) -> PathBuf {
    let mut store = fixture.open();
    store
        .save_mount(MountConfig::new(
            fixture.mount_id.clone(),
            "notion",
            fixture.mount_root.join("workspace"),
        ))
        .expect("save workspace notion mount");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("private-database"),
            EntityKind::Database,
            "Private Database",
            "Private",
        ))
        .expect("save private-named database");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("workspace-database"),
            EntityKind::Database,
            "Workspace Database",
            "Workspace",
        ))
        .expect("save workspace-named database");

    store.db_path.clone()
}

fn seed_notion_workspace_root_page_name_collision_state(fixture: &SqliteFixture) -> PathBuf {
    let mut store = fixture.open();
    store
        .save_mount(MountConfig::new(
            fixture.mount_id.clone(),
            "notion",
            fixture.mount_root.join("workspace"),
        ))
        .expect("save workspace notion mount");

    for (remote_id, title, path) in [
        ("private-page", "Private", "Private/page.md"),
        (
            "private-page-child",
            "Private Child",
            "Private/Child/page.md",
        ),
        ("workspace-page", "Workspace", "Workspace/page.md"),
        (
            "workspace-page-child",
            "Workspace Child",
            "Workspace/Child/page.md",
        ),
        ("already-workspace", "Already", "Workspace/Already/page.md"),
        (
            "already-workspace-child",
            "Already Child",
            "Workspace/Already/Child/page.md",
        ),
    ] {
        let remote_id_value = remote_id;
        let remote_id = RemoteId::new(remote_id_value);
        store
            .save_entity(EntityRecord::new(
                fixture.mount_id.clone(),
                remote_id.clone(),
                EntityKind::Page,
                title,
                path,
            ))
            .expect("save workspace entity");
        let mut observation = RemoteObservationRecord::new(
            fixture.mount_id.clone(),
            remote_id.clone(),
            EntityKind::Page,
            title,
            path,
            "2026-07-03T00:00:00Z",
        );
        observation = match remote_id_value {
            "private-page-child" => observation.with_parent(RemoteId::new("private-page")),
            "workspace-page-child" => observation.with_parent(RemoteId::new("workspace-page")),
            "already-workspace-child" => {
                observation.with_parent(RemoteId::new("already-workspace"))
            }
            _ => observation,
        };
        store
            .save_remote_observation(observation)
            .expect("save workspace observation");
        store
            .upsert_hydration_job(HydrationJobRecord {
                mount_id: fixture.mount_id.clone(),
                remote_id: remote_id.clone(),
                path: PathBuf::from(path),
                target_state: HydrationState::Hydrated,
                reason: HydrationReason::Policy,
                attempts: 0,
                last_error: None,
            })
            .expect("save workspace hydration job");
        store
            .save_virtual_mutation(VirtualMutationRecord {
                mount_id: fixture.mount_id.clone(),
                local_id: format!("local:{remote_id_value}"),
                mutation_kind: VirtualMutationKind::Rename,
                target_remote_id: Some(remote_id.clone()),
                parent_remote_id: None,
                original_path: Some(PathBuf::from(path)),
                projected_path: PathBuf::from(path),
                title: title.to_string(),
                content_path: Some(PathBuf::from(path)),
                created_at: "2026-07-03T00:00:00Z".to_string(),
                updated_at: "2026-07-03T00:00:00Z".to_string(),
            })
            .expect("save workspace virtual mutation");
        let mut enrollment = AutoSaveEnrollmentRecord::new(
            fixture.mount_id.clone(),
            path,
            AutoSaveOrigin::UserEnabled,
            "2026-07-03T00:00:00Z",
        );
        enrollment.remote_id = Some(remote_id);
        store
            .save_auto_save_enrollment(enrollment)
            .expect("save workspace auto-save");
    }

    store.db_path.clone()
}

fn query_entities_for_mount(
    connection: &Connection,
    mount_id: &MountId,
) -> Vec<(String, String, String, String, String)> {
    let mut statement = connection
        .prepare(
            "SELECT remote_id, kind_json, title, path, hydration_json
             FROM entities
             WHERE mount_id = ?1
             ORDER BY path, remote_id",
        )
        .expect("prepare entities query");
    let rows = statement
        .query_map(params![mount_id.0.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .expect("query entities");
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect entities")
        .into_iter()
        .map(|(remote_id, kind_json, title, path, hydration_json)| {
            (
                remote_id,
                serde_json::from_str::<String>(&kind_json).expect("kind json"),
                title,
                path,
                serde_json::from_str::<String>(&hydration_json).expect("hydration json"),
            )
        })
        .collect()
}

fn query_paths_by_remote_id(
    connection: &Connection,
    table: &str,
    column: &str,
    mount_id: &MountId,
) -> Vec<(String, String)> {
    let sql = format!(
        "SELECT remote_id, {column}
         FROM {table}
         WHERE mount_id = ?1
           AND remote_id IS NOT NULL
         ORDER BY remote_id"
    );
    let mut statement = connection.prepare(&sql).expect("prepare paths query");
    let rows = statement
        .query_map(params![mount_id.0.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("query paths");
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect paths")
}

fn query_virtual_mutation_paths(
    connection: &Connection,
    mount_id: &MountId,
) -> Vec<(String, Option<String>, String, Option<String>)> {
    let mut statement = connection
        .prepare(
            "SELECT local_id, original_path, projected_path, content_path
             FROM virtual_mutations
             WHERE mount_id = ?1
             ORDER BY local_id",
        )
        .expect("prepare virtual mutation paths query");
    let rows = statement
        .query_map(params![mount_id.0.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .expect("query virtual mutation paths");
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect virtual mutation paths")
}

fn assert_notion_workspace_roots_state_not_rewritten(db_path: &PathBuf, mount_id: &MountId) {
    let connection = Connection::open(db_path).expect("raw connection");
    let workspace_path: String = connection
        .query_row(
            "SELECT path
             FROM entities
             WHERE mount_id = ?1
               AND remote_id = 'workspace-page'",
            params![mount_id.0.as_str()],
            |row| row.get(0),
        )
        .expect("workspace entity path");
    let synthetic_root_count: i64 = connection
        .query_row(
            "SELECT COUNT(*)
             FROM entities
             WHERE mount_id = ?1
               AND remote_id IN ('notion-root:private', 'notion-root:workspace')",
            params![mount_id.0.as_str()],
            |row| row.get(0),
        )
        .expect("synthetic root count");
    let migration_count: i64 = connection
        .query_row(
            "SELECT COUNT(*)
             FROM state_migrations
             WHERE migration_id = 'component-projection-notion_workspace_roots-0-to-1'",
            [],
            |row| row.get(0),
        )
        .expect("component migration count");

    assert_eq!(workspace_path, "Roadmap/page.md");
    assert_eq!(synthetic_root_count, 0);
    assert_eq!(migration_count, 0);
}

struct SqliteFixture {
    state_root: PathBuf,
    mount_root: PathBuf,
    mount_id: MountId,
}

impl SqliteFixture {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "locality-store-sqlite-{}-{unique}-{suffix}",
            std::process::id()
        ));
        let state_root = root.join("state");
        let mount_root = root.join("mount");
        fs::create_dir_all(&mount_root).expect("mount root");

        Self {
            state_root,
            mount_root,
            mount_id: MountId::new("notion-main"),
        }
    }

    fn open(&self) -> SqliteStateStore {
        SqliteStateStore::open(self.state_root.clone()).expect("open sqlite store")
    }

    fn mount_config(&self) -> MountConfig {
        MountConfig::new(self.mount_id.clone(), "notion", self.mount_root.clone())
            .with_remote_root_id(RemoteId::new("root-page"))
    }
}

impl Drop for SqliteFixture {
    fn drop(&mut self) {
        if let Some(root) = self.state_root.parent() {
            let _ = fs::remove_dir_all(root);
        }
    }
}

fn entity_record() -> EntityRecord {
    EntityRecord::new(
        MountId::new("notion-main"),
        RemoteId::new("page-1"),
        EntityKind::Page,
        "Roadmap",
        "Roadmap.md",
    )
    .with_hydration(HydrationState::Hydrated)
    .with_content_hash("body-hash")
    .with_remote_edited_at("2026-06-10T00:00:00Z")
}

fn hydration_job_record() -> HydrationJobRecord {
    HydrationJobRecord {
        mount_id: MountId::new("notion-main"),
        remote_id: RemoteId::new("page-1"),
        path: PathBuf::from("Roadmap.md"),
        target_state: HydrationState::Hydrated,
        reason: HydrationReason::Policy,
        attempts: 0,
        last_error: None,
    }
}

fn virtual_mutation_record() -> VirtualMutationRecord {
    VirtualMutationRecord {
        mount_id: MountId::new("notion-main"),
        local_id: "local:draft".to_string(),
        mutation_kind: VirtualMutationKind::Create,
        target_remote_id: None,
        parent_remote_id: Some(RemoteId::new("page-1")),
        original_path: None,
        projected_path: PathBuf::from("Roadmap/Draft.md"),
        title: "Draft".to_string(),
        content_path: Some(PathBuf::from(
            "/tmp/loc-state/content/notion-main/files/Roadmap/Draft.md",
        )),
        created_at: "2026-06-12T00:00:00Z".to_string(),
        updated_at: "2026-06-12T00:00:00Z".to_string(),
    }
}

fn remote_observation_record() -> RemoteObservationRecord {
    RemoteObservationRecord::new(
        MountId::new("notion-main"),
        RemoteId::new("page-1"),
        EntityKind::Page,
        "Roadmap",
        "Roadmap.md",
        "2026-06-15T00:00:00Z",
    )
    .with_parent(RemoteId::new("root-page"))
    .with_remote_version(RemoteVersion::new("remote-v1"))
    .with_raw_metadata_json("{\"source\":\"test\"}")
}

fn freshness_state_record() -> FreshnessStateRecord {
    FreshnessStateRecord::new(
        MountId::new("notion-main"),
        RemoteId::new("page-1"),
        FreshnessTier::Hot,
    )
    .checked_at("2026-06-15T00:00:00Z")
    .next_check_at("2026-06-15T00:01:00Z")
    .opened_at("2026-06-15T00:00:05Z")
    .local_change_at("2026-06-15T00:00:10Z")
    .remote_hint_pending(true)
}

fn connection_record(connection_id: &str) -> ConnectionRecord {
    ConnectionRecord {
        connection_id: ConnectionId::new(connection_id),
        profile_id: Some(ConnectorProfileId::new("notion-token-default")),
        connector: "notion".to_string(),
        display_name: "work".to_string(),
        account_label: Some("Locality Workspace".to_string()),
        workspace_id: Some("workspace-1".to_string()),
        workspace_name: Some("Locality".to_string()),
        auth_kind: "token".to_string(),
        secret_ref: format!("connection:{connection_id}"),
        scopes: vec![],
        capabilities_json: "{}".to_string(),
        status: "active".to_string(),
        created_at: "2026-06-11T00:00:00Z".to_string(),
        updated_at: "2026-06-11T00:00:00Z".to_string(),
        expires_at: None,
    }
}

fn default_profile() -> ConnectorProfileRecord {
    ConnectorProfileRecord {
        profile_id: ConnectorProfileId::new("notion-token-default"),
        connector: "notion".to_string(),
        display_name: "Notion token auth".to_string(),
        auth_kind: "token".to_string(),
        scopes: vec![],
        capabilities_json: DEFAULT_NOTION_CAPABILITIES_JSON.to_string(),
        enabled_actions_json: "[\"read\",\"write\"]".to_string(),
        connector_version: "notion.v1".to_string(),
        status: "active".to_string(),
        created_at: "0".to_string(),
        updated_at: "0".to_string(),
    }
}

fn shadow_document(body: &str) -> ShadowDocument {
    ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        body,
        9,
        [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
    )
    .expect("shadow")
}

fn journal_entry(push_id: &str, status: JournalStatus) -> JournalEntry {
    JournalEntry::new(
        PushId(push_id.to_string()),
        MountId::new("notion-main"),
        vec![RemoteId::new("page-1")],
        push_plan(),
        status,
    )
    .with_preimages(vec![JournalPreimage::from_shadow(shadow_document(
        "# Roadmap\n\nOriginal paragraph.",
    ))])
}

fn seed_source_scoped_state(store: &mut SqliteStateStore, mount_id: &MountId) {
    store.save_entity(entity_record()).expect("save entity");
    store
        .save_shadow(mount_id, shadow_document("# Roadmap\n\nSame paragraph."))
        .expect("save shadow");
    store
        .save_remote_observation(remote_observation_record())
        .expect("save observation");
    store
        .save_virtual_mutation(virtual_mutation_record())
        .expect("save virtual mutation");
    store
        .save_auto_save_enrollment(AutoSaveEnrollmentRecord::new(
            mount_id.clone(),
            "Roadmap/Draft.md",
            AutoSaveOrigin::LocalityCreated,
            "1",
        ))
        .expect("save auto-save enrollment");
    store
        .save_freshness_state(freshness_state_record())
        .expect("save freshness");
    store
        .upsert_hydration_job(hydration_job_record())
        .expect("save hydration job");
    store
        .append_journal(journal_entry("push-1", JournalStatus::Prepared))
        .expect("append journal");
}

fn apply_effects(push_id: &str) -> Vec<JournalApplyEffect> {
    vec![JournalApplyEffect::UpdatedBlock {
        operation_id: PushOperationId(format!("{push_id}:0:update_block:paragraph-1")),
        operation_index: 0,
        block_id: RemoteId::new("paragraph-1"),
    }]
}

fn push_plan() -> PushPlan {
    PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![PushOperation::UpdateBlock {
            block_id: RemoteId::new("paragraph-1"),
            content: "Updated paragraph.".to_string(),
        }],
    )
}
