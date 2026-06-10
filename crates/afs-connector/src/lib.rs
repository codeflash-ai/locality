//! Connector SDK boundary.
//!
//! First-party connectors implement this trait in-process. The host owns
//! validation, diffing, journals, rate limiting, and conflict handling; a
//! connector owns source-specific enumeration, rendering, concurrency checks,
//! and apply calls.

use afs_core::AfsResult;
use afs_core::journal::PushId;
use afs_core::model::{CanonicalDocument, MountId, RemoteId, TreeEntry};
use afs_core::planner::PushPlan;
use afs_core::push::{
    PushApplier, PushApplyRequest, PushApplyResult, PushConcurrencyCheck, PushConcurrencyRequest,
};
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
    fn enumerate(&self, request: EnumerateRequest) -> AfsResult<Vec<TreeEntry>>;
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

/// Adapter from a connector's concurrency check into `afs-core`'s executor hook.
pub struct ConnectorPushConcurrencyCheck<'a, C>
where
    C: Connector + ?Sized,
{
    connector: &'a C,
}

impl<'a, C> ConnectorPushConcurrencyCheck<'a, C>
where
    C: Connector + ?Sized,
{
    pub fn new(connector: &'a C) -> Self {
        Self { connector }
    }
}

impl<C> PushConcurrencyCheck for ConnectorPushConcurrencyCheck<'_, C>
where
    C: Connector + ?Sized,
{
    fn check(&mut self, request: PushConcurrencyRequest<'_>) -> AfsResult<()> {
        self.connector.check_concurrency(ApplyPlanRequest {
            push_id: request.push_id,
            mount_id: request.mount_id,
            plan: request.plan,
            operation_ids: request.operation_ids,
        })
    }
}

/// Adapter from a connector's apply method into `afs-core`'s executor hook.
pub struct ConnectorPushApplier<'a, C>
where
    C: Connector + ?Sized,
{
    connector: &'a C,
}

impl<'a, C> ConnectorPushApplier<'a, C>
where
    C: Connector + ?Sized,
{
    pub fn new(connector: &'a C) -> Self {
        Self { connector }
    }
}

impl<C> PushApplier for ConnectorPushApplier<'_, C>
where
    C: Connector + ?Sized,
{
    fn apply(&mut self, request: PushApplyRequest<'_>) -> AfsResult<PushApplyResult> {
        let result = self.connector.apply(ApplyPlanRequest {
            push_id: request.push_id,
            mount_id: request.mount_id,
            plan: request.plan,
            operation_ids: request.operation_ids,
        })?;

        Ok(PushApplyResult {
            changed_remote_ids: result.changed_remote_ids,
            effects: result.effects,
        })
    }
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
