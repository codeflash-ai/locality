use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};

use locality_core::portable::LogicalPath;
use serde::Serialize;

use super::native::{HostedSlackNativeSnapshot, MAX_HOSTED_SLACK_COLLECTION_ENTRIES};

/// Fixture-pinned logical-path format proposed by ADR 0004.
///
/// This constant identifies only V1; it does not promise compatibility with an
/// unreviewed later format.
pub const HOSTED_SLACK_LOGICAL_PATH_FORMAT_VERSION_V1: u16 = 1;
pub const MAX_HOSTED_SLACK_SLUG_BYTES_V1: usize = 64;
pub const MAX_HOSTED_SLACK_PATH_COMPONENT_BYTES_V1: usize = 255;
pub const MAX_HOSTED_SLACK_LOGICAL_PATH_BYTES_V1: usize = 1024;
pub const MAX_HOSTED_SLACK_PROJECTION_DOCUMENTS_V1: usize =
    1 + (2 * MAX_HOSTED_SLACK_COLLECTION_ENTRIES);

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostedSlackThreadPathV1 {
    pub root_message_id: String,
    pub logical_path: LogicalPath,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostedSlackFilePathV1 {
    pub file_id: String,
    pub logical_path: LogicalPath,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostedSlackLogicalPathsV1 {
    pub path_format_version: u16,
    pub channel: LogicalPath,
    pub threads: Vec<HostedSlackThreadPathV1>,
    pub files: Vec<HostedSlackFilePathV1>,
    #[serde(skip)]
    channel_directory: String,
}

impl HostedSlackLogicalPathsV1 {
    pub fn channel_directory(&self) -> &str {
        &self.channel_directory
    }

    pub fn thread_path(&self, root_message_id: &str) -> Option<&LogicalPath> {
        self.threads
            .binary_search_by(|entry| entry.root_message_id.as_str().cmp(root_message_id))
            .ok()
            .map(|index| &self.threads[index].logical_path)
    }

    pub fn file_path(&self, file_id: &str) -> Option<&LogicalPath> {
        self.files
            .binary_search_by(|entry| entry.file_id.as_str().cmp(file_id))
            .ok()
            .map(|index| &self.files[index].logical_path)
    }

    pub fn channel_relative_path<'a>(&self, path: &'a LogicalPath) -> Option<&'a str> {
        path.as_str()
            .strip_prefix(&self.channel_directory)
            .and_then(|relative| relative.strip_prefix('/'))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostedSlackPathError {
    MissingRootMessage(String),
    TooManyDocuments {
        maximum: usize,
        actual: usize,
    },
    ComponentTooLong {
        component: String,
        maximum_bytes: usize,
        actual_bytes: usize,
    },
    LogicalPathTooLong {
        maximum_bytes: usize,
        actual_bytes: usize,
    },
    UnsupportedLogicalPath(String),
    PathCollision {
        first: String,
        second: String,
    },
}

impl Display for HostedSlackPathError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingRootMessage(root_message_id) => {
                write!(formatter, "thread root {root_message_id} has no message")
            }
            Self::TooManyDocuments { maximum, actual } => write!(
                formatter,
                "hosted Slack projection has {actual} documents, exceeding {maximum}"
            ),
            Self::ComponentTooLong {
                component,
                maximum_bytes,
                actual_bytes,
            } => write!(
                formatter,
                "logical-path component {component:?} is {actual_bytes} bytes, exceeding {maximum_bytes}"
            ),
            Self::LogicalPathTooLong {
                maximum_bytes,
                actual_bytes,
            } => write!(
                formatter,
                "hosted Slack logical path is {actual_bytes} bytes, exceeding {maximum_bytes}"
            ),
            Self::UnsupportedLogicalPath(path) => {
                write!(
                    formatter,
                    "hosted Slack generated unsupported logical path {path:?}"
                )
            }
            Self::PathCollision { first, second } => write!(
                formatter,
                "hosted Slack logical paths collide: {first:?} and {second:?}"
            ),
        }
    }
}

impl std::error::Error for HostedSlackPathError {}

pub fn build_hosted_slack_logical_paths_v1(
    snapshot: &HostedSlackNativeSnapshot,
) -> Result<HostedSlackLogicalPathsV1, HostedSlackPathError> {
    let document_count = 1usize
        .checked_add(snapshot.threads().len())
        .and_then(|count| count.checked_add(snapshot.files().len()))
        .ok_or(HostedSlackPathError::TooManyDocuments {
            maximum: MAX_HOSTED_SLACK_PROJECTION_DOCUMENTS_V1,
            actual: usize::MAX,
        })?;
    if document_count > MAX_HOSTED_SLACK_PROJECTION_DOCUMENTS_V1 {
        return Err(HostedSlackPathError::TooManyDocuments {
            maximum: MAX_HOSTED_SLACK_PROJECTION_DOCUMENTS_V1,
            actual: document_count,
        });
    }

    let channel = snapshot.channel();
    let channel_component = format!(
        "{}-{}",
        slug_v1(channel.name(), "channel"),
        channel.channel_id()
    );
    let channel_directory = format!("channels/{channel_component}");
    let channel_path = checked_logical_path([channel_directory.as_str(), "channel.md"])?;

    let messages = snapshot
        .messages()
        .iter()
        .map(|message| (message.message_id(), message))
        .collect::<BTreeMap<_, _>>();
    let mut threads = Vec::with_capacity(snapshot.threads().len());
    for thread in snapshot.threads() {
        let root = messages.get(thread.root_message_id()).ok_or_else(|| {
            HostedSlackPathError::MissingRootMessage(thread.root_message_id().to_string())
        })?;
        let posted_at = root.posted_at();
        let year = posted_at
            .get(0..4)
            .ok_or_else(|| HostedSlackPathError::UnsupportedLogicalPath(posted_at.to_string()))?;
        let month = posted_at
            .get(5..7)
            .ok_or_else(|| HostedSlackPathError::UnsupportedLogicalPath(posted_at.to_string()))?;
        let root_date = posted_at
            .get(0..10)
            .ok_or_else(|| HostedSlackPathError::UnsupportedLogicalPath(posted_at.to_string()))?;
        let timestamp_token = thread.root_message_id().replace('.', "-");
        let summary = if root.deleted() {
            "deleted-message".to_string()
        } else {
            slug_v1(root.text(), "message")
        };
        let filename = format!("{root_date}-{timestamp_token}-{summary}.md");
        let logical_path = checked_logical_path([
            channel_directory.as_str(),
            "threads",
            year,
            month,
            filename.as_str(),
        ])?;
        threads.push(HostedSlackThreadPathV1 {
            root_message_id: thread.root_message_id().to_string(),
            logical_path,
        });
    }

    let mut files = Vec::with_capacity(snapshot.files().len());
    for file in snapshot.files() {
        let file_component = format!("{}-{}", slug_v1(file.name(), "file"), file.file_id());
        let logical_path = checked_logical_path([
            channel_directory.as_str(),
            "files",
            file_component.as_str(),
            "metadata.md",
        ])?;
        files.push(HostedSlackFilePathV1 {
            file_id: file.file_id().to_string(),
            logical_path,
        });
    }

    validate_path_collisions(
        std::iter::once(&channel_path)
            .chain(threads.iter().map(|entry| &entry.logical_path))
            .chain(files.iter().map(|entry| &entry.logical_path)),
    )?;

    Ok(HostedSlackLogicalPathsV1 {
        path_format_version: HOSTED_SLACK_LOGICAL_PATH_FORMAT_VERSION_V1,
        channel: channel_path,
        threads,
        files,
        channel_directory,
    })
}

pub fn hosted_slack_slug_v1(value: &str) -> String {
    slug_v1(value, "item")
}

fn slug_v1(value: &str, fallback: &'static str) -> String {
    let mut slug = String::with_capacity(value.len().min(MAX_HOSTED_SLACK_SLUG_BYTES_V1));
    let mut separator_pending = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            if separator_pending && !slug.is_empty() && slug.len() < MAX_HOSTED_SLACK_SLUG_BYTES_V1
            {
                slug.push('-');
            }
            separator_pending = false;
            if slug.len() == MAX_HOSTED_SLACK_SLUG_BYTES_V1 {
                break;
            }
            slug.push(character.to_ascii_lowercase());
        } else if !slug.is_empty() {
            separator_pending = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        fallback.to_string()
    } else {
        slug
    }
}

fn checked_logical_path<'a>(
    components: impl IntoIterator<Item = &'a str>,
) -> Result<LogicalPath, HostedSlackPathError> {
    let components = components.into_iter().collect::<Vec<_>>();
    for component in &components {
        let utf16_len = component.encode_utf16().count();
        if component.len() > MAX_HOSTED_SLACK_PATH_COMPONENT_BYTES_V1
            || utf16_len > MAX_HOSTED_SLACK_PATH_COMPONENT_BYTES_V1
        {
            return Err(HostedSlackPathError::ComponentTooLong {
                component: (*component).to_string(),
                maximum_bytes: MAX_HOSTED_SLACK_PATH_COMPONENT_BYTES_V1,
                actual_bytes: component.len().max(utf16_len),
            });
        }
    }
    let value = components.join("/");
    let utf16_len = value.encode_utf16().count();
    if value.len() > MAX_HOSTED_SLACK_LOGICAL_PATH_BYTES_V1
        || utf16_len > MAX_HOSTED_SLACK_LOGICAL_PATH_BYTES_V1
    {
        return Err(HostedSlackPathError::LogicalPathTooLong {
            maximum_bytes: MAX_HOSTED_SLACK_LOGICAL_PATH_BYTES_V1,
            actual_bytes: value.len().max(utf16_len),
        });
    }
    LogicalPath::new(value.clone()).map_err(|_| HostedSlackPathError::UnsupportedLogicalPath(value))
}

fn validate_path_collisions<'a>(
    paths: impl IntoIterator<Item = &'a LogicalPath>,
) -> Result<(), HostedSlackPathError> {
    let mut seen = BTreeMap::<String, String>::new();
    for path in paths {
        let collision_key = path.as_str().to_ascii_lowercase();
        if let Some(first) = seen.insert(collision_key, path.as_str().to_string()) {
            return Err(HostedSlackPathError::PathCollision {
                first,
                second: path.as_str().to_string(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collision_validation_fails_closed_for_portable_case_collisions() {
        let first = LogicalPath::new("channels/example/channel.md").expect("path");
        let second = LogicalPath::new("CHANNELS/example/channel.md").expect("path");
        assert!(matches!(
            validate_path_collisions([&first, &second]),
            Err(HostedSlackPathError::PathCollision { .. })
        ));
    }

    #[test]
    fn slug_is_ascii_bounded_and_has_a_deterministic_fallback() {
        assert_eq!(hosted_slack_slug_v1(" Résumé / Q3? "), "r-sum-q3");
        assert_eq!(hosted_slack_slug_v1("日本語"), "item");
        assert_eq!(
            hosted_slack_slug_v1(&"A".repeat(MAX_HOSTED_SLACK_SLUG_BYTES_V1 + 10)),
            "a".repeat(MAX_HOSTED_SLACK_SLUG_BYTES_V1)
        );
    }

    #[test]
    fn connector_path_validation_rejects_unsupported_components_and_lengths() {
        assert!(matches!(
            checked_logical_path(["channels", "NUL", "channel.md"]),
            Err(HostedSlackPathError::UnsupportedLogicalPath(_))
        ));
        assert!(matches!(
            checked_logical_path(["channels", &"a".repeat(256), "channel.md"]),
            Err(HostedSlackPathError::ComponentTooLong { .. })
        ));
    }
}
