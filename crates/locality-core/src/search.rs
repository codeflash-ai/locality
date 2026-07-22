//! Connector-neutral search metadata.
//!
//! Connectors can include this payload under `loc_search` in persisted remote
//! observation metadata. The store treats it as rebuildable search-index input,
//! not as source-of-truth connector state.

use serde::{Deserialize, Serialize};

pub const RAW_SEARCH_METADATA_KEY: &str = "loc_search";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchMetadata {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metadata_text: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
}

impl SearchMetadata {
    pub fn is_empty(&self) -> bool {
        self.metadata_text.is_empty() && self.aliases.is_empty() && self.source_url.is_none()
    }

    pub fn push_metadata_text(&mut self, value: impl AsRef<str>) {
        push_non_empty(&mut self.metadata_text, value.as_ref());
    }

    pub fn push_alias(&mut self, value: impl AsRef<str>) {
        push_non_empty(&mut self.aliases, value.as_ref());
    }

    pub fn set_source_url(&mut self, value: impl Into<String>) {
        let value = value.into();
        if !value.trim().is_empty() {
            self.source_url = Some(value);
        }
    }
}

fn push_non_empty(values: &mut Vec<String>, value: &str) {
    let value = value.trim();
    if !value.is_empty() {
        values.push(value.to_string());
    }
}
