use std::fmt;

use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GmailMountSettings {
    pub gmail: GmailSettings,
}

impl Default for GmailMountSettings {
    fn default() -> Self {
        Self {
            gmail: GmailSettings::default(),
        }
    }
}

impl GmailMountSettings {
    pub fn from_json(value: &str) -> LocalityResult<Self> {
        if value.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str::<Self>(value).map_err(|error| {
            LocalityError::Validation(vec![locality_core::validation::ValidationIssue::new(
                "gmail_mount_settings_invalid",
                std::path::PathBuf::new(),
                Some(1),
                format!("Gmail mount settings JSON is invalid: {error}"),
                Some("remount Gmail with valid --after/--before/--view options".to_string()),
            )])
        })
    }

    pub fn to_json(&self) -> LocalityResult<String> {
        serde_json::to_string(self)
            .map_err(|error| LocalityError::Io(format!("gmail settings encode failed: {error}")))
    }

    pub fn with_date_window(after: &str, before: &str) -> LocalityResult<Self> {
        Ok(Self {
            gmail: GmailSettings {
                date_window: Some(GmailDateWindow::new(after, before)?),
                view: GmailProjectionView::Messages,
            },
        })
    }

    pub fn with_view(mut self, view: GmailProjectionView) -> Self {
        self.gmail.view = view;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GmailSettings {
    pub date_window: Option<GmailDateWindow>,
    pub view: GmailProjectionView,
}

impl Default for GmailSettings {
    fn default() -> Self {
        Self {
            date_window: None,
            view: GmailProjectionView::Messages,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GmailProjectionView {
    Messages,
    Threads,
}

impl Default for GmailProjectionView {
    fn default() -> Self {
        Self::Messages
    }
}

impl GmailProjectionView {
    pub fn parse(value: &str) -> LocalityResult<Self> {
        match value {
            "messages" => Ok(Self::Messages),
            "threads" => Ok(Self::Threads),
            other => Err(settings_validation(
                "gmail_mount_view_invalid",
                format!("unsupported Gmail view `{other}`"),
                "use `--view messages` or `--view threads`",
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Messages => "messages",
            Self::Threads => "threads",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GmailDateWindow {
    pub after: GmailSearchDate,
    pub before: GmailSearchDate,
}

impl GmailDateWindow {
    pub fn new(after: &str, before: &str) -> LocalityResult<Self> {
        let after = GmailSearchDate::parse(after)?;
        let before = GmailSearchDate::parse(before)?;
        if before <= after {
            return Err(settings_validation(
                "gmail_mount_date_window_order",
                "`--before` must be later than `--after`",
                "choose a before date after the after date",
            ));
        }
        Ok(Self { after, before })
    }

    pub fn query(&self) -> String {
        format!(
            "after:{} before:{}",
            self.after.gmail_query_date(),
            self.before.gmail_query_date()
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct GmailSearchDate(String);

impl GmailSearchDate {
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
                "gmail_mount_date_invalid",
                format!("Gmail date `{value}` must use YYYY-MM-DD"),
                "use a date such as 2026-07-15",
            ));
        }
        let year = value[0..4].parse::<u32>().unwrap_or(0);
        let month = value[5..7].parse::<u32>().unwrap_or(0);
        let day = value[8..10].parse::<u32>().unwrap_or(0);
        if !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
            return Err(settings_validation(
                "gmail_mount_date_invalid",
                format!("Gmail date `{value}` is not a calendar date"),
                "use a valid calendar date",
            ));
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn gmail_query_date(&self) -> String {
        self.0.replace('-', "/")
    }
}

impl fmt::Display for GmailSearchDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn leap_year(year: u32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
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

#[cfg(test)]
mod tests {
    use super::{GmailMountSettings, GmailProjectionView, GmailSearchDate};

    #[test]
    fn default_settings_keep_message_view_without_date_window() {
        let settings = GmailMountSettings::from_json("{}").expect("settings");

        assert_eq!(settings.gmail.date_window, None);
        assert_eq!(settings.gmail.view, GmailProjectionView::Messages);
    }

    #[test]
    fn settings_serialize_date_window_and_thread_view() {
        let settings = GmailMountSettings::with_date_window("2026-07-01", "2026-07-15")
            .expect("date window")
            .with_view(GmailProjectionView::Threads);

        let json = settings.to_json().expect("json");
        let parsed = GmailMountSettings::from_json(&json).expect("parsed json");

        assert_eq!(parsed.gmail.view, GmailProjectionView::Threads);
        assert_eq!(
            parsed.gmail.date_window.as_ref().expect("window").query(),
            "after:2026/07/01 before:2026/07/15"
        );
    }

    #[test]
    fn date_window_rejects_invalid_or_reversed_dates() {
        assert!(GmailSearchDate::parse("2026-02-29").is_err());
        assert!(GmailSearchDate::parse("2024-02-29").is_ok());
        assert!(GmailMountSettings::with_date_window("2026-07-15", "2026-07-01").is_err());
    }

    #[test]
    fn view_parser_accepts_only_known_views() {
        assert_eq!(
            GmailProjectionView::parse("messages").expect("messages"),
            GmailProjectionView::Messages
        );
        assert_eq!(
            GmailProjectionView::parse("threads").expect("threads"),
            GmailProjectionView::Threads
        );
        assert!(GmailProjectionView::parse("conversation").is_err());
    }
}
