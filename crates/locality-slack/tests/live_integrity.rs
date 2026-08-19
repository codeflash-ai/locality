use locality_slack::{HttpSlackApiClient, SlackApi};

#[test]
#[ignore = "requires a live Slack OAuth credential and stable conversation; performs read-only API calls"]
fn live_auth_conversation_history_and_thread_reads_preserve_identity() {
    let token = oauth_access_token("LOCALITY_SLACK_LIVE_CREDENTIAL_JSON", "slack");
    let conversation_id = required_env("LOCALITY_SLACK_LIVE_CONVERSATION_ID");
    let types = std::env::var("LOCALITY_SLACK_LIVE_TYPES")
        .unwrap_or_else(|_| "private_channel,im,mpim".to_string());
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

    let conversations = api
        .conversations_list(&types, None, 200)
        .unwrap_or_else(|error| panic!("Slack conversations.list failed: {error}"));
    assert!(
        conversations
            .channels
            .iter()
            .any(|channel| channel.id == conversation_id),
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
