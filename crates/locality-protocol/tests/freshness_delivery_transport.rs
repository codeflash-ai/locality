use locality_core::portable::{
    ContentVersionId, LogicalPath, ProjectionId, SourceConnectionId, SourceGenerationId,
};
use locality_core::workspace_layout::PortableMountId;
use locality_protocol::freshness_delivery::{
    GENERATION_DELTA_RECEIPT_V1_GOLDEN_JSON, GenerationDeltaTerminalReceipt, GenerationFileIdentity,
};
use locality_protocol::freshness_delivery_transport::{
    GENERATION_BODY_WINDOW_METADATA_V1_GOLDEN_JSON, GENERATION_BODY_WINDOW_REQUEST_V1_GOLDEN_JSON,
    GENERATION_DELIVERY_ACKNOWLEDGMENT_V1_GOLDEN_JSON, GENERATION_DELIVERY_REQUEST_V1_GOLDEN_JSON,
    GENERATION_PIN_LEASE_V1_GOLDEN_JSON, GENERATION_TRANSPORT_CAPABILITIES_V1_GOLDEN_JSON,
    GENERATION_TRANSPORT_FORMAT_VERSION, GENERATION_TRANSPORT_READER_VERSION, GenerationBodyRange,
    GenerationBodyWindowCapability, GenerationBodyWindowMetadata, GenerationBodyWindowRequest,
    GenerationDeliveryAcknowledgment, GenerationDeliveryAcknowledgmentRequest,
    GenerationDeliveryRequest, GenerationPinFallbackPolicy, GenerationPinLeaseAcquireRequest,
    GenerationPinLeaseAcquireResponse, GenerationPinLeaseCapability, GenerationPinLeaseRelease,
    GenerationPinLeaseReleaseRequest, GenerationPinLeaseRenewRequest, GenerationPinLeaseRenewal,
    GenerationTransportCapabilities, GenerationTransportContractError,
    MAX_GENERATION_BODY_WINDOW_BYTES, MAX_GENERATION_PIN_LEASE_SECONDS,
    MAX_GENERATION_PIN_LEASES_PER_DEVICE, MAX_GENERATION_TRANSPORT_REQUEST_BYTES,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

fn pretty_json(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("serialize JSON");
    bytes.push(b'\n');
    bytes
}

fn capabilities() -> GenerationTransportCapabilities {
    GenerationTransportCapabilities::decode_json(GENERATION_TRANSPORT_CAPABILITIES_V1_GOLDEN_JSON)
        .expect("capabilities fixture")
}

fn body_request() -> GenerationBodyWindowRequest {
    serde_json::from_slice(GENERATION_BODY_WINDOW_REQUEST_V1_GOLDEN_JSON)
        .expect("body request fixture")
}

fn body_metadata() -> GenerationBodyWindowMetadata {
    serde_json::from_slice(GENERATION_BODY_WINDOW_METADATA_V1_GOLDEN_JSON)
        .expect("body metadata fixture")
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct AcknowledgmentGolden {
    request: GenerationDeliveryAcknowledgmentRequest,
    response: GenerationDeliveryAcknowledgment,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PinLeaseGolden {
    acquire_request: GenerationPinLeaseAcquireRequest,
    acquire_response: GenerationPinLeaseAcquireResponse,
    renew_request: GenerationPinLeaseRenewRequest,
    renewal: GenerationPinLeaseRenewal,
    release_request: GenerationPinLeaseReleaseRequest,
    release: GenerationPinLeaseRelease,
    quota_unavailable: GenerationPinLeaseAcquireResponse,
}

#[test]
fn transport_contracts_match_exact_lf_json_goldens() {
    let capabilities = capabilities();
    capabilities.validate().expect("capabilities");
    assert_eq!(
        pretty_json(&capabilities),
        GENERATION_TRANSPORT_CAPABILITIES_V1_GOLDEN_JSON
    );
    let delivery_request =
        GenerationDeliveryRequest::decode_json(GENERATION_DELIVERY_REQUEST_V1_GOLDEN_JSON)
            .expect("delivery request fixture");
    delivery_request.validate().expect("delivery request");
    assert_eq!(
        pretty_json(&delivery_request),
        GENERATION_DELIVERY_REQUEST_V1_GOLDEN_JSON
    );

    let request = body_request();
    request.validate().expect("body request");
    assert_eq!(
        pretty_json(&request),
        GENERATION_BODY_WINDOW_REQUEST_V1_GOLDEN_JSON
    );
    let metadata = body_metadata();
    metadata.validate_against(&request).expect("body metadata");
    metadata.validate_body(b"hello wo").expect("body integrity");
    assert_eq!(
        pretty_json(&metadata),
        GENERATION_BODY_WINDOW_METADATA_V1_GOLDEN_JSON
    );

    let acknowledgment: AcknowledgmentGolden =
        serde_json::from_slice(GENERATION_DELIVERY_ACKNOWLEDGMENT_V1_GOLDEN_JSON)
            .expect("acknowledgment fixture");
    GenerationDeliveryAcknowledgmentRequest::decode_json(
        &serde_json::to_vec(&acknowledgment.request).unwrap(),
    )
    .expect("bounded acknowledgment request");
    let receipt: GenerationDeltaTerminalReceipt =
        serde_json::from_slice(GENERATION_DELTA_RECEIPT_V1_GOLDEN_JSON).expect("receipt fixture");
    acknowledgment
        .request
        .validate_against_receipt(&receipt)
        .expect("ack request");
    acknowledgment
        .response
        .validate_against(&acknowledgment.request)
        .expect("ack response");
    assert_eq!(
        pretty_json(&acknowledgment),
        GENERATION_DELIVERY_ACKNOWLEDGMENT_V1_GOLDEN_JSON
    );

    let pins: PinLeaseGolden =
        serde_json::from_slice(GENERATION_PIN_LEASE_V1_GOLDEN_JSON).expect("pin fixture");
    GenerationPinLeaseAcquireRequest::decode_json(
        &serde_json::to_vec(&pins.acquire_request).unwrap(),
    )
    .expect("bounded acquire request");
    GenerationPinLeaseRenewRequest::decode_json(&serde_json::to_vec(&pins.renew_request).unwrap())
        .expect("bounded renew request");
    GenerationPinLeaseReleaseRequest::decode_json(
        &serde_json::to_vec(&pins.release_request).unwrap(),
    )
    .expect("bounded release request");
    pins.acquire_response
        .validate_against(&pins.acquire_request)
        .expect("acquire");
    pins.renewal
        .validate_against(&pins.renew_request)
        .expect("renew");
    pins.release
        .validate_against(&pins.release_request)
        .expect("release");
    pins.quota_unavailable
        .validate_against(&pins.acquire_request)
        .expect("quota unavailable");
    assert_eq!(pretty_json(&pins), GENERATION_PIN_LEASE_V1_GOLDEN_JSON);
}

#[test]
fn body_windows_bind_identity_range_and_integrity_with_hard_limits() {
    let request = body_request();
    let metadata = body_metadata();

    let mut changed = metadata.clone();
    changed.content.content_version_id = ContentVersionId::new("substituted");
    assert_eq!(
        changed.validate_against(&request),
        Err(GenerationTransportContractError::BodyWindowMismatch)
    );

    let mut skipped = metadata.clone();
    skipped.range.offset = 1;
    assert_eq!(
        skipped.validate_against(&request),
        Err(GenerationTransportContractError::BodyWindowMismatch)
    );

    let mut false_terminal = metadata.clone();
    false_terminal.range.complete = true;
    assert_eq!(
        false_terminal.validate_against(&request),
        Err(GenerationTransportContractError::InvalidBodyRange)
    );
    assert_eq!(
        metadata.validate_body(b"evilbody"),
        Err(GenerationTransportContractError::BodyIntegrityMismatch)
    );

    let mut oversized = request.clone();
    oversized.max_bytes = MAX_GENERATION_BODY_WINDOW_BYTES + 1;
    assert_eq!(
        oversized.validate(),
        Err(GenerationTransportContractError::InvalidBodyWindowLimit {
            actual: MAX_GENERATION_BODY_WINDOW_BYTES + 1,
        })
    );
    assert!(matches!(
        GenerationBodyWindowRequest::decode_json(&vec![
            b' ';
            MAX_GENERATION_TRANSPORT_REQUEST_BYTES + 1
        ]),
        Err(GenerationTransportContractError::EncodingTooLarge { .. })
    ));
}

#[test]
fn capabilities_are_explicit_bounded_and_selection_is_a_subset() {
    let offered = capabilities();
    let selected = GenerationTransportCapabilities {
        body_windows: Some(GenerationBodyWindowCapability {
            max_window_bytes: 256 * 1024,
        }),
        terminal_receipt_acknowledgments: true,
        generation_pin_leases: Some(GenerationPinLeaseCapability {
            min_lease_seconds: 600,
            max_lease_seconds: 1800,
            max_active_leases_per_device: 4,
            fallback_policies: vec![GenerationPinFallbackPolicy::RequireExact],
        }),
        ..GenerationTransportCapabilities::legacy()
    };
    selected
        .validate_selection(&offered)
        .expect("selected subset");

    let mut not_offered = selected.clone();
    not_offered.body_windows.as_mut().unwrap().max_window_bytes = 2 * 1024 * 1024;
    assert_eq!(
        not_offered.validate_selection(&offered),
        Err(GenerationTransportContractError::CapabilityNotOffered)
    );

    let mut quota = selected.clone();
    quota
        .generation_pin_leases
        .as_mut()
        .unwrap()
        .max_active_leases_per_device = MAX_GENERATION_PIN_LEASES_PER_DEVICE + 1;
    assert_eq!(
        quota.validate(),
        Err(GenerationTransportContractError::InvalidPinCapability)
    );
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct PriorGenerationDeliveryRequest {
    mount_id: PortableMountId,
    source_connection_id: SourceConnectionId,
    observed_generation_id: SourceGenerationId,
}

#[test]
fn additive_versions_and_legacy_delivery_requests_decode_tolerantly() {
    let legacy_json = json!({
        "mount_id": "mount-alpha",
        "source_connection_id": "source-018f4f6e",
        "observed_generation_id": "generation-0007"
    });
    let decoded: GenerationDeliveryRequest = serde_json::from_value(legacy_json).unwrap();
    decoded.validate().expect("legacy request");
    assert_eq!(
        decoded.capabilities,
        GenerationTransportCapabilities::legacy()
    );

    let request = GenerationDeliveryRequest {
        format_version: GENERATION_TRANSPORT_FORMAT_VERSION,
        minimum_reader_version: GENERATION_TRANSPORT_READER_VERSION,
        mount_id: PortableMountId::new("mount-alpha").unwrap(),
        source_connection_id: SourceConnectionId::new("source-018f4f6e"),
        observed_generation_id: SourceGenerationId::new("generation-0007").unwrap(),
        capabilities: capabilities(),
    };
    let prior: PriorGenerationDeliveryRequest =
        serde_json::from_value(serde_json::to_value(&request).unwrap()).unwrap();
    assert_eq!(prior.mount_id, request.mount_id);

    let mut future = serde_json::to_value(&request).unwrap();
    future["format_version"] = json!(2);
    future["future_additive_field"] = json!({"safe_to_ignore": true});
    future["capabilities"]["future_capability"] = json!({"version": 1});
    let decoded: GenerationDeliveryRequest = serde_json::from_value(future).unwrap();
    decoded.validate().expect("additive future format");

    let mut update_required = serde_json::to_value(&request).unwrap();
    update_required["format_version"] = json!(2);
    update_required["minimum_reader_version"] = json!(2);
    let decoded: GenerationDeliveryRequest = serde_json::from_value(update_required).unwrap();
    assert_eq!(
        decoded.validate(),
        Err(GenerationTransportContractError::UpdateRequired {
            minimum: 2,
            supported: 1,
        })
    );
}

#[test]
fn opaque_and_content_bearing_debug_output_is_redacted() {
    let acknowledgment: AcknowledgmentGolden =
        serde_json::from_slice(GENERATION_DELIVERY_ACKNOWLEDGMENT_V1_GOLDEN_JSON).unwrap();
    let pins: PinLeaseGolden = serde_json::from_slice(GENERATION_PIN_LEASE_V1_GOLDEN_JSON).unwrap();
    let values = [
        format!("{:?}", body_request()),
        format!("{:?}", body_metadata()),
        format!("{:?}", acknowledgment.request),
        format!("{:?}", acknowledgment.response),
        format!("{:?}", pins.acquire_request),
        format!("{:?}", pins.acquire_response),
        format!("{:?}", pins.renew_request),
        format!("{:?}", pins.renewal),
        format!("{:?}", pins.release_request),
        format!("{:?}", pins.release),
    ];
    for debug in values {
        assert!(debug.contains("<redacted>"), "{debug}");
        for secret in [
            "device-scope-opaque-01",
            "lease-opaque-01",
            "Engineering/roadmap.md",
            "3ec8d4089470a2e4620d65c03a01635c028200982c94dfbc2e34eac95e2370b1",
        ] {
            assert!(!debug.contains(secret), "{debug}");
        }
    }
}

#[test]
fn pin_leases_enforce_expiry_quota_and_safe_fallback_semantics() {
    let pins: PinLeaseGolden = serde_json::from_slice(GENERATION_PIN_LEASE_V1_GOLDEN_JSON).unwrap();
    let mut excessive = pins.acquire_request.clone();
    excessive.requested_lease_seconds = MAX_GENERATION_PIN_LEASE_SECONDS + 1;
    assert_eq!(
        excessive.validate(),
        Err(GenerationTransportContractError::InvalidPinLeaseDuration {
            actual: MAX_GENERATION_PIN_LEASE_SECONDS + 1,
        })
    );

    let mut malformed_expiry = pins.renewal.clone();
    malformed_expiry.lease.expires_at = "2026-02-30T12:00:00Z".to_string();
    assert_eq!(
        malformed_expiry.validate_against(&pins.renew_request),
        Err(GenerationTransportContractError::InvalidTimestamp)
    );

    let mut wrong_device = pins.acquire_response.clone();
    let GenerationPinLeaseAcquireResponse::Granted { lease, .. } = &mut wrong_device else {
        panic!("granted fixture");
    };
    lease.device_scope_id = "another-device".to_string();
    assert_eq!(
        wrong_device.validate_against(&pins.acquire_request),
        Err(GenerationTransportContractError::PinLeaseMismatch)
    );

    let fallback_request = GenerationPinLeaseAcquireRequest {
        fallback_policy: GenerationPinFallbackPolicy::UseLatestRetained,
        ..pins.acquire_request.clone()
    };
    let mut fallback_response = pins.acquire_response.clone();
    let GenerationPinLeaseAcquireResponse::Granted {
        fallback_applied,
        lease,
        ..
    } = &mut fallback_response
    else {
        panic!("granted fixture");
    };
    *fallback_applied = true;
    lease.generation_id = SourceGenerationId::new("generation-0009").unwrap();
    fallback_response
        .validate_against(&fallback_request)
        .expect("newer retained generation remains pinned");

    let unsafe_fallback = GenerationPinLeaseAcquireRequest {
        fallback_policy: GenerationPinFallbackPolicy::RequireExact,
        ..fallback_request
    };
    assert_eq!(
        fallback_response.validate_against(&unsafe_fallback),
        Err(GenerationTransportContractError::PinLeaseMismatch)
    );
}

#[test]
fn terminal_windows_must_end_exactly_at_content_length() {
    let request = GenerationBodyWindowRequest {
        format_version: 1,
        minimum_reader_version: 1,
        delta_id: "delta-empty-check".to_string(),
        terminal_receipt_sha256:
            "sha256:3ec8d4089470a2e4620d65c03a01635c028200982c94dfbc2e34eac95e2370b1".to_string(),
        content: GenerationFileIdentity {
            projection_id: ProjectionId::new("projection-a"),
            logical_path: LogicalPath::new("file.md").unwrap(),
            content_version_id: ContentVersionId::new("content-a"),
            content_sha256:
                "sha256:7509e5bda0c762d2bac7f90d758b5b2263fa01ccbc542ab5e3df163be08e6ca9"
                    .to_string(),
            byte_length: 12,
        },
        offset: 8,
        max_bytes: 8,
    };
    let metadata = GenerationBodyWindowMetadata {
        format_version: 1,
        minimum_reader_version: 1,
        delta_id: request.delta_id.clone(),
        terminal_receipt_sha256: request.terminal_receipt_sha256.clone(),
        content: request.content.clone(),
        range: GenerationBodyRange {
            offset: 8,
            length: 4,
            complete: true,
        },
        window_sha256: "sha256:0f75130f1d4d2f3d788ec780452cb5327299f550e3fcf01dd7a6cf6d2f452076"
            .to_string(),
    };
    metadata.validate_against(&request).expect("terminal range");
}
