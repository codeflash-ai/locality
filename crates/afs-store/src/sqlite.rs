//! SQLite state-store implementation.
//!
//! This is the first durable adapter for the repository traits. It keeps the
//! schema intentionally compact: path-addressable facts live in relational
//! columns, while shadow block arrays and journal plans are stored as JSON blobs
//! until query needs justify normalization.

use std::path::{Path, PathBuf};

use afs_core::AfsResult;
use afs_core::hydration::HydrationReason;
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
    ConnectionId, ConnectionRecord, ConnectorProfileId, ConnectorProfileRecord, EntityRecord,
    HydrationJobRecord, MountConfig, ProjectionMode, ShadowBlockRecord, ShadowSnapshotRecord,
    VirtualMutationKind, VirtualMutationRecord,
};
use crate::repository::{
    ConnectionRepository, ConnectorProfileRepository, EntityRepository, HydrationJobRepository,
    JournalRepository, MountRepository, ShadowRepository, VirtualMutationRepository,
};

const DB_FILE: &str = "state.sqlite3";
const SCHEMA_VERSION: i64 = 10;

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
            PRAGMA busy_timeout = 10000;
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
            "INSERT INTO mounts (mount_id, connector, root, remote_root_id, read_only, projection_json, connection_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(mount_id) DO UPDATE SET
                connector = excluded.connector,
                root = excluded.root,
                remote_root_id = excluded.remote_root_id,
                read_only = excluded.read_only,
                projection_json = excluded.projection_json,
                connection_id = excluded.connection_id",
            params![
                mount.mount_id.0,
                mount.connector,
                path_to_text(&mount.root),
                mount.remote_root_id.map(|remote_id| remote_id.0),
                bool_to_int(mount.read_only),
                to_json(&mount.projection)?,
                mount.connection_id.map(|connection_id| connection_id.0),
            ],
        )?;
        Ok(())
    }

    fn get_mount(&self, mount_id: &MountId) -> StoreResult<Option<MountConfig>> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT mount_id, connector, root, remote_root_id, read_only, projection_json, connection_id
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
                        row.get::<_, Option<String>>(6)?,
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
            "SELECT mount_id, connector, root, remote_root_id, read_only, projection_json, connection_id
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
                row.get::<_, Option<String>>(6)?,
            ))
        })?;

        rows.map(|row| mount_from_row(row?)).collect()
    }
}

impl ConnectionRepository for SqliteStateStore {
    fn save_connection(&mut self, connection_record: ConnectionRecord) -> StoreResult<()> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO connections (
                connection_id,
                profile_id,
                connector,
                display_name,
                account_label,
                workspace_id,
                workspace_name,
                auth_kind,
                secret_ref,
                scopes_json,
                capabilities_json,
                status,
                created_at,
                updated_at,
                expires_at
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
             ON CONFLICT(connection_id) DO UPDATE SET
                profile_id = excluded.profile_id,
                connector = excluded.connector,
                display_name = excluded.display_name,
                account_label = excluded.account_label,
                workspace_id = excluded.workspace_id,
                workspace_name = excluded.workspace_name,
                auth_kind = excluded.auth_kind,
                secret_ref = excluded.secret_ref,
                scopes_json = excluded.scopes_json,
                capabilities_json = excluded.capabilities_json,
                status = excluded.status,
                updated_at = excluded.updated_at,
                expires_at = excluded.expires_at",
            params![
                connection_record.connection_id.0,
                connection_record.profile_id.map(|profile_id| profile_id.0),
                connection_record.connector,
                connection_record.display_name,
                connection_record.account_label,
                connection_record.workspace_id,
                connection_record.workspace_name,
                connection_record.auth_kind,
                connection_record.secret_ref,
                to_json(&connection_record.scopes)?,
                connection_record.capabilities_json,
                connection_record.status,
                connection_record.created_at,
                connection_record.updated_at,
                connection_record.expires_at,
            ],
        )?;
        Ok(())
    }

    fn get_connection(
        &self,
        connection_id: &ConnectionId,
    ) -> StoreResult<Option<ConnectionRecord>> {
        let connection = self.connection()?;
        let sql = CONNECTION_SELECT_WITH_WHERE.to_owned() + "WHERE connection_id = ?1";
        connection
            .query_row(&sql, params![connection_id.0], connection_row)
            .optional()?
            .map(connection_from_row)
            .transpose()
    }

    fn list_connections(&self) -> StoreResult<Vec<ConnectionRecord>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            &(CONNECTION_SELECT_WITH_WHERE.to_owned() + "ORDER BY connector, connection_id"),
        )?;
        let rows = statement.query_map([], connection_row)?;

        rows.map(|row| connection_from_row(row?)).collect()
    }

    fn delete_connection(&mut self, connection_id: &ConnectionId) -> StoreResult<()> {
        let connection = self.connection()?;
        connection.execute(
            "DELETE FROM connections WHERE connection_id = ?1",
            params![connection_id.0],
        )?;
        Ok(())
    }
}

impl ConnectorProfileRepository for SqliteStateStore {
    fn save_connector_profile(&mut self, profile: ConnectorProfileRecord) -> StoreResult<()> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO connector_profiles (
                profile_id,
                connector,
                display_name,
                auth_kind,
                scopes_json,
                capabilities_json,
                enabled_actions_json,
                connector_version,
                status,
                created_at,
                updated_at
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(profile_id) DO UPDATE SET
                connector = excluded.connector,
                display_name = excluded.display_name,
                auth_kind = excluded.auth_kind,
                scopes_json = excluded.scopes_json,
                capabilities_json = excluded.capabilities_json,
                enabled_actions_json = excluded.enabled_actions_json,
                connector_version = excluded.connector_version,
                status = excluded.status,
                updated_at = excluded.updated_at",
            params![
                profile.profile_id.0,
                profile.connector,
                profile.display_name,
                profile.auth_kind,
                to_json(&profile.scopes)?,
                profile.capabilities_json,
                profile.enabled_actions_json,
                profile.connector_version,
                profile.status,
                profile.created_at,
                profile.updated_at,
            ],
        )?;
        Ok(())
    }

    fn get_connector_profile(
        &self,
        profile_id: &ConnectorProfileId,
    ) -> StoreResult<Option<ConnectorProfileRecord>> {
        let connection = self.connection()?;
        let sql = CONNECTOR_PROFILE_SELECT_WITH_WHERE.to_owned() + "WHERE profile_id = ?1";
        connection
            .query_row(&sql, params![profile_id.0], connector_profile_row)
            .optional()?
            .map(connector_profile_from_row)
            .transpose()
    }

    fn list_connector_profiles(&self) -> StoreResult<Vec<ConnectorProfileRecord>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            &(CONNECTOR_PROFILE_SELECT_WITH_WHERE.to_owned() + "ORDER BY connector, profile_id"),
        )?;
        let rows = statement.query_map([], connector_profile_row)?;

        rows.map(|row| connector_profile_from_row(row?)).collect()
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

    fn delete_entity(&mut self, mount_id: &MountId, remote_id: &RemoteId) -> StoreResult<()> {
        let connection = self.connection()?;
        connection.execute(
            "DELETE FROM entities WHERE mount_id = ?1 AND remote_id = ?2",
            params![mount_id.0, remote_id.0],
        )?;
        Ok(())
    }
}

impl HydrationJobRepository for SqliteStateStore {
    fn upsert_hydration_job(&mut self, job: HydrationJobRecord) -> StoreResult<()> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO hydration_jobs (
                mount_id,
                remote_id,
                path,
                target_state_json,
                reason_json,
                attempts,
                last_error
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(mount_id, remote_id) DO UPDATE SET
                path = excluded.path,
                target_state_json = excluded.target_state_json,
                reason_json = excluded.reason_json",
            params![
                job.mount_id.0,
                job.remote_id.0,
                path_to_text(&job.path),
                to_json(&job.target_state)?,
                to_json(&job.reason)?,
                i64::from(job.attempts),
                job.last_error,
            ],
        )?;
        Ok(())
    }

    fn list_hydration_jobs(&self) -> StoreResult<Vec<HydrationJobRecord>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT mount_id, remote_id, path, target_state_json, reason_json, attempts, last_error
             FROM hydration_jobs
             ORDER BY attempts, mount_id, remote_id",
        )?;
        let rows = statement.query_map([], hydration_job_row)?;

        rows.map(|row| hydration_job_from_row(row?)).collect()
    }

    fn delete_hydration_job(
        &mut self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<()> {
        let connection = self.connection()?;
        connection.execute(
            "DELETE FROM hydration_jobs WHERE mount_id = ?1 AND remote_id = ?2",
            params![mount_id.0, remote_id.0],
        )?;
        Ok(())
    }

    fn record_hydration_job_failure(
        &mut self,
        mount_id: &MountId,
        remote_id: &RemoteId,
        message: String,
    ) -> StoreResult<()> {
        let connection = self.connection()?;
        connection.execute(
            "UPDATE hydration_jobs
             SET attempts = attempts + 1,
                 last_error = ?3
             WHERE mount_id = ?1 AND remote_id = ?2",
            params![mount_id.0, remote_id.0, message],
        )?;
        Ok(())
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

impl VirtualMutationRepository for SqliteStateStore {
    fn save_virtual_mutation(&mut self, mutation: VirtualMutationRecord) -> StoreResult<()> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO virtual_mutations (
                mount_id,
                local_id,
                mutation_kind_json,
                target_remote_id,
                parent_remote_id,
                original_path,
                projected_path,
                title,
                content_path,
                created_at,
                updated_at
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(mount_id, local_id) DO UPDATE SET
                mutation_kind_json = excluded.mutation_kind_json,
                target_remote_id = excluded.target_remote_id,
                parent_remote_id = excluded.parent_remote_id,
                original_path = excluded.original_path,
                projected_path = excluded.projected_path,
                title = excluded.title,
                content_path = excluded.content_path,
                updated_at = excluded.updated_at",
            params![
                mutation.mount_id.0,
                mutation.local_id,
                to_json(&mutation.mutation_kind)?,
                mutation.target_remote_id.map(|remote_id| remote_id.0),
                mutation.parent_remote_id.map(|remote_id| remote_id.0),
                mutation
                    .original_path
                    .as_ref()
                    .map(|path| path_to_text(path)),
                path_to_text(&mutation.projected_path),
                mutation.title,
                mutation
                    .content_path
                    .as_ref()
                    .map(|path| path_to_text(path)),
                mutation.created_at,
                mutation.updated_at,
            ],
        )?;
        Ok(())
    }

    fn get_virtual_mutation(
        &self,
        mount_id: &MountId,
        local_id: &str,
    ) -> StoreResult<Option<VirtualMutationRecord>> {
        let connection = self.connection()?;
        let sql =
            VIRTUAL_MUTATION_SELECT_WITH_WHERE.to_owned() + "WHERE mount_id = ?1 AND local_id = ?2";
        connection
            .query_row(&sql, params![mount_id.0, local_id], virtual_mutation_row)
            .optional()?
            .map(virtual_mutation_from_row)
            .transpose()
    }

    fn find_virtual_mutation_by_path(
        &self,
        mount_id: &MountId,
        path: &Path,
    ) -> StoreResult<Option<VirtualMutationRecord>> {
        let connection = self.connection()?;
        let sql = VIRTUAL_MUTATION_SELECT_WITH_WHERE.to_owned()
            + "WHERE mount_id = ?1 AND projected_path = ?2";
        connection
            .query_row(
                &sql,
                params![mount_id.0, path_to_text(path)],
                virtual_mutation_row,
            )
            .optional()?
            .map(virtual_mutation_from_row)
            .transpose()
    }

    fn list_virtual_mutations(
        &self,
        mount_id: &MountId,
    ) -> StoreResult<Vec<VirtualMutationRecord>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            &(VIRTUAL_MUTATION_SELECT_WITH_WHERE.to_owned()
                + "WHERE mount_id = ?1 ORDER BY projected_path, local_id"),
        )?;
        let rows = statement.query_map(params![mount_id.0], virtual_mutation_row)?;

        rows.map(|row| virtual_mutation_from_row(row?)).collect()
    }

    fn delete_virtual_mutation(&mut self, mount_id: &MountId, local_id: &str) -> StoreResult<()> {
        let connection = self.connection()?;
        connection.execute(
            "DELETE FROM virtual_mutations WHERE mount_id = ?1 AND local_id = ?2",
            params![mount_id.0, local_id],
        )?;
        Ok(())
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
const CONNECTION_SELECT_WITH_WHERE: &str = "
    SELECT connection_id, profile_id, connector, display_name, account_label, workspace_id, workspace_name,
           auth_kind, secret_ref, scopes_json, capabilities_json, status, created_at, updated_at,
           expires_at
    FROM connections
    ";
const CONNECTOR_PROFILE_SELECT_WITH_WHERE: &str = "
    SELECT profile_id, connector, display_name, auth_kind, scopes_json, capabilities_json,
           enabled_actions_json, connector_version, status, created_at, updated_at
    FROM connector_profiles
    ";
const VIRTUAL_MUTATION_SELECT_WITH_WHERE: &str = "
    SELECT mount_id, local_id, mutation_kind_json, target_remote_id, parent_remote_id,
           original_path, projected_path, title, content_path, created_at, updated_at
    FROM virtual_mutations
    ";

type MountRow = (
    String,
    String,
    String,
    Option<String>,
    i64,
    String,
    Option<String>,
);
type ConnectionRow = (
    String,
    Option<String>,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
);
type ConnectorProfileRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
);
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
type HydrationJobRow = (String, String, String, String, String, i64, Option<String>);
type ShadowRow = (String, String, String, String, String, String);
type JournalRow = (String, String, String, String, String, String, String);
type VirtualMutationRow = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    String,
    Option<String>,
    String,
    String,
);

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
            projection_json TEXT NOT NULL DEFAULT '\"plain_files\"',
            connection_id TEXT
        );

        CREATE TABLE IF NOT EXISTS connections (
            connection_id TEXT PRIMARY KEY,
            profile_id TEXT,
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

        CREATE TABLE IF NOT EXISTS connector_profiles (
            profile_id TEXT PRIMARY KEY,
            connector TEXT NOT NULL,
            display_name TEXT NOT NULL,
            auth_kind TEXT NOT NULL,
            scopes_json TEXT NOT NULL DEFAULT '[]',
            capabilities_json TEXT NOT NULL DEFAULT '{}',
            enabled_actions_json TEXT NOT NULL DEFAULT '[]',
            connector_version TEXT NOT NULL,
            status TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
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

        CREATE TABLE IF NOT EXISTS hydration_jobs (
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

        CREATE TABLE IF NOT EXISTS virtual_mutations (
            mount_id TEXT NOT NULL,
            local_id TEXT NOT NULL,
            mutation_kind_json TEXT NOT NULL,
            target_remote_id TEXT,
            parent_remote_id TEXT,
            original_path TEXT,
            projected_path TEXT NOT NULL,
            title TEXT NOT NULL,
            content_path TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (mount_id, local_id),
            UNIQUE (mount_id, projected_path),
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

    if user_version < 7 {
        if !column_exists(connection, "mounts", "connection_id")? {
            connection.execute_batch(
                "ALTER TABLE mounts
                 ADD COLUMN connection_id TEXT;",
            )?;
        }
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS connections (
                connection_id TEXT PRIMARY KEY,
                profile_id TEXT,
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
            );",
        )?;
    }

    if user_version < 8 {
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS hydration_jobs (
                mount_id TEXT NOT NULL,
                remote_id TEXT NOT NULL,
                path TEXT NOT NULL,
                target_state_json TEXT NOT NULL,
                reason_json TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                last_error TEXT,
                PRIMARY KEY (mount_id, remote_id),
                FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
            );",
        )?;
    }

    if user_version < 9 {
        if !column_exists(connection, "connections", "profile_id")? {
            connection.execute_batch(
                "ALTER TABLE connections
                 ADD COLUMN profile_id TEXT;",
            )?;
        }
        seed_default_notion_profile(connection)?;
        connection.execute_batch(
            "UPDATE connections
             SET profile_id = 'notion-token-default'
             WHERE profile_id IS NULL AND connector = 'notion';",
        )?;
    }

    if user_version < 10 {
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS virtual_mutations (
                mount_id TEXT NOT NULL,
                local_id TEXT NOT NULL,
                mutation_kind_json TEXT NOT NULL,
                target_remote_id TEXT,
                parent_remote_id TEXT,
                original_path TEXT,
                projected_path TEXT NOT NULL,
                title TEXT NOT NULL,
                content_path TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (mount_id, local_id),
                UNIQUE (mount_id, projected_path),
                FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
            );",
        )?;
    }

    if user_version < SCHEMA_VERSION {
        seed_default_notion_profile(connection)?;
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
        connection_id: row.6.map(ConnectionId),
    })
}

fn connection_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConnectionRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
        row.get(14)?,
    ))
}

fn connection_from_row(row: ConnectionRow) -> StoreResult<ConnectionRecord> {
    Ok(ConnectionRecord {
        connection_id: ConnectionId(row.0),
        profile_id: row.1.map(ConnectorProfileId),
        connector: row.2,
        display_name: row.3,
        account_label: row.4,
        workspace_id: row.5,
        workspace_name: row.6,
        auth_kind: row.7,
        secret_ref: row.8,
        scopes: from_json::<Vec<String>>(&row.9)?,
        capabilities_json: row.10,
        status: row.11,
        created_at: row.12,
        updated_at: row.13,
        expires_at: row.14,
    })
}

fn connector_profile_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConnectorProfileRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
    ))
}

fn connector_profile_from_row(row: ConnectorProfileRow) -> StoreResult<ConnectorProfileRecord> {
    Ok(ConnectorProfileRecord {
        profile_id: ConnectorProfileId(row.0),
        connector: row.1,
        display_name: row.2,
        auth_kind: row.3,
        scopes: from_json::<Vec<String>>(&row.4)?,
        capabilities_json: row.5,
        enabled_actions_json: row.6,
        connector_version: row.7,
        status: row.8,
        created_at: row.9,
        updated_at: row.10,
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

fn hydration_job_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HydrationJobRow> {
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

fn hydration_job_from_row(row: HydrationJobRow) -> StoreResult<HydrationJobRecord> {
    let attempts = u32::try_from(row.5)
        .map_err(|_| StoreError::Database(format!("invalid hydration attempt count {}", row.5)))?;

    Ok(HydrationJobRecord {
        mount_id: MountId(row.0),
        remote_id: RemoteId(row.1),
        path: PathBuf::from(row.2),
        target_state: from_json::<HydrationState>(&row.3)?,
        reason: from_json::<HydrationReason>(&row.4)?,
        attempts,
        last_error: row.6,
    })
}

fn virtual_mutation_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<VirtualMutationRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
    ))
}

fn virtual_mutation_from_row(row: VirtualMutationRow) -> StoreResult<VirtualMutationRecord> {
    Ok(VirtualMutationRecord {
        mount_id: MountId(row.0),
        local_id: row.1,
        mutation_kind: from_json::<VirtualMutationKind>(&row.2)?,
        target_remote_id: row.3.map(RemoteId),
        parent_remote_id: row.4.map(RemoteId),
        original_path: row.5.map(PathBuf::from),
        projected_path: PathBuf::from(row.6),
        title: row.7,
        content_path: row.8.map(PathBuf::from),
        created_at: row.9,
        updated_at: row.10,
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

fn seed_default_notion_profile(connection: &Connection) -> StoreResult<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS connector_profiles (
            profile_id TEXT PRIMARY KEY,
            connector TEXT NOT NULL,
            display_name TEXT NOT NULL,
            auth_kind TEXT NOT NULL,
            scopes_json TEXT NOT NULL DEFAULT '[]',
            capabilities_json TEXT NOT NULL DEFAULT '{}',
            enabled_actions_json TEXT NOT NULL DEFAULT '[]',
            connector_version TEXT NOT NULL,
            status TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );",
    )?;
    connection.execute(
        "INSERT OR IGNORE INTO connector_profiles (
            profile_id,
            connector,
            display_name,
            auth_kind,
            scopes_json,
            capabilities_json,
            enabled_actions_json,
            connector_version,
            status,
            created_at,
            updated_at
         )
         VALUES (
            'notion-token-default',
            'notion',
            'Notion token auth',
            'token',
            '[]',
            '{}',
            '[\"read\",\"write\"]',
            'notion.v1',
            'active',
            '0',
            '0'
         )",
        [],
    )?;
    Ok(())
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
