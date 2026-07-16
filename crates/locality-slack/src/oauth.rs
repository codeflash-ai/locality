pub const DEFAULT_SLACK_OAUTH_BROKER_URL: &str = "https://oauth.locality.dev";
pub const DEFAULT_SLACK_OAUTH_REDIRECT_URI: &str =
    "http://localhost:8757/oauth/slack/callback";

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

#[derive(Clone, Debug, Default)]
pub struct HttpSlackOAuthBrokerClient;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlackOAuthScopeError {
    pub missing_scopes: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredSlackCredential {
    pub access_token: String,
}

pub fn slack_capabilities_json() -> serde_json::Value {
    serde_json::json!({
        "connector": "slack",
        "readonly": true,
    })
}

pub fn validate_slack_oauth_scopes<I, S>(scopes: I) -> Result<(), SlackOAuthScopeError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let provided: Vec<String> = scopes
        .into_iter()
        .map(|scope| scope.as_ref().to_owned())
        .collect();
    let missing_scopes = SLACK_OAUTH_SCOPES
        .iter()
        .filter(|required| !provided.iter().any(|scope| scope == *required))
        .map(|scope| (*scope).to_owned())
        .collect::<Vec<_>>();

    if missing_scopes.is_empty() {
        Ok(())
    } else {
        Err(SlackOAuthScopeError { missing_scopes })
    }
}
