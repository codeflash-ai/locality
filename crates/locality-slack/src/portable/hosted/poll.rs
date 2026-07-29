use std::collections::{BTreeMap, BTreeSet};

use chrono::Utc;
use locality_protocol::{
    ReplicaFreshnessState, SlackChannelSharingClassification, SlackInstallationId,
};
use serde::{Deserialize, Serialize};

use super::checkpoint::{
    HostedSlackAppliedPageV1, HostedSlackCompletedRootV1, HostedSlackPollCheckpointV1,
    HostedSlackPollError, HostedSlackPollEvidenceV1, HostedSlackPollKindV1, HostedSlackPollPhaseV1,
    HostedSlackRootExpectationV1, MAX_HOSTED_SLACK_APPLIED_PAGES_V1, compare_slack_timestamps,
    parse_canonical_utc_timestamp, parse_slack_timestamp, validate_cursor, validate_page_scope_id,
};
use super::native::{
    HostedSlackFileMetadata, HostedSlackMessage, HostedSlackNativeSnapshot, HostedSlackUser,
    MAX_HOSTED_SLACK_THREAD_REPLIES, RawHostedSlackFileMetadata, RawHostedSlackMessage,
    RawHostedSlackNativeSnapshot, RawHostedSlackThread, RawHostedSlackUser,
};
use super::render::{
    HOSTED_SLACK_OPERATIONAL_STATUS_FORMAT_VERSION_V1, HostedSlackOperationalStatusV1,
};

pub const HOSTED_SLACK_POLL_PAGE_FORMAT_VERSION_V1: u16 = 1;
pub const HOSTED_SLACK_POLL_PAGE_MINIMUM_READER_VERSION_V1: u16 = 1;
pub const MAX_HOSTED_SLACK_POLL_PAGE_BYTES_V1: usize = 512 * 1024;
pub const MAX_HOSTED_SLACK_POLL_PAGE_MESSAGES_V1: usize = 512;
pub const MAX_HOSTED_SLACK_POLL_PAGE_USERS_V1: usize = 512;
pub const MAX_HOSTED_SLACK_POLL_PAGE_FILES_V1: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedSlackHistoryMessageV1 {
    pub message: RawHostedSlackMessage,
    pub reply_count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedSlackHistoryPageV1 {
    pub page_format_version: u16,
    pub minimum_reader_version: u16,
    pub poll_kind: HostedSlackPollKindV1,
    pub phase: HostedSlackPollPhaseV1,
    pub installation_id: SlackInstallationId,
    pub team_id: String,
    pub channel_id: String,
    pub sharing: SlackChannelSharingClassification,
    pub authorized_history_start_at: String,
    pub backfill_cut_at: String,
    pub poll_cut_at: Option<String>,
    pub poll_overlap_watermark: String,
    pub request_cursor: Option<String>,
    pub next_cursor: Option<String>,
    pub observed_at: String,
    pub messages: Vec<HostedSlackHistoryMessageV1>,
    pub users: Vec<RawHostedSlackUser>,
    pub files: Vec<RawHostedSlackFileMetadata>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedSlackRepliesPageV1 {
    pub page_format_version: u16,
    pub minimum_reader_version: u16,
    pub poll_kind: HostedSlackPollKindV1,
    pub phase: HostedSlackPollPhaseV1,
    pub installation_id: SlackInstallationId,
    pub team_id: String,
    pub channel_id: String,
    pub sharing: SlackChannelSharingClassification,
    pub authorized_history_start_at: String,
    pub backfill_cut_at: String,
    pub poll_cut_at: Option<String>,
    pub poll_overlap_watermark: String,
    pub root_message_id: String,
    pub root_reply_count: u32,
    pub request_cursor: Option<String>,
    pub next_cursor: Option<String>,
    pub observed_at: String,
    pub messages: Vec<RawHostedSlackMessage>,
    pub users: Vec<RawHostedSlackUser>,
    pub files: Vec<RawHostedSlackFileMetadata>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostedSlackPageApplyOutcomeV1 {
    Applied,
    ExactReplay,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostedSlackPollOutputV1 {
    pub snapshot: HostedSlackNativeSnapshot,
    pub operational_status: HostedSlackOperationalStatusV1,
}

impl HostedSlackHistoryPageV1 {
    pub fn validate(&self) -> Result<(), HostedSlackPollError> {
        validate_page_versions(self.page_format_version, self.minimum_reader_version)?;
        if !matches!(
            self.phase,
            HostedSlackPollPhaseV1::HistoricalHistory | HostedSlackPollPhaseV1::CatchUpHistory
        ) {
            return Err(HostedSlackPollError::UnexpectedPhase {
                expected: HostedSlackPollPhaseV1::HistoricalHistory,
                actual: self.phase,
            });
        }
        validate_page_scope(
            &self.team_id,
            &self.channel_id,
            &self.authorized_history_start_at,
            &self.backfill_cut_at,
            self.poll_cut_at.as_deref(),
            &self.poll_overlap_watermark,
        )?;
        validate_cursor("page.request_cursor", self.request_cursor.as_deref())?;
        validate_cursor("page.next_cursor", self.next_cursor.as_deref())?;
        parse_canonical_utc_timestamp("page.observed_at", &self.observed_at)?;
        validate_page_collection(
            "page.messages",
            self.messages.len(),
            MAX_HOSTED_SLACK_POLL_PAGE_MESSAGES_V1,
        )?;
        validate_page_collection(
            "page.users",
            self.users.len(),
            MAX_HOSTED_SLACK_POLL_PAGE_USERS_V1,
        )?;
        validate_page_collection(
            "page.files",
            self.files.len(),
            MAX_HOSTED_SLACK_POLL_PAGE_FILES_V1,
        )?;
        ensure_unique_page_values(
            "page.messages",
            self.messages.iter().map(|value| value.message.ts.as_str()),
        )?;
        validate_page_entities(
            &self.channel_id,
            &self.team_id,
            self.messages.iter().map(|value| &value.message),
            &self.users,
            &self.files,
        )?;
        for message in &self.messages {
            if message.reply_count as usize > MAX_HOSTED_SLACK_THREAD_REPLIES {
                return Err(HostedSlackPollError::CollectionTooLarge("page.reply_count"));
            }
            if normalized_root_id(&message.message).is_some() && message.reply_count != 0 {
                return Err(HostedSlackPollError::InvalidMessageRelationship(
                    message.message.ts.clone(),
                ));
            }
        }
        validate_serialized_page_size(self, "history page")?;
        Ok(())
    }
}

impl HostedSlackRepliesPageV1 {
    pub fn validate(&self) -> Result<(), HostedSlackPollError> {
        validate_page_versions(self.page_format_version, self.minimum_reader_version)?;
        if !matches!(
            self.phase,
            HostedSlackPollPhaseV1::HistoricalReplies | HostedSlackPollPhaseV1::CatchUpReplies
        ) {
            return Err(HostedSlackPollError::UnexpectedPhase {
                expected: HostedSlackPollPhaseV1::HistoricalReplies,
                actual: self.phase,
            });
        }
        validate_page_scope(
            &self.team_id,
            &self.channel_id,
            &self.authorized_history_start_at,
            &self.backfill_cut_at,
            self.poll_cut_at.as_deref(),
            &self.poll_overlap_watermark,
        )?;
        parse_slack_timestamp("page.root_message_id", &self.root_message_id)?;
        if self.root_reply_count as usize > MAX_HOSTED_SLACK_THREAD_REPLIES {
            return Err(HostedSlackPollError::CollectionTooLarge(
                "page.root_reply_count",
            ));
        }
        validate_cursor("page.request_cursor", self.request_cursor.as_deref())?;
        validate_cursor("page.next_cursor", self.next_cursor.as_deref())?;
        parse_canonical_utc_timestamp("page.observed_at", &self.observed_at)?;
        validate_page_collection(
            "page.messages",
            self.messages.len(),
            MAX_HOSTED_SLACK_POLL_PAGE_MESSAGES_V1,
        )?;
        validate_page_collection(
            "page.users",
            self.users.len(),
            MAX_HOSTED_SLACK_POLL_PAGE_USERS_V1,
        )?;
        validate_page_collection(
            "page.files",
            self.files.len(),
            MAX_HOSTED_SLACK_POLL_PAGE_FILES_V1,
        )?;
        ensure_unique_page_values(
            "page.messages",
            self.messages.iter().map(|value| value.ts.as_str()),
        )?;
        validate_page_entities(
            &self.channel_id,
            &self.team_id,
            self.messages.iter(),
            &self.users,
            &self.files,
        )?;
        if self.messages.is_empty() {
            return Err(HostedSlackPollError::IncompleteCandidate(
                "replies page messages",
            ));
        }
        if self.request_cursor.is_none() {
            let first = &self.messages[0];
            if first.ts != self.root_message_id {
                return Err(HostedSlackPollError::MissingRoot(
                    self.root_message_id.clone(),
                ));
            }
            if normalized_root_id(first).is_some() {
                return Err(HostedSlackPollError::InvalidMessageRelationship(
                    first.ts.clone(),
                ));
            }
        } else if self
            .messages
            .iter()
            .any(|message| message.ts == self.root_message_id)
        {
            return Err(HostedSlackPollError::InvalidMessageRelationship(
                self.root_message_id.clone(),
            ));
        }
        for message in &self.messages {
            if message.ts == self.root_message_id {
                if normalized_root_id(message).is_some() {
                    return Err(HostedSlackPollError::InvalidMessageRelationship(
                        message.ts.clone(),
                    ));
                }
            } else if normalized_root_id(message) != Some(self.root_message_id.as_str()) {
                return Err(HostedSlackPollError::InvalidMessageRelationship(
                    message.ts.clone(),
                ));
            }
        }
        validate_serialized_page_size(self, "replies page")?;
        Ok(())
    }
}

pub fn decode_hosted_slack_history_page_v1(
    bytes: &[u8],
) -> Result<HostedSlackHistoryPageV1, HostedSlackPollError> {
    validate_page_bytes("history page", bytes)?;
    let page = serde_json::from_slice::<HostedSlackHistoryPageV1>(bytes)
        .map_err(|_| HostedSlackPollError::InvalidJson("history page"))?;
    page.validate()?;
    Ok(page)
}

pub fn decode_hosted_slack_replies_page_v1(
    bytes: &[u8],
) -> Result<HostedSlackRepliesPageV1, HostedSlackPollError> {
    validate_page_bytes("replies page", bytes)?;
    let page = serde_json::from_slice::<HostedSlackRepliesPageV1>(bytes)
        .map_err(|_| HostedSlackPollError::InvalidJson("replies page"))?;
    page.validate()?;
    Ok(page)
}

impl HostedSlackPollCheckpointV1 {
    pub fn apply_history_page(
        &mut self,
        page: &HostedSlackHistoryPageV1,
    ) -> Result<HostedSlackPageApplyOutcomeV1, HostedSlackPollError> {
        page.validate()?;
        validate_history_page_scope(self, page)?;
        let accepted_page = accepted_history_evidence_page(self, page)?;
        let canonical_page_json = serde_json::to_string(&accepted_page)
            .map_err(|_| HostedSlackPollError::Serialization)?;
        if let Some(outcome) = replay_outcome(
            self,
            page.phase,
            None,
            page.request_cursor.as_deref(),
            &canonical_page_json,
        )? {
            return Ok(outcome);
        }
        if self.phase != page.phase {
            return Err(HostedSlackPollError::UnexpectedPhase {
                expected: self.phase,
                actual: page.phase,
            });
        }
        if self.history_cursor != page.request_cursor {
            return Err(HostedSlackPollError::UnexpectedCursor);
        }
        validate_next_cursor(
            self,
            page.phase,
            None,
            page.request_cursor.as_deref(),
            page.next_cursor.as_deref(),
        )?;

        let mut next = self.clone();
        let seen_messages = messages_seen_in_refresh(&next, page.phase)?;
        let (window_start, window_end) = history_window(&next, page.phase)?;
        for wrapped in &accepted_page.messages {
            let message = &wrapped.message;
            let message_time = parse_slack_timestamp("page.message.ts", &message.ts)?;
            let root_id = normalized_root_id(message).unwrap_or(message.ts.as_str());
            let root_time = parse_slack_timestamp("page.message.thread_ts", root_id)?;
            let history_start = parse_canonical_utc_timestamp(
                "authorized_history_start_at",
                &next.authorized_history_start_at,
            )?;
            if root_time < history_start {
                continue;
            }
            if message_time < window_start || message_time >= window_end {
                return Err(HostedSlackPollError::PageWindowMismatch);
            }
            apply_message_current_state(
                &mut next.candidate.messages,
                message.clone(),
                page.phase,
                seen_messages.get(&message.ts),
            )?;
            insert_sorted_unique(&mut next.candidate.stage_root_ids, root_id.to_string());
            if normalized_root_id(message).is_some() {
                insert_sorted_unique(
                    &mut next.candidate.stage_yielded_reply_root_ids,
                    root_id.to_string(),
                );
            } else {
                upsert_root_expectation_checked(
                    &mut next.candidate.root_expectations,
                    HostedSlackRootExpectationV1 {
                        root_message_id: message.ts.clone(),
                        expected_reply_count: wrapped.reply_count,
                    },
                    seen_messages.contains_key(&message.ts),
                )?;
            }
        }
        normalize_candidate(&mut next);
        next.history_cursor = page.next_cursor.clone();
        next.last_page_observed_at = Some(page.observed_at.clone());
        record_page(
            &mut next,
            page.phase,
            None,
            page.request_cursor.clone(),
            page.next_cursor.clone(),
            canonical_page_json,
        );
        if page.next_cursor.is_none() {
            prepare_reply_phase(&mut next, page.phase)?;
        } else {
            rebuild_candidate_reference_metadata(&mut next)?;
        }
        next.validate_internal()?;
        *self = next;
        Ok(HostedSlackPageApplyOutcomeV1::Applied)
    }

    pub fn apply_replies_page(
        &mut self,
        page: &HostedSlackRepliesPageV1,
    ) -> Result<HostedSlackPageApplyOutcomeV1, HostedSlackPollError> {
        page.validate()?;
        validate_replies_page_scope(self, page)?;
        let accepted_page = accepted_replies_evidence_page(page);
        let canonical_page_json = serde_json::to_string(&accepted_page)
            .map_err(|_| HostedSlackPollError::Serialization)?;
        if let Some(outcome) = replay_outcome(
            self,
            page.phase,
            Some(&page.root_message_id),
            page.request_cursor.as_deref(),
            &canonical_page_json,
        )? {
            return Ok(outcome);
        }
        if self.phase != page.phase {
            return Err(HostedSlackPollError::UnexpectedPhase {
                expected: self.phase,
                actual: page.phase,
            });
        }
        if self.current_root_message_id.as_deref() != Some(page.root_message_id.as_str())
            || self.reply_cursor != page.request_cursor
        {
            return Err(HostedSlackPollError::UnexpectedCursor);
        }
        validate_next_cursor(
            self,
            page.phase,
            Some(&page.root_message_id),
            page.request_cursor.as_deref(),
            page.next_cursor.as_deref(),
        )?;

        let mut next = self.clone();
        let history_start = parse_canonical_utc_timestamp(
            "authorized_history_start_at",
            &next.authorized_history_start_at,
        )?;
        let root_time = parse_slack_timestamp("page.root_message_id", &page.root_message_id)?;
        let window_end = reply_window_end(&next, page.phase)?;
        if root_time < history_start || root_time >= window_end {
            return Err(HostedSlackPollError::PageWindowMismatch);
        }
        let first_page = page.request_cursor.is_none();
        let expectation_is_from_this_catch_up = page.phase
            != HostedSlackPollPhaseV1::CatchUpReplies
            || catch_up_history_contains_root(&next, &page.root_message_id)?
            || !first_page;
        if expectation_is_from_this_catch_up {
            let expected = expected_reply_count(&next, &page.root_message_id)?;
            if expected != page.root_reply_count {
                return Err(HostedSlackPollError::ReplyCountMismatch {
                    root_message_id: page.root_message_id.clone(),
                    expected,
                    actual: page.root_reply_count,
                });
            }
        } else {
            upsert_root_expectation(
                &mut next.candidate.root_expectations,
                HostedSlackRootExpectationV1 {
                    root_message_id: page.root_message_id.clone(),
                    expected_reply_count: page.root_reply_count,
                },
            );
        }
        if first_page {
            next.candidate.messages.retain(|message| {
                normalized_root_id(message) != Some(page.root_message_id.as_str())
            });
            next.candidate.current_root_reply_message_ids.clear();
        }
        let seen_messages = messages_seen_in_refresh(&next, page.phase)?;
        for message in &accepted_page.messages {
            let message_time = parse_slack_timestamp("page.message.ts", &message.ts)?;
            if message.ts == page.root_message_id {
                apply_message_current_state(
                    &mut next.candidate.messages,
                    message.clone(),
                    page.phase,
                    seen_messages.get(&message.ts),
                )?;
                continue;
            }
            if message_time < root_time || message_time >= window_end {
                return Err(HostedSlackPollError::PageWindowMismatch);
            }
            apply_message_current_state(
                &mut next.candidate.messages,
                message.clone(),
                page.phase,
                seen_messages.get(&message.ts),
            )?;
            insert_sorted_unique(
                &mut next.candidate.current_root_reply_message_ids,
                message.ts.clone(),
            );
        }
        normalize_candidate(&mut next);
        next.reply_cursor = page.next_cursor.clone();
        next.last_page_observed_at = Some(page.observed_at.clone());
        if page.next_cursor.is_none() {
            complete_current_root(&mut next, page.phase)?;
        }
        record_page(
            &mut next,
            page.phase,
            Some(page.root_message_id.clone()),
            page.request_cursor.clone(),
            page.next_cursor.clone(),
            canonical_page_json,
        );
        rebuild_candidate_reference_metadata(&mut next)?;
        if next.phase == HostedSlackPollPhaseV1::CompleteCandidate {
            build_snapshot(&next)?;
        }
        next.validate_internal()?;
        *self = next;
        Ok(HostedSlackPageApplyOutcomeV1::Applied)
    }

    pub fn completed_output(&self) -> Result<HostedSlackPollOutputV1, HostedSlackPollError> {
        if self.phase != HostedSlackPollPhaseV1::CompleteCandidate {
            return Err(HostedSlackPollError::IncompleteCandidate("poll phase"));
        }
        self.validate()?;
        let snapshot = build_snapshot(self)?;
        let poll_cut_at = self
            .poll_cut_at
            .clone()
            .ok_or(HostedSlackPollError::IncompleteCandidate("poll cut"))?;
        let last_successful_sync_at =
            self.last_page_observed_at
                .clone()
                .ok_or(HostedSlackPollError::IncompleteCandidate(
                    "last page observation",
                ))?;
        let operational_status = HostedSlackOperationalStatusV1 {
            status_format_version: HOSTED_SLACK_OPERATIONAL_STATUS_FORMAT_VERSION_V1,
            installation_id: self.installation_id.clone(),
            team_id: self.team_id.clone(),
            channel_id: self.channel_id.clone(),
            authorized_history_start_at: self.authorized_history_start_at.clone(),
            sharing: self.sharing,
            coverage_start_at: self.authorized_history_start_at.clone(),
            coverage_end_at: poll_cut_at.clone(),
            coverage_complete: true,
            freshness_state: ReplicaFreshnessState::Fresh,
            freshness_observed_through: poll_cut_at,
            last_successful_sync_at,
        };
        operational_status
            .validate(&self.selector())
            .map_err(|_| HostedSlackPollError::IncompleteCandidate("operational status"))?;
        Ok(HostedSlackPollOutputV1 {
            snapshot,
            operational_status,
        })
    }
}

pub(crate) fn replay_applied_page_evidence(
    checkpoint: &mut HostedSlackPollCheckpointV1,
    record: &HostedSlackAppliedPageV1,
) -> Result<(), HostedSlackPollError> {
    match record.phase {
        HostedSlackPollPhaseV1::HistoricalHistory | HostedSlackPollPhaseV1::CatchUpHistory => {
            let page =
                serde_json::from_str::<HostedSlackHistoryPageV1>(&record.canonical_page_json)
                    .map_err(|_| HostedSlackPollError::InvalidJson("checkpoint replay evidence"))?;
            if serde_json::to_string(&page).map_err(|_| HostedSlackPollError::Serialization)?
                != record.canonical_page_json
                || page.phase != record.phase
                || record.root_message_id.is_some()
                || page.request_cursor != record.request_cursor
                || page.next_cursor != record.next_cursor
            {
                return Err(HostedSlackPollError::IncompleteCandidate(
                    "history page replay evidence",
                ));
            }
            if checkpoint.apply_history_page(&page)? != HostedSlackPageApplyOutcomeV1::Applied {
                return Err(HostedSlackPollError::ConflictingReplay);
            }
        }
        HostedSlackPollPhaseV1::HistoricalReplies | HostedSlackPollPhaseV1::CatchUpReplies => {
            let page =
                serde_json::from_str::<HostedSlackRepliesPageV1>(&record.canonical_page_json)
                    .map_err(|_| HostedSlackPollError::InvalidJson("checkpoint replay evidence"))?;
            if serde_json::to_string(&page).map_err(|_| HostedSlackPollError::Serialization)?
                != record.canonical_page_json
                || page.phase != record.phase
                || record.root_message_id.as_deref() != Some(page.root_message_id.as_str())
                || page.request_cursor != record.request_cursor
                || page.next_cursor != record.next_cursor
            {
                return Err(HostedSlackPollError::IncompleteCandidate(
                    "replies page replay evidence",
                ));
            }
            if checkpoint.apply_replies_page(&page)? != HostedSlackPageApplyOutcomeV1::Applied {
                return Err(HostedSlackPollError::ConflictingReplay);
            }
        }
        _ => {
            return Err(HostedSlackPollError::IncompleteCandidate(
                "applied page phase",
            ));
        }
    }
    Ok(())
}

fn validate_page_versions(
    format_version: u16,
    minimum_reader_version: u16,
) -> Result<(), HostedSlackPollError> {
    if format_version != HOSTED_SLACK_POLL_PAGE_FORMAT_VERSION_V1
        || minimum_reader_version == 0
        || minimum_reader_version > HOSTED_SLACK_POLL_PAGE_MINIMUM_READER_VERSION_V1
    {
        return Err(HostedSlackPollError::UnsupportedVersion {
            format_version,
            minimum_reader_version,
        });
    }
    Ok(())
}

fn validate_page_scope(
    team_id: &str,
    channel_id: &str,
    history_start_at: &str,
    backfill_cut_at: &str,
    poll_cut_at: Option<&str>,
    poll_overlap_watermark: &str,
) -> Result<(), HostedSlackPollError> {
    validate_page_scope_id("page.team_id", team_id, b"T")?;
    validate_page_scope_id("page.channel_id", channel_id, b"CG")?;
    let history_start =
        parse_canonical_utc_timestamp("page.authorized_history_start_at", history_start_at)?;
    let backfill_cut = parse_canonical_utc_timestamp("page.backfill_cut_at", backfill_cut_at)?;
    let overlap =
        parse_canonical_utc_timestamp("page.poll_overlap_watermark", poll_overlap_watermark)?;
    if history_start >= backfill_cut || overlap < history_start || overlap >= backfill_cut {
        return Err(HostedSlackPollError::InvalidCutOrder);
    }
    if let Some(poll_cut_at) = poll_cut_at {
        let poll_cut = parse_canonical_utc_timestamp("page.poll_cut_at", poll_cut_at)?;
        if poll_cut <= backfill_cut {
            return Err(HostedSlackPollError::InvalidCutOrder);
        }
    }
    Ok(())
}

fn validate_page_collection(
    field: &'static str,
    actual: usize,
    maximum: usize,
) -> Result<(), HostedSlackPollError> {
    if actual > maximum {
        return Err(HostedSlackPollError::CollectionTooLarge(field));
    }
    Ok(())
}

fn validate_page_entities<'a>(
    channel_id: &str,
    team_id: &str,
    messages: impl Iterator<Item = &'a RawHostedSlackMessage>,
    users: &[RawHostedSlackUser],
    files: &[RawHostedSlackFileMetadata],
) -> Result<(), HostedSlackPollError> {
    for message in messages {
        HostedSlackMessage::try_from(message.clone())
            .map_err(HostedSlackPollError::InvalidNative)?;
        if message.channel_id != channel_id {
            return Err(HostedSlackPollError::PageScopeMismatch(
                "message.channel_id",
            ));
        }
    }
    for user in users {
        HostedSlackUser::try_from(user.clone()).map_err(HostedSlackPollError::InvalidNative)?;
        if user.team_id != team_id {
            return Err(HostedSlackPollError::PageScopeMismatch("user.team_id"));
        }
    }
    for file in files {
        HostedSlackFileMetadata::try_from(file.clone())
            .map_err(HostedSlackPollError::InvalidNative)?;
        if file.channel_id != channel_id {
            return Err(HostedSlackPollError::PageScopeMismatch("file.channel_id"));
        }
    }
    ensure_unique_page_values("page.users", users.iter().map(|value| value.id.as_str()))?;
    ensure_unique_page_values("page.files", files.iter().map(|value| value.id.as_str()))?;
    Ok(())
}

fn validate_page_bytes(input: &'static str, bytes: &[u8]) -> Result<(), HostedSlackPollError> {
    if bytes.len() > MAX_HOSTED_SLACK_POLL_PAGE_BYTES_V1 {
        return Err(HostedSlackPollError::InputTooLarge {
            input,
            maximum_bytes: MAX_HOSTED_SLACK_POLL_PAGE_BYTES_V1,
            actual_bytes: bytes.len(),
        });
    }
    Ok(())
}

fn validate_serialized_page_size(
    page: &impl Serialize,
    input: &'static str,
) -> Result<(), HostedSlackPollError> {
    let actual_bytes = serde_json::to_vec(page)
        .map_err(|_| HostedSlackPollError::Serialization)?
        .len();
    if actual_bytes > MAX_HOSTED_SLACK_POLL_PAGE_BYTES_V1 {
        return Err(HostedSlackPollError::InputTooLarge {
            input,
            maximum_bytes: MAX_HOSTED_SLACK_POLL_PAGE_BYTES_V1,
            actual_bytes,
        });
    }
    Ok(())
}

fn validate_history_page_scope(
    checkpoint: &HostedSlackPollCheckpointV1,
    page: &HostedSlackHistoryPageV1,
) -> Result<(), HostedSlackPollError> {
    validate_scope_match(
        checkpoint,
        page.poll_kind,
        &page.installation_id,
        &page.team_id,
        &page.channel_id,
        page.sharing,
        &page.authorized_history_start_at,
        &page.backfill_cut_at,
        page.poll_cut_at.as_deref(),
        &page.poll_overlap_watermark,
        &page.observed_at,
        page.phase,
    )
}

fn validate_replies_page_scope(
    checkpoint: &HostedSlackPollCheckpointV1,
    page: &HostedSlackRepliesPageV1,
) -> Result<(), HostedSlackPollError> {
    validate_scope_match(
        checkpoint,
        page.poll_kind,
        &page.installation_id,
        &page.team_id,
        &page.channel_id,
        page.sharing,
        &page.authorized_history_start_at,
        &page.backfill_cut_at,
        page.poll_cut_at.as_deref(),
        &page.poll_overlap_watermark,
        &page.observed_at,
        page.phase,
    )
}

#[allow(clippy::too_many_arguments)]
fn validate_scope_match(
    checkpoint: &HostedSlackPollCheckpointV1,
    poll_kind: HostedSlackPollKindV1,
    installation_id: &SlackInstallationId,
    team_id: &str,
    channel_id: &str,
    sharing: SlackChannelSharingClassification,
    history_start_at: &str,
    backfill_cut_at: &str,
    poll_cut_at: Option<&str>,
    poll_overlap_watermark: &str,
    observed_at: &str,
    phase: HostedSlackPollPhaseV1,
) -> Result<(), HostedSlackPollError> {
    for (field, matches) in [
        ("poll_kind", poll_kind == checkpoint.poll_kind),
        (
            "installation_id",
            installation_id == &checkpoint.installation_id,
        ),
        ("team_id", team_id == checkpoint.team_id),
        ("channel_id", channel_id == checkpoint.channel_id),
        ("sharing", sharing == checkpoint.sharing),
        (
            "authorized_history_start_at",
            history_start_at == checkpoint.authorized_history_start_at,
        ),
        (
            "backfill_cut_at",
            backfill_cut_at == checkpoint.backfill_cut_at,
        ),
        (
            "poll_cut_at",
            poll_cut_at == checkpoint.poll_cut_at.as_deref(),
        ),
        (
            "poll_overlap_watermark",
            poll_overlap_watermark == checkpoint.poll_overlap_watermark,
        ),
    ] {
        if !matches {
            return Err(HostedSlackPollError::PageScopeMismatch(field));
        }
    }
    let observed_at = parse_canonical_utc_timestamp("page.observed_at", observed_at)?;
    let window_end = match phase {
        HostedSlackPollPhaseV1::HistoricalHistory | HostedSlackPollPhaseV1::HistoricalReplies => {
            parse_canonical_utc_timestamp("backfill_cut_at", &checkpoint.backfill_cut_at)?
        }
        HostedSlackPollPhaseV1::CatchUpHistory | HostedSlackPollPhaseV1::CatchUpReplies => {
            parse_canonical_utc_timestamp(
                "poll_cut_at",
                checkpoint
                    .poll_cut_at
                    .as_deref()
                    .ok_or(HostedSlackPollError::PageWindowMismatch)?,
            )?
        }
        _ => return Err(HostedSlackPollError::PageWindowMismatch),
    };
    if observed_at < window_end {
        return Err(HostedSlackPollError::PageWindowMismatch);
    }
    if let Some(previous) = &checkpoint.last_page_observed_at
        && observed_at < parse_canonical_utc_timestamp("last_page_observed_at", previous)?
    {
        return Err(HostedSlackPollError::PageWindowMismatch);
    }
    Ok(())
}

fn replay_outcome(
    checkpoint: &HostedSlackPollCheckpointV1,
    phase: HostedSlackPollPhaseV1,
    root_message_id: Option<&str>,
    request_cursor: Option<&str>,
    canonical_page_json: &str,
) -> Result<Option<HostedSlackPageApplyOutcomeV1>, HostedSlackPollError> {
    let existing = checkpoint
        .evidence
        .iter()
        .find_map(|evidence| match evidence {
            HostedSlackPollEvidenceV1::AppliedPage { page }
                if page.phase == phase
                    && page.root_message_id.as_deref() == root_message_id
                    && page.request_cursor.as_deref() == request_cursor =>
            {
                Some(page)
            }
            _ => None,
        });
    match existing {
        Some(existing) if existing.canonical_page_json == canonical_page_json => {
            Ok(Some(HostedSlackPageApplyOutcomeV1::ExactReplay))
        }
        Some(_) => Err(HostedSlackPollError::ConflictingReplay),
        None => Ok(None),
    }
}

fn validate_next_cursor(
    checkpoint: &HostedSlackPollCheckpointV1,
    phase: HostedSlackPollPhaseV1,
    root_message_id: Option<&str>,
    request_cursor: Option<&str>,
    next_cursor: Option<&str>,
) -> Result<(), HostedSlackPollError> {
    let Some(next_cursor) = next_cursor else {
        return Ok(());
    };
    if Some(next_cursor) == request_cursor
        || checkpoint.evidence.iter().any(|evidence| match evidence {
            HostedSlackPollEvidenceV1::AppliedPage { page } => {
                page.phase == phase
                    && page.root_message_id.as_deref() == root_message_id
                    && (page.request_cursor.as_deref() == Some(next_cursor)
                        || page.next_cursor.as_deref() == Some(next_cursor))
            }
            HostedSlackPollEvidenceV1::BeginCatchUp { .. } => false,
        })
    {
        return Err(HostedSlackPollError::CursorCycle);
    }
    Ok(())
}

fn history_window(
    checkpoint: &HostedSlackPollCheckpointV1,
    phase: HostedSlackPollPhaseV1,
) -> Result<(chrono::DateTime<Utc>, chrono::DateTime<Utc>), HostedSlackPollError> {
    match phase {
        HostedSlackPollPhaseV1::HistoricalHistory => Ok((
            parse_canonical_utc_timestamp(
                "authorized_history_start_at",
                &checkpoint.authorized_history_start_at,
            )?,
            parse_canonical_utc_timestamp("backfill_cut_at", &checkpoint.backfill_cut_at)?,
        )),
        HostedSlackPollPhaseV1::CatchUpHistory => Ok((
            parse_canonical_utc_timestamp(
                "poll_overlap_watermark",
                &checkpoint.poll_overlap_watermark,
            )?,
            parse_canonical_utc_timestamp(
                "poll_cut_at",
                checkpoint
                    .poll_cut_at
                    .as_deref()
                    .ok_or(HostedSlackPollError::PageWindowMismatch)?,
            )?,
        )),
        _ => Err(HostedSlackPollError::PageWindowMismatch),
    }
}

fn reply_window_end(
    checkpoint: &HostedSlackPollCheckpointV1,
    phase: HostedSlackPollPhaseV1,
) -> Result<chrono::DateTime<Utc>, HostedSlackPollError> {
    match phase {
        HostedSlackPollPhaseV1::HistoricalReplies => {
            parse_canonical_utc_timestamp("backfill_cut_at", &checkpoint.backfill_cut_at)
        }
        HostedSlackPollPhaseV1::CatchUpReplies => parse_canonical_utc_timestamp(
            "poll_cut_at",
            checkpoint
                .poll_cut_at
                .as_deref()
                .ok_or(HostedSlackPollError::PageWindowMismatch)?,
        ),
        _ => Err(HostedSlackPollError::PageWindowMismatch),
    }
}

fn prepare_reply_phase(
    checkpoint: &mut HostedSlackPollCheckpointV1,
    history_phase: HostedSlackPollPhaseV1,
) -> Result<(), HostedSlackPollError> {
    let reply_phase = match history_phase {
        HostedSlackPollPhaseV1::HistoricalHistory => HostedSlackPollPhaseV1::HistoricalReplies,
        HostedSlackPollPhaseV1::CatchUpHistory => HostedSlackPollPhaseV1::CatchUpReplies,
        _ => return Err(HostedSlackPollError::PageWindowMismatch),
    };
    checkpoint.history_cursor = None;
    let touched = checkpoint
        .candidate
        .stage_root_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    for root in &touched {
        if !checkpoint
            .candidate
            .messages
            .iter()
            .any(|message| message.ts == *root && normalized_root_id(message).is_none())
        {
            return Err(HostedSlackPollError::MissingRoot(root.clone()));
        }
    }
    let pending = if history_phase == HostedSlackPollPhaseV1::HistoricalHistory {
        checkpoint
            .completed_roots
            .retain(|root| !touched.contains(&root.root_message_id));
        checkpoint.candidate.messages.retain(|message| {
            normalized_root_id(message).is_none_or(|root| !touched.contains(root))
        });

        let yielded = checkpoint
            .candidate
            .stage_yielded_reply_root_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut pending = Vec::new();
        for root in &checkpoint.candidate.stage_root_ids {
            let expected = expected_reply_count(checkpoint, root)?;
            if expected == 0 && !yielded.contains(root) {
                checkpoint.completed_roots.push(HostedSlackCompletedRootV1 {
                    root_message_id: root.clone(),
                    expected_reply_count: 0,
                    observed_reply_count: 0,
                    completed_phase: reply_phase,
                });
            } else {
                pending.push(root.clone());
            }
        }
        pending
    } else {
        let roots = checkpoint
            .candidate
            .messages
            .iter()
            .filter(|message| normalized_root_id(message).is_none())
            .map(|message| message.ts.clone())
            .collect::<Vec<_>>();
        let applied_pages = checkpoint
            .evidence
            .iter()
            .filter(|evidence| matches!(evidence, HostedSlackPollEvidenceV1::AppliedPage { .. }))
            .count();
        let minimum_required_pages = applied_pages.saturating_add(roots.len());
        if minimum_required_pages > MAX_HOSTED_SLACK_APPLIED_PAGES_V1 {
            return Err(HostedSlackPollError::CollectionTooLarge(
                "catch-up root sweep",
            ));
        }
        checkpoint.completed_roots.clear();
        roots
    };
    checkpoint
        .completed_roots
        .sort_by(|left, right| left.root_message_id.cmp(&right.root_message_id));
    checkpoint.candidate.stage_root_ids = pending;
    checkpoint.candidate.stage_yielded_reply_root_ids.clear();
    checkpoint.candidate.current_root_reply_message_ids.clear();
    checkpoint.reply_cursor = None;
    checkpoint.current_root_message_id = checkpoint.candidate.stage_root_ids.first().cloned();
    if checkpoint.current_root_message_id.is_some() {
        checkpoint.phase = reply_phase;
    } else if history_phase == HostedSlackPollPhaseV1::HistoricalHistory {
        checkpoint.phase = HostedSlackPollPhaseV1::AwaitingCatchUpCut;
    } else {
        checkpoint.phase = HostedSlackPollPhaseV1::CompleteCandidate;
    }
    normalize_candidate(checkpoint);
    rebuild_candidate_reference_metadata(checkpoint)?;
    if checkpoint.phase == HostedSlackPollPhaseV1::CompleteCandidate {
        build_snapshot(checkpoint)?;
    }
    Ok(())
}

fn complete_current_root(
    checkpoint: &mut HostedSlackPollCheckpointV1,
    reply_phase: HostedSlackPollPhaseV1,
) -> Result<(), HostedSlackPollError> {
    let root = checkpoint
        .current_root_message_id
        .clone()
        .ok_or(HostedSlackPollError::IncompleteCandidate("current root"))?;
    let expected = expected_reply_count(checkpoint, &root)?;
    let final_reply_ids = checkpoint
        .candidate
        .messages
        .iter()
        .filter(|message| normalized_root_id(message) == Some(root.as_str()))
        .map(|message| message.ts.clone())
        .collect::<BTreeSet<_>>();
    let collected_reply_ids = checkpoint
        .candidate
        .current_root_reply_message_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if final_reply_ids != collected_reply_ids {
        return Err(HostedSlackPollError::IncompleteCandidate(
            "root reply membership",
        ));
    }
    let actual = u32::try_from(final_reply_ids.len())
        .map_err(|_| HostedSlackPollError::CollectionTooLarge("root replies"))?;
    if expected != actual {
        return Err(HostedSlackPollError::ReplyCountMismatch {
            root_message_id: root,
            expected,
            actual,
        });
    }
    checkpoint.completed_roots.push(HostedSlackCompletedRootV1 {
        root_message_id: root.clone(),
        expected_reply_count: expected,
        observed_reply_count: actual,
        completed_phase: reply_phase,
    });
    checkpoint
        .completed_roots
        .sort_by(|left, right| left.root_message_id.cmp(&right.root_message_id));
    checkpoint
        .candidate
        .stage_root_ids
        .retain(|value| value != &root);
    checkpoint.candidate.current_root_reply_message_ids.clear();
    checkpoint.reply_cursor = None;
    checkpoint.current_root_message_id = checkpoint.candidate.stage_root_ids.first().cloned();
    if checkpoint.current_root_message_id.is_none() {
        if reply_phase == HostedSlackPollPhaseV1::HistoricalReplies {
            checkpoint.phase = HostedSlackPollPhaseV1::AwaitingCatchUpCut;
        } else {
            checkpoint.phase = HostedSlackPollPhaseV1::CompleteCandidate;
        }
    }
    normalize_candidate(checkpoint);
    Ok(())
}

fn catch_up_history_contains_root(
    checkpoint: &HostedSlackPollCheckpointV1,
    root: &str,
) -> Result<bool, HostedSlackPollError> {
    for evidence in &checkpoint.evidence {
        let HostedSlackPollEvidenceV1::AppliedPage { page } = evidence else {
            continue;
        };
        if page.phase != HostedSlackPollPhaseV1::CatchUpHistory {
            continue;
        }
        let history =
            serde_json::from_str::<HostedSlackHistoryPageV1>(&page.canonical_page_json)
                .map_err(|_| HostedSlackPollError::InvalidJson("checkpoint replay evidence"))?;
        if history.messages.iter().any(|wrapped| {
            wrapped.message.ts == root && normalized_root_id(&wrapped.message).is_none()
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn build_snapshot(
    checkpoint: &HostedSlackPollCheckpointV1,
) -> Result<HostedSlackNativeSnapshot, HostedSlackPollError> {
    let roots = checkpoint
        .candidate
        .messages
        .iter()
        .filter(|message| normalized_root_id(message).is_none())
        .map(|message| message.ts.as_str())
        .collect::<BTreeSet<_>>();
    let completed = checkpoint
        .completed_roots
        .iter()
        .map(|root| root.root_message_id.as_str())
        .collect::<BTreeSet<_>>();
    if roots != completed {
        return Err(HostedSlackPollError::IncompleteCandidate(
            "root completion evidence",
        ));
    }
    for completed_root in &checkpoint.completed_roots {
        let actual = u32::try_from(
            checkpoint
                .candidate
                .messages
                .iter()
                .filter(|message| {
                    normalized_root_id(message) == Some(completed_root.root_message_id.as_str())
                })
                .count(),
        )
        .map_err(|_| HostedSlackPollError::CollectionTooLarge("root replies"))?;
        if actual != completed_root.expected_reply_count {
            return Err(HostedSlackPollError::ReplyCountMismatch {
                root_message_id: completed_root.root_message_id.clone(),
                expected: completed_root.expected_reply_count,
                actual,
            });
        }
    }
    let mut threads = roots
        .iter()
        .map(|root| RawHostedSlackThread {
            channel_id: checkpoint.channel_id.clone(),
            root_ts: (*root).to_string(),
            reply_ts: checkpoint
                .candidate
                .messages
                .iter()
                .filter(|message| normalized_root_id(message) == Some(*root))
                .map(|message| message.ts.clone())
                .collect(),
        })
        .collect::<Vec<_>>();
    for thread in &mut threads {
        thread.reply_ts.sort();
    }
    let raw = RawHostedSlackNativeSnapshot {
        installation_id: checkpoint.installation_id.clone(),
        channel: checkpoint.channel.clone(),
        users: checkpoint.candidate.users.clone(),
        messages: checkpoint.candidate.messages.clone(),
        threads,
        files: checkpoint.candidate.files.clone(),
    };
    HostedSlackNativeSnapshot::try_from(raw).map_err(HostedSlackPollError::InvalidNative)
}

fn accepted_history_evidence_page(
    checkpoint: &HostedSlackPollCheckpointV1,
    source: &HostedSlackHistoryPageV1,
) -> Result<HostedSlackHistoryPageV1, HostedSlackPollError> {
    let (window_start, window_end) = history_window(checkpoint, source.phase)?;
    let history_start = parse_canonical_utc_timestamp(
        "authorized_history_start_at",
        &checkpoint.authorized_history_start_at,
    )?;
    let mut messages = Vec::new();
    for wrapped in &source.messages {
        let message_time = parse_slack_timestamp("page.message.ts", &wrapped.message.ts)?;
        let root_id = normalized_root_id(&wrapped.message).unwrap_or(&wrapped.message.ts);
        let root_time = parse_slack_timestamp("page.message.thread_ts", root_id)?;
        if root_time < history_start {
            continue;
        }
        if message_time < window_start || message_time >= window_end {
            return Err(HostedSlackPollError::PageWindowMismatch);
        }
        messages.push(wrapped.clone());
    }
    let (users, files) = retained_page_metadata(
        messages.iter().map(|wrapped| &wrapped.message),
        &source.users,
        &source.files,
    );
    let mut accepted = source.clone();
    accepted.messages = messages;
    accepted.users = users;
    accepted.files = files;
    Ok(accepted)
}

fn accepted_replies_evidence_page(source: &HostedSlackRepliesPageV1) -> HostedSlackRepliesPageV1 {
    let (users, files) =
        retained_page_metadata(source.messages.iter(), &source.users, &source.files);
    let mut accepted = source.clone();
    accepted.users = users;
    accepted.files = files;
    accepted
}

fn retained_page_metadata<'a>(
    messages: impl Iterator<Item = &'a RawHostedSlackMessage>,
    page_users: &[RawHostedSlackUser],
    page_files: &[RawHostedSlackFileMetadata],
) -> (Vec<RawHostedSlackUser>, Vec<RawHostedSlackFileMetadata>) {
    let messages = messages.collect::<Vec<_>>();
    let referenced_file_ids = messages
        .iter()
        .flat_map(|message| message.file_ids.iter().map(String::as_str))
        .collect::<BTreeSet<_>>();
    let mut files = page_files
        .iter()
        .filter(|file| referenced_file_ids.contains(file.id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.id.cmp(&right.id));

    let mut referenced_user_ids = messages
        .iter()
        .filter_map(|message| message.user_id.as_deref())
        .collect::<BTreeSet<_>>();
    referenced_user_ids.extend(files.iter().filter_map(|file| file.user_id.as_deref()));
    let mut users = page_users
        .iter()
        .filter(|user| referenced_user_ids.contains(user.id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    users.sort_by(|left, right| left.id.cmp(&right.id));
    (users, files)
}

fn rebuild_candidate_reference_metadata(
    checkpoint: &mut HostedSlackPollCheckpointV1,
) -> Result<(), HostedSlackPollError> {
    let referenced_file_ids = checkpoint
        .candidate
        .messages
        .iter()
        .flat_map(|message| message.file_ids.iter().cloned())
        .collect::<BTreeSet<_>>();
    let mut files = Vec::new();
    for evidence in &checkpoint.evidence {
        let HostedSlackPollEvidenceV1::AppliedPage { page } = evidence else {
            continue;
        };
        let (_, page_files) = page_reference_metadata(page)?;
        for file in page_files {
            if referenced_file_ids.contains(&file.id) {
                upsert_by(&mut files, file, |value| &value.id);
            }
        }
    }

    let mut referenced_user_ids = checkpoint
        .candidate
        .messages
        .iter()
        .filter_map(|message| message.user_id.clone())
        .collect::<BTreeSet<_>>();
    referenced_user_ids.extend(files.iter().filter_map(|file| file.user_id.clone()));
    let mut users = Vec::new();
    for evidence in &checkpoint.evidence {
        let HostedSlackPollEvidenceV1::AppliedPage { page } = evidence else {
            continue;
        };
        let (page_users, _) = page_reference_metadata(page)?;
        for user in page_users {
            if referenced_user_ids.contains(&user.id) {
                upsert_by(&mut users, user, |value| &value.id);
            }
        }
    }
    checkpoint.candidate.files = files;
    checkpoint.candidate.users = users;
    Ok(())
}

fn page_reference_metadata(
    record: &HostedSlackAppliedPageV1,
) -> Result<(Vec<RawHostedSlackUser>, Vec<RawHostedSlackFileMetadata>), HostedSlackPollError> {
    match record.phase {
        HostedSlackPollPhaseV1::HistoricalHistory | HostedSlackPollPhaseV1::CatchUpHistory => {
            let page =
                serde_json::from_str::<HostedSlackHistoryPageV1>(&record.canonical_page_json)
                    .map_err(|_| HostedSlackPollError::InvalidJson("checkpoint replay evidence"))?;
            Ok((page.users, page.files))
        }
        HostedSlackPollPhaseV1::HistoricalReplies | HostedSlackPollPhaseV1::CatchUpReplies => {
            let page =
                serde_json::from_str::<HostedSlackRepliesPageV1>(&record.canonical_page_json)
                    .map_err(|_| HostedSlackPollError::InvalidJson("checkpoint replay evidence"))?;
            Ok((page.users, page.files))
        }
        _ => Err(HostedSlackPollError::IncompleteCandidate(
            "applied page phase",
        )),
    }
}

fn messages_seen_in_refresh(
    checkpoint: &HostedSlackPollCheckpointV1,
    incoming_phase: HostedSlackPollPhaseV1,
) -> Result<BTreeMap<String, RawHostedSlackMessage>, HostedSlackPollError> {
    let mut seen = BTreeMap::new();
    for evidence in &checkpoint.evidence {
        let HostedSlackPollEvidenceV1::AppliedPage { page } = evidence else {
            continue;
        };
        let relevant = match incoming_phase {
            HostedSlackPollPhaseV1::HistoricalHistory => {
                page.phase == HostedSlackPollPhaseV1::HistoricalHistory
            }
            HostedSlackPollPhaseV1::HistoricalReplies => matches!(
                page.phase,
                HostedSlackPollPhaseV1::HistoricalHistory
                    | HostedSlackPollPhaseV1::HistoricalReplies
            ),
            HostedSlackPollPhaseV1::CatchUpHistory => {
                page.phase == HostedSlackPollPhaseV1::CatchUpHistory
            }
            HostedSlackPollPhaseV1::CatchUpReplies => matches!(
                page.phase,
                HostedSlackPollPhaseV1::CatchUpHistory | HostedSlackPollPhaseV1::CatchUpReplies
            ),
            _ => false,
        };
        if !relevant {
            continue;
        }
        match page.phase {
            HostedSlackPollPhaseV1::HistoricalHistory | HostedSlackPollPhaseV1::CatchUpHistory => {
                let history =
                    serde_json::from_str::<HostedSlackHistoryPageV1>(&page.canonical_page_json)
                        .map_err(|_| {
                            HostedSlackPollError::InvalidJson("checkpoint replay evidence")
                        })?;
                for wrapped in history.messages {
                    insert_seen_message(&mut seen, wrapped.message)?;
                }
            }
            HostedSlackPollPhaseV1::HistoricalReplies | HostedSlackPollPhaseV1::CatchUpReplies => {
                let replies =
                    serde_json::from_str::<HostedSlackRepliesPageV1>(&page.canonical_page_json)
                        .map_err(|_| {
                            HostedSlackPollError::InvalidJson("checkpoint replay evidence")
                        })?;
                for message in replies.messages {
                    insert_seen_message(&mut seen, message)?;
                }
            }
            _ => {}
        }
    }
    Ok(seen)
}

fn insert_seen_message(
    seen: &mut BTreeMap<String, RawHostedSlackMessage>,
    message: RawHostedSlackMessage,
) -> Result<(), HostedSlackPollError> {
    if let Some(existing) = seen.get(&message.ts) {
        if normalized_root_id(existing) != normalized_root_id(&message) {
            return Err(HostedSlackPollError::InvalidMessageRelationship(message.ts));
        }
        if existing != &message {
            return Err(HostedSlackPollError::ConflictingMessage(message.ts));
        }
        return Ok(());
    }
    seen.insert(message.ts.clone(), message);
    Ok(())
}

fn apply_message_current_state(
    messages: &mut Vec<RawHostedSlackMessage>,
    message: RawHostedSlackMessage,
    phase: HostedSlackPollPhaseV1,
    seen_in_refresh: Option<&RawHostedSlackMessage>,
) -> Result<(), HostedSlackPollError> {
    if let Some(seen) = seen_in_refresh {
        if normalized_root_id(seen) != normalized_root_id(&message) {
            return Err(HostedSlackPollError::InvalidMessageRelationship(message.ts));
        }
        if seen != &message {
            return Err(HostedSlackPollError::ConflictingMessage(message.ts));
        }
    }
    let Some(index) = messages
        .iter()
        .position(|existing| existing.ts == message.ts)
    else {
        messages.push(message);
        messages.sort_by(|left, right| left.ts.cmp(&right.ts));
        return Ok(());
    };
    let existing = &messages[index];
    if normalized_root_id(existing) != normalized_root_id(&message) {
        return Err(HostedSlackPollError::InvalidMessageRelationship(message.ts));
    }
    if existing == &message {
        return Ok(());
    }
    let is_current_state_refresh = matches!(
        phase,
        HostedSlackPollPhaseV1::CatchUpHistory | HostedSlackPollPhaseV1::CatchUpReplies
    );
    if seen_in_refresh.is_some() || !is_current_state_refresh {
        return Err(HostedSlackPollError::ConflictingMessage(message.ts));
    }
    messages[index] = message;
    Ok(())
}

fn upsert_root_expectation(
    values: &mut Vec<HostedSlackRootExpectationV1>,
    value: HostedSlackRootExpectationV1,
) {
    upsert_by(values, value, |value| &value.root_message_id);
}

fn upsert_root_expectation_checked(
    values: &mut Vec<HostedSlackRootExpectationV1>,
    value: HostedSlackRootExpectationV1,
    seen_in_refresh: bool,
) -> Result<(), HostedSlackPollError> {
    if let Some(existing) = values
        .iter()
        .find(|existing| existing.root_message_id == value.root_message_id)
        && existing.expected_reply_count != value.expected_reply_count
        && seen_in_refresh
    {
        return Err(HostedSlackPollError::ReplyCountMismatch {
            root_message_id: value.root_message_id,
            expected: existing.expected_reply_count,
            actual: value.expected_reply_count,
        });
    }
    upsert_root_expectation(values, value);
    Ok(())
}

fn upsert_by<T>(values: &mut Vec<T>, value: T, key: impl Fn(&T) -> &String) {
    if let Some(index) = values
        .iter()
        .position(|existing| key(existing) == key(&value))
    {
        values[index] = value;
    } else {
        values.push(value);
    }
    values.sort_by(|left, right| key(left).cmp(key(right)));
}

fn insert_sorted_unique(values: &mut Vec<String>, value: String) {
    match values.binary_search(&value) {
        Ok(_) => {}
        Err(index) => values.insert(index, value),
    }
}

fn normalize_candidate(checkpoint: &mut HostedSlackPollCheckpointV1) {
    checkpoint
        .candidate
        .users
        .sort_by(|left, right| left.id.cmp(&right.id));
    checkpoint
        .candidate
        .messages
        .sort_by(|left, right| left.ts.cmp(&right.ts));
    checkpoint
        .candidate
        .files
        .sort_by(|left, right| left.id.cmp(&right.id));
    checkpoint
        .candidate
        .root_expectations
        .sort_by(|left, right| left.root_message_id.cmp(&right.root_message_id));
    checkpoint.latest_observed_message_timestamp = checkpoint
        .candidate
        .messages
        .iter()
        .map(|message| message.ts.clone())
        .max_by(|left, right| compare_slack_timestamps(left, right));
}

fn expected_reply_count(
    checkpoint: &HostedSlackPollCheckpointV1,
    root: &str,
) -> Result<u32, HostedSlackPollError> {
    checkpoint
        .candidate
        .root_expectations
        .iter()
        .find(|expectation| expectation.root_message_id == root)
        .map(|expectation| expectation.expected_reply_count)
        .ok_or_else(|| HostedSlackPollError::MissingRoot(root.to_string()))
}

fn normalized_root_id(message: &RawHostedSlackMessage) -> Option<&str> {
    message
        .thread_ts
        .as_deref()
        .filter(|thread_ts| *thread_ts != message.ts)
}

fn record_page(
    checkpoint: &mut HostedSlackPollCheckpointV1,
    phase: HostedSlackPollPhaseV1,
    root_message_id: Option<String>,
    request_cursor: Option<String>,
    next_cursor: Option<String>,
    canonical_page_json: String,
) {
    checkpoint
        .evidence
        .push(HostedSlackPollEvidenceV1::AppliedPage {
            page: HostedSlackAppliedPageV1 {
                phase,
                root_message_id,
                request_cursor,
                next_cursor,
                canonical_page_json,
            },
        });
}

fn ensure_unique_page_values<'a>(
    field: &'static str,
    values: impl Iterator<Item = &'a str>,
) -> Result<(), HostedSlackPollError> {
    let mut seen = BTreeSet::new();
    if values.into_iter().any(|value| !seen.insert(value)) {
        return Err(HostedSlackPollError::DuplicateValue(field));
    }
    Ok(())
}
