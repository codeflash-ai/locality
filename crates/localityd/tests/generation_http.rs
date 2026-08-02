use std::collections::BTreeMap;
use std::error::Error as _;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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
    MAX_GENERATION_TRANSPORT_REQUEST_BYTES,
};
use localityd::generation_http::{
    GenerationHttpError, GenerationHttpOperation, GenerationHttpOptions, GenerationHttpRemoteCode,
    GenerationHttpResponseProblem, GenerationHttpRetryClassification, GenerationHttpTransport,
    GenerationHttpTransportFailure,
};
use localityd::generation_sync::{
    GenerationDeliveryRequest as LegacyGenerationDeliveryRequest, GenerationDeliveryTransport,
};
use serde::Deserialize;

const SESSION_ID: &str = "018f4f6e-7b8c-7d9e-8f01-23456789abcd";
const DELTA_ID: &str = "018f4f6e-1111-7222-8333-444444444444";
const CROSSED_DELTA_ID: &str = "018f4f6e-5555-7666-8777-888888888888";
const SESSION_SECRET: &str = "session-secret-never-log-this";
const PROXY_CHILD_TARGET_URL: &str = "LOCALITY_GENERATION_HTTP_TEST_TARGET_URL";
const INVALID_ROUTE_UUIDS: &[&str] = &[
    ".",
    "..",
    "../018f4f6e-1111-7222-8333-444444444444",
    "018f4f6e-1111-7222-8333/444444444444",
    "%2e%2e%2f018f4f6e-1111-7222-8333-444444444444",
    "018F4F6E-1111-7222-8333-444444444444",
    "00000000-0000-0000-0000-000000000000",
    "not-a-uuid",
];

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
}

impl ResponseSpec {
    fn json(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type: Some("application/json"),
            content_length: Some(body.len()),
            headers: Vec::new(),
            body,
        }
    }

    fn window(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            content_type: Some(GENERATION_BODY_WINDOW_CONTENT_TYPE),
            content_length: Some(body.len()),
            headers: Vec::new(),
            body,
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

struct WithholdingServer {
    url: String,
    accepted: mpsc::Receiver<CapturedRequest>,
    release: mpsc::Sender<()>,
    handle: JoinHandle<()>,
}

impl WithholdingServer {
    fn start(expected_requests: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind withholding server");
        let address = listener.local_addr().expect("withholding server address");
        let (accepted_tx, accepted) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let mut withheld = Vec::with_capacity(expected_requests);
            for _ in 0..expected_requests {
                let (mut stream, _) = listener.accept().expect("accept timeout request");
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .expect("timeout request read deadline");
                let request = read_request(&mut stream);
                accepted_tx
                    .send(request)
                    .expect("report accepted timeout request");
                withheld.push(stream);
            }
            release_rx
                .recv()
                .expect("test releases withheld connections");
            drop(withheld);
        });
        Self {
            url: format!("http://{address}"),
            accepted,
            release,
            handle,
        }
    }

    fn accepted_request(&self) -> CapturedRequest {
        self.accepted
            .recv_timeout(Duration::from_secs(5))
            .expect("client request accepted before test deadline")
    }

    fn finish(self) {
        self.release.send(()).expect("release withheld connections");
        self.handle.join().expect("withholding server thread");
    }
}

struct ProxyProbe {
    url: String,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    handle: JoinHandle<()>,
}

impl ProxyProbe {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind proxy probe");
        listener
            .set_nonblocking(true)
            .expect("nonblocking proxy probe");
        let address = listener.local_addr().expect("proxy address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(750);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_nonblocking(false)
                            .expect("blocking proxy stream");
                        stream
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .expect("proxy read timeout");
                        captured
                            .lock()
                            .expect("proxy captures")
                            .push(read_request(&mut stream));
                        let body = b"{\"code\":\"unavailable\"}";
                        let response = format!(
                            "HTTP/1.1 502 Bad Gateway\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.write_all(body);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("accept proxy request: {error}"),
                }
            }
        });
        Self {
            url: format!("http://{address}"),
            requests,
            handle,
        }
    }

    fn finish(self) -> Vec<CapturedRequest> {
        self.handle.join().expect("proxy probe thread");
        Arc::try_unwrap(self.requests)
            .expect("only test owns proxy captures")
            .into_inner()
            .expect("proxy captures")
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
    let mut fixtures: PollFixtures =
        serde_json::from_slice(GENERATION_DELIVERY_POLL_V1_GOLDEN_JSON).expect("poll fixtures");
    let delivery = fixtures
        .delivery
        .delivery
        .as_mut()
        .expect("delivery fixture payload");
    delivery.delta.delta_id = DELTA_ID.to_string();
    delivery.terminal_receipt.delta_id = DELTA_ID.to_string();
    delivery.terminal_receipt.delta_sha256 = delivery
        .delta
        .canonical_sha256()
        .expect("UUID-bound delta digest");
    fixtures
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
    let mut request =
        GenerationBodyWindowRequest::decode_json(GENERATION_BODY_WINDOW_REQUEST_V1_GOLDEN_JSON)
            .expect("body-window request fixture");
    request.delta_id = DELTA_ID.to_string();
    request.validate().expect("UUID-bound body request");
    request
}

fn body_metadata() -> GenerationBodyWindowMetadata {
    let mut metadata =
        GenerationBodyWindowMetadata::decode_json(GENERATION_BODY_WINDOW_METADATA_V1_GOLDEN_JSON)
            .expect("body-window metadata fixture");
    metadata.delta_id = DELTA_ID.to_string();
    metadata.validate().expect("UUID-bound body metadata");
    metadata
}

fn acknowledgment_fixture() -> AcknowledgmentFixture {
    let mut fixture: AcknowledgmentFixture =
        serde_json::from_slice(GENERATION_DELIVERY_ACKNOWLEDGMENT_V1_GOLDEN_JSON)
            .expect("acknowledgment fixture");
    fixture.request.delta_id = DELTA_ID.to_string();
    fixture.response.delta_id = DELTA_ID.to_string();
    fixture
        .request
        .validate()
        .expect("UUID-bound acknowledgment");
    fixture
}

fn transport(server: &ScriptedServer) -> GenerationHttpTransport {
    GenerationHttpTransport::new(&server.url, SESSION_ID, SESSION_SECRET)
        .expect("loopback transport")
}

fn raw_frame(metadata: &GenerationBodyWindowMetadata, body: &[u8]) -> Vec<u8> {
    let metadata = serde_json::to_vec(metadata).expect("serialize metadata");
    raw_json_frame(&metadata, body)
}

fn raw_json_frame(metadata: &[u8], body: &[u8]) -> Vec<u8> {
    let mut frame = Vec::new();
    frame.extend_from_slice(&(metadata.len() as u32).to_be_bytes());
    frame.extend_from_slice(metadata);
    frame.extend_from_slice(body);
    frame
}

fn chunked_body(payload: &[u8]) -> Vec<u8> {
    let mut body = format!("{:x}\r\n", payload.len()).into_bytes();
    body.extend_from_slice(payload);
    body.extend_from_slice(b"\r\n0\r\n\r\n");
    body
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

fn assert_response_diagnostics_redact(error: &GenerationHttpError, sentinels: &[&str]) {
    let mut diagnostics = format!("display: {error}\ndebug: {error:?}");
    let mut source = error.source();
    while let Some(error) = source {
        diagnostics.push_str(&format!(
            "\nsource display: {error}\nsource debug: {error:?}"
        ));
        source = error.source();
    }
    for sentinel in sentinels {
        assert!(
            !diagnostics.contains(sentinel),
            "response sentinel leaked into diagnostics: {diagnostics}"
        );
    }
}

#[test]
fn session_and_local_delta_routes_require_canonical_non_nil_uuids() {
    for invalid in INVALID_ROUTE_UUIDS {
        let error = GenerationHttpTransport::new("http://127.0.0.1:9", *invalid, SESSION_SECRET)
            .expect_err("invalid session route identity");
        assert_eq!(
            error,
            GenerationHttpError::InvalidConfiguration(
                "session ID must be a canonical lowercase hyphenated non-nil UUID"
            ),
            "session ID {invalid:?}"
        );

        let mut transport =
            GenerationHttpTransport::new("http://127.0.0.1:9", SESSION_ID, SESSION_SECRET).unwrap();
        let mut window = body_request();
        window.delta_id = (*invalid).to_string();
        assert_eq!(
            transport
                .open_content_window(&window)
                .err()
                .expect("invalid window route identity"),
            GenerationHttpError::RequestContract(
                GenerationTransportContractError::InvalidOpaqueValue("delta_id")
            ),
            "window delta ID {invalid:?}"
        );

        let mut acknowledgment = acknowledgment_fixture().request;
        acknowledgment.delta_id = (*invalid).to_string();
        assert_eq!(
            transport.acknowledge_terminal_receipt(&acknowledgment),
            Err(GenerationHttpError::RequestContract(
                GenerationTransportContractError::InvalidOpaqueValue("delta_id")
            )),
            "acknowledgment delta ID {invalid:?}"
        );
    }
}

#[test]
fn poll_rejects_noncanonical_server_delta_ids_without_echo_or_route_collapse() {
    let request = delivery_request();
    let responses = INVALID_ROUTE_UUIDS
        .iter()
        .map(|invalid| {
            let mut poll = poll_fixtures().delivery;
            poll.selected_capabilities = request.capabilities.clone();
            let delivery = poll.delivery.as_mut().expect("delivery payload");
            delivery.delta.delta_id = (*invalid).to_string();
            delivery.terminal_receipt.delta_id = (*invalid).to_string();
            delivery.terminal_receipt.delta_sha256 = delivery
                .delta
                .canonical_sha256()
                .expect("generic protocol delta remains internally valid");
            ResponseSpec::json(200, serde_json::to_vec(&poll).unwrap())
        })
        .collect();
    let server = ScriptedServer::start(responses);
    let mut transport = transport(&server);

    for invalid in INVALID_ROUTE_UUIDS {
        let error = transport
            .next_delta_poll(&request)
            .expect_err("noncanonical server delta ID");
        assert!(matches!(
            &error,
            GenerationHttpError::InvalidResponse {
                operation: GenerationHttpOperation::Poll,
                problem: GenerationHttpResponseProblem::ProtocolViolation,
                retry: GenerationHttpRetryClassification::Never,
                ..
            }
        ));
        assert_response_diagnostics_redact(&error, &[*invalid]);
    }

    let requests = server.finish();
    assert_eq!(requests.len(), INVALID_ROUTE_UUIDS.len());
    for request in &requests {
        assert_authenticated_request(
            request,
            "/v2/sessions/018f4f6e-7b8c-7d9e-8f01-23456789abcd/generation-deliveries",
        );
    }
}

#[test]
fn window_rejects_noncanonical_server_delta_ids_without_echo_or_route_collapse() {
    let request = body_request();
    let responses = INVALID_ROUTE_UUIDS
        .iter()
        .map(|invalid| {
            let mut metadata = body_metadata();
            metadata.delta_id = (*invalid).to_string();
            ResponseSpec::window(raw_frame(&metadata, b"hello wo"))
        })
        .collect();
    let server = ScriptedServer::start(responses);
    let mut transport = transport(&server);

    for invalid in INVALID_ROUTE_UUIDS {
        let error = transport
            .open_content_window(&request)
            .err()
            .expect("noncanonical server window delta ID");
        assert!(matches!(
            &error,
            GenerationHttpError::InvalidResponse {
                operation: GenerationHttpOperation::BodyWindow,
                problem: GenerationHttpResponseProblem::CorrelationMismatch,
                retry: GenerationHttpRetryClassification::Never,
                ..
            }
        ));
        assert_response_diagnostics_redact(&error, &[*invalid]);
    }

    let requests = server.finish();
    assert_eq!(requests.len(), INVALID_ROUTE_UUIDS.len());
    for captured in &requests {
        assert_authenticated_request(
            captured,
            "/v2/sessions/018f4f6e-7b8c-7d9e-8f01-23456789abcd/generation-deliveries/018f4f6e-1111-7222-8333-444444444444/body-windows",
        );
    }
}

#[test]
fn acknowledgment_rejects_noncanonical_server_delta_ids_without_echo_or_route_collapse() {
    let fixture = acknowledgment_fixture();
    let responses = INVALID_ROUTE_UUIDS
        .iter()
        .map(|invalid| {
            let mut response = fixture.response.clone();
            response.delta_id = (*invalid).to_string();
            ResponseSpec::json(200, serde_json::to_vec(&response).unwrap())
        })
        .collect();
    let server = ScriptedServer::start(responses);
    let mut transport = transport(&server);

    for invalid in INVALID_ROUTE_UUIDS {
        let error = transport
            .acknowledge_terminal_receipt(&fixture.request)
            .expect_err("noncanonical server acknowledgment delta ID");
        assert!(matches!(
            &error,
            GenerationHttpError::InvalidResponse {
                operation: GenerationHttpOperation::Acknowledgment,
                problem: GenerationHttpResponseProblem::CorrelationMismatch,
                retry: GenerationHttpRetryClassification::Never,
                ..
            }
        ));
        assert_response_diagnostics_redact(&error, &[*invalid]);
    }

    let requests = server.finish();
    assert_eq!(requests.len(), INVALID_ROUTE_UUIDS.len());
    for captured in &requests {
        assert_authenticated_request(
            captured,
            "/v2/sessions/018f4f6e-7b8c-7d9e-8f01-23456789abcd/generation-deliveries/018f4f6e-1111-7222-8333-444444444444/acknowledgments",
        );
    }
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
        DELTA_ID
    );

    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    for captured in &requests {
        assert_authenticated_request(
            captured,
            "/v2/sessions/018f4f6e-7b8c-7d9e-8f01-23456789abcd/generation-deliveries",
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
        &error,
        GenerationHttpError::InvalidResponse {
            problem: GenerationHttpResponseProblem::InvalidJson,
            ..
        }
    ));
    malformed.finish();

    let oversized = ScriptedServer::start(vec![ResponseSpec {
        status: 200,
        content_type: Some("application/json"),
        content_length: Some(MAX_GENERATION_DELIVERY_POLL_RESPONSE_BYTES + 1),
        headers: Vec::new(),
        body: Vec::new(),
    }]);
    let error = transport(&oversized)
        .next_delta_poll(&request)
        .expect_err("oversized poll");
    assert!(matches!(
        &error,
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
        &error,
        GenerationHttpError::InvalidResponse {
            problem: GenerationHttpResponseProblem::CorrelationMismatch,
            ..
        }
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
fn malformed_poll_response_sentinel_is_absent_from_full_error_chain() {
    const SENTINEL: &str = "poll-response-body-SENTINEL";
    let mut payload = serde_json::to_value(poll_fixtures().no_delivery).unwrap();
    payload["format_version"] = serde_json::Value::String(SENTINEL.to_string());
    let server = ScriptedServer::start(vec![ResponseSpec::json(
        200,
        serde_json::to_vec(&payload).unwrap(),
    )]);
    let error = transport(&server)
        .next_delta_poll(&delivery_request())
        .expect_err("malformed poll response");
    assert!(matches!(
        &error,
        GenerationHttpError::InvalidResponse {
            problem: GenerationHttpResponseProblem::InvalidJson,
            retry: GenerationHttpRetryClassification::Never,
            ..
        }
    ));
    assert_response_diagnostics_redact(&error, &[SENTINEL]);
    assert_eq!(server.finish().len(), 1);
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
        "/v2/sessions/018f4f6e-7b8c-7d9e-8f01-23456789abcd/generation-deliveries/018f4f6e-1111-7222-8333-444444444444/body-windows",
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
        GenerationHttpResponseProblem::IntegrityMismatch,
    );

    let mut extra = valid.clone();
    extra.push(b'!');
    assert_window_contract_error(
        &request,
        ResponseSpec::window(extra),
        GenerationHttpResponseProblem::IntegrityMismatch,
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
        GenerationHttpResponseProblem::IntegrityMismatch,
    );

    let mut crossed_range = body_metadata();
    crossed_range.range.offset = 1;
    let crossed_range = raw_frame(&crossed_range, b"hello wo");
    assert_window_contract_error(
        &request,
        ResponseSpec::window(crossed_range),
        GenerationHttpResponseProblem::CorrelationMismatch,
    );
}

#[test]
fn malformed_window_path_sentinel_is_absent_from_full_error_chain() {
    const TENANT_PATH_SENTINEL: &str = "tenant-private-e\u{301}.md";
    let request = body_request();
    let mut metadata = serde_json::to_value(body_metadata()).unwrap();
    metadata["content"]["logical_path"] =
        serde_json::Value::String(TENANT_PATH_SENTINEL.to_string());
    let metadata = serde_json::to_vec(&metadata).unwrap();
    let response = ResponseSpec::window(raw_json_frame(&metadata, b"hello wo"));
    let error = window_error(&request, response);
    assert!(matches!(
        &error,
        GenerationHttpError::InvalidResponse {
            problem: GenerationHttpResponseProblem::InvalidJson,
            retry: GenerationHttpRetryClassification::Never,
            ..
        }
    ));
    assert_response_diagnostics_redact(&error, &[TENANT_PATH_SENTINEL]);
}

fn assert_window_contract_error(
    request: &GenerationBodyWindowRequest,
    response: ResponseSpec,
    expected: GenerationHttpResponseProblem,
) {
    let error = window_error(request, response);
    match error {
        GenerationHttpError::InvalidResponse {
            problem: actual, ..
        } => assert_eq!(actual, expected),
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
            "/v2/sessions/018f4f6e-7b8c-7d9e-8f01-23456789abcd/generation-deliveries/018f4f6e-1111-7222-8333-444444444444/acknowledgments",
        );
    }
    assert_eq!(requests[0].body, requests[1].body);
}

#[test]
fn acknowledgment_rejects_malformed_crossed_and_oversized_responses() {
    let fixture = acknowledgment_fixture();

    let malformed = ScriptedServer::start(vec![ResponseSpec::json(200, b"{".to_vec())]);
    let error = transport(&malformed)
        .acknowledge_terminal_receipt(&fixture.request)
        .expect_err("malformed acknowledgment");
    assert!(matches!(
        error,
        GenerationHttpError::InvalidResponse {
            problem: GenerationHttpResponseProblem::InvalidJson,
            ..
        }
    ));
    assert_eq!(malformed.finish().len(), 1);

    let mut crossed = fixture.response.clone();
    crossed.delta_id = CROSSED_DELTA_ID.to_string();
    let crossed_server = ScriptedServer::start(vec![ResponseSpec::json(
        200,
        serde_json::to_vec(&crossed).unwrap(),
    )]);
    let error = transport(&crossed_server)
        .acknowledge_terminal_receipt(&fixture.request)
        .expect_err("crossed acknowledgment");
    assert!(matches!(
        error,
        GenerationHttpError::InvalidResponse {
            problem: GenerationHttpResponseProblem::CorrelationMismatch,
            ..
        }
    ));
    assert_eq!(crossed_server.finish().len(), 1);

    let oversized = ScriptedServer::start(vec![ResponseSpec {
        status: 200,
        content_type: Some("application/json"),
        content_length: Some(MAX_GENERATION_TRANSPORT_REQUEST_BYTES + 1),
        headers: Vec::new(),
        body: Vec::new(),
    }]);
    let error = transport(&oversized)
        .acknowledge_terminal_receipt(&fixture.request)
        .expect_err("oversized acknowledgment");
    assert!(matches!(
        error,
        GenerationHttpError::InvalidResponse {
            problem: GenerationHttpResponseProblem::ContentLengthTooLarge,
            ..
        }
    ));
    assert_eq!(oversized.finish().len(), 1);
}

#[test]
fn malformed_acknowledgment_sentinel_is_absent_from_full_error_chain() {
    const SENTINEL: &str = "ack-response-body-SENTINEL";
    let fixture = acknowledgment_fixture();
    let mut payload = serde_json::to_value(&fixture.response).unwrap();
    payload["format_version"] = serde_json::Value::String(SENTINEL.to_string());
    let server = ScriptedServer::start(vec![ResponseSpec::json(
        200,
        serde_json::to_vec(&payload).unwrap(),
    )]);
    let error = transport(&server)
        .acknowledge_terminal_receipt(&fixture.request)
        .expect_err("malformed acknowledgment response");
    assert!(matches!(
        &error,
        GenerationHttpError::InvalidResponse {
            problem: GenerationHttpResponseProblem::InvalidJson,
            retry: GenerationHttpRetryClassification::Never,
            ..
        }
    ));
    assert_response_diagnostics_redact(&error, &[SENTINEL]);
    assert_eq!(server.finish().len(), 1);
}

#[test]
fn contradictory_small_content_length_cannot_bypass_chunked_response_bound() {
    let fixture = acknowledgment_fixture();
    let payload = vec![b' '; MAX_GENERATION_TRANSPORT_REQUEST_BYTES + 1];
    let server = ScriptedServer::start(vec![ResponseSpec {
        status: 200,
        content_type: Some("application/json"),
        content_length: Some(1),
        headers: vec![("Transfer-Encoding", "chunked")],
        body: chunked_body(&payload),
    }]);
    let error = transport(&server)
        .acknowledge_terminal_receipt(&fixture.request)
        .expect_err("chunked response exceeds the hard read bound");
    assert!(matches!(
        error,
        GenerationHttpError::InvalidResponse {
            problem: GenerationHttpResponseProblem::TransferEncodingNotAllowed,
            ..
        }
    ));
    assert_eq!(server.finish().len(), 1);
}

#[test]
fn matching_content_length_cannot_make_transfer_encoding_acceptable() {
    let fixture = acknowledgment_fixture();
    let payload = serde_json::to_vec(&fixture.response).unwrap();
    let server = ScriptedServer::start(vec![ResponseSpec {
        status: 200,
        content_type: Some("application/json"),
        content_length: Some(payload.len()),
        headers: vec![("Transfer-Encoding", "chunked")],
        body: chunked_body(&payload),
    }]);
    let error = transport(&server)
        .acknowledge_terminal_receipt(&fixture.request)
        .expect_err("transfer encoding conflicts with exact framing");
    assert!(matches!(
        error,
        GenerationHttpError::InvalidResponse {
            problem: GenerationHttpResponseProblem::TransferEncodingNotAllowed,
            retry: GenerationHttpRetryClassification::Never,
            ..
        }
    ));
    assert_eq!(server.finish().len(), 1);
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
fn truncated_success_response_retries_with_identical_request_identity() {
    let success = serde_json::to_vec(&poll_fixtures().no_delivery).unwrap();
    let server = ScriptedServer::start(vec![
        ResponseSpec {
            content_length: Some(success.len() + 17),
            ..ResponseSpec::json(200, success.clone())
        },
        ResponseSpec::json(200, success),
    ]);
    let mut transport = transport(&server);
    let poll = transport
        .next_delta_poll(&delivery_request())
        .expect("truncated success retries exactly");
    assert!(poll.delivery.is_none());

    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].path, requests[1].path);
    assert_eq!(requests[0].body, requests[1].body);
    assert_eq!(
        requests[0].headers.get("authorization"),
        requests[1].headers.get("authorization")
    );
}

#[test]
fn timeout_is_retried_boundedly_with_the_same_request() {
    let server = WithholdingServer::start(3);
    let url = server.url.clone();
    let client = thread::spawn(move || {
        let mut transport = GenerationHttpTransport::new_with_options(
            &url,
            SESSION_ID,
            SESSION_SECRET,
            GenerationHttpOptions {
                connect_timeout: Duration::from_millis(100),
                request_timeout: Duration::from_millis(100),
                max_attempts: 3,
            },
        )
        .unwrap();
        transport.next_delta_poll(&delivery_request())
    });
    let requests = (0..3)
        .map(|_| server.accepted_request())
        .collect::<Vec<_>>();
    let error = client
        .join()
        .expect("timeout client thread")
        .expect_err("bounded timeout retries");
    assert!(matches!(
        error,
        GenerationHttpError::Transport {
            failure: GenerationHttpTransportFailure::Timeout,
            retry: GenerationHttpRetryClassification::Transient,
            ..
        }
    ));
    server.finish();
    assert_eq!(requests.len(), 3);
    assert!(requests.windows(2).all(|pair| pair[0].body == pair[1].body));
    assert!(requests.windows(2).all(|pair| pair[0].path == pair[1].path));
    assert!(requests.windows(2).all(|pair| {
        pair[0].headers.get("authorization") == pair[1].headers.get("authorization")
    }));
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
        "http://localhost:8757",
        "http://localhost.:8757",
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
    for url in [
        "http://127.255.0.1:9",
        "http://[::1]:9",
        "https://localhost:8757",
    ] {
        GenerationHttpTransport::new(url, SESSION_ID, SESSION_SECRET)
            .expect("literal loopback HTTP or authenticated HTTPS URL");
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

#[test]
fn plaintext_loopback_never_sends_bearer_through_environment_proxy() {
    let target = ScriptedServer::start(vec![ResponseSpec::json(
        200,
        serde_json::to_vec(&poll_fixtures().no_delivery).unwrap(),
    )]);
    let proxy = ProxyProbe::start();
    let output = Command::new(std::env::current_exe().expect("current test binary"))
        .args([
            "--exact",
            "plaintext_loopback_proxy_child",
            "--ignored",
            "--nocapture",
        ])
        .env(PROXY_CHILD_TARGET_URL, &target.url)
        .env("HTTP_PROXY", &proxy.url)
        .env("http_proxy", &proxy.url)
        .env("ALL_PROXY", &proxy.url)
        .env("all_proxy", &proxy.url)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .output()
        .expect("run isolated proxy child");
    assert!(
        output.status.success(),
        "proxy child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let target_requests = target.finish();
    assert_eq!(target_requests.len(), 1);
    assert_authenticated_request(
        &target_requests[0],
        "/v2/sessions/018f4f6e-7b8c-7d9e-8f01-23456789abcd/generation-deliveries",
    );
    let proxy_requests = proxy.finish();
    assert!(
        proxy_requests.is_empty(),
        "plaintext bearer request reached proxy: {proxy_requests:#?}"
    );
}

#[test]
#[ignore = "subprocess helper for proxy environment isolation"]
fn plaintext_loopback_proxy_child() {
    let target = std::env::var(PROXY_CHILD_TARGET_URL).expect("target URL from parent test");
    let mut transport = GenerationHttpTransport::new(&target, SESSION_ID, SESSION_SECRET)
        .expect("direct plaintext loopback transport");
    let poll = transport
        .next_delta_poll(&delivery_request())
        .expect("direct request bypasses environment proxy");
    assert!(poll.delivery.is_none());
}
