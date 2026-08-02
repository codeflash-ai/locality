use localityd::generation_sync::GenerationSyncError;

// This integration test is compiled as a downstream crate. Keep the match
// exhaustive so additions to the legacy public enum fail at compile time.
fn classify_legacy_error(error: GenerationSyncError) -> &'static str {
    match error {
        GenerationSyncError::Store(_) => "store",
        GenerationSyncError::Io(_) => "io",
        GenerationSyncError::MountAccess(_) => "mount_access",
        GenerationSyncError::StateCoordinator(_) => "state_coordinator",
        GenerationSyncError::MountBusy => "mount_busy",
        GenerationSyncError::MountCoordinatorPoisoned => "mount_coordinator_poisoned",
        GenerationSyncError::Transport(_) => "transport",
        GenerationSyncError::Contract(_) => "contract",
        GenerationSyncError::MissingObservedGeneration(_) => "missing_observed_generation",
        GenerationSyncError::UnexpectedMount => "unexpected_mount",
        GenerationSyncError::InvalidStagePath => "invalid_stage_path",
        GenerationSyncError::ContentMismatch(_) => "content_mismatch",
        GenerationSyncError::LocalBaseMismatch => "local_base_mismatch",
        GenerationSyncError::JournalMismatch => "journal_mismatch",
        GenerationSyncError::ConcurrentMutation => "concurrent_mutation",
        GenerationSyncError::MissingConflictEvidence(_) => "missing_conflict_evidence",
        GenerationSyncError::MissingMergeBaseEvidence(_) => "missing_merge_base_evidence",
        GenerationSyncError::MissingInodeEvidence(_) => "missing_inode_evidence",
        GenerationSyncError::ConflictRetentionQuotaExceeded => "conflict_retention_quota",
        GenerationSyncError::CapturedEvidenceReservationExceeded => "evidence_reservation",
        GenerationSyncError::BaseRetentionQuotaExceeded => "base_retention_quota",
        GenerationSyncError::InjectedInterruption => "injected_interruption",
    }
}

#[test]
fn legacy_generation_sync_error_remains_exhaustively_matchable() {
    assert_eq!(
        classify_legacy_error(GenerationSyncError::Contract("fixture".to_string())),
        "contract"
    );
}
