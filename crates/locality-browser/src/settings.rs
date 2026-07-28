use std::path::PathBuf;

use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserMountSettings {
    #[serde(default)]
    pub browser: BrowserSettings,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserSettings {
    #[serde(default)]
    pub capture_root: String,
}

impl BrowserMountSettings {
    pub fn with_capture_root(capture_root: impl Into<String>) -> Self {
        Self {
            browser: BrowserSettings {
                capture_root: capture_root.into(),
            },
        }
    }

    pub fn capture_root(&self) -> LocalityResult<PathBuf> {
        let trimmed = self.browser.capture_root.trim();
        if trimmed.is_empty() {
            return Err(LocalityError::Io(
                "Browser mount settings require `browser.capture_root`".to_string(),
            ));
        }
        Ok(PathBuf::from(trimmed))
    }

    pub fn from_json(value: &str) -> LocalityResult<Self> {
        if value.trim().is_empty() {
            return Err(LocalityError::Io(
                "Browser mount settings JSON is empty".to_string(),
            ));
        }
        serde_json::from_str(value).map_err(|error| {
            LocalityError::Io(format!("Browser mount settings JSON is invalid: {error}"))
        })
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}
