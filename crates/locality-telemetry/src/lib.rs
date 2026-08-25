//! Small, privacy-bounded telemetry client for Locality product events and errors.
//!
//! Events are first written to a durable local spool and are deleted only after
//! the configured first-party endpoint acknowledges the batch. The API accepts
//! only a fixed set of low-cardinality fields; user content and arbitrary error
//! messages have no representation in the wire contract.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const TELEMETRY_SCHEMA_VERSION: u16 = 1;
const INSTALLATION_ID_FILENAME: &str = "installation-id";
const SPOOL_DIR_NAME: &str = "telemetry";
const MAX_QUEUED_EVENTS: usize = 1_000;
const MAX_BATCH_EVENTS: usize = 50;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct TelemetryConfig {
    pub state_root: PathBuf,
    pub endpoint: Option<String>,
    pub enabled: bool,
    pub app: &'static str,
    pub version: &'static str,
    pub build_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Error,
    Fatal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Started,
    Succeeded,
    Failed,
    Cancelled,
}

/// Low-cardinality properties accepted by the remote telemetry contract.
///
/// Do not add paths, URLs, user-entered strings, provider object IDs, account
/// labels, or error messages here. Those belong only in local diagnostics.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventProperties {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connector: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<Severity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_line: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryEvent {
    pub schema_version: u16,
    pub event_id: String,
    pub occurred_at_ms: u64,
    pub anonymous_id: String,
    pub session_id: String,
    pub app: String,
    pub version: String,
    pub build_id: String,
    pub os: String,
    pub arch: String,
    pub name: String,
    pub properties: EventProperties,
}

#[derive(Debug, Serialize)]
struct TelemetryBatch<'a> {
    schema_version: u16,
    events: &'a [TelemetryEvent],
}

#[derive(Debug)]
pub struct TelemetryClient {
    config: TelemetryConfig,
    anonymous_id: String,
    session_id: String,
    enabled: AtomicBool,
    sequence: AtomicU64,
}

impl TelemetryClient {
    pub fn new(config: TelemetryConfig) -> io::Result<Self> {
        let anonymous_id = load_or_create_installation_id(&config.state_root)?;
        Ok(Self {
            enabled: AtomicBool::new(config.enabled),
            config,
            anonymous_id,
            session_id: random_id(),
            sequence: AtomicU64::new(0),
        })
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed) && self.config.endpoint.is_some()
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// Durably enqueue an event. Failure is returned for tests and diagnostics,
    /// but callers should keep telemetry best-effort and never fail product work.
    pub fn capture(&self, name: &str, properties: EventProperties) -> io::Result<bool> {
        if !self.enabled() {
            return Ok(false);
        }
        validate_event_name(name)?;
        validate_properties(&properties)?;

        let event = TelemetryEvent {
            schema_version: TELEMETRY_SCHEMA_VERSION,
            event_id: self.next_event_id(),
            occurred_at_ms: unix_ms(),
            anonymous_id: self.anonymous_id.clone(),
            session_id: self.session_id.clone(),
            app: self.config.app.to_string(),
            version: self.config.version.to_string(),
            build_id: bounded_token(&self.config.build_id, 128)?,
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            name: name.to_string(),
            properties,
        };
        write_event(&self.spool_dir(), &event)?;
        trim_spool(&self.spool_dir(), MAX_QUEUED_EVENTS)?;
        Ok(true)
    }

    /// Send one batch and remove only events acknowledged with a 2xx response.
    pub fn flush(&self) -> Result<usize, FlushError> {
        if !self.enabled() {
            return Ok(0);
        }
        let endpoint = self
            .config
            .endpoint
            .as_deref()
            .ok_or(FlushError::Disabled)?;
        let paths = queued_event_paths(&self.spool_dir())?
            .into_iter()
            .take(MAX_BATCH_EVENTS)
            .collect::<Vec<_>>();
        if paths.is_empty() {
            return Ok(0);
        }

        let mut events = Vec::with_capacity(paths.len());
        let mut accepted_paths = Vec::with_capacity(paths.len());
        for path in paths {
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            match serde_json::from_slice::<TelemetryEvent>(&bytes) {
                Ok(event) => {
                    events.push(event);
                    accepted_paths.push(path);
                }
                Err(_) => {
                    // A malformed spool entry cannot ever be delivered. Remove it
                    // without affecting valid neighboring events.
                    let _ = fs::remove_file(path);
                }
            }
        }
        if events.is_empty() {
            return Ok(0);
        }

        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::blocking::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        let response = client
            .post(endpoint)
            .header("content-type", "application/json")
            .json(&TelemetryBatch {
                schema_version: TELEMETRY_SCHEMA_VERSION,
                events: &events,
            })
            .send()?;
        if !response.status().is_success() {
            return Err(FlushError::HttpStatus(response.status().as_u16()));
        }
        for path in &accepted_paths {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(accepted_paths.len())
    }

    pub fn purge(&self) -> io::Result<()> {
        let directory = self.spool_dir();
        if !directory.exists() {
            return Ok(());
        }
        for path in queued_event_paths(&directory)? {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn spool_dir(&self) -> PathBuf {
        self.config.state_root.join(SPOOL_DIR_NAME)
    }

    fn next_event_id(&self) -> String {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        format!("{}-{sequence:016x}", random_id())
    }
}

#[derive(Debug)]
pub enum FlushError {
    Disabled,
    Io(io::Error),
    Http(reqwest::Error),
    HttpStatus(u16),
}

impl std::fmt::Display for FlushError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => formatter.write_str("telemetry is disabled"),
            Self::Io(error) => write!(formatter, "telemetry spool failed: {error}"),
            Self::Http(error) => write!(formatter, "telemetry delivery failed: {error}"),
            Self::HttpStatus(status) => {
                write!(formatter, "telemetry endpoint returned HTTP {status}")
            }
        }
    }
}

impl std::error::Error for FlushError {}

impl From<io::Error> for FlushError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<reqwest::Error> for FlushError {
    fn from(value: reqwest::Error) -> Self {
        Self::Http(value)
    }
}

fn write_event(directory: &Path, event: &TelemetryEvent) -> io::Result<()> {
    fs::create_dir_all(directory)?;
    let filename = format!("{:020}-{}.json", event.occurred_at_ms, event.event_id);
    let path = directory.join(filename);
    let temporary = path.with_extension("tmp");
    let bytes = serde_json::to_vec(event).map_err(io::Error::other)?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(temporary, path)
}

fn queued_event_paths(directory: &Path) -> io::Result<Vec<PathBuf>> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut paths = fs::read_dir(directory)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn trim_spool(directory: &Path, limit: usize) -> io::Result<()> {
    let paths = queued_event_paths(directory)?;
    let excess = paths.len().saturating_sub(limit);
    for path in paths.into_iter().take(excess) {
        let _ = fs::remove_file(path);
    }
    Ok(())
}

fn load_or_create_installation_id(state_root: &Path) -> io::Result<String> {
    fs::create_dir_all(state_root)?;
    let path = state_root.join(INSTALLATION_ID_FILENAME);
    match fs::read_to_string(&path) {
        Ok(value) if valid_id(value.trim()) => return Ok(value.trim().to_string()),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let id = random_id();
    match OpenOptions::new().create_new(true).write(true).open(&path) {
        Ok(mut file) => {
            file.write_all(id.as_bytes())?;
            file.sync_all()?;
            Ok(id)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let existing = fs::read_to_string(path)?;
            if valid_id(existing.trim()) {
                Ok(existing.trim().to_string())
            } else {
                Err(io::Error::new(io::ErrorKind::InvalidData, error))
            }
        }
        Err(error) => Err(error),
    }
}

fn validate_event_name(value: &str) -> io::Result<()> {
    bounded_token(value, 80).map(|_| ())
}

fn validate_properties(properties: &EventProperties) -> io::Result<()> {
    for value in [
        properties.code.as_deref(),
        properties.connector.as_deref(),
        properties.kind.as_deref(),
        properties.source_file.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        bounded_token(value, 160)?;
    }
    Ok(())
}

fn bounded_token(value: &str, max_len: usize) -> io::Result<String> {
    if value.is_empty()
        || value.len() > max_len
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "telemetry values must be bounded machine-readable tokens",
        ));
    }
    Ok(value.to_string())
}

fn valid_id(value: &str) -> bool {
    value.len() == 32 && value.chars().all(|character| character.is_ascii_hexdigit())
}

fn random_id() -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        let fallback = unix_ms().to_le_bytes();
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = fallback[index % fallback.len()] ^ (std::process::id() as u8);
        }
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read};
    use std::net::TcpListener;
    use std::thread;

    fn config(root: &Path, endpoint: Option<String>, enabled: bool) -> TelemetryConfig {
        TelemetryConfig {
            state_root: root.to_path_buf(),
            endpoint,
            enabled,
            app: "test",
            version: "1.2.3",
            build_id: "build-1".to_string(),
        }
    }

    #[test]
    fn disabled_client_does_not_create_spool() {
        let root = tempfile::tempdir().expect("temp dir");
        let client = TelemetryClient::new(config(root.path(), Some("http://unused".into()), false))
            .expect("client");
        assert!(
            !client
                .capture("app.started", EventProperties::default())
                .expect("capture")
        );
        assert!(!root.path().join(SPOOL_DIR_NAME).exists());
    }

    #[test]
    fn rejects_unbounded_or_human_text_properties() {
        let root = tempfile::tempdir().expect("temp dir");
        let client = TelemetryClient::new(config(root.path(), Some("http://unused".into()), true))
            .expect("client");
        let properties = EventProperties {
            code: Some("failed for saurabh@example.com".to_string()),
            ..EventProperties::default()
        };
        assert_eq!(
            client.capture("diagnostic", properties).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let path_properties = EventProperties {
            code: Some("/Users/example/private.md".to_string()),
            ..EventProperties::default()
        };
        assert_eq!(
            client
                .capture("diagnostic", path_properties)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn installation_identity_is_stable_across_clients() {
        let root = tempfile::tempdir().expect("temp dir");
        let first = TelemetryClient::new(config(root.path(), None, true)).expect("first");
        let second = TelemetryClient::new(config(root.path(), None, true)).expect("second");
        assert_eq!(first.anonymous_id, second.anonymous_id);
        assert_ne!(first.session_id, second.session_id);
    }

    #[test]
    fn successful_flush_delivers_and_removes_durable_events() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = format!(
            "http://{}/v1/telemetry/batch",
            listener.local_addr().unwrap()
        );
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut content_length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("header");
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = value.trim().parse().expect("content length");
                }
            }
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).expect("body");
            stream
                .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 2\r\n\r\n{}")
                .expect("response");
            serde_json::from_slice::<serde_json::Value>(&body).expect("json body")
        });

        let root = tempfile::tempdir().expect("temp dir");
        let client =
            TelemetryClient::new(config(root.path(), Some(endpoint), true)).expect("client");
        assert!(
            client
                .capture(
                    "activity.completed",
                    EventProperties {
                        kind: Some("connect".to_string()),
                        outcome: Some(Outcome::Succeeded),
                        ..EventProperties::default()
                    }
                )
                .expect("capture")
        );
        assert_eq!(client.flush().expect("flush"), 1);
        assert!(queued_event_paths(&client.spool_dir()).unwrap().is_empty());

        let body = server.join().expect("server");
        assert_eq!(body["schema_version"], 1);
        assert_eq!(body["events"][0]["name"], "activity.completed");
        assert_eq!(body["events"][0]["properties"]["kind"], "connect");
        assert!(body["events"][0].get("message").is_none());
    }

    #[test]
    fn failed_flush_keeps_events_for_retry() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = format!(
            "http://{}/v1/telemetry/batch",
            listener.local_addr().unwrap()
        );
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = stream
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 2\r\n\r\n{}");
        });
        let root = tempfile::tempdir().expect("temp dir");
        let client =
            TelemetryClient::new(config(root.path(), Some(endpoint), true)).expect("client");
        client
            .capture("app.started", EventProperties::default())
            .expect("capture");
        assert!(matches!(client.flush(), Err(FlushError::HttpStatus(503))));
        server.join().expect("server");
        assert_eq!(queued_event_paths(&client.spool_dir()).unwrap().len(), 1);
    }
}
