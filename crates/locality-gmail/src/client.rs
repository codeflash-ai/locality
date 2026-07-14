use std::fmt;
use std::sync::OnceLock;
use std::time::Duration;

use locality_core::{LocalityError, LocalityResult};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::dto::{
    GmailDraft, GmailDraftCreateRequest, GmailDraftSendRequest, GmailMessage, GmailMessageList,
};

pub const DEFAULT_GMAIL_API_BASE_URL: &str = "https://gmail.googleapis.com/gmail/v1";
const GMAIL_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

static REQWEST_CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();

pub trait GmailApi: std::fmt::Debug + Send + Sync {
    fn list_messages(
        &self,
        label_id: &str,
        max_results: u32,
        page_token: Option<&str>,
        query: Option<&str>,
    ) -> LocalityResult<GmailMessageList>;
    fn get_message_metadata(&self, message_id: &str) -> LocalityResult<GmailMessage>;
    fn get_message_full(&self, message_id: &str) -> LocalityResult<GmailMessage>;
    fn create_draft(&self, request: GmailDraftCreateRequest) -> LocalityResult<GmailDraft>;
    fn send_draft(&self, request: GmailDraftSendRequest) -> LocalityResult<GmailMessage>;
}

#[derive(Clone)]
pub struct HttpGmailApiClient {
    access_token: String,
    base_url: String,
    client: Client,
}

impl fmt::Debug for HttpGmailApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpGmailApiClient")
            .field("access_token", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("client", &self.client)
            .finish()
    }
}

impl HttpGmailApiClient {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self::with_base_url(access_token, DEFAULT_GMAIL_API_BASE_URL)
    }

    pub fn with_base_url(access_token: impl Into<String>, base_url: impl Into<String>) -> Self {
        ensure_reqwest_crypto_provider();
        let client = Client::builder()
            .timeout(GMAIL_HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            access_token: access_token.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client,
        }
    }

    fn get_json<T>(&self, path: &str, query: Vec<(String, String)>) -> LocalityResult<T>
    where
        T: DeserializeOwned,
    {
        let mut request = self
            .client
            .get(format!("{}{}", self.base_url, path))
            .bearer_auth(&self.access_token);
        for (key, value) in query {
            request = request.query(&[(key.as_str(), value.as_str())]);
        }
        decode_response(request.send(), "gmail api GET")
    }

    fn post_json_with_context<T, B>(&self, path: &str, body: &B, context: &str) -> LocalityResult<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        decode_response(
            self.client
                .post(format!("{}{}", self.base_url, path))
                .bearer_auth(&self.access_token)
                .json(body)
                .send(),
            context,
        )
    }
}

impl GmailApi for HttpGmailApiClient {
    fn list_messages(
        &self,
        label_id: &str,
        max_results: u32,
        page_token: Option<&str>,
        search_query: Option<&str>,
    ) -> LocalityResult<GmailMessageList> {
        let mut params = vec![
            ("labelIds".to_string(), label_id.to_string()),
            ("maxResults".to_string(), max_results.to_string()),
        ];
        if let Some(page_token) = page_token {
            params.push(("pageToken".to_string(), page_token.to_string()));
        }
        if let Some(search_query) = search_query {
            params.push(("q".to_string(), search_query.to_string()));
        }
        self.get_json("/users/me/messages", params)
    }

    fn get_message_metadata(&self, message_id: &str) -> LocalityResult<GmailMessage> {
        let mut query = vec![("format".to_string(), "metadata".to_string())];
        for header in ["From", "To", "Cc", "Bcc", "Subject", "Date", "Message-ID"] {
            query.push(("metadataHeaders".to_string(), header.to_string()));
        }
        self.get_json(&format!("/users/me/messages/{message_id}"), query)
    }

    fn get_message_full(&self, message_id: &str) -> LocalityResult<GmailMessage> {
        self.get_json(
            &format!("/users/me/messages/{message_id}"),
            vec![("format".to_string(), "full".to_string())],
        )
    }

    fn create_draft(&self, request: GmailDraftCreateRequest) -> LocalityResult<GmailDraft> {
        self.post_json_with_context("/users/me/drafts", &request, "gmail draft create")
    }

    fn send_draft(&self, request: GmailDraftSendRequest) -> LocalityResult<GmailMessage> {
        self.post_json_with_context("/users/me/drafts/send", &request, "gmail draft send")
    }
}

fn decode_response<T>(
    response: Result<reqwest::blocking::Response, reqwest::Error>,
    context: &str,
) -> LocalityResult<T>
where
    T: DeserializeOwned,
{
    let response =
        response.map_err(|error| LocalityError::Io(format!("{context} failed: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .unwrap_or_else(|error| format!("<failed to read error body: {error}>"));
        if status == StatusCode::NOT_FOUND {
            return Err(LocalityError::RemoteNotFound(body));
        }
        if status == StatusCode::FORBIDDEN {
            return Err(LocalityError::Guardrail(format!(
                "gmail permission denied: {body}"
            )));
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(LocalityError::Io(format!("gmail rate limited: {body}")));
        }
        return Err(LocalityError::Io(format!(
            "{context} returned HTTP {status}: {body}"
        )));
    }
    response
        .json()
        .map_err(|error| LocalityError::Io(format!("{context} response decode failed: {error}")))
}

fn ensure_reqwest_crypto_provider() {
    REQWEST_CRYPTO_PROVIDER.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;

    use locality_core::LocalityError;

    use super::{GmailApi, HttpGmailApiClient};

    #[test]
    fn debug_redacts_http_client_access_token() {
        let client =
            HttpGmailApiClient::with_base_url("http-client-access-token", "http://127.0.0.1:1");

        let debug = format!("{client:?}");
        assert!(!debug.contains("http-client-access-token"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn get_message_metadata_sends_repeated_metadata_header_query_params() {
        let (base_url, request_rx, server) = spawn_response_server(
            "HTTP/1.1 200 OK",
            r#"{"id":"message-1","threadId":"thread-1"}"#,
        );
        let client = HttpGmailApiClient::with_base_url("access-token", base_url);

        client
            .get_message_metadata("message-1")
            .expect("metadata response");

        let request = request_rx.recv().expect("request line");
        server.join().expect("server exits");
        let query = request
            .split_whitespace()
            .nth(1)
            .and_then(|target| target.split_once('?').map(|(_, query)| query))
            .expect("request query");
        let metadata_headers: Vec<&str> = query
            .split('&')
            .filter(|pair| pair.starts_with("metadataHeaders="))
            .collect();

        assert_eq!(
            metadata_headers,
            vec![
                "metadataHeaders=From",
                "metadataHeaders=To",
                "metadataHeaders=Cc",
                "metadataHeaders=Bcc",
                "metadataHeaders=Subject",
                "metadataHeaders=Date",
                "metadataHeaders=Message-ID",
            ]
        );
        assert!(!query.contains("From%2CTo"));
        assert!(!query.contains("From,To"));
    }

    #[test]
    fn http_errors_map_google_status_semantics() {
        assert!(matches!(
            request_error_for_status("HTTP/1.1 404 Not Found", "missing"),
            LocalityError::RemoteNotFound(body) if body == "missing"
        ));
        assert!(matches!(
            request_error_for_status("HTTP/1.1 403 Forbidden", "forbidden"),
            LocalityError::Guardrail(message)
                if message == "gmail permission denied: forbidden"
        ));
        assert!(matches!(
            request_error_for_status("HTTP/1.1 429 Too Many Requests", "slow down"),
            LocalityError::Io(message) if message == "gmail rate limited: slow down"
        ));
        assert!(matches!(
            request_error_for_status("HTTP/1.1 500 Internal Server Error", "broken"),
            LocalityError::Io(message)
                if message.contains("gmail api GET returned HTTP 500 Internal Server Error: broken")
        ));
    }

    fn request_error_for_status(status_line: &'static str, body: &'static str) -> LocalityError {
        let (base_url, request_rx, server) = spawn_response_server(status_line, body);
        let client = HttpGmailApiClient::with_base_url("access-token", base_url);
        let error = client
            .get_message_full("message-1")
            .expect_err("status should fail");
        request_rx.recv().expect("request line");
        server.join().expect("server exits");
        error
    }

    fn spawn_response_server(
        status_line: &'static str,
        body: &'static str,
    ) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let (request_tx, request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let request = read_http_request(&mut stream);
            let request_line = request.lines().next().unwrap_or_default().to_string();
            request_tx.send(request_line).expect("send request line");
            let response = format!(
                "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        });
        (base_url, request_rx, server)
    }

    fn read_http_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let bytes_read = stream.read(&mut buffer).expect("read request");
            if bytes_read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..bytes_read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8(request).expect("utf8 request")
    }
}
