use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use afs_core::journal::{
    JournalApplyEffect, JournalEntry, JournalPreimage, JournalStatus, PushId, PushOperationId,
};
use afs_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use afs_core::planner::{PushOperation, PushPlan};
use afs_core::shadow::{MarkdownBlockKind, ShadowDocument};
use afs_store::{
    EntityRecord, EntityRepository, JournalRepository, MountConfig, MountRepository,
    ShadowRepository, SqliteStateStore, StoreError,
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
    assert_eq!(user_version, 3);
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
            supported: 3,
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

    assert_eq!(user_version, 3);
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

    assert_eq!(user_version, 3);
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
