//! Shared OAuth connector profiles.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OAuthConnector {
    Notion,
    GoogleDocs,
    GoogleCalendar,
    Gmail,
    Slack,
}

impl OAuthConnector {
    pub const fn all() -> &'static [Self] {
        &[
            Self::Notion,
            Self::GoogleDocs,
            Self::GoogleCalendar,
            Self::Gmail,
            Self::Slack,
        ]
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Notion => "notion",
            Self::GoogleDocs => "google-docs",
            Self::GoogleCalendar => "google-calendar",
            Self::Gmail => "gmail",
            Self::Slack => "slack",
        }
    }

    pub const fn broker_callback_path(self) -> &'static str {
        match self {
            Self::Notion => "/v1/oauth/notion/callback",
            Self::GoogleDocs => "/v1/oauth/google-docs/callback",
            Self::GoogleCalendar => "/v1/oauth/google-calendar/callback",
            Self::Gmail => "/v1/oauth/gmail/callback",
            Self::Slack => "/v1/oauth/slack/callback",
        }
    }

    pub const fn default_local_callback_uri(self) -> &'static str {
        match self {
            Self::Notion => "http://localhost:8757/oauth/notion/callback",
            Self::GoogleDocs => "http://localhost:8757/oauth/google-docs/callback",
            Self::GoogleCalendar => "http://localhost:8757/oauth/google-calendar/callback",
            Self::Gmail => "http://localhost:8757/oauth/gmail/callback",
            Self::Slack => "http://localhost:8757/oauth/slack/callback",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OAuthHostMode {
    LocalBrokered,
    HostedAdmin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OAuthProfile {
    pub connector: OAuthConnector,
    pub host: OAuthHostMode,
    pub scopes: &'static [&'static str],
    pub required_scopes: &'static [&'static str],
    pub client_completion_redirect_uri: &'static str,
    pub broker_callback_path: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OAuthProfileError {
    BrokerBaseUrlMustBeHttps,
    BrokerBaseUrlMustNotContainQueryOrFragment,
    BrokerBaseUrlMustNotBeEmpty,
}

pub const GOOGLE_OAUTH_AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const GOOGLE_OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

pub const GOOGLE_IDENTITY_SCOPES: &[&str] = &["openid", "email", "profile"];

pub const NOTION_LOCAL_BROKER_SCOPES: &[&str] = &[];
pub const NOTION_HOSTED_ADMIN_SCOPES: &[&str] = &[];

pub const GOOGLE_DOCS_REQUIRED_API_SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/documents",
    "https://www.googleapis.com/auth/drive.file",
    "https://www.googleapis.com/auth/drive.metadata",
];
pub const GOOGLE_DOCS_LOCAL_BROKER_SCOPES: &[&str] = &[
    "openid",
    "email",
    "profile",
    "https://www.googleapis.com/auth/documents",
    "https://www.googleapis.com/auth/drive.file",
    "https://www.googleapis.com/auth/drive.metadata",
];
pub const GOOGLE_DOCS_HOSTED_ADMIN_SCOPES: &[&str] = GOOGLE_DOCS_LOCAL_BROKER_SCOPES;

pub const GOOGLE_CALENDAR_LOCAL_BROKER_SCOPES: &[&str] = &[
    "openid",
    "email",
    "profile",
    "https://www.googleapis.com/auth/calendar.events",
];
pub const GOOGLE_CALENDAR_HOSTED_ADMIN_SCOPES: &[&str] = GOOGLE_CALENDAR_LOCAL_BROKER_SCOPES;
pub const GOOGLE_CALENDAR_REQUIRED_API_SCOPES: &[&str] =
    &["https://www.googleapis.com/auth/calendar.events"];

pub const GMAIL_LOCAL_BROKER_SCOPES: &[&str] = &[
    "openid",
    "email",
    "profile",
    "https://www.googleapis.com/auth/gmail.readonly",
    "https://www.googleapis.com/auth/gmail.compose",
];
pub const GMAIL_HOSTED_ADMIN_SCOPES: &[&str] = GMAIL_LOCAL_BROKER_SCOPES;
pub const GMAIL_REQUIRED_API_SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/gmail.readonly",
    "https://www.googleapis.com/auth/gmail.compose",
];
pub const GMAIL_FULL_MAILBOX_SCOPE: &str = "https://mail.google.com/";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoogleOAuthProfileError {
    UnsupportedConnector,
    InvalidClientId,
    InvalidRedirectUri,
    InvalidState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoogleOAuthScopeError {
    UnsupportedConnector,
    FullMailboxScope,
    MissingRequiredScope(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoogleOAuthTokenResponse {
    pub access_token: String,
    pub token_type: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_in: Option<u64>,
    pub scope: Option<String>,
    pub id_token: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoogleHostedCredential {
    pub kind: String,
    pub connector: String,
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: Option<String>,
    pub oauth_client_id: String,
    pub account_id: Option<String>,
    pub account_label: Option<String>,
    pub workspace_id: Option<String>,
    pub workspace_name: Option<String>,
    pub scopes: Vec<String>,
    pub acquired_at: u64,
    pub expires_at: Option<u64>,
}

pub const SLACK_AUTO_JOIN_PUBLIC_CHANNELS_SCOPE: &str = "channels:join";
pub const SLACK_LOCAL_BROKER_SCOPES: &[&str] = &[
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
    SLACK_AUTO_JOIN_PUBLIC_CHANNELS_SCOPE,
];
pub const SLACK_HOSTED_ADMIN_SCOPES: &[&str] = &[
    "channels:history",
    "channels:read",
    "files:read",
    "groups:history",
    "groups:read",
    "users:read",
];

pub const fn oauth_profile(connector: OAuthConnector, host: OAuthHostMode) -> Option<OAuthProfile> {
    let scopes = match (connector, host) {
        (OAuthConnector::Notion, OAuthHostMode::LocalBrokered) => NOTION_LOCAL_BROKER_SCOPES,
        (OAuthConnector::Notion, OAuthHostMode::HostedAdmin) => NOTION_HOSTED_ADMIN_SCOPES,
        (OAuthConnector::GoogleDocs, OAuthHostMode::LocalBrokered) => {
            GOOGLE_DOCS_LOCAL_BROKER_SCOPES
        }
        (OAuthConnector::GoogleDocs, OAuthHostMode::HostedAdmin) => GOOGLE_DOCS_HOSTED_ADMIN_SCOPES,
        (OAuthConnector::GoogleCalendar, OAuthHostMode::LocalBrokered) => {
            GOOGLE_CALENDAR_LOCAL_BROKER_SCOPES
        }
        (OAuthConnector::GoogleCalendar, OAuthHostMode::HostedAdmin) => {
            GOOGLE_CALENDAR_HOSTED_ADMIN_SCOPES
        }
        (OAuthConnector::Gmail, OAuthHostMode::LocalBrokered) => GMAIL_LOCAL_BROKER_SCOPES,
        (OAuthConnector::Gmail, OAuthHostMode::HostedAdmin) => GMAIL_HOSTED_ADMIN_SCOPES,
        (OAuthConnector::Slack, OAuthHostMode::LocalBrokered) => SLACK_LOCAL_BROKER_SCOPES,
        (OAuthConnector::Slack, OAuthHostMode::HostedAdmin) => SLACK_HOSTED_ADMIN_SCOPES,
    };
    let required_scopes = match (connector, host) {
        (OAuthConnector::GoogleDocs, _) => GOOGLE_DOCS_REQUIRED_API_SCOPES,
        (OAuthConnector::GoogleCalendar, _) => GOOGLE_CALENDAR_REQUIRED_API_SCOPES,
        (OAuthConnector::Gmail, _) => GMAIL_REQUIRED_API_SCOPES,
        (OAuthConnector::Slack, _) => scopes,
        _ => scopes,
    };
    Some(OAuthProfile {
        connector,
        host,
        scopes,
        required_scopes,
        client_completion_redirect_uri: connector.default_local_callback_uri(),
        broker_callback_path: connector.broker_callback_path(),
    })
}

pub fn broker_callback_uri(
    public_base_url: &str,
    connector: OAuthConnector,
) -> Result<String, OAuthProfileError> {
    let trimmed = public_base_url.trim();
    if trimmed.is_empty() {
        return Err(OAuthProfileError::BrokerBaseUrlMustNotBeEmpty);
    }
    if !trimmed.starts_with("https://") {
        return Err(OAuthProfileError::BrokerBaseUrlMustBeHttps);
    }
    if trimmed.contains('?') || trimmed.contains('#') {
        return Err(OAuthProfileError::BrokerBaseUrlMustNotContainQueryOrFragment);
    }
    if https_base_url_host(trimmed).is_none() {
        return Err(OAuthProfileError::BrokerBaseUrlMustNotBeEmpty);
    }
    Ok(format!(
        "{}{}",
        trimmed.trim_end_matches('/'),
        connector.broker_callback_path()
    ))
}

pub fn google_authorization_url(
    connector: OAuthConnector,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
) -> Result<String, GoogleOAuthProfileError> {
    let profile = google_hosted_profile(connector)?;
    if !valid_google_oauth_client_id(client_id) {
        return Err(GoogleOAuthProfileError::InvalidClientId);
    }
    if !valid_google_hosted_redirect_uri(connector, redirect_uri) {
        return Err(GoogleOAuthProfileError::InvalidRedirectUri);
    }
    if state.is_empty() || state.chars().any(char::is_control) {
        return Err(GoogleOAuthProfileError::InvalidState);
    }

    let mut url = String::from(GOOGLE_OAUTH_AUTHORIZE_URL);
    url.push('?');
    append_query_param(&mut url, "client_id", client_id);
    append_query_param(&mut url, "response_type", "code");
    append_query_param(&mut url, "redirect_uri", redirect_uri);
    append_query_param(&mut url, "scope", &profile.scopes.join(" "));
    append_query_param(&mut url, "state", state);
    append_query_param(&mut url, "access_type", "offline");
    append_query_param(&mut url, "prompt", "consent");
    Ok(url)
}

pub fn validate_google_oauth_scopes(
    connector: OAuthConnector,
    granted: &[String],
) -> Result<(), GoogleOAuthScopeError> {
    let required_scopes = google_required_api_scopes(connector)?;
    if connector == OAuthConnector::Gmail
        && granted
            .iter()
            .any(|scope| scope.as_str() == GMAIL_FULL_MAILBOX_SCOPE)
    {
        return Err(GoogleOAuthScopeError::FullMailboxScope);
    }

    for required in required_scopes {
        if !granted.iter().any(|scope| scope == required) {
            return Err(GoogleOAuthScopeError::MissingRequiredScope(required));
        }
    }

    Ok(())
}

fn google_hosted_profile(
    connector: OAuthConnector,
) -> Result<OAuthProfile, GoogleOAuthProfileError> {
    match connector {
        OAuthConnector::GoogleDocs | OAuthConnector::GoogleCalendar | OAuthConnector::Gmail => {
            oauth_profile(connector, OAuthHostMode::HostedAdmin)
                .ok_or(GoogleOAuthProfileError::UnsupportedConnector)
        }
        _ => Err(GoogleOAuthProfileError::UnsupportedConnector),
    }
}

fn google_required_api_scopes(
    connector: OAuthConnector,
) -> Result<&'static [&'static str], GoogleOAuthScopeError> {
    match connector {
        OAuthConnector::GoogleDocs => Ok(GOOGLE_DOCS_REQUIRED_API_SCOPES),
        OAuthConnector::GoogleCalendar => Ok(GOOGLE_CALENDAR_REQUIRED_API_SCOPES),
        OAuthConnector::Gmail => Ok(GMAIL_REQUIRED_API_SCOPES),
        _ => Err(GoogleOAuthScopeError::UnsupportedConnector),
    }
}

fn valid_google_oauth_client_id(client_id: &str) -> bool {
    !client_id.is_empty()
        && client_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
}

fn valid_google_hosted_redirect_uri(connector: OAuthConnector, redirect_uri: &str) -> bool {
    if redirect_uri.is_empty() || redirect_uri.chars().any(char::is_whitespace) {
        return false;
    }
    if redirect_uri.contains('?') || redirect_uri.contains('#') {
        return false;
    }
    let Some(after_scheme) = redirect_uri.strip_prefix("https://") else {
        return false;
    };
    let Some((authority, path)) = after_scheme.split_once('/') else {
        return false;
    };
    if authority.is_empty()
        || authority.contains('@')
        || authority.contains(':')
        || authority.chars().any(char::is_control)
    {
        return false;
    }
    if is_loopback_google_redirect_host(authority) {
        return false;
    }

    path == connector.broker_callback_path().trim_start_matches('/')
}

fn is_loopback_google_redirect_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "localhost" || host.ends_with(".localhost") || host == "127.0.0.1" || host == "0.0.0.0"
}

fn append_query_param(url: &mut String, key: &str, value: &str) {
    if !url.ends_with('?') {
        url.push('&');
    }
    url.push_str(&percent_encode_query_component(key));
    url.push('=');
    url.push_str(&percent_encode_query_component(value));
}

fn percent_encode_query_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[(byte >> 4) as usize]));
            encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
    }
    encoded
}

fn https_base_url_host(public_base_url: &str) -> Option<&str> {
    let after_scheme = public_base_url.strip_prefix("https://")?;
    let authority = after_scheme.split('/').next().unwrap_or_default();
    if authority.is_empty() || authority.chars().any(char::is_whitespace) {
        return None;
    }

    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(bracketed_host) = authority.strip_prefix('[') {
        let closing_bracket = bracketed_host.find(']')?;
        &bracketed_host[..closing_bracket]
    } else {
        authority.split(':').next().unwrap_or(authority)
    };

    if host.is_empty() { None } else { Some(host) }
}

pub fn scope_csv(scopes: &[&str]) -> String {
    scopes.join(",")
}

pub fn granted_scopes_match_exact(granted: &[String], expected: &[&str]) -> bool {
    if granted.len() != expected.len() {
        return false;
    }
    if string_slice_has_duplicates(granted) || str_slice_has_duplicates(expected) {
        return false;
    }
    expected
        .iter()
        .all(|expected_scope| granted.iter().any(|scope| scope == expected_scope))
}

fn string_slice_has_duplicates(values: &[String]) -> bool {
    values
        .iter()
        .enumerate()
        .any(|(index, value)| values[index + 1..].iter().any(|other| other == value))
}

fn str_slice_has_duplicates(values: &[&str]) -> bool {
    values
        .iter()
        .enumerate()
        .any(|(index, value)| values[index + 1..].iter().any(|other| other == value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_broker_profiles_cover_all_public_oauth_connectors() {
        let connectors = OAuthConnector::all();
        assert_eq!(
            connectors,
            &[
                OAuthConnector::Notion,
                OAuthConnector::GoogleDocs,
                OAuthConnector::GoogleCalendar,
                OAuthConnector::Gmail,
                OAuthConnector::Slack,
            ]
        );

        for connector in connectors {
            let profile = oauth_profile(*connector, OAuthHostMode::LocalBrokered)
                .expect("local broker profile");
            assert_eq!(profile.connector, *connector);
            assert_eq!(profile.host, OAuthHostMode::LocalBrokered);
            assert!(
                profile
                    .client_completion_redirect_uri
                    .starts_with("http://localhost:8757/")
            );
            assert!(profile.broker_callback_path.starts_with("/v1/oauth/"));
            assert!(profile.broker_callback_path.ends_with("/callback"));
        }
    }

    #[test]
    fn hosted_slack_profile_is_reduced_from_local_slack_profile() {
        let hosted = oauth_profile(OAuthConnector::Slack, OAuthHostMode::HostedAdmin)
            .expect("hosted Slack profile");
        assert_eq!(
            hosted.scopes,
            &[
                "channels:history",
                "channels:read",
                "files:read",
                "groups:history",
                "groups:read",
                "users:read",
            ]
        );
        assert!(!hosted.scopes.contains(&"im:read"));
        assert!(!hosted.scopes.contains(&"mpim:read"));
        assert!(!hosted.scopes.contains(&"team:read"));
        assert!(!hosted.scopes.contains(&"channels:join"));
    }

    #[test]
    fn broker_callback_uri_requires_https_base_url() {
        assert_eq!(
            broker_callback_uri("https://oauth.locality.test/", OAuthConnector::Gmail).unwrap(),
            "https://oauth.locality.test/v1/oauth/gmail/callback"
        );
        assert_eq!(
            broker_callback_uri("http://oauth.locality.test", OAuthConnector::Gmail),
            Err(OAuthProfileError::BrokerBaseUrlMustBeHttps)
        );
        assert_eq!(
            broker_callback_uri(
                "https://oauth.locality.test/path?query=1",
                OAuthConnector::Gmail
            ),
            Err(OAuthProfileError::BrokerBaseUrlMustNotContainQueryOrFragment)
        );
        assert_eq!(
            broker_callback_uri(
                "https://oauth.locality.test/path#fragment",
                OAuthConnector::Gmail
            ),
            Err(OAuthProfileError::BrokerBaseUrlMustNotContainQueryOrFragment)
        );
    }

    #[test]
    fn broker_callback_uri_rejects_empty_or_whitespace_hosts() {
        for base_url in [
            "https://",
            "https:///foo",
            "https://:443",
            "https://   /foo",
            "https://\t/foo",
        ] {
            assert_eq!(
                broker_callback_uri(base_url, OAuthConnector::Gmail),
                Err(OAuthProfileError::BrokerBaseUrlMustNotBeEmpty),
                "{base_url} must be rejected as missing a usable host"
            );
        }
    }

    #[test]
    fn google_hosted_authorization_urls_are_profile_driven_and_https() {
        let url = google_authorization_url(
            OAuthConnector::GoogleDocs,
            "google-client.apps.googleusercontent.com",
            "https://api.locality.test/v1/oauth/google-docs/callback",
            "intent.random",
        )
        .expect("authorization URL");

        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        assert!(url.contains("client_id=google-client.apps.googleusercontent.com"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains(
            "redirect_uri=https%3A%2F%2Fapi.locality.test%2Fv1%2Foauth%2Fgoogle-docs%2Fcallback"
        ));
        assert!(url.contains("scope=openid%20email%20profile%20https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fdocuments%20https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fdrive.file%20https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fdrive.metadata"));
        assert!(url.contains("state=intent.random"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("prompt=consent"));
    }

    #[test]
    fn google_hosted_authorization_rejects_wrong_callback_shape() {
        assert_eq!(
            google_authorization_url(
                OAuthConnector::Gmail,
                "google-client",
                "http://localhost:8757/oauth/gmail/callback",
                "intent.random",
            ),
            Err(GoogleOAuthProfileError::InvalidRedirectUri)
        );
        assert_eq!(
            google_authorization_url(
                OAuthConnector::Slack,
                "slack-client",
                "https://api.locality.test/v1/oauth/slack/callback",
                "intent.random",
            ),
            Err(GoogleOAuthProfileError::UnsupportedConnector)
        );
    }

    #[test]
    fn google_scope_validation_is_provider_bound() {
        let docs = [
            "openid",
            "email",
            "profile",
            "https://www.googleapis.com/auth/documents",
            "https://www.googleapis.com/auth/drive.file",
            "https://www.googleapis.com/auth/drive.metadata",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        assert_eq!(
            validate_google_oauth_scopes(OAuthConnector::GoogleDocs, &docs),
            Ok(())
        );

        let gmail_full = [
            "openid",
            "email",
            "profile",
            "https://www.googleapis.com/auth/gmail.readonly",
            "https://www.googleapis.com/auth/gmail.compose",
            "https://mail.google.com/",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        assert_eq!(
            validate_google_oauth_scopes(OAuthConnector::Gmail, &gmail_full),
            Err(GoogleOAuthScopeError::FullMailboxScope)
        );

        assert_eq!(
            validate_google_oauth_scopes(OAuthConnector::GoogleCalendar, &docs),
            Err(GoogleOAuthScopeError::MissingRequiredScope(
                "https://www.googleapis.com/auth/calendar.events"
            ))
        );
    }

    #[test]
    fn scope_csv_uses_provider_expected_order() {
        let profile = oauth_profile(OAuthConnector::Slack, OAuthHostMode::HostedAdmin)
            .expect("hosted Slack profile");
        assert_eq!(
            scope_csv(profile.scopes),
            "channels:history,channels:read,files:read,groups:history,groups:read,users:read"
        );
    }

    #[test]
    fn granted_scopes_match_exact_accepts_different_order() {
        let granted = scope_strings(&["email", "profile", "openid"]);
        assert!(granted_scopes_match_exact(
            &granted,
            &["openid", "email", "profile"]
        ));
    }

    #[test]
    fn granted_scopes_match_exact_rejects_missing_scope() {
        let granted = scope_strings(&["openid", "email"]);
        assert!(!granted_scopes_match_exact(
            &granted,
            &["openid", "email", "profile"]
        ));
    }

    #[test]
    fn granted_scopes_match_exact_rejects_extra_scope() {
        let granted = scope_strings(&["openid", "email", "profile", "calendar"]);
        assert!(!granted_scopes_match_exact(
            &granted,
            &["openid", "email", "profile"]
        ));
    }

    #[test]
    fn granted_scopes_match_exact_rejects_duplicate_scope() {
        let granted = scope_strings(&["openid", "email", "email"]);
        assert!(!granted_scopes_match_exact(
            &granted,
            &["openid", "email", "email"]
        ));
    }

    fn scope_strings(scopes: &[&str]) -> Vec<String> {
        scopes.iter().map(|scope| (*scope).to_string()).collect()
    }
}
