use std::sync::OnceLock;
use std::time::Duration;

use locality_core::{LocalityError, LocalityResult};
use reqwest::blocking::Client;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::dto::{
    GmailDraft, GmailDraftCreateRequest, GmailDraftSendRequest, GmailMessage, GmailMessageList,
};

pub const DEFAULT_GMAIL_API_BASE_URL: &str = "https://gmail.googleapis.com/gmail/v1";
const GMAIL_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

static REQWEST_CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();

pub trait GmailApi: std::fmt::Debug + Send + Sync {
    fn list_messages(
        &self,
        label_id: &str,
        max_results: u32,
        page_token: Option<&str>,
    ) -> LocalityResult<GmailMessageList>;
    fn get_message_metadata(&self, message_id: &str) -> LocalityResult<GmailMessage>;
    fn get_message_full(&self, message_id: &str) -> LocalityResult<GmailMessage>;
    fn create_draft(&self, request: GmailDraftCreateRequest) -> LocalityResult<GmailDraft>;
    fn send_draft(&self, request: GmailDraftSendRequest) -> LocalityResult<GmailMessage>;
}

#[derive(Clone, Debug)]
pub struct HttpGmailApiClient {
    access_token: String,
    base_url: String,
    client: Client,
}

impl HttpGmailApiClient {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self::with_base_url(access_token, DEFAULT_GMAIL_API_BASE_URL)
    }

    pub fn with_base_url(access_token: impl Into<String>, base_url: impl Into<String>) -> Self {
        ensure_reqwest_crypto_provider();
        let client = Client::builder()
            .timeout(GMAIL_HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            access_token: access_token.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client,
        }
    }

    fn get_json<T>(&self, path: &str, query: Vec<(String, String)>) -> LocalityResult<T>
    where
        T: DeserializeOwned,
    {
        let mut request = self
            .client
            .get(format!("{}{}", self.base_url, path))
            .bearer_auth(&self.access_token);
        for (key, value) in query {
            request = request.query(&[(key.as_str(), value.as_str())]);
        }
        decode_response(request.send(), "gmail api GET")
    }

    fn post_json<T, B>(&self, path: &str, body: &B) -> LocalityResult<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        decode_response(
            self.client
                .post(format!("{}{}", self.base_url, path))
                .bearer_auth(&self.access_token)
                .json(body)
                .send(),
            "gmail api POST",
        )
    }
}

impl GmailApi for HttpGmailApiClient {
    fn list_messages(
        &self,
        label_id: &str,
        max_results: u32,
        page_token: Option<&str>,
    ) -> LocalityResult<GmailMessageList> {
        let mut query = vec![
            ("labelIds".to_string(), label_id.to_string()),
            ("maxResults".to_string(), max_results.to_string()),
        ];
        if let Some(page_token) = page_token {
            query.push(("pageToken".to_string(), page_token.to_string()));
        }
        self.get_json("/users/me/messages", query)
    }

    fn get_message_metadata(&self, message_id: &str) -> LocalityResult<GmailMessage> {
        self.get_json(
            &format!("/users/me/messages/{message_id}"),
            vec![
                ("format".to_string(), "metadata".to_string()),
                (
                    "metadataHeaders".to_string(),
                    "From,To,Cc,Bcc,Subject,Date,Message-ID".to_string(),
                ),
            ],
        )
    }

    fn get_message_full(&self, message_id: &str) -> LocalityResult<GmailMessage> {
        self.get_json(
            &format!("/users/me/messages/{message_id}"),
            vec![("format".to_string(), "full".to_string())],
        )
    }

    fn create_draft(&self, request: GmailDraftCreateRequest) -> LocalityResult<GmailDraft> {
        self.post_json("/users/me/drafts", &request)
    }

    fn send_draft(&self, request: GmailDraftSendRequest) -> LocalityResult<GmailMessage> {
        self.post_json("/users/me/drafts/send", &request)
    }
}

fn decode_response<T>(
    response: Result<reqwest::blocking::Response, reqwest::Error>,
    context: &str,
) -> LocalityResult<T>
where
    T: DeserializeOwned,
{
    let response =
        response.map_err(|error| LocalityError::Io(format!("{context} failed: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .unwrap_or_else(|error| format!("<failed to read error body: {error}>"));
        return Err(LocalityError::Io(format!(
            "{context} returned HTTP {status}: {body}"
        )));
    }
    response
        .json()
        .map_err(|error| LocalityError::Io(format!("{context} response decode failed: {error}")))
}

fn ensure_reqwest_crypto_provider() {
    REQWEST_CRYPTO_PROVIDER.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
