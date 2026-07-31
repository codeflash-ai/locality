use locality_core::portable::{
    ContentVersionId, LogicalPath, ProjectionId, SourceConnectionId, SourceGenerationId,
};
use locality_core::workspace_layout::PortableMountId;
use locality_protocol::freshness_delivery::{
    DeliveredTreeHealth, DeliveredTreeState, FRESHNESS_DELIVERY_READER_VERSION,
    FRESHNESS_HEALTH_V1_GOLDEN_JSON, FreshnessDeliveryError, FreshnessHealth, FreshnessReasonCode,
    FreshnessRetry, FreshnessRetryClass, GENERATION_DELTA_FORMAT_VERSION,
    GENERATION_DELTA_PREIMAGE_V1_GOLDEN_JSON, GENERATION_DELTA_RECEIPT_V1_GOLDEN_JSON,
    GENERATION_DELTA_V1_GOLDEN_JSON, GENERATION_TARGET_INVENTORY_V1_VECTORS_JSON, GenerationDelta,
    GenerationDeltaEntry, GenerationDeltaTerminalReceipt, GenerationFileIdentity,
    MAX_GENERATION_DELTA_CONTENT_BYTES, MAX_GENERATION_FILE_BYTES, ProviderHealth,
    ProviderHealthState, ProviderWorkerProgress, PublicationGenerationHealth,
    PublicationGenerationState, canonical_target_inventory_preimage,
    canonical_target_inventory_sha256,
};
use locality_protocol::workspace_layout::LayoutDigest;
use locality_protocol::{FreshnessEpoch, ScopeFreshnessEpochs};
use serde::{Deserialize, Serialize};

fn epoch(value: i64) -> FreshnessEpoch {
    FreshnessEpoch::new(value).expect("non-negative epoch")
}

fn digest(character: char) -> String {
    format!("sha256:{}", character.to_string().repeat(64))
}

fn layout_digest() -> LayoutDigest {
    LayoutDigest::new(digest('a')).expect("layout digest")
}

fn identity(
    projection_id: &str,
    path: &str,
    content_version_id: &str,
    digest_character: char,
    byte_length: u64,
) -> GenerationFileIdentity {
    GenerationFileIdentity {
        projection_id: ProjectionId::new(projection_id),
        logical_path: LogicalPath::new(path).expect("logical path"),
        content_version_id: ContentVersionId::new(content_version_id),
        content_sha256: digest(digest_character),
        byte_length,
    }
}

fn delta() -> GenerationDelta {
    GenerationDelta {
        format_version: GENERATION_DELTA_FORMAT_VERSION,
        minimum_reader_version: FRESHNESS_DELIVERY_READER_VERSION,
        delta_id: "delta-018f4f6e".to_string(),
        mount_id: PortableMountId::new("mount-alpha").unwrap(),
        source_connection_id: SourceConnectionId::new("source-018f4f6e"),
        base_generation_id: SourceGenerationId::new("generation-0007").unwrap(),
        target_generation_id: SourceGenerationId::new("generation-0008").unwrap(),
        target_complete: true,
        target_inventory_sha256: digest('b'),
        workspace_layout_version: 1,
        workspace_layout_digest: layout_digest(),
        entries: vec![
            GenerationDeltaEntry {
                old: None,
                new: Some(identity(
                    "projection-1",
                    "Engineering/new.md",
                    "content-3",
                    '3',
                    3,
                )),
            },
            GenerationDeltaEntry {
                old: Some(identity(
                    "projection-2",
                    "Engineering/roadmap.md",
                    "content-1",
                    '1',
                    11,
                )),
                new: Some(identity(
                    "projection-2",
                    "Engineering/roadmap.md",
                    "content-2",
                    '2',
                    12,
                )),
            },
            GenerationDeltaEntry {
                old: Some(identity(
                    "projection-9",
                    "Sales/old.md",
                    "content-9",
                    '9',
                    9,
                )),
                new: None,
            },
        ],
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
        delta_sha256: delta.canonical_sha256().expect("delta digest"),
        entry_count: delta.entries.len() as u64,
        changed_content_bytes: delta.changed_content_bytes().expect("content bytes"),
        authorization_epoch: epoch(42),
        completed_at: "2026-07-31T12:34:56Z".to_string(),
    }
}

fn health() -> FreshnessHealth {
    FreshnessHealth {
        provider: ProviderHealth {
            source_connection_id: SourceConnectionId::new("source-018f4f6e"),
            state: ProviderHealthState::Degraded,
            reason: Some(FreshnessReasonCode::ProviderCooldown),
            retry: Some(FreshnessRetry {
                class: FreshnessRetryClass::AfterDelay,
                retry_after_seconds: Some(30),
            }),
            epochs: ScopeFreshnessEpochs {
                demand_epoch: epoch(12),
                received_epoch: epoch(12),
                processed_epoch: epoch(11),
                applied_epoch: epoch(11),
            },
            worker_progress: ProviderWorkerProgress::Fetching,
            latest_observation_at: Some("2026-07-31T12:30:00Z".to_string()),
            provider_cooldown_until: Some("2026-07-31T12:35:00Z".to_string()),
        },
        publication: PublicationGenerationHealth {
            source_connection_id: SourceConnectionId::new("source-018f4f6e"),
            generation_id: SourceGenerationId::new("generation-0008").unwrap(),
            state: PublicationGenerationState::Complete,
            verified: true,
            retained: true,
            selectable: true,
            applied_receipt_sha256: Some(digest('c')),
            reason: None,
            retry: None,
        },
        local_delivery: DeliveredTreeHealth {
            mount_id: PortableMountId::new("mount-alpha").unwrap(),
            state: DeliveredTreeState::Conflicted,
            observed_generation_id: Some(SourceGenerationId::new("generation-0008").unwrap()),
            available_generation_id: Some(SourceGenerationId::new("generation-0008").unwrap()),
            clean_path_count: 8,
            dirty_path_count: 2,
            pending_path_count: 1,
            conflicted_path_count: 1,
            last_delta_receipt_sha256: Some(digest('d')),
            reason: Some(FreshnessReasonCode::MergeConflict),
            retry: Some(FreshnessRetry {
                class: FreshnessRetryClass::AfterUserAction,
                retry_after_seconds: None,
            }),
        },
    }
}

fn pretty_json(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("serialize JSON");
    bytes.push(b'\n');
    bytes
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("write hex");
    }
    output
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PreimageGolden {
    preimage_hex: String,
    delta_sha256: String,
    terminal_receipt_sha256: String,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
struct TargetInventoryVector {
    name: String,
    inventory: Vec<GenerationFileIdentity>,
    preimage_hex: String,
    sha256: String,
}

#[test]
fn canonical_delivery_values_match_exact_lf_goldens() {
    let health = health();
    health.validate().expect("health");
    let delta = delta();
    delta.validate().expect("delta");
    let receipt = receipt(&delta);
    receipt.validate_against(&delta).expect("receipt");

    let decoded_health: FreshnessHealth =
        serde_json::from_slice(FRESHNESS_HEALTH_V1_GOLDEN_JSON).expect("health golden");
    let decoded_delta: GenerationDelta =
        serde_json::from_slice(GENERATION_DELTA_V1_GOLDEN_JSON).expect("delta golden");
    let decoded_receipt: GenerationDeltaTerminalReceipt =
        serde_json::from_slice(GENERATION_DELTA_RECEIPT_V1_GOLDEN_JSON).expect("receipt golden");
    assert_eq!(decoded_health, health);
    assert_eq!(decoded_delta, delta);
    assert_eq!(decoded_receipt, receipt);
    assert_eq!(
        pretty_json(&decoded_health),
        FRESHNESS_HEALTH_V1_GOLDEN_JSON
    );
    assert_eq!(pretty_json(&decoded_delta), GENERATION_DELTA_V1_GOLDEN_JSON);
    assert_eq!(
        pretty_json(&decoded_receipt),
        GENERATION_DELTA_RECEIPT_V1_GOLDEN_JSON
    );

    let expected_preimage = PreimageGolden {
        preimage_hex: hex(&delta.canonical_preimage().unwrap()),
        delta_sha256: delta.canonical_sha256().unwrap(),
        terminal_receipt_sha256: receipt.canonical_sha256().unwrap(),
    };
    let decoded_preimage: PreimageGolden =
        serde_json::from_slice(GENERATION_DELTA_PREIMAGE_V1_GOLDEN_JSON).expect("preimage golden");
    assert_eq!(decoded_preimage, expected_preimage);
    assert_eq!(
        pretty_json(&decoded_preimage),
        GENERATION_DELTA_PREIMAGE_V1_GOLDEN_JSON
    );
}

#[test]
fn canonical_target_inventory_matches_cross_implementation_vectors() {
    let vectors: Vec<TargetInventoryVector> =
        serde_json::from_slice(GENERATION_TARGET_INVENTORY_V1_VECTORS_JSON)
            .expect("target inventory vectors");
    assert_eq!(
        vectors
            .iter()
            .map(|vector| vector.name.as_str())
            .collect::<Vec<_>>(),
        ["empty", "single_ascii", "multi_unicode_and_boundaries"]
    );
    for vector in vectors {
        let preimage = canonical_target_inventory_preimage(&vector.inventory)
            .unwrap_or_else(|error| panic!("{} preimage failed: {error}", vector.name));
        assert_eq!(
            hex(&preimage),
            vector.preimage_hex,
            "{} preimage",
            vector.name
        );
        assert_eq!(
            canonical_target_inventory_sha256(&vector.inventory).unwrap(),
            vector.sha256,
            "{} digest",
            vector.name
        );
    }
}

#[test]
fn target_inventory_digest_rejects_reorder_collision_and_substitution() {
    let mut inventory = vec![
        identity("projection-a", "Docs/A.md", "content-a", '1', 1),
        identity("projection-b", "Docs/B.md", "content-b", '2', 2),
    ];
    let mut candidate = delta();
    candidate.target_inventory_sha256 = canonical_target_inventory_sha256(&inventory).unwrap();
    candidate
        .validate_target_inventory(&inventory)
        .expect("exact target inventory");

    inventory.swap(0, 1);
    assert_eq!(
        canonical_target_inventory_sha256(&inventory),
        Err(FreshnessDeliveryError::NonCanonicalTargetInventoryOrder)
    );

    inventory.swap(0, 1);
    inventory[1].logical_path = LogicalPath::new("docs/a.MD").unwrap();
    assert_eq!(
        canonical_target_inventory_sha256(&inventory),
        Err(FreshnessDeliveryError::TargetInventoryPathReuse)
    );

    inventory[1].logical_path = LogicalPath::new("Docs/B.md").unwrap();
    inventory[1].content_version_id = ContentVersionId::new("substituted");
    assert_eq!(
        candidate.validate_target_inventory(&inventory),
        Err(FreshnessDeliveryError::TargetInventoryMismatch)
    );
}

#[test]
fn newer_required_reader_fails_update_required_after_tolerant_decode() {
    let mut value = serde_json::to_value(delta()).unwrap();
    value["format_version"] = serde_json::json!(2);
    value["minimum_reader_version"] = serde_json::json!(2);
    value["future_additive_field"] = serde_json::json!({"retained": true});

    let decoded: GenerationDelta = serde_json::from_value(value).expect("tolerant decode");
    assert_eq!(
        decoded.validate(),
        Err(FreshnessDeliveryError::UpdateRequired {
            minimum: 2,
            supported: 1,
        })
    );
}

#[test]
fn unknown_taxonomy_decodes_but_fails_closed_when_used() {
    let reason: FreshnessReasonCode = serde_json::from_str(r#""future_reason""#).unwrap();
    let retry: FreshnessRetryClass = serde_json::from_str(r#""future_retry""#).unwrap();
    assert_eq!(reason, FreshnessReasonCode::Unknown);
    assert_eq!(retry, FreshnessRetryClass::Unknown);

    let mut health = health();
    health.provider.reason = Some(reason);
    assert_eq!(
        health.validate(),
        Err(FreshnessDeliveryError::UnknownReasonCode)
    );
}

#[test]
fn delta_validation_rejects_reorder_path_reuse_incomplete_and_receipt_substitution() {
    let mut reordered = delta();
    reordered.entries.swap(0, 1);
    assert_eq!(
        reordered.validate(),
        Err(FreshnessDeliveryError::NonCanonicalDeltaOrder)
    );

    let mut collision = delta();
    collision.entries[1].new.as_mut().unwrap().logical_path =
        LogicalPath::new("Engineering/new.md").unwrap();
    assert_eq!(
        collision.validate(),
        Err(FreshnessDeliveryError::CrossEntryPathReuse)
    );

    let mut incomplete = delta();
    incomplete.target_complete = false;
    assert_eq!(
        incomplete.validate(),
        Err(FreshnessDeliveryError::IncompleteTargetGeneration)
    );

    let delta = delta();
    let mut substituted = receipt(&delta);
    substituted.entry_count += 1;
    assert_eq!(
        substituted.validate_against(&delta),
        Err(FreshnessDeliveryError::ReceiptMismatch)
    );
}

#[test]
fn delete_create_path_reuse_is_rejected_in_both_canonical_orders() {
    for (deleted_projection, created_projection) in [
        ("projection-1", "projection-2"),
        ("projection-2", "projection-1"),
    ] {
        let mut delta = delta();
        delta.entries = vec![
            GenerationDeltaEntry {
                old: Some(identity(
                    deleted_projection,
                    "Shared.md",
                    "content-old",
                    '1',
                    1,
                )),
                new: None,
            },
            GenerationDeltaEntry {
                old: None,
                new: Some(identity(
                    created_projection,
                    "Shared.md",
                    "content-new",
                    '2',
                    1,
                )),
            },
        ];
        delta.entries.sort_by(|left, right| {
            left.projection_id()
                .unwrap()
                .as_str()
                .cmp(right.projection_id().unwrap().as_str())
        });
        assert_eq!(
            delta.validate(),
            Err(FreshnessDeliveryError::CrossEntryPathReuse)
        );
    }
}

#[test]
fn portable_casefold_and_unicode_path_collisions_are_rejected() {
    for (first, second) in [
        ("Docs/Roadmap.md", "docs/roadmap.MD"),
        ("Docs/é.md", "docs/É.md"),
        ("Docs/ß.md", "docs/ss.md"),
    ] {
        let mut candidate = delta();
        candidate.entries = vec![
            GenerationDeltaEntry {
                old: None,
                new: Some(identity("projection-a", first, "content-a", '1', 1)),
            },
            GenerationDeltaEntry {
                old: None,
                new: Some(identity("projection-b", second, "content-b", '2', 1)),
            },
        ];
        assert_eq!(
            candidate.validate(),
            Err(FreshnessDeliveryError::CrossEntryPathReuse),
            "portable collision should reject {first:?} and {second:?}"
        );
    }
}

#[test]
fn empty_delta_advances_generation_and_content_limits_are_bounded() {
    let mut empty = delta();
    empty.entries.clear();
    empty.validate().expect("empty generation advancement");
    assert_eq!(empty.changed_content_bytes().unwrap(), 0);
    receipt(&empty)
        .validate_against(&empty)
        .expect("empty terminal receipt");

    let mut oversized_file = delta();
    oversized_file.entries.truncate(1);
    oversized_file.entries[0].new.as_mut().unwrap().byte_length = MAX_GENERATION_FILE_BYTES + 1;
    assert_eq!(
        oversized_file.validate(),
        Err(FreshnessDeliveryError::FileContentTooLarge {
            actual: MAX_GENERATION_FILE_BYTES + 1,
        })
    );

    let mut oversized_delta = delta();
    oversized_delta.entries = (0..=MAX_GENERATION_DELTA_CONTENT_BYTES / MAX_GENERATION_FILE_BYTES)
        .map(|index| GenerationDeltaEntry {
            old: None,
            new: Some(identity(
                &format!("projection-{index:03}"),
                &format!("file-{index:03}.md"),
                &format!("content-{index:03}"),
                '3',
                MAX_GENERATION_FILE_BYTES,
            )),
        })
        .collect();
    assert!(matches!(
        oversized_delta.validate(),
        Err(FreshnessDeliveryError::DeltaContentTooLarge { .. })
    ));
}
