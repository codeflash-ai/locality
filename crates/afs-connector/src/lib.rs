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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyPlanResult {
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
        })?;

        Ok(PushApplyResult {
            changed_remote_ids: result.changed_remote_ids,
        })
    }
}
