use loc_cli::connect::{
    BrokerOAuthConnectOptions, ConnectOptions, DEFAULT_GMAIL_OAUTH_PROFILE_ID,
    DEFAULT_GOOGLE_DOCS_OAUTH_PROFILE_ID, DEFAULT_NOTION_OAUTH_PROFILE_ID,
    DEFAULT_NOTION_PROFILE_ID, GmailBrokerOAuthConnectOptions, GmailOAuthBrokerExchange,
    GoogleDocsBrokerOAuthConnectOptions, GoogleDocsOAuthBrokerExchange, NotionConnectionProbe,
    NotionConnectionProbeResult, NotionOAuthBrokerExchange, NotionOAuthExchange,
    OAuthConnectOptions, OAuthExchangeFailure, run_connect_gmail_broker_oauth,
    run_connect_google_docs_broker_oauth, run_connect_notion, run_connect_notion_broker_oauth,
    run_connect_notion_oauth, run_disconnect, run_profiles,
};
use locality_connector::oauth_broker::{OAuthBrokerCodeExchange, OAuthBrokerToken};
use locality_gmail::{GMAIL_OAUTH_SCOPES, StoredGmailCredential};
use locality_google_docs::{GOOGLE_DOCS_OAUTH_SCOPES, StoredGoogleDocsCredential};
use locality_notion::oauth::{
    NotionOAuthBrokerCodeExchange, NotionOAuthCodeExchange, NotionOAuthToken,
    StoredNotionCredential,
};
use locality_store::{
    ConnectionId, ConnectionRepository, ConnectorProfileId, ConnectorProfileRepository,
    CredentialStore, InMemoryCredentialStore, InMemoryStateStore,
};

#[test]
fn connect_notion_stores_metadata_and_secret_separately() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let probe = FakeProbe;

    let report = run_connect_notion(
        &mut store,
        &credentials,
        ConnectOptions {
            connection_id: Some(ConnectionId::new("work")),
            token: "ntn_secret_test_token".to_string(),
        },
        &probe,
    )
    .expect("connect");

    assert_eq!(report.connection_id, "work");
    assert_eq!(report.profile_id, DEFAULT_NOTION_PROFILE_ID);
    assert_eq!(report.workspace_name.as_deref(), Some("Locality"));
    assert_eq!(
        credentials
            .get("connection:work")
            .expect("credential saved"),
        "ntn_secret_test_token"
    );

    let connection = store
        .get_connection(&ConnectionId::new("work"))
        .expect("get connection")
        .expect("connection");
    assert_eq!(connection.secret_ref, "connection:work");
    assert_eq!(
        connection.profile_id,
        Some(ConnectorProfileId::new(DEFAULT_NOTION_PROFILE_ID))
    );
    assert_eq!(connection.status, "active");
    let profile = store
        .get_connector_profile(&ConnectorProfileId::new(DEFAULT_NOTION_PROFILE_ID))
        .expect("get profile")
        .expect("profile");
    assert_eq!(profile.connector, "notion");
    assert_eq!(profile.auth_kind, "token");

    let json = serde_json::to_string(&report).expect("json");
    assert!(!json.contains("ntn_secret_test_token"));
    assert!(!json.contains("secret_ref"));
}

#[test]
fn connect_notion_oauth_stores_oauth_bundle_and_metadata() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let exchange = FakeOAuthExchange;

    let report = run_connect_notion_oauth(
        &mut store,
        &credentials,
        OAuthConnectOptions {
            connection_id: Some(ConnectionId::new("work")),
            client_id: "client-id".to_string(),
            client_secret: "client-secret".to_string(),
            code: "oauth-code".to_string(),
            redirect_uri: "http://localhost:8757/oauth/notion/callback".to_string(),
        },
        &exchange,
    )
    .expect("connect oauth");

    assert_eq!(report.connection_id, "work");
    assert_eq!(report.profile_id, DEFAULT_NOTION_OAUTH_PROFILE_ID);
    assert_eq!(report.auth_kind, "oauth");
    assert_eq!(report.workspace_name.as_deref(), Some("Locality"));

    let secret = credentials
        .get("connection:work")
        .expect("credential saved");
    let stored = serde_json::from_str::<StoredNotionCredential>(&secret).expect("stored oauth");
    assert_eq!(stored.kind, "oauth");
    assert_eq!(stored.access_token, "oauth-access-token");
    assert_eq!(stored.refresh_token.as_deref(), Some("oauth-refresh-token"));
    assert_eq!(stored.oauth_client_id.as_deref(), Some("client-id"));
    assert_eq!(stored.oauth_client_secret.as_deref(), Some("client-secret"));
    assert_eq!(stored.oauth_broker_url, None);
    assert_eq!(stored.refresh_token_handle, None);

    let connection = store
        .get_connection(&ConnectionId::new("work"))
        .expect("get connection")
        .expect("connection");
    assert_eq!(connection.auth_kind, "oauth");
    assert_eq!(
        connection.profile_id,
        Some(ConnectorProfileId::new(DEFAULT_NOTION_OAUTH_PROFILE_ID))
    );
    assert_eq!(connection.workspace_id.as_deref(), Some("workspace-1"));

    let profile = store
        .get_connector_profile(&ConnectorProfileId::new(DEFAULT_NOTION_OAUTH_PROFILE_ID))
        .expect("get profile")
        .expect("profile");
    assert_eq!(profile.auth_kind, "oauth");

    let json = serde_json::to_string(&report).expect("json");
    assert!(!json.contains("oauth-access-token"));
    assert!(!json.contains("oauth-refresh-token"));
    assert!(!json.contains("client-secret"));
    assert!(!json.contains("secret_ref"));
}

#[test]
fn connect_notion_broker_oauth_stores_refresh_handle_without_client_secret() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let exchange = FakeBrokerOAuthExchange;

    let report = run_connect_notion_broker_oauth(
        &mut store,
        &credentials,
        BrokerOAuthConnectOptions {
            connection_id: Some(ConnectionId::new("work")),
            broker_url: "https://auth.example.test".to_string(),
            client_id: "client-id".to_string(),
            session: "broker-session".to_string(),
            state: "state-1".to_string(),
            code: "oauth-code".to_string(),
            redirect_uri: "http://localhost:8757/oauth/notion/callback".to_string(),
        },
        &exchange,
    )
    .expect("connect oauth");

    assert_eq!(report.connection_id, "work");
    assert_eq!(report.profile_id, DEFAULT_NOTION_OAUTH_PROFILE_ID);
    assert_eq!(report.auth_kind, "oauth");
    assert_eq!(report.workspace_name.as_deref(), Some("Locality"));

    let secret = credentials
        .get("connection:work")
        .expect("credential saved");
    let stored = serde_json::from_str::<StoredNotionCredential>(&secret).expect("stored oauth");
    assert_eq!(stored.kind, "oauth");
    assert_eq!(stored.access_token, "oauth-access-token");
    assert_eq!(stored.refresh_token, None);
    assert_eq!(
        stored.refresh_token_handle.as_deref(),
        Some("opaque-refresh-handle")
    );
    assert_eq!(stored.oauth_client_id.as_deref(), Some("client-id"));
    assert_eq!(stored.oauth_client_secret, None);
    assert_eq!(
        stored.oauth_broker_url.as_deref(),
        Some("https://auth.example.test")
    );

    let json = serde_json::to_string(&report).expect("json");
    assert!(!json.contains("oauth-access-token"));
    assert!(!json.contains("opaque-refresh-handle"));
    assert!(!json.contains("client-secret"));
    assert!(!json.contains("secret_ref"));
}

#[test]
fn connect_google_docs_broker_oauth_stores_refresh_handle_without_secrets() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let exchange = FakeGoogleDocsBrokerOAuthExchange;

    let report = run_connect_google_docs_broker_oauth(
        &mut store,
        &credentials,
        GoogleDocsBrokerOAuthConnectOptions {
            connection_id: Some(ConnectionId::new("docs-work")),
            broker_url: "https://auth.example.test".to_string(),
            client_id: "client-id".to_string(),
            session: "broker-session".to_string(),
            state: "state-1".to_string(),
            code: "oauth-code".to_string(),
            redirect_uri: "http://localhost:8757/oauth/google-docs/callback".to_string(),
        },
        &exchange,
    )
    .expect("connect google docs oauth");

    assert_eq!(report.connection_id, "docs-work");
    assert_eq!(report.profile_id, DEFAULT_GOOGLE_DOCS_OAUTH_PROFILE_ID);
    assert_eq!(report.connector, "google-docs");
    assert_eq!(report.auth_kind, "oauth");
    assert_eq!(report.account_label.as_deref(), Some("user@example.com"));

    let secret = credentials
        .get("connection:docs-work")
        .expect("credential saved");
    let stored = serde_json::from_str::<StoredGoogleDocsCredential>(&secret).expect("stored oauth");
    assert_eq!(
        stored.refresh_token_handle.as_deref(),
        Some("opaque-refresh-handle")
    );
    assert_eq!(
        stored.oauth_broker_url.as_deref(),
        Some("https://auth.example.test")
    );

    let json = serde_json::to_string(&report).expect("json");
    assert!(!json.contains("oauth-access-token"));
    assert!(!json.contains("opaque-refresh-handle"));
    assert!(!json.contains("client-secret"));
    assert!(!json.contains("secret_ref"));
}

#[test]
fn connect_gmail_broker_oauth_stores_refresh_handle_without_secrets() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let exchange = FakeGmailBrokerOAuthExchange;

    let report = run_connect_gmail_broker_oauth(
        &mut store,
        &credentials,
        GmailBrokerOAuthConnectOptions {
            connection_id: Some(ConnectionId::new("gmail-default")),
            broker_url: "https://auth.example.test".to_string(),
            client_id: "gmail-client-id".to_string(),
            session: "broker-session".to_string(),
            state: "state-1".to_string(),
            code: "oauth-code".to_string(),
            redirect_uri: "http://localhost:8757/oauth/gmail/callback".to_string(),
        },
        &exchange,
    )
    .expect("connect gmail oauth");

    assert_eq!(report.connection_id, "gmail-default");
    assert_eq!(report.profile_id, DEFAULT_GMAIL_OAUTH_PROFILE_ID);
    assert_eq!(report.connector, "gmail");
    assert_eq!(report.auth_kind, "oauth");
    assert_eq!(report.account_label.as_deref(), Some("user@example.com"));

    let secret = credentials
        .get("connection:gmail-default")
        .expect("credential saved");
    let stored = serde_json::from_str::<StoredGmailCredential>(&secret).expect("stored oauth");
    assert_eq!(
        stored.refresh_token_handle.as_deref(),
        Some("opaque-refresh-handle")
    );
    assert_eq!(
        stored.oauth_broker_url.as_deref(),
        Some("https://auth.example.test")
    );

    let json = serde_json::to_string(&report).expect("json");
    assert!(!json.contains("oauth-access-token"));
    assert!(!json.contains("opaque-refresh-handle"));
    assert!(!json.contains("client-secret"));
    assert!(!json.contains("secret_ref"));
}

#[test]
fn connect_gmail_broker_oauth_accepts_worker_scope_string() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let exchange = JsonGmailBrokerOAuthExchange {
        payload: gmail_worker_token_payload(GMAIL_OAUTH_SCOPES.join(" ")),
    };

    run_connect_gmail_broker_oauth(&mut store, &credentials, gmail_connect_options(), &exchange)
        .expect("connect gmail oauth");

    let secret = credentials
        .get("connection:gmail-default")
        .expect("credential saved");
    let stored = serde_json::from_str::<StoredGmailCredential>(&secret).expect("stored oauth");
    assert_eq!(
        stored.scopes,
        GMAIL_OAUTH_SCOPES
            .iter()
            .map(|scope| scope.to_string())
            .collect::<Vec<_>>()
    );
}

#[test]
fn connect_gmail_broker_oauth_rejects_missing_required_scope() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let exchange = ScopedFakeGmailBrokerOAuthExchange {
        scopes: GMAIL_OAUTH_SCOPES
            .iter()
            .filter(|scope| **scope != "https://www.googleapis.com/auth/gmail.compose")
            .map(|scope| scope.to_string())
            .collect(),
    };

    let error = run_connect_gmail_broker_oauth(
        &mut store,
        &credentials,
        gmail_connect_options(),
        &exchange,
    )
    .expect_err("missing Gmail compose scope must be rejected");

    assert_eq!(error.code(), "oauth_exchange_failed");
    assert!(
        error
            .message()
            .contains("missing required Gmail OAuth scope")
    );
    assert!(
        error
            .message()
            .contains("https://www.googleapis.com/auth/gmail.compose")
    );
    assert!(credentials.get("connection:gmail-default").is_err());
    assert!(
        store
            .get_connection(&ConnectionId::new("gmail-default"))
            .expect("lookup connection")
            .is_none()
    );
}

#[test]
fn connect_gmail_broker_oauth_scope_validation_reports_gmail_guidance() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let exchange = ScopedFakeGmailBrokerOAuthExchange {
        scopes: GMAIL_OAUTH_SCOPES
            .iter()
            .filter(|scope| **scope != "https://www.googleapis.com/auth/gmail.compose")
            .map(|scope| scope.to_string())
            .collect(),
    };

    let error = run_connect_gmail_broker_oauth(
        &mut store,
        &credentials,
        gmail_connect_options(),
        &exchange,
    )
    .expect_err("missing Gmail compose scope must be rejected");
    let message = error.message();

    assert_eq!(error.code(), "oauth_exchange_failed");
    assert!(
        message.starts_with("Gmail OAuth exchange failed: "),
        "{message}"
    );
    assert!(!message.contains("Notion OAuth"), "{message}");
    assert_eq!(error.suggested_command(), Some("loc connect gmail"));
}

#[test]
fn connect_oauth_exchange_errors_report_connector_guidance() {
    let notion = loc_cli::connect::ConnectError::OAuthExchangeFailed(OAuthExchangeFailure::notion(
        "authorization code was rejected",
    ));
    assert_eq!(
        notion.message(),
        "Notion OAuth exchange failed: authorization code was rejected"
    );
    assert_eq!(notion.suggested_command(), Some("loc connect notion"));

    let google_docs = loc_cli::connect::ConnectError::OAuthExchangeFailed(
        OAuthExchangeFailure::google_docs("authorization code was rejected"),
    );
    let google_docs_message = google_docs.message();
    assert_eq!(
        google_docs_message,
        "Google Docs OAuth exchange failed: authorization code was rejected"
    );
    assert!(!google_docs_message.contains("Notion OAuth"));
    assert_eq!(
        google_docs.suggested_command(),
        Some("loc connect google-docs")
    );

    let gmail = loc_cli::connect::ConnectError::OAuthExchangeFailed(OAuthExchangeFailure::gmail(
        "authorization code was rejected",
    ));
    let gmail_message = gmail.message();
    assert_eq!(
        gmail_message,
        "Gmail OAuth exchange failed: authorization code was rejected"
    );
    assert!(!gmail_message.contains("Notion OAuth"));
    assert_eq!(gmail.suggested_command(), Some("loc connect gmail"));
}

#[test]
fn connect_gmail_broker_oauth_rejects_full_mailbox_scope() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let mut scopes = GMAIL_OAUTH_SCOPES
        .iter()
        .rev()
        .map(|scope| scope.to_string())
        .collect::<Vec<_>>();
    scopes.push("https://mail.google.com/".to_string());
    let exchange = ScopedFakeGmailBrokerOAuthExchange { scopes };

    let error = run_connect_gmail_broker_oauth(
        &mut store,
        &credentials,
        gmail_connect_options(),
        &exchange,
    )
    .expect_err("full Gmail mailbox scope must be rejected");

    assert_eq!(error.code(), "oauth_exchange_failed");
    assert!(
        error
            .message()
            .contains("Gmail OAuth broker returned unsupported full mailbox scope")
    );
    assert!(error.message().contains("https://mail.google.com/"));
    assert!(credentials.get("connection:gmail-default").is_err());
    assert!(
        store
            .get_connection(&ConnectionId::new("gmail-default"))
            .expect("lookup connection")
            .is_none()
    );
}

#[test]
fn connect_gmail_broker_oauth_rejects_full_mailbox_scope_from_worker_scope_string() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let mut scope = GMAIL_OAUTH_SCOPES.join(" ");
    scope.push_str(" https://mail.google.com/");
    let exchange = JsonGmailBrokerOAuthExchange {
        payload: gmail_worker_token_payload(scope),
    };

    let error = run_connect_gmail_broker_oauth(
        &mut store,
        &credentials,
        gmail_connect_options(),
        &exchange,
    )
    .expect_err("full Gmail mailbox scope must be rejected");

    assert_eq!(error.code(), "oauth_exchange_failed");
    assert!(
        error
            .message()
            .contains("Gmail OAuth broker returned unsupported full mailbox scope")
    );
    assert!(error.message().contains("https://mail.google.com/"));
    assert!(credentials.get("connection:gmail-default").is_err());
    assert!(
        store
            .get_connection(&ConnectionId::new("gmail-default"))
            .expect("lookup connection")
            .is_none()
    );
}

#[test]
fn connect_google_docs_reuses_default_id_when_previous_default_is_revoked() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let exchange = FakeGoogleDocsBrokerOAuthExchange;

    run_connect_google_docs_broker_oauth(
        &mut store,
        &credentials,
        GoogleDocsBrokerOAuthConnectOptions {
            connection_id: Some(ConnectionId::new("google-docs-default")),
            broker_url: "https://auth.example.test".to_string(),
            client_id: "client-id".to_string(),
            session: "broker-session".to_string(),
            state: "state-1".to_string(),
            code: "oauth-code".to_string(),
            redirect_uri: "http://localhost:8757/oauth/google-docs/callback".to_string(),
        },
        &exchange,
    )
    .expect("initial connect");
    run_disconnect(
        &mut store,
        &credentials,
        ConnectionId::new("google-docs-default"),
    )
    .expect("disconnect");

    let report = run_connect_google_docs_broker_oauth(
        &mut store,
        &credentials,
        GoogleDocsBrokerOAuthConnectOptions {
            connection_id: None,
            broker_url: "https://auth.example.test".to_string(),
            client_id: "client-id".to_string(),
            session: "broker-session".to_string(),
            state: "state-1".to_string(),
            code: "oauth-code".to_string(),
            redirect_uri: "http://localhost:8757/oauth/google-docs/callback".to_string(),
        },
        &exchange,
    )
    .expect("reconnect default");

    assert_eq!(report.connection_id, "google-docs-default");
    assert_eq!(
        store
            .get_connection(&ConnectionId::new("google-docs-default"))
            .expect("get connection")
            .expect("connection")
            .status,
        "active"
    );
}

#[test]
fn profiles_list_auth_configs_without_secrets() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let probe = FakeProbe;
    run_connect_notion(
        &mut store,
        &credentials,
        ConnectOptions {
            connection_id: Some(ConnectionId::new("work")),
            token: "ntn_secret_test_token".to_string(),
        },
        &probe,
    )
    .expect("connect");

    let report = run_profiles(&store).expect("profiles");

    assert_eq!(report.profiles.len(), 1);
    assert_eq!(report.profiles[0].profile_id, DEFAULT_NOTION_PROFILE_ID);
    assert_eq!(report.profiles[0].connector_version, "notion.v1");
    let json = serde_json::to_string(&report).expect("json");
    assert!(!json.contains("ntn_secret_test_token"));
    assert!(!json.contains("secret_ref"));
}

#[test]
fn disconnect_revokes_connection_and_deletes_credential() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let probe = FakeProbe;
    run_connect_notion(
        &mut store,
        &credentials,
        ConnectOptions {
            connection_id: Some(ConnectionId::new("work")),
            token: "ntn_secret_test_token".to_string(),
        },
        &probe,
    )
    .expect("connect");

    let report =
        run_disconnect(&mut store, &credentials, ConnectionId::new("work")).expect("disconnect");

    assert_eq!(report.status, "revoked");
    assert!(credentials.get("connection:work").is_err());
    assert_eq!(
        store
            .get_connection(&ConnectionId::new("work"))
            .expect("get connection")
            .expect("connection")
            .status,
        "revoked"
    );
}

#[derive(Clone, Debug)]
struct FakeProbe;

impl NotionConnectionProbe for FakeProbe {
    fn probe(
        &self,
        token: &str,
    ) -> Result<NotionConnectionProbeResult, loc_cli::connect::ConnectError> {
        assert_eq!(token, "ntn_secret_test_token");
        Ok(NotionConnectionProbeResult {
            account_label: Some("agent@example.com".to_string()),
            workspace_id: Some("workspace-1".to_string()),
            workspace_name: Some("Locality".to_string()),
        })
    }
}

#[derive(Clone, Debug)]
struct FakeOAuthExchange;

impl NotionOAuthExchange for FakeOAuthExchange {
    fn exchange_code(
        &self,
        request: &NotionOAuthCodeExchange,
    ) -> Result<NotionOAuthToken, loc_cli::connect::ConnectError> {
        assert_eq!(request.client_id, "client-id");
        assert_eq!(request.client_secret, "client-secret");
        assert_eq!(request.code, "oauth-code");
        Ok(NotionOAuthToken {
            access_token: "oauth-access-token".to_string(),
            token_type: Some("bearer".to_string()),
            refresh_token: Some("oauth-refresh-token".to_string()),
            refresh_token_kind: None,
            refresh_token_handle: None,
            expires_in: Some(3600),
            bot_id: Some("bot-1".to_string()),
            workspace_id: Some("workspace-1".to_string()),
            workspace_name: Some("Locality".to_string()),
            workspace_icon: None,
            owner: None,
            duplicated_template_id: None,
        })
    }
}

#[derive(Clone, Debug)]
struct FakeBrokerOAuthExchange;

impl NotionOAuthBrokerExchange for FakeBrokerOAuthExchange {
    fn exchange_code(
        &self,
        request: &NotionOAuthBrokerCodeExchange,
    ) -> Result<NotionOAuthToken, loc_cli::connect::ConnectError> {
        assert_eq!(request.session, "broker-session");
        assert_eq!(request.state, "state-1");
        assert_eq!(request.code, "oauth-code");
        assert_eq!(
            request.redirect_uri,
            "http://localhost:8757/oauth/notion/callback"
        );
        Ok(NotionOAuthToken {
            access_token: "oauth-access-token".to_string(),
            token_type: Some("bearer".to_string()),
            refresh_token: None,
            refresh_token_kind: Some("handle".to_string()),
            refresh_token_handle: Some("opaque-refresh-handle".to_string()),
            expires_in: Some(3600),
            bot_id: Some("bot-1".to_string()),
            workspace_id: Some("workspace-1".to_string()),
            workspace_name: Some("Locality".to_string()),
            workspace_icon: None,
            owner: None,
            duplicated_template_id: None,
        })
    }
}

#[derive(Clone, Debug)]
struct FakeGoogleDocsBrokerOAuthExchange;

impl GoogleDocsOAuthBrokerExchange for FakeGoogleDocsBrokerOAuthExchange {
    fn exchange_code(
        &self,
        request: &OAuthBrokerCodeExchange,
    ) -> Result<OAuthBrokerToken, loc_cli::connect::ConnectError> {
        assert_eq!(request.connector, "google-docs");
        assert_eq!(request.session, "broker-session");
        assert_eq!(request.state, "state-1");
        assert_eq!(request.code, "oauth-code");
        assert_eq!(
            request.redirect_uri,
            "http://localhost:8757/oauth/google-docs/callback"
        );
        Ok(OAuthBrokerToken {
            access_token: "oauth-access-token".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: Some(3600),
            refresh_token_handle: Some("opaque-refresh-handle".to_string()),
            account_id: Some("acct-1".to_string()),
            account_label: Some("user@example.com".to_string()),
            workspace_id: Some("google-drive".to_string()),
            workspace_name: Some("Google Drive".to_string()),
            scopes: GOOGLE_DOCS_OAUTH_SCOPES
                .iter()
                .map(|scope| scope.to_string())
                .collect(),
        })
    }
}

#[derive(Clone, Debug)]
struct FakeGmailBrokerOAuthExchange;

impl GmailOAuthBrokerExchange for FakeGmailBrokerOAuthExchange {
    fn exchange_code(
        &self,
        request: &OAuthBrokerCodeExchange,
    ) -> Result<OAuthBrokerToken, loc_cli::connect::ConnectError> {
        assert_eq!(request.connector, "gmail");
        assert_eq!(request.session, "broker-session");
        assert_eq!(request.state, "state-1");
        assert_eq!(request.code, "oauth-code");
        assert_eq!(
            request.redirect_uri,
            "http://localhost:8757/oauth/gmail/callback"
        );
        Ok(OAuthBrokerToken {
            access_token: "oauth-access-token".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: Some(3600),
            refresh_token_handle: Some("opaque-refresh-handle".to_string()),
            account_id: Some("acct-1".to_string()),
            account_label: Some("user@example.com".to_string()),
            workspace_id: Some("gmail".to_string()),
            workspace_name: Some("Gmail".to_string()),
            scopes: GMAIL_OAUTH_SCOPES
                .iter()
                .rev()
                .map(|scope| scope.to_string())
                .collect(),
        })
    }
}

#[derive(Clone, Debug)]
struct ScopedFakeGmailBrokerOAuthExchange {
    scopes: Vec<String>,
}

impl GmailOAuthBrokerExchange for ScopedFakeGmailBrokerOAuthExchange {
    fn exchange_code(
        &self,
        request: &OAuthBrokerCodeExchange,
    ) -> Result<OAuthBrokerToken, loc_cli::connect::ConnectError> {
        assert_eq!(request.connector, "gmail");
        assert_eq!(request.session, "broker-session");
        assert_eq!(request.state, "state-1");
        assert_eq!(request.code, "oauth-code");
        assert_eq!(
            request.redirect_uri,
            "http://localhost:8757/oauth/gmail/callback"
        );
        Ok(gmail_broker_token(self.scopes.clone()))
    }
}

#[derive(Clone, Debug)]
struct JsonGmailBrokerOAuthExchange {
    payload: serde_json::Value,
}

impl GmailOAuthBrokerExchange for JsonGmailBrokerOAuthExchange {
    fn exchange_code(
        &self,
        request: &OAuthBrokerCodeExchange,
    ) -> Result<OAuthBrokerToken, loc_cli::connect::ConnectError> {
        assert_eq!(request.connector, "gmail");
        assert_eq!(request.session, "broker-session");
        assert_eq!(request.state, "state-1");
        assert_eq!(request.code, "oauth-code");
        assert_eq!(
            request.redirect_uri,
            "http://localhost:8757/oauth/gmail/callback"
        );
        Ok(serde_json::from_value(self.payload.clone()).expect("decode worker-shaped token"))
    }
}

fn gmail_connect_options() -> GmailBrokerOAuthConnectOptions {
    GmailBrokerOAuthConnectOptions {
        connection_id: Some(ConnectionId::new("gmail-default")),
        broker_url: "https://auth.example.test".to_string(),
        client_id: "gmail-client-id".to_string(),
        session: "broker-session".to_string(),
        state: "state-1".to_string(),
        code: "oauth-code".to_string(),
        redirect_uri: "http://localhost:8757/oauth/gmail/callback".to_string(),
    }
}

fn gmail_broker_token(scopes: Vec<String>) -> OAuthBrokerToken {
    OAuthBrokerToken {
        access_token: "oauth-access-token".to_string(),
        token_type: Some("Bearer".to_string()),
        expires_in: Some(3600),
        refresh_token_handle: Some("opaque-refresh-handle".to_string()),
        account_id: Some("acct-1".to_string()),
        account_label: Some("user@example.com".to_string()),
        workspace_id: Some("gmail".to_string()),
        workspace_name: Some("Gmail".to_string()),
        scopes,
    }
}

fn gmail_worker_token_payload(scope: String) -> serde_json::Value {
    serde_json::json!({
        "access_token": "oauth-access-token",
        "token_type": "Bearer",
        "expires_in": 3600,
        "refresh_token_handle": "opaque-refresh-handle",
        "account_id": "acct-1",
        "account_label": "user@example.com",
        "workspace_id": "gmail",
        "workspace_name": "Gmail",
        "scope": scope,
    })
}
