use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, ChildContainer,
    Connector, ConnectorCapabilities, ConnectorExecutionPolicy, ConnectorKind, EnumerateRequest,
    FetchRequest, ListChildrenRequest, ListChildrenResult, NativeEntity, ObserveRequest,
    ParsedEntity,
};
use locality_core::freshness::{RemoteObservation, RemoteVersion};
use locality_core::model::{
    CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, TreeEntry,
};
use locality_core::planner::PushOperationKind;
use locality_core::search::{RAW_SEARCH_METADATA_KEY, SearchMetadata};
use locality_core::{LocalityError, LocalityResult};

use crate::client::{GitHubApi, HttpGitHubApiClient};
use crate::dto::{GitHubIssue, GitHubPullRequest, GitHubRepository};
use crate::render::{
    GitHubNativeBundle, remote_version_for_issue, remote_version_for_pull_request,
    remote_version_for_repository, render_github_entity,
};

pub const GITHUB_CONNECTOR_ID: &str = "github";

const REPOSITORIES_DIRECTORY_NAME: &str = "Repositories";
const ISSUES_DIRECTORY_NAME: &str = "Issues";
const PULL_REQUESTS_DIRECTORY_NAME: &str = "Pull Requests";
const REPOSITORY_SUMMARY_FILENAME: &str = "repository.md";
const README_FILENAME: &str = "README.md";

const REPOSITORIES_ROOT_REMOTE_ID: &str = "github:repositories";
const REPO_REMOTE_ID_PREFIX: &str = "github:repo:";
const REPO_SUMMARY_REMOTE_ID_PREFIX: &str = "github:repo-summary:";
const README_REMOTE_ID_PREFIX: &str = "github:readme:";
const ISSUES_REMOTE_ID_PREFIX: &str = "github:issues:";
const ISSUE_REMOTE_ID_PREFIX: &str = "github:issue:";
const PULLS_REMOTE_ID_PREFIX: &str = "github:pulls:";
const PULL_REMOTE_ID_PREFIX: &str = "github:pull:";

#[derive(Clone, PartialEq, Eq)]
pub struct GitHubConfig {
    pub token: String,
    pub execution_policy: ConnectorExecutionPolicy,
}

impl GitHubConfig {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            execution_policy: ConnectorExecutionPolicy::Inline,
        }
    }

    pub fn with_execution_policy(mut self, execution_policy: ConnectorExecutionPolicy) -> Self {
        self.execution_policy = execution_policy;
        self
    }
}

impl fmt::Debug for GitHubConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitHubConfig")
            .field("token", &"<redacted>")
            .field("execution_policy", &self.execution_policy)
            .finish()
    }
}

#[derive(Clone)]
pub struct GitHubConnector {
    config: GitHubConfig,
    api: Arc<dyn GitHubApi>,
}

impl fmt::Debug for GitHubConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitHubConnector")
            .field("token", &"<redacted>")
            .finish()
    }
}

impl GitHubConnector {
    pub fn new(config: GitHubConfig) -> Self {
        let api = Arc::new(HttpGitHubApiClient::new(config.token.clone()));
        Self::with_api(config, api)
    }

    pub fn with_api(config: GitHubConfig, api: Arc<dyn GitHubApi>) -> Self {
        Self { config, api }
    }

    pub fn config(&self) -> &GitHubConfig {
        &self.config
    }

    fn repository_for_full_name(&self, full_name: &str) -> LocalityResult<GitHubRepository> {
        let (owner, repo) = split_full_name(full_name)?;
        self.api.get_repository(owner, repo)
    }
}

impl Connector for GitHubConnector {
    fn with_execution_policy(&self, policy: ConnectorExecutionPolicy) -> Self {
        Self::new(self.config.clone().with_execution_policy(policy))
    }

    fn kind(&self) -> ConnectorKind {
        ConnectorKind(GITHUB_CONNECTOR_ID)
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities::read_only()
    }

    fn supported_push_operations(&self) -> BTreeSet<PushOperationKind> {
        BTreeSet::new()
    }

    fn enumerate(&self, request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        let repos = self.api.list_repositories()?;
        Ok(entries_for_repositories(
            &request.mount_id,
            Path::new(""),
            repos,
        ))
    }

    fn list_children(&self, request: ListChildrenRequest) -> LocalityResult<ListChildrenResult> {
        let entries = match request.container {
            ChildContainer::Root => {
                vec![repositories_root_entry(
                    &request.mount_id,
                    &request.parent_path,
                )]
            }
            ChildContainer::DirectoryChildren(remote_id)
                if remote_id.as_str() == REPOSITORIES_ROOT_REMOTE_ID =>
            {
                self.api
                    .list_repositories()?
                    .into_iter()
                    .map(|repo| repository_entry(&request.mount_id, &request.parent_path, &repo))
                    .collect()
            }
            ChildContainer::DirectoryChildren(remote_id) => list_github_directory_children(
                self,
                &request.mount_id,
                &request.parent_path,
                &remote_id,
            )?,
            _ => Vec::new(),
        };
        Ok(ListChildrenResult::complete(entries))
    }

    fn observe(&self, request: ObserveRequest) -> LocalityResult<RemoteObservation> {
        let remote_id = request.remote_id.as_str();
        if let Some(full_name) = remote_id.strip_prefix(REPO_SUMMARY_REMOTE_ID_PREFIX) {
            let repo = self.repository_for_full_name(full_name)?;
            return Ok(observation_from_entry(
                repository_summary_entry(&request.mount_id, &repo_parent_path(&repo), &repo),
                Some(RemoteId::new(repo_remote_id(&repo.full_name))),
                Some(github_repository_metadata_json(&repo)),
            ));
        }
        if let Some(full_name) = remote_id.strip_prefix(README_REMOTE_ID_PREFIX) {
            let repo = self.repository_for_full_name(full_name)?;
            let Some(readme) = self.api.get_readme(&repo.owner.login, &repo.name)? else {
                return Err(LocalityError::RemoteNotFound(remote_id.to_string()));
            };
            let entry = readme_entry(
                &request.mount_id,
                &repo_parent_path(&repo),
                &repo,
                &readme.sha,
            );
            return Ok(observation_from_entry(
                entry,
                Some(RemoteId::new(repo_remote_id(&repo.full_name))),
                None,
            ));
        }
        if let Some((full_name, number)) =
            parse_numbered_remote_id(remote_id, ISSUE_REMOTE_ID_PREFIX)
        {
            let repo = self.repository_for_full_name(full_name)?;
            let issue = self.api.get_issue(&repo.owner.login, &repo.name, number)?;
            return Ok(observation_from_entry(
                issue_entry(&request.mount_id, &repo, &issue),
                Some(RemoteId::new(issues_remote_id(&repo.full_name))),
                Some(github_issue_metadata_json(&repo, &issue)),
            ));
        }
        if let Some((full_name, number)) =
            parse_numbered_remote_id(remote_id, PULL_REMOTE_ID_PREFIX)
        {
            let repo = self.repository_for_full_name(full_name)?;
            let pull = self
                .api
                .get_pull_request(&repo.owner.login, &repo.name, number)?;
            return Ok(observation_from_entry(
                pull_request_entry(&request.mount_id, &repo, &pull),
                Some(RemoteId::new(pulls_remote_id(&repo.full_name))),
                Some(github_pull_request_metadata_json(&repo, &pull)),
            ));
        }
        Err(LocalityError::Unsupported("GitHub directory observation"))
    }

    fn fetch(&self, request: FetchRequest) -> LocalityResult<NativeEntity> {
        let remote_id = request.remote_id.as_str();
        let bundle = if let Some(full_name) = remote_id.strip_prefix(REPO_SUMMARY_REMOTE_ID_PREFIX)
        {
            GitHubNativeBundle::Repository {
                repository: self.repository_for_full_name(full_name)?,
            }
        } else if let Some(full_name) = remote_id.strip_prefix(README_REMOTE_ID_PREFIX) {
            let repository = self.repository_for_full_name(full_name)?;
            let content = self
                .api
                .get_readme(&repository.owner.login, &repository.name)?
                .ok_or_else(|| LocalityError::RemoteNotFound(remote_id.to_string()))?;
            GitHubNativeBundle::Readme {
                repository,
                content,
            }
        } else if let Some((full_name, number)) =
            parse_numbered_remote_id(remote_id, ISSUE_REMOTE_ID_PREFIX)
        {
            let repository = self.repository_for_full_name(full_name)?;
            let issue = self
                .api
                .get_issue(&repository.owner.login, &repository.name, number)?;
            GitHubNativeBundle::Issue { repository, issue }
        } else if let Some((full_name, number)) =
            parse_numbered_remote_id(remote_id, PULL_REMOTE_ID_PREFIX)
        {
            let repository = self.repository_for_full_name(full_name)?;
            let pull_request =
                self.api
                    .get_pull_request(&repository.owner.login, &repository.name, number)?;
            GitHubNativeBundle::PullRequest {
                repository,
                pull_request,
            }
        } else {
            return Err(LocalityError::Unsupported("GitHub directory hydration"));
        };
        let raw = serde_json::to_vec(&bundle)
            .map_err(|error| LocalityError::Io(format!("GitHub native encode failed: {error}")))?;
        Ok(NativeEntity {
            remote_id: request.remote_id,
            kind: "github_entity".to_string(),
            raw,
        })
    }

    fn render(&self, entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        let bundle = serde_json::from_slice::<GitHubNativeBundle>(&entity.raw)
            .map_err(|error| LocalityError::Io(format!("GitHub native decode failed: {error}")))?;
        render_github_entity(&bundle)
    }

    fn parse(&self, _document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
        Err(LocalityError::Unsupported("GitHub writes"))
    }

    fn check_concurrency(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
        Err(LocalityError::Unsupported("GitHub writes"))
    }

    fn apply(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        Err(LocalityError::Unsupported("GitHub writes"))
    }

    fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
        Err(LocalityError::Unsupported("GitHub undo"))
    }
}

fn entries_for_repositories(
    mount_id: &MountId,
    parent: &Path,
    repos: Vec<GitHubRepository>,
) -> Vec<TreeEntry> {
    let mut entries = vec![repositories_root_entry(mount_id, parent)];
    for repo in repos {
        let repo_parent = parent.join(REPOSITORIES_DIRECTORY_NAME);
        let repo_path = repo_parent.join(repo_directory_name(&repo.owner.login, &repo.name));
        entries.push(repository_entry(mount_id, &repo_parent, &repo));
        entries.extend(repository_child_entries(mount_id, &repo_path, &repo, None));
    }
    entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.remote_id.cmp(&right.remote_id))
    });
    entries
}

fn list_github_directory_children(
    connector: &GitHubConnector,
    mount_id: &MountId,
    parent_path: &Path,
    remote_id: &RemoteId,
) -> LocalityResult<Vec<TreeEntry>> {
    if let Some(full_name) = remote_id.as_str().strip_prefix(REPO_REMOTE_ID_PREFIX) {
        let repo = connector.repository_for_full_name(full_name)?;
        let readme_sha = connector
            .api
            .get_readme(&repo.owner.login, &repo.name)?
            .map(|content| content.sha);
        return Ok(repository_child_entries(
            mount_id,
            parent_path,
            &repo,
            readme_sha.as_deref(),
        ));
    }
    if let Some(full_name) = remote_id.as_str().strip_prefix(ISSUES_REMOTE_ID_PREFIX) {
        let repo = connector.repository_for_full_name(full_name)?;
        return connector
            .api
            .list_issues(&repo.owner.login, &repo.name)
            .map(|issues| {
                issues
                    .into_iter()
                    .map(|issue| issue_child_entry(mount_id, parent_path, &repo, &issue))
                    .collect()
            });
    }
    if let Some(full_name) = remote_id.as_str().strip_prefix(PULLS_REMOTE_ID_PREFIX) {
        let repo = connector.repository_for_full_name(full_name)?;
        return connector
            .api
            .list_pull_requests(&repo.owner.login, &repo.name)
            .map(|pulls| {
                pulls
                    .into_iter()
                    .map(|pull| pull_request_child_entry(mount_id, parent_path, &repo, &pull))
                    .collect()
            });
    }
    Ok(Vec::new())
}

fn repositories_root_entry(mount_id: &MountId, parent: &Path) -> TreeEntry {
    directory_entry(
        mount_id,
        RemoteId::new(REPOSITORIES_ROOT_REMOTE_ID),
        REPOSITORIES_DIRECTORY_NAME,
        parent.join(REPOSITORIES_DIRECTORY_NAME),
    )
}

fn repository_entry(mount_id: &MountId, parent: &Path, repo: &GitHubRepository) -> TreeEntry {
    directory_entry(
        mount_id,
        RemoteId::new(repo_remote_id(&repo.full_name)),
        repo.full_name.clone(),
        parent.join(repo_directory_name(&repo.owner.login, &repo.name)),
    )
}

fn repository_child_entries(
    mount_id: &MountId,
    repo_path: &Path,
    repo: &GitHubRepository,
    readme_sha: Option<&str>,
) -> Vec<TreeEntry> {
    let mut entries = vec![
        repository_summary_entry(mount_id, repo_path, repo),
        directory_entry(
            mount_id,
            RemoteId::new(issues_remote_id(&repo.full_name)),
            ISSUES_DIRECTORY_NAME,
            repo_path.join(ISSUES_DIRECTORY_NAME),
        ),
        directory_entry(
            mount_id,
            RemoteId::new(pulls_remote_id(&repo.full_name)),
            PULL_REQUESTS_DIRECTORY_NAME,
            repo_path.join(PULL_REQUESTS_DIRECTORY_NAME),
        ),
    ];
    if let Some(readme_sha) = readme_sha {
        entries.push(readme_entry(mount_id, repo_path, repo, readme_sha));
    }
    entries
}

fn repository_summary_entry(
    mount_id: &MountId,
    repo_path: &Path,
    repo: &GitHubRepository,
) -> TreeEntry {
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: RemoteId::new(repo_summary_remote_id(&repo.full_name)),
        kind: EntityKind::Asset,
        title: format!("{} repository", repo.full_name),
        path: repo_path.join(REPOSITORY_SUMMARY_FILENAME),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some(remote_version_for_repository(repo)),
        stub_frontmatter: None,
    }
}

fn readme_entry(
    mount_id: &MountId,
    repo_path: &Path,
    repo: &GitHubRepository,
    sha: &str,
) -> TreeEntry {
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: RemoteId::new(readme_remote_id(&repo.full_name)),
        kind: EntityKind::Asset,
        title: format!("{} README", repo.full_name),
        path: repo_path.join(README_FILENAME),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some(format!("github:readme:{}:{}", repo.full_name, sha)),
        stub_frontmatter: None,
    }
}

fn issue_entry(mount_id: &MountId, repo: &GitHubRepository, issue: &GitHubIssue) -> TreeEntry {
    issue_child_entry(
        mount_id,
        &repo_parent_path(repo).join(ISSUES_DIRECTORY_NAME),
        repo,
        issue,
    )
}

fn issue_child_entry(
    mount_id: &MountId,
    parent: &Path,
    repo: &GitHubRepository,
    issue: &GitHubIssue,
) -> TreeEntry {
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: RemoteId::new(issue_remote_id(&repo.full_name, issue.number)),
        kind: EntityKind::Page,
        title: issue.title.clone(),
        path: parent
            .join(numbered_directory_name(issue.number, &issue.title))
            .join("page.md"),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some(remote_version_for_issue(repo, issue)),
        stub_frontmatter: None,
    }
}

fn pull_request_entry(
    mount_id: &MountId,
    repo: &GitHubRepository,
    pull_request: &GitHubPullRequest,
) -> TreeEntry {
    pull_request_child_entry(
        mount_id,
        &repo_parent_path(repo).join(PULL_REQUESTS_DIRECTORY_NAME),
        repo,
        pull_request,
    )
}

fn pull_request_child_entry(
    mount_id: &MountId,
    parent: &Path,
    repo: &GitHubRepository,
    pull_request: &GitHubPullRequest,
) -> TreeEntry {
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: RemoteId::new(pull_remote_id(&repo.full_name, pull_request.number)),
        kind: EntityKind::Page,
        title: pull_request.title.clone(),
        path: parent
            .join(numbered_directory_name(
                pull_request.number,
                &pull_request.title,
            ))
            .join("page.md"),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some(remote_version_for_pull_request(repo, pull_request)),
        stub_frontmatter: None,
    }
}

fn directory_entry(
    mount_id: &MountId,
    remote_id: RemoteId,
    title: impl Into<String>,
    path: impl Into<PathBuf>,
) -> TreeEntry {
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id,
        kind: EntityKind::Directory,
        title: title.into(),
        path: path.into(),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: None,
        stub_frontmatter: None,
    }
}

fn observation_from_entry(
    entry: TreeEntry,
    parent: Option<RemoteId>,
    raw_metadata_json: Option<String>,
) -> RemoteObservation {
    let mut observation = RemoteObservation::new(
        entry.mount_id,
        entry.remote_id,
        entry.kind,
        entry.title,
        entry.path,
    );
    if let Some(parent) = parent {
        observation = observation.with_parent(parent);
    }
    if let Some(version) = entry.remote_edited_at {
        observation = observation.with_remote_version(RemoteVersion::new(version));
    }
    if let Some(raw_metadata_json) = raw_metadata_json {
        observation = observation.with_raw_metadata_json(raw_metadata_json);
    }
    observation
}

fn github_repository_metadata_json(repo: &GitHubRepository) -> String {
    let mut value = serde_json::to_value(repo).unwrap_or_else(|_| serde_json::json!({}));
    if let serde_json::Value::Object(object) = &mut value {
        let metadata = SearchMetadata {
            metadata_text: vec![
                repo.full_name.clone(),
                repo.owner.login.clone(),
                repo.name.clone(),
                repo.default_branch.clone(),
            ],
            aliases: vec![repo.full_name.clone()],
            source_url: Some(repo.html_url.clone()),
        };
        if let Ok(metadata_value) = serde_json::to_value(metadata) {
            object.insert(RAW_SEARCH_METADATA_KEY.to_string(), metadata_value);
        }
    }
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

fn github_issue_metadata_json(repo: &GitHubRepository, issue: &GitHubIssue) -> String {
    let mut value = serde_json::to_value(issue).unwrap_or_else(|_| serde_json::json!({}));
    if let serde_json::Value::Object(object) = &mut value {
        let mut metadata_text = vec![
            repo.full_name.clone(),
            issue.number.to_string(),
            issue.state.clone(),
        ];
        metadata_text.extend(issue.labels.iter().map(|label| label.name.clone()));
        let metadata = SearchMetadata {
            metadata_text,
            aliases: vec![format!("{}#{}", repo.full_name, issue.number)],
            source_url: Some(issue.html_url.clone()),
        };
        if let Ok(metadata_value) = serde_json::to_value(metadata) {
            object.insert(RAW_SEARCH_METADATA_KEY.to_string(), metadata_value);
        }
    }
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

fn github_pull_request_metadata_json(repo: &GitHubRepository, pull: &GitHubPullRequest) -> String {
    let mut value = serde_json::to_value(pull).unwrap_or_else(|_| serde_json::json!({}));
    if let serde_json::Value::Object(object) = &mut value {
        let metadata = SearchMetadata {
            metadata_text: vec![
                repo.full_name.clone(),
                pull.number.to_string(),
                pull.state.clone(),
                pull.base.label.clone().unwrap_or_default(),
                pull.head.label.clone().unwrap_or_default(),
            ],
            aliases: vec![format!("{}#{}", repo.full_name, pull.number)],
            source_url: Some(pull.html_url.clone()),
        };
        if let Ok(metadata_value) = serde_json::to_value(metadata) {
            object.insert(RAW_SEARCH_METADATA_KEY.to_string(), metadata_value);
        }
    }
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

fn repo_parent_path(repo: &GitHubRepository) -> PathBuf {
    PathBuf::from(REPOSITORIES_DIRECTORY_NAME)
        .join(repo_directory_name(&repo.owner.login, &repo.name))
}

fn repo_directory_name(owner: &str, repo: &str) -> PathBuf {
    PathBuf::from(safe_filename(owner, 120)).join(safe_filename(repo, 120))
}

fn numbered_directory_name(number: u64, title: &str) -> String {
    let prefix = format!("#{number}");
    if title.trim().is_empty() {
        return prefix;
    }
    let title_limit = 140usize.saturating_sub(prefix.len() + 1);
    let title = safe_filename(title, title_limit);
    if title.is_empty() {
        prefix
    } else {
        format!("{prefix} {title}")
    }
}

fn safe_filename(value: &str, byte_limit: usize) -> String {
    let mut name = String::new();
    let mut pending_separator = false;
    for character in value.chars() {
        if character.is_control()
            || matches!(
                character,
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
            )
        {
            pending_separator = true;
            continue;
        }
        if character.is_whitespace() {
            pending_separator = true;
            continue;
        }
        let separator = if pending_separator && !name.is_empty() {
            " "
        } else {
            ""
        };
        if !name.is_empty() && name.len() + separator.len() + character.len_utf8() > byte_limit {
            break;
        }
        name.push_str(separator);
        name.push(character);
        pending_separator = false;
    }
    let name = name.trim_matches([' ', '.', '-']);
    if name.is_empty() {
        "Untitled".to_string()
    } else {
        name.to_string()
    }
}

fn split_full_name(full_name: &str) -> LocalityResult<(&str, &str)> {
    let (owner, repo) = full_name.split_once('/').ok_or_else(|| {
        LocalityError::InvalidState(format!("invalid GitHub repository name `{full_name}`"))
    })?;
    if owner.is_empty() || repo.is_empty() {
        return Err(LocalityError::InvalidState(format!(
            "invalid GitHub repository name `{full_name}`"
        )));
    }
    Ok((owner, repo))
}

fn parse_numbered_remote_id<'a>(remote_id: &'a str, prefix: &str) -> Option<(&'a str, u64)> {
    let value = remote_id.strip_prefix(prefix)?;
    let (full_name, number) = value.rsplit_once(':')?;
    let number = number.parse().ok()?;
    (!full_name.is_empty()).then_some((full_name, number))
}

fn repo_remote_id(full_name: &str) -> String {
    format!("{REPO_REMOTE_ID_PREFIX}{full_name}")
}

fn repo_summary_remote_id(full_name: &str) -> String {
    format!("{REPO_SUMMARY_REMOTE_ID_PREFIX}{full_name}")
}

fn readme_remote_id(full_name: &str) -> String {
    format!("{README_REMOTE_ID_PREFIX}{full_name}")
}

fn issues_remote_id(full_name: &str) -> String {
    format!("{ISSUES_REMOTE_ID_PREFIX}{full_name}")
}

fn issue_remote_id(full_name: &str, number: u64) -> String {
    format!("{ISSUE_REMOTE_ID_PREFIX}{full_name}:{number}")
}

fn pulls_remote_id(full_name: &str) -> String {
    format!("{PULLS_REMOTE_ID_PREFIX}{full_name}")
}

fn pull_remote_id(full_name: &str, number: u64) -> String {
    format!("{PULL_REMOTE_ID_PREFIX}{full_name}:{number}")
}
