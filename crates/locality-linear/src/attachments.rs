use std::path::{Path, PathBuf};

use locality_core::{LocalityError, LocalityResult};

use crate::dto::{LinearAttachmentDownload, LinearIssueContext};

pub const DEFAULT_LINEAR_ATTACHMENT_DOWNLOAD_BYTES: u64 = 25 * 1024 * 1024;
const MAX_ATTACHMENT_FILENAME_LEN: usize = 240;
const MAX_OPAQUE_COMPONENT_LEN: usize = 96;
const MAX_EXTENSION_LEN: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinearAttachmentAsset {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
}

pub fn enrich_linear_attachment_downloads<F>(
    context: &mut LinearIssueContext,
    mut download: F,
    max_bytes: u64,
) -> Vec<LinearAttachmentAsset>
where
    F: FnMut(&str, u64) -> LocalityResult<Vec<u8>>,
{
    let mut assets = Vec::new();
    for attachment in &mut context.attachments {
        if !is_http_url(&attachment.url) {
            attachment.download = Some(LinearAttachmentDownload {
                status: "skipped".to_string(),
                local_path: None,
                error: Some("only HTTP(S) attachment URLs can be downloaded".to_string()),
            });
            continue;
        }
        let local_path = attachment_local_path(
            &context.issue_id,
            &attachment.id,
            &attachment.title,
            &attachment.url,
        );
        match download(&attachment.url, max_bytes) {
            Ok(bytes) => {
                attachment.download = Some(LinearAttachmentDownload {
                    status: "downloaded".to_string(),
                    local_path: Some(local_path.to_string_lossy().replace('\\', "/")),
                    error: None,
                });
                assets.push(LinearAttachmentAsset {
                    path: local_path,
                    bytes,
                });
            }
            Err(error) => {
                attachment.download = Some(LinearAttachmentDownload {
                    status: if matches!(
                        error,
                        LocalityError::Guardrail(_) | LocalityError::Unsupported(_)
                    ) {
                        "skipped".to_string()
                    } else {
                        "failed".to_string()
                    },
                    local_path: None,
                    error: Some(error.to_string()),
                });
            }
        }
    }
    assets
}

pub fn attachment_local_path(
    issue_id: &str,
    attachment_id: &str,
    title: &str,
    url: &str,
) -> PathBuf {
    let filename = filename_from_url(url).unwrap_or(title);
    let extension = Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .map(safe_component)
        .map(|value| truncate_component(&value, MAX_EXTENSION_LEN.saturating_sub(1)))
        .filter(|value| !value.is_empty())
        .map(|value| format!(".{value}"))
        .unwrap_or_default();
    let attachment_component = bounded_opaque_id_component(attachment_id);
    let stem_budget = MAX_ATTACHMENT_FILENAME_LEN
        .saturating_sub(1)
        .saturating_sub(attachment_component.len())
        .saturating_sub(extension.len())
        .max("attachment".len());
    let stem = truncate_component(&safe_stem(filename), stem_budget);
    PathBuf::from(".loc")
        .join("linear")
        .join("attachments")
        .join(bounded_opaque_id_component(issue_id))
        .join(format!("{stem}-{attachment_component}{extension}"))
}

fn is_http_url(url: &str) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

fn filename_from_url(url: &str) -> Option<&str> {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let filename = without_query.trim_end_matches('/').rsplit('/').next()?;
    (!filename.trim().is_empty()).then_some(filename)
}

fn safe_stem(filename: &str) -> String {
    let stem = Path::new(filename)
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

fn bounded_opaque_id_component(value: &str) -> String {
    let component = opaque_id_component(value);
    if component.len() <= MAX_OPAQUE_COMPONENT_LEN {
        return component;
    }

    let hash = stable_hex_hash(value.as_bytes());
    let prefix_budget = MAX_OPAQUE_COMPONENT_LEN
        .saturating_sub(hash.len())
        .saturating_sub(1);
    let prefix = truncate_component(&safe_component(value), prefix_budget);
    if prefix.is_empty() {
        format!("id-{hash}")
    } else {
        format!("{prefix}-{hash}")
    }
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

fn truncate_component(value: &str, max_len: usize) -> String {
    if value.len() <= max_len {
        return value.to_string();
    }
    let truncated = value
        .chars()
        .take(max_len)
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if truncated.is_empty() {
        "attachment".to_string()
    } else {
        truncated
    }
}

fn stable_hex_hash(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
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
    use super::attachment_local_path;

    #[test]
    fn attachment_local_path_uses_issue_cache_and_stable_attachment_id() {
        let path = attachment_local_path(
            "issue-1",
            "attach-1",
            "Design Spec",
            "https://uploads.linear.app/spec.final.pdf?download=1",
        );

        assert_eq!(
            path.to_string_lossy(),
            ".loc/linear/attachments/issue-1-69737375652d31/spec-final-attach-1-6174746163682d31.pdf"
        );
    }
}
