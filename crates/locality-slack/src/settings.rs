use std::collections::BTreeSet;
use std::path::PathBuf;

use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};

const DEFAULT_SLACK_HISTORY_LIMIT: u32 = 15;
const MAX_SLACK_HISTORY_LIMIT: u32 = 15;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlackConversationType {
    PublicChannel,
    PrivateChannel,
    Im,
    Mpim,
}

impl SlackConversationType {
    pub fn conversations_api_value(&self) -> &'static str {
        match self {
            Self::PublicChannel => "public_channel",
            Self::PrivateChannel => "private_channel",
            Self::Im => "im",
            Self::Mpim => "mpim",
        }
    }

    pub fn root_folder(&self) -> &'static str {
        match self {
            Self::PublicChannel => "channels",
            Self::PrivateChannel => "private-channels",
            Self::Im => "dms",
            Self::Mpim => "group-dms",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackMountSettings {
    #[serde(default)]
    pub slack: SlackSettings,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackSettings {
    #[serde(default = "default_history_limit")]
    pub history_limit: u32,
    #[serde(default = "default_conversation_types")]
    pub types: BTreeSet<SlackConversationType>,
}

impl Default for SlackMountSettings {
    fn default() -> Self {
        Self {
            slack: SlackSettings::default(),
        }
    }
}

impl Default for SlackSettings {
    fn default() -> Self {
        Self {
            history_limit: DEFAULT_SLACK_HISTORY_LIMIT,
            types: default_conversation_types(),
        }
    }
}

impl SlackMountSettings {
    pub fn from_json(value: &str) -> LocalityResult<Self> {
        let mut parsed = if value.trim().is_empty() {
            Self::default()
        } else {
            serde_json::from_str::<Self>(value).map_err(|error| {
                settings_validation(format!("Slack mount settings are invalid JSON: {error}"))
            })?
        };
        parsed.normalize()?;
        Ok(parsed)
    }

    pub fn to_json(&self) -> LocalityResult<String> {
        serde_json::to_string(self).map_err(|error| {
            LocalityError::Io(format!("Slack mount settings encode failed: {error}"))
        })
    }

    pub fn conversations_api_types(&self) -> String {
        self.slack
            .types
            .iter()
            .map(SlackConversationType::conversations_api_value)
            .collect::<Vec<_>>()
            .join(",")
    }

    fn normalize(&mut self) -> LocalityResult<()> {
        self.slack.history_limit = self.slack.history_limit.clamp(1, MAX_SLACK_HISTORY_LIMIT);
        if self.slack.types.is_empty() {
            return Err(settings_validation(
                "Slack settings must include at least one Slack conversation type",
            ));
        }
        Ok(())
    }
}

fn default_history_limit() -> u32 {
    DEFAULT_SLACK_HISTORY_LIMIT
}

fn default_conversation_types() -> BTreeSet<SlackConversationType> {
    [
        SlackConversationType::PublicChannel,
        SlackConversationType::PrivateChannel,
        SlackConversationType::Im,
        SlackConversationType::Mpim,
    ]
    .into_iter()
    .collect()
}

fn settings_validation(message: impl Into<String>) -> LocalityError {
    LocalityError::Validation(vec![locality_core::validation::ValidationIssue::new(
        "slack_mount_settings_invalid",
        PathBuf::new(),
        Some(1),
        message,
        Some("remount Slack with valid slack.history_limit and slack.types settings".to_string()),
    )])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_are_conservative() {
        let settings = SlackMountSettings::default();

        assert_eq!(settings.slack.history_limit, 15);
        assert!(
            settings
                .slack
                .types
                .contains(&SlackConversationType::PublicChannel)
        );
        assert!(
            settings
                .slack
                .types
                .contains(&SlackConversationType::PrivateChannel)
        );
        assert!(settings.slack.types.contains(&SlackConversationType::Im));
        assert!(settings.slack.types.contains(&SlackConversationType::Mpim));
    }

    #[test]
    fn parses_json_settings_with_clamped_history_limit() {
        let settings = SlackMountSettings::from_json(
            r#"{"slack":{"history_limit":50,"types":["public_channel","im"]}}"#,
        )
        .expect("parse settings");

        assert_eq!(settings.slack.history_limit, 15);
        assert_eq!(
            settings.conversations_api_types(),
            "public_channel,im".to_string()
        );
    }

    #[test]
    fn rejects_empty_conversation_type_list() {
        let error = SlackMountSettings::from_json(r#"{"slack":{"types":[]}}"#)
            .expect_err("empty type list rejected");

        let LocalityError::Validation(issues) = error else {
            panic!("expected validation error");
        };
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0]
                .message
                .contains("at least one Slack conversation type")
        );
    }
}
