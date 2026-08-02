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
use locality_protocol::freshness_delivery_transport::GenerationTransportCapabilities;
use locality_protocol::generation_baseline::GenerationBaselineRefreshModeV1;
use serde::{Deserialize, Serialize};

use crate::{StoreError, StoreResult};

pub const GENERATION_DELIVERY_COMPONENT_VERSION: i64 = 7;

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

/// One source's complete observed head and merge-base inventory. A baseline
/// containing multiple sources must be committed through the repository's
/// batch API so callers never expose only part of a shared mount.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationBaselineSeedRecord {
    pub observed: ObservedGenerationRecord,
    pub paths: Vec<GenerationPathRecord>,
}

impl GenerationBaselineSeedRecord {
    pub const fn new(observed: ObservedGenerationRecord, paths: Vec<GenerationPathRecord>) -> Self {
        Self { observed, paths }
    }
}

/// Additive baseline seed envelope that preserves the authenticated refresh
/// route selected for one mount/source state. The original seed record remains
/// source-compatible and means generation-delta V1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationBaselineSeedRecordV2 {
    pub seed: GenerationBaselineSeedRecord,
    pub refresh_mode: GenerationBaselineRefreshModeV1,
}

impl GenerationBaselineSeedRecordV2 {
    pub const fn new(
        seed: GenerationBaselineSeedRecord,
        refresh_mode: GenerationBaselineRefreshModeV1,
    ) -> Self {
        Self { seed, refresh_mode }
    }
}

/// Additive observed-head view containing the durable refresh route. Older
/// repository implementations safely default their released rows to the only
/// route they could previously represent: generation-delta V1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedGenerationRecordV2 {
    pub observed: ObservedGenerationRecord,
    pub refresh_mode: GenerationBaselineRefreshModeV1,
}

impl ObservedGenerationRecordV2 {
    pub const fn new(
        observed: ObservedGenerationRecord,
        refresh_mode: GenerationBaselineRefreshModeV1,
    ) -> Self {
        Self {
            observed,
            refresh_mode,
        }
    }
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

/// Additive reservation envelope for delivery capabilities that need durable
/// state beyond the original generation-apply repository contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedGenerationApplyV2 {
    pub apply: PreparedGenerationApply,
    pub acknowledgment_required: bool,
}

impl PreparedGenerationApplyV2 {
    pub const fn new(apply: PreparedGenerationApply, acknowledgment_required: bool) -> Self {
        Self {
            apply,
            acknowledgment_required,
        }
    }
}

/// Reservation envelope that durably binds the complete authenticated server
/// selection. The selection controls every replay of this apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedGenerationApplyV3 {
    pub apply: PreparedGenerationApply,
    pub selected_capabilities: GenerationTransportCapabilities,
}

impl PreparedGenerationApplyV3 {
    pub const fn new(
        apply: PreparedGenerationApply,
        selected_capabilities: GenerationTransportCapabilities,
    ) -> Self {
        Self {
            apply,
            selected_capabilities,
        }
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

/// Additive journal view containing either the immutable negotiated transport
/// selection or the explicit terminal-only compatibility state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NegotiatedGenerationApplyJournalRecord {
    pub apply: GenerationApplyJournalRecord,
    pub selection_binding: GenerationTransportSelectionBinding,
}

/// Whether a journal has a complete authenticated transport selection. A
/// pre-binding journal is legal only after completion: replay is an exact
/// delta/receipt no-op and only its recorded acknowledgment bit remains live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GenerationTransportSelectionBinding {
    Bound(GenerationTransportCapabilities),
    PreBindingCompleted {
        terminal_receipt_acknowledgments: bool,
    },
}

impl GenerationTransportSelectionBinding {
    pub fn selected_capabilities(&self) -> Option<&GenerationTransportCapabilities> {
        match self {
            Self::Bound(capabilities) => Some(capabilities),
            Self::PreBindingCompleted { .. } => None,
        }
    }

    pub const fn terminal_receipt_acknowledgments(&self) -> bool {
        match self {
            Self::Bound(capabilities) => capabilities.terminal_receipt_acknowledgments,
            Self::PreBindingCompleted {
                terminal_receipt_acknowledgments,
            } => *terminal_receipt_acknowledgments,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationInodeEvidenceRecord {
    pub delta_id: String,
    pub entry_index: u64,
    pub mount_id: MountId,
    pub logical_path: String,
    pub evidence_name: String,
    /// Digest captured while Locality owned the evidence transition. This is
    /// not a live fingerprint after resolution: a foreign descriptor may
    /// subsequently write the retained inode.
    pub captured_sha256: String,
    /// Bytes captured and reserved as Locality-managed evidence. Later growth
    /// through a foreign descriptor is reachable user recovery data, but is
    /// outside this managed-evidence reservation.
    pub captured_byte_length: u64,
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
    /// reserved at its captured lengths until a future exclusive,
    /// no-active-mount GC gate.
    pub resolved_at: Option<String>,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationRetainedInodeRecord {
    pub evidence_name: String,
    pub captured_sha256: String,
    pub captured_byte_length: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationInodeEvidenceConflictUpdate {
    /// Digest of the small conflict manifest at the logical path.
    pub local_sha256: String,
    /// Fingerprint captured from the pre-merge retained inode.
    pub captured_sha256: String,
    pub captured_byte_length: u64,
    /// Fingerprint captured from the retained visible merged inode.
    pub visible_evidence: Option<GenerationRetainedInodeRecord>,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationInodeEvidenceResolution {
    pub captured_sha256: String,
    pub captured_byte_length: u64,
    pub visible_captured_sha256: String,
    pub visible_captured_byte_length: u64,
    pub updated_at: String,
}

pub trait GenerationDeliveryRepository {
    /// Seeds or exactly replays a complete local base. Replacing a different
    /// observed head is intentionally not part of this operation.
    fn seed_observed_generation(
        &mut self,
        observed: ObservedGenerationRecord,
        paths: Vec<GenerationPathRecord>,
    ) -> StoreResult<()>;

    /// Atomically seeds every source record in one authenticated baseline.
    /// Legacy repositories safely support only a one-source batch.
    fn seed_observed_generations(
        &mut self,
        mut seeds: Vec<GenerationBaselineSeedRecord>,
    ) -> StoreResult<()> {
        if seeds.len() != 1 {
            return Err(StoreError::InvalidState(
                "generation repository does not support atomic multi-source baseline seeding"
                    .to_string(),
            ));
        }
        let seed = seeds.pop().expect("one seed was checked");
        self.seed_observed_generation(seed.observed, seed.paths)
    }

    /// Atomically seeds refresh-mode-aware source records. Legacy repositories
    /// can preserve only the released generation-delta route and reject a
    /// full-export-only state instead of later polling it as a delta source.
    fn seed_observed_generations_v2(
        &mut self,
        seeds: Vec<GenerationBaselineSeedRecordV2>,
    ) -> StoreResult<()> {
        if seeds
            .iter()
            .any(|seed| seed.refresh_mode != GenerationBaselineRefreshModeV1::GenerationDeltaV1)
        {
            return Err(StoreError::InvalidState(
                "generation repository does not support full-export-only baseline state"
                    .to_string(),
            ));
        }
        self.seed_observed_generations(seeds.into_iter().map(|seed| seed.seed).collect())
    }

    fn get_observed_generation(
        &self,
        mount_id: &MountId,
    ) -> StoreResult<Option<ObservedGenerationRecord>>;

    /// Reads one exact mount/source head. The default preserves compatibility
    /// with repositories that can contain only one source per mount.
    fn get_observed_generation_for_source(
        &self,
        mount_id: &MountId,
        source_connection_id: &SourceConnectionId,
    ) -> StoreResult<Option<ObservedGenerationRecord>> {
        self.get_observed_generation(mount_id).map(|observed| {
            observed.filter(|record| &record.source_connection_id == source_connection_id)
        })
    }

    fn get_observed_generation_for_source_v2(
        &self,
        mount_id: &MountId,
        source_connection_id: &SourceConnectionId,
    ) -> StoreResult<Option<ObservedGenerationRecordV2>> {
        self.get_observed_generation_for_source(mount_id, source_connection_id)
            .map(|record| {
                record.map(|observed| {
                    ObservedGenerationRecordV2::new(
                        observed,
                        GenerationBaselineRefreshModeV1::GenerationDeltaV1,
                    )
                })
            })
    }

    /// Lists source heads in deterministic source-ID order.
    fn list_observed_generations(
        &self,
        mount_id: &MountId,
    ) -> StoreResult<Vec<ObservedGenerationRecord>> {
        self.get_observed_generation(mount_id)
            .map(|record| record.into_iter().collect())
    }

    fn list_observed_generations_v2(
        &self,
        mount_id: &MountId,
    ) -> StoreResult<Vec<ObservedGenerationRecordV2>> {
        self.list_observed_generations(mount_id).map(|records| {
            records
                .into_iter()
                .map(|observed| {
                    ObservedGenerationRecordV2::new(
                        observed,
                        GenerationBaselineRefreshModeV1::GenerationDeltaV1,
                    )
                })
                .collect()
        })
    }

    fn list_generation_paths(&self, mount_id: &MountId) -> StoreResult<Vec<GenerationPathRecord>>;

    /// Lists merge bases owned by one exact mount/source pair.
    fn list_generation_paths_for_source(
        &self,
        mount_id: &MountId,
        source_connection_id: &SourceConnectionId,
    ) -> StoreResult<Vec<GenerationPathRecord>> {
        if self
            .get_observed_generation_for_source(mount_id, source_connection_id)?
            .is_none()
        {
            return Ok(Vec::new());
        }
        self.list_generation_paths(mount_id)
    }

    /// Retires completed clean delivery lineage for one exact source without
    /// disturbing other source heads on the mount.
    fn reset_observed_generation_source(
        &mut self,
        _mount_id: &MountId,
        _source_connection_id: &SourceConnectionId,
    ) -> StoreResult<()> {
        Err(StoreError::InvalidState(
            "generation repository does not support source-explicit reset".to_string(),
        ))
    }

    /// Reserves one immutable apply. Exact replay returns the existing journal;
    /// a changed payload or a concurrent source apply fails closed.
    fn reserve_generation_apply(
        &mut self,
        prepared: PreparedGenerationApply,
    ) -> StoreResult<GenerationApplyJournalRecord>;

    /// Additive V2 reservation. Legacy repositories safely support only
    /// reservations that do not require a durable terminal acknowledgment.
    fn reserve_generation_apply_v2(
        &mut self,
        prepared: PreparedGenerationApplyV2,
    ) -> StoreResult<GenerationApplyJournalRecord> {
        if prepared.acknowledgment_required {
            return Err(StoreError::InvalidState(
                "generation repository does not support durable terminal acknowledgments"
                    .to_string(),
            ));
        }
        self.reserve_generation_apply(prepared.apply)
    }

    /// Additive V3 reservation. Legacy repositories may safely accept only the
    /// legacy selection, which requires no negotiated durable behavior.
    fn reserve_generation_apply_v3(
        &mut self,
        prepared: PreparedGenerationApplyV3,
    ) -> StoreResult<NegotiatedGenerationApplyJournalRecord> {
        if prepared.selected_capabilities != GenerationTransportCapabilities::legacy() {
            return Err(StoreError::InvalidState(
                "generation repository does not support durable transport selection".to_string(),
            ));
        }
        self.reserve_generation_apply(prepared.apply).map(|apply| {
            NegotiatedGenerationApplyJournalRecord {
                apply,
                selection_binding: GenerationTransportSelectionBinding::Bound(
                    GenerationTransportCapabilities::legacy(),
                ),
            }
        })
    }

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

    fn get_generation_apply_v2(
        &self,
        delta_id: &str,
    ) -> StoreResult<Option<NegotiatedGenerationApplyJournalRecord>> {
        self.get_generation_apply(delta_id).map(|journal| {
            journal.map(|apply| NegotiatedGenerationApplyJournalRecord {
                apply,
                selection_binding: GenerationTransportSelectionBinding::Bound(
                    GenerationTransportCapabilities::legacy(),
                ),
            })
        })
    }

    fn list_active_generation_applies(&self) -> StoreResult<Vec<GenerationApplyJournalRecord>>;

    /// Lists the at-most-one active filesystem transaction for a mount. The
    /// default filters the released global view for source compatibility.
    fn list_active_generation_applies_for_mount(
        &self,
        mount_id: &MountId,
    ) -> StoreResult<Vec<GenerationApplyJournalRecord>> {
        self.list_active_generation_applies().map(|journals| {
            journals
                .into_iter()
                .filter(|journal| journal.delta.mount_id.as_str() == mount_id.as_str())
                .collect()
        })
    }

    /// Lists active and completed journals so the staging owner can reconcile
    /// retained conflict evidence and discard non-live payloads.
    fn list_generation_applies(&self) -> StoreResult<Vec<GenerationApplyJournalRecord>>;

    /// Completed acknowledgments are replayed before polling for another delta.
    fn list_pending_generation_acknowledgments(
        &self,
        _mount_id: &MountId,
    ) -> StoreResult<Vec<GenerationApplyJournalRecord>> {
        Ok(Vec::new())
    }

    /// Lists pending receipts for one exact source row. Legacy repositories
    /// filter their mount-wide result without inventing source ownership.
    fn list_pending_generation_acknowledgments_for_source(
        &self,
        mount_id: &MountId,
        source_connection_id: &SourceConnectionId,
    ) -> StoreResult<Vec<GenerationApplyJournalRecord>> {
        self.list_pending_generation_acknowledgments(mount_id)
            .map(|journals| {
                journals
                    .into_iter()
                    .filter(|journal| &journal.delta.source_connection_id == source_connection_id)
                    .collect()
            })
    }

    fn mark_generation_acknowledged(
        &mut self,
        _delta_id: &str,
        _receipt_sha256: &str,
        _acknowledged_at: &str,
    ) -> StoreResult<GenerationApplyJournalRecord> {
        Err(StoreError::InvalidState(
            "generation repository does not support durable terminal acknowledgments".to_string(),
        ))
    }

    fn record_generation_inode_evidence(
        &mut self,
        evidence: GenerationInodeEvidenceRecord,
    ) -> StoreResult<()>;

    fn list_generation_inode_evidence(&self) -> StoreResult<Vec<GenerationInodeEvidenceRecord>>;

    /// Converts a completed apply to conflict and atomically advances both
    /// retained-inode fences, including their captured reservation lengths.
    fn mark_generation_inode_evidence_conflict(
        &mut self,
        delta_id: &str,
        entry_index: u64,
        update: GenerationInodeEvidenceConflictUpdate,
    ) -> StoreResult<()>;

    /// Atomically clears the late-write conflict after the visible file was
    /// durably replaced by one exact retained version. Both retained inodes
    /// and their evidence remain as a captured-reservation tombstoned GC
    /// journal. Later foreign-descriptor growth is deliberately not measured.
    fn mark_generation_inode_evidence_resolved(
        &mut self,
        delta_id: &str,
        entry_index: u64,
        resolution: GenerationInodeEvidenceResolution,
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
