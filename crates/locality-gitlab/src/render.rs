use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use locality_connector::ConnectorCapabilities;
use locality_core::model::CanonicalDocument;
use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};

use crate::connector::GITLAB_CONNECTOR_ID;
use crate::dto::{GitLabContent, GitLabIssue, GitLabMergeRequest, GitLabRepository};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GitLabNativeBundle {
    Repository {
        repository: GitLabRepository,
    },
    Readme {
        repository: GitLabRepository,
        content: GitLabContent,
    },
    Issue {
        repository: GitLabRepository,
        issue: GitLabIssue,
    },
    MergeRequest {
        repository: GitLabRepository,
        merge_request: GitLabMergeRequest,
    },
}

pub fn gitlab_capabilities_json() -> Result<String, serde_json::Error> {
    serde_json::to_string(&ConnectorCapabilities::read_only())
}

pub fn render_gitlab_entity(bundle: &GitLabNativeBundle) -> LocalityResult<CanonicalDocument> {
    match bundle {
        GitLabNativeBundle::Repository { repository } => render_repository(repository),
        GitLabNativeBundle::Readme {
            repository,
            content,
        } => render_readme(repository, content),
        GitLabNativeBundle::Issue { repository, issue } => render_issue(repository, issue),
        GitLabNativeBundle::MergeRequest {
            repository,
            merge_request,
        } => render_merge_request(repository, merge_request),
    }
}

pub fn remote_version_for_repository(repository: &GitLabRepository) -> String {
    format!(
        "gitlab:repo:{}:{}",
        repository.full_name,
        repository_updated_at(repository)
    )
}

pub fn readme_remote_version(repository: &GitLabRepository, content: &GitLabContent) -> String {
    format!("gitlab:readme:{}:{}", repository.full_name, content.sha)
}

pub fn remote_version_for_issue(repository: &GitLabRepository, issue: &GitLabIssue) -> String {
    format!(
        "gitlab:issue:{}:{}:{}",
        repository.full_name, issue.number, issue.updated_at
    )
}

pub fn remote_version_for_merge_request(
    repository: &GitLabRepository,
    merge_request: &GitLabMergeRequest,
) -> String {
    format!(
        "gitlab:merge:{}:{}:{}",
        repository.full_name, merge_request.number, merge_request.updated_at
    )
}

fn render_repository(repository: &GitLabRepository) -> LocalityResult<CanonicalDocument> {
    Ok(CanonicalDocument::new(
        format!(
            "loc:\n  id: {}\n  type: asset\n  connector: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\ngitlab:\n  kind: repository\n  namespace: {}\n  project: {}\n  full_name: {}\n  url: {}\n  visibility: {}\n  archived: {}\n  default_branch: {}\n  created_at: {}\n  updated_at: {}\n",
            yaml_string(&format!("gitlab:repo-summary:{}", repository.full_name)),
            GITLAB_CONNECTOR_ID,
            yaml_string(&repository.updated_at),
            yaml_string(&remote_version_for_repository(repository)),
            yaml_string(&repository.full_name),
            yaml_string(namespace_label(repository)),
            yaml_string(&repository.name),
            yaml_string(&repository.full_name),
            yaml_string(&repository.html_url),
            yaml_string(&repository.visibility),
            repository.archived,
            yaml_string(&repository.default_branch),
            yaml_string(&repository.created_at),
            yaml_string(repository_updated_at(repository)),
        ),
        repository_body(repository),
    ))
}

fn render_readme(
    repository: &GitLabRepository,
    content: &GitLabContent,
) -> LocalityResult<CanonicalDocument> {
    Ok(CanonicalDocument::new(
        format!(
            "loc:\n  id: {}\n  type: asset\n  connector: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\ngitlab:\n  kind: readme\n  namespace: {}\n  project: {}\n  full_name: {}\n  path: {}\n  sha: {}\n",
            yaml_string(&format!("gitlab:readme:{}", repository.full_name)),
            GITLAB_CONNECTOR_ID,
            yaml_string(&repository.updated_at),
            yaml_string(&readme_remote_version(repository, content)),
            yaml_string(&format!("{} README", repository.full_name)),
            yaml_string(namespace_label(repository)),
            yaml_string(&repository.name),
            yaml_string(&repository.full_name),
            yaml_string(&content.path),
            yaml_string(&content.sha),
        ),
        decode_content_body(content)?,
    ))
}

fn render_issue(
    repository: &GitLabRepository,
    issue: &GitLabIssue,
) -> LocalityResult<CanonicalDocument> {
    Ok(CanonicalDocument::new(
        format!(
            "loc:\n  id: {}\n  type: page\n  connector: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\ngitlab:\n  kind: issue\n  namespace: {}\n  project: {}\n  full_name: {}\n  number: {}\n  state: {}\n  url: {}\n  author: {}\n  created_at: {}\n  updated_at: {}\n  closed_at: {}\n  labels:\n{}\n",
            yaml_string(&format!(
                "gitlab:issue:{}:{}",
                repository.full_name, issue.number
            )),
            GITLAB_CONNECTOR_ID,
            yaml_string(&issue.updated_at),
            yaml_string(&remote_version_for_issue(repository, issue)),
            yaml_string(&issue.title),
            yaml_string(namespace_label(repository)),
            yaml_string(&repository.name),
            yaml_string(&repository.full_name),
            issue.number,
            yaml_string(&issue.state),
            yaml_string(&issue.html_url),
            optional_yaml_string(issue.user.as_ref().map(|user| user.username.as_str())),
            yaml_string(&issue.created_at),
            yaml_string(&issue.updated_at),
            optional_yaml_string(issue.closed_at.as_deref()),
            labels_yaml(&issue.labels.iter().map(String::as_str).collect::<Vec<_>>()),
        ),
        ensure_trailing_newline(issue.body.clone().unwrap_or_default()),
    ))
}

fn render_merge_request(
    repository: &GitLabRepository,
    merge_request: &GitLabMergeRequest,
) -> LocalityResult<CanonicalDocument> {
    Ok(CanonicalDocument::new(
        format!(
            "loc:\n  id: {}\n  type: page\n  connector: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\ngitlab:\n  kind: merge_request\n  namespace: {}\n  project: {}\n  full_name: {}\n  number: {}\n  state: {}\n  draft: {}\n  url: {}\n  author: {}\n  source_branch: {}\n  target_branch: {}\n  created_at: {}\n  updated_at: {}\n  closed_at: {}\n  merged_at: {}\n",
            yaml_string(&format!(
                "gitlab:merge:{}:{}",
                repository.full_name, merge_request.number
            )),
            GITLAB_CONNECTOR_ID,
            yaml_string(&merge_request.updated_at),
            yaml_string(&remote_version_for_merge_request(repository, merge_request)),
            yaml_string(&merge_request.title),
            yaml_string(namespace_label(repository)),
            yaml_string(&repository.name),
            yaml_string(&repository.full_name),
            merge_request.number,
            yaml_string(&merge_request.state),
            merge_request.draft || merge_request.work_in_progress,
            yaml_string(&merge_request.html_url),
            optional_yaml_string(
                merge_request
                    .user
                    .as_ref()
                    .map(|user| user.username.as_str())
            ),
            optional_yaml_string(merge_request.source_branch.as_deref()),
            optional_yaml_string(merge_request.target_branch.as_deref()),
            yaml_string(&merge_request.created_at),
            yaml_string(&merge_request.updated_at),
            optional_yaml_string(merge_request.closed_at.as_deref()),
            optional_yaml_string(merge_request.merged_at.as_deref()),
        ),
        ensure_trailing_newline(merge_request.body.clone().unwrap_or_default()),
    ))
}

fn repository_body(repository: &GitLabRepository) -> String {
    let mut body = format!("# {}\n\n", repository.full_name);
    if let Some(description) = repository
        .description
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        body.push_str(description);
        body.push_str("\n\n");
    }
    body.push_str(&format!("- URL: {}\n", repository.html_url));
    body.push_str(&format!(
        "- Default branch: `{}`\n",
        repository.default_branch
    ));
    body.push_str(&format!("- Visibility: {}\n", repository.visibility));
    body.push_str(&format!("- Archived: {}\n", repository.archived));
    ensure_trailing_newline(body)
}

fn namespace_label(repository: &GitLabRepository) -> &str {
    if !repository.owner.full_path.trim().is_empty() {
        &repository.owner.full_path
    } else if let Some((namespace, _)) = repository.full_name.rsplit_once('/') {
        namespace
    } else {
        ""
    }
}

fn repository_updated_at(repository: &GitLabRepository) -> &str {
    if !repository.updated_at.trim().is_empty() {
        &repository.updated_at
    } else {
        repository.updated_at_fallback.as_deref().unwrap_or("")
    }
}

fn decode_content_body(content: &GitLabContent) -> LocalityResult<String> {
    if content.encoding != "base64" {
        return Ok(ensure_trailing_newline(content.content.clone()));
    }
    let compact = content.content.replace(['\n', '\r'], "");
    let bytes = STANDARD
        .decode(compact)
        .map_err(|error| LocalityError::Io(format!("GitLab content decode failed: {error}")))?;
    let text = String::from_utf8(bytes)
        .map_err(|error| LocalityError::Io(format!("GitLab content is not UTF-8: {error}")))?;
    Ok(ensure_trailing_newline(text))
}

fn labels_yaml(labels: &[&str]) -> String {
    if labels.is_empty() {
        return "    []".to_string();
    }
    labels
        .iter()
        .map(|label| format!("    - {}", yaml_string(label)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn ensure_trailing_newline(mut value: String) -> String {
    if !value.ends_with('\n') {
        value.push('\n');
    }
    value
}

fn optional_yaml_string(value: Option<&str>) -> String {
    value
        .filter(|value| !value.trim().is_empty())
        .map(yaml_string)
        .unwrap_or_else(|| "null".to_string())
}

fn yaml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}
