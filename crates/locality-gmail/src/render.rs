use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use locality_core::model::{CanonicalDocument, RemoteId};
use locality_core::shadow::ShadowDocument;
use locality_core::validation::ValidationIssue;
use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};

use crate::dto::{GmailMessage, GmailMessagePart, header_map};
use crate::oauth::GMAIL_CONNECTOR_ID;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GmailNativeBundle {
    pub mailbox: String,
    pub message: GmailMessage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GmailRenderedEntity {
    pub document: CanonicalDocument,
    pub shadow: ShadowDocument,
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
    let body = message_body(&bundle.message)
        .filter(|body| !body.is_empty())
        .unwrap_or_else(|| {
            if has_attachments(&bundle.message) {
                "Attachments are not rendered by Locality Gmail v1.\n".to_string()
            } else {
                String::new()
            }
        });
    let frontmatter = message_frontmatter(bundle);
    let document = CanonicalDocument::new(frontmatter.clone(), body.clone());
    let native_block_ids = if body.trim().is_empty() {
        Vec::new()
    } else {
        vec![RemoteId::new(format!("{}:body", bundle.message.id))]
    };
    let shadow = ShadowDocument::from_synced_body(
        RemoteId::new(bundle.message.id.clone()),
        body,
        1,
        native_block_ids,
    )
    .map_err(|error| LocalityError::InvalidState(error.to_string()))?
    .with_frontmatter(frontmatter);

    Ok(GmailRenderedEntity { document, shadow })
}

pub fn message_frontmatter(bundle: &GmailNativeBundle) -> String {
    let message = &bundle.message;
    let version = remote_version(message);
    let headers = message.payload.as_ref().map(header_map).unwrap_or_default();
    let subject = headers
        .get("subject")
        .cloned()
        .unwrap_or_else(|| "(no subject)".to_string());

    format!(
        "loc:\n  id: {}\n  type: page\n  connector: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\ngmail:\n  mailbox: {}\n  message_id: {}\n  thread_id: {}\n  labels: [{}]\nfrom: {}\nto: [{}]\ncc: [{}]\nbcc: []\nsubject: {}\ndate: {}\n",
        yaml_scalar(&message.id),
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
        yaml_scalar(headers.get("from").map(String::as_str).unwrap_or("")),
        yaml_list_items(headers.get("to").map(String::as_str).unwrap_or("")),
        yaml_list_items(headers.get("cc").map(String::as_str).unwrap_or("")),
        yaml_scalar(&subject),
        yaml_scalar(headers.get("date").map(String::as_str).unwrap_or("")),
    )
}

pub fn remote_version(message: &GmailMessage) -> String {
    format!(
        "gmail:{}:{}",
        message.id,
        message.internal_date.as_deref().unwrap_or("unknown")
    )
}

pub fn build_draft_mime(draft: &GmailDraftDocument) -> LocalityResult<String> {
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
    mime.push_str("MIME-Version: 1.0\r\n");
    mime.push_str("Content-Type: text/plain; charset=\"UTF-8\"\r\n");
    mime.push_str("Content-Transfer-Encoding: 8bit\r\n");
    mime.push_str("\r\n");
    mime.push_str(&draft.body);

    Ok(mime)
}

pub fn raw_message_base64url(mime: &str) -> String {
    URL_SAFE_NO_PAD.encode(mime.as_bytes())
}

fn message_body(message: &GmailMessage) -> Option<String> {
    let payload = message.payload.as_ref()?;
    text_part(payload, "text/plain")
        .or_else(|| text_part(payload, "text/html").map(strip_html_tags))
        .map(ensure_trailing_newline)
}

fn text_part(part: &GmailMessagePart, mime_type: &str) -> Option<String> {
    if part.mime_type.as_deref() == Some(mime_type)
        && let Some(data) = part.body.as_ref().and_then(|body| body.data.as_ref())
    {
        return URL_SAFE_NO_PAD
            .decode(data.as_bytes())
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok());
    }

    part.parts
        .iter()
        .find_map(|part| text_part(part, mime_type))
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

fn ensure_trailing_newline(mut value: String) -> String {
    if !value.ends_with('\n') {
        value.push('\n');
    }
    value
}

fn yaml_list_items(header: &str) -> String {
    header
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(yaml_scalar)
        .collect::<Vec<_>>()
        .join(", ")
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

fn yaml_scalar(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use locality_core::LocalityError;

    use super::{GmailDraftDocument, GmailNativeBundle, build_draft_mime, render_gmail_message};
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
        assert!(rendered.document.frontmatter.contains("subject: \"Hello\""));
        assert_eq!(rendered.document.body, "Hello from Gmail.\n");
        assert_eq!(rendered.shadow.entity_id.as_str(), "msg-1");
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
        assert!(mime.ends_with("\r\n\r\nThanks.\n"));
        assert!(!mime.contains("Bcc:"));
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
