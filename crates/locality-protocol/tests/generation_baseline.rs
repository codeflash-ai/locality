use std::collections::BTreeSet;

use locality_core::portable::{
    ContentVersionId, LogicalPath, ProjectionFileKind, ProjectionId, SessionId, SourceAction,
    SourceConnectionId, SourceGenerationId, SourceScopeId,
};
use locality_core::workspace_layout::{MountTarget, PortableMountId};
use locality_protocol::freshness_delivery::{GenerationFileIdentity, MAX_GENERATION_FILE_BYTES};
use locality_protocol::generation_baseline::{
    GENERATION_BASELINE_PREIMAGE_V1_GOLDEN_JSON, GENERATION_BASELINE_V1_GOLDEN_JSON,
    GenerationBaselineError, GenerationBaselineMountV1, GenerationBaselineRefreshModeV1,
    GenerationBaselineResponseV1, GenerationBaselineSourceV1,
    MAX_GENERATION_BASELINE_CONTENT_VERSION_ID_BYTES, maximum_encoded_bytes_for_export,
};
use locality_protocol::workspace_api_v2::{
    WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON, WORKSPACE_PROFILE_SESSION_V2_GOLDEN_JSON,
    WorkspaceClientCapabilitiesV2, WorkspaceExportOfferV2, WorkspaceProfileSessionV2,
    WorkspaceSessionStatusV2,
};
use locality_protocol::workspace_export_v2::{
    WORKSPACE_EXPORT_INVENTORY_V2_GOLDEN_JSON, WorkspaceAuthorizedExportEntryV2,
    WorkspaceNamespacedInventoryV2, WorkspaceScopeSourceAuthorityV2,
};
use locality_protocol::workspace_layout::{
    ProfileMount, ProfileScopeBinding, SessionLayout, WorkspaceLayout, WorkspaceProfileId,
};
use locality_protocol::{
    ExportAttemptLimits, OrderedSourceGeneration, ReplicaFreshnessState, ReplicaFreshnessStatus,
    SCOPE_AUTHORIZED_COMPONENT_VERSIONS, SandboxSessionState, SealedExportOffer,
    StaleSessionBehavior, TarContentEncoding,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

struct ExportContext {
    session: WorkspaceProfileSessionV2,
    offer: WorkspaceExportOfferV2,
    inventory: WorkspaceNamespacedInventoryV2,
}

fn session() -> WorkspaceProfileSessionV2 {
    serde_json::from_slice(WORKSPACE_PROFILE_SESSION_V2_GOLDEN_JSON).expect("workspace session")
}

fn offer() -> WorkspaceExportOfferV2 {
    serde_json::from_slice(WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON).expect("workspace offer")
}

fn inventory() -> WorkspaceNamespacedInventoryV2 {
    WorkspaceNamespacedInventoryV2::decode_json(
        WORKSPACE_EXPORT_INVENTORY_V2_GOLDEN_JSON,
        session().session_layout(),
        &offer(),
    )
    .expect("verified canonical inventory")
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

fn notion_files() -> Vec<GenerationFileIdentity> {
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

fn source_state(
    source_connection_id: &str,
    generation_id: &str,
    files: Vec<GenerationFileIdentity>,
) -> GenerationBaselineSourceV1 {
    GenerationBaselineSourceV1::new(
        SourceConnectionId::new(source_connection_id),
        SourceGenerationId::new(generation_id).unwrap(),
        files,
    )
    .unwrap()
}

fn mounts() -> Vec<GenerationBaselineMountV1> {
    vec![
        GenerationBaselineMountV1::new(
            PortableMountId::new("mount-alpha").unwrap(),
            vec![source_state("source-drive", "generation-drive-44", vec![])],
        )
        .unwrap(),
        GenerationBaselineMountV1::new(
            PortableMountId::new("mount-zeta").unwrap(),
            vec![source_state(
                "source-notion",
                "generation-notion-109",
                notion_files(),
            )],
        )
        .unwrap(),
    ]
}

fn baseline_with(
    source_generations: Vec<OrderedSourceGeneration>,
    mounts: Vec<GenerationBaselineMountV1>,
) -> Result<GenerationBaselineResponseV1, GenerationBaselineError> {
    let session = session();
    let offer = offer();
    GenerationBaselineResponseV1::new(
        session.profile_id().clone(),
        session.profile_revision(),
        offer.offer().session_id.clone(),
        offer.offer().export_attempt_id.clone(),
        offer.layout_version(),
        offer.layout_digest().clone(),
        offer.offer().inventory_sha256.clone(),
        source_generations,
        mounts,
    )
}

fn baseline() -> GenerationBaselineResponseV1 {
    GenerationBaselineResponseV1::from_export(&session(), &offer(), &inventory(), mounts())
        .expect("valid baseline")
}

fn decode_value(value: &Value) -> Result<GenerationBaselineResponseV1, GenerationBaselineError> {
    GenerationBaselineResponseV1::decode_json_against_export(
        &serde_json::to_vec(value).unwrap(),
        &session(),
        &offer(),
        &inventory(),
    )
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
fn generation_baseline_golden_is_exact_and_bound_to_verified_export() {
    let baseline = baseline();
    assert_eq!(
        exact_pretty_json(&baseline),
        GENERATION_BASELINE_V1_GOLDEN_JSON
    );

    let decoded = GenerationBaselineResponseV1::decode_json_against_export(
        GENERATION_BASELINE_V1_GOLDEN_JSON,
        &session(),
        &offer(),
        &inventory(),
    )
    .expect("strict golden decode");
    assert_eq!(decoded, baseline);
    assert_eq!(decoded.mounts()[0].sources()[0].files().len(), 0);
    assert_eq!(decoded.mounts()[1].sources()[0].files().len(), 2);

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
fn every_baseline_file_must_equal_authoritative_export_inventory() {
    let context_session = session();
    let context_offer = offer();
    let context_inventory = inventory();

    for mismatched_file in [
        file(
            "projection-readme",
            "Changed.md",
            "content-readme-v1",
            '6',
            7,
        ),
        file(
            "projection-readme",
            "README.md",
            "content-readme-v1",
            '7',
            7,
        ),
        file(
            "projection-readme",
            "README.md",
            "content-readme-v1",
            '6',
            8,
        ),
    ] {
        let candidate_mounts = vec![
            mounts()[0].clone(),
            GenerationBaselineMountV1::new(
                PortableMountId::new("mount-zeta").unwrap(),
                vec![source_state(
                    "source-notion",
                    "generation-notion-109",
                    vec![mismatched_file, notion_files()[1].clone()],
                )],
            )
            .unwrap(),
        ];
        assert!(matches!(
            GenerationBaselineResponseV1::from_export(
                &context_session,
                &context_offer,
                &context_inventory,
                candidate_mounts,
            ),
            Err(GenerationBaselineError::InventoryFileMismatch { .. })
        ));
    }

    let missing_file_mounts = vec![
        mounts()[0].clone(),
        GenerationBaselineMountV1::new(
            PortableMountId::new("mount-zeta").unwrap(),
            vec![source_state(
                "source-notion",
                "generation-notion-109",
                vec![notion_files()[0].clone()],
            )],
        )
        .unwrap(),
    ];
    assert_eq!(
        GenerationBaselineResponseV1::from_export(
            &context_session,
            &context_offer,
            &context_inventory,
            missing_file_mounts,
        ),
        Err(GenerationBaselineError::InventoryFilesMissing)
    );
}

#[test]
fn authoritative_content_version_ids_are_cryptographically_bound() {
    let first = baseline();
    let mut alternate_files = notion_files();
    alternate_files[0].content_version_id = ContentVersionId::new("content-readme-v2");
    let alternate_mounts = vec![
        mounts()[0].clone(),
        GenerationBaselineMountV1::new(
            PortableMountId::new("mount-zeta").unwrap(),
            vec![source_state(
                "source-notion",
                "generation-notion-109",
                alternate_files,
            )],
        )
        .unwrap(),
    ];
    let second = GenerationBaselineResponseV1::from_export(
        &session(),
        &offer(),
        &inventory(),
        alternate_mounts,
    )
    .expect("authenticated endpoint may authoritatively select the content version ID");
    assert_ne!(
        first.mounts()[1].sources()[0].target_inventory_sha256(),
        second.mounts()[1].sources()[0].target_inventory_sha256()
    );
    assert_ne!(first.baseline_sha256(), second.baseline_sha256());

    let mut tampered: Value = serde_json::from_slice(GENERATION_BASELINE_V1_GOLDEN_JSON).unwrap();
    tampered["mounts"][1]["sources"][0]["files"][0]["content_version_id"] =
        json!("content-readme-v2");
    assert!(decode_value(&tampered).is_err());
}

#[test]
fn canonical_order_uniqueness_and_cross_field_consistency_are_enforced() {
    let generations = offer().offer().source_generations.clone();
    let mut reordered_generations = generations.clone();
    reordered_generations.reverse();
    assert!(matches!(
        baseline_with(reordered_generations, mounts()),
        Err(GenerationBaselineError::NonCanonicalSourceGenerationOrder { .. })
    ));

    let mut reordered_mounts = mounts();
    reordered_mounts.reverse();
    assert!(matches!(
        baseline_with(generations.clone(), reordered_mounts),
        Err(GenerationBaselineError::NonCanonicalMountOrder { .. })
    ));

    let crossed = GenerationBaselineMountV1::new(
        PortableMountId::new("mount-alpha").unwrap(),
        vec![source_state(
            "source-drive",
            "generation-notion-109",
            vec![],
        )],
    )
    .unwrap();
    assert!(matches!(
        baseline_with(generations.clone(), vec![crossed, mounts()[1].clone()]),
        Err(GenerationBaselineError::MountGenerationMismatch { .. })
    ));

    let mut reordered_files = notion_files();
    reordered_files.reverse();
    assert_eq!(
        GenerationBaselineSourceV1::new(
            SourceConnectionId::new("source-notion"),
            SourceGenerationId::new("generation-notion-109").unwrap(),
            reordered_files,
        ),
        Err(GenerationBaselineError::NonCanonicalFileOrder)
    );

    let duplicate_projection = notion_files()[0].clone();
    let duplicate_mounts = vec![
        GenerationBaselineMountV1::new(
            PortableMountId::new("mount-alpha").unwrap(),
            vec![source_state(
                "source-drive",
                "generation-drive-44",
                vec![duplicate_projection.clone()],
            )],
        )
        .unwrap(),
        GenerationBaselineMountV1::new(
            PortableMountId::new("mount-zeta").unwrap(),
            vec![source_state(
                "source-notion",
                "generation-notion-109",
                vec![duplicate_projection],
            )],
        )
        .unwrap(),
    ];
    assert!(matches!(
        baseline_with(generations, duplicate_mounts),
        Err(GenerationBaselineError::DuplicateProjectionId { .. })
    ));
}

#[derive(Clone)]
struct ExportFileSpec<'a> {
    source_index: usize,
    projection_id: &'a str,
    logical_path: &'a str,
    byte_length: u64,
    digest_byte: char,
}

fn replica_status(source_connection_id: &str) -> ReplicaFreshnessStatus {
    ReplicaFreshnessStatus {
        source_connection_id: SourceConnectionId::new(source_connection_id),
        state: ReplicaFreshnessState::Fresh,
        coverage_complete: true,
        provider_observed_through: Some("checkpoint-1".to_string()),
        last_successful_sync_at: Some("2026-07-19T11:58:00Z".to_string()),
        last_repair_at: None,
        pending_events: 0,
        backlog: 0,
        provider_cooldown_until: None,
    }
}

fn shared_mount_export(source_ids: &[&str], files: &[ExportFileSpec<'_>]) -> ExportContext {
    let profile_id = WorkspaceProfileId::new("018f4f6e-9f2c-7b1a-8c3d-4e5f60718293").unwrap();
    let profile_revision = 7;
    let mount_id = PortableMountId::new("mount-shared").unwrap();
    let target = MountTarget::new("Shared").unwrap();
    let workspace = WorkspaceLayout::new(
        profile_id.clone(),
        profile_revision,
        vec![ProfileMount::new(mount_id.clone(), target)],
        source_ids
            .iter()
            .enumerate()
            .map(|(ordinal, _)| {
                ProfileScopeBinding::new(
                    ordinal as u32,
                    SourceScopeId::new(format!("scope-{ordinal}")).unwrap(),
                    mount_id.clone(),
                )
            })
            .collect(),
    )
    .unwrap();
    let layout = SessionLayout::from_workspace(&workspace).unwrap();
    let session = WorkspaceProfileSessionV2::new(
        SessionId::new("session-shared"),
        "opaque-capability",
        "2026-07-29T01:00:00Z",
        profile_id,
        profile_revision,
        layout,
    )
    .unwrap();
    let capabilities = WorkspaceClientCapabilitiesV2::workspace_layout_v1(true);
    let selected_content_bytes = files
        .iter()
        .map(|file| file.byte_length)
        .try_fold(0_u64, u64::checked_add)
        .unwrap();
    let limits = ExportAttemptLimits {
        max_files: u64::try_from(files.len().max(1)).unwrap(),
        max_directories: 10,
        max_content_bytes: selected_content_bytes.max(1),
    };
    let status = WorkspaceSessionStatusV2::new(
        &session,
        &capabilities,
        SCOPE_AUTHORIZED_COMPONENT_VERSIONS,
        SandboxSessionState::Ready,
        locality_protocol::FreshnessRequirement {
            max_age_seconds: 300,
            on_stale: StaleSessionBehavior::WaitThenFail,
            wait_timeout_seconds: 30,
        },
        source_ids
            .iter()
            .map(|source| replica_status(source))
            .collect(),
        Some(limits.clone()),
        None,
        "2026-07-23T20:00:00Z",
    )
    .unwrap();
    let source_generations = source_ids
        .iter()
        .enumerate()
        .map(|(ordinal, source)| OrderedSourceGeneration {
            ordinal: ordinal as u32,
            source_connection_id: SourceConnectionId::new(*source),
            source_generation_id: SourceGenerationId::new(format!("generation-{ordinal}")).unwrap(),
        })
        .collect::<Vec<_>>();
    let sealed_offer = SealedExportOffer {
        versions: SCOPE_AUTHORIZED_COMPONENT_VERSIONS,
        session_id: session.session_id().clone(),
        export_attempt_id: locality_core::portable::ExportAttemptId::new("attempt-shared").unwrap(),
        source_generations,
        media_type: "application/x-tar".to_string(),
        content_encoding: TarContentEncoding::Zstd,
        limits,
        control_entry_count: 1,
        file_count: files.len() as u64,
        directory_count: 1,
        archive_entry_count: files.len() as u64 + 2,
        selected_content_bytes,
        inventory_sha256: format!("sha256:{}", "0".repeat(64)),
        writable_metadata_sha256: format!("sha256:{}", "1".repeat(64)),
        sealed_at: "2026-07-23T19:00:01Z".to_string(),
        expires_at: "2026-07-23T19:10:01Z".to_string(),
    };
    let placeholder_offer =
        WorkspaceExportOfferV2::new(&session, &status, &capabilities, sealed_offer.clone())
            .unwrap();
    let scope_sources = source_ids
        .iter()
        .enumerate()
        .map(|(ordinal, source)| {
            WorkspaceScopeSourceAuthorityV2::new(ordinal as u32, SourceConnectionId::new(*source))
        })
        .collect::<Vec<_>>();
    let authorized_entries = files
        .iter()
        .map(|file| WorkspaceAuthorizedExportEntryV2::File {
            winning_scope_ordinal: file.source_index as u32,
            mount_id: mount_id.clone(),
            logical_path: file.logical_path.to_string(),
            projection_id: ProjectionId::new(file.projection_id),
            source_connection_id: SourceConnectionId::new(source_ids[file.source_index]),
            file_kind: ProjectionFileKind::Binary,
            effective_actions: BTreeSet::from([SourceAction::Read]),
            content_sha256: format!("sha256:{}", file.digest_byte.to_string().repeat(64)),
            byte_length: file.byte_length,
        })
        .collect::<Vec<_>>();
    let inventory = WorkspaceNamespacedInventoryV2::plan(
        session.session_layout(),
        &placeholder_offer,
        &scope_sources,
        &authorized_entries,
    )
    .unwrap();
    let mut final_sealed_offer = sealed_offer;
    final_sealed_offer.inventory_sha256 = inventory.inventory_sha256().to_string();
    let offer =
        WorkspaceExportOfferV2::new(&session, &status, &capabilities, final_sealed_offer).unwrap();
    inventory
        .validate_against_export(session.session_layout(), &offer)
        .unwrap();
    ExportContext {
        session,
        offer,
        inventory,
    }
}

#[test]
fn shared_mount_carries_canonical_per_source_state() {
    let specs = [ExportFileSpec {
        source_index: 1,
        projection_id: "projection-notion",
        logical_path: "notion.md",
        byte_length: 6,
        digest_byte: '3',
    }];
    let context = shared_mount_export(&["source-drive", "source-notion"], &specs);
    let mount = GenerationBaselineMountV1::new(
        PortableMountId::new("mount-shared").unwrap(),
        vec![
            source_state("source-drive", "generation-0", vec![]),
            source_state(
                "source-notion",
                "generation-1",
                vec![file(
                    "projection-notion",
                    "notion.md",
                    "content-notion-v1",
                    '3',
                    6,
                )],
            ),
        ],
    )
    .unwrap();
    let baseline = GenerationBaselineResponseV1::from_export(
        &context.session,
        &context.offer,
        &context.inventory,
        vec![mount],
    )
    .expect("shared mount baseline");
    assert_eq!(baseline.mounts()[0].sources().len(), 2);
    assert!(baseline.mounts()[0].sources()[0].files().is_empty());

    let mut reversed_sources = baseline.mounts()[0].sources().to_vec();
    reversed_sources.reverse();
    let reversed_mount = GenerationBaselineMountV1::new(
        PortableMountId::new("mount-shared").unwrap(),
        reversed_sources,
    )
    .unwrap();
    assert!(matches!(
        GenerationBaselineResponseV1::from_export(
            &context.session,
            &context.offer,
            &context.inventory,
            vec![reversed_mount],
        ),
        Err(GenerationBaselineError::NonCanonicalMountSourceOrder { .. })
    ));
}

#[test]
fn negotiated_export_limits_do_not_become_lower_baseline_limits() {
    let byte_length = MAX_GENERATION_FILE_BYTES + 1;
    let specs = [ExportFileSpec {
        source_index: 0,
        projection_id: "projection-large",
        logical_path: "large.bin",
        byte_length,
        digest_byte: '4',
    }];
    let context = shared_mount_export(&["source-large"], &specs);
    let source = source_state(
        "source-large",
        "generation-0",
        vec![file(
            "projection-large",
            "large.bin",
            "content-large-v1",
            '4',
            byte_length,
        )],
    );
    assert_eq!(
        source.refresh_mode(),
        GenerationBaselineRefreshModeV1::FullExportOnly
    );
    let mount =
        GenerationBaselineMountV1::new(PortableMountId::new("mount-shared").unwrap(), vec![source])
            .unwrap();
    GenerationBaselineResponseV1::from_export(
        &context.session,
        &context.offer,
        &context.inventory,
        vec![mount],
    )
    .expect("valid negotiated export remains representable with explicit fallback");

    assert_eq!(
        baseline().mounts()[1].sources()[0].refresh_mode(),
        GenerationBaselineRefreshModeV1::GenerationDeltaV1
    );
}

#[test]
fn strict_wire_path_digest_type_unknown_and_derived_encoding_bounds_fail_closed() {
    assert!(LogicalPath::new("/absolute/page.md").is_err());
    assert!(LogicalPath::new("../escape.md").is_err());

    for pointer in [
        "",
        "/source_generations/0",
        "/mounts/0",
        "/mounts/0/sources/0",
        "/mounts/1/sources/0/files/0",
    ] {
        let mut value: Value = serde_json::from_slice(GENERATION_BASELINE_V1_GOLDEN_JSON).unwrap();
        value
            .pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("credential".to_string(), json!("must-not-be-accepted"));
        assert!(
            decode_value(&value).is_err(),
            "unknown field accepted at {pointer}"
        );
    }

    for (pointer, value) in [
        ("/profile_id", json!("NOT-A-UUID")),
        (
            "/mounts/1/sources/0/files/0/logical_path",
            json!("/tmp/credential.txt"),
        ),
        ("/mounts/1/sources/0/files/0/byte_length", json!("7")),
        (
            "/inventory_sha256",
            json!(format!("sha256:{}", "A".repeat(64))),
        ),
        (
            "/baseline_sha256",
            json!(format!("sha256:{}", "0".repeat(64))),
        ),
    ] {
        let mut candidate: Value =
            serde_json::from_slice(GENERATION_BASELINE_V1_GOLDEN_JSON).unwrap();
        *candidate.pointer_mut(pointer).unwrap() = value;
        assert!(
            decode_value(&candidate).is_err(),
            "invalid value accepted at {pointer}"
        );
    }

    let maximum = maximum_encoded_bytes_for_export(&session(), &offer(), &inventory()).unwrap();
    let oversized = vec![b' '; maximum + 1];
    assert!(matches!(
        GenerationBaselineResponseV1::decode_json_against_export(
            &oversized,
            &session(),
            &offer(),
            &inventory(),
        ),
        Err(GenerationBaselineError::EncodingTooLarge { .. })
    ));

    let oversized_content_version =
        "c".repeat(MAX_GENERATION_BASELINE_CONTENT_VERSION_ID_BYTES + 1);
    assert!(matches!(
        GenerationBaselineSourceV1::new(
            SourceConnectionId::new("source-notion"),
            SourceGenerationId::new("generation-notion-109").unwrap(),
            vec![file(
                "projection-readme",
                "README.md",
                &oversized_content_version,
                '6',
                7,
            )],
        ),
        Err(GenerationBaselineError::ContentVersionIdTooLong { .. })
    ));
}
