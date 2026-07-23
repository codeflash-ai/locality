use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubUser {
    pub login: String,
    #[serde(default)]
    pub html_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubRepositoryOwner {
    pub login: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubRepository {
    pub id: u64,
    pub name: String,
    pub full_name: String,
    pub owner: GitHubRepositoryOwner,
    #[serde(default)]
    pub description: Option<String>,
    pub html_url: String,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub fork: bool,
    #[serde(default)]
    pub archived: bool,
    pub default_branch: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub pushed_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubLabel {
    pub name: String,
    #[serde(default)]
    pub color: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubIssue {
    pub id: u64,
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub state: String,
    pub html_url: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub closed_at: Option<String>,
    #[serde(default)]
    pub user: Option<GitHubUser>,
    #[serde(default)]
    pub labels: Vec<GitHubLabel>,
    #[serde(default)]
    pub pull_request: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubPullRef {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    #[serde(rename = "ref")]
    pub branch_ref: Option<String>,
    #[serde(default)]
    pub sha: Option<String>,
    pub repo: Option<GitHubRepository>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubPullRequest {
    pub id: u64,
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub state: String,
    pub html_url: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub closed_at: Option<String>,
    #[serde(default)]
    pub merged_at: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub user: Option<GitHubUser>,
    pub base: GitHubPullRef,
    pub head: GitHubPullRef,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubContent {
    pub name: String,
    pub path: String,
    pub sha: String,
    #[serde(default)]
    pub html_url: Option<String>,
    #[serde(default)]
    pub download_url: Option<String>,
    pub content: String,
    pub encoding: String,
}
