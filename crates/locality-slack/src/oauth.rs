use std::collections::BTreeSet;
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

pub const SLACK_CONNECTOR_ID: &str = "slack";
pub const DEFAULT_SLACK_OAUTH_BROKER_URL: &str = "https://afs-oauth-broker.saurabh-b07.workers.dev";
pub const DEFAULT_SLACK_OAUTH_REDIRECT_URI: &str = "http://localhost:8757/oauth/slack/callback";
pub const SLACK_OAUTH_SCOPES: &[&str] = &["channels:read", "channels:history", "users:read"];

static REQWEST_CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredSlackCredential {
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

impl fmt::Debug for StoredSlackCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredSlackCredential")
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

impl StoredSlackCredential {
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
            connector: SLACK_CONNECTOR_ID.to_string(),
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

    pub fn refreshed(
        &self,
        token: OAuthBrokerToken,
        acquired_at: u64,
    ) -> Result<Self, SlackOAuthScopeError> {
        let expires_at = token
            .expires_in
            .and_then(|expires_in| acquired_at.checked_add(expires_in));
        let scopes = if token.scopes.is_empty() {
            self.scopes.clone()
        } else {
            validate_slack_oauth_scopes(&token.scopes)?;
            token.scopes
        };
        Ok(Self {
            kind: "oauth".to_string(),
            connector: SLACK_CONNECTOR_ID.to_string(),
            access_token: token.access_token,
            token_type: token.token_type.or_else(|| self.token_type.clone()),
            oauth_client_id: self.oauth_client_id.clone(),
            oauth_broker_url: self.oauth_broker_url.clone(),
            account_id: token.account_id.or_else(|| self.account_id.clone()),
            account_label: token.account_label.or_else(|| self.account_label.clone()),
            workspace_id: token.workspace_id.or_else(|| self.workspace_id.clone()),
            workspace_name: token.workspace_name.or_else(|| self.workspace_name.clone()),
            scopes,
            refresh_token_handle: token
                .refresh_token_handle
                .or_else(|| self.refresh_token_handle.clone()),
            acquired_at,
            expires_at,
        })
    }

    pub fn expires_soon(&self, now: u64) -> bool {
        self.expires_at
            .is_some_and(|expires_at| expires_at <= now.saturating_add(60))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlackOAuthScopeError {
    MissingRequiredScope(&'static str),
}

impl fmt::Display for SlackOAuthScopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRequiredScope(scope) => write!(
                f,
                "Slack OAuth broker response missing required Slack OAuth scope `{scope}`; reconnect with the default Slack OAuth broker configuration"
            ),
        }
    }
}

impl std::error::Error for SlackOAuthScopeError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlackOAuthCredentialError {
    MissingRefreshHandleForExpiringToken,
}

impl fmt::Display for SlackOAuthCredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRefreshHandleForExpiringToken => write!(
                f,
                "expiring Slack OAuth broker response did not include refresh_token_handle; reconnect with a broker configured for refresh handle mode"
            ),
        }
    }
}

impl std::error::Error for SlackOAuthCredentialError {}

pub fn validate_slack_oauth_scopes(scopes: &[String]) -> Result<(), SlackOAuthScopeError> {
    let granted = scopes.iter().map(String::as_str).collect::<BTreeSet<_>>();
    for required in SLACK_OAUTH_SCOPES {
        if !granted.contains(required) {
            return Err(SlackOAuthScopeError::MissingRequiredScope(required));
        }
    }
    Ok(())
}

pub fn validate_slack_oauth_refresh_path(
    token: &OAuthBrokerToken,
) -> Result<(), SlackOAuthCredentialError> {
    if token.expires_in.is_some() && token.refresh_token_handle.is_none() {
        return Err(SlackOAuthCredentialError::MissingRefreshHandleForExpiringToken);
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct HttpSlackOAuthBrokerClient {
    base_url: String,
    client: Client,
}

impl HttpSlackOAuthBrokerClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: slack_http_client(),
        }
    }

    pub fn start(&self, request: &OAuthBrokerStart) -> LocalityResult<OAuthBrokerStartResponse> {
        self.post_json("/v1/oauth/slack/start", request)
    }

    pub fn exchange_code(
        &self,
        request: &OAuthBrokerCodeExchange,
    ) -> LocalityResult<OAuthBrokerToken> {
        self.post_json("/v1/oauth/slack/exchange", request)
    }

    pub fn refresh_token(&self, request: &OAuthBrokerRefresh) -> LocalityResult<OAuthBrokerToken> {
        self.post_json("/v1/oauth/slack/refresh", request)
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
                LocalityError::Io(format!("slack oauth broker request failed: {error}"))
            })?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .unwrap_or_else(|error| format!("<failed to read error body: {error}>"));
            return Err(LocalityError::Io(format!(
                "slack oauth broker returned HTTP {status}: {body}"
            )));
        }
        response.json().map_err(|error| {
            LocalityError::Io(format!(
                "slack oauth broker response decode failed: {error}"
            ))
        })
    }
}

pub fn slack_capabilities_json() -> Result<String, serde_json::Error> {
    serde_json::to_string(&ConnectorCapabilities {
        supports_oauth: true,
        ..ConnectorCapabilities::read_only()
    })
}

fn slack_http_client() -> Client {
    ensure_reqwest_crypto_provider();
    Client::new()
}

fn ensure_reqwest_crypto_provider() {
    REQWEST_CRYPTO_PROVIDER.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
