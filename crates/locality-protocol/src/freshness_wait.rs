//! Capability-gated, HTTP-neutral freshness wait-attempt contracts.
//!
//! These values bind a durable multi-source wait to its caller idempotency key
//! and original deadline. They deliberately contain no tenant route, provider
//! cursor, job identity, lease token, database key, or persistence behavior.

use std::collections::BTreeSet;
use std::fmt::{Debug, Display, Formatter};

use locality_core::portable::{SessionId, SourceConnectionId};
use serde::{Deserialize, Deserializer, Serialize};

use crate::FreshnessEpoch;
use crate::freshness_delivery::{FreshnessReasonCode, FreshnessRetry, FreshnessRetryClass};
use crate::workspace_api_v2::{WORKSPACE_HTTP_API_GENERATION_V2, WorkspaceClientCapabilitiesV2};

pub const FRESHNESS_WAIT_FORMAT_VERSION: u16 = 1;
pub const FRESHNESS_WAIT_READER_VERSION: u16 = 1;
pub const MAX_FRESHNESS_WAIT_REQUEST_BYTES: usize = 4 * 1024;
pub const MAX_FRESHNESS_WAIT_ATTEMPT_BYTES: usize = 64 * 1024;
pub const MAX_FRESHNESS_WAIT_SOURCES: usize = 64;
pub const MAX_FRESHNESS_WAIT_ID_BYTES: usize = 128;
pub const MAX_FRESHNESS_WAIT_IDEMPOTENCY_KEY_BYTES: usize = 128;
pub const MAX_FRESHNESS_WAIT_DURATION_SECONDS: i64 = 5 * 60;
pub const MAX_FRESHNESS_WAIT_POLL_AFTER_SECONDS: u64 = 60 * 60;

pub const FRESHNESS_WAIT_ATTEMPT_REQUEST_V1_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/freshness-wait-attempt-request-v1.json");
pub const FRESHNESS_WAIT_ATTEMPT_V1_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/freshness-wait-attempt-v1.json");

/// Starts or resumes one durable wait. Reusing the idempotency key with a
/// different session or deadline is a contract mismatch, not a new wait.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct FreshnessWaitAttemptRequest {
    pub format_version: u16,
    pub minimum_reader_version: u16,
    pub api_generation: u16,
    pub session_id: SessionId,
    pub idempotency_key: String,
    pub original_deadline_at: String,
}

impl FreshnessWaitAttemptRequest {
    pub fn decode_json(
        input: &[u8],
        capabilities: &WorkspaceClientCapabilitiesV2,
    ) -> Result<Self, FreshnessWaitContractError> {
        validate_encoding_length(input.len(), MAX_FRESHNESS_WAIT_REQUEST_BYTES)?;
        let request: Self = serde_json::from_slice(input)
            .map_err(|error| FreshnessWaitContractError::InvalidJson(error.to_string()))?;
        request.validate(capabilities)?;
        Ok(request)
    }

    pub fn validate(
        &self,
        capabilities: &WorkspaceClientCapabilitiesV2,
    ) -> Result<(), FreshnessWaitContractError> {
        self.validate_shape()?;
        if !capabilities.supports_freshness_wait() {
            return Err(FreshnessWaitContractError::CapabilityRequired);
        }
        validate_encoded(self, MAX_FRESHNESS_WAIT_REQUEST_BYTES)
    }

    fn validate_shape(&self) -> Result<(), FreshnessWaitContractError> {
        validate_versions(self.format_version, self.minimum_reader_version)?;
        if self.api_generation != WORKSPACE_HTTP_API_GENERATION_V2 {
            return Err(FreshnessWaitContractError::UnsupportedApiGeneration {
                actual: self.api_generation,
            });
        }
        validate_opaque(
            "session_id",
            self.session_id.as_str(),
            MAX_FRESHNESS_WAIT_ID_BYTES,
        )?;
        validate_opaque(
            "idempotency_key",
            &self.idempotency_key,
            MAX_FRESHNESS_WAIT_IDEMPOTENCY_KEY_BYTES,
        )?;
        validate_timestamp(&self.original_deadline_at)
    }
}

impl Debug for FreshnessWaitAttemptRequest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FreshnessWaitAttemptRequest")
            .field("format_version", &self.format_version)
            .field("minimum_reader_version", &self.minimum_reader_version)
            .field("api_generation", &self.api_generation)
            .field("session_id", &self.session_id)
            .field("idempotency_key", &"<redacted>")
            .field("original_deadline_at", &self.original_deadline_at)
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FreshnessWaitAttemptRequestWire {
    format_version: u16,
    minimum_reader_version: u16,
    api_generation: u16,
    session_id: SessionId,
    idempotency_key: String,
    original_deadline_at: String,
}

impl<'de> Deserialize<'de> for FreshnessWaitAttemptRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = FreshnessWaitAttemptRequestWire::deserialize(deserializer)?;
        let request = Self {
            format_version: wire.format_version,
            minimum_reader_version: wire.minimum_reader_version,
            api_generation: wire.api_generation,
            session_id: wire.session_id,
            idempotency_key: wire.idempotency_key,
            original_deadline_at: wire.original_deadline_at,
        };
        request.validate_shape().map_err(serde::de::Error::custom)?;
        Ok(request)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessWaitSourceState {
    Waiting,
    Satisfied,
    Failed,
    #[serde(other)]
    Unknown,
}

/// Captured demand fence and current applied progress for one configured
/// source. Ordinals preserve the session's source order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FreshnessWaitSourceTarget {
    pub ordinal: u32,
    pub source_connection_id: SourceConnectionId,
    pub target_epoch: FreshnessEpoch,
    pub applied_epoch: FreshnessEpoch,
    pub state: FreshnessWaitSourceState,
    pub reason: Option<FreshnessReasonCode>,
    pub retry: Option<FreshnessRetry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictFreshnessRetryWire {
    class: FreshnessRetryClass,
    retry_after_seconds: Option<u64>,
}

impl From<StrictFreshnessRetryWire> for FreshnessRetry {
    fn from(wire: StrictFreshnessRetryWire) -> Self {
        Self {
            class: wire.class,
            retry_after_seconds: wire.retry_after_seconds,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FreshnessWaitSourceTargetWire {
    ordinal: u32,
    source_connection_id: SourceConnectionId,
    target_epoch: FreshnessEpoch,
    applied_epoch: FreshnessEpoch,
    state: FreshnessWaitSourceState,
    reason: Option<FreshnessReasonCode>,
    retry: Option<StrictFreshnessRetryWire>,
}

impl<'de> Deserialize<'de> for FreshnessWaitSourceTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = FreshnessWaitSourceTargetWire::deserialize(deserializer)?;
        let target = Self {
            ordinal: wire.ordinal,
            source_connection_id: wire.source_connection_id,
            target_epoch: wire.target_epoch,
            applied_epoch: wire.applied_epoch,
            state: wire.state,
            reason: wire.reason,
            retry: wire.retry.map(Into::into),
        };
        target.validate().map_err(serde::de::Error::custom)?;
        Ok(target)
    }
}

impl FreshnessWaitSourceTarget {
    pub fn validate(&self) -> Result<(), FreshnessWaitContractError> {
        validate_opaque(
            "source_connection_id",
            self.source_connection_id.as_str(),
            MAX_FRESHNESS_WAIT_ID_BYTES,
        )?;
        validate_reason(self.reason)?;
        if let Some(retry) = self.retry {
            retry
                .validate()
                .map_err(|_| FreshnessWaitContractError::InvalidRetry)?;
        }
        match self.state {
            FreshnessWaitSourceState::Waiting
                if self.applied_epoch < self.target_epoch && self.reason.is_some() =>
            {
                Ok(())
            }
            FreshnessWaitSourceState::Satisfied
                if self.applied_epoch >= self.target_epoch
                    && self.reason.is_none()
                    && self.retry.is_none() =>
            {
                Ok(())
            }
            FreshnessWaitSourceState::Failed
                if self.applied_epoch < self.target_epoch && self.reason.is_some() =>
            {
                Ok(())
            }
            FreshnessWaitSourceState::Unknown => {
                Err(FreshnessWaitContractError::UnknownSourceState)
            }
            _ => Err(FreshnessWaitContractError::AmbiguousSourceState),
        }
    }
}

/// Advice for the next short, independently authorized status read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FreshnessWaitPollMetadata {
    pub sequence: u64,
    pub observed_at: String,
    pub retry: FreshnessRetry,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FreshnessWaitPollMetadataWire {
    sequence: u64,
    observed_at: String,
    retry: StrictFreshnessRetryWire,
}

impl<'de> Deserialize<'de> for FreshnessWaitPollMetadata {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = FreshnessWaitPollMetadataWire::deserialize(deserializer)?;
        let poll = Self {
            sequence: wire.sequence,
            observed_at: wire.observed_at,
            retry: wire.retry.into(),
        };
        poll.validate().map_err(serde::de::Error::custom)?;
        Ok(poll)
    }
}

impl FreshnessWaitPollMetadata {
    pub fn validate(&self) -> Result<(), FreshnessWaitContractError> {
        validate_timestamp(&self.observed_at)?;
        self.retry
            .validate()
            .map_err(|_| FreshnessWaitContractError::InvalidRetry)?;
        if self.sequence == 0
            || self.retry.class != FreshnessRetryClass::AfterDelay
            || !matches!(
                self.retry.retry_after_seconds,
                Some(1..=MAX_FRESHNESS_WAIT_POLL_AFTER_SECONDS)
            )
        {
            return Err(FreshnessWaitContractError::InvalidPollMetadata);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessWaitTerminalOutcome {
    Satisfied,
    DeadlineExceeded,
    Failed,
    #[serde(other)]
    Unknown,
}

/// Terminal result. Deadline expiry is distinct from a provider/control-plane
/// failure so a client can render and retry it without parsing text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FreshnessWaitTerminal {
    pub outcome: FreshnessWaitTerminalOutcome,
    pub reason: Option<FreshnessReasonCode>,
    pub retry: FreshnessRetry,
    pub completed_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FreshnessWaitTerminalWire {
    outcome: FreshnessWaitTerminalOutcome,
    reason: Option<FreshnessReasonCode>,
    retry: StrictFreshnessRetryWire,
    completed_at: String,
}

impl<'de> Deserialize<'de> for FreshnessWaitTerminal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = FreshnessWaitTerminalWire::deserialize(deserializer)?;
        let terminal = Self {
            outcome: wire.outcome,
            reason: wire.reason,
            retry: wire.retry.into(),
            completed_at: wire.completed_at,
        };
        terminal.validate().map_err(serde::de::Error::custom)?;
        Ok(terminal)
    }
}

impl FreshnessWaitTerminal {
    pub fn validate(&self) -> Result<(), FreshnessWaitContractError> {
        validate_timestamp(&self.completed_at)?;
        validate_reason(self.reason)?;
        self.retry
            .validate()
            .map_err(|_| FreshnessWaitContractError::InvalidRetry)?;
        match self.outcome {
            FreshnessWaitTerminalOutcome::Satisfied
                if self.reason.is_none()
                    && self.retry.class == FreshnessRetryClass::Never
                    && self.retry.retry_after_seconds.is_none() =>
            {
                Ok(())
            }
            FreshnessWaitTerminalOutcome::DeadlineExceeded
            | FreshnessWaitTerminalOutcome::Failed
                if self.reason.is_some() =>
            {
                Ok(())
            }
            FreshnessWaitTerminalOutcome::Unknown => {
                Err(FreshnessWaitContractError::UnknownTerminalOutcome)
            }
            _ => Err(FreshnessWaitContractError::AmbiguousTerminalOutcome),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessWaitAggregateState {
    Waiting,
    Terminal,
    #[serde(other)]
    Unknown,
}

/// Durable status snapshot for one wait attempt. The attempt identity,
/// idempotency identity, session, and deadline are immutable across polls.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct FreshnessWaitAttempt {
    pub format_version: u16,
    pub minimum_reader_version: u16,
    pub api_generation: u16,
    pub wait_attempt_id: String,
    pub session_id: SessionId,
    pub idempotency_key: String,
    pub original_deadline_at: String,
    pub source_targets: Vec<FreshnessWaitSourceTarget>,
    pub state: FreshnessWaitAggregateState,
    pub poll: Option<FreshnessWaitPollMetadata>,
    pub terminal: Option<FreshnessWaitTerminal>,
    pub created_at: String,
    pub updated_at: String,
}

impl FreshnessWaitAttempt {
    pub fn decode_json(
        input: &[u8],
        request: &FreshnessWaitAttemptRequest,
        capabilities: &WorkspaceClientCapabilitiesV2,
    ) -> Result<Self, FreshnessWaitContractError> {
        validate_encoding_length(input.len(), MAX_FRESHNESS_WAIT_ATTEMPT_BYTES)?;
        let attempt: Self = serde_json::from_slice(input)
            .map_err(|error| FreshnessWaitContractError::InvalidJson(error.to_string()))?;
        attempt.validate_against(request, capabilities)?;
        Ok(attempt)
    }

    pub fn validate(&self) -> Result<(), FreshnessWaitContractError> {
        self.validate_shape()?;
        validate_encoded(self, MAX_FRESHNESS_WAIT_ATTEMPT_BYTES)
    }

    pub fn validate_against(
        &self,
        request: &FreshnessWaitAttemptRequest,
        capabilities: &WorkspaceClientCapabilitiesV2,
    ) -> Result<(), FreshnessWaitContractError> {
        request.validate(capabilities)?;
        self.validate()?;
        if self.format_version != request.format_version
            || self.minimum_reader_version != request.minimum_reader_version
            || self.api_generation != request.api_generation
            || self.session_id != request.session_id
            || self.idempotency_key != request.idempotency_key
            || self.original_deadline_at != request.original_deadline_at
        {
            return Err(FreshnessWaitContractError::AttemptBindingMismatch);
        }
        Ok(())
    }

    fn validate_shape(&self) -> Result<(), FreshnessWaitContractError> {
        validate_versions(self.format_version, self.minimum_reader_version)?;
        if self.api_generation != WORKSPACE_HTTP_API_GENERATION_V2 {
            return Err(FreshnessWaitContractError::UnsupportedApiGeneration {
                actual: self.api_generation,
            });
        }
        validate_opaque(
            "wait_attempt_id",
            &self.wait_attempt_id,
            MAX_FRESHNESS_WAIT_ID_BYTES,
        )?;
        validate_opaque(
            "session_id",
            self.session_id.as_str(),
            MAX_FRESHNESS_WAIT_ID_BYTES,
        )?;
        validate_opaque(
            "idempotency_key",
            &self.idempotency_key,
            MAX_FRESHNESS_WAIT_IDEMPOTENCY_KEY_BYTES,
        )?;
        validate_timestamp(&self.original_deadline_at)?;
        validate_timestamp(&self.created_at)?;
        validate_timestamp(&self.updated_at)?;
        let wait_duration_seconds = seconds_between(&self.created_at, &self.original_deadline_at)?;
        if self.created_at > self.updated_at
            || !(1..=MAX_FRESHNESS_WAIT_DURATION_SECONDS).contains(&wait_duration_seconds)
        {
            return Err(FreshnessWaitContractError::InvalidAttemptTimeline);
        }
        validate_source_targets(&self.source_targets)?;
        if let Some(poll) = &self.poll {
            poll.validate()?;
            let poll_after_seconds = poll
                .retry
                .retry_after_seconds
                .expect("validated poll metadata has a retry delay");
            if poll.observed_at != self.updated_at
                || seconds_between(&poll.observed_at, &self.original_deadline_at)?
                    < i64::try_from(poll_after_seconds)
                        .map_err(|_| FreshnessWaitContractError::InvalidPollMetadata)?
            {
                return Err(FreshnessWaitContractError::InvalidPollMetadata);
            }
        }
        if let Some(terminal) = &self.terminal {
            terminal.validate()?;
            if terminal.completed_at != self.updated_at {
                return Err(FreshnessWaitContractError::AmbiguousTerminalOutcome);
            }
        }
        validate_aggregate_state(self)
    }
}

impl Debug for FreshnessWaitAttempt {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FreshnessWaitAttempt")
            .field("format_version", &self.format_version)
            .field("minimum_reader_version", &self.minimum_reader_version)
            .field("api_generation", &self.api_generation)
            .field("wait_attempt_id", &"<redacted>")
            .field("session_id", &self.session_id)
            .field("idempotency_key", &"<redacted>")
            .field("original_deadline_at", &self.original_deadline_at)
            .field("source_targets", &self.source_targets)
            .field("state", &self.state)
            .field("poll", &self.poll)
            .field("terminal", &self.terminal)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FreshnessWaitAttemptWire {
    format_version: u16,
    minimum_reader_version: u16,
    api_generation: u16,
    wait_attempt_id: String,
    session_id: SessionId,
    idempotency_key: String,
    original_deadline_at: String,
    source_targets: Vec<FreshnessWaitSourceTarget>,
    state: FreshnessWaitAggregateState,
    poll: Option<FreshnessWaitPollMetadata>,
    terminal: Option<FreshnessWaitTerminal>,
    created_at: String,
    updated_at: String,
}

impl<'de> Deserialize<'de> for FreshnessWaitAttempt {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = FreshnessWaitAttemptWire::deserialize(deserializer)?;
        let attempt = Self {
            format_version: wire.format_version,
            minimum_reader_version: wire.minimum_reader_version,
            api_generation: wire.api_generation,
            wait_attempt_id: wire.wait_attempt_id,
            session_id: wire.session_id,
            idempotency_key: wire.idempotency_key,
            original_deadline_at: wire.original_deadline_at,
            source_targets: wire.source_targets,
            state: wire.state,
            poll: wire.poll,
            terminal: wire.terminal,
            created_at: wire.created_at,
            updated_at: wire.updated_at,
        };
        attempt.validate_shape().map_err(serde::de::Error::custom)?;
        Ok(attempt)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FreshnessWaitContractError {
    UpdateRequired { minimum: u16, supported: u16 },
    InvalidVersionEnvelope,
    UnsupportedApiGeneration { actual: u16 },
    CapabilityRequired,
    InvalidJson(String),
    EncodingTooLarge { actual: usize, maximum: usize },
    InvalidOpaqueValue(&'static str),
    InvalidTimestamp,
    InvalidRetry,
    InvalidPollMetadata,
    InvalidSourceCount { actual: usize },
    NonCanonicalSourceOrdinal { index: usize, actual: u32 },
    DuplicateSource { index: usize },
    UnknownReason,
    UnknownSourceState,
    AmbiguousSourceState,
    UnknownAggregateState,
    AmbiguousAggregateState,
    UnknownTerminalOutcome,
    AmbiguousTerminalOutcome,
    InvalidAttemptTimeline,
    AttemptBindingMismatch,
}

impl Display for FreshnessWaitContractError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UpdateRequired { minimum, supported } => write!(
                formatter,
                "freshness wait requires reader version {minimum}, supported version is {supported}"
            ),
            Self::InvalidVersionEnvelope => {
                formatter.write_str("invalid freshness wait version envelope")
            }
            Self::UnsupportedApiGeneration { actual } => write!(
                formatter,
                "workspace HTTP API generation {actual} cannot use freshness wait version 1"
            ),
            Self::CapabilityRequired => {
                formatter.write_str("freshness_wait capability version 1 is required")
            }
            Self::InvalidJson(error) => write!(formatter, "invalid freshness wait JSON: {error}"),
            Self::EncodingTooLarge { actual, maximum } => write!(
                formatter,
                "freshness wait encoding is {actual} bytes, exceeding {maximum}"
            ),
            Self::InvalidOpaqueValue(field) => {
                write!(formatter, "{field} is not a bounded opaque value")
            }
            Self::InvalidTimestamp => {
                formatter.write_str("invalid canonical freshness wait timestamp")
            }
            Self::InvalidRetry => formatter.write_str("invalid freshness wait retry metadata"),
            Self::InvalidPollMetadata => {
                formatter.write_str("invalid freshness wait poll metadata")
            }
            Self::InvalidSourceCount { actual } => write!(
                formatter,
                "freshness wait source count is {actual}; expected 1 through {MAX_FRESHNESS_WAIT_SOURCES}"
            ),
            Self::NonCanonicalSourceOrdinal { index, actual } => write!(
                formatter,
                "freshness wait source at index {index} has noncanonical ordinal {actual}"
            ),
            Self::DuplicateSource { index } => write!(
                formatter,
                "freshness wait source at index {index} duplicates a source connection ID"
            ),
            Self::UnknownReason => formatter.write_str("unknown freshness wait reason"),
            Self::UnknownSourceState => formatter.write_str("unknown freshness wait source state"),
            Self::AmbiguousSourceState => {
                formatter.write_str("freshness wait source state contradicts its progress metadata")
            }
            Self::UnknownAggregateState => {
                formatter.write_str("unknown aggregate freshness wait state")
            }
            Self::AmbiguousAggregateState => formatter.write_str(
                "aggregate freshness wait state contradicts its source or terminal metadata",
            ),
            Self::UnknownTerminalOutcome => {
                formatter.write_str("unknown freshness wait terminal outcome")
            }
            Self::AmbiguousTerminalOutcome => {
                formatter.write_str("freshness wait terminal outcome is ambiguous")
            }
            Self::InvalidAttemptTimeline => {
                formatter.write_str("freshness wait attempt timeline is invalid")
            }
            Self::AttemptBindingMismatch => {
                formatter.write_str("freshness wait attempt does not match its idempotent request")
            }
        }
    }
}

impl std::error::Error for FreshnessWaitContractError {}

fn validate_versions(
    format_version: u16,
    minimum_reader_version: u16,
) -> Result<(), FreshnessWaitContractError> {
    if format_version == 0 || minimum_reader_version == 0 || minimum_reader_version > format_version
    {
        return Err(FreshnessWaitContractError::InvalidVersionEnvelope);
    }
    if minimum_reader_version > FRESHNESS_WAIT_READER_VERSION {
        return Err(FreshnessWaitContractError::UpdateRequired {
            minimum: minimum_reader_version,
            supported: FRESHNESS_WAIT_READER_VERSION,
        });
    }
    Ok(())
}

fn validate_encoding_length(
    actual: usize,
    maximum: usize,
) -> Result<(), FreshnessWaitContractError> {
    if actual > maximum {
        Err(FreshnessWaitContractError::EncodingTooLarge { actual, maximum })
    } else {
        Ok(())
    }
}

fn validate_encoded<T: Serialize>(
    value: &T,
    maximum: usize,
) -> Result<(), FreshnessWaitContractError> {
    let actual = serde_json::to_vec(value)
        .expect("serializing a typed freshness wait value cannot fail")
        .len();
    validate_encoding_length(actual, maximum)
}

fn validate_opaque(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<(), FreshnessWaitContractError> {
    if value.is_empty()
        || value.len() > maximum
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        Err(FreshnessWaitContractError::InvalidOpaqueValue(field))
    } else {
        Ok(())
    }
}

fn validate_timestamp(value: &str) -> Result<(), FreshnessWaitContractError> {
    crate::validate_canonical_utc_timestamp("freshness_wait_timestamp", value)
        .map_err(|_| FreshnessWaitContractError::InvalidTimestamp)
}

fn validate_reason(reason: Option<FreshnessReasonCode>) -> Result<(), FreshnessWaitContractError> {
    if reason == Some(FreshnessReasonCode::Unknown) {
        Err(FreshnessWaitContractError::UnknownReason)
    } else {
        Ok(())
    }
}

fn validate_source_targets(
    targets: &[FreshnessWaitSourceTarget],
) -> Result<(), FreshnessWaitContractError> {
    if targets.is_empty() || targets.len() > MAX_FRESHNESS_WAIT_SOURCES {
        return Err(FreshnessWaitContractError::InvalidSourceCount {
            actual: targets.len(),
        });
    }
    let mut sources = BTreeSet::new();
    for (index, target) in targets.iter().enumerate() {
        if target.ordinal as usize != index {
            return Err(FreshnessWaitContractError::NonCanonicalSourceOrdinal {
                index,
                actual: target.ordinal,
            });
        }
        if !sources.insert(&target.source_connection_id) {
            return Err(FreshnessWaitContractError::DuplicateSource { index });
        }
        target.validate()?;
    }
    Ok(())
}

fn validate_aggregate_state(
    attempt: &FreshnessWaitAttempt,
) -> Result<(), FreshnessWaitContractError> {
    let waiting = attempt
        .source_targets
        .iter()
        .filter(|target| target.state == FreshnessWaitSourceState::Waiting)
        .count();
    let satisfied = attempt
        .source_targets
        .iter()
        .filter(|target| target.state == FreshnessWaitSourceState::Satisfied)
        .count();
    let failed = attempt
        .source_targets
        .iter()
        .filter(|target| target.state == FreshnessWaitSourceState::Failed)
        .count();
    match (
        attempt.state,
        attempt.poll.as_ref(),
        attempt.terminal.as_ref(),
    ) {
        (FreshnessWaitAggregateState::Waiting, Some(_), None)
            if waiting > 0 && failed == 0 && attempt.updated_at <= attempt.original_deadline_at =>
        {
            Ok(())
        }
        (FreshnessWaitAggregateState::Terminal, None, Some(terminal)) => match terminal.outcome {
            FreshnessWaitTerminalOutcome::Satisfied
                if satisfied == attempt.source_targets.len()
                    && terminal.completed_at <= attempt.original_deadline_at =>
            {
                Ok(())
            }
            FreshnessWaitTerminalOutcome::DeadlineExceeded
                if waiting > 0
                    && failed == 0
                    && attempt.source_targets.iter().any(|target| {
                        target.state == FreshnessWaitSourceState::Waiting
                            && target.reason == terminal.reason
                    })
                    && terminal.completed_at >= attempt.original_deadline_at =>
            {
                Ok(())
            }
            FreshnessWaitTerminalOutcome::Failed
                if failed > 0
                    && terminal.completed_at <= attempt.original_deadline_at
                    && attempt.source_targets.iter().any(|target| {
                        target.state == FreshnessWaitSourceState::Failed
                            && target.reason == terminal.reason
                    }) =>
            {
                Ok(())
            }
            _ => Err(FreshnessWaitContractError::AmbiguousAggregateState),
        },
        (FreshnessWaitAggregateState::Unknown, _, _) => {
            Err(FreshnessWaitContractError::UnknownAggregateState)
        }
        _ => Err(FreshnessWaitContractError::AmbiguousAggregateState),
    }
}

fn seconds_between(start: &str, end: &str) -> Result<i64, FreshnessWaitContractError> {
    let start = canonical_timestamp_seconds(start)?;
    let end = canonical_timestamp_seconds(end)?;
    end.checked_sub(start)
        .ok_or(FreshnessWaitContractError::InvalidPollMetadata)
}

fn canonical_timestamp_seconds(value: &str) -> Result<i64, FreshnessWaitContractError> {
    validate_timestamp(value)?;
    let bytes = value.as_bytes();
    let decimal = |range: std::ops::Range<usize>| {
        bytes[range].iter().try_fold(0_i64, |value, byte| {
            byte.is_ascii_digit()
                .then_some(value * 10 + i64::from(byte - b'0'))
        })
    };
    let mut year = decimal(0..4).ok_or(FreshnessWaitContractError::InvalidTimestamp)?;
    let month = decimal(5..7).ok_or(FreshnessWaitContractError::InvalidTimestamp)?;
    let day = decimal(8..10).ok_or(FreshnessWaitContractError::InvalidTimestamp)?;
    let hour = decimal(11..13).ok_or(FreshnessWaitContractError::InvalidTimestamp)?;
    let minute = decimal(14..16).ok_or(FreshnessWaitContractError::InvalidTimestamp)?;
    let second = decimal(17..19).ok_or(FreshnessWaitContractError::InvalidTimestamp)?;

    year -= i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days_since_epoch = era * 146_097 + day_of_era - 719_468;
    Ok(days_since_epoch * 86_400 + hour * 3_600 + minute * 60 + second)
}
