use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use locality_core::model::{MountId, RemoteId};
use locality_core::portable::{
    ContentVersionId, LogicalPath, ProjectionId, SourceConnectionId, SourceGenerationId,
};
use locality_core::workspace_layout::PortableMountId;
use locality_protocol::FreshnessEpoch;
use locality_protocol::freshness_delivery::{
    FRESHNESS_DELIVERY_READER_VERSION, GENERATION_DELTA_FORMAT_VERSION, GenerationDelta,
    GenerationDeltaEntry, GenerationDeltaTerminalReceipt, GenerationFileIdentity,
};
use locality_protocol::freshness_delivery_transport::{
    GenerationBodyWindowCapability, GenerationPinFallbackPolicy, GenerationPinLeaseCapability,
    GenerationTransportCapabilities,
};
use locality_protocol::workspace_layout::LayoutDigest;
use locality_store::{
    ConnectionId, GenerationApplyOutcome, GenerationApplyStatus, GenerationDeliveryRepository,
    GenerationPathRecord, GenerationPathState, GenerationTransportSelectionBinding, MountConfig,
    MountRepository, ObservedGenerationRecord, PreparedGenerationApply, PreparedGenerationApplyV2,
    PreparedGenerationApplyV3, SqliteStateStore, StoreError,
};

fn digest(character: char) -> String {
    format!("sha256:{}", character.to_string().repeat(64))
}

fn generation(value: &str) -> SourceGenerationId {
    SourceGenerationId::new(value).unwrap()
}

fn identity(version: &str, digest_character: char, bytes: u64) -> GenerationFileIdentity {
    GenerationFileIdentity {
        projection_id: ProjectionId::new("projection-roadmap"),
        logical_path: LogicalPath::new("Roadmap.md").unwrap(),
        content_version_id: ContentVersionId::new(version),
        content_sha256: digest(digest_character),
        byte_length: bytes,
    }
}

fn delta() -> GenerationDelta {
    GenerationDelta {
        format_version: GENERATION_DELTA_FORMAT_VERSION,
        minimum_reader_version: FRESHNESS_DELIVERY_READER_VERSION,
        delta_id: "delta-2".to_string(),
        mount_id: PortableMountId::new("mount-main").unwrap(),
        source_connection_id: SourceConnectionId::new("source-main"),
        base_generation_id: generation("generation-1"),
        target_generation_id: generation("generation-2"),
        target_complete: true,
        target_inventory_sha256: digest('2'),
        workspace_layout_version: 1,
        workspace_layout_digest: LayoutDigest::new(digest('a')).unwrap(),
        entries: vec![GenerationDeltaEntry {
            old: Some(identity("content-1", '1', 3)),
            new: Some(identity("content-2", '2', 4)),
        }],
    }
}

fn receipt(delta: &GenerationDelta) -> GenerationDeltaTerminalReceipt {
    GenerationDeltaTerminalReceipt {
        format_version: delta.format_version,
        minimum_reader_version: delta.minimum_reader_version,
        delta_id: delta.delta_id.clone(),
        mount_id: delta.mount_id.clone(),
        source_connection_id: delta.source_connection_id.clone(),
        base_generation_id: delta.base_generation_id.clone(),
        target_generation_id: delta.target_generation_id.clone(),
        target_inventory_sha256: delta.target_inventory_sha256.clone(),
        workspace_layout_version: delta.workspace_layout_version,
        workspace_layout_digest: delta.workspace_layout_digest.clone(),
        delta_sha256: delta.canonical_sha256().unwrap(),
        entry_count: delta.entries.len() as u64,
        changed_content_bytes: delta.changed_content_bytes().unwrap(),
        authorization_epoch: FreshnessEpoch::new(7).unwrap(),
        completed_at: "2026-07-31T12:00:00Z".to_string(),
    }
}

fn seed(store: &mut SqliteStateStore, fixture: &Fixture) {
    store
        .save_mount(MountConfig::new(
            fixture.mount_id.clone(),
            "backend",
            &fixture.mount_root,
        ))
        .unwrap();
    store
        .seed_observed_generation(
            ObservedGenerationRecord {
                mount_id: fixture.mount_id.clone(),
                source_connection_id: SourceConnectionId::new("source-main"),
                generation_id: generation("generation-1"),
                inventory_sha256: digest('1'),
                workspace_layout_version: 1,
                workspace_layout_digest: digest('a'),
                last_receipt_sha256: None,
                updated_at: "2026-07-31T11:00:00Z".to_string(),
            },
            vec![GenerationPathRecord {
                mount_id: fixture.mount_id.clone(),
                projection_id: ProjectionId::new("projection-roadmap"),
                logical_path: "Roadmap.md".to_string(),
                local_logical_path: "Roadmap.md".to_string(),
                base_generation_id: generation("generation-1"),
                base_identity: Some(identity("content-1", '1', 3)),
                base_payload_delta_id: None,
                base_payload_entry_index: None,
                conflict_payload_delta_id: None,
                conflict_payload_entry_index: None,
                state: GenerationPathState::Clean,
                incoming_identity: None,
                updated_at: "2026-07-31T11:00:00Z".to_string(),
            }],
        )
        .unwrap();
}

#[test]
fn observed_generation_apply_is_persisted_exact_replayable_and_atomic() {
    let fixture = Fixture::new("persist-replay");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    seed(&mut store, &fixture);
    let delta = delta();
    let receipt = receipt(&delta);
    let prepared = PreparedGenerationApply {
        delta: delta.clone(),
        receipt: receipt.clone(),
        receipt_sha256: receipt.canonical_sha256().unwrap(),
        stage_root: "generation-delivery/delta-2".to_string(),
        created_at: "2026-07-31T12:01:00Z".to_string(),
    };

    let reserved = store.reserve_generation_apply(prepared.clone()).unwrap();
    assert_eq!(reserved.status, GenerationApplyStatus::Staged);
    assert_eq!(store.reserve_generation_apply(prepared).unwrap(), reserved);
    store
        .record_generation_apply_outcome(
            "delta-2",
            0,
            GenerationApplyOutcome::Applied,
            "2026-07-31T12:02:00Z",
        )
        .unwrap();
    let completed = store
        .complete_generation_apply("delta-2", "2026-07-31T12:03:00Z")
        .unwrap();
    assert_eq!(completed.status, GenerationApplyStatus::Completed);
    assert_eq!(
        store
            .complete_generation_apply("delta-2", "2026-07-31T12:04:00Z")
            .unwrap(),
        completed
    );
    drop(store);

    let reopened = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    let observed = reopened
        .get_observed_generation(&fixture.mount_id)
        .unwrap()
        .unwrap();
    assert_eq!(observed.generation_id, generation("generation-2"));
    assert_eq!(
        observed.last_receipt_sha256,
        Some(receipt.canonical_sha256().unwrap())
    );
    let paths = reopened.list_generation_paths(&fixture.mount_id).unwrap();
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].state, GenerationPathState::Clean);
    assert_eq!(paths[0].base_identity, delta.entries[0].new);
    assert!(
        reopened
            .list_active_generation_applies()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn completed_required_acknowledgment_is_durable_exact_and_idempotent() {
    let fixture = Fixture::new("durable-acknowledgment");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    seed(&mut store, &fixture);
    let delta = delta();
    let receipt = receipt(&delta);
    let receipt_sha256 = receipt.canonical_sha256().unwrap();
    store
        .reserve_generation_apply_v2(PreparedGenerationApplyV2::new(
            PreparedGenerationApply {
                delta: delta.clone(),
                receipt,
                receipt_sha256: receipt_sha256.clone(),
                stage_root: "generation-delivery/durable-ack".to_string(),
                created_at: "2026-07-31T12:01:00Z".to_string(),
            },
            true,
        ))
        .unwrap();
    store
        .record_generation_apply_outcome(
            &delta.delta_id,
            0,
            GenerationApplyOutcome::Applied,
            "2026-07-31T12:02:00Z",
        )
        .unwrap();
    store
        .complete_generation_apply(&delta.delta_id, "2026-07-31T12:03:00Z")
        .unwrap();
    drop(store);

    let mut reopened = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    let pending = reopened
        .list_pending_generation_acknowledgments(&fixture.mount_id)
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].receipt_sha256, receipt_sha256);
    let reset_error = reopened
        .clear_mount_source_state(&fixture.mount_id)
        .expect_err("pending terminal acknowledgment must fence source reset");
    assert!(matches!(reset_error, StoreError::InvalidState(_)));
    assert!(
        reopened
            .mark_generation_acknowledged(&delta.delta_id, &digest('f'), "2026-07-31T12:04:00Z",)
            .is_err()
    );
    let acknowledged = reopened
        .mark_generation_acknowledged(&delta.delta_id, &receipt_sha256, "2026-07-31T12:04:00Z")
        .unwrap();
    assert_eq!(
        reopened
            .mark_generation_acknowledged(&delta.delta_id, &receipt_sha256, "2026-07-31T12:05:00Z",)
            .unwrap(),
        acknowledged
    );
    assert!(
        reopened
            .list_pending_generation_acknowledgments(&fixture.mount_id)
            .unwrap()
            .is_empty()
    );
    reopened
        .clear_mount_source_state(&fixture.mount_id)
        .expect("acknowledged clean lineage may be retired");
}

#[test]
fn complete_negotiated_transport_selection_is_immutable_and_survives_reopen() {
    let fixture = Fixture::new("durable-complete-transport-selection");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    seed(&mut store, &fixture);
    let delta = delta();
    let receipt = receipt(&delta);
    let prepared = PreparedGenerationApply {
        delta: delta.clone(),
        receipt: receipt.clone(),
        receipt_sha256: receipt.canonical_sha256().unwrap(),
        stage_root: "generation-delivery/selection".to_string(),
        created_at: "2026-07-31T12:01:00Z".to_string(),
    };
    let selected = GenerationTransportCapabilities {
        body_windows: Some(GenerationBodyWindowCapability {
            max_window_bytes: 256 * 1024,
        }),
        terminal_receipt_acknowledgments: true,
        generation_pin_leases: Some(GenerationPinLeaseCapability {
            min_lease_seconds: 60,
            max_lease_seconds: 900,
            max_active_leases_per_device: 8,
            fallback_policies: vec![
                GenerationPinFallbackPolicy::RequireExact,
                GenerationPinFallbackPolicy::UseLatestRetained,
            ],
        }),
        ..GenerationTransportCapabilities::legacy()
    };
    store
        .reserve_generation_apply_v3(PreparedGenerationApplyV3::new(
            prepared.clone(),
            selected.clone(),
        ))
        .unwrap();
    drop(store);

    let mut reopened = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    let stored = reopened
        .get_generation_apply_v2(&delta.delta_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.selection_binding,
        GenerationTransportSelectionBinding::Bound(selected.clone())
    );

    let mut changed = selected;
    changed.body_windows.as_mut().unwrap().max_window_bytes /= 2;
    assert!(
        reopened
            .reserve_generation_apply_v3(PreparedGenerationApplyV3::new(prepared, changed))
            .is_err()
    );
}

#[test]
fn schema_24_component_v3_migrates_existing_journals_without_pending_acknowledgments() {
    use rusqlite::Connection;

    let fixture = Fixture::new("migration-v24-component-v3");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    seed(&mut store, &fixture);
    let delta = delta();
    let receipt = receipt(&delta);
    store
        .reserve_generation_apply(PreparedGenerationApply {
            delta: delta.clone(),
            receipt: receipt.clone(),
            receipt_sha256: receipt.canonical_sha256().unwrap(),
            stage_root: "generation-delivery/prior-v3".to_string(),
            created_at: "2026-07-31T12:01:00Z".to_string(),
        })
        .unwrap();
    store
        .record_generation_apply_outcome(
            &delta.delta_id,
            0,
            GenerationApplyOutcome::Applied,
            "2026-07-31T12:02:00Z",
        )
        .unwrap();
    store
        .complete_generation_apply(&delta.delta_id, "2026-07-31T12:03:00Z")
        .unwrap();
    let db_path = store.db_path.clone();
    drop(store);

    let connection = Connection::open(&db_path).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE generation_apply_journals DROP COLUMN acknowledged_at;
             ALTER TABLE generation_apply_journals DROP COLUMN acknowledgment_required;
             ALTER TABLE generation_apply_journals DROP COLUMN selected_capabilities_json;
             ALTER TABLE generation_apply_journals DROP COLUMN selection_binding;
             UPDATE state_components
             SET version = 3, min_reader_version = 3
             WHERE component_id = 'durable:generation_delivery';
             UPDATE state_components SET version = 24
             WHERE component_id = 'core:schema';
             PRAGMA user_version = 24;",
        )
        .unwrap();
    drop(connection);

    let reopened = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    let journal = reopened
        .get_generation_apply(&delta.delta_id)
        .unwrap()
        .unwrap();
    assert_eq!(journal.delta.delta_id, delta.delta_id);
    assert_eq!(
        reopened
            .get_generation_apply_v2(&delta.delta_id)
            .unwrap()
            .unwrap()
            .selection_binding,
        GenerationTransportSelectionBinding::Bound(GenerationTransportCapabilities::legacy())
    );
    assert!(
        reopened
            .list_pending_generation_acknowledgments(&fixture.mount_id)
            .unwrap()
            .is_empty()
    );
    let connection = Connection::open(&db_path).unwrap();
    let component: (i64, i64) = connection
        .query_row(
            "SELECT version, min_reader_version FROM state_components
             WHERE component_id = 'durable:generation_delivery'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let user_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(component, (6, 6));
    assert_eq!(user_version, 26);
}

#[test]
fn schema_25_completed_acknowledgment_migrates_without_inventing_selection() {
    use rusqlite::Connection;

    let fixture = Fixture::new("migration-v25-component-v4-pending-ack");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    seed(&mut store, &fixture);
    let delta = delta();
    let receipt = receipt(&delta);
    store
        .reserve_generation_apply_v2(PreparedGenerationApplyV2::new(
            PreparedGenerationApply {
                delta: delta.clone(),
                receipt: receipt.clone(),
                receipt_sha256: receipt.canonical_sha256().unwrap(),
                stage_root: "generation-delivery/prior-v4-ack".to_string(),
                created_at: "2026-07-31T12:01:00Z".to_string(),
            },
            true,
        ))
        .unwrap();
    store
        .record_generation_apply_outcome(
            &delta.delta_id,
            0,
            GenerationApplyOutcome::Applied,
            "2026-07-31T12:02:00Z",
        )
        .unwrap();
    store
        .complete_generation_apply(&delta.delta_id, "2026-07-31T12:03:00Z")
        .unwrap();
    let db_path = store.db_path.clone();
    drop(store);

    let connection = Connection::open(&db_path).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE generation_apply_journals DROP COLUMN selected_capabilities_json;
             ALTER TABLE generation_apply_journals DROP COLUMN selection_binding;
             UPDATE state_components
             SET version = 4, min_reader_version = 4
             WHERE component_id = 'durable:generation_delivery';
             UPDATE state_components SET version = 25
             WHERE component_id = 'core:schema';
             PRAGMA user_version = 25;",
        )
        .unwrap();
    drop(connection);

    let mut reopened = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    let selection_binding = reopened
        .get_generation_apply_v2(&delta.delta_id)
        .unwrap()
        .unwrap()
        .selection_binding;
    assert_eq!(
        selection_binding,
        GenerationTransportSelectionBinding::PreBindingCompleted {
            terminal_receipt_acknowledgments: true,
        }
    );
    assert_eq!(
        reopened
            .list_pending_generation_acknowledgments(&fixture.mount_id)
            .unwrap()
            .len(),
        1
    );

    let replay = reopened
        .reserve_generation_apply_v3(PreparedGenerationApplyV3::new(
            PreparedGenerationApply {
                delta: delta.clone(),
                receipt: receipt.clone(),
                receipt_sha256: receipt.canonical_sha256().unwrap(),
                stage_root: "generation-delivery/prior-v4-ack".to_string(),
                created_at: "2026-07-31T12:01:00Z".to_string(),
            },
            GenerationTransportCapabilities {
                body_windows: Some(GenerationBodyWindowCapability {
                    max_window_bytes: 64 * 1024,
                }),
                ..GenerationTransportCapabilities::legacy()
            },
        ))
        .expect("completed pre-binding replay must not renegotiate or mismatch");
    assert_eq!(replay.selection_binding, selection_binding);
}

#[test]
fn schema_25_active_nonlegacy_apply_fails_atomically_and_retry_preserves_v25() {
    use rusqlite::Connection;

    let fixture = Fixture::new("migration-v25-active-nonlegacy");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    seed(&mut store, &fixture);
    let delta = delta();
    let receipt = receipt(&delta);
    let selected = GenerationTransportCapabilities {
        body_windows: Some(GenerationBodyWindowCapability {
            max_window_bytes: 64 * 1024,
        }),
        terminal_receipt_acknowledgments: true,
        generation_pin_leases: Some(GenerationPinLeaseCapability {
            min_lease_seconds: 60,
            max_lease_seconds: 600,
            max_active_leases_per_device: 4,
            fallback_policies: vec![GenerationPinFallbackPolicy::RequireExact],
        }),
        ..GenerationTransportCapabilities::legacy()
    };
    store
        .reserve_generation_apply_v3(PreparedGenerationApplyV3::new(
            PreparedGenerationApply {
                delta: delta.clone(),
                receipt: receipt.clone(),
                receipt_sha256: receipt.canonical_sha256().unwrap(),
                stage_root: "generation-delivery/v25-active".to_string(),
                created_at: "2026-07-31T12:01:00Z".to_string(),
            },
            selected,
        ))
        .unwrap();
    store
        .mark_generation_apply_started(&delta.delta_id, "2026-07-31T12:01:30Z")
        .unwrap();
    let db_path = store.db_path.clone();
    drop(store);

    let connection = Connection::open(&db_path).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE generation_apply_journals DROP COLUMN selected_capabilities_json;
             ALTER TABLE generation_apply_journals DROP COLUMN selection_binding;
             UPDATE state_components
             SET version = 4, min_reader_version = 4
             WHERE component_id = 'durable:generation_delivery';
             UPDATE state_components SET version = 25
             WHERE component_id = 'core:schema';
             PRAGMA user_version = 25;",
        )
        .unwrap();
    drop(connection);

    for _ in 0..2 {
        let error = SqliteStateStore::open(fixture.state_root.clone())
            .expect_err("ambiguous active v25 selection must block migration");
        assert!(
            error
                .to_string()
                .contains("has no complete immutable transport selection")
        );
        let connection = Connection::open(&db_path).unwrap();
        let user_version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        let component: (i64, i64) = connection
            .query_row(
                "SELECT version, min_reader_version FROM state_components
                 WHERE component_id = 'durable:generation_delivery'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let selection_columns: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('generation_apply_journals')
                 WHERE name IN ('selected_capabilities_json', 'selection_binding')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(user_version, 25);
        assert_eq!(component, (4, 4));
        assert_eq!(selection_columns, 0, "failed migration must roll back");
    }

    let connection = Connection::open(&db_path).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE generation_apply_journals
             ADD COLUMN selected_capabilities_json TEXT NOT NULL DEFAULT '{}';
             UPDATE generation_apply_journals
             SET selected_capabilities_json =
                 '{\"format_version\":1,\"minimum_reader_version\":1,\"terminal_receipt_acknowledgments\":true}';
             UPDATE state_components
             SET version = 5, min_reader_version = 5
             WHERE component_id = 'durable:generation_delivery';
             UPDATE state_components SET version = 26
             WHERE component_id = 'core:schema';
             PRAGMA user_version = 26;",
        )
        .unwrap();
    drop(connection);

    let error = SqliteStateStore::open(fixture.state_root.clone())
        .expect_err("prerelease v26 ack-only backfill remains ambiguous");
    assert!(
        error
            .to_string()
            .contains("has no complete immutable transport selection")
    );
    let connection = Connection::open(&db_path).unwrap();
    let component: (i64, i64) = connection
        .query_row(
            "SELECT version, min_reader_version FROM state_components
             WHERE component_id = 'durable:generation_delivery'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let binding_columns: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('generation_apply_journals')
             WHERE name = 'selection_binding'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(component, (5, 5));
    assert_eq!(binding_columns, 0, "component-v6 migration must roll back");
}

#[test]
fn prerelease_v26_active_nonlegacy_selection_is_faithfully_recovered() {
    use rusqlite::Connection;

    let fixture = Fixture::new("migration-prerelease-v26-active-nonlegacy");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    seed(&mut store, &fixture);
    let delta = delta();
    let receipt = receipt(&delta);
    let prepared = PreparedGenerationApply {
        delta: delta.clone(),
        receipt: receipt.clone(),
        receipt_sha256: receipt.canonical_sha256().unwrap(),
        stage_root: "generation-delivery/v26-active".to_string(),
        created_at: "2026-07-31T12:01:00Z".to_string(),
    };
    let selected = GenerationTransportCapabilities {
        body_windows: Some(GenerationBodyWindowCapability {
            max_window_bytes: 64 * 1024,
        }),
        generation_pin_leases: Some(GenerationPinLeaseCapability {
            min_lease_seconds: 60,
            max_lease_seconds: 600,
            max_active_leases_per_device: 4,
            fallback_policies: vec![GenerationPinFallbackPolicy::RequireExact],
        }),
        ..GenerationTransportCapabilities::legacy()
    };
    store
        .reserve_generation_apply_v3(PreparedGenerationApplyV3::new(
            prepared.clone(),
            selected.clone(),
        ))
        .unwrap();
    let db_path = store.db_path.clone();
    drop(store);

    let connection = Connection::open(&db_path).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE generation_apply_journals DROP COLUMN selection_binding;
             UPDATE state_components
             SET version = 5, min_reader_version = 5
             WHERE component_id = 'durable:generation_delivery';",
        )
        .unwrap();
    drop(connection);

    let mut reopened = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    let stored = reopened
        .get_generation_apply_v2(&delta.delta_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.selection_binding,
        GenerationTransportSelectionBinding::Bound(selected.clone())
    );
    assert_eq!(
        reopened
            .reserve_generation_apply_v3(PreparedGenerationApplyV3::new(prepared, selected))
            .unwrap(),
        stored
    );
}

#[test]
fn reservation_fails_closed_on_generation_layout_and_old_identity_mismatch() {
    let fixture = Fixture::new("mismatch");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    seed(&mut store, &fixture);

    for changed in ["generation", "layout", "identity"] {
        let mut delta = delta();
        match changed {
            "generation" => delta.base_generation_id = generation("generation-0"),
            "layout" => {
                delta.workspace_layout_digest = LayoutDigest::new(digest('f')).unwrap();
            }
            "identity" => {
                delta.entries[0].old.as_mut().unwrap().content_version_id =
                    ContentVersionId::new("different");
            }
            _ => unreachable!(),
        }
        delta.delta_id = format!("delta-{changed}");
        let receipt = receipt(&delta);
        let error = store
            .reserve_generation_apply(PreparedGenerationApply {
                receipt_sha256: receipt.canonical_sha256().unwrap(),
                receipt,
                delta,
                stage_root: format!("generation-delivery/{changed}"),
                created_at: "2026-07-31T12:01:00Z".to_string(),
            })
            .expect_err("mismatch must fail closed");
        assert!(matches!(error, StoreError::InvalidState(_)));
    }
    assert!(store.list_active_generation_applies().unwrap().is_empty());
}

#[test]
fn empty_mount_delta_completes_and_advances_observed_generation() {
    let fixture = Fixture::new("empty-advance");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    seed(&mut store, &fixture);
    let mut delta = delta();
    delta.delta_id = "delta-empty".to_string();
    delta.entries.clear();
    let receipt = receipt(&delta);
    store
        .reserve_generation_apply(PreparedGenerationApply {
            receipt_sha256: receipt.canonical_sha256().unwrap(),
            receipt,
            delta,
            stage_root: "generation-delivery/delta-empty".to_string(),
            created_at: "2026-07-31T12:01:00Z".to_string(),
        })
        .unwrap();

    let completed = store
        .complete_generation_apply("delta-empty", "2026-07-31T12:02:00Z")
        .unwrap();

    assert!(completed.outcomes.is_empty());
    assert_eq!(
        store
            .get_observed_generation(&fixture.mount_id)
            .unwrap()
            .unwrap()
            .generation_id,
        generation("generation-2")
    );
}

#[test]
fn active_apply_blocks_connection_and_settings_source_reset_transactionally() {
    let fixture = Fixture::new("active-reset-fence");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    seed(&mut store, &fixture);
    let delta = delta();
    let receipt = receipt(&delta);
    store
        .reserve_generation_apply(PreparedGenerationApply {
            receipt_sha256: receipt.canonical_sha256().unwrap(),
            receipt,
            delta,
            stage_root: "generation-delivery/delta-2".to_string(),
            created_at: "2026-07-31T12:01:00Z".to_string(),
        })
        .unwrap();

    let changed_connection =
        MountConfig::new(fixture.mount_id.clone(), "backend", &fixture.mount_root)
            .with_connection_id(ConnectionId::new("replacement"));
    let connection_error = store
        .save_mount(changed_connection)
        .expect_err("connection reset must not orphan active apply");
    assert!(matches!(connection_error, StoreError::InvalidState(_)));

    let changed_settings =
        MountConfig::new(fixture.mount_id.clone(), "backend", &fixture.mount_root)
            .with_settings_json(r#"{"view":"replacement"}"#);
    let settings_error = store
        .save_mount(changed_settings)
        .expect_err("settings reset must not orphan active apply");
    assert!(matches!(settings_error, StoreError::InvalidState(_)));

    let mount = store.get_mount(&fixture.mount_id).unwrap().unwrap();
    assert_eq!(mount.connection_id, None);
    assert_eq!(mount.settings_json, "{}");
    assert!(
        store
            .get_observed_generation(&fixture.mount_id)
            .unwrap()
            .is_some()
    );
    assert_eq!(store.list_active_generation_applies().unwrap().len(), 1);
}

#[test]
fn source_reset_retires_clean_completed_lineage_but_preserves_conflicts() {
    let clean_fixture = Fixture::new("completed-clean-reset");
    let mut clean_store = SqliteStateStore::open(clean_fixture.state_root.clone()).unwrap();
    seed(&mut clean_store, &clean_fixture);
    let mut clean_delta = delta();
    clean_delta.delta_id = "delta-clean-lineage".to_string();
    clean_delta.entries.clear();
    let clean_receipt = receipt(&clean_delta);
    clean_store
        .reserve_generation_apply(PreparedGenerationApply {
            receipt_sha256: clean_receipt.canonical_sha256().unwrap(),
            receipt: clean_receipt,
            delta: clean_delta,
            stage_root: "generation-delivery/clean-lineage".to_string(),
            created_at: "2026-07-31T12:01:00Z".to_string(),
        })
        .unwrap();
    clean_store
        .complete_generation_apply("delta-clean-lineage", "2026-07-31T12:02:00Z")
        .unwrap();
    clean_store
        .save_mount(
            MountConfig::new(
                clean_fixture.mount_id.clone(),
                "backend",
                &clean_fixture.mount_root,
            )
            .with_settings_json(r#"{"view":"new"}"#),
        )
        .unwrap();
    assert!(
        clean_store
            .get_generation_apply("delta-clean-lineage")
            .unwrap()
            .is_none()
    );
    assert!(
        clean_store
            .get_observed_generation(&clean_fixture.mount_id)
            .unwrap()
            .is_none()
    );

    let conflict_fixture = Fixture::new("completed-conflict-reset");
    let mut conflict_store = SqliteStateStore::open(conflict_fixture.state_root.clone()).unwrap();
    seed(&mut conflict_store, &conflict_fixture);
    let conflict_delta = delta();
    let conflict_receipt = receipt(&conflict_delta);
    conflict_store
        .reserve_generation_apply(PreparedGenerationApply {
            receipt_sha256: conflict_receipt.canonical_sha256().unwrap(),
            receipt: conflict_receipt,
            delta: conflict_delta.clone(),
            stage_root: "generation-delivery/conflict-lineage".to_string(),
            created_at: "2026-07-31T12:01:00Z".to_string(),
        })
        .unwrap();
    conflict_store
        .record_generation_apply_outcome(
            &conflict_delta.delta_id,
            0,
            GenerationApplyOutcome::Conflict {
                local_sha256: Some(digest('9')),
                incoming_identity: conflict_delta.entries[0].new.clone(),
            },
            "2026-07-31T12:02:00Z",
        )
        .unwrap();
    conflict_store
        .complete_generation_apply(&conflict_delta.delta_id, "2026-07-31T12:03:00Z")
        .unwrap();
    let error = conflict_store
        .save_mount(
            MountConfig::new(
                conflict_fixture.mount_id.clone(),
                "backend",
                &conflict_fixture.mount_root,
            )
            .with_connection_id(ConnectionId::new("replacement")),
        )
        .expect_err("completed conflict evidence must survive source reset");
    assert!(matches!(error, StoreError::InvalidState(_)));
    assert!(
        conflict_store
            .get_generation_apply(&conflict_delta.delta_id)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        conflict_store
            .list_generation_paths(&conflict_fixture.mount_id)
            .unwrap()[0]
            .state,
        GenerationPathState::Conflicted
    );
}

#[test]
fn source_reset_preserves_pending_virtual_mutations_and_unsettled_push_journals() {
    use locality_core::journal::{JournalEntry, JournalStatus, PushId};
    use locality_core::planner::PushPlan;
    use locality_store::{
        JournalRepository, VirtualMutationKind, VirtualMutationRecord, VirtualMutationRepository,
    };

    let virtual_fixture = Fixture::new("reset-pending-virtual");
    let mut virtual_store = SqliteStateStore::open(virtual_fixture.state_root.clone()).unwrap();
    seed(&mut virtual_store, &virtual_fixture);
    let mutation = VirtualMutationRecord {
        mount_id: virtual_fixture.mount_id.clone(),
        local_id: "local-rename".to_string(),
        mutation_kind: VirtualMutationKind::Rename,
        target_remote_id: Some(RemoteId::new("page-1")),
        parent_remote_id: None,
        original_path: Some(PathBuf::from("Roadmap.md")),
        projected_path: PathBuf::from("Roadmap-local.md"),
        title: "Roadmap local".to_string(),
        content_path: Some(virtual_fixture.mount_root.join("Roadmap-local.md")),
        created_at: "2026-07-31T12:00:00Z".to_string(),
        updated_at: "2026-07-31T12:00:00Z".to_string(),
    };
    virtual_store
        .save_virtual_mutation(mutation.clone())
        .unwrap();
    let error = virtual_store
        .save_mount(
            MountConfig::new(
                virtual_fixture.mount_id.clone(),
                "backend",
                &virtual_fixture.mount_root,
            )
            .with_settings_json(r#"{"view":"replacement"}"#),
        )
        .expect_err("pending virtual mutation must fence source reset");
    assert!(matches!(error, StoreError::InvalidState(_)));
    assert_eq!(
        virtual_store
            .get_virtual_mutation(&virtual_fixture.mount_id, &mutation.local_id)
            .unwrap(),
        Some(mutation)
    );

    let push_fixture = Fixture::new("reset-unsettled-push");
    let mut push_store = SqliteStateStore::open(push_fixture.state_root.clone()).unwrap();
    seed(&mut push_store, &push_fixture);
    let journal = JournalEntry::new(
        PushId("push-pending-reset".to_string()),
        push_fixture.mount_id.clone(),
        vec![RemoteId::new("page-1")],
        PushPlan::default(),
        JournalStatus::Applied,
    );
    push_store.append_journal(journal.clone()).unwrap();
    let error = push_store
        .save_mount(
            MountConfig::new(
                push_fixture.mount_id.clone(),
                "backend",
                &push_fixture.mount_root,
            )
            .with_connection_id(ConnectionId::new("replacement")),
        )
        .expect_err("unsettled push journal must fence source reset");
    assert!(matches!(error, StoreError::InvalidState(_)));
    assert_eq!(
        push_store.get_journal(&journal.push_id).unwrap(),
        Some(journal)
    );
}

#[test]
fn schema_20_migration_preserves_pending_local_state_and_adds_delivery_tables() {
    use locality_core::journal::{JournalEntry, JournalStatus, PushId};
    use locality_core::planner::PushPlan;
    use locality_store::{
        JournalRepository, VirtualMutationKind, VirtualMutationRecord, VirtualMutationRepository,
    };
    use rusqlite::Connection;

    let fixture = Fixture::new("migration-v20");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    store
        .save_mount(MountConfig::new(
            fixture.mount_id.clone(),
            "notion",
            &fixture.mount_root,
        ))
        .unwrap();
    let journal = JournalEntry::new(
        PushId("push-pending".to_string()),
        fixture.mount_id.clone(),
        vec![RemoteId::new("page-1")],
        PushPlan::default(),
        JournalStatus::Prepared,
    );
    store.append_journal(journal.clone()).unwrap();
    let mutation = VirtualMutationRecord {
        mount_id: fixture.mount_id.clone(),
        local_id: "local-dirty".to_string(),
        mutation_kind: VirtualMutationKind::Create,
        target_remote_id: None,
        parent_remote_id: None,
        original_path: None,
        projected_path: PathBuf::from("dirty.md"),
        title: "Dirty".to_string(),
        content_path: Some(fixture.mount_root.join("dirty.md")),
        created_at: "2026-07-31T10:00:00Z".to_string(),
        updated_at: "2026-07-31T10:00:00Z".to_string(),
    };
    store.save_virtual_mutation(mutation.clone()).unwrap();
    fs::write(fixture.mount_root.join("dirty.md"), b"local pending bytes").unwrap();
    let db_path = store.db_path.clone();
    drop(store);

    let connection = Connection::open(&db_path).unwrap();
    connection
        .execute_batch(
            "DROP TABLE generation_apply_outcomes;
         DROP INDEX generation_apply_one_active_per_source;
         DROP TABLE generation_apply_journals;
         DROP TABLE generation_paths;
         DROP TABLE observed_generations;
         DELETE FROM state_components WHERE component_id = 'durable:generation_delivery';
         UPDATE state_components SET version = 20 WHERE component_id = 'core:schema';
         PRAGMA user_version = 20;",
        )
        .unwrap();
    drop(connection);

    let reopened = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    assert_eq!(
        reopened.get_journal(&journal.push_id).unwrap(),
        Some(journal)
    );
    assert_eq!(
        reopened
            .get_virtual_mutation(&fixture.mount_id, "local-dirty")
            .unwrap(),
        Some(mutation)
    );
    assert_eq!(
        fs::read(fixture.mount_root.join("dirty.md")).unwrap(),
        b"local pending bytes"
    );
    assert_eq!(SqliteStateStore::current_schema_version(), 26);
    assert!(
        reopened
            .get_observed_generation(&fixture.mount_id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn genuine_schema_21_component_v1_fixture_migrates_without_losing_active_state() {
    use rusqlite::Connection;

    let fixture = Fixture::new("migration-v21-journal-mount");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    seed(&mut store, &fixture);
    let delta = delta();
    let receipt = receipt(&delta);
    store
        .reserve_generation_apply(PreparedGenerationApply {
            receipt_sha256: receipt.canonical_sha256().unwrap(),
            receipt,
            delta,
            stage_root: "generation-delivery/delta-2".to_string(),
            created_at: "2026-07-31T12:01:00Z".to_string(),
        })
        .unwrap();
    let db_path = store.db_path.clone();
    drop(store);

    let connection = Connection::open(&db_path).unwrap();
    connection
        .execute_batch(include_str!(
            "fixtures/generation-delivery-v21-component-v1.sql"
        ))
        .unwrap();
    let constraint_error = connection
        .execute(
            "UPDATE generation_apply_journals
             SET status = 'completed'
             WHERE delta_id = 'delta-2'",
            [],
        )
        .expect_err("released v21 active/status CHECK must be present");
    assert!(
        constraint_error
            .to_string()
            .contains("CHECK constraint failed")
    );
    drop(connection);

    let mut reopened = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    let active = reopened.list_active_generation_applies().unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].delta.mount_id.as_str(), fixture.mount_id.as_str());
    let migrated_path = reopened
        .list_generation_paths(&fixture.mount_id)
        .unwrap()
        .remove(0);
    assert_eq!(migrated_path.local_logical_path, migrated_path.logical_path);
    let connection = Connection::open(&db_path).unwrap();
    let component: (i64, i64) = connection
        .query_row(
            "SELECT version, min_reader_version FROM state_components
             WHERE component_id = 'durable:generation_delivery'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(component, (6, 6));
    let error = reopened
        .clear_mount_source_state(&fixture.mount_id)
        .expect_err("migrated active journal must fence source reset");
    assert!(matches!(error, StoreError::InvalidState(_)));
}

#[test]
fn partial_v2_v6_generation_migration_is_atomic_and_resumable_per_column() {
    use rusqlite::Connection;

    let fixture = Fixture::new("partial-v2-v6-migration");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    seed(&mut store, &fixture);
    let db_path = store.db_path.clone();
    drop(store);

    let connection = Connection::open(&db_path).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE generation_paths DROP COLUMN base_payload_entry_index;
             ALTER TABLE generation_paths DROP COLUMN local_logical_path;
             ALTER TABLE generation_paths DROP COLUMN conflict_payload_entry_index;
             ALTER TABLE generation_inode_evidence DROP COLUMN base_payload_entry_index;
             CREATE TRIGGER fail_generation_path_backfill
             BEFORE UPDATE ON generation_paths
             BEGIN
                 SELECT RAISE(ABORT, 'injected migration interruption');
             END;
             UPDATE state_components
             SET version = 2, min_reader_version = 2
             WHERE component_id = 'durable:generation_delivery';
             UPDATE state_components SET version = 23
             WHERE component_id = 'core:schema';
             PRAGMA user_version = 23;",
        )
        .unwrap();
    drop(connection);

    assert!(SqliteStateStore::open(fixture.state_root.clone()).is_err());
    let connection = Connection::open(&db_path).unwrap();
    for (table, column) in [
        ("generation_paths", "base_payload_entry_index"),
        ("generation_paths", "local_logical_path"),
        ("generation_paths", "conflict_payload_entry_index"),
        ("generation_inode_evidence", "base_payload_entry_index"),
    ] {
        let present: bool = connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2
                 )",
                rusqlite::params![table, column],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!present, "failed migration advanced {table}.{column}");
    }
    let component: (i64, i64) = connection
        .query_row(
            "SELECT version, min_reader_version FROM state_components
             WHERE component_id = 'durable:generation_delivery'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let user_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(component, (2, 2));
    assert_eq!(user_version, 23);
    connection
        .execute_batch("DROP TRIGGER fail_generation_path_backfill;")
        .unwrap();
    drop(connection);

    let reopened = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    let connection = Connection::open(&reopened.db_path).unwrap();
    for (table, column) in [
        ("generation_paths", "base_payload_entry_index"),
        ("generation_paths", "local_logical_path"),
        ("generation_paths", "conflict_payload_entry_index"),
        ("generation_inode_evidence", "base_payload_entry_index"),
    ] {
        let present: bool = connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2
                 )",
                rusqlite::params![table, column],
                |row| row.get(0),
            )
            .unwrap();
        assert!(present, "retry did not restore {table}.{column}");
    }
    let component: (i64, i64) = connection
        .query_row(
            "SELECT version, min_reader_version FROM state_components
             WHERE component_id = 'durable:generation_delivery'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let user_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(component, (6, 6));
    assert_eq!(user_version, 26);
    assert_eq!(
        reopened.list_generation_paths(&fixture.mount_id).unwrap()[0].local_logical_path,
        "Roadmap.md"
    );
}

struct Fixture {
    state_root: PathBuf,
    mount_root: PathBuf,
    mount_id: MountId,
}

impl Fixture {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "locality-generation-delivery-{label}-{}-{stamp}-{nonce}",
            std::process::id()
        ));
        let state_root = root.join("state");
        let mount_root = root.join("mount");
        fs::create_dir_all(&mount_root).unwrap();
        Self {
            state_root,
            mount_root,
            mount_id: MountId::new("mount-main"),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.state_root.parent().unwrap());
    }
}
