#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use locality_core::model::{MountId, RemoteId};
use locality_gmail::{
    GMAIL_CONNECTOR_ID, GMAIL_OAUTH_SCOPES, GmailConnector, StoredGmailCredential,
    gmail_capabilities_json,
};
use locality_google_docs::{
    GOOGLE_DOCS_CONNECTOR_ID, GOOGLE_DOCS_OAUTH_SCOPES, GoogleDocsConnector,
    StoredGoogleDocsCredential, google_docs_capabilities_json,
};
use locality_store::{
    ConnectionId, ConnectionRecord, ConnectionRepository, ConnectorProfileId,
    ConnectorProfileRecord, ConnectorProfileRepository, CredentialStore, FileCredentialStore,
    MountConfig, MountRepository, ProjectionMode, SqliteStateStore,
};
use localityd::source::{ResolvedSource, resolve_source_for_mount};

const LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON: &str = "LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON";
const LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON: &str = "LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON";
const LOCALITY_GMAIL_LIVE_TEST_RECIPIENT: &str = "LOCALITY_GMAIL_LIVE_TEST_RECIPIENT";
const LOCALITY_GOOGLE_LIVE_FORCE_REFRESH: &str = "LOCALITY_GOOGLE_LIVE_FORCE_REFRESH";
const KEEP_TMP_ENV: &str = "LOCALITY_GOOGLE_LIVE_KEEP_TMP";
const EXPIRED_SENTINEL_ACCESS_TOKEN: &str = "locality-live-expired-access-token";

#[derive(Clone, Copy)]
enum GoogleLiveConnector {
    GoogleDocs,
    Gmail,
}

struct LiveFixture {
    state_root: PathBuf,
    mount_root: PathBuf,
}

impl LiveFixture {
    fn new(prefix: &str) -> Self {
        let suffix = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_millis()
        );
        let root = std::env::temp_dir().join(format!("{prefix}-{suffix}"));
        let state_root = root.join("state");
        let mount_root = root.join("Locality");
        fs::create_dir_all(&state_root).expect("create state root");
        fs::create_dir_all(&mount_root).expect("create mount root");
        Self {
            state_root,
            mount_root,
        }
    }
}

impl Drop for LiveFixture {
    fn drop(&mut self) {
        if std::env::var(KEEP_TMP_ENV).ok().as_deref() != Some("1") {
            let _ = fs::remove_dir_all(self.state_root.parent().expect("fixture root parent"));
        }
    }
}

fn seed_live_connection(
    state_root: &Path,
    connector: GoogleLiveConnector,
    connection_id: &ConnectionId,
) -> String {
    let mut secret = match connector {
        GoogleLiveConnector::GoogleDocs => stored_google_docs_secret(),
        GoogleLiveConnector::Gmail => stored_gmail_secret(),
    };
    if std::env::var(LOCALITY_GOOGLE_LIVE_FORCE_REFRESH)
        .ok()
        .as_deref()
        == Some("1")
    {
        secret = force_expired_secret(connector, &secret);
    }

    let secret_ref = format!("connection:{}", connection_id.as_str());
    FileCredentialStore::new(state_root)
        .put(&secret_ref, &secret)
        .expect("save live credential");

    let mut store = SqliteStateStore::open(state_root.to_path_buf()).expect("open state store");
    let profile_id = ConnectorProfileId::new(format!("{}-profile", connection_id.as_str()));
    let timestamp = timestamp_string();
    let connector_id = connector_id(connector);
    let scopes = connector_scopes(connector);
    let capabilities_json = connector_capabilities_json(connector);

    store
        .save_connector_profile(ConnectorProfileRecord {
            profile_id: profile_id.clone(),
            connector: connector_id.to_string(),
            display_name: connector_display_name(connector).to_string(),
            auth_kind: "oauth".to_string(),
            scopes: scopes.clone(),
            capabilities_json: capabilities_json.clone(),
            enabled_actions_json: connector_enabled_actions_json(connector).to_string(),
            connector_version: connector_version(connector).to_string(),
            status: "active".to_string(),
            created_at: timestamp.clone(),
            updated_at: timestamp.clone(),
        })
        .expect("save connector profile");
    store
        .save_connection(ConnectionRecord {
            connection_id: connection_id.clone(),
            profile_id: Some(profile_id),
            connector: connector_id.to_string(),
            display_name: connector_display_name(connector).to_string(),
            account_label: live_account_label(connector, &secret),
            workspace_id: live_workspace_id(connector, &secret),
            workspace_name: live_workspace_name(connector, &secret),
            auth_kind: "oauth".to_string(),
            secret_ref: secret_ref.clone(),
            scopes,
            capabilities_json,
            status: "active".to_string(),
            created_at: timestamp.clone(),
            updated_at: timestamp,
            expires_at: credential_expires_at(connector, &secret).map(|expires_at| {
                expires_at
                    .duration_since(UNIX_EPOCH)
                    .expect("credential expires after epoch")
                    .as_secs()
                    .to_string()
            }),
        })
        .expect("save connection");

    secret_ref
}

fn stored_google_docs_secret() -> String {
    let secret = required_env(LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON);
    let stored =
        serde_json::from_str::<StoredGoogleDocsCredential>(&secret).expect("Google Docs secret");
    assert_eq!(stored.connector, GOOGLE_DOCS_CONNECTOR_ID);
    assert_eq!(stored.kind, "oauth");
    assert_broker_url(stored.oauth_broker_url.as_deref(), "Google Docs");
    assert_refresh_handle(stored.refresh_token_handle.as_deref());
    secret
}

fn stored_gmail_secret() -> String {
    let secret = required_env(LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON);
    let stored = serde_json::from_str::<StoredGmailCredential>(&secret).expect("Gmail secret");
    assert_eq!(stored.connector, GMAIL_CONNECTOR_ID);
    assert_eq!(stored.kind, "oauth");
    assert_broker_url(stored.oauth_broker_url.as_deref(), "Gmail");
    assert_refresh_handle(stored.refresh_token_handle.as_deref());
    secret
}

fn force_expired_secret(connector: GoogleLiveConnector, secret: &str) -> String {
    match connector {
        GoogleLiveConnector::GoogleDocs => {
            let mut stored =
                serde_json::from_str::<StoredGoogleDocsCredential>(secret).expect("Google Docs");
            assert_eq!(stored.connector, GOOGLE_DOCS_CONNECTOR_ID);
            assert_eq!(stored.kind, "oauth");
            assert_broker_url(stored.oauth_broker_url.as_deref(), "Google Docs");
            assert_refresh_handle(stored.refresh_token_handle.as_deref());
            stored.access_token = EXPIRED_SENTINEL_ACCESS_TOKEN.to_string();
            stored.acquired_at = 1;
            stored.expires_at = Some(1);
            serde_json::to_string(&stored).expect("serialize expired Google Docs credential")
        }
        GoogleLiveConnector::Gmail => {
            let mut stored = serde_json::from_str::<StoredGmailCredential>(secret).expect("Gmail");
            assert_eq!(stored.connector, GMAIL_CONNECTOR_ID);
            assert_eq!(stored.kind, "oauth");
            assert_broker_url(stored.oauth_broker_url.as_deref(), "Gmail");
            assert_refresh_handle(stored.refresh_token_handle.as_deref());
            stored.access_token = EXPIRED_SENTINEL_ACCESS_TOKEN.to_string();
            stored.acquired_at = 1;
            stored.expires_at = Some(1);
            serde_json::to_string(&stored).expect("serialize expired Gmail credential")
        }
    }
}

fn live_account_label(connector: GoogleLiveConnector, secret: &str) -> Option<String> {
    match connector {
        GoogleLiveConnector::GoogleDocs => {
            serde_json::from_str::<StoredGoogleDocsCredential>(secret)
                .expect("Google Docs credential")
                .account_label
        }
        GoogleLiveConnector::Gmail => {
            serde_json::from_str::<StoredGmailCredential>(secret)
                .expect("Gmail credential")
                .account_label
        }
    }
}

fn live_workspace_id(connector: GoogleLiveConnector, secret: &str) -> Option<String> {
    match connector {
        GoogleLiveConnector::GoogleDocs => {
            serde_json::from_str::<StoredGoogleDocsCredential>(secret)
                .expect("Google Docs credential")
                .workspace_id
        }
        GoogleLiveConnector::Gmail => {
            serde_json::from_str::<StoredGmailCredential>(secret)
                .expect("Gmail credential")
                .workspace_id
        }
    }
}

fn live_workspace_name(connector: GoogleLiveConnector, secret: &str) -> Option<String> {
    match connector {
        GoogleLiveConnector::GoogleDocs => {
            serde_json::from_str::<StoredGoogleDocsCredential>(secret)
                .expect("Google Docs credential")
                .workspace_name
        }
        GoogleLiveConnector::Gmail => {
            serde_json::from_str::<StoredGmailCredential>(secret)
                .expect("Gmail credential")
                .workspace_name
        }
    }
}

fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name} to run live Google connector tests"))
}

fn timestamp_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs()
        .to_string()
}

fn resolve_google_docs_from_store(
    state_root: &Path,
    connection_id: ConnectionId,
    workspace_folder_id: RemoteId,
) -> GoogleDocsConnector {
    let mut store = SqliteStateStore::open(state_root.to_path_buf()).expect("open state store");
    let credentials = FileCredentialStore::new(state_root);
    let secret_ref = format!("connection:{}", connection_id.as_str());
    let mount = MountConfig::new(
        MountId::new("google-docs-main"),
        GOOGLE_DOCS_CONNECTOR_ID,
        mount_root_for_state_root(state_root).join("google-docs-main"),
    )
    .with_remote_root_id(workspace_folder_id.clone())
    .with_connection_id(connection_id.clone())
    .projection(ProjectionMode::PlainFiles);
    store
        .save_mount(mount.clone())
        .expect("save Google Docs mount");

    let source =
        resolve_source_for_mount(&store, &credentials, &mount).expect("resolve Google Docs");
    let ResolvedSource::GoogleDocs(connector) = source else {
        panic!("expected Google Docs connector");
    };
    assert_eq!(
        connector.config().workspace_folder_id.as_ref(),
        Some(&workspace_folder_id)
    );
    assert_ne!(
        connector.config().access_token,
        EXPIRED_SENTINEL_ACCESS_TOKEN,
        "Google Docs token was not refreshed"
    );
    assert_forced_refresh_persisted(&credentials, GoogleLiveConnector::GoogleDocs, &secret_ref);
    connector
}

fn resolve_gmail_from_store(state_root: &Path, connection_id: ConnectionId) -> GmailConnector {
    let mut store = SqliteStateStore::open(state_root.to_path_buf()).expect("open state store");
    let credentials = FileCredentialStore::new(state_root);
    let secret_ref = format!("connection:{}", connection_id.as_str());
    let mount = MountConfig::new(
        MountId::new("gmail-main"),
        GMAIL_CONNECTOR_ID,
        mount_root_for_state_root(state_root).join("gmail-main"),
    )
    .with_connection_id(connection_id.clone())
    .projection(ProjectionMode::PlainFiles);
    store.save_mount(mount.clone()).expect("save Gmail mount");

    let source = resolve_source_for_mount(&store, &credentials, &mount).expect("resolve Gmail");
    let ResolvedSource::Gmail(connector) = source else {
        panic!("expected Gmail connector");
    };
    assert_ne!(
        connector.config().access_token,
        EXPIRED_SENTINEL_ACCESS_TOKEN,
        "Gmail token was not refreshed"
    );
    assert_forced_refresh_persisted(&credentials, GoogleLiveConnector::Gmail, &secret_ref);
    connector
}

fn assert_broker_url(oauth_broker_url: Option<&str>, connector_name: &str) {
    let oauth_broker_url = oauth_broker_url.unwrap_or_else(|| {
        panic!("{connector_name} credential must include oauth_broker_url");
    });
    assert!(
        !oauth_broker_url.trim().is_empty(),
        "{connector_name} credential oauth_broker_url cannot be empty"
    );
}

fn assert_refresh_handle(refresh_token_handle: Option<&str>) {
    let refresh_token_handle = refresh_token_handle.expect("refresh token handle");
    assert!(
        refresh_token_handle.starts_with("locrh_v1."),
        "refresh token handle must be a locrh_v1 handle"
    );
}

fn assert_forced_refresh_persisted(
    credentials: &dyn CredentialStore,
    connector: GoogleLiveConnector,
    secret_ref: &str,
) {
    if std::env::var(LOCALITY_GOOGLE_LIVE_FORCE_REFRESH)
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }

    let secret = credentials
        .get(secret_ref)
        .expect("read refreshed live credential");
    match connector {
        GoogleLiveConnector::GoogleDocs => {
            let stored = serde_json::from_str::<StoredGoogleDocsCredential>(&secret)
                .expect("refreshed Google Docs credential");
            assert_ne!(stored.access_token, EXPIRED_SENTINEL_ACCESS_TOKEN);
            assert_ne!(stored.acquired_at, 1);
            assert_ne!(stored.expires_at, Some(1));
            assert_refresh_handle(stored.refresh_token_handle.as_deref());
        }
        GoogleLiveConnector::Gmail => {
            let stored = serde_json::from_str::<StoredGmailCredential>(&secret)
                .expect("refreshed Gmail credential");
            assert_ne!(stored.access_token, EXPIRED_SENTINEL_ACCESS_TOKEN);
            assert_ne!(stored.acquired_at, 1);
            assert_ne!(stored.expires_at, Some(1));
            assert_refresh_handle(stored.refresh_token_handle.as_deref());
        }
    }
}

fn connector_id(connector: GoogleLiveConnector) -> &'static str {
    match connector {
        GoogleLiveConnector::GoogleDocs => GOOGLE_DOCS_CONNECTOR_ID,
        GoogleLiveConnector::Gmail => GMAIL_CONNECTOR_ID,
    }
}

fn connector_display_name(connector: GoogleLiveConnector) -> &'static str {
    match connector {
        GoogleLiveConnector::GoogleDocs => "Google Docs",
        GoogleLiveConnector::Gmail => "Gmail",
    }
}

fn connector_version(connector: GoogleLiveConnector) -> &'static str {
    match connector {
        GoogleLiveConnector::GoogleDocs => "google-docs.v1",
        GoogleLiveConnector::Gmail => "gmail.v1",
    }
}

fn connector_enabled_actions_json(connector: GoogleLiveConnector) -> &'static str {
    match connector {
        GoogleLiveConnector::GoogleDocs => "[\"read\",\"write\"]",
        GoogleLiveConnector::Gmail => "[\"read\",\"send\"]",
    }
}

fn connector_scopes(connector: GoogleLiveConnector) -> Vec<String> {
    match connector {
        GoogleLiveConnector::GoogleDocs => GOOGLE_DOCS_OAUTH_SCOPES,
        GoogleLiveConnector::Gmail => GMAIL_OAUTH_SCOPES,
    }
    .iter()
    .map(|scope| scope.to_string())
    .collect()
}

fn connector_capabilities_json(connector: GoogleLiveConnector) -> String {
    match connector {
        GoogleLiveConnector::GoogleDocs => {
            google_docs_capabilities_json().expect("Google Docs capabilities")
        }
        GoogleLiveConnector::Gmail => gmail_capabilities_json().expect("Gmail capabilities"),
    }
}

fn credential_expires_at(connector: GoogleLiveConnector, secret: &str) -> Option<SystemTime> {
    let expires_at = match connector {
        GoogleLiveConnector::GoogleDocs => {
            serde_json::from_str::<StoredGoogleDocsCredential>(secret)
                .expect("Google Docs credential")
                .expires_at
        }
        GoogleLiveConnector::Gmail => {
            serde_json::from_str::<StoredGmailCredential>(secret)
                .expect("Gmail credential")
                .expires_at
        }
    }?;
    UNIX_EPOCH.checked_add(std::time::Duration::from_secs(expires_at))
}

fn mount_root_for_state_root(state_root: &Path) -> PathBuf {
    state_root
        .parent()
        .map(|root| root.join("Locality"))
        .unwrap_or_else(|| state_root.join("Locality"))
}
