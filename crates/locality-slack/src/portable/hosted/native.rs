use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use locality_protocol::{SlackChannelSharingClassification, SlackInstallationId};
use serde::{Deserialize, Serialize};

use super::identity::{
    HOSTED_SLACK_CONVERSATION_ID_PREFIXES, HostedSlackPortableError,
    validate_bounded_metadata_text, validate_bounded_text, validate_collection_len,
    validate_slack_id,
};

pub const MAX_HOSTED_SLACK_NAME_BYTES: usize = 512;
pub const MAX_HOSTED_SLACK_TOPIC_BYTES: usize = 4 * 1024;
pub const MAX_HOSTED_SLACK_MESSAGE_TEXT_BYTES: usize = 64 * 1024;
pub const MAX_HOSTED_SLACK_COLLECTION_ENTRIES: usize = 4 * 1024;
pub const MAX_HOSTED_SLACK_MESSAGE_FILES: usize = 100;
pub const MAX_HOSTED_SLACK_THREAD_REPLIES: usize = 4 * 1024;
pub const MAX_HOSTED_SLACK_RAW_JSON_BYTES: usize = 1024 * 1024;
pub const MAX_HOSTED_SLACK_SNAPSHOT_STRING_BYTES: usize = 256 * 1024;
pub const MAX_HOSTED_SLACK_SNAPSHOT_REFERENCES: usize = 2 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostedSlackConversationKindV1 {
    PublicChannel,
    PrivateChannel,
    Im,
    Mpim,
}

impl HostedSlackConversationKindV1 {
    pub const fn root_folder(self) -> &'static str {
        match self {
            Self::PublicChannel => "channels",
            Self::PrivateChannel => "private-channels",
            Self::Im => "dms",
            Self::Mpim => "group-dms",
        }
    }

    pub const fn source_scope_kind(self) -> &'static str {
        match self {
            Self::PublicChannel | Self::PrivateChannel => "slack_channel",
            Self::Im => "slack_dm",
            Self::Mpim => "slack_group_dm",
        }
    }
}

impl Default for HostedSlackConversationKindV1 {
    fn default() -> Self {
        Self::PublicChannel
    }
}

fn is_default_hosted_slack_conversation_kind(value: &HostedSlackConversationKindV1) -> bool {
    *value == HostedSlackConversationKindV1::PublicChannel
}

pub fn decode_and_sanitize_hosted_slack_native_snapshot(
    bytes: &[u8],
) -> Result<HostedSlackNativeSnapshot, HostedSlackPortableError> {
    if bytes.len() > MAX_HOSTED_SLACK_RAW_JSON_BYTES {
        return Err(HostedSlackPortableError::RawInputTooLarge {
            maximum_bytes: MAX_HOSTED_SLACK_RAW_JSON_BYTES,
            actual_bytes: bytes.len(),
        });
    }
    let raw = serde_json::from_slice::<RawHostedSlackNativeSnapshot>(bytes)
        .map_err(|_| HostedSlackPortableError::InvalidRawJson)?;
    HostedSlackNativeSnapshot::try_from(raw)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawHostedSlackChannel {
    pub team_id: String,
    pub id: String,
    #[serde(
        default,
        skip_serializing_if = "is_default_hosted_slack_conversation_kind"
    )]
    pub conversation_kind: HostedSlackConversationKindV1,
    pub name: String,
    pub topic: Option<String>,
    pub purpose: Option<String>,
    pub created_ts: String,
    pub updated_ts: Option<String>,
    pub sharing: SlackChannelSharingClassification,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawHostedSlackUser {
    pub team_id: String,
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub real_name: String,
    pub is_bot: bool,
    pub deleted: bool,
    pub updated_ts: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawHostedSlackMessage {
    pub channel_id: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub user_id: Option<String>,
    pub text: String,
    pub edited_ts: Option<String>,
    pub deleted: bool,
    #[serde(default)]
    pub file_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawHostedSlackThread {
    pub channel_id: String,
    pub root_ts: String,
    #[serde(default)]
    pub reply_ts: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawHostedSlackFileMetadata {
    pub channel_id: String,
    pub id: String,
    pub user_id: Option<String>,
    pub name: String,
    pub title: String,
    pub mimetype: String,
    pub byte_length: u64,
    pub created_ts: String,
    pub deleted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawHostedSlackNativeSnapshot {
    pub installation_id: SlackInstallationId,
    pub channel: RawHostedSlackChannel,
    #[serde(default)]
    pub users: Vec<RawHostedSlackUser>,
    #[serde(default)]
    pub messages: Vec<RawHostedSlackMessage>,
    #[serde(default)]
    pub threads: Vec<RawHostedSlackThread>,
    #[serde(default)]
    pub files: Vec<RawHostedSlackFileMetadata>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostedSlackChannel {
    team_id: String,
    channel_id: String,
    conversation_kind: HostedSlackConversationKindV1,
    name: String,
    topic: Option<String>,
    purpose: Option<String>,
    created_at: String,
    updated_at: Option<String>,
    sharing: SlackChannelSharingClassification,
}

impl HostedSlackChannel {
    pub fn team_id(&self) -> &str {
        &self.team_id
    }

    pub fn channel_id(&self) -> &str {
        &self.channel_id
    }

    pub fn conversation_kind(&self) -> HostedSlackConversationKindV1 {
        self.conversation_kind
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn topic(&self) -> Option<&str> {
        self.topic.as_deref()
    }

    pub fn purpose(&self) -> Option<&str> {
        self.purpose.as_deref()
    }

    pub fn created_at(&self) -> &str {
        &self.created_at
    }

    pub fn updated_at(&self) -> Option<&str> {
        self.updated_at.as_deref()
    }

    pub fn sharing(&self) -> SlackChannelSharingClassification {
        self.sharing
    }
}

impl TryFrom<RawHostedSlackChannel> for HostedSlackChannel {
    type Error = HostedSlackPortableError;

    fn try_from(raw: RawHostedSlackChannel) -> Result<Self, Self::Error> {
        validate_slack_id("channel.team_id", &raw.team_id, b"T")?;
        let channel_id_prefixes: &[u8] = match raw.conversation_kind {
            HostedSlackConversationKindV1::PublicChannel
            | HostedSlackConversationKindV1::PrivateChannel => b"CG",
            HostedSlackConversationKindV1::Im => b"D",
            HostedSlackConversationKindV1::Mpim => b"G",
        };
        validate_slack_id("channel.id", &raw.id, channel_id_prefixes)?;
        validate_bounded_metadata_text("channel.name", &raw.name, MAX_HOSTED_SLACK_NAME_BYTES)?;
        validate_optional_metadata_text(
            "channel.topic",
            raw.topic.as_deref(),
            MAX_HOSTED_SLACK_TOPIC_BYTES,
        )?;
        validate_optional_metadata_text(
            "channel.purpose",
            raw.purpose.as_deref(),
            MAX_HOSTED_SLACK_TOPIC_BYTES,
        )?;
        let created_at = canonicalize_slack_timestamp("channel.created_ts", &raw.created_ts)?;
        let updated_at = raw
            .updated_ts
            .as_deref()
            .map(|timestamp| canonicalize_slack_timestamp("channel.updated_ts", timestamp))
            .transpose()?;
        Ok(Self {
            team_id: raw.team_id,
            channel_id: raw.id,
            conversation_kind: raw.conversation_kind,
            name: raw.name,
            topic: raw.topic,
            purpose: raw.purpose,
            created_at,
            updated_at,
            sharing: raw.sharing,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostedSlackUser {
    team_id: String,
    user_id: String,
    name: String,
    display_name: String,
    real_name: String,
    is_bot: bool,
    deleted: bool,
    updated_at: Option<String>,
}

impl HostedSlackUser {
    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn real_name(&self) -> &str {
        &self.real_name
    }

    pub fn is_bot(&self) -> bool {
        self.is_bot
    }

    pub fn deleted(&self) -> bool {
        self.deleted
    }

    pub fn updated_at(&self) -> Option<&str> {
        self.updated_at.as_deref()
    }
}

impl TryFrom<RawHostedSlackUser> for HostedSlackUser {
    type Error = HostedSlackPortableError;

    fn try_from(raw: RawHostedSlackUser) -> Result<Self, Self::Error> {
        validate_slack_id("user.team_id", &raw.team_id, b"T")?;
        validate_slack_id("user.id", &raw.id, b"UW")?;
        for (field, value) in [
            ("user.name", raw.name.as_str()),
            ("user.display_name", raw.display_name.as_str()),
            ("user.real_name", raw.real_name.as_str()),
        ] {
            validate_bounded_metadata_text(field, value, MAX_HOSTED_SLACK_NAME_BYTES)?;
        }
        let updated_at = raw
            .updated_ts
            .as_deref()
            .map(|timestamp| canonicalize_slack_timestamp("user.updated_ts", timestamp))
            .transpose()?;
        Ok(Self {
            team_id: raw.team_id,
            user_id: raw.id,
            name: raw.name,
            display_name: raw.display_name,
            real_name: raw.real_name,
            is_bot: raw.is_bot,
            deleted: raw.deleted,
            updated_at,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostedSlackMessage {
    channel_id: String,
    message_id: String,
    posted_at: String,
    thread_root_message_id: Option<String>,
    user_id: Option<String>,
    text: String,
    edited_at: Option<String>,
    deleted: bool,
    file_ids: Vec<String>,
}

impl HostedSlackMessage {
    pub fn message_id(&self) -> &str {
        &self.message_id
    }

    pub fn posted_at(&self) -> &str {
        &self.posted_at
    }

    pub fn thread_root_message_id(&self) -> Option<&str> {
        self.thread_root_message_id.as_deref()
    }

    pub fn user_id(&self) -> Option<&str> {
        self.user_id.as_deref()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn edited_at(&self) -> Option<&str> {
        self.edited_at.as_deref()
    }

    pub fn deleted(&self) -> bool {
        self.deleted
    }

    pub fn file_ids(&self) -> &[String] {
        &self.file_ids
    }
}

impl TryFrom<RawHostedSlackMessage> for HostedSlackMessage {
    type Error = HostedSlackPortableError;

    fn try_from(raw: RawHostedSlackMessage) -> Result<Self, Self::Error> {
        validate_slack_id(
            "message.channel_id",
            &raw.channel_id,
            HOSTED_SLACK_CONVERSATION_ID_PREFIXES,
        )?;
        validate_slack_timestamp("message.ts", &raw.ts)?;
        if let Some(thread_ts) = &raw.thread_ts {
            validate_slack_timestamp("message.thread_ts", thread_ts)?;
        }
        if let Some(user_id) = &raw.user_id {
            validate_slack_id("message.user_id", user_id, b"UW")?;
        }
        validate_bounded_text(
            "message.text",
            &raw.text,
            MAX_HOSTED_SLACK_MESSAGE_TEXT_BYTES,
        )?;
        let edited_at = raw
            .edited_ts
            .as_deref()
            .map(|timestamp| canonicalize_slack_timestamp("message.edited_ts", timestamp))
            .transpose()?;
        validate_collection_len(
            "message.file_ids",
            raw.file_ids.len(),
            MAX_HOSTED_SLACK_MESSAGE_FILES,
        )?;
        let file_ids = canonical_ids("message.file_ids", raw.file_ids, b"F")?;
        let thread_root_message_id = raw.thread_ts.filter(|thread_ts| thread_ts != &raw.ts);
        Ok(Self {
            channel_id: raw.channel_id,
            posted_at: canonicalize_slack_timestamp("message.ts", &raw.ts)?,
            message_id: raw.ts,
            thread_root_message_id,
            user_id: raw.user_id,
            text: raw.text,
            edited_at,
            deleted: raw.deleted,
            file_ids,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostedSlackThread {
    channel_id: String,
    root_message_id: String,
    reply_message_ids: Vec<String>,
}

impl HostedSlackThread {
    pub fn root_message_id(&self) -> &str {
        &self.root_message_id
    }

    pub fn reply_message_ids(&self) -> &[String] {
        &self.reply_message_ids
    }
}

impl TryFrom<RawHostedSlackThread> for HostedSlackThread {
    type Error = HostedSlackPortableError;

    fn try_from(raw: RawHostedSlackThread) -> Result<Self, Self::Error> {
        validate_slack_id(
            "thread.channel_id",
            &raw.channel_id,
            HOSTED_SLACK_CONVERSATION_ID_PREFIXES,
        )?;
        validate_slack_timestamp("thread.root_ts", &raw.root_ts)?;
        validate_collection_len(
            "thread.reply_ts",
            raw.reply_ts.len(),
            MAX_HOSTED_SLACK_THREAD_REPLIES,
        )?;
        let mut reply_message_ids = raw.reply_ts;
        for reply in &reply_message_ids {
            validate_slack_timestamp("thread.reply_ts", reply)?;
        }
        reply_message_ids.sort();
        ensure_unique("thread.reply_ts", &reply_message_ids)?;
        Ok(Self {
            channel_id: raw.channel_id,
            root_message_id: raw.root_ts,
            reply_message_ids,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostedSlackFileCaptureReceipt {
    status: HostedSlackFileCaptureStatus,
}

impl HostedSlackFileCaptureReceipt {
    fn bytes_not_captured() -> Self {
        Self {
            status: HostedSlackFileCaptureStatus::BytesNotCaptured,
        }
    }

    pub fn status(&self) -> HostedSlackFileCaptureStatus {
        self.status
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostedSlackFileCaptureStatus {
    BytesNotCaptured,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostedSlackFileMetadata {
    channel_id: String,
    file_id: String,
    user_id: Option<String>,
    name: String,
    title: String,
    mimetype: String,
    byte_length: u64,
    created_at: String,
    deleted: bool,
    capture_receipt: HostedSlackFileCaptureReceipt,
}

impl HostedSlackFileMetadata {
    pub fn file_id(&self) -> &str {
        &self.file_id
    }

    pub fn user_id(&self) -> Option<&str> {
        self.user_id.as_deref()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn mimetype(&self) -> &str {
        &self.mimetype
    }

    pub fn byte_length(&self) -> u64 {
        self.byte_length
    }

    pub fn created_at(&self) -> &str {
        &self.created_at
    }

    pub fn deleted(&self) -> bool {
        self.deleted
    }

    pub fn capture_receipt(&self) -> &HostedSlackFileCaptureReceipt {
        &self.capture_receipt
    }
}

impl TryFrom<RawHostedSlackFileMetadata> for HostedSlackFileMetadata {
    type Error = HostedSlackPortableError;

    fn try_from(raw: RawHostedSlackFileMetadata) -> Result<Self, Self::Error> {
        validate_slack_id(
            "file.channel_id",
            &raw.channel_id,
            HOSTED_SLACK_CONVERSATION_ID_PREFIXES,
        )?;
        validate_slack_id("file.id", &raw.id, b"F")?;
        if let Some(user_id) = &raw.user_id {
            validate_slack_id("file.user_id", user_id, b"UW")?;
        }
        validate_bounded_metadata_text("file.name", &raw.name, MAX_HOSTED_SLACK_NAME_BYTES)?;
        validate_bounded_metadata_text("file.title", &raw.title, MAX_HOSTED_SLACK_NAME_BYTES)?;
        validate_mimetype(&raw.mimetype)?;
        Ok(Self {
            channel_id: raw.channel_id,
            file_id: raw.id,
            user_id: raw.user_id,
            name: raw.name,
            title: raw.title,
            mimetype: raw.mimetype,
            byte_length: raw.byte_length,
            created_at: canonicalize_slack_timestamp("file.created_ts", &raw.created_ts)?,
            deleted: raw.deleted,
            capture_receipt: HostedSlackFileCaptureReceipt::bytes_not_captured(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostedSlackNativeSnapshot {
    installation_id: SlackInstallationId,
    channel: HostedSlackChannel,
    users: Vec<HostedSlackUser>,
    messages: Vec<HostedSlackMessage>,
    threads: Vec<HostedSlackThread>,
    files: Vec<HostedSlackFileMetadata>,
}

impl HostedSlackNativeSnapshot {
    pub fn installation_id(&self) -> &SlackInstallationId {
        &self.installation_id
    }

    pub fn channel(&self) -> &HostedSlackChannel {
        &self.channel
    }

    pub fn users(&self) -> &[HostedSlackUser] {
        &self.users
    }

    pub fn messages(&self) -> &[HostedSlackMessage] {
        &self.messages
    }

    pub fn threads(&self) -> &[HostedSlackThread] {
        &self.threads
    }

    pub fn files(&self) -> &[HostedSlackFileMetadata] {
        &self.files
    }

    pub fn channel_identity(&self) -> (&SlackInstallationId, &str, &str) {
        (
            &self.installation_id,
            self.channel.team_id.as_str(),
            self.channel.channel_id.as_str(),
        )
    }
}

impl TryFrom<RawHostedSlackNativeSnapshot> for HostedSlackNativeSnapshot {
    type Error = HostedSlackPortableError;

    fn try_from(raw: RawHostedSlackNativeSnapshot) -> Result<Self, Self::Error> {
        for (field, actual) in [
            ("users", raw.users.len()),
            ("messages", raw.messages.len()),
            ("threads", raw.threads.len()),
            ("files", raw.files.len()),
        ] {
            validate_collection_len(field, actual, MAX_HOSTED_SLACK_COLLECTION_ENTRIES)?;
        }
        for message in &raw.messages {
            validate_collection_len(
                "message.file_ids",
                message.file_ids.len(),
                MAX_HOSTED_SLACK_MESSAGE_FILES,
            )?;
        }
        for thread in &raw.threads {
            validate_collection_len(
                "thread.reply_ts",
                thread.reply_ts.len(),
                MAX_HOSTED_SLACK_THREAD_REPLIES,
            )?;
        }
        let reference_count = raw
            .messages
            .iter()
            .map(|message| message.file_ids.len())
            .chain(raw.threads.iter().map(|thread| thread.reply_ts.len()))
            .fold(0usize, usize::saturating_add);
        validate_collection_len(
            "snapshot.references",
            reference_count,
            MAX_HOSTED_SLACK_SNAPSHOT_REFERENCES,
        )?;

        let channel = HostedSlackChannel::try_from(raw.channel)?;
        let expected_team_id = channel.team_id.as_str();
        let expected_channel_id = channel.channel_id.as_str();

        let mut users = raw
            .users
            .into_iter()
            .map(HostedSlackUser::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        if users.iter().any(|user| user.team_id != expected_team_id) {
            return Err(HostedSlackPortableError::IdentityMismatch("user.team_id"));
        }
        users.sort_by(|left, right| left.user_id.cmp(&right.user_id));
        ensure_unique_by("users.user_id", &users, |user| user.user_id.as_str())?;

        let mut messages = raw
            .messages
            .into_iter()
            .map(HostedSlackMessage::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        if messages
            .iter()
            .any(|message| message.channel_id != expected_channel_id)
        {
            return Err(HostedSlackPortableError::IdentityMismatch(
                "message.channel_id",
            ));
        }
        messages.sort_by(|left, right| left.message_id.cmp(&right.message_id));
        ensure_unique_by("messages.message_id", &messages, |message| {
            message.message_id.as_str()
        })?;

        let mut threads = raw
            .threads
            .into_iter()
            .map(HostedSlackThread::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        if threads
            .iter()
            .any(|thread| thread.channel_id != expected_channel_id)
        {
            return Err(HostedSlackPortableError::IdentityMismatch(
                "thread.channel_id",
            ));
        }
        threads.sort_by(|left, right| left.root_message_id.cmp(&right.root_message_id));
        ensure_unique_by("threads.root_message_id", &threads, |thread| {
            thread.root_message_id.as_str()
        })?;

        let mut files = raw
            .files
            .into_iter()
            .map(HostedSlackFileMetadata::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        if files
            .iter()
            .any(|file| file.channel_id != expected_channel_id)
        {
            return Err(HostedSlackPortableError::IdentityMismatch(
                "file.channel_id",
            ));
        }
        files.sort_by(|left, right| left.file_id.cmp(&right.file_id));
        ensure_unique_by("files.file_id", &files, |file| file.file_id.as_str())?;

        validate_snapshot_string_bytes(&channel, &users, &messages, &threads, &files)?;
        validate_snapshot_graph(&users, &messages, &threads, &files)?;

        Ok(Self {
            installation_id: raw.installation_id,
            channel,
            users,
            messages,
            threads,
            files,
        })
    }
}

fn validate_snapshot_string_bytes(
    channel: &HostedSlackChannel,
    users: &[HostedSlackUser],
    messages: &[HostedSlackMessage],
    threads: &[HostedSlackThread],
    files: &[HostedSlackFileMetadata],
) -> Result<(), HostedSlackPortableError> {
    let mut total = 0usize;
    add_string_bytes(&mut total, &channel.team_id);
    add_string_bytes(&mut total, &channel.channel_id);
    add_string_bytes(&mut total, &channel.name);
    add_optional_string_bytes(&mut total, channel.topic.as_deref());
    add_optional_string_bytes(&mut total, channel.purpose.as_deref());
    add_string_bytes(&mut total, &channel.created_at);
    add_optional_string_bytes(&mut total, channel.updated_at.as_deref());

    for user in users {
        for value in [
            user.team_id.as_str(),
            user.user_id.as_str(),
            user.name.as_str(),
            user.display_name.as_str(),
            user.real_name.as_str(),
        ] {
            add_string_bytes(&mut total, value);
        }
        add_optional_string_bytes(&mut total, user.updated_at.as_deref());
    }
    for message in messages {
        for value in [
            message.channel_id.as_str(),
            message.message_id.as_str(),
            message.posted_at.as_str(),
            message.text.as_str(),
        ] {
            add_string_bytes(&mut total, value);
        }
        add_optional_string_bytes(&mut total, message.thread_root_message_id.as_deref());
        add_optional_string_bytes(&mut total, message.user_id.as_deref());
        add_optional_string_bytes(&mut total, message.edited_at.as_deref());
        for file_id in &message.file_ids {
            add_string_bytes(&mut total, file_id);
        }
    }
    for thread in threads {
        add_string_bytes(&mut total, &thread.channel_id);
        add_string_bytes(&mut total, &thread.root_message_id);
        for reply_id in &thread.reply_message_ids {
            add_string_bytes(&mut total, reply_id);
        }
    }
    for file in files {
        for value in [
            file.channel_id.as_str(),
            file.file_id.as_str(),
            file.name.as_str(),
            file.title.as_str(),
            file.mimetype.as_str(),
            file.created_at.as_str(),
        ] {
            add_string_bytes(&mut total, value);
        }
        add_optional_string_bytes(&mut total, file.user_id.as_deref());
    }

    if total > MAX_HOSTED_SLACK_SNAPSHOT_STRING_BYTES {
        return Err(HostedSlackPortableError::ValueTooLong {
            field: "snapshot.string_bytes",
            maximum_bytes: MAX_HOSTED_SLACK_SNAPSHOT_STRING_BYTES,
            actual_bytes: total,
        });
    }
    Ok(())
}

fn add_string_bytes(total: &mut usize, value: &str) {
    *total = total.saturating_add(value.len());
}

fn add_optional_string_bytes(total: &mut usize, value: Option<&str>) {
    if let Some(value) = value {
        add_string_bytes(total, value);
    }
}

fn validate_snapshot_graph(
    users: &[HostedSlackUser],
    messages: &[HostedSlackMessage],
    threads: &[HostedSlackThread],
    files: &[HostedSlackFileMetadata],
) -> Result<(), HostedSlackPortableError> {
    let user_ids = users
        .iter()
        .map(|user| user.user_id.as_str())
        .collect::<BTreeSet<_>>();
    let messages_by_id = messages
        .iter()
        .map(|message| (message.message_id.as_str(), message))
        .collect::<BTreeMap<_, _>>();
    let file_ids = files
        .iter()
        .map(|file| file.file_id.as_str())
        .collect::<BTreeSet<_>>();

    for message in messages {
        if message
            .user_id
            .as_deref()
            .is_some_and(|user_id| !user_ids.contains(user_id))
        {
            return Err(HostedSlackPortableError::MissingReference(
                "messages.user_id",
            ));
        }
        if message
            .file_ids
            .iter()
            .any(|file_id| !file_ids.contains(file_id.as_str()))
        {
            return Err(HostedSlackPortableError::MissingReference(
                "messages.file_ids",
            ));
        }
    }
    if files.iter().any(|file| {
        file.user_id
            .as_deref()
            .is_some_and(|user_id| !user_ids.contains(user_id))
    }) {
        return Err(HostedSlackPortableError::MissingReference("files.user_id"));
    }

    let thread_root_ids = threads
        .iter()
        .map(|thread| thread.root_message_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut referenced_reply_ids = BTreeSet::new();
    for thread in threads {
        let root = messages_by_id.get(thread.root_message_id.as_str()).ok_or(
            HostedSlackPortableError::MissingReference("threads.root_message_id"),
        )?;
        if root.thread_root_message_id.is_some() {
            return Err(HostedSlackPortableError::InvalidRelationship(
                "threads.root_message_id",
            ));
        }
        for reply_id in &thread.reply_message_ids {
            if !referenced_reply_ids.insert(reply_id.as_str()) {
                return Err(HostedSlackPortableError::DuplicateReference(
                    "threads.reply_message_ids",
                ));
            }
            let reply = messages_by_id.get(reply_id.as_str()).ok_or(
                HostedSlackPortableError::MissingReference("threads.reply_message_ids"),
            )?;
            if reply.thread_root_message_id.as_deref() != Some(thread.root_message_id.as_str()) {
                return Err(HostedSlackPortableError::InvalidRelationship(
                    "threads.reply_message_ids",
                ));
            }
        }
    }

    for message in messages {
        let Some(root_id) = message.thread_root_message_id.as_deref() else {
            if !thread_root_ids.contains(message.message_id.as_str()) {
                return Err(HostedSlackPortableError::MissingReference(
                    "messages.thread_record",
                ));
            }
            continue;
        };
        let root =
            messages_by_id
                .get(root_id)
                .ok_or(HostedSlackPortableError::MissingReference(
                    "messages.thread_root_message_id",
                ))?;
        if root.thread_root_message_id.is_some() || !thread_root_ids.contains(root_id) {
            return Err(HostedSlackPortableError::InvalidRelationship(
                "messages.thread_root_message_id",
            ));
        }
        if !referenced_reply_ids.contains(message.message_id.as_str()) {
            return Err(HostedSlackPortableError::MissingReference(
                "messages.thread_membership",
            ));
        }
    }

    Ok(())
}

fn validate_optional_metadata_text(
    field: &'static str,
    value: Option<&str>,
    maximum_bytes: usize,
) -> Result<(), HostedSlackPortableError> {
    value
        .map(|value| validate_bounded_metadata_text(field, value, maximum_bytes))
        .transpose()
        .map(|_| ())
}

fn validate_mimetype(value: &str) -> Result<(), HostedSlackPortableError> {
    validate_bounded_metadata_text("file.mimetype", value, MAX_HOSTED_SLACK_NAME_BYTES)?;
    let Some((type_name, subtype_name)) = value.split_once('/') else {
        return Err(HostedSlackPortableError::InvalidMimetype);
    };
    if subtype_name.contains('/')
        || !valid_mimetype_component(type_name)
        || !valid_mimetype_component(subtype_name)
    {
        return Err(HostedSlackPortableError::InvalidMimetype);
    }
    Ok(())
}

fn valid_mimetype_component(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(byte) if byte.is_ascii_alphanumeric())
        && bytes.all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-'
                )
        })
}

fn validate_slack_timestamp(
    field: &'static str,
    value: &str,
) -> Result<(), HostedSlackPortableError> {
    let Some((seconds, micros)) = value.split_once('.') else {
        return Err(HostedSlackPortableError::InvalidTimestamp(field));
    };
    if seconds.is_empty()
        || seconds.len() > 12
        || (seconds.len() > 1 && seconds.starts_with('0'))
        || !seconds.bytes().all(|byte| byte.is_ascii_digit())
        || micros.len() != 6
        || !micros.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(HostedSlackPortableError::InvalidTimestamp(field));
    }
    let seconds = seconds
        .parse::<i64>()
        .map_err(|_| HostedSlackPortableError::InvalidTimestamp(field))?;
    let micros = micros
        .parse::<u32>()
        .map_err(|_| HostedSlackPortableError::InvalidTimestamp(field))?;
    DateTime::<Utc>::from_timestamp(seconds, micros * 1_000)
        .ok_or(HostedSlackPortableError::InvalidTimestamp(field))?;
    Ok(())
}

fn canonicalize_slack_timestamp(
    field: &'static str,
    value: &str,
) -> Result<String, HostedSlackPortableError> {
    validate_slack_timestamp(field, value)?;
    let (seconds, micros) = value.split_once('.').expect("validated Slack timestamp");
    let timestamp = DateTime::<Utc>::from_timestamp(
        seconds
            .parse::<i64>()
            .expect("validated Slack timestamp seconds"),
        micros
            .parse::<u32>()
            .expect("validated Slack timestamp micros")
            * 1_000,
    )
    .expect("validated Slack timestamp range");
    Ok(timestamp.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string())
}

fn canonical_ids(
    field: &'static str,
    mut values: Vec<String>,
    prefixes: &[u8],
) -> Result<Vec<String>, HostedSlackPortableError> {
    for value in &values {
        validate_slack_id(field, value, prefixes)?;
    }
    values.sort();
    ensure_unique(field, &values)?;
    Ok(values)
}

fn ensure_unique(field: &'static str, values: &[String]) -> Result<(), HostedSlackPortableError> {
    if values.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(HostedSlackPortableError::DuplicateValue(field));
    }
    Ok(())
}

fn ensure_unique_by<T, F>(
    field: &'static str,
    values: &[T],
    key: F,
) -> Result<(), HostedSlackPortableError>
where
    F: Fn(&T) -> &str,
{
    if values.windows(2).any(|pair| key(&pair[0]) == key(&pair[1])) {
        return Err(HostedSlackPortableError::DuplicateValue(field));
    }
    Ok(())
}
