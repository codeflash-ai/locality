use std::fmt;
use std::sync::OnceLock;
use std::time::Duration;

use locality_connector::ConnectorExecutionPolicy;
use locality_connector::network::{ConnectorNetworkConfig, ConnectorNetworkGate, RetryConfig};
use locality_core::{LocalityError, LocalityResult};
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use serde::de::DeserializeOwned;

use crate::dto::{
    SlackAuthTest, SlackConversation, SlackConversationInfoResponse, SlackConversationList,
    SlackConversationListResponse, SlackHistory, SlackHistoryResponse, SlackUserInfo,
    SlackUserInfoResponse,
};

pub const DEFAULT_SLACK_API_BASE_URL: &str = "https://slack.com";
const SLACK_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const SLACK_HISTORY_REQUESTS_PER_SECOND: f64 = 1.0 / 60.0;
const SLACK_METADATA_REQUESTS_PER_SECOND: f64 = 1.0;
const SLACK_REQUEST_BURST: f64 = 1.0;
const SLACK_HISTORY_MAX_IN_FLIGHT: usize = 1;
const SLACK_METADATA_MAX_IN_FLIGHT: usize = 2;
const DEFAULT_SLACK_RATE_LIMIT_RETRIES: usize = 1;

static REQWEST_CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();
static SLACK_AUTH_TEST_NETWORK_GATE: OnceLock<ConnectorNetworkGate> = OnceLock::new();
static SLACK_CONVERSATIONS_LIST_NETWORK_GATE: OnceLock<ConnectorNetworkGate> = OnceLock::new();
static SLACK_CONVERSATIONS_INFO_NETWORK_GATE: OnceLock<ConnectorNetworkGate> = OnceLock::new();
static SLACK_CONVERSATIONS_HISTORY_NETWORK_GATE: OnceLock<ConnectorNetworkGate> = OnceLock::new();
static SLACK_CONVERSATIONS_REPLIES_NETWORK_GATE: OnceLock<ConnectorNetworkGate> = OnceLock::new();
static SLACK_USERS_INFO_NETWORK_GATE: OnceLock<ConnectorNetworkGate> = OnceLock::new();

pub trait SlackApi: fmt::Debug + Send + Sync {
    fn auth_test(&self) -> LocalityResult<SlackAuthTest>;
    fn list_public_channels(
        &self,
        limit: u32,
        cursor: Option<&str>,
    ) -> LocalityResult<SlackConversationList>;
    fn conversation_info(&self, channel: &str) -> LocalityResult<SlackConversation>;
    fn conversation_history(
        &self,
        channel: &str,
        limit: u32,
        cursor: Option<&str>,
    ) -> LocalityResult<SlackHistory>;
    fn conversation_replies(
        &self,
        channel: &str,
        thread_ts: &str,
        limit: u32,
        cursor: Option<&str>,
    ) -> LocalityResult<SlackHistory>;
    fn user_info(&self, user: &str) -> LocalityResult<SlackUserInfo>;
}

#[derive(Clone)]
pub struct HttpSlackApiClient {
    access_token: String,
    base_url: String,
    client: Client,
    execution_policy: ConnectorExecutionPolicy,
    use_network_gate: bool,
}

impl fmt::Debug for HttpSlackApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpSlackApiClient")
            .field("access_token", &"<redacted>")
            .field("base_url", &self.base_url)
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

    pub fn with_base_url(access_token: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self::with_base_url_and_execution_policy(
            access_token,
            base_url,
            ConnectorExecutionPolicy::Inline,
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
            use_network_gate: true,
        }
        .without_network_gate_for_loopback()
    }

    fn get_json<T>(&self, method: &str, query: &[(&str, String)]) -> LocalityResult<T>
    where
        T: DeserializeOwned,
    {
        let send = || {
            let mut request = self
                .client
                .get(format!("{}/api/{method}", self.base_url))
                .bearer_auth(&self.access_token);
            for (key, value) in query {
                request = request.query(&[(*key, value.as_str())]);
            }
            request.send()
        };

        for attempt in 0..=DEFAULT_SLACK_RATE_LIMIT_RETRIES {
            let _network_permit = self.acquire_method_permit(method);
            let response = match send() {
                Ok(response) => response,
                Err(error)
                    if is_retryable_transport_error(&error)
                        && attempt < DEFAULT_SLACK_RATE_LIMIT_RETRIES =>
                {
                    self.record_method_cooldown(method, slack_backoff(method, attempt));
                    continue;
                }
                Err(error) => {
                    return Err(LocalityError::Io(format!(
                        "Slack API request failed: {error}"
                    )));
                }
            };
            let status = response.status();
            if status == StatusCode::TOO_MANY_REQUESTS {
                let delay =
                    retry_after(&response).unwrap_or_else(|| slack_backoff(method, attempt));
                self.record_method_cooldown(method, delay);
                let body = response
                    .text()
                    .unwrap_or_else(|error| format!("<failed to read error body: {error}>"));
                if self.execution_policy.defers_provider_cooldown() {
                    return Err(LocalityError::RateLimited {
                        provider: "slack".to_string(),
                        retry_after: delay,
                        message: body,
                    });
                }
                return Err(LocalityError::Io(format!("Slack API rate limited: {body}")));
            }
            if is_retryable_status(status) && attempt < DEFAULT_SLACK_RATE_LIMIT_RETRIES {
                let delay =
                    retry_after(&response).unwrap_or_else(|| slack_backoff(method, attempt));
                self.record_method_cooldown(method, delay);
                continue;
            }
            return decode_http_response(response, "Slack API GET");
        }
        Err(LocalityError::Io(
            "Slack API request exhausted retries".to_string(),
        ))
    }
}

impl HttpSlackApiClient {
    fn without_network_gate_for_loopback(mut self) -> Self {
        if is_loopback_base_url(&self.base_url) {
            self.use_network_gate = false;
        }
        self
    }

    fn acquire_method_permit(
        &self,
        method: &str,
    ) -> Option<locality_connector::network::NetworkPermit> {
        self.use_network_gate
            .then(|| slack_network_gate(method).acquire())
    }

    fn record_method_cooldown(&self, method: &str, delay: Duration) {
        if self.use_network_gate {
            slack_network_gate(method).record_cooldown(delay);
        }
    }
}

fn is_loopback_base_url(url: &str) -> bool {
    let Some(authority) = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
    else {
        return false;
    };
    let host_port = authority
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(authority)
        .to_ascii_lowercase();
    let host = if host_port.starts_with('[') {
        host_port
            .split(']')
            .next()
            .map(|value| format!("{value}]"))
            .unwrap_or(host_port)
    } else {
        host_port
            .split(':')
            .next()
            .unwrap_or(host_port.as_str())
            .to_string()
    };
    matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]")
}

impl SlackApi for HttpSlackApiClient {
    fn auth_test(&self) -> LocalityResult<SlackAuthTest> {
        let response = self.get_json::<SlackAuthTest>("auth.test", &[])?;
        if response.ok {
            Ok(response)
        } else {
            Err(LocalityError::Guardrail(
                "Slack auth.test failed; reconnect Slack".to_string(),
            ))
        }
    }

    fn list_public_channels(
        &self,
        limit: u32,
        cursor: Option<&str>,
    ) -> LocalityResult<SlackConversationList> {
        let mut query = vec![
            ("types", "public_channel".to_string()),
            ("exclude_archived", "true".to_string()),
            ("limit", limit.clamp(1, 15).to_string()),
        ];
        if let Some(cursor) = cursor {
            query.push(("cursor", cursor.to_string()));
        }
        let response =
            self.get_json::<SlackConversationListResponse>("conversations.list", &query)?;
        slack_ok(response.ok, response.error.as_deref(), "conversations.list")?;
        Ok(response.into())
    }

    fn conversation_info(&self, channel: &str) -> LocalityResult<SlackConversation> {
        let query = [("channel", channel.to_string())];
        let response =
            self.get_json::<SlackConversationInfoResponse>("conversations.info", &query)?;
        slack_ok(response.ok, response.error.as_deref(), "conversations.info")?;
        response.channel.ok_or_else(|| {
            LocalityError::Io(
                "Slack conversations.info response did not include channel".to_string(),
            )
        })
    }

    fn conversation_history(
        &self,
        channel: &str,
        limit: u32,
        cursor: Option<&str>,
    ) -> LocalityResult<SlackHistory> {
        let mut query = vec![
            ("channel", channel.to_string()),
            ("limit", limit.clamp(1, 15).to_string()),
        ];
        if let Some(cursor) = cursor {
            query.push(("cursor", cursor.to_string()));
        }
        let response = self.get_json::<SlackHistoryResponse>("conversations.history", &query)?;
        slack_ok(
            response.ok,
            response.error.as_deref(),
            "conversations.history",
        )?;
        Ok(response.into())
    }

    fn conversation_replies(
        &self,
        channel: &str,
        thread_ts: &str,
        limit: u32,
        cursor: Option<&str>,
    ) -> LocalityResult<SlackHistory> {
        let mut query = vec![
            ("channel", channel.to_string()),
            ("ts", thread_ts.to_string()),
            ("limit", limit.clamp(1, 15).to_string()),
        ];
        if let Some(cursor) = cursor {
            query.push(("cursor", cursor.to_string()));
        }
        let response = self.get_json::<SlackHistoryResponse>("conversations.replies", &query)?;
        slack_ok(
            response.ok,
            response.error.as_deref(),
            "conversations.replies",
        )?;
        Ok(response.into())
    }

    fn user_info(&self, user: &str) -> LocalityResult<SlackUserInfo> {
        let query = [("user", user.to_string())];
        let response = self.get_json::<SlackUserInfoResponse>("users.info", &query)?;
        slack_ok(response.ok, response.error.as_deref(), "users.info")?;
        let user = response.user.ok_or_else(|| {
            LocalityError::Io("Slack users.info response did not include user".to_string())
        })?;
        Ok(SlackUserInfo { user })
    }
}

fn slack_ok(ok: bool, error: Option<&str>, method: &str) -> LocalityResult<()> {
    if ok {
        return Ok(());
    }
    let error = error.unwrap_or("unknown_error");
    match error {
        "invalid_auth" | "account_inactive" | "not_authed" => Err(LocalityError::Guardrail(
            "Slack token is invalid or revoked; reconnect Slack".to_string(),
        )),
        "channel_not_found" => Err(LocalityError::RemoteNotFound(error.to_string())),
        _ => Err(LocalityError::Io(format!(
            "Slack API {method} returned error `{error}`"
        ))),
    }
}

fn decode_http_response<T>(response: Response, context: &str) -> LocalityResult<T>
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
                "Slack token is invalid or revoked; reconnect Slack".to_string(),
            )),
            StatusCode::FORBIDDEN => Err(LocalityError::Guardrail(format!(
                "Slack API access is not allowed by token scopes: {body}"
            ))),
            StatusCode::NOT_FOUND => Err(LocalityError::RemoteNotFound(body)),
            _ => Err(LocalityError::Io(format!(
                "{context} returned HTTP {status}: {body}"
            ))),
        };
    }
    response
        .json()
        .map_err(|error| LocalityError::Io(format!("{context} response decode failed: {error}")))
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

fn slack_network_config(method: &str) -> ConnectorNetworkConfig {
    let (requests_per_second, max_in_flight) = match method {
        "conversations.history" | "conversations.replies" => (
            SLACK_HISTORY_REQUESTS_PER_SECOND,
            SLACK_HISTORY_MAX_IN_FLIGHT,
        ),
        _ => (
            SLACK_METADATA_REQUESTS_PER_SECOND,
            SLACK_METADATA_MAX_IN_FLIGHT,
        ),
    };
    ConnectorNetworkConfig::new(
        format!("slack:{method}"),
        requests_per_second,
        SLACK_REQUEST_BURST,
    )
    .max_in_flight(max_in_flight)
    .request_timeout(SLACK_HTTP_TIMEOUT)
    .retry(RetryConfig::exponential(
        DEFAULT_SLACK_RATE_LIMIT_RETRIES,
        Duration::from_secs(1),
        Duration::from_secs(60),
    ))
}

fn slack_network_gate(method: &str) -> &'static ConnectorNetworkGate {
    match method {
        "auth.test" => SLACK_AUTH_TEST_NETWORK_GATE
            .get_or_init(|| ConnectorNetworkGate::global(slack_network_config(method))),
        "conversations.list" => SLACK_CONVERSATIONS_LIST_NETWORK_GATE
            .get_or_init(|| ConnectorNetworkGate::global(slack_network_config(method))),
        "conversations.info" => SLACK_CONVERSATIONS_INFO_NETWORK_GATE
            .get_or_init(|| ConnectorNetworkGate::global(slack_network_config(method))),
        "conversations.history" => SLACK_CONVERSATIONS_HISTORY_NETWORK_GATE
            .get_or_init(|| ConnectorNetworkGate::global(slack_network_config(method))),
        "conversations.replies" => SLACK_CONVERSATIONS_REPLIES_NETWORK_GATE
            .get_or_init(|| ConnectorNetworkGate::global(slack_network_config(method))),
        "users.info" => SLACK_USERS_INFO_NETWORK_GATE
            .get_or_init(|| ConnectorNetworkGate::global(slack_network_config(method))),
        _ => SLACK_AUTH_TEST_NETWORK_GATE
            .get_or_init(|| ConnectorNetworkGate::global(slack_network_config("unknown"))),
    }
}

fn slack_backoff(method: &str, attempt: usize) -> Duration {
    slack_network_config(method).retry.backoff(attempt)
}

fn is_retryable_transport_error(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request() || error.is_body()
}

fn is_retryable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT
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
    use super::{SLACK_HISTORY_REQUESTS_PER_SECOND, SLACK_METADATA_REQUESTS_PER_SECOND};

    #[test]
    fn network_config_scopes_strict_history_limits_to_history_methods() {
        let history = super::slack_network_config("conversations.history");
        let replies = super::slack_network_config("conversations.replies");
        let users = super::slack_network_config("users.info");
        let channels = super::slack_network_config("conversations.info");

        assert_eq!(history.quota_scope, "slack:conversations.history");
        assert_eq!(replies.quota_scope, "slack:conversations.replies");
        assert_eq!(
            history.requests_per_second,
            SLACK_HISTORY_REQUESTS_PER_SECOND
        );
        assert_eq!(
            replies.requests_per_second,
            SLACK_HISTORY_REQUESTS_PER_SECOND
        );
        assert_eq!(
            users.requests_per_second,
            SLACK_METADATA_REQUESTS_PER_SECOND
        );
        assert_eq!(
            channels.requests_per_second,
            SLACK_METADATA_REQUESTS_PER_SECOND
        );
    }
}
