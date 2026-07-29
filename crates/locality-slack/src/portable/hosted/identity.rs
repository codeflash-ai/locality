use std::fmt::{Debug, Display, Formatter};

use locality_protocol::SlackInstallationId;
use serde::{Deserialize, Serialize};

pub const MAX_HOSTED_SLACK_ID_BYTES: usize = 32;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedSlackInstallationBinding {
    pub installation_id: SlackInstallationId,
    pub api_app_id: String,
    pub team_id: String,
    pub enterprise_id: Option<String>,
    pub enterprise_install: bool,
    pub bot_user_id: String,
    pub oauth_subject_id: String,
}

impl HostedSlackInstallationBinding {
    pub fn validate(&self) -> Result<(), HostedSlackPortableError> {
        validate_slack_id("api_app_id", &self.api_app_id, b"A")?;
        validate_slack_id("team_id", &self.team_id, b"T")?;
        if self.enterprise_install {
            return Err(HostedSlackPortableError::EnterpriseInstallUnsupported);
        }
        if let Some(enterprise_id) = &self.enterprise_id {
            validate_slack_id("enterprise_id", enterprise_id, b"E")?;
        }
        validate_slack_id("bot_user_id", &self.bot_user_id, b"UW")?;
        validate_slack_id("oauth_subject_id", &self.oauth_subject_id, b"UW")?;
        Ok(())
    }

    pub fn verify_observed_identity(
        &self,
        observed: &HostedSlackObservedInstallationIdentity,
    ) -> Result<(), HostedSlackPortableError> {
        self.validate()?;
        observed.validate()?;
        for (field, matches) in [
            ("api_app_id", self.api_app_id == observed.api_app_id),
            ("team_id", self.team_id == observed.team_id),
            (
                "enterprise_id",
                self.enterprise_id == observed.enterprise_id,
            ),
            (
                "enterprise_install",
                self.enterprise_install == observed.enterprise_install,
            ),
            ("bot_user_id", self.bot_user_id == observed.bot_user_id),
            (
                "oauth_subject_id",
                self.oauth_subject_id == observed.oauth_subject_id,
            ),
        ] {
            if !matches {
                return Err(HostedSlackPortableError::IdentityMismatch(field));
            }
        }
        Ok(())
    }
}

impl Debug for HostedSlackInstallationBinding {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostedSlackInstallationBinding")
            .field("installation_id", &self.installation_id)
            .field("api_app_id", &self.api_app_id)
            .field("team_id", &self.team_id)
            .field("enterprise_id", &self.enterprise_id)
            .field("enterprise_install", &self.enterprise_install)
            .field("bot_user_id", &self.bot_user_id)
            .field("oauth_subject_id", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedSlackObservedInstallationIdentity {
    pub api_app_id: String,
    pub team_id: String,
    pub enterprise_id: Option<String>,
    pub enterprise_install: bool,
    pub bot_user_id: String,
    pub oauth_subject_id: String,
}

impl HostedSlackObservedInstallationIdentity {
    pub fn validate(&self) -> Result<(), HostedSlackPortableError> {
        validate_slack_id("api_app_id", &self.api_app_id, b"A")?;
        validate_slack_id("team_id", &self.team_id, b"T")?;
        if self.enterprise_install {
            return Err(HostedSlackPortableError::EnterpriseInstallUnsupported);
        }
        if let Some(enterprise_id) = &self.enterprise_id {
            validate_slack_id("enterprise_id", enterprise_id, b"E")?;
        }
        validate_slack_id("bot_user_id", &self.bot_user_id, b"UW")?;
        validate_slack_id("oauth_subject_id", &self.oauth_subject_id, b"UW")?;
        Ok(())
    }
}

impl Debug for HostedSlackObservedInstallationIdentity {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostedSlackObservedInstallationIdentity")
            .field("api_app_id", &self.api_app_id)
            .field("team_id", &self.team_id)
            .field("enterprise_id", &self.enterprise_id)
            .field("enterprise_install", &self.enterprise_install)
            .field("bot_user_id", &self.bot_user_id)
            .field("oauth_subject_id", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostedSlackPortableError {
    EmptyField(&'static str),
    InvalidSlackId(&'static str),
    ValueTooLong {
        field: &'static str,
        maximum_bytes: usize,
        actual_bytes: usize,
    },
    InvalidTimestamp(&'static str),
    CollectionTooLarge {
        field: &'static str,
        maximum: usize,
        actual: usize,
    },
    DuplicateValue(&'static str),
    EnterpriseInstallUnsupported,
    IdentityMismatch(&'static str),
    RawInputTooLarge {
        maximum_bytes: usize,
        actual_bytes: usize,
    },
    InvalidRawJson,
    MissingReference(&'static str),
    InvalidRelationship(&'static str),
    DuplicateReference(&'static str),
}

impl Display for HostedSlackPortableError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyField(field) => write!(formatter, "{field} must not be empty"),
            Self::InvalidSlackId(field) => {
                write!(formatter, "{field} must be a canonical Slack identifier")
            }
            Self::ValueTooLong {
                field,
                maximum_bytes,
                actual_bytes,
            } => write!(
                formatter,
                "{field} is {actual_bytes} bytes, exceeding {maximum_bytes} bytes"
            ),
            Self::InvalidTimestamp(field) => {
                write!(formatter, "{field} must be a canonical Slack timestamp")
            }
            Self::CollectionTooLarge {
                field,
                maximum,
                actual,
            } => write!(
                formatter,
                "{field} has {actual} entries, exceeding {maximum} entries"
            ),
            Self::DuplicateValue(field) => write!(formatter, "{field} must be unique"),
            Self::EnterpriseInstallUnsupported => {
                formatter.write_str("enterprise-wide Slack installs are unsupported in V1")
            }
            Self::IdentityMismatch(field) => {
                write!(
                    formatter,
                    "observed Slack installation {field} does not match binding"
                )
            }
            Self::RawInputTooLarge {
                maximum_bytes,
                actual_bytes,
            } => write!(
                formatter,
                "raw hosted Slack snapshot is {actual_bytes} bytes, exceeding {maximum_bytes} bytes"
            ),
            Self::InvalidRawJson => {
                formatter.write_str("raw hosted Slack snapshot JSON is invalid")
            }
            Self::MissingReference(relation) => {
                write!(
                    formatter,
                    "hosted Slack snapshot has a missing {relation} reference"
                )
            }
            Self::InvalidRelationship(relation) => {
                write!(
                    formatter,
                    "hosted Slack snapshot has an invalid {relation} relationship"
                )
            }
            Self::DuplicateReference(relation) => {
                write!(
                    formatter,
                    "hosted Slack snapshot repeats a {relation} reference"
                )
            }
        }
    }
}

impl std::error::Error for HostedSlackPortableError {}

pub(crate) fn validate_bounded_text(
    field: &'static str,
    value: &str,
    maximum_bytes: usize,
) -> Result<(), HostedSlackPortableError> {
    if value.len() > maximum_bytes {
        return Err(HostedSlackPortableError::ValueTooLong {
            field,
            maximum_bytes,
            actual_bytes: value.len(),
        });
    }
    Ok(())
}

pub(crate) fn validate_collection_len(
    field: &'static str,
    actual: usize,
    maximum: usize,
) -> Result<(), HostedSlackPortableError> {
    if actual > maximum {
        return Err(HostedSlackPortableError::CollectionTooLarge {
            field,
            maximum,
            actual,
        });
    }
    Ok(())
}

pub(crate) fn validate_slack_id(
    field: &'static str,
    value: &str,
    prefixes: &[u8],
) -> Result<(), HostedSlackPortableError> {
    if value.is_empty() {
        return Err(HostedSlackPortableError::EmptyField(field));
    }
    if value.len() > MAX_HOSTED_SLACK_ID_BYTES {
        return Err(HostedSlackPortableError::ValueTooLong {
            field,
            maximum_bytes: MAX_HOSTED_SLACK_ID_BYTES,
            actual_bytes: value.len(),
        });
    }
    let bytes = value.as_bytes();
    if bytes.len() < 2
        || !prefixes.contains(&bytes[0])
        || !bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
    {
        return Err(HostedSlackPortableError::InvalidSlackId(field));
    }
    Ok(())
}
