use std::collections::BTreeSet;

use locality_core::model::RemoteId;
use locality_core::portable::{
    LogicalPath, ProjectionFileKind, ProjectionId, SourceAction, SourceConnectionId,
};
use locality_core::workspace_layout::PortableMountId;
use locality_protocol::workspace_api_v2::{
    WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON, WORKSPACE_PROFILE_SESSION_V2_GOLDEN_JSON,
    WorkspaceExportOfferV2, WorkspaceProfileSessionV2,
};
use locality_protocol::workspace_export_v2::{
    WORKSPACE_EXPORT_INVENTORY_V2_GOLDEN_JSON, WORKSPACE_EXPORT_TERMINAL_CONTROL_V2_GOLDEN_JSON,
    WorkspaceArchiveEntryKindV2, WorkspaceArchiveMemberV2, WorkspaceAuthorizedExportEntryV2,
    WorkspaceExportCompletionReceiptV2, WorkspaceExportControlMetadataV2,
    WorkspaceExportTerminalControlV2, WorkspaceExportV2Error, WorkspaceMaterializationPlanV2,
    WorkspaceMaterializationPlanWithInventoryV2, WorkspaceNamespacedExportRecordV2,
    WorkspaceNamespacedInventoryV2, WorkspaceScopeSourceAuthorityV2,
};
use locality_protocol::workspace_layout::{SESSION_LAYOUT_V1_GOLDEN_JSON, SessionLayout};
use locality_protocol::{
    CANONICAL_EXPORT_RECORDS_GOLDEN_JSON, CanonicalExportRecord, DeliveredBodyDigestV2,
    EXPORT_TERMINAL_CONTROL_V2_GOLDEN_JSON, ExportCompletionReceipt, ExportTerminalControlV2,
    ScopeAuthorizedWritableExportMetadata, WritableMetadataEntry,
    canonical_export_inventory_sha256, canonical_writable_metadata_sha256,
};
use serde_json::json;

fn actions(actions: &[SourceAction]) -> BTreeSet<SourceAction> {
    actions.iter().cloned().collect()
}

fn authorized_entries() -> Vec<WorkspaceAuthorizedExportEntryV2> {
    vec![
        WorkspaceAuthorizedExportEntryV2::Directory {
            winning_scope_ordinal: 0,
            mount_id: PortableMountId::new("mount-zeta").unwrap(),
            logical_path: "Projects".to_string(),
        },
        WorkspaceAuthorizedExportEntryV2::Directory {
            winning_scope_ordinal: 0,
            mount_id: PortableMountId::new("mount-zeta").unwrap(),
            logical_path: "Projects/Roadmap".to_string(),
        },
        WorkspaceAuthorizedExportEntryV2::File {
            winning_scope_ordinal: 0,
            mount_id: PortableMountId::new("mount-zeta").unwrap(),
            logical_path: "Projects/Roadmap/page.md".to_string(),
            projection_id: ProjectionId::new("projection-roadmap"),
            source_connection_id: SourceConnectionId::new("source-notion"),
            file_kind: ProjectionFileKind::Markdown,
            effective_actions: actions(&[SourceAction::Read, SourceAction::Update]),
            content_sha256: format!("sha256:{}", "5".repeat(64)),
            byte_length: 10,
        },
        WorkspaceAuthorizedExportEntryV2::File {
            winning_scope_ordinal: 2,
            mount_id: PortableMountId::new("mount-zeta").unwrap(),
            logical_path: "README.md".to_string(),
            projection_id: ProjectionId::new("projection-readme"),
            source_connection_id: SourceConnectionId::new("source-notion"),
            file_kind: ProjectionFileKind::Markdown,
            effective_actions: actions(&[SourceAction::Read]),
            content_sha256: format!("sha256:{}", "6".repeat(64)),
            byte_length: 7,
        },
    ]
}

fn session_layout() -> SessionLayout {
    serde_json::from_slice(SESSION_LAYOUT_V1_GOLDEN_JSON).expect("session layout fixture")
}

fn scope_sources() -> Vec<WorkspaceScopeSourceAuthorityV2> {
    vec![
        WorkspaceScopeSourceAuthorityV2::new(0, SourceConnectionId::new("source-notion")),
        WorkspaceScopeSourceAuthorityV2::new(1, SourceConnectionId::new("source-drive")),
        WorkspaceScopeSourceAuthorityV2::new(2, SourceConnectionId::new("source-notion")),
        WorkspaceScopeSourceAuthorityV2::new(3, SourceConnectionId::new("source-drive")),
    ]
}

fn inventory() -> WorkspaceNamespacedInventoryV2 {
    plan_inventory(&authorized_entries()).expect("namespaced inventory")
}

fn plan_inventory(
    entries: &[WorkspaceAuthorizedExportEntryV2],
) -> Result<WorkspaceNamespacedInventoryV2, WorkspaceExportV2Error> {
    WorkspaceNamespacedInventoryV2::plan(&session_layout(), &offer(), &scope_sources(), entries)
}

fn exact_pretty_json(value: &impl serde::Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("serialize fixture");
    bytes.push(b'\n');
    bytes
}

fn session() -> WorkspaceProfileSessionV2 {
    serde_json::from_slice(WORKSPACE_PROFILE_SESSION_V2_GOLDEN_JSON).expect("workspace session")
}

fn offer() -> WorkspaceExportOfferV2 {
    serde_json::from_slice(WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON).expect("workspace offer")
}

fn writable_metadata() -> ScopeAuthorizedWritableExportMetadata {
    let offer = offer();
    ScopeAuthorizedWritableExportMetadata {
        versions: offer.offer().versions,
        session_id: offer.offer().session_id.clone(),
        export_attempt_id: offer.offer().export_attempt_id.clone(),
        source_generations: offer.offer().source_generations.clone(),
        writable_entries: vec![WritableMetadataEntry {
            projection_id: ProjectionId::new("projection-roadmap"),
            logical_path: LogicalPath::new("Projects/Roadmap/page.md").unwrap(),
            source_remote_ids: vec![RemoteId::new("page-roadmap")],
            delivered_content_sha256: format!("sha256:{}", "5".repeat(64)),
            provider_precondition: "opaque-v4".to_string(),
            effective_actions: actions(&[SourceAction::Read, SourceAction::Update]),
            baseline_required: true,
        }],
    }
}

fn terminal_control() -> WorkspaceExportTerminalControlV2 {
    let session = session();
    let offer = offer();
    let inventory = inventory();
    let metadata = WorkspaceExportControlMetadataV2::new(&session, &offer, &inventory)
        .expect("control metadata");
    let writable_metadata = writable_metadata();
    let writable_metadata_sha256 =
        canonical_writable_metadata_sha256(&writable_metadata).expect("writable digest");
    let mut body_digest = DeliveredBodyDigestV2::new(2);
    body_digest
        .update_file(&ProjectionId::new("projection-roadmap"), b"# Roadmap\n")
        .unwrap();
    body_digest
        .update_file(&ProjectionId::new("projection-readme"), b"Public\n")
        .unwrap();
    let receipt = ExportCompletionReceipt {
        versions: offer.offer().versions,
        session_id: offer.offer().session_id.clone(),
        export_attempt_id: offer.offer().export_attempt_id.clone(),
        source_generations: offer.offer().source_generations.clone(),
        inventory_sha256: inventory.inventory_sha256().to_string(),
        writable_metadata_sha256,
        delivered_control_entry_count: inventory.control_entry_count(),
        delivered_file_count: inventory.file_count(),
        delivered_directory_count: inventory.directory_count(),
        delivered_archive_entry_count: inventory.archive_entry_count(),
        delivered_content_bytes: inventory.selected_content_bytes(),
        delivered_body_sha256: body_digest.finish().unwrap(),
        completed_at: "2026-07-23T19:00:04Z".to_string(),
    };
    WorkspaceExportTerminalControlV2 {
        metadata: metadata.clone(),
        writable_metadata,
        completion_receipt: WorkspaceExportCompletionReceiptV2 { metadata, receipt },
    }
}

fn archive_members() -> Vec<WorkspaceArchiveMemberV2> {
    let entries = authorized_entries();
    vec![
        WorkspaceArchiveMemberV2 {
            kind: WorkspaceArchiveEntryKindV2::Directory,
            member_path: "Engineering".to_string(),
            authorized_entry: None,
        },
        WorkspaceArchiveMemberV2 {
            kind: WorkspaceArchiveEntryKindV2::Directory,
            member_path: "Sales".to_string(),
            authorized_entry: None,
        },
        WorkspaceArchiveMemberV2 {
            kind: WorkspaceArchiveEntryKindV2::Directory,
            member_path: "Sales/Projects".to_string(),
            authorized_entry: Some(entries[0].clone()),
        },
        WorkspaceArchiveMemberV2 {
            kind: WorkspaceArchiveEntryKindV2::Directory,
            member_path: "Sales/Projects/Roadmap".to_string(),
            authorized_entry: Some(entries[1].clone()),
        },
        WorkspaceArchiveMemberV2 {
            kind: WorkspaceArchiveEntryKindV2::File,
            member_path: "Sales/Projects/Roadmap/page.md".to_string(),
            authorized_entry: Some(entries[2].clone()),
        },
        WorkspaceArchiveMemberV2 {
            kind: WorkspaceArchiveEntryKindV2::File,
            member_path: "Sales/README.md".to_string(),
            authorized_entry: Some(entries[3].clone()),
        },
        WorkspaceArchiveMemberV2 {
            kind: WorkspaceArchiveEntryKindV2::Control,
            member_path: ".loc/session.json".to_string(),
            authorized_entry: None,
        },
    ]
}

#[test]
fn namespaced_inventory_golden_is_deterministic_and_keeps_empty_targets() {
    let inventory = inventory();
    assert_eq!(inventory.directory_count(), 4);
    assert_eq!(inventory.file_count(), 2);
    assert_eq!(inventory.archive_entry_count(), 7);
    assert_eq!(inventory.selected_content_bytes(), 17);
    assert_eq!(
        inventory.target_directories()[0].target().as_str(),
        "Engineering"
    );
    assert_eq!(inventory.target_directories()[0].directory_count(), 1);
    assert_eq!(inventory.target_directories()[0].file_count(), 0);

    let decoded = WorkspaceNamespacedInventoryV2::decode_json(
        WORKSPACE_EXPORT_INVENTORY_V2_GOLDEN_JSON,
        &session_layout(),
        &offer(),
    )
    .expect("inventory golden");
    decoded
        .validate_against_export(&session_layout(), &offer())
        .expect("inventory must recompute against the exact layout and offer");
    assert_eq!(decoded, inventory);
    assert_eq!(
        exact_pretty_json(&decoded),
        WORKSPACE_EXPORT_INVENTORY_V2_GOLDEN_JSON
    );
}

#[test]
fn inventory_golden_mutations_cannot_change_digest_counts_or_namespaced_paths() {
    let valid: serde_json::Value =
        serde_json::from_slice(WORKSPACE_EXPORT_INVENTORY_V2_GOLDEN_JSON).unwrap();
    let mut mutations = Vec::new();

    let mut digest = valid.clone();
    digest["inventory_sha256"] = json!(format!("sha256:{}", "0".repeat(64)));
    mutations.push(digest);

    let mut count = valid.clone();
    count["directory_count"] = json!(5);
    mutations.push(count);

    let mut target = valid;
    target["records"][4]["member_path"] = json!("Engineering/Roadmap/page.md");
    mutations.push(target);

    let mut foreign_source = mutations.last().unwrap().clone();
    foreign_source["records"][4]["member_path"] = json!("Sales/Projects/Roadmap/page.md");
    foreign_source["records"][4]["source_connection_id"] = json!("source-foreign");
    mutations.push(foreign_source);

    let mut mismatched_source = mutations.last().unwrap().clone();
    mismatched_source["records"][4]["source_connection_id"] = json!("source-drive");
    mutations.push(mismatched_source);

    let mut tampered_authority = mutations.last().unwrap().clone();
    tampered_authority["records"][4]["source_connection_id"] = json!("source-notion");
    tampered_authority["scope_sources"][0]["source_connection_id"] = json!("source-drive");
    tampered_authority["scope_sources"][1]["source_connection_id"] = json!("source-notion");
    mutations.push(tampered_authority);

    for mutation in mutations {
        assert!(
            WorkspaceNamespacedInventoryV2::decode_json(
                &serde_json::to_vec(&mutation).unwrap(),
                &session_layout(),
                &offer(),
            )
            .is_err()
        );
    }
}

#[test]
fn terminal_control_golden_binds_session_offer_layout_inventory_targets_and_counts() {
    let control = terminal_control();
    let decoded: WorkspaceExportTerminalControlV2 =
        serde_json::from_slice(WORKSPACE_EXPORT_TERMINAL_CONTROL_V2_GOLDEN_JSON)
            .expect("terminal control golden");
    assert_eq!(decoded, control);
    assert_eq!(
        serde_json::to_vec(&decoded).unwrap(),
        WORKSPACE_EXPORT_TERMINAL_CONTROL_V2_GOLDEN_JSON
    );

    let session = session();
    let offer = offer();
    let inventory = inventory();
    decoded
        .validate_against(&session, &offer, &inventory)
        .expect("terminal control matches the sealed generation-2 exchange");
    assert_eq!(
        session.session_layout().layout_digest(),
        offer.layout_digest()
    );
    assert_eq!(offer.layout_digest(), decoded.metadata.layout_digest());
    assert_eq!(
        decoded.metadata.layout_digest(),
        decoded.completion_receipt.metadata.layout_digest()
    );
    assert_eq!(offer.offer().inventory_sha256, inventory.inventory_sha256());
    assert_eq!(
        inventory.inventory_sha256(),
        decoded.metadata.inventory_sha256()
    );
    assert_eq!(
        decoded.metadata.inventory_sha256(),
        decoded.completion_receipt.receipt.inventory_sha256
    );
}

#[test]
fn terminal_control_decode_requires_exact_canonical_compact_json() {
    #[derive(serde::Serialize)]
    struct ReorderedControl<'a> {
        completion_receipt: &'a WorkspaceExportCompletionReceiptV2,
        writable_metadata: &'a ScopeAuthorizedWritableExportMetadata,
        metadata: &'a WorkspaceExportControlMetadataV2,
    }

    let control = terminal_control();
    let session = session();
    let offer = offer();
    let inventory = inventory();
    let canonical = serde_json::to_vec(&control).unwrap();
    assert_eq!(canonical, WORKSPACE_EXPORT_TERMINAL_CONTROL_V2_GOLDEN_JSON);
    assert_eq!(
        canonical,
        String::from_utf8(canonical.clone())
            .unwrap()
            .trim()
            .as_bytes()
    );
    WorkspaceExportTerminalControlV2::decode_json(&canonical, &session, &offer, &inventory)
        .expect("canonical compact control");

    let pretty = serde_json::to_vec_pretty(&control).unwrap();
    let reordered = serde_json::to_vec(&ReorderedControl {
        completion_receipt: &control.completion_receipt,
        writable_metadata: &control.writable_metadata,
        metadata: &control.metadata,
    })
    .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&canonical).unwrap(),
        serde_json::from_slice::<serde_json::Value>(&reordered).unwrap()
    );
    let mut trailing_whitespace = canonical.clone();
    trailing_whitespace.push(b'\n');

    for noncanonical in [pretty, reordered, trailing_whitespace] {
        assert!(matches!(
            WorkspaceExportTerminalControlV2::decode_json(
                &noncanonical,
                &session,
                &offer,
                &inventory,
            ),
            Err(WorkspaceExportV2Error::NonCanonicalControlJson)
        ));
    }
}

#[test]
fn pure_materialization_plan_maps_without_a_host_root_and_preserves_logical_paths() {
    let plan = WorkspaceMaterializationPlanV2::plan(
        &session(),
        &offer(),
        &terminal_control(),
        &archive_members(),
    )
    .expect("materialization plan");
    assert_eq!(plan.entries().len(), 6);
    assert_eq!(plan.entries()[0].member_path, "Engineering");
    assert_eq!(plan.entries()[0].logical_path, None);
    assert_eq!(
        plan.entries()[4].logical_path.as_ref().unwrap().as_str(),
        "Projects/Roadmap/page.md"
    );
    assert_eq!(
        plan.entries()[4].member_path,
        "Sales/Projects/Roadmap/page.md"
    );

    let json = serde_json::to_string(&(session(), offer(), inventory(), terminal_control(), plan))
        .expect("portable exchange JSON");
    for forbidden in [
        "host_root",
        "staging_root",
        "absolute_path",
        "/private/",
        "/Users/",
        "C:\\\\",
    ] {
        assert!(
            !json.contains(forbidden),
            "serialized host root marker {forbidden}"
        );
    }
}

#[test]
fn combined_planner_returns_the_exact_inventory_used_for_the_plan() {
    let planned = WorkspaceMaterializationPlanWithInventoryV2::plan(
        &session(),
        &offer(),
        &terminal_control(),
        &archive_members(),
    )
    .expect("combined materialization plan");

    assert_eq!(planned.inventory(), &inventory());
    assert_eq!(planned.materialization_plan().entries().len(), 6);
    let (plan, returned_inventory) = planned.into_parts();
    assert_eq!(
        plan.entries()[4].member_path,
        "Sales/Projects/Roadmap/page.md"
    );
    assert_eq!(
        returned_inventory.inventory_sha256(),
        inventory().inventory_sha256()
    );
}

#[test]
fn combined_planner_structurally_builds_one_inventory_without_authorized_entry_clones() {
    let source = include_str!("../src/workspace_export_v2.rs");
    let combined_start = source
        .find("impl WorkspaceMaterializationPlanWithInventoryV2 {")
        .expect("combined planner implementation");
    let combined_end = source[combined_start..]
        .find("\n#[derive(Clone, Debug, PartialEq, Eq)]\npub enum WorkspaceExportV2Error")
        .expect("combined planner implementation end");
    let combined = &source[combined_start..combined_start + combined_end];

    assert_eq!(
        combined.matches("let inventory = plan_inventory(").count(),
        1
    );
    assert!(!combined.contains("WorkspaceNamespacedInventoryV2::plan("));
    assert!(!combined.contains("authorized_entries"));
    assert!(!combined.contains("authorized_entry.clone()"));

    let inventory_start = source
        .find("fn plan_inventory<'a>(")
        .expect("inventory planner implementation");
    let inventory_end = source[inventory_start..]
        .find("\nfn canonical_inventory_preimage(")
        .expect("inventory planner implementation end");
    let inventory_planner = &source[inventory_start..inventory_start + inventory_end];
    assert!(
        inventory_planner.contains("IntoIterator<Item = &'a WorkspaceAuthorizedExportEntryV2>")
    );
    assert!(!inventory_planner.contains("entry.clone()"));
}

#[test]
fn inventory_rejects_unknown_scope_mount_duplicates_and_case_fold_collisions() {
    let mut unknown_scope = authorized_entries();
    if let WorkspaceAuthorizedExportEntryV2::Directory {
        winning_scope_ordinal,
        ..
    } = &mut unknown_scope[0]
    {
        *winning_scope_ordinal = 99;
    }
    assert!(matches!(
        plan_inventory(&unknown_scope),
        Err(WorkspaceExportV2Error::UnknownScopeOrdinal { actual: 99 })
    ));

    let mut unknown_mount = authorized_entries();
    if let WorkspaceAuthorizedExportEntryV2::File { mount_id, .. } = &mut unknown_mount[2] {
        *mount_id = PortableMountId::new("mount-unknown").unwrap();
    }
    assert!(matches!(
        plan_inventory(&unknown_mount),
        Err(WorkspaceExportV2Error::UnknownMount { .. })
    ));

    let mut duplicate = authorized_entries();
    duplicate.push(duplicate[3].clone());
    assert!(matches!(
        plan_inventory(&duplicate),
        Err(WorkspaceExportV2Error::DuplicateMaterializedPath { .. })
    ));

    let mut collision = authorized_entries();
    let mut colliding_file = collision[3].clone();
    if let WorkspaceAuthorizedExportEntryV2::File {
        logical_path,
        projection_id,
        ..
    } = &mut colliding_file
    {
        *logical_path = "readme.MD".to_string();
        *projection_id = ProjectionId::new("projection-collision");
    }
    collision.push(colliding_file);
    assert!(matches!(
        plan_inventory(&collision),
        Err(WorkspaceExportV2Error::CaseFoldCollision { .. })
    ));
}

#[test]
fn inventory_binds_file_sources_to_offered_scope_authority() {
    let mut foreign_source = authorized_entries();
    if let WorkspaceAuthorizedExportEntryV2::File {
        source_connection_id,
        ..
    } = &mut foreign_source[2]
    {
        *source_connection_id = SourceConnectionId::new("source-foreign");
    }
    assert!(matches!(
        plan_inventory(&foreign_source),
        Err(WorkspaceExportV2Error::SourceNotInOffer { source_connection_id })
            if source_connection_id == "source-foreign"
    ));

    let mut wrong_offered_source = authorized_entries();
    if let WorkspaceAuthorizedExportEntryV2::File {
        source_connection_id,
        ..
    } = &mut wrong_offered_source[2]
    {
        *source_connection_id = SourceConnectionId::new("source-drive");
    }
    assert!(matches!(
        plan_inventory(&wrong_offered_source),
        Err(WorkspaceExportV2Error::SourceScopeMismatch {
            scope_ordinal: 0,
            expected,
            actual,
        }) if expected == "source-notion" && actual == "source-drive"
    ));

    let mut wrong_authority = scope_sources();
    wrong_authority[0] =
        WorkspaceScopeSourceAuthorityV2::new(0, SourceConnectionId::new("source-drive"));
    wrong_authority[1] =
        WorkspaceScopeSourceAuthorityV2::new(1, SourceConnectionId::new("source-notion"));
    assert!(matches!(
        WorkspaceNamespacedInventoryV2::plan(
            &session_layout(),
            &offer(),
            &wrong_authority,
            &authorized_entries(),
        ),
        Err(WorkspaceExportV2Error::SourceScopeMismatch {
            scope_ordinal: 0,
            ..
        })
    ));

    let mut foreign_authority = scope_sources();
    foreign_authority[0] =
        WorkspaceScopeSourceAuthorityV2::new(0, SourceConnectionId::new("source-foreign"));
    assert!(matches!(
        WorkspaceNamespacedInventoryV2::plan(
            &session_layout(),
            &offer(),
            &foreign_authority,
            &authorized_entries(),
        ),
        Err(WorkspaceExportV2Error::InvalidScopeSourceAuthority)
    ));
}

#[test]
fn inventory_rejects_unsafe_components_and_utf8_utf16_path_ceilings() {
    for path in [
        "../escape.md".to_string(),
        "/absolute.md".to_string(),
        "bad?.md".to_string(),
        "e\u{301}.md".to_string(),
        "CON.txt".to_string(),
    ] {
        let mut entries = authorized_entries();
        if let WorkspaceAuthorizedExportEntryV2::File { logical_path, .. } = &mut entries[3] {
            *logical_path = path;
        }
        assert!(plan_inventory(&entries).is_err());
    }

    let mut component_too_long = authorized_entries();
    if let WorkspaceAuthorizedExportEntryV2::File { logical_path, .. } = &mut component_too_long[3]
    {
        *logical_path = "é".repeat(128);
    }
    assert!(matches!(
        plan_inventory(&component_too_long),
        Err(WorkspaceExportV2Error::ComponentUtf8TooLong { actual: 256 })
    ));

    let long_component = "a".repeat(250);
    let mut path_too_long = authorized_entries();
    if let WorkspaceAuthorizedExportEntryV2::File { logical_path, .. } = &mut path_too_long[3] {
        *logical_path = std::iter::repeat_n(long_component, 5)
            .collect::<Vec<_>>()
            .join("/");
    }
    assert!(matches!(
        plan_inventory(&path_too_long),
        Err(WorkspaceExportV2Error::PathUtf8TooLong { .. })
    ));

    // Equal byte/unit ceilings mean every scalar string crossing the UTF-16
    // ceiling also crosses UTF-8. Both negotiated ceilings remain enforced.
    let mut utf16_too_long = authorized_entries();
    if let WorkspaceAuthorizedExportEntryV2::File { logical_path, .. } = &mut utf16_too_long[3] {
        *logical_path = "a".repeat(256);
    }
    assert!(plan_inventory(&utf16_too_long).is_err());
}

#[test]
fn materialization_rejects_unknown_targets_forbidden_kinds_and_bad_control_sequence() {
    let mut unknown_target = archive_members();
    unknown_target[5].member_path = "Unknown/README.md".to_string();
    assert!(matches!(
        WorkspaceMaterializationPlanV2::plan(
            &session(),
            &offer(),
            &terminal_control(),
            &unknown_target,
        ),
        Err(WorkspaceExportV2Error::UnknownTopLevelTarget { .. })
    ));

    for kind in [
        WorkspaceArchiveEntryKindV2::Symlink,
        WorkspaceArchiveEntryKindV2::Hardlink,
        WorkspaceArchiveEntryKindV2::BlockDevice,
        WorkspaceArchiveEntryKindV2::CharacterDevice,
        WorkspaceArchiveEntryKindV2::Fifo,
    ] {
        let mut members = archive_members();
        members.insert(
            members.len() - 1,
            WorkspaceArchiveMemberV2 {
                kind,
                member_path: "Sales/forbidden".to_string(),
                authorized_entry: None,
            },
        );
        assert!(matches!(
            WorkspaceMaterializationPlanV2::plan(
                &session(),
                &offer(),
                &terminal_control(),
                &members,
            ),
            Err(WorkspaceExportV2Error::UnsupportedArchiveEntryKind { kind: actual })
                if actual == kind
        ));
    }

    let mut control_first = archive_members();
    let control = control_first.pop().unwrap();
    control_first.insert(0, control);
    assert!(matches!(
        WorkspaceMaterializationPlanV2::plan(
            &session(),
            &offer(),
            &terminal_control(),
            &control_first,
        ),
        Err(WorkspaceExportV2Error::InvalidControlSequence)
    ));

    let mut missing_control = archive_members();
    missing_control.pop();
    assert!(matches!(
        WorkspaceMaterializationPlanV2::plan(
            &session(),
            &offer(),
            &terminal_control(),
            &missing_control,
        ),
        Err(WorkspaceExportV2Error::InvalidControlSequence)
    ));
}

#[test]
fn terminal_control_mutations_break_layout_inventory_target_and_receipt_bindings() {
    let session = session();
    let offer = offer();
    let inventory = inventory();
    let valid = serde_json::to_value(terminal_control()).unwrap();
    let mut mutations = Vec::new();

    let mut wrong_layout = valid.clone();
    wrong_layout["metadata"]["layout_digest"] = json!(format!("sha256:{}", "0".repeat(64)));
    mutations.push(wrong_layout);

    let mut wrong_inventory = valid.clone();
    wrong_inventory["completion_receipt"]["receipt"]["inventory_sha256"] =
        json!(format!("sha256:{}", "0".repeat(64)));
    mutations.push(wrong_inventory);

    let mut wrong_target_count = valid.clone();
    wrong_target_count["metadata"]["target_directories"][0]["directory_count"] = json!(2);
    mutations.push(wrong_target_count);

    let mut wrong_offer_version = valid.clone();
    wrong_offer_version["completion_receipt"]["receipt"]["versions"]["session"] = json!(1);
    mutations.push(wrong_offer_version);

    let mut host_root = valid;
    host_root["metadata"]["host_root"] = json!("/private/tmp/workspace");
    mutations.push(host_root);

    for mutation in mutations {
        let bytes = serde_json::from_value::<WorkspaceExportTerminalControlV2>(mutation.clone())
            .map(|control| serde_json::to_vec(&control).unwrap())
            .unwrap_or_else(|_| serde_json::to_vec(&mutation).unwrap());
        assert!(
            WorkspaceExportTerminalControlV2::decode_json(&bytes, &session, &offer, &inventory,)
                .is_err()
        );
    }
}

#[test]
fn generation_1_flat_inventory_and_control_bytes_are_unchanged() {
    let records: Vec<CanonicalExportRecord> =
        serde_json::from_slice(CANONICAL_EXPORT_RECORDS_GOLDEN_JSON).unwrap();
    assert_eq!(
        canonical_export_inventory_sha256(&records).unwrap(),
        "sha256:025cdbae136931542f7fa881da423e8e1f29a6132cf26ae5f4eea53c53a8ef51"
    );
    let control: ExportTerminalControlV2 =
        serde_json::from_slice(EXPORT_TERMINAL_CONTROL_V2_GOLDEN_JSON).unwrap();
    assert_eq!(
        exact_pretty_json(&control),
        EXPORT_TERMINAL_CONTROL_V2_GOLDEN_JSON
    );

    assert!(matches!(
        inventory().records()[0],
        WorkspaceNamespacedExportRecordV2::TargetDirectory { .. }
    ));
}
