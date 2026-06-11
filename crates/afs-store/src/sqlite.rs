//! SQLite state-store implementation.
//!
//! This is the first durable adapter for the repository traits. It keeps the
//! schema intentionally compact: path-addressable facts live in relational
//! columns, while shadow block arrays and journal plans are stored as JSON blobs
//! until query needs justify normalization.

use std::path::{Path, PathBuf};

use afs_core::AfsResult;
use afs_core::journal::{
    JournalApplyEffect, JournalEntry, JournalPreimage, JournalStatus, JournalStore, PushId,
};
use afs_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use afs_core::shadow::ShadowDocument;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{StoreError, StoreResult};
use crate::records::{
    EntityRecord, MountConfig, ProjectionMode, ShadowBlockRecord, ShadowSnapshotRecord,
};
use crate::repository::{EntityRepository, JournalRepository, MountRepository, ShadowRepository};

const DB_FILE: &str = "state.sqlite3";
const SCHEMA_VERSION: i64 = 6;

#[derive(Clone, Debug)]
pub struct SqliteStateStore {
    pub root: PathBuf,
    pub db_path: PathBuf,
}

impl SqliteStateStore {
    pub fn open(root: PathBuf) -> StoreResult<Self> {
        std::fs::create_dir_all(&root)?;
        let db_path = root.join(DB_FILE);
        let store = Self { root, db_path };
        let connection = store.connection()?;
        initialize_schema(&connection)?;
        Ok(store)
    }

    fn connection(&self) -> StoreResult<Connection> {
        let connection = Connection::open(&self.db_path)?;
        connection.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            PRAGMA busy_timeout = 5000;
            PRAGMA synchronous = NORMAL;
            ",
        )?;
        Ok(connection)
    }
}

impl MountRepository for SqliteStateStore {
    fn save_mount(&mut self, mount: MountConfig) -> StoreResult<()> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO mounts (mount_id, connector, root, remote_root_id, read_only, projection_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(mount_id) DO UPDATE SET
                connector = excluded.connector,
                root = excluded.root,
                remote_root_id = excluded.remote_root_id,
                read_only = excluded.read_only,
                projection_json = excluded.projection_json",
            params![
                mount.mount_id.0,
                mount.connector,
                path_to_text(&mount.root),
                mount.remote_root_id.map(|remote_id| remote_id.0),
                bool_to_int(mount.read_only),
                to_json(&mount.projection)?,
            ],
        )?;
        Ok(())
    }

    fn get_mount(&self, mount_id: &MountId) -> StoreResult<Option<MountConfig>> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT mount_id, connector, root, remote_root_id, read_only, projection_json
                 FROM mounts
                 WHERE mount_id = ?1",
                params![mount_id.0],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?
            .map(mount_from_row)
            .transpose()
    }

    fn load_mounts(&self) -> StoreResult<Vec<MountConfig>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT mount_id, connector, root, remote_root_id, read_only, projection_json
             FROM mounts
             ORDER BY mount_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;

        rows.map(|row| mount_from_row(row?)).collect()
    }
}

impl EntityRepository for SqliteStateStore {
    fn save_entity(&mut self, entity: EntityRecord) -> StoreResult<()> {
        let connection = self.connection()?;
        let path = path_to_text(&entity.path);
        let existing_remote_id: Option<String> = connection
            .query_row(
                "SELECT remote_id
                 FROM entities
                 WHERE mount_id = ?1 AND path = ?2",
                params![entity.mount_id.0, path],
                |row| row.get(0),
            )
            .optional()?;

        if existing_remote_id
            .as_deref()
            .is_some_and(|remote_id| remote_id != entity.remote_id.0)
        {
            return Err(StoreError::DuplicateEntityPath {
                mount_id: entity.mount_id,
                path: entity.path,
            });
        }

        connection.execute(
            "INSERT INTO entities (
                mount_id,
                remote_id,
                kind_json,
                title,
                path,
                hydration_json,
                content_hash,
                remote_edited_at
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(mount_id, remote_id) DO UPDATE SET
                kind_json = excluded.kind_json,
                title = excluded.title,
                path = excluded.path,
                hydration_json = excluded.hydration_json,
                content_hash = excluded.content_hash,
                remote_edited_at = excluded.remote_edited_at",
            params![
                entity.mount_id.0,
                entity.remote_id.0,
                to_json(&entity.kind)?,
                entity.title,
                path,
                to_json(&entity.hydration)?,
                entity.content_hash,
                entity.remote_edited_at,
            ],
        )?;
        Ok(())
    }

    fn get_entity(
        &self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<Option<EntityRecord>> {
        let connection = self.connection()?;
        let sql = format!("{ENTITY_SELECT_WITH_WHERE}WHERE mount_id = ?1 AND remote_id = ?2");
        connection
            .query_row(&sql, params![mount_id.0, remote_id.0], entity_row)
            .optional()?
            .map(entity_from_row)
            .transpose()
    }

    fn find_entity_by_path(
        &self,
        mount_id: &MountId,
        path: &Path,
    ) -> StoreResult<Option<EntityRecord>> {
        let connection = self.connection()?;
        let sql = format!("{ENTITY_SELECT_WITH_WHERE}WHERE mount_id = ?1 AND path = ?2");
        connection
            .query_row(&sql, params![mount_id.0, path_to_text(path)], entity_row)
            .optional()?
            .map(entity_from_row)
            .transpose()
    }

    fn list_entities(&self, mount_id: &MountId) -> StoreResult<Vec<EntityRecord>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            &(ENTITY_SELECT_WITH_WHERE.to_owned() + "WHERE mount_id = ?1 ORDER BY path"),
        )?;
        let rows = statement.query_map(params![mount_id.0], entity_row)?;

        rows.map(|row| entity_from_row(row?)).collect()
    }
}

impl ShadowRepository for SqliteStateStore {
    fn save_shadow(&mut self, mount_id: &MountId, shadow: ShadowDocument) -> StoreResult<()> {
        let connection = self.connection()?;
        let record = ShadowSnapshotRecord::from_document(mount_id.clone(), &shadow);
        connection.execute(
            "INSERT INTO shadows (mount_id, entity_id, frontmatter, body_hash, rendered_body, blocks_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(mount_id, entity_id) DO UPDATE SET
                frontmatter = excluded.frontmatter,
                body_hash = excluded.body_hash,
                rendered_body = excluded.rendered_body,
                blocks_json = excluded.blocks_json",
            params![
                record.mount_id.0,
                record.entity_id.0,
                record.frontmatter,
                record.body_hash,
                record.rendered_body,
                to_json(&record.blocks)?,
            ],
        )?;
        Ok(())
    }

    fn load_shadow(&self, mount_id: &MountId, entity_id: &RemoteId) -> StoreResult<ShadowDocument> {
        self.get_shadow_record(mount_id, entity_id)?
            .map(ShadowSnapshotRecord::into_document)
            .ok_or_else(|| StoreError::ShadowMissing {
                mount_id: mount_id.clone(),
                entity_id: entity_id.clone(),
            })
    }

    fn get_shadow_record(
        &self,
        mount_id: &MountId,
        entity_id: &RemoteId,
    ) -> StoreResult<Option<ShadowSnapshotRecord>> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT mount_id, entity_id, frontmatter, body_hash, rendered_body, blocks_json
                 FROM shadows
                 WHERE mount_id = ?1 AND entity_id = ?2",
                params![mount_id.0, entity_id.0],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?
            .map(shadow_from_row)
            .transpose()
    }
}

impl JournalRepository for SqliteStateStore {
    fn append_journal(&mut self, entry: JournalEntry) -> StoreResult<()> {
        if self.get_journal(&entry.push_id)?.is_some() {
            return Err(StoreError::JournalAlreadyExists(entry.push_id));
        }

        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO journals (
                push_id,
                mount_id,
                remote_ids_json,
                plan_json,
                preimages_json,
                apply_effects_json,
                status_json
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                entry.push_id.0,
                entry.mount_id.0,
                to_json(&entry.remote_ids)?,
                to_json(&entry.plan)?,
                to_json(&entry.preimages)?,
                to_json(&entry.apply_effects)?,
                to_json(&entry.status)?,
            ],
        )?;
        Ok(())
    }

    fn record_journal_apply_effects(
        &mut self,
        push_id: &PushId,
        effects: Vec<JournalApplyEffect>,
    ) -> StoreResult<()> {
        let connection = self.connection()?;
        let changed = connection.execute(
            "UPDATE journals
             SET apply_effects_json = ?2
             WHERE push_id = ?1",
            params![push_id.0, to_json(&effects)?],
        )?;

        if changed == 0 {
            return Err(StoreError::JournalMissing(push_id.clone()));
        }

        Ok(())
    }

    fn update_journal_status(
        &mut self,
        push_id: &PushId,
        status: JournalStatus,
    ) -> StoreResult<()> {
        let connection = self.connection()?;
        let changed = connection.execute(
            "UPDATE journals
             SET status_json = ?2
             WHERE push_id = ?1",
            params![push_id.0, to_json(&status)?],
        )?;

        if changed == 0 {
            return Err(StoreError::JournalMissing(push_id.clone()));
        }

        Ok(())
    }

    fn get_journal(&self, push_id: &PushId) -> StoreResult<Option<JournalEntry>> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT push_id, mount_id, remote_ids_json, plan_json, preimages_json, apply_effects_json, status_json
                 FROM journals
                 WHERE push_id = ?1",
                params![push_id.0],
                journal_row,
            )
            .optional()?
            .map(journal_from_row)
            .transpose()
    }

    fn list_journal(&self) -> StoreResult<Vec<JournalEntry>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT push_id, mount_id, remote_ids_json, plan_json, preimages_json, apply_effects_json, status_json
             FROM journals
             ORDER BY push_id",
        )?;
        let rows = statement.query_map([], journal_row)?;

        rows.map(|row| journal_from_row(row?)).collect()
    }
}

impl JournalStore for SqliteStateStore {
    fn append(&mut self, entry: JournalEntry) -> AfsResult<()> {
        self.append_journal(entry).map_err(Into::into)
    }

    fn update_status(&mut self, push_id: &PushId, status: JournalStatus) -> AfsResult<()> {
        self.update_journal_status(push_id, status)
            .map_err(Into::into)
    }

    fn record_apply_effects(
        &mut self,
        push_id: &PushId,
        effects: Vec<JournalApplyEffect>,
    ) -> AfsResult<()> {
        self.record_journal_apply_effects(push_id, effects)
            .map_err(Into::into)
    }
}

const ENTITY_SELECT_WITH_WHERE: &str = "
    SELECT mount_id, remote_id, kind_json, title, path, hydration_json, content_hash, remote_edited_at
    FROM entities
    ";

type MountRow = (String, String, String, Option<String>, i64, String);
type EntityRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
);
type ShadowRow = (String, String, String, String, String, String);
type JournalRow = (String, String, String, String, String, String, String);

fn initialize_schema(connection: &Connection) -> StoreResult<()> {
    let user_version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if user_version > SCHEMA_VERSION {
        return Err(StoreError::SchemaVersion {
            found: user_version,
            supported: SCHEMA_VERSION,
        });
    }
    if user_version == SCHEMA_VERSION {
        return Ok(());
    }

    connection.execute_batch(
        "
        PRAGMA foreign_keys = ON;
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;

        CREATE TABLE IF NOT EXISTS mounts (
            mount_id TEXT PRIMARY KEY,
            connector TEXT NOT NULL,
            root TEXT NOT NULL,
            remote_root_id TEXT,
            read_only INTEGER NOT NULL CHECK (read_only IN (0, 1)),
            projection_json TEXT NOT NULL DEFAULT '\"plain_files\"'
        );

        CREATE TABLE IF NOT EXISTS entities (
            mount_id TEXT NOT NULL,
            remote_id TEXT NOT NULL,
            kind_json TEXT NOT NULL,
            title TEXT NOT NULL,
            path TEXT NOT NULL,
            hydration_json TEXT NOT NULL,
            content_hash TEXT,
            remote_edited_at TEXT,
            PRIMARY KEY (mount_id, remote_id),
            UNIQUE (mount_id, path),
            FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS shadows (
            mount_id TEXT NOT NULL,
            entity_id TEXT NOT NULL,
            frontmatter TEXT NOT NULL DEFAULT '',
            body_hash TEXT NOT NULL,
            rendered_body TEXT NOT NULL,
            blocks_json TEXT NOT NULL,
            PRIMARY KEY (mount_id, entity_id),
            FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS journals (
            push_id TEXT PRIMARY KEY,
            mount_id TEXT NOT NULL,
            remote_ids_json TEXT NOT NULL,
            plan_json TEXT NOT NULL,
            preimages_json TEXT NOT NULL DEFAULT '[]',
            apply_effects_json TEXT NOT NULL DEFAULT '[]',
            status_json TEXT NOT NULL,
            FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
        );
        ",
    )?;

    if user_version < 2 && !column_exists(connection, "journals", "preimages_json")? {
        connection.execute_batch(
            "ALTER TABLE journals
             ADD COLUMN preimages_json TEXT NOT NULL DEFAULT '[]';",
        )?;
    }

    if user_version < 3 && !column_exists(connection, "journals", "apply_effects_json")? {
        connection.execute_batch(
            "ALTER TABLE journals
             ADD COLUMN apply_effects_json TEXT NOT NULL DEFAULT '[]';",
        )?;
    }

    if user_version < 4 && !column_exists(connection, "mounts", "remote_root_id")? {
        connection.execute_batch(
            "ALTER TABLE mounts
             ADD COLUMN remote_root_id TEXT;",
        )?;
    }

    if user_version < 5 && !column_exists(connection, "shadows", "frontmatter")? {
        connection.execute_batch(
            "ALTER TABLE shadows
             ADD COLUMN frontmatter TEXT NOT NULL DEFAULT '';",
        )?;
    }

    if user_version < 6 && !column_exists(connection, "mounts", "projection_json")? {
        connection.execute_batch(
            "ALTER TABLE mounts
             ADD COLUMN projection_json TEXT NOT NULL DEFAULT '\"plain_files\"';",
        )?;
    }

    if user_version < SCHEMA_VERSION {
        connection.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
    }

    Ok(())
}

fn mount_from_row(row: MountRow) -> StoreResult<MountConfig> {
    Ok(MountConfig {
        mount_id: MountId(row.0),
        connector: row.1,
        root: PathBuf::from(row.2),
        remote_root_id: row.3.map(RemoteId),
        read_only: row.4 != 0,
        projection: from_json::<ProjectionMode>(&row.5)?,
    })
}

fn entity_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EntityRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

fn entity_from_row(row: EntityRow) -> StoreResult<EntityRecord> {
    Ok(EntityRecord {
        mount_id: MountId(row.0),
        remote_id: RemoteId(row.1),
        kind: from_json::<EntityKind>(&row.2)?,
        title: row.3,
        path: PathBuf::from(row.4),
        hydration: from_json::<HydrationState>(&row.5)?,
        content_hash: row.6,
        remote_edited_at: row.7,
    })
}

fn shadow_from_row(row: ShadowRow) -> StoreResult<ShadowSnapshotRecord> {
    Ok(ShadowSnapshotRecord {
        mount_id: MountId(row.0),
        entity_id: RemoteId(row.1),
        frontmatter: row.2,
        body_hash: row.3,
        rendered_body: row.4,
        blocks: from_json::<Vec<ShadowBlockRecord>>(&row.5)?,
    })
}

fn journal_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<JournalRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

fn journal_from_row(row: JournalRow) -> StoreResult<JournalEntry> {
    Ok(JournalEntry {
        push_id: PushId(row.0),
        mount_id: MountId(row.1),
        remote_ids: from_json::<Vec<RemoteId>>(&row.2)?,
        plan: from_json(&row.3)?,
        preimages: from_json::<Vec<JournalPreimage>>(&row.4)?,
        apply_effects: from_json::<Vec<JournalApplyEffect>>(&row.5)?,
        status: from_json(&row.6)?,
    })
}

fn column_exists(connection: &Connection, table: &str, column: &str) -> StoreResult<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;

    for result in columns {
        if result? == column {
            return Ok(true);
        }
    }

    Ok(false)
}

fn path_to_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn bool_to_int(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

fn to_json<T: Serialize>(value: &T) -> StoreResult<String> {
    serde_json::to_string(value).map_err(Into::into)
}

fn from_json<T: DeserializeOwned>(value: &str) -> StoreResult<T> {
    serde_json::from_str(value).map_err(Into::into)
}
