use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};

pub const DEFAULT_SLACK_RECENT_LIMIT: u32 = 15;
pub const SLACK_PUBLIC_CHANNEL_TYPES: &str = "public_channel";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SlackMountSettings {
    pub recent_limit: u32,
    pub conversation_types: String,
}

impl Default for SlackMountSettings {
    fn default() -> Self {
        Self {
            recent_limit: DEFAULT_SLACK_RECENT_LIMIT,
            conversation_types: SLACK_PUBLIC_CHANNEL_TYPES.to_string(),
        }
    }
}

impl SlackMountSettings {
    pub fn from_json(value: &str) -> LocalityResult<Self> {
        if value.trim().is_empty() {
            return Ok(Self::default());
        }
        let settings = serde_json::from_str::<Self>(value).map_err(|error| {
            settings_validation(
                "slack_mount_settings_invalid",
                format!("Slack mount settings JSON is invalid: {error}"),
                "remount Slack with valid settings",
            )
        })?;
        settings.validate()?;
        Ok(settings)
    }

    pub fn to_json(&self) -> LocalityResult<String> {
        self.validate()?;
        serde_json::to_string(self)
            .map_err(|error| LocalityError::Io(format!("slack settings encode failed: {error}")))
    }

    pub fn validate(&self) -> LocalityResult<()> {
        if !(1..=DEFAULT_SLACK_RECENT_LIMIT).contains(&self.recent_limit) {
            return Err(settings_validation(
                "slack_recent_limit_invalid",
                "Slack recent_limit must be between 1 and 15".to_string(),
                "use a recent_limit from 1 to 15",
            ));
        }
        if self.conversation_types != SLACK_PUBLIC_CHANNEL_TYPES {
            return Err(settings_validation(
                "slack_conversation_types_invalid",
                "Slack V1 supports public channels only".to_string(),
                "use conversation_types \"public_channel\"",
            ));
        }
        Ok(())
    }
}

fn settings_validation(
    code: &'static str,
    message: String,
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
