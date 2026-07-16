use std::fmt;

#[derive(Clone, PartialEq, Eq)]
pub struct GoogleCalendarConfig {
    pub access_token: String,
}

impl fmt::Debug for GoogleCalendarConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GoogleCalendarConfig")
            .field("access_token", &"<redacted>")
            .finish()
    }
}

#[derive(Clone)]
pub struct GoogleCalendarConnector {
    config: GoogleCalendarConfig,
}

impl fmt::Debug for GoogleCalendarConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GoogleCalendarConnector")
            .field("config", &self.config)
            .finish()
    }
}

impl GoogleCalendarConfig {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
        }
    }
}

impl GoogleCalendarConnector {
    pub fn new(config: GoogleCalendarConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &GoogleCalendarConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::{GoogleCalendarConfig, GoogleCalendarConnector};

    #[test]
    fn config_debug_redacts_access_token() {
        let debug = format!("{:?}", GoogleCalendarConfig::new("secret-access-token"));

        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("secret-access-token"));
    }

    #[test]
    fn connector_debug_redacts_access_token() {
        let connector = GoogleCalendarConnector::new(GoogleCalendarConfig::new(
            "connector-secret-access-token",
        ));

        let debug = format!("{connector:?}");

        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("connector-secret-access-token"));
    }
}
