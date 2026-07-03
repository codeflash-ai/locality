//! SQLite state-store implementation.
//!
//! This is the first durable adapter for the repository traits. It keeps the
//! schema intentionally compact: path-addressable facts live in relational
//! columns, while shadow block arrays and journal plans are stored as JSON blobs
//! until query needs justify normalization.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use locality_core::LocalityResult;
use locality_core::hydration::HydrationReason;
use locality_core::journal::{
    JournalApplyEffect, JournalEntry, JournalPreimage, JournalStatus, JournalStore, PushId,
};
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use locality_core::planner::{PlanSummary, PushOperation, PushPlan};
use locality_core::shadow::ShadowDocument;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::compatibility::{
    StateCompatibilityIssue, StateCompatibilityReport, StateCompatibilityStatus,
    StateComponentDefinition, StateComponentRecord,
};
use crate::error::{StoreError, StoreResult};
use crate::records::{
    AutoSaveEnrollmentRecord, ConnectionId, ConnectionRecord, ConnectorProfileId,
    ConnectorProfileRecord, EntityRecord, FreshnessStateRecord, HydrationJobRecord, MountConfig,
    MountLiveModeRecord, MountLiveModeState, ProjectionMode, RemoteObservationRecord,
    ShadowBlockRecord, ShadowSnapshotRecord, VirtualMutationKind, VirtualMutationRecord,
};
use crate::repository::{
    AutoSaveRepository, ConnectionRepository, ConnectorProfileRepository, EntityRepository,
    EntitySearchCandidate, EntitySearchRepository, FreshnessStateRepository,
    HydrationJobRepository, JournalRepository, MountLiveModeRepository, MountRepository,
    RemoteObservationRepository, ShadowRepository, VirtualMutationRepository,
};

const DB_FILE: &str = "state.sqlite3";
const SCHEMA_VERSION: i64 = 15;
const LINUX_FUSE_PROJECTION_LAYOUT_VERSION: i64 = 2;
const WINDOWS_CLOUD_FILES_PROJECTION_LAYOUT_VERSION: i64 = 2;
const NOTION_WORKSPACE_ROOTS_PROJECTION_LAYOUT_VERSION: i64 = 1;
const NOTION_WORKSPACE_ROOTS_COMPONENT_ID: &str = "projection:notion_workspace_roots";
const NOTION_PRIVATE_ROOT_ID: &str = "notion-root:private";
const NOTION_WORKSPACE_ROOT_ID: &str = "notion-root:workspace";
const NOTION_PRIVATE_ROOT_DIR: &str = "Private";
const NOTION_WORKSPACE_ROOT_DIR: &str = "Workspace";
const ENTITY_SEARCH_CANDIDATE_LIMIT: i64 = 256;
const DEFAULT_NOTION_CAPABILITIES_JSON: &str = "{\"supports_block_updates\":true,\"supports_databases\":true,\"supports_oauth\":true,\"supports_remote_observation\":true,\"supports_lazy_child_enumeration\":true,\"supports_media_download\":true,\"supports_undo\":true,\"supports_batch_observation\":false}";
const CURRENT_COMPONENT_DEFINITIONS: &[StateComponentDefinition] = &[
    StateComponentDefinition {
        component_id: "core:schema",
        component_kind: "schema",
        current_version: SCHEMA_VERSION,
        min_reader_version: 1,
        required: true,
        rebuildable: false,
        data_json: "{}",
    },
    StateComponentDefinition {
        component_id: "connector:notion",
        component_kind: "connector_state",
        current_version: 1,
        min_reader_version: 1,
        required: true,
        rebuildable: false,
        data_json: "{\"connector_version\":\"notion.v1\"}",
    },
    StateComponentDefinition {
        component_id: "projection:plain_files",
        component_kind: "projection_layout",
        current_version: 1,
        min_reader_version: 1,
        required: true,
        rebuildable: false,
        data_json: "{}",
    },
    StateComponentDefinition {
        component_id: "projection:macos_file_provider",
        component_kind: "projection_layout",
        current_version: 1,
        min_reader_version: 1,
        required: true,
        rebuildable: false,
        data_json: "{}",
    },
    StateComponentDefinition {
        component_id: NOTION_WORKSPACE_ROOTS_COMPONENT_ID,
        component_kind: "projection_layout",
        current_version: NOTION_WORKSPACE_ROOTS_PROJECTION_LAYOUT_VERSION,
        min_reader_version: 1,
        required: true,
        rebuildable: false,
        data_json: "{}",
    },
    StateComponentDefinition {
        component_id: "projection:linux_fuse",
        component_kind: "projection_layout",
        current_version: LINUX_FUSE_PROJECTION_LAYOUT_VERSION,
        min_reader_version: 1,
        required: true,
        rebuildable: false,
        data_json: "{}",
    },
    StateComponentDefinition {
        component_id: "projection:windows_cloud_files",
        component_kind: "projection_layout",
        current_version: WINDOWS_CLOUD_FILES_PROJECTION_LAYOUT_VERSION,
        min_reader_version: 1,
        required: true,
        rebuildable: false,
        data_json: "{}",
    },
    StateComponentDefinition {
        component_id: "durable:journals",
        component_kind: "durable_json",
        current_version: 1,
        min_reader_version: 1,
        required: true,
        rebuildable: false,
        data_json: "{}",
    },
    StateComponentDefinition {
        component_id: "durable:virtual_mutations",
        component_kind: "durable_json",
        current_version: 1,
        min_reader_version: 1,
        required: true,
        rebuildable: false,
        data_json: "{}",
    },
    StateComponentDefinition {
        component_id: "durable:auto_save",
        component_kind: "durable_json",
        current_version: 1,
        min_reader_version: 1,
        required: true,
        rebuildable: false,
        data_json: "{}",
    },
    StateComponentDefinition {
        component_id: "durable:live_mode",
        component_kind: "durable_json",
        current_version: 1,
        min_reader_version: 1,
        required: true,
        rebuildable: false,
        data_json: "{}",
    },
    StateComponentDefinition {
        component_id: "auth:connections",
        component_kind: "secret_binding",
        current_version: 1,
        min_reader_version: 1,
        required: true,
        rebuildable: false,
        data_json: "{}",
    },
    StateComponentDefinition {
        component_id: "cache:entity_search",
        component_kind: "rebuildable_cache",
        current_version: 1,
        min_reader_version: 1,
        required: false,
        rebuildable: true,
        data_json: "{}",
    },
];

#[derive(Clone, Debug)]
pub struct SqliteStateStore {
    pub root: PathBuf,
    pub db_path: PathBuf,
}

impl SqliteStateStore {
    pub fn current_schema_version() -> i64 {
        SCHEMA_VERSION
    }

    pub fn current_component_definitions() -> &'static [StateComponentDefinition] {
        CURRENT_COMPONENT_DEFINITIONS
    }

    pub fn inspect_compatibility(root: PathBuf) -> StoreResult<StateCompatibilityReport> {
        inspect_state_compatibility(root)
    }

    pub fn open(root: PathBuf) -> StoreResult<Self> {
        std::fs::create_dir_all(&root)?;
        let db_path = root.join(DB_FILE);
        let store = Self { root, db_path };
        let connection = store.connection()?;
        initialize_schema(&connection)?;
        ensure_current_state_is_readable(&connection)?;
        Ok(store)
    }

    pub fn clear_mount_source_state(&mut self, mount_id: &MountId) -> StoreResult<()> {
        let connection = self.connection()?;
        clear_mount_source_state(&connection, mount_id)
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
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let existing = transaction
            .query_row(
                "SELECT mount_id, connector, root, remote_root_id, read_only, projection_json, connection_id
                 FROM mounts
                 WHERE mount_id = ?1",
                params![&mount.mount_id.0],
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
            .transpose()?;
        if existing
            .as_ref()
            .is_some_and(|existing| mount_source_identity_changed(existing, &mount))
        {
            clear_mount_source_state(&transaction, &mount.mount_id)?;
        }

        transaction.execute(
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
                &mount.mount_id.0,
                &mount.connector,
                path_to_text(&mount.root),
                mount.remote_root_id.as_ref().map(|remote_id| remote_id.0.as_str()),
                bool_to_int(mount.read_only),
                to_json(&mount.projection)?,
                mount.connection_id.as_ref().map(|connection_id| connection_id.0.as_str()),
            ],
        )?;
        transaction.commit()?;
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

impl MountLiveModeRepository for SqliteStateStore {
    fn save_mount_live_mode(&mut self, live_mode: MountLiveModeRecord) -> StoreResult<()> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO mount_live_modes (
                mount_id,
                enabled,
                state_json,
                last_reason,
                last_run_at,
                created_at,
                updated_at
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(mount_id) DO UPDATE SET
                enabled = excluded.enabled,
                state_json = excluded.state_json,
                last_reason = excluded.last_reason,
                last_run_at = excluded.last_run_at,
                updated_at = excluded.updated_at",
            params![
                live_mode.mount_id.0,
                bool_to_int(live_mode.enabled),
                to_json(&live_mode.state)?,
                live_mode.last_reason,
                live_mode.last_run_at,
                live_mode.created_at,
                live_mode.updated_at,
            ],
        )?;
        Ok(())
    }

    fn get_mount_live_mode(&self, mount_id: &MountId) -> StoreResult<Option<MountLiveModeRecord>> {
        let connection = self.connection()?;
        let sql = MOUNT_LIVE_MODE_SELECT_WITH_WHERE.to_owned() + "WHERE mount_id = ?1";
        connection
            .query_row(&sql, params![mount_id.0], mount_live_mode_row)
            .optional()?
            .map(mount_live_mode_from_row)
            .transpose()
    }

    fn list_mount_live_modes(&self) -> StoreResult<Vec<MountLiveModeRecord>> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(&(MOUNT_LIVE_MODE_SELECT_WITH_WHERE.to_owned() + "ORDER BY mount_id"))?;
        let rows = statement.query_map([], mount_live_mode_row)?;

        rows.map(|row| mount_live_mode_from_row(row?)).collect()
    }

    fn delete_mount_live_mode(&mut self, mount_id: &MountId) -> StoreResult<()> {
        let connection = self.connection()?;
        connection.execute(
            "DELETE FROM mount_live_modes WHERE mount_id = ?1",
            params![mount_id.0],
        )?;
        Ok(())
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
        let path = logical_path_to_text(&entity.path);
        let kind_json = to_json(&entity.kind)?;
        let hydration_json = to_json(&entity.hydration)?;
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
                &entity.mount_id.0,
                &entity.remote_id.0,
                &kind_json,
                &entity.title,
                &path,
                &hydration_json,
                &entity.content_hash,
                &entity.remote_edited_at,
            ],
        )?;
        upsert_entity_search_index(&connection, &entity.mount_id, &entity.remote_id)?;
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
            .query_row(
                &sql,
                params![mount_id.0, logical_path_to_text(path)],
                entity_row,
            )
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
        delete_entity_search_index(&connection, mount_id, remote_id)?;
        Ok(())
    }
}

impl EntitySearchRepository for SqliteStateStore {
    fn list_entity_search_candidates(
        &self,
        mount_id: &MountId,
        query: &str,
        compact_remote_id: Option<&str>,
    ) -> StoreResult<Option<Vec<EntitySearchCandidate>>> {
        let connection = self.connection()?;
        let remote_ids = if let Some(compact_remote_id) = compact_remote_id {
            let mut statement = connection.prepare(
                "SELECT remote_id
                 FROM entities
                 WHERE mount_id = ?1
                   AND replace(lower(remote_id), '-', '') = ?2
                 LIMIT ?3",
            )?;
            let rows = statement.query_map(
                params![mount_id.0, compact_remote_id, ENTITY_SEARCH_CANDIDATE_LIMIT],
                |row| row.get::<_, String>(0),
            )?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            let Some(match_query) = entity_search_match_query(query) else {
                return Ok(Some(Vec::new()));
            };
            let mut statement = connection.prepare(
                "SELECT remote_id
                 FROM entity_search_fts
                 WHERE entity_search_fts MATCH ?1
                   AND mount_id = ?2
                 ORDER BY bm25(entity_search_fts)
                 LIMIT ?3",
            )?;
            let rows = statement.query_map(
                params![match_query, mount_id.0, ENTITY_SEARCH_CANDIDATE_LIMIT],
                |row| row.get::<_, String>(0),
            )?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut candidates = Vec::with_capacity(remote_ids.len());
        for remote_id in remote_ids {
            let remote_id = RemoteId(remote_id);
            if let Some(entity) = self.get_entity(mount_id, &remote_id)? {
                candidates.push(EntitySearchCandidate {
                    entity,
                    observation: self.get_remote_observation(mount_id, &remote_id)?,
                });
            }
        }

        Ok(Some(candidates))
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
                    .map(|path| logical_path_to_text(path)),
                logical_path_to_text(&mutation.projected_path),
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
                params![mount_id.0, logical_path_to_text(path)],
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

impl AutoSaveRepository for SqliteStateStore {
    fn save_auto_save_enrollment(
        &mut self,
        enrollment: AutoSaveEnrollmentRecord,
    ) -> StoreResult<()> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO auto_save_enrollments (
                mount_id,
                path,
                remote_id,
                enabled,
                origin_json,
                state_json,
                last_reason,
                last_push_id,
                created_at,
                updated_at
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(mount_id, path) DO UPDATE SET
                remote_id = excluded.remote_id,
                enabled = excluded.enabled,
                origin_json = excluded.origin_json,
                state_json = excluded.state_json,
                last_reason = excluded.last_reason,
                last_push_id = excluded.last_push_id,
                updated_at = excluded.updated_at",
            params![
                enrollment.mount_id.0,
                path_to_text(&enrollment.path),
                enrollment.remote_id.map(|remote_id| remote_id.0),
                bool_to_int(enrollment.enabled),
                to_json(&enrollment.origin)?,
                to_json(&enrollment.state)?,
                enrollment.last_reason,
                enrollment.last_push_id,
                enrollment.created_at,
                enrollment.updated_at,
            ],
        )?;
        Ok(())
    }

    fn get_auto_save_enrollment(
        &self,
        mount_id: &MountId,
        path: &Path,
    ) -> StoreResult<Option<AutoSaveEnrollmentRecord>> {
        let connection = self.connection()?;
        let sql = AUTO_SAVE_SELECT_WITH_WHERE.to_owned() + "WHERE mount_id = ?1 AND path = ?2";
        connection
            .query_row(
                &sql,
                params![mount_id.0, path_to_text(path)],
                auto_save_enrollment_row,
            )
            .optional()?
            .map(auto_save_enrollment_from_row)
            .transpose()
    }

    fn find_auto_save_enrollment_by_remote_id(
        &self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<Option<AutoSaveEnrollmentRecord>> {
        let connection = self.connection()?;
        let sql = AUTO_SAVE_SELECT_WITH_WHERE.to_owned() + "WHERE mount_id = ?1 AND remote_id = ?2";
        connection
            .query_row(
                &sql,
                params![mount_id.0, remote_id.0],
                auto_save_enrollment_row,
            )
            .optional()?
            .map(auto_save_enrollment_from_row)
            .transpose()
    }

    fn list_auto_save_enrollments(
        &self,
        mount_id: &MountId,
    ) -> StoreResult<Vec<AutoSaveEnrollmentRecord>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            &(AUTO_SAVE_SELECT_WITH_WHERE.to_owned() + "WHERE mount_id = ?1 ORDER BY path"),
        )?;
        let rows = statement.query_map(params![mount_id.0], auto_save_enrollment_row)?;

        rows.map(|row| auto_save_enrollment_from_row(row?))
            .collect()
    }

    fn delete_auto_save_enrollment(&mut self, mount_id: &MountId, path: &Path) -> StoreResult<()> {
        let connection = self.connection()?;
        connection.execute(
            "DELETE FROM auto_save_enrollments WHERE mount_id = ?1 AND path = ?2",
            params![mount_id.0, path_to_text(path)],
        )?;
        Ok(())
    }
}

impl RemoteObservationRepository for SqliteStateStore {
    fn save_remote_observation(&mut self, observation: RemoteObservationRecord) -> StoreResult<()> {
        let connection = self.connection()?;
        let kind_json = to_json(&observation.kind)?;
        let remote_version_json = to_json(&observation.remote_version)?;
        let parent_remote_id = observation
            .parent_remote_id
            .as_ref()
            .map(|remote_id| remote_id.0.as_str());
        let projected_path = logical_path_to_text(&observation.projected_path);
        connection.execute(
            "INSERT INTO remote_observations (
                mount_id,
                remote_id,
                kind_json,
                title,
                parent_remote_id,
                projected_path,
                remote_version_json,
                observed_at,
                deleted,
                raw_metadata_json
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(mount_id, remote_id) DO UPDATE SET
                kind_json = excluded.kind_json,
                title = excluded.title,
                parent_remote_id = excluded.parent_remote_id,
                projected_path = excluded.projected_path,
                remote_version_json = excluded.remote_version_json,
                observed_at = excluded.observed_at,
                deleted = excluded.deleted,
                raw_metadata_json = excluded.raw_metadata_json",
            params![
                &observation.mount_id.0,
                &observation.remote_id.0,
                &kind_json,
                &observation.title,
                parent_remote_id,
                &projected_path,
                &remote_version_json,
                &observation.observed_at,
                bool_to_int(observation.deleted),
                &observation.raw_metadata_json,
            ],
        )?;
        upsert_entity_search_index(&connection, &observation.mount_id, &observation.remote_id)?;
        Ok(())
    }

    fn get_remote_observation(
        &self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<Option<RemoteObservationRecord>> {
        let connection = self.connection()?;
        let sql = REMOTE_OBSERVATION_SELECT_WITH_WHERE.to_owned()
            + "WHERE mount_id = ?1 AND remote_id = ?2";
        connection
            .query_row(
                &sql,
                params![mount_id.0, remote_id.0],
                remote_observation_row,
            )
            .optional()?
            .map(remote_observation_from_row)
            .transpose()
    }

    fn list_remote_observations(
        &self,
        mount_id: &MountId,
    ) -> StoreResult<Vec<RemoteObservationRecord>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            &(REMOTE_OBSERVATION_SELECT_WITH_WHERE.to_owned()
                + "WHERE mount_id = ?1 ORDER BY projected_path, remote_id"),
        )?;
        let rows = statement.query_map(params![mount_id.0], remote_observation_row)?;

        rows.map(|row| remote_observation_from_row(row?)).collect()
    }

    fn delete_remote_observation(
        &mut self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<()> {
        let connection = self.connection()?;
        connection.execute(
            "DELETE FROM remote_observations WHERE mount_id = ?1 AND remote_id = ?2",
            params![mount_id.0, remote_id.0],
        )?;
        upsert_entity_search_index(&connection, mount_id, remote_id)?;
        Ok(())
    }
}

impl FreshnessStateRepository for SqliteStateStore {
    fn save_freshness_state(&mut self, state: FreshnessStateRecord) -> StoreResult<()> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO freshness_states (
                mount_id,
                remote_id,
                tier_json,
                last_checked_at,
                next_check_at,
                last_opened_at,
                last_local_change_at,
                remote_hint_pending
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(mount_id, remote_id) DO UPDATE SET
                tier_json = excluded.tier_json,
                last_checked_at = excluded.last_checked_at,
                next_check_at = excluded.next_check_at,
                last_opened_at = excluded.last_opened_at,
                last_local_change_at = excluded.last_local_change_at,
                remote_hint_pending = excluded.remote_hint_pending",
            params![
                state.mount_id.0,
                state.remote_id.0,
                to_json(&state.tier)?,
                state.last_checked_at,
                state.next_check_at,
                state.last_opened_at,
                state.last_local_change_at,
                bool_to_int(state.remote_hint_pending),
            ],
        )?;
        Ok(())
    }

    fn get_freshness_state(
        &self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<Option<FreshnessStateRecord>> {
        let connection = self.connection()?;
        let sql =
            FRESHNESS_STATE_SELECT_WITH_WHERE.to_owned() + "WHERE mount_id = ?1 AND remote_id = ?2";
        connection
            .query_row(&sql, params![mount_id.0, remote_id.0], freshness_state_row)
            .optional()?
            .map(freshness_state_from_row)
            .transpose()
    }

    fn list_freshness_states(&self, mount_id: &MountId) -> StoreResult<Vec<FreshnessStateRecord>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            &(FRESHNESS_STATE_SELECT_WITH_WHERE.to_owned()
                + "WHERE mount_id = ?1 ORDER BY tier_json, remote_id"),
        )?;
        let rows = statement.query_map(params![mount_id.0], freshness_state_row)?;

        rows.map(|row| freshness_state_from_row(row?)).collect()
    }

    fn delete_freshness_state(
        &mut self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<()> {
        let connection = self.connection()?;
        connection.execute(
            "DELETE FROM freshness_states WHERE mount_id = ?1 AND remote_id = ?2",
            params![mount_id.0, remote_id.0],
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
    fn append(&mut self, entry: JournalEntry) -> LocalityResult<()> {
        self.append_journal(entry).map_err(Into::into)
    }

    fn update_status(&mut self, push_id: &PushId, status: JournalStatus) -> LocalityResult<()> {
        self.update_journal_status(push_id, status)
            .map_err(Into::into)
    }

    fn record_apply_effects(
        &mut self,
        push_id: &PushId,
        effects: Vec<JournalApplyEffect>,
    ) -> LocalityResult<()> {
        self.record_journal_apply_effects(push_id, effects)
            .map_err(Into::into)
    }
}

fn mount_source_identity_changed(existing: &MountConfig, next: &MountConfig) -> bool {
    existing.connector != next.connector
        || existing.remote_root_id != next.remote_root_id
        || existing.connection_id != next.connection_id
}

fn clear_mount_source_state(connection: &Connection, mount_id: &MountId) -> StoreResult<()> {
    for table in [
        "entities",
        "shadows",
        "hydration_jobs",
        "virtual_mutations",
        "mount_live_modes",
        "auto_save_enrollments",
        "remote_observations",
        "freshness_states",
        "journals",
        "entity_search_fts",
    ] {
        connection.execute(
            &format!("DELETE FROM {table} WHERE mount_id = ?1"),
            params![&mount_id.0],
        )?;
    }
    Ok(())
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
const AUTO_SAVE_SELECT_WITH_WHERE: &str = "
    SELECT mount_id, path, remote_id, enabled, origin_json, state_json, last_reason,
           last_push_id, created_at, updated_at
    FROM auto_save_enrollments
    ";
const MOUNT_LIVE_MODE_SELECT_WITH_WHERE: &str = "
    SELECT mount_id, enabled, state_json, last_reason, last_run_at, created_at, updated_at
    FROM mount_live_modes
    ";
const REMOTE_OBSERVATION_SELECT_WITH_WHERE: &str = "
    SELECT mount_id, remote_id, kind_json, title, parent_remote_id, projected_path,
           remote_version_json, observed_at, deleted, raw_metadata_json
    FROM remote_observations
    ";
const FRESHNESS_STATE_SELECT_WITH_WHERE: &str = "
    SELECT mount_id, remote_id, tier_json, last_checked_at, next_check_at, last_opened_at,
           last_local_change_at, remote_hint_pending
    FROM freshness_states
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
type AutoSaveEnrollmentRow = (
    String,
    String,
    Option<String>,
    i64,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
);
type MountLiveModeRow = (
    String,
    i64,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
);
type RemoteObservationRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    String,
    i64,
    String,
);
type FreshnessStateRow = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
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
        repair_missing_state_components(connection)?;
        ensure_state_components_allow_schema_migration(connection, user_version)?;
        migrate_linux_fuse_projection_layout_to_v2(connection, false)?;
        migrate_windows_cloud_files_projection_layout_to_v2(connection, false)?;
        migrate_notion_workspace_roots_projection_layout_to_v1(connection)?;
        return Ok(());
    }

    if user_version >= 13 {
        ensure_state_components_allow_schema_migration(connection, user_version)?;
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

        CREATE TABLE IF NOT EXISTS auto_save_enrollments (
            mount_id TEXT NOT NULL,
            path TEXT NOT NULL,
            remote_id TEXT,
            enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
            origin_json TEXT NOT NULL,
            state_json TEXT NOT NULL,
            last_reason TEXT,
            last_push_id TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (mount_id, path),
            FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS mount_live_modes (
            mount_id TEXT PRIMARY KEY,
            enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
            state_json TEXT NOT NULL,
            last_reason TEXT,
            last_run_at TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS remote_observations (
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
            PRIMARY KEY (mount_id, remote_id),
            FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS freshness_states (
            mount_id TEXT NOT NULL,
            remote_id TEXT NOT NULL,
            tier_json TEXT NOT NULL,
            last_checked_at TEXT,
            next_check_at TEXT,
            last_opened_at TEXT,
            last_local_change_at TEXT,
            remote_hint_pending INTEGER NOT NULL CHECK (remote_hint_pending IN (0, 1)),
            PRIMARY KEY (mount_id, remote_id),
            FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS entity_search_fts USING fts5(
            mount_id UNINDEXED,
            remote_id UNINDEXED,
            title,
            path,
            observed_title,
            observed_path
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

    if user_version < 11 {
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS remote_observations (
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
                PRIMARY KEY (mount_id, remote_id),
                FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS freshness_states (
                mount_id TEXT NOT NULL,
                remote_id TEXT NOT NULL,
                tier_json TEXT NOT NULL,
                last_checked_at TEXT,
                next_check_at TEXT,
                last_opened_at TEXT,
                last_local_change_at TEXT,
                remote_hint_pending INTEGER NOT NULL CHECK (remote_hint_pending IN (0, 1)),
                PRIMARY KEY (mount_id, remote_id),
                FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
            );",
        )?;
    }

    if user_version < 12 {
        create_entity_search_index(connection)?;
        rebuild_entity_search_index(connection)?;
    }

    if user_version < 13 {
        create_state_management_tables(connection)?;
        record_schema_migration(connection, user_version, SCHEMA_VERSION)?;
    }

    if user_version < 14 {
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS auto_save_enrollments (
                mount_id TEXT NOT NULL,
                path TEXT NOT NULL,
                remote_id TEXT,
                enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
                origin_json TEXT NOT NULL,
                state_json TEXT NOT NULL,
                last_reason TEXT,
                last_push_id TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (mount_id, path),
                FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
            );",
        )?;
        if user_version >= 13 {
            record_schema_migration(connection, user_version, SCHEMA_VERSION)?;
        }
    }

    if user_version < 15 {
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS mount_live_modes (
                mount_id TEXT PRIMARY KEY,
                enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
                state_json TEXT NOT NULL,
                last_reason TEXT,
                last_run_at TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
            );",
        )?;
        if user_version >= 13 {
            record_schema_migration(connection, user_version, SCHEMA_VERSION)?;
        }
    }

    if user_version < SCHEMA_VERSION {
        seed_default_notion_profile(connection)?;
        migrate_linux_fuse_projection_layout_to_v2(connection, user_version < 13)?;
        migrate_windows_cloud_files_projection_layout_to_v2(connection, user_version < 13)?;
        migrate_notion_workspace_roots_projection_layout_to_v1(connection)?;
        seed_current_state_components(connection)?;
        connection.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
    }

    Ok(())
}

fn ensure_state_components_allow_schema_migration(
    connection: &Connection,
    user_version: i64,
) -> StoreResult<()> {
    let blocking_issues = inspect_state_component_issues(connection)?
        .into_iter()
        .filter(|issue| !state_component_issue_allows_schema_migration(issue, user_version))
        .collect::<Vec<_>>();

    if blocking_issues.is_empty() {
        Ok(())
    } else {
        Err(StoreError::StateCompatibility(format!(
            "state components are not safe to migrate: {blocking_issues:?}",
        )))
    }
}

fn state_component_issue_allows_schema_migration(
    issue: &StateCompatibilityIssue,
    user_version: i64,
) -> bool {
    matches!(
        issue,
        StateCompatibilityIssue::OlderComponent {
            component_id,
            found,
            current: SCHEMA_VERSION,
        } if component_id == "core:schema" && *found == user_version && user_version < SCHEMA_VERSION
    ) || matches!(
        issue,
        StateCompatibilityIssue::OlderComponent {
            component_id,
            found: 1,
            current: LINUX_FUSE_PROJECTION_LAYOUT_VERSION,
        } if component_id == "projection:linux_fuse"
    ) || matches!(
        issue,
        StateCompatibilityIssue::OlderComponent {
            component_id,
            found: 1,
            current: WINDOWS_CLOUD_FILES_PROJECTION_LAYOUT_VERSION,
        } if component_id == "projection:windows_cloud_files"
    ) || matches!(
        issue,
        StateCompatibilityIssue::OlderComponent {
            component_id,
            found: 0,
            current: NOTION_WORKSPACE_ROOTS_PROJECTION_LAYOUT_VERSION,
        } if component_id == NOTION_WORKSPACE_ROOTS_COMPONENT_ID
    ) || matches!(
        issue,
        StateCompatibilityIssue::MissingComponent { component_id }
            if component_id == "projection:windows_cloud_files"
    ) || matches!(
        issue,
        StateCompatibilityIssue::MissingComponent { component_id }
            if component_id == NOTION_WORKSPACE_ROOTS_COMPONENT_ID
    ) || matches!(
        issue,
        StateCompatibilityIssue::MissingComponent { component_id }
            if user_version < 14 && component_id == "durable:auto_save"
    ) || matches!(
        issue,
        StateCompatibilityIssue::MissingComponent { component_id }
            if user_version < 15 && component_id == "durable:live_mode"
    )
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

fn auto_save_enrollment_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AutoSaveEnrollmentRow> {
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
    ))
}

fn auto_save_enrollment_from_row(
    row: AutoSaveEnrollmentRow,
) -> StoreResult<AutoSaveEnrollmentRecord> {
    Ok(AutoSaveEnrollmentRecord {
        mount_id: MountId(row.0),
        path: PathBuf::from(row.1),
        remote_id: row.2.map(RemoteId),
        enabled: row.3 != 0,
        origin: from_json(&row.4)?,
        state: from_json(&row.5)?,
        last_reason: row.6,
        last_push_id: row.7,
        created_at: row.8,
        updated_at: row.9,
    })
}

fn mount_live_mode_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MountLiveModeRow> {
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

fn mount_live_mode_from_row(row: MountLiveModeRow) -> StoreResult<MountLiveModeRecord> {
    Ok(MountLiveModeRecord {
        mount_id: MountId(row.0),
        enabled: row.1 != 0,
        state: from_json::<MountLiveModeState>(&row.2)?,
        last_reason: row.3,
        last_run_at: row.4,
        created_at: row.5,
        updated_at: row.6,
    })
}

fn remote_observation_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RemoteObservationRow> {
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
    ))
}

fn remote_observation_from_row(row: RemoteObservationRow) -> StoreResult<RemoteObservationRecord> {
    Ok(RemoteObservationRecord {
        mount_id: MountId(row.0),
        remote_id: RemoteId(row.1),
        kind: from_json::<EntityKind>(&row.2)?,
        title: row.3,
        parent_remote_id: row.4.map(RemoteId),
        projected_path: PathBuf::from(row.5),
        remote_version: from_json(&row.6)?,
        observed_at: row.7,
        deleted: row.8 != 0,
        raw_metadata_json: row.9,
    })
}

fn freshness_state_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FreshnessStateRow> {
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

fn freshness_state_from_row(row: FreshnessStateRow) -> StoreResult<FreshnessStateRecord> {
    Ok(FreshnessStateRecord {
        mount_id: MountId(row.0),
        remote_id: RemoteId(row.1),
        tier: from_json(&row.2)?,
        last_checked_at: row.3,
        next_check_at: row.4,
        last_opened_at: row.5,
        last_local_change_at: row.6,
        remote_hint_pending: row.7 != 0,
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
    let (plan, operation_index_map) = journal_plan_from_json(&row.3)?;
    Ok(JournalEntry {
        push_id: PushId(row.0),
        mount_id: MountId(row.1),
        remote_ids: from_json::<Vec<RemoteId>>(&row.2)?,
        plan,
        preimages: from_json::<Vec<JournalPreimage>>(&row.4)?,
        apply_effects: journal_apply_effects_from_json(&row.5, &operation_index_map)?,
        status: from_json(&row.6)?,
    })
}

fn journal_plan_from_json(value: &str) -> StoreResult<(PushPlan, Vec<Option<usize>>)> {
    let mut plan = serde_json::from_str::<Value>(value)?;
    let mut operation_index_map = Vec::new();
    if let Some(operations) = plan.get_mut("operations").and_then(Value::as_array_mut) {
        let mut supported = Vec::with_capacity(operations.len());
        operation_index_map = vec![None; operations.len()];
        for (operation_index, operation) in operations.iter().enumerate() {
            match serde_json::from_value::<PushOperation>(operation.clone()) {
                Ok(_) => {
                    operation_index_map[operation_index] = Some(supported.len());
                    supported.push(operation.clone());
                }
                Err(_) if json_type(operation) == Some("update_entity_content") => {}
                Err(error) => return Err(error.into()),
            }
        }
        *operations = supported;
    }

    let mut plan = serde_json::from_value::<PushPlan>(plan)?;
    plan.summary = PlanSummary::from_operations(&plan.operations);
    Ok((plan, operation_index_map))
}

fn journal_apply_effects_from_json(
    value: &str,
    operation_index_map: &[Option<usize>],
) -> StoreResult<Vec<JournalApplyEffect>> {
    let effects = serde_json::from_str::<Vec<Value>>(value)?;
    let mut supported = Vec::with_capacity(effects.len());

    for effect in effects {
        match serde_json::from_value::<JournalApplyEffect>(effect.clone()) {
            Ok(mut effect) => {
                if remap_apply_effect_operation_index(&mut effect, operation_index_map) {
                    supported.push(effect);
                }
            }
            Err(_) if json_type(&effect) == Some("updated_entity_content") => {}
            Err(error) => return Err(error.into()),
        }
    }

    Ok(supported)
}

fn remap_apply_effect_operation_index(
    effect: &mut JournalApplyEffect,
    operation_index_map: &[Option<usize>],
) -> bool {
    if operation_index_map.is_empty() {
        return true;
    }

    let operation_index = match effect {
        JournalApplyEffect::UpdatedBlock {
            operation_index, ..
        }
        | JournalApplyEffect::CreatedBlock {
            operation_index, ..
        }
        | JournalApplyEffect::MovedBlock {
            operation_index, ..
        }
        | JournalApplyEffect::ArchivedBlock {
            operation_index, ..
        }
        | JournalApplyEffect::ArchivedEntity {
            operation_index, ..
        }
        | JournalApplyEffect::UpdatedProperties {
            operation_index, ..
        }
        | JournalApplyEffect::CreatedEntity {
            operation_index, ..
        } => operation_index,
    };

    match operation_index_map.get(*operation_index) {
        Some(Some(new_index)) => {
            *operation_index = *new_index;
            true
        }
        Some(None) => false,
        None => true,
    }
}

fn json_type(value: &Value) -> Option<&str> {
    value.get("type").and_then(Value::as_str)
}

fn inspect_state_compatibility(root: PathBuf) -> StoreResult<StateCompatibilityReport> {
    let db_path = root.join(DB_FILE);
    if !db_path.exists() {
        return Ok(StateCompatibilityReport::ready(
            db_path,
            false,
            SCHEMA_VERSION,
        ));
    }

    let connection = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let schema_version = read_user_version(&connection)?;
    let mut issues = Vec::new();

    if schema_version > SCHEMA_VERSION {
        issues.push(StateCompatibilityIssue::NewerSchema {
            found: schema_version,
            supported: SCHEMA_VERSION,
        });
    } else if schema_version < SCHEMA_VERSION {
        issues.push(StateCompatibilityIssue::OlderSchema {
            found: schema_version,
            current: SCHEMA_VERSION,
        });
    } else {
        issues.extend(inspect_state_component_issues(&connection)?);
    }

    Ok(StateCompatibilityReport::from_issues(
        db_path,
        true,
        Some(schema_version),
        SCHEMA_VERSION,
        issues,
    ))
}

fn ensure_current_state_is_readable(connection: &Connection) -> StoreResult<()> {
    let report = inspect_open_connection_compatibility(connection)?;
    match report.status {
        StateCompatibilityStatus::Ready => Ok(()),
        StateCompatibilityStatus::Migratable
        | StateCompatibilityStatus::NeedsUpdate
        | StateCompatibilityStatus::Incompatible => Err(StoreError::StateCompatibility(format!(
            "state is not readable by this binary: {:?}",
            report.issues
        ))),
    }
}

fn inspect_open_connection_compatibility(
    connection: &Connection,
) -> StoreResult<StateCompatibilityReport> {
    let schema_version = read_user_version(connection)?;
    let mut issues = Vec::new();

    if schema_version > SCHEMA_VERSION {
        issues.push(StateCompatibilityIssue::NewerSchema {
            found: schema_version,
            supported: SCHEMA_VERSION,
        });
    } else if schema_version < SCHEMA_VERSION {
        issues.push(StateCompatibilityIssue::OlderSchema {
            found: schema_version,
            current: SCHEMA_VERSION,
        });
    } else {
        issues.extend(inspect_state_component_issues(connection)?);
    }

    Ok(StateCompatibilityReport::from_issues(
        PathBuf::from(DB_FILE),
        true,
        Some(schema_version),
        SCHEMA_VERSION,
        issues,
    ))
}

fn inspect_state_component_issues(
    connection: &Connection,
) -> StoreResult<Vec<StateCompatibilityIssue>> {
    if !table_exists(connection, "state_components")? {
        return Ok(CURRENT_COMPONENT_DEFINITIONS
            .iter()
            .map(|definition| StateCompatibilityIssue::MissingComponent {
                component_id: definition.component_id.to_string(),
            })
            .collect());
    }

    let mut components = list_state_components(connection)?;
    let mut issues = Vec::new();

    for definition in CURRENT_COMPONENT_DEFINITIONS {
        match components
            .iter()
            .position(|component| component.component_id == definition.component_id)
        {
            Some(index) => {
                let component = components.remove(index);
                if component.version > definition.current_version {
                    issues.push(StateCompatibilityIssue::NewerComponent {
                        component_id: component.component_id,
                        found: component.version,
                        supported: definition.current_version,
                    });
                } else if component.min_reader_version > definition.current_version {
                    issues.push(StateCompatibilityIssue::ComponentRequiresNewerReader {
                        component_id: component.component_id,
                        min_reader_version: component.min_reader_version,
                        supported: definition.current_version,
                    });
                } else if component.version < definition.current_version {
                    issues.push(StateCompatibilityIssue::OlderComponent {
                        component_id: component.component_id,
                        found: component.version,
                        current: definition.current_version,
                    });
                }
            }
            None => issues.push(StateCompatibilityIssue::MissingComponent {
                component_id: definition.component_id.to_string(),
            }),
        }
    }

    for component in components {
        if component.required {
            issues.push(StateCompatibilityIssue::UnknownRequiredComponent {
                component_id: component.component_id,
                version: component.version,
            });
        }
    }

    Ok(issues)
}

fn list_state_components(connection: &Connection) -> StoreResult<Vec<StateComponentRecord>> {
    let mut statement = connection.prepare(
        "SELECT component_id, component_kind, version, min_reader_version, required, rebuildable, data_json, updated_at
         FROM state_components
         ORDER BY component_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(StateComponentRecord {
            component_id: row.get(0)?,
            component_kind: row.get(1)?,
            version: row.get(2)?,
            min_reader_version: row.get(3)?,
            required: row.get::<_, i64>(4)? != 0,
            rebuildable: row.get::<_, i64>(5)? != 0,
            data_json: row.get(6)?,
            updated_at: row.get(7)?,
        })
    })?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn create_state_management_tables(connection: &Connection) -> StoreResult<()> {
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS state_components (
            component_id TEXT PRIMARY KEY,
            component_kind TEXT NOT NULL,
            version INTEGER NOT NULL,
            min_reader_version INTEGER NOT NULL DEFAULT 1,
            required INTEGER NOT NULL DEFAULT 1 CHECK (required IN (0, 1)),
            rebuildable INTEGER NOT NULL DEFAULT 0 CHECK (rebuildable IN (0, 1)),
            data_json TEXT NOT NULL DEFAULT '{}',
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS state_migrations (
            migration_id TEXT PRIMARY KEY,
            from_schema_version INTEGER NOT NULL,
            to_schema_version INTEGER NOT NULL,
            app_version TEXT NOT NULL,
            app_build_id TEXT,
            daemon_build_id TEXT,
            started_at TEXT NOT NULL,
            finished_at TEXT,
            status TEXT NOT NULL,
            error_json TEXT
        );

        CREATE TABLE IF NOT EXISTS connector_state (
            connector TEXT NOT NULL,
            scope_kind TEXT NOT NULL,
            scope_id TEXT NOT NULL,
            state_version INTEGER NOT NULL,
            min_reader_version INTEGER NOT NULL DEFAULT 1,
            state_json TEXT NOT NULL DEFAULT '{}',
            updated_at TEXT NOT NULL,
            PRIMARY KEY (connector, scope_kind, scope_id)
        );

        CREATE TABLE IF NOT EXISTS projection_state (
            mount_id TEXT NOT NULL,
            projection TEXT NOT NULL,
            layout_version INTEGER NOT NULL,
            min_reader_version INTEGER NOT NULL DEFAULT 1,
            os_domain_id TEXT,
            root_item_id TEXT,
            repair_generation INTEGER NOT NULL DEFAULT 0,
            state_json TEXT NOT NULL DEFAULT '{}',
            updated_at TEXT NOT NULL,
            PRIMARY KEY (mount_id, projection),
            FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
        );
        ",
    )?;
    Ok(())
}

fn migrate_linux_fuse_projection_layout_to_v2(
    connection: &Connection,
    pre_state_components_schema: bool,
) -> StoreResult<()> {
    migrate_virtual_projection_layout_to_v2(
        connection,
        pre_state_components_schema,
        "projection:linux_fuse",
        ProjectionMode::LinuxFuse,
        LINUX_FUSE_PROJECTION_LAYOUT_VERSION,
        MissingProjectionComponent::Error,
    )
}

fn migrate_windows_cloud_files_projection_layout_to_v2(
    connection: &Connection,
    pre_state_components_schema: bool,
) -> StoreResult<()> {
    migrate_virtual_projection_layout_to_v2(
        connection,
        pre_state_components_schema,
        "projection:windows_cloud_files",
        ProjectionMode::WindowsCloudFiles,
        WINDOWS_CLOUD_FILES_PROJECTION_LAYOUT_VERSION,
        MissingProjectionComponent::TreatAsV1,
    )
}

fn migrate_notion_workspace_roots_projection_layout_to_v1(
    connection: &Connection,
) -> StoreResult<()> {
    create_state_management_tables(connection)?;
    let component = connection
        .query_row(
            "SELECT version, min_reader_version
             FROM state_components
             WHERE component_id = ?1",
            params![NOTION_WORKSPACE_ROOTS_COMPONENT_ID],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    if let Some((component_version, min_reader_version)) = component {
        if component_version > NOTION_WORKSPACE_ROOTS_PROJECTION_LAYOUT_VERSION {
            return Err(StoreError::StateCompatibility(format!(
                "state component {NOTION_WORKSPACE_ROOTS_COMPONENT_ID} version {component_version} is newer than supported version {NOTION_WORKSPACE_ROOTS_PROJECTION_LAYOUT_VERSION}",
            )));
        }
        if min_reader_version > NOTION_WORKSPACE_ROOTS_PROJECTION_LAYOUT_VERSION {
            return Err(StoreError::StateCompatibility(format!(
                "state component {NOTION_WORKSPACE_ROOTS_COMPONENT_ID} requires reader version {min_reader_version}, but supported version is {NOTION_WORKSPACE_ROOTS_PROJECTION_LAYOUT_VERSION}",
            )));
        }
        if component_version >= NOTION_WORKSPACE_ROOTS_PROJECTION_LAYOUT_VERSION {
            return Ok(());
        }
    }

    let from_version = component.map(|(version, _)| version).unwrap_or(0);
    let transaction = connection.unchecked_transaction()?;
    let workspace_mounts = {
        let mut statement = transaction.prepare(
            "SELECT mount_id
             FROM mounts
             WHERE connector = 'notion'
               AND remote_root_id IS NULL
             ORDER BY mount_id",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut rewritten_paths = 0;
    rewritten_paths += rewrite_notion_workspace_root_paths(
        &transaction,
        "entities",
        "path",
        "AND remote_id NOT IN ('notion-root:private', 'notion-root:workspace')",
        false,
    )?;
    rewritten_paths += rewrite_notion_workspace_root_paths(
        &transaction,
        "remote_observations",
        "projected_path",
        "",
        false,
    )?;
    rewritten_paths +=
        rewrite_notion_workspace_root_paths(&transaction, "hydration_jobs", "path", "", false)?;
    rewritten_paths += rewrite_notion_workspace_root_paths(
        &transaction,
        "virtual_mutations",
        "projected_path",
        "",
        false,
    )?;
    rewritten_paths += rewrite_notion_workspace_root_paths(
        &transaction,
        "virtual_mutations",
        "original_path",
        "",
        false,
    )?;
    rewritten_paths += rewrite_notion_workspace_root_paths(
        &transaction,
        "virtual_mutations",
        "content_path",
        "",
        true,
    )?;
    rewritten_paths += rewrite_notion_workspace_root_paths(
        &transaction,
        "auto_save_enrollments",
        "path",
        "",
        false,
    )?;

    for mount_id in &workspace_mounts {
        upsert_notion_workspace_synthetic_root(
            &transaction,
            mount_id,
            NOTION_PRIVATE_ROOT_ID,
            NOTION_PRIVATE_ROOT_DIR,
        )?;
        upsert_notion_workspace_synthetic_root(
            &transaction,
            mount_id,
            NOTION_WORKSPACE_ROOT_ID,
            NOTION_WORKSPACE_ROOT_DIR,
        )?;
    }

    if !workspace_mounts.is_empty() || rewritten_paths > 0 {
        rebuild_entity_search_index(&transaction)?;
    }
    upsert_current_state_component_version(
        &transaction,
        NOTION_WORKSPACE_ROOTS_COMPONENT_ID,
        NOTION_WORKSPACE_ROOTS_PROJECTION_LAYOUT_VERSION,
    )?;
    if from_version > 0 || !workspace_mounts.is_empty() || rewritten_paths > 0 {
        record_component_migration(
            &transaction,
            NOTION_WORKSPACE_ROOTS_COMPONENT_ID,
            from_version,
            NOTION_WORKSPACE_ROOTS_PROJECTION_LAYOUT_VERSION,
        )?;
    }
    transaction.commit()?;
    Ok(())
}

fn upsert_notion_workspace_synthetic_root(
    connection: &Connection,
    mount_id: &str,
    remote_id: &str,
    title_and_path: &str,
) -> StoreResult<()> {
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
         VALUES (?1, ?2, ?3, ?4, ?4, ?5, NULL, NULL)
         ON CONFLICT(mount_id, remote_id) DO UPDATE SET
            kind_json = excluded.kind_json,
            title = excluded.title,
            path = excluded.path,
            hydration_json = excluded.hydration_json,
            content_hash = NULL,
            remote_edited_at = NULL",
        params![
            mount_id,
            remote_id,
            to_json(&EntityKind::Directory)?,
            title_and_path,
            to_json(&HydrationState::Virtual)?,
        ],
    )?;
    Ok(())
}

fn rewrite_notion_workspace_root_paths(
    connection: &Connection,
    table: &str,
    column: &str,
    extra_where: &str,
    skip_absolute: bool,
) -> StoreResult<usize> {
    let select_sql = format!(
        "SELECT rowid, {column}
         FROM {table}
         WHERE mount_id IN (
             SELECT mount_id
             FROM mounts
             WHERE connector = 'notion'
               AND remote_root_id IS NULL
         )
           AND {column} IS NOT NULL
           {extra_where}
         ORDER BY rowid"
    );
    let rows = {
        let mut statement = connection.prepare(&select_sql)?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let update_sql = format!("UPDATE {table} SET {column} = ?1 WHERE rowid = ?2");
    let mut rewritten = 0;
    for (rowid, path) in rows {
        let Some(rewritten_path) = notion_workspace_root_repaired_path(&path, skip_absolute) else {
            continue;
        };
        connection.execute(&update_sql, params![rewritten_path, rowid])?;
        rewritten += 1;
    }
    Ok(rewritten)
}

fn notion_workspace_root_repaired_path(path: &str, skip_absolute: bool) -> Option<String> {
    if path.is_empty()
        || path.starts_with("Private/")
        || path.starts_with("Private\\")
        || path.starts_with("Workspace/")
        || path.starts_with("Workspace\\")
        || (skip_absolute && is_probably_absolute_path(path))
    {
        None
    } else {
        Some(format!("{NOTION_WORKSPACE_ROOT_DIR}/{path}"))
    }
}

fn is_probably_absolute_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with('/')
        || path.starts_with('\\')
        || (bytes.len() >= 3
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\')
            && bytes[0].is_ascii_alphabetic())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MissingProjectionComponent {
    Error,
    TreatAsV1,
}

fn migrate_virtual_projection_layout_to_v2(
    connection: &Connection,
    pre_state_components_schema: bool,
    component_id: &str,
    projection: ProjectionMode,
    layout_version: i64,
    missing_component: MissingProjectionComponent,
) -> StoreResult<()> {
    create_state_management_tables(connection)?;
    let component = connection
        .query_row(
            "SELECT version, min_reader_version
             FROM state_components
             WHERE component_id = ?1",
            params![component_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    if let Some((component_version, min_reader_version)) = component {
        if component_version > layout_version {
            return Err(StoreError::StateCompatibility(format!(
                "state component {component_id} version {component_version} is newer than supported version {layout_version}",
            )));
        }
        if min_reader_version > layout_version {
            return Err(StoreError::StateCompatibility(format!(
                "state component {component_id} requires reader version {min_reader_version}, but supported version is {layout_version}",
            )));
        }
        if component_version >= layout_version {
            return Ok(());
        }
    } else if !pre_state_components_schema && missing_component == MissingProjectionComponent::Error
    {
        return Err(StoreError::StateCompatibility(format!(
            "missing required state component {component_id}"
        )));
    }

    let transaction = connection.unchecked_transaction()?;
    let projection_json = to_json(&projection)?;
    let mounts = {
        let mut statement = transaction.prepare(
            "SELECT mount_id, connector, root
         FROM mounts
         WHERE projection_json = ?1
         ORDER BY mount_id",
        )?;
        let rows = statement.query_map(params![projection_json], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    for (mount_id, connector, root) in mounts {
        let connector_root = connector_root_directory_name(&connector);
        let mount_id_root = connector_root_directory_name(&mount_id);
        let root = PathBuf::from(root);
        let root_file_name = root.file_name().and_then(|name| name.to_str());
        let already_mount_point_root = root_file_name == Some(connector_root.as_str())
            || (projection == ProjectionMode::WindowsCloudFiles
                && root_file_name.is_some_and(|name| {
                    name == mount_id.as_str() || name == mount_id_root.as_str()
                }));
        let migrated_root = if already_mount_point_root {
            root
        } else {
            root.join(connector_root)
        };
        transaction.execute(
            "UPDATE mounts
             SET root = ?1
             WHERE mount_id = ?2",
            params![path_to_text(&migrated_root), mount_id],
        )?;
    }

    let definition = CURRENT_COMPONENT_DEFINITIONS
        .iter()
        .find(|definition| definition.component_id == component_id)
        .expect("known state component definition");
    let updated_at = unix_timestamp_string();
    transaction.execute(
        "INSERT INTO state_components (
            component_id,
            component_kind,
            version,
            min_reader_version,
            required,
            rebuildable,
            data_json,
            updated_at
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(component_id) DO UPDATE SET
            component_kind = excluded.component_kind,
            version = excluded.version,
            min_reader_version = excluded.min_reader_version,
            required = excluded.required,
            rebuildable = excluded.rebuildable,
            data_json = excluded.data_json,
            updated_at = excluded.updated_at",
        params![
            definition.component_id,
            definition.component_kind,
            layout_version,
            definition.min_reader_version,
            bool_to_int(definition.required),
            bool_to_int(definition.rebuildable),
            definition.data_json,
            &updated_at,
        ],
    )?;
    transaction.commit()?;
    Ok(())
}

fn connector_root_directory_name(connector: &str) -> String {
    let normalized = connector
        .chars()
        .filter_map(|character| {
            if character.is_ascii_alphanumeric() {
                Some(character.to_ascii_lowercase())
            } else if matches!(character, '-' | '_') {
                Some('-')
            } else {
                None
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if normalized.is_empty() {
        "source".to_string()
    } else {
        normalized
    }
}

fn seed_current_state_components(connection: &Connection) -> StoreResult<()> {
    create_state_management_tables(connection)?;
    for definition in CURRENT_COMPONENT_DEFINITIONS {
        upsert_current_state_component_version(
            connection,
            definition.component_id,
            definition.current_version,
        )?;
    }
    Ok(())
}

fn upsert_current_state_component_version(
    connection: &Connection,
    component_id: &str,
    version: i64,
) -> StoreResult<()> {
    create_state_management_tables(connection)?;
    let definition = CURRENT_COMPONENT_DEFINITIONS
        .iter()
        .find(|definition| definition.component_id == component_id)
        .expect("known state component definition");
    let updated_at = unix_timestamp_string();
    connection.execute(
        "INSERT INTO state_components (
            component_id,
            component_kind,
            version,
            min_reader_version,
            required,
            rebuildable,
            data_json,
            updated_at
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(component_id) DO UPDATE SET
            component_kind = excluded.component_kind,
            version = excluded.version,
            min_reader_version = excluded.min_reader_version,
            required = excluded.required,
            rebuildable = excluded.rebuildable,
            data_json = excluded.data_json,
            updated_at = excluded.updated_at",
        params![
            definition.component_id,
            definition.component_kind,
            version,
            definition.min_reader_version,
            bool_to_int(definition.required),
            bool_to_int(definition.rebuildable),
            definition.data_json,
            &updated_at,
        ],
    )?;
    Ok(())
}

fn seed_missing_state_components(connection: &Connection) -> StoreResult<()> {
    create_state_management_tables(connection)?;
    let updated_at = unix_timestamp_string();
    for definition in CURRENT_COMPONENT_DEFINITIONS {
        if !repairable_missing_state_component(definition.component_id) {
            continue;
        }
        connection.execute(
            "INSERT OR IGNORE INTO state_components (
                component_id,
                component_kind,
                version,
                min_reader_version,
                required,
                rebuildable,
                data_json,
                updated_at
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                definition.component_id,
                definition.component_kind,
                definition.current_version,
                definition.min_reader_version,
                bool_to_int(definition.required),
                bool_to_int(definition.rebuildable),
                definition.data_json,
                &updated_at,
            ],
        )?;
    }
    Ok(())
}

fn repairable_missing_state_component(component_id: &str) -> bool {
    !matches!(
        component_id,
        "projection:linux_fuse"
            | "projection:windows_cloud_files"
            | NOTION_WORKSPACE_ROOTS_COMPONENT_ID
    )
}

fn repair_missing_state_components(connection: &Connection) -> StoreResult<()> {
    if inspect_state_component_issues(connection)?
        .iter()
        .any(|issue| {
            matches!(
                issue,
                StateCompatibilityIssue::MissingComponent { component_id }
                    if repairable_missing_state_component(component_id)
            )
        })
    {
        seed_missing_state_components(connection)?;
    }
    Ok(())
}

fn record_schema_migration(connection: &Connection, from: i64, to: i64) -> StoreResult<()> {
    create_state_management_tables(connection)?;
    let now = unix_timestamp_string();
    let migration_id = format!("schema-{from}-to-{to}");
    connection.execute(
        "INSERT INTO state_migrations (
            migration_id,
            from_schema_version,
            to_schema_version,
            app_version,
            app_build_id,
            daemon_build_id,
            started_at,
            finished_at,
            status,
            error_json
         )
         VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5, ?5, 'finished', NULL)
         ON CONFLICT(migration_id) DO NOTHING",
        params![migration_id, from, to, env!("CARGO_PKG_VERSION"), now],
    )?;
    Ok(())
}

fn record_component_migration(
    connection: &Connection,
    component_id: &str,
    from: i64,
    to: i64,
) -> StoreResult<()> {
    create_state_management_tables(connection)?;
    let now = unix_timestamp_string();
    let migration_id = component_migration_id(component_id, from, to);
    connection.execute(
        "INSERT INTO state_migrations (
            migration_id,
            from_schema_version,
            to_schema_version,
            app_version,
            app_build_id,
            daemon_build_id,
            started_at,
            finished_at,
            status,
            error_json
         )
         VALUES (?1, ?2, ?2, ?3, NULL, NULL, ?4, ?4, 'finished', NULL)
         ON CONFLICT(migration_id) DO NOTHING",
        params![migration_id, SCHEMA_VERSION, env!("CARGO_PKG_VERSION"), now],
    )?;
    Ok(())
}

fn component_migration_id(component_id: &str, from: i64, to: i64) -> String {
    let component = component_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    format!("component-{component}-{from}-to-{to}")
}

fn read_user_version(connection: &Connection) -> StoreResult<i64> {
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(Into::into)
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
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            "notion-token-default",
            "notion",
            "Notion token auth",
            "token",
            "[]",
            DEFAULT_NOTION_CAPABILITIES_JSON,
            "[\"read\",\"write\"]",
            "notion.v1",
            "active",
            "0",
            "0",
        ],
    )?;
    Ok(())
}

fn create_entity_search_index(connection: &Connection) -> StoreResult<()> {
    connection.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS entity_search_fts USING fts5(
            mount_id UNINDEXED,
            remote_id UNINDEXED,
            title,
            path,
            observed_title,
            observed_path
        );",
    )?;
    Ok(())
}

fn rebuild_entity_search_index(connection: &Connection) -> StoreResult<()> {
    create_entity_search_index(connection)?;
    connection.execute("DELETE FROM entity_search_fts", [])?;

    let entity_ids = {
        let mut statement = connection.prepare("SELECT mount_id, remote_id FROM entities")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    for (mount_id, remote_id) in entity_ids {
        upsert_entity_search_index(connection, &MountId(mount_id), &RemoteId(remote_id))?;
    }

    Ok(())
}

fn upsert_entity_search_index(
    connection: &Connection,
    mount_id: &MountId,
    remote_id: &RemoteId,
) -> StoreResult<()> {
    delete_entity_search_index(connection, mount_id, remote_id)?;

    let indexed: Option<(String, String, Option<String>, Option<String>)> = connection
        .query_row(
            "SELECT e.title, e.path, o.title, o.projected_path
             FROM entities e
             LEFT JOIN remote_observations o
               ON o.mount_id = e.mount_id AND o.remote_id = e.remote_id
             WHERE e.mount_id = ?1 AND e.remote_id = ?2",
            params![mount_id.0, remote_id.0],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;

    if let Some((title, path, observed_title, observed_path)) = indexed {
        connection.execute(
            "INSERT INTO entity_search_fts (
                mount_id,
                remote_id,
                title,
                path,
                observed_title,
                observed_path
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                mount_id.0,
                remote_id.0,
                title,
                path,
                observed_title,
                observed_path,
            ],
        )?;
    }

    Ok(())
}

fn delete_entity_search_index(
    connection: &Connection,
    mount_id: &MountId,
    remote_id: &RemoteId,
) -> StoreResult<()> {
    create_entity_search_index(connection)?;
    connection.execute(
        "DELETE FROM entity_search_fts WHERE mount_id = ?1 AND remote_id = ?2",
        params![mount_id.0, remote_id.0],
    )?;
    Ok(())
}

fn entity_search_match_query(query: &str) -> Option<String> {
    let normalized = query
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>();
    let tokens = normalized
        .split_whitespace()
        .filter(|token| search_token_allowed(token))
        .take(16)
        .map(|token| format!("{token}*"))
        .collect::<Vec<_>>();

    if tokens.is_empty() {
        return None;
    }

    Some(tokens.join(" AND "))
}

fn search_token_allowed(token: &str) -> bool {
    token.len() >= 2 || token.chars().any(|character| character.is_ascii_digit())
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

fn table_exists(connection: &Connection, table: &str) -> StoreResult<bool> {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type IN ('table', 'view') AND name = ?1",
            params![table],
            |_| Ok(()),
        )
        .optional()
        .map(|result| result.is_some())
        .map_err(Into::into)
}

fn path_to_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn logical_path_to_text(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
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

fn unix_timestamp_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}
