use std::sync::Arc;

use locality_connector::{ChildContainer, Connector, FetchRequest, ListChildrenRequest};
use locality_core::model::{MountId, RemoteId};
use locality_core::{LocalityError, LocalityResult};
use locality_github::{
    GitHubApi, GitHubConfig, GitHubConnector, GitHubContent, GitHubIssue, GitHubPullRef,
    GitHubPullRequest, GitHubRepository, GitHubRepositoryOwner, GitHubUser,
};

#[test]
fn github_connector_projects_repository_context_as_files() {
    let connector = test_connector();
    let mount_id = MountId::new("github-main");

    let root = connector
        .list_children(ListChildrenRequest {
            mount_id: mount_id.clone(),
            container: ChildContainer::Root,
            parent_path: "".into(),
        })
        .expect("root children");
    assert_eq!(
        root.entries[0].path,
        std::path::PathBuf::from("Repositories")
    );

    let repos = connector
        .list_children(ListChildrenRequest {
            mount_id: mount_id.clone(),
            container: ChildContainer::DirectoryChildren(RemoteId::new("github:repositories")),
            parent_path: "Repositories".into(),
        })
        .expect("repo children");
    assert_eq!(
        repos.entries[0].path,
        std::path::PathBuf::from("Repositories/codeflash-ai/locality")
    );

    let repo_children = connector
        .list_children(ListChildrenRequest {
            mount_id,
            container: ChildContainer::DirectoryChildren(RemoteId::new(
                "github:repo:codeflash-ai/locality",
            )),
            parent_path: "Repositories/codeflash-ai/locality".into(),
        })
        .expect("repo metadata children");
    let paths = repo_children
        .entries
        .iter()
        .map(|entry| entry.path.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec![
            "Repositories/codeflash-ai/locality/repository.md",
            "Repositories/codeflash-ai/locality/Issues",
            "Repositories/codeflash-ai/locality/Pull Requests",
            "Repositories/codeflash-ai/locality/README.md",
        ]
    );
}

#[test]
fn github_connector_hydrates_issue_markdown() {
    let connector = test_connector();
    let native = connector
        .fetch(FetchRequest {
            remote_id: RemoteId::new("github:issue:codeflash-ai/locality:42"),
        })
        .expect("issue native");
    let document = connector.render(&native).expect("issue render");

    assert!(document.frontmatter.contains("connector: github"));
    assert!(document.frontmatter.contains("kind: issue"));
    assert!(document.frontmatter.contains("number: 42"));
    assert_eq!(document.body, "Fixes the sync bug.\n");
}

#[test]
fn github_connector_is_read_only() {
    let connector = test_connector();

    assert!(connector.supported_push_operations().is_empty());
    assert!(matches!(
        connector.parse(&locality_core::model::CanonicalDocument::new("", "")),
        Err(LocalityError::Unsupported(message)) if message == "GitHub writes"
    ));
}

fn test_connector() -> GitHubConnector {
    GitHubConnector::with_api(
        GitHubConfig::new("test-token"),
        Arc::new(FakeGitHubApi::default()),
    )
}

#[derive(Clone, Debug)]
struct FakeGitHubApi {
    repo: GitHubRepository,
    issue: GitHubIssue,
    pull: GitHubPullRequest,
    readme: GitHubContent,
}

impl Default for FakeGitHubApi {
    fn default() -> Self {
        let repo = GitHubRepository {
            id: 1,
            name: "locality".to_string(),
            full_name: "codeflash-ai/locality".to_string(),
            owner: GitHubRepositoryOwner {
                login: "codeflash-ai".to_string(),
            },
            description: Some("Filesystem for agents".to_string()),
            html_url: "https://github.com/codeflash-ai/locality".to_string(),
            private: false,
            fork: false,
            archived: false,
            default_branch: "main".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-07-23T00:00:00Z".to_string(),
            pushed_at: Some("2026-07-23T01:00:00Z".to_string()),
        };
        let user = GitHubUser {
            login: "saga4".to_string(),
            html_url: Some("https://github.com/saga4".to_string()),
        };
        Self {
            repo: repo.clone(),
            issue: GitHubIssue {
                id: 42,
                number: 42,
                title: "Fix sync".to_string(),
                body: Some("Fixes the sync bug.".to_string()),
                state: "open".to_string(),
                html_url: "https://github.com/codeflash-ai/locality/issues/42".to_string(),
                created_at: "2026-07-22T00:00:00Z".to_string(),
                updated_at: "2026-07-23T00:00:00Z".to_string(),
                closed_at: None,
                user: Some(user.clone()),
                labels: Vec::new(),
                pull_request: None,
            },
            pull: GitHubPullRequest {
                id: 7,
                number: 7,
                title: "Improve connector".to_string(),
                body: Some("Adds connector coverage.".to_string()),
                state: "open".to_string(),
                html_url: "https://github.com/codeflash-ai/locality/pull/7".to_string(),
                created_at: "2026-07-22T00:00:00Z".to_string(),
                updated_at: "2026-07-23T00:00:00Z".to_string(),
                closed_at: None,
                merged_at: None,
                draft: false,
                user: Some(user),
                base: GitHubPullRef {
                    label: Some("codeflash-ai:main".to_string()),
                    branch_ref: Some("main".to_string()),
                    sha: Some("base".to_string()),
                    repo: Some(repo.clone()),
                },
                head: GitHubPullRef {
                    label: Some("codeflash-ai:branch".to_string()),
                    branch_ref: Some("branch".to_string()),
                    sha: Some("head".to_string()),
                    repo: Some(repo),
                },
            },
            readme: GitHubContent {
                name: "README.md".to_string(),
                path: "README.md".to_string(),
                sha: "readme-sha".to_string(),
                html_url: Some(
                    "https://github.com/codeflash-ai/locality/blob/main/README.md".to_string(),
                ),
                download_url: None,
                content: "IyBMb2NhbGl0eQo=".to_string(),
                encoding: "base64".to_string(),
            },
        }
    }
}

impl GitHubApi for FakeGitHubApi {
    fn current_user(&self) -> LocalityResult<GitHubUser> {
        Ok(GitHubUser {
            login: "saga4".to_string(),
            html_url: None,
        })
    }

    fn list_repositories(&self) -> LocalityResult<Vec<GitHubRepository>> {
        Ok(vec![self.repo.clone()])
    }

    fn get_repository(&self, owner: &str, repo: &str) -> LocalityResult<GitHubRepository> {
        if owner == "codeflash-ai" && repo == "locality" {
            return Ok(self.repo.clone());
        }
        Err(LocalityError::RemoteNotFound(format!("{owner}/{repo}")))
    }

    fn get_readme(&self, owner: &str, repo: &str) -> LocalityResult<Option<GitHubContent>> {
        if owner == "codeflash-ai" && repo == "locality" {
            return Ok(Some(self.readme.clone()));
        }
        Ok(None)
    }

    fn list_issues(&self, _owner: &str, _repo: &str) -> LocalityResult<Vec<GitHubIssue>> {
        Ok(vec![self.issue.clone()])
    }

    fn get_issue(&self, _owner: &str, _repo: &str, number: u64) -> LocalityResult<GitHubIssue> {
        if number == self.issue.number {
            return Ok(self.issue.clone());
        }
        Err(LocalityError::RemoteNotFound(number.to_string()))
    }

    fn list_pull_requests(
        &self,
        _owner: &str,
        _repo: &str,
    ) -> LocalityResult<Vec<GitHubPullRequest>> {
        Ok(vec![self.pull.clone()])
    }

    fn get_pull_request(
        &self,
        _owner: &str,
        _repo: &str,
        number: u64,
    ) -> LocalityResult<GitHubPullRequest> {
        if number == self.pull.number {
            return Ok(self.pull.clone());
        }
        Err(LocalityError::RemoteNotFound(number.to_string()))
    }
}
