//! Notion API client boundary.
//!
//! The connector depends on this trait rather than directly on HTTP so tests
//! can run against deterministic fixtures and live API calls stay isolated.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use locality_connector::network::{
    ConnectorNetworkConfig, ConnectorNetworkGate, NetworkPermit, RetryConfig,
};
use locality_core::{LocalityError, LocalityResult};
use reqwest::StatusCode;
use reqwest::blocking::{Client, multipart};
use reqwest::header::HeaderMap;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::NotionConfig;
use crate::dto::{
    BlockDto, BlockListDto, DataSourceDto, DataSourceListDto, DatabaseDto, DatabaseListDto,
    PageDto, PageListDto,
};

pub const DEFAULT_NOTION_API_BASE_URL: &str = "https://api.notion.com";
pub const DEFAULT_NOTION_VERSION: &str = "2026-03-11";
pub const DEFAULT_NOTION_TOKEN_ENV: &str = "NOTION_TOKEN";
const DEFAULT_NOTION_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_NOTION_REQUESTS_PER_SECOND: f64 = 3.0;
const DEFAULT_NOTION_REQUEST_BURST: f64 = 3.0;
const DEFAULT_NOTION_RATE_LIMIT_RETRIES: usize = 4;

static NOTION_NETWORK_GATE: OnceLock<ConnectorNetworkGate> = OnceLock::new();
static NOTION_REQUEST_DEBUG: OnceLock<Mutex<NotionRequestDebugState>> = OnceLock::new();
static NOTION_REQUEST_DEBUG_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static NOTION_REQUEST_DEBUG_ENABLED_UNTIL_MS: AtomicU64 = AtomicU64::new(0);
static REQWEST_CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotionRequestDebugStatus {
    pub waiting_for_token: usize,
    pub active: Vec<NotionRequestDebugActive>,
    pub last_completed: Option<NotionRequestDebugCompleted>,
    pub limiter: NotionRateLimiterDebugStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotionRequestDebugActive {
    pub id: u64,
    pub method: String,
    pub path: String,
    pub attempt: usize,
    pub waited_for_token_ms: u64,
    pub started_at_unix_ms: u64,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotionRequestDebugCompleted {
    pub id: u64,
    pub method: String,
    pub path: String,
    pub attempt: usize,
    pub waited_for_token_ms: u64,
    pub elapsed_ms: u64,
    pub status: String,
    pub completed_at_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotionRateLimiterDebugStatus {
    pub tokens: f64,
    pub burst: f64,
    pub requests_per_second: f64,
    pub cooldown_remaining_ms: Option<u64>,
}

pub fn notion_http_client() -> Client {
    ensure_reqwest_crypto_provider();
    Client::new()
}

fn notion_http_client_builder() -> reqwest::blocking::ClientBuilder {
    ensure_reqwest_crypto_provider();
    Client::builder()
}

fn ensure_reqwest_crypto_provider() {
    REQWEST_CRYPTO_PROVIDER.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub trait NotionApi: std::fmt::Debug + Send + Sync {
    fn retrieve_current_user(&self) -> LocalityResult<serde_json::Value> {
        Err(LocalityError::NotImplemented(
            "retrieve Notion current user",
        ))
    }
    fn retrieve_page(&self, page_id: &str) -> LocalityResult<PageDto>;
    fn retrieve_database(&self, database_id: &str) -> LocalityResult<DatabaseDto> {
        let _ = database_id;
        Err(LocalityError::NotImplemented("retrieve Notion database"))
    }
    fn retrieve_data_source(&self, data_source_id: &str) -> LocalityResult<DataSourceDto> {
        let _ = data_source_id;
        Err(LocalityError::NotImplemented("retrieve Notion data source"))
    }
    fn retrieve_block(&self, block_id: &str) -> LocalityResult<BlockDto> {
        let _ = block_id;
        Err(LocalityError::NotImplemented("retrieve Notion block"))
    }
    fn query_data_source(
        &self,
        data_source_id: &str,
        start_cursor: Option<&str>,
    ) -> LocalityResult<PageListDto> {
        let _ = (data_source_id, start_cursor);
        Err(LocalityError::NotImplemented("query Notion data source"))
    }
    fn update_page(&self, page_id: &str, body: serde_json::Value) -> LocalityResult<PageDto> {
        let _ = (page_id, body);
        Err(LocalityError::NotImplemented("update Notion page"))
    }
    fn move_page(&self, page_id: &str, parent: serde_json::Value) -> LocalityResult<PageDto> {
        let _ = (page_id, parent);
        Err(LocalityError::NotImplemented("move Notion page"))
    }
    fn create_page(&self, body: serde_json::Value) -> LocalityResult<PageDto> {
        let _ = body;
        Err(LocalityError::NotImplemented("create Notion page"))
    }
    fn create_database(&self, body: serde_json::Value) -> LocalityResult<DatabaseDto> {
        let _ = body;
        Err(LocalityError::NotImplemented("create Notion database"))
    }
    fn retrieve_block_children(
        &self,
        block_id: &str,
        start_cursor: Option<&str>,
    ) -> LocalityResult<BlockListDto>;
    fn search_pages(&self, start_cursor: Option<&str>) -> LocalityResult<PageListDto>;
    fn search_databases(&self, start_cursor: Option<&str>) -> LocalityResult<DatabaseListDto> {
        let _ = start_cursor;
        Err(LocalityError::NotImplemented("search Notion databases"))
    }
    fn update_block(&self, block_id: &str, body: serde_json::Value) -> LocalityResult<BlockDto>;
    fn move_block(
        &self,
        block_id: &str,
        parent_id: &str,
        after: Option<&str>,
    ) -> LocalityResult<BlockDto> {
        let _ = (block_id, parent_id, after);
        Err(LocalityError::NotImplemented("move Notion block"))
    }
    fn append_block_children(
        &self,
        block_id: &str,
        body: serde_json::Value,
    ) -> LocalityResult<BlockListDto>;
    fn delete_block(&self, block_id: &str) -> LocalityResult<BlockDto>;
    fn upload_file(
        &self,
        filename: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> LocalityResult<String> {
        let _ = (filename, content_type, bytes);
        Err(LocalityError::NotImplemented("upload Notion file"))
    }
}

#[derive(Debug, Default)]
struct NotionRequestDebugState {
    waiting_for_token: usize,
    active: BTreeMap<u64, NotionRequestDebugActiveInternal>,
    last_completed: Option<NotionRequestDebugCompleted>,
}

#[derive(Clone, Debug)]
struct NotionRequestDebugActiveInternal {
    id: u64,
    method: String,
    path: String,
    attempt: usize,
    waited_for_token_ms: u64,
    started_at: Instant,
    started_at_unix_ms: u64,
}

impl NotionRequestDebugActiveInternal {
    fn public_status(&self) -> NotionRequestDebugActive {
        NotionRequestDebugActive {
            id: self.id,
            method: self.method.clone(),
            path: self.path.clone(),
            attempt: self.attempt,
            waited_for_token_ms: self.waited_for_token_ms,
            started_at_unix_ms: self.started_at_unix_ms,
            elapsed_ms: duration_ms(self.started_at.elapsed()),
        }
    }
}

pub fn notion_request_debug_status() -> NotionRequestDebugStatus {
    enable_notion_request_debug_for(Duration::from_secs(3));
    let network = notion_network_gate().status();
    let limiter = NotionRateLimiterDebugStatus {
        tokens: network.tokens,
        burst: network.burst,
        requests_per_second: network.requests_per_second,
        cooldown_remaining_ms: network.cooldown_remaining.map(duration_ms),
    };
    let state = notion_request_debug_state()
        .lock()
        .expect("notion request debug lock poisoned");
    NotionRequestDebugStatus {
        waiting_for_token: state.waiting_for_token.max(network.waiting),
        active: state
            .active
            .values()
            .map(NotionRequestDebugActiveInternal::public_status)
            .collect(),
        last_completed: state.last_completed.clone(),
        limiter,
    }
}

fn enable_notion_request_debug_for(duration: Duration) {
    let until = unix_time_ms().saturating_add(duration_ms(duration));
    NOTION_REQUEST_DEBUG_ENABLED_UNTIL_MS.fetch_max(until, Ordering::Relaxed);
}

fn notion_request_debug_enabled() -> bool {
    unix_time_ms() <= NOTION_REQUEST_DEBUG_ENABLED_UNTIL_MS.load(Ordering::Relaxed)
}

fn notion_request_debug_state() -> &'static Mutex<NotionRequestDebugState> {
    NOTION_REQUEST_DEBUG.get_or_init(|| Mutex::new(NotionRequestDebugState::default()))
}

fn record_notion_token_wait_start() -> bool {
    if !notion_request_debug_enabled() {
        return false;
    }
    let mut state = notion_request_debug_state()
        .lock()
        .expect("notion request debug lock poisoned");
    state.waiting_for_token = state.waiting_for_token.saturating_add(1);
    true
}

fn record_notion_token_wait_end(recorded: bool) {
    if !recorded {
        return;
    }
    let mut state = notion_request_debug_state()
        .lock()
        .expect("notion request debug lock poisoned");
    state.waiting_for_token = state.waiting_for_token.saturating_sub(1);
}

fn start_notion_request_debug(
    method: &str,
    path: &str,
    attempt: usize,
    waited_for_token: Duration,
) -> Option<u64> {
    if !notion_request_debug_enabled() {
        return None;
    }
    let id = NOTION_REQUEST_DEBUG_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let active = NotionRequestDebugActiveInternal {
        id,
        method: method.to_string(),
        path: path.to_string(),
        attempt,
        waited_for_token_ms: duration_ms(waited_for_token),
        started_at: Instant::now(),
        started_at_unix_ms: unix_time_ms(),
    };
    notion_request_debug_state()
        .lock()
        .expect("notion request debug lock poisoned")
        .active
        .insert(id, active);
    Some(id)
}

fn finish_notion_request_debug(id: Option<u64>, status: impl Into<String>) {
    let Some(id) = id else {
        return;
    };
    let mut state = notion_request_debug_state()
        .lock()
        .expect("notion request debug lock poisoned");
    let Some(active) = state.active.remove(&id) else {
        return;
    };
    state.last_completed = Some(NotionRequestDebugCompleted {
        id: active.id,
        method: active.method,
        path: active.path,
        attempt: active.attempt,
        waited_for_token_ms: active.waited_for_token_ms,
        elapsed_ms: duration_ms(active.started_at.elapsed()),
        status: status.into(),
        completed_at_unix_ms: unix_time_ms(),
    });
}

#[derive(Clone, Debug)]
pub struct HttpNotionApi {
    config: NotionConfig,
    client: Client,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NotionRetryClass {
    ReadSafe,
    Mutation,
}

impl HttpNotionApi {
    pub fn new(config: NotionConfig) -> Self {
        let client = notion_http_client_builder()
            .timeout(DEFAULT_NOTION_HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| notion_http_client());
        Self { config, client }
    }

    fn get_json<T>(&self, path: &str, query: &[(&str, String)]) -> LocalityResult<T>
    where
        T: DeserializeOwned,
    {
        let token = self.token()?;
        let url = format!(
            "{}/{}",
            DEFAULT_NOTION_API_BASE_URL,
            path.trim_start_matches('/')
        );
        let query = query
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect::<Vec<_>>();

        self.send_request_with_retry("GET", path, NotionRetryClass::ReadSafe, || {
            let mut request = self
                .client
                .get(&url)
                .bearer_auth(&token)
                .header("Notion-Version", DEFAULT_NOTION_VERSION);

            for (key, value) in &query {
                request = request.query(&[(key.as_str(), value.as_str())]);
            }
            request
        })
    }

    fn post_json<T>(&self, path: &str, body: impl Serialize) -> LocalityResult<T>
    where
        T: DeserializeOwned,
    {
        self.send_json(reqwest::Method::POST, path, Some(body))
    }

    fn post_read_json<T>(&self, path: &str, body: impl Serialize) -> LocalityResult<T>
    where
        T: DeserializeOwned,
    {
        self.send_json_with_retry_class(
            reqwest::Method::POST,
            path,
            Some(body),
            NotionRetryClass::ReadSafe,
        )
    }

    fn patch_json<T>(&self, path: &str, body: impl Serialize) -> LocalityResult<T>
    where
        T: DeserializeOwned,
    {
        self.send_json(reqwest::Method::PATCH, path, Some(body))
    }

    fn delete_json<T>(&self, path: &str) -> LocalityResult<T>
    where
        T: DeserializeOwned,
    {
        self.send_json::<T, serde_json::Value>(reqwest::Method::DELETE, path, None)
    }

    fn upload_file_bytes(
        &self,
        filename: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> LocalityResult<String> {
        let created: Value = self.post_json(
            "/v1/file_uploads",
            json!({
                "mode": "single_part",
                "filename": filename,
                "content_type": content_type,
            }),
        )?;
        let upload_id = created
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| LocalityError::Io("notion file upload response missing id".to_string()))?
            .to_string();
        let token = self.token()?;
        let url = format!(
            "{}/v1/file_uploads/{}/send",
            DEFAULT_NOTION_API_BASE_URL, upload_id
        );
        let upload_path = format!("/v1/file_uploads/{upload_id}/send");
        for attempt in 0..=DEFAULT_NOTION_RATE_LIMIT_RETRIES {
            let (_network_permit, waited_for_token) = acquire_notion_request_token();
            let request_debug_id =
                start_notion_request_debug("POST", &upload_path, attempt, waited_for_token);
            let part = multipart::Part::bytes(bytes.clone())
                .file_name(filename.to_string())
                .mime_str(content_type)
                .map_err(|error| {
                    LocalityError::Io(format!("notion file upload MIME failed: {error}"))
                })?;
            let form = multipart::Form::new().part("file", part);
            let response = self
                .client
                .post(&url)
                .bearer_auth(&token)
                .header("Notion-Version", DEFAULT_NOTION_VERSION)
                .multipart(form)
                .send()
                .map_err(|error| {
                    finish_notion_request_debug(
                        request_debug_id,
                        format!("transport error: {error}"),
                    );
                    LocalityError::Io(format!("notion file upload failed: {error}"))
                })?;
            let status = response.status();
            finish_notion_request_debug(request_debug_id, format!("HTTP {status}"));
            if status.is_success() {
                return Ok(upload_id);
            }

            let retry_after = retry_after_header(response.headers());
            let body = response
                .text()
                .unwrap_or_else(|error| format!("<failed to read error body: {error}>"));
            if status == StatusCode::NOT_FOUND {
                return Err(LocalityError::RemoteNotFound(body));
            }
            if is_retryable_notion_http_status(status, NotionRetryClass::Mutation)
                && attempt < DEFAULT_NOTION_RATE_LIMIT_RETRIES
            {
                record_notion_rate_limit(attempt, retry_after);
                continue;
            }
            return Err(LocalityError::Io(format!(
                "notion file upload returned HTTP {status}: {body}"
            )));
        }

        Err(LocalityError::Io(
            "notion file upload exhausted rate limit retries".to_string(),
        ))
    }

    fn send_json<T, B>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<B>,
    ) -> LocalityResult<T>
    where
        T: DeserializeOwned,
        B: Serialize,
    {
        self.send_json_with_retry_class(method, path, body, NotionRetryClass::Mutation)
    }

    fn send_json_with_retry_class<T, B>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<B>,
        retry_class: NotionRetryClass,
    ) -> LocalityResult<T>
    where
        T: DeserializeOwned,
        B: Serialize,
    {
        let token = self.token()?;
        let body = body
            .map(serde_json::to_value)
            .transpose()
            .map_err(|error| LocalityError::Io(format!("notion request encode failed: {error}")))?;
        let url = format!(
            "{}/{}",
            DEFAULT_NOTION_API_BASE_URL,
            path.trim_start_matches('/')
        );

        self.send_request_with_retry(method.as_str(), path, retry_class, || {
            let mut request = self
                .client
                .request(method.clone(), &url)
                .bearer_auth(&token)
                .header("Notion-Version", DEFAULT_NOTION_VERSION);
            if let Some(body) = &body {
                request = request.json(body);
            }
            request
        })
    }

    fn send_request_with_retry<T>(
        &self,
        method: &str,
        path: &str,
        retry_class: NotionRetryClass,
        mut build_request: impl FnMut() -> reqwest::blocking::RequestBuilder,
    ) -> LocalityResult<T>
    where
        T: DeserializeOwned,
    {
        for attempt in 0..=DEFAULT_NOTION_RATE_LIMIT_RETRIES {
            let (_network_permit, waited_for_token) = acquire_notion_request_token();
            let request_debug_id =
                start_notion_request_debug(method, path, attempt, waited_for_token);
            let response = match build_request().send() {
                Ok(response) => response,
                Err(error)
                    if is_retryable_notion_transport_error(&error)
                        && attempt < DEFAULT_NOTION_RATE_LIMIT_RETRIES =>
                {
                    finish_notion_request_debug(
                        request_debug_id,
                        format!("retryable transport error: {error}"),
                    );
                    record_notion_transient_request_failure(attempt);
                    continue;
                }
                Err(error) => {
                    finish_notion_request_debug(
                        request_debug_id,
                        format!("transport error: {error}"),
                    );
                    return Err(LocalityError::Io(format!("notion request failed: {error}")));
                }
            };
            let status = response.status();
            finish_notion_request_debug(request_debug_id, format!("HTTP {status}"));

            if status.is_success() {
                return response.json().map_err(|error| {
                    LocalityError::Io(format!("notion response decode failed: {error}"))
                });
            }

            let retry_after = retry_after_header(response.headers());
            let body = response
                .text()
                .unwrap_or_else(|error| format!("<failed to read error body: {error}>"));
            if is_retryable_notion_http_status(status, retry_class)
                && attempt < DEFAULT_NOTION_RATE_LIMIT_RETRIES
            {
                let delay = retry_after.unwrap_or_else(|| rate_limit_backoff(attempt));
                record_notion_rate_limit(attempt, Some(delay));
                if self.config.execution_policy.defers_provider_cooldown()
                    && is_notion_rate_limited(status)
                {
                    return Err(LocalityError::RateLimited {
                        provider: "notion".to_string(),
                        retry_after: delay,
                        message: body,
                    });
                }
                continue;
            }
            return Err(LocalityError::Io(format!(
                "notion api returned HTTP {status}: {body}"
            )));
        }

        Err(LocalityError::Io(
            "notion request exhausted rate limit retries".to_string(),
        ))
    }

    fn token(&self) -> LocalityResult<String> {
        if let Some(token) = &self.config.token {
            return Ok(token.clone());
        }

        std::env::var(&self.config.token_key)
            .or_else(|_| std::env::var(DEFAULT_NOTION_TOKEN_ENV))
            .map_err(|_| {
                LocalityError::InvalidState(format!(
                    "missing Notion connection; run `loc connect notion` or set {}",
                    self.config.token_key
                ))
            })
    }
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub fn notion_requests_per_second_setting() -> f64 {
    DEFAULT_NOTION_REQUESTS_PER_SECOND
}

fn notion_network_config() -> ConnectorNetworkConfig {
    ConnectorNetworkConfig::new(
        "notion",
        DEFAULT_NOTION_REQUESTS_PER_SECOND,
        DEFAULT_NOTION_REQUEST_BURST,
    )
    .request_timeout(DEFAULT_NOTION_HTTP_TIMEOUT)
    .retry(RetryConfig::exponential(
        DEFAULT_NOTION_RATE_LIMIT_RETRIES,
        Duration::from_secs(1),
        Duration::from_secs(16),
    ))
}

fn notion_network_gate() -> &'static ConnectorNetworkGate {
    NOTION_NETWORK_GATE.get_or_init(|| ConnectorNetworkGate::global(notion_network_config()))
}

fn acquire_notion_request_token() -> (NetworkPermit, Duration) {
    let recorded_wait = record_notion_token_wait_start();
    let permit = notion_network_gate().acquire();
    record_notion_token_wait_end(recorded_wait);
    let waited = permit.waited();
    (permit, waited)
}

fn record_notion_rate_limit(attempt: usize, retry_after: Option<Duration>) {
    let delay = retry_after.unwrap_or_else(|| rate_limit_backoff(attempt));
    notion_network_gate().record_cooldown(delay);
}

fn record_notion_transient_request_failure(attempt: usize) {
    notion_network_gate().record_cooldown(rate_limit_backoff(attempt));
}

fn is_retryable_notion_transport_error(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request() || error.is_body()
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

fn is_notion_rate_limited(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.as_u16() == 529
}

fn is_retryable_notion_http_status(status: StatusCode, retry_class: NotionRetryClass) -> bool {
    is_notion_rate_limited(status)
        || (retry_class == NotionRetryClass::ReadSafe
            && matches!(
                status,
                StatusCode::REQUEST_TIMEOUT
                    | StatusCode::INTERNAL_SERVER_ERROR
                    | StatusCode::BAD_GATEWAY
                    | StatusCode::SERVICE_UNAVAILABLE
                    | StatusCode::GATEWAY_TIMEOUT
            ))
}

fn rate_limit_backoff(attempt: usize) -> Duration {
    notion_network_config().retry.backoff(attempt)
}

impl NotionApi for HttpNotionApi {
    fn retrieve_current_user(&self) -> LocalityResult<serde_json::Value> {
        self.get_json("/v1/users/me", &[])
    }

    fn retrieve_page(&self, page_id: &str) -> LocalityResult<PageDto> {
        self.get_json(&format!("/v1/pages/{page_id}"), &[])
    }

    fn retrieve_database(&self, database_id: &str) -> LocalityResult<DatabaseDto> {
        self.get_json(&format!("/v1/databases/{database_id}"), &[])
    }

    fn retrieve_data_source(&self, data_source_id: &str) -> LocalityResult<DataSourceDto> {
        self.get_json(&format!("/v1/data_sources/{data_source_id}"), &[])
    }

    fn retrieve_block(&self, block_id: &str) -> LocalityResult<BlockDto> {
        self.get_json(&format!("/v1/blocks/{block_id}"), &[])
    }

    fn query_data_source(
        &self,
        data_source_id: &str,
        start_cursor: Option<&str>,
    ) -> LocalityResult<PageListDto> {
        let mut body = json!({
            "page_size": 100,
        });

        if let Some(start_cursor) = start_cursor {
            body["start_cursor"] = json!(start_cursor);
        }

        self.post_read_json(&format!("/v1/data_sources/{data_source_id}/query"), body)
    }

    fn retrieve_block_children(
        &self,
        block_id: &str,
        start_cursor: Option<&str>,
    ) -> LocalityResult<BlockListDto> {
        let mut query = vec![("page_size", "100".to_string())];
        if let Some(start_cursor) = start_cursor {
            query.push(("start_cursor", start_cursor.to_string()));
        }

        self.get_json(&format!("/v1/blocks/{block_id}/children"), &query)
    }

    fn search_pages(&self, start_cursor: Option<&str>) -> LocalityResult<PageListDto> {
        let mut body = json!({
            "page_size": 100,
            "filter": {
                "property": "object",
                "value": "page"
            },
            "sort": {
                "direction": "descending",
                "timestamp": "last_edited_time"
            }
        });

        if let Some(start_cursor) = start_cursor {
            body["start_cursor"] = json!(start_cursor);
        }

        self.post_read_json("/v1/search", body)
    }

    fn search_databases(&self, start_cursor: Option<&str>) -> LocalityResult<DatabaseListDto> {
        let data_sources: DataSourceListDto =
            self.post_read_json("/v1/search", data_source_search_body(start_cursor))?;
        let mut seen = BTreeSet::new();
        let mut databases = Vec::new();
        for data_source in data_sources.results {
            let Some(database_id) = data_source
                .parent
                .as_ref()
                .and_then(|parent| parent.database_id.as_deref())
            else {
                continue;
            };
            if seen.insert(database_id.to_string()) {
                databases.push(self.retrieve_database(database_id)?);
            }
        }

        Ok(DatabaseListDto {
            results: databases,
            next_cursor: data_sources.next_cursor,
            has_more: data_sources.has_more,
        })
    }

    fn update_page(&self, page_id: &str, body: serde_json::Value) -> LocalityResult<PageDto> {
        self.patch_json(&format!("/v1/pages/{page_id}"), body)
    }

    fn move_page(&self, page_id: &str, parent: serde_json::Value) -> LocalityResult<PageDto> {
        self.post_json(
            &format!("/v1/pages/{page_id}/move"),
            json!({ "parent": parent }),
        )
    }

    fn create_page(&self, body: serde_json::Value) -> LocalityResult<PageDto> {
        self.post_json("/v1/pages", body)
    }

    fn create_database(&self, body: serde_json::Value) -> LocalityResult<DatabaseDto> {
        self.post_json("/v1/databases", body)
    }

    fn update_block(&self, block_id: &str, body: serde_json::Value) -> LocalityResult<BlockDto> {
        self.patch_json(&format!("/v1/blocks/{block_id}"), body)
    }

    fn move_block(
        &self,
        block_id: &str,
        parent_id: &str,
        after: Option<&str>,
    ) -> LocalityResult<BlockDto> {
        let _ = (block_id, parent_id, after);
        Err(LocalityError::Unsupported(
            "Notion API does not support moving existing blocks directly",
        ))
    }

    fn append_block_children(
        &self,
        block_id: &str,
        body: serde_json::Value,
    ) -> LocalityResult<BlockListDto> {
        self.patch_json(&format!("/v1/blocks/{block_id}/children"), body)
    }

    fn delete_block(&self, block_id: &str) -> LocalityResult<BlockDto> {
        self.delete_json(&format!("/v1/blocks/{block_id}"))
    }

    fn upload_file(
        &self,
        filename: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> LocalityResult<String> {
        self.upload_file_bytes(filename, content_type, bytes)
    }
}

fn data_source_search_body(start_cursor: Option<&str>) -> Value {
    let mut body = json!({
        "page_size": 100,
        "filter": {
            "property": "object",
            "value": "data_source"
        },
        "sort": {
            "direction": "descending",
            "timestamp": "last_edited_time"
        }
    });

    if let Some(start_cursor) = start_cursor {
        body["start_cursor"] = json!(start_cursor);
    }

    body
}

#[cfg(test)]
mod tests {
    use super::{
        HttpNotionApi, NotionRetryClass, data_source_search_body, notion_http_client_builder,
        notion_network_config, rate_limit_backoff, retry_after_header,
    };
    use locality_core::LocalityError;
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
    use serde_json::Value;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::thread;
    use std::time::Duration;

    #[test]
    fn search_databases_uses_current_notion_data_source_filter() {
        let body = data_source_search_body(Some("cursor-1"));

        assert_eq!(body["filter"]["property"], "object");
        assert_eq!(body["filter"]["value"], "data_source");
        assert_eq!(body["start_cursor"], "cursor-1");
    }

    #[test]
    fn retry_after_header_parses_seconds() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("3"));

        assert_eq!(retry_after_header(&headers), Some(Duration::from_secs(3)));
    }

    #[test]
    fn rate_limit_backoff_caps_exponential_delay() {
        assert_eq!(rate_limit_backoff(0), Duration::from_secs(1));
        assert_eq!(rate_limit_backoff(3), Duration::from_secs(8));
        assert_eq!(rate_limit_backoff(99), Duration::from_secs(16));
    }

    #[test]
    fn network_policy_uses_the_established_internal_notion_values() {
        let config = notion_network_config();

        assert_eq!(config.quota_scope, "notion");
        assert_eq!(config.requests_per_second, 3.0);
        assert_eq!(config.burst, 3.0);
        assert_eq!(config.request_timeout, Duration::from_secs(30));
        assert_eq!(config.retry.max_retries, 4);
        assert_eq!(config.retry.initial_backoff, Duration::from_secs(1));
        assert_eq!(config.retry.max_backoff, Duration::from_secs(16));
    }

    #[test]
    fn send_request_retries_transient_timeout_before_returning_response() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local server");
        listener
            .set_nonblocking(true)
            .expect("set listener nonblocking");
        let url = format!("http://{}/transient", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let server = thread::spawn(move || {
            let mut accepted = 0;
            while !server_stop.load(Ordering::Relaxed) || accepted == 0 {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        accepted += 1;
                        read_http_request_headers(&mut stream);
                        if accepted == 1 {
                            thread::sleep(Duration::from_millis(250));
                            continue;
                        }

                        let _ = write_ok_response(&mut stream);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept local request: {error}"),
                }
            }
            accepted
        });

        let api = HttpNotionApi {
            config: crate::NotionConfig::default(),
            client: notion_http_client_builder()
                .timeout(Duration::from_millis(50))
                .build()
                .expect("build timeout client"),
        };
        let mut attempts = 0;
        let result = api.send_request_with_retry::<Value>(
            "GET",
            "/test",
            NotionRetryClass::ReadSafe,
            || {
                attempts += 1;
                api.client.get(&url)
            },
        );

        stop.store(true, Ordering::Relaxed);
        let accepted = server.join().expect("join local server");
        assert!(
            accepted >= 2,
            "test server should receive the timed-out request and at least one retry; accepted {accepted}"
        );
        assert!(
            attempts >= 2,
            "request should retry after the transient timeout; attempts {attempts}"
        );
        assert_eq!(
            result.expect("retry timeout request").get("ok"),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn send_request_retries_service_unavailable_before_returning_response() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local server");
        listener
            .set_nonblocking(true)
            .expect("set listener nonblocking");
        let url = format!("http://{}/transient", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let server = thread::spawn(move || {
            let mut accepted = 0;
            while !server_stop.load(Ordering::Relaxed) || accepted == 0 {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        accepted += 1;
                        read_http_request_headers(&mut stream);
                        if accepted == 1 {
                            let _ = write_service_unavailable_response(&mut stream);
                            continue;
                        }

                        let _ = write_ok_response(&mut stream);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept local request: {error}"),
                }
            }
            accepted
        });

        let api = HttpNotionApi {
            config: crate::NotionConfig::default(),
            client: notion_http_client_builder()
                .timeout(Duration::from_millis(500))
                .build()
                .expect("build timeout client"),
        };
        let mut attempts = 0;
        let result = api.send_request_with_retry::<Value>(
            "GET",
            "/test",
            NotionRetryClass::ReadSafe,
            || {
                attempts += 1;
                api.client.get(&url)
            },
        );

        stop.store(true, Ordering::Relaxed);
        let accepted = server.join().expect("join local server");
        assert_eq!(accepted, 2);
        assert_eq!(attempts, 2);
        assert_eq!(
            result.expect("retry 503 request").get("ok"),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn deferred_background_request_returns_structured_rate_limit_without_inline_retry() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local server");
        let url = format!("http://{}/rate-limited", listener.local_addr().unwrap());
        let expected_body =
            r#"{"object":"error","status":429,"code":"rate_limited","message":"slow down"}"#;
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            read_http_request_headers(&mut stream);
            let body =
                br#"{"object":"error","status":429,"code":"rate_limited","message":"slow down"}"#;
            write!(
                stream,
                "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("write headers");
            stream.write_all(body).expect("write body");
        });

        let api = HttpNotionApi {
            config: crate::NotionConfig::default().with_execution_policy(
                locality_connector::ConnectorExecutionPolicy::DeferProviderCooldown,
            ),
            client: notion_http_client_builder()
                .timeout(Duration::from_millis(500))
                .build()
                .expect("build client"),
        };
        let mut attempts = 0;
        let error = api
            .send_request_with_retry::<Value>("GET", "/test", NotionRetryClass::ReadSafe, || {
                attempts += 1;
                api.client.get(&url)
            })
            .expect_err("429 should be returned to background scheduler");

        server.join().expect("join server");
        assert_eq!(
            attempts, 1,
            "background request must not sleep and retry inline"
        );
        assert_eq!(
            error,
            LocalityError::RateLimited {
                provider: "notion".to_string(),
                retry_after: Duration::ZERO,
                message: expected_body.to_string(),
            }
        );
    }

    #[test]
    fn send_request_does_not_retry_service_unavailable_for_mutation_methods() {
        for method in ["POST", "PATCH"] {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind local server");
            listener
                .set_nonblocking(true)
                .expect("set listener nonblocking");
            let url = format!("http://{}/transient", listener.local_addr().unwrap());
            let stop = Arc::new(AtomicBool::new(false));
            let server_stop = Arc::clone(&stop);
            let server = thread::spawn(move || {
                let mut accepted = 0;
                while !server_stop.load(Ordering::Relaxed) || accepted == 0 {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            accepted += 1;
                            read_http_request_headers(&mut stream);
                            let _ = write_service_unavailable_response(&mut stream);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("accept local request: {error}"),
                    }
                }
                accepted
            });

            let api = HttpNotionApi {
                config: crate::NotionConfig::default(),
                client: notion_http_client_builder()
                    .timeout(Duration::from_millis(500))
                    .build()
                    .expect("build timeout client"),
            };
            let mut attempts = 0;
            let result = api.send_request_with_retry::<Value>(
                method,
                "/test",
                NotionRetryClass::Mutation,
                || {
                    attempts += 1;
                    api.client.request(
                        reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
                        &url,
                    )
                },
            );

            stop.store(true, Ordering::Relaxed);
            let accepted = server.join().expect("join local server");
            assert_eq!(accepted, 1, "{method} should not retry HTTP 503");
            assert_eq!(attempts, 1, "{method} should only be built once");
            assert!(
                result
                    .expect_err("mutation 503 should be returned without retry")
                    .to_string()
                    .contains("notion api returned HTTP 503"),
                "{method} should surface the original HTTP 503"
            );
        }
    }

    fn write_service_unavailable_response(stream: &mut TcpStream) -> std::io::Result<()> {
        let body = br#"{"object":"error","status":503,"code":"service_unavailable"}"#;
        write!(
            stream,
            "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )?;
        stream.write_all(body)
    }

    fn write_ok_response(stream: &mut TcpStream) -> std::io::Result<()> {
        let body = br#"{"ok":true}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )?;
        stream.write_all(body)
    }

    fn read_http_request_headers(stream: &mut TcpStream) {
        stream
            .set_nonblocking(false)
            .expect("set request stream blocking");
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .expect("set request read timeout");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 512];
        loop {
            let read = stream.read(&mut buffer).expect("read request headers");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
            assert!(
                request.len() <= 8192,
                "request headers exceeded test server limit"
            );
        }
    }
}
