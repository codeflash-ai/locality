use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};

use crate::dto::{GmailMessage, GmailMessagePart, GmailMessagePartBody};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GmailAttachmentSpec {
    pub message_id: String,
    pub attachment_id: String,
    pub filename: String,
    pub mime_type: String,
    pub size: Option<u64>,
    pub local_path: PathBuf,
}

pub fn collect_attachment_specs(message: &GmailMessage) -> Vec<GmailAttachmentSpec> {
    let mut specs = Vec::new();
    if let Some(payload) = &message.payload {
        collect_part_specs(&message.id, payload, &mut specs);
    }
    specs
}

fn collect_part_specs(
    message_id: &str,
    part: &GmailMessagePart,
    specs: &mut Vec<GmailAttachmentSpec>,
) {
    if let Some(filename) = part
        .filename
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        if let Some(body) = &part.body {
            if let Some(attachment_id) = body.attachment_id.as_deref() {
                specs.push(GmailAttachmentSpec {
                    message_id: message_id.to_string(),
                    attachment_id: attachment_id.to_string(),
                    filename: filename.to_string(),
                    mime_type: part
                        .mime_type
                        .clone()
                        .unwrap_or_else(|| "application/octet-stream".to_string()),
                    size: body.size,
                    local_path: attachment_local_path(message_id, attachment_id, filename),
                });
            }
        }
    }
    for child in &part.parts {
        collect_part_specs(message_id, child, specs);
    }
}

pub fn attachment_local_path(message_id: &str, attachment_id: &str, filename: &str) -> PathBuf {
    let extension = std::path::Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{}", safe_component(value)))
        .unwrap_or_default();
    PathBuf::from(".loc")
        .join("gmail")
        .join("attachments")
        .join(safe_component(message_id))
        .join(format!(
            "{}-{}{}",
            safe_stem(filename),
            safe_component(attachment_id),
            extension
        ))
}

pub fn decode_attachment_body(body: &GmailMessagePartBody) -> LocalityResult<Vec<u8>> {
    let data = body.data.as_deref().ok_or_else(|| {
        LocalityError::Io("gmail attachment response did not include body data".to_string())
    })?;
    URL_SAFE_NO_PAD
        .decode(data.as_bytes())
        .or_else(|_| URL_SAFE.decode(data.as_bytes()))
        .map_err(|error| LocalityError::Io(format!("gmail attachment decode failed: {error}")))
}

fn safe_stem(filename: &str) -> String {
    let stem = std::path::Path::new(filename)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("attachment");
    let safe = safe_component(stem);
    if safe.is_empty() {
        "attachment".to_string()
    } else {
        safe
    }
}

fn safe_component(value: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::{attachment_local_path, collect_attachment_specs, decode_attachment_body};
    use crate::dto::GmailMessage;

    #[test]
    fn collects_nested_attachment_specs_with_safe_local_paths() {
        let message: GmailMessage = serde_json::from_value(serde_json::json!({
            "id": "msg/1",
            "payload": {
                "mimeType": "multipart/mixed",
                "parts": [
                    {
                        "mimeType": "text/plain",
                        "body": { "data": "Qm9keQo" }
                    },
                    {
                        "filename": "Invoice July.pdf",
                        "mimeType": "application/pdf",
                        "body": { "attachmentId": "attach/1", "size": 12345 }
                    }
                ]
            }
        }))
        .expect("message");

        let specs = collect_attachment_specs(&message);

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].message_id, "msg/1");
        assert_eq!(specs[0].attachment_id, "attach/1");
        assert_eq!(specs[0].filename, "Invoice July.pdf");
        assert_eq!(specs[0].mime_type, "application/pdf");
        assert_eq!(specs[0].size, Some(12345));
        assert_eq!(
            specs[0].local_path,
            std::path::PathBuf::from(".loc/gmail/attachments/msg-1/invoice-july-attach-1.pdf")
        );
    }

    #[test]
    fn attachment_local_path_keeps_distinct_attachment_ids() {
        assert_eq!(
            attachment_local_path("msg-1", "a-1", "report.final.pdf"),
            std::path::PathBuf::from(".loc/gmail/attachments/msg-1/report-final-a-1.pdf")
        );
    }

    #[test]
    fn decodes_padded_and_unpadded_gmail_attachment_data() {
        let padded = crate::dto::GmailMessagePartBody {
            data: Some("SGVsbG8=".to_string()),
            ..crate::dto::GmailMessagePartBody::default()
        };
        let unpadded = crate::dto::GmailMessagePartBody {
            data: Some("SGVsbG8".to_string()),
            ..crate::dto::GmailMessagePartBody::default()
        };

        assert_eq!(decode_attachment_body(&padded).expect("padded"), b"Hello");
        assert_eq!(
            decode_attachment_body(&unpadded).expect("unpadded"),
            b"Hello"
        );
    }
}
