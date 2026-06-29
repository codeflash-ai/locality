use std::sync::OnceLock;
use std::time::Duration;

use locality_core::{LocalityError, LocalityResult};
use reqwest::blocking::Client;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::docs_dto::{BatchUpdateDocumentRequest, GoogleDocument};
use crate::drive_dto::{
    DRIVE_FOLDER_MIME_TYPE, DriveCreateFileRequest, DriveFile, DriveFileList,
    DriveUpdateFileRequest,
};

pub const DEFAULT_GOOGLE_DRIVE_API_BASE_URL: &str = "https://www.googleapis.com/drive/v3";
pub const DEFAULT_GOOGLE_DOCS_API_BASE_URL: &str = "https://docs.googleapis.com";
const GOOGLE_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const DRIVE_FILE_FIELDS: &str = "id, name, mimeType, parents, modifiedTime, version, trashed";

static REQWEST_CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();

pub trait GoogleDriveApi: std::fmt::Debug + Send + Sync {
    fn get_file(&self, file_id: &str) -> LocalityResult<DriveFile>;
    fn list_children(
        &self,
        parent_id: &str,
        page_token: Option<&str>,
    ) -> LocalityResult<DriveFileList>;
    fn list_workspace_folders_by_name(
        &self,
        name: &str,
        page_token: Option<&str>,
    ) -> LocalityResult<DriveFileList>;
    fn create_file(&self, request: DriveCreateFileRequest) -> LocalityResult<DriveFile>;
    fn update_file(
        &self,
        file_id: &str,
        request: DriveUpdateFileRequest,
    ) -> LocalityResult<DriveFile>;
}

pub trait GoogleDocsApi: std::fmt::Debug + Send + Sync {
    fn get_document(&self, document_id: &str) -> LocalityResult<GoogleDocument>;
    fn batch_update_document(
        &self,
        document_id: &str,
        request: BatchUpdateDocumentRequest,
    ) -> LocalityResult<GoogleDocument>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveListQuery {
    pub q: String,
    pub fields: String,
    pub page_token: Option<String>,
}

pub fn drive_children_query(parent_id: &str, page_token: Option<&str>) -> DriveListQuery {
    DriveListQuery {
        q: format!("'{parent_id}' in parents and trashed = false"),
        fields: format!("nextPageToken, files({DRIVE_FILE_FIELDS})"),
        page_token: page_token.map(str::to_string),
    }
}

pub fn drive_workspace_folder_query(name: &str, page_token: Option<&str>) -> DriveListQuery {
    DriveListQuery {
        q: format!(
            "mimeType = '{DRIVE_FOLDER_MIME_TYPE}' and name = '{}' and trashed = false",
            drive_query_literal(name)
        ),
        fields: format!("nextPageToken, files({DRIVE_FILE_FIELDS})"),
        page_token: page_token.map(str::to_string),
    }
}

pub fn google_docs_batch_update_url(base_url: &str, document_id: &str) -> String {
    format!(
        "{}/v1/documents/{}:batchUpdate",
        base_url.trim_end_matches('/'),
        document_id
    )
}

#[derive(Clone, Debug)]
pub struct HttpGoogleApiClient {
    access_token: String,
    drive_base_url: String,
    docs_base_url: String,
    client: Client,
}

impl HttpGoogleApiClient {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self::with_base_urls(
            access_token,
            DEFAULT_GOOGLE_DRIVE_API_BASE_URL,
            DEFAULT_GOOGLE_DOCS_API_BASE_URL,
        )
    }

    pub fn with_base_urls(
        access_token: impl Into<String>,
        drive_base_url: impl Into<String>,
        docs_base_url: impl Into<String>,
    ) -> Self {
        ensure_reqwest_crypto_provider();
        let client = Client::builder()
            .timeout(GOOGLE_HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            access_token: access_token.into(),
            drive_base_url: drive_base_url.into().trim_end_matches('/').to_string(),
            docs_base_url: docs_base_url.into().trim_end_matches('/').to_string(),
            client,
        }
    }

    fn get_json<T>(&self, url: String, query: Vec<(String, String)>) -> LocalityResult<T>
    where
        T: DeserializeOwned,
    {
        let mut request = self.client.get(url).bearer_auth(&self.access_token);
        for (key, value) in query {
            request = request.query(&[(key.as_str(), value.as_str())]);
        }
        decode_response(request.send(), "google api GET")
    }

    fn post_json<T, B>(
        &self,
        url: String,
        body: &B,
        query: Vec<(String, String)>,
    ) -> LocalityResult<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let mut request = self
            .client
            .post(url)
            .bearer_auth(&self.access_token)
            .json(body);
        for (key, value) in query {
            request = request.query(&[(key.as_str(), value.as_str())]);
        }
        decode_response(request.send(), "google api POST")
    }

    fn patch_json<T, B>(
        &self,
        url: String,
        body: &B,
        query: Vec<(String, String)>,
    ) -> LocalityResult<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let mut request = self
            .client
            .patch(url)
            .bearer_auth(&self.access_token)
            .json(body);
        for (key, value) in query {
            request = request.query(&[(key.as_str(), value.as_str())]);
        }
        decode_response(request.send(), "google api PATCH")
    }
}

impl GoogleDriveApi for HttpGoogleApiClient {
    fn get_file(&self, file_id: &str) -> LocalityResult<DriveFile> {
        self.get_json(
            format!("{}/files/{file_id}", self.drive_base_url),
            vec![("fields".to_string(), DRIVE_FILE_FIELDS.to_string())],
        )
    }

    fn list_children(
        &self,
        parent_id: &str,
        page_token: Option<&str>,
    ) -> LocalityResult<DriveFileList> {
        let query = drive_children_query(parent_id, page_token);
        let mut query_pairs = vec![
            ("q".to_string(), query.q),
            ("fields".to_string(), query.fields),
            ("spaces".to_string(), "drive".to_string()),
        ];
        if let Some(page_token) = query.page_token {
            query_pairs.push(("pageToken".to_string(), page_token));
        }
        self.get_json(format!("{}/files", self.drive_base_url), query_pairs)
    }

    fn list_workspace_folders_by_name(
        &self,
        name: &str,
        page_token: Option<&str>,
    ) -> LocalityResult<DriveFileList> {
        let query = drive_workspace_folder_query(name, page_token);
        let mut query_pairs = vec![
            ("q".to_string(), query.q),
            ("fields".to_string(), query.fields),
            ("spaces".to_string(), "drive".to_string()),
        ];
        if let Some(page_token) = query.page_token {
            query_pairs.push(("pageToken".to_string(), page_token));
        }
        self.get_json(format!("{}/files", self.drive_base_url), query_pairs)
    }

    fn create_file(&self, request: DriveCreateFileRequest) -> LocalityResult<DriveFile> {
        self.post_json(
            format!("{}/files", self.drive_base_url),
            &request,
            vec![("fields".to_string(), DRIVE_FILE_FIELDS.to_string())],
        )
    }

    fn update_file(
        &self,
        file_id: &str,
        request: DriveUpdateFileRequest,
    ) -> LocalityResult<DriveFile> {
        self.patch_json(
            format!("{}/files/{file_id}", self.drive_base_url),
            &request,
            vec![("fields".to_string(), DRIVE_FILE_FIELDS.to_string())],
        )
    }
}

impl GoogleDocsApi for HttpGoogleApiClient {
    fn get_document(&self, document_id: &str) -> LocalityResult<GoogleDocument> {
        self.get_json(
            format!("{}/v1/documents/{document_id}", self.docs_base_url),
            Vec::new(),
        )
    }

    fn batch_update_document(
        &self,
        document_id: &str,
        request: BatchUpdateDocumentRequest,
    ) -> LocalityResult<GoogleDocument> {
        self.post_json(
            google_docs_batch_update_url(&self.docs_base_url, document_id),
            &request,
            Vec::new(),
        )
    }
}

fn decode_response<T>(
    response: Result<reqwest::blocking::Response, reqwest::Error>,
    operation: &'static str,
) -> LocalityResult<T>
where
    T: DeserializeOwned,
{
    let response =
        response.map_err(|error| LocalityError::Io(format!("{operation} failed: {error}")))?;
    let status = response.status();
    if status.is_success() {
        return response
            .json()
            .map_err(|error| LocalityError::Io(format!("{operation} decode failed: {error}")));
    }
    let body = response
        .text()
        .unwrap_or_else(|error| format!("<failed to read error body: {error}>"));
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(LocalityError::RemoteNotFound(body));
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        return Err(LocalityError::Guardrail(format!(
            "google docs permission denied: {body}"
        )));
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(LocalityError::Io(format!(
            "google docs rate limited: {body}"
        )));
    }
    Err(LocalityError::Io(format!(
        "{operation} returned HTTP {status}: {body}"
    )))
}

fn ensure_reqwest_crypto_provider() {
    REQWEST_CRYPTO_PROVIDER.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn drive_query_literal(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

#[cfg(test)]
mod tests {
    use super::{
        DriveListQuery, drive_children_query, drive_workspace_folder_query,
        google_docs_batch_update_url,
    };

    #[test]
    fn drive_children_query_filters_immediate_untrashed_children() {
        let query = drive_children_query("folder-1", Some("cursor-1"));

        assert_eq!(
            query,
            DriveListQuery {
                q: "'folder-1' in parents and trashed = false".to_string(),
                fields: "nextPageToken, files(id, name, mimeType, parents, modifiedTime, version, trashed)".to_string(),
                page_token: Some("cursor-1".to_string()),
            }
        );
    }

    #[test]
    fn docs_batch_update_url_targets_document_resource() {
        assert_eq!(
            google_docs_batch_update_url("https://docs.googleapis.com", "doc-1"),
            "https://docs.googleapis.com/v1/documents/doc-1:batchUpdate"
        );
    }

    #[test]
    fn workspace_folder_query_filters_by_exact_untrashed_folder_name() {
        let query = drive_workspace_folder_query("Locality's Workspace", None);

        assert_eq!(
            query,
            DriveListQuery {
                q: "mimeType = 'application/vnd.google-apps.folder' and name = 'Locality\\'s Workspace' and trashed = false".to_string(),
                fields: "nextPageToken, files(id, name, mimeType, parents, modifiedTime, version, trashed)".to_string(),
                page_token: None,
            }
        );
    }
}
