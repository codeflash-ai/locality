use locality_core::portable::SessionId;
use locality_protocol::freshness_delivery::{
    FreshnessReasonCode, FreshnessRetry, FreshnessRetryClass,
};
use locality_protocol::freshness_delivery_transport::{
    GENERATION_DELIVERY_REQUEST_V1_GOLDEN_JSON, GenerationDeliveryRequest,
};
use locality_protocol::freshness_wait::{
    FRESHNESS_WAIT_ATTEMPT_REQUEST_V1_GOLDEN_JSON, FRESHNESS_WAIT_ATTEMPT_V1_GOLDEN_JSON,
    FreshnessWaitAggregateState, FreshnessWaitAttempt, FreshnessWaitAttemptRequest,
    FreshnessWaitContractError, FreshnessWaitSourceState, FreshnessWaitTerminal,
    FreshnessWaitTerminalOutcome, MAX_FRESHNESS_WAIT_ATTEMPT_BYTES,
    MAX_FRESHNESS_WAIT_IDEMPOTENCY_KEY_BYTES, MAX_FRESHNESS_WAIT_POLL_AFTER_SECONDS,
    MAX_FRESHNESS_WAIT_REQUEST_BYTES, MAX_FRESHNESS_WAIT_SOURCES,
};
use locality_protocol::workspace_api_v2::WorkspaceClientCapabilitiesV2;
use serde::Serialize;
use serde_json::{Value, json};

fn pretty_json(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("serialize fixture");
    bytes.push(b'\n');
    bytes
}

fn capabilities() -> WorkspaceClientCapabilitiesV2 {
    WorkspaceClientCapabilitiesV2::workspace_layout_v1(true)
}

fn request() -> FreshnessWaitAttemptRequest {
    FreshnessWaitAttemptRequest::decode_json(
        FRESHNESS_WAIT_ATTEMPT_REQUEST_V1_GOLDEN_JSON,
        &capabilities(),
    )
    .expect("request fixture")
}

fn waiting_attempt() -> FreshnessWaitAttempt {
    FreshnessWaitAttempt::decode_json(
        FRESHNESS_WAIT_ATTEMPT_V1_GOLDEN_JSON,
        &request(),
        &capabilities(),
    )
    .expect("attempt fixture")
}

fn terminal_retry(class: FreshnessRetryClass) -> FreshnessRetry {
    FreshnessRetry {
        class,
        retry_after_seconds: None,
    }
}

#[test]
fn freshness_wait_contracts_match_exact_lf_json_goldens() {
    let request = request();
    assert_eq!(
        pretty_json(&request),
        FRESHNESS_WAIT_ATTEMPT_REQUEST_V1_GOLDEN_JSON
    );
    let attempt = waiting_attempt();
    assert_eq!(pretty_json(&attempt), FRESHNESS_WAIT_ATTEMPT_V1_GOLDEN_JSON);

    let request_debug = format!("{request:?}");
    let attempt_debug = format!("{attempt:?}");
    for debug in [&request_debug, &attempt_debug] {
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("wait-export-018f4f6e"));
    }
    assert!(!attempt_debug.contains("freshness-wait-018f4f6e"));
}

#[test]
fn strict_decoders_reject_unknown_oversized_and_noncanonical_inputs() {
    let mut unknown_request: Value =
        serde_json::from_slice(FRESHNESS_WAIT_ATTEMPT_REQUEST_V1_GOLDEN_JSON).unwrap();
    unknown_request["future_field"] = json!(true);
    assert!(
        FreshnessWaitAttemptRequest::decode_json(
            &serde_json::to_vec(&unknown_request).unwrap(),
            &capabilities()
        )
        .is_err()
    );

    for missing in ["format_version", "minimum_reader_version"] {
        let mut value: Value =
            serde_json::from_slice(FRESHNESS_WAIT_ATTEMPT_REQUEST_V1_GOLDEN_JSON).unwrap();
        value.as_object_mut().unwrap().remove(missing);
        assert!(
            FreshnessWaitAttemptRequest::decode_json(
                &serde_json::to_vec(&value).unwrap(),
                &capabilities()
            )
            .is_err()
        );
    }

    for path in ["top", "source", "source_retry", "poll", "poll_retry"] {
        let mut value: Value =
            serde_json::from_slice(FRESHNESS_WAIT_ATTEMPT_V1_GOLDEN_JSON).unwrap();
        match path {
            "top" => value["future_field"] = json!(true),
            "source" => value["source_targets"][0]["future_field"] = json!(true),
            "source_retry" => value["source_targets"][0]["retry"]["future_field"] = json!(true),
            "poll" => value["poll"]["future_field"] = json!(true),
            "poll_retry" => value["poll"]["retry"]["future_field"] = json!(true),
            _ => unreachable!(),
        }
        assert!(
            FreshnessWaitAttempt::decode_json(
                &serde_json::to_vec(&value).unwrap(),
                &request(),
                &capabilities()
            )
            .is_err(),
            "{path}"
        );
    }

    let mut oversized_request = FRESHNESS_WAIT_ATTEMPT_REQUEST_V1_GOLDEN_JSON.to_vec();
    oversized_request.resize(MAX_FRESHNESS_WAIT_REQUEST_BYTES + 1, b' ');
    assert!(matches!(
        FreshnessWaitAttemptRequest::decode_json(&oversized_request, &capabilities()),
        Err(FreshnessWaitContractError::EncodingTooLarge { .. })
    ));
    let mut oversized_attempt = FRESHNESS_WAIT_ATTEMPT_V1_GOLDEN_JSON.to_vec();
    oversized_attempt.resize(MAX_FRESHNESS_WAIT_ATTEMPT_BYTES + 1, b' ');
    assert!(matches!(
        FreshnessWaitAttempt::decode_json(&oversized_attempt, &request(), &capabilities()),
        Err(FreshnessWaitContractError::EncodingTooLarge { .. })
    ));

    let mut oversized_key = request();
    oversized_key.idempotency_key = "x".repeat(MAX_FRESHNESS_WAIT_IDEMPOTENCY_KEY_BYTES + 1);
    assert!(matches!(
        oversized_key.validate(&capabilities()),
        Err(FreshnessWaitContractError::InvalidOpaqueValue(
            "idempotency_key"
        ))
    ));

    let mut numeric_epoch: Value =
        serde_json::from_slice(FRESHNESS_WAIT_ATTEMPT_V1_GOLDEN_JSON).unwrap();
    numeric_epoch["source_targets"][0]["target_epoch"] = json!(44);
    assert!(serde_json::from_value::<FreshnessWaitAttempt>(numeric_epoch).is_err());

    let mut leading_zero_epoch: Value =
        serde_json::from_slice(FRESHNESS_WAIT_ATTEMPT_V1_GOLDEN_JSON).unwrap();
    leading_zero_epoch["source_targets"][0]["target_epoch"] = json!("044");
    assert!(serde_json::from_value::<FreshnessWaitAttempt>(leading_zero_epoch).is_err());

    assert!(
        serde_json::from_value::<FreshnessWaitTerminal>(json!({
            "outcome": "failed",
            "reason": "provider_unavailable",
            "retry": {
                "class": "after_refresh",
                "retry_after_seconds": null,
                "future_field": true
            },
            "completed_at": "2026-07-31T12:00:09Z"
        }))
        .is_err()
    );
}

#[test]
fn source_targets_are_bounded_unique_canonical_and_unambiguous() {
    let mut attempt = waiting_attempt();
    attempt.source_targets[1].source_connection_id =
        attempt.source_targets[0].source_connection_id.clone();
    assert!(matches!(
        attempt.validate(),
        Err(FreshnessWaitContractError::DuplicateSource { index: 1 })
    ));

    let mut attempt = waiting_attempt();
    attempt.source_targets[1].ordinal = 2;
    assert!(matches!(
        attempt.validate(),
        Err(FreshnessWaitContractError::NonCanonicalSourceOrdinal { index: 1, .. })
    ));

    let mut attempt = waiting_attempt();
    let template = attempt.source_targets[0].clone();
    attempt.source_targets = (0..=MAX_FRESHNESS_WAIT_SOURCES)
        .map(|index| {
            let mut target = template.clone();
            target.ordinal = index as u32;
            target.source_connection_id =
                locality_core::portable::SourceConnectionId::new(format!("source-{index}"));
            target
        })
        .collect();
    assert!(matches!(
        attempt.validate(),
        Err(FreshnessWaitContractError::InvalidSourceCount { .. })
    ));

    let mut attempt = waiting_attempt();
    attempt.source_targets[0].applied_epoch = attempt.source_targets[0].target_epoch;
    assert_eq!(
        attempt.validate(),
        Err(FreshnessWaitContractError::AmbiguousSourceState)
    );

    let mut attempt = waiting_attempt();
    attempt.source_targets[1].reason = Some(FreshnessReasonCode::RefreshQueued);
    assert_eq!(
        attempt.validate(),
        Err(FreshnessWaitContractError::AmbiguousSourceState)
    );
}

#[test]
fn waiting_poll_metadata_is_bounded_and_matches_the_snapshot() {
    let mut attempt = waiting_attempt();
    attempt.poll.as_mut().unwrap().sequence = 0;
    assert_eq!(
        attempt.validate(),
        Err(FreshnessWaitContractError::InvalidPollMetadata)
    );

    let mut attempt = waiting_attempt();
    attempt.poll.as_mut().unwrap().retry.retry_after_seconds =
        Some(MAX_FRESHNESS_WAIT_POLL_AFTER_SECONDS + 1);
    assert_eq!(
        attempt.validate(),
        Err(FreshnessWaitContractError::InvalidPollMetadata)
    );

    let mut attempt = waiting_attempt();
    attempt.poll.as_mut().unwrap().observed_at = "2026-07-31T12:00:07Z".to_string();
    assert_eq!(
        attempt.validate(),
        Err(FreshnessWaitContractError::InvalidPollMetadata)
    );

    let mut attempt = waiting_attempt();
    attempt.poll.as_mut().unwrap().retry.retry_after_seconds = Some(23);
    assert_eq!(
        attempt.validate(),
        Err(FreshnessWaitContractError::InvalidPollMetadata)
    );

    let mut attempt = waiting_attempt();
    attempt.updated_at = "2026-07-31T12:00:31Z".to_string();
    attempt.poll.as_mut().unwrap().observed_at = attempt.updated_at.clone();
    assert_eq!(
        attempt.validate(),
        Err(FreshnessWaitContractError::InvalidPollMetadata)
    );

    let mut attempt = waiting_attempt();
    attempt.original_deadline_at = "2026-07-31T12:05:01Z".to_string();
    assert_eq!(
        attempt.validate(),
        Err(FreshnessWaitContractError::InvalidAttemptTimeline)
    );
}

#[test]
fn terminal_outcomes_are_distinct_and_match_per_source_progress() {
    let mut satisfied = waiting_attempt();
    for target in &mut satisfied.source_targets {
        target.applied_epoch = target.target_epoch;
        target.state = FreshnessWaitSourceState::Satisfied;
        target.reason = None;
        target.retry = None;
    }
    satisfied.state = FreshnessWaitAggregateState::Terminal;
    satisfied.poll = None;
    satisfied.updated_at = "2026-07-31T12:00:10Z".to_string();
    satisfied.terminal = Some(FreshnessWaitTerminal {
        outcome: FreshnessWaitTerminalOutcome::Satisfied,
        reason: None,
        retry: terminal_retry(FreshnessRetryClass::Never),
        completed_at: satisfied.updated_at.clone(),
    });
    satisfied.validate().expect("satisfied terminal outcome");

    let mut deadline = waiting_attempt();
    deadline.state = FreshnessWaitAggregateState::Terminal;
    deadline.poll = None;
    deadline.updated_at = deadline.original_deadline_at.clone();
    deadline.terminal = Some(FreshnessWaitTerminal {
        outcome: FreshnessWaitTerminalOutcome::DeadlineExceeded,
        reason: Some(FreshnessReasonCode::RefreshProcessing),
        retry: terminal_retry(FreshnessRetryClass::AfterRefresh),
        completed_at: deadline.updated_at.clone(),
    });
    deadline.validate().expect("deadline terminal outcome");

    let mut failed = waiting_attempt();
    failed.source_targets[0].state = FreshnessWaitSourceState::Failed;
    failed.source_targets[0].reason = Some(FreshnessReasonCode::ProviderAuthenticationRequired);
    failed.source_targets[0].retry = Some(terminal_retry(FreshnessRetryClass::AfterUserAction));
    failed.state = FreshnessWaitAggregateState::Terminal;
    failed.poll = None;
    failed.updated_at = "2026-07-31T12:00:09Z".to_string();
    failed.terminal = Some(FreshnessWaitTerminal {
        outcome: FreshnessWaitTerminalOutcome::Failed,
        reason: Some(FreshnessReasonCode::ProviderAuthenticationRequired),
        retry: terminal_retry(FreshnessRetryClass::AfterUserAction),
        completed_at: failed.updated_at.clone(),
    });
    failed.validate().expect("failed terminal outcome");

    let mut ambiguous = deadline;
    ambiguous.terminal.as_mut().unwrap().outcome = FreshnessWaitTerminalOutcome::Satisfied;
    assert!(ambiguous.validate().is_err());

    let mut late_failure = failed;
    late_failure.updated_at = "2026-07-31T12:00:31Z".to_string();
    late_failure.terminal.as_mut().unwrap().completed_at = late_failure.updated_at.clone();
    assert_eq!(
        late_failure.validate(),
        Err(FreshnessWaitContractError::AmbiguousAggregateState)
    );
}

#[test]
fn idempotent_replay_cannot_change_identity_session_or_original_deadline() {
    let attempt = waiting_attempt();
    for changed in ["key", "session", "deadline"] {
        let mut request = request();
        match changed {
            "key" => request.idempotency_key = "different-key".to_string(),
            "session" => request.session_id = SessionId::new("different-session"),
            "deadline" => request.original_deadline_at = "2026-07-31T12:00:31Z".to_string(),
            _ => unreachable!(),
        }
        assert_eq!(
            attempt.validate_against(&request, &capabilities()),
            Err(FreshnessWaitContractError::AttemptBindingMismatch),
            "{changed}"
        );
    }
}

#[test]
fn capability_and_version_gates_preserve_existing_v1_clients() {
    let without_wait = WorkspaceClientCapabilitiesV2::workspace_layout_v1(false);
    assert_eq!(
        request().validate(&without_wait),
        Err(FreshnessWaitContractError::CapabilityRequired)
    );

    let legacy_delivery =
        GenerationDeliveryRequest::decode_json(GENERATION_DELIVERY_REQUEST_V1_GOLDEN_JSON)
            .expect("existing V1 generation delivery remains readable");
    legacy_delivery.validate().unwrap();

    let mut update_required: Value =
        serde_json::from_slice(FRESHNESS_WAIT_ATTEMPT_REQUEST_V1_GOLDEN_JSON).unwrap();
    update_required["format_version"] = json!(2);
    update_required["minimum_reader_version"] = json!(2);
    assert!(matches!(
        FreshnessWaitAttemptRequest::decode_json(
            &serde_json::to_vec(&update_required).unwrap(),
            &capabilities()
        ),
        Err(FreshnessWaitContractError::InvalidJson(_))
    ));
}
