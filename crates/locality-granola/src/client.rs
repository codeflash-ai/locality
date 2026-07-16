use std::fmt;
use std::sync::OnceLock;
use std::time::Duration;

use locality_connector::ConnectorExecutionPolicy;
use locality_connector::network::{ConnectorNetworkConfig, ConnectorNetworkGate, RetryConfig};
use locality_core::{LocalityError, LocalityResult};
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use serde::de::DeserializeOwned;

use crate::dto::{GranolaNote, GranolaNoteList};

pub const DEFAULT_GRANOLA_API_BASE_URL: &str = "https://public-api.granola.ai";
const GRANOLA_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_GRANOLA_REQUESTS_PER_SECOND: f64 = 5.0;
const DEFAULT_GRANOLA_REQUEST_BURST: f64 = 3.0;
const DEFAULT_GRANOLA_MAX_IN_FLIGHT: usize = 8;
const DEFAULT_GRANOLA_RATE_LIMIT_RETRIES: usize = 4;

static REQWEST_CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();
static GRANOLA_NETWORK_GATE: OnceLock<ConnectorNetworkGate> = OnceLock::new();

pub trait GranolaApi: fmt::Debug + Send + Sync {
    fn list_notes(
        &self,
        cursor: Option<&str>,
        page_size: u32,
        created_after: Option<&str>,
        created_before: Option<&str>,
        updated_after: Option<&str>,
    ) -> LocalityResult<GranolaNoteList>;
    fn get_note(&self, note_id: &str, include_transcript: bool) -> LocalityResult<GranolaNote>;
}

#[derive(Clone)]
pub struct HttpGranolaApiClient {
    api_key: String,
    base_url: String,
    client: Client,
    execution_policy: ConnectorExecutionPolicy,
}

impl fmt::Debug for HttpGranolaApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpGranolaApiClient")
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl HttpGranolaApiClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_execution_policy(api_key, ConnectorExecutionPolicy::Inline)
    }

    pub fn with_execution_policy(
        api_key: impl Into<String>,
        execution_policy: ConnectorExecutionPolicy,
    ) -> Self {
        Self::with_base_url_and_execution_policy(
            api_key,
            DEFAULT_GRANOLA_API_BASE_URL,
            execution_policy,
        )
    }

    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self::with_base_url_and_execution_policy(
            api_key,
            base_url,
            ConnectorExecutionPolicy::Inline,
        )
    }

    pub fn with_base_url_and_execution_policy(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        execution_policy: ConnectorExecutionPolicy,
    ) -> Self {
        ensure_reqwest_crypto_provider();
        let client = Client::builder()
            .timeout(GRANOLA_HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            api_key: api_key.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client,
            execution_policy,
        }
    }

    fn get_json<T>(&self, path: &str, query: &[(&str, String)]) -> LocalityResult<T>
    where
        T: DeserializeOwned,
    {
        let send = || {
            let mut request = self
                .client
                .get(format!("{}{}", self.base_url, path))
                .bearer_auth(&self.api_key);
            for (key, value) in query {
                request = request.query(&[(*key, value.as_str())]);
            }
            request.send()
        };

        for attempt in 0..=DEFAULT_GRANOLA_RATE_LIMIT_RETRIES {
            let _network_permit = granola_network_gate().acquire();
            let response = match send() {
                Ok(response) => response,
                Err(error)
                    if is_retryable_transport_error(&error)
                        && attempt < DEFAULT_GRANOLA_RATE_LIMIT_RETRIES =>
                {
                    granola_network_gate().record_cooldown(granola_backoff(attempt));
                    continue;
                }
                Err(error) => {
                    return Err(LocalityError::Io(format!(
                        "Granola API request failed: {error}"
                    )));
                }
            };
            let status = response.status();
            if is_retryable_status(status) && attempt < DEFAULT_GRANOLA_RATE_LIMIT_RETRIES {
                let delay = retry_after(&response).unwrap_or_else(|| granola_backoff(attempt));
                granola_network_gate().record_cooldown(delay);
                if status == StatusCode::TOO_MANY_REQUESTS
                    && self.execution_policy.defers_provider_cooldown()
                {
                    let body = response
                        .text()
                        .unwrap_or_else(|error| format!("<failed to read error body: {error}>"));
                    return Err(LocalityError::RateLimited {
                        provider: "granola".to_string(),
                        retry_after: delay,
                        message: body,
                    });
                }
                continue;
            }
            return decode_response(response);
        }
        Err(LocalityError::Io(
            "Granola API request exhausted retries".to_string(),
        ))
    }
}

impl GranolaApi for HttpGranolaApiClient {
    fn list_notes(
        &self,
        cursor: Option<&str>,
        page_size: u32,
        created_after: Option<&str>,
        created_before: Option<&str>,
        updated_after: Option<&str>,
    ) -> LocalityResult<GranolaNoteList> {
        let mut query = vec![("page_size", page_size.clamp(1, 30).to_string())];
        if let Some(cursor) = cursor {
            query.push(("cursor", cursor.to_string()));
        }
        if let Some(created_after) = created_after {
            query.push(("created_after", created_after.to_string()));
        }
        if let Some(created_before) = created_before {
            query.push(("created_before", created_before.to_string()));
        }
        if let Some(updated_after) = updated_after {
            query.push(("updated_after", updated_after.to_string()));
        }
        self.get_json("/v1/notes", &query)
    }

    fn get_note(&self, note_id: &str, include_transcript: bool) -> LocalityResult<GranolaNote> {
        let query = if include_transcript {
            vec![("include", "transcript".to_string())]
        } else {
            Vec::new()
        };
        self.get_json(&format!("/v1/notes/{note_id}"), &query)
    }
}

fn retry_after(response: &Response) -> Option<Duration> {
    let seconds = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .parse::<u64>()
        .ok()?;
    Some(Duration::from_secs(seconds))
}

fn granola_network_config() -> ConnectorNetworkConfig {
    ConnectorNetworkConfig::new(
        "granola",
        DEFAULT_GRANOLA_REQUESTS_PER_SECOND,
        DEFAULT_GRANOLA_REQUEST_BURST,
    )
    .max_in_flight(DEFAULT_GRANOLA_MAX_IN_FLIGHT)
    .request_timeout(GRANOLA_HTTP_TIMEOUT)
    .retry(RetryConfig::exponential(
        DEFAULT_GRANOLA_RATE_LIMIT_RETRIES,
        Duration::from_secs(1),
        Duration::from_secs(16),
    ))
}

fn granola_network_gate() -> &'static ConnectorNetworkGate {
    GRANOLA_NETWORK_GATE.get_or_init(|| ConnectorNetworkGate::global(granola_network_config()))
}

fn granola_backoff(attempt: usize) -> Duration {
    granola_network_config().retry.backoff(attempt)
}

fn is_retryable_transport_error(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request() || error.is_body()
}

fn is_retryable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

fn decode_response<T>(response: Response) -> LocalityResult<T>
where
    T: DeserializeOwned,
{
    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .unwrap_or_else(|error| format!("<failed to read error body: {error}>"));
        return match status {
            StatusCode::UNAUTHORIZED => Err(LocalityError::Guardrail(
                "Granola API key is invalid or revoked; reconnect Granola".to_string(),
            )),
            StatusCode::FORBIDDEN => Err(LocalityError::Guardrail(format!(
                "Granola API access is not allowed by this plan or key scope: {body}"
            ))),
            StatusCode::NOT_FOUND => Err(LocalityError::RemoteNotFound(body)),
            StatusCode::TOO_MANY_REQUESTS => Err(LocalityError::Io(format!(
                "Granola API rate limited: {body}"
            ))),
            _ => Err(LocalityError::Io(format!(
                "Granola API returned HTTP {status}: {body}"
            ))),
        };
    }
    response
        .json()
        .map_err(|error| LocalityError::Io(format!("Granola API response decode failed: {error}")))
}

fn ensure_reqwest_crypto_provider() {
    REQWEST_CRYPTO_PROVIDER.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use locality_connector::ConnectorExecutionPolicy;
    use locality_core::LocalityError;

    use super::{GranolaApi, HttpGranolaApiClient};

    #[test]
    fn debug_redacts_api_key() {
        let client = HttpGranolaApiClient::with_base_url("grn_secret", "http://127.0.0.1:1");
        let debug = format!("{client:?}");
        assert!(!debug.contains("grn_secret"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn network_policy_uses_internal_granola_values() {
        let config = super::granola_network_config();

        assert_eq!(config.quota_scope, "granola");
        assert_eq!(config.requests_per_second, 5.0);
        assert_eq!(config.burst, 3.0);
        assert_eq!(config.max_in_flight, 8);
        assert_eq!(config.request_timeout, Duration::from_secs(30));
        assert_eq!(config.retry.max_retries, 4);
        assert_eq!(config.retry.initial_backoff, Duration::from_secs(1));
        assert_eq!(config.retry.max_backoff, Duration::from_secs(16));
    }

    #[test]
    fn list_notes_sends_bearer_key_and_pagination_query() {
        let (base_url, request_rx, server) = spawn_response(
            "HTTP/1.1 200 OK",
            r#"{"notes":[],"hasMore":false,"cursor":null}"#,
        );
        let client = HttpGranolaApiClient::with_base_url("grn_secret", base_url);
        client
            .list_notes(
                Some("next page"),
                30,
                Some("2026-01-01T00:00:00Z"),
                Some("2027-01-01T00:00:00Z"),
                Some("2026-06-01"),
            )
            .expect("list notes");
        let request = request_rx.recv().expect("request");
        server.join().expect("server");
        assert!(request.starts_with("GET /v1/notes?"));
        assert!(request.contains("page_size=30"));
        assert!(request.contains("cursor=next+page") || request.contains("cursor=next%20page"));
        assert!(request.contains("created_after="));
        assert!(request.contains("created_before="));
        assert!(request.contains("updated_after="));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer grn_secret")
        );
    }

    #[test]
    fn invalid_key_maps_to_reconnect_guardrail() {
        let (base_url, _request_rx, server) =
            spawn_response("HTTP/1.1 401 Unauthorized", "invalid key");
        let client = HttpGranolaApiClient::with_base_url("bad", base_url);
        let error = client
            .list_notes(None, 1, None, None, None)
            .expect_err("unauthorized");
        server.join().expect("server");
        assert!(
            matches!(error, LocalityError::Guardrail(message) if message.contains("reconnect Granola"))
        );
    }

    #[test]
    fn deferred_execution_returns_structured_rate_limit_without_inline_retry() {
        let (base_url, request_rx, server) =
            spawn_response("HTTP/1.1 429 Too Many Requests", "slow down");
        let client = HttpGranolaApiClient::with_base_url_and_execution_policy(
            "grn_secret",
            base_url,
            ConnectorExecutionPolicy::DeferProviderCooldown,
        );
        let error = client
            .list_notes(None, 1, None, None, None)
            .expect_err("rate limit");
        request_rx.recv().expect("one request");
        server.join().expect("server");
        assert_eq!(
            error,
            LocalityError::RateLimited {
                provider: "granola".to_string(),
                retry_after: Duration::from_secs(1),
                message: "slow down".to_string(),
            }
        );
    }

    fn spawn_response(
        status: &'static str,
        body: &'static str,
    ) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base_url = format!("http://{}", listener.local_addr().expect("address"));
        let (request_tx, request_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let count = stream.read(&mut buffer).expect("read");
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..count]);
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            request_tx
                .send(String::from_utf8_lossy(&bytes).to_string())
                .expect("send request");
            let response = format!(
                "{status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).expect("respond");
        });
        (base_url, request_rx, handle)
    }
}
