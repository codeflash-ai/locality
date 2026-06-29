//! Host-side MCP endpoint served by the daemon.
//!
//! This endpoint is intentionally dormant unless an MCP client connects. It
//! exposes Locality as a CLI-shaped tool so sandboxed agents can fall back to MCP
//! when they cannot execute the host `loc` binary directly.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Value, json};

pub const DEFAULT_MCP_ADDR: &str = "127.0.0.1:38568";
pub const MCP_TOKEN_FILE_NAME: &str = "mcp-token";
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const DEFAULT_TIMEOUT_MS: u64 = 60_000;
const MAX_TIMEOUT_MS: u64 = 300_000;
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MCP_TOKEN_BYTES: usize = 32;
const MIN_MCP_TOKEN_LEN: usize = 32;

const AGENT_GUIDE: &str = r#"Locality MCP fallback

Prefer the native `loc` CLI when it is available in your shell. Use the MCP
`loc` tool only when the local agent sandbox cannot execute the host CLI.

Call the MCP tool with arguments exactly as you would pass them after `loc`.
Examples:

- {"argv":["--help"]}
- {"argv":["status","/Users/me/Library/CloudStorage/Locality/notion","--json"]}
- {"argv":["diff","/Users/me/Library/CloudStorage/Locality/notion/Plan/page.md","--json"]}
- {"argv":["pull","/Users/me/Library/CloudStorage/Locality/notion/Plan/page.md","--json"]}
- {"argv":["push","/Users/me/Library/CloudStorage/Locality/notion/Plan/page.md","--yes","--json"]}

Regular online-only files hydrate automatically when opened. Use `loc pull`
only when you need to force a clean local file to match the latest remote copy.
Leave local edits pending for user review unless the user explicitly asks you to
push them.
"#;

#[derive(Clone, Debug)]
pub struct McpServerConfig {
    loc_bin: PathBuf,
    token: String,
}

impl McpServerConfig {
    pub fn discover(state_root: &Path) -> Result<Self, String> {
        Ok(Self {
            loc_bin: discover_loc_binary(),
            token: ensure_mcp_token(state_root)?,
        })
    }

    #[cfg(test)]
    fn with_loc_bin(loc_bin: PathBuf) -> Self {
        Self {
            loc_bin,
            token: "test-token".to_string(),
        }
    }
}

pub fn default_mcp_addr() -> SocketAddr {
    DEFAULT_MCP_ADDR
        .parse()
        .expect("DEFAULT_MCP_ADDR must be a valid socket address")
}

pub fn discover_loc_binary() -> PathBuf {
    if let Ok(value) = env::var("LOCALITY_MCP_LOCALITY_BIN")
        && !value.trim().is_empty()
    {
        return PathBuf::from(value);
    }
    if let Ok(current) = env::current_exe()
        && let Some(parent) = current.parent()
    {
        let sibling = parent.join(binary_name("loc"));
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from(binary_name("loc"))
}

pub fn mcp_token_path(state_root: &Path) -> PathBuf {
    state_root.join(MCP_TOKEN_FILE_NAME)
}

pub fn ensure_mcp_token(state_root: &Path) -> Result<String, String> {
    let path = mcp_token_path(state_root);
    if let Ok(existing) = fs::read_to_string(&path) {
        let token = existing.trim();
        if token.len() >= MIN_MCP_TOKEN_LEN {
            return Ok(token.to_string());
        }
    }

    fs::create_dir_all(state_root)
        .map_err(|error| format!("could not create MCP token directory: {error}"))?;
    let token = generate_mcp_token()?;
    write_private_token_file(&path, &token)?;
    Ok(token)
}

fn generate_mcp_token() -> Result<String, String> {
    let mut bytes = [0u8; MCP_TOKEN_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("could not generate MCP token: {error}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn write_private_token_file(path: &Path, token: &str) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options
        .open(path)
        .map_err(|error| format!("could not create MCP token file: {error}"))?;
    file.write_all(token.as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .map_err(|error| format!("could not write MCP token file: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("could not protect MCP token file: {error}"))?;
    }
    Ok(())
}

fn binary_name(name: &str) -> String {
    #[cfg(windows)]
    {
        format!("{name}.exe")
    }
    #[cfg(not(windows))]
    {
        name.to_string()
    }
}

pub fn serve_http(listener: TcpListener, config: McpServerConfig) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let config = config.clone();
                thread::spawn(move || handle_http_connection(stream, &config));
            }
            Err(error) => eprintln!("localityd MCP accept failed: {error}"),
        }
    }
}

pub fn serve_stdio(config: McpServerConfig) -> Result<(), String> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();

    for line in stdin.lock().lines() {
        let line = line.map_err(|error| format!("failed to read MCP stdin: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        match handle_json_rpc(&line, &config) {
            Ok(Some(response)) | Err(response) => {
                serde_json::to_writer(&mut stdout, &response)
                    .map_err(|error| format!("failed to encode MCP response: {error}"))?;
                stdout
                    .write_all(b"\n")
                    .and_then(|_| stdout.flush())
                    .map_err(|error| format!("failed to write MCP stdout: {error}"))?;
            }
            Ok(None) => {}
        }
    }

    Ok(())
}

fn handle_http_connection(mut stream: TcpStream, config: &McpServerConfig) {
    let response = match read_http_request(&mut stream) {
        Ok(request) => handle_http_request(request, config),
        Err(error) => http_json_response(
            400,
            "Bad Request",
            error_response(Value::Null, -32700, error),
        ),
    };
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn handle_http_request(request: HttpRequest, config: &McpServerConfig) -> String {
    if !origin_allowed(request.origin.as_deref()) {
        return http_json_response(
            403,
            "Forbidden",
            error_response(Value::Null, -32600, "forbidden Origin header"),
        );
    }
    match (request.method.as_str(), request.path.as_str()) {
        ("OPTIONS", _) => http_empty_response(204, "No Content"),
        ("GET", "/health") => http_json_response(200, "OK", json!({"status":"ok"})),
        ("POST", "/mcp") => {
            if !request_token_allowed(&request, &config.token) {
                return http_json_response(
                    401,
                    "Unauthorized",
                    error_response(Value::Null, -32001, "missing or invalid MCP token"),
                );
            }
            if request.body.trim().is_empty() {
                return http_json_response(
                    400,
                    "Bad Request",
                    error_response(Value::Null, -32700, "empty JSON-RPC request"),
                );
            }
            match handle_json_rpc(&request.body, config) {
                Ok(Some(response)) => http_json_response(200, "OK", response),
                Ok(None) => http_empty_response(202, "Accepted"),
                Err(response) => http_json_response(200, "OK", response),
            }
        }
        ("POST", _) => http_json_response(
            404,
            "Not Found",
            error_response(Value::Null, -32601, "unknown MCP endpoint"),
        ),
        _ => http_json_response(
            405,
            "Method Not Allowed",
            error_response(Value::Null, -32600, "unsupported MCP HTTP method"),
        ),
    }
}

fn handle_json_rpc(line: &str, config: &McpServerConfig) -> Result<Option<Value>, Value> {
    let parsed: Result<JsonRpcRequest, _> = serde_json::from_str(line);
    let request = match parsed {
        Ok(request) => request,
        Err(error) => {
            return Err(error_response(
                Value::Null,
                -32700,
                format!("parse error: {error}"),
            ));
        }
    };
    let Some(id) = request.id.clone() else {
        return Ok(None);
    };
    let Some(method) = request.method.as_deref() else {
        return Err(error_response(id, -32600, "missing method"));
    };

    let result = match method {
        "initialize" => initialize_result(),
        "ping" => json!({}),
        "tools/list" => tools_list_result(),
        "tools/call" => tools_call_result(request.params, config)
            .map_err(|message| error_response(id.clone(), -32602, message))?,
        "resources/list" => resources_list_result(),
        "resources/read" => resources_read_result(request.params, config)
            .map_err(|message| error_response(id.clone(), -32602, message))?,
        _ => {
            return Err(error_response(
                id,
                -32601,
                format!("unknown method `{method}`"),
            ));
        }
    };

    Ok(Some(success_response(id, result)))
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": {
            "tools": {},
            "resources": {}
        },
        "serverInfo": {
            "name": "loc",
            "version": env!("CARGO_PKG_VERSION")
        },
        "instructions": "Prefer the native loc CLI when available. If the sandbox cannot execute loc, call the MCP loc tool with argv exactly as CLI arguments after `loc`."
    })
}

fn tools_list_result() -> Value {
    json!({
        "tools": [
            {
                "name": "loc",
                "title": "Locality CLI",
                "description": "Run an Locality CLI command on the host. Prefer direct shell usage of `loc` when available; use this MCP fallback when the agent sandbox cannot access the host CLI.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "argv": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Arguments to pass after `loc`, for example [\"status\", \"/path\", \"--json\"]."
                        },
                        "cwd": {
                            "type": "string",
                            "description": "Optional host working directory for the command."
                        },
                        "timeoutMs": {
                            "type": "integer",
                            "minimum": 1000,
                            "maximum": MAX_TIMEOUT_MS,
                            "description": "Optional command timeout in milliseconds. Defaults to 60000."
                        }
                    },
                    "required": ["argv"],
                    "additionalProperties": false
                }
            }
        ]
    })
}

fn tools_call_result(params: Option<Value>, config: &McpServerConfig) -> Result<Value, String> {
    let params: ToolCallParams = serde_json::from_value(params.unwrap_or(Value::Null))
        .map_err(|error| format!("invalid tools/call params: {error}"))?;
    if params.name != "loc" {
        return Err(format!("unknown tool `{}`", params.name));
    }
    let args: LocalityToolArguments = serde_json::from_value(params.arguments)
        .map_err(|error| format!("invalid loc arguments: {error}"))?;
    let execution = execute_loc_tool(args, config)?;
    let report = execution_report(&execution);
    Ok(json!({
        "content": [
            {
                "type": "text",
                "text": serde_json::to_string_pretty(&report).unwrap_or_else(|_| report.to_string())
            }
        ],
        "isError": execution.timed_out || execution.exit_code.unwrap_or(1) != 0
    }))
}

fn resources_list_result() -> Value {
    json!({
        "resources": [
            {
                "uri": "loc://help",
                "name": "Locality CLI help",
                "description": "Top-level `loc --help` output.",
                "mimeType": "text/plain"
            },
            {
                "uri": "loc://agent-guide",
                "name": "Locality agent guide",
                "description": "Short instructions for using Locality from sandboxed agents.",
                "mimeType": "text/plain"
            },
            {
                "uri": "loc://mounts",
                "name": "Locality mount info",
                "description": "`loc info --json` output for the current host.",
                "mimeType": "application/json"
            }
        ]
    })
}

fn resources_read_result(params: Option<Value>, config: &McpServerConfig) -> Result<Value, String> {
    let params: ResourceReadParams = serde_json::from_value(params.unwrap_or(Value::Null))
        .map_err(|error| format!("invalid resources/read params: {error}"))?;
    let (mime_type, text) = match params.uri.as_str() {
        "loc://help" => (
            "text/plain",
            run_resource_command(config, ["--help"].as_slice()).stdout,
        ),
        "loc://agent-guide" => ("text/plain", AGENT_GUIDE.to_string()),
        "loc://mounts" => {
            let execution = run_resource_command(config, ["info", "--json"].as_slice());
            let text = if execution.stdout.trim().is_empty() && !execution.stderr.trim().is_empty()
            {
                execution.stderr
            } else {
                execution.stdout
            };
            ("application/json", text)
        }
        other => return Err(format!("unknown resource `{other}`")),
    };

    Ok(json!({
        "contents": [
            {
                "uri": params.uri,
                "mimeType": mime_type,
                "text": text
            }
        ]
    }))
}

fn execute_loc_tool(
    args: LocalityToolArguments,
    config: &McpServerConfig,
) -> Result<CliExecution, String> {
    validate_argv(&args.argv)?;
    let timeout_ms = args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    if !(1_000..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
        return Err(format!(
            "timeoutMs must be between 1000 and {MAX_TIMEOUT_MS}"
        ));
    }
    run_cli(
        config,
        &args.argv,
        args.cwd.as_deref(),
        Duration::from_millis(timeout_ms),
    )
}

fn validate_argv(argv: &[String]) -> Result<(), String> {
    if argv.iter().any(|arg| arg.contains('\0')) {
        return Err("argv must not contain NUL bytes".to_string());
    }
    Ok(())
}

fn run_resource_command(config: &McpServerConfig, argv: &[&str]) -> CliExecution {
    let argv = argv
        .iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>();
    run_cli(
        config,
        &argv,
        None,
        Duration::from_millis(DEFAULT_TIMEOUT_MS),
    )
    .unwrap_or_else(|error| CliExecution {
        command: command_display(config, &argv),
        exit_code: Some(1),
        timed_out: false,
        stdout: String::new(),
        stderr: error,
    })
}

fn run_cli(
    config: &McpServerConfig,
    argv: &[String],
    cwd: Option<&str>,
    timeout: Duration,
) -> Result<CliExecution, String> {
    let mut command = Command::new(&config.loc_bin);
    command
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start `{}`: {error}", config.loc_bin.display()))?;

    let started = Instant::now();
    loop {
        if child
            .try_wait()
            .map_err(|error| format!("could not wait for loc: {error}"))?
            .is_some()
        {
            let output = child
                .wait_with_output()
                .map_err(|error| format!("could not read loc output: {error}"))?;
            return Ok(CliExecution {
                command: command_display(config, argv),
                exit_code: output.status.code(),
                timed_out: false,
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .map_err(|error| format!("could not read timed-out loc output: {error}"))?;
            return Ok(CliExecution {
                command: command_display(config, argv),
                exit_code: output.status.code(),
                timed_out: true,
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn execution_report(execution: &CliExecution) -> Value {
    let stdout_json = serde_json::from_str::<Value>(execution.stdout.trim()).ok();
    json!({
        "command": execution.command,
        "exitCode": execution.exit_code,
        "timedOut": execution.timed_out,
        "stdout": execution.stdout,
        "stderr": execution.stderr,
        "stdoutJson": stdout_json
    })
}

fn command_display(config: &McpServerConfig, argv: &[String]) -> Vec<String> {
    std::iter::once(config.loc_bin.display().to_string())
        .chain(argv.iter().cloned())
        .collect()
}

fn success_response(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn error_response(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message.into()
        }
    })
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    origin: Option<String>,
    authorization: Option<String>,
    loc_mcp_token: Option<String>,
    body: String,
}

fn read_http_request(stream: &mut impl Read) -> Result<HttpRequest, String> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|error| format!("could not read request line: {error}"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| "missing HTTP method".to_string())?
        .to_string();
    let path = parts
        .next()
        .ok_or_else(|| "missing HTTP path".to_string())?
        .to_string();

    let mut content_length = 0usize;
    let mut origin = None;
    let mut authorization = None;
    let mut loc_mcp_token = None;
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|error| format!("could not read header: {error}"))?;
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            match name.as_str() {
                "content-length" => {
                    content_length = value
                        .parse::<usize>()
                        .map_err(|_| "invalid Content-Length".to_string())?;
                    if content_length > MAX_REQUEST_BYTES {
                        return Err(format!("request body exceeds {MAX_REQUEST_BYTES} bytes"));
                    }
                }
                "origin" => origin = Some(value),
                "authorization" => authorization = Some(value),
                "x-loc-mcp-token" => loc_mcp_token = Some(value),
                _ => {}
            }
        }
    }

    let mut body = vec![0; content_length];
    reader
        .read_exact(&mut body)
        .map_err(|error| format!("could not read body: {error}"))?;
    Ok(HttpRequest {
        method,
        path,
        origin,
        authorization,
        loc_mcp_token,
        body: String::from_utf8_lossy(&body).to_string(),
    })
}

fn request_token_allowed(request: &HttpRequest, expected: &str) -> bool {
    request
        .authorization
        .as_deref()
        .and_then(bearer_token)
        .is_some_and(|token| constant_time_eq(token.as_bytes(), expected.as_bytes()))
        || request
            .loc_mcp_token
            .as_deref()
            .is_some_and(|token| constant_time_eq(token.as_bytes(), expected.as_bytes()))
}

fn bearer_token(header: &str) -> Option<&str> {
    let (scheme, token) = header.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then_some(token.trim())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let diff = left
        .iter()
        .zip(right.iter())
        .fold(0u8, |acc, (left, right)| acc | (left ^ right));
    diff == 0
}

fn origin_allowed(origin: Option<&str>) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    origin == "null"
        || origin.starts_with("http://127.0.0.1:")
        || origin.starts_with("http://localhost:")
        || origin.starts_with("https://127.0.0.1:")
        || origin.starts_with("https://localhost:")
}

fn http_json_response(status: u16, reason: &str, body: Value) -> String {
    let body = body.to_string();
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nAccess-Control-Allow-Headers: authorization,content-type,mcp-session-id,mcp-protocol-version,x-loc-mcp-token\r\nAccess-Control-Allow-Methods: GET,POST,OPTIONS\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn http_empty_response(status: u16, reason: &str) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\nAccess-Control-Allow-Headers: authorization,content-type,mcp-session-id,mcp-protocol-version,x-loc-mcp-token\r\nAccess-Control-Allow-Methods: GET,POST,OPTIONS\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    id: Option<Value>,
    method: Option<String>,
    params: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallParams {
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocalityToolArguments {
    argv: Vec<String>,
    cwd: Option<String>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ResourceReadParams {
    uri: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CliExecution {
    command: Vec<String>,
    exit_code: Option<i32>,
    timed_out: bool,
    stdout: String,
    stderr: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_list_exposes_single_cli_shaped_tool() {
        let result = tools_list_result();
        let tools = result["tools"].as_array().expect("tools array");

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "loc");
        assert_eq!(tools[0]["inputSchema"]["required"][0], "argv");
    }

    #[test]
    fn execution_report_includes_parsed_stdout_json_when_available() {
        let report = execution_report(&CliExecution {
            command: vec![
                "loc".to_string(),
                "status".to_string(),
                "--json".to_string(),
            ],
            exit_code: Some(0),
            timed_out: false,
            stdout: r#"{"ok":true}"#.to_string(),
            stderr: String::new(),
        });

        assert_eq!(report["stdoutJson"]["ok"], true);
    }

    #[test]
    fn argv_validation_rejects_embedded_nul_bytes() {
        let argv = vec!["status".to_string(), "bad\0path".to_string()];

        assert_eq!(
            validate_argv(&argv),
            Err("argv must not contain NUL bytes".to_string())
        );
    }

    #[test]
    fn notification_messages_do_not_emit_responses() {
        let config = McpServerConfig::with_loc_bin(PathBuf::from("loc"));
        let response = handle_json_rpc(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            &config,
        )
        .expect("notification is valid");

        assert!(response.is_none());
    }

    #[test]
    fn http_post_mcp_returns_initialize_response() {
        let config = McpServerConfig::with_loc_bin(PathBuf::from("loc"));
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
        let request = HttpRequest {
            method: "POST".to_string(),
            path: "/mcp".to_string(),
            origin: None,
            authorization: Some("Bearer test-token".to_string()),
            loc_mcp_token: None,
            body: body.to_string(),
        };
        let response = handle_http_request(request, &config);

        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("\"serverInfo\":{\"name\":\"loc\""));
    }

    #[test]
    fn http_rejects_non_localhost_origin() {
        let config = McpServerConfig::with_loc_bin(PathBuf::from("loc"));
        let request = HttpRequest {
            method: "POST".to_string(),
            path: "/mcp".to_string(),
            origin: Some("https://evil.example".to_string()),
            authorization: Some("Bearer test-token".to_string()),
            loc_mcp_token: None,
            body: r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#.to_string(),
        };
        let response = handle_http_request(request, &config);

        assert!(response.starts_with("HTTP/1.1 403 Forbidden"));
    }

    #[test]
    fn http_rejects_missing_mcp_token() {
        let config = McpServerConfig::with_loc_bin(PathBuf::from("loc"));
        let request = HttpRequest {
            method: "POST".to_string(),
            path: "/mcp".to_string(),
            origin: None,
            authorization: None,
            loc_mcp_token: None,
            body: r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#.to_string(),
        };
        let response = handle_http_request(request, &config);

        assert!(response.starts_with("HTTP/1.1 401 Unauthorized"));
    }

    #[test]
    fn http_accepts_private_mcp_token_header() {
        let config = McpServerConfig::with_loc_bin(PathBuf::from("loc"));
        let request = HttpRequest {
            method: "POST".to_string(),
            path: "/mcp".to_string(),
            origin: None,
            authorization: None,
            loc_mcp_token: Some("test-token".to_string()),
            body: r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#.to_string(),
        };
        let response = handle_http_request(request, &config);

        assert!(response.starts_with("HTTP/1.1 200 OK"));
    }

    #[test]
    fn ensure_mcp_token_reuses_existing_private_token() {
        let root = env::temp_dir().join(format!(
            "loc-mcp-token-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("create temp root");
        fs::write(
            mcp_token_path(&root),
            "existing-token-with-enough-length-123\n",
        )
        .expect("write token");

        let token = ensure_mcp_token(&root).expect("load token");

        assert_eq!(token, "existing-token-with-enough-length-123");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn discover_loc_prefers_env_override() {
        let previous = env::var_os("LOCALITY_MCP_LOCALITY_BIN");
        // SAFETY: This unit test mutates process environment before restoring it
        // and does not rely on parallel environment-sensitive behavior.
        unsafe {
            env::set_var("LOCALITY_MCP_LOCALITY_BIN", "/tmp/custom-loc");
        }

        assert_eq!(
            discover_loc_binary(),
            std::path::Path::new("/tmp/custom-loc")
        );

        // SAFETY: Restores the environment to its previous value for other tests.
        unsafe {
            if let Some(previous) = previous {
                env::set_var("LOCALITY_MCP_LOCALITY_BIN", previous);
            } else {
                env::remove_var("LOCALITY_MCP_LOCALITY_BIN");
            }
        }
    }
}
