use std::collections::BTreeMap;

use chrono::{DateTime, TimeZone, Utc};
use locality_core::model::CanonicalDocument;
use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};

use crate::connector::SLACK_CONNECTOR_ID;
use crate::dto::{SlackConversation, SlackMessage, SlackUser};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlackRenderedKind {
    Recent,
    Users,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SlackNativeBundle {
    pub kind: SlackRenderedKind,
    pub conversation: Option<SlackConversation>,
    pub users: Vec<SlackUser>,
    pub messages: Vec<SlackMessage>,
}

pub fn conversation_remote_id(conversation_id: &str) -> String {
    format!("slack-conversation:{conversation_id}")
}

pub fn recent_remote_id(conversation_id: &str) -> String {
    format!("slack-recent:{conversation_id}")
}

pub fn parse_recent_remote_id(remote_id: &str) -> Option<&str> {
    remote_id.strip_prefix("slack-recent:")
}

pub fn users_remote_id() -> &'static str {
    "slack-users"
}

pub fn render_slack_entity(bundle: &SlackNativeBundle) -> LocalityResult<CanonicalDocument> {
    match bundle.kind {
        SlackRenderedKind::Recent => render_recent(bundle),
        SlackRenderedKind::Users => render_users(bundle),
    }
}

fn render_recent(bundle: &SlackNativeBundle) -> LocalityResult<CanonicalDocument> {
    let conversation = bundle.conversation.as_ref().ok_or_else(|| {
        LocalityError::InvalidState("Slack recent bundle is missing conversation".to_string())
    })?;
    let remote_id = recent_remote_id(&conversation.id);
    let latest = bundle
        .messages
        .iter()
        .map(|message| message.ts.as_str())
        .max()
        .unwrap_or("empty");
    let title = conversation_title(conversation, &user_map(&bundle.users));
    let frontmatter = format!(
        "loc:\n  id: {}\n  type: page\n  connector: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\nslack:\n  conversation_id: {}\n  conversation_name: {}\n  rendered_kind: recent\n",
        yaml_scalar(&remote_id),
        SLACK_CONNECTOR_ID,
        yaml_scalar(latest),
        yaml_scalar(latest),
        yaml_scalar(&format!("{title} recent messages")),
        yaml_scalar(&conversation.id),
        yaml_scalar(&title),
    );
    Ok(CanonicalDocument::new(
        frontmatter,
        render_recent_body(bundle, &title),
    ))
}

fn render_users(bundle: &SlackNativeBundle) -> LocalityResult<CanonicalDocument> {
    let frontmatter = format!(
        "loc:\n  id: {}\n  type: page\n  connector: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\nslack:\n  rendered_kind: users\n",
        yaml_scalar(users_remote_id()),
        SLACK_CONNECTOR_ID,
        yaml_scalar("users"),
        yaml_scalar("users"),
        yaml_scalar("Slack users"),
    );
    let mut users = bundle.users.clone();
    users.sort_by_key(user_display_name);
    let mut body = String::from(
        "| User ID | Name | Display Name | Bot | Deleted |\n| --- | --- | --- | --- | --- |\n",
    );
    for user in users {
        body.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            markdown_table_cell(&user.id),
            markdown_table_cell(user.name.as_deref().unwrap_or("")),
            markdown_table_cell(&user_display_name(&user)),
            user.is_bot,
            user.deleted,
        ));
    }
    Ok(CanonicalDocument::new(frontmatter, body))
}

fn render_recent_body(bundle: &SlackNativeBundle, title: &str) -> String {
    let users = user_map(&bundle.users);
    let mut messages = bundle.messages.clone();
    messages.sort_by(|left, right| left.ts.cmp(&right.ts));
    let mut body = format!("# {title}\n\n");
    if messages.is_empty() {
        body.push_str("_No recent Slack messages were returned for this conversation._\n");
        return body;
    }
    for message in messages {
        let author = message
            .user
            .as_deref()
            .and_then(|user_id| users.get(user_id))
            .cloned()
            .or_else(|| message.username.clone())
            .or_else(|| message.bot_id.clone())
            .unwrap_or_else(|| "Unknown".to_string());
        body.push_str(&format!(
            "## {}\n\n**{}**\n\n{}\n\n",
            slack_ts_heading(&message.ts),
            escape_markdown_inline(&author),
            slack_text_to_markdown(&message.text, &users),
        ));
        if let Some(reply_count) = message.reply_count.filter(|count| *count > 0) {
            body.push_str(&format!("_Thread replies: {reply_count}_\n\n"));
        }
        if !message.files.is_empty() {
            body.push_str("Files:\n");
            for file in &message.files {
                let name = file
                    .title
                    .as_deref()
                    .or(file.name.as_deref())
                    .unwrap_or(file.id.as_str());
                let mimetype = file.mimetype.as_deref().unwrap_or("unknown");
                body.push_str(&format!(
                    "- {} ({}, id `{}`)\n",
                    escape_markdown_inline(name),
                    escape_markdown_inline(mimetype),
                    file.id
                ));
            }
            body.push('\n');
        }
    }
    body
}

fn user_map(users: &[SlackUser]) -> BTreeMap<String, String> {
    users
        .iter()
        .map(|user| (user.id.clone(), user_display_name(user)))
        .collect()
}

fn user_display_name(user: &SlackUser) -> String {
    user.profile
        .as_ref()
        .and_then(|profile| profile.real_name.as_deref())
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            user.profile
                .as_ref()
                .and_then(|profile| profile.display_name.as_deref())
        })
        .filter(|value| !value.trim().is_empty())
        .or(user.real_name.as_deref())
        .filter(|value| !value.trim().is_empty())
        .or(user.name.as_deref())
        .unwrap_or(user.id.as_str())
        .to_string()
}

fn conversation_title(
    conversation: &SlackConversation,
    users: &BTreeMap<String, String>,
) -> String {
    conversation
        .name
        .clone()
        .or_else(|| {
            conversation
                .user
                .as_ref()
                .and_then(|user_id| users.get(user_id).cloned())
        })
        .unwrap_or_else(|| conversation.id.clone())
}

fn slack_ts_heading(ts: &str) -> String {
    let seconds = ts
        .split('.')
        .next()
        .and_then(|value| value.parse::<i64>().ok());
    seconds
        .and_then(|seconds| Utc.timestamp_opt(seconds, 0).single())
        .map(|value: DateTime<Utc>| value.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| escape_markdown_inline(ts))
}

fn slack_text_to_markdown(value: &str, users: &BTreeMap<String, String>) -> String {
    let mut output = value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    for (user_id, display) in users {
        output = output.replace(&format!("<@{user_id}>"), &format!("@{display}"));
    }
    let mut converted = String::new();
    let mut rest = output.as_str();
    while let Some(start) = rest.find('<') {
        converted.push_str(&escape_locality_directive_lines(&rest[..start]));
        let after_start = &rest[start + 1..];
        let Some(end) = after_start.find('>') else {
            converted.push('<');
            rest = after_start;
            continue;
        };
        let token = &after_start[..end];
        if let Some((url, label)) = token
            .split_once('|')
            .filter(|(url, _)| url.starts_with("http"))
        {
            converted.push_str(&format!(
                "[{}]({})",
                escape_markdown_link_label(label),
                escape_markdown_link_destination(url)
            ));
        } else if token.starts_with("http") {
            converted.push_str(&escape_markdown_link_destination(token));
        } else if let Some((channel_id, label)) = token
            .strip_prefix('#')
            .and_then(|value| value.split_once('|'))
        {
            let label = if label.is_empty() { channel_id } else { label };
            converted.push('#');
            converted.push_str(&escape_markdown_link_label(label));
        } else {
            converted.push('<');
            converted.push_str(&escape_markdown_inline(token));
            converted.push('>');
        }
        rest = &after_start[end + 1..];
    }
    converted.push_str(&escape_locality_directive_lines(rest));
    ensure_trailing_newline(converted)
}

fn escape_locality_directive_lines(value: &str) -> String {
    value
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("::loc") || trimmed.starts_with("::afs") {
                format!("\\{line}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn ensure_trailing_newline(mut value: String) -> String {
    if !value.ends_with('\n') {
        value.push('\n');
    }
    value
}

fn markdown_table_cell(value: &str) -> String {
    value.replace('|', "\\|").replace('\n', " ")
}

fn escape_markdown_inline(value: &str) -> String {
    normalize_inline_text(value)
}

fn escape_markdown_link_label(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '[' => output.push_str("\\["),
            ']' => output.push_str("\\]"),
            '\r' | '\n' => output.push(' '),
            _ => output.push(character),
        }
    }
    output
}

fn escape_markdown_link_destination(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => output.push_str("%5C"),
            ')' => output.push_str("%29"),
            '\r' => output.push_str("%0D"),
            '\n' => output.push_str("%0A"),
            _ => output.push(character),
        }
    }
    output
}

fn normalize_inline_text(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '\r' | '\n' => ' ',
            _ => character,
        })
        .collect()
}

fn yaml_scalar(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::{SlackConversation, SlackMessage, SlackUser, SlackUserProfile};

    fn render_recent_message(text: &str, ts: &str) -> CanonicalDocument {
        let bundle = SlackNativeBundle {
            kind: SlackRenderedKind::Recent,
            conversation: Some(SlackConversation {
                id: "C123".to_string(),
                name: Some("general".to_string()),
                is_channel: true,
                ..SlackConversation::default()
            }),
            users: Vec::new(),
            messages: vec![SlackMessage {
                text: text.to_string(),
                ts: ts.to_string(),
                ..SlackMessage::default()
            }],
        };

        render_slack_entity(&bundle).expect("render")
    }

    #[test]
    fn renders_recent_messages_with_frontmatter_and_names() {
        let bundle = SlackNativeBundle {
            kind: SlackRenderedKind::Recent,
            conversation: Some(SlackConversation {
                id: "C123".to_string(),
                name: Some("general".to_string()),
                is_channel: true,
                ..SlackConversation::default()
            }),
            users: vec![SlackUser {
                id: "U123".to_string(),
                name: Some("ada".to_string()),
                profile: Some(SlackUserProfile {
                    display_name: Some("Ada".to_string()),
                    real_name: Some("Ada Lovelace".to_string()),
                    ..SlackUserProfile::default()
                }),
                ..SlackUser::default()
            }],
            messages: vec![SlackMessage {
                user: Some("U123".to_string()),
                text: "hello <https://example.com|example>".to_string(),
                ts: "1780000000.000100".to_string(),
                thread_ts: Some("1780000000.000100".to_string()),
                reply_count: Some(2),
                ..SlackMessage::default()
            }],
        };

        let document = render_slack_entity(&bundle).expect("render");

        assert!(document.frontmatter.contains("  connector: slack\n"));
        assert!(
            document
                .frontmatter
                .contains("  conversation_id: \"C123\"\n")
        );
        assert!(document.body.contains("**Ada Lovelace**"));
        assert!(document.body.contains("[example](https://example.com)"));
        assert!(document.body.contains("_Thread replies: 2_"));
    }

    #[test]
    fn renders_users_directory_without_email() {
        let bundle = SlackNativeBundle {
            kind: SlackRenderedKind::Users,
            conversation: None,
            messages: Vec::new(),
            users: vec![SlackUser {
                id: "U123".to_string(),
                name: Some("ada".to_string()),
                real_name: Some("Ada Lovelace".to_string()),
                ..SlackUser::default()
            }],
        };

        let document = render_slack_entity(&bundle).expect("render users");

        assert!(document.frontmatter.contains("  id: \"slack-users\"\n"));
        assert!(
            document
                .body
                .contains("| User ID | Name | Display Name | Bot | Deleted |")
        );
        assert!(
            document
                .body
                .contains("| U123 | ada | Ada Lovelace | false | false |")
        );
        assert!(!document.body.contains("email"));
    }

    #[test]
    fn escapes_slack_message_directive_lines() {
        let document =
            render_recent_message("safe\n::loc\n::loc{sync}\n::afs{sync}", "1780000000.000100");

        assert!(document.body.contains("\\::loc\n"));
        assert!(document.body.contains("\\::loc{sync}\n"));
        assert!(document.body.contains("\\::afs{sync}\n"));
        assert!(!document.body.contains("\n::loc"));
        assert!(!document.body.contains("\n::afs"));
    }

    #[test]
    fn sanitizes_malformed_timestamp_headings() {
        let document = render_recent_message("safe", "bad\n::loc{timestamp}");

        assert!(document.body.contains("## bad ::loc{timestamp}"));
        assert!(!document.body.contains("\n::loc"));
    }

    #[test]
    fn escapes_slack_link_labels_and_destinations() {
        let document = render_recent_message(
            "see <https://example.com/a)b\n::loc{evil}|bad ] [label> and <#C123|eng]ops>",
            "1780000000.000100",
        );

        assert!(
            document
                .body
                .contains("[bad \\] \\[label](https://example.com/a%29b%0A::loc{evil})")
        );
        assert!(document.body.contains("#eng\\]ops"));
        assert!(!document.body.contains("\n::loc"));
    }
}
