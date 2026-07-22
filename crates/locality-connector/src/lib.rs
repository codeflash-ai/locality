//! Connector SDK boundary.
//!
//! First-party connectors implement this trait in-process. The host owns
//! validation, diffing, journals, rate limiting, and conflict handling; a
//! connector owns source-specific enumeration, rendering, concurrency checks,
//! and apply calls.

use locality_core::LocalityResult;
use locality_core::freshness::RemoteObservation;
use locality_core::journal::PushId;
use locality_core::model::{CanonicalDocument, MountId, RemoteId, TreeEntry};
use locality_core::planner::{PushOperationKind, PushPlan};
use locality_core::portable::{ProjectionEntry, SourceConnectionId, SourceObject};
use locality_core::push::RemotePrecondition;
use locality_core::undo::{UndoApplier, UndoApplyRequest, UndoApplyResult, UndoPlan};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

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
    pub supports_databases: bool,
    pub supports_oauth: bool,
    pub supports_remote_observation: bool,
    pub supports_lazy_child_enumeration: bool,
    pub supports_media_download: bool,
    pub supports_undo: bool,
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
        self.supports_remote_observation || self.supports_lazy_child_enumeration
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableSyncRequest {
    pub source_connection_id: SourceConnectionId,
    pub scope: PortableSourceScope,
    pub checkpoint: PortableCheckpoint,
    #[serde(default)]
    pub hints: Vec<PortableSyncHint>,
    pub max_changes: u32,
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
        PushOperationKind::all().into_iter().collect()
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
        })
    }
}
