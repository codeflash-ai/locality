use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::credentials::{CredentialError, open_credential_store};
use crate::repository::ConnectionRepository;
use crate::sqlite::SqliteStateStore;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LocalStateResetStorageReport {
    pub state_root: String,
    pub deleted_secret_refs: Vec<String>,
    pub credential_errors: Vec<LocalStateResetCredentialError>,
    pub removed_state_entries: Vec<String>,
    pub preserved_state_entries: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LocalStateResetCredentialError {
    pub secret_ref: String,
    pub code: String,
    pub message: String,
}

#[derive(Debug)]
pub enum LocalStateResetError {
    Io { path: PathBuf, message: String },
}

impl Display for LocalStateResetError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, message } => {
                write!(f, "could not reset `{}`: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for LocalStateResetError {}

pub fn reset_locality_state_storage(
    state_root: &Path,
) -> Result<LocalStateResetStorageReport, LocalStateResetError> {
    let secret_refs = connection_secret_refs(state_root);
    let (deleted_secret_refs, credential_errors) =
        delete_connection_secrets(state_root, &secret_refs);
    let clear = clear_state_root_contents(state_root)?;
    Ok(LocalStateResetStorageReport {
        state_root: state_root.display().to_string(),
        deleted_secret_refs,
        credential_errors,
        removed_state_entries: clear.removed_state_entries,
        preserved_state_entries: clear.preserved_state_entries,
    })
}

pub fn connection_secret_refs(state_root: &Path) -> Vec<String> {
    let mut refs = vec![
        "connection:notion-default".to_string(),
        "connection:notion-main".to_string(),
        "connection:notion-test".to_string(),
    ];
    if state_root.join("state.sqlite3").exists()
        && let Ok(store) = SqliteStateStore::open(state_root.to_path_buf())
        && let Ok(connections) = store.list_connections()
    {
        refs.extend(
            connections
                .into_iter()
                .filter(|connection| !connection.secret_ref.is_empty())
                .map(|connection| connection.secret_ref),
        );
    }
    refs.sort();
    refs.dedup();
    refs
}

fn delete_connection_secrets(
    state_root: &Path,
    secret_refs: &[String],
) -> (Vec<String>, Vec<LocalStateResetCredentialError>) {
    let credentials = open_credential_store(state_root);
    let mut deleted = Vec::new();
    let mut errors = Vec::new();
    for secret_ref in secret_refs {
        match credentials.delete(secret_ref) {
            Ok(()) => deleted.push(secret_ref.clone()),
            Err(error) => errors.push(credential_error(secret_ref, error)),
        }
    }
    (deleted, errors)
}

fn credential_error(secret_ref: &str, error: CredentialError) -> LocalStateResetCredentialError {
    LocalStateResetCredentialError {
        secret_ref: secret_ref.to_string(),
        code: error.code().to_string(),
        message: error.to_string(),
    }
}

struct StateRootClearReport {
    removed_state_entries: Vec<String>,
    preserved_state_entries: Vec<String>,
}

fn clear_state_root_contents(
    state_root: &Path,
) -> Result<StateRootClearReport, LocalStateResetError> {
    if !state_root.exists() {
        fs::create_dir_all(state_root).map_err(|error| reset_io_error(state_root, error))?;
        return Ok(StateRootClearReport {
            removed_state_entries: Vec::new(),
            preserved_state_entries: Vec::new(),
        });
    }

    let mut removed_state_entries = Vec::new();
    let mut preserved_state_entries = Vec::new();
    let entries = fs::read_dir(state_root).map_err(|error| reset_io_error(state_root, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| reset_io_error(state_root, error))?;
        let path = entry.path();
        let display_name = state_entry_name(&path);
        if should_preserve_state_reset_entry(&path) {
            preserved_state_entries.push(display_name);
            continue;
        }
        remove_path_if_exists(&path)?;
        removed_state_entries.push(display_name);
    }
    removed_state_entries.sort();
    preserved_state_entries.sort();
    Ok(StateRootClearReport {
        removed_state_entries,
        preserved_state_entries,
    })
}

fn state_entry_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| path.display().to_string())
}

fn should_preserve_state_reset_entry(path: &Path) -> bool {
    #[cfg(target_os = "windows")]
    {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        matches!(
            name.to_ascii_lowercase().as_str(),
            "locality.exe"
                | "locality-desktop.exe"
                | "loc.exe"
                | "localityd.exe"
                | "locality-cloud-files.exe"
                | "uninstall.exe"
                | "bin"
        )
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        false
    }
}

fn remove_path_if_exists(path: &Path) -> Result<(), LocalStateResetError> {
    if !path.exists() && !path.is_symlink() {
        return Ok(());
    }
    if path.is_dir() && !path.is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .map_err(|error| reset_io_error(path, error))
}

fn reset_io_error(path: &Path, error: std::io::Error) -> LocalStateResetError {
    LocalStateResetError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}
