use std::fmt;
use std::sync::OnceLock;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use locality_core::{LocalityError, LocalityResult};
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use serde::de::DeserializeOwned;

use crate::dto::{ConfluenceCollection, ConfluencePage, ConfluenceSpace};

const CONFLUENCE_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const PAGE_SIZE: u32 = 100;

static REQWEST_CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();

pub trait ConfluenceApi: fmt::Debug + Send + Sync {
    fn list_spaces(&self) -> LocalityResult<Vec<ConfluenceSpace>>;
    fn get_space(&self, space_id: &str) -> LocalityResult<ConfluenceSpace>;
    fn list_pages(&self, space_id: &str) -> LocalityResult<Vec<ConfluencePage>>;
    fn get_page(&self, page_id: &str) -> LocalityResult<ConfluencePage>;
}

#[derive(Clone)]
pub struct HttpConfluenceApiClient {
    email: String,
    api_token: String,
    api_base_url: String,
    client: Client,
}

impl fmt::Debug for HttpConfluenceApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpConfluenceApiClient")
            .field("email", &self.email)
            .field("api_token", &"<redacted>")
            .field("api_base_url", &self.api_base_url)
            .finish_non_exhaustive()
    }
}

impl HttpConfluenceApiClient {
    pub fn new(
        site_url: impl Into<String>,
        email: impl Into<String>,
        api_token: impl Into<String>,
    ) -> Self {
        ensure_reqwest_crypto_provider();
        let client = Client::builder()
            .timeout(CONFLUENCE_HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            email: email.into(),
            api_token: api_token.into(),
            api_base_url: confluence_api_base_url(&site_url.into()),
            client,
        }
    }

    fn get<T>(&self, path: &str, query: &[(&str, String)]) -> LocalityResult<T>
    where
        T: DeserializeOwned,
    {
        let response = self.request(path, query).send().map_err(|error| {
            LocalityError::Io(format!("Confluence API request failed: {error}"))
        })?;
        decode_response(response)
    }

    fn request(&self, path: &str, query: &[(&str, String)]) -> reqwest::blocking::RequestBuilder {
        let credentials = format!("{}:{}", self.email, self.api_token);
        self.client
            .get(format!("{}{}", self.api_base_url, path))
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Basic {}", STANDARD.encode(credentials)),
            )
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, "Locality")
            .query(query)
    }
}

impl ConfluenceApi for HttpConfluenceApiClient {
    fn list_spaces(&self) -> LocalityResult<Vec<ConfluenceSpace>> {
        let collection: ConfluenceCollection<ConfluenceSpace> = self.get(
            "/spaces",
            &[
                ("limit", PAGE_SIZE.to_string()),
                ("status", "current".to_string()),
            ],
        )?;
        let mut spaces = collection.results;
        spaces.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.key.cmp(&right.key))
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(spaces)
    }

    fn get_space(&self, space_id: &str) -> LocalityResult<ConfluenceSpace> {
        self.get(&format!("/spaces/{}", path_segment(space_id)), &[])
    }

    fn list_pages(&self, space_id: &str) -> LocalityResult<Vec<ConfluencePage>> {
        let collection: ConfluenceCollection<ConfluencePage> = self.get(
            &format!("/spaces/{}/pages", path_segment(space_id)),
            &[
                ("limit", PAGE_SIZE.to_string()),
                ("status", "current".to_string()),
            ],
        )?;
        let mut pages = collection.results;
        pages.sort_by(|left, right| {
            left.title
                .to_lowercase()
                .cmp(&right.title.to_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(pages)
    }

    fn get_page(&self, page_id: &str) -> LocalityResult<ConfluencePage> {
        self.get(
            &format!("/pages/{}", path_segment(page_id)),
            &[("body-format", "storage".to_string())],
        )
    }
}

pub fn confluence_api_base_url(site_url: &str) -> String {
    let trimmed = site_url.trim().trim_end_matches('/');
    if trimmed.ends_with("/wiki/api/v2") {
        trimmed.to_string()
    } else if trimmed.ends_with("/wiki") {
        format!("{trimmed}/api/v2")
    } else {
        format!("{trimmed}/wiki/api/v2")
    }
}

fn path_segment(value: &str) -> String {
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
            .map_err(|error| LocalityError::Io(format!("Confluence API decode failed: {error}")));
    }
    if status == StatusCode::NOT_FOUND {
        return Err(LocalityError::RemoteNotFound(
            "Confluence object".to_string(),
        ));
    }
    let body = response.text().unwrap_or_default();
    Err(LocalityError::Io(format!(
        "Confluence API returned HTTP {status}: {body}"
    )))
}

fn ensure_reqwest_crypto_provider() {
    REQWEST_CRYPTO_PROVIDER.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
