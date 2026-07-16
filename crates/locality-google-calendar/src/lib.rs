pub mod client;
pub mod connector;
pub mod dto;
pub mod oauth;
pub mod render;
pub mod settings;

pub use connector::{GoogleCalendarConfig, GoogleCalendarConnector};
pub use oauth::{
    DEFAULT_GOOGLE_CALENDAR_OAUTH_BROKER_URL, DEFAULT_GOOGLE_CALENDAR_OAUTH_REDIRECT_URI,
    GOOGLE_CALENDAR_CONNECTOR_ID, GOOGLE_CALENDAR_OAUTH_SCOPES,
    HttpGoogleCalendarOAuthBrokerClient, StoredGoogleCalendarCredential,
};
pub use settings::{
    GoogleCalendarDate, GoogleCalendarDateWindow, GoogleCalendarMountSettings,
    GoogleCalendarSettings,
};

#[cfg(test)]
mod tests {
    use super::{GoogleCalendarDate, GoogleCalendarSettings};

    #[test]
    fn settings_reexports_include_date_and_settings_types() {
        let date = GoogleCalendarDate::parse("2026-07-01").expect("valid date");
        let settings = GoogleCalendarSettings::default();

        assert_eq!(date.as_str(), "2026-07-01");
        assert_eq!(settings.date_window, None);
    }
}
