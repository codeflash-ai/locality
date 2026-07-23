use std::fmt;
use std::sync::OnceLock;
use std::time::Duration;

use locality_core::{LocalityError, LocalityResult};
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use serde::de::DeserializeOwned;

use crate::dto::{GitLabContent, GitLabIssue, GitLabMergeRequest, GitLabRepository, GitLabUser};

pub const DEFAULT_GITLAB_API_BASE_URL: &str = "https://gitlab.com/api/v4";
const GITLAB_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const PAGE_SIZE: u32 = 100;
const MAX_PAGES: u32 = 20;

static REQWEST_CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();

pub trait GitLabApi: fmt::Debug + Send + Sync {
    fn current_user(&self) -> LocalityResult<GitLabUser>;
    fn list_repositories(&self) -> LocalityResult<Vec<GitLabRepository>>;
    fn get_repository(&self, full_name: &str) -> LocalityResult<GitLabRepository>;
    fn get_readme(
        &self,
        full_name: &str,
        default_branch: &str,
    ) -> LocalityResult<Option<GitLabContent>>;
    fn list_issues(&self, full_name: &str) -> LocalityResult<Vec<GitLabIssue>>;
    fn get_issue(&self, full_name: &str, number: u64) -> LocalityResult<GitLabIssue>;
    fn list_merge_requests(&self, full_name: &str) -> LocalityResult<Vec<GitLabMergeRequest>>;
    fn get_merge_request(&self, full_name: &str, number: u64)
    -> LocalityResult<GitLabMergeRequest>;
}

#[derive(Clone)]
pub struct HttpGitLabApiClient {
    token: String,
    api_base_url: String,
    client: Client,
}

impl fmt::Debug for HttpGitLabApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpGitLabApiClient")
            .field("token", &"<redacted>")
            .field("api_base_url", &self.api_base_url)
            .finish_non_exhaustive()
    }
}

impl HttpGitLabApiClient {
    pub fn new(token: impl Into<String>) -> Self {
        Self::with_api_base_url(token, DEFAULT_GITLAB_API_BASE_URL)
    }

    pub fn with_api_base_url(token: impl Into<String>, api_base_url: impl Into<String>) -> Self {
        ensure_reqwest_crypto_provider();
        let client = Client::builder()
            .timeout(GITLAB_HTTP_TIMEOUT)
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
            .map_err(|error| LocalityError::Io(format!("GitLab API request failed: {error}")))?;
        decode_response(response)
    }

    fn get_optional<T>(&self, path: &str, query: &[(&str, String)]) -> LocalityResult<Option<T>>
    where
        T: DeserializeOwned,
    {
        let response = self
            .request(path, query)
            .send()
            .map_err(|error| LocalityError::Io(format!("GitLab API request failed: {error}")))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        decode_response(response).map(Some)
    }

    fn request(&self, path: &str, query: &[(&str, String)]) -> reqwest::blocking::RequestBuilder {
        self.client
            .get(format!("{}{}", self.api_base_url, path))
            .header("PRIVATE-TOKEN", self.token.as_str())
            .header(reqwest::header::ACCEPT, "application/json")
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

impl GitLabApi for HttpGitLabApiClient {
    fn current_user(&self) -> LocalityResult<GitLabUser> {
        self.get("/user", &[])
    }

    fn list_repositories(&self) -> LocalityResult<Vec<GitLabRepository>> {
        let mut repos: Vec<GitLabRepository> = self.get_paginated(
            "/projects",
            &[
                ("membership", "true".to_string()),
                ("simple", "true".to_string()),
                ("order_by", "last_activity_at".to_string()),
                ("sort", "desc".to_string()),
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

    fn get_repository(&self, full_name: &str) -> LocalityResult<GitLabRepository> {
        self.get(&format!("/projects/{}", project_path(full_name)), &[])
    }

    fn get_readme(
        &self,
        full_name: &str,
        default_branch: &str,
    ) -> LocalityResult<Option<GitLabContent>> {
        if default_branch.trim().is_empty() {
            return Ok(None);
        }
        self.get_optional(
            &format!(
                "/projects/{}/repository/files/README.md",
                project_path(full_name)
            ),
            &[("ref", default_branch.to_string())],
        )
    }

    fn list_issues(&self, full_name: &str) -> LocalityResult<Vec<GitLabIssue>> {
        let mut issues: Vec<GitLabIssue> = self.get_paginated(
            &format!("/projects/{}/issues", project_path(full_name)),
            &[
                ("state", "all".to_string()),
                ("scope", "all".to_string()),
                ("order_by", "updated_at".to_string()),
                ("sort", "desc".to_string()),
            ],
        )?;
        issues.sort_by(|left, right| left.number.cmp(&right.number));
        Ok(issues)
    }

    fn get_issue(&self, full_name: &str, number: u64) -> LocalityResult<GitLabIssue> {
        self.get(
            &format!("/projects/{}/issues/{number}", project_path(full_name)),
            &[],
        )
    }

    fn list_merge_requests(&self, full_name: &str) -> LocalityResult<Vec<GitLabMergeRequest>> {
        let mut merge_requests: Vec<GitLabMergeRequest> = self.get_paginated(
            &format!("/projects/{}/merge_requests", project_path(full_name)),
            &[
                ("state", "all".to_string()),
                ("scope", "all".to_string()),
                ("order_by", "updated_at".to_string()),
                ("sort", "desc".to_string()),
            ],
        )?;
        merge_requests.sort_by(|left, right| left.number.cmp(&right.number));
        Ok(merge_requests)
    }

    fn get_merge_request(
        &self,
        full_name: &str,
        number: u64,
    ) -> LocalityResult<GitLabMergeRequest> {
        self.get(
            &format!(
                "/projects/{}/merge_requests/{number}",
                project_path(full_name)
            ),
            &[],
        )
    }
}

fn project_path(value: &str) -> String {
    percent_encode(value)
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn decode_response<T>(response: Response) -> LocalityResult<T>
where
    T: DeserializeOwned,
{
    let status = response.status();
    if status.is_success() {
        return response
            .json::<T>()
            .map_err(|error| LocalityError::Io(format!("GitLab API decode failed: {error}")));
    }
    if status == StatusCode::NOT_FOUND {
        return Err(LocalityError::RemoteNotFound("GitLab object".to_string()));
    }
    let body = response.text().unwrap_or_default();
    Err(LocalityError::Io(format!(
        "GitLab API returned HTTP {status}: {body}"
    )))
}

fn ensure_reqwest_crypto_provider() {
    REQWEST_CRYPTO_PROVIDER.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
