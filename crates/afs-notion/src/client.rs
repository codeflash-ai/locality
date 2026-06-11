//! Notion API client boundary.
//!
//! The connector depends on this trait rather than directly on HTTP so tests
//! can run against deterministic fixtures and live API calls stay isolated.

use afs_core::{AfsError, AfsResult};
use reqwest::blocking::Client;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::NotionConfig;
use crate::dto::{BlockDto, BlockListDto, DataSourceDto, DatabaseDto, PageDto, PageListDto};

pub const DEFAULT_NOTION_API_BASE_URL: &str = "https://api.notion.com";
pub const DEFAULT_NOTION_VERSION: &str = "2026-03-11";
pub const DEFAULT_NOTION_TOKEN_ENV: &str = "NOTION_TOKEN";

pub trait NotionApi: std::fmt::Debug + Send + Sync {
    fn retrieve_current_user(&self) -> AfsResult<serde_json::Value> {
        Err(AfsError::NotImplemented("retrieve Notion current user"))
    }
    fn retrieve_page(&self, page_id: &str) -> AfsResult<PageDto>;
    fn retrieve_database(&self, database_id: &str) -> AfsResult<DatabaseDto> {
        let _ = database_id;
        Err(AfsError::NotImplemented("retrieve Notion database"))
    }
    fn retrieve_data_source(&self, data_source_id: &str) -> AfsResult<DataSourceDto> {
        let _ = data_source_id;
        Err(AfsError::NotImplemented("retrieve Notion data source"))
    }
    fn query_data_source(
        &self,
        data_source_id: &str,
        start_cursor: Option<&str>,
    ) -> AfsResult<PageListDto> {
        let _ = (data_source_id, start_cursor);
        Err(AfsError::NotImplemented("query Notion data source"))
    }
    fn update_page(&self, page_id: &str, body: serde_json::Value) -> AfsResult<PageDto> {
        let _ = (page_id, body);
        Err(AfsError::NotImplemented("update Notion page"))
    }
    fn create_page(&self, body: serde_json::Value) -> AfsResult<PageDto> {
        let _ = body;
        Err(AfsError::NotImplemented("create Notion page"))
    }
    fn retrieve_block_children(
        &self,
        block_id: &str,
        start_cursor: Option<&str>,
    ) -> AfsResult<BlockListDto>;
    fn search_pages(&self, start_cursor: Option<&str>) -> AfsResult<PageListDto>;
    fn update_block(&self, block_id: &str, body: serde_json::Value) -> AfsResult<BlockDto>;
    fn move_block(
        &self,
        block_id: &str,
        parent_id: &str,
        after: Option<&str>,
    ) -> AfsResult<BlockDto> {
        let _ = (block_id, parent_id, after);
        Err(AfsError::NotImplemented("move Notion block"))
    }
    fn append_block_children(
        &self,
        block_id: &str,
        body: serde_json::Value,
    ) -> AfsResult<BlockListDto>;
    fn delete_block(&self, block_id: &str) -> AfsResult<BlockDto>;
}

#[derive(Clone, Debug)]
pub struct HttpNotionApi {
    config: NotionConfig,
    client: Client,
}

impl HttpNotionApi {
    pub fn new(config: NotionConfig) -> Self {
        Self {
            config,
            client: Client::new(),
        }
    }

    fn get_json<T>(&self, path: &str, query: &[(&str, String)]) -> AfsResult<T>
    where
        T: DeserializeOwned,
    {
        let token = self.token()?;
        let url = format!(
            "{}/{}",
            DEFAULT_NOTION_API_BASE_URL,
            path.trim_start_matches('/')
        );
        let mut request = self
            .client
            .get(url)
            .bearer_auth(token)
            .header("Notion-Version", DEFAULT_NOTION_VERSION);

        for (key, value) in query {
            request = request.query(&[(key, value)]);
        }

        let response = request
            .send()
            .map_err(|error| AfsError::Io(format!("notion request failed: {error}")))?;
        let status = response.status();

        if !status.is_success() {
            let body = response
                .text()
                .unwrap_or_else(|error| format!("<failed to read error body: {error}>"));
            return Err(AfsError::Io(format!(
                "notion api returned HTTP {status}: {body}"
            )));
        }

        response
            .json()
            .map_err(|error| AfsError::Io(format!("notion response decode failed: {error}")))
    }

    fn post_json<T>(&self, path: &str, body: impl Serialize) -> AfsResult<T>
    where
        T: DeserializeOwned,
    {
        self.send_json(reqwest::Method::POST, path, Some(body))
    }

    fn patch_json<T>(&self, path: &str, body: impl Serialize) -> AfsResult<T>
    where
        T: DeserializeOwned,
    {
        self.send_json(reqwest::Method::PATCH, path, Some(body))
    }

    fn delete_json<T>(&self, path: &str) -> AfsResult<T>
    where
        T: DeserializeOwned,
    {
        self.send_json::<T, serde_json::Value>(reqwest::Method::DELETE, path, None)
    }

    fn send_json<T, B>(&self, method: reqwest::Method, path: &str, body: Option<B>) -> AfsResult<T>
    where
        T: DeserializeOwned,
        B: Serialize,
    {
        let token = self.token()?;
        let url = format!(
            "{}/{}",
            DEFAULT_NOTION_API_BASE_URL,
            path.trim_start_matches('/')
        );
        let mut request = self
            .client
            .request(method, url)
            .bearer_auth(token)
            .header("Notion-Version", DEFAULT_NOTION_VERSION);
        if let Some(body) = body {
            request = request.json(&body);
        }

        let response = request
            .send()
            .map_err(|error| AfsError::Io(format!("notion request failed: {error}")))?;
        let status = response.status();

        if !status.is_success() {
            let body = response
                .text()
                .unwrap_or_else(|error| format!("<failed to read error body: {error}>"));
            return Err(AfsError::Io(format!(
                "notion api returned HTTP {status}: {body}"
            )));
        }

        response
            .json()
            .map_err(|error| AfsError::Io(format!("notion response decode failed: {error}")))
    }

    fn token(&self) -> AfsResult<String> {
        if let Some(token) = &self.config.token {
            return Ok(token.clone());
        }

        std::env::var(&self.config.token_key)
            .or_else(|_| std::env::var(DEFAULT_NOTION_TOKEN_ENV))
            .map_err(|_| {
                AfsError::InvalidState(format!(
                    "missing Notion connection; run `afs connect notion` or set {}",
                    self.config.token_key
                ))
            })
    }
}

impl NotionApi for HttpNotionApi {
    fn retrieve_current_user(&self) -> AfsResult<serde_json::Value> {
        self.get_json("/v1/users/me", &[])
    }

    fn retrieve_page(&self, page_id: &str) -> AfsResult<PageDto> {
        self.get_json(&format!("/v1/pages/{page_id}"), &[])
    }

    fn retrieve_database(&self, database_id: &str) -> AfsResult<DatabaseDto> {
        self.get_json(&format!("/v1/databases/{database_id}"), &[])
    }

    fn retrieve_data_source(&self, data_source_id: &str) -> AfsResult<DataSourceDto> {
        self.get_json(&format!("/v1/data_sources/{data_source_id}"), &[])
    }

    fn query_data_source(
        &self,
        data_source_id: &str,
        start_cursor: Option<&str>,
    ) -> AfsResult<PageListDto> {
        let mut body = json!({
            "page_size": 100,
        });

        if let Some(start_cursor) = start_cursor {
            body["start_cursor"] = json!(start_cursor);
        }

        self.post_json(&format!("/v1/data_sources/{data_source_id}/query"), body)
    }

    fn retrieve_block_children(
        &self,
        block_id: &str,
        start_cursor: Option<&str>,
    ) -> AfsResult<BlockListDto> {
        let mut query = vec![("page_size", "100".to_string())];
        if let Some(start_cursor) = start_cursor {
            query.push(("start_cursor", start_cursor.to_string()));
        }

        self.get_json(&format!("/v1/blocks/{block_id}/children"), &query)
    }

    fn search_pages(&self, start_cursor: Option<&str>) -> AfsResult<PageListDto> {
        let mut body = json!({
            "page_size": 100,
            "filter": {
                "property": "object",
                "value": "page"
            },
            "sort": {
                "direction": "descending",
                "timestamp": "last_edited_time"
            }
        });

        if let Some(start_cursor) = start_cursor {
            body["start_cursor"] = json!(start_cursor);
        }

        self.post_json("/v1/search", body)
    }

    fn update_page(&self, page_id: &str, body: serde_json::Value) -> AfsResult<PageDto> {
        self.patch_json(&format!("/v1/pages/{page_id}"), body)
    }

    fn create_page(&self, body: serde_json::Value) -> AfsResult<PageDto> {
        self.post_json("/v1/pages", body)
    }

    fn update_block(&self, block_id: &str, body: serde_json::Value) -> AfsResult<BlockDto> {
        self.patch_json(&format!("/v1/blocks/{block_id}"), body)
    }

    fn move_block(
        &self,
        block_id: &str,
        parent_id: &str,
        after: Option<&str>,
    ) -> AfsResult<BlockDto> {
        self.patch_json(
            &format!("/v1/blocks/{block_id}"),
            move_block_body(parent_id, after),
        )
    }

    fn append_block_children(
        &self,
        block_id: &str,
        body: serde_json::Value,
    ) -> AfsResult<BlockListDto> {
        self.patch_json(&format!("/v1/blocks/{block_id}/children"), body)
    }

    fn delete_block(&self, block_id: &str) -> AfsResult<BlockDto> {
        self.delete_json(&format!("/v1/blocks/{block_id}"))
    }
}

fn move_block_body(parent_id: &str, after: Option<&str>) -> serde_json::Value {
    let mut body = json!({
        "parent": {
            "type": "page_id",
            "page_id": parent_id,
        },
        "position": {
            "type": "start",
        },
    });
    if let Some(after) = after {
        body["position"] = json!({
            "type": "after_block",
            "after_block": {
                "id": after,
            },
        });
    }
    body
}
