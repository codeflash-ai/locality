use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use loc_cli::generation_http::{
    GenerationHttpError, GenerationHttpOperation, GenerationHttpOptions, GenerationHttpRemoteCode,
    GenerationHttpResponseProblem, GenerationHttpRetryClassification, GenerationHttpTransport,
    GenerationHttpTransportFailure,
};
use locality_core::model::MountId;
use locality_core::portable::{SourceConnectionId, SourceGenerationId};
use locality_protocol::freshness_delivery::{FreshnessReasonCode, FreshnessRetryClass};
use locality_protocol::freshness_delivery_transport::{
    GENERATION_BODY_WINDOW_CONTENT_TYPE, GENERATION_BODY_WINDOW_METADATA_V1_GOLDEN_JSON,
    GENERATION_BODY_WINDOW_REQUEST_V1_GOLDEN_JSON,
    GENERATION_DELIVERY_ACKNOWLEDGMENT_V1_GOLDEN_JSON, GENERATION_DELIVERY_POLL_V1_GOLDEN_JSON,
    GENERATION_DELIVERY_REQUEST_V1_GOLDEN_JSON, GenerationBodyWindowFrame,
    GenerationBodyWindowMetadata, GenerationBodyWindowRequest, GenerationDeliveryAcknowledgment,
    GenerationDeliveryAcknowledgmentRequest, GenerationDeliveryAcknowledgmentStatus,
    GenerationDeliveryPollResponse, GenerationDeliveryRequest, GenerationTransportContractError,
    MAX_GENERATION_BODY_WINDOW_BYTES, MAX_GENERATION_DELIVERY_POLL_RESPONSE_BYTES,
};
use localityd::generation_sync::{
    GenerationDeliveryRequest as LegacyGenerationDeliveryRequest, GenerationDeliveryTransport,
};
use serde::Deserialize;

const SESSION_ID: &str = "session-test-01";
const SESSION_SECRET: &str = "session-secret-never-log-this";

#[derive(Clone, Deserialize)]
struct PollFixtures {
    delivery: GenerationDeliveryPollResponse,
    no_delivery: GenerationDeliveryPollResponse,
    error: GenerationDeliveryPollResponse,
}

#[derive(Clone, Deserialize)]
struct AcknowledgmentFixture {
    request: GenerationDeliveryAcknowledgmentRequest,
    response: GenerationDeliveryAcknowledgment,
}

#[derive(Clone)]
struct ResponseSpec {
    status: u16,
    content_type: Option<&'static str>,
    content_length: Option<usize>,
    headers: Vec<(&'static str, &'static str)>,
    body: Vec<u8>,
    delay: Duration,
}

impl ResponseSpec {
    fn json(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type: Some("application/json"),
            content_length: Some(body.len()),
            headers: Vec::new(),
            body,
            delay: Duration::ZERO,
        }
    }

    fn window(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            content_type: Some(GENERATION_BODY_WINDOW_CONTENT_TYPE),
            content_length: Some(body.len()),
            headers: Vec::new(),
            body,
            delay: Duration::ZERO,
        }
    }
}

#[derive(Clone, Debug)]
struct CapturedRequest {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

struct ScriptedServer {
    url: String,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    handle: JoinHandle<()>,
}

impl ScriptedServer {
    fn start(responses: Vec<ResponseSpec>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback server");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let address = listener.local_addr().expect("server address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let handle = thread::spawn(move || {
            for response in responses {
                let mut stream = accept_before(&listener, Instant::now() + Duration::from_secs(3));
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .expect("request read timeout");
                captured
                    .lock()
                    .expect("captured requests")
                    .push(read_request(&mut stream));
                if !response.delay.is_zero() {
                    thread::sleep(response.delay);
                }
                let mut head = format!(
                    "HTTP/1.1 {} {}\r\nConnection: close\r\n",
                    response.status,
                    reason_phrase(response.status)
                );
                if let Some(content_type) = response.content_type {
                    head.push_str(&format!("Content-Type: {content_type}\r\n"));
                }
                if let Some(content_length) = response.content_length {
                    head.push_str(&format!("Content-Length: {content_length}\r\n"));
                }
                for (name, value) in response.headers {
                    head.push_str(&format!("{name}: {value}\r\n"));
                }
                head.push_str("\r\n");
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&response.body);
                let _ = stream.flush();
            }
        });
        Self {
            url: format!("http://{address}"),
            requests,
            handle,
        }
    }

    fn finish(self) -> Vec<CapturedRequest> {
        self.handle.join().expect("loopback server thread");
        Arc::try_unwrap(self.requests)
            .expect("only test owns request captures")
            .into_inner()
            .expect("captured requests")
    }
}

fn accept_before(listener: &TcpListener, deadline: Instant) -> TcpStream {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .expect("blocking accepted stream");
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "timed out waiting for request");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("accept request: {error}"),
        }
    }
}

fn read_request(stream: &mut TcpStream) -> CapturedRequest {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).expect("read request headers");
        assert!(read > 0, "request ended before headers");
        bytes.extend_from_slice(&chunk[..read]);
        assert!(bytes.len() <= 64 * 1024, "request headers are bounded");
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let head = std::str::from_utf8(&bytes[..header_end]).expect("request headers are UTF-8");
    let mut lines = head.split("\r\n");
    let request_line = lines.next().expect("request line");
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().expect("request method").to_string();
    let path = request_parts.next().expect("request path").to_string();
    let mut headers = BTreeMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').expect("request header");
        headers.insert(name.to_ascii_lowercase(), value.trim().to_string());
    }
    let content_length = headers
        .get("content-length")
        .expect("request content length")
        .parse::<usize>()
        .expect("numeric request content length");
    while bytes.len() < header_end + content_length {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).expect("read request body");
        assert!(read > 0, "request body truncated");
        bytes.extend_from_slice(&chunk[..read]);
    }
    CapturedRequest {
        method,
        path,
        headers,
        body: bytes[header_end..header_end + content_length].to_vec(),
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        409 => "Conflict",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Response",
    }
}

fn poll_fixtures() -> PollFixtures {
    serde_json::from_slice(GENERATION_DELIVERY_POLL_V1_GOLDEN_JSON).expect("poll fixtures")
}

fn delivery_request() -> GenerationDeliveryRequest {
    let mut request =
        GenerationDeliveryRequest::decode_json(GENERATION_DELIVERY_REQUEST_V1_GOLDEN_JSON)
            .expect("delivery request fixture");
    let transport = GenerationHttpTransport::new("http://127.0.0.1:9", SESSION_ID, SESSION_SECRET)
        .expect("test transport");
    request.capabilities = transport.capabilities();
    request
}

fn body_request() -> GenerationBodyWindowRequest {
    GenerationBodyWindowRequest::decode_json(GENERATION_BODY_WINDOW_REQUEST_V1_GOLDEN_JSON)
        .expect("body-window request fixture")
}

fn body_metadata() -> GenerationBodyWindowMetadata {
    GenerationBodyWindowMetadata::decode_json(GENERATION_BODY_WINDOW_METADATA_V1_GOLDEN_JSON)
        .expect("body-window metadata fixture")
}

fn acknowledgment_fixture() -> AcknowledgmentFixture {
    serde_json::from_slice(GENERATION_DELIVERY_ACKNOWLEDGMENT_V1_GOLDEN_JSON)
        .expect("acknowledgment fixture")
}

fn transport(server: &ScriptedServer) -> GenerationHttpTransport {
    GenerationHttpTransport::new(&server.url, SESSION_ID, SESSION_SECRET)
        .expect("loopback transport")
}

fn raw_frame(metadata: &GenerationBodyWindowMetadata, body: &[u8]) -> Vec<u8> {
    let metadata = serde_json::to_vec(metadata).expect("serialize metadata");
    let mut frame = Vec::new();
    frame.extend_from_slice(&(metadata.len() as u32).to_be_bytes());
    frame.extend_from_slice(&metadata);
    frame.extend_from_slice(body);
    frame
}

fn assert_authenticated_request(request: &CapturedRequest, expected_path: &str) {
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, expected_path);
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer session-secret-never-log-this")
    );
    assert_eq!(
        request.headers.get("content-type").map(String::as_str),
        Some("application/json")
    );
}

#[test]
fn polls_no_delivery_and_delivery_with_the_exact_authenticated_route() {
    let mut fixtures = poll_fixtures();
    let request = delivery_request();
    fixtures.delivery.selected_capabilities = request.capabilities.clone();
    let server = ScriptedServer::start(vec![
        ResponseSpec::json(200, serde_json::to_vec(&fixtures.no_delivery).unwrap()),
        ResponseSpec::json(200, serde_json::to_vec(&fixtures.delivery).unwrap()),
    ]);
    let mut transport = transport(&server);

    let empty = transport.next_delta_poll(&request).expect("no delivery");
    assert!(empty.delivery.is_none());
    let delivery = transport.next_delta_poll(&request).expect("delivery");
    assert_eq!(
        delivery.delivery.expect("delivery payload").delta.delta_id,
        "delta-poll-v1"
    );

    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    for captured in &requests {
        assert_authenticated_request(
            captured,
            "/v2/sessions/session-test-01/generation-deliveries",
        );
        let sent = GenerationDeliveryRequest::decode_json(&captured.body).expect("sent poll");
        assert_eq!(sent, request);
        assert_eq!(
            sent.capabilities.body_windows.unwrap().max_window_bytes,
            MAX_GENERATION_BODY_WINDOW_BYTES
        );
        assert!(sent.capabilities.terminal_receipt_acknowledgments);
        assert!(sent.capabilities.generation_pin_leases.is_none());
    }
}

#[test]
fn poll_rejects_malformed_oversized_and_crossed_responses() {
    let request = delivery_request();

    let malformed = ScriptedServer::start(vec![ResponseSpec::json(200, b"{".to_vec())]);
    let error = transport(&malformed)
        .next_delta_poll(&request)
        .expect_err("malformed JSON");
    assert!(matches!(
        error,
        GenerationHttpError::RequestContract(GenerationTransportContractError::InvalidJson(_))
    ));
    malformed.finish();

    let oversized = ScriptedServer::start(vec![ResponseSpec {
        status: 200,
        content_type: Some("application/json"),
        content_length: Some(MAX_GENERATION_DELIVERY_POLL_RESPONSE_BYTES + 1),
        headers: Vec::new(),
        body: Vec::new(),
        delay: Duration::ZERO,
    }]);
    let error = transport(&oversized)
        .next_delta_poll(&request)
        .expect_err("oversized poll");
    assert!(matches!(
        error,
        GenerationHttpError::InvalidResponse {
            problem: GenerationHttpResponseProblem::ContentLengthTooLarge,
            ..
        }
    ));
    oversized.finish();

    let mut crossed = poll_fixtures().no_delivery;
    crossed.observed_generation_id = SourceGenerationId::new("generation-crossed").unwrap();
    let crossed_server = ScriptedServer::start(vec![ResponseSpec::json(
        200,
        serde_json::to_vec(&crossed).unwrap(),
    )]);
    let error = transport(&crossed_server)
        .next_delta_poll(&request)
        .expect_err("crossed poll");
    assert!(matches!(
        error,
        GenerationHttpError::RequestContract(
            GenerationTransportContractError::PollResponseMismatch
        )
    ));
    crossed_server.finish();

    let legacy_delivery = poll_fixtures().delivery;
    let legacy_server = ScriptedServer::start(vec![ResponseSpec::json(
        200,
        serde_json::to_vec(&legacy_delivery).unwrap(),
    )]);
    let error = transport(&legacy_server)
        .next_delta_poll(&request)
        .expect_err("whole-body delivery selection");
    assert!(matches!(
        error,
        GenerationHttpError::InvalidResponse {
            problem: GenerationHttpResponseProblem::MissingRequiredCapability,
            ..
        }
    ));
    legacy_server.finish();
}

#[test]
fn poll_preserves_structured_remote_reason_and_retry_advice() {
    let fixture = poll_fixtures().error;
    let server = ScriptedServer::start(vec![ResponseSpec::json(
        200,
        serde_json::to_vec(&fixture).unwrap(),
    )]);
    let error = transport(&server)
        .next_delta_poll(&delivery_request())
        .expect_err("remote poll error");
    match error {
        GenerationHttpError::RemotePoll { reason, retry } => {
            assert_eq!(reason, FreshnessReasonCode::ProviderUnavailable);
            let retry = retry.expect("retry advice");
            assert_eq!(retry.class, FreshnessRetryClass::AfterDelay);
            assert_eq!(retry.retry_after_seconds, Some(30));
        }
        other => panic!("unexpected error: {other:?}"),
    }
    server.finish();
}

#[test]
fn body_window_accepts_valid_frame_and_rejects_truncation_extra_media_digest_and_range() {
    let request = body_request();
    let valid = GenerationBodyWindowFrame {
        metadata: body_metadata(),
        body: b"hello wo".to_vec(),
    }
    .encode_http_body(&request)
    .expect("valid body frame");

    let server = ScriptedServer::start(vec![ResponseSpec::window(valid.clone())]);
    let mut transport = transport(&server);
    let mut window = transport
        .open_content_window(&request)
        .expect("window request")
        .expect("window response");
    let mut body = Vec::new();
    window.body.read_to_end(&mut body).expect("window body");
    assert_eq!(body, b"hello wo");
    let requests = server.finish();
    assert_authenticated_request(
        &requests[0],
        "/v2/sessions/session-test-01/generation-deliveries/delta-018f4f6e/body-windows",
    );
    assert_eq!(
        GenerationBodyWindowRequest::decode_json(&requests[0].body).unwrap(),
        request
    );

    let mut truncated = valid.clone();
    truncated.pop();
    assert_window_contract_error(
        &request,
        ResponseSpec::window(truncated),
        GenerationTransportContractError::BodyIntegrityMismatch,
    );

    let mut extra = valid.clone();
    extra.push(b'!');
    assert_window_contract_error(
        &request,
        ResponseSpec::window(extra),
        GenerationTransportContractError::BodyIntegrityMismatch,
    );

    let mut wrong_media = ResponseSpec::window(valid.clone());
    wrong_media.content_type = Some("application/octet-stream");
    let error = window_error(&request, wrong_media);
    assert!(matches!(
        error,
        GenerationHttpError::InvalidResponse {
            problem: GenerationHttpResponseProblem::InvalidContentType,
            ..
        }
    ));

    let mut missing_length = ResponseSpec::window(valid.clone());
    missing_length.content_length = None;
    let error = window_error(&request, missing_length);
    assert!(matches!(
        error,
        GenerationHttpError::InvalidResponse {
            problem: GenerationHttpResponseProblem::MissingContentLength,
            ..
        }
    ));

    let mut corrupt = valid;
    *corrupt.last_mut().unwrap() ^= 1;
    assert_window_contract_error(
        &request,
        ResponseSpec::window(corrupt),
        GenerationTransportContractError::BodyIntegrityMismatch,
    );

    let mut crossed_range = body_metadata();
    crossed_range.range.offset = 1;
    let crossed_range = raw_frame(&crossed_range, b"hello wo");
    assert_window_contract_error(
        &request,
        ResponseSpec::window(crossed_range),
        GenerationTransportContractError::BodyWindowMismatch,
    );
}

fn assert_window_contract_error(
    request: &GenerationBodyWindowRequest,
    response: ResponseSpec,
    expected: GenerationTransportContractError,
) {
    let error = window_error(request, response);
    match error {
        GenerationHttpError::RequestContract(actual) => assert_eq!(actual, expected),
        other => panic!("unexpected window error: {other:?}"),
    }
}

fn window_error(
    request: &GenerationBodyWindowRequest,
    response: ResponseSpec,
) -> GenerationHttpError {
    let server = ScriptedServer::start(vec![response]);
    let error = transport(&server)
        .open_content_window(request)
        .err()
        .expect("invalid window");
    server.finish();
    error
}

#[test]
fn body_window_enforces_request_and_response_bounds_before_allocation() {
    let request = body_request();
    let server = ScriptedServer::start(vec![ResponseSpec {
        status: 200,
        content_type: Some(GENERATION_BODY_WINDOW_CONTENT_TYPE),
        content_length: Some(32 * 1024),
        headers: Vec::new(),
        body: Vec::new(),
        delay: Duration::ZERO,
    }]);
    let error = transport(&server)
        .open_content_window(&request)
        .err()
        .expect("oversized bounded window");
    assert!(matches!(
        error,
        GenerationHttpError::InvalidResponse {
            problem: GenerationHttpResponseProblem::ContentLengthTooLarge,
            ..
        }
    ));
    server.finish();

    let mut invalid_request = request;
    invalid_request.max_bytes = MAX_GENERATION_BODY_WINDOW_BYTES + 1;
    let mut transport =
        GenerationHttpTransport::new("http://127.0.0.1:9", SESSION_ID, SESSION_SECRET).unwrap();
    assert!(matches!(
        transport.open_content_window(&invalid_request),
        Err(GenerationHttpError::RequestContract(
            GenerationTransportContractError::InvalidBodyWindowLimit { .. }
        ))
    ));
}

#[test]
fn acknowledgments_accept_new_and_exact_replay_responses() {
    let fixture = acknowledgment_fixture();
    let mut replay = fixture.response.clone();
    replay.status = GenerationDeliveryAcknowledgmentStatus::AlreadyAccepted;
    let server = ScriptedServer::start(vec![
        ResponseSpec::json(200, serde_json::to_vec(&fixture.response).unwrap()),
        ResponseSpec::json(200, serde_json::to_vec(&replay).unwrap()),
    ]);
    let mut transport = transport(&server);

    let accepted = transport
        .acknowledge_terminal_receipt(&fixture.request)
        .expect("accepted acknowledgment")
        .expect("acknowledgment response");
    assert_eq!(
        accepted.status,
        GenerationDeliveryAcknowledgmentStatus::Accepted
    );
    let replayed = transport
        .acknowledge_terminal_receipt(&fixture.request)
        .expect("replayed acknowledgment")
        .expect("replay response");
    assert_eq!(
        replayed.status,
        GenerationDeliveryAcknowledgmentStatus::AlreadyAccepted
    );

    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_authenticated_request(
            request,
            "/v2/sessions/session-test-01/generation-deliveries/delta-018f4f6e/acknowledgments",
        );
    }
    assert_eq!(requests[0].body, requests[1].body);
}

#[test]
fn retries_only_transient_statuses_with_identical_request_identity() {
    let unavailable = serde_json::to_vec(&serde_json::json!({"code": "unavailable"})).unwrap();
    let success = serde_json::to_vec(&poll_fixtures().no_delivery).unwrap();
    let server = ScriptedServer::start(vec![
        ResponseSpec::json(502, unavailable.clone()),
        ResponseSpec::json(503, unavailable.clone()),
        ResponseSpec::json(504, unavailable),
        ResponseSpec::json(200, success),
    ]);
    let mut transport = GenerationHttpTransport::new_with_options(
        &server.url,
        SESSION_ID,
        SESSION_SECRET,
        GenerationHttpOptions {
            max_attempts: 4,
            ..GenerationHttpOptions::default()
        },
    )
    .unwrap();
    transport
        .next_delta_poll(&delivery_request())
        .expect("transient status retries");
    let requests = server.finish();
    assert_eq!(requests.len(), 4);
    for request in &requests[1..] {
        assert_eq!(request.path, requests[0].path);
        assert_eq!(request.body, requests[0].body);
        assert_eq!(
            request.headers.get("authorization"),
            requests[0].headers.get("authorization")
        );
    }
}

#[test]
fn timeout_is_retried_boundedly_with_the_same_request() {
    let body = serde_json::to_vec(&poll_fixtures().no_delivery).unwrap();
    let delayed = ResponseSpec {
        delay: Duration::from_millis(60),
        ..ResponseSpec::json(200, body)
    };
    let server = ScriptedServer::start(vec![delayed.clone(), delayed.clone(), delayed]);
    let mut transport = GenerationHttpTransport::new_with_options(
        &server.url,
        SESSION_ID,
        SESSION_SECRET,
        GenerationHttpOptions {
            connect_timeout: Duration::from_millis(20),
            request_timeout: Duration::from_millis(20),
            max_attempts: 3,
        },
    )
    .unwrap();
    let error = transport
        .next_delta_poll(&delivery_request())
        .expect_err("bounded timeout retries");
    assert!(matches!(
        error,
        GenerationHttpError::Transport {
            failure: GenerationHttpTransportFailure::Timeout,
            retry: GenerationHttpRetryClassification::Transient,
            ..
        }
    ));
    let requests = server.finish();
    assert_eq!(requests.len(), 3);
    assert!(requests.windows(2).all(|pair| pair[0].body == pair[1].body));
}

#[test]
fn bad_request_unauthorized_and_conflict_are_never_blindly_retried() {
    for (status, code, expected) in [
        (
            400,
            "invalid_request",
            GenerationHttpRemoteCode::InvalidRequest,
        ),
        (401, "unauthorized", GenerationHttpRemoteCode::Unauthorized),
        (409, "stale", GenerationHttpRemoteCode::Stale),
    ] {
        let server = ScriptedServer::start(vec![ResponseSpec::json(
            status,
            serde_json::to_vec(&serde_json::json!({"code": code})).unwrap(),
        )]);
        let error = transport(&server)
            .next_delta_poll(&delivery_request())
            .expect_err("terminal HTTP status");
        assert!(matches!(
            error,
            GenerationHttpError::RemoteHttp {
                code,
                retry: GenerationHttpRetryClassification::Never,
                ..
            } if code == expected
        ));
        assert_eq!(server.finish().len(), 1);
    }
}

#[test]
fn urls_redirects_legacy_methods_and_diagnostics_fail_closed_without_secrets() {
    for url in [
        "http://example.com",
        "https://user:url-secret@example.com",
        "https://example.com?token=url-secret",
        "https://example.com/#url-secret",
        "https://example.com/api",
    ] {
        let error = GenerationHttpTransport::new(url, SESSION_ID, SESSION_SECRET)
            .expect_err("invalid base URL");
        let diagnostic = format!("{error:?} {error}");
        assert!(!diagnostic.contains(SESSION_SECRET));
        assert!(!diagnostic.contains("url-secret"));
    }

    let redirect = ScriptedServer::start(vec![ResponseSpec {
        headers: vec![("Location", "http://127.0.0.1:1/secret")],
        ..ResponseSpec::json(302, b"{\"code\":\"unauthorized\"}".to_vec())
    }]);
    let mut transport = transport(&redirect);
    let debug = format!("{transport:?}");
    assert!(!debug.contains(SESSION_SECRET));
    assert!(!debug.contains(SESSION_ID));
    let error = transport
        .next_delta_poll(&delivery_request())
        .expect_err("redirect is not followed");
    assert!(matches!(
        error,
        GenerationHttpError::RemoteHttp { status: 302, .. }
    ));
    let diagnostic = format!("{error:?} {error}");
    assert!(!diagnostic.contains(SESSION_SECRET));
    assert_eq!(redirect.finish().len(), 1);

    let legacy_request = LegacyGenerationDeliveryRequest {
        mount_id: MountId::new("mount-alpha"),
        source_connection_id: SourceConnectionId::new("source-018f4f6e"),
        observed_generation_id: SourceGenerationId::new("generation-0007").unwrap(),
    };
    assert!(matches!(
        transport.next_delta(&legacy_request),
        Err(GenerationHttpError::UnsupportedOperation(
            GenerationHttpOperation::LegacyNextDelta
        ))
    ));
    assert!(matches!(
        transport.open_content("delta", &body_request().content),
        Err(GenerationHttpError::UnsupportedOperation(
            GenerationHttpOperation::LegacyOpenContent
        ))
    ));
}
