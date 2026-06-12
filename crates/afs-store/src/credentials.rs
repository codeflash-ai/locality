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
            let password = keychain_output_password(&output.stdout);
            if keychain_reports_hex_password(secret_ref, &password) {
                return Ok(decode_hex_encoded_password(&password).unwrap_or(password));
            }
            Ok(password)
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

#[cfg(any(test, target_os = "macos"))]
fn keychain_output_password(output: &[u8]) -> String {
    String::from_utf8_lossy(output)
        .trim_end_matches(['\r', '\n'])
        .to_string()
}

#[cfg(target_os = "macos")]
fn keychain_reports_hex_password(secret_ref: &str, encoded_password: &str) -> bool {
    let Ok(output) = std::process::Command::new("security")
        .args(["find-generic-password", "-a", secret_ref, "-s", "afs", "-g"])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }

    keychain_hex_password_from_diagnostics(&output.stderr)
        .is_some_and(|hex_password| hex_password.eq_ignore_ascii_case(encoded_password))
}

#[cfg(any(test, target_os = "macos"))]
fn keychain_hex_password_from_diagnostics(stderr: &[u8]) -> Option<String> {
    let diagnostics = String::from_utf8_lossy(stderr);
    diagnostics.lines().find_map(|line| {
        let hex_password = line.trim_start().strip_prefix("password: 0x")?;
        let hex_password = hex_password.split_whitespace().next()?;
        if hex_password.is_empty() || !hex_password.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        Some(hex_password.to_string())
    })
}

#[cfg(any(test, target_os = "macos"))]
fn decode_hex_encoded_password(value: &str) -> Option<String> {
    let decoded = decode_hex_string(value)?;
    Some(decoded.trim_end_matches(['\r', '\n']).to_string())
}

#[cfg(any(test, target_os = "macos"))]
fn decode_hex_string(value: &str) -> Option<String> {
    if value.len() % 2 != 0 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }

    let mut bytes = Vec::with_capacity(value.len() / 2);
    for chunk in value.as_bytes().chunks_exact(2) {
        let hex = std::str::from_utf8(chunk).ok()?;
        bytes.push(u8::from_str_radix(hex, 16).ok()?);
    }

    String::from_utf8(bytes).ok()
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

#[cfg(test)]
mod tests {
    use super::{
        decode_hex_encoded_password, keychain_hex_password_from_diagnostics,
        keychain_output_password,
    };

    #[test]
    fn keychain_output_password_trims_security_newline() {
        assert_eq!(keychain_output_password(b"secret\n"), "secret");
    }

    #[test]
    fn keychain_password_decodes_hex_encoded_json() {
        let encoded = "7b22776f726b7370616365223a225361727468616be2809973227d0a";

        assert_eq!(
            decode_hex_encoded_password(encoded).as_deref(),
            Some("{\"workspace\":\"Sarthak’s\"}")
        );
    }

    #[test]
    fn keychain_password_decodes_hex_encoded_utf8() {
        assert_eq!(
            decode_hex_encoded_password("5361727468616be2809973").as_deref(),
            Some("Sarthak’s")
        );
    }

    #[test]
    fn keychain_password_preserves_invalid_utf8_hex() {
        assert_eq!(decode_hex_encoded_password("deadbeef"), None);
    }

    #[test]
    fn keychain_diagnostics_ignore_quoted_hex_json_password() {
        assert_eq!(
            keychain_hex_password_from_diagnostics(br#"password: "7b2261223a317d""#),
            None
        );
    }

    #[test]
    fn keychain_diagnostics_extract_hex_password_marker() {
        let diagnostics = br#"password: 0x7B2261223A317D  "{"a":1}""#;

        assert_eq!(
            keychain_hex_password_from_diagnostics(diagnostics).as_deref(),
            Some("7B2261223A317D")
        );
    }
}
