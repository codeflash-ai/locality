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

pub const GOOGLE_IDENTITY_SCOPES: &[&str] = &["openid", "email", "profile"];

pub const NOTION_LOCAL_BROKER_SCOPES: &[&str] = &[];
pub const NOTION_HOSTED_ADMIN_SCOPES: &[&str] = &[];

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
    Ok(format!(
        "{}{}",
        trimmed.trim_end_matches('/'),
        connector.broker_callback_path()
    ))
}

pub fn scope_csv(scopes: &[&str]) -> String {
    scopes.join(",")
}

pub fn granted_scopes_match_exact(granted: &[String], expected: &[&str]) -> bool {
    if granted.len() != expected.len() {
        return false;
    }
    expected
        .iter()
        .all(|expected_scope| granted.iter().any(|scope| scope == expected_scope))
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
}
