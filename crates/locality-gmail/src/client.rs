use std::fmt;
use std::fmt::Write as _;
use std::sync::OnceLock;
use std::time::Duration;

use locality_core::{LocalityError, LocalityResult};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::dto::{
    GmailDraft, GmailDraftCreateRequest, GmailDraftList, GmailMessage, GmailMessageList,
    GmailMessagePartBody, GmailThread, GmailThreadList,
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
    fn list_threads(
        &self,
        label_id: &str,
        max_results: u32,
        page_token: Option<&str>,
        query: Option<&str>,
    ) -> LocalityResult<GmailThreadList>;
    fn get_message_metadata(&self, message_id: &str) -> LocalityResult<GmailMessage>;
    fn get_message_full(&self, message_id: &str) -> LocalityResult<GmailMessage>;
    fn get_thread_metadata(&self, thread_id: &str) -> LocalityResult<GmailThread>;
    fn get_thread_full(&self, thread_id: &str) -> LocalityResult<GmailThread>;
    fn list_drafts(
        &self,
        max_results: u32,
        page_token: Option<&str>,
    ) -> LocalityResult<GmailDraftList>;
    fn get_draft_metadata(&self, draft_id: &str) -> LocalityResult<GmailDraft>;
    fn get_draft_full(&self, draft_id: &str) -> LocalityResult<GmailDraft>;
    fn get_attachment(
        &self,
        message_id: &str,
        attachment_id: &str,
    ) -> LocalityResult<GmailMessagePartBody>;
    fn create_draft(&self, request: GmailDraftCreateRequest) -> LocalityResult<GmailDraft>;
    fn update_draft(
        &self,
        draft_id: &str,
        request: GmailDraftCreateRequest,
    ) -> LocalityResult<GmailDraft>;
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

    fn put_json_with_context<T, B>(&self, path: &str, body: &B, context: &str) -> LocalityResult<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        decode_response(
            self.client
                .put(format!("{}{}", self.base_url, path))
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

    fn list_threads(
        &self,
        label_id: &str,
        max_results: u32,
        page_token: Option<&str>,
        search_query: Option<&str>,
    ) -> LocalityResult<GmailThreadList> {
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
        self.get_json("/users/me/threads", params)
    }

    fn get_message_metadata(&self, message_id: &str) -> LocalityResult<GmailMessage> {
        let mut query = vec![("format".to_string(), "metadata".to_string())];
        for header in [
            "From",
            "Reply-To",
            "To",
            "Cc",
            "Bcc",
            "Subject",
            "Date",
            "Message-ID",
            "References",
            "In-Reply-To",
        ] {
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

    fn get_thread_metadata(&self, thread_id: &str) -> LocalityResult<GmailThread> {
        let thread_id = percent_encode_path_segment(thread_id);
        let mut query = vec![("format".to_string(), "metadata".to_string())];
        for header in [
            "From",
            "Reply-To",
            "To",
            "Cc",
            "Bcc",
            "Subject",
            "Date",
            "Message-ID",
            "References",
            "In-Reply-To",
        ] {
            query.push(("metadataHeaders".to_string(), header.to_string()));
        }
        self.get_json(&format!("/users/me/threads/{thread_id}"), query)
    }

    fn get_thread_full(&self, thread_id: &str) -> LocalityResult<GmailThread> {
        let thread_id = percent_encode_path_segment(thread_id);
        self.get_json(
            &format!("/users/me/threads/{thread_id}"),
            vec![("format".to_string(), "full".to_string())],
        )
    }

    fn list_drafts(
        &self,
        max_results: u32,
        page_token: Option<&str>,
    ) -> LocalityResult<GmailDraftList> {
        let mut params = vec![("maxResults".to_string(), max_results.to_string())];
        if let Some(page_token) = page_token {
            params.push(("pageToken".to_string(), page_token.to_string()));
        }
        self.get_json("/users/me/drafts", params)
    }

    fn get_draft_metadata(&self, draft_id: &str) -> LocalityResult<GmailDraft> {
        let draft_id = percent_encode_path_segment(draft_id);
        let mut query = vec![("format".to_string(), "metadata".to_string())];
        for header in [
            "From",
            "Reply-To",
            "To",
            "Cc",
            "Bcc",
            "Subject",
            "Date",
            "Message-ID",
            "References",
            "In-Reply-To",
        ] {
            query.push(("metadataHeaders".to_string(), header.to_string()));
        }
        self.get_json(&format!("/users/me/drafts/{draft_id}"), query)
    }

    fn get_draft_full(&self, draft_id: &str) -> LocalityResult<GmailDraft> {
        let draft_id = percent_encode_path_segment(draft_id);
        self.get_json(
            &format!("/users/me/drafts/{draft_id}"),
            vec![("format".to_string(), "full".to_string())],
        )
    }

    fn get_attachment(
        &self,
        message_id: &str,
        attachment_id: &str,
    ) -> LocalityResult<GmailMessagePartBody> {
        let message_id = percent_encode_path_segment(message_id);
        let attachment_id = percent_encode_path_segment(attachment_id);
        self.get_json(
            &format!("/users/me/messages/{message_id}/attachments/{attachment_id}"),
            Vec::new(),
        )
    }

    fn create_draft(&self, request: GmailDraftCreateRequest) -> LocalityResult<GmailDraft> {
        self.post_json_with_context("/users/me/drafts", &request, "gmail draft create")
    }

    fn update_draft(
        &self,
        draft_id: &str,
        request: GmailDraftCreateRequest,
    ) -> LocalityResult<GmailDraft> {
        let draft_id = percent_encode_path_segment(draft_id);
        self.put_json_with_context(
            &format!("/users/me/drafts/{draft_id}"),
            &request,
            "gmail draft update",
        )
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

fn percent_encode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            let _ = write!(&mut encoded, "%{byte:02X}");
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;

    use locality_core::LocalityError;

    use crate::dto::{GmailDraftCreateRequest, GmailRawMessage};

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
                "metadataHeaders=Reply-To",
                "metadataHeaders=To",
                "metadataHeaders=Cc",
                "metadataHeaders=Bcc",
                "metadataHeaders=Subject",
                "metadataHeaders=Date",
                "metadataHeaders=Message-ID",
                "metadataHeaders=References",
                "metadataHeaders=In-Reply-To",
            ]
        );
        assert!(!query.contains("From%2CTo"));
        assert!(!query.contains("From,To"));
    }

    #[test]
    fn list_threads_calls_gmail_threads_endpoint_with_query() {
        let (base_url, request_rx, server) = spawn_response_server(
            "HTTP/1.1 200 OK",
            r#"{"threads":[{"id":"thread-1","snippet":"hello"}],"nextPageToken":"next"}"#,
        );
        let client = HttpGmailApiClient::with_base_url("access-token", base_url);

        let threads = client
            .list_threads(
                "INBOX",
                100,
                Some("page-2"),
                Some("after:2026/07/01 before:2026/07/15"),
            )
            .expect("threads");

        assert_eq!(threads.threads[0].id, "thread-1");
        assert_eq!(threads.next_page_token.as_deref(), Some("next"));
        let request = request_rx.recv().expect("request line");
        server.join().expect("server exits");
        assert!(request.starts_with("GET /users/me/threads?"), "{request}");
        assert!(request.contains("labelIds=INBOX"), "{request}");
        assert!(request.contains("maxResults=100"), "{request}");
        assert!(request.contains("pageToken=page-2"), "{request}");
        assert!(
            request.contains("q=after%3A2026%2F07%2F01+before%3A2026%2F07%2F15"),
            "{request}"
        );
    }

    #[test]
    fn get_thread_metadata_requests_metadata_format_headers() {
        let (base_url, request_rx, server) = spawn_response_server(
            "HTTP/1.1 200 OK",
            r#"{"id":"thread-1","messages":[{"id":"msg-1","threadId":"thread-1"}]}"#,
        );
        let client = HttpGmailApiClient::with_base_url("access-token", base_url);

        let thread = client.get_thread_metadata("thread-1").expect("thread");

        assert_eq!(thread.id, "thread-1");
        let request = request_rx.recv().expect("request line");
        server.join().expect("server exits");
        assert!(
            request.starts_with("GET /users/me/threads/thread-1?"),
            "{request}"
        );
        assert!(request.contains("format=metadata"), "{request}");
        assert!(request.contains("metadataHeaders=Subject"), "{request}");
    }

    #[test]
    fn get_thread_full_percent_encodes_thread_path_segment() {
        let (base_url, request_rx, server) = spawn_response_server(
            "HTTP/1.1 200 OK",
            r#"{"id":"thread/1 space","messages":[]}"#,
        );
        let client = HttpGmailApiClient::with_base_url("access-token", base_url);

        client
            .get_thread_full("thread/1 space")
            .expect("thread response");

        let request = request_rx.recv().expect("request line");
        server.join().expect("server exits");
        let target = request.split_whitespace().nth(1).expect("request target");
        assert_eq!(target, "/users/me/threads/thread%2F1%20space?format=full");
        assert!(!target.contains("thread/1"), "{target}");
        assert!(!target.contains(' '), "{target}");
    }

    #[test]
    fn list_drafts_uses_gmail_drafts_endpoint_with_pagination() {
        let (base_url, request_rx, server) = spawn_response_server(
            "HTTP/1.1 200 OK",
            r#"{"drafts":[{"id":"draft-1","message":{"id":"message-1"}}],"nextPageToken":"next"}"#,
        );
        let client = HttpGmailApiClient::with_base_url("access-token", base_url);

        let drafts = client.list_drafts(100, Some("page-2")).expect("draft list");

        assert_eq!(drafts.drafts[0].id, "draft-1");
        assert_eq!(drafts.next_page_token.as_deref(), Some("next"));
        let request = request_rx.recv().expect("request");
        server.join().expect("server exits");
        assert!(
            request.starts_with("GET /users/me/drafts?maxResults=100&pageToken=page-2 "),
            "{request}"
        );
    }

    #[test]
    fn get_draft_metadata_percent_encodes_id_and_requests_reply_headers() {
        let (base_url, request_rx, server) = spawn_response_server(
            "HTTP/1.1 200 OK",
            r#"{"id":"draft/1","message":{"id":"message-1","threadId":"thread-1"}}"#,
        );
        let client = HttpGmailApiClient::with_base_url("access-token", base_url);

        let draft = client
            .get_draft_metadata("draft/1 space")
            .expect("draft metadata");

        assert_eq!(draft.id, "draft/1");
        let request = request_rx.recv().expect("request");
        server.join().expect("server exits");
        let request_line = request.lines().next().expect("request line");
        assert!(
            request_line.starts_with("GET /users/me/drafts/draft%2F1%20space?format=metadata&"),
            "{request_line}"
        );
        assert!(
            request_line.contains("metadataHeaders=Reply-To"),
            "{request_line}"
        );
        assert!(
            request_line.contains("metadataHeaders=References"),
            "{request_line}"
        );
        assert!(
            request_line.contains("metadataHeaders=In-Reply-To"),
            "{request_line}"
        );
    }

    #[test]
    fn get_draft_full_requests_full_format() {
        let (base_url, request_rx, server) = spawn_response_server(
            "HTTP/1.1 200 OK",
            r#"{"id":"draft-1","message":{"id":"message-1"}}"#,
        );
        let client = HttpGmailApiClient::with_base_url("access-token", base_url);

        client.get_draft_full("draft-1").expect("draft full");

        let request = request_rx.recv().expect("request");
        server.join().expect("server exits");
        assert!(
            request.starts_with("GET /users/me/drafts/draft-1?format=full "),
            "{request}"
        );
    }

    #[test]
    fn update_draft_puts_threaded_raw_message_to_percent_encoded_draft_path() {
        let (base_url, request_rx, server) = spawn_response_server(
            "HTTP/1.1 200 OK",
            r#"{"id":"draft/1","message":{"id":"message-2","threadId":"thread-1"}}"#,
        );
        let client = HttpGmailApiClient::with_base_url("access-token", base_url);

        let draft = client
            .update_draft(
                "draft/1",
                GmailDraftCreateRequest {
                    message: GmailRawMessage {
                        raw: "base64url-mime".to_string(),
                        thread_id: Some("thread-1".to_string()),
                    },
                },
            )
            .expect("draft update");

        assert_eq!(draft.message.id, "message-2");
        let request = request_rx.recv().expect("request");
        server.join().expect("server exits");
        let (headers, body) = request.split_once("\r\n\r\n").expect("request body");
        assert!(
            headers.starts_with("PUT /users/me/drafts/draft%2F1 HTTP/1.1\r\n"),
            "{headers}"
        );
        let body: serde_json::Value = serde_json::from_str(body).expect("JSON request body");
        assert_eq!(body["message"]["raw"], "base64url-mime");
        assert_eq!(body["message"]["threadId"], "thread-1");
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

    #[test]
    fn get_attachment_calls_gmail_attachment_endpoint() {
        let (base_url, request_rx, server) = spawn_response_server(
            "HTTP/1.1 200 OK",
            r#"{"attachmentId":"attach-1","size":5,"data":"SGVsbG8"}"#,
        );
        let client = HttpGmailApiClient::with_base_url("access-token", base_url);

        let attachment = client
            .get_attachment("msg-1", "attach-1")
            .expect("attachment response");

        assert_eq!(attachment.attachment_id.as_deref(), Some("attach-1"));
        assert_eq!(attachment.size, Some(5));
        assert_eq!(attachment.data.as_deref(), Some("SGVsbG8"));
        let request = request_rx.recv().expect("request line");
        server.join().expect("server exits");
        assert!(
            request.starts_with("GET /users/me/messages/msg-1/attachments/attach-1 "),
            "{request}"
        );
    }

    #[test]
    fn get_attachment_percent_encodes_message_and_attachment_path_segments() {
        let (base_url, request_rx, server) = spawn_response_server(
            "HTTP/1.1 200 OK",
            r#"{"attachmentId":"attach/1?x","size":5,"data":"SGVsbG8"}"#,
        );
        let client = HttpGmailApiClient::with_base_url("access-token", base_url);

        client
            .get_attachment("msg/1", "attach/1?x")
            .expect("attachment response");

        let request = request_rx.recv().expect("request line");
        server.join().expect("server exits");
        let target = request.split_whitespace().nth(1).expect("request target");
        assert_eq!(
            target,
            "/users/me/messages/msg%2F1/attachments/attach%2F1%3Fx"
        );
        assert!(!target.contains("msg/1"), "{target}");
        assert!(!target.contains("attach/1"), "{target}");
        assert!(!target.contains("?x"), "{target}");
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
            request_tx.send(request).expect("send request");
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
            if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
        }
        String::from_utf8(request).expect("utf8 request")
    }
}
