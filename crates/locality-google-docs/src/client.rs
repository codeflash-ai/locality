use std::sync::OnceLock;
use std::time::Duration;

use locality_core::{LocalityError, LocalityResult};
use reqwest::blocking::Client;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::docs_dto::{BatchUpdateDocumentRequest, GoogleDocument};
use crate::drive_dto::{
    DRIVE_GOOGLE_DOC_MIME_TYPE, DriveCreateFileRequest, DriveFile, DriveFileList,
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
    fn list_accessible_google_docs(
        &self,
        page_token: Option<&str>,
    ) -> LocalityResult<DriveFileList> {
        let _ = page_token;
        Err(LocalityError::Unsupported(
            "google drive client does not support account-wide Google Docs discovery",
        ))
    }
    #[deprecated(note = "workspace-folder discovery is retired")]
    fn list_workspace_folders_by_name(
        &self,
        _name: &str,
        _page_token: Option<&str>,
    ) -> LocalityResult<DriveFileList> {
        Err(LocalityError::Unsupported(
            "workspace-folder discovery is retired",
        ))
    }
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
        fields: format!("nextPageToken, incompleteSearch, files({DRIVE_FILE_FIELDS})"),
        page_token: page_token.map(str::to_string),
    }
}

pub fn drive_accessible_google_docs_query(page_token: Option<&str>) -> DriveListQuery {
    DriveListQuery {
        q: format!("mimeType = '{DRIVE_GOOGLE_DOC_MIME_TYPE}' and trashed = false"),
        fields: format!("nextPageToken, incompleteSearch, files({DRIVE_FILE_FIELDS})"),
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
            vec![
                ("fields".to_string(), DRIVE_FILE_FIELDS.to_string()),
                ("supportsAllDrives".to_string(), "true".to_string()),
            ],
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
            ("includeItemsFromAllDrives".to_string(), "true".to_string()),
            ("supportsAllDrives".to_string(), "true".to_string()),
        ];
        if let Some(page_token) = query.page_token {
            query_pairs.push(("pageToken".to_string(), page_token));
        }
        self.get_json(format!("{}/files", self.drive_base_url), query_pairs)
    }

    fn list_accessible_google_docs(
        &self,
        page_token: Option<&str>,
    ) -> LocalityResult<DriveFileList> {
        let query = drive_accessible_google_docs_query(page_token);
        let mut query_pairs = vec![
            ("q".to_string(), query.q),
            ("fields".to_string(), query.fields),
            ("corpora".to_string(), "allDrives".to_string()),
            ("includeItemsFromAllDrives".to_string(), "true".to_string()),
            ("supportsAllDrives".to_string(), "true".to_string()),
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
            vec![
                ("fields".to_string(), DRIVE_FILE_FIELDS.to_string()),
                ("supportsAllDrives".to_string(), "true".to_string()),
            ],
        )
    }

    fn update_file(
        &self,
        file_id: &str,
        request: DriveUpdateFileRequest,
    ) -> LocalityResult<DriveFile> {
        let mut query = vec![
            ("fields".to_string(), DRIVE_FILE_FIELDS.to_string()),
            ("supportsAllDrives".to_string(), "true".to_string()),
        ];
        if let Some(add_parents) = request.add_parents.clone() {
            query.push(("addParents".to_string(), add_parents));
        }
        if let Some(remove_parents) = request.remove_parents.clone() {
            query.push(("removeParents".to_string(), remove_parents));
        }
        self.patch_json(
            format!("{}/files/{file_id}", self.drive_base_url),
            &request,
            query,
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

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, mpsc};
    use std::thread;

    use locality_connector::{
        Connector, PortableBootstrapRequest, PortableFetchReason, PortableFetchRequest,
        PortableSourceScope,
    };
    use locality_core::model::RemoteId;
    use locality_core::portable::SourceConnectionId;

    use super::{
        DriveListQuery, GoogleDocsApi, GoogleDriveApi, HttpGoogleApiClient,
        drive_accessible_google_docs_query, drive_children_query, google_docs_batch_update_url,
    };
    use crate::connector::{GoogleDocsConfig, GoogleDocsConnector};
    use crate::docs_dto::{BatchUpdateDocumentRequest, GoogleDocument};
    use crate::drive_dto::{DriveCreateFileRequest, DriveUpdateFileRequest};

    #[test]
    fn accessible_google_docs_query_lists_untrashed_documents() {
        let query = drive_accessible_google_docs_query(Some("cursor-1"));

        assert_eq!(
            query,
            DriveListQuery {
                q: "mimeType = 'application/vnd.google-apps.document' and trashed = false".to_string(),
                fields: "nextPageToken, incompleteSearch, files(id, name, mimeType, parents, modifiedTime, version, trashed)".to_string(),
                page_token: Some("cursor-1".to_string()),
            }
        );
    }

    #[test]
    fn drive_children_query_filters_immediate_untrashed_children() {
        let query = drive_children_query("folder-1", Some("cursor-1"));

        assert_eq!(
            query,
            DriveListQuery {
                q: "'folder-1' in parents and trashed = false".to_string(),
                fields: "nextPageToken, incompleteSearch, files(id, name, mimeType, parents, modifiedTime, version, trashed)".to_string(),
                page_token: Some("cursor-1".to_string()),
            }
        );
    }

    #[test]
    fn all_drives_list_requests_and_decodes_incomplete_search() {
        let (base_url, requests, server) =
            spawn_drive_server([r#"{"incompleteSearch":true,"files":[]}"#.to_string()]);
        let client =
            HttpGoogleApiClient::with_base_urls("access-token", base_url, "http://unused.test");

        let page = client
            .list_accessible_google_docs(None)
            .expect("list response");

        let requests = requests.recv().expect("requests");
        server.join().expect("server exits");
        assert!(page.incomplete_search);
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains("corpora=allDrives"));
        assert!(requests[0].contains("incompleteSearch"));
    }

    #[test]
    fn create_file_requests_shared_drive_support() {
        let (base_url, requests, server) =
            spawn_drive_server([drive_document_response("doc-1", "shared-root")]);
        let client =
            HttpGoogleApiClient::with_base_urls("access-token", base_url, "http://unused.test");

        client
            .create_file(DriveCreateFileRequest::google_doc(
                "Shared Doc",
                "shared-root",
            ))
            .expect("create response");

        let requests = requests.recv().expect("requests");
        server.join().expect("server exits");
        assert_eq!(requests.len(), 1);
        assert_drive_flags(&requests[0], "/files?");
    }

    #[test]
    fn update_file_requests_shared_drive_support() {
        let (base_url, requests, server) =
            spawn_drive_server([drive_document_response("doc-1", "shared-root")]);
        let client =
            HttpGoogleApiClient::with_base_urls("access-token", base_url, "http://unused.test");

        client
            .update_file("doc-1", DriveUpdateFileRequest::rename("Renamed"))
            .expect("update response");

        let requests = requests.recv().expect("requests");
        server.join().expect("server exits");
        assert_eq!(requests.len(), 1);
        assert_drive_flags(&requests[0], "/files/doc-1?");
    }

    #[test]
    fn docs_batch_update_url_targets_document_resource() {
        assert_eq!(
            google_docs_batch_update_url("https://docs.googleapis.com", "doc-1"),
            "https://docs.googleapis.com/v1/documents/doc-1:batchUpdate"
        );
    }

    #[test]
    fn shared_drive_scoped_portable_bootstrap_sends_drive_flags() {
        let (base_url, requests, server) = spawn_drive_server([
            drive_folder_response("shared-root"),
            r#"{"files":[]}"#.to_string(),
        ]);
        let connector = GoogleDocsConnector::with_apis(
            GoogleDocsConfig::new("access-token"),
            Arc::new(HttpGoogleApiClient::with_base_urls(
                "access-token",
                base_url,
                "http://unused.test",
            )),
            Arc::new(FakeDocs),
        )
        .with_portable_workspace_folder_id(RemoteId::new("shared-root"));

        connector
            .bootstrap_portable(PortableBootstrapRequest {
                source_connection_id: SourceConnectionId::new("hosted-google-docs"),
                scope: PortableSourceScope::explicit_roots([RemoteId::new("shared-root")]),
                checkpoint: None,
                max_changes: 100,
            })
            .expect("portable bootstrap");

        let requests = requests.recv().expect("requests");
        server.join().expect("server exits");
        assert_eq!(requests.len(), 2);
        assert_drive_flags(&requests[0], "/files/shared-root");
        assert_drive_flags(&requests[1], "/files?");
        assert!(requests[1].contains("includeItemsFromAllDrives=true"));
        assert!(requests[1].contains("incompleteSearch"));
    }

    #[test]
    fn shared_drive_scoped_portable_fetch_sends_drive_flags() {
        let (base_url, requests, server) = spawn_drive_server([
            drive_folder_response("shared-root"),
            drive_document_response("doc-1", "shared-root"),
        ]);
        let connector = GoogleDocsConnector::with_apis(
            GoogleDocsConfig::new("access-token"),
            Arc::new(HttpGoogleApiClient::with_base_urls(
                "access-token",
                base_url,
                "http://unused.test",
            )),
            Arc::new(FakeDocs),
        )
        .with_portable_workspace_folder_id(RemoteId::new("shared-root"));

        connector
            .fetch_portable(PortableFetchRequest {
                source_connection_id: SourceConnectionId::new("hosted-google-docs"),
                remote_id: RemoteId::new("doc-1"),
                reason: PortableFetchReason::Bootstrap,
            })
            .expect("portable fetch");

        let requests = requests.recv().expect("requests");
        server.join().expect("server exits");
        assert_eq!(requests.len(), 2);
        assert_drive_flags(&requests[0], "/files/shared-root");
        assert_drive_flags(&requests[1], "/files/doc-1");
    }

    #[derive(Debug)]
    struct FakeDocs;

    impl GoogleDocsApi for FakeDocs {
        fn get_document(
            &self,
            _document_id: &str,
        ) -> locality_core::LocalityResult<GoogleDocument> {
            Ok(GoogleDocument {
                document_id: "doc-1".to_string(),
                revision_id: Some("revision-1".to_string()),
                ..GoogleDocument::default()
            })
        }

        fn batch_update_document(
            &self,
            _document_id: &str,
            _request: BatchUpdateDocumentRequest,
        ) -> locality_core::LocalityResult<GoogleDocument> {
            unreachable!("portable tests do not update documents")
        }
    }

    fn assert_drive_flags(request: &str, path: &str) {
        let target = request.split_whitespace().nth(1).expect("request target");
        assert!(target.starts_with(path), "unexpected target: {target}");
        assert!(
            target.contains("supportsAllDrives=true"),
            "missing supportsAllDrives: {target}"
        );
    }

    fn drive_folder_response(id: &str) -> String {
        format!(
            r#"{{"id":"{id}","name":"Shared Root","mimeType":"application/vnd.google-apps.folder","parents":[],"modifiedTime":"2026-08-29T00:00:00Z","version":"1","trashed":false}}"#
        )
    }

    fn drive_document_response(id: &str, parent: &str) -> String {
        format!(
            r#"{{"id":"{id}","name":"Shared Doc","mimeType":"application/vnd.google-apps.document","parents":["{parent}"],"modifiedTime":"2026-08-29T00:00:00Z","version":"1","trashed":false}}"#
        )
    }

    fn spawn_drive_server(
        responses: impl IntoIterator<Item = String>,
    ) -> (String, mpsc::Receiver<Vec<String>>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let responses = responses.into_iter().collect::<Vec<_>>();
        let (requests_tx, requests_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut requests = Vec::with_capacity(responses.len());
            for body in responses {
                let (mut stream, _) = listener.accept().expect("accept request");
                requests.push(read_http_request(&mut stream));
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write response");
            }
            requests_tx.send(requests).expect("send requests");
        });
        (base_url, requests_rx, server)
    }

    fn read_http_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let bytes_read = stream.read(&mut buffer).expect("read request");
            if bytes_read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..bytes_read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8(request).expect("utf8 request")
    }
}
