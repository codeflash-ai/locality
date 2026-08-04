use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as _;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use locality_core::portable::{
    ContentVersionId, ExportAttemptId, LogicalPath, ProjectionFileKind, ProjectionId, SessionId,
    SourceAction, SourceConnectionId, SourceGenerationId, SourceScopeId,
};
use locality_core::workspace_layout::{MountTarget, PortableMountId};
use locality_protocol::freshness_delivery::GenerationFileIdentity;
use locality_protocol::generation_baseline::{
    GenerationBaselineMountV1, GenerationBaselineResponseV1, GenerationBaselineSourceV1,
    maximum_encoded_bytes_for_export,
};
use locality_protocol::workspace_api_v2::{
    WorkspaceClientCapabilitiesV2, WorkspaceExportOfferV2, WorkspaceProfileSessionV2,
    WorkspaceSessionStatusV2,
};
use locality_protocol::workspace_export_v2::{
    WorkspaceAuthorizedExportEntryV2, WorkspaceNamespacedInventoryV2,
    WorkspaceScopeSourceAuthorityV2,
};
use locality_protocol::workspace_layout::{
    ProfileMount, ProfileScopeBinding, SessionLayout, WorkspaceLayout, WorkspaceProfileId,
};
use locality_protocol::{
    ExportAttemptLimits, OrderedSourceGeneration, ReplicaFreshnessState, ReplicaFreshnessStatus,
    SCOPE_AUTHORIZED_COMPONENT_VERSIONS, SandboxSessionState, SealedExportOffer,
    StaleSessionBehavior, TarContentEncoding,
};
use localityd::generation_http::{
    GENERATION_BASELINE_CACHE_CONTROL, GENERATION_BASELINE_CONTENT_TYPE,
    GenerationBaselineHttpClient, GenerationHttpError, GenerationHttpOperation,
    GenerationHttpOptions, GenerationHttpRemoteCode, GenerationHttpResponseProblem,
    GenerationHttpRetryClassification, GenerationHttpRuntime, GenerationHttpTransport,
};

const SESSION_ID: &str = "session/shared ?#%ü";
const ATTEMPT_ID: &str = "attempt/shared ?#%ü";
const SESSION_CAPABILITY: &str = "baseline-capability-never-log-this";
const RESPONSE_SENTINEL: &str = "response-secret-never-log-this";

#[derive(Clone)]
struct ResponseSpec {
    status: u16,
    content_type: Option<&'static str>,
    content_length: Option<usize>,
    cache_control: Option<&'static str>,
    headers: Vec<(&'static str, &'static str)>,
    body: Vec<u8>,
}

impl ResponseSpec {
    fn baseline(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            content_type: Some(GENERATION_BASELINE_CONTENT_TYPE),
            content_length: Some(body.len()),
            cache_control: Some(GENERATION_BASELINE_CACHE_CONTROL),
            headers: Vec::new(),
            body,
        }
    }

    fn error(status: u16, code: &str) -> Self {
        let body = serde_json::to_vec(&serde_json::json!({"code": code})).unwrap();
        Self {
            status,
            ..Self::baseline(body)
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
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let handle = thread::spawn(move || {
            for response in responses {
                let mut stream = accept_before(&listener, Instant::now() + Duration::from_secs(3));
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                captured.lock().unwrap().push(read_request(&mut stream));
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
                if let Some(cache_control) = response.cache_control {
                    head.push_str(&format!("Cache-Control: {cache_control}\r\n"));
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
            .unwrap()
            .into_inner()
            .unwrap()
    }
}

fn accept_before(listener: &TcpListener, deadline: Instant) -> TcpStream {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
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
    let head = std::str::from_utf8(&bytes[..header_end]).unwrap();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap().split_whitespace();
    let method = request_line.next().unwrap().to_string();
    let path = request_line.next().unwrap().to_string();
    let mut headers = BTreeMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').unwrap();
        headers.insert(name.to_ascii_lowercase(), value.trim().to_string());
    }
    let content_length = headers
        .get("content-length")
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or(0);
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
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        410 => "Gone",
        426 => "Upgrade Required",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Response",
    }
}

#[derive(Clone)]
struct ExportContext {
    session: WorkspaceProfileSessionV2,
    offer: WorkspaceExportOfferV2,
    inventory: WorkspaceNamespacedInventoryV2,
    baseline: GenerationBaselineResponseV1,
}

fn export_context(session_id: &str, attempt_id: &str, capability: &str) -> ExportContext {
    let profile_id = WorkspaceProfileId::new("018f4f6e-9f2c-7b1a-8c3d-4e5f60718293").unwrap();
    let profile_revision = 7;
    let mount_id = PortableMountId::new("mount-shared").unwrap();
    let workspace = WorkspaceLayout::new(
        profile_id.clone(),
        profile_revision,
        vec![ProfileMount::new(
            mount_id.clone(),
            MountTarget::new("Shared").unwrap(),
        )],
        vec![
            ProfileScopeBinding::new(
                0,
                SourceScopeId::new("scope-drive").unwrap(),
                mount_id.clone(),
            ),
            ProfileScopeBinding::new(
                1,
                SourceScopeId::new("scope-notion").unwrap(),
                mount_id.clone(),
            ),
        ],
    )
    .unwrap();
    let session = WorkspaceProfileSessionV2::new(
        SessionId::new(session_id),
        capability,
        "2026-08-02T12:00:00Z",
        profile_id,
        profile_revision,
        SessionLayout::from_workspace(&workspace).unwrap(),
    )
    .unwrap();
    let capabilities = WorkspaceClientCapabilitiesV2::workspace_layout_v1(true);
    let limits = ExportAttemptLimits {
        max_files: 1,
        max_directories: 2,
        max_content_bytes: 6,
    };
    let status = WorkspaceSessionStatusV2::new(
        &session,
        &capabilities,
        SCOPE_AUTHORIZED_COMPONENT_VERSIONS,
        SandboxSessionState::Ready,
        locality_protocol::FreshnessRequirement {
            max_age_seconds: 300,
            on_stale: StaleSessionBehavior::WaitThenFail,
            wait_timeout_seconds: 30,
        },
        vec![replica("source-drive"), replica("source-notion")],
        Some(limits.clone()),
        None,
        "2026-08-02T11:00:00Z",
    )
    .unwrap();
    let sealed_offer = SealedExportOffer {
        versions: SCOPE_AUTHORIZED_COMPONENT_VERSIONS,
        session_id: session.session_id().clone(),
        export_attempt_id: ExportAttemptId::new(attempt_id).unwrap(),
        source_generations: vec![
            OrderedSourceGeneration {
                ordinal: 0,
                source_connection_id: SourceConnectionId::new("source-drive"),
                source_generation_id: SourceGenerationId::new("generation-drive-44").unwrap(),
            },
            OrderedSourceGeneration {
                ordinal: 1,
                source_connection_id: SourceConnectionId::new("source-notion"),
                source_generation_id: SourceGenerationId::new("generation-notion-109").unwrap(),
            },
        ],
        media_type: "application/x-tar".to_string(),
        content_encoding: TarContentEncoding::Zstd,
        limits,
        control_entry_count: 1,
        file_count: 1,
        directory_count: 1,
        archive_entry_count: 3,
        selected_content_bytes: 6,
        inventory_sha256: format!("sha256:{}", "0".repeat(64)),
        writable_metadata_sha256: format!("sha256:{}", "1".repeat(64)),
        sealed_at: "2026-08-02T11:00:01Z".to_string(),
        expires_at: "2026-08-02T11:10:01Z".to_string(),
    };
    let placeholder =
        WorkspaceExportOfferV2::new(&session, &status, &capabilities, sealed_offer.clone())
            .unwrap();
    let inventory = WorkspaceNamespacedInventoryV2::plan(
        session.session_layout(),
        &placeholder,
        &[
            WorkspaceScopeSourceAuthorityV2::new(0, SourceConnectionId::new("source-drive")),
            WorkspaceScopeSourceAuthorityV2::new(1, SourceConnectionId::new("source-notion")),
        ],
        &[WorkspaceAuthorizedExportEntryV2::File {
            winning_scope_ordinal: 1,
            mount_id: mount_id.clone(),
            logical_path: "notion.md".to_string(),
            projection_id: ProjectionId::new("projection-notion"),
            source_connection_id: SourceConnectionId::new("source-notion"),
            file_kind: ProjectionFileKind::Markdown,
            effective_actions: BTreeSet::from([SourceAction::Read]),
            content_sha256: format!("sha256:{}", "3".repeat(64)),
            byte_length: 6,
        }],
    )
    .unwrap();
    let mut final_offer = sealed_offer;
    final_offer.inventory_sha256 = inventory.inventory_sha256().to_string();
    let offer = WorkspaceExportOfferV2::new(&session, &status, &capabilities, final_offer).unwrap();
    inventory
        .validate_against_export(session.session_layout(), &offer)
        .unwrap();
    let baseline = GenerationBaselineResponseV1::from_export(
        &session,
        &offer,
        &inventory,
        vec![
            GenerationBaselineMountV1::new(
                mount_id,
                vec![
                    baseline_source("source-drive", "generation-drive-44", vec![]),
                    baseline_source(
                        "source-notion",
                        "generation-notion-109",
                        vec![GenerationFileIdentity {
                            projection_id: ProjectionId::new("projection-notion"),
                            logical_path: LogicalPath::new("notion.md").unwrap(),
                            content_version_id: ContentVersionId::new("content-notion-v1"),
                            content_sha256: format!("sha256:{}", "3".repeat(64)),
                            byte_length: 6,
                        }],
                    ),
                ],
            )
            .unwrap(),
        ],
    )
    .unwrap();
    ExportContext {
        session,
        offer,
        inventory,
        baseline,
    }
}

fn replica(source: &str) -> ReplicaFreshnessStatus {
    ReplicaFreshnessStatus {
        source_connection_id: SourceConnectionId::new(source),
        state: ReplicaFreshnessState::Fresh,
        coverage_complete: true,
        provider_observed_through: Some("checkpoint-1".to_string()),
        last_successful_sync_at: Some("2026-08-02T10:58:00Z".to_string()),
        last_repair_at: None,
        pending_events: 0,
        backlog: 0,
        provider_cooldown_until: None,
    }
}

fn baseline_source(
    source: &str,
    generation: &str,
    files: Vec<GenerationFileIdentity>,
) -> GenerationBaselineSourceV1 {
    GenerationBaselineSourceV1::new(
        SourceConnectionId::new(source),
        SourceGenerationId::new(generation).unwrap(),
        files,
    )
    .unwrap()
}

fn fetch_error(context: &ExportContext, response: ResponseSpec) -> GenerationHttpError {
    let server = ScriptedServer::start(vec![response]);
    let error = GenerationBaselineHttpClient::new(&server.url, &context.session)
        .unwrap()
        .fetch_generation_baseline(&context.session, &context.offer, &context.inventory)
        .expect_err("invalid baseline response");
    assert_eq!(server.finish().len(), 1);
    error
}

fn diagnostics(error: &GenerationHttpError) -> String {
    let mut output = format!("display: {error}\ndebug: {error:?}");
    let mut source = error.source();
    while let Some(error) = source {
        output.push_str(&format!(
            "\nsource display: {error}\nsource debug: {error:?}"
        ));
        source = error.source();
    }
    output
}

#[test]
fn runtime_composes_baseline_and_delivery_from_one_redacted_capability() {
    let context = export_context(
        "018f4f6e-7b8c-7d9e-8f01-23456789abcd",
        ATTEMPT_ID,
        SESSION_CAPABILITY,
    );
    let mut runtime = GenerationHttpRuntime::new("http://127.0.0.1:9", &context.session).unwrap();

    let _: &GenerationBaselineHttpClient = runtime.baseline_client();
    let _: &GenerationHttpTransport = runtime.delivery_transport();
    let _: &mut GenerationHttpTransport = runtime.delivery_transport_mut();
    assert!(!format!("{runtime:?}").contains(SESSION_CAPABILITY));

    let (baseline, delivery) = runtime.into_parts();
    assert!(!format!("{baseline:?}{delivery:?}").contains(SESSION_CAPABILITY));
}

#[test]
fn shared_mount_baseline_uses_exact_encoded_route_and_required_headers() {
    let context = export_context(SESSION_ID, ATTEMPT_ID, SESSION_CAPABILITY);
    let body = serde_json::to_vec(&context.baseline).unwrap();
    let server = ScriptedServer::start(vec![ResponseSpec::baseline(body)]);
    let client = GenerationBaselineHttpClient::new(&server.url, &context.session).unwrap();
    assert!(!format!("{client:?}").contains(SESSION_CAPABILITY));

    let baseline = client
        .fetch_generation_baseline(&context.session, &context.offer, &context.inventory)
        .expect("bound shared-mount baseline");
    assert_eq!(baseline, context.baseline);
    assert_eq!(baseline.mounts()[0].sources().len(), 2);
    assert!(baseline.mounts()[0].sources()[0].files().is_empty());

    let requests = server.finish();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "GET");
    assert_eq!(
        request.path,
        "/v2/sessions/session%2Fshared%20%3F%23%25%C3%BC/export-attempts/attempt%2Fshared%20%3F%23%25%C3%BC/generation-baseline"
    );
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer baseline-capability-never-log-this")
    );
    assert_eq!(
        request.headers.get("accept").map(String::as_str),
        Some(GENERATION_BASELINE_CONTENT_TYPE)
    );
    assert_eq!(
        request.headers.get("cache-control").map(String::as_str),
        Some(GENERATION_BASELINE_CACHE_CONTROL)
    );
    assert!(!request.headers.contains_key("content-type"));
    assert!(request.body.is_empty());
    assert!(!request.path.contains(SESSION_CAPABILITY));
}

#[test]
fn response_is_bound_to_the_exact_session_and_attempt() {
    let expected = export_context(SESSION_ID, ATTEMPT_ID, SESSION_CAPABILITY);
    let crossed = [
        export_context(SESSION_ID, "crossed-attempt", SESSION_CAPABILITY),
        export_context("crossed-session", ATTEMPT_ID, "crossed-capability"),
    ];
    for body_context in crossed {
        let error = fetch_error(
            &expected,
            ResponseSpec::baseline(serde_json::to_vec(&body_context.baseline).unwrap()),
        );
        assert!(matches!(
            error,
            GenerationHttpError::InvalidResponse {
                operation: GenerationHttpOperation::Baseline,
                problem: GenerationHttpResponseProblem::CorrelationMismatch,
                retry: GenerationHttpRetryClassification::Never,
                ..
            }
        ));
    }
}

#[test]
fn client_rejects_a_different_session_capability_before_network_access() {
    let expected = export_context(SESSION_ID, ATTEMPT_ID, SESSION_CAPABILITY);
    let crossed = export_context(SESSION_ID, ATTEMPT_ID, "different-capability-never-log");
    let client =
        GenerationBaselineHttpClient::new("http://127.0.0.1:9", &expected.session).unwrap();
    let error = client
        .fetch_generation_baseline(&crossed.session, &crossed.offer, &crossed.inventory)
        .expect_err("crossed session capability");
    assert_eq!(error, GenerationHttpError::InvalidBaselineContext);
    let output = diagnostics(&error);
    assert!(!output.contains(SESSION_CAPABILITY));
    assert!(!output.contains("different-capability-never-log"));
}

#[test]
fn client_rejects_opaque_dot_segment_ids_before_network_access() {
    for session_id in [".", ".."] {
        let context = export_context(session_id, ATTEMPT_ID, SESSION_CAPABILITY);
        let error = GenerationBaselineHttpClient::new("http://127.0.0.1:9", &context.session)
            .expect_err("dot-segment session ID must not become a route");
        assert_eq!(error, GenerationHttpError::InvalidBaselineContext);
    }

    let expected = export_context(SESSION_ID, ATTEMPT_ID, SESSION_CAPABILITY);
    let client =
        GenerationBaselineHttpClient::new("http://127.0.0.1:9", &expected.session).unwrap();
    for attempt_id in [".", ".."] {
        let context = export_context(SESSION_ID, attempt_id, SESSION_CAPABILITY);
        let error = client
            .fetch_generation_baseline(&context.session, &context.offer, &context.inventory)
            .expect_err("dot-segment attempt ID must not become a route");
        assert_eq!(error, GenerationHttpError::InvalidBaselineContext);
    }
}

#[test]
fn terminal_statuses_are_typed_sanitized_and_not_retried() {
    let context = export_context(SESSION_ID, ATTEMPT_ID, SESSION_CAPABILITY);
    for (status, code) in [
        (401, GenerationHttpRemoteCode::Unauthorized),
        (403, GenerationHttpRemoteCode::Forbidden),
        (404, GenerationHttpRemoteCode::NotFound),
        (409, GenerationHttpRemoteCode::Conflict),
        (410, GenerationHttpRemoteCode::Gone),
        (426, GenerationHttpRemoteCode::NeedsUpdate),
    ] {
        let error = fetch_error(&context, ResponseSpec::error(status, RESPONSE_SENTINEL));
        assert!(matches!(
            error,
            GenerationHttpError::RemoteHttp {
                operation: GenerationHttpOperation::Baseline,
                status: actual_status,
                code: actual_code,
                retry: GenerationHttpRetryClassification::Never,
            } if actual_status == status && actual_code == code
        ));
        let output = diagnostics(&error);
        assert!(!output.contains(RESPONSE_SENTINEL));
        assert!(!output.contains(SESSION_CAPABILITY));
    }
}

#[test]
fn response_content_type_and_no_store_policy_are_mandatory() {
    let context = export_context(SESSION_ID, ATTEMPT_ID, SESSION_CAPABILITY);
    let body = serde_json::to_vec(&context.baseline).unwrap();
    let mut wrong_type = ResponseSpec::baseline(body.clone());
    wrong_type.content_type = Some("application/octet-stream");
    let mut missing_cache = ResponseSpec::baseline(body.clone());
    missing_cache.cache_control = None;
    let mut cacheable = ResponseSpec::baseline(body);
    cacheable.cache_control = Some("public, max-age=3600");

    for (response, expected) in [
        (
            wrong_type,
            GenerationHttpResponseProblem::InvalidContentType,
        ),
        (
            missing_cache,
            GenerationHttpResponseProblem::InvalidCacheControl,
        ),
        (
            cacheable,
            GenerationHttpResponseProblem::InvalidCacheControl,
        ),
    ] {
        let error = fetch_error(&context, response);
        assert!(matches!(
            error,
            GenerationHttpError::InvalidResponse {
                operation: GenerationHttpOperation::Baseline,
                problem,
                retry: GenerationHttpRetryClassification::Never,
                ..
            } if problem == expected
        ));
    }
}

#[test]
fn oversized_malformed_update_required_and_tampered_bodies_fail_typed_and_redacted() {
    let context = export_context(SESSION_ID, ATTEMPT_ID, SESSION_CAPABILITY);
    let maximum =
        maximum_encoded_bytes_for_export(&context.session, &context.offer, &context.inventory)
            .unwrap();
    let oversized = ResponseSpec {
        content_length: Some(maximum + 1),
        body: Vec::new(),
        ..ResponseSpec::baseline(Vec::new())
    };
    let error = fetch_error(&context, oversized);
    assert!(matches!(
        error,
        GenerationHttpError::InvalidResponse {
            problem: GenerationHttpResponseProblem::ContentLengthTooLarge,
            ..
        }
    ));

    let malformed =
        ResponseSpec::baseline(format!("{{\"reflected\":\"{RESPONSE_SENTINEL}\"").into_bytes());
    let error = fetch_error(&context, malformed);
    assert!(matches!(
        error,
        GenerationHttpError::InvalidResponse {
            problem: GenerationHttpResponseProblem::InvalidJson,
            ..
        }
    ));
    assert!(!diagnostics(&error).contains(RESPONSE_SENTINEL));

    let mut update_required = serde_json::to_value(&context.baseline).unwrap();
    update_required["format_version"] = serde_json::json!(2);
    update_required["minimum_reader_version"] = serde_json::json!(2);
    let error = fetch_error(
        &context,
        ResponseSpec::baseline(serde_json::to_vec(&update_required).unwrap()),
    );
    assert!(matches!(
        error,
        GenerationHttpError::InvalidResponse {
            problem: GenerationHttpResponseProblem::UnsupportedVersion,
            ..
        }
    ));

    let mut tampered = serde_json::to_value(&context.baseline).unwrap();
    tampered["mounts"][0]["sources"][1]["files"][0]["content_version_id"] =
        serde_json::json!(RESPONSE_SENTINEL);
    let error = fetch_error(
        &context,
        ResponseSpec::baseline(serde_json::to_vec(&tampered).unwrap()),
    );
    assert!(matches!(
        error,
        GenerationHttpError::InvalidResponse {
            problem: GenerationHttpResponseProblem::IntegrityMismatch,
            ..
        }
    ));
    assert!(!diagnostics(&error).contains(RESPONSE_SENTINEL));
}

#[test]
fn safe_get_retries_transient_status_and_truncated_framing_with_identical_identity() {
    let context = export_context(SESSION_ID, ATTEMPT_ID, SESSION_CAPABILITY);
    let body = serde_json::to_vec(&context.baseline).unwrap();
    let unavailable = ResponseSpec::error(503, "unavailable");
    let truncated = ResponseSpec {
        content_length: Some(body.len() + 1),
        body: body.clone(),
        ..ResponseSpec::baseline(body.clone())
    };
    let server = ScriptedServer::start(vec![unavailable, truncated, ResponseSpec::baseline(body)]);
    let client = GenerationBaselineHttpClient::new_with_options(
        &server.url,
        &context.session,
        GenerationHttpOptions {
            max_attempts: 3,
            ..GenerationHttpOptions::default()
        },
    )
    .unwrap();
    client
        .fetch_generation_baseline(&context.session, &context.offer, &context.inventory)
        .expect("safe GET retry succeeds");

    let requests = server.finish();
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|request| request.method == "GET"));
    assert!(requests.iter().all(|request| request.body.is_empty()));
    assert!(requests.windows(2).all(|pair| pair[0].path == pair[1].path));
    assert!(requests.windows(2).all(|pair| {
        pair[0].headers.get("authorization") == pair[1].headers.get("authorization")
            && pair[0].headers.get("accept") == pair[1].headers.get("accept")
            && pair[0].headers.get("cache-control") == pair[1].headers.get("cache-control")
    }));
}
