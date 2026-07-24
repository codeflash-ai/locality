//! Apply/reconcile workflow boundary.
//!
//! Phase 0 keeps this as a narrow portable port. Journal transitions,
//! connector apply, and authoritative read-back are composed here only when
//! the Phase 3 backend path needs them.

use locality_core::LocalityResult;
use locality_core::model::RemoteId;
use locality_core::portable::{ChangesetId, SourceOperationPlan};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyReconcileRequest {
    pub changeset_id: ChangesetId,
    pub operations: SourceOperationPlan,
    pub preconditions: Vec<SourcePrecondition>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourcePrecondition {
    pub remote_id: RemoteId,
    pub opaque_version: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyReconcileResult {
    pub changed_remote_ids: Vec<RemoteId>,
    pub reconciled: bool,
}

pub trait ApplyAndReconcileWorkflow {
    fn apply_and_reconcile(
        &self,
        request: ApplyReconcileRequest,
    ) -> LocalityResult<ApplyReconcileResult>;
}
