use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use locality_platform::{
    DaemonManager, DaemonProcessError, DaemonProcessManager, DaemonProcessPaths,
    DaemonProcessStartConfig, DaemonProcessStartReport, DaemonStartMode,
    DefaultDaemonProcessManager,
};
use localityd::ipc::{
    DaemonClientError, DaemonEndpoint, DaemonReloadReport, DaemonRequest, DaemonResponse,
    DaemonStatusReport, send_endpoint_request_with_timeout,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

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
    pub localityd_bin: Option<String>,
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

type StartMode = DaemonStartMode;

#[derive(Clone, Debug, PartialEq, Eq)]
struct DaemonOptions {
    action: DaemonAction,
    mode: StartMode,
    state_root: PathBuf,
    localityd_bin: Option<PathBuf>,
    tcp_addr: Option<String>,
    include_env: Vec<String>,
}

type DaemonPaths = DaemonProcessPaths;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct DaemonMetadata {
    manager: DaemonManager,
    localityd_bin: String,
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
                && is_running(&options, &paths)
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
                "usage: loc daemon start|stop|status|reload|restart [--session|--launchd] [--localityd-bin <path>] [--state-dir <path>] [--tcp-addr <host:port|off>] [--include-env <KEY>]",
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
        localityd_bin: flag_value(args, "--localityd-bin").map(PathBuf::from),
        tcp_addr: flag_value(args, "--tcp-addr")
            .map(str::to_string)
            .or_else(|| env::var("LOCALITY_DAEMON_TCP_ADDR").ok()),
        include_env: flag_values(args, "--include-env"),
    })
}

fn start_daemon(
    options: &DaemonOptions,
    paths: &DaemonPaths,
) -> Result<DaemonControlReport, DaemonControlError> {
    if is_running(options, paths) {
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
    let localityd_bin = find_localityd_binary(options.localityd_bin.as_deref())?;
    let process_manager = DefaultDaemonProcessManager;
    process_manager
        .resolve_start_manager(options.mode)
        .map_err(daemon_process_error)?;
    validate_start_endpoint(options)?;
    let environment = included_environment(options)?;
    let artifacts = process_manager
        .start(&DaemonProcessStartConfig {
            mode: options.mode,
            paths,
            localityd_bin: &localityd_bin,
            tcp_addr: options.tcp_addr.as_deref(),
            environment,
        })
        .map_err(daemon_process_error)?;

    if !wait_for_state(options, paths, DaemonRunState::Running, START_TIMEOUT) {
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
        Some(artifacts.localityd_bin),
        "daemon started",
    ))
}

fn stop_daemon(
    options: &DaemonOptions,
    paths: &DaemonPaths,
) -> Result<DaemonControlReport, DaemonControlError> {
    let was_running = is_running(options, paths);
    let mut stopped_by_shutdown = false;
    if was_running && request_graceful_shutdown(options, paths) {
        stopped_by_shutdown = wait_for_state(options, paths, DaemonRunState::Stopped, STOP_TIMEOUT);
    }

    let mut stopped_managed_process = false;
    if should_stop_managed_process(options.mode, stopped_by_shutdown, std::env::consts::OS) {
        stopped_managed_process = DefaultDaemonProcessManager
            .stop(options.mode, paths)
            .map_err(daemon_process_error)?
            .stopped_managed_process;
    }

    if was_running
        && !stopped_managed_process
        && wait_for_state(
            options,
            paths,
            DaemonRunState::Stopped,
            Duration::from_millis(250),
        )
    {
        stopped_managed_process = true;
    }

    if !wait_for_state(options, paths, DaemonRunState::Stopped, STOP_TIMEOUT) {
        return Err(DaemonControlError::new(
            "stop_failed",
            "daemon is still responding; if it was started manually, stop the localityd process directly",
        ));
    }
    let _ = fs::remove_file(&paths.pid_file);
    let _ = fs::remove_file(&paths.metadata_file);

    let message = if was_running || stopped_managed_process || stopped_by_shutdown {
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

fn request_graceful_shutdown(options: &DaemonOptions, paths: &DaemonPaths) -> bool {
    send_daemon_request(
        options,
        paths,
        &DaemonRequest::Shutdown,
        DAEMON_CONTROL_REQUEST_TIMEOUT,
    )
    .is_ok_and(|response| response.ok)
}

fn should_stop_managed_process(
    mode: StartMode,
    stopped_by_shutdown: bool,
    target_os: &str,
) -> bool {
    !stopped_by_shutdown || mode.should_use_launchd_for_target(target_os)
}

fn status_report(
    action: DaemonAction,
    options: &DaemonOptions,
    paths: &DaemonPaths,
) -> DaemonControlReport {
    let state = if is_running(options, paths) {
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
        send_daemon_report::<DaemonStatusReport>(options, paths, &DaemonRequest::Status).ok()
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
    if !is_running(options, paths) {
        return Err(DaemonControlError::new(
            "daemon_not_running",
            "daemon is not running",
        ));
    }
    let reload =
        send_daemon_report::<DaemonReloadReport>(options, paths, &DaemonRequest::ReloadMounts)?;
    let daemon_status =
        send_daemon_report::<DaemonStatusReport>(options, paths, &DaemonRequest::Status).ok();
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
    localityd_bin: Option<PathBuf>,
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
            .unwrap_or_else(|| localityd::ipc::DEFAULT_TCP_ADDR.to_string()),
        localityd_bin: localityd_bin
            .map(|path| path.display().to_string())
            .or_else(|| metadata.map(|metadata| metadata.localityd_bin)),
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
    options: &DaemonOptions,
    paths: &DaemonPaths,
    request: &DaemonRequest,
) -> Result<T, DaemonControlError>
where
    T: DeserializeOwned,
{
    let response = send_daemon_request(options, paths, request, DAEMON_CONTROL_REQUEST_TIMEOUT)
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
    artifacts: &DaemonProcessStartReport,
) -> Result<(), DaemonControlError> {
    let metadata = DaemonMetadata {
        manager: artifacts.manager,
        localityd_bin: artifacts.localityd_bin.display().to_string(),
        tcp_addr: options
            .tcp_addr
            .clone()
            .unwrap_or_else(|| localityd::ipc::DEFAULT_TCP_ADDR.to_string()),
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

fn daemon_process_error(error: DaemonProcessError) -> DaemonControlError {
    DaemonControlError::new(error.code(), error.message())
}

fn included_environment(
    options: &DaemonOptions,
) -> Result<Vec<(String, String)>, DaemonControlError> {
    let mut environment = Vec::new();
    for key in &options.include_env {
        let value = env::var(key).map_err(|_| {
            DaemonControlError::new(
                "env_missing",
                format!("environment variable `{key}` is not set"),
            )
        })?;
        environment.push((key.clone(), value));
    }
    Ok(environment)
}

fn validate_start_endpoint(options: &DaemonOptions) -> Result<(), DaemonControlError> {
    #[cfg(windows)]
    {
        let value = options
            .tcp_addr
            .as_deref()
            .unwrap_or(localityd::ipc::DEFAULT_TCP_ADDR);
        if tcp_addr_disabled(value) {
            return Err(DaemonControlError::new(
                "unsupported",
                "Windows daemon session mode requires TCP IPC; omit --tcp-addr off or pass --tcp-addr <host:port>",
            ));
        }
        parse_tcp_addr(value).map_err(|error| DaemonControlError::new("usage", error))?;
    }

    #[cfg(not(windows))]
    {
        let _ = options;
    }

    Ok(())
}

fn send_daemon_request(
    options: &DaemonOptions,
    paths: &DaemonPaths,
    request: &DaemonRequest,
    timeout: Duration,
) -> Result<DaemonResponse, DaemonClientError> {
    let endpoint = control_endpoint(options, paths)?;
    send_endpoint_request_with_timeout(&endpoint, request, timeout).or_else(|error| {
        if cfg!(unix) && matches!(error, DaemonClientError::NotAvailable(_)) {
            control_tcp_addr(options, paths)
                .and_then(|addr| {
                    send_endpoint_request_with_timeout(
                        &DaemonEndpoint::LocalTcp(addr),
                        request,
                        timeout,
                    )
                })
                .map_err(|fallback| match fallback {
                    DaemonClientError::NotAvailable(_) => error,
                    fallback => fallback,
                })
        } else {
            Err(error)
        }
    })
}

fn control_endpoint(
    options: &DaemonOptions,
    paths: &DaemonPaths,
) -> Result<DaemonEndpoint, DaemonClientError> {
    #[cfg(windows)]
    {
        control_tcp_addr(options, paths).map(DaemonEndpoint::LocalTcp)
    }

    #[cfg(not(windows))]
    {
        let _ = options;
        Ok(DaemonEndpoint::UnixSocket(paths.socket.clone()))
    }
}

fn control_tcp_addr(
    options: &DaemonOptions,
    paths: &DaemonPaths,
) -> Result<std::net::SocketAddr, DaemonClientError> {
    let value = options
        .tcp_addr
        .as_deref()
        .map(str::to_string)
        .or_else(|| read_metadata(paths).map(|metadata| metadata.tcp_addr))
        .unwrap_or_else(|| localityd::ipc::DEFAULT_TCP_ADDR.to_string());
    if tcp_addr_disabled(&value) {
        return Err(DaemonClientError::NotAvailable(
            "daemon TCP IPC is disabled".to_string(),
        ));
    }
    parse_tcp_addr(&value).map_err(DaemonClientError::Protocol)
}

fn tcp_addr_disabled(value: &str) -> bool {
    matches!(value, "0" | "off" | "none" | "disabled")
}

fn parse_tcp_addr(value: &str) -> Result<std::net::SocketAddr, String> {
    value
        .parse()
        .map_err(|error| format!("invalid daemon TCP address `{value}`: {error}"))
}

fn is_running(options: &DaemonOptions, paths: &DaemonPaths) -> bool {
    match send_daemon_request(
        options,
        paths,
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

fn wait_for_state(
    options: &DaemonOptions,
    paths: &DaemonPaths,
    state: DaemonRunState,
    timeout: Duration,
) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let running = is_running(options, paths);
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
    DefaultDaemonProcessManager.detected_manager(paths)
}

fn find_localityd_binary(explicit: Option<&Path>) -> Result<PathBuf, DaemonControlError> {
    if let Some(path) = explicit {
        return existing_file(path).ok_or_else(|| {
            DaemonControlError::new(
                "binary_missing",
                format!("localityd binary not found at `{}`", path.display()),
            )
        });
    }
    if let Ok(value) = env::var("LOCALITYD_BIN")
        && let Some(path) = existing_file(Path::new(&value))
    {
        return Ok(path);
    }
    if let Ok(current_exe) = env::current_exe()
        && let Some(dir) = current_exe.parent()
        && let Some(path) = existing_file(&dir.join(binary_name("localityd")))
    {
        return Ok(path);
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest_dir.join("../..");
    for candidate in [
        workspace
            .join("target/debug")
            .join(binary_name("localityd")),
        workspace
            .join("target/release")
            .join(binary_name("localityd")),
    ] {
        if let Some(path) = existing_file(&candidate) {
            return Ok(path);
        }
    }

    if let Some(path) = find_on_path(&binary_name("localityd")) {
        return Ok(path);
    }

    Err(DaemonControlError::new(
        "binary_missing",
        "localityd binary was not found; build/install localityd or pass --localityd-bin <path>",
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
        "--localityd-bin" | "--state-dir" | "--tcp-addr" | "--include-env"
    )
}

fn default_state_root() -> PathBuf {
    locality_platform::default_state_root()
}

#[cfg(test)]
mod tests {
    use super::*;
    use localityd::ipc::{DaemonBuildInfo, DaemonRuntimeStatus, DaemonWatchStatus};
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    #[test]
    fn parses_daemon_start_options() {
        let args = strings(&[
            "start",
            "--session",
            "--state-dir",
            "/tmp/loc-state",
            "--tcp-addr",
            "off",
            "--include-env",
            "NOTION_TOKEN",
        ]);

        let options = parse_options(&args).expect("parse");

        assert_eq!(options.action, DaemonAction::Start);
        assert_eq!(options.mode, StartMode::Session);
        assert_eq!(options.state_root, PathBuf::from("/tmp/loc-state"));
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
    fn launchd_stop_unloads_manager_after_graceful_shutdown() {
        assert!(should_stop_managed_process(StartMode::Auto, true, "macos"));
        assert!(should_stop_managed_process(
            StartMode::Launchd,
            true,
            "macos"
        ));
        assert!(!should_stop_managed_process(
            StartMode::Session,
            true,
            "macos"
        ));
        assert!(!should_stop_managed_process(StartMode::Auto, true, "linux"));
        assert!(should_stop_managed_process(
            StartMode::Session,
            false,
            "macos"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn daemon_status_falls_back_to_tcp_when_unix_socket_is_absent() {
        let root = temp_root("loc-daemon-control-tcp-fallback");
        std::fs::create_dir_all(&root).expect("state root");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind tcp");
        let addr = listener.local_addr().expect("local addr");
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept tcp request");
                let mut line = String::new();
                BufReader::new(stream.try_clone().expect("clone tcp stream"))
                    .read_line(&mut line)
                    .expect("read request");
                let request: DaemonRequest = serde_json::from_str(&line).expect("decode request");
                let response = match request {
                    DaemonRequest::Ping => DaemonResponse::ok(serde_json::json!({})),
                    DaemonRequest::Status => DaemonResponse::ok(DaemonStatusReport {
                        status: "ok".to_string(),
                        build: DaemonBuildInfo {
                            version: "0.1.3".to_string(),
                            build_id: "test-build".to_string(),
                        },
                        runtime: DaemonRuntimeStatus {
                            active_job: false,
                            active_job_detail: None,
                            pending_requests: 0,
                            pending_hydrations: 0,
                            deferred_hydrations: 0,
                            pending_freshness: 0,
                            ready_freshness: 0,
                            deferred_freshness: 0,
                            freshness_budget_units: 0,
                            ready_freshness_budget_units: 0,
                            pending_scheduled_pull: false,
                            scheduler_mode: "polling".to_string(),
                            active_interval_ms: 15_000,
                            cold_interval_ms: 300_000,
                        },
                        watches: DaemonWatchStatus {
                            watched_mounts: 0,
                            watched_roots: Vec::new(),
                        },
                    }),
                    other => DaemonResponse::error(
                        "unexpected_request",
                        format!("unexpected request: {other:?}"),
                    ),
                };
                localityd::ipc::write_response(&mut stream, &response).expect("write response");
            }
        });

        let report = run_daemon_control(&[
            "status".to_string(),
            "--state-dir".to_string(),
            root.display().to_string(),
            "--tcp-addr".to_string(),
            addr.to_string(),
        ])
        .expect("daemon status");

        assert!(report.ok);
        assert_eq!(report.state, DaemonRunState::Running);
        assert_eq!(
            report
                .daemon_status
                .as_ref()
                .map(|status| status.build.build_id.as_str()),
            Some("test-build")
        );
        server.join().expect("server thread");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn windows_start_rejects_disabled_tcp_endpoint() {
        let options = DaemonOptions {
            action: DaemonAction::Start,
            mode: StartMode::Session,
            state_root: PathBuf::from(r"C:\loc-state"),
            localityd_bin: None,
            tcp_addr: Some("off".to_string()),
            include_env: Vec::new(),
        };

        let error = validate_start_endpoint(&options).expect_err("tcp off rejected");

        assert_eq!(error.code(), "unsupported");
        assert!(error.message().contains("requires TCP IPC"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_control_tcp_addr_prefers_cli_then_metadata() {
        let root = temp_root("loc-daemon-control-tcp");
        std::fs::create_dir_all(&root).expect("state root");
        let paths = DaemonPaths::new(root.clone());
        std::fs::write(
            &paths.metadata_file,
            r#"{"manager":"session","localityd_bin":"localityd.exe","tcp_addr":"127.0.0.1:40100"}"#,
        )
        .expect("metadata");
        let mut options = DaemonOptions {
            action: DaemonAction::Status,
            mode: StartMode::Auto,
            state_root: root.clone(),
            localityd_bin: None,
            tcp_addr: None,
            include_env: Vec::new(),
        };

        assert_eq!(
            control_tcp_addr(&options, &paths)
                .expect("metadata tcp")
                .to_string(),
            "127.0.0.1:40100"
        );

        options.tcp_addr = Some("127.0.0.1:40200".to_string());
        assert_eq!(
            control_tcp_addr(&options, &paths)
                .expect("option tcp")
                .to_string(),
            "127.0.0.1:40200"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    fn temp_root(prefix: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};

        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("{prefix}-{}-{unique}-{suffix}", std::process::id()))
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }
}
