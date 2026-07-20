//! Versioned portable contracts shared by Locality hosts and the hosted data
//! plane.
//!
//! This crate owns envelopes, not transport or persistence. In particular it
//! contains no HTTP client, database repository, cloud SDK, or host path.

use std::collections::BTreeSet;
use std::fmt::{Display, Formatter};

use locality_core::model::RemoteId;
use locality_core::portable::{
    AccessSetId, ChangesetId, ContentVersionId, LogicalPath, PrincipalId, ProjectionEntry,
    ProjectionFileKind, ProjectionId, ProjectionVersionId, ReplicaRevisionId, SessionId,
    SourceAction, SourceConnectionId, SourceOperationPlan, SourceVersionId, TenantId,
};
use serde::{Deserialize, Serialize};

pub use locality_core::portable::RESERVED_EXPORT_METADATA_PATH;

pub const COMPONENT_VERSIONS: ComponentVersions = ComponentVersions {
    session: 1,
    replica: 1,
    export_metadata: 1,
    writable_session_store: 1,
    canonical: 1,
    path: 1,
    changeset: 1,
};

pub const MINIMUM_COMPONENT_VERSIONS: ComponentVersions = COMPONENT_VERSIONS;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentVersions {
    pub session: u16,
    pub replica: u16,
    pub export_metadata: u16,
    pub writable_session_store: u16,
    pub canonical: u16,
    pub path: u16,
    pub changeset: u16,
}

impl ComponentVersions {
    pub fn validate_required(&self) -> Result<(), VersionCompatibilityError> {
        for (component, required, minimum, supported) in [
            (
                ProtocolComponent::Session,
                self.session,
                MINIMUM_COMPONENT_VERSIONS.session,
                COMPONENT_VERSIONS.session,
            ),
            (
                ProtocolComponent::Replica,
                self.replica,
                MINIMUM_COMPONENT_VERSIONS.replica,
                COMPONENT_VERSIONS.replica,
            ),
            (
                ProtocolComponent::ExportMetadata,
                self.export_metadata,
                MINIMUM_COMPONENT_VERSIONS.export_metadata,
                COMPONENT_VERSIONS.export_metadata,
            ),
            (
                ProtocolComponent::WritableSessionStore,
                self.writable_session_store,
                MINIMUM_COMPONENT_VERSIONS.writable_session_store,
                COMPONENT_VERSIONS.writable_session_store,
            ),
            (
                ProtocolComponent::Canonical,
                self.canonical,
                MINIMUM_COMPONENT_VERSIONS.canonical,
                COMPONENT_VERSIONS.canonical,
            ),
            (
                ProtocolComponent::Path,
                self.path,
                MINIMUM_COMPONENT_VERSIONS.path,
                COMPONENT_VERSIONS.path,
            ),
            (
                ProtocolComponent::Changeset,
                self.changeset,
                MINIMUM_COMPONENT_VERSIONS.changeset,
                COMPONENT_VERSIONS.changeset,
            ),
        ] {
            if required > supported {
                return Err(VersionCompatibilityError::NeedsUpdate {
                    component,
                    required,
                    supported,
                });
            }
            if required < minimum {
                return Err(VersionCompatibilityError::UnsupportedLegacy {
                    component,
                    required,
                    minimum,
                });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolComponent {
    Session,
    Replica,
    ExportMetadata,
    WritableSessionStore,
    Canonical,
    Path,
    Changeset,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VersionCompatibilityError {
    NeedsUpdate {
        component: ProtocolComponent,
        required: u16,
        supported: u16,
    },
    UnsupportedLegacy {
        component: ProtocolComponent,
        required: u16,
        minimum: u16,
    },
}

impl Display for VersionCompatibilityError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NeedsUpdate {
                component,
                required,
                supported,
            } => write!(
                formatter,
                "{component:?} version {required} requires an update (supported: {supported})"
            ),
            Self::UnsupportedLegacy {
                component,
                required,
                minimum,
            } => write!(
                formatter,
                "{component:?} version {required} is older than minimum {minimum}"
            ),
        }
    }
}

impl std::error::Error for VersionCompatibilityError {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceVersionContract {
    pub tenant_id: TenantId,
    pub source_connection_id: SourceConnectionId,
    pub source_version_id: SourceVersionId,
    pub remote_id: RemoteId,
    pub provider_version: String,
    pub native_sha256: String,
    pub canonical_sha256: String,
    pub observed_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentVersionContract {
    pub tenant_id: TenantId,
    pub source_connection_id: SourceConnectionId,
    pub content_version_id: ContentVersionId,
    pub sha256: String,
    pub byte_length: u64,
    pub media_type: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionVersionContract {
    pub tenant_id: TenantId,
    pub source_connection_id: SourceConnectionId,
    pub projection_version_id: ProjectionVersionId,
    pub projection: ProjectionEntry,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum AccessSubject {
    Principal(PrincipalId),
    Group(String),
    Workload(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessSetContract {
    pub tenant_id: TenantId,
    pub access_set_id: AccessSetId,
    pub revision: u64,
    pub source_connection_id: SourceConnectionId,
    pub subjects: BTreeSet<AccessSubject>,
    pub source_remote_ids: BTreeSet<RemoteId>,
    pub actions: BTreeSet<SourceAction>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyReplicaRevision {
    pub tenant_id: TenantId,
    pub source_connection_id: SourceConnectionId,
    pub replica_revision_id: ReplicaRevisionId,
    pub source_watermark: String,
    pub projection_revision: u64,
    pub coverage_complete: bool,
    pub published_at: String,
}

/// An already-authorized, server-created query plan.
///
/// Export implementations receive this fixed value rather than raw selectors,
/// user SQL, or client-provided authorization predicates.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizedSessionQuery {
    pub versions: ComponentVersions,
    pub tenant_id: TenantId,
    pub session_id: SessionId,
    pub acting_principal_id: PrincipalId,
    pub authorization_revision: u64,
    pub profile_revision: u64,
    pub replica_revisions: Vec<SessionReplicaRevision>,
    pub validated_filter_digest: String,
    pub max_entries: u64,
    pub max_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionReplicaRevision {
    pub source_connection_id: SourceConnectionId,
    pub replica_revision_id: ReplicaRevisionId,
}

/// One logical row from the exact authorized query.
///
/// The exporter yields these rows in its authorized stream order. Physical
/// serving hints (`scope_root_id`, `export_order`, and `content_storage_id`)
/// remain private repository concerns and are intentionally absent from this
/// wire-visible contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderedExportRow {
    pub tenant_id: TenantId,
    pub source_connection_id: SourceConnectionId,
    pub projection_id: ProjectionId,
    pub logical_path: LogicalPath,
    pub file_kind: ProjectionFileKind,
    pub effective_actions: BTreeSet<SourceAction>,
    pub provider_version: String,
    pub content_sha256: String,
    pub byte_length: u64,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveredCount {
    pub selected_entries: u64,
    pub delivered_entries: u64,
    pub delivered_bytes: u64,
    pub inventory_sha256: String,
}

impl DeliveredCount {
    pub fn is_exact(&self) -> bool {
        self.selected_entries == self.delivered_entries
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WritableMetadataEntry {
    pub projection_id: ProjectionId,
    pub logical_path: LogicalPath,
    pub source_remote_ids: Vec<RemoteId>,
    pub delivered_content_sha256: String,
    pub provider_precondition: String,
    pub effective_actions: BTreeSet<SourceAction>,
    pub baseline_required: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WritableExportMetadata {
    pub versions: ComponentVersions,
    pub session_id: SessionId,
    pub replica_revisions: Vec<SessionReplicaRevision>,
    pub writable_entries: Vec<WritableMetadataEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WritableSessionState {
    pub versions: ComponentVersions,
    pub session_id: SessionId,
    pub metadata: WritableExportMetadata,
    pub dirty_projection_ids: BTreeSet<ProjectionId>,
    pub pending_changeset_ids: BTreeSet<ChangesetId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapExchangeRequest {
    pub versions: ComponentVersions,
    pub bootstrap_token: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCapability {
    pub session_id: SessionId,
    pub opaque_capability: String,
    pub expires_at: String,
}

/// Portable immutable changeset envelope.
///
/// The operation plan uses `LogicalPath`; the legacy `PushPlan` with host
/// `PathBuf` values is never serialized as protocol.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangesetEnvelope {
    pub versions: ComponentVersions,
    pub changeset_id: ChangesetId,
    pub session_id: SessionId,
    pub parent_changeset_id: Option<ChangesetId>,
    pub base_revisions: Vec<SessionReplicaRevision>,
    pub operations: SourceOperationPlan,
    pub readable_diff_sha256: String,
    pub content_digest: String,
    pub submitted_at: String,
}

pub const COMPONENT_VERSIONS_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/component-versions.json");
pub const AUTHORIZED_SESSION_QUERY_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/authorized-session-query.json");
pub const SOURCE_VERSION_GOLDEN_JSON: &[u8] = include_bytes!("../fixtures/source-version.json");
pub const CONTENT_VERSION_GOLDEN_JSON: &[u8] = include_bytes!("../fixtures/content-version.json");
pub const PROJECTION_VERSION_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/projection-version.json");
pub const ACCESS_SET_GOLDEN_JSON: &[u8] = include_bytes!("../fixtures/access-set.json");
pub const READY_REPLICA_REVISION_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/ready-replica-revision.json");
pub const ORDERED_EXPORT_ROWS_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/ordered-export-rows.json");
pub const DELIVERED_COUNT_GOLDEN_JSON: &[u8] = include_bytes!("../fixtures/delivered-count.json");
pub const WRITABLE_EXPORT_METADATA_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/writable-export-metadata.json");
pub const CHANGESET_ENVELOPE_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/changeset-envelope.json");
