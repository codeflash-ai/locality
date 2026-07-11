//! Credential storage boundary.
//!
//! Connection records persist metadata in SQLite, while provider bearer tokens
//! live behind this trait. The file store is used for Linux/dev/CI; macOS uses
//! the system keychain through the `security` tool.

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

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

#[cfg(any(test, target_os = "macos"))]
const PRIMARY_KEYCHAIN_SERVICE: &str = "loc";

#[cfg(any(test, target_os = "macos"))]
const COMPAT_KEYCHAIN_SERVICES: [&str; 2] = [PRIMARY_KEYCHAIN_SERVICE, "afs"];

#[cfg(target_os = "macos")]
impl CredentialStore for KeychainCredentialStore {
    fn put(&self, secret_ref: &str, secret: &str) -> CredentialResult<()> {
        let output = std::process::Command::new("security")
            .args([
                "add-generic-password",
                "-a",
                secret_ref,
                "-s",
                PRIMARY_KEYCHAIN_SERVICE,
                "-w",
                secret,
                "-U",
            ])
            .output()
            .map_err(|error| CredentialError::Unavailable(error.to_string()))?;
        if output.status.success() {
            cache_keychain_secret(secret_ref, secret)?;
            Ok(())
        } else {
            Err(CredentialError::Unavailable(
                "macOS keychain write failed".to_string(),
            ))
        }
    }

    fn get(&self, secret_ref: &str) -> CredentialResult<String> {
        get_cached_keychain_secret(secret_ref, read_keychain_password, |secret_ref, secret| {
            self.put(secret_ref, secret)
        })
    }

    fn delete(&self, secret_ref: &str) -> CredentialResult<()> {
        for service in COMPAT_KEYCHAIN_SERVICES {
            let _ = std::process::Command::new("security")
                .args(["delete-generic-password", "-a", secret_ref, "-s", service])
                .output()
                .map_err(|error| CredentialError::Unavailable(error.to_string()))?;
        }
        forget_keychain_secret(secret_ref)?;
        Ok(())
    }
}

#[cfg(any(test, target_os = "macos"))]
static KEYCHAIN_CREDENTIAL_CACHE: LazyLock<Mutex<BTreeMap<String, String>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

#[cfg(any(test, target_os = "macos"))]
fn get_cached_keychain_secret(
    secret_ref: &str,
    mut read_keychain_password: impl FnMut(&str, &str) -> CredentialResult<Option<String>>,
    mut promote_secret: impl FnMut(&str, &str) -> CredentialResult<()>,
) -> CredentialResult<String> {
    if let Some(secret) = cached_keychain_secret(secret_ref)? {
        return Ok(secret);
    }

    for service in COMPAT_KEYCHAIN_SERVICES {
        if let Some(password) = read_keychain_password(secret_ref, service)? {
            if service != PRIMARY_KEYCHAIN_SERVICE {
                let _ = promote_secret(secret_ref, &password);
            }
            cache_keychain_secret(secret_ref, &password)?;
            return Ok(password);
        }
    }

    Err(CredentialError::NotFound(secret_ref.to_string()))
}

#[cfg(any(test, target_os = "macos"))]
fn cached_keychain_secret(secret_ref: &str) -> CredentialResult<Option<String>> {
    Ok(KEYCHAIN_CREDENTIAL_CACHE
        .lock()
        .map_err(|_| CredentialError::Unavailable("credential cache lock poisoned".to_string()))?
        .get(secret_ref)
        .cloned())
}

#[cfg(any(test, target_os = "macos"))]
fn cache_keychain_secret(secret_ref: &str, secret: &str) -> CredentialResult<()> {
    KEYCHAIN_CREDENTIAL_CACHE
        .lock()
        .map_err(|_| CredentialError::Unavailable("credential cache lock poisoned".to_string()))?
        .insert(secret_ref.to_string(), secret.to_string());
    Ok(())
}

#[cfg(any(test, target_os = "macos"))]
fn forget_keychain_secret(secret_ref: &str) -> CredentialResult<()> {
    KEYCHAIN_CREDENTIAL_CACHE
        .lock()
        .map_err(|_| CredentialError::Unavailable("credential cache lock poisoned".to_string()))?
        .remove(secret_ref);
    Ok(())
}

#[cfg(target_os = "macos")]
fn read_keychain_password(secret_ref: &str, service: &str) -> CredentialResult<Option<String>> {
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-a",
            secret_ref,
            "-s",
            service,
            "-w",
        ])
        .output()
        .map_err(|error| CredentialError::Unavailable(error.to_string()))?;
    if !output.status.success() {
        return Ok(None);
    }

    let password = keychain_output_password(&output.stdout);
    if keychain_reports_hex_password(secret_ref, service, &password) {
        return Ok(Some(
            decode_hex_encoded_password(&password).unwrap_or(password),
        ));
    }

    Ok(Some(password))
}

#[cfg(windows)]
#[derive(Clone, Debug, Default)]
pub struct WindowsCredentialStore;

#[cfg(windows)]
impl CredentialStore for WindowsCredentialStore {
    fn put(&self, secret_ref: &str, secret: &str) -> CredentialResult<()> {
        use std::ptr::null_mut;
        use windows_sys::Win32::Security::Credentials::{
            CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredWriteW,
        };

        let mut target_name = wide_null(&primary_windows_target_name(secret_ref));
        let mut blob = secret.as_bytes().to_vec();
        let blob_size = u32::try_from(blob.len()).map_err(|_| {
            CredentialError::Unavailable(
                "credential is too large for Windows Credential Manager".to_string(),
            )
        })?;
        let mut credential = CREDENTIALW {
            Flags: 0,
            Type: CRED_TYPE_GENERIC,
            TargetName: target_name.as_mut_ptr(),
            Comment: null_mut(),
            LastWritten: unsafe { std::mem::zeroed() },
            CredentialBlobSize: blob_size,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: null_mut(),
            TargetAlias: null_mut(),
            UserName: null_mut(),
        };

        let ok = unsafe { CredWriteW(&mut credential, 0) };
        if ok != 0 {
            Ok(())
        } else {
            Err(last_windows_credential_error("write"))
        }
    }

    fn get(&self, secret_ref: &str) -> CredentialResult<String> {
        for target_name in windows_target_names(secret_ref) {
            if let Some(secret) = read_windows_secret(&target_name)? {
                if target_name != primary_windows_target_name(secret_ref) {
                    let _ = self.put(secret_ref, &secret);
                }
                return Ok(secret);
            }
        }

        Err(CredentialError::NotFound(secret_ref.to_string()))
    }

    fn delete(&self, secret_ref: &str) -> CredentialResult<()> {
        use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NOT_FOUND, GetLastError};
        use windows_sys::Win32::Security::Credentials::{CRED_TYPE_GENERIC, CredDeleteW};

        for target_name in windows_target_names(secret_ref) {
            let wide_name = wide_null(&target_name);
            let ok = unsafe { CredDeleteW(wide_name.as_ptr(), CRED_TYPE_GENERIC, 0) };
            if ok != 0 {
                continue;
            }

            let code = unsafe { GetLastError() };
            if code != ERROR_NOT_FOUND && code != ERROR_FILE_NOT_FOUND {
                return Err(windows_credential_error("delete", code));
            }
        }

        Ok(())
    }
}

#[cfg(windows)]
fn read_windows_secret(target_name: &str) -> CredentialResult<Option<String>> {
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NOT_FOUND, GetLastError};
    use windows_sys::Win32::Security::Credentials::{
        CRED_TYPE_GENERIC, CREDENTIALW, CredFree, CredReadW,
    };

    let target_name = wide_null(target_name);
    let mut credential: *mut CREDENTIALW = null_mut();
    let ok = unsafe { CredReadW(target_name.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) };
    if ok == 0 {
        let code = unsafe { GetLastError() };
        if code == ERROR_NOT_FOUND || code == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        return Err(windows_credential_error("read", code));
    }

    let credential_ref = unsafe { &*credential };
    let bytes = unsafe {
        std::slice::from_raw_parts(
            credential_ref.CredentialBlob,
            credential_ref.CredentialBlobSize as usize,
        )
    };
    let secret = String::from_utf8(bytes.to_vec()).map_err(|error| {
        CredentialError::Unavailable(format!("Windows credential is not valid UTF-8: {error}"))
    });
    unsafe {
        CredFree(credential.cast());
    }
    secret.map(Some)
}

#[cfg(any(test, windows))]
fn primary_windows_target_name(secret_ref: &str) -> String {
    windows_target_name("ai.codeflash.locality:", secret_ref)
}

#[cfg(any(test, windows))]
fn windows_target_names(secret_ref: &str) -> [String; 2] {
    [
        primary_windows_target_name(secret_ref),
        windows_target_name("ai.codeflash.afs:", secret_ref),
    ]
}

#[cfg(any(test, windows))]
fn windows_target_name(prefix: &str, secret_ref: &str) -> String {
    format!("{prefix}{secret_ref}")
}

#[cfg(windows)]
fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn last_windows_credential_error(operation: &str) -> CredentialError {
    use windows_sys::Win32::Foundation::GetLastError;

    windows_credential_error(operation, unsafe { GetLastError() })
}

#[cfg(windows)]
fn windows_credential_error(operation: &str, code: u32) -> CredentialError {
    CredentialError::Unavailable(format!(
        "Windows Credential Manager {operation} failed with error {code}"
    ))
}

#[cfg(any(test, target_os = "macos"))]
fn keychain_output_password(output: &[u8]) -> String {
    String::from_utf8_lossy(output)
        .trim_end_matches(['\r', '\n'])
        .to_string()
}

#[cfg(target_os = "macos")]
fn keychain_reports_hex_password(secret_ref: &str, service: &str, encoded_password: &str) -> bool {
    let Ok(output) = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-a",
            secret_ref,
            "-s",
            service,
            "-g",
        ])
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
    if !value.len().is_multiple_of(2) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }

    let mut bytes = Vec::with_capacity(value.len() / 2);
    for chunk in value.as_bytes().chunks_exact(2) {
        let hex = std::str::from_utf8(chunk).ok()?;
        bytes.push(u8::from_str_radix(hex, 16).ok()?);
    }

    String::from_utf8(bytes).ok()
}

const CREDENTIAL_STORE_ENV: &str = "LOCALITY_CREDENTIAL_STORE";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CredentialStoreBackend {
    File,
    PlatformDefault,
}

fn credential_store_backend_for_override(value: Option<&str>) -> CredentialStoreBackend {
    match value.map(str::trim) {
        Some("file") => CredentialStoreBackend::File,
        _ => CredentialStoreBackend::PlatformDefault,
    }
}

pub fn open_credential_store(state_root: &Path) -> Box<dyn CredentialStore> {
    if credential_store_backend_for_override(std::env::var(CREDENTIAL_STORE_ENV).ok().as_deref())
        == CredentialStoreBackend::File
    {
        return Box::new(FileCredentialStore::new(state_root));
    }

    #[cfg(target_os = "macos")]
    {
        let _ = state_root;
        Box::new(KeychainCredentialStore)
    }

    #[cfg(windows)]
    {
        let _ = state_root;
        Box::new(WindowsCredentialStore)
    }

    #[cfg(all(not(target_os = "macos"), not(windows)))]
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
    use std::cell::{Cell, RefCell};

    use super::{
        COMPAT_KEYCHAIN_SERVICES, CredentialResult, CredentialStoreBackend,
        PRIMARY_KEYCHAIN_SERVICE, credential_store_backend_for_override,
        decode_hex_encoded_password, get_cached_keychain_secret,
        keychain_hex_password_from_diagnostics, keychain_output_password,
        primary_windows_target_name, windows_target_names,
    };

    #[test]
    fn credential_store_backend_honors_file_override() {
        assert_eq!(
            credential_store_backend_for_override(Some("file")),
            CredentialStoreBackend::File
        );
    }

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

    #[test]
    fn keychain_services_include_afs_compatibility_alias() {
        assert_eq!(COMPAT_KEYCHAIN_SERVICES, [PRIMARY_KEYCHAIN_SERVICE, "afs"]);
    }

    #[test]
    fn keychain_get_uses_process_cache_after_first_read() {
        let calls = Cell::new(0);
        let secret_ref = "connection:test-cache-primary";

        let first = get_cached_keychain_secret(
            secret_ref,
            |read_ref, service| {
                assert_eq!(read_ref, secret_ref);
                assert_eq!(service, PRIMARY_KEYCHAIN_SERVICE);
                calls.set(calls.get() + 1);
                Ok(Some("cached-secret".to_string()))
            },
            |_, _| -> CredentialResult<()> { panic!("primary keychain hit should not promote") },
        )
        .expect("first credential lookup");

        let second = get_cached_keychain_secret(
            secret_ref,
            |_, _| -> CredentialResult<Option<String>> {
                panic!("cached credential lookup should not query keychain")
            },
            |_, _| -> CredentialResult<()> {
                panic!("cached credential lookup should not promote")
            },
        )
        .expect("cached credential lookup");

        assert_eq!(first, "cached-secret");
        assert_eq!(second, "cached-secret");
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn keychain_get_caches_compatibility_service_result_after_promotion() {
        let calls = Cell::new(0);
        let promotions = RefCell::new(Vec::new());
        let secret_ref = "connection:test-cache-compat";

        let first = get_cached_keychain_secret(
            secret_ref,
            |read_ref, service| {
                assert_eq!(read_ref, secret_ref);
                calls.set(calls.get() + 1);
                if service == "afs" {
                    Ok(Some("legacy-secret".to_string()))
                } else {
                    Ok(None)
                }
            },
            |promoted_ref, secret| {
                promotions
                    .borrow_mut()
                    .push((promoted_ref.to_string(), secret.to_string()));
                Ok(())
            },
        )
        .expect("compatibility credential lookup");

        let second = get_cached_keychain_secret(
            secret_ref,
            |_, _| -> CredentialResult<Option<String>> {
                panic!("cached compatibility credential should not query keychain")
            },
            |_, _| -> CredentialResult<()> {
                panic!("cached compatibility credential should not promote")
            },
        )
        .expect("cached compatibility credential lookup");

        assert_eq!(first, "legacy-secret");
        assert_eq!(second, "legacy-secret");
        assert_eq!(calls.get(), 2);
        assert_eq!(
            promotions.into_inner(),
            vec![(secret_ref.to_string(), "legacy-secret".to_string())]
        );
    }

    #[test]
    fn windows_target_names_include_afs_compatibility_alias() {
        assert_eq!(
            windows_target_names("workspace"),
            [
                primary_windows_target_name("workspace"),
                "ai.codeflash.afs:workspace".to_string(),
            ]
        );
    }
}
