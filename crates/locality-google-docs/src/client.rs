use std::sync::OnceLock;
use std::time::Duration;

use locality_core::{LocalityError, LocalityResult};
use reqwest::blocking::Client;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::docs_dto::{BatchUpdateDocumentRequest, CreateDocumentRequest, GoogleDocument};

pub const DEFAULT_GOOGLE_DOCS_API_BASE_URL: &str = "https://docs.googleapis.com";
const GOOGLE_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
static REQWEST_CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();

/// Picker selection is persisted locally; this connector never enumerates Drive metadata.
pub trait GoogleDocsApi: std::fmt::Debug + Send + Sync {
    fn get_document(&self, document_id: &str) -> LocalityResult<GoogleDocument>;
    fn create_document(&self, title: &str) -> LocalityResult<GoogleDocument>;
    fn batch_update_document(
        &self,
        document_id: &str,
        request: BatchUpdateDocumentRequest,
    ) -> LocalityResult<GoogleDocument>;
}

pub fn google_docs_batch_update_url(base_url: &str, document_id: &str) -> String {
    format!(
        "{}/v1/documents/{document_id}:batchUpdate",
        base_url.trim_end_matches('/')
    )
}

#[derive(Clone, Debug)]
pub struct HttpGoogleApiClient {
    access_token: String,
    docs_base_url: String,
    client: Client,
}

impl HttpGoogleApiClient {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self::with_base_url(access_token, DEFAULT_GOOGLE_DOCS_API_BASE_URL)
    }
    pub fn with_base_url(
        access_token: impl Into<String>,
        docs_base_url: impl Into<String>,
    ) -> Self {
        ensure_reqwest_crypto_provider();
        let client = Client::builder()
            .timeout(GOOGLE_HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            access_token: access_token.into(),
            docs_base_url: docs_base_url.into().trim_end_matches('/').to_string(),
            client,
        }
    }
    fn get_json<T: DeserializeOwned>(&self, url: String) -> LocalityResult<T> {
        decode_response(
            self.client.get(url).bearer_auth(&self.access_token).send(),
            "google docs GET",
        )
    }
    fn post_json<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        url: String,
        body: &B,
    ) -> LocalityResult<T> {
        decode_response(
            self.client
                .post(url)
                .bearer_auth(&self.access_token)
                .json(body)
                .send(),
            "google docs POST",
        )
    }
}

impl GoogleDocsApi for HttpGoogleApiClient {
    fn get_document(&self, document_id: &str) -> LocalityResult<GoogleDocument> {
        self.get_json(format!("{}/v1/documents/{document_id}", self.docs_base_url))
    }
    fn create_document(&self, title: &str) -> LocalityResult<GoogleDocument> {
        self.post_json(
            format!("{}/v1/documents", self.docs_base_url),
            &CreateDocumentRequest {
                title: title.to_string(),
            },
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
        )
    }
}

fn decode_response<T: DeserializeOwned>(
    response: Result<reqwest::blocking::Response, reqwest::Error>,
    operation: &'static str,
) -> LocalityResult<T> {
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
    use super::google_docs_batch_update_url;
    #[test]
    fn docs_batch_update_url_targets_document_resource() {
        assert_eq!(
            google_docs_batch_update_url("https://docs.googleapis.com", "doc-1"),
            "https://docs.googleapis.com/v1/documents/doc-1:batchUpdate"
        );
    }
}
