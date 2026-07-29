use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fmt::{Display, Formatter};

use chrono::{DateTime, Datelike, SecondsFormat, Utc};
use locality_protocol::{
    HostedSlackChannelSelector, SlackChannelSharingClassification, SlackInstallationId,
};
use serde::{Deserialize, Deserializer, Serialize, de};

use super::identity::{HostedSlackPortableError, validate_slack_id};
use super::native::{
    HostedSlackChannel, HostedSlackFileMetadata, HostedSlackMessage, HostedSlackUser,
    MAX_HOSTED_SLACK_COLLECTION_ENTRIES, MAX_HOSTED_SLACK_THREAD_REPLIES, RawHostedSlackChannel,
    RawHostedSlackFileMetadata, RawHostedSlackMessage, RawHostedSlackUser,
};

pub const HOSTED_SLACK_POLL_CHECKPOINT_FORMAT_VERSION_V1: u16 = 1;
pub const HOSTED_SLACK_POLL_MINIMUM_READER_VERSION_V1: u16 = 1;
pub const MAX_HOSTED_SLACK_CURSOR_BYTES_V1: usize = 1024;
pub const MAX_HOSTED_SLACK_APPLIED_PAGES_V1: usize = 256;
pub const MAX_HOSTED_SLACK_REPLAY_BYTES_V1: usize = 512 * 1024;
pub const MAX_HOSTED_SLACK_CHECKPOINT_BYTES_V1: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostedSlackPollKindV1 {
    Bootstrap,
    FullRepair,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostedSlackPollPhaseV1 {
    HistoricalHistory,
    HistoricalReplies,
    AwaitingCatchUpCut,
    CatchUpHistory,
    CatchUpReplies,
    CompleteCandidate,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedSlackCompletedRootV1 {
    pub root_message_id: String,
    pub expected_reply_count: u32,
    pub observed_reply_count: u32,
    pub completed_phase: HostedSlackPollPhaseV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedSlackRootExpectationV1 {
    pub root_message_id: String,
    pub expected_reply_count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedSlackAppliedPageV1 {
    pub phase: HostedSlackPollPhaseV1,
    pub root_message_id: Option<String>,
    pub request_cursor: Option<String>,
    pub next_cursor: Option<String>,
    pub canonical_page_json: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostedSlackPollEvidenceV1 {
    AppliedPage { page: HostedSlackAppliedPageV1 },
    BeginCatchUp { poll_cut_at: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedSlackPollCandidateV1 {
    pub(super) users: Vec<RawHostedSlackUser>,
    pub(super) messages: Vec<RawHostedSlackMessage>,
    pub(super) files: Vec<RawHostedSlackFileMetadata>,
    pub(super) root_expectations: Vec<HostedSlackRootExpectationV1>,
    pub(super) stage_root_ids: Vec<String>,
    pub(super) stage_yielded_reply_root_ids: Vec<String>,
    pub(super) current_root_reply_message_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostedSlackPollCheckpointV1 {
    pub(super) checkpoint_format_version: u16,
    pub(super) minimum_reader_version: u16,
    pub(super) poll_kind: HostedSlackPollKindV1,
    pub(super) installation_id: SlackInstallationId,
    pub(super) team_id: String,
    pub(super) channel_id: String,
    pub(super) sharing: SlackChannelSharingClassification,
    pub(super) channel: RawHostedSlackChannel,
    pub(super) authorized_history_start_at: String,
    pub(super) backfill_cut_at: String,
    pub(super) poll_cut_at: Option<String>,
    pub(super) phase: HostedSlackPollPhaseV1,
    pub(super) history_cursor: Option<String>,
    pub(super) current_root_message_id: Option<String>,
    pub(super) reply_cursor: Option<String>,
    pub(super) completed_roots: Vec<HostedSlackCompletedRootV1>,
    pub(super) latest_observed_message_timestamp: Option<String>,
    pub(super) poll_overlap_watermark: String,
    pub(super) last_page_observed_at: Option<String>,
    pub(super) candidate: HostedSlackPollCandidateV1,
    pub(super) evidence: Vec<HostedSlackPollEvidenceV1>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostedSlackPollCheckpointWireV1 {
    checkpoint_format_version: u16,
    minimum_reader_version: u16,
    poll_kind: HostedSlackPollKindV1,
    installation_id: SlackInstallationId,
    team_id: String,
    channel_id: String,
    sharing: SlackChannelSharingClassification,
    channel: RawHostedSlackChannel,
    authorized_history_start_at: String,
    backfill_cut_at: String,
    poll_cut_at: Option<String>,
    phase: HostedSlackPollPhaseV1,
    history_cursor: Option<String>,
    current_root_message_id: Option<String>,
    reply_cursor: Option<String>,
    completed_roots: Vec<HostedSlackCompletedRootV1>,
    latest_observed_message_timestamp: Option<String>,
    poll_overlap_watermark: String,
    last_page_observed_at: Option<String>,
    candidate: HostedSlackPollCandidateV1,
    evidence: Vec<HostedSlackPollEvidenceV1>,
}

impl From<HostedSlackPollCheckpointWireV1> for HostedSlackPollCheckpointV1 {
    fn from(wire: HostedSlackPollCheckpointWireV1) -> Self {
        Self {
            checkpoint_format_version: wire.checkpoint_format_version,
            minimum_reader_version: wire.minimum_reader_version,
            poll_kind: wire.poll_kind,
            installation_id: wire.installation_id,
            team_id: wire.team_id,
            channel_id: wire.channel_id,
            sharing: wire.sharing,
            channel: wire.channel,
            authorized_history_start_at: wire.authorized_history_start_at,
            backfill_cut_at: wire.backfill_cut_at,
            poll_cut_at: wire.poll_cut_at,
            phase: wire.phase,
            history_cursor: wire.history_cursor,
            current_root_message_id: wire.current_root_message_id,
            reply_cursor: wire.reply_cursor,
            completed_roots: wire.completed_roots,
            latest_observed_message_timestamp: wire.latest_observed_message_timestamp,
            poll_overlap_watermark: wire.poll_overlap_watermark,
            last_page_observed_at: wire.last_page_observed_at,
            candidate: wire.candidate,
            evidence: wire.evidence,
        }
    }
}

impl<'de> Deserialize<'de> for HostedSlackPollCheckpointV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let checkpoint = Self::from(HostedSlackPollCheckpointWireV1::deserialize(deserializer)?);
        checkpoint.validate().map_err(de::Error::custom)?;
        Ok(checkpoint)
    }
}

impl HostedSlackPollCandidateV1 {
    pub fn users(&self) -> &[RawHostedSlackUser] {
        &self.users
    }

    pub fn messages(&self) -> &[RawHostedSlackMessage] {
        &self.messages
    }

    pub fn files(&self) -> &[RawHostedSlackFileMetadata] {
        &self.files
    }
}

impl HostedSlackPollCheckpointV1 {
    pub fn checkpoint_format_version(&self) -> u16 {
        self.checkpoint_format_version
    }

    pub fn minimum_reader_version(&self) -> u16 {
        self.minimum_reader_version
    }

    pub fn poll_kind(&self) -> HostedSlackPollKindV1 {
        self.poll_kind
    }

    pub fn phase(&self) -> HostedSlackPollPhaseV1 {
        self.phase
    }

    pub fn authorized_history_start_at(&self) -> &str {
        &self.authorized_history_start_at
    }

    pub fn backfill_cut_at(&self) -> &str {
        &self.backfill_cut_at
    }

    pub fn poll_cut_at(&self) -> Option<&str> {
        self.poll_cut_at.as_deref()
    }

    pub fn poll_overlap_watermark(&self) -> &str {
        &self.poll_overlap_watermark
    }

    pub fn history_cursor(&self) -> Option<&str> {
        self.history_cursor.as_deref()
    }

    pub fn current_root_message_id(&self) -> Option<&str> {
        self.current_root_message_id.as_deref()
    }

    pub fn reply_cursor(&self) -> Option<&str> {
        self.reply_cursor.as_deref()
    }

    pub fn completed_roots(&self) -> &[HostedSlackCompletedRootV1] {
        &self.completed_roots
    }

    pub fn latest_observed_message_timestamp(&self) -> Option<&str> {
        self.latest_observed_message_timestamp.as_deref()
    }

    pub fn candidate(&self) -> &HostedSlackPollCandidateV1 {
        &self.candidate
    }

    pub fn new(
        selector: &HostedSlackChannelSelector,
        channel: RawHostedSlackChannel,
        poll_kind: HostedSlackPollKindV1,
        backfill_cut_at: String,
        poll_overlap_watermark: String,
    ) -> Result<Self, HostedSlackPollError> {
        selector
            .validate()
            .map_err(|_| HostedSlackPollError::InvalidIdentity("selector"))?;
        let checkpoint = Self::genesis(
            selector,
            channel,
            poll_kind,
            backfill_cut_at,
            poll_overlap_watermark,
        );
        checkpoint.validate()?;
        Ok(checkpoint)
    }

    fn genesis(
        selector: &HostedSlackChannelSelector,
        channel: RawHostedSlackChannel,
        poll_kind: HostedSlackPollKindV1,
        backfill_cut_at: String,
        poll_overlap_watermark: String,
    ) -> Self {
        Self {
            checkpoint_format_version: HOSTED_SLACK_POLL_CHECKPOINT_FORMAT_VERSION_V1,
            minimum_reader_version: HOSTED_SLACK_POLL_MINIMUM_READER_VERSION_V1,
            poll_kind,
            installation_id: selector.installation_id.clone(),
            team_id: selector.team_id.clone(),
            channel_id: selector.channel_id.clone(),
            sharing: selector.sharing,
            channel,
            authorized_history_start_at: selector.authorized_history_start_at.clone(),
            backfill_cut_at,
            poll_cut_at: None,
            phase: HostedSlackPollPhaseV1::HistoricalHistory,
            history_cursor: None,
            current_root_message_id: None,
            reply_cursor: None,
            completed_roots: Vec::new(),
            latest_observed_message_timestamp: None,
            poll_overlap_watermark,
            last_page_observed_at: None,
            candidate: HostedSlackPollCandidateV1 {
                users: Vec::new(),
                messages: Vec::new(),
                files: Vec::new(),
                root_expectations: Vec::new(),
                stage_root_ids: Vec::new(),
                stage_yielded_reply_root_ids: Vec::new(),
                current_root_reply_message_ids: Vec::new(),
            },
            evidence: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), HostedSlackPollError> {
        self.validate_internal()?;
        let rebuilt = self.rebuild_from_evidence()?;
        if rebuilt != *self {
            return Err(HostedSlackPollError::DerivedStateMismatch);
        }
        Ok(())
    }

    pub(super) fn validate_internal(&self) -> Result<(), HostedSlackPollError> {
        if self.checkpoint_format_version != HOSTED_SLACK_POLL_CHECKPOINT_FORMAT_VERSION_V1
            || self.minimum_reader_version == 0
            || self.minimum_reader_version > HOSTED_SLACK_POLL_MINIMUM_READER_VERSION_V1
        {
            return Err(HostedSlackPollError::UnsupportedVersion {
                format_version: self.checkpoint_format_version,
                minimum_reader_version: self.minimum_reader_version,
            });
        }
        let selector = self.selector();
        selector
            .validate()
            .map_err(|_| HostedSlackPollError::InvalidIdentity("checkpoint scope"))?;

        let channel = HostedSlackChannel::try_from(self.channel.clone())
            .map_err(HostedSlackPollError::InvalidNative)?;
        if channel.team_id() != self.team_id
            || channel.channel_id() != self.channel_id
            || channel.sharing() != self.sharing
        {
            return Err(HostedSlackPollError::InvalidIdentity("candidate.channel"));
        }

        let history_start = parse_canonical_utc_timestamp(
            "authorized_history_start_at",
            &self.authorized_history_start_at,
        )?;
        let backfill_cut = parse_canonical_utc_timestamp("backfill_cut_at", &self.backfill_cut_at)?;
        let overlap =
            parse_canonical_utc_timestamp("poll_overlap_watermark", &self.poll_overlap_watermark)?;
        if history_start >= backfill_cut || overlap < history_start || overlap >= backfill_cut {
            return Err(HostedSlackPollError::InvalidCutOrder);
        }
        if let Some(poll_cut_at) = &self.poll_cut_at {
            let poll_cut = parse_canonical_utc_timestamp("poll_cut_at", poll_cut_at)?;
            if poll_cut <= backfill_cut {
                return Err(HostedSlackPollError::InvalidCutOrder);
            }
        }
        if let Some(observed_at) = &self.last_page_observed_at {
            parse_canonical_utc_timestamp("last_page_observed_at", observed_at)?;
        }

        validate_cursor("history_cursor", self.history_cursor.as_deref())?;
        validate_cursor("reply_cursor", self.reply_cursor.as_deref())?;
        validate_checkpoint_phase(self)?;
        validate_candidate(self)?;
        validate_evidence(self)?;

        let actual_bytes = serde_json::to_vec(self)
            .map_err(|_| HostedSlackPollError::Serialization)?
            .len();
        if actual_bytes > MAX_HOSTED_SLACK_CHECKPOINT_BYTES_V1 {
            return Err(HostedSlackPollError::InputTooLarge {
                input: "checkpoint",
                maximum_bytes: MAX_HOSTED_SLACK_CHECKPOINT_BYTES_V1,
                actual_bytes,
            });
        }
        Ok(())
    }

    fn rebuild_from_evidence(&self) -> Result<Self, HostedSlackPollError> {
        let selector = self.selector();
        let mut rebuilt = Self::genesis(
            &selector,
            self.channel.clone(),
            self.poll_kind,
            self.backfill_cut_at.clone(),
            self.poll_overlap_watermark.clone(),
        );
        rebuilt.checkpoint_format_version = self.checkpoint_format_version;
        rebuilt.minimum_reader_version = self.minimum_reader_version;

        for evidence in self.evidence.clone() {
            match evidence {
                HostedSlackPollEvidenceV1::AppliedPage { page } => {
                    super::poll::replay_applied_page_evidence(&mut rebuilt, &page)?;
                }
                HostedSlackPollEvidenceV1::BeginCatchUp { poll_cut_at } => {
                    rebuilt.begin_catch_up(poll_cut_at)?;
                }
            }
        }
        rebuilt.validate_internal()?;
        Ok(rebuilt)
    }

    pub fn selector(&self) -> HostedSlackChannelSelector {
        HostedSlackChannelSelector {
            selector_version: 1,
            installation_id: self.installation_id.clone(),
            team_id: self.team_id.clone(),
            channel_id: self.channel_id.clone(),
            authorized_history_start_at: self.authorized_history_start_at.clone(),
            sharing: self.sharing,
        }
    }

    pub fn begin_catch_up(&mut self, poll_cut_at: String) -> Result<(), HostedSlackPollError> {
        if self.phase != HostedSlackPollPhaseV1::AwaitingCatchUpCut {
            return Err(HostedSlackPollError::UnexpectedPhase {
                expected: HostedSlackPollPhaseV1::AwaitingCatchUpCut,
                actual: self.phase,
            });
        }
        let backfill_cut = parse_canonical_utc_timestamp("backfill_cut_at", &self.backfill_cut_at)?;
        let poll_cut = parse_canonical_utc_timestamp("poll_cut_at", &poll_cut_at)?;
        if poll_cut <= backfill_cut {
            return Err(HostedSlackPollError::InvalidCutOrder);
        }
        let mut next = self.clone();
        next.poll_cut_at = Some(poll_cut_at.clone());
        next.phase = HostedSlackPollPhaseV1::CatchUpHistory;
        next.history_cursor = None;
        next.candidate.stage_root_ids.clear();
        next.candidate.stage_yielded_reply_root_ids.clear();
        next.evidence
            .push(HostedSlackPollEvidenceV1::BeginCatchUp { poll_cut_at });
        next.validate_internal()?;
        *self = next;
        Ok(())
    }
}

pub fn decode_hosted_slack_poll_checkpoint_v1(
    bytes: &[u8],
) -> Result<HostedSlackPollCheckpointV1, HostedSlackPollError> {
    if bytes.len() > MAX_HOSTED_SLACK_CHECKPOINT_BYTES_V1 {
        return Err(HostedSlackPollError::InputTooLarge {
            input: "checkpoint",
            maximum_bytes: MAX_HOSTED_SLACK_CHECKPOINT_BYTES_V1,
            actual_bytes: bytes.len(),
        });
    }
    let checkpoint = serde_json::from_slice::<HostedSlackPollCheckpointV1>(bytes)
        .map_err(|_| HostedSlackPollError::InvalidJson("checkpoint"))?;
    checkpoint.validate()?;
    Ok(checkpoint)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostedSlackPollError {
    InputTooLarge {
        input: &'static str,
        maximum_bytes: usize,
        actual_bytes: usize,
    },
    InvalidJson(&'static str),
    UnsupportedVersion {
        format_version: u16,
        minimum_reader_version: u16,
    },
    InvalidIdentity(&'static str),
    InvalidCanonicalUtcTimestamp(&'static str),
    InvalidCutOrder,
    InvalidCursor(&'static str),
    CollectionTooLarge(&'static str),
    DuplicateValue(&'static str),
    UnexpectedPhase {
        expected: HostedSlackPollPhaseV1,
        actual: HostedSlackPollPhaseV1,
    },
    PageScopeMismatch(&'static str),
    PageWindowMismatch,
    UnexpectedCursor,
    CursorCycle,
    ConflictingReplay,
    ConflictingMessage(String),
    DerivedStateMismatch,
    MissingRoot(String),
    InvalidMessageRelationship(String),
    ReplyCountMismatch {
        root_message_id: String,
        expected: u32,
        actual: u32,
    },
    IncompleteCandidate(&'static str),
    InvalidNative(HostedSlackPortableError),
    Serialization,
}

impl Display for HostedSlackPollError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InputTooLarge {
                input,
                maximum_bytes,
                actual_bytes,
            } => write!(
                formatter,
                "hosted Slack {input} is {actual_bytes} bytes, exceeding {maximum_bytes} bytes"
            ),
            Self::InvalidJson(input) => write!(formatter, "hosted Slack {input} JSON is invalid"),
            Self::UnsupportedVersion {
                format_version,
                minimum_reader_version,
            } => write!(
                formatter,
                "unsupported hosted Slack format {format_version} / reader {minimum_reader_version}"
            ),
            Self::InvalidIdentity(field) => write!(formatter, "invalid hosted Slack {field}"),
            Self::InvalidCanonicalUtcTimestamp(field) => {
                write!(formatter, "{field} must be canonical UTC seconds")
            }
            Self::InvalidCutOrder => formatter.write_str("hosted Slack poll cuts are out of order"),
            Self::InvalidCursor(field) => write!(formatter, "invalid hosted Slack {field}"),
            Self::CollectionTooLarge(field) => {
                write!(formatter, "hosted Slack {field} exceeds its V1 bound")
            }
            Self::DuplicateValue(field) => write!(formatter, "hosted Slack {field} is duplicated"),
            Self::UnexpectedPhase { expected, actual } => {
                write!(formatter, "expected phase {expected:?}, got {actual:?}")
            }
            Self::PageScopeMismatch(field) => {
                write!(formatter, "hosted Slack page scope mismatch: {field}")
            }
            Self::PageWindowMismatch => formatter.write_str("hosted Slack page window mismatch"),
            Self::UnexpectedCursor => formatter.write_str("unexpected hosted Slack page cursor"),
            Self::CursorCycle => formatter.write_str("hosted Slack cursor repeated or cycled"),
            Self::ConflictingReplay => formatter.write_str("conflicting hosted Slack page replay"),
            Self::ConflictingMessage(message) => {
                write!(formatter, "conflicting hosted Slack message {message}")
            }
            Self::DerivedStateMismatch => {
                formatter.write_str("hosted Slack checkpoint derived state does not match evidence")
            }
            Self::MissingRoot(root) => write!(formatter, "missing hosted Slack root {root}"),
            Self::InvalidMessageRelationship(message) => {
                write!(
                    formatter,
                    "invalid hosted Slack message relationship {message}"
                )
            }
            Self::ReplyCountMismatch {
                root_message_id,
                expected,
                actual,
            } => write!(
                formatter,
                "Slack root {root_message_id} expected {expected} replies but observed {actual}"
            ),
            Self::IncompleteCandidate(reason) => {
                write!(formatter, "hosted Slack candidate is incomplete: {reason}")
            }
            Self::InvalidNative(error) => Display::fmt(error, formatter),
            Self::Serialization => formatter.write_str("hosted Slack poll serialization failed"),
        }
    }
}

impl std::error::Error for HostedSlackPollError {}

pub(crate) fn parse_canonical_utc_timestamp(
    field: &'static str,
    value: &str,
) -> Result<DateTime<Utc>, HostedSlackPollError> {
    let parsed = DateTime::parse_from_rfc3339(value)
        .map_err(|_| HostedSlackPollError::InvalidCanonicalUtcTimestamp(field))?
        .with_timezone(&Utc);
    if parsed.year() <= 0 || parsed.to_rfc3339_opts(SecondsFormat::Secs, true) != value {
        return Err(HostedSlackPollError::InvalidCanonicalUtcTimestamp(field));
    }
    Ok(parsed)
}

pub(crate) fn parse_slack_timestamp(
    field: &'static str,
    value: &str,
) -> Result<DateTime<Utc>, HostedSlackPollError> {
    let Some((seconds, micros)) = value.split_once('.') else {
        return Err(HostedSlackPollError::InvalidNative(
            HostedSlackPortableError::InvalidTimestamp(field),
        ));
    };
    if seconds.is_empty()
        || seconds.len() > 12
        || (seconds.len() > 1 && seconds.starts_with('0'))
        || !seconds.bytes().all(|byte| byte.is_ascii_digit())
        || micros.len() != 6
        || !micros.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(HostedSlackPollError::InvalidNative(
            HostedSlackPortableError::InvalidTimestamp(field),
        ));
    }
    let seconds = seconds.parse::<i64>().map_err(|_| {
        HostedSlackPollError::InvalidNative(HostedSlackPortableError::InvalidTimestamp(field))
    })?;
    let micros = micros.parse::<u32>().map_err(|_| {
        HostedSlackPollError::InvalidNative(HostedSlackPortableError::InvalidTimestamp(field))
    })?;
    DateTime::<Utc>::from_timestamp(seconds, micros * 1_000).ok_or(
        HostedSlackPollError::InvalidNative(HostedSlackPortableError::InvalidTimestamp(field)),
    )
}

pub(crate) fn validate_cursor(
    field: &'static str,
    value: Option<&str>,
) -> Result<(), HostedSlackPollError> {
    if value.is_some_and(|value| {
        value.is_empty()
            || value.len() > MAX_HOSTED_SLACK_CURSOR_BYTES_V1
            || value.chars().any(char::is_control)
    }) {
        return Err(HostedSlackPollError::InvalidCursor(field));
    }
    Ok(())
}

fn validate_checkpoint_phase(
    checkpoint: &HostedSlackPollCheckpointV1,
) -> Result<(), HostedSlackPollError> {
    let valid = match checkpoint.phase {
        HostedSlackPollPhaseV1::HistoricalHistory => {
            checkpoint.poll_cut_at.is_none()
                && checkpoint.current_root_message_id.is_none()
                && checkpoint.reply_cursor.is_none()
        }
        HostedSlackPollPhaseV1::HistoricalReplies => {
            checkpoint.poll_cut_at.is_none()
                && checkpoint.history_cursor.is_none()
                && checkpoint.current_root_message_id.is_some()
        }
        HostedSlackPollPhaseV1::AwaitingCatchUpCut => {
            checkpoint.poll_cut_at.is_none()
                && checkpoint.history_cursor.is_none()
                && checkpoint.current_root_message_id.is_none()
                && checkpoint.reply_cursor.is_none()
                && checkpoint.candidate.stage_root_ids.is_empty()
                && checkpoint
                    .candidate
                    .current_root_reply_message_ids
                    .is_empty()
        }
        HostedSlackPollPhaseV1::CatchUpHistory => {
            checkpoint.poll_cut_at.is_some()
                && checkpoint.current_root_message_id.is_none()
                && checkpoint.reply_cursor.is_none()
        }
        HostedSlackPollPhaseV1::CatchUpReplies => {
            checkpoint.poll_cut_at.is_some()
                && checkpoint.history_cursor.is_none()
                && checkpoint.current_root_message_id.is_some()
        }
        HostedSlackPollPhaseV1::CompleteCandidate => {
            checkpoint.poll_cut_at.is_some()
                && checkpoint.history_cursor.is_none()
                && checkpoint.current_root_message_id.is_none()
                && checkpoint.reply_cursor.is_none()
                && checkpoint.last_page_observed_at.is_some()
                && checkpoint.candidate.stage_root_ids.is_empty()
                && checkpoint
                    .candidate
                    .current_root_reply_message_ids
                    .is_empty()
        }
    };
    if !valid {
        return Err(HostedSlackPollError::IncompleteCandidate(
            "checkpoint phase fields",
        ));
    }
    Ok(())
}

fn validate_candidate(
    checkpoint: &HostedSlackPollCheckpointV1,
) -> Result<(), HostedSlackPollError> {
    let candidate = &checkpoint.candidate;
    for (field, len) in [
        ("candidate.users", candidate.users.len()),
        ("candidate.messages", candidate.messages.len()),
        ("candidate.files", candidate.files.len()),
        (
            "candidate.root_expectations",
            candidate.root_expectations.len(),
        ),
        ("candidate.stage_root_ids", candidate.stage_root_ids.len()),
        (
            "candidate.stage_yielded_reply_root_ids",
            candidate.stage_yielded_reply_root_ids.len(),
        ),
        (
            "candidate.current_root_reply_message_ids",
            candidate.current_root_reply_message_ids.len(),
        ),
    ] {
        if len > MAX_HOSTED_SLACK_COLLECTION_ENTRIES {
            return Err(HostedSlackPollError::CollectionTooLarge(field));
        }
    }
    for user in &candidate.users {
        HostedSlackUser::try_from(user.clone()).map_err(HostedSlackPollError::InvalidNative)?;
        if user.team_id != checkpoint.team_id {
            return Err(HostedSlackPollError::InvalidIdentity(
                "candidate.user.team_id",
            ));
        }
    }
    for message in &candidate.messages {
        HostedSlackMessage::try_from(message.clone())
            .map_err(HostedSlackPollError::InvalidNative)?;
        if message.channel_id != checkpoint.channel_id {
            return Err(HostedSlackPollError::InvalidIdentity(
                "candidate.message.channel_id",
            ));
        }
    }
    for file in &candidate.files {
        HostedSlackFileMetadata::try_from(file.clone())
            .map_err(HostedSlackPollError::InvalidNative)?;
        if file.channel_id != checkpoint.channel_id {
            return Err(HostedSlackPollError::InvalidIdentity(
                "candidate.file.channel_id",
            ));
        }
    }
    ensure_unique(
        "candidate.users",
        candidate.users.iter().map(|value| value.id.as_str()),
    )?;
    ensure_unique(
        "candidate.messages",
        candidate.messages.iter().map(|value| value.ts.as_str()),
    )?;
    ensure_unique(
        "candidate.files",
        candidate.files.iter().map(|value| value.id.as_str()),
    )?;
    ensure_sorted_unique(
        "candidate.root_expectations",
        candidate
            .root_expectations
            .iter()
            .map(|value| value.root_message_id.as_str()),
    )?;
    ensure_sorted_unique(
        "candidate.stage_root_ids",
        candidate.stage_root_ids.iter().map(String::as_str),
    )?;
    ensure_sorted_unique(
        "candidate.stage_yielded_reply_root_ids",
        candidate
            .stage_yielded_reply_root_ids
            .iter()
            .map(String::as_str),
    )?;
    ensure_sorted_unique(
        "candidate.current_root_reply_message_ids",
        candidate
            .current_root_reply_message_ids
            .iter()
            .map(String::as_str),
    )?;
    ensure_sorted_unique(
        "completed_roots",
        checkpoint
            .completed_roots
            .iter()
            .map(|value| value.root_message_id.as_str()),
    )?;
    for expectation in &candidate.root_expectations {
        parse_slack_timestamp(
            "root_expectations.root_message_id",
            &expectation.root_message_id,
        )?;
        if expectation.expected_reply_count as usize > MAX_HOSTED_SLACK_THREAD_REPLIES {
            return Err(HostedSlackPollError::CollectionTooLarge(
                "root expected replies",
            ));
        }
    }
    for completed in &checkpoint.completed_roots {
        parse_slack_timestamp(
            "completed_roots.root_message_id",
            &completed.root_message_id,
        )?;
        if completed.expected_reply_count as usize > MAX_HOSTED_SLACK_THREAD_REPLIES
            || completed.observed_reply_count as usize > MAX_HOSTED_SLACK_THREAD_REPLIES
        {
            return Err(HostedSlackPollError::CollectionTooLarge(
                "completed root replies",
            ));
        }
        if completed.expected_reply_count != completed.observed_reply_count
            || !matches!(
                completed.completed_phase,
                HostedSlackPollPhaseV1::HistoricalReplies | HostedSlackPollPhaseV1::CatchUpReplies
            )
        {
            return Err(HostedSlackPollError::IncompleteCandidate(
                "completed root evidence",
            ));
        }
        let Some(expectation) = candidate
            .root_expectations
            .iter()
            .find(|expectation| expectation.root_message_id == completed.root_message_id)
        else {
            return Err(HostedSlackPollError::MissingRoot(
                completed.root_message_id.clone(),
            ));
        };
        if expectation.expected_reply_count != completed.expected_reply_count
            || !candidate.messages.iter().any(|message| {
                message.ts == completed.root_message_id
                    && message
                        .thread_ts
                        .as_deref()
                        .is_none_or(|thread_ts| thread_ts == message.ts)
            })
        {
            return Err(HostedSlackPollError::IncompleteCandidate(
                "completed root binding",
            ));
        }
    }
    if let Some(root) = &checkpoint.current_root_message_id {
        parse_slack_timestamp("current_root_message_id", root)?;
        if !candidate
            .root_expectations
            .iter()
            .any(|expectation| expectation.root_message_id == *root)
        {
            return Err(HostedSlackPollError::MissingRoot(root.clone()));
        }
        if candidate.stage_root_ids.first() != Some(root) {
            return Err(HostedSlackPollError::IncompleteCandidate(
                "current root ordering",
            ));
        }
    }
    if let Some(latest) = &checkpoint.latest_observed_message_timestamp {
        parse_slack_timestamp("latest_observed_message_timestamp", latest)?;
    }
    let actual_latest = candidate
        .messages
        .iter()
        .map(|message| message.ts.as_str())
        .max_by(|left, right| compare_slack_timestamps(left, right));
    if actual_latest != checkpoint.latest_observed_message_timestamp.as_deref() {
        return Err(HostedSlackPollError::IncompleteCandidate(
            "latest observed message timestamp",
        ));
    }
    Ok(())
}

fn validate_evidence(checkpoint: &HostedSlackPollCheckpointV1) -> Result<(), HostedSlackPollError> {
    if checkpoint.evidence.len() > MAX_HOSTED_SLACK_APPLIED_PAGES_V1 + 1 {
        return Err(HostedSlackPollError::CollectionTooLarge("evidence"));
    }
    let mut keys = BTreeSet::new();
    let mut replay_bytes = 0usize;
    let mut transitions = 0usize;
    let mut applied_pages = 0usize;
    for evidence in &checkpoint.evidence {
        match evidence {
            HostedSlackPollEvidenceV1::AppliedPage { page } => {
                applied_pages += 1;
                if applied_pages > MAX_HOSTED_SLACK_APPLIED_PAGES_V1 {
                    return Err(HostedSlackPollError::CollectionTooLarge(
                        "applied page evidence",
                    ));
                }
                validate_cursor(
                    "applied page request_cursor",
                    page.request_cursor.as_deref(),
                )?;
                validate_cursor("applied page next_cursor", page.next_cursor.as_deref())?;
                if let Some(root) = &page.root_message_id {
                    parse_slack_timestamp("applied page root_message_id", root)?;
                }
                if !keys.insert((
                    page.phase,
                    page.root_message_id.as_deref(),
                    page.request_cursor.as_deref(),
                )) {
                    return Err(HostedSlackPollError::DuplicateValue("applied page key"));
                }
                replay_bytes = replay_bytes.saturating_add(page.canonical_page_json.len());
            }
            HostedSlackPollEvidenceV1::BeginCatchUp { poll_cut_at } => {
                parse_canonical_utc_timestamp("evidence.poll_cut_at", poll_cut_at)?;
                transitions += 1;
                if transitions > 1 {
                    return Err(HostedSlackPollError::DuplicateValue(
                        "begin catch-up evidence",
                    ));
                }
                replay_bytes = replay_bytes.saturating_add(poll_cut_at.len());
            }
        }
    }
    if replay_bytes > MAX_HOSTED_SLACK_REPLAY_BYTES_V1 {
        return Err(HostedSlackPollError::InputTooLarge {
            input: "checkpoint replay evidence",
            maximum_bytes: MAX_HOSTED_SLACK_REPLAY_BYTES_V1,
            actual_bytes: replay_bytes,
        });
    }
    Ok(())
}

fn ensure_unique<'a>(
    field: &'static str,
    values: impl Iterator<Item = &'a str>,
) -> Result<(), HostedSlackPollError> {
    let mut seen = BTreeSet::new();
    if values.into_iter().any(|value| !seen.insert(value)) {
        return Err(HostedSlackPollError::DuplicateValue(field));
    }
    Ok(())
}

fn ensure_sorted_unique<'a>(
    field: &'static str,
    values: impl Iterator<Item = &'a str>,
) -> Result<(), HostedSlackPollError> {
    let values = values.collect::<Vec<_>>();
    if values.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(HostedSlackPollError::DuplicateValue(field));
    }
    Ok(())
}

pub(crate) fn validate_page_scope_id(
    field: &'static str,
    value: &str,
    prefixes: &[u8],
) -> Result<(), HostedSlackPollError> {
    validate_slack_id(field, value, prefixes).map_err(HostedSlackPollError::InvalidNative)
}

pub(crate) fn compare_slack_timestamps(left: &str, right: &str) -> Ordering {
    let left_seconds = left.split_once('.').map_or(left, |(seconds, _)| seconds);
    let right_seconds = right.split_once('.').map_or(right, |(seconds, _)| seconds);
    left_seconds
        .len()
        .cmp(&right_seconds.len())
        .then_with(|| left.cmp(right))
}
