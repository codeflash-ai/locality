use std::fmt;
use std::sync::OnceLock;
use std::time::Duration;

use locality_connector::network::{ConnectorNetworkConfig, ConnectorNetworkGate, RetryConfig};
use locality_core::{LocalityError, LocalityResult};
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::dto::{
    LinearIssue, LinearIssuePage, LinearIssuePriority, LinearIssueState, LinearIssueUpdateInput,
    LinearLabel, LinearProject, LinearTeam, LinearUser,
};

pub const DEFAULT_LINEAR_GRAPHQL_URL: &str = "https://api.linear.app/graphql";
const LINEAR_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_LINEAR_REQUESTS_PER_SECOND: f64 = 5.0;
const DEFAULT_LINEAR_REQUEST_BURST: f64 = 4.0;
const DEFAULT_LINEAR_MAX_IN_FLIGHT: usize = 8;
const DEFAULT_LINEAR_RATE_LIMIT_RETRIES: usize = 4;
const PAGE_SIZE: i64 = 50;

static REQWEST_CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();
static LINEAR_NETWORK_GATE: OnceLock<ConnectorNetworkGate> = OnceLock::new();

pub trait LinearApi: fmt::Debug + Send + Sync {
    fn list_issues(
        &self,
        cursor: Option<&str>,
        updated_after: Option<&str>,
        team_id: Option<&str>,
    ) -> LocalityResult<LinearIssuePage>;
    fn get_issue(&self, issue_id: &str) -> LocalityResult<LinearIssue>;
    fn update_issue(&self, input: LinearIssueUpdateInput) -> LocalityResult<LinearIssue>;
}

#[derive(Clone)]
pub struct HttpLinearApiClient {
    token: String,
    graphql_url: String,
    client: Client,
}

impl fmt::Debug for HttpLinearApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpLinearApiClient")
            .field("token", &"<redacted>")
            .field("graphql_url", &self.graphql_url)
            .finish_non_exhaustive()
    }
}

impl HttpLinearApiClient {
    pub fn new(token: impl Into<String>) -> Self {
        Self::with_graphql_url(token, DEFAULT_LINEAR_GRAPHQL_URL)
    }

    pub fn with_graphql_url(token: impl Into<String>, graphql_url: impl Into<String>) -> Self {
        ensure_reqwest_crypto_provider();
        let client = Client::builder()
            .timeout(LINEAR_HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            token: token.into(),
            graphql_url: graphql_url.into(),
            client,
        }
    }

    fn graphql<T>(&self, query: &str, variables: Value) -> LocalityResult<T>
    where
        T: DeserializeOwned,
    {
        let body = json!({
            "query": query,
            "variables": variables,
        });
        let send = || {
            self.client
                .post(&self.graphql_url)
                .header(reqwest::header::AUTHORIZATION, self.token.as_str())
                .json(&body)
                .send()
        };

        for attempt in 0..=DEFAULT_LINEAR_RATE_LIMIT_RETRIES {
            let _network_permit = linear_network_gate().acquire();
            let response = match send() {
                Ok(response) => response,
                Err(error)
                    if is_retryable_transport_error(&error)
                        && attempt < DEFAULT_LINEAR_RATE_LIMIT_RETRIES =>
                {
                    linear_network_gate().record_cooldown(linear_backoff(attempt));
                    continue;
                }
                Err(error) => {
                    return Err(LocalityError::Io(format!(
                        "Linear API request failed: {error}"
                    )));
                }
            };
            let status = response.status();
            if is_retryable_status(status) && attempt < DEFAULT_LINEAR_RATE_LIMIT_RETRIES {
                let delay = retry_after(&response).unwrap_or_else(|| linear_backoff(attempt));
                linear_network_gate().record_cooldown(delay);
                continue;
            }
            return decode_graphql_response(response);
        }
        Err(LocalityError::Io(
            "Linear API request exhausted retries".to_string(),
        ))
    }
}

impl LinearApi for HttpLinearApiClient {
    fn list_issues(
        &self,
        cursor: Option<&str>,
        updated_after: Option<&str>,
        team_id: Option<&str>,
    ) -> LocalityResult<LinearIssuePage> {
        let filter = issue_filter(updated_after, team_id);
        let data: IssuesData = self.graphql(
            ISSUE_LIST_QUERY,
            json!({
                "first": PAGE_SIZE,
                "after": cursor,
                "filter": filter,
            }),
        )?;
        Ok(data.issues.into())
    }

    fn get_issue(&self, issue_id: &str) -> LocalityResult<LinearIssue> {
        let data: IssueData = self.graphql(ISSUE_QUERY, json!({ "id": issue_id }))?;
        data.issue
            .map(LinearIssue::from)
            .ok_or_else(|| LocalityError::RemoteNotFound(issue_id.to_string()))
    }

    fn update_issue(&self, input: LinearIssueUpdateInput) -> LocalityResult<LinearIssue> {
        let issue_id = input.issue_id.clone();
        let Some(update_input) = issue_update_input(&input) else {
            return self.get_issue(&issue_id);
        };
        let data: IssueUpdateData = self.graphql(
            ISSUE_UPDATE_MUTATION,
            json!({
                "id": issue_id,
                "input": update_input,
            }),
        )?;
        if !data.issue_update.success {
            return Err(LocalityError::InvalidState(
                "Linear issueUpdate returned success=false".to_string(),
            ));
        }
        Ok(LinearIssue::from(data.issue_update.issue))
    }
}

fn issue_filter(updated_after: Option<&str>, team_id: Option<&str>) -> Option<Value> {
    let mut filter = serde_json::Map::new();
    if let Some(updated_after) = updated_after {
        filter.insert("updatedAt".to_string(), json!({ "gte": updated_after }));
    }
    if let Some(team_id) = team_id {
        filter.insert("team".to_string(), json!({ "id": { "eq": team_id } }));
    }
    (!filter.is_empty()).then_some(Value::Object(filter))
}

fn issue_update_input(input: &LinearIssueUpdateInput) -> Option<Value> {
    let mut map = serde_json::Map::new();
    if let Some(title) = &input.title {
        map.insert("title".to_string(), json!(title));
    }
    if let Some(description) = &input.description {
        map.insert("description".to_string(), json!(description));
    }
    if let Some(team_id) = &input.team_id {
        map.insert("teamId".to_string(), json!(team_id));
    }
    if let Some(state_id) = &input.state_id {
        map.insert("stateId".to_string(), json!(state_id));
    }
    if let Some(project_id) = &input.project_id {
        map.insert("projectId".to_string(), json!(project_id));
    }
    if let Some(assignee_id) = &input.assignee_id {
        map.insert("assigneeId".to_string(), json!(assignee_id));
    }
    (!map.is_empty()).then_some(Value::Object(map))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssuesData {
    issues: IssueConnection,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssueData {
    issue: Option<GraphqlIssue>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssueUpdateData {
    issue_update: IssueUpdatePayload,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssueUpdatePayload {
    success: bool,
    issue: GraphqlIssue,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssueConnection {
    nodes: Vec<GraphqlIssue>,
    page_info: PageInfo,
}

impl From<IssueConnection> for LinearIssuePage {
    fn from(value: IssueConnection) -> Self {
        Self {
            issues: value.nodes.into_iter().map(LinearIssue::from).collect(),
            has_next_page: value.page_info.has_next_page,
            end_cursor: value.page_info.end_cursor,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphqlIssue {
    id: String,
    identifier: String,
    title: String,
    description: Option<String>,
    url: String,
    created_at: String,
    updated_at: String,
    archived_at: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
    canceled_at: Option<String>,
    auto_archived_at: Option<String>,
    auto_closed_at: Option<String>,
    started_triage_at: Option<String>,
    triaged_at: Option<String>,
    snoozed_until_at: Option<String>,
    added_to_cycle_at: Option<String>,
    added_to_project_at: Option<String>,
    added_to_team_at: Option<String>,
    due_date: Option<String>,
    priority: Option<i64>,
    priority_label: Option<String>,
    estimate: Option<f64>,
    team: GraphqlTeam,
    state: GraphqlState,
    project: Option<GraphqlProject>,
    assignee: Option<GraphqlUser>,
    labels: GraphqlLabelConnection,
}

impl From<GraphqlIssue> for LinearIssue {
    fn from(value: GraphqlIssue) -> Self {
        Self {
            id: value.id,
            identifier: value.identifier,
            title: value.title,
            description: value.description,
            url: value.url,
            created_at: value.created_at,
            updated_at: value.updated_at,
            archived_at: value.archived_at,
            started_at: value.started_at,
            completed_at: value.completed_at,
            canceled_at: value.canceled_at,
            auto_archived_at: value.auto_archived_at,
            auto_closed_at: value.auto_closed_at,
            started_triage_at: value.started_triage_at,
            triaged_at: value.triaged_at,
            snoozed_until_at: value.snoozed_until_at,
            added_to_cycle_at: value.added_to_cycle_at,
            added_to_project_at: value.added_to_project_at,
            added_to_team_at: value.added_to_team_at,
            due_date: value.due_date,
            priority: value.priority.map(|priority| LinearIssuePriority {
                value: priority,
                label: value.priority_label.unwrap_or_else(|| priority.to_string()),
            }),
            estimate: value.estimate,
            team: value.team.into(),
            state: value.state.into(),
            project: value.project.map(Into::into),
            assignee: value.assignee.map(Into::into),
            labels: value.labels.nodes.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct GraphqlTeam {
    id: String,
    key: String,
    name: String,
}

impl From<GraphqlTeam> for LinearTeam {
    fn from(value: GraphqlTeam) -> Self {
        Self {
            id: value.id,
            key: value.key,
            name: value.name,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphqlState {
    id: String,
    name: String,
    #[serde(rename = "type")]
    state_type: Option<String>,
}

impl From<GraphqlState> for LinearIssueState {
    fn from(value: GraphqlState) -> Self {
        Self {
            id: value.id,
            name: value.name,
            state_type: value.state_type,
        }
    }
}

#[derive(Debug, Deserialize)]
struct GraphqlProject {
    id: String,
    name: String,
}

impl From<GraphqlProject> for LinearProject {
    fn from(value: GraphqlProject) -> Self {
        Self {
            id: value.id,
            name: value.name,
        }
    }
}

#[derive(Debug, Deserialize)]
struct GraphqlUser {
    id: String,
    name: String,
    email: Option<String>,
}

impl From<GraphqlUser> for LinearUser {
    fn from(value: GraphqlUser) -> Self {
        Self {
            id: value.id,
            name: value.name,
            email: value.email,
        }
    }
}

#[derive(Debug, Deserialize)]
struct GraphqlLabelConnection {
    nodes: Vec<GraphqlLabel>,
}

#[derive(Debug, Deserialize)]
struct GraphqlLabel {
    id: String,
    name: String,
}

impl From<GraphqlLabel> for LinearLabel {
    fn from(value: GraphqlLabel) -> Self {
        Self {
            id: value.id,
            name: value.name,
        }
    }
}

#[derive(Debug, Deserialize)]
struct GraphqlEnvelope<T> {
    data: Option<T>,
    errors: Option<Vec<GraphqlError>>,
}

#[derive(Debug, Deserialize)]
struct GraphqlError {
    message: String,
}

fn decode_graphql_response<T>(response: Response) -> LocalityResult<T>
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
                "Linear token is invalid or revoked; reconnect Linear".to_string(),
            )),
            StatusCode::FORBIDDEN => Err(LocalityError::Guardrail(format!(
                "Linear API access is not allowed by this token: {body}"
            ))),
            StatusCode::NOT_FOUND => Err(LocalityError::RemoteNotFound(body)),
            StatusCode::TOO_MANY_REQUESTS => Err(LocalityError::Io(format!(
                "Linear API rate limited: {body}"
            ))),
            _ => Err(LocalityError::Io(format!(
                "Linear API returned HTTP {status}: {body}"
            ))),
        };
    }
    let envelope = response.json::<GraphqlEnvelope<T>>().map_err(|error| {
        LocalityError::Io(format!("Linear API response decode failed: {error}"))
    })?;
    if let Some(errors) = envelope.errors.filter(|errors| !errors.is_empty()) {
        return Err(LocalityError::Io(format!(
            "Linear GraphQL error: {}",
            errors
                .into_iter()
                .map(|error| error.message)
                .collect::<Vec<_>>()
                .join("; ")
        )));
    }
    envelope
        .data
        .ok_or_else(|| LocalityError::Io("Linear GraphQL response missing data".to_string()))
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

fn linear_network_config() -> ConnectorNetworkConfig {
    ConnectorNetworkConfig::new(
        "linear",
        DEFAULT_LINEAR_REQUESTS_PER_SECOND,
        DEFAULT_LINEAR_REQUEST_BURST,
    )
    .max_in_flight(DEFAULT_LINEAR_MAX_IN_FLIGHT)
    .request_timeout(LINEAR_HTTP_TIMEOUT)
    .retry(RetryConfig::exponential(
        DEFAULT_LINEAR_RATE_LIMIT_RETRIES,
        Duration::from_secs(1),
        Duration::from_secs(16),
    ))
}

fn linear_network_gate() -> &'static ConnectorNetworkGate {
    LINEAR_NETWORK_GATE.get_or_init(|| ConnectorNetworkGate::global(linear_network_config()))
}

fn linear_backoff(attempt: usize) -> Duration {
    linear_network_config().retry.backoff(attempt)
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

const ISSUE_LIST_QUERY: &str = r#"
query LocalityIssues($first: Int!, $after: String, $filter: IssueFilter) {
  issues(first: $first, after: $after, filter: $filter) {
    nodes {
      id
      identifier
      title
      description
      url
      createdAt
      updatedAt
      archivedAt
      startedAt
      completedAt
      canceledAt
      autoArchivedAt
      autoClosedAt
      startedTriageAt
      triagedAt
      snoozedUntilAt
      addedToCycleAt
      addedToProjectAt
      addedToTeamAt
      dueDate
      priority
      priorityLabel
      estimate
      team { id key name }
      state { id name type }
      project { id name }
      assignee { id name email }
      labels { nodes { id name } }
    }
    pageInfo { hasNextPage endCursor }
  }
}
"#;

const ISSUE_QUERY: &str = r#"
query LocalityIssue($id: String!) {
  issue(id: $id) {
    id
    identifier
    title
    description
    url
    createdAt
    updatedAt
    archivedAt
    startedAt
    completedAt
    canceledAt
    autoArchivedAt
    autoClosedAt
    startedTriageAt
    triagedAt
    snoozedUntilAt
    addedToCycleAt
    addedToProjectAt
    addedToTeamAt
    dueDate
    priority
    priorityLabel
    estimate
    team { id key name }
    state { id name type }
    project { id name }
    assignee { id name email }
    labels { nodes { id name } }
  }
}
"#;

const ISSUE_UPDATE_MUTATION: &str = r#"
mutation LocalityIssueUpdate($id: String!, $input: IssueUpdateInput!) {
  issueUpdate(id: $id, input: $input) {
    success
    issue {
      id
      identifier
      title
      description
      url
      createdAt
      updatedAt
      archivedAt
      startedAt
      completedAt
      canceledAt
      autoArchivedAt
      autoClosedAt
      startedTriageAt
      triagedAt
      snoozedUntilAt
      addedToCycleAt
      addedToProjectAt
      addedToTeamAt
      dueDate
      priority
      priorityLabel
      estimate
      team { id key name }
      state { id name type }
      project { id name }
      assignee { id name email }
      labels { nodes { id name } }
    }
  }
}
"#;
