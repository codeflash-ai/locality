//! Connector SDK boundary.
//!
//! First-party connectors implement this trait in-process. The host owns
//! validation, diffing, journals, rate limiting, and conflict handling; a
//! connector owns source-specific enumeration, rendering, concurrency checks,
//! and apply calls.

use afs_core::AfsResult;
use afs_core::journal::PushId;
use afs_core::model::{CanonicalDocument, MountId, RemoteId, TreeEntry};
use afs_core::planner::{PushOperationKind, PushPlan};
use afs_core::push::RemotePrecondition;
use afs_core::undo::{UndoApplier, UndoApplyRequest, UndoApplyResult, UndoPlan};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectorKind(pub &'static str);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectorCapabilities {
    pub supports_block_updates: bool,
    pub supports_databases: bool,
    pub supports_oauth: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumerateRequest {
    pub mount_id: MountId,
    pub cursor: Option<String>,
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
    pub operation_ids: &'a [afs_core::journal::PushOperationId],
    /// Last-synced remote timestamps for compare-and-swap checks.
    pub remote_preconditions: &'a [RemotePrecondition],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyPlanResult {
    pub changed_remote_ids: Vec<RemoteId>,
    pub effects: Vec<afs_core::journal::JournalApplyEffect>,
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
    fn enumerate(&self, request: EnumerateRequest) -> AfsResult<Vec<TreeEntry>>;
    /// List immediate child metadata for a single filesystem container.
    ///
    /// This must not fetch full document bodies. Returning metadata only lets
    /// FileProvider/FUSE make directory navigation lazy while page hydration
    /// remains tied to file open or explicit pull.
    fn list_children(&self, _request: ListChildrenRequest) -> AfsResult<ListChildrenResult> {
        Err(afs_core::AfsError::Unsupported(
            "connector does not support lazy child enumeration",
        ))
    }
    fn fetch(&self, request: FetchRequest) -> AfsResult<NativeEntity>;
    fn render(&self, entity: &NativeEntity) -> AfsResult<CanonicalDocument>;
    fn parse(&self, document: &CanonicalDocument) -> AfsResult<ParsedEntity>;
    /// Re-read source metadata immediately before apply and fail if the remote
    /// moved past the synced preimage.
    fn check_concurrency(&self, request: ApplyPlanRequest<'_>) -> AfsResult<()>;
    /// Apply an approved push plan using source-specific API operations.
    fn apply(&self, request: ApplyPlanRequest<'_>) -> AfsResult<ApplyPlanResult>;
    /// Apply a complete undo plan using source-specific reverse operations.
    fn apply_undo(&self, request: ApplyUndoRequest<'_>) -> AfsResult<ApplyUndoResult>;
}

/// Adapter from a connector's undo method into `afs-core`'s undo hook.
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
    fn apply_undo(&mut self, request: UndoApplyRequest<'_>) -> AfsResult<UndoApplyResult> {
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
