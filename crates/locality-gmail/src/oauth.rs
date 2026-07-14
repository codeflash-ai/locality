use std::fmt;
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

pub const GMAIL_CONNECTOR_ID: &str = "gmail";
pub const DEFAULT_GMAIL_OAUTH_BROKER_URL: &str = "https://afs-oauth-broker.saurabh-b07.workers.dev";
pub const DEFAULT_GMAIL_OAUTH_REDIRECT_URI: &str = "http://localhost:8757/oauth/gmail/callback";
pub const GMAIL_OAUTH_SCOPES: &[&str] = &[
    "openid",
    "email",
    "profile",
    "https://www.googleapis.com/auth/gmail.readonly",
    "https://www.googleapis.com/auth/gmail.compose",
];

static REQWEST_CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredGmailCredential {
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

impl fmt::Debug for StoredGmailCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredGmailCredential")
            .field("kind", &self.kind)
            .field("connector", &self.connector)
            .field("access_token", &"<redacted>")
            .field("token_type", &self.token_type)
            .field("oauth_client_id", &self.oauth_client_id)
            .field("oauth_broker_url", &self.oauth_broker_url)
            .field("account_id", &self.account_id)
            .field("account_label", &self.account_label)
            .field("workspace_id", &self.workspace_id)
            .field("workspace_name", &self.workspace_name)
            .field("scopes", &self.scopes)
            .field(
                "refresh_token_handle",
                &self.refresh_token_handle.as_ref().map(|_| "<redacted>"),
            )
            .field("acquired_at", &self.acquired_at)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl StoredGmailCredential {
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
            connector: GMAIL_CONNECTOR_ID.to_string(),
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
            connector: GMAIL_CONNECTOR_ID.to_string(),
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
pub struct HttpGmailOAuthBrokerClient {
    base_url: String,
    client: Client,
}

impl HttpGmailOAuthBrokerClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: gmail_http_client(),
        }
    }

    pub fn start(&self, request: &OAuthBrokerStart) -> LocalityResult<OAuthBrokerStartResponse> {
        self.post_json("/v1/oauth/gmail/start", request)
    }

    pub fn exchange_code(
        &self,
        request: &OAuthBrokerCodeExchange,
    ) -> LocalityResult<OAuthBrokerToken> {
        self.post_json("/v1/oauth/gmail/exchange", request)
    }

    pub fn refresh_token(&self, request: &OAuthBrokerRefresh) -> LocalityResult<OAuthBrokerToken> {
        self.post_json("/v1/oauth/gmail/refresh", request)
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
                LocalityError::Io(format!("gmail oauth broker request failed: {error}"))
            })?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .unwrap_or_else(|error| format!("<failed to read error body: {error}>"));
            return Err(LocalityError::Io(format!(
                "gmail oauth broker returned HTTP {status}: {body}"
            )));
        }
        response.json().map_err(|error| {
            LocalityError::Io(format!(
                "gmail oauth broker response decode failed: {error}"
            ))
        })
    }
}

pub fn gmail_capabilities_json() -> Result<String, serde_json::Error> {
    let capabilities = ConnectorCapabilities {
        supports_block_updates: false,
        supports_databases: false,
        supports_oauth: true,
        supports_remote_observation: false,
        supports_lazy_child_enumeration: false,
        supports_media_download: false,
        supports_undo: false,
        supports_batch_observation: false,
    };
    serde_json::to_string(&capabilities)
}

fn gmail_http_client() -> Client {
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
        GMAIL_CONNECTOR_ID, GMAIL_OAUTH_SCOPES, StoredGmailCredential, gmail_capabilities_json,
    };

    #[test]
    fn oauth_scopes_cover_read_and_compose_without_full_mailbox_scope() {
        assert!(GMAIL_OAUTH_SCOPES.contains(&"openid"));
        assert!(GMAIL_OAUTH_SCOPES.contains(&"email"));
        assert!(GMAIL_OAUTH_SCOPES.contains(&"profile"));
        assert!(GMAIL_OAUTH_SCOPES.contains(&"https://www.googleapis.com/auth/gmail.readonly"));
        assert!(GMAIL_OAUTH_SCOPES.contains(&"https://www.googleapis.com/auth/gmail.compose"));
        assert!(!GMAIL_OAUTH_SCOPES.contains(&"https://mail.google.com/"));
    }

    #[test]
    fn stored_capabilities_match_gmail_v1() {
        let capabilities: ConnectorCapabilities =
            serde_json::from_str(&gmail_capabilities_json().expect("capabilities json"))
                .expect("decode capabilities");

        assert!(capabilities.supports_oauth);
        assert!(!capabilities.supports_remote_observation);
        assert!(!capabilities.supports_lazy_child_enumeration);
        assert!(!capabilities.supports_databases);
        assert!(!capabilities.supports_media_download);
        assert!(!capabilities.supports_block_updates);
        assert!(!capabilities.supports_undo);
        assert!(!capabilities.supports_batch_observation);
    }

    #[test]
    fn broker_credential_stores_refresh_handle_without_secret() {
        let stored = StoredGmailCredential::from_broker_token(
            OAuthBrokerToken {
                access_token: "access-token".to_string(),
                token_type: Some("Bearer".to_string()),
                expires_in: Some(3600),
                refresh_token_handle: Some("handle-1".to_string()),
                account_id: Some("acct-1".to_string()),
                account_label: Some("me@example.com".to_string()),
                workspace_id: Some("gmail".to_string()),
                workspace_name: Some("Gmail".to_string()),
                scopes: vec!["openid".to_string()],
            },
            "client-id".to_string(),
            "https://auth.example.test".to_string(),
            100,
        );

        assert_eq!(stored.kind, "oauth");
        assert_eq!(stored.connector, GMAIL_CONNECTOR_ID);
        assert_eq!(stored.refresh_token_handle.as_deref(), Some("handle-1"));
        assert_eq!(stored.expires_at, Some(3700));
        let debug = format!("{stored:?}");
        assert!(!debug.contains("access-token"));
        assert!(!debug.contains("handle-1"));
        assert!(debug.contains("<redacted>"));
        let json = serde_json::to_string(&stored).expect("serialize");
        assert!(!json.contains("\"refresh_token\":"));
        assert!(!json.contains("client_secret"));
    }
}
