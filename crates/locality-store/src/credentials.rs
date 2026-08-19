//! Credential storage boundary.
//!
//! Connection records persist metadata in SQLite, while provider bearer tokens
//! live behind this trait. The file store is used for Linux/dev/CI; macOS uses
//! the system keychain through the `security` tool.

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
#[cfg(any(test, target_os = "macos"))]
use std::sync::LazyLock;
use std::sync::{Arc, Mutex};

use fs2::FileExt;

pub type CredentialResult<T> = Result<T, CredentialError>;

pub trait CredentialStore: Send + Sync {
    fn put(&self, secret_ref: &str, secret: &str) -> CredentialResult<()>;
    fn get(&self, secret_ref: &str) -> CredentialResult<String>;
    fn delete(&self, secret_ref: &str) -> CredentialResult<()>;

    /// Reads persisted credential state without trusting an in-process cache.
    /// Backends without a cache can use the default implementation.
    fn get_fresh(&self, secret_ref: &str) -> CredentialResult<String> {
        self.get(secret_ref)
    }

    fn acquire_refresh_lock(
        &self,
        _secret_ref: &str,
    ) -> CredentialResult<Box<dyn CredentialRefreshLock>> {
        Ok(Box::new(NoopCredentialRefreshLock))
    }
}

pub trait CredentialRefreshLock: Send {}

struct NoopCredentialRefreshLock;

impl CredentialRefreshLock for NoopCredentialRefreshLock {}

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

    fn acquire_refresh_lock(
        &self,
        secret_ref: &str,
    ) -> CredentialResult<Box<dyn CredentialRefreshLock>> {
        acquire_file_refresh_lock(&self.root, secret_ref)
    }
}

fn acquire_file_refresh_lock(
    lock_root: &Path,
    secret_ref: &str,
) -> CredentialResult<Box<dyn CredentialRefreshLock>> {
    std::fs::create_dir_all(lock_root)?;
    let lock_path = lock_root.join(format!("{}.refresh.lock", hex_name(secret_ref)));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    set_private_file_permissions(&lock_path)?;
    file.lock_exclusive()
        .map_err(|error| CredentialError::Io(error.to_string()))?;
    Ok(Box::new(FileCredentialRefreshLock { file }))
}

struct FileCredentialRefreshLock {
    file: File,
}

impl CredentialRefreshLock for FileCredentialRefreshLock {}

impl Drop for FileCredentialRefreshLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
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

// `/usr/bin/security` returns the low byte of `errSecItemNotFound` (-25300)
// as its process exit status.
#[cfg(any(test, target_os = "macos"))]
const SECURITY_ITEM_NOT_FOUND_EXIT_CODE: i32 = 44;

#[cfg(target_os = "macos")]
impl CredentialStore for KeychainCredentialStore {
    fn put(&self, secret_ref: &str, secret: &str) -> CredentialResult<()> {
        put_keychain_secret(
            secret_ref,
            secret,
            write_keychain_password,
            read_keychain_password,
        )
    }

    fn get(&self, secret_ref: &str) -> CredentialResult<String> {
        get_cached_keychain_secret(secret_ref, read_keychain_password, |secret_ref, secret| {
            self.put(secret_ref, secret)
        })
    }

    fn get_fresh(&self, secret_ref: &str) -> CredentialResult<String> {
        get_uncached_keychain_secret(secret_ref, read_keychain_password, |secret_ref, secret| {
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

#[cfg(target_os = "macos")]
fn write_keychain_password(secret_ref: &str, service: &str, secret: &str) -> CredentialResult<()> {
    let output = std::process::Command::new("security")
        .args([
            "add-generic-password",
            "-a",
            secret_ref,
            "-s",
            service,
            "-w",
            secret,
            "-U",
        ])
        .output()
        .map_err(|error| CredentialError::Unavailable(error.to_string()))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(CredentialError::Unavailable(
            "macOS keychain write failed".to_string(),
        ))
    }
}

#[cfg(any(test, target_os = "macos"))]
fn put_keychain_secret(
    secret_ref: &str,
    secret: &str,
    mut write_keychain_password: impl FnMut(&str, &str, &str) -> CredentialResult<()>,
    mut read_keychain_password: impl FnMut(&str, &str) -> CredentialResult<Option<String>>,
) -> CredentialResult<()> {
    // A previous credential may still be cached even when another process has
    // removed or replaced the durable item. Do not let that copy survive a
    // failed write or participate in verification.
    forget_keychain_secret(secret_ref)?;
    write_keychain_password(secret_ref, PRIMARY_KEYCHAIN_SERVICE, secret)?;

    match read_keychain_password(secret_ref, PRIMARY_KEYCHAIN_SERVICE)? {
        Some(persisted) if persisted == secret => cache_keychain_secret(secret_ref, secret),
        Some(_) => Err(CredentialError::Unavailable(
            "macOS keychain write verification returned different credential data".to_string(),
        )),
        None => Err(CredentialError::Unavailable(
            "macOS keychain write was not readable after it completed".to_string(),
        )),
    }
}

#[cfg(any(test, target_os = "macos"))]
static KEYCHAIN_CREDENTIAL_CACHE: LazyLock<Mutex<BTreeMap<String, String>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

#[cfg(any(test, target_os = "macos"))]
fn get_cached_keychain_secret(
    secret_ref: &str,
    read_keychain_password: impl FnMut(&str, &str) -> CredentialResult<Option<String>>,
    promote_secret: impl FnMut(&str, &str) -> CredentialResult<()>,
) -> CredentialResult<String> {
    if let Some(secret) = cached_keychain_secret(secret_ref)? {
        return Ok(secret);
    }

    get_uncached_keychain_secret(secret_ref, read_keychain_password, promote_secret)
}

#[cfg(any(test, target_os = "macos"))]
fn get_uncached_keychain_secret(
    secret_ref: &str,
    mut read_keychain_password: impl FnMut(&str, &str) -> CredentialResult<Option<String>>,
    mut promote_secret: impl FnMut(&str, &str) -> CredentialResult<()>,
) -> CredentialResult<String> {
    forget_keychain_secret(secret_ref)?;

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
        if keychain_read_failure_is_not_found(output.status.code(), &output.stderr) {
            return Ok(None);
        }
        return Err(CredentialError::Unavailable(format!(
            "macOS keychain read failed{}",
            output
                .status
                .code()
                .map(|code| format!(" with status {code}"))
                .unwrap_or_default()
        )));
    }

    let password = keychain_output_password(&output.stdout);
    if keychain_reports_hex_password(secret_ref, service, &password) {
        return Ok(Some(
            decode_hex_encoded_password(&password).unwrap_or(password),
        ));
    }

    Ok(Some(password))
}

#[cfg(any(test, target_os = "macos"))]
fn keychain_read_failure_is_not_found(status_code: Option<i32>, stderr: &[u8]) -> bool {
    status_code == Some(SECURITY_ITEM_NOT_FOUND_EXIT_CODE)
        || String::from_utf8_lossy(stderr)
            .to_ascii_lowercase()
            .contains("item could not be found")
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

/// Adds state-root refresh coordination to every production credential backend.
struct CoordinatedCredentialStore {
    inner: Box<dyn CredentialStore>,
    refresh_lock_root: PathBuf,
}

impl CoordinatedCredentialStore {
    fn new(state_root: &Path, inner: Box<dyn CredentialStore>) -> Self {
        Self {
            inner,
            refresh_lock_root: state_root.join("credentials"),
        }
    }
}

impl CredentialStore for CoordinatedCredentialStore {
    fn put(&self, secret_ref: &str, secret: &str) -> CredentialResult<()> {
        self.inner.put(secret_ref, secret)
    }

    fn get(&self, secret_ref: &str) -> CredentialResult<String> {
        self.inner.get(secret_ref)
    }

    fn get_fresh(&self, secret_ref: &str) -> CredentialResult<String> {
        self.inner.get_fresh(secret_ref)
    }

    fn delete(&self, secret_ref: &str) -> CredentialResult<()> {
        self.inner.delete(secret_ref)
    }

    fn acquire_refresh_lock(
        &self,
        secret_ref: &str,
    ) -> CredentialResult<Box<dyn CredentialRefreshLock>> {
        acquire_file_refresh_lock(&self.refresh_lock_root, secret_ref)
    }
}

pub fn open_credential_store(state_root: &Path) -> Box<dyn CredentialStore> {
    let inner: Box<dyn CredentialStore> = if credential_store_backend_for_override(
        std::env::var(CREDENTIAL_STORE_ENV).ok().as_deref(),
    ) == CredentialStoreBackend::File
    {
        Box::new(FileCredentialStore::new(state_root))
    } else {
        #[cfg(target_os = "macos")]
        {
            Box::new(KeychainCredentialStore)
        }

        #[cfg(windows)]
        {
            Box::new(WindowsCredentialStore)
        }

        #[cfg(all(not(target_os = "macos"), not(windows)))]
        {
            Box::new(FileCredentialStore::new(state_root))
        }
    };

    Box::new(CoordinatedCredentialStore::new(state_root, inner))
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
    use std::path::Path;
    use std::process::Command;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{
        COMPAT_KEYCHAIN_SERVICES, CoordinatedCredentialStore, CredentialError, CredentialResult,
        CredentialStore, CredentialStoreBackend, FileCredentialStore, InMemoryCredentialStore,
        PRIMARY_KEYCHAIN_SERVICE, cache_keychain_secret, cached_keychain_secret,
        credential_store_backend_for_override, decode_hex_encoded_password, forget_keychain_secret,
        get_cached_keychain_secret, get_uncached_keychain_secret,
        keychain_hex_password_from_diagnostics, keychain_output_password,
        keychain_read_failure_is_not_found, primary_windows_target_name, put_keychain_secret,
        windows_target_names,
    };

    #[test]
    fn file_credential_refresh_lock_is_exclusive_across_store_instances() {
        let state_root = tempfile::tempdir().expect("temporary state root");
        let first_store = FileCredentialStore::new(state_root.path());
        let second_store = FileCredentialStore::new(state_root.path());
        let first_lock = first_store
            .acquire_refresh_lock("connection:slack-live")
            .expect("first refresh lock");
        let (acquired_tx, acquired_rx) = mpsc::channel();

        let waiter = thread::spawn(move || {
            let second_lock = second_store
                .acquire_refresh_lock("connection:slack-live")
                .expect("second refresh lock");
            acquired_tx.send(()).expect("report acquired lock");
            drop(second_lock);
        });

        assert!(
            matches!(
                acquired_rx.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "second store acquired the refresh lock before the first released it"
        );
        drop(first_lock);
        acquired_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("second store should acquire released refresh lock");
        waiter.join().expect("refresh lock waiter");
    }

    #[test]
    fn production_credential_wrapper_locks_across_processes() {
        let state_root = tempfile::tempdir().expect("temporary state root");
        let store = CoordinatedCredentialStore::new(
            state_root.path(),
            Box::new(InMemoryCredentialStore::new()),
        );
        let first_lock = store
            .acquire_refresh_lock("connection:slack-live")
            .expect("first refresh lock");
        let acquired_marker = state_root.path().join("child-acquired-refresh-lock");
        let mut child = Command::new(std::env::current_exe().expect("current test executable"))
            .args([
                "--exact",
                "credentials::tests::production_credential_wrapper_process_lock_child",
                "--nocapture",
            ])
            .env(
                "LOCALITY_CREDENTIAL_LOCK_TEST_ROOT",
                state_root.path().as_os_str(),
            )
            .env(
                "LOCALITY_CREDENTIAL_LOCK_TEST_MARKER",
                acquired_marker.as_os_str(),
            )
            .spawn()
            .expect("spawn credential lock child");

        thread::sleep(Duration::from_millis(150));
        let child_blocked = child
            .try_wait()
            .expect("poll credential lock child")
            .is_none()
            && !acquired_marker.exists();
        drop(first_lock);
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().expect("poll credential lock child") {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().expect("stop timed out credential lock child");
                panic!("credential lock child did not finish after the parent released the lock");
            }
            thread::sleep(Duration::from_millis(10));
        };

        assert!(
            child_blocked,
            "production credential wrapper did not block the independent process"
        );
        assert!(status.success(), "credential lock child failed: {status}");
        assert!(
            acquired_marker.exists(),
            "credential lock child did not acquire the released lock"
        );
    }

    #[test]
    fn production_credential_wrapper_process_lock_child() {
        let Ok(state_root) = std::env::var("LOCALITY_CREDENTIAL_LOCK_TEST_ROOT") else {
            return;
        };
        let acquired_marker = std::env::var("LOCALITY_CREDENTIAL_LOCK_TEST_MARKER")
            .expect("credential lock child marker");
        let store = CoordinatedCredentialStore::new(
            Path::new(&state_root),
            Box::new(InMemoryCredentialStore::new()),
        );
        let _lock = store
            .acquire_refresh_lock("connection:slack-live")
            .expect("child refresh lock");
        std::fs::write(acquired_marker, b"acquired").expect("write child lock marker");
    }

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
    fn keychain_put_is_readable_after_process_cache_is_cleared() {
        let secret_ref = "connection:test-restart-readable";
        let persisted = RefCell::new(None::<String>);

        put_keychain_secret(
            secret_ref,
            "oauth-json",
            |write_ref, service, secret| {
                assert_eq!(write_ref, secret_ref);
                assert_eq!(service, PRIMARY_KEYCHAIN_SERVICE);
                *persisted.borrow_mut() = Some(secret.to_string());
                Ok(())
            },
            |read_ref, service| {
                assert_eq!(read_ref, secret_ref);
                assert_eq!(service, PRIMARY_KEYCHAIN_SERVICE);
                Ok(persisted.borrow().clone())
            },
        )
        .expect("put verified credential");

        forget_keychain_secret(secret_ref).expect("simulate process restart");
        let restarted_read = get_cached_keychain_secret(
            secret_ref,
            |read_ref, service| {
                assert_eq!(read_ref, secret_ref);
                assert_eq!(service, PRIMARY_KEYCHAIN_SERVICE);
                Ok(persisted.borrow().clone())
            },
            |_, _| -> CredentialResult<()> { panic!("primary credential should not promote") },
        )
        .expect("read credential after simulated restart");

        assert_eq!(restarted_read, "oauth-json");
        forget_keychain_secret(secret_ref).expect("clear test credential cache");
    }

    #[test]
    fn keychain_put_rejects_write_that_is_not_durably_readable() {
        let secret_ref = "connection:test-unreadable-write";
        cache_keychain_secret(secret_ref, "stale-oauth-json").expect("seed process cache");

        let error = put_keychain_secret(
            secret_ref,
            "new-oauth-json",
            |_, _, _| Ok(()),
            |_, _| Ok(None),
        )
        .expect_err("unreadable write must fail");

        assert_eq!(
            error,
            CredentialError::Unavailable(
                "macOS keychain write was not readable after it completed".to_string()
            )
        );
        assert_eq!(
            cached_keychain_secret(secret_ref).expect("read process cache"),
            None
        );
    }

    #[test]
    fn keychain_read_distinguishes_missing_item_from_store_failure() {
        assert!(keychain_read_failure_is_not_found(Some(44), b""));
        assert!(keychain_read_failure_is_not_found(
            None,
            b"The specified item could not be found in the keychain."
        ));
        assert!(!keychain_read_failure_is_not_found(
            Some(51),
            b"User interaction is not allowed."
        ));
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
    fn keychain_fresh_get_bypasses_and_replaces_process_cache() {
        let calls = Cell::new(0);
        let secret_ref = "connection:test-cache-fresh";
        cache_keychain_secret(secret_ref, "stale-secret").expect("seed stale credential cache");

        let fresh = get_uncached_keychain_secret(
            secret_ref,
            |read_ref, service| {
                assert_eq!(read_ref, secret_ref);
                assert_eq!(service, PRIMARY_KEYCHAIN_SERVICE);
                calls.set(calls.get() + 1);
                Ok(Some("fresh-secret".to_string()))
            },
            |_, _| -> CredentialResult<()> { panic!("primary keychain hit should not promote") },
        )
        .expect("fresh keychain lookup");

        assert_eq!(fresh, "fresh-secret");
        assert_eq!(calls.get(), 1);
        assert_eq!(
            cached_keychain_secret(secret_ref).expect("read credential cache"),
            Some("fresh-secret".to_string())
        );
        forget_keychain_secret(secret_ref).expect("clear credential cache");
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
