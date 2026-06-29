use std::fs::{self, OpenOptions};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::process::{DefaultSessionProcessManager, SessionProcessManager};
use crate::user_home;

pub const MACOS_LAUNCHD_LABEL: &str = "ai.codeflash.locality.localityd";
pub const DAEMON_SOCKET_FILENAME: &str = "localityd.sock";
pub const DAEMON_PID_FILENAME: &str = "localityd.pid";
pub const DAEMON_METADATA_FILENAME: &str = "localityd.manager.json";
pub const DAEMON_STDOUT_LOG_FILENAME: &str = "localityd.out.log";
pub const DAEMON_STDERR_LOG_FILENAME: &str = "localityd.err.log";
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
        DaemonManager, DaemonProcessPaths, DaemonProcessStartConfig, DaemonStartMode,
        daemon_socket_path, launch_agent_plist, launchd_service_target, xml_escape,
    };
    use std::path::PathBuf;

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
    }
}
