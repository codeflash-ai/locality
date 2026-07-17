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

use crate::connector::SLACK_CONNECTOR_ID;

pub const DEFAULT_SLACK_OAUTH_BROKER_URL: &str = "https://afs-oauth-broker.saurabh-b07.workers.dev";
pub const DEFAULT_SLACK_OAUTH_REDIRECT_URI: &str = "http://localhost:8757/oauth/slack/callback";

pub const SLACK_AUTO_JOIN_PUBLIC_CHANNELS_SCOPE: &str = "channels:join";

pub const SLACK_OAUTH_SCOPES: &[&str] = &[
    "channels:read",
    "channels:history",
    "groups:read",
    "groups:history",
    "im:read",
    "im:history",
    "mpim:read",
    "mpim:history",
    "users:read",
    "team:read",
    "files:read",
];

pub const SLACK_OPTIONAL_OAUTH_SCOPES: &[&str] = &[SLACK_AUTO_JOIN_PUBLIC_CHANNELS_SCOPE];

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
    ) -> Result<Self, SlackOAuthScopeError> {
        validate_slack_oauth_scopes(&token.scopes)?;
        let expires_at = token
            .expires_in
            .and_then(|expires_in| acquired_at.checked_add(expires_in));
        Ok(Self {
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
        })
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
            validate_refreshed_slack_oauth_scopes(&self.scopes, &token.scopes)?;
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
    MissingPreviouslyGrantedScope(String),
    UnsupportedScope(String),
}

impl fmt::Display for SlackOAuthScopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRequiredScope(scope) => write!(
                f,
                "Slack OAuth broker response missing required Slack OAuth scope `{scope}`; reconnect with the default Slack OAuth broker configuration"
            ),
            Self::MissingPreviouslyGrantedScope(scope) => write!(
                f,
                "Slack OAuth broker refresh response missing previously granted Slack OAuth scope `{scope}`; reconnect with `loc connect slack --auto-join-public-channels` if this mount uses public channel auto-join"
            ),
            Self::UnsupportedScope(scope) => write!(
                f,
                "Slack OAuth broker returned unsupported Slack OAuth scope `{scope}` for read-only Slack v1"
            ),
        }
    }
}

impl std::error::Error for SlackOAuthScopeError {}

pub fn validate_slack_oauth_scopes(scopes: &[String]) -> Result<(), SlackOAuthScopeError> {
    let allowed = SLACK_OAUTH_SCOPES
        .iter()
        .chain(SLACK_OPTIONAL_OAUTH_SCOPES.iter())
        .copied()
        .collect::<BTreeSet<_>>();
    for scope in scopes {
        if !allowed.contains(scope.as_str()) {
            return Err(SlackOAuthScopeError::UnsupportedScope(scope.clone()));
        }
    }

    let granted = scopes.iter().map(String::as_str).collect::<BTreeSet<_>>();
    for required in SLACK_OAUTH_SCOPES {
        if !granted.contains(required) {
            return Err(SlackOAuthScopeError::MissingRequiredScope(required));
        }
    }

    Ok(())
}

fn validate_refreshed_slack_oauth_scopes(
    previous_scopes: &[String],
    refreshed_scopes: &[String],
) -> Result<(), SlackOAuthScopeError> {
    let refreshed = refreshed_scopes
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    for previous in previous_scopes {
        if !refreshed.contains(previous.as_str()) {
            return Err(SlackOAuthScopeError::MissingPreviouslyGrantedScope(
                previous.clone(),
            ));
        }
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
            return Err(LocalityError::Io(format!(
                "slack oauth broker returned HTTP {status}"
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
    let capabilities = ConnectorCapabilities {
        supports_block_updates: false,
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

fn slack_http_client() -> Client {
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
    use super::*;
    use locality_connector::ConnectorCapabilities;
    use locality_connector::oauth_broker::OAuthBrokerToken;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn slack_scopes() -> Vec<String> {
        SLACK_OAUTH_SCOPES
            .iter()
            .map(|scope| scope.to_string())
            .collect()
    }

    fn broker_token(scopes: Vec<String>) -> OAuthBrokerToken {
        OAuthBrokerToken {
            access_token: "xoxb-access".to_string(),
            token_type: Some("bot".to_string()),
            expires_in: None,
            refresh_token_handle: Some("opaque-refresh-handle".to_string()),
            account_id: Some("T123".to_string()),
            account_label: Some("Locality".to_string()),
            workspace_id: Some("T123".to_string()),
            workspace_name: Some("Locality".to_string()),
            scopes,
        }
    }

    #[test]
    fn validates_required_slack_scopes() {
        validate_slack_oauth_scopes(&slack_scopes()).expect("valid scopes");
    }

    #[test]
    fn accepts_optional_auto_join_scope() {
        let mut scopes = slack_scopes();
        scopes.push(SLACK_AUTO_JOIN_PUBLIC_CHANNELS_SCOPE.to_string());

        validate_slack_oauth_scopes(&scopes).expect("optional auto-join scope");
    }

    #[test]
    fn rejects_chat_write_scope_in_read_only_v1() {
        let mut scopes = slack_scopes();
        scopes.push("chat:write".to_string());

        let error = validate_slack_oauth_scopes(&scopes).expect_err("write scope rejected");
        assert!(error.to_string().contains("unsupported Slack OAuth scope"));
    }

    #[test]
    fn rejects_unlisted_write_scopes_in_read_only_v1() {
        for write_scope in ["chat:write.public", "reactions:write", "users:write"] {
            let mut scopes = slack_scopes();
            scopes.push(write_scope.to_string());

            let error = validate_slack_oauth_scopes(&scopes).expect_err("write scope rejected");

            assert!(
                error.to_string().contains("unsupported Slack OAuth scope"),
                "{write_scope} should be reported as unsupported: {error}"
            );
        }
    }

    #[test]
    fn stored_capabilities_match_slack_v1() {
        let capabilities: ConnectorCapabilities =
            serde_json::from_str(&slack_capabilities_json().expect("capabilities json"))
                .expect("decode capabilities");

        assert!(capabilities.supports_oauth);
        assert!(capabilities.supports_remote_observation);
        assert!(capabilities.supports_lazy_child_enumeration);
        assert!(!capabilities.supports_block_updates);
        assert!(!capabilities.supports_databases);
        assert!(!capabilities.supports_media_download);
        assert!(!capabilities.supports_undo);
        assert!(!capabilities.supports_batch_observation);
    }

    #[test]
    fn stores_broker_token_without_refresh_secret() {
        let credential = StoredSlackCredential::from_broker_token(
            broker_token(slack_scopes()),
            "slack-client-id".to_string(),
            "https://auth.example.test".to_string(),
            1780000000,
        )
        .expect("stored credential");

        assert_eq!(credential.connector, "slack");
        assert_eq!(
            credential.refresh_token_handle.as_deref(),
            Some("opaque-refresh-handle")
        );
        assert!(format!("{credential:?}").contains("<redacted>"));
        assert!(!format!("{credential:?}").contains("xoxb-access"));
    }

    #[test]
    fn refreshed_broker_credential_rejects_write_scope() {
        let credential = StoredSlackCredential::from_broker_token(
            broker_token(slack_scopes()),
            "slack-client-id".to_string(),
            "https://auth.example.test".to_string(),
            1780000000,
        )
        .expect("stored credential");
        let mut scopes = slack_scopes();
        scopes.push("reactions:write".to_string());

        let error = credential
            .refreshed(broker_token(scopes), 1780000300)
            .expect_err("write scope rejected");

        assert!(error.to_string().contains("unsupported Slack OAuth scope"));
    }

    #[test]
    fn refreshed_broker_credential_rejects_dropped_existing_scope() {
        let mut original_scopes = slack_scopes();
        original_scopes.push(SLACK_AUTO_JOIN_PUBLIC_CHANNELS_SCOPE.to_string());
        let credential = StoredSlackCredential::from_broker_token(
            broker_token(original_scopes),
            "slack-client-id".to_string(),
            "https://auth.example.test".to_string(),
            1780000000,
        )
        .expect("stored credential");

        let error = credential
            .refreshed(broker_token(slack_scopes()), 1780000300)
            .expect_err("dropped existing scope rejected");

        assert!(
            error
                .to_string()
                .contains(SLACK_AUTO_JOIN_PUBLIC_CHANNELS_SCOPE)
        );
    }

    #[test]
    fn broker_non_success_error_does_not_echo_response_body() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test broker");
        let broker_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).expect("read request");
            let body = r#"{"error":"invalid_code","code":"secret-code","refresh_token_handle":"opaque-refresh-handle"}"#;
            write!(
                stream,
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("write response");
        });
        let client = HttpSlackOAuthBrokerClient::new(broker_url);

        let error = client
            .start(&OAuthBrokerStart {
                connector: "slack".to_string(),
                redirect_uri: DEFAULT_SLACK_OAUTH_REDIRECT_URI.to_string(),
                scopes: Vec::new(),
            })
            .expect_err("non-success broker response");

        server.join().expect("server thread");
        let message = error.to_string();
        assert!(message.contains("slack oauth broker returned HTTP 400 Bad Request"));
        assert!(!message.contains("secret-code"));
        assert!(!message.contains("opaque-refresh-handle"));
    }
}
