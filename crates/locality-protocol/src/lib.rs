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
    AccessSetId, ChangesetId, ContentVersionId, ExportAttemptId, LogicalPath, PrincipalId,
    ProjectionEntry, ProjectionFileKind, ProjectionId, ProjectionVersionId, ReplicaRevisionId,
    SessionId, SourceAction, SourceConnectionId, SourceGenerationId, SourceOperationPlan,
    SourceScopeId, SourceVersionId, TenantId,
};
use locality_core::readable_diff::ReadableDiffOutput;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use locality_core::portable::RESERVED_EXPORT_METADATA_PATH;

pub const PAX_SOURCE_CONNECTION_ID: &str = "locality.source_connection_id";
pub const PAX_PROJECTION_ID: &str = "locality.projection_id";
pub const PAX_WINNING_SCOPE_ORDINAL: &str = "locality.winning_scope_ordinal";
pub const PAX_FILE_KIND: &str = "locality.file_kind";
pub const PAX_EFFECTIVE_ACTIONS: &str = "locality.effective_actions";
pub const PAX_CONTENT_SHA256: &str = "locality.content_sha256";
pub const EXPORT_V2_FILE_PAX_KEYS: [&str; 6] = [
    PAX_SOURCE_CONNECTION_ID,
    PAX_PROJECTION_ID,
    PAX_WINNING_SCOPE_ORDINAL,
    PAX_FILE_KIND,
    PAX_EFFECTIVE_ACTIONS,
    PAX_CONTENT_SHA256,
];

pub const COMPONENT_VERSIONS: ComponentVersions = ComponentVersions {
    session: 1,
    replica: 1,
    export_metadata: 1,
    writable_session_store: 1,
    canonical: 1,
    path: 1,
    changeset: 1,
};

/// Latest component versions this crate can decode and validate.
///
/// [`COMPONENT_VERSIONS`] remains the byte-compatible v1 emission default.
/// Callers opt into the scope-authorized export contracts explicitly.
pub const LATEST_COMPONENT_VERSIONS: ComponentVersions = ComponentVersions {
    session: 2,
    replica: 2,
    export_metadata: 2,
    ..COMPONENT_VERSIONS
};

/// Component versions required by scope-authorized current-head exports.
pub const SCOPE_AUTHORIZED_COMPONENT_VERSIONS: ComponentVersions = LATEST_COMPONENT_VERSIONS;

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
                LATEST_COMPONENT_VERSIONS.session,
            ),
            (
                ProtocolComponent::Replica,
                self.replica,
                MINIMUM_COMPONENT_VERSIONS.replica,
                LATEST_COMPONENT_VERSIONS.replica,
            ),
            (
                ProtocolComponent::ExportMetadata,
                self.export_metadata,
                MINIMUM_COMPONENT_VERSIONS.export_metadata,
                LATEST_COMPONENT_VERSIONS.export_metadata,
            ),
            (
                ProtocolComponent::WritableSessionStore,
                self.writable_session_store,
                MINIMUM_COMPONENT_VERSIONS.writable_session_store,
                LATEST_COMPONENT_VERSIONS.writable_session_store,
            ),
            (
                ProtocolComponent::Canonical,
                self.canonical,
                MINIMUM_COMPONENT_VERSIONS.canonical,
                LATEST_COMPONENT_VERSIONS.canonical,
            ),
            (
                ProtocolComponent::Path,
                self.path,
                MINIMUM_COMPONENT_VERSIONS.path,
                LATEST_COMPONENT_VERSIONS.path,
            ),
            (
                ProtocolComponent::Changeset,
                self.changeset,
                MINIMUM_COMPONENT_VERSIONS.changeset,
                LATEST_COMPONENT_VERSIONS.changeset,
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

/// The provider-specific meaning of one stable source scope.
///
/// Each variant uses stable provider IDs. Titles, projected paths, and search
/// text are deliberately absent because they cannot establish membership.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum ProviderSourceScopeSelector {
    Notion {
        selector_version: u16,
        scope_kind: NotionScopeKind,
        provider_scope_id: String,
    },
    Slack {
        selector_version: u16,
        conversation_id: String,
    },
    Granola {
        selector_version: u16,
        scope_kind: GranolaScopeKind,
        provider_scope_id: String,
    },
    Gmail {
        selector_version: u16,
        mailbox_id: String,
        scope_kind: GmailScopeKind,
        provider_scope_id: String,
    },
    GoogleDrive {
        selector_version: u16,
        scope_kind: GoogleDriveScopeKind,
        provider_scope_id: String,
    },
    Github {
        selector_version: u16,
        scope_kind: GithubScopeKind,
        provider_scope_id: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotionScopeKind {
    Page,
    Database,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GranolaScopeKind {
    Collection,
    Team,
    Folder,
    Meeting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GmailScopeKind {
    Label,
    Thread,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoogleDriveScopeKind {
    SharedDrive,
    Folder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GithubScopeKind {
    Organization,
    Repository,
}

impl ProviderSourceScopeSelector {
    pub fn validate(&self) -> Result<(), ScopeContractError> {
        let selector_version = match self {
            Self::Notion {
                selector_version, ..
            }
            | Self::Slack {
                selector_version, ..
            }
            | Self::Granola {
                selector_version, ..
            }
            | Self::Gmail {
                selector_version, ..
            }
            | Self::GoogleDrive {
                selector_version, ..
            }
            | Self::Github {
                selector_version, ..
            } => *selector_version,
        };
        if selector_version != 1 {
            return Err(ScopeContractError::UnsupportedSelectorVersion {
                version: selector_version,
            });
        }

        match self {
            Self::Notion {
                provider_scope_id, ..
            }
            | Self::Granola {
                provider_scope_id, ..
            }
            | Self::GoogleDrive {
                provider_scope_id, ..
            }
            | Self::Github {
                provider_scope_id, ..
            } => validate_nonempty("provider_scope_id", provider_scope_id)?,
            Self::Slack {
                conversation_id, ..
            } => validate_nonempty("conversation_id", conversation_id)?,
            Self::Gmail {
                mailbox_id,
                provider_scope_id,
                ..
            } => {
                validate_nonempty("mailbox_id", mailbox_id)?;
                validate_nonempty("provider_scope_id", provider_scope_id)?;
            }
        }
        Ok(())
    }
}

/// One source scope after provider, tenant, principal, workload, and profile
/// authority have been intersected. `ordinal` is the profile-configured order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizedSourceScope {
    pub ordinal: u32,
    pub source_scope_id: SourceScopeId,
    pub source_connection_id: SourceConnectionId,
    pub selector: ProviderSourceScopeSelector,
    pub effective_actions: BTreeSet<SourceAction>,
    pub validated_filter_digest: Option<String>,
}

impl AuthorizedSourceScope {
    pub fn validate(&self) -> Result<(), ScopeContractError> {
        validate_nonempty("source_connection_id", self.source_connection_id.as_str())?;
        self.selector.validate()?;
        if self.effective_actions.is_empty() {
            return Err(ScopeContractError::EmptyCollection("effective_actions"));
        }
        if let Some(digest) = &self.validated_filter_digest {
            validate_sha256("validated_filter_digest", digest)?;
        }
        Ok(())
    }
}

/// An authorized current-head query. Unlike [`AuthorizedSessionQuery`], this
/// value names stable source scopes and deliberately does not pin revisions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeAuthorizedSessionQuery {
    pub versions: ComponentVersions,
    pub tenant_id: TenantId,
    pub session_id: SessionId,
    pub acting_principal_id: PrincipalId,
    pub workload_id: String,
    pub authorization_revision: u64,
    pub policy_revision: u64,
    pub profile_revision: u64,
    pub authorized_scopes: Vec<AuthorizedSourceScope>,
    pub max_files: u64,
    pub max_directories: u64,
    pub max_bytes: u64,
}

impl ScopeAuthorizedSessionQuery {
    pub fn validate(&self) -> Result<(), ScopeContractError> {
        self.versions.validate_required()?;
        if self.versions.session < 2 {
            return Err(ScopeContractError::ComponentVersionTooOld {
                component: ProtocolComponent::Session,
                required: 2,
                actual: self.versions.session,
            });
        }
        if self.versions.replica < 2 {
            return Err(ScopeContractError::ComponentVersionTooOld {
                component: ProtocolComponent::Replica,
                required: 2,
                actual: self.versions.replica,
            });
        }
        validate_nonempty("tenant_id", self.tenant_id.as_str())?;
        validate_nonempty("session_id", self.session_id.as_str())?;
        validate_nonempty("acting_principal_id", self.acting_principal_id.as_str())?;
        validate_nonempty("workload_id", &self.workload_id)?;
        if self.authorized_scopes.is_empty() {
            return Err(ScopeContractError::EmptyCollection("authorized_scopes"));
        }
        if self.max_files == 0 || self.max_directories == 0 || self.max_bytes == 0 {
            return Err(ScopeContractError::InvalidLimit);
        }

        let mut scope_ids = BTreeSet::new();
        for (expected_ordinal, scope) in self.authorized_scopes.iter().enumerate() {
            scope.validate()?;
            if scope.ordinal as usize != expected_ordinal {
                return Err(ScopeContractError::NonCanonicalOrdinal {
                    collection: "authorized_scopes",
                    expected: expected_ordinal as u32,
                    actual: scope.ordinal,
                });
            }
            if !scope_ids.insert(scope.source_scope_id.clone()) {
                return Err(ScopeContractError::DuplicateValue("source_scope_id"));
            }
        }
        Ok(())
    }
}

/// Compatibility decoder for already-issued revision-pinned sessions and new
/// scope-authorized current-head sessions. Variants remain structurally
/// distinct on the wire and are never silently reinterpreted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CompatibleAuthorizedSessionQuery {
    Scope(ScopeAuthorizedSessionQuery),
    Legacy(AuthorizedSessionQuery),
}

/// One source generation in configured source order for an export attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderedSourceGeneration {
    pub ordinal: u32,
    pub source_connection_id: SourceConnectionId,
    pub source_generation_id: SourceGenerationId,
}

pub fn validate_source_generations(
    source_generations: &[OrderedSourceGeneration],
) -> Result<(), ScopeContractError> {
    if source_generations.is_empty() {
        return Err(ScopeContractError::EmptyCollection("source_generations"));
    }
    let mut source_ids = BTreeSet::new();
    for (expected_ordinal, generation) in source_generations.iter().enumerate() {
        if generation.ordinal as usize != expected_ordinal {
            return Err(ScopeContractError::NonCanonicalOrdinal {
                collection: "source_generations",
                expected: expected_ordinal as u32,
                actual: generation.ordinal,
            });
        }
        validate_nonempty(
            "source_connection_id",
            generation.source_connection_id.as_str(),
        )?;
        if !source_ids.insert(generation.source_connection_id.clone()) {
            return Err(ScopeContractError::DuplicateValue("source_connection_id"));
        }
    }
    Ok(())
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

/// Canonical key for the single final metadata/receipt control member.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CanonicalControlOrderKey {
    pub ordinal: u8,
}

/// Canonical first-phase order for a directory tar record. `depth` counts the
/// directory itself, so `Projects` is depth 1 and `Projects/Roadmap` is 2.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CanonicalDirectoryOrderKey {
    pub depth: u32,
    pub logical_path: LogicalPath,
}

/// Canonical second-phase order for a projected file tar record.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CanonicalFileOrderKey {
    pub winning_scope_ordinal: u32,
    pub parent_path: Option<LogicalPath>,
    pub logical_path: LogicalPath,
    pub projection_id: ProjectionId,
}

/// The enum discriminant makes directories sort before files and the single
/// reserved control member sort last.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "record_class", content = "order_key", rename_all = "snake_case")]
pub enum CanonicalExportOrderKey {
    Directory(CanonicalDirectoryOrderKey),
    File(CanonicalFileOrderKey),
    Control(CanonicalControlOrderKey),
}

/// Metadata-only record used to validate and order an export before body reads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "record_class", rename_all = "snake_case")]
pub enum CanonicalExportRecord {
    Directory {
        order_key: CanonicalDirectoryOrderKey,
    },
    File {
        order_key: CanonicalFileOrderKey,
        source_connection_id: SourceConnectionId,
        file_kind: ProjectionFileKind,
        effective_actions: BTreeSet<SourceAction>,
        content_sha256: String,
        byte_length: u64,
    },
    Control {
        order_key: CanonicalControlOrderKey,
        member_path: String,
    },
}

impl CanonicalExportRecord {
    pub fn order_key(&self) -> CanonicalExportOrderKey {
        match self {
            Self::Control { order_key, .. } => CanonicalExportOrderKey::Control(order_key.clone()),
            Self::Directory { order_key } => CanonicalExportOrderKey::Directory(order_key.clone()),
            Self::File { order_key, .. } => CanonicalExportOrderKey::File(order_key.clone()),
        }
    }

    pub fn validate(&self) -> Result<(), ScopeContractError> {
        match self {
            Self::Control {
                order_key,
                member_path,
            } => {
                if order_key.ordinal != 0 || member_path != RESERVED_EXPORT_METADATA_PATH {
                    return Err(ScopeContractError::InvalidControlRecord);
                }
            }
            Self::Directory { order_key } => {
                if order_key.logical_path.as_str().eq_ignore_ascii_case(".loc") {
                    return Err(ScopeContractError::InvalidControlDirectory);
                }
                let actual_depth = path_depth(&order_key.logical_path);
                if order_key.depth != actual_depth {
                    return Err(ScopeContractError::InvalidDirectoryDepth {
                        expected: actual_depth,
                        actual: order_key.depth,
                    });
                }
            }
            Self::File {
                order_key,
                source_connection_id,
                effective_actions,
                content_sha256,
                ..
            } => {
                let expected_parent = logical_parent(&order_key.logical_path)?;
                if order_key.parent_path != expected_parent {
                    return Err(ScopeContractError::InvalidParentPath);
                }
                validate_nonempty("source_connection_id", source_connection_id.as_str())?;
                if effective_actions.is_empty() {
                    return Err(ScopeContractError::EmptyCollection("effective_actions"));
                }
                validate_sha256("content_sha256", content_sha256)?;
            }
        }
        Ok(())
    }
}

pub fn validate_canonical_export_records(
    records: &[CanonicalExportRecord],
) -> Result<(), ScopeContractError> {
    let mut control_count = 0_u64;
    let mut logical_paths = BTreeSet::new();
    let mut directory_paths = BTreeSet::new();
    let mut previous_key = None;
    for record in records {
        record.validate()?;
        if matches!(record, CanonicalExportRecord::Control { .. }) {
            control_count += 1;
        }
        let logical_path = match record {
            CanonicalExportRecord::Control { .. } => None,
            CanonicalExportRecord::Directory { order_key } => Some(&order_key.logical_path),
            CanonicalExportRecord::File { order_key, .. } => Some(&order_key.logical_path),
        };
        if let Some(logical_path) = logical_path {
            if !logical_paths.insert(logical_path.clone()) {
                return Err(ScopeContractError::DuplicateValue("logical_path"));
            }
        }
        match record {
            CanonicalExportRecord::Directory { order_key } => {
                if logical_parent(&order_key.logical_path)?
                    .as_ref()
                    .is_some_and(|parent| {
                        !parent.as_str().eq_ignore_ascii_case(".loc")
                            && !directory_paths.contains(parent)
                    })
                {
                    return Err(ScopeContractError::MissingParentDirectory);
                }
                directory_paths.insert(order_key.logical_path.clone());
            }
            CanonicalExportRecord::File { order_key, .. } => {
                if order_key.parent_path.as_ref().is_some_and(|parent| {
                    !parent.as_str().eq_ignore_ascii_case(".loc")
                        && !directory_paths.contains(parent)
                }) {
                    return Err(ScopeContractError::MissingParentDirectory);
                }
            }
            CanonicalExportRecord::Control { .. } => {}
        }
        let key = record.order_key();
        if previous_key
            .as_ref()
            .is_some_and(|previous| previous >= &key)
        {
            return Err(ScopeContractError::NonCanonicalRecordOrder);
        }
        previous_key = Some(key);
    }
    if control_count != 1 {
        return Err(ScopeContractError::InvalidControlRecordCount {
            actual: control_count,
        });
    }
    Ok(())
}

/// Exact domain-separated, length-framed directory/file inventory preimage
/// shared by exporters and clients. The final control member is excluded to
/// avoid a receipt-hash recursion, and file bodies are never read here.
pub fn canonical_export_inventory_preimage(
    records: &[CanonicalExportRecord],
) -> Result<Vec<u8>, ScopeContractError> {
    validate_canonical_export_records(records)?;
    let mut output = b"locality.export.inventory.v2\0".to_vec();
    append_count(
        &mut output,
        records
            .iter()
            .filter(|record| !matches!(record, CanonicalExportRecord::Control { .. }))
            .count(),
    )?;
    for record in records {
        match record {
            CanonicalExportRecord::Control { .. } => {}
            CanonicalExportRecord::Directory { order_key } => {
                append_text(&mut output, "directory")?;
                append_text(&mut output, order_key.logical_path.as_str())?;
            }
            CanonicalExportRecord::File {
                order_key,
                source_connection_id,
                file_kind,
                effective_actions,
                content_sha256,
                byte_length,
            } => {
                append_text(&mut output, "file")?;
                append_u64(&mut output, u64::from(order_key.winning_scope_ordinal))?;
                append_text(&mut output, source_connection_id.as_str())?;
                append_text(&mut output, order_key.projection_id.as_str())?;
                append_text(&mut output, order_key.logical_path.as_str())?;
                append_text(&mut output, projection_file_kind_wire_label(file_kind))?;
                append_count(&mut output, effective_actions.len())?;
                let mut action_labels = effective_actions
                    .iter()
                    .map(source_action_wire_label)
                    .collect::<Vec<_>>();
                action_labels.sort_unstable();
                for action_label in action_labels {
                    append_text(&mut output, action_label)?;
                }
                append_text(&mut output, content_sha256)?;
                append_u64(&mut output, *byte_length)?;
            }
        }
    }
    Ok(output)
}

pub fn canonical_export_inventory_sha256(
    records: &[CanonicalExportRecord],
) -> Result<String, ScopeContractError> {
    let preimage = canonical_export_inventory_preimage(records)?;
    Ok(format!("sha256:{:x}", Sha256::digest(preimage)))
}

/// Incremental digest of delivered file bodies in canonical file order.
///
/// The preimage begins with `locality.export.delivered-bodies.v2\0` and a u64
/// big-endian file count. Each file contributes a length-framed projection ID
/// followed by its length-framed exact body bytes. Directory and final control
/// members are excluded. The sealed inventory binds each projection ID to its
/// path, metadata hash, and expected byte length.
pub struct DeliveredBodyDigestV2 {
    hasher: Sha256,
    expected_file_count: u64,
    delivered_file_count: u64,
}

impl DeliveredBodyDigestV2 {
    pub fn new(expected_file_count: u64) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"locality.export.delivered-bodies.v2\0");
        hasher.update(expected_file_count.to_be_bytes());
        Self {
            hasher,
            expected_file_count,
            delivered_file_count: 0,
        }
    }

    pub fn update_file(
        &mut self,
        projection_id: &ProjectionId,
        body: &[u8],
    ) -> Result<(), ScopeContractError> {
        validate_nonempty("projection_id", projection_id.as_str())?;
        update_digest_scalar(&mut self.hasher, projection_id.as_str().as_bytes())?;
        update_digest_scalar(&mut self.hasher, body)?;
        self.delivered_file_count = self
            .delivered_file_count
            .checked_add(1)
            .ok_or(ScopeContractError::InventoryTooLarge)?;
        Ok(())
    }

    pub fn finish(self) -> Result<String, ScopeContractError> {
        if self.delivered_file_count != self.expected_file_count {
            return Err(ScopeContractError::DeliveredBodyCountMismatch {
                expected: self.expected_file_count,
                actual: self.delivered_file_count,
            });
        }
        Ok(format!("sha256:{:x}", self.hasher.finalize()))
    }
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

/// One-time bootstrap exchange containing no client-selected scope.
///
/// This is the Phase 1 token-only request. [`BootstrapExchangeRequest`] remains
/// available for version-negotiating legacy transports.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpaqueBootstrapExchangeRequest {
    pub bootstrap_token: String,
}

impl Debug for OpaqueBootstrapExchangeRequest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpaqueBootstrapExchangeRequest")
            .field("bootstrap_token", &"<redacted>")
            .finish()
    }
}

/// Status lookup authorized only by the opaque session capability.
///
/// Tenant, principal, workload, roots, filters, and requested actions were
/// sealed before bootstrap issuance and cannot be supplied again here.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpaqueSessionStatusRequest {
    pub opaque_capability: String,
}

impl Debug for OpaqueSessionStatusRequest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpaqueSessionStatusRequest")
            .field("opaque_capability", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleSessionBehavior {
    Fail,
    WaitThenFail,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshnessRequirement {
    pub max_age_seconds: u64,
    pub on_stale: StaleSessionBehavior,
    pub wait_timeout_seconds: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplicaFreshnessState {
    Bootstrapping,
    Fresh,
    Stale,
    Unavailable,
}

/// Explicit freshness and coverage facts for one pinned source replica.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaFreshnessStatus {
    pub source_connection_id: SourceConnectionId,
    pub state: ReplicaFreshnessState,
    pub coverage_complete: bool,
    pub provider_observed_through: Option<String>,
    pub last_successful_sync_at: Option<String>,
    pub last_repair_at: Option<String>,
    pub pending_events: u64,
    pub backlog: u64,
    pub provider_cooldown_until: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TarContentEncoding {
    Identity,
    Zstd,
}

/// Starts (or idempotently replays) export-attempt preflight. The idempotency
/// key is distinct from the server-assigned [`ExportAttemptId`].
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportAttemptRequest {
    pub versions: ComponentVersions,
    pub opaque_session_capability: String,
    pub idempotency_key: String,
    pub content_encoding: TarContentEncoding,
    pub limits: ExportAttemptLimits,
}

impl Debug for ExportAttemptRequest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExportAttemptRequest")
            .field("versions", &self.versions)
            .field("opaque_session_capability", &"<redacted>")
            .field("idempotency_key", &"<redacted>")
            .field("content_encoding", &self.content_encoding)
            .field("limits", &self.limits)
            .finish()
    }
}

impl ExportAttemptRequest {
    pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;

    pub fn validate(&self) -> Result<(), ScopeContractError> {
        validate_export_versions(&self.versions)?;
        validate_nonempty("opaque_session_capability", &self.opaque_session_capability)?;
        validate_nonempty("idempotency_key", &self.idempotency_key)?;
        if self.idempotency_key.len() > Self::MAX_IDEMPOTENCY_KEY_BYTES {
            return Err(ScopeContractError::ValueTooLong {
                field: "idempotency_key",
                maximum_bytes: Self::MAX_IDEMPOTENCY_KEY_BYTES,
                actual_bytes: self.idempotency_key.len(),
            });
        }
        self.limits.validate()?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportAttemptLimits {
    pub max_files: u64,
    pub max_directories: u64,
    pub max_content_bytes: u64,
}

impl ExportAttemptLimits {
    pub fn validate(&self) -> Result<(), ScopeContractError> {
        if self.max_files == 0 || self.max_directories == 0 || self.max_content_bytes == 0 {
            return Err(ScopeContractError::InvalidLimit);
        }
        Ok(())
    }
}

/// Exact immutable selection sealed to one export attempt and source-head
/// generation vector before the first body byte is authorized.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedExportOffer {
    pub versions: ComponentVersions,
    pub session_id: SessionId,
    pub export_attempt_id: ExportAttemptId,
    pub source_generations: Vec<OrderedSourceGeneration>,
    pub media_type: String,
    pub content_encoding: TarContentEncoding,
    pub limits: ExportAttemptLimits,
    pub control_entry_count: u64,
    pub file_count: u64,
    pub directory_count: u64,
    pub archive_entry_count: u64,
    pub selected_content_bytes: u64,
    pub inventory_sha256: String,
    pub sealed_at: String,
    pub expires_at: String,
}

impl SealedExportOffer {
    pub fn validate(&self) -> Result<(), ScopeContractError> {
        validate_export_versions(&self.versions)?;
        validate_nonempty("session_id", self.session_id.as_str())?;
        validate_source_generations(&self.source_generations)?;
        validate_nonempty("media_type", &self.media_type)?;
        validate_nonempty("sealed_at", &self.sealed_at)?;
        validate_nonempty("expires_at", &self.expires_at)?;
        self.limits.validate()?;
        validate_export_counts(
            self.control_entry_count,
            self.file_count,
            self.directory_count,
            self.archive_entry_count,
        )?;
        validate_selection_limits(
            &self.limits,
            self.file_count,
            self.directory_count,
            self.selected_content_bytes,
        )?;
        validate_sha256("inventory_sha256", &self.inventory_sha256)?;
        Ok(())
    }

    pub fn validate_inventory(
        &self,
        records: &[CanonicalExportRecord],
    ) -> Result<(), ScopeContractError> {
        self.validate()?;
        validate_canonical_export_records(records)?;
        let mut control_entry_count = 0_u64;
        let mut directory_count = 0_u64;
        let mut file_count = 0_u64;
        let mut selected_content_bytes = 0_u64;
        for record in records {
            match record {
                CanonicalExportRecord::Control { .. } => control_entry_count += 1,
                CanonicalExportRecord::Directory { .. } => directory_count += 1,
                CanonicalExportRecord::File { byte_length, .. } => {
                    file_count += 1;
                    selected_content_bytes = selected_content_bytes
                        .checked_add(*byte_length)
                        .ok_or(ScopeContractError::InventoryTooLarge)?;
                }
            }
        }
        let inventory_sha256 = canonical_export_inventory_sha256(records)?;
        let archive_entry_count =
            u64::try_from(records.len()).map_err(|_| ScopeContractError::InventoryTooLarge)?;
        if control_entry_count != self.control_entry_count
            || directory_count != self.directory_count
            || file_count != self.file_count
            || archive_entry_count != self.archive_entry_count
            || selected_content_bytes != self.selected_content_bytes
            || inventory_sha256 != self.inventory_sha256
        {
            return Err(ScopeContractError::InventoryDoesNotMatchOffer);
        }
        Ok(())
    }
}

/// Durable completion facts embedded in the final reserved control member.
/// Clean tar/Zstd EOF, this unique final receipt, and independently recomputed
/// inventory equality together gate atomic tree publication.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportCompletionReceipt {
    pub versions: ComponentVersions,
    pub session_id: SessionId,
    pub export_attempt_id: ExportAttemptId,
    pub source_generations: Vec<OrderedSourceGeneration>,
    pub inventory_sha256: String,
    pub delivered_control_entry_count: u64,
    pub delivered_file_count: u64,
    pub delivered_directory_count: u64,
    pub delivered_archive_entry_count: u64,
    pub delivered_content_bytes: u64,
    pub delivered_body_sha256: String,
    pub completed_at: String,
}

impl ExportCompletionReceipt {
    pub fn validate(&self) -> Result<(), ScopeContractError> {
        validate_export_versions(&self.versions)?;
        validate_nonempty("session_id", self.session_id.as_str())?;
        validate_source_generations(&self.source_generations)?;
        validate_nonempty("completed_at", &self.completed_at)?;
        validate_export_counts(
            self.delivered_control_entry_count,
            self.delivered_file_count,
            self.delivered_directory_count,
            self.delivered_archive_entry_count,
        )?;
        validate_sha256("inventory_sha256", &self.inventory_sha256)?;
        validate_sha256("delivered_body_sha256", &self.delivered_body_sha256)?;
        Ok(())
    }

    pub fn validate_against(&self, offer: &SealedExportOffer) -> Result<(), ScopeContractError> {
        self.validate()?;
        offer.validate()?;
        if self.session_id != offer.session_id
            || self.export_attempt_id != offer.export_attempt_id
            || self.source_generations != offer.source_generations
            || self.inventory_sha256 != offer.inventory_sha256
            || self.delivered_control_entry_count != offer.control_entry_count
            || self.delivered_file_count != offer.file_count
            || self.delivered_directory_count != offer.directory_count
            || self.delivered_archive_entry_count != offer.archive_entry_count
            || self.delivered_content_bytes != offer.selected_content_bytes
        {
            return Err(ScopeContractError::ReceiptDoesNotMatchOffer);
        }
        Ok(())
    }
}

/// Encodings and exact decoded bounds available for one immutable tar export.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TarExportOffer {
    pub media_type: String,
    pub supported_content_encodings: BTreeSet<TarContentEncoding>,
    pub selected_entries: u64,
    pub decoded_bytes: u64,
    pub decoded_tar_sha256: String,
}

/// Metadata accompanying a negotiated identity or Zstd tar response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TarExportMetadata {
    pub versions: ComponentVersions,
    pub session_id: SessionId,
    pub media_type: String,
    pub content_encoding: TarContentEncoding,
    pub delivered_entries: u64,
    pub decoded_bytes: u64,
    pub wire_bytes: u64,
    pub decoded_tar_sha256: String,
    pub inventory_sha256: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxSessionState {
    Bootstrapping,
    Ready,
    Failed,
    Expired,
    Revoked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionErrorCode {
    NeedsUpdate,
    Bootstrapping,
    Stale,
    Incomplete,
    Unavailable,
    Unauthorized,
    Expired,
    LimitExceeded,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionProtocolError {
    pub code: SessionErrorCode,
    pub message: String,
    pub retriable: bool,
    pub retry_after_seconds: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxSessionStatus {
    pub versions: ComponentVersions,
    pub session_id: SessionId,
    pub state: SandboxSessionState,
    pub freshness_requirement: FreshnessRequirement,
    pub replicas: Vec<ReplicaFreshnessStatus>,
    pub export_offer: Option<TarExportOffer>,
    pub error: Option<SessionProtocolError>,
    pub updated_at: String,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeContractError {
    VersionCompatibility(VersionCompatibilityError),
    ComponentVersionTooOld {
        component: ProtocolComponent,
        required: u16,
        actual: u16,
    },
    UnsupportedSelectorVersion {
        version: u16,
    },
    EmptyField(&'static str),
    EmptyCollection(&'static str),
    DuplicateValue(&'static str),
    NonCanonicalOrdinal {
        collection: &'static str,
        expected: u32,
        actual: u32,
    },
    InvalidLimit,
    ValueTooLong {
        field: &'static str,
        maximum_bytes: usize,
        actual_bytes: usize,
    },
    InvalidSha256(&'static str),
    InconsistentArchiveEntryCount,
    SelectionExceedsLimits,
    InvalidControlRecord,
    InvalidControlDirectory,
    InvalidControlRecordCount {
        actual: u64,
    },
    NonCanonicalRecordOrder,
    InvalidDirectoryDepth {
        expected: u32,
        actual: u32,
    },
    InvalidParentPath,
    MissingParentDirectory,
    ReceiptDoesNotMatchOffer,
    InventoryTooLarge,
    InventoryDoesNotMatchOffer,
    DeliveredBodyCountMismatch {
        expected: u64,
        actual: u64,
    },
}

impl From<VersionCompatibilityError> for ScopeContractError {
    fn from(error: VersionCompatibilityError) -> Self {
        Self::VersionCompatibility(error)
    }
}

impl Display for ScopeContractError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VersionCompatibility(error) => Display::fmt(error, formatter),
            Self::ComponentVersionTooOld {
                component,
                required,
                actual,
            } => write!(
                formatter,
                "{component:?} version {actual} is older than required version {required}"
            ),
            Self::UnsupportedSelectorVersion { version } => {
                write!(
                    formatter,
                    "source-scope selector version {version} is unsupported"
                )
            }
            Self::EmptyField(field) => write!(formatter, "{field} must not be empty"),
            Self::EmptyCollection(field) => write!(formatter, "{field} must not be empty"),
            Self::DuplicateValue(field) => write!(formatter, "{field} must be unique"),
            Self::NonCanonicalOrdinal {
                collection,
                expected,
                actual,
            } => write!(
                formatter,
                "{collection} ordinal must be {expected}, got {actual}"
            ),
            Self::InvalidLimit => formatter.write_str("export limits must be positive"),
            Self::ValueTooLong {
                field,
                maximum_bytes,
                actual_bytes,
            } => write!(
                formatter,
                "{field} is {actual_bytes} bytes, exceeding {maximum_bytes} bytes"
            ),
            Self::InvalidSha256(field) => {
                write!(
                    formatter,
                    "{field} must be `sha256:` plus 64 lowercase hex digits"
                )
            }
            Self::InconsistentArchiveEntryCount => formatter.write_str(
                "archive_entry_count must equal one control entry plus directories and files",
            ),
            Self::SelectionExceedsLimits => {
                formatter.write_str("sealed export selection exceeds negotiated limits")
            }
            Self::InvalidControlRecord => formatter.write_str(
                "control record must be ordinal zero at the reserved export metadata path",
            ),
            Self::InvalidControlDirectory => {
                formatter.write_str("the implicit .loc control directory must not be emitted")
            }
            Self::InvalidControlRecordCount { actual } => {
                write!(
                    formatter,
                    "canonical export must contain one control record, got {actual}"
                )
            }
            Self::NonCanonicalRecordOrder => {
                formatter.write_str("canonical export records are not in strict canonical order")
            }
            Self::InvalidDirectoryDepth { expected, actual } => write!(
                formatter,
                "directory depth must be {expected}, got {actual}"
            ),
            Self::InvalidParentPath => {
                formatter.write_str("file parent_path does not match logical_path")
            }
            Self::MissingParentDirectory => {
                formatter.write_str("canonical export record is missing its parent directory")
            }
            Self::ReceiptDoesNotMatchOffer => {
                formatter.write_str("completion receipt does not match sealed export offer")
            }
            Self::InventoryTooLarge => {
                formatter.write_str("canonical export inventory exceeds u64 framing")
            }
            Self::InventoryDoesNotMatchOffer => {
                formatter.write_str("canonical export inventory does not match sealed offer")
            }
            Self::DeliveredBodyCountMismatch { expected, actual } => write!(
                formatter,
                "delivered body count must be {expected}, got {actual}"
            ),
        }
    }
}

impl std::error::Error for ScopeContractError {}

fn validate_nonempty(field: &'static str, value: &str) -> Result<(), ScopeContractError> {
    if value.is_empty() {
        return Err(ScopeContractError::EmptyField(field));
    }
    Ok(())
}

fn append_count(output: &mut Vec<u8>, count: usize) -> Result<(), ScopeContractError> {
    let count = u64::try_from(count).map_err(|_| ScopeContractError::InventoryTooLarge)?;
    output.extend_from_slice(&count.to_be_bytes());
    Ok(())
}

fn update_digest_scalar(hasher: &mut Sha256, value: &[u8]) -> Result<(), ScopeContractError> {
    let length = u64::try_from(value.len()).map_err(|_| ScopeContractError::InventoryTooLarge)?;
    hasher.update(length.to_be_bytes());
    hasher.update(value);
    Ok(())
}

fn append_scalar(output: &mut Vec<u8>, value: &[u8]) -> Result<(), ScopeContractError> {
    append_count(output, value.len())?;
    output.extend_from_slice(value);
    Ok(())
}

fn append_text(output: &mut Vec<u8>, value: &str) -> Result<(), ScopeContractError> {
    append_scalar(output, value.as_bytes())
}

fn append_u64(output: &mut Vec<u8>, value: u64) -> Result<(), ScopeContractError> {
    append_scalar(output, &value.to_be_bytes())
}

pub fn projection_file_kind_wire_label(kind: &ProjectionFileKind) -> &'static str {
    match kind {
        ProjectionFileKind::Markdown => "markdown",
        ProjectionFileKind::Text => "text",
        ProjectionFileKind::Json => "json",
        ProjectionFileKind::Yaml => "yaml",
        ProjectionFileKind::Binary => "binary",
        ProjectionFileKind::Directory => "directory",
    }
}

pub fn projection_file_kind_from_wire_label(value: &str) -> Option<ProjectionFileKind> {
    match value {
        "markdown" => Some(ProjectionFileKind::Markdown),
        "text" => Some(ProjectionFileKind::Text),
        "json" => Some(ProjectionFileKind::Json),
        "yaml" => Some(ProjectionFileKind::Yaml),
        "binary" => Some(ProjectionFileKind::Binary),
        "directory" => Some(ProjectionFileKind::Directory),
        _ => None,
    }
}

pub fn source_action_wire_label(action: &SourceAction) -> &'static str {
    match action {
        SourceAction::Read => "read",
        SourceAction::Search => "search",
        SourceAction::DownloadAttachment => "download_attachment",
        SourceAction::Create => "create",
        SourceAction::Update => "update",
        SourceAction::Move => "move",
        SourceAction::Delete => "delete",
        SourceAction::Comment => "comment",
        SourceAction::UpdateProperties => "update_properties",
        SourceAction::ManageSchema => "manage_schema",
    }
}

pub fn source_action_from_wire_label(value: &str) -> Option<SourceAction> {
    match value {
        "read" => Some(SourceAction::Read),
        "search" => Some(SourceAction::Search),
        "download_attachment" => Some(SourceAction::DownloadAttachment),
        "create" => Some(SourceAction::Create),
        "update" => Some(SourceAction::Update),
        "move" => Some(SourceAction::Move),
        "delete" => Some(SourceAction::Delete),
        "comment" => Some(SourceAction::Comment),
        "update_properties" => Some(SourceAction::UpdateProperties),
        "manage_schema" => Some(SourceAction::ManageSchema),
        _ => None,
    }
}

/// Encodes the effective actions carried by an export-v2 file PAX record.
///
/// The wire value is a compact JSON array whose labels are sorted by their
/// UTF-8 bytes. An empty action set is invalid for an exported file.
pub fn canonical_effective_actions_pax_value(actions: &BTreeSet<SourceAction>) -> Option<String> {
    if actions.is_empty() {
        return None;
    }
    let mut labels = actions
        .iter()
        .map(source_action_wire_label)
        .collect::<Vec<_>>();
    labels.sort_unstable();
    Some(format!(r#"["{}"]"#, labels.join(r#"",""#)))
}

/// Decodes an export-v2 effective-actions PAX value only when it is already in
/// the exact canonical encoding accepted by the inventory digest.
pub fn source_actions_from_canonical_pax_value(value: &str) -> Option<BTreeSet<SourceAction>> {
    let labels = value.strip_prefix(r#"[""#)?.strip_suffix(r#""]"#)?;
    let mut actions = BTreeSet::new();
    for label in labels.split(r#"",""#) {
        let action = source_action_from_wire_label(label)?;
        if !actions.insert(action) {
            return None;
        }
    }
    (canonical_effective_actions_pax_value(&actions)?.as_str() == value).then_some(actions)
}

fn validate_sha256(field: &'static str, value: &str) -> Result<(), ScopeContractError> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(ScopeContractError::InvalidSha256(field));
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ScopeContractError::InvalidSha256(field));
    }
    Ok(())
}

fn validate_export_versions(versions: &ComponentVersions) -> Result<(), ScopeContractError> {
    versions.validate_required()?;
    for (component, actual) in [
        (ProtocolComponent::Session, versions.session),
        (ProtocolComponent::Replica, versions.replica),
        (ProtocolComponent::ExportMetadata, versions.export_metadata),
    ] {
        if actual < 2 {
            return Err(ScopeContractError::ComponentVersionTooOld {
                component,
                required: 2,
                actual,
            });
        }
    }
    Ok(())
}

fn validate_export_counts(
    control_entry_count: u64,
    file_count: u64,
    directory_count: u64,
    archive_entry_count: u64,
) -> Result<(), ScopeContractError> {
    if control_entry_count != 1
        || control_entry_count
            .checked_add(directory_count)
            .and_then(|count| count.checked_add(file_count))
            != Some(archive_entry_count)
    {
        return Err(ScopeContractError::InconsistentArchiveEntryCount);
    }
    Ok(())
}

fn validate_selection_limits(
    limits: &ExportAttemptLimits,
    file_count: u64,
    directory_count: u64,
    selected_content_bytes: u64,
) -> Result<(), ScopeContractError> {
    if file_count > limits.max_files
        || directory_count > limits.max_directories
        || selected_content_bytes > limits.max_content_bytes
    {
        return Err(ScopeContractError::SelectionExceedsLimits);
    }
    Ok(())
}

fn path_depth(path: &LogicalPath) -> u32 {
    path.as_str().split('/').count() as u32
}

fn logical_parent(path: &LogicalPath) -> Result<Option<LogicalPath>, ScopeContractError> {
    path.as_str()
        .rsplit_once('/')
        .map(|(parent, _)| {
            LogicalPath::new(parent).map_err(|_| ScopeContractError::InvalidParentPath)
        })
        .transpose()
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
pub const BOOTSTRAP_EXCHANGE_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/bootstrap-exchange.json");
pub const FRESHNESS_STATUS_GOLDEN_JSON: &[u8] = include_bytes!("../fixtures/freshness-status.json");
pub const SANDBOX_SESSION_STATUS_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/sandbox-session-status.json");
pub const SESSION_PROTOCOL_ERROR_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/session-protocol-error.json");
pub const TAR_EXPORT_OFFER_GOLDEN_JSON: &[u8] = include_bytes!("../fixtures/tar-export-offer.json");
pub const TAR_EXPORT_METADATA_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/tar-export-metadata.json");
pub const SCOPE_AUTHORIZED_SESSION_QUERY_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/scope-authorized-session-query.json");
pub const EXPORT_ATTEMPT_REQUEST_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/export-attempt-request.json");
pub const SEALED_EXPORT_OFFER_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/sealed-export-offer.json");
pub const EXPORT_COMPLETION_RECEIPT_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/export-completion-receipt.json");
pub const CANONICAL_EXPORT_RECORDS_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/canonical-export-records.json");
pub const CANONICAL_EXPORT_INVENTORY_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/canonical-export-inventory.json");
