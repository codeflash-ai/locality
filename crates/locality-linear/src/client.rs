use std::fmt;
use std::io::Read;
use std::sync::OnceLock;
use std::time::Duration;

use locality_connector::network::{ConnectorNetworkConfig, ConnectorNetworkGate, RetryConfig};
use locality_core::{LocalityError, LocalityResult};
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use serde::Deserialize;
use serde::de::{DeserializeOwned, Deserializer};
use serde_json::{Value, json};

use crate::dto::{
    LinearAttachment, LinearBotActor, LinearComment, LinearExternalUser, LinearIssue,
    LinearIssueContext, LinearIssueHistoryEntry, LinearIssuePage, LinearIssuePriority,
    LinearIssueState, LinearIssueUpdateInput, LinearLabel, LinearProject, LinearTeam, LinearUser,
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
    fn get_issue_context(&self, _issue_id: &str) -> LocalityResult<LinearIssueContext> {
        Err(LocalityError::Unsupported("Linear issue context"))
    }
    fn download_attachment(&self, _url: &str, _max_bytes: u64) -> LocalityResult<Vec<u8>> {
        Err(LocalityError::Unsupported("Linear attachment download"))
    }
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

    fn issue_comments(&self, issue_id: &str) -> LocalityResult<Vec<LinearComment>> {
        let mut comments: Vec<LinearComment> = Vec::new();
        let mut cursor = None;
        loop {
            let data: IssueCommentsData = self.graphql(
                ISSUE_COMMENTS_QUERY,
                json!({
                    "id": issue_id,
                    "first": PAGE_SIZE,
                    "after": cursor,
                }),
            )?;
            let issue = data
                .issue
                .ok_or_else(|| LocalityError::RemoteNotFound(issue_id.to_string()))?;
            comments.extend(issue.comments.nodes.into_iter().map(Into::into));
            if !issue.comments.page_info.has_next_page {
                break;
            }
            cursor = issue
                .comments
                .page_info
                .end_cursor
                .filter(|value| !value.is_empty());
            if cursor.is_none() {
                return Err(LocalityError::InvalidState(
                    "Linear API reported another comments page without a cursor".to_string(),
                ));
            }
        }
        comments.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(comments)
    }

    fn issue_attachments(&self, issue_id: &str) -> LocalityResult<Vec<LinearAttachment>> {
        let mut attachments: Vec<LinearAttachment> = Vec::new();
        let mut cursor = None;
        loop {
            let data: IssueAttachmentsData = self.graphql(
                ISSUE_ATTACHMENTS_QUERY,
                json!({
                    "id": issue_id,
                    "first": PAGE_SIZE,
                    "after": cursor,
                }),
            )?;
            let issue = data
                .issue
                .ok_or_else(|| LocalityError::RemoteNotFound(issue_id.to_string()))?;
            attachments.extend(issue.attachments.nodes.into_iter().map(Into::into));
            if !issue.attachments.page_info.has_next_page {
                break;
            }
            cursor = issue
                .attachments
                .page_info
                .end_cursor
                .filter(|value| !value.is_empty());
            if cursor.is_none() {
                return Err(LocalityError::InvalidState(
                    "Linear API reported another attachments page without a cursor".to_string(),
                ));
            }
        }
        attachments.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(attachments)
    }

    fn issue_history(&self, issue_id: &str) -> LocalityResult<Vec<LinearIssueHistoryEntry>> {
        let mut history: Vec<LinearIssueHistoryEntry> = Vec::new();
        let mut cursor = None;
        loop {
            let data: IssueHistoryData = self.graphql(
                ISSUE_HISTORY_QUERY,
                json!({
                    "id": issue_id,
                    "first": PAGE_SIZE,
                    "after": cursor,
                }),
            )?;
            let issue = data
                .issue
                .ok_or_else(|| LocalityError::RemoteNotFound(issue_id.to_string()))?;
            history.extend(issue.history.nodes.into_iter().map(Into::into));
            if !issue.history.page_info.has_next_page {
                break;
            }
            cursor = issue
                .history
                .page_info
                .end_cursor
                .filter(|value| !value.is_empty());
            if cursor.is_none() {
                return Err(LocalityError::InvalidState(
                    "Linear API reported another history page without a cursor".to_string(),
                ));
            }
        }
        history.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(history)
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

    fn get_issue_context(&self, issue_id: &str) -> LocalityResult<LinearIssueContext> {
        let data: IssueContextHeaderData = self.graphql(
            ISSUE_CONTEXT_HEADER_QUERY,
            json!({
                "id": issue_id,
            }),
        )?;
        let issue = data
            .issue
            .ok_or_else(|| LocalityError::RemoteNotFound(issue_id.to_string()))?;
        Ok(LinearIssueContext {
            issue_id: issue.id,
            issue_identifier: issue.identifier,
            issue_title: issue.title,
            issue_updated_at: issue.updated_at,
            branch_name: issue.branch_name,
            comments: self.issue_comments(issue_id)?,
            attachments: self.issue_attachments(issue_id)?,
            history: self.issue_history(issue_id)?,
        })
    }

    fn download_attachment(&self, url: &str, max_bytes: u64) -> LocalityResult<Vec<u8>> {
        let parsed = reqwest::Url::parse(url).map_err(|error| {
            LocalityError::Io(format!("Linear attachment URL is invalid: {error}"))
        })?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(LocalityError::Unsupported(
                "Linear attachment URL is not HTTP(S)",
            ));
        }

        let send = || {
            let mut request = self.client.get(parsed.clone());
            if should_send_linear_authorization(&parsed, &self.graphql_url) {
                request = request.header(reqwest::header::AUTHORIZATION, self.token.as_str());
            }
            request.send()
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
                        "Linear attachment download failed: {error}"
                    )));
                }
            };
            let status = response.status();
            if is_retryable_status(status) && attempt < DEFAULT_LINEAR_RATE_LIMIT_RETRIES {
                let delay = retry_after(&response).unwrap_or_else(|| linear_backoff(attempt));
                linear_network_gate().record_cooldown(delay);
                continue;
            }
            return decode_attachment_response(response, max_bytes);
        }
        Err(LocalityError::Io(
            "Linear attachment download exhausted retries".to_string(),
        ))
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
struct IssueContextHeaderData {
    issue: Option<GraphqlIssueContextHeader>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphqlIssueContextHeader {
    id: String,
    identifier: String,
    title: String,
    updated_at: String,
    branch_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssueCommentsData {
    issue: Option<GraphqlIssueComments>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphqlIssueComments {
    comments: CommentConnection,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssueAttachmentsData {
    issue: Option<GraphqlIssueAttachments>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphqlIssueAttachments {
    attachments: AttachmentConnection,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssueHistoryData {
    issue: Option<GraphqlIssueHistoryList>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphqlIssueHistoryList {
    history: IssueHistoryConnection,
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
struct CommentConnection {
    nodes: Vec<GraphqlComment>,
    page_info: PageInfo,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphqlComment {
    id: String,
    body: String,
    url: String,
    created_at: String,
    updated_at: String,
    edited_at: Option<String>,
    parent_id: Option<String>,
    resolved_at: Option<String>,
    user: Option<GraphqlUser>,
    external_user: Option<GraphqlExternalUser>,
    bot_actor: Option<GraphqlBotActor>,
}

impl From<GraphqlComment> for LinearComment {
    fn from(value: GraphqlComment) -> Self {
        Self {
            id: value.id,
            body: value.body,
            url: value.url,
            created_at: value.created_at,
            updated_at: value.updated_at,
            edited_at: value.edited_at,
            parent_id: value.parent_id,
            resolved_at: value.resolved_at,
            user: value.user.map(Into::into),
            external_user: value.external_user.map(Into::into),
            bot_actor: value.bot_actor.map(Into::into),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttachmentConnection {
    nodes: Vec<GraphqlAttachment>,
    page_info: PageInfo,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphqlAttachment {
    id: String,
    title: String,
    url: String,
    created_at: String,
    updated_at: String,
    source_type: Option<String>,
    subtitle: Option<String>,
    creator: Option<GraphqlUser>,
    external_user_creator: Option<GraphqlExternalUser>,
    #[serde(default)]
    metadata: Value,
}

impl From<GraphqlAttachment> for LinearAttachment {
    fn from(value: GraphqlAttachment) -> Self {
        Self {
            id: value.id,
            title: value.title,
            url: value.url,
            created_at: value.created_at,
            updated_at: value.updated_at,
            source_type: value.source_type,
            subtitle: value.subtitle,
            creator: value.creator.map(Into::into),
            external_user_creator: value.external_user_creator.map(Into::into),
            metadata: value.metadata,
            download: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssueHistoryConnection {
    nodes: Vec<GraphqlIssueHistory>,
    page_info: PageInfo,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphqlIssueHistory {
    id: String,
    created_at: String,
    updated_at: String,
    actor: Option<GraphqlUser>,
    bot_actor: Option<GraphqlBotActor>,
    from_state: Option<GraphqlState>,
    to_state: Option<GraphqlState>,
    from_title: Option<String>,
    to_title: Option<String>,
    from_assignee: Option<GraphqlUser>,
    to_assignee: Option<GraphqlUser>,
    from_project: Option<GraphqlProject>,
    to_project: Option<GraphqlProject>,
    from_team: Option<GraphqlTeam>,
    to_team: Option<GraphqlTeam>,
    from_due_date: Option<String>,
    to_due_date: Option<String>,
    from_estimate: Option<f64>,
    to_estimate: Option<f64>,
    from_priority: Option<f64>,
    to_priority: Option<f64>,
    updated_description: Option<bool>,
    attachment_id: Option<String>,
    attachment: Option<GraphqlAttachment>,
    #[serde(default, deserialize_with = "null_to_empty_vec")]
    added_labels: Vec<GraphqlLabel>,
    #[serde(default, deserialize_with = "null_to_empty_vec")]
    removed_labels: Vec<GraphqlLabel>,
    changes: Option<Value>,
}

impl From<GraphqlIssueHistory> for LinearIssueHistoryEntry {
    fn from(value: GraphqlIssueHistory) -> Self {
        Self {
            id: value.id,
            created_at: value.created_at,
            updated_at: value.updated_at,
            actor: value.actor.map(Into::into),
            bot_actor: value.bot_actor.map(Into::into),
            from_state: value.from_state.map(Into::into),
            to_state: value.to_state.map(Into::into),
            from_title: value.from_title,
            to_title: value.to_title,
            from_assignee: value.from_assignee.map(Into::into),
            to_assignee: value.to_assignee.map(Into::into),
            from_project: value.from_project.map(Into::into),
            to_project: value.to_project.map(Into::into),
            from_team: value.from_team.map(Into::into),
            to_team: value.to_team.map(Into::into),
            from_due_date: value.from_due_date,
            to_due_date: value.to_due_date,
            from_estimate: value.from_estimate,
            to_estimate: value.to_estimate,
            from_priority: value.from_priority,
            to_priority: value.to_priority,
            updated_description: value.updated_description,
            attachment_id: value.attachment_id,
            attachment: value.attachment.map(Into::into),
            added_labels: value.added_labels.into_iter().map(Into::into).collect(),
            removed_labels: value.removed_labels.into_iter().map(Into::into).collect(),
            changes: value.changes,
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
#[serde(rename_all = "camelCase")]
struct GraphqlExternalUser {
    id: Option<String>,
    name: Option<String>,
    email: Option<String>,
}

impl From<GraphqlExternalUser> for LinearExternalUser {
    fn from(value: GraphqlExternalUser) -> Self {
        Self {
            id: value.id,
            name: value.name,
            email: value.email,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphqlBotActor {
    id: Option<String>,
    name: Option<String>,
    #[serde(rename = "type")]
    actor_type: String,
    sub_type: Option<String>,
    user_display_name: Option<String>,
}

impl From<GraphqlBotActor> for LinearBotActor {
    fn from(value: GraphqlBotActor) -> Self {
        Self {
            id: value.id,
            name: value.name,
            actor_type: value.actor_type,
            sub_type: value.sub_type,
            user_display_name: value.user_display_name,
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

fn null_to_empty_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

fn decode_attachment_response(mut response: Response, max_bytes: u64) -> LocalityResult<Vec<u8>> {
    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .unwrap_or_else(|error| format!("<failed to read error body: {error}>"));
        return Err(LocalityError::Io(format!(
            "Linear attachment download returned HTTP {status}: {body}"
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes)
    {
        return Err(LocalityError::Guardrail(format!(
            "Linear attachment exceeds {} MB download limit",
            max_bytes / 1024 / 1024
        )));
    }

    let mut bytes = Vec::new();
    response
        .by_ref()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| {
            LocalityError::Io(format!("Linear attachment download body failed: {error}"))
        })?;
    if bytes.len() as u64 > max_bytes {
        return Err(LocalityError::Guardrail(format!(
            "Linear attachment exceeds {} MB download limit",
            max_bytes / 1024 / 1024
        )));
    }
    Ok(bytes)
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

fn should_send_linear_authorization(url: &reqwest::Url, graphql_url: &str) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    if host == "linear.app" || host.ends_with(".linear.app") {
        return true;
    }
    reqwest::Url::parse(graphql_url)
        .ok()
        .and_then(|url| url.host_str().map(ToOwned::to_owned))
        .is_some_and(|graphql_host| graphql_host == host)
}

fn ensure_reqwest_crypto_provider() {
    REQWEST_CRYPTO_PROVIDER.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn decodes_issue_context_header_comments_and_attachments() {
        let header: IssueContextHeaderData = serde_json::from_value(json!({
            "issue": {
                "id": "issue-1",
                "identifier": "ENG-1",
                "title": "Improve sync",
                "updatedAt": "2026-07-15T12:00:00Z",
                "branchName": "eng-1-improve-sync"
            }
        }))
        .expect("header");
        let issue = header.issue.expect("issue");
        assert_eq!(issue.branch_name, "eng-1-improve-sync");

        let comments: IssueCommentsData = serde_json::from_value(json!({
            "issue": {
                "comments": {
                    "nodes": [{
                        "id": "comment-1",
                        "body": "Looks good.",
                        "url": "https://linear.app/acme/issue/ENG-1#comment-comment-1",
                        "createdAt": "2026-07-15T13:00:00Z",
                        "updatedAt": "2026-07-15T13:05:00Z",
                        "editedAt": "2026-07-15T13:05:00Z",
                        "parentId": null,
                        "resolvedAt": null,
                        "user": { "id": "user-1", "name": "Ada", "email": "ada@example.com" },
                        "externalUser": null,
                        "botActor": null
                    }],
                    "pageInfo": { "hasNextPage": false, "endCursor": null }
                }
            }
        }))
        .expect("comments");
        let comment = LinearComment::from(
            comments
                .issue
                .expect("issue")
                .comments
                .nodes
                .into_iter()
                .next()
                .expect("comment"),
        );
        assert_eq!(comment.user.expect("user").name, "Ada");
        assert_eq!(comment.edited_at.as_deref(), Some("2026-07-15T13:05:00Z"));

        let attachments: IssueAttachmentsData = serde_json::from_value(json!({
            "issue": {
                "attachments": {
                    "nodes": [{
                        "id": "attach-1",
                        "title": "GitHub PR #42",
                        "url": "https://github.com/acme/app/pull/42",
                        "createdAt": "2026-07-15T14:00:00Z",
                        "updatedAt": "2026-07-15T14:10:00Z",
                        "sourceType": "github",
                        "subtitle": "Open pull request",
                        "creator": { "id": "user-1", "name": "Ada", "email": "ada@example.com" },
                        "externalUserCreator": null,
                        "metadata": {
                            "repository": "acme/app",
                            "number": 42,
                            "branch": "eng-1-improve-sync",
                            "status": "open"
                        }
                    }],
                    "pageInfo": { "hasNextPage": false, "endCursor": null }
                }
            }
        }))
        .expect("attachments");
        let attachment = LinearAttachment::from(
            attachments
                .issue
                .expect("issue")
                .attachments
                .nodes
                .into_iter()
                .next()
                .expect("attachment"),
        );
        assert_eq!(attachment.source_type.as_deref(), Some("github"));
        assert_eq!(attachment.metadata["number"], 42);
    }

    #[test]
    fn decodes_issue_history_change_fields_and_raw_changes() {
        let data: IssueHistoryData = serde_json::from_value(json!({
            "issue": {
                "history": {
                    "nodes": [{
                        "id": "history-1",
                        "createdAt": "2026-07-15T15:00:00Z",
                        "updatedAt": "2026-07-15T15:01:00Z",
                        "actor": { "id": "user-1", "name": "Ada", "email": "ada@example.com" },
                        "botActor": null,
                        "fromState": { "id": "state-1", "name": "Todo", "type": "unstarted" },
                        "toState": { "id": "state-2", "name": "Done", "type": "completed" },
                        "fromTitle": "Improve sync",
                        "toTitle": "Improve sync quickly",
                        "fromAssignee": null,
                        "toAssignee": { "id": "user-1", "name": "Ada", "email": "ada@example.com" },
                        "fromProject": null,
                        "toProject": { "id": "project-1", "name": "Launch" },
                        "fromTeam": null,
                        "toTeam": { "id": "team-1", "key": "ENG", "name": "Engineering" },
                        "fromDueDate": null,
                        "toDueDate": "2026-07-31",
                        "fromEstimate": null,
                        "toEstimate": 3,
                        "fromPriority": null,
                        "toPriority": 3,
                        "updatedDescription": true,
                        "attachmentId": "attach-1",
                        "attachment": null,
                        "addedLabels": [{ "id": "label-1", "name": "Bug" }],
                        "removedLabels": [],
                        "changes": { "description": true }
                    }],
                    "pageInfo": { "hasNextPage": false, "endCursor": null }
                }
            }
        }))
        .expect("history");

        let entry = LinearIssueHistoryEntry::from(
            data.issue
                .expect("issue")
                .history
                .nodes
                .into_iter()
                .next()
                .expect("history entry"),
        );

        assert_eq!(entry.actor.expect("actor").name, "Ada");
        assert_eq!(entry.to_state.expect("state").name, "Done");
        assert_eq!(entry.to_project.expect("project").name, "Launch");
        assert_eq!(entry.to_due_date.as_deref(), Some("2026-07-31"));
        assert_eq!(entry.to_estimate, Some(3.0));
        assert_eq!(entry.to_priority, Some(3.0));
        assert_eq!(entry.added_labels[0].name, "Bug");
        assert_eq!(entry.changes.expect("changes")["description"], true);
    }

    #[test]
    fn attachment_download_authorization_is_limited_to_linear_hosts() {
        assert!(should_send_linear_authorization(
            &reqwest::Url::parse("https://uploads.linear.app/spec.pdf").expect("url"),
            DEFAULT_LINEAR_GRAPHQL_URL,
        ));
        assert!(should_send_linear_authorization(
            &reqwest::Url::parse("https://linear.internal/download/spec.pdf").expect("url"),
            "https://linear.internal/graphql",
        ));
        assert!(!should_send_linear_authorization(
            &reqwest::Url::parse("https://github.com/acme/app/pull/42").expect("url"),
            DEFAULT_LINEAR_GRAPHQL_URL,
        ));
    }
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

const ISSUE_CONTEXT_HEADER_QUERY: &str = r#"
query LocalityIssueContextHeader($id: String!) {
  issue(id: $id) {
    id
    identifier
    title
    updatedAt
    branchName
  }
}
"#;

const ISSUE_COMMENTS_QUERY: &str = r#"
query LocalityIssueComments($id: String!, $first: Int!, $after: String) {
  issue(id: $id) {
    comments(first: $first, after: $after, includeArchived: true) {
      nodes {
        id
        body
        url
        createdAt
        updatedAt
        editedAt
        parentId
        resolvedAt
        user { id name email }
        externalUser { id name email }
        botActor { id name type subType userDisplayName }
      }
      pageInfo { hasNextPage endCursor }
    }
  }
}
"#;

const ISSUE_ATTACHMENTS_QUERY: &str = r#"
query LocalityIssueAttachments($id: String!, $first: Int!, $after: String) {
  issue(id: $id) {
    attachments(first: $first, after: $after, includeArchived: true) {
      nodes {
        id
        title
        url
        createdAt
        updatedAt
        sourceType
        subtitle
        creator { id name email }
        externalUserCreator { id name email }
        metadata
      }
      pageInfo { hasNextPage endCursor }
    }
  }
}
"#;

const ISSUE_HISTORY_QUERY: &str = r#"
query LocalityIssueHistory($id: String!, $first: Int!, $after: String) {
  issue(id: $id) {
    history(first: $first, after: $after, includeArchived: true) {
      nodes {
        id
        createdAt
        updatedAt
        actor { id name email }
        botActor { id name type subType userDisplayName }
        fromState { id name type }
        toState { id name type }
        fromTitle
        toTitle
        fromAssignee { id name email }
        toAssignee { id name email }
        fromProject { id name }
        toProject { id name }
        fromTeam { id key name }
        toTeam { id key name }
        fromDueDate
        toDueDate
        fromEstimate
        toEstimate
        fromPriority
        toPriority
        updatedDescription
        attachmentId
        attachment {
          id
          title
          url
          createdAt
          updatedAt
          sourceType
          subtitle
          creator { id name email }
          externalUserCreator { id name email }
          metadata
        }
        addedLabels { id name }
        removedLabels { id name }
        changes
      }
      pageInfo { hasNextPage endCursor }
    }
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
