use locality_connector::oauth_broker::OAuthBrokerToken;
use locality_slack::oauth::{
    SLACK_CONNECTOR_ID, SLACK_OAUTH_SCOPES, SlackOAuthScopeError, StoredSlackCredential,
    validate_slack_oauth_scopes,
};

#[test]
fn scope_validation_requires_public_channel_read_history_and_users() {
    let scopes = SLACK_OAUTH_SCOPES
        .iter()
        .map(|scope| scope.to_string())
        .collect::<Vec<_>>();

    assert_eq!(validate_slack_oauth_scopes(&scopes), Ok(()));

    let missing_history = scopes
        .iter()
        .filter(|scope| scope.as_str() != "channels:history")
        .cloned()
        .collect::<Vec<_>>();

    assert_eq!(
        validate_slack_oauth_scopes(&missing_history),
        Err(SlackOAuthScopeError::MissingRequiredScope(
            "channels:history"
        ))
    );
}

#[test]
fn stored_credential_refresh_preserves_refresh_handle_when_broker_omits_one() {
    let original = StoredSlackCredential::from_broker_token(
        OAuthBrokerToken {
            access_token: "old-token".to_string(),
            refresh_token_handle: Some("refresh-handle".to_string()),
            expires_in: Some(60),
            token_type: Some("bot".to_string()),
            account_id: Some("U123".to_string()),
            account_label: Some("ada@example.com".to_string()),
            workspace_id: Some("T123".to_string()),
            workspace_name: Some("Example".to_string()),
            scopes: SLACK_OAUTH_SCOPES
                .iter()
                .map(|scope| scope.to_string())
                .collect(),
        },
        "client-id".to_string(),
        "https://broker.example".to_string(),
        100,
    );

    let refreshed = original
        .refreshed(
            OAuthBrokerToken {
                access_token: "new-token".to_string(),
                refresh_token_handle: None,
                expires_in: Some(120),
                token_type: None,
                account_id: None,
                account_label: None,
                workspace_id: None,
                workspace_name: None,
                scopes: Vec::new(),
            },
            200,
        )
        .expect("refresh");

    assert_eq!(refreshed.connector, SLACK_CONNECTOR_ID);
    assert_eq!(refreshed.access_token, "new-token");
    assert_eq!(
        refreshed.refresh_token_handle.as_deref(),
        Some("refresh-handle")
    );
    assert_eq!(refreshed.expires_at, Some(320));
    assert_eq!(refreshed.workspace_name.as_deref(), Some("Example"));
}
