use locality_core::portable::{SessionId, SourceConnectionId, SourceGenerationId};
use locality_protocol::workspace_api_v2::{
    MAX_WORKSPACE_SESSION_REQUEST_V2_BYTES, WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON,
    WORKSPACE_INCOMPATIBLE_CAPABILITIES_V2_GOLDEN_JSON,
    WORKSPACE_PROFILE_SESSION_REQUEST_V2_GOLDEN_JSON, WORKSPACE_PROFILE_SESSION_V2_GOLDEN_JSON,
    WORKSPACE_SESSION_STATUS_V2_GOLDEN_JSON, WORKSPACE_UPDATE_REQUIRED_V2_GOLDEN_JSON,
    WorkspaceApiV2DecodeError, WorkspaceApiV2ValidationError, WorkspaceClientCapabilitiesV2,
    WorkspaceCompatibilityErrorV2, WorkspaceExportOfferV2, WorkspaceProfileSessionRequestV2,
    WorkspaceProfileSessionV2, WorkspaceSessionStatusV2,
};
use locality_protocol::workspace_layout::{
    SESSION_LAYOUT_V1_GOLDEN_JSON, SessionLayout, WorkspaceProfileId,
};
use locality_protocol::{
    FreshnessRequirement, ReplicaFreshnessState, ReplicaFreshnessStatus,
    SANDBOX_SESSION_STATUS_GOLDEN_JSON, SANDBOX_SESSION_STATUS_V2_GOLDEN_JSON,
    SCOPE_AUTHORIZED_COMPONENT_VERSIONS, SEALED_EXPORT_OFFER_GOLDEN_JSON,
    SESSION_PROTOCOL_ERROR_GOLDEN_JSON, SandboxSessionState, SandboxSessionStatus,
    SealedExportOffer, SessionProtocolError, StaleSessionBehavior,
    WORKSPACE_PROFILE_SESSION_GOLDEN_JSON, WorkspaceProfileSession,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

fn exact_pretty_json(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("serialize fixture");
    bytes.push(b'\n');
    bytes
}

fn assert_exact_round_trip<T>(golden: &[u8], expected: &T)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    assert!(golden.ends_with(b"\n"));
    assert!(!golden.contains(&b'\r'));
    let decoded = serde_json::from_slice::<T>(golden).expect("golden value must deserialize");
    assert_eq!(&decoded, expected);
    assert_eq!(exact_pretty_json(&decoded), golden);
}

fn capabilities() -> WorkspaceClientCapabilitiesV2 {
    WorkspaceClientCapabilitiesV2::workspace_layout_v1(true)
}

fn session() -> WorkspaceProfileSessionV2 {
    let layout: SessionLayout = serde_json::from_slice(SESSION_LAYOUT_V1_GOLDEN_JSON)
        .expect("session layout fixture must decode");
    WorkspaceProfileSessionV2::new(
        SessionId::new("session-scope-7"),
        "opaque-session-capability",
        "2026-07-29T01:00:00Z",
        WorkspaceProfileId::new("018f4f6e-9f2c-7b1a-8c3d-4e5f60718293").expect("profile ID"),
        7,
        layout,
    )
    .expect("session fixture must be valid")
}

fn freshness_requirement() -> FreshnessRequirement {
    FreshnessRequirement {
        max_age_seconds: 300,
        on_stale: StaleSessionBehavior::WaitThenFail,
        wait_timeout_seconds: 30,
    }
}

fn replica_status(
    source_connection_id: &str,
    provider_observed_through: &str,
) -> ReplicaFreshnessStatus {
    ReplicaFreshnessStatus {
        source_connection_id: SourceConnectionId::new(source_connection_id),
        state: ReplicaFreshnessState::Fresh,
        coverage_complete: true,
        provider_observed_through: Some(provider_observed_through.to_string()),
        last_successful_sync_at: Some("2026-07-19T11:58:00Z".to_string()),
        last_repair_at: Some("2026-07-19T11:55:00Z".to_string()),
        pending_events: 0,
        backlog: 0,
        provider_cooldown_until: None,
    }
}

fn status() -> WorkspaceSessionStatusV2 {
    WorkspaceSessionStatusV2::new(
        &session(),
        &capabilities(),
        SCOPE_AUTHORIZED_COMPONENT_VERSIONS,
        SandboxSessionState::Ready,
        freshness_requirement(),
        vec![
            replica_status("source-drive", "drive-repair:43"),
            replica_status("source-notion", "notion-repair:108"),
        ],
        Some(locality_protocol::ExportAttemptLimits {
            max_files: 10_000,
            max_directories: 10_000,
            max_content_bytes: 104_857_600,
        }),
        None,
        "2026-07-23T20:00:00Z",
    )
    .expect("status fixture must be valid")
}

fn sealed_offer() -> SealedExportOffer {
    let mut offer: SealedExportOffer = serde_json::from_slice(SEALED_EXPORT_OFFER_GOLDEN_JSON)
        .expect("existing sealed offer fixture must decode");
    offer.directory_count = 4;
    offer.archive_entry_count = 7;
    offer.inventory_sha256 =
        "sha256:3282a9e3a380a97d53bead78c1e025e2ed428b6627b3446ddb4266bd6d06b0c4".to_string();
    offer.writable_metadata_sha256 =
        "sha256:07fc6fceeff5dfc7e04362aa09fb29df8f76876856f90a1e645330644b746642".to_string();
    offer.source_generations[0].ordinal = 1;
    offer.source_generations.insert(
        0,
        locality_protocol::OrderedSourceGeneration {
            ordinal: 0,
            source_connection_id: SourceConnectionId::new("source-drive"),
            source_generation_id: SourceGenerationId::new("generation-drive-44")
                .expect("source generation ID"),
        },
    );
    offer
}

fn workspace_offer() -> WorkspaceExportOfferV2 {
    WorkspaceExportOfferV2::new(&session(), &status(), &capabilities(), sealed_offer())
        .expect("workspace offer fixture must be valid")
}

fn fixture_value(fixture: &[u8]) -> Value {
    serde_json::from_slice(fixture).expect("fixture JSON")
}

fn assert_request_rejected(value: &Value) {
    let bytes = serde_json::to_vec(value).expect("mutation JSON");
    assert!(WorkspaceProfileSessionRequestV2::decode_json(&bytes).is_err());
}

fn assert_session_rejected(value: &Value) {
    let bytes = serde_json::to_vec(value).expect("mutation JSON");
    assert!(WorkspaceProfileSessionV2::decode_json(&bytes).is_err());
}

fn assert_status_rejected(value: &Value) {
    let bytes = serde_json::to_vec(value).expect("mutation JSON");
    assert!(WorkspaceSessionStatusV2::decode_json(&bytes, &session(), &capabilities()).is_err());
}

fn assert_offer_rejected(value: &Value) {
    let bytes = serde_json::to_vec(value).expect("mutation JSON");
    assert!(
        WorkspaceExportOfferV2::decode_json(&bytes, &session(), &status(), &capabilities())
            .is_err()
    );
}

fn decode_status_value(
    value: &Value,
) -> Result<WorkspaceSessionStatusV2, WorkspaceApiV2DecodeError> {
    let bytes = serde_json::to_vec(value).expect("mutation JSON");
    WorkspaceSessionStatusV2::decode_json(&bytes, &session(), &capabilities())
}

fn decode_offer_value_against_status(
    value: &Value,
    status: &WorkspaceSessionStatusV2,
) -> Result<WorkspaceExportOfferV2, WorkspaceApiV2DecodeError> {
    let bytes = serde_json::to_vec(value).expect("mutation JSON");
    WorkspaceExportOfferV2::decode_json(&bytes, &session(), status, &capabilities())
}

fn assert_source_set_mismatch(error: WorkspaceApiV2DecodeError) {
    assert!(matches!(
        error,
        WorkspaceApiV2DecodeError::Contract(WorkspaceApiV2ValidationError::SourceSetMismatch)
    ));
}

fn assert_source_order_mismatch(error: WorkspaceApiV2DecodeError) {
    assert!(matches!(
        error,
        WorkspaceApiV2DecodeError::Contract(WorkspaceApiV2ValidationError::SourceOrderMismatch)
    ));
}

#[test]
fn generation_2_workspace_contracts_are_exact_golden_bytes() {
    let request = WorkspaceProfileSessionRequestV2::new(capabilities());
    assert_exact_round_trip(WORKSPACE_PROFILE_SESSION_REQUEST_V2_GOLDEN_JSON, &request);
    assert_eq!(
        WorkspaceProfileSessionRequestV2::decode_json(
            WORKSPACE_PROFILE_SESSION_REQUEST_V2_GOLDEN_JSON
        )
        .expect("bounded request decoder"),
        request
    );

    let session = session();
    assert_exact_round_trip(WORKSPACE_PROFILE_SESSION_V2_GOLDEN_JSON, &session);
    assert_eq!(
        WorkspaceProfileSessionV2::decode_json(WORKSPACE_PROFILE_SESSION_V2_GOLDEN_JSON)
            .expect("session decoder"),
        session
    );
    let debug = format!("{session:?}");
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains("opaque-session-capability"));

    let status = status();
    assert_exact_round_trip(WORKSPACE_SESSION_STATUS_V2_GOLDEN_JSON, &status);
    assert_eq!(
        WorkspaceSessionStatusV2::decode_json(
            WORKSPACE_SESSION_STATUS_V2_GOLDEN_JSON,
            &session,
            &capabilities(),
        )
        .expect("status decoder"),
        status
    );

    let offer = workspace_offer();
    assert_exact_round_trip(WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON, &offer);
    assert_eq!(
        WorkspaceExportOfferV2::decode_json(
            WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON,
            &session,
            &status,
            &capabilities(),
        )
        .expect("offer decoder"),
        offer
    );

    assert_exact_round_trip(
        WORKSPACE_UPDATE_REQUIRED_V2_GOLDEN_JSON,
        &WorkspaceCompatibilityErrorV2::update_required(
            "workspace profile requires workspace layout version 1",
        )
        .expect("update-required fixture"),
    );
    assert_exact_round_trip(
        WORKSPACE_INCOMPATIBLE_CAPABILITIES_V2_GOLDEN_JSON,
        &WorkspaceCompatibilityErrorV2::incompatible_capabilities(
            "client capabilities cannot safely open this workspace session",
            true,
        )
        .expect("incompatible-capabilities fixture"),
    );
}

#[test]
fn capability_negotiation_is_closed_bounded_and_exact() {
    let mut wrong_api_generation = fixture_value(WORKSPACE_PROFILE_SESSION_REQUEST_V2_GOLDEN_JSON);
    wrong_api_generation["api_generation"] = json!(1);
    assert_request_rejected(&wrong_api_generation);

    let mut unknown = fixture_value(WORKSPACE_PROFILE_SESSION_REQUEST_V2_GOLDEN_JSON);
    unknown["capabilities"][0]["name"] = json!("future_layout_magic");
    assert_request_rejected(&unknown);

    let base = WorkspaceProfileSessionRequestV2::new(
        WorkspaceClientCapabilitiesV2::workspace_layout_v1(false),
    );
    let mut duplicate = serde_json::to_value(&base).expect("request JSON");
    let workspace_layout = duplicate["capabilities"][0].clone();
    duplicate["capabilities"]
        .as_array_mut()
        .expect("capability array")
        .push(workspace_layout.clone());
    assert_request_rejected(&duplicate);

    let mut conflicting = serde_json::to_value(&base).expect("request JSON");
    let mut conflicting_layout = workspace_layout;
    conflicting_layout["version"] = json!(2);
    conflicting["capabilities"]
        .as_array_mut()
        .expect("capability array")
        .push(conflicting_layout);
    assert_request_rejected(&conflicting);

    let mut missing = serde_json::to_value(&base).expect("request JSON");
    missing["capabilities"]
        .as_array_mut()
        .expect("capability array")
        .remove(0);
    missing["capabilities"]
        .as_array_mut()
        .expect("capability array")
        .push(json!({"name": "freshness_wait", "version": 1}));
    assert_request_rejected(&missing);

    for (capability_index, field, incompatible) in [
        (0, "version", json!(2)),
        (1, "version", json!(2)),
        (2, "max_component_utf8_bytes", json!(254)),
        (2, "max_component_utf16_units", json!(254)),
        (2, "max_path_utf8_bytes", json!(1023)),
        (2, "max_path_utf16_units", json!(1023)),
        (4, "version", json!(2)),
    ] {
        let mut value = fixture_value(WORKSPACE_PROFILE_SESSION_REQUEST_V2_GOLDEN_JSON);
        value["capabilities"][capability_index][field] = incompatible;
        assert_request_rejected(&value);
    }

    for encodings in [
        json!([]),
        json!(["zstd", "identity"]),
        json!(["identity", "identity", "zstd"]),
    ] {
        let mut value = fixture_value(WORKSPACE_PROFILE_SESSION_REQUEST_V2_GOLDEN_JSON);
        value["capabilities"][3]["encodings"] = encodings;
        assert_request_rejected(&value);
    }
    let mut unknown_encoding = fixture_value(WORKSPACE_PROFILE_SESSION_REQUEST_V2_GOLDEN_JSON);
    unknown_encoding["capabilities"][3]["encodings"] = json!(["identity", "brotli"]);
    assert_request_rejected(&unknown_encoding);

    for encodings in [json!(["identity"]), json!(["zstd"])] {
        let mut supported_subset = fixture_value(WORKSPACE_PROFILE_SESSION_REQUEST_V2_GOLDEN_JSON);
        supported_subset["capabilities"][3]["encodings"] = encodings;
        let bytes = serde_json::to_vec(&supported_subset).expect("subset JSON");
        WorkspaceProfileSessionRequestV2::decode_json(&bytes)
            .expect("a nonempty canonical supported-encoding subset is compatible");
    }

    let mut larger_path_ceiling = fixture_value(WORKSPACE_PROFILE_SESSION_REQUEST_V2_GOLDEN_JSON);
    larger_path_ceiling["capabilities"][2]["max_component_utf8_bytes"] = json!(300);
    larger_path_ceiling["capabilities"][2]["max_component_utf16_units"] = json!(300);
    larger_path_ceiling["capabilities"][2]["max_path_utf8_bytes"] = json!(2048);
    larger_path_ceiling["capabilities"][2]["max_path_utf16_units"] = json!(2048);
    let bytes = serde_json::to_vec(&larger_path_ceiling).expect("larger ceiling JSON");
    WorkspaceProfileSessionRequestV2::decode_json(&bytes)
        .expect("larger client maxima do not broaden the frozen server contract");

    let mut too_many = fixture_value(WORKSPACE_PROFILE_SESSION_REQUEST_V2_GOLDEN_JSON);
    too_many["capabilities"]
        .as_array_mut()
        .expect("capability array")
        .push(json!({"name": "workspace_layout", "version": 1}));
    assert_request_rejected(&too_many);

    let mut oversized_capability = serde_json::to_value(&base).expect("request JSON");
    oversized_capability["capabilities"][3]["encodings"] =
        Value::Array((0..600).map(|_| json!("identity")).collect());
    assert_request_rejected(&oversized_capability);

    let mut padded = WORKSPACE_PROFILE_SESSION_REQUEST_V2_GOLDEN_JSON.to_vec();
    padded.resize(MAX_WORKSPACE_SESSION_REQUEST_V2_BYTES + 1, b' ');
    assert!(matches!(
        WorkspaceProfileSessionRequestV2::decode_json(&padded),
        Err(WorkspaceApiV2DecodeError::RequestEncodingTooLarge { .. })
    ));
}

#[test]
fn freshness_wait_is_required_only_for_waiting_freshness() {
    let without_wait = WorkspaceClientCapabilitiesV2::workspace_layout_v1(false);
    assert_eq!(
        without_wait.validate_for_freshness(&freshness_requirement()),
        Err(WorkspaceApiV2ValidationError::FreshnessWaitRequired)
    );

    let fail_fast = FreshnessRequirement {
        max_age_seconds: 300,
        on_stale: StaleSessionBehavior::Fail,
        wait_timeout_seconds: 0,
    };
    without_wait
        .validate_for_freshness(&fail_fast)
        .expect("fail-fast freshness does not require waiting support");

    assert!(
        WorkspaceSessionStatusV2::new(
            &session(),
            &without_wait,
            SCOPE_AUTHORIZED_COMPONENT_VERSIONS,
            SandboxSessionState::Ready,
            freshness_requirement(),
            vec![
                replica_status("source-drive", "drive-repair:43"),
                replica_status("source-notion", "notion-repair:108"),
            ],
            None,
            None,
            "2026-07-23T20:00:00Z",
        )
        .is_err()
    );
}

#[test]
fn client_mapping_and_absolute_root_injection_is_rejected() {
    for field in [
        "mount_id",
        "target",
        "source_scope_id",
        "scope_ordinal",
        "absolute_root",
        "host_root",
        "mounts",
        "scope_bindings",
        "entries",
        "session_layout",
        "layout_digest",
    ] {
        let mut request = fixture_value(WORKSPACE_PROFILE_SESSION_REQUEST_V2_GOLDEN_JSON);
        request[field] = json!("client-controlled");
        assert_request_rejected(&request);

        let mut session = fixture_value(WORKSPACE_PROFILE_SESSION_V2_GOLDEN_JSON);
        session[field] = json!("client-controlled");
        assert_session_rejected(&session);

        let mut status = fixture_value(WORKSPACE_SESSION_STATUS_V2_GOLDEN_JSON);
        status[field] = json!("client-controlled");
        assert_status_rejected(&status);

        let mut offer = fixture_value(WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON);
        offer[field] = json!("client-controlled");
        assert_offer_rejected(&offer);
    }

    for field in [
        "profile_id",
        "profile_revision",
        "session_id",
        "export_attempt_id",
    ] {
        let mut request = fixture_value(WORKSPACE_PROFILE_SESSION_REQUEST_V2_GOLDEN_JSON);
        request[field] = json!("client-controlled");
        assert_request_rejected(&request);
    }

    let mut capability_injection = fixture_value(WORKSPACE_PROFILE_SESSION_REQUEST_V2_GOLDEN_JSON);
    capability_injection["capabilities"][0]["absolute_root"] = json!("/mnt/locality");
    assert_request_rejected(&capability_injection);

    let mut nested_offer_injection = fixture_value(WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON);
    nested_offer_injection["offer"]["mount_id"] = json!("client-mount");
    assert_offer_rejected(&nested_offer_injection);

    let mut nested_generation_injection = fixture_value(WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON);
    nested_generation_injection["offer"]["source_generations"][0]["absolute_root"] =
        json!("/mnt/locality");
    assert_offer_rejected(&nested_generation_injection);

    let mut nested_limits_injection = fixture_value(WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON);
    nested_limits_injection["offer"]["limits"]["mount_id"] = json!("client-mount");
    assert_offer_rejected(&nested_limits_injection);

    let mut nested_replica_injection = fixture_value(WORKSPACE_SESSION_STATUS_V2_GOLDEN_JSON);
    nested_replica_injection["replicas"][0]["host_root"] = json!("/mnt/locality");
    assert_status_rejected(&nested_replica_injection);
}

#[test]
fn session_layout_profile_context_is_recomputed_on_decode() {
    let mut wrong_generation = fixture_value(WORKSPACE_PROFILE_SESSION_V2_GOLDEN_JSON);
    wrong_generation["api_generation"] = json!(1);
    assert_session_rejected(&wrong_generation);

    let mut wrong_profile = fixture_value(WORKSPACE_PROFILE_SESSION_V2_GOLDEN_JSON);
    wrong_profile["profile_id"] = json!("018f4f6e-9f2c-7b1a-8c3d-4e5f60718294");
    assert_session_rejected(&wrong_profile);

    let mut wrong_revision = fixture_value(WORKSPACE_PROFILE_SESSION_V2_GOLDEN_JSON);
    wrong_revision["profile_revision"] = json!(8);
    assert_session_rejected(&wrong_revision);

    let mut zero_revision = fixture_value(WORKSPACE_PROFILE_SESSION_V2_GOLDEN_JSON);
    zero_revision["profile_revision"] = json!(0);
    assert_session_rejected(&zero_revision);

    let mut wrong_layout_version = fixture_value(WORKSPACE_PROFILE_SESSION_V2_GOLDEN_JSON);
    wrong_layout_version["session_layout"]["layout_version"] = json!(2);
    assert_session_rejected(&wrong_layout_version);

    let mut wrong_digest = fixture_value(WORKSPACE_PROFILE_SESSION_V2_GOLDEN_JSON);
    wrong_digest["session_layout"]["layout_digest"] = json!(format!("sha256:{}", "0".repeat(64)));
    assert_session_rejected(&wrong_digest);

    let mut changed_mapping = fixture_value(WORKSPACE_PROFILE_SESSION_V2_GOLDEN_JSON);
    changed_mapping["session_layout"]["entries"][0]["target"] = json!("Finance");
    assert_session_rejected(&changed_mapping);
}

#[test]
fn status_and_offer_must_match_the_sealed_session_and_layout() {
    let mut wrong_status_generation = fixture_value(WORKSPACE_SESSION_STATUS_V2_GOLDEN_JSON);
    wrong_status_generation["api_generation"] = json!(1);
    assert_status_rejected(&wrong_status_generation);

    for (field, replacement) in [
        ("session_id", json!("session-other")),
        ("profile_id", json!("018f4f6e-9f2c-7b1a-8c3d-4e5f60718294")),
        ("profile_revision", json!(8)),
        ("layout_digest", json!(format!("sha256:{}", "0".repeat(64)))),
    ] {
        let mut status = fixture_value(WORKSPACE_SESSION_STATUS_V2_GOLDEN_JSON);
        status[field] = replacement;
        assert_status_rejected(&status);
    }
    let mut wrong_status_layout_version = fixture_value(WORKSPACE_SESSION_STATUS_V2_GOLDEN_JSON);
    wrong_status_layout_version["layout_version"] = json!(2);
    assert_status_rejected(&wrong_status_layout_version);

    for (field, value) in [
        ("profile_id", json!("018f4f6e-9f2c-7b1a-8c3d-4e5f60718294")),
        ("profile_revision", json!(8)),
        ("layout_digest", json!(format!("sha256:{}", "0".repeat(64)))),
    ] {
        let mut offer = fixture_value(WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON);
        offer[field] = value;
        assert_offer_rejected(&offer);
    }
    let mut wrong_offer_layout_version = fixture_value(WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON);
    wrong_offer_layout_version["layout_version"] = json!(2);
    assert_offer_rejected(&wrong_offer_layout_version);

    let mut wrong_offer_generation = fixture_value(WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON);
    wrong_offer_generation["api_generation"] = json!(1);
    assert_offer_rejected(&wrong_offer_generation);

    let mut wrong_offer_session = fixture_value(WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON);
    wrong_offer_session["offer"]["session_id"] = json!("session-other");
    assert_offer_rejected(&wrong_offer_session);

    let mut invalid_offer_id = fixture_value(WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON);
    invalid_offer_id["offer"]["export_attempt_id"] = json!("");
    assert_offer_rejected(&invalid_offer_id);

    let mut noncanonical_generation = fixture_value(WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON);
    noncanonical_generation["offer"]["source_generations"][0]["ordinal"] = json!(1);
    assert_offer_rejected(&noncanonical_generation);

    let mut invalid_inventory = fixture_value(WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON);
    invalid_inventory["offer"]["inventory_sha256"] = json!(format!("sha256:{}", "A".repeat(64)));
    assert_offer_rejected(&invalid_inventory);

    let mut identity_only_request = fixture_value(WORKSPACE_PROFILE_SESSION_REQUEST_V2_GOLDEN_JSON);
    identity_only_request["capabilities"][3]["encodings"] = json!(["identity"]);
    let identity_only_request: WorkspaceProfileSessionRequestV2 =
        serde_json::from_value(identity_only_request).expect("identity-only request");
    assert!(
        WorkspaceExportOfferV2::decode_json(
            WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON,
            &session(),
            &status(),
            identity_only_request.capabilities(),
        )
        .is_err()
    );
}

#[test]
fn status_and_offer_source_authority_sets_are_exact_and_ordered() {
    let mut empty_status_id = fixture_value(WORKSPACE_SESSION_STATUS_V2_GOLDEN_JSON);
    empty_status_id["replicas"][0]["source_connection_id"] = json!("");
    assert_status_rejected(&empty_status_id);

    let mut invalid_status_id = fixture_value(WORKSPACE_SESSION_STATUS_V2_GOLDEN_JSON);
    invalid_status_id["replicas"][0]["source_connection_id"] = json!(17);
    assert_status_rejected(&invalid_status_id);

    let mut duplicate_status_id = fixture_value(WORKSPACE_SESSION_STATUS_V2_GOLDEN_JSON);
    duplicate_status_id["replicas"][1]["source_connection_id"] = json!("source-drive");
    assert_status_rejected(&duplicate_status_id);

    let offer = fixture_value(WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON);

    let mut substituted_status = fixture_value(WORKSPACE_SESSION_STATUS_V2_GOLDEN_JSON);
    substituted_status["replicas"][1]["source_connection_id"] = json!("source-slack");
    let substituted_status =
        decode_status_value(&substituted_status).expect("substituted ID remains canonical");
    assert_source_set_mismatch(
        decode_offer_value_against_status(&offer, &substituted_status)
            .expect_err("valid but different source authority must fail"),
    );

    let mut omitted_status = fixture_value(WORKSPACE_SESSION_STATUS_V2_GOLDEN_JSON);
    omitted_status["replicas"]
        .as_array_mut()
        .expect("replica array")
        .pop();
    let omitted_status = decode_status_value(&omitted_status).expect("remaining status is valid");
    assert_source_set_mismatch(
        decode_offer_value_against_status(&offer, &omitted_status)
            .expect_err("omitted status source must fail"),
    );

    let mut added_status = fixture_value(WORKSPACE_SESSION_STATUS_V2_GOLDEN_JSON);
    let mut added_replica = added_status["replicas"][1].clone();
    added_replica["source_connection_id"] = json!("source-slack");
    added_status["replicas"]
        .as_array_mut()
        .expect("replica array")
        .push(added_replica);
    let added_status = decode_status_value(&added_status).expect("added status source is valid");
    assert_source_set_mismatch(
        decode_offer_value_against_status(&offer, &added_status)
            .expect_err("added status source must fail"),
    );

    let mut reordered_status = fixture_value(WORKSPACE_SESSION_STATUS_V2_GOLDEN_JSON);
    reordered_status["replicas"]
        .as_array_mut()
        .expect("replica array")
        .swap(0, 1);
    let reordered_status =
        decode_status_value(&reordered_status).expect("configured status order is contextual");
    assert_source_order_mismatch(
        decode_offer_value_against_status(&offer, &reordered_status)
            .expect_err("reordered status sources must fail"),
    );

    let mut substituted_offer = offer.clone();
    substituted_offer["offer"]["source_generations"][1]["source_connection_id"] =
        json!("source-slack");
    assert_source_set_mismatch(
        decode_offer_value_against_status(&substituted_offer, &status())
            .expect_err("valid offer source substitution must fail"),
    );

    let mut omitted_offer = offer.clone();
    omitted_offer["offer"]["source_generations"]
        .as_array_mut()
        .expect("source generation array")
        .pop();
    assert_source_set_mismatch(
        decode_offer_value_against_status(&omitted_offer, &status())
            .expect_err("omitted offer source must fail"),
    );

    let mut added_offer = offer.clone();
    added_offer["offer"]["source_generations"]
        .as_array_mut()
        .expect("source generation array")
        .push(json!({
            "ordinal": 2,
            "source_connection_id": "source-slack",
            "source_generation_id": "generation-slack-12"
        }));
    assert_source_set_mismatch(
        decode_offer_value_against_status(&added_offer, &status())
            .expect_err("added offer source must fail"),
    );

    let mut duplicate_offer = offer.clone();
    duplicate_offer["offer"]["source_generations"][1]["source_connection_id"] =
        json!("source-drive");
    assert_offer_rejected(&duplicate_offer);

    let mut duplicate_generation_id = offer.clone();
    duplicate_generation_id["offer"]["source_generations"][1]["source_generation_id"] =
        json!("generation-drive-44");
    assert_offer_rejected(&duplicate_generation_id);

    let mut empty_offer_id = offer.clone();
    empty_offer_id["offer"]["source_generations"][0]["source_connection_id"] = json!("");
    assert_offer_rejected(&empty_offer_id);

    let mut invalid_offer_id = offer.clone();
    invalid_offer_id["offer"]["source_generations"][0]["source_connection_id"] = json!([]);
    assert_offer_rejected(&invalid_offer_id);

    let mut noncanonical_offer = offer.clone();
    noncanonical_offer["offer"]["source_generations"]
        .as_array_mut()
        .expect("source generation array")
        .swap(0, 1);
    assert_offer_rejected(&noncanonical_offer);

    let mut reordered_offer = offer.clone();
    reordered_offer["offer"]["source_generations"]
        .as_array_mut()
        .expect("source generation array")
        .swap(0, 1);
    reordered_offer["offer"]["source_generations"][0]["ordinal"] = json!(0);
    reordered_offer["offer"]["source_generations"][1]["ordinal"] = json!(1);
    assert_source_order_mismatch(
        decode_offer_value_against_status(&reordered_offer, &status())
            .expect_err("renumbered source reorder must still fail"),
    );

    let mut empty_generation = offer;
    empty_generation["offer"]["source_generations"][0]["source_generation_id"] = json!("");
    assert_offer_rejected(&empty_generation);

    let mut missing_inventory = fixture_value(WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON);
    missing_inventory["offer"]
        .as_object_mut()
        .expect("offer object")
        .remove("inventory_sha256");
    assert_offer_rejected(&missing_inventory);
}

#[test]
fn compatibility_errors_are_strict_non_retriable_contracts() {
    assert!(WorkspaceCompatibilityErrorV2::update_required("").is_err());
    assert!(WorkspaceCompatibilityErrorV2::incompatible_capabilities("", true).is_err());

    for fixture in [
        WORKSPACE_UPDATE_REQUIRED_V2_GOLDEN_JSON,
        WORKSPACE_INCOMPATIBLE_CAPABILITIES_V2_GOLDEN_JSON,
    ] {
        let mut retriable = fixture_value(fixture);
        retriable["retriable"] = json!(true);
        assert!(serde_json::from_value::<WorkspaceCompatibilityErrorV2>(retriable).is_err());

        let mut wrong_api = fixture_value(fixture);
        wrong_api["required_api_generation"] = json!(1);
        assert!(serde_json::from_value::<WorkspaceCompatibilityErrorV2>(wrong_api).is_err());

        let mut wrong_layout = fixture_value(fixture);
        wrong_layout["minimum_layout_version"] = json!(2);
        assert!(serde_json::from_value::<WorkspaceCompatibilityErrorV2>(wrong_layout).is_err());

        let mut injected = fixture_value(fixture);
        injected["profile_id"] = json!("018f4f6e-9f2c-7b1a-8c3d-4e5f60718293");
        assert!(serde_json::from_value::<WorkspaceCompatibilityErrorV2>(injected).is_err());

        let mut incompatible_requirement = fixture_value(fixture);
        incompatible_requirement["required_capabilities"][0]["version"] = json!(2);
        assert!(
            serde_json::from_value::<WorkspaceCompatibilityErrorV2>(incompatible_requirement)
                .is_err()
        );
    }
}

fn assert_existing_fixture_round_trips<T>(fixture: &[u8])
where
    T: Serialize + DeserializeOwned,
{
    let value: T = serde_json::from_slice(fixture).expect("existing fixture must decode");
    assert_eq!(exact_pretty_json(&value), fixture);
}

#[test]
fn existing_api_v1_session_fixtures_remain_byte_exact() {
    assert_existing_fixture_round_trips::<WorkspaceProfileSession>(
        WORKSPACE_PROFILE_SESSION_GOLDEN_JSON,
    );
    assert_existing_fixture_round_trips::<SandboxSessionStatus>(SANDBOX_SESSION_STATUS_GOLDEN_JSON);
    assert_existing_fixture_round_trips::<SandboxSessionStatus>(
        SANDBOX_SESSION_STATUS_V2_GOLDEN_JSON,
    );
    assert_existing_fixture_round_trips::<SealedExportOffer>(SEALED_EXPORT_OFFER_GOLDEN_JSON);
    assert_existing_fixture_round_trips::<SessionProtocolError>(SESSION_PROTOCOL_ERROR_GOLDEN_JSON);
}
