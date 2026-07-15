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
        .map(safe_component)
        .filter(|value| !value.is_empty())
        .map(|value| format!(".{value}"))
        .unwrap_or_default();
    PathBuf::from(".loc")
        .join("gmail")
        .join("attachments")
        .join(opaque_id_component(message_id))
        .join(format!(
            "{}-{}{}",
            safe_stem(filename),
            opaque_id_component(attachment_id),
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

fn opaque_id_component(value: &str) -> String {
    let readable = safe_component(value);
    let encoded = if value.is_empty() {
        "empty".to_string()
    } else {
        hex_encode(value.as_bytes())
    };
    if readable.is_empty() {
        format!("id-{encoded}")
    } else {
        format!("{readable}-{encoded}")
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

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
            specs[0]
                .local_path
                .parent()
                .and_then(|parent| parent.parent()),
            Some(std::path::Path::new(".loc/gmail/attachments"))
        );
        assert_safe_identity_fragment(&message_identity_fragment(&specs[0].local_path));
        assert_safe_identity_fragment(&attachment_identity_fragment(
            &specs[0].local_path,
            "invoice-july-",
            ".pdf",
        ));
    }

    #[test]
    fn attachment_local_path_keeps_distinct_attachment_ids() {
        let path = attachment_local_path("msg-1", "a-1", "report.final.pdf");

        assert_safe_identity_fragment(&attachment_identity_fragment(
            &path,
            "report-final-",
            ".pdf",
        ));
    }

    #[test]
    fn attachment_local_path_keeps_colliding_opaque_ids_distinct() {
        let raw_ids = ["A_B", "a-b", "a/b", "💥"];
        let attachment_paths = raw_ids
            .map(|attachment_id| attachment_local_path("msg-1", attachment_id, "report.final.pdf"));
        let attachment_fragments: Vec<String> = attachment_paths
            .iter()
            .map(|path| attachment_identity_fragment(path, "report-final-", ".pdf"))
            .collect();

        assert_eq!(
            attachment_fragments.iter().collect::<BTreeSet<_>>().len(),
            attachment_fragments.len(),
            "{attachment_fragments:?}"
        );
        for fragment in &attachment_fragments {
            assert_safe_identity_fragment(fragment);
        }

        let message_paths =
            raw_ids.map(|message_id| attachment_local_path(message_id, "attach-1", "report.pdf"));
        let message_fragments: Vec<String> = message_paths
            .iter()
            .map(|path| message_identity_fragment(path))
            .collect();

        assert_eq!(
            message_fragments.iter().collect::<BTreeSet<_>>().len(),
            message_fragments.len(),
            "{message_fragments:?}"
        );
        for fragment in &message_fragments {
            assert_safe_identity_fragment(fragment);
        }
    }

    #[test]
    fn attachment_local_path_omits_empty_sanitized_extension() {
        let path = attachment_local_path("msg-1", "attach-1", "file.💥");
        let filename = file_name(&path);

        assert!(!filename.ends_with('.'), "{filename}");
    }

    #[test]
    fn collects_only_real_attachments_and_defaults_missing_mime_type() {
        let message: GmailMessage = serde_json::from_value(serde_json::json!({
            "id": "msg-1",
            "payload": {
                "mimeType": "multipart/mixed",
                "parts": [
                    {
                        "filename": "   ",
                        "mimeType": "application/pdf",
                        "body": { "attachmentId": "blank-filename" }
                    },
                    {
                        "filename": "missing-id.txt",
                        "mimeType": "text/plain",
                        "body": { "size": 9 }
                    },
                    {
                        "filename": "fallback.bin",
                        "body": { "attachmentId": "attach-1", "size": 7 }
                    }
                ]
            }
        }))
        .expect("message");

        let specs = collect_attachment_specs(&message);

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].filename, "fallback.bin");
        assert_eq!(specs[0].mime_type, "application/octet-stream");
        assert_eq!(specs[0].size, Some(7));
    }

    #[test]
    fn decode_attachment_body_reports_missing_data_and_invalid_base64() {
        let missing = crate::dto::GmailMessagePartBody::default();
        let missing_error = decode_attachment_body(&missing).expect_err("missing data");
        assert!(
            missing_error
                .to_string()
                .contains("gmail attachment response did not include body data"),
            "{missing_error}"
        );

        let invalid = crate::dto::GmailMessagePartBody {
            data: Some("not base64***".to_string()),
            ..crate::dto::GmailMessagePartBody::default()
        };
        let invalid_error = decode_attachment_body(&invalid).expect_err("invalid base64");
        assert!(
            invalid_error
                .to_string()
                .contains("gmail attachment decode failed"),
            "{invalid_error}"
        );
    }

    #[test]
    fn attachment_local_path_sanitizes_traversal_and_windows_hostile_filenames() {
        let path = attachment_local_path("msg-1", "attach-1", "..\\CON: report?.tar.gz");
        let filename = file_name(&path);

        assert!(!filename.contains(".."), "{filename}");
        assert!(!filename.contains('\\'), "{filename}");
        assert!(!filename.contains('/'), "{filename}");
        assert!(!filename.contains(':'), "{filename}");
        assert!(!filename.contains('?'), "{filename}");
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

    fn attachment_identity_fragment(path: &std::path::Path, prefix: &str, suffix: &str) -> String {
        file_name(path)
            .strip_prefix(prefix)
            .and_then(|value| value.strip_suffix(suffix))
            .expect("attachment identity fragment")
            .to_string()
    }

    fn message_identity_fragment(path: &std::path::Path) -> String {
        path.parent()
            .and_then(|parent| parent.file_name())
            .and_then(|value| value.to_str())
            .expect("message identity fragment")
            .to_string()
    }

    fn file_name(path: &std::path::Path) -> String {
        path.file_name()
            .and_then(|value| value.to_str())
            .expect("file name")
            .to_string()
    }

    fn assert_safe_identity_fragment(fragment: &str) {
        assert!(!fragment.is_empty(), "{fragment:?}");
        assert_ne!(fragment, ".", "{fragment}");
        assert_ne!(fragment, "..", "{fragment}");
        assert!(!fragment.contains('/'), "{fragment}");
        assert!(!fragment.contains('\\'), "{fragment}");
    }
}
