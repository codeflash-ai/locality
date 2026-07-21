//! Sealed-token Phase 1 sandbox bootstrap client.
//!
//! Scope is fixed before the bootstrap token is issued. The client sends no
//! tenant, actor, workload, profile, filter, or requested-action fields and
//! persists no per-file or SQLite state.

use std::ffi::OsString;
use std::fmt::{Debug, Display, Formatter};
use std::fs;
use std::io::{self, Read};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use locality_protocol::{
    OpaqueBootstrapExchangeRequest, SandboxSessionState, SandboxSessionStatus, SessionCapability,
    SessionErrorCode, TarContentEncoding, TarExportOffer,
};
use localityd::remote_truth::{ReplicaArchive, ReplicaArchiveEncoding};
use localityd::replica_materializer::{
    ExpectedReplicaMaterializationReceipt, ReplicaMaterializationLimits,
    ReplicaMaterializationSummary, materialize_replica_archive_with_expected_receipt,
};
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_TYPE, HeaderMap};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

const JSON_MEDIA_TYPE: &str = "application/json";
const TAR_MEDIA_TYPE: &str = "application/x-tar";
const MAX_JSON_RESPONSE_BYTES: u64 = 1024 * 1024;
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const BOOTSTRAP_EXCHANGE_ATTEMPTS: usize = 2;
const BOOTSTRAP_IDEMPOTENCY_DOMAIN: &[u8] = b"locality.session-exchange-idempotency.v1\0";
const IDEMPOTENCY_KEY_HEADER: &str = "Idempotency-Key";
static REQWEST_CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();

#[derive(Clone)]
pub struct SandboxBootstrapToken(String);

impl SandboxBootstrapToken {
    pub fn new(value: impl Into<String>) -> Result<Self, SandboxInitError> {
        let value = value.into();
        if value.is_empty() || value.contains(['\r', '\n']) {
            return Err(SandboxInitError::InvalidBootstrapToken);
        }
        Ok(Self(value))
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl Debug for SandboxBootstrapToken {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SandboxBootstrapToken(<redacted>)")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxInitOptions {
    pub api_url: String,
    pub root: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SandboxInitReport {
    pub ok: bool,
    pub command: &'static str,
    pub root: String,
    pub session_id: String,
    pub content_encoding: &'static str,
    pub entries: u64,
    pub files: u64,
    pub directories: u64,
    pub materialized_bytes: u64,
    pub decoded_bytes: u64,
}

#[derive(Debug)]
pub enum SandboxInitError {
    MissingBootstrapToken,
    AmbiguousBootstrapToken,
    InvalidBootstrapToken,
    BootstrapTokenEnvironmentNotUnicode,
    ReadBootstrapToken(io::Error),
    InvalidApiUrl(&'static str),
    CurrentDirectory(io::Error),
    InvalidDestination,
    DestinationParentMissing(PathBuf),
    DestinationExists(PathBuf),
    Http {
        operation: &'static str,
        detail: String,
    },
    HttpStatus {
        operation: &'static str,
        status: StatusCode,
    },
    JsonResponseTooLarge {
        operation: &'static str,
        limit: u64,
    },
    InvalidJson {
        operation: &'static str,
        detail: String,
    },
    UnexpectedMediaType {
        operation: &'static str,
        expected: &'static str,
        actual: String,
    },
    InvalidCapability(&'static str),
    SessionIdMismatch,
    ComponentVersion(String),
    SessionNotReady {
        state: SandboxSessionState,
        code: Option<SessionErrorCode>,
    },
    InvalidReadySession(&'static str),
    InvalidExportOffer(&'static str),
    ExportLimit {
        limit: &'static str,
        offered: u64,
        maximum: u64,
    },
    UnsupportedExportEncoding(String),
    Materialization(String),
}

impl SandboxInitError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingBootstrapToken => "bootstrap_token_missing",
            Self::AmbiguousBootstrapToken => "bootstrap_token_ambiguous",
            Self::InvalidBootstrapToken | Self::BootstrapTokenEnvironmentNotUnicode => {
                "bootstrap_token_invalid"
            }
            Self::ReadBootstrapToken(_) => "bootstrap_token_read_failed",
            Self::InvalidApiUrl(_) => "api_url_invalid",
            Self::CurrentDirectory(_) => "current_directory_failed",
            Self::InvalidDestination
            | Self::DestinationParentMissing(_)
            | Self::DestinationExists(_) => "destination_invalid",
            Self::Http { .. } | Self::HttpStatus { .. } => "backend_request_failed",
            Self::JsonResponseTooLarge { .. }
            | Self::InvalidJson { .. }
            | Self::UnexpectedMediaType { .. }
            | Self::InvalidCapability(_)
            | Self::SessionIdMismatch
            | Self::InvalidReadySession(_)
            | Self::InvalidExportOffer(_)
            | Self::UnsupportedExportEncoding(_) => "backend_protocol_invalid",
            Self::ComponentVersion(_) => "update_required",
            Self::SessionNotReady { .. } => "session_not_ready",
            Self::ExportLimit { .. } => "export_limit_exceeded",
            Self::Materialization(_) => "materialization_failed",
        }
    }

    pub fn is_usage_error(&self) -> bool {
        matches!(
            self,
            Self::MissingBootstrapToken
                | Self::AmbiguousBootstrapToken
                | Self::InvalidBootstrapToken
                | Self::BootstrapTokenEnvironmentNotUnicode
                | Self::ReadBootstrapToken(_)
                | Self::InvalidApiUrl(_)
                | Self::CurrentDirectory(_)
                | Self::InvalidDestination
                | Self::DestinationParentMissing(_)
                | Self::DestinationExists(_)
        )
    }
}

impl Display for SandboxInitError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingBootstrapToken => formatter.write_str(
                "provide the bootstrap token through LOCALITY_BOOTSTRAP_TOKEN or --bootstrap-token-stdin",
            ),
            Self::AmbiguousBootstrapToken => formatter.write_str(
                "set only one bootstrap token source: LOCALITY_BOOTSTRAP_TOKEN or --bootstrap-token-stdin",
            ),
            Self::InvalidBootstrapToken => {
                formatter.write_str("bootstrap token must be non-empty and contain no newlines")
            }
            Self::BootstrapTokenEnvironmentNotUnicode => {
                formatter.write_str("LOCALITY_BOOTSTRAP_TOKEN is not valid Unicode")
            }
            Self::ReadBootstrapToken(error) => {
                write!(formatter, "failed to read bootstrap token from stdin: {error}")
            }
            Self::InvalidApiUrl(reason) => write!(formatter, "invalid API URL: {reason}"),
            Self::CurrentDirectory(error) => {
                write!(formatter, "failed to resolve the current directory: {error}")
            }
            Self::InvalidDestination => {
                formatter.write_str("sandbox root must have an existing parent and file name")
            }
            Self::DestinationParentMissing(path) => write!(
                formatter,
                "sandbox root parent does not exist: {}",
                path.display()
            ),
            Self::DestinationExists(path) => {
                write!(formatter, "sandbox root already exists: {}", path.display())
            }
            Self::Http { operation, detail } => {
                write!(formatter, "{operation} failed: {detail}")
            }
            Self::HttpStatus { operation, status } => {
                write!(formatter, "{operation} returned HTTP {status}")
            }
            Self::JsonResponseTooLarge { operation, limit } => write!(
                formatter,
                "{operation} JSON response exceeds {limit} bytes"
            ),
            Self::InvalidJson { operation, detail } => {
                write!(formatter, "{operation} returned invalid JSON: {detail}")
            }
            Self::UnexpectedMediaType {
                operation,
                expected,
                actual,
            } => write!(
                formatter,
                "{operation} returned media type `{actual}`; expected `{expected}`"
            ),
            Self::InvalidCapability(reason) => {
                write!(formatter, "bootstrap returned an invalid capability: {reason}")
            }
            Self::SessionIdMismatch => {
                formatter.write_str("session status ID does not match the bootstrap capability")
            }
            Self::ComponentVersion(detail) => write!(formatter, "{detail}"),
            Self::SessionNotReady { state, code } => {
                write!(formatter, "sandbox session is {state:?}")?;
                if let Some(code) = code {
                    write!(formatter, " ({code:?})")?;
                }
                Ok(())
            }
            Self::InvalidReadySession(reason) => {
                write!(formatter, "ready sandbox session is invalid: {reason}")
            }
            Self::InvalidExportOffer(reason) => {
                write!(formatter, "sandbox export offer is invalid: {reason}")
            }
            Self::ExportLimit {
                limit,
                offered,
                maximum,
            } => write!(
                formatter,
                "sandbox export {limit} {offered} exceeds client maximum {maximum}"
            ),
            Self::UnsupportedExportEncoding(encoding) => {
                write!(formatter, "unsupported sandbox export encoding `{encoding}`")
            }
            Self::Materialization(detail) => {
                write!(formatter, "sandbox export materialization failed: {detail}")
            }
        }
    }
}

impl std::error::Error for SandboxInitError {}

/// Resolve exactly one token source without ever placing the token in argv.
pub fn resolve_bootstrap_token(
    use_stdin: bool,
    environment: Option<OsString>,
    stdin: &mut impl Read,
) -> Result<SandboxBootstrapToken, SandboxInitError> {
    if use_stdin && environment.is_some() {
        return Err(SandboxInitError::AmbiguousBootstrapToken);
    }
    let value = if use_stdin {
        let mut value = String::new();
        stdin
            .read_to_string(&mut value)
            .map_err(SandboxInitError::ReadBootstrapToken)?;
        value.trim_end_matches(['\r', '\n']).to_string()
    } else if let Some(value) = environment {
        value
            .into_string()
            .map_err(|_| SandboxInitError::BootstrapTokenEnvironmentNotUnicode)?
    } else {
        return Err(SandboxInitError::MissingBootstrapToken);
    };
    SandboxBootstrapToken::new(value)
}

pub fn run_sandbox_init(
    options: SandboxInitOptions,
    bootstrap_token: SandboxBootstrapToken,
) -> Result<SandboxInitReport, SandboxInitError> {
    let root = absolute_destination(&options.root)?;
    validate_destination(&root)?;
    let client = SandboxHttpClient::new(&options.api_url)?;

    let capability = client.exchange_bootstrap(&bootstrap_token)?;
    validate_capability(&capability)?;
    let status = client.session_status(&capability)?;
    let (offer, expected_receipt) = validate_status(&capability, &status)?;
    let limits = limits_for_offer(offer)?;
    let (encoding, response) = client.open_export(&capability, offer)?;
    let archive = ReplicaArchive::new(encoding, response);
    let summary =
        materialize_replica_archive_with_expected_receipt(archive, &root, limits, expected_receipt)
            .map_err(|error| SandboxInitError::Materialization(error.to_string()))?;

    Ok(report(&root, &capability, encoding, summary))
}

fn absolute_destination(path: &Path) -> Result<PathBuf, SandboxInitError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .map_err(SandboxInitError::CurrentDirectory)
    }
}

fn validate_destination(root: &Path) -> Result<(), SandboxInitError> {
    let parent = root
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or(SandboxInitError::InvalidDestination)?;
    if root.file_name().is_none() {
        return Err(SandboxInitError::InvalidDestination);
    }
    match fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) | Err(_) => {
            return Err(SandboxInitError::DestinationParentMissing(
                parent.to_path_buf(),
            ));
        }
    }
    if fs::symlink_metadata(root).is_ok() {
        return Err(SandboxInitError::DestinationExists(root.to_path_buf()));
    }
    Ok(())
}

fn validate_capability(capability: &SessionCapability) -> Result<(), SandboxInitError> {
    if capability.session_id.as_str().is_empty() {
        return Err(SandboxInitError::InvalidCapability("session ID is empty"));
    }
    if capability.opaque_capability.is_empty() {
        return Err(SandboxInitError::InvalidCapability(
            "opaque capability is empty",
        ));
    }
    if capability.expires_at.is_empty() {
        return Err(SandboxInitError::InvalidCapability("expiry is empty"));
    }
    Ok(())
}

fn validate_status<'a>(
    capability: &SessionCapability,
    status: &'a SandboxSessionStatus,
) -> Result<(&'a TarExportOffer, ExpectedReplicaMaterializationReceipt), SandboxInitError> {
    status
        .versions
        .validate_required()
        .map_err(|error| SandboxInitError::ComponentVersion(error.to_string()))?;
    if status.session_id != capability.session_id {
        return Err(SandboxInitError::SessionIdMismatch);
    }
    if status.state != SandboxSessionState::Ready {
        return Err(SandboxInitError::SessionNotReady {
            state: status.state,
            code: status.error.as_ref().map(|error| error.code),
        });
    }
    if status.error.is_some() {
        return Err(SandboxInitError::InvalidReadySession("error is present"));
    }
    let offer = status
        .export_offer
        .as_ref()
        .ok_or(SandboxInitError::InvalidReadySession(
            "export offer is missing",
        ))?;
    let expected_receipt = validate_offer(offer)?;
    Ok((offer, expected_receipt))
}

fn validate_offer(
    offer: &TarExportOffer,
) -> Result<ExpectedReplicaMaterializationReceipt, SandboxInitError> {
    if offer.media_type != TAR_MEDIA_TYPE {
        return Err(SandboxInitError::InvalidExportOffer(
            "media type must be application/x-tar",
        ));
    }
    if !offer
        .supported_content_encodings
        .contains(&TarContentEncoding::Identity)
    {
        return Err(SandboxInitError::InvalidExportOffer(
            "identity encoding fallback is missing",
        ));
    }
    let decoded_tar_sha256 =
        parse_sha256(&offer.decoded_tar_sha256).ok_or(SandboxInitError::InvalidExportOffer(
            "decoded tar digest must use canonical sha256:<64 lowercase hex>",
        ))?;
    Ok(ExpectedReplicaMaterializationReceipt {
        decoded_tar_sha256,
        decoded_bytes: offer.decoded_bytes,
        entries: offer.selected_entries,
    })
}

fn parse_sha256(value: &str) -> Option<[u8; 32]> {
    let hex = value.strip_prefix("sha256:")?;
    if hex.len() != 64
        || !hex
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        return None;
    }
    let mut digest = [0_u8; 32];
    for (output, pair) in digest.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
        *output = (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]);
    }
    Some(digest)
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => unreachable!("canonical lowercase hexadecimal was validated"),
    }
}

fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

fn require_api_host(api_url: &reqwest::Url) -> Result<(), SandboxInitError> {
    let host = api_url
        .host_str()
        .ok_or(SandboxInitError::InvalidApiUrl("host is required"))?;
    if api_url.scheme() == "http" && !is_loopback_host(host) {
        return Err(SandboxInitError::InvalidApiUrl(
            "http scheme is allowed only for loopback hosts",
        ));
    }
    Ok(())
}

fn limits_for_offer(
    offer: &TarExportOffer,
) -> Result<ReplicaMaterializationLimits, SandboxInitError> {
    let defaults = ReplicaMaterializationLimits::default();
    if offer.selected_entries > defaults.max_entries {
        return Err(SandboxInitError::ExportLimit {
            limit: "entry count",
            offered: offer.selected_entries,
            maximum: defaults.max_entries,
        });
    }
    if offer.decoded_bytes > defaults.max_decoded_bytes {
        return Err(SandboxInitError::ExportLimit {
            limit: "decoded bytes",
            offered: offer.decoded_bytes,
            maximum: defaults.max_decoded_bytes,
        });
    }
    Ok(ReplicaMaterializationLimits {
        max_entries: offer.selected_entries,
        max_decoded_bytes: offer.decoded_bytes,
        ..defaults
    })
}

fn report(
    root: &Path,
    capability: &SessionCapability,
    encoding: ReplicaArchiveEncoding,
    summary: ReplicaMaterializationSummary,
) -> SandboxInitReport {
    SandboxInitReport {
        ok: true,
        command: "sandbox_init",
        root: root.to_string_lossy().into_owned(),
        session_id: capability.session_id.as_str().to_string(),
        content_encoding: match encoding {
            ReplicaArchiveEncoding::Identity => "identity",
            ReplicaArchiveEncoding::Zstd => "zstd",
        },
        entries: summary.entries,
        files: summary.files,
        directories: summary.directories,
        materialized_bytes: summary.materialized_bytes,
        decoded_bytes: summary.decoded_bytes,
    }
}

struct SandboxHttpClient {
    client: Client,
    api_url: reqwest::Url,
}

impl SandboxHttpClient {
    fn new(api_url: &str) -> Result<Self, SandboxInitError> {
        let api_url = reqwest::Url::parse(api_url)
            .map_err(|_| SandboxInitError::InvalidApiUrl("URL cannot be parsed"))?;
        if !matches!(api_url.scheme(), "http" | "https") {
            return Err(SandboxInitError::InvalidApiUrl(
                "scheme must be http or https",
            ));
        }
        require_api_host(&api_url)?;
        if !api_url.username().is_empty() || api_url.password().is_some() {
            return Err(SandboxInitError::InvalidApiUrl(
                "embedded credentials are not allowed",
            ));
        }
        if api_url.query().is_some() || api_url.fragment().is_some() {
            return Err(SandboxInitError::InvalidApiUrl(
                "query strings and fragments are not allowed",
            ));
        }
        if api_url.path() != "/" && !api_url.path().is_empty() {
            return Err(SandboxInitError::InvalidApiUrl(
                "URL must not contain a path",
            ));
        }
        REQWEST_CRYPTO_PROVIDER.get_or_init(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .timeout(HTTP_REQUEST_TIMEOUT)
            .build()
            .map_err(|error| SandboxInitError::Http {
                operation: "HTTP client setup",
                detail: error.without_url().to_string(),
            })?;
        Ok(Self { client, api_url })
    }

    fn exchange_bootstrap(
        &self,
        token: &SandboxBootstrapToken,
    ) -> Result<SessionCapability, SandboxInitError> {
        let request = OpaqueBootstrapExchangeRequest {
            bootstrap_token: token.expose().to_string(),
        };
        let idempotency_key = derive_idempotency_key(token);

        for attempt in 0..BOOTSTRAP_EXCHANGE_ATTEMPTS {
            let response = match self
                .client
                .post(self.sessions_url())
                .header(ACCEPT, JSON_MEDIA_TYPE)
                .header(IDEMPOTENCY_KEY_HEADER, &idempotency_key)
                .json(&request)
                .send()
            {
                Ok(response) => response,
                Err(error) => {
                    let error = SandboxInitError::Http {
                        operation: "bootstrap exchange",
                        detail: error.without_url().to_string(),
                    };
                    if has_retry_remaining(attempt) {
                        continue;
                    }
                    return Err(error);
                }
            };

            if is_retriable_bootstrap_status(response.status()) && has_retry_remaining(attempt) {
                continue;
            }
            match read_json_response(response, "bootstrap exchange") {
                Ok(capability) => return Ok(capability),
                Err(error)
                    if has_retry_remaining(attempt) && is_ambiguous_bootstrap_error(&error) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }

        unreachable!("bootstrap exchange attempt loop always returns")
    }

    fn session_status(
        &self,
        capability: &SessionCapability,
    ) -> Result<SandboxSessionStatus, SandboxInitError> {
        let response = self
            .client
            .get(self.session_url(capability.session_id.as_str()))
            .header(ACCEPT, JSON_MEDIA_TYPE)
            .bearer_auth(&capability.opaque_capability)
            .send()
            .map_err(|error| SandboxInitError::Http {
                operation: "session status",
                detail: error.without_url().to_string(),
            })?;
        read_json_response(response, "session status")
    }

    fn open_export(
        &self,
        capability: &SessionCapability,
        offer: &TarExportOffer,
    ) -> Result<(ReplicaArchiveEncoding, Response), SandboxInitError> {
        let response = self
            .client
            .get(self.export_url(capability.session_id.as_str()))
            .header(ACCEPT, TAR_MEDIA_TYPE)
            .header(ACCEPT_ENCODING, "zstd, identity")
            .bearer_auth(&capability.opaque_capability)
            .send()
            .map_err(|error| SandboxInitError::Http {
                operation: "session export",
                detail: error.without_url().to_string(),
            })?;
        ensure_success(&response, "session export")?;
        require_media_type(response.headers(), "session export", TAR_MEDIA_TYPE)?;
        let encoding = response_encoding(response.headers())?;
        let offered = match encoding {
            ReplicaArchiveEncoding::Identity => TarContentEncoding::Identity,
            ReplicaArchiveEncoding::Zstd => TarContentEncoding::Zstd,
        };
        if !offer.supported_content_encodings.contains(&offered) {
            return Err(SandboxInitError::UnsupportedExportEncoding(
                encoding_name(encoding).to_string(),
            ));
        }
        Ok((encoding, response))
    }

    fn sessions_url(&self) -> reqwest::Url {
        endpoint_url(&self.api_url, &["v1", "sessions"])
    }

    fn session_url(&self, session_id: &str) -> reqwest::Url {
        endpoint_url(&self.api_url, &["v1", "sessions", session_id])
    }

    fn export_url(&self, session_id: &str) -> reqwest::Url {
        endpoint_url(&self.api_url, &["v1", "sessions", session_id, "export"])
    }
}

fn derive_idempotency_key(token: &SandboxBootstrapToken) -> String {
    let mut hasher = Sha256::new();
    hasher.update(BOOTSTRAP_IDEMPOTENCY_DOMAIN);
    hasher.update(token.expose().as_bytes());
    let digest = hasher.finalize();
    let mut encoded = [0_u8; 64];
    const LOWER_HEX: &[u8; 16] = b"0123456789abcdef";
    for (index, byte) in digest.into_iter().enumerate() {
        encoded[index * 2] = LOWER_HEX[usize::from(byte >> 4)];
        encoded[index * 2 + 1] = LOWER_HEX[usize::from(byte & 0x0f)];
    }
    String::from_utf8(encoded.to_vec()).expect("lowercase hexadecimal is valid UTF-8")
}

fn has_retry_remaining(attempt: usize) -> bool {
    attempt + 1 < BOOTSTRAP_EXCHANGE_ATTEMPTS
}

fn is_retriable_bootstrap_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT
    )
}

fn is_ambiguous_bootstrap_error(error: &SandboxInitError) -> bool {
    matches!(
        error,
        SandboxInitError::Http {
            operation: "bootstrap exchange",
            ..
        }
    )
}

fn endpoint_url(base: &reqwest::Url, segments: &[&str]) -> reqwest::Url {
    let mut url = base.clone();
    url.set_path("");
    url.path_segments_mut()
        .expect("http URLs support path segments")
        .extend(segments);
    url
}

fn read_json_response<T: DeserializeOwned>(
    mut response: Response,
    operation: &'static str,
) -> Result<T, SandboxInitError> {
    ensure_success(&response, operation)?;
    require_media_type(response.headers(), operation, JSON_MEDIA_TYPE)?;
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take(MAX_JSON_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| SandboxInitError::Http {
            operation,
            detail: error.to_string(),
        })?;
    if bytes.len() as u64 > MAX_JSON_RESPONSE_BYTES {
        return Err(SandboxInitError::JsonResponseTooLarge {
            operation,
            limit: MAX_JSON_RESPONSE_BYTES,
        });
    }
    serde_json::from_slice(&bytes).map_err(|error| SandboxInitError::InvalidJson {
        operation,
        detail: error.to_string(),
    })
}

fn ensure_success(response: &Response, operation: &'static str) -> Result<(), SandboxInitError> {
    if response.status() == StatusCode::OK {
        Ok(())
    } else {
        Err(SandboxInitError::HttpStatus {
            operation,
            status: response.status(),
        })
    }
}

fn require_media_type(
    headers: &HeaderMap,
    operation: &'static str,
    expected: &'static str,
) -> Result<(), SandboxInitError> {
    let actual = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("<missing>");
    if actual == expected {
        Ok(())
    } else {
        Err(SandboxInitError::UnexpectedMediaType {
            operation,
            expected,
            actual: actual.to_string(),
        })
    }
}

fn response_encoding(headers: &HeaderMap) -> Result<ReplicaArchiveEncoding, SandboxInitError> {
    let encoding = match headers.get(CONTENT_ENCODING) {
        None => return Ok(ReplicaArchiveEncoding::Identity),
        Some(value) => value
            .to_str()
            .map_err(|_| SandboxInitError::UnsupportedExportEncoding("<invalid>".to_string()))?,
    };
    match encoding {
        "identity" => Ok(ReplicaArchiveEncoding::Identity),
        "zstd" => Ok(ReplicaArchiveEncoding::Zstd),
        other => Err(SandboxInitError::UnsupportedExportEncoding(
            other.to_string(),
        )),
    }
}

fn encoding_name(encoding: ReplicaArchiveEncoding) -> &'static str {
    match encoding {
        ReplicaArchiveEncoding::Identity => "identity",
        ReplicaArchiveEncoding::Zstd => "zstd",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{self, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc::{self, Receiver};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use locality_core::portable::SessionId;

    use super::*;

    #[derive(Debug)]
    struct CapturedRequest {
        method: String,
        path: String,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
    }

    enum TestResponse {
        DropConnection,
        Json { status: &'static str, body: Vec<u8> },
    }

    struct TestServer {
        api_url: String,
        requests: Receiver<CapturedRequest>,
        handle: JoinHandle<()>,
    }

    impl TestServer {
        fn start(responses: Vec<TestResponse>, reject_extra_request: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
            listener
                .set_nonblocking(true)
                .expect("set test listener nonblocking");
            let address = listener.local_addr().expect("test server address");
            let (sender, requests) = mpsc::channel();
            let handle = thread::spawn(move || {
                for response in responses {
                    let mut stream =
                        accept_before(&listener, Instant::now() + Duration::from_secs(5));
                    let request = read_request(&mut stream);
                    sender.send(request).expect("capture request");
                    match response {
                        TestResponse::DropConnection => {}
                        TestResponse::Json { status, body } => {
                            write_json_response(&mut stream, status, &body);
                        }
                    }
                }
                if reject_extra_request {
                    let deadline = Instant::now() + Duration::from_millis(250);
                    loop {
                        match listener.accept() {
                            Ok((mut stream, _)) => {
                                let request = read_request(&mut stream);
                                panic!("unexpected retry: {request:?}");
                            }
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                if Instant::now() >= deadline {
                                    break;
                                }
                                thread::sleep(Duration::from_millis(5));
                            }
                            Err(error) => panic!("accept unexpected request: {error}"),
                        }
                    }
                }
            });
            Self {
                api_url: format!("http://{address}"),
                requests,
                handle,
            }
        }

        fn finish(self) -> Vec<CapturedRequest> {
            self.handle.join().expect("test server completed");
            self.requests.try_iter().collect()
        }
    }

    #[test]
    fn dropped_bootstrap_response_retries_with_identical_key_and_body() {
        let response = serde_json::to_vec(&capability()).expect("serialize capability");
        let server = TestServer::start(
            vec![
                TestResponse::DropConnection,
                TestResponse::Json {
                    status: "200 OK",
                    body: response,
                },
            ],
            true,
        );
        let client = SandboxHttpClient::new(&server.api_url).expect("HTTP client");
        let token = SandboxBootstrapToken::new("bootstrap-secret").expect("bootstrap token");

        let actual = client
            .exchange_bootstrap(&token)
            .expect("retry bootstrap exchange");

        assert_eq!(actual, capability());
        let requests = server.finish();
        assert_eq!(requests.len(), 2);
        assert_bootstrap_request(&requests[0], br#"{"bootstrap_token":"bootstrap-secret"}"#);
        assert_bootstrap_request(&requests[1], br#"{"bootstrap_token":"bootstrap-secret"}"#);
        assert_eq!(requests[0].body, requests[1].body);
        assert_eq!(idempotency_key(&requests[0]), idempotency_key(&requests[1]));
        assert_eq!(
            idempotency_key(&requests[0]),
            "fe1fd6a544a78d3a3087bf1517b0ca83b6d122bf1d88d1eddc264e883500bded"
        );
    }

    #[test]
    fn retriable_gateway_responses_retry_with_identical_key_and_body() {
        let cases = [
            "502 Bad Gateway",
            "503 Service Unavailable",
            "504 Gateway Timeout",
        ];

        for status in cases {
            let response = serde_json::to_vec(&capability()).expect("serialize capability");
            let server = TestServer::start(
                vec![
                    TestResponse::Json {
                        status,
                        body: Vec::new(),
                    },
                    TestResponse::Json {
                        status: "200 OK",
                        body: response,
                    },
                ],
                true,
            );
            let client = SandboxHttpClient::new(&server.api_url).expect("HTTP client");
            let token = SandboxBootstrapToken::new("bootstrap-secret").expect("bootstrap token");

            assert_eq!(
                client
                    .exchange_bootstrap(&token)
                    .expect("retry gateway response"),
                capability()
            );

            let requests = server.finish();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].body, requests[1].body);
            assert_eq!(idempotency_key(&requests[0]), idempotency_key(&requests[1]));
        }
    }

    #[test]
    fn bootstrap_gateway_retry_is_bounded_to_two_attempts() {
        let server = TestServer::start(
            vec![
                TestResponse::Json {
                    status: "503 Service Unavailable",
                    body: Vec::new(),
                },
                TestResponse::Json {
                    status: "503 Service Unavailable",
                    body: Vec::new(),
                },
            ],
            true,
        );
        let client = SandboxHttpClient::new(&server.api_url).expect("HTTP client");
        let token = SandboxBootstrapToken::new("bootstrap-secret").expect("bootstrap token");

        let error = client
            .exchange_bootstrap(&token)
            .expect_err("repeated service failure must stop");

        assert!(matches!(
            &error,
            SandboxInitError::HttpStatus {
                operation: "bootstrap exchange",
                status: StatusCode::SERVICE_UNAVAILABLE
            }
        ));
        assert_eq!(
            error.to_string(),
            "bootstrap exchange returned HTTP 503 Service Unavailable"
        );
        let requests = server.finish();
        assert_eq!(requests.len(), BOOTSTRAP_EXCHANGE_ATTEMPTS);
        assert_eq!(requests[0].body, requests[1].body);
        assert_eq!(idempotency_key(&requests[0]), idempotency_key(&requests[1]));
    }

    #[test]
    fn bootstrap_idempotency_keys_are_stable_per_token_and_separate_between_tokens() {
        let response = serde_json::to_vec(&capability()).expect("serialize capability");
        let server = TestServer::start(
            vec![
                TestResponse::Json {
                    status: "200 OK",
                    body: response.clone(),
                },
                TestResponse::Json {
                    status: "200 OK",
                    body: response.clone(),
                },
                TestResponse::Json {
                    status: "200 OK",
                    body: response,
                },
            ],
            true,
        );
        let token = SandboxBootstrapToken::new("bootstrap-secret").expect("bootstrap token");
        let different_token = SandboxBootstrapToken::new("different-bootstrap-secret")
            .expect("different bootstrap token");

        assert_eq!(
            SandboxHttpClient::new(&server.api_url)
                .expect("first HTTP client")
                .exchange_bootstrap(&token)
                .expect("first exchange"),
            capability()
        );
        assert_eq!(
            SandboxHttpClient::new(&server.api_url)
                .expect("second HTTP client")
                .exchange_bootstrap(&token)
                .expect("second exchange"),
            capability()
        );
        assert_eq!(
            SandboxHttpClient::new(&server.api_url)
                .expect("third HTTP client")
                .exchange_bootstrap(&different_token)
                .expect("third exchange"),
            capability()
        );

        let requests = server.finish();
        assert_eq!(requests.len(), 3);
        let first = idempotency_key(&requests[0]);
        let second = idempotency_key(&requests[1]);
        let third = idempotency_key(&requests[2]);
        assert_valid_idempotency_key(first);
        assert_valid_idempotency_key(second);
        assert_valid_idempotency_key(third);
        assert_eq!(first, second);
        assert_ne!(first, third);
        assert_eq!(
            first,
            "fe1fd6a544a78d3a3087bf1517b0ca83b6d122bf1d88d1eddc264e883500bded"
        );
        assert_eq!(
            third,
            "c5ffe95233e3b77d1a6170c672587fa9fdf3f7feccbf2a6e30ba3eb3bcf81b9b"
        );
        assert_bootstrap_request(&requests[0], br#"{"bootstrap_token":"bootstrap-secret"}"#);
        assert_bootstrap_request(&requests[1], br#"{"bootstrap_token":"bootstrap-secret"}"#);
        assert_bootstrap_request(
            &requests[2],
            br#"{"bootstrap_token":"different-bootstrap-secret"}"#,
        );
    }

    #[test]
    fn deterministic_bootstrap_errors_do_not_retry_or_leak_the_token() {
        let cases = [
            ("400 Bad Request", StatusCode::BAD_REQUEST),
            ("401 Unauthorized", StatusCode::UNAUTHORIZED),
            ("409 Conflict", StatusCode::CONFLICT),
            ("422 Unprocessable Entity", StatusCode::UNPROCESSABLE_ENTITY),
        ];

        for (status_line, expected_status) in cases {
            let server = TestServer::start(
                vec![TestResponse::Json {
                    status: status_line,
                    body: Vec::new(),
                }],
                true,
            );
            let client = SandboxHttpClient::new(&server.api_url).expect("HTTP client");
            let token = SandboxBootstrapToken::new("bootstrap-secret").expect("bootstrap token");

            let error = client
                .exchange_bootstrap(&token)
                .expect_err("deterministic response must fail");

            assert!(matches!(
                &error,
                SandboxInitError::HttpStatus {
                    operation: "bootstrap exchange",
                    status
                } if *status == expected_status
            ));
            assert_eq!(
                error.to_string(),
                format!("bootstrap exchange returned HTTP {expected_status}")
            );
            assert!(!format!("{error:?}").contains("bootstrap-secret"));
            assert!(!error.to_string().contains("bootstrap-secret"));
            assert_eq!(format!("{token:?}"), "SandboxBootstrapToken(<redacted>)");
            let requests = server.finish();
            assert_eq!(requests.len(), 1);
            assert_bootstrap_request(&requests[0], br#"{"bootstrap_token":"bootstrap-secret"}"#);
        }
    }

    fn capability() -> SessionCapability {
        SessionCapability {
            session_id: SessionId::new("session-idempotent"),
            opaque_capability: "capability-secret".to_string(),
            expires_at: "2026-07-20T12:00:00Z".to_string(),
        }
    }

    fn assert_bootstrap_request(request: &CapturedRequest, expected_body: &[u8]) {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1/sessions");
        assert_eq!(
            request.headers.get("accept").map(String::as_str),
            Some(JSON_MEDIA_TYPE)
        );
        assert_eq!(
            request.headers.get("content-type").map(String::as_str),
            Some(JSON_MEDIA_TYPE)
        );
        assert_eq!(request.body, expected_body);
        assert_valid_idempotency_key(idempotency_key(request));
    }

    fn idempotency_key(request: &CapturedRequest) -> &str {
        request
            .headers
            .get("idempotency-key")
            .map(String::as_str)
            .expect("idempotency key header")
    }

    fn assert_valid_idempotency_key(key: &str) {
        assert_eq!(key.len(), 64);
        assert!(
            key.as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        );
    }

    fn accept_before(listener: &TcpListener, deadline: Instant) -> TcpStream {
        loop {
            match listener.accept() {
                Ok((stream, _)) => return stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "request timed out");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept request: {error}"),
            }
        }
    }

    fn read_request(stream: &mut TcpStream) -> CapturedRequest {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set request timeout");
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut chunk = [0_u8; 4096];
            let read = stream.read(&mut chunk).expect("read request");
            assert!(read > 0, "request ended before headers");
            bytes.extend_from_slice(&chunk[..read]);
            if let Some(position) = find_bytes(&bytes, b"\r\n\r\n") {
                break position + 4;
            }
        };
        let headers_text = std::str::from_utf8(&bytes[..header_end]).expect("headers UTF-8");
        let mut lines = headers_text.split("\r\n");
        let mut request_line = lines.next().expect("request line").split_whitespace();
        let method = request_line.next().expect("method").to_string();
        let path = request_line.next().expect("path").to_string();
        let mut headers = BTreeMap::new();
        for line in lines.filter(|line| !line.is_empty()) {
            let (name, value) = line.split_once(':').expect("header delimiter");
            headers.insert(name.to_ascii_lowercase(), value.trim().to_string());
        }
        let content_length = headers
            .get("content-length")
            .map(|value| value.parse::<usize>().expect("content length"))
            .unwrap_or(0);
        while bytes.len() < header_end + content_length {
            let mut chunk = [0_u8; 4096];
            let read = stream.read(&mut chunk).expect("read request body");
            assert!(read > 0, "request body ended early");
            bytes.extend_from_slice(&chunk[..read]);
        }
        CapturedRequest {
            method,
            path,
            headers,
            body: bytes[header_end..header_end + content_length].to_vec(),
        }
    }

    fn write_json_response(stream: &mut TcpStream, status: &str, body: &[u8]) {
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: {JSON_MEDIA_TYPE}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .expect("write response head");
        stream.write_all(body).expect("write response body");
        stream.flush().expect("flush response");
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }
}
