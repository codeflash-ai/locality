//! Versioned portable contracts shared by Locality hosts and the hosted data
//! plane.
//!
//! This crate owns envelopes, not transport or persistence. In particular it
//! contains no HTTP client, database repository, cloud SDK, or host path.

use std::collections::BTreeSet;
use std::fmt::{Debug, Display, Formatter};

use locality_core::journal::PushOperationId;
use locality_core::model::RemoteId;
use locality_core::portable::{
    AccessSetId, ChangesetId, ContentVersionId, LogicalPath, PrincipalId, ProjectionEntry,
    ProjectionFileKind, ProjectionId, ProjectionVersionId, ReplicaRevisionId, SessionId,
    SourceAction, SourceConnectionId, SourceOperationPlan, SourceVersionId, TenantId,
};
use locality_core::readable_diff::ReadableDiffOutput;
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
    pub workload_id: String,
    pub authorization_revision: u64,
    pub policy_revision: u64,
    pub profile_revision: u64,
    pub effective_actions: BTreeSet<SourceAction>,
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

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapExchangeRequest {
    pub versions: ComponentVersions,
    pub bootstrap_token: String,
}

impl Debug for BootstrapExchangeRequest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BootstrapExchangeRequest")
            .field("versions", &self.versions)
            .field("bootstrap_token", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCapability {
    pub session_id: SessionId,
    pub opaque_capability: String,
    pub expires_at: String,
}

impl Debug for SessionCapability {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionCapability")
            .field("session_id", &self.session_id)
            .field("opaque_capability", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Versioned request for a backend-authorized working session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRequest {
    pub versions: ComponentVersions,
    pub tenant_id: TenantId,
    pub profile_revision: u64,
    pub acting_principal_id: PrincipalId,
    pub workload_id: String,
    pub requested_actions: BTreeSet<SourceAction>,
    pub narrowing_filter_digest: Option<String>,
}

/// Backend decision returned after intersecting tenant, actor, workload, and
/// profile authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGrant {
    pub versions: ComponentVersions,
    pub session_id: SessionId,
    pub capability: SessionCapability,
    pub authorization_revision: u64,
    pub policy_revision: u64,
    pub profile_revision: u64,
    pub replica_revisions: Vec<SessionReplicaRevision>,
    pub effective_actions: BTreeSet<SourceAction>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaExportRequest {
    pub versions: ComponentVersions,
    pub capability: SessionCapability,
}

/// One versioned frame in an ordered replica export. Transports may encode
/// frames as HTTP records, tar members, or another bounded streaming format.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaExportFrame {
    pub versions: ComponentVersions,
    pub sequence: u64,
    pub payload: ReplicaExportPayload,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ReplicaExportPayload {
    Started {
        session_id: SessionId,
        replica_revisions: Vec<SessionReplicaRevision>,
    },
    Entry(OrderedExportRow),
    WritableMetadata(WritableExportMetadata),
    Completed(DeliveredCount),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ChangesetSourceObject {
    pub source_connection_id: SourceConnectionId,
    pub remote_id: RemoteId,
}

/// Base facts delivered with one writable projection. These are sufficient for
/// a server to retrieve the same base, verify its preconditions, and re-plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveredChangesetBase {
    pub projection_id: ProjectionId,
    pub source_object: ChangesetSourceObject,
    pub provider_precondition: String,
    pub delivered_content_sha256: String,
    pub delivered_shadow_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditedCanonicalBody {
    pub projection_id: ProjectionId,
    pub logical_path: LogicalPath,
    pub canonical_sha256: String,
    pub canonical_markdown: String,
}

/// An upload capability is secret-bearing. Its custom Debug implementation
/// keeps logs and assertion failures from revealing it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizedChangesetUpload {
    pub upload_id: String,
    pub opaque_capability: String,
    pub content_sha256: String,
    pub byte_length: u64,
}

impl Debug for AuthorizedChangesetUpload {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthorizedChangesetUpload")
            .field("upload_id", &self.upload_id)
            .field("opaque_capability", &"<redacted>")
            .field("content_sha256", &self.content_sha256)
            .field("byte_length", &self.byte_length)
            .finish()
    }
}

/// The enum makes inline bodies and an authorized upload mutually exclusive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChangesetContent {
    Inline {
        edited_canonical_bodies: Vec<EditedCanonicalBody>,
    },
    AuthorizedUpload {
        upload: AuthorizedChangesetUpload,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientValidationResult {
    pub code: String,
    pub projection_id: Option<ProjectionId>,
    pub logical_path: LogicalPath,
    pub line: Option<usize>,
    pub message: String,
    pub suggested_fix: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditReference {
    pub kind: String,
    pub reference: String,
}

/// Portable immutable changeset envelope.
///
/// The operation plan uses `LogicalPath`; the legacy `PushPlan` with host
/// `PathBuf` values is never serialized as protocol.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangesetEnvelope {
    pub versions: ComponentVersions,
    pub changeset_id: ChangesetId,
    pub tenant_id: TenantId,
    pub session_id: SessionId,
    pub acting_principal_id: PrincipalId,
    pub workload_id: String,
    pub authorization_revision: u64,
    pub policy_revision: u64,
    pub profile_revision: u64,
    pub parent_changeset_id: Option<ChangesetId>,
    pub replica_revisions: Vec<SessionReplicaRevision>,
    pub affected_projection_ids: BTreeSet<ProjectionId>,
    pub affected_source_objects: BTreeSet<ChangesetSourceObject>,
    pub delivered_bases: Vec<DeliveredChangesetBase>,
    pub content: ChangesetContent,
    pub advisory_operations: SourceOperationPlan,
    pub readable_diff: ReadableDiffOutput,
    pub readable_diff_sha256: String,
    pub operation_ids: Vec<PushOperationId>,
    pub idempotency_key: String,
    pub client_validation_results: Vec<ClientValidationResult>,
    pub audit_reference: Option<AuditReference>,
    pub content_digest: String,
    pub submitted_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangesetReceipt {
    pub versions: ComponentVersions,
    pub changeset_id: ChangesetId,
    pub state: ChangesetState,
    pub received_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangesetStatusRequest {
    pub versions: ComponentVersions,
    pub changeset_id: ChangesetId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangesetStatus {
    pub versions: ComponentVersions,
    pub changeset_id: ChangesetId,
    pub state: ChangesetState,
    pub updated_at: String,
    pub detail: Option<ChangesetStatusDetail>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangesetState {
    NoOp,
    Received,
    Applying,
    Applied,
    Reconciled,
    Conflicted,
    Rejected,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangesetStatusDetail {
    pub code: String,
    pub message: String,
    pub retriable: bool,
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
