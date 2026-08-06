use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::process::{DefaultSessionProcessManager, SessionProcessManager};
use crate::user_home;

pub const MACOS_LAUNCHD_LABEL: &str = "ai.codeflash.locality.localityd";
pub const DAEMON_SOCKET_FILENAME: &str = "localityd.sock";
pub const DAEMON_PID_FILENAME: &str = "localityd.pid";
pub const DAEMON_METADATA_FILENAME: &str = "localityd.manager.json";
pub const DAEMON_STDOUT_LOG_FILENAME: &str = "localityd.out.log";
pub const DAEMON_STDERR_LOG_FILENAME: &str = "localityd.err.log";
pub const DAEMON_REMOUNT_FENCE_FILENAME: &str = "localityd.remount.fence";
pub const DAEMON_REMOUNT_LOCK_FILENAME: &str = "localityd.remount.lock";
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonManager {
    Launchd,
    Session,
    Unknown,
}

impl DaemonManager {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Launchd => "launchd",
            Self::Session => "session",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonStartMode {
    Auto,
    Launchd,
    Session,
}

impl DaemonStartMode {
    pub fn resolve_for_current_target(self) -> Option<DaemonManager> {
        self.resolve_for_target(std::env::consts::OS)
    }

    pub fn resolve_for_target(self, target_os: &str) -> Option<DaemonManager> {
        match self {
            Self::Session => Some(DaemonManager::Session),
            Self::Launchd if target_os == "macos" => Some(DaemonManager::Launchd),
            Self::Launchd => None,
            Self::Auto if target_os == "macos" => Some(DaemonManager::Launchd),
            Self::Auto => Some(DaemonManager::Session),
        }
    }

    pub fn should_use_launchd_for_current_target(self) -> bool {
        self.should_use_launchd_for_target(std::env::consts::OS)
    }

    pub fn should_use_launchd_for_target(self, target_os: &str) -> bool {
        matches!(self, Self::Auto | Self::Launchd) && target_os == "macos"
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonProcessPaths {
    pub state_root: PathBuf,
    pub socket: PathBuf,
    pub pid_file: PathBuf,
    pub metadata_file: PathBuf,
    pub stdout_log: PathBuf,
    pub stderr_log: PathBuf,
    pub launch_agent: Option<PathBuf>,
}

impl DaemonProcessPaths {
    pub fn new(state_root: PathBuf) -> Self {
        Self::for_target(state_root, std::env::consts::OS, user_home())
    }

    pub fn for_target(state_root: PathBuf, target_os: &str, home: Option<PathBuf>) -> Self {
        let logs_dir = state_root.join("logs");
        let launch_agent = (target_os == "macos")
            .then(|| {
                home.map(|home| {
                    home.join("Library")
                        .join("LaunchAgents")
                        .join(format!("{MACOS_LAUNCHD_LABEL}.plist"))
                })
            })
            .flatten();

        Self {
            socket: daemon_socket_path(&state_root),
            pid_file: state_root.join(DAEMON_PID_FILENAME),
            metadata_file: state_root.join(DAEMON_METADATA_FILENAME),
            stdout_log: logs_dir.join(DAEMON_STDOUT_LOG_FILENAME),
            stderr_log: logs_dir.join(DAEMON_STDERR_LOG_FILENAME),
            state_root,
            launch_agent,
        }
    }

    pub fn detected_manager(&self) -> DaemonManager {
        if self.pid_file.exists() {
            return DaemonManager::Session;
        }
        if self.launch_agent.as_ref().is_some_and(|path| path.exists()) {
            return DaemonManager::Launchd;
        }
        DaemonManager::Unknown
    }
}

pub fn daemon_socket_path(state_root: &Path) -> PathBuf {
    state_root.join(DAEMON_SOCKET_FILENAME)
}

pub fn daemon_remount_fence_path(state_root: &Path) -> PathBuf {
    state_root.join(DAEMON_REMOUNT_FENCE_FILENAME)
}

pub fn daemon_remount_lock_path(state_root: &Path) -> PathBuf {
    state_root.join(DAEMON_REMOUNT_LOCK_FILENAME)
}

fn daemon_remount_recovery_gate_path(state_root: &Path) -> PathBuf {
    let parent = state_root.parent().unwrap_or_else(|| Path::new("."));
    let state_name = state_root
        .file_name()
        .unwrap_or_else(|| OsStr::new("locality-state"))
        .to_string_lossy();
    parent.join(format!(
        ".{state_name}.{DAEMON_REMOUNT_LOCK_FILENAME}.recovery"
    ))
}

fn daemon_remount_start_handoff_path(state_root: &Path) -> PathBuf {
    let parent = state_root.parent().unwrap_or_else(|| Path::new("."));
    let state_name = state_root
        .file_name()
        .unwrap_or_else(|| OsStr::new("locality-state"))
        .to_string_lossy();
    parent.join(format!(
        ".{state_name}.{DAEMON_REMOUNT_LOCK_FILENAME}.startup"
    ))
}

/// Resets local state while retaining exclusive ownership of the remount lock
/// inode. This prevents an unlink/recreate race with daemon startup or remount
/// recovery during the destructive state-root sweep.
pub fn reset_locality_state_storage_coordinated(
    state_root: &Path,
) -> Result<locality_store::LocalStateResetStorageReport, DaemonProcessError> {
    let ownership = DaemonRemountCoordinatorLock::try_acquire(state_root).map_err(|error| {
        DaemonProcessError::new(
            if error.kind() == io::ErrorKind::WouldBlock {
                "remount_in_progress"
            } else {
                "io_error"
            },
            error.to_string(),
        )
    })?;
    ownership.reset_locality_state_storage()
}

/// Process-scoped exclusive ownership of remount coordination and recovery.
///
/// The lock lives beside (not inside) the removable durable fence so an owner
/// can verify and delete its exact fence generation without releasing the OS
/// exclusion boundary. Both CLI and Desktop use this same primitive.
pub struct DaemonRemountCoordinatorLock {
    state_root: PathBuf,
    _recovery_gate: fs::File,
    startup_handoff: fs::File,
    file: fs::File,
    startup_handoff_begun: AtomicBool,
}

/// Process-scoped liveness lease for one durable hosted-workspace transition.
///
/// The attaching process retains this lock while it downloads and stages the
/// immutable export. Recovery uses the same non-blocking lock before deciding
/// that an unpublished pending transition was abandoned, so it cannot cancel
/// work that is still active in another process.
pub struct HostedWorkspaceTransitionLock {
    _file: fs::File,
}

impl HostedWorkspaceTransitionLock {
    pub fn try_acquire(state_root: &Path, transition_id: &str) -> io::Result<Self> {
        if transition_id.is_empty()
            || transition_id.len() > 200
            || transition_id.bytes().any(|byte| {
                !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "hosted workspace transition ID is not safe for coordination",
            ));
        }
        let directory = state_root.join("hosted-workspace-transition-locks");
        fs::create_dir_all(&directory)?;
        let path = directory.join(format!("{transition_id}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        prevent_coordination_file_inheritance(&file)?;
        try_lock_coordination_file(&file, CoordinationLockMode::Exclusive).map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!(
                        "hosted workspace transition `{transition_id}` is active in another process"
                    ),
                )
            } else {
                error
            }
        })?;
        Ok(Self { _file: file })
    }
}

impl DaemonRemountCoordinatorLock {
    pub fn try_acquire(state_root: &Path) -> io::Result<Self> {
        fs::create_dir_all(state_root)?;
        let path = daemon_remount_lock_path(state_root);
        let recovery_gate_path = daemon_remount_recovery_gate_path(state_root);
        let recovery_gate = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&recovery_gate_path)?;
        prevent_coordination_file_inheritance(&recovery_gate)?;
        try_lock_coordination_file(&recovery_gate, CoordinationLockMode::Exclusive)
            .map_err(|error| remount_coordination_lock_error(error, &path))?;
        let startup_handoff = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(daemon_remount_start_handoff_path(state_root))?;
        prevent_coordination_file_inheritance(&startup_handoff)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        prevent_coordination_file_inheritance(&file)?;
        try_lock_coordination_file(&file, CoordinationLockMode::Exclusive)
            .map_err(|error| remount_coordination_lock_error(error, &path))?;
        Ok(Self {
            state_root: state_root.to_path_buf(),
            _recovery_gate: recovery_gate,
            startup_handoff,
            file,
            startup_handoff_begun: AtomicBool::new(false),
        })
    }

    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    /// Deletes resettable state while retaining this exact lock handle and its
    /// inode for the complete storage sweep.
    pub fn reset_locality_state_storage(
        &self,
    ) -> Result<locality_store::LocalStateResetStorageReport, DaemonProcessError> {
        if self.startup_handoff_begun.load(Ordering::Acquire) {
            return Err(DaemonProcessError::new(
                "remount_handoff_started",
                "state storage cannot be reset after daemon startup handoff begins",
            ));
        }
        locality_store::reset_locality_state_storage_preserving(
            &self.state_root,
            &[OsStr::new(DAEMON_REMOUNT_LOCK_FILENAME)],
        )
        .map_err(|error| DaemonProcessError::new("state_reset_failed", error.to_string()))
    }

    /// Retains the non-shareable recovery gate while allowing the replacement
    /// daemon to acquire ordinary shared startup ownership. A second recovery
    /// remains excluded throughout the manager launch and readiness wait.
    fn begin_daemon_start_handoff(&self) -> Result<(), DaemonProcessError> {
        if self
            .startup_handoff_begun
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(DaemonProcessError::new(
                "remount_handoff_started",
                "daemon startup handoff has already begun for this recovery owner",
            ));
        }
        if let Err(error) =
            try_lock_coordination_file(&self.startup_handoff, CoordinationLockMode::Exclusive)
        {
            self.startup_handoff_begun.store(false, Ordering::Release);
            return Err(DaemonProcessError::new("io_error", error.to_string()));
        }
        if let Err(error) = transition_coordination_file_to_shared(&self.file) {
            self.startup_handoff_begun.store(false, Ordering::Release);
            return Err(DaemonProcessError::new("io_error", error.to_string()));
        }
        Ok(())
    }
}

fn remount_coordination_lock_error(error: io::Error, path: &Path) -> io::Error {
    if matches!(error.kind(), io::ErrorKind::WouldBlock) {
        io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "another Locality coordinator owns remount recovery through `{}`",
                path.display()
            ),
        )
    } else {
        error
    }
}

/// Shared startup ownership that excludes remount coordination until the
/// daemon's control endpoint is ready.
///
/// A process manager and the daemon it launches may hold this lock at the same
/// time. A remount requires the exclusive counterpart, so it cannot pass the
/// fence check between process launch and daemon readiness.
#[must_use = "dropping startup ownership permits remount coordination"]
pub struct DaemonStartupCoordinatorLock {
    _file: fs::File,
}

impl DaemonStartupCoordinatorLock {
    pub fn try_acquire(paths: &DaemonProcessPaths) -> Result<Self, DaemonProcessError> {
        fs::create_dir_all(&paths.state_root)
            .map_err(|error| DaemonProcessError::new("io_error", error.to_string()))?;
        let path = daemon_remount_lock_path(&paths.state_root);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| DaemonProcessError::new("io_error", error.to_string()))?;
        prevent_coordination_file_inheritance(&file)
            .map_err(|error| DaemonProcessError::new("io_error", error.to_string()))?;
        try_lock_coordination_file(&file, CoordinationLockMode::Shared).map_err(|error| {
            if matches!(error.kind(), io::ErrorKind::WouldBlock) {
                DaemonProcessError::new(
                    "remount_in_progress",
                    format!(
                        "daemon start is fenced while remount coordination owns `{}`",
                        path.display()
                    ),
                )
            } else {
                DaemonProcessError::new("io_error", error.to_string())
            }
        })?;
        if let Err(error) = ensure_daemon_start_allowed(paths) {
            let live_handoff = error.code() == "remount_in_progress"
                && recovery_start_handoff_is_live(paths)
                    .map_err(|error| DaemonProcessError::new("io_error", error.to_string()))?;
            if !live_handoff {
                return Err(error);
            }
        }
        Ok(Self { _file: file })
    }
}

#[derive(Clone, Copy)]
enum CoordinationLockMode {
    Shared,
    Exclusive,
}

#[cfg(unix)]
fn prevent_coordination_file_inheritance(file: &fs::File) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn prevent_coordination_file_inheritance(file: &fs::File) -> io::Result<()> {
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};

    if unsafe { SetHandleInformation(file.as_raw_handle() as _, HANDLE_FLAG_INHERIT, 0) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn prevent_coordination_file_inheritance(_file: &fs::File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "cross-process remount ownership is unavailable on this platform",
    ))
}

#[cfg(unix)]
fn transition_coordination_file_to_shared(file: &fs::File) -> io::Result<()> {
    try_lock_coordination_file(file, CoordinationLockMode::Shared)
}

#[cfg(unix)]
fn try_lock_coordination_file(file: &fs::File, mode: CoordinationLockMode) -> io::Result<()> {
    let operation = match mode {
        CoordinationLockMode::Shared => libc::LOCK_SH,
        CoordinationLockMode::Exclusive => libc::LOCK_EX,
    };
    let result = unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) };
    if result == 0 {
        Ok(())
    } else {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        } else {
            Err(error)
        }
    }
}

#[cfg(windows)]
fn try_lock_coordination_file(file: &fs::File, mode: CoordinationLockMode) -> io::Result<()> {
    use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, ERROR_LOCK_VIOLATION};
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let mut overlapped = OVERLAPPED::default();
    let ok = unsafe {
        LockFileEx(
            file.as_raw_handle() as _,
            if matches!(mode, CoordinationLockMode::Exclusive) {
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY
            } else {
                LOCKFILE_FAIL_IMMEDIATELY
            },
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if ok != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if matches!(
        error.raw_os_error().map(|code| code as u32),
        Some(ERROR_LOCK_VIOLATION) | Some(ERROR_IO_PENDING)
    ) {
        Err(io::Error::from(io::ErrorKind::WouldBlock))
    } else {
        Err(error)
    }
}

#[cfg(windows)]
fn transition_coordination_file_to_shared(file: &fs::File) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let mut overlapped = OVERLAPPED::default();
    if unsafe {
        UnlockFileEx(
            file.as_raw_handle() as _,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    try_lock_coordination_file(file, CoordinationLockMode::Shared)
}

#[cfg(not(any(unix, windows)))]
fn try_lock_coordination_file(_file: &fs::File, _mode: CoordinationLockMode) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "cross-process remount ownership is unavailable on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
fn transition_coordination_file_to_shared(_file: &fs::File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "cross-process remount ownership is unavailable on this platform",
    ))
}

fn recovery_start_handoff_is_live(paths: &DaemonProcessPaths) -> io::Result<bool> {
    Ok(
        coordination_gate_is_live(&daemon_remount_recovery_gate_path(&paths.state_root))?
            && coordination_gate_is_live(&daemon_remount_start_handoff_path(&paths.state_root))?,
    )
}

fn coordination_gate_is_live(gate_path: &Path) -> io::Result<bool> {
    let gate = match OpenOptions::new().read(true).write(true).open(gate_path) {
        Ok(gate) => gate,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    prevent_coordination_file_inheritance(&gate)?;
    match try_lock_coordination_file(&gate, CoordinationLockMode::Shared) {
        Ok(()) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
        Err(error) => Err(error),
    }
}

pub fn ensure_daemon_start_allowed(paths: &DaemonProcessPaths) -> Result<(), DaemonProcessError> {
    let fence = daemon_remount_fence_path(&paths.state_root);
    match fs::symlink_metadata(&fence) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(DaemonProcessError::new("io_error", error.to_string())),
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            Err(DaemonProcessError::new(
                "remount_in_progress",
                format!(
                    "daemon start is fenced while interrupted remount recovery is pending at `{}`",
                    fence.display()
                ),
            ))
        }
        Ok(_) => Err(DaemonProcessError::new(
            "remount_in_progress",
            format!(
                "unsafe daemon remount fence exists at `{}`",
                fence.display()
            ),
        )),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonProcessError {
    code: &'static str,
    message: String,
}

impl DaemonProcessError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn code(&self) -> &'static str {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Clone, Debug)]
pub struct DaemonProcessStartConfig<'a> {
    pub mode: DaemonStartMode,
    pub paths: &'a DaemonProcessPaths,
    pub localityd_bin: &'a Path,
    pub tcp_addr: Option<&'a str>,
    pub environment: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonProcessStartReport {
    pub manager: DaemonManager,
    pub localityd_bin: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonProcessStopReport {
    pub stopped_managed_process: bool,
}

/// Keeps a managed daemon from being relaunched while a coordinator-owned
/// operation waits for the current process to exit.
pub struct DaemonManagerRestartFence {
    #[cfg(target_os = "macos")]
    launchd_policy: Option<(String, bool)>,
}

impl DaemonManagerRestartFence {
    pub fn suspend(
        paths: &DaemonProcessPaths,
        supervision_was_enabled: Option<bool>,
    ) -> Result<Self, DaemonProcessError> {
        #[cfg(target_os = "macos")]
        {
            let launchd_policy = if paths
                .launch_agent
                .as_ref()
                .is_some_and(|path| path.exists())
            {
                let target = launchd_service_target(&launchd_domain()?);
                if supervision_was_enabled == Some(true) {
                    set_launchd_service_enabled(&target, false)?;
                }
                supervision_was_enabled.map(|enabled| (target, enabled))
            } else {
                None
            };
            Ok(Self { launchd_policy })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (paths, supervision_was_enabled);
            Ok(Self {})
        }
    }

    pub fn restore(&mut self) -> Result<(), DaemonProcessError> {
        #[cfg(target_os = "macos")]
        if let Some((target, enabled)) = self.launchd_policy.take() {
            if let Err(error) = set_launchd_service_enabled(&target, enabled) {
                self.launchd_policy = Some((target, enabled));
                return Err(error);
            }
        }
        Ok(())
    }

    /// Transfers responsibility for restoring supervision to persisted-fence
    /// startup reconciliation.
    pub fn remain_suspended(&mut self) {
        #[cfg(target_os = "macos")]
        {
            self.launchd_policy = None;
        }
    }
}

pub fn restore_daemon_manager_supervision(
    paths: &DaemonProcessPaths,
    supervision_was_enabled: Option<bool>,
) -> Result<(), DaemonProcessError> {
    #[cfg(target_os = "macos")]
    if paths
        .launch_agent
        .as_ref()
        .is_some_and(|path| path.exists())
        && let Some(enabled) = supervision_was_enabled
    {
        let target = launchd_service_target(&launchd_domain()?);
        set_launchd_service_enabled(&target, enabled)?;
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (paths, supervision_was_enabled);
    Ok(())
}

/// Captures whether the platform process manager should be restored after a
/// coordinated daemon suspension. `None` means no managed service exists.
pub fn daemon_manager_supervision_enabled(
    paths: &DaemonProcessPaths,
) -> Result<Option<bool>, DaemonProcessError> {
    #[cfg(target_os = "macos")]
    {
        if !paths
            .launch_agent
            .as_ref()
            .is_some_and(|path| path.exists())
        {
            return Ok(None);
        }
        let domain = launchd_domain()?;
        return launchd_service_enabled(&domain).map(Some);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = paths;
        Ok(None)
    }
}

impl Drop for DaemonManagerRestartFence {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some((target, enabled)) = self.launchd_policy.take() {
            let _ = set_launchd_service_enabled(&target, enabled);
        }
    }
}

pub trait DaemonProcessManager {
    fn resolve_start_manager(
        &self,
        mode: DaemonStartMode,
    ) -> Result<DaemonManager, DaemonProcessError>;

    fn start(
        &self,
        config: &DaemonProcessStartConfig<'_>,
    ) -> Result<DaemonProcessStartReport, DaemonProcessError>;

    fn stop(
        &self,
        mode: DaemonStartMode,
        paths: &DaemonProcessPaths,
    ) -> Result<DaemonProcessStopReport, DaemonProcessError>;

    fn detected_manager(&self, paths: &DaemonProcessPaths) -> DaemonManager;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultDaemonProcessManager;

impl DaemonProcessManager for DefaultDaemonProcessManager {
    fn resolve_start_manager(
        &self,
        mode: DaemonStartMode,
    ) -> Result<DaemonManager, DaemonProcessError> {
        mode.resolve_for_current_target().ok_or_else(|| {
            DaemonProcessError::new("unsupported", "--launchd is only supported on macOS")
        })
    }

    fn start(
        &self,
        config: &DaemonProcessStartConfig<'_>,
    ) -> Result<DaemonProcessStartReport, DaemonProcessError> {
        ensure_daemon_start_allowed(config.paths)?;
        match self.resolve_start_manager(config.mode)? {
            DaemonManager::Launchd => start_launchd(config),
            DaemonManager::Session => start_session(config),
            DaemonManager::Unknown => Err(DaemonProcessError::new(
                "unsupported",
                "daemon start manager could not be resolved",
            )),
        }
    }

    fn stop(
        &self,
        mode: DaemonStartMode,
        paths: &DaemonProcessPaths,
    ) -> Result<DaemonProcessStopReport, DaemonProcessError> {
        let mut stopped_managed_process = false;

        if mode.should_use_launchd_for_current_target() && paths.launch_agent.is_some() {
            stopped_managed_process = stop_launchd(paths)?;
        }

        if paths.pid_file.exists() {
            stop_session(paths)?;
            stopped_managed_process = true;
        }

        Ok(DaemonProcessStopReport {
            stopped_managed_process,
        })
    }

    fn detected_manager(&self, paths: &DaemonProcessPaths) -> DaemonManager {
        paths.detected_manager()
    }
}

impl DefaultDaemonProcessManager {
    /// Starts a daemon on behalf of an owner that still holds the exclusive
    /// remount lock and durable fence.
    pub fn start_during_remount(
        &self,
        config: &DaemonProcessStartConfig<'_>,
        ownership: &DaemonRemountCoordinatorLock,
    ) -> Result<DaemonProcessStartReport, DaemonProcessError> {
        if ownership.state_root() != config.paths.state_root {
            return Err(DaemonProcessError::new(
                "remount_owner_mismatch",
                "daemon start does not match the remount owner's state root",
            ));
        }
        ownership.begin_daemon_start_handoff()?;
        match self.resolve_start_manager(config.mode)? {
            DaemonManager::Launchd => start_launchd(config),
            DaemonManager::Session => start_session(config),
            DaemonManager::Unknown => Err(DaemonProcessError::new(
                "unsupported",
                "daemon start manager could not be resolved",
            )),
        }
    }
}

fn start_session(
    config: &DaemonProcessStartConfig<'_>,
) -> Result<DaemonProcessStartReport, DaemonProcessError> {
    let paths = config.paths;
    fs::create_dir_all(paths.stdout_log.parent().unwrap_or(&paths.state_root))
        .map_err(|error| DaemonProcessError::new("io_error", error.to_string()))?;
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.stdout_log)
        .map_err(|error| DaemonProcessError::new("io_error", error.to_string()))?;
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.stderr_log)
        .map_err(|error| DaemonProcessError::new("io_error", error.to_string()))?;

    let mut command = Command::new(config.localityd_bin);
    command
        .env("LOCALITY_STATE_DIR", &paths.state_root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    if let Some(tcp_addr) = config.tcp_addr {
        command.env("LOCALITY_DAEMON_TCP_ADDR", tcp_addr);
    }
    for (key, value) in &config.environment {
        command.env(key, value);
    }

    let child = DefaultSessionProcessManager
        .spawn_detached(&mut command)
        .map_err(|error| DaemonProcessError::new("start_failed", error.to_string()))?;
    fs::write(&paths.pid_file, child.id().to_string())
        .map_err(|error| DaemonProcessError::new("io_error", error.to_string()))?;

    Ok(DaemonProcessStartReport {
        manager: DaemonManager::Session,
        localityd_bin: config.localityd_bin.to_path_buf(),
    })
}

fn stop_session(paths: &DaemonProcessPaths) -> Result<(), DaemonProcessError> {
    let pid = fs::read_to_string(&paths.pid_file)
        .map_err(|error| DaemonProcessError::new("io_error", error.to_string()))?
        .trim()
        .to_string();
    if !pid.is_empty() {
        let stop_command = DefaultSessionProcessManager.stop_command(&pid);
        let mut command = Command::new(stop_command.program());
        configure_hidden_windows_command(&mut command);
        let _ = command.args(stop_command.args()).output();
    }
    let _ = fs::remove_file(&paths.pid_file);
    Ok(())
}

#[cfg(windows)]
fn configure_hidden_windows_command(command: &mut Command) {
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn configure_hidden_windows_command(_command: &mut Command) {}

#[cfg(target_os = "macos")]
fn start_launchd(
    config: &DaemonProcessStartConfig<'_>,
) -> Result<DaemonProcessStartReport, DaemonProcessError> {
    let paths = config.paths;
    let Some(launch_agent) = &paths.launch_agent else {
        return Err(DaemonProcessError::new(
            "unsupported",
            "launchd requires a user LaunchAgents directory",
        ));
    };
    fs::create_dir_all(launch_agent.parent().unwrap_or(Path::new(".")))
        .map_err(|error| DaemonProcessError::new("io_error", error.to_string()))?;
    fs::create_dir_all(paths.stdout_log.parent().unwrap_or(&paths.state_root))
        .map_err(|error| DaemonProcessError::new("io_error", error.to_string()))?;

    let plist = launch_agent_plist(config)?;
    fs::write(launch_agent, plist)
        .map_err(|error| DaemonProcessError::new("io_error", error.to_string()))?;

    let domain = launchd_domain()?;
    let _ = fs::remove_file(&paths.pid_file);
    launchctl_bootout_service(&domain);
    launchctl_bootout_plist(&domain, launch_agent);
    run_launchctl(
        Command::new("launchctl")
            .arg("bootstrap")
            .arg(&domain)
            .arg(launch_agent),
    )?;
    run_launchctl(
        Command::new("launchctl")
            .arg("enable")
            .arg(format!("{domain}/{MACOS_LAUNCHD_LABEL}")),
    )?;
    run_launchctl(
        Command::new("launchctl")
            .arg("kickstart")
            .arg("-k")
            .arg(format!("{domain}/{MACOS_LAUNCHD_LABEL}")),
    )?;

    Ok(DaemonProcessStartReport {
        manager: DaemonManager::Launchd,
        localityd_bin: config.localityd_bin.to_path_buf(),
    })
}

#[cfg(not(target_os = "macos"))]
fn start_launchd(
    _config: &DaemonProcessStartConfig<'_>,
) -> Result<DaemonProcessStartReport, DaemonProcessError> {
    Err(DaemonProcessError::new(
        "unsupported",
        "launchd is only supported on macOS",
    ))
}

#[cfg(target_os = "macos")]
fn stop_launchd(paths: &DaemonProcessPaths) -> Result<bool, DaemonProcessError> {
    let Some(launch_agent) = &paths.launch_agent else {
        return Ok(false);
    };
    let domain = launchd_domain()?;
    let had_launch_agent = launch_agent.exists();
    let unloaded_service = launchctl_bootout_service(&domain);
    let unloaded_plist = launchctl_bootout_plist(&domain, launch_agent);
    if launch_agent.exists() {
        fs::remove_file(launch_agent)
            .map_err(|error| DaemonProcessError::new("io_error", error.to_string()))?;
    }
    Ok(had_launch_agent || unloaded_service || unloaded_plist)
}

#[cfg(not(target_os = "macos"))]
fn stop_launchd(_paths: &DaemonProcessPaths) -> Result<bool, DaemonProcessError> {
    Ok(false)
}

#[cfg(target_os = "macos")]
fn launchd_domain() -> Result<String, DaemonProcessError> {
    let output = Command::new("id")
        .arg("-u")
        .output()
        .map_err(|error| DaemonProcessError::new("launchctl_failed", error.to_string()))?;
    if !output.status.success() {
        return Err(DaemonProcessError::new(
            "launchctl_failed",
            "could not determine current user id",
        ));
    }
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(format!("gui/{uid}"))
}

#[cfg(target_os = "macos")]
fn run_launchctl(command: &mut Command) -> Result<(), DaemonProcessError> {
    let output = command
        .output()
        .map_err(|error| DaemonProcessError::new("launchctl_failed", error.to_string()))?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let message = if !stderr.is_empty() { stderr } else { stdout };
    Err(DaemonProcessError::new(
        "launchctl_failed",
        if message.is_empty() {
            format!("launchctl exited with {}", output.status)
        } else {
            message
        },
    ))
}

#[cfg(target_os = "macos")]
fn set_launchd_service_enabled(target: &str, enabled: bool) -> Result<(), DaemonProcessError> {
    let (verb, target) = launchd_restart_fence_action(target, enabled);
    run_launchctl(Command::new("launchctl").arg(verb).arg(target))
}

#[cfg(target_os = "macos")]
fn launchd_service_enabled(domain: &str) -> Result<bool, DaemonProcessError> {
    let output = Command::new("launchctl")
        .arg("print-disabled")
        .arg(domain)
        .output()
        .map_err(|error| DaemonProcessError::new("launchctl_failed", error.to_string()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(DaemonProcessError::new(
            "launchctl_failed",
            if stderr.is_empty() {
                format!("launchctl print-disabled exited with {}", output.status)
            } else {
                stderr
            },
        ));
    }
    parse_launchd_service_enabled(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(any(target_os = "macos", test))]
fn parse_launchd_service_enabled(output: &str) -> Result<bool, DaemonProcessError> {
    let quoted_label = format!("\"{MACOS_LAUNCHD_LABEL}\"");
    let Some(line) = output.lines().find(|line| line.contains(&quoted_label)) else {
        // launchd omits services that retain their default enabled state.
        return Ok(true);
    };
    let Some((_, value)) = line.split_once("=>") else {
        return Err(DaemonProcessError::new(
            "launchctl_failed",
            "launchctl returned an invalid disabled-services entry",
        ));
    };
    match value.trim().trim_end_matches(',').trim() {
        "true" => Ok(false),
        "false" => Ok(true),
        _ => Err(DaemonProcessError::new(
            "launchctl_failed",
            "launchctl returned an invalid disabled-services value",
        )),
    }
}

#[cfg(any(target_os = "macos", test))]
fn launchd_restart_fence_action(target: &str, enabled: bool) -> (&'static str, &str) {
    (if enabled { "enable" } else { "disable" }, target)
}

#[cfg(target_os = "macos")]
fn launchctl_bootout_service(domain: &str) -> bool {
    Command::new("launchctl")
        .arg("bootout")
        .arg(launchd_service_target(domain))
        .output()
        .is_ok_and(|output| output.status.success())
}

#[cfg(target_os = "macos")]
fn launchctl_bootout_plist(domain: &str, launch_agent: &Path) -> bool {
    Command::new("launchctl")
        .arg("bootout")
        .arg(domain)
        .arg(launch_agent)
        .output()
        .is_ok_and(|output| output.status.success())
}

#[cfg(any(target_os = "macos", test))]
fn launchd_service_target(domain: &str) -> String {
    format!("{domain}/{MACOS_LAUNCHD_LABEL}")
}

#[cfg(any(target_os = "macos", test))]
fn launch_agent_plist(config: &DaemonProcessStartConfig<'_>) -> Result<String, DaemonProcessError> {
    let paths = config.paths;
    let mut env_vars = vec![
        (
            "HOME".to_string(),
            user_home()
                .ok_or_else(|| DaemonProcessError::new("env_missing", "home directory is not set"))?
                .display()
                .to_string(),
        ),
        (
            "LOCALITY_STATE_DIR".to_string(),
            paths.state_root.display().to_string(),
        ),
    ];
    if let Some(tcp_addr) = config.tcp_addr {
        env_vars.push(("LOCALITY_DAEMON_TCP_ADDR".to_string(), tcp_addr.to_string()));
    }
    env_vars.extend(config.environment.iter().cloned());
    env_vars.sort_by(|a, b| a.0.cmp(&b.0));
    env_vars.dedup_by(|a, b| a.0 == b.0);

    let env_xml = env_vars
        .iter()
        .map(|(key, value)| {
            format!(
                "    <key>{}</key>\n    <string>{}</string>\n",
                xml_escape(key),
                xml_escape(value)
            )
        })
        .collect::<String>();

    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{localityd_bin}</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
{env_xml}  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{stdout}</string>
  <key>StandardErrorPath</key>
  <string>{stderr}</string>
</dict>
</plist>
"#,
        label = MACOS_LAUNCHD_LABEL,
        localityd_bin = xml_escape(&config.localityd_bin.display().to_string()),
        env_xml = env_xml,
        stdout = xml_escape(&paths.stdout_log.display().to_string()),
        stderr = xml_escape(&paths.stderr_log.display().to_string()),
    ))
}

#[cfg(any(target_os = "macos", test))]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::{
        DaemonManager, DaemonProcessPaths, DaemonProcessStartConfig, DaemonRemountCoordinatorLock,
        DaemonStartMode, DaemonStartupCoordinatorLock, daemon_remount_fence_path,
        daemon_remount_recovery_gate_path, daemon_remount_start_handoff_path, daemon_socket_path,
        ensure_daemon_start_allowed, launch_agent_plist, launchd_restart_fence_action,
        launchd_service_target, parse_launchd_service_enabled, xml_escape,
    };
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard};

    static PROCESS_LOCK_TEST_MUTEX: Mutex<()> = Mutex::new(());

    fn process_lock_test_guard() -> MutexGuard<'static, ()> {
        PROCESS_LOCK_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn remove_process_lock_test_root(root: &std::path::Path) {
        let gate = daemon_remount_recovery_gate_path(root);
        let handoff = daemon_remount_start_handoff_path(root);
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_file(gate);
        let _ = std::fs::remove_file(handoff);
    }

    #[test]
    fn process_paths_use_stable_daemon_filenames() {
        let root = PathBuf::from("/tmp/loc-state");
        let paths = DaemonProcessPaths::for_target(root.clone(), "linux", None);

        assert_eq!(paths.state_root, root);
        assert_eq!(paths.socket, PathBuf::from("/tmp/loc-state/localityd.sock"));
        assert_eq!(
            paths.pid_file,
            PathBuf::from("/tmp/loc-state/localityd.pid")
        );
        assert_eq!(
            paths.metadata_file,
            PathBuf::from("/tmp/loc-state/localityd.manager.json")
        );
        assert_eq!(
            paths.stdout_log,
            PathBuf::from("/tmp/loc-state/logs/localityd.out.log")
        );
        assert_eq!(
            paths.stderr_log,
            PathBuf::from("/tmp/loc-state/logs/localityd.err.log")
        );
        assert!(paths.launch_agent.is_none());
    }

    #[test]
    fn macos_process_paths_include_launch_agent() {
        let paths = DaemonProcessPaths::for_target(
            PathBuf::from("/tmp/loc-state"),
            "macos",
            Some(PathBuf::from("/Users/ada")),
        );

        assert_eq!(
            paths.launch_agent,
            Some(PathBuf::from(
                "/Users/ada/Library/LaunchAgents/ai.codeflash.locality.localityd.plist"
            ))
        );
    }

    #[test]
    fn daemon_socket_path_uses_state_root() {
        assert_eq!(
            daemon_socket_path(&PathBuf::from("/tmp/loc-state")),
            PathBuf::from("/tmp/loc-state/localityd.sock")
        );
    }

    #[test]
    fn persisted_remount_fence_blocks_all_manager_start_modes() {
        let root = std::env::temp_dir().join(format!(
            "locality-daemon-fence-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&root).expect("create state root");
        std::fs::write(daemon_remount_fence_path(&root), "version=1\n").expect("write fence");
        for target in ["linux", "windows", "macos"] {
            let paths = DaemonProcessPaths::for_target(root.clone(), target, None);
            let error = ensure_daemon_start_allowed(&paths).expect_err("fence blocks start");
            assert_eq!(error.code(), "remount_in_progress");
        }
        std::fs::remove_dir_all(root).expect("remove state root");
    }

    #[test]
    fn startup_shared_locks_handoff_to_exclusive_remount_ownership() {
        let _guard = process_lock_test_guard();
        let root = std::env::temp_dir().join(format!(
            "locality-daemon-startup-handoff-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let paths = DaemonProcessPaths::for_target(root.clone(), "linux", None);

        let controller =
            DaemonStartupCoordinatorLock::try_acquire(&paths).expect("controller startup lock");
        let daemon =
            DaemonStartupCoordinatorLock::try_acquire(&paths).expect("daemon startup lock");
        assert!(
            DaemonRemountCoordinatorLock::try_acquire(&root).is_err(),
            "remount must not pass the controller/daemon startup handoff"
        );

        drop(controller);
        assert!(
            DaemonRemountCoordinatorLock::try_acquire(&root).is_err(),
            "daemon retains startup exclusion until its endpoint is ready"
        );

        drop(daemon);
        let remount =
            DaemonRemountCoordinatorLock::try_acquire(&root).expect("exclusive remount ownership");
        let error = DaemonStartupCoordinatorLock::try_acquire(&paths)
            .err()
            .expect("remount excludes daemon startup");
        assert_eq!(error.code(), "remount_in_progress");

        drop(remount);
        let _startup = DaemonStartupCoordinatorLock::try_acquire(&paths)
            .expect("startup resumes after remount");
        drop(_startup);
        remove_process_lock_test_root(&root);
    }

    #[test]
    fn copied_fence_and_environment_cannot_replay_remount_start_permission() {
        let _guard = process_lock_test_guard();
        let root = std::env::temp_dir().join(format!(
            "locality-daemon-owned-restart-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&root).expect("create state root");
        let ownership =
            DaemonRemountCoordinatorLock::try_acquire(&root).expect("exclusive remount ownership");
        std::fs::write(
            daemon_remount_fence_path(&root),
            b"{\"version\":4,\"owner\":\"desktop:123\",\"generation\":\"aabbccdd\"}\n",
        )
        .expect("write exact remount fence");

        let copied_values_status =
            std::process::Command::new(std::env::current_exe().expect("test executable"))
                .arg("--ignored")
                .arg("--exact")
                .arg("daemon::tests::remount_daemon_start_helper")
                .arg("--nocapture")
                .env("LOCALITY_REMOUNT_START_TEST_ROOT", &root)
                .env("LOCALITY_REMOUNT_START_TEST_EXPECT", "blocked")
                .env("LOCALITY_REMOUNT_START_OWNER", "desktop:123")
                .env("LOCALITY_REMOUNT_START_GENERATION", "aabbccdd")
                .status()
                .expect("run copied-value adversary helper");
        assert!(copied_values_status.success());

        ownership
            .begin_daemon_start_handoff()
            .expect("begin live restart handoff");
        let live_handoff_status =
            std::process::Command::new(std::env::current_exe().expect("test executable"))
                .arg("--ignored")
                .arg("--exact")
                .arg("daemon::tests::remount_daemon_start_helper")
                .arg("--nocapture")
                .env("LOCALITY_REMOUNT_START_TEST_ROOT", &root)
                .env("LOCALITY_REMOUNT_START_TEST_EXPECT", "allowed")
                .status()
                .expect("run live handoff replacement daemon helper");

        assert!(live_handoff_status.success());
        drop(ownership);
        remove_process_lock_test_root(&root);
    }

    #[test]
    #[ignore]
    fn remount_daemon_start_helper() {
        let Some(root) = std::env::var_os("LOCALITY_REMOUNT_START_TEST_ROOT") else {
            return;
        };
        let expectation = std::env::var("LOCALITY_REMOUNT_START_TEST_EXPECT")
            .expect("helper startup expectation");
        let paths = DaemonProcessPaths::for_target(PathBuf::from(root), "linux", None);
        let startup = DaemonStartupCoordinatorLock::try_acquire(&paths);
        match expectation.as_str() {
            "blocked" => assert_eq!(
                startup
                    .err()
                    .expect("copied fence values cannot authorize startup")
                    .code(),
                "remount_in_progress"
            ),
            "allowed" => {
                let startup = startup.expect("live lock handoff authorizes replacement startup");
                drop(startup);
                assert!(
                    DaemonRemountCoordinatorLock::try_acquire(&paths.state_root).is_err(),
                    "the live recovery gate excludes a second recovery after startup handoff"
                );
            }
            value => panic!("unknown helper expectation `{value}`"),
        }
    }

    #[test]
    fn coordinated_reset_preserves_lock_inode_and_rejects_active_remount() {
        let _guard = process_lock_test_guard();
        let root = std::env::temp_dir().join(format!(
            "locality-daemon-reset-lock-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&root).expect("create state root");
        let ordinary = root.join("ordinary-state");
        std::fs::write(&ordinary, "state").expect("write ordinary state");

        let remount =
            DaemonRemountCoordinatorLock::try_acquire(&root).expect("active remount ownership");
        let error = super::reset_locality_state_storage_coordinated(&root)
            .expect_err("active remount blocks reset");
        assert_eq!(error.code(), "remount_in_progress");
        assert!(ordinary.exists());
        drop(remount);

        let reset_ownership =
            DaemonRemountCoordinatorLock::try_acquire(&root).expect("reset ownership");
        #[cfg(unix)]
        let lock_inode = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(super::daemon_remount_lock_path(&root))
                .expect("lock metadata before reset")
                .ino()
        };
        let report = reset_ownership
            .reset_locality_state_storage()
            .expect("owned coordinated reset");
        assert!(!ordinary.exists());
        assert_eq!(
            report.preserved_state_entries,
            vec![super::DAEMON_REMOUNT_LOCK_FILENAME.to_string()]
        );
        let lock_path = super::daemon_remount_lock_path(&root);
        assert!(lock_path.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                std::fs::metadata(&lock_path)
                    .expect("lock metadata after reset")
                    .ino(),
                lock_inode
            );
        }
        assert!(
            DaemonRemountCoordinatorLock::try_acquire(&root).is_err(),
            "the original reset ownership remains held through the storage wipe"
        );
        drop(reset_ownership);
        let _next = DaemonRemountCoordinatorLock::try_acquire(&root)
            .expect("preserved lock remains reusable");
        drop(_next);
        remove_process_lock_test_root(&root);
    }

    #[test]
    fn start_mode_resolution_is_platform_specific() {
        assert_eq!(
            DaemonStartMode::Auto.resolve_for_target("macos"),
            Some(DaemonManager::Launchd)
        );
        assert_eq!(
            DaemonStartMode::Auto.resolve_for_target("windows"),
            Some(DaemonManager::Session)
        );
        assert_eq!(DaemonStartMode::Launchd.resolve_for_target("windows"), None);
    }

    #[test]
    fn launchd_service_target_uses_user_domain_and_label() {
        assert_eq!(
            launchd_service_target("gui/501"),
            "gui/501/ai.codeflash.locality.localityd"
        );
    }

    #[test]
    fn launchd_restart_fence_disables_before_drain_and_enables_afterward() {
        let target = launchd_service_target("gui/501");
        assert_eq!(
            launchd_restart_fence_action(&target, false),
            ("disable", target.as_str())
        );
        assert_eq!(
            launchd_restart_fence_action(&target, true),
            ("enable", target.as_str())
        );
    }

    #[test]
    fn launchd_disabled_state_parser_preserves_explicit_service_state() {
        assert!(
            parse_launchd_service_enabled("disabled services = {\n}")
                .expect("missing entry uses enabled default")
        );
        assert!(
            !parse_launchd_service_enabled(&format!(
                "disabled services = {{\n    \"{}\" => true\n}}",
                super::MACOS_LAUNCHD_LABEL
            ))
            .expect("explicit disabled state")
        );
        assert!(
            parse_launchd_service_enabled(&format!(
                "disabled services = {{\n    \"{}\" => false,\n}}",
                super::MACOS_LAUNCHD_LABEL
            ))
            .expect("explicit enabled state")
        );
    }

    #[test]
    fn escapes_xml_special_characters() {
        assert_eq!(
            xml_escape("a&b<c>d\"e'f"),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
    }

    #[test]
    fn launch_agent_plist_contains_daemon_environment() {
        let paths = DaemonProcessPaths::for_target(
            PathBuf::from("/tmp/loc-state"),
            "macos",
            Some(PathBuf::from("/Users/ada")),
        );
        let config = DaemonProcessStartConfig {
            mode: DaemonStartMode::Launchd,
            paths: &paths,
            localityd_bin: &PathBuf::from("/tmp/localityd"),
            tcp_addr: Some("127.0.0.1:38567"),
            environment: vec![("NOTION_TOKEN".to_string(), "secret&value".to_string())],
        };

        let plist = launch_agent_plist(&config).expect("plist");

        assert!(plist.contains("<string>/tmp/localityd</string>"));
        assert!(plist.contains("<key>LOCALITY_STATE_DIR</key>"));
        assert!(plist.contains("<string>/tmp/loc-state</string>"));
        assert!(plist.contains("<key>LOCALITY_DAEMON_TCP_ADDR</key>"));
        assert!(plist.contains("<key>NOTION_TOKEN</key>"));
        assert!(plist.contains("<string>secret&amp;value</string>"));
        assert!(!plist.contains("LOCALITY_REMOUNT_START_OWNER"));
        assert!(!plist.contains("LOCALITY_REMOUNT_START_GENERATION"));
    }
}
