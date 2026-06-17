//! Local media materialization for Notion file-like blocks.
//!
//! Notion file URLs are useful for API round-trips, but agents work better when
//! images are also present as normal local files. Media assets are projected
//! under a mount-level `media/` directory that mirrors the page path without
//! putting binary files next to Markdown documents.

use std::path::{Component, Path, PathBuf};

use afs_core::path_projection::page_container_path;
use afs_core::{AfsError, AfsResult};
use reqwest::blocking::Client;

const MEDIA_DIR: &str = "media";

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
    pub skipped: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadedMediaAsset {
    pub local_path: PathBuf,
    pub bytes: Vec<u8>,
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

    let downloaded = fetch_media_assets(assets)?;
    let mut report = MediaDownloadReport::default();

    for asset in &downloaded {
        let destination = mount_root.join(&asset.local_path);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_atomic(&destination, &asset.bytes)?;
        report.downloaded += 1;
    }

    report.skipped = assets.len().saturating_sub(report.downloaded);
    Ok(report)
}

pub fn fetch_media_assets(assets: &[MediaAsset]) -> AfsResult<Vec<DownloadedMediaAsset>> {
    let client = Client::new();
    assets
        .iter()
        .filter(|asset| should_download(asset))
        .map(|asset| fetch_media_asset(&client, asset))
        .collect()
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
        local_path: asset.local_path.clone(),
        bytes,
    })
}

fn should_download(asset: &MediaAsset) -> bool {
    asset.kind == "image"
}

fn media_page_dir(page_path: &Path) -> PathBuf {
    let mut path = PathBuf::from(MEDIA_DIR);
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
        .take(12)
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
    use super::media_local_path;
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
            Path::new("media/Tasks/Fix login/image-0123456789ab.png")
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
            Path::new("media/Tasks/Fix login/image-0123456789ab.png")
        );
    }
}
