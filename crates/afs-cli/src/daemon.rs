use std::env;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use afsd::ipc::{
    DaemonClientError, DaemonReloadReport, DaemonRequest, DaemonStatusReport,
    send_request_with_timeout,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

const LABEL: &str = "ai.codeflash.afs.afsd";
const START_TIMEOUT: Duration = Duration::from_secs(5);
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
const DAEMON_CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonControlError {
    code: &'static str,
    message: String,
}

impl DaemonControlError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn code(&self) -> &'static str {
        self.code
    }

    pub fn message(&self) -> String {
        self.message.clone()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DaemonControlReport {
    pub ok: bool,
    pub command: &'static str,
    pub action: String,
    pub state: DaemonRunState,
    pub manager: DaemonManager,
    pub state_root: String,
    pub socket: String,
    pub tcp_addr: String,
    pub afsd_bin: Option<String>,
    pub launch_agent: Option<String>,
    pub pid_file: Option<String>,
    pub stdout_log: Option<String>,
    pub stderr_log: Option<String>,
    pub daemon_status: Option<DaemonStatusReport>,
    pub reload: Option<DaemonReloadReport>,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonRunState {
    Running,
    Stopped,
}

impl DaemonRunState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
        }
    }
}

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
enum DaemonAction {
    Start,
    Stop,
    Status,
    Reload,
    Restart,
}

impl DaemonAction {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "start" => Some(Self::Start),
            "stop" => Some(Self::Stop),
            "status" => Some(Self::Status),
            "reload" => Some(Self::Reload),
            "restart" => Some(Self::Restart),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Status => "status",
            Self::Reload => "reload",
            Self::Restart => "restart",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartMode {
    Auto,
    Launchd,
    Session,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DaemonOptions {
    action: DaemonAction,
    mode: StartMode,
    state_root: PathBuf,
    afsd_bin: Option<PathBuf>,
    tcp_addr: Option<String>,
    include_env: Vec<String>,
}

#[derive(Clone, Debug)]
struct DaemonPaths {
    state_root: PathBuf,
    socket: PathBuf,
    pid_file: PathBuf,
    metadata_file: PathBuf,
    stdout_log: PathBuf,
    stderr_log: PathBuf,
    launch_agent: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct StartArtifacts {
    manager: DaemonManager,
    afsd_bin: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct DaemonMetadata {
    manager: DaemonManager,
    afsd_bin: String,
    tcp_addr: String,
}

pub fn run_daemon_control(args: &[String]) -> Result<DaemonControlReport, DaemonControlError> {
    let options = parse_options(args)?;
    let paths = DaemonPaths::new(options.state_root.clone());

    match options.action {
        DaemonAction::Start => start_daemon(&options, &paths),
        DaemonAction::Stop => stop_daemon(&options, &paths),
        DaemonAction::Status => Ok(status_report(options.action, &options, &paths)),
        DaemonAction::Reload => reload_daemon(&options, &paths),
        DaemonAction::Restart => {
            if let Err(error) = stop_daemon(&options, &paths)
                && is_running(&paths)
            {
                return Err(error);
            }
            start_daemon(&options, &paths).map(|mut report| {
                report.action = DaemonAction::Restart.as_str().to_string();
                report.message = "daemon restarted".to_string();
                report
            })
        }
    }
}

fn parse_options(args: &[String]) -> Result<DaemonOptions, DaemonControlError> {
    let action = first_positional(args)
        .and_then(DaemonAction::parse)
        .ok_or_else(|| {
            DaemonControlError::new(
                "usage",
                "usage: afs daemon start|stop|status|reload|restart [--session|--launchd] [--afsd-bin <path>] [--state-dir <path>] [--tcp-addr <host:port|off>] [--include-env <KEY>]",
            )
        })?;

    let session = has_flag(args, "--session");
    let launchd = has_flag(args, "--launchd");
    if session && launchd {
        return Err(DaemonControlError::new(
            "usage",
            "--session and --launchd are mutually exclusive",
        ));
    }
    let mode = if session {
        StartMode::Session
    } else if launchd {
        StartMode::Launchd
    } else {
        StartMode::Auto
    };

    Ok(DaemonOptions {
        action,
        mode,
        state_root: flag_value(args, "--state-dir")
            .map(PathBuf::from)
            .unwrap_or_else(default_state_root),
        afsd_bin: flag_value(args, "--afsd-bin").map(PathBuf::from),
        tcp_addr: flag_value(args, "--tcp-addr")
            .map(str::to_string)
            .or_else(|| env::var("AFS_DAEMON_TCP_ADDR").ok()),
        include_env: flag_values(args, "--include-env"),
    })
}

fn start_daemon(
    options: &DaemonOptions,
    paths: &DaemonPaths,
) -> Result<DaemonControlReport, DaemonControlError> {
    if is_running(paths) {
        return Ok(report(
            options.action,
            DaemonRunState::Running,
            detected_manager(paths),
            options,
            paths,
            None,
            "daemon already running",
        ));
    }

    fs::create_dir_all(&paths.state_root)
        .map_err(|error| DaemonControlError::new("io_error", error.to_string()))?;
    let afsd_bin = find_afsd_binary(options.afsd_bin.as_deref())?;
    let manager = resolve_start_manager(options.mode)?;
    let artifacts = match manager {
        DaemonManager::Launchd => start_launchd(options, paths, &afsd_bin)?,
        DaemonManager::Session => start_session(options, paths, &afsd_bin)?,
        DaemonManager::Unknown => {
            return Err(DaemonControlError::new(
                "unsupported",
                "daemon start manager could not be resolved",
            ));
        }
    };

    if !wait_for_state(paths, DaemonRunState::Running, START_TIMEOUT) {
        return Err(DaemonControlError::new(
            "start_failed",
            format!(
                "daemon did not become ready; check `{}`",
                paths.stderr_log.display()
            ),
        ));
    }
    write_metadata(options, paths, &artifacts)?;

    Ok(report(
        options.action,
        DaemonRunState::Running,
        artifacts.manager,
        options,
        paths,
        Some(artifacts.afsd_bin),
        "daemon started",
    ))
}

fn stop_daemon(
    options: &DaemonOptions,
    paths: &DaemonPaths,
) -> Result<DaemonControlReport, DaemonControlError> {
    let was_running = is_running(paths);
    let mut stopped_managed_process = false;

    if should_use_launchd(options.mode)
        && paths
            .launch_agent
            .as_ref()
            .is_some_and(|path| path.exists())
    {
        stop_launchd(paths)?;
        stopped_managed_process = true;
    }

    if paths.pid_file.exists() {
        stop_session(paths)?;
        stopped_managed_process = true;
    }
    let _ = fs::remove_file(&paths.metadata_file);

    if was_running
        && !stopped_managed_process
        && wait_for_state(paths, DaemonRunState::Stopped, Duration::from_millis(250))
    {
        stopped_managed_process = true;
    }

    if !wait_for_state(paths, DaemonRunState::Stopped, STOP_TIMEOUT) {
        return Err(DaemonControlError::new(
            "stop_failed",
            "daemon is still responding; if it was started manually, stop the afsd process directly",
        ));
    }

    let message = if was_running || stopped_managed_process {
        "daemon stopped"
    } else {
        "daemon was not running"
    };

    Ok(report(
        options.action,
        DaemonRunState::Stopped,
        DaemonManager::Unknown,
        options,
        paths,
        None,
        message,
    ))
}

fn status_report(
    action: DaemonAction,
    options: &DaemonOptions,
    paths: &DaemonPaths,
) -> DaemonControlReport {
    let state = if is_running(paths) {
        DaemonRunState::Running
    } else {
        DaemonRunState::Stopped
    };
    let manager = if state == DaemonRunState::Running {
        detected_manager(paths)
    } else {
        DaemonManager::Unknown
    };
    let message = format!("daemon {}", state.as_str());
    let daemon_status = if state == DaemonRunState::Running {
        send_daemon_report::<DaemonStatusReport>(paths, &DaemonRequest::Status).ok()
    } else {
        None
    };
    let mut report = report(action, state, manager, options, paths, None, message);
    report.daemon_status = daemon_status;
    report
}

fn reload_daemon(
    options: &DaemonOptions,
    paths: &DaemonPaths,
) -> Result<DaemonControlReport, DaemonControlError> {
    if !is_running(paths) {
        return Err(DaemonControlError::new(
            "daemon_not_running",
            "daemon is not running",
        ));
    }
    let reload = send_daemon_report::<DaemonReloadReport>(paths, &DaemonRequest::ReloadMounts)?;
    let daemon_status =
        send_daemon_report::<DaemonStatusReport>(paths, &DaemonRequest::Status).ok();
    let mut report = report(
        DaemonAction::Reload,
        DaemonRunState::Running,
        detected_manager(paths),
        options,
        paths,
        None,
        "daemon watches reloaded",
    );
    report.reload = Some(reload);
    report.daemon_status = daemon_status;
    Ok(report)
}

fn report(
    action: DaemonAction,
    state: DaemonRunState,
    manager: DaemonManager,
    options: &DaemonOptions,
    paths: &DaemonPaths,
    afsd_bin: Option<PathBuf>,
    message: impl Into<String>,
) -> DaemonControlReport {
    let metadata = (state == DaemonRunState::Running)
        .then(|| read_metadata(paths))
        .flatten();
    let manager = if manager == DaemonManager::Unknown {
        metadata
            .as_ref()
            .map(|metadata| metadata.manager)
            .unwrap_or(manager)
    } else {
        manager
    };
    DaemonControlReport {
        ok: true,
        command: "daemon",
        action: action.as_str().to_string(),
        state,
        manager,
        state_root: paths.state_root.display().to_string(),
        socket: paths.socket.display().to_string(),
        tcp_addr: metadata
            .as_ref()
            .map(|metadata| metadata.tcp_addr.clone())
            .or_else(|| options.tcp_addr.clone())
            .unwrap_or_else(|| afsd::ipc::DEFAULT_TCP_ADDR.to_string()),
        afsd_bin: afsd_bin
            .map(|path| path.display().to_string())
            .or_else(|| metadata.map(|metadata| metadata.afsd_bin)),
        launch_agent: paths
            .launch_agent
            .as_ref()
            .map(|path| path.display().to_string()),
        pid_file: Some(paths.pid_file.display().to_string()),
        stdout_log: Some(paths.stdout_log.display().to_string()),
        stderr_log: Some(paths.stderr_log.display().to_string()),
        daemon_status: None,
        reload: None,
        message: message.into(),
    }
}

fn send_daemon_report<T>(
    paths: &DaemonPaths,
    request: &DaemonRequest,
) -> Result<T, DaemonControlError>
where
    T: DeserializeOwned,
{
    let response =
        send_request_with_timeout(&paths.state_root, request, DAEMON_CONTROL_REQUEST_TIMEOUT)
            .map_err(|error| {
                DaemonControlError::new(
                    "daemon_error",
                    format!("daemon request failed: {}", error.message()),
                )
            })?;
    if let Some(error) = response.error {
        return Err(DaemonControlError::new(
            "daemon_error",
            format!("{}: {}", error.code, error.message),
        ));
    }
    let Some(payload) = response.payload else {
        return Err(DaemonControlError::new(
            "daemon_protocol_error",
            "daemon returned no payload",
        ));
    };
    serde_json::from_value(payload)
        .map_err(|error| DaemonControlError::new("daemon_protocol_error", error.to_string()))
}

fn write_metadata(
    options: &DaemonOptions,
    paths: &DaemonPaths,
    artifacts: &StartArtifacts,
) -> Result<(), DaemonControlError> {
    let metadata = DaemonMetadata {
        manager: artifacts.manager,
        afsd_bin: artifacts.afsd_bin.display().to_string(),
        tcp_addr: options
            .tcp_addr
            .clone()
            .unwrap_or_else(|| afsd::ipc::DEFAULT_TCP_ADDR.to_string()),
    };
    let json = serde_json::to_string_pretty(&metadata)
        .map_err(|error| DaemonControlError::new("json_encode_failed", error.to_string()))?;
    fs::write(&paths.metadata_file, json)
        .map_err(|error| DaemonControlError::new("io_error", error.to_string()))
}

fn read_metadata(paths: &DaemonPaths) -> Option<DaemonMetadata> {
    let bytes = fs::read(&paths.metadata_file).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn resolve_start_manager(mode: StartMode) -> Result<DaemonManager, DaemonControlError> {
    match mode {
        StartMode::Session => Ok(DaemonManager::Session),
        StartMode::Launchd => {
            if cfg!(target_os = "macos") {
                Ok(DaemonManager::Launchd)
            } else {
                Err(DaemonControlError::new(
                    "unsupported",
                    "--launchd is only supported on macOS",
                ))
            }
        }
        StartMode::Auto => {
            if cfg!(target_os = "macos") {
                Ok(DaemonManager::Launchd)
            } else {
                Ok(DaemonManager::Session)
            }
        }
    }
}

fn should_use_launchd(mode: StartMode) -> bool {
    matches!(mode, StartMode::Auto | StartMode::Launchd) && cfg!(target_os = "macos")
}

fn start_session(
    options: &DaemonOptions,
    paths: &DaemonPaths,
    afsd_bin: &Path,
) -> Result<StartArtifacts, DaemonControlError> {
    fs::create_dir_all(paths.stdout_log.parent().unwrap_or(&paths.state_root))
        .map_err(|error| DaemonControlError::new("io_error", error.to_string()))?;
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.stdout_log)
        .map_err(|error| DaemonControlError::new("io_error", error.to_string()))?;
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.stderr_log)
        .map_err(|error| DaemonControlError::new("io_error", error.to_string()))?;

    let mut command = Command::new(afsd_bin);
    command
        .env("AFS_STATE_DIR", &paths.state_root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    if let Some(tcp_addr) = &options.tcp_addr {
        command.env("AFS_DAEMON_TCP_ADDR", tcp_addr);
    }
    detach_session_process(&mut command);

    let child = command
        .spawn()
        .map_err(|error| DaemonControlError::new("start_failed", error.to_string()))?;
    fs::write(&paths.pid_file, child.id().to_string())
        .map_err(|error| DaemonControlError::new("io_error", error.to_string()))?;

    Ok(StartArtifacts {
        manager: DaemonManager::Session,
        afsd_bin: afsd_bin.to_path_buf(),
    })
}

#[cfg(unix)]
fn detach_session_process(command: &mut Command) {
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn detach_session_process(_command: &mut Command) {}

#[cfg(target_os = "macos")]
fn start_launchd(
    options: &DaemonOptions,
    paths: &DaemonPaths,
    afsd_bin: &Path,
) -> Result<StartArtifacts, DaemonControlError> {
    let Some(launch_agent) = &paths.launch_agent else {
        return Err(DaemonControlError::new(
            "unsupported",
            "launchd requires a user LaunchAgents directory",
        ));
    };
    fs::create_dir_all(launch_agent.parent().unwrap_or(Path::new(".")))
        .map_err(|error| DaemonControlError::new("io_error", error.to_string()))?;
    fs::create_dir_all(paths.stdout_log.parent().unwrap_or(&paths.state_root))
        .map_err(|error| DaemonControlError::new("io_error", error.to_string()))?;

    let plist = launch_agent_plist(options, paths, afsd_bin)?;
    fs::write(launch_agent, plist)
        .map_err(|error| DaemonControlError::new("io_error", error.to_string()))?;

    let domain = launchd_domain()?;
    let _ = Command::new("launchctl")
        .arg("bootout")
        .arg(&domain)
        .arg(launch_agent)
        .output();
    run_launchctl(
        Command::new("launchctl")
            .arg("bootstrap")
            .arg(&domain)
            .arg(launch_agent),
    )?;
    run_launchctl(
        Command::new("launchctl")
            .arg("enable")
            .arg(format!("{domain}/{LABEL}")),
    )?;
    run_launchctl(
        Command::new("launchctl")
            .arg("kickstart")
            .arg("-k")
            .arg(format!("{domain}/{LABEL}")),
    )?;

    Ok(StartArtifacts {
        manager: DaemonManager::Launchd,
        afsd_bin: afsd_bin.to_path_buf(),
    })
}

#[cfg(not(target_os = "macos"))]
fn start_launchd(
    _options: &DaemonOptions,
    _paths: &DaemonPaths,
    _afsd_bin: &Path,
) -> Result<StartArtifacts, DaemonControlError> {
    Err(DaemonControlError::new(
        "unsupported",
        "launchd is only supported on macOS",
    ))
}

#[cfg(target_os = "macos")]
fn stop_launchd(paths: &DaemonPaths) -> Result<(), DaemonControlError> {
    let Some(launch_agent) = &paths.launch_agent else {
        return Ok(());
    };
    let domain = launchd_domain()?;
    let _ = Command::new("launchctl")
        .arg("bootout")
        .arg(&domain)
        .arg(launch_agent)
        .output();
    if launch_agent.exists() {
        fs::remove_file(launch_agent)
            .map_err(|error| DaemonControlError::new("io_error", error.to_string()))?;
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn stop_launchd(_paths: &DaemonPaths) -> Result<(), DaemonControlError> {
    Ok(())
}

fn stop_session(paths: &DaemonPaths) -> Result<(), DaemonControlError> {
    let pid = fs::read_to_string(&paths.pid_file)
        .map_err(|error| DaemonControlError::new("io_error", error.to_string()))?
        .trim()
        .to_string();
    if !pid.is_empty() {
        let _ = Command::new("kill").arg(&pid).output();
    }
    let _ = fs::remove_file(&paths.pid_file);
    Ok(())
}

fn is_running(paths: &DaemonPaths) -> bool {
    match send_request_with_timeout(
        &paths.state_root,
        &DaemonRequest::Ping,
        DAEMON_CONTROL_REQUEST_TIMEOUT,
    ) {
        Ok(response) => response.ok,
        Err(DaemonClientError::NotAvailable(_))
        | Err(DaemonClientError::TimedOut(_))
        | Err(DaemonClientError::Io(_))
        | Err(DaemonClientError::Protocol(_)) => false,
    }
}

fn wait_for_state(paths: &DaemonPaths, state: DaemonRunState, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let running = is_running(paths);
        if (state == DaemonRunState::Running && running)
            || (state == DaemonRunState::Stopped && !running)
        {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

fn detected_manager(paths: &DaemonPaths) -> DaemonManager {
    if paths.pid_file.exists() {
        return DaemonManager::Session;
    }
    if paths
        .launch_agent
        .as_ref()
        .is_some_and(|path| path.exists())
    {
        return DaemonManager::Launchd;
    }
    DaemonManager::Unknown
}

fn find_afsd_binary(explicit: Option<&Path>) -> Result<PathBuf, DaemonControlError> {
    if let Some(path) = explicit {
        return existing_file(path).ok_or_else(|| {
            DaemonControlError::new(
                "binary_missing",
                format!("afsd binary not found at `{}`", path.display()),
            )
        });
    }
    if let Ok(value) = env::var("AFSD_BIN")
        && let Some(path) = existing_file(Path::new(&value))
    {
        return Ok(path);
    }
    if let Ok(current_exe) = env::current_exe()
        && let Some(dir) = current_exe.parent()
        && let Some(path) = existing_file(&dir.join(binary_name("afsd")))
    {
        return Ok(path);
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest_dir.join("../..");
    for candidate in [
        workspace.join("target/debug").join(binary_name("afsd")),
        workspace.join("target/release").join(binary_name("afsd")),
    ] {
        if let Some(path) = existing_file(&candidate) {
            return Ok(path);
        }
    }

    if let Some(path) = find_on_path(&binary_name("afsd")) {
        return Ok(path);
    }

    Err(DaemonControlError::new(
        "binary_missing",
        "afsd binary was not found; build/install afsd or pass --afsd-bin <path>",
    ))
}

fn existing_file(path: &Path) -> Option<PathBuf> {
    path.is_file().then(|| path.to_path_buf())
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn binary_name(name: &str) -> String {
    #[cfg(windows)]
    {
        format!("{name}.exe")
    }
    #[cfg(not(windows))]
    {
        name.to_string()
    }
}

#[cfg(target_os = "macos")]
fn launchd_domain() -> Result<String, DaemonControlError> {
    let output = Command::new("id")
        .arg("-u")
        .output()
        .map_err(|error| DaemonControlError::new("launchctl_failed", error.to_string()))?;
    if !output.status.success() {
        return Err(DaemonControlError::new(
            "launchctl_failed",
            "could not determine current user id",
        ));
    }
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(format!("gui/{uid}"))
}

#[cfg(target_os = "macos")]
fn run_launchctl(command: &mut Command) -> Result<(), DaemonControlError> {
    let output = command
        .output()
        .map_err(|error| DaemonControlError::new("launchctl_failed", error.to_string()))?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let message = if !stderr.is_empty() { stderr } else { stdout };
    Err(DaemonControlError::new(
        "launchctl_failed",
        if message.is_empty() {
            format!("launchctl exited with {}", output.status)
        } else {
            message
        },
    ))
}

#[cfg(target_os = "macos")]
fn launch_agent_plist(
    options: &DaemonOptions,
    paths: &DaemonPaths,
    afsd_bin: &Path,
) -> Result<String, DaemonControlError> {
    let mut env_vars = vec![
        ("HOME".to_string(), home_dir()?.display().to_string()),
        (
            "AFS_STATE_DIR".to_string(),
            paths.state_root.display().to_string(),
        ),
    ];
    if let Some(tcp_addr) = &options.tcp_addr {
        env_vars.push(("AFS_DAEMON_TCP_ADDR".to_string(), tcp_addr.clone()));
    }
    for key in &options.include_env {
        let value = env::var(key).map_err(|_| {
            DaemonControlError::new(
                "env_missing",
                format!("environment variable `{key}` is not set"),
            )
        })?;
        env_vars.push((key.clone(), value));
    }
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
    <string>{afsd_bin}</string>
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
        label = LABEL,
        afsd_bin = xml_escape(&afsd_bin.display().to_string()),
        env_xml = env_xml,
        stdout = xml_escape(&paths.stdout_log.display().to_string()),
        stderr = xml_escape(&paths.stderr_log.display().to_string()),
    ))
}

#[cfg(test)]
fn test_launch_agent_plist(
    options: &DaemonOptions,
    paths: &DaemonPaths,
    afsd_bin: &Path,
) -> Result<String, DaemonControlError> {
    #[cfg(target_os = "macos")]
    {
        launch_agent_plist(options, paths, afsd_bin)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (options, paths, afsd_bin);
        Ok(String::new())
    }
}

impl DaemonPaths {
    fn new(state_root: PathBuf) -> Self {
        let socket = afsd::ipc::socket_path(&state_root);
        let pid_file = state_root.join("afsd.pid");
        let metadata_file = state_root.join("afsd.manager.json");
        let stdout_log = state_root.join("logs/afsd.out.log");
        let stderr_log = state_root.join("logs/afsd.err.log");
        let launch_agent = home_dir().ok().map(|home| {
            home.join("Library/LaunchAgents")
                .join(format!("{LABEL}.plist"))
        });
        Self {
            state_root,
            socket,
            pid_file,
            metadata_file,
            stdout_log,
            stderr_log,
            launch_agent,
        }
    }
}

fn first_positional(args: &[String]) -> Option<&str> {
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if takes_value(arg) {
            skip_next = true;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        return Some(arg.as_str());
    }
    None
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|arg| arg == flag)
}

fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|index| args.get(index + 1))
        .filter(|value| !value.starts_with('-'))
        .map(String::as_str)
}

fn flag_values(args: &[String], flag: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == flag
            && let Some(value) = args.get(index + 1)
            && !value.starts_with('-')
        {
            values.push(value.clone());
            index += 2;
            continue;
        }
        index += 1;
    }
    values
}

fn takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "--afsd-bin" | "--state-dir" | "--tcp-addr" | "--include-env"
    )
}

fn default_state_root() -> PathBuf {
    afs_platform::default_state_root()
}

fn home_dir() -> Result<PathBuf, DaemonControlError> {
    afs_platform::user_home()
        .ok_or_else(|| DaemonControlError::new("env_missing", "home directory is not set"))
}

#[allow(dead_code)]
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
    use super::*;

    #[test]
    fn parses_daemon_start_options() {
        let args = strings(&[
            "start",
            "--session",
            "--state-dir",
            "/tmp/afs-state",
            "--tcp-addr",
            "off",
            "--include-env",
            "NOTION_TOKEN",
        ]);

        let options = parse_options(&args).expect("parse");

        assert_eq!(options.action, DaemonAction::Start);
        assert_eq!(options.mode, StartMode::Session);
        assert_eq!(options.state_root, PathBuf::from("/tmp/afs-state"));
        assert_eq!(options.tcp_addr.as_deref(), Some("off"));
        assert_eq!(options.include_env, vec!["NOTION_TOKEN"]);
    }

    #[test]
    fn rejects_multiple_start_modes() {
        let error = parse_options(&strings(&["start", "--session", "--launchd"]))
            .expect_err("parse should fail");

        assert_eq!(error.code(), "usage");
    }

    #[test]
    fn escapes_xml_special_characters() {
        assert_eq!(
            xml_escape("a&b<c>d\"e'f"),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
    }

    #[test]
    fn launch_agent_test_hook_is_available() {
        let options = DaemonOptions {
            action: DaemonAction::Start,
            mode: StartMode::Launchd,
            state_root: PathBuf::from("/tmp/afs-state"),
            afsd_bin: None,
            tcp_addr: Some("127.0.0.1:38567".to_string()),
            include_env: Vec::new(),
        };
        let paths = DaemonPaths::new(PathBuf::from("/tmp/afs-state"));

        let plist =
            test_launch_agent_plist(&options, &paths, Path::new("/tmp/afsd")).expect("plist");

        if cfg!(target_os = "macos") {
            assert!(plist.contains("<string>/tmp/afsd</string>"));
            assert!(plist.contains("<key>AFS_STATE_DIR</key>"));
            assert!(plist.contains("<string>/tmp/afs-state</string>"));
        } else {
            assert!(plist.is_empty());
        }
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }
}
