use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
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
    canonical_target_inventory_sha256,
};
use locality_protocol::freshness_delivery_transport::{
    GenerationBodyWindowCapability, GenerationPinFallbackPolicy, GenerationPinLeaseCapability,
    GenerationTransportCapabilities,
};
use locality_protocol::generation_baseline::GenerationBaselineRefreshModeV1;
use locality_protocol::workspace_layout::LayoutDigest;
use locality_store::{
    ConnectionId, GenerationApplyOutcome, GenerationApplyStatus, GenerationBaselineSeedRecord,
    GenerationBaselineSeedRecordV2, GenerationDeliveryRepository,
    GenerationInodeEvidenceConflictUpdate, GenerationInodeEvidenceRecord,
    GenerationInodeEvidenceResolution, GenerationPathRecord, GenerationPathState,
    GenerationRetainedInodeRecord, GenerationTransportSelectionBinding, MountConfig,
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
    identity_for(
        "projection-roadmap",
        "Roadmap.md",
        version,
        digest_character,
        bytes,
    )
}

fn identity_for(
    projection_id: &str,
    logical_path: &str,
    version: &str,
    digest_character: char,
    bytes: u64,
) -> GenerationFileIdentity {
    GenerationFileIdentity {
        projection_id: ProjectionId::new(projection_id),
        logical_path: LogicalPath::new(logical_path).unwrap(),
        content_version_id: ContentVersionId::new(version),
        content_sha256: digest(digest_character),
        byte_length: bytes,
    }
}

fn source_seed(
    mount_id: &MountId,
    source: &str,
    generation_id: &str,
    identity: Option<GenerationFileIdentity>,
) -> GenerationBaselineSeedRecord {
    let paths = identity
        .into_iter()
        .map(|identity| GenerationPathRecord {
            mount_id: mount_id.clone(),
            projection_id: identity.projection_id.clone(),
            logical_path: identity.logical_path.as_str().to_string(),
            local_logical_path: identity.logical_path.as_str().to_string(),
            base_generation_id: generation(generation_id),
            base_identity: Some(identity),
            base_payload_delta_id: None,
            base_payload_entry_index: None,
            conflict_payload_delta_id: None,
            conflict_payload_entry_index: None,
            state: GenerationPathState::Clean,
            incoming_identity: None,
            updated_at: "2026-07-31T11:00:00Z".to_string(),
        })
        .collect::<Vec<_>>();
    let inventory = paths
        .iter()
        .map(|path| path.base_identity.clone().unwrap())
        .collect::<Vec<_>>();
    GenerationBaselineSeedRecord::new(
        ObservedGenerationRecord {
            mount_id: mount_id.clone(),
            source_connection_id: SourceConnectionId::new(source),
            generation_id: generation(generation_id),
            inventory_sha256: canonical_target_inventory_sha256(&inventory).unwrap(),
            workspace_layout_version: 1,
            workspace_layout_digest: digest('a'),
            last_receipt_sha256: None,
            updated_at: "2026-07-31T11:00:00Z".to_string(),
        },
        paths,
    )
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

fn delta_for_source(
    delta_id: &str,
    source: &str,
    base_generation: &str,
    target_generation: &str,
    old: Option<GenerationFileIdentity>,
    new: Option<GenerationFileIdentity>,
) -> GenerationDelta {
    let target_inventory = new.iter().cloned().collect::<Vec<_>>();
    GenerationDelta {
        format_version: GENERATION_DELTA_FORMAT_VERSION,
        minimum_reader_version: FRESHNESS_DELIVERY_READER_VERSION,
        delta_id: delta_id.to_string(),
        mount_id: PortableMountId::new("mount-main").unwrap(),
        source_connection_id: SourceConnectionId::new(source),
        base_generation_id: generation(base_generation),
        target_generation_id: generation(target_generation),
        target_complete: true,
        target_inventory_sha256: canonical_target_inventory_sha256(&target_inventory).unwrap(),
        workspace_layout_version: 1,
        workspace_layout_digest: LayoutDigest::new(digest('a')).unwrap(),
        entries: vec![GenerationDeltaEntry { old, new }],
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
    let base_identity = identity("content-1", '1', 3);
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
                inventory_sha256: canonical_target_inventory_sha256(&[base_identity.clone()])
                    .unwrap(),
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
                base_identity: Some(base_identity),
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
fn observed_generation_seed_exact_replay_is_path_order_independent() {
    let fixture = Fixture::new("seed-replay-path-order");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    store
        .save_mount(MountConfig::new(
            fixture.mount_id.clone(),
            "backend",
            &fixture.mount_root,
        ))
        .unwrap();
    let path = |projection: &str, logical_path: &str| GenerationPathRecord {
        mount_id: fixture.mount_id.clone(),
        projection_id: ProjectionId::new(projection),
        logical_path: logical_path.to_string(),
        local_logical_path: logical_path.to_string(),
        base_generation_id: generation("generation-1"),
        base_identity: Some(identity_for(
            projection,
            logical_path,
            &format!("content-{projection}"),
            if projection == "projection-a" {
                'a'
            } else {
                'b'
            },
            1,
        )),
        base_payload_delta_id: None,
        base_payload_entry_index: None,
        conflict_payload_delta_id: None,
        conflict_payload_entry_index: None,
        state: GenerationPathState::Clean,
        incoming_identity: None,
        updated_at: "2026-07-31T11:00:00Z".to_string(),
    };
    let first = path("projection-a", "A.md");
    let second = path("projection-b", "B.md");
    let expected = vec![first.clone(), second.clone()];
    let inventory = expected
        .iter()
        .map(|path| path.base_identity.clone().unwrap())
        .collect::<Vec<_>>();
    let observed = ObservedGenerationRecord {
        mount_id: fixture.mount_id.clone(),
        source_connection_id: SourceConnectionId::new("source-main"),
        generation_id: generation("generation-1"),
        inventory_sha256: canonical_target_inventory_sha256(&inventory).unwrap(),
        workspace_layout_version: 1,
        workspace_layout_digest: digest('a'),
        last_receipt_sha256: None,
        updated_at: "2026-07-31T11:00:00Z".to_string(),
    };

    store
        .seed_observed_generation(observed.clone(), expected.clone())
        .unwrap();
    store
        .seed_observed_generation(observed, vec![second, first])
        .unwrap();

    assert_eq!(
        store.list_generation_paths(&fixture.mount_id).unwrap(),
        expected
    );
}

#[test]
fn baseline_seed_rejects_incomplete_or_mismatched_inventory_before_commit() {
    let fixture = Fixture::new("baseline-inventory-validation");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    store
        .save_mount(MountConfig::new(
            fixture.mount_id.clone(),
            "backend",
            &fixture.mount_root,
        ))
        .unwrap();

    let valid = source_seed(
        &fixture.mount_id,
        "source-a",
        "generation-a1",
        Some(identity_for("projection-a", "A.md", "content-a1", 'a', 1)),
    );
    let mut mismatched_digest = valid.clone();
    mismatched_digest.observed.inventory_sha256 = digest('f');
    assert!(
        store
            .seed_observed_generations(vec![mismatched_digest])
            .is_err()
    );

    let mut missing_identity = valid.clone();
    missing_identity.paths[0].base_identity = None;
    assert!(
        store
            .seed_observed_generations(vec![missing_identity])
            .is_err()
    );

    let mut dirty_path = valid;
    dirty_path.paths[0].state = GenerationPathState::Dirty;
    assert!(store.seed_observed_generations(vec![dirty_path]).is_err());
    assert!(
        store
            .list_observed_generations(&fixture.mount_id)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn shared_mount_baseline_rejects_mixed_layouts_atomically() {
    let fixture = Fixture::new("baseline-shared-layout-validation");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    store
        .save_mount(MountConfig::new(
            fixture.mount_id.clone(),
            "backend",
            &fixture.mount_root,
        ))
        .unwrap();
    let source_a = source_seed(
        &fixture.mount_id,
        "source-a",
        "generation-a1",
        Some(identity_for("projection-a", "A.md", "content-a1", 'a', 1)),
    );
    let mut source_b = source_seed(
        &fixture.mount_id,
        "source-b",
        "generation-b1",
        Some(identity_for("projection-b", "B.md", "content-b1", 'b', 1)),
    );
    source_b.observed.workspace_layout_digest = digest('b');

    assert!(
        store
            .seed_observed_generations(vec![source_a, source_b])
            .is_err()
    );
    assert!(
        store
            .list_observed_generations(&fixture.mount_id)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn multi_mount_baseline_rejects_mixed_layouts_atomically() {
    let fixture = Fixture::new("baseline-multi-mount-layout-validation");
    let other_mount_id = MountId::new("mount-other");
    let other_mount_root = fixture.state_root.parent().unwrap().join("mount-other");
    fs::create_dir_all(&other_mount_root).unwrap();
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    for (mount_id, mount_root) in [
        (&fixture.mount_id, &fixture.mount_root),
        (&other_mount_id, &other_mount_root),
    ] {
        store
            .save_mount(MountConfig::new(mount_id.clone(), "backend", mount_root))
            .unwrap();
    }
    let first_mount = source_seed(
        &fixture.mount_id,
        "source-a",
        "generation-a1",
        Some(identity_for("projection-a", "A.md", "content-a1", 'a', 1)),
    );
    let mut second_mount = source_seed(
        &other_mount_id,
        "source-b",
        "generation-b1",
        Some(identity_for("projection-b", "B.md", "content-b1", 'b', 1)),
    );
    second_mount.observed.workspace_layout_version = 2;
    second_mount.observed.workspace_layout_digest = digest('b');

    assert!(
        store
            .seed_observed_generations(vec![first_mount, second_mount])
            .is_err()
    );
    for mount_id in [&fixture.mount_id, &other_mount_id] {
        assert!(
            store
                .list_observed_generations(mount_id)
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn shared_mount_sources_seed_atomically_replay_after_restart_and_keep_legacy_reads_safe() {
    let fixture = Fixture::new("shared-mount-baseline");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    store
        .save_mount(MountConfig::new(
            fixture.mount_id.clone(),
            "backend",
            &fixture.mount_root,
        ))
        .unwrap();
    let source_a = source_seed(
        &fixture.mount_id,
        "source-a",
        "generation-a1",
        Some(identity_for("projection-a", "A.md", "content-a1", 'a', 1)),
    );
    let source_b = source_seed(
        &fixture.mount_id,
        "source-b",
        "generation-b1",
        Some(identity_for("projection-b", "B.md", "content-b1", 'b', 1)),
    );
    let source_empty = source_seed(&fixture.mount_id, "source-empty", "generation-empty1", None);
    let baseline = vec![source_b.clone(), source_empty.clone(), source_a.clone()];
    store.seed_observed_generations(baseline.clone()).unwrap();

    assert_eq!(
        store
            .list_observed_generations(&fixture.mount_id)
            .unwrap()
            .into_iter()
            .map(|record| record.source_connection_id)
            .collect::<Vec<_>>(),
        vec![
            SourceConnectionId::new("source-a"),
            SourceConnectionId::new("source-b"),
            SourceConnectionId::new("source-empty"),
        ]
    );
    assert!(store.get_observed_generation(&fixture.mount_id).is_err());
    assert!(store.list_generation_paths(&fixture.mount_id).is_err());
    assert!(
        store
            .seed_observed_generation(source_a.observed.clone(), source_a.paths.clone())
            .is_err()
    );
    assert!(
        store
            .list_generation_paths_for_source(
                &fixture.mount_id,
                &SourceConnectionId::new("source-empty"),
            )
            .unwrap()
            .is_empty()
    );
    drop(store);

    let mut reopened = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    reopened
        .seed_observed_generations(baseline)
        .expect("exact multi-source replay survives restart");
    assert_eq!(
        reopened
            .get_observed_generation_for_source(
                &fixture.mount_id,
                &SourceConnectionId::new("source-b"),
            )
            .unwrap(),
        Some(source_b.observed)
    );
}

#[test]
fn changed_multi_source_replay_rolls_back_new_source() {
    let fixture = Fixture::new("shared-mount-baseline-rollback");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    store
        .save_mount(MountConfig::new(
            fixture.mount_id.clone(),
            "backend",
            &fixture.mount_root,
        ))
        .unwrap();
    let source_a = source_seed(&fixture.mount_id, "source-a", "generation-a1", None);
    store
        .seed_observed_generations(vec![source_a.clone()])
        .unwrap();
    let mut changed_a = source_a;
    changed_a.observed.inventory_sha256 = digest('z');
    let source_b = source_seed(&fixture.mount_id, "source-b", "generation-b1", None);

    assert!(
        store
            .seed_observed_generations(vec![source_b, changed_a])
            .is_err()
    );
    assert!(
        store
            .get_observed_generation_for_source(
                &fixture.mount_id,
                &SourceConnectionId::new("source-b"),
            )
            .unwrap()
            .is_none()
    );
}

#[test]
fn sequential_partial_baselines_cannot_extend_an_existing_mount() {
    let fixture = Fixture::new("shared-mount-sequential-partial-baselines");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    store
        .save_mount(MountConfig::new(
            fixture.mount_id.clone(),
            "backend",
            &fixture.mount_root,
        ))
        .unwrap();
    let source_a = source_seed(&fixture.mount_id, "source-a", "generation-a1", None);
    let source_b = source_seed(&fixture.mount_id, "source-b", "generation-b2", None);
    store
        .seed_observed_generations(vec![source_a.clone()])
        .unwrap();

    let error = store
        .seed_observed_generations(vec![source_a, source_b])
        .expect_err("a later baseline cannot append a source to an existing mount");

    assert!(matches!(error, StoreError::InvalidState(_)));
    let observed = store.list_observed_generations(&fixture.mount_id).unwrap();
    assert_eq!(observed.len(), 1);
    assert_eq!(
        observed[0].source_connection_id,
        SourceConnectionId::new("source-a")
    );
}

#[test]
fn concurrent_partial_baselines_cannot_merge_on_an_empty_mount() {
    let fixture = Fixture::new("shared-mount-concurrent-partial-baselines");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    store
        .save_mount(MountConfig::new(
            fixture.mount_id.clone(),
            "backend",
            &fixture.mount_root,
        ))
        .unwrap();
    drop(store);

    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for (source, generation_id) in [("source-a", "generation-a1"), ("source-b", "generation-b2")] {
        let state_root = fixture.state_root.clone();
        let mount_id = fixture.mount_id.clone();
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            let mut store = SqliteStateStore::open(state_root).unwrap();
            let seed = source_seed(&mount_id, source, generation_id, None);
            barrier.wait();
            store.seed_observed_generations(vec![seed])
        }));
    }
    barrier.wait();

    let results = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(StoreError::InvalidState(_))))
            .count(),
        1
    );

    let reopened = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    assert_eq!(
        reopened
            .list_observed_generations(&fixture.mount_id)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn active_apply_is_mount_wide_and_fences_baseline_target_claims() {
    let fixture = Fixture::new("shared-mount-active-fence");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    store
        .save_mount(MountConfig::new(
            fixture.mount_id.clone(),
            "backend",
            &fixture.mount_root,
        ))
        .unwrap();
    store
        .seed_observed_generations(vec![
            source_seed(&fixture.mount_id, "source-a", "generation-a1", None),
            source_seed(&fixture.mount_id, "source-b", "generation-b1", None),
        ])
        .unwrap();

    let claimed = identity_for("projection-claim", "Claim.md", "content-a2", 'a', 1);
    let delta_a = delta_for_source(
        "delta-a-active",
        "source-a",
        "generation-a1",
        "generation-a2",
        None,
        Some(claimed.clone()),
    );
    let receipt_a = receipt(&delta_a);
    store
        .reserve_generation_apply(PreparedGenerationApply {
            delta: delta_a,
            receipt_sha256: receipt_a.canonical_sha256().unwrap(),
            receipt: receipt_a,
            stage_root: "generation-delivery/delta-a-active".to_string(),
            created_at: "2026-07-31T12:01:00Z".to_string(),
        })
        .unwrap();

    let delta_b = delta_for_source(
        "delta-b-concurrent",
        "source-b",
        "generation-b1",
        "generation-b2",
        None,
        Some(identity_for("projection-b", "B.md", "content-b2", 'b', 1)),
    );
    let receipt_b = receipt(&delta_b);
    assert!(
        store
            .reserve_generation_apply(PreparedGenerationApply {
                delta: delta_b,
                receipt_sha256: receipt_b.canonical_sha256().unwrap(),
                receipt: receipt_b,
                stage_root: "generation-delivery/delta-b-concurrent".to_string(),
                created_at: "2026-07-31T12:01:00Z".to_string(),
            })
            .is_err(),
        "another source cannot reserve an apply while the mount has a durable target claim"
    );

    let source_c = source_seed(
        &fixture.mount_id,
        "source-c",
        "generation-c1",
        Some(claimed),
    );
    assert!(store.seed_observed_generations(vec![source_c]).is_err());
    assert!(
        store
            .get_observed_generation_for_source(
                &fixture.mount_id,
                &SourceConnectionId::new("source-c"),
            )
            .unwrap()
            .is_none()
    );
}

#[test]
fn typed_refresh_mode_is_validated_persisted_and_never_defaulted_for_ineligible_state() {
    let fixture = Fixture::new("typed-refresh-mode");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    store
        .save_mount(MountConfig::new(
            fixture.mount_id.clone(),
            "backend",
            &fixture.mount_root,
        ))
        .unwrap();
    let long_source = "s".repeat(129);
    let seed = source_seed(
        &fixture.mount_id,
        &long_source,
        "generation-full-export",
        None,
    );
    assert!(
        store.seed_observed_generations(vec![seed.clone()]).is_err(),
        "the compatibility API must not silently label an ineligible source as delta-capable"
    );
    store
        .seed_observed_generations_v2(vec![GenerationBaselineSeedRecordV2::new(
            seed,
            GenerationBaselineRefreshModeV1::FullExportOnly,
        )])
        .unwrap();
    let persisted = store
        .get_observed_generation_for_source_v2(
            &fixture.mount_id,
            &SourceConnectionId::new(long_source),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        persisted.refresh_mode,
        GenerationBaselineRefreshModeV1::FullExportOnly
    );
}

#[test]
fn v6_fixture_migrates_exact_generation_rows_and_paths_to_v7() {
    use rusqlite::Connection;

    let fixture = Fixture::new("migration-component-v6");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    seed(&mut store, &fixture);
    let expected_observed = store
        .get_observed_generation(&fixture.mount_id)
        .unwrap()
        .unwrap();
    let expected_paths = store.list_generation_paths(&fixture.mount_id).unwrap();
    let db_path = store.db_path.clone();
    drop(store);

    let connection = Connection::open(&db_path).unwrap();
    connection
        .execute_batch(include_str!(
            "fixtures/generation-delivery-component-v6.sql"
        ))
        .unwrap();
    drop(connection);

    let reopened = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    assert_eq!(
        reopened
            .get_observed_generation_for_source(
                &fixture.mount_id,
                &expected_observed.source_connection_id,
            )
            .unwrap(),
        Some(expected_observed.clone())
    );
    assert_eq!(
        reopened
            .list_generation_paths_for_source(
                &fixture.mount_id,
                &expected_observed.source_connection_id,
            )
            .unwrap(),
        expected_paths
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
    let primary_key = connection
        .prepare(
            "SELECT name FROM pragma_table_info('observed_generations')
             WHERE pk > 0 ORDER BY pk",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(component, (7, 7));
    assert_eq!(primary_key, ["mount_id", "source_connection_id"]);
    assert_eq!(
        reopened
            .get_observed_generation_for_source_v2(
                &fixture.mount_id,
                &expected_observed.source_connection_id,
            )
            .unwrap()
            .unwrap()
            .refresh_mode,
        GenerationBaselineRefreshModeV1::GenerationDeltaV1
    );
}

#[test]
fn v6_ineligible_generation_row_migrates_to_full_export_only() {
    use rusqlite::Connection;

    let fixture = Fixture::new("migration-component-v6-full-export");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    seed(&mut store, &fixture);
    let db_path = store.db_path.clone();
    drop(store);

    let connection = Connection::open(&db_path).unwrap();
    connection
        .execute_batch(include_str!(
            "fixtures/generation-delivery-component-v6.sql"
        ))
        .unwrap();
    let long_source = "s".repeat(129);
    connection
        .execute(
            "UPDATE observed_generations SET source_connection_id = ?1",
            [&long_source],
        )
        .unwrap();
    drop(connection);

    let reopened = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    let migrated = reopened
        .get_observed_generation_for_source_v2(
            &fixture.mount_id,
            &SourceConnectionId::new(long_source),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        migrated.refresh_mode,
        GenerationBaselineRefreshModeV1::FullExportOnly
    );
}

#[test]
fn failed_v6_to_v7_rebuild_rolls_back_every_schema_and_component_change() {
    use rusqlite::Connection;

    let fixture = Fixture::new("migration-component-v6-rollback");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    seed(&mut store, &fixture);
    let db_path = store.db_path.clone();
    drop(store);

    let connection = Connection::open(&db_path).unwrap();
    connection
        .execute_batch(include_str!(
            "fixtures/generation-delivery-component-v6.sql"
        ))
        .unwrap();
    connection
        .execute(
            "INSERT INTO generation_paths (
                mount_id, projection_id, logical_path, local_logical_path,
                base_generation_id, base_identity_json,
                base_payload_delta_id, base_payload_entry_index,
                conflict_payload_delta_id, conflict_payload_entry_index,
                state, incoming_identity_json, updated_at
             )
             SELECT mount_id, 'projection-injected', 'Injected.md', local_logical_path,
                    base_generation_id, NULL, base_payload_delta_id,
                    base_payload_entry_index, conflict_payload_delta_id,
                    conflict_payload_entry_index, state, NULL, updated_at
             FROM generation_paths LIMIT 1",
            [],
        )
        .unwrap();
    drop(connection);

    assert!(SqliteStateStore::open(fixture.state_root.clone()).is_err());
    let connection = Connection::open(&db_path).unwrap();
    let component: (i64, i64) = connection
        .query_row(
            "SELECT version, min_reader_version FROM state_components
             WHERE component_id = 'durable:generation_delivery'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let source_column: bool = connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('generation_paths')
                WHERE name = 'source_connection_id'
             )",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let rows: i64 = connection
        .query_row("SELECT COUNT(*) FROM generation_paths", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(component, (6, 6));
    assert!(!source_column);
    assert_eq!(rows, 2);
    connection
        .execute(
            "DELETE FROM generation_paths WHERE projection_id = 'projection-injected'",
            [],
        )
        .unwrap();
    drop(connection);

    let reopened = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    assert_eq!(
        reopened
            .list_generation_paths_for_source(
                &fixture.mount_id,
                &SourceConnectionId::new("source-main"),
            )
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn shared_mount_applies_and_acknowledgments_advance_sources_independently() {
    let fixture = Fixture::new("shared-mount-independent-applies");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    store
        .save_mount(MountConfig::new(
            fixture.mount_id.clone(),
            "backend",
            &fixture.mount_root,
        ))
        .unwrap();
    let a1 = identity_for("projection-a", "A.md", "content-a1", 'a', 1);
    let a2 = identity_for("projection-a", "A.md", "content-a2", 'c', 2);
    let b1 = identity_for("projection-b", "B.md", "content-b1", 'b', 1);
    let b2 = identity_for("projection-b", "B.md", "content-b2", 'd', 2);
    store
        .seed_observed_generations(vec![
            source_seed(
                &fixture.mount_id,
                "source-a",
                "generation-a1",
                Some(a1.clone()),
            ),
            source_seed(
                &fixture.mount_id,
                "source-b",
                "generation-b1",
                Some(b1.clone()),
            ),
        ])
        .unwrap();

    let crossed = delta_for_source(
        "delta-crossed",
        "source-a",
        "generation-a1",
        "generation-a2",
        Some(b1.clone()),
        Some(b2.clone()),
    );
    let crossed_receipt = receipt(&crossed);
    assert!(
        store
            .reserve_generation_apply(PreparedGenerationApply {
                delta: crossed,
                receipt_sha256: crossed_receipt.canonical_sha256().unwrap(),
                receipt: crossed_receipt,
                stage_root: "generation-delivery/crossed".to_string(),
                created_at: "2026-07-31T12:00:00Z".to_string(),
            })
            .is_err(),
        "source A must not inspect source B's projection base"
    );

    let deltas = [
        delta_for_source(
            "delta-a2",
            "source-a",
            "generation-a1",
            "generation-a2",
            Some(a1),
            Some(a2),
        ),
        delta_for_source(
            "delta-b2",
            "source-b",
            "generation-b1",
            "generation-b2",
            Some(b1),
            Some(b2),
        ),
    ];
    for delta in &deltas {
        let receipt = receipt(delta);
        store
            .reserve_generation_apply_v2(PreparedGenerationApplyV2::new(
                PreparedGenerationApply {
                    delta: delta.clone(),
                    receipt_sha256: receipt.canonical_sha256().unwrap(),
                    receipt,
                    stage_root: format!("generation-delivery/{}", delta.delta_id),
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
    }

    assert_eq!(
        store
            .get_observed_generation_for_source(
                &fixture.mount_id,
                &SourceConnectionId::new("source-a"),
            )
            .unwrap()
            .unwrap()
            .generation_id,
        generation("generation-a2")
    );
    assert_eq!(
        store
            .get_observed_generation_for_source(
                &fixture.mount_id,
                &SourceConnectionId::new("source-b"),
            )
            .unwrap()
            .unwrap()
            .generation_id,
        generation("generation-b2")
    );
    assert_eq!(
        store
            .list_pending_generation_acknowledgments_for_source(
                &fixture.mount_id,
                &SourceConnectionId::new("source-a"),
            )
            .unwrap()
            .len(),
        1
    );
    let a_journal = store.get_generation_apply("delta-a2").unwrap().unwrap();
    store
        .mark_generation_acknowledged(
            "delta-a2",
            &a_journal.receipt_sha256,
            "2026-07-31T12:04:00Z",
        )
        .unwrap();
    assert!(
        store
            .list_pending_generation_acknowledgments_for_source(
                &fixture.mount_id,
                &SourceConnectionId::new("source-a"),
            )
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .list_pending_generation_acknowledgments_for_source(
                &fixture.mount_id,
                &SourceConnectionId::new("source-b"),
            )
            .unwrap()
            .len(),
        1
    );
    store
        .reset_observed_generation_source(&fixture.mount_id, &SourceConnectionId::new("source-a"))
        .unwrap();
    assert!(
        store
            .get_observed_generation_for_source(
                &fixture.mount_id,
                &SourceConnectionId::new("source-a"),
            )
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .get_observed_generation_for_source(
                &fixture.mount_id,
                &SourceConnectionId::new("source-b"),
            )
            .unwrap()
            .unwrap()
            .generation_id,
        generation("generation-b2")
    );
}

#[test]
fn source_reset_is_fenced_by_another_sources_active_mount_apply() {
    let fixture = Fixture::new("shared-mount-reset-active-other-source");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    store
        .save_mount(MountConfig::new(
            fixture.mount_id.clone(),
            "backend",
            &fixture.mount_root,
        ))
        .unwrap();
    let a1 = identity_for("projection-a", "A.md", "content-a1", 'a', 1);
    let b1 = identity_for("projection-b", "B.md", "content-b1", 'b', 1);
    let b2 = identity_for("projection-b", "B.md", "content-b2", 'd', 2);
    store
        .seed_observed_generations(vec![
            source_seed(&fixture.mount_id, "source-a", "generation-a1", Some(a1)),
            source_seed(
                &fixture.mount_id,
                "source-b",
                "generation-b1",
                Some(b1.clone()),
            ),
        ])
        .unwrap();
    let delta = delta_for_source(
        "delta-b-active",
        "source-b",
        "generation-b1",
        "generation-b2",
        Some(b1),
        Some(b2),
    );
    let terminal_receipt = receipt(&delta);
    store
        .reserve_generation_apply(PreparedGenerationApply {
            delta,
            receipt_sha256: terminal_receipt.canonical_sha256().unwrap(),
            receipt: terminal_receipt,
            stage_root: "generation-delivery/delta-b-active".to_string(),
            created_at: "2026-07-31T12:01:00Z".to_string(),
        })
        .unwrap();

    let error = store
        .reset_observed_generation_source(&fixture.mount_id, &SourceConnectionId::new("source-a"))
        .expect_err("source A reset must not invalidate source B's active mount transaction");

    assert!(matches!(error, StoreError::InvalidState(_)));
    assert!(
        store
            .get_observed_generation_for_source(
                &fixture.mount_id,
                &SourceConnectionId::new("source-a"),
            )
            .unwrap()
            .is_some()
    );
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
    assert_eq!(component, (7, 7));
    assert_eq!(user_version, 28);
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
fn captured_retained_inode_hashes_and_lengths_advance_atomically() {
    use rusqlite::Connection;

    let fixture = Fixture::new("atomic-inode-fingerprints");
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
    store
        .record_generation_inode_evidence(GenerationInodeEvidenceRecord {
            delta_id: "delta-2".to_string(),
            entry_index: 0,
            mount_id: fixture.mount_id.clone(),
            logical_path: "Roadmap.md".to_string(),
            evidence_name: ".preimage".to_string(),
            captured_sha256: digest('1'),
            captured_byte_length: 3,
            visible_evidence: None,
            base_payload_delta_id: None,
            base_payload_entry_index: None,
            resolved_at: None,
            created_at: "2026-07-31T12:01:00Z".to_string(),
        })
        .unwrap();
    store
        .record_generation_apply_outcome(
            "delta-2",
            0,
            GenerationApplyOutcome::Merged,
            "2026-07-31T12:02:00Z",
        )
        .unwrap();
    store
        .complete_generation_apply("delta-2", "2026-07-31T12:03:00Z")
        .unwrap();

    let first = GenerationInodeEvidenceConflictUpdate {
        local_sha256: digest('m'),
        captured_sha256: digest('p'),
        captured_byte_length: 11,
        visible_evidence: Some(GenerationRetainedInodeRecord {
            evidence_name: ".visible".to_string(),
            captured_sha256: digest('v'),
            captured_byte_length: 22,
        }),
        updated_at: "2026-07-31T12:04:00Z".to_string(),
    };
    store
        .mark_generation_inode_evidence_conflict("delta-2", 0, first.clone())
        .unwrap();
    let evidence = store.list_generation_inode_evidence().unwrap().remove(0);
    assert_eq!(evidence.captured_sha256, first.captured_sha256);
    assert_eq!(evidence.captured_byte_length, first.captured_byte_length);
    assert_eq!(evidence.visible_evidence, first.visible_evidence);

    let connection = Connection::open(&store.db_path).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_inode_fingerprint_update
             BEFORE UPDATE ON generation_inode_evidence
             BEGIN
                 SELECT RAISE(ABORT, 'injected evidence update failure');
             END;",
        )
        .unwrap();
    drop(connection);
    let second = GenerationInodeEvidenceConflictUpdate {
        local_sha256: digest('n'),
        captured_sha256: digest('q'),
        captured_byte_length: 111,
        visible_evidence: Some(GenerationRetainedInodeRecord {
            evidence_name: ".visible".to_string(),
            captured_sha256: digest('w'),
            captured_byte_length: 222,
        }),
        updated_at: "2026-07-31T12:05:00Z".to_string(),
    };
    assert!(
        store
            .mark_generation_inode_evidence_conflict("delta-2", 0, second)
            .is_err()
    );
    let evidence = store.list_generation_inode_evidence().unwrap().remove(0);
    assert_eq!(evidence.captured_sha256, first.captured_sha256);
    assert_eq!(evidence.captured_byte_length, first.captured_byte_length);
    assert_eq!(evidence.visible_evidence, first.visible_evidence);
    let outcome = store
        .get_generation_apply("delta-2")
        .unwrap()
        .unwrap()
        .outcomes[0]
        .1
        .clone();
    assert!(matches!(
        outcome,
        GenerationApplyOutcome::Conflict {
            local_sha256: Some(local_sha256),
            ..
        } if local_sha256 == first.local_sha256
    ));
}

#[test]
fn generation_v4_migration_tombstones_already_resolved_dual_evidence() {
    use rusqlite::Connection;

    let fixture = Fixture::new("generation-v4-resolved-tombstone");
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
    store
        .record_generation_inode_evidence(GenerationInodeEvidenceRecord {
            delta_id: "delta-2".to_string(),
            entry_index: 0,
            mount_id: fixture.mount_id.clone(),
            logical_path: "Roadmap.md".to_string(),
            evidence_name: ".preimage".to_string(),
            captured_sha256: digest('p'),
            captured_byte_length: 11,
            visible_evidence: None,
            base_payload_delta_id: None,
            base_payload_entry_index: None,
            resolved_at: None,
            created_at: "2026-07-31T12:01:00Z".to_string(),
        })
        .unwrap();
    store
        .record_generation_apply_outcome(
            "delta-2",
            0,
            GenerationApplyOutcome::Merged,
            "2026-07-31T12:02:00Z",
        )
        .unwrap();
    store
        .complete_generation_apply("delta-2", "2026-07-31T12:03:00Z")
        .unwrap();
    store
        .mark_generation_inode_evidence_conflict(
            "delta-2",
            0,
            GenerationInodeEvidenceConflictUpdate {
                local_sha256: digest('m'),
                captured_sha256: digest('p'),
                captured_byte_length: 11,
                visible_evidence: Some(GenerationRetainedInodeRecord {
                    evidence_name: ".visible".to_string(),
                    captured_sha256: digest('v'),
                    captured_byte_length: 22,
                }),
                updated_at: "2026-07-31T12:04:00Z".to_string(),
            },
        )
        .unwrap();
    store
        .mark_generation_inode_evidence_resolved(
            "delta-2",
            0,
            GenerationInodeEvidenceResolution {
                captured_sha256: digest('p'),
                captured_byte_length: 11,
                visible_captured_sha256: digest('v'),
                visible_captured_byte_length: 22,
                updated_at: "2026-07-31T12:05:00Z".to_string(),
            },
        )
        .unwrap();
    let db_path = store.db_path.clone();
    drop(store);

    let connection = Connection::open(&db_path).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE generation_inode_evidence DROP COLUMN resolved_at;
             UPDATE state_components
             SET version = 4, min_reader_version = 4
             WHERE component_id = 'durable:generation_delivery';",
        )
        .unwrap();
    drop(connection);

    let reopened = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    let evidence = reopened.list_generation_inode_evidence().unwrap().remove(0);
    assert_eq!(
        evidence.resolved_at.as_deref(),
        Some("2026-07-31T12:05:00Z")
    );
    assert!(evidence.visible_evidence.is_some());
    assert_eq!(
        reopened.list_generation_paths(&fixture.mount_id).unwrap()[0].state,
        GenerationPathState::Dirty
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
         DROP INDEX IF EXISTS generation_apply_one_active_per_source;
         DROP INDEX IF EXISTS generation_apply_one_active_per_mount;
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
    assert_eq!(SqliteStateStore::current_schema_version(), 28);
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
    assert_eq!(component, (7, 7));
    let error = reopened
        .clear_mount_source_state(&fixture.mount_id)
        .expect_err("migrated active journal must fence source reset");
    assert!(matches!(error, StoreError::InvalidState(_)));
}

#[test]
fn current_schema_generation_v3_migrates_dual_inode_evidence_and_tombstone_columns() {
    use rusqlite::Connection;

    let fixture = Fixture::new("generation-v3-to-v7");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    seed(&mut store, &fixture);
    let db_path = store.db_path.clone();
    drop(store);

    let connection = Connection::open(&db_path).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE generation_inode_evidence DROP COLUMN visible_evidence_name;
             ALTER TABLE generation_inode_evidence DROP COLUMN visible_expected_sha256;
             ALTER TABLE generation_inode_evidence DROP COLUMN visible_byte_length;
             ALTER TABLE generation_inode_evidence DROP COLUMN resolved_at;
             UPDATE state_components
             SET version = 3, min_reader_version = 3
             WHERE component_id = 'durable:generation_delivery';",
        )
        .unwrap();
    drop(connection);

    let reopened = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    assert_eq!(
        reopened.list_generation_paths(&fixture.mount_id).unwrap()[0].logical_path,
        "Roadmap.md"
    );
    let connection = Connection::open(&db_path).unwrap();
    for column in [
        "visible_evidence_name",
        "visible_expected_sha256",
        "visible_byte_length",
        "resolved_at",
    ] {
        let present: bool = connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM pragma_table_info('generation_inode_evidence')
                    WHERE name = ?1
                 )",
                [column],
                |row| row.get(0),
            )
            .unwrap();
        assert!(present, "v7 migration did not add {column}");
    }
    let component: (i64, i64) = connection
        .query_row(
            "SELECT version, min_reader_version FROM state_components
             WHERE component_id = 'durable:generation_delivery'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(component, (7, 7));
}

#[test]
fn partial_v2_v7_generation_migration_is_atomic_and_resumable_per_column() {
    use rusqlite::Connection;

    let fixture = Fixture::new("partial-v2-v6-migration");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
    seed(&mut store, &fixture);
    let db_path = store.db_path.clone();
    drop(store);

    let connection = Connection::open(&db_path).unwrap();
    connection
        .execute_batch(concat!(
            include_str!("fixtures/generation-delivery-component-v6.sql"),
            "ALTER TABLE generation_paths DROP COLUMN base_payload_entry_index;
             ALTER TABLE generation_paths DROP COLUMN local_logical_path;
             ALTER TABLE generation_paths DROP COLUMN conflict_payload_entry_index;
             ALTER TABLE generation_inode_evidence DROP COLUMN base_payload_entry_index;
             ALTER TABLE generation_inode_evidence DROP COLUMN visible_evidence_name;
             ALTER TABLE generation_inode_evidence DROP COLUMN visible_expected_sha256;
             ALTER TABLE generation_inode_evidence DROP COLUMN visible_byte_length;
             ALTER TABLE generation_inode_evidence DROP COLUMN resolved_at;
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
             PRAGMA user_version = 23;"
        ))
        .unwrap();
    drop(connection);

    assert!(SqliteStateStore::open(fixture.state_root.clone()).is_err());
    let connection = Connection::open(&db_path).unwrap();
    for (table, column) in [
        ("generation_paths", "base_payload_entry_index"),
        ("generation_paths", "local_logical_path"),
        ("generation_paths", "conflict_payload_entry_index"),
        ("generation_inode_evidence", "base_payload_entry_index"),
        ("generation_inode_evidence", "visible_evidence_name"),
        ("generation_inode_evidence", "visible_expected_sha256"),
        ("generation_inode_evidence", "visible_byte_length"),
        ("generation_inode_evidence", "resolved_at"),
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
        ("generation_inode_evidence", "visible_evidence_name"),
        ("generation_inode_evidence", "visible_expected_sha256"),
        ("generation_inode_evidence", "visible_byte_length"),
        ("generation_inode_evidence", "resolved_at"),
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
    assert_eq!(component, (7, 7));
    assert_eq!(user_version, 28);
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
