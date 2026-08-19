use locality_gmail::client::{GmailApi, HttpGmailApiClient};

#[test]
#[ignore = "requires a live Gmail OAuth credential; performs read-only mailbox API calls"]
fn live_mailbox_lists_and_hydrates_without_mutation() {
    let token = oauth_access_token("LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON", "gmail");
    let api = HttpGmailApiClient::new(token);

    for label in ["INBOX", "SENT"] {
        let page = api
            .list_messages(label, 2, None, None)
            .unwrap_or_else(|error| panic!("Gmail {label} list failed: {error}"));
        assert!(
            page.messages.len() <= 2,
            "Gmail ignored the requested page size"
        );
        if let Some(message) = page.messages.first() {
            let hydrated = api
                .get_message_full(&message.id)
                .unwrap_or_else(|error| panic!("Gmail {label} hydration failed: {error}"));
            assert_eq!(
                hydrated.id, message.id,
                "Gmail hydration changed message identity"
            );
            assert!(
                hydrated.payload.is_some(),
                "Gmail full message omitted its MIME payload"
            );
        }
    }

    let drafts = api
        .list_drafts(2, None, None)
        .unwrap_or_else(|error| panic!("Gmail draft list failed: {error}"));
    assert!(
        drafts.drafts.len() <= 2,
        "Gmail ignored the draft page size"
    );
    if let Some(draft) = drafts.drafts.first() {
        let hydrated = api
            .get_draft_full(&draft.id)
            .unwrap_or_else(|error| panic!("Gmail draft hydration failed: {error}"));
        assert_eq!(
            hydrated.id, draft.id,
            "Gmail draft hydration changed identity"
        );
    }
}

fn oauth_access_token(environment: &str, connector: &str) -> String {
    let raw = std::env::var(environment)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| panic!("set {environment} to run the live Gmail integrity test"));
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
