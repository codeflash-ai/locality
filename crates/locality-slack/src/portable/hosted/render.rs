use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};

use chrono::{DateTime, Datelike, SecondsFormat, Utc};
use locality_core::portable::LogicalPath;
use locality_protocol::{
    HostedSlackChannelSelector, ReplicaFreshnessState, SlackChannelSharingClassification,
    SlackInstallationId,
};
use serde::{Deserialize, Serialize};

use super::native::{
    HostedSlackFileCaptureStatus, HostedSlackFileMetadata, HostedSlackMessage,
    HostedSlackNativeSnapshot, HostedSlackThread, HostedSlackUser,
};
use super::path::{
    HOSTED_SLACK_LOGICAL_PATH_FORMAT_VERSION_V1, HostedSlackLogicalPathsV1, HostedSlackPathError,
    build_hosted_slack_logical_paths_v1,
};

/// Fixture-pinned Markdown format proposed by ADR 0004.
///
/// A later format must use a new explicit version rather than inheriting this
/// fixture contract silently.
pub const HOSTED_SLACK_MARKDOWN_FORMAT_VERSION_V1: u16 = 1;
pub const HOSTED_SLACK_OPERATIONAL_STATUS_FORMAT_VERSION_V1: u16 = 1;
pub const MAX_HOSTED_SLACK_RENDERED_DOCUMENT_BYTES_V1: usize = 128 * 1024;
pub const MAX_HOSTED_SLACK_RENDERED_PROJECTION_BYTES_V1: usize = 512 * 1024;

/// Immutable operational facts sealed with one complete V1 channel projection.
///
/// Timestamps are supplied by the caller and never derived from an ambient
/// clock. The coverage interval is half-open: `[coverage_start_at,
/// coverage_end_at)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedSlackOperationalStatusV1 {
    pub status_format_version: u16,
    pub installation_id: SlackInstallationId,
    pub team_id: String,
    pub channel_id: String,
    pub authorized_history_start_at: String,
    pub sharing: SlackChannelSharingClassification,
    pub coverage_start_at: String,
    pub coverage_end_at: String,
    pub coverage_complete: bool,
    pub freshness_state: ReplicaFreshnessState,
    pub freshness_observed_through: String,
    pub last_successful_sync_at: String,
}

impl HostedSlackOperationalStatusV1 {
    pub fn validate(
        &self,
        selector: &HostedSlackChannelSelector,
    ) -> Result<(), HostedSlackRenderError> {
        selector
            .validate()
            .map_err(|error| HostedSlackRenderError::InvalidSelector(error.to_string()))?;
        if self.status_format_version != HOSTED_SLACK_OPERATIONAL_STATUS_FORMAT_VERSION_V1 {
            return Err(
                HostedSlackRenderError::UnsupportedOperationalStatusVersion {
                    version: self.status_format_version,
                },
            );
        }
        for (field, matches) in [
            (
                "installation_id",
                &self.installation_id == &selector.installation_id,
            ),
            ("team_id", self.team_id == selector.team_id),
            ("channel_id", self.channel_id == selector.channel_id),
            (
                "authorized_history_start_at",
                self.authorized_history_start_at == selector.authorized_history_start_at,
            ),
            ("sharing", self.sharing == selector.sharing),
        ] {
            if !matches {
                return Err(HostedSlackRenderError::OperationalStatusScopeMismatch(
                    field,
                ));
            }
        }

        let authorized_history_start_at = parse_operational_timestamp(
            "authorized_history_start_at",
            &self.authorized_history_start_at,
        )?;
        let coverage_start_at =
            parse_operational_timestamp("coverage_start_at", &self.coverage_start_at)?;
        let coverage_end_at =
            parse_operational_timestamp("coverage_end_at", &self.coverage_end_at)?;
        let freshness_observed_through = parse_operational_timestamp(
            "freshness_observed_through",
            &self.freshness_observed_through,
        )?;
        let last_successful_sync_at =
            parse_operational_timestamp("last_successful_sync_at", &self.last_successful_sync_at)?;

        if coverage_start_at != authorized_history_start_at {
            return Err(HostedSlackRenderError::OperationalStatusScopeMismatch(
                "coverage_start_at",
            ));
        }
        if coverage_start_at >= coverage_end_at {
            return Err(HostedSlackRenderError::InvalidOperationalStatusOrder(
                "coverage_start_at must precede coverage_end_at",
            ));
        }
        if coverage_end_at > freshness_observed_through {
            return Err(HostedSlackRenderError::InvalidOperationalStatusOrder(
                "coverage_end_at must not follow freshness_observed_through",
            ));
        }
        if freshness_observed_through > last_successful_sync_at {
            return Err(HostedSlackRenderError::InvalidOperationalStatusOrder(
                "freshness_observed_through must not follow last_successful_sync_at",
            ));
        }
        if !self.coverage_complete {
            return Err(HostedSlackRenderError::IncompleteCoverage);
        }
        if self.freshness_state == ReplicaFreshnessState::Bootstrapping {
            return Err(HostedSlackRenderError::UnrenderableFreshnessState(
                self.freshness_state,
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct HostedSlackProjectionFormatV1 {
    pub logical_path_format_version: u16,
    pub markdown_format_version: u16,
}

pub const HOSTED_SLACK_PROJECTION_FORMAT_V1: HostedSlackProjectionFormatV1 =
    HostedSlackProjectionFormatV1 {
        logical_path_format_version: HOSTED_SLACK_LOGICAL_PATH_FORMAT_VERSION_V1,
        markdown_format_version: HOSTED_SLACK_MARKDOWN_FORMAT_VERSION_V1,
    };

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostedSlackDocumentKindV1 {
    Channel,
    Thread,
    FileMetadata,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostedSlackRenderedDocumentV1 {
    kind: HostedSlackDocumentKindV1,
    logical_path: LogicalPath,
    bytes: Vec<u8>,
}

impl HostedSlackRenderedDocumentV1 {
    pub fn kind(&self) -> HostedSlackDocumentKindV1 {
        self.kind
    }

    pub fn logical_path(&self) -> &LogicalPath {
        &self.logical_path
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostedSlackRenderedProjectionV1 {
    format: HostedSlackProjectionFormatV1,
    paths: HostedSlackLogicalPathsV1,
    documents: Vec<HostedSlackRenderedDocumentV1>,
}

impl HostedSlackRenderedProjectionV1 {
    pub fn format(&self) -> HostedSlackProjectionFormatV1 {
        self.format
    }

    pub fn paths(&self) -> &HostedSlackLogicalPathsV1 {
        &self.paths
    }

    pub fn documents(&self) -> &[HostedSlackRenderedDocumentV1] {
        &self.documents
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostedSlackRenderError {
    InvalidSelector(String),
    ScopeMismatch(&'static str),
    UnsupportedOperationalStatusVersion {
        version: u16,
    },
    OperationalStatusScopeMismatch(&'static str),
    InvalidOperationalStatusTimestamp(&'static str),
    InvalidOperationalStatusOrder(&'static str),
    IncompleteCoverage,
    UnrenderableFreshnessState(ReplicaFreshnessState),
    RootOutsideCoverage {
        root_message_id: String,
    },
    Path(HostedSlackPathError),
    MissingNativeReference(&'static str),
    Serialization,
    DocumentTooLarge {
        logical_path: String,
        maximum_bytes: usize,
        actual_bytes: usize,
    },
    ProjectionTooLarge {
        maximum_bytes: usize,
        actual_bytes: usize,
    },
}

impl Display for HostedSlackRenderError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSelector(error) => {
                write!(formatter, "hosted Slack selector is invalid: {error}")
            }
            Self::ScopeMismatch(field) => {
                write!(
                    formatter,
                    "hosted Slack selector does not match snapshot {field}"
                )
            }
            Self::UnsupportedOperationalStatusVersion { version } => write!(
                formatter,
                "hosted Slack operational status version {version} is unsupported"
            ),
            Self::OperationalStatusScopeMismatch(field) => write!(
                formatter,
                "hosted Slack operational status does not match selector {field}"
            ),
            Self::InvalidOperationalStatusTimestamp(field) => write!(
                formatter,
                "hosted Slack operational status {field} must be canonical UTC seconds"
            ),
            Self::InvalidOperationalStatusOrder(requirement) => write!(
                formatter,
                "hosted Slack operational status order is invalid: {requirement}"
            ),
            Self::IncompleteCoverage => formatter
                .write_str("hosted Slack projection cannot render incomplete channel coverage"),
            Self::UnrenderableFreshnessState(state) => write!(
                formatter,
                "hosted Slack projection cannot render freshness state {state:?}"
            ),
            Self::RootOutsideCoverage { root_message_id } => write!(
                formatter,
                "hosted Slack root {root_message_id} is outside the authorized complete coverage interval"
            ),
            Self::Path(error) => Display::fmt(error, formatter),
            Self::MissingNativeReference(reference) => {
                write!(formatter, "hosted Slack render is missing {reference}")
            }
            Self::Serialization => {
                formatter.write_str("hosted Slack frontmatter serialization failed")
            }
            Self::DocumentTooLarge {
                logical_path,
                maximum_bytes,
                actual_bytes,
            } => write!(
                formatter,
                "hosted Slack document {logical_path:?} is {actual_bytes} bytes, exceeding {maximum_bytes}"
            ),
            Self::ProjectionTooLarge {
                maximum_bytes,
                actual_bytes,
            } => write!(
                formatter,
                "hosted Slack projection is {actual_bytes} bytes, exceeding {maximum_bytes}"
            ),
        }
    }
}

impl std::error::Error for HostedSlackRenderError {}

impl From<HostedSlackPathError> for HostedSlackRenderError {
    fn from(error: HostedSlackPathError) -> Self {
        Self::Path(error)
    }
}

pub fn render_hosted_slack_projection_v1(
    selector: &HostedSlackChannelSelector,
    status: &HostedSlackOperationalStatusV1,
    snapshot: &HostedSlackNativeSnapshot,
) -> Result<HostedSlackRenderedProjectionV1, HostedSlackRenderError> {
    selector
        .validate()
        .map_err(|error| HostedSlackRenderError::InvalidSelector(error.to_string()))?;
    validate_scope_matches_snapshot(selector, snapshot)?;
    status.validate(selector)?;
    validate_snapshot_root_coverage(status, snapshot)?;
    let paths = build_hosted_slack_logical_paths_v1(snapshot)?;

    let messages = snapshot
        .messages()
        .iter()
        .map(|message| (message.message_id(), message))
        .collect::<BTreeMap<_, _>>();
    let users = snapshot
        .users()
        .iter()
        .map(|user| (user.user_id(), user))
        .collect::<BTreeMap<_, _>>();
    let files = snapshot
        .files()
        .iter()
        .map(|file| (file.file_id(), file))
        .collect::<BTreeMap<_, _>>();

    let mut documents = Vec::with_capacity(1 + snapshot.threads().len() + snapshot.files().len());
    documents.push(rendered_document(
        HostedSlackDocumentKindV1::Channel,
        paths.channel.clone(),
        render_channel(selector, status, snapshot, &paths, &messages)?,
    )?);
    for thread in snapshot.threads() {
        let logical_path = paths.thread_path(thread.root_message_id()).ok_or(
            HostedSlackRenderError::MissingNativeReference("thread logical path"),
        )?;
        documents.push(rendered_document(
            HostedSlackDocumentKindV1::Thread,
            logical_path.clone(),
            render_thread(snapshot, thread, &paths, &messages, &users, &files)?,
        )?);
    }
    for file in snapshot.files() {
        let logical_path = paths.file_path(file.file_id()).ok_or(
            HostedSlackRenderError::MissingNativeReference("file logical path"),
        )?;
        documents.push(rendered_document(
            HostedSlackDocumentKindV1::FileMetadata,
            logical_path.clone(),
            render_file_metadata(snapshot, file, &paths, &messages)?,
        )?);
    }
    documents.sort_by(|left, right| left.logical_path.cmp(&right.logical_path));

    let total_bytes = documents
        .iter()
        .map(|document| document.bytes.len())
        .fold(0usize, usize::saturating_add);
    if total_bytes > MAX_HOSTED_SLACK_RENDERED_PROJECTION_BYTES_V1 {
        return Err(HostedSlackRenderError::ProjectionTooLarge {
            maximum_bytes: MAX_HOSTED_SLACK_RENDERED_PROJECTION_BYTES_V1,
            actual_bytes: total_bytes,
        });
    }

    Ok(HostedSlackRenderedProjectionV1 {
        format: HOSTED_SLACK_PROJECTION_FORMAT_V1,
        paths,
        documents,
    })
}

fn validate_scope_matches_snapshot(
    selector: &HostedSlackChannelSelector,
    snapshot: &HostedSlackNativeSnapshot,
) -> Result<(), HostedSlackRenderError> {
    for (field, matches) in [
        (
            "installation_id",
            &selector.installation_id == snapshot.installation_id(),
        ),
        ("team_id", selector.team_id == snapshot.channel().team_id()),
        (
            "channel_id",
            selector.channel_id == snapshot.channel().channel_id(),
        ),
        ("sharing", selector.sharing == snapshot.channel().sharing()),
    ] {
        if !matches {
            return Err(HostedSlackRenderError::ScopeMismatch(field));
        }
    }
    Ok(())
}

fn validate_snapshot_root_coverage(
    status: &HostedSlackOperationalStatusV1,
    snapshot: &HostedSlackNativeSnapshot,
) -> Result<(), HostedSlackRenderError> {
    let coverage_start_at =
        parse_operational_timestamp("coverage_start_at", &status.coverage_start_at)?;
    let coverage_end_at = parse_operational_timestamp("coverage_end_at", &status.coverage_end_at)?;
    let messages = snapshot
        .messages()
        .iter()
        .map(|message| (message.message_id(), message))
        .collect::<BTreeMap<_, _>>();
    for thread in snapshot.threads() {
        let root = messages.get(thread.root_message_id()).ok_or(
            HostedSlackRenderError::MissingNativeReference("thread root message"),
        )?;
        let posted_at = DateTime::parse_from_rfc3339(root.posted_at())
            .map_err(|_| HostedSlackRenderError::MissingNativeReference("root timestamp"))?
            .with_timezone(&Utc);
        if posted_at < coverage_start_at || posted_at >= coverage_end_at {
            return Err(HostedSlackRenderError::RootOutsideCoverage {
                root_message_id: root.message_id().to_string(),
            });
        }
    }
    Ok(())
}

fn parse_operational_timestamp(
    field: &'static str,
    value: &str,
) -> Result<DateTime<Utc>, HostedSlackRenderError> {
    let parsed = DateTime::parse_from_rfc3339(value)
        .map_err(|_| HostedSlackRenderError::InvalidOperationalStatusTimestamp(field))?
        .with_timezone(&Utc);
    if parsed.year() <= 0 || parsed.to_rfc3339_opts(SecondsFormat::Secs, true) != value {
        return Err(HostedSlackRenderError::InvalidOperationalStatusTimestamp(
            field,
        ));
    }
    Ok(parsed)
}

fn rendered_document(
    kind: HostedSlackDocumentKindV1,
    logical_path: LogicalPath,
    markdown: String,
) -> Result<HostedSlackRenderedDocumentV1, HostedSlackRenderError> {
    let bytes = markdown.into_bytes();
    if bytes.len() > MAX_HOSTED_SLACK_RENDERED_DOCUMENT_BYTES_V1 {
        return Err(HostedSlackRenderError::DocumentTooLarge {
            logical_path: logical_path.as_str().to_string(),
            maximum_bytes: MAX_HOSTED_SLACK_RENDERED_DOCUMENT_BYTES_V1,
            actual_bytes: bytes.len(),
        });
    }
    Ok(HostedSlackRenderedDocumentV1 {
        kind,
        logical_path,
        bytes,
    })
}

fn render_channel(
    selector: &HostedSlackChannelSelector,
    status: &HostedSlackOperationalStatusV1,
    snapshot: &HostedSlackNativeSnapshot,
    paths: &HostedSlackLogicalPathsV1,
    messages: &BTreeMap<&str, &HostedSlackMessage>,
) -> Result<String, HostedSlackRenderError> {
    let channel = snapshot.channel();
    let mut markdown = String::new();
    markdown.push_str("---\n");
    frontmatter_string(&mut markdown, "format", "locality.hosted_slack.channel.v1")?;
    frontmatter_number(
        &mut markdown,
        "format_version",
        HOSTED_SLACK_MARKDOWN_FORMAT_VERSION_V1,
    );
    frontmatter_string(
        &mut markdown,
        "installation_id",
        snapshot.installation_id().as_str(),
    )?;
    frontmatter_string(&mut markdown, "team_id", channel.team_id())?;
    frontmatter_string(&mut markdown, "channel_id", channel.channel_id())?;
    frontmatter_string(
        &mut markdown,
        "authorized_history_start_at",
        &selector.authorized_history_start_at,
    )?;
    frontmatter_string(
        &mut markdown,
        "coverage_start_at",
        &status.coverage_start_at,
    )?;
    frontmatter_string(&mut markdown, "coverage_end_at", &status.coverage_end_at)?;
    frontmatter_bool(&mut markdown, "coverage_complete", status.coverage_complete);
    frontmatter_serializable(&mut markdown, "freshness_state", &status.freshness_state)?;
    frontmatter_string(
        &mut markdown,
        "freshness_observed_through",
        &status.freshness_observed_through,
    )?;
    frontmatter_string(
        &mut markdown,
        "last_successful_sync_at",
        &status.last_successful_sync_at,
    )?;
    frontmatter_serializable(&mut markdown, "sharing", &channel.sharing())?;
    frontmatter_string(&mut markdown, "name", channel.name())?;
    frontmatter_optional_string(&mut markdown, "topic", channel.topic())?;
    frontmatter_optional_string(&mut markdown, "purpose", channel.purpose())?;
    frontmatter_string(&mut markdown, "created_at", channel.created_at())?;
    frontmatter_optional_string(&mut markdown, "updated_at", channel.updated_at())?;
    markdown.push_str("---\n\n");

    let channel_label = nonempty_or(channel.name(), channel.channel_id());
    markdown.push_str("# Slack channel: ");
    markdown.push_str(&escape_markdown_inline(channel_label));
    markdown.push_str("\n\n");
    markdown.push_str("- Channel ID: `");
    markdown.push_str(channel.channel_id());
    markdown.push_str("`\n- Team ID: `");
    markdown.push_str(channel.team_id());
    markdown.push_str("`\n- Authorized history start: `");
    markdown.push_str(&selector.authorized_history_start_at);
    markdown.push_str("`\n- Complete coverage: `[");
    markdown.push_str(&status.coverage_start_at);
    markdown.push_str(", ");
    markdown.push_str(&status.coverage_end_at);
    markdown.push_str(")`\n- Coverage complete: `true`\n- Freshness: `");
    markdown.push_str(freshness_state_label(status.freshness_state));
    markdown.push_str("` through `");
    markdown.push_str(&status.freshness_observed_through);
    markdown.push_str("`\n- Last successful sync: `");
    markdown.push_str(&status.last_successful_sync_at);
    markdown.push_str("`\n\n");

    render_optional_data_section(&mut markdown, "Topic", channel.topic());
    render_optional_data_section(&mut markdown, "Purpose", channel.purpose());

    markdown.push_str("## Threads\n\n");
    if paths.threads.is_empty() {
        markdown.push_str("_No thread documents._\n\n");
    } else {
        for thread_path in &paths.threads {
            let root = messages.get(thread_path.root_message_id.as_str()).ok_or(
                HostedSlackRenderError::MissingNativeReference("thread root message"),
            )?;
            let relative = paths
                .channel_relative_path(&thread_path.logical_path)
                .ok_or(HostedSlackRenderError::MissingNativeReference(
                    "channel-relative thread path",
                ))?;
            markdown.push_str("- [");
            markdown.push_str(&escape_markdown_inline(&message_summary(root)));
            markdown.push_str("](");
            markdown.push_str(relative);
            markdown.push_str(") — message `");
            markdown.push_str(root.message_id());
            markdown.push_str("`\n");
        }
        markdown.push('\n');
    }

    markdown.push_str("## Files\n\n");
    if paths.files.is_empty() {
        markdown.push_str("_No file metadata documents._\n");
    } else {
        for file_path in &paths.files {
            let file = snapshot
                .files()
                .iter()
                .find(|file| file.file_id() == file_path.file_id)
                .ok_or(HostedSlackRenderError::MissingNativeReference(
                    "file metadata",
                ))?;
            let relative = paths.channel_relative_path(&file_path.logical_path).ok_or(
                HostedSlackRenderError::MissingNativeReference("channel-relative file path"),
            )?;
            markdown.push_str("- [");
            markdown.push_str(&escape_markdown_inline(file_display_name(file)));
            markdown.push_str("](");
            markdown.push_str(relative);
            markdown.push_str(") — file `");
            markdown.push_str(file.file_id());
            markdown.push_str("`\n");
        }
    }
    Ok(markdown)
}

fn render_thread(
    snapshot: &HostedSlackNativeSnapshot,
    thread: &HostedSlackThread,
    paths: &HostedSlackLogicalPathsV1,
    messages: &BTreeMap<&str, &HostedSlackMessage>,
    users: &BTreeMap<&str, &HostedSlackUser>,
    files: &BTreeMap<&str, &HostedSlackFileMetadata>,
) -> Result<String, HostedSlackRenderError> {
    let root = messages.get(thread.root_message_id()).ok_or(
        HostedSlackRenderError::MissingNativeReference("thread root message"),
    )?;
    let mut markdown = String::new();
    markdown.push_str("---\n");
    frontmatter_string(&mut markdown, "format", "locality.hosted_slack.thread.v1")?;
    frontmatter_number(
        &mut markdown,
        "format_version",
        HOSTED_SLACK_MARKDOWN_FORMAT_VERSION_V1,
    );
    frontmatter_string(
        &mut markdown,
        "installation_id",
        snapshot.installation_id().as_str(),
    )?;
    frontmatter_string(&mut markdown, "team_id", snapshot.channel().team_id())?;
    frontmatter_string(&mut markdown, "channel_id", snapshot.channel().channel_id())?;
    frontmatter_string(&mut markdown, "root_message_id", thread.root_message_id())?;
    frontmatter_string(&mut markdown, "root_posted_at", root.posted_at())?;
    frontmatter_bool(&mut markdown, "root_deleted", root.deleted());
    frontmatter_number(
        &mut markdown,
        "reply_count",
        thread.reply_message_ids().len(),
    );
    markdown.push_str("---\n\n# Slack thread: ");
    markdown.push_str(&escape_markdown_inline(&message_summary(root)));
    markdown.push_str("\n\n");

    render_message(&mut markdown, "Root message", root, paths, users, files)?;
    for reply_id in thread.reply_message_ids() {
        let reply = messages.get(reply_id.as_str()).ok_or(
            HostedSlackRenderError::MissingNativeReference("thread reply message"),
        )?;
        render_message(&mut markdown, "Reply", reply, paths, users, files)?;
    }
    while markdown.ends_with("\n\n") {
        markdown.pop();
    }
    Ok(markdown)
}

fn render_message(
    markdown: &mut String,
    heading: &str,
    message: &HostedSlackMessage,
    paths: &HostedSlackLogicalPathsV1,
    users: &BTreeMap<&str, &HostedSlackUser>,
    files: &BTreeMap<&str, &HostedSlackFileMetadata>,
) -> Result<(), HostedSlackRenderError> {
    markdown.push_str("## ");
    markdown.push_str(heading);
    markdown.push_str(" `");
    markdown.push_str(message.message_id());
    markdown.push_str("`\n\n- Author: ");
    markdown.push_str(&escape_markdown_inline(&message_author(message, users)));
    if let Some(user_id) = message.user_id() {
        markdown.push_str(" (`");
        markdown.push_str(user_id);
        markdown.push_str("`)");
    }
    markdown.push_str("\n- Posted at: `");
    markdown.push_str(message.posted_at());
    markdown.push_str("`\n- Edited at: ");
    if let Some(edited_at) = message.edited_at() {
        markdown.push('`');
        markdown.push_str(edited_at);
        markdown.push('`');
    } else {
        markdown.push_str("not edited");
    }
    markdown.push_str("\n- Deleted: ");
    markdown.push_str(if message.deleted() { "true" } else { "false" });
    markdown.push_str("\n\n");

    if message.deleted() {
        markdown.push_str("> Tombstone: Slack reports this message as deleted.\n\n");
    } else {
        markdown.push_str("### Slack text (verbatim)\n\n");
        render_data_block(markdown, message.text());
        markdown.push('\n');
    }

    if !message.file_ids().is_empty() {
        markdown.push_str("### Files\n\n");
        for file_id in message.file_ids() {
            let file = files.get(file_id.as_str()).ok_or(
                HostedSlackRenderError::MissingNativeReference("message file metadata"),
            )?;
            let file_path =
                paths
                    .file_path(file_id)
                    .ok_or(HostedSlackRenderError::MissingNativeReference(
                        "message file logical path",
                    ))?;
            let relative = paths.channel_relative_path(file_path).ok_or(
                HostedSlackRenderError::MissingNativeReference("channel-relative file path"),
            )?;
            markdown.push_str("- [");
            markdown.push_str(&escape_markdown_inline(file_display_name(file)));
            markdown.push_str("](../../../");
            markdown.push_str(relative);
            markdown.push_str(") — file `");
            markdown.push_str(file_id);
            markdown.push_str("`\n");
        }
        markdown.push('\n');
    }
    Ok(())
}

fn render_file_metadata(
    snapshot: &HostedSlackNativeSnapshot,
    file: &HostedSlackFileMetadata,
    paths: &HostedSlackLogicalPathsV1,
    messages: &BTreeMap<&str, &HostedSlackMessage>,
) -> Result<String, HostedSlackRenderError> {
    let owning_messages = messages
        .values()
        .copied()
        .filter(|message| {
            message
                .file_ids()
                .iter()
                .any(|file_id| file_id == file.file_id())
        })
        .collect::<Vec<_>>();

    let mut markdown = String::new();
    markdown.push_str("---\n");
    frontmatter_string(
        &mut markdown,
        "format",
        "locality.hosted_slack.file_metadata.v1",
    )?;
    frontmatter_number(
        &mut markdown,
        "format_version",
        HOSTED_SLACK_MARKDOWN_FORMAT_VERSION_V1,
    );
    frontmatter_string(
        &mut markdown,
        "installation_id",
        snapshot.installation_id().as_str(),
    )?;
    frontmatter_string(&mut markdown, "team_id", snapshot.channel().team_id())?;
    frontmatter_string(&mut markdown, "channel_id", snapshot.channel().channel_id())?;
    frontmatter_string(&mut markdown, "file_id", file.file_id())?;
    frontmatter_optional_string(&mut markdown, "user_id", file.user_id())?;
    frontmatter_string(&mut markdown, "name", file.name())?;
    frontmatter_string(&mut markdown, "title", file.title())?;
    frontmatter_string(&mut markdown, "mimetype", file.mimetype())?;
    frontmatter_number(&mut markdown, "byte_length", file.byte_length());
    frontmatter_string(&mut markdown, "created_at", file.created_at())?;
    frontmatter_bool(&mut markdown, "deleted", file.deleted());
    frontmatter_string(
        &mut markdown,
        "capture_status",
        file_capture_status(file.capture_receipt().status()),
    )?;
    if owning_messages.is_empty() {
        markdown.push_str("owning_message_ids: []\n");
    } else {
        markdown.push_str("owning_message_ids:\n");
        for message in &owning_messages {
            markdown.push_str("  - ");
            markdown.push_str(&json_scalar(&message.message_id())?);
            markdown.push('\n');
        }
    }
    markdown.push_str("---\n\n# Slack file metadata: ");
    markdown.push_str(&escape_markdown_inline(file_display_name(file)));
    markdown.push_str("\n\n- File ID: `");
    markdown.push_str(file.file_id());
    markdown.push_str("`\n- Name: ");
    markdown.push_str(&escape_markdown_inline(file.name()));
    markdown.push_str("\n- Title: ");
    markdown.push_str(&escape_markdown_inline(file.title()));
    markdown.push_str("\n- MIME type: ");
    markdown.push_str(&escape_markdown_inline(file.mimetype()));
    markdown.push_str("\n- Declared bytes: ");
    markdown.push_str(&file.byte_length().to_string());
    markdown.push_str("\n- Created at: `");
    markdown.push_str(file.created_at());
    markdown.push_str("`\n- Capture status: `bytes_not_captured`\n\n");
    if file.deleted() {
        markdown.push_str("> Tombstone: Slack reports this file metadata as deleted.\n\n");
    }

    markdown.push_str("## Owning messages\n\n");
    if owning_messages.is_empty() {
        markdown.push_str("_No owning message reference was captured._\n");
    } else {
        for message in owning_messages {
            let root_id = message
                .thread_root_message_id()
                .unwrap_or(message.message_id());
            let thread_path = paths.thread_path(root_id).ok_or(
                HostedSlackRenderError::MissingNativeReference("owning message thread path"),
            )?;
            let relative = paths.channel_relative_path(thread_path).ok_or(
                HostedSlackRenderError::MissingNativeReference("channel-relative thread path"),
            )?;
            markdown.push_str("- [Message `");
            markdown.push_str(message.message_id());
            markdown.push_str("`](../../");
            markdown.push_str(relative);
            markdown.push_str(")\n");
        }
    }
    Ok(markdown)
}

fn frontmatter_string(
    output: &mut String,
    key: &str,
    value: &str,
) -> Result<(), HostedSlackRenderError> {
    output.push_str(key);
    output.push_str(": ");
    output.push_str(&json_scalar(&value)?);
    output.push('\n');
    Ok(())
}

fn frontmatter_optional_string(
    output: &mut String,
    key: &str,
    value: Option<&str>,
) -> Result<(), HostedSlackRenderError> {
    output.push_str(key);
    output.push_str(": ");
    match value {
        Some(value) => output.push_str(&json_scalar(&value)?),
        None => output.push_str("null"),
    }
    output.push('\n');
    Ok(())
}

fn frontmatter_serializable(
    output: &mut String,
    key: &str,
    value: &impl Serialize,
) -> Result<(), HostedSlackRenderError> {
    output.push_str(key);
    output.push_str(": ");
    output.push_str(&json_scalar(value)?);
    output.push('\n');
    Ok(())
}

fn frontmatter_number(output: &mut String, key: &str, value: impl Display) {
    output.push_str(key);
    output.push_str(": ");
    output.push_str(&value.to_string());
    output.push('\n');
}

fn frontmatter_bool(output: &mut String, key: &str, value: bool) {
    frontmatter_number(output, key, value);
}

fn json_scalar(value: &impl Serialize) -> Result<String, HostedSlackRenderError> {
    serde_json::to_string(value).map_err(|_| HostedSlackRenderError::Serialization)
}

fn render_optional_data_section(output: &mut String, heading: &str, value: Option<&str>) {
    output.push_str("## ");
    output.push_str(heading);
    output.push_str("\n\n");
    match value.filter(|value| !value.is_empty()) {
        Some(value) => render_data_block(output, value),
        None => output.push_str("_Not provided._\n"),
    }
    output.push('\n');
}

fn render_data_block(output: &mut String, value: &str) {
    let value = canonical_data_text(value);
    let fence_length = longest_backtick_run(&value).saturating_add(1).max(3);
    let fence = "`".repeat(fence_length);
    output.push_str(&fence);
    output.push_str("text\n");
    output.push_str(&value);
    if !value.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(&fence);
    output.push('\n');
}

fn canonical_data_text(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                output.push('\n');
            }
            '\n' | '\t' => output.push(character),
            character if character.is_control() => {
                output.push_str(&format!("\\u{{{:04x}}}", character as u32));
            }
            _ => output.push(character),
        }
    }
    output
}

fn longest_backtick_run(value: &str) -> usize {
    let mut longest = 0usize;
    let mut current = 0usize;
    for byte in value.bytes() {
        if byte == b'`' {
            current = current.saturating_add(1);
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

fn escape_markdown_inline(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '\\' | '`' | '*' | '_' | '{' | '}' | '[' | ']' | '(' | ')' | '#' | '+' | '-' | '.'
            | '!' | '|' => {
                output.push('\\');
                output.push(character);
            }
            '\r' | '\n' | '\t' => output.push(' '),
            character if character.is_control() => {
                output.push_str(&format!("\\u{{{:04x}}}", character as u32));
            }
            _ => output.push(character),
        }
    }
    output
}

fn freshness_state_label(state: ReplicaFreshnessState) -> &'static str {
    match state {
        ReplicaFreshnessState::Bootstrapping => "bootstrapping",
        ReplicaFreshnessState::Fresh => "fresh",
        ReplicaFreshnessState::Stale => "stale",
        ReplicaFreshnessState::Unavailable => "unavailable",
    }
}

fn message_author(
    message: &HostedSlackMessage,
    users: &BTreeMap<&str, &HostedSlackUser>,
) -> String {
    let Some(user_id) = message.user_id() else {
        return "Unknown Slack user".to_string();
    };
    let Some(user) = users.get(user_id) else {
        return format!("Unknown Slack user {user_id}");
    };
    if user.deleted() {
        return format!("Deleted Slack user {user_id}");
    }
    let name = [user.display_name(), user.real_name(), user.name()]
        .into_iter()
        .find(|name| !name.trim().is_empty())
        .unwrap_or(user_id);
    if user.is_bot() {
        format!("{name} (bot)")
    } else {
        name.to_string()
    }
}

fn message_summary(message: &HostedSlackMessage) -> String {
    if message.deleted() {
        return "Deleted message".to_string();
    }
    let normalized = canonical_data_text(message.text());
    let first_line = normalized
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("Message");
    truncate_chars(first_line, 80)
}

fn truncate_chars(value: &str, maximum: usize) -> String {
    let mut characters = value.chars();
    let truncated = characters.by_ref().take(maximum).collect::<String>();
    if characters.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn file_display_name(file: &HostedSlackFileMetadata) -> &str {
    nonempty_or(file.title(), nonempty_or(file.name(), file.file_id()))
}

fn nonempty_or<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.trim().is_empty() {
        fallback
    } else {
        value
    }
}

fn file_capture_status(status: HostedSlackFileCaptureStatus) -> &'static str {
    match status {
        HostedSlackFileCaptureStatus::BytesNotCaptured => "bytes_not_captured",
    }
}
