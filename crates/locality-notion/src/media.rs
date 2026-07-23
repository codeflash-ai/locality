//! Local media materialization for Notion file-like blocks.
//!
//! Notion file URLs are useful for API round-trips, but agents work better when
//! file-like media are also present as normal local files. Media assets are projected
//! under a mount-level `.loc/media/` directory that mirrors the page path without
//! putting binary files next to Markdown documents or colliding with projected
//! Notion page/database names.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use locality_core::path_projection::page_container_path;
use locality_core::{LocalityError, LocalityResult};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, LOCATION};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::client::notion_http_client;

const LOCALITY_DIR: &str = ".loc";
const MEDIA_DIR: &str = "media";
const MEDIA_MANIFEST: &str = "manifest.json";
const MEDIA_FETCH_ATTEMPTS: usize = 3;
const MEDIA_FETCH_RETRY_DELAY: Duration = Duration::from_millis(50);

pub const PORTABLE_MEDIA_MAX_ASSETS: usize = 128;
pub const PORTABLE_MEDIA_MAX_ASSET_BYTES: usize = 20 * 1024 * 1024;
pub const PORTABLE_MEDIA_MAX_AGGREGATE_BYTES: usize = 100 * 1024 * 1024;
const PORTABLE_MEDIA_READ_BUFFER_BYTES: usize = 64 * 1024;
const PORTABLE_EXTERNAL_MEDIA_MAX_URL_BYTES: usize = 8 * 1024;
const PORTABLE_MEDIA_MAX_REDIRECTS: usize = 3;
const PORTABLE_MEDIA_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const PORTABLE_MEDIA_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Opt-in hosted-media behavior for portable Notion fetches.
///
/// The default preserves the existing direct connector behavior. `HostedPilot`
/// captures only Notion-hosted file payloads under the fixed pilot limits above.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PortableMediaCapturePolicy {
    #[default]
    Disabled,
    HostedPilot,
}

impl PortableMediaCapturePolicy {
    pub(crate) fn captures_hosted_media(self) -> bool {
        self == Self::HostedPilot
    }
}

/// A bounded hosted-media response returned to portable capture.
///
/// Custom implementations are intended for deterministic tests. The connector
/// validates the initial provider URL before invoking the fetcher and enforces
/// the same byte limits on every returned body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortableMediaCapture {
    pub bytes: Vec<u8>,
    pub media_type: String,
}

/// Redaction-safe result of attempting to capture one Notion-hosted asset.
///
/// The variants intentionally carry no provider URL or transport message. This
/// type crosses the public portable boundary, where signed URLs and response
/// details must never be serialized into completeness metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostedMediaCaptureOutcome {
    Captured(PortableMediaCapture),
    Unavailable,
    TooLarge,
    Unsafe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostedMediaFailureKind {
    Unavailable,
    TooLarge,
    Unsafe,
}

pub trait PortableMediaCaptureFetcher: Send + Sync {
    fn fetch(&self, hosted_url: &str, max_bytes: usize) -> LocalityResult<PortableMediaCapture>;

    /// Typed capture API used by portable and desktop projection paths.
    ///
    /// Existing fetcher implementations remain source-compatible: their
    /// errors conservatively become genuine unavailability, while an
    /// oversized successful body is still classified as a policy omission.
    fn fetch_outcome(&self, hosted_url: &str, max_bytes: usize) -> HostedMediaCaptureOutcome {
        match self.fetch(hosted_url, max_bytes) {
            Ok(capture) if capture.bytes.len() > max_bytes => HostedMediaCaptureOutcome::TooLarge,
            Ok(capture) => HostedMediaCaptureOutcome::Captured(capture),
            Err(_) => HostedMediaCaptureOutcome::Unavailable,
        }
    }
}

#[derive(Default)]
struct SecurePortableMediaCaptureFetcher {
    transport: Mutex<Option<ReqwestPortableMediaTransport>>,
}

impl PortableMediaCaptureFetcher for SecurePortableMediaCaptureFetcher {
    fn fetch(&self, hosted_url: &str, max_bytes: usize) -> LocalityResult<PortableMediaCapture> {
        match self.fetch_outcome(hosted_url, max_bytes) {
            HostedMediaCaptureOutcome::Captured(capture) => Ok(capture),
            HostedMediaCaptureOutcome::Unavailable => {
                Err(LocalityError::Io("hosted media is unavailable".to_string()))
            }
            HostedMediaCaptureOutcome::TooLarge => Err(LocalityError::InvalidState(
                "hosted media exceeds the asset limit".to_string(),
            )),
            HostedMediaCaptureOutcome::Unsafe => Err(LocalityError::InvalidState(
                "hosted media failed safety validation".to_string(),
            )),
        }
    }

    fn fetch_outcome(&self, hosted_url: &str, max_bytes: usize) -> HostedMediaCaptureOutcome {
        let Ok(mut transport) = self.transport.lock() else {
            return HostedMediaCaptureOutcome::Unavailable;
        };
        if transport.is_none() {
            let Ok(client) = ReqwestPortableMediaTransport::new() else {
                return HostedMediaCaptureOutcome::Unavailable;
            };
            *transport = Some(client);
        }
        fetch_hosted_media_outcome_with_transport(
            transport
                .as_ref()
                .expect("portable media HTTP client was initialized above"),
            hosted_url,
            max_bytes,
        )
    }
}

pub(crate) fn default_portable_media_fetcher() -> Arc<dyn PortableMediaCaptureFetcher> {
    Arc::new(SecurePortableMediaCaptureFetcher::default())
}

struct PortableMediaHttpResponse {
    status: StatusCode,
    location: Option<String>,
    content_encoding: Option<String>,
    content_length: Option<u64>,
    content_type: Option<String>,
    body: Box<dyn Read + Send>,
}

trait PortableMediaHttpTransport {
    fn get(&self, url: &str, timeout: Duration) -> LocalityResult<PortableMediaHttpResponse>;
}

struct ReqwestPortableMediaTransport {
    client: Client,
}

impl ReqwestPortableMediaTransport {
    fn new() -> LocalityResult<Self> {
        // Install the crate's selected rustls provider before building a
        // separately hardened client.
        drop(notion_http_client());
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(PORTABLE_MEDIA_CONNECT_TIMEOUT)
            .timeout(PORTABLE_MEDIA_REQUEST_TIMEOUT)
            .build()
            .map_err(|_| {
                LocalityError::Io("portable media HTTP client initialization failed".to_string())
            })?;
        Ok(Self { client })
    }
}

impl PortableMediaHttpTransport for ReqwestPortableMediaTransport {
    fn get(&self, url: &str, timeout: Duration) -> LocalityResult<PortableMediaHttpResponse> {
        let response = self
            .client
            .get(url)
            .header(ACCEPT_ENCODING, "identity")
            .timeout(timeout)
            .send()
            .map_err(|_| LocalityError::Io("portable media request failed".to_string()))?;
        let headers = response.headers();
        let location = headers
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let content_encoding = headers
            .get(CONTENT_ENCODING)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let content_length = headers
            .get(CONTENT_LENGTH)
            .map(|value| {
                value
                    .to_str()
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .ok_or_else(|| {
                        LocalityError::InvalidState(
                            "portable media content length is invalid".to_string(),
                        )
                    })
            })
            .transpose()?;
        let content_type = headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let status = response.status();
        Ok(PortableMediaHttpResponse {
            status,
            location,
            content_encoding,
            content_length,
            content_type,
            body: Box::new(response),
        })
    }
}

#[cfg(test)]
fn fetch_portable_media_with_transport(
    transport: &dyn PortableMediaHttpTransport,
    hosted_url: &str,
    max_bytes: usize,
) -> LocalityResult<PortableMediaCapture> {
    match fetch_hosted_media_outcome_with_transport(transport, hosted_url, max_bytes) {
        HostedMediaCaptureOutcome::Captured(capture) => Ok(capture),
        HostedMediaCaptureOutcome::Unavailable => Err(LocalityError::Io(
            "portable media is unavailable".to_string(),
        )),
        HostedMediaCaptureOutcome::TooLarge => Err(LocalityError::InvalidState(
            "portable media exceeds the asset limit".to_string(),
        )),
        HostedMediaCaptureOutcome::Unsafe => Err(LocalityError::InvalidState(
            "portable media failed safety validation".to_string(),
        )),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostedMediaTransferFailure {
    RetryableUnavailable,
    Unavailable,
    TooLarge,
    Unsafe,
}

fn fetch_hosted_media_outcome_with_transport(
    transport: &dyn PortableMediaHttpTransport,
    hosted_url: &str,
    max_bytes: usize,
) -> HostedMediaCaptureOutcome {
    fetch_hosted_media_outcome_with_policy(
        transport,
        hosted_url,
        max_bytes,
        PORTABLE_MEDIA_REQUEST_TIMEOUT,
        MEDIA_FETCH_RETRY_DELAY,
    )
}

fn fetch_hosted_media_outcome_with_policy(
    transport: &dyn PortableMediaHttpTransport,
    hosted_url: &str,
    max_bytes: usize,
    deadline: Duration,
    retry_delay: Duration,
) -> HostedMediaCaptureOutcome {
    let clock = SystemHostedMediaRetryClock {
        started: Instant::now(),
    };
    fetch_hosted_media_outcome_with_clock(
        transport,
        hosted_url,
        max_bytes,
        deadline,
        retry_delay,
        &clock,
    )
}

trait HostedMediaRetryClock {
    fn elapsed(&self) -> Duration;
    fn sleep(&self, duration: Duration);
}

struct SystemHostedMediaRetryClock {
    started: Instant,
}

impl HostedMediaRetryClock for SystemHostedMediaRetryClock {
    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    fn sleep(&self, duration: Duration) {
        thread::sleep(duration);
    }
}

fn fetch_hosted_media_outcome_with_clock(
    transport: &dyn PortableMediaHttpTransport,
    hosted_url: &str,
    max_bytes: usize,
    deadline: Duration,
    retry_delay: Duration,
    clock: &dyn HostedMediaRetryClock,
) -> HostedMediaCaptureOutcome {
    let initial = match validate_portable_hosted_media_url(hosted_url) {
        Ok(url) => url,
        Err(_) => return HostedMediaCaptureOutcome::Unsafe,
    };
    for attempt in 1..=MEDIA_FETCH_ATTEMPTS {
        match fetch_hosted_media_once(transport, &initial, max_bytes, deadline, clock) {
            Ok(capture) => return HostedMediaCaptureOutcome::Captured(capture),
            Err(HostedMediaTransferFailure::RetryableUnavailable)
                if attempt < MEDIA_FETCH_ATTEMPTS && clock.elapsed() < deadline =>
            {
                let remaining = deadline.saturating_sub(clock.elapsed());
                clock.sleep(retry_delay.min(remaining));
            }
            Err(HostedMediaTransferFailure::RetryableUnavailable)
            | Err(HostedMediaTransferFailure::Unavailable) => {
                return HostedMediaCaptureOutcome::Unavailable;
            }
            Err(HostedMediaTransferFailure::TooLarge) => {
                return HostedMediaCaptureOutcome::TooLarge;
            }
            Err(HostedMediaTransferFailure::Unsafe) => {
                return HostedMediaCaptureOutcome::Unsafe;
            }
        }
    }
    HostedMediaCaptureOutcome::Unavailable
}

fn fetch_hosted_media_once(
    transport: &dyn PortableMediaHttpTransport,
    initial: &reqwest::Url,
    max_bytes: usize,
    deadline: Duration,
    clock: &dyn HostedMediaRetryClock,
) -> Result<PortableMediaCapture, HostedMediaTransferFailure> {
    let mut current = initial.clone();
    let mut visited = std::collections::BTreeSet::new();

    for redirects in 0..=PORTABLE_MEDIA_MAX_REDIRECTS {
        if !visited.insert(current.as_str().to_string()) {
            return Err(HostedMediaTransferFailure::Unsafe);
        }
        let timeout = deadline
            .checked_sub(clock.elapsed())
            .ok_or(HostedMediaTransferFailure::RetryableUnavailable)?;
        if timeout.is_zero() {
            return Err(HostedMediaTransferFailure::RetryableUnavailable);
        }
        let mut response = transport
            .get(current.as_str(), timeout)
            .map_err(|_| HostedMediaTransferFailure::RetryableUnavailable)?;

        if response.status.is_redirection() {
            if redirects == PORTABLE_MEDIA_MAX_REDIRECTS {
                return Err(HostedMediaTransferFailure::Unsafe);
            }
            let location = response
                .location
                .as_deref()
                .ok_or(HostedMediaTransferFailure::Unsafe)?;
            let destination = current
                .join(location)
                .map_err(|_| HostedMediaTransferFailure::Unsafe)?;
            current = validate_portable_hosted_media_url(destination.as_str())
                .map_err(|_| HostedMediaTransferFailure::Unsafe)?;
            continue;
        }
        if !response.status.is_success() {
            return Err(
                if response.status == StatusCode::REQUEST_TIMEOUT
                    || response.status == StatusCode::TOO_MANY_REQUESTS
                    || response.status.is_server_error()
                {
                    HostedMediaTransferFailure::RetryableUnavailable
                } else {
                    HostedMediaTransferFailure::Unavailable
                },
            );
        }
        if let Some(encoding) = response.content_encoding.as_deref()
            && !encoding.eq_ignore_ascii_case("identity")
        {
            return Err(HostedMediaTransferFailure::Unsafe);
        }
        if response
            .content_length
            .is_some_and(|length| length > max_bytes as u64)
        {
            return Err(HostedMediaTransferFailure::TooLarge);
        }

        let mut bytes = Vec::with_capacity(
            response
                .content_length
                .and_then(|length| usize::try_from(length).ok())
                .unwrap_or(0)
                .min(max_bytes),
        );
        let mut buffer = [0_u8; PORTABLE_MEDIA_READ_BUFFER_BYTES];
        loop {
            let read = response
                .body
                .read(&mut buffer)
                .map_err(|_| HostedMediaTransferFailure::RetryableUnavailable)?;
            if read == 0 {
                break;
            }
            if bytes.len().saturating_add(read) > max_bytes {
                return Err(HostedMediaTransferFailure::TooLarge);
            }
            bytes.extend_from_slice(&buffer[..read]);
        }
        if response
            .content_length
            .is_some_and(|length| length != bytes.len() as u64)
        {
            return Err(HostedMediaTransferFailure::RetryableUnavailable);
        }
        return Ok(PortableMediaCapture {
            bytes,
            media_type: sanitize_portable_media_type(response.content_type.as_deref()),
        });
    }

    Err(HostedMediaTransferFailure::Unsafe)
}

pub(crate) fn validate_portable_hosted_media_url(url: &str) -> LocalityResult<reqwest::Url> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| LocalityError::InvalidState("portable media URL is invalid".to_string()))?;
    if parsed.scheme() != "https"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some_and(|port| port != 443)
    {
        return Err(LocalityError::InvalidState(
            "portable media URL violates the HTTPS origin policy".to_string(),
        ));
    }
    let host = parsed.host_str().ok_or_else(|| {
        LocalityError::InvalidState("portable media URL has no allowed host".to_string())
    })?;
    let allowed = matches!(
        host,
        "secure.notion-static.com" | "prod-files-secure.s3.us-west-2.amazonaws.com"
    ) || (host == "s3.us-west-2.amazonaws.com"
        && parsed.path().starts_with("/secure.notion-static.com/"));
    if !allowed {
        return Err(LocalityError::InvalidState(
            "portable media URL host is not allowed".to_string(),
        ));
    }
    Ok(parsed)
}

pub(crate) fn sanitize_portable_hosted_media_url(url: &str) -> LocalityResult<String> {
    let mut parsed = validate_portable_hosted_media_url(url)?;
    parsed.set_query(None);
    parsed.set_fragment(None);
    Ok(parsed.to_string())
}

/// Validates a public external media reference without rewriting it.
///
/// Portable rendering never requests these URLs. Keeping validation separate
/// from hosted-media capture makes that no-fetch boundary explicit and lets the
/// renderer preserve the provider's exact safe spelling.
pub(crate) fn validate_portable_external_media_url(url: &str) -> LocalityResult<()> {
    if url.is_empty()
        || url.len() > PORTABLE_EXTERNAL_MEDIA_MAX_URL_BYTES
        || url
            .chars()
            .any(|character| character.is_ascii_control() || character.is_whitespace())
    {
        return Err(LocalityError::InvalidState(
            "portable external media URL is not raw-safe".to_string(),
        ));
    }
    let parsed = reqwest::Url::parse(url).map_err(|_| {
        LocalityError::InvalidState("portable external media URL is invalid".to_string())
    })?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(LocalityError::InvalidState(
            "portable external media URL violates the HTTPS reference policy".to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn portable_media_expired(expiry_time: &str) -> LocalityResult<bool> {
    let expiry = parse_rfc3339_utc_seconds(expiry_time).ok_or_else(|| {
        LocalityError::InvalidState("portable media expiry is invalid".to_string())
    })?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| LocalityError::InvalidState("system clock is before Unix time".to_string()))?
        .as_secs();
    Ok(expiry <= now)
}

fn parse_rfc3339_utc_seconds(value: &str) -> Option<u64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || *bytes.last()? != b'Z'
    {
        return None;
    }
    if bytes.len() > 20 {
        let fraction = &bytes[20..bytes.len() - 1];
        if bytes[19] != b'.' || fraction.is_empty() || !fraction.iter().all(u8::is_ascii_digit) {
            return None;
        }
    }
    let number = |start: usize, end: usize| -> Option<i64> {
        std::str::from_utf8(&bytes[start..end]).ok()?.parse().ok()
    };
    let year = number(0, 4)?;
    let month = number(5, 7)?;
    let day = number(8, 10)?;
    let hour = number(11, 13)?;
    let minute = number(14, 16)?;
    let second = number(17, 19)?;
    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&second)
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    if days < 0 {
        return None;
    }
    u64::try_from(days * 86_400 + hour * 3_600 + minute * 60 + second).ok()
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

pub(crate) fn sanitize_portable_media_type(value: Option<&str>) -> String {
    let essence = value
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|value| {
            let mut parts = value.split('/');
            let valid_part = |part: &str| {
                !part.is_empty()
                    && part.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-')
                    })
            };
            matches!((parts.next(), parts.next(), parts.next()), (Some(kind), Some(subtype), None) if valid_part(kind) && valid_part(subtype))
        });
    essence
        .unwrap_or("application/octet-stream")
        .to_ascii_lowercase()
}

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
    /// Redaction-safe failure code retained as a string for API compatibility.
    pub error: String,
}

impl MediaDownloadFailure {
    pub fn outcome(&self) -> HostedMediaFailureKind {
        match self.error.as_str() {
            "hosted_media_too_large" => HostedMediaFailureKind::TooLarge,
            "unsafe_hosted_media" => HostedMediaFailureKind::Unsafe,
            _ => HostedMediaFailureKind::Unavailable,
        }
    }
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
) -> LocalityResult<MediaDownloadReport> {
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

pub fn fetch_media_assets(assets: &[MediaAsset]) -> LocalityResult<Vec<DownloadedMediaAsset>> {
    Ok(fetch_media_asset_report(assets).downloaded)
}

pub fn fetch_media_asset_report(assets: &[MediaAsset]) -> MediaFetchReport {
    let fetcher = default_portable_media_fetcher();
    fetch_media_asset_report_with_fetcher(assets, fetcher.as_ref())
}

pub fn fetch_media_asset_report_with_fetcher(
    assets: &[MediaAsset],
    fetcher: &dyn PortableMediaCaptureFetcher,
) -> MediaFetchReport {
    let mut report = MediaFetchReport::default();

    for asset in assets.iter().filter(|asset| should_download(asset)) {
        let outcome = if validate_portable_hosted_media_url(&asset.source_url).is_ok() {
            fetcher.fetch_outcome(&asset.source_url, PORTABLE_MEDIA_MAX_ASSET_BYTES)
        } else {
            // The renderer emits MediaAsset only for structurally hosted
            // sources. Validate again at this trust boundary so custom
            // fetchers never receive an unsafe provider URL.
            HostedMediaCaptureOutcome::Unsafe
        };
        match outcome {
            HostedMediaCaptureOutcome::Captured(capture) => {
                report.downloaded.push(DownloadedMediaAsset {
                    block_id: asset.block_id.clone(),
                    kind: asset.kind.clone(),
                    source_url: asset.source_url.clone(),
                    local_path: asset.local_path.clone(),
                    bytes: capture.bytes,
                });
            }
            outcome => report.failed.push(MediaDownloadFailure {
                block_id: asset.block_id.clone(),
                kind: asset.kind.clone(),
                source_url: asset.source_url.clone(),
                local_path: asset.local_path.clone(),
                error: match outcome {
                    HostedMediaCaptureOutcome::TooLarge => "hosted_media_too_large",
                    HostedMediaCaptureOutcome::Unsafe => "unsafe_hosted_media",
                    HostedMediaCaptureOutcome::Unavailable
                    | HostedMediaCaptureOutcome::Captured(_) => "unavailable_hosted_media",
                }
                .to_string(),
            }),
        }
    }

    report
}

fn should_download(asset: &MediaAsset) -> bool {
    is_file_like_media_kind(&asset.kind)
}

fn is_file_like_media_kind(kind: &str) -> bool {
    matches!(kind, "image" | "video" | "file" | "pdf" | "audio")
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

    let unescaped = unescape_markdown_href(href);
    let decoded = percent_decode(&unescaped)?;
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

pub(crate) fn unescape_markdown_href(href: &str) -> String {
    let mut unescaped = String::with_capacity(href.len());
    let mut chars = href.chars();

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some(next) if is_escaped_href_punctuation(next) => unescaped.push(next),
                Some(next) => {
                    unescaped.push('\\');
                    unescaped.push(next);
                }
                None => unescaped.push('\\'),
            }
        } else {
            unescaped.push(ch);
        }
    }

    unescaped
}

fn is_escaped_href_punctuation(ch: char) -> bool {
    matches!(ch, '(' | ')' | '[' | ']')
}

fn markdown_parent_dir(page_path: &Path) -> PathBuf {
    page_path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .to_path_buf()
}

pub fn load_media_manifest(mount_root: &Path) -> LocalityResult<MediaManifest> {
    let path = media_manifest_path(mount_root);
    if !path.exists() {
        return Ok(MediaManifest {
            version: 1,
            assets: BTreeMap::new(),
        });
    }
    let contents = std::fs::read_to_string(&path)?;
    serde_json::from_str(&contents)
        .map_err(|error| LocalityError::Io(format!("media manifest decode failed: {error}")))
}

pub fn update_media_manifest(
    mount_root: &Path,
    assets: &[DownloadedMediaAsset],
) -> LocalityResult<()> {
    if assets.is_empty() {
        return Ok(());
    }
    let mut manifest = load_media_manifest(mount_root)?;
    manifest.version = 1;
    for asset in assets {
        manifest.assets.insert(
            media_manifest_key(&asset.local_path),
            media_manifest_entry_for_asset(asset),
        );
    }
    write_media_manifest(mount_root, &manifest)
}

pub fn replace_media_manifest(
    mount_root: &Path,
    assets: &[DownloadedMediaAsset],
) -> LocalityResult<()> {
    let existing = load_media_manifest(mount_root)?;
    if assets.is_empty() && existing.assets.is_empty() {
        return Ok(());
    }
    let mut manifest = MediaManifest {
        version: 1,
        assets: BTreeMap::new(),
    };
    for asset in assets {
        manifest.assets.insert(
            media_manifest_key(&asset.local_path),
            media_manifest_entry_for_asset(asset),
        );
    }

    for (key, entry) in existing.assets {
        if manifest.assets.contains_key(&key) {
            continue;
        }
        if !is_media_path(&entry.local_path) {
            continue;
        }
        validate_mount_relative_path(&entry.local_path)?;
        let absolute_path = mount_root.join(&entry.local_path);
        match std::fs::remove_file(&absolute_path) {
            Ok(()) => prune_empty_media_dirs(mount_root, absolute_path.parent()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }

    write_media_manifest(mount_root, &manifest)
}

fn media_manifest_entry_for_asset(asset: &DownloadedMediaAsset) -> MediaManifestEntry {
    MediaManifestEntry {
        block_id: asset.block_id.clone(),
        kind: asset.kind.clone(),
        source_url: asset.source_url.clone(),
        local_path: asset.local_path.clone(),
        sha256: sha256_hex(&asset.bytes),
        size: asset.bytes.len() as u64,
    }
}

fn write_media_manifest(mount_root: &Path, manifest: &MediaManifest) -> LocalityResult<()> {
    let path = media_manifest_path(mount_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| LocalityError::Io(format!("media manifest encode failed: {error}")))?;
    write_atomic(&path, &json)
}

fn prune_empty_media_dirs(mount_root: &Path, start: Option<&Path>) {
    let media_root = mount_root.join(media_root_path());
    let Some(mut current) = start.map(Path::to_path_buf) else {
        return;
    };

    while current.starts_with(&media_root) && current != media_root {
        match std::fs::remove_dir(&current) {
            Ok(()) => {
                if !current.pop() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
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
    PathBuf::from(LOCALITY_DIR).join(MEDIA_DIR)
}

fn is_media_path(path: &Path) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(Component::Normal(value)) if value == LOCALITY_DIR)
        && matches!(components.next(), Some(Component::Normal(value)) if value == MEDIA_DIR)
}

fn is_external_href(href: &str) -> bool {
    let lower = href.to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("loc://")
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

fn validate_mount_relative_path(path: &Path) -> LocalityResult<()> {
    if path.components().any(|component| {
        matches!(
            component,
            Component::Prefix(_) | Component::RootDir | Component::ParentDir
        )
    }) {
        return Err(LocalityError::InvalidState(format!(
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

fn write_atomic(path: &Path, contents: &[u8]) -> LocalityResult<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("loc-media");
    let temp_path = path.with_file_name(format!(".{file_name}.loc-tmp"));
    std::fs::write(&temp_path, contents)?;
    std::fs::rename(&temp_path, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::resolve_media_href_with_content_root;
    use super::{
        HostedMediaCaptureOutcome, HostedMediaRetryClock, MediaAsset,
        PORTABLE_MEDIA_READ_BUFFER_BYTES, PortableMediaCapture, PortableMediaCaptureFetcher,
        PortableMediaHttpResponse, PortableMediaHttpTransport,
        fetch_hosted_media_outcome_with_clock, fetch_hosted_media_outcome_with_policy,
        fetch_hosted_media_outcome_with_transport, fetch_portable_media_with_transport,
        local_media_href, media_local_path, portable_media_expired, replace_media_manifest,
        resolve_media_href, sanitize_portable_hosted_media_url, validate_portable_hosted_media_url,
    };
    use reqwest::StatusCode;
    use std::collections::VecDeque;
    use std::io::Read;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    #[test]
    fn replacing_absent_manifest_with_no_assets_does_not_create_mount_root() {
        let root = std::env::temp_dir().join(format!(
            "locality-empty-media-manifest-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);

        replace_media_manifest(&root, &[]).expect("replace empty manifest");

        assert!(!root.exists());
    }

    #[test]
    fn media_paths_mirror_page_paths_under_media_directory() {
        assert_eq!(
            media_local_path(
                Path::new("Tasks/Fix login/page.md"),
                "01234567-89ab-cdef",
                "image",
                "https://example.com/diagram.PNG?download=1",
            ),
            Path::new(".loc/media/Tasks/Fix login/image-0123456789abcdef.png")
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
            Path::new(".loc/media/Tasks/Fix login/image-0123456789abcdef.png")
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
            Path::new(".loc/media/Docs/Whitepaper/image-2f03ac0ebb8880ef9328c370dd88d9ba.png")
        );
        assert_eq!(
            second,
            Path::new(".loc/media/Docs/Whitepaper/image-2f03ac0ebb8880c18ed8d4bc56b2e490.png")
        );
    }

    #[test]
    fn local_media_href_is_relative_to_markdown_file_parent() {
        assert_eq!(
            local_media_href(
                Path::new("Tasks/Fix login/page.md"),
                Path::new(".loc/media/Tasks/Fix login/image-0123456789abcdef.png"),
            ),
            "../../.loc/media/Tasks/Fix login/image-0123456789abcdef.png"
        );
        assert_eq!(
            local_media_href(
                Path::new("Roadmap.md"),
                Path::new(".loc/media/Roadmap/image-0123456789abcdef.png"),
            ),
            ".loc/media/Roadmap/image-0123456789abcdef.png"
        );
    }

    #[test]
    fn resolves_local_media_href_to_mount_relative_path() {
        assert_eq!(
            resolve_media_href(
                Path::new("Tasks/Fix login/page.md"),
                "../../.loc/media/Tasks/Fix login/image-0123456789ab.png",
            ),
            Some(Path::new(".loc/media/Tasks/Fix login/image-0123456789ab.png").to_path_buf())
        );
    }

    #[test]
    fn resolves_markdown_escaped_local_media_href() {
        assert_eq!(
            resolve_media_href(
                Path::new("Tasks/Fix login/page.md"),
                "../../.loc/media/Tasks/Fix login \\(new\\)/image-0123456789ab.png",
            ),
            Some(
                Path::new(".loc/media/Tasks/Fix login (new)/image-0123456789ab.png").to_path_buf()
            )
        );
    }

    #[cfg(windows)]
    #[test]
    fn resolves_windows_absolute_media_href_without_stripping_separators() {
        let content_root =
            Path::new("C:\\Users\\runner\\AppData\\Local\\Temp\\loc\\.content\\notion-main\\files");
        let media_path = content_root.join(".loc\\media\\Roadmap\\image-1.png");

        assert_eq!(
            resolve_media_href_with_content_root(
                Path::new("Roadmap/page.md"),
                &media_path.display().to_string(),
                content_root,
            ),
            Some(Path::new(".loc\\media\\Roadmap\\image-1.png").to_path_buf())
        );
    }

    #[test]
    fn selects_only_file_like_media_for_hosted_validation() {
        for kind in ["image", "video", "file", "pdf", "audio"] {
            let asset = MediaAsset {
                block_id: format!("{kind}-1"),
                kind: kind.to_string(),
                source_url: format!("https://secure.notion-static.com/{kind}.bin"),
                local_path: Path::new(".loc/media/Page/media.bin").to_path_buf(),
            };

            assert!(super::should_download(&asset), "{kind} should download");
        }

        let unsupported = MediaAsset {
            block_id: "unsupported-1".to_string(),
            kind: "bookmark".to_string(),
            source_url: "https://example.com/cars.mp4".to_string(),
            local_path: Path::new(".loc/media/Page/video-1.mp4").to_path_buf(),
        };

        assert!(!super::should_download(&unsupported));
    }

    #[test]
    fn unsafe_hosted_asset_never_reaches_an_injected_fetcher() {
        struct NeverCalledFetcher;
        impl PortableMediaCaptureFetcher for NeverCalledFetcher {
            fn fetch(
                &self,
                _hosted_url: &str,
                _max_bytes: usize,
            ) -> locality_core::LocalityResult<PortableMediaCapture> {
                panic!("unsafe URL reached injected fetcher")
            }
        }

        let report = super::fetch_media_asset_report_with_fetcher(
            &[MediaAsset {
                block_id: "unsafe".to_string(),
                kind: "image".to_string(),
                source_url: "https://example.com/not-a-hosted-origin.png".to_string(),
                local_path: Path::new(".loc/media/Page/image.png").to_path_buf(),
            }],
            &NeverCalledFetcher,
        );
        assert!(report.downloaded.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert_eq!(
            report.failed[0].outcome(),
            super::HostedMediaFailureKind::Unsafe
        );
    }

    #[test]
    fn media_download_failure_preserves_legacy_public_fields() {
        let failure = super::MediaDownloadFailure {
            block_id: "block".to_string(),
            kind: "image".to_string(),
            source_url: "https://secure.notion-static.com/image.png".to_string(),
            local_path: Path::new(".loc/media/Page/image.png").to_path_buf(),
            error: "legacy transport message".to_string(),
        };
        assert_eq!(failure.error, "legacy transport message");
        assert_eq!(
            failure.outcome(),
            super::HostedMediaFailureKind::Unavailable
        );
    }

    #[test]
    fn portable_hosted_media_url_policy_is_exact_and_sanitizes_signatures() {
        for url in [
            "https://secure.notion-static.com/image.png",
            "https://prod-files-secure.s3.us-west-2.amazonaws.com/image.png",
            "https://s3.us-west-2.amazonaws.com/secure.notion-static.com/image.png",
            "https://secure.notion-static.com:443/image.png",
        ] {
            validate_portable_hosted_media_url(url)
                .unwrap_or_else(|error| panic!("allowed URL {url}: {error}"));
        }
        for url in [
            "http://secure.notion-static.com/image.png",
            "https://127.0.0.1/image.png",
            "https://[::1]/image.png",
            "https://user@secure.notion-static.com/image.png",
            "https://user:pass@secure.notion-static.com/image.png",
            "https://secure.notion-static.com:444/image.png",
            "https://notion-static.com/image.png",
            "https://sub.secure.notion-static.com/image.png",
            "https://s3.amazonaws.com/secure.notion-static.com/image.png",
            "https://s3.us-west-2.amazonaws.com/other/image.png",
        ] {
            assert!(
                validate_portable_hosted_media_url(url).is_err(),
                "denied URL {url}"
            );
        }
        assert_eq!(
            sanitize_portable_hosted_media_url(concat!(
                "https://secure.notion-static.com/image.png?",
                "X-Amz-Signature=secret&token=secret#fragment"
            ))
            .expect("sanitize"),
            "https://secure.notion-static.com/image.png"
        );
    }

    #[test]
    fn portable_media_follows_only_three_revalidated_redirects() {
        let transport = ScriptedPortableMediaTransport::new([
            scripted_response(
                StatusCode::FOUND,
                Some("https://prod-files-secure.s3.us-west-2.amazonaws.com/second.png"),
                None,
                None,
                None,
                Vec::new(),
            ),
            scripted_response(
                StatusCode::OK,
                None,
                None,
                Some(4),
                Some("image/png"),
                b"data".to_vec(),
            ),
        ]);
        let captured = fetch_portable_media_with_transport(
            &transport,
            "https://secure.notion-static.com/first.png?X-Amz-Signature=secret",
            1024,
        )
        .expect("allowed redirect");
        assert_eq!(captured.bytes, b"data");
        assert_eq!(captured.media_type, "image/png");
        assert_eq!(transport.requests().len(), 2);

        let disallowed = ScriptedPortableMediaTransport::new([scripted_response(
            StatusCode::FOUND,
            Some("https://example.com/stolen.png?token=secret"),
            None,
            None,
            None,
            Vec::new(),
        )]);
        assert_eq!(
            fetch_hosted_media_outcome_with_transport(
                &disallowed,
                "https://secure.notion-static.com/first.png",
                1024,
            ),
            HostedMediaCaptureOutcome::Unsafe
        );
        assert_eq!(disallowed.requests().len(), 1);

        let redirect_loop = ScriptedPortableMediaTransport::new([scripted_response(
            StatusCode::FOUND,
            Some("/first.png"),
            None,
            None,
            None,
            Vec::new(),
        )]);
        assert_eq!(
            fetch_hosted_media_outcome_with_transport(
                &redirect_loop,
                "https://secure.notion-static.com/first.png",
                1024,
            ),
            HostedMediaCaptureOutcome::Unsafe
        );

        let too_many = ScriptedPortableMediaTransport::new((0..=3).map(|index| {
            scripted_response(
                StatusCode::FOUND,
                Some(&format!(
                    "https://secure.notion-static.com/redirect-{}.png",
                    index + 1
                )),
                None,
                None,
                None,
                Vec::new(),
            )
        }));
        assert_eq!(
            fetch_hosted_media_outcome_with_transport(
                &too_many,
                "https://secure.notion-static.com/start.png",
                1024,
            ),
            HostedMediaCaptureOutcome::Unsafe
        );
    }

    #[test]
    fn portable_media_rejects_encoding_length_and_stream_overflow() {
        let encoded = ScriptedPortableMediaTransport::new([scripted_response(
            StatusCode::OK,
            None,
            Some("gzip"),
            Some(4),
            Some("image/png"),
            b"data".to_vec(),
        )]);
        assert_eq!(
            portable_transport_outcome(&encoded, 4),
            HostedMediaCaptureOutcome::Unsafe
        );
        assert_eq!(encoded.requests().len(), 1);

        let declared_oversize = ScriptedPortableMediaTransport::new([scripted_response(
            StatusCode::OK,
            None,
            None,
            Some(5),
            Some("image/png"),
            b"data".to_vec(),
        )]);
        assert_eq!(
            portable_transport_outcome(&declared_oversize, 4),
            HostedMediaCaptureOutcome::TooLarge
        );
        assert_eq!(declared_oversize.requests().len(), 1);

        let mismatched = ScriptedPortableMediaTransport::new([
            scripted_response(
                StatusCode::OK,
                None,
                Some("identity"),
                Some(3),
                Some("image/png"),
                b"data".to_vec(),
            ),
            scripted_response(
                StatusCode::OK,
                None,
                None,
                Some(4),
                Some("image/png"),
                b"data".to_vec(),
            ),
        ]);
        assert_eq!(
            portable_transport_outcome(&mismatched, 4),
            HostedMediaCaptureOutcome::Captured(PortableMediaCapture {
                bytes: b"data".to_vec(),
                media_type: "image/png".to_string(),
            })
        );
        assert_eq!(mismatched.requests().len(), 2);

        let streamed_oversize = ScriptedPortableMediaTransport::new([scripted_response(
            StatusCode::OK,
            None,
            None,
            None,
            Some("image/png"),
            b"12345".to_vec(),
        )]);
        assert_eq!(
            portable_transport_outcome(&streamed_oversize, 4),
            HostedMediaCaptureOutcome::TooLarge
        );
        assert_eq!(streamed_oversize.requests().len(), 1);
    }

    #[test]
    fn portable_media_stream_reads_are_bounded_to_64_kib() {
        let body = vec![7_u8; PORTABLE_MEDIA_READ_BUFFER_BYTES * 2 + 1];
        let transport = ScriptedPortableMediaTransport::new([scripted_response(
            StatusCode::OK,
            None,
            None,
            Some(body.len() as u64),
            Some("APPLICATION/OCTET-STREAM; charset=binary"),
            body.clone(),
        )]);
        let capture = fetch_portable_media_with_transport(
            &transport,
            "https://secure.notion-static.com/data.bin",
            body.len(),
        )
        .expect("bounded stream");
        assert_eq!(capture.bytes, body);
        assert_eq!(capture.media_type, "application/octet-stream");
        assert!(transport.max_read_size() <= 64 * 1024);
    }

    #[test]
    fn portable_media_expiry_is_strict_and_utc() {
        assert!(portable_media_expired("2000-01-01T00:00:00.000Z").expect("past"));
        assert!(!portable_media_expired("2099-01-01T00:00:00Z").expect("future"));
        assert!(portable_media_expired("2099-13-01T00:00:00Z").is_err());
        assert!(portable_media_expired("2099-01-01T00:00:00.Z").is_err());
        assert!(portable_media_expired("2099-01-01T00:00:00+01:00").is_err());
    }

    struct ScriptedPortableMediaTransport {
        responses: Mutex<VecDeque<PortableMediaHttpResponse>>,
        transport_failures: Mutex<usize>,
        requests: Mutex<Vec<String>>,
        timeouts: Mutex<Vec<std::time::Duration>>,
        max_read_size: Arc<AtomicUsize>,
    }

    impl ScriptedPortableMediaTransport {
        fn new(responses: impl IntoIterator<Item = ScriptedResponse>) -> Self {
            let max_read_size = Arc::new(AtomicUsize::new(0));
            let responses = responses
                .into_iter()
                .map(|response| PortableMediaHttpResponse {
                    status: response.status,
                    location: response.location,
                    content_encoding: response.content_encoding,
                    content_length: response.content_length,
                    content_type: response.content_type,
                    body: Box::new(TrackingReader {
                        bytes: std::io::Cursor::new(response.body),
                        max_read_size: Arc::clone(&max_read_size),
                        fail_next_read: response.read_error,
                    }),
                })
                .collect();
            Self {
                responses: Mutex::new(responses),
                transport_failures: Mutex::new(0),
                requests: Mutex::new(Vec::new()),
                timeouts: Mutex::new(Vec::new()),
                max_read_size,
            }
        }

        fn with_transport_failures(
            failures: usize,
            responses: impl IntoIterator<Item = ScriptedResponse>,
        ) -> Self {
            let transport = Self::new(responses);
            *transport
                .transport_failures
                .lock()
                .expect("transport failures") = failures;
            transport
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().expect("requests").clone()
        }

        fn max_read_size(&self) -> usize {
            self.max_read_size.load(Ordering::SeqCst)
        }

        fn timeouts(&self) -> Vec<std::time::Duration> {
            self.timeouts.lock().expect("timeouts").clone()
        }
    }

    impl PortableMediaHttpTransport for ScriptedPortableMediaTransport {
        fn get(
            &self,
            url: &str,
            timeout: std::time::Duration,
        ) -> locality_core::LocalityResult<PortableMediaHttpResponse> {
            self.requests
                .lock()
                .expect("requests")
                .push(url.to_string());
            self.timeouts.lock().expect("timeouts").push(timeout);
            let mut failures = self.transport_failures.lock().expect("transport failures");
            if *failures > 0 {
                *failures -= 1;
                return Err(locality_core::LocalityError::Io(
                    "scripted transport failure".to_string(),
                ));
            }
            self.responses
                .lock()
                .expect("responses")
                .pop_front()
                .ok_or_else(|| {
                    locality_core::LocalityError::InvalidState(
                        "scripted transport exhausted".to_string(),
                    )
                })
        }
    }

    struct ScriptedResponse {
        status: StatusCode,
        location: Option<String>,
        content_encoding: Option<String>,
        content_length: Option<u64>,
        content_type: Option<String>,
        body: Vec<u8>,
        read_error: bool,
    }

    fn scripted_response(
        status: StatusCode,
        location: Option<&str>,
        content_encoding: Option<&str>,
        content_length: Option<u64>,
        content_type: Option<&str>,
        body: Vec<u8>,
    ) -> ScriptedResponse {
        ScriptedResponse {
            status,
            location: location.map(str::to_string),
            content_encoding: content_encoding.map(str::to_string),
            content_length,
            content_type: content_type.map(str::to_string),
            body,
            read_error: false,
        }
    }

    fn scripted_read_error_response(mut response: ScriptedResponse) -> ScriptedResponse {
        response.read_error = true;
        response
    }

    struct TrackingReader {
        bytes: std::io::Cursor<Vec<u8>>,
        max_read_size: Arc<AtomicUsize>,
        fail_next_read: bool,
    }

    impl Read for TrackingReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.max_read_size.fetch_max(buffer.len(), Ordering::SeqCst);
            if self.fail_next_read {
                self.fail_next_read = false;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "scripted response read failure",
                ));
            }
            self.bytes.read(buffer)
        }
    }

    fn portable_transport_outcome(
        transport: &ScriptedPortableMediaTransport,
        max_bytes: usize,
    ) -> HostedMediaCaptureOutcome {
        fetch_hosted_media_outcome_with_transport(
            transport,
            "https://secure.notion-static.com/image.png",
            max_bytes,
        )
    }

    #[test]
    fn hosted_media_retries_only_transient_statuses_with_one_deadline() {
        let transport = ScriptedPortableMediaTransport::new([
            scripted_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                None,
                None,
                None,
                None,
                vec![],
            ),
            scripted_response(
                StatusCode::TOO_MANY_REQUESTS,
                None,
                None,
                None,
                None,
                vec![],
            ),
            scripted_response(
                StatusCode::OK,
                None,
                None,
                Some(4),
                Some("image/png"),
                b"data".to_vec(),
            ),
        ]);
        assert_eq!(
            fetch_hosted_media_outcome_with_policy(
                &transport,
                "https://secure.notion-static.com/image.png",
                1024,
                std::time::Duration::from_secs(1),
                std::time::Duration::from_millis(1),
            ),
            HostedMediaCaptureOutcome::Captured(super::PortableMediaCapture {
                bytes: b"data".to_vec(),
                media_type: "image/png".to_string(),
            })
        );
        assert_eq!(transport.requests().len(), 3);
        let timeouts = transport.timeouts();
        assert_eq!(timeouts.len(), 3);
        assert!(timeouts.windows(2).all(|pair| pair[1] <= pair[0]));
        assert!(
            timeouts
                .iter()
                .all(|timeout| *timeout <= std::time::Duration::from_secs(1))
        );

        let exhausted = ScriptedPortableMediaTransport::new((0..4).map(|_| {
            scripted_response(
                StatusCode::SERVICE_UNAVAILABLE,
                None,
                None,
                None,
                None,
                vec![],
            )
        }));
        assert_eq!(
            fetch_hosted_media_outcome_with_policy(
                &exhausted,
                "https://secure.notion-static.com/image.png",
                1024,
                std::time::Duration::from_secs(1),
                std::time::Duration::ZERO,
            ),
            HostedMediaCaptureOutcome::Unavailable
        );
        assert_eq!(exhausted.requests().len(), 3);

        let expired = ScriptedPortableMediaTransport::new([scripted_response(
            StatusCode::OK,
            None,
            None,
            Some(4),
            Some("image/png"),
            b"data".to_vec(),
        )]);
        assert_eq!(
            fetch_hosted_media_outcome_with_policy(
                &expired,
                "https://secure.notion-static.com/image.png",
                1024,
                std::time::Duration::ZERO,
                std::time::Duration::ZERO,
            ),
            HostedMediaCaptureOutcome::Unavailable
        );
        assert!(expired.requests().is_empty());
    }

    #[test]
    fn hosted_media_terminal_4xx_is_not_retried() {
        let transport = ScriptedPortableMediaTransport::new([
            scripted_response(StatusCode::NOT_FOUND, None, None, None, None, vec![]),
            scripted_response(
                StatusCode::OK,
                None,
                None,
                Some(4),
                Some("image/png"),
                b"data".to_vec(),
            ),
        ]);
        assert_eq!(
            fetch_hosted_media_outcome_with_transport(
                &transport,
                "https://secure.notion-static.com/image.png",
                1024,
            ),
            HostedMediaCaptureOutcome::Unavailable
        );
        assert_eq!(transport.requests().len(), 1);
    }

    #[test]
    fn hosted_media_retries_transport_and_read_io_failures() {
        let success = || {
            scripted_response(
                StatusCode::OK,
                None,
                None,
                Some(4),
                Some("image/png"),
                b"data".to_vec(),
            )
        };
        let transport_failure =
            ScriptedPortableMediaTransport::with_transport_failures(1, [success()]);
        assert_eq!(
            fetch_hosted_media_outcome_with_policy(
                &transport_failure,
                "https://secure.notion-static.com/image.png",
                1024,
                std::time::Duration::from_secs(1),
                std::time::Duration::ZERO,
            ),
            HostedMediaCaptureOutcome::Captured(PortableMediaCapture {
                bytes: b"data".to_vec(),
                media_type: "image/png".to_string(),
            })
        );
        assert_eq!(transport_failure.requests().len(), 2);

        let read_failure = ScriptedPortableMediaTransport::new([
            scripted_read_error_response(success()),
            success(),
        ]);
        assert_eq!(
            fetch_hosted_media_outcome_with_policy(
                &read_failure,
                "https://secure.notion-static.com/image.png",
                1024,
                std::time::Duration::from_secs(1),
                std::time::Duration::ZERO,
            ),
            HostedMediaCaptureOutcome::Captured(PortableMediaCapture {
                bytes: b"data".to_vec(),
                media_type: "image/png".to_string(),
            })
        );
        assert_eq!(read_failure.requests().len(), 2);
    }

    #[derive(Default)]
    struct ManualHostedMediaRetryClock {
        elapsed: Mutex<std::time::Duration>,
    }

    impl ManualHostedMediaRetryClock {
        fn advance(&self, duration: std::time::Duration) {
            let mut elapsed = self.elapsed.lock().expect("manual elapsed");
            *elapsed += duration;
        }
    }

    impl HostedMediaRetryClock for ManualHostedMediaRetryClock {
        fn elapsed(&self) -> std::time::Duration {
            *self.elapsed.lock().expect("manual elapsed")
        }

        fn sleep(&self, duration: std::time::Duration) {
            self.advance(duration);
        }
    }

    struct DeadlineConsumingTransport {
        clock: Arc<ManualHostedMediaRetryClock>,
        timeouts: Mutex<Vec<std::time::Duration>>,
    }

    impl PortableMediaHttpTransport for DeadlineConsumingTransport {
        fn get(
            &self,
            _url: &str,
            timeout: std::time::Duration,
        ) -> locality_core::LocalityResult<PortableMediaHttpResponse> {
            self.timeouts
                .lock()
                .expect("deadline timeouts")
                .push(timeout);
            self.clock.advance(std::time::Duration::from_millis(600));
            Err(locality_core::LocalityError::Io(
                "scripted transport timeout".to_string(),
            ))
        }
    }

    #[test]
    fn hosted_media_retries_share_one_total_elapsed_deadline() {
        let clock = Arc::new(ManualHostedMediaRetryClock::default());
        let transport = DeadlineConsumingTransport {
            clock: Arc::clone(&clock),
            timeouts: Mutex::new(Vec::new()),
        };

        assert_eq!(
            fetch_hosted_media_outcome_with_clock(
                &transport,
                "https://secure.notion-static.com/image.png",
                1024,
                std::time::Duration::from_secs(1),
                std::time::Duration::from_millis(100),
                clock.as_ref(),
            ),
            HostedMediaCaptureOutcome::Unavailable
        );
        assert_eq!(
            transport
                .timeouts
                .lock()
                .expect("deadline timeouts")
                .as_slice(),
            [
                std::time::Duration::from_millis(1000),
                std::time::Duration::from_millis(300),
            ]
        );
        assert_eq!(
            HostedMediaRetryClock::elapsed(clock.as_ref()),
            std::time::Duration::from_millis(1300)
        );
    }
}
