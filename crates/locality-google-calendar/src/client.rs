use std::fmt;
use std::fmt::Write as _;
use std::sync::OnceLock;
use std::time::Duration;

use locality_core::{LocalityError, LocalityResult};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::dto::{CalendarEvent, CalendarEventCreateRequest, CalendarEventList};

pub const DEFAULT_GOOGLE_CALENDAR_API_BASE_URL: &str = "https://www.googleapis.com";
const GOOGLE_CALENDAR_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

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
            .unwrap_or_else(|_| Client::new());
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
                "google calendar permission denied: {body}"
            )));
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(LocalityError::Io(format!(
                "google calendar rate limited: {body}"
            )));
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
    use super::{calendar_event_url, calendar_events_url, event_insert_query, event_list_query};

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
}
