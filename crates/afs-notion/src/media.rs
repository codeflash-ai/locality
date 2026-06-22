//! Local media materialization for Notion file-like blocks.
//!
//! Notion file URLs are useful for API round-trips, but agents work better when
//! images are also present as normal local files. Media assets are projected
//! under a mount-level `.afs/media/` directory that mirrors the page path without
//! putting binary files next to Markdown documents or colliding with projected
//! Notion page/database names.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use afs_core::path_projection::page_container_path;
use afs_core::{AfsError, AfsResult};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::client::notion_http_client;

const AFS_DIR: &str = ".afs";
const MEDIA_DIR: &str = "media";
const MEDIA_MANIFEST: &str = "manifest.json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaAsset {
    pub block_id: String,
    pub kind: String,
    pub source_url: String,
    pub local_path: PathBuf,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MediaDownloadReport {
    pub downloaded: usize,
    pub failed: usize,
    pub skipped: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadedMediaAsset {
    pub block_id: String,
    pub kind: String,
    pub source_url: String,
    pub local_path: PathBuf,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MediaFetchReport {
    pub downloaded: Vec<DownloadedMediaAsset>,
    pub failed: Vec<MediaDownloadFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaDownloadFailure {
    pub block_id: String,
    pub kind: String,
    pub source_url: String,
    pub local_path: PathBuf,
    pub error: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaManifest {
    pub version: u32,
    #[serde(default)]
    pub assets: BTreeMap<String, MediaManifestEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaManifestEntry {
    pub block_id: String,
    pub kind: String,
    pub source_url: String,
    pub local_path: PathBuf,
    pub sha256: String,
    pub size: u64,
}

pub fn media_local_path(page_path: &Path, block_id: &str, kind: &str, source_url: &str) -> PathBuf {
    media_page_dir(page_path).join(format!(
        "{}-{}{}",
        sanitize_path_component(kind),
        short_block_id(block_id),
        media_extension(source_url, kind)
    ))
}

pub fn download_media_assets(
    mount_root: &Path,
    assets: &[MediaAsset],
) -> AfsResult<MediaDownloadReport> {
    for asset in assets.iter().filter(|asset| should_download(asset)) {
        validate_mount_relative_path(&asset.local_path)?;
    }

    let fetched = fetch_media_asset_report(assets);
    let mut report = MediaDownloadReport::default();

    for asset in &fetched.downloaded {
        let destination = mount_root.join(&asset.local_path);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_atomic(&destination, &asset.bytes)?;
        report.downloaded += 1;
    }
    update_media_manifest(mount_root, &fetched.downloaded)?;

    report.failed = fetched.failed.len();
    report.skipped = assets.len().saturating_sub(report.downloaded);
    Ok(report)
}

pub fn fetch_media_assets(assets: &[MediaAsset]) -> AfsResult<Vec<DownloadedMediaAsset>> {
    Ok(fetch_media_asset_report(assets).downloaded)
}

pub fn fetch_media_asset_report(assets: &[MediaAsset]) -> MediaFetchReport {
    let client = notion_http_client();
    let mut report = MediaFetchReport::default();

    for asset in assets.iter().filter(|asset| should_download(asset)) {
        match fetch_media_asset(&client, asset) {
            Ok(downloaded) => report.downloaded.push(downloaded),
            Err(error) => report.failed.push(MediaDownloadFailure {
                block_id: asset.block_id.clone(),
                kind: asset.kind.clone(),
                source_url: asset.source_url.clone(),
                local_path: asset.local_path.clone(),
                error: error.to_string(),
            }),
        }
    }

    report
}

fn fetch_media_asset(client: &Client, asset: &MediaAsset) -> AfsResult<DownloadedMediaAsset> {
    let response = client
        .get(&asset.source_url)
        .send()
        .map_err(|error| AfsError::Io(format!("media download failed: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(AfsError::Io(format!(
            "media download returned HTTP {status} for block `{}`",
            asset.block_id
        )));
    }
    let bytes = response
        .bytes()
        .map_err(|error| AfsError::Io(format!("media download body failed: {error}")))?
        .to_vec();

    Ok(DownloadedMediaAsset {
        block_id: asset.block_id.clone(),
        kind: asset.kind.clone(),
        source_url: asset.source_url.clone(),
        local_path: asset.local_path.clone(),
        bytes,
    })
}

fn should_download(asset: &MediaAsset) -> bool {
    asset.kind == "image" && is_downloadable_url(&asset.source_url)
}

pub(crate) fn is_downloadable_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn media_page_dir(page_path: &Path) -> PathBuf {
    let mut path = media_root_path();
    let mut pushed_component = false;
    let page_path = page_container_path(page_path);

    for component in page_path.components() {
        if let Component::Normal(component) = component {
            path.push(sanitize_page_component(&component.to_string_lossy()));
            pushed_component = true;
        }
    }

    if !pushed_component {
        path.push("page");
    }

    path
}

pub fn local_media_href(page_path: &Path, local_path: &Path) -> String {
    let base = markdown_parent_dir(page_path);
    let base_components = normal_components(&base);
    let target_components = normal_components(local_path);
    let common = base_components
        .iter()
        .zip(&target_components)
        .take_while(|(left, right)| left == right)
        .count();
    let mut parts = Vec::new();
    for _ in common..base_components.len() {
        parts.push("..".to_string());
    }
    parts.extend(target_components[common..].iter().cloned());
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

pub fn resolve_media_href(page_path: &Path, href: &str) -> Option<PathBuf> {
    resolve_media_href_inner(page_path, href, None)
}

pub fn resolve_media_href_with_content_root(
    page_path: &Path,
    href: &str,
    content_root: &Path,
) -> Option<PathBuf> {
    resolve_media_href_inner(page_path, href, Some(content_root))
}

fn resolve_media_href_inner(
    page_path: &Path,
    href: &str,
    content_root: Option<&Path>,
) -> Option<PathBuf> {
    if is_external_href(href) {
        return None;
    }

    let decoded = percent_decode(href)?;
    let decoded_path = Path::new(&decoded);
    if decoded_path.is_absolute() {
        let content_root = content_root?;
        let relative = decoded_path.strip_prefix(content_root).ok()?;
        let normalized = normalize_relative_path(relative)?;
        return is_media_path(&normalized).then_some(normalized);
    }

    let mut combined = markdown_parent_dir(page_path);
    combined.push(decoded);
    let normalized = normalize_relative_path(&combined)?;
    if is_media_path(&normalized) {
        Some(normalized)
    } else {
        None
    }
}

fn markdown_parent_dir(page_path: &Path) -> PathBuf {
    page_path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .to_path_buf()
}

pub fn load_media_manifest(mount_root: &Path) -> AfsResult<MediaManifest> {
    let path = media_manifest_path(mount_root);
    if !path.exists() {
        return Ok(MediaManifest {
            version: 1,
            assets: BTreeMap::new(),
        });
    }
    let contents = std::fs::read_to_string(&path)?;
    serde_json::from_str(&contents)
        .map_err(|error| AfsError::Io(format!("media manifest decode failed: {error}")))
}

pub fn update_media_manifest(mount_root: &Path, assets: &[DownloadedMediaAsset]) -> AfsResult<()> {
    if assets.is_empty() {
        return Ok(());
    }
    let mut manifest = load_media_manifest(mount_root)?;
    manifest.version = 1;
    for asset in assets {
        let entry = MediaManifestEntry {
            block_id: asset.block_id.clone(),
            kind: asset.kind.clone(),
            source_url: asset.source_url.clone(),
            local_path: asset.local_path.clone(),
            sha256: sha256_hex(&asset.bytes),
            size: asset.bytes.len() as u64,
        };
        manifest
            .assets
            .insert(media_manifest_key(&asset.local_path), entry);
    }
    let path = media_manifest_path(mount_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| AfsError::Io(format!("media manifest encode failed: {error}")))?;
    write_atomic(&path, &json)
}

pub fn media_manifest_entry<'a>(
    manifest: &'a MediaManifest,
    local_path: &Path,
) -> Option<&'a MediaManifestEntry> {
    manifest.assets.get(&media_manifest_key(local_path))
}

pub fn media_manifest_key(path: &Path) -> String {
    normal_components(path).join("/")
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub fn media_manifest_path(mount_root: &Path) -> PathBuf {
    mount_root.join(media_root_path()).join(MEDIA_MANIFEST)
}

fn media_root_path() -> PathBuf {
    PathBuf::from(AFS_DIR).join(MEDIA_DIR)
}

fn is_media_path(path: &Path) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(Component::Normal(value)) if value == AFS_DIR)
        && matches!(components.next(), Some(Component::Normal(value)) if value == MEDIA_DIR)
}

fn is_external_href(href: &str) -> bool {
    let lower = href.to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("afs://")
        || lower.starts_with("notion://")
}

fn normal_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().to_string()),
            _ => None,
        })
        .collect()
}

fn normalize_relative_path(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::Prefix(_) | Component::RootDir => return None,
        }
    }
    Some(normalized)
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return None;
            }
            let high = hex_value(bytes[index + 1])?;
            let low = hex_value(bytes[index + 2])?;
            out.push(high << 4 | low);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn validate_mount_relative_path(path: &Path) -> AfsResult<()> {
    if path.components().any(|component| {
        matches!(
            component,
            Component::Prefix(_) | Component::RootDir | Component::ParentDir
        )
    }) {
        return Err(AfsError::InvalidState(format!(
            "media asset path `{}` is not mount-relative",
            path.display()
        )));
    }

    Ok(())
}

fn media_extension(source_url: &str, kind: &str) -> String {
    let without_query = source_url
        .split(['?', '#'])
        .next()
        .unwrap_or(source_url)
        .trim_end_matches('/');
    let extension = Path::new(without_query)
        .extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| extension.len() <= 8)
        .filter(|extension| extension.chars().all(|ch| ch.is_ascii_alphanumeric()));

    match extension {
        Some(extension) => format!(".{}", extension.to_ascii_lowercase()),
        None if kind == "image" => ".img".to_string(),
        None => ".bin".to_string(),
    }
}

fn short_block_id(block_id: &str) -> String {
    let hex = block_id
        .chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .collect::<String>();
    if hex.is_empty() {
        sanitize_path_component(block_id)
    } else {
        hex
    }
}

fn sanitize_path_component(value: &str) -> String {
    let mut out = String::new();
    let mut previous_dash = false;

    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            previous_dash = false;
        } else if !previous_dash && !out.is_empty() {
            out.push('-');
            previous_dash = true;
        }
    }

    while out.ends_with('-') {
        out.pop();
    }

    if out.is_empty() {
        "asset".to_string()
    } else {
        out
    }
}

fn sanitize_page_component(value: &str) -> String {
    let mut out = String::new();
    let mut previous_dash = false;

    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, ' ' | '.' | '_' | '-' | '~') {
            out.push(ch);
            previous_dash = false;
        } else if !previous_dash && !out.is_empty() {
            out.push('-');
            previous_dash = true;
        }
    }

    while out.ends_with('-') {
        out.pop();
    }

    if out.is_empty() {
        "page".to_string()
    } else {
        out
    }
}

fn write_atomic(path: &Path, contents: &[u8]) -> AfsResult<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("afs-media");
    let temp_path = path.with_file_name(format!(".{file_name}.afs-tmp"));
    std::fs::write(&temp_path, contents)?;
    std::fs::rename(&temp_path, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{local_media_href, media_local_path, resolve_media_href};
    use std::path::Path;

    #[test]
    fn media_paths_mirror_page_paths_under_media_directory() {
        assert_eq!(
            media_local_path(
                Path::new("Tasks/Fix login/page.md"),
                "01234567-89ab-cdef",
                "image",
                "https://example.com/diagram.PNG?download=1",
            ),
            Path::new(".afs/media/Tasks/Fix login/image-0123456789abcdef.png")
        );
    }

    #[test]
    fn media_paths_ignore_parent_and_root_components() {
        assert_eq!(
            media_local_path(
                Path::new("/../Tasks/../Fix login/page.md"),
                "01234567-89ab-cdef",
                "image",
                "https://example.com/diagram.PNG?download=1",
            ),
            Path::new(".afs/media/Tasks/Fix login/image-0123456789abcdef.png")
        );
    }

    #[test]
    fn media_paths_use_full_compact_block_ids_to_avoid_same_page_collisions() {
        let first = media_local_path(
            Path::new("Docs/Whitepaper/page.md"),
            "2f03ac0e-bb88-80ef-9328-c370dd88d9ba",
            "image",
            "https://example.com/first.png",
        );
        let second = media_local_path(
            Path::new("Docs/Whitepaper/page.md"),
            "2f03ac0e-bb88-80c1-8ed8-d4bc56b2e490",
            "image",
            "https://example.com/second.png",
        );

        assert_ne!(first, second);
        assert_eq!(
            first,
            Path::new(".afs/media/Docs/Whitepaper/image-2f03ac0ebb8880ef9328c370dd88d9ba.png")
        );
        assert_eq!(
            second,
            Path::new(".afs/media/Docs/Whitepaper/image-2f03ac0ebb8880c18ed8d4bc56b2e490.png")
        );
    }

    #[test]
    fn local_media_href_is_relative_to_markdown_file_parent() {
        assert_eq!(
            local_media_href(
                Path::new("Tasks/Fix login/page.md"),
                Path::new(".afs/media/Tasks/Fix login/image-0123456789abcdef.png"),
            ),
            "../../.afs/media/Tasks/Fix login/image-0123456789abcdef.png"
        );
        assert_eq!(
            local_media_href(
                Path::new("Roadmap.md"),
                Path::new(".afs/media/Roadmap/image-0123456789abcdef.png"),
            ),
            ".afs/media/Roadmap/image-0123456789abcdef.png"
        );
    }

    #[test]
    fn resolves_local_media_href_to_mount_relative_path() {
        assert_eq!(
            resolve_media_href(
                Path::new("Tasks/Fix login/page.md"),
                "../../.afs/media/Tasks/Fix login/image-0123456789ab.png",
            ),
            Some(Path::new(".afs/media/Tasks/Fix login/image-0123456789ab.png").to_path_buf())
        );
    }
}
