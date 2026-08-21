use std::collections::BTreeSet;

use locality_slack::{
    HttpSlackApiClient, SlackApi, SlackConversation, SlackConversationsListResponse,
    SlackResponseMetadata,
};

const DEFAULT_SLACK_TYPES: &str = "private_channel,im,mpim";

#[test]
#[ignore = "requires a live Slack OAuth credential and stable conversation; performs read-only API calls"]
fn live_auth_conversation_history_and_thread_reads_preserve_identity() {
    let token = oauth_access_token("LOCALITY_SLACK_LIVE_CREDENTIAL_JSON", "slack");
    let conversation_id = required_env("LOCALITY_SLACK_LIVE_CONVERSATION_ID");
    let types = configured_slack_types(std::env::var("LOCALITY_SLACK_LIVE_TYPES").ok());
    assert!(
        !types
            .split(',')
            .any(|value| value.trim() == "public_channel"),
        "live integrity refuses Slack public_channel because listing may auto-join elsewhere"
    );
    let api = HttpSlackApiClient::new(token);

    let auth = api
        .auth_test()
        .unwrap_or_else(|error| panic!("Slack auth.test failed: {error}"));
    assert!(auth.ok, "Slack auth.test did not report ok=true");

    let configured_conversation_found =
        conversation_in_paginated_list(&conversation_id, |cursor| {
            api.conversations_list(&types, cursor, 200)
                .unwrap_or_else(|error| panic!("Slack conversations.list failed: {error}"))
        });
    assert!(
        configured_conversation_found,
        "configured Slack conversation was not returned for the selected types"
    );

    let first = api
        .conversations_history(&conversation_id, None, 3)
        .unwrap_or_else(|error| panic!("Slack conversations.history failed: {error}"));
    let second = api
        .conversations_history(&conversation_id, None, 3)
        .unwrap_or_else(|error| panic!("Slack repeated conversations.history failed: {error}"));
    assert_eq!(
        first
            .messages
            .iter()
            .map(|message| &message.ts)
            .collect::<Vec<_>>(),
        second
            .messages
            .iter()
            .map(|message| &message.ts)
            .collect::<Vec<_>>(),
        "repeated Slack observation changed message identity"
    );

    if let Some(thread) = first
        .messages
        .iter()
        .find(|message| message.reply_count.unwrap_or(0) > 0)
    {
        let replies = api
            .conversations_replies(&conversation_id, &thread.ts, None, 15)
            .unwrap_or_else(|error| panic!("Slack conversations.replies failed: {error}"));
        assert!(
            replies
                .messages
                .first()
                .is_some_and(|message| message.ts == thread.ts),
            "Slack replies omitted or changed the thread root identity"
        );
    }
}

fn configured_slack_types(value: Option<String>) -> String {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_SLACK_TYPES.to_string())
}

fn conversation_in_paginated_list(
    conversation_id: &str,
    mut list: impl FnMut(Option<&str>) -> SlackConversationsListResponse,
) -> bool {
    let mut cursor = None;
    let mut seen_cursors = BTreeSet::new();
    loop {
        let conversations = list(cursor.as_deref());
        if conversations
            .channels
            .iter()
            .any(|channel| channel.id == conversation_id)
        {
            return true;
        }
        let Some(next_cursor) = conversations
            .response_metadata
            .next_cursor
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
        else {
            return false;
        };
        assert!(
            seen_cursors.insert(next_cursor.clone()),
            "Slack conversations.list repeated a pagination cursor"
        );
        cursor = Some(next_cursor);
    }
}

#[test]
fn empty_slack_types_use_the_private_conversation_default() {
    assert_eq!(configured_slack_types(None), DEFAULT_SLACK_TYPES);
    assert_eq!(
        configured_slack_types(Some("   ".to_string())),
        DEFAULT_SLACK_TYPES
    );
    assert_eq!(
        configured_slack_types(Some(" private_channel,im ".to_string())),
        "private_channel,im"
    );
}

#[test]
fn configured_conversation_lookup_follows_slack_pagination() {
    let mut requested_cursors = Vec::new();
    let mut pages = vec![
        SlackConversationsListResponse {
            ok: true,
            channels: vec![SlackConversation {
                id: "C_OTHER".to_string(),
                ..SlackConversation::default()
            }],
            response_metadata: SlackResponseMetadata {
                next_cursor: Some("page-two".to_string()),
            },
            ..SlackConversationsListResponse::default()
        },
        SlackConversationsListResponse {
            ok: true,
            channels: vec![SlackConversation {
                id: "C_CONFIGURED".to_string(),
                ..SlackConversation::default()
            }],
            ..SlackConversationsListResponse::default()
        },
    ]
    .into_iter();

    assert!(conversation_in_paginated_list("C_CONFIGURED", |cursor| {
        requested_cursors.push(cursor.map(str::to_string));
        pages.next().expect("one response per requested page")
    }));
    assert_eq!(requested_cursors, vec![None, Some("page-two".to_string())]);
}

fn required_env(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| panic!("set {name} to run the live Slack integrity test"))
}

fn oauth_access_token(environment: &str, connector: &str) -> String {
    let raw = required_env(environment);
    let value: serde_json::Value = serde_json::from_str(&raw)
        .unwrap_or_else(|_| panic!("{environment} must contain stored OAuth JSON"));
    assert_eq!(
        value.get("connector").and_then(|value| value.as_str()),
        Some(connector)
    );
    value
        .get("access_token")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| panic!("{environment} omitted access_token"))
        .to_string()
}
