use std::fmt;
use std::fmt::Write as _;
use std::sync::OnceLock;
use std::time::Duration;

use locality_core::{LocalityError, LocalityResult};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::dto::{CalendarEvent, CalendarEventCreateRequest, CalendarEventList};

pub const DEFAULT_GOOGLE_CALENDAR_API_BASE_URL: &str = "https://www.googleapis.com";
const GOOGLE_CALENDAR_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const GOOGLE_CALENDAR_ERROR_SUMMARY_LIMIT: usize = 1000;

static REQWEST_CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();

pub trait GoogleCalendarApi: std::fmt::Debug + Send + Sync {
    fn list_events(
        &self,
        calendar_id: &str,
        time_min: &str,
        time_max: &str,
        page_token: Option<&str>,
    ) -> LocalityResult<CalendarEventList>;

    fn get_event(&self, calendar_id: &str, event_id: &str) -> LocalityResult<CalendarEvent>;

    fn insert_event(
        &self,
        calendar_id: &str,
        request: CalendarEventCreateRequest,
        create_conference: bool,
    ) -> LocalityResult<CalendarEvent>;
}

#[derive(Clone)]
pub struct HttpGoogleCalendarApiClient {
    access_token: String,
    base_url: String,
    client: Client,
}

impl fmt::Debug for HttpGoogleCalendarApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpGoogleCalendarApiClient")
            .field("access_token", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("client", &self.client)
            .finish()
    }
}

impl HttpGoogleCalendarApiClient {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self::with_base_url(access_token, DEFAULT_GOOGLE_CALENDAR_API_BASE_URL)
    }

    pub fn with_base_url(access_token: impl Into<String>, base_url: impl Into<String>) -> Self {
        ensure_reqwest_crypto_provider();
        let client = Client::builder()
            .timeout(GOOGLE_CALENDAR_HTTP_TIMEOUT)
            .build()
            .expect("build google calendar api http client");
        Self {
            access_token: access_token.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client,
        }
    }

    fn get_json<T>(&self, url: String, query: Vec<(String, String)>) -> LocalityResult<T>
    where
        T: DeserializeOwned,
    {
        let mut request = self.client.get(url).bearer_auth(&self.access_token);
        for (key, value) in query {
            request = request.query(&[(key.as_str(), value.as_str())]);
        }
        decode_response(request.send(), "google calendar api GET")
    }

    fn post_json<T, B>(
        &self,
        url: String,
        body: &B,
        query: Vec<(String, String)>,
    ) -> LocalityResult<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let mut request = self
            .client
            .post(url)
            .bearer_auth(&self.access_token)
            .json(body);
        for (key, value) in query {
            request = request.query(&[(key.as_str(), value.as_str())]);
        }
        decode_response(request.send(), "google calendar api POST")
    }
}

impl GoogleCalendarApi for HttpGoogleCalendarApiClient {
    fn list_events(
        &self,
        calendar_id: &str,
        time_min: &str,
        time_max: &str,
        page_token: Option<&str>,
    ) -> LocalityResult<CalendarEventList> {
        self.get_json(
            calendar_events_url(&self.base_url, calendar_id),
            event_list_query(time_min, time_max, page_token),
        )
    }

    fn get_event(&self, calendar_id: &str, event_id: &str) -> LocalityResult<CalendarEvent> {
        self.get_json(
            calendar_event_url(&self.base_url, calendar_id, event_id),
            Vec::new(),
        )
    }

    fn insert_event(
        &self,
        calendar_id: &str,
        request: CalendarEventCreateRequest,
        create_conference: bool,
    ) -> LocalityResult<CalendarEvent> {
        self.post_json(
            calendar_events_url(&self.base_url, calendar_id),
            &request,
            event_insert_query(create_conference),
        )
    }
}

pub fn calendar_events_url(base_url: &str, calendar_id: &str) -> String {
    format!(
        "{}/calendar/v3/calendars/{}/events",
        base_url.trim_end_matches('/'),
        percent_encode_path_segment(calendar_id)
    )
}

pub fn calendar_event_url(base_url: &str, calendar_id: &str, event_id: &str) -> String {
    format!(
        "{}/{}",
        calendar_events_url(base_url, calendar_id),
        percent_encode_path_segment(event_id)
    )
}

pub fn event_list_query(
    time_min: &str,
    time_max: &str,
    page_token: Option<&str>,
) -> Vec<(String, String)> {
    let mut query = vec![
        ("timeMin".to_string(), time_min.to_string()),
        ("timeMax".to_string(), time_max.to_string()),
        ("singleEvents".to_string(), "true".to_string()),
        ("orderBy".to_string(), "startTime".to_string()),
        ("maxResults".to_string(), "250".to_string()),
    ];
    if let Some(page_token) = page_token {
        query.push(("pageToken".to_string(), page_token.to_string()));
    }
    query
}

pub fn event_insert_query(create_conference: bool) -> Vec<(String, String)> {
    let mut query = vec![("sendUpdates".to_string(), "all".to_string())];
    if create_conference {
        query.push(("conferenceDataVersion".to_string(), "1".to_string()));
    }
    query
}

#[derive(Debug, Deserialize)]
struct GoogleApiErrorEnvelope {
    error: Option<GoogleApiErrorBody>,
}

#[derive(Debug, Deserialize)]
struct GoogleApiErrorBody {
    code: Option<i64>,
    message: Option<String>,
    status: Option<String>,
    #[serde(default)]
    errors: Vec<GoogleApiErrorDetail>,
}

#[derive(Debug, Deserialize)]
struct GoogleApiErrorDetail {
    domain: Option<String>,
    reason: Option<String>,
    message: Option<String>,
}

#[derive(Debug)]
struct GoogleApiErrorSummary {
    text: String,
    status: Option<String>,
    reasons: Vec<String>,
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
        let summary = google_api_error_summary(&body);
        if status == StatusCode::UNAUTHORIZED {
            return Err(LocalityError::Guardrail(format!(
                "google calendar authentication failed; reconnect Google Calendar: {}",
                summary.text
            )));
        }
        if status == StatusCode::NOT_FOUND {
            return Err(LocalityError::RemoteNotFound(summary.text));
        }
        if status == StatusCode::FORBIDDEN {
            if is_google_rate_limit_error(&summary) {
                return Err(LocalityError::RateLimited {
                    provider: "google calendar".to_string(),
                    retry_after: Duration::from_secs(0),
                    message: summary.text,
                });
            }
            return Err(LocalityError::Guardrail(format!(
                "google calendar permission denied: {}",
                summary.text
            )));
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(LocalityError::RateLimited {
                provider: "google calendar".to_string(),
                retry_after: Duration::from_secs(0),
                message: summary.text,
            });
        }
        return Err(LocalityError::Io(format!(
            "{context} returned HTTP {status}: {}",
            summary.text
        )));
    }
    response
        .json()
        .map_err(|error| LocalityError::Io(format!("{context} response decode failed: {error}")))
}

fn google_api_error_summary(body: &str) -> GoogleApiErrorSummary {
    let parsed = serde_json::from_str::<GoogleApiErrorEnvelope>(body)
        .ok()
        .and_then(|envelope| envelope.error);
    let Some(error) = parsed else {
        return GoogleApiErrorSummary {
            text: capped_error_text(body),
            status: None,
            reasons: Vec::new(),
        };
    };

    let status = cleaned_error_field(error.status.as_deref());
    let message = cleaned_error_field(error.message.as_deref());
    let mut text = match (status.as_deref(), error.code) {
        (Some(status), Some(code)) => format!("{status} ({code})"),
        (Some(status), None) => status.to_string(),
        (None, Some(code)) => format!("code {code}"),
        (None, None) => String::new(),
    };
    if let Some(message) = message.as_deref() {
        if text.is_empty() {
            text.push_str(message);
        } else {
            text.push_str(": ");
            text.push_str(message);
        }
    }

    let mut reasons = Vec::new();
    let details: Vec<String> = error
        .errors
        .iter()
        .filter_map(|detail| {
            let domain = cleaned_error_field(detail.domain.as_deref());
            let reason = cleaned_error_field(detail.reason.as_deref());
            let detail_message = cleaned_error_field(detail.message.as_deref());
            if let Some(reason) = reason.as_deref() {
                reasons.push(reason.to_string());
            }

            let label = match (domain.as_deref(), reason.as_deref()) {
                (Some(domain), Some(reason)) => Some(format!("{domain}/{reason}")),
                (Some(domain), None) => Some(domain.to_string()),
                (None, Some(reason)) => Some(reason.to_string()),
                (None, None) => None,
            };
            let detail_message = detail_message
                .as_deref()
                .filter(|detail_message| Some(*detail_message) != message.as_deref());
            match (label, detail_message) {
                (Some(label), Some(detail_message)) => Some(format!("{label}: {detail_message}")),
                (Some(label), None) => Some(label),
                (None, Some(detail_message)) => Some(detail_message.to_string()),
                (None, None) => None,
            }
        })
        .collect();
    if !details.is_empty() {
        if text.is_empty() {
            text.push_str(&details.join("; "));
        } else {
            text.push_str(" [");
            text.push_str(&details.join("; "));
            text.push(']');
        }
    }
    if text.is_empty() {
        text.push_str("google api error");
    }

    GoogleApiErrorSummary {
        text: cap_error_text(&text, GOOGLE_CALENDAR_ERROR_SUMMARY_LIMIT),
        status,
        reasons,
    }
}

fn is_google_rate_limit_error(summary: &GoogleApiErrorSummary) -> bool {
    summary
        .status
        .as_deref()
        .is_some_and(|status| status.eq_ignore_ascii_case("RESOURCE_EXHAUSTED"))
        || summary
            .reasons
            .iter()
            .any(|reason| is_google_rate_limit_reason(reason))
}

fn is_google_rate_limit_reason(reason: &str) -> bool {
    let normalized = reason
        .chars()
        .filter(|character| !matches!(character, '_' | '-' | ' '))
        .collect::<String>()
        .to_ascii_lowercase();
    normalized.contains("ratelimit")
        || normalized.contains("quota")
        || normalized.contains("resourceexhausted")
        || normalized == "toomanyrequests"
}

fn cleaned_error_field(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn capped_error_text(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return "<empty error body>".to_string();
    }
    cap_error_text(value, GOOGLE_CALENDAR_ERROR_SUMMARY_LIMIT)
}

fn cap_error_text(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    const SUFFIX: &str = "... [truncated]";
    let take = limit.saturating_sub(SUFFIX.chars().count());
    let mut capped = value.chars().take(take).collect::<String>();
    capped.push_str(SUFFIX);
    capped
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
    use std::time::Duration;

    use locality_core::LocalityError;
    use serde_json::{Value, json};

    use crate::dto::{CalendarEventCreateRequest, EventDateTime};

    use super::{
        GoogleCalendarApi, HttpGoogleCalendarApiClient, calendar_event_url, calendar_events_url,
        event_insert_query, event_list_query,
    };

    #[test]
    fn event_list_query_uses_primary_calendar_window_and_expands_instances() {
        let query = event_list_query(
            "2026-06-16T00:00:00Z",
            "2027-01-12T00:00:00Z",
            Some("page-2"),
        );

        assert_eq!(
            query,
            vec![
                ("timeMin".to_string(), "2026-06-16T00:00:00Z".to_string()),
                ("timeMax".to_string(), "2027-01-12T00:00:00Z".to_string()),
                ("singleEvents".to_string(), "true".to_string()),
                ("orderBy".to_string(), "startTime".to_string()),
                ("maxResults".to_string(), "250".to_string()),
                ("pageToken".to_string(), "page-2".to_string()),
            ]
        );
    }

    #[test]
    fn insert_query_sends_attendee_updates_and_enables_conference_data_when_needed() {
        assert_eq!(
            event_insert_query(true),
            vec![
                ("sendUpdates".to_string(), "all".to_string()),
                ("conferenceDataVersion".to_string(), "1".to_string()),
            ]
        );
        assert_eq!(
            event_insert_query(false),
            vec![("sendUpdates".to_string(), "all".to_string())]
        );
    }

    #[test]
    fn event_urls_percent_encode_calendar_and_event_ids() {
        assert_eq!(
            calendar_events_url("https://calendar.example.test", "primary"),
            "https://calendar.example.test/calendar/v3/calendars/primary/events"
        );
        assert_eq!(
            calendar_event_url(
                "https://calendar.example.test",
                "team@example.com",
                "event:1"
            ),
            "https://calendar.example.test/calendar/v3/calendars/team%40example.com/events/event%3A1"
        );
    }

    #[test]
    fn list_events_sends_bearer_auth_and_calendar_window_query() {
        let (base_url, request_rx, server) = spawn_response_server(
            "HTTP/1.1 200 OK",
            r#"{"items":[{"id":"event-1","summary":"Team sync"}],"nextPageToken":"next"}"#,
        );
        let client = HttpGoogleCalendarApiClient::with_base_url("access-token", base_url);

        let events = client
            .list_events(
                "team@example.com",
                "2026-06-16T00:00:00Z",
                "2027-01-12T00:00:00Z",
                Some("page-2"),
            )
            .expect("events");

        assert_eq!(events.items[0].id.as_deref(), Some("event-1"));
        assert_eq!(events.next_page_token.as_deref(), Some("next"));
        let request = request_rx.recv().expect("request");
        server.join().expect("server exits");
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.target,
            "/calendar/v3/calendars/team%40example.com/events?timeMin=2026-06-16T00%3A00%3A00Z&timeMax=2027-01-12T00%3A00%3A00Z&singleEvents=true&orderBy=startTime&maxResults=250&pageToken=page-2"
        );
        assert_eq!(request.header("authorization"), Some("Bearer access-token"));
    }

    #[test]
    fn insert_event_sends_json_body_and_conference_query() {
        let (base_url, request_rx, server) = spawn_response_server(
            "HTTP/1.1 200 OK",
            r#"{"id":"created-1","summary":"Planning"}"#,
        );
        let client = HttpGoogleCalendarApiClient::with_base_url("access-token", base_url);
        let request_body = CalendarEventCreateRequest {
            summary: "Planning".to_string(),
            start: EventDateTime {
                date_time: Some("2026-07-20T09:00:00Z".to_string()),
                ..EventDateTime::default()
            },
            end: EventDateTime {
                date_time: Some("2026-07-20T09:30:00Z".to_string()),
                ..EventDateTime::default()
            },
            ..CalendarEventCreateRequest::default()
        };

        let event = client
            .insert_event("team@example.com", request_body, true)
            .expect("created event");

        assert_eq!(event.id.as_deref(), Some("created-1"));
        let request = request_rx.recv().expect("request");
        server.join().expect("server exits");
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.target,
            "/calendar/v3/calendars/team%40example.com/events?sendUpdates=all&conferenceDataVersion=1"
        );
        assert_eq!(request.header("authorization"), Some("Bearer access-token"));
        assert_eq!(request.header("content-type"), Some("application/json"));
        let body: Value = serde_json::from_str(&request.body).expect("json body");
        assert_eq!(body["summary"], json!("Planning"));
        assert_eq!(body["start"]["dateTime"], json!("2026-07-20T09:00:00Z"));
        assert_eq!(body["end"]["dateTime"], json!("2026-07-20T09:30:00Z"));
    }

    #[test]
    fn http_errors_map_google_calendar_status_semantics() {
        assert_eq!(
            request_error_for_status(
                "HTTP/1.1 401 Unauthorized",
                google_error_body(401, "UNAUTHENTICATED", "Invalid Credentials", "authError"),
            ),
            LocalityError::Guardrail(
                "google calendar authentication failed; reconnect Google Calendar: UNAUTHENTICATED (401): Invalid Credentials [global/authError]"
                    .to_string()
            )
        );
        assert_eq!(
            request_error_for_status(
                "HTTP/1.1 403 Forbidden",
                google_error_body(
                    403,
                    "PERMISSION_DENIED",
                    "Calendar access denied",
                    "forbidden"
                ),
            ),
            LocalityError::Guardrail(
                "google calendar permission denied: PERMISSION_DENIED (403): Calendar access denied [global/forbidden]"
                    .to_string()
            )
        );
        assert_eq!(
            request_error_for_status(
                "HTTP/1.1 403 Forbidden",
                google_error_body(
                    403,
                    "RESOURCE_EXHAUSTED",
                    "Calendar quota exceeded",
                    "userRateLimitExceeded"
                ),
            ),
            LocalityError::RateLimited {
                provider: "google calendar".to_string(),
                retry_after: Duration::from_secs(0),
                message:
                    "RESOURCE_EXHAUSTED (403): Calendar quota exceeded [global/userRateLimitExceeded]"
                        .to_string(),
            }
        );
        assert_eq!(
            request_error_for_status(
                "HTTP/1.1 404 Not Found",
                google_error_body(404, "NOT_FOUND", "Calendar not found", "notFound"),
            ),
            LocalityError::RemoteNotFound(
                "NOT_FOUND (404): Calendar not found [global/notFound]".to_string()
            )
        );
        assert_eq!(
            request_error_for_status(
                "HTTP/1.1 429 Too Many Requests",
                google_error_body(429, "RESOURCE_EXHAUSTED", "Slow down", "rateLimitExceeded"),
            ),
            LocalityError::RateLimited {
                provider: "google calendar".to_string(),
                retry_after: Duration::from_secs(0),
                message: "RESOURCE_EXHAUSTED (429): Slow down [global/rateLimitExceeded]"
                    .to_string(),
            }
        );
        assert_eq!(
            request_error_for_status(
                "HTTP/1.1 500 Internal Server Error",
                google_error_body(500, "INTERNAL", "Backend error", "backendError"),
            ),
            LocalityError::Io(
                "google calendar api GET returned HTTP 500 Internal Server Error: INTERNAL (500): Backend error [global/backendError]"
                    .to_string()
            )
        );
    }

    #[test]
    fn server_error_with_large_non_json_body_is_capped() {
        let large_body = format!("{}SENSITIVE_TAIL_DO_NOT_COPY", "x".repeat(5000));

        let error = request_error_for_status("HTTP/1.1 500 Internal Server Error", large_body);

        let LocalityError::Io(message) = error else {
            panic!("expected io error");
        };
        assert!(
            message
                .starts_with("google calendar api GET returned HTTP 500 Internal Server Error: ")
        );
        assert!(message.contains("[truncated]"));
        assert!(!message.contains("SENSITIVE_TAIL_DO_NOT_COPY"));
        assert!(message.len() < 1150, "{message}");
    }

    fn request_error_for_status(status_line: &'static str, body: String) -> LocalityError {
        let (base_url, request_rx, server) = spawn_response_server(status_line, body);
        let client = HttpGoogleCalendarApiClient::with_base_url("access-token", base_url);
        let error = client
            .get_event("primary", "event-1")
            .expect_err("status should fail");
        request_rx.recv().expect("request");
        server.join().expect("server exits");
        error
    }

    fn google_error_body(code: i64, status: &str, message: &str, reason: &str) -> String {
        json!({
            "error": {
                "code": code,
                "message": message,
                "status": status,
                "errors": [
                    {
                        "domain": "global",
                        "reason": reason,
                        "message": message,
                    }
                ],
            }
        })
        .to_string()
    }

    #[derive(Debug)]
    struct CapturedRequest {
        method: String,
        target: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl CapturedRequest {
        fn parse(raw: String) -> Self {
            let (header_block, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
            let mut lines = header_block.lines();
            let request_line = lines.next().unwrap_or_default();
            let mut request_parts = request_line.split_whitespace();
            let method = request_parts.next().unwrap_or_default().to_string();
            let target = request_parts.next().unwrap_or_default().to_string();
            let headers = lines
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
                .collect();

            Self {
                method,
                target,
                headers,
                body: body.to_string(),
            }
        }

        fn header(&self, name: &str) -> Option<&str> {
            let name = name.to_ascii_lowercase();
            self.headers
                .iter()
                .find(|(header_name, _)| header_name == &name)
                .map(|(_, value)| value.as_str())
        }
    }

    fn spawn_response_server(
        status_line: &'static str,
        body: impl Into<String>,
    ) -> (
        String,
        mpsc::Receiver<CapturedRequest>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let body = body.into();
        let (request_tx, request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let request = CapturedRequest::parse(read_http_request(&mut stream));
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
        let mut buffer = [0_u8; 4096];
        let headers_end = loop {
            let bytes_read = stream.read(&mut buffer).expect("read request");
            if bytes_read == 0 {
                break request.len();
            }
            request.extend_from_slice(&buffer[..bytes_read]);
            if let Some(headers_end) = find_headers_end(&request) {
                break headers_end;
            }
        };
        let content_length = content_length(&request[..headers_end]);
        while request.len() < headers_end + content_length {
            let bytes_read = stream.read(&mut buffer).expect("read request body");
            if bytes_read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..bytes_read]);
        }
        String::from_utf8(request).expect("utf8 request")
    }

    fn find_headers_end(request: &[u8]) -> Option<usize> {
        request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
    }

    fn content_length(headers: &[u8]) -> usize {
        String::from_utf8_lossy(headers)
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().ok())
                    .flatten()
            })
            .unwrap_or(0)
    }
}
