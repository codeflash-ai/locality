//! Capability-gated, HTTP-neutral freshness wait-attempt contracts.
//!
//! The client offers workspace capabilities. An authenticated response selects
//! `freshness_wait: 1`, binds a trusted creation time and sealed freshness
//! requirement, and derives one immutable server deadline. These values contain
//! no tenant route, provider cursor, job identity, lease token, or persistence.

use std::collections::BTreeSet;
use std::fmt::{Debug, Display, Formatter};

use locality_core::portable::{SessionId, SourceConnectionId};
use serde::{Deserialize, Deserializer, Serialize};

use crate::freshness_delivery::{FreshnessReasonCode, FreshnessRetry, FreshnessRetryClass};
use crate::workspace_api_v2::{
    WORKSPACE_CAPABILITY_VERSION_V1, WORKSPACE_HTTP_API_GENERATION_V2,
    WorkspaceClientCapabilitiesV2,
};
use crate::{FreshnessEpoch, FreshnessRequirement, StaleSessionBehavior};

pub const FRESHNESS_WAIT_FORMAT_VERSION: u16 = 1;
pub const FRESHNESS_WAIT_READER_VERSION: u16 = 1;
pub const MAX_FRESHNESS_WAIT_REQUEST_BYTES: usize = 4 * 1024;
pub const MAX_FRESHNESS_WAIT_ATTEMPT_BYTES: usize = 64 * 1024;
pub const MAX_FRESHNESS_WAIT_SOURCES: usize = 64;
pub const MAX_FRESHNESS_WAIT_ID_BYTES: usize = 128;
pub const MAX_FRESHNESS_WAIT_IDEMPOTENCY_KEY_BYTES: usize = 128;
pub const MAX_FRESHNESS_WAIT_DURATION_SECONDS: u64 = 5 * 60;
pub const MAX_FRESHNESS_WAIT_POLL_AFTER_SECONDS: u64 = 60 * 60;
pub const MAX_FRESHNESS_WAIT_FUTURE_SKEW_SECONDS: i64 = 5;

pub const FRESHNESS_WAIT_ATTEMPT_REQUEST_V1_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/freshness-wait-attempt-request-v1.json");
pub const FRESHNESS_WAIT_ATTEMPT_V1_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/freshness-wait-attempt-v1.json");

/// Starts or resumes one durable wait. Capability offers are intentionally not
/// immutable attempt identity: a later poll may offer more capabilities as long
/// as it still includes the server's already-selected wait version.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct FreshnessWaitAttemptRequest {
    pub format_version: u16,
    pub minimum_reader_version: u16,
    pub api_generation: u16,
    pub session_id: SessionId,
    pub idempotency_key: String,
    pub capabilities: WorkspaceClientCapabilitiesV2,
}

impl FreshnessWaitAttemptRequest {
    pub fn decode_json(input: &[u8]) -> Result<Self, FreshnessWaitContractError> {
        validate_encoding_length(input.len(), MAX_FRESHNESS_WAIT_REQUEST_BYTES)?;
        let request: Self = serde_json::from_slice(input)
            .map_err(|error| FreshnessWaitContractError::InvalidJson(error.to_string()))?;
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), FreshnessWaitContractError> {
        validate_versions(self.format_version, self.minimum_reader_version)?;
        validate_api_generation(self.api_generation)?;
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
        if !self.capabilities.supports_freshness_wait() {
            return Err(FreshnessWaitContractError::CapabilityRequired);
        }
        validate_encoded(self, MAX_FRESHNESS_WAIT_REQUEST_BYTES)
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
            .field("capabilities", &self.capabilities)
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
    capabilities: WorkspaceClientCapabilitiesV2,
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
            capabilities: wire.capabilities,
        };
        request.validate().map_err(serde::de::Error::custom)?;
        Ok(request)
    }
}

/// Authenticated server selection. It is immutable once an attempt is created
/// and is checked against every later client offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FreshnessWaitCapabilitySelection {
    pub version: u16,
}

impl FreshnessWaitCapabilitySelection {
    pub fn v1() -> Self {
        Self {
            version: WORKSPACE_CAPABILITY_VERSION_V1,
        }
    }

    pub fn validate_against(
        &self,
        capabilities: &WorkspaceClientCapabilitiesV2,
    ) -> Result<(), FreshnessWaitContractError> {
        if self.version != WORKSPACE_CAPABILITY_VERSION_V1
            || capabilities.freshness_wait_version() != Some(self.version)
        {
            return Err(FreshnessWaitContractError::SelectionNotOffered);
        }
        Ok(())
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FreshnessWaitPollMetadata {
    pub observed_at: String,
    pub retry: FreshnessRetry,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FreshnessWaitPollMetadataWire {
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
        if self.retry.class != FreshnessRetryClass::AfterDelay
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

/// Aggregate terminal advice is deliberately absent. Source reason/retry pairs
/// remain authoritative and ordered; clients never receive a lossy derived
/// reason or retry tuple.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FreshnessWaitTerminal {
    pub outcome: FreshnessWaitTerminalOutcome,
    pub completed_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FreshnessWaitTerminalWire {
    outcome: FreshnessWaitTerminalOutcome,
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
            completed_at: wire.completed_at,
        };
        terminal.validate().map_err(serde::de::Error::custom)?;
        Ok(terminal)
    }
}

impl FreshnessWaitTerminal {
    pub fn validate(&self) -> Result<(), FreshnessWaitContractError> {
        validate_timestamp(&self.completed_at)?;
        if self.outcome == FreshnessWaitTerminalOutcome::Unknown {
            return Err(FreshnessWaitContractError::UnknownTerminalOutcome);
        }
        Ok(())
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

#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct FreshnessWaitAttempt {
    pub format_version: u16,
    pub minimum_reader_version: u16,
    pub api_generation: u16,
    pub selected_capability: FreshnessWaitCapabilitySelection,
    pub wait_attempt_id: String,
    pub session_id: SessionId,
    pub idempotency_key: String,
    pub freshness_requirement: FreshnessRequirement,
    pub created_at: String,
    pub original_deadline_at: String,
    pub sequence: u64,
    pub source_targets: Vec<FreshnessWaitSourceTarget>,
    pub state: FreshnessWaitAggregateState,
    pub poll: Option<FreshnessWaitPollMetadata>,
    pub terminal: Option<FreshnessWaitTerminal>,
    pub updated_at: String,
}

impl FreshnessWaitAttempt {
    pub fn decode_json(
        input: &[u8],
        request: &FreshnessWaitAttemptRequest,
        authenticated_server_time: &str,
    ) -> Result<Self, FreshnessWaitContractError> {
        validate_encoding_length(input.len(), MAX_FRESHNESS_WAIT_ATTEMPT_BYTES)?;
        let attempt: Self = serde_json::from_slice(input)
            .map_err(|error| FreshnessWaitContractError::InvalidJson(error.to_string()))?;
        attempt.validate_against_at(request, authenticated_server_time)?;
        Ok(attempt)
    }

    pub fn derive_original_deadline_at(
        created_at: &str,
        freshness_requirement: &FreshnessRequirement,
    ) -> Result<String, FreshnessWaitContractError> {
        validate_freshness_requirement(freshness_requirement)?;
        add_timestamp_seconds(created_at, freshness_requirement.wait_timeout_seconds)
    }

    pub fn validate(&self) -> Result<(), FreshnessWaitContractError> {
        validate_versions(self.format_version, self.minimum_reader_version)?;
        validate_api_generation(self.api_generation)?;
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
        validate_freshness_requirement(&self.freshness_requirement)?;
        validate_timestamp(&self.created_at)?;
        validate_timestamp(&self.original_deadline_at)?;
        validate_timestamp(&self.updated_at)?;
        if self.original_deadline_at
            != Self::derive_original_deadline_at(&self.created_at, &self.freshness_requirement)?
            || self.updated_at < self.created_at
            || self.sequence == 0
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
        validate_aggregate_state(self)?;
        validate_encoded(self, MAX_FRESHNESS_WAIT_ATTEMPT_BYTES)
    }

    pub fn validate_against(
        &self,
        request: &FreshnessWaitAttemptRequest,
    ) -> Result<(), FreshnessWaitContractError> {
        request.validate()?;
        self.validate()?;
        self.selected_capability
            .validate_against(&request.capabilities)?;
        if self.format_version != request.format_version
            || self.minimum_reader_version != request.minimum_reader_version
            || self.api_generation != request.api_generation
            || self.session_id != request.session_id
            || self.idempotency_key != request.idempotency_key
        {
            return Err(FreshnessWaitContractError::AttemptBindingMismatch);
        }
        Ok(())
    }

    pub fn validate_at(
        &self,
        authenticated_server_time: &str,
    ) -> Result<(), FreshnessWaitContractError> {
        self.validate()?;
        validate_timestamp(authenticated_server_time)?;
        if seconds_between(&self.created_at, authenticated_server_time)?
            < -MAX_FRESHNESS_WAIT_FUTURE_SKEW_SECONDS
            || seconds_between(&self.updated_at, authenticated_server_time)?
                < -MAX_FRESHNESS_WAIT_FUTURE_SKEW_SECONDS
        {
            return Err(FreshnessWaitContractError::TimestampBeyondAllowedSkew);
        }
        if self.state == FreshnessWaitAggregateState::Waiting
            && authenticated_server_time >= self.original_deadline_at.as_str()
        {
            return Err(FreshnessWaitContractError::AttemptPastDeadline);
        }
        Ok(())
    }

    pub fn validate_against_at(
        &self,
        request: &FreshnessWaitAttemptRequest,
        authenticated_server_time: &str,
    ) -> Result<(), FreshnessWaitContractError> {
        self.validate_against(request)?;
        self.validate_at(authenticated_server_time)
    }

    /// Accepts an exact replay or the next immutable snapshot. Sequence and
    /// time advance strictly, applied epochs never decrease, source targets and
    /// order cannot change, and a terminal snapshot absorbs all successors.
    pub fn validate_successor(
        &self,
        previous: &Self,
        request: &FreshnessWaitAttemptRequest,
        authenticated_server_time: &str,
    ) -> Result<(), FreshnessWaitContractError> {
        self.validate_against_at(request, authenticated_server_time)?;
        previous.validate()?;
        previous
            .selected_capability
            .validate_against(&request.capabilities)?;
        if self == previous {
            return Ok(());
        }
        if previous.state == FreshnessWaitAggregateState::Terminal {
            return Err(FreshnessWaitContractError::TerminalAttemptChanged);
        }
        if self.sequence
            != previous
                .sequence
                .checked_add(1)
                .ok_or(FreshnessWaitContractError::NonMonotonicAttemptSequence)?
        {
            return Err(FreshnessWaitContractError::NonMonotonicAttemptSequence);
        }
        if !self.same_immutable_attempt(previous) {
            return Err(FreshnessWaitContractError::AttemptBindingMismatch);
        }
        if self.updated_at <= previous.updated_at {
            return Err(FreshnessWaitContractError::NonMonotonicAttemptTime);
        }
        for (prior, next) in previous
            .source_targets
            .iter()
            .zip(self.source_targets.iter())
        {
            if prior.state == FreshnessWaitSourceState::Satisfied
                && next.state != FreshnessWaitSourceState::Satisfied
            {
                return Err(FreshnessWaitContractError::SourceTerminalStateChanged);
            }
            if prior.state == FreshnessWaitSourceState::Failed {
                return Err(FreshnessWaitContractError::SourceTerminalStateChanged);
            }
            if next.applied_epoch < prior.applied_epoch {
                return Err(FreshnessWaitContractError::AppliedEpochRegressed);
            }
        }
        Ok(())
    }

    fn same_immutable_attempt(&self, other: &Self) -> bool {
        self.format_version == other.format_version
            && self.minimum_reader_version == other.minimum_reader_version
            && self.api_generation == other.api_generation
            && self.selected_capability == other.selected_capability
            && self.wait_attempt_id == other.wait_attempt_id
            && self.session_id == other.session_id
            && self.idempotency_key == other.idempotency_key
            && self.freshness_requirement == other.freshness_requirement
            && self.created_at == other.created_at
            && self.original_deadline_at == other.original_deadline_at
            && self.source_targets.len() == other.source_targets.len()
            && self
                .source_targets
                .iter()
                .zip(other.source_targets.iter())
                .all(|(left, right)| {
                    left.ordinal == right.ordinal
                        && left.source_connection_id == right.source_connection_id
                        && left.target_epoch == right.target_epoch
                })
    }
}

impl Debug for FreshnessWaitAttempt {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FreshnessWaitAttempt")
            .field("format_version", &self.format_version)
            .field("minimum_reader_version", &self.minimum_reader_version)
            .field("api_generation", &self.api_generation)
            .field("selected_capability", &self.selected_capability)
            .field("wait_attempt_id", &"<redacted>")
            .field("session_id", &self.session_id)
            .field("idempotency_key", &"<redacted>")
            .field("freshness_requirement", &self.freshness_requirement)
            .field("created_at", &self.created_at)
            .field("original_deadline_at", &self.original_deadline_at)
            .field("sequence", &self.sequence)
            .field("source_targets", &self.source_targets)
            .field("state", &self.state)
            .field("poll", &self.poll)
            .field("terminal", &self.terminal)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictFreshnessRequirementWire {
    max_age_seconds: u64,
    on_stale: StaleSessionBehavior,
    wait_timeout_seconds: u64,
}

impl From<StrictFreshnessRequirementWire> for FreshnessRequirement {
    fn from(wire: StrictFreshnessRequirementWire) -> Self {
        Self {
            max_age_seconds: wire.max_age_seconds,
            on_stale: wire.on_stale,
            wait_timeout_seconds: wire.wait_timeout_seconds,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FreshnessWaitAttemptWire {
    format_version: u16,
    minimum_reader_version: u16,
    api_generation: u16,
    selected_capability: FreshnessWaitCapabilitySelection,
    wait_attempt_id: String,
    session_id: SessionId,
    idempotency_key: String,
    freshness_requirement: StrictFreshnessRequirementWire,
    created_at: String,
    original_deadline_at: String,
    sequence: u64,
    source_targets: Vec<FreshnessWaitSourceTarget>,
    state: FreshnessWaitAggregateState,
    poll: Option<FreshnessWaitPollMetadata>,
    terminal: Option<FreshnessWaitTerminal>,
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
            selected_capability: wire.selected_capability,
            wait_attempt_id: wire.wait_attempt_id,
            session_id: wire.session_id,
            idempotency_key: wire.idempotency_key,
            freshness_requirement: wire.freshness_requirement.into(),
            created_at: wire.created_at,
            original_deadline_at: wire.original_deadline_at,
            sequence: wire.sequence,
            source_targets: wire.source_targets,
            state: wire.state,
            poll: wire.poll,
            terminal: wire.terminal,
            updated_at: wire.updated_at,
        };
        attempt.validate().map_err(serde::de::Error::custom)?;
        Ok(attempt)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FreshnessWaitContractError {
    UpdateRequired { minimum: u16, supported: u16 },
    InvalidVersionEnvelope,
    UnsupportedApiGeneration { actual: u16 },
    CapabilityRequired,
    SelectionNotOffered,
    InvalidJson(String),
    EncodingTooLarge { actual: usize, maximum: usize },
    InvalidOpaqueValue(&'static str),
    InvalidTimestamp,
    InvalidFreshnessRequirement,
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
    TimestampBeyondAllowedSkew,
    AttemptPastDeadline,
    AttemptBindingMismatch,
    NonMonotonicAttemptSequence,
    NonMonotonicAttemptTime,
    AppliedEpochRegressed,
    SourceTerminalStateChanged,
    TerminalAttemptChanged,
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
            Self::SelectionNotOffered => {
                formatter.write_str("server freshness wait selection was not offered by the client")
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
            Self::InvalidFreshnessRequirement => {
                formatter.write_str("invalid sealed freshness wait requirement")
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
            Self::TimestampBeyondAllowedSkew => {
                formatter.write_str("freshness wait timestamp is beyond allowed server clock skew")
            }
            Self::AttemptPastDeadline => formatter
                .write_str("waiting freshness attempt is already past its durable deadline"),
            Self::AttemptBindingMismatch => {
                formatter.write_str("freshness wait attempt changed immutable identity or targets")
            }
            Self::NonMonotonicAttemptSequence => {
                formatter.write_str("freshness wait sequence is not the exact successor")
            }
            Self::NonMonotonicAttemptTime => {
                formatter.write_str("freshness wait update time did not advance")
            }
            Self::AppliedEpochRegressed => {
                formatter.write_str("freshness wait applied epoch regressed")
            }
            Self::SourceTerminalStateChanged => {
                formatter.write_str("freshness wait source terminal state changed")
            }
            Self::TerminalAttemptChanged => {
                formatter.write_str("terminal freshness wait attempt is absorbing")
            }
        }
    }
}

impl std::error::Error for FreshnessWaitContractError {}

fn validate_versions(format: u16, minimum: u16) -> Result<(), FreshnessWaitContractError> {
    if format == 0 || minimum == 0 || minimum > format {
        return Err(FreshnessWaitContractError::InvalidVersionEnvelope);
    }
    if minimum > FRESHNESS_WAIT_READER_VERSION {
        return Err(FreshnessWaitContractError::UpdateRequired {
            minimum,
            supported: FRESHNESS_WAIT_READER_VERSION,
        });
    }
    Ok(())
}

fn validate_api_generation(actual: u16) -> Result<(), FreshnessWaitContractError> {
    if actual != WORKSPACE_HTTP_API_GENERATION_V2 {
        return Err(FreshnessWaitContractError::UnsupportedApiGeneration { actual });
    }
    Ok(())
}

fn validate_freshness_requirement(
    requirement: &FreshnessRequirement,
) -> Result<(), FreshnessWaitContractError> {
    if requirement.on_stale != StaleSessionBehavior::WaitThenFail
        || !(1..=MAX_FRESHNESS_WAIT_DURATION_SECONDS).contains(&requirement.wait_timeout_seconds)
    {
        return Err(FreshnessWaitContractError::InvalidFreshnessRequirement);
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
        .expect("typed freshness wait serialization")
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
            if waiting > 0 && failed == 0 && attempt.updated_at < attempt.original_deadline_at =>
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
                    && terminal.completed_at >= attempt.original_deadline_at =>
            {
                Ok(())
            }
            FreshnessWaitTerminalOutcome::Failed
                if failed > 0 && terminal.completed_at <= attempt.original_deadline_at =>
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
    canonical_timestamp_seconds(end)?
        .checked_sub(canonical_timestamp_seconds(start)?)
        .ok_or(FreshnessWaitContractError::InvalidAttemptTimeline)
}

fn add_timestamp_seconds(value: &str, seconds: u64) -> Result<String, FreshnessWaitContractError> {
    let base = canonical_timestamp_seconds(value)?;
    let seconds =
        i64::try_from(seconds).map_err(|_| FreshnessWaitContractError::InvalidAttemptTimeline)?;
    format_timestamp_seconds(
        base.checked_add(seconds)
            .ok_or(FreshnessWaitContractError::InvalidAttemptTimeline)?,
    )
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
    Ok((era * 146_097 + day_of_era - 719_468) * 86_400 + hour * 3_600 + minute * 60 + second)
}

fn format_timestamp_seconds(value: i64) -> Result<String, FreshnessWaitContractError> {
    let days = value.div_euclid(86_400);
    let seconds = value.rem_euclid(86_400);
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    if !(1..=9999).contains(&year) {
        return Err(FreshnessWaitContractError::InvalidAttemptTimeline);
    }
    let hour = seconds / 3_600;
    let minute = seconds % 3_600 / 60;
    let second = seconds % 60;
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}
