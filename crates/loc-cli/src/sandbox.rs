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

use chrono::{DateTime, NaiveDateTime};
use locality_protocol::freshness_wait::{
    FRESHNESS_WAIT_FORMAT_VERSION, FRESHNESS_WAIT_READER_VERSION, FreshnessWaitAggregateState,
    FreshnessWaitAttempt, FreshnessWaitAttemptRequest, MAX_FRESHNESS_WAIT_ATTEMPT_BYTES,
};
use locality_protocol::workspace_api_v2::{
    WORKSPACE_HTTP_API_GENERATION_V2, WorkspaceClientCapabilitiesV2, WorkspaceExportOfferV2,
    WorkspaceProfileSessionRequestV2, WorkspaceProfileSessionV2, WorkspaceSessionStatusV2,
};
use locality_protocol::{
    ExportAttemptLimits, ExportAttemptRequest, FreshnessRequirement,
    OpaqueBootstrapExchangeRequest, SCOPE_AUTHORIZED_COMPONENT_VERSIONS, SandboxSessionState,
    SandboxSessionStatus, SealedExportOffer, SessionCapability, SessionErrorCode,
    TarContentEncoding, TarExportOffer, WorkspaceProfileSession,
};
use locality_store::{SqliteStateStore, WorkspaceHostBindingError, WorkspaceHostBindingResolver};
use localityd::remote_truth::{ReplicaArchive, ReplicaArchiveEncoding};
use localityd::replica_materializer::{
    ExpectedReplicaMaterializationReceipt, ReplicaMaterializationError,
    ReplicaMaterializationLimits, ReplicaMaterializationSummary,
    materialize_replica_archive_with_expected_receipt_and_prepublication_check,
    materialize_scope_authorized_replica_archive_with_prepublication_check,
};
use localityd::workspace_archive::WorkspaceArchiveLimits;
use localityd::workspace_materializer::{
    PublishedWorkspace, WorkspaceMaterializationError, WorkspaceMaterializationLimits,
    WorkspaceOwnershipCapability, WorkspacePublicationCheckpoint, WorkspacePublicationHooks,
    materialize_workspace_archive_durable_with_hooks,
    recover_and_verify_workspace_publication_state, recover_workspace_publication,
};
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_TYPE, DATE, HeaderMap};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

const JSON_MEDIA_TYPE: &str = "application/json";
const TAR_MEDIA_TYPE: &str = "application/x-tar";
const SANDBOX_USER_AGENT: &str = concat!("locality-loc/", env!("CARGO_PKG_VERSION"));
const MAX_JSON_RESPONSE_BYTES: u64 = 1024 * 1024;
const MAX_SESSION_CREDENTIAL_BYTES: u64 = 16 * 1024;
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Reqwest's blocking response reapplies the client's operation timeout to
/// each `Read`. A dedicated export client therefore bounds idle body reads
/// without imposing a 60-second total limit on a progressing export.
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(60);
const BOOTSTRAP_EXCHANGE_ATTEMPTS: usize = 2;
const EXPORT_ATTEMPT_CREATION_ATTEMPTS: usize = 2;
const EXPORT_ATTEMPT_STREAM_ATTEMPTS: usize = 2;
const FRESHNESS_WAIT_REQUEST_ATTEMPTS: usize = 2;
const FRESHNESS_WAIT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const BOOTSTRAP_IDEMPOTENCY_DOMAIN: &[u8] = b"locality.session-exchange-idempotency.v1\0";
const IDEMPOTENCY_KEY_HEADER: &str = "Idempotency-Key";
const EXPORT_READ_AHEAD_CHUNK_BYTES: usize = 64 * 1024;
const EXPORT_READ_AHEAD_CHUNKS: usize = 8;
static REQWEST_CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();

pub(crate) const PROFILE_BOOTSTRAP_TOKEN_INPUT: &str = "bootstrap_token_input";
const PROFILE_CLIENT_SETUP: &str = "client_setup";
const PROFILE_BOOTSTRAP_EXCHANGE: &str = "bootstrap_exchange";
const PROFILE_SESSION_STATUS: &str = "session_status";
const PROFILE_FRESHNESS_WAIT: &str = "freshness_wait";
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
    export_transport_wait: Duration,
    export_transport_read_calls: u64,
    export_transport_wire_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SandboxExportTransportMetrics {
    pub wait_ms: u128,
    pub read_calls: u64,
    pub wire_bytes: u64,
}

impl SandboxInitProfile {
    pub(crate) fn start() -> Self {
        Self {
            started: Instant::now(),
            last_total_ms: 0,
            timings: Vec::new(),
            export_transport_wait: Duration::ZERO,
            export_transport_read_calls: 0,
            export_transport_wire_bytes: 0,
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

    fn observe_export_transport_read(&mut self, wait: Duration, bytes: usize) {
        self.export_transport_wait = self.export_transport_wait.saturating_add(wait);
        self.export_transport_read_calls = self.export_transport_read_calls.saturating_add(1);
        self.export_transport_wire_bytes = self
            .export_transport_wire_bytes
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
    }

    pub(crate) fn export_transport_metrics(&self) -> SandboxExportTransportMetrics {
        SandboxExportTransportMetrics {
            wait_ms: self.export_transport_wait.as_millis(),
            read_calls: self.export_transport_read_calls,
            wire_bytes: self.export_transport_wire_bytes,
        }
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

#[derive(Clone)]
pub struct SandboxProfileKey(String);

impl SandboxProfileKey {
    pub fn new(value: impl Into<String>) -> Result<Self, SandboxInitError> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(SandboxInitError::InvalidProfileKey);
        }
        Ok(Self(value))
    }

    fn expose(&self) -> &str {
        &self.0
    }

    fn ownership_capability(&self) -> WorkspaceOwnershipCapability {
        let mut secret = [0_u8; 32];
        for (output, pair) in secret.iter_mut().zip(self.0.as_bytes().chunks_exact(2)) {
            *output = (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]);
        }
        WorkspaceOwnershipCapability::new(secret)
    }
}

impl Debug for SandboxProfileKey {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SandboxProfileKey(<redacted>)")
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

/// Bind the historical sandbox destination as one whole ephemeral root while
/// proving that it does not overlap any configured persistent mount root.
///
/// Both the CLI and Desktop call this boundary before network or filesystem
/// publication. Workspace targets are intentionally not appended to `--root`.
pub fn resolve_sandbox_init_options_at_state_root(
    mut options: SandboxInitOptions,
    state_root: &Path,
) -> Result<SandboxInitOptions, SandboxInitError> {
    let root = absolute_destination(&options.root)?;
    revalidate_sandbox_publication_at_state_root(&root, state_root)?;
    options.root = root;
    Ok(options)
}

/// Re-read mount roots without schema migration and reject lexical or
/// filesystem-alias overlap on the running host.
pub fn revalidate_sandbox_publication_at_state_root(
    root: &Path,
    state_root: &Path,
) -> Result<(), SandboxInitError> {
    let active_mounts = SqliteStateStore::inspect_mount_roots_read_only(state_root)
        .map_err(|error| SandboxInitError::WorkspaceState(error.to_string()))?;
    WorkspaceHostBindingResolver::current()
        .resolve_ephemeral_publication_root_on_current_host(root, &active_mounts)
        .map_err(SandboxInitError::HostBinding)?;
    Ok(())
}

#[derive(Clone, Debug)]
struct SandboxPublicationGuard {
    state_root: PathBuf,
}

impl SandboxPublicationGuard {
    fn new(state_root: &Path) -> Self {
        Self {
            state_root: state_root.to_path_buf(),
        }
    }

    fn check(&self, root: &Path) -> Result<(), SandboxInitError> {
        revalidate_sandbox_publication_at_state_root(root, &self.state_root)
    }

    fn check_io(&self, root: &Path) -> io::Result<()> {
        self.check(root).map_err(io::Error::other)
    }
}

struct SandboxWorkspacePublicationHooks<'a> {
    guard: &'a SandboxPublicationGuard,
    root: &'a Path,
}

impl WorkspacePublicationHooks for SandboxWorkspacePublicationHooks<'_> {
    fn before_publication(&mut self) -> io::Result<()> {
        self.guard.check_io(self.root)
    }

    fn checkpoint(&mut self, _checkpoint: WorkspacePublicationCheckpoint) -> io::Result<()> {
        Ok(())
    }
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
    MissingProfileKey,
    InvalidProfileKey,
    ProfileKeyEnvironmentNotUnicode,
    MissingSessionCredential,
    InvalidSessionCredential,
    SessionCredentialEnvironmentNotUnicode,
    AmbiguousSandboxCredential,
    ReadBootstrapToken(io::Error),
    InvalidApiUrl(&'static str),
    CurrentDirectory(io::Error),
    InvalidDestination,
    DestinationParentMissing(PathBuf),
    DestinationExists(PathBuf),
    WorkspaceState(String),
    HostBinding(WorkspaceHostBindingError),
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
            Self::MissingProfileKey
            | Self::InvalidProfileKey
            | Self::ProfileKeyEnvironmentNotUnicode => "profile_key_invalid",
            Self::MissingSessionCredential
            | Self::InvalidSessionCredential
            | Self::SessionCredentialEnvironmentNotUnicode => "session_credential_invalid",
            Self::AmbiguousSandboxCredential => "sandbox_credential_ambiguous",
            Self::ReadBootstrapToken(_) => "bootstrap_token_read_failed",
            Self::InvalidApiUrl(_) => "api_url_invalid",
            Self::CurrentDirectory(_) => "current_directory_failed",
            Self::InvalidDestination
            | Self::DestinationParentMissing(_)
            | Self::DestinationExists(_)
            | Self::HostBinding(_) => "destination_invalid",
            Self::WorkspaceState(_) => "workspace_state_invalid",
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
                | Self::MissingProfileKey
                | Self::InvalidProfileKey
                | Self::ProfileKeyEnvironmentNotUnicode
                | Self::MissingSessionCredential
                | Self::InvalidSessionCredential
                | Self::SessionCredentialEnvironmentNotUnicode
                | Self::AmbiguousSandboxCredential
                | Self::ReadBootstrapToken(_)
                | Self::InvalidApiUrl(_)
                | Self::CurrentDirectory(_)
                | Self::InvalidDestination
                | Self::DestinationParentMissing(_)
                | Self::DestinationExists(_)
                | Self::WorkspaceState(_)
                | Self::HostBinding(_)
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
            Self::MissingProfileKey => formatter.write_str(
                "provide the Workspace Profile key through LOCALITY_PROFILE_KEY or --profile-key-stdin",
            ),
            Self::InvalidProfileKey => formatter.write_str(
                "Workspace Profile key must contain exactly 64 lowercase hexadecimal characters",
            ),
            Self::ProfileKeyEnvironmentNotUnicode => {
                formatter.write_str("LOCALITY_PROFILE_KEY is not valid Unicode")
            }
            Self::MissingSessionCredential => formatter.write_str(
                "provide the ephemeral session credential through LOCALITY_SESSION_CREDENTIAL or --session-credential-stdin",
            ),
            Self::InvalidSessionCredential => formatter.write_str(
                "ephemeral session credential must be a valid WorkspaceProfileSession JSON document",
            ),
            Self::SessionCredentialEnvironmentNotUnicode => {
                formatter.write_str("LOCALITY_SESSION_CREDENTIAL is not valid Unicode")
            }
            Self::AmbiguousSandboxCredential => formatter.write_str(
                "provide either a bootstrap token or a Workspace Profile key, not both",
            ),
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
            Self::WorkspaceState(detail) => {
                write!(formatter, "could not inspect active workspace roots: {detail}")
            }
            Self::HostBinding(error) => write!(formatter, "invalid sandbox host binding: {error}"),
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

pub fn resolve_profile_key(
    use_stdin: bool,
    environment: Option<OsString>,
    stdin: &mut impl Read,
) -> Result<SandboxProfileKey, SandboxInitError> {
    if use_stdin && environment.is_some() {
        return Err(SandboxInitError::AmbiguousSandboxCredential);
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
            .map_err(|_| SandboxInitError::ProfileKeyEnvironmentNotUnicode)?
    } else {
        return Err(SandboxInitError::MissingProfileKey);
    };
    SandboxProfileKey::new(value)
}

pub fn resolve_session_credential(
    use_stdin: bool,
    environment: Option<OsString>,
    stdin: &mut impl Read,
) -> Result<SessionCapability, SandboxInitError> {
    if use_stdin && environment.is_some() {
        return Err(SandboxInitError::AmbiguousSandboxCredential);
    }
    let value = if use_stdin {
        let mut value = String::new();
        stdin
            .take(MAX_SESSION_CREDENTIAL_BYTES + 1)
            .read_to_string(&mut value)
            .map_err(SandboxInitError::ReadBootstrapToken)?;
        value
    } else if let Some(value) = environment {
        value
            .into_string()
            .map_err(|_| SandboxInitError::SessionCredentialEnvironmentNotUnicode)?
    } else {
        return Err(SandboxInitError::MissingSessionCredential);
    };
    if value.len() as u64 > MAX_SESSION_CREDENTIAL_BYTES {
        return Err(SandboxInitError::InvalidSessionCredential);
    }
    let session: WorkspaceProfileSession =
        serde_json::from_str(&value).map_err(|_| SandboxInitError::InvalidSessionCredential)?;
    if session.profile_id.is_empty() || session.profile_revision == 0 {
        return Err(SandboxInitError::InvalidSessionCredential);
    }
    let capability = SessionCapability {
        session_id: session.session_id,
        opaque_capability: session.opaque_capability,
        expires_at: session.expires_at,
    };
    validate_capability(&capability)?;
    Ok(capability)
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

/// Headless `loc` integration for an authenticated generation-2 export.
///
/// Desktop calls the same public localityd materializer directly. Keeping this
/// additive entry point separate leaves every API-v1 bootstrap path unchanged.
pub fn materialize_workspace_export_v2<Body: Read>(
    archive: ReplicaArchive<Body>,
    destination: &Path,
    limits: WorkspaceMaterializationLimits,
    session: &WorkspaceProfileSessionV2,
    offer: &WorkspaceExportOfferV2,
    ownership: &WorkspaceOwnershipCapability,
) -> Result<PublishedWorkspace, WorkspaceMaterializationError> {
    materialize_workspace_export_v2_at_state_root(
        archive,
        destination,
        limits,
        session,
        offer,
        ownership,
        &locality_platform::default_state_root(),
    )
}

/// Generation-2 materialization bound to an explicit Locality state root.
pub fn materialize_workspace_export_v2_at_state_root<Body: Read>(
    archive: ReplicaArchive<Body>,
    destination: &Path,
    limits: WorkspaceMaterializationLimits,
    session: &WorkspaceProfileSessionV2,
    offer: &WorkspaceExportOfferV2,
    ownership: &WorkspaceOwnershipCapability,
    state_root: &Path,
) -> Result<PublishedWorkspace, WorkspaceMaterializationError> {
    let destination = if destination.is_absolute() {
        destination.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|current| current.join(destination))
            .map_err(WorkspaceMaterializationError::PrepublicationCheck)?
    };
    let guard = SandboxPublicationGuard::new(state_root);
    guard.check(&destination).map_err(|error| {
        WorkspaceMaterializationError::PrepublicationCheck(io::Error::other(error))
    })?;
    let mut hooks = SandboxWorkspacePublicationHooks {
        guard: &guard,
        root: &destination,
    };
    materialize_workspace_archive_durable_with_hooks(
        archive,
        &destination,
        limits,
        session,
        offer,
        ownership,
        &mut hooks,
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
    run_sandbox_init_with_encoding_at_state_root(
        options,
        &locality_platform::default_state_root(),
        bootstrap_token,
        content_encoding,
    )
}

pub fn run_sandbox_init_with_encoding_at_state_root(
    options: SandboxInitOptions,
    state_root: &Path,
    bootstrap_token: SandboxBootstrapToken,
    content_encoding: SandboxContentEncodingPreference,
) -> Result<SandboxInitReport, SandboxInitError> {
    let options = resolve_sandbox_init_options_at_state_root(options, state_root)?;
    let guard = SandboxPublicationGuard::new(state_root);
    run_sandbox_init_internal(
        options,
        SandboxCredential::Bootstrap(bootstrap_token),
        content_encoding,
        None,
        &guard,
    )
}

pub fn run_sandbox_init_with_profile_key(
    options: SandboxInitOptions,
    profile_key: SandboxProfileKey,
    content_encoding: SandboxContentEncodingPreference,
) -> Result<SandboxInitReport, SandboxInitError> {
    run_sandbox_init_with_profile_key_at_state_root(
        options,
        &locality_platform::default_state_root(),
        profile_key,
        content_encoding,
    )
}

pub fn run_sandbox_init_with_profile_key_at_state_root(
    options: SandboxInitOptions,
    state_root: &Path,
    profile_key: SandboxProfileKey,
    content_encoding: SandboxContentEncodingPreference,
) -> Result<SandboxInitReport, SandboxInitError> {
    let options = resolve_sandbox_init_options_at_state_root(options, state_root)?;
    let guard = SandboxPublicationGuard::new(state_root);
    run_sandbox_init_internal(
        options,
        SandboxCredential::ProfileKey(profile_key),
        content_encoding,
        None,
        &guard,
    )
}

pub fn run_sandbox_init_with_session_credential(
    options: SandboxInitOptions,
    capability: SessionCapability,
    content_encoding: SandboxContentEncodingPreference,
) -> Result<SandboxInitReport, SandboxInitError> {
    run_sandbox_init_with_session_credential_at_state_root(
        options,
        &locality_platform::default_state_root(),
        capability,
        content_encoding,
    )
}

pub fn run_sandbox_init_with_session_credential_at_state_root(
    options: SandboxInitOptions,
    state_root: &Path,
    capability: SessionCapability,
    content_encoding: SandboxContentEncodingPreference,
) -> Result<SandboxInitReport, SandboxInitError> {
    let options = resolve_sandbox_init_options_at_state_root(options, state_root)?;
    let guard = SandboxPublicationGuard::new(state_root);
    run_sandbox_init_internal(
        options,
        SandboxCredential::Session(capability),
        content_encoding,
        None,
        &guard,
    )
}

pub(crate) fn run_sandbox_init_with_encoding_and_profile_at_state_root(
    options: SandboxInitOptions,
    state_root: &Path,
    bootstrap_token: SandboxBootstrapToken,
    content_encoding: SandboxContentEncodingPreference,
    profile: &mut SandboxInitProfile,
) -> Result<SandboxInitReport, SandboxInitError> {
    let options = resolve_sandbox_init_options_at_state_root(options, state_root)?;
    let guard = SandboxPublicationGuard::new(state_root);
    run_sandbox_init_internal(
        options,
        SandboxCredential::Bootstrap(bootstrap_token),
        content_encoding,
        Some(profile),
        &guard,
    )
}

pub(crate) fn run_sandbox_init_with_profile_key_and_profile_at_state_root(
    options: SandboxInitOptions,
    state_root: &Path,
    profile_key: SandboxProfileKey,
    content_encoding: SandboxContentEncodingPreference,
    profile: &mut SandboxInitProfile,
) -> Result<SandboxInitReport, SandboxInitError> {
    let options = resolve_sandbox_init_options_at_state_root(options, state_root)?;
    let guard = SandboxPublicationGuard::new(state_root);
    run_sandbox_init_internal(
        options,
        SandboxCredential::ProfileKey(profile_key),
        content_encoding,
        Some(profile),
        &guard,
    )
}

pub(crate) fn run_sandbox_init_with_session_credential_and_profile_at_state_root(
    options: SandboxInitOptions,
    state_root: &Path,
    capability: SessionCapability,
    content_encoding: SandboxContentEncodingPreference,
    profile: &mut SandboxInitProfile,
) -> Result<SandboxInitReport, SandboxInitError> {
    let options = resolve_sandbox_init_options_at_state_root(options, state_root)?;
    let guard = SandboxPublicationGuard::new(state_root);
    run_sandbox_init_internal(
        options,
        SandboxCredential::Session(capability),
        content_encoding,
        Some(profile),
        &guard,
    )
}

enum SandboxCredential {
    Bootstrap(SandboxBootstrapToken),
    ProfileKey(SandboxProfileKey),
    Session(SessionCapability),
}

enum WorkspaceProfileNegotiation {
    Generation1(SessionCapability),
    Generation2 {
        session: WorkspaceProfileSessionV2,
        capabilities: WorkspaceClientCapabilitiesV2,
    },
}

fn run_sandbox_init_internal(
    options: SandboxInitOptions,
    credential: SandboxCredential,
    content_encoding: SandboxContentEncodingPreference,
    mut profile: Option<&mut SandboxInitProfile>,
    publication_guard: &SandboxPublicationGuard,
) -> Result<SandboxInitReport, SandboxInitError> {
    let root = absolute_destination(&options.root)?;
    validate_destination_parent(&root)?;
    let ownership = match &credential {
        SandboxCredential::ProfileKey(profile_key) => Some(profile_key.ownership_capability()),
        _ => None,
    };
    if let Some(ownership) = ownership.as_ref() {
        let verified_v2_state = recover_and_verify_workspace_publication_state(&root, ownership)
            .map_err(|error| SandboxInitError::Materialization(error.to_string()))?;
        if !verified_v2_state {
            validate_destination_absent(&root)?;
        }
    } else {
        validate_destination_absent(&root)?;
    }
    let client = SandboxHttpClient::new(&options.api_url)?;
    mark_profile(&mut profile, PROFILE_CLIENT_SETUP);

    let capability = match credential {
        SandboxCredential::Bootstrap(bootstrap_token) => {
            client.exchange_bootstrap(&bootstrap_token)?
        }
        SandboxCredential::ProfileKey(profile_key) => {
            match client.create_workspace_profile_session_negotiated(&profile_key)? {
                WorkspaceProfileNegotiation::Generation1(capability) => {
                    validate_destination_absent(&root)?;
                    capability
                }
                WorkspaceProfileNegotiation::Generation2 {
                    session,
                    capabilities,
                } => {
                    mark_profile(&mut profile, PROFILE_BOOTSTRAP_EXCHANGE);
                    let ownership = ownership
                        .as_ref()
                        .expect("profile-key ownership capability");
                    publication_guard.check(&root)?;
                    recover_workspace_publication(&root, ownership)
                        .map_err(|error| SandboxInitError::Materialization(error.to_string()))?;
                    return run_generation2_workspace_init(
                        &client,
                        &root,
                        content_encoding,
                        session,
                        capabilities,
                        ownership,
                        profile,
                        publication_guard,
                    );
                }
            }
        }
        SandboxCredential::Session(capability) => capability,
    };
    mark_profile(&mut profile, PROFILE_BOOTSTRAP_EXCHANGE);
    validate_capability(&capability)?;
    let status = client.session_status(&capability)?;
    mark_profile(&mut profile, PROFILE_SESSION_STATUS);
    let session = validate_status(&capability, &status)?;
    let (encoding, summary) = match session {
        ValidatedSandboxSession::Legacy {
            offer,
            expected_receipt,
        } => {
            validate_encoding_preference(offer, content_encoding)?;
            let limits = limits_for_offer(offer)?;
            let (encoding, response) = client.open_export(&capability, offer, content_encoding)?;
            mark_profile(&mut profile, PROFILE_EXPORT_OPEN_HEADERS);
            let summary = materialize_export_response(
                response,
                encoding,
                &root,
                limits,
                &ExportValidation::Legacy(expected_receipt),
                profile.as_deref_mut(),
                publication_guard,
            )
            .map_err(|failure| failure.error)?;
            (encoding, summary)
        }
        ValidatedSandboxSession::ScopeAuthorized {
            export_attempt_limits,
        } => {
            let request =
                export_attempt_request(&capability, content_encoding, export_attempt_limits)?;
            let offer = client.create_export_attempt(&capability, &request)?;
            validate_scope_offer(&capability, &request, &offer)?;
            let limits = limits_for_scope_offer(&offer)?;
            let validation = ExportValidation::ScopeAuthorized(offer);
            let mut last_retryable_error = None;
            let mut completed = None;
            for attempt in 0..EXPORT_ATTEMPT_STREAM_ATTEMPTS {
                let (encoding, response) =
                    match client.open_export_attempt(&capability, validation.scope_offer()) {
                        Ok(opened) => opened,
                        Err(failure)
                            if failure.retryable
                                && has_retry_remaining(attempt, EXPORT_ATTEMPT_STREAM_ATTEMPTS) =>
                        {
                            last_retryable_error = Some(failure.error);
                            continue;
                        }
                        Err(failure) => return Err(failure.error),
                    };
                mark_profile(&mut profile, PROFILE_EXPORT_OPEN_HEADERS);
                match materialize_export_response(
                    response,
                    encoding,
                    &root,
                    limits,
                    &validation,
                    profile.as_deref_mut(),
                    publication_guard,
                ) {
                    Ok(summary) => {
                        completed = Some((encoding, summary));
                        break;
                    }
                    Err(failure)
                        if failure.retryable
                            && has_retry_remaining(attempt, EXPORT_ATTEMPT_STREAM_ATTEMPTS) =>
                    {
                        last_retryable_error = Some(failure.error);
                    }
                    Err(failure) => return Err(failure.error),
                }
            }
            completed.ok_or_else(|| {
                last_retryable_error.unwrap_or_else(|| {
                    SandboxInitError::Materialization(
                        "sandbox export retry ended without a result".to_string(),
                    )
                })
            })?
        }
    };

    Ok(report(&root, &capability, encoding, summary))
}

fn run_generation2_workspace_init(
    client: &SandboxHttpClient,
    root: &Path,
    content_encoding: SandboxContentEncodingPreference,
    session: WorkspaceProfileSessionV2,
    capabilities: WorkspaceClientCapabilitiesV2,
    ownership: &WorkspaceOwnershipCapability,
    mut profile: Option<&mut SandboxInitProfile>,
    publication_guard: &SandboxPublicationGuard,
) -> Result<SandboxInitReport, SandboxInitError> {
    let capability = SessionCapability {
        session_id: session.session_id().clone(),
        opaque_capability: session.opaque_capability().to_string(),
        expires_at: session.expires_at().to_string(),
    };
    validate_capability(&capability)?;
    let mut status = client.workspace_session_status(&session, &capabilities)?;
    mark_profile(&mut profile, PROFILE_SESSION_STATUS);
    if status.state() == SandboxSessionState::Bootstrapping
        && status.error().is_some_and(|error| {
            error.retriable
                && matches!(
                    error.code,
                    SessionErrorCode::Bootstrapping
                        | SessionErrorCode::Stale
                        | SessionErrorCode::Incomplete
                )
        })
        && status.freshness_requirement().on_stale
            == locality_protocol::StaleSessionBehavior::WaitThenFail
        && capabilities.supports_freshness_wait()
    {
        match client.wait_for_workspace_freshness(
            &session,
            &capabilities,
            status.freshness_requirement(),
        )? {
            FreshnessWaitAvailability::Completed => {
                mark_profile(&mut profile, PROFILE_FRESHNESS_WAIT);
                status = client.workspace_session_status(&session, &capabilities)?;
                mark_profile(&mut profile, PROFILE_SESSION_STATUS);
            }
            FreshnessWaitAvailability::Unavailable => {}
        }
    }
    if status.state() != SandboxSessionState::Ready {
        return Err(SandboxInitError::SessionNotReady {
            state: status.state(),
            code: status.error().map(|error| error.code),
        });
    }
    if status.error().is_some() {
        return Err(SandboxInitError::InvalidReadySession("error is present"));
    }
    let export_limits =
        status
            .export_attempt_limits()
            .ok_or(SandboxInitError::InvalidReadySession(
                "export-attempt limits are absent",
            ))?;
    let request = export_attempt_request(&capability, content_encoding, export_limits)?;
    let offer =
        client.create_workspace_export_attempt(&session, &status, &capabilities, &request)?;
    let encoding = replica_encoding_for_protocol(offer.offer().content_encoding);
    if let Some(required) = content_encoding.required_encoding()
        && required != encoding
    {
        return Err(SandboxInitError::UnsupportedExportEncoding(format!(
            "{} (requested {})",
            encoding_name(encoding),
            encoding_name(required)
        )));
    }
    let response = client.open_workspace_export_attempt(&session, &offer)?;
    mark_profile(&mut profile, PROFILE_EXPORT_OPEN_HEADERS);
    let limits = workspace_limits_for_offer(&offer)?;
    let published = materialize_workspace_export_response(
        response,
        encoding,
        root,
        limits,
        &session,
        &offer,
        ownership,
        profile,
        publication_guard,
    )?;
    let summary = ReplicaMaterializationSummary {
        entries: published.validated.archive_entries,
        files: published.validated.files,
        directories: published.validated.directories,
        materialized_bytes: published.validated.content_bytes,
        decoded_bytes: published.decoded_bytes,
    };
    Ok(report(root, &capability, encoding, summary))
}

fn workspace_limits_for_offer(
    offer: &WorkspaceExportOfferV2,
) -> Result<WorkspaceMaterializationLimits, SandboxInitError> {
    let sealed = offer.offer();
    if sealed.archive_entry_count > ReplicaMaterializationLimits::default().max_entries {
        return Err(SandboxInitError::ExportLimit {
            limit: "archive entries",
            offered: sealed.archive_entry_count,
            maximum: ReplicaMaterializationLimits::default().max_entries,
        });
    }
    if sealed.selected_content_bytes > ReplicaMaterializationLimits::default().max_disk_bytes {
        return Err(SandboxInitError::ExportLimit {
            limit: "selected content bytes",
            offered: sealed.selected_content_bytes,
            maximum: ReplicaMaterializationLimits::default().max_disk_bytes,
        });
    }
    Ok(WorkspaceMaterializationLimits {
        archive: WorkspaceArchiveLimits {
            max_entries: sealed.archive_entry_count,
            max_file_bytes: ReplicaMaterializationLimits::default().max_file_bytes,
            max_content_bytes: sealed.selected_content_bytes,
        },
        ..WorkspaceMaterializationLimits::default()
    })
}

fn materialize_workspace_export_response(
    response: Response,
    encoding: ReplicaArchiveEncoding,
    root: &Path,
    limits: WorkspaceMaterializationLimits,
    session: &WorkspaceProfileSessionV2,
    offer: &WorkspaceExportOfferV2,
    ownership: &WorkspaceOwnershipCapability,
    mut profile: Option<&mut SandboxInitProfile>,
    publication_guard: &SandboxPublicationGuard,
) -> Result<PublishedWorkspace, SandboxInitError> {
    publication_guard.check(root)?;
    let (body, mut producer) =
        spawn_export_read_ahead(response).map_err(|error| SandboxInitError::Http {
            operation: "workspace export read-ahead setup",
            detail: error.to_string(),
        })?;
    let profiled_body = ProfiledExportBody::new(body, profile.as_deref_mut());
    let archive = ReplicaArchive::new(encoding, profiled_body);
    let mut hooks = SandboxWorkspacePublicationHooks {
        guard: publication_guard,
        root,
    };
    let materialization = materialize_workspace_archive_durable_with_hooks(
        archive, root, limits, session, offer, ownership, &mut hooks,
    );
    let producer_outcome = producer.join();
    if let Some(profile) = profile {
        profile.mark(PROFILE_STREAM_DECODE_MATERIALIZE);
    }
    let published =
        materialization.map_err(|error| SandboxInitError::Materialization(error.to_string()))?;
    match producer_outcome {
        Ok(ReadAheadProducerOutcome::CleanEof) => Ok(published),
        Ok(ReadAheadProducerOutcome::ConsumerClosed | ReadAheadProducerOutcome::ErrorDelivered) => {
            Err(SandboxInitError::Materialization(
                "workspace export transport ended without a clean EOF".to_string(),
            ))
        }
        Err(()) => Err(SandboxInitError::Materialization(
            "workspace export read-ahead worker panicked".to_string(),
        )),
    }
}

fn replica_encoding_for_protocol(encoding: TarContentEncoding) -> ReplicaArchiveEncoding {
    match encoding {
        TarContentEncoding::Identity => ReplicaArchiveEncoding::Identity,
        TarContentEncoding::Zstd => ReplicaArchiveEncoding::Zstd,
    }
}

struct ExportStreamFailure {
    error: SandboxInitError,
    retryable: bool,
}

impl ExportStreamFailure {
    fn fatal(error: SandboxInitError) -> Self {
        Self {
            error,
            retryable: false,
        }
    }
}

fn materialize_export_response(
    response: Response,
    encoding: ReplicaArchiveEncoding,
    root: &Path,
    limits: ReplicaMaterializationLimits,
    validation: &ExportValidation,
    mut profile: Option<&mut SandboxInitProfile>,
    publication_guard: &SandboxPublicationGuard,
) -> Result<ReplicaMaterializationSummary, ExportStreamFailure> {
    let (body, mut producer) =
        spawn_export_read_ahead(response).map_err(|error| ExportStreamFailure {
            error: SandboxInitError::Http {
                operation: "session export read-ahead setup",
                detail: error.to_string(),
            },
            retryable: false,
        })?;
    let profiled_body = ProfiledExportBody::new(body, profile.as_deref_mut());
    let archive = ReplicaArchive::new(encoding, profiled_body);
    let mut prepublication_check = || publication_guard.check_io(root);
    let materialization = match validation {
        ExportValidation::Legacy(expected_receipt) => {
            materialize_replica_archive_with_expected_receipt_and_prepublication_check(
                archive,
                root,
                limits,
                *expected_receipt,
                &mut prepublication_check,
            )
        }
        ExportValidation::ScopeAuthorized(offer) => {
            materialize_scope_authorized_replica_archive_with_prepublication_check(
                archive,
                root,
                limits,
                offer,
                &mut prepublication_check,
            )
        }
    };
    let producer_outcome = producer.join();
    if let Some(profile) = profile {
        profile.mark(PROFILE_STREAM_DECODE_MATERIALIZE);
    }

    match materialization {
        Err(error) => {
            let retryable = matches!(validation, ExportValidation::ScopeAuthorized(_))
                && match producer_outcome {
                    Ok(ReadAheadProducerOutcome::ErrorDelivered) => {
                        is_retryable_truncated_materialization(&error)
                    }
                    Ok(ReadAheadProducerOutcome::CleanEof) => {
                        matches!(&error, ReplicaMaterializationError::MissingTarEndMarker)
                    }
                    Ok(ReadAheadProducerOutcome::ConsumerClosed) | Err(()) => false,
                };
            Err(ExportStreamFailure {
                error: SandboxInitError::Materialization(error.to_string()),
                retryable,
            })
        }
        Ok(summary) => {
            match producer_outcome {
                Ok(ReadAheadProducerOutcome::CleanEof) => {}
                Ok(
                    ReadAheadProducerOutcome::ConsumerClosed
                    | ReadAheadProducerOutcome::ErrorDelivered,
                ) => {
                    return Err(ExportStreamFailure {
                        error: SandboxInitError::Materialization(
                            "sandbox export transport ended without a clean EOF".to_string(),
                        ),
                        retryable: false,
                    });
                }
                Err(()) => {
                    return Err(ExportStreamFailure {
                        error: SandboxInitError::Materialization(
                            "sandbox export read-ahead worker panicked".to_string(),
                        ),
                        retryable: false,
                    });
                }
            }
            Ok(summary)
        }
    }
}

fn is_retryable_truncated_materialization(error: &ReplicaMaterializationError) -> bool {
    match error {
        ReplicaMaterializationError::Decode(message)
        | ReplicaMaterializationError::MalformedTar(message) => {
            message.contains("sandbox export transport read failed")
        }
        ReplicaMaterializationError::MissingTarEndMarker => true,
        ReplicaMaterializationError::Write { source, .. } => source
            .to_string()
            .contains("sandbox export transport read failed"),
        _ => false,
    }
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
        let started = Instant::now();
        let result = self.body.read(output);
        let wait = started.elapsed();
        if let Some(profile) = self.profile.as_deref_mut() {
            profile.observe_export_transport_read(wait, result.as_ref().copied().unwrap_or(0));
        }
        let read = result?;
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

fn validate_destination_parent(root: &Path) -> Result<(), SandboxInitError> {
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
    Ok(())
}

fn validate_destination_absent(root: &Path) -> Result<(), SandboxInitError> {
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
    ScopeAuthorized {
        export_attempt_limits: &'a ExportAttemptLimits,
    },
}

enum ExportValidation {
    Legacy(ExpectedReplicaMaterializationReceipt),
    ScopeAuthorized(SealedExportOffer),
}

impl ExportValidation {
    fn scope_offer(&self) -> &SealedExportOffer {
        match self {
            Self::ScopeAuthorized(offer) => offer,
            Self::Legacy(_) => unreachable!("legacy export validation has no scope offer"),
        }
    }
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
        let export_attempt_limits =
            status
                .export_attempt_limits
                .as_ref()
                .ok_or(SandboxInitError::InvalidReadySession(
                    "scope-authorized export-attempt limits are missing",
                ))?;
        validate_negotiated_export_attempt_limits(export_attempt_limits)?;
        return Ok(ValidatedSandboxSession::ScopeAuthorized {
            export_attempt_limits,
        });
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
    limits: &ExportAttemptLimits,
) -> Result<ExportAttemptRequest, SandboxInitError> {
    let request = ExportAttemptRequest {
        versions: SCOPE_AUTHORIZED_COMPONENT_VERSIONS,
        opaque_session_capability: capability.opaque_capability.clone(),
        idempotency_key: random_export_idempotency_key()?,
        content_encoding: match preference {
            SandboxContentEncodingPreference::Automatic
            | SandboxContentEncodingPreference::Zstd => TarContentEncoding::Zstd,
            SandboxContentEncodingPreference::Identity => TarContentEncoding::Identity,
        },
        limits: limits.clone(),
    };
    request.validate().map_err(|_| {
        SandboxInitError::InvalidExportOffer("client export-attempt request is invalid")
    })?;
    Ok(request)
}

fn validate_negotiated_export_attempt_limits(
    limits: &ExportAttemptLimits,
) -> Result<(), SandboxInitError> {
    limits.validate().map_err(|_| {
        SandboxInitError::InvalidReadySession("scope-authorized export-attempt limits are invalid")
    })?;
    let defaults = ReplicaMaterializationLimits::default();
    let maximum_entries = defaults.max_entries.saturating_sub(1);
    for (limit, offered, maximum) in [
        ("maximum file count", limits.max_files, maximum_entries),
        (
            "maximum directory count",
            limits.max_directories,
            maximum_entries,
        ),
        (
            "maximum content bytes",
            limits.max_content_bytes,
            defaults.max_disk_bytes,
        ),
    ] {
        if offered > maximum {
            return Err(SandboxInitError::ExportLimit {
                limit,
                offered,
                maximum,
            });
        }
    }
    Ok(())
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FreshnessWaitAvailability {
    Completed,
    Unavailable,
}

enum FreshnessWaitPostResult {
    Response {
        bytes: Vec<u8>,
        authenticated_server_time: String,
        response_headers_received_at: Instant,
    },
    RouteUnavailable,
    DeadlineReached,
}

/// Validates the exact API URL shape accepted by sandbox and portable
/// workspace clients without performing network I/O.
pub fn validate_sandbox_api_url(api_url: &str) -> Result<(), SandboxInitError> {
    parse_sandbox_api_url(api_url).map(|_| ())
}

fn parse_sandbox_api_url(api_url: &str) -> Result<reqwest::Url, SandboxInitError> {
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
    Ok(api_url)
}

impl SandboxHttpClient {
    fn new(api_url: &str) -> Result<Self, SandboxInitError> {
        Self::new_with_read_timeout(api_url, HTTP_READ_TIMEOUT)
    }

    fn new_with_read_timeout(
        api_url: &str,
        read_timeout: Duration,
    ) -> Result<Self, SandboxInitError> {
        let api_url = parse_sandbox_api_url(api_url)?;
        REQWEST_CRYPTO_PROVIDER.get_or_init(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
        let client = Client::builder()
            .user_agent(SANDBOX_USER_AGENT)
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

    fn create_workspace_profile_session(
        &self,
        profile_key: &SandboxProfileKey,
    ) -> Result<SessionCapability, SandboxInitError> {
        let idempotency_key = random_idempotency_key()?;
        for attempt in 0..BOOTSTRAP_EXCHANGE_ATTEMPTS {
            let response = match self
                .client
                .post(self.workspace_profile_sessions_url())
                .header(ACCEPT, JSON_MEDIA_TYPE)
                .header(IDEMPOTENCY_KEY_HEADER, &idempotency_key)
                .bearer_auth(profile_key.expose())
                .send()
            {
                Ok(response) => response,
                Err(error) => {
                    let error = SandboxInitError::Http {
                        operation: "Workspace Profile session creation",
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
            let session: WorkspaceProfileSession = match read_json_response_with_status(
                response,
                "Workspace Profile session creation",
                StatusCode::CREATED,
            ) {
                Ok(session) => session,
                Err(error)
                    if has_retry_remaining(attempt, BOOTSTRAP_EXCHANGE_ATTEMPTS)
                        && is_ambiguous_idempotent_error(
                            &error,
                            "Workspace Profile session creation",
                        ) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            if session.profile_id.is_empty() || session.profile_revision == 0 {
                return Err(SandboxInitError::InvalidCapability(
                    "Workspace Profile identity is invalid",
                ));
            }
            return Ok(SessionCapability {
                session_id: session.session_id,
                opaque_capability: session.opaque_capability,
                expires_at: session.expires_at,
            });
        }
        unreachable!("profile session attempt loop always returns")
    }

    fn create_workspace_profile_session_negotiated(
        &self,
        profile_key: &SandboxProfileKey,
    ) -> Result<WorkspaceProfileNegotiation, SandboxInitError> {
        let capabilities = WorkspaceClientCapabilitiesV2::workspace_layout_v1(true);
        let request = WorkspaceProfileSessionRequestV2::new(capabilities.clone());
        let response = self
            .client
            .post(self.workspace_profile_sessions_v2_url())
            .header(ACCEPT, JSON_MEDIA_TYPE)
            .header(IDEMPOTENCY_KEY_HEADER, random_idempotency_key()?)
            .bearer_auth(profile_key.expose())
            .json(&request)
            .send()
            .map_err(|error| SandboxInitError::Http {
                operation: "generation-2 Workspace Profile session creation",
                detail: error.without_url().to_string(),
            })?;
        if matches!(
            response.status(),
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
        ) {
            return self
                .create_workspace_profile_session(profile_key)
                .map(WorkspaceProfileNegotiation::Generation1);
        }
        let bytes = read_json_response_bytes(
            response,
            "generation-2 Workspace Profile session creation",
            StatusCode::CREATED,
        )?;
        let session = WorkspaceProfileSessionV2::decode_json(&bytes).map_err(|error| {
            SandboxInitError::InvalidJson {
                operation: "generation-2 Workspace Profile session creation",
                detail: error.to_string(),
            }
        })?;
        Ok(WorkspaceProfileNegotiation::Generation2 {
            session,
            capabilities,
        })
    }

    fn workspace_session_status(
        &self,
        session: &WorkspaceProfileSessionV2,
        capabilities: &WorkspaceClientCapabilitiesV2,
    ) -> Result<WorkspaceSessionStatusV2, SandboxInitError> {
        let response = self
            .client
            .get(self.workspace_session_v2_url(session.session_id().as_str()))
            .header(ACCEPT, JSON_MEDIA_TYPE)
            .bearer_auth(session.opaque_capability())
            .send()
            .map_err(|error| SandboxInitError::Http {
                operation: "generation-2 session status",
                detail: error.without_url().to_string(),
            })?;
        let bytes =
            read_json_response_bytes(response, "generation-2 session status", StatusCode::OK)?;
        WorkspaceSessionStatusV2::decode_json(&bytes, session, capabilities).map_err(|error| {
            SandboxInitError::InvalidJson {
                operation: "generation-2 session status",
                detail: error.to_string(),
            }
        })
    }

    fn wait_for_workspace_freshness(
        &self,
        session: &WorkspaceProfileSessionV2,
        capabilities: &WorkspaceClientCapabilitiesV2,
        expected_freshness_requirement: &FreshnessRequirement,
    ) -> Result<FreshnessWaitAvailability, SandboxInitError> {
        let request = FreshnessWaitAttemptRequest {
            format_version: FRESHNESS_WAIT_FORMAT_VERSION,
            minimum_reader_version: FRESHNESS_WAIT_READER_VERSION,
            api_generation: WORKSPACE_HTTP_API_GENERATION_V2,
            session_id: session.session_id().clone(),
            idempotency_key: random_idempotency_key()?,
            capabilities: capabilities.clone(),
        };
        request
            .validate()
            .map_err(|error| invalid_freshness_wait_response(error.to_string()))?;

        let (accepted, server_time, response_headers_received_at) =
            match self.post_freshness_wait_attempt(session, &request, None)? {
                FreshnessWaitPostResult::Response {
                    bytes,
                    authenticated_server_time,
                    response_headers_received_at,
                } => (
                    bytes,
                    authenticated_server_time,
                    response_headers_received_at,
                ),
                FreshnessWaitPostResult::RouteUnavailable => {
                    return Ok(FreshnessWaitAvailability::Unavailable);
                }
                FreshnessWaitPostResult::DeadlineReached => {
                    unreachable!("the first freshness wait request has no local deadline")
                }
            };
        let mut attempt = FreshnessWaitAttempt::decode_json(&accepted, &request, &server_time)
            .map_err(|error| invalid_freshness_wait_response(error.to_string()))?;
        if attempt.freshness_requirement != *expected_freshness_requirement {
            return Err(invalid_freshness_wait_response(
                "freshness wait requirement does not match the initiating session status"
                    .to_string(),
            ));
        }
        let mut local_deadline = if attempt.state == FreshnessWaitAggregateState::Waiting {
            Some(freshness_wait_local_deadline(
                &attempt,
                &server_time,
                response_headers_received_at,
            )?)
        } else {
            None
        };

        loop {
            if attempt.state == FreshnessWaitAggregateState::Terminal {
                return Ok(FreshnessWaitAvailability::Completed);
            }

            let deadline = local_deadline.expect("validated waiting attempts have a deadline");
            let retry_after = Duration::from_secs(
                attempt
                    .poll
                    .as_ref()
                    .and_then(|poll| poll.retry.retry_after_seconds)
                    .expect("validated waiting attempts carry positive poll advice"),
            );
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining <= retry_after {
                if !remaining.is_zero() {
                    thread::sleep(remaining);
                }
                return Ok(FreshnessWaitAvailability::Completed);
            }
            thread::sleep(retry_after);

            let (successor_bytes, successor_server_time, successor_headers_received_at) =
                match self.post_freshness_wait_attempt(session, &request, Some(deadline))? {
                    FreshnessWaitPostResult::Response {
                        bytes,
                        authenticated_server_time,
                        response_headers_received_at,
                    } => (
                        bytes,
                        authenticated_server_time,
                        response_headers_received_at,
                    ),
                    FreshnessWaitPostResult::RouteUnavailable => {
                        return Ok(FreshnessWaitAvailability::Unavailable);
                    }
                    FreshnessWaitPostResult::DeadlineReached => {
                        return Ok(FreshnessWaitAvailability::Completed);
                    }
                };
            let successor = FreshnessWaitAttempt::decode_json(
                &successor_bytes,
                &request,
                &successor_server_time,
            )
            .map_err(|error| invalid_freshness_wait_response(error.to_string()))?;
            successor
                .validate_successor(&attempt, &request, &successor_server_time)
                .map_err(|error| invalid_freshness_wait_response(error.to_string()))?;

            let successor_deadline = freshness_wait_local_deadline(
                &successor,
                &successor_server_time,
                successor_headers_received_at,
            )?;
            local_deadline = Some(deadline.min(successor_deadline));
            attempt = successor;
        }
    }

    fn post_freshness_wait_attempt(
        &self,
        session: &WorkspaceProfileSessionV2,
        request: &FreshnessWaitAttemptRequest,
        local_deadline: Option<Instant>,
    ) -> Result<FreshnessWaitPostResult, SandboxInitError> {
        for attempt in 0..FRESHNESS_WAIT_REQUEST_ATTEMPTS {
            let timeout = match local_deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Ok(FreshnessWaitPostResult::DeadlineReached);
                    }
                    FRESHNESS_WAIT_REQUEST_TIMEOUT.min(remaining)
                }
                None => FRESHNESS_WAIT_REQUEST_TIMEOUT,
            };
            let response = match self
                .client
                .post(self.workspace_freshness_wait_attempts_v2_url(session.session_id().as_str()))
                .header(ACCEPT, JSON_MEDIA_TYPE)
                .bearer_auth(session.opaque_capability())
                .timeout(timeout)
                .json(request)
                .send()
            {
                Ok(response) => response,
                Err(error) => {
                    let error = SandboxInitError::Http {
                        operation: "freshness wait attempt",
                        detail: error.without_url().to_string(),
                    };
                    if has_retry_remaining(attempt, FRESHNESS_WAIT_REQUEST_ATTEMPTS)
                        && local_deadline.is_none_or(|deadline| Instant::now() < deadline)
                    {
                        continue;
                    }
                    if local_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                        return Ok(FreshnessWaitPostResult::DeadlineReached);
                    }
                    return Err(error);
                }
            };

            if matches!(
                response.status(),
                StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
            ) {
                return Ok(FreshnessWaitPostResult::RouteUnavailable);
            }
            if is_retriable_idempotent_status(response.status())
                && has_retry_remaining(attempt, FRESHNESS_WAIT_REQUEST_ATTEMPTS)
                && local_deadline.is_none_or(|deadline| Instant::now() < deadline)
            {
                continue;
            }
            match read_freshness_wait_response(response) {
                Ok((bytes, authenticated_server_time, response_headers_received_at)) => {
                    return Ok(FreshnessWaitPostResult::Response {
                        bytes,
                        authenticated_server_time,
                        response_headers_received_at,
                    });
                }
                Err(error)
                    if has_retry_remaining(attempt, FRESHNESS_WAIT_REQUEST_ATTEMPTS)
                        && is_ambiguous_idempotent_error(&error, "freshness wait attempt")
                        && local_deadline.is_none_or(|deadline| Instant::now() < deadline) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("freshness wait request loop always returns")
    }

    fn create_workspace_export_attempt(
        &self,
        session: &WorkspaceProfileSessionV2,
        status: &WorkspaceSessionStatusV2,
        capabilities: &WorkspaceClientCapabilitiesV2,
        request: &ExportAttemptRequest,
    ) -> Result<WorkspaceExportOfferV2, SandboxInitError> {
        let response = self
            .client
            .post(self.workspace_export_attempts_v2_url(session.session_id().as_str()))
            .header(ACCEPT, JSON_MEDIA_TYPE)
            .bearer_auth(session.opaque_capability())
            .json(request)
            .send()
            .map_err(|error| SandboxInitError::Http {
                operation: "generation-2 export-attempt creation",
                detail: error.without_url().to_string(),
            })?;
        let bytes = read_json_response_bytes(
            response,
            "generation-2 export-attempt creation",
            StatusCode::OK,
        )?;
        WorkspaceExportOfferV2::decode_json(&bytes, session, status, capabilities).map_err(
            |error| SandboxInitError::InvalidJson {
                operation: "generation-2 export-attempt creation",
                detail: error.to_string(),
            },
        )
    }

    fn open_workspace_export_attempt(
        &self,
        session: &WorkspaceProfileSessionV2,
        offer: &WorkspaceExportOfferV2,
    ) -> Result<Response, SandboxInitError> {
        let encoding = offer.offer().content_encoding;
        let response = self
            .export_client
            .get(self.workspace_export_attempt_v2_url(
                session.session_id().as_str(),
                offer.offer().export_attempt_id.as_str(),
            ))
            .header(ACCEPT, TAR_MEDIA_TYPE)
            .header(
                ACCEPT_ENCODING,
                match encoding {
                    TarContentEncoding::Identity => "identity",
                    TarContentEncoding::Zstd => "zstd",
                },
            )
            .bearer_auth(session.opaque_capability())
            .send()
            .map_err(|error| SandboxInitError::Http {
                operation: "generation-2 export-attempt stream",
                detail: error.without_url().to_string(),
            })?;
        ensure_success(&response, "generation-2 export-attempt stream")?;
        require_media_type(
            response.headers(),
            "generation-2 export-attempt stream",
            TAR_MEDIA_TYPE,
        )?;
        if protocol_encoding(response_encoding(response.headers())?) != encoding {
            return Err(SandboxInitError::UnsupportedExportEncoding(
                "generation-2 stream does not match its sealed encoding".to_string(),
            ));
        }
        Ok(response)
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
    ) -> Result<(ReplicaArchiveEncoding, Response), ExportStreamFailure> {
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
            .map_err(|error| {
                let retryable = error.is_connect() || error.is_timeout() || error.is_body();
                ExportStreamFailure {
                    error: SandboxInitError::Http {
                        operation: "export-attempt stream",
                        detail: error.without_url().to_string(),
                    },
                    retryable,
                }
            })?;
        ensure_success(&response, "export-attempt stream").map_err(ExportStreamFailure::fatal)?;
        require_media_type(response.headers(), "export-attempt stream", TAR_MEDIA_TYPE)
            .map_err(ExportStreamFailure::fatal)?;
        let encoding = response_encoding(response.headers()).map_err(ExportStreamFailure::fatal)?;
        if protocol_encoding(encoding) != offer.content_encoding {
            return Err(ExportStreamFailure::fatal(
                SandboxInitError::UnsupportedExportEncoding(format!(
                    "{} (sealed {})",
                    encoding_name(encoding),
                    match offer.content_encoding {
                        TarContentEncoding::Identity => "identity",
                        TarContentEncoding::Zstd => "zstd",
                    }
                )),
            ));
        }
        Ok((encoding, response))
    }

    fn sessions_url(&self) -> reqwest::Url {
        endpoint_url(&self.api_url, &["v1", "sessions"])
    }

    fn workspace_profile_sessions_url(&self) -> reqwest::Url {
        endpoint_url(&self.api_url, &["v1", "workspace-profile-sessions"])
    }

    fn workspace_profile_sessions_v2_url(&self) -> reqwest::Url {
        endpoint_url(&self.api_url, &["v2", "workspace-profile-sessions"])
    }

    fn workspace_session_v2_url(&self, session_id: &str) -> reqwest::Url {
        endpoint_url(&self.api_url, &["v2", "sessions", session_id])
    }

    fn workspace_export_attempts_v2_url(&self, session_id: &str) -> reqwest::Url {
        endpoint_url(
            &self.api_url,
            &["v2", "sessions", session_id, "export-attempts"],
        )
    }

    fn workspace_freshness_wait_attempts_v2_url(&self, session_id: &str) -> reqwest::Url {
        endpoint_url(
            &self.api_url,
            &["v2", "sessions", session_id, "freshness-wait-attempts"],
        )
    }

    fn workspace_export_attempt_v2_url(&self, session_id: &str, attempt_id: &str) -> reqwest::Url {
        endpoint_url(
            &self.api_url,
            &[
                "v2",
                "sessions",
                session_id,
                "export-attempts",
                attempt_id,
                "export",
            ],
        )
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

fn random_idempotency_key() -> Result<String, SandboxInitError> {
    let mut random = [0_u8; 32];
    getrandom::fill(&mut random).map_err(|_| SandboxInitError::Http {
        operation: "Workspace Profile session credential setup",
        detail: "secure random generation failed".to_string(),
    })?;
    Ok(lower_hex(&random))
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
    response: Response,
    operation: &'static str,
) -> Result<T, SandboxInitError> {
    read_json_response_with_status(response, operation, StatusCode::OK)
}

fn read_json_response_with_status<T: DeserializeOwned>(
    response: Response,
    operation: &'static str,
    expected_status: StatusCode,
) -> Result<T, SandboxInitError> {
    let bytes = read_json_response_bytes(response, operation, expected_status)?;
    serde_json::from_slice(&bytes).map_err(|error| SandboxInitError::InvalidJson {
        operation,
        detail: error.to_string(),
    })
}

fn read_json_response_bytes(
    mut response: Response,
    operation: &'static str,
    expected_status: StatusCode,
) -> Result<Vec<u8>, SandboxInitError> {
    ensure_status(&response, operation, expected_status)?;
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
    Ok(bytes)
}

fn read_freshness_wait_response(
    mut response: Response,
) -> Result<(Vec<u8>, String, Instant), SandboxInitError> {
    let response_headers_received_at = Instant::now();
    ensure_status(&response, "freshness wait attempt", StatusCode::OK)?;
    require_media_type(
        response.headers(),
        "freshness wait attempt",
        JSON_MEDIA_TYPE,
    )?;
    let authenticated_server_time = authenticated_server_time(response.headers())?;
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take(MAX_FRESHNESS_WAIT_ATTEMPT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| SandboxInitError::Http {
            operation: "freshness wait attempt",
            detail: error.to_string(),
        })?;
    if bytes.len() > MAX_FRESHNESS_WAIT_ATTEMPT_BYTES {
        return Err(SandboxInitError::JsonResponseTooLarge {
            operation: "freshness wait attempt",
            limit: MAX_FRESHNESS_WAIT_ATTEMPT_BYTES as u64,
        });
    }
    Ok((
        bytes,
        authenticated_server_time,
        response_headers_received_at,
    ))
}

fn authenticated_server_time(headers: &HeaderMap) -> Result<String, SandboxInitError> {
    let value = headers
        .get(DATE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            invalid_freshness_wait_response(
                "response is missing a valid authenticated HTTP Date header".to_string(),
            )
        })?;
    let timestamp =
        NaiveDateTime::parse_from_str(value, "%a, %d %b %Y %H:%M:%S GMT").map_err(|_| {
            invalid_freshness_wait_response(
                "response has an invalid authenticated HTTP Date header".to_string(),
            )
        })?;
    Ok(timestamp.and_utc().format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

fn freshness_wait_local_deadline(
    attempt: &FreshnessWaitAttempt,
    authenticated_server_time: &str,
    response_headers_received_at: Instant,
) -> Result<Instant, SandboxInitError> {
    let server_time = DateTime::parse_from_rfc3339(authenticated_server_time)
        .map_err(|error| invalid_freshness_wait_response(error.to_string()))?;
    let deadline = DateTime::parse_from_rfc3339(&attempt.original_deadline_at)
        .map_err(|error| invalid_freshness_wait_response(error.to_string()))?;
    let remaining = deadline.signed_duration_since(server_time).num_seconds();
    let remaining = u64::try_from(remaining.max(0))
        .map_err(|error| invalid_freshness_wait_response(error.to_string()))?;
    response_headers_received_at
        .checked_add(Duration::from_secs(remaining))
        .ok_or_else(|| invalid_freshness_wait_response("deadline overflow".to_string()))
}

fn invalid_freshness_wait_response(detail: String) -> SandboxInitError {
    SandboxInitError::InvalidJson {
        operation: "freshness wait attempt",
        detail,
    }
}

fn ensure_success(response: &Response, operation: &'static str) -> Result<(), SandboxInitError> {
    ensure_status(response, operation, StatusCode::OK)
}

fn ensure_status(
    response: &Response,
    operation: &'static str,
    expected_status: StatusCode,
) -> Result<(), SandboxInitError> {
    if response.status() == expected_status {
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

    #[test]
    fn session_credential_json_is_validated_without_exposing_the_capability() {
        let json = r#"{
            "session_id":"session-profile-7",
            "opaque_capability":"opaque-session-capability",
            "expires_at":"2026-07-29T01:00:00Z",
            "profile_id":"00000000-0000-0000-0000-000000000007",
            "profile_revision":9
        }"#;
        let capability = resolve_session_credential(true, None, &mut json.as_bytes())
            .expect("valid profile session JSON");
        assert_eq!(capability.session_id.as_str(), "session-profile-7");
        assert_eq!(capability.opaque_capability, "opaque-session-capability");
        assert!(!format!("{capability:?}").contains("opaque-session-capability"));
    }

    #[test]
    fn session_credential_rejects_missing_profile_identity() {
        let json = r#"{
            "session_id":"session-profile-7",
            "opaque_capability":"opaque-session-capability",
            "expires_at":"2026-07-29T01:00:00Z",
            "profile_id":"",
            "profile_revision":0
        }"#;
        let error = resolve_session_credential(true, None, &mut json.as_bytes())
            .expect_err("profile identity is mandatory");
        assert!(matches!(error, SandboxInitError::InvalidSessionCredential));
    }

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
        const READ_DEADLINE: Duration = Duration::from_millis(500);
        const CHUNK_PAUSE: Duration = Duration::from_millis(300);

        let chunks = vec![vec![1; 17], vec![2; 19], vec![3; 23]];
        let expected = chunks.iter().flatten().copied().collect::<Vec<_>>();
        let server = TestServer::start(
            vec![TestResponse::ProgressingExport {
                chunks,
                // Two pauses make the total transfer exceed READ_DEADLINE,
                // while each individual pause leaves enough scheduling margin
                // to remain below it on loaded CI runners.
                pause: CHUNK_PAUSE,
            }],
            false,
        );
        let client = SandboxHttpClient::new_with_read_timeout(&server.api_url, READ_DEADLINE)
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
            started.elapsed() > READ_DEADLINE,
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
    fn workspace_profile_session_creation_accepts_exact_created_status() {
        let session = WorkspaceProfileSession {
            session_id: SessionId::new("session-profile-created"),
            opaque_capability: "ephemeral-session-secret".to_string(),
            expires_at: "2026-07-29T08:00:00Z".to_string(),
            profile_id: "00000000-0000-0000-0000-000000000007".to_string(),
            profile_revision: 9,
        };
        let server = TestServer::start(
            vec![TestResponse::Json {
                status: "201 Created",
                body: serde_json::to_vec(&session).expect("serialize profile session"),
            }],
            true,
        );
        let client = SandboxHttpClient::new(&server.api_url).expect("HTTP client");
        let profile_key = SandboxProfileKey::new("a".repeat(64)).expect("profile key");

        assert_eq!(
            client
                .create_workspace_profile_session(&profile_key)
                .expect("created profile session"),
            SessionCapability {
                session_id: session.session_id,
                opaque_capability: session.opaque_capability,
                expires_at: session.expires_at,
            }
        );
        let requests = server.finish();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/v1/workspace-profile-sessions");
        assert_eq!(requests[0].body, Vec::<u8>::new());
        let expected_authorization = format!("Bearer {}", "a".repeat(64));
        assert_eq!(
            requests[0].headers.get("authorization").map(String::as_str),
            Some(expected_authorization.as_str())
        );
    }

    #[test]
    fn workspace_profile_negotiation_sends_generation2_capabilities() {
        let server = TestServer::start(
            vec![TestResponse::Json {
                status: "201 Created",
                body: locality_protocol::workspace_api_v2::WORKSPACE_PROFILE_SESSION_V2_GOLDEN_JSON
                    .to_vec(),
            }],
            true,
        );
        let client = SandboxHttpClient::new(&server.api_url).expect("HTTP client");
        let profile_key = SandboxProfileKey::new("a".repeat(64)).expect("profile key");

        let negotiated = client
            .create_workspace_profile_session_negotiated(&profile_key)
            .expect("generation-2 profile session");
        assert!(matches!(
            negotiated,
            WorkspaceProfileNegotiation::Generation2 { .. }
        ));
        let requests = server.finish();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/v2/workspace-profile-sessions");
        let request = WorkspaceProfileSessionRequestV2::decode_json(&requests[0].body)
            .expect("strict generation-2 request");
        assert_eq!(request.api_generation(), 2);
        assert!(
            request
                .capabilities()
                .supports_tar_encoding(TarContentEncoding::Zstd)
        );
    }

    #[test]
    fn workspace_profile_negotiation_falls_back_only_after_no_route() {
        let session = WorkspaceProfileSession {
            session_id: SessionId::new("session-profile-fallback"),
            opaque_capability: "ephemeral-session-secret".to_string(),
            expires_at: "2026-07-29T08:00:00Z".to_string(),
            profile_id: "00000000-0000-0000-0000-000000000007".to_string(),
            profile_revision: 9,
        };
        let server = TestServer::start(
            vec![
                TestResponse::Json {
                    status: "404 Not Found",
                    body: Vec::new(),
                },
                TestResponse::Json {
                    status: "201 Created",
                    body: serde_json::to_vec(&session).expect("serialize profile session"),
                },
            ],
            true,
        );
        let client = SandboxHttpClient::new(&server.api_url).expect("HTTP client");
        let profile_key = SandboxProfileKey::new("a".repeat(64)).expect("profile key");

        assert!(matches!(
            client
                .create_workspace_profile_session_negotiated(&profile_key)
                .expect("generation-1 fallback"),
            WorkspaceProfileNegotiation::Generation1(_)
        ));
        let requests = server.finish();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].path, "/v2/workspace-profile-sessions");
        assert!(!requests[0].body.is_empty());
        assert_eq!(requests[1].path, "/v1/workspace-profile-sessions");
        assert!(requests[1].body.is_empty());
    }

    #[test]
    fn generation2_client_invokes_bound_status_and_export_routes() {
        let mut offer_json: serde_json::Value = serde_json::from_slice(
            locality_protocol::workspace_api_v2::WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON,
        )
        .expect("golden offer JSON");
        offer_json["offer"]["content_encoding"] = serde_json::json!("identity");
        let server = TestServer::start(
            vec![
                TestResponse::Json {
                    status: "200 OK",
                    body:
                        locality_protocol::workspace_api_v2::WORKSPACE_SESSION_STATUS_V2_GOLDEN_JSON
                            .to_vec(),
                },
                TestResponse::Json {
                    status: "200 OK",
                    body: serde_json::to_vec(&offer_json).expect("identity offer JSON"),
                },
                TestResponse::ProgressingExport {
                    chunks: vec![Vec::new()],
                    pause: Duration::ZERO,
                },
            ],
            true,
        );
        let client = SandboxHttpClient::new(&server.api_url).expect("HTTP client");
        let session = WorkspaceProfileSessionV2::decode_json(
            locality_protocol::workspace_api_v2::WORKSPACE_PROFILE_SESSION_V2_GOLDEN_JSON,
        )
        .expect("golden profile session");
        let capabilities = WorkspaceClientCapabilitiesV2::workspace_layout_v1(true);
        let status = client
            .workspace_session_status(&session, &capabilities)
            .expect("generation-2 status");
        let capability = SessionCapability {
            session_id: session.session_id().clone(),
            opaque_capability: session.opaque_capability().to_string(),
            expires_at: session.expires_at().to_string(),
        };
        let request = export_attempt_request(
            &capability,
            SandboxContentEncodingPreference::Automatic,
            status.export_attempt_limits().expect("attempt limits"),
        )
        .expect("export request");
        let offer = client
            .create_workspace_export_attempt(&session, &status, &capabilities, &request)
            .expect("generation-2 offer");
        assert_eq!(offer.offer().export_attempt_id.as_str(), "export-attempt-9");
        let _response = client
            .open_workspace_export_attempt(&session, &offer)
            .expect("generation-2 export stream");

        let requests = server.finish();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].path, "/v2/sessions/session-scope-7");
        assert_eq!(
            requests[1].path,
            "/v2/sessions/session-scope-7/export-attempts"
        );
        let decoded: ExportAttemptRequest =
            serde_json::from_slice(&requests[1].body).expect("export request JSON");
        assert_eq!(decoded, request);
        assert_eq!(
            requests[2].path,
            "/v2/sessions/session-scope-7/export-attempts/export-attempt-9/export"
        );
        assert_eq!(
            requests[2]
                .headers
                .get("accept-encoding")
                .map(String::as_str),
            Some("identity")
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
