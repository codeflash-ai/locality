//! Shared helpers for registering and opening platform virtual filesystem domains.
//!
//! The macOS File Provider control surface lives in the Swift helper bundled
//! with the File Provider extension. Rust entrypoints call this module rather
//! than shelling through `afs file-provider`, so the CLI and desktop app share
//! the same platform boundary.

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(target_os = "windows")]
use std::process::Stdio;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use std::time::Duration;

use afs_store::MountConfig;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use afsd::ipc::{DaemonRequest, send_request_with_timeout};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(any(target_os = "linux", target_os = "windows"))]
const DEFAULT_DAEMON_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_os = "windows")]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
#[cfg(target_os = "windows")]
const DETACHED_PROCESS: u32 = 0x0000_0008;
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(target_os = "windows")]
const HIDDEN_WINDOWS_PROCESS_FLAGS: u32 =
    CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_NO_WINDOW;

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowsCloudFilesHelperError {
    DaemonNotRunning,
    Missing,
    Failed(String),
    UnsupportedPlatform(String),
}

impl WindowsCloudFilesHelperError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::DaemonNotRunning => "daemon_not_running",
            Self::Missing => "helper_missing",
            Self::Failed(_) => "helper_failed",
            Self::UnsupportedPlatform(_) => "unsupported_platform",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::DaemonNotRunning => {
                "afsd is not running; start it with `afs daemon start` before starting the Windows Cloud Files provider"
                    .to_string()
            }
            Self::Missing => {
                "afs-cloud-files was not found; build or install the Windows Cloud Files helper"
                    .to_string()
            }
            Self::Failed(message) | Self::UnsupportedPlatform(message) => message.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowsCloudFilesLifecycleAction {
    Start,
    Stop,
    Status,
    Restart,
}

impl WindowsCloudFilesLifecycleAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Status => "status",
            Self::Restart => "restart",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WindowsCloudFilesProviderState {
    Running,
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct WindowsCloudFilesProcessMetadata {
    mount_id: String,
    pid: u32,
    helper: PathBuf,
    sync_root: PathBuf,
    state_dir: PathBuf,
    stdout_log: PathBuf,
    stderr_log: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct WindowsCloudFilesLifecycleReport {
    message: String,
    state: WindowsCloudFilesProviderState,
    mount_id: String,
    sync_root: String,
    state_dir: String,
    helper: String,
    helper_present: bool,
    daemon_running: bool,
    registered: Option<bool>,
    pid: Option<u32>,
    stale_pid_file: bool,
    pid_file: String,
    stdout_log: String,
    stderr_log: String,
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

pub fn register_windows_cloud_files_sync_root(
    state_root: &Path,
    mount: &MountConfig,
    display_name: &str,
) -> Result<FileProviderHelperReport, WindowsCloudFilesHelperError> {
    #[cfg(target_os = "windows")]
    {
        run_windows_cloud_files_helper(
            "register",
            windows_cloud_files_register_args(state_root, mount, display_name),
        )
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = state_root;
        let _ = display_name;
        Err(WindowsCloudFilesHelperError::UnsupportedPlatform(format!(
            "Windows Cloud Files registration is only supported on Windows; mount `{}` cannot be registered here",
            mount.mount_id.0
        )))
    }
}

pub fn open_windows_cloud_files_sync_root(
    mount: &MountConfig,
) -> Result<FileProviderHelperReport, WindowsCloudFilesHelperError> {
    #[cfg(target_os = "windows")]
    {
        run_windows_cloud_files_helper("open", windows_cloud_files_open_args(mount))
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err(WindowsCloudFilesHelperError::UnsupportedPlatform(format!(
            "Windows Cloud Files opening is only supported on Windows; mount `{}` cannot be opened here",
            mount.mount_id.0
        )))
    }
}

pub fn run_windows_cloud_files_provider(
    state_root: &Path,
    mount: &MountConfig,
) -> Result<FileProviderHelperReport, WindowsCloudFilesHelperError> {
    #[cfg(target_os = "windows")]
    {
        run_windows_cloud_files_helper("run", windows_cloud_files_run_args(state_root, mount))
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = state_root;
        Err(WindowsCloudFilesHelperError::UnsupportedPlatform(format!(
            "Windows Cloud Files provider runtime is only supported on Windows; mount `{}` cannot run here",
            mount.mount_id.0
        )))
    }
}

pub fn run_windows_cloud_files_lifecycle(
    state_root: &Path,
    mount: &MountConfig,
    display_name: &str,
    action: WindowsCloudFilesLifecycleAction,
) -> Result<FileProviderHelperReport, WindowsCloudFilesHelperError> {
    #[cfg(target_os = "windows")]
    {
        match action {
            WindowsCloudFilesLifecycleAction::Start => {
                start_windows_cloud_files_lifecycle(state_root, mount, display_name, action)
            }
            WindowsCloudFilesLifecycleAction::Stop => {
                stop_windows_cloud_files_lifecycle(state_root, mount)
            }
            WindowsCloudFilesLifecycleAction::Status => {
                status_windows_cloud_files_lifecycle(state_root, mount)
            }
            WindowsCloudFilesLifecycleAction::Restart => {
                stop_windows_cloud_files_lifecycle(state_root, mount)?;
                start_windows_cloud_files_lifecycle(state_root, mount, display_name, action)
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = state_root;
        let _ = display_name;
        Err(WindowsCloudFilesHelperError::UnsupportedPlatform(format!(
            "Windows Cloud Files provider lifecycle is only supported on Windows; mount `{}` cannot {} here",
            mount.mount_id.0,
            action.as_str()
        )))
    }
}

pub fn unregister_windows_cloud_files_sync_root(
    state_root: &Path,
    mount_id: &str,
) -> Result<FileProviderHelperReport, WindowsCloudFilesHelperError> {
    #[cfg(target_os = "windows")]
    {
        run_windows_cloud_files_helper(
            "unregister",
            windows_cloud_files_unregister_args(state_root, mount_id),
        )
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = state_root;
        Err(WindowsCloudFilesHelperError::UnsupportedPlatform(format!(
            "Windows Cloud Files unregister is only supported on Windows for `{mount_id}`"
        )))
    }
}

pub fn run_windows_cloud_files_helper(
    action: &str,
    args: Vec<String>,
) -> Result<FileProviderHelperReport, WindowsCloudFilesHelperError> {
    #[cfg(target_os = "windows")]
    {
        let helper =
            windows_cloud_files_helper_path().ok_or(WindowsCloudFilesHelperError::Missing)?;
        let mut command = Command::new(&helper);
        command.arg(action);
        command.args(args);
        command.arg("--json");
        configure_hidden_windows_command(&mut command);

        let output = command
            .output()
            .map_err(|error| WindowsCloudFilesHelperError::Failed(error.to_string()))?;
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let helper_report = serde_json::from_str::<Value>(&stdout)
            .unwrap_or_else(|_| Value::String(stdout.clone()));

        if !output.status.success() {
            let message = helper_report
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|message| !message.is_empty())
                .or_else(|| (!stderr.is_empty()).then_some(stderr))
                .unwrap_or_else(|| format!("afs-cloud-files exited with {}", output.status));
            return Err(WindowsCloudFilesHelperError::Failed(message));
        }

        Ok(FileProviderHelperReport {
            helper,
            helper_report,
        })
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = args;
        Err(WindowsCloudFilesHelperError::UnsupportedPlatform(format!(
            "Windows Cloud Files {action} is only supported on Windows"
        )))
    }
}

#[cfg(target_os = "windows")]
fn start_windows_cloud_files_lifecycle(
    state_root: &Path,
    mount: &MountConfig,
    display_name: &str,
    action: WindowsCloudFilesLifecycleAction,
) -> Result<FileProviderHelperReport, WindowsCloudFilesHelperError> {
    if !daemon_is_running(state_root) {
        return Err(WindowsCloudFilesHelperError::DaemonNotRunning);
    }

    let helper = windows_cloud_files_helper_path().ok_or(WindowsCloudFilesHelperError::Missing)?;
    let existing = read_windows_cloud_files_lifecycle_metadata(state_root, &mount.mount_id.0)?;
    if let Some(metadata) = existing
        && windows_process_is_running(metadata.pid, &metadata.helper)
    {
        let registered =
            windows_cloud_files_registration_marker_exists(state_root, &mount.mount_id.0);
        if !registered {
            register_windows_cloud_files_sync_root(state_root, mount, display_name)?;
        }
        return Ok(windows_cloud_files_lifecycle_report(
            action,
            mount,
            state_root,
            helper,
            true,
            Some(true),
            Some(metadata.pid),
            false,
            WindowsCloudFilesProviderState::Running,
        ));
    }

    register_windows_cloud_files_sync_root(state_root, mount, display_name)?;
    std::fs::create_dir_all(windows_cloud_files_lifecycle_dir(state_root))
        .map_err(|error| WindowsCloudFilesHelperError::Failed(error.to_string()))?;
    let log_dir = windows_cloud_files_log_dir(state_root);
    std::fs::create_dir_all(&log_dir)
        .map_err(|error| WindowsCloudFilesHelperError::Failed(error.to_string()))?;
    let stdout_log = windows_cloud_files_stdout_log_path(state_root, &mount.mount_id.0);
    let stderr_log = windows_cloud_files_stderr_log_path(state_root, &mount.mount_id.0);
    let stdout = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stdout_log)
        .map_err(|error| WindowsCloudFilesHelperError::Failed(error.to_string()))?;
    let stderr = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stderr_log)
        .map_err(|error| WindowsCloudFilesHelperError::Failed(error.to_string()))?;

    let mut command = Command::new(&helper);
    command
        .args(windows_cloud_files_run_command_args(state_root, mount))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    configure_hidden_windows_command(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| WindowsCloudFilesHelperError::Failed(error.to_string()))?;
    let pid = child.id();
    let metadata = WindowsCloudFilesProcessMetadata {
        mount_id: mount.mount_id.0.clone(),
        pid,
        helper: helper.clone(),
        sync_root: mount.root.clone(),
        state_dir: state_root.to_path_buf(),
        stdout_log,
        stderr_log,
    };
    write_windows_cloud_files_lifecycle_metadata(state_root, &metadata)?;

    std::thread::sleep(Duration::from_millis(350));
    match child.try_wait() {
        Ok(None) => Ok(windows_cloud_files_lifecycle_report(
            action,
            mount,
            state_root,
            helper,
            true,
            Some(true),
            Some(pid),
            false,
            WindowsCloudFilesProviderState::Running,
        )),
        Ok(Some(status)) => {
            let _ = remove_windows_cloud_files_lifecycle_metadata(state_root, &mount.mount_id.0);
            Err(WindowsCloudFilesHelperError::Failed(format!(
                "afs-cloud-files exited immediately with {status}; see {}",
                metadata.stderr_log.display()
            )))
        }
        Err(error) => Err(WindowsCloudFilesHelperError::Failed(error.to_string())),
    }
}

#[cfg(target_os = "windows")]
fn stop_windows_cloud_files_lifecycle(
    state_root: &Path,
    mount: &MountConfig,
) -> Result<FileProviderHelperReport, WindowsCloudFilesHelperError> {
    let helper = windows_cloud_files_helper_path().unwrap_or_else(|| {
        read_windows_cloud_files_lifecycle_metadata(state_root, &mount.mount_id.0)
            .ok()
            .flatten()
            .map(|metadata| metadata.helper)
            .unwrap_or_else(|| PathBuf::from("afs-cloud-files"))
    });
    let metadata = read_windows_cloud_files_lifecycle_metadata(state_root, &mount.mount_id.0)?;
    let mut stopped_pid = None;
    if let Some(metadata) = &metadata
        && windows_process_is_running(metadata.pid, &metadata.helper)
    {
        stop_windows_process(metadata.pid)?;
        stopped_pid = Some(metadata.pid);
    }
    let _ = remove_windows_cloud_files_lifecycle_metadata(state_root, &mount.mount_id.0);
    Ok(windows_cloud_files_lifecycle_report(
        WindowsCloudFilesLifecycleAction::Stop,
        mount,
        state_root,
        helper,
        daemon_is_running(state_root),
        windows_cloud_files_registration_status(state_root, &mount.mount_id.0),
        stopped_pid,
        false,
        WindowsCloudFilesProviderState::Stopped,
    ))
}

#[cfg(target_os = "windows")]
fn status_windows_cloud_files_lifecycle(
    state_root: &Path,
    mount: &MountConfig,
) -> Result<FileProviderHelperReport, WindowsCloudFilesHelperError> {
    let metadata = read_windows_cloud_files_lifecycle_metadata(state_root, &mount.mount_id.0)?;
    let helper = windows_cloud_files_helper_path()
        .or_else(|| metadata.as_ref().map(|metadata| metadata.helper.clone()))
        .unwrap_or_else(|| PathBuf::from("afs-cloud-files"));
    let running = metadata
        .as_ref()
        .is_some_and(|metadata| windows_process_is_running(metadata.pid, &metadata.helper));
    let stale_pid_file = metadata.is_some() && !running;
    let pid = metadata.as_ref().map(|metadata| metadata.pid);
    let state = if running {
        WindowsCloudFilesProviderState::Running
    } else {
        WindowsCloudFilesProviderState::Stopped
    };

    Ok(windows_cloud_files_lifecycle_report(
        WindowsCloudFilesLifecycleAction::Status,
        mount,
        state_root,
        helper,
        daemon_is_running(state_root),
        windows_cloud_files_registration_status(state_root, &mount.mount_id.0),
        pid,
        stale_pid_file,
        state,
    ))
}

fn windows_cloud_files_lifecycle_report(
    action: WindowsCloudFilesLifecycleAction,
    mount: &MountConfig,
    state_root: &Path,
    helper: PathBuf,
    daemon_running: bool,
    registered: Option<bool>,
    pid: Option<u32>,
    stale_pid_file: bool,
    state: WindowsCloudFilesProviderState,
) -> FileProviderHelperReport {
    let helper_present = helper.exists();
    let message = windows_cloud_files_lifecycle_message(action, &mount.mount_id.0, state, pid);
    let report = WindowsCloudFilesLifecycleReport {
        message,
        state,
        mount_id: mount.mount_id.0.clone(),
        sync_root: mount.root.display().to_string(),
        state_dir: state_root.display().to_string(),
        helper: helper.display().to_string(),
        helper_present,
        daemon_running,
        registered,
        pid,
        stale_pid_file,
        pid_file: windows_cloud_files_lifecycle_file(state_root, &mount.mount_id.0)
            .display()
            .to_string(),
        stdout_log: windows_cloud_files_stdout_log_path(state_root, &mount.mount_id.0)
            .display()
            .to_string(),
        stderr_log: windows_cloud_files_stderr_log_path(state_root, &mount.mount_id.0)
            .display()
            .to_string(),
    };

    FileProviderHelperReport {
        helper,
        helper_report: serde_json::to_value(report)
            .unwrap_or_else(|error| Value::String(error.to_string())),
    }
}

fn windows_cloud_files_lifecycle_message(
    action: WindowsCloudFilesLifecycleAction,
    mount_id: &str,
    state: WindowsCloudFilesProviderState,
    pid: Option<u32>,
) -> String {
    match (action, state, pid) {
        (
            WindowsCloudFilesLifecycleAction::Start,
            WindowsCloudFilesProviderState::Running,
            Some(pid),
        ) => {
            format!("Windows Cloud Files provider started for `{mount_id}` (pid {pid})")
        }
        (
            WindowsCloudFilesLifecycleAction::Restart,
            WindowsCloudFilesProviderState::Running,
            Some(pid),
        ) => {
            format!("Windows Cloud Files provider restarted for `{mount_id}` (pid {pid})")
        }
        (
            WindowsCloudFilesLifecycleAction::Status,
            WindowsCloudFilesProviderState::Running,
            Some(pid),
        ) => {
            format!("Windows Cloud Files provider is running for `{mount_id}` (pid {pid})")
        }
        (
            WindowsCloudFilesLifecycleAction::Stop,
            WindowsCloudFilesProviderState::Stopped,
            Some(pid),
        ) => {
            format!("Windows Cloud Files provider stopped for `{mount_id}` (pid {pid})")
        }
        (WindowsCloudFilesLifecycleAction::Stop, WindowsCloudFilesProviderState::Stopped, None) => {
            format!("Windows Cloud Files provider is already stopped for `{mount_id}`")
        }
        (WindowsCloudFilesLifecycleAction::Status, WindowsCloudFilesProviderState::Stopped, _) => {
            format!("Windows Cloud Files provider is stopped for `{mount_id}`")
        }
        _ => format!(
            "Windows Cloud Files provider {} complete for `{mount_id}`",
            action.as_str()
        ),
    }
}

#[cfg(target_os = "windows")]
fn windows_cloud_files_registration_status(state_root: &Path, mount_id: &str) -> Option<bool> {
    let report = run_windows_cloud_files_helper(
        "list",
        vec!["--state-dir".to_string(), helper_path_arg(state_root)],
    )
    .ok()?;
    let roots = report
        .helper_report
        .get("roots")
        .and_then(Value::as_array)?;
    Some(roots.iter().any(|root| {
        root.get("mount_id")
            .and_then(Value::as_str)
            .is_some_and(|registered_mount_id| registered_mount_id == mount_id)
    }))
}

#[cfg(target_os = "windows")]
fn windows_cloud_files_registration_marker_exists(state_root: &Path, mount_id: &str) -> bool {
    afs_platform::windows_cloud_files_registration_marker_dir(state_root, mount_id)
        .join("registration.json")
        .exists()
}

fn windows_cloud_files_lifecycle_dir(state_root: &Path) -> PathBuf {
    state_root.join("cloud-files-lifecycle")
}

fn windows_cloud_files_lifecycle_file(state_root: &Path, mount_id: &str) -> PathBuf {
    windows_cloud_files_lifecycle_dir(state_root).join(format!(
        "{}.json",
        windows_cloud_files_lifecycle_fragment(mount_id)
    ))
}

fn windows_cloud_files_log_dir(state_root: &Path) -> PathBuf {
    state_root.join("logs")
}

fn windows_cloud_files_stdout_log_path(state_root: &Path, mount_id: &str) -> PathBuf {
    windows_cloud_files_log_dir(state_root).join(format!(
        "afs-cloud-files.{}.out.log",
        windows_cloud_files_lifecycle_fragment(mount_id)
    ))
}

fn windows_cloud_files_stderr_log_path(state_root: &Path, mount_id: &str) -> PathBuf {
    windows_cloud_files_log_dir(state_root).join(format!(
        "afs-cloud-files.{}.err.log",
        windows_cloud_files_lifecycle_fragment(mount_id)
    ))
}

fn windows_cloud_files_lifecycle_fragment(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let sanitized = if sanitized.is_empty() {
        "mount".to_string()
    } else {
        sanitized
    };
    format!("{sanitized}-{:016x}", stable_lifecycle_hash(value))
}

fn stable_lifecycle_hash(value: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(target_os = "windows")]
fn read_windows_cloud_files_lifecycle_metadata(
    state_root: &Path,
    mount_id: &str,
) -> Result<Option<WindowsCloudFilesProcessMetadata>, WindowsCloudFilesHelperError> {
    let path = windows_cloud_files_lifecycle_file(state_root, mount_id);
    match std::fs::read_to_string(&path) {
        Ok(json) => serde_json::from_str(&json)
            .map(Some)
            .map_err(|error| WindowsCloudFilesHelperError::Failed(error.to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(WindowsCloudFilesHelperError::Failed(error.to_string())),
    }
}

#[cfg(target_os = "windows")]
fn write_windows_cloud_files_lifecycle_metadata(
    state_root: &Path,
    metadata: &WindowsCloudFilesProcessMetadata,
) -> Result<(), WindowsCloudFilesHelperError> {
    let dir = windows_cloud_files_lifecycle_dir(state_root);
    std::fs::create_dir_all(&dir)
        .map_err(|error| WindowsCloudFilesHelperError::Failed(error.to_string()))?;
    let json = serde_json::to_string_pretty(metadata)
        .map_err(|error| WindowsCloudFilesHelperError::Failed(error.to_string()))?;
    std::fs::write(
        windows_cloud_files_lifecycle_file(state_root, &metadata.mount_id),
        json,
    )
    .map_err(|error| WindowsCloudFilesHelperError::Failed(error.to_string()))
}

#[cfg(target_os = "windows")]
fn remove_windows_cloud_files_lifecycle_metadata(
    state_root: &Path,
    mount_id: &str,
) -> Result<(), WindowsCloudFilesHelperError> {
    let path = windows_cloud_files_lifecycle_file(state_root, mount_id);
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(WindowsCloudFilesHelperError::Failed(error.to_string())),
    }
}

#[cfg(target_os = "windows")]
fn windows_process_is_running(pid: u32, helper: &Path) -> bool {
    let filter = format!("PID eq {pid}");
    let mut command = Command::new("tasklist");
    command.args(["/FI", &filter, "/FO", "CSV", "/NH"]);
    configure_hidden_windows_command(&mut command);
    let Ok(output) = command.output() else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected = helper
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("afs-cloud-files.exe");
    let pid = pid.to_string();
    stdout.lines().any(|line| {
        let columns = parse_tasklist_csv_line(line);
        let image = columns.first().map(String::as_str).unwrap_or_default();
        let task_pid = columns.get(1).map(String::as_str).unwrap_or_default();
        task_pid == pid
            && (image.eq_ignore_ascii_case(expected)
                || image.eq_ignore_ascii_case("afs-cloud-files.exe"))
    })
}

#[cfg(target_os = "windows")]
fn parse_tasklist_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars().peekable();
    let mut in_quotes = false;
    while let Some(character) = chars.next() {
        match character {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                current.push('"');
                let _ = chars.next();
            }
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                fields.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(character),
        }
    }
    fields.push(current.trim().to_string());
    fields
}

#[cfg(target_os = "windows")]
fn stop_windows_process(pid: u32) -> Result<(), WindowsCloudFilesHelperError> {
    let mut command = Command::new("taskkill");
    command.args(["/PID", &pid.to_string(), "/T", "/F"]);
    configure_hidden_windows_command(&mut command);
    let output = command
        .output()
        .map_err(|error| WindowsCloudFilesHelperError::Failed(error.to_string()))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let message = if stderr.is_empty() { stdout } else { stderr };
    Err(WindowsCloudFilesHelperError::Failed(
        if message.is_empty() {
            format!("taskkill exited with {}", output.status)
        } else {
            message
        },
    ))
}

#[cfg(target_os = "windows")]
fn configure_hidden_windows_command(command: &mut Command) {
    command.creation_flags(HIDDEN_WINDOWS_PROCESS_FLAGS);
}

fn windows_cloud_files_register_args(
    state_root: &Path,
    mount: &MountConfig,
    display_name: &str,
) -> Vec<String> {
    vec![
        "--mount-id".to_string(),
        mount.mount_id.0.clone(),
        "--display-name".to_string(),
        display_name.to_string(),
        "--sync-root".to_string(),
        helper_path_arg(&mount.root),
        "--state-dir".to_string(),
        helper_path_arg(state_root),
    ]
}

fn windows_cloud_files_open_args(mount: &MountConfig) -> Vec<String> {
    vec![
        "--mount-id".to_string(),
        mount.mount_id.0.clone(),
        "--sync-root".to_string(),
        helper_path_arg(&mount.root),
    ]
}

fn windows_cloud_files_run_args(state_root: &Path, mount: &MountConfig) -> Vec<String> {
    vec![
        "--mount-id".to_string(),
        mount.mount_id.0.clone(),
        "--sync-root".to_string(),
        helper_path_arg(&mount.root),
        "--state-dir".to_string(),
        helper_path_arg(state_root),
    ]
}

pub fn windows_cloud_files_run_command_args(state_root: &Path, mount: &MountConfig) -> Vec<String> {
    let mut args = vec!["run".to_string()];
    args.extend(windows_cloud_files_run_args(state_root, mount));
    args
}

fn windows_cloud_files_unregister_args(state_root: &Path, mount_id: &str) -> Vec<String> {
    vec![
        "--mount-id".to_string(),
        mount_id.to_string(),
        "--state-dir".to_string(),
        helper_path_arg(state_root),
    ]
}

fn helper_path_arg(path: &Path) -> String {
    absolute_helper_path(path).display().to_string()
}

#[cfg(target_os = "windows")]
fn absolute_helper_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }

    std::env::current_dir()
        .map(|current_dir| current_dir.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(not(target_os = "windows"))]
fn absolute_helper_path(path: &Path) -> PathBuf {
    path.to_path_buf()
}

#[cfg(target_os = "windows")]
pub fn windows_cloud_files_helper_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("AFS_CLOUD_FILES_BIN") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }

    let helper_name = afs_platform::executable_filename("afs-cloud-files");
    let mut candidates = Vec::new();
    if let Ok(current_exe) = std::env::current_exe()
        && let Some(dir) = current_exe.parent()
    {
        candidates.push(dir.join(&helper_name));
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest_dir.join("../..");
    candidates.push(workspace.join("target/debug").join(&helper_name));
    candidates.push(workspace.join("target/release").join(&helper_name));

    if let Some(path) = candidates.into_iter().find(|path| path.exists()) {
        return Some(path);
    }
    find_on_path(&helper_name)
}

#[cfg(not(target_os = "windows"))]
pub fn windows_cloud_files_helper_path() -> Option<PathBuf> {
    None
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
    macos_file_provider_domain_path(root)
        .file_name()
        .and_then(|name| name.to_str())
        .map(strip_file_provider_directory_prefix)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| fallback.to_string())
}

fn macos_file_provider_domain_path(root: &Path) -> &Path {
    let Some(parent) = root.parent() else {
        return root;
    };
    let Some(grandparent_name) = parent
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
    else {
        return root;
    };
    if grandparent_name == "CloudStorage" {
        parent
    } else {
        root
    }
}

fn strip_file_provider_directory_prefix(name: &str) -> &str {
    name.strip_prefix("AgentFS-")
        .or_else(|| name.strip_prefix("AFS-"))
        .filter(|stripped| !stripped.is_empty())
        .unwrap_or(name)
}

pub fn run_macos_file_provider_helper(
    action: &str,
    args: Vec<String>,
) -> Result<FileProviderHelperReport, FileProviderHelperError> {
    let helper = macos_file_provider_helper_path().ok_or(FileProviderHelperError::Missing)?;
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

pub fn macos_file_provider_helper_path() -> Option<PathBuf> {
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

#[cfg(any(target_os = "linux", target_os = "windows"))]
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
pub fn afs_fuse_helper_path() -> Option<PathBuf> {
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

#[cfg(not(target_os = "linux"))]
pub fn afs_fuse_helper_path() -> Option<PathBuf> {
    None
}

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

#[cfg(any(target_os = "linux", target_os = "windows"))]
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
    use afs_core::model::MountId;
    use afs_store::{MountConfig, ProjectionMode};

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
                std::path::Path::new("/Users/example/Library/CloudStorage/AFS-Notion"),
                "fallback",
            ),
            "Notion"
        );
        assert_eq!(
            super::macos_file_provider_display_name(
                std::path::Path::new("/Users/example/Library/CloudStorage/AFS/notion"),
                "fallback",
            ),
            "AFS"
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

    #[test]
    fn windows_cloud_files_register_args_are_stable_helper_contract() {
        let mount = MountConfig::new(MountId::new("notion-main"), "notion", r"C:\Users\Ada\AFS")
            .projection(ProjectionMode::WindowsCloudFiles);

        assert_eq!(
            super::windows_cloud_files_register_args(
                std::path::Path::new(r"C:\Users\Ada\AppData\Local\AgentFS"),
                &mount,
                "Notion"
            ),
            vec![
                "--mount-id",
                "notion-main",
                "--display-name",
                "Notion",
                "--sync-root",
                r"C:\Users\Ada\AFS",
                "--state-dir",
                r"C:\Users\Ada\AppData\Local\AgentFS",
            ]
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_cloud_files_helper_args_absolutize_relative_state_dir() {
        let mount = MountConfig::new(MountId::new("notion-main"), "notion", r"C:\Users\Ada\AFS")
            .projection(ProjectionMode::WindowsCloudFiles);
        let current_dir = std::env::current_dir().expect("current dir");
        let expected_state_dir = current_dir.join(".afs").display().to_string();

        assert_eq!(
            super::windows_cloud_files_register_args(
                std::path::Path::new(".afs"),
                &mount,
                "Notion"
            ),
            vec![
                "--mount-id",
                "notion-main",
                "--display-name",
                "Notion",
                "--sync-root",
                r"C:\Users\Ada\AFS",
                "--state-dir",
                &expected_state_dir,
            ]
        );
    }

    #[test]
    fn windows_cloud_files_open_and_unregister_args_are_stable_helper_contract() {
        let mount = MountConfig::new(MountId::new("notion-main"), "notion", r"C:\Users\Ada\AFS")
            .projection(ProjectionMode::WindowsCloudFiles);

        assert_eq!(
            super::windows_cloud_files_open_args(&mount),
            vec![
                "--mount-id",
                "notion-main",
                "--sync-root",
                r"C:\Users\Ada\AFS"
            ]
        );
        assert_eq!(
            super::windows_cloud_files_run_args(
                std::path::Path::new(r"C:\Users\Ada\AppData\Local\AgentFS"),
                &mount,
            ),
            vec![
                "--mount-id",
                "notion-main",
                "--sync-root",
                r"C:\Users\Ada\AFS",
                "--state-dir",
                r"C:\Users\Ada\AppData\Local\AgentFS",
            ]
        );
        assert_eq!(
            super::windows_cloud_files_run_command_args(
                std::path::Path::new(r"C:\Users\Ada\AppData\Local\AgentFS"),
                &mount,
            ),
            vec![
                "run",
                "--mount-id",
                "notion-main",
                "--sync-root",
                r"C:\Users\Ada\AFS",
                "--state-dir",
                r"C:\Users\Ada\AppData\Local\AgentFS",
            ]
        );
        assert_eq!(
            super::windows_cloud_files_unregister_args(
                std::path::Path::new(r"C:\Users\Ada\AppData\Local\AgentFS"),
                "notion-main"
            ),
            vec![
                "--mount-id",
                "notion-main",
                "--state-dir",
                r"C:\Users\Ada\AppData\Local\AgentFS",
            ]
        );
    }

    #[test]
    fn windows_cloud_files_lifecycle_paths_are_mount_specific_and_stable() {
        let state_root = std::path::Path::new(r"C:\Users\Ada\AppData\Local\AgentFS");
        let mount_id = "notion/main";
        let fragment = super::windows_cloud_files_lifecycle_fragment(mount_id);

        assert!(fragment.starts_with("notion_main-"));
        assert_eq!(
            super::windows_cloud_files_lifecycle_file(state_root, mount_id),
            state_root
                .join("cloud-files-lifecycle")
                .join(format!("{fragment}.json"))
        );
        assert_eq!(
            super::windows_cloud_files_stdout_log_path(state_root, mount_id),
            state_root
                .join("logs")
                .join(format!("afs-cloud-files.{fragment}.out.log"))
        );
        assert_eq!(
            super::windows_cloud_files_stderr_log_path(state_root, mount_id),
            state_root
                .join("logs")
                .join(format!("afs-cloud-files.{fragment}.err.log"))
        );
    }

    #[test]
    fn windows_cloud_files_registration_marker_paths_escape_mount_ids() {
        let state_root = std::path::Path::new(r"C:\Users\Ada\AppData\Local\AgentFS");
        let marker_path =
            afs_platform::windows_cloud_files_registration_marker_dir(state_root, "notion/main")
                .join("registration.json");

        assert_eq!(
            marker_path,
            state_root
                .join("cloud-files")
                .join("notion%2Fmain")
                .join("registration.json")
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn tasklist_csv_parser_extracts_pid_column() {
        assert_eq!(
            super::parse_tasklist_csv_line(
                "\"afs-cloud-files.exe\",\"1234\",\"Console\",\"1\",\"10,000 K\""
            ),
            vec!["afs-cloud-files.exe", "1234", "Console", "1", "10,000 K"]
        );
    }
}
