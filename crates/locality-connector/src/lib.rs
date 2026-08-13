//! Connector SDK boundary.
//!
//! First-party connectors implement this trait in-process. The host owns
//! validation, diffing, journals, rate limiting, and conflict handling; a
//! connector owns source-specific enumeration, rendering, concurrency checks,
//! and apply calls.

use locality_core::LocalityResult;
use locality_core::freshness::RemoteObservation;
use locality_core::journal::PushId;
use locality_core::model::{CanonicalDocument, EntityKind, MountId, RemoteId, TreeEntry};
use locality_core::planner::{PushOperationKind, PushPlan};
use locality_core::portable::{ProjectionEntry, SourceConnectionId, SourceObject};
use locality_core::push::RemotePrecondition;
use locality_core::undo::{UndoApplier, UndoApplyRequest, UndoApplyResult, UndoPlan};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

pub mod conformance;
pub mod hydration_budget;
pub mod manifest;
pub mod network;
pub mod oauth_broker;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectorKind(pub &'static str);

/// Host-selected execution behavior for connector network operations.
///
/// Connectors still own provider quotas, retry classification, and response
/// decoding. This policy only decides whether a provider cooldown is waited
/// inline or returned to a scheduler that can park the operation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConnectorExecutionPolicy {
    #[default]
    Inline,
    DeferProviderCooldown,
}

impl ConnectorExecutionPolicy {
    pub fn defers_provider_cooldown(self) -> bool {
        self == Self::DeferProviderCooldown
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorCapabilities {
    pub supports_block_updates: bool,
    #[serde(default)]
    pub supports_entity_body_updates: bool,
    pub supports_databases: bool,
    pub supports_oauth: bool,
    pub supports_remote_observation: bool,
    pub supports_lazy_child_enumeration: bool,
    pub supports_media_download: bool,
    pub supports_undo: bool,
    #[serde(default)]
    pub supports_batch_observation: bool,
}

impl ConnectorCapabilities {
    pub fn read_only() -> Self {
        Self {
            supports_remote_observation: true,
            supports_lazy_child_enumeration: true,
            ..Self::default()
        }
    }

    pub fn supports_local_only_stage10(&self) -> bool {
        self.supports_remote_observation
            || self.supports_lazy_child_enumeration
            || self.supports_batch_observation
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumerateRequest {
    pub mount_id: MountId,
    pub cursor: Option<String>,
}

/// Host-neutral request for portable connector enumeration.
///
/// Unlike [`EnumerateRequest`], this carries no local mount or filesystem
/// state. Connectors may adopt it incrementally while the legacy method remains
/// available to direct-mode hosts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortableEnumerateRequest {
    pub source_connection_id: SourceConnectionId,
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableEnumerateResult {
    pub source_objects: Vec<SourceObject>,
    pub projections: Vec<ProjectionEntry>,
    pub next_cursor: Option<String>,
}

/// One explicit provider scope for portable bootstrap and synchronization.
///
/// Roots are provider identities, not titles or projected paths. An empty root
/// list is invalid for connectors, such as Notion, whose provider inventory API
/// cannot prove exhaustive coverage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableSourceScope {
    pub root_remote_ids: Vec<RemoteId>,
}

impl PortableSourceScope {
    pub fn explicit_roots(root_remote_ids: impl IntoIterator<Item = RemoteId>) -> Self {
        Self {
            root_remote_ids: root_remote_ids.into_iter().collect(),
        }
    }
}

/// Opaque, connector-owned progress state.
///
/// Hosts persist and return this value without interpreting `opaque`. The
/// format version lets a connector fail cleanly instead of silently opening a
/// newer or obsolete checkpoint representation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableCheckpoint {
    pub format_version: u16,
    pub opaque: String,
}

/// Stable identity for a rendered artifact.
///
/// Artifact keys must be independent of mutable titles and projected paths.
/// Backend and direct-mode hosts may bind this key to their own durable IDs.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PortableArtifactKey(String);

impl PortableArtifactKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_valid(&self) -> bool {
        !self.0.is_empty() && !self.0.chars().any(char::is_control)
    }
}

/// A reason a connector cannot claim exhaustive coverage for a batch.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PortableIncompleteReason {
    CheckpointContinuation,
    UnsupportedSourceKind {
        remote_id: RemoteId,
        source_kind: String,
    },
    UnsupportedArtifact {
        artifact_key: PortableArtifactKey,
        artifact_kind: String,
    },
    ConnectorLimitation {
        code: String,
        remote_id: Option<RemoteId>,
    },
}

/// Explicit coverage state for bootstrap, sync, fetch, and render results.
///
/// The default is deliberately incomplete so a newly added connector cannot
/// accidentally authorize publication by forgetting to set completeness.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableCompleteness {
    #[serde(default)]
    incomplete_reasons: Vec<PortableIncompleteReason>,
    complete: bool,
}

impl PortableCompleteness {
    pub fn complete() -> Self {
        Self {
            incomplete_reasons: Vec::new(),
            complete: true,
        }
    }

    pub fn incomplete(reason: PortableIncompleteReason) -> Self {
        Self {
            incomplete_reasons: vec![reason],
            complete: false,
        }
    }

    pub fn is_complete(&self) -> bool {
        self.complete && self.incomplete_reasons.is_empty()
    }

    pub fn incomplete_reasons(&self) -> &[PortableIncompleteReason] {
        &self.incomplete_reasons
    }

    pub fn merge(&mut self, other: Self) {
        self.complete &= other.complete;
        self.incomplete_reasons.extend(other.incomplete_reasons);
        self.incomplete_reasons.sort();
        self.incomplete_reasons.dedup();
    }
}

/// Reserved [`SourceObject::edges`] relationship for the canonical explicit
/// scope root that owns a portable source object.
pub const PORTABLE_SCOPE_ROOT_RELATIONSHIP: &str = "locality_scope_root";

/// Decode the optional owning-root edge, rejecting ambiguous source objects.
pub fn portable_scope_root_remote_id(
    source_object: &SourceObject,
) -> LocalityResult<Option<&RemoteId>> {
    let mut roots = source_object
        .edges
        .iter()
        .filter(|edge| edge.relationship == PORTABLE_SCOPE_ROOT_RELATIONSHIP)
        .map(|edge| &edge.target_remote_id);
    let root = roots.next();
    if roots.next().is_some() {
        return Err(locality_core::LocalityError::InvalidState(format!(
            "portable source `{}` returned multiple owning-root edges",
            source_object.remote_id.as_str()
        )));
    }
    Ok(root)
}

/// One provider object discovered by bootstrap or synchronization.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableSourceChange {
    pub source_object: SourceObject,
    /// Current projection hint. It may change after a provider rename and is
    /// never used as source or artifact identity.
    pub logical_path: Option<locality_core::portable::LogicalPath>,
    /// Whether this object has a supported native fetch/render path.
    pub requires_fetch: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableChangeBatch {
    pub changes: Vec<PortableSourceChange>,
    pub next_checkpoint: PortableCheckpoint,
    pub completeness: PortableCompleteness,
}

/// The deletion authority carried by one portable change batch.
///
/// Missing serialized values default to [`Self::Incremental`], so older
/// payloads can never gain deletion authority during an upgrade.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortableBatchAuthority {
    /// Only explicit changes, including explicit tombstones, are authoritative.
    #[default]
    Incremental,
    /// A terminal batch describes the complete requested scope, so omission is
    /// authoritative after the host has validated the complete result.
    CompleteScopeSnapshot,
}

/// Versioned portable synchronization result with explicit omission authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableChangeBatchV2 {
    pub changes: Vec<PortableSourceChange>,
    pub next_checkpoint: PortableCheckpoint,
    pub completeness: PortableCompleteness,
    /// Exact requested roots whose terminal coverage this response proves.
    /// Missing coverage defaults to none and cannot authorize omission.
    #[serde(default)]
    pub covered_root_remote_ids: Vec<RemoteId>,
    /// Whether omission from this batch is meaningful for reconciliation.
    ///
    /// This is independent of [`PortableCompleteness`]: completeness describes
    /// gaps in the returned data, while authority decides whether an omitted
    /// object may be treated as absent from the requested scope.
    #[serde(default)]
    pub authority: PortableBatchAuthority,
}

impl From<PortableChangeBatch> for PortableChangeBatchV2 {
    fn from(batch: PortableChangeBatch) -> Self {
        Self {
            changes: batch.changes,
            next_checkpoint: batch.next_checkpoint,
            completeness: batch.completeness,
            covered_root_remote_ids: Vec::new(),
            authority: PortableBatchAuthority::Incremental,
        }
    }
}

impl PortableChangeBatchV2 {
    /// Validate bounded response fields against the original request and
    /// report whether covered roots exactly equal the requested scope.
    pub fn validate_for_request(
        &self,
        scope: &PortableSourceScope,
        max_changes: u32,
    ) -> LocalityResult<bool> {
        if max_changes == 0 || max_changes > PORTABLE_SYNC_V2_MAX_CHANGES {
            return Err(locality_core::LocalityError::InvalidState(format!(
                "portable sync v2 max_changes must be in 1..={}",
                PORTABLE_SYNC_V2_MAX_CHANGES
            )));
        }
        if self.changes.len() > max_changes as usize {
            return Err(locality_core::LocalityError::InvalidState(format!(
                "portable sync v2 batch has {} changes; request maximum is {}",
                self.changes.len(),
                max_changes
            )));
        }
        if self.next_checkpoint.opaque.len() > PORTABLE_SYNC_V2_MAX_CHECKPOINT_BYTES {
            return Err(locality_core::LocalityError::InvalidState(format!(
                "portable sync v2 response checkpoint is {} UTF-8 bytes; maximum is {}",
                self.next_checkpoint.opaque.len(),
                PORTABLE_SYNC_V2_MAX_CHECKPOINT_BYTES
            )));
        }
        self.has_exact_scope_coverage(scope)
    }

    /// Validate response coverage and report whether it exactly equals the
    /// requested scope. A strict subset is valid but non-authoritative.
    pub fn has_exact_scope_coverage(&self, scope: &PortableSourceScope) -> LocalityResult<bool> {
        if self.covered_root_remote_ids.len() > PORTABLE_SYNC_V2_MAX_SCOPE_ROOTS {
            return Err(locality_core::LocalityError::InvalidState(format!(
                "portable sync v2 batch covers {} scope roots; maximum is {}",
                self.covered_root_remote_ids.len(),
                PORTABLE_SYNC_V2_MAX_SCOPE_ROOTS
            )));
        }
        let requested = validate_portable_sync_v2_scope(scope)?;
        let mut covered = BTreeSet::new();
        for root_remote_id in &self.covered_root_remote_ids {
            validate_portable_sync_v2_id(root_remote_id.as_str(), "covered root remote ID")?;
            if !covered.insert(root_remote_id) {
                return Err(locality_core::LocalityError::InvalidState(
                    "portable sync v2 batch contains duplicate covered root remote IDs".to_string(),
                ));
            }
            if !requested.contains(root_remote_id) {
                return Err(locality_core::LocalityError::InvalidState(
                    "portable sync v2 batch covers a root outside the requested scope".to_string(),
                ));
            }
        }
        Ok(covered == requested)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableBootstrapRequest {
    pub source_connection_id: SourceConnectionId,
    pub scope: PortableSourceScope,
    pub checkpoint: Option<PortableCheckpoint>,
    pub max_changes: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableSyncHint {
    pub remote_id: RemoteId,
}

/// Maximum number of differential hints accepted by one v2 sync request.
pub const PORTABLE_SYNC_V2_MAX_HINTS: usize = 4_096;
/// Maximum number of explicit scope roots accepted by one v2 sync request.
pub const PORTABLE_SYNC_V2_MAX_SCOPE_ROOTS: usize = 256;
/// Maximum provider changes requested in one v2 sync batch.
pub const PORTABLE_SYNC_V2_MAX_CHANGES: u32 = 10_000;
/// Maximum UTF-8 bytes accepted for each v2 source, remote, or owning-root ID.
pub const PORTABLE_SYNC_V2_MAX_ID_BYTES: usize = 1_024;
/// Maximum UTF-8 bytes accepted for a v2 prior provider version.
pub const PORTABLE_SYNC_V2_MAX_PROVIDER_VERSION_BYTES: usize = 1_024;
/// Maximum UTF-8 bytes accepted for a connector-defined v2 source kind.
pub const PORTABLE_SYNC_V2_MAX_SOURCE_KIND_BYTES: usize = 128;
/// Maximum UTF-8 bytes accepted for an opaque v2 connector checkpoint.
pub const PORTABLE_SYNC_V2_MAX_CHECKPOINT_BYTES: usize = 65_536;

/// Differential metadata supplied to a connector by a v2 sync host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableSyncHintV2 {
    pub remote_id: RemoteId,
    /// Last provider version accepted by the host, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_version: Option<String>,
    /// Last accepted projected path, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_path: Option<locality_core::portable::LogicalPath>,
    /// Last accepted connector-owned source kind, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<EntityKind>,
    /// Stable provider identity of the scope root that owned the object.
    /// Validation requires this field and binds it to the request scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owning_root_remote_id: Option<RemoteId>,
}

/// Host intent for one portable synchronization request.
///
/// Missing serialized values default to [`Self::HintsOnly`]. A legacy request
/// therefore cannot accidentally ask a connector for omission-authoritative
/// scope reconciliation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortableSyncMode {
    /// Reconcile only the explicitly supplied hints and provider checkpoint.
    #[default]
    HintsOnly,
    /// Reconcile the entire requested scope. The returned batch still needs
    /// [`PortableBatchAuthority::CompleteScopeSnapshot`] before omission is
    /// authoritative.
    ReconcileScope,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableSyncRequest {
    pub source_connection_id: SourceConnectionId,
    pub scope: PortableSourceScope,
    pub checkpoint: PortableCheckpoint,
    #[serde(default)]
    pub hints: Vec<PortableSyncHint>,
    pub max_changes: u32,
}

/// Versioned portable synchronization request with explicit host intent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableSyncRequestV2 {
    pub source_connection_id: SourceConnectionId,
    pub scope: PortableSourceScope,
    pub checkpoint: PortableCheckpoint,
    #[serde(default)]
    pub mode: PortableSyncMode,
    #[serde(default)]
    pub hints: Vec<PortableSyncHintV2>,
    pub max_changes: u32,
}

impl PortableSyncRequestV2 {
    /// Validate host-controlled bounds and scope relationships before provider
    /// connector dispatch.
    pub fn validate(&self) -> LocalityResult<()> {
        validate_portable_sync_v2_id(self.source_connection_id.as_str(), "source connection ID")?;
        if self.checkpoint.opaque.len() > PORTABLE_SYNC_V2_MAX_CHECKPOINT_BYTES {
            return Err(locality_core::LocalityError::InvalidState(format!(
                "portable sync v2 checkpoint is {} UTF-8 bytes; maximum is {}",
                self.checkpoint.opaque.len(),
                PORTABLE_SYNC_V2_MAX_CHECKPOINT_BYTES
            )));
        }
        if self.hints.len() > PORTABLE_SYNC_V2_MAX_HINTS {
            return Err(locality_core::LocalityError::InvalidState(format!(
                "portable sync v2 has {} hints; maximum is {}",
                self.hints.len(),
                PORTABLE_SYNC_V2_MAX_HINTS
            )));
        }
        if self.max_changes == 0 || self.max_changes > PORTABLE_SYNC_V2_MAX_CHANGES {
            return Err(locality_core::LocalityError::InvalidState(format!(
                "portable sync v2 max_changes must be in 1..={}",
                PORTABLE_SYNC_V2_MAX_CHANGES
            )));
        }

        let scope_roots = validate_portable_sync_v2_scope(&self.scope)?;
        let mut remote_ids = BTreeSet::new();
        for hint in &self.hints {
            validate_portable_sync_v2_id(hint.remote_id.as_str(), "hint remote ID")?;
            if !remote_ids.insert(&hint.remote_id) {
                return Err(locality_core::LocalityError::InvalidState(
                    "portable sync v2 contains duplicate hint remote IDs".to_string(),
                ));
            }
            if hint
                .provider_version
                .as_ref()
                .is_some_and(|version| version.len() > PORTABLE_SYNC_V2_MAX_PROVIDER_VERSION_BYTES)
            {
                return Err(locality_core::LocalityError::InvalidState(format!(
                    "portable sync v2 provider version exceeds {} UTF-8 bytes",
                    PORTABLE_SYNC_V2_MAX_PROVIDER_VERSION_BYTES
                )));
            }
            if let Some(EntityKind::Unknown(source_kind)) = &hint.source_kind
                && source_kind.len() > PORTABLE_SYNC_V2_MAX_SOURCE_KIND_BYTES
            {
                return Err(locality_core::LocalityError::InvalidState(format!(
                    "portable sync v2 source kind exceeds {} UTF-8 bytes",
                    PORTABLE_SYNC_V2_MAX_SOURCE_KIND_BYTES
                )));
            }
            let root_remote_id = hint.owning_root_remote_id.as_ref().ok_or_else(|| {
                locality_core::LocalityError::InvalidState(
                    "portable sync v2 hint must include an owning root".to_string(),
                )
            })?;
            validate_portable_sync_v2_id(root_remote_id.as_str(), "owning root remote ID")?;
            if !scope_roots.contains(root_remote_id) {
                return Err(locality_core::LocalityError::InvalidState(
                    "portable sync v2 hint owning root is outside the request scope".to_string(),
                ));
            }
        }
        Ok(())
    }

    fn into_legacy(self) -> PortableSyncRequest {
        PortableSyncRequest {
            source_connection_id: self.source_connection_id,
            scope: self.scope,
            checkpoint: self.checkpoint,
            hints: self
                .hints
                .into_iter()
                .map(|hint| PortableSyncHint {
                    remote_id: hint.remote_id,
                })
                .collect(),
            max_changes: self.max_changes,
        }
    }
}

fn validate_portable_sync_v2_id(value: &str, label: &str) -> LocalityResult<()> {
    if value.is_empty() || value.len() > PORTABLE_SYNC_V2_MAX_ID_BYTES {
        return Err(locality_core::LocalityError::InvalidState(format!(
            "portable sync v2 {label} must contain 1..={} UTF-8 bytes",
            PORTABLE_SYNC_V2_MAX_ID_BYTES
        )));
    }
    Ok(())
}

fn validate_portable_sync_v2_scope(
    scope: &PortableSourceScope,
) -> LocalityResult<BTreeSet<&RemoteId>> {
    if scope.root_remote_ids.is_empty() {
        return Err(locality_core::LocalityError::InvalidState(
            "portable sync v2 scope must contain at least one root".to_string(),
        ));
    }
    if scope.root_remote_ids.len() > PORTABLE_SYNC_V2_MAX_SCOPE_ROOTS {
        return Err(locality_core::LocalityError::InvalidState(format!(
            "portable sync v2 has {} scope roots; maximum is {}",
            scope.root_remote_ids.len(),
            PORTABLE_SYNC_V2_MAX_SCOPE_ROOTS
        )));
    }
    let mut roots = BTreeSet::new();
    for root_remote_id in &scope.root_remote_ids {
        validate_portable_sync_v2_id(root_remote_id.as_str(), "scope root remote ID")?;
        if !roots.insert(root_remote_id) {
            return Err(locality_core::LocalityError::InvalidState(
                "portable sync v2 contains duplicate scope root remote IDs".to_string(),
            ));
        }
    }
    Ok(roots)
}

/// Validate and dispatch one v2 synchronization request.
///
/// This is the trust boundary for hosts accepting untrusted request values.
/// Connector implementations customize behavior through
/// [`Connector::sync_portable_v2_impl`], which is called only after validation.
pub fn dispatch_portable_sync_v2<C: Connector + ?Sized>(
    connector: &C,
    request: PortableSyncRequestV2,
) -> LocalityResult<PortableChangeBatchV2> {
    request.validate()?;
    connector.sync_portable_v2_impl(request)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortableFetchReason {
    Bootstrap,
    Synchronization,
    Repair,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableFetchRequest {
    pub source_connection_id: SourceConnectionId,
    pub remote_id: RemoteId,
    pub reason: PortableFetchReason,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortableFetchResult {
    pub native: NativeEntity,
    pub provider_version: Option<String>,
    pub completeness: PortableCompleteness,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortableRenderRequest {
    pub source_connection_id: SourceConnectionId,
    pub logical_path: locality_core::portable::LogicalPath,
    pub native: NativeEntity,
    pub format_version: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableContentArtifact {
    pub artifact_key: PortableArtifactKey,
    pub media_type: String,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableProjectionArtifact {
    pub artifact: PortableContentArtifact,
    pub logical_path: locality_core::portable::LogicalPath,
    pub file_kind: locality_core::portable::ProjectionFileKind,
    pub format_version: u32,
    #[serde(default)]
    pub supported_actions: BTreeSet<locality_core::portable::SourceAction>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableRenderResult {
    pub canonical: PortableContentArtifact,
    pub projections: Vec<PortableProjectionArtifact>,
    pub completeness: PortableCompleteness,
}

/// Cheap metadata request for one known source object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObserveRequest {
    pub mount_id: MountId,
    pub remote_id: RemoteId,
}

/// Opaque connector-owned state for the next mount-wide observation batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectorCheckpoint {
    pub state_version: i64,
    pub min_reader_version: i64,
    pub state_json: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchObserveRequest {
    pub mount_id: MountId,
    pub checkpoint: Option<ConnectorCheckpoint>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchObservationChange {
    Upsert(TreeEntry),
    Tombstone { remote_id: RemoteId },
}

/// Whether omitted entities are authoritative for the configured mount scope.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BatchObservationCompleteness {
    Complete,
    #[default]
    Incremental,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchObserveResult {
    pub changes: Vec<BatchObservationChange>,
    pub completeness: BatchObservationCompleteness,
    pub next_checkpoint: ConnectorCheckpoint,
}

impl BatchObserveResult {
    pub fn complete(
        changes: Vec<BatchObservationChange>,
        next_checkpoint: ConnectorCheckpoint,
    ) -> Self {
        Self {
            changes,
            completeness: BatchObservationCompleteness::Complete,
            next_checkpoint,
        }
    }

    pub fn incremental(
        changes: Vec<BatchObservationChange>,
        next_checkpoint: ConnectorCheckpoint,
    ) -> Self {
        Self {
            changes,
            completeness: BatchObservationCompleteness::Incremental,
            next_checkpoint,
        }
    }

    pub fn is_complete(&self) -> bool {
        self.completeness == BatchObservationCompleteness::Complete
    }
}

/// A source-side container whose immediate children can be listed lazily.
///
/// Filesystem backends use this for directory enumeration. It is intentionally
/// source-neutral: a connector maps the variants to its own hierarchy, while
/// the host maps the returned entries into a local path projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChildContainer {
    /// The mount root. For workspace mounts, this is the visible workspace root;
    /// for scoped mounts, this is the configured remote root.
    Root,
    /// Child pages/databases under a page.
    PageChildren(RemoteId),
    /// Row pages under a database-like collection.
    DatabaseRows(RemoteId),
    /// Child entities under a source folder/directory.
    DirectoryChildren(RemoteId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListChildrenRequest {
    pub mount_id: MountId,
    pub container: ChildContainer,
    /// Path of the local directory receiving these children.
    pub parent_path: std::path::PathBuf,
}

/// Whether a child listing is a complete snapshot of a container or only a
/// mergeable subset.
///
/// Hosts may remove locally known children that are absent from a complete
/// listing. Incremental listings must only upsert the returned entries because
/// absence does not mean that a remote child was deleted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChildListingCompleteness {
    Complete,
    #[default]
    Incremental,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListChildrenResult {
    pub entries: Vec<TreeEntry>,
    pub completeness: ChildListingCompleteness,
}

impl ListChildrenResult {
    pub fn complete(entries: Vec<TreeEntry>) -> Self {
        Self {
            entries,
            completeness: ChildListingCompleteness::Complete,
        }
    }

    pub fn incremental(entries: Vec<TreeEntry>) -> Self {
        Self {
            entries,
            completeness: ChildListingCompleteness::Incremental,
        }
    }

    pub fn is_complete(&self) -> bool {
        self.completeness == ChildListingCompleteness::Complete
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchRequest {
    pub remote_id: RemoteId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeEntity {
    pub remote_id: RemoteId,
    pub kind: String,
    pub raw: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedEntity {
    pub remote_id: RemoteId,
    pub native: NativeEntity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyPlanRequest<'a> {
    /// Stable push identifier used for idempotency keys and request tracing.
    pub push_id: &'a PushId,
    /// Mount whose source account/workspace is being mutated.
    pub mount_id: &'a MountId,
    /// Connector-neutral plan approved by the core pipeline.
    pub plan: &'a PushPlan,
    /// Stable idempotency keys aligned to `plan.operations`.
    pub operation_ids: &'a [locality_core::journal::PushOperationId],
    /// Synced Tree remote versions for compare-and-swap checks.
    pub remote_preconditions: &'a [RemotePrecondition],
    /// Local mount/output root for operations that need local sidecar files.
    pub local_root: Option<&'a Path>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyPlanResult {
    pub changed_remote_ids: Vec<RemoteId>,
    pub effects: Vec<locality_core::journal::JournalApplyEffect>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyUndoRequest<'a> {
    /// Push being reversed.
    pub target_push_id: &'a PushId,
    /// Mount whose source account/workspace is being mutated.
    pub mount_id: &'a MountId,
    /// Connector-neutral undo plan derived by core.
    pub plan: &'a UndoPlan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyUndoResult {
    pub changed_remote_ids: Vec<RemoteId>,
    pub observations: Vec<RemoteObservation>,
}

pub trait Connector {
    fn with_execution_policy(&self, _policy: ConnectorExecutionPolicy) -> Self
    where
        Self: Sized + Clone,
    {
        self.clone()
    }

    fn kind(&self) -> ConnectorKind;
    fn capabilities(&self) -> ConnectorCapabilities;
    fn supported_push_operations(&self) -> std::collections::BTreeSet<PushOperationKind> {
        let mut operations = PushOperationKind::all()
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        if !self.capabilities().supports_entity_body_updates {
            operations.remove(&PushOperationKind::UpdateEntityBody);
        }
        operations
    }
    fn enumerate(&self, request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>>;
    /// Enumerate provider state without binding it to local mount semantics.
    ///
    /// The default is intentionally explicit: a host must use the legacy
    /// `enumerate` API until a connector supplies stable projection and source
    /// version identities. Falling back by deriving identity from title or path
    /// would corrupt remote identity.
    fn enumerate_portable(
        &self,
        _request: PortableEnumerateRequest,
    ) -> LocalityResult<PortableEnumerateResult> {
        Err(locality_core::LocalityError::Unsupported(
            "connector does not support portable enumeration",
        ))
    }
    /// Start or resume an exhaustive provider inventory for an explicit scope.
    fn bootstrap_portable(
        &self,
        _request: PortableBootstrapRequest,
    ) -> LocalityResult<PortableChangeBatch> {
        Err(locality_core::LocalityError::Unsupported(
            "connector does not support portable bootstrap",
        ))
    }
    /// Observe changes since a connector-owned checkpoint.
    fn sync_portable(&self, _request: PortableSyncRequest) -> LocalityResult<PortableChangeBatch> {
        Err(locality_core::LocalityError::Unsupported(
            "connector does not support portable synchronization",
        ))
    }
    /// Compatibility entry point for v2 portable synchronization.
    ///
    /// The default calls [`dispatch_portable_sync_v2`]. Because Rust trait
    /// methods remain overrideable, hosts must use that free dispatcher rather
    /// than trusting a connector override of this method to validate input.
    /// Connector implementations should override only
    /// [`Self::sync_portable_v2_impl`].
    fn sync_portable_v2(
        &self,
        request: PortableSyncRequestV2,
    ) -> LocalityResult<PortableChangeBatchV2> {
        dispatch_portable_sync_v2(self, request)
    }
    /// Connector implementation hook for already validated v2 requests.
    ///
    /// Hosts must not call this directly. The default compatibility adapter
    /// forwards only legacy fields to [`Self::sync_portable`] and always
    /// returns incremental authority. Connectors override this hook to honor
    /// [`PortableSyncMode::ReconcileScope`] or return complete-scope authority.
    fn sync_portable_v2_impl(
        &self,
        request: PortableSyncRequestV2,
    ) -> LocalityResult<PortableChangeBatchV2> {
        self.sync_portable(request.into_legacy()).map(Into::into)
    }
    /// Fetch one authoritative native provider object.
    fn fetch_portable(
        &self,
        _request: PortableFetchRequest,
    ) -> LocalityResult<PortableFetchResult> {
        Err(locality_core::LocalityError::Unsupported(
            "connector does not support portable fetch",
        ))
    }
    /// Render one native object into canonical and projected artifacts.
    fn render_portable(
        &self,
        _request: &PortableRenderRequest,
    ) -> LocalityResult<PortableRenderResult> {
        Err(locality_core::LocalityError::Unsupported(
            "connector does not support portable render",
        ))
    }
    /// Observe one entity without hydrating its body.
    ///
    /// Implementations should return identity, display metadata, parent/path
    /// hints, deletion state, and an opaque remote version when available.
    /// Hosts use this for freshness scheduling; push preflight still performs
    /// authoritative connector-specific concurrency checks.
    fn observe(&self, _request: ObserveRequest) -> LocalityResult<RemoteObservation> {
        Err(locality_core::LocalityError::Unsupported(
            "connector does not support remote observation",
        ))
    }
    /// Observe a mount-wide batch of metadata changes without hydrating bodies.
    fn observe_batch(&self, _request: BatchObserveRequest) -> LocalityResult<BatchObserveResult> {
        Err(locality_core::LocalityError::Unsupported(
            "connector does not support batch observation",
        ))
    }
    /// List immediate child metadata for a single filesystem container.
    ///
    /// This must not fetch full document bodies. Returning metadata only lets
    /// FileProvider/FUSE make directory navigation lazy while page hydration
    /// remains tied to file open or explicit pull. Results must declare whether
    /// they are a complete container snapshot. Incremental results are merged
    /// and never authorize deletion of omitted children.
    fn list_children(&self, _request: ListChildrenRequest) -> LocalityResult<ListChildrenResult> {
        Err(locality_core::LocalityError::Unsupported(
            "connector does not support lazy child enumeration",
        ))
    }
    fn fetch(&self, request: FetchRequest) -> LocalityResult<NativeEntity>;
    fn render(&self, entity: &NativeEntity) -> LocalityResult<CanonicalDocument>;
    fn parse(&self, document: &CanonicalDocument) -> LocalityResult<ParsedEntity>;
    /// Re-read source metadata immediately before apply and fail if the Remote
    /// Tree moved past the Synced Tree preimage.
    fn check_concurrency(&self, request: ApplyPlanRequest<'_>) -> LocalityResult<()>;
    /// Apply an approved push plan using source-specific API operations.
    fn apply(&self, request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult>;
    /// Apply a complete undo plan using source-specific reverse operations.
    fn apply_undo(&self, request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult>;
}

/// Adapter from a connector's undo method into `locality-core`'s undo hook.
pub struct ConnectorUndoApplier<'a, C>
where
    C: Connector + ?Sized,
{
    connector: &'a C,
}

impl<'a, C> ConnectorUndoApplier<'a, C>
where
    C: Connector + ?Sized,
{
    pub fn new(connector: &'a C) -> Self {
        Self { connector }
    }
}

impl<C> UndoApplier for ConnectorUndoApplier<'_, C>
where
    C: Connector + ?Sized,
{
    fn apply_undo(&mut self, request: UndoApplyRequest<'_>) -> LocalityResult<UndoApplyResult> {
        let result = self.connector.apply_undo(ApplyUndoRequest {
            target_push_id: request.target_push_id,
            mount_id: request.mount_id,
            plan: request.plan,
        })?;

        Ok(UndoApplyResult {
            changed_remote_ids: result.changed_remote_ids,
            observations: result.observations,
        })
    }
}
