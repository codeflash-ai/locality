use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use afs_core::hydration::HydrationReason;
use afs_core::journal::{
    JournalApplyEffect, JournalEntry, JournalPreimage, JournalStatus, PushId, PushOperationId,
};
use afs_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use afs_core::planner::{PushOperation, PushPlan};
use afs_core::shadow::{MarkdownBlockKind, ShadowDocument};
use afs_store::{
    ConnectionId, ConnectionRecord, ConnectionRepository, ConnectorProfileId,
    ConnectorProfileRecord, ConnectorProfileRepository, EntityRecord, EntityRepository,
    HydrationJobRecord, HydrationJobRepository, JournalRepository, MountConfig, MountRepository,
    ProjectionMode, ShadowRepository, SqliteStateStore, StoreError,
};
use rusqlite::{Connection, params};

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
    assert_eq!(user_version, 9);
    assert_eq!(journal_mode, "wal");
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
            supported: 9,
        }
    );
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
        serde_json::to_string(&ProjectionMode::MacosFileProvider).expect("projection json"),
        "\"macos_file_provider\""
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
    let shadow = shadow_document("# Roadmap\n\nSame paragraph.");

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

    assert_eq!(user_version, 9);
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

    assert_eq!(user_version, 9);
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

    assert_eq!(user_version, 9);
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
             VALUES (?1, 'notion', 'work', 'AgentFS Workspace', 'workspace-1', 'AgentFS',
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

    assert_eq!(user_version, 9);
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

    assert_eq!(user_version, 9);
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

    assert_eq!(user_version, 9);
    assert!(entry.apply_effects.is_empty());
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
            "afs-store-sqlite-{}-{unique}-{suffix}",
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

fn connection_record(connection_id: &str) -> ConnectionRecord {
    ConnectionRecord {
        connection_id: ConnectionId::new(connection_id),
        profile_id: Some(ConnectorProfileId::new("notion-token-default")),
        connector: "notion".to_string(),
        display_name: "work".to_string(),
        account_label: Some("AgentFS Workspace".to_string()),
        workspace_id: Some("workspace-1".to_string()),
        workspace_name: Some("AgentFS".to_string()),
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
        capabilities_json: "{}".to_string(),
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
