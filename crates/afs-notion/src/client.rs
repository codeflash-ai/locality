//! Notion API client boundary.
//!
//! The connector depends on this trait rather than directly on HTTP so tests
//! can run against deterministic fixtures and live API calls stay isolated.

use std::collections::BTreeSet;
use std::time::Duration;

use afs_core::{AfsError, AfsResult};
use reqwest::blocking::{Client, multipart};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::NotionConfig;
use crate::dto::{
    BlockDto, BlockListDto, DataSourceDto, DataSourceListDto, DatabaseDto, DatabaseListDto,
    PageDto, PageListDto,
};

pub const DEFAULT_NOTION_API_BASE_URL: &str = "https://api.notion.com";
pub const DEFAULT_NOTION_VERSION: &str = "2026-03-11";
pub const DEFAULT_NOTION_TOKEN_ENV: &str = "NOTION_TOKEN";
const DEFAULT_NOTION_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

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
    fn create_database(&self, body: serde_json::Value) -> AfsResult<DatabaseDto> {
        let _ = body;
        Err(AfsError::NotImplemented("create Notion database"))
    }
    fn retrieve_block_children(
        &self,
        block_id: &str,
        start_cursor: Option<&str>,
    ) -> AfsResult<BlockListDto>;
    fn search_pages(&self, start_cursor: Option<&str>) -> AfsResult<PageListDto>;
    fn search_databases(&self, start_cursor: Option<&str>) -> AfsResult<DatabaseListDto> {
        let _ = start_cursor;
        Err(AfsError::NotImplemented("search Notion databases"))
    }
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
    fn upload_file(&self, filename: &str, content_type: &str, bytes: Vec<u8>) -> AfsResult<String> {
        let _ = (filename, content_type, bytes);
        Err(AfsError::NotImplemented("upload Notion file"))
    }
}

#[derive(Clone, Debug)]
pub struct HttpNotionApi {
    config: NotionConfig,
    client: Client,
}

impl HttpNotionApi {
    pub fn new(config: NotionConfig) -> Self {
        let client = Client::builder()
            .timeout(notion_http_timeout())
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { config, client }
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

    fn upload_file_bytes(
        &self,
        filename: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> AfsResult<String> {
        let created: Value = self.post_json(
            "/v1/file_uploads",
            json!({
                "mode": "single_part",
                "filename": filename,
                "content_type": content_type,
            }),
        )?;
        let upload_id = created
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| AfsError::Io("notion file upload response missing id".to_string()))?
            .to_string();
        let token = self.token()?;
        let url = format!(
            "{}/v1/file_uploads/{}/send",
            DEFAULT_NOTION_API_BASE_URL, upload_id
        );
        let part = multipart::Part::bytes(bytes)
            .file_name(filename.to_string())
            .mime_str(content_type)
            .map_err(|error| AfsError::Io(format!("notion file upload MIME failed: {error}")))?;
        let form = multipart::Form::new().part("file", part);
        let response = self
            .client
            .post(url)
            .bearer_auth(token)
            .header("Notion-Version", DEFAULT_NOTION_VERSION)
            .multipart(form)
            .send()
            .map_err(|error| AfsError::Io(format!("notion file upload failed: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .unwrap_or_else(|error| format!("<failed to read error body: {error}>"));
            return Err(AfsError::Io(format!(
                "notion file upload returned HTTP {status}: {body}"
            )));
        }
        Ok(upload_id)
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

fn notion_http_timeout() -> Duration {
    std::env::var("AFS_NOTION_HTTP_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_NOTION_HTTP_TIMEOUT)
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

    fn search_databases(&self, start_cursor: Option<&str>) -> AfsResult<DatabaseListDto> {
        let data_sources: DataSourceListDto =
            self.post_json("/v1/search", data_source_search_body(start_cursor))?;
        let mut seen = BTreeSet::new();
        let mut databases = Vec::new();
        for data_source in data_sources.results {
            let Some(database_id) = data_source
                .parent
                .as_ref()
                .and_then(|parent| parent.database_id.as_deref())
            else {
                continue;
            };
            if seen.insert(database_id.to_string()) {
                databases.push(self.retrieve_database(database_id)?);
            }
        }

        Ok(DatabaseListDto {
            results: databases,
            next_cursor: data_sources.next_cursor,
            has_more: data_sources.has_more,
        })
    }

    fn update_page(&self, page_id: &str, body: serde_json::Value) -> AfsResult<PageDto> {
        self.patch_json(&format!("/v1/pages/{page_id}"), body)
    }

    fn create_page(&self, body: serde_json::Value) -> AfsResult<PageDto> {
        self.post_json("/v1/pages", body)
    }

    fn create_database(&self, body: serde_json::Value) -> AfsResult<DatabaseDto> {
        self.post_json("/v1/databases", body)
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

    fn upload_file(&self, filename: &str, content_type: &str, bytes: Vec<u8>) -> AfsResult<String> {
        self.upload_file_bytes(filename, content_type, bytes)
    }
}

fn data_source_search_body(start_cursor: Option<&str>) -> Value {
    let mut body = json!({
        "page_size": 100,
        "filter": {
            "property": "object",
            "value": "data_source"
        },
        "sort": {
            "direction": "descending",
            "timestamp": "last_edited_time"
        }
    });

    if let Some(start_cursor) = start_cursor {
        body["start_cursor"] = json!(start_cursor);
    }

    body
}

#[cfg(test)]
mod tests {
    use super::data_source_search_body;

    #[test]
    fn search_databases_uses_current_notion_data_source_filter() {
        let body = data_source_search_body(Some("cursor-1"));

        assert_eq!(body["filter"]["property"], "object");
        assert_eq!(body["filter"]["value"], "data_source");
        assert_eq!(body["start_cursor"], "cursor-1");
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
