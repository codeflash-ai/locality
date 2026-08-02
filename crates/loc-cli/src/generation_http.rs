//! Authenticated HTTP adapter for the private generation-delivery v2 routes.
//!
//! The hosted service owns route implementation and authorization. This module
//! only maps those fixed routes onto the public generation-delivery transport
//! contract and validates every complete response before exposing it locally.

use std::fmt::{Debug, Display, Formatter};
use std::io::{Cursor, Read};
use std::net::{IpAddr, Ipv6Addr};
use std::sync::OnceLock;
use std::time::Duration;

use locality_protocol::freshness_delivery::{
    FreshnessReasonCode, FreshnessRetry, GenerationFileIdentity,
};
use locality_protocol::freshness_delivery_transport::{
    GENERATION_BODY_WINDOW_CONTENT_TYPE, GENERATION_DELIVERY_POLL_CONTENT_TYPE,
    GENERATION_TRANSPORT_FORMAT_VERSION, GENERATION_TRANSPORT_READER_VERSION,
    GenerationBodyWindowCapability, GenerationBodyWindowFrame, GenerationBodyWindowRequest,
    GenerationDeliveryAcknowledgment, GenerationDeliveryAcknowledgmentRequest,
    GenerationDeliveryPollResponse, GenerationDeliveryPollStatus,
    GenerationDeliveryRequest as VersionedGenerationDeliveryRequest,
    GenerationTransportCapabilities, GenerationTransportContractError,
    MAX_GENERATION_BODY_WINDOW_BYTES, MAX_GENERATION_BODY_WINDOW_METADATA_BYTES,
    MAX_GENERATION_DELIVERY_POLL_RESPONSE_BYTES, MAX_GENERATION_TRANSPORT_REQUEST_BYTES,
};
use localityd::generation_sync::{
    AuthorizedGenerationBodyWindow, AuthorizedGenerationDelivery, AuthorizedGenerationDeliveryPoll,
    GenerationDeliveryRequest, GenerationDeliveryTransport,
};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, HeaderValue, TRANSFER_ENCODING,
};
use serde::Deserialize;

const USER_AGENT: &str = "locality-generation-delivery/1";
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_ATTEMPTS: u8 = 3;
const MAX_ATTEMPTS: u8 = 10;
const MAX_SESSION_ID_BYTES: usize = 128;
const JSON_MEDIA_TYPE: &str = "application/json";
const BODY_WINDOW_FRAME_OVERHEAD: usize =
    std::mem::size_of::<u32>() + MAX_GENERATION_BODY_WINDOW_METADATA_BYTES;

static REQWEST_CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();

/// Bounded retry policy for one exact generation-delivery request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GenerationHttpOptions {
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub max_attempts: u8,
}

impl Default for GenerationHttpOptions {
    fn default() -> Self {
        Self {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GenerationHttpOperation {
    Poll,
    BodyWindow,
    Acknowledgment,
    LegacyNextDelta,
    LegacyOpenContent,
}

impl Display for GenerationHttpOperation {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Poll => "generation delivery poll",
            Self::BodyWindow => "generation body window",
            Self::Acknowledgment => "generation delivery acknowledgment",
            Self::LegacyNextDelta => "legacy whole-body next delta",
            Self::LegacyOpenContent => "legacy whole-body content",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GenerationHttpRetryClassification {
    Never,
    Transient,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GenerationHttpTransportFailure {
    Connect,
    Timeout,
    Request,
    ResponseBody,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GenerationHttpResponseProblem {
    TransferEncodingNotAllowed,
    MissingContentLength,
    MultipleContentLengths,
    InvalidContentLength,
    ContentLengthTooLarge,
    ContentLengthMismatch,
    InvalidContentType,
    InvalidJson,
    UnsupportedVersion,
    CorrelationMismatch,
    IntegrityMismatch,
    ProtocolViolation,
    MissingRequiredCapability,
}

/// Stable private-service error codes. Unknown values remain classified but
/// are never copied into diagnostics, preventing reflected secrets or text
/// from reaching logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GenerationHttpRemoteCode {
    InvalidRequest,
    Unauthorized,
    UnsupportedEncoding,
    NeedsUpdate,
    DeadlineExceeded,
    Unavailable,
    Bootstrapping,
    Stale,
    Incomplete,
    Expired,
    LimitExceeded,
    Unknown,
}

impl GenerationHttpRemoteCode {
    fn parse(value: &str) -> Self {
        match value {
            "invalid_request" => Self::InvalidRequest,
            "unauthorized" => Self::Unauthorized,
            "unsupported_encoding" => Self::UnsupportedEncoding,
            "needs_update" => Self::NeedsUpdate,
            "deadline_exceeded" => Self::DeadlineExceeded,
            "unavailable" => Self::Unavailable,
            "bootstrapping" => Self::Bootstrapping,
            "stale" => Self::Stale,
            "incomplete" => Self::Incomplete,
            "expired" => Self::Expired,
            "limit_exceeded" => Self::LimitExceeded,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum GenerationHttpError {
    InvalidConfiguration(&'static str),
    UnsupportedOperation(GenerationHttpOperation),
    RequestContract(GenerationTransportContractError),
    Transport {
        operation: GenerationHttpOperation,
        failure: GenerationHttpTransportFailure,
        retry: GenerationHttpRetryClassification,
    },
    InvalidResponse {
        operation: GenerationHttpOperation,
        status: Option<u16>,
        problem: GenerationHttpResponseProblem,
        retry: GenerationHttpRetryClassification,
    },
    RemoteHttp {
        operation: GenerationHttpOperation,
        status: u16,
        code: GenerationHttpRemoteCode,
        retry: GenerationHttpRetryClassification,
    },
    RemotePoll {
        reason: FreshnessReasonCode,
        retry: Option<FreshnessRetry>,
    },
}

impl GenerationHttpError {
    pub const fn retry_classification(&self) -> GenerationHttpRetryClassification {
        match self {
            Self::Transport { retry, .. }
            | Self::InvalidResponse { retry, .. }
            | Self::RemoteHttp { retry, .. } => *retry,
            Self::InvalidConfiguration(_)
            | Self::UnsupportedOperation(_)
            | Self::RequestContract(_)
            | Self::RemotePoll { .. } => GenerationHttpRetryClassification::Never,
        }
    }
}

impl Display for GenerationHttpError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfiguration(detail) => {
                write!(formatter, "invalid generation HTTP configuration: {detail}")
            }
            Self::UnsupportedOperation(operation) => {
                write!(
                    formatter,
                    "{operation} is unsupported by the windowed HTTP transport"
                )
            }
            Self::RequestContract(error) => {
                write!(formatter, "invalid generation request: {error}")
            }
            Self::Transport {
                operation, failure, ..
            } => write!(formatter, "{operation} transport failure: {failure:?}"),
            Self::InvalidResponse {
                operation,
                status,
                problem,
                ..
            } => {
                if let Some(status) = status {
                    write!(
                        formatter,
                        "{operation} returned invalid HTTP {status} response: {problem:?}"
                    )
                } else {
                    write!(
                        formatter,
                        "{operation} returned an invalid response: {problem:?}"
                    )
                }
            }
            Self::RemoteHttp {
                operation,
                status,
                code,
                ..
            } => write!(formatter, "{operation} failed with HTTP {status}: {code:?}"),
            Self::RemotePoll { reason, retry } => {
                write!(
                    formatter,
                    "generation delivery poll failed: {reason:?} ({retry:?})"
                )
            }
        }
    }
}

impl std::error::Error for GenerationHttpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::RequestContract(error) => Some(error),
            _ => None,
        }
    }
}

struct SessionSecret(HeaderValue);

impl Debug for SessionSecret {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SessionSecret(<redacted>)")
    }
}

/// Blocking authenticated adapter for the private generation-delivery v2 API.
pub struct GenerationHttpTransport {
    client: Client,
    base_url: reqwest::Url,
    session_id: String,
    session_secret: SessionSecret,
    capabilities: GenerationTransportCapabilities,
    max_attempts: u8,
}

impl Debug for GenerationHttpTransport {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GenerationHttpTransport")
            .field("base_url", &self.base_url)
            .field("session_id", &"<redacted>")
            .field("session_secret", &self.session_secret)
            .field("capabilities", &self.capabilities)
            .field("max_attempts", &self.max_attempts)
            .finish_non_exhaustive()
    }
}

impl GenerationHttpTransport {
    pub fn new(
        base_url: &str,
        session_id: impl Into<String>,
        session_secret: impl Into<String>,
    ) -> Result<Self, GenerationHttpError> {
        Self::new_with_options(
            base_url,
            session_id,
            session_secret,
            GenerationHttpOptions::default(),
        )
    }

    pub fn new_with_options(
        base_url: &str,
        session_id: impl Into<String>,
        session_secret: impl Into<String>,
        options: GenerationHttpOptions,
    ) -> Result<Self, GenerationHttpError> {
        validate_options(options)?;
        let base_url = parse_base_url(base_url)?;
        let session_id = session_id.into();
        validate_opaque(&session_id, MAX_SESSION_ID_BYTES, "session ID")?;
        let session_secret = session_secret.into();
        validate_secret(&session_secret)?;
        let mut authorization = HeaderValue::from_str(&format!("Bearer {session_secret}"))
            .map_err(|_| GenerationHttpError::InvalidConfiguration("invalid session secret"))?;
        authorization.set_sensitive(true);

        REQWEST_CRYPTO_PROVIDER.get_or_init(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
        let mut client = Client::builder()
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(options.connect_timeout)
            .timeout(options.request_timeout);
        if base_url.scheme() == "http" {
            // Plaintext bearer authentication is permitted only to a literal
            // loopback address and must never traverse environment or system
            // proxy configuration. HTTPS retains the platform proxy policy.
            client = client.no_proxy();
        }
        let client = client
            .build()
            .map_err(|_| GenerationHttpError::InvalidConfiguration("HTTP client setup failed"))?;
        let capabilities = GenerationTransportCapabilities {
            format_version: GENERATION_TRANSPORT_FORMAT_VERSION,
            minimum_reader_version: GENERATION_TRANSPORT_READER_VERSION,
            body_windows: Some(GenerationBodyWindowCapability {
                max_window_bytes: MAX_GENERATION_BODY_WINDOW_BYTES,
            }),
            terminal_receipt_acknowledgments: true,
            generation_pin_leases: None,
        };
        capabilities
            .validate()
            .map_err(GenerationHttpError::RequestContract)?;

        Ok(Self {
            client,
            base_url,
            session_id,
            session_secret: SessionSecret(authorization),
            capabilities,
            max_attempts: options.max_attempts,
        })
    }

    fn endpoint(&self, tail: &[&str]) -> reqwest::Url {
        let mut url = self.base_url.clone();
        url.set_path("");
        url.path_segments_mut()
            .expect("validated HTTP URLs support path segments")
            .extend(["v2", "sessions", self.session_id.as_str()])
            .extend(tail);
        url
    }

    fn post_json(
        &self,
        operation: GenerationHttpOperation,
        url: reqwest::Url,
        accept: &'static str,
        request_body: &[u8],
        success_content_type: &'static str,
        success_maximum: usize,
    ) -> Result<WireResponse, GenerationHttpError> {
        for attempt in 1..=self.max_attempts {
            let response = match self
                .client
                .post(url.clone())
                .header(AUTHORIZATION, self.session_secret.0.clone())
                .header(ACCEPT, accept)
                .header(CONTENT_TYPE, JSON_MEDIA_TYPE)
                .body(request_body.to_vec())
                .send()
            {
                Ok(response) => response,
                Err(error) => {
                    let (failure, retry) = classify_transport_error(&error);
                    if retry == GenerationHttpRetryClassification::Transient
                        && attempt < self.max_attempts
                    {
                        continue;
                    }
                    return Err(GenerationHttpError::Transport {
                        operation,
                        failure,
                        retry,
                    });
                }
            };

            let status = response.status();
            let status_retry = classify_status(status);
            if status_retry == GenerationHttpRetryClassification::Transient
                && attempt < self.max_attempts
            {
                continue;
            }
            let (content_type, maximum) = if status == StatusCode::OK {
                (success_content_type, success_maximum)
            } else {
                (JSON_MEDIA_TYPE, MAX_GENERATION_TRANSPORT_REQUEST_BYTES)
            };
            match read_wire_response(response, operation, content_type, maximum, status_retry) {
                Ok(response) => return Ok(response),
                Err(error)
                    if error.retry_classification()
                        == GenerationHttpRetryClassification::Transient
                        && attempt < self.max_attempts =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("validated maximum attempt count is positive")
    }

    fn require_success(
        &self,
        operation: GenerationHttpOperation,
        response: WireResponse,
    ) -> Result<Vec<u8>, GenerationHttpError> {
        if response.status == StatusCode::OK {
            return Ok(response.body);
        }
        let remote: RemoteErrorBody = serde_json::from_slice(&response.body).map_err(|_| {
            GenerationHttpError::InvalidResponse {
                operation,
                status: Some(response.status.as_u16()),
                problem: GenerationHttpResponseProblem::InvalidJson,
                retry: classify_status(response.status),
            }
        })?;
        Err(GenerationHttpError::RemoteHttp {
            operation,
            status: response.status.as_u16(),
            code: GenerationHttpRemoteCode::parse(&remote.code),
            retry: classify_status(response.status),
        })
    }

    fn encode_request<T>(
        request: &T,
        decode: impl FnOnce(&[u8]) -> Result<T, GenerationTransportContractError>,
    ) -> Result<Vec<u8>, GenerationHttpError>
    where
        T: serde::Serialize,
    {
        let body = serde_json::to_vec(request).map_err(|_| {
            GenerationHttpError::InvalidConfiguration("generation request serialization failed")
        })?;
        decode(&body).map_err(GenerationHttpError::RequestContract)?;
        Ok(body)
    }
}

impl GenerationDeliveryTransport for GenerationHttpTransport {
    type Error = GenerationHttpError;

    fn capabilities(&self) -> GenerationTransportCapabilities {
        self.capabilities.clone()
    }

    fn next_delta(
        &mut self,
        _request: &GenerationDeliveryRequest,
    ) -> Result<Option<AuthorizedGenerationDelivery>, Self::Error> {
        Err(GenerationHttpError::UnsupportedOperation(
            GenerationHttpOperation::LegacyNextDelta,
        ))
    }

    fn next_delta_poll(
        &mut self,
        request: &VersionedGenerationDeliveryRequest,
    ) -> Result<AuthorizedGenerationDeliveryPoll, Self::Error> {
        let body = Self::encode_request(request, VersionedGenerationDeliveryRequest::decode_json)?;
        let response = self.post_json(
            GenerationHttpOperation::Poll,
            self.endpoint(&["generation-deliveries"]),
            GENERATION_DELIVERY_POLL_CONTENT_TYPE,
            &body,
            GENERATION_DELIVERY_POLL_CONTENT_TYPE,
            MAX_GENERATION_DELIVERY_POLL_RESPONSE_BYTES,
        )?;
        let response = self.require_success(GenerationHttpOperation::Poll, response)?;
        let poll = GenerationDeliveryPollResponse::decode_json(&response, request)
            .map_err(|error| response_contract_error(GenerationHttpOperation::Poll, error))?;
        match poll.status {
            GenerationDeliveryPollStatus::Delivery => {
                if poll.selected_capabilities.body_windows.is_none()
                    || !poll.selected_capabilities.terminal_receipt_acknowledgments
                {
                    return Err(GenerationHttpError::InvalidResponse {
                        operation: GenerationHttpOperation::Poll,
                        status: Some(StatusCode::OK.as_u16()),
                        problem: GenerationHttpResponseProblem::MissingRequiredCapability,
                        retry: GenerationHttpRetryClassification::Never,
                    });
                }
                let delivery = poll
                    .delivery
                    .expect("validated delivery poll has a delivery payload");
                Ok(AuthorizedGenerationDeliveryPoll {
                    selected_capabilities: poll.selected_capabilities,
                    delivery: Some(AuthorizedGenerationDelivery {
                        delta: delivery.delta,
                        terminal_receipt: delivery.terminal_receipt,
                    }),
                })
            }
            GenerationDeliveryPollStatus::NoDelivery => Ok(AuthorizedGenerationDeliveryPoll {
                selected_capabilities: poll.selected_capabilities,
                delivery: None,
            }),
            GenerationDeliveryPollStatus::Error => {
                let error = poll
                    .error
                    .expect("validated error poll has an error payload");
                Err(GenerationHttpError::RemotePoll {
                    reason: error.reason,
                    retry: error.retry,
                })
            }
            GenerationDeliveryPollStatus::Unknown => {
                unreachable!("the protocol decoder rejects unknown poll statuses")
            }
        }
    }

    fn open_content(
        &mut self,
        _delta_id: &str,
        _identity: &GenerationFileIdentity,
    ) -> Result<Box<dyn Read + Send>, Self::Error> {
        Err(GenerationHttpError::UnsupportedOperation(
            GenerationHttpOperation::LegacyOpenContent,
        ))
    }

    fn open_content_window(
        &mut self,
        request: &GenerationBodyWindowRequest,
    ) -> Result<Option<AuthorizedGenerationBodyWindow>, Self::Error> {
        let body = Self::encode_request(request, GenerationBodyWindowRequest::decode_json)?;
        let maximum = BODY_WINDOW_FRAME_OVERHEAD
            .checked_add(usize::try_from(request.max_bytes).map_err(|_| {
                GenerationHttpError::RequestContract(
                    GenerationTransportContractError::InvalidBodyRange,
                )
            })?)
            .ok_or(GenerationHttpError::RequestContract(
                GenerationTransportContractError::InvalidBodyRange,
            ))?;
        let response = self.post_json(
            GenerationHttpOperation::BodyWindow,
            self.endpoint(&[
                "generation-deliveries",
                request.delta_id.as_str(),
                "body-windows",
            ]),
            GENERATION_BODY_WINDOW_CONTENT_TYPE,
            &body,
            GENERATION_BODY_WINDOW_CONTENT_TYPE,
            maximum,
        )?;
        let response = self.require_success(GenerationHttpOperation::BodyWindow, response)?;
        let frame = GenerationBodyWindowFrame::decode_http_body(
            request,
            GENERATION_BODY_WINDOW_CONTENT_TYPE,
            u64::try_from(response.len()).map_err(|_| {
                response_contract_error(
                    GenerationHttpOperation::BodyWindow,
                    GenerationTransportContractError::InvalidBodyFrame,
                )
            })?,
            &response,
        )
        .map_err(|error| response_contract_error(GenerationHttpOperation::BodyWindow, error))?;
        Ok(Some(AuthorizedGenerationBodyWindow {
            metadata: frame.metadata,
            body: Box::new(Cursor::new(frame.body)),
        }))
    }

    fn acknowledge_terminal_receipt(
        &mut self,
        request: &GenerationDeliveryAcknowledgmentRequest,
    ) -> Result<Option<GenerationDeliveryAcknowledgment>, Self::Error> {
        let body = Self::encode_request(
            request,
            GenerationDeliveryAcknowledgmentRequest::decode_json,
        )?;
        let response = self.post_json(
            GenerationHttpOperation::Acknowledgment,
            self.endpoint(&[
                "generation-deliveries",
                request.delta_id.as_str(),
                "acknowledgments",
            ]),
            JSON_MEDIA_TYPE,
            &body,
            JSON_MEDIA_TYPE,
            MAX_GENERATION_TRANSPORT_REQUEST_BYTES,
        )?;
        let response = self.require_success(GenerationHttpOperation::Acknowledgment, response)?;
        let acknowledgment: GenerationDeliveryAcknowledgment = serde_json::from_slice(&response)
            .map_err(|_| GenerationHttpError::InvalidResponse {
                operation: GenerationHttpOperation::Acknowledgment,
                status: Some(StatusCode::OK.as_u16()),
                problem: GenerationHttpResponseProblem::InvalidJson,
                retry: GenerationHttpRetryClassification::Never,
            })?;
        acknowledgment.validate_against(request).map_err(|error| {
            response_contract_error(GenerationHttpOperation::Acknowledgment, error)
        })?;
        Ok(Some(acknowledgment))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteErrorBody {
    code: String,
}

struct WireResponse {
    status: StatusCode,
    body: Vec<u8>,
}

fn response_contract_error(
    operation: GenerationHttpOperation,
    error: GenerationTransportContractError,
) -> GenerationHttpError {
    let problem = match error {
        GenerationTransportContractError::InvalidJson(_) => {
            GenerationHttpResponseProblem::InvalidJson
        }
        GenerationTransportContractError::EncodingTooLarge { .. } => {
            GenerationHttpResponseProblem::ContentLengthTooLarge
        }
        GenerationTransportContractError::UpdateRequired { .. }
        | GenerationTransportContractError::InvalidVersionEnvelope => {
            GenerationHttpResponseProblem::UnsupportedVersion
        }
        GenerationTransportContractError::InvalidBodyWindowContentType => {
            GenerationHttpResponseProblem::InvalidContentType
        }
        GenerationTransportContractError::BodyContentLengthMismatch => {
            GenerationHttpResponseProblem::ContentLengthMismatch
        }
        GenerationTransportContractError::BodyWindowMismatch
        | GenerationTransportContractError::PollResponseMismatch
        | GenerationTransportContractError::AcknowledgmentMismatch => {
            GenerationHttpResponseProblem::CorrelationMismatch
        }
        GenerationTransportContractError::BodyIntegrityMismatch => {
            GenerationHttpResponseProblem::IntegrityMismatch
        }
        GenerationTransportContractError::CapabilityNotOffered => {
            GenerationHttpResponseProblem::MissingRequiredCapability
        }
        _ => GenerationHttpResponseProblem::ProtocolViolation,
    };
    GenerationHttpError::InvalidResponse {
        operation,
        status: Some(StatusCode::OK.as_u16()),
        problem,
        retry: GenerationHttpRetryClassification::Never,
    }
}

fn validate_options(options: GenerationHttpOptions) -> Result<(), GenerationHttpError> {
    if options.connect_timeout.is_zero() || options.request_timeout.is_zero() {
        return Err(GenerationHttpError::InvalidConfiguration(
            "HTTP timeouts must be positive",
        ));
    }
    if !(1..=MAX_ATTEMPTS).contains(&options.max_attempts) {
        return Err(GenerationHttpError::InvalidConfiguration(
            "HTTP attempts must be between 1 and 10",
        ));
    }
    Ok(())
}

fn parse_base_url(base_url: &str) -> Result<reqwest::Url, GenerationHttpError> {
    let url = reqwest::Url::parse(base_url)
        .map_err(|_| GenerationHttpError::InvalidConfiguration("base URL cannot be parsed"))?;
    let host = url
        .host_str()
        .ok_or(GenerationHttpError::InvalidConfiguration(
            "base URL has no host",
        ))?;
    match url.scheme() {
        "https" => {}
        "http" if is_literal_loopback_host(host) => {}
        "http" => {
            return Err(GenerationHttpError::InvalidConfiguration(
                "HTTP is allowed only for loopback hosts",
            ));
        }
        _ => {
            return Err(GenerationHttpError::InvalidConfiguration(
                "base URL must use HTTPS",
            ));
        }
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(GenerationHttpError::InvalidConfiguration(
            "URL credentials are not allowed",
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(GenerationHttpError::InvalidConfiguration(
            "URL query strings and fragments are not allowed",
        ));
    }
    if !matches!(url.path(), "" | "/") {
        return Err(GenerationHttpError::InvalidConfiguration(
            "base URL paths are not allowed",
        ));
    }
    Ok(url)
}

fn is_literal_loopback_host(host: &str) -> bool {
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(address)) => address.is_loopback(),
        Ok(IpAddr::V6(address)) => address == Ipv6Addr::LOCALHOST,
        Err(_) => false,
    }
}

fn validate_opaque(
    value: &str,
    maximum: usize,
    label: &'static str,
) -> Result<(), GenerationHttpError> {
    if value.is_empty()
        || value.len() > maximum
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        Err(GenerationHttpError::InvalidConfiguration(label))
    } else {
        Ok(())
    }
}

fn validate_secret(secret: &str) -> Result<(), GenerationHttpError> {
    if secret.is_empty()
        || !secret
            .as_bytes()
            .iter()
            .all(|byte| (b'!'..=b'~').contains(byte))
    {
        Err(GenerationHttpError::InvalidConfiguration(
            "invalid session secret",
        ))
    } else {
        Ok(())
    }
}

fn classify_status(status: StatusCode) -> GenerationHttpRetryClassification {
    if matches!(
        status,
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT
    ) {
        GenerationHttpRetryClassification::Transient
    } else {
        GenerationHttpRetryClassification::Never
    }
}

fn classify_transport_error(
    error: &reqwest::Error,
) -> (
    GenerationHttpTransportFailure,
    GenerationHttpRetryClassification,
) {
    if error.is_timeout() {
        (
            GenerationHttpTransportFailure::Timeout,
            GenerationHttpRetryClassification::Transient,
        )
    } else if error.is_connect() {
        (
            GenerationHttpTransportFailure::Connect,
            GenerationHttpRetryClassification::Transient,
        )
    } else if error.is_body() {
        (
            GenerationHttpTransportFailure::ResponseBody,
            GenerationHttpRetryClassification::Never,
        )
    } else {
        (
            GenerationHttpTransportFailure::Request,
            GenerationHttpRetryClassification::Never,
        )
    }
}

fn read_wire_response(
    mut response: reqwest::blocking::Response,
    operation: GenerationHttpOperation,
    expected_content_type: &'static str,
    maximum: usize,
    retry: GenerationHttpRetryClassification,
) -> Result<WireResponse, GenerationHttpError> {
    let status = response.status();
    require_content_type(response.headers(), expected_content_type).map_err(|problem| {
        GenerationHttpError::InvalidResponse {
            operation,
            status: Some(status.as_u16()),
            problem,
            retry,
        }
    })?;
    let content_length =
        require_content_length(response.headers(), maximum).map_err(|problem| {
            GenerationHttpError::InvalidResponse {
                operation,
                status: Some(status.as_u16()),
                problem,
                retry,
            }
        })?;
    let hard_limit = maximum
        .checked_add(1)
        .ok_or(GenerationHttpError::InvalidResponse {
            operation,
            status: Some(status.as_u16()),
            problem: GenerationHttpResponseProblem::ContentLengthTooLarge,
            retry: GenerationHttpRetryClassification::Never,
        })?;
    let mut body = Vec::with_capacity(content_length.min(maximum));
    response
        .by_ref()
        .take(u64::try_from(hard_limit).unwrap_or(u64::MAX))
        .read_to_end(&mut body)
        .map_err(|error| {
            let (failure, transport_retry) = classify_body_read_error(&error);
            GenerationHttpError::Transport {
                operation,
                failure,
                retry: if status == StatusCode::OK {
                    transport_retry
                } else {
                    retry
                },
            }
        })?;
    if body.len() > maximum {
        return Err(GenerationHttpError::InvalidResponse {
            operation,
            status: Some(status.as_u16()),
            problem: GenerationHttpResponseProblem::ContentLengthTooLarge,
            retry: GenerationHttpRetryClassification::Never,
        });
    }
    if body.len() != content_length {
        return Err(GenerationHttpError::InvalidResponse {
            operation,
            status: Some(status.as_u16()),
            problem: GenerationHttpResponseProblem::ContentLengthMismatch,
            retry: if status == StatusCode::OK {
                GenerationHttpRetryClassification::Transient
            } else {
                retry
            },
        });
    }
    Ok(WireResponse { status, body })
}

fn classify_body_read_error(
    error: &std::io::Error,
) -> (
    GenerationHttpTransportFailure,
    GenerationHttpRetryClassification,
) {
    let timed_out = error.kind() == std::io::ErrorKind::TimedOut
        || error
            .get_ref()
            .and_then(|source| source.downcast_ref::<reqwest::Error>())
            .is_some_and(reqwest::Error::is_timeout);
    if timed_out {
        (
            GenerationHttpTransportFailure::Timeout,
            GenerationHttpRetryClassification::Transient,
        )
    } else {
        (
            GenerationHttpTransportFailure::ResponseBody,
            GenerationHttpRetryClassification::Transient,
        )
    }
}

fn require_content_type(
    headers: &HeaderMap,
    expected: &'static str,
) -> Result<(), GenerationHttpResponseProblem> {
    let mut values = headers.get_all(CONTENT_TYPE).iter();
    let value = values
        .next()
        .and_then(|value| value.to_str().ok())
        .ok_or(GenerationHttpResponseProblem::InvalidContentType)?;
    if values.next().is_some() {
        return Err(GenerationHttpResponseProblem::InvalidContentType);
    }
    if value.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(GenerationHttpResponseProblem::InvalidContentType)
    }
}

fn require_content_length(
    headers: &HeaderMap,
    maximum: usize,
) -> Result<usize, GenerationHttpResponseProblem> {
    if headers.contains_key(TRANSFER_ENCODING) {
        return Err(GenerationHttpResponseProblem::TransferEncodingNotAllowed);
    }
    let mut values = headers.get_all(CONTENT_LENGTH).iter();
    let value = values
        .next()
        .ok_or(GenerationHttpResponseProblem::MissingContentLength)?;
    if values.next().is_some() {
        return Err(GenerationHttpResponseProblem::MultipleContentLengths);
    }
    let value = value
        .to_str()
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or(GenerationHttpResponseProblem::InvalidContentLength)?;
    let value =
        usize::try_from(value).map_err(|_| GenerationHttpResponseProblem::ContentLengthTooLarge)?;
    if value > maximum {
        Err(GenerationHttpResponseProblem::ContentLengthTooLarge)
    } else {
        Ok(value)
    }
}
