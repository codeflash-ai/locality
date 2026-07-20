//! Store-specific errors.
//!
//! Repository methods return `StoreError` instead of flattening missing state
//! into strings. Callers can still convert these errors into core `LocalityError`
//! when implementing core traits such as `JournalStore`.

use std::fmt::{Display, Formatter};
use std::path::PathBuf;

use locality_core::LocalityError;
use locality_core::journal::PushId;
use locality_core::model::{MountId, RemoteId};

pub type StoreResult<T> = Result<T, StoreError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreError {
    MountMissing(MountId),
    EntityMissing {
        mount_id: MountId,
        remote_id: RemoteId,
    },
    EntityPathMissing {
        mount_id: MountId,
        path: PathBuf,
    },
    DuplicateEntityPath {
        mount_id: MountId,
        path: PathBuf,
    },
    ShadowMissing {
        mount_id: MountId,
        entity_id: RemoteId,
    },
    JournalMissing(PushId),
    JournalAlreadyExists(PushId),
    InvalidState(String),
    NotImplemented(&'static str),
    SchemaVersion {
        found: i64,
        supported: i64,
    },
    StateCompatibility(String),
    Database(String),
    Json(String),
    Io(String),
}

impl Display for StoreError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MountMissing(mount_id) => write!(f, "mount `{}` was not found", mount_id.0),
            Self::EntityMissing {
                mount_id,
                remote_id,
            } => write!(
                f,
                "entity `{}` was not found in mount `{}`",
                remote_id.0, mount_id.0
            ),
            Self::EntityPathMissing { mount_id, path } => write!(
                f,
                "path `{}` was not found in mount `{}`",
                path.display(),
                mount_id.0
            ),
            Self::DuplicateEntityPath { mount_id, path } => write!(
                f,
                "path `{}` is already mapped in mount `{}`",
                path.display(),
                mount_id.0
            ),
            Self::ShadowMissing {
                mount_id,
                entity_id,
            } => write!(
                f,
                "shadow for entity `{}` was not found in mount `{}`",
                entity_id.0, mount_id.0
            ),
            Self::JournalMissing(push_id) => {
                write!(f, "journal entry `{}` was not found", push_id.0)
            }
            Self::JournalAlreadyExists(push_id) => {
                write!(f, "journal entry `{}` already exists", push_id.0)
            }
            Self::InvalidState(message) => write!(f, "invalid state: {message}"),
            Self::NotImplemented(feature) => write!(f, "not implemented: {feature}"),
            Self::SchemaVersion { found, supported } => write!(
                f,
                "unsupported schema version {found}; this binary supports up to {supported}"
            ),
            Self::StateCompatibility(message) => write!(f, "state compatibility error: {message}"),
            Self::Database(message) => write!(f, "database error: {message}"),
            Self::Json(message) => write!(f, "json error: {message}"),
            Self::Io(message) => write!(f, "io error: {message}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value.to_string())
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Database(value.to_string())
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value.to_string())
    }
}

impl From<StoreError> for LocalityError {
    fn from(value: StoreError) -> Self {
        match value {
            StoreError::InvalidState(message) => Self::InvalidState(message),
            StoreError::NotImplemented(feature) => Self::NotImplemented(feature),
            StoreError::SchemaVersion { found, supported } => Self::Io(format!(
                "unsupported schema version {found}; this binary supports up to {supported}"
            )),
            StoreError::StateCompatibility(message) => Self::Io(message),
            StoreError::Database(message) | StoreError::Json(message) => Self::Io(message),
            StoreError::Io(message) => Self::Io(message),
            other => Self::Io(other.to_string()),
        }
    }
}
