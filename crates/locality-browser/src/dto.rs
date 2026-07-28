use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserSession {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub browser: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub captured_at: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub windows: Vec<BrowserWindow>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserWindow {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub tabs: Vec<BrowserTab>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserTab {
    pub id: String,
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub captured_at: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub discarded: bool,
    #[serde(default)]
    pub markdown: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub selected_text: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub html_path: Option<String>,
    #[serde(default)]
    pub screenshot_path: Option<String>,
    #[serde(default)]
    pub favicon_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserTabContext {
    pub session: BrowserSession,
    pub window: BrowserWindow,
    pub tab: BrowserTab,
    pub index: usize,
}
