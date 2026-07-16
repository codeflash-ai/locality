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

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

#[derive(Clone, Debug)]
pub struct HttpGoogleCalendarOAuthBrokerClient;
