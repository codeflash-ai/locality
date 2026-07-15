use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use locality_core::model::{CanonicalDocument, RemoteId};
use locality_core::shadow::{ShadowDocument, segment_markdown_body};
use locality_core::validation::ValidationIssue;
use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};

use crate::attachments::{GmailAttachmentSpec, collect_attachment_specs};
use crate::dto::{GmailMessage, GmailMessagePart, GmailThread, header_map};
use crate::oauth::GMAIL_CONNECTOR_ID;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GmailNativeBundle {
    pub mailbox: String,
    pub message: GmailMessage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GmailThreadNativeBundle {
    pub mailbox: String,
    pub thread: GmailThread,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GmailThreadMessageNativeBundle {
    pub mailbox: String,
    pub thread_id: String,
    pub message: GmailMessage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GmailRenderedEntity {
    pub document: CanonicalDocument,
    pub shadow: ShadowDocument,
    pub attachment_specs: Vec<GmailAttachmentSpec>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GmailDraftDocument {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
}

pub fn render_gmail_message(bundle: &GmailNativeBundle) -> LocalityResult<GmailRenderedEntity> {
    render_gmail_message_with_entity_id(bundle, RemoteId::new(bundle.message.id.clone()))
}

fn render_gmail_message_with_entity_id(
    bundle: &GmailNativeBundle,
    entity_id: RemoteId,
) -> LocalityResult<GmailRenderedEntity> {
    let attachment_specs = collect_attachment_specs(&bundle.message);
    let body = message_body(&bundle.message)
        .filter(|body| !body.is_empty())
        .unwrap_or_else(|| {
            if has_attachments(&bundle.message) {
                "Attachments are not rendered by Locality Gmail v1.\n".to_string()
            } else {
                String::new()
            }
        });
    let frontmatter =
        message_frontmatter_with_attachment_state(bundle, Some(&attachment_specs), &entity_id);
    let document = CanonicalDocument::new(frontmatter.clone(), body.clone());
    let native_block_ids = synthetic_body_block_ids(&bundle.message.id, &body);
    let shadow = ShadowDocument::from_synced_body(entity_id, body, 1, native_block_ids)
        .map_err(|error| LocalityError::InvalidState(error.to_string()))?
        .with_frontmatter(frontmatter);

    Ok(GmailRenderedEntity {
        document,
        shadow,
        attachment_specs,
    })
}

pub fn thread_remote_id(mailbox: &str, thread_id: &str) -> RemoteId {
    RemoteId::new(format!("gmail-thread:{mailbox}:{thread_id}"))
}

pub fn parse_thread_remote_id(remote_id: &RemoteId) -> Option<(&str, &str)> {
    let rest = remote_id.as_str().strip_prefix("gmail-thread:")?;
    rest.split_once(':')
}

pub fn thread_message_remote_id(mailbox: &str, thread_id: &str, message_id: &str) -> RemoteId {
    RemoteId::new(format!(
        "gmail-thread-message:{mailbox}:{thread_id}:{message_id}"
    ))
}

pub fn parse_thread_message_remote_id(remote_id: &RemoteId) -> Option<(&str, &str, &str)> {
    let rest = remote_id.as_str().strip_prefix("gmail-thread-message:")?;
    let (mailbox, rest) = rest.split_once(':')?;
    let (thread_id, message_id) = rest.split_once(':')?;
    Some((mailbox, thread_id, message_id))
}

pub fn render_gmail_thread_message(
    bundle: &GmailThreadMessageNativeBundle,
) -> LocalityResult<GmailRenderedEntity> {
    render_gmail_message_with_entity_id(
        &GmailNativeBundle {
            mailbox: bundle.mailbox.clone(),
            message: bundle.message.clone(),
        },
        thread_message_remote_id(&bundle.mailbox, &bundle.thread_id, &bundle.message.id),
    )
}

pub fn render_gmail_thread(
    bundle: &GmailThreadNativeBundle,
) -> LocalityResult<GmailRenderedEntity> {
    let remote_id = thread_remote_id(&bundle.mailbox, &bundle.thread.id);
    let subject = bundle
        .thread
        .messages
        .first()
        .map(message_subject_from_headers)
        .unwrap_or_else(|| "(no subject)".to_string());
    let attachment_specs = bundle
        .thread
        .messages
        .iter()
        .flat_map(collect_attachment_specs)
        .collect::<Vec<_>>();
    let version = thread_remote_version(&bundle.thread);
    let body = thread_body(&bundle.thread);
    let frontmatter = thread_frontmatter(
        bundle,
        remote_id.as_str(),
        &subject,
        &version,
        &attachment_specs,
    );
    let document = CanonicalDocument::new(frontmatter.clone(), body.clone());
    let native_block_ids = synthetic_body_block_ids(remote_id.as_str(), &body);
    let shadow = ShadowDocument::from_synced_body(remote_id, body, 1, native_block_ids)
        .map_err(|error| LocalityError::InvalidState(error.to_string()))?
        .with_frontmatter(frontmatter);

    Ok(GmailRenderedEntity {
        document,
        shadow,
        attachment_specs,
    })
}

fn synthetic_body_block_ids(message_id: &str, body: &str) -> Vec<RemoteId> {
    segment_markdown_body(body, 1)
        .into_iter()
        .filter(|block| !block.is_directive())
        .enumerate()
        .map(|(index, _)| RemoteId::new(format!("{message_id}:body:{index}")))
        .collect()
}

fn thread_frontmatter(
    bundle: &GmailThreadNativeBundle,
    remote_id: &str,
    subject: &str,
    version: &str,
    attachment_specs: &[GmailAttachmentSpec],
) -> String {
    let attachments = attachment_frontmatter(attachment_specs);

    format!(
        "loc:\n  id: {}\n  type: page\n  connector: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\ngmail:\n  mailbox: {}\n  thread_id: {}\n  message_count: {}\n{}",
        yaml_scalar(remote_id),
        GMAIL_CONNECTOR_ID,
        yaml_scalar(version),
        yaml_scalar(version),
        yaml_scalar(subject),
        yaml_scalar(&bundle.mailbox),
        yaml_scalar(&bundle.thread.id),
        bundle.thread.messages.len(),
        attachments,
    )
}

pub fn message_frontmatter(bundle: &GmailNativeBundle) -> String {
    message_frontmatter_with_attachment_state(
        bundle,
        None,
        &RemoteId::new(bundle.message.id.clone()),
    )
}

fn message_frontmatter_with_attachment_state(
    bundle: &GmailNativeBundle,
    attachment_specs: Option<&[GmailAttachmentSpec]>,
    entity_id: &RemoteId,
) -> String {
    let message = &bundle.message;
    let version = remote_version(message);
    let headers = message.payload.as_ref().map(header_map).unwrap_or_default();
    let subject = headers
        .get("subject")
        .cloned()
        .unwrap_or_else(|| "(no subject)".to_string());
    let attachments = attachment_specs
        .map(attachment_frontmatter)
        .unwrap_or_default();

    format!(
        "loc:\n  id: {}\n  type: page\n  connector: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\ngmail:\n  mailbox: {}\n  message_id: {}\n  thread_id: {}\n  labels: [{}]\n{}from: {}\nto: [{}]\ncc: [{}]\nbcc: []\nsubject: {}\ndate: {}\n",
        yaml_scalar(entity_id.as_str()),
        GMAIL_CONNECTOR_ID,
        yaml_scalar(&version),
        yaml_scalar(&version),
        yaml_scalar(&subject),
        yaml_scalar(&bundle.mailbox),
        yaml_scalar(&message.id),
        yaml_scalar(message.thread_id.as_deref().unwrap_or("")),
        message
            .label_ids
            .iter()
            .map(|label| yaml_scalar(label))
            .collect::<Vec<_>>()
            .join(", "),
        attachments,
        yaml_scalar(headers.get("from").map(String::as_str).unwrap_or("")),
        yaml_list_items(headers.get("to").map(String::as_str).unwrap_or("")),
        yaml_list_items(headers.get("cc").map(String::as_str).unwrap_or("")),
        yaml_scalar(&subject),
        yaml_scalar(headers.get("date").map(String::as_str).unwrap_or("")),
    )
}

fn attachment_frontmatter(attachment_specs: &[GmailAttachmentSpec]) -> String {
    if attachment_specs.is_empty() {
        return "  attachments: []\n".to_string();
    }

    let mut output = String::from("  attachments:\n");
    for spec in attachment_specs {
        output.push_str(&format!(
            "    - filename: {}\n      attachment_id: {}\n      mime_type: {}\n      size: {}\n      path: {}\n",
            yaml_scalar(&spec.filename),
            yaml_scalar(&spec.attachment_id),
            yaml_scalar(&spec.mime_type),
            spec.size
                .map(|size| size.to_string())
                .unwrap_or_else(|| "null".to_string()),
            yaml_scalar(&spec.local_path.display().to_string())
        ));
    }
    output
}

pub fn remote_version(message: &GmailMessage) -> String {
    let mut labels = message.label_ids.clone();
    labels.sort();

    format!(
        "gmail:{}:{}:{}",
        message.id,
        message.internal_date.as_deref().unwrap_or("unknown"),
        labels.join(",")
    )
}

pub fn thread_remote_version(thread: &GmailThread) -> String {
    let mut message_versions = thread
        .messages
        .iter()
        .map(remote_version)
        .collect::<Vec<_>>();
    message_versions.sort();
    format!(
        "gmail-thread:{}:{}:{}",
        thread.id,
        thread.history_id.as_deref().unwrap_or("unknown"),
        message_versions.join("|")
    )
}

pub fn build_draft_mime(draft: &GmailDraftDocument) -> LocalityResult<String> {
    build_draft_mime_with_message_id(draft, None)
}

pub fn build_draft_mime_with_message_id(
    draft: &GmailDraftDocument,
    message_id: Option<&str>,
) -> LocalityResult<String> {
    if draft.to.iter().all(|value| value.trim().is_empty()) {
        return Err(LocalityError::Validation(vec![ValidationIssue::new(
            "gmail_draft_missing_to",
            std::path::PathBuf::new(),
            Some(1),
            "Gmail draft requires at least one `to` recipient",
            Some("add `to: [\"name@example.com\"]` to the frontmatter".to_string()),
        )]));
    }
    if draft.subject.trim().is_empty() {
        return Err(LocalityError::Validation(vec![ValidationIssue::new(
            "gmail_draft_missing_subject",
            std::path::PathBuf::new(),
            Some(1),
            "Gmail draft requires a non-empty subject",
            Some("add `subject: \"Subject text\"` to the frontmatter".to_string()),
        )]));
    }

    let mut mime = String::new();
    mime.push_str(&format!("To: {}\r\n", sanitize_recipients(&draft.to)));
    if draft.cc.iter().any(|value| !value.trim().is_empty()) {
        mime.push_str(&format!("Cc: {}\r\n", sanitize_recipients(&draft.cc)));
    }
    if draft.bcc.iter().any(|value| !value.trim().is_empty()) {
        mime.push_str(&format!("Bcc: {}\r\n", sanitize_recipients(&draft.bcc)));
    }
    mime.push_str(&format!("Subject: {}\r\n", sanitize_header(&draft.subject)));
    if let Some(message_id) = message_id
        .map(sanitize_message_id)
        .filter(|value| !value.is_empty())
    {
        mime.push_str(&format!("Message-ID: <{message_id}>\r\n"));
    }
    mime.push_str("MIME-Version: 1.0\r\n");
    mime.push_str("Content-Type: text/plain; charset=\"UTF-8\"\r\n");
    mime.push_str("Content-Transfer-Encoding: 8bit\r\n");
    mime.push_str("\r\n");
    mime.push_str(&normalize_mime_body_line_endings(&draft.body));

    Ok(mime)
}

pub fn raw_message_base64url(mime: &str) -> String {
    URL_SAFE_NO_PAD.encode(mime.as_bytes())
}

fn message_body(message: &GmailMessage) -> Option<String> {
    let payload = message.payload.as_ref()?;
    text_part(payload, "text/plain")
        .or_else(|| text_part(payload, "text/html").map(strip_html_tags))
        .map(escape_locality_directive_lines)
        .map(ensure_trailing_newline)
}

fn thread_body(thread: &GmailThread) -> String {
    let mut output = String::new();
    for (index, message) in thread.messages.iter().enumerate() {
        if index > 0 {
            output.push('\n');
        }

        let headers = message.payload.as_ref().map(header_map).unwrap_or_default();
        let from = headers.get("from").map(String::as_str).unwrap_or("");
        let date = headers.get("date").map(String::as_str).unwrap_or("");
        let body = message_body(message).unwrap_or_default();
        output.push_str(&format!(
            "## {from}\n\nDate: {date}\nMessage-ID: {}\n\n{body}",
            message.id
        ));
        if !output.ends_with('\n') {
            output.push('\n');
        }
    }
    output
}

fn message_subject_from_headers(message: &GmailMessage) -> String {
    message
        .payload
        .as_ref()
        .map(header_map)
        .and_then(|headers| headers.get("subject").cloned())
        .filter(|subject| !subject.trim().is_empty())
        .unwrap_or_else(|| "(no subject)".to_string())
}

fn text_part(part: &GmailMessagePart, mime_type: &str) -> Option<String> {
    if part.mime_type.as_deref() == Some(mime_type)
        && let Some(data) = part.body.as_ref().and_then(|body| body.data.as_ref())
    {
        return decode_gmail_base64url(data).and_then(|bytes| String::from_utf8(bytes).ok());
    }

    part.parts
        .iter()
        .find_map(|part| text_part(part, mime_type))
}

fn decode_gmail_base64url(data: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(data.as_bytes())
        .or_else(|_| URL_SAFE.decode(data.as_bytes()))
        .ok()
}

fn has_attachments(message: &GmailMessage) -> bool {
    fn part_has_attachment(part: &GmailMessagePart) -> bool {
        part.body
            .as_ref()
            .and_then(|body| body.attachment_id.as_ref())
            .is_some()
            || part.parts.iter().any(part_has_attachment)
    }

    message.payload.as_ref().is_some_and(part_has_attachment)
}

fn strip_html_tags(input: String) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_tag = false;

    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => output.push(ch),
            _ => {}
        }
    }

    output
}

fn escape_locality_directive_lines(value: String) -> String {
    let mut escaped = String::with_capacity(value.len());
    let mut line_start = 0;

    while line_start < value.len() {
        let Some((line_end, terminator_end)) = next_line_bounds(&value, line_start) else {
            escape_locality_directive_line(&value[line_start..], &mut escaped);
            break;
        };

        escape_locality_directive_line(&value[line_start..line_end], &mut escaped);
        escaped.push_str(&value[line_end..terminator_end]);
        line_start = terminator_end;
    }

    escaped
}

fn next_line_bounds(value: &str, line_start: usize) -> Option<(usize, usize)> {
    for (offset, ch) in value[line_start..].char_indices() {
        let index = line_start + offset;
        match ch {
            '\n' => return Some((index, index + ch.len_utf8())),
            '\r' => {
                let terminator_end = if value[index + ch.len_utf8()..].starts_with('\n') {
                    index + "\r\n".len()
                } else {
                    index + ch.len_utf8()
                };
                return Some((index, terminator_end));
            }
            _ => {}
        }
    }

    None
}

fn escape_locality_directive_line(line: &str, output: &mut String) {
    let Some((index, _)) = line
        .char_indices()
        .find(|(_, ch)| !matches!(ch, ' ' | '\t'))
    else {
        output.push_str(line);
        return;
    };

    if locality_directive_marker_needs_escape(&line[index..]) {
        output.push_str(&line[..index]);
        output.push('\\');
        output.push_str(&line[index..]);
    } else {
        output.push_str(line);
    }
}

fn locality_directive_marker_needs_escape(value: &str) -> bool {
    value.starts_with("::loc") || value.starts_with("::afs")
}

fn ensure_trailing_newline(mut value: String) -> String {
    if !value.ends_with('\n') {
        value.push('\n');
    }
    value
}

fn yaml_list_items(header: &str) -> String {
    split_address_header(header)
        .into_iter()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(yaml_scalar)
        .collect::<Vec<_>>()
        .join(", ")
}

fn split_address_header(header: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut escaped = false;

    for (index, ch) in header.char_indices() {
        if in_quotes {
            if escaped {
                escaped = false;
                continue;
            }

            match ch {
                '\\' => escaped = true,
                '"' => in_quotes = false,
                _ => {}
            }
            continue;
        }

        match ch {
            '"' => in_quotes = true,
            ',' => {
                let part = header[start..index].trim();
                if !part.is_empty() {
                    parts.push(part);
                }
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }

    let part = header[start..].trim();
    if !part.is_empty() {
        parts.push(part);
    }

    parts
}

fn sanitize_recipients(values: &[String]) -> String {
    values
        .iter()
        .map(|value| sanitize_header(value))
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

fn sanitize_header(value: &str) -> String {
    value.replace(['\r', '\n'], " ").trim().to_string()
}

fn sanitize_message_id(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | '@'))
        .collect()
}

fn normalize_mime_body_line_endings(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                normalized.push_str("\r\n");
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
            }
            '\n' => normalized.push_str("\r\n"),
            ch => normalized.push(ch),
        }
    }

    normalized
}

fn yaml_scalar(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch if ch.is_control() => escaped.push_str(&format!("\\u{:04X}", u32::from(ch))),
            ch => escaped.push(ch),
        }
    }

    format!("\"{}\"", escaped)
}

#[cfg(test)]
mod tests {
    use locality_core::LocalityError;

    use super::{
        GmailDraftDocument, GmailNativeBundle, GmailThreadNativeBundle, build_draft_mime,
        message_frontmatter, remote_version, render_gmail_message, render_gmail_thread,
        yaml_scalar,
    };
    use crate::dto::GmailMessage;

    #[test]
    fn renders_plain_text_message_with_gmail_frontmatter() {
        let message: GmailMessage = serde_json::from_value(serde_json::json!({
            "id": "msg-1",
            "threadId": "thread-1",
            "labelIds": ["INBOX"],
            "internalDate": "1720900000000",
            "payload": {
                "mimeType": "text/plain",
                "headers": [
                    { "name": "From", "value": "Ann <ann@example.com>" },
                    { "name": "To", "value": "me@example.com" },
                    { "name": "Subject", "value": "Hello" },
                    { "name": "Date", "value": "Tue, 14 Jul 2026 09:30:00 +0000" }
                ],
                "body": { "data": "SGVsbG8gZnJvbSBHbWFpbC4K" }
            }
        }))
        .expect("message");
        let rendered = render_gmail_message(&GmailNativeBundle {
            mailbox: "inbox".to_string(),
            message,
        })
        .expect("render");

        assert!(rendered.document.frontmatter.contains("connector: gmail"));
        assert!(rendered.document.frontmatter.contains("mailbox: \"inbox\""));
        assert!(rendered.document.frontmatter.contains("attachments: []"));
        assert!(rendered.document.frontmatter.contains("subject: \"Hello\""));
        assert_eq!(rendered.document.body, "Hello from Gmail.\n");
        assert_eq!(rendered.shadow.entity_id.as_str(), "msg-1");
    }

    #[test]
    fn renders_thread_document_with_message_sections() {
        let thread: crate::dto::GmailThread = serde_json::from_value(serde_json::json!({
            "id": "thread-1",
            "historyId": "h1",
            "messages": [
                {
                    "id": "msg-1",
                    "threadId": "thread-1",
                    "labelIds": ["INBOX"],
                    "internalDate": "1720900000000",
                    "payload": {
                        "mimeType": "text/plain",
                        "headers": [
                            { "name": "From", "value": "Ann <ann@example.com>" },
                            { "name": "Subject", "value": "Quarterly update" },
                            { "name": "Date", "value": "Tue, 14 Jul 2026 09:30:00 +0000" }
                        ],
                        "body": { "data": "Rmlyc3QgbWVzc2FnZS4K" }
                    }
                },
                {
                    "id": "msg-2",
                    "threadId": "thread-1",
                    "labelIds": ["SENT"],
                    "internalDate": "1720900500000",
                    "payload": {
                        "mimeType": "text/plain",
                        "headers": [
                            { "name": "From", "value": "Me <me@example.com>" },
                            { "name": "Subject", "value": "Re: Quarterly update" },
                            { "name": "Date", "value": "Tue, 14 Jul 2026 09:38:20 +0000" }
                        ],
                        "body": { "data": "UmVwbHkuCg" }
                    }
                }
            ]
        }))
        .expect("thread");

        let rendered = render_gmail_thread(&GmailThreadNativeBundle {
            mailbox: "inbox".to_string(),
            thread,
        })
        .expect("render thread");

        assert!(rendered.document.frontmatter.contains("type: page"));
        assert!(
            rendered
                .document
                .frontmatter
                .contains("thread_id: \"thread-1\"")
        );
        assert!(rendered.document.frontmatter.contains("message_count: 2"));
        assert!(rendered.document.body.contains("## Ann <ann@example.com>"));
        assert!(rendered.document.body.contains("First message."));
        assert!(rendered.document.body.contains("## Me <me@example.com>"));
        assert!(rendered.document.body.contains("Reply."));
        assert_eq!(
            rendered.shadow.entity_id.as_str(),
            "gmail-thread:inbox:thread-1"
        );
    }

    #[test]
    fn renders_padded_gmail_body_data() {
        let message: GmailMessage = serde_json::from_value(serde_json::json!({
            "id": "msg-padded",
            "threadId": "thread-padded",
            "labelIds": ["INBOX"],
            "internalDate": "1720900000000",
            "payload": {
                "mimeType": "text/plain",
                "headers": [
                    { "name": "Subject", "value": "Padded" }
                ],
                "body": { "data": "SGVsbG8gZnJvbSBwYWRkZWQuCg==" }
            }
        }))
        .expect("message");

        let rendered = render_gmail_message(&GmailNativeBundle {
            mailbox: "inbox".to_string(),
            message,
        })
        .expect("render");

        assert_eq!(rendered.document.body, "Hello from padded.\n");
    }

    #[test]
    fn message_frontmatter_omits_attachments_when_metadata_cannot_know_them() {
        let message: GmailMessage = serde_json::from_value(serde_json::json!({
            "id": "msg-metadata",
            "threadId": "thread-metadata",
            "labelIds": ["INBOX"],
            "internalDate": "1720900000000",
            "payload": {
                "mimeType": "text/plain",
                "headers": [
                    { "name": "Subject", "value": "Metadata only" }
                ]
            }
        }))
        .expect("message");

        let frontmatter = message_frontmatter(&GmailNativeBundle {
            mailbox: "inbox".to_string(),
            message,
        });

        assert!(!frontmatter.contains("attachments:"));
    }

    #[test]
    fn renders_attachment_metadata_without_downloading_bytes() {
        let message: GmailMessage = serde_json::from_value(serde_json::json!({
            "id": "msg-attach",
            "threadId": "thread-attach",
            "labelIds": ["INBOX"],
            "internalDate": "1720900000000",
            "payload": {
                "mimeType": "multipart/mixed",
                "headers": [
                    { "name": "Subject", "value": "Attachments" }
                ],
                "parts": [
                    {
                        "mimeType": "text/plain",
                        "body": { "data": "Qm9keQo" }
                    },
                    {
                        "partId": "2",
                        "filename": "Invoice.pdf",
                        "mimeType": "application/pdf",
                        "body": { "attachmentId": "attach-1", "size": 12 }
                    }
                ]
            }
        }))
        .expect("message");

        let rendered = render_gmail_message(&GmailNativeBundle {
            mailbox: "inbox".to_string(),
            message,
        })
        .expect("render");

        let expected_path =
            crate::attachments::attachment_local_path("msg-attach", "part-id:2", "Invoice.pdf");

        assert_eq!(rendered.document.body, "Body\n");
        assert_eq!(rendered.attachment_specs.len(), 1);
        assert!(rendered.document.frontmatter.contains("attachments:"));
        assert!(
            rendered
                .document
                .frontmatter
                .contains("filename: \"Invoice.pdf\"")
        );
        assert!(
            rendered
                .document
                .frontmatter
                .contains("attachment_id: \"attach-1\"")
        );
        assert!(rendered.document.frontmatter.contains(&format!(
            "path: {}",
            yaml_scalar(&expected_path.display().to_string())
        )));
    }

    #[test]
    fn renders_multi_paragraph_body_with_matching_shadow_blocks() {
        let message: GmailMessage = serde_json::from_value(serde_json::json!({
            "id": "msg-multi-paragraph",
            "threadId": "thread-multi-paragraph",
            "labelIds": ["INBOX"],
            "internalDate": "1720900000000",
            "payload": {
                "mimeType": "text/plain",
                "headers": [
                    { "name": "Subject", "value": "Multi paragraph" }
                ],
                "body": { "data": "Rmlyc3QgcGFyYWdyYXBoLgoKU2Vjb25kIHBhcmFncmFwaC4K" }
            }
        }))
        .expect("message");

        let rendered = render_gmail_message(&GmailNativeBundle {
            mailbox: "inbox".to_string(),
            message,
        })
        .expect("render");

        assert_eq!(
            rendered.document.body,
            "First paragraph.\n\nSecond paragraph.\n"
        );
        assert_eq!(rendered.shadow.blocks.len(), 2);
    }

    #[test]
    fn escapes_control_chars_in_frontmatter_scalars() {
        let message: GmailMessage = serde_json::from_value(serde_json::json!({
            "id": "msg-control",
            "threadId": "thread-control",
            "labelIds": ["INBOX"],
            "internalDate": "1720900000000",
            "payload": {
                "mimeType": "text/plain",
                "headers": [
                    { "name": "Subject", "value": "Hello\n---\r\t\u{0001}" }
                ],
                "body": { "data": "Qm9keQo" }
            }
        }))
        .expect("message");

        let rendered = render_gmail_message(&GmailNativeBundle {
            mailbox: "inbox".to_string(),
            message,
        })
        .expect("render");

        assert!(
            rendered
                .document
                .frontmatter
                .contains("subject: \"Hello\\n---\\r\\t\\u0001\"")
        );
        assert!(
            !rendered
                .document
                .frontmatter
                .contains("subject: \"Hello\n---")
        );
    }

    #[test]
    fn keeps_quoted_display_name_commas_in_address_lists() {
        let message: GmailMessage = serde_json::from_value(serde_json::json!({
            "id": "msg-addresses",
            "threadId": "thread-addresses",
            "labelIds": ["INBOX"],
            "internalDate": "1720900000000",
            "payload": {
                "mimeType": "text/plain",
                "headers": [
                    { "name": "To", "value": "\"Doe, Jane\" <jane@example.com>, bob@example.com" },
                    { "name": "Subject", "value": "Addresses" }
                ],
                "body": { "data": "Qm9keQo" }
            }
        }))
        .expect("message");

        let rendered = render_gmail_message(&GmailNativeBundle {
            mailbox: "inbox".to_string(),
            message,
        })
        .expect("render");

        assert!(
            rendered
                .document
                .frontmatter
                .contains("to: [\"\\\"Doe, Jane\\\" <jane@example.com>\", \"bob@example.com\"]")
        );
    }

    #[test]
    fn remote_version_changes_when_labels_change() {
        let mut inbox_message = GmailMessage {
            id: "msg-labels".to_string(),
            internal_date: Some("1720900000000".to_string()),
            label_ids: vec!["INBOX".to_string()],
            ..GmailMessage::default()
        };
        let archived_message = GmailMessage {
            label_ids: vec!["ARCHIVE".to_string()],
            ..inbox_message.clone()
        };

        assert_ne!(
            remote_version(&inbox_message),
            remote_version(&archived_message)
        );

        inbox_message.label_ids = vec!["ARCHIVE".to_string()];
        assert_eq!(
            remote_version(&inbox_message),
            remote_version(&archived_message)
        );
    }

    #[test]
    fn builds_rfc822_mime_from_local_draft() {
        let draft = GmailDraftDocument {
            to: vec!["ann@example.com".to_string()],
            cc: vec!["copy@example.com".to_string()],
            bcc: Vec::new(),
            subject: "Hello".to_string(),
            body: "Thanks.\n".to_string(),
        };

        let mime = build_draft_mime(&draft).expect("mime");

        assert!(mime.contains("To: ann@example.com\r\n"));
        assert!(mime.contains("Cc: copy@example.com\r\n"));
        assert!(mime.contains("Subject: Hello\r\n"));
        assert!(mime.contains("Content-Type: text/plain; charset=\"UTF-8\"\r\n"));
        assert!(mime.ends_with("\r\n\r\nThanks.\r\n"));
        assert!(!mime.contains("Bcc:"));
    }

    #[test]
    fn normalizes_draft_mime_body_line_endings_to_crlf() {
        let draft = GmailDraftDocument {
            to: vec!["ann@example.com".to_string()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Hello".to_string(),
            body: "Line 1\nLine 2\n".to_string(),
        };

        let mime = build_draft_mime(&draft).expect("mime");

        assert!(mime.ends_with("\r\n\r\nLine 1\r\nLine 2\r\n"));
    }

    #[test]
    fn render_escapes_literal_locality_directives_in_remote_body() {
        let message: GmailMessage = serde_json::from_value(serde_json::json!({
            "id": "msg-directives",
            "threadId": "thread-directives",
            "labelIds": ["INBOX"],
            "internalDate": "1720900000000",
            "payload": {
                "mimeType": "text/plain",
                "headers": [
                    { "name": "Subject", "value": "Directive text" }
                ],
                "body": { "data": "Ojpsb2N7aWQ9eCB0eXBlPXBhcmFncmFwaH0KCjo6YWZze2lkPXkgdHlwZT1wYXJhZ3JhcGh9Cg" }
            }
        }))
        .expect("message");

        let rendered = render_gmail_message(&GmailNativeBundle {
            mailbox: "inbox".to_string(),
            message,
        })
        .expect("render");

        assert_eq!(
            rendered.document.body,
            "\\::loc{id=x type=paragraph}\n\n\\::afs{id=y type=paragraph}\n"
        );
        assert_eq!(rendered.shadow.blocks.len(), 2);
    }

    #[test]
    fn draft_requires_recipient_and_subject() {
        let draft = GmailDraftDocument {
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: String::new(),
            body: "Body".to_string(),
        };

        let error = build_draft_mime(&draft).expect_err("invalid draft");
        let LocalityError::Validation(issues) = error else {
            panic!("expected validation error");
        };
        assert!(
            issues[0]
                .message
                .contains("Gmail draft requires at least one `to` recipient")
        );
    }
}
