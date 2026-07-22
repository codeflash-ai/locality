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

pub const GOOGLE_CALENDAR_CONNECTOR_ID: &str = "google-calendar";
pub const DEFAULT_GOOGLE_CALENDAR_OAUTH_BROKER_URL: &str =
    "https://afs-oauth-broker.saurabh-b07.workers.dev";
pub const DEFAULT_GOOGLE_CALENDAR_OAUTH_REDIRECT_URI: &str =
    "http://localhost:8757/oauth/google-calendar/callback";
pub const GOOGLE_CALENDAR_OAUTH_SCOPES: &[&str] = &[
    "openid",
    "email",
    "profile",
    "https://www.googleapis.com/auth/calendar.events",
];
const REQUIRED_GOOGLE_CALENDAR_API_SCOPES: &[&str] =
    &["https://www.googleapis.com/auth/calendar.events"];

static REQWEST_CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredGoogleCalendarCredential {
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

impl fmt::Debug for StoredGoogleCalendarCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredGoogleCalendarCredential")
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

impl StoredGoogleCalendarCredential {
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
            connector: GOOGLE_CALENDAR_CONNECTOR_ID.to_string(),
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
    ) -> Result<Self, GoogleCalendarOAuthScopeError> {
        let expires_at = token
            .expires_in
            .and_then(|expires_in| acquired_at.checked_add(expires_in));
        let scopes = if token.scopes.is_empty() {
            self.scopes.clone()
        } else {
            validate_google_calendar_oauth_scopes(&token.scopes)?;
            token.scopes
        };
        Ok(Self {
            kind: "oauth".to_string(),
            connector: GOOGLE_CALENDAR_CONNECTOR_ID.to_string(),
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
pub enum GoogleCalendarOAuthScopeError {
    MissingRequiredScope(&'static str),
}

impl fmt::Display for GoogleCalendarOAuthScopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRequiredScope(scope) => write!(
                f,
                "Google Calendar OAuth broker response missing required Google Calendar OAuth scope `{scope}`; reconnect with the default Google Calendar OAuth broker configuration"
            ),
        }
    }
}

impl std::error::Error for GoogleCalendarOAuthScopeError {}

pub fn validate_google_calendar_oauth_scopes(
    scopes: &[String],
) -> Result<(), GoogleCalendarOAuthScopeError> {
    let granted = scopes.iter().map(String::as_str).collect::<BTreeSet<_>>();
    for required in REQUIRED_GOOGLE_CALENDAR_API_SCOPES {
        if !granted.contains(required) {
            return Err(GoogleCalendarOAuthScopeError::MissingRequiredScope(
                required,
            ));
        }
    }

    Ok(())
}

#[derive(Clone, Debug)]
pub struct HttpGoogleCalendarOAuthBrokerClient {
    base_url: String,
    client: Client,
}

impl HttpGoogleCalendarOAuthBrokerClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: google_calendar_http_client(),
        }
    }

    pub fn start(&self, request: &OAuthBrokerStart) -> LocalityResult<OAuthBrokerStartResponse> {
        self.post_json("/v1/oauth/google-calendar/start", request)
    }

    pub fn exchange_code(
        &self,
        request: &OAuthBrokerCodeExchange,
    ) -> LocalityResult<OAuthBrokerToken> {
        self.post_json("/v1/oauth/google-calendar/exchange", request)
    }

    pub fn refresh_token(&self, request: &OAuthBrokerRefresh) -> LocalityResult<OAuthBrokerToken> {
        self.post_json("/v1/oauth/google-calendar/refresh", request)
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
                LocalityError::Io(format!(
                    "google calendar oauth broker request failed: {error}"
                ))
            })?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .unwrap_or_else(|error| format!("<failed to read error body: {error}>"));
            return Err(LocalityError::Io(format!(
                "google calendar oauth broker returned HTTP {status}: {body}"
            )));
        }
        response.json().map_err(|error| {
            LocalityError::Io(format!(
                "google calendar oauth broker response decode failed: {error}"
            ))
        })
    }
}

pub fn google_calendar_capabilities_json() -> Result<String, serde_json::Error> {
    let capabilities = ConnectorCapabilities {
        supports_block_updates: false,
        supports_databases: false,
        supports_oauth: true,
        supports_remote_observation: true,
        supports_entity_body_updates: false,
        supports_lazy_child_enumeration: true,
        supports_media_download: false,
        supports_undo: false,
        supports_batch_observation: false,
    };
    serde_json::to_string(&capabilities)
}

fn google_calendar_http_client() -> Client {
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
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;

    use locality_connector::ConnectorCapabilities;
    use locality_connector::oauth_broker::{
        OAuthBrokerCodeExchange, OAuthBrokerRefresh, OAuthBrokerStart, OAuthBrokerToken,
    };
    use locality_core::LocalityError;
    use serde_json::{Value, json};

    use super::{
        DEFAULT_GOOGLE_CALENDAR_OAUTH_BROKER_URL, DEFAULT_GOOGLE_CALENDAR_OAUTH_REDIRECT_URI,
        GOOGLE_CALENDAR_CONNECTOR_ID, GOOGLE_CALENDAR_OAUTH_SCOPES, GoogleCalendarOAuthScopeError,
        HttpGoogleCalendarOAuthBrokerClient, StoredGoogleCalendarCredential,
        google_calendar_capabilities_json, validate_google_calendar_oauth_scopes,
    };

    fn calendar_scopes() -> Vec<String> {
        GOOGLE_CALENDAR_OAUTH_SCOPES
            .iter()
            .map(|scope| scope.to_string())
            .collect()
    }

    fn calendar_broker_token(scopes: Vec<String>) -> OAuthBrokerToken {
        OAuthBrokerToken {
            access_token: "access-token".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: Some(3600),
            refresh_token_handle: Some("handle-1".to_string()),
            account_id: Some("acct-1".to_string()),
            account_label: Some("ann@example.com".to_string()),
            workspace_id: Some("primary".to_string()),
            workspace_name: Some("Primary calendar".to_string()),
            scopes,
        }
    }

    #[test]
    fn oauth_constants_match_google_calendar_broker_contract() {
        assert_eq!(GOOGLE_CALENDAR_CONNECTOR_ID, "google-calendar");
        assert_eq!(
            DEFAULT_GOOGLE_CALENDAR_OAUTH_BROKER_URL,
            "https://afs-oauth-broker.saurabh-b07.workers.dev"
        );
        assert_eq!(
            DEFAULT_GOOGLE_CALENDAR_OAUTH_REDIRECT_URI,
            "http://localhost:8757/oauth/google-calendar/callback"
        );
        assert_eq!(
            GOOGLE_CALENDAR_OAUTH_SCOPES,
            &[
                "openid",
                "email",
                "profile",
                "https://www.googleapis.com/auth/calendar.events",
            ]
        );
    }

    #[test]
    fn google_calendar_scope_validation_allows_calendar_events_scope_only() {
        validate_google_calendar_oauth_scopes(&vec![
            "https://www.googleapis.com/auth/calendar.events".to_string(),
        ])
        .expect("calendar events scope is sufficient");
    }

    #[test]
    fn google_calendar_scope_validation_rejects_missing_calendar_events_scope() {
        let error = validate_google_calendar_oauth_scopes(&[
            "openid".to_string(),
            "email".to_string(),
            "profile".to_string(),
        ])
        .expect_err("missing calendar events scope");

        assert_eq!(
            error,
            GoogleCalendarOAuthScopeError::MissingRequiredScope(
                "https://www.googleapis.com/auth/calendar.events"
            )
        );
        assert!(
            error
                .to_string()
                .contains("missing required Google Calendar OAuth scope")
        );
    }

    #[test]
    fn stored_capabilities_match_google_calendar_connector_support() {
        let capabilities: ConnectorCapabilities =
            serde_json::from_str(&google_calendar_capabilities_json().expect("capabilities json"))
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
    fn broker_credential_stores_token_metadata_and_refresh_handle_without_secret() {
        let stored = StoredGoogleCalendarCredential::from_broker_token(
            calendar_broker_token(calendar_scopes()),
            "client-id".to_string(),
            "https://auth.example.test".to_string(),
            100,
        );

        assert_eq!(stored.kind, "oauth");
        assert_eq!(stored.connector, GOOGLE_CALENDAR_CONNECTOR_ID);
        assert_eq!(stored.access_token, "access-token");
        assert_eq!(stored.token_type.as_deref(), Some("Bearer"));
        assert_eq!(stored.oauth_client_id.as_deref(), Some("client-id"));
        assert_eq!(
            stored.oauth_broker_url.as_deref(),
            Some("https://auth.example.test")
        );
        assert_eq!(stored.account_id.as_deref(), Some("acct-1"));
        assert_eq!(stored.account_label.as_deref(), Some("ann@example.com"));
        assert_eq!(stored.workspace_id.as_deref(), Some("primary"));
        assert_eq!(stored.workspace_name.as_deref(), Some("Primary calendar"));
        assert_eq!(stored.scopes, calendar_scopes());
        assert_eq!(stored.refresh_token_handle.as_deref(), Some("handle-1"));
        assert_eq!(stored.acquired_at, 100);
        assert_eq!(stored.expires_at, Some(3700));

        let json = serde_json::to_string(&stored).expect("serialize stored credential");
        assert!(!json.contains("\"refresh_token\":"));
        assert!(!json.contains("client_secret"));
    }

    #[test]
    fn stored_credential_debug_redacts_token_and_refresh_handle() {
        let mut credential = StoredGoogleCalendarCredential::from_broker_token(
            calendar_broker_token(calendar_scopes()),
            "client-id".to_string(),
            "https://auth.example.test".to_string(),
            100,
        );
        credential.access_token = "secret-access-token".to_string();
        credential.refresh_token_handle = Some("secret-refresh-handle".to_string());

        let debug = format!("{credential:?}");

        assert!(!debug.contains("secret-access-token"));
        assert!(!debug.contains("secret-refresh-handle"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn refreshed_broker_credential_preserves_scopes_and_falls_back_metadata() {
        let stored = StoredGoogleCalendarCredential::from_broker_token(
            calendar_broker_token(calendar_scopes()),
            "client-id".to_string(),
            "https://auth.example.test".to_string(),
            100,
        );

        let refreshed = stored
            .refreshed(
                OAuthBrokerToken {
                    access_token: "new-access-token".to_string(),
                    token_type: None,
                    expires_in: Some(7200),
                    refresh_token_handle: Some("handle-2".to_string()),
                    account_id: None,
                    account_label: None,
                    workspace_id: None,
                    workspace_name: None,
                    scopes: vec![],
                },
                200,
            )
            .expect("refresh with omitted scopes");

        assert_eq!(refreshed.access_token, "new-access-token");
        assert_eq!(refreshed.token_type.as_deref(), Some("Bearer"));
        assert_eq!(refreshed.oauth_client_id.as_deref(), Some("client-id"));
        assert_eq!(
            refreshed.oauth_broker_url.as_deref(),
            Some("https://auth.example.test")
        );
        assert_eq!(refreshed.account_id.as_deref(), Some("acct-1"));
        assert_eq!(refreshed.account_label.as_deref(), Some("ann@example.com"));
        assert_eq!(refreshed.workspace_id.as_deref(), Some("primary"));
        assert_eq!(
            refreshed.workspace_name.as_deref(),
            Some("Primary calendar")
        );
        assert_eq!(refreshed.scopes, stored.scopes);
        assert_eq!(refreshed.refresh_token_handle.as_deref(), Some("handle-2"));
        assert_eq!(refreshed.acquired_at, 200);
        assert_eq!(refreshed.expires_at, Some(7400));
    }

    #[test]
    fn refreshed_broker_credential_rejects_invalid_non_empty_scopes() {
        let stored = StoredGoogleCalendarCredential::from_broker_token(
            calendar_broker_token(calendar_scopes()),
            "client-id".to_string(),
            "https://auth.example.test".to_string(),
            100,
        );

        let error = stored
            .refreshed(
                OAuthBrokerToken {
                    access_token: "new-access-token".to_string(),
                    token_type: Some("Bearer".to_string()),
                    expires_in: Some(3600),
                    refresh_token_handle: Some("handle-2".to_string()),
                    account_id: None,
                    account_label: None,
                    workspace_id: None,
                    workspace_name: None,
                    scopes: vec!["openid".to_string()],
                },
                200,
            )
            .expect_err("invalid refreshed scopes");

        assert_eq!(
            error,
            GoogleCalendarOAuthScopeError::MissingRequiredScope(
                "https://www.googleapis.com/auth/calendar.events"
            )
        );
    }

    #[test]
    fn expires_soon_uses_sixty_second_threshold() {
        let mut stored = StoredGoogleCalendarCredential::from_broker_token(
            calendar_broker_token(calendar_scopes()),
            "client-id".to_string(),
            "https://auth.example.test".to_string(),
            100,
        );

        stored.expires_at = Some(160);
        assert!(stored.expires_soon(100));

        stored.expires_at = Some(161);
        assert!(!stored.expires_soon(100));

        stored.expires_at = None;
        assert!(!stored.expires_soon(u64::MAX));
    }

    #[test]
    fn oauth_broker_start_posts_google_calendar_path_and_decodes_response() {
        let (base_url, request_rx, server) = spawn_response_server(
            "HTTP/1.1 200 OK",
            json!({
                "connector": GOOGLE_CALENDAR_CONNECTOR_ID,
                "client_id": "client-1",
                "authorization_url": "https://accounts.example.test/auth",
                "redirect_uri": DEFAULT_GOOGLE_CALENDAR_OAUTH_REDIRECT_URI,
                "session": "session-1",
                "state": "state-1",
                "expires_in": 300,
            })
            .to_string(),
        );
        let client = HttpGoogleCalendarOAuthBrokerClient::new(format!("{base_url}/"));

        let response = client
            .start(&OAuthBrokerStart {
                connector: GOOGLE_CALENDAR_CONNECTOR_ID.to_string(),
                redirect_uri: DEFAULT_GOOGLE_CALENDAR_OAUTH_REDIRECT_URI.to_string(),
            })
            .expect("start response");

        assert_eq!(response.connector, GOOGLE_CALENDAR_CONNECTOR_ID);
        assert_eq!(response.client_id, "client-1");
        assert_eq!(
            response.authorization_url,
            "https://accounts.example.test/auth"
        );
        assert_eq!(
            response.redirect_uri,
            DEFAULT_GOOGLE_CALENDAR_OAUTH_REDIRECT_URI
        );
        assert_eq!(response.session, "session-1");
        assert_eq!(response.state, "state-1");
        assert_eq!(response.expires_in, 300);
        let request = request_rx.recv().expect("request");
        server.join().expect("server exits");
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/v1/oauth/google-calendar/start");
        assert_eq!(request.header("content-type"), Some("application/json"));
        let body: Value = serde_json::from_str(&request.body).expect("json body");
        assert_eq!(body["connector"], json!(GOOGLE_CALENDAR_CONNECTOR_ID));
        assert_eq!(
            body["redirect_uri"],
            json!(DEFAULT_GOOGLE_CALENDAR_OAUTH_REDIRECT_URI)
        );
    }

    #[test]
    fn oauth_broker_exchange_posts_google_calendar_path_and_decodes_token() {
        let (base_url, request_rx, server) = spawn_response_server(
            "HTTP/1.1 200 OK",
            json!({
                "access_token": "access-2",
                "token_type": "Bearer",
                "expires_in": 3600,
                "refresh_token_handle": "handle-2",
                "account_id": "acct-2",
                "account_label": "sam@example.com",
                "workspace_id": "primary",
                "workspace_name": "Primary calendar",
                "scopes": ["https://www.googleapis.com/auth/calendar.events"],
            })
            .to_string(),
        );
        let client = HttpGoogleCalendarOAuthBrokerClient::new(base_url);

        let token = client
            .exchange_code(&OAuthBrokerCodeExchange {
                connector: GOOGLE_CALENDAR_CONNECTOR_ID.to_string(),
                session: "session-2".to_string(),
                state: "state-2".to_string(),
                code: "code-2".to_string(),
                redirect_uri: DEFAULT_GOOGLE_CALENDAR_OAUTH_REDIRECT_URI.to_string(),
            })
            .expect("exchange token");

        assert_eq!(token.access_token, "access-2");
        assert_eq!(token.token_type.as_deref(), Some("Bearer"));
        assert_eq!(token.expires_in, Some(3600));
        assert_eq!(token.refresh_token_handle.as_deref(), Some("handle-2"));
        assert_eq!(token.account_id.as_deref(), Some("acct-2"));
        assert_eq!(token.account_label.as_deref(), Some("sam@example.com"));
        assert_eq!(token.workspace_id.as_deref(), Some("primary"));
        assert_eq!(token.workspace_name.as_deref(), Some("Primary calendar"));
        assert_eq!(
            token.scopes,
            vec!["https://www.googleapis.com/auth/calendar.events".to_string()]
        );
        let request = request_rx.recv().expect("request");
        server.join().expect("server exits");
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/v1/oauth/google-calendar/exchange");
        let body: Value = serde_json::from_str(&request.body).expect("json body");
        assert_eq!(body["connector"], json!(GOOGLE_CALENDAR_CONNECTOR_ID));
        assert_eq!(body["session"], json!("session-2"));
        assert_eq!(body["state"], json!("state-2"));
        assert_eq!(body["code"], json!("code-2"));
        assert_eq!(
            body["redirect_uri"],
            json!(DEFAULT_GOOGLE_CALENDAR_OAUTH_REDIRECT_URI)
        );
    }

    #[test]
    fn oauth_broker_refresh_posts_google_calendar_path_and_decodes_token() {
        let (base_url, request_rx, server) = spawn_response_server(
            "HTTP/1.1 200 OK",
            json!({
                "access_token": "access-3",
                "token_type": "Bearer",
                "expires_in": 1800,
                "refresh_token_handle": "handle-3",
                "account_id": "acct-3",
                "account_label": "lee@example.com",
                "workspace_id": "primary",
                "workspace_name": "Primary calendar",
                "scope": "openid email profile https://www.googleapis.com/auth/calendar.events",
            })
            .to_string(),
        );
        let client = HttpGoogleCalendarOAuthBrokerClient::new(base_url);

        let token = client
            .refresh_token(&OAuthBrokerRefresh {
                connector: GOOGLE_CALENDAR_CONNECTOR_ID.to_string(),
                refresh_token_handle: Some("handle-2".to_string()),
            })
            .expect("refresh token");

        assert_eq!(token.access_token, "access-3");
        assert_eq!(token.expires_in, Some(1800));
        assert_eq!(token.refresh_token_handle.as_deref(), Some("handle-3"));
        assert_eq!(token.account_id.as_deref(), Some("acct-3"));
        assert_eq!(token.account_label.as_deref(), Some("lee@example.com"));
        assert_eq!(
            token.scopes,
            vec![
                "openid".to_string(),
                "email".to_string(),
                "profile".to_string(),
                "https://www.googleapis.com/auth/calendar.events".to_string(),
            ]
        );
        let request = request_rx.recv().expect("request");
        server.join().expect("server exits");
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/v1/oauth/google-calendar/refresh");
        let body: Value = serde_json::from_str(&request.body).expect("json body");
        assert_eq!(body["connector"], json!(GOOGLE_CALENDAR_CONNECTOR_ID));
        assert_eq!(body["refresh_token_handle"], json!("handle-2"));
    }

    #[test]
    fn oauth_broker_non_success_status_maps_to_io_error_with_body() {
        let (base_url, request_rx, server) =
            spawn_response_server("HTTP/1.1 503 Service Unavailable", "broker unavailable");
        let client = HttpGoogleCalendarOAuthBrokerClient::new(base_url);

        let error = client
            .start(&OAuthBrokerStart {
                connector: GOOGLE_CALENDAR_CONNECTOR_ID.to_string(),
                redirect_uri: DEFAULT_GOOGLE_CALENDAR_OAUTH_REDIRECT_URI.to_string(),
            })
            .expect_err("status should fail");

        request_rx.recv().expect("request");
        server.join().expect("server exits");
        assert!(matches!(
            error,
            LocalityError::Io(message)
                if message.contains(
                    "google calendar oauth broker returned HTTP 503 Service Unavailable: broker unavailable"
                )
        ));
    }

    #[test]
    fn oauth_broker_invalid_success_json_maps_to_decode_error() {
        let (base_url, request_rx, server) = spawn_response_server("HTTP/1.1 200 OK", "not-json");
        let client = HttpGoogleCalendarOAuthBrokerClient::new(base_url);

        let error = client
            .start(&OAuthBrokerStart {
                connector: GOOGLE_CALENDAR_CONNECTOR_ID.to_string(),
                redirect_uri: DEFAULT_GOOGLE_CALENDAR_OAUTH_REDIRECT_URI.to_string(),
            })
            .expect_err("decode should fail");

        request_rx.recv().expect("request");
        server.join().expect("server exits");
        assert!(matches!(
            error,
            LocalityError::Io(message)
                if message.contains("google calendar oauth broker response decode failed")
        ));
    }

    #[test]
    fn oauth_broker_send_failure_maps_to_request_error() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind unused port");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        drop(listener);
        let client = HttpGoogleCalendarOAuthBrokerClient::new(base_url);

        let error = client
            .start(&OAuthBrokerStart {
                connector: GOOGLE_CALENDAR_CONNECTOR_ID.to_string(),
                redirect_uri: DEFAULT_GOOGLE_CALENDAR_OAUTH_REDIRECT_URI.to_string(),
            })
            .expect_err("request should fail");

        assert!(matches!(
            error,
            LocalityError::Io(message)
                if message.contains("google calendar oauth broker request failed")
        ));
    }

    #[derive(Debug)]
    struct CapturedRequest {
        method: String,
        target: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl CapturedRequest {
        fn parse(raw: String) -> Self {
            let (header_block, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
            let mut lines = header_block.lines();
            let request_line = lines.next().unwrap_or_default();
            let mut request_parts = request_line.split_whitespace();
            let method = request_parts.next().unwrap_or_default().to_string();
            let target = request_parts.next().unwrap_or_default().to_string();
            let headers = lines
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
                .collect();

            Self {
                method,
                target,
                headers,
                body: body.to_string(),
            }
        }

        fn header(&self, name: &str) -> Option<&str> {
            let name = name.to_ascii_lowercase();
            self.headers
                .iter()
                .find(|(header_name, _)| header_name == &name)
                .map(|(_, value)| value.as_str())
        }
    }

    fn spawn_response_server(
        status_line: &'static str,
        body: impl Into<String>,
    ) -> (
        String,
        mpsc::Receiver<CapturedRequest>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let body = body.into();
        let (request_tx, request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let request = CapturedRequest::parse(read_http_request(&mut stream));
            request_tx.send(request).expect("send request");
            let response = format!(
                "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        });
        (base_url, request_rx, server)
    }

    fn read_http_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let headers_end = loop {
            let bytes_read = stream.read(&mut buffer).expect("read request");
            if bytes_read == 0 {
                break request.len();
            }
            request.extend_from_slice(&buffer[..bytes_read]);
            if let Some(headers_end) = find_headers_end(&request) {
                break headers_end;
            }
        };
        let content_length = content_length(&request[..headers_end]);
        while request.len() < headers_end + content_length {
            let bytes_read = stream.read(&mut buffer).expect("read request body");
            if bytes_read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..bytes_read]);
        }
        String::from_utf8(request).expect("utf8 request")
    }

    fn find_headers_end(request: &[u8]) -> Option<usize> {
        request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
    }

    fn content_length(headers: &[u8]) -> usize {
        String::from_utf8_lossy(headers)
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().ok())
                    .flatten()
            })
            .unwrap_or(0)
    }
}
