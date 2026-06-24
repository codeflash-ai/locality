//! Compatibility contracts for durable Locality state.
//!
//! The SQLite schema version tells us whether the physical database shape is
//! readable. Component versions tell us whether subsystem-specific durable
//! meanings are readable, migratable, or too new for this binary.

use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateCompatibilityStatus {
    Ready,
    Migratable,
    NeedsUpdate,
    Incompatible,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateCompatibilityIssue {
    OlderSchema {
        found: i64,
        current: i64,
    },
    NewerSchema {
        found: i64,
        supported: i64,
    },
    MissingComponent {
        component_id: String,
    },
    OlderComponent {
        component_id: String,
        found: i64,
        current: i64,
    },
    NewerComponent {
        component_id: String,
        found: i64,
        supported: i64,
    },
    ComponentRequiresNewerReader {
        component_id: String,
        min_reader_version: i64,
        supported: i64,
    },
    UnknownRequiredComponent {
        component_id: String,
        version: i64,
    },
    InvalidMetadata {
        message: String,
    },
}

impl StateCompatibilityIssue {
    pub fn status(&self) -> StateCompatibilityStatus {
        match self {
            Self::OlderSchema { .. }
            | Self::MissingComponent { .. }
            | Self::OlderComponent { .. } => StateCompatibilityStatus::Migratable,
            Self::NewerSchema { .. }
            | Self::NewerComponent { .. }
            | Self::ComponentRequiresNewerReader { .. }
            | Self::UnknownRequiredComponent { .. } => StateCompatibilityStatus::NeedsUpdate,
            Self::InvalidMetadata { .. } => StateCompatibilityStatus::Incompatible,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateCompatibilityReport {
    pub db_path: PathBuf,
    pub db_exists: bool,
    pub schema_version: Option<i64>,
    pub current_schema_version: i64,
    pub status: StateCompatibilityStatus,
    pub issues: Vec<StateCompatibilityIssue>,
}

impl StateCompatibilityReport {
    pub fn ready(db_path: PathBuf, db_exists: bool, current_schema_version: i64) -> Self {
        Self {
            db_path,
            db_exists,
            schema_version: if db_exists {
                Some(current_schema_version)
            } else {
                None
            },
            current_schema_version,
            status: StateCompatibilityStatus::Ready,
            issues: Vec::new(),
        }
    }

    pub fn from_issues(
        db_path: PathBuf,
        db_exists: bool,
        schema_version: Option<i64>,
        current_schema_version: i64,
        issues: Vec<StateCompatibilityIssue>,
    ) -> Self {
        let status = issues
            .iter()
            .map(StateCompatibilityIssue::status)
            .fold(StateCompatibilityStatus::Ready, merge_status);
        Self {
            db_path,
            db_exists,
            schema_version,
            current_schema_version,
            status,
            issues,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateComponentRecord {
    pub component_id: String,
    pub component_kind: String,
    pub version: i64,
    pub min_reader_version: i64,
    pub required: bool,
    pub rebuildable: bool,
    pub data_json: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateComponentDefinition {
    pub component_id: &'static str,
    pub component_kind: &'static str,
    pub current_version: i64,
    pub min_reader_version: i64,
    pub required: bool,
    pub rebuildable: bool,
    pub data_json: &'static str,
}

fn merge_status(
    left: StateCompatibilityStatus,
    right: StateCompatibilityStatus,
) -> StateCompatibilityStatus {
    use StateCompatibilityStatus::{Incompatible, Migratable, NeedsUpdate, Ready};
    match (left, right) {
        (Incompatible, _) | (_, Incompatible) => Incompatible,
        (NeedsUpdate, _) | (_, NeedsUpdate) => NeedsUpdate,
        (Migratable, _) | (_, Migratable) => Migratable,
        (Ready, Ready) => Ready,
    }
}
