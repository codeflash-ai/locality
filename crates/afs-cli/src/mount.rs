//! `afs mount` orchestration.
//!
//! This first real mount command records enough connector configuration for the
//! pull path to build a filesystem projection from a Notion root page.

use std::path::{Path, PathBuf};

use afs_core::model::{MountId, RemoteId};
use afs_store::{MountConfig, MountRepository, StoreError};
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountOptions {
    pub mount_id: MountId,
    pub connector: String,
    pub root: PathBuf,
    pub remote_root_id: Option<RemoteId>,
    pub read_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MountReport {
    pub ok: bool,
    pub command: &'static str,
    pub mount_id: String,
    pub connector: String,
    pub root: String,
    pub remote_root_id: Option<String>,
    pub read_only: bool,
}

pub fn run_mount<S>(store: &mut S, options: MountOptions) -> Result<MountReport, MountError>
where
    S: MountRepository,
{
    let root = absolute_path(&options.root)?;
    std::fs::create_dir_all(&root).map_err(|error| MountError::CreateRoot {
        path: root.clone(),
        message: error.to_string(),
    })?;

    let mut mount = MountConfig::new(options.mount_id.clone(), options.connector.clone(), &root)
        .read_only(options.read_only);
    if let Some(remote_root_id) = options.remote_root_id.clone() {
        mount = mount.with_remote_root_id(remote_root_id);
    }

    store.save_mount(mount).map_err(MountError::Store)?;

    Ok(MountReport {
        ok: true,
        command: "mount",
        mount_id: options.mount_id.0,
        connector: options.connector,
        root: root.display().to_string(),
        remote_root_id: options.remote_root_id.map(|remote_id| remote_id.0),
        read_only: options.read_only,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MountError {
    CreateRoot { path: PathBuf, message: String },
    CurrentDir(String),
    Store(StoreError),
}

impl MountError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::CreateRoot { .. } => "create_mount_root_failed",
            Self::CurrentDir(_) => "current_dir_failed",
            Self::Store(_) => "store_error",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::CreateRoot { path, message } => {
                format!(
                    "failed to create mount root `{}`: {message}",
                    path.display()
                )
            }
            Self::CurrentDir(message) => format!("failed to resolve current directory: {message}"),
            Self::Store(error) => error.to_string(),
        }
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, MountError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| MountError::CurrentDir(error.to_string()))
    }
}
