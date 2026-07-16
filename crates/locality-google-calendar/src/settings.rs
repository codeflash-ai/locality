use std::fmt;

use chrono::{Datelike, Duration, NaiveDate, Utc};
use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Deserializer, Serialize, de};

const DEFAULT_PAST_DAYS: i64 = 30;
const DEFAULT_FUTURE_DAYS: i64 = 180;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GoogleCalendarMountSettings {
    pub google_calendar: GoogleCalendarSettings,
}

impl Default for GoogleCalendarMountSettings {
    fn default() -> Self {
        Self {
            google_calendar: GoogleCalendarSettings::default(),
        }
    }
}

impl GoogleCalendarMountSettings {
    pub fn from_json(value: &str) -> LocalityResult<Self> {
        if value.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str::<Self>(value).map_err(|error| {
            LocalityError::Validation(vec![locality_core::validation::ValidationIssue::new(
                "google_calendar_mount_settings_invalid",
                std::path::PathBuf::new(),
                Some(1),
                format!("Google Calendar mount settings JSON is invalid: {error}"),
                Some("remount Google Calendar with valid --after/--before options".to_string()),
            )])
        })
    }

    pub fn to_json(&self) -> LocalityResult<String> {
        serde_json::to_string(self).map_err(|error| {
            LocalityError::Io(format!("google calendar settings encode failed: {error}"))
        })
    }

    pub fn with_date_window(after: &str, before: &str) -> LocalityResult<Self> {
        Ok(Self {
            google_calendar: GoogleCalendarSettings {
                date_window: Some(GoogleCalendarDateWindow::new(after, before)?),
            },
        })
    }

    pub fn effective_date_window(&self) -> GoogleCalendarDateWindow {
        self.google_calendar
            .date_window
            .clone()
            .unwrap_or_else(GoogleCalendarDateWindow::default_for_now)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GoogleCalendarSettings {
    pub date_window: Option<GoogleCalendarDateWindow>,
}

impl Default for GoogleCalendarSettings {
    fn default() -> Self {
        Self { date_window: None }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GoogleCalendarDateWindow {
    after: GoogleCalendarDate,
    before: GoogleCalendarDate,
}

impl<'de> Deserialize<'de> for GoogleCalendarDateWindow {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawGoogleCalendarDateWindow {
            after: String,
            before: String,
        }

        let raw = RawGoogleCalendarDateWindow::deserialize(deserializer)?;
        Self::new(&raw.after, &raw.before).map_err(|error| de::Error::custom(error_message(error)))
    }
}

impl GoogleCalendarDateWindow {
    pub fn new(after: &str, before: &str) -> LocalityResult<Self> {
        let after = GoogleCalendarDate::parse(after)?;
        let before = GoogleCalendarDate::parse(before)?;
        if before <= after {
            return Err(settings_validation(
                "google_calendar_mount_date_window_order",
                "`--before` must be later than `--after`",
                "choose a before date after the after date",
            ));
        }
        Ok(Self { after, before })
    }

    pub fn default_for_unix_day(unix_day: i64) -> Self {
        let day = unix_epoch()
            .checked_add_signed(Duration::days(unix_day))
            .expect("unix day within chrono range");
        let after = day
            .checked_sub_signed(Duration::days(DEFAULT_PAST_DAYS))
            .expect("default after date within chrono range");
        let before = day
            .checked_add_signed(Duration::days(DEFAULT_FUTURE_DAYS))
            .expect("default before date within chrono range");
        Self {
            after: GoogleCalendarDate::from_naive(after),
            before: GoogleCalendarDate::from_naive(before),
        }
    }

    pub fn default_for_now() -> Self {
        let today = Utc::now().date_naive();
        let epoch = unix_epoch();
        let unix_day = today.signed_duration_since(epoch).num_days();
        Self::default_for_unix_day(unix_day)
    }

    pub fn after(&self) -> &GoogleCalendarDate {
        &self.after
    }

    pub fn before(&self) -> &GoogleCalendarDate {
        &self.before
    }

    pub fn time_min_rfc3339(&self) -> String {
        self.after.rfc3339_midnight()
    }

    pub fn time_max_rfc3339(&self) -> String {
        self.before.rfc3339_midnight()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct GoogleCalendarDate(String);

impl<'de> Deserialize<'de> for GoogleCalendarDate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(|error| de::Error::custom(error_message(error)))
    }
}

impl GoogleCalendarDate {
    pub fn parse(value: &str) -> LocalityResult<Self> {
        let bytes = value.as_bytes();
        let valid = bytes.len() == 10
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes
                .iter()
                .enumerate()
                .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit());
        if !valid {
            return Err(settings_validation(
                "google_calendar_mount_date_invalid",
                format!("Google Calendar date `{value}` must use YYYY-MM-DD"),
                "use a date such as 2026-07-15",
            ));
        }
        let year = value[0..4].parse::<i32>().unwrap_or(0);
        let month = value[5..7].parse::<u32>().unwrap_or(0);
        let day = value[8..10].parse::<u32>().unwrap_or(0);
        if year == 0 || NaiveDate::from_ymd_opt(year, month, day).is_none() {
            return Err(settings_validation(
                "google_calendar_mount_date_invalid",
                format!("Google Calendar date `{value}` is not a calendar date"),
                "use a valid calendar date",
            ));
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn from_naive(date: NaiveDate) -> Self {
        Self(format!(
            "{:04}-{:02}-{:02}",
            date.year(),
            date.month(),
            date.day()
        ))
    }

    fn rfc3339_midnight(&self) -> String {
        format!("{}T00:00:00Z", self.0)
    }
}

impl fmt::Display for GoogleCalendarDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn unix_epoch() -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid unix epoch")
}

fn settings_validation(
    code: &'static str,
    message: impl Into<String>,
    suggestion: &'static str,
) -> LocalityError {
    LocalityError::Validation(vec![locality_core::validation::ValidationIssue::new(
        code,
        std::path::PathBuf::new(),
        Some(1),
        message,
        Some(suggestion.to_string()),
    )])
}

fn error_message(error: LocalityError) -> String {
    match error {
        LocalityError::Validation(issues) => issues
            .into_iter()
            .map(|issue| issue.message)
            .collect::<Vec<_>>()
            .join("; "),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{GoogleCalendarDateWindow, GoogleCalendarMountSettings, GoogleCalendarSettings};

    #[test]
    fn explicit_date_window_serializes_to_mount_settings_json() {
        let settings = GoogleCalendarMountSettings::with_date_window("2026-07-01", "2026-07-31")
            .expect("valid date window");

        assert_eq!(
            settings.to_json().expect("json"),
            r#"{"google_calendar":{"date_window":{"after":"2026-07-01","before":"2026-07-31"}}}"#
        );
        assert_eq!(
            settings
                .google_calendar
                .date_window
                .as_ref()
                .expect("window")
                .time_min_rfc3339(),
            "2026-07-01T00:00:00Z"
        );
        assert_eq!(
            settings
                .google_calendar
                .date_window
                .as_ref()
                .expect("window")
                .time_max_rfc3339(),
            "2026-07-31T00:00:00Z"
        );
    }

    #[test]
    fn default_date_window_uses_thirty_days_back_and_one_hundred_eighty_days_forward() {
        let window = GoogleCalendarDateWindow::default_for_unix_day(20650);

        assert_eq!(window.after().as_str(), "2026-06-16");
        assert_eq!(window.before().as_str(), "2027-01-12");
        assert_eq!(window.time_min_rfc3339(), "2026-06-16T00:00:00Z");
        assert_eq!(window.time_max_rfc3339(), "2027-01-12T00:00:00Z");
    }

    #[test]
    fn default_settings_store_no_date_window_and_effective_window_uses_current_default() {
        let settings = GoogleCalendarMountSettings::default();

        assert_eq!(GoogleCalendarSettings::default().date_window, None);
        assert_eq!(settings.google_calendar.date_window, None);
    }

    #[test]
    fn date_window_rejects_partial_invalid_and_reversed_dates() {
        assert!(GoogleCalendarMountSettings::with_date_window("2026/07/01", "2026-07-31").is_err());
        assert!(GoogleCalendarMountSettings::with_date_window("2026-02-30", "2026-07-31").is_err());
        assert!(GoogleCalendarMountSettings::with_date_window("2026-07-31", "2026-07-31").is_err());
        assert!(GoogleCalendarMountSettings::with_date_window("2026-08-01", "2026-07-31").is_err());
    }
}
