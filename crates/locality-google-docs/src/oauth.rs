use std::sync::OnceLock;

use locality_connector::ConnectorCapabilities;
use locality_connector::oauth_broker::{
    OAuthBrokerCodeExchange, OAuthBrokerRefresh, OAuthBrokerStart, OAuthBrokerStartResponse,
    OAuthBrokerToken,
};
use locality_core::{LocalityError, LocalityResult};
use reqwest::blocking::Client;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const GOOGLE_DOCS_CONNECTOR_ID: &str = "google-docs";
// Cloudflare worker name is still `afs-oauth-broker`; the workers.dev hostname
// predates the Locality product rename until auth.locality.dev is deployed.
pub const DEFAULT_GOOGLE_DOCS_OAUTH_BROKER_URL: &str =
    "https://afs-oauth-broker.saurabh-b07.workers.dev";
pub const DEFAULT_GOOGLE_DOCS_OAUTH_REDIRECT_URI: &str =
    "http://localhost:8757/oauth/google-docs/callback";
pub const GOOGLE_DOCS_OAUTH_SCOPES: &[&str] = &[
    "openid",
    "email",
    "profile",
    "https://www.googleapis.com/auth/documents",
    "https://www.googleapis.com/auth/drive.file",
    "https://www.googleapis.com/auth/drive.metadata",
];

static REQWEST_CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredGoogleDocsCredential {
    pub kind: String,
    pub connector: String,
    pub access_token: String,
    pub token_type: Option<String>,
    pub oauth_client_id: Option<String>,
    pub oauth_broker_url: Option<String>,
    pub account_id: Option<String>,
    pub account_label: Option<String>,
    pub workspace_id: Option<String>,
    pub workspace_name: Option<String>,
    pub scopes: Vec<String>,
    pub refresh_token_handle: Option<String>,
    pub acquired_at: u64,
    pub expires_at: Option<u64>,
}

impl StoredGoogleDocsCredential {
    pub fn from_broker_token(
        token: OAuthBrokerToken,
        client_id: String,
        broker_url: String,
        acquired_at: u64,
    ) -> Self {
        let expires_at = token
            .expires_in
            .and_then(|expires_in| acquired_at.checked_add(expires_in));
        Self {
            kind: "oauth".to_string(),
            connector: GOOGLE_DOCS_CONNECTOR_ID.to_string(),
            access_token: token.access_token,
            token_type: token.token_type,
            oauth_client_id: Some(client_id),
            oauth_broker_url: Some(broker_url),
            account_id: token.account_id,
            account_label: token.account_label,
            workspace_id: token.workspace_id,
            workspace_name: token.workspace_name,
            scopes: token.scopes,
            refresh_token_handle: token.refresh_token_handle,
            acquired_at,
            expires_at,
        }
    }

    pub fn refreshed(&self, token: OAuthBrokerToken, acquired_at: u64) -> Self {
        let expires_at = token
            .expires_in
            .and_then(|expires_in| acquired_at.checked_add(expires_in));
        Self {
            kind: "oauth".to_string(),
            connector: GOOGLE_DOCS_CONNECTOR_ID.to_string(),
            access_token: token.access_token,
            token_type: token.token_type.or_else(|| self.token_type.clone()),
            oauth_client_id: self.oauth_client_id.clone(),
            oauth_broker_url: self.oauth_broker_url.clone(),
            account_id: token.account_id.or_else(|| self.account_id.clone()),
            account_label: token.account_label.or_else(|| self.account_label.clone()),
            workspace_id: token.workspace_id.or_else(|| self.workspace_id.clone()),
            workspace_name: token.workspace_name.or_else(|| self.workspace_name.clone()),
            scopes: if token.scopes.is_empty() {
                self.scopes.clone()
            } else {
                token.scopes
            },
            refresh_token_handle: token
                .refresh_token_handle
                .or_else(|| self.refresh_token_handle.clone()),
            acquired_at,
            expires_at,
        }
    }

    pub fn expires_soon(&self, now: u64) -> bool {
        self.expires_at
            .is_some_and(|expires_at| expires_at <= now.saturating_add(60))
    }
}

#[derive(Clone, Debug)]
pub struct HttpGoogleDocsOAuthBrokerClient {
    base_url: String,
    client: Client,
}

impl HttpGoogleDocsOAuthBrokerClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: google_docs_http_client(),
        }
    }

    pub fn start(&self, request: &OAuthBrokerStart) -> LocalityResult<OAuthBrokerStartResponse> {
        self.post_json("/v1/oauth/google-docs/start", request)
    }

    pub fn exchange_code(
        &self,
        request: &OAuthBrokerCodeExchange,
    ) -> LocalityResult<OAuthBrokerToken> {
        self.post_json("/v1/oauth/google-docs/exchange", request)
    }

    pub fn refresh_token(&self, request: &OAuthBrokerRefresh) -> LocalityResult<OAuthBrokerToken> {
        self.post_json("/v1/oauth/google-docs/refresh", request)
    }

    fn post_json<T, B>(&self, path: &str, body: &B) -> LocalityResult<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let response = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .json(body)
            .send()
            .map_err(|error| {
                LocalityError::Io(format!("google docs oauth broker request failed: {error}"))
            })?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .unwrap_or_else(|error| format!("<failed to read error body: {error}>"));
            return Err(LocalityError::Io(format!(
                "google docs oauth broker returned HTTP {status}: {body}"
            )));
        }
        response.json().map_err(|error| {
            LocalityError::Io(format!(
                "google docs oauth broker response decode failed: {error}"
            ))
        })
    }
}

pub fn google_docs_capabilities_json() -> Result<String, serde_json::Error> {
    let capabilities = ConnectorCapabilities {
        supports_block_updates: true,
        supports_databases: false,
        supports_oauth: true,
        supports_remote_observation: true,
        supports_lazy_child_enumeration: true,
        supports_media_download: false,
        supports_undo: false,
        supports_batch_observation: false,
    };
    serde_json::to_string(&capabilities)
}

fn google_docs_http_client() -> Client {
    ensure_reqwest_crypto_provider();
    Client::new()
}

fn ensure_reqwest_crypto_provider() {
    REQWEST_CRYPTO_PROVIDER.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[cfg(test)]
mod tests {
    use locality_connector::ConnectorCapabilities;
    use locality_connector::oauth_broker::OAuthBrokerToken;

    use super::{
        GOOGLE_DOCS_CONNECTOR_ID, GOOGLE_DOCS_OAUTH_SCOPES, StoredGoogleDocsCredential,
        google_docs_capabilities_json,
    };

    #[test]
    fn oauth_scopes_include_google_docs_and_workspace_metadata_access() {
        assert!(GOOGLE_DOCS_OAUTH_SCOPES.contains(&"https://www.googleapis.com/auth/documents"));
        assert!(!GOOGLE_DOCS_OAUTH_SCOPES.contains(&"https://www.googleapis.com/auth/drive"));
        assert!(
            GOOGLE_DOCS_OAUTH_SCOPES.contains(&"https://www.googleapis.com/auth/drive.metadata")
        );
        assert!(GOOGLE_DOCS_OAUTH_SCOPES.contains(&"https://www.googleapis.com/auth/drive.file"));
    }

    #[test]
    fn stored_capabilities_match_google_docs_connector_support() {
        let capabilities: ConnectorCapabilities =
            serde_json::from_str(&google_docs_capabilities_json().expect("capabilities json"))
                .expect("decode capabilities");

        assert!(capabilities.supports_block_updates);
        assert!(capabilities.supports_oauth);
        assert!(!capabilities.supports_databases);
        assert!(!capabilities.supports_undo);
    }

    #[test]
    fn broker_credential_stores_refresh_handle_without_refresh_token_or_secret() {
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
                scopes: vec!["openid".to_string()],
            },
            "client-id".to_string(),
            "https://auth.example.test".to_string(),
            100,
        );

        assert_eq!(stored.kind, "oauth");
        assert_eq!(stored.connector, GOOGLE_DOCS_CONNECTOR_ID);
        assert_eq!(stored.oauth_client_id.as_deref(), Some("client-id"));
        assert_eq!(
            stored.oauth_broker_url.as_deref(),
            Some("https://auth.example.test")
        );
        assert_eq!(stored.refresh_token_handle.as_deref(), Some("handle-1"));
        assert_eq!(stored.expires_at, Some(3700));

        let json = serde_json::to_string(&stored).expect("serialize stored credential");
        assert!(!json.contains("\"refresh_token\":"));
        assert!(!json.contains("client_secret"));
    }

    #[test]
    fn refreshed_broker_credential_rotates_access_token_and_handle() {
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
                scopes: vec!["openid".to_string()],
            },
            "client-id".to_string(),
            "https://auth.example.test".to_string(),
            100,
        );

        let refreshed = stored.refreshed(
            OAuthBrokerToken {
                access_token: "new-access-token".to_string(),
                token_type: Some("Bearer".to_string()),
                expires_in: Some(7200),
                refresh_token_handle: Some("handle-2".to_string()),
                account_id: None,
                account_label: None,
                workspace_id: None,
                workspace_name: None,
                scopes: vec![],
            },
            200,
        );

        assert_eq!(refreshed.access_token, "new-access-token");
        assert_eq!(refreshed.refresh_token_handle.as_deref(), Some("handle-2"));
        assert_eq!(refreshed.account_label.as_deref(), Some("user@example.com"));
        assert_eq!(refreshed.scopes, vec!["openid".to_string()]);
        assert_eq!(refreshed.expires_at, Some(7400));
    }
}
