use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::net::UnixStream;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const DEFAULT_TCP_ADDR: &str = "127.0.0.1:38567";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum DaemonRequest {
    Ping,
    Pull {
        path: PathBuf,
    },
    Push {
        path: PathBuf,
        assume_yes: bool,
        confirm_dangerous: bool,
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
    Io(String),
    Protocol(String),
}

impl DaemonClientError {
    pub fn message(&self) -> &str {
        match self {
            Self::NotAvailable(message) | Self::Io(message) | Self::Protocol(message) => message,
        }
    }
}

pub fn socket_path(state_root: &Path) -> PathBuf {
    state_root.join("afsd.sock")
}

pub fn default_tcp_addr() -> SocketAddr {
    DEFAULT_TCP_ADDR
        .parse()
        .expect("default daemon TCP address is valid")
}

#[cfg(unix)]
pub fn send_request(
    state_root: &Path,
    request: &DaemonRequest,
) -> Result<DaemonResponse, DaemonClientError> {
    let path = socket_path(state_root);
    let mut stream = UnixStream::connect(&path)
        .map_err(|error| DaemonClientError::NotAvailable(error.to_string()))?;
    write_json_line(&mut stream, request)
        .map_err(|error| DaemonClientError::Io(error.to_string()))?;
    read_response(stream)
}

pub fn send_tcp_request(
    addr: SocketAddr,
    request: &DaemonRequest,
) -> Result<DaemonResponse, DaemonClientError> {
    let mut stream = TcpStream::connect(addr)
        .map_err(|error| DaemonClientError::NotAvailable(error.to_string()))?;
    write_json_line(&mut stream, request)
        .map_err(|error| DaemonClientError::Io(error.to_string()))?;
    read_response(stream)
}

#[cfg(not(unix))]
pub fn send_request(
    _state_root: &Path,
    _request: &DaemonRequest,
) -> Result<DaemonResponse, DaemonClientError> {
    Err(DaemonClientError::NotAvailable(
        "daemon IPC is only implemented on Unix sockets".to_string(),
    ))
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
    reader
        .read_line(&mut line)
        .map_err(|error| DaemonClientError::Io(error.to_string()))?;
    if line.trim().is_empty() {
        return Err(DaemonClientError::Protocol(
            "daemon returned an empty response".to_string(),
        ));
    }
    serde_json::from_str(&line).map_err(|error| DaemonClientError::Protocol(error.to_string()))
}

fn write_json_line<T>(writer: &mut impl Write, value: &T) -> std::io::Result<()>
where
    T: Serialize,
{
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}
