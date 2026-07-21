use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinearIssuePage {
    pub issues: Vec<LinearIssue>,
    pub has_next_page: bool,
    pub end_cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinearIssue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub description: Option<String>,
    pub url: String,
    pub created_at: String,
    pub updated_at: String,
    pub archived_at: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub canceled_at: Option<String>,
    pub auto_archived_at: Option<String>,
    pub auto_closed_at: Option<String>,
    pub started_triage_at: Option<String>,
    pub triaged_at: Option<String>,
    pub snoozed_until_at: Option<String>,
    pub added_to_cycle_at: Option<String>,
    pub added_to_project_at: Option<String>,
    pub added_to_team_at: Option<String>,
    pub due_date: Option<String>,
    pub priority: Option<LinearIssuePriority>,
    pub estimate: Option<f64>,
    pub team: LinearTeam,
    pub state: LinearIssueState,
    pub project: Option<LinearProject>,
    pub assignee: Option<LinearUser>,
    pub labels: Vec<LinearLabel>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinearIssueContext {
    pub issue_id: String,
    pub issue_identifier: String,
    pub issue_title: String,
    pub issue_updated_at: String,
    pub branch_name: String,
    pub comments: Vec<LinearComment>,
    pub attachments: Vec<LinearAttachment>,
    pub history: Vec<LinearIssueHistoryEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LinearIssueContextKind {
    Comments,
    Attachments,
    PullRequests,
    History,
}

impl LinearIssueContextKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Comments => "comments",
            Self::Attachments => "attachments",
            Self::PullRequests => "pull-requests",
            Self::History => "history",
        }
    }

    pub fn filename(self) -> &'static str {
        match self {
            Self::Comments => "comments.md",
            Self::Attachments => "attachments.md",
            Self::PullRequests => "pull-requests.md",
            Self::History => "history.md",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Comments => "Comments",
            Self::Attachments => "Attachments",
            Self::PullRequests => "Pull requests",
            Self::History => "History",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinearComment {
    pub id: String,
    pub body: String,
    pub url: String,
    pub created_at: String,
    pub updated_at: String,
    pub edited_at: Option<String>,
    pub parent_id: Option<String>,
    pub resolved_at: Option<String>,
    pub user: Option<LinearUser>,
    pub external_user: Option<LinearExternalUser>,
    pub bot_actor: Option<LinearBotActor>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinearAttachment {
    pub id: String,
    pub title: String,
    pub url: String,
    pub created_at: String,
    pub updated_at: String,
    pub source_type: Option<String>,
    pub subtitle: Option<String>,
    pub creator: Option<LinearUser>,
    pub external_user_creator: Option<LinearExternalUser>,
    #[serde(default)]
    pub metadata: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download: Option<LinearAttachmentDownload>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinearAttachmentDownload {
    pub status: String,
    pub local_path: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinearPullRequestLink {
    pub attachment_id: String,
    pub title: String,
    pub url: String,
    pub source_type: Option<String>,
    pub status: Option<String>,
    pub repository: Option<String>,
    pub number: Option<String>,
    pub branch: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinearIssueHistoryEntry {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub actor: Option<LinearUser>,
    pub bot_actor: Option<LinearBotActor>,
    pub from_state: Option<LinearIssueState>,
    pub to_state: Option<LinearIssueState>,
    pub from_title: Option<String>,
    pub to_title: Option<String>,
    pub from_assignee: Option<LinearUser>,
    pub to_assignee: Option<LinearUser>,
    pub from_project: Option<LinearProject>,
    pub to_project: Option<LinearProject>,
    pub from_team: Option<LinearTeam>,
    pub to_team: Option<LinearTeam>,
    pub from_due_date: Option<String>,
    pub to_due_date: Option<String>,
    pub from_estimate: Option<f64>,
    pub to_estimate: Option<f64>,
    pub from_priority: Option<f64>,
    pub to_priority: Option<f64>,
    pub updated_description: Option<bool>,
    pub attachment_id: Option<String>,
    pub attachment: Option<LinearAttachment>,
    pub added_labels: Vec<LinearLabel>,
    pub removed_labels: Vec<LinearLabel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinearIssuePriority {
    pub value: i64,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinearTeam {
    pub id: String,
    pub key: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinearIssueState {
    pub id: String,
    pub name: String,
    pub state_type: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinearProject {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinearUser {
    pub id: String,
    pub name: String,
    pub email: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinearLabel {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinearExternalUser {
    pub id: Option<String>,
    pub name: Option<String>,
    pub email: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinearBotActor {
    pub id: Option<String>,
    pub name: Option<String>,
    pub actor_type: String,
    pub sub_type: Option<String>,
    pub user_display_name: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinearIssueUpdateInput {
    pub issue_id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub team_id: Option<String>,
    pub state_id: Option<String>,
    pub project_id: Option<Option<String>>,
    pub assignee_id: Option<Option<String>>,
}
