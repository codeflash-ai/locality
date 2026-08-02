use locality_core::model::MountId;
use locality_core::portable::{SourceConnectionId, SourceGenerationId};
use locality_protocol::freshness_delivery::{
    GENERATION_DELTA_RECEIPT_V1_GOLDEN_JSON, GENERATION_DELTA_V1_GOLDEN_JSON, GenerationDelta,
    GenerationDeltaTerminalReceipt,
};
use locality_protocol::freshness_delivery_transport::GenerationTransportCapabilities;
use locality_protocol::generation_baseline::GenerationBaselineRefreshModeV1;
use locality_store::{
    GenerationApplyJournalRecord, GenerationApplyOutcome, GenerationApplyStatus,
    GenerationBaselineSeedRecord, GenerationBaselineSeedRecordV2, GenerationDeliveryRepository,
    GenerationInodeEvidenceConflictUpdate, GenerationInodeEvidenceRecord,
    GenerationInodeEvidenceResolution, GenerationPathRecord, ObservedGenerationRecord,
    PreparedGenerationApply, PreparedGenerationApplyV2, PreparedGenerationApplyV3, StoreError,
    StoreResult,
};

struct LegacyGenerationRepository;

impl GenerationDeliveryRepository for LegacyGenerationRepository {
    fn seed_observed_generation(
        &mut self,
        _observed: ObservedGenerationRecord,
        _paths: Vec<GenerationPathRecord>,
    ) -> StoreResult<()> {
        unimplemented!()
    }

    fn get_observed_generation(
        &self,
        _mount_id: &MountId,
    ) -> StoreResult<Option<ObservedGenerationRecord>> {
        unimplemented!()
    }

    fn list_generation_paths(&self, _mount_id: &MountId) -> StoreResult<Vec<GenerationPathRecord>> {
        unimplemented!()
    }

    fn reserve_generation_apply(
        &mut self,
        _prepared: PreparedGenerationApply,
    ) -> StoreResult<GenerationApplyJournalRecord> {
        unimplemented!()
    }

    fn mark_generation_apply_started(
        &mut self,
        _delta_id: &str,
        _updated_at: &str,
    ) -> StoreResult<GenerationApplyJournalRecord> {
        unimplemented!()
    }

    fn record_generation_apply_outcome(
        &mut self,
        _delta_id: &str,
        _entry_index: u64,
        _outcome: GenerationApplyOutcome,
        _updated_at: &str,
    ) -> StoreResult<GenerationApplyJournalRecord> {
        unimplemented!()
    }

    fn get_generation_apply(
        &self,
        _delta_id: &str,
    ) -> StoreResult<Option<GenerationApplyJournalRecord>> {
        unimplemented!()
    }

    fn list_active_generation_applies(&self) -> StoreResult<Vec<GenerationApplyJournalRecord>> {
        unimplemented!()
    }

    fn list_generation_applies(&self) -> StoreResult<Vec<GenerationApplyJournalRecord>> {
        unimplemented!()
    }

    fn record_generation_inode_evidence(
        &mut self,
        _evidence: GenerationInodeEvidenceRecord,
    ) -> StoreResult<()> {
        unimplemented!()
    }

    fn list_generation_inode_evidence(&self) -> StoreResult<Vec<GenerationInodeEvidenceRecord>> {
        unimplemented!()
    }

    fn mark_generation_inode_evidence_conflict(
        &mut self,
        _delta_id: &str,
        _entry_index: u64,
        _update: GenerationInodeEvidenceConflictUpdate,
    ) -> StoreResult<()> {
        unimplemented!()
    }

    fn mark_generation_inode_evidence_resolved(
        &mut self,
        _delta_id: &str,
        _entry_index: u64,
        _resolution: GenerationInodeEvidenceResolution,
    ) -> StoreResult<()> {
        unimplemented!()
    }

    fn remove_generation_inode_evidence(
        &mut self,
        _delta_id: &str,
        _entry_index: u64,
    ) -> StoreResult<()> {
        unimplemented!()
    }

    fn complete_generation_apply(
        &mut self,
        _delta_id: &str,
        _completed_at: &str,
    ) -> StoreResult<GenerationApplyJournalRecord> {
        unimplemented!()
    }
}

#[allow(dead_code)]
fn original_public_struct_literals_compile(
    delta: GenerationDelta,
    receipt: GenerationDeltaTerminalReceipt,
) {
    let _prepared = PreparedGenerationApply {
        delta: delta.clone(),
        receipt: receipt.clone(),
        receipt_sha256: String::new(),
        stage_root: String::new(),
        created_at: String::new(),
    };
    let _journal = GenerationApplyJournalRecord {
        delta,
        receipt,
        receipt_sha256: String::new(),
        stage_root: String::new(),
        status: GenerationApplyStatus::Staged,
        outcomes: Vec::new(),
        created_at: String::new(),
        updated_at: String::new(),
        completed_at: None,
    };
}

#[test]
fn original_repository_implementation_uses_safe_additive_defaults() {
    fn assert_repository<T: GenerationDeliveryRepository>() {}
    assert_repository::<LegacyGenerationRepository>();

    let mut repository = LegacyGenerationRepository;
    assert!(
        repository
            .list_pending_generation_acknowledgments(&MountId::new("legacy-mount"))
            .unwrap()
            .is_empty()
    );
    assert!(
        repository
            .list_pending_generation_acknowledgments_for_source(
                &MountId::new("legacy-mount"),
                &SourceConnectionId::new("legacy-source"),
            )
            .unwrap()
            .is_empty()
    );
    let error = repository
        .seed_observed_generations(vec![
            GenerationBaselineSeedRecord::new(
                ObservedGenerationRecord {
                    mount_id: MountId::new("legacy-mount"),
                    source_connection_id: SourceConnectionId::new("source-a"),
                    generation_id: SourceGenerationId::new("generation-a").unwrap(),
                    inventory_sha256:
                        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                            .to_string(),
                    workspace_layout_version: 1,
                    workspace_layout_digest:
                        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                            .to_string(),
                    last_receipt_sha256: None,
                    updated_at: "2026-08-02T00:00:00Z".to_string(),
                },
                Vec::new(),
            ),
            GenerationBaselineSeedRecord::new(
                ObservedGenerationRecord {
                    mount_id: MountId::new("legacy-mount"),
                    source_connection_id: SourceConnectionId::new("source-b"),
                    generation_id: SourceGenerationId::new("generation-b").unwrap(),
                    inventory_sha256:
                        "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                            .to_string(),
                    workspace_layout_version: 1,
                    workspace_layout_digest:
                        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                            .to_string(),
                    last_receipt_sha256: None,
                    updated_at: "2026-08-02T00:00:00Z".to_string(),
                },
                Vec::new(),
            ),
        ])
        .expect_err("legacy repository must reject a non-atomic multi-source seed");
    assert!(matches!(error, StoreError::InvalidState(_)));
    let error = repository
        .seed_observed_generations_v2(vec![GenerationBaselineSeedRecordV2::new(
            GenerationBaselineSeedRecord::new(
                ObservedGenerationRecord {
                    mount_id: MountId::new("legacy-mount"),
                    source_connection_id: SourceConnectionId::new("source-full"),
                    generation_id: SourceGenerationId::new("generation-full").unwrap(),
                    inventory_sha256:
                        "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
                            .to_string(),
                    workspace_layout_version: 1,
                    workspace_layout_digest:
                        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                            .to_string(),
                    last_receipt_sha256: None,
                    updated_at: "2026-08-02T00:00:00Z".to_string(),
                },
                Vec::new(),
            ),
            GenerationBaselineRefreshModeV1::FullExportOnly,
        )])
        .expect_err("legacy repository must not silently store an unsupported refresh route");
    assert!(matches!(error, StoreError::InvalidState(_)));

    let delta: GenerationDelta = serde_json::from_slice(GENERATION_DELTA_V1_GOLDEN_JSON).unwrap();
    let receipt: GenerationDeltaTerminalReceipt =
        serde_json::from_slice(GENERATION_DELTA_RECEIPT_V1_GOLDEN_JSON).unwrap();
    let prepared = PreparedGenerationApply {
        delta,
        receipt,
        receipt_sha256: String::new(),
        stage_root: String::new(),
        created_at: String::new(),
    };
    let error = repository
        .reserve_generation_apply_v2(PreparedGenerationApplyV2::new(prepared.clone(), true))
        .expect_err("legacy repository must not silently lose a required acknowledgment");
    assert!(matches!(error, StoreError::InvalidState(_)));

    let error = repository
        .reserve_generation_apply_v3(PreparedGenerationApplyV3::new(
            prepared,
            GenerationTransportCapabilities {
                terminal_receipt_acknowledgments: true,
                ..GenerationTransportCapabilities::legacy()
            },
        ))
        .expect_err("legacy repository must not silently lose a negotiated selection");
    assert!(matches!(error, StoreError::InvalidState(_)));
}
