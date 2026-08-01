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
#[serde(deny_unknown_fields)]
pub struct SlackMountSettings {
    #[serde(default)]
    pub slack: SlackSettings,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlackSettings {
    #[serde(default = "default_history_limit")]
    pub history_limit: u32,
    #[serde(default = "default_conversation_types")]
    pub types: BTreeSet<SlackConversationType>,
    #[serde(default = "default_auto_join_public_channels")]
    pub auto_join_public_channels: bool,
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
            auto_join_public_channels: true,
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
        if !(1..=MAX_SLACK_HISTORY_LIMIT).contains(&self.slack.history_limit) {
            return Err(settings_validation(format!(
                "Slack history_limit must be between 1 and {MAX_SLACK_HISTORY_LIMIT}"
            )));
        }
        if self.slack.types.is_empty() {
            return Err(settings_validation(
                "Slack settings must include at least one Slack conversation type",
            ));
        }
        if !self
            .slack
            .types
            .contains(&SlackConversationType::PublicChannel)
        {
            self.slack.auto_join_public_channels = false;
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

fn default_auto_join_public_channels() -> bool {
    true
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
        assert!(settings.slack.auto_join_public_channels);
    }

    #[test]
    fn omitted_auto_join_uses_the_documented_default() {
        let settings = SlackMountSettings::from_json(
            r#"{"slack":{"history_limit":15,"types":["public_channel","im"]}}"#,
        )
        .expect("parse settings");

        assert_eq!(settings.slack.history_limit, 15);
        assert!(settings.slack.auto_join_public_channels);
        assert_eq!(
            settings.conversations_api_types(),
            "public_channel,im".to_string()
        );
    }

    #[test]
    fn explicit_false_disables_public_channel_auto_join() {
        let settings = SlackMountSettings::from_json(
            r#"{"slack":{"types":["public_channel"],"auto_join_public_channels":false}}"#,
        )
        .expect("parse settings");

        assert!(!settings.slack.auto_join_public_channels);
        let encoded = settings.to_json().expect("settings json");
        assert_eq!(
            encoded,
            r#"{"slack":{"history_limit":15,"types":["public_channel"],"auto_join_public_channels":false}}"#
        );
        let reparsed = SlackMountSettings::from_json(&encoded).expect("reparse settings");
        assert!(!reparsed.slack.auto_join_public_channels);
    }

    #[test]
    fn rejects_history_limits_outside_the_manifest_schema() {
        for history_limit in [0, 16, 50] {
            let error = SlackMountSettings::from_json(&format!(
                r#"{{"slack":{{"history_limit":{history_limit}}}}}"#
            ))
            .expect_err("out-of-range history limit rejected");
            let LocalityError::Validation(issues) = error else {
                panic!("expected settings validation error");
            };
            assert_eq!(issues.len(), 1);
            assert!(issues[0].message.contains("between 1 and 15"));
        }
    }

    #[test]
    fn rejects_unknown_settings_fields() {
        assert!(SlackMountSettings::from_json(r#"{"unexpected":true}"#).is_err());
        assert!(SlackMountSettings::from_json(r#"{"slack":{"unexpected":true}}"#).is_err());
    }

    #[test]
    fn derives_auto_join_from_public_channel_type() {
        let settings = SlackMountSettings::from_json(
            r#"{"slack":{"types":["im"],"auto_join_public_channels":true}}"#,
        )
        .expect("parse settings");

        assert!(!settings.slack.auto_join_public_channels);
        assert_eq!(
            settings.to_json().expect("settings json"),
            r#"{"slack":{"history_limit":15,"types":["im"],"auto_join_public_channels":false}}"#
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
