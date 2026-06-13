//! Shared helpers for registering and opening platform virtual filesystem domains.
//!
//! The macOS File Provider control surface lives in the Swift helper bundled
//! with the File Provider extension. Rust entrypoints call this module rather
//! than shelling through `afs file-provider`, so the CLI and desktop app share
//! the same platform boundary.

use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(target_os = "linux")]
use std::time::Duration;

use afs_store::MountConfig;
#[cfg(target_os = "macos")]
use afs_store::ProjectionMode;
#[cfg(target_os = "linux")]
use afsd::ipc::{DaemonRequest, send_request_with_timeout};
use serde_json::Value;

#[cfg(target_os = "linux")]
const DEFAULT_DAEMON_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq)]
pub struct FileProviderHelperReport {
    pub helper: PathBuf,
    pub helper_report: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileProviderHelperError {
    Missing,
    Failed(String),
}

impl FileProviderHelperError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Missing => "helper_missing",
            Self::Failed(_) => "helper_failed",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Missing => {
                "agentfs-file-providerctl was not found; build or install platform/macos/AgentFSFileProvider first"
                    .to_string()
            }
            Self::Failed(message) => message.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinuxFuseRegistrationReport {
    pub service: String,
    pub unit_path: PathBuf,
    pub mountpoint: PathBuf,
    pub afs_fuse: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinuxFuseRegistrationError {
    DaemonNotRunning,
    HelperMissing,
    EnvMissing(String),
    Io(String),
    SystemctlFailed(String),
    UnsupportedPlatform(String),
}

impl LinuxFuseRegistrationError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::DaemonNotRunning => "daemon_not_running",
            Self::HelperMissing => "helper_missing",
            Self::EnvMissing(_) => "env_missing",
            Self::Io(_) => "io_error",
            Self::SystemctlFailed(_) => "systemctl_failed",
            Self::UnsupportedPlatform(_) => "unsupported_platform",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::DaemonNotRunning => {
                "afsd is not running; start it with `afs daemon start` before registering the FUSE mount"
                    .to_string()
            }
            Self::HelperMissing => {
                "afs-fuse was not found; build or install the afs-fuse binary".to_string()
            }
            Self::EnvMissing(message)
            | Self::Io(message)
            | Self::SystemctlFailed(message)
            | Self::UnsupportedPlatform(message) => message.clone(),
        }
    }
}

pub fn register_macos_file_provider_domain(
    mount_id: &str,
    display_name: &str,
) -> Result<FileProviderHelperReport, FileProviderHelperError> {
    run_macos_file_provider_helper(
        "register",
        vec![
            "--mount-id".to_string(),
            mount_id.to_string(),
            "--display-name".to_string(),
            display_name.to_string(),
        ],
    )
}

#[cfg(target_os = "linux")]
pub fn register_linux_fuse_mount(
    state_root: &Path,
    mount: &MountConfig,
) -> Result<LinuxFuseRegistrationReport, LinuxFuseRegistrationError> {
    if !daemon_is_running(state_root) {
        return Err(LinuxFuseRegistrationError::DaemonNotRunning);
    }

    let afs_fuse = afs_fuse_helper_path().ok_or(LinuxFuseRegistrationError::HelperMissing)?;
    let service = linux_fuse_unit_name(&mount.mount_id.0);
    let unit_path = linux_fuse_unit_path(&service)?;
    write_linux_fuse_unit(&unit_path, &afs_fuse, state_root, mount)?;
    run_systemctl_user(&["daemon-reload"])?;
    run_systemctl_user(&["enable", &service])?;
    run_systemctl_user(&["restart", &service])?;

    Ok(LinuxFuseRegistrationReport {
        service,
        unit_path,
        mountpoint: mount.root.clone(),
        afs_fuse,
    })
}

#[cfg(not(target_os = "linux"))]
pub fn register_linux_fuse_mount(
    _state_root: &Path,
    mount: &MountConfig,
) -> Result<LinuxFuseRegistrationReport, LinuxFuseRegistrationError> {
    Err(LinuxFuseRegistrationError::UnsupportedPlatform(format!(
        "linux_fuse registration is only supported on Linux; mount `{}` cannot be registered here",
        mount.mount_id.0
    )))
}

pub fn open_macos_file_provider_domain(
    mount_id: &str,
) -> Result<FileProviderHelperReport, FileProviderHelperError> {
    let (report, url) = resolve_macos_file_provider_domain(mount_id)?;
    Command::new("open")
        .arg(&url)
        .spawn()
        .map_err(|error| FileProviderHelperError::Failed(error.to_string()))?;
    Ok(report)
}

pub fn macos_file_provider_domain_url(mount_id: &str) -> Result<PathBuf, FileProviderHelperError> {
    resolve_macos_file_provider_domain(mount_id).map(|(_, url)| url)
}

pub fn ensure_macos_file_provider_shortcut(
    mount: &MountConfig,
) -> Result<Option<PathBuf>, FileProviderHelperError> {
    #[cfg(target_os = "macos")]
    {
        if mount.projection != ProjectionMode::MacosFileProvider {
            return Ok(None);
        }
        let access_root = macos_file_provider_domain_url(&mount.mount_id.0)?;
        ensure_macos_file_provider_shortcut_at(mount, &access_root)
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = mount;
        Ok(None)
    }
}

#[cfg(target_os = "macos")]
fn ensure_macos_file_provider_shortcut_at(
    mount: &MountConfig,
    access_root: &Path,
) -> Result<Option<PathBuf>, FileProviderHelperError> {
    if access_root == mount.root {
        return Ok(None);
    }

    let shortcut = mount
        .root
        .join(file_provider_shortcut_name(&mount.connector));
    std::fs::create_dir_all(&mount.root).map_err(|error| {
        FileProviderHelperError::Failed(format!(
            "could not create shortcut folder `{}`: {error}",
            mount.root.display()
        ))
    })?;
    if shortcut.exists() || shortcut.symlink_metadata().is_ok() {
        return Ok(Some(shortcut));
    }
    std::os::unix::fs::symlink(&access_root, &shortcut)
        .map_err(|error| FileProviderHelperError::Failed(error.to_string()))?;
    Ok(Some(shortcut))
}

fn resolve_macos_file_provider_domain(
    mount_id: &str,
) -> Result<(FileProviderHelperReport, PathBuf), FileProviderHelperError> {
    let report = run_macos_file_provider_helper(
        "open",
        vec!["--mount-id".to_string(), mount_id.to_string()],
    )?;
    let url = report
        .helper_report
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            FileProviderHelperError::Failed(
                "agentfs-file-providerctl did not return a CloudStorage URL".to_string(),
            )
        })?;
    Ok((report, PathBuf::from(url)))
}

pub fn macos_file_provider_display_name(root: &Path, fallback: &str) -> String {
    root.file_name()
        .and_then(|name| name.to_str())
        .map(strip_file_provider_directory_prefix)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| fallback.to_string())
}

fn strip_file_provider_directory_prefix(name: &str) -> &str {
    name.strip_prefix("AgentFS-")
        .filter(|stripped| !stripped.is_empty())
        .unwrap_or(name)
}

#[cfg(target_os = "macos")]
fn file_provider_shortcut_name(connector: &str) -> String {
    match connector {
        "notion" => "Notion Files".to_string(),
        other => format!("{} Files", title_case_connector(other)),
    }
}

#[cfg(target_os = "macos")]
fn title_case_connector(connector: &str) -> String {
    connector
        .split(['-', '_', ' '])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn run_macos_file_provider_helper(
    action: &str,
    args: Vec<String>,
) -> Result<FileProviderHelperReport, FileProviderHelperError> {
    let helper = file_provider_helper_path().ok_or(FileProviderHelperError::Missing)?;
    let mut command = Command::new(&helper);
    command.arg(action);
    command.args(args);
    command.arg("--json");

    let output = command
        .output()
        .map_err(|error| FileProviderHelperError::Failed(error.to_string()))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let helper_report =
        serde_json::from_str::<Value>(&stdout).unwrap_or_else(|_| Value::String(stdout.clone()));

    if !output.status.success() {
        let message = helper_report
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|message| !message.is_empty())
            .or_else(|| (!stderr.is_empty()).then_some(stderr))
            .unwrap_or_else(|| format!("agentfs-file-providerctl exited with {}", output.status));
        return Err(FileProviderHelperError::Failed(message));
    }

    Ok(FileProviderHelperReport {
        helper,
        helper_report,
    })
}

fn file_provider_helper_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("AFS_FILE_PROVIDERCTL") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }

    let mut candidates = Vec::new();
    if let Ok(current_exe) = std::env::current_exe()
        && let Some(dir) = current_exe.parent()
    {
        candidates.push(dir.join("agentfs-file-providerctl"));
    }

    candidates.push(PathBuf::from(
        "/Applications/AFS.app/Contents/MacOS/agentfs-file-providerctl",
    ));
    candidates.push(PathBuf::from(
        "/Applications/AgentFS.app/Contents/MacOS/agentfs-file-providerctl",
    ));
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(
            PathBuf::from(&home)
                .join("Applications/AFS.app/Contents/MacOS/agentfs-file-providerctl"),
        );
        candidates.push(
            PathBuf::from(home)
                .join("Applications/AgentFS.app/Contents/MacOS/agentfs-file-providerctl"),
        );
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let package_dir = manifest_dir.join("../../platform/macos/AgentFSFileProvider");
    candidates.push(
        package_dir.join(".build/dev-bundle/AgentFS.app/Contents/MacOS/agentfs-file-providerctl"),
    );
    candidates.push(package_dir.join(".build/debug/agentfs-file-providerctl"));
    candidates.push(package_dir.join(".build/release/agentfs-file-providerctl"));

    candidates.into_iter().find(|path| path.exists())
}

#[cfg(target_os = "linux")]
fn write_linux_fuse_unit(
    unit_path: &Path,
    afs_fuse: &Path,
    state_root: &Path,
    mount: &MountConfig,
) -> Result<(), LinuxFuseRegistrationError> {
    let log_dir = state_root.join("logs");
    std::fs::create_dir_all(unit_path.parent().unwrap_or_else(|| Path::new(".")))
        .map_err(|error| LinuxFuseRegistrationError::Io(error.to_string()))?;
    std::fs::create_dir_all(&log_dir)
        .map_err(|error| LinuxFuseRegistrationError::Io(error.to_string()))?;
    std::fs::create_dir_all(&mount.root)
        .map_err(|error| LinuxFuseRegistrationError::Io(error.to_string()))?;

    let log_path = log_dir.join(format!(
        "afs-fuse.{}.log",
        sanitize_systemd_fragment(&mount.mount_id.0)
    ));
    let unit = linux_fuse_unit_contents(afs_fuse, state_root, mount, &log_path);
    std::fs::write(unit_path, unit)
        .map_err(|error| LinuxFuseRegistrationError::Io(error.to_string()))
}

#[cfg(target_os = "linux")]
pub(crate) fn linux_fuse_unit_contents(
    afs_fuse: &Path,
    state_root: &Path,
    mount: &MountConfig,
    log_path: &Path,
) -> String {
    format!(
        "[Unit]\nDescription=AgentFS FUSE mount for {mount_id}\nAfter=default.target\n\n[Service]\nType=simple\nExecStart={afs_fuse} --mount-id {mount_id_arg} --state-dir {state_root} --mountpoint {mountpoint}\nExecStop=/usr/bin/fusermount3 -uz {mountpoint}\nKillSignal=SIGINT\nTimeoutStopSec=10\nLimitCORE=0\nRestart=on-failure\nRestartSec=2\nStandardOutput=append:{log_path}\nStandardError=append:{log_path}\n\n[Install]\nWantedBy=default.target\n",
        mount_id = mount.mount_id.0,
        afs_fuse = systemd_quote(&afs_fuse.display().to_string()),
        mount_id_arg = systemd_quote(&mount.mount_id.0),
        state_root = systemd_quote(&state_root.display().to_string()),
        mountpoint = systemd_quote(&mount.root.display().to_string()),
        log_path = log_path.display(),
    )
}

#[cfg(target_os = "linux")]
pub(crate) fn run_systemctl_user(args: &[&str]) -> Result<(), LinuxFuseRegistrationError> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(|error| LinuxFuseRegistrationError::SystemctlFailed(error.to_string()))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let message = if stderr.is_empty() { stdout } else { stderr };
    Err(LinuxFuseRegistrationError::SystemctlFailed(
        if message.is_empty() {
            format!("systemctl --user exited with {}", output.status)
        } else {
            message
        },
    ))
}

#[cfg(target_os = "linux")]
fn daemon_is_running(state_root: &Path) -> bool {
    matches!(
        send_request_with_timeout(
            state_root,
            &DaemonRequest::Ping,
            daemon_request_timeout()
        ),
        Ok(response) if response.ok
    )
}

#[cfg(target_os = "linux")]
pub(crate) fn linux_fuse_unit_name(mount_id: &str) -> String {
    format!(
        "ai.codeflash.afs.fuse.{}.service",
        sanitize_systemd_fragment(mount_id)
    )
}

#[cfg(target_os = "linux")]
pub(crate) fn linux_fuse_unit_path(unit_name: &str) -> Result<PathBuf, LinuxFuseRegistrationError> {
    let home = home_dir_path()?;
    Ok(home.join(".config/systemd/user").join(unit_name))
}

#[cfg(target_os = "linux")]
fn sanitize_systemd_fragment(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn systemd_quote(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`");
    format!("\"{escaped}\"")
}

#[cfg(target_os = "linux")]
fn home_dir_path() -> Result<PathBuf, LinuxFuseRegistrationError> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| LinuxFuseRegistrationError::EnvMissing("HOME is not set".to_string()))
}

#[cfg(target_os = "linux")]
fn afs_fuse_helper_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("AFS_FUSE_BIN") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }

    let mut candidates = Vec::new();
    if let Ok(current_exe) = std::env::current_exe()
        && let Some(dir) = current_exe.parent()
    {
        candidates.push(dir.join("afs-fuse"));
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest_dir.join("../..");
    candidates.push(workspace.join("target/debug/afs-fuse"));
    candidates.push(workspace.join("target/release/afs-fuse"));

    if let Some(path) = candidates.into_iter().find(|path| path.exists()) {
        return Some(path);
    }
    find_on_path("afs-fuse")
}

#[cfg(target_os = "linux")]
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn daemon_request_timeout() -> Duration {
    std::env::var("AFS_DAEMON_REQUEST_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_DAEMON_REQUEST_TIMEOUT)
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use afs_core::model::MountId;
    use afs_store::{MountConfig, ProjectionMode};

    #[test]
    fn linux_fuse_systemd_unit_uses_mount_specific_helper_args() {
        let mount = MountConfig::new(
            MountId::new("notion/main"),
            "notion",
            "/home/example/afs notion",
        )
        .projection(ProjectionMode::LinuxFuse);
        let unit_name = super::linux_fuse_unit_name(&mount.mount_id.0);
        let unit = super::linux_fuse_unit_contents(
            std::path::Path::new("/opt/agent fs/afs-fuse"),
            std::path::Path::new("/home/example/.afs"),
            &mount,
            std::path::Path::new("/home/example/.afs/logs/afs-fuse.notion_main.log"),
        );

        assert_eq!(unit_name, "ai.codeflash.afs.fuse.notion_main.service");
        assert!(unit.contains("ExecStart=\"/opt/agent fs/afs-fuse\""));
        assert!(unit.contains("--mount-id \"notion/main\""));
        assert!(unit.contains("--state-dir \"/home/example/.afs\""));
        assert!(unit.contains("--mountpoint \"/home/example/afs notion\""));
        assert!(unit.contains("ExecStop=/usr/bin/fusermount3 -uz \"/home/example/afs notion\""));
        assert!(unit.contains("TimeoutStopSec=10"));
        assert!(unit.contains("LimitCORE=0"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(
            unit.contains("StandardOutput=append:/home/example/.afs/logs/afs-fuse.notion_main.log")
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn macos_file_provider_display_name_strips_agentfs_cloudstorage_prefix() {
        assert_eq!(
            super::macos_file_provider_display_name(
                std::path::Path::new("/Users/example/Library/CloudStorage/AgentFS-Notion"),
                "fallback",
            ),
            "Notion"
        );
        assert_eq!(
            super::macos_file_provider_display_name(
                std::path::Path::new("/Users/example/Documents/AFS/Notion"),
                "fallback",
            ),
            "Notion"
        );
        assert_eq!(
            super::macos_file_provider_display_name(std::path::Path::new("/"), "fallback"),
            "fallback"
        );
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use afs_core::model::MountId;
    use afs_store::{MountConfig, ProjectionMode};

    #[test]
    fn file_provider_shortcut_creates_missing_mount_root() {
        let base = unique_temp_path("afs-file-provider-shortcut");
        let root = base.join("Mount");
        let access_root = base.join("CloudStorage").join("AgentFS-Notion");
        let mount = MountConfig::new(MountId::new("notion-main"), "notion", root.clone())
            .projection(ProjectionMode::MacosFileProvider);

        let shortcut = super::ensure_macos_file_provider_shortcut_at(&mount, &access_root)
            .expect("create shortcut")
            .expect("shortcut path");

        assert_eq!(shortcut, root.join("Notion Files"));
        assert!(root.is_dir());
        assert_eq!(
            fs::read_link(shortcut).expect("shortcut target"),
            access_root
        );

        let _ = fs::remove_dir_all(base);
    }

    fn unique_temp_path(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }
}
