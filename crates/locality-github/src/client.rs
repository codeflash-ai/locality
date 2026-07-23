use std::fmt;
use std::sync::OnceLock;
use std::time::Duration;

use locality_core::{LocalityError, LocalityResult};
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use serde::de::DeserializeOwned;

use crate::dto::{GitHubContent, GitHubIssue, GitHubPullRequest, GitHubRepository, GitHubUser};

pub const DEFAULT_GITHUB_API_BASE_URL: &str = "https://api.github.com";
const GITHUB_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const PAGE_SIZE: u32 = 100;
const MAX_PAGES: u32 = 20;

static REQWEST_CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();

pub trait GitHubApi: fmt::Debug + Send + Sync {
    fn current_user(&self) -> LocalityResult<GitHubUser>;
    fn list_repositories(&self) -> LocalityResult<Vec<GitHubRepository>>;
    fn get_repository(&self, owner: &str, repo: &str) -> LocalityResult<GitHubRepository>;
    fn get_readme(&self, owner: &str, repo: &str) -> LocalityResult<Option<GitHubContent>>;
    fn list_issues(&self, owner: &str, repo: &str) -> LocalityResult<Vec<GitHubIssue>>;
    fn get_issue(&self, owner: &str, repo: &str, number: u64) -> LocalityResult<GitHubIssue>;
    fn list_pull_requests(&self, owner: &str, repo: &str)
    -> LocalityResult<Vec<GitHubPullRequest>>;
    fn get_pull_request(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> LocalityResult<GitHubPullRequest>;
}

#[derive(Clone)]
pub struct HttpGitHubApiClient {
    token: String,
    api_base_url: String,
    client: Client,
}

impl fmt::Debug for HttpGitHubApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpGitHubApiClient")
            .field("token", &"<redacted>")
            .field("api_base_url", &self.api_base_url)
            .finish_non_exhaustive()
    }
}

impl HttpGitHubApiClient {
    pub fn new(token: impl Into<String>) -> Self {
        Self::with_api_base_url(token, DEFAULT_GITHUB_API_BASE_URL)
    }

    pub fn with_api_base_url(token: impl Into<String>, api_base_url: impl Into<String>) -> Self {
        ensure_reqwest_crypto_provider();
        let client = Client::builder()
            .timeout(GITHUB_HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            token: token.into(),
            api_base_url: api_base_url.into().trim_end_matches('/').to_string(),
            client,
        }
    }

    fn get<T>(&self, path: &str, query: &[(&str, String)]) -> LocalityResult<T>
    where
        T: DeserializeOwned,
    {
        let response = self
            .request(path, query)
            .send()
            .map_err(|error| LocalityError::Io(format!("GitHub API request failed: {error}")))?;
        decode_response(response)
    }

    fn get_optional<T>(&self, path: &str, query: &[(&str, String)]) -> LocalityResult<Option<T>>
    where
        T: DeserializeOwned,
    {
        let response = self
            .request(path, query)
            .send()
            .map_err(|error| LocalityError::Io(format!("GitHub API request failed: {error}")))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        decode_response(response).map(Some)
    }

    fn request(&self, path: &str, query: &[(&str, String)]) -> reqwest::blocking::RequestBuilder {
        self.client
            .get(format!("{}{}", self.api_base_url, path))
            .bearer_auth(&self.token)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header(reqwest::header::USER_AGENT, "Locality")
            .query(query)
    }

    fn get_paginated<T>(&self, path: &str, extra_query: &[(&str, String)]) -> LocalityResult<Vec<T>>
    where
        T: DeserializeOwned,
    {
        let mut output = Vec::new();
        for page in 1..=MAX_PAGES {
            let mut query = vec![
                ("per_page", PAGE_SIZE.to_string()),
                ("page", page.to_string()),
            ];
            query.extend_from_slice(extra_query);
            let mut items: Vec<T> = self.get(path, &query)?;
            let count = items.len();
            output.append(&mut items);
            if count < PAGE_SIZE as usize {
                break;
            }
        }
        Ok(output)
    }
}

impl GitHubApi for HttpGitHubApiClient {
    fn current_user(&self) -> LocalityResult<GitHubUser> {
        self.get("/user", &[])
    }

    fn list_repositories(&self) -> LocalityResult<Vec<GitHubRepository>> {
        let mut repos: Vec<GitHubRepository> = self.get_paginated(
            "/user/repos",
            &[
                (
                    "affiliation",
                    "owner,collaborator,organization_member".to_string(),
                ),
                ("sort", "updated".to_string()),
            ],
        )?;
        repos.sort_by(|left, right| {
            left.full_name
                .to_lowercase()
                .cmp(&right.full_name.to_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(repos)
    }

    fn get_repository(&self, owner: &str, repo: &str) -> LocalityResult<GitHubRepository> {
        self.get(&format!("/repos/{owner}/{repo}"), &[])
    }

    fn get_readme(&self, owner: &str, repo: &str) -> LocalityResult<Option<GitHubContent>> {
        self.get_optional(&format!("/repos/{owner}/{repo}/readme"), &[])
    }

    fn list_issues(&self, owner: &str, repo: &str) -> LocalityResult<Vec<GitHubIssue>> {
        let mut issues: Vec<GitHubIssue> = self.get_paginated(
            &format!("/repos/{owner}/{repo}/issues"),
            &[
                ("state", "all".to_string()),
                ("sort", "updated".to_string()),
            ],
        )?;
        issues.retain(|issue| issue.pull_request.is_none());
        issues.sort_by(|left, right| left.number.cmp(&right.number));
        Ok(issues)
    }

    fn get_issue(&self, owner: &str, repo: &str, number: u64) -> LocalityResult<GitHubIssue> {
        self.get(&format!("/repos/{owner}/{repo}/issues/{number}"), &[])
    }

    fn list_pull_requests(
        &self,
        owner: &str,
        repo: &str,
    ) -> LocalityResult<Vec<GitHubPullRequest>> {
        let mut pulls: Vec<GitHubPullRequest> = self.get_paginated(
            &format!("/repos/{owner}/{repo}/pulls"),
            &[
                ("state", "all".to_string()),
                ("sort", "updated".to_string()),
            ],
        )?;
        pulls.sort_by(|left, right| left.number.cmp(&right.number));
        Ok(pulls)
    }

    fn get_pull_request(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> LocalityResult<GitHubPullRequest> {
        self.get(&format!("/repos/{owner}/{repo}/pulls/{number}"), &[])
    }
}

fn decode_response<T>(response: Response) -> LocalityResult<T>
where
    T: DeserializeOwned,
{
    let status = response.status();
    if status.is_success() {
        return response
            .json::<T>()
            .map_err(|error| LocalityError::Io(format!("GitHub API decode failed: {error}")));
    }
    if status == StatusCode::NOT_FOUND {
        return Err(LocalityError::RemoteNotFound("GitHub object".to_string()));
    }
    let body = response.text().unwrap_or_default();
    Err(LocalityError::Io(format!(
        "GitHub API returned HTTP {status}: {body}"
    )))
}

fn ensure_reqwest_crypto_provider() {
    REQWEST_CRYPTO_PROVIDER.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
