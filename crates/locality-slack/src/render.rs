use std::collections::BTreeMap;

use chrono::{TimeZone, Utc};
use locality_core::model::{CanonicalDocument, RemoteId};
use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};

use crate::dto::{SlackMessage, SlackUser};
use crate::oauth::SLACK_CONNECTOR_ID;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlackContentKind {
    Recent,
    Thread,
}

impl SlackContentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Recent => "recent",
            Self::Thread => "thread",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackNativeBundle {
    pub content_kind: SlackContentKind,
    pub channel_id: String,
    pub channel_name: String,
    pub recent_limit: u32,
    pub thread_ts: Option<String>,
    pub messages: Vec<SlackMessage>,
    pub users: BTreeMap<String, SlackUser>,
}

pub fn slack_recent_remote_id(channel_id: &str) -> RemoteId {
    RemoteId::new(format!("slack-recent:{channel_id}"))
}

pub fn parse_slack_recent_remote_id(remote_id: &RemoteId) -> Option<&str> {
    remote_id.as_str().strip_prefix("slack-recent:")
}

pub fn slack_thread_remote_id(channel_id: &str, thread_ts: &str) -> RemoteId {
    RemoteId::new(format!("slack-thread:{channel_id}:{thread_ts}"))
}

pub fn parse_slack_thread_remote_id(remote_id: &RemoteId) -> Option<(&str, &str)> {
    let rest = remote_id.as_str().strip_prefix("slack-thread:")?;
    rest.split_once(':')
}

pub fn slack_channel_remote_id(channel_id: &str) -> RemoteId {
    RemoteId::new(format!("slack-channel:{channel_id}"))
}

pub fn parse_slack_channel_remote_id(remote_id: &RemoteId) -> Option<&str> {
    remote_id.as_str().strip_prefix("slack-channel:")
}

pub fn slack_threads_remote_id(channel_id: &str) -> RemoteId {
    RemoteId::new(format!("slack-threads:{channel_id}"))
}

pub fn parse_slack_threads_remote_id(remote_id: &RemoteId) -> Option<&str> {
    remote_id.as_str().strip_prefix("slack-threads:")
}

pub fn render_slack_document(bundle: &SlackNativeBundle) -> LocalityResult<CanonicalDocument> {
    if bundle.channel_id.trim().is_empty() {
        return Err(LocalityError::InvalidState(
            "Slack bundle is missing its channel id".to_string(),
        ));
    }
    let body = match bundle.content_kind {
        SlackContentKind::Recent => render_messages(bundle, false),
        SlackContentKind::Thread => render_messages(bundle, true),
    };
    Ok(CanonicalDocument::new(frontmatter(bundle), body))
}

pub fn remote_version(bundle: &SlackNativeBundle) -> String {
    match bundle.content_kind {
        SlackContentKind::Recent => slack_recent_version(&bundle.channel_id, &bundle.messages),
        SlackContentKind::Thread => slack_thread_version(
            &bundle.channel_id,
            bundle.thread_ts.as_deref().unwrap_or(""),
            &bundle.messages,
        ),
    }
}

pub fn slack_recent_version(channel_id: &str, messages: &[SlackMessage]) -> String {
    format!(
        "slack:{channel_id}:recent:{}",
        latest_message_ts(messages).unwrap_or("")
    )
}

pub fn slack_thread_version(
    channel_id: &str,
    thread_ts: &str,
    messages: &[SlackMessage],
) -> String {
    format!(
        "slack:{channel_id}:thread:{thread_ts}:{}",
        latest_message_ts(messages).unwrap_or(thread_ts)
    )
}

pub fn latest_message_ts(messages: &[SlackMessage]) -> Option<&str> {
    messages
        .iter()
        .filter_map(|message| {
            message
                .latest_reply
                .as_deref()
                .or_else(|| Some(message.ts.as_str()))
        })
        .max()
}

pub fn thread_file_name(thread_ts: &str) -> String {
    format!("{}-{thread_ts}.md", slack_timestamp_file_stem(thread_ts))
}

pub fn slack_timestamp_file_stem(ts: &str) -> String {
    slack_timestamp(ts)
        .map(|value| value.format("%Y-%m-%d-%H.%M.%S").to_string())
        .unwrap_or_else(|| safe_filename(ts, 64))
}

fn frontmatter(bundle: &SlackNativeBundle) -> String {
    let remote_id = match bundle.content_kind {
        SlackContentKind::Recent => slack_recent_remote_id(&bundle.channel_id),
        SlackContentKind::Thread => slack_thread_remote_id(
            &bundle.channel_id,
            bundle.thread_ts.as_deref().unwrap_or(""),
        ),
    };
    let title = match bundle.content_kind {
        SlackContentKind::Recent => format!("#{} recent Slack history", bundle.channel_name),
        SlackContentKind::Thread => format!(
            "#{} Slack thread {}",
            bundle.channel_name,
            bundle.thread_ts.as_deref().unwrap_or("")
        ),
    };
    let latest_ts = latest_message_ts(&bundle.messages).unwrap_or("");
    let mut output = format!(
        "loc:\n  id: {}\n  type: page\n  connector: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\nslack:\n  channel_id: {}\n  channel_name: {}\n  content_kind: {}\n  recent_limit: {}\n  latest_ts: {}\n",
        yaml_scalar(remote_id.as_str()),
        SLACK_CONNECTOR_ID,
        yaml_scalar(&remote_version(bundle)),
        yaml_scalar(&remote_version(bundle)),
        yaml_scalar(&title),
        yaml_scalar(&bundle.channel_id),
        yaml_scalar(&bundle.channel_name),
        bundle.content_kind.as_str(),
        bundle.recent_limit,
        yaml_scalar(latest_ts),
    );
    match bundle.thread_ts.as_deref() {
        Some(thread_ts) => output.push_str(&format!("  thread_ts: {}\n", yaml_scalar(thread_ts))),
        None => output.push_str("  thread_ts: null\n"),
    }
    output
}

fn render_messages(bundle: &SlackNativeBundle, sort_by_timestamp: bool) -> String {
    let mut messages = bundle.messages.iter().collect::<Vec<_>>();
    if sort_by_timestamp {
        messages.sort_by(|left, right| left.ts.cmp(&right.ts));
    }

    let mut output = String::new();
    if messages.is_empty() {
        return "_No Slack messages were returned for this view._\n".to_string();
    }
    for message in messages {
        output.push_str(&format!(
            "## {} - {}\n\n",
            display_timestamp(&message.ts),
            sender_label(message, &bundle.users),
        ));
        output.push_str(&message_markdown(message));
        if !output.ends_with("\n\n") {
            if !output.ends_with('\n') {
                output.push('\n');
            }
            output.push('\n');
        }
        if let Some(reply_count) = message.reply_count.filter(|count| *count > 0) {
            output.push_str(&format!(
                "Thread: {reply_count} {}\n\n",
                if reply_count == 1 { "reply" } else { "replies" }
            ));
        }
        if let Some(permalink) = message
            .permalink
            .as_deref()
            .filter(|permalink| !permalink.trim().is_empty())
        {
            output.push_str(&format!("Permalink: {permalink}\n\n"));
        }
        output.push_str(&format!(
            "Slack ID: `{}/{}`\n\n",
            bundle.channel_id, message.ts
        ));
    }
    output
}

fn message_markdown(message: &SlackMessage) -> String {
    let mut parts = Vec::new();
    if !message.text.trim().is_empty() {
        parts.push(escape_locality_directive_lines(&message.text));
    }
    for block in &message.blocks {
        match block_text(block) {
            Some(text) if !text.trim().is_empty() => {
                parts.push(escape_locality_directive_lines(&text));
            }
            _ => parts
                .push("::loc{{unsupported source=\"slack\" kind=\"rich_block\"}}\n".to_string()),
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        let mut output = parts.join("\n\n");
        if !output.ends_with('\n') {
            output.push('\n');
        }
        output
    }
}

fn block_text(block: &serde_json::Value) -> Option<String> {
    let block_type = block.get("type").and_then(serde_json::Value::as_str);
    match block_type {
        Some("section") | Some("context") | Some("rich_text") => text_field(block),
        _ => None,
    }
}

fn text_field(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value.get("text") {
        if let Some(text) = text.as_str() {
            return Some(text.to_string());
        }
        if let Some(text) = text.get("text").and_then(serde_json::Value::as_str) {
            return Some(text.to_string());
        }
    }
    if let Some(elements) = value.get("elements").and_then(serde_json::Value::as_array) {
        let text = elements
            .iter()
            .filter_map(text_field)
            .collect::<Vec<_>>()
            .join(" ");
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    None
}

fn sender_label(message: &SlackMessage, users: &BTreeMap<String, SlackUser>) -> String {
    let Some(user_id) = message.user.as_deref() else {
        return "Unknown Slack sender".to_string();
    };
    users
        .get(user_id)
        .map(|user| {
            user.real_name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or(&user.name)
                .to_string()
        })
        .unwrap_or_else(|| user_id.to_string())
}

fn display_timestamp(ts: &str) -> String {
    slack_timestamp(ts)
        .map(|value| value.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn slack_timestamp(ts: &str) -> Option<chrono::DateTime<Utc>> {
    let (seconds, fraction) = ts.split_once('.').unwrap_or((ts, ""));
    let seconds = seconds.parse::<i64>().ok()?;
    let mut nanos = fraction
        .chars()
        .take(9)
        .filter(|value| value.is_ascii_digit())
        .collect::<String>();
    while nanos.len() < 9 {
        nanos.push('0');
    }
    let nanos = nanos.parse::<u32>().ok()?;
    Utc.timestamp_opt(seconds, nanos).single()
}

fn yaml_scalar(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn escape_locality_directive_lines(value: &str) -> String {
    value
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("::loc{") {
                format!("\\{line}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn safe_filename(value: &str, max_chars: usize) -> String {
    let mut output = value
        .chars()
        .map(|character| match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            character if character.is_control() => '-',
            character => character,
        })
        .collect::<String>();
    output = output.trim_matches([' ', '.']).trim().to_string();
    if output.is_empty() {
        output = "untitled".to_string();
    }
    if output.chars().count() > max_chars {
        output = output.chars().take(max_chars).collect::<String>();
        output = output.trim_matches([' ', '.']).trim().to_string();
    }
    if output.is_empty() {
        "untitled".to_string()
    } else {
        output
    }
}
