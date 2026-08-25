use std::collections::BTreeSet;
use std::path::PathBuf;

use locality_core::validation::ValidationIssue;
use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Deserializer, Serialize};

const GOOGLE_DOCS_SETTINGS_VERSION: u32 = 2;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GoogleDocsMountSettings {
    pub google_docs: GoogleDocsSelection,
}

impl<'de> Deserialize<'de> for GoogleDocsMountSettings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawGoogleDocsMountSettings {
            google_docs: GoogleDocsSelection,
        }

        let raw = RawGoogleDocsMountSettings::deserialize(deserializer)?;
        let mut settings = Self {
            google_docs: raw.google_docs,
        };
        settings
            .normalize(true)
            .map_err(|error| serde::de::Error::custom(error_message(error)))?;
        Ok(settings)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleDocsSelection {
    pub version: u32,
    pub document_ids: Vec<String>,
}

impl GoogleDocsMountSettings {
    pub fn from_document_ids<I, S>(document_ids: I) -> LocalityResult<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let document_ids = document_ids
            .into_iter()
            .map(|id| id.as_ref().to_string())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut settings = Self {
            google_docs: GoogleDocsSelection {
                version: GOOGLE_DOCS_SETTINGS_VERSION,
                document_ids,
            },
        };
        settings.normalize(false)?;
        Ok(settings)
    }

    pub fn from_json(value: &str) -> LocalityResult<Self> {
        serde_json::from_str(value).map_err(|error| {
            settings_validation(format!(
                "Google Docs mount settings JSON is invalid: {error}"
            ))
        })
    }

    pub fn to_json(&self) -> LocalityResult<String> {
        let mut canonical = self.clone();
        canonical.normalize(true)?;
        serde_json::to_string(&canonical).map_err(|error| {
            LocalityError::Io(format!("Google Docs settings encode failed: {error}"))
        })
    }

    pub fn document_ids(&self) -> &[String] {
        &self.google_docs.document_ids
    }

    fn normalize(&mut self, reject_duplicates: bool) -> LocalityResult<()> {
        if self.google_docs.version != GOOGLE_DOCS_SETTINGS_VERSION {
            return Err(settings_validation(format!(
                "Google Docs mount settings version must be {GOOGLE_DOCS_SETTINGS_VERSION}"
            )));
        }
        if self.google_docs.document_ids.is_empty() {
            return Err(settings_validation(
                "Google Docs mount settings must include at least one selected document",
            ));
        }
        if self.google_docs.document_ids.iter().any(|id| id.is_empty()) {
            return Err(settings_validation(
                "Google Docs selected document IDs must not be empty",
            ));
        }

        let document_ids = self
            .google_docs
            .document_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if reject_duplicates && document_ids.len() != self.google_docs.document_ids.len() {
            return Err(settings_validation(
                "Google Docs selected document IDs must be unique",
            ));
        }
        self.google_docs.document_ids = document_ids.into_iter().collect();
        Ok(())
    }
}

fn settings_validation(message: impl Into<String>) -> LocalityError {
    LocalityError::Validation(vec![ValidationIssue::new(
        "google_docs_mount_settings_invalid",
        PathBuf::new(),
        Some(1),
        message,
        Some("select one or more Google Docs documents for this mount".to_string()),
    )])
}

fn error_message(error: LocalityError) -> String {
    match error {
        LocalityError::Validation(issues) => issues
            .into_iter()
            .map(|issue| issue.message)
            .collect::<Vec<_>>()
            .join("; "),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::GoogleDocsMountSettings;

    #[test]
    fn document_ids_are_deduplicated_sorted_and_encoded_stably() {
        let settings = GoogleDocsMountSettings::from_document_ids(["doc-b", "doc-a", "doc-b"])
            .expect("valid document ids");

        assert_eq!(settings.document_ids(), ["doc-a", "doc-b"]);
        assert_eq!(
            settings.to_json().expect("settings json"),
            r#"{"google_docs":{"version":2,"document_ids":["doc-a","doc-b"]}}"#
        );
    }

    #[test]
    fn settings_require_version_two_and_a_non_empty_unique_selection() {
        for value in [
            "{}",
            r#"{"google_docs":{"version":1,"document_ids":["doc-a"]}}"#,
            r#"{"google_docs":{"version":2,"document_ids":[]}}"#,
            r#"{"google_docs":{"version":2,"document_ids":["doc-a","doc-a"]}}"#,
            r#"{"google_docs":{"version":2,"document_ids":[""]}}"#,
            r#"{"google_docs":{"version":2,"document_ids":["doc-a"],"unexpected":true}}"#,
        ] {
            assert!(
                GoogleDocsMountSettings::from_json(value).is_err(),
                "accepted invalid settings {value}"
            );
        }
    }
}
