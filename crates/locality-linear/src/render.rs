use locality_core::model::CanonicalDocument;
use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};

use crate::connector::LINEAR_CONNECTOR_ID;
use crate::dto::{LinearIssue, LinearIssuePriority};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinearNativeBundle {
    pub issue: LinearIssue,
}

pub fn render_linear_issue(issue: &LinearIssue) -> LocalityResult<CanonicalDocument> {
    if issue.id.trim().is_empty() {
        return Err(LocalityError::InvalidState(
            "Linear issue is missing its id".to_string(),
        ));
    }
    if issue.identifier.trim().is_empty() {
        return Err(LocalityError::InvalidState(
            "Linear issue is missing its identifier".to_string(),
        ));
    }

    let mut frontmatter = format!(
        "loc:\n  id: {}\n  type: page\n  connector: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\nidentifier: {}\nurl: {}\ncreated_at: {}\nupdated_at: {}\narchived_at: {}\nstarted_at: {}\ncompleted_at: {}\ncanceled_at: {}\nauto_archived_at: {}\nauto_closed_at: {}\nstarted_triage_at: {}\ntriaged_at: {}\nsnoozed_until_at: {}\nadded_to_cycle_at: {}\nadded_to_project_at: {}\nadded_to_team_at: {}\ndue_date: {}\nStatus: {}\nTeam: {}\n",
        safe_plain_scalar(&issue.id),
        LINEAR_CONNECTOR_ID,
        yaml_string(&issue.updated_at),
        yaml_string(&issue.updated_at),
        yaml_string(&issue.title),
        safe_plain_scalar(&issue.identifier),
        yaml_string(&issue.url),
        yaml_string(&issue.created_at),
        yaml_string(&issue.updated_at),
        optional_yaml_string(issue.archived_at.as_deref()),
        optional_yaml_string(issue.started_at.as_deref()),
        optional_yaml_string(issue.completed_at.as_deref()),
        optional_yaml_string(issue.canceled_at.as_deref()),
        optional_yaml_string(issue.auto_archived_at.as_deref()),
        optional_yaml_string(issue.auto_closed_at.as_deref()),
        optional_yaml_string(issue.started_triage_at.as_deref()),
        optional_yaml_string(issue.triaged_at.as_deref()),
        optional_yaml_string(issue.snoozed_until_at.as_deref()),
        optional_yaml_string(issue.added_to_cycle_at.as_deref()),
        optional_yaml_string(issue.added_to_project_at.as_deref()),
        optional_yaml_string(issue.added_to_team_at.as_deref()),
        optional_yaml_string(issue.due_date.as_deref()),
        yaml_string(&reference(&issue.state.name, &issue.state.id)),
        yaml_string(&reference(&issue.team.name, &issue.team.id)),
    );
    if let Some(project) = &issue.project {
        frontmatter.push_str(&format!(
            "Project: {}\n",
            yaml_string(&reference(&project.name, &project.id))
        ));
    } else {
        frontmatter.push_str("Project: null\n");
    }
    if let Some(assignee) = &issue.assignee {
        frontmatter.push_str(&format!(
            "Assignee: {}\n",
            yaml_string(&reference(&assignee.name, &assignee.id))
        ));
    } else {
        frontmatter.push_str("Assignee: null\n");
    }
    if let Some(priority) = &issue.priority {
        frontmatter.push_str(&format!("Priority: {}\n", priority_scalar(priority)));
    } else {
        frontmatter.push_str("Priority: null\n");
    }
    if let Some(estimate) = issue.estimate {
        frontmatter.push_str(&format!("Estimate: {}\n", number_scalar(estimate)));
    } else {
        frontmatter.push_str("Estimate: null\n");
    }
    frontmatter.push_str("Labels:\n");
    if issue.labels.is_empty() {
        frontmatter.push_str("  []\n");
    } else {
        for label in &issue.labels {
            frontmatter.push_str(&format!(
                "  - {}\n",
                yaml_string(&reference(&label.name, &label.id))
            ));
        }
    }

    Ok(CanonicalDocument::new(
        frontmatter,
        ensure_trailing_newline(issue.description.clone().unwrap_or_default()),
    ))
}

pub fn remote_version(issue: &LinearIssue) -> String {
    format!("linear:{}:{}", issue.id, issue.updated_at)
}

pub fn reference(label: &str, id: &str) -> String {
    format!("{} <{}>", label.trim(), id.trim())
}

fn priority_scalar(priority: &LinearIssuePriority) -> String {
    safe_plain_scalar(&priority.label)
}

fn number_scalar(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

fn safe_plain_scalar(value: &str) -> String {
    let value = value.trim();
    if !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        value.to_string()
    } else {
        yaml_string(value)
    }
}

fn yaml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn optional_yaml_string(value: Option<&str>) -> String {
    value.map(yaml_string).unwrap_or_else(|| "null".to_string())
}

fn ensure_trailing_newline(mut value: String) -> String {
    if !value.is_empty() && !value.ends_with('\n') {
        value.push('\n');
    }
    value
}
