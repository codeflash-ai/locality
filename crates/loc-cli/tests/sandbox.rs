use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Cursor, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use loc_cli::sandbox::{
    SandboxBootstrapToken, SandboxInitError, SandboxInitOptions, resolve_bootstrap_token,
    run_sandbox_init,
};
use locality_core::portable::SessionId;
use locality_protocol::{
    COMPONENT_VERSIONS, ComponentVersions, FreshnessRequirement, OpaqueBootstrapExchangeRequest,
    SandboxSessionState, SandboxSessionStatus, SessionCapability, SessionErrorCode,
    SessionProtocolError, StaleSessionBehavior, TarContentEncoding, TarExportOffer,
};
use localityd::replica_materializer::ReplicaMaterializationLimits;
use tar::{Builder, EntryType, Header};

static TEST_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = TEST_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "loc-sandbox-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create test directory");
        Self(path)
    }

    fn root(&self) -> PathBuf {
        self.0.join("replica")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        make_removable(&self.0);
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug)]
struct CapturedRequest {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

struct ResponseFixture {
    status: &'static str,
    headers: Vec<(&'static str, &'static str)>,
    body: Vec<u8>,
}

impl ResponseFixture {
    fn json<T: serde::Serialize>(value: &T) -> Self {
        Self {
            status: "200 OK",
            headers: vec![("Content-Type", "application/json")],
            body: serde_json::to_vec(value).expect("serialize response"),
        }
    }

    fn export(encoding: &'static str, body: Vec<u8>) -> Self {
        Self {
            status: "200 OK",
            headers: vec![
                ("Content-Type", "application/x-tar"),
                ("Content-Encoding", encoding),
            ],
            body,
        }
    }
}

struct MockServer {
    api_url: String,
    requests: Receiver<CapturedRequest>,
    handle: Option<JoinHandle<()>>,
}

impl MockServer {
    fn start(responses: Vec<ResponseFixture>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        listener
            .set_nonblocking(true)
            .expect("set mock listener nonblocking");
        let address = listener.local_addr().expect("mock address");
        let (sender, requests) = mpsc::channel();
        let handle = thread::spawn(move || {
            for response in responses {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(std::time::Instant::now() < deadline, "request timed out");
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("accept request: {error}"),
                    }
                };
                let request = read_request(&mut stream);
                sender.send(request).expect("capture request");
                write_response(&mut stream, response);
            }
        });
        Self {
            api_url: format!("http://{address}"),
            requests,
            handle: Some(handle),
        }
    }

    fn request(&self) -> CapturedRequest {
        self.requests
            .recv_timeout(Duration::from_secs(5))
            .expect("receive captured request")
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[test]
fn identity_bootstrap_sends_only_sealed_requests_and_materializes() {
    let directory = TestDirectory::new("identity");
    let tar = tar_file(b"docs/readme.md", b"hello\n");
    let capability = capability();
    let status = ready_status(
        capability.session_id.clone(),
        COMPONENT_VERSIONS,
        tar.len() as u64,
        BTreeSet::from([TarContentEncoding::Identity, TarContentEncoding::Zstd]),
    );
    let server = MockServer::start(vec![
        ResponseFixture::json(&capability),
        ResponseFixture::json(&status),
        ResponseFixture::export("identity", tar),
    ]);
    let token = SandboxBootstrapToken::new("bootstrap-secret").expect("token");

    let report = run_sandbox_init(
        SandboxInitOptions {
            api_url: server.api_url.clone(),
            root: directory.root(),
        },
        token.clone(),
    )
    .expect("bootstrap identity export");

    assert_eq!(
        fs::read(directory.root().join("docs/readme.md")).expect("read replica"),
        b"hello\n"
    );
    assert_eq!(report.content_encoding, "identity");
    assert_eq!(report.entries, 1);
    assert_eq!(report.files, 1);
    assert_eq!(report.materialized_bytes, 6);
    assert!(!format!("{token:?}").contains("bootstrap-secret"));
    assert!(!format!("{report:?}").contains("bootstrap-secret"));
    assert!(!format!("{report:?}").contains("capability-secret"));

    let bootstrap = server.request();
    assert_eq!(bootstrap.method, "POST");
    assert_eq!(bootstrap.path, "/v1/sessions");
    assert_eq!(bootstrap.headers.get("accept").unwrap(), "application/json");
    assert_eq!(
        bootstrap.headers.get("content-type").unwrap(),
        "application/json"
    );
    assert!(!bootstrap.headers.contains_key("authorization"));
    assert_eq!(
        serde_json::from_slice::<OpaqueBootstrapExchangeRequest>(&bootstrap.body)
            .expect("decode bootstrap body"),
        OpaqueBootstrapExchangeRequest {
            bootstrap_token: "bootstrap-secret".to_string()
        }
    );
    assert_eq!(
        String::from_utf8(bootstrap.body).expect("body UTF-8"),
        r#"{"bootstrap_token":"bootstrap-secret"}"#
    );

    let session_status = server.request();
    assert_eq!(session_status.method, "GET");
    assert_eq!(session_status.path, "/v1/sessions/session-7");
    assert_eq!(
        session_status.headers.get("authorization").unwrap(),
        "Bearer capability-secret"
    );
    assert_eq!(
        session_status.headers.get("accept").unwrap(),
        "application/json"
    );
    assert!(session_status.body.is_empty());

    let export = server.request();
    assert_eq!(export.method, "GET");
    assert_eq!(export.path, "/v1/sessions/session-7/export");
    assert_eq!(
        export.headers.get("authorization").unwrap(),
        "Bearer capability-secret"
    );
    assert_eq!(export.headers.get("accept").unwrap(), "application/x-tar");
    assert_eq!(
        export.headers.get("accept-encoding").unwrap(),
        "zstd, identity"
    );
    assert!(export.body.is_empty());
}

#[test]
fn zstd_bootstrap_streams_into_the_shared_materializer() {
    let directory = TestDirectory::new("zstd");
    let tar = tar_file(b"answer.txt", b"42\n");
    let compressed = zstd::stream::encode_all(tar.as_slice(), 1).expect("compress tar");
    let capability = capability();
    let status = ready_status(
        capability.session_id.clone(),
        COMPONENT_VERSIONS,
        tar.len() as u64,
        BTreeSet::from([TarContentEncoding::Identity, TarContentEncoding::Zstd]),
    );
    let server = MockServer::start(vec![
        ResponseFixture::json(&capability),
        ResponseFixture::json(&status),
        ResponseFixture::export("zstd", compressed),
    ]);

    let report = run_sandbox_init(
        SandboxInitOptions {
            api_url: server.api_url.clone(),
            root: directory.root(),
        },
        SandboxBootstrapToken::new("bootstrap-secret").expect("token"),
    )
    .expect("bootstrap zstd export");

    assert_eq!(report.content_encoding, "zstd");
    assert_eq!(report.decoded_bytes, tar.len() as u64);
    assert_eq!(
        fs::read(directory.root().join("answer.txt")).expect("read replica"),
        b"42\n"
    );
    let _ = server.request();
    let _ = server.request();
    let export = server.request();
    assert_eq!(
        export.headers.get("accept-encoding").unwrap(),
        "zstd, identity"
    );
}

#[test]
fn hostile_and_truncated_exports_roll_back_the_destination() {
    let fixtures = [
        ("hostile", tar_file(b"../escape.txt", b"escape")),
        ("truncated", {
            let mut tar = tar_file(b"safe.txt", b"safe");
            tar.truncate(tar.len() - 512);
            tar
        }),
    ];

    for (label, tar) in fixtures {
        let directory = TestDirectory::new(label);
        let capability = capability();
        let status = ready_status(
            capability.session_id.clone(),
            COMPONENT_VERSIONS,
            tar.len() as u64,
            BTreeSet::from([TarContentEncoding::Identity]),
        );
        let server = MockServer::start(vec![
            ResponseFixture::json(&capability),
            ResponseFixture::json(&status),
            ResponseFixture::export("identity", tar),
        ]);

        let error = run_sandbox_init(
            SandboxInitOptions {
                api_url: server.api_url.clone(),
                root: directory.root(),
            },
            SandboxBootstrapToken::new("bootstrap-secret").expect("token"),
        )
        .expect_err("hostile export must fail");

        assert_eq!(error.code(), "materialization_failed", "case {label}");
        assert!(!directory.root().exists(), "case {label}");
        assert!(!error.to_string().contains("bootstrap-secret"));
        let _ = server.request();
        let _ = server.request();
        let _ = server.request();
    }
}

#[test]
fn non_ready_and_oversize_sessions_fail_before_export() {
    let cases = [
        (
            "not-ready",
            non_ready_status(),
            "session_not_ready",
            "sandbox session is Bootstrapping (Bootstrapping)",
        ),
        (
            "oversize",
            ready_status(
                SessionId::new("session-7"),
                COMPONENT_VERSIONS,
                1024,
                BTreeSet::from([TarContentEncoding::Identity]),
            )
            .with_selected_entries(ReplicaMaterializationLimits::default().max_entries + 1),
            "export_limit_exceeded",
            "sandbox export entry count 100001 exceeds client maximum 100000",
        ),
    ];

    for (label, status, expected_code, expected_message) in cases {
        let directory = TestDirectory::new(label);
        let server = MockServer::start(vec![
            ResponseFixture::json(&capability()),
            ResponseFixture::json(&status),
        ]);
        let error = run_sandbox_init(
            SandboxInitOptions {
                api_url: server.api_url.clone(),
                root: directory.root(),
            },
            SandboxBootstrapToken::new("bootstrap-secret").expect("token"),
        )
        .expect_err("session must fail before export");

        assert_eq!(error.code(), expected_code, "case {label}");
        assert_eq!(error.to_string(), expected_message, "case {label}");
        assert!(!directory.root().exists(), "case {label}");
        let _ = server.request();
        let _ = server.request();
    }
}

#[test]
fn version_session_offer_media_and_response_encoding_are_validated() {
    let tar = tar_file(b"safe.txt", b"safe");
    let cases = [
        (
            "version",
            ready_status(
                SessionId::new("session-7"),
                ComponentVersions {
                    session: COMPONENT_VERSIONS.session + 1,
                    ..COMPONENT_VERSIONS
                },
                tar.len() as u64,
                BTreeSet::from([TarContentEncoding::Identity]),
            ),
            None,
            "update_required",
        ),
        (
            "session-id",
            ready_status(
                SessionId::new("different-session"),
                COMPONENT_VERSIONS,
                tar.len() as u64,
                BTreeSet::from([TarContentEncoding::Identity]),
            ),
            None,
            "backend_protocol_invalid",
        ),
        (
            "offer-media",
            ready_status(
                SessionId::new("session-7"),
                COMPONENT_VERSIONS,
                tar.len() as u64,
                BTreeSet::from([TarContentEncoding::Identity]),
            )
            .with_media_type("application/octet-stream"),
            None,
            "backend_protocol_invalid",
        ),
        (
            "response-media",
            ready_status(
                SessionId::new("session-7"),
                COMPONENT_VERSIONS,
                tar.len() as u64,
                BTreeSet::from([TarContentEncoding::Identity]),
            ),
            Some(ResponseFixture {
                status: "200 OK",
                headers: vec![("Content-Type", "application/octet-stream")],
                body: tar.clone(),
            }),
            "backend_protocol_invalid",
        ),
        (
            "response-encoding",
            ready_status(
                SessionId::new("session-7"),
                COMPONENT_VERSIONS,
                tar.len() as u64,
                BTreeSet::from([TarContentEncoding::Identity]),
            ),
            Some(ResponseFixture::export("gzip", tar.clone())),
            "backend_protocol_invalid",
        ),
    ];

    for (label, status, export, expected_code) in cases {
        let directory = TestDirectory::new(label);
        let mut responses = vec![
            ResponseFixture::json(&capability()),
            ResponseFixture::json(&status),
        ];
        if let Some(export) = export {
            responses.push(export);
        }
        let expected_requests = responses.len();
        let server = MockServer::start(responses);
        let error = run_sandbox_init(
            SandboxInitOptions {
                api_url: server.api_url.clone(),
                root: directory.root(),
            },
            SandboxBootstrapToken::new("bootstrap-secret").expect("token"),
        )
        .expect_err("invalid session/export must fail");

        assert_eq!(error.code(), expected_code, "case {label}: {error}");
        assert!(!directory.root().exists(), "case {label}");
        for _ in 0..expected_requests {
            let _ = server.request();
        }
    }
}

#[test]
fn token_sources_are_exclusive_trim_only_line_endings_and_redact_debug() {
    let environment = resolve_bootstrap_token(
        false,
        Some("environment-secret".into()),
        &mut Cursor::new(Vec::<u8>::new()),
    )
    .expect("environment token");
    let stdin = resolve_bootstrap_token(true, None, &mut Cursor::new(b"stdin-secret\r\n".to_vec()))
        .expect("stdin token");
    let ambiguous = resolve_bootstrap_token(
        true,
        Some("environment-secret".into()),
        &mut Cursor::new(b"stdin-secret\n".to_vec()),
    )
    .expect_err("ambiguous token sources");
    let missing = resolve_bootstrap_token(false, None, &mut Cursor::new(Vec::<u8>::new()))
        .expect_err("missing token");

    assert_eq!(
        format!("{environment:?}"),
        "SandboxBootstrapToken(<redacted>)"
    );
    assert_eq!(format!("{stdin:?}"), "SandboxBootstrapToken(<redacted>)");
    assert!(matches!(
        ambiguous,
        SandboxInitError::AmbiguousBootstrapToken
    ));
    assert!(matches!(missing, SandboxInitError::MissingBootstrapToken));
}

#[test]
fn cli_environment_token_never_appears_in_output() {
    let directory = TestDirectory::new("cli-redaction");
    let tar = tar_file(b"visible.txt", b"visible\n");
    let capability = capability();
    let status = ready_status(
        capability.session_id.clone(),
        COMPONENT_VERSIONS,
        tar.len() as u64,
        BTreeSet::from([TarContentEncoding::Identity]),
    );
    let server = MockServer::start(vec![
        ResponseFixture::json(&capability),
        ResponseFixture::json(&status),
        ResponseFixture::export("identity", tar),
    ]);
    let root = directory.root().to_string_lossy().into_owned();

    let output = Command::new(env!("CARGO_BIN_EXE_loc"))
        .args([
            "sandbox",
            "init",
            "--api-url",
            &server.api_url,
            "--root",
            &root,
            "--json",
        ])
        .env("LOCALITY_BOOTSTRAP_TOKEN", "cli-bootstrap-secret")
        .output()
        .expect("run loc sandbox init");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout UTF-8");
    let stderr = String::from_utf8(output.stderr).expect("stderr UTF-8");
    assert!(!stdout.contains("cli-bootstrap-secret"));
    assert!(!stdout.contains("capability-secret"));
    assert!(!stderr.contains("cli-bootstrap-secret"));
    assert!(!stderr.contains("capability-secret"));
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("JSON report");
    assert_eq!(report["command"], "sandbox_init");
    assert_eq!(report["content_encoding"], "identity");
    assert_eq!(
        fs::read(directory.root().join("visible.txt")).expect("read CLI replica"),
        b"visible\n"
    );

    let bootstrap = server.request();
    assert_eq!(
        String::from_utf8(bootstrap.body).expect("bootstrap UTF-8"),
        r#"{"bootstrap_token":"cli-bootstrap-secret"}"#
    );
    let _ = server.request();
    let _ = server.request();
}

trait StatusFixtureExt {
    fn with_selected_entries(self, selected_entries: u64) -> Self;
    fn with_media_type(self, media_type: &str) -> Self;
}

impl StatusFixtureExt for SandboxSessionStatus {
    fn with_selected_entries(mut self, selected_entries: u64) -> Self {
        self.export_offer
            .as_mut()
            .expect("ready offer")
            .selected_entries = selected_entries;
        self
    }

    fn with_media_type(mut self, media_type: &str) -> Self {
        self.export_offer.as_mut().expect("ready offer").media_type = media_type.to_string();
        self
    }
}

fn capability() -> SessionCapability {
    SessionCapability {
        session_id: SessionId::new("session-7"),
        opaque_capability: "capability-secret".to_string(),
        expires_at: "2026-07-20T12:00:00Z".to_string(),
    }
}

fn ready_status(
    session_id: SessionId,
    versions: ComponentVersions,
    decoded_bytes: u64,
    encodings: BTreeSet<TarContentEncoding>,
) -> SandboxSessionStatus {
    SandboxSessionStatus {
        versions,
        session_id,
        state: SandboxSessionState::Ready,
        freshness_requirement: freshness_requirement(),
        replicas: Vec::new(),
        export_offer: Some(TarExportOffer {
            media_type: "application/x-tar".to_string(),
            supported_content_encodings: encodings,
            selected_entries: 1,
            decoded_bytes,
            decoded_tar_sha256: "sha256:decoded-tar".to_string(),
        }),
        error: None,
        updated_at: "2026-07-20T11:00:00Z".to_string(),
    }
}

fn non_ready_status() -> SandboxSessionStatus {
    SandboxSessionStatus {
        versions: COMPONENT_VERSIONS,
        session_id: SessionId::new("session-7"),
        state: SandboxSessionState::Bootstrapping,
        freshness_requirement: freshness_requirement(),
        replicas: Vec::new(),
        export_offer: None,
        error: Some(SessionProtocolError {
            code: SessionErrorCode::Bootstrapping,
            message: "still building".to_string(),
            retriable: true,
            retry_after_seconds: Some(5),
        }),
        updated_at: "2026-07-20T11:00:00Z".to_string(),
    }
}

fn freshness_requirement() -> FreshnessRequirement {
    FreshnessRequirement {
        max_age_seconds: 300,
        on_stale: StaleSessionBehavior::WaitThenFail,
        wait_timeout_seconds: 30,
    }
}

fn tar_file(path: &[u8], body: &[u8]) -> Vec<u8> {
    assert!(path.len() <= 100);
    let mut builder = Builder::new(Vec::new());
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::file());
    header.set_mode(0o444);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_size(body.len() as u64);
    {
        let bytes = header.as_mut_bytes();
        bytes[..100].fill(0);
        bytes[..path.len()].copy_from_slice(path);
    }
    header.set_cksum();
    builder.append(&header, body).expect("append tar fixture");
    builder.finish().expect("finish tar fixture");
    builder.into_inner().expect("collect tar fixture")
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
    let headers_text = std::str::from_utf8(&bytes[..header_end]).expect("request headers UTF-8");
    let mut lines = headers_text.split("\r\n");
    let request_line = lines.next().expect("request line");
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().expect("method").to_string();
    let path = request_parts.next().expect("path").to_string();
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

fn write_response(stream: &mut TcpStream, response: ResponseFixture) {
    write!(
        stream,
        "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        response.body.len()
    )
    .expect("write response head");
    for (name, value) in response.headers {
        write!(stream, "{name}: {value}\r\n").expect("write response header");
    }
    write!(stream, "\r\n").expect("finish response headers");
    stream
        .write_all(&response.body)
        .expect("write response body");
    stream.flush().expect("flush response");
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn make_removable(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        make_writable(path, 0o700);
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                make_removable(&entry.path());
            }
        }
    } else {
        make_writable(path, 0o600);
    }
}

#[cfg(unix)]
fn make_writable(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn make_writable(path: &Path, _mode: u32) {
    let Ok(mut permissions) = fs::metadata(path).map(|metadata| metadata.permissions()) else {
        return;
    };
    permissions.set_readonly(false);
    let _ = fs::set_permissions(path, permissions);
}
