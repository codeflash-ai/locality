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

pub const GENERATION_DELIVERY_COMPONENT_VERSION: i64 = 5;

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
    /// Actual local working path. This may differ from the remote logical path
    /// while a rename is conflicted and must survive subsequent generations.
    pub local_logical_path: String,
    pub base_generation_id: SourceGenerationId,
    pub base_identity: Option<GenerationFileIdentity>,
    /// Authenticated staged payload that contains the exact merge-base bytes.
    pub base_payload_delta_id: Option<String>,
    pub base_payload_entry_index: Option<u64>,
    /// Exact retained incoming payload for a conflict, when quota admitted it.
    pub conflict_payload_delta_id: Option<String>,
    pub conflict_payload_entry_index: Option<u64>,
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
    /// The local conflict is durable, but its incoming bytes could not be
    /// retained within the configured evidence quota. The metadata remains
    /// terminal so unrelated entries and the generation can advance.
    ConflictOverQuota {
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
    /// The second local inode retained when a late write races a published
    /// merge. Keeping this inode named means a descriptor opened on the
    /// published merge cannot become unreachable after conflict conversion.
    pub visible_evidence: Option<GenerationRetainedInodeRecord>,
    /// Merge-base payload lineage that must be restored if a late writer turns
    /// this completed apply into a local conflict.
    pub base_payload_delta_id: Option<String>,
    pub base_payload_entry_index: Option<u64>,
    /// Set only after an exact retained version has been durably selected and
    /// the path was atomically advanced. Tombstoned evidence remains named and
    /// quota-accounted until a future exclusive, no-active-mount GC gate.
    pub resolved_at: Option<String>,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationRetainedInodeRecord {
    pub evidence_name: String,
    pub expected_sha256: String,
    pub byte_length: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationInodeEvidenceConflictUpdate {
    /// Digest of the small conflict manifest at the logical path.
    pub local_sha256: String,
    /// Current fingerprint of the pre-merge retained inode.
    pub expected_sha256: String,
    pub byte_length: u64,
    /// Current fingerprint of the retained visible merged inode.
    pub visible_evidence: Option<GenerationRetainedInodeRecord>,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationInodeEvidenceResolution {
    pub expected_sha256: String,
    pub byte_length: u64,
    pub visible_expected_sha256: String,
    pub visible_byte_length: u64,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationInodeEvidenceTombstoneRefresh {
    pub expected_sha256: String,
    pub byte_length: u64,
    pub visible_expected_sha256: String,
    pub visible_byte_length: u64,
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

    /// Converts a completed apply to conflict and atomically advances both
    /// retained-inode fences, including their quota-accounted lengths.
    fn mark_generation_inode_evidence_conflict(
        &mut self,
        delta_id: &str,
        entry_index: u64,
        update: GenerationInodeEvidenceConflictUpdate,
    ) -> StoreResult<()>;

    /// Atomically clears the late-write conflict after the visible file was
    /// durably replaced by one exact retained version. Both retained inodes
    /// and their evidence remain as a quota-accounted tombstoned GC journal.
    fn mark_generation_inode_evidence_resolved(
        &mut self,
        delta_id: &str,
        entry_index: u64,
        resolution: GenerationInodeEvidenceResolution,
    ) -> StoreResult<()>;

    /// Atomically refreshes both fingerprints of an already-resolved dual
    /// inode tombstone. This never changes path/apply state or retires files.
    fn refresh_generation_inode_evidence_tombstone(
        &mut self,
        delta_id: &str,
        entry_index: u64,
        refresh: GenerationInodeEvidenceTombstoneRefresh,
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
