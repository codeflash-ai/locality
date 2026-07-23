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
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use locality_protocol::{
    ExportAttemptLimits, ExportAttemptRequest, OpaqueBootstrapExchangeRequest,
    SCOPE_AUTHORIZED_COMPONENT_VERSIONS, SandboxSessionState, SandboxSessionStatus,
    SealedExportOffer, SessionCapability, SessionErrorCode, TarContentEncoding, TarExportOffer,
};
use localityd::remote_truth::{ReplicaArchive, ReplicaArchiveEncoding};
use localityd::replica_materializer::{
    ExpectedReplicaMaterializationReceipt, ReplicaMaterializationLimits,
    ReplicaMaterializationSummary, materialize_replica_archive_with_expected_receipt,
    materialize_scope_authorized_replica_archive,
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
/// Reqwest's blocking response reapplies the client's operation timeout to
/// each `Read`. A dedicated export client therefore bounds idle body reads
/// without imposing a 60-second total limit on a progressing export.
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(60);
const BOOTSTRAP_EXCHANGE_ATTEMPTS: usize = 2;
const EXPORT_ATTEMPT_CREATION_ATTEMPTS: usize = 2;
const BOOTSTRAP_IDEMPOTENCY_DOMAIN: &[u8] = b"locality.session-exchange-idempotency.v1\0";
const IDEMPOTENCY_KEY_HEADER: &str = "Idempotency-Key";
const EXPORT_READ_AHEAD_CHUNK_BYTES: usize = 64 * 1024;
const EXPORT_READ_AHEAD_CHUNKS: usize = 8;
static REQWEST_CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();

pub(crate) const PROFILE_BOOTSTRAP_TOKEN_INPUT: &str = "bootstrap_token_input";
const PROFILE_CLIENT_SETUP: &str = "client_setup";
const PROFILE_BOOTSTRAP_EXCHANGE: &str = "bootstrap_exchange";
const PROFILE_SESSION_STATUS: &str = "session_status";
const PROFILE_EXPORT_OPEN_HEADERS: &str = "export_open_headers";
const PROFILE_FIRST_CONSUMER_BODY_BYTE: &str = "first_consumer_body_byte";
const PROFILE_STREAM_DECODE_MATERIALIZE: &str = "stream_decode_materialize";
pub(crate) const PROFILE_TOTAL: &str = "total";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SandboxProfileTiming {
    pub phase: &'static str,
    pub phase_ms: u128,
    pub total_ms: u128,
}

pub(crate) struct SandboxInitProfile {
    started: Instant,
    last_total_ms: u128,
    timings: Vec<SandboxProfileTiming>,
}

impl SandboxInitProfile {
    pub(crate) fn start() -> Self {
        Self {
            started: Instant::now(),
            last_total_ms: 0,
            timings: Vec::new(),
        }
    }

    pub(crate) fn mark(&mut self, phase: &'static str) {
        let total_ms = self.started.elapsed().as_millis();
        self.timings.push(SandboxProfileTiming {
            phase,
            phase_ms: total_ms.saturating_sub(self.last_total_ms),
            total_ms,
        });
        self.last_total_ms = total_ms;
    }

    pub(crate) fn timings(&self) -> &[SandboxProfileTiming] {
        &self.timings
    }
}

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

/// Controls HTTP content negotiation for a sandbox export.
///
/// [`Self::Automatic`] preserves the original preference for Zstd with an
/// identity fallback. The forced variants are intended for acceptance and
/// interoperability testing and fail closed if the server selects a different
/// encoding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SandboxContentEncodingPreference {
    #[default]
    Automatic,
    Identity,
    Zstd,
}

impl SandboxContentEncodingPreference {
    fn accept_encoding(self) -> &'static str {
        match self {
            Self::Automatic => "zstd, identity",
            Self::Identity => "identity",
            Self::Zstd => "zstd",
        }
    }

    fn required_encoding(self) -> Option<ReplicaArchiveEncoding> {
        match self {
            Self::Automatic => None,
            Self::Identity => Some(ReplicaArchiveEncoding::Identity),
            Self::Zstd => Some(ReplicaArchiveEncoding::Zstd),
        }
    }
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
    run_sandbox_init_with_encoding(
        options,
        bootstrap_token,
        SandboxContentEncodingPreference::Automatic,
    )
}

/// Initializes a sandbox with an explicit export content-negotiation policy.
///
/// The existing [`run_sandbox_init`] entry point remains automatic for source
/// compatibility. A forced preference is checked against the sealed offer
/// before the export request and against the response before any body bytes are
/// materialized.
///
/// # Errors
///
/// Returns [`SandboxInitError`] when validation, protocol exchange, content
/// negotiation, or atomic materialization fails.
pub fn run_sandbox_init_with_encoding(
    options: SandboxInitOptions,
    bootstrap_token: SandboxBootstrapToken,
    content_encoding: SandboxContentEncodingPreference,
) -> Result<SandboxInitReport, SandboxInitError> {
    run_sandbox_init_internal(options, bootstrap_token, content_encoding, None)
}

pub(crate) fn run_sandbox_init_with_encoding_and_profile(
    options: SandboxInitOptions,
    bootstrap_token: SandboxBootstrapToken,
    content_encoding: SandboxContentEncodingPreference,
    profile: &mut SandboxInitProfile,
) -> Result<SandboxInitReport, SandboxInitError> {
    run_sandbox_init_internal(options, bootstrap_token, content_encoding, Some(profile))
}

fn run_sandbox_init_internal(
    options: SandboxInitOptions,
    bootstrap_token: SandboxBootstrapToken,
    content_encoding: SandboxContentEncodingPreference,
    mut profile: Option<&mut SandboxInitProfile>,
) -> Result<SandboxInitReport, SandboxInitError> {
    let root = absolute_destination(&options.root)?;
    validate_destination(&root)?;
    let client = SandboxHttpClient::new(&options.api_url)?;
    mark_profile(&mut profile, PROFILE_CLIENT_SETUP);

    let capability = client.exchange_bootstrap(&bootstrap_token)?;
    mark_profile(&mut profile, PROFILE_BOOTSTRAP_EXCHANGE);
    validate_capability(&capability)?;
    let status = client.session_status(&capability)?;
    mark_profile(&mut profile, PROFILE_SESSION_STATUS);
    let session = validate_status(&capability, &status)?;
    let (encoding, response, limits, validation) = match session {
        ValidatedSandboxSession::Legacy {
            offer,
            expected_receipt,
        } => {
            validate_encoding_preference(offer, content_encoding)?;
            let limits = limits_for_offer(offer)?;
            let (encoding, response) = client.open_export(&capability, offer, content_encoding)?;
            (
                encoding,
                response,
                limits,
                ExportValidation::Legacy(expected_receipt),
            )
        }
        ValidatedSandboxSession::ScopeAuthorized => {
            let request = export_attempt_request(&capability, content_encoding)?;
            let offer = client.create_export_attempt(&capability, &request)?;
            validate_scope_offer(&capability, &request, &offer)?;
            let limits = limits_for_scope_offer(&offer)?;
            let (encoding, response) = client.open_export_attempt(&capability, &offer)?;
            (
                encoding,
                response,
                limits,
                ExportValidation::ScopeAuthorized(offer),
            )
        }
    };
    mark_profile(&mut profile, PROFILE_EXPORT_OPEN_HEADERS);
    let (body, mut producer) =
        spawn_export_read_ahead(response).map_err(|error| SandboxInitError::Http {
            operation: "session export read-ahead setup",
            detail: error.to_string(),
        })?;
    let profiled_body = ProfiledExportBody::new(body, profile.as_deref_mut());
    let archive = ReplicaArchive::new(encoding, profiled_body);
    let materialization = match validation {
        ExportValidation::Legacy(expected_receipt) => {
            materialize_replica_archive_with_expected_receipt(
                archive,
                &root,
                limits,
                expected_receipt,
            )
        }
        ExportValidation::ScopeAuthorized(offer) => {
            materialize_scope_authorized_replica_archive(archive, &root, limits, &offer)
        }
    }
    .map_err(|error| SandboxInitError::Materialization(error.to_string()));
    let producer_outcome = producer.join();
    mark_profile(&mut profile, PROFILE_STREAM_DECODE_MATERIALIZE);

    let summary = match materialization {
        Err(error) => return Err(error),
        Ok(summary) => {
            match producer_outcome {
                Ok(ReadAheadProducerOutcome::CleanEof) => {}
                Ok(
                    ReadAheadProducerOutcome::ConsumerClosed
                    | ReadAheadProducerOutcome::ErrorDelivered,
                ) => {
                    return Err(SandboxInitError::Materialization(
                        "sandbox export transport ended without a clean EOF".to_string(),
                    ));
                }
                Err(()) => {
                    return Err(SandboxInitError::Materialization(
                        "sandbox export read-ahead worker panicked".to_string(),
                    ));
                }
            }
            summary
        }
    };

    Ok(report(&root, &capability, encoding, summary))
}

fn mark_profile(profile: &mut Option<&mut SandboxInitProfile>, phase: &'static str) {
    if let Some(profile) = profile.as_deref_mut() {
        profile.mark(phase);
    }
}

struct ProfiledExportBody<'a, Body> {
    body: Body,
    profile: Option<&'a mut SandboxInitProfile>,
    observed_first_byte: bool,
}

impl<'a, Body> ProfiledExportBody<'a, Body> {
    fn new(body: Body, profile: Option<&'a mut SandboxInitProfile>) -> Self {
        Self {
            body,
            profile,
            observed_first_byte: false,
        }
    }
}

impl<Body: Read> Read for ProfiledExportBody<'_, Body> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let read = self.body.read(output)?;
        if read != 0 && !self.observed_first_byte {
            self.observed_first_byte = true;
            if let Some(profile) = self.profile.as_deref_mut() {
                profile.mark(PROFILE_FIRST_CONSUMER_BODY_BYTE);
            }
        }
        Ok(read)
    }
}

enum ReadAheadMessage {
    Data(Vec<u8>),
    Error(io::Error),
    CleanEof,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadAheadProducerOutcome {
    CleanEof,
    ConsumerClosed,
    ErrorDelivered,
}

struct ExportReadAhead {
    receiver: Receiver<ReadAheadMessage>,
    recycle: SyncSender<Vec<u8>>,
    current: Option<Vec<u8>>,
    offset: usize,
    clean_eof: bool,
}

impl Read for ExportReadAhead {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.clean_eof {
            return Ok(0);
        }

        loop {
            if self
                .current
                .as_ref()
                .is_some_and(|current| self.offset < current.len())
            {
                let available = &self.current.as_ref().expect("current chunk")[self.offset..];
                let copied = available.len().min(output.len());
                output[..copied].copy_from_slice(&available[..copied]);
                self.offset += copied;
                return Ok(copied);
            }
            if let Some(mut exhausted) = self.current.take() {
                exhausted.clear();
                let _ = self.recycle.send(exhausted);
            }

            match self.receiver.recv() {
                Ok(ReadAheadMessage::Data(chunk)) => {
                    self.current = Some(chunk);
                    self.offset = 0;
                }
                Ok(ReadAheadMessage::Error(error)) => return Err(error),
                Ok(ReadAheadMessage::CleanEof) => {
                    self.clean_eof = true;
                    return Ok(0);
                }
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "sandbox export read-ahead producer stopped before EOF",
                    ));
                }
            }
        }
    }
}

struct ReadAheadProducer {
    handle: Option<JoinHandle<ReadAheadProducerOutcome>>,
}

impl ReadAheadProducer {
    fn join(&mut self) -> Result<ReadAheadProducerOutcome, ()> {
        let handle = self.handle.take().ok_or(())?;
        handle.join().map_err(|_| ())
    }
}

impl Drop for ReadAheadProducer {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn spawn_export_read_ahead<Body>(body: Body) -> io::Result<(ExportReadAhead, ReadAheadProducer)>
where
    Body: Read + Send + 'static,
{
    let (sender, receiver) = sync_channel(EXPORT_READ_AHEAD_CHUNKS);
    let (recycle, buffers) = sync_channel(EXPORT_READ_AHEAD_CHUNKS);
    for _ in 0..EXPORT_READ_AHEAD_CHUNKS {
        recycle
            .send(Vec::with_capacity(EXPORT_READ_AHEAD_CHUNK_BYTES))
            .expect("new buffer pool accepts its fixed capacity");
    }
    let handle = thread::Builder::new()
        .name("locality-export-read-ahead".to_string())
        .spawn(move || produce_export(body, &sender, &buffers))?;
    Ok((
        ExportReadAhead {
            receiver,
            recycle,
            current: None,
            offset: 0,
            clean_eof: false,
        },
        ReadAheadProducer {
            handle: Some(handle),
        },
    ))
}

fn produce_export<Body: Read>(
    mut body: Body,
    sender: &SyncSender<ReadAheadMessage>,
    buffers: &Receiver<Vec<u8>>,
) -> ReadAheadProducerOutcome {
    loop {
        let Ok(mut chunk) = buffers.recv() else {
            return ReadAheadProducerOutcome::ConsumerClosed;
        };
        chunk.resize(EXPORT_READ_AHEAD_CHUNK_BYTES, 0);
        match body.read(&mut chunk) {
            Ok(0) => {
                return if sender.send(ReadAheadMessage::CleanEof).is_ok() {
                    ReadAheadProducerOutcome::CleanEof
                } else {
                    ReadAheadProducerOutcome::ConsumerClosed
                };
            }
            Ok(read) => {
                chunk.truncate(read);
                if sender.send(ReadAheadMessage::Data(chunk)).is_err() {
                    return ReadAheadProducerOutcome::ConsumerClosed;
                }
            }
            Err(error) => {
                let redacted = io::Error::new(error.kind(), "sandbox export transport read failed");
                return if sender.send(ReadAheadMessage::Error(redacted)).is_ok() {
                    ReadAheadProducerOutcome::ErrorDelivered
                } else {
                    ReadAheadProducerOutcome::ConsumerClosed
                };
            }
        }
    }
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

enum ValidatedSandboxSession<'a> {
    Legacy {
        offer: &'a TarExportOffer,
        expected_receipt: ExpectedReplicaMaterializationReceipt,
    },
    ScopeAuthorized,
}

enum ExportValidation {
    Legacy(ExpectedReplicaMaterializationReceipt),
    ScopeAuthorized(SealedExportOffer),
}

fn validate_status<'a>(
    capability: &SessionCapability,
    status: &'a SandboxSessionStatus,
) -> Result<ValidatedSandboxSession<'a>, SandboxInitError> {
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
    if status.versions.session >= 2 {
        if status.versions.replica < 2 || status.versions.export_metadata < 2 {
            return Err(SandboxInitError::ComponentVersion(
                "scope-authorized session requires replica and export-metadata version 2"
                    .to_string(),
            ));
        }
        if status.export_offer.is_some() {
            return Err(SandboxInitError::InvalidReadySession(
                "scope-authorized status contains a legacy export offer",
            ));
        }
        return Ok(ValidatedSandboxSession::ScopeAuthorized);
    }
    let offer = status
        .export_offer
        .as_ref()
        .ok_or(SandboxInitError::InvalidReadySession(
            "export offer is missing",
        ))?;
    let expected_receipt = validate_offer(offer)?;
    Ok(ValidatedSandboxSession::Legacy {
        offer,
        expected_receipt,
    })
}

fn export_attempt_request(
    capability: &SessionCapability,
    preference: SandboxContentEncodingPreference,
) -> Result<ExportAttemptRequest, SandboxInitError> {
    let defaults = ReplicaMaterializationLimits::default();
    let request = ExportAttemptRequest {
        versions: SCOPE_AUTHORIZED_COMPONENT_VERSIONS,
        opaque_session_capability: capability.opaque_capability.clone(),
        idempotency_key: random_export_idempotency_key()?,
        content_encoding: match preference {
            SandboxContentEncodingPreference::Automatic
            | SandboxContentEncodingPreference::Zstd => TarContentEncoding::Zstd,
            SandboxContentEncodingPreference::Identity => TarContentEncoding::Identity,
        },
        limits: ExportAttemptLimits {
            max_files: defaults.max_entries.saturating_sub(1),
            max_directories: defaults.max_entries.saturating_sub(1),
            max_content_bytes: defaults.max_disk_bytes,
        },
    };
    request.validate().map_err(|_| {
        SandboxInitError::InvalidExportOffer("client export-attempt request is invalid")
    })?;
    Ok(request)
}

fn random_export_idempotency_key() -> Result<String, SandboxInitError> {
    let mut random = [0_u8; 32];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut random)
        .map_err(|_| SandboxInitError::Http {
            operation: "export-attempt idempotency-key generation",
            detail: "secure randomness is unavailable".to_string(),
        })?;
    Ok(format!("loc-export-v2-{}", lower_hex(&random)))
}

fn validate_scope_offer(
    capability: &SessionCapability,
    request: &ExportAttemptRequest,
    offer: &SealedExportOffer,
) -> Result<(), SandboxInitError> {
    offer
        .versions
        .validate_required()
        .map_err(|error| SandboxInitError::ComponentVersion(error.to_string()))?;
    offer
        .validate()
        .map_err(|_| SandboxInitError::InvalidExportOffer("scope-authorized offer is invalid"))?;
    if offer.session_id != capability.session_id {
        return Err(SandboxInitError::SessionIdMismatch);
    }
    if offer.media_type != TAR_MEDIA_TYPE {
        return Err(SandboxInitError::InvalidExportOffer(
            "media type must be application/x-tar",
        ));
    }
    if offer.content_encoding != request.content_encoding {
        return Err(SandboxInitError::InvalidExportOffer(
            "content encoding does not match the export-attempt request",
        ));
    }
    if offer.limits != request.limits {
        return Err(SandboxInitError::InvalidExportOffer(
            "limits do not match the export-attempt request",
        ));
    }
    Ok(())
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

fn limits_for_scope_offer(
    offer: &SealedExportOffer,
) -> Result<ReplicaMaterializationLimits, SandboxInitError> {
    let defaults = ReplicaMaterializationLimits::default();
    if offer.archive_entry_count > defaults.max_entries {
        return Err(SandboxInitError::ExportLimit {
            limit: "entry count",
            offered: offer.archive_entry_count,
            maximum: defaults.max_entries,
        });
    }
    if offer.selected_content_bytes > defaults.max_disk_bytes {
        return Err(SandboxInitError::ExportLimit {
            limit: "content bytes",
            offered: offer.selected_content_bytes,
            maximum: defaults.max_disk_bytes,
        });
    }
    Ok(ReplicaMaterializationLimits {
        max_entries: offer.archive_entry_count,
        max_disk_bytes: offer.selected_content_bytes,
        ..defaults
    })
}

fn validate_encoding_preference(
    offer: &TarExportOffer,
    preference: SandboxContentEncodingPreference,
) -> Result<(), SandboxInitError> {
    let Some(required) = preference.required_encoding() else {
        return Ok(());
    };
    if offer
        .supported_content_encodings
        .contains(&protocol_encoding(required))
    {
        Ok(())
    } else {
        Err(SandboxInitError::UnsupportedExportEncoding(
            encoding_name(required).to_string(),
        ))
    }
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
    export_client: Client,
    api_url: reqwest::Url,
}

impl SandboxHttpClient {
    fn new(api_url: &str) -> Result<Self, SandboxInitError> {
        Self::new_with_read_timeout(api_url, HTTP_READ_TIMEOUT)
    }

    fn new_with_read_timeout(
        api_url: &str,
        read_timeout: Duration,
    ) -> Result<Self, SandboxInitError> {
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
        let export_client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .timeout(read_timeout)
            .build()
            .map_err(|error| SandboxInitError::Http {
                operation: "HTTP client setup",
                detail: error.without_url().to_string(),
            })?;
        Ok(Self {
            client,
            export_client,
            api_url,
        })
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
                    if has_retry_remaining(attempt, BOOTSTRAP_EXCHANGE_ATTEMPTS) {
                        continue;
                    }
                    return Err(error);
                }
            };

            if is_retriable_idempotent_status(response.status())
                && has_retry_remaining(attempt, BOOTSTRAP_EXCHANGE_ATTEMPTS)
            {
                continue;
            }
            match read_json_response(response, "bootstrap exchange") {
                Ok(capability) => return Ok(capability),
                Err(error)
                    if has_retry_remaining(attempt, BOOTSTRAP_EXCHANGE_ATTEMPTS)
                        && is_ambiguous_idempotent_error(&error, "bootstrap exchange") =>
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
        preference: SandboxContentEncodingPreference,
    ) -> Result<(ReplicaArchiveEncoding, Response), SandboxInitError> {
        let response = self
            .export_client
            .get(self.export_url(capability.session_id.as_str()))
            .header(ACCEPT, TAR_MEDIA_TYPE)
            .header(ACCEPT_ENCODING, preference.accept_encoding())
            .bearer_auth(&capability.opaque_capability)
            .send()
            .map_err(|error| SandboxInitError::Http {
                operation: "session export",
                detail: error.without_url().to_string(),
            })?;
        ensure_success(&response, "session export")?;
        require_media_type(response.headers(), "session export", TAR_MEDIA_TYPE)?;
        let encoding = response_encoding(response.headers())?;
        let offered = protocol_encoding(encoding);
        if !offer.supported_content_encodings.contains(&offered) {
            return Err(SandboxInitError::UnsupportedExportEncoding(
                encoding_name(encoding).to_string(),
            ));
        }
        if let Some(required) = preference.required_encoding()
            && encoding != required
        {
            return Err(SandboxInitError::UnsupportedExportEncoding(format!(
                "{} (requested {})",
                encoding_name(encoding),
                encoding_name(required)
            )));
        }
        Ok((encoding, response))
    }

    fn create_export_attempt(
        &self,
        capability: &SessionCapability,
        request: &ExportAttemptRequest,
    ) -> Result<SealedExportOffer, SandboxInitError> {
        for attempt in 0..EXPORT_ATTEMPT_CREATION_ATTEMPTS {
            let response = match self
                .client
                .post(self.export_attempts_url(capability.session_id.as_str()))
                .header(ACCEPT, JSON_MEDIA_TYPE)
                .bearer_auth(&capability.opaque_capability)
                .json(request)
                .send()
            {
                Ok(response) => response,
                Err(error) => {
                    let error = SandboxInitError::Http {
                        operation: "export-attempt creation",
                        detail: error.without_url().to_string(),
                    };
                    if has_retry_remaining(attempt, EXPORT_ATTEMPT_CREATION_ATTEMPTS) {
                        continue;
                    }
                    return Err(error);
                }
            };

            if is_retriable_idempotent_status(response.status())
                && has_retry_remaining(attempt, EXPORT_ATTEMPT_CREATION_ATTEMPTS)
            {
                continue;
            }
            match read_json_response(response, "export-attempt creation") {
                Ok(offer) => return Ok(offer),
                Err(error)
                    if has_retry_remaining(attempt, EXPORT_ATTEMPT_CREATION_ATTEMPTS)
                        && is_ambiguous_idempotent_error(&error, "export-attempt creation") =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }

        unreachable!("export-attempt creation loop always returns")
    }

    fn open_export_attempt(
        &self,
        capability: &SessionCapability,
        offer: &SealedExportOffer,
    ) -> Result<(ReplicaArchiveEncoding, Response), SandboxInitError> {
        let response = self
            .export_client
            .get(self.export_attempt_url(
                capability.session_id.as_str(),
                offer.export_attempt_id.as_str(),
            ))
            .header(ACCEPT, TAR_MEDIA_TYPE)
            .header(
                ACCEPT_ENCODING,
                match offer.content_encoding {
                    TarContentEncoding::Identity => "identity",
                    TarContentEncoding::Zstd => "zstd",
                },
            )
            .bearer_auth(&capability.opaque_capability)
            .send()
            .map_err(|error| SandboxInitError::Http {
                operation: "export-attempt stream",
                detail: error.without_url().to_string(),
            })?;
        ensure_success(&response, "export-attempt stream")?;
        require_media_type(response.headers(), "export-attempt stream", TAR_MEDIA_TYPE)?;
        let encoding = response_encoding(response.headers())?;
        if protocol_encoding(encoding) != offer.content_encoding {
            return Err(SandboxInitError::UnsupportedExportEncoding(format!(
                "{} (sealed {})",
                encoding_name(encoding),
                match offer.content_encoding {
                    TarContentEncoding::Identity => "identity",
                    TarContentEncoding::Zstd => "zstd",
                }
            )));
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

    fn export_attempts_url(&self, session_id: &str) -> reqwest::Url {
        endpoint_url(
            &self.api_url,
            &["v1", "sessions", session_id, "export-attempts"],
        )
    }

    fn export_attempt_url(&self, session_id: &str, attempt_id: &str) -> reqwest::Url {
        endpoint_url(
            &self.api_url,
            &[
                "v1",
                "sessions",
                session_id,
                "export-attempts",
                attempt_id,
                "export",
            ],
        )
    }
}

fn derive_idempotency_key(token: &SandboxBootstrapToken) -> String {
    let mut hasher = Sha256::new();
    hasher.update(BOOTSTRAP_IDEMPOTENCY_DOMAIN);
    hasher.update(token.expose().as_bytes());
    let digest = hasher.finalize();
    lower_hex(&digest)
}

fn lower_hex(bytes: &[u8]) -> String {
    let mut encoded = vec![0_u8; bytes.len() * 2];
    const LOWER_HEX: &[u8; 16] = b"0123456789abcdef";
    for (index, byte) in bytes.iter().copied().enumerate() {
        encoded[index * 2] = LOWER_HEX[usize::from(byte >> 4)];
        encoded[index * 2 + 1] = LOWER_HEX[usize::from(byte & 0x0f)];
    }
    String::from_utf8(encoded).expect("lowercase hexadecimal is valid UTF-8")
}

fn has_retry_remaining(attempt: usize, max_attempts: usize) -> bool {
    attempt + 1 < max_attempts
}

fn is_retriable_idempotent_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT
    )
}

fn is_ambiguous_idempotent_error(error: &SandboxInitError, operation: &'static str) -> bool {
    matches!(
        error,
        SandboxInitError::Http {
            operation: actual_operation,
            ..
        } if *actual_operation == operation
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

fn protocol_encoding(encoding: ReplicaArchiveEncoding) -> TarContentEncoding {
    match encoding {
        ReplicaArchiveEncoding::Identity => TarContentEncoding::Identity,
        ReplicaArchiveEncoding::Zstd => TarContentEncoding::Zstd,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{self, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Receiver};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use locality_core::portable::SessionId;

    use super::*;

    struct FixedChunkBody {
        reads: Arc<AtomicUsize>,
    }

    impl Read for FixedChunkBody {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            output.fill(0x5a);
            Ok(output.len())
        }
    }

    struct FailingBody {
        first_read: bool,
    }

    impl Read for FailingBody {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if self.first_read {
                self.first_read = false;
                output[..3].copy_from_slice(b"abc");
                Ok(3)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "sentinel export transport failure",
                ))
            }
        }
    }

    #[derive(Debug)]
    struct CapturedRequest {
        method: String,
        path: String,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
    }

    enum TestResponse {
        DropConnection,
        Json {
            status: &'static str,
            body: Vec<u8>,
        },
        StalledExport {
            prefix: Vec<u8>,
            stall: Duration,
        },
        ProgressingExport {
            chunks: Vec<Vec<u8>>,
            pause: Duration,
        },
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
                        TestResponse::StalledExport { prefix, stall } => {
                            write!(
                                stream,
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/x-tar\r\nContent-Encoding: identity\r\nConnection: close\r\n\r\n",
                                prefix.len() + 512
                            )
                            .expect("write stalled response head");
                            stream
                                .write_all(&prefix)
                                .expect("write stalled response prefix");
                            stream.flush().expect("flush stalled response prefix");
                            thread::sleep(stall);
                        }
                        TestResponse::ProgressingExport { chunks, pause } => {
                            let content_length = chunks.iter().map(Vec::len).sum::<usize>();
                            write!(
                                stream,
                                "HTTP/1.1 200 OK\r\nContent-Length: {content_length}\r\nContent-Type: application/x-tar\r\nContent-Encoding: identity\r\nConnection: close\r\n\r\n"
                            )
                            .expect("write progressing response head");
                            for (index, chunk) in chunks.into_iter().enumerate() {
                                if index != 0 {
                                    thread::sleep(pause);
                                }
                                stream
                                    .write_all(&chunk)
                                    .expect("write progressing response chunk");
                                stream.flush().expect("flush progressing response chunk");
                            }
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
    fn export_read_ahead_is_byte_bounded_and_consumer_drop_unblocks_producer() {
        let reads = Arc::new(AtomicUsize::new(0));
        let (reader, mut producer) = spawn_export_read_ahead(FixedChunkBody {
            reads: Arc::clone(&reads),
        })
        .expect("start read-ahead producer");
        let expected_reads = EXPORT_READ_AHEAD_CHUNKS;
        let deadline = Instant::now() + Duration::from_secs(2);
        while reads.load(Ordering::SeqCst) < expected_reads {
            assert!(
                Instant::now() < deadline,
                "producer did not fill bounded queue"
            );
            thread::yield_now();
        }
        thread::sleep(Duration::from_millis(25));
        assert_eq!(
            reads.load(Ordering::SeqCst),
            expected_reads,
            "the producer is bounded by exactly eight reusable 64 KiB buffers"
        );

        drop(reader);
        assert_eq!(
            producer.join(),
            Ok(ReadAheadProducerOutcome::ConsumerClosed),
            "dropping a rejecting consumer must promptly release a blocked producer"
        );
    }

    #[test]
    fn export_read_ahead_redacts_the_original_io_error() {
        let (mut reader, mut producer) = spawn_export_read_ahead(FailingBody { first_read: true })
            .expect("start read-ahead producer");
        let mut prefix = [0_u8; 3];
        reader.read_exact(&mut prefix).expect("read prefix");
        assert_eq!(&prefix, b"abc");

        let error = reader.read(&mut [0_u8; 1]).expect_err("transport fails");
        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
        assert_eq!(error.to_string(), "sandbox export transport read failed");
        assert!(!error.to_string().contains("sentinel"));
        drop(reader);
        assert_eq!(
            producer.join(),
            Ok(ReadAheadProducerOutcome::ErrorDelivered)
        );
    }

    #[test]
    fn export_read_ahead_disconnect_is_not_mistaken_for_clean_eof() {
        let (sender, receiver) = sync_channel(1);
        let (recycle, _) = sync_channel(1);
        drop(sender);
        let mut reader = ExportReadAhead {
            receiver,
            recycle,
            current: None,
            offset: 0,
            clean_eof: false,
        };

        let error = reader
            .read(&mut [0_u8; 1])
            .expect_err("disconnect without an EOF marker must fail");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(
            error.to_string(),
            "sandbox export read-ahead producer stopped before EOF"
        );
    }

    #[test]
    fn early_materializer_rejection_joins_a_producer_blocked_on_http_read() {
        static DIRECTORY_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

        let server = TestServer::start(
            vec![TestResponse::StalledExport {
                prefix: vec![0xff; 512],
                stall: Duration::from_millis(500),
            }],
            false,
        );
        let client =
            SandboxHttpClient::new_with_read_timeout(&server.api_url, Duration::from_millis(100))
                .expect("HTTP client");
        let response = client
            .export_client
            .get(endpoint_url(&client.api_url, &["stalled-export"]))
            .send()
            .expect("open stalled response");
        let (body, mut producer) = spawn_export_read_ahead(response).expect("start producer");
        let parent = std::env::temp_dir().join(format!(
            "locality-stalled-export-{}-{}",
            std::process::id(),
            DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&parent).expect("create test parent");
        let destination = parent.join("tree");

        let archive = ReplicaArchive::new(ReplicaArchiveEncoding::Identity, body);
        localityd::replica_materializer::materialize_replica_archive(
            archive,
            &destination,
            ReplicaMaterializationLimits::default(),
        )
        .expect_err("invalid first header rejects before HTTP EOF");
        let join_started = Instant::now();
        assert_eq!(
            producer.join(),
            Ok(ReadAheadProducerOutcome::ConsumerClosed)
        );
        assert!(
            join_started.elapsed() >= Duration::from_millis(20),
            "producer was not blocked in the stalled response read"
        );
        assert!(
            join_started.elapsed() < Duration::from_secs(1),
            "blocked response read exceeded its configured deadline"
        );
        assert!(!destination.exists());
        fs::remove_dir_all(&parent).expect("remove test parent");
        server.finish();
    }

    #[test]
    fn export_read_deadline_resets_for_a_progressing_multi_read_response() {
        let chunks = vec![vec![1; 17], vec![2; 19], vec![3; 23]];
        let expected = chunks.iter().flatten().copied().collect::<Vec<_>>();
        let server = TestServer::start(
            vec![TestResponse::ProgressingExport {
                chunks,
                pause: Duration::from_millis(120),
            }],
            false,
        );
        let client =
            SandboxHttpClient::new_with_read_timeout(&server.api_url, Duration::from_millis(200))
                .expect("HTTP client");
        let response = client
            .export_client
            .get(endpoint_url(&client.api_url, &["progressing-export"]))
            .send()
            .expect("open progressing response");
        let started = Instant::now();
        let (mut body, mut producer) = spawn_export_read_ahead(response).expect("start producer");
        let mut actual = Vec::new();
        body.read_to_end(&mut actual)
            .expect("read progressing body");
        assert_eq!(producer.join(), Ok(ReadAheadProducerOutcome::CleanEof));
        assert_eq!(actual, expected);
        assert!(
            started.elapsed() > Duration::from_millis(200),
            "fixture must exceed one read deadline in total"
        );
        server.finish();
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
    fn dropped_export_attempt_response_retries_the_exact_sealed_request() {
        let offer = scope_offer_fixture();
        let response = serde_json::to_vec(&offer).expect("serialize scope offer");
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
        let request = scope_attempt_request_fixture();

        assert_eq!(
            client
                .create_export_attempt(&capability(), &request)
                .expect("retry export-attempt creation"),
            offer
        );

        let requests = server.finish();
        assert_eq!(requests.len(), EXPORT_ATTEMPT_CREATION_ATTEMPTS);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(
            requests[0].path,
            "/v1/sessions/session-idempotent/export-attempts"
        );
        assert_eq!(requests[0].body, requests[1].body);
        assert_eq!(
            serde_json::from_slice::<ExportAttemptRequest>(&requests[0].body)
                .expect("decode captured request")
                .idempotency_key,
            request.idempotency_key
        );
    }

    #[test]
    fn export_attempt_gateway_retry_is_bounded_and_reuses_the_request() {
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
        let request = scope_attempt_request_fixture();

        let error = client
            .create_export_attempt(&capability(), &request)
            .expect_err("repeated service failure must stop");

        assert!(matches!(
            error,
            SandboxInitError::HttpStatus {
                operation: "export-attempt creation",
                status: StatusCode::SERVICE_UNAVAILABLE,
            }
        ));
        let requests = server.finish();
        assert_eq!(requests.len(), EXPORT_ATTEMPT_CREATION_ATTEMPTS);
        assert_eq!(requests[0].body, requests[1].body);
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

    fn scope_attempt_request_fixture() -> ExportAttemptRequest {
        serde_json::from_str(include_str!(
            "../../locality-protocol/fixtures/export-attempt-request.json"
        ))
        .expect("scope export-attempt request fixture")
    }

    fn scope_offer_fixture() -> SealedExportOffer {
        serde_json::from_str(include_str!(
            "../../locality-protocol/fixtures/sealed-export-offer.json"
        ))
        .expect("sealed scope export offer fixture")
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
        // Accepted sockets may inherit the listener's nonblocking mode on some
        // platforms. Normalize both test-server accept paths before reading.
        stream
            .set_nonblocking(false)
            .expect("set request stream blocking");
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
