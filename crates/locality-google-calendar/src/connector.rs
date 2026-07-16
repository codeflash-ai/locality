#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoogleCalendarConfig {
    pub access_token: String,
}

#[derive(Clone, Debug)]
pub struct GoogleCalendarConnector {
    config: GoogleCalendarConfig,
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
