//! Local OAuth browser/callback flow shared by CLI and desktop entrypoints.
//!
//! Connector-specific code starts the remote authorization session, then uses
//! this module to open the browser, listen on the loopback redirect URI, verify
//! state, and return the authorization code. Secret exchange and persistence
//! stay in connector-specific orchestration functions.

use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::process::Command as ProcessCommand;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalOAuthAuthorization {
    pub code: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalRedirect {
    pub bind_addr: String,
    pub callback_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalOAuthError {
    pub code: String,
    pub message: String,
}

impl LocalOAuthError {
    fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

pub fn run_local_oauth_authorization(
    provider_name: &str,
    authorize_url: &str,
    redirect_uri: &str,
    expected_state: &str,
    no_browser: bool,
    quiet: bool,
) -> Result<LocalOAuthAuthorization, LocalOAuthError> {
    let redirect = local_redirect(redirect_uri)?;
    let listener = TcpListener::bind(&redirect.bind_addr).map_err(|error| {
        LocalOAuthError::new(
            "callback_bind_failed",
            format!(
                "failed to listen for {provider_name} OAuth callback at {}: {error}",
                redirect.bind_addr
            ),
        )
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|error| LocalOAuthError::new("callback_failed", error.to_string()))?;

    if !quiet {
        println!("opening {provider_name} authorization in your browser...");
        println!("callback: {redirect_uri}");
        println!("authorization URL: {authorize_url}");
    }
    if !no_browser
        && let Err(error) = open_browser(authorize_url)
        && !quiet
    {
        eprintln!("loc connect: failed to open browser: {error}");
        eprintln!("open the authorization URL manually");
    }

    wait_for_oauth_callback(
        provider_name,
        &listener,
        &redirect.callback_path,
        expected_state,
    )
}

fn wait_for_oauth_callback(
    provider_name: &str,
    listener: &TcpListener,
    callback_path: &str,
    expected_state: &str,
) -> Result<LocalOAuthAuthorization, LocalOAuthError> {
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut buffer = [0_u8; 8192];
                let read = stream
                    .read(&mut buffer)
                    .map_err(|error| LocalOAuthError::new("callback_failed", error.to_string()))?;
                let request = String::from_utf8_lossy(&buffer[..read]);
                let result = parse_oauth_callback(&request, callback_path, expected_state);
                let response = match &result {
                    Ok(_) => oauth_http_response(
                        &format!("{provider_name} connected"),
                        &format!(
                            "{provider_name} authorization is complete. You can close this window."
                        ),
                    ),
                    Err(error) => oauth_http_response("Locality OAuth failed", &error.message),
                };
                let _ = stream.write_all(response.as_bytes());
                match result {
                    Ok(authorization) => return Ok(authorization),
                    Err(error) if retryable_callback_error(&error) => continue,
                    Err(error) => return Err(error),
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(LocalOAuthError::new(
                        "oauth_timeout",
                        format!("timed out waiting for {provider_name} OAuth callback"),
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                return Err(LocalOAuthError::new("callback_failed", error.to_string()));
            }
        }
    }
}

fn retryable_callback_error(error: &LocalOAuthError) -> bool {
    matches!(
        error.code.as_str(),
        "callback_failed" | "oauth_state_mismatch" | "oauth_missing_code"
    )
}

pub fn parse_oauth_callback(
    request: &str,
    callback_path: &str,
    expected_state: &str,
) -> Result<LocalOAuthAuthorization, LocalOAuthError> {
    let request_line = request
        .lines()
        .next()
        .ok_or_else(|| LocalOAuthError::new("callback_failed", "empty callback"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    if method != "GET" {
        return Err(LocalOAuthError::new(
            "callback_failed",
            "OAuth callback used an unsupported HTTP method",
        ));
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != callback_path {
        return Err(LocalOAuthError::new(
            "callback_failed",
            format!("OAuth callback path `{path}` did not match `{callback_path}`"),
        ));
    }
    let params = query_params(query);
    if let Some(error) = params.get("error") {
        return Err(LocalOAuthError::new(
            "oauth_denied",
            params
                .get("error_description")
                .cloned()
                .unwrap_or_else(|| format!("OAuth provider returned OAuth error `{error}`")),
        ));
    }
    if params.get("state").map(String::as_str) != Some(expected_state) {
        return Err(LocalOAuthError::new(
            "oauth_state_mismatch",
            "OAuth callback state did not match",
        ));
    }
    let code = params
        .get("code")
        .filter(|code| !code.is_empty())
        .cloned()
        .ok_or_else(|| {
            LocalOAuthError::new(
                "oauth_missing_code",
                "OAuth callback did not include a code",
            )
        })?;
    Ok(LocalOAuthAuthorization { code })
}

fn oauth_http_response(title: &str, message: &str) -> String {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title></head><body><h1>{}</h1><p>{}</p></body></html>",
        html_escape(title),
        html_escape(title),
        html_escape(message)
    );
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

pub fn local_redirect(uri: &str) -> Result<LocalRedirect, LocalOAuthError> {
    let rest = uri.strip_prefix("http://").ok_or_else(|| {
        LocalOAuthError::new(
            "invalid_redirect_uri",
            "OAuth redirect URI must start with http://",
        )
    })?;
    let (host_port, path) = rest.split_once('/').unwrap_or((rest, ""));
    let callback_path = format!("/{path}");
    if callback_path == "/" {
        return Err(LocalOAuthError::new(
            "invalid_redirect_uri",
            "OAuth redirect URI must include a callback path",
        ));
    }
    let (host, port) = host_port.rsplit_once(':').ok_or_else(|| {
        LocalOAuthError::new(
            "invalid_redirect_uri",
            "OAuth redirect URI must include a localhost port",
        )
    })?;
    if host != "127.0.0.1" && host != "localhost" {
        return Err(LocalOAuthError::new(
            "invalid_redirect_uri",
            "OAuth redirect URI must use 127.0.0.1 or localhost",
        ));
    }
    let port = port.parse::<u16>().map_err(|_| {
        LocalOAuthError::new(
            "invalid_redirect_uri",
            "OAuth redirect URI has an invalid port",
        )
    })?;
    Ok(LocalRedirect {
        bind_addr: format!("{host}:{port}"),
        callback_path,
    })
}

fn query_params(query: &str) -> std::collections::BTreeMap<String, String> {
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .filter_map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            Some((url_decode(key).ok()?, url_decode(value).ok()?))
        })
        .collect()
}

pub fn random_state() -> String {
    let mut bytes = [0_u8; 24];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_err()
    {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id() as u128;
        let mixed = nanos ^ (pid << 64);
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = (mixed.rotate_left(index as u32) & 0xff) as u8;
        }
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn open_browser(url: &str) -> io::Result<()> {
    let mut command = browser_command(url).into_command();

    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "browser command exited with {status}"
        )))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BrowserCommandSpec {
    program: &'static str,
    args: Vec<String>,
}

impl BrowserCommandSpec {
    fn into_command(self) -> ProcessCommand {
        let mut command = ProcessCommand::new(self.program);
        command.args(self.args);
        command
    }
}

#[cfg(target_os = "macos")]
fn browser_command(url: &str) -> BrowserCommandSpec {
    BrowserCommandSpec {
        program: "open",
        args: vec![url.to_string()],
    }
}

#[cfg(target_os = "windows")]
fn browser_command(url: &str) -> BrowserCommandSpec {
    BrowserCommandSpec {
        program: "rundll32.exe",
        args: vec!["url.dll,FileProtocolHandler".to_string(), url.to_string()],
    }
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn browser_command(url: &str) -> BrowserCommandSpec {
    BrowserCommandSpec {
        program: "xdg-open",
        args: vec![url.to_string()],
    }
}

fn url_decode(value: &str) -> Result<String, ()> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).map_err(|_| ())?;
                decoded.push(u8::from_str_radix(hex, 16).map_err(|_| ())?);
                index += 3;
            }
            b'%' => return Err(()),
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded).map_err(|_| ())
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::thread;

    #[cfg(target_os = "windows")]
    use super::browser_command;
    use super::{
        LocalOAuthAuthorization, local_redirect, parse_oauth_callback, retryable_callback_error,
        wait_for_oauth_callback,
    };

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_browser_command_does_not_route_oauth_url_through_cmd() {
        let url =
            "https://accounts.google.com/o/oauth2/v2/auth?client_id=client&response_type=code";
        let command = browser_command(url);

        assert_eq!(command.program, "rundll32.exe");
        assert_eq!(
            command.args,
            vec!["url.dll,FileProtocolHandler".to_string(), url.to_string()]
        );
    }

    #[test]
    fn retryable_errors_keep_callback_listener_alive() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let addr = listener.local_addr().expect("listener addr");

        let handle = thread::spawn(move || {
            let _ = send_request(addr, "GET /favicon.ico HTTP/1.1\r\nHost: localhost\r\n\r\n");
            send_request(
                addr,
                "GET /oauth/google-docs/callback?state=expected&code=abc123 HTTP/1.1\r\nHost: localhost\r\n\r\n",
            )
        });

        let authorization = wait_for_oauth_callback(
            "Google Docs",
            &listener,
            "/oauth/google-docs/callback",
            "expected",
        )
        .expect("eventual valid callback");

        assert_eq!(
            authorization,
            LocalOAuthAuthorization {
                code: "abc123".to_string()
            }
        );
        let response = handle.join().expect("callback response");
        assert!(response.contains("Google Docs connected"));
    }

    #[test]
    fn oauth_denial_is_not_retryable() {
        let error = super::LocalOAuthError {
            code: "oauth_denied".to_string(),
            message: "denied".to_string(),
        };

        assert!(!retryable_callback_error(&error));
    }

    #[test]
    fn oauth_callback_error_message_is_provider_neutral() {
        let request = "GET /oauth/google-docs/callback?error=access_denied&state=expected HTTP/1.1\r\nHost: localhost\r\n\r\n";

        let error = parse_oauth_callback(request, "/oauth/google-docs/callback", "expected")
            .expect_err("provider denied callback");

        assert_eq!(error.code, "oauth_denied");
        assert_eq!(
            error.message,
            "OAuth provider returned OAuth error `access_denied`"
        );
    }

    #[test]
    fn local_redirect_errors_are_provider_neutral() {
        let error = local_redirect("https://localhost:8757/oauth/google-docs/callback")
            .expect_err("https callback rejected");

        assert_eq!(error.code, "invalid_redirect_uri");
    }

    fn send_request(addr: SocketAddr, request: &str) -> String {
        let mut stream = TcpStream::connect(addr).expect("connect callback listener");
        stream.write_all(request.as_bytes()).expect("write request");
        let _ = stream.shutdown(std::net::Shutdown::Write);
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
    }
}
