use locality_core::portable::SourceScopeId;
use locality_core::workspace_layout::{MountTarget, PortableMountId};
use locality_protocol::workspace_layout::{
    LayoutDigest, MAX_PROFILE_MOUNTS, MAX_PROFILE_SCOPE_BINDINGS, ProfileMount,
    ProfileScopeBinding, SESSION_LAYOUT_V1_GOLDEN_JSON, SessionLayout, SessionLayoutEntry,
    WORKSPACE_LAYOUT_V1_GOLDEN_JSON, WORKSPACE_LAYOUT_V1_PREIMAGE_GOLDEN_JSON, WorkspaceLayout,
    WorkspaceLayoutError, WorkspaceProfileId,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PreimageFixture {
    preimage_hex: String,
    layout_digest: String,
}

fn profile_id() -> WorkspaceProfileId {
    WorkspaceProfileId::new("018f4f6e-9f2c-7b1a-8c3d-4e5f60718293").expect("profile ID")
}

fn mount(id: &str, target: &str) -> ProfileMount {
    ProfileMount::new(
        PortableMountId::new(id).expect("mount ID"),
        MountTarget::new(target).expect("target"),
    )
}

fn binding(ordinal: u32, source_scope_id: &str, mount_id: &str) -> ProfileScopeBinding {
    ProfileScopeBinding::new(
        ordinal,
        SourceScopeId::new(source_scope_id).expect("scope ID"),
        PortableMountId::new(mount_id).expect("mount ID"),
    )
}

fn fixture_layout_with_scope_prefix(prefix: &str) -> WorkspaceLayout {
    WorkspaceLayout::new(
        profile_id(),
        7,
        vec![
            mount("mount-alpha", "Engineering"),
            mount("mount-zeta", "Sales"),
        ],
        vec![
            binding(0, &format!("{prefix}-sales-primary"), "mount-zeta"),
            binding(1, &format!("{prefix}-eng-primary"), "mount-alpha"),
            binding(2, &format!("{prefix}-sales-secondary"), "mount-zeta"),
            binding(3, &format!("{prefix}-eng-secondary"), "mount-alpha"),
        ],
    )
    .expect("fixture layout")
}

fn fixture_layout() -> WorkspaceLayout {
    fixture_layout_with_scope_prefix("scope")
}

fn exact_pretty_json(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("serialize fixture");
    bytes.push(b'\n');
    bytes
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut value, "{byte:02x}").expect("write hex");
    }
    value
}

#[test]
fn workspace_and_session_layouts_are_exact_lf_golden_json() {
    let workspace = fixture_layout();
    let decoded = serde_json::from_slice::<WorkspaceLayout>(WORKSPACE_LAYOUT_V1_GOLDEN_JSON)
        .expect("workspace fixture decodes");
    assert_eq!(decoded, workspace);
    assert_eq!(exact_pretty_json(&decoded), WORKSPACE_LAYOUT_V1_GOLDEN_JSON);

    let session = SessionLayout::from_workspace(&workspace).expect("session layout");
    let decoded = serde_json::from_slice::<SessionLayout>(SESSION_LAYOUT_V1_GOLDEN_JSON)
        .expect("session fixture decodes");
    assert_eq!(decoded, session);
    assert_eq!(exact_pretty_json(&decoded), SESSION_LAYOUT_V1_GOLDEN_JSON);
    decoded
        .validate_against_workspace(&workspace)
        .expect("fixture session matches workspace");
}

#[test]
fn canonical_preimage_and_digest_are_exact_golden_bytes() {
    let workspace = fixture_layout();
    let preimage = workspace.canonical_preimage().expect("canonical preimage");
    assert!(preimage.starts_with(b"locality.workspace-layout.v1\0"));
    assert_eq!(
        workspace.recompute_digest().expect("digest"),
        *workspace.layout_digest()
    );

    let expected = PreimageFixture {
        preimage_hex: hex(&preimage),
        layout_digest: workspace.layout_digest().as_str().to_string(),
    };
    let decoded =
        serde_json::from_slice::<PreimageFixture>(WORKSPACE_LAYOUT_V1_PREIMAGE_GOLDEN_JSON)
            .expect("preimage fixture decodes");
    assert_eq!(decoded, expected);
    assert_eq!(
        exact_pretty_json(&decoded),
        WORKSPACE_LAYOUT_V1_PREIMAGE_GOLDEN_JSON
    );
}

#[test]
fn source_scope_ids_are_unique_authorization_facts_but_not_digest_input() {
    let first = fixture_layout_with_scope_prefix("scope");
    let second = fixture_layout_with_scope_prefix("replacement");
    assert_ne!(first.scope_bindings(), second.scope_bindings());
    assert_eq!(
        first.canonical_preimage().unwrap(),
        second.canonical_preimage().unwrap()
    );
    assert_eq!(first.layout_digest(), second.layout_digest());
}

#[test]
fn profile_and_digest_spellings_are_canonical() {
    assert!(WorkspaceProfileId::new("018F4F6E-9F2C-7B1A-8C3D-4E5F60718293").is_err());
    assert!(WorkspaceProfileId::new("018f4f6e9f2c7b1a8c3d4e5f60718293").is_err());
    assert!(WorkspaceProfileId::new("00000000-0000-0000-0000-000000000000").is_err());

    let lower = format!("sha256:{}", "a".repeat(64));
    assert_eq!(LayoutDigest::new(&lower).expect("digest").as_str(), lower);
    assert!(LayoutDigest::new(format!("sha256:{}", "A".repeat(64))).is_err());
    assert!(LayoutDigest::new("sha256:abc").is_err());
}

#[test]
fn constructors_reject_noncanonical_profile_collections_without_reordering() {
    assert_eq!(
        WorkspaceLayout::new(
            profile_id(),
            0,
            vec![mount("a", "A")],
            vec![binding(0, "s", "a")]
        ),
        Err(WorkspaceLayoutError::ZeroProfileRevision)
    );
    assert!(matches!(
        WorkspaceLayout::new(profile_id(), 1, Vec::new(), Vec::new()),
        Err(WorkspaceLayoutError::MountCount { actual: 0 })
    ));

    let too_many_mounts = (0..=MAX_PROFILE_MOUNTS)
        .map(|index| mount(&format!("mount-{index:03}"), &format!("Target-{index:03}")))
        .collect();
    assert!(matches!(
        WorkspaceLayout::new(profile_id(), 1, too_many_mounts, vec![binding(0, "s", "mount-000")]),
        Err(WorkspaceLayoutError::MountCount { actual }) if actual == MAX_PROFILE_MOUNTS + 1
    ));

    assert!(matches!(
        WorkspaceLayout::new(
            profile_id(),
            1,
            vec![mount("z", "Zed"), mount("a", "Alpha")],
            vec![binding(0, "s-z", "z"), binding(1, "s-a", "a")],
        ),
        Err(WorkspaceLayoutError::NonCanonicalMountOrder { index: 1 })
    ));
    assert!(matches!(
        WorkspaceLayout::new(
            profile_id(),
            1,
            vec![mount("a", "Alpha"), mount("a", "Beta")],
            vec![binding(0, "s", "a")],
        ),
        Err(WorkspaceLayoutError::DuplicateMountId { index: 1 })
    ));
    assert!(matches!(
        WorkspaceLayout::new(
            profile_id(),
            1,
            vec![mount("a", "Straße"), mount("b", "STRASSE")],
            vec![binding(0, "s-a", "a"), binding(1, "s-b", "b")],
        ),
        Err(WorkspaceLayoutError::TargetCollision { index: 1 })
    ));
}

#[test]
fn constructors_reject_invalid_scope_bindings() {
    assert!(matches!(
        WorkspaceLayout::new(
            profile_id(),
            1,
            vec![mount("a", "Alpha")],
            vec![binding(1, "s", "a")],
        ),
        Err(WorkspaceLayoutError::NonCanonicalScopeOrdinal {
            index: 0,
            actual: 1
        })
    ));
    assert!(matches!(
        WorkspaceLayout::new(
            profile_id(),
            1,
            vec![mount("a", "Alpha")],
            vec![binding(0, "s", "a"), binding(1, "s", "a")],
        ),
        Err(WorkspaceLayoutError::DuplicateSourceScopeId { index: 1 })
    ));
    assert!(matches!(
        WorkspaceLayout::new(
            profile_id(),
            1,
            vec![mount("a", "Alpha")],
            vec![binding(0, "s", "missing")],
        ),
        Err(WorkspaceLayoutError::UnknownMountReference { scope_ordinal: 0 })
    ));
    assert!(matches!(
        WorkspaceLayout::new(
            profile_id(),
            1,
            vec![mount("a", "Alpha"), mount("b", "Beta")],
            vec![binding(0, "s", "a")],
        ),
        Err(WorkspaceLayoutError::UnusedMount { index: 1 })
    ));

    let too_many = (0..=MAX_PROFILE_SCOPE_BINDINGS)
        .map(|index| binding(index as u32, &format!("scope-{index}"), "a"))
        .collect();
    assert!(matches!(
        WorkspaceLayout::new(profile_id(), 1, vec![mount("a", "Alpha")], too_many),
        Err(WorkspaceLayoutError::ScopeBindingCount { actual })
            if actual == MAX_PROFILE_SCOPE_BINDINGS + 1
    ));
}

#[test]
fn canonical_encoder_enforces_one_mib_preimage_ceiling() {
    let mount_id = "m".repeat(128);
    let target = "t".repeat(120);
    let bindings = (0..MAX_PROFILE_SCOPE_BINDINGS)
        .map(|index| binding(index as u32, &format!("scope-{index}"), &mount_id))
        .collect();
    assert!(matches!(
        WorkspaceLayout::new(
            profile_id(),
            1,
            vec![mount(&mount_id, &target)],
            bindings,
        ),
        Err(WorkspaceLayoutError::PreimageTooLarge { actual }) if actual > 1024 * 1024
    ));
}

#[test]
fn workspace_json_cannot_bypass_validation_or_digest_recomputation() {
    let value = serde_json::to_value(fixture_layout()).expect("layout JSON");
    for mutation in [
        ("layout_version", serde_json::json!(2)),
        ("profile_revision", serde_json::json!(0)),
        (
            "layout_digest",
            serde_json::json!(format!("sha256:{}", "0".repeat(64))),
        ),
    ] {
        let mut invalid = value.clone();
        invalid[mutation.0] = mutation.1;
        assert!(serde_json::from_value::<WorkspaceLayout>(invalid).is_err());
    }

    let mut reordered = value.clone();
    reordered["mounts"].as_array_mut().unwrap().reverse();
    assert!(serde_json::from_value::<WorkspaceLayout>(reordered).is_err());

    let mut unknown = value;
    unknown["host_root"] = serde_json::json!("/mnt/locality");
    assert!(serde_json::from_value::<WorkspaceLayout>(unknown).is_err());
}

#[test]
fn session_layout_requires_valid_syntax_profile_context_and_exact_workspace() {
    let workspace = fixture_layout();
    let session = SessionLayout::from_workspace(&workspace).expect("session");
    session
        .verify_profile_context(workspace.profile_id(), workspace.profile_revision())
        .expect("profile context");
    session
        .validate_against_workspace(&workspace)
        .expect("workspace entries");
    assert!(
        session
            .verify_profile_context(workspace.profile_id(), workspace.profile_revision() + 1)
            .is_err()
    );
    assert!(
        session
            .verify_profile_context(
                &WorkspaceProfileId::new("018f4f6e-9f2c-7b1a-8c3d-4e5f60718294").unwrap(),
                workspace.profile_revision(),
            )
            .is_err()
    );

    let mut invalid = serde_json::to_value(&session).expect("session JSON");
    invalid["entries"][1]["scope_ordinal"] = serde_json::json!(3);
    assert!(serde_json::from_value::<SessionLayout>(invalid).is_err());

    let mut inconsistent = serde_json::to_value(&session).expect("session JSON");
    inconsistent["entries"][2]["target"] = serde_json::json!("Different");
    assert!(serde_json::from_value::<SessionLayout>(inconsistent).is_err());

    let altered_entry = SessionLayoutEntry::new(
        0,
        PortableMountId::new("mount-zeta").unwrap(),
        MountTarget::new("Different").unwrap(),
    );
    let mut entries = session.entries().to_vec();
    entries[0] = altered_entry;
    entries[2] = SessionLayoutEntry::new(
        2,
        PortableMountId::new("mount-zeta").unwrap(),
        MountTarget::new("Different").unwrap(),
    );
    let altered = SessionLayout::new(session.layout_digest().clone(), entries).unwrap();
    assert!(altered.validate_against_workspace(&workspace).is_err());
}
