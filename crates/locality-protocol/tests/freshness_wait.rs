use locality_core::portable::{SessionId, SourceConnectionId};
use locality_protocol::freshness_delivery::{
    FreshnessReasonCode, FreshnessRetry, FreshnessRetryClass,
};
use locality_protocol::freshness_delivery_transport::{
    GENERATION_DELIVERY_REQUEST_V1_GOLDEN_JSON, GenerationDeliveryRequest,
};
use locality_protocol::freshness_wait::{
    FRESHNESS_WAIT_ATTEMPT_REQUEST_V1_GOLDEN_JSON, FRESHNESS_WAIT_ATTEMPT_V1_GOLDEN_JSON,
    FreshnessWaitAggregateState, FreshnessWaitAttempt, FreshnessWaitAttemptRequest,
    FreshnessWaitCapabilitySelection, FreshnessWaitContractError, FreshnessWaitSourceState,
    FreshnessWaitTerminal, FreshnessWaitTerminalOutcome, MAX_FRESHNESS_WAIT_ATTEMPT_BYTES,
    MAX_FRESHNESS_WAIT_IDEMPOTENCY_KEY_BYTES, MAX_FRESHNESS_WAIT_POLL_AFTER_SECONDS,
    MAX_FRESHNESS_WAIT_REQUEST_BYTES, MAX_FRESHNESS_WAIT_SOURCES,
};
use locality_protocol::workspace_api_v2::{
    WorkspaceClientCapabilitiesV2, WorkspaceClientCapabilityV2,
};
use locality_protocol::{FreshnessEpoch, StaleSessionBehavior};
use serde::Serialize;
use serde_json::{Value, json};

const SNAPSHOT_TIME: &str = "2026-07-31T12:00:08Z";

fn pretty_json(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("serialize fixture");
    bytes.push(b'\n');
    bytes
}

fn request() -> FreshnessWaitAttemptRequest {
    FreshnessWaitAttemptRequest::decode_json(FRESHNESS_WAIT_ATTEMPT_REQUEST_V1_GOLDEN_JSON)
        .expect("request fixture")
}

fn waiting_attempt() -> FreshnessWaitAttempt {
    FreshnessWaitAttempt::decode_json(
        FRESHNESS_WAIT_ATTEMPT_V1_GOLDEN_JSON,
        &request(),
        SNAPSHOT_TIME,
    )
    .expect("attempt fixture")
}

fn retry(class: FreshnessRetryClass, delay: Option<u64>) -> FreshnessRetry {
    FreshnessRetry {
        class,
        retry_after_seconds: delay,
    }
}

fn next_waiting(previous: &FreshnessWaitAttempt) -> FreshnessWaitAttempt {
    let mut next = previous.clone();
    next.sequence += 1;
    next.updated_at = "2026-07-31T12:00:10Z".to_string();
    next.poll.as_mut().unwrap().observed_at = next.updated_at.clone();
    next.source_targets[0].reason = Some(FreshnessReasonCode::RefreshApplying);
    next
}

fn satisfied_terminal(previous: &FreshnessWaitAttempt) -> FreshnessWaitAttempt {
    let mut terminal = next_waiting(previous);
    terminal.source_targets[0].applied_epoch = terminal.source_targets[0].target_epoch;
    terminal.source_targets[0].state = FreshnessWaitSourceState::Satisfied;
    terminal.source_targets[0].reason = None;
    terminal.source_targets[0].retry = None;
    terminal.state = FreshnessWaitAggregateState::Terminal;
    terminal.poll = None;
    terminal.terminal = Some(FreshnessWaitTerminal {
        outcome: FreshnessWaitTerminalOutcome::Satisfied,
        completed_at: terminal.updated_at.clone(),
    });
    terminal
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
    assert_eq!(
        FreshnessWaitAttempt::derive_original_deadline_at(
            &attempt.created_at,
            &attempt.freshness_requirement
        )
        .unwrap(),
        attempt.original_deadline_at
    );

    for debug in [format!("{request:?}"), format!("{attempt:?}")] {
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("wait-export-018f4f6e"));
        assert!(!debug.contains("freshness-wait-018f4f6e"));
    }
}

#[test]
fn request_is_strict_bounded_and_contains_only_the_client_offer() {
    let mut unknown: Value =
        serde_json::from_slice(FRESHNESS_WAIT_ATTEMPT_REQUEST_V1_GOLDEN_JSON).unwrap();
    unknown["original_deadline_at"] = json!("2099-01-01T00:00:00Z");
    assert!(
        FreshnessWaitAttemptRequest::decode_json(&serde_json::to_vec(&unknown).unwrap()).is_err()
    );

    for missing in ["format_version", "minimum_reader_version", "capabilities"] {
        let mut value: Value =
            serde_json::from_slice(FRESHNESS_WAIT_ATTEMPT_REQUEST_V1_GOLDEN_JSON).unwrap();
        value.as_object_mut().unwrap().remove(missing);
        assert!(
            FreshnessWaitAttemptRequest::decode_json(&serde_json::to_vec(&value).unwrap()).is_err()
        );
    }

    let mut without_wait: Value =
        serde_json::from_slice(FRESHNESS_WAIT_ATTEMPT_REQUEST_V1_GOLDEN_JSON).unwrap();
    without_wait["capabilities"].as_array_mut().unwrap().pop();
    assert!(
        FreshnessWaitAttemptRequest::decode_json(&serde_json::to_vec(&without_wait).unwrap())
            .is_err()
    );

    let mut oversized = FRESHNESS_WAIT_ATTEMPT_REQUEST_V1_GOLDEN_JSON.to_vec();
    oversized.resize(MAX_FRESHNESS_WAIT_REQUEST_BYTES + 1, b' ');
    assert!(matches!(
        FreshnessWaitAttemptRequest::decode_json(&oversized),
        Err(FreshnessWaitContractError::EncodingTooLarge { .. })
    ));

    let mut oversized_key = request();
    oversized_key.idempotency_key = "x".repeat(MAX_FRESHNESS_WAIT_IDEMPOTENCY_KEY_BYTES + 1);
    assert!(matches!(
        oversized_key.validate(),
        Err(FreshnessWaitContractError::InvalidOpaqueValue(
            "idempotency_key"
        ))
    ));
}

#[test]
fn authenticated_selection_is_separate_from_and_checked_against_every_offer() {
    let attempt = waiting_attempt();
    assert_eq!(
        attempt.selected_capability,
        FreshnessWaitCapabilitySelection::v1()
    );

    let mut changed_offer = request();
    changed_offer.capabilities = WorkspaceClientCapabilitiesV2::new(vec![
        WorkspaceClientCapabilityV2::WorkspaceLayout { version: 1 },
        WorkspaceClientCapabilityV2::AtomicRootPublication { version: 1 },
        WorkspaceClientCapabilityV2::PathCeilings {
            version: 1,
            max_component_utf8_bytes: 255,
            max_component_utf16_units: 255,
            max_path_utf8_bytes: 1024,
            max_path_utf16_units: 1024,
        },
        WorkspaceClientCapabilityV2::TarEncodings {
            version: 1,
            encodings: vec![locality_protocol::TarContentEncoding::Identity],
        },
        WorkspaceClientCapabilityV2::FreshnessWait { version: 1 },
    ])
    .unwrap();
    attempt
        .validate_against(&changed_offer)
        .expect("a later broader/different offer retains the immutable selected wait version");

    let mut missing_selection = changed_offer;
    missing_selection.capabilities = WorkspaceClientCapabilitiesV2::workspace_layout_v1(false);
    assert_eq!(
        attempt.validate_against(&missing_selection),
        Err(FreshnessWaitContractError::CapabilityRequired)
    );

    let mut unoffered = attempt;
    unoffered.selected_capability.version = 2;
    assert_eq!(
        unoffered.validate_against(&request()),
        Err(FreshnessWaitContractError::SelectionNotOffered)
    );
}

#[test]
fn deadline_is_derived_from_trusted_creation_and_sealed_requirement() {
    let mut attempt = waiting_attempt();
    attempt.original_deadline_at = "2026-07-31T12:00:31Z".to_string();
    assert_eq!(
        attempt.validate(),
        Err(FreshnessWaitContractError::InvalidAttemptTimeline)
    );

    let mut attempt = waiting_attempt();
    attempt.freshness_requirement.wait_timeout_seconds = 301;
    assert_eq!(
        attempt.validate(),
        Err(FreshnessWaitContractError::InvalidFreshnessRequirement)
    );

    let mut attempt = waiting_attempt();
    attempt.freshness_requirement.on_stale = StaleSessionBehavior::Fail;
    assert_eq!(
        attempt.validate(),
        Err(FreshnessWaitContractError::InvalidFreshnessRequirement)
    );

    let attempt = waiting_attempt();
    assert_eq!(
        attempt.validate_at("2026-07-31T12:00:02Z"),
        Err(FreshnessWaitContractError::TimestampBeyondAllowedSkew)
    );
    attempt
        .validate_at("2026-07-31T12:00:03Z")
        .expect("five seconds of authenticated server skew is allowed");
    assert_eq!(
        attempt.validate_at("2026-07-31T12:00:30Z"),
        Err(FreshnessWaitContractError::AttemptPastDeadline)
    );
}

#[test]
fn strict_status_decoder_rejects_unknown_oversized_and_ambiguous_inputs() {
    for path in [
        "top",
        "selection",
        "requirement",
        "source",
        "source_retry",
        "poll",
        "poll_retry",
    ] {
        let mut value: Value =
            serde_json::from_slice(FRESHNESS_WAIT_ATTEMPT_V1_GOLDEN_JSON).unwrap();
        match path {
            "top" => value["future_field"] = json!(true),
            "selection" => value["selected_capability"]["future_field"] = json!(true),
            "requirement" => value["freshness_requirement"]["future_field"] = json!(true),
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
                SNAPSHOT_TIME
            )
            .is_err(),
            "{path}"
        );
    }

    let mut oversized = FRESHNESS_WAIT_ATTEMPT_V1_GOLDEN_JSON.to_vec();
    oversized.resize(MAX_FRESHNESS_WAIT_ATTEMPT_BYTES + 1, b' ');
    assert!(matches!(
        FreshnessWaitAttempt::decode_json(&oversized, &request(), SNAPSHOT_TIME),
        Err(FreshnessWaitContractError::EncodingTooLarge { .. })
    ));

    let mut numeric_epoch: Value =
        serde_json::from_slice(FRESHNESS_WAIT_ATTEMPT_V1_GOLDEN_JSON).unwrap();
    numeric_epoch["source_targets"][0]["target_epoch"] = json!(44);
    assert!(serde_json::from_value::<FreshnessWaitAttempt>(numeric_epoch).is_err());

    let mut leading_zero_epoch: Value =
        serde_json::from_slice(FRESHNESS_WAIT_ATTEMPT_V1_GOLDEN_JSON).unwrap();
    leading_zero_epoch["source_targets"][0]["target_epoch"] = json!("044");
    assert!(serde_json::from_value::<FreshnessWaitAttempt>(leading_zero_epoch).is_err());

    for (path, label) in [
        ("source_state", "future_source_state"),
        ("aggregate_state", "future_aggregate_state"),
        ("reason", "future_reason"),
        ("retry", "future_retry_class"),
    ] {
        let mut value: Value =
            serde_json::from_slice(FRESHNESS_WAIT_ATTEMPT_V1_GOLDEN_JSON).unwrap();
        match path {
            "source_state" => value["source_targets"][0]["state"] = json!(label),
            "aggregate_state" => value["state"] = json!(label),
            "reason" => value["source_targets"][0]["reason"] = json!(label),
            "retry" => value["source_targets"][0]["retry"]["class"] = json!(label),
            _ => unreachable!(),
        }
        assert!(
            FreshnessWaitAttempt::decode_json(
                &serde_json::to_vec(&value).unwrap(),
                &request(),
                SNAPSHOT_TIME
            )
            .is_err(),
            "{path}"
        );
    }

    let mut terminal = satisfied_terminal(&waiting_attempt());
    let mut terminal_json = serde_json::to_value(&terminal).unwrap();
    terminal_json["terminal"]["reason"] = json!("provider_unavailable");
    assert!(serde_json::from_value::<FreshnessWaitAttempt>(terminal_json).is_err());

    terminal.source_targets[0].reason = Some(FreshnessReasonCode::RefreshQueued);
    assert_eq!(
        terminal.validate(),
        Err(FreshnessWaitContractError::AmbiguousSourceState)
    );
}

#[test]
fn source_targets_are_bounded_unique_and_canonical() {
    let mut duplicate = waiting_attempt();
    duplicate.source_targets[1].source_connection_id =
        duplicate.source_targets[0].source_connection_id.clone();
    assert!(matches!(
        duplicate.validate(),
        Err(FreshnessWaitContractError::DuplicateSource { index: 1 })
    ));

    let mut reordered = waiting_attempt();
    reordered.source_targets.swap(0, 1);
    assert!(matches!(
        reordered.validate(),
        Err(FreshnessWaitContractError::NonCanonicalSourceOrdinal { .. })
    ));

    let mut oversized = waiting_attempt();
    let template = oversized.source_targets[0].clone();
    oversized.source_targets = (0..=MAX_FRESHNESS_WAIT_SOURCES)
        .map(|index| {
            let mut target = template.clone();
            target.ordinal = index as u32;
            target.source_connection_id = SourceConnectionId::new(format!("source-{index}"));
            target
        })
        .collect();
    assert!(matches!(
        oversized.validate(),
        Err(FreshnessWaitContractError::InvalidSourceCount { .. })
    ));
}

#[test]
fn poll_metadata_is_bounded_and_cannot_cross_the_deadline() {
    let mut oversized = waiting_attempt();
    oversized.poll.as_mut().unwrap().retry.retry_after_seconds =
        Some(MAX_FRESHNESS_WAIT_POLL_AFTER_SECONDS + 1);
    assert_eq!(
        oversized.validate(),
        Err(FreshnessWaitContractError::InvalidPollMetadata)
    );

    let mut crosses_deadline = waiting_attempt();
    crosses_deadline
        .poll
        .as_mut()
        .unwrap()
        .retry
        .retry_after_seconds = Some(23);
    assert_eq!(
        crosses_deadline.validate(),
        Err(FreshnessWaitContractError::InvalidPollMetadata)
    );
}

#[test]
fn successor_and_exact_replay_validation_bind_all_immutable_facts() {
    let previous = waiting_attempt();
    previous
        .validate_successor(&previous, &request(), SNAPSHOT_TIME)
        .expect("exact replay");

    let successor = next_waiting(&previous);
    successor
        .validate_successor(&previous, &request(), "2026-07-31T12:00:10Z")
        .expect("monotonic successor");

    for changed in [
        "attempt",
        "session",
        "key",
        "selection",
        "requirement",
        "created",
        "deadline",
        "source",
        "order",
        "target",
    ] {
        let mut candidate = successor.clone();
        match changed {
            "attempt" => candidate.wait_attempt_id = "different-attempt".to_string(),
            "session" => candidate.session_id = SessionId::new("different-session"),
            "key" => candidate.idempotency_key = "different-key".to_string(),
            "selection" => candidate.selected_capability.version = 2,
            "requirement" => candidate.freshness_requirement.max_age_seconds += 1,
            "created" => {
                candidate.created_at = "2026-07-31T12:00:01Z".to_string();
                candidate.original_deadline_at = "2026-07-31T12:00:31Z".to_string();
            }
            "deadline" => {
                candidate.freshness_requirement.wait_timeout_seconds += 1;
                candidate.original_deadline_at = "2026-07-31T12:00:31Z".to_string();
            }
            "source" => {
                candidate.source_targets[0].source_connection_id =
                    SourceConnectionId::new("different-source")
            }
            "order" => candidate.source_targets.swap(0, 1),
            "target" => candidate.source_targets[0].target_epoch = FreshnessEpoch::new(45).unwrap(),
            _ => unreachable!(),
        }
        assert!(
            candidate
                .validate_successor(&previous, &request(), "2026-07-31T12:00:10Z")
                .is_err(),
            "{changed}"
        );
    }
}

#[test]
fn successor_rejects_sequence_time_epoch_regression_and_terminal_mutation() {
    let previous = waiting_attempt();
    let mut skipped = next_waiting(&previous);
    skipped.sequence += 1;
    assert_eq!(
        skipped.validate_successor(&previous, &request(), "2026-07-31T12:00:10Z"),
        Err(FreshnessWaitContractError::NonMonotonicAttemptSequence)
    );

    let mut same_time = next_waiting(&previous);
    same_time.updated_at = previous.updated_at.clone();
    same_time.poll.as_mut().unwrap().observed_at = same_time.updated_at.clone();
    assert_eq!(
        same_time.validate_successor(&previous, &request(), SNAPSHOT_TIME),
        Err(FreshnessWaitContractError::NonMonotonicAttemptTime)
    );

    let newer_previous = previous.clone();
    let mut regressed = next_waiting(&newer_previous);
    regressed.source_targets[0].applied_epoch = FreshnessEpoch::new(42).unwrap();
    regressed.source_targets[0].state = FreshnessWaitSourceState::Waiting;
    regressed.source_targets[0].reason = Some(FreshnessReasonCode::RefreshProcessing);
    regressed.source_targets[0].retry = Some(retry(FreshnessRetryClass::AfterDelay, Some(2)));
    assert_eq!(
        regressed.validate_successor(&newer_previous, &request(), "2026-07-31T12:00:10Z"),
        Err(FreshnessWaitContractError::AppliedEpochRegressed)
    );

    let mut reopened_source = next_waiting(&previous);
    reopened_source.source_targets[1].applied_epoch = FreshnessEpoch::new(108).unwrap();
    reopened_source.source_targets[1].state = FreshnessWaitSourceState::Waiting;
    reopened_source.source_targets[1].reason = Some(FreshnessReasonCode::RefreshProcessing);
    reopened_source.source_targets[1].retry = Some(retry(FreshnessRetryClass::AfterDelay, Some(2)));
    assert_eq!(
        reopened_source.validate_successor(&previous, &request(), "2026-07-31T12:00:10Z"),
        Err(FreshnessWaitContractError::SourceTerminalStateChanged)
    );

    let terminal = satisfied_terminal(&previous);
    terminal
        .validate_successor(&previous, &request(), "2026-07-31T12:00:10Z")
        .expect("waiting may become terminal");
    let mut changed_terminal = terminal.clone();
    changed_terminal.sequence += 1;
    changed_terminal.updated_at = "2026-07-31T12:00:11Z".to_string();
    changed_terminal.terminal.as_mut().unwrap().completed_at = changed_terminal.updated_at.clone();
    assert_eq!(
        changed_terminal.validate_successor(&terminal, &request(), "2026-07-31T12:00:11Z"),
        Err(FreshnessWaitContractError::TerminalAttemptChanged)
    );
}

#[test]
fn terminal_outcomes_use_only_ordered_source_reason_retry_pairs() {
    let previous = waiting_attempt();
    satisfied_terminal(&previous).validate().unwrap();

    let mut deadline = previous.clone();
    deadline.sequence += 1;
    deadline.state = FreshnessWaitAggregateState::Terminal;
    deadline.poll = None;
    deadline.updated_at = deadline.original_deadline_at.clone();
    deadline.terminal = Some(FreshnessWaitTerminal {
        outcome: FreshnessWaitTerminalOutcome::DeadlineExceeded,
        completed_at: deadline.updated_at.clone(),
    });
    deadline.validate().unwrap();

    let mut failed = previous;
    failed.sequence += 1;
    failed.source_targets[0].state = FreshnessWaitSourceState::Failed;
    failed.source_targets[0].reason = Some(FreshnessReasonCode::ProviderAuthenticationRequired);
    failed.source_targets[0].retry = Some(retry(FreshnessRetryClass::AfterUserAction, None));
    failed.state = FreshnessWaitAggregateState::Terminal;
    failed.poll = None;
    failed.updated_at = "2026-07-31T12:00:09Z".to_string();
    failed.terminal = Some(FreshnessWaitTerminal {
        outcome: FreshnessWaitTerminalOutcome::Failed,
        completed_at: failed.updated_at.clone(),
    });
    failed.validate().unwrap();
    assert_eq!(
        failed.source_targets[0].retry,
        Some(retry(FreshnessRetryClass::AfterUserAction, None))
    );

    let mut ambiguous = deadline;
    ambiguous.terminal.as_mut().unwrap().outcome = FreshnessWaitTerminalOutcome::Satisfied;
    assert_eq!(
        ambiguous.validate(),
        Err(FreshnessWaitContractError::AmbiguousAggregateState)
    );

    let mut late_failure = failed;
    late_failure.updated_at = "2026-07-31T12:00:31Z".to_string();
    late_failure.terminal.as_mut().unwrap().completed_at = late_failure.updated_at.clone();
    assert_eq!(
        late_failure.validate(),
        Err(FreshnessWaitContractError::AmbiguousAggregateState)
    );
}

#[test]
fn capability_and_version_gates_preserve_existing_v1_clients() {
    let legacy_delivery =
        GenerationDeliveryRequest::decode_json(GENERATION_DELIVERY_REQUEST_V1_GOLDEN_JSON)
            .expect("existing V1 generation delivery remains readable");
    legacy_delivery.validate().unwrap();

    let mut update_required: Value =
        serde_json::from_slice(FRESHNESS_WAIT_ATTEMPT_REQUEST_V1_GOLDEN_JSON).unwrap();
    update_required["format_version"] = json!(2);
    update_required["minimum_reader_version"] = json!(2);
    assert!(
        FreshnessWaitAttemptRequest::decode_json(&serde_json::to_vec(&update_required).unwrap())
            .is_err()
    );
}
