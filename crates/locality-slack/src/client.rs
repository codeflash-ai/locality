use std::fmt;
use std::sync::OnceLock;
use std::time::Duration;

use locality_connector::ConnectorExecutionPolicy;
use locality_connector::network::{ConnectorNetworkConfig, ConnectorNetworkGate, RetryConfig};
use locality_core::{LocalityError, LocalityResult};
use reqwest::blocking::{Client, Response};
use reqwest::header::HeaderMap;
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;

use crate::dto::{
    SlackAuthTestResponse, SlackConversationsListResponse, SlackHistoryResponse,
    SlackUsersListResponse,
};

pub const DEFAULT_SLACK_API_BASE_URL: &str = "https://slack.com/api";
const SLACK_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const SLACK_METADATA_REQUESTS_PER_SECOND: f64 = 1.0;
const SLACK_METADATA_BURST: f64 = 2.0;
const SLACK_METADATA_MAX_IN_FLIGHT: usize = 2;
const SLACK_HISTORY_REQUESTS_PER_SECOND: f64 = 1.0 / 60.0;
const SLACK_HISTORY_BURST: f64 = 1.0;
const SLACK_HISTORY_MAX_IN_FLIGHT: usize = 1;
const SLACK_RATE_LIMIT_RETRIES: usize = 4;

static REQWEST_CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();
static SLACK_METADATA_GATE: OnceLock<ConnectorNetworkGate> = OnceLock::new();
static SLACK_HISTORY_GATE: OnceLock<ConnectorNetworkGate> = OnceLock::new();

pub trait SlackApi: fmt::Debug + Send + Sync {
    fn auth_test(&self) -> LocalityResult<SlackAuthTestResponse>;

    fn conversations_list(
        &self,
        types: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> LocalityResult<SlackConversationsListResponse>;

    fn conversations_history(
        &self,
        channel: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> LocalityResult<SlackHistoryResponse>;

    fn users_list(
        &self,
        cursor: Option<&str>,
        limit: u32,
    ) -> LocalityResult<SlackUsersListResponse>;
}

#[derive(Clone)]
pub struct HttpSlackApiClient {
    access_token: String,
    base_url: String,
    client: Client,
    execution_policy: ConnectorExecutionPolicy,
}

impl fmt::Debug for HttpSlackApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpSlackApiClient")
            .field("access_token", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("execution_policy", &self.execution_policy)
            .finish_non_exhaustive()
    }
}

impl HttpSlackApiClient {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self::with_execution_policy(access_token, ConnectorExecutionPolicy::Inline)
    }

    pub fn with_execution_policy(
        access_token: impl Into<String>,
        execution_policy: ConnectorExecutionPolicy,
    ) -> Self {
        Self::with_base_url_and_execution_policy(
            access_token,
            DEFAULT_SLACK_API_BASE_URL,
            execution_policy,
        )
    }

    pub fn with_base_url_and_execution_policy(
        access_token: impl Into<String>,
        base_url: impl Into<String>,
        execution_policy: ConnectorExecutionPolicy,
    ) -> Self {
        ensure_reqwest_crypto_provider();
        let client = Client::builder()
            .timeout(SLACK_HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            access_token: access_token.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client,
            execution_policy,
        }
    }

    fn get_json<T>(
        &self,
        http_method: Method,
        method: &str,
        query: &[(&str, String)],
        rate_gate: SlackRateGate,
    ) -> LocalityResult<T>
    where
        T: DeserializeOwned + SlackOk,
    {
        for attempt in 0..=SLACK_RATE_LIMIT_RETRIES {
            let gate = slack_rate_gate(rate_gate);
            let _network_permit = gate.acquire();
            let mut request = self
                .client
                .request(http_method.clone(), format!("{}/{}", self.base_url, method))
                .bearer_auth(&self.access_token);
            for (key, value) in query {
                request = request.query(&[(*key, value.as_str())]);
            }

            let response = match request.send() {
                Ok(response) => response,
                Err(error)
                    if is_retryable_transport_error(&error)
                        && attempt < SLACK_RATE_LIMIT_RETRIES =>
                {
                    gate.record_cooldown(slack_backoff(rate_gate, attempt));
                    continue;
                }
                Err(error) => {
                    return Err(LocalityError::Io(format!(
                        "Slack API {method} failed: {error}"
                    )));
                }
            };

            let status = response.status();
            if status == StatusCode::TOO_MANY_REQUESTS
                && self.execution_policy.defers_provider_cooldown()
            {
                let delay =
                    retry_after(&response).unwrap_or_else(|| slack_backoff(rate_gate, attempt));
                gate.record_cooldown(delay);
                let body = response
                    .text()
                    .unwrap_or_else(|error| format!("<failed to read error body: {error}>"));
                return Err(LocalityError::RateLimited {
                    provider: "slack".to_string(),
                    retry_after: delay,
                    message: body,
                });
            }

            if is_retryable_status(status) && attempt < SLACK_RATE_LIMIT_RETRIES {
                let delay =
                    retry_after(&response).unwrap_or_else(|| slack_backoff(rate_gate, attempt));
                gate.record_cooldown(delay);
                continue;
            }

            return decode_slack_response(method, response);
        }

        Err(LocalityError::Io(
            "Slack API request exhausted retries".to_string(),
        ))
    }
}

impl SlackApi for HttpSlackApiClient {
    fn auth_test(&self) -> LocalityResult<SlackAuthTestResponse> {
        self.get_json(Method::POST, "auth.test", &[], SlackRateGate::Metadata)
    }

    fn conversations_list(
        &self,
        types: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> LocalityResult<SlackConversationsListResponse> {
        let mut query = vec![
            ("types", types.to_string()),
            ("exclude_archived", "true".to_string()),
            ("limit", limit.clamp(1, 200).to_string()),
        ];
        if let Some(cursor) = cursor.filter(|value| !value.is_empty()) {
            query.push(("cursor", cursor.to_string()));
        }
        self.get_json(
            Method::GET,
            "conversations.list",
            &query,
            SlackRateGate::Metadata,
        )
    }

    fn conversations_history(
        &self,
        channel: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> LocalityResult<SlackHistoryResponse> {
        let mut query = vec![
            ("channel", channel.to_string()),
            ("limit", limit.clamp(1, 15).to_string()),
        ];
        if let Some(cursor) = cursor.filter(|value| !value.is_empty()) {
            query.push(("cursor", cursor.to_string()));
        }
        self.get_json(
            Method::GET,
            "conversations.history",
            &query,
            SlackRateGate::History,
        )
    }

    fn users_list(
        &self,
        cursor: Option<&str>,
        limit: u32,
    ) -> LocalityResult<SlackUsersListResponse> {
        let mut query = vec![("limit", limit.clamp(1, 200).to_string())];
        if let Some(cursor) = cursor.filter(|value| !value.is_empty()) {
            query.push(("cursor", cursor.to_string()));
        }
        self.get_json(Method::GET, "users.list", &query, SlackRateGate::Metadata)
    }
}

trait SlackOk {
    fn ok(&self) -> bool;
    fn error(&self) -> Option<&str>;
}

impl SlackOk for SlackAuthTestResponse {
    fn ok(&self) -> bool {
        self.ok
    }

    fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

impl SlackOk for SlackConversationsListResponse {
    fn ok(&self) -> bool {
        self.ok
    }

    fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

impl SlackOk for SlackHistoryResponse {
    fn ok(&self) -> bool {
        self.ok
    }

    fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

impl SlackOk for SlackUsersListResponse {
    fn ok(&self) -> bool {
        self.ok
    }

    fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlackRateGate {
    Metadata,
    History,
}

fn decode_slack_response<T>(method: &str, response: Response) -> LocalityResult<T>
where
    T: DeserializeOwned + SlackOk,
{
    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .unwrap_or_else(|error| format!("<failed to read error body: {error}>"));
        return match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(LocalityError::Guardrail(
                format!("Slack API access denied for {method}: {body}"),
            )),
            StatusCode::NOT_FOUND => Err(LocalityError::RemoteNotFound(body)),
            StatusCode::TOO_MANY_REQUESTS => Err(LocalityError::Io(format!(
                "Slack API {method} rate limited: {body}"
            ))),
            _ => Err(LocalityError::Io(format!(
                "Slack API {method} returned HTTP {status}: {body}"
            ))),
        };
    }

    let decoded = response
        .json::<T>()
        .map_err(|error| LocalityError::Io(format!("Slack API {method} decode failed: {error}")))?;
    if decoded.ok() {
        Ok(decoded)
    } else {
        slack_logical_error(method, decoded.error().unwrap_or("unknown_error")).map(|_| decoded)
    }
}

fn slack_logical_error(method: &str, error: &str) -> LocalityResult<()> {
    match error {
        "channel_not_found" | "file_not_found" | "message_not_found" | "team_not_found"
        | "thread_not_found" | "user_not_found" => {
            Err(LocalityError::RemoteNotFound(error.to_string()))
        }
        "access_denied"
        | "accesslimited"
        | "account_inactive"
        | "ekm_access_denied"
        | "enterprise_is_restricted"
        | "invalid_auth"
        | "missing_scope"
        | "no_permission"
        | "not_allowed_token_type"
        | "not_authed"
        | "not_in_channel"
        | "team_access_not_granted"
        | "token_expired"
        | "token_revoked" => Err(LocalityError::Guardrail(format!(
            "Slack API {method} failed: {error}"
        ))),
        _ => Err(LocalityError::Io(format!(
            "Slack API {method} failed: {error}"
        ))),
    }
}

fn slack_metadata_config() -> ConnectorNetworkConfig {
    ConnectorNetworkConfig::new(
        "slack-metadata",
        SLACK_METADATA_REQUESTS_PER_SECOND,
        SLACK_METADATA_BURST,
    )
    .max_in_flight(SLACK_METADATA_MAX_IN_FLIGHT)
    .request_timeout(SLACK_HTTP_TIMEOUT)
    .retry(RetryConfig::exponential(
        SLACK_RATE_LIMIT_RETRIES,
        Duration::from_secs(1),
        Duration::from_secs(16),
    ))
}

fn slack_history_config() -> ConnectorNetworkConfig {
    ConnectorNetworkConfig::new(
        "slack-history",
        SLACK_HISTORY_REQUESTS_PER_SECOND,
        SLACK_HISTORY_BURST,
    )
    .max_in_flight(SLACK_HISTORY_MAX_IN_FLIGHT)
    .request_timeout(SLACK_HTTP_TIMEOUT)
    .retry(RetryConfig::exponential(
        SLACK_RATE_LIMIT_RETRIES,
        Duration::from_secs(15),
        Duration::from_secs(60),
    ))
}

fn slack_rate_gate(rate_gate: SlackRateGate) -> &'static ConnectorNetworkGate {
    match rate_gate {
        SlackRateGate::Metadata => SLACK_METADATA_GATE
            .get_or_init(|| ConnectorNetworkGate::global(slack_metadata_config())),
        SlackRateGate::History => {
            SLACK_HISTORY_GATE.get_or_init(|| ConnectorNetworkGate::global(slack_history_config()))
        }
    }
}

fn slack_backoff(rate_gate: SlackRateGate, attempt: usize) -> Duration {
    match rate_gate {
        SlackRateGate::Metadata => slack_metadata_config().retry.backoff(attempt),
        SlackRateGate::History => slack_history_config().retry.backoff(attempt),
    }
}

fn retry_after(response: &Response) -> Option<Duration> {
    retry_after_header(response.headers())
}

fn retry_after_header(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
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

    use super::*;

    #[test]
    fn auth_test_sends_post_request() {
        let (base_url, request_rx, server) = spawn_response(
            "HTTP/1.1 200 OK",
            r#"{"ok":true,"team":"Locality","team_id":"T123"}"#,
        );
        let client = HttpSlackApiClient::with_base_url_and_execution_policy(
            "xoxb-token",
            base_url,
            ConnectorExecutionPolicy::Inline,
        );

        client.auth_test().expect("auth test");
        let request = request_rx.recv().expect("request");
        server.join().expect("server");

        assert!(request.starts_with("POST /auth.test HTTP/1.1"));
    }

    #[test]
    fn slack_error_body_becomes_guardrail() {
        let error = slack_logical_error("conversations.list", "missing_scope").expect_err("error");
        assert!(
            error
                .to_string()
                .contains("Slack API conversations.list failed: missing_scope")
        );
    }

    #[test]
    fn slack_access_errors_become_guardrails() {
        assert!(matches!(
            slack_logical_error("conversations.history", "not_in_channel"),
            Err(LocalityError::Guardrail(_))
        ));
        assert!(matches!(
            slack_logical_error("auth.test", "token_expired"),
            Err(LocalityError::Guardrail(_))
        ));
    }

    #[test]
    fn slack_missing_resource_errors_remain_remote_not_found() {
        assert!(matches!(
            slack_logical_error("conversations.history", "channel_not_found"),
            Err(LocalityError::RemoteNotFound(error)) if error == "channel_not_found"
        ));
    }

    #[test]
    fn slack_unknown_errors_remain_io() {
        assert!(matches!(
            slack_logical_error("users.list", "something_new"),
            Err(LocalityError::Io(error))
                if error == "Slack API users.list failed: something_new"
        ));
    }

    #[test]
    fn retry_after_seconds_parses() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "42".parse().expect("header"));
        assert_eq!(retry_after_header(&headers), Some(Duration::from_secs(42)));
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
