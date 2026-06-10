//! Store-specific errors.
//!
//! Repository methods return `StoreError` instead of flattening missing state
//! into strings. Callers can still convert these errors into core `AfsError`
//! when implementing core traits such as `JournalStore`.

use std::fmt::{Display, Formatter};
use std::path::PathBuf;

use afs_core::AfsError;
use afs_core::journal::PushId;
use afs_core::model::{MountId, RemoteId};

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
    NotImplemented(&'static str),
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
            Self::NotImplemented(feature) => write!(f, "not implemented: {feature}"),
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

impl From<StoreError> for AfsError {
    fn from(value: StoreError) -> Self {
        match value {
            StoreError::NotImplemented(feature) => Self::NotImplemented(feature),
            StoreError::Io(message) => Self::Io(message),
            other => Self::Io(other.to_string()),
        }
    }
}
