use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::net::UnixStream;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const DEFAULT_TCP_ADDR: &str = "127.0.0.1:38567";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum DaemonRequest {
    Ping,
    Status,
    ReloadMounts,
    Pull {
        path: PathBuf,
    },
    Push {
        path: PathBuf,
        assume_yes: bool,
        confirm_dangerous: bool,
    },
    Hydrate {
        mount_id: String,
        remote_id: String,
        path: PathBuf,
    },
    VirtualFsItem {
        mount_id: String,
        identifier: String,
    },
    VirtualFsChildren {
        mount_id: String,
        container_identifier: String,
    },
    VirtualFsMaterialize {
        mount_id: String,
        identifier: String,
    },
    VirtualFsCommitWrite {
        mount_id: String,
        identifier: String,
        contents_base64: String,
    },
    VirtualFsCreateFile {
        mount_id: String,
        parent_identifier: String,
        filename: String,
    },
    VirtualFsCreateDirectory {
        mount_id: String,
        parent_identifier: String,
        dirname: String,
    },
    VirtualFsRename {
        mount_id: String,
        identifier: String,
        new_parent_identifier: String,
        new_filename: String,
    },
    VirtualFsTrash {
        mount_id: String,
        identifier: String,
    },
    FileProviderItem {
        mount_id: String,
        identifier: String,
    },
    FileProviderChildren {
        mount_id: String,
        container_identifier: String,
    },
    FileProviderMaterialize {
        mount_id: String,
        identifier: String,
    },
    FileProviderRead {
        mount_id: String,
        identifier: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatusReport {
    pub status: String,
    pub build: DaemonBuildInfo,
    pub runtime: DaemonRuntimeStatus,
    pub watches: DaemonWatchStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonBuildInfo {
    pub version: String,
    pub build_id: String,
}

impl DaemonBuildInfo {
    pub fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            build_id: option_env!("AFS_BUILD_ID").unwrap_or("unknown").to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonRuntimeStatus {
    pub active_job: bool,
    pub active_job_detail: Option<DaemonActiveJobStatus>,
    pub pending_requests: usize,
    pub pending_hydrations: usize,
    pub deferred_hydrations: usize,
    pub pending_freshness: usize,
    pub ready_freshness: usize,
    pub deferred_freshness: usize,
    pub freshness_budget_units: u16,
    pub ready_freshness_budget_units: u16,
    pub pending_scheduled_pull: bool,
    pub scheduler_mode: String,
    pub active_interval_ms: u64,
    pub cold_interval_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonActiveJobStatus {
    pub kind: String,
    pub target: Option<String>,
    pub elapsed_ms: u64,
    pub started_at_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonWatchStatus {
    pub watched_mounts: usize,
    pub watched_roots: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonReloadReport {
    pub added: usize,
    pub removed: usize,
    pub unchanged: usize,
    pub watches: DaemonWatchStatus,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DaemonResponse {
    pub ok: bool,
    pub payload: Option<Value>,
    pub error: Option<DaemonErrorResponse>,
}

impl DaemonResponse {
    pub fn ok(payload: impl Serialize) -> Self {
        match serde_json::to_value(payload) {
            Ok(payload) => Self {
                ok: true,
                payload: Some(payload),
                error: None,
            },
            Err(error) => Self::error("json_encode_failed", error.to_string()),
        }
    }

    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            payload: None,
            error: Some(DaemonErrorResponse {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonErrorResponse {
    pub code: String,
    pub message: String,
}

#[derive(Debug)]
pub enum DaemonClientError {
    NotAvailable(String),
    TimedOut(String),
    Io(String),
    Protocol(String),
}

impl DaemonClientError {
    pub fn message(&self) -> &str {
        match self {
            Self::NotAvailable(message)
            | Self::TimedOut(message)
            | Self::Io(message)
            | Self::Protocol(message) => message,
        }
    }
}

pub fn socket_path(state_root: &Path) -> PathBuf {
    afs_platform::daemon_socket_path(state_root)
}

pub fn default_tcp_addr() -> SocketAddr {
    DEFAULT_TCP_ADDR
        .parse()
        .expect("default daemon TCP address is valid")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DaemonEndpoint {
    UnixSocket(PathBuf),
    LocalTcp(SocketAddr),
    WindowsNamedPipe(String),
}

impl DaemonEndpoint {
    pub fn for_state_root(state_root: &Path) -> Result<Self, DaemonClientError> {
        #[cfg(unix)]
        {
            Ok(Self::UnixSocket(socket_path(state_root)))
        }

        #[cfg(not(unix))]
        {
            let _ = state_root;
            configured_tcp_addr().map(Self::LocalTcp)
        }
    }
}

pub trait DaemonTransport {
    fn send(
        &self,
        request: &DaemonRequest,
        timeout: Duration,
    ) -> Result<DaemonResponse, DaemonClientError>;
}

impl DaemonTransport for DaemonEndpoint {
    fn send(
        &self,
        request: &DaemonRequest,
        timeout: Duration,
    ) -> Result<DaemonResponse, DaemonClientError> {
        send_endpoint_request_with_timeout(self, request, timeout)
    }
}

pub fn send_endpoint_request(
    endpoint: &DaemonEndpoint,
    request: &DaemonRequest,
) -> Result<DaemonResponse, DaemonClientError> {
    match endpoint {
        DaemonEndpoint::UnixSocket(path) => send_unix_socket_request(path, request),
        DaemonEndpoint::LocalTcp(addr) => send_tcp_request(*addr, request),
        DaemonEndpoint::WindowsNamedPipe(name) => Err(DaemonClientError::NotAvailable(format!(
            "daemon named pipe IPC `{name}` is not implemented yet"
        ))),
    }
}

pub fn send_endpoint_request_with_timeout(
    endpoint: &DaemonEndpoint,
    request: &DaemonRequest,
    timeout: Duration,
) -> Result<DaemonResponse, DaemonClientError> {
    match endpoint {
        DaemonEndpoint::UnixSocket(path) => {
            send_unix_socket_request_with_timeout(path, request, timeout)
        }
        DaemonEndpoint::LocalTcp(addr) => send_tcp_request_with_timeout(*addr, request, timeout),
        DaemonEndpoint::WindowsNamedPipe(name) => Err(DaemonClientError::NotAvailable(format!(
            "daemon named pipe IPC `{name}` is not implemented yet"
        ))),
    }
}

#[cfg(unix)]
pub fn send_request(
    state_root: &Path,
    request: &DaemonRequest,
) -> Result<DaemonResponse, DaemonClientError> {
    let endpoint = DaemonEndpoint::for_state_root(state_root)?;
    send_endpoint_request(&endpoint, request)
}

#[cfg(unix)]
pub fn send_request_with_timeout(
    state_root: &Path,
    request: &DaemonRequest,
    timeout: Duration,
) -> Result<DaemonResponse, DaemonClientError> {
    let endpoint = DaemonEndpoint::for_state_root(state_root)?;
    send_endpoint_request_with_timeout(&endpoint, request, timeout)
}

#[cfg(unix)]
fn send_unix_socket_request(
    path: &Path,
    request: &DaemonRequest,
) -> Result<DaemonResponse, DaemonClientError> {
    let mut stream = UnixStream::connect(path)
        .map_err(|error| DaemonClientError::NotAvailable(error.to_string()))?;
    write_json_line(&mut stream, request).map_err(daemon_io_error)?;
    read_response(stream)
}

#[cfg(unix)]
fn send_unix_socket_request_with_timeout(
    path: &Path,
    request: &DaemonRequest,
    timeout: Duration,
) -> Result<DaemonResponse, DaemonClientError> {
    let mut stream = UnixStream::connect(path)
        .map_err(|error| DaemonClientError::NotAvailable(error.to_string()))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(daemon_io_error)?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(daemon_io_error)?;
    write_json_line(&mut stream, request).map_err(daemon_io_error)?;
    read_response(stream)
}

#[cfg(not(unix))]
fn send_unix_socket_request(
    path: &Path,
    _request: &DaemonRequest,
) -> Result<DaemonResponse, DaemonClientError> {
    Err(DaemonClientError::NotAvailable(format!(
        "Unix socket IPC is not available on this platform: {}",
        path.display()
    )))
}

#[cfg(not(unix))]
fn send_unix_socket_request_with_timeout(
    path: &Path,
    _request: &DaemonRequest,
    _timeout: Duration,
) -> Result<DaemonResponse, DaemonClientError> {
    Err(DaemonClientError::NotAvailable(format!(
        "Unix socket IPC is not available on this platform: {}",
        path.display()
    )))
}

pub fn send_tcp_request(
    addr: SocketAddr,
    request: &DaemonRequest,
) -> Result<DaemonResponse, DaemonClientError> {
    let mut stream = TcpStream::connect(addr)
        .map_err(|error| DaemonClientError::NotAvailable(error.to_string()))?;
    write_json_line(&mut stream, request).map_err(daemon_io_error)?;
    read_response(stream)
}

pub fn send_tcp_request_with_timeout(
    addr: SocketAddr,
    request: &DaemonRequest,
    timeout: Duration,
) -> Result<DaemonResponse, DaemonClientError> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|error| DaemonClientError::NotAvailable(error.to_string()))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(daemon_io_error)?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(daemon_io_error)?;
    write_json_line(&mut stream, request).map_err(daemon_io_error)?;
    read_response(stream)
}

#[cfg(not(unix))]
pub fn send_request(
    _state_root: &Path,
    request: &DaemonRequest,
) -> Result<DaemonResponse, DaemonClientError> {
    let endpoint = DaemonEndpoint::for_state_root(_state_root)?;
    send_endpoint_request(&endpoint, request)
}

#[cfg(not(unix))]
pub fn send_request_with_timeout(
    _state_root: &Path,
    request: &DaemonRequest,
    timeout: Duration,
) -> Result<DaemonResponse, DaemonClientError> {
    let endpoint = DaemonEndpoint::for_state_root(_state_root)?;
    send_endpoint_request_with_timeout(&endpoint, request, timeout)
}

#[cfg(not(unix))]
fn configured_tcp_addr() -> Result<SocketAddr, DaemonClientError> {
    match std::env::var("AFS_DAEMON_TCP_ADDR") {
        Ok(value) if matches!(value.as_str(), "0" | "off" | "none" | "disabled") => {
            Err(DaemonClientError::NotAvailable(
                "daemon TCP IPC is disabled by AFS_DAEMON_TCP_ADDR".to_string(),
            ))
        }
        Ok(value) => value.parse().map_err(|error| {
            DaemonClientError::Protocol(format!("invalid AFS_DAEMON_TCP_ADDR `{value}`: {error}"))
        }),
        Err(_) => Ok(default_tcp_addr()),
    }
}

pub fn read_request(stream: impl Read) -> Result<DaemonRequest, DaemonClientError> {
    read_json_line(stream)
}

pub fn write_response(
    stream: &mut impl Write,
    response: &DaemonResponse,
) -> Result<(), DaemonClientError> {
    write_json_line(stream, response).map_err(|error| DaemonClientError::Io(error.to_string()))
}

fn read_response(stream: impl Read) -> Result<DaemonResponse, DaemonClientError> {
    read_json_line(stream)
}

fn read_json_line<T>(stream: impl Read) -> Result<T, DaemonClientError>
where
    T: for<'de> Deserialize<'de>,
{
    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    reader.read_line(&mut line).map_err(daemon_io_error)?;
    if line.trim().is_empty() {
        return Err(DaemonClientError::Protocol(
            "daemon returned an empty response".to_string(),
        ));
    }
    serde_json::from_str(&line).map_err(|error| DaemonClientError::Protocol(error.to_string()))
}

fn daemon_io_error(error: io::Error) -> DaemonClientError {
    match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => {
            DaemonClientError::TimedOut(error.to_string())
        }
        _ => DaemonClientError::Io(error.to_string()),
    }
}

fn write_json_line<T>(writer: &mut impl Write, value: &T) -> std::io::Result<()>
where
    T: Serialize,
{
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::DaemonClientError;
    use super::{DaemonEndpoint, DaemonRequest, DaemonTransport};
    use std::time::Duration;

    #[test]
    fn virtual_fs_item_command_decodes_as_platform_neutral_request() {
        let request: DaemonRequest = serde_json::from_str(
            r#"{"command":"virtual_fs_item","mount_id":"notion-main","identifier":"page-1"}"#,
        )
        .expect("decode virtual fs item request");

        assert_eq!(
            request,
            DaemonRequest::VirtualFsItem {
                mount_id: "notion-main".to_string(),
                identifier: "page-1".to_string(),
            }
        );
    }

    #[test]
    fn file_provider_item_command_remains_a_compatibility_alias() {
        let request: DaemonRequest = serde_json::from_str(
            r#"{"command":"file_provider_item","mount_id":"notion-main","identifier":"page-1"}"#,
        )
        .expect("decode file provider item request");

        assert_eq!(
            request,
            DaemonRequest::FileProviderItem {
                mount_id: "notion-main".to_string(),
                identifier: "page-1".to_string(),
            }
        );
    }

    #[test]
    fn virtual_fs_commit_write_command_decodes() {
        let request: DaemonRequest = serde_json::from_str(
            r#"{"command":"virtual_fs_commit_write","mount_id":"notion-main","identifier":"page-1","contents_base64":"SGVsbG8="}"#,
        )
        .expect("decode virtual fs commit write request");

        assert_eq!(
            request,
            DaemonRequest::VirtualFsCommitWrite {
                mount_id: "notion-main".to_string(),
                identifier: "page-1".to_string(),
                contents_base64: "SGVsbG8=".to_string()
            }
        );
    }

    #[test]
    fn virtual_fs_mutation_commands_decode() {
        let create: DaemonRequest = serde_json::from_str(
            r#"{"command":"virtual_fs_create_file","mount_id":"notion-main","parent_identifier":"children:page-1","filename":"Draft.md"}"#,
        )
        .expect("decode virtual fs create request");
        let mkdir: DaemonRequest = serde_json::from_str(
            r#"{"command":"virtual_fs_create_directory","mount_id":"notion-main","parent_identifier":"children:page-1","dirname":"Draft"}"#,
        )
        .expect("decode virtual fs create directory request");
        let rename: DaemonRequest = serde_json::from_str(
            r#"{"command":"virtual_fs_rename","mount_id":"notion-main","identifier":"local:1","new_parent_identifier":"children:page-1","new_filename":"Updated.md"}"#,
        )
        .expect("decode virtual fs rename request");
        let trash: DaemonRequest = serde_json::from_str(
            r#"{"command":"virtual_fs_trash","mount_id":"notion-main","identifier":"page-1"}"#,
        )
        .expect("decode virtual fs trash request");

        assert_eq!(
            create,
            DaemonRequest::VirtualFsCreateFile {
                mount_id: "notion-main".to_string(),
                parent_identifier: "children:page-1".to_string(),
                filename: "Draft.md".to_string(),
            }
        );
        assert_eq!(
            mkdir,
            DaemonRequest::VirtualFsCreateDirectory {
                mount_id: "notion-main".to_string(),
                parent_identifier: "children:page-1".to_string(),
                dirname: "Draft".to_string(),
            }
        );
        assert_eq!(
            rename,
            DaemonRequest::VirtualFsRename {
                mount_id: "notion-main".to_string(),
                identifier: "local:1".to_string(),
                new_parent_identifier: "children:page-1".to_string(),
                new_filename: "Updated.md".to_string(),
            }
        );
        assert_eq!(
            trash,
            DaemonRequest::VirtualFsTrash {
                mount_id: "notion-main".to_string(),
                identifier: "page-1".to_string(),
            }
        );
    }

    #[test]
    fn file_provider_read_command_decodes() {
        let request: DaemonRequest = serde_json::from_str(
            r#"{"command":"file_provider_read","mount_id":"notion-main","identifier":"page-1"}"#,
        )
        .expect("decode file provider read request");

        assert_eq!(
            request,
            DaemonRequest::FileProviderRead {
                mount_id: "notion-main".to_string(),
                identifier: "page-1".to_string(),
            }
        );
    }

    #[test]
    fn named_pipe_endpoint_reports_not_implemented() {
        let endpoint = DaemonEndpoint::WindowsNamedPipe(r"\\.\pipe\afs-test".to_string());

        let error = endpoint
            .send(&DaemonRequest::Ping, Duration::from_millis(1))
            .expect_err("named pipe transport is not implemented yet");

        assert!(matches!(error, super::DaemonClientError::NotAvailable(_)));
        assert!(error.message().contains("named pipe IPC"));
    }

    #[cfg(unix)]
    #[test]
    fn state_root_endpoint_uses_unix_socket_on_unix() {
        let root = std::path::PathBuf::from("/tmp/afs-state");

        let endpoint = DaemonEndpoint::for_state_root(&root).expect("endpoint");

        assert_eq!(endpoint, DaemonEndpoint::UnixSocket(root.join("afsd.sock")));
    }

    #[cfg(unix)]
    #[test]
    fn unix_request_timeout_maps_to_timed_out_error() {
        use std::io::Read;
        use std::os::unix::net::UnixListener;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::thread;
        use std::time::Duration;

        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "afs-ipc-timeout-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("create state root");
        let listener = UnixListener::bind(super::socket_path(&root)).expect("bind socket");

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = [0; 128];
            let _ = stream.read(&mut request);
            thread::sleep(Duration::from_millis(200));
        });

        let result = super::send_request_with_timeout(
            &root,
            &DaemonRequest::Ping,
            Duration::from_millis(50),
        );

        assert!(matches!(result, Err(DaemonClientError::TimedOut(_))));
        server.join().expect("server thread");
        let _ = std::fs::remove_dir_all(root);
    }
}
