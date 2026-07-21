use serde::{Deserialize, Serialize};

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
