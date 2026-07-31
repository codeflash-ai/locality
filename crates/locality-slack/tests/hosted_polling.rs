use locality_protocol::{HostedSlackChannelSelector, ProviderSourceScopeSelector};
use locality_slack::portable::hosted::{
    HOSTED_SLACK_POLL_PAGE_FORMAT_VERSION_V3, HOSTED_SLACK_POLL_PAGE_MINIMUM_READER_VERSION_V3,
    HostedSlackHistoryMessageV1, HostedSlackHistoryPageV1, HostedSlackHistoryPageV2,
    HostedSlackPageApplyOutcomeV1, HostedSlackPollCheckpointV1, HostedSlackPollError,
    HostedSlackPollEvidenceV1, HostedSlackPollKindV1, HostedSlackPollKindV2,
    HostedSlackPollOutputV1, HostedSlackPollPhaseV1, HostedSlackRepliesPageV1,
    MAX_HOSTED_SLACK_CHECKPOINT_BYTES_V1, MAX_HOSTED_SLACK_CURSOR_BYTES_V1,
    MAX_HOSTED_SLACK_POLL_PAGE_BYTES_V1, MAX_HOSTED_SLACK_POLL_PAGE_MESSAGES_V1,
    RawHostedSlackMessage, RawHostedSlackNativeSnapshot, decode_hosted_slack_history_page_v1,
    decode_hosted_slack_poll_checkpoint_v1, decode_hosted_slack_poll_checkpoint_v2,
    decode_hosted_slack_replies_page_v1,
};
use serde::Serialize;

const SELECTOR: &[u8] =
    include_bytes!("../../locality-protocol/fixtures/hosted-slack-channel-selector-v1.json");
const NATIVE_RAW: &[u8] = include_bytes!("../fixtures/hosted-v1/native-raw.json");
const CHECKPOINT: &[u8] = include_bytes!("../fixtures/hosted-v1/poll-v1/checkpoint-v1.json");
const HISTORY_PAGE: &[u8] = include_bytes!("../fixtures/hosted-v1/poll-v1/history-page-v1.json");
const REPLIES_PAGE: &[u8] = include_bytes!("../fixtures/hosted-v1/poll-v1/replies-page-v1.json");
const COMPLETE_OUTPUT: &[u8] =
    include_bytes!("../fixtures/hosted-v1/poll-v1/complete-output-v1.json");
const CHECKPOINT_REPLY_PAGINATION: &[u8] =
    include_bytes!("../fixtures/hosted-v1/poll-v1/checkpoint-reply-pagination-v1.json");
const CHECKPOINT_AWAITING_CATCH_UP: &[u8] =
    include_bytes!("../fixtures/hosted-v1/poll-v1/checkpoint-awaiting-catch-up-v1.json");
const CHECKPOINT_CATCH_UP_HISTORY: &[u8] =
    include_bytes!("../fixtures/hosted-v1/poll-v1/checkpoint-catch-up-history-v1.json");
const CHECKPOINT_CATCH_UP_REPLIES: &[u8] =
    include_bytes!("../fixtures/hosted-v1/poll-v1/checkpoint-catch-up-replies-v1.json");
const CATCH_UP_OLD_ROOT_REPLIES_PAGE: &[u8] =
    include_bytes!("../fixtures/hosted-v1/poll-v1/catch-up-old-root-replies-page-v1.json");

fn selector() -> HostedSlackChannelSelector {
    match serde_json::from_slice::<ProviderSourceScopeSelector>(SELECTOR).expect("selector") {
        ProviderSourceScopeSelector::HostedSlackChannel(selector) => selector,
        other => panic!("unexpected selector: {other:?}"),
    }
}

fn raw_snapshot() -> RawHostedSlackNativeSnapshot {
    serde_json::from_slice(NATIVE_RAW).expect("raw snapshot")
}

fn checkpoint_with_kind(kind: HostedSlackPollKindV1) -> HostedSlackPollCheckpointV1 {
    HostedSlackPollCheckpointV1::new(
        &selector(),
        raw_snapshot().channel,
        kind,
        "2026-06-01T00:00:00Z".to_string(),
        "2026-05-28T20:00:00Z".to_string(),
    )
    .expect("checkpoint")
}

fn checkpoint() -> HostedSlackPollCheckpointV1 {
    checkpoint_with_kind(HostedSlackPollKindV1::Bootstrap)
}

fn history_page() -> HostedSlackHistoryPageV1 {
    decode_hosted_slack_history_page_v1(HISTORY_PAGE).expect("history page")
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
    decode_hosted_slack_replies_page_v1(REPLIES_PAGE).expect("replies page")
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
            .expect("second reply"),
    ];
    page
}

fn catch_up_page() -> HostedSlackHistoryPageV1 {
    let mut page = history_page();
    page.phase = HostedSlackPollPhaseV1::CatchUpHistory;
    page.poll_cut_at = Some("2026-06-02T00:00:00Z".to_string());
    page.request_cursor = None;
    page.next_cursor = None;
    page.observed_at = "2026-06-02T00:00:05Z".to_string();
    page.messages.clear();
    page.users.clear();
    page.files.clear();
    page
}

fn catch_up_root_page() -> HostedSlackHistoryPageV1 {
    let mut page = catch_up_page();
    page.messages = vec![history_page().messages[0].clone()];
    page
}

fn catch_up_replies_page(mut page: HostedSlackRepliesPageV1) -> HostedSlackRepliesPageV1 {
    page.phase = HostedSlackPollPhaseV1::CatchUpReplies;
    page.poll_cut_at = Some("2026-06-02T00:00:00Z".to_string());
    page.observed_at = if page.next_cursor.is_some() {
        "2026-06-02T00:00:06Z"
    } else {
        "2026-06-02T00:00:07Z"
    }
    .to_string();
    page
}

fn old_root_late_replies_page() -> HostedSlackRepliesPageV1 {
    let mut page = catch_up_replies_page(replies_page());
    page.poll_overlap_watermark = "2026-05-29T00:00:00Z".to_string();
    page.root_reply_count = 3;
    page.next_cursor = None;
    page.observed_at = "2026-06-02T00:00:08Z".to_string();
    page.messages = raw_snapshot().messages;
    let mut late_reply = page.messages[2].clone();
    late_reply.ts = "1780272001.000500".to_string();
    late_reply.text = "Late non-broadcast reply".to_string();
    page.messages.push(late_reply);
    page.messages.sort_by(|left, right| left.ts.cmp(&right.ts));
    page.users.clear();
    page.files.clear();
    page
}

fn json_contains_string(value: &serde_json::Value, expected: &str) -> bool {
    match value {
        serde_json::Value::String(value) => value.contains(expected),
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| json_contains_string(value, expected)),
        serde_json::Value::Object(values) => values
            .values()
            .any(|value| json_contains_string(value, expected)),
        _ => false,
    }
}

fn exact_pretty_json(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("serialize JSON");
    bytes.push(b'\n');
    bytes
}

fn finish_historical(checkpoint: &mut HostedSlackPollCheckpointV1) {
    let poll_kind = checkpoint.poll_kind();
    let overlap = checkpoint.poll_overlap_watermark().to_string();
    let mut history_page = history_page();
    history_page.poll_kind = poll_kind;
    history_page.poll_overlap_watermark = overlap.clone();
    let mut history_terminal_page = history_terminal_page();
    history_terminal_page.poll_kind = poll_kind;
    history_terminal_page.poll_overlap_watermark = overlap.clone();
    let mut replies_page = replies_page();
    replies_page.poll_kind = poll_kind;
    replies_page.poll_overlap_watermark = overlap.clone();
    let mut replies_terminal_page = replies_terminal_page();
    replies_terminal_page.poll_kind = poll_kind;
    replies_terminal_page.poll_overlap_watermark = overlap;
    checkpoint
        .apply_history_page(&history_page)
        .expect("history page one");
    checkpoint
        .apply_history_page(&history_terminal_page)
        .expect("history terminal");
    checkpoint
        .apply_replies_page(&replies_page)
        .expect("replies page one");
    checkpoint
        .apply_replies_page(&replies_terminal_page)
        .expect("replies terminal");
    assert_eq!(
        checkpoint.phase(),
        HostedSlackPollPhaseV1::AwaitingCatchUpCut
    );
}

fn reply_pagination_boundary() -> HostedSlackPollCheckpointV1 {
    let mut checkpoint = checkpoint();
    checkpoint.apply_history_page(&history_page()).unwrap();
    checkpoint
        .apply_history_page(&history_terminal_page())
        .unwrap();
    checkpoint.apply_replies_page(&replies_page()).unwrap();
    checkpoint
}

fn awaiting_catch_up_boundary() -> HostedSlackPollCheckpointV1 {
    let mut checkpoint = checkpoint();
    finish_historical(&mut checkpoint);
    checkpoint
}

fn catch_up_history_boundary() -> HostedSlackPollCheckpointV1 {
    let mut checkpoint = awaiting_catch_up_boundary();
    checkpoint
        .begin_catch_up("2026-06-02T00:00:00Z".to_string())
        .unwrap();
    checkpoint
}

fn catch_up_replies_boundary() -> HostedSlackPollCheckpointV1 {
    let mut checkpoint = catch_up_history_boundary();
    checkpoint
        .apply_history_page(&catch_up_root_page())
        .unwrap();
    checkpoint
        .apply_replies_page(&catch_up_replies_page(replies_page()))
        .unwrap();
    checkpoint
}

fn completed_checkpoint(
    mut checkpoint: HostedSlackPollCheckpointV1,
) -> HostedSlackPollCheckpointV1 {
    finish_historical(&mut checkpoint);
    checkpoint
        .begin_catch_up("2026-06-02T00:00:00Z".to_string())
        .expect("fixed catch-up cut");
    let mut catch_up = catch_up_page();
    catch_up.poll_kind = checkpoint.poll_kind();
    checkpoint
        .apply_history_page(&catch_up)
        .expect("catch-up page");
    let mut replies = catch_up_replies_page(replies_page());
    replies.poll_kind = checkpoint.poll_kind();
    let mut terminal = catch_up_replies_page(replies_terminal_page());
    terminal.poll_kind = checkpoint.poll_kind();
    checkpoint
        .apply_replies_page(&replies)
        .expect("catch-up replies page");
    checkpoint
        .apply_replies_page(&terminal)
        .expect("catch-up replies terminal");
    checkpoint
}

fn complete_poll(checkpoint: HostedSlackPollCheckpointV1) -> HostedSlackPollOutputV1 {
    completed_checkpoint(checkpoint)
        .completed_output()
        .expect("complete output")
}

#[test]
fn checkpoint_pages_and_completed_output_are_exact_v1_bytes() {
    let checkpoint = checkpoint();
    assert_eq!(exact_pretty_json(&checkpoint), CHECKPOINT);
    assert_eq!(
        decode_hosted_slack_poll_checkpoint_v1(CHECKPOINT).unwrap(),
        checkpoint
    );
    assert_eq!(exact_pretty_json(&history_page()), HISTORY_PAGE);
    assert_eq!(exact_pretty_json(&replies_page()), REPLIES_PAGE);

    let output = complete_poll(checkpoint);
    assert_eq!(exact_pretty_json(&output), COMPLETE_OUTPUT);
    assert_eq!(output.snapshot.threads().len(), 1);
    assert_eq!(output.snapshot.threads()[0].reply_message_ids().len(), 2);
    let json = String::from_utf8(exact_pretty_json(&output)).expect("UTF-8");
    for forbidden in [
        "token",
        "authorization",
        "cookie",
        "url_private",
        "permalink",
    ] {
        assert!(!json.to_ascii_lowercase().contains(forbidden));
    }
}

#[test]
fn exact_page_replay_and_checkpoint_resume_are_deterministic() {
    let page = history_page();
    let mut uninterrupted = checkpoint();
    assert_eq!(
        uninterrupted.apply_history_page(&page),
        Ok(HostedSlackPageApplyOutcomeV1::Applied)
    );
    let after_first = uninterrupted.clone();
    assert_eq!(
        uninterrupted.apply_history_page(&page),
        Ok(HostedSlackPageApplyOutcomeV1::ExactReplay)
    );
    assert_eq!(uninterrupted, after_first);

    let encoded = serde_json::to_vec(&uninterrupted).expect("checkpoint JSON");
    let mut resumed = decode_hosted_slack_poll_checkpoint_v1(&encoded).expect("resume checkpoint");
    for state in [&mut uninterrupted, &mut resumed] {
        state.apply_history_page(&history_terminal_page()).unwrap();
        state.apply_replies_page(&replies_page()).unwrap();
        state.apply_replies_page(&replies_terminal_page()).unwrap();
        state
            .begin_catch_up("2026-06-02T00:00:00Z".to_string())
            .unwrap();
        state.apply_history_page(&catch_up_page()).unwrap();
        state
            .apply_replies_page(&catch_up_replies_page(replies_page()))
            .unwrap();
        state
            .apply_replies_page(&catch_up_replies_page(replies_terminal_page()))
            .unwrap();
    }
    assert_eq!(uninterrupted, resumed);
    assert_eq!(uninterrupted.completed_output(), resumed.completed_output());
}

#[test]
fn checkpoint_boundary_fixtures_decode_and_resume_deterministically() {
    for (expected, fixture) in [
        (reply_pagination_boundary(), CHECKPOINT_REPLY_PAGINATION),
        (awaiting_catch_up_boundary(), CHECKPOINT_AWAITING_CATCH_UP),
        (catch_up_history_boundary(), CHECKPOINT_CATCH_UP_HISTORY),
        (catch_up_replies_boundary(), CHECKPOINT_CATCH_UP_REPLIES),
    ] {
        assert_eq!(exact_pretty_json(&expected), fixture);
        assert_eq!(
            decode_hosted_slack_poll_checkpoint_v1(fixture),
            Ok(expected)
        );
        let json = String::from_utf8_lossy(fixture).to_ascii_lowercase();
        for forbidden in [
            "token",
            "authorization",
            "cookie",
            "url_private",
            "permalink",
        ] {
            assert!(!json.contains(forbidden));
        }
    }

    let mut reply = decode_hosted_slack_poll_checkpoint_v1(CHECKPOINT_REPLY_PAGINATION).unwrap();
    reply.apply_replies_page(&replies_terminal_page()).unwrap();
    assert_eq!(reply.phase(), HostedSlackPollPhaseV1::AwaitingCatchUpCut);

    let mut awaiting =
        decode_hosted_slack_poll_checkpoint_v1(CHECKPOINT_AWAITING_CATCH_UP).unwrap();
    awaiting
        .begin_catch_up("2026-06-02T00:00:00Z".to_string())
        .unwrap();
    assert_eq!(awaiting.phase(), HostedSlackPollPhaseV1::CatchUpHistory);

    let mut catch_up_history =
        decode_hosted_slack_poll_checkpoint_v1(CHECKPOINT_CATCH_UP_HISTORY).unwrap();
    catch_up_history
        .apply_history_page(&catch_up_page())
        .unwrap();
    assert_eq!(
        catch_up_history.phase(),
        HostedSlackPollPhaseV1::CatchUpReplies
    );
    catch_up_history
        .apply_replies_page(&catch_up_replies_page(replies_page()))
        .unwrap();
    catch_up_history
        .apply_replies_page(&catch_up_replies_page(replies_terminal_page()))
        .unwrap();
    assert!(catch_up_history.completed_output().is_ok());

    let mut catch_up_replies =
        decode_hosted_slack_poll_checkpoint_v1(CHECKPOINT_CATCH_UP_REPLIES).unwrap();
    catch_up_replies
        .apply_replies_page(&catch_up_replies_page(replies_terminal_page()))
        .unwrap();
    assert!(catch_up_replies.completed_output().is_ok());
}

#[test]
fn decoded_checkpoint_rejects_forged_derived_state_and_evidence() {
    fn assert_rejected(mut value: serde_json::Value, mutate: impl FnOnce(&mut serde_json::Value)) {
        mutate(&mut value);
        assert!(
            decode_hosted_slack_poll_checkpoint_v1(&serde_json::to_vec(&value).unwrap()).is_err()
        );
    }

    let awaiting =
        serde_json::from_slice::<serde_json::Value>(CHECKPOINT_AWAITING_CATCH_UP).unwrap();
    assert_rejected(awaiting.clone(), |value| {
        value["phase"] = serde_json::json!("historical_history");
    });
    assert_rejected(awaiting.clone(), |value| {
        value["candidate"]["messages"] = serde_json::json!([]);
    });
    assert_rejected(awaiting.clone(), |value| {
        let mut extra = value["candidate"]["messages"][1].clone();
        extra["ts"] = serde_json::json!("1780000003.000400");
        value["candidate"]["messages"]
            .as_array_mut()
            .unwrap()
            .push(extra);
    });
    assert_rejected(awaiting.clone(), |value| {
        value["completed_roots"] = serde_json::json!([]);
    });
    assert_rejected(awaiting.clone(), |value| {
        value["evidence"] = serde_json::json!([]);
    });
    assert_rejected(awaiting.clone(), |value| {
        let canonical = value["evidence"][0]["page"]["canonical_page_json"]
            .as_str()
            .unwrap();
        let mut page = serde_json::from_str::<serde_json::Value>(canonical).unwrap();
        let mut unreferenced_user = page["users"][0].clone();
        unreferenced_user["id"] = serde_json::json!("U08FORGED001");
        unreferenced_user["name"] = serde_json::json!("forged-unreferenced-user");
        page["users"]
            .as_array_mut()
            .unwrap()
            .push(unreferenced_user);
        value["evidence"][0]["page"]["canonical_page_json"] =
            serde_json::json!(serde_json::to_string(&page).unwrap());
    });

    let reply = serde_json::from_slice::<serde_json::Value>(CHECKPOINT_REPLY_PAGINATION).unwrap();
    assert_rejected(reply, |value| {
        value["reply_cursor"] = serde_json::json!("forged-cursor");
    });

    let catch_up =
        serde_json::from_slice::<serde_json::Value>(CHECKPOINT_CATCH_UP_HISTORY).unwrap();
    assert_rejected(catch_up.clone(), |value| {
        value["evidence"].as_array_mut().unwrap().pop();
    });
    assert_rejected(catch_up, |value| {
        let page_json = value["evidence"][0]["page"]["canonical_page_json"]
            .as_str()
            .unwrap()
            .to_string();
        value["evidence"][0]["page"]["canonical_page_json"] =
            serde_json::json!(format!("{page_json} "));
    });

    let mut complete = catch_up_history_boundary();
    complete.apply_history_page(&catch_up_page()).unwrap();
    let complete = serde_json::to_value(complete).unwrap();
    assert_rejected(complete, |value| {
        value["phase"] = serde_json::json!("awaiting_catch_up_cut");
    });
}

#[test]
fn conflicting_replay_after_checkpoint_resume_fails_closed() {
    let mut resumed = decode_hosted_slack_poll_checkpoint_v1(CHECKPOINT_REPLY_PAGINATION).unwrap();
    let mut conflicting = replies_page();
    conflicting.messages[0].text = "forged replay after resume".to_string();
    assert_eq!(
        resumed.apply_replies_page(&conflicting),
        Err(HostedSlackPollError::ConflictingReplay)
    );
}

#[test]
fn conflicting_replays_cursor_cycles_and_wrong_cursors_fail_closed() {
    let mut checkpoint = checkpoint();
    checkpoint.apply_history_page(&history_page()).unwrap();

    let mut conflicting = history_page();
    conflicting.messages[0].message.text = "conflicting replay".to_string();
    assert_eq!(
        checkpoint.apply_history_page(&conflicting),
        Err(HostedSlackPollError::ConflictingReplay)
    );

    let mut cycle = history_terminal_page();
    cycle.next_cursor = cycle.request_cursor.clone();
    assert_eq!(
        checkpoint.apply_history_page(&cycle),
        Err(HostedSlackPollError::CursorCycle)
    );

    let mut wrong = history_terminal_page();
    wrong.request_cursor = Some("wrong-cursor".to_string());
    assert_eq!(
        checkpoint.apply_history_page(&wrong),
        Err(HostedSlackPollError::UnexpectedCursor)
    );
}

#[test]
fn root_horizon_self_reference_zero_reply_and_broadcast_rules_are_deterministic() {
    let mut page = history_page();
    page.next_cursor = None;

    let mut old_root = page.messages[0].clone();
    old_root.message.ts = "1767225599.000000".to_string();
    old_root.message.thread_ts = Some(old_root.message.ts.clone());
    old_root.message.user_id = Some("U08CANARY001".to_string());
    old_root.message.file_ids = vec!["F08CANARY001".to_string()];
    old_root.message.text = "rejected-canary-root".to_string();
    old_root.reply_count = 1;
    let mut later_broadcast = page.messages[1].clone();
    later_broadcast.message.ts = "1775000000.000500".to_string();
    later_broadcast.message.thread_ts = Some(old_root.message.ts.clone());
    later_broadcast.message.user_id = Some("U08CANARY001".to_string());
    later_broadcast.message.file_ids = vec!["F08CANARY001".to_string()];
    later_broadcast.message.text = "rejected-canary-reply".to_string();
    page.messages.extend([old_root, later_broadcast]);
    let mut canary_user = page.users[0].clone();
    canary_user.id = "U08CANARY001".to_string();
    canary_user.name = "rejected-canary-user".to_string();
    let mut canary_file = page.files[0].clone();
    canary_file.id = "F08CANARY001".to_string();
    canary_file.user_id = Some(canary_user.id.clone());
    canary_file.name = "rejected-canary-file".to_string();
    page.users.push(canary_user);
    page.files.push(canary_file);

    let mut checkpoint = checkpoint();
    checkpoint.apply_history_page(&page).expect("history page");
    assert!(
        !checkpoint
            .candidate()
            .messages()
            .iter()
            .any(|message| message.ts == "1767225599.000000" || message.ts == "1775000000.000500")
    );
    assert_eq!(
        checkpoint.phase(),
        HostedSlackPollPhaseV1::HistoricalReplies
    );
    assert!(
        checkpoint
            .candidate()
            .users()
            .iter()
            .all(|user| user.id != "U08CANARY001")
    );
    assert!(
        checkpoint
            .candidate()
            .files()
            .iter()
            .all(|file| file.id != "F08CANARY001")
    );
    let encoded_checkpoint = serde_json::to_vec(&checkpoint).unwrap();
    let checkpoint_json = serde_json::from_slice::<serde_json::Value>(&encoded_checkpoint).unwrap();
    for rejected in [
        "1767225599.000000",
        "1775000000.000500",
        "U08CANARY001",
        "F08CANARY001",
        "rejected-canary-root",
        "rejected-canary-reply",
        "rejected-canary-user",
        "rejected-canary-file",
    ] {
        assert!(!json_contains_string(&checkpoint_json, rejected));
    }
    let before_replay = checkpoint.clone();
    let mut rejected_only_replay = page.clone();
    rejected_only_replay
        .messages
        .iter_mut()
        .find(|wrapped| wrapped.message.ts == "1767225599.000000")
        .unwrap()
        .message
        .text = "different rejected payload".to_string();
    assert_eq!(
        checkpoint.apply_history_page(&rejected_only_replay),
        Ok(HostedSlackPageApplyOutcomeV1::ExactReplay)
    );
    assert_eq!(checkpoint, before_replay);
    checkpoint = decode_hosted_slack_poll_checkpoint_v1(&encoded_checkpoint).unwrap();
    checkpoint.apply_replies_page(&replies_page()).unwrap();
    checkpoint
        .apply_replies_page(&replies_terminal_page())
        .unwrap();
    checkpoint
        .begin_catch_up("2026-06-02T00:00:00Z".to_string())
        .unwrap();
    checkpoint.apply_history_page(&catch_up_page()).unwrap();
    checkpoint
        .apply_replies_page(&catch_up_replies_page(replies_page()))
        .unwrap();
    checkpoint
        .apply_replies_page(&catch_up_replies_page(replies_terminal_page()))
        .unwrap();
    let snapshot_json = serde_json::to_string(&checkpoint.completed_output().unwrap()).unwrap();
    assert!(!snapshot_json.contains("CANARY"));
    assert!(!snapshot_json.contains("rejected-canary"));

    let mut at_cut = history_page();
    at_cut.next_cursor = None;
    at_cut.messages.truncate(1);
    at_cut.messages[0].message.ts = "1780272000.000000".to_string();
    at_cut.messages[0].message.thread_ts = None;
    at_cut.messages[0].message.user_id = None;
    at_cut.messages[0].message.file_ids.clear();
    at_cut.messages[0].reply_count = 0;
    at_cut.users.clear();
    at_cut.files.clear();
    assert_eq!(
        checkpoint_with_kind(HostedSlackPollKindV1::Bootstrap).apply_history_page(&at_cut),
        Err(HostedSlackPollError::PageWindowMismatch),
        "the immutable backfill cut is exclusive",
    );

    let mut zero_page = history_page();
    zero_page.next_cursor = None;
    zero_page.messages.truncate(1);
    zero_page.messages[0].message.thread_ts = Some(zero_page.messages[0].message.ts.clone());
    zero_page.messages[0].message.user_id = None;
    zero_page.messages[0].message.file_ids.clear();
    zero_page.messages[0].reply_count = 0;
    zero_page.users.clear();
    zero_page.files.clear();
    zero_page.poll_kind = HostedSlackPollKindV1::FullRepair;
    let mut zero = checkpoint_with_kind(HostedSlackPollKindV1::FullRepair);
    zero.apply_history_page(&zero_page)
        .expect("zero-reply root");
    assert_eq!(zero.phase(), HostedSlackPollPhaseV1::AwaitingCatchUpCut);
    assert_eq!(zero.completed_roots().len(), 1);
    zero.begin_catch_up("2026-06-02T00:00:00Z".to_string())
        .unwrap();
    let mut zero_catch_up = catch_up_page();
    zero_catch_up.poll_kind = HostedSlackPollKindV1::FullRepair;
    zero.apply_history_page(&zero_catch_up).unwrap();
    let mut zero_replies = catch_up_replies_page(replies_page());
    zero_replies.poll_kind = HostedSlackPollKindV1::FullRepair;
    zero_replies.next_cursor = None;
    zero_replies.root_reply_count = 0;
    zero_replies.messages = vec![zero_page.messages[0].message.clone()];
    zero_replies.users.clear();
    zero_replies.files.clear();
    zero.apply_replies_page(&zero_replies).unwrap();
    let output = zero.completed_output().expect("zero-reply output");
    assert_eq!(output.snapshot.threads().len(), 1);
    assert!(output.snapshot.threads()[0].reply_message_ids().is_empty());
}

#[test]
fn reply_count_mismatch_and_missing_pages_prevent_completeness_and_allow_repair() {
    let mut checkpoint = checkpoint();
    checkpoint.apply_history_page(&history_page()).unwrap();
    checkpoint
        .apply_history_page(&history_terminal_page())
        .unwrap();
    let before = checkpoint.clone();

    let mut incomplete = replies_page();
    incomplete.next_cursor = None;
    assert_eq!(
        checkpoint.apply_replies_page(&incomplete),
        Err(HostedSlackPollError::ReplyCountMismatch {
            root_message_id: "1780000000.000100".to_string(),
            expected: 2,
            actual: 1,
        })
    );
    assert_eq!(checkpoint, before);

    let mut wrong_declared_count = replies_page();
    wrong_declared_count.root_reply_count = 3;
    assert_eq!(
        checkpoint.apply_replies_page(&wrong_declared_count),
        Err(HostedSlackPollError::ReplyCountMismatch {
            root_message_id: "1780000000.000100".to_string(),
            expected: 2,
            actual: 3,
        })
    );
    assert_eq!(checkpoint, before);

    let mut over = replies_page();
    over.next_cursor = None;
    let second_reply = raw_snapshot()
        .messages
        .into_iter()
        .find(|message| message.ts == "1780000002.000300")
        .unwrap();
    let mut extra = second_reply.clone();
    extra.ts = "1780000003.000400".to_string();
    extra.text = "unexpected extra reply".to_string();
    over.messages.extend([second_reply, extra]);
    assert_eq!(
        checkpoint.apply_replies_page(&over),
        Err(HostedSlackPollError::ReplyCountMismatch {
            root_message_id: "1780000000.000100".to_string(),
            expected: 2,
            actual: 3,
        })
    );
    assert_eq!(checkpoint, before);

    checkpoint.apply_replies_page(&replies_page()).unwrap();
    assert!(matches!(
        checkpoint.completed_output(),
        Err(HostedSlackPollError::IncompleteCandidate("poll phase"))
    ));
    checkpoint
        .apply_replies_page(&replies_terminal_page())
        .expect("repaired complete replies");
}

#[test]
fn replies_sweeps_require_the_requested_root_and_never_erase_on_empty_first_page() {
    let mut checkpoint = checkpoint();
    finish_historical(&mut checkpoint);
    checkpoint
        .begin_catch_up("2026-06-02T00:00:00Z".to_string())
        .unwrap();
    checkpoint.apply_history_page(&catch_up_page()).unwrap();
    let before = checkpoint.clone();
    let before_json = serde_json::to_value(&before).unwrap();
    assert_eq!(
        before_json["candidate"]["messages"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    let mut empty = catch_up_replies_page(replies_page());
    empty.next_cursor = None;
    empty.messages.clear();
    assert_eq!(
        checkpoint.apply_replies_page(&empty),
        Err(HostedSlackPollError::IncompleteCandidate(
            "replies page messages"
        ))
    );
    assert_eq!(checkpoint, before);

    let encoded = serde_json::to_vec(&checkpoint).unwrap();
    let mut resumed = decode_hosted_slack_poll_checkpoint_v1(&encoded).unwrap();
    assert_eq!(
        resumed.apply_replies_page(&empty),
        Err(HostedSlackPollError::IncompleteCandidate(
            "replies page messages"
        ))
    );
    assert_eq!(resumed, before);

    let mut missing = catch_up_replies_page(replies_page());
    missing.messages.remove(0);
    assert_eq!(
        checkpoint.apply_replies_page(&missing),
        Err(HostedSlackPollError::MissingRoot(
            "1780000000.000100".to_string()
        ))
    );

    let mut wrong_association = catch_up_replies_page(replies_page());
    wrong_association.messages[0].thread_ts = Some("1780000001.000200".to_string());
    assert_eq!(
        checkpoint.apply_replies_page(&wrong_association),
        Err(HostedSlackPollError::InvalidMessageRelationship(
            "1780000000.000100".to_string()
        ))
    );

    let mut root_on_continuation = catch_up_replies_page(replies_terminal_page());
    root_on_continuation
        .messages
        .push(replies_page().messages[0].clone());
    assert_eq!(
        root_on_continuation.validate(),
        Err(HostedSlackPollError::InvalidMessageRelationship(
            "1780000000.000100".to_string()
        ))
    );

    checkpoint
        .apply_replies_page(&catch_up_replies_page(replies_page()))
        .unwrap();
    let before_empty_terminal = checkpoint.clone();
    let mut empty_terminal = catch_up_replies_page(replies_terminal_page());
    empty_terminal.messages.clear();
    assert_eq!(
        checkpoint.apply_replies_page(&empty_terminal),
        Err(HostedSlackPollError::IncompleteCandidate(
            "replies page messages"
        ))
    );
    assert_eq!(checkpoint, before_empty_terminal);
    let mut resumed_terminal = decode_hosted_slack_poll_checkpoint_v1(
        &serde_json::to_vec(&before_empty_terminal).unwrap(),
    )
    .unwrap();
    assert_eq!(
        resumed_terminal.apply_replies_page(&empty_terminal),
        Err(HostedSlackPollError::IncompleteCandidate(
            "replies page messages"
        ))
    );
    assert_eq!(resumed_terminal, before_empty_terminal);
}

#[test]
fn duplicate_message_timestamps_cannot_change_payload_or_thread_association() {
    let mut payload_checkpoint = checkpoint();
    payload_checkpoint
        .apply_history_page(&history_page())
        .unwrap();
    payload_checkpoint
        .apply_history_page(&history_terminal_page())
        .unwrap();
    payload_checkpoint
        .apply_replies_page(&replies_page())
        .unwrap();
    let payload_before = payload_checkpoint.clone();
    let mut conflicting_payload = replies_terminal_page();
    conflicting_payload.messages = vec![replies_page().messages[1].clone()];
    conflicting_payload.messages[0].text = "conflicting duplicate payload".to_string();
    assert_eq!(
        payload_checkpoint.apply_replies_page(&conflicting_payload),
        Err(HostedSlackPollError::ConflictingMessage(
            "1780000001.000200".to_string()
        ))
    );
    assert_eq!(payload_checkpoint, payload_before);

    let mut catch_up_checkpoint = checkpoint();
    finish_historical(&mut catch_up_checkpoint);
    catch_up_checkpoint
        .begin_catch_up("2026-06-02T00:00:00Z".to_string())
        .unwrap();
    let mut catch_up_history = catch_up_page();
    catch_up_history.messages = history_page().messages;
    catch_up_checkpoint
        .apply_history_page(&catch_up_history)
        .unwrap();
    catch_up_checkpoint
        .apply_replies_page(&catch_up_replies_page(replies_page()))
        .unwrap();
    let catch_up_before_conflict = catch_up_checkpoint.clone();
    let mut conflicting_broadcast = catch_up_replies_page(replies_terminal_page());
    conflicting_broadcast.messages[0].text = "conflicting broadcast payload".to_string();
    assert_eq!(
        catch_up_checkpoint.apply_replies_page(&conflicting_broadcast),
        Err(HostedSlackPollError::ConflictingMessage(
            "1780000002.000300".to_string()
        ))
    );
    assert_eq!(catch_up_checkpoint, catch_up_before_conflict);
    catch_up_checkpoint
        .apply_replies_page(&catch_up_replies_page(replies_terminal_page()))
        .unwrap();

    let mut two_roots = history_page();
    two_roots.next_cursor = None;
    two_roots.users.clear();
    two_roots.files.clear();
    let mut first_root = two_roots.messages[0].clone();
    first_root.message.user_id = None;
    first_root.message.file_ids.clear();
    first_root.message.edited_ts = None;
    first_root.reply_count = 1;
    let mut second_root = first_root.clone();
    second_root.message.ts = "1780000000.000200".to_string();
    second_root.message.text = "Second root".to_string();
    two_roots.messages = vec![first_root.clone(), second_root.clone()];

    let mut association_checkpoint = checkpoint();
    association_checkpoint
        .apply_history_page(&two_roots)
        .unwrap();
    let mut first_replies = replies_page();
    first_replies.next_cursor = None;
    first_replies.root_reply_count = 1;
    first_replies.messages[0] = first_root.message;
    first_replies.messages[0].file_ids.clear();
    first_replies.messages[0].user_id = None;
    first_replies.messages[0].edited_ts = None;
    first_replies.messages[1].user_id = None;
    first_replies.messages[1].ts = "1780000001.000200".to_string();
    association_checkpoint
        .apply_replies_page(&first_replies)
        .unwrap();

    let encoded = serde_json::to_vec(&association_checkpoint).unwrap();
    let mut resumed = decode_hosted_slack_poll_checkpoint_v1(&encoded).unwrap();
    let mut reassigned = first_replies;
    reassigned.root_message_id = second_root.message.ts.clone();
    reassigned.messages[0] = second_root.message;
    reassigned.messages[1].thread_ts = Some(reassigned.root_message_id.clone());
    reassigned.messages[1].text = "reassigned reply".to_string();
    for state in [&mut association_checkpoint, &mut resumed] {
        assert_eq!(
            state.apply_replies_page(&reassigned),
            Err(HostedSlackPollError::InvalidMessageRelationship(
                "1780000001.000200".to_string()
            ))
        );
    }
    assert_eq!(association_checkpoint, resumed);
}

#[test]
fn catch_up_replaces_current_message_state_and_revalidates_all_replies() {
    let mut checkpoint = checkpoint();
    finish_historical(&mut checkpoint);
    checkpoint
        .begin_catch_up("2026-06-02T00:00:00Z".to_string())
        .unwrap();

    let mut catch_up = catch_up_page();
    let mut updated_root = history_page().messages[0].clone();
    updated_root.message.text = "Current edited root".to_string();
    updated_root.message.edited_ts = Some("1780000300.000400".to_string());
    updated_root.message.deleted = true;
    catch_up.messages.push(updated_root.clone());
    catch_up.observed_at = "2026-06-02T00:00:01Z".to_string();
    checkpoint.apply_history_page(&catch_up).unwrap();
    assert_eq!(checkpoint.phase(), HostedSlackPollPhaseV1::CatchUpReplies);

    let mut replies_one = replies_page();
    replies_one.phase = HostedSlackPollPhaseV1::CatchUpReplies;
    replies_one.poll_cut_at = Some("2026-06-02T00:00:00Z".to_string());
    replies_one.observed_at = "2026-06-02T00:00:02Z".to_string();
    replies_one.messages[0] = updated_root.message;
    checkpoint.apply_replies_page(&replies_one).unwrap();

    let mut replies_two = replies_terminal_page();
    replies_two.phase = HostedSlackPollPhaseV1::CatchUpReplies;
    replies_two.poll_cut_at = Some("2026-06-02T00:00:00Z".to_string());
    replies_two.observed_at = "2026-06-02T00:00:03Z".to_string();
    checkpoint.apply_replies_page(&replies_two).unwrap();
    let output = checkpoint.completed_output().expect("updated output");
    assert_eq!(output.snapshot.messages()[0].text(), "Current edited root");
    assert!(output.snapshot.messages()[0].deleted());
}

#[test]
fn catch_up_sweeps_old_roots_and_captures_late_non_broadcast_replies() {
    let page = old_root_late_replies_page();
    assert_eq!(exact_pretty_json(&page), CATCH_UP_OLD_ROOT_REPLIES_PAGE);
    assert_eq!(
        decode_hosted_slack_replies_page_v1(CATCH_UP_OLD_ROOT_REPLIES_PAGE),
        Ok(page.clone())
    );

    let mut uninterrupted = HostedSlackPollCheckpointV1::new(
        &selector(),
        raw_snapshot().channel,
        HostedSlackPollKindV1::Bootstrap,
        "2026-06-01T00:00:00Z".to_string(),
        "2026-05-29T00:00:00Z".to_string(),
    )
    .unwrap();
    finish_historical(&mut uninterrupted);
    uninterrupted
        .begin_catch_up("2026-06-02T00:00:00Z".to_string())
        .unwrap();
    let mut catch_up = catch_up_page();
    catch_up.poll_overlap_watermark = uninterrupted.poll_overlap_watermark().to_string();
    uninterrupted.apply_history_page(&catch_up).unwrap();
    assert_eq!(
        uninterrupted.phase(),
        HostedSlackPollPhaseV1::CatchUpReplies
    );
    assert_eq!(
        uninterrupted.current_root_message_id(),
        Some("1780000000.000100"),
        "the root predates the overlap but must still be swept",
    );

    let encoded = serde_json::to_vec(&uninterrupted).unwrap();
    let mut resumed = decode_hosted_slack_poll_checkpoint_v1(&encoded).unwrap();
    uninterrupted.apply_replies_page(&page).unwrap();
    resumed.apply_replies_page(&page).unwrap();
    assert_eq!(uninterrupted, resumed);
    let output = resumed.completed_output().unwrap();
    assert_eq!(output.snapshot.threads()[0].reply_message_ids().len(), 3);
    assert!(
        output
            .snapshot
            .messages()
            .iter()
            .any(|message| message.text() == "Late non-broadcast reply")
    );
}

#[test]
fn catch_up_root_sweep_fails_before_exceeding_checkpoint_page_bound() {
    let template = history_page().messages[0].clone();
    let mut history = history_page();
    history.next_cursor = None;
    history.users.clear();
    history.files.clear();
    history.messages = (0..255)
        .map(|offset| {
            let mut root = template.clone();
            root.message.ts = format!("{}.000100", 1_779_000_000 + offset);
            root.message.thread_ts = None;
            root.message.user_id = None;
            root.message.file_ids.clear();
            root.reply_count = 0;
            root
        })
        .collect();

    let mut checkpoint = checkpoint();
    checkpoint.apply_history_page(&history).unwrap();
    checkpoint
        .begin_catch_up("2026-06-02T00:00:00Z".to_string())
        .unwrap();
    let before = checkpoint.clone();
    assert_eq!(
        checkpoint.apply_history_page(&catch_up_page()),
        Err(HostedSlackPollError::CollectionTooLarge(
            "catch-up root sweep"
        ))
    );
    assert_eq!(checkpoint, before);
}

#[test]
fn page_and_checkpoint_decoders_enforce_bounds_unknown_fields_and_scope() {
    let oversized = vec![b' '; MAX_HOSTED_SLACK_POLL_PAGE_BYTES_V1 + 1];
    assert!(matches!(
        decode_hosted_slack_history_page_v1(&oversized),
        Err(HostedSlackPollError::InputTooLarge {
            input: "history page",
            ..
        })
    ));
    let oversized_checkpoint = vec![b' '; MAX_HOSTED_SLACK_CHECKPOINT_BYTES_V1 + 1];
    assert!(matches!(
        decode_hosted_slack_poll_checkpoint_v1(&oversized_checkpoint),
        Err(HostedSlackPollError::InputTooLarge {
            input: "checkpoint",
            ..
        })
    ));

    let mut unknown = serde_json::from_slice::<serde_json::Value>(HISTORY_PAGE).unwrap();
    unknown["provider_cursor"] = serde_json::json!("unsafe");
    assert_eq!(
        decode_hosted_slack_history_page_v1(&serde_json::to_vec(&unknown).unwrap()),
        Err(HostedSlackPollError::InvalidJson("history page"))
    );

    let mut unversioned_reconciliation =
        serde_json::from_slice::<serde_json::Value>(REPLIES_PAGE).unwrap();
    unversioned_reconciliation["reconciliation"] = serde_json::json!("thread_not_found");
    assert_eq!(
        decode_hosted_slack_replies_page_v1(
            &serde_json::to_vec(&unversioned_reconciliation).unwrap()
        ),
        Err(HostedSlackPollError::InvalidJson("replies page"))
    );

    let mut too_many = history_page();
    too_many.messages =
        vec![too_many.messages[0].clone(); MAX_HOSTED_SLACK_POLL_PAGE_MESSAGES_V1 + 1];
    assert_eq!(
        too_many.validate(),
        Err(HostedSlackPollError::CollectionTooLarge("page.messages"))
    );

    let mut bad_cursor = history_page();
    bad_cursor.next_cursor = Some("x".repeat(MAX_HOSTED_SLACK_CURSOR_BYTES_V1 + 1));
    assert_eq!(
        bad_cursor.validate(),
        Err(HostedSlackPollError::InvalidCursor("page.next_cursor"))
    );

    let mut wrong_scope = history_page();
    wrong_scope.channel_id = "C08OTHER001".to_string();
    for message in &mut wrong_scope.messages {
        message.message.channel_id = wrong_scope.channel_id.clone();
    }
    for file in &mut wrong_scope.files {
        file.channel_id = wrong_scope.channel_id.clone();
    }
    assert_eq!(
        checkpoint().apply_history_page(&wrong_scope),
        Err(HostedSlackPollError::PageScopeMismatch("channel_id"))
    );

    let mut wrong_horizon = history_page();
    wrong_horizon.authorized_history_start_at = "2026-02-01T00:00:00Z".to_string();
    assert_eq!(
        checkpoint().apply_history_page(&wrong_horizon),
        Err(HostedSlackPollError::PageScopeMismatch(
            "authorized_history_start_at"
        ))
    );

    let mut checkpoint_unknown = serde_json::from_slice::<serde_json::Value>(CHECKPOINT).unwrap();
    checkpoint_unknown["token"] = serde_json::json!("forbidden");
    assert_eq!(
        decode_hosted_slack_poll_checkpoint_v1(&serde_json::to_vec(&checkpoint_unknown).unwrap()),
        Err(HostedSlackPollError::InvalidJson("checkpoint"))
    );

    assert_eq!(
        HostedSlackPollCheckpointV1::new(
            &selector(),
            raw_snapshot().channel,
            HostedSlackPollKindV1::Bootstrap,
            "2026-06-01T00:00:00Z".to_string(),
            "2026-06-01T00:00:00Z".to_string(),
        ),
        Err(HostedSlackPollError::InvalidCutOrder),
        "the overlap watermark must be strictly before the backfill cut",
    );
    assert!(
        HostedSlackPollCheckpointV1::new(
            &selector(),
            raw_snapshot().channel.clone(),
            HostedSlackPollKindV1::Bootstrap,
            "2026-06-01T00:00:00Z".to_string(),
            "2026-01-01T00:00:00Z".to_string(),
        )
        .is_ok(),
        "the overlap watermark may equal the authorized history start",
    );
    assert_eq!(
        HostedSlackPollCheckpointV1::new(
            &selector(),
            raw_snapshot().channel,
            HostedSlackPollKindV1::Bootstrap,
            "2026-06-01T00:00:00Z".to_string(),
            "2025-12-31T23:59:59Z".to_string(),
        ),
        Err(HostedSlackPollError::InvalidCutOrder),
        "the overlap watermark may not precede the authorized history start",
    );
}

#[test]
fn full_repair_and_poll_only_bootstrap_converge_without_events() {
    assert_eq!(
        complete_poll(checkpoint_with_kind(HostedSlackPollKindV1::FullRepair)),
        complete_poll(checkpoint_with_kind(HostedSlackPollKindV1::Bootstrap))
    );
}

#[test]
fn incremental_poll_starts_from_applied_candidate_and_skips_historical_and_untouched_replies() {
    let applied = completed_checkpoint(checkpoint());
    let mut wrong_non_incremental_version = serde_json::to_value(&applied).unwrap();
    wrong_non_incremental_version["checkpoint_format_version"] = 3.into();
    wrong_non_incremental_version["minimum_reader_version"] = 3.into();
    assert!(
        decode_hosted_slack_poll_checkpoint_v2(
            &serde_json::to_vec(&wrong_non_incremental_version).unwrap()
        )
        .is_err()
    );
    let applied_snapshot = applied.completed_output().unwrap().snapshot;
    let mut incremental = HostedSlackPollCheckpointV1::incremental_from_applied(
        &applied,
        raw_snapshot().channel,
        "2026-06-01T23:55:00Z".to_string(),
    )
    .expect("incremental checkpoint");
    assert_eq!(
        incremental.poll_kind_v2(),
        HostedSlackPollKindV2::Incremental
    );
    assert_eq!(
        incremental.phase(),
        HostedSlackPollPhaseV1::AwaitingCatchUpCut
    );
    assert_eq!(incremental.backfill_cut_at(), "2026-06-02T00:00:00Z");
    let encoded = serde_json::to_vec(&incremental).unwrap();
    let encoded_value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(encoded_value["evidence"][0]["kind"], "incremental_baseline");
    assert!(encoded_value["evidence"][0].get("candidate").is_none());
    assert!(
        serde_json::to_vec(&encoded_value["evidence"][0])
            .unwrap()
            .len()
            < 256,
        "the applied candidate must not be duplicated into replay evidence"
    );
    assert_eq!(
        decode_hosted_slack_poll_checkpoint_v2(&encoded).unwrap(),
        incremental
    );
    assert!(decode_hosted_slack_poll_checkpoint_v1(&encoded).is_err());
    let mut wrong_incremental_version = encoded_value.clone();
    wrong_incremental_version["checkpoint_format_version"] = 2.into();
    wrong_incremental_version["minimum_reader_version"] = 2.into();
    assert!(
        decode_hosted_slack_poll_checkpoint_v2(
            &serde_json::to_vec(&wrong_incremental_version).unwrap()
        )
        .is_err()
    );

    incremental
        .begin_catch_up("2026-06-02T00:05:00Z".to_string())
        .unwrap();
    let mut page: HostedSlackHistoryPageV2 = catch_up_page().into();
    page.page_format_version = HOSTED_SLACK_POLL_PAGE_FORMAT_VERSION_V3;
    page.minimum_reader_version = HOSTED_SLACK_POLL_PAGE_MINIMUM_READER_VERSION_V3;
    page.poll_kind = HostedSlackPollKindV2::Incremental;
    page.backfill_cut_at = "2026-06-02T00:00:00Z".to_string();
    page.poll_overlap_watermark = "2026-06-01T23:55:00Z".to_string();
    page.poll_cut_at = Some("2026-06-02T00:05:00Z".to_string());
    page.observed_at = "2026-06-02T00:05:01Z".to_string();
    incremental.apply_history_page_v2(&page).unwrap();
    assert_eq!(
        incremental.phase(),
        HostedSlackPollPhaseV1::CompleteCandidate,
        "an empty incremental window must retain untouched applied roots without a reply sweep"
    );
    assert_eq!(
        incremental.completed_output().unwrap().snapshot,
        applied_snapshot
    );
}

#[test]
fn v1_poll_enums_remain_exhaustive_and_source_compatible() {
    fn kind_name(kind: HostedSlackPollKindV1) -> &'static str {
        match kind {
            HostedSlackPollKindV1::Bootstrap => "bootstrap",
            HostedSlackPollKindV1::FullRepair => "full_repair",
        }
    }
    fn evidence_name(evidence: HostedSlackPollEvidenceV1) -> &'static str {
        match evidence {
            HostedSlackPollEvidenceV1::AppliedPage { .. } => "applied_page",
            HostedSlackPollEvidenceV1::BeginCatchUp { .. } => "begin_catch_up",
        }
    }
    assert_eq!(kind_name(HostedSlackPollKindV1::Bootstrap), "bootstrap");
    assert_eq!(
        evidence_name(HostedSlackPollEvidenceV1::BeginCatchUp {
            poll_cut_at: "2026-06-02T00:05:00Z".to_string(),
        }),
        "begin_catch_up"
    );
}

#[allow(dead_code)]
fn public_poll_message_type_is_narrow(_: HostedSlackHistoryMessageV1, _: RawHostedSlackMessage) {}
