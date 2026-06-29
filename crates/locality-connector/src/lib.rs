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
use locality_core::push::RemotePrecondition;
use locality_core::undo::{UndoApplier, UndoApplyRequest, UndoApplyResult, UndoPlan};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub mod oauth_broker;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectorKind(pub &'static str);

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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListChildrenRequest {
    pub mount_id: MountId,
    pub container: ChildContainer,
    /// Path of the local directory receiving these children.
    pub parent_path: std::path::PathBuf,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListChildrenResult {
    pub entries: Vec<TreeEntry>,
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
    fn kind(&self) -> ConnectorKind;
    fn capabilities(&self) -> ConnectorCapabilities;
    fn supported_push_operations(&self) -> std::collections::BTreeSet<PushOperationKind> {
        PushOperationKind::all().into_iter().collect()
    }
    fn enumerate(&self, request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>>;
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
    /// remains tied to file open or explicit pull.
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
