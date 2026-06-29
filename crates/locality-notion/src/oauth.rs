//! Notion OAuth transport and credential payloads.
//!
//! Secrets returned by this module must be stored only through the Locality
//! credential store. SQLite stores metadata and a `secret_ref`, never these
//! values.

use locality_core::{LocalityError, LocalityResult};
use reqwest::{Url, blocking::Client};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::client::{DEFAULT_NOTION_API_BASE_URL, DEFAULT_NOTION_VERSION, notion_http_client};

pub const DEFAULT_NOTION_OAUTH_AUTHORIZE_URL: &str = "https://api.notion.com/v1/oauth/authorize";
// Cloudflare worker name is still `afs-oauth-broker`; the workers.dev hostname
// predates the Locality product rename until auth.locality.dev is deployed.
pub const DEFAULT_LOCALITY_NOTION_OAUTH_BROKER_URL: &str =
    "https://afs-oauth-broker.saurabh-b07.workers.dev";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotionOAuthCodeExchange {
    pub client_id: String,
    pub client_secret: String,
    pub code: String,
    pub redirect_uri: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotionOAuthRefresh {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotionOAuthBrokerStart {
    pub redirect_uri: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct NotionOAuthBrokerStartResponse {
    pub connector: String,
    pub client_id: String,
    pub authorization_url: String,
    pub redirect_uri: String,
    pub session: String,
    pub state: String,
    pub expires_in: u64,
}

impl NotionOAuthBrokerStartResponse {
    pub fn normalized_authorization_url(&self) -> String {
        normalize_notion_authorization_url(
            &self.authorization_url,
            &self.client_id,
            &self.redirect_uri,
            &self.state,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotionOAuthBrokerCodeExchange {
    pub session: String,
    pub state: String,
    pub code: String,
    pub redirect_uri: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotionOAuthBrokerRefresh {
    pub refresh_token: Option<String>,
    pub refresh_token_handle: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotionOAuthToken {
    pub access_token: String,
    pub token_type: Option<String>,
    pub refresh_token: Option<String>,
    pub refresh_token_kind: Option<String>,
    pub refresh_token_handle: Option<String>,
    pub expires_in: Option<u64>,
    pub bot_id: Option<String>,
    pub workspace_id: Option<String>,
    pub workspace_name: Option<String>,
    pub workspace_icon: Option<String>,
    pub owner: Option<serde_json::Value>,
    pub duplicated_template_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredNotionCredential {
    pub kind: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_type: Option<String>,
    pub oauth_client_id: Option<String>,
    pub oauth_client_secret: Option<String>,
    pub oauth_broker_url: Option<String>,
    pub workspace_id: Option<String>,
    pub workspace_name: Option<String>,
    pub bot_id: Option<String>,
    pub refresh_token_handle: Option<String>,
    pub acquired_at: u64,
    pub expires_at: Option<u64>,
}

impl StoredNotionCredential {
    pub fn from_oauth_token(
        token: NotionOAuthToken,
        client_id: String,
        client_secret: String,
        acquired_at: u64,
    ) -> Self {
        let expires_at = token
            .expires_in
            .and_then(|expires_in| acquired_at.checked_add(expires_in));
        Self {
            kind: "oauth".to_string(),
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            token_type: token.token_type,
            oauth_client_id: Some(client_id),
            oauth_client_secret: Some(client_secret),
            oauth_broker_url: None,
            workspace_id: token.workspace_id,
            workspace_name: token.workspace_name,
            bot_id: token.bot_id,
            refresh_token_handle: token.refresh_token_handle,
            acquired_at,
            expires_at,
        }
    }

    pub fn from_broker_oauth_token(
        token: NotionOAuthToken,
        client_id: String,
        broker_url: String,
        acquired_at: u64,
    ) -> Self {
        let expires_at = token
            .expires_in
            .and_then(|expires_in| acquired_at.checked_add(expires_in));
        Self {
            kind: "oauth".to_string(),
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            token_type: token.token_type,
            oauth_client_id: Some(client_id),
            oauth_client_secret: None,
            oauth_broker_url: Some(broker_url),
            workspace_id: token.workspace_id,
            workspace_name: token.workspace_name,
            bot_id: token.bot_id,
            refresh_token_handle: token.refresh_token_handle,
            acquired_at,
            expires_at,
        }
    }

    pub fn refreshed(&self, token: NotionOAuthToken, acquired_at: u64) -> Self {
        let expires_at = token
            .expires_in
            .and_then(|expires_in| acquired_at.checked_add(expires_in));
        Self {
            kind: "oauth".to_string(),
            access_token: token.access_token,
            refresh_token: token.refresh_token.or_else(|| self.refresh_token.clone()),
            token_type: token.token_type.or_else(|| self.token_type.clone()),
            oauth_client_id: self.oauth_client_id.clone(),
            oauth_client_secret: self.oauth_client_secret.clone(),
            oauth_broker_url: self.oauth_broker_url.clone(),
            workspace_id: token.workspace_id.or_else(|| self.workspace_id.clone()),
            workspace_name: token.workspace_name.or_else(|| self.workspace_name.clone()),
            bot_id: token.bot_id.or_else(|| self.bot_id.clone()),
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
pub struct HttpNotionOAuthBrokerClient {
    base_url: String,
    client: Client,
}

pub fn normalize_notion_authorization_url(
    authorization_url: &str,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
) -> String {
    let mut url = Url::parse(authorization_url)
        .ok()
        .filter(is_notion_authorization_url)
        .unwrap_or_else(default_notion_authorization_url);

    set_notion_authorization_query(&mut url, client_id, redirect_uri, state);
    url.to_string()
}

fn default_notion_authorization_url() -> Url {
    Url::parse(DEFAULT_NOTION_OAUTH_AUTHORIZE_URL)
        .expect("default Notion OAuth authorize URL must be valid")
}

fn is_notion_authorization_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some("api.notion.com")
        && url.path() == "/v1/oauth/authorize"
}

fn set_notion_authorization_query(url: &mut Url, client_id: &str, redirect_uri: &str, state: &str) {
    let preserved_pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| !is_managed_notion_authorization_query_key(key))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();

    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in preserved_pairs {
            query.append_pair(&key, &value);
        }
        query.append_pair("client_id", client_id);
        query.append_pair("response_type", "code");
        query.append_pair("owner", "user");
        query.append_pair("redirect_uri", redirect_uri);
        query.append_pair("state", state);
    }
}

fn is_managed_notion_authorization_query_key(key: &str) -> bool {
    matches!(
        key,
        "client_id" | "response_type" | "owner" | "redirect_uri" | "state"
    )
}

impl HttpNotionOAuthBrokerClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: notion_http_client(),
        }
    }

    pub fn start(
        &self,
        request: &NotionOAuthBrokerStart,
    ) -> LocalityResult<NotionOAuthBrokerStartResponse> {
        self.post_json(
            "/v1/oauth/notion/start",
            json!({
                "redirect_uri": request.redirect_uri,
            }),
        )
    }

    pub fn exchange_code(
        &self,
        request: &NotionOAuthBrokerCodeExchange,
    ) -> LocalityResult<NotionOAuthToken> {
        self.post_json(
            "/v1/oauth/notion/exchange",
            json!({
                "session": request.session,
                "state": request.state,
                "code": request.code,
                "redirect_uri": request.redirect_uri,
            }),
        )
    }

    pub fn refresh_token(
        &self,
        request: &NotionOAuthBrokerRefresh,
    ) -> LocalityResult<NotionOAuthToken> {
        self.post_json(
            "/v1/oauth/notion/refresh",
            json!({
                "refresh_token": request.refresh_token,
                "refresh_token_handle": request.refresh_token_handle,
            }),
        )
    }

    fn post_json<T>(&self, path: &str, body: serde_json::Value) -> LocalityResult<T>
    where
        T: DeserializeOwned,
    {
        let response = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .json(&body)
            .send()
            .map_err(|error| {
                LocalityError::Io(format!("notion oauth broker request failed: {error}"))
            })?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .unwrap_or_else(|error| format!("<failed to read error body: {error}>"));
            return Err(LocalityError::Io(format!(
                "notion oauth broker returned HTTP {status}: {body}"
            )));
        }
        response.json().map_err(|error| {
            LocalityError::Io(format!(
                "notion oauth broker response decode failed: {error}"
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use reqwest::Url;

    use super::{
        DEFAULT_NOTION_OAUTH_AUTHORIZE_URL, NotionOAuthBrokerStartResponse, NotionOAuthToken,
        StoredNotionCredential, normalize_notion_authorization_url,
    };

    #[test]
    fn broker_start_response_normalizes_missing_response_type() {
        let start = NotionOAuthBrokerStartResponse {
            connector: "notion".to_string(),
            client_id: "client-id".to_string(),
            authorization_url:
                "https://api.notion.com/v1/oauth/authorize?client_id=client-id&prompt=select"
                    .to_string(),
            redirect_uri: "http://localhost:8757/oauth/notion/callback".to_string(),
            session: "session-1".to_string(),
            state: "state-1".to_string(),
            expires_in: 300,
        };

        let url = Url::parse(&start.normalized_authorization_url()).expect("normalized URL");

        assert_eq!(
            url.as_str().split('?').next(),
            Some(DEFAULT_NOTION_OAUTH_AUTHORIZE_URL)
        );
        assert_eq!(query_value(&url, "prompt").as_deref(), Some("select"));
        assert_eq!(query_value(&url, "client_id").as_deref(), Some("client-id"));
        assert_eq!(query_value(&url, "response_type").as_deref(), Some("code"));
        assert_eq!(query_value(&url, "owner").as_deref(), Some("user"));
        assert_eq!(
            query_value(&url, "redirect_uri").as_deref(),
            Some("http://localhost:8757/oauth/notion/callback")
        );
        assert_eq!(query_value(&url, "state").as_deref(), Some("state-1"));
    }

    #[test]
    fn normalize_notion_authorization_url_replaces_managed_parameters() {
        let normalized = normalize_notion_authorization_url(
            "https://api.notion.com/v1/oauth/authorize?client_id=wrong&response_type=token&response_type=none&owner=workspace&redirect_uri=http%3A%2F%2Fwrong&state=wrong",
            "client-id",
            "http://localhost:8757/oauth/notion/callback",
            "state-1",
        );
        let url = Url::parse(&normalized).expect("normalized URL");

        assert_eq!(query_value(&url, "client_id").as_deref(), Some("client-id"));
        assert_eq!(query_value(&url, "response_type").as_deref(), Some("code"));
        assert_eq!(query_count(&url, "response_type"), 1);
        assert_eq!(query_value(&url, "owner").as_deref(), Some("user"));
        assert_eq!(
            query_value(&url, "redirect_uri").as_deref(),
            Some("http://localhost:8757/oauth/notion/callback")
        );
        assert_eq!(query_value(&url, "state").as_deref(), Some("state-1"));
    }

    #[test]
    fn normalize_notion_authorization_url_discards_non_notion_authorize_url() {
        let normalized = normalize_notion_authorization_url(
            "https://example.test/oauth?prompt=select",
            "client-id",
            "http://localhost:8757/oauth/notion/callback",
            "state-1",
        );
        let url = Url::parse(&normalized).expect("normalized URL");

        assert_eq!(
            url.as_str().split('?').next(),
            Some(DEFAULT_NOTION_OAUTH_AUTHORIZE_URL)
        );
        assert_eq!(query_value(&url, "prompt"), None);
        assert_eq!(query_value(&url, "response_type").as_deref(), Some("code"));
    }

    #[test]
    fn broker_oauth_credential_stores_refresh_handle_without_client_secret() {
        let stored = StoredNotionCredential::from_broker_oauth_token(
            NotionOAuthToken {
                access_token: "access-token".to_string(),
                token_type: Some("bearer".to_string()),
                refresh_token: None,
                refresh_token_kind: Some("handle".to_string()),
                refresh_token_handle: Some("handle-1".to_string()),
                expires_in: Some(3600),
                bot_id: Some("bot-1".to_string()),
                workspace_id: Some("workspace-1".to_string()),
                workspace_name: Some("Locality".to_string()),
                workspace_icon: None,
                owner: None,
                duplicated_template_id: None,
            },
            "client-id".to_string(),
            "https://auth.example.test".to_string(),
            100,
        );

        assert_eq!(stored.oauth_client_id.as_deref(), Some("client-id"));
        assert_eq!(stored.oauth_client_secret, None);
        assert_eq!(
            stored.oauth_broker_url.as_deref(),
            Some("https://auth.example.test")
        );
        assert_eq!(stored.refresh_token, None);
        assert_eq!(stored.refresh_token_handle.as_deref(), Some("handle-1"));
        assert_eq!(stored.expires_at, Some(3700));
    }

    #[test]
    fn refreshed_broker_oauth_credential_rotates_refresh_handle() {
        let stored = StoredNotionCredential::from_broker_oauth_token(
            NotionOAuthToken {
                access_token: "access-token".to_string(),
                token_type: Some("bearer".to_string()),
                refresh_token: None,
                refresh_token_kind: Some("handle".to_string()),
                refresh_token_handle: Some("handle-1".to_string()),
                expires_in: Some(3600),
                bot_id: Some("bot-1".to_string()),
                workspace_id: Some("workspace-1".to_string()),
                workspace_name: Some("Locality".to_string()),
                workspace_icon: None,
                owner: None,
                duplicated_template_id: None,
            },
            "client-id".to_string(),
            "https://auth.example.test".to_string(),
            100,
        );

        let refreshed = stored.refreshed(
            NotionOAuthToken {
                access_token: "new-access-token".to_string(),
                token_type: Some("bearer".to_string()),
                refresh_token: None,
                refresh_token_kind: Some("handle".to_string()),
                refresh_token_handle: Some("handle-2".to_string()),
                expires_in: Some(7200),
                bot_id: None,
                workspace_id: None,
                workspace_name: None,
                workspace_icon: None,
                owner: None,
                duplicated_template_id: None,
            },
            200,
        );

        assert_eq!(refreshed.access_token, "new-access-token");
        assert_eq!(refreshed.refresh_token_handle.as_deref(), Some("handle-2"));
        assert_eq!(
            refreshed.oauth_broker_url.as_deref(),
            Some("https://auth.example.test")
        );
        assert_eq!(refreshed.workspace_name.as_deref(), Some("Locality"));
        assert_eq!(refreshed.expires_at, Some(7400));
    }

    fn query_value(url: &Url, name: &str) -> Option<String> {
        url.query_pairs()
            .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
    }

    fn query_count(url: &Url, name: &str) -> usize {
        url.query_pairs().filter(|(key, _)| key == name).count()
    }
}

#[derive(Clone, Debug, Default)]
pub struct HttpNotionOAuthClient {
    client: Client,
}

impl HttpNotionOAuthClient {
    pub fn new() -> Self {
        Self {
            client: notion_http_client(),
        }
    }

    pub fn exchange_code(
        &self,
        request: &NotionOAuthCodeExchange,
    ) -> LocalityResult<NotionOAuthToken> {
        self.token(
            json!({
                "grant_type": "authorization_code",
                "code": request.code,
                "redirect_uri": request.redirect_uri,
            }),
            &request.client_id,
            &request.client_secret,
        )
    }

    pub fn refresh_token(&self, request: &NotionOAuthRefresh) -> LocalityResult<NotionOAuthToken> {
        self.token(
            json!({
                "grant_type": "refresh_token",
                "refresh_token": request.refresh_token,
            }),
            &request.client_id,
            &request.client_secret,
        )
    }

    fn token(
        &self,
        body: serde_json::Value,
        client_id: &str,
        client_secret: &str,
    ) -> LocalityResult<NotionOAuthToken> {
        let response = self
            .client
            .post(format!("{DEFAULT_NOTION_API_BASE_URL}/v1/oauth/token"))
            .basic_auth(client_id, Some(client_secret))
            .header("Notion-Version", DEFAULT_NOTION_VERSION)
            .json(&body)
            .send()
            .map_err(|error| LocalityError::Io(format!("notion oauth request failed: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .unwrap_or_else(|error| format!("<failed to read error body: {error}>"));
            return Err(LocalityError::Io(format!(
                "notion oauth returned HTTP {status}: {body}"
            )));
        }
        response.json().map_err(|error| {
            LocalityError::Io(format!("notion oauth response decode failed: {error}"))
        })
    }
}
