use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitLabUser {
    pub username: String,
    #[serde(default)]
    pub web_url: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitLabRepositoryOwner {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub full_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitLabRepository {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub path: String,
    #[serde(rename = "path_with_namespace")]
    pub full_name: String,
    #[serde(default, rename = "namespace")]
    pub owner: GitLabRepositoryOwner,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "web_url")]
    pub html_url: String,
    #[serde(default)]
    pub visibility: String,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub default_branch: String,
    pub created_at: String,
    #[serde(default, rename = "last_activity_at")]
    pub updated_at: String,
    #[serde(default)]
    pub updated_at_fallback: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitLabIssue {
    pub id: u64,
    #[serde(rename = "iid")]
    pub number: u64,
    pub title: String,
    #[serde(default, rename = "description")]
    pub body: Option<String>,
    pub state: String,
    #[serde(rename = "web_url")]
    pub html_url: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub closed_at: Option<String>,
    #[serde(default, rename = "author")]
    pub user: Option<GitLabUser>,
    #[serde(default)]
    pub labels: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitLabMergeRequest {
    pub id: u64,
    #[serde(rename = "iid")]
    pub number: u64,
    pub title: String,
    #[serde(default, rename = "description")]
    pub body: Option<String>,
    pub state: String,
    #[serde(rename = "web_url")]
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
    pub work_in_progress: bool,
    #[serde(default)]
    pub source_branch: Option<String>,
    #[serde(default)]
    pub target_branch: Option<String>,
    #[serde(default)]
    #[serde(rename = "author")]
    pub user: Option<GitLabUser>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitLabContent {
    #[serde(rename = "file_name")]
    pub name: String,
    #[serde(rename = "file_path")]
    pub path: String,
    #[serde(rename = "blob_id")]
    pub sha: String,
    pub content: String,
    pub encoding: String,
}
