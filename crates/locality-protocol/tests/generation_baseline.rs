use locality_core::portable::{
    ContentVersionId, LogicalPath, ProjectionId, SourceConnectionId, SourceGenerationId,
};
use locality_core::workspace_layout::PortableMountId;
use locality_protocol::OrderedSourceGeneration;
use locality_protocol::freshness_delivery::{
    FreshnessDeliveryError, GenerationFileIdentity, MAX_GENERATION_FILE_BYTES,
};
use locality_protocol::generation_baseline::{
    GENERATION_BASELINE_PREIMAGE_V1_GOLDEN_JSON, GENERATION_BASELINE_V1_GOLDEN_JSON,
    GenerationBaselineError, GenerationBaselineMountV1, GenerationBaselineResponseV1,
    MAX_GENERATION_BASELINE_CONTENT_BYTES, MAX_GENERATION_BASELINE_ENCODED_BYTES,
    MAX_GENERATION_BASELINE_MOUNTS,
};
use locality_protocol::workspace_api_v2::{
    WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON, WorkspaceExportOfferV2,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

fn offer() -> WorkspaceExportOfferV2 {
    serde_json::from_slice(WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON).expect("workspace offer")
}

fn file(
    projection_id: &str,
    logical_path: &str,
    content_version_id: &str,
    digest_byte: char,
    byte_length: u64,
) -> GenerationFileIdentity {
    GenerationFileIdentity {
        projection_id: ProjectionId::new(projection_id),
        logical_path: LogicalPath::new(logical_path).expect("portable logical path"),
        content_version_id: ContentVersionId::new(content_version_id),
        content_sha256: format!("sha256:{}", digest_byte.to_string().repeat(64)),
        byte_length,
    }
}

fn files() -> Vec<GenerationFileIdentity> {
    vec![
        file(
            "projection-readme",
            "README.md",
            "content-readme-v1",
            '6',
            7,
        ),
        file(
            "projection-roadmap",
            "Projects/Roadmap/page.md",
            "content-roadmap-v4",
            '5',
            10,
        ),
    ]
}

fn mounts() -> Vec<GenerationBaselineMountV1> {
    vec![
        GenerationBaselineMountV1::new(
            PortableMountId::new("mount-alpha").unwrap(),
            SourceConnectionId::new("source-drive"),
            SourceGenerationId::new("generation-drive-44").unwrap(),
            vec![],
        )
        .unwrap(),
        GenerationBaselineMountV1::new(
            PortableMountId::new("mount-zeta").unwrap(),
            SourceConnectionId::new("source-notion"),
            SourceGenerationId::new("generation-notion-109").unwrap(),
            files(),
        )
        .unwrap(),
    ]
}

fn baseline_with(
    source_generations: Vec<OrderedSourceGeneration>,
    mounts: Vec<GenerationBaselineMountV1>,
) -> Result<GenerationBaselineResponseV1, GenerationBaselineError> {
    let offer = offer();
    let sealed = offer.offer();
    GenerationBaselineResponseV1::new(
        offer.profile_id().clone(),
        offer.profile_revision(),
        sealed.session_id.clone(),
        sealed.export_attempt_id.clone(),
        offer.layout_version(),
        offer.layout_digest().clone(),
        sealed.inventory_sha256.clone(),
        source_generations,
        mounts,
    )
}

fn baseline() -> GenerationBaselineResponseV1 {
    GenerationBaselineResponseV1::from_export_offer(&offer(), mounts()).expect("valid baseline")
}

fn exact_pretty_json(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("serialize fixture");
    bytes.push(b'\n');
    bytes
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PreimageGolden {
    preimage_hex: String,
    sha256: String,
}

#[test]
fn generation_baseline_golden_is_exact_and_bound_to_export_attempt() {
    let baseline = baseline();
    assert_eq!(
        exact_pretty_json(&baseline),
        GENERATION_BASELINE_V1_GOLDEN_JSON
    );

    let decoded = GenerationBaselineResponseV1::decode_json_against_export_offer(
        GENERATION_BASELINE_V1_GOLDEN_JSON,
        &offer(),
    )
    .expect("strict golden decode");
    assert_eq!(decoded, baseline);
    assert_eq!(decoded.mounts()[0].files().len(), 0);
    assert_eq!(decoded.mounts()[1].files().len(), 2);

    let expected: PreimageGolden =
        serde_json::from_slice(GENERATION_BASELINE_PREIMAGE_V1_GOLDEN_JSON)
            .expect("preimage golden");
    let actual = PreimageGolden {
        preimage_hex: baseline
            .canonical_preimage()
            .unwrap()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        sha256: baseline.recompute_baseline_sha256().unwrap(),
    };
    assert_eq!(actual, expected);
    assert_eq!(actual.sha256, baseline.baseline_sha256());
}

#[test]
fn canonical_order_and_uniqueness_are_enforced() {
    let source_generations = offer().offer().source_generations.clone();

    let mut reordered_sources = source_generations.clone();
    reordered_sources.reverse();
    assert!(matches!(
        baseline_with(reordered_sources, mounts()),
        Err(GenerationBaselineError::NonCanonicalSourceGenerationOrder { .. })
    ));

    let mut reordered_and_renumbered = source_generations.clone();
    reordered_and_renumbered.reverse();
    for (ordinal, generation) in reordered_and_renumbered.iter_mut().enumerate() {
        generation.ordinal = ordinal as u32;
    }
    let crossed_order = baseline_with(reordered_and_renumbered, mounts()).unwrap();
    assert_eq!(
        crossed_order.validate_against_export_offer(&offer()),
        Err(GenerationBaselineError::ExportBindingMismatch)
    );

    let mut duplicate_source = source_generations.clone();
    duplicate_source[1].source_connection_id = duplicate_source[0].source_connection_id.clone();
    assert_eq!(
        baseline_with(duplicate_source, mounts()),
        Err(GenerationBaselineError::DuplicateSourceConnection)
    );

    let mut duplicate_generation = source_generations.clone();
    duplicate_generation[1].source_generation_id =
        duplicate_generation[0].source_generation_id.clone();
    assert_eq!(
        baseline_with(duplicate_generation, mounts()),
        Err(GenerationBaselineError::DuplicateSourceGeneration)
    );

    let mut reordered_mounts = mounts();
    reordered_mounts.reverse();
    assert!(matches!(
        baseline_with(source_generations.clone(), reordered_mounts),
        Err(GenerationBaselineError::NonCanonicalMountOrder { .. })
    ));

    let duplicate_mount = mounts()[0].clone();
    assert!(matches!(
        baseline_with(
            source_generations,
            vec![duplicate_mount.clone(), duplicate_mount]
        ),
        Err(GenerationBaselineError::NonCanonicalMountOrder { .. })
    ));

    let mut reordered_files = files();
    reordered_files.reverse();
    assert!(matches!(
        GenerationBaselineMountV1::new(
            PortableMountId::new("mount-zeta").unwrap(),
            SourceConnectionId::new("source-notion"),
            SourceGenerationId::new("generation-notion-109").unwrap(),
            reordered_files,
        ),
        Err(GenerationBaselineError::TargetInventory(
            FreshnessDeliveryError::NonCanonicalTargetInventoryOrder
        ))
    ));

    let duplicate_file = files()[0].clone();
    assert!(
        GenerationBaselineMountV1::new(
            PortableMountId::new("mount-zeta").unwrap(),
            SourceConnectionId::new("source-notion"),
            SourceGenerationId::new("generation-notion-109").unwrap(),
            vec![duplicate_file.clone(), duplicate_file],
        )
        .is_err()
    );

    let duplicate_projection = files()[0].clone();
    let duplicate_projection_mounts = vec![
        GenerationBaselineMountV1::new(
            PortableMountId::new("mount-alpha").unwrap(),
            SourceConnectionId::new("source-drive"),
            SourceGenerationId::new("generation-drive-44").unwrap(),
            vec![duplicate_projection.clone()],
        )
        .unwrap(),
        GenerationBaselineMountV1::new(
            PortableMountId::new("mount-zeta").unwrap(),
            SourceConnectionId::new("source-notion"),
            SourceGenerationId::new("generation-notion-109").unwrap(),
            vec![duplicate_projection],
        )
        .unwrap(),
    ];
    assert!(matches!(
        baseline_with(
            offer().offer().source_generations.clone(),
            duplicate_projection_mounts
        ),
        Err(GenerationBaselineError::DuplicateProjectionId { .. })
    ));
}

#[test]
fn crossed_mount_source_and_generation_are_rejected() {
    let source_generations = offer().offer().source_generations.clone();
    let crossed_generation = GenerationBaselineMountV1::new(
        PortableMountId::new("mount-alpha").unwrap(),
        SourceConnectionId::new("source-drive"),
        SourceGenerationId::new("generation-notion-109").unwrap(),
        vec![],
    )
    .unwrap();
    assert!(matches!(
        baseline_with(
            source_generations.clone(),
            vec![crossed_generation, mounts()[1].clone()]
        ),
        Err(GenerationBaselineError::MountGenerationMismatch { .. })
    ));

    let crossed_source = GenerationBaselineMountV1::new(
        PortableMountId::new("mount-alpha").unwrap(),
        SourceConnectionId::new("source-notion"),
        SourceGenerationId::new("generation-drive-44").unwrap(),
        vec![],
    )
    .unwrap();
    assert!(matches!(
        baseline_with(
            source_generations.clone(),
            vec![crossed_source, mounts()[1].clone()]
        ),
        Err(GenerationBaselineError::MountGenerationMismatch { .. })
    ));

    let unknown_source = GenerationBaselineMountV1::new(
        PortableMountId::new("mount-alpha").unwrap(),
        SourceConnectionId::new("source-unknown"),
        SourceGenerationId::new("generation-unknown").unwrap(),
        vec![],
    )
    .unwrap();
    assert!(matches!(
        baseline_with(
            source_generations,
            vec![unknown_source, mounts()[1].clone()]
        ),
        Err(GenerationBaselineError::MountSourceNotInGenerationVector { .. })
    ));
}

#[test]
fn path_digest_uuid_and_wire_types_fail_closed() {
    assert!(LogicalPath::new("/absolute/page.md").is_err());
    assert!(LogicalPath::new("../escape.md").is_err());
    assert!(LogicalPath::new(".loc/session.json").is_err());

    let invalid_digest_file = GenerationFileIdentity {
        content_sha256: format!("sha256:{}", "A".repeat(64)),
        ..files()[0].clone()
    };
    assert!(matches!(
        GenerationBaselineMountV1::new(
            PortableMountId::new("mount-zeta").unwrap(),
            SourceConnectionId::new("source-notion"),
            SourceGenerationId::new("generation-notion-109").unwrap(),
            vec![invalid_digest_file],
        ),
        Err(GenerationBaselineError::TargetInventory(
            FreshnessDeliveryError::InvalidSha256
        ))
    ));

    let mut value: Value = serde_json::from_slice(GENERATION_BASELINE_V1_GOLDEN_JSON).unwrap();
    value["profile_id"] = json!("NOT-A-UUID");
    assert!(
        GenerationBaselineResponseV1::decode_json(&serde_json::to_vec(&value).unwrap()).is_err()
    );

    let mut value: Value = serde_json::from_slice(GENERATION_BASELINE_V1_GOLDEN_JSON).unwrap();
    value["mounts"][1]["files"][0]["logical_path"] = json!("/tmp/credential.txt");
    assert!(
        GenerationBaselineResponseV1::decode_json(&serde_json::to_vec(&value).unwrap()).is_err()
    );

    let mut value: Value = serde_json::from_slice(GENERATION_BASELINE_V1_GOLDEN_JSON).unwrap();
    value["mounts"][1]["files"][0]["byte_length"] = json!("7");
    assert!(
        GenerationBaselineResponseV1::decode_json(&serde_json::to_vec(&value).unwrap()).is_err()
    );

    let mut value: Value = serde_json::from_slice(GENERATION_BASELINE_V1_GOLDEN_JSON).unwrap();
    value["baseline_sha256"] = json!(format!("sha256:{}", "0".repeat(64)));
    assert!(
        GenerationBaselineResponseV1::decode_json(&serde_json::to_vec(&value).unwrap()).is_err()
    );

    let mut value: Value = serde_json::from_slice(GENERATION_BASELINE_V1_GOLDEN_JSON).unwrap();
    value["inventory_sha256"] = json!(format!("sha256:{}", "A".repeat(64)));
    assert!(
        GenerationBaselineResponseV1::decode_json(&serde_json::to_vec(&value).unwrap()).is_err()
    );

    let mut value: Value = serde_json::from_slice(GENERATION_BASELINE_V1_GOLDEN_JSON).unwrap();
    value["mounts"][1]["target_inventory_sha256"] = json!(format!("sha256:{}", "0".repeat(64)));
    assert!(
        GenerationBaselineResponseV1::decode_json(&serde_json::to_vec(&value).unwrap()).is_err()
    );
}

#[test]
fn unknown_fields_are_rejected_at_every_wire_level() {
    for pointer in [
        "",
        "/source_generations/0",
        "/mounts/0",
        "/mounts/1/files/0",
    ] {
        let mut value: Value = serde_json::from_slice(GENERATION_BASELINE_V1_GOLDEN_JSON).unwrap();
        value
            .pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("credential".to_string(), json!("must-not-be-accepted"));
        assert!(
            GenerationBaselineResponseV1::decode_json(&serde_json::to_vec(&value).unwrap())
                .is_err(),
            "unknown field accepted at {pointer}"
        );
    }
}

#[test]
fn metadata_mount_identifier_and_content_bounds_are_enforced() {
    let oversized = vec![b' '; MAX_GENERATION_BASELINE_ENCODED_BYTES + 1];
    assert!(matches!(
        GenerationBaselineResponseV1::decode_json(&oversized),
        Err(GenerationBaselineError::EncodingTooLarge { .. })
    ));

    let mut source_generations = offer().offer().source_generations.clone();
    source_generations[0].source_connection_id = SourceConnectionId::new(
        "s".repeat(locality_protocol::freshness_delivery::MAX_DELIVERY_ID_BYTES + 1),
    );
    assert_eq!(
        baseline_with(source_generations, mounts()),
        Err(GenerationBaselineError::IdentifierTooLong(
            "source_connection_id"
        ))
    );

    let offer_generations = offer().offer().source_generations.clone();
    let too_many_mounts = (0..=MAX_GENERATION_BASELINE_MOUNTS)
        .map(|index| {
            GenerationBaselineMountV1::new(
                PortableMountId::new(format!("mount-{index:03}")).unwrap(),
                SourceConnectionId::new("source-drive"),
                SourceGenerationId::new("generation-drive-44").unwrap(),
                vec![],
            )
            .unwrap()
        })
        .collect();
    assert!(matches!(
        baseline_with(offer_generations, too_many_mounts),
        Err(GenerationBaselineError::MountCount { .. })
    ));

    let oversized_files = (0..=MAX_GENERATION_BASELINE_CONTENT_BYTES / MAX_GENERATION_FILE_BYTES)
        .map(|index| {
            file(
                &format!("projection-{index:03}"),
                &format!("file-{index:03}.bin"),
                &format!("content-{index:03}"),
                '7',
                MAX_GENERATION_FILE_BYTES,
            )
        })
        .collect();
    let mount = GenerationBaselineMountV1::new(
        PortableMountId::new("mount-alpha").unwrap(),
        SourceConnectionId::new("source-drive"),
        SourceGenerationId::new("generation-drive-44").unwrap(),
        oversized_files,
    )
    .unwrap();
    assert!(matches!(
        baseline_with(
            vec![offer().offer().source_generations[0].clone()],
            vec![mount]
        ),
        Err(GenerationBaselineError::ContentBytesTooLarge { .. })
    ));
}
