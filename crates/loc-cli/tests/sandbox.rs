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
    SandboxBootstrapToken, SandboxContentEncodingPreference, SandboxInitError, SandboxInitOptions,
    resolve_bootstrap_token, run_sandbox_init, run_sandbox_init_with_encoding,
};
use locality_core::portable::SessionId;
use locality_protocol::{
    COMPONENT_VERSIONS, ComponentVersions, FreshnessRequirement, OpaqueBootstrapExchangeRequest,
    SandboxSessionState, SandboxSessionStatus, SessionCapability, SessionErrorCode,
    SessionProtocolError, StaleSessionBehavior, TarContentEncoding, TarExportOffer,
};
use localityd::replica_materializer::ReplicaMaterializationLimits;
use sha2::{Digest, Sha256};
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
    declared_content_length: Option<usize>,
    split_after: Option<(usize, Duration)>,
    staging_gate: Option<(PathBuf, PathBuf, PathBuf)>,
}

impl ResponseFixture {
    fn json<T: serde::Serialize>(value: &T) -> Self {
        Self {
            status: "200 OK",
            headers: vec![("Content-Type", "application/json")],
            body: serde_json::to_vec(value).expect("serialize response"),
            declared_content_length: None,
            split_after: None,
            staging_gate: None,
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
            declared_content_length: None,
            split_after: None,
            staging_gate: None,
        }
    }

    fn streaming_export(
        encoding: &'static str,
        body: Vec<u8>,
        split_after: usize,
        pause: Duration,
    ) -> Self {
        Self::export(encoding, body).with_split_after(split_after, pause)
    }

    fn with_declared_content_length(mut self, length: usize) -> Self {
        self.declared_content_length = Some(length);
        self
    }

    fn with_split_after(mut self, bytes: usize, pause: Duration) -> Self {
        self.split_after = Some((bytes, pause));
        self
    }

    fn with_staging_gate(
        mut self,
        parent: PathBuf,
        logical_path: PathBuf,
        destination: PathBuf,
    ) -> Self {
        self.staging_gate = Some((parent, logical_path, destination));
        self
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
                        Ok((stream, _)) => {
                            stream
                                .set_nonblocking(false)
                                .expect("set accepted mock socket blocking");
                            break stream;
                        }
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

    fn assert_no_request(&self) {
        match self.requests.recv_timeout(Duration::from_millis(100)) {
            Ok(request) => panic!("unexpected request: {request:?}"),
            Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {}
        }
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
fn loopback_identity_bootstrap_sends_only_sealed_requests_and_materializes() {
    let directory = TestDirectory::new("identity");
    let tar = tar_file(b"docs/readme.md", b"hello\n");
    let capability = capability();
    let status = ready_status(
        capability.session_id.clone(),
        COMPONENT_VERSIONS,
        &tar,
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
        &tar,
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
fn export_bytes_are_staged_while_the_http_response_is_still_streaming() {
    let directory = TestDirectory::new("streaming-overlap");
    let body = vec![0x5a; 256 * 1024];
    let tar = tar_file(b"large.bin", &body);
    let capability = capability();
    let status = ready_status(
        capability.session_id.clone(),
        COMPONENT_VERSIONS,
        &tar,
        BTreeSet::from([TarContentEncoding::Identity]),
    );
    let destination = directory.root();
    let response = ResponseFixture::streaming_export(
        "identity",
        tar,
        512 + 64 * 1024,
        Duration::from_millis(10),
    )
    .with_staging_gate(
        destination
            .parent()
            .expect("destination parent")
            .to_path_buf(),
        PathBuf::from("large.bin"),
        destination.clone(),
    );
    let server = MockServer::start(vec![
        ResponseFixture::json(&capability),
        ResponseFixture::json(&status),
        response,
    ]);

    let report = run_sandbox_init(
        SandboxInitOptions {
            api_url: server.api_url.clone(),
            root: destination.clone(),
        },
        SandboxBootstrapToken::new("bootstrap-secret").expect("token"),
    )
    .expect("stream and publish export");

    assert_eq!(report.files, 1);
    assert_eq!(report.materialized_bytes, body.len() as u64);
    assert_eq!(
        fs::read(destination.join("large.bin")).expect("read published file"),
        body
    );
}

#[test]
fn truncated_http_body_producer_error_prevents_publication() {
    let directory = TestDirectory::new("transport-truncation");
    let tar = tar_file(b"complete-before-http-eof.txt", b"complete\n");
    let capability = capability();
    let status = ready_status(
        capability.session_id.clone(),
        COMPONENT_VERSIONS,
        &tar,
        BTreeSet::from([TarContentEncoding::Identity]),
    );
    let declared_length = tar.len() + 128;
    let server = MockServer::start(vec![
        ResponseFixture::json(&capability),
        ResponseFixture::json(&status),
        ResponseFixture::export("identity", tar).with_declared_content_length(declared_length),
    ]);

    let error = run_sandbox_init(
        SandboxInitOptions {
            api_url: server.api_url.clone(),
            root: directory.root(),
        },
        SandboxBootstrapToken::new("bootstrap-secret").expect("token"),
    )
    .expect_err("an incomplete HTTP response must not publish a complete-looking tar");

    assert_eq!(error.code(), "materialization_failed");
    assert!(!directory.root().exists());
    assert!(!error.to_string().contains("bootstrap-secret"));
    assert!(!error.to_string().contains("capability-secret"));
}

#[test]
fn forced_content_encodings_send_exact_headers_and_match_reports() {
    let tar = tar_file(b"forced.txt", b"forced\n");
    let zstd = zstd::stream::encode_all(tar.as_slice(), 1).expect("compress tar");
    let cases = [
        (
            "forced-identity",
            SandboxContentEncodingPreference::Identity,
            "identity",
            tar.clone(),
        ),
        (
            "forced-zstd",
            SandboxContentEncodingPreference::Zstd,
            "zstd",
            zstd,
        ),
    ];

    for (label, preference, expected_encoding, body) in cases {
        let directory = TestDirectory::new(label);
        let capability = capability();
        let status = ready_status(
            capability.session_id.clone(),
            COMPONENT_VERSIONS,
            &tar,
            BTreeSet::from([TarContentEncoding::Identity, TarContentEncoding::Zstd]),
        );
        let server = MockServer::start(vec![
            ResponseFixture::json(&capability),
            ResponseFixture::json(&status),
            ResponseFixture::export(expected_encoding, body),
        ]);

        let report = run_sandbox_init_with_encoding(
            SandboxInitOptions {
                api_url: server.api_url.clone(),
                root: directory.root(),
            },
            SandboxBootstrapToken::new("bootstrap-secret").expect("token"),
            preference,
        )
        .expect("bootstrap forced export encoding");

        assert_eq!(report.content_encoding, expected_encoding, "case {label}");
        assert_eq!(
            fs::read(directory.root().join("forced.txt")).expect("read replica"),
            b"forced\n",
            "case {label}"
        );
        let _ = server.request();
        let _ = server.request();
        let export = server.request();
        assert_eq!(
            export.headers.get("accept-encoding").unwrap(),
            expected_encoding,
            "case {label}"
        );
    }
}

#[test]
fn forced_content_encoding_fails_closed_on_offer_or_response_mismatch() {
    let tar = tar_file(b"never-published.txt", b"body");
    let offer_capability = capability();
    let identity_only = ready_status(
        offer_capability.session_id.clone(),
        COMPONENT_VERSIONS,
        &tar,
        BTreeSet::from([TarContentEncoding::Identity]),
    );
    let server = MockServer::start(vec![
        ResponseFixture::json(&offer_capability),
        ResponseFixture::json(&identity_only),
    ]);
    let directory = TestDirectory::new("forced-zstd-not-offered");

    let error = run_sandbox_init_with_encoding(
        SandboxInitOptions {
            api_url: server.api_url.clone(),
            root: directory.root(),
        },
        SandboxBootstrapToken::new("bootstrap-secret").expect("token"),
        SandboxContentEncodingPreference::Zstd,
    )
    .expect_err("unoffered forced encoding must fail before export");

    assert_eq!(error.code(), "backend_protocol_invalid");
    assert_eq!(
        error.to_string(),
        "unsupported sandbox export encoding `zstd`"
    );
    assert!(!directory.root().exists());
    let _ = server.request();
    let _ = server.request();
    server.assert_no_request();

    for (label, preference, response_encoding, requested) in [
        (
            "forced-identity-got-zstd",
            SandboxContentEncodingPreference::Identity,
            "zstd",
            "identity",
        ),
        (
            "forced-zstd-got-identity",
            SandboxContentEncodingPreference::Zstd,
            "identity",
            "zstd",
        ),
    ] {
        let directory = TestDirectory::new(label);
        let capability = capability();
        let status = ready_status(
            capability.session_id.clone(),
            COMPONENT_VERSIONS,
            &tar,
            BTreeSet::from([TarContentEncoding::Identity, TarContentEncoding::Zstd]),
        );
        let server = MockServer::start(vec![
            ResponseFixture::json(&capability),
            ResponseFixture::json(&status),
            ResponseFixture::export(response_encoding, tar.clone()),
        ]);

        let error = run_sandbox_init_with_encoding(
            SandboxInitOptions {
                api_url: server.api_url.clone(),
                root: directory.root(),
            },
            SandboxBootstrapToken::new("bootstrap-secret").expect("token"),
            preference,
        )
        .expect_err("forced response mismatch must fail before materialization");

        assert_eq!(error.code(), "backend_protocol_invalid", "case {label}");
        assert_eq!(
            error.to_string(),
            format!(
                "unsupported sandbox export encoding `{response_encoding} (requested {requested})`"
            ),
            "case {label}"
        );
        assert!(!directory.root().exists(), "case {label}");
        let _ = server.request();
        let _ = server.request();
        let export = server.request();
        assert_eq!(
            export.headers.get("accept-encoding").unwrap(),
            requested,
            "case {label}"
        );
    }
}

#[test]
fn export_receipt_mismatches_roll_back_without_leaking_tokens() {
    let tar = tar_file(b"never-published.txt", b"body");
    let wrong_digest = format!("sha256:{}", "0".repeat(64));
    let cases = [
        (
            "digest",
            Some(wrong_digest.as_str()),
            None,
            None,
            "digest mismatch",
        ),
        (
            "decoded-bytes",
            None,
            Some(tar.len() as u64 + 1),
            None,
            "decoded-byte receipt mismatch",
        ),
        (
            "entries",
            None,
            None,
            Some(2),
            "entry-count receipt mismatch",
        ),
    ];

    for (label, digest, decoded_bytes, entries, expected_detail) in cases {
        let directory = TestDirectory::new(label);
        let capability = capability();
        let mut status = ready_status(
            capability.session_id.clone(),
            COMPONENT_VERSIONS,
            &tar,
            BTreeSet::from([TarContentEncoding::Identity]),
        );
        if let Some(digest) = digest {
            status = status.with_decoded_tar_sha256(digest);
        }
        if let Some(decoded_bytes) = decoded_bytes {
            status = status.with_decoded_bytes(decoded_bytes);
        }
        if let Some(entries) = entries {
            status = status.with_selected_entries(entries);
        }
        let server = MockServer::start(vec![
            ResponseFixture::json(&capability),
            ResponseFixture::json(&status),
            ResponseFixture::export("identity", tar.clone()),
        ]);

        let error = run_sandbox_init(
            SandboxInitOptions {
                api_url: server.api_url.clone(),
                root: directory.root(),
            },
            SandboxBootstrapToken::new("bootstrap-secret").expect("token"),
        )
        .expect_err("receipt mismatch must fail before publish");

        assert_eq!(error.code(), "materialization_failed", "case {label}");
        assert!(
            error.to_string().contains(expected_detail),
            "case {label}: {error}"
        );
        assert!(!directory.root().exists(), "case {label}");
        assert!(!error.to_string().contains("bootstrap-secret"));
        assert!(!error.to_string().contains("capability-secret"));
        assert!(!format!("{error:?}").contains("bootstrap-secret"));
        assert!(!format!("{error:?}").contains("capability-secret"));
        let _ = server.request();
        let _ = server.request();
        let _ = server.request();
    }
}

#[test]
fn malformed_offer_digests_fail_before_the_export_request() {
    let malformed = [
        ("empty", ""),
        ("short", "sha256:abcd"),
        (
            "uppercase",
            "sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ),
        (
            "wrong-prefix",
            "SHA256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ),
        (
            "non-hex",
            "sha256:gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg",
        ),
    ];
    let tar = tar_file(b"safe.txt", b"safe");

    for (label, digest) in malformed {
        let directory = TestDirectory::new(label);
        let capability = capability();
        let status = ready_status(
            capability.session_id.clone(),
            COMPONENT_VERSIONS,
            &tar,
            BTreeSet::from([TarContentEncoding::Identity]),
        )
        .with_decoded_tar_sha256(digest);
        let server = MockServer::start(vec![
            ResponseFixture::json(&capability),
            ResponseFixture::json(&status),
        ]);

        let error = run_sandbox_init(
            SandboxInitOptions {
                api_url: server.api_url.clone(),
                root: directory.root(),
            },
            SandboxBootstrapToken::new("bootstrap-secret").expect("token"),
        )
        .expect_err("malformed digest must fail before export");

        assert_eq!(error.code(), "backend_protocol_invalid", "case {label}");
        assert_eq!(
            error.to_string(),
            "sandbox export offer is invalid: decoded tar digest must use canonical sha256:<64 lowercase hex>",
            "case {label}"
        );
        assert!(!directory.root().exists(), "case {label}");
        let _ = server.request();
        let _ = server.request();
        server.assert_no_request();
    }
}

#[test]
fn non_loopback_http_api_is_rejected_before_any_request() {
    let directory = TestDirectory::new("non-loopback-http");
    let error = run_sandbox_init(
        SandboxInitOptions {
            api_url: "http://bootstrap-secret.example".to_string(),
            root: directory.root(),
        },
        SandboxBootstrapToken::new("bootstrap-secret").expect("token"),
    )
    .expect_err("non-loopback HTTP must fail before a request");

    assert_eq!(error.code(), "api_url_invalid");
    assert_eq!(
        error.to_string(),
        "invalid API URL: http scheme is allowed only for loopback hosts"
    );
    assert!(!error.to_string().contains("bootstrap-secret"));
    assert!(!format!("{error:?}").contains("bootstrap-secret"));
    assert!(!directory.root().exists());
}

#[test]
fn bearer_authenticated_redirect_is_not_followed() {
    let directory = TestDirectory::new("redirect");
    let capability = capability();
    let server = MockServer::start(vec![
        ResponseFixture::json(&capability),
        ResponseFixture {
            status: "302 Found",
            headers: vec![("Location", "/redirected")],
            body: Vec::new(),
            declared_content_length: None,
            split_after: None,
            staging_gate: None,
        },
    ]);

    let error = run_sandbox_init(
        SandboxInitOptions {
            api_url: server.api_url.clone(),
            root: directory.root(),
        },
        SandboxBootstrapToken::new("bootstrap-secret").expect("token"),
    )
    .expect_err("redirects must fail closed");

    assert_eq!(error.code(), "backend_request_failed");
    assert_eq!(error.to_string(), "session status returned HTTP 302 Found");
    assert!(!error.to_string().contains("bootstrap-secret"));
    assert!(!error.to_string().contains("capability-secret"));
    let _ = server.request();
    let status = server.request();
    assert_eq!(status.path, "/v1/sessions/session-7");
    assert_eq!(
        status.headers.get("authorization").unwrap(),
        "Bearer capability-secret"
    );
    server.assert_no_request();
    assert!(!directory.root().exists());
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
            &tar,
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
    let tar = tar_file(b"safe.txt", b"safe");
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
                &tar,
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
                &tar,
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
                &tar,
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
                &tar,
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
                &tar,
                BTreeSet::from([TarContentEncoding::Identity]),
            ),
            Some(ResponseFixture {
                status: "200 OK",
                headers: vec![("Content-Type", "application/octet-stream")],
                body: tar.clone(),
                declared_content_length: None,
                split_after: None,
                staging_gate: None,
            }),
            "backend_protocol_invalid",
        ),
        (
            "response-encoding",
            ready_status(
                SessionId::new("session-7"),
                COMPONENT_VERSIONS,
                &tar,
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
fn cli_forced_identity_reports_encoding_without_leaking_environment_token() {
    let directory = TestDirectory::new("cli-redaction");
    let tar = tar_file(b"visible.txt", b"visible\n");
    let capability = capability();
    let status = ready_status(
        capability.session_id.clone(),
        COMPONENT_VERSIONS,
        &tar,
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
            "--encoding",
            "identity",
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
    assert!(stderr.is_empty(), "profiling is opt-in: {stderr}");
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
    let export = server.request();
    assert_eq!(export.headers.get("accept-encoding").unwrap(), "identity");
}

#[test]
fn cli_profile_has_stable_monotonic_phases_and_no_request_details() {
    let directory = TestDirectory::new("cli-profile");
    let tar = tar_file(b"profiled.txt", b"profile-content-secret\n");
    let capability = capability();
    let status = ready_status(
        capability.session_id.clone(),
        COMPONENT_VERSIONS,
        &tar,
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
            "--encoding",
            "identity",
            "--profile",
            "--json",
        ])
        .env("LOCALITY_BOOTSTRAP_TOKEN", "profile-bootstrap-secret")
        .output()
        .expect("run profiled loc sandbox init");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr UTF-8");
    let expected_phases = [
        "bootstrap_token_input",
        "client_setup",
        "bootstrap_exchange",
        "session_status",
        "export_open_headers",
        "first_consumer_body_byte",
        "stream_decode_materialize",
        "total",
    ];
    let lines = stderr.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), expected_phases.len(), "{stderr}");
    let mut previous_total_ms = 0_u128;
    for (line, expected_phase) in lines.iter().zip(expected_phases) {
        let prefix = format!("locality sandbox profile phase={expected_phase} phase_ms=");
        let timing = line
            .strip_prefix(&prefix)
            .unwrap_or_else(|| panic!("unexpected profile line: {line}"));
        let (phase_ms, total_ms) = timing
            .split_once(" total_ms=")
            .unwrap_or_else(|| panic!("profile line lacks exact timing fields: {line}"));
        assert!(
            !phase_ms.is_empty()
                && phase_ms.bytes().all(|byte| byte.is_ascii_digit())
                && !total_ms.is_empty()
                && total_ms.bytes().all(|byte| byte.is_ascii_digit()),
            "profile timing fields must contain only decimal milliseconds: {line}"
        );
        let phase_ms = phase_ms.parse::<u128>().expect("phase milliseconds");
        let total_ms = total_ms.parse::<u128>().expect("total milliseconds");
        assert!(
            total_ms >= previous_total_ms,
            "profile timings must be monotonic: {stderr}"
        );
        assert_eq!(
            phase_ms,
            total_ms - previous_total_ms,
            "phase timing must be the delta since the prior mark: {stderr}"
        );
        previous_total_ms = total_ms;
    }

    for secret_or_detail in [
        "profile-bootstrap-secret",
        "capability-secret",
        "session-7",
        server.api_url.as_str(),
        root.as_str(),
        "application/x-tar",
        "profile-content-secret",
        "authorization",
    ] {
        assert!(
            !stderr.contains(secret_or_detail),
            "profile leaked forbidden detail `{secret_or_detail}`: {stderr}"
        );
    }
}

#[test]
fn cli_profile_failure_prints_completed_phases_and_total() {
    let directory = TestDirectory::new("cli-profile-failure");
    let tar = tar_file(b"never-exported.txt", b"never-exported-content\n");
    let capability = capability();
    let status = ready_status(
        capability.session_id.clone(),
        COMPONENT_VERSIONS,
        &tar,
        BTreeSet::from([TarContentEncoding::Identity]),
    );
    let server = MockServer::start(vec![
        ResponseFixture::json(&capability),
        ResponseFixture::json(&status),
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
            "--encoding",
            "zstd",
            "--profile",
        ])
        .env("LOCALITY_BOOTSTRAP_TOKEN", "failed-profile-secret")
        .output()
        .expect("run failing profiled loc sandbox init");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr UTF-8");
    let phases = stderr
        .lines()
        .filter_map(|line| line.strip_prefix("locality sandbox profile phase="))
        .map(|timing| timing.split_once(' ').expect("phase timing fields").0)
        .collect::<Vec<_>>();
    assert_eq!(
        phases,
        [
            "bootstrap_token_input",
            "client_setup",
            "bootstrap_exchange",
            "session_status",
            "total"
        ],
        "{stderr}"
    );
    assert!(!stderr.contains("failed-profile-secret"));
    assert!(!stderr.contains("capability-secret"));
    assert!(!stderr.contains("session-7"));
    assert!(!stderr.contains(&server.api_url));
    assert!(!stderr.contains(&root));
}

trait StatusFixtureExt {
    fn with_selected_entries(self, selected_entries: u64) -> Self;
    fn with_decoded_bytes(self, decoded_bytes: u64) -> Self;
    fn with_decoded_tar_sha256(self, decoded_tar_sha256: &str) -> Self;
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

    fn with_decoded_bytes(mut self, decoded_bytes: u64) -> Self {
        self.export_offer
            .as_mut()
            .expect("ready offer")
            .decoded_bytes = decoded_bytes;
        self
    }

    fn with_decoded_tar_sha256(mut self, decoded_tar_sha256: &str) -> Self {
        self.export_offer
            .as_mut()
            .expect("ready offer")
            .decoded_tar_sha256 = decoded_tar_sha256.to_string();
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
    decoded_tar: &[u8],
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
            decoded_bytes: decoded_tar.len() as u64,
            decoded_tar_sha256: sha256_label(decoded_tar),
        }),
        error: None,
        updated_at: "2026-07-20T11:00:00Z".to_string(),
    }
}

fn sha256_label(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256:{hex}")
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
    let content_length = response
        .declared_content_length
        .unwrap_or(response.body.len());
    write!(
        stream,
        "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status, content_length
    )
    .expect("write response head");
    for (name, value) in response.headers {
        write!(stream, "{name}: {value}\r\n").expect("write response header");
    }
    write!(stream, "\r\n").expect("finish response headers");
    if let Some((split_after, pause)) = response.split_after {
        let split_after = split_after.min(response.body.len());
        stream
            .write_all(&response.body[..split_after])
            .expect("write first response body chunk");
        stream.flush().expect("flush first response body chunk");
        if let Some((parent, logical_path, destination)) = &response.staging_gate {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                let staged = fs::read_dir(parent)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .any(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".locality-stage-")
                            && entry.path().join(logical_path).is_file()
                    });
                if staged {
                    assert!(
                        !destination.exists(),
                        "the destination must remain absent while the response is incomplete"
                    );
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "materializer did not stage the first file while the response was paused"
                );
                thread::sleep(Duration::from_millis(2));
            }
        }
        thread::sleep(pause);
        stream
            .write_all(&response.body[split_after..])
            .expect("write remaining response body");
    } else {
        stream
            .write_all(&response.body)
            .expect("write response body");
    }
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
