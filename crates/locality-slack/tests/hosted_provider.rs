use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use locality_protocol::{
    HostedSlackChannelSelector, ProviderSourceScopeSelector, SlackChannelSharingClassification,
};
use locality_slack::portable::hosted::{
    HostedSlackCancellationToken, HostedSlackDriveControlV1, HostedSlackDriveOutcomeV1,
    HostedSlackDrivePendingReasonV1, HostedSlackHistoryPageV1, HostedSlackInstallationBinding,
    HostedSlackObservedChannelAuthorityV1, HostedSlackObservedInstallationIdentity,
    HostedSlackPollCheckpointV1, HostedSlackPollError, HostedSlackPollKindV1,
    HostedSlackPollPhaseV1, HostedSlackProviderError, HostedSlackProviderFuture,
    HostedSlackProviderMessagePageV1, HostedSlackProviderMessageV1, HostedSlackProviderPort,
    HostedSlackProviderRequestV1, HostedSlackRepliesPageV1, RawHostedSlackFileMetadata,
    RawHostedSlackMessage, RawHostedSlackNativeSnapshot, RawHostedSlackUser,
    decode_hosted_slack_history_page_v1, decode_hosted_slack_poll_checkpoint_v1,
    decode_hosted_slack_replies_page_v1, drive_hosted_slack_poll_v1,
};

const SELECTOR: &[u8] =
    include_bytes!("../../locality-protocol/fixtures/hosted-slack-channel-selector-v1.json");
const BINDING: &[u8] = include_bytes!("../fixtures/hosted-v1/installation-binding.json");
const NATIVE_RAW: &[u8] = include_bytes!("../fixtures/hosted-v1/native-raw.json");
const HISTORY_PAGE: &[u8] = include_bytes!("../fixtures/hosted-v1/poll-v1/history-page-v1.json");
const REPLIES_PAGE: &[u8] = include_bytes!("../fixtures/hosted-v1/poll-v1/replies-page-v1.json");
const LATE_REPLIES_PAGE: &[u8] =
    include_bytes!("../fixtures/hosted-v1/poll-v1/catch-up-old-root-replies-page-v1.json");
const INITIAL_TRANSCRIPT: &[u8] =
    include_bytes!("../fixtures/hosted-v1/provider-v1/initial-repair-transcript-v1.json");
const LATE_TRANSCRIPT: &[u8] =
    include_bytes!("../fixtures/hosted-v1/provider-v1/late-old-root-transcript-v1.json");
const RATE_TRANSCRIPT: &[u8] =
    include_bytes!("../fixtures/hosted-v1/provider-v1/rate-limit-transcript-v1.json");
const IDENTITY_ONLY_TRANSCRIPT: &[u8] =
    include_bytes!("../fixtures/hosted-v1/provider-v1/identity-only-transcript-v1.json");
const CHANNEL_AUTHORITY_TRANSCRIPT: &[u8] =
    include_bytes!("../fixtures/hosted-v1/provider-v1/channel-authority-transcript-v1.json");
const HISTORICAL_PAGE_ONE_TRANSCRIPT: &[u8] =
    include_bytes!("../fixtures/hosted-v1/provider-v1/historical-page-one-transcript-v1.json");
const CURSOR_CONFLICT_TRANSCRIPT: &[u8] =
    include_bytes!("../fixtures/hosted-v1/provider-v1/cursor-conflict-transcript-v1.json");
const RESUMED_HISTORY_TRANSCRIPT: &[u8] =
    include_bytes!("../fixtures/hosted-v1/provider-v1/resumed-history-transcript-v1.json");

#[derive(Debug)]
struct FakeProvider {
    script: Mutex<VecDeque<ScriptedResponse>>,
    transcript: Mutex<Vec<HostedSlackProviderRequestV1>>,
}

#[derive(Debug)]
enum ScriptedResponse {
    Identity(Result<HostedSlackObservedInstallationIdentity, HostedSlackProviderError>),
    Authority(Result<HostedSlackObservedChannelAuthorityV1, HostedSlackProviderError>),
    History(Result<HostedSlackProviderMessagePageV1, HostedSlackProviderError>),
    HistoryAndCancel(
        Result<HostedSlackProviderMessagePageV1, HostedSlackProviderError>,
        HostedSlackCancellationToken,
    ),
    PendingHistory,
    Replies(Result<HostedSlackProviderMessagePageV1, HostedSlackProviderError>),
    User(Result<RawHostedSlackUser, HostedSlackProviderError>),
    File(Result<RawHostedSlackFileMetadata, HostedSlackProviderError>),
}

impl FakeProvider {
    fn new(script: impl IntoIterator<Item = ScriptedResponse>) -> Self {
        Self {
            script: Mutex::new(script.into_iter().collect()),
            transcript: Mutex::new(Vec::new()),
        }
    }

    fn transcript(&self) -> Vec<HostedSlackProviderRequestV1> {
        self.transcript.lock().unwrap().clone()
    }

    fn assert_exhausted(&self) {
        assert!(self.script.lock().unwrap().is_empty());
    }

    fn pop(&self, expected: &'static str) -> ScriptedResponse {
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("missing scripted {expected} response"))
    }
}

impl HostedSlackProviderPort for FakeProvider {
    fn verify_installation(
        &self,
    ) -> HostedSlackProviderFuture<'_, HostedSlackObservedInstallationIdentity> {
        Box::pin(async move {
            self.transcript
                .lock()
                .unwrap()
                .push(HostedSlackProviderRequestV1::VerifyInstallation);
            match self.pop("identity") {
                ScriptedResponse::Identity(result) => result,
                other => panic!("expected identity response, got {other:?}"),
            }
        })
    }

    fn conversations_info(
        &self,
        channel_id: String,
    ) -> HostedSlackProviderFuture<'_, HostedSlackObservedChannelAuthorityV1> {
        Box::pin(async move {
            self.transcript
                .lock()
                .unwrap()
                .push(HostedSlackProviderRequestV1::ConversationsInfo { channel_id });
            match self.pop("channel authority") {
                ScriptedResponse::Authority(result) => result,
                other => panic!("expected channel authority response, got {other:?}"),
            }
        })
    }

    fn conversations_history(
        &self,
        request: locality_slack::portable::hosted::HostedSlackHistoryRequestV1,
    ) -> HostedSlackProviderFuture<'_, HostedSlackProviderMessagePageV1> {
        Box::pin(async move {
            self.transcript.lock().unwrap().push(
                HostedSlackProviderRequestV1::ConversationsHistory {
                    request: request.clone(),
                },
            );
            match self.pop("history") {
                ScriptedResponse::History(result) => result,
                ScriptedResponse::HistoryAndCancel(result, cancellation) => {
                    cancellation.cancel();
                    result
                }
                ScriptedResponse::PendingHistory => std::future::pending().await,
                other => panic!("expected history response, got {other:?}"),
            }
        })
    }

    fn conversations_replies(
        &self,
        request: locality_slack::portable::hosted::HostedSlackRepliesRequestV1,
    ) -> HostedSlackProviderFuture<'_, HostedSlackProviderMessagePageV1> {
        Box::pin(async move {
            self.transcript.lock().unwrap().push(
                HostedSlackProviderRequestV1::ConversationsReplies {
                    request: request.clone(),
                },
            );
            match self.pop("replies") {
                ScriptedResponse::Replies(result) => result,
                other => panic!("expected replies response, got {other:?}"),
            }
        })
    }

    fn users_info(&self, user_id: String) -> HostedSlackProviderFuture<'_, RawHostedSlackUser> {
        Box::pin(async move {
            self.transcript
                .lock()
                .unwrap()
                .push(HostedSlackProviderRequestV1::UsersInfo { user_id });
            match self.pop("user") {
                ScriptedResponse::User(result) => result,
                other => panic!("expected user response, got {other:?}"),
            }
        })
    }

    fn files_info(
        &self,
        file_id: String,
        channel_id: String,
    ) -> HostedSlackProviderFuture<'_, RawHostedSlackFileMetadata> {
        Box::pin(async move {
            self.transcript
                .lock()
                .unwrap()
                .push(HostedSlackProviderRequestV1::FilesInfo {
                    file_id,
                    channel_id,
                });
            match self.pop("file") {
                ScriptedResponse::File(result) => result,
                other => panic!("expected file response, got {other:?}"),
            }
        })
    }
}

fn selector() -> HostedSlackChannelSelector {
    match serde_json::from_slice::<ProviderSourceScopeSelector>(SELECTOR).unwrap() {
        ProviderSourceScopeSelector::HostedSlackChannel(mut selector) => {
            selector.sharing = SlackChannelSharingClassification::Private;
            selector
        }
        other => panic!("unexpected selector: {other:?}"),
    }
}

fn binding() -> HostedSlackInstallationBinding {
    serde_json::from_slice(BINDING).unwrap()
}

fn observed() -> HostedSlackObservedInstallationIdentity {
    let binding = binding();
    HostedSlackObservedInstallationIdentity {
        api_app_id: binding.api_app_id,
        team_id: binding.team_id,
        enterprise_id: binding.enterprise_id,
        enterprise_install: binding.enterprise_install,
        bot_user_id: binding.bot_user_id,
        oauth_subject_id: binding.oauth_subject_id,
    }
}

fn authority() -> HostedSlackObservedChannelAuthorityV1 {
    HostedSlackObservedChannelAuthorityV1 {
        team_id: "T08LOCALITY1".to_string(),
        channel_id: "C08ENGINEER1".to_string(),
        is_private: true,
        is_shared: false,
        is_externally_shared: false,
        is_org_shared: false,
        is_member: true,
        shared_team_ids: vec!["T08LOCALITY1".to_string()],
    }
}

fn raw_snapshot() -> RawHostedSlackNativeSnapshot {
    let mut snapshot: RawHostedSlackNativeSnapshot = serde_json::from_slice(NATIVE_RAW).unwrap();
    snapshot.channel.sharing = SlackChannelSharingClassification::Private;
    snapshot
}

fn new_checkpoint(overlap: &str) -> HostedSlackPollCheckpointV1 {
    HostedSlackPollCheckpointV1::new(
        &selector(),
        raw_snapshot().channel,
        HostedSlackPollKindV1::FullRepair,
        "2026-06-01T00:00:00Z".to_string(),
        overlap.to_string(),
    )
    .unwrap()
}

fn history_page() -> HostedSlackHistoryPageV1 {
    let mut page = decode_hosted_slack_history_page_v1(HISTORY_PAGE).unwrap();
    page.sharing = SlackChannelSharingClassification::Private;
    page
}

fn history_terminal_page() -> HostedSlackHistoryPageV1 {
    let mut page = history_page();
    page.request_cursor = page.next_cursor.take();
    page.observed_at = "2026-06-01T00:00:02Z".to_string();
    page.messages.clear();
    page.users.clear();
    page.files.clear();
    page
}

fn replies_page() -> HostedSlackRepliesPageV1 {
    let mut page = decode_hosted_slack_replies_page_v1(REPLIES_PAGE).unwrap();
    page.sharing = SlackChannelSharingClassification::Private;
    page
}

fn replies_terminal_page() -> HostedSlackRepliesPageV1 {
    let mut page = replies_page();
    page.request_cursor = page.next_cursor.take();
    page.observed_at = "2026-06-01T00:00:04Z".to_string();
    page.messages = vec![
        raw_snapshot()
            .messages
            .into_iter()
            .find(|message| message.ts == "1780000002.000300")
            .unwrap(),
    ];
    page.users.clear();
    page.files.clear();
    page
}

fn catch_up_history_page(overlap: &str) -> HostedSlackHistoryPageV1 {
    let mut page = history_page();
    page.phase = HostedSlackPollPhaseV1::CatchUpHistory;
    page.poll_kind = HostedSlackPollKindV1::FullRepair;
    page.poll_cut_at = Some("2026-06-02T00:00:00Z".to_string());
    page.poll_overlap_watermark = overlap.to_string();
    page.request_cursor = None;
    page.next_cursor = None;
    page.observed_at = "2026-06-02T00:00:05Z".to_string();
    page.messages.clear();
    page.users.clear();
    page.files.clear();
    page
}

fn as_history_response(page: &HostedSlackHistoryPageV1) -> HostedSlackProviderMessagePageV1 {
    HostedSlackProviderMessagePageV1 {
        observed_at: page.observed_at.clone(),
        has_more: Some(page.next_cursor.is_some()),
        next_cursor: page.next_cursor.clone(),
        messages: page
            .messages
            .iter()
            .map(|wrapped| HostedSlackProviderMessageV1 {
                message: wrapped.message.clone(),
                reply_count: (wrapped.message.thread_ts.is_none()
                    || wrapped.message.thread_ts.as_deref() == Some(wrapped.message.ts.as_str()))
                .then_some(wrapped.reply_count),
            })
            .collect(),
    }
}

fn as_replies_response(page: &HostedSlackRepliesPageV1) -> HostedSlackProviderMessagePageV1 {
    HostedSlackProviderMessagePageV1 {
        observed_at: page.observed_at.clone(),
        has_more: Some(page.next_cursor.is_some()),
        next_cursor: page.next_cursor.clone(),
        messages: page
            .messages
            .iter()
            .map(|message| HostedSlackProviderMessageV1 {
                message: message.clone(),
                reply_count: (message.ts == page.root_message_id).then_some(page.root_reply_count),
            })
            .collect(),
    }
}

fn exact_json(value: &impl serde::Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).unwrap();
    bytes.push(b'\n');
    bytes
}

fn control(cut: Option<&str>) -> HostedSlackDriveControlV1 {
    HostedSlackDriveControlV1::new(
        Instant::now() + Duration::from_secs(60 * 60),
        HostedSlackCancellationToken::new(),
        cut.map(str::to_string),
    )
}

fn metadata_script(page: &HostedSlackHistoryPageV1) -> Vec<ScriptedResponse> {
    let mut files = page.files.clone();
    files.sort_by(|left, right| left.id.cmp(&right.id));
    let mut users = page.users.clone();
    users.sort_by(|left, right| left.id.cmp(&right.id));
    files
        .into_iter()
        .map(|file| ScriptedResponse::File(Ok(file)))
        .chain(
            users
                .into_iter()
                .map(|user| ScriptedResponse::User(Ok(user))),
        )
        .collect()
}

#[tokio::test]
async fn initial_repair_paginates_and_matches_exact_request_transcript() {
    let mut first_history = history_page();
    first_history.poll_kind = HostedSlackPollKindV1::FullRepair;
    let mut terminal_history = history_terminal_page();
    terminal_history.poll_kind = HostedSlackPollKindV1::FullRepair;
    let mut first_replies = replies_page();
    first_replies.poll_kind = HostedSlackPollKindV1::FullRepair;
    let mut terminal_replies = replies_terminal_page();
    terminal_replies.poll_kind = HostedSlackPollKindV1::FullRepair;
    let catch_up = catch_up_history_page("2026-05-28T20:00:00Z");
    let mut catch_up_first = first_replies.clone();
    catch_up_first.phase = HostedSlackPollPhaseV1::CatchUpReplies;
    catch_up_first.poll_cut_at = Some("2026-06-02T00:00:00Z".to_string());
    catch_up_first.observed_at = "2026-06-02T00:00:06Z".to_string();
    let mut catch_up_terminal = terminal_replies.clone();
    catch_up_terminal.phase = HostedSlackPollPhaseV1::CatchUpReplies;
    catch_up_terminal.poll_cut_at = Some("2026-06-02T00:00:00Z".to_string());
    catch_up_terminal.observed_at = "2026-06-02T00:00:07Z".to_string();

    let mut script = vec![
        ScriptedResponse::Identity(Ok(observed())),
        ScriptedResponse::Authority(Ok(authority())),
        ScriptedResponse::History(Ok(as_history_response(&first_history))),
    ];
    script.extend(metadata_script(&first_history));
    script.extend([
        ScriptedResponse::History(Ok(as_history_response(&terminal_history))),
        ScriptedResponse::Replies(Ok(as_replies_response(&first_replies))),
        ScriptedResponse::Replies(Ok(as_replies_response(&terminal_replies))),
        ScriptedResponse::History(Ok(as_history_response(&catch_up))),
        ScriptedResponse::Replies(Ok(as_replies_response(&catch_up_first))),
        ScriptedResponse::Replies(Ok(as_replies_response(&catch_up_terminal))),
    ]);
    let provider = FakeProvider::new(script);
    let mut checkpoint = new_checkpoint("2026-05-28T20:00:00Z");
    let outcome = drive_hosted_slack_poll_v1(
        &provider,
        &binding(),
        &selector(),
        &mut checkpoint,
        &control(Some("2026-06-02T00:00:00Z")),
    )
    .await
    .unwrap();

    let HostedSlackDriveOutcomeV1::Complete(output) = outcome else {
        panic!("poll did not complete");
    };
    assert_eq!(output.snapshot.messages().len(), 3);
    assert_eq!(output.snapshot.threads()[0].reply_message_ids().len(), 2);
    provider.assert_exhausted();
    assert_eq!(exact_json(&provider.transcript()), INITIAL_TRANSCRIPT);
    let transcript = String::from_utf8(exact_json(&provider.transcript())).unwrap();
    for forbidden in ["token", "authorization", "cookie", "email", "url_private"] {
        assert!(!transcript.to_ascii_lowercase().contains(forbidden));
    }
}

#[tokio::test]
async fn rejected_pre_horizon_messages_never_trigger_metadata_fetch_or_checkpoint_retention() {
    let accepted = zero_root_response(None, "2026-06-01T00:00:01Z").messages[0].clone();
    let rejected = HostedSlackProviderMessageV1 {
        message: RawHostedSlackMessage {
            channel_id: selector().channel_id,
            ts: "1767225599.000000".to_string(),
            thread_ts: None,
            user_id: Some("U08CANARY001".to_string()),
            text: "rejected-message-canary".to_string(),
            edited_ts: None,
            deleted: false,
            file_ids: vec!["F08CANARY001".to_string()],
        },
        reply_count: Some(0),
    };
    let provider = FakeProvider::new([
        ScriptedResponse::Identity(Ok(observed())),
        ScriptedResponse::Authority(Ok(authority())),
        ScriptedResponse::History(Ok(HostedSlackProviderMessagePageV1 {
            observed_at: "2026-06-01T00:00:01Z".to_string(),
            has_more: Some(false),
            next_cursor: None,
            messages: vec![accepted, rejected],
        })),
    ]);
    let mut checkpoint = new_checkpoint("2026-05-28T20:00:00Z");
    let limited = control(None).with_budgets(1, 8).unwrap();

    assert_eq!(
        drive_hosted_slack_poll_v1(
            &provider,
            &binding(),
            &selector(),
            &mut checkpoint,
            &limited,
        )
        .await
        .unwrap(),
        HostedSlackDriveOutcomeV1::Pending {
            phase: HostedSlackPollPhaseV1::AwaitingCatchUpCut,
            reason: HostedSlackDrivePendingReasonV1::AwaitingCatchUpCut,
        }
    );
    provider.assert_exhausted();
    assert_eq!(
        exact_json(&provider.transcript()),
        HISTORICAL_PAGE_ONE_TRANSCRIPT
    );
    let encoded = serde_json::to_string(&checkpoint).unwrap();
    for canary in ["rejected-message-canary", "U08CANARY001", "F08CANARY001"] {
        assert!(!encoded.contains(canary));
    }
    decode_hosted_slack_poll_checkpoint_v1(encoded.as_bytes()).unwrap();
}

fn awaiting_checkpoint_with_overlap(overlap: &str) -> HostedSlackPollCheckpointV1 {
    let mut checkpoint = new_checkpoint(overlap);
    let mut history = history_page();
    history.poll_kind = HostedSlackPollKindV1::FullRepair;
    history.poll_overlap_watermark = overlap.to_string();
    let mut history_terminal = history_terminal_page();
    history_terminal.poll_kind = HostedSlackPollKindV1::FullRepair;
    history_terminal.poll_overlap_watermark = overlap.to_string();
    let mut replies = replies_page();
    replies.poll_kind = HostedSlackPollKindV1::FullRepair;
    replies.poll_overlap_watermark = overlap.to_string();
    let mut replies_terminal = replies_terminal_page();
    replies_terminal.poll_kind = HostedSlackPollKindV1::FullRepair;
    replies_terminal.poll_overlap_watermark = overlap.to_string();
    checkpoint.apply_history_page(&history).unwrap();
    checkpoint.apply_history_page(&history_terminal).unwrap();
    checkpoint.apply_replies_page(&replies).unwrap();
    checkpoint.apply_replies_page(&replies_terminal).unwrap();
    checkpoint
}

#[tokio::test]
async fn catch_up_sweeps_every_old_root_and_retains_late_non_broadcast_reply() {
    let overlap = "2026-05-29T00:00:00Z";
    let mut checkpoint = awaiting_checkpoint_with_overlap(overlap);
    let catch_up = catch_up_history_page(overlap);
    let mut late_page = decode_hosted_slack_replies_page_v1(LATE_REPLIES_PAGE).unwrap();
    late_page.sharing = SlackChannelSharingClassification::Private;
    let provider = FakeProvider::new([
        ScriptedResponse::Identity(Ok(observed())),
        ScriptedResponse::Authority(Ok(authority())),
        ScriptedResponse::History(Ok(as_history_response(&catch_up))),
        ScriptedResponse::Replies(Ok(as_replies_response(&late_page))),
    ]);

    let outcome = drive_hosted_slack_poll_v1(
        &provider,
        &binding(),
        &selector(),
        &mut checkpoint,
        &control(Some("2026-06-02T00:00:00Z")),
    )
    .await
    .unwrap();
    let HostedSlackDriveOutcomeV1::Complete(output) = outcome else {
        panic!("catch-up did not complete");
    };
    assert_eq!(output.snapshot.threads()[0].reply_message_ids().len(), 3);
    assert!(
        output
            .snapshot
            .messages()
            .iter()
            .any(|message| message.text() == "Late non-broadcast reply")
    );
    provider.assert_exhausted();
    assert_eq!(exact_json(&provider.transcript()), LATE_TRANSCRIPT);
}

fn zero_root_response(
    next_cursor: Option<&str>,
    observed_at: &str,
) -> HostedSlackProviderMessagePageV1 {
    HostedSlackProviderMessagePageV1 {
        observed_at: observed_at.to_string(),
        has_more: Some(next_cursor.is_some()),
        next_cursor: next_cursor.map(str::to_string),
        messages: vec![HostedSlackProviderMessageV1 {
            message: RawHostedSlackMessage {
                channel_id: selector().channel_id,
                ts: "1779000000.000100".to_string(),
                thread_ts: None,
                user_id: None,
                text: "root".to_string(),
                edited_ts: None,
                deleted: false,
                file_ids: Vec::new(),
            },
            reply_count: Some(0),
        }],
    }
}

#[tokio::test(start_paused = true)]
async fn rate_limit_retry_after_is_respected_without_losing_progress() {
    let provider = FakeProvider::new([
        ScriptedResponse::Identity(Ok(observed())),
        ScriptedResponse::Authority(Ok(authority())),
        ScriptedResponse::History(Err(HostedSlackProviderError::RateLimited {
            retry_after: Duration::from_secs(2),
        })),
        ScriptedResponse::History(Ok(zero_root_response(None, "2026-06-01T00:00:01Z"))),
    ]);
    let mut checkpoint = new_checkpoint("2026-05-28T20:00:00Z");
    let started = tokio::time::Instant::now();
    let outcome = drive_hosted_slack_poll_v1(
        &provider,
        &binding(),
        &selector(),
        &mut checkpoint,
        &control(None),
    )
    .await
    .unwrap();
    assert_eq!(
        outcome,
        HostedSlackDriveOutcomeV1::Pending {
            phase: HostedSlackPollPhaseV1::AwaitingCatchUpCut,
            reason: HostedSlackDrivePendingReasonV1::AwaitingCatchUpCut,
        }
    );
    assert!(tokio::time::Instant::now().duration_since(started) >= Duration::from_secs(2));
    assert_eq!(exact_json(&provider.transcript()), RATE_TRANSCRIPT);
}

#[tokio::test(start_paused = true)]
async fn long_retry_after_is_retained_while_current_drive_wait_is_capped() {
    let provider = FakeProvider::new([
        ScriptedResponse::Identity(Ok(observed())),
        ScriptedResponse::Authority(Ok(authority())),
        ScriptedResponse::History(Err(HostedSlackProviderError::RateLimited {
            retry_after: Duration::from_secs(601),
        })),
    ]);
    let mut checkpoint = new_checkpoint("2026-05-28T20:00:00Z");
    let before = checkpoint.clone();
    let installation_binding = binding();
    let channel_selector = selector();
    let drive_control = control(None);
    let result = {
        let drive = drive_hosted_slack_poll_v1(
            &provider,
            &installation_binding,
            &channel_selector,
            &mut checkpoint,
            &drive_control,
        );
        tokio::pin!(drive);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(299)).await;
        tokio::task::yield_now().await;
        assert!(
            tokio::time::timeout(Duration::ZERO, drive.as_mut())
                .await
                .is_err()
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        drive.await
    };
    assert_eq!(
        result,
        Err(HostedSlackProviderError::RateLimited {
            retry_after: Duration::from_secs(601),
        })
    );
    assert_eq!(checkpoint, before);
    provider.assert_exhausted();
    assert_eq!(provider.transcript().len(), 3);
}

#[tokio::test]
async fn identity_swap_and_revocation_fail_before_content_calls() {
    let mut swapped = observed();
    swapped.team_id = "T08OTHERTEAM".to_string();
    let provider = FakeProvider::new([ScriptedResponse::Identity(Ok(swapped))]);
    let mut checkpoint = new_checkpoint("2026-05-28T20:00:00Z");
    assert_eq!(
        drive_hosted_slack_poll_v1(
            &provider,
            &binding(),
            &selector(),
            &mut checkpoint,
            &control(None),
        )
        .await,
        Err(HostedSlackProviderError::IdentityMismatch("team_id"))
    );
    assert_eq!(exact_json(&provider.transcript()), IDENTITY_ONLY_TRANSCRIPT);

    let revoked = FakeProvider::new([ScriptedResponse::Identity(Err(
        HostedSlackProviderError::Revoked,
    ))]);
    assert_eq!(
        drive_hosted_slack_poll_v1(
            &revoked,
            &binding(),
            &selector(),
            &mut checkpoint,
            &control(None),
        )
        .await,
        Err(HostedSlackProviderError::Revoked)
    );
    assert_eq!(exact_json(&revoked.transcript()), IDENTITY_ONLY_TRANSCRIPT);
}

#[tokio::test]
async fn channel_authority_drift_and_access_revocation_fail_before_history() {
    let cases = [
        {
            let mut value = authority();
            value.team_id = "T08OTHERTEAM".to_string();
            (
                value,
                HostedSlackProviderError::IdentityMismatch("channel team_id"),
            )
        },
        {
            let mut value = authority();
            value.channel_id = "C08OTHERCHAN".to_string();
            (
                value,
                HostedSlackProviderError::IdentityMismatch("channel_id"),
            )
        },
        {
            let mut value = authority();
            value.is_private = false;
            (
                value,
                HostedSlackProviderError::IdentityMismatch("channel sharing"),
            )
        },
        {
            let mut value = authority();
            value.is_member = false;
            (value, HostedSlackProviderError::Revoked)
        },
        {
            let mut value = authority();
            value.shared_team_ids = vec!["T08EXTERNAL1".to_string()];
            (
                value,
                HostedSlackProviderError::InvalidResponse("channel sharing facts"),
            )
        },
        {
            let mut value = authority();
            value.is_shared = true;
            (
                value,
                HostedSlackProviderError::InvalidResponse("channel sharing facts"),
            )
        },
        {
            let mut value = authority();
            value.is_org_shared = true;
            (
                value,
                HostedSlackProviderError::InvalidResponse("channel sharing facts"),
            )
        },
        {
            let mut value = authority();
            value.is_shared = true;
            value.is_externally_shared = true;
            value.shared_team_ids = vec!["T08EXTERNAL1".to_string(), "T08LOCALITY1".to_string()];
            (
                value,
                HostedSlackProviderError::Unsupported("Slack Connect channel identity in V1"),
            )
        },
    ];
    for (channel_authority, expected) in cases {
        let provider = FakeProvider::new([
            ScriptedResponse::Identity(Ok(observed())),
            ScriptedResponse::Authority(Ok(channel_authority)),
        ]);
        let mut checkpoint = new_checkpoint("2026-05-28T20:00:00Z");
        assert_eq!(
            drive_hosted_slack_poll_v1(
                &provider,
                &binding(),
                &selector(),
                &mut checkpoint,
                &control(None),
            )
            .await,
            Err(expected)
        );
        provider.assert_exhausted();
        assert_eq!(
            exact_json(&provider.transcript()),
            CHANNEL_AUTHORITY_TRANSCRIPT
        );
    }

    let mut wrong_enterprise = observed();
    wrong_enterprise.enterprise_id = Some("E08OTHERGRID".to_string());
    let provider = FakeProvider::new([ScriptedResponse::Identity(Ok(wrong_enterprise))]);
    let mut checkpoint = new_checkpoint("2026-05-28T20:00:00Z");
    assert_eq!(
        drive_hosted_slack_poll_v1(
            &provider,
            &binding(),
            &selector(),
            &mut checkpoint,
            &control(None),
        )
        .await,
        Err(HostedSlackProviderError::IdentityMismatch("enterprise_id"))
    );
    assert_eq!(exact_json(&provider.transcript()), IDENTITY_ONLY_TRANSCRIPT);
}

#[tokio::test]
async fn exact_history_boundaries_include_start_exclude_cut_and_require_root_count() {
    let mut start = zero_root_response(None, "2026-06-01T00:00:01Z");
    start.messages[0].message.ts = "1767225600.000000".to_string();
    start.messages[0].reply_count = None;
    let provider = FakeProvider::new([
        ScriptedResponse::Identity(Ok(observed())),
        ScriptedResponse::Authority(Ok(authority())),
        ScriptedResponse::History(Ok(start)),
    ]);
    let mut checkpoint = new_checkpoint("2026-05-28T20:00:00Z");
    assert_eq!(
        drive_hosted_slack_poll_v1(
            &provider,
            &binding(),
            &selector(),
            &mut checkpoint,
            &control(None),
        )
        .await,
        Err(HostedSlackProviderError::InvalidResponse(
            "root reply_count"
        ))
    );

    let mut cut = zero_root_response(None, "2026-06-01T00:00:01Z");
    cut.messages[0].message.ts = "1780272000.000000".to_string();
    let provider = FakeProvider::new([
        ScriptedResponse::Identity(Ok(observed())),
        ScriptedResponse::Authority(Ok(authority())),
        ScriptedResponse::History(Ok(cut)),
    ]);
    let mut checkpoint = new_checkpoint("2026-05-28T20:00:00Z");
    assert_eq!(
        drive_hosted_slack_poll_v1(
            &provider,
            &binding(),
            &selector(),
            &mut checkpoint,
            &control(None),
        )
        .await,
        Err(HostedSlackProviderError::Poll(
            HostedSlackPollError::PageWindowMismatch
        ))
    );
}

#[tokio::test]
async fn exact_zero_reply_count_skips_replies_at_history_and_catch_up_boundaries() {
    let historical = FakeProvider::new([
        ScriptedResponse::Identity(Ok(observed())),
        ScriptedResponse::Authority(Ok(authority())),
        ScriptedResponse::History(Ok(zero_root_response(None, "2026-06-01T00:00:01Z"))),
    ]);
    let mut zero = new_checkpoint("2026-05-28T20:00:00Z");
    assert_eq!(
        drive_hosted_slack_poll_v1(
            &historical,
            &binding(),
            &selector(),
            &mut zero,
            &control(None),
        )
        .await
        .unwrap(),
        HostedSlackDriveOutcomeV1::Pending {
            phase: HostedSlackPollPhaseV1::AwaitingCatchUpCut,
            reason: HostedSlackDrivePendingReasonV1::AwaitingCatchUpCut,
        }
    );
    historical.assert_exhausted();
    assert_eq!(historical.transcript().len(), 3);

    let overlap = "2026-05-28T20:00:00Z";
    let mut checkpoint = awaiting_checkpoint_with_overlap(overlap);
    let mut zero_catch_up = as_history_response(&history_page());
    zero_catch_up.observed_at = "2026-06-02T00:00:01Z".to_string();
    zero_catch_up.has_more = Some(false);
    zero_catch_up.next_cursor = None;
    zero_catch_up.messages.truncate(1);
    zero_catch_up.messages[0].reply_count = Some(0);
    let catch_up = FakeProvider::new([
        ScriptedResponse::Identity(Ok(observed())),
        ScriptedResponse::Authority(Ok(authority())),
        ScriptedResponse::History(Ok(zero_catch_up)),
    ]);
    let outcome = drive_hosted_slack_poll_v1(
        &catch_up,
        &binding(),
        &selector(),
        &mut checkpoint,
        &control(Some("2026-06-02T00:00:00Z")),
    )
    .await
    .unwrap();
    let HostedSlackDriveOutcomeV1::Complete(output) = outcome else {
        panic!("zero-reply catch-up did not complete");
    };
    catch_up.assert_exhausted();
    assert_eq!(catch_up.transcript().len(), 3);
    assert!(output.snapshot.threads()[0].reply_message_ids().is_empty());
}

#[tokio::test]
async fn thread_not_found_reconciles_deleted_root_and_replays_without_stale_files() {
    let mut checkpoint = new_checkpoint("2026-05-28T20:00:00Z");
    let mut first = history_page();
    first.poll_kind = HostedSlackPollKindV1::FullRepair;
    let mut terminal = history_terminal_page();
    terminal.poll_kind = HostedSlackPollKindV1::FullRepair;
    checkpoint.apply_history_page(&first).unwrap();
    checkpoint.apply_history_page(&terminal).unwrap();
    let provider = FakeProvider::new([
        ScriptedResponse::Identity(Ok(observed())),
        ScriptedResponse::Authority(Ok(authority())),
        ScriptedResponse::Replies(Err(HostedSlackProviderError::ThreadNotFound)),
    ]);

    assert_eq!(
        drive_hosted_slack_poll_v1(
            &provider,
            &binding(),
            &selector(),
            &mut checkpoint,
            &control(None),
        )
        .await
        .unwrap(),
        HostedSlackDriveOutcomeV1::Pending {
            phase: HostedSlackPollPhaseV1::AwaitingCatchUpCut,
            reason: HostedSlackDrivePendingReasonV1::AwaitingCatchUpCut,
        }
    );
    provider.assert_exhausted();
    let root = checkpoint
        .candidate()
        .messages()
        .iter()
        .find(|message| message.ts == "1780000000.000100")
        .unwrap();
    assert!(root.deleted);
    assert!(root.text.is_empty());
    assert!(root.file_ids.is_empty());
    assert!(checkpoint.candidate().files().is_empty());
    assert_eq!(checkpoint.completed_roots()[0].expected_reply_count, 0);
    let encoded = serde_json::to_vec(&checkpoint).unwrap();
    let encoded_value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    assert!(
        encoded_value["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| {
                item["page"]["canonical_page_json"]
                    .as_str()
                    .is_some_and(|page| page.contains(r#""reconciliation":"thread_not_found""#))
            })
    );
    assert_eq!(
        decode_hosted_slack_poll_checkpoint_v1(&encoded).unwrap(),
        checkpoint
    );
}

#[tokio::test]
async fn contradictory_or_missing_reply_metadata_fails_closed() {
    let mut contradictory = zero_root_response(None, "2026-06-01T00:00:01Z");
    let root_id = contradictory.messages[0].message.ts.clone();
    let mut reply = contradictory.messages[0].clone();
    reply.message.ts = "1779000001.000100".to_string();
    reply.message.thread_ts = Some(root_id);
    reply.reply_count = Some(1);
    contradictory.messages.push(reply);
    let provider = FakeProvider::new([
        ScriptedResponse::Identity(Ok(observed())),
        ScriptedResponse::Authority(Ok(authority())),
        ScriptedResponse::History(Ok(contradictory)),
    ]);
    let mut checkpoint = new_checkpoint("2026-05-28T20:00:00Z");
    assert_eq!(
        drive_hosted_slack_poll_v1(
            &provider,
            &binding(),
            &selector(),
            &mut checkpoint,
            &control(None),
        )
        .await,
        Err(HostedSlackProviderError::InvalidResponse(
            "reply reply_count"
        ))
    );

    let mut checkpoint = new_checkpoint("2026-05-28T20:00:00Z");
    let mut history = history_page();
    history.poll_kind = HostedSlackPollKindV1::FullRepair;
    let mut terminal = history_terminal_page();
    terminal.poll_kind = HostedSlackPollKindV1::FullRepair;
    checkpoint.apply_history_page(&history).unwrap();
    checkpoint.apply_history_page(&terminal).unwrap();
    let root = history.messages[0].message.clone();
    let provider = FakeProvider::new([
        ScriptedResponse::Identity(Ok(observed())),
        ScriptedResponse::Authority(Ok(authority())),
        ScriptedResponse::Replies(Ok(HostedSlackProviderMessagePageV1 {
            observed_at: "2026-06-01T00:00:03Z".to_string(),
            has_more: Some(false),
            next_cursor: None,
            messages: vec![HostedSlackProviderMessageV1 {
                message: root,
                reply_count: None,
            }],
        })),
    ]);
    assert_eq!(
        drive_hosted_slack_poll_v1(
            &provider,
            &binding(),
            &selector(),
            &mut checkpoint,
            &control(None),
        )
        .await,
        Err(HostedSlackProviderError::InvalidResponse(
            "initial replies root reply_count"
        ))
    );
}

#[tokio::test]
async fn cursor_conflict_fails_closed_and_resume_never_restarts_page_one() {
    let provider = FakeProvider::new([
        ScriptedResponse::Identity(Ok(observed())),
        ScriptedResponse::Authority(Ok(authority())),
        ScriptedResponse::History(Ok(zero_root_response(
            Some("history-next"),
            "2026-06-01T00:00:01Z",
        ))),
        ScriptedResponse::History(Ok(zero_root_response(
            Some("history-next"),
            "2026-06-01T00:00:02Z",
        ))),
    ]);
    let mut checkpoint = new_checkpoint("2026-05-28T20:00:00Z");
    assert_eq!(
        drive_hosted_slack_poll_v1(
            &provider,
            &binding(),
            &selector(),
            &mut checkpoint,
            &control(None),
        )
        .await,
        Err(HostedSlackProviderError::InvalidResponse(
            "repeated response cursor"
        ))
    );
    assert_eq!(checkpoint.history_cursor(), Some("history-next"));
    assert_eq!(
        exact_json(&provider.transcript()),
        CURSOR_CONFLICT_TRANSCRIPT
    );

    let first = FakeProvider::new([
        ScriptedResponse::Identity(Ok(observed())),
        ScriptedResponse::Authority(Ok(authority())),
        ScriptedResponse::History(Ok(zero_root_response(
            Some("resume-next"),
            "2026-06-01T00:00:01Z",
        ))),
    ]);
    let mut resumed = new_checkpoint("2026-05-28T20:00:00Z");
    let limited = control(None).with_budgets(1, 8).unwrap();
    assert_eq!(
        drive_hosted_slack_poll_v1(&first, &binding(), &selector(), &mut resumed, &limited,)
            .await
            .unwrap(),
        HostedSlackDriveOutcomeV1::Pending {
            phase: HostedSlackPollPhaseV1::HistoricalHistory,
            reason: HostedSlackDrivePendingReasonV1::PageBudgetExhausted,
        }
    );
    let encoded = serde_json::to_vec(&resumed).unwrap();
    let mut resumed = decode_hosted_slack_poll_checkpoint_v1(&encoded).unwrap();
    let terminal = FakeProvider::new([
        ScriptedResponse::Identity(Ok(observed())),
        ScriptedResponse::Authority(Ok(authority())),
        ScriptedResponse::History(Ok(HostedSlackProviderMessagePageV1 {
            observed_at: "2026-06-01T00:00:02Z".to_string(),
            has_more: Some(false),
            next_cursor: None,
            messages: Vec::new(),
        })),
    ]);
    let outcome = drive_hosted_slack_poll_v1(
        &terminal,
        &binding(),
        &selector(),
        &mut resumed,
        &control(None),
    )
    .await
    .unwrap();
    assert!(matches!(
        outcome,
        HostedSlackDriveOutcomeV1::Pending {
            phase: HostedSlackPollPhaseV1::AwaitingCatchUpCut,
            ..
        }
    ));
    let transcript = terminal.transcript();
    assert_eq!(exact_json(&transcript), RESUMED_HISTORY_TRANSCRIPT);
    let HostedSlackProviderRequestV1::ConversationsHistory { request } = &transcript[2] else {
        panic!("missing resumed history request");
    };
    assert_eq!(request.cursor.as_deref(), Some("resume-next"));
}

#[tokio::test]
async fn missing_or_ambiguous_pagination_facts_fail_without_checkpoint_progress() {
    for response in [
        HostedSlackProviderMessagePageV1 {
            observed_at: "2026-06-01T00:00:01Z".to_string(),
            has_more: None,
            next_cursor: None,
            messages: Vec::new(),
        },
        HostedSlackProviderMessagePageV1 {
            observed_at: "2026-06-01T00:00:01Z".to_string(),
            has_more: Some(false),
            next_cursor: Some("ambiguous-cursor".to_string()),
            messages: Vec::new(),
        },
        HostedSlackProviderMessagePageV1 {
            observed_at: "2026-06-01T00:00:01Z".to_string(),
            has_more: Some(true),
            next_cursor: None,
            messages: Vec::new(),
        },
    ] {
        let provider = FakeProvider::new([
            ScriptedResponse::Identity(Ok(observed())),
            ScriptedResponse::Authority(Ok(authority())),
            ScriptedResponse::History(Ok(response)),
        ]);
        let mut checkpoint = new_checkpoint("2026-05-28T20:00:00Z");
        let before = checkpoint.clone();
        assert_eq!(
            drive_hosted_slack_poll_v1(
                &provider,
                &binding(),
                &selector(),
                &mut checkpoint,
                &control(None),
            )
            .await,
            Err(HostedSlackProviderError::InvalidResponse(
                "pagination facts"
            ))
        );
        assert_eq!(checkpoint, before);
        provider.assert_exhausted();
        assert_eq!(
            exact_json(&provider.transcript()),
            HISTORICAL_PAGE_ONE_TRANSCRIPT
        );
    }
}

#[tokio::test(start_paused = true)]
async fn cancellation_interrupts_retry_wait_and_preserves_checkpoint() {
    let provider = FakeProvider::new([
        ScriptedResponse::Identity(Ok(observed())),
        ScriptedResponse::Authority(Ok(authority())),
        ScriptedResponse::History(Err(HostedSlackProviderError::RateLimited {
            retry_after: Duration::from_secs(60),
        })),
    ]);
    let token = HostedSlackCancellationToken::new();
    let drive_control = HostedSlackDriveControlV1::new(
        Instant::now() + Duration::from_secs(60 * 60),
        token.clone(),
        None,
    );
    let mut checkpoint = new_checkpoint("2026-05-28T20:00:00Z");
    let before = checkpoint.clone();
    let installation_binding = binding();
    let channel_selector = selector();
    let drive = drive_hosted_slack_poll_v1(
        &provider,
        &installation_binding,
        &channel_selector,
        &mut checkpoint,
        &drive_control,
    );
    let cancel = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        token.cancel();
    };
    let (result, ()) = tokio::join!(drive, cancel);
    assert_eq!(result, Err(HostedSlackProviderError::Cancelled));
    assert_eq!(checkpoint, before);
    assert_eq!(
        exact_json(&provider.transcript()),
        HISTORICAL_PAGE_ONE_TRANSCRIPT
    );
}

#[tokio::test]
async fn cancellation_after_response_is_checked_before_checkpoint_mutation() {
    let token = HostedSlackCancellationToken::new();
    let provider = FakeProvider::new([
        ScriptedResponse::Identity(Ok(observed())),
        ScriptedResponse::Authority(Ok(authority())),
        ScriptedResponse::HistoryAndCancel(
            Ok(zero_root_response(None, "2026-06-01T00:00:01Z")),
            token.clone(),
        ),
    ]);
    let drive_control =
        HostedSlackDriveControlV1::new(Instant::now() + Duration::from_secs(60), token, None);
    let mut checkpoint = new_checkpoint("2026-05-28T20:00:00Z");
    let before = checkpoint.clone();

    assert_eq!(
        drive_hosted_slack_poll_v1(
            &provider,
            &binding(),
            &selector(),
            &mut checkpoint,
            &drive_control,
        )
        .await,
        Err(HostedSlackProviderError::Cancelled)
    );
    assert_eq!(checkpoint, before);
    provider.assert_exhausted();
    assert_eq!(
        exact_json(&provider.transcript()),
        HISTORICAL_PAGE_ONE_TRANSCRIPT
    );
}

#[tokio::test(start_paused = true)]
async fn deadline_interrupts_an_in_flight_call_and_preserves_checkpoint() {
    let provider = FakeProvider::new([
        ScriptedResponse::Identity(Ok(observed())),
        ScriptedResponse::Authority(Ok(authority())),
        ScriptedResponse::PendingHistory,
    ]);
    let drive_control = HostedSlackDriveControlV1::new(
        Instant::now() + Duration::from_secs(2),
        HostedSlackCancellationToken::new(),
        None,
    );
    let mut checkpoint = new_checkpoint("2026-05-28T20:00:00Z");
    let before = checkpoint.clone();

    assert_eq!(
        drive_hosted_slack_poll_v1(
            &provider,
            &binding(),
            &selector(),
            &mut checkpoint,
            &drive_control,
        )
        .await,
        Err(HostedSlackProviderError::DeadlineExceeded)
    );
    assert_eq!(checkpoint, before);
    provider.assert_exhausted();
    assert_eq!(
        exact_json(&provider.transcript()),
        HISTORICAL_PAGE_ONE_TRANSCRIPT
    );
}

#[tokio::test]
async fn provider_and_metadata_limits_fail_before_unbounded_calls() {
    let request_limited = FakeProvider::new([ScriptedResponse::Identity(Ok(observed()))]);
    let mut checkpoint = new_checkpoint("2026-05-28T20:00:00Z");
    let request_control = control(None).with_budgets(1, 1).unwrap();
    assert_eq!(
        drive_hosted_slack_poll_v1(
            &request_limited,
            &binding(),
            &selector(),
            &mut checkpoint,
            &request_control,
        )
        .await,
        Err(HostedSlackProviderError::LimitExceeded(
            "provider request budget"
        ))
    );
    assert_eq!(
        exact_json(&request_limited.transcript()),
        IDENTITY_ONLY_TRANSCRIPT
    );

    let template = zero_root_response(None, "2026-06-01T00:00:01Z").messages[0].clone();
    let mut messages = Vec::new();
    for message_index in 0..3 {
        let mut message = template.clone();
        message.message.ts = format!("177900000{message_index}.000100");
        message.message.file_ids = (0..100)
            .map(|file_index| format!("F{message_index:02}{file_index:08}"))
            .collect();
        messages.push(message);
    }
    let metadata_limited = FakeProvider::new([
        ScriptedResponse::Identity(Ok(observed())),
        ScriptedResponse::Authority(Ok(authority())),
        ScriptedResponse::History(Ok(HostedSlackProviderMessagePageV1 {
            observed_at: "2026-06-01T00:00:01Z".to_string(),
            has_more: Some(false),
            next_cursor: None,
            messages,
        })),
    ]);
    let mut checkpoint = new_checkpoint("2026-05-28T20:00:00Z");
    assert_eq!(
        drive_hosted_slack_poll_v1(
            &metadata_limited,
            &binding(),
            &selector(),
            &mut checkpoint,
            &control(None),
        )
        .await,
        Err(HostedSlackProviderError::LimitExceeded(
            "metadata reference closure"
        ))
    );
    assert_eq!(
        exact_json(&metadata_limited.transcript()),
        HISTORICAL_PAGE_ONE_TRANSCRIPT
    );
}

#[tokio::test]
async fn file_owner_expansion_still_obeys_the_combined_metadata_closure_limit() {
    let file_ids = (0..128)
        .map(|index| format!("FOWN{index:08}"))
        .collect::<Vec<_>>();
    let mut first = zero_root_response(None, "2026-06-01T00:00:01Z").messages[0].clone();
    first.message.user_id = Some("U08ADA00001".to_string());
    first.message.file_ids = file_ids[..100].to_vec();
    let mut second = first.clone();
    second.message.ts = "1779000001.000100".to_string();
    second.message.file_ids = file_ids[100..].to_vec();
    let response = HostedSlackProviderMessagePageV1 {
        observed_at: "2026-06-01T00:00:01Z".to_string(),
        has_more: Some(false),
        next_cursor: None,
        messages: vec![first, second],
    };
    let mut file_template = raw_snapshot().files.into_iter().next().unwrap();
    let mut script = vec![
        ScriptedResponse::Identity(Ok(observed())),
        ScriptedResponse::Authority(Ok(authority())),
        ScriptedResponse::History(Ok(response)),
    ];
    for (index, file_id) in file_ids.iter().enumerate() {
        file_template.id = file_id.clone();
        file_template.user_id = Some(format!("UOWN{index:08}"));
        script.push(ScriptedResponse::File(Ok(file_template.clone())));
    }
    let provider = FakeProvider::new(script);
    let mut checkpoint = new_checkpoint("2026-05-28T20:00:00Z");

    assert_eq!(
        drive_hosted_slack_poll_v1(
            &provider,
            &binding(),
            &selector(),
            &mut checkpoint,
            &control(None),
        )
        .await,
        Err(HostedSlackProviderError::LimitExceeded(
            "metadata reference closure"
        ))
    );
    provider.assert_exhausted();
    assert_eq!(provider.transcript().len(), 131);
    assert!(
        !provider
            .transcript()
            .iter()
            .any(|request| matches!(request, HostedSlackProviderRequestV1::UsersInfo { .. }))
    );
}
