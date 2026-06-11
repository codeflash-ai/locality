//! Notion OAuth transport and credential payloads.
//!
//! Secrets returned by this module must be stored only through the AgentFS
//! credential store. SQLite stores metadata and a `secret_ref`, never these
//! values.

use afs_core::{AfsError, AfsResult};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::client::{DEFAULT_NOTION_API_BASE_URL, DEFAULT_NOTION_VERSION};

pub const DEFAULT_NOTION_OAUTH_AUTHORIZE_URL: &str = "https://api.notion.com/v1/oauth/authorize";

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotionOAuthToken {
    pub access_token: String,
    pub token_type: Option<String>,
    pub refresh_token: Option<String>,
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
    pub workspace_id: Option<String>,
    pub workspace_name: Option<String>,
    pub bot_id: Option<String>,
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
            workspace_id: token.workspace_id,
            workspace_name: token.workspace_name,
            bot_id: token.bot_id,
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
            workspace_id: token.workspace_id.or_else(|| self.workspace_id.clone()),
            workspace_name: token.workspace_name.or_else(|| self.workspace_name.clone()),
            bot_id: token.bot_id.or_else(|| self.bot_id.clone()),
            acquired_at,
            expires_at,
        }
    }

    pub fn expires_soon(&self, now: u64) -> bool {
        self.expires_at
            .is_some_and(|expires_at| expires_at <= now.saturating_add(60))
    }
}

#[derive(Clone, Debug, Default)]
pub struct HttpNotionOAuthClient {
    client: Client,
}

impl HttpNotionOAuthClient {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
        }
    }

    pub fn exchange_code(&self, request: &NotionOAuthCodeExchange) -> AfsResult<NotionOAuthToken> {
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

    pub fn refresh_token(&self, request: &NotionOAuthRefresh) -> AfsResult<NotionOAuthToken> {
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
    ) -> AfsResult<NotionOAuthToken> {
        let response = self
            .client
            .post(format!("{DEFAULT_NOTION_API_BASE_URL}/v1/oauth/token"))
            .basic_auth(client_id, Some(client_secret))
            .header("Notion-Version", DEFAULT_NOTION_VERSION)
            .json(&body)
            .send()
            .map_err(|error| AfsError::Io(format!("notion oauth request failed: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .unwrap_or_else(|error| format!("<failed to read error body: {error}>"));
            return Err(AfsError::Io(format!(
                "notion oauth returned HTTP {status}: {body}"
            )));
        }
        response
            .json()
            .map_err(|error| AfsError::Io(format!("notion oauth response decode failed: {error}")))
    }
}
