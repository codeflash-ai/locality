use locality_core::model::CanonicalDocument;
use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::connector::LINEAR_CONNECTOR_ID;
use crate::dto::{
    LinearAttachment, LinearComment, LinearIssue, LinearIssueContext, LinearIssueContextKind,
    LinearIssueHistoryEntry, LinearIssuePriority, LinearPullRequestLink, LinearUser,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinearNativeBundle {
    pub issue: LinearIssue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<LinearNativeContextBundle>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinearNativeContextBundle {
    pub kind: LinearIssueContextKind,
    pub context: LinearIssueContext,
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

pub fn context_remote_version(
    context: &LinearIssueContext,
    kind: LinearIssueContextKind,
) -> String {
    format!(
        "linear-context:{}:{}:{}",
        context.issue_id,
        kind.as_str(),
        context.issue_updated_at
    )
}

pub fn linear_context_remote_id(issue_id: &str, kind: LinearIssueContextKind) -> String {
    format!("linear-context:{issue_id}:{}", kind.as_str())
}

pub fn render_linear_issue_context(
    context: &LinearIssueContext,
    kind: LinearIssueContextKind,
) -> LocalityResult<CanonicalDocument> {
    validate_context(context)?;
    let body = match kind {
        LinearIssueContextKind::Comments => render_comments_body(context),
        LinearIssueContextKind::Attachments => render_attachments_body(context),
        LinearIssueContextKind::PullRequests => render_pull_requests_body(context),
        LinearIssueContextKind::History => render_history_body(context),
    };
    Ok(CanonicalDocument::new(
        context_frontmatter(context, kind),
        body,
    ))
}

pub fn derive_linear_pull_requests(context: &LinearIssueContext) -> Vec<LinearPullRequestLink> {
    let mut links = context
        .attachments
        .iter()
        .filter_map(pull_request_link_from_attachment)
        .collect::<Vec<_>>();
    links.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.attachment_id.cmp(&right.attachment_id))
    });
    links
}

pub fn reference(label: &str, id: &str) -> String {
    format!("{} <{}>", label.trim(), id.trim())
}

fn validate_context(context: &LinearIssueContext) -> LocalityResult<()> {
    if context.issue_id.trim().is_empty() {
        return Err(LocalityError::InvalidState(
            "Linear issue context is missing its issue id".to_string(),
        ));
    }
    if context.issue_identifier.trim().is_empty() {
        return Err(LocalityError::InvalidState(
            "Linear issue context is missing its issue identifier".to_string(),
        ));
    }
    Ok(())
}

fn context_frontmatter(context: &LinearIssueContext, kind: LinearIssueContextKind) -> String {
    format!(
        "loc:\n  id: {}\n  type: asset\n  connector: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\nlinear:\n  issue_id: {}\n  issue_identifier: {}\n  context: {}\n  read_only: true\n",
        yaml_string(&linear_context_remote_id(&context.issue_id, kind)),
        LINEAR_CONNECTOR_ID,
        yaml_string(&context.issue_updated_at),
        yaml_string(&context.issue_updated_at),
        yaml_string(&format!("{} {}", context.issue_identifier, kind.title())),
        safe_plain_scalar(&context.issue_id),
        safe_plain_scalar(&context.issue_identifier),
        kind.as_str(),
    )
}

fn render_comments_body(context: &LinearIssueContext) -> String {
    let mut comments = context.comments.clone();
    comments.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    let mut output = String::from("# Comments\n\n");
    if comments.is_empty() {
        output.push_str("_No comments found._\n");
        return output;
    }
    for comment in comments {
        output.push_str(&format!(
            "## {} - {}\n\n",
            comment.created_at,
            comment_author(&comment)
        ));
        output.push_str(&format!("- id: `{}`\n", comment.id));
        output.push_str(&format!("- url: {}\n", comment.url));
        output.push_str(&format!("- updated_at: {}\n", comment.updated_at));
        if let Some(edited_at) = &comment.edited_at {
            output.push_str(&format!("- edited_at: {edited_at}\n"));
        }
        if let Some(parent_id) = &comment.parent_id {
            output.push_str(&format!("- parent_id: `{parent_id}`\n"));
        }
        if let Some(resolved_at) = &comment.resolved_at {
            output.push_str(&format!("- resolved_at: {resolved_at}\n"));
        }
        output.push('\n');
        output.push_str(&ensure_trailing_newline(escape_locality_directive_lines(
            &comment.body,
        )));
        output.push('\n');
    }
    output
}

fn render_attachments_body(context: &LinearIssueContext) -> String {
    let mut attachments = context.attachments.clone();
    attachments.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    let mut output = String::from("# Attachments\n\n");
    if attachments.is_empty() {
        output.push_str("_No attachments found._\n");
        return output;
    }
    for attachment in attachments {
        output.push_str(&format!("## {}\n\n", attachment.title));
        output.push_str(&format!("- id: `{}`\n", attachment.id));
        output.push_str(&format!("- url: {}\n", attachment.url));
        if let Some(source_type) = &attachment.source_type {
            output.push_str(&format!("- source_type: {source_type}\n"));
        }
        output.push_str(&format!("- created_at: {}\n", attachment.created_at));
        output.push_str(&format!("- updated_at: {}\n", attachment.updated_at));
        if let Some(creator) = &attachment.creator {
            output.push_str(&format!("- creator: {}\n", user_label(creator)));
        } else if let Some(external) = &attachment.external_user_creator {
            output.push_str(&format!(
                "- creator: {}\n",
                external
                    .name
                    .as_deref()
                    .or(external.email.as_deref())
                    .unwrap_or("External user")
            ));
        }
        if let Some(subtitle) = &attachment.subtitle {
            output.push_str(&format!("- subtitle: {subtitle}\n"));
        }
        if let Some(download) = &attachment.download {
            output.push_str(&format!("- download_status: {}\n", download.status));
            if let Some(path) = &download.local_path {
                output.push_str(&format!("- local_path: {path}\n"));
            }
            if let Some(error) = &download.error {
                output.push_str(&format!("- download_error: {}\n", one_line(error)));
            }
        }
        output.push_str("- metadata:\n\n");
        output.push_str("```json\n");
        output.push_str(&stable_json(&attachment.metadata));
        output.push_str("\n```\n\n");
    }
    output
}

fn render_pull_requests_body(context: &LinearIssueContext) -> String {
    let links = derive_linear_pull_requests(context);
    let mut output = String::from("# Pull Requests\n\n");
    if context.branch_name.trim().is_empty() {
        output.push_str("Suggested branch: null\n\n");
    } else {
        output.push_str(&format!("Suggested branch: `{}`\n\n", context.branch_name));
    }
    if links.is_empty() {
        output.push_str("_No pull request attachments found._\n");
        return output;
    }
    for link in links {
        output.push_str(&format!("## {}\n\n", link.title));
        output.push_str(&format!("- attachment_id: `{}`\n", link.attachment_id));
        output.push_str(&format!("- url: {}\n", link.url));
        if let Some(source_type) = &link.source_type {
            output.push_str(&format!("- source_type: {source_type}\n"));
        }
        if let Some(repository) = &link.repository {
            output.push_str(&format!("- repository: {repository}\n"));
        }
        if let Some(number) = &link.number {
            output.push_str(&format!("- number: {number}\n"));
        }
        if let Some(branch) = &link.branch {
            output.push_str(&format!("- branch: {branch}\n"));
        }
        if let Some(status) = &link.status {
            output.push_str(&format!("- status: {status}\n"));
        }
        output.push_str(&format!("- created_at: {}\n", link.created_at));
        output.push_str(&format!("- updated_at: {}\n", link.updated_at));
        output.push_str("- metadata:\n\n");
        output.push_str("```json\n");
        output.push_str(&stable_json(&link.metadata));
        output.push_str("\n```\n\n");
    }
    output
}

fn render_history_body(context: &LinearIssueContext) -> String {
    let mut entries = context.history.clone();
    entries.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    let mut output = String::from("# History\n\n");
    if entries.is_empty() {
        output.push_str("_No history entries found._\n");
        return output;
    }
    for entry in entries {
        output.push_str(&format!(
            "## {} - {}\n\n",
            entry.created_at,
            history_actor(&entry)
        ));
        output.push_str(&format!("- id: `{}`\n", entry.id));
        output.push_str(&format!("- updated_at: {}\n", entry.updated_at));
        render_history_change(
            &mut output,
            "status",
            entry
                .from_state
                .as_ref()
                .map(|state| reference(&state.name, &state.id)),
            entry
                .to_state
                .as_ref()
                .map(|state| reference(&state.name, &state.id)),
        );
        render_history_change(&mut output, "title", entry.from_title, entry.to_title);
        render_history_change(
            &mut output,
            "assignee",
            entry.from_assignee.as_ref().map(user_label),
            entry.to_assignee.as_ref().map(user_label),
        );
        render_history_change(
            &mut output,
            "project",
            entry
                .from_project
                .as_ref()
                .map(|project| reference(&project.name, &project.id)),
            entry
                .to_project
                .as_ref()
                .map(|project| reference(&project.name, &project.id)),
        );
        render_history_change(
            &mut output,
            "team",
            entry
                .from_team
                .as_ref()
                .map(|team| reference(&team.name, &team.id)),
            entry
                .to_team
                .as_ref()
                .map(|team| reference(&team.name, &team.id)),
        );
        render_history_change(
            &mut output,
            "due_date",
            entry.from_due_date,
            entry.to_due_date,
        );
        render_history_change(
            &mut output,
            "estimate",
            entry.from_estimate.map(number_scalar),
            entry.to_estimate.map(number_scalar),
        );
        render_history_change(
            &mut output,
            "priority",
            entry.from_priority.map(number_scalar),
            entry.to_priority.map(number_scalar),
        );
        if entry.updated_description == Some(true) {
            output.push_str("- description_updated: true\n");
        }
        if !entry.added_labels.is_empty() {
            output.push_str(&format!(
                "- labels_added: {}\n",
                entry
                    .added_labels
                    .iter()
                    .map(|label| reference(&label.name, &label.id))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !entry.removed_labels.is_empty() {
            output.push_str(&format!(
                "- labels_removed: {}\n",
                entry
                    .removed_labels
                    .iter()
                    .map(|label| reference(&label.name, &label.id))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if let Some(attachment) = &entry.attachment {
            output.push_str(&format!(
                "- attachment: {} <{}>\n",
                attachment.title, attachment.id
            ));
            output.push_str(&format!("- attachment_url: {}\n", attachment.url));
        } else if let Some(attachment_id) = &entry.attachment_id {
            output.push_str(&format!("- attachment_id: `{attachment_id}`\n"));
        }
        if let Some(changes) = &entry.changes {
            output.push_str("- changes:\n\n");
            output.push_str("```json\n");
            output.push_str(&stable_json(changes));
            output.push_str("\n```\n");
        }
        output.push('\n');
    }
    output
}

fn render_history_change(
    output: &mut String,
    label: &str,
    from: Option<String>,
    to: Option<String>,
) {
    if from.is_none() && to.is_none() {
        return;
    }
    output.push_str(&format!(
        "- {label}: {} -> {}\n",
        from.unwrap_or_else(|| "null".to_string()),
        to.unwrap_or_else(|| "null".to_string())
    ));
}

fn pull_request_link_from_attachment(
    attachment: &LinearAttachment,
) -> Option<LinearPullRequestLink> {
    if !is_pull_request_attachment(attachment) {
        return None;
    }
    Some(LinearPullRequestLink {
        attachment_id: attachment.id.clone(),
        title: attachment.title.clone(),
        url: attachment.url.clone(),
        source_type: attachment.source_type.clone(),
        status: metadata_string(
            &attachment.metadata,
            &["status", "state", "pullRequestStatus"],
        )
        .or_else(|| merged_status(&attachment.metadata)),
        repository: metadata_string(
            &attachment.metadata,
            &["repository", "repo", "repositoryName", "repoName"],
        )
        .or_else(|| {
            nested_metadata_string(
                &attachment.metadata,
                "repository",
                &["nameWithOwner", "fullName", "name"],
            )
        }),
        number: metadata_string(
            &attachment.metadata,
            &[
                "number",
                "pullRequestNumber",
                "prNumber",
                "mergeRequestNumber",
            ],
        )
        .or_else(|| nested_metadata_string(&attachment.metadata, "pullRequest", &["number"])),
        branch: metadata_string(
            &attachment.metadata,
            &[
                "branch",
                "branchName",
                "headRefName",
                "sourceBranch",
                "sourceBranchName",
            ],
        )
        .or_else(|| {
            nested_metadata_string(
                &attachment.metadata,
                "pullRequest",
                &["branch", "headRefName"],
            )
        }),
        created_at: attachment.created_at.clone(),
        updated_at: attachment.updated_at.clone(),
        metadata: attachment.metadata.clone(),
    })
}

fn is_pull_request_attachment(attachment: &LinearAttachment) -> bool {
    let source_type = attachment
        .source_type
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let pr_shaped = attachment.url.contains("/pull/")
        || attachment.url.contains("/merge_requests/")
        || has_metadata_key(
            &attachment.metadata,
            &[
                "pullRequest",
                "pullRequestNumber",
                "prNumber",
                "mergeRequestNumber",
                "headRefName",
            ],
        );
    let source_like = source_type.contains("github")
        || source_type.contains("gitlab")
        || attachment.url.contains("github.com")
        || attachment.url.contains("gitlab.com");
    source_like && pr_shaped
}

fn metadata_string(value: &Value, keys: &[&str]) -> Option<String> {
    let object = value.as_object()?;
    for key in keys {
        if let Some(value) = object.get(*key).and_then(value_to_string) {
            return Some(value);
        }
    }
    None
}

fn nested_metadata_string(value: &Value, parent: &str, keys: &[&str]) -> Option<String> {
    let nested = value.as_object()?.get(parent)?;
    metadata_string(nested, keys)
}

fn has_metadata_key(value: &Value, keys: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|object| keys.iter().any(|key| object.contains_key(*key)))
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) if !value.trim().is_empty() => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn merged_status(value: &Value) -> Option<String> {
    value
        .as_object()
        .and_then(|object| object.get("merged"))
        .and_then(Value::as_bool)
        .and_then(|merged| merged.then(|| "merged".to_string()))
}

fn comment_author(comment: &LinearComment) -> String {
    comment
        .user
        .as_ref()
        .map(user_label)
        .or_else(|| {
            comment
                .external_user
                .as_ref()
                .and_then(|user| user.name.clone().or(user.email.clone()))
        })
        .or_else(|| {
            comment
                .bot_actor
                .as_ref()
                .and_then(|bot| bot.name.clone().or(bot.user_display_name.clone()))
        })
        .unwrap_or_else(|| "Unknown".to_string())
}

fn history_actor(entry: &LinearIssueHistoryEntry) -> String {
    entry
        .actor
        .as_ref()
        .map(user_label)
        .or_else(|| {
            entry
                .bot_actor
                .as_ref()
                .and_then(|bot| bot.name.clone().or(bot.user_display_name.clone()))
        })
        .unwrap_or_else(|| "Unknown".to_string())
}

fn user_label(user: &LinearUser) -> String {
    reference(&user.name, &user.id)
}

fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn stable_json(value: &Value) -> String {
    serde_json::to_string_pretty(&sorted_json_value(value)).unwrap_or_else(|_| "null".to_string())
}

fn sorted_json_value(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(sorted_json_value).collect()),
        Value::Object(map) => {
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            let mut sorted = serde_json::Map::new();
            for (key, value) in entries {
                sorted.insert(key.clone(), sorted_json_value(value));
            }
            Value::Object(sorted)
        }
        _ => value.clone(),
    }
}

fn escape_locality_directive_lines(value: &str) -> String {
    let mut output = String::new();
    for line in value.lines() {
        if line.trim_start().starts_with("::loc") {
            output.push('\\');
        }
        output.push_str(line);
        output.push('\n');
    }
    if value.is_empty() {
        return output;
    }
    if !value.ends_with('\n') {
        return output.trim_end_matches('\n').to_string();
    }
    output
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
