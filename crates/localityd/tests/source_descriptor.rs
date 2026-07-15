use locality_connector::Connector;
use locality_connector::oauth_broker::OAuthBrokerToken;
use locality_core::canonical::parse_canonical_markdown;
use locality_core::model::{EntityKind, MountId, RemoteId};
use locality_core::push::BodyDiffMode;
use locality_core::shadow::ShadowDocument;
use locality_core::validation::ValidationIssue;
use locality_gmail::{GMAIL_CONNECTOR_ID, GMAIL_OAUTH_SCOPES, StoredGmailCredential};
use locality_google_docs::{GOOGLE_DOCS_CONNECTOR_ID, StoredGoogleDocsCredential};
use locality_granola::GRANOLA_CONNECTOR_ID;
use locality_notion::client::DEFAULT_NOTION_TOKEN_ENV;
use locality_store::{
    ConnectionId, ConnectionRecord, ConnectionRepository, ConnectorProfileId,
    ConnectorProfileRecord, ConnectorProfileRepository, CredentialStore, InMemoryCredentialStore,
    InMemoryStateStore, MountConfig,
};
use localityd::source::{
    LocalSourceValidator, ResolvedSource, ResolvedSourceSet, SourcePushValidator,
    SourceValidationContext, resolve_source_for_mount, source_create_decision_for_parent_path,
    source_descriptor, source_display_name, source_write_decision_for_path,
    supported_source_connectors,
};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

#[test]
fn notion_descriptor_exposes_cli_and_mount_metadata() {
    let descriptor = source_descriptor("notion");

    assert_eq!(descriptor.id(), "notion");
    assert_eq!(descriptor.display_name(), "Notion");
    assert_eq!(descriptor.default_mount_id(), "notion-main");
    assert_eq!(descriptor.connect_command(), Some("loc connect notion"));
    assert_eq!(descriptor.auth_env_var(), Some(DEFAULT_NOTION_TOKEN_ENV));
    assert!(descriptor.supports_oauth());
    assert!(descriptor.mount_guidance().contains("Notion facts:"));
    assert_eq!(descriptor.periodic_discovery_interval(), None);
}

#[test]
fn google_docs_descriptor_comes_from_registry() {
    let descriptor = source_descriptor("google-docs");

    assert_eq!(descriptor.id(), "google-docs");
    assert_eq!(descriptor.display_name(), "Google Docs");
    assert_eq!(descriptor.default_mount_id(), "google-docs-main");
    assert_eq!(
        descriptor.connect_command(),
        Some("loc connect google-docs")
    );
    assert_eq!(descriptor.auth_env_var(), None);
    assert!(descriptor.supports_oauth());
    assert!(
        descriptor
            .mount_guidance()
            .contains("# Locality Google Docs Mount")
    );
    assert!(descriptor.mount_guidance().contains("Drive metadata"));
    assert!(
        descriptor
            .mount_guidance()
            .contains("Docs manually added inside the workspace folder")
    );
}

#[test]
fn gmail_descriptor_comes_from_registry() {
    let descriptor = source_descriptor("gmail");

    assert_eq!(descriptor.id(), "gmail");
    assert_eq!(descriptor.display_name(), "Gmail");
    assert_eq!(descriptor.default_mount_id(), "gmail-main");
    assert_eq!(descriptor.connect_command(), Some("loc connect gmail"));
    assert_eq!(descriptor.auth_env_var(), None);
    assert!(descriptor.supports_oauth());
    assert!(descriptor.mount_guidance().contains("Gmail facts"));
    assert_eq!(
        descriptor.create_entity_parent_kinds(),
        &[EntityKind::Directory]
    );
}

#[test]
fn granola_descriptor_is_read_only_and_uses_api_key_setup() {
    let descriptor = source_descriptor(GRANOLA_CONNECTOR_ID);

    assert_eq!(descriptor.display_name(), "Granola");
    assert_eq!(descriptor.default_mount_id(), "granola-main");
    assert_eq!(
        descriptor.connect_command(),
        Some("loc connect granola --api-key-stdin")
    );
    assert!(!descriptor.supports_oauth());
    assert!(descriptor.create_entity_parent_kinds().is_empty());
    assert!(descriptor.mount_guidance().contains("read-only"));
    assert_eq!(
        descriptor.periodic_discovery_interval(),
        Some(Duration::from_secs(300))
    );
}

#[test]
fn granola_rejects_every_write_and_create_path() {
    let mut mount = MountConfig::new(
        MountId::new("granola-main"),
        GRANOLA_CONNECTOR_ID,
        "/tmp/locality/granola",
    );
    mount.read_only = false;
    assert!(
        !source_write_decision_for_path(&mount, std::path::Path::new("meeting/summary.md"))
            .is_writable()
    );
    assert!(
        !source_create_decision_for_parent_path(&mount, std::path::Path::new("meeting"))
            .is_writable()
    );
}

#[test]
fn generic_descriptor_preserves_source_id_in_guidance() {
    let descriptor = source_descriptor("linear");

    assert_eq!(descriptor.id(), "linear");
    assert_eq!(descriptor.display_name(), "Linear");
    assert_eq!(descriptor.default_mount_id(), "linear-main");
    assert_eq!(descriptor.connect_command(), None);
    assert_eq!(descriptor.auth_env_var(), None);
    assert!(!descriptor.supports_oauth());
    assert!(
        descriptor
            .mount_guidance()
            .contains("# Locality linear Mount")
    );
    assert!(descriptor.mount_guidance().contains("to linear"));
    assert_eq!(descriptor.body_diff_mode(), BodyDiffMode::WholeEntity);

    assert_eq!(
        source_descriptor("custom").body_diff_mode(),
        BodyDiffMode::Block
    );
}

#[test]
fn source_display_name_uses_descriptor_registry() {
    assert_eq!(source_display_name("notion"), "Notion");
    assert_eq!(source_display_name("google-docs"), "Google Docs");
    assert_eq!(source_display_name("linear"), "Linear");
    assert_eq!(source_display_name("custom"), "custom");
}

fn spawn_refresh_broker(status: &'static str, body: String) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind refresh broker");
    let url = format!("http://{}", listener.local_addr().expect("broker addr"));
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept refresh request");
        let mut buffer = [0_u8; 4096];
        let _ = stream.read(&mut buffer).expect("read refresh request");
        let response = format!(
            "{status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .expect("write refresh response");
    });
    (url, handle)
}

fn save_google_docs_oauth_connection(store: &mut InMemoryStateStore) -> (ConnectionId, String) {
    let profile_id = ConnectorProfileId::new("google-docs-oauth-default");
    let connection_id = ConnectionId::new("google-docs-default");
    let secret_ref = "connection:google-docs-default".to_string();

    store
        .save_connector_profile(ConnectorProfileRecord {
            profile_id: profile_id.clone(),
            connector: GOOGLE_DOCS_CONNECTOR_ID.to_string(),
            display_name: "Google Docs OAuth".to_string(),
            auth_kind: "oauth".to_string(),
            scopes: vec!["https://www.googleapis.com/auth/documents".to_string()],
            capabilities_json: "{}".to_string(),
            enabled_actions_json: "[]".to_string(),
            connector_version: "1".to_string(),
            status: "active".to_string(),
            created_at: "2026-06-25T10:00:00Z".to_string(),
            updated_at: "2026-06-25T10:00:00Z".to_string(),
        })
        .expect("save profile");
    store
        .save_connection(ConnectionRecord {
            connection_id: connection_id.clone(),
            profile_id: Some(profile_id),
            connector: GOOGLE_DOCS_CONNECTOR_ID.to_string(),
            display_name: "Google Docs".to_string(),
            account_label: Some("user@example.com".to_string()),
            workspace_id: Some("google-drive".to_string()),
            workspace_name: Some("Google Drive".to_string()),
            auth_kind: "oauth".to_string(),
            secret_ref: secret_ref.clone(),
            scopes: vec!["https://www.googleapis.com/auth/documents".to_string()],
            capabilities_json: "{}".to_string(),
            status: "active".to_string(),
            created_at: "2026-06-25T10:00:00Z".to_string(),
            updated_at: "2026-06-25T10:00:00Z".to_string(),
            expires_at: None,
        })
        .expect("save connection");

    (connection_id, secret_ref)
}

fn save_gmail_connection(
    store: &mut InMemoryStateStore,
    connection_id: &str,
    connector: &str,
    auth_kind: &str,
) -> (ConnectionId, String) {
    let profile_id = ConnectorProfileId::new(format!("{connection_id}-profile"));
    let connection_id = ConnectionId::new(connection_id);
    let secret_ref = format!("connection:{}", connection_id.0);

    store
        .save_connector_profile(ConnectorProfileRecord {
            profile_id: profile_id.clone(),
            connector: connector.to_string(),
            display_name: "Gmail OAuth".to_string(),
            auth_kind: auth_kind.to_string(),
            scopes: vec!["https://www.googleapis.com/auth/gmail.readonly".to_string()],
            capabilities_json: "{}".to_string(),
            enabled_actions_json: "[]".to_string(),
            connector_version: "1".to_string(),
            status: "active".to_string(),
            created_at: "2026-06-25T10:00:00Z".to_string(),
            updated_at: "2026-06-25T10:00:00Z".to_string(),
        })
        .expect("save profile");
    store
        .save_connection(ConnectionRecord {
            connection_id: connection_id.clone(),
            profile_id: Some(profile_id),
            connector: connector.to_string(),
            display_name: "Gmail".to_string(),
            account_label: Some("user@example.com".to_string()),
            workspace_id: Some("gmail".to_string()),
            workspace_name: Some("Gmail".to_string()),
            auth_kind: auth_kind.to_string(),
            secret_ref: secret_ref.clone(),
            scopes: vec!["https://www.googleapis.com/auth/gmail.readonly".to_string()],
            capabilities_json: "{}".to_string(),
            status: "active".to_string(),
            created_at: "2026-06-25T10:00:00Z".to_string(),
            updated_at: "2026-06-25T10:00:00Z".to_string(),
            expires_at: None,
        })
        .expect("save connection");

    (connection_id, secret_ref)
}

fn stored_gmail_credential(access_token: &str) -> StoredGmailCredential {
    StoredGmailCredential::from_broker_token(
        OAuthBrokerToken {
            access_token: access_token.to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: Some(3600),
            refresh_token_handle: Some("handle-1".to_string()),
            account_id: Some("acct-1".to_string()),
            account_label: Some("user@example.com".to_string()),
            workspace_id: Some("gmail".to_string()),
            workspace_name: Some("Gmail".to_string()),
            scopes: vec!["https://www.googleapis.com/auth/gmail.readonly".to_string()],
        },
        "client-id".to_string(),
        "https://auth.example.test".to_string(),
        4_102_444_800,
    )
}

fn expired_gmail_credential(access_token: &str, broker_url: String) -> StoredGmailCredential {
    let mut stored = StoredGmailCredential::from_broker_token(
        OAuthBrokerToken {
            access_token: access_token.to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: Some(1),
            refresh_token_handle: Some("handle-1".to_string()),
            account_id: Some("acct-1".to_string()),
            account_label: Some("user@example.com".to_string()),
            workspace_id: Some("gmail".to_string()),
            workspace_name: Some("Gmail".to_string()),
            scopes: GMAIL_OAUTH_SCOPES
                .iter()
                .map(|scope| scope.to_string())
                .collect(),
        },
        "client-id".to_string(),
        broker_url,
        1,
    );
    stored.expires_at = Some(1);
    stored
}

fn gmail_mount() -> MountConfig {
    MountConfig::new(MountId::new("gmail-main"), GMAIL_CONNECTOR_ID, "/tmp/gmail")
}

fn validate_gmail_create_issues(path: &str, markdown: &str) -> Vec<ValidationIssue> {
    let mount = gmail_mount();
    let parsed = parse_canonical_markdown(markdown).expect("parse gmail markdown");

    LocalSourceValidator
        .validate_create_frontmatter(SourceValidationContext {
            state_root: None,
            mount: &mount,
            parent: None,
            relative_path: std::path::Path::new(path),
            parsed: &parsed,
            shadow: None,
        })
        .expect("validate gmail create")
        .issues
}

fn validate_gmail_create(path: &str, markdown: &str) -> Vec<String> {
    validate_gmail_create_issues(path, markdown)
        .into_iter()
        .map(|issue| issue.code)
        .collect()
}

fn validate_gmail_changed(path: &str, markdown: &str) -> Vec<String> {
    let mount = gmail_mount();
    let parsed = parse_canonical_markdown(markdown).expect("parse gmail markdown");

    LocalSourceValidator
        .validate_changed_frontmatter(SourceValidationContext {
            state_root: None,
            mount: &mount,
            parent: None,
            relative_path: std::path::Path::new(path),
            parsed: &parsed,
            shadow: None,
        })
        .expect("validate gmail changed")
        .issues
        .into_iter()
        .map(|issue| issue.code)
        .collect()
}

#[test]
fn supported_source_connectors_include_first_party_connectors() {
    assert_eq!(
        supported_source_connectors(),
        vec!["notion", "google-docs", "gmail", "granola"]
    );
}

#[test]
fn resolving_unregistered_connector_reports_unsupported_connector() {
    let store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let mount = MountConfig::new(
        MountId::new("custom-main"),
        "custom",
        "/tmp/locality/custom",
    );

    let error = resolve_source_for_mount(&store, &credentials, &mount).expect_err("unsupported");

    assert_eq!(error.code(), "unsupported_connector");
    assert_eq!(
        error.message(),
        "connector `custom` is not supported by this build"
    );
}

#[test]
fn available_source_set_isolates_an_unavailable_mount() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let (connection_id, secret_ref) =
        save_gmail_connection(&mut store, "gmail-default", GMAIL_CONNECTOR_ID, "oauth");
    credentials
        .put(
            &secret_ref,
            &serde_json::to_string(&stored_gmail_credential("gmail-access-token"))
                .expect("credential json"),
        )
        .expect("save credential");
    let gmail = gmail_mount().with_connection_id(connection_id);
    let unavailable = MountConfig::new(
        MountId::new("custom-main"),
        "custom",
        "/tmp/locality/custom",
    );

    let (sources, failures) = ResolvedSourceSet::new_available(
        &store,
        &credentials,
        &[gmail.clone(), unavailable.clone()],
    );

    assert!(sources.contains_mount(&gmail.mount_id));
    assert!(!sources.contains_mount(&unavailable.mount_id));
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].0, unavailable.mount_id);
    assert_eq!(failures[0].1.code(), "unsupported_connector");
}

#[test]
fn resolving_gmail_mount_uses_active_oauth_connection_credentials() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let (_connection_id, secret_ref) =
        save_gmail_connection(&mut store, "gmail-default", GMAIL_CONNECTOR_ID, "oauth");
    credentials
        .put(
            &secret_ref,
            &serde_json::to_string(&stored_gmail_credential("gmail-access-token"))
                .expect("credential json"),
        )
        .expect("save credential");

    let source =
        resolve_source_for_mount(&store, &credentials, &gmail_mount()).expect("resolve gmail");

    let ResolvedSource::Gmail(connector) = source else {
        panic!("expected gmail source");
    };
    assert_eq!(connector.config().access_token, "gmail-access-token");
}

#[test]
fn resolving_gmail_mount_with_invalid_settings_reports_validation_detail() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let (connection_id, secret_ref) =
        save_gmail_connection(&mut store, "gmail-default", GMAIL_CONNECTOR_ID, "oauth");
    credentials
        .put(
            &secret_ref,
            &serde_json::to_string(&stored_gmail_credential("gmail-access-token"))
                .expect("credential json"),
        )
        .expect("save credential");
    let mount = gmail_mount()
        .with_connection_id(connection_id)
        .with_settings_json("{");

    let error = resolve_source_for_mount(&store, &credentials, &mount)
        .expect_err("invalid Gmail settings should reject resolver");

    assert_eq!(error.code(), "credential_store_unavailable");
    let message = error.message();
    assert!(message.contains("Gmail mount `gmail-main` settings are invalid"));
    assert!(message.contains("Gmail mount settings JSON is invalid"));
}

#[test]
fn resolving_expired_gmail_credential_rejects_refresh_missing_required_scope() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let (connection_id, secret_ref) =
        save_gmail_connection(&mut store, "gmail-default", GMAIL_CONNECTOR_ID, "oauth");
    let refresh_scopes = GMAIL_OAUTH_SCOPES
        .iter()
        .filter(|scope| **scope != "https://www.googleapis.com/auth/gmail.compose")
        .collect::<Vec<_>>();
    let refresh_response = serde_json::json!({
        "access_token": "new-access-token",
        "token_type": "Bearer",
        "expires_in": 3600,
        "refresh_token_handle": "handle-2",
        "account_id": "acct-1",
        "account_label": "user@example.com",
        "workspace_id": "gmail",
        "workspace_name": "Gmail",
        "scopes": refresh_scopes,
    })
    .to_string();
    let (broker_url, broker) = spawn_refresh_broker("HTTP/1.1 200 OK", refresh_response);
    let stored = expired_gmail_credential("expired-access-token", broker_url);
    let original_secret = serde_json::to_string(&stored).expect("credential json");
    credentials
        .put(&secret_ref, &original_secret)
        .expect("save credential");
    let mount = gmail_mount().with_connection_id(connection_id);

    let error = resolve_source_for_mount(&store, &credentials, &mount)
        .expect_err("missing refreshed Gmail compose scope must be rejected");
    broker.join().expect("broker thread");

    assert_eq!(error.code(), "auth_required");
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
    assert_eq!(error.suggested_command(), Some("loc connect gmail"));
    assert_eq!(
        credentials.get(&secret_ref).expect("saved credential"),
        original_secret
    );
}

#[test]
fn resolving_expired_gmail_credential_rejects_refresh_full_mailbox_scope() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let (connection_id, secret_ref) =
        save_gmail_connection(&mut store, "gmail-default", GMAIL_CONNECTOR_ID, "oauth");
    let mut refresh_scopes = GMAIL_OAUTH_SCOPES
        .iter()
        .map(|scope| scope.to_string())
        .collect::<Vec<_>>();
    refresh_scopes.push("https://mail.google.com/".to_string());
    let refresh_response = serde_json::json!({
        "access_token": "new-access-token",
        "token_type": "Bearer",
        "expires_in": 3600,
        "refresh_token_handle": "handle-2",
        "account_id": "acct-1",
        "account_label": "user@example.com",
        "workspace_id": "gmail",
        "workspace_name": "Gmail",
        "scopes": refresh_scopes,
    })
    .to_string();
    let (broker_url, broker) = spawn_refresh_broker("HTTP/1.1 200 OK", refresh_response);
    let stored = expired_gmail_credential("expired-access-token", broker_url);
    let original_secret = serde_json::to_string(&stored).expect("credential json");
    credentials
        .put(&secret_ref, &original_secret)
        .expect("save credential");
    let mount = gmail_mount().with_connection_id(connection_id);

    let error = resolve_source_for_mount(&store, &credentials, &mount)
        .expect_err("full Gmail mailbox refresh scope must be rejected");
    broker.join().expect("broker thread");

    assert_eq!(error.code(), "auth_required");
    assert!(
        error
            .message()
            .contains("Gmail OAuth broker returned unsupported full mailbox scope")
    );
    assert!(error.message().contains("https://mail.google.com/"));
    assert_eq!(error.suggested_command(), Some("loc connect gmail"));
    assert_eq!(
        credentials.get(&secret_ref).expect("saved credential"),
        original_secret
    );
}

#[test]
fn resolving_explicit_gmail_connection_rejects_non_oauth_credentials() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let (connection_id, secret_ref) =
        save_gmail_connection(&mut store, "gmail-api-key", GMAIL_CONNECTOR_ID, "api_key");
    credentials
        .put(&secret_ref, "raw-secret-token")
        .expect("save credential");
    let mount = gmail_mount().with_connection_id(connection_id);

    let error =
        resolve_source_for_mount(&store, &credentials, &mount).expect_err("reject non-oauth");

    assert_eq!(error.code(), "auth_required");
    assert!(error.message().contains("OAuth"));
    assert_eq!(error.suggested_command(), Some("loc connect gmail"));
}

#[test]
fn resolving_explicit_gmail_connection_rejects_wrong_connector_record() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let (connection_id, secret_ref) = save_gmail_connection(
        &mut store,
        "google-docs-default",
        GOOGLE_DOCS_CONNECTOR_ID,
        "oauth",
    );
    credentials
        .put(
            &secret_ref,
            &serde_json::to_string(&stored_gmail_credential("wrong-connector-token"))
                .expect("credential json"),
        )
        .expect("save credential");
    let mount = gmail_mount().with_connection_id(connection_id);

    let error =
        resolve_source_for_mount(&store, &credentials, &mount).expect_err("reject connector");

    assert_eq!(error.code(), "unsupported_connector");
    assert!(error.message().contains(GOOGLE_DOCS_CONNECTOR_ID));
}

#[test]
fn resolving_implicit_gmail_mount_ignores_active_non_oauth_connections() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let (_raw_connection_id, raw_secret_ref) =
        save_gmail_connection(&mut store, "gmail-api-key", GMAIL_CONNECTOR_ID, "api_key");
    credentials
        .put(&raw_secret_ref, "raw-secret-token")
        .expect("save raw credential");
    let (_oauth_connection_id, oauth_secret_ref) =
        save_gmail_connection(&mut store, "gmail-oauth", GMAIL_CONNECTOR_ID, "oauth");
    credentials
        .put(
            &oauth_secret_ref,
            &serde_json::to_string(&stored_gmail_credential("oauth-token"))
                .expect("credential json"),
        )
        .expect("save oauth credential");

    let source =
        resolve_source_for_mount(&store, &credentials, &gmail_mount()).expect("resolve gmail");

    let ResolvedSource::Gmail(connector) = source else {
        panic!("expected gmail source");
    };
    assert_eq!(connector.config().access_token, "oauth-token");
}

#[test]
fn resolving_google_docs_mount_uses_active_connection_credentials() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let (connection_id, secret_ref) = save_google_docs_oauth_connection(&mut store);
    let stored = StoredGoogleDocsCredential::from_broker_token(
        OAuthBrokerToken {
            access_token: "access-token".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: Some(3600),
            refresh_token_handle: Some("handle-1".to_string()),
            account_id: Some("acct-1".to_string()),
            account_label: Some("user@example.com".to_string()),
            workspace_id: Some("google-drive".to_string()),
            workspace_name: Some("Google Drive".to_string()),
            scopes: vec!["https://www.googleapis.com/auth/documents".to_string()],
        },
        "client-id".to_string(),
        "https://auth.example.test".to_string(),
        4_102_444_800,
    );
    credentials
        .put(
            &secret_ref,
            &serde_json::to_string(&stored).expect("credential json"),
        )
        .expect("save credential");
    let mount = MountConfig::new(
        MountId::new("google-docs-main"),
        GOOGLE_DOCS_CONNECTOR_ID,
        "/tmp/locality/google-docs",
    )
    .with_remote_root_id(RemoteId::new("workspace-folder"))
    .with_connection_id(connection_id);

    let source =
        resolve_source_for_mount(&store, &credentials, &mount).expect("resolve google docs");

    assert_eq!(source.kind().0, GOOGLE_DOCS_CONNECTOR_ID);
    assert!(source.capabilities().supports_oauth);
}

#[test]
fn resolving_expired_google_docs_credential_refreshes_with_broker_handle() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let (connection_id, secret_ref) = save_google_docs_oauth_connection(&mut store);
    let refresh_response = serde_json::json!({
        "access_token": "new-access-token",
        "token_type": "Bearer",
        "expires_in": 3600,
        "refresh_token_handle": "handle-2",
        "account_id": "acct-1",
        "account_label": "user@example.com",
        "workspace_id": "google-drive",
        "workspace_name": "Google Drive",
        "scopes": [
            "openid",
            "email",
            "profile",
            "https://www.googleapis.com/auth/documents",
            "https://www.googleapis.com/auth/drive.file",
            "https://www.googleapis.com/auth/drive.metadata"
        ],
    })
    .to_string();
    let (broker_url, broker) = spawn_refresh_broker("HTTP/1.1 200 OK", refresh_response);

    let mut stored = StoredGoogleDocsCredential::from_broker_token(
        OAuthBrokerToken {
            access_token: "expired-access-token".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: Some(1),
            refresh_token_handle: Some("handle-1".to_string()),
            account_id: Some("acct-1".to_string()),
            account_label: Some("user@example.com".to_string()),
            workspace_id: Some("google-drive".to_string()),
            workspace_name: Some("Google Drive".to_string()),
            scopes: vec!["https://www.googleapis.com/auth/documents".to_string()],
        },
        "client-id".to_string(),
        broker_url,
        1,
    );
    stored.expires_at = Some(1);
    credentials
        .put(
            &secret_ref,
            &serde_json::to_string(&stored).expect("credential json"),
        )
        .expect("save credential");
    let mount = MountConfig::new(
        MountId::new("google-docs-main"),
        GOOGLE_DOCS_CONNECTOR_ID,
        "/tmp/locality/google-docs",
    )
    .with_remote_root_id(RemoteId::new("workspace-folder"))
    .with_connection_id(connection_id);

    let source = resolve_source_for_mount(&store, &credentials, &mount).expect("resolve source");
    broker.join().expect("broker thread");

    let ResolvedSource::GoogleDocs(connector) = source else {
        panic!("expected google docs source");
    };
    assert_eq!(connector.config().access_token, "new-access-token");
    let saved = credentials.get(&secret_ref).expect("saved credential");
    let saved =
        serde_json::from_str::<StoredGoogleDocsCredential>(&saved).expect("stored credential");
    assert_eq!(saved.access_token, "new-access-token");
    assert_eq!(saved.refresh_token_handle.as_deref(), Some("handle-2"));
}

#[test]
fn resolving_expired_google_docs_credential_with_stopped_local_broker_requires_reconnect() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let (connection_id, secret_ref) = save_google_docs_oauth_connection(&mut store);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
    let broker_url = format!("http://{}", listener.local_addr().expect("local addr"));
    drop(listener);

    let mut stored = StoredGoogleDocsCredential::from_broker_token(
        OAuthBrokerToken {
            access_token: "expired-access-token".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: Some(1),
            refresh_token_handle: Some("handle-1".to_string()),
            account_id: Some("acct-1".to_string()),
            account_label: Some("user@example.com".to_string()),
            workspace_id: Some("google-drive".to_string()),
            workspace_name: Some("Google Drive".to_string()),
            scopes: vec!["https://www.googleapis.com/auth/documents".to_string()],
        },
        "client-id".to_string(),
        broker_url.clone(),
        1,
    );
    stored.expires_at = Some(1);
    credentials
        .put(
            &secret_ref,
            &serde_json::to_string(&stored).expect("credential json"),
        )
        .expect("save credential");
    let mount = MountConfig::new(
        MountId::new("google-docs-main"),
        GOOGLE_DOCS_CONNECTOR_ID,
        "/tmp/locality/google-docs",
    )
    .with_remote_root_id(RemoteId::new("workspace-folder"))
    .with_connection_id(connection_id);

    let error =
        resolve_source_for_mount(&store, &credentials, &mount).expect_err("broker unavailable");

    assert_eq!(error.code(), "auth_required");
    assert!(error.message().contains("could not be refreshed"));
    assert!(error.message().contains(&broker_url));
    assert!(error.message().contains("default hosted broker"));
    assert_eq!(error.suggested_command(), Some("loc connect google-docs"));
}

#[test]
fn local_gmail_validator_allows_valid_direct_draft_create() {
    let issues = validate_gmail_create(
        "draft/foo.md",
        "---\nto: [\"user@example.com\"]\nsubject: Hello\n---\nBody\n",
    );

    assert!(issues.is_empty());
}

#[test]
fn local_gmail_validator_blocks_missing_and_empty_draft_recipients() {
    for markdown in [
        "---\nsubject: Hello\n---\nBody\n",
        "---\nto: []\nsubject: Hello\n---\nBody\n",
        "---\nto: \"\"\nsubject: Hello\n---\nBody\n",
    ] {
        let issues = validate_gmail_create("draft/foo.md", markdown);

        assert_eq!(issues, vec!["gmail_draft_missing_to"]);
    }
}

#[test]
fn local_gmail_validator_blocks_empty_subject_without_title() {
    let issues = validate_gmail_create(
        "draft/foo.md",
        "---\nto: [\"user@example.com\"]\nsubject: \"\"\n---\nBody\n",
    );

    assert_eq!(issues, vec!["gmail_draft_missing_subject"]);
}

#[test]
fn local_gmail_validator_allows_empty_subject_with_non_empty_title() {
    let issues = validate_gmail_create(
        "draft/foo.md",
        "---\ntitle: Fallback title\nto: [\"user@example.com\"]\nsubject: \"\"\n---\nBody\n",
    );

    assert!(issues.is_empty());
}

#[test]
fn local_gmail_validator_blocks_attachment_frontmatter_on_draft_create() {
    for (field, markdown) in [
        (
            "attachment",
            "---\nto: [\"user@example.com\"]\nsubject: Hello\nattachment: invoice.pdf\n---\nBody\n",
        ),
        (
            "attachments",
            "---\nto: [\"user@example.com\"]\nsubject: Hello\nattachments: [\"invoice.pdf\"]\n---\nBody\n",
        ),
        (
            "gmail.attachments",
            "---\nto: [\"user@example.com\"]\nsubject: Hello\ngmail:\n  attachments:\n    - filename: invoice.pdf\n---\nBody\n",
        ),
    ] {
        let issues = validate_gmail_create_issues("draft/foo.md", markdown);

        assert_eq!(issues.len(), 1, "{field}");
        assert_eq!(issues[0].code, "gmail_attachments_unsupported", "{field}");
        assert_eq!(
            issues[0].suggested_fix.as_deref(),
            Some("remove attachment frontmatter"),
            "{field}"
        );
    }
}

#[test]
fn local_gmail_validator_blocks_nested_draft_create() {
    let issues = validate_gmail_create(
        "draft/nested/foo.md",
        "---\nto: [\"user@example.com\"]\nsubject: Hello\n---\nBody\n",
    );

    assert_eq!(issues, vec!["gmail_create_outside_draft"]);
}

#[test]
fn local_gmail_validator_blocks_changed_inbox_and_sent_items() {
    for path in ["inbox/message.md", "sent/message.md"] {
        let issues = validate_gmail_changed(
            path,
            "---\nloc:\n  id: message-1\n  type: page\n  connector: gmail\nsubject: Hello\n---\nBody\n",
        );

        assert_eq!(issues, vec!["gmail_read_only_mailbox"]);
    }
}

#[test]
fn local_google_docs_validator_blocks_unsupported_directives() {
    let mount = MountConfig::new(
        MountId::new("google-docs-main"),
        GOOGLE_DOCS_CONNECTOR_ID,
        "/tmp/google-docs",
    )
    .with_remote_root_id(RemoteId::new("workspace-folder"));
    let parsed = parse_canonical_markdown(
        "---\nloc:\n  id: doc-1\n  type: page\n  connector: google-docs\ntitle: Launch Brief\n---\n::loc{id=doc-1:1:2 type=google_docs_unsupported kind=\"section_break\"}\n",
    )
    .expect("parse google docs markdown");

    let report = LocalSourceValidator
        .validate_changed_frontmatter(SourceValidationContext {
            state_root: None,
            mount: &mount,
            parent: None,
            relative_path: std::path::Path::new("launch-brief/page.md"),
            parsed: &parsed,
            shadow: None,
        })
        .expect("validate google docs");

    assert!(!report.is_clean());
    assert_eq!(
        report.issues[0].code,
        "google_docs_unsupported_document_structure"
    );
}

#[test]
fn local_google_docs_validator_blocks_when_unsupported_directive_was_deleted() {
    let mount = MountConfig::new(
        MountId::new("google-docs-main"),
        GOOGLE_DOCS_CONNECTOR_ID,
        "/tmp/google-docs",
    )
    .with_remote_root_id(RemoteId::new("workspace-folder"));
    let shadow = ShadowDocument::from_synced_body(
        RemoteId::new("doc-1"),
        "::loc{id=doc-1:1:2:unsupported type=google_docs_unsupported kind=\"inline_element\"}\n",
        1,
        Vec::<RemoteId>::new(),
    )
    .expect("shadow");
    let parsed = parse_canonical_markdown(
        "---\nloc:\n  id: doc-1\n  type: page\n  connector: google-docs\ntitle: Launch Brief\n---\nEdited content\n",
    )
    .expect("parse google docs markdown");

    let report = LocalSourceValidator
        .validate_changed_frontmatter(SourceValidationContext {
            state_root: None,
            mount: &mount,
            parent: None,
            relative_path: std::path::Path::new("launch-brief/page.md"),
            parsed: &parsed,
            shadow: Some(&shadow),
        })
        .expect("validate google docs");

    assert_eq!(
        report.issues[0].code,
        "google_docs_unsupported_document_structure"
    );
}

#[test]
fn local_google_docs_validator_blocks_markdown_table_edits() {
    let mount = MountConfig::new(
        MountId::new("google-docs-main"),
        GOOGLE_DOCS_CONNECTOR_ID,
        "/tmp/google-docs",
    )
    .with_remote_root_id(RemoteId::new("workspace-folder"));
    let mut shadow = ShadowDocument::from_synced_body(
        RemoteId::new("doc-1"),
        "| Key | Value |\n| --- | --- |\n| Owner | Locality |\n",
        1,
        vec![RemoteId::new("doc-1:1:40")],
    )
    .expect("shadow");
    shadow.blocks[0].native_kind = Some("google_docs_table".to_string());
    let parsed = parse_canonical_markdown(
        "---\nloc:\n  id: doc-1\n  type: page\n  connector: google-docs\ntitle: Launch Brief\n---\n| Key | Value |\n| --- | --- |\n| Owner | Edited |\n",
    )
    .expect("parse google docs markdown");

    let report = LocalSourceValidator
        .validate_changed_frontmatter(SourceValidationContext {
            state_root: None,
            mount: &mount,
            parent: None,
            relative_path: std::path::Path::new("launch-brief/page.md"),
            parsed: &parsed,
            shadow: Some(&shadow),
        })
        .expect("validate google docs");

    assert_eq!(report.issues[0].code, "google_docs_table_edit_unsupported");
}

#[test]
fn local_google_docs_validator_blocks_inline_image_block_edits() {
    let mount = MountConfig::new(
        MountId::new("google-docs-main"),
        GOOGLE_DOCS_CONNECTOR_ID,
        "/tmp/google-docs",
    )
    .with_remote_root_id(RemoteId::new("workspace-folder"));
    let mut shadow = ShadowDocument::from_synced_body(
        RemoteId::new("doc-1"),
        "![A circle with logo written in the center](https://example.test/circle.png)\n",
        1,
        [RemoteId::new("doc-1:2:4")],
    )
    .expect("shadow");
    shadow.blocks[0].native_kind = Some("google_docs_inline_object".to_string());
    let parsed = parse_canonical_markdown(
        "---\nloc:\n  id: doc-1\n  type: page\n  connector: google-docs\ntitle: Logo Doc\n---\nEdited circle\n",
    )
    .expect("parse google docs markdown");

    let report = LocalSourceValidator
        .validate_changed_frontmatter(SourceValidationContext {
            state_root: None,
            mount: &mount,
            parent: None,
            relative_path: std::path::Path::new("logo-doc/page.md"),
            parsed: &parsed,
            shadow: Some(&shadow),
        })
        .expect("validate google docs");

    assert_eq!(
        report.issues[0].code,
        "google_docs_inline_object_edit_unsupported"
    );
}
