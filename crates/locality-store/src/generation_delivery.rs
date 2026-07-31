//! Durable local state for generation-aware differential delivery.
//!
//! The protocol crate owns portable metadata and canonical receipts. This
//! module owns only the local SQLite repository contract: observed mount heads,
//! per-path merge bases, and resumable apply journals.

use locality_core::model::MountId;
use locality_core::portable::{ProjectionId, SourceConnectionId, SourceGenerationId};
use locality_protocol::freshness_delivery::{
    GenerationDelta, GenerationDeltaTerminalReceipt, GenerationFileIdentity,
};
use serde::{Deserialize, Serialize};

use crate::{StoreError, StoreResult};

pub const GENERATION_DELIVERY_COMPONENT_VERSION: i64 = 2;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedGenerationRecord {
    pub mount_id: MountId,
    pub source_connection_id: SourceConnectionId,
    pub generation_id: SourceGenerationId,
    pub inventory_sha256: String,
    pub workspace_layout_version: u16,
    pub workspace_layout_digest: String,
    pub last_receipt_sha256: Option<String>,
    pub updated_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationPathState {
    Clean,
    Dirty,
    Conflicted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationPathRecord {
    pub mount_id: MountId,
    pub projection_id: ProjectionId,
    pub logical_path: String,
    pub base_generation_id: SourceGenerationId,
    pub base_identity: Option<GenerationFileIdentity>,
    /// Authenticated staged payload that contains the exact merge-base bytes.
    pub base_payload_delta_id: Option<String>,
    pub base_payload_entry_index: Option<u64>,
    pub state: GenerationPathState,
    pub incoming_identity: Option<GenerationFileIdentity>,
    pub updated_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationApplyStatus {
    Staged,
    Applying,
    Completed,
}

impl GenerationApplyStatus {
    pub const fn is_active(self) -> bool {
        !matches!(self, Self::Completed)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::Applying => "applying",
            Self::Completed => "completed",
        }
    }

    pub fn parse(value: &str) -> StoreResult<Self> {
        match value {
            "staged" => Ok(Self::Staged),
            "applying" => Ok(Self::Applying),
            "completed" => Ok(Self::Completed),
            _ => Err(StoreError::InvalidState(format!(
                "unknown generation apply status `{value}`"
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GenerationApplyOutcome {
    Applied,
    /// Remote advanced and a clean three-way merge retained local edits.
    Merged,
    Deleted,
    Conflict {
        local_sha256: Option<String>,
        incoming_identity: Option<GenerationFileIdentity>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedGenerationApply {
    pub delta: GenerationDelta,
    pub receipt: GenerationDeltaTerminalReceipt,
    pub receipt_sha256: String,
    pub stage_root: String,
    pub created_at: String,
}

impl PreparedGenerationApply {
    pub fn validate(&self) -> StoreResult<()> {
        self.receipt
            .validate_against(&self.delta)
            .map_err(|error| StoreError::InvalidState(error.to_string()))?;
        if self.receipt_sha256
            != self
                .receipt
                .canonical_sha256()
                .map_err(|error| StoreError::InvalidState(error.to_string()))?
        {
            return Err(StoreError::InvalidState(
                "generation apply receipt digest mismatch".to_string(),
            ));
        }
        if self.stage_root.is_empty() || self.created_at.is_empty() {
            return Err(StoreError::InvalidState(
                "generation apply stage root and timestamp must not be empty".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationApplyJournalRecord {
    pub delta: GenerationDelta,
    pub receipt: GenerationDeltaTerminalReceipt,
    pub receipt_sha256: String,
    pub stage_root: String,
    pub status: GenerationApplyStatus,
    pub outcomes: Vec<(u64, GenerationApplyOutcome)>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationInodeEvidenceRecord {
    pub delta_id: String,
    pub entry_index: u64,
    pub mount_id: MountId,
    pub logical_path: String,
    pub evidence_name: String,
    pub expected_sha256: String,
    pub byte_length: u64,
    pub created_at: String,
}

pub trait GenerationDeliveryRepository {
    /// Seeds or exactly replays a complete local base. Replacing a different
    /// observed head is intentionally not part of this operation.
    fn seed_observed_generation(
        &mut self,
        observed: ObservedGenerationRecord,
        paths: Vec<GenerationPathRecord>,
    ) -> StoreResult<()>;

    fn get_observed_generation(
        &self,
        mount_id: &MountId,
    ) -> StoreResult<Option<ObservedGenerationRecord>>;

    fn list_generation_paths(&self, mount_id: &MountId) -> StoreResult<Vec<GenerationPathRecord>>;

    /// Reserves one immutable apply. Exact replay returns the existing journal;
    /// a changed payload or a concurrent source apply fails closed.
    fn reserve_generation_apply(
        &mut self,
        prepared: PreparedGenerationApply,
    ) -> StoreResult<GenerationApplyJournalRecord>;

    fn mark_generation_apply_started(
        &mut self,
        delta_id: &str,
        updated_at: &str,
    ) -> StoreResult<GenerationApplyJournalRecord>;

    fn record_generation_apply_outcome(
        &mut self,
        delta_id: &str,
        entry_index: u64,
        outcome: GenerationApplyOutcome,
        updated_at: &str,
    ) -> StoreResult<GenerationApplyJournalRecord>;

    fn get_generation_apply(
        &self,
        delta_id: &str,
    ) -> StoreResult<Option<GenerationApplyJournalRecord>>;

    fn list_active_generation_applies(&self) -> StoreResult<Vec<GenerationApplyJournalRecord>>;

    /// Lists active and completed journals so the staging owner can reconcile
    /// retained conflict evidence and discard non-live payloads.
    fn list_generation_applies(&self) -> StoreResult<Vec<GenerationApplyJournalRecord>>;

    fn record_generation_inode_evidence(
        &mut self,
        evidence: GenerationInodeEvidenceRecord,
    ) -> StoreResult<()>;

    fn list_generation_inode_evidence(&self) -> StoreResult<Vec<GenerationInodeEvidenceRecord>>;

    fn mark_generation_inode_evidence_conflict(
        &mut self,
        delta_id: &str,
        entry_index: u64,
        local_sha256: &str,
        updated_at: &str,
    ) -> StoreResult<()>;

    fn remove_generation_inode_evidence(
        &mut self,
        delta_id: &str,
        entry_index: u64,
    ) -> StoreResult<()>;

    /// Atomically advances every affected mount head and its per-path bases
    /// after every journal entry has a terminal local outcome.
    fn complete_generation_apply(
        &mut self,
        delta_id: &str,
        completed_at: &str,
    ) -> StoreResult<GenerationApplyJournalRecord>;
}
