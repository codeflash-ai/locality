//! SQLite state-store implementation.
//!
//! This is the first durable adapter for the repository traits. It keeps the
//! schema intentionally compact: path-addressable facts live in relational
//! columns, while shadow block arrays and journal plans are stored as JSON blobs
//! until query needs justify normalization.

use std::collections::BTreeSet;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use locality_core::LocalityResult;
use locality_core::hydration::HydrationReason;
use locality_core::journal::{
    JournalApplyEffect, JournalEntry, JournalMetadata, JournalPreimage, JournalStatus,
    JournalStore, PushId,
};
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use locality_core::planner::{PlanSummary, PushOperation, PushPlan};
use locality_core::readable_diff::ReadableDiffOutput;
use locality_core::search::{RAW_SEARCH_METADATA_KEY, SearchMetadata};
use locality_core::shadow::ShadowDocument;
use locality_core::workspace_layout::{MountTarget, PortableMountId};
use locality_protocol::freshness_delivery::{GenerationDelta, MAX_DELIVERY_ID_BYTES};
use locality_protocol::generation_baseline::{
    GenerationBaselineRefreshModeV1, GenerationBaselineSourceV1,
};
use locality_protocol::workspace_layout::{LayoutDigest, WorkspaceProfileId};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::compatibility::{
    StateCompatibilityIssue, StateCompatibilityReport, StateCompatibilityStatus,
    StateComponentDefinition, StateComponentRecord,
};
use crate::discovery::{
    DISCOVERY_TRANSACTION_MIN_READER_VERSION, DISCOVERY_TRANSACTION_STATE_VERSION, DiscoveryCommit,
    DiscoveryPreflight, DiscoveryRepository, DiscoveryReservation, DiscoveryTransactionId,
    DiscoveryTransactionRecord, DiscoveryTransactionStatus, PreparedDiscoveryTransaction,
    TransactionalDiscoveryCommit, canonical_envelope_json, canonical_json, canonicalize_json_value,
    decode_envelope, prepared_matches_record, record_from_prepared, require_transaction_status,
    reservation_changed, transaction_missing, validate_envelope_version,
};
use crate::error::{StoreError, StoreResult};
use crate::generation_delivery::{
    GENERATION_DELIVERY_COMPONENT_VERSION, GenerationApplyJournalRecord, GenerationApplyOutcome,
    GenerationApplyStatus, GenerationBaselineSeedRecord, GenerationBaselineSeedRecordV2,
    GenerationDeliveryRepository, GenerationInodeEvidenceConflictUpdate,
    GenerationInodeEvidenceRecord, GenerationInodeEvidenceResolution, GenerationPathRecord,
    GenerationPathState, GenerationRetainedInodeRecord, GenerationTransportSelectionBinding,
    NegotiatedGenerationApplyJournalRecord, ObservedGenerationRecord, ObservedGenerationRecordV2,
    PreparedGenerationApply, PreparedGenerationApplyV2, PreparedGenerationApplyV3,
};
use crate::hosted_workspace::{
    CanonicalApiOrigin, HOSTED_WORKSPACE_ATTACHMENT_COMPONENT_VERSION, HostedWorkspaceAttachment,
    HostedWorkspaceCredentialRef, HostedWorkspaceIdentity, HostedWorkspaceMountMapping,
    HostedWorkspaceTransitionKind, PendingHostedWorkspaceCleanup, PendingHostedWorkspaceTransition,
    PreparedHostedWorkspaceTransition, committed_attachment, prepare_pending_transition,
    relocation_cleanup,
};
use crate::records::{
    AutoSaveEnrollmentRecord, ConnectionId, ConnectionRecord, ConnectorProfileId,
    ConnectorProfileRecord, ConnectorStateRecord, EntityRecord, FreshnessStateRecord,
    HydrationJobRecord, MetadataDiscoveryJobRecord, MetadataDiscoveryPriority, MountConfig,
    MountLiveModeRecord, MountLiveModeState, ProjectionMode, RemoteObservationRecord,
    ShadowBlockRecord, ShadowSnapshotRecord, VirtualMutationKind, VirtualMutationRecord,
};
use crate::repository::{
    AutoSaveRepository, ConnectionRepository, ConnectorProfileRepository, ConnectorStateRepository,
    EntityRepository, EntitySearchCandidate, EntitySearchDocument, EntitySearchRepository,
    FreshnessStateRepository, HostedWorkspaceRepository, HydrationJobRepository, JournalRepository,
    MetadataDiscoveryJobRepository, MountLiveModeRepository, MountRepository,
    RemoteObservationRepository, ShadowRepository, VirtualMoveRepository, VirtualMoveTransition,
    VirtualMutationRepository, WorkspaceBindingRepository, WorkspaceRemountRecoveryOutcome,
    validate_virtual_move_transition, virtual_move_content_changed, virtual_move_missing,
};
use crate::workspace_binding::{
    LegacyWorkspaceMount, WorkspaceBinding, WorkspaceBindingRecord, WorkspaceHostBinding,
    WorkspaceHostBindingResolver, WorkspaceId, WorkspaceRebindBlocker, host_paths_equivalent,
    legacy_mount_collision_key, legacy_mount_collision_key_for_host,
};
use locality_protocol::freshness_delivery_transport::GenerationTransportCapabilities;

const DB_FILE: &str = "state.sqlite3";
const SCHEMA_VERSION: i64 = 29;
const ENTITY_SEARCH_COMPONENT_VERSION: i64 = 2;
const JOURNALS_COMPONENT_VERSION: i64 = 3;
const VIRTUAL_MUTATIONS_COMPONENT_VERSION: i64 = 4;
const LINUX_FUSE_PROJECTION_LAYOUT_VERSION: i64 = 2;
const WINDOWS_CLOUD_FILES_PROJECTION_LAYOUT_VERSION: i64 = 2;
const RETIRED_NOTION_WORKSPACE_ROOTS_COMPONENT_ID: &str = "projection:notion_workspace_roots";
const RETIRED_NOTION_WORKSPACE_ROOTS_SUPPORTED_VERSION: i64 = 2;
const RETIRED_NOTION_PRIVATE_ROOT_ID: &str = "notion-root:private";
const RETIRED_NOTION_WORKSPACE_ROOT_ID: &str = "notion-root:workspace";
const ENTITY_SEARCH_CANDIDATE_LIMIT: i64 = 256;
const DEFAULT_NOTION_CAPABILITIES_JSON: &str = "{\"supports_block_updates\":true,\"supports_databases\":true,\"supports_oauth\":true,\"supports_remote_observation\":true,\"supports_lazy_child_enumeration\":true,\"supports_media_download\":true,\"supports_undo\":true,\"supports_batch_observation\":false}";
const DEFAULT_JOURNAL_METADATA_JSON: &str =
    "{\"author\":{\"kind\":\"anonymous\",\"display_name\":\"anonymous\"}}";
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
        component_id: "connector:granola",
        component_kind: "connector_state",
        current_version: 1,
        min_reader_version: 1,
        required: true,
        rebuildable: false,
        data_json: "{\"connector_version\":\"granola.v1\"}",
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
        current_version: JOURNALS_COMPONENT_VERSION,
        min_reader_version: JOURNALS_COMPONENT_VERSION,
        required: true,
        rebuildable: false,
        data_json: "{}",
    },
    StateComponentDefinition {
        component_id: "durable:virtual_mutations",
        component_kind: "durable_json",
        current_version: VIRTUAL_MUTATIONS_COMPONENT_VERSION,
        min_reader_version: VIRTUAL_MUTATIONS_COMPONENT_VERSION,
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
        component_id: "durable:workspace_bindings",
        component_kind: "durable_json",
        current_version: 4,
        min_reader_version: 4,
        required: true,
        rebuildable: false,
        data_json: "{\"format\":\"workspace_binding.v2\",\"layout_0_without_binding\":true,\"legacy_v1_readable\":true,\"target_scope\":\"workspace_id\",\"remount_recovery\":\"v1\"}",
    },
    StateComponentDefinition {
        component_id: "durable:hosted_workspaces",
        component_kind: "durable_transaction",
        current_version: HOSTED_WORKSPACE_ATTACHMENT_COMPONENT_VERSION as i64,
        min_reader_version: HOSTED_WORKSPACE_ATTACHMENT_COMPONENT_VERSION as i64,
        required: true,
        rebuildable: false,
        data_json: "{\"identity\":\"canonical_api_origin+profile_id\",\"credential_storage\":\"reference_only\",\"mount_mapping\":\"stable_local_id\",\"publication\":\"whole_workspace\",\"relocation_cleanup\":\"durable_v1\"}",
    },
    StateComponentDefinition {
        component_id: "durable:metadata_discovery",
        component_kind: "durable_queue",
        current_version: 1,
        min_reader_version: 1,
        required: true,
        rebuildable: true,
        data_json: "{}",
    },
    StateComponentDefinition {
        component_id: "durable:discovery_projection",
        component_kind: "durable_transaction",
        current_version: 1,
        min_reader_version: 1,
        required: true,
        rebuildable: false,
        data_json: "{}",
    },
    StateComponentDefinition {
        component_id: "durable:generation_delivery",
        component_kind: "durable_transaction",
        current_version: GENERATION_DELIVERY_COMPONENT_VERSION,
        min_reader_version: GENERATION_DELIVERY_COMPONENT_VERSION,
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
        current_version: ENTITY_SEARCH_COMPONENT_VERSION,
        min_reader_version: ENTITY_SEARCH_COMPONENT_VERSION,
        required: false,
        rebuildable: true,
        data_json: "{\"index\":\"search_documents_fts\",\"legacy_index\":\"entity_search_fts\"}",
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

    /// Read configured mount roots without creating, migrating, or repairing
    /// Locality state. This intentionally supports older schemas whose `mounts`
    /// table already has the stable `mount_id` and `root` columns.
    pub fn inspect_mount_roots_read_only(
        root: impl AsRef<Path>,
    ) -> StoreResult<Vec<LegacyWorkspaceMount>> {
        let db_path = root.as_ref().join(DB_FILE);
        if !db_path.exists() {
            return Ok(Vec::new());
        }
        let connection = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        if !table_exists(&connection, "mounts")? {
            return Err(StoreError::StateCompatibility(
                "state database has no readable mounts table".to_string(),
            ));
        }
        let mut statement =
            connection.prepare("SELECT mount_id, root FROM mounts ORDER BY mount_id")?;
        let rows = statement.query_map([], |row| {
            Ok(LegacyWorkspaceMount::new(
                MountId(row.get::<_, String>(0)?),
                PathBuf::from(row.get::<_, String>(1)?),
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn open(root: PathBuf) -> StoreResult<Self> {
        std::fs::create_dir_all(&root)?;
        let db_path = root.join(DB_FILE);
        let store = Self { root, db_path };
        let mut connection = store.connection()?;
        initialize_schema(&mut connection)?;
        ensure_current_state_is_readable(&connection)?;
        Ok(store)
    }

    pub fn clear_mount_source_state(&mut self, mount_id: &MountId) -> StoreResult<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        clear_mount_source_state(&transaction, mount_id)?;
        transaction.commit()?;
        Ok(())
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

fn generation_path_state_label(state: GenerationPathState) -> &'static str {
    match state {
        GenerationPathState::Clean => "clean",
        GenerationPathState::Dirty => "dirty",
        GenerationPathState::Conflicted => "conflicted",
    }
}

fn parse_generation_path_state(value: &str) -> StoreResult<GenerationPathState> {
    match value {
        "clean" => Ok(GenerationPathState::Clean),
        "dirty" => Ok(GenerationPathState::Dirty),
        "conflicted" => Ok(GenerationPathState::Conflicted),
        _ => Err(StoreError::InvalidState(format!(
            "unknown generation path state `{value}`"
        ))),
    }
}

fn generation_baseline_refresh_mode_label(mode: GenerationBaselineRefreshModeV1) -> &'static str {
    match mode {
        GenerationBaselineRefreshModeV1::GenerationDeltaV1 => "generation_delta_v1",
        GenerationBaselineRefreshModeV1::FullExportOnly => "full_export_only",
    }
}

fn parse_generation_baseline_refresh_mode(
    value: &str,
) -> StoreResult<GenerationBaselineRefreshModeV1> {
    match value {
        "generation_delta_v1" => Ok(GenerationBaselineRefreshModeV1::GenerationDeltaV1),
        "full_export_only" => Ok(GenerationBaselineRefreshModeV1::FullExportOnly),
        _ => Err(StoreError::InvalidState(format!(
            "unknown generation baseline refresh mode `{value}`"
        ))),
    }
}

fn generation_seed_refresh_mode(
    observed: &ObservedGenerationRecord,
    paths: &[GenerationPathRecord],
) -> GenerationBaselineRefreshModeV1 {
    let identifiers_fit = !observed.mount_id.as_str().is_empty()
        && observed.mount_id.as_str().len() <= MAX_DELIVERY_ID_BYTES
        && !observed.source_connection_id.as_str().is_empty()
        && observed.source_connection_id.as_str().len() <= MAX_DELIVERY_ID_BYTES
        && observed.generation_id.as_str().len() <= MAX_DELIVERY_ID_BYTES
        && paths.iter().all(|path| {
            !path.projection_id.as_str().is_empty()
                && path.projection_id.as_str().len() <= MAX_DELIVERY_ID_BYTES
                && path
                    .base_identity
                    .iter()
                    .chain(path.incoming_identity.iter())
                    .all(|identity| identity.validate().is_ok())
        });
    if identifiers_fit {
        GenerationBaselineRefreshModeV1::GenerationDeltaV1
    } else {
        GenerationBaselineRefreshModeV1::FullExportOnly
    }
}

fn observed_generation_from_row(
    row: (
        String,
        String,
        String,
        String,
        i64,
        String,
        Option<String>,
        String,
    ),
) -> StoreResult<ObservedGenerationRecord> {
    Ok(ObservedGenerationRecord {
        mount_id: MountId::new(row.0),
        source_connection_id: locality_core::portable::SourceConnectionId::new(row.1),
        generation_id: locality_core::portable::SourceGenerationId::new(row.2)
            .map_err(|error| StoreError::InvalidState(error.to_string()))?,
        inventory_sha256: row.3,
        workspace_layout_version: u16::try_from(row.4).map_err(|_| {
            StoreError::InvalidState("invalid stored workspace layout version".to_string())
        })?,
        workspace_layout_digest: row.5,
        last_receipt_sha256: row.6,
        updated_at: row.7,
    })
}

fn select_observed_generation(
    connection: &Connection,
    mount_id: &MountId,
) -> StoreResult<Option<ObservedGenerationRecord>> {
    let mut records = list_observed_generations_from_connection(connection, mount_id)?;
    match records.len() {
        0 => Ok(None),
        1 => Ok(records.pop()),
        _ => Err(StoreError::InvalidState(format!(
            "mount `{}` has multiple observed generation sources; use a source-explicit repository method",
            mount_id.0
        ))),
    }
}

fn select_observed_generation_for_source(
    connection: &Connection,
    mount_id: &MountId,
    source_connection_id: &locality_core::portable::SourceConnectionId,
) -> StoreResult<Option<ObservedGenerationRecord>> {
    connection
        .query_row(
            "SELECT mount_id, source_connection_id, generation_id, inventory_sha256,
                    workspace_layout_version, workspace_layout_digest,
                    last_receipt_sha256, updated_at
             FROM observed_generations
             WHERE mount_id = ?1 AND source_connection_id = ?2",
            params![mount_id.0.as_str(), source_connection_id.as_str()],
            |row| {
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
            },
        )
        .optional()?
        .map(observed_generation_from_row)
        .transpose()
}

fn select_observed_generation_for_source_v2(
    connection: &Connection,
    mount_id: &MountId,
    source_connection_id: &locality_core::portable::SourceConnectionId,
) -> StoreResult<Option<ObservedGenerationRecordV2>> {
    connection
        .query_row(
            "SELECT mount_id, source_connection_id, generation_id, inventory_sha256,
                    workspace_layout_version, workspace_layout_digest,
                    last_receipt_sha256, updated_at, refresh_mode
             FROM observed_generations
             WHERE mount_id = ?1 AND source_connection_id = ?2",
            params![mount_id.0.as_str(), source_connection_id.as_str()],
            |row| {
                Ok((
                    (
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ),
                    row.get::<_, String>(8)?,
                ))
            },
        )
        .optional()?
        .map(|(row, refresh_mode)| {
            Ok(ObservedGenerationRecordV2::new(
                observed_generation_from_row(row)?,
                parse_generation_baseline_refresh_mode(&refresh_mode)?,
            ))
        })
        .transpose()
}

fn list_observed_generations_from_connection(
    connection: &Connection,
    mount_id: &MountId,
) -> StoreResult<Vec<ObservedGenerationRecord>> {
    let mut statement = connection.prepare(
        "SELECT mount_id, source_connection_id, generation_id, inventory_sha256,
                workspace_layout_version, workspace_layout_digest,
                last_receipt_sha256, updated_at
         FROM observed_generations
         WHERE mount_id = ?1
         ORDER BY source_connection_id",
    )?;
    let rows = statement.query_map(params![mount_id.0.as_str()], |row| {
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
    })?;
    rows.map(|row| observed_generation_from_row(row?)).collect()
}

fn list_observed_generations_v2_from_connection(
    connection: &Connection,
    mount_id: &MountId,
) -> StoreResult<Vec<ObservedGenerationRecordV2>> {
    let mut statement = connection.prepare(
        "SELECT mount_id, source_connection_id, generation_id, inventory_sha256,
                workspace_layout_version, workspace_layout_digest,
                last_receipt_sha256, updated_at, refresh_mode
         FROM observed_generations
         WHERE mount_id = ?1
         ORDER BY source_connection_id",
    )?;
    let rows = statement.query_map(params![mount_id.0.as_str()], |row| {
        Ok((
            (
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
            ),
            row.get::<_, String>(8)?,
        ))
    })?;
    rows.map(|row| {
        let (observed, refresh_mode) = row?;
        Ok(ObservedGenerationRecordV2::new(
            observed_generation_from_row(observed)?,
            parse_generation_baseline_refresh_mode(&refresh_mode)?,
        ))
    })
    .collect()
}

fn generation_path_from_row(
    row: (
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<i64>,
        String,
        Option<String>,
        String,
    ),
) -> StoreResult<GenerationPathRecord> {
    Ok(GenerationPathRecord {
        mount_id: MountId::new(row.0),
        projection_id: locality_core::portable::ProjectionId::new(row.1),
        logical_path: row.2,
        local_logical_path: row.3,
        base_generation_id: locality_core::portable::SourceGenerationId::new(row.4)
            .map_err(|error| StoreError::InvalidState(error.to_string()))?,
        base_identity: row.5.map(|value| from_json(&value)).transpose()?,
        base_payload_delta_id: row.6,
        base_payload_entry_index: row
            .7
            .map(|value| {
                u64::try_from(value).map_err(|_| {
                    StoreError::InvalidState("negative generation base payload index".to_string())
                })
            })
            .transpose()?,
        conflict_payload_delta_id: row.8,
        conflict_payload_entry_index: row
            .9
            .map(|value| {
                u64::try_from(value).map_err(|_| {
                    StoreError::InvalidState(
                        "negative generation conflict payload index".to_string(),
                    )
                })
            })
            .transpose()?,
        state: parse_generation_path_state(&row.10)?,
        incoming_identity: row.11.map(|value| from_json(&value)).transpose()?,
        updated_at: row.12,
    })
}

fn select_generation_path(
    connection: &Connection,
    mount_id: &MountId,
    source_connection_id: &locality_core::portable::SourceConnectionId,
    projection_id: &locality_core::portable::ProjectionId,
) -> StoreResult<Option<GenerationPathRecord>> {
    connection
        .query_row(
            "SELECT mount_id, projection_id, logical_path, local_logical_path, base_generation_id,
                    base_identity_json, base_payload_delta_id, base_payload_entry_index,
                    conflict_payload_delta_id, conflict_payload_entry_index,
                    state, incoming_identity_json, updated_at
             FROM generation_paths
             WHERE mount_id = ?1 AND source_connection_id = ?2 AND projection_id = ?3",
            params![
                mount_id.0.as_str(),
                source_connection_id.as_str(),
                projection_id.as_str()
            ],
            |row| {
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
                ))
            },
        )
        .optional()?
        .map(generation_path_from_row)
        .transpose()
}

fn list_generation_paths_from_connection(
    connection: &Connection,
    mount_id: &MountId,
) -> StoreResult<Vec<GenerationPathRecord>> {
    let mut observed = list_observed_generations_from_connection(connection, mount_id)?;
    match observed.len() {
        0 => Ok(Vec::new()),
        1 => list_generation_paths_for_source_from_connection(
            connection,
            mount_id,
            &observed
                .pop()
                .expect("one source was checked")
                .source_connection_id,
        ),
        _ => Err(StoreError::InvalidState(format!(
            "mount `{}` has multiple observed generation sources; use a source-explicit repository method",
            mount_id.0
        ))),
    }
}

fn list_generation_paths_for_source_from_connection(
    connection: &Connection,
    mount_id: &MountId,
    source_connection_id: &locality_core::portable::SourceConnectionId,
) -> StoreResult<Vec<GenerationPathRecord>> {
    let mut statement = connection.prepare(
        "SELECT mount_id, projection_id, logical_path, local_logical_path, base_generation_id,
                base_identity_json, base_payload_delta_id, base_payload_entry_index,
                conflict_payload_delta_id, conflict_payload_entry_index,
                state, incoming_identity_json, updated_at
         FROM generation_paths
         WHERE mount_id = ?1 AND source_connection_id = ?2
         ORDER BY projection_id",
    )?;
    let rows = statement.query_map(
        params![mount_id.0.as_str(), source_connection_id.as_str()],
        |row| {
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
            ))
        },
    )?;
    rows.map(|row| generation_path_from_row(row?)).collect()
}

fn list_all_generation_paths_from_connection(
    connection: &Connection,
    mount_id: &MountId,
) -> StoreResult<Vec<GenerationPathRecord>> {
    let mut statement = connection.prepare(
        "SELECT mount_id, projection_id, logical_path, local_logical_path, base_generation_id,
                base_identity_json, base_payload_delta_id, base_payload_entry_index,
                conflict_payload_delta_id, conflict_payload_entry_index,
                state, incoming_identity_json, updated_at
         FROM generation_paths WHERE mount_id = ?1 ORDER BY projection_id",
    )?;
    let rows = statement.query_map(params![mount_id.0.as_str()], |row| {
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
        ))
    })?;
    rows.map(|row| generation_path_from_row(row?)).collect()
}

struct StoredGenerationApply {
    journal: GenerationApplyJournalRecord,
    selection_binding: GenerationTransportSelectionBinding,
    acknowledgment_required: bool,
    acknowledged_at: Option<String>,
}

impl Deref for StoredGenerationApply {
    type Target = GenerationApplyJournalRecord;

    fn deref(&self) -> &Self::Target {
        &self.journal
    }
}

fn generation_apply_from_connection(
    connection: &Connection,
    delta_id: &str,
) -> StoreResult<Option<StoredGenerationApply>> {
    let row = connection
        .query_row(
            "SELECT mount_id, delta_json, receipt_json, receipt_sha256,
                    selected_capabilities_json, selection_binding, acknowledgment_required,
                    acknowledged_at, stage_root, status, active, created_at, updated_at, completed_at
             FROM generation_apply_journals WHERE delta_id = ?1",
            params![delta_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, bool>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, bool>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, String>(12)?,
                    row.get::<_, Option<String>>(13)?,
                ))
            },
        )
        .optional()?;
    let Some(row) = row else {
        return Ok(None);
    };
    let mut outcomes = Vec::new();
    let mut statement = connection.prepare(
        "SELECT entry_index, outcome_json
         FROM generation_apply_outcomes WHERE delta_id = ?1 ORDER BY entry_index",
    )?;
    let rows = statement.query_map(params![delta_id], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (entry_index, outcome_json) = row?;
        outcomes.push((
            u64::try_from(entry_index).map_err(|_| {
                StoreError::InvalidState("negative generation outcome index".to_string())
            })?,
            from_json(&outcome_json)?,
        ));
    }
    let delta: GenerationDelta = from_json(&row.1)?;
    if delta.mount_id.as_str() != row.0 {
        return Err(StoreError::InvalidState(format!(
            "generation apply `{delta_id}` mount relation does not match its delta"
        )));
    }
    let status = GenerationApplyStatus::parse(&row.9)?;
    let selection_binding = match row.5.as_str() {
        "bound" => {
            let selected_capabilities: GenerationTransportCapabilities = from_json(&row.4)?;
            selected_capabilities
                .validate()
                .map_err(|error| StoreError::InvalidState(error.to_string()))?;
            if selected_capabilities.terminal_receipt_acknowledgments != row.6 {
                return Err(StoreError::InvalidState(format!(
                    "generation apply `{delta_id}` acknowledgment selection does not match its journal"
                )));
            }
            GenerationTransportSelectionBinding::Bound(selected_capabilities)
        }
        "pre_binding_completed" if status == GenerationApplyStatus::Completed && !row.10 => {
            GenerationTransportSelectionBinding::PreBindingCompleted {
                terminal_receipt_acknowledgments: row.6,
            }
        }
        binding => {
            return Err(StoreError::InvalidState(format!(
                "generation apply `{delta_id}` has invalid transport selection binding `{binding}`"
            )));
        }
    };
    Ok(Some(StoredGenerationApply {
        selection_binding,
        acknowledgment_required: row.6,
        acknowledged_at: row.7,
        journal: GenerationApplyJournalRecord {
            delta,
            receipt: from_json(&row.2)?,
            receipt_sha256: row.3,
            stage_root: row.8,
            status,
            outcomes,
            created_at: row.11,
            updated_at: row.12,
            completed_at: row.13,
        },
    }))
}

fn validate_seed_generation(
    observed: &ObservedGenerationRecord,
    paths: &[GenerationPathRecord],
) -> StoreResult<()> {
    if observed.workspace_layout_version == 0
        || observed.workspace_layout_digest.is_empty()
        || observed.inventory_sha256.is_empty()
        || observed.updated_at.is_empty()
    {
        return Err(StoreError::InvalidState(
            "observed generation has incomplete metadata".to_string(),
        ));
    }
    let mut projections = BTreeSet::new();
    let mut logical_paths = BTreeSet::new();
    let mut local_paths = BTreeSet::new();
    let mut inventory = Vec::with_capacity(paths.len());
    for path in paths {
        let logical_path = locality_core::portable::LogicalPath::new(path.logical_path.clone())
            .map_err(|error| StoreError::InvalidState(error.to_string()))?;
        let local_path = locality_core::portable::LogicalPath::new(path.local_logical_path.clone())
            .map_err(|error| StoreError::InvalidState(error.to_string()))?;
        if path.mount_id != observed.mount_id
            || path.base_generation_id != observed.generation_id
            || path.updated_at.is_empty()
            || !projections.insert(path.projection_id.clone())
            || !logical_paths.insert(logical_path.portable_collision_key())
            || !local_paths.insert(local_path.portable_collision_key())
        {
            return Err(StoreError::InvalidState(
                "generation path seed does not match its observed generation".to_string(),
            ));
        }
        let Some(identity) = &path.base_identity else {
            return Err(StoreError::InvalidState(
                "generation baseline path has no clean base identity".to_string(),
            ));
        };
        if identity.projection_id != path.projection_id
            || identity.logical_path.as_str() != path.logical_path
        {
            return Err(StoreError::InvalidState(
                "generation path base identity does not match its row".to_string(),
            ));
        }
        if path.state != GenerationPathState::Clean
            || path.local_logical_path != path.logical_path
            || path.incoming_identity.is_some()
            || path.base_payload_delta_id.is_some()
            || path.base_payload_entry_index.is_some()
            || path.conflict_payload_delta_id.is_some()
            || path.conflict_payload_entry_index.is_some()
        {
            return Err(StoreError::InvalidState(
                "generation baseline path is not a complete clean base".to_string(),
            ));
        }
        inventory.push(identity.clone());
    }
    inventory.sort_by(|left, right| left.projection_id.cmp(&right.projection_id));
    let baseline_source = GenerationBaselineSourceV1::new(
        observed.source_connection_id.clone(),
        observed.generation_id.clone(),
        inventory,
    )
    .map_err(|error| StoreError::InvalidState(error.to_string()))?;
    if baseline_source.target_inventory_sha256() != observed.inventory_sha256 {
        return Err(StoreError::InvalidState(
            "observed generation inventory does not match its complete path identities".to_string(),
        ));
    }
    Ok(())
}

impl GenerationDeliveryRepository for SqliteStateStore {
    fn seed_observed_generation(
        &mut self,
        observed: ObservedGenerationRecord,
        paths: Vec<GenerationPathRecord>,
    ) -> StoreResult<()> {
        let existing =
            list_observed_generations_from_connection(&self.connection()?, &observed.mount_id)?;
        if existing
            .iter()
            .any(|record| record.source_connection_id != observed.source_connection_id)
        {
            return Err(StoreError::InvalidState(format!(
                "mount `{}` has multiple observed generation sources; use atomic multi-source baseline seeding",
                observed.mount_id.0
            )));
        }
        self.seed_observed_generations_v2(vec![GenerationBaselineSeedRecordV2::new(
            GenerationBaselineSeedRecord::new(observed, paths),
            GenerationBaselineRefreshModeV1::GenerationDeltaV1,
        )])
    }

    fn seed_observed_generations(
        &mut self,
        seeds: Vec<GenerationBaselineSeedRecord>,
    ) -> StoreResult<()> {
        self.seed_observed_generations_v2(
            seeds
                .into_iter()
                .map(|seed| {
                    GenerationBaselineSeedRecordV2::new(
                        seed,
                        GenerationBaselineRefreshModeV1::GenerationDeltaV1,
                    )
                })
                .collect(),
        )
    }

    fn seed_observed_generations_v2(
        &mut self,
        mut seeds: Vec<GenerationBaselineSeedRecordV2>,
    ) -> StoreResult<()> {
        let mut baseline_layout = None;
        for seed in &mut seeds {
            validate_seed_generation(&seed.seed.observed, &seed.seed.paths)?;
            let layout = (
                seed.seed.observed.workspace_layout_version,
                seed.seed.observed.workspace_layout_digest.clone(),
            );
            match &baseline_layout {
                Some(existing) if existing != &layout => {
                    return Err(StoreError::InvalidState(
                        "generation baseline mounts do not share one workspace layout".to_string(),
                    ));
                }
                None => baseline_layout = Some(layout),
                Some(_) => {}
            }
            let expected_mode = generation_seed_refresh_mode(&seed.seed.observed, &seed.seed.paths);
            if seed.refresh_mode != expected_mode {
                return Err(StoreError::InvalidState(format!(
                    "mount `{}` source `{}` baseline refresh mode does not match its generation state",
                    seed.seed.observed.mount_id.0,
                    seed.seed.observed.source_connection_id.as_str()
                )));
            }
            seed.seed
                .paths
                .sort_by(|left, right| left.projection_id.cmp(&right.projection_id));
        }
        seeds.sort_by(|left, right| {
            left.seed
                .observed
                .mount_id
                .cmp(&right.seed.observed.mount_id)
                .then_with(|| {
                    left.seed
                        .observed
                        .source_connection_id
                        .cmp(&right.seed.observed.source_connection_id)
                })
        });
        let mut source_pairs = BTreeSet::new();
        if seeds.iter().any(|seed| {
            !source_pairs.insert((
                seed.seed.observed.mount_id.clone(),
                seed.seed.observed.source_connection_id.clone(),
            ))
        }) {
            return Err(StoreError::InvalidState(
                "generation baseline repeats a mount/source record".to_string(),
            ));
        }

        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for seed in &seeds {
            let mount_exists = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM mounts WHERE mount_id = ?1)",
                params![seed.seed.observed.mount_id.0.as_str()],
                |row| row.get::<_, bool>(0),
            )?;
            if !mount_exists {
                return Err(StoreError::MountMissing(
                    seed.seed.observed.mount_id.clone(),
                ));
            }
        }

        let mounts = seeds
            .iter()
            .map(|seed| seed.seed.observed.mount_id.clone())
            .collect::<BTreeSet<_>>();
        for mount_id in &mounts {
            let active: Option<String> = transaction
                .query_row(
                    "SELECT delta_id FROM generation_apply_journals
                     WHERE mount_id = ?1 AND active = 1 ORDER BY delta_id LIMIT 1",
                    params![mount_id.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(delta_id) = active {
                return Err(StoreError::InvalidState(format!(
                    "mount `{}` cannot seed a generation baseline while apply `{delta_id}` is active",
                    mount_id.0
                )));
            }

            // BEGIN IMMEDIATE serializes competing seeders. Once any source
            // exists, the batch must name the complete durable source set;
            // the exact record, inventory, layout, mode, and path checks below
            // then bind the replay to that same baseline.
            let existing_sources =
                list_observed_generations_from_connection(&transaction, mount_id)?
                    .into_iter()
                    .map(|record| record.source_connection_id)
                    .collect::<BTreeSet<_>>();
            let incoming_sources = seeds
                .iter()
                .filter(|seed| &seed.seed.observed.mount_id == mount_id)
                .map(|seed| seed.seed.observed.source_connection_id.clone())
                .collect::<BTreeSet<_>>();
            if !existing_sources.is_empty() && existing_sources != incoming_sources {
                return Err(StoreError::InvalidState(format!(
                    "mount `{}` already has a generation baseline; replay must include every exact source",
                    mount_id.0
                )));
            }
        }

        // Validate every replay before inserting any new source. This keeps a
        // mixed exact/changed batch closed and transactionally invisible.
        let mut insert_indexes = Vec::new();
        for (index, seed) in seeds.iter().enumerate() {
            if let Some(existing) = select_observed_generation_for_source(
                &transaction,
                &seed.seed.observed.mount_id,
                &seed.seed.observed.source_connection_id,
            )? {
                let existing_v2 = select_observed_generation_for_source_v2(
                    &transaction,
                    &seed.seed.observed.mount_id,
                    &seed.seed.observed.source_connection_id,
                )?
                .expect("the same observed row was selected");
                let existing_paths = list_generation_paths_for_source_from_connection(
                    &transaction,
                    &seed.seed.observed.mount_id,
                    &seed.seed.observed.source_connection_id,
                )?;
                if existing != seed.seed.observed
                    || existing_v2.refresh_mode != seed.refresh_mode
                    || existing_paths != seed.seed.paths
                {
                    return Err(StoreError::InvalidState(format!(
                        "mount `{}` source `{}` already has a different observed generation",
                        seed.seed.observed.mount_id.0,
                        seed.seed.observed.source_connection_id.as_str()
                    )));
                }
            } else {
                insert_indexes.push(index);
            }
        }

        // Projection IDs and both remote and local portable paths remain
        // mount-wide namespaces even when their source heads are independent.
        let mut projections = BTreeSet::new();
        let mut logical_paths = BTreeSet::new();
        let mut local_paths = BTreeSet::new();
        for mount_id in &mounts {
            for path in list_all_generation_paths_from_connection(&transaction, mount_id)? {
                projections.insert((mount_id.clone(), path.projection_id));
                logical_paths.insert((
                    mount_id.clone(),
                    locality_core::portable::LogicalPath::new(path.logical_path)
                        .map_err(|error| StoreError::InvalidState(error.to_string()))?
                        .portable_collision_key(),
                ));
                local_paths.insert((
                    mount_id.clone(),
                    locality_core::portable::LogicalPath::new(path.local_logical_path)
                        .map_err(|error| StoreError::InvalidState(error.to_string()))?
                        .portable_collision_key(),
                ));
            }
        }
        for &index in &insert_indexes {
            let seed = &seeds[index];
            for path in &seed.seed.paths {
                let logical = locality_core::portable::LogicalPath::new(path.logical_path.clone())
                    .map_err(|error| StoreError::InvalidState(error.to_string()))?;
                let local =
                    locality_core::portable::LogicalPath::new(path.local_logical_path.clone())
                        .map_err(|error| StoreError::InvalidState(error.to_string()))?;
                if !projections.insert((
                    seed.seed.observed.mount_id.clone(),
                    path.projection_id.clone(),
                )) || !logical_paths.insert((
                    seed.seed.observed.mount_id.clone(),
                    logical.portable_collision_key(),
                )) || !local_paths.insert((
                    seed.seed.observed.mount_id.clone(),
                    local.portable_collision_key(),
                )) {
                    return Err(StoreError::InvalidState(format!(
                        "mount `{}` generation baseline has a projection or path collision",
                        seed.seed.observed.mount_id.0
                    )));
                }
            }
        }

        for index in insert_indexes {
            let seed = &seeds[index];
            let observed = &seed.seed.observed;
            transaction.execute(
                "INSERT INTO observed_generations (
                    mount_id, source_connection_id, generation_id, inventory_sha256,
                    workspace_layout_version, workspace_layout_digest,
                    last_receipt_sha256, updated_at, refresh_mode
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    observed.mount_id.0.as_str(),
                    observed.source_connection_id.as_str(),
                    observed.generation_id.as_str(),
                    observed.inventory_sha256.as_str(),
                    observed.workspace_layout_version,
                    observed.workspace_layout_digest.as_str(),
                    observed.last_receipt_sha256.as_deref(),
                    observed.updated_at.as_str(),
                    generation_baseline_refresh_mode_label(seed.refresh_mode),
                ],
            )?;
            for path in &seed.seed.paths {
                transaction.execute(
                    "INSERT INTO generation_paths (
                        mount_id, source_connection_id, projection_id, logical_path,
                        local_logical_path, base_generation_id, base_identity_json,
                        base_payload_delta_id, base_payload_entry_index,
                        conflict_payload_delta_id, conflict_payload_entry_index,
                        state, incoming_identity_json, updated_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    params![
                        path.mount_id.0.as_str(),
                        observed.source_connection_id.as_str(),
                        path.projection_id.as_str(),
                        path.logical_path.as_str(),
                        path.local_logical_path.as_str(),
                        path.base_generation_id.as_str(),
                        path.base_identity.as_ref().map(to_json).transpose()?,
                        path.base_payload_delta_id.as_deref(),
                        path.base_payload_entry_index
                            .map(i64::try_from)
                            .transpose()
                            .map_err(|_| StoreError::InvalidState(
                                "generation base payload index is too large".to_string()
                            ))?,
                        path.conflict_payload_delta_id.as_deref(),
                        path.conflict_payload_entry_index
                            .map(i64::try_from)
                            .transpose()
                            .map_err(|_| StoreError::InvalidState(
                                "generation conflict payload index is too large".to_string()
                            ))?,
                        generation_path_state_label(path.state),
                        path.incoming_identity.as_ref().map(to_json).transpose()?,
                        path.updated_at.as_str(),
                    ],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    fn get_observed_generation(
        &self,
        mount_id: &MountId,
    ) -> StoreResult<Option<ObservedGenerationRecord>> {
        select_observed_generation(&self.connection()?, mount_id)
    }

    fn get_observed_generation_for_source(
        &self,
        mount_id: &MountId,
        source_connection_id: &locality_core::portable::SourceConnectionId,
    ) -> StoreResult<Option<ObservedGenerationRecord>> {
        select_observed_generation_for_source(&self.connection()?, mount_id, source_connection_id)
    }

    fn get_observed_generation_for_source_v2(
        &self,
        mount_id: &MountId,
        source_connection_id: &locality_core::portable::SourceConnectionId,
    ) -> StoreResult<Option<ObservedGenerationRecordV2>> {
        select_observed_generation_for_source_v2(
            &self.connection()?,
            mount_id,
            source_connection_id,
        )
    }

    fn list_observed_generations(
        &self,
        mount_id: &MountId,
    ) -> StoreResult<Vec<ObservedGenerationRecord>> {
        list_observed_generations_from_connection(&self.connection()?, mount_id)
    }

    fn list_observed_generations_v2(
        &self,
        mount_id: &MountId,
    ) -> StoreResult<Vec<ObservedGenerationRecordV2>> {
        list_observed_generations_v2_from_connection(&self.connection()?, mount_id)
    }

    fn list_generation_paths(&self, mount_id: &MountId) -> StoreResult<Vec<GenerationPathRecord>> {
        list_generation_paths_from_connection(&self.connection()?, mount_id)
    }

    fn list_generation_paths_for_source(
        &self,
        mount_id: &MountId,
        source_connection_id: &locality_core::portable::SourceConnectionId,
    ) -> StoreResult<Vec<GenerationPathRecord>> {
        list_generation_paths_for_source_from_connection(
            &self.connection()?,
            mount_id,
            source_connection_id,
        )
    }

    fn reset_observed_generation_source(
        &mut self,
        mount_id: &MountId,
        source_connection_id: &locality_core::portable::SourceConnectionId,
    ) -> StoreResult<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let active_or_unacknowledged: Option<String> = transaction
            .query_row(
                "SELECT delta_id FROM generation_apply_journals
                 WHERE mount_id = ?1
                   AND (
                       active = 1
                       OR (
                           source_connection_id = ?2
                           AND acknowledgment_required = 1
                           AND acknowledged_at IS NULL
                       )
                   )
                 ORDER BY delta_id LIMIT 1",
                params![mount_id.as_str(), source_connection_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(delta_id) = active_or_unacknowledged {
            return Err(StoreError::InvalidState(format!(
                "mount `{}` source `{}` cannot reset generation delivery while apply `{delta_id}` is active or unacknowledged",
                mount_id.0,
                source_connection_id.as_str()
            )));
        }
        let preserved_path: Option<(String, String)> = transaction
            .query_row(
                "SELECT projection_id, state FROM generation_paths
                 WHERE mount_id = ?1 AND source_connection_id = ?2
                   AND state IN ('dirty', 'conflicted')
                 ORDER BY projection_id LIMIT 1",
                params![mount_id.as_str(), source_connection_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((projection_id, state)) = preserved_path {
            return Err(StoreError::InvalidState(format!(
                "mount `{}` source `{}` cannot reset generation path `{projection_id}` while it is {state}",
                mount_id.0,
                source_connection_id.as_str()
            )));
        }
        let retained_inode: Option<String> = transaction
            .query_row(
                "SELECT evidence.logical_path
                 FROM generation_inode_evidence AS evidence
                 JOIN generation_apply_journals AS journals
                   ON journals.delta_id = evidence.delta_id
                 WHERE journals.mount_id = ?1 AND journals.source_connection_id = ?2
                 ORDER BY evidence.logical_path LIMIT 1",
                params![mount_id.as_str(), source_connection_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(logical_path) = retained_inode {
            return Err(StoreError::InvalidState(format!(
                "mount `{}` source `{}` cannot reset while displaced inode evidence for `{logical_path}` is retained",
                mount_id.0,
                source_connection_id.as_str()
            )));
        }
        transaction.execute(
            "DELETE FROM generation_apply_journals
             WHERE mount_id = ?1 AND source_connection_id = ?2 AND active = 0",
            params![mount_id.as_str(), source_connection_id.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM observed_generations
             WHERE mount_id = ?1 AND source_connection_id = ?2",
            params![mount_id.as_str(), source_connection_id.as_str()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn reserve_generation_apply(
        &mut self,
        prepared: PreparedGenerationApply,
    ) -> StoreResult<GenerationApplyJournalRecord> {
        self.reserve_generation_apply_v2(PreparedGenerationApplyV2::new(prepared, false))
    }

    fn reserve_generation_apply_v2(
        &mut self,
        prepared: PreparedGenerationApplyV2,
    ) -> StoreResult<GenerationApplyJournalRecord> {
        let mut selected_capabilities = GenerationTransportCapabilities::legacy();
        selected_capabilities.terminal_receipt_acknowledgments = prepared.acknowledgment_required;
        self.reserve_generation_apply_v3(PreparedGenerationApplyV3::new(
            prepared.apply,
            selected_capabilities,
        ))
        .map(|record| record.apply)
    }

    fn reserve_generation_apply_v3(
        &mut self,
        prepared: PreparedGenerationApplyV3,
    ) -> StoreResult<NegotiatedGenerationApplyJournalRecord> {
        prepared
            .selected_capabilities
            .validate()
            .map_err(|error| StoreError::InvalidState(error.to_string()))?;
        let selected_capabilities = prepared.selected_capabilities;
        let acknowledgment_required = selected_capabilities.terminal_receipt_acknowledgments;
        let prepared = prepared.apply;
        prepared.validate()?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) =
            generation_apply_from_connection(&transaction, &prepared.delta.delta_id)?
        {
            let immutable_apply_matches = existing.delta == prepared.delta
                && existing.receipt == prepared.receipt
                && existing.receipt_sha256 == prepared.receipt_sha256
                && existing.stage_root == prepared.stage_root
                && existing.created_at == prepared.created_at;
            let selection_matches = match &existing.selection_binding {
                GenerationTransportSelectionBinding::Bound(bound) => {
                    bound == &selected_capabilities
                        && existing.acknowledgment_required == acknowledgment_required
                }
                GenerationTransportSelectionBinding::PreBindingCompleted { .. } => {
                    existing.status == GenerationApplyStatus::Completed
                }
            };
            if immutable_apply_matches && selection_matches {
                transaction.commit()?;
                return Ok(NegotiatedGenerationApplyJournalRecord {
                    apply: existing.journal,
                    selection_binding: existing.selection_binding,
                });
            }
            return Err(StoreError::InvalidState(format!(
                "generation apply `{}` replay changed its immutable payload",
                prepared.delta.delta_id
            )));
        }

        let mount_id = MountId::new(prepared.delta.mount_id.as_str());
        let observed = select_observed_generation_for_source_v2(
            &transaction,
            &mount_id,
            &prepared.delta.source_connection_id,
        )?
        .ok_or_else(|| {
            StoreError::InvalidState(format!(
                "mount `{}` source `{}` has no observed generation",
                prepared.delta.mount_id,
                prepared.delta.source_connection_id.as_str()
            ))
        })?;
        if observed.refresh_mode != GenerationBaselineRefreshModeV1::GenerationDeltaV1 {
            return Err(StoreError::InvalidState(format!(
                "mount `{}` source `{}` requires a full export",
                prepared.delta.mount_id,
                prepared.delta.source_connection_id.as_str()
            )));
        }
        let observed = observed.observed;
        if observed.generation_id != prepared.delta.base_generation_id
            || observed.workspace_layout_version != prepared.delta.workspace_layout_version
            || observed.workspace_layout_digest != prepared.delta.workspace_layout_digest.as_str()
        {
            return Err(StoreError::InvalidState(format!(
                "mount `{}` generation or layout does not match delta base",
                prepared.delta.mount_id
            )));
        }
        for entry in &prepared.delta.entries {
            if let Some(old) = &entry.old {
                let path = select_generation_path(
                    &transaction,
                    &mount_id,
                    &prepared.delta.source_connection_id,
                    &old.projection_id,
                )?
                .ok_or_else(|| {
                    StoreError::InvalidState(format!(
                        "delta old projection `{}` has no local merge base",
                        old.projection_id.as_str()
                    ))
                })?;
                let expected = if path.state == GenerationPathState::Conflicted {
                    path.incoming_identity.as_ref()
                } else {
                    path.base_identity.as_ref()
                };
                if expected != Some(old) {
                    return Err(StoreError::InvalidState(format!(
                        "delta old projection `{}` does not match local merge state",
                        old.projection_id.as_str()
                    )));
                }
            }
        }
        let active: Option<String> = transaction
            .query_row(
                "SELECT delta_id FROM generation_apply_journals
                 WHERE mount_id = ?1 AND active = 1",
                params![prepared.delta.mount_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(active) = active {
            return Err(StoreError::InvalidState(format!(
                "mount already has active generation apply `{active}`"
            )));
        }
        transaction.execute(
            "INSERT INTO generation_apply_journals (
                delta_id, mount_id, source_connection_id, base_generation_id,
                target_generation_id, delta_json, receipt_json, receipt_sha256,
                selected_capabilities_json, selection_binding, acknowledgment_required, acknowledged_at,
                stage_root, status, active, created_at, updated_at, completed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'bound', ?10, NULL, ?11, 'staged', 1, ?12, ?12, NULL)",
            params![
                prepared.delta.delta_id.as_str(),
                prepared.delta.mount_id.as_str(),
                prepared.delta.source_connection_id.as_str(),
                prepared.delta.base_generation_id.as_str(),
                prepared.delta.target_generation_id.as_str(),
                to_json(&prepared.delta)?,
                to_json(&prepared.receipt)?,
                prepared.receipt_sha256.as_str(),
                to_json(&selected_capabilities)?,
                acknowledgment_required,
                prepared.stage_root.as_str(),
                prepared.created_at.as_str(),
            ],
        )?;
        let record = generation_apply_from_connection(&transaction, &prepared.delta.delta_id)?
            .expect("inserted generation journal exists");
        transaction.commit()?;
        Ok(NegotiatedGenerationApplyJournalRecord {
            apply: record.journal,
            selection_binding: record.selection_binding,
        })
    }

    fn mark_generation_apply_started(
        &mut self,
        delta_id: &str,
        updated_at: &str,
    ) -> StoreResult<GenerationApplyJournalRecord> {
        let connection = self.connection()?;
        let existing =
            generation_apply_from_connection(&connection, delta_id)?.ok_or_else(|| {
                StoreError::InvalidState(format!("generation apply `{delta_id}` is missing"))
            })?;
        if existing.status == GenerationApplyStatus::Completed {
            return Ok(existing.journal);
        }
        connection.execute(
            "UPDATE generation_apply_journals
             SET status = 'applying', updated_at = ?2
             WHERE delta_id = ?1 AND active = 1",
            params![delta_id, updated_at],
        )?;
        generation_apply_from_connection(&connection, delta_id)?
            .map(|stored| stored.journal)
            .ok_or_else(|| {
                StoreError::InvalidState(format!("generation apply `{delta_id}` disappeared"))
            })
    }

    fn record_generation_apply_outcome(
        &mut self,
        delta_id: &str,
        entry_index: u64,
        outcome: GenerationApplyOutcome,
        updated_at: &str,
    ) -> StoreResult<GenerationApplyJournalRecord> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let journal =
            generation_apply_from_connection(&transaction, delta_id)?.ok_or_else(|| {
                StoreError::InvalidState(format!("generation apply `{delta_id}` is missing"))
            })?;
        if journal.status == GenerationApplyStatus::Completed {
            let exact = journal
                .outcomes
                .iter()
                .find(|(index, _)| *index == entry_index)
                .is_some_and(|(_, existing)| existing == &outcome);
            return if exact {
                Ok(journal.journal)
            } else {
                Err(StoreError::InvalidState(format!(
                    "completed generation apply `{delta_id}` outcome changed"
                )))
            };
        }
        if entry_index >= journal.delta.entries.len() as u64 {
            return Err(StoreError::InvalidState(format!(
                "generation apply `{delta_id}` outcome index is out of bounds"
            )));
        }
        if let Some((_, existing)) = journal
            .outcomes
            .iter()
            .find(|(index, _)| *index == entry_index)
        {
            return if existing == &outcome {
                Ok(journal.journal)
            } else {
                Err(StoreError::InvalidState(format!(
                    "generation apply `{delta_id}` outcome replay changed"
                )))
            };
        }
        transaction.execute(
            "INSERT INTO generation_apply_outcomes (delta_id, entry_index, outcome_json, updated_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                delta_id,
                i64::try_from(entry_index).map_err(|_| StoreError::InvalidState(
                    "generation apply outcome index is too large".to_string()
                ))?,
                to_json(&outcome)?,
                updated_at
            ],
        )?;
        transaction.execute(
            "UPDATE generation_apply_journals SET status = 'applying', updated_at = ?2
             WHERE delta_id = ?1 AND active = 1",
            params![delta_id, updated_at],
        )?;
        let updated = generation_apply_from_connection(&transaction, delta_id)?
            .expect("generation journal remains present");
        transaction.commit()?;
        Ok(updated.journal)
    }

    fn get_generation_apply(
        &self,
        delta_id: &str,
    ) -> StoreResult<Option<GenerationApplyJournalRecord>> {
        Ok(
            generation_apply_from_connection(&self.connection()?, delta_id)?
                .map(|stored| stored.journal),
        )
    }

    fn get_generation_apply_v2(
        &self,
        delta_id: &str,
    ) -> StoreResult<Option<NegotiatedGenerationApplyJournalRecord>> {
        Ok(
            generation_apply_from_connection(&self.connection()?, delta_id)?.map(|stored| {
                NegotiatedGenerationApplyJournalRecord {
                    apply: stored.journal,
                    selection_binding: stored.selection_binding,
                }
            }),
        )
    }

    fn list_active_generation_applies(&self) -> StoreResult<Vec<GenerationApplyJournalRecord>> {
        let connection = self.connection()?;
        let delta_ids = {
            let mut statement = connection.prepare(
                "SELECT delta_id FROM generation_apply_journals WHERE active = 1 ORDER BY delta_id",
            )?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        delta_ids
            .iter()
            .map(|delta_id| {
                generation_apply_from_connection(&connection, delta_id)?
                    .map(|stored| stored.journal)
                    .ok_or_else(|| {
                        StoreError::InvalidState(format!(
                            "active generation apply `{delta_id}` disappeared"
                        ))
                    })
            })
            .collect()
    }

    fn list_active_generation_applies_for_mount(
        &self,
        mount_id: &MountId,
    ) -> StoreResult<Vec<GenerationApplyJournalRecord>> {
        let connection = self.connection()?;
        let delta_ids = {
            let mut statement = connection.prepare(
                "SELECT delta_id FROM generation_apply_journals
                 WHERE mount_id = ?1 AND active = 1 ORDER BY delta_id",
            )?;
            statement
                .query_map(params![mount_id.as_str()], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        delta_ids
            .iter()
            .map(|delta_id| {
                generation_apply_from_connection(&connection, delta_id)?
                    .map(|stored| stored.journal)
                    .ok_or_else(|| {
                        StoreError::InvalidState(format!(
                            "active generation apply `{delta_id}` disappeared"
                        ))
                    })
            })
            .collect()
    }

    fn list_generation_applies(&self) -> StoreResult<Vec<GenerationApplyJournalRecord>> {
        let connection = self.connection()?;
        let delta_ids = {
            let mut statement = connection.prepare(
                "SELECT delta_id FROM generation_apply_journals ORDER BY created_at, delta_id",
            )?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        delta_ids
            .iter()
            .map(|delta_id| {
                generation_apply_from_connection(&connection, delta_id)?
                    .map(|stored| stored.journal)
                    .ok_or_else(|| {
                        StoreError::InvalidState(format!(
                            "generation apply `{delta_id}` disappeared"
                        ))
                    })
            })
            .collect()
    }

    fn list_pending_generation_acknowledgments(
        &self,
        mount_id: &MountId,
    ) -> StoreResult<Vec<GenerationApplyJournalRecord>> {
        let connection = self.connection()?;
        let delta_ids = {
            let mut statement = connection.prepare(
                "SELECT delta_id FROM generation_apply_journals
                 WHERE mount_id = ?1 AND status = 'completed'
                   AND acknowledgment_required = 1 AND acknowledged_at IS NULL
                 ORDER BY completed_at, delta_id",
            )?;
            statement
                .query_map(params![mount_id.as_str()], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        delta_ids
            .iter()
            .map(|delta_id| {
                generation_apply_from_connection(&connection, delta_id)?
                    .map(|stored| stored.journal)
                    .ok_or_else(|| {
                        StoreError::InvalidState(format!(
                            "pending generation acknowledgment `{delta_id}` disappeared"
                        ))
                    })
            })
            .collect()
    }

    fn list_pending_generation_acknowledgments_for_source(
        &self,
        mount_id: &MountId,
        source_connection_id: &locality_core::portable::SourceConnectionId,
    ) -> StoreResult<Vec<GenerationApplyJournalRecord>> {
        let connection = self.connection()?;
        let delta_ids = {
            let mut statement = connection.prepare(
                "SELECT delta_id FROM generation_apply_journals
                 WHERE mount_id = ?1 AND source_connection_id = ?2
                   AND status = 'completed'
                   AND acknowledgment_required = 1 AND acknowledged_at IS NULL
                 ORDER BY completed_at, delta_id",
            )?;
            statement
                .query_map(
                    params![mount_id.as_str(), source_connection_id.as_str()],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        delta_ids
            .iter()
            .map(|delta_id| {
                generation_apply_from_connection(&connection, delta_id)?
                    .map(|stored| stored.journal)
                    .ok_or_else(|| {
                        StoreError::InvalidState(format!(
                            "pending generation acknowledgment `{delta_id}` disappeared"
                        ))
                    })
            })
            .collect()
    }

    fn mark_generation_acknowledged(
        &mut self,
        delta_id: &str,
        receipt_sha256: &str,
        acknowledged_at: &str,
    ) -> StoreResult<GenerationApplyJournalRecord> {
        if acknowledged_at.is_empty() {
            return Err(StoreError::InvalidState(
                "generation acknowledgment timestamp must not be empty".to_string(),
            ));
        }
        let connection = self.connection()?;
        let journal =
            generation_apply_from_connection(&connection, delta_id)?.ok_or_else(|| {
                StoreError::InvalidState(format!("generation apply `{delta_id}` is missing"))
            })?;
        if journal.status != GenerationApplyStatus::Completed
            || !journal.acknowledgment_required
            || journal.receipt_sha256 != receipt_sha256
        {
            return Err(StoreError::InvalidState(format!(
                "generation acknowledgment `{delta_id}` does not match a completed required receipt"
            )));
        }
        if journal.acknowledged_at.is_some() {
            return Ok(journal.journal);
        }
        let changed = connection.execute(
            "UPDATE generation_apply_journals
             SET acknowledged_at = ?3, updated_at = ?3
             WHERE delta_id = ?1 AND receipt_sha256 = ?2
               AND status = 'completed' AND acknowledgment_required = 1
               AND acknowledged_at IS NULL",
            params![delta_id, receipt_sha256, acknowledged_at],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidState(format!(
                "generation acknowledgment `{delta_id}` changed concurrently"
            )));
        }
        generation_apply_from_connection(&connection, delta_id)?
            .map(|stored| stored.journal)
            .ok_or_else(|| {
                StoreError::InvalidState(format!(
                    "acknowledged generation apply `{delta_id}` disappeared"
                ))
            })
    }

    fn record_generation_inode_evidence(
        &mut self,
        evidence: GenerationInodeEvidenceRecord,
    ) -> StoreResult<()> {
        // The SQL column names predate the captured-reservation model and stay
        // stable for schema compatibility. They store captured snapshots, not
        // live post-resolution filesystem fingerprints or disk usage.
        if evidence.resolved_at.is_some() {
            return Err(StoreError::InvalidState(
                "new generation inode evidence cannot start resolved".to_string(),
            ));
        }
        let connection = self.connection()?;
        let entry_index = i64::try_from(evidence.entry_index).map_err(|_| {
            StoreError::InvalidState("generation evidence index is too large".to_string())
        })?;
        let byte_length = i64::try_from(evidence.captured_byte_length).map_err(|_| {
            StoreError::InvalidState("generation evidence length is too large".to_string())
        })?;
        let changed = connection.execute(
            "INSERT INTO generation_inode_evidence (
                delta_id, entry_index, mount_id, logical_path, evidence_name,
                expected_sha256, byte_length, base_payload_delta_id,
                base_payload_entry_index, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(delta_id, entry_index) DO NOTHING",
            params![
                evidence.delta_id.as_str(),
                entry_index,
                evidence.mount_id.0.as_str(),
                evidence.logical_path.as_str(),
                evidence.evidence_name.as_str(),
                evidence.captured_sha256.as_str(),
                byte_length,
                evidence.base_payload_delta_id.as_deref(),
                evidence
                    .base_payload_entry_index
                    .map(i64::try_from)
                    .transpose()
                    .map_err(|_| StoreError::InvalidState(
                        "generation evidence base payload index is too large".to_string()
                    ))?,
                evidence.created_at.as_str(),
            ],
        )?;
        if changed == 0 {
            let exact: bool = connection.query_row(
                "SELECT mount_id = ?3 AND logical_path = ?4 AND evidence_name = ?5
                        AND expected_sha256 = ?6 AND byte_length = ?7
                        AND base_payload_delta_id IS ?8 AND base_payload_entry_index IS ?9
                        AND created_at = ?10
                        AND visible_evidence_name IS NULL
                        AND visible_expected_sha256 IS NULL
                        AND visible_byte_length IS NULL
                        AND resolved_at IS NULL
                 FROM generation_inode_evidence WHERE delta_id = ?1 AND entry_index = ?2",
                params![
                    evidence.delta_id.as_str(),
                    entry_index,
                    evidence.mount_id.0.as_str(),
                    evidence.logical_path.as_str(),
                    evidence.evidence_name.as_str(),
                    evidence.captured_sha256.as_str(),
                    byte_length,
                    evidence.base_payload_delta_id.as_deref(),
                    evidence
                        .base_payload_entry_index
                        .map(i64::try_from)
                        .transpose()
                        .map_err(|_| StoreError::InvalidState(
                            "generation evidence base payload index is too large".to_string()
                        ))?,
                    evidence.created_at.as_str(),
                ],
                |row| row.get(0),
            )?;
            if !exact {
                return Err(StoreError::InvalidState(
                    "generation inode evidence replay changed".to_string(),
                ));
            }
        }
        Ok(())
    }

    fn list_generation_inode_evidence(&self) -> StoreResult<Vec<GenerationInodeEvidenceRecord>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT delta_id, entry_index, mount_id, logical_path, evidence_name,
                    expected_sha256, byte_length, visible_evidence_name,
                    visible_expected_sha256, visible_byte_length,
                    base_payload_delta_id, base_payload_entry_index, resolved_at, created_at
             FROM generation_inode_evidence ORDER BY created_at, delta_id, entry_index",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<i64>>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<i64>>(11)?,
                row.get::<_, Option<String>>(12)?,
                row.get::<_, String>(13)?,
            ))
        })?;
        rows.map(|row| {
            let row = row?;
            Ok(GenerationInodeEvidenceRecord {
                delta_id: row.0,
                entry_index: u64::try_from(row.1).map_err(|_| {
                    StoreError::InvalidState("negative generation evidence index".to_string())
                })?,
                mount_id: MountId::new(row.2),
                logical_path: row.3,
                evidence_name: row.4,
                captured_sha256: row.5,
                captured_byte_length: u64::try_from(row.6).map_err(|_| {
                    StoreError::InvalidState("negative generation evidence length".to_string())
                })?,
                visible_evidence: match (row.7, row.8, row.9) {
                    (Some(evidence_name), Some(captured_sha256), Some(captured_byte_length)) => {
                        Some(GenerationRetainedInodeRecord {
                            evidence_name,
                            captured_sha256,
                            captured_byte_length: u64::try_from(captured_byte_length).map_err(
                                |_| {
                                    StoreError::InvalidState(
                                        "negative visible generation evidence length".to_string(),
                                    )
                                },
                            )?,
                        })
                    }
                    (None, None, None) => None,
                    _ => {
                        return Err(StoreError::InvalidState(
                            "partial visible generation inode evidence".to_string(),
                        ));
                    }
                },
                base_payload_delta_id: row.10,
                base_payload_entry_index: row
                    .11
                    .map(|value| {
                        u64::try_from(value).map_err(|_| {
                            StoreError::InvalidState(
                                "negative generation evidence base payload index".to_string(),
                            )
                        })
                    })
                    .transpose()?,
                resolved_at: row.12,
                created_at: row.13,
            })
        })
        .collect()
    }

    fn mark_generation_inode_evidence_conflict(
        &mut self,
        delta_id: &str,
        entry_index: u64,
        update: GenerationInodeEvidenceConflictUpdate,
    ) -> StoreResult<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let journal =
            generation_apply_from_connection(&transaction, delta_id)?.ok_or_else(|| {
                StoreError::InvalidState(format!("generation apply `{delta_id}` is missing"))
            })?;
        if journal.status != GenerationApplyStatus::Completed {
            return Err(StoreError::InvalidState(
                "late inode conflict requires a completed generation apply".to_string(),
            ));
        }
        let entry = journal
            .delta
            .entries
            .get(entry_index as usize)
            .ok_or_else(|| {
                StoreError::InvalidState("generation evidence index is out of bounds".to_string())
            })?;
        let old = entry.old.as_ref().ok_or_else(|| {
            StoreError::InvalidState("generation evidence entry has no old identity".to_string())
        })?;
        let was_over_quota = journal
            .outcomes
            .iter()
            .find(|(index, _)| *index == entry_index)
            .is_some_and(|(_, outcome)| {
                matches!(outcome, GenerationApplyOutcome::ConflictOverQuota { .. })
            });
        let incoming = entry.new.as_ref();
        let existing = select_generation_path(
            &transaction,
            &MountId::new(journal.delta.mount_id.as_str()),
            &journal.delta.source_connection_id,
            &old.projection_id,
        )?;
        let (evidence_base_delta_id, evidence_base_entry_index): (Option<String>, Option<i64>) =
            transaction
                .query_row(
                    "SELECT base_payload_delta_id, base_payload_entry_index
                     FROM generation_inode_evidence
                     WHERE delta_id = ?1 AND entry_index = ?2",
                    params![
                        delta_id,
                        i64::try_from(entry_index).map_err(|_| StoreError::InvalidState(
                            "generation evidence index is too large".to_string()
                        ))?
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?
                .ok_or_else(|| {
                    StoreError::InvalidState(
                        "generation inode evidence disappeared during conflict conversion"
                            .to_string(),
                    )
                })?;
        let remote_logical_path =
            incoming.map_or(&old.logical_path, |identity| &identity.logical_path);
        let local_logical_path = existing.as_ref().map_or(old.logical_path.as_str(), |path| {
            path.local_logical_path.as_str()
        });
        let conflict_payload_delta_id = incoming.filter(|_| !was_over_quota).map(|_| delta_id);
        let conflict_payload_entry_index = incoming
            .filter(|_| !was_over_quota)
            .map(|_| i64::try_from(entry_index))
            .transpose()
            .map_err(|_| {
                StoreError::InvalidState("generation evidence index is too large".to_string())
            })?;
        let changed = transaction.execute(
            "INSERT INTO generation_paths (
                mount_id, source_connection_id, projection_id, logical_path,
                local_logical_path, base_generation_id, base_identity_json,
                base_payload_delta_id, base_payload_entry_index,
                conflict_payload_delta_id, conflict_payload_entry_index,
                state, incoming_identity_json, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                       'conflicted', ?12, ?13)
             ON CONFLICT(mount_id, projection_id) DO UPDATE SET
                logical_path = excluded.logical_path,
                local_logical_path = excluded.local_logical_path,
                base_generation_id = excluded.base_generation_id,
                base_identity_json = excluded.base_identity_json,
                base_payload_delta_id = excluded.base_payload_delta_id,
                base_payload_entry_index = excluded.base_payload_entry_index,
                conflict_payload_delta_id = excluded.conflict_payload_delta_id,
                conflict_payload_entry_index = excluded.conflict_payload_entry_index,
                state = 'conflicted', incoming_identity_json = excluded.incoming_identity_json,
                updated_at = excluded.updated_at
             WHERE generation_paths.source_connection_id = excluded.source_connection_id",
            params![
                journal.delta.mount_id.as_str(),
                journal.delta.source_connection_id.as_str(),
                old.projection_id.as_str(),
                remote_logical_path.as_str(),
                local_logical_path,
                journal.delta.base_generation_id.as_str(),
                to_json(old)?,
                evidence_base_delta_id,
                evidence_base_entry_index,
                conflict_payload_delta_id,
                conflict_payload_entry_index,
                incoming.map(to_json).transpose()?,
                update.updated_at.as_str(),
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidState(
                "generation inode conflict crossed a source-owned projection".to_string(),
            ));
        }
        let converted_outcome = if was_over_quota {
            GenerationApplyOutcome::ConflictOverQuota {
                local_sha256: Some(update.local_sha256.clone()),
                incoming_identity: incoming.cloned(),
            }
        } else {
            GenerationApplyOutcome::Conflict {
                local_sha256: Some(update.local_sha256.clone()),
                incoming_identity: incoming.cloned(),
            }
        };
        transaction.execute(
            "UPDATE generation_apply_outcomes SET outcome_json = ?3, updated_at = ?4
             WHERE delta_id = ?1 AND entry_index = ?2",
            params![
                delta_id,
                i64::try_from(entry_index).map_err(|_| StoreError::InvalidState(
                    "generation evidence index is too large".to_string()
                ))?,
                to_json(&converted_outcome)?,
                update.updated_at.as_str(),
            ],
        )?;
        let byte_length = i64::try_from(update.captured_byte_length).map_err(|_| {
            StoreError::InvalidState("generation evidence length is too large".to_string())
        })?;
        let visible_byte_length = update
            .visible_evidence
            .as_ref()
            .map(|visible| i64::try_from(visible.captured_byte_length))
            .transpose()
            .map_err(|_| {
                StoreError::InvalidState(
                    "visible generation evidence length is too large".to_string(),
                )
            })?;
        transaction.execute(
            "UPDATE generation_inode_evidence
             SET expected_sha256 = ?3, byte_length = ?4,
                 visible_evidence_name = ?5, visible_expected_sha256 = ?6,
                 visible_byte_length = ?7, resolved_at = NULL
             WHERE delta_id = ?1 AND entry_index = ?2",
            params![
                delta_id,
                i64::try_from(entry_index).map_err(|_| StoreError::InvalidState(
                    "generation evidence index is too large".to_string()
                ))?,
                update.captured_sha256.as_str(),
                byte_length,
                update
                    .visible_evidence
                    .as_ref()
                    .map(|visible| visible.evidence_name.as_str()),
                update
                    .visible_evidence
                    .as_ref()
                    .map(|visible| visible.captured_sha256.as_str()),
                visible_byte_length,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn mark_generation_inode_evidence_resolved(
        &mut self,
        delta_id: &str,
        entry_index: u64,
        resolution: GenerationInodeEvidenceResolution,
    ) -> StoreResult<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let journal =
            generation_apply_from_connection(&transaction, delta_id)?.ok_or_else(|| {
                StoreError::InvalidState(format!("generation apply `{delta_id}` is missing"))
            })?;
        if journal.status != GenerationApplyStatus::Completed {
            return Err(StoreError::InvalidState(
                "inode evidence resolution requires a completed generation apply".to_string(),
            ));
        }
        let entry = journal
            .delta
            .entries
            .get(entry_index as usize)
            .ok_or_else(|| {
                StoreError::InvalidState("generation evidence index is out of bounds".to_string())
            })?;
        let new = entry.new.as_ref().ok_or_else(|| {
            StoreError::InvalidState(
                "retained inode resolution requires an incoming identity".to_string(),
            )
        })?;
        let index = i64::try_from(entry_index).map_err(|_| {
            StoreError::InvalidState("generation evidence index is too large".to_string())
        })?;
        let byte_length = i64::try_from(resolution.captured_byte_length).map_err(|_| {
            StoreError::InvalidState("generation evidence length is too large".to_string())
        })?;
        let visible_byte_length =
            i64::try_from(resolution.visible_captured_byte_length).map_err(|_| {
                StoreError::InvalidState(
                    "visible generation evidence length is too large".to_string(),
                )
            })?;
        let exact_evidence: bool = transaction
            .query_row(
                "SELECT expected_sha256 = ?3 AND byte_length = ?4
                        AND visible_expected_sha256 = ?5
                        AND visible_byte_length = ?6
                 FROM generation_inode_evidence
                 WHERE delta_id = ?1 AND entry_index = ?2
                       AND visible_evidence_name IS NOT NULL
                       AND resolved_at IS NULL",
                params![
                    delta_id,
                    index,
                    resolution.captured_sha256.as_str(),
                    byte_length,
                    resolution.visible_captured_sha256.as_str(),
                    visible_byte_length,
                ],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::InvalidState(
                    "generation inode evidence disappeared during resolution".to_string(),
                )
            })?;
        if !exact_evidence {
            return Err(StoreError::InvalidState(
                "generation inode evidence changed during resolution".to_string(),
            ));
        }
        let existing = select_generation_path(
            &transaction,
            &MountId::new(journal.delta.mount_id.as_str()),
            &journal.delta.source_connection_id,
            &new.projection_id,
        )?;
        let local_logical_path = existing.as_ref().map_or(new.logical_path.as_str(), |path| {
            path.local_logical_path.as_str()
        });
        let changed = transaction.execute(
            "INSERT INTO generation_paths (
                mount_id, source_connection_id, projection_id, logical_path,
                local_logical_path, base_generation_id, base_identity_json,
                base_payload_delta_id, base_payload_entry_index,
                conflict_payload_delta_id, conflict_payload_entry_index,
                state, incoming_identity_json, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, NULL,
                       'dirty', NULL, ?10)
             ON CONFLICT(mount_id, projection_id) DO UPDATE SET
                logical_path = excluded.logical_path,
                local_logical_path = excluded.local_logical_path,
                base_generation_id = excluded.base_generation_id,
                base_identity_json = excluded.base_identity_json,
                base_payload_delta_id = excluded.base_payload_delta_id,
                base_payload_entry_index = excluded.base_payload_entry_index,
                conflict_payload_delta_id = NULL,
                conflict_payload_entry_index = NULL,
                state = 'dirty', incoming_identity_json = NULL,
                updated_at = excluded.updated_at
             WHERE generation_paths.source_connection_id = excluded.source_connection_id",
            params![
                journal.delta.mount_id.as_str(),
                journal.delta.source_connection_id.as_str(),
                new.projection_id.as_str(),
                new.logical_path.as_str(),
                local_logical_path,
                journal.delta.target_generation_id.as_str(),
                to_json(new)?,
                delta_id,
                index,
                resolution.updated_at.as_str(),
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidState(
                "generation inode resolution crossed a source-owned projection".to_string(),
            ));
        }
        transaction.execute(
            "UPDATE generation_apply_outcomes
             SET outcome_json = ?3, updated_at = ?4
             WHERE delta_id = ?1 AND entry_index = ?2",
            params![
                delta_id,
                index,
                to_json(&GenerationApplyOutcome::Merged)?,
                resolution.updated_at.as_str(),
            ],
        )?;
        transaction.execute(
            "UPDATE generation_inode_evidence
             SET resolved_at = ?3
             WHERE delta_id = ?1 AND entry_index = ?2",
            params![delta_id, index, resolution.updated_at.as_str()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn remove_generation_inode_evidence(
        &mut self,
        delta_id: &str,
        entry_index: u64,
    ) -> StoreResult<()> {
        self.connection()?.execute(
            "DELETE FROM generation_inode_evidence WHERE delta_id = ?1 AND entry_index = ?2",
            params![
                delta_id,
                i64::try_from(entry_index).map_err(|_| StoreError::InvalidState(
                    "generation evidence index is too large".to_string()
                ))?
            ],
        )?;
        Ok(())
    }

    fn complete_generation_apply(
        &mut self,
        delta_id: &str,
        completed_at: &str,
    ) -> StoreResult<GenerationApplyJournalRecord> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let journal =
            generation_apply_from_connection(&transaction, delta_id)?.ok_or_else(|| {
                StoreError::InvalidState(format!("generation apply `{delta_id}` is missing"))
            })?;
        if journal.status == GenerationApplyStatus::Completed {
            transaction.commit()?;
            return Ok(journal.journal);
        }
        if journal.outcomes.len() != journal.delta.entries.len()
            || journal
                .outcomes
                .iter()
                .enumerate()
                .any(|(expected, (actual, _))| *actual != expected as u64)
        {
            return Err(StoreError::InvalidState(format!(
                "generation apply `{delta_id}` does not have one outcome per entry"
            )));
        }

        let mount_id = MountId::new(journal.delta.mount_id.as_str());
        for (entry_index, (entry, (_, outcome))) in journal
            .delta
            .entries
            .iter()
            .zip(&journal.outcomes)
            .enumerate()
        {
            match outcome {
                GenerationApplyOutcome::Applied => {
                    let new = entry.new.as_ref().ok_or_else(|| {
                        StoreError::InvalidState("applied outcome has no new identity".to_string())
                    })?;
                    let changed = transaction.execute(
                        "INSERT INTO generation_paths (
                            mount_id, source_connection_id, projection_id, logical_path,
                            local_logical_path, base_generation_id,
                            base_identity_json, base_payload_delta_id, base_payload_entry_index,
                            conflict_payload_delta_id, conflict_payload_entry_index,
                            state, incoming_identity_json, updated_at
                         ) VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7, ?8,
                                   NULL, NULL, 'clean', NULL, ?9)
                         ON CONFLICT(mount_id, projection_id) DO UPDATE SET
                            logical_path = excluded.logical_path,
                            local_logical_path = excluded.local_logical_path,
                            base_generation_id = excluded.base_generation_id,
                            base_identity_json = excluded.base_identity_json,
                            base_payload_delta_id = excluded.base_payload_delta_id,
                            base_payload_entry_index = excluded.base_payload_entry_index,
                            conflict_payload_delta_id = NULL,
                            conflict_payload_entry_index = NULL,
                            state = 'clean', incoming_identity_json = NULL,
                            updated_at = excluded.updated_at
                         WHERE generation_paths.source_connection_id = excluded.source_connection_id",
                        params![
                            mount_id.0.as_str(),
                            journal.delta.source_connection_id.as_str(),
                            new.projection_id.as_str(),
                            new.logical_path.as_str(),
                            journal.delta.target_generation_id.as_str(),
                            to_json(new)?,
                            journal.delta.delta_id.as_str(),
                            i64::try_from(entry_index).map_err(|_| StoreError::InvalidState(
                                "generation entry index is too large".to_string()
                            ))?,
                            completed_at,
                        ],
                    )?;
                    if changed != 1 {
                        return Err(StoreError::InvalidState(
                            "generation apply crossed a source-owned projection".to_string(),
                        ));
                    }
                }
                GenerationApplyOutcome::Merged => {
                    let new = entry.new.as_ref().ok_or_else(|| {
                        StoreError::InvalidState("merged outcome has no new identity".to_string())
                    })?;
                    let changed = transaction.execute(
                        "INSERT INTO generation_paths (
                            mount_id, source_connection_id, projection_id, logical_path,
                            local_logical_path, base_generation_id,
                            base_identity_json, base_payload_delta_id, base_payload_entry_index,
                            conflict_payload_delta_id, conflict_payload_entry_index,
                            state, incoming_identity_json, updated_at
                         ) VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7, ?8,
                                   NULL, NULL, 'dirty', NULL, ?9)
                         ON CONFLICT(mount_id, projection_id) DO UPDATE SET
                            logical_path = excluded.logical_path,
                            local_logical_path = excluded.local_logical_path,
                            base_generation_id = excluded.base_generation_id,
                            base_identity_json = excluded.base_identity_json,
                            base_payload_delta_id = excluded.base_payload_delta_id,
                            base_payload_entry_index = excluded.base_payload_entry_index,
                            conflict_payload_delta_id = NULL,
                            conflict_payload_entry_index = NULL,
                            state = 'dirty', incoming_identity_json = NULL,
                            updated_at = excluded.updated_at
                         WHERE generation_paths.source_connection_id = excluded.source_connection_id",
                        params![
                            mount_id.0.as_str(),
                            journal.delta.source_connection_id.as_str(),
                            new.projection_id.as_str(),
                            new.logical_path.as_str(),
                            journal.delta.target_generation_id.as_str(),
                            to_json(new)?,
                            journal.delta.delta_id.as_str(),
                            i64::try_from(entry_index).map_err(|_| StoreError::InvalidState(
                                "generation entry index is too large".to_string()
                            ))?,
                            completed_at,
                        ],
                    )?;
                    if changed != 1 {
                        return Err(StoreError::InvalidState(
                            "generation apply crossed a source-owned projection".to_string(),
                        ));
                    }
                }
                GenerationApplyOutcome::Deleted => {
                    let old = entry.old.as_ref().ok_or_else(|| {
                        StoreError::InvalidState("deleted outcome has no old identity".to_string())
                    })?;
                    if entry.new.is_some() {
                        return Err(StoreError::InvalidState(
                            "deleted outcome names a non-deletion entry".to_string(),
                        ));
                    }
                    let changed = transaction.execute(
                        "DELETE FROM generation_paths
                         WHERE mount_id = ?1 AND source_connection_id = ?2 AND projection_id = ?3",
                        params![
                            mount_id.0.as_str(),
                            journal.delta.source_connection_id.as_str(),
                            old.projection_id.as_str()
                        ],
                    )?;
                    if changed != 1 {
                        return Err(StoreError::InvalidState(
                            "generation deletion crossed a source-owned projection".to_string(),
                        ));
                    }
                }
                GenerationApplyOutcome::Conflict {
                    local_sha256: _,
                    incoming_identity,
                }
                | GenerationApplyOutcome::ConflictOverQuota {
                    local_sha256: _,
                    incoming_identity,
                } => {
                    let projection_id = entry
                        .projection_id()
                        .expect("validated delta entry has projection identity");
                    let existing = select_generation_path(
                        &transaction,
                        &mount_id,
                        &journal.delta.source_connection_id,
                        projection_id,
                    )?;
                    let logical_path = entry
                        .new
                        .as_ref()
                        .or(entry.old.as_ref())
                        .expect("validated delta entry has identity")
                        .logical_path
                        .as_str();
                    let local_logical_path = existing
                        .as_ref()
                        .map_or(logical_path, |path| path.local_logical_path.as_str());
                    let base_generation_id = existing
                        .as_ref()
                        .map_or(&journal.delta.base_generation_id, |path| {
                            &path.base_generation_id
                        });
                    let base_identity = existing
                        .as_ref()
                        .and_then(|path| path.base_identity.as_ref())
                        .or(entry.old.as_ref());
                    let base_payload_delta_id = existing
                        .as_ref()
                        .and_then(|path| path.base_payload_delta_id.as_deref());
                    let base_payload_entry_index = existing
                        .as_ref()
                        .and_then(|path| path.base_payload_entry_index)
                        .map(i64::try_from)
                        .transpose()
                        .map_err(|_| {
                            StoreError::InvalidState(
                                "generation base payload index is too large".to_string(),
                            )
                        })?;
                    let retains_conflict_payload =
                        matches!(outcome, GenerationApplyOutcome::Conflict { .. })
                            && entry.new.is_some();
                    let changed = transaction.execute(
                        "INSERT INTO generation_paths (
                            mount_id, source_connection_id, projection_id, logical_path,
                            local_logical_path, base_generation_id, base_identity_json,
                            base_payload_delta_id, base_payload_entry_index,
                            conflict_payload_delta_id, conflict_payload_entry_index,
                            state, incoming_identity_json, updated_at
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                                   ?11, 'conflicted', ?12, ?13)
                         ON CONFLICT(mount_id, projection_id) DO UPDATE SET
                            logical_path = excluded.logical_path,
                            local_logical_path = excluded.local_logical_path,
                            base_payload_delta_id = excluded.base_payload_delta_id,
                            base_payload_entry_index = excluded.base_payload_entry_index,
                            conflict_payload_delta_id = excluded.conflict_payload_delta_id,
                            conflict_payload_entry_index = excluded.conflict_payload_entry_index,
                            state = 'conflicted',
                            incoming_identity_json = excluded.incoming_identity_json,
                            updated_at = excluded.updated_at
                         WHERE generation_paths.source_connection_id = excluded.source_connection_id",
                        params![
                            mount_id.0.as_str(),
                            journal.delta.source_connection_id.as_str(),
                            projection_id.as_str(),
                            logical_path,
                            local_logical_path,
                            base_generation_id.as_str(),
                            base_identity.map(to_json).transpose()?,
                            base_payload_delta_id,
                            base_payload_entry_index,
                            retains_conflict_payload.then_some(journal.delta.delta_id.as_str()),
                            retains_conflict_payload.then_some(
                                i64::try_from(entry_index).map_err(
                                    |_| StoreError::InvalidState(
                                        "generation conflict payload index is too large"
                                            .to_string()
                                    )
                                )?
                            ),
                            incoming_identity.as_ref().map(to_json).transpose()?,
                            completed_at,
                        ],
                    )?;
                    if changed != 1 {
                        return Err(StoreError::InvalidState(
                            "generation conflict crossed a source-owned projection".to_string(),
                        ));
                    }
                }
            }
        }
        let changed = transaction.execute(
            "UPDATE observed_generations
             SET generation_id = ?2, inventory_sha256 = ?3,
                 workspace_layout_version = ?4, workspace_layout_digest = ?5,
                 last_receipt_sha256 = ?6, updated_at = ?7
             WHERE mount_id = ?1 AND source_connection_id = ?8 AND generation_id = ?9",
            params![
                mount_id.0.as_str(),
                journal.delta.target_generation_id.as_str(),
                journal.delta.target_inventory_sha256.as_str(),
                journal.delta.workspace_layout_version,
                journal.delta.workspace_layout_digest.as_str(),
                journal.receipt_sha256.as_str(),
                completed_at,
                journal.delta.source_connection_id.as_str(),
                journal.delta.base_generation_id.as_str(),
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidState(format!(
                "mount `{}` observed generation changed during apply",
                mount_id.0
            )));
        }
        transaction.execute(
            "UPDATE generation_apply_journals
             SET status = 'completed', active = 0, updated_at = ?2, completed_at = ?2
             WHERE delta_id = ?1 AND active = 1",
            params![delta_id, completed_at],
        )?;
        let completed = generation_apply_from_connection(&transaction, delta_id)?
            .expect("completed generation journal remains present");
        transaction.commit()?;
        Ok(completed.journal)
    }
}

impl HostedWorkspaceRepository for SqliteStateStore {
    fn begin_hosted_workspace_transition(
        &mut self,
        prepared: PreparedHostedWorkspaceTransition,
    ) -> StoreResult<PendingHostedWorkspaceTransition> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let identity = prepared.identity().clone();
        let cleanup_pending: bool = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM hosted_workspace_pending_cleanups
                 WHERE api_origin = ?1 AND profile_id = ?2
             )",
            params![
                identity.api_origin().as_str(),
                identity.profile_id().as_str()
            ],
            |row| row.get(0),
        )?;
        if cleanup_pending {
            return Err(StoreError::InvalidState(
                "hosted workspace attachment still has pending relocation cleanup".to_string(),
            ));
        }
        let attachment = hosted_workspace_attachment_from_connection(&transaction, &identity)?;
        let mappings = hosted_workspace_mappings_from_connection(&transaction, &identity)?;
        let mut reserved = BTreeSet::new();
        {
            let mut statement = transaction.prepare("SELECT mount_id FROM mounts")?;
            reserved.extend(
                statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .map(MountId::new),
            );
        }
        {
            let mut statement = transaction.prepare(
                "SELECT local_mount_id, api_origin, profile_id
                 FROM hosted_workspace_mount_mappings",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            reserved.extend(
                rows.into_iter()
                    .filter(|(_, origin, profile_id)| {
                        origin != identity.api_origin().as_str()
                            || profile_id != identity.profile_id().as_str()
                    })
                    .map(|(mount_id, _, _)| MountId::new(mount_id)),
            );
        }
        {
            let mut statement = transaction.prepare(
                "SELECT m.local_mount_id, t.api_origin, t.profile_id
                 FROM hosted_workspace_pending_mounts m
                 JOIN hosted_workspace_pending_transitions t
                   ON t.transition_id = m.transition_id",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            reserved.extend(
                rows.into_iter()
                    .filter(|(_, origin, profile_id)| {
                        origin != identity.api_origin().as_str()
                            || profile_id != identity.profile_id().as_str()
                    })
                    .map(|(mount_id, _, _)| MountId::new(mount_id)),
            );
        }
        let pending =
            prepare_pending_transition(attachment.as_ref(), &mappings, &reserved, prepared)?;
        if let Some(existing) = pending_hosted_workspace_from_connection(&transaction, &identity)? {
            if existing == pending {
                transaction.commit()?;
                return Ok(existing);
            }
            return Err(StoreError::InvalidState(
                "hosted workspace attachment already has a different pending transition"
                    .to_string(),
            ));
        }
        let transition_id_in_use: bool = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM hosted_workspace_pending_transitions WHERE transition_id = ?1
             )",
            params![pending.prepared().transition_id()],
            |row| row.get(0),
        )?;
        if transition_id_in_use {
            return Err(StoreError::InvalidState(
                "hosted workspace transition ID is already in use".to_string(),
            ));
        }
        let exact_root_in_use: bool = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM hosted_workspace_attachments
                 WHERE root = ?1 AND (api_origin != ?2 OR profile_id != ?3)
                UNION ALL
                SELECT 1 FROM hosted_workspace_pending_transitions
                 WHERE target_root = ?1 AND (api_origin != ?2 OR profile_id != ?3)
                UNION ALL
                SELECT 1 FROM hosted_workspace_pending_cleanups
                 WHERE root = ?1 AND (api_origin != ?2 OR profile_id != ?3)
             )",
            params![
                path_to_text(pending.prepared().target_root()),
                identity.api_origin().as_str(),
                identity.profile_id().as_str(),
            ],
            |row| row.get(0),
        )?;
        if exact_root_in_use {
            return Err(StoreError::InvalidState(
                "hosted workspace root is already reserved by another attachment".to_string(),
            ));
        }
        transaction.execute(
            "INSERT INTO hosted_workspace_pending_transitions (
                transition_id, api_origin, profile_id, credential_ref, target_root,
                transition_kind, profile_revision, layout_version, layout_digest,
                base_profile_revision, base_layout_digest, base_root, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                pending.prepared().transition_id(),
                identity.api_origin().as_str(),
                identity.profile_id().as_str(),
                pending.prepared().credential_ref().as_str(),
                path_to_text(pending.prepared().target_root()),
                pending.kind().as_str(),
                pending.prepared().profile_revision() as i64,
                pending.prepared().layout_version() as i64,
                pending.prepared().layout_digest().as_str(),
                pending.base_profile_revision().map(|value| value as i64),
                pending.base_layout_digest().map(LayoutDigest::as_str),
                pending.base_root().map(path_to_text),
                pending.prepared().created_at(),
            ],
        )?;
        for mapping in pending.prepared().mounts() {
            transaction.execute(
                "INSERT INTO hosted_workspace_pending_mounts (
                    transition_id, portable_mount_id, local_mount_id, mount_target,
                    target_collision_key, first_seen_revision
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    pending.prepared().transition_id(),
                    mapping.portable_mount_id().as_str(),
                    mapping.local_mount_id().as_str(),
                    mapping.mount_target().as_str(),
                    mapping.mount_target().collision_key(),
                    mapping.first_seen_revision() as i64,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(pending)
    }

    fn get_hosted_workspace_attachment(
        &self,
        identity: &HostedWorkspaceIdentity,
    ) -> StoreResult<Option<HostedWorkspaceAttachment>> {
        let connection = self.connection()?;
        hosted_workspace_attachment_from_connection(&connection, identity)
    }

    fn list_hosted_workspace_attachments(&self) -> StoreResult<Vec<HostedWorkspaceAttachment>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT api_origin, profile_id, credential_ref, root, profile_revision,
                    layout_version, layout_digest, updated_at
             FROM hosted_workspace_attachments ORDER BY api_origin, profile_id",
        )?;
        let rows = statement
            .query_map([], hosted_workspace_attachment_row)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(hosted_workspace_attachment_from_row)
            .collect()
    }

    fn list_hosted_workspace_mount_mappings(
        &self,
        identity: &HostedWorkspaceIdentity,
    ) -> StoreResult<Vec<HostedWorkspaceMountMapping>> {
        let connection = self.connection()?;
        hosted_workspace_mappings_from_connection(&connection, identity)
    }

    fn get_pending_hosted_workspace_transition(
        &self,
        identity: &HostedWorkspaceIdentity,
    ) -> StoreResult<Option<PendingHostedWorkspaceTransition>> {
        let connection = self.connection()?;
        pending_hosted_workspace_from_connection(&connection, identity)
    }

    fn list_pending_hosted_workspace_transitions(
        &self,
    ) -> StoreResult<Vec<PendingHostedWorkspaceTransition>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT api_origin, profile_id
             FROM hosted_workspace_pending_transitions ORDER BY api_origin, profile_id",
        )?;
        let identities = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        identities
            .into_iter()
            .map(|(origin, profile_id)| {
                let identity = hosted_workspace_identity(origin, profile_id)?;
                pending_hosted_workspace_from_connection(&connection, &identity)?.ok_or_else(|| {
                    StoreError::InvalidState(
                        "hosted workspace pending transition disappeared while listing".to_string(),
                    )
                })
            })
            .collect()
    }

    fn get_pending_hosted_workspace_cleanup(
        &self,
        identity: &HostedWorkspaceIdentity,
    ) -> StoreResult<Option<PendingHostedWorkspaceCleanup>> {
        let connection = self.connection()?;
        pending_hosted_workspace_cleanup_from_connection(&connection, identity)
    }

    fn list_pending_hosted_workspace_cleanups(
        &self,
    ) -> StoreResult<Vec<PendingHostedWorkspaceCleanup>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT api_origin, profile_id
             FROM hosted_workspace_pending_cleanups ORDER BY api_origin, profile_id",
        )?;
        let identities = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        identities
            .into_iter()
            .map(|(origin, profile_id)| {
                let identity = hosted_workspace_identity(origin, profile_id)?;
                pending_hosted_workspace_cleanup_from_connection(&connection, &identity)?
                    .ok_or_else(|| {
                        StoreError::InvalidState(
                            "hosted workspace cleanup disappeared while listing".to_string(),
                        )
                    })
            })
            .collect()
    }

    fn commit_hosted_workspace_transition(
        &mut self,
        transition_id: &str,
        committed_at: &str,
    ) -> StoreResult<HostedWorkspaceAttachment> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let identity_row = transaction
            .query_row(
                "SELECT api_origin, profile_id
                 FROM hosted_workspace_pending_transitions WHERE transition_id = ?1",
                params![transition_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::InvalidState("hosted workspace transition was not found".to_string())
            })?;
        let identity = hosted_workspace_identity(identity_row.0, identity_row.1)?;
        let pending = pending_hosted_workspace_from_connection(&transaction, &identity)?
            .ok_or_else(|| {
                StoreError::InvalidState("hosted workspace transition was not found".to_string())
            })?;
        if pending.prepared().transition_id() != transition_id {
            return Err(StoreError::InvalidState(
                "hosted workspace transition identity mismatch".to_string(),
            ));
        }
        let current = hosted_workspace_attachment_from_connection(&transaction, &identity)?;
        let attachment = committed_attachment(&pending, current.as_ref(), committed_at)?;
        let cleanup = relocation_cleanup(&pending, current.as_ref())?;
        let existing_cleanup: bool = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM hosted_workspace_pending_cleanups
                 WHERE api_origin = ?1 AND profile_id = ?2
             )",
            params![
                identity.api_origin().as_str(),
                identity.profile_id().as_str()
            ],
            |row| row.get(0),
        )?;
        if existing_cleanup {
            return Err(StoreError::InvalidState(
                "hosted workspace attachment already has pending relocation cleanup".to_string(),
            ));
        }
        ensure_hosted_transition_mount_ids_available(&transaction, &pending)?;
        transaction.execute(
            "INSERT INTO hosted_workspace_attachments (
                api_origin, profile_id, credential_ref, root, profile_revision,
                layout_version, layout_digest, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(api_origin, profile_id) DO UPDATE SET
                credential_ref = excluded.credential_ref,
                root = excluded.root,
                profile_revision = excluded.profile_revision,
                layout_version = excluded.layout_version,
                layout_digest = excluded.layout_digest,
                updated_at = excluded.updated_at",
            params![
                identity.api_origin().as_str(),
                identity.profile_id().as_str(),
                attachment.credential_ref().as_str(),
                path_to_text(attachment.root()),
                attachment.profile_revision() as i64,
                attachment.layout_version() as i64,
                attachment.layout_digest().as_str(),
                attachment.updated_at(),
            ],
        )?;
        transaction.execute(
            "UPDATE hosted_workspace_mount_mappings SET active = 0
             WHERE api_origin = ?1 AND profile_id = ?2",
            params![
                identity.api_origin().as_str(),
                identity.profile_id().as_str()
            ],
        )?;
        for mapping in pending.prepared().mounts() {
            transaction.execute(
                "INSERT INTO hosted_workspace_mount_mappings (
                    api_origin, profile_id, portable_mount_id, local_mount_id,
                    mount_target, target_collision_key, active,
                    first_seen_revision, last_seen_revision
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8)
                 ON CONFLICT(api_origin, profile_id, portable_mount_id) DO UPDATE SET
                    local_mount_id = excluded.local_mount_id,
                    mount_target = excluded.mount_target,
                    target_collision_key = excluded.target_collision_key,
                    active = 1,
                    first_seen_revision = excluded.first_seen_revision,
                    last_seen_revision = excluded.last_seen_revision",
                params![
                    identity.api_origin().as_str(),
                    identity.profile_id().as_str(),
                    mapping.portable_mount_id().as_str(),
                    mapping.local_mount_id().as_str(),
                    mapping.mount_target().as_str(),
                    mapping.mount_target().collision_key(),
                    mapping.first_seen_revision() as i64,
                    mapping.last_seen_revision() as i64,
                ],
            )?;
        }
        if let Some(cleanup) = cleanup {
            transaction.execute(
                "INSERT INTO hosted_workspace_pending_cleanups (
                    cleanup_id, api_origin, profile_id, credential_ref, root,
                    profile_revision, layout_version, layout_digest, created_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    cleanup.cleanup_id(),
                    cleanup.identity().api_origin().as_str(),
                    cleanup.identity().profile_id().as_str(),
                    cleanup.credential_ref().as_str(),
                    path_to_text(cleanup.root()),
                    cleanup.profile_revision() as i64,
                    cleanup.layout_version() as i64,
                    cleanup.layout_digest().as_str(),
                    cleanup.created_at(),
                ],
            )?;
        }
        let deleted = transaction.execute(
            "DELETE FROM hosted_workspace_pending_transitions WHERE transition_id = ?1",
            params![transition_id],
        )?;
        if deleted != 1 {
            return Err(StoreError::InvalidState(
                "hosted workspace transition disappeared during commit".to_string(),
            ));
        }
        transaction.commit()?;
        Ok(attachment)
    }

    fn cancel_hosted_workspace_transition(&mut self, transition_id: &str) -> StoreResult<()> {
        let connection = self.connection()?;
        let deleted = connection.execute(
            "DELETE FROM hosted_workspace_pending_transitions WHERE transition_id = ?1",
            params![transition_id],
        )?;
        if deleted != 1 {
            return Err(StoreError::InvalidState(
                "hosted workspace transition was not found".to_string(),
            ));
        }
        Ok(())
    }

    fn complete_hosted_workspace_cleanup(&mut self, cleanup_id: &str) -> StoreResult<()> {
        let connection = self.connection()?;
        let deleted = connection.execute(
            "DELETE FROM hosted_workspace_pending_cleanups WHERE cleanup_id = ?1",
            params![cleanup_id],
        )?;
        if deleted != 1 {
            return Err(StoreError::InvalidState(
                "hosted workspace relocation cleanup was not found".to_string(),
            ));
        }
        Ok(())
    }
}

type PendingHostedWorkspaceCleanupRow = (
    String,
    String,
    String,
    String,
    String,
    i64,
    i64,
    String,
    String,
);

fn pending_hosted_workspace_cleanup_from_connection(
    connection: &Connection,
    identity: &HostedWorkspaceIdentity,
) -> StoreResult<Option<PendingHostedWorkspaceCleanup>> {
    let row = connection
        .query_row(
            "SELECT cleanup_id, api_origin, profile_id, credential_ref, root,
                    profile_revision, layout_version, layout_digest, created_at
             FROM hosted_workspace_pending_cleanups
             WHERE api_origin = ?1 AND profile_id = ?2",
            params![
                identity.api_origin().as_str(),
                identity.profile_id().as_str()
            ],
            |row| {
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
                ))
            },
        )
        .optional()?;
    let Some(row): Option<PendingHostedWorkspaceCleanupRow> = row else {
        return Ok(None);
    };
    let parsed_identity = hosted_workspace_identity(row.1, row.2)?;
    if &parsed_identity != identity {
        return Err(StoreError::InvalidState(
            "hosted workspace cleanup identity is inconsistent".to_string(),
        ));
    }
    PendingHostedWorkspaceCleanup::new(
        row.0,
        parsed_identity,
        HostedWorkspaceCredentialRef::new(row.3)
            .map_err(|error| StoreError::InvalidState(error.to_string()))?,
        PathBuf::from(row.4),
        u64::try_from(row.5).map_err(|_| {
            StoreError::InvalidState(
                "hosted workspace cleanup profile revision is invalid".to_string(),
            )
        })?,
        u16::try_from(row.6).map_err(|_| {
            StoreError::InvalidState(
                "hosted workspace cleanup layout version is invalid".to_string(),
            )
        })?,
        LayoutDigest::new(row.7).map_err(|error| StoreError::InvalidState(error.to_string()))?,
        row.8,
    )
    .map(Some)
}

type HostedWorkspaceAttachmentRow = (String, String, String, String, i64, i64, String, String);

fn hosted_workspace_attachment_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<HostedWorkspaceAttachmentRow> {
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

fn hosted_workspace_identity(
    origin: String,
    profile_id: String,
) -> StoreResult<HostedWorkspaceIdentity> {
    Ok(HostedWorkspaceIdentity::new(
        CanonicalApiOrigin::new(origin)
            .map_err(|error| StoreError::InvalidState(error.to_string()))?,
        WorkspaceProfileId::new(profile_id)
            .map_err(|error| StoreError::InvalidState(error.to_string()))?,
    ))
}

fn hosted_workspace_attachment_from_row(
    row: HostedWorkspaceAttachmentRow,
) -> StoreResult<HostedWorkspaceAttachment> {
    HostedWorkspaceAttachment::new(
        hosted_workspace_identity(row.0, row.1)?,
        HostedWorkspaceCredentialRef::new(row.2)
            .map_err(|error| StoreError::InvalidState(error.to_string()))?,
        PathBuf::from(row.3),
        u64::try_from(row.4).map_err(|_| {
            StoreError::InvalidState("hosted workspace profile revision is invalid".to_string())
        })?,
        u16::try_from(row.5).map_err(|_| {
            StoreError::InvalidState("hosted workspace layout version is invalid".to_string())
        })?,
        LayoutDigest::new(row.6).map_err(|error| StoreError::InvalidState(error.to_string()))?,
        row.7,
    )
}

fn hosted_workspace_attachment_from_connection(
    connection: &Connection,
    identity: &HostedWorkspaceIdentity,
) -> StoreResult<Option<HostedWorkspaceAttachment>> {
    connection
        .query_row(
            "SELECT api_origin, profile_id, credential_ref, root, profile_revision,
                    layout_version, layout_digest, updated_at
             FROM hosted_workspace_attachments
             WHERE api_origin = ?1 AND profile_id = ?2",
            params![
                identity.api_origin().as_str(),
                identity.profile_id().as_str()
            ],
            hosted_workspace_attachment_row,
        )
        .optional()?
        .map(hosted_workspace_attachment_from_row)
        .transpose()
}

fn hosted_workspace_mappings_from_connection(
    connection: &Connection,
    identity: &HostedWorkspaceIdentity,
) -> StoreResult<Vec<HostedWorkspaceMountMapping>> {
    let mut statement = connection.prepare(
        "SELECT portable_mount_id, local_mount_id, mount_target, active,
                first_seen_revision, last_seen_revision, target_collision_key
         FROM hosted_workspace_mount_mappings
         WHERE api_origin = ?1 AND profile_id = ?2
         ORDER BY portable_mount_id",
    )?;
    let rows = statement
        .query_map(
            params![
                identity.api_origin().as_str(),
                identity.profile_id().as_str()
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, String>(6)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|row| {
            let portable = PortableMountId::new(row.0)
                .map_err(|error| StoreError::InvalidState(error.to_string()))?;
            let target = MountTarget::new(row.2)
                .map_err(|error| StoreError::InvalidState(error.to_string()))?;
            if target.collision_key() != row.6 {
                return Err(StoreError::InvalidState(
                    "hosted workspace mount target collision key is inconsistent".to_string(),
                ));
            }
            HostedWorkspaceMountMapping::persisted(
                portable,
                MountId::new(row.1),
                target,
                match row.3 {
                    0 => false,
                    1 => true,
                    _ => {
                        return Err(StoreError::InvalidState(
                            "hosted workspace mount active flag is invalid".to_string(),
                        ));
                    }
                },
                u64::try_from(row.4).map_err(|_| {
                    StoreError::InvalidState(
                        "hosted workspace mount first revision is invalid".to_string(),
                    )
                })?,
                u64::try_from(row.5).map_err(|_| {
                    StoreError::InvalidState(
                        "hosted workspace mount last revision is invalid".to_string(),
                    )
                })?,
            )
        })
        .collect()
}

type PendingHostedWorkspaceRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
    i64,
    String,
    Option<i64>,
    Option<String>,
    Option<String>,
    String,
);

fn pending_hosted_workspace_from_connection(
    connection: &Connection,
    identity: &HostedWorkspaceIdentity,
) -> StoreResult<Option<PendingHostedWorkspaceTransition>> {
    let row = connection
        .query_row(
            "SELECT transition_id, api_origin, profile_id, credential_ref, target_root,
                    transition_kind, profile_revision, layout_version, layout_digest,
                    base_profile_revision, base_layout_digest, base_root, created_at
             FROM hosted_workspace_pending_transitions
             WHERE api_origin = ?1 AND profile_id = ?2",
            params![
                identity.api_origin().as_str(),
                identity.profile_id().as_str()
            ],
            |row| {
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
                ))
            },
        )
        .optional()?;
    let Some(row): Option<PendingHostedWorkspaceRow> = row else {
        return Ok(None);
    };
    let parsed_identity = hosted_workspace_identity(row.1, row.2)?;
    if &parsed_identity != identity {
        return Err(StoreError::InvalidState(
            "hosted workspace pending identity is inconsistent".to_string(),
        ));
    }
    let revision = u64::try_from(row.6).map_err(|_| {
        StoreError::InvalidState("hosted workspace pending revision is invalid".to_string())
    })?;
    let mut statement = connection.prepare(
        "SELECT portable_mount_id, local_mount_id, mount_target,
                first_seen_revision, target_collision_key
         FROM hosted_workspace_pending_mounts
         WHERE transition_id = ?1 ORDER BY portable_mount_id",
    )?;
    let mount_rows = statement
        .query_map(params![row.0.as_str()], |mount_row| {
            Ok((
                mount_row.get::<_, String>(0)?,
                mount_row.get::<_, String>(1)?,
                mount_row.get::<_, String>(2)?,
                mount_row.get::<_, i64>(3)?,
                mount_row.get::<_, String>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mounts = mount_rows
        .into_iter()
        .map(|mount_row| {
            let target = MountTarget::new(mount_row.2)
                .map_err(|error| StoreError::InvalidState(error.to_string()))?;
            if target.collision_key() != mount_row.4 {
                return Err(StoreError::InvalidState(
                    "hosted workspace pending target collision key is inconsistent".to_string(),
                ));
            }
            let first_seen_revision = u64::try_from(mount_row.3).map_err(|_| {
                StoreError::InvalidState(
                    "hosted workspace pending first revision is invalid".to_string(),
                )
            })?;
            HostedWorkspaceMountMapping::proposal(
                PortableMountId::new(mount_row.0)
                    .map_err(|error| StoreError::InvalidState(error.to_string()))?,
                MountId::new(mount_row.1),
                target,
                revision,
            )
            .map(|mapping| mapping.with_history(first_seen_revision))
        })
        .collect::<StoreResult<Vec<_>>>()?;
    let prepared = PreparedHostedWorkspaceTransition::new(
        row.0,
        parsed_identity,
        HostedWorkspaceCredentialRef::new(row.3)
            .map_err(|error| StoreError::InvalidState(error.to_string()))?,
        PathBuf::from(row.4),
        revision,
        u16::try_from(row.7).map_err(|_| {
            StoreError::InvalidState(
                "hosted workspace pending layout version is invalid".to_string(),
            )
        })?,
        LayoutDigest::new(row.8).map_err(|error| StoreError::InvalidState(error.to_string()))?,
        mounts,
        row.12,
    )?;
    PendingHostedWorkspaceTransition::new(
        prepared,
        HostedWorkspaceTransitionKind::parse(&row.5)?,
        row.9
            .map(|value| {
                u64::try_from(value).map_err(|_| {
                    StoreError::InvalidState(
                        "hosted workspace base revision is invalid".to_string(),
                    )
                })
            })
            .transpose()?,
        row.10
            .map(LayoutDigest::new)
            .transpose()
            .map_err(|error| StoreError::InvalidState(error.to_string()))?,
        row.11.map(PathBuf::from),
    )
    .map(Some)
}

fn ensure_hosted_transition_mount_ids_available(
    connection: &Connection,
    pending: &PendingHostedWorkspaceTransition,
) -> StoreResult<()> {
    for mapping in pending.prepared().mounts() {
        let reserved: bool = connection.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM mounts WHERE mount_id = ?1
                UNION ALL
                SELECT 1 FROM hosted_workspace_mount_mappings
                 WHERE local_mount_id = ?1
                   AND (api_origin != ?2 OR profile_id != ?3)
                UNION ALL
                SELECT 1 FROM hosted_workspace_pending_mounts m
                JOIN hosted_workspace_pending_transitions t
                  ON t.transition_id = m.transition_id
                 WHERE m.local_mount_id = ?1 AND t.transition_id != ?4
             )",
            params![
                mapping.local_mount_id().as_str(),
                pending.prepared().identity().api_origin().as_str(),
                pending.prepared().identity().profile_id().as_str(),
                pending.prepared().transition_id(),
            ],
            |row| row.get(0),
        )?;
        if reserved {
            return Err(StoreError::InvalidState(format!(
                "local mount `{}` became reserved outside this hosted profile",
                mapping.local_mount_id().as_str()
            )));
        }
    }
    Ok(())
}

fn ensure_connector_mount_id_available(
    connection: &Connection,
    mount_id: &MountId,
) -> StoreResult<()> {
    let reserved: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM hosted_workspace_mount_mappings WHERE local_mount_id = ?1
            UNION ALL
            SELECT 1 FROM hosted_workspace_pending_mounts WHERE local_mount_id = ?1
         )",
        params![mount_id.as_str()],
        |row| row.get(0),
    )?;
    if reserved {
        return Err(StoreError::InvalidState(format!(
            "mount `{}` is reserved by a hosted workspace",
            mount_id.as_str()
        )));
    }
    Ok(())
}

impl MountRepository for SqliteStateStore {
    fn save_mount(&mut self, mount: MountConfig) -> StoreResult<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_connector_mount_id_available(&transaction, &mount.mount_id)?;
        let existing = transaction
            .query_row(
                "SELECT mount_id, connector, root, remote_root_id, read_only, projection_json, connection_id, settings_json
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
                        row.get::<_, String>(7)?,
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
            "INSERT INTO mounts (mount_id, connector, root, remote_root_id, read_only, projection_json, connection_id, settings_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(mount_id) DO UPDATE SET
                connector = excluded.connector,
                root = excluded.root,
                remote_root_id = excluded.remote_root_id,
                read_only = excluded.read_only,
                projection_json = excluded.projection_json,
                connection_id = excluded.connection_id,
                settings_json = excluded.settings_json",
            params![
                &mount.mount_id.0,
                &mount.connector,
                path_to_text(&mount.root),
                mount.remote_root_id.as_ref().map(|remote_id| remote_id.0.as_str()),
                bool_to_int(mount.read_only),
                to_json(&mount.projection)?,
                mount.connection_id.as_ref().map(|connection_id| connection_id.0.as_str()),
                mount.settings_json.as_str(),
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn get_mount(&self, mount_id: &MountId) -> StoreResult<Option<MountConfig>> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT mount_id, connector, root, remote_root_id, read_only, projection_json, connection_id, settings_json
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
                        row.get::<_, String>(7)?,
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
            "SELECT mount_id, connector, root, remote_root_id, read_only, projection_json, connection_id, settings_json
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
                row.get::<_, String>(7)?,
            ))
        })?;

        rows.map(|row| mount_from_row(row?)).collect()
    }
}

impl WorkspaceBindingRepository for SqliteStateStore {
    fn save_workspace_binding(&mut self, record: WorkspaceBindingRecord) -> StoreResult<()> {
        if record.binding.workspace_id().is_some() {
            return Err(StoreError::InvalidState(
                "layout-1 workspace bindings require an atomic host-binding commit".to_string(),
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let mount_exists = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM mounts WHERE mount_id = ?1)",
            params![record.mount_id.0.as_str()],
            |row| row.get::<_, bool>(0),
        )?;
        if !mount_exists {
            return Err(StoreError::MountMissing(record.mount_id));
        }
        let existing = transaction
            .query_row(
                "SELECT workspace_id, binding_json, target_collision_key
                 FROM workspace_bindings
                 WHERE mount_id = ?1",
                params![record.mount_id.0.as_str()],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .map(workspace_binding_from_persisted_row)
            .transpose()?;
        if let Some(existing) = existing {
            if existing == record.binding {
                return Ok(());
            }
            return Err(StoreError::WorkspaceBindingTargetImmutable {
                mount_id: record.mount_id,
                existing_target: existing.mount_target().as_str().to_string(),
                requested_target: record.binding.mount_target().as_str().to_string(),
            });
        }
        let collision_key = record.binding.collision_key();
        let collision = {
            let mut statement = transaction.prepare(
                "SELECT m.mount_id, m.root, b.binding_json, b.target_collision_key
                 FROM mounts m
                 LEFT JOIN workspace_bindings b ON b.mount_id = m.mount_id
                 WHERE m.mount_id <> ?1
                 ORDER BY m.mount_id",
            )?;
            let rows = statement.query_map(params![record.mount_id.0.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    PathBuf::from(row.get::<_, String>(1)?),
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?;
            let mut collision = None;
            for row in rows {
                let (mount_id, root, binding_json, stored_collision_key) = row?;
                let existing_collision_key = match (binding_json, stored_collision_key) {
                    (Some(binding_json), Some(stored_collision_key)) => Some(
                        workspace_binding_from_row((binding_json, stored_collision_key))?
                            .collision_key(),
                    ),
                    (None, None) => legacy_mount_collision_key(&root),
                    _ => {
                        return Err(StoreError::InvalidState(
                            "workspace binding row is partially present".to_string(),
                        ));
                    }
                };
                if existing_collision_key.as_deref() == Some(collision_key.as_str()) {
                    collision = Some(mount_id);
                    break;
                }
            }
            collision
        };
        if let Some(existing_mount_id) = collision {
            return Err(StoreError::WorkspaceMountTargetCollision {
                target: record.binding.mount_target().as_str().to_string(),
                existing_mount_id: MountId(existing_mount_id),
            });
        }
        transaction.execute(
            "INSERT INTO workspace_bindings (
                mount_id, workspace_id, binding_json, target_collision_key
             ) VALUES (?1, NULL, ?2, ?3)",
            params![
                record.mount_id.0.as_str(),
                to_json(&record.binding)?,
                collision_key,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn get_workspace_binding(&self, mount_id: &MountId) -> StoreResult<Option<WorkspaceBinding>> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT workspace_id, binding_json, target_collision_key
                 FROM workspace_bindings
                 WHERE mount_id = ?1",
                params![mount_id.0.as_str()],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .map(workspace_binding_from_persisted_row)
            .transpose()
    }

    fn load_workspace_bindings(&self) -> StoreResult<Vec<WorkspaceBindingRecord>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT mount_id, workspace_id, binding_json, target_collision_key
             FROM workspace_bindings
             ORDER BY mount_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (mount_id, workspace_id, binding_json, collision_key) = row?;
            Ok(WorkspaceBindingRecord::new(
                MountId(mount_id),
                workspace_binding_from_persisted_row((workspace_id, binding_json, collision_key))?,
            ))
        })
        .collect()
    }

    fn commit_workspace_binding(
        &mut self,
        host_binding: WorkspaceHostBinding,
        record: WorkspaceBindingRecord,
    ) -> StoreResult<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mount = mount_from_connection(&transaction, &record.mount_id)?
            .ok_or_else(|| StoreError::MountMissing(record.mount_id.clone()))?;
        commit_workspace_binding_in_transaction(&transaction, &mount, &host_binding, &record)?;
        transaction.commit()?;
        Ok(())
    }

    fn save_mount_with_workspace_binding(
        &mut self,
        mount: MountConfig,
        host_binding: WorkspaceHostBinding,
        record: WorkspaceBindingRecord,
    ) -> StoreResult<()> {
        if mount.mount_id != record.mount_id {
            return Err(StoreError::InvalidState(
                "mount and workspace binding identities do not match".to_string(),
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = mount_from_connection(&transaction, &mount.mount_id)?;
        validate_workspace_binding_commit(&mount, &host_binding, &record)?;
        if existing
            .as_ref()
            .is_some_and(|existing| mount_source_identity_changed(existing, &mount))
        {
            clear_mount_source_state(&transaction, &mount.mount_id)?;
        }
        save_mount_row(&transaction, &mount)?;
        commit_workspace_binding_in_transaction(&transaction, &mount, &host_binding, &record)?;
        transaction.commit()?;
        Ok(())
    }

    fn save_mount_with_workspace_binding_and_cleanup(
        &mut self,
        mount: MountConfig,
        host_binding: WorkspaceHostBinding,
        record: WorkspaceBindingRecord,
        cleanup: &mut dyn FnMut() -> StoreResult<()>,
    ) -> StoreResult<()> {
        if mount.mount_id != record.mount_id {
            return Err(StoreError::InvalidState(
                "mount and workspace binding identities do not match".to_string(),
            ));
        }
        let mut connection = self.connection()?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prepared = {
            let mut statement = transaction.prepare(
                "SELECT recovery_id FROM workspace_remount_recoveries
                 WHERE mount_id = ?1 AND committed = 0",
            )?;
            statement
                .query_map(params![mount.mount_id.as_str()], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        let [recovery_id] = prepared.as_slice() else {
            return Err(StoreError::InvalidState(format!(
                "mount `{}` must have exactly one prepared workspace remount recovery",
                mount.mount_id.as_str()
            )));
        };
        validate_workspace_binding_commit(&mount, &host_binding, &record)?;
        clear_mount_source_state_after_durable_preservation(&transaction, &mount.mount_id)?;
        save_mount_row(&transaction, &mount)?;
        commit_workspace_binding_in_transaction(&transaction, &mount, &host_binding, &record)?;
        cleanup()?;
        let updated = transaction.execute(
            "UPDATE workspace_remount_recoveries
             SET committed = 1
             WHERE recovery_id = ?1 AND mount_id = ?2 AND committed = 0",
            params![recovery_id, mount.mount_id.as_str()],
        )?;
        if updated != 1 {
            return Err(StoreError::InvalidState(format!(
                "workspace remount recovery `{recovery_id}` lost its prepared outcome"
            )));
        }
        transaction.commit()?;
        Ok(())
    }

    fn begin_workspace_remount_recovery(
        &mut self,
        recovery_id: &str,
        mount_id: &MountId,
    ) -> StoreResult<()> {
        let connection = self.connection()?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        let existing = connection
            .query_row(
                "SELECT mount_id, committed FROM workspace_remount_recoveries
                 WHERE recovery_id = ?1",
                params![recovery_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        match existing {
            Some((existing_mount_id, 0)) if existing_mount_id == mount_id.as_str() => Ok(()),
            Some(_) => Err(StoreError::InvalidState(format!(
                "workspace remount recovery `{recovery_id}` already exists with different state"
            ))),
            None => connection
                .execute(
                    "INSERT INTO workspace_remount_recoveries (
                        recovery_id, mount_id, committed
                     ) VALUES (?1, ?2, 0)",
                    params![recovery_id, mount_id.as_str()],
                )
                .map(|_| ())
                .map_err(|error| {
                    if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) {
                        StoreError::InvalidState(format!(
                            "mount `{}` already has a workspace remount recovery",
                            mount_id.as_str()
                        ))
                    } else {
                        error.into()
                    }
                }),
        }
    }

    fn get_workspace_remount_recovery(
        &self,
        recovery_id: &str,
    ) -> StoreResult<Option<(MountId, WorkspaceRemountRecoveryOutcome)>> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT mount_id, committed FROM workspace_remount_recoveries
                 WHERE recovery_id = ?1",
                params![recovery_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
            .map(|(mount_id, committed)| match committed {
                0 => Ok((
                    MountId::new(mount_id),
                    WorkspaceRemountRecoveryOutcome::Prepared,
                )),
                1 => Ok((
                    MountId::new(mount_id),
                    WorkspaceRemountRecoveryOutcome::Committed,
                )),
                _ => Err(StoreError::InvalidState(format!(
                    "workspace remount recovery `{recovery_id}` has invalid outcome"
                ))),
            })
            .transpose()
    }

    fn list_workspace_remount_recoveries(
        &self,
    ) -> StoreResult<Vec<(String, MountId, WorkspaceRemountRecoveryOutcome)>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT recovery_id, mount_id, committed
             FROM workspace_remount_recoveries ORDER BY recovery_id",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .map(|row| {
                let (recovery_id, mount_id, committed) = row?;
                let outcome = match committed {
                    0 => WorkspaceRemountRecoveryOutcome::Prepared,
                    1 => WorkspaceRemountRecoveryOutcome::Committed,
                    _ => {
                        return Err(StoreError::InvalidState(format!(
                            "workspace remount recovery `{recovery_id}` has invalid outcome"
                        )));
                    }
                };
                Ok((recovery_id, MountId::new(mount_id), outcome))
            })
            .collect()
    }

    fn finish_workspace_remount_recovery(&mut self, recovery_id: &str) -> StoreResult<()> {
        let connection = self.connection()?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute(
            "DELETE FROM workspace_remount_recoveries WHERE recovery_id = ?1",
            params![recovery_id],
        )?;
        Ok(())
    }

    fn get_workspace_host_binding(
        &self,
        workspace_id: &WorkspaceId,
    ) -> StoreResult<Option<WorkspaceHostBinding>> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT binding_json FROM workspace_host_bindings WHERE workspace_id = ?1",
                params![workspace_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|json| workspace_host_binding_from_row(&json))
            .transpose()
    }

    fn check_workspace_rebind(&self, mount_id: &MountId) -> StoreResult<()> {
        let mount = self
            .get_mount(mount_id)?
            .ok_or_else(|| StoreError::MountMissing(mount_id.clone()))?;
        if self.get_workspace_binding(mount_id)?.is_none() {
            return Err(StoreError::WorkspaceBindingMissing(mount_id.clone()));
        }
        let blocker = if self.list_entities(mount_id)?.iter().any(|entity| {
            matches!(
                entity.hydration,
                HydrationState::Dirty | HydrationState::Conflicted
            )
        }) {
            WorkspaceRebindBlocker::DirtyOrConflictedState
        } else if self
            .list_journal()?
            .iter()
            .any(|journal| journal.mount_id == *mount_id && journal.status.is_unsettled())
        {
            WorkspaceRebindBlocker::UnsettledApplyJournal
        } else if !self.list_virtual_mutations(mount_id)?.is_empty() {
            WorkspaceRebindBlocker::PendingVirtualMutation
        } else {
            let connection = self.connection()?;
            let persisted_projection_state = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM projection_state WHERE mount_id = ?1)",
                params![mount_id.0.as_str()],
                |row| row.get::<_, bool>(0),
            )?;
            if mount.projection.uses_virtual_filesystem() || persisted_projection_state {
                WorkspaceRebindBlocker::ActiveProjection
            } else {
                WorkspaceRebindBlocker::RequiresOwningCoordinator
            }
        };
        Err(StoreError::WorkspaceRebindBlocked {
            mount_id: mount_id.clone(),
            blocker,
        })
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

impl ConnectorStateRepository for SqliteStateStore {
    fn save_connector_state(&mut self, state: ConnectorStateRecord) -> StoreResult<()> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO connector_state (
                connector,
                scope_kind,
                scope_id,
                state_version,
                min_reader_version,
                state_json,
                updated_at
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(connector, scope_kind, scope_id) DO UPDATE SET
                state_version = excluded.state_version,
                min_reader_version = excluded.min_reader_version,
                state_json = excluded.state_json,
                updated_at = excluded.updated_at",
            params![
                state.connector,
                state.scope_kind,
                state.scope_id,
                state.state_version,
                state.min_reader_version,
                state.state_json,
                state.updated_at,
            ],
        )?;
        Ok(())
    }

    fn get_connector_state(
        &self,
        connector: &str,
        scope_kind: &str,
        scope_id: &str,
    ) -> StoreResult<Option<ConnectorStateRecord>> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT connector, scope_kind, scope_id, state_version,
                        min_reader_version, state_json, updated_at
                 FROM connector_state
                 WHERE connector = ?1 AND scope_kind = ?2 AND scope_id = ?3",
                params![connector, scope_kind, scope_id],
                |row| {
                    Ok(ConnectorStateRecord {
                        connector: row.get(0)?,
                        scope_kind: row.get(1)?,
                        scope_id: row.get(2)?,
                        state_version: row.get(3)?,
                        min_reader_version: row.get(4)?,
                        state_json: row.get(5)?,
                        updated_at: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }
}

fn apply_discovery_commit(transaction: &Connection, commit: &DiscoveryCommit) -> StoreResult<()> {
    commit.validate()?;
    let connector = transaction
        .query_row(
            "SELECT connector FROM mounts WHERE mount_id = ?1",
            params![commit.mount_id.0.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::MountMissing(commit.mount_id.clone()))?;
    commit.validate_connector(&connector)?;

    let existing_entities = discovery_entities(&transaction, &commit.mount_id)?;
    let auto_save_enrollments = discovery_auto_save_enrollments(&transaction, &commit.mount_id)?;
    let virtual_mutations = discovery_virtual_mutations(&transaction, &commit.mount_id)?;
    let DiscoveryPreflight {
        final_entities: _,
        entity_deletes: _,
        deleted_paths,
        path_moves,
        auto_save_rehomes,
    } = commit.preflight_details(
        &connector,
        &existing_entities,
        &auto_save_enrollments,
        &virtual_mutations,
    )?;

    let final_path_texts = commit
        .entity_upserts
        .iter()
        .map(|entity| logical_path_to_text(&entity.path))
        .collect::<BTreeSet<_>>();
    for (index, (remote_id, _, _)) in path_moves.iter().enumerate() {
        let staging_path =
            discovery_staging_path(&transaction, &commit.mount_id, index, &final_path_texts)?;
        transaction.execute(
            "UPDATE entities SET path = ?3 WHERE mount_id = ?1 AND remote_id = ?2",
            params![
                commit.mount_id.0.as_str(),
                remote_id.0.as_str(),
                staging_path
            ],
        )?;
    }

    for rehome in &auto_save_rehomes {
        transaction.execute(
            "DELETE FROM auto_save_enrollments WHERE mount_id = ?1 AND path = ?2",
            params![commit.mount_id.0.as_str(), path_to_text(&rehome.old_path)],
        )?;
    }

    for remote_id in &commit.entity_deletes {
        transaction.execute(
            "DELETE FROM shadows WHERE mount_id = ?1 AND entity_id = ?2",
            params![commit.mount_id.0.as_str(), remote_id.0.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM hydration_jobs WHERE mount_id = ?1 AND remote_id = ?2",
            params![commit.mount_id.0.as_str(), remote_id.0.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM freshness_states WHERE mount_id = ?1 AND remote_id = ?2",
            params![commit.mount_id.0.as_str(), remote_id.0.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM remote_observations WHERE mount_id = ?1 AND remote_id = ?2",
            params![commit.mount_id.0.as_str(), remote_id.0.as_str()],
        )?;
        if let Some(path) = deleted_paths.get(remote_id) {
            let path = logical_path_to_text(path);
            transaction.execute(
                "DELETE FROM auto_save_enrollments
                     WHERE mount_id = ?1 AND (remote_id = ?2 OR path = ?3)",
                params![commit.mount_id.0.as_str(), remote_id.0.as_str(), path],
            )?;
        } else {
            transaction.execute(
                "DELETE FROM auto_save_enrollments
                     WHERE mount_id = ?1 AND remote_id = ?2",
                params![commit.mount_id.0.as_str(), remote_id.0.as_str()],
            )?;
        }
        delete_entity_search_index(&transaction, &commit.mount_id, remote_id)?;
        transaction.execute(
            "DELETE FROM entities WHERE mount_id = ?1 AND remote_id = ?2",
            params![commit.mount_id.0.as_str(), remote_id.0.as_str()],
        )?;
    }
    for identifier in &commit.metadata_discovery_deletes {
        transaction.execute(
            "DELETE FROM metadata_discovery_jobs
                 WHERE mount_id = ?1 AND container_identifier = ?2",
            params![commit.mount_id.0.as_str(), identifier],
        )?;
    }
    for local_id in &commit.virtual_mutation_deletes {
        transaction.execute(
            "DELETE FROM virtual_mutations WHERE mount_id = ?1 AND local_id = ?2",
            params![commit.mount_id.0.as_str(), local_id],
        )?;
    }

    for (remote_id, _, new_path) in &path_moves {
        transaction.execute(
            "UPDATE hydration_jobs SET path = ?3
                 WHERE mount_id = ?1 AND remote_id = ?2",
            params![
                commit.mount_id.0.as_str(),
                remote_id.0.as_str(),
                path_to_text(new_path),
            ],
        )?;
    }
    for entity in &commit.entity_upserts {
        upsert_discovery_entity(&transaction, entity)?;
    }
    for observation in &commit.observation_upserts {
        upsert_discovery_observation(&transaction, observation)?;
    }
    for freshness in &commit.freshness_upserts {
        upsert_discovery_freshness(&transaction, freshness)?;
    }
    for rehome in auto_save_rehomes {
        upsert_discovery_auto_save(&transaction, &rehome.enrollment)?;
    }
    for enrollment in &commit.auto_save_upserts {
        upsert_discovery_auto_save(&transaction, enrollment)?;
    }

    let search_updates = commit
        .entity_deletes
        .iter()
        .chain(commit.entity_upserts.iter().map(|entity| &entity.remote_id))
        .chain(
            commit
                .observation_upserts
                .iter()
                .map(|observation| &observation.remote_id),
        )
        .cloned()
        .collect::<BTreeSet<_>>();
    for remote_id in search_updates {
        upsert_entity_search_index(&transaction, &commit.mount_id, &remote_id)?;
    }

    let checkpoint = &commit.checkpoint;
    transaction.execute(
        "INSERT INTO connector_state (
                connector, scope_kind, scope_id, state_version,
                min_reader_version, state_json, updated_at
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(connector, scope_kind, scope_id) DO UPDATE SET
                state_version = excluded.state_version,
                min_reader_version = excluded.min_reader_version,
                state_json = excluded.state_json,
                updated_at = excluded.updated_at",
        params![
            checkpoint.connector.as_str(),
            checkpoint.scope_kind.as_str(),
            checkpoint.scope_id.as_str(),
            checkpoint.state_version,
            checkpoint.min_reader_version,
            checkpoint.state_json.as_str(),
            checkpoint.updated_at.as_str(),
        ],
    )?;
    Ok(())
}

#[derive(Debug)]
struct DiscoveryTransactionDbRow {
    transaction_id: String,
    mount_id: String,
    projection_json: String,
    status: String,
    active: i64,
    state_version: i64,
    min_reader_version: i64,
    plan_json: String,
    commit_json: String,
    reservation_json: String,
    effects_json: String,
    error_json: Option<String>,
    created_at: String,
    updated_at: String,
    committed_at: Option<String>,
    finalized_at: Option<String>,
}

fn discovery_transaction_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<DiscoveryTransactionDbRow> {
    Ok(DiscoveryTransactionDbRow {
        transaction_id: row.get(0)?,
        mount_id: row.get(1)?,
        projection_json: row.get(2)?,
        status: row.get(3)?,
        active: row.get(4)?,
        state_version: row.get(5)?,
        min_reader_version: row.get(6)?,
        plan_json: row.get(7)?,
        commit_json: row.get(8)?,
        reservation_json: row.get(9)?,
        effects_json: row.get(10)?,
        error_json: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
        committed_at: row.get(14)?,
        finalized_at: row.get(15)?,
    })
}

const DISCOVERY_TRANSACTION_SELECT: &str = "
    SELECT transaction_id, mount_id, projection_json, status, active,
           state_version, min_reader_version, plan_json, commit_json,
           reservation_json, effects_json, error_json, created_at, updated_at,
           committed_at, finalized_at
    FROM discovery_projection_transactions
    ";

fn discovery_transaction_from_row(
    row: DiscoveryTransactionDbRow,
) -> StoreResult<DiscoveryTransactionRecord> {
    validate_envelope_version(row.state_version, row.min_reader_version, "record")?;
    let transaction_id = DiscoveryTransactionId::new(row.transaction_id);
    let mount_id = MountId::new(row.mount_id);
    let projection = from_json::<ProjectionMode>(&row.projection_json)?;
    let status = DiscoveryTransactionStatus::parse(&row.status)?;
    let active = row.active != 0;
    if active != status.is_active() {
        return Err(StoreError::InvalidState(format!(
            "discovery transaction `{}` status and active marker disagree",
            transaction_id.0
        )));
    }
    let plan = decode_envelope::<Value>(&row.plan_json, "plan")?;
    let commit = decode_envelope::<TransactionalDiscoveryCommit>(&row.commit_json, "commit")?;
    let reservation =
        decode_envelope::<DiscoveryReservation>(&row.reservation_json, "reservation")?;
    let effects = decode_envelope::<Value>(&row.effects_json, "effects")?;
    if commit.transaction_id != transaction_id {
        return Err(StoreError::InvalidState(format!(
            "discovery transaction `{}` stored commit identifier does not match its row",
            transaction_id.0
        )));
    }
    if commit.commit.mount_id != mount_id || reservation.mount.mount_id != mount_id {
        return Err(StoreError::InvalidState(format!(
            "discovery transaction `{}` stored mount identifiers disagree",
            transaction_id.0
        )));
    }
    if reservation.mount.projection != projection {
        return Err(StoreError::InvalidState(format!(
            "discovery transaction `{}` stored projection identifiers disagree",
            transaction_id.0
        )));
    }
    Ok(DiscoveryTransactionRecord {
        transaction_id,
        mount_id,
        projection,
        status,
        active,
        plan: canonicalize_json_value(plan),
        commit,
        reservation,
        effects: canonicalize_json_value(effects),
        error: row
            .error_json
            .map(|value| serde_json::from_str::<Value>(&value))
            .transpose()?
            .map(canonicalize_json_value),
        created_at: row.created_at,
        updated_at: row.updated_at,
        committed_at: row.committed_at,
        finalized_at: row.finalized_at,
    })
}

fn select_discovery_transaction(
    connection: &Connection,
    transaction_id: &DiscoveryTransactionId,
) -> StoreResult<Option<DiscoveryTransactionRecord>> {
    connection
        .query_row(
            &(DISCOVERY_TRANSACTION_SELECT.to_owned() + "WHERE transaction_id = ?1"),
            params![transaction_id.0.as_str()],
            discovery_transaction_row,
        )
        .optional()?
        .map(discovery_transaction_from_row)
        .transpose()
}

fn capture_discovery_reservation_from_connection(
    connection: &Connection,
    mount_id: &MountId,
) -> StoreResult<DiscoveryReservation> {
    let mount = connection
        .query_row(
            "SELECT mount_id, connector, root, remote_root_id, read_only, projection_json,
                    connection_id, settings_json
             FROM mounts WHERE mount_id = ?1",
            params![mount_id.0.as_str()],
            |row| {
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
            },
        )
        .optional()?
        .map(mount_from_row)
        .transpose()?
        .ok_or_else(|| StoreError::MountMissing(mount_id.clone()))?;
    let mount_live_mode = connection
        .query_row(
            &(MOUNT_LIVE_MODE_SELECT_WITH_WHERE.to_owned() + "WHERE mount_id = ?1"),
            params![mount_id.0.as_str()],
            mount_live_mode_row,
        )
        .optional()?
        .map(mount_live_mode_from_row)
        .transpose()?;
    let checkpoint = connection
        .query_row(
            "SELECT connector, scope_kind, scope_id, state_version, min_reader_version,
                    state_json, updated_at
             FROM connector_state
             WHERE connector = ?1 AND scope_kind = 'mount' AND scope_id = ?2",
            params![mount.connector.as_str(), mount_id.0.as_str()],
            |row| {
                Ok(ConnectorStateRecord {
                    connector: row.get(0)?,
                    scope_kind: row.get(1)?,
                    scope_id: row.get(2)?,
                    state_version: row.get(3)?,
                    min_reader_version: row.get(4)?,
                    state_json: row.get(5)?,
                    updated_at: row.get(6)?,
                })
            },
        )
        .optional()?;

    let shadows = {
        let mut statement = connection.prepare(
            "SELECT mount_id, entity_id, frontmatter, body_hash, rendered_body, blocks_json
             FROM shadows WHERE mount_id = ?1 ORDER BY entity_id",
        )?;
        let rows = statement.query_map(params![mount_id.0.as_str()], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })?;
        rows.map(|row| shadow_from_row(row?))
            .collect::<StoreResult<Vec<_>>>()?
    };
    let hydration_jobs = {
        let mut statement = connection.prepare(
            "SELECT mount_id, remote_id, path, target_state_json, reason_json, attempts, last_error
             FROM hydration_jobs WHERE mount_id = ?1 ORDER BY remote_id",
        )?;
        let rows = statement.query_map(params![mount_id.0.as_str()], hydration_job_row)?;
        rows.map(|row| hydration_job_from_row(row?))
            .collect::<StoreResult<Vec<_>>>()?
    };
    let remote_observations = {
        let mut statement = connection.prepare(
            &(REMOTE_OBSERVATION_SELECT_WITH_WHERE.to_owned()
                + "WHERE mount_id = ?1 ORDER BY remote_id"),
        )?;
        let rows = statement.query_map(params![mount_id.0.as_str()], remote_observation_row)?;
        rows.map(|row| remote_observation_from_row(row?))
            .collect::<StoreResult<Vec<_>>>()?
    };
    let freshness_states = {
        let mut statement = connection.prepare(
            &(FRESHNESS_STATE_SELECT_WITH_WHERE.to_owned()
                + "WHERE mount_id = ?1 ORDER BY remote_id"),
        )?;
        let rows = statement.query_map(params![mount_id.0.as_str()], freshness_state_row)?;
        rows.map(|row| freshness_state_from_row(row?))
            .collect::<StoreResult<Vec<_>>>()?
    };
    let metadata_discovery_jobs = {
        let mut statement = connection.prepare(
            &(METADATA_DISCOVERY_JOB_SELECT_WITH_WHERE.to_owned()
                + "WHERE mount_id = ?1 ORDER BY container_identifier"),
        )?;
        let rows = statement.query_map(params![mount_id.0.as_str()], metadata_discovery_job_row)?;
        rows.map(|row| metadata_discovery_job_from_row(row?))
            .collect::<StoreResult<Vec<_>>>()?
    };
    let unsettled_journals = {
        let mut statement = connection.prepare(
            "SELECT push_id, mount_id, remote_ids_json, plan_json, preimages_json,
                    apply_effects_json, status_json, metadata_json, readable_diff_json
             FROM journals WHERE mount_id = ?1 ORDER BY push_id",
        )?;
        let rows = statement.query_map(params![mount_id.0.as_str()], journal_row)?;
        rows.map(|row| journal_from_row(row?))
            .collect::<StoreResult<Vec<_>>>()?
            .into_iter()
            .filter(|entry| entry.status.is_unsettled())
            .collect()
    };

    Ok(DiscoveryReservation {
        mount,
        mount_live_mode,
        checkpoint,
        entities: discovery_entities(connection, mount_id)?,
        shadows,
        hydration_jobs,
        virtual_mutations: discovery_virtual_mutations(connection, mount_id)?,
        auto_save_enrollments: discovery_auto_save_enrollments(connection, mount_id)?,
        remote_observations,
        freshness_states,
        metadata_discovery_jobs,
        unsettled_journals,
    })
}

fn illegal_discovery_transition(
    transaction_id: &DiscoveryTransactionId,
    from: DiscoveryTransactionStatus,
    to: DiscoveryTransactionStatus,
) -> StoreError {
    StoreError::InvalidState(format!(
        "discovery transaction `{}` cannot transition from `{}` to `{}`",
        transaction_id.0,
        from.as_str(),
        to.as_str()
    ))
}

fn transition_discovery_transaction(
    store: &mut SqliteStateStore,
    transaction_id: &DiscoveryTransactionId,
    expected_status: DiscoveryTransactionStatus,
    next_status: DiscoveryTransactionStatus,
    updated_at: &str,
    error: Option<Value>,
) -> StoreResult<DiscoveryTransactionRecord> {
    let mut connection = store.connection()?;
    let transaction = connection.transaction()?;
    let record = select_discovery_transaction(&transaction, transaction_id)?
        .ok_or_else(|| transaction_missing(transaction_id))?;
    require_transaction_status(&record, expected_status)?;
    let error_json = error
        .map(canonicalize_json_value)
        .as_ref()
        .map(canonical_json)
        .transpose()?;
    let finalized_at = (next_status == DiscoveryTransactionStatus::Finalized).then_some(updated_at);
    let changed = transaction.execute(
        "UPDATE discovery_projection_transactions
         SET status = ?3,
             active = ?4,
             updated_at = ?5,
             error_json = COALESCE(?6, error_json),
             finalized_at = COALESCE(?7, finalized_at)
         WHERE transaction_id = ?1 AND status = ?2 AND active = 1",
        params![
            transaction_id.0.as_str(),
            expected_status.as_str(),
            next_status.as_str(),
            bool_to_int(next_status.is_active()),
            updated_at,
            error_json.as_deref(),
            finalized_at,
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::InvalidState(format!(
            "discovery transaction `{}` changed during transition",
            transaction_id.0
        )));
    }
    let updated = select_discovery_transaction(&transaction, transaction_id)?
        .ok_or_else(|| transaction_missing(transaction_id))?;
    transaction.commit()?;
    Ok(updated)
}

impl DiscoveryRepository for SqliteStateStore {
    fn capture_discovery_reservation(
        &self,
        mount_id: &MountId,
    ) -> StoreResult<DiscoveryReservation> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let reservation = capture_discovery_reservation_from_connection(&transaction, mount_id)?;
        transaction.commit()?;
        Ok(reservation)
    }

    fn reserve_discovery_transaction(
        &mut self,
        prepared: PreparedDiscoveryTransaction,
    ) -> StoreResult<DiscoveryTransactionRecord> {
        let transaction_id = prepared.commit.transaction_id.clone();
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = select_discovery_transaction(&transaction, &transaction_id)? {
            if prepared_matches_record(&prepared, &existing) {
                transaction.commit()?;
                return Ok(existing);
            }
            return Err(StoreError::InvalidState(format!(
                "discovery transaction `{}` reservation retry does not match its immutable payload",
                transaction_id.0
            )));
        }

        let mount_id = prepared.commit.commit.mount_id.clone();
        let active_transaction = transaction
            .query_row(
                "SELECT transaction_id
                 FROM discovery_projection_transactions
                 WHERE mount_id = ?1 AND active = 1",
                params![mount_id.0.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(active_transaction) = active_transaction {
            return Err(StoreError::InvalidState(format!(
                "mount `{}` already has active discovery transaction `{active_transaction}`",
                mount_id.0
            )));
        }
        let current = capture_discovery_reservation_from_connection(&transaction, &mount_id)?;
        let record = record_from_prepared(prepared, &current)?;
        transaction.execute(
            "INSERT INTO discovery_projection_transactions (
                transaction_id, mount_id, projection_json, status, active,
                state_version, min_reader_version, plan_json, commit_json,
                reservation_json, effects_json, error_json, created_at, updated_at,
                committed_at, finalized_at
             ) VALUES (?1, ?2, ?3, 'reserved', 1, ?4, ?5, ?6, ?7, ?8, ?9,
                       NULL, ?10, ?10, NULL, NULL)",
            params![
                record.transaction_id.0.as_str(),
                record.mount_id.0.as_str(),
                to_json(&record.projection)?,
                DISCOVERY_TRANSACTION_STATE_VERSION,
                DISCOVERY_TRANSACTION_MIN_READER_VERSION,
                canonical_envelope_json(&record.plan)?,
                canonical_envelope_json(&record.commit)?,
                canonical_envelope_json(&record.reservation)?,
                canonical_envelope_json(&record.effects)?,
                record.created_at.as_str(),
            ],
        )?;
        let stored = select_discovery_transaction(&transaction, &transaction_id)?
            .ok_or_else(|| transaction_missing(&transaction_id))?;
        transaction.commit()?;
        Ok(stored)
    }

    fn get_discovery_transaction(
        &self,
        transaction_id: &DiscoveryTransactionId,
    ) -> StoreResult<Option<DiscoveryTransactionRecord>> {
        let connection = self.connection()?;
        select_discovery_transaction(&connection, transaction_id)
    }

    fn list_active_discovery_transactions(&self) -> StoreResult<Vec<DiscoveryTransactionRecord>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            &(DISCOVERY_TRANSACTION_SELECT.to_owned()
                + "WHERE active = 1 ORDER BY mount_id, transaction_id"),
        )?;
        let rows = statement.query_map([], discovery_transaction_row)?;
        rows.map(|row| discovery_transaction_from_row(row?))
            .collect()
    }

    fn mark_discovery_transaction_applying(
        &mut self,
        transaction_id: &DiscoveryTransactionId,
        updated_at: &str,
    ) -> StoreResult<DiscoveryTransactionRecord> {
        transition_discovery_transaction(
            self,
            transaction_id,
            DiscoveryTransactionStatus::Reserved,
            DiscoveryTransactionStatus::Applying,
            updated_at,
            None,
        )
    }

    fn record_discovery_transaction_effects(
        &mut self,
        transaction_id: &DiscoveryTransactionId,
        expected_status: DiscoveryTransactionStatus,
        effects: Value,
        updated_at: &str,
    ) -> StoreResult<DiscoveryTransactionRecord> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let record = select_discovery_transaction(&transaction, transaction_id)?
            .ok_or_else(|| transaction_missing(transaction_id))?;
        require_transaction_status(&record, expected_status)?;
        let changed = transaction.execute(
            "UPDATE discovery_projection_transactions
             SET effects_json = ?3, updated_at = ?4
             WHERE transaction_id = ?1 AND status = ?2 AND active = 1",
            params![
                transaction_id.0.as_str(),
                expected_status.as_str(),
                canonical_envelope_json(&canonicalize_json_value(effects))?,
                updated_at,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidState(format!(
                "discovery transaction `{}` changed during effects update",
                transaction_id.0
            )));
        }
        let updated = select_discovery_transaction(&transaction, transaction_id)?
            .ok_or_else(|| transaction_missing(transaction_id))?;
        transaction.commit()?;
        Ok(updated)
    }

    fn mark_discovery_transaction_projected(
        &mut self,
        transaction_id: &DiscoveryTransactionId,
        expected_status: DiscoveryTransactionStatus,
        updated_at: &str,
    ) -> StoreResult<DiscoveryTransactionRecord> {
        if !matches!(
            expected_status,
            DiscoveryTransactionStatus::Applying | DiscoveryTransactionStatus::RepairPending
        ) {
            return Err(illegal_discovery_transition(
                transaction_id,
                expected_status,
                DiscoveryTransactionStatus::Projected,
            ));
        }
        transition_discovery_transaction(
            self,
            transaction_id,
            expected_status,
            DiscoveryTransactionStatus::Projected,
            updated_at,
            None,
        )
    }

    fn commit_discovery_transaction(
        &mut self,
        transaction_id: &DiscoveryTransactionId,
        committed_at: &str,
    ) -> StoreResult<DiscoveryTransactionRecord> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record = select_discovery_transaction(&transaction, transaction_id)?
            .ok_or_else(|| transaction_missing(transaction_id))?;
        require_transaction_status(&record, DiscoveryTransactionStatus::Projected)?;
        let current =
            capture_discovery_reservation_from_connection(&transaction, &record.mount_id)?;
        if let Some(category) = record.reservation.changed_category(&current) {
            return Err(reservation_changed(transaction_id, category));
        }
        apply_discovery_commit(&transaction, &record.commit.commit)?;
        let changed = transaction.execute(
            "UPDATE discovery_projection_transactions
             SET status = 'committed', updated_at = ?2, committed_at = ?2,
                 error_json = NULL
             WHERE transaction_id = ?1 AND status = 'projected' AND active = 1",
            params![transaction_id.0.as_str(), committed_at],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidState(format!(
                "discovery transaction `{}` changed during commit",
                transaction_id.0
            )));
        }
        let committed = select_discovery_transaction(&transaction, transaction_id)?
            .ok_or_else(|| transaction_missing(transaction_id))?;
        transaction.commit()?;
        Ok(committed)
    }

    fn mark_discovery_transaction_repair_pending(
        &mut self,
        transaction_id: &DiscoveryTransactionId,
        expected_status: DiscoveryTransactionStatus,
        error: Value,
        updated_at: &str,
    ) -> StoreResult<DiscoveryTransactionRecord> {
        if !matches!(
            expected_status,
            DiscoveryTransactionStatus::Applying | DiscoveryTransactionStatus::Projected
        ) {
            return Err(illegal_discovery_transition(
                transaction_id,
                expected_status,
                DiscoveryTransactionStatus::RepairPending,
            ));
        }
        transition_discovery_transaction(
            self,
            transaction_id,
            expected_status,
            DiscoveryTransactionStatus::RepairPending,
            updated_at,
            Some(error),
        )
    }

    fn mark_discovery_transaction_aborted(
        &mut self,
        transaction_id: &DiscoveryTransactionId,
        expected_status: DiscoveryTransactionStatus,
        updated_at: &str,
    ) -> StoreResult<DiscoveryTransactionRecord> {
        if !matches!(
            expected_status,
            DiscoveryTransactionStatus::Reserved
                | DiscoveryTransactionStatus::Applying
                | DiscoveryTransactionStatus::Projected
                | DiscoveryTransactionStatus::RepairPending
        ) {
            return Err(illegal_discovery_transition(
                transaction_id,
                expected_status,
                DiscoveryTransactionStatus::Aborted,
            ));
        }
        transition_discovery_transaction(
            self,
            transaction_id,
            expected_status,
            DiscoveryTransactionStatus::Aborted,
            updated_at,
            None,
        )
    }

    fn mark_discovery_transaction_finalized(
        &mut self,
        transaction_id: &DiscoveryTransactionId,
        finalized_at: &str,
    ) -> StoreResult<DiscoveryTransactionRecord> {
        transition_discovery_transaction(
            self,
            transaction_id,
            DiscoveryTransactionStatus::Committed,
            DiscoveryTransactionStatus::Finalized,
            finalized_at,
            None,
        )
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
                 FROM search_documents_fts
                 WHERE search_documents_fts MATCH ?1
                   AND mount_id = ?2
                 ORDER BY bm25(
                    search_documents_fts,
                    0.0, 0.0, 0.0, 0.0,
                    8.0, 6.0, 7.0, 5.0,
                    2.0, 1.0, 4.0, 3.0, 9.0, 6.0
                 )
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
                let search_document = search_document(&connection, mount_id, &remote_id)?;
                candidates.push(EntitySearchCandidate {
                    entity,
                    observation: self.get_remote_observation(mount_id, &remote_id)?,
                    search_document,
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

impl MetadataDiscoveryJobRepository for SqliteStateStore {
    fn upsert_metadata_discovery_job(
        &mut self,
        job: MetadataDiscoveryJobRecord,
    ) -> StoreResult<()> {
        let connection = self.connection()?;
        let sql = METADATA_DISCOVERY_JOB_SELECT_WITH_WHERE.to_owned()
            + "WHERE mount_id = ?1 AND container_identifier = ?2";
        let record = connection
            .query_row(
                &sql,
                params![job.mount_id.0.as_str(), job.container_identifier.as_str()],
                metadata_discovery_job_row,
            )
            .optional()?
            .map(metadata_discovery_job_from_row)
            .transpose()?
            .map(|mut existing| {
                existing.priority = existing.priority.max(job.priority);
                existing.depth = job.depth;
                existing.updated_at = job.updated_at.clone();
                existing
            })
            .unwrap_or(job);

        connection.execute(
            "INSERT INTO metadata_discovery_jobs (
                mount_id,
                container_identifier,
                priority_json,
                depth,
                attempts,
                last_error,
                created_at,
                updated_at
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(mount_id, container_identifier) DO UPDATE SET
                priority_json = excluded.priority_json,
                depth = excluded.depth,
                attempts = excluded.attempts,
                last_error = excluded.last_error,
                updated_at = excluded.updated_at",
            params![
                record.mount_id.0.as_str(),
                record.container_identifier.as_str(),
                to_json(&record.priority)?,
                i64::from(record.depth),
                i64::from(record.attempts),
                record.last_error.as_deref(),
                record.created_at.as_str(),
                record.updated_at.as_str(),
            ],
        )?;
        Ok(())
    }

    fn list_metadata_discovery_jobs(&self) -> StoreResult<Vec<MetadataDiscoveryJobRecord>> {
        let connection = self.connection()?;
        let mut statement =
            connection.prepare(&(METADATA_DISCOVERY_JOB_SELECT_WITH_WHERE.to_owned()))?;
        let rows = statement.query_map([], metadata_discovery_job_row)?;
        let mut jobs = rows
            .map(|row| metadata_discovery_job_from_row(row?))
            .collect::<StoreResult<Vec<_>>>()?;
        jobs.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.depth.cmp(&right.depth))
                .then_with(|| left.attempts.cmp(&right.attempts))
                .then_with(|| left.mount_id.0.cmp(&right.mount_id.0))
                .then_with(|| left.container_identifier.cmp(&right.container_identifier))
        });
        Ok(jobs)
    }

    fn delete_metadata_discovery_job(
        &mut self,
        mount_id: &MountId,
        container_identifier: &str,
    ) -> StoreResult<()> {
        let connection = self.connection()?;
        connection.execute(
            "DELETE FROM metadata_discovery_jobs
             WHERE mount_id = ?1 AND container_identifier = ?2",
            params![mount_id.0, container_identifier],
        )?;
        Ok(())
    }

    fn record_metadata_discovery_job_failure(
        &mut self,
        mount_id: &MountId,
        container_identifier: &str,
        message: String,
    ) -> StoreResult<()> {
        let connection = self.connection()?;
        connection.execute(
            "UPDATE metadata_discovery_jobs
             SET attempts = attempts + 1,
                 last_error = ?3
             WHERE mount_id = ?1 AND container_identifier = ?2",
            params![mount_id.0, container_identifier, message],
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
                record.mount_id.0.as_str(),
                record.entity_id.0.as_str(),
                record.frontmatter.as_str(),
                record.body_hash.as_str(),
                record.rendered_body.as_str(),
                to_json(&record.blocks)?,
            ],
        )?;
        upsert_entity_search_index(&connection, mount_id, &shadow.entity_id)?;
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
                    .map(|path| native_path_to_text(path)),
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

impl VirtualMoveRepository for SqliteStateStore {
    fn begin_virtual_move(&mut self, transition: VirtualMoveTransition) -> StoreResult<()> {
        validate_virtual_move_transition(&transition)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let mount_id = transition.mutation.mount_id.clone();

        for local_id in &transition.superseded_local_ids {
            transaction.execute(
                "DELETE FROM virtual_mutations WHERE mount_id = ?1 AND local_id = ?2",
                params![mount_id.0, local_id],
            )?;
        }

        if let Some(entity) = &transition.entity {
            let path = logical_path_to_text(&entity.path);
            let existing_remote_id: Option<String> = transaction
                .query_row(
                    "SELECT remote_id FROM entities WHERE mount_id = ?1 AND path = ?2",
                    params![entity.mount_id.0, path],
                    |row| row.get(0),
                )
                .optional()?;
            if existing_remote_id
                .as_deref()
                .is_some_and(|remote_id| remote_id != entity.remote_id.0)
            {
                return Err(StoreError::DuplicateEntityPath {
                    mount_id: entity.mount_id.clone(),
                    path: entity.path.clone(),
                });
            }
            transaction.execute(
                "INSERT INTO entities (
                    mount_id, remote_id, kind_json, title, path, hydration_json,
                    content_hash, remote_edited_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
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
            upsert_entity_search_index(&transaction, &entity.mount_id, &entity.remote_id)?;
        }

        if let Some(freshness) = &transition.freshness {
            transaction.execute(
                "INSERT INTO freshness_states (
                    mount_id, remote_id, tier_json, last_checked_at, next_check_at,
                    last_opened_at, last_local_change_at, remote_hint_pending
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(mount_id, remote_id) DO UPDATE SET
                    tier_json = excluded.tier_json,
                    last_checked_at = excluded.last_checked_at,
                    next_check_at = excluded.next_check_at,
                    last_opened_at = excluded.last_opened_at,
                    last_local_change_at = excluded.last_local_change_at,
                    remote_hint_pending = excluded.remote_hint_pending",
                params![
                    freshness.mount_id.0,
                    freshness.remote_id.0,
                    to_json(&freshness.tier)?,
                    freshness.last_checked_at,
                    freshness.next_check_at,
                    freshness.last_opened_at,
                    freshness.last_local_change_at,
                    bool_to_int(freshness.remote_hint_pending),
                ],
            )?;
        }

        let mutation = &transition.mutation;
        transaction.execute(
            "INSERT INTO virtual_mutations (
                mount_id, local_id, mutation_kind_json, target_remote_id,
                parent_remote_id, original_path, projected_path, title,
                content_path, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
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
                mutation.target_remote_id.as_ref().map(|id| id.0.as_str()),
                mutation.parent_remote_id.as_ref().map(|id| id.0.as_str()),
                mutation
                    .original_path
                    .as_ref()
                    .map(|path| logical_path_to_text(path)),
                logical_path_to_text(&mutation.projected_path),
                mutation.title,
                mutation
                    .content_path
                    .as_ref()
                    .map(|path| native_path_to_text(path)),
                mutation.created_at,
                mutation.updated_at,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn finalize_virtual_move_content(
        &mut self,
        mount_id: &MountId,
        local_id: &str,
        expected_content_path: Option<&Path>,
        content_path: PathBuf,
        updated_at: &str,
    ) -> StoreResult<VirtualMutationRecord> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let sql =
            VIRTUAL_MUTATION_SELECT_WITH_WHERE.to_owned() + "WHERE mount_id = ?1 AND local_id = ?2";
        let mut mutation = transaction
            .query_row(&sql, params![mount_id.0, local_id], virtual_mutation_row)
            .optional()?
            .map(virtual_mutation_from_row)
            .transpose()?
            .ok_or_else(|| virtual_move_missing(mount_id, local_id))?;
        if mutation.content_path.as_deref() != expected_content_path {
            return Err(virtual_move_content_changed(
                mount_id,
                local_id,
                expected_content_path,
                mutation.content_path.as_deref(),
            ));
        }
        transaction.execute(
            "UPDATE virtual_mutations
             SET content_path = ?3, updated_at = ?4
             WHERE mount_id = ?1 AND local_id = ?2",
            params![
                mount_id.0,
                local_id,
                native_path_to_text(&content_path),
                updated_at
            ],
        )?;
        mutation.content_path = Some(content_path);
        mutation.updated_at = updated_at.to_string();
        transaction.commit()?;
        Ok(mutation)
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
                status_json,
                metadata_json,
                readable_diff_json
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                entry.push_id.0,
                entry.mount_id.0,
                to_json(&entry.remote_ids)?,
                to_json(&entry.plan)?,
                to_json(&entry.preimages)?,
                to_json(&entry.apply_effects)?,
                to_json(&entry.status)?,
                to_json(&entry.metadata)?,
                optional_to_json(&entry.readable_diff)?,
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
                "SELECT push_id, mount_id, remote_ids_json, plan_json, preimages_json, apply_effects_json, status_json, metadata_json, readable_diff_json
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
            "SELECT push_id, mount_id, remote_ids_json, plan_json, preimages_json, apply_effects_json, status_json, metadata_json, readable_diff_json
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
        || existing.settings_json != next.settings_json
}

fn clear_mount_source_state(connection: &Connection, mount_id: &MountId) -> StoreResult<()> {
    clear_mount_source_state_with_policy(connection, mount_id, false)
}

/// Coordinator-only source reset used after Desktop has durably copied every
/// pending local mutation into the remount recovery directory.
fn clear_mount_source_state_after_durable_preservation(
    connection: &Connection,
    mount_id: &MountId,
) -> StoreResult<()> {
    clear_mount_source_state_with_policy(connection, mount_id, true)
}

fn clear_mount_source_state_with_policy(
    connection: &Connection,
    mount_id: &MountId,
    local_mutations_were_durably_preserved: bool,
) -> StoreResult<()> {
    let pending_virtual_mutation: Option<String> = connection
        .query_row(
            "SELECT local_id FROM virtual_mutations WHERE mount_id = ?1 LIMIT 1",
            params![&mount_id.0],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(local_id) = pending_virtual_mutation
        && !local_mutations_were_durably_preserved
    {
        return Err(StoreError::InvalidState(format!(
            "mount `{}` cannot change source while virtual mutation `{local_id}` is pending",
            mount_id.0
        )));
    }
    let unsettled_push = {
        let mut statement = connection.prepare(
            "SELECT push_id, status_json FROM journals WHERE mount_id = ?1 ORDER BY push_id",
        )?;
        let rows = statement.query_map(params![&mount_id.0], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut unsettled = None;
        for row in rows {
            let (push_id, status_json) = row?;
            let status: JournalStatus = from_json(&status_json)?;
            if status.is_unsettled() {
                unsettled = Some(push_id);
                break;
            }
        }
        unsettled
    };
    if let Some(push_id) = unsettled_push {
        return Err(StoreError::InvalidState(format!(
            "mount `{}` cannot change source while push journal `{push_id}` is unsettled",
            mount_id.0
        )));
    }
    let active_delta: Option<String> = connection
        .query_row(
            "SELECT delta_id FROM generation_apply_journals
             WHERE mount_id = ?1 AND active = 1 LIMIT 1",
            params![&mount_id.0],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(delta_id) = active_delta {
        return Err(StoreError::InvalidState(format!(
            "mount `{}` has active generation apply `{delta_id}`",
            mount_id.0
        )));
    }
    let pending_acknowledgment: Option<String> = connection
        .query_row(
            "SELECT delta_id FROM generation_apply_journals
             WHERE mount_id = ?1 AND status = 'completed'
               AND acknowledgment_required = 1 AND acknowledged_at IS NULL
             LIMIT 1",
            params![&mount_id.0],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(delta_id) = pending_acknowledgment {
        return Err(StoreError::InvalidState(format!(
            "mount `{}` has pending terminal acknowledgment for generation apply `{delta_id}`",
            mount_id.0
        )));
    }
    let preserved_path: Option<(String, String)> = connection
        .query_row(
            "SELECT projection_id, state FROM generation_paths
             WHERE mount_id = ?1 AND state IN ('dirty', 'conflicted') LIMIT 1",
            params![&mount_id.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((projection_id, state)) = preserved_path {
        return Err(StoreError::InvalidState(format!(
            "mount `{}` cannot change source while generation path `{projection_id}` is {state}",
            mount_id.0
        )));
    }
    let retained_inode: Option<String> = connection
        .query_row(
            "SELECT evidence.logical_path
             FROM generation_inode_evidence AS evidence
             WHERE evidence.mount_id = ?1 LIMIT 1",
            params![&mount_id.0],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(logical_path) = retained_inode {
        return Err(StoreError::InvalidState(format!(
            "mount `{}` cannot change source while displaced inode evidence for `{logical_path}` is retained",
            mount_id.0
        )));
    }
    // Retire only completed clean lineage in the same transaction as the
    // source reset. Active, dirty, and conflicted lineage is preserved above.
    connection.execute(
        "DELETE FROM generation_apply_journals WHERE mount_id = ?1 AND active = 0",
        params![&mount_id.0],
    )?;
    for table in [
        "generation_paths",
        "observed_generations",
        "entities",
        "shadows",
        "hydration_jobs",
        "metadata_discovery_jobs",
        "virtual_mutations",
        "mount_live_modes",
        "auto_save_enrollments",
        "remote_observations",
        "freshness_states",
        "journals",
        "entity_search_fts",
        "search_documents_fts",
    ] {
        connection.execute(
            &format!("DELETE FROM {table} WHERE mount_id = ?1"),
            params![&mount_id.0],
        )?;
    }
    connection.execute(
        "DELETE FROM connector_state WHERE scope_kind = 'mount' AND scope_id = ?1",
        params![&mount_id.0],
    )?;
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
const METADATA_DISCOVERY_JOB_SELECT_WITH_WHERE: &str = "
    SELECT mount_id, container_identifier, priority_json, depth, attempts, last_error,
           created_at, updated_at
    FROM metadata_discovery_jobs
    ";

type MountRow = (
    String,
    String,
    String,
    Option<String>,
    i64,
    String,
    Option<String>,
    String,
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
type JournalRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
);
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
type MetadataDiscoveryJobRow = (
    String,
    String,
    String,
    i64,
    i64,
    Option<String>,
    String,
    String,
);

fn initialize_schema(connection: &mut Connection) -> StoreResult<()> {
    let user_version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if user_version > SCHEMA_VERSION {
        return Err(StoreError::SchemaVersion {
            found: user_version,
            supported: SCHEMA_VERSION,
        });
    }
    if user_version == SCHEMA_VERSION {
        migrate_workspace_bindings_component_to_v4(connection)?;
        ensure_state_components_safe_before_mutation(connection, user_version)?;
        validate_workspace_bindings(connection)?;
        validate_hosted_workspace_storage(connection)?;
        retire_removed_state_components(connection)?;
        repair_missing_state_components(connection)?;
        ensure_state_components_allow_schema_migration(connection, user_version)?;
        migrate_linux_fuse_projection_layout_to_v2(connection, false)?;
        migrate_windows_cloud_files_projection_layout_to_v2(connection, false)?;
        migrate_journals_component_to_v3(connection)?;
        migrate_virtual_mutations_component_to_v4(connection)?;
        migrate_entity_search_component_to_v2(connection)?;
        migrate_generation_delivery_to_v7(connection, None, true)?;
        return Ok(());
    }

    if user_version >= 13 {
        ensure_state_components_safe_before_mutation(connection, user_version)?;
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
            connection_id TEXT,
            settings_json TEXT NOT NULL DEFAULT '{}'
        );

        CREATE TABLE IF NOT EXISTS hosted_workspace_attachments (
            api_origin TEXT NOT NULL,
            profile_id TEXT NOT NULL,
            credential_ref TEXT NOT NULL,
            root TEXT NOT NULL,
            profile_revision INTEGER NOT NULL CHECK (profile_revision > 0),
            layout_version INTEGER NOT NULL CHECK (layout_version > 0),
            layout_digest TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (api_origin, profile_id),
            UNIQUE (root)
        );

        CREATE TABLE IF NOT EXISTS hosted_workspace_mount_mappings (
            api_origin TEXT NOT NULL,
            profile_id TEXT NOT NULL,
            portable_mount_id TEXT NOT NULL,
            local_mount_id TEXT NOT NULL UNIQUE,
            mount_target TEXT NOT NULL,
            target_collision_key TEXT NOT NULL,
            active INTEGER NOT NULL CHECK (active IN (0, 1)),
            first_seen_revision INTEGER NOT NULL CHECK (first_seen_revision > 0),
            last_seen_revision INTEGER NOT NULL CHECK (
                last_seen_revision >= first_seen_revision
            ),
            PRIMARY KEY (api_origin, profile_id, portable_mount_id),
            FOREIGN KEY (api_origin, profile_id)
                REFERENCES hosted_workspace_attachments(api_origin, profile_id)
                ON DELETE CASCADE
        );

        CREATE UNIQUE INDEX IF NOT EXISTS hosted_workspace_active_target_unique
            ON hosted_workspace_mount_mappings(api_origin, profile_id, target_collision_key)
            WHERE active = 1;

        CREATE TABLE IF NOT EXISTS hosted_workspace_pending_transitions (
            transition_id TEXT PRIMARY KEY,
            api_origin TEXT NOT NULL,
            profile_id TEXT NOT NULL,
            credential_ref TEXT NOT NULL,
            target_root TEXT NOT NULL,
            transition_kind TEXT NOT NULL CHECK (
                transition_kind IN ('attach', 'refresh', 'relocate')
            ),
            profile_revision INTEGER NOT NULL CHECK (profile_revision > 0),
            layout_version INTEGER NOT NULL CHECK (layout_version > 0),
            layout_digest TEXT NOT NULL,
            base_profile_revision INTEGER CHECK (base_profile_revision > 0),
            base_layout_digest TEXT,
            base_root TEXT,
            created_at TEXT NOT NULL,
            UNIQUE (api_origin, profile_id),
            CHECK (
                (base_profile_revision IS NULL AND base_layout_digest IS NULL AND base_root IS NULL)
                OR
                (base_profile_revision IS NOT NULL AND base_layout_digest IS NOT NULL AND base_root IS NOT NULL)
            )
        );

        CREATE TABLE IF NOT EXISTS hosted_workspace_pending_mounts (
            transition_id TEXT NOT NULL,
            portable_mount_id TEXT NOT NULL,
            local_mount_id TEXT NOT NULL,
            mount_target TEXT NOT NULL,
            target_collision_key TEXT NOT NULL,
            first_seen_revision INTEGER NOT NULL CHECK (first_seen_revision > 0),
            PRIMARY KEY (transition_id, portable_mount_id),
            UNIQUE (transition_id, local_mount_id),
            UNIQUE (transition_id, target_collision_key),
            FOREIGN KEY (transition_id)
                REFERENCES hosted_workspace_pending_transitions(transition_id)
                ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS hosted_workspace_pending_cleanups (
            cleanup_id TEXT PRIMARY KEY,
            api_origin TEXT NOT NULL,
            profile_id TEXT NOT NULL,
            credential_ref TEXT NOT NULL,
            root TEXT NOT NULL,
            profile_revision INTEGER NOT NULL CHECK (profile_revision > 0),
            layout_version INTEGER NOT NULL CHECK (layout_version > 0),
            layout_digest TEXT NOT NULL,
            created_at TEXT NOT NULL,
            UNIQUE (api_origin, profile_id),
            UNIQUE (root),
            FOREIGN KEY (api_origin, profile_id)
                REFERENCES hosted_workspace_attachments(api_origin, profile_id)
                ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS discovery_projection_transactions (
            transaction_id TEXT PRIMARY KEY,
            mount_id TEXT NOT NULL,
            projection_json TEXT NOT NULL,
            status TEXT NOT NULL CHECK (
                status IN (
                    'reserved', 'applying', 'projected', 'committed',
                    'repair_pending', 'aborted', 'finalized'
                )
            ),
            active INTEGER NOT NULL CHECK (active IN (0, 1)),
            state_version INTEGER NOT NULL CHECK (state_version > 0),
            min_reader_version INTEGER NOT NULL CHECK (
                min_reader_version > 0 AND min_reader_version <= state_version
            ),
            plan_json TEXT NOT NULL,
            commit_json TEXT NOT NULL,
            reservation_json TEXT NOT NULL,
            effects_json TEXT NOT NULL,
            error_json TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            committed_at TEXT,
            finalized_at TEXT,
            CHECK (
                (active = 0 AND status IN ('aborted', 'finalized'))
                OR
                (active = 1 AND status IN (
                    'reserved', 'applying', 'projected', 'committed', 'repair_pending'
                ))
            ),
            FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
        );

        CREATE UNIQUE INDEX IF NOT EXISTS discovery_projection_one_active_per_mount
        ON discovery_projection_transactions(mount_id)
        WHERE active = 1;

        CREATE TRIGGER IF NOT EXISTS discovery_projection_block_active_mount_delete
        BEFORE DELETE ON mounts
        WHEN EXISTS (
            SELECT 1
            FROM discovery_projection_transactions
            WHERE mount_id = OLD.mount_id AND active = 1
        )
        BEGIN
            SELECT RAISE(ABORT, 'mount has an active discovery projection transaction');
        END;

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

        CREATE TABLE IF NOT EXISTS metadata_discovery_jobs (
            mount_id TEXT NOT NULL,
            container_identifier TEXT NOT NULL,
            priority_json TEXT NOT NULL,
            depth INTEGER NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0,
            last_error TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (mount_id, container_identifier),
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

        CREATE VIRTUAL TABLE IF NOT EXISTS search_documents_fts USING fts5(
            mount_id UNINDEXED,
            remote_id UNINDEXED,
            connector UNINDEXED,
            kind UNINDEXED,
            title,
            path,
            observed_title,
            observed_path,
            frontmatter,
            body,
            metadata_text,
            breadcrumbs,
            aliases,
            source_url
        );

        CREATE TABLE IF NOT EXISTS journals (
            push_id TEXT PRIMARY KEY,
            mount_id TEXT NOT NULL,
            remote_ids_json TEXT NOT NULL,
            plan_json TEXT NOT NULL,
            preimages_json TEXT NOT NULL DEFAULT '[]',
            apply_effects_json TEXT NOT NULL DEFAULT '[]',
            status_json TEXT NOT NULL,
            metadata_json TEXT NOT NULL DEFAULT '{\"author\":{\"kind\":\"anonymous\",\"display_name\":\"anonymous\"}}',
            readable_diff_json TEXT,
            FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS observed_generations (
            mount_id TEXT NOT NULL,
            source_connection_id TEXT NOT NULL,
            generation_id TEXT NOT NULL,
            inventory_sha256 TEXT NOT NULL,
            workspace_layout_version INTEGER NOT NULL CHECK (workspace_layout_version > 0),
            workspace_layout_digest TEXT NOT NULL,
            last_receipt_sha256 TEXT,
            updated_at TEXT NOT NULL,
            refresh_mode TEXT NOT NULL
                CHECK (refresh_mode IN ('generation_delta_v1', 'full_export_only')),
            PRIMARY KEY (mount_id, source_connection_id),
            FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS generation_paths (
            mount_id TEXT NOT NULL,
            source_connection_id TEXT NOT NULL,
            projection_id TEXT NOT NULL,
            logical_path TEXT NOT NULL,
            local_logical_path TEXT NOT NULL,
            base_generation_id TEXT NOT NULL,
            base_identity_json TEXT,
            base_payload_delta_id TEXT,
            base_payload_entry_index INTEGER CHECK (base_payload_entry_index >= 0),
            conflict_payload_delta_id TEXT,
            conflict_payload_entry_index INTEGER CHECK (conflict_payload_entry_index >= 0),
            state TEXT NOT NULL CHECK (state IN ('clean', 'dirty', 'conflicted')),
            incoming_identity_json TEXT,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (mount_id, projection_id),
            UNIQUE (mount_id, logical_path),
            UNIQUE (mount_id, local_logical_path),
            FOREIGN KEY (mount_id, source_connection_id)
                REFERENCES observed_generations(mount_id, source_connection_id)
                ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS generation_apply_journals (
            delta_id TEXT PRIMARY KEY,
            mount_id TEXT NOT NULL,
            source_connection_id TEXT NOT NULL,
            base_generation_id TEXT NOT NULL,
            target_generation_id TEXT NOT NULL,
            delta_json TEXT NOT NULL,
            receipt_json TEXT NOT NULL,
            receipt_sha256 TEXT NOT NULL,
            selected_capabilities_json TEXT NOT NULL DEFAULT '{}',
            selection_binding TEXT NOT NULL DEFAULT 'bound'
                CHECK (selection_binding IN ('bound', 'pre_binding_unknown', 'pre_binding_completed')),
            acknowledgment_required INTEGER NOT NULL DEFAULT 0
                CHECK (acknowledgment_required IN (0, 1)),
            acknowledged_at TEXT,
            stage_root TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('staged', 'applying', 'completed')),
            active INTEGER NOT NULL CHECK (active IN (0, 1)),
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            completed_at TEXT,
            CHECK (
                (active = 0 AND status = 'completed' AND completed_at IS NOT NULL)
                OR (active = 1 AND status IN ('staged', 'applying') AND completed_at IS NULL)
            ),
            FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS generation_apply_outcomes (
            delta_id TEXT NOT NULL,
            entry_index INTEGER NOT NULL CHECK (entry_index >= 0),
            outcome_json TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (delta_id, entry_index),
            FOREIGN KEY (delta_id) REFERENCES generation_apply_journals(delta_id) ON DELETE CASCADE
        );
        ",
    )?;
    ensure_generation_active_index_for_schema(connection)?;

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

    if user_version < 16 {
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS metadata_discovery_jobs (
                mount_id TEXT NOT NULL,
                container_identifier TEXT NOT NULL,
                priority_json TEXT NOT NULL,
                depth INTEGER NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                last_error TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (mount_id, container_identifier),
                FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
            );",
        )?;
        if user_version >= 13 {
            record_schema_migration(connection, user_version, SCHEMA_VERSION)?;
        }
    }

    if user_version < 17 {
        if !column_exists(connection, "journals", "metadata_json")? {
            connection.execute_batch(
                "ALTER TABLE journals
                 ADD COLUMN metadata_json TEXT NOT NULL DEFAULT '{\"author\":{\"kind\":\"anonymous\",\"display_name\":\"anonymous\"}}';",
            )?;
        }
        connection.execute(
            "UPDATE journals
             SET metadata_json = ?1
             WHERE metadata_json = '{}'",
            params![DEFAULT_JOURNAL_METADATA_JSON],
        )?;
        if !column_exists(connection, "journals", "readable_diff_json")? {
            connection.execute_batch(
                "ALTER TABLE journals
                 ADD COLUMN readable_diff_json TEXT;",
            )?;
        }
        if user_version >= 13 {
            record_schema_migration(connection, user_version, SCHEMA_VERSION)?;
        }
    }

    if user_version < 18 && !column_exists(connection, "mounts", "settings_json")? {
        connection.execute_batch(
            "ALTER TABLE mounts
             ADD COLUMN settings_json TEXT NOT NULL DEFAULT '{}';",
        )?;
        if user_version >= 13 {
            record_schema_migration(connection, user_version, SCHEMA_VERSION)?;
        }
    }

    if user_version < 19 && user_version >= 13 {
        record_schema_migration(connection, user_version, SCHEMA_VERSION)?;
    }

    if user_version < 20 {
        create_entity_search_index(connection)?;
        rebuild_entity_search_index(connection)?;
        if user_version >= 13 {
            record_schema_migration(connection, user_version, SCHEMA_VERSION)?;
        }
    }

    if user_version < 21 {
        create_generation_delivery_tables(connection)?;
        if user_version >= 13 {
            record_schema_migration(connection, user_version, SCHEMA_VERSION)?;
        }
    }

    if user_version == 21 && !column_exists(connection, "generation_apply_journals", "mount_id")? {
        migrate_generation_delivery_journals_to_mount_relation(connection)?;
        record_schema_migration(connection, user_version, SCHEMA_VERSION)?;
    }

    if user_version < SCHEMA_VERSION {
        migrate_generation_delivery_to_v7(
            connection,
            (user_version >= 21).then_some(user_version),
            user_version >= 21,
        )?;
    }

    if user_version < SCHEMA_VERSION {
        seed_default_notion_profile(connection)?;
        migrate_workspace_bindings_schema_v21(connection, user_version)?;
    }

    Ok(())
}

fn migrate_workspace_bindings_component_to_v4(connection: &mut Connection) -> StoreResult<()> {
    if !table_exists(connection, "state_components")? {
        return Ok(());
    }
    let version = connection
        .query_row(
            "SELECT version FROM state_components WHERE component_id = 'durable:workspace_bindings'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    let needs_table_migration = table_exists(connection, "workspace_bindings")?
        && !column_exists(connection, "workspace_bindings", "workspace_id")?;
    if !matches!(version, Some(2 | 3)) && !needs_table_migration {
        return Ok(());
    }

    ensure_state_components_safe_before_mutation(connection, SCHEMA_VERSION)?;

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
    create_workspace_host_bindings_table(&transaction)?;
    migrate_workspace_bindings_table_to_v3(&transaction)?;
    create_workspace_remount_recoveries_table(&transaction)?;
    if matches!(version, Some(2 | 3)) {
        transaction.execute(
            "UPDATE state_components
             SET version = 4,
                 min_reader_version = 4,
                 data_json = ?1
             WHERE component_id = 'durable:workspace_bindings' AND version IN (2, 3)",
            params!["{\"format\":\"workspace_binding.v2\",\"layout_0_without_binding\":true,\"legacy_v1_readable\":true,\"target_scope\":\"workspace_id\",\"remount_recovery\":\"v1\"}"],
        )?;
    }
    validate_workspace_bindings(&transaction)?;
    transaction.commit()?;
    Ok(())
}

fn ensure_state_components_safe_before_mutation(
    connection: &Connection,
    user_version: i64,
) -> StoreResult<()> {
    let blocking_issues = inspect_state_component_issues(connection)?
        .into_iter()
        .filter(|issue| {
            let current_schema_repair = user_version == SCHEMA_VERSION
                && matches!(
                    issue,
                    StateCompatibilityIssue::MissingComponent { component_id }
                        if repairable_missing_state_component(component_id)
                );
            !current_schema_repair
                && !state_component_issue_allows_schema_migration(issue, user_version)
        })
        .collect::<Vec<_>>();

    if blocking_issues.is_empty() {
        Ok(())
    } else {
        Err(StoreError::StateCompatibility(format!(
            "state components are not safe to mutate: {blocking_issues:?}",
        )))
    }
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
            found,
            current: GENERATION_DELIVERY_COMPONENT_VERSION,
        } if component_id == "durable:generation_delivery" && matches!(*found, 1..=6)
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
            found,
            current: JOURNALS_COMPONENT_VERSION,
        } if component_id == "durable:journals" && matches!(*found, 1 | 2)
    ) || matches!(
        issue,
        StateCompatibilityIssue::OlderComponent {
            component_id,
            found,
            current: VIRTUAL_MUTATIONS_COMPONENT_VERSION,
        } if component_id == "durable:virtual_mutations" && matches!(*found, 1 | 2 | 3)
    ) || matches!(
        issue,
        StateCompatibilityIssue::OlderComponent {
            component_id,
            found: 1,
            current: ENTITY_SEARCH_COMPONENT_VERSION,
        } if component_id == "cache:entity_search"
    ) || matches!(
        issue,
        StateCompatibilityIssue::MissingComponent { component_id }
            if component_id == "projection:windows_cloud_files"
    ) || matches!(
        issue,
        StateCompatibilityIssue::MissingComponent { component_id }
            if user_version < 14 && component_id == "durable:auto_save"
    ) || matches!(
        issue,
        StateCompatibilityIssue::MissingComponent { component_id }
            if user_version < 15 && component_id == "durable:live_mode"
    ) || matches!(
        issue,
        StateCompatibilityIssue::MissingComponent { component_id }
            if user_version < 16 && component_id == "durable:metadata_discovery"
    ) || matches!(
        issue,
        StateCompatibilityIssue::MissingComponent { component_id }
            if user_version < 19 && component_id == "durable:discovery_projection"
    ) || matches!(
        issue,
        StateCompatibilityIssue::MissingComponent { component_id }
            if user_version < 22 && component_id == "durable:generation_delivery"
    ) || matches!(
        issue,
        StateCompatibilityIssue::OlderComponent {
            component_id,
            found,
            current: 4,
        } if component_id == "durable:workspace_bindings"
            && ((matches!(*found, 2 | 3) && user_version == SCHEMA_VERSION)
                || (matches!(*found, 1 | 2 | 3) && user_version < SCHEMA_VERSION))
    ) || matches!(
        issue,
        StateCompatibilityIssue::MissingComponent { component_id }
            if user_version < SCHEMA_VERSION && component_id == "durable:workspace_bindings"
    ) || matches!(
        issue,
        StateCompatibilityIssue::OlderComponent {
            component_id,
            found: 1,
            current,
        } if component_id == "durable:hosted_workspaces"
            && *current == HOSTED_WORKSPACE_ATTACHMENT_COMPONENT_VERSION as i64
            && user_version < SCHEMA_VERSION
    ) || matches!(
        issue,
        StateCompatibilityIssue::MissingComponent { component_id }
            if user_version < SCHEMA_VERSION && component_id == "durable:hosted_workspaces"
    )
}

fn migrate_workspace_bindings_schema_v21(
    connection: &mut Connection,
    user_version: i64,
) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
    if user_version >= 13 {
        ensure_state_components_safe_before_mutation(&transaction, user_version)?;
        retire_removed_state_components_in_transaction(&transaction)?;
        ensure_state_components_allow_schema_migration(&transaction, user_version)?;
    }

    migrate_virtual_projection_layout_to_v2_in_transaction(
        &transaction,
        user_version < 13,
        "projection:linux_fuse",
        ProjectionMode::LinuxFuse,
        LINUX_FUSE_PROJECTION_LAYOUT_VERSION,
        MissingProjectionComponent::Error,
    )?;
    migrate_virtual_projection_layout_to_v2_in_transaction(
        &transaction,
        user_version < 13,
        "projection:windows_cloud_files",
        ProjectionMode::WindowsCloudFiles,
        WINDOWS_CLOUD_FILES_PROJECTION_LAYOUT_VERSION,
        MissingProjectionComponent::TreatAsV1,
    )?;
    migrate_entity_search_component_to_v2(&transaction)?;
    create_workspace_host_bindings_table(&transaction)?;
    migrate_workspace_bindings_table_to_v3(&transaction)?;
    create_workspace_remount_recoveries_table(&transaction)?;
    create_hosted_workspace_tables(&transaction)?;
    if user_version < 27 {
        discard_untrusted_legacy_workspace_bindings(&transaction)?;
    }
    seed_current_state_components(&transaction)?;
    record_schema_migration(&transaction, user_version, SCHEMA_VERSION)?;
    transaction.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
    validate_workspace_bindings(&transaction)?;
    validate_hosted_workspace_storage(&transaction)?;
    transaction.commit()?;
    Ok(())
}

fn create_hosted_workspace_tables(connection: &Connection) -> StoreResult<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS hosted_workspace_attachments (
            api_origin TEXT NOT NULL,
            profile_id TEXT NOT NULL,
            credential_ref TEXT NOT NULL,
            root TEXT NOT NULL,
            profile_revision INTEGER NOT NULL CHECK (profile_revision > 0),
            layout_version INTEGER NOT NULL CHECK (layout_version > 0),
            layout_digest TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (api_origin, profile_id),
            UNIQUE (root)
        );
        CREATE TABLE IF NOT EXISTS hosted_workspace_mount_mappings (
            api_origin TEXT NOT NULL,
            profile_id TEXT NOT NULL,
            portable_mount_id TEXT NOT NULL,
            local_mount_id TEXT NOT NULL UNIQUE,
            mount_target TEXT NOT NULL,
            target_collision_key TEXT NOT NULL,
            active INTEGER NOT NULL CHECK (active IN (0, 1)),
            first_seen_revision INTEGER NOT NULL CHECK (first_seen_revision > 0),
            last_seen_revision INTEGER NOT NULL CHECK (last_seen_revision >= first_seen_revision),
            PRIMARY KEY (api_origin, profile_id, portable_mount_id),
            FOREIGN KEY (api_origin, profile_id)
                REFERENCES hosted_workspace_attachments(api_origin, profile_id) ON DELETE CASCADE
        );
        CREATE UNIQUE INDEX IF NOT EXISTS hosted_workspace_active_target_unique
            ON hosted_workspace_mount_mappings(api_origin, profile_id, target_collision_key)
            WHERE active = 1;
        CREATE TABLE IF NOT EXISTS hosted_workspace_pending_transitions (
            transition_id TEXT PRIMARY KEY,
            api_origin TEXT NOT NULL,
            profile_id TEXT NOT NULL,
            credential_ref TEXT NOT NULL,
            target_root TEXT NOT NULL,
            transition_kind TEXT NOT NULL CHECK (transition_kind IN ('attach', 'refresh', 'relocate')),
            profile_revision INTEGER NOT NULL CHECK (profile_revision > 0),
            layout_version INTEGER NOT NULL CHECK (layout_version > 0),
            layout_digest TEXT NOT NULL,
            base_profile_revision INTEGER CHECK (base_profile_revision > 0),
            base_layout_digest TEXT,
            base_root TEXT,
            created_at TEXT NOT NULL,
            UNIQUE (api_origin, profile_id),
            CHECK (
                (base_profile_revision IS NULL AND base_layout_digest IS NULL AND base_root IS NULL)
                OR
                (base_profile_revision IS NOT NULL AND base_layout_digest IS NOT NULL AND base_root IS NOT NULL)
            )
        );
        CREATE TABLE IF NOT EXISTS hosted_workspace_pending_mounts (
            transition_id TEXT NOT NULL,
            portable_mount_id TEXT NOT NULL,
            local_mount_id TEXT NOT NULL,
            mount_target TEXT NOT NULL,
            target_collision_key TEXT NOT NULL,
            first_seen_revision INTEGER NOT NULL CHECK (first_seen_revision > 0),
            PRIMARY KEY (transition_id, portable_mount_id),
            UNIQUE (transition_id, local_mount_id),
            UNIQUE (transition_id, target_collision_key),
            FOREIGN KEY (transition_id)
                REFERENCES hosted_workspace_pending_transitions(transition_id) ON DELETE CASCADE
        );
        CREATE TABLE IF NOT EXISTS hosted_workspace_pending_cleanups (
            cleanup_id TEXT PRIMARY KEY,
            api_origin TEXT NOT NULL,
            profile_id TEXT NOT NULL,
            credential_ref TEXT NOT NULL,
            root TEXT NOT NULL,
            profile_revision INTEGER NOT NULL CHECK (profile_revision > 0),
            layout_version INTEGER NOT NULL CHECK (layout_version > 0),
            layout_digest TEXT NOT NULL,
            created_at TEXT NOT NULL,
            UNIQUE (api_origin, profile_id),
            UNIQUE (root),
            FOREIGN KEY (api_origin, profile_id)
                REFERENCES hosted_workspace_attachments(api_origin, profile_id) ON DELETE CASCADE
        );",
    )?;
    Ok(())
}

fn validate_hosted_workspace_storage(connection: &Connection) -> StoreResult<()> {
    for table in [
        "hosted_workspace_attachments",
        "hosted_workspace_mount_mappings",
        "hosted_workspace_pending_transitions",
        "hosted_workspace_pending_mounts",
        "hosted_workspace_pending_cleanups",
    ] {
        if !table_exists(connection, table)? {
            return Err(StoreError::StateCompatibility(format!(
                "required hosted workspace table `{table}` is missing"
            )));
        }
    }
    let mapping_owner_count: i64 = connection.query_row(
        "SELECT COUNT(*)
         FROM hosted_workspace_mount_mappings m
         LEFT JOIN hosted_workspace_attachments a
           ON a.api_origin = m.api_origin AND a.profile_id = m.profile_id
         WHERE a.profile_id IS NULL",
        [],
        |row| row.get(0),
    )?;
    if mapping_owner_count != 0 {
        return Err(StoreError::InvalidState(
            "hosted workspace storage contains orphan mappings".to_string(),
        ));
    }
    let cleanup_owner_count: i64 = connection.query_row(
        "SELECT COUNT(*)
         FROM hosted_workspace_pending_cleanups c
         LEFT JOIN hosted_workspace_attachments a
           ON a.api_origin = c.api_origin AND a.profile_id = c.profile_id
         WHERE a.profile_id IS NULL",
        [],
        |row| row.get(0),
    )?;
    if cleanup_owner_count != 0 {
        return Err(StoreError::InvalidState(
            "hosted workspace storage contains orphan relocation cleanups".to_string(),
        ));
    }
    Ok(())
}

fn create_workspace_bindings_table(connection: &Connection) -> StoreResult<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS workspace_bindings (
            mount_id TEXT PRIMARY KEY,
            workspace_id TEXT,
            binding_json TEXT NOT NULL,
            target_collision_key TEXT NOT NULL,
            FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE,
            FOREIGN KEY (workspace_id) REFERENCES workspace_host_bindings(workspace_id)
        );
        CREATE UNIQUE INDEX IF NOT EXISTS workspace_bindings_workspace_target_unique
            ON workspace_bindings(workspace_id, target_collision_key)
            WHERE workspace_id IS NOT NULL;
        CREATE UNIQUE INDEX IF NOT EXISTS workspace_bindings_legacy_target_unique
            ON workspace_bindings(target_collision_key)
            WHERE workspace_id IS NULL;",
    )?;
    Ok(())
}

fn create_workspace_remount_recoveries_table(connection: &Connection) -> StoreResult<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS workspace_remount_recoveries (
            recovery_id TEXT PRIMARY KEY,
            mount_id TEXT NOT NULL,
            committed INTEGER NOT NULL CHECK (committed IN (0, 1)),
            FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
        );
        CREATE UNIQUE INDEX IF NOT EXISTS workspace_remount_recoveries_mount_unique
            ON workspace_remount_recoveries(mount_id);",
    )?;
    Ok(())
}

fn migrate_workspace_bindings_table_to_v3(connection: &Connection) -> StoreResult<()> {
    if !table_exists(connection, "workspace_bindings")? {
        create_workspace_bindings_table(connection)?;
        return Ok(());
    }
    if column_exists(connection, "workspace_bindings", "workspace_id")? {
        create_workspace_bindings_table(connection)?;
        return Ok(());
    }

    let rows = {
        let mut statement = connection.prepare(
            "SELECT mount_id, binding_json, target_collision_key
             FROM workspace_bindings
             ORDER BY mount_id",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    connection.execute_batch(
        "ALTER TABLE workspace_bindings RENAME TO workspace_bindings_component_v2;",
    )?;
    create_workspace_bindings_table(connection)?;
    for (mount_id, binding_json, collision_key) in rows {
        let binding = workspace_binding_from_row((binding_json.clone(), collision_key.clone()))?;
        connection.execute(
            "INSERT INTO workspace_bindings (
                mount_id, workspace_id, binding_json, target_collision_key
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                mount_id,
                binding.workspace_id().map(WorkspaceId::as_str),
                binding_json,
                collision_key,
            ],
        )?;
    }
    connection.execute_batch("DROP TABLE workspace_bindings_component_v2;")?;
    Ok(())
}

fn create_workspace_host_bindings_table(connection: &Connection) -> StoreResult<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS workspace_host_bindings (
            workspace_id TEXT PRIMARY KEY,
            binding_json TEXT NOT NULL
        );",
    )?;
    Ok(())
}

fn discard_untrusted_legacy_workspace_bindings(connection: &Connection) -> StoreResult<()> {
    create_workspace_bindings_table(connection)?;
    // Version 1 bindings were inferred without a persisted trusted workspace
    // identity. Schema migration cannot distinguish them from coordinator-owned
    // records, so every legacy row remains layout 0 until an owning coordinator
    // performs an atomic migration with its trusted root.
    connection.execute("DELETE FROM workspace_bindings", [])?;
    validate_workspace_bindings(connection)?;
    Ok(())
}

fn validate_workspace_bindings(connection: &Connection) -> StoreResult<()> {
    if !table_exists(connection, "workspace_bindings")? {
        return Err(StoreError::StateCompatibility(
            "missing required non-rebuildable workspace binding table".to_string(),
        ));
    }
    if !table_exists(connection, "workspace_host_bindings")? {
        return Err(StoreError::StateCompatibility(
            "missing required non-rebuildable workspace host binding table".to_string(),
        ));
    }
    let mut host_statement = connection.prepare(
        "SELECT workspace_id, binding_json
         FROM workspace_host_bindings
         ORDER BY workspace_id",
    )?;
    let host_rows = host_statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut host_ids = BTreeSet::new();
    for row in host_rows {
        let (workspace_id, binding_json) = row?;
        let host_binding = workspace_host_binding_from_row(&binding_json)?;
        if host_binding.workspace_id().as_str() != workspace_id {
            return Err(StoreError::InvalidState(
                "workspace host binding key does not match its metadata".to_string(),
            ));
        }
        host_ids.insert(workspace_id);
    }
    let mut statement = connection.prepare(
        "SELECT mount_id, workspace_id, binding_json, target_collision_key
         FROM workspace_bindings
         ORDER BY mount_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (mount_id, workspace_id, binding_json, collision_key) = row?;
        let binding =
            workspace_binding_from_persisted_row((workspace_id, binding_json, collision_key))?;
        if let Some(workspace_id) = binding.workspace_id()
            && !host_ids.contains(workspace_id.as_str())
        {
            return Err(StoreError::InvalidState(format!(
                "workspace binding for mount `{mount_id}` references missing workspace `{}`",
                workspace_id.as_str()
            )));
        }
    }
    Ok(())
}

fn workspace_host_binding_from_row(binding_json: &str) -> StoreResult<WorkspaceHostBinding> {
    serde_json::from_str::<WorkspaceHostBinding>(binding_json).map_err(|error| {
        StoreError::StateCompatibility(format!(
            "workspace host binding metadata is not readable by this binary; update required or repair invalid metadata: {error}"
        ))
    })
}

fn workspace_binding_from_row(row: (String, String)) -> StoreResult<WorkspaceBinding> {
    let binding = serde_json::from_str::<WorkspaceBinding>(&row.0).map_err(|error| {
        StoreError::StateCompatibility(format!(
            "workspace binding metadata is not readable by this binary; update required or repair invalid metadata: {error}"
        ))
    })?;
    if binding.collision_key() != row.1 {
        return Err(StoreError::InvalidState(
            "workspace binding target collision key does not match its metadata".to_string(),
        ));
    }
    Ok(binding)
}

fn workspace_binding_from_persisted_row(
    row: (Option<String>, String, String),
) -> StoreResult<WorkspaceBinding> {
    let (stored_workspace_id, binding_json, collision_key) = row;
    let binding = workspace_binding_from_row((binding_json, collision_key))?;
    if binding.workspace_id().map(WorkspaceId::as_str) != stored_workspace_id.as_deref() {
        return Err(StoreError::InvalidState(
            "workspace binding identity column does not match its metadata".to_string(),
        ));
    }
    Ok(binding)
}

fn mount_from_connection(
    connection: &Connection,
    mount_id: &MountId,
) -> StoreResult<Option<MountConfig>> {
    connection
        .query_row(
            "SELECT mount_id, connector, root, remote_root_id, read_only, projection_json, connection_id, settings_json
             FROM mounts WHERE mount_id = ?1",
            params![mount_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                ))
            },
        )
        .optional()?
        .map(mount_from_row)
        .transpose()
}

fn save_mount_row(connection: &Connection, mount: &MountConfig) -> StoreResult<()> {
    ensure_connector_mount_id_available(connection, &mount.mount_id)?;
    connection.execute(
        "INSERT INTO mounts (mount_id, connector, root, remote_root_id, read_only, projection_json, connection_id, settings_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(mount_id) DO UPDATE SET
            connector = excluded.connector,
            root = excluded.root,
            remote_root_id = excluded.remote_root_id,
            read_only = excluded.read_only,
            projection_json = excluded.projection_json,
            connection_id = excluded.connection_id,
            settings_json = excluded.settings_json",
        params![
            mount.mount_id.as_str(),
            &mount.connector,
            path_to_text(&mount.root),
            mount.remote_root_id.as_ref().map(|remote_id| remote_id.0.as_str()),
            bool_to_int(mount.read_only),
            to_json(&mount.projection)?,
            mount.connection_id.as_ref().map(|connection_id| connection_id.0.as_str()),
            mount.settings_json.as_str(),
        ],
    )?;
    Ok(())
}

fn validate_workspace_binding_commit(
    mount: &MountConfig,
    host_binding: &WorkspaceHostBinding,
    record: &WorkspaceBindingRecord,
) -> StoreResult<()> {
    let workspace_id = record.binding.workspace_id().ok_or_else(|| {
        StoreError::InvalidState(
            "atomic workspace commit requires a layout-1 mount binding".to_string(),
        )
    })?;
    if record.mount_id != mount.mount_id || workspace_id != host_binding.workspace_id() {
        return Err(StoreError::InvalidState(
            "workspace mount binding does not match its mount or host identity".to_string(),
        ));
    }
    let resolved_root = host_binding.mount_root(record.binding.mount_target());
    if !host_paths_equivalent(
        crate::WorkspaceHostPlatform::current(),
        &resolved_root,
        &mount.root,
    ) {
        return Err(StoreError::InvalidState(format!(
            "workspace binding for mount `{}` resolves to `{}` instead of its preserved root `{}`",
            record.mount_id.as_str(),
            resolved_root.display(),
            mount.root.display()
        )));
    }
    WorkspaceHostBindingResolver::current()
        .validate_persistent_mount_root(host_binding, &resolved_root, record.binding.mount_target())
        .map_err(|error| StoreError::InvalidState(error.to_string()))
}

fn commit_workspace_binding_in_transaction(
    connection: &Connection,
    mount: &MountConfig,
    host_binding: &WorkspaceHostBinding,
    record: &WorkspaceBindingRecord,
) -> StoreResult<()> {
    validate_workspace_binding_commit(mount, host_binding, record)?;
    let workspace_id = record
        .binding
        .workspace_id()
        .expect("validated layout-1 workspace binding");
    let existing_binding = connection
        .query_row(
            "SELECT workspace_id, binding_json, target_collision_key
             FROM workspace_bindings WHERE mount_id = ?1",
            params![record.mount_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?
        .map(workspace_binding_from_persisted_row)
        .transpose()?;
    let exact_replay = existing_binding.as_ref() == Some(&record.binding);
    let safe_v1_upgrade = existing_binding.as_ref().is_some_and(|existing| {
        existing.workspace_id().is_none()
            && existing.mount_target() == record.binding.mount_target()
    });
    if let Some(existing) = &existing_binding
        && !exact_replay
        && !safe_v1_upgrade
    {
        return Err(StoreError::WorkspaceBindingTargetImmutable {
            mount_id: record.mount_id.clone(),
            existing_target: existing.mount_target().as_str().to_string(),
            requested_target: record.binding.mount_target().as_str().to_string(),
        });
    }

    let existing_host = connection
        .query_row(
            "SELECT binding_json FROM workspace_host_bindings WHERE workspace_id = ?1",
            params![workspace_id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|json| workspace_host_binding_from_row(&json))
        .transpose()?;
    if let Some(existing_host) = &existing_host {
        if !host_paths_equivalent(
            crate::WorkspaceHostPlatform::current(),
            existing_host.trusted_workspace_root(),
            host_binding.trusted_workspace_root(),
        ) || existing_host.projection_identity() != host_binding.projection_identity()
        {
            return Err(StoreError::InvalidState(format!(
                "workspace `{}` host root or projection identity is immutable outside an owning coordinator",
                workspace_id.as_str()
            )));
        }
        let expected_sequence = if exact_replay {
            existing_host.layout_sequence()
        } else {
            existing_host
                .next_layout_sequence()
                .map_err(|error| StoreError::InvalidState(error.to_string()))?
        };
        if host_binding.layout_sequence() != expected_sequence {
            return Err(StoreError::InvalidState(format!(
                "workspace `{}` layout sequence must be {expected_sequence}, got {}",
                workspace_id.as_str(),
                host_binding.layout_sequence()
            )));
        }
    } else if host_binding.layout_sequence() != 1 {
        return Err(StoreError::InvalidState(format!(
            "new workspace `{}` must start at layout sequence 1",
            workspace_id.as_str()
        )));
    }
    let persisted_host = if let Some(existing_host) = &existing_host {
        WorkspaceHostBinding::new(
            crate::WorkspaceHostPlatform::current(),
            workspace_id.clone(),
            existing_host.trusted_workspace_root().to_path_buf(),
            host_binding.projection_identity().clone(),
            host_binding.layout_sequence(),
        )
        .map_err(|error| StoreError::InvalidState(error.to_string()))?
    } else {
        host_binding.clone()
    };

    if !exact_replay {
        let collision_key = record.binding.collision_key();
        let mut statement = connection.prepare(
            "SELECT m.mount_id, m.root, b.workspace_id, b.binding_json, b.target_collision_key
             FROM mounts m
             LEFT JOIN workspace_bindings b ON b.mount_id = m.mount_id
             WHERE m.mount_id <> ?1
             ORDER BY m.mount_id",
        )?;
        let rows = statement.query_map(params![record.mount_id.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                PathBuf::from(row.get::<_, String>(1)?),
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        for row in rows {
            let (mount_id, root, stored_workspace_id, binding_json, stored_collision_key) = row?;
            let existing_collision_key = match (binding_json, stored_collision_key) {
                (Some(binding_json), Some(stored_collision_key)) => {
                    let binding = workspace_binding_from_persisted_row((
                        stored_workspace_id,
                        binding_json,
                        stored_collision_key,
                    ))?;
                    match binding.workspace_id() {
                        Some(existing_workspace_id) if existing_workspace_id == workspace_id => {
                            Some(binding.collision_key())
                        }
                        Some(_) => None,
                        None => legacy_mount_collision_key_for_host(&persisted_host, &root),
                    }
                }
                (None, None) => legacy_mount_collision_key_for_host(&persisted_host, &root),
                _ => {
                    return Err(StoreError::InvalidState(
                        "workspace binding row is partially present".to_string(),
                    ));
                }
            };
            if existing_collision_key.as_deref() == Some(collision_key.as_str()) {
                return Err(StoreError::WorkspaceMountTargetCollision {
                    target: record.binding.mount_target().as_str().to_string(),
                    existing_mount_id: MountId(mount_id),
                });
            }
        }
    }

    connection.execute(
        "INSERT INTO workspace_host_bindings (workspace_id, binding_json)
         VALUES (?1, ?2)
         ON CONFLICT(workspace_id) DO UPDATE SET binding_json = excluded.binding_json",
        params![workspace_id.as_str(), to_json(&persisted_host)?],
    )?;
    if !exact_replay {
        connection.execute(
            "INSERT INTO workspace_bindings (
                mount_id, workspace_id, binding_json, target_collision_key
             ) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(mount_id) DO UPDATE SET
                workspace_id = excluded.workspace_id,
                binding_json = excluded.binding_json,
                target_collision_key = excluded.target_collision_key",
            params![
                record.mount_id.as_str(),
                workspace_id.as_str(),
                to_json(&record.binding)?,
                record.binding.collision_key(),
            ],
        )?;
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
        settings_json: row.7,
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

fn metadata_discovery_job_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<MetadataDiscoveryJobRow> {
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

fn metadata_discovery_job_from_row(
    row: MetadataDiscoveryJobRow,
) -> StoreResult<MetadataDiscoveryJobRecord> {
    let depth = u32::try_from(row.3)
        .map_err(|_| StoreError::Database(format!("invalid metadata discovery depth {}", row.3)))?;
    let attempts = u32::try_from(row.4).map_err(|_| {
        StoreError::Database(format!(
            "invalid metadata discovery attempt count {}",
            row.4
        ))
    })?;

    Ok(MetadataDiscoveryJobRecord {
        mount_id: MountId(row.0),
        container_identifier: row.1,
        priority: from_json::<MetadataDiscoveryPriority>(&row.2)?,
        depth,
        attempts,
        last_error: row.5,
        created_at: row.6,
        updated_at: row.7,
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
        content_path: row.8.map(|path| native_path_from_text(&path)).transpose()?,
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
        row.get(7)?,
        row.get(8)?,
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
        metadata: journal_metadata_from_json(&row.7)?,
        readable_diff: row
            .8
            .as_deref()
            .map(from_json::<ReadableDiffOutput>)
            .transpose()?,
    })
}

fn journal_metadata_from_json(value: &str) -> StoreResult<JournalMetadata> {
    if value == "{}" {
        Ok(JournalMetadata::default())
    } else {
        from_json::<JournalMetadata>(value)
    }
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
        | JournalApplyEffect::UpdatedEntityBody {
            operation_index, ..
        }
        | JournalApplyEffect::UpdatedProperties {
            operation_index, ..
        }
        | JournalApplyEffect::MovedEntity {
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
        if retired_state_component_is_readable(&component) {
            continue;
        }
        if component.required {
            issues.push(StateCompatibilityIssue::UnknownRequiredComponent {
                component_id: component.component_id,
                version: component.version,
            });
        }
    }

    Ok(issues)
}

fn retired_state_component_is_readable(component: &StateComponentRecord) -> bool {
    component.component_id == RETIRED_NOTION_WORKSPACE_ROOTS_COMPONENT_ID
        && component.version <= RETIRED_NOTION_WORKSPACE_ROOTS_SUPPORTED_VERSION
        && component.min_reader_version <= RETIRED_NOTION_WORKSPACE_ROOTS_SUPPORTED_VERSION
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

fn migrate_generation_delivery_journals_to_mount_relation(
    connection: &Connection,
) -> StoreResult<()> {
    connection.execute_batch(
        "BEGIN IMMEDIATE;
         ALTER TABLE generation_apply_outcomes RENAME TO generation_apply_outcomes_v21;
         ALTER TABLE generation_apply_journals RENAME TO generation_apply_journals_v21;
         DROP INDEX IF EXISTS generation_apply_one_active_per_source;
         DROP INDEX IF EXISTS generation_apply_one_active_per_mount;

         CREATE TABLE generation_apply_journals (
            delta_id TEXT PRIMARY KEY,
            mount_id TEXT NOT NULL,
            source_connection_id TEXT NOT NULL,
            base_generation_id TEXT NOT NULL,
            target_generation_id TEXT NOT NULL,
            delta_json TEXT NOT NULL,
            receipt_json TEXT NOT NULL,
            receipt_sha256 TEXT NOT NULL,
            selected_capabilities_json TEXT NOT NULL DEFAULT '{}',
            selection_binding TEXT NOT NULL DEFAULT 'pre_binding_unknown'
                CHECK (selection_binding IN ('bound', 'pre_binding_unknown', 'pre_binding_completed')),
            acknowledgment_required INTEGER NOT NULL DEFAULT 0
                CHECK (acknowledgment_required IN (0, 1)),
            acknowledged_at TEXT,
            stage_root TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('staged', 'applying', 'completed')),
            active INTEGER NOT NULL CHECK (active IN (0, 1)),
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            completed_at TEXT,
            CHECK (
                (active = 0 AND status = 'completed' AND completed_at IS NOT NULL)
                OR (active = 1 AND status IN ('staged', 'applying') AND completed_at IS NULL)
            ),
            FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
         );

         INSERT INTO generation_apply_journals (
            delta_id, mount_id, source_connection_id, base_generation_id,
            target_generation_id, delta_json, receipt_json, receipt_sha256,
            stage_root, status, active, created_at, updated_at, completed_at
         )
         SELECT delta_id, json_extract(delta_json, '$.mount_id'), source_connection_id,
                base_generation_id, target_generation_id, delta_json, receipt_json,
                receipt_sha256, stage_root, status, active, created_at, updated_at,
                completed_at
         FROM generation_apply_journals_v21;

         CREATE UNIQUE INDEX generation_apply_one_active_per_mount
         ON generation_apply_journals(mount_id)
         WHERE active = 1;

         CREATE TABLE generation_apply_outcomes (
            delta_id TEXT NOT NULL,
            entry_index INTEGER NOT NULL CHECK (entry_index >= 0),
            outcome_json TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (delta_id, entry_index),
            FOREIGN KEY (delta_id) REFERENCES generation_apply_journals(delta_id) ON DELETE CASCADE
         );
         INSERT INTO generation_apply_outcomes
         SELECT delta_id, entry_index, outcome_json, updated_at
         FROM generation_apply_outcomes_v21;

         DROP TABLE generation_apply_outcomes_v21;
         DROP TABLE generation_apply_journals_v21;
         COMMIT;",
    )?;
    Ok(())
}

fn create_generation_delivery_tables(connection: &Connection) -> StoreResult<()> {
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS observed_generations (
            mount_id TEXT NOT NULL,
            source_connection_id TEXT NOT NULL,
            generation_id TEXT NOT NULL,
            inventory_sha256 TEXT NOT NULL,
            workspace_layout_version INTEGER NOT NULL CHECK (workspace_layout_version > 0),
            workspace_layout_digest TEXT NOT NULL,
            last_receipt_sha256 TEXT,
            updated_at TEXT NOT NULL,
            refresh_mode TEXT NOT NULL
                CHECK (refresh_mode IN ('generation_delta_v1', 'full_export_only')),
            PRIMARY KEY (mount_id, source_connection_id),
            FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS generation_paths (
            mount_id TEXT NOT NULL,
            source_connection_id TEXT NOT NULL,
            projection_id TEXT NOT NULL,
            logical_path TEXT NOT NULL,
            local_logical_path TEXT NOT NULL,
            base_generation_id TEXT NOT NULL,
            base_identity_json TEXT,
            base_payload_delta_id TEXT,
            base_payload_entry_index INTEGER CHECK (base_payload_entry_index >= 0),
            conflict_payload_delta_id TEXT,
            conflict_payload_entry_index INTEGER CHECK (conflict_payload_entry_index >= 0),
            state TEXT NOT NULL CHECK (state IN ('clean', 'dirty', 'conflicted')),
            incoming_identity_json TEXT,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (mount_id, projection_id),
            UNIQUE (mount_id, logical_path),
            UNIQUE (mount_id, local_logical_path),
            FOREIGN KEY (mount_id, source_connection_id)
                REFERENCES observed_generations(mount_id, source_connection_id)
                ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS generation_apply_journals (
            delta_id TEXT PRIMARY KEY,
            mount_id TEXT NOT NULL,
            source_connection_id TEXT NOT NULL,
            base_generation_id TEXT NOT NULL,
            target_generation_id TEXT NOT NULL,
            delta_json TEXT NOT NULL,
            receipt_json TEXT NOT NULL,
            receipt_sha256 TEXT NOT NULL,
            selected_capabilities_json TEXT NOT NULL DEFAULT '{}',
            selection_binding TEXT NOT NULL DEFAULT 'bound'
                CHECK (selection_binding IN ('bound', 'pre_binding_unknown', 'pre_binding_completed')),
            acknowledgment_required INTEGER NOT NULL DEFAULT 0
                CHECK (acknowledgment_required IN (0, 1)),
            acknowledged_at TEXT,
            stage_root TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('staged', 'applying', 'completed')),
            active INTEGER NOT NULL CHECK (active IN (0, 1)),
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            completed_at TEXT,
            CHECK (
                (active = 0 AND status = 'completed' AND completed_at IS NOT NULL)
                OR (active = 1 AND status IN ('staged', 'applying') AND completed_at IS NULL)
            ),
            FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS generation_apply_outcomes (
            delta_id TEXT NOT NULL,
            entry_index INTEGER NOT NULL CHECK (entry_index >= 0),
            outcome_json TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (delta_id, entry_index),
            FOREIGN KEY (delta_id) REFERENCES generation_apply_journals(delta_id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS generation_inode_evidence (
            delta_id TEXT NOT NULL,
            entry_index INTEGER NOT NULL CHECK (entry_index >= 0),
            mount_id TEXT NOT NULL,
            logical_path TEXT NOT NULL,
            evidence_name TEXT NOT NULL,
            expected_sha256 TEXT NOT NULL,
            byte_length INTEGER NOT NULL CHECK (byte_length >= 0),
            visible_evidence_name TEXT,
            visible_expected_sha256 TEXT,
            visible_byte_length INTEGER CHECK (visible_byte_length >= 0),
            base_payload_delta_id TEXT,
            base_payload_entry_index INTEGER CHECK (base_payload_entry_index >= 0),
            resolved_at TEXT,
            created_at TEXT NOT NULL,
            PRIMARY KEY (delta_id, entry_index),
            FOREIGN KEY (delta_id) REFERENCES generation_apply_journals(delta_id) ON DELETE CASCADE,
            FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
        );
        ",
    )?;
    ensure_generation_active_index_for_schema(connection)?;
    Ok(())
}

fn ensure_generation_active_index_for_schema(connection: &Connection) -> StoreResult<()> {
    if column_exists(connection, "generation_apply_journals", "mount_id")? {
        if !index_exists(connection, "generation_apply_one_active_per_source")? {
            connection.execute_batch(
                "CREATE UNIQUE INDEX IF NOT EXISTS generation_apply_one_active_per_mount
                 ON generation_apply_journals(mount_id) WHERE active = 1;",
            )?;
        }
    } else {
        connection.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS generation_apply_one_active_per_source
             ON generation_apply_journals(source_connection_id) WHERE active = 1;",
        )?;
    }
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

fn migrate_virtual_mutations_component_to_v4(connection: &Connection) -> StoreResult<()> {
    migrate_state_component_to_current(connection, "durable:virtual_mutations")
}

fn migrate_generation_delivery_component_to_v7(connection: &Connection) -> StoreResult<()> {
    migrate_state_component_to_current(connection, "durable:generation_delivery")
}

fn migrate_generation_delivery_to_v7(
    connection: &Connection,
    record_from_schema: Option<i64>,
    update_component: bool,
) -> StoreResult<()> {
    if record_from_schema.is_none()
        && generation_delivery_storage_v7_is_complete(connection)?
        && (!update_component || generation_delivery_component_is_current(connection)?)
    {
        return Ok(());
    }
    let selected_capabilities_preexisted = column_exists(
        connection,
        "generation_apply_journals",
        "selected_capabilities_json",
    )?;
    let finalized_inode_evidence_preexisted =
        column_exists(
            connection,
            "generation_inode_evidence",
            "visible_evidence_name",
        )? && column_exists(connection, "generation_inode_evidence", "resolved_at")?;
    let transaction = connection.unchecked_transaction()?;
    migrate_generation_delivery_storage_to_v2(&transaction)?;
    migrate_generation_delivery_storage_to_v3(&transaction)?;
    migrate_generation_delivery_storage_to_v4(&transaction)?;
    migrate_generation_delivery_storage_to_v5(&transaction)?;
    migrate_generation_delivery_storage_to_v6(
        &transaction,
        selected_capabilities_preexisted,
        finalized_inode_evidence_preexisted,
    )?;
    verify_generation_delivery_storage_v6(&transaction)?;
    migrate_generation_delivery_storage_to_v7(&transaction)?;
    verify_generation_delivery_storage_v7(&transaction)?;
    if update_component {
        migrate_generation_delivery_component_to_v7(&transaction)?;
    }
    if let Some(from) = record_from_schema {
        record_schema_migration(&transaction, from, SCHEMA_VERSION)?;
    }
    transaction.commit()?;
    Ok(())
}

fn generation_delivery_component_is_current(connection: &Connection) -> StoreResult<bool> {
    connection
        .query_row(
            "SELECT version = ?2 AND min_reader_version = ?2
             FROM state_components WHERE component_id = ?1",
            params![
                "durable:generation_delivery",
                GENERATION_DELIVERY_COMPONENT_VERSION
            ],
            |row| row.get(0),
        )
        .optional()
        .map(|value| value.unwrap_or(false))
        .map_err(Into::into)
}

fn add_column_if_missing(
    connection: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> StoreResult<()> {
    if !column_exists(connection, table, column)? {
        connection.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition};"
        ))?;
    }
    Ok(())
}

fn migrate_generation_delivery_storage_to_v2(connection: &Connection) -> StoreResult<()> {
    add_column_if_missing(
        connection,
        "generation_paths",
        "base_payload_delta_id",
        "TEXT",
    )?;
    add_column_if_missing(
        connection,
        "generation_paths",
        "base_payload_entry_index",
        "INTEGER CHECK (base_payload_entry_index >= 0)",
    )?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS generation_inode_evidence (
            delta_id TEXT NOT NULL,
            entry_index INTEGER NOT NULL CHECK (entry_index >= 0),
            mount_id TEXT NOT NULL,
            logical_path TEXT NOT NULL,
            evidence_name TEXT NOT NULL,
            expected_sha256 TEXT NOT NULL,
            byte_length INTEGER NOT NULL CHECK (byte_length >= 0),
            visible_evidence_name TEXT,
            visible_expected_sha256 TEXT,
            visible_byte_length INTEGER CHECK (visible_byte_length >= 0),
            resolved_at TEXT,
            created_at TEXT NOT NULL,
            PRIMARY KEY (delta_id, entry_index),
            FOREIGN KEY (delta_id) REFERENCES generation_apply_journals(delta_id) ON DELETE CASCADE,
            FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
         );",
    )?;
    Ok(())
}

fn migrate_generation_delivery_storage_to_v3(connection: &Connection) -> StoreResult<()> {
    add_column_if_missing(connection, "generation_paths", "local_logical_path", "TEXT")?;
    connection.execute(
        "UPDATE generation_paths SET local_logical_path = logical_path
         WHERE local_logical_path IS NULL",
        [],
    )?;
    add_column_if_missing(
        connection,
        "generation_paths",
        "conflict_payload_delta_id",
        "TEXT",
    )?;
    add_column_if_missing(
        connection,
        "generation_paths",
        "conflict_payload_entry_index",
        "INTEGER CHECK (conflict_payload_entry_index >= 0)",
    )?;
    add_column_if_missing(
        connection,
        "generation_inode_evidence",
        "base_payload_delta_id",
        "TEXT",
    )?;
    add_column_if_missing(
        connection,
        "generation_inode_evidence",
        "base_payload_entry_index",
        "INTEGER CHECK (base_payload_entry_index >= 0)",
    )?;
    Ok(())
}

fn migrate_generation_delivery_storage_to_v4(connection: &Connection) -> StoreResult<()> {
    add_column_if_missing(
        connection,
        "generation_apply_journals",
        "acknowledgment_required",
        "INTEGER NOT NULL DEFAULT 0 CHECK (acknowledgment_required IN (0, 1))",
    )?;
    add_column_if_missing(
        connection,
        "generation_apply_journals",
        "acknowledged_at",
        "TEXT",
    )?;
    add_column_if_missing(
        connection,
        "generation_inode_evidence",
        "visible_evidence_name",
        "TEXT",
    )?;
    add_column_if_missing(
        connection,
        "generation_inode_evidence",
        "visible_expected_sha256",
        "TEXT",
    )?;
    add_column_if_missing(
        connection,
        "generation_inode_evidence",
        "visible_byte_length",
        "INTEGER CHECK (visible_byte_length >= 0)",
    )?;
    Ok(())
}

fn migrate_generation_delivery_storage_to_v5(connection: &Connection) -> StoreResult<()> {
    add_column_if_missing(
        connection,
        "generation_apply_journals",
        "selected_capabilities_json",
        "TEXT NOT NULL DEFAULT '{}'",
    )?;
    connection.execute(
        "UPDATE generation_apply_journals
         SET selected_capabilities_json =
             '{\"format_version\":1,\"minimum_reader_version\":1,\"terminal_receipt_acknowledgments\":true}'
         WHERE acknowledgment_required = 1
           AND selected_capabilities_json = '{}'",
        [],
    )?;
    add_column_if_missing(
        connection,
        "generation_inode_evidence",
        "resolved_at",
        "TEXT",
    )?;
    connection.execute(
        "UPDATE generation_inode_evidence
         SET resolved_at = (
             SELECT outcomes.updated_at
             FROM generation_apply_outcomes AS outcomes
             WHERE outcomes.delta_id = generation_inode_evidence.delta_id
               AND outcomes.entry_index = generation_inode_evidence.entry_index
               AND json_extract(outcomes.outcome_json, '$.kind') = 'merged'
         )
         WHERE resolved_at IS NULL
           AND visible_evidence_name IS NOT NULL
           AND EXISTS (
               SELECT 1 FROM generation_apply_outcomes AS outcomes
               WHERE outcomes.delta_id = generation_inode_evidence.delta_id
                 AND outcomes.entry_index = generation_inode_evidence.entry_index
                 AND json_extract(outcomes.outcome_json, '$.kind') = 'merged'
           )",
        [],
    )?;
    Ok(())
}

fn migrate_generation_delivery_storage_to_v6(
    connection: &Connection,
    selected_capabilities_preexisted: bool,
    finalized_inode_evidence_preexisted: bool,
) -> StoreResult<()> {
    let prior_component_version = if table_exists(connection, "state_components")? {
        connection
            .query_row(
                "SELECT version FROM state_components
                 WHERE component_id = 'durable:generation_delivery'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
    } else {
        None
    };
    let selected_capabilities_preexisted = selected_capabilities_preexisted
        && prior_component_version.is_some_and(|version| version >= 5);
    add_column_if_missing(
        connection,
        "generation_apply_journals",
        "selection_binding",
        "TEXT NOT NULL DEFAULT 'pre_binding_unknown' CHECK (selection_binding IN ('bound', 'pre_binding_unknown', 'pre_binding_completed'))",
    )?;

    let candidates = {
        let mut statement = connection.prepare(
            "SELECT delta_id, status, active, selected_capabilities_json,
                    acknowledgment_required
             FROM generation_apply_journals
             WHERE selection_binding = 'pre_binding_unknown'
             ORDER BY delta_id",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, bool>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };

    for (delta_id, status, active, selected_json, acknowledgment_required) in candidates {
        let known_legacy_binding = (prior_component_version.is_some_and(|version| version <= 3)
            || (prior_component_version == Some(5)
                && finalized_inode_evidence_preexisted
                && !selected_capabilities_preexisted))
            && !acknowledgment_required;
        let recoverable_persisted_binding = selected_capabilities_preexisted
            && from_json::<GenerationTransportCapabilities>(&selected_json)
                .ok()
                .filter(|selected| {
                    (selected.body_windows.is_some() || selected.generation_pin_leases.is_some())
                        && selected.terminal_receipt_acknowledgments == acknowledgment_required
                        && selected.validate().is_ok()
                })
                .is_some();
        let recoverable_binding = known_legacy_binding || recoverable_persisted_binding;
        if recoverable_binding {
            connection.execute(
                "UPDATE generation_apply_journals
                 SET selected_capabilities_json = CASE
                         WHEN ?2 THEN '{}' ELSE selected_capabilities_json END,
                     selection_binding = 'bound'
                 WHERE delta_id = ?1",
                params![delta_id, known_legacy_binding],
            )?;
        } else if status == "completed" && !active {
            connection.execute(
                "UPDATE generation_apply_journals
                 SET selection_binding = 'pre_binding_completed'
                 WHERE delta_id = ?1",
                params![delta_id],
            )?;
        } else {
            return Err(StoreError::InvalidState(format!(
                "active generation apply `{delta_id}` has no complete immutable transport selection; complete it with the prerelease database reader before retrying this migration"
            )));
        }
    }
    Ok(())
}

fn migrate_generation_delivery_storage_to_v7(connection: &Connection) -> StoreResult<()> {
    if generation_delivery_storage_v7_is_complete(connection)? {
        return Ok(());
    }
    let already_multi_source =
        column_exists(connection, "generation_paths", "source_connection_id")?
            && table_primary_key_columns(connection, "observed_generations")?
                == ["mount_id", "source_connection_id"];
    if already_multi_source {
        add_column_if_missing(
            connection,
            "observed_generations",
            "refresh_mode",
            "TEXT NOT NULL DEFAULT 'full_export_only' CHECK (refresh_mode IN ('generation_delta_v1', 'full_export_only'))",
        )?;
        backfill_generation_refresh_modes(connection)?;
        migrate_generation_active_index_to_mount(connection)?;
        return Ok(());
    }
    if !generation_delivery_storage_v6_is_complete(connection)? {
        return Err(StoreError::InvalidState(
            "generation delivery v7 migration requires complete v6 storage".to_string(),
        ));
    }
    connection.execute_batch(
        "ALTER TABLE generation_paths RENAME TO generation_paths_v6;
         ALTER TABLE observed_generations RENAME TO observed_generations_v6;

         CREATE TABLE observed_generations (
            mount_id TEXT NOT NULL,
            source_connection_id TEXT NOT NULL,
            generation_id TEXT NOT NULL,
            inventory_sha256 TEXT NOT NULL,
            workspace_layout_version INTEGER NOT NULL CHECK (workspace_layout_version > 0),
            workspace_layout_digest TEXT NOT NULL,
            last_receipt_sha256 TEXT,
            updated_at TEXT NOT NULL,
            refresh_mode TEXT NOT NULL
                CHECK (refresh_mode IN ('generation_delta_v1', 'full_export_only')),
            PRIMARY KEY (mount_id, source_connection_id),
            FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
         );

         INSERT INTO observed_generations (
            mount_id, source_connection_id, generation_id, inventory_sha256,
            workspace_layout_version, workspace_layout_digest,
            last_receipt_sha256, updated_at, refresh_mode
         )
         SELECT mount_id, source_connection_id, generation_id, inventory_sha256,
                workspace_layout_version, workspace_layout_digest,
                last_receipt_sha256, updated_at, 'full_export_only'
         FROM observed_generations_v6;

         CREATE TABLE generation_paths (
            mount_id TEXT NOT NULL,
            source_connection_id TEXT NOT NULL,
            projection_id TEXT NOT NULL,
            logical_path TEXT NOT NULL,
            local_logical_path TEXT NOT NULL,
            base_generation_id TEXT NOT NULL,
            base_identity_json TEXT,
            base_payload_delta_id TEXT,
            base_payload_entry_index INTEGER CHECK (base_payload_entry_index >= 0),
            conflict_payload_delta_id TEXT,
            conflict_payload_entry_index INTEGER CHECK (conflict_payload_entry_index >= 0),
            state TEXT NOT NULL CHECK (state IN ('clean', 'dirty', 'conflicted')),
            incoming_identity_json TEXT,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (mount_id, projection_id),
            UNIQUE (mount_id, logical_path),
            UNIQUE (mount_id, local_logical_path),
            FOREIGN KEY (mount_id, source_connection_id)
                REFERENCES observed_generations(mount_id, source_connection_id)
                ON DELETE CASCADE
         );

         INSERT INTO generation_paths (
            mount_id, source_connection_id, projection_id, logical_path,
            local_logical_path, base_generation_id, base_identity_json,
            base_payload_delta_id, base_payload_entry_index,
            conflict_payload_delta_id, conflict_payload_entry_index,
            state, incoming_identity_json, updated_at
         )
         SELECT paths.mount_id, observed.source_connection_id,
                paths.projection_id, paths.logical_path, paths.local_logical_path,
                paths.base_generation_id, paths.base_identity_json,
                paths.base_payload_delta_id, paths.base_payload_entry_index,
                paths.conflict_payload_delta_id, paths.conflict_payload_entry_index,
                paths.state, paths.incoming_identity_json, paths.updated_at
         FROM generation_paths_v6 AS paths
         JOIN observed_generations_v6 AS observed
           ON observed.mount_id = paths.mount_id;

         DROP TABLE generation_paths_v6;
         DROP TABLE observed_generations_v6;",
    )?;
    backfill_generation_refresh_modes(connection)?;
    migrate_generation_active_index_to_mount(connection)?;
    Ok(())
}

fn backfill_generation_refresh_modes(connection: &Connection) -> StoreResult<()> {
    let observed = {
        let mut statement = connection.prepare(
            "SELECT mount_id, source_connection_id, generation_id, inventory_sha256,
                    workspace_layout_version, workspace_layout_digest,
                    last_receipt_sha256, updated_at
             FROM observed_generations ORDER BY mount_id, source_connection_id",
        )?;
        statement
            .query_map([], |row| {
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
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for row in observed {
        let observed = observed_generation_from_row(row)?;
        let paths = list_generation_paths_for_source_from_connection(
            connection,
            &observed.mount_id,
            &observed.source_connection_id,
        )?;
        let mode = generation_seed_refresh_mode(&observed, &paths);
        connection.execute(
            "UPDATE observed_generations SET refresh_mode = ?3
             WHERE mount_id = ?1 AND source_connection_id = ?2",
            params![
                observed.mount_id.as_str(),
                observed.source_connection_id.as_str(),
                generation_baseline_refresh_mode_label(mode),
            ],
        )?;
    }
    Ok(())
}

fn migrate_generation_active_index_to_mount(connection: &Connection) -> StoreResult<()> {
    let duplicate_mount: Option<(String, i64)> = connection
        .query_row(
            "SELECT mount_id, COUNT(*) FROM generation_apply_journals
             WHERE active = 1 GROUP BY mount_id HAVING COUNT(*) > 1
             ORDER BY mount_id LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((mount_id, count)) = duplicate_mount {
        return Err(StoreError::InvalidState(format!(
            "mount `{mount_id}` has {count} active generation applies; complete all but one with the prerelease reader before retrying migration"
        )));
    }
    connection.execute_batch(
        "DROP INDEX IF EXISTS generation_apply_one_active_per_source;
         DROP INDEX IF EXISTS generation_apply_one_active_per_mount;
         CREATE UNIQUE INDEX generation_apply_one_active_per_mount
         ON generation_apply_journals(mount_id) WHERE active = 1;",
    )?;
    Ok(())
}

fn table_primary_key_columns(connection: &Connection, table: &str) -> StoreResult<Vec<String>> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut columns = statement
        .query_map([], |row| {
            Ok((row.get::<_, i64>(5)?, row.get::<_, String>(1)?))
        })?
        .filter_map(|row| match row {
            Ok((0, _)) => None,
            other => Some(other),
        })
        .collect::<rusqlite::Result<Vec<_>>>()?;
    columns.sort_by_key(|(ordinal, _)| *ordinal);
    Ok(columns.into_iter().map(|(_, name)| name).collect())
}

fn table_has_unique_columns(
    connection: &Connection,
    table: &str,
    expected: &[&str],
) -> StoreResult<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA index_list({table})"))?;
    let indexes = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, bool>(2)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (index, unique) in indexes {
        if !unique {
            continue;
        }
        let escaped = index.replace('"', "\"\"");
        let mut columns_statement =
            connection.prepare(&format!("PRAGMA index_info(\"{escaped}\")"))?;
        let columns = columns_statement
            .query_map([], |row| row.get::<_, String>(2))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if columns
            .iter()
            .map(String::as_str)
            .eq(expected.iter().copied())
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn index_exists(connection: &Connection, index: &str) -> StoreResult<bool> {
    connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1
             )",
            params![index],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn generation_paths_have_source_foreign_key(connection: &Connection) -> StoreResult<bool> {
    let mut statement = connection.prepare("PRAGMA foreign_key_list(generation_paths)")?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.iter().any(|first| {
        first.1 == 0
            && first.2 == "observed_generations"
            && first.3 == "mount_id"
            && first.4 == "mount_id"
            && first.5 == "CASCADE"
            && rows.iter().any(|second| {
                second.0 == first.0
                    && second.1 == 1
                    && second.2 == "observed_generations"
                    && second.3 == "source_connection_id"
                    && second.4 == "source_connection_id"
                    && second.5 == "CASCADE"
            })
    }))
}

fn verify_generation_delivery_storage_v7(connection: &Connection) -> StoreResult<()> {
    if generation_delivery_storage_v7_is_complete(connection)? {
        return Ok(());
    }
    Err(StoreError::InvalidState(
        "generation delivery migration left incomplete multi-source storage".to_string(),
    ))
}

fn generation_delivery_storage_v7_is_complete(connection: &Connection) -> StoreResult<bool> {
    if !generation_delivery_storage_v6_is_complete(connection)?
        || !column_exists(connection, "generation_paths", "source_connection_id")?
        || !column_exists(connection, "observed_generations", "refresh_mode")?
        || table_primary_key_columns(connection, "observed_generations")?
            != ["mount_id", "source_connection_id"]
        || !table_has_unique_columns(
            connection,
            "generation_paths",
            &["mount_id", "projection_id"],
        )?
        || !table_has_unique_columns(
            connection,
            "generation_paths",
            &["mount_id", "logical_path"],
        )?
        || !table_has_unique_columns(
            connection,
            "generation_paths",
            &["mount_id", "local_logical_path"],
        )?
        || !generation_paths_have_source_foreign_key(connection)?
        || !table_has_unique_columns(connection, "generation_apply_journals", &["mount_id"])?
        || index_exists(connection, "generation_apply_one_active_per_source")?
    {
        return Ok(false);
    }
    let invalid_source_binding: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM generation_paths AS paths
            LEFT JOIN observed_generations AS observed
              ON observed.mount_id = paths.mount_id
             AND observed.source_connection_id = paths.source_connection_id
            WHERE observed.mount_id IS NULL
         )",
        [],
        |row| row.get(0),
    )?;
    let invalid_refresh_mode: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM observed_generations
            WHERE refresh_mode NOT IN ('generation_delta_v1', 'full_export_only')
         )",
        [],
        |row| row.get(0),
    )?;
    Ok(!invalid_source_binding && !invalid_refresh_mode)
}

fn verify_generation_delivery_storage_v6(connection: &Connection) -> StoreResult<()> {
    if generation_delivery_storage_v6_is_complete(connection)? {
        return Ok(());
    }
    for (table, columns) in [
        (
            "generation_paths",
            &[
                "base_payload_delta_id",
                "base_payload_entry_index",
                "local_logical_path",
                "conflict_payload_delta_id",
                "conflict_payload_entry_index",
            ][..],
        ),
        (
            "generation_inode_evidence",
            &[
                "base_payload_delta_id",
                "base_payload_entry_index",
                "visible_evidence_name",
                "visible_expected_sha256",
                "visible_byte_length",
                "resolved_at",
            ][..],
        ),
        (
            "generation_apply_journals",
            &[
                "acknowledgment_required",
                "acknowledged_at",
                "selected_capabilities_json",
                "selection_binding",
            ][..],
        ),
    ] {
        for column in columns {
            if !column_exists(connection, table, column)? {
                return Err(StoreError::InvalidState(format!(
                    "generation delivery migration left `{table}.{column}` incomplete"
                )));
            }
        }
    }
    let null_local_path: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM generation_paths WHERE local_logical_path IS NULL
         )",
        [],
        |row| row.get(0),
    )?;
    if null_local_path {
        return Err(StoreError::InvalidState(
            "generation delivery migration left a null local logical path".to_string(),
        ));
    }
    let partial_visible_evidence: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM generation_inode_evidence
            WHERE (visible_evidence_name IS NULL)
                != (visible_expected_sha256 IS NULL)
               OR (visible_evidence_name IS NULL) != (visible_byte_length IS NULL)
         )",
        [],
        |row| row.get(0),
    )?;
    if partial_visible_evidence {
        return Err(StoreError::InvalidState(
            "generation delivery migration left partial visible inode evidence".to_string(),
        ));
    }
    let invalid_tombstone: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM generation_inode_evidence
            WHERE resolved_at IS NOT NULL AND visible_evidence_name IS NULL
         )",
        [],
        |row| row.get(0),
    )?;
    if invalid_tombstone {
        return Err(StoreError::InvalidState(
            "generation delivery migration left a tombstone without both retained inodes"
                .to_string(),
        ));
    }
    if !generation_delivery_storage_v6_is_complete(connection)? {
        return Err(StoreError::InvalidState(
            "generation delivery migration left an unsafe transport selection binding".to_string(),
        ));
    }
    Ok(())
}

fn generation_delivery_storage_v6_is_complete(connection: &Connection) -> StoreResult<bool> {
    for (table, columns) in [
        (
            "generation_paths",
            &[
                "base_payload_delta_id",
                "base_payload_entry_index",
                "local_logical_path",
                "conflict_payload_delta_id",
                "conflict_payload_entry_index",
            ][..],
        ),
        (
            "generation_inode_evidence",
            &[
                "base_payload_delta_id",
                "base_payload_entry_index",
                "visible_evidence_name",
                "visible_expected_sha256",
                "visible_byte_length",
                "resolved_at",
            ][..],
        ),
        (
            "generation_apply_journals",
            &[
                "acknowledgment_required",
                "acknowledged_at",
                "selected_capabilities_json",
                "selection_binding",
            ][..],
        ),
    ] {
        for column in columns {
            if !column_exists(connection, table, column)? {
                return Ok(false);
            }
        }
    }
    let null_local_path: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM generation_paths WHERE local_logical_path IS NULL
         )",
        [],
        |row| row.get(0),
    )?;
    if null_local_path {
        return Ok(false);
    }
    let invalid_binding: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM generation_apply_journals
            WHERE selection_binding NOT IN ('bound', 'pre_binding_completed')
               OR (selection_binding = 'pre_binding_completed'
                   AND (status != 'completed' OR active != 0))
         )",
        [],
        |row| row.get(0),
    )?;
    let partial_visible_evidence: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM generation_inode_evidence
            WHERE (visible_evidence_name IS NULL)
                != (visible_expected_sha256 IS NULL)
               OR (visible_evidence_name IS NULL) != (visible_byte_length IS NULL)
         )",
        [],
        |row| row.get(0),
    )?;
    let invalid_tombstone: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM generation_inode_evidence
            WHERE resolved_at IS NOT NULL AND visible_evidence_name IS NULL
         )",
        [],
        |row| row.get(0),
    )?;
    Ok(!invalid_binding && !partial_visible_evidence && !invalid_tombstone)
}

fn migrate_journals_component_to_v3(connection: &Connection) -> StoreResult<()> {
    migrate_state_component_to_current(connection, "durable:journals")
}

fn migrate_entity_search_component_to_v2(connection: &Connection) -> StoreResult<()> {
    create_state_management_tables(connection)?;
    let component = connection
        .query_row(
            "SELECT version, min_reader_version
             FROM state_components
             WHERE component_id = 'cache:entity_search'",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    if let Some((component_version, min_reader_version)) = component
        && component_version >= ENTITY_SEARCH_COMPONENT_VERSION
        && min_reader_version <= ENTITY_SEARCH_COMPONENT_VERSION
    {
        return Ok(());
    }

    create_entity_search_index(connection)?;
    rebuild_entity_search_index(connection)?;
    migrate_state_component_to_current(connection, "cache:entity_search")
}

fn migrate_state_component_to_current(
    connection: &Connection,
    component_id: &str,
) -> StoreResult<()> {
    create_state_management_tables(connection)?;
    let definition = CURRENT_COMPONENT_DEFINITIONS
        .iter()
        .find(|definition| definition.component_id == component_id)
        .expect("known state component definition");
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
        if component_version > definition.current_version {
            return Err(StoreError::StateCompatibility(format!(
                "state component {component_id} version {component_version} is newer than supported version {}",
                definition.current_version
            )));
        }
        if min_reader_version > definition.current_version {
            return Err(StoreError::StateCompatibility(format!(
                "state component {component_id} requires reader version {min_reader_version}, but supported version is {}",
                definition.current_version
            )));
        }
        if component_version >= definition.current_version {
            return Ok(());
        }
    }

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
            definition.current_version,
            definition.min_reader_version,
            bool_to_int(definition.required),
            bool_to_int(definition.rebuildable),
            definition.data_json,
            &updated_at,
        ],
    )?;
    Ok(())
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
    let transaction = connection.unchecked_transaction()?;
    migrate_virtual_projection_layout_to_v2_in_transaction(
        &transaction,
        pre_state_components_schema,
        component_id,
        projection,
        layout_version,
        missing_component,
    )?;
    transaction.commit()?;
    Ok(())
}

fn migrate_virtual_projection_layout_to_v2_in_transaction(
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

    let projection_json = to_json(&projection)?;
    let mounts = {
        let mut statement = connection.prepare(
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
        connection.execute(
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
            layout_version,
            definition.min_reader_version,
            bool_to_int(definition.required),
            bool_to_int(definition.rebuildable),
            definition.data_json,
            &updated_at,
        ],
    )?;
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
    let updated_at = unix_timestamp_string();
    for definition in CURRENT_COMPONENT_DEFINITIONS {
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
            | "durable:workspace_bindings"
            | "durable:hosted_workspaces"
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

fn retire_removed_state_components(connection: &Connection) -> StoreResult<()> {
    if !table_exists(connection, "state_components")? {
        return Ok(());
    }
    let transaction = connection.unchecked_transaction()?;
    retire_removed_state_components_in_transaction(&transaction)?;
    transaction.commit()?;
    Ok(())
}

fn retire_removed_state_components_in_transaction(connection: &Connection) -> StoreResult<()> {
    if !table_exists(connection, "state_components")? {
        return Ok(());
    }
    retire_notion_workspace_roots_component_in_transaction(connection)
}

fn retire_notion_workspace_roots_component_in_transaction(
    connection: &Connection,
) -> StoreResult<()> {
    let component = connection
        .query_row(
            "SELECT version, min_reader_version
             FROM state_components
             WHERE component_id = ?1",
            params![RETIRED_NOTION_WORKSPACE_ROOTS_COMPONENT_ID],
            |row| {
                Ok(StateComponentRecord {
                    component_id: RETIRED_NOTION_WORKSPACE_ROOTS_COMPONENT_ID.to_string(),
                    component_kind: "projection_layout".to_string(),
                    version: row.get(0)?,
                    min_reader_version: row.get(1)?,
                    required: true,
                    rebuildable: false,
                    data_json: "{}".to_string(),
                    updated_at: String::new(),
                })
            },
        )
        .optional()?;
    let Some(component) = component else {
        return Ok(());
    };
    if !retired_state_component_is_readable(&component) {
        return Ok(());
    }

    let mut changed = 0usize;
    changed += delete_retired_notion_workspace_root_rows(connection, "shadows", "entity_id")?;
    changed += delete_retired_notion_workspace_root_rows(connection, "entities", "remote_id")?;
    changed +=
        delete_retired_notion_workspace_root_rows(connection, "hydration_jobs", "remote_id")?;
    changed +=
        delete_retired_notion_workspace_root_rows(connection, "freshness_states", "remote_id")?;
    changed +=
        delete_retired_notion_workspace_root_rows(connection, "remote_observations", "remote_id")?;
    changed += clear_retired_notion_workspace_root_parent(
        connection,
        "remote_observations",
        "parent_remote_id",
    )?;
    changed += clear_retired_notion_workspace_root_parent(
        connection,
        "virtual_mutations",
        "parent_remote_id",
    )?;
    changed += rewrite_retired_notion_workspace_root_paths(
        connection,
        "entities",
        "path",
        "AND remote_id NOT IN ('notion-root:private', 'notion-root:workspace')",
        false,
    )?;
    changed += rewrite_retired_notion_workspace_root_paths(
        connection,
        "remote_observations",
        "projected_path",
        "",
        false,
    )?;
    changed += rewrite_retired_notion_workspace_root_paths(
        connection,
        "hydration_jobs",
        "path",
        "",
        false,
    )?;
    changed += rewrite_retired_notion_workspace_root_paths(
        connection,
        "virtual_mutations",
        "projected_path",
        "",
        false,
    )?;
    changed += rewrite_retired_notion_workspace_root_paths(
        connection,
        "virtual_mutations",
        "original_path",
        "",
        false,
    )?;
    changed += rewrite_retired_notion_workspace_root_paths(
        connection,
        "virtual_mutations",
        "content_path",
        "",
        true,
    )?;
    changed += rewrite_retired_notion_workspace_root_paths(
        connection,
        "auto_save_enrollments",
        "path",
        "",
        false,
    )?;
    changed += connection.execute(
        "DELETE FROM state_components WHERE component_id = ?1",
        params![RETIRED_NOTION_WORKSPACE_ROOTS_COMPONENT_ID],
    )?;
    if changed > 0 && table_exists(connection, "entities")? {
        rebuild_entity_search_index(connection)?;
    }
    Ok(())
}

fn delete_retired_notion_workspace_root_rows(
    connection: &Connection,
    table: &str,
    remote_id_column: &str,
) -> StoreResult<usize> {
    if !table_exists(connection, table)? {
        return Ok(0);
    }
    let sql = format!(
        "DELETE FROM {table}
         WHERE mount_id IN (
             SELECT mount_id
             FROM mounts
             WHERE connector = 'notion'
               AND remote_root_id IS NULL
         )
           AND {remote_id_column} IN (?1, ?2)"
    );
    Ok(connection.execute(
        &sql,
        params![
            RETIRED_NOTION_PRIVATE_ROOT_ID,
            RETIRED_NOTION_WORKSPACE_ROOT_ID
        ],
    )?)
}

fn clear_retired_notion_workspace_root_parent(
    connection: &Connection,
    table: &str,
    parent_column: &str,
) -> StoreResult<usize> {
    if !table_exists(connection, table)? {
        return Ok(0);
    }
    let sql = format!(
        "UPDATE {table}
         SET {parent_column} = NULL
         WHERE mount_id IN (
             SELECT mount_id
             FROM mounts
             WHERE connector = 'notion'
               AND remote_root_id IS NULL
         )
           AND {parent_column} IN (?1, ?2)"
    );
    Ok(connection.execute(
        &sql,
        params![
            RETIRED_NOTION_PRIVATE_ROOT_ID,
            RETIRED_NOTION_WORKSPACE_ROOT_ID
        ],
    )?)
}

fn rewrite_retired_notion_workspace_root_paths(
    connection: &Connection,
    table: &str,
    column: &str,
    extra_where: &str,
    skip_absolute: bool,
) -> StoreResult<usize> {
    if !table_exists(connection, table)? {
        return Ok(0);
    }
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
        let Some(rewritten_path) =
            retired_notion_workspace_root_flattened_path(&path, skip_absolute)
        else {
            continue;
        };
        connection.execute(&update_sql, params![rewritten_path, rowid])?;
        rewritten += 1;
    }
    Ok(rewritten)
}

fn retired_notion_workspace_root_flattened_path(path: &str, skip_absolute: bool) -> Option<String> {
    if path.is_empty() || (skip_absolute && is_probably_absolute_path(path)) {
        return None;
    }
    path.strip_prefix("Workspace/")
        .or_else(|| path.strip_prefix("Workspace\\"))
        .or_else(|| path.strip_prefix("Private/"))
        .or_else(|| path.strip_prefix("Private\\"))
        .filter(|suffix| !suffix.is_empty())
        .map(ToOwned::to_owned)
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
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS search_documents_fts USING fts5(
            mount_id UNINDEXED,
            remote_id UNINDEXED,
            connector UNINDEXED,
            kind UNINDEXED,
            title,
            path,
            observed_title,
            observed_path,
            frontmatter,
            body,
            metadata_text,
            breadcrumbs,
            aliases,
            source_url
        );",
    )?;
    Ok(())
}

fn rebuild_entity_search_index(connection: &Connection) -> StoreResult<()> {
    create_entity_search_index(connection)?;
    connection.execute("DELETE FROM entity_search_fts", [])?;
    connection.execute("DELETE FROM search_documents_fts", [])?;

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

    let indexed: Option<(
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = connection
        .query_row(
            "SELECT m.connector, e.kind_json, e.title, e.path,
                    o.title, o.projected_path, s.frontmatter, s.rendered_body,
                    o.raw_metadata_json
             FROM entities e
             INNER JOIN mounts m
               ON m.mount_id = e.mount_id
             LEFT JOIN remote_observations o
               ON o.mount_id = e.mount_id AND o.remote_id = e.remote_id
             LEFT JOIN shadows s
               ON s.mount_id = e.mount_id AND s.entity_id = e.remote_id
             WHERE e.mount_id = ?1 AND e.remote_id = ?2",
            params![mount_id.0, remote_id.0],
            |row| {
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
                ))
            },
        )
        .optional()?;

    if let Some((
        connector,
        kind,
        title,
        path,
        observed_title,
        observed_path,
        frontmatter,
        body,
        raw_metadata_json,
    )) = indexed
    {
        let breadcrumbs = search_breadcrumb_text(Path::new(&path));
        let search_metadata = search_metadata_from_raw_metadata_json(raw_metadata_json.as_deref());
        let metadata_text = join_search_metadata_values(&search_metadata.metadata_text);
        let aliases = join_search_metadata_values(&search_metadata.aliases);
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
                mount_id.0.as_str(),
                remote_id.0.as_str(),
                title.as_str(),
                path.as_str(),
                observed_title.as_deref(),
                observed_path.as_deref(),
            ],
        )?;
        connection.execute(
            "INSERT INTO search_documents_fts (
                mount_id,
                remote_id,
                connector,
                kind,
                title,
                path,
                observed_title,
                observed_path,
                frontmatter,
                body,
                metadata_text,
                breadcrumbs,
                aliases,
                source_url
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                mount_id.0.as_str(),
                remote_id.0.as_str(),
                connector.as_str(),
                kind.as_str(),
                title.as_str(),
                path.as_str(),
                observed_title.as_deref(),
                observed_path.as_deref(),
                frontmatter.as_deref(),
                body.as_deref(),
                metadata_text.as_str(),
                breadcrumbs.as_str(),
                aliases.as_str(),
                search_metadata.source_url.as_deref(),
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
    connection.execute(
        "DELETE FROM search_documents_fts WHERE mount_id = ?1 AND remote_id = ?2",
        params![mount_id.0, remote_id.0],
    )?;
    Ok(())
}

fn search_breadcrumb_text(path: &Path) -> String {
    let mut components = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .map(str::trim)
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    if components
        .last()
        .is_some_and(|component| component.eq_ignore_ascii_case("page.md"))
    {
        components.pop();
    }
    components.join(" ")
}

fn search_document(
    connection: &Connection,
    mount_id: &MountId,
    remote_id: &RemoteId,
) -> StoreResult<Option<EntitySearchDocument>> {
    let row = connection
        .query_row(
            "SELECT title, path, observed_title, observed_path, frontmatter, body,
                    metadata_text, breadcrumbs, aliases, source_url
             FROM search_documents_fts
             WHERE mount_id = ?1 AND remote_id = ?2
             LIMIT 1",
            params![mount_id.0, remote_id.0],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                ))
            },
        )
        .optional()?;

    Ok(row.map(
        |(
            title,
            path,
            observed_title,
            observed_path,
            frontmatter,
            body,
            metadata_text,
            breadcrumbs,
            aliases,
            source_url,
        )| {
            EntitySearchDocument {
                title,
                path,
                observed_title,
                observed_path,
                frontmatter,
                body,
                metadata_text,
                breadcrumbs,
                aliases,
                source_url,
            }
        },
    ))
}

fn search_metadata_from_raw_metadata_json(raw_metadata_json: Option<&str>) -> SearchMetadata {
    let Some(raw_metadata_json) = raw_metadata_json else {
        return SearchMetadata::default();
    };
    let Ok(value) = serde_json::from_str::<Value>(raw_metadata_json) else {
        return SearchMetadata::default();
    };
    value
        .get(RAW_SEARCH_METADATA_KEY)
        .and_then(|value| serde_json::from_value::<SearchMetadata>(value.clone()).ok())
        .unwrap_or_default()
}

fn join_search_metadata_values(values: &[String]) -> String {
    values
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn discovery_entities(
    connection: &Connection,
    mount_id: &MountId,
) -> StoreResult<Vec<EntityRecord>> {
    let mut statement = connection.prepare(
        &(ENTITY_SELECT_WITH_WHERE.to_owned() + "WHERE mount_id = ?1 ORDER BY remote_id"),
    )?;
    let rows = statement.query_map(params![mount_id.0.as_str()], entity_row)?;
    rows.map(|row| entity_from_row(row?)).collect()
}

fn discovery_virtual_mutations(
    connection: &Connection,
    mount_id: &MountId,
) -> StoreResult<Vec<VirtualMutationRecord>> {
    let mut statement = connection.prepare(
        &(VIRTUAL_MUTATION_SELECT_WITH_WHERE.to_owned() + "WHERE mount_id = ?1 ORDER BY local_id"),
    )?;
    let rows = statement.query_map(params![mount_id.0.as_str()], virtual_mutation_row)?;
    rows.map(|row| virtual_mutation_from_row(row?)).collect()
}

fn discovery_auto_save_enrollments(
    connection: &Connection,
    mount_id: &MountId,
) -> StoreResult<Vec<AutoSaveEnrollmentRecord>> {
    let mut statement = connection
        .prepare(&(AUTO_SAVE_SELECT_WITH_WHERE.to_owned() + "WHERE mount_id = ?1 ORDER BY path"))?;
    let rows = statement.query_map(params![mount_id.0.as_str()], auto_save_enrollment_row)?;
    rows.map(|row| auto_save_enrollment_from_row(row?))
        .collect()
}

fn discovery_staging_path(
    connection: &Connection,
    mount_id: &MountId,
    index: usize,
    final_paths: &BTreeSet<String>,
) -> StoreResult<String> {
    for nonce in 0_u64.. {
        let candidate = format!(".locality-discovery-staging/{index}-{nonce}");
        if final_paths.contains(&candidate) {
            continue;
        }
        let exists = connection
            .query_row(
                "SELECT 1 FROM entities WHERE mount_id = ?1 AND path = ?2",
                params![mount_id.0.as_str(), candidate.as_str()],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !exists {
            return Ok(candidate);
        }
    }
    unreachable!("u64 staging path space exhausted")
}

fn upsert_discovery_entity(connection: &Connection, entity: &EntityRecord) -> StoreResult<()> {
    connection.execute(
        "INSERT INTO entities (
            mount_id, remote_id, kind_json, title, path, hydration_json,
            content_hash, remote_edited_at
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
            entity.mount_id.0.as_str(),
            entity.remote_id.0.as_str(),
            to_json(&entity.kind)?,
            entity.title.as_str(),
            logical_path_to_text(&entity.path),
            to_json(&entity.hydration)?,
            entity.content_hash.as_deref(),
            entity.remote_edited_at.as_deref(),
        ],
    )?;
    Ok(())
}

fn upsert_discovery_observation(
    connection: &Connection,
    observation: &RemoteObservationRecord,
) -> StoreResult<()> {
    connection.execute(
        "INSERT INTO remote_observations (
            mount_id, remote_id, kind_json, title, parent_remote_id, projected_path,
            remote_version_json, observed_at, deleted, raw_metadata_json
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
            observation.mount_id.0.as_str(),
            observation.remote_id.0.as_str(),
            to_json(&observation.kind)?,
            observation.title.as_str(),
            observation.parent_remote_id.as_ref().map(RemoteId::as_str),
            logical_path_to_text(&observation.projected_path),
            to_json(&observation.remote_version)?,
            observation.observed_at.as_str(),
            bool_to_int(observation.deleted),
            observation.raw_metadata_json.as_str(),
        ],
    )?;
    Ok(())
}

fn upsert_discovery_freshness(
    connection: &Connection,
    state: &FreshnessStateRecord,
) -> StoreResult<()> {
    connection.execute(
        "INSERT INTO freshness_states (
            mount_id, remote_id, tier_json, last_checked_at, next_check_at,
            last_opened_at, last_local_change_at, remote_hint_pending
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
            state.mount_id.0.as_str(),
            state.remote_id.0.as_str(),
            to_json(&state.tier)?,
            state.last_checked_at.as_deref(),
            state.next_check_at.as_deref(),
            state.last_opened_at.as_deref(),
            state.last_local_change_at.as_deref(),
            bool_to_int(state.remote_hint_pending),
        ],
    )?;
    Ok(())
}

fn upsert_discovery_auto_save(
    connection: &Connection,
    enrollment: &AutoSaveEnrollmentRecord,
) -> StoreResult<()> {
    connection.execute(
        "INSERT INTO auto_save_enrollments (
            mount_id, path, remote_id, enabled, origin_json, state_json,
            last_reason, last_push_id, created_at, updated_at
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
            enrollment.mount_id.0.as_str(),
            path_to_text(&enrollment.path),
            enrollment.remote_id.as_ref().map(RemoteId::as_str),
            bool_to_int(enrollment.enabled),
            to_json(&enrollment.origin)?,
            to_json(&enrollment.state)?,
            enrollment.last_reason.as_deref(),
            enrollment.last_push_id.as_deref(),
            enrollment.created_at.as_str(),
            enrollment.updated_at.as_str(),
        ],
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

const NATIVE_PATH_ENCODING_PREFIX: &str = "locality-native-path-v1:";

fn native_path_to_text(path: &Path) -> String {
    if let Some(path) = path.to_str() {
        return format!(
            "{NATIVE_PATH_ENCODING_PREFIX}utf8:{}",
            encode_hex(path.as_bytes())
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        return format!(
            "{NATIVE_PATH_ENCODING_PREFIX}unix:{}",
            encode_hex(path.as_os_str().as_bytes())
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let bytes = path
            .as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        return format!(
            "{NATIVE_PATH_ENCODING_PREFIX}windows:{}",
            encode_hex(&bytes)
        );
    }
    #[cfg(not(any(unix, windows)))]
    unreachable!("non-UTF-8 native paths are unsupported on this platform")
}

fn native_path_from_text(encoded: &str) -> StoreResult<PathBuf> {
    let Some(encoded) = encoded.strip_prefix(NATIVE_PATH_ENCODING_PREFIX) else {
        return Ok(PathBuf::from(encoded));
    };
    let (kind, bytes) = encoded.split_once(':').ok_or_else(|| {
        StoreError::InvalidState("invalid durable native path encoding".to_string())
    })?;
    let bytes = decode_hex(bytes)?;
    match kind {
        "utf8" => String::from_utf8(bytes).map(PathBuf::from).map_err(|_| {
            StoreError::InvalidState("durable native UTF-8 path is invalid".to_string())
        }),
        #[cfg(unix)]
        "unix" => {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;
            Ok(PathBuf::from(OsString::from_vec(bytes)))
        }
        #[cfg(windows)]
        "windows" => {
            use std::ffi::OsString;
            use std::os::windows::ffi::OsStringExt;
            if bytes.len() % 2 != 0 {
                return Err(StoreError::InvalidState(
                    "durable native Windows path has an odd byte count".to_string(),
                ));
            }
            let wide = bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>();
            Ok(PathBuf::from(OsString::from_wide(&wide)))
        }
        _ => Err(StoreError::InvalidState(format!(
            "durable native path encoding `{kind}` is unsupported on this host"
        ))),
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn decode_hex(encoded: &str) -> StoreResult<Vec<u8>> {
    if encoded.len() % 2 != 0 {
        return Err(StoreError::InvalidState(
            "durable native path has an odd hex length".to_string(),
        ));
    }
    encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16);
            let low = (pair[1] as char).to_digit(16);
            match (high, low) {
                (Some(high), Some(low)) => Ok(((high << 4) | low) as u8),
                _ => Err(StoreError::InvalidState(
                    "durable native path contains invalid hex".to_string(),
                )),
            }
        })
        .collect()
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

fn optional_to_json<T: Serialize>(value: &Option<T>) -> StoreResult<Option<String>> {
    value.as_ref().map(to_json).transpose()
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
