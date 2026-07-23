use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use locality_connector::ConnectorCapabilities;
use locality_core::model::CanonicalDocument;
use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};

use crate::connector::GITHUB_CONNECTOR_ID;
use crate::dto::{GitHubContent, GitHubIssue, GitHubPullRequest, GitHubRepository};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GitHubNativeBundle {
    Repository {
        repository: GitHubRepository,
    },
    Readme {
        repository: GitHubRepository,
        content: GitHubContent,
    },
    Issue {
        repository: GitHubRepository,
        issue: GitHubIssue,
    },
    PullRequest {
        repository: GitHubRepository,
        pull_request: GitHubPullRequest,
    },
}

pub fn github_capabilities_json() -> Result<String, serde_json::Error> {
    serde_json::to_string(&ConnectorCapabilities::read_only())
}

pub fn render_github_entity(bundle: &GitHubNativeBundle) -> LocalityResult<CanonicalDocument> {
    match bundle {
        GitHubNativeBundle::Repository { repository } => render_repository(repository),
        GitHubNativeBundle::Readme {
            repository,
            content,
        } => render_readme(repository, content),
        GitHubNativeBundle::Issue { repository, issue } => render_issue(repository, issue),
        GitHubNativeBundle::PullRequest {
            repository,
            pull_request,
        } => render_pull_request(repository, pull_request),
    }
}

pub fn remote_version_for_repository(repository: &GitHubRepository) -> String {
    format!(
        "github:repo:{}:{}",
        repository.full_name,
        repository
            .pushed_at
            .as_deref()
            .unwrap_or(repository.updated_at.as_str())
    )
}

pub fn readme_remote_version(repository: &GitHubRepository, content: &GitHubContent) -> String {
    format!("github:readme:{}:{}", repository.full_name, content.sha)
}

pub fn remote_version_for_issue(repository: &GitHubRepository, issue: &GitHubIssue) -> String {
    format!(
        "github:issue:{}:{}:{}",
        repository.full_name, issue.number, issue.updated_at
    )
}

pub fn remote_version_for_pull_request(
    repository: &GitHubRepository,
    pull_request: &GitHubPullRequest,
) -> String {
    format!(
        "github:pull:{}:{}:{}",
        repository.full_name, pull_request.number, pull_request.updated_at
    )
}

fn render_repository(repository: &GitHubRepository) -> LocalityResult<CanonicalDocument> {
    Ok(CanonicalDocument::new(
        format!(
            "loc:\n  id: {}\n  type: asset\n  connector: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\ngithub:\n  kind: repository\n  owner: {}\n  repo: {}\n  full_name: {}\n  url: {}\n  private: {}\n  fork: {}\n  archived: {}\n  default_branch: {}\n  created_at: {}\n  updated_at: {}\n  pushed_at: {}\n",
            yaml_string(&format!("github:repo-summary:{}", repository.full_name)),
            GITHUB_CONNECTOR_ID,
            yaml_string(&repository.updated_at),
            yaml_string(&remote_version_for_repository(repository)),
            yaml_string(&repository.full_name),
            yaml_string(&repository.owner.login),
            yaml_string(&repository.name),
            yaml_string(&repository.full_name),
            yaml_string(&repository.html_url),
            repository.private,
            repository.fork,
            repository.archived,
            yaml_string(&repository.default_branch),
            yaml_string(&repository.created_at),
            yaml_string(&repository.updated_at),
            optional_yaml_string(repository.pushed_at.as_deref()),
        ),
        repository_body(repository),
    ))
}

fn render_readme(
    repository: &GitHubRepository,
    content: &GitHubContent,
) -> LocalityResult<CanonicalDocument> {
    Ok(CanonicalDocument::new(
        format!(
            "loc:\n  id: {}\n  type: asset\n  connector: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\ngithub:\n  kind: readme\n  owner: {}\n  repo: {}\n  full_name: {}\n  path: {}\n  sha: {}\n  url: {}\n",
            yaml_string(&format!("github:readme:{}", repository.full_name)),
            GITHUB_CONNECTOR_ID,
            yaml_string(&repository.updated_at),
            yaml_string(&readme_remote_version(repository, content)),
            yaml_string(&format!("{} README", repository.full_name)),
            yaml_string(&repository.owner.login),
            yaml_string(&repository.name),
            yaml_string(&repository.full_name),
            yaml_string(&content.path),
            yaml_string(&content.sha),
            optional_yaml_string(content.html_url.as_deref()),
        ),
        decode_content_body(content)?,
    ))
}

fn render_issue(
    repository: &GitHubRepository,
    issue: &GitHubIssue,
) -> LocalityResult<CanonicalDocument> {
    Ok(CanonicalDocument::new(
        format!(
            "loc:\n  id: {}\n  type: page\n  connector: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\ngithub:\n  kind: issue\n  owner: {}\n  repo: {}\n  full_name: {}\n  number: {}\n  state: {}\n  url: {}\n  author: {}\n  created_at: {}\n  updated_at: {}\n  closed_at: {}\n  labels:\n{}\n",
            yaml_string(&format!(
                "github:issue:{}:{}",
                repository.full_name, issue.number
            )),
            GITHUB_CONNECTOR_ID,
            yaml_string(&issue.updated_at),
            yaml_string(&remote_version_for_issue(repository, issue)),
            yaml_string(&issue.title),
            yaml_string(&repository.owner.login),
            yaml_string(&repository.name),
            yaml_string(&repository.full_name),
            issue.number,
            yaml_string(&issue.state),
            yaml_string(&issue.html_url),
            optional_yaml_string(issue.user.as_ref().map(|user| user.login.as_str())),
            yaml_string(&issue.created_at),
            yaml_string(&issue.updated_at),
            optional_yaml_string(issue.closed_at.as_deref()),
            labels_yaml(
                &issue
                    .labels
                    .iter()
                    .map(|label| label.name.as_str())
                    .collect::<Vec<_>>()
            ),
        ),
        ensure_trailing_newline(issue.body.clone().unwrap_or_default()),
    ))
}

fn render_pull_request(
    repository: &GitHubRepository,
    pull_request: &GitHubPullRequest,
) -> LocalityResult<CanonicalDocument> {
    Ok(CanonicalDocument::new(
        format!(
            "loc:\n  id: {}\n  type: page\n  connector: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\ngithub:\n  kind: pull_request\n  owner: {}\n  repo: {}\n  full_name: {}\n  number: {}\n  state: {}\n  draft: {}\n  url: {}\n  author: {}\n  base: {}\n  head: {}\n  created_at: {}\n  updated_at: {}\n  closed_at: {}\n  merged_at: {}\n",
            yaml_string(&format!(
                "github:pull:{}:{}",
                repository.full_name, pull_request.number
            )),
            GITHUB_CONNECTOR_ID,
            yaml_string(&pull_request.updated_at),
            yaml_string(&remote_version_for_pull_request(repository, pull_request)),
            yaml_string(&pull_request.title),
            yaml_string(&repository.owner.login),
            yaml_string(&repository.name),
            yaml_string(&repository.full_name),
            pull_request.number,
            yaml_string(&pull_request.state),
            pull_request.draft,
            yaml_string(&pull_request.html_url),
            optional_yaml_string(pull_request.user.as_ref().map(|user| user.login.as_str())),
            optional_yaml_string(pull_request.base.label.as_deref()),
            optional_yaml_string(pull_request.head.label.as_deref()),
            yaml_string(&pull_request.created_at),
            yaml_string(&pull_request.updated_at),
            optional_yaml_string(pull_request.closed_at.as_deref()),
            optional_yaml_string(pull_request.merged_at.as_deref()),
        ),
        ensure_trailing_newline(pull_request.body.clone().unwrap_or_default()),
    ))
}

fn repository_body(repository: &GitHubRepository) -> String {
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
    body.push_str(&format!("- Private: {}\n", repository.private));
    body.push_str(&format!("- Archived: {}\n", repository.archived));
    ensure_trailing_newline(body)
}

fn decode_content_body(content: &GitHubContent) -> LocalityResult<String> {
    if content.encoding != "base64" {
        return Ok(ensure_trailing_newline(content.content.clone()));
    }
    let compact = content.content.replace(['\n', '\r'], "");
    let bytes = STANDARD
        .decode(compact)
        .map_err(|error| LocalityError::Io(format!("GitHub content decode failed: {error}")))?;
    let text = String::from_utf8(bytes)
        .map_err(|error| LocalityError::Io(format!("GitHub content is not UTF-8: {error}")))?;
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
