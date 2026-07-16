use std::fmt;

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

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
                "has_refresh_token_handle",
                &self.refresh_token_handle.is_some(),
            )
            .field("acquired_at", &self.acquired_at)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct HttpGoogleCalendarOAuthBrokerClient;

#[cfg(test)]
mod tests {
    use super::{GOOGLE_CALENDAR_CONNECTOR_ID, StoredGoogleCalendarCredential};

    #[test]
    fn stored_credential_debug_redacts_token_and_refresh_handle() {
        let credential = StoredGoogleCalendarCredential {
            kind: "oauth".to_string(),
            connector: GOOGLE_CALENDAR_CONNECTOR_ID.to_string(),
            access_token: "secret-access-token".to_string(),
            token_type: Some("Bearer".to_string()),
            oauth_client_id: Some("client-id".to_string()),
            oauth_broker_url: Some("https://broker.example.test".to_string()),
            account_id: Some("account-id".to_string()),
            account_label: Some("ann@example.com".to_string()),
            workspace_id: Some("primary".to_string()),
            workspace_name: Some("Primary calendar".to_string()),
            scopes: vec!["https://www.googleapis.com/auth/calendar.events".to_string()],
            refresh_token_handle: Some("secret-refresh-handle".to_string()),
            acquired_at: 100,
            expires_at: Some(200),
        };

        let debug = format!("{credential:?}");

        assert!(debug.contains("has_refresh_token_handle"));
        assert!(debug.contains("true"));
        assert!(!debug.contains("secret-access-token"));
        assert!(!debug.contains("secret-refresh-handle"));
    }
}
