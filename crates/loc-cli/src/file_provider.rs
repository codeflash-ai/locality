//! Shared helpers for registering and opening platform virtual filesystem domains.
//!
//! The macOS File Provider control surface lives in the Swift helper bundled
//! with the File Provider extension. Rust entrypoints call this module rather
//! than shelling through `loc file-provider`, so the CLI and desktop app share
//! the same platform boundary.

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(target_os = "windows")]
use std::process::Stdio;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use std::time::Duration;

use locality_store::MountConfig;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use localityd::ipc::{DaemonRequest, send_request_with_timeout};
#[cfg(target_os = "windows")]
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(any(target_os = "linux", target_os = "windows"))]
const DEFAULT_DAEMON_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_os = "linux")]
const LINUX_FUSE_ROOT_HINT_MAX_BYTES: usize = 48;
#[cfg(target_os = "linux")]
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
#[cfg(target_os = "linux")]
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
#[cfg(target_os = "windows")]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
#[cfg(target_os = "windows")]
const DETACHED_PROCESS: u32 = 0x0000_0008;
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(target_os = "windows")]
const HIDDEN_WINDOWS_PROCESS_FLAGS: u32 =
    CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_NO_WINDOW;
#[cfg(any(test, target_os = "windows"))]
const WINDOWS_CLOUD_FILES_SYNC_ROOT_ID_PREFIX: &str = "codeflash.ai.loc!default!";
#[cfg(any(test, target_os = "windows"))]
const WINDOWS_CLOUD_FILES_SHARED_SYNC_ROOT_COMPONENT: &str = "locality";

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
                "locality-file-providerctl was not found; install Locality.app, or from a source checkout run `make install-macos-file-provider`"
                    .to_string()
            }
            Self::Failed(message) if macos_file_provider_application_unavailable(message) => {
                let message = message.trim_end_matches('.');
                format!(
                    "{message}. The Locality macOS File Provider app or extension is not available to macOS. For local development, run `make install-macos-file-provider`, then reopen Locality and enable the File Provider if macOS asks."
                )
            }
            Self::Failed(message) => message.clone(),
        }
    }
}

fn macos_file_provider_application_unavailable(message: &str) -> bool {
    message.contains("The application cannot be used right now")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinuxFuseRegistrationReport {
    pub service: String,
    pub unit_path: PathBuf,
    pub mountpoint: PathBuf,
    pub locality_fuse: PathBuf,
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
                "localityd is not running; start it with `loc daemon start` before registering the FUSE mount"
                    .to_string()
            }
            Self::HelperMissing => {
                "locality-fuse was not found; build or install the locality-fuse binary".to_string()
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
                "localityd is not running; start it with `loc daemon start` before starting the Windows Cloud Files provider"
                    .to_string()
            }
            Self::Missing => {
                "locality-cloud-files was not found; build or install the Windows Cloud Files helper"
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

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WindowsCloudFilesProviderState {
    Running,
    Stopped,
}

#[cfg(target_os = "windows")]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct WindowsCloudFilesProcessMetadata {
    #[serde(alias = "mount_id")]
    root_id: String,
    pid: u32,
    helper: PathBuf,
    sync_root: PathBuf,
    state_dir: PathBuf,
    stdout_log: PathBuf,
    stderr_log: PathBuf,
}

#[cfg(target_os = "windows")]
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct WindowsCloudFilesLifecycleReport {
    message: String,
    state: WindowsCloudFilesProviderState,
    mount_id: String,
    root_id: String,
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

    let locality_fuse =
        locality_fuse_helper_path().ok_or(LinuxFuseRegistrationError::HelperMissing)?;
    let root_id = linux_fuse_root_id(mount);
    let service = linux_fuse_unit_name(&root_id);
    let unit_path = linux_fuse_unit_path(&service)?;
    write_linux_fuse_unit(&unit_path, &locality_fuse, state_root, mount)?;
    run_systemctl_user(&["daemon-reload"])?;
    run_systemctl_user(&["enable", &service])?;
    run_systemctl_user(&["restart", &service])?;

    Ok(LinuxFuseRegistrationReport {
        service,
        unit_path,
        mountpoint: localityd::virtual_fs::virtual_projection_root(mount),
        locality_fuse,
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
                .unwrap_or_else(|| format!("locality-cloud-files exited with {}", output.status));
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
    let root_id = windows_cloud_files_lifecycle_root_id(mount);
    let existing = read_windows_cloud_files_lifecycle_metadata_for_mount(state_root, mount)?;
    if let Some(metadata) = existing
        && windows_process_is_running(metadata.pid, &metadata.helper)
    {
        let registered = windows_cloud_files_registration_marker_exists(state_root, mount);
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
    let stdout_log = windows_cloud_files_stdout_log_path(state_root, &root_id);
    let stderr_log = windows_cloud_files_stderr_log_path(state_root, &root_id);
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
        root_id: root_id.clone(),
        pid,
        helper: helper.clone(),
        sync_root: windows_cloud_files_projection_root(mount),
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
            let _ = remove_windows_cloud_files_lifecycle_metadata(state_root, &root_id);
            Err(WindowsCloudFilesHelperError::Failed(format!(
                "locality-cloud-files exited immediately with {status}; see {}",
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
    let root_id = windows_cloud_files_lifecycle_root_id(mount);
    let helper = windows_cloud_files_helper_path().unwrap_or_else(|| {
        read_windows_cloud_files_lifecycle_metadata(state_root, &root_id)
            .ok()
            .flatten()
            .filter(|metadata| {
                windows_cloud_files_lifecycle_metadata_matches_mount(&metadata.sync_root, mount)
            })
            .or_else(|| {
                read_windows_cloud_files_lifecycle_metadata(state_root, &mount.mount_id.0)
                    .ok()
                    .flatten()
                    .filter(|metadata| {
                        windows_cloud_files_lifecycle_metadata_matches_mount(
                            &metadata.sync_root,
                            mount,
                        )
                    })
            })
            .map(|metadata| metadata.helper)
            .unwrap_or_else(|| PathBuf::from("locality-cloud-files"))
    });
    let metadata = read_windows_cloud_files_lifecycle_metadata_for_mount(state_root, mount)?;
    let mut stopped_pid = None;
    if let Some(metadata) = &metadata
        && windows_process_is_running(metadata.pid, &metadata.helper)
    {
        stop_windows_process(metadata.pid)?;
        stopped_pid = Some(metadata.pid);
    }
    let _ = remove_windows_cloud_files_lifecycle_metadata(state_root, &root_id);
    let _ = remove_windows_cloud_files_lifecycle_metadata(state_root, &mount.mount_id.0);
    Ok(windows_cloud_files_lifecycle_report(
        WindowsCloudFilesLifecycleAction::Stop,
        mount,
        state_root,
        helper,
        daemon_is_running(state_root),
        windows_cloud_files_registration_status(state_root, mount),
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
    let metadata = read_windows_cloud_files_lifecycle_metadata_for_mount(state_root, mount)?;
    let helper = windows_cloud_files_helper_path()
        .or_else(|| metadata.as_ref().map(|metadata| metadata.helper.clone()))
        .unwrap_or_else(|| PathBuf::from("locality-cloud-files"));
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
        windows_cloud_files_registration_status(state_root, mount),
        pid,
        stale_pid_file,
        state,
    ))
}

#[cfg(target_os = "windows")]
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
    let root_id = windows_cloud_files_lifecycle_root_id(mount);
    let report = WindowsCloudFilesLifecycleReport {
        message,
        state,
        mount_id: mount.mount_id.0.clone(),
        root_id: root_id.clone(),
        sync_root: windows_cloud_files_projection_root(mount)
            .display()
            .to_string(),
        state_dir: state_root.display().to_string(),
        helper: helper.display().to_string(),
        helper_present,
        daemon_running,
        registered,
        pid,
        stale_pid_file,
        pid_file: windows_cloud_files_lifecycle_file(state_root, &root_id)
            .display()
            .to_string(),
        stdout_log: windows_cloud_files_stdout_log_path(state_root, &root_id)
            .display()
            .to_string(),
        stderr_log: windows_cloud_files_stderr_log_path(state_root, &root_id)
            .display()
            .to_string(),
    };

    FileProviderHelperReport {
        helper,
        helper_report: serde_json::to_value(report)
            .unwrap_or_else(|error| Value::String(error.to_string())),
    }
}

#[cfg(target_os = "windows")]
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
fn windows_cloud_files_registration_status(state_root: &Path, mount: &MountConfig) -> Option<bool> {
    let report = run_windows_cloud_files_helper(
        "list",
        vec!["--state-dir".to_string(), helper_path_arg(state_root)],
    )
    .ok()?;
    let roots = report
        .helper_report
        .get("roots")
        .and_then(Value::as_array)?;
    Some(windows_cloud_files_roots_contain_registration(roots, mount))
}

#[cfg(any(test, target_os = "windows"))]
fn windows_cloud_files_roots_contain_registration(roots: &[Value], mount: &MountConfig) -> bool {
    let expected_sync_root_id = windows_cloud_files_shared_sync_root_id(mount);
    let expected_sync_root_key =
        windows_cloud_files_path_key(&windows_cloud_files_projection_root(mount));
    roots.iter().any(|root| {
        let sync_root_id_matches = root
            .get("sync_root_id")
            .or_else(|| root.get("id"))
            .and_then(Value::as_str)
            .is_some_and(|sync_root_id| sync_root_id == expected_sync_root_id);
        let legacy_mount_matches = root
            .get("mount_id")
            .and_then(Value::as_str)
            .is_some_and(|registered_mount_id| registered_mount_id == mount.mount_id.0);
        let legacy_shared_path_matches = root
            .get("sync_root_id")
            .or_else(|| root.get("id"))
            .and_then(Value::as_str)
            .is_some_and(|sync_root_id| sync_root_id == "codeflash.ai.loc!default!locality")
            && root
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|path| {
                    windows_cloud_files_path_key(Path::new(path)) == expected_sync_root_key
                });

        sync_root_id_matches || legacy_mount_matches || legacy_shared_path_matches
    })
}

#[cfg(target_os = "windows")]
fn windows_cloud_files_registration_marker_exists(state_root: &Path, mount: &MountConfig) -> bool {
    let expected_sync_root_id = windows_cloud_files_shared_sync_root_id(mount);
    let expected_sync_root_key =
        windows_cloud_files_path_key(&windows_cloud_files_projection_root(mount));
    windows_cloud_files_marker_matches(
        state_root,
        &expected_sync_root_id,
        &expected_sync_root_id,
        &expected_sync_root_key,
    ) || windows_cloud_files_marker_matches(
        state_root,
        &mount.mount_id.0,
        &expected_sync_root_id,
        &expected_sync_root_key,
    ) || windows_cloud_files_marker_matches(
        state_root,
        WINDOWS_CLOUD_FILES_SHARED_SYNC_ROOT_COMPONENT,
        &expected_sync_root_id,
        &expected_sync_root_key,
    )
}

#[cfg(target_os = "windows")]
fn windows_cloud_files_marker_matches(
    state_root: &Path,
    marker_key: &str,
    expected_sync_root_id: &str,
    expected_sync_root_key: &str,
) -> bool {
    let marker_path =
        locality_platform::windows_cloud_files_registration_marker_dir(state_root, marker_key)
            .join("registration.json");
    let Ok(json) = std::fs::read_to_string(marker_path) else {
        return false;
    };
    let Ok(marker) = serde_json::from_str::<Value>(&json) else {
        return false;
    };
    marker
        .get("sync_root_id")
        .or_else(|| marker.get("id"))
        .and_then(Value::as_str)
        .is_some_and(|sync_root_id| sync_root_id == expected_sync_root_id)
        || marker
            .get("path")
            .or_else(|| marker.get("sync_root"))
            .and_then(Value::as_str)
            .is_some_and(|path| {
                windows_cloud_files_path_key(Path::new(path)) == expected_sync_root_key
            })
}

#[cfg(any(test, target_os = "windows"))]
fn windows_cloud_files_lifecycle_dir(state_root: &Path) -> PathBuf {
    state_root.join("cloud-files-lifecycle")
}

#[cfg(any(test, target_os = "windows"))]
fn windows_cloud_files_lifecycle_file(state_root: &Path, mount_id: &str) -> PathBuf {
    windows_cloud_files_lifecycle_dir(state_root).join(format!(
        "{}.json",
        windows_cloud_files_lifecycle_fragment(mount_id)
    ))
}

#[cfg(any(test, target_os = "windows"))]
fn windows_cloud_files_log_dir(state_root: &Path) -> PathBuf {
    state_root.join("logs")
}

#[cfg(any(test, target_os = "windows"))]
fn windows_cloud_files_stdout_log_path(state_root: &Path, mount_id: &str) -> PathBuf {
    windows_cloud_files_log_dir(state_root).join(format!(
        "locality-cloud-files.{}.out.log",
        windows_cloud_files_lifecycle_fragment(mount_id)
    ))
}

#[cfg(any(test, target_os = "windows"))]
fn windows_cloud_files_stderr_log_path(state_root: &Path, mount_id: &str) -> PathBuf {
    windows_cloud_files_log_dir(state_root).join(format!(
        "locality-cloud-files.{}.err.log",
        windows_cloud_files_lifecycle_fragment(mount_id)
    ))
}

#[cfg(any(test, target_os = "windows"))]
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

#[cfg(target_os = "windows")]
fn windows_cloud_files_lifecycle_root_id(mount: &MountConfig) -> String {
    let root = windows_cloud_files_projection_root(mount);
    let root = root.display().to_string();
    format!("root-{}", windows_cloud_files_lifecycle_fragment(&root))
}

#[cfg(any(test, target_os = "windows"))]
fn windows_cloud_files_lifecycle_metadata_matches_mount(
    metadata_sync_root: &Path,
    mount: &MountConfig,
) -> bool {
    windows_cloud_files_path_key(metadata_sync_root)
        == windows_cloud_files_path_key(&windows_cloud_files_projection_root(mount))
}

#[cfg(any(test, target_os = "windows"))]
fn windows_cloud_files_path_key(path: &Path) -> String {
    let mut value = path.display().to_string().replace('/', "\\");
    while value.ends_with('\\') && value.len() > 3 {
        value.pop();
    }
    value.to_ascii_lowercase()
}

#[cfg(any(test, target_os = "windows"))]
fn stable_lifecycle_hash(value: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(target_os = "windows")]
fn read_windows_cloud_files_lifecycle_metadata_for_mount(
    state_root: &Path,
    mount: &MountConfig,
) -> Result<Option<WindowsCloudFilesProcessMetadata>, WindowsCloudFilesHelperError> {
    let root_id = windows_cloud_files_lifecycle_root_id(mount);
    let shared =
        read_windows_cloud_files_lifecycle_metadata(state_root, &root_id)?.filter(|metadata| {
            windows_cloud_files_lifecycle_metadata_matches_mount(&metadata.sync_root, mount)
        });
    if shared.is_some() {
        return Ok(shared);
    }

    Ok(
        read_windows_cloud_files_lifecycle_metadata(state_root, &mount.mount_id.0)?.filter(
            |metadata| {
                windows_cloud_files_lifecycle_metadata_matches_mount(&metadata.sync_root, mount)
            },
        ),
    )
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
        windows_cloud_files_lifecycle_file(state_root, &metadata.root_id),
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
        .unwrap_or("locality-cloud-files.exe");
    let pid = pid.to_string();
    stdout.lines().any(|line| {
        let columns = parse_tasklist_csv_line(line);
        let image = columns.first().map(String::as_str).unwrap_or_default();
        let task_pid = columns.get(1).map(String::as_str).unwrap_or_default();
        task_pid == pid
            && (image.eq_ignore_ascii_case(expected)
                || image.eq_ignore_ascii_case("locality-cloud-files.exe"))
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

#[cfg(any(test, target_os = "windows"))]
fn windows_cloud_files_register_args(
    state_root: &Path,
    mount: &MountConfig,
    _display_name: &str,
) -> Vec<String> {
    vec![
        "--display-name".to_string(),
        "Locality".to_string(),
        "--sync-root".to_string(),
        helper_path_arg(&windows_cloud_files_projection_root(mount)),
        "--state-dir".to_string(),
        helper_path_arg(state_root),
    ]
}

#[cfg(any(test, target_os = "windows"))]
fn windows_cloud_files_open_args(mount: &MountConfig) -> Vec<String> {
    vec![
        "--sync-root".to_string(),
        helper_path_arg(&windows_cloud_files_projection_root(mount)),
    ]
}

fn windows_cloud_files_run_args(state_root: &Path, mount: &MountConfig) -> Vec<String> {
    vec![
        "--sync-root".to_string(),
        helper_path_arg(&windows_cloud_files_projection_root(mount)),
        "--state-dir".to_string(),
        helper_path_arg(state_root),
    ]
}

pub fn windows_cloud_files_run_command_args(state_root: &Path, mount: &MountConfig) -> Vec<String> {
    let mut args = vec!["run".to_string()];
    args.extend(windows_cloud_files_run_args(state_root, mount));
    args
}

#[cfg(any(test, target_os = "windows"))]
fn windows_cloud_files_unregister_args(state_root: &Path, mount_id: &str) -> Vec<String> {
    vec![
        "--mount-id".to_string(),
        mount_id.to_string(),
        "--state-dir".to_string(),
        helper_path_arg(state_root),
    ]
}

fn windows_cloud_files_projection_root(mount: &MountConfig) -> PathBuf {
    let root = localityd::virtual_fs::virtual_projection_root(mount);
    if !root.as_os_str().is_empty() {
        return root;
    }

    windows_style_parent(&mount.root).unwrap_or(root)
}

#[cfg(any(test, target_os = "windows"))]
fn windows_cloud_files_shared_sync_root_id(mount: &MountConfig) -> String {
    windows_cloud_files_shared_sync_root_id_for_projection_root(
        &windows_cloud_files_projection_root(mount),
    )
}

#[cfg(any(test, target_os = "windows"))]
fn windows_cloud_files_shared_sync_root_id_for_projection_root(sync_root: &Path) -> String {
    format!(
        "{WINDOWS_CLOUD_FILES_SYNC_ROOT_ID_PREFIX}{WINDOWS_CLOUD_FILES_SHARED_SYNC_ROOT_COMPONENT}-{:016x}",
        stable_lifecycle_hash(&windows_cloud_files_path_key(sync_root))
    )
}

fn windows_style_parent(path: &Path) -> Option<PathBuf> {
    let value = path.to_str()?;
    let (parent, _) = value.rsplit_once(['\\', '/'])?;
    if parent.is_empty() {
        None
    } else {
        Some(PathBuf::from(parent))
    }
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
    if let Ok(path) = std::env::var("LOCALITY_CLOUD_FILES_BIN") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }

    let helper_name = locality_platform::executable_filename("locality-cloud-files");
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
                "locality-file-providerctl did not return a CloudStorage URL".to_string(),
            )
        })?;
    Ok((report, PathBuf::from(url)))
}

pub fn macos_file_provider_display_name(root: &Path, fallback: &str) -> String {
    let Some(name) = macos_file_provider_domain_path(root)
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return fallback.to_string();
    };
    if name == "Locality" {
        return String::new();
    }
    let stripped = strip_file_provider_directory_prefix(name);
    if stripped.is_empty() {
        fallback.to_string()
    } else {
        stripped.to_string()
    }
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
    name.strip_prefix("Locality-")
        .or_else(|| name.strip_prefix("Locality-"))
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
            .unwrap_or_else(|| format!("locality-file-providerctl exited with {}", output.status));
        return Err(FileProviderHelperError::Failed(message));
    }

    Ok(FileProviderHelperReport {
        helper,
        helper_report,
    })
}

pub fn macos_file_provider_helper_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("LOCALITY_FILE_PROVIDERCTL") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }

    let mut candidates = Vec::new();
    if let Ok(current_exe) = std::env::current_exe()
        && let Some(dir) = current_exe.parent()
    {
        candidates.push(dir.join("locality-file-providerctl"));
    }

    candidates.push(PathBuf::from(
        "/Applications/Locality.app/Contents/MacOS/locality-file-providerctl",
    ));
    candidates.push(PathBuf::from(
        "/Applications/Locality.app/Contents/MacOS/locality-file-providerctl",
    ));
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(
            PathBuf::from(&home)
                .join("Applications/Locality.app/Contents/MacOS/locality-file-providerctl"),
        );
        candidates.push(
            PathBuf::from(home)
                .join("Applications/Locality.app/Contents/MacOS/locality-file-providerctl"),
        );
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let package_dir = manifest_dir.join("../../platform/macos/LocalityFileProvider");
    candidates.push(
        package_dir.join(".build/dev-bundle/Locality.app/Contents/MacOS/locality-file-providerctl"),
    );
    candidates.push(
        package_dir.join(".build/dev-bundle/Locality.app/Contents/MacOS/locality-file-providerctl"),
    );
    candidates.push(package_dir.join(".build/debug/locality-file-providerctl"));
    candidates.push(package_dir.join(".build/release/locality-file-providerctl"));

    candidates.into_iter().find(|path| path.exists())
}

#[cfg(target_os = "linux")]
fn write_linux_fuse_unit(
    unit_path: &Path,
    locality_fuse: &Path,
    state_root: &Path,
    mount: &MountConfig,
) -> Result<(), LinuxFuseRegistrationError> {
    let log_dir = state_root.join("logs");
    std::fs::create_dir_all(unit_path.parent().unwrap_or_else(|| Path::new(".")))
        .map_err(|error| LinuxFuseRegistrationError::Io(error.to_string()))?;
    std::fs::create_dir_all(&log_dir)
        .map_err(|error| LinuxFuseRegistrationError::Io(error.to_string()))?;
    let projection_root = localityd::virtual_fs::virtual_projection_root(mount);
    std::fs::create_dir_all(&projection_root)
        .map_err(|error| LinuxFuseRegistrationError::Io(error.to_string()))?;

    let log_path = log_dir.join(format!("locality-fuse.{}.log", linux_fuse_root_id(mount)));
    let unit = linux_fuse_unit_contents(locality_fuse, state_root, mount, &log_path);
    std::fs::write(unit_path, unit)
        .map_err(|error| LinuxFuseRegistrationError::Io(error.to_string()))
}

#[cfg(target_os = "linux")]
pub(crate) fn linux_fuse_unit_contents(
    locality_fuse: &Path,
    state_root: &Path,
    mount: &MountConfig,
    log_path: &Path,
) -> String {
    let projection_root = localityd::virtual_fs::virtual_projection_root(mount);
    format!(
        "[Unit]\nDescription=Locality FUSE root for {mountpoint}\nAfter=default.target\n\n[Service]\nType=simple\nExecStart={locality_fuse} --state-dir {state_root} --mountpoint {mountpoint}\nExecStop=/usr/bin/fusermount3 -uz {mountpoint}\nKillSignal=SIGINT\nTimeoutStopSec=10\nLimitCORE=0\nRestart=on-failure\nRestartSec=2\nStandardOutput=append:{log_path}\nStandardError=append:{log_path}\n\n[Install]\nWantedBy=default.target\n",
        locality_fuse = systemd_quote(&locality_fuse.display().to_string()),
        state_root = systemd_quote(&state_root.display().to_string()),
        mountpoint = systemd_quote(&projection_root.display().to_string()),
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
        "ai.codeflash.locality.fuse.{}.service",
        sanitize_systemd_fragment(mount_id)
    )
}

#[cfg(target_os = "linux")]
pub fn linux_fuse_root_id(mount: &MountConfig) -> String {
    let root = localityd::virtual_fs::virtual_projection_root(mount);
    let root = root.display().to_string();
    let hint = bounded_systemd_hint(&root, LINUX_FUSE_ROOT_HINT_MAX_BYTES);
    format!("root-{hint}-{}", stable_hex_hash(&root))
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
fn bounded_systemd_hint(value: &str, max_bytes: usize) -> String {
    let sanitized = sanitize_systemd_fragment(value);
    let mut hint = String::with_capacity(max_bytes.min(sanitized.len()));
    for character in sanitized.chars() {
        if hint.len() + character.len_utf8() > max_bytes {
            break;
        }
        hint.push(character);
    }
    let hint = hint.trim_matches('_');
    if hint.is_empty() {
        "root".to_string()
    } else {
        hint.to_string()
    }
}

#[cfg(target_os = "linux")]
fn stable_hex_hash(value: &str) -> String {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

#[cfg(target_os = "linux")]
fn systemd_quote(value: &str) -> String {
    if value.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '/' | '.' | '-' | '_' | ':')
    }) {
        return value.to_string();
    }

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
pub fn locality_fuse_helper_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("LOCALITY_FUSE_BIN") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }

    let mut candidates = Vec::new();
    if let Ok(current_exe) = std::env::current_exe()
        && let Some(dir) = current_exe.parent()
    {
        candidates.push(dir.join("locality-fuse"));
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest_dir.join("../..");
    candidates.push(workspace.join("target/debug/locality-fuse"));
    candidates.push(workspace.join("target/release/locality-fuse"));

    if let Some(path) = candidates.into_iter().find(|path| path.exists()) {
        return Some(path);
    }
    find_on_path("locality-fuse")
}

#[cfg(not(target_os = "linux"))]
pub fn locality_fuse_helper_path() -> Option<PathBuf> {
    None
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
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
    std::env::var("LOCALITY_DAEMON_REQUEST_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_DAEMON_REQUEST_TIMEOUT)
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use locality_core::model::MountId;
    use locality_store::{MountConfig, ProjectionMode};

    #[test]
    fn linux_fuse_systemd_unit_uses_shared_root_helper_args() {
        let mount = MountConfig::new(
            MountId::new("notion/main"),
            "notion",
            "/home/example/loc notion",
        )
        .projection(ProjectionMode::LinuxFuse);
        let root_id = super::linux_fuse_root_id(&mount);
        let unit_name = super::linux_fuse_unit_name(&root_id);
        let log_path = format!("/home/example/.loc/logs/locality-fuse.{root_id}.log");
        let unit = super::linux_fuse_unit_contents(
            std::path::Path::new("/opt/agent fs/locality-fuse"),
            std::path::Path::new("/home/example/.loc"),
            &mount,
            std::path::Path::new(&log_path),
        );

        assert!(root_id.starts_with("root-home_example-"));
        assert!(root_id.len() <= 80);
        assert_eq!(
            unit_name,
            format!("ai.codeflash.locality.fuse.{root_id}.service")
        );
        assert!(unit.contains("ExecStart=\"/opt/agent fs/locality-fuse\""));
        assert!(!unit.contains("--mount-id"));
        assert!(unit.contains("--state-dir /home/example/.loc"));
        assert!(unit.contains("--mountpoint /home/example"));
        assert!(unit.contains("ExecStop=/usr/bin/fusermount3 -uz /home/example"));
        assert!(unit.contains("TimeoutStopSec=10"));
        assert!(unit.contains("LimitCORE=0"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains(&format!("StandardOutput=append:{log_path}")));
    }

    #[test]
    fn linux_fuse_root_id_distinguishes_sanitized_collisions() {
        let colon_root = MountConfig::new(MountId::new("colon"), "notion", "/tmp/a:b/notion")
            .projection(ProjectionMode::LinuxFuse);
        let question_root = MountConfig::new(MountId::new("question"), "notion", "/tmp/a?b/notion")
            .projection(ProjectionMode::LinuxFuse);

        let colon_id = super::linux_fuse_root_id(&colon_root);
        let question_id = super::linux_fuse_root_id(&question_root);

        assert_ne!(colon_id, question_id);
        assert_ne!(
            super::linux_fuse_unit_name(&colon_id),
            super::linux_fuse_unit_name(&question_id)
        );
    }

    #[test]
    fn linux_fuse_root_id_is_bounded_for_long_roots() {
        let long_component = "x".repeat(300);
        let root = format!("/tmp/{long_component}/notion");
        let mount = MountConfig::new(MountId::new("long"), "notion", root)
            .projection(ProjectionMode::LinuxFuse);

        let root_id = super::linux_fuse_root_id(&mount);

        assert!(
            root_id.len() <= 80,
            "root id should be bounded, got {} bytes: {root_id}",
            root_id.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use locality_core::model::MountId;
    use locality_store::{MountConfig, ProjectionMode};

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_fuse_unit_uses_shared_projection_root() {
        let mount = MountConfig::new(
            locality_core::model::MountId::new("notion-main"),
            "notion",
            "/tmp/Locality/notion-main",
        )
        .projection(ProjectionMode::LinuxFuse);
        let unit = super::linux_fuse_unit_contents(
            std::path::Path::new("/usr/bin/locality-fuse"),
            std::path::Path::new("/tmp/.loc"),
            &mount,
            std::path::Path::new("/tmp/locality-fuse.log"),
        );

        assert!(unit.contains("--mountpoint /tmp/Locality"));
        assert!(!unit.contains("--mount-id notion-main"));
    }

    #[test]
    fn macos_file_provider_display_name_strips_locality_cloudstorage_prefix() {
        assert_eq!(
            super::macos_file_provider_display_name(
                std::path::Path::new("/Users/example/Library/CloudStorage/Locality-Notion"),
                "fallback",
            ),
            "Notion"
        );
        assert_eq!(
            super::macos_file_provider_display_name(
                std::path::Path::new("/Users/example/Library/CloudStorage/Locality-Notion"),
                "fallback",
            ),
            "Notion"
        );
        assert_eq!(
            super::macos_file_provider_display_name(
                std::path::Path::new("/Users/example/Library/CloudStorage/Locality/notion-main"),
                "fallback",
            ),
            ""
        );
        assert_eq!(
            super::macos_file_provider_display_name(
                std::path::Path::new("/Users/example/Library/CloudStorage/Locality"),
                "fallback",
            ),
            ""
        );
        assert_eq!(
            super::macos_file_provider_display_name(
                std::path::Path::new("/Users/example/Documents/Locality/Notion"),
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
    fn macos_file_provider_unavailable_error_is_actionable() {
        let message = super::FileProviderHelperError::Failed(
            "The application cannot be used right now.".to_string(),
        )
        .message();

        assert!(message.contains("make install-macos-file-provider"));
        assert!(message.contains("enable the File Provider"));
        assert!(!message.contains("right now.."));
    }

    #[test]
    fn windows_cloud_files_register_args_are_stable_helper_contract() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            r"C:\Users\Ada\Locality\notion-main",
        )
        .projection(ProjectionMode::WindowsCloudFiles);

        assert_eq!(
            super::windows_cloud_files_register_args(
                std::path::Path::new(r"C:\Users\Ada\AppData\Local\Locality"),
                &mount,
                "Notion"
            ),
            vec![
                "--display-name",
                "Locality",
                "--sync-root",
                r"C:\Users\Ada\Locality",
                "--state-dir",
                r"C:\Users\Ada\AppData\Local\Locality",
            ]
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_cloud_files_helper_args_absolutize_relative_state_dir() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            r"C:\Users\Ada\Locality\notion-main",
        )
        .projection(ProjectionMode::WindowsCloudFiles);
        let current_dir = std::env::current_dir().expect("current dir");
        let expected_state_dir = current_dir.join(".loc").display().to_string();

        assert_eq!(
            super::windows_cloud_files_register_args(
                std::path::Path::new(".loc"),
                &mount,
                "Notion"
            ),
            vec![
                "--display-name",
                "Locality",
                "--sync-root",
                r"C:\Users\Ada\Locality",
                "--state-dir",
                &expected_state_dir,
            ]
        );
    }

    #[test]
    fn windows_cloud_files_open_and_unregister_args_are_stable_helper_contract() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            r"C:\Users\Ada\Locality\notion-main",
        )
        .projection(ProjectionMode::WindowsCloudFiles);

        assert_eq!(
            super::windows_cloud_files_open_args(&mount),
            vec!["--sync-root", r"C:\Users\Ada\Locality"]
        );
        assert_eq!(
            super::windows_cloud_files_run_args(
                std::path::Path::new(r"C:\Users\Ada\AppData\Local\Locality"),
                &mount,
            ),
            vec![
                "--sync-root",
                r"C:\Users\Ada\Locality",
                "--state-dir",
                r"C:\Users\Ada\AppData\Local\Locality",
            ]
        );
        assert_eq!(
            super::windows_cloud_files_run_command_args(
                std::path::Path::new(r"C:\Users\Ada\AppData\Local\Locality"),
                &mount,
            ),
            vec![
                "run",
                "--sync-root",
                r"C:\Users\Ada\Locality",
                "--state-dir",
                r"C:\Users\Ada\AppData\Local\Locality",
            ]
        );
        assert_eq!(
            super::windows_cloud_files_unregister_args(
                std::path::Path::new(r"C:\Users\Ada\AppData\Local\Locality"),
                "notion-main"
            ),
            vec![
                "--mount-id",
                "notion-main",
                "--state-dir",
                r"C:\Users\Ada\AppData\Local\Locality",
            ]
        );
    }

    #[test]
    fn windows_cloud_files_run_args_use_shared_projection_root() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            r"C:\Users\Ada\Locality\notion-main",
        )
        .projection(ProjectionMode::WindowsCloudFiles);

        let args = super::windows_cloud_files_run_command_args(
            std::path::Path::new(r"C:\Users\Ada\.loc"),
            &mount,
        );

        assert!(
            args.windows(2)
                .any(|pair| { pair[0] == "--sync-root" && pair[1] == r"C:\Users\Ada\Locality" })
        );
        assert!(!args.windows(2).any(|pair| pair[0] == "--mount-id"));
    }

    #[test]
    fn windows_cloud_files_registration_status_accepts_shared_root_id_field() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            r"C:\Users\Ada\Locality\notion-main",
        )
        .projection(ProjectionMode::WindowsCloudFiles);
        let roots = serde_json::json!([
            {
                "id": super::windows_cloud_files_shared_sync_root_id(&mount),
                "mount_id": null,
                "display_name": "Locality",
                "path": r"C:\Users\Ada\Locality"
            }
        ]);

        assert!(super::windows_cloud_files_roots_contain_registration(
            roots.as_array().expect("roots array"),
            &mount
        ));
    }

    #[test]
    fn windows_cloud_files_registration_status_rejects_other_shared_root_id() {
        let mount_a = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            r"C:\Users\Ada\Locality\notion-main",
        )
        .projection(ProjectionMode::WindowsCloudFiles);
        let mount_b = MountConfig::new(
            MountId::new("linear-main"),
            "linear",
            r"D:\Teams\Grace\Locality\linear-main",
        )
        .projection(ProjectionMode::WindowsCloudFiles);
        let roots = serde_json::json!([
            {
                "id": super::windows_cloud_files_shared_sync_root_id(&mount_b),
                "mount_id": null,
                "display_name": "Locality",
                "path": r"D:\Teams\Grace\Locality"
            }
        ]);
        let roots = roots.as_array().expect("roots array");

        assert!(!super::windows_cloud_files_roots_contain_registration(
            roots, &mount_a
        ));
        assert!(super::windows_cloud_files_roots_contain_registration(
            roots, &mount_b
        ));
        assert_ne!(
            super::windows_cloud_files_shared_sync_root_id(&mount_a),
            super::windows_cloud_files_shared_sync_root_id(&mount_b)
        );
    }

    #[test]
    fn windows_cloud_files_lifecycle_paths_are_mount_specific_and_stable() {
        let state_root = std::path::Path::new(r"C:\Users\Ada\AppData\Local\Locality");
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
                .join(format!("locality-cloud-files.{fragment}.out.log"))
        );
        assert_eq!(
            super::windows_cloud_files_stderr_log_path(state_root, mount_id),
            state_root
                .join("logs")
                .join(format!("locality-cloud-files.{fragment}.err.log"))
        );
    }

    #[test]
    fn windows_cloud_files_lifecycle_metadata_rejects_old_mount_point_sync_root() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            r"C:\Users\Ada\Locality\notion-main",
        )
        .projection(ProjectionMode::WindowsCloudFiles);

        assert!(
            !super::windows_cloud_files_lifecycle_metadata_matches_mount(
                std::path::Path::new(r"C:\Users\Ada\Locality\notion-main"),
                &mount
            )
        );
        assert!(super::windows_cloud_files_lifecycle_metadata_matches_mount(
            std::path::Path::new(r"C:\Users\Ada\Locality"),
            &mount
        ));
    }

    #[test]
    fn windows_cloud_files_registration_marker_paths_escape_mount_ids() {
        let state_root = std::path::Path::new(r"C:\Users\Ada\AppData\Local\Locality");
        let marker_path = locality_platform::windows_cloud_files_registration_marker_dir(
            state_root,
            "notion/main",
        )
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
                "\"locality-cloud-files.exe\",\"1234\",\"Console\",\"1\",\"10,000 K\""
            ),
            vec![
                "locality-cloud-files.exe",
                "1234",
                "Console",
                "1",
                "10,000 K"
            ]
        );
    }
}
