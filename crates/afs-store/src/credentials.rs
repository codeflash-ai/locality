//! Credential storage boundary.
//!
//! Connection records persist metadata in SQLite, while provider bearer tokens
//! live behind this trait. The file store is used for Linux/dev/CI; macOS uses
//! the system keychain through the `security` tool.

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub type CredentialResult<T> = Result<T, CredentialError>;

pub trait CredentialStore: Send + Sync {
    fn put(&self, secret_ref: &str, secret: &str) -> CredentialResult<()>;
    fn get(&self, secret_ref: &str) -> CredentialResult<String>;
    fn delete(&self, secret_ref: &str) -> CredentialResult<()>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialError {
    NotFound(String),
    Unavailable(String),
    Io(String),
}

impl CredentialError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "auth_required",
            Self::Unavailable(_) => "credential_store_unavailable",
            Self::Io(_) => "credential_store_unavailable",
        }
    }
}

impl Display for CredentialError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(secret_ref) => {
                write!(f, "credential `{secret_ref}` was not found")
            }
            Self::Unavailable(message) => write!(f, "credential store unavailable: {message}"),
            Self::Io(message) => write!(f, "credential store error: {message}"),
        }
    }
}

impl std::error::Error for CredentialError {}

impl From<std::io::Error> for CredentialError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value.to_string())
    }
}

#[derive(Clone, Debug)]
pub struct FileCredentialStore {
    root: PathBuf,
}

impl FileCredentialStore {
    pub fn new(state_root: impl Into<PathBuf>) -> Self {
        Self {
            root: state_root.into().join("credentials"),
        }
    }

    fn path_for(&self, secret_ref: &str) -> PathBuf {
        self.root.join(hex_name(secret_ref))
    }
}

impl CredentialStore for FileCredentialStore {
    fn put(&self, secret_ref: &str, secret: &str) -> CredentialResult<()> {
        std::fs::create_dir_all(&self.root)?;
        let path = self.path_for(secret_ref);
        let temp_path = path.with_extension("tmp");
        std::fs::write(&temp_path, secret)?;
        set_private_file_permissions(&temp_path)?;
        std::fs::rename(temp_path, path)?;
        Ok(())
    }

    fn get(&self, secret_ref: &str) -> CredentialResult<String> {
        let path = self.path_for(secret_ref);
        match std::fs::read_to_string(path) {
            Ok(secret) => Ok(secret.trim_end_matches(['\r', '\n']).to_string()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(CredentialError::NotFound(secret_ref.to_string()))
            }
            Err(error) => Err(error.into()),
        }
    }

    fn delete(&self, secret_ref: &str) -> CredentialResult<()> {
        let path = self.path_for(secret_ref);
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct InMemoryCredentialStore {
    secrets: Arc<Mutex<BTreeMap<String, String>>>,
}

impl InMemoryCredentialStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl CredentialStore for InMemoryCredentialStore {
    fn put(&self, secret_ref: &str, secret: &str) -> CredentialResult<()> {
        self.secrets
            .lock()
            .map_err(|_| CredentialError::Unavailable("credential lock poisoned".to_string()))?
            .insert(secret_ref.to_string(), secret.to_string());
        Ok(())
    }

    fn get(&self, secret_ref: &str) -> CredentialResult<String> {
        self.secrets
            .lock()
            .map_err(|_| CredentialError::Unavailable("credential lock poisoned".to_string()))?
            .get(secret_ref)
            .cloned()
            .ok_or_else(|| CredentialError::NotFound(secret_ref.to_string()))
    }

    fn delete(&self, secret_ref: &str) -> CredentialResult<()> {
        self.secrets
            .lock()
            .map_err(|_| CredentialError::Unavailable("credential lock poisoned".to_string()))?
            .remove(secret_ref);
        Ok(())
    }
}

#[cfg(target_os = "macos")]
#[derive(Clone, Debug, Default)]
pub struct KeychainCredentialStore;

#[cfg(target_os = "macos")]
impl CredentialStore for KeychainCredentialStore {
    fn put(&self, secret_ref: &str, secret: &str) -> CredentialResult<()> {
        let status = std::process::Command::new("security")
            .args([
                "add-generic-password",
                "-a",
                secret_ref,
                "-s",
                "afs",
                "-w",
                secret,
                "-U",
            ])
            .status()
            .map_err(|error| CredentialError::Unavailable(error.to_string()))?;
        if status.success() {
            Ok(())
        } else {
            Err(CredentialError::Unavailable(
                "macOS keychain write failed".to_string(),
            ))
        }
    }

    fn get(&self, secret_ref: &str) -> CredentialResult<String> {
        let output = std::process::Command::new("security")
            .args(["find-generic-password", "-a", secret_ref, "-s", "afs", "-w"])
            .output()
            .map_err(|error| CredentialError::Unavailable(error.to_string()))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout)
                .trim_end_matches(['\r', '\n'])
                .to_string())
        } else {
            Err(CredentialError::NotFound(secret_ref.to_string()))
        }
    }

    fn delete(&self, secret_ref: &str) -> CredentialResult<()> {
        let _ = std::process::Command::new("security")
            .args(["delete-generic-password", "-a", secret_ref, "-s", "afs"])
            .status()
            .map_err(|error| CredentialError::Unavailable(error.to_string()))?;
        Ok(())
    }
}

pub fn open_credential_store(state_root: &Path) -> Box<dyn CredentialStore> {
    #[cfg(target_os = "macos")]
    {
        let _ = state_root;
        Box::new(KeychainCredentialStore)
    }

    #[cfg(not(target_os = "macos"))]
    {
        Box::new(FileCredentialStore::new(state_root))
    }
}

fn hex_name(value: &str) -> String {
    let mut name = String::with_capacity(value.len() * 2);
    for byte in value.as_bytes() {
        name.push_str(&format!("{byte:02x}"));
    }
    name
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o600);
    std::fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}
